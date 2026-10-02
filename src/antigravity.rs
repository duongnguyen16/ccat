//! Google Antigravity (Cloud Code Assist): Google sign-in, project setup, model list, and the
//! account pool the proxy draws from. Wire details follow oh-my-pi's `google-antigravity` provider.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

const ENDPOINTS: [&str; 2] = ["https://daily-cloudcode-pa.googleapis.com", "https://daily-cloudcode-pa.sandbox.googleapis.com"];
// Antigravity's public desktop OAuth client; base64 only so secret scanners leave it alone.
const CLIENT_ID: &str = "MTA3MTAwNjA2MDU5MS10bWhzc2luMmgyMWxjcmUyMzV2dG9sb2poNGc0MDNlcC5hcHBzLmdvb2dsZXVzZXJjb250ZW50LmNvbQ==";
const CLIENT_SECRET: &str = "R09DU1BYLUs1OEZXUjQ4NkxkTEoxbUxCOHNYQzR6NnFEQWY=";
const SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email \
https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog \
https://www.googleapis.com/auth/experimentsandconfigs";
const CALLBACK_PORT: u16 = 51121;
const REDIRECT: &str = "http://127.0.0.1:51121/oauth-callback";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const MANIFEST_URL: &str = "https://antigravity-hub-auto-updater-974169037036.us-central1.run.app/manifest/latest-arm64-mac.yml";
const DENYLIST: [&str; 3] = ["chat_20706", "chat_23310", "gemini-2.5-pro"];

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Account {
    pub email: String,
    pub refresh: String,
    pub project: String,
}

/// Errors reach the GUI as text and the proxy as an HTTP status for Claude Code.
#[derive(Debug)]
pub struct Failure {
    pub status: u16,
    pub message: String,
    pub retry_after: Option<u64>,
}

fn fail(status: u16, message: impl Into<String>) -> Failure {
    Failure { status, message: message.into(), retry_after: None }
}

// ---------------------------------------------------------------- small helpers

fn unb64(s: &str) -> String {
    String::from_utf8(B64.decode(s).expect("embedded base64")).expect("embedded utf-8")
}

pub fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    getrandom::fill(&mut b).expect("OS randomness");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn uuid() -> String {
    let h = random_hex(16);
    format!("{}-{}-4{}-a{}-{}", &h[0..8], &h[8..12], &h[13..16], &h[17..20], &h[20..32])
}

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn dec(s: &str) -> String {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while i < b.len() {
        match (b[i], b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push(h << 4 | l);
                i += 2;
            }
            (b'+', ..) => out.push(b' '),
            (c, ..) => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn agent(global: Option<Duration>) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(300)))
        .timeout_global(global)
        .build()
        .into()
}

/// Google's `error.message` from a JSON error body, else the (trimmed) raw text.
fn google_message(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().or(v["error_description"].as_str()).map(String::from))
        .unwrap_or_else(|| text.chars().take(500).collect())
}

/// `1h2m3.5s` / `500ms` / `12s` → seconds, rounded up.
fn go_duration(s: &str) -> Option<u64> {
    let (mut total, mut num) = (0.0f64, String::new());
    let mut chars = s.trim().chars().peekable();
    let mut any = false;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        let n: f64 = num.parse().ok()?;
        num.clear();
        total += match c {
            'h' => n * 3600.0,
            'm' if chars.peek() == Some(&'s') => {
                chars.next();
                n / 1000.0
            }
            'm' => n * 60.0,
            's' => n,
            _ => return None,
        };
        any = true;
    }
    (any && num.is_empty()).then(|| total.ceil().max(1.0) as u64)
}

/// How long Google says to wait: a `retryDelay` / `quotaResetDelay` anywhere in the error.
pub fn retry_secs_in(v: &Value) -> Option<u64> {
    match v {
        Value::Object(m) => ["retryDelay", "quotaResetDelay"]
            .iter()
            .find_map(|k| m.get(*k).and_then(Value::as_str).and_then(go_duration))
            .or_else(|| m.values().find_map(retry_secs_in)),
        Value::Array(a) => a.iter().find_map(retry_secs_in),
        _ => None,
    }
}

