//! Native account admission shared by ccat processes; no port or sidecar identity.

use super::store::{self, Account, Settings, StoreLock};
use super::wire::{self, Session};
use crate::common::{fail, Failure};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn runtime_file_in(root: &Path) -> PathBuf {
    root.join("logs").join("runtime.json")
}

fn runtime_lock_in(root: &Path) -> PathBuf {
    root.join("logs").join(".runtime.lock")
}

#[derive(Clone, Serialize, Deserialize)]
struct Ticket {
    id: String,
    pid: u32,
    touched_ms: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Active {
    id: String,
    account: String,
    pid: u32,
    touched_ms: u64,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Runtime {
    queue: Vec<Ticket>,
    active: Vec<Active>,
    next_allowed_ms: BTreeMap<String, u64>,
    cooldown_ms: BTreeMap<String, u64>,
    cursor: usize,
}

impl Runtime {
    fn clean_stale(&mut self, now: u64) {
        // Prune leases older than 10 minutes (600_000 ms)
        self.queue.retain(|t| now.saturating_sub(t.touched_ms) < 600_000);
        self.active.retain(|a| now.saturating_sub(a.touched_ms) < 600_000);
        self.cooldown_ms.retain(|_, until| *until > now);
        self.next_allowed_ms.retain(|_, until| *until > now);
    }
}

pub struct Pool {
    root: PathBuf,
    sessions: Mutex<HashMap<String, (String, Arc<Mutex<Session>>)>>,
}

impl Pool {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn read_runtime(&self) -> Runtime {
        let p = runtime_file_in(&self.root);
        if let Ok(text) = store::read_bounded_string(&p) {
            serde_json::from_str(&text).unwrap_or_default()
        } else {
            Runtime::default()
        }
    }

    fn write_runtime(&self, runtime: &Runtime) -> Result<(), Failure> {
        let p = runtime_file_in(&self.root);
        let text = serde_json::to_string(runtime).map_err(|e| fail(500, e.to_string()))?;
        store::write_atomic_secure(&p, text.as_bytes()).map_err(|e| fail(500, e))
    }

    /// Acquires a queue slot and waits until concurrency limits permit admission.
    pub fn acquire_slot(&self, cancel: &AtomicBool, timeout: Duration) -> Result<SlotGuard, Failure> {
        let ticket_id = crate::common::uuid();
        let pid = std::process::id();
        let start = Instant::now();
        let lock_path = runtime_lock_in(&self.root);

        // Enqueue ticket
        {
            let _lock = StoreLock::acquire(&lock_path, Duration::from_secs(5)).map_err(|e| fail(500, e))?;
            let mut rt = self.read_runtime();
            rt.clean_stale(now_ms());
            rt.queue.push(Ticket { id: ticket_id.clone(), pid, touched_ms: now_ms() });
            self.write_runtime(&rt)?;
        }

        while start.elapsed() < timeout {
            if cancel.load(Ordering::Relaxed) {
                let _ = self.release_ticket(&ticket_id);
                return Err(fail(499, "Request canceled"));
            }

            let max_concurrent = store::settings_in(&self.root).map(|s| s.max_concurrent_requests as usize).unwrap_or(1);
            let admitted = {
                let _lock = StoreLock::acquire(&lock_path, Duration::from_secs(5)).map_err(|e| fail(500, e))?;
                let mut rt = self.read_runtime();
                rt.clean_stale(now_ms());
                if can_admit(&rt, &ticket_id, max_concurrent) {
                    rt.queue.retain(|t| t.id != ticket_id);
                    rt.active.push(Active { id: ticket_id.clone(), account: String::new(), pid, touched_ms: now_ms() });
                    self.write_runtime(&rt)?;
                    true
                } else {
                    // Update heartbeat
                    if let Some(t) = rt.queue.iter_mut().find(|t| t.id == ticket_id) {
                        t.touched_ms = now_ms();
                        let _ = self.write_runtime(&rt);
                    }
                    false
                }
            };

            if admitted {
                return Ok(SlotGuard {
                    pool: self,
                    ticket_id,
                    account_id: None,
                });
            }

            std::thread::sleep(Duration::from_millis(100));
        }

        let _ = self.release_ticket(&ticket_id);
        Err(fail(429, "Timed out waiting for concurrency slot"))
    }

    fn release_ticket(&self, ticket_id: &str) -> Result<(), Failure> {
        let lock_path = runtime_lock_in(&self.root);
        let _lock = StoreLock::acquire(&lock_path, Duration::from_secs(5)).map_err(|e| fail(500, e))?;
        let mut rt = self.read_runtime();
        rt.queue.retain(|t| t.id != ticket_id);
        rt.active.retain(|a| a.id != ticket_id);
        self.write_runtime(&rt)
    }

    /// Selects an eligible account from current storage and marks it active in runtime.
    pub fn select_and_bind(&self, guard: &mut SlotGuard, pinned: Option<&str>, excluded: &BTreeSet<String>) -> Result<Account, Failure> {
        let lock_path = runtime_lock_in(&self.root);
        let _lock = StoreLock::acquire(&lock_path, Duration::from_secs(5)).map_err(|e| fail(500, e))?;
        let accounts = store::list_in(&self.root).map_err(|e| fail(500, e))?;
        let settings = store::settings_in(&self.root).map_err(|e| fail(500, e))?;
        let mut rt = self.read_runtime();
        let now = now_ms();
        rt.clean_stale(now);

        let chosen = select_account(&accounts, &settings, &rt, now, pinned, excluded)
            .ok_or_else(|| fail(429, "No Gemini Web accounts currently available"))?
            .clone();

        if let Some(act) = rt.active.iter_mut().find(|a| a.id == guard.ticket_id) {
            act.account = chosen.id.clone();
            act.touched_ms = now;
        }
        guard.account_id = Some(chosen.id.clone());

        if settings.rotation_enabled {
            rt.cursor = (rt.cursor + 1) % accounts.len().max(1);
        }
        self.write_runtime(&rt)?;
        Ok(chosen)
    }

    pub fn record_success(&self, account_id: &str, latency_ms: f64) {
        let _ = store::mutate_accounts_in(&self.root, |accs| {
            if let Some(a) = accs.iter_mut().find(|a| a.id == account_id) {
                a.request_count += 1;
            }
            Ok(())
        });
        let _ = store::record_history_in(&self.root, &store::HistoryEntry {
            id: crate::common::uuid(),
            account_id: account_id.to_string(),
            requested_model: String::new(),
            actual_model: String::new(),
            started_at: 0.0,
            ended_at: latency_ms / 1000.0,
            status: "success".into(),
            error: None,
            latency_ms,
        });
    }

    pub fn record_failure(&self, account_id: &str, error_msg: &str, cooldown_secs: u64) {
        let _ = store::mutate_accounts_in(&self.root, |accs| {
            if let Some(a) = accs.iter_mut().find(|a| a.id == account_id) {
                a.error_count += 1;
            }
            Ok(())
        });
        if cooldown_secs > 0 {
            let lock_path = runtime_lock_in(&self.root);
            if let Ok(_lock) = StoreLock::acquire(&lock_path, Duration::from_secs(5)) {
                let mut rt = self.read_runtime();
                rt.cooldown_ms.insert(account_id.to_string(), now_ms() + cooldown_secs * 1000);
                let _ = self.write_runtime(&rt);
            }
        }
    }

    /// Gets or initializes an authenticated Session for the given account.
    pub fn get_session(&self, account: &Account) -> Result<Arc<Mutex<Session>>, Failure> {
        let cookie = store::cookie_in(&self.root, &account.id).map_err(|e| fail(401, e))?;
        let mut map = self.sessions.lock().unwrap();

        if let Some((stored_cookie, session_arc)) = map.get(&account.id) {
            if stored_cookie == &cookie {
                return Ok(session_arc.clone());
            }
        }

        let session = wire::initialize(&cookie, "")?;
        let session_arc = Arc::new(Mutex::new(session));
        map.insert(account.id.clone(), (cookie, session_arc.clone()));
        Ok(session_arc)
    }

    pub fn proxy_messages(
        &self,
        stream: &mut TcpStream,
        sigs: &Mutex<HashMap<String, String>>,
        body: &Value,
        asked: &str,
        model: &str,
        cancel: &AtomicBool,
    ) -> Result<(), Failure> {
        let (prompt, files) = super::messages::build(body)?;
        let mut guard = self.acquire_slot(cancel, Duration::from_secs(60))?;
        let mut excluded = BTreeSet::new();

        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(fail(499, "Request canceled"));
            }

            let account = match self.select_and_bind(&mut guard, None, &excluded) {
                Ok(a) => a,
                Err(e) => return Err(e),
            };

            let start_t = Instant::now();
            let session_arc = match self.get_session(&account) {
                Ok(s) => s,
                Err(f) => {
                    excluded.insert(account.id.clone());
                    self.record_failure(&account.id, &f.message, 15);
                    continue;
                }
            };

            let res = {
                let mut session = session_arc.lock().unwrap();
                wire::generate(&mut session, model, &prompt, &files, &json!({}))
            };

            match res {
                Ok(reply) => {
                    let latency = start_t.elapsed().as_secs_f64() * 1000.0;
                    self.record_success(&account.id, latency);

                    let finished = super::messages::finish(body, &reply, asked)?;
                    let is_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);

                    if !is_stream {
                        use crate::proxy::http::respond;
                        respond(stream, 200, &finished, "");
                        return Ok(());
                    }

                    // SSE streaming
                    let mut tx = crate::proxy::sse::Tx::new(model, asked, Some(stream), sigs);
                    if let Err(e) = emit_sse_response(&mut tx, &finished) {
                        match e {
                            crate::proxy::sse::Stop::Gone => return Ok(()),
                            crate::proxy::sse::Stop::Retry(f, _) => return Err(f),
                        }
                    }
                    return Ok(());
                }
                Err(f) => {
                    // Pre-content error: retry with next account if available
                    excluded.insert(account.id.clone());
                    self.record_failure(&account.id, &f.message, if f.status == 429 { 60 } else { 15 });
                    continue;
                }
            }
        }
    }
}

