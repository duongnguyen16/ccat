//! OpenCode's configured outbound paths. Admission/quota errors never select another IP.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Cursor, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    #[default]
    Direct,
    BindIp,
    HttpProxy,
    HttpsProxy,
    Socks5,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct NodeConfig {
    pub id: String,
    pub name: String,
    pub kind: Kind,
    pub address: String,
    pub username_env: String,
    pub password_env: String,
    pub weight: u32,
    pub enabled: bool,
    pub draining: bool,
}
impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            id: "direct".into(),
            name: "Direct".into(),
            kind: Kind::Direct,
            address: String::new(),
            username_env: String::new(),
            password_env: String::new(),
            weight: 100,
            enabled: true,
            draining: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct Config {
    pub nodes: Vec<NodeConfig>,
    pub connect_timeout_seconds: u64,
    pub read_timeout_seconds: u64,
    pub health_interval_seconds: u64,
    pub failure_threshold: u32,
    pub recovery_seconds: u64,
    pub max_connect_failovers: usize,
    pub max_active_streams: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            nodes: vec![NodeConfig::default()],
            connect_timeout_seconds: 10,
            read_timeout_seconds: 120,
            health_interval_seconds: 60,
            failure_threshold: 3,
            recovery_seconds: 60,
            max_connect_failovers: 1,
            max_active_streams: 32,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub(crate) struct Snapshot {
    pub id: String,
    pub address: String,
    pub kind: Kind,
    pub enabled: bool,
    pub draining: bool,
    pub weight: u32,
    pub active_streams: usize,
    pub total_requests: u64,
    pub total_errors: u64,
    pub last_status: Option<u16>,
}

pub(crate) fn validate(config: &Config) -> Result<(), String> {
    if config.connect_timeout_seconds == 0 {
        return Err("connect_timeout_seconds must be greater than 0.".into());
    }
    if config.read_timeout_seconds == 0 {
        return Err("read_timeout_seconds must be greater than 0.".into());
    }
    if config.max_active_streams == 0 {
        return Err("max_active_streams must be greater than 0.".into());
    }
    let mut seen = HashSet::new();
    for node in &config.nodes {
        if node.id.trim().is_empty() {
            return Err("Node ID cannot be empty.".into());
        }
        if !seen.insert(&node.id) {
            return Err(format!("Duplicate egress node ID \"{}\".", node.id));
        }
        if node.enabled && !node.draining && node.weight == 0 {
            return Err(format!("Node \"{}\": enabled node cannot have zero weight.", node.id));
        }
        match node.kind {
            Kind::BindIp => {
                return Err(format!("Node \"{}\": BindIp is not supported by Bun fetch (HTTP and HTTPS proxies only).", node.id));
            }
            Kind::Socks5 => {
                return Err(format!("Node \"{}\": Socks5 is not supported by Bun fetch (HTTP and HTTPS proxies only).", node.id));
            }
            Kind::HttpProxy | Kind::HttpsProxy => {
                if node.address.contains('@') {
                    return Err(format!("Node \"{}\": use credential environment variables, not embedded credentials.", node.id));
                }
                if node.address.trim().is_empty() {
                    return Err(format!("Node \"{}\": proxy address is required.", node.id));
                }
                let (scheme, rest) = node.address.split_once("://")
                    .ok_or_else(|| format!("Node \"{}\": proxy address \"{}\" missing scheme (e.g. http://).", node.id, node.address))?;
                let scheme_lower = scheme.to_ascii_lowercase();
                match node.kind {
                    Kind::HttpProxy if scheme_lower != "http" => {
                        return Err(format!("Node \"{}\": HTTP proxy address must use http:// scheme.", node.id));
                    }
                    Kind::HttpsProxy if scheme_lower != "https" && scheme_lower != "http" => {
                        return Err(format!("Node \"{}\": HTTPS proxy address must use https:// or http:// scheme.", node.id));
                    }
                    _ => {}
                }
                let host = rest.split('/').next().unwrap_or("").trim();
                if host.is_empty() {
                    return Err(format!("Node \"{}\": proxy address must include a valid host.", node.id));
                }
            }
            Kind::Direct => {}
        }
    }
    Ok(())
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn select_node<'a>(config: &'a Config, scope: &str, affinity: &str) -> Result<&'a NodeConfig, crate::common::Failure> {
    let candidates: Vec<&NodeConfig> = config.nodes.iter()
        .filter(|n| n.enabled && !n.draining && n.weight > 0)
        .collect();
    if candidates.is_empty() {
        return Err(crate::common::fail(503, "No available egress nodes"));
    }
    if candidates.len() == 1 {
        return Ok(candidates[0]);
    }
    let mut best_node = candidates[0];
    let mut best_score = f64::NEG_INFINITY;
    for node in candidates {
        let key = format!("{scope}:{affinity}:{}", node.id);
        let h = fnv1a(key.as_bytes());
        let u = ((h >> 11) + 1) as f64 / (1u64 << 53) as f64;
        let score = - (node.weight as f64) / u.ln();
        if score > best_score {
            best_score = score;
            best_node = node;
        }
    }
    Ok(best_node)
}