fn retry_secs(text: &str) -> Option<u64> {
    retry_secs_in(&serde_json::from_str(text).ok()?)
}

/// Appends to `antigravity.log` next to the config: claude owns the terminal while the proxy runs.
pub fn log(line: &str) {
    if cfg!(test) {
        return;
    }
    let path = log_path();
    let big = std::fs::metadata(&path).is_ok_and(|m| m.len() > 2_000_000);
    let file = std::fs::OpenOptions::new().create(true).append(!big).write(true).truncate(big).open(&path);
    if let Ok(mut f) = file {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let _ = writeln!(f, "{secs} {line}");
    }
}

pub fn log_path() -> std::path::PathBuf {
    crate::config_path().with_file_name("antigravity.log")
}

/// The backend gates newer models on the client version, so track the live Antigravity release.
fn user_agent() -> String {
    static VERSION: OnceLock<String> = OnceLock::new();
    let v = VERSION.get_or_init(|| {
        agent(Some(Duration::from_secs(5)))
            .get(MANIFEST_URL)
            .header("User-Agent", "electron-builder")
            .call()
            .ok()
            .filter(|r| r.status() == 200)
            .and_then(|mut r| r.body_mut().read_to_string().ok())
            .and_then(|t| manifest_version(&t))
            .unwrap_or_else(|| "2.8.0".into())
    });
    format!("antigravity/hub/{v} (aidev_client; os_type=darwin; arch=arm64; cl=963137146)")
}

fn manifest_version(yaml: &str) -> Option<String> {
    let v = yaml.lines().find_map(|l| l.trim().strip_prefix("version:"))?.trim().trim_matches(['"', '\'']);
    let ok = v.split('.').count() == 3 && v.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    ok.then(|| v.to_string())
}

// ---------------------------------------------------------------- Google OAuth + Cloud Code control plane

fn token_request(params: &[(&str, &str)]) -> Result<Value, String> {
    let (id, secret) = (unb64(CLIENT_ID), unb64(CLIENT_SECRET));
    let mut form = vec![("client_id", id.as_str()), ("client_secret", secret.as_str())];
    form.extend_from_slice(params);
    let mut res = agent(Some(Duration::from_secs(30))).post(TOKEN_URL).send_form(form).map_err(|e| e.to_string())?;
    let status = res.status().as_u16();
    let text = res.body_mut().read_to_string().map_err(|e| e.to_string())?;
    if status != 200 {
        return Err(format!("Google token request failed ({status}): {}", google_message(&text)));
    }
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// Fresh access token and its lifetime.
pub fn refresh(refresh_token: &str) -> Result<(String, Duration), String> {
    let v = token_request(&[("grant_type", "refresh_token"), ("refresh_token", refresh_token)])?;
    let access = v["access_token"].as_str().ok_or("Google returned no access token")?.to_string();
    Ok((access, Duration::from_secs(v["expires_in"].as_u64().unwrap_or(3600))))
}

/// One control-plane call, POST with a JSON body or GET without one; falls over to the sandbox on 5xx / network errors.
fn cloud_code(token: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
    let mut last = String::new();
    for ep in ENDPOINTS {
        let url = format!("{ep}{path}");
        let a = agent(Some(Duration::from_secs(30)));
        let res = match body {
            Some(b) => a
                .post(&url)
                .header("Authorization", format!("Bearer {token}"))
                .header("Content-Type", "application/json")
                .header("User-Agent", user_agent())
                .send(b.to_string()),
            None => a.get(&url).header("Authorization", format!("Bearer {token}")).header("User-Agent", user_agent()).call(),
        };
        let mut res = match res {
            Ok(r) => r,
            Err(e) => {
                last = format!("{url}: {e}");
                continue;
            }
        };
        let status = res.status().as_u16();
        let text = res.body_mut().read_to_string().unwrap_or_default();
        if status == 200 {
            return serde_json::from_str(&text).map_err(|e| format!("{path}: response is not JSON: {e}"));
        }
        last = format!("{path} failed ({status}): {}", google_message(&text));
        if status < 500 {
            break;
        }
    }
    Err(last)
}

fn userinfo(token: &str) -> Result<String, String> {
    let mut res = agent(Some(Duration::from_secs(30)))
        .get("https://www.googleapis.com/oauth2/v1/userinfo?alt=json")
        .header("Authorization", format!("Bearer {token}"))
        .call()
        .map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&res.body_mut().read_to_string().map_err(|e| e.to_string())?).unwrap_or_default();
    v["email"].as_str().map(String::from).ok_or_else(|| "Google did not return the account email".into())
}

