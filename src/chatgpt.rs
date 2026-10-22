//! ChatGPT plan usage through OpenAI's official "Sign in with ChatGPT" token sharing: browser sign-in (OAuth + PKCE),
//! per-account credential files, a cross-process refresh lock, and the account pool the proxy draws from.
//! Protocol: https://developers.openai.com/siwc/token-sharing-open-source
//!
//! Secrets live in `%APPDATA%\ccat\chatgpt\<key>.json`, never in config.json (which several ccat processes read and
//! save): the config only keeps non-secret metadata (`ChatgptAccount`). Every routed session runs its own proxy and they
//! share these accounts, so refreshes (the refresh token rotates; using an old one is a terminal error) are serialised
//! across processes by a lock file. The UI process signs in and out; the proxy only reads.

use crate::{
    antigravity::{agent, log, Failure},
    now,
};
use base64::{
    engine::general_purpose::URL_SAFE_NO_PAD as B64U,
    Engine,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, RwLock,
    },
    time::{Duration, Instant, SystemTime},
};

const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
const DISCOVERY_URL: &str = "https://auth.openai.com/.well-known/openid-configuration";

const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";

const RESOURCE: &str = "https://api.openai.com/v1";
/// Where plan usage is spent (models, inference).
pub const API: &str = RESOURCE;
pub const USAGE_PAGE: &str = "https://chatgpt.com/settings/usage";
/// An access token is replaced this long before it expires.
const EARLY: u64 = 300;

const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

pub const CODEX_BASE: &str = "https://chatgpt.com/backend-api";
/// The Codex CLI version the backend is told (it gates newer models on it, see oh-my-pi's `CODEX_CLIENT_VERSION`).
const CODEX_VERSION: &str = "0.159.0";
const ORIGINATOR: &str = "ccat";
pub const WHAM_USAGE: &str = "https://chatgpt.com/backend-api/wham/usage";
pub const CODEX: &str = "codex";
/// Codex CLI's own login (`~/.codex/auth.json`), shared rather than duplicated: a second sign-in with Codex's client
/// makes OpenAI revoke the earlier sessions (Codex CLI and the user's other devices), so ccat reads that login fresh on
/// every request and never refreshes, writes or revokes it. Codex refreshes it whenever it runs.
pub const CODEX_CLI: &str = "codex-cli";
const AUTH_CLAIM: &str = "https://api.openai.com/auth";
const TERMINAL: [&str; 6] = ["invalid_grant", "invalid_refresh_token", "token_expired", "refresh_token_expired", "refresh_token_invalidated", "refresh_token_reused"];

/// The non-secret part of an account, kept in the config; the tokens are in the account's credential file.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct ChatgptAccount {
    /// First 16 hex of SHA-256(`sub`): names the credential file.
    pub key: String,
    pub email: String,
    /// Turned off in the Providers view: the pool skips it (running sessions follow, see `Pool::accounts`).
    pub disabled: bool,
    /// The token response carried `chatgpt.tokens.use.direct`; without it the account is never used for inference.
    pub granted: bool,
    /// "" = the official Sign in with ChatGPT route; "codex" = the Codex backend (see `CODEX`).
    pub route: String,
    /// The plan type from the Codex token ("plus", "pro"…); "" on the official route, whose tokens don't reveal it.
    pub plan: String,
}

impl ChatgptAccount {
    pub fn usable(&self) -> bool {
        !self.disabled && self.granted
    }

    /// Goes through the Codex backend: ccat's own Codex-route sign-in, or Codex CLI's shared login.
    pub fn codex(&self) -> bool {
        self.route == CODEX || self.route == CODEX_CLI
    }

    pub fn cli(&self) -> bool {
        self.route == CODEX_CLI
    }
}

/// (access token, ChatGPT account id) of Codex CLI's login, read fresh from its file; `None` when Codex isn't signed in
/// with ChatGPT.
pub fn codex_cli_tokens() -> Option<(String, String)> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(codex_home().join("auth.json")).ok()?).ok()?;
    (v["auth_mode"] == "chatgpt").then_some(())?;
    let t = &v["tokens"];
    let access = t["access_token"].as_str().filter(|s| !s.is_empty())?.to_string();
    let id = t["account_id"].as_str().filter(|s| !s.is_empty()).map(String::from).unwrap_or_else(|| codex_profile(&access, "").0);
    Some((access, id))
}

/// Codex CLI's login as an account (email and plan from its tokens), when it is signed in with ChatGPT.
pub fn codex_cli_account() -> Option<ChatgptAccount> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(codex_home().join("auth.json")).ok()?).ok()?;
    (v["auth_mode"] == "chatgpt").then_some(())?;
    let t = &v["tokens"];
    let (_, plan, email, _) = codex_profile(t["access_token"].as_str().unwrap_or(""), t["id_token"].as_str().unwrap_or(""));
    Some(ChatgptAccount { key: CODEX_CLI.into(), email: if email.is_empty() { "Codex CLI".into() } else { email }, granted: true, route: CODEX_CLI.into(), plan, ..Default::default() })
}

/// Keeps Codex CLI's login first in `accounts`: added when Codex is signed in, its email and plan refreshed, the
/// on/off choice kept. Left in place (unusable) while Codex is signed out, so the choice survives a `codex login`.
pub fn sync_codex_cli(accounts: &mut Vec<ChatgptAccount>) {
    let Some(mut cli) = codex_cli_account() else { return };
    if let Some(i) = accounts.iter().position(|a| a.cli()) {
        cli.disabled = accounts.remove(i).disabled;
    }
    accounts.insert(0, cli);
}

/// What ccat knows of one ChatGPT model (kept in the config, so the pickers and the proxy need no network).
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct ChatgptModel {
    pub slug: String,
    pub name: String,
    /// Reasoning efforts the model takes, lowest first (never "ultra": that is Codex's own task delegation).
    pub efforts: Vec<String>,
    pub default_effort: String,
}

/// An account's secrets. No `Debug` on purpose: nothing here may reach a log by accident.
#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct Creds {
    pub client_id: String,
    pub refresh_token: String,
    pub access_token: String,
    /// Unix seconds.
    pub expires_at: u64,
    pub earliest_refresh_at: u64,
    pub scope: String,
    pub id_token: String,
    pub email: String,
    pub sub: String,
    /// "codex" for the Codex route.
    pub route: String,
    /// The ChatGPT workspace the Codex backend wants in `chatgpt-account-id`.
    pub account_id: String,
    pub plan: String,
    /// The refresh token is dead (revoked, expired, reused): the account needs a new browser sign-in.
    pub reauth: bool,
}