type CacheKey = (String, String, String, u64); // scope, node.id, node.address, read_timeout_seconds

struct Worker {
    child: Child,
    stdin: ChildStdin,
    stdout_lines: std::io::Lines<std::io::BufReader<ChildStdout>>,
}

static IDLE_WORKERS: Mutex<Option<HashMap<CacheKey, Vec<Worker>>>> = Mutex::new(None);
static ACTIVE_COUNT: AtomicUsize = AtomicUsize::new(0);
static SNAPSHOTS: Mutex<Option<HashMap<String, Snapshot>>> = Mutex::new(None);

fn find_runtime() -> (std::path::PathBuf, bool) {
    #[cfg(windows)]
    if let Ok(appdata) = std::env::var("APPDATA") {
        let base = std::path::PathBuf::from(appdata).join("npm").join("node_modules");
        let candidates = [
            base.join("@opencode").join("cli").join("bin").join("opencode.exe"),
            base.join("@opencode").join("cli").join("node_modules").join("@opencode/cli-windows-x64").join("bin").join("opencode.exe"),
            base.join("opencode-ai").join("bin").join("opencode.exe"),
            base.join("opencode-ai").join("node_modules").join("opencode-windows-x64").join("bin").join("opencode.exe"),
            base.join("opencode-windows-x64").join("bin").join("opencode.exe"),
        ];
        for c in candidates {
            if c.is_file() {
                return (c, true);
            }
        }
    }
    #[cfg(not(windows))]
    if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
        let candidates = [
            home.join(".opencode/bin/opencode"),
            home.join(".bun/bin/bun"),
            home.join(".local/bin/opencode"),
            home.join(".local/bin/bun"),
        ];
        for c in candidates {
            if c.is_file() {
                let is_op = c.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("opencode"));
                return (c, is_op);
            }
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            #[cfg(windows)]
            let candidates = [("opencode.exe", true), ("bun.exe", false)];
            #[cfg(not(windows))]
            let candidates = [("opencode", true), ("bun", false)];
            for (name, is_op) in candidates {
                let exe = dir.join(name);
                if exe.is_file() {
                    return (exe, is_op);
                }
            }
        }
    }
    (std::path::PathBuf::from("bun"), false)
}

fn spawn_worker() -> Result<Worker, String> {
    const FETCH_JS: &str = include_str!("fetch.js");
    let (runtime, is_opencode) = find_runtime();
    let mut cmd = Command::new(&runtime);
    if is_opencode {
        cmd.env("BUN_BE_BUN", "1");
    }
    cmd.arg("--eval").arg(FETCH_JS)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for var in &["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy", "NO_PROXY", "no_proxy"] {
        cmd.env_remove(var);
    }
    let mut child = cmd.spawn().or_else(|_| {
        let mut fallback = Command::new("bun");
        fallback.arg("--eval").arg(FETCH_JS)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for var in &["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy", "NO_PROXY", "no_proxy"] {
            fallback.env_remove(var);
        }
        fallback.spawn()
    }).map_err(|e| format!("Failed to spawn worker with runtime {}: {e}", runtime.display()))?;

    let stdin = child.stdin.take().ok_or_else(|| "Failed to capture stdin".to_string())?;
    let stdout = child.stdout.take().ok_or_else(|| "Failed to capture stdout".to_string())?;
    let stdout_lines = std::io::BufReader::new(stdout).lines();
    Ok(Worker { child, stdin, stdout_lines })
}