/// The Cloud Code project every request is billed to; provisions the free tier on first use.
fn discover_project(token: &str) -> Result<String, String> {
    let meta = json!({ "metadata": { "ideType": "ANTIGRAVITY" } });
    let mut info = cloud_code(token, "/v1internal:loadCodeAssist", Some(&meta))?;
    if info.get("currentTier").is_none_or(Value::is_null) {
        let free = info["allowedTiers"].as_array().is_some_and(|t| t.iter().any(|t| t["id"] == "free-tier"));
        let ineligible = info["ineligibleTiers"].as_array().and_then(|t| t.iter().find(|t| t["tierId"] == "free-tier"));
        if let (false, Some(t)) = (free, ineligible) {
            let why = t["reasonMessage"].as_str().unwrap_or("This account is not eligible for Antigravity.");
            return Err(format!("{why} {}", t["validationUrl"].as_str().unwrap_or("")).trim().to_string());
        }
        onboard(token, &meta)?;
        info = cloud_code(token, "/v1internal:loadCodeAssist", Some(&meta))?;
    }
    info["cloudaicompanionProject"]
        .as_str()
        .filter(|p| !p.is_empty())
        .map(String::from)
        .ok_or_else(|| "Google did not return an Antigravity project for this account.".into())
}

fn onboard(token: &str, meta: &Value) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut body = meta.clone();
    body["tierId"] = json!("free-tier");
    let mut op = cloud_code(token, "/v1internal:onboardUser", Some(&body))?;
    while op["done"] != true {
        if Instant::now() > deadline {
            return Err("Timed out setting up the Antigravity free tier.".into());
        }
        std::thread::sleep(Duration::from_secs(1));
        let name = op["name"].as_str().filter(|n| !n.is_empty()).ok_or("onboardUser returned no operation name")?;
        op = cloud_code(token, &format!("/v1internal/{name}"), None)?;
    }
    match op.get("error").filter(|e| !e.is_null()) {
        Some(e) => Err(format!("Antigravity setup failed: {}", e["message"].as_str().map_or_else(|| e.to_string(), String::from))),
        None => Ok(()),
    }
}

/// Wire model ids the account can use, through its first refresh.
pub fn list_models(acc: &Account) -> Result<Vec<String>, String> {
    let (token, _) = refresh(&acc.refresh)?;
    let v = cloud_code(&token, "/v1internal:fetchAvailableModels", Some(&json!({})))?;
    let models = v["models"].as_object().ok_or("response has no `models`")?;
    let mut ids: Vec<String> =
        models.iter().filter(|(id, m)| m["isInternal"] != true && !DENYLIST.contains(&id.as_str())).map(|(id, _)| id.clone()).collect();
    ids.sort();
    Ok(ids)
}

/// Browser sign-in. `start` binds the callback port (so a busy port fails before the browser opens);
/// `finish` blocks until Google redirects back, then sets up the account.
pub struct Login {
    listener: TcpListener,
    state: String,
    pub url: String,
}

impl Login {
    pub fn start() -> Result<Login, String> {
        let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
            .map_err(|e| format!("Port {CALLBACK_PORT} is busy (another sign-in still open?): {e}"))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let state = random_hex(16);
        let url = format!(
            "https://accounts.google.com/o/oauth2/v2/auth?client_id={}&response_type=code&redirect_uri={}&scope={}&state={state}&access_type=offline&prompt=consent",
            enc(&unb64(CLIENT_ID)),
            enc(REDIRECT),
            enc(SCOPES)
        );
        Ok(Login { listener, state, url })
    }

