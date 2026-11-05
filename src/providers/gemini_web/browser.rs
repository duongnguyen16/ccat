//! Native browser automation for Gemini Web authentication via Chrome/Edge CDP.
//!
//! Handles:
//! - Finding installed Chrome / Edge binaries on Windows.
//! - Spawning an isolated browser process with an exclusive user profile.
//! - Connecting to Chrome DevTools Protocol (CDP) via WebSocket over localhost.
//! - Filtering Google authentication cookies without storing prompt or user data.
//! - Safe cancellation and RAII process cleanup.

use super::store::{self, Account};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CdpCookie {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub path: String,
}

// ---------------------------------------------------------------- Browser Discovery & Arguments

/// Searches standard installation locations on Windows for Google Chrome or Microsoft Edge.
pub fn find_browser_executable() -> Result<PathBuf, String> {
    if let Ok(env_path) = std::env::var("CCAT_BROWSER_PATH") {
        let p = PathBuf::from(env_path);
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Ok(env_path) = std::env::var("CHROME_PATH") {
        let p = PathBuf::from(env_path);
        if p.is_file() {
            return Ok(p);
        }
    }

    #[cfg(windows)]
    {
        let mut candidates = Vec::new();
        if let Ok(prog_files) = std::env::var("ProgramFiles") {
            candidates.push(PathBuf::from(&prog_files).join(r"Google\Chrome\Application\chrome.exe"));
            candidates.push(PathBuf::from(&prog_files).join(r"Microsoft\Edge\Application\msedge.exe"));
        }
        if let Ok(prog_files_x86) = std::env::var("ProgramFiles(x86)") {
            candidates.push(PathBuf::from(&prog_files_x86).join(r"Google\Chrome\Application\chrome.exe"));
            candidates.push(PathBuf::from(&prog_files_x86).join(r"Microsoft\Edge\Application\msedge.exe"));
        }
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            candidates.push(PathBuf::from(&local_app_data).join(r"Google\Chrome\Application\chrome.exe"));
            candidates.push(PathBuf::from(&local_app_data).join(r"Microsoft\Edge\Application\msedge.exe"));
        }

        for path in candidates {
            if path.is_file() {
                return Ok(path);
            }
        }
    }

    Err("Neither Google Chrome nor Microsoft Edge could be found. Please ensure one is installed.".into())
}

/// Builds command line arguments for the isolated browser instance.
pub fn build_browser_args(
    profile_dir: &Path,
    port: u16,
    headless: bool,
    target_url: &str,
) -> Vec<String> {
    let mut args = vec![
        format!("--remote-debugging-port={port}"),
        format!("--user-data-dir={}", profile_dir.display()),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-blink-features=AutomationControlled".into(),
    ];
    if headless {
        args.push("--headless=new".into());
        args.push("--disable-gpu".into());
    }
    args.push(target_url.into());
    args
}

/// Parses the DevToolsActivePort file written by Chromium.
/// Format: Line 1 = port, Line 2 = target browser path.
pub fn parse_devtools_active_port(content: &str) -> Result<(u16, String), String> {
    let mut lines = content.lines();
    let port_str = lines.next().ok_or("Empty DevToolsActivePort content")?.trim();
    let port = port_str.parse::<u16>().map_err(|e| format!("Invalid port in DevToolsActivePort: {e}"))?;
    let path = lines.next().unwrap_or("").trim().to_string();
    Ok((port, path))
}

/// Validates that a debugger URL points strictly to loopback (127.0.0.1, localhost, [::1]) with no userinfo or fragment.
pub fn validate_debugger_url(raw_url: &str) -> Result<String, String> {
    let trimmed = raw_url.trim();
    if !trimmed.starts_with("ws://") && !trimmed.starts_with("http://") {
        return Err("Debugger URL must have ws:// or http:// scheme".into());
    }
    if trimmed.contains('#') || trimmed.contains('@') {
        return Err("Debugger URL cannot contain userinfo or fragment".into());
    }

    let after_scheme = if let Some(rest) = trimmed.strip_prefix("ws://") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        rest
    } else {
        return Err("Invalid debugger scheme".into());
    };

    let host_port = after_scheme.split('/').next().unwrap_or(after_scheme);
    let (host, port_str) = if host_port.starts_with('[') {
        let mut parts = host_port.split(']');
        let h = parts.next().unwrap_or("").trim_start_matches('[');
        let p = parts.next().and_then(|s| s.strip_prefix(':')).unwrap_or("");
        (h, p)
    } else {
        let mut parts = host_port.split(':');
        let h = parts.next().unwrap_or("");
        let p = parts.next().unwrap_or("");
        (h, p)
    };

    if host != "127.0.0.1" && host != "localhost" && host != "::1" {
        return Err(format!("Debugger host '{host}' is not a local loopback address"));
    }

    if !port_str.is_empty() {
        let port: u16 = port_str.parse().map_err(|e| format!("Invalid port in debugger URL: {e}"))?;
        if port == 0 {
            return Err("Invalid port 0 in debugger URL".into());
        }
    }

    Ok(trimmed.to_string())
}