fn acquire_worker(key: &CacheKey) -> Result<Worker, String> {
    {
        let mut pool = IDLE_WORKERS.lock().unwrap_or_else(|e| e.into_inner());
        let map = pool.get_or_insert_with(HashMap::new);
        if let Some(list) = map.get_mut(key) {
            while let Some(mut w) = list.pop() {
                if w.child.try_wait().ok().flatten().is_none() {
                    return Ok(w);
                }
            }
        }
    }
    spawn_worker()
}

fn release_worker(key: CacheKey, worker: Worker) {
    let mut pool = IDLE_WORKERS.lock().unwrap_or_else(|e| e.into_inner());
    let map = pool.get_or_insert_with(HashMap::new);
    map.entry(key).or_default().push(worker);
}

fn record_start(node: &NodeConfig) {
    let mut lock = SNAPSHOTS.lock().unwrap_or_else(|e| e.into_inner());
    let map = lock.get_or_insert_with(HashMap::new);
    let s = map.entry(node.id.clone()).or_insert_with(|| Snapshot {
        id: node.id.clone(),
        address: node.address.clone(),
        kind: node.kind,
        enabled: node.enabled,
        draining: node.draining,
        weight: node.weight,
        active_streams: 0,
        total_requests: 0,
        total_errors: 0,
        last_status: None,
    });
    s.active_streams += 1;
    s.total_requests += 1;
}

fn record_status(node_id: &str, status: u16) {
    let mut lock = SNAPSHOTS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = lock.as_mut() {
        if let Some(s) = map.get_mut(node_id) {
            s.last_status = Some(status);
            if status >= 400 {
                s.total_errors += 1;
            }
        }
    }
}

fn record_end(node_id: &str) {
    let mut lock = SNAPSHOTS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = lock.as_mut() {
        if let Some(s) = map.get_mut(node_id) {
            s.active_streams = s.active_streams.saturating_sub(1);
        }
    }
}

pub(crate) fn snapshots(_scope: &str) -> Vec<Snapshot> {
    let lock = SNAPSHOTS.lock().unwrap_or_else(|e| e.into_inner());
    lock.as_ref().map(|m| m.values().cloned().collect()).unwrap_or_default()
}

struct EgressStream {
    worker: Option<Worker>,
    cache_key: CacheKey,
    node_id: String,
    current_chunk: Cursor<Vec<u8>>,
    clean_end: bool,
}

impl BufRead for EgressStream {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        let pos = self.current_chunk.position() as usize;
        if pos < self.current_chunk.get_ref().len() {
            return Ok(&self.current_chunk.get_ref()[pos..]);
        }
        if self.clean_end {
            return Ok(&[]);
        }
        let worker = match self.worker.as_mut() {
            Some(w) => w,
            None => return Ok(&[]),
        };
        match worker.stdout_lines.next() {
            Some(Ok(line)) => {
                let v: serde_json::Value = serde_json::from_str(&line)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                if v.get("end").and_then(|e| e.as_bool()).unwrap_or(false) {
                    self.clean_end = true;
                    if let Some(w) = self.worker.take() {
                        release_worker(self.cache_key.clone(), w);
                        ACTIVE_COUNT.fetch_sub(1, Ordering::SeqCst);
                        record_end(&self.node_id);
                    }
                    self.current_chunk = Cursor::new(Vec::new());
                    Ok(&[])
                } else if let Some(b64) = v.get("data").and_then(|d| d.as_str()) {
                    let bytes = B64.decode(b64.trim())
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                    self.current_chunk = Cursor::new(bytes);
                    Ok(self.current_chunk.get_ref().as_slice())
                } else if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
                    Err(std::io::Error::new(std::io::ErrorKind::Other, err.to_string()))
                } else {
                    Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Unknown frame"))
                }
            }
            Some(Err(e)) => Err(e),
            None => Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Worker closed stdout unexpectedly")),
        }
    }

    fn consume(&mut self, amt: usize) {
        let pos = self.current_chunk.position() + amt as u64;
        self.current_chunk.set_position(pos);
    }
}