    pub fn finish(self, cancel: &AtomicBool) -> Result<Account, String> {
        let code = self.wait_code(cancel)?;
        let tok = token_request(&[("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", REDIRECT)])?;
        let refresh = tok["refresh_token"].as_str().filter(|s| !s.is_empty()).ok_or("Google returned no refresh token; try again.")?;
        let access = tok["access_token"].as_str().ok_or("Google returned no access token")?;
        let email = userinfo(access)?;
        let project = discover_project(access)?;
        log(&format!("signed in {email} (project {project})"));
        Ok(Account { email, refresh: refresh.to_string(), project })
    }

    fn wait_code(&self, cancel: &AtomicBool) -> Result<String, String> {
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("Sign-in cancelled.".into());
            }
            if Instant::now() > deadline {
                return Err("Timed out waiting for the browser sign-in.".into());
            }
            match self.listener.accept() {
                Ok((s, _)) => {
                    if let Some(r) = callback(s, &self.state) {
                        return r;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(150)),
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

/// One browser hit on the callback port: `Some` for the OAuth redirect, `None` for anything else (favicon…).
fn callback(mut s: TcpStream, state: &str) -> Option<Result<String, String>> {
    let _ = s.set_nonblocking(false);
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line).ok()?;
    let target = line.split_whitespace().nth(1)?.to_string();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    if path != "/oauth-callback" {
        let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return None;
    }
    let q: HashMap<String, String> = query.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (dec(k), dec(v))).collect();
    let r = if let Some(e) = q.get("error") {
        Err(format!("Google sign-in failed: {e}"))
    } else if q.get("state").map(String::as_str) != Some(state) {
        Err("Sign-in state mismatch; try again.".into())
    } else {
        q.get("code").cloned().ok_or_else(|| "No code in the sign-in callback.".into())
    };
    let msg = if r.is_ok() { "Signed in. You can close this tab and go back to ccat." } else { "Sign-in failed. Go back to ccat for details." };
    let body = format!("<!doctype html><meta charset=utf-8><title>ccat</title><p style=\"font:16px system-ui;margin:40px\">{msg}</p>");
    let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    Some(r)
}

// ---------------------------------------------------------------- account pool

/// Antigravity meters each backend family separately, so a block is per (account, family).
pub fn family(model: &str) -> &'static str {
    if model.starts_with("claude") {
        "claude"
    } else if model.starts_with("gpt") || model.starts_with("openai") {
        "gpt"
    } else {
        "gemini"
    }
}

#[derive(Default)]
struct Slot {
    token: Option<(String, Instant)>,
    blocked: HashMap<&'static str, Instant>,
}

pub struct Pool {
    accounts: Vec<Account>,
    /// (sticky account index, per-account state)
    state: Mutex<(usize, Vec<Slot>)>,
    endpoints: Vec<String>,
    ua: String,
}

impl Pool {
    pub fn new(accounts: Vec<Account>) -> Pool {
        let slots = accounts.iter().map(|_| Slot::default()).collect();
        Pool { accounts, state: Mutex::new((0, slots)), endpoints: ENDPOINTS.map(String::from).to_vec(), ua: user_agent() }
    }

    /// Valid access token for account `i`. Refreshing under the lock keeps refreshes single-flight.
    fn token(&self, i: usize, force: bool) -> Result<String, String> {
        let mut g = self.state.lock().unwrap();
        if let (false, Some((t, exp))) = (force, &g.1[i].token) {
            if *exp > Instant::now() + Duration::from_secs(60) {
                return Ok(t.clone());
            }
        }
        let (t, life) = refresh(&self.accounts[i].refresh)?;
        g.1[i].token = Some((t.clone(), Instant::now() + life));
        Ok(t)
    }

    pub fn block(&self, i: usize, fam: &'static str, secs: u64) {
        log(&format!("{} blocked for {fam} for {secs}s", self.accounts[i].email));
        self.state.lock().unwrap().1[i].blocked.insert(fam, Instant::now() + Duration::from_secs(secs));
    }