// ---------------------------------------------------------------- Cookie Filtering

pub const REQUIRED_COOKIES: &[&str] = &["__Secure-1PSID", "__Secure-1PSIDTS"];
pub const USEFUL_COOKIES: &[&str] = &[
    "__Secure-1PSID",
    "__Secure-1PSIDTS",
    "__Secure-1PSIDCC",
    "__Secure-3PSID",
    "__Secure-3PSIDTS",
    "__Secure-3PSIDCC",
    "SID",
    "HSID",
    "SSID",
    "APISID",
    "SAPISID",
    "__Secure-1PAPISID",
    "__Secure-3PAPISID",
    "NID",
    "1P_JAR",
    "AEC",
    "SOCS",
];

/// Filters captured CDP cookies to Google authentication cookies.
/// Requires exact google.com or direct subdomain of google.com (no .contains or evil suffixes).
/// Cookie values must not contain control characters or semicolons.
pub fn filter_google_cookies(cookies: &[CdpCookie]) -> Option<String> {
    let mut map = std::collections::BTreeMap::new();
    let useful: HashSet<&str> = USEFUL_COOKIES.iter().copied().collect();

    for c in cookies {
        let domain = c.domain.trim().trim_start_matches('.').to_ascii_lowercase();
        let is_google = domain == "google.com" || domain.ends_with(".google.com");
        if !is_google {
            continue;
        }
        if c.value.chars().any(|ch| ch.is_control() || ch == ';') {
            continue;
        }
        if useful.contains(c.name.as_str()) && !map.contains_key(&c.name) {
            map.insert(c.name.clone(), c.value.clone());
        }
    }

    for req in REQUIRED_COOKIES {
        if !map.contains_key(*req) {
            return None;
        }
    }

    let pairs: Vec<String> = map.into_iter().map(|(k, v)| format!("{k}={v}")).collect();
    Some(pairs.join("; "))
}

/// Parses the cookies array from CDP `Storage.getCookies` or `Network.getCookies` JSON response.
pub fn parse_cdp_cookies_response(val: &serde_json::Value) -> Result<Vec<CdpCookie>, String> {
    let cookies_val = if let Some(arr) = val.get("result").and_then(|r| r.get("cookies")) {
        arr
    } else if let Some(arr) = val.get("cookies") {
        arr
    } else {
        return Err("CDP response does not contain 'cookies' array".into());
    };

    serde_json::from_value(cookies_val.clone())
        .map_err(|e| format!("Failed to parse CDP cookies: {e}"))
}

// ---------------------------------------------------------------- RAII Child Process Guard

/// Owns a spawned browser child process. Reaps and kills the child on drop.
pub struct BrowserProcess {
    pub child: std::process::Child,
}

impl BrowserProcess {
    pub fn spawn(exe: &Path, args: &[String]) -> Result<Self, String> {
        let mut cmd = std::process::Command::new(exe);
        cmd.args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }

