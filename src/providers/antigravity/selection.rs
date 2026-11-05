//! Account selection from observed cache hits and the tray's per-account quota snapshots.
//! No cache inspection API exists here: affinity is a short-lived hint, never a guarantee.

use std::{collections::{hash_map::DefaultHasher, HashMap}, hash::{Hash, Hasher}};
use serde_json::Value;
use crate::usage::{Bar, FIVE_HOURS, WEEK};

// ponytail: local evidence, not Google's TTL. Reconsider idle conversations after five minutes;
// keep only digests (not prompts). Cross-process affinity needs a shared registry if measured useful.
const CACHE_HINT_SECS: u64 = 300;
const MAX_HINTS: usize = 256;
const QUOTA_FRESH_SECS: u64 = 600;

#[derive(Default)]
pub(super) struct Scheduler {
    quotas: HashMap<(usize, &'static str), Quota>,
    hints: Vec<Hint>,
}

struct Quota {
    fetched: u64,
    windows: Vec<Bar>,
}

impl Quota {
    fn fresh(&self, now: u64) -> bool {
        self.fetched > 0 && now + 5 >= self.fetched && now.saturating_sub(self.fetched) <= QUOTA_FRESH_SECS
    }

    fn wait(&self, now: u64) -> Option<u64> {
        if !self.fresh(now) { return None; }
        self.windows.iter().filter(|b| b.out())
            .filter_map(|b| {
                let deadline = if b.resets == 0 { self.fetched + 60 } else { b.resets };
                (deadline > now).then(|| deadline - now)
            }).max()
    }

    fn priority(&self, now: u64) -> (u64, u64, u64) {
        if !self.fresh(now) || self.windows.is_empty() { return (u64::MAX, u64::MAX, u64::MAX); }
        let deadline = |window| self.windows.iter().filter(|b| b.window == window && b.resets > now && !b.out())
            .map(|b| b.resets).min().unwrap_or(u64::MAX);
        let (short, week) = (deadline(FIVE_HOURS), deadline(WEEK));
        // Earliest expiring usable allowance wins. A later weekly reset must not waste an earlier 5h reset.
        let remaining = self.windows.iter().filter(|b| b.resets == 0 || b.resets > now)
            .map(|b| ((100.0 - b.used).clamp(0.0, 100.0) * 1000.0) as u64).min().unwrap_or(0);
        (short.min(week), week, 100_000 - remaining)
    }
}

struct Hint {
    account: usize,
    head: u64,
    prefix: u64,
    len: usize,
    at: u64,
}

/// Hash only stable cache-relevant input, never request IDs, step counters, or telemetry labels.
struct Prompt {
    head: u64,
    prefixes: Vec<u64>,
}

impl Prompt {
    fn new(model: &str, body: &Value) -> Self {
        let req = &body["request"];
        let mut h = DefaultHasher::new();
        model.hash(&mut h);
        for key in ["systemInstruction", "tools", "toolConfig", "generationConfig"] {
            req[key].to_string().hash(&mut h);
        }
        let head = h.finish();
        let prefixes = req["contents"].as_array().into_iter().flatten().map(|v| {
            v.to_string().hash(&mut h);
            h.finish()
        }).collect();
        Self { head, prefixes }
    }

    fn matches(&self, hint: &Hint) -> bool {
        self.head == hint.head && hint.len > 0 && self.prefixes.get(hint.len - 1) == Some(&hint.prefix)
    }
}

impl Scheduler {
    pub(super) fn update(&mut self, account: usize, fam: &'static str, bars: &[&Bar], fetched: u64) {
        if self.quotas.get(&(account, fam)).is_some_and(|q| q.fetched > fetched) { return; }
        let windows = bars.iter().filter(|b| matches!(b.window, FIVE_HOURS | WEEK) && b.used.is_finite())
            .map(|b| (*b).clone()).collect();
        self.quotas.insert((account, fam), Quota { fetched, windows });
    }

    pub(super) fn wait(&self, account: usize, fam: &'static str, now: u64) -> Option<u64> {
        self.quotas.get(&(account, fam)).and_then(|q| q.wait(now))
    }

    pub(super) fn order(&mut self, model: &str, body: &Value, candidates: Vec<usize>, now: u64) -> Vec<usize> {
        self.hints.retain(|h| h.at <= now && now - h.at <= CACHE_HINT_SECS);
        let prompt = Prompt::new(model, body);
        let fam = super::family(model);
        let mut candidates: Vec<_> = candidates.into_iter().filter(|&i| self.wait(i, fam, now).is_none()).collect();
        candidates.sort_by_key(|&i| {
            let warm = self.hints.iter().filter(|h| h.account == i && prompt.matches(h))
                .map(|h| (h.len, h.at)).max().unwrap_or_default();
            let quota = self.quotas.get(&(i, fam)).map(|q| q.priority(now)).unwrap_or((u64::MAX, u64::MAX, u64::MAX));
            (std::cmp::Reverse(warm), quota, i)
        });
        candidates
    }

