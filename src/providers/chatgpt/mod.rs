//! ChatGPT plan usage through OpenAI's official "Sign in with ChatGPT" token sharing: ccat's own browser sign-in (OAuth +
//! PKCE), per-account credential files, a cross-process refresh lock, and the account pool the proxy draws from.
//! Protocol: https://developers.openai.com/siwc/token-sharing-open-source
//!
//! Two modes (`Pool::legacy`, switched in Settings): Official sends the token to `api.openai.com/v1/responses`;
//! Codex (Legacy) sends it to the undocumented `chatgpt.com/backend-api/codex/responses` the way Codex CLI does. One
//! sign-in serves both: the same token is tried on the Codex backend, and an account it refuses there is marked
//! (`Creds::legacy_refused_at`) and offered a second, Codex-client sign-in (`route` "codex"). ccat never reads Codex CLI's files.
//!
//! Secrets live in `%APPDATA%\ccat\chatgpt\<key>.json`, never in config.json (which several ccat processes read and
//! save): the config only keeps non-secret metadata (`ChatgptAccount`). Every routed session runs its own proxy and they
//! share these accounts, so refreshes (the refresh token rotates; using an old one is a terminal error) are serialised
//! across processes by a lock file. The UI process signs in and out; the proxy only reads.

pub(crate) mod proxy;
mod ui;
pub(crate) use ui::page;

use crate::{
    common::{agent, log, random_hex, uuid, Failure},
    now,
    providers::antigravity::{self, enc},
};
use crate::usage::{plan_name, Bar, FIVE_HOURS, WEEK};
use base64::{
    engine::general_purpose::URL_SAFE_NO_PAD as B64U,
    Engine,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, RwLock,
    },
    time::{Duration, Instant, SystemTime},
};

const ISSUER: &str = "https://auth.openai.com";
const AUTH_URL: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
const DISCOVERY_URL: &str = "https://auth.openai.com/.well-known/openid-configuration";
/// The client id of a first registration; OpenAI answers with the `oaiapp_…` id this install uses from then on.
const FIRST_CLIENT: &str = "dynamic_agent_client";
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPES: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const RESOURCE: &str = "https://api.openai.com/v1";
/// Where plan usage is spent (models, inference) in Official mode.
pub const API: &str = RESOURCE;
pub const USAGE_PAGE: &str = "https://chatgpt.com/settings/usage";
/// An access token is replaced this long before it expires.
const EARLY: u64 = 300;

// The Codex (Legacy) backend: what OpenAI's Codex CLI talks to; OpenAI does not support it for third-party apps. Its
// sign-in (the public Codex OAuth client and its fixed loopback port) is the optional second one.
const CODEX_CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";
const CODEX_PORT: u16 = 1455;
const CODEX_REDIRECT: &str = "http://localhost:1455/auth/callback";
pub const CODEX_BASE: &str = "https://chatgpt.com/backend-api";
/// The Codex CLI version the backend is told (it gates newer models on it, see oh-my-pi's `CODEX_CLIENT_VERSION`).
/// ponytail: fixed; ccat has no Codex release check (its only one is for ccat itself) and never reads Codex's files for
/// it. When the backend starts refusing models, bump it by hand, or fetch the latest release tag at startup.
const CODEX_VERSION: &str = "0.159.0";
const ORIGINATOR: &str = "ccat";
pub const WHAM_USAGE: &str = "https://chatgpt.com/backend-api/wham/usage";
/// `ChatgptAccount::route` of an account signed in with Codex's client (works in Legacy mode only).
pub const CODEX: &str = "codex";
/// `ChatgptAccount::route` of an account an older ccat imported from Codex CLI's `~/.codex/auth.json`. ccat no longer
/// reads that file, so such an account has no token: it only stays as a row until a new sign-in replaces it.
pub const CODEX_CLI: &str = "codex-cli";
/// Request bodies over this many bytes are zstd-compressed on the Legacy route, as Codex does.
const ZSTD_OVER: usize = 256 * 1024;
/// How long Legacy mode leaves a Sign in with ChatGPT token the Codex backend refused alone before trying it again.
const LEGACY_RETRY: u64 = 3600;
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
    /// "" = ccat's Sign in with ChatGPT (both modes); `CODEX` = signed in with Codex's client (Legacy only);
    /// `CODEX_CLI` = a leftover import from Codex CLI (never usable, see `CODEX_CLI`).
    pub route: String,
    /// The plan type from the token's claims ("plus", "pro"…); "" when they don't carry it.
    pub plan: String,
}

impl ChatgptAccount {
    pub fn usable(&self) -> bool {
        !self.disabled && self.granted
    }

    /// Signed in with Codex's client: Legacy mode only.
    pub fn own_codex(&self) -> bool {
        self.route == CODEX
    }

    /// An import from Codex CLI left by an older ccat.
    pub fn cli(&self) -> bool {
        self.route == CODEX_CLI
    }

    /// Whether the mode can ever use this account: Official only takes the Sign in with ChatGPT token; Legacy takes it
    /// too (tried, see `Creds::legacy_refused_at`) and Codex-client sign-ins.
    pub fn runs(&self, legacy: bool) -> bool {
        match self.route.as_str() {
            CODEX_CLI => false,
            CODEX => legacy,
            _ => true,
        }
    }
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
    /// When the Codex backend last refused this Sign in with ChatGPT token even after a fresh refresh (unix seconds, 0 =
    /// never): Legacy mode skips the account for `LEGACY_RETRY` after that, then tries the token again. Official mode is
    /// not affected (unlike `reauth`).
    pub legacy_refused_at: u64,
    /// When this account was last signed in (a refresh keeps it): a block set before it is lifted (see `Pool::pick`).
    pub signed_in: u64,
}

impl Creds {
    /// Legacy mode is skipping this token: the Codex backend refused it less than `LEGACY_RETRY` ago.
    fn refused(&self) -> bool {
        self.legacy_refused_at > 0 && now() < self.legacy_refused_at + LEGACY_RETRY
    }
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

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74,
    0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d,
    0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e,
    0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5,
    0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 (FIPS 180-4). Here only for PKCE and account keys, so a crate would be a lot of dependency for ~30 lines.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&(data.len() as u64 * 8).to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, c) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let t1 = v[7].wrapping_add(s1).wrapping_add((v[4] & v[5]) ^ (!v[4] & v[6])).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let t2 = s0.wrapping_add((v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]));
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for (x, y) in h.iter_mut().zip(v) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 32];
    for (o, x) in out.chunks_mut(4).zip(h) {
        o.copy_from_slice(&x.to_be_bytes());
    }
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// PKCE S256: base64url (no padding) of the SHA-256 of the verifier.
fn challenge(verifier: &str) -> String {
    B64U.encode(sha256(verifier.as_bytes()))
}

/// Names the account's credential file, so the email (which can change) never does.
pub fn key_of(sub: &str) -> String {
    hex(&sha256(sub.as_bytes()))[..16].to_string()
}

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