pub enum TokenErr {
    /// Terminal: the user must sign in again.
    SignIn(String),
    /// Network trouble or a server error: credentials are kept, try later.
    Later(String),
}

impl TokenErr {
    pub fn message(&self) -> &str {
        match self {
            TokenErr::SignIn(m) | TokenErr::Later(m) => m,
        }
    }
}

// ---------------------------------------------------------------- small helpers: SHA-256, PKCE, time

/// A time as unix seconds: a number (seconds, or milliseconds when huge), a numeric string, or an RFC 3339 string.
/// Anything else is 0, i.e. "no restriction": an unreadable `earliest_refresh_at` must never block a refresh for ever.
fn unix_time(v: &Value) -> u64 {
    let secs = |n: f64| if n > 1e11 { (n / 1000.0) as u64 } else { n as u64 };
    match v {
        Value::Number(n) => secs(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => s.trim().parse::<f64>().map(secs).unwrap_or_else(|_| rfc3339(s).unwrap_or(0)),
        _ => 0,
    }
}

fn rfc3339(s: &str) -> Option<u64> {
    let (date, time) = s.trim().split_once(['T', 't', ' '])?;
    let mut d = date.split('-').map(|x| x.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let at = time.find(['Z', 'z', '+', '-']).unwrap_or(time.len());
    let (clock, zone) = time.split_at(at);
    let mut c = clock.split(':');
    let (h, mi) = (c.next()?.parse::<i64>().ok()?, c.next()?.parse::<i64>().ok()?);
    let sec = c.next().and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0) as i64;
    let offset = match zone.get(..1) {
        Some(sign @ ("+" | "-")) => {
            let (zh, zm) = zone[1..].split_once(':').unwrap_or((&zone[1..], "0"));
            (if sign == "-" { -1 } else { 1 }) * (zh.parse::<i64>().ok()? * 3600 + zm.parse::<i64>().ok()? * 60)
        }
        _ => 0,
    };
    // Days since 1970-01-01 (Howard Hinnant's civil-date algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let (era, mp) = (y.div_euclid(400), if m > 2 { m - 3 } else { m + 9 });
    let yoe = y - era * 400;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + sec - offset).ok()
}

// ---------------------------------------------------------------- credential files

/// `%APPDATA%\ccat\chatgpt`.
pub fn dir() -> PathBuf {
    crate::config_path().with_file_name("chatgpt")
}

fn creds_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.json"))
}

pub fn read(dir: &Path, key: &str) -> Option<Creds> {
    serde_json::from_str(&std::fs::read_to_string(creds_path(dir, key)).ok()?).ok()
}

/// The folder for secrets. On Unix it is user-only (0700); on Windows it sits under the per-user %APPDATA%, which
/// inherits a user-only ACL.
fn make_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}