    /// Records affinity for the account that served this turn, so subsequent turns can hit server prompt cache.
    pub(super) fn observe(&mut self, account: usize, model: &str, body: &Value, _cached: u64, now: u64) {
        let prompt = Prompt::new(model, body);
        self.hints.retain(|h| h.at <= now && now - h.at <= CACHE_HINT_SECS && !(h.account == account && prompt.matches(h)));
        let Some(&prefix) = prompt.prefixes.last() else { return };
        if self.hints.len() >= MAX_HINTS {
            let oldest = self.hints.iter().enumerate().min_by_key(|(_, h)| h.at).map(|(i, _)| i).unwrap();
            self.hints.swap_remove(oldest);
        }
        self.hints.push(Hint { account, head: prompt.head, prefix, len: prompt.prefixes.len(), at: now });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const NOW: u64 = 1_800_000_000;

    fn body() -> Value {
        json!({ "request": { "systemInstruction": { "parts": [{ "text": "system" }] },
            "contents": [{ "role": "user", "parts": [{ "text": "hello" }] }] } })
    }

    fn bar(used: f64, resets: u64, window: u64) -> Bar {
        Bar { used, resets, window, ..Default::default() }
    }

    #[test]
    fn completed_turns_create_affinity_and_idle_expiry_clears_it() {
        let mut s = Scheduler::default();
        let b = body();
        s.observe(1, "gemini-a", &b, 0, NOW);
        assert_eq!(s.order("gemini-a", &b, vec![0, 1], NOW), [1, 0], "turn 1 bootstraps affinity even with cached=0");
        assert_eq!(s.order("gemini-a", &b, vec![0, 1], NOW + 301), [0, 1], "idle horizon clears affinity");
    }

    #[test]
    fn longer_matching_history_wins_and_telemetry_does_not_break_affinity() {
        let mut s = Scheduler::default();
        let mut b = body();
        s.observe(0, "gemini-a", &b, 20, NOW);
        b["request"]["contents"].as_array_mut().unwrap().push(json!({ "role": "model", "parts": [{ "text": "reply" }] }));
        s.observe(1, "gemini-a", &b, 40, NOW);
        b["request"]["labels"] = json!({ "last_step_index": 99 });
        b["requestId"] = json!("different-each-time");
        assert_eq!(s.order("gemini-a", &b, vec![0, 1], NOW + 1), [1, 0]);
        b["request"]["tools"] = json!([{ "functionDeclarations": [{ "name": "changed" }] }]);
        assert_eq!(s.order("gemini-a", &b, vec![0, 1], NOW + 1), [0, 1]);
    }

    #[test]
    fn stale_quota_does_not_lock_accounts_and_expired_windows_are_probeable() {
        let mut s = Scheduler::default();
        let exhausted = bar(100.0, NOW + 3600, WEEK);
        s.update(0, "gemini", &[&exhausted], NOW);
        assert_eq!(s.order("gemini-a", &body(), vec![0, 1], NOW), [1]);
        assert_eq!(s.wait(0, "gemini", NOW), Some(3600));
        assert_eq!(s.order("gemini-a", &body(), vec![0, 1], NOW + 601), [0, 1], "a stale snapshot cannot keep an account excluded");
        let expired = bar(100.0, NOW - 1, WEEK);
        s.update(0, "gemini", &[&expired], NOW + 1);
        assert_eq!(s.wait(0, "gemini", NOW + 1), None, "reset boundary allows a probe, not a fabricated full quota");
        s.update(0, "gemini", &[&exhausted], NOW);
        assert_eq!(s.wait(0, "gemini", NOW + 1), None, "an older snapshot cannot resurrect the exhausted window");
    }

    #[test]
    fn quota_is_family_specific_and_a_missing_reset_gets_a_short_retry() {
        let mut s = Scheduler::default();
        s.update(0, "claude", &[&bar(100.0, 0, WEEK)], NOW);
        assert_eq!(s.wait(0, "claude", NOW), Some(60));
        assert_eq!(s.wait(0, "claude", NOW + 30), Some(30));
        assert_eq!(s.wait(0, "claude", NOW + 61), None, "probeable after 60s, not permanently locked");
        assert_eq!(s.order("gemini-a", &body(), vec![0, 1], NOW), [0, 1]);
        assert_eq!(s.order("claude-a", &body(), vec![0, 1], NOW), [1]);
    }

    #[test]
    fn hints_are_bounded_and_do_not_cross_models() {
        let mut s = Scheduler::default();
        for i in 0..300 {
            let mut b = body();
            b["request"]["contents"][0]["parts"][0]["text"] = json!(format!("chat {i}"));
            s.observe(1, "gemini-a", &b, 10, NOW + i);
        }
        assert!(s.hints.len() <= 256);
        let b = body();
        s.observe(1, "gemini-a", &b, 10, NOW + 300);
        assert_eq!(s.order("gemini-b", &b, vec![0, 1], NOW + 300), [0, 1]);
    }
}