fn emit_sse_response(tx: &mut crate::proxy::sse::Tx, msg: &Value) -> Result<(), crate::proxy::sse::Stop> {
    tx.begin()?;
    if let Some(content) = msg.get("content").and_then(Value::as_array) {
        for block in content {
            match block.get("type").and_then(Value::as_str).unwrap_or("") {
                "thinking" => {
                    if let Some(t) = block.get("thinking").and_then(Value::as_str) {
                        tx.delta("thinking", t)?;
                    }
                }
                "text" => {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        tx.delta("text", t)?;
                    }
                }
                "tool_use" => {
                    let id = block.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                    let input = block.get("input").cloned().unwrap_or(json!({}));
                    tx.tool("tool_use", id, name, input)?;
                }
                _ => {}
            }
        }
    }
    tx.close()?;
    let stop_reason = msg.get("stop_reason").and_then(Value::as_str).unwrap_or("end_turn");
    tx.send("message_delta", json!({
        "type": "message_delta",
        "delta": { "stop_reason": stop_reason, "stop_sequence": null },
        "usage": msg.get("usage").cloned().unwrap_or(json!({}))
    }))?;
    tx.send("message_stop", json!({ "type": "message_stop" }))?;
    Ok(())
}

pub struct SlotGuard<'a> {
    pool: &'a Pool,
    ticket_id: String,
    account_id: Option<String>,
}