/// One id per install, `urn:uuid:<v4>`: OpenAI ties the registrations of this install together with it.
fn host_id(dir: &Path) -> Result<String, String> {
    let path = dir.join("host-id");
    if let Some(id) = std::fs::read_to_string(&path).ok().map(|t| t.trim().to_string()).filter(|t| t.starts_with("urn:uuid:")) {
        return Ok(id);
    }
    make_dir(dir).map_err(|e| e.to_string())?;
    let id = format!("urn:uuid:{}", uuid());
    write_private(&path, id.as_bytes())?;
    Ok(id)
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

/// No credential file (a `CODEX_CLI` leftover has none), marked "sign in again", or a Sign in with ChatGPT token without
/// the plan-usage scope (a Codex token has no such scope).
fn signed_out_by(c: Option<&Creds>) -> bool {
    c.is_none_or(|c| c.reauth || (c.route != CODEX && !c.scope.split_whitespace().any(|s| s == PLAN_SCOPE)))
}

fn signed_out(dir: &Path, key: &str) -> bool {
    signed_out_by(read(dir, key).as_ref())
}

/// The Codex backend recently refused this account's Sign in with ChatGPT token (see `Creds::legacy_refused_at`).
pub fn legacy_refused(key: &str) -> bool {
    read(&dir(), key).is_some_and(|c| c.refused())
}

/// Remembers that the Codex backend refused the account's token. Under the account's lock and on a fresh read, so a
/// refresh another process just wrote is not overwritten; best effort (a lock that can't be had changes nothing).
fn refuse_legacy(dir: &Path, key: &str) {
    let Ok(_lock) = Lock::take(dir, key) else { return };
    if let Some(mut c) = read(dir, key).filter(|c| !c.refused()) {
        c.legacy_refused_at = now();
        let _ = write(dir, key, &c);
    }
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

/// The account id and plan the tokens' claims carry, kept when they do (an opaque Sign in with ChatGPT token may not).
fn fill_profile(c: &mut Creds) {
    let (account_id, plan, ..) = codex_profile(&c.access_token, &c.id_token);
    for (field, new) in [(&mut c.account_id, account_id), (&mut c.plan, plan)] {
        if !new.is_empty() {
            *field = new;
        }
    }
}

fn refresh_tokens(c: &Creds) -> Result<Creds, TokenErr> {
    let mut n = c.clone();
    let v = if c.route == CODEX {
        token_post(CODEX_TOKEN_URL, &[("grant_type", "refresh_token"), ("client_id", &c.client_id), ("refresh_token", &c.refresh_token)])?
    } else {
        token_post(TOKEN_URL, &[("grant_type", "refresh_token"), ("client_id", &c.client_id), ("refresh_token", &c.refresh_token), ("resource", RESOURCE)])?
    };
    apply(&mut n, &v)?;
    fill_profile(&mut n);
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

/// Codex's request headers (the same on the model list and on inference), minus the per-request ones. An account whose
/// token doesn't name its ChatGPT workspace (`account_id` empty) is sent without that header, and the backend decides.
fn codex_headers(token: &str, account_id: &str) -> Vec<(&'static str, String)> {
    let mut h = vec![("Authorization", format!("Bearer {token}"))];
    if !account_id.is_empty() {
        h.push(("chatgpt-account-id", account_id.to_string()));
    }
    h.extend([
        ("originator", ORIGINATOR.to_string()),
        ("OpenAI-Beta", "responses=experimental".to_string()),
        ("version", CODEX_VERSION.to_string()),
        ("User-Agent", concat!("ccat/", env!("CARGO_PKG_VERSION")).to_string()),
    ]);
    h
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

/// The account's listed models (`visibility: "list"`) with what the answer says about each (display name, reasoning
/// levels). Legacy mode (and a Codex-client account) asks the Codex backend, as Codex does; a Sign in with ChatGPT token it
/// refuses falls back to the official `/models`.
pub fn list_models(key: &str, legacy: bool) -> Result<Vec<ChatgptModel>, String> {
    let d = dir();
    let creds = read(&d, key).ok_or("This ChatGPT account must be signed in again.")?;
    let (token, _) = access(&d, key, None, &refresh_tokens).map_err(|e| e.message().to_string())?;
    if creds.route == CODEX || legacy {
        let url = format!("{CODEX_BASE}/codex/models?client_version={CODEX_VERSION}");
        match get_json(&url, &codex_headers(&token, &creds.account_id)) {
            Ok(v) => return Ok(parse_models(v["models"].as_array().ok_or("response has no `models`")?)),
            Err(e) if creds.route == CODEX => return Err(e),
            Err(_) => {}
        }
    }
    let v = get_json(&format!("{API}/models"), &[("Authorization", format!("Bearer {token}"))])?;
    Ok(parse_models(v["models"].as_array().ok_or("response has no `models`")?))
}

/// One model entry as far as it says anything.
fn model_of(m: &Value) -> ChatgptModel {
    let efforts = m["supported_reasoning_levels"].as_array().into_iter().flatten().filter_map(|l| l["effort"].as_str().or(l.as_str())).filter(|e| *e != "ultra").map(String::from).collect();
    ChatgptModel {
        slug: m["slug"].as_str().unwrap_or("").into(),
        name: m["display_name"].as_str().unwrap_or("").into(),
        efforts,
        default_effort: m["default_reasoning_level"].as_str().unwrap_or("").into(),
    }
}

/// The listed models (`visibility: "list"`), in the server's order.
fn parse_models(api: &[Value]) -> Vec<ChatgptModel> {
    api.iter().filter(|m| m["visibility"] == "list").map(model_of).filter(|m| !m.slug.is_empty()).collect()
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

/// (access token, ChatGPT account id or "") of an account, for the usage call; refreshed under the usual lock.
pub fn account_token(key: &str) -> Result<(String, String), String> {
    let d = dir();
    let (token, _) = access(&d, key, None, &refresh_tokens).map_err(|e| e.message().to_string())?;
    let id = read(&d, key).map(|c| c.account_id).unwrap_or_default();
    Ok((token, id))
}

/// Revokes the account's refresh token at OpenAI, then deletes its local files, revoked or not. Blocking: run it on a thread.
pub fn sign_out(key: &str) {
    let d = dir();
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

/// The `/authorize` URL. `again` is the saved (client id, ID token, email) of an account being signed in again: it keeps
/// its issued client id and gets hints, and is not a new registration (no `agent_name_hint`).
fn authorize_url(port: u16, host: &str, state: &str, nonce: &str, verifier: &str, again: Option<(&str, &str, &str)>) -> String {
    let redirect = format!("http://127.0.0.1:{port}/auth/callback");
    let mut q = vec![
        ("response_type", "code"),
        ("client_id", again.map_or(FIRST_CLIENT, |a| a.0)),
        ("ext_agent_host_id", host),
        ("redirect_uri", &redirect),
        ("scope", SCOPES),
        ("resource", RESOURCE),
        ("state", state),
        ("nonce", nonce),
        ("code_challenge_method", "S256"),
    ];
    let code_challenge = challenge(verifier);
    q.push(("code_challenge", &code_challenge));
    match again {
        None => q.push(("agent_name_hint", "ccat")),
        Some((_, id_token, email)) => {
            q.extend([("id_token_hint", id_token), ("login_hint", email)].into_iter().filter(|(_, v)| !v.is_empty()));
        }
    }
    format!("{AUTH_URL}?{}", q.iter().map(|(k, v)| format!("{k}={}", enc(v))).collect::<Vec<_>>().join("&"))
}

/// The Codex `/oauth/authorize` URL, with the parameters the Codex CLI sends.
fn codex_authorize_url(state: &str, verifier: &str) -> String {
    let q = [
        ("response_type", "code"),
        ("client_id", CODEX_CLIENT),
        ("redirect_uri", CODEX_REDIRECT),
        ("scope", CODEX_SCOPE),
        ("code_challenge", &challenge(verifier)),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", ORIGINATOR),
    ];
    format!("{CODEX_AUTH_URL}?{}", q.iter().map(|(k, v)| format!("{k}={}", enc(v))).collect::<Vec<_>>().join("&"))
}

/// Browser sign-in. `start` binds the loopback port first (so the redirect always has a listener, and a failure shows
/// before the browser opens); `finish` blocks until OpenAI redirects back, then trades the code for tokens.
pub struct Login {
    listener: TcpListener,
    state: String,
    nonce: String,
    verifier: String,
    redirect: String,
    /// The issued client id of the account being signed in again; none for a first registration.
    saved_client: Option<String>,
    /// The Codex route (see `CODEX`) instead of the official one.
    codex: bool,
    dir: PathBuf,
    pub url: String,
}

impl Login {
    /// `again`: an official account to sign in again (it keeps its client id and gets hints); else a new registration.
    /// `codex`: the Codex route, whose redirect port is fixed (1455, like the Codex CLI's).
    pub fn start(again: Option<&ChatgptAccount>, codex: bool) -> Result<Login, String> {
        Self::start_in(&dir(), again, codex)
    }

    fn start_in(dir: &Path, again: Option<&ChatgptAccount>, codex: bool) -> Result<Login, String> {
        // The docs' example registers on 1455 (and only the port may differ later); a busy 1455 (a Codex login) falls back to any.
        let listener = if codex { TcpListener::bind(("127.0.0.1", CODEX_PORT)) } else { TcpListener::bind(("127.0.0.1", CODEX_PORT)).or_else(|_| TcpListener::bind(("127.0.0.1", 0))) }.map_err(|e| {
            if codex {
                format!("Port {CODEX_PORT} is busy (a Codex login may still be open); finish or close it and try again: {e}")
            } else {
                format!("Could not listen for the ChatGPT sign-in: {e}")
            }
        })?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let verifier = B64U.encode(random_hex(32));
        let (state, nonce) = (random_hex(16), random_hex(16));
        let login = |url: String, redirect: String, saved_client: Option<String>| Login { listener, state: state.clone(), nonce: nonce.clone(), verifier: verifier.clone(), redirect, saved_client, codex, dir: dir.to_path_buf(), url };
        if codex {
            return Ok(login(codex_authorize_url(&state, &verifier), CODEX_REDIRECT.into(), None));
        }
        let saved = again.filter(|a| a.route.is_empty()).and_then(|a| read(dir, &a.key)).filter(|c| !c.client_id.is_empty());
        let hint = saved.as_ref().map(|c| (c.client_id.as_str(), c.id_token.as_str(), c.email.as_str()));
        let url = authorize_url(port, &host_id(dir)?, &state, &nonce, &verifier, hint);
        Ok(login(url, format!("http://127.0.0.1:{port}/auth/callback"), saved.map(|c| c.client_id)))
    }

    pub fn finish(self, cancel: &AtomicBool) -> Result<ChatgptAccount, String> {
        let q = antigravity::accept(&self.listener, cancel, "/auth/callback", &self.state, "ChatGPT").map_err(|e| {
            if e.contains("access_denied") { "Sign-in was declined in ChatGPT.".to_string() } else { e }
        })?;
        let code = &q["code"];
        if self.codex {
            return self.finish_codex(code);
        }
        let issued = q.get("client_id").filter(|c| !c.is_empty());
        let client = match (&self.saved_client, issued) {
            (None, Some(id)) => id.clone(),
            (None, None) => return Err("OpenAI did not issue a client id for ccat; try again.".into()),
            (Some(old), Some(id)) if id != old => return Err("OpenAI answered with a different client id than this account's; refusing it.".into()),
            (Some(old), _) => old.clone(),
        };
        let tok = token_post(
            TOKEN_URL,
            &[("grant_type", "authorization_code"), ("client_id", &client), ("code", code), ("code_verifier", &self.verifier), ("redirect_uri", &self.redirect), ("resource", RESOURCE)],
        )
        .map_err(|e| e.message().to_string())?;
        let (sub, email) = check_id_token(tok["id_token"].as_str().unwrap_or(""), &client, &self.nonce, now())?;
        let mut c = Creds { client_id: client, email: email.clone(), sub: sub.clone(), signed_in: now(), ..Default::default() };
        apply(&mut c, &tok).map_err(|e| e.message().to_string())?;
        if c.refresh_token.is_empty() {
            return Err("OpenAI returned no refresh token; try again.".into());
        }
        fill_profile(&mut c);
        let granted = c.scope.split_whitespace().any(|s| s == PLAN_SCOPE);
        let key = key_of(&sub);
        write(&self.dir, &key, &c)?;
        log(&format!("ChatGPT signed in {email} (plan usage {})", if granted { "granted" } else { "NOT granted" }));
        Ok(ChatgptAccount { key, email, granted, plan: c.plan, ..Default::default() })
    }

    /// The Codex route: ccat's own tokens from the public Codex client. Codex CLI's `~/.codex/auth.json` is never
    /// touched: its refresh tokens rotate, so refreshing its token from here would sign the Codex CLI out.
    fn finish_codex(&self, code: &str) -> Result<ChatgptAccount, String> {
        let tok = token_post(
            CODEX_TOKEN_URL,
            &[("grant_type", "authorization_code"), ("client_id", CODEX_CLIENT), ("code", code), ("code_verifier", &self.verifier), ("redirect_uri", CODEX_REDIRECT)],
        )
        .map_err(|e| e.message().to_string())?;
        let mut c = Creds { client_id: CODEX_CLIENT.into(), route: CODEX.into(), signed_in: now(), ..Default::default() };
        apply(&mut c, &tok).map_err(|e| e.message().to_string())?;
        let (account_id, plan, email, sub) = codex_profile(&c.access_token, &c.id_token);
        if account_id.is_empty() || c.refresh_token.is_empty() {
            return Err("OpenAI's Codex sign-in returned no ChatGPT account id or refresh token; try again.".into());
        }
        let who = if sub.is_empty() { email.clone() } else { sub.clone() };
        (c.account_id, c.plan, c.email, c.sub) = (account_id, plan.clone(), email.clone(), sub);
        let key = key_of(&format!("codex:{who}"));
        write(&self.dir, &key, &c)?;
        log(&format!("ChatGPT (Codex route) signed in {email} (plan {plan})"));
        Ok(ChatgptAccount { key, email, granted: true, route: CODEX.into(), plan, ..Default::default() })
    }
}

/// Checks an ID token and returns (`sub`, email). It arrives straight from the token endpoint over TLS, so (OpenID Connect
/// Core 3.1.3.7) its signature is not verified; the claims that bind it to this sign-in are: issuer, audience (our issued
/// client id), expiry and the nonce we sent. `sub` is the account's identity, `email` only for display.
fn check_id_token(jwt: &str, client_id: &str, nonce: &str, now: u64) -> Result<(String, String), String> {
    let bad = |what: &str| format!("OpenAI's ID token is not valid ({what}); try again.");
    let payload = jwt.split('.').nth(1).ok_or_else(|| bad("missing"))?;
    let claims: Value = B64U.decode(payload.trim_end_matches('=')).ok().and_then(|b| serde_json::from_slice(&b).ok()).ok_or_else(|| bad("unreadable"))?;
    let aud = &claims["aud"];
    match () {
        _ if claims["iss"] != ISSUER => Err(bad("issuer")),
        _ if !(aud == client_id || aud.as_array().is_some_and(|a| a.iter().any(|x| x == client_id))) => Err(bad("audience")),
        _ if claims["exp"].as_u64().is_none_or(|e| e <= now) => Err(bad("expired")),
        _ if claims["nonce"] != nonce => Err(bad("nonce")),
        _ => match claims["sub"].as_str().filter(|s| !s.is_empty()) {
            Some(sub) => Ok((sub.to_string(), claims["email"].as_str().unwrap_or("").to_string())),
            None => Err(bad("no subject")),
        },
    }
}

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
        // The conversation is too long, not the account's fault: no block, no other account; Claude Code compacts on this message.
        "context_length_exceeded" => (400, None),
        _ if status == 0 => (502, Some(30)),
        _ if status >= 500 => (status, Some(30)),
        _ if status == 429 => (429, Some(60)),
        _ if status == 401 || status == 403 => (status, Some(3600)),
        _ => (status, None),
    };
    let unsupported = code == "subscription_sharing_unsupported_capability";
    let message = match (unsupported, param.is_empty()) {
        _ if code == "context_length_exceeded" => format!("prompt is too long: {message}"),
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
    /// Codex (Legacy) mode instead of Official; set from the config (`set_legacy`), so a switch reaches running sessions.
    legacy: AtomicBool,
    /// The official API refused `prompt_cache_key` once: dropped for the rest of the process (Legacy always sends it).
    pub no_cache_key: AtomicBool,
    /// What refreshes a token (tests swap in one that stays offline); `None` = OpenAI's token endpoint.
    pub(crate) refresher: Option<fn(&Creds) -> Result<Creds, TokenErr>>,
    /// Shared like `accounts`: what the config says about each model.
    pub models: Arc<RwLock<Vec<ChatgptModel>>>,
}

impl Pool {
    pub fn new(accounts: Vec<ChatgptAccount>, models: Vec<ChatgptModel>) -> Pool {
        let mut p = Pool::default();
        (p.accounts, p.models, p.dir, p.base, p.codex_base) = (Arc::new(RwLock::new(accounts)), Arc::new(RwLock::new(models)), dir(), API.into(), CODEX_BASE.into());
        p
    }

    pub fn legacy(&self) -> bool {
        self.legacy.load(Ordering::Relaxed)
    }

    /// Switches mode. Everything learned about the accounts in the other one (blocks, a Codex refusal, cached tokens) is
    /// forgotten: a 401 on one route says nothing about the other.
    pub fn set_legacy(&self, on: bool) {
        if self.legacy.swap(on, Ordering::Relaxed) != on {
            self.blocked.lock().unwrap().clear();
            self.since.lock().unwrap().clear();
            self.cache.lock().unwrap().clear();
        }
    }

    /// Not usable in this mode: off, granted nothing, a route the mode can't take, signed out, or (Legacy) a Sign in with
    /// ChatGPT token the Codex backend refused.
    fn off(&self, a: &ChatgptAccount) -> bool {
        let legacy = self.legacy();
        if !a.usable() || !a.runs(legacy) {
            return true;
        }
        let c = read(&self.dir, &a.key);
        signed_out_by(c.as_ref()) || (legacy && c.is_some_and(|c| c.refused()))
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
        let accounts = self.accounts.read().unwrap();
        accounts.iter().for_each(|a| self.lift_resigned(&a.key));
        accounts.iter().find(|a| !self.off(a) && self.blocked_for(&a.key).is_none()).cloned()
    }

    /// A sign-in made after an account's 401/403 block was set lifts it: that block was about the old sign-in (the key is a
    /// hash of the user id, so signing in again keeps it). The token cached for the old sign-in goes too.
    fn lift_resigned(&self, key: &str) {
        let Some(since) = self.since.lock().unwrap().get(key).copied() else { return };
        if !self.blocked.lock().unwrap().get(key).is_some_and(|(_, st, _)| matches!(st, 401 | 403)) || read(&self.dir, key).is_none_or(|c| c.signed_in <= since) {
            return;
        }
        log(&format!("ChatGPT {key} signed in again: its block is lifted"));
        self.blocked.lock().unwrap().remove(key);
        self.since.lock().unwrap().remove(key);
        self.cache.lock().unwrap().remove(key);
    }

    /// Why `pick` found nothing.
    pub fn exhausted(&self) -> Failure {
        let accounts = self.accounts.read().unwrap();
        let usable: Vec<&ChatgptAccount> = accounts.iter().filter(|a| !self.off(a)).collect();
        let fail = |message: &str| Failure { status: 403, message: message.into(), retry_after: None };
        if accounts.is_empty() {
            return fail("No ChatGPT accounts. Sign in with ChatGPT in ccat → Settings → ChatGPT.");
        }
        if usable.is_empty() {
            let refused = self.legacy() && accounts.iter().any(|a| a.usable() && read(&self.dir, &a.key).is_some_and(|c| c.refused()));
            return fail(if refused {
                "Codex (Legacy) needs its own sign-in: the Codex backend refused the ChatGPT sign-in. Use \"Sign in for Codex (Legacy)\" in ccat → Settings → ChatGPT, or switch to Official."
            } else {
                "No ChatGPT account can be used in this mode: turn one on, or sign in (again) in ccat → Settings → ChatGPT (plan usage must be granted)."
            });
        }
        // The account back soonest, and why it is out.
        let blocked = self.blocked.lock().unwrap();
        let soonest = usable.iter().filter_map(|a| blocked.get(&a.key)).filter(|(t, ..)| *t > Instant::now()).min_by_key(|(t, ..)| *t);
        let (secs, status, why) = soonest.map_or((60, 429, String::new()), |(t, st, m)| (t.saturating_duration_since(Instant::now()).as_secs().max(1), *st, m.clone()));
        let wait = format!("{}m{}s", secs / 60, secs % 60);
        match status {
            429 => Failure { status: 429, message: format!("All {} ChatGPT account(s) are over their plan usage; trying again in {wait}.", usable.len()), retry_after: Some(secs) },
            401 => Failure { status: 401, message: format!("{why}. Sign in again in ccat → Settings → ChatGPT."), retry_after: None },
            st => Failure { status: st, message: format!("{why} (ChatGPT is skipped for {wait})."), retry_after: None },
        }
    }

    /// A valid access token for the account, from memory or through the locked refresh (see `access`).
    fn token(&self, a: &ChatgptAccount, rejected: Option<&str>) -> Result<String, TokenErr> {
        if rejected.is_none() {
            if let Some((t, _)) = self.cache.lock().unwrap().get(&a.key).filter(|(_, until)| *until > Instant::now()) {
                return Ok(t.clone());
            }
        }
        let refresh = self.refresher.unwrap_or(refresh_tokens);
        let (t, expires) = access(&self.dir, &a.key, rejected, &refresh)?;
        let life = expires.saturating_sub(now() + EARLY);
        self.cache.lock().unwrap().insert(a.key.clone(), (t.clone(), Instant::now() + Duration::from_secs(life)));
        Ok(t)
    }

    /// Opens the `/responses` SSE stream for `body` on account `a`, in `legacy` mode (the one `body` was built for; `session`:
    /// Claude Code's session id, the cache affinity on both routes). A refused token gets one forced refresh; one the Codex backend still refuses is
    /// marked (`Creds::legacy_refused_at`).
    pub fn open(&self, a: &ChatgptAccount, body: &Value, session: &str, legacy: bool) -> Result<Box<dyn BufRead + Send>, Denied> {
        let deny = |status: u16, message: String, block: u64| Denied { failure: Failure { status, message, retry_after: None }, block: Some(block), param: String::new() };
        let (bytes, zstd) = encode_body(body, legacy);
        let mut rejected: Option<String> = None;
        // The refusal that made us refresh: the answer if the refresh can't run now.
        let mut prior: Option<Denied> = None;
        loop {
            let token = match self.token(a, rejected.as_deref()) {
                Ok(t) => t,
                Err(TokenErr::Later(_)) if prior.is_some() => return Err(prior.take().unwrap()),
                // The credential file is already marked; the pick skips this account until the user signs in again.
                Err(TokenErr::SignIn(m)) => return Err(deny(403, format!("{}: {m}", a.email), 86_400)),
                Err(TokenErr::Later(m)) => return Err(deny(502, format!("{}: {m}", a.email), 30)),
            };
            let (url, mut headers) = if legacy {
                let id = read(&self.dir, &a.key).map(|c| c.account_id).unwrap_or_default();
                // A Codex-client token always names its workspace; a Sign in with ChatGPT one may not, and is tried without.
                if id.is_empty() && a.own_codex() {
                    return Err(deny(403, format!("{}: no ChatGPT account id; sign in again.", a.email), 86_400));
                }
                let mut h = codex_headers(&token, &id);
                // As Codex sends it: which model the request is routed for.
                h.push(("x-codex-routing-hint", format!("model={}", body["model"].as_str().unwrap_or(""))));
                (format!("{}/codex/responses", self.codex_base), h)
            } else {
                (format!("{}/responses", self.base), vec![("Authorization", format!("Bearer {token}")), ("User-Agent", concat!("ccat/", env!("CARGO_PKG_VERSION")).to_string())])
            };
            // Cache affinity, as current Codex sends it (hyphenated names), on both routes.
            headers.extend([("session-id", session.to_string()), ("thread-id", session.to_string()), ("x-client-request-id", session.to_string())]);
            headers.extend([("Content-Type", "application/json".to_string()), ("Accept", "text/event-stream".to_string())]);
            if zstd {
                headers.push(("Content-Encoding", "zstd".to_string()));
            }
            let mut req = agent(None).post(&url);
            for (k, v) in &headers {
                req = req.header(*k, v);
            }
            let sent = req.send(&bytes[..]);
            let mut res = sent.map_err(|e| deny(502, format!("{url}: {e}"), 30))?;
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
            // A plain 401 (no code) may only be a stale token: refresh once and ask again.
            if status == 401 && code.is_empty() && rejected.is_none() {
                self.cache.lock().unwrap().remove(&a.key);
                prior = Some(with_reset(classify(status, &code, &message, &param), &serde_json::from_str(&text).unwrap_or_default()));
                rejected = Some(token);
                continue;
            }
            // Still a plain 401 with a freshly refreshed Sign in with ChatGPT token: the Codex backend doesn't take it. The
            // account is marked for `LEGACY_RETRY` (Official is unaffected); then the token is tried again.
            // ponytail: only that signal marks; a 403 or a 401 with a code is shown as the error it is, because the
            // Codex backend's codes for a foreign token are not known. Add them here once a live run names them.
            if legacy && a.route.is_empty() && status == 401 && code.is_empty() && rejected.is_some() {
                refuse_legacy(&self.dir, &a.key);
                return Err(deny(403, format!("{}: Codex (Legacy) needs its own sign-in ({status} {message}); use \"Sign in for Codex (Legacy)\", or switch to Official.", a.email), LEGACY_RETRY));
            }
            return Err(with_reset(classify(status, &code, &message, &param), &serde_json::from_str(&text).unwrap_or_default()));
        }
    }
}

/// The request's bytes, and whether they are zstd-compressed: on the Legacy route a body over `ZSTD_OVER` is, as Codex
/// does (a long conversation resent every turn is mostly repetition).
/// ponytail: the Official route is never compressed: OpenAI's Sign in with ChatGPT docs don't say it accepts
/// `Content-Encoding: zstd`; try it there if long requests get slow, and keep the plain body as the fallback.
pub fn encode_body(body: &Value, legacy: bool) -> (Vec<u8>, bool) {
    let plain = body.to_string().into_bytes();
    if legacy && plain.len() > ZSTD_OVER {
        if let Ok(z) = zstd::encode_all(&plain[..], 3) {
            return (z, true);
        }
    }
    (plain, false)
}

// ---------------------------------------------------------------- usage

/// (plan, bars) from `/backend-api/wham/usage`: its primary (5-hour) and secondary (weekly) rate-limit windows.
pub fn parse_wham(v: &Value, now: u64) -> (String, Vec<Bar>) {
    let window = |w: &Value, label: &str, default: u64| {
        let used = w["used_percent"].as_f64()?;
        let resets = w["reset_at"].as_u64().filter(|r| *r > 0).or_else(|| w["reset_after_seconds"].as_u64().map(|s| now + s)).unwrap_or(0);
        Some(Bar { label: label.into(), used, resets, window: w["limit_window_seconds"].as_u64().filter(|s| *s > 0).unwrap_or(default), ..Default::default() })
    };
    let limit = &v["rate_limit"];
    let bars = [window(&limit["primary_window"], "Session (5hr)", FIVE_HOURS), window(&limit["secondary_window"], "Weekly (7 day)", WEEK)].into_iter().flatten().collect();
    (plan_name(v["plan_type"].as_str().unwrap_or(""), ""), bars)
}

/// The ChatGPT provider's mode, as the usage call needs it.
fn gpt_mode(cfg: &crate::Config) -> bool {
    cfg.providers.iter().find(|p| p.chatgpt).is_some_and(|p| p.gpt_legacy())
}

/// Whether there is a login to ask with: a usable ChatGPT account that can serve the current mode (not a leftover Codex
/// CLI import, and not a Codex-client sign-in in Official mode).
pub(crate) fn gpt_has_login(cfg: &crate::Config) -> bool {
    let legacy = gpt_mode(cfg);
    cfg.providers.iter().filter(|p| p.chatgpt).flat_map(|p| &p.chatgpt_accounts).any(|a| a.usable() && a.runs(legacy))
}

/// The accounts the usage call asks, in order: those that can serve the current mode (and, in Legacy, whose token the
/// Codex backend has not just refused), Codex-client sign-ins first because their tokens are known to work.
fn usage_accounts(cfg: &crate::Config, refused: &dyn Fn(&str) -> bool) -> Vec<ChatgptAccount> {
    let legacy = gpt_mode(cfg);
    let mut accounts: Vec<ChatgptAccount> = cfg.providers.iter().filter(|p| p.chatgpt).flat_map(|p| &p.chatgpt_accounts).filter(|a| a.usable() && a.runs(legacy) && !(legacy && refused(&a.key))).cloned().collect();
    accounts.sort_by_key(|a| !a.own_codex());
    accounts
}

/// The usage call with the stored tokens one at a time (each refreshed only when its turn comes) until one answers.
/// Tokens and the account id are never logged. In Official mode a refusal is not a failure: the official token may simply
/// not read it, and the answer is no numbers (the Settings page says so).
pub(crate) fn fetch_chatgpt(cfg: &crate::Config) -> Result<(String, Vec<Bar>), (String, u64)> {
    let legacy = gpt_mode(cfg);
    let mut last = ("ChatGPT sign-in expired: sign in again in Settings → ChatGPT.".to_string(), 0);
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(15))).http_status_as_error(false).build().into();
    let mut refused = false;
    for a in usage_accounts(cfg, &legacy_refused) {
        let Ok((token, id)) = account_token(&a.key) else { continue };
        let mut req = agent.get(WHAM_USAGE).header("Authorization", format!("Bearer {token}")).header("User-Agent", "codex-cli").header("Accept", "application/json");
        if !id.is_empty() {
            req = req.header("ChatGPT-Account-Id", id);
        }
        let mut res = match req.call() {
            Ok(r) => r,
            Err(e) => {
                last = (format!("Can't reach ChatGPT: {e}"), 0);
                continue;
            }
        };
        let status = res.status().as_u16();
        let retry = res.headers().get("retry-after").and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
        let body = res.body_mut().read_to_string().unwrap_or_default();
        match status {
            200 => match serde_json::from_str::<Value>(&body) {
                Ok(v) => return Ok(parse_wham(&v, now())),
                Err(e) => last = (format!("Unreadable answer: {e}"), 0),
            },
            429 => {
                let wait = retry.unwrap_or(300).max(60);
                return Err((format!("Rate limited, trying again in {} min.", wait.div_ceil(60)), wait));
            }
            401 | 403 => {
                refused = true;
                last = ("ChatGPT login expired: sign in again in ccat.".into(), 0);
            }
            s => {
                refused = true;
                last = (format!("ChatGPT answered {s}."), 0);
            }
        }
    }
    if refused && !legacy {
        return Ok((String::new(), Vec::new()));
    }
    Err(last)
}

#[cfg(test)]
impl Pool {
    /// Accounts whose access token is their name, in a scratch folder, pointed at a fake API.
    pub fn fake(base: &str, names: &[&str]) -> Pool {
        let dir = std::env::temp_dir().join(format!("ccat-gpt-{}", crate::common::random_hex(6)));
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
        p.legacy = AtomicBool::new(names.iter().any(|n| n.starts_with("cx")));
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
        let d = std::env::temp_dir().join(format!("ccat-gpt-{tag}-{}", crate::common::random_hex(4)));
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
    fn models_are_the_listed_ones_with_what_the_answer_says() {
        let api = json!([
            { "slug": "gpt-6.1-sol", "visibility": "list" },
            { "slug": "gpt-reserve", "visibility": "hide", "display_name": "Reserve" },
            { "slug": "gpt-6-sol", "visibility": "list", "display_name": "GPT-6 Sol", "default_reasoning_level": "low",
              "supported_reasoning_levels": [{ "effort": "low" }, { "effort": "max" }, { "effort": "ultra" }] }
        ]);
        let m = parse_models(api.as_array().unwrap());
        assert_eq!(m.len(), 2, "only visibility list, the server's order");
        assert_eq!((m[0].slug.as_str(), m[0].name.as_str(), m[0].efforts.len()), ("gpt-6.1-sol", "", 0), "nothing said: the slug shows, efforts unknown");
        assert_eq!((m[1].name.as_str(), m[1].default_effort.as_str(), m[1].efforts.clone()), ("GPT-6 Sol", "low", vec!["low".to_string(), "max".to_string()]), "never ultra");
        assert_eq!(clamp_effort("max", &m[1].efforts), "max");
        assert_eq!(clamp_effort("max", &m[0].efforts), "xhigh", "unknown levels: capped at xhigh");
    }

    #[test]
    fn a_revoked_sign_in_says_so_instead_of_plan_usage() {
        let d = classify(401, "token_revoked", "Encountered invalidated oauth token for user, failing request", "");
        assert_eq!((d.failure.status, d.block), (401, Some(86_400)));
        let p = Pool::fake("http://127.0.0.1:1", &["cxa"]);
        p.block("cxa", 60, &d.failure);
        let f = p.exhausted();
        assert_eq!(f.status, 401);
        assert!(f.message.contains("invalidated oauth token") && f.message.contains("Settings → ChatGPT") && !f.message.contains("plan usage"), "{}", f.message);
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

    fn query(url: &str) -> Vec<(String, String)> {
        url.split_once('?').unwrap().1.split('&').map(|kv| kv.split_once('=').unwrap()).map(|(k, v)| (k.to_string(), antigravity::dec(v))).collect()
    }

    #[test]
    fn sha256_and_pkce_known_answers() {
        assert_eq!(hex(&sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let two_blocks = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(hex(&sha256(two_blocks)), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
        // RFC 7636 appendix B.
        assert_eq!(challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        let c = challenge(&B64U.encode(random_hex(32)));
        assert!(c.len() == 43 && !c.contains(['=', '+', '/']), "S256 challenge is 43 base64url chars: {c}");
        assert_eq!(key_of("user-1").len(), 16);
    }

    #[test]
    fn authorize_url_params() {
        let verifier = "v".repeat(43);
        fn keys(q: &[(String, String)]) -> Vec<&str> {
            let mut k: Vec<&str> = q.iter().map(|(k, _)| k.as_str()).collect();
            k.sort();
            k
        }
        let get = |q: &[(String, String)], k: &str| q.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());

        let first = query(&authorize_url(4242, "urn:uuid:h", "st", "no", &verifier, None));
        assert_eq!(
            keys(&first),
            ["agent_name_hint", "client_id", "code_challenge", "code_challenge_method", "ext_agent_host_id", "nonce", "redirect_uri", "resource", "response_type", "scope", "state"],
            "a first registration: no hints"
        );
        assert_eq!(get(&first, "client_id").unwrap(), "dynamic_agent_client");
        assert_eq!(get(&first, "agent_name_hint").unwrap(), "ccat");
        assert_eq!(get(&first, "redirect_uri").unwrap(), "http://127.0.0.1:4242/auth/callback");
        assert_eq!(get(&first, "scope").unwrap(), "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct");
        assert_eq!(get(&first, "resource").unwrap(), "https://api.openai.com/v1");
        assert_eq!(get(&first, "code_challenge").unwrap(), challenge(&verifier));

        let again = query(&authorize_url(4242, "urn:uuid:h", "st", "no", &verifier, Some(("oaiapp_x", "idtok", "a@b.c"))));
        assert_eq!(
            keys(&again),
            ["client_id", "code_challenge", "code_challenge_method", "ext_agent_host_id", "id_token_hint", "login_hint", "nonce", "redirect_uri", "resource", "response_type", "scope", "state"],
            "a re-auth: the saved client id and hints, not a registration"
        );
        assert_eq!((get(&again, "client_id").unwrap(), get(&again, "id_token_hint").unwrap(), get(&again, "login_hint").unwrap()), ("oaiapp_x".into(), "idtok".into(), "a@b.c".into()));
    }

    #[test]
    fn login_starts_with_its_own_host_id_and_a_listener_first() {
        let d = scratch("login");
        let a = Login::start_in(&d, None, false).unwrap();
        let q = query(&a.url);
        let id = std::fs::read_to_string(d.join("host-id")).unwrap();
        assert!(id.starts_with("urn:uuid:") && id.len() == 9 + 36, "{id}");
        assert!(q.iter().any(|(k, v)| k == "ext_agent_host_id" && *v == id));
        let port = a.listener.local_addr().unwrap().port();
        assert!(q.iter().any(|(k, v)| k == "redirect_uri" && *v == format!("http://127.0.0.1:{port}/auth/callback")), "the listener is bound before the URL exists");
        assert_eq!(host_id(&d).unwrap(), id, "one id per install");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn id_token_claims_are_checked() {
        let good = json!({ "iss": ISSUER, "aud": ["x", "oaiapp_t"], "exp": 2_000_000_000u64, "nonce": "n1", "sub": "user-1", "email": "me@x.io" });
        assert_eq!(check_id_token(&jwt(good.clone()), "oaiapp_t", "n1", 1_900_000_000), Ok(("user-1".into(), "me@x.io".into())));
        let with = |k: &str, v: Value| {
            let mut c = good.clone();
            c[k] = v;
            check_id_token(&jwt(c), "oaiapp_t", "n1", 1_900_000_000).is_err()
        };
        assert!(with("iss", json!("https://evil.example")) && with("aud", json!("someone-else")) && with("exp", json!(1_800_000_000u64)) && with("nonce", json!("n2")) && with("sub", json!("")));
        assert!(check_id_token("not-a-jwt", "oaiapp_t", "n1", 0).is_err());
    }

    #[test]
    fn the_codex_sign_in_uses_codexs_client_and_port() {
        let q = query(&codex_authorize_url("st", &"v".repeat(43)));
        let get = |k: &str| q.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str());
        assert_eq!((get("client_id"), get("redirect_uri")), (Some("app_EMoamEEZ73f0CkXaXp7hrann"), Some("http://localhost:1455/auth/callback")));
        assert_eq!(get("scope"), Some("openid profile email offline_access api.connectors.read api.connectors.invoke"));
        assert!(codex_authorize_url("s", "v").starts_with("https://auth.openai.com/oauth/authorize?"));
    }

    #[test]
    fn token_responses_are_stored_and_rotated_whole() {
        // A sign-in response: tokens, expiry, scope, and the profile the claims carry.
        let access = jwt(json!({ AUTH_CLAIM: { "chatgpt_account_id": "acct-1", "chatgpt_plan_type": "Pro" } }));
        let mut c = Creds { client_id: "oaiapp_t".into(), ..Default::default() };
        assert!(apply(&mut c, &json!({ "access_token": access, "refresh_token": "r1", "id_token": "i1", "scope": "openid chatgpt.tokens.use.direct", "expires_in": 3600, "earliest_refresh_at": "2027-01-15T08:00:00Z" })).is_ok());
        fill_profile(&mut c);
        assert_eq!((c.refresh_token.as_str(), c.id_token.as_str(), c.scope.as_str()), ("r1", "i1", "openid chatgpt.tokens.use.direct"));
        assert_eq!((c.account_id.as_str(), c.plan.as_str(), c.earliest_refresh_at), ("acct-1", "pro", 1_800_000_000));
        assert!((now() + 3500..=now() + 3600).contains(&c.expires_at));
        assert!(!signed_out_by(Some(&c)), "plan usage granted");
        // A refresh response: the rotated refresh token replaces the old; fields it leaves out are kept; an opaque access token keeps the profile.
        assert!(apply(&mut c, &json!({ "access_token": "opaque", "refresh_token": "r2", "expires_in": 60 })).is_ok());
        fill_profile(&mut c);
        assert_eq!((c.access_token.as_str(), c.refresh_token.as_str(), c.id_token.as_str(), c.account_id.as_str()), ("opaque", "r2", "i1", "acct-1"));
        assert!(apply(&mut c, &json!({ "refresh_token": "r3" })).is_err(), "no access token: nothing is replaced");
        assert_eq!(c.refresh_token, "r2");
        // Stored and read back; a file from before `legacy_refused_at` loads with it 0.
        let d = scratch("store");
        write(&d, "k", &c).unwrap();
        assert_eq!((read(&d, "k").unwrap().refresh_token, read(&d, "k").unwrap().legacy_refused_at), ("r2".to_string(), 0));
        std::fs::write(d.join("old.json"), r#"{"client_id":"oaiapp_t","access_token":"a","refresh_token":"r","scope":"chatgpt.tokens.use.direct"}"#).unwrap();
        assert_eq!(read(&d, "old").unwrap().legacy_refused_at, 0);
        // Without the plan-usage scope an official sign-in is not used; a Codex-client one has no such scope and is.
        assert!(signed_out_by(Some(&Creds { scope: "openid".into(), ..Default::default() })) && !signed_out_by(Some(&Creds { route: CODEX.into(), ..Default::default() })));
        assert!(signed_out_by(None), "no file (a Codex CLI leftover has none)");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_mode_decides_which_accounts_run() {
        let acc = |route: &str| ChatgptAccount { route: route.into(), granted: true, ..Default::default() };
        assert_eq!([acc("").runs(false), acc(CODEX).runs(false), acc(CODEX_CLI).runs(false)], [true, false, false], "Official: only the Sign in with ChatGPT token");
        assert_eq!([acc("").runs(true), acc(CODEX).runs(true), acc(CODEX_CLI).runs(true)], [true, true, false], "Legacy tries the same token too; an import has none");

        // `fake`: "a" is a Sign in with ChatGPT account, "cx…" Codex-client ones; any "cx" starts the pool in Legacy.
        let p = Pool::fake("http://127.0.0.1:1", &["a", "cxb"]);
        assert!(p.legacy());
        p.set_legacy(false);
        assert_eq!(p.pick().map(|x| x.key), Some("a".into()));
        p.block("a", 60, &Failure { status: 429, message: "m".into(), retry_after: None });
        assert!(p.pick().is_none(), "Official never falls to the Codex-client account");
        p.set_legacy(true);
        assert_eq!(p.pick().map(|x| x.key), Some("a".into()), "switching forgets blocks learned in the other mode");

        // The Codex backend refused the token: Legacy skips that account (and says why), Official still takes it.
        refuse_legacy(&p.dir, "a");
        assert!(read(&p.dir, "a").unwrap().refused());
        assert_eq!(p.pick().map(|x| x.key), Some("cxb".into()));
        p.accounts.write().unwrap().retain(|x| x.key == "a");
        assert!(p.pick().is_none() && p.exhausted().message.contains("Codex (Legacy) needs its own sign-in"), "{}", p.exhausted().message);
        p.set_legacy(false);
        assert_eq!(p.pick().map(|x| x.key), Some("a".into()));

        // A leftover Codex CLI import is never picked, and not "signed in".
        *p.accounts.write().unwrap() = vec![ChatgptAccount { key: CODEX_CLI.into(), granted: true, route: CODEX_CLI.into(), ..Default::default() }];
        p.set_legacy(true);
        assert!(p.pick().is_none() && signed_out(&p.dir, CODEX_CLI));
    }

    #[test]
    fn a_too_long_prompt_is_the_requests_fault_not_the_accounts() {
        for status in [0, 400] {
            let d = classify(status, "context_length_exceeded", "Your input exceeds the context window of this model.", "");
            assert_eq!((d.failure.status, d.block, d.failure.retry_after), (400, None, None), "no block, so no other account is tried");
            assert!(d.failure.message.starts_with("prompt is too long: Your input exceeds"), "Claude Code compacts on this prefix: {}", d.failure.message);
        }
        // Wherever the code sits in the error.
        let v = json!({ "type": "response.failed", "response": { "error": { "code": "context_length_exceeded", "message": "too long" } } });
        let (code, message, param) = error_fields(&v);
        assert_eq!(classify(0, &code, &message, &param).failure.message, "prompt is too long: too long");
        // The Codex backend names it in `type`.
        assert_eq!(error_fields(&json!({ "error": { "type": "context_length_exceeded", "message": "m" } })).0, "context_length_exceeded");
    }

    #[test]
    fn big_bodies_are_zstd_compressed_on_the_legacy_route_only() {
        let big = json!({ "model": "m", "input": [{ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "lorem ipsum ".repeat(40_000) }] }] });
        let plain = big.to_string().into_bytes();
        assert!(plain.len() > ZSTD_OVER);
        let (z, compressed) = encode_body(&big, true);
        assert!(compressed && z.len() < plain.len() / 10, "{} -> {}", plain.len(), z.len());
        assert_eq!(zstd::decode_all(&z[..]).unwrap(), plain, "round trip");
        assert_eq!(encode_body(&big, false), (plain, false), "Official is never compressed: the docs don't say it takes zstd");
        let small = json!({ "model": "m", "input": [] });
        assert_eq!(encode_body(&small, true), (small.to_string().into_bytes(), false), "small bodies stay plain");
    }

    /// A fake backend that answers every request with `status` and `body`, counting them.
    fn fixed_server(status: u16, body: &'static str) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = hits.clone();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                // Read the whole request (head, then the body its Content-Length names) before answering.
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while let Ok(n) = s.read(&mut chunk) {
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    let Some((head, rest)) = text.split_once("\r\n\r\n") else { continue };
                    let len = head.to_lowercase().split("content-length:").nth(1).and_then(|v| v.lines().next()?.trim().parse::<usize>().ok()).unwrap_or(0);
                    if n == 0 || rest.len() >= len {
                        break;
                    }
                }
                count.fetch_add(1, Ordering::Relaxed);
                let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        (url, hits)
    }

    /// A refresh that stays offline: the access token becomes "fresh".
    fn offline_refresh(c: &Creds) -> Result<Creds, TokenErr> {
        Ok(Creds { access_token: "fresh".into(), expires_at: now() + 3600, ..c.clone() })
    }

    #[test]
    fn a_token_the_codex_backend_refuses_even_fresh_marks_the_account_for_an_hour() {
        let (url, hits) = fixed_server(401, r#"{"error":{"message":"nope"}}"#);
        let mut p = Pool::fake(&url, &["a"]);
        p.refresher = Some(offline_refresh);
        p.set_legacy(true);
        let a = p.accounts.read().unwrap()[0].clone();
        let Err(d) = p.open(&a, &json!({ "model": "m" }), "s", true) else { panic!("a 401 must not open a stream") };
        assert!(d.failure.message.contains("needs its own sign-in") && d.block == Some(LEGACY_RETRY), "{}", d.failure.message);
        assert_eq!(hits.load(Ordering::Relaxed), 2, "the first 401 gets a forced refresh and a second try");
        let c = read(&p.dir, "a").unwrap();
        assert!(c.refused() && !c.reauth, "marked for Legacy only: Official still takes it");
        assert!(p.pick().is_none(), "Legacy now skips it");
        p.set_legacy(false);
        assert_eq!(p.pick().map(|x| x.key), Some("a".into()));
        // An hour later the token is tried again.
        p.set_legacy(true);
        write(&p.dir, "a", &Creds { legacy_refused_at: now() - LEGACY_RETRY - 5, ..c }).unwrap();
        assert_eq!(p.pick().map(|x| x.key), Some("a".into()));
    }

    #[test]
    fn a_401_that_cannot_be_refreshed_now_is_not_a_mark() {
        let (url, hits) = fixed_server(401, r#"{"error":{"message":"nope"}}"#);
        let p = Pool::fake(&url, &["a"]);
        // May not be refreshed before `earliest_refresh_at`: the refusal stands as the plain 401 it is.
        let mut c = read(&p.dir, "a").unwrap();
        c.earliest_refresh_at = now() + 3600;
        write(&p.dir, "a", &c).unwrap();
        p.set_legacy(true);
        let a = p.accounts.read().unwrap()[0].clone();
        let Err(d) = p.open(&a, &json!({ "model": "m" }), "s", true) else { panic!() };
        assert_eq!((d.failure.status, d.block, hits.load(Ordering::Relaxed)), (401, Some(3600), 1), "{}", d.failure.message);
        assert!(!d.failure.message.contains("needs its own sign-in") && !read(&p.dir, "a").unwrap().refused());
        // A 403, with or without a body, is the error it says, never a mark.
        let (url, _) = fixed_server(403, r#"{"error":{"message":"blocked"}}"#);
        let mut q = Pool::fake(&url, &["a"]);
        q.refresher = Some(offline_refresh);
        let a = q.accounts.read().unwrap()[0].clone();
        let Err(d) = q.open(&a, &json!({ "model": "m" }), "s", true) else { panic!() };
        assert_eq!(d.failure.status, 403);
        assert!(!read(&q.dir, "a").unwrap().refused());
        // And Official never marks.
        let (url, _) = fixed_server(401, r#"{"error":{"message":"nope"}}"#);
        let mut r = Pool::fake(&url, &["a"]);
        r.refresher = Some(offline_refresh);
        let a = r.accounts.read().unwrap()[0].clone();
        let Err(d) = r.open(&a, &json!({ "model": "m" }), "s", false) else { panic!() };
        assert!(!d.failure.message.contains("needs its own sign-in") && !read(&r.dir, "a").unwrap().refused());
    }

    #[test]
    fn signing_in_again_lifts_the_block_of_the_old_sign_in() {
        let p = Pool::fake("http://127.0.0.1:1", &["a", "b"]);
        let denied = |status| Failure { status, message: "m".into(), retry_after: None };
        p.block("a", 86_400, &denied(403));
        p.block("b", 86_400, &denied(429));
        assert!(p.pick().is_none());
        // A refresh (same `signed_in`) changes nothing; a sign-in after the block lifts the 401/403 one only.
        let mut c = read(&p.dir, "a").unwrap();
        c.expires_at += 10;
        write(&p.dir, "a", &c).unwrap();
        assert!(p.pick().is_none());
        c.signed_in = now() + 5;
        write(&p.dir, "a", &c).unwrap();
        assert_eq!(p.pick().map(|x| x.key), Some("a".into()));
        let mut c = read(&p.dir, "b").unwrap();
        c.signed_in = now() + 5;
        write(&p.dir, "b", &c).unwrap();
        p.accounts.write().unwrap().retain(|x| x.key == "b");
        assert!(p.pick().is_none(), "a usage-limit (429) block waits for its time or for fresh usage");
    }

    #[test]
    fn usage_asks_only_accounts_that_serve_the_mode_and_codex_logins_first() {
        let acc = |key: &str, route: &str| ChatgptAccount { key: key.into(), granted: true, route: route.into(), ..Default::default() };
        let cfg = |legacy: bool| crate::Config {
            providers: vec![crate::Provider {
                chatgpt: true,
                chatgpt_legacy: Some(legacy),
                chatgpt_accounts: vec![acc("siwc", ""), acc("cx", CODEX), acc("cli", CODEX_CLI), ChatgptAccount { disabled: true, ..acc("off", CODEX) }, acc("refused", "")],
                ..Default::default()
            }],
            ..Default::default()
        };
        let keys = |c: &crate::Config, refused: &str| usage_accounts(c, &|k| k == refused).into_iter().map(|a| a.key).collect::<Vec<_>>();
        assert_eq!(keys(&cfg(true), "refused"), ["cx", "siwc"], "Legacy: Codex sign-ins first; no import, nothing off, nothing the backend just refused");
        assert_eq!(keys(&cfg(false), "refused"), ["siwc", "refused"], "Official: only the Sign in with ChatGPT tokens; a Legacy refusal doesn't matter");
        assert!(gpt_has_login(&cfg(false)) && gpt_has_login(&cfg(true)));
        let mut only_codex = cfg(false);
        only_codex.providers[0].chatgpt_accounts.retain(|a| a.own_codex());
        assert!(!gpt_has_login(&only_codex), "a Codex-client sign-in can't serve Official");
    }

    /// Live, by hand: `cargo test live_both_modes -- --ignored --nocapture`. Reads the real config and credential folder,
    /// refreshing a token there if it is due (as ccat itself does), and sends one tiny request in each mode.
    /// `CCAT_GPT_MODEL` picks the model (else the first one of the config's list).
    #[test]
    #[ignore = "uses your ChatGPT sign-in"]
    fn live_both_modes() {
        let cfg: crate::Config = serde_json::from_str(&std::fs::read_to_string(crate::user_config_path()).expect("ccat config")).unwrap();
        let g = cfg.providers.iter().find(|p| p.chatgpt).expect("ChatGPT provider");
        let model = std::env::var("CCAT_GPT_MODEL").ok().or_else(|| g.models.first().cloned()).expect("a model: fetch the list in Settings, or set CCAT_GPT_MODEL");
        let mut pool = Pool::new(g.chatgpt_accounts.clone(), g.chatgpt_models.clone());
        pool.dir = crate::user_config_path().with_file_name("chatgpt");
        let req = json!({ "model": "claude-opus-5-5", "max_tokens": 50, "stream": true, "messages": [{ "role": "user", "content": "Reply with the single word pong." }] });
        let hints = proxy::Hints { session: format!("ccat-live-{}", random_hex(4)) };
        for legacy in [false, true] {
            let mode = if legacy { "Legacy" } else { "Official" };
            pool.set_legacy(legacy);
            let a = pool.pick().unwrap_or_else(|| panic!("no account usable in {mode} mode: {}", pool.exhausted().message));
            let body = proxy::responses_body(&req, &model, None, &hints, &pool, legacy);
            let reader = match pool.open(&a, &body, &hints.session, legacy) {
                Ok(r) => r,
                Err(d) => panic!("{mode} mode: {} {}", d.failure.status, d.failure.message),
            };
            let events = reader.lines().map_while(Result::ok).filter_map(|l| serde_json::from_str::<Value>(l.strip_prefix("data:")?.trim()).ok());
            let text: String = events.filter(|v| v["type"] == "response.output_text.delta").filter_map(|v| v["delta"].as_str().map(String::from)).collect();
            println!("{mode}: {text}");
            assert!(text.to_lowercase().contains("pong"), "{text}");
        }
    }
}