        let child = cmd.spawn().map_err(|e| format!("Failed to spawn browser {}: {e}", exe.display()))?;
        Ok(Self { child })
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for BrowserProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

// ---------------------------------------------------------------- CDP WebSocket Communication

/// Resolves the WebSocket debugger URL from the DevTools HTTP endpoint `http://127.0.0.1:{port}/json/version`.
pub fn get_debugger_ws_url(port: u16, timeout: Duration) -> Result<String, String> {
    let ep = format!("http://127.0.0.1:{port}/json/version");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .build()
        .into();

    let mut res = agent.get(&ep).call().map_err(|e| format!("Failed to query {ep}: {e}"))?;
    let body = res.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;

    let ws_url = v.get("webSocketDebuggerUrl")
        .and_then(|u| u.as_str())
        .ok_or("webSocketDebuggerUrl missing from DevTools version JSON")?;

    validate_debugger_url(ws_url)
}

/// Executes a synchronous CDP call over a local TCP WebSocket connection.
/// Normalizes localhost to 127.0.0.1, bounds message size, and correlates response ID ignoring events.
pub fn query_cdp_cookies(ws_url: &str, timeout: Duration) -> Result<Vec<CdpCookie>, String> {
    let validated = validate_debugger_url(ws_url)?;
    let trimmed = validated.trim();
    let after_ws = trimmed.strip_prefix("ws://").ok_or("Debugger URL must begin with ws://")?;
    let host_port = after_ws.split('/').next().ok_or("Invalid ws URL path")?;

    let (host, port_str) = if host_port.starts_with('[') {
        let mut parts = host_port.split(']');
        let h = parts.next().unwrap_or("").trim_start_matches('[');
        let p = parts.next().and_then(|s| s.strip_prefix(':')).unwrap_or("");
        (h, p)
    } else {
        let mut parts = host_port.split(':');
        let h = parts.next().unwrap_or("");
        let p = parts.next().unwrap_or("");
        (h, p)
    };

    let port: u16 = port_str.parse().map_err(|e| format!("Invalid port in host_port {host_port}: {e}"))?;
    let socket_addr: std::net::SocketAddr = if host == "localhost" || host == "127.0.0.1" {
        std::net::SocketAddr::from(([127, 0, 0, 1], port))
    } else if host == "::1" {
        std::net::SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port))
    } else {
        return Err(format!("Unsupported local host: {host}"));
    };

    let stream = std::net::TcpStream::connect_timeout(&socket_addr, timeout)
        .map_err(|e| format!("Failed to connect to CDP socket at {socket_addr}: {e}"))?;

    stream.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(timeout)).map_err(|e| e.to_string())?;

    let mut config = tungstenite::protocol::WebSocketConfig::default();
    config.max_message_size = Some(4 * 1024 * 1024);
    config.max_frame_size = Some(2 * 1024 * 1024);

    let (mut socket, _resp) = tungstenite::client::client_with_config(trimmed, stream, Some(config))
        .map_err(|e| format!("WebSocket handshake failed: {e}"))?;

    let req_id: i64 = 1;
    let req = serde_json::json!({
        "id": req_id,
        "method": "Storage.getCookies",
        "params": {}
    });

    socket.send(tungstenite::Message::text(req.to_string()))
        .map_err(|e| format!("Failed to send CDP message: {e}"))?;

    let start = Instant::now();
    loop {
        if start.elapsed() >= timeout {
            return Err("Timed out waiting for CDP response".into());
        }

        let msg = socket.read().map_err(|e| format!("Failed to read CDP response: {e}"))?;
        let text = match msg {
            tungstenite::Message::Text(t) => t.to_string(),
            tungstenite::Message::Binary(b) => String::from_utf8(b.to_vec()).map_err(|e| e.to_string())?,
            tungstenite::Message::Ping(_) | tungstenite::Message::Pong(_) => continue,
            other => return Err(format!("Unexpected CDP message frame: {other:?}")),
        };

        let val: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| format!("Invalid JSON from CDP: {e}"))?;

        if val.get("id").and_then(|v| v.as_i64()) == Some(req_id) {
            return parse_cdp_cookies_response(&val);
        }
        // Asynchronous CDP events (e.g. Target.targetCreated) are ignored until our matching response arrives
    }
}

// ---------------------------------------------------------------- Profile Resolution

/// Resolves the isolated browser profile directory relative to the provider root.
/// Preserves legacy stored relative paths (`profiles/<id>`, `.gemini_profiles/<id>`),
/// resolves bare directory names under `profiles/`, and validates against directory breakout.
pub fn resolve_profile_dir(
    provider_root: &Path,
    stored_profile: &str,
    account_id: &str,
) -> Result<PathBuf, String> {
    let p = stored_profile.trim();
    let relative = if p.is_empty() {
        PathBuf::from("profiles").join(account_id)
    } else {
        let validated = store::validate_profile_path(p)?;
        if p.starts_with("profiles/") || p.starts_with("profiles\\")
            || p.starts_with(".gemini_profiles/") || p.starts_with(".gemini_profiles\\")
        {
            validated
        } else {
            PathBuf::from("profiles").join(validated)
        }
    };

    let target = provider_root.join(&relative);
    if target.is_dir() {
        if let Ok(canon_root) = provider_root.canonicalize() {
            if let Ok(canon_target) = target.canonicalize() {
                if !canon_target.starts_with(&canon_root) {
                    return Err(format!("Profile path {} escapes provider root", target.display()));
                }
            }
        }
    }

    Ok(target)
}