impl<'a> Drop for SlotGuard<'a> {
    fn drop(&mut self) {
        let _ = self.pool.release_ticket(&self.ticket_id);
    }
}

fn can_admit(runtime: &Runtime, ticket: &str, maximum: usize) -> bool {
    let maximum = maximum.max(1);
    runtime.active.len() < maximum && runtime.queue.first().is_some_and(|head| head.id == ticket)
}

fn select_account<'a>(
    accounts: &'a [Account],
    settings: &Settings,
    runtime: &Runtime,
    now_ms: u64,
    pinned: Option<&str>,
    excluded: &BTreeSet<String>,
) -> Option<&'a Account> {
    let busy: BTreeSet<&str> = runtime.active.iter().map(|a| a.account.as_str()).collect();
    let eligible = |a: &&Account| {
        a.enabled
            && !excluded.contains(&a.id)
            && !busy.contains(a.id.as_str())
            && runtime.cooldown_ms.get(&a.id).copied().unwrap_or(0) <= now_ms
            && runtime.next_allowed_ms.get(&a.id).copied().unwrap_or(0) <= now_ms
    };
    if let Some(id) = pinned {
        return accounts.iter().find(|a| a.id == id && eligible(a));
    }
    if let Some(id) = settings.preferred_account_id.as_deref() {
        if let Some(preferred) = accounts.iter().find(|a| a.id == id && eligible(a)) {
            return Some(preferred);
        }
    }
    if !settings.rotation_enabled {
        let first = accounts.first()?;
        return if eligible(&first) { Some(first) } else { None };
    }
    let mut candidates: Vec<&'a Account> = accounts.iter().filter(eligible).collect();
    if candidates.is_empty() {
        return None;
    }
    if settings.rotation_strategy == "least_used" {
        candidates.sort_by_key(|a| (a.request_count, a.error_count));
        return candidates.into_iter().next();
    }
    let offset = runtime.cursor % candidates.len();
    candidates.rotate_left(offset);
    candidates.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str, count: u64) -> Account {
        serde_json::from_value(serde_json::json!({"id":id,"name":id,"port":0,"request_count":count})).unwrap()
    }

    #[test]
    fn fifo_and_global_slot_limit_are_both_required() {
        let mut runtime = Runtime::default();
        for id in ["first", "second"] {
            runtime.queue.push(Ticket { id:id.into(), pid:1, touched_ms:0 });
        }
        assert!(can_admit(&runtime, "first", 1));
        assert!(!can_admit(&runtime, "second", 2));
        runtime.queue.remove(0);
        runtime.active.push(Active { id:"first".into(), account:"a".into(), pid:1, touched_ms:0 });
        assert!(!can_admit(&runtime, "second", 1));
        assert!(can_admit(&runtime, "second", 2));
        assert!(!can_admit(&runtime, "second", 0));
    }

    #[test]
    fn preference_least_used_and_observed_cooldowns_are_separate() {
        let mut accounts = vec![account("a", 50), account("b", 5)];
        let mut settings = Settings { rotation_strategy:"least_used".into(), ..Default::default() };
        let mut runtime = Runtime::default();
        let excluded = BTreeSet::new();
        assert_eq!(select_account(&accounts,&settings,&runtime,100,None,&excluded).unwrap().id,"b");
        settings.preferred_account_id = Some("a".into());
        assert_eq!(select_account(&accounts,&settings,&runtime,100,None,&excluded).unwrap().id,"a");
        accounts[0].enabled = false;
        assert_eq!(select_account(&accounts,&settings,&runtime,100,None,&excluded).unwrap().id,"b");
        runtime.cooldown_ms.insert("b".into(),200);
        assert!(select_account(&accounts,&settings,&runtime,100,None,&excluded).is_none());
        assert_eq!(select_account(&accounts,&settings,&runtime,200,None,&excluded).unwrap().id,"b");
    }

    #[test]
    fn round_robin_skips_disabled_busy_rate_limited_and_already_failed_accounts() {
        let accounts = vec![account("a",0), account("b",0)];
        let settings = Settings::default();
        let mut runtime = Runtime { cursor:1, ..Default::default() };
        let mut excluded = BTreeSet::new();
        assert_eq!(select_account(&accounts,&settings,&runtime,100,None,&excluded).unwrap().id,"b");
        excluded.insert("b".into());
        assert_eq!(select_account(&accounts,&settings,&runtime,100,None,&excluded).unwrap().id,"a");
        runtime.next_allowed_ms.insert("a".into(),101);
        assert!(select_account(&accounts,&settings,&runtime,100,None,&excluded).is_none());
        excluded.clear();
        runtime.active.push(Active { id:"r".into(), account:"b".into(), pid:1,touched_ms:100 });
        assert!(select_account(&accounts,&settings,&runtime,100,None,&excluded).is_none());
    }

    #[test]
    fn rotation_off_and_pinned_jobs_do_not_silently_switch_identities() {
        let accounts = vec![account("a",0), account("b",0)];
        let settings = Settings { rotation_enabled:false, ..Default::default() };
        let mut runtime = Runtime::default();
        let excluded = BTreeSet::new();
        runtime.cooldown_ms.insert("a".into(),200);
        assert!(select_account(&accounts,&settings,&runtime,100,None,&excluded).is_none());
        assert_eq!(select_account(&accounts,&settings,&runtime,100,Some("b"),&excluded).unwrap().id,"b");
        assert!(select_account(&accounts,&settings,&runtime,100,Some("deleted"),&excluded).is_none());
    }
}