/// A file with `data`, written to a temp file (user-only, 0600, on Unix) and renamed over `path`: another process
/// reading never sees half a file.
fn write_private(path: &Path, data: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let tmp = path.with_extension(format!("{}.{}.tmp", path.extension().and_then(|e| e.to_str()).unwrap_or(""), std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    open.open(&tmp).and_then(|mut f| f.write_all(data)).and_then(|_| std::fs::rename(&tmp, path)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

fn write(dir: &Path, key: &str, c: &Creds) -> Result<(), String> {
    make_dir(dir).map_err(|e| e.to_string())?;
    write_private(&creds_path(dir, key), serde_json::to_string_pretty(c).map_err(|e| e.to_string())?.as_bytes())
}

/// The refresh lock. Dropping it releases it, so every error path lets go.
struct Lock(PathBuf);

impl Lock {
    /// `<key>.lock`, created exclusively; waits up to 15 s, and takes over one older than 60 s (its owner died).
    /// ponytail: two processes can both judge one lock stale and both take it; that needs a crash plus a 60 s coincidence.
    fn take(dir: &Path, key: &str) -> Result<Lock, TokenErr> {
        let path = dir.join(format!("{key}.lock"));
        let _ = make_dir(dir);
        let end = Instant::now() + Duration::from_secs(15);
        loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Lock(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let age = std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok());
                    if age.is_some_and(|a| a > Duration::from_secs(60)) {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                }
                Err(e) => return Err(TokenErr::Later(format!("could not lock the ChatGPT sign-in: {e}"))),
            }
            if Instant::now() > end {
                return Err(TokenErr::Later("another ccat is refreshing this ChatGPT sign-in; try again".into()));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A valid access token for the account and its expiry (unix seconds). The one place tokens are refreshed, for the proxy
/// and the UI alike: under the account's lock, re-reading the file first, because another process may have refreshed
/// while this one waited (a second refresh with the rotated token would be `refresh_token_reused`, which is terminal).
/// `rejected` is a token the API just refused; it is never handed out again. `refresh` does the network call.
pub fn access(dir: &Path, key: &str, rejected: Option<&str>, refresh: &dyn Fn(&Creds) -> Result<Creds, TokenErr>) -> Result<(String, u64), TokenErr> {
    let again = || TokenErr::SignIn("This ChatGPT account must be signed in again.".into());
    let usable = |c: &Creds| c.expires_at > now() + EARLY && Some(c.access_token.as_str()) != rejected;
    let c = read(dir, key).ok_or_else(again)?;
    if c.reauth {
        return Err(again());
    }
    if usable(&c) {
        return Ok((c.access_token, c.expires_at));
    }
    let _lock = Lock::take(dir, key)?;
    let mut c = read(dir, key).ok_or_else(again)?;
    if c.reauth {
        return Err(again());
    }
    if usable(&c) {
        return Ok((c.access_token, c.expires_at));
    }
    // Refreshing is not allowed before `earliest_refresh_at`; a token still alive until then is simply kept.
    if now() < c.earliest_refresh_at {
        return match c.expires_at > now() && Some(c.access_token.as_str()) != rejected {
            true => Ok((c.access_token, c.expires_at)),
            false => Err(TokenErr::Later("too early to refresh this ChatGPT sign-in".into())),
        };
    }
    match refresh(&c) {
        Ok(n) => {
            write(dir, key, &n).map_err(|e| TokenErr::Later(format!("could not save the ChatGPT sign-in: {e}")))?;
            Ok((n.access_token, n.expires_at))
        }
        Err(TokenErr::SignIn(m)) => {
            c.reauth = true;
            let _ = write(dir, key, &c);
            Err(TokenErr::SignIn(m))
        }
        Err(e) => Err(e),
    }
}

/// An account that has no usable sign-in left: file gone, marked "sign in again", or without plan usage.
pub fn needs_signin(key: &str) -> bool {
    signed_out(&dir(), key)
}

fn signed_out(dir: &Path, key: &str) -> bool {
    if key == CODEX_CLI {
        return codex_cli_tokens().is_none();
    }
    // The official route needs the plan-usage scope; a Codex token has no such scope.
    read(dir, key).is_none_or(|c| c.reauth || (c.route != CODEX && !c.scope.split_whitespace().any(|s| s == PLAN_SCOPE)))
}

// ---------------------------------------------------------------- OpenAI endpoints

/// Terminal errors have an `error` code in the token endpoint's JSON (a string, or an object with `code`).
fn oauth_code(v: &Value) -> &str {
    v["error"].as_str().or(v["error"]["code"].as_str()).unwrap_or("")
}

fn token_post(url: &str, form: &[(&str, &str)]) -> Result<Value, TokenErr> {
    let mut res = agent(Some(Duration::from_secs(30))).post(url).send_form(form.iter().copied()).map_err(|e| TokenErr::Later(format!("OpenAI token request failed: {e}")))?;
    let status = res.status().as_u16();
    let text = res.body_mut().read_to_string().unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or_default();
    if status == 200 {
        return Ok(v);
    }
    let why = v["error_description"].as_str().or(v["error"]["message"].as_str()).unwrap_or(oauth_code(&v));
    let msg = format!("OpenAI token request failed ({status}): {why}");
    Err(if TERMINAL.contains(&oauth_code(&v)) { TokenErr::SignIn(msg) } else { TokenErr::Later(msg) })
}

/// Folds a token response into `c`: access token, expiry, scope and the rotated refresh token replace the old ones together.
fn apply(c: &mut Creds, v: &Value) -> Result<(), TokenErr> {
    let access = v["access_token"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| TokenErr::Later("OpenAI returned no access token".into()))?;
    c.access_token = access.to_string();
    c.expires_at = now() + v["expires_in"].as_u64().unwrap_or(3600);
    c.earliest_refresh_at = unix_time(&v["earliest_refresh_at"]);
    for (field, key) in [(&mut c.refresh_token, "refresh_token"), (&mut c.scope, "scope"), (&mut c.id_token, "id_token")] {
        if let Some(s) = v[key].as_str().filter(|s| !s.is_empty()) {
            *field = s.to_string();
        }
    }
    Ok(())
}

fn refresh_tokens(c: &Creds) -> Result<Creds, TokenErr> {
    let mut n = c.clone();
    if c.route == CODEX {
        let v = token_post(CODEX_TOKEN_URL, &[("grant_type", "refresh_token"), ("client_id", &c.client_id), ("refresh_token", &c.refresh_token)])?;
        apply(&mut n, &v)?;
        let (account_id, plan, ..) = codex_profile(&n.access_token, &n.id_token);
        for (field, new) in [(&mut n.account_id, account_id), (&mut n.plan, plan)] {
            if !new.is_empty() {
                *field = new;
            }
        }
        return Ok(n);
    }
    let v = token_post(TOKEN_URL, &[("grant_type", "refresh_token"), ("client_id", &c.client_id), ("refresh_token", &c.refresh_token), ("resource", RESOURCE)])?;
    apply(&mut n, &v)?;
    Ok(n)
}

/// A JWT's claims (not verified: see `check_id_token`), `null` when it isn't one.
fn claims(jwt: &str) -> Value {
    jwt.split('.').nth(1).and_then(|p| B64U.decode(p.trim_end_matches('=')).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// (ChatGPT account id, plan type, email, sub) from a Codex access and ID token: the id and plan sit in the
/// `https://api.openai.com/auth` claim of either, the plan sometimes only in the ID token.
fn codex_profile(access: &str, id: &str) -> (String, String, String, String) {
    let (a, i) = (claims(access), claims(id));
    let auth = |k: &str| a[AUTH_CLAIM][k].as_str().or(i[AUTH_CLAIM][k].as_str()).unwrap_or("").trim().to_string();
    let email = a["https://api.openai.com/profile"]["email"].as_str().or(i["https://api.openai.com/profile"]["email"].as_str()).or(i["email"].as_str()).unwrap_or("");
    (auth("chatgpt_account_id"), auth("chatgpt_plan_type").to_lowercase(), email.trim().to_lowercase(), i["sub"].as_str().or(a["sub"].as_str()).unwrap_or("").to_string())
}

/// Codex's request headers (the same on the model list and on inference), minus the per-request ones.
fn codex_headers(token: &str, account_id: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Authorization", format!("Bearer {token}")),
        ("chatgpt-account-id", account_id.to_string()),
        ("originator", ORIGINATOR.to_string()),
        ("OpenAI-Beta", "responses=experimental".to_string()),
        ("version", CODEX_VERSION.to_string()),
        ("User-Agent", concat!("ccat/", env!("CARGO_PKG_VERSION")).to_string()),
    ]
}

fn get_json(url: &str, headers: &[(&str, String)]) -> Result<Value, String> {
    let mut req = agent(Some(Duration::from_secs(30))).get(url).header("Accept", "application/json");
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let mut res = req.call().map_err(|e| e.to_string())?;
    let status = res.status().as_u16();
    let text = res.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("response is not JSON: {e}"))?;
    if status != 200 {
        return Err(format!("ChatGPT request failed ({status}): {}", error_fields(&v).1));
    }
    Ok(v)
}

/// The account's listed models (`visibility: "list"`), with whatever the API says about each (display name, reasoning
/// levels, service tiers) and the gaps filled from Codex's own catalogue, which is only read.
pub fn list_models(key: &str) -> Result<Vec<ChatgptModel>, String> {
    let d = dir();
    let route = read(&d, key).ok_or("This ChatGPT account must be signed in again.")?;
    let (token, _) = access(&d, key, None, &refresh_tokens).map_err(|e| e.message().to_string())?;
    let cache: Value = std::fs::read_to_string(codex_home().join("models_cache.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    if route.route == CODEX {
        // What Codex itself asks (its models_cache.json is this answer); when that fails, the cache it left behind.
        let url = format!("{CODEX_BASE}/codex/models?client_version={CODEX_VERSION}");
        return match get_json(&url, &codex_headers(&token, &route.account_id)) {
            Ok(v) => Ok(parse_models(v["models"].as_array().ok_or("response has no `models`")?, &cache)),
            Err(e) => match cache["models"].as_array() {
                Some(m) => Ok(parse_models(m, &cache)),
                None => Err(e),
            },
        };
    }
    let v = get_json(&format!("{API}/models"), &[("Authorization", format!("Bearer {token}"))])?;
    Ok(parse_models(v["models"].as_array().ok_or("response has no `models`")?, &cache))
}

fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).unwrap_or_default()).join(".codex"))
}

/// One model entry (API or Codex cache shape) as far as it says anything.
fn model_of(m: &Value) -> ChatgptModel {
    let efforts = m["supported_reasoning_levels"].as_array().into_iter().flatten().filter_map(|l| l["effort"].as_str().or(l.as_str())).filter(|e| *e != "ultra").map(String::from).collect();
    ChatgptModel {
        slug: m["slug"].as_str().unwrap_or("").into(),
        name: m["display_name"].as_str().unwrap_or("").into(),
        efforts,
        default_effort: m["default_reasoning_level"].as_str().unwrap_or("").into(),
    }
}

/// The API's listed models; a field the API leaves out is taken from Codex's `models_cache.json` entry with the same slug.
fn parse_models(api: &[Value], codex: &Value) -> Vec<ChatgptModel> {
    let known: Vec<ChatgptModel> = codex["models"].as_array().into_iter().flatten().map(model_of).collect();
    api.iter()
        .filter(|m| m["visibility"] == "list")
        .map(model_of)
        .filter(|m| !m.slug.is_empty())
        .map(|mut m| {
            if let Some(c) = known.iter().find(|c| c.slug == m.slug) {
                for (mine, theirs) in [(&mut m.name, &c.name), (&mut m.default_effort, &c.default_effort)] {
                    if mine.is_empty() {
                        *mine = theirs.clone();
                    }
                }
                if m.efforts.is_empty() {
                    m.efforts = c.efforts.clone();
                }
            }
            m
        })
        .collect()
}

const ORDER: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// The effort to send for `wanted`: itself when the model takes it, else the nearest lower level it takes, else the
/// nearest higher. A model with no known levels gets `wanted` capped at xhigh.
pub fn clamp_effort(wanted: &str, supported: &[String]) -> String {
    let at = ORDER.iter().position(|e| *e == wanted).unwrap_or(2);
    let has = |i: usize| supported.iter().any(|s| s == ORDER[i]);
    if supported.is_empty() {
        return ORDER[at.min(4)].into();
    }
    let pick = (0..=at).rev().chain(at + 1..ORDER.len()).find(|&i| has(i));
    pick.map_or_else(|| supported[0].clone(), |i| ORDER[i].into())
}

/// (access token, ChatGPT account id) of a Codex-route account, for the usage call; refreshed under the usual lock.
pub fn codex_token(key: &str) -> Result<(String, String), String> {
    let d = dir();
    let (token, _) = access(&d, key, None, &refresh_tokens).map_err(|e| e.message().to_string())?;
    let id = read(&d, key).map(|c| c.account_id).unwrap_or_default();
    Ok((token, id))
}

/// Revokes the account's refresh token at OpenAI, then deletes its local files, revoked or not. Blocking: run it on a thread.
pub fn sign_out(key: &str) {
    let d = dir();
    if key == CODEX_CLI {
        return; // Codex CLI's login is Codex's to manage
    }
    if let Some(c) = read(&d, key).filter(|c| c.route != CODEX) {
        if let Err(e) = revoke(&c) {
            log(&format!("ChatGPT sign-out: could not revoke the token at OpenAI ({e}); deleting the local copy anyway"));
        }
    }
    for f in [format!("{key}.json"), format!("{key}.lock")] {
        let _ = std::fs::remove_file(d.join(f));
    }
}

fn revoke(c: &Creds) -> Result<(), String> {
    let a = agent(Some(Duration::from_secs(10)));
    let mut res = a.get(DISCOVERY_URL).call().map_err(|e| e.to_string())?;
    let found: Value = serde_json::from_str(&res.body_mut().read_to_string().map_err(|e| e.to_string())?).unwrap_or_default();
    let url = found["revocation_endpoint"].as_str().ok_or("no revocation_endpoint")?;
    let res = a.post(url).send_form([("token", c.refresh_token.as_str()), ("token_type_hint", "refresh_token"), ("client_id", c.client_id.as_str())]).map_err(|e| e.to_string())?;
    match res.status().as_u16() {
        200..=299 => Ok(()),
        s => Err(format!("revocation answered {s}")),
    }
}

// ---------------------------------------------------------------- browser sign-in

// ---------------------------------------------------------------- errors

/// A refusal, with what the pool should do about it.
pub struct Denied {
    pub failure: Failure,
    /// Keep the account out of rotation this many seconds and move on; `None`: the request itself is at fault, stop.
    pub block: Option<u64>,
    /// What an `unsupported_capability` refusal named (`error.param`); "" otherwise. Some of these are worth a retry without it.
    pub param: String,
}

/// (code, message, param) from an error body, wherever it sits: `error{}`, `response.error{}` (a `response.failed`
/// event), or the top level (an `error` event).
pub fn error_fields(v: &Value) -> (String, String, String) {
    let e = [&v["error"], &v["response"]["error"], v].into_iter().find(|e| e["message"].is_string() || e["code"].is_string()).unwrap_or(&Value::Null);
    let s = |k: &str| e[k].as_str().unwrap_or("").to_string();
    // The Codex backend names the error in `type` (`usage_limit_reached`) where the API uses `code`.
    (if e["code"].is_string() { s("code") } else { s("type") }, s("message"), s("param"))
}

/// What a code or status means for the pool. `status` is 0 for an error that arrived inside an open stream.
pub fn classify(status: u16, code: &str, message: &str, param: &str) -> Denied {
    // ponytail: OpenAI gives no reset time for plan usage, so an hour is a guess; the account is simply tried again after it.
    let (st, block) = match code {
        "subscription_sharing_usage_limit_exceeded" | "usage_limit_reached" => (429, Some(3600)),
        // The sign-in itself was revoked (typically by a newer sign-in with the same client): no retrying until it changes.
        "token_revoked" | "token_invalidated" | "invalid_token" | "token_expired" => (401, Some(86_400)),
        "usage_not_included" => (403, Some(86_400)),
        "subscription_sharing_user_not_eligible" => (403, Some(86_400)),
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => (503, Some(30)),
        "subscription_sharing_unsupported_capability" => (400, None),
        "subscription_sharing_route_not_supported" => (403, None),
        "subscription_sharing_invalid_user" => (401, Some(86_400)),
        "chatpass_v2_scope_not_authorized" | "chatpass_v2_invalid_authorization_context" => (403, Some(86_400)),
        _ if status == 0 => (502, Some(30)),
        _ if status >= 500 => (status, Some(30)),
        _ if status == 429 => (429, Some(60)),
        _ if status == 401 || status == 403 => (status, Some(3600)),
        _ => (status, None),
    };
    let unsupported = code == "subscription_sharing_unsupported_capability";
    let message = match (unsupported, param.is_empty()) {
        (true, false) => format!("ChatGPT: {message} (unsupported: {param})"),
        _ => format!("ChatGPT: {message}"),
    };
    Denied { failure: Failure { status: st, message, retry_after: block.filter(|_| st == 429) }, block, param: if unsupported { param.to_string() } else { String::new() } }
}

/// The Codex route says when the limit lifts (`resets_in_seconds`, or `resets_at` as unix time) in the error: block for that
/// long instead of the guess.
pub fn with_reset(mut d: Denied, v: &Value) -> Denied {
    let e = [&v["error"], &v["response"]["error"], v].into_iter().find(|e| e["message"].is_string() || e["code"].is_string()).unwrap_or(&Value::Null);
    let secs = e["resets_in_seconds"].as_u64().or_else(|| Some(unix_time(&e["resets_at"]).saturating_sub(now())).filter(|s| *s > 0));
    if let (429, Some(secs)) = (d.failure.status, secs) {
        (d.block, d.failure.retry_after) = (Some(secs.max(1)), Some(secs.max(1)));
    }
    d
}

/// The headers that might say how much plan usage is left: names, and the value where it is a plain number. Auth headers never.
fn limit_headers(h: &ureq::http::HeaderMap) -> Vec<String> {
    h.iter()
        .filter(|(k, _)| ["ratelimit", "limit", "usage"].iter().any(|w| k.as_str().contains(w)) && !k.as_str().contains("auth"))
        .map(|(k, v)| match v.to_str().ok().filter(|v| v.trim().parse::<f64>().is_ok()) {
            Some(n) => format!("{k}={n}"),
            None => k.to_string(),
        })
        .collect()
}

// ---------------------------------------------------------------- account pool

#[derive(Default)]
pub struct Pool {
    /// Shared with the proxy's owner: the config watcher swaps in the current list, so a switch (or a new sign-in) reaches
    /// running sessions.
    pub accounts: Arc<RwLock<Vec<ChatgptAccount>>>,
    dir: PathBuf,
    base: String,
    codex_base: String,
    /// Access tokens by account key, kept in memory until 5 minutes before they expire.
    cache: Mutex<HashMap<String, (String, Instant)>>,
    /// Until when, and why (status, message), each account is skipped.
    blocked: Mutex<HashMap<String, (Instant, u16, String)>>,
    /// Unix time each block was set: only usage fetched after it can lift it (see `unblock`).
    since: Mutex<HashMap<String, u64>>,
    /// The API refused plain function tools once: send them in a namespace from now on.
    pub wrap: AtomicBool,
    /// Likewise for `prompt_cache_key`: dropped for the rest of the process.
    pub no_cache_key: AtomicBool,
    /// Shared like `accounts`: what the config says about each model.
    pub models: Arc<RwLock<Vec<ChatgptModel>>>,
}

impl Pool {
    pub fn new(accounts: Vec<ChatgptAccount>, models: Vec<ChatgptModel>) -> Pool {
        let mut p = Pool::default();
        (p.accounts, p.models, p.dir, p.base, p.codex_base) = (Arc::new(RwLock::new(accounts)), Arc::new(RwLock::new(models)), dir(), API.into(), CODEX_BASE.into());
        p
    }

    fn off(&self, a: &ChatgptAccount) -> bool {
        !a.usable() || signed_out(&self.dir, &a.key)
    }

    fn blocked_for(&self, key: &str) -> Option<Duration> {
        self.blocked.lock().unwrap().get(key).and_then(|(t, ..)| t.checked_duration_since(Instant::now()))
    }

    /// Skips the account for `secs`, remembering why, so "nothing left" names the real reason.
    pub fn block(&self, key: &str, secs: u64, why: &Failure) {
        let who = self.accounts.read().unwrap().iter().find(|a| a.key == key).map_or(key.to_string(), |a| a.email.clone());
        log(&format!("ChatGPT {who} skipped for {secs}s: {} {}", why.status, why.message));
        self.blocked.lock().unwrap().insert(key.to_string(), (Instant::now() + Duration::from_secs(secs), why.status, why.message.clone()));
        self.since.lock().unwrap().insert(key.to_string(), crate::now());
    }

    /// Lifts plan-usage blocks (429s) set before usage fetched at `fetched` showed room again. True if one was lifted.
    pub fn unblock(&self, fetched: u64) -> bool {
        let since = self.since.lock().unwrap();
        let mut blocked = self.blocked.lock().unwrap();
        let before = blocked.len();
        blocked.retain(|k, (t, status, _)| !(*status == 429 && *t > Instant::now() && since.get(k).is_some_and(|s| *s < fetched)));
        let lifted = blocked.len() < before;
        if lifted {
            log(&format!("ChatGPT has plan usage again (usage at {fetched}): back in the rotation"));
        }
        lifted
    }

    /// The first account, in the order they were added, that is on, granted, signed in and not blocked.
    pub fn pick(&self) -> Option<ChatgptAccount> {
        self.accounts.read().unwrap().iter().find(|a| !self.off(a) && self.blocked_for(&a.key).is_none()).cloned()
    }

    /// Why `pick` found nothing.
    pub fn exhausted(&self) -> Failure {
        let accounts = self.accounts.read().unwrap();
        let usable: Vec<&ChatgptAccount> = accounts.iter().filter(|a| !self.off(a)).collect();
        let fail = |message: &str| Failure { status: 403, message: message.into(), retry_after: None };
        if accounts.is_empty() {
            return fail("No ChatGPT accounts. Sign in with ChatGPT in ccat → Providers.");
        }
        if usable.is_empty() {
            return fail("No ChatGPT account can be used: turn one on, or sign in again in ccat → Providers (plan usage must be granted).");
        }
        // The account back soonest, and why it is out.
        let blocked = self.blocked.lock().unwrap();
        let soonest = usable.iter().filter_map(|a| blocked.get(&a.key)).filter(|(t, ..)| *t > Instant::now()).min_by_key(|(t, ..)| *t);
        let (secs, status, why) = soonest.map_or((60, 429, String::new()), |(t, st, m)| (t.saturating_duration_since(Instant::now()).as_secs().max(1), *st, m.clone()));
        let wait = format!("{}m{}s", secs / 60, secs % 60);
        match status {
            429 => Failure { status: 429, message: format!("All {} ChatGPT account(s) are over their plan usage; trying again in {wait}.", usable.len()), retry_after: Some(secs) },
            401 => Failure { status: 401, message: format!("{why}. Sign in again: run `codex login` (ccat shares Codex's login)."), retry_after: None },
            st => Failure { status: st, message: format!("{why} (ChatGPT is skipped for {wait})."), retry_after: None },
        }
    }

    /// A valid access token for the account, from memory or through the locked refresh (see `access`).
    fn token(&self, a: &ChatgptAccount, rejected: Option<&str>) -> Result<String, TokenErr> {
        if a.cli() {
            // Read fresh each time, never refreshed: Codex refreshes it when it runs.
            return match codex_cli_tokens() {
                Some((t, _)) if Some(t.as_str()) != rejected => Ok(t),
                Some(_) => Err(TokenErr::SignIn("Codex CLI's login was refused; run `codex` (or `codex login`) to renew it".into())),
                None => Err(TokenErr::SignIn("Codex CLI isn't signed in with ChatGPT; run `codex login`".into())),
            };
        }
        if rejected.is_none() {
            if let Some((t, _)) = self.cache.lock().unwrap().get(&a.key).filter(|(_, until)| *until > Instant::now()) {
                return Ok(t.clone());
            }
        }
        let (t, expires) = access(&self.dir, &a.key, rejected, &refresh_tokens)?;
        let life = expires.saturating_sub(now() + EARLY);
        self.cache.lock().unwrap().insert(a.key.clone(), (t.clone(), Instant::now() + Duration::from_secs(life)));
        Ok(t)
    }

    /// Opens the `/responses` SSE stream for `body` on account `a` (`session`: Claude Code's session id, the Codex route's
    /// cache affinity). A refused token gets one forced refresh.
    pub fn open(&self, a: &ChatgptAccount, body: &Value, session: &str) -> Result<Box<dyn BufRead + Send>, Denied> {
        let mut rejected: Option<String> = None;
        loop {
            let token = self.token(a, rejected.as_deref()).map_err(|e| match e {
                // The credential file is already marked; the pick skips this account until the user signs in again.
                TokenErr::SignIn(m) => Denied { failure: Failure { status: 403, message: format!("{}: {m}", a.email), retry_after: None }, block: Some(86_400), param: String::new() },
                TokenErr::Later(m) => Denied { failure: Failure { status: 502, message: format!("{}: {m}", a.email), retry_after: None }, block: Some(30), param: String::new() },
            })?;
            let (url, mut headers) = if a.codex() {
                let id = if a.cli() { codex_cli_tokens().map(|t| t.1).unwrap_or_default() } else { read(&self.dir, &a.key).map(|c| c.account_id).unwrap_or_default() };
                if id.is_empty() {
                    return Err(Denied { failure: Failure { status: 403, message: format!("{}: no ChatGPT account id; sign in again.", a.email), retry_after: None }, block: Some(86_400), param: String::new() });
                }
                let mut h = codex_headers(&token, &id);
                // As Codex sends them: the session for cache affinity, and which model the request is routed for.
                h.extend([
                    ("session_id", session.to_string()),
                    ("conversation_id", session.to_string()),
                    ("x-client-request-id", session.to_string()),
                    ("x-codex-routing-hint", format!("model={}", body["model"].as_str().unwrap_or(""))),
                ]);
                (format!("{}/codex/responses", self.codex_base), h)
            } else {
                (format!("{}/responses", self.base), vec![("Authorization", format!("Bearer {token}")), ("User-Agent", concat!("ccat/", env!("CARGO_PKG_VERSION")).to_string())])
            };
            headers.extend([("Content-Type", "application/json".to_string()), ("Accept", "text/event-stream".to_string())]);
            let mut req = agent(None).post(&url);
            for (k, v) in &headers {
                req = req.header(*k, v);
            }
            let sent = req.send(body.to_string());
            let mut res = sent.map_err(|e| Denied { failure: Failure { status: 502, message: format!("{url}: {e}"), retry_after: None }, block: Some(30), param: String::new() })?;
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if !LOGGED.swap(true, Ordering::Relaxed) {
                let h = limit_headers(res.headers());
                log(&format!("ChatGPT response headers about limits: {}", if h.is_empty() { "none".to_string() } else { h.join(" ") }));
            }
            let status = res.status().as_u16();
            if status == 200 {
                return Ok(Box::new(BufReader::new(res.into_body().into_reader())));
            }
            let text = res.body_mut().read_to_string().unwrap_or_default();
            let (code, message, param) = error_fields(&serde_json::from_str(&text).unwrap_or_default());
            let message = if message.is_empty() { text.chars().take(300).collect() } else { message };
            log(&format!("ChatGPT {}: {status} {code} {message}", a.email));
            if status == 401 && code.is_empty() && rejected.is_none() {
                self.cache.lock().unwrap().remove(&a.key);
                rejected = Some(token);
                continue;
            }
            return Err(with_reset(classify(status, &code, &message, &param), &serde_json::from_str(&text).unwrap_or_default()));
        }
    }
}

#[cfg(test)]
impl Pool {
    /// Accounts whose access token is their name, in a scratch folder, pointed at a fake API.
    pub fn fake(base: &str, names: &[&str]) -> Pool {
        let dir = std::env::temp_dir().join(format!("ccat-gpt-{}", crate::antigravity::random_hex(6)));
        let accounts = names
            .iter()
            .map(|n| {
                // Names starting "cx" are Codex-route accounts.
                let codex = n.starts_with("cx");
                let c = Creds {
                    client_id: "oaiapp_t".into(),
                    refresh_token: "r".into(),
                    access_token: n.to_string(),
                    expires_at: now() + 86_400,
                    scope: if codex { String::new() } else { PLAN_SCOPE.into() },
                    email: format!("{n}@x"),
                    sub: n.to_string(),
                    route: if codex { CODEX.into() } else { String::new() },
                    account_id: if codex { format!("acct-{n}") } else { String::new() },
                    ..Default::default()
                };
                write(&dir, n, &c).unwrap();
                ChatgptAccount { key: n.to_string(), email: c.email, granted: true, route: c.route, ..Default::default() }
            })
            .collect();
        let mut p = Pool::default();
        (p.accounts, p.dir, p.base, p.codex_base) = (Arc::new(RwLock::new(accounts)), dir, base.into(), base.into());
        p
    }
}

#[cfg(test)]
impl Drop for Pool {
    fn drop(&mut self) {
        if self.dir.starts_with(std::env::temp_dir()) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ccat-gpt-{tag}-{}", crate::antigravity::random_hex(4)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn creds(access: &str, expires_in: i64) -> Creds {
        Creds { client_id: "oaiapp_t".into(), refresh_token: "r1".into(), access_token: access.into(), expires_at: (now() as i64 + expires_in) as u64, scope: PLAN_SCOPE.into(), ..Default::default() }
    }

    /// The `Later` side of a result, for asserts (`TokenErr` has no Debug).
    fn kind(r: Result<(String, u64), TokenErr>) -> String {
        match r {
            Ok((t, _)) => format!("ok {t}"),
            Err(TokenErr::SignIn(_)) => "sign-in".into(),
            Err(TokenErr::Later(_)) => "later".into(),
        }
    }


    #[test]
    fn times_in_any_shape() {
        assert_eq!(unix_time(&json!(1_800_000_000)), 1_800_000_000);
        assert_eq!(unix_time(&json!(1_800_000_000_000u64)), 1_800_000_000, "milliseconds");
        assert_eq!(unix_time(&json!("1800000000")), 1_800_000_000);
        assert_eq!(unix_time(&json!("2027-01-15T08:00:00Z")), 1_800_000_000, "RFC 3339");
        assert_eq!(unix_time(&json!("2027-01-15T10:00:00+02:00")), 1_800_000_000);
        assert_eq!(unix_time(&json!("2027-01-15T08:00:00.5Z")), 1_800_000_000);
        assert_eq!((unix_time(&json!("soon")), unix_time(&json!(null))), (0, 0), "unreadable means no restriction");
    }

    fn jwt(claims: Value) -> String {
        format!("{}.{}.sig", B64U.encode("{}"), B64U.encode(claims.to_string()))
    }


    #[test]
    fn refresh_waits_for_the_lock_and_reuses_what_another_process_wrote() {
        let d = scratch("lock");
        write(&d, "k", &creds("old", 10)).unwrap(); // about to expire
        let lock = d.join("k.lock");
        std::fs::write(&lock, "").unwrap(); // another process is refreshing
        let (d2, lock2) = (d.clone(), lock.clone());
        let other = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            let mut c = creds("rotated", 3600);
            c.refresh_token = "r2".into();
            write(&d2, "k", &c).unwrap();
            std::fs::remove_file(lock2).unwrap();
        });
        let start = Instant::now();
        let got = access(&d, "k", None, &|_| panic!("the other process already refreshed: this must not refresh again"));
        assert_eq!(kind(got), "ok rotated");
        assert!(start.elapsed() >= Duration::from_millis(300), "it waited for the lock");
        other.join().unwrap();
        assert!(!lock.exists(), "released");

        // Nobody else did: refresh once, under the lock, and keep the rotated refresh token.
        write(&d, "k", &creds("old", 10)).unwrap();
        let calls = std::cell::Cell::new(0);
        let got = access(&d, "k", None, &|c| {
            calls.set(calls.get() + 1);
            assert!(c.refresh_token == "r1" && d.join("k.lock").exists(), "refreshing under the lock, with the current token");
            let mut n = c.clone();
            (n.access_token, n.refresh_token, n.expires_at) = ("new".into(), "r2".into(), now() + 3600);
            Ok(n)
        });
        assert_eq!((kind(got), calls.get()), ("ok new".into(), 1));
        assert_eq!(read(&d, "k").unwrap().refresh_token, "r2");
        assert!(!lock.exists());
        // Now valid for an hour: no more refreshing; but a token the API just refused is replaced.
        assert_eq!(kind(access(&d, "k", None, &|_| panic!("still valid"))), "ok new");
        assert_eq!(kind(access(&d, "k", Some("new"), &|c| Ok(Creds { access_token: "newer".into(), expires_at: now() + 3600, ..c.clone() }))), "ok newer");

        // A terminal answer marks the file; the account then needs a new sign-in without asking OpenAI again.
        write(&d, "k", &creds("old", 10)).unwrap();
        assert_eq!(kind(access(&d, "k", None, &|_| Err(TokenErr::SignIn("invalid_grant".into())))), "sign-in");
        assert!(read(&d, "k").unwrap().reauth && signed_out(&d, "k"));
        assert_eq!(kind(access(&d, "k", None, &|_| panic!("not asked again"))), "sign-in");

        // A network error keeps the credentials and the lock is released.
        write(&d, "k", &creds("old", 10)).unwrap();
        assert_eq!(kind(access(&d, "k", None, &|_| Err(TokenErr::Later("down".into())))), "later");
        assert!(!read(&d, "k").unwrap().reauth && !lock.exists());

        // Not before `earliest_refresh_at`: a token still alive is kept.
        let mut c = creds("alive", 100);
        c.earliest_refresh_at = now() + 1000;
        write(&d, "k", &c).unwrap();
        assert_eq!(kind(access(&d, "k", None, &|_| panic!("too early"))), "ok alive");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_stale_lock_is_taken_over() {
        let d = scratch("stale");
        write(&d, "k", &creds("old", 10)).unwrap();
        let lock = d.join("k.lock");
        let f = std::fs::File::create(&lock).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(90)).unwrap();
        drop(f);
        let got = access(&d, "k", None, &|c| Ok(Creds { access_token: "fresh".into(), expires_at: now() + 3600, ..c.clone() }));
        assert_eq!(kind(got), "ok fresh");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn codex_profiles_and_limits() {
        // Account id and plan from the claims of either token; the plan may only be in the ID token.
        let access = jwt(json!({ AUTH_CLAIM: { "chatgpt_account_id": "acct-1" }, "https://api.openai.com/profile": { "email": "Me@X.io " } }));
        let id = jwt(json!({ AUTH_CLAIM: { "chatgpt_plan_type": "Plus" }, "sub": "user-9" }));
        assert_eq!(codex_profile(&access, &id), ("acct-1".into(), "plus".into(), "me@x.io".into(), "user-9".into()));
        assert_eq!(codex_profile("junk", "junk").0, "");

        // A limit error that says when it lifts.
        let body = json!({ "error": { "type": "usage_limit_reached", "message": "m", "resets_in_seconds": 5400 } });
        let (code, message, param) = error_fields(&body);
        assert_eq!(code, "usage_limit_reached");
        let d = with_reset(classify(429, &code, &message, &param), &body);
        assert_eq!((d.failure.status, d.block, d.failure.retry_after), (429, Some(5400), Some(5400)));
        let at = json!({ "error": { "code": "usage_limit_reached", "message": "m", "resets_at": now() + 600 } });
        assert!(with_reset(classify(429, "usage_limit_reached", "m", ""), &at).block.is_some_and(|b| (590..=600).contains(&b)));
    }

    #[test]
    fn models_merge_what_the_api_says_with_codexs_catalogue() {
        let api = json!([
            { "slug": "gpt-6.1-sol", "visibility": "list" },
            { "slug": "gpt-reserve", "visibility": "hide", "display_name": "Reserve" },
            { "slug": "gpt-6-sol", "visibility": "list", "display_name": "GPT-6 Sol", "default_reasoning_level": "low", "supported_reasoning_levels": [{ "effort": "low" }] }
        ]);
        let cache = json!({ "models": [
            { "slug": "gpt-6.1-sol", "display_name": "GPT-6.1 Sol", "default_reasoning_level": "low",
              "supported_reasoning_levels": [{ "effort": "low" }, { "effort": "medium" }, { "effort": "high" }, { "effort": "xhigh" }, { "effort": "max" }, { "effort": "ultra" }] },
            { "slug": "gpt-6-sol", "display_name": "Other name", "default_reasoning_level": "high", "supported_reasoning_levels": [{ "effort": "high" }] }
        ] });
        let m = parse_models(api.as_array().unwrap(), &cache);
        assert_eq!(m.len(), 2, "only visibility list");
        assert_eq!((m[0].slug.as_str(), m[0].name.as_str(), m[0].default_effort.as_str()), ("gpt-6.1-sol", "GPT-6.1 Sol", "low"), "gaps come from the cache");
        assert_eq!(m[0].efforts, ["low", "medium", "high", "xhigh", "max"], "never ultra");
        assert_eq!((m[1].name.as_str(), m[1].efforts.clone()), ("GPT-6 Sol", vec!["low".to_string()]), "what the API said wins");
        assert_eq!(parse_models(api.as_array().unwrap(), &Value::Null)[0].name, "", "no cache: no name, the slug shows");
        assert_eq!(clamp_effort("max", &m[0].efforts), "max");
    }

    #[test]
    fn a_revoked_sign_in_says_so_instead_of_plan_usage() {
        let d = classify(401, "token_revoked", "Encountered invalidated oauth token for user, failing request", "");
        assert_eq!((d.failure.status, d.block), (401, Some(86_400)));
        let p = Pool::fake("http://127.0.0.1:1", &["cxa"]);
        p.block("cxa", 60, &d.failure);
        let f = p.exhausted();
        assert_eq!(f.status, 401);
        assert!(f.message.contains("invalidated oauth token") && f.message.contains("codex login") && !f.message.contains("plan usage"), "{}", f.message);
    }

    #[test]
    fn errors_tell_the_pool_what_to_do() {
        let d = |status, code| classify(status, code, "m", "");
        let (limit, eligible, down, scope) = (d(429, "subscription_sharing_usage_limit_exceeded"), d(403, "subscription_sharing_user_not_eligible"), d(503, "subscription_sharing_usage_unavailable"), d(403, "chatpass_v2_scope_not_authorized"));
        assert_eq!((limit.failure.status, limit.block), (429, Some(3600)));
        assert_eq!(limit.failure.retry_after, Some(3600));
        assert_eq!((eligible.block, down.block, scope.block), (Some(86_400), Some(30), Some(86_400)));
        assert_eq!(d(502, "").block, Some(30), "a plain 5xx is a short backoff");
        assert_eq!(classify(0, "server_error", "boom", "").failure.status, 502, "inside an open stream");
        let u = classify(400, "subscription_sharing_unsupported_capability", "no", "tools[0]");
        assert!(u.block.is_none() && u.param == "tools[0]" && u.failure.message.contains("tools[0]"), "no retry elsewhere; the param is shown");
        assert!(classify(500, "x", "no", "background").param.is_empty());
        assert_eq!(error_fields(&json!({ "response": { "error": { "code": "c", "message": "m", "param": "p" } } })), ("c".into(), "m".into(), "p".into()));
        assert_eq!(error_fields(&json!({ "type": "error", "code": "c", "message": "m" })).0, "c");
    }

    #[test]
    fn rate_headers_are_named_and_auth_never() {
        let mut h = ureq::http::HeaderMap::new();
        for (k, v) in [("x-ratelimit-remaining", "42"), ("x-usage-reset", "soon"), ("content-type", "text/event-stream"), ("authorization-limit", "9")] {
            h.insert(ureq::http::HeaderName::from_static(k), v.parse().unwrap());
        }
        let mut got = limit_headers(&h);
        got.sort();
        assert_eq!(got, ["x-ratelimit-remaining=42", "x-usage-reset"]);
    }
}