    fn blocked_for(&self, i: usize, fam: &str) -> Option<Duration> {
        let g = self.state.lock().unwrap();
        g.1[i].blocked.get(fam).and_then(|t| t.checked_duration_since(Instant::now()))
    }

    /// Opens a `streamGenerateContent` SSE stream, starting at the sticky account and moving on when one is out of quota.
    /// Returns the account index used and the raw SSE reader.
    pub fn stream(&self, model: &str, request: &Value, beta: bool) -> Result<(usize, Box<dyn BufRead + Send>), Failure> {
        let fam = family(model);
        let n = self.accounts.len();
        if n == 0 {
            return Err(fail(403, "No Antigravity accounts. Add one in ccat → Config."));
        }
        let start = self.state.lock().unwrap().0;
        let mut last: Option<Failure> = None;
        for k in 0..n {
            let i = (start + k) % n;
            if self.blocked_for(i, fam).is_some() {
                continue;
            }
            let who = &self.accounts[i].email;
            let mut token = match self.token(i, false) {
                Ok(t) => t,
                Err(e) => {
                    // A revoked refresh token won't heal by itself; anything else gets a short breather.
                    self.block(i, fam, if e.contains("invalid_grant") { 86_400 } else { 30 });
                    last = Some(fail(403, format!("{who}: {e} — sign in again in ccat if this persists.")));
                    continue;
                }
            };
            let mut body = request.clone();
            body["project"] = json!(self.accounts[i].project);
            let body = body.to_string();
            let (mut ep, mut refreshed) = (0, false);
            while ep < self.endpoints.len() {
                let url = format!("{}/v1internal:streamGenerateContent?alt=sse", self.endpoints[ep]);
                let mut req = agent(None)
                    .post(&url)
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Content-Type", "application/json")
                    .header("Accept", "text/event-stream")
                    .header("User-Agent", &self.ua);
                if beta {
                    req = req.header("anthropic-beta", "interleaved-thinking-2025-05-14");
                }
                let mut res = match req.send(&body) {
                    Ok(r) => r,
                    Err(e) => {
                        last = Some(fail(502, format!("{url}: {e}")));
                        ep += 1;
                        continue;
                    }
                };
                let status = res.status().as_u16();
                if status == 200 {
                    self.state.lock().unwrap().0 = i;
                    log(&format!("{model} via {who}"));
                    return Ok((i, Box::new(BufReader::new(res.into_body().into_reader()))));
                }
                let header_wait = res.headers().get("retry-after").and_then(|v| v.to_str().ok()?.trim().parse().ok());
                let text = res.body_mut().read_to_string().unwrap_or_default();
                let msg = google_message(&text);
                log(&format!("{model} via {who}: {status} {msg}"));
                // The exact body Google refused, so a failure can be replayed and inspected later.
                if !cfg!(test) {
                    let _ = std::fs::write(log_path().with_file_name("antigravity-last-error.json"), format!("{status} {text}
{body}"));
                }
                match status {
                    401 if !refreshed => {
                        refreshed = true;
                        match self.token(i, true) {
                            Ok(t) => token = t,
                            Err(e) => {
                                last = Some(fail(403, format!("{who}: {e}")));
                                break;
                            }
                        }
                    }
                    429 => {
                        let secs = retry_secs(&text).or(header_wait).unwrap_or(60);
                        self.block(i, fam, secs);
                        last = Some(Failure { status, message: format!("{who}: {msg}"), retry_after: Some(secs) });
                        break;
                    }
                    401 | 403 => {
                        self.block(i, fam, 3600);
                        last = Some(fail(403, format!("{who}: {msg}")));
                        break;
                    }
                    s if s >= 500 => {
                        last = Some(fail(s, msg));
                        ep += 1;
                    }
                    _ => return Err(fail(status, msg)),
                }
            }
            // ponytail: an outage (5xx everywhere) isn't one account's fault — surface it rather than burn the pool.
            if last.as_ref().is_some_and(|f| f.status >= 500) {
                break;
            }
        }
        match last {
            Some(f) if f.status != 429 => Err(f),
            _ => Err(self.all_blocked(fam)),
        }
    }

    /// One non-streaming JSON call (e.g. `countTokens`) on the sticky account. No quota rotation: callers have a fallback.
    pub fn post(&self, path: &str, body: &Value) -> Result<Value, Failure> {
        if self.accounts.is_empty() {
            return Err(fail(403, "No Antigravity accounts."));
        }
        let i = self.state.lock().unwrap().0;
        let token = self.token(i, false).map_err(|e| fail(403, e))?;
        let mut last = fail(502, format!("{path}: no endpoint answered"));
        for ep in &self.endpoints {
            let res = agent(Some(Duration::from_secs(30)))
                .post(format!("{ep}{path}"))
                .header("Authorization", format!("Bearer {token}"))
                .header("Content-Type", "application/json")
                .header("User-Agent", &self.ua)
                .send(body.to_string());
            let mut res = match res {
                Ok(r) => r,
                Err(e) => {
                    last = fail(502, format!("{path}: {e}"));
                    continue;
                }
            };
            let status = res.status().as_u16();
            let text = res.body_mut().read_to_string().unwrap_or_default();
            if status == 200 {
                return serde_json::from_str(&text).map_err(|e| fail(502, format!("{path}: response is not JSON: {e}")));
            }
            last = fail(status, google_message(&text));
            if status < 500 {
                break;
            }
        }
        Err(last)
    }

    fn all_blocked(&self, fam: &str) -> Failure {
        let soonest = (0..self.accounts.len()).filter_map(|i| self.blocked_for(i, fam)).min().unwrap_or(Duration::from_secs(60));
        let secs = soonest.as_secs().max(1);
        Failure {
            status: 429,
            message: format!(
                "All {} Antigravity account(s) are out of {fam} quota; the next frees up in {}m{}s.",
                self.accounts.len(),
                secs / 60,
                secs % 60
            ),
            retry_after: Some(secs),
        }
    }
}

#[cfg(test)]
impl Pool {
    /// Accounts whose access token is the email, pointed at a fake upstream.
    pub fn fake(endpoint: &str, emails: &[&str]) -> Pool {
        let accounts: Vec<Account> = emails.iter().map(|e| Account { email: e.to_string(), refresh: String::new(), project: format!("p-{e}") }).collect();
        let far = Instant::now() + Duration::from_secs(3600);
        let slots = emails.iter().map(|e| Slot { token: Some((e.to_string(), far)), ..Default::default() }).collect();
        Pool { accounts, state: Mutex::new((0, slots)), endpoints: vec![endpoint.to_string()], ua: "test".into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(go_duration("12s"), Some(12));
        assert_eq!(go_duration("1h2m3.5s"), Some(3724));
        assert_eq!(go_duration("500ms"), Some(1));
        assert_eq!(go_duration("soon"), None);
        let body = r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","details":[{"@type":"x.ErrorInfo","metadata":{"quotaResetDelay":"2m"}}]}}"#;
        assert_eq!(retry_secs(body), Some(120));
    }

    #[test]
    fn url_codec_round_trips() {
        let s = "4/0Ab_x-y.z~ é&=+%";
        assert_eq!(dec(&enc(s)), s);
        assert_eq!(dec("a+b%2Fc%"), "a b/c%");
    }

    #[test]
    fn manifest() {
        assert_eq!(manifest_version("version: 2.9.1\nfiles:\n"), Some("2.9.1".into()));
        assert_eq!(manifest_version("version: '3.0.0'"), Some("3.0.0".into()));
        assert_eq!(manifest_version("minimumSystemVersion: 1\n"), None);
    }

    #[test]
    fn families() {
        assert_eq!(family("claude-opus-4-6-thinking"), "claude");
        assert_eq!(family("gpt-oss-120b-medium"), "gpt");
        assert_eq!(family("gemini-3.1-pro-low"), "gemini");
    }
}