impl Read for EgressStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let available = self.fill_buf()?;
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl Drop for EgressStream {
    fn drop(&mut self) {
        if let Some(mut w) = self.worker.take() {
            let _ = w.child.kill();
            let _ = w.child.wait();
            ACTIVE_COUNT.fetch_sub(1, Ordering::SeqCst);
            record_end(&self.node_id);
            record_status(&self.node_id, 0);
        }
    }
}

pub(crate) fn open(
    url: &str,
    payload: &serde_json::Value,
    headers: Vec<(&str, String)>,
    config: &Config,
    scope: &str,
    affinity: &str,
) -> Result<Box<dyn BufRead + Send>, crate::common::Failure> {
    validate(config).map_err(|e| crate::common::fail(400, e))?;
    let node = select_node(config, scope, affinity)?;

    let active = ACTIVE_COUNT.fetch_add(1, Ordering::SeqCst);
    if active >= config.max_active_streams {
        ACTIVE_COUNT.fetch_sub(1, Ordering::SeqCst);
        return Err(crate::common::Failure {
            status: 429,
            message: "Egress pool max active streams reached".into(),
            retry_after: Some(1),
        });
    }

    record_start(node);

    let trimmed = url.trim().trim_end_matches('/');
    let full_url = if trimmed.ends_with("/chat/completions") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/chat/completions")
    };

    let proxy_str = match node.kind {
        Kind::HttpProxy | Kind::HttpsProxy => {
            let mut p = node.address.clone();
            let user = if !node.username_env.is_empty() {
                std::env::var(&node.username_env).unwrap_or_default()
            } else {
                String::new()
            };
            let pass = if !node.password_env.is_empty() {
                std::env::var(&node.password_env).unwrap_or_default()
            } else {
                String::new()
            };
            if !user.is_empty() {
                if let Some((scheme, rest)) = p.split_once("://") {
                    if !pass.is_empty() {
                        p = format!("{scheme}://{user}:{pass}@{rest}");
                    } else {
                        p = format!("{scheme}://{user}@{rest}");
                    }
                }
            }
            Some(p)
        }
        _ => None,
    };

    let mut req_headers: Vec<(String, String)> = Vec::new();
    req_headers.push(("Content-Type".into(), "application/json".into()));
    req_headers.push(("Authorization".into(), "Bearer public".into()));
    for (k, v) in headers {
        let lower = k.to_ascii_lowercase();
        if lower != "content-type" && lower != "authorization" && lower != "api-key" && lower != "proxy-authorization" {
            req_headers.push((k.to_string(), v));
        }
    }

    let cache_key: CacheKey = (scope.to_string(), node.id.clone(), node.address.clone(), config.read_timeout_seconds);

    let mut worker = match acquire_worker(&cache_key) {
        Ok(w) => w,
        Err(e) => {
            ACTIVE_COUNT.fetch_sub(1, Ordering::SeqCst);
            record_end(&node.id);
            record_status(&node.id, 502);
            return Err(crate::common::fail(502, e));
        }
    };

    let req_obj = serde_json::json!({
        "url": full_url,
        "payload": payload,
        "headers": req_headers,
        "proxy": proxy_str,
        "timeout_seconds": config.read_timeout_seconds,
    });

    let mut req_line = match serde_json::to_string(&req_obj) {
        Ok(l) => l,
        Err(e) => {
            ACTIVE_COUNT.fetch_sub(1, Ordering::SeqCst);
            record_end(&node.id);
            return Err(crate::common::fail(400, e.to_string()));
        }
    };
    req_line.push('\n');

    if let Err(e) = worker.stdin.write_all(req_line.as_bytes()).and_then(|_| worker.stdin.flush()) {
        let _ = worker.child.kill();
        let _ = worker.child.wait();
        ACTIVE_COUNT.fetch_sub(1, Ordering::SeqCst);
        record_end(&node.id);
        record_status(&node.id, 502);
        return Err(crate::common::fail(502, format!("Failed to write to worker: {e}")));
    }

    let first_line = match worker.stdout_lines.next() {
        Some(Ok(l)) => l,
        Some(Err(e)) => {
            let _ = worker.child.kill();
            let _ = worker.child.wait();
            ACTIVE_COUNT.fetch_sub(1, Ordering::SeqCst);
            record_end(&node.id);
            record_status(&node.id, 502);
            return Err(crate::common::fail(502, format!("Failed to read status: {e}")));
        }
        None => {
            let _ = worker.child.kill();
            let _ = worker.child.wait();
            ACTIVE_COUNT.fetch_sub(1, Ordering::SeqCst);
            record_end(&node.id);
            record_status(&node.id, 502);
            return Err(crate::common::fail(502, "Worker exited unexpectedly"));
        }
    };

    #[derive(Deserialize)]
    struct StatusFrame {
        status: u16,
        retry_after: Option<u64>,
    }
    let frame: StatusFrame = match serde_json::from_str(&first_line) {
        Ok(f) => f,
        Err(e) => {
            let _ = worker.child.kill();
            let _ = worker.child.wait();
            ACTIVE_COUNT.fetch_sub(1, Ordering::SeqCst);
            record_end(&node.id);
            record_status(&node.id, 502);
            return Err(crate::common::fail(502, format!("Invalid status frame: {e}")));
        }
    };

    record_status(&node.id, frame.status);

    if frame.status >= 400 {
        let _ = worker.child.kill();
        let _ = worker.child.wait();
        ACTIVE_COUNT.fetch_sub(1, Ordering::SeqCst);
        record_end(&node.id);
        return Err(crate::common::Failure {
            status: frame.status,
            message: format!("HTTP {}", frame.status),
            retry_after: frame.retry_after,
        });
    }

    Ok(Box::new(EgressStream {
        worker: Some(worker),
        cache_key,
        node_id: node.id.clone(),
        current_chunk: Cursor::new(Vec::new()),
        clean_end: false,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_config_rejects_ambiguous_routes_and_embedded_secrets() {
        assert!(validate(&Config::default()).is_ok());
        let mut cfg = Config::default();
        cfg.nodes.push(NodeConfig::default());
        assert!(validate(&cfg).is_err(), "duplicate IDs cannot define session affinity");
        cfg.nodes.pop();
        cfg.nodes[0].kind = Kind::HttpProxy;
        cfg.nodes[0].address = "http://user:secret@127.0.0.1:3128".into();
        let error = validate(&cfg).unwrap_err();
        assert!(!error.contains("secret"), "credentials must not enter errors");
        cfg.nodes[0].kind = Kind::BindIp;
        cfg.nodes[0].address = "127.0.0.1".into();
        let bind_err = validate(&cfg).unwrap_err();
        assert!(bind_err.contains("BindIp is not supported"), "BindIp returns explicit error");
        cfg.nodes[0].kind = Kind::Socks5;
        cfg.nodes[0].address = "socks5://127.0.0.1:1080".into();
        let socks_err = validate(&cfg).unwrap_err();
        assert!(socks_err.contains("Socks5 is not supported"), "Socks5 returns explicit error");
        cfg.nodes[0].kind = Kind::Direct;
        cfg.read_timeout_seconds = 0;
        assert!(validate(&cfg).is_err(), "zero timeout is rejected");
        cfg.read_timeout_seconds = 60;
        cfg.nodes[0].weight = 0;
        assert!(validate(&cfg).is_err(), "zero weight on enabled node is rejected");
    }

    #[test]
    fn default_config_has_one_direct_node() {
        let cfg = Config::default();
        assert_eq!(cfg.nodes.len(), 1);
        assert_eq!(cfg.nodes[0].kind, Kind::Direct);
        assert!(cfg.nodes[0].enabled && !cfg.nodes[0].draining);
    }

    #[test]
    fn config_survives_serde_roundtrip_and_missing_fields() {
        let cfg = Config::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
        let empty: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.nodes.len(), 1, "missing egress falls back to default Direct");
        let partial: Config = serde_json::from_str(r#"{"nodes":[{"id":"socks","kind":"socks5","address":"socks5h://proxy:1080","weight":50}]}"#).unwrap();
        assert_eq!(partial.nodes[0].kind, Kind::Socks5);
        assert!(validate(&partial).is_err(), "Socks5 rejected explicitly by Bun egress");
    }

    #[test]
    fn deterministic_weighted_sticky_rendezvous_excludes_disabled_and_draining() {
        let mut cfg = Config::default();
        cfg.nodes = vec![
            NodeConfig { id: "n1".into(), weight: 100, enabled: true, draining: false, ..Default::default() },
            NodeConfig { id: "n2".into(), weight: 100, enabled: false, draining: false, ..Default::default() },
            NodeConfig { id: "n3".into(), weight: 100, enabled: true, draining: true, ..Default::default() },
        ];
        let n = select_node(&cfg, "scopeA", "conv1").unwrap();
        assert_eq!(n.id, "n1", "n2 is disabled and n3 is draining");

        cfg.nodes[1].enabled = true; // both n1 and n2 enabled
        let pick1 = select_node(&cfg, "scopeA", "conv1").unwrap().id.clone();
        let pick2 = select_node(&cfg, "scopeA", "conv1").unwrap().id.clone();
        assert_eq!(pick1, pick2, "rendezvous is deterministic and sticky");
    }

    #[test]
    fn test_incremental_streaming_before_upstream_finish() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        let chunk2_sent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let chunk2_sent_server = std::sync::Arc::clone(&chunk2_sent);

        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/event-stream\r\n\r\n").unwrap();
            socket.flush().unwrap();
            let chunk1 = b"data: chunk 1\n\n";
            write!(socket, "{:x}\r\n", chunk1.len()).unwrap();
            socket.write_all(chunk1).unwrap();
            socket.write_all(b"\r\n").unwrap();
            socket.flush().unwrap();
            let _ = rx.recv_timeout(std::time::Duration::from_millis(600));
            chunk2_sent_server.store(true, std::sync::atomic::Ordering::SeqCst);
            // Send chunk 2
            let chunk2 = b"data: chunk 2\n\n";
            write!(socket, "{:x}\r\n", chunk2.len()).unwrap();
            socket.write_all(chunk2).unwrap();
            socket.write_all(b"\r\n").unwrap();
            socket.write_all(b"0\r\n\r\n").unwrap();
            socket.flush().unwrap();
        });

        let cfg = Config {
            nodes: vec![NodeConfig {
                id: "direct".into(),
                kind: Kind::Direct,
                address: String::new(),
                weight: 100,
                enabled: true,
                draining: false,
                ..Default::default()
            }],
            ..Default::default()
        };

        let url = format!("http://127.0.0.1:{port}/v1");
        let mut reader = open(&url, &serde_json::json!({}), vec![], &cfg, "s1", "a1").unwrap();
        let mut line1 = String::new();
        reader.read_line(&mut line1).unwrap();
        assert!(line1.contains("chunk 1"));
        assert!(!chunk2_sent.load(std::sync::atomic::Ordering::SeqCst), "chunk 1 received before chunk 2 was sent");
        let _ = tx.send(());

        let mut line2 = String::new();
        while line2.trim().is_empty() {
            line2.clear();
            if reader.read_line(&mut line2).unwrap() == 0 {
                break;
            }
        }
        assert!(line2.contains("chunk 2"));

        handle.join().unwrap();
    }

    #[test]
    fn test_actual_http_proxy_route() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let n = socket.read(&mut buf).unwrap();
            let req_str = String::from_utf8_lossy(&buf[..n]);
            assert!(req_str.contains("Bearer public"), "Bearer public marker must be present");
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 17\r\n\r\n{\"proxied\": true}").unwrap();
            socket.flush().unwrap();
        });

        let cfg = Config {
            nodes: vec![NodeConfig {
                id: "proxy1".into(),
                kind: Kind::HttpProxy,
                address: format!("http://127.0.0.1:{port}"),
                weight: 100,
                enabled: true,
                draining: false,
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut reader = open("http://upstream.example.com/v1", &serde_json::json!({"test": 1}), vec![("Authorization", "secret-token".into())], &cfg, "s_proxy", "a_proxy").unwrap();
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        assert_eq!(body, "{\"proxied\": true}");
        handle.join().unwrap();
    }

    #[test]
    fn test_active_stream_release_and_cancellation() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf);
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/event-stream\r\n\r\n");
                let _ = socket.write_all(b"7\r\nstream\n\r\n");
                let _ = socket.flush();
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        });

        let cfg = Config {
            nodes: vec![NodeConfig {
                id: "node_cancel_test".into(),
                kind: Kind::Direct,
                address: String::new(),
                weight: 100,
                enabled: true,
                draining: false,
                ..Default::default()
            }],
            ..Default::default()
        };

        let url = format!("http://127.0.0.1:{port}/v1");
        let reader = open(&url, &serde_json::json!({}), vec![], &cfg, "s_cancel", "a_cancel").unwrap();
        let during = snapshots("s_cancel").into_iter().find(|s| s.id == "node_cancel_test").map(|s| s.active_streams).unwrap_or(0);
        assert_eq!(during, 1, "node active stream counted");
        drop(reader);
        let after = snapshots("s_cancel").into_iter().find(|s| s.id == "node_cancel_test").map(|s| s.active_streams).unwrap_or(0);
        assert_eq!(after, 0, "stream drop releases active count");
        let _ = handle.join();
    }

    #[test]
    fn test_no_rotation_on_429_or_403() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            socket.write_all(b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 42\r\nContent-Type: application/json\r\n\r\n{\"error\":\"rate limit exceeded\"}").unwrap();
            socket.flush().unwrap();
        });

        let cfg = Config {
            nodes: vec![
                NodeConfig {
                    id: "primary".into(),
                    kind: Kind::Direct,
                    address: String::new(),
                    weight: 100,
                    enabled: true,
                    draining: false,
                    ..Default::default()
                },
                NodeConfig {
                    id: "backup".into(),
                    kind: Kind::Direct,
                    address: String::new(),
                    weight: 100,
                    enabled: true,
                    draining: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let url = format!("http://127.0.0.1:{port}/v1");
        let res = open(&url, &serde_json::json!({}), vec![], &cfg, "s429", "a429");
        assert!(res.is_err());
        let failure = res.err().unwrap();
        assert_eq!(failure.status, 429);
        assert_eq!(failure.retry_after, Some(42));
        assert_eq!(failure.message, "HTTP 429", "no raw upstream body in failure message");
        handle.join().unwrap();
    }

    #[test]
    fn test_environment_proxy_isolation() {
        if std::env::var_os("CCAT_EGRESS_ENV_TEST_CHILD").is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "providers::opencode::egress::tests::test_environment_proxy_isolation", "--nocapture"])
                .env("CCAT_EGRESS_ENV_TEST_CHILD", "1")
                .env("HTTP_PROXY", "http://127.0.0.1:1").env("http_proxy", "http://127.0.0.1:1")
                .output().unwrap();
            assert!(output.status.success(), "isolated test failed: {} {}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            return;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf);
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 6\r\n\r\n\"pure\"").unwrap();
            socket.flush().unwrap();
        });

        let cfg = Config {
            nodes: vec![NodeConfig {
                id: "direct_env_iso".into(),
                kind: Kind::Direct,
                address: String::new(),
                weight: 100,
                enabled: true,
                draining: false,
                ..Default::default()
            }],
            ..Default::default()
        };

        // The subprocess inherited a dead proxy; the parallel test harness did not.
        let url = format!("http://127.0.0.1:{port}/v1");
        let mut reader = open(&url, &serde_json::json!({}), vec![], &cfg, "s_iso", "a_iso").unwrap();
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        assert_eq!(body, "\"pure\"");
        handle.join().unwrap();
    }

    #[test]
    fn test_runtime_discovery() {
        let (runtime, is_opencode) = find_runtime();
        assert!(runtime.is_file() || runtime == std::path::Path::new("bun"));
        if is_opencode {
            assert!(runtime.to_string_lossy().contains("opencode"));
        }
    }
}