// ---------------------------------------------------------------- Native Login Entrypoint

/// Runs the native browser login / cookie sync flow using real authenticated verification.
pub fn login(
    account: &Account,
    headless: bool,
    cancel: &AtomicBool,
) -> Result<(), String> {
    login_with_validator(
        account,
        headless,
        cancel,
        &crate::providers::gemini_web::dir(),
        |cookie| {
            crate::providers::gemini_web::wire::initialize(cookie, "")
                .map(|_| ())
                .map_err(|e| e.message)
        },
    )
}

/// Parameterized login supporting injected/mock validator for tests.
pub fn login_with_validator<F>(
    account: &Account,
    headless: bool,
    cancel: &AtomicBool,
    provider_root: &Path,
    validator: F,
) -> Result<(), String>
where
    F: Fn(&str) -> Result<(), String> + Send + Sync + 'static,
{
    if cancel.load(Ordering::Relaxed) {
        return Err("Login canceled by user".into());
    }

    store::validate_id(&account.id)?;
    store::validate_name(&account.name)?;

    let profile_dir = resolve_profile_dir(provider_root, &account.profile, &account.id)?;
    std::fs::create_dir_all(&profile_dir)
        .map_err(|e| format!("Failed to create profile dir {}: {e}", profile_dir.display()))?;

    // Profile singleton lock to prevent concurrent browser access to same profile
    let lock_file = profile_dir.join(".ccat_singleton.lock");
    let _profile_lock = store::StoreLock::acquire(&lock_file, Duration::from_millis(500))
        .map_err(|_| format!("Profile {} is currently open in another process", profile_dir.display()))?;

    // Remove stale DevToolsActivePort before launching
    let active_port_file = profile_dir.join("DevToolsActivePort");
    if active_port_file.is_file() {
        let _ = std::fs::remove_file(&active_port_file);
    }

    let exe = find_browser_executable()?;
    // Pass port 0 so Chrome exclusively creates fresh DevToolsActivePort
    let args = build_browser_args(&profile_dir, 0, headless, "https://gemini.google.com/app");
    let mut browser = BrowserProcess::spawn(&exe, &args)?;

    let total_timeout = if headless {
        Duration::from_secs(25)
    } else {
        Duration::from_secs(300)
    };
    let start = Instant::now();

    // 1. Wait for fresh DevToolsActivePort written by our spawned child
    let mut ws_url = None;
    while start.elapsed() < Duration::from_secs(12) {
        if cancel.load(Ordering::Relaxed) {
            return Err("Login canceled by user".into());
        }
        if let Ok(Some(status)) = browser.child.try_wait() {
            return Err(format!("Browser exited prematurely (code {:?}) - profile may be in use", status.code()));
        }
        if active_port_file.is_file() {
            if let Ok(content) = std::fs::read_to_string(&active_port_file) {
                if let Ok((port, path)) = parse_devtools_active_port(&content) {
                    let raw = format!("ws://127.0.0.1:{port}{path}");
                    if let Ok(v) = validate_debugger_url(&raw) {
                        ws_url = Some(v);
                        break;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    let ws_url = ws_url.ok_or_else(|| {
        format!("Timed out waiting for DevToolsActivePort in {}", profile_dir.display())
    })?;

    // 2. Poll for Google cookies and perform real verification
    let mut last_failed_candidate: Option<String> = None;
    let validator_arc = std::sync::Arc::new(validator);

    while start.elapsed() < total_timeout {
        if cancel.load(Ordering::Relaxed) {
            return Err("Login canceled by user".into());
        }
        if let Ok(Some(status)) = browser.child.try_wait() {
            return Err(format!("Browser process exited (code: {:?})", status.code()));
        }

        if let Ok(cookies) = query_cdp_cookies(&ws_url, Duration::from_secs(2)) {
            if let Some(candidate) = filter_google_cookies(&cookies) {
                if last_failed_candidate.as_deref() != Some(&candidate) {
                    // Background verification so cancel remains immediately responsive
                    let (tx, rx) = std::sync::mpsc::channel();
                    let cand_clone = candidate.clone();
                    let val_clone = validator_arc.clone();

                    std::thread::spawn(move || {
                        let res = val_clone(&cand_clone);
                        let _ = tx.send(res);
                    });

                    let val_start = Instant::now();
                    let mut verified = false;
                    while val_start.elapsed() < Duration::from_secs(60) {
                        if cancel.load(Ordering::Relaxed) {
                            return Err("Login canceled by user".into());
                        }
                        if let Ok(Some(status)) = browser.child.try_wait() {
                            return Err(format!("Browser exited during validation (code: {:?})", status.code()));
                        }
                        if let Ok(res) = rx.recv_timeout(Duration::from_millis(200)) {
                            match res {
                                Ok(()) => {
                                    verified = true;
                                    break;
                                }
                                Err(_) => {
                                    last_failed_candidate = Some(candidate.clone());
                                    break;
                                }
                            }
                        }
                    }

                    if !verified && last_failed_candidate.as_deref() != Some(&candidate) {
                        // Verifier timed out: memoize candidate to prevent duplicate verification threads
                        last_failed_candidate = Some(candidate.clone());
                    }

                    if verified {
                        let rel_profile = profile_dir
                            .strip_prefix(provider_root)
                            .map(|p| p.display().to_string().replace('\\', "/"))
                            .unwrap_or_else(|_| account.id.clone());

                        // Atomic commit under ONE StoreLock: verifies account still exists, saves credentials and profile
                        store::commit_login_in(provider_root, &account.id, &candidate, &rel_profile)?;
                        browser.kill();
                        return Ok(());
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(500));
    }

    Err(if headless {
        "Headless session refresh expired: Google sign-in required".into()
    } else {
        "Login timed out waiting for Google sign-in (5 minutes). Please try again.".into()
    })
}

// ---------------------------------------------------------------- Unit Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_devtools_active_port() {
        let content = "51234\n/devtools/browser/98b7e2\n";
        let (port, path) = parse_devtools_active_port(content).expect("parse port");
        assert_eq!(port, 51234);
        assert_eq!(path, "/devtools/browser/98b7e2");

        assert!(parse_devtools_active_port("").is_err());
        assert!(parse_devtools_active_port("invalid_port\n/path").is_err());
    }

    #[test]
    fn test_validate_debugger_url() {
        assert!(validate_debugger_url("ws://127.0.0.1:51234/devtools/page/1").is_ok());
        assert!(validate_debugger_url("ws://localhost:51234/devtools/page/1").is_ok());
        assert!(validate_debugger_url("http://127.0.0.1:51234/json/version").is_ok());
        assert!(validate_debugger_url("ws://192.168.1.100:51234/devtools/page/1").is_err());
        assert!(validate_debugger_url("ws://evil.com/devtools/page/1").is_err());
        assert!(validate_debugger_url("ftp://127.0.0.1:51234").is_err());
        assert!(validate_debugger_url("ws://admin@127.0.0.1:51234/path").is_err());
        assert!(validate_debugger_url("ws://127.0.0.1:51234/path#frag").is_err());
    }

    #[test]
    fn test_build_browser_args() {
        let profile = Path::new("profiles/acc1");
        let args = build_browser_args(profile, 9222, false, "https://gemini.google.com/app");
        assert!(args.contains(&"--remote-debugging-port=9222".to_string()));
        assert!(args.contains(&"--no-first-run".to_string()));
        assert!(args.contains(&"--no-default-browser-check".to_string()));
        assert!(args.contains(&"--disable-blink-features=AutomationControlled".to_string()));
        assert!(args.contains(&"https://gemini.google.com/app".to_string()));
        assert!(!args.contains(&"--headless=new".to_string()));

        let headless_args = build_browser_args(profile, 9222, true, "https://gemini.google.com/app");
        assert!(headless_args.contains(&"--headless=new".to_string()));
        assert!(headless_args.contains(&"--disable-gpu".to_string()));
    }

    #[test]
    fn test_filter_google_cookies() {
        // Missing required cookies -> returns None
        let partial = vec![
            CdpCookie {
                name: "__Secure-1PSID".into(),
                value: "psid_val".into(),
                domain: ".google.com".into(),
                path: "/".into(),
            },
        ];
        assert_eq!(filter_google_cookies(&partial), None);

        // Required and useful cookies present
        let cookies = vec![
            CdpCookie {
                name: "__Secure-1PSID".into(),
                value: "psid_val".into(),
                domain: ".google.com".into(),
                path: "/".into(),
            },
            CdpCookie {
                name: "__Secure-1PSIDTS".into(),
                value: "ts_val".into(),
                domain: ".google.com".into(),
                path: "/".into(),
            },
            CdpCookie {
                name: "SID".into(),
                value: "sid_val".into(),
                domain: ".google.com".into(),
                path: "/".into(),
            },
            CdpCookie {
                name: "unrelated_tracker".into(),
                value: "ignore_me".into(),
                domain: "other.com".into(),
                path: "/".into(),
            },
            CdpCookie {
                name: "google_unrelated".into(),
                value: "skip_me".into(),
                domain: ".google.com".into(),
                path: "/".into(),
            },
        ];

        let mut spoofed = cookies[..3].to_vec();
        for cookie in &mut spoofed { cookie.domain = "google.com.evil.test".into(); }
        assert!(filter_google_cookies(&spoofed).is_none());
        let header = filter_google_cookies(&cookies).expect("should succeed");
        assert!(header.contains("__Secure-1PSID=psid_val"));
        assert!(header.contains("__Secure-1PSIDTS=ts_val"));
        assert!(header.contains("SID=sid_val"));
        assert!(!header.contains("unrelated_tracker"));
        assert!(!header.contains("google_unrelated"));
    }

    #[test]
    fn test_parse_cdp_cookies_response() {
        let json_val = serde_json::json!({
            "id": 1,
            "result": {
                "cookies": [
                    {
                        "name": "__Secure-1PSID",
                        "value": "secret123",
                        "domain": ".google.com",
                        "path": "/"
                    }
                ]
            }
        });

        let parsed = parse_cdp_cookies_response(&json_val).expect("parse cdp");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "__Secure-1PSID");
        assert_eq!(parsed[0].value, "secret123");
    }

    #[test]
    fn test_cancellation_before_launch() {
        let cancel = AtomicBool::new(true);
        let acc = Account {
            id: "test-cancel".into(),
            name: "Cancel Account".into(),
            port: 4982,
            enabled: true,
            profile: "".into(),
            request_count: 0,
            error_count: 0,
            extra: Default::default(),
        };

        let res = login(&acc, true, &cancel);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err(), "Login canceled by user");
    }

    #[test]
    fn test_synthetic_cdp_loopback() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("port").port();

        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept connection");
            let mut ws = tungstenite::accept(stream).expect("accept websocket");
            let msg = ws.read().expect("read request");
            if let tungstenite::Message::Text(t) = msg {
                assert!(t.contains("Storage.getCookies"));
            }
            let resp = serde_json::json!({
                "id": 1,
                "result": {
                    "cookies": [
                        {
                            "name": "__Secure-1PSID",
                            "value": "synthetic_psid_123",
                            "domain": ".google.com",
                            "path": "/"
                        },
                        {
                            "name": "__Secure-1PSIDTS",
                            "value": "synthetic_ts_456",
                            "domain": ".google.com",
                            "path": "/"
                        }
                    ]
                }
            });
            ws.send(tungstenite::Message::text(serde_json::json!({"method":"Target.targetCreated","params":{}}).to_string())).unwrap();
            ws.send(tungstenite::Message::text(resp.to_string())).expect("send response");
        });

        let ws_url = format!("ws://localhost:{port}/devtools/browser/synthetic_target");
        let cookies = query_cdp_cookies(&ws_url, Duration::from_secs(3)).expect("query synthetic cdp");
        assert_eq!(cookies.len(), 2);
        let header = filter_google_cookies(&cookies).expect("filter synthetic cookies");
        assert!(header.contains("__Secure-1PSID=synthetic_psid_123"));
        assert!(header.contains("__Secure-1PSIDTS=synthetic_ts_456"));

        handle.join().expect("join server thread");
    }

    #[test]
    fn test_resolve_profile_dir() {
        let root = Path::new("D:\\ccat\\gemini_web");
        // Empty profile -> profiles/<id>
        assert_eq!(
            resolve_profile_dir(root, "", "acc1").unwrap(),
            root.join("profiles").join("acc1")
        );
        // Stored "profiles/acc1"
        assert_eq!(
            resolve_profile_dir(root, "profiles/acc1", "acc1").unwrap(),
            root.join("profiles").join("acc1")
        );
        // Legacy ".gemini_profiles/key123"
        assert_eq!(
            resolve_profile_dir(root, ".gemini_profiles/key123", "acc1").unwrap(),
            root.join(".gemini_profiles").join("key123")
        );
        // Bare name "custom_dir"
        assert_eq!(
            resolve_profile_dir(root, "custom_dir", "acc1").unwrap(),
            root.join("profiles").join("custom_dir")
        );
        // Traversal attempt
        assert!(resolve_profile_dir(root, "../escape", "acc1").is_err());
    }
}
