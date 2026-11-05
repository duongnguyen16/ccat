//! `ccat --serve [addr]`: the other half of cloud settings. Another machine's Settings window edits this machine's
//! config through it: it lists the files the window shows (config, profiles, usage, request log, live sessions),
//! hands them out, takes the config and profiles back, and lists the terminals (tmux sessions) running here.
//! Every request carries the machine's token; the sign-in files (`chatgpt/…`) are never served.

use crate::remote::fnv;
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::Command,
    time::Duration,
};

pub const FLAG: &str = "--serve";
const DEFAULT_ADDR: &str = "127.0.0.1:8788";
/// The biggest file the other side may put (a config is a few KB).
const MAX_PUT: usize = 4 << 20;

fn dir() -> PathBuf {
    crate::config_path().parent().map(PathBuf::from).unwrap_or_default()
}

/// The token this machine's Settings clients must send: `CCAT_AGENT_TOKEN`, else the one kept next to the config
/// (made on first use).
pub fn token() -> String {
    if let Some(t) = std::env::var("CCAT_AGENT_TOKEN").ok().filter(|t| !t.trim().is_empty()) {
        return t.trim().to_string();
    }
    let path = dir().join("agent-token");
    if let Some(t) = fs::read_to_string(&path).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
        return t;
    }
    let mut b = [0u8; 24];
    getrandom::fill(&mut b).expect("OS randomness");
    let t: String = b.iter().map(|x| format!("{x:02x}")).collect();
    let _ = fs::create_dir_all(dir());
    let _ = fs::write(&path, &t);
    let _ = crate::common::secure_file(&path);
    t
}

/// Where a synced file (`config.json`, `profiles/<name>.json`, …) lives here, and whether the other side may write it;
/// `None` for anything else, so a path can never leave the few places the window shows.
pub(crate) fn resolve(rel: &str) -> Option<(PathBuf, bool)> {
    crate::remote::rel_kind(rel).map(|writable| (dir().join(rel), writable))
}

fn manifest() -> Value {
    let mut rels: Vec<String> = ["config.json", "usage.json", "pricing.json", "tokens.jsonl"].map(String::from).to_vec();
    for sub in ["profiles", "live"] {
        let names = fs::read_dir(dir().join(sub)).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned());
        rels.extend(names.map(|n| format!("{sub}/{n}")));
    }
    let files: Vec<Value> = rels
        .iter()
        .filter_map(|rel| {
            let (path, writable) = resolve(rel)?;
            let meta = fs::metadata(&path).ok().filter(|m| m.is_file())?;
            let ms = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64);
            let hash = writable.then(|| fs::read(&path).map(|b| fnv(&b)).unwrap_or_default());
            Some(json!({ "path": rel, "size": meta.len(), "mtime": ms, "writable": writable, "hash": hash }))
        })
        .collect();
    json!({ "version": &crate::VERSION[1..], "now": crate::usage::now(), "files": files })
}

/// The tmux sessions of this user: name, whether one is attached, the command running in the window now, and where.
fn sessions() -> Value {
    let out = Command::new("tmux").args(["list-panes", "-a", "-F", "#{session_name}\t#{session_attached}\t#{pane_current_command}\t#{pane_current_path}\t#{session_created}"]).output();
    let mut list: Vec<Value> = Vec::new();
    for line in out.iter().filter(|o| o.status.success()).flat_map(|o| String::from_utf8_lossy(&o.stdout).into_owned().lines().map(String::from).collect::<Vec<_>>()) {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() == 5 && !list.iter().any(|s| s["name"] == f[0]) {
            list.push(json!({ "name": f[0], "attached": f[1] != "0", "command": f[2], "path": f[3], "created": f[4].parse::<u64>().unwrap_or(0) }));
        }
    }
    json!({ "tmux": Command::new("tmux").arg("-V").output().is_ok(), "sessions": list })
}

// ---------------------------------------------------------------- power
// The Azure VM sleeps by itself (`mx-idle`, see apkb): it deallocates the machine 15 minutes after the `keepalive`
// file was last touched, unless a build runs or `stay_awake` exists, and at 23:00 Vietnam time whatever is set.
// The Terminals page drives the same files.

fn power_paths() -> (PathBuf, PathBuf, PathBuf) {
    let env = |k: &str, d: &str| PathBuf::from(std::env::var(k).unwrap_or_else(|_| d.into()));
    (env("APKB_KEEPALIVE", "/var/lib/mx-idle/keepalive"), env("APKB_HOME", "/opt/apkb").join("stay_awake"), env("APKB_LOCK", "/opt/maxinfra-hq/locks/build.lock"))
}

fn mtime_s(p: &std::path::Path) -> Option<u64> {
    fs::metadata(p).ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs())
}

/// The next 23:00 in Vietnam (UTC+7), when `mx-idle` stops the machine whatever else is set.
fn hard_stop(now: u64) -> u64 {
    let at = now / 86_400 * 86_400 + 16 * 3600;
    if at <= now { at + 86_400 } else { at }
}

fn power() -> Value {
    let (keep, stay, lock) = power_paths();
    let now = crate::usage::now();
    let building = mtime_s(&lock).is_some_and(|t| now.saturating_sub(t) < 7200);
    let stay_on = stay.exists();
    let up = fs::read_to_string("/proc/uptime").ok().and_then(|t| t.split_whitespace().next()?.parse::<f64>().ok());
    let base = mtime_s(&keep).map(|k| k + 900).max(up.map(|u| now.saturating_sub(u as u64) + 900));
    json!({
        "supported": keep.parent().is_some_and(|d| d.exists()), "now": now, "building": building, "stay_awake": stay_on,
        "sleep_at": if building || stay_on { None } else { base }, "hard_stop": hard_stop(now),
    })
}

/// Sets the file's modified time (made if it is not there).
fn touch_at(p: &std::path::Path, t: u64) -> std::io::Result<()> {
    let _ = fs::create_dir_all(p.parent().unwrap_or(p));
    fs::OpenOptions::new().create(true).write(true).truncate(false).open(p)?.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(t))
}

/// `{"action":"timer","minutes":30}`: sleep that long from now; `{"action":"stay","on":true}`: do not sleep (apkb keeps
/// touching the file); `{"action":"sleep_now"}`: stop the machine now.
fn set_power(v: &Value) -> Result<Value, String> {
    let (keep, stay, _) = power_paths();
    let now = crate::usage::now();
    match v["action"].as_str().unwrap_or("") {
        "timer" => {
            let minutes = v["minutes"].as_u64().filter(|m| (1..=1440).contains(m)).ok_or("minutes: 1 to 1440")?;
            let _ = fs::remove_file(&stay);
            // mx-idle sleeps 15 minutes after the file's time.
            touch_at(&keep, (now + minutes * 60).saturating_sub(900)).map_err(|e| e.to_string())?;
        }
        "stay" if v["on"].as_bool() == Some(true) => {
            touch_at(&stay, now).and_then(|_| touch_at(&keep, now)).map_err(|e| e.to_string())?;
        }
        "stay" => {
            let _ = fs::remove_file(&stay);
        }
        "sleep_now" => deallocate()?,
        a => return Err(format!("unknown action {a}")),
    }
    Ok(power())
}

/// What `mx-idle` does: the machine's managed identity asks Azure to deallocate it (answered at once).
fn deallocate() -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).build().into();
    let json = |url: &str| -> Result<Value, String> {
        let mut r = agent.get(url).header("Metadata", "true").call().map_err(|e| e.to_string())?;
        serde_json::from_str(&r.body_mut().read_to_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())
    };
    let vm = json("http://169.254.169.254/metadata/instance/compute?api-version=2021-02-01")?;
    let tok = json("http://169.254.169.254/metadata/identity/oauth2/token?api-version=2018-02-01&resource=https://management.azure.com/")?;
    let url = format!(
        "https://management.azure.com/subscriptions/{}/resourceGroups/{}/providers/Microsoft.Compute/virtualMachines/{}/deallocate?api-version=2024-07-01",
        vm["subscriptionId"].as_str().unwrap_or(""), vm["resourceGroupName"].as_str().unwrap_or(""), vm["name"].as_str().unwrap_or("")
    );
    agent.post(&url).header("Authorization", format!("Bearer {}", tok["access_token"].as_str().unwrap_or(""))).send(&b""[..]).map(|_| ()).map_err(|e| format!("Azure refused: {e}"))
}

struct Req {
    method: String,
    path: String,
    query: String,
    authorized: bool,
    body: Vec<u8>,
}

fn param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| kv.strip_prefix(key)?.strip_prefix('=')).map(|v| v.replace("%2F", "/").replace("%2f", "/").replace("%20", " ").replace('+', " "))
}

fn read_req(s: &TcpStream, token: &str) -> std::io::Result<Req> {
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut it = line.split_whitespace();
    let (method, target) = (it.next().unwrap_or("").to_string(), it.next().unwrap_or("/").to_string());
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let (mut len, mut authorized) = (0usize, false);
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 || line.trim_end().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => len = v.trim().parse().unwrap_or(0).min(MAX_PUT),
                "authorization" => authorized = v.trim().strip_prefix("Bearer ").is_some_and(|t| same(t.trim(), token)),
                _ => {}
            }
        }
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    Ok(Req { method, path: path.to_string(), query: query.to_string(), authorized, body })
}

/// Compared without stopping at the first difference.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn answer(r: &Req) -> (u16, &'static str, Vec<u8>) {
    let json = |v: Value| (200, "application/json", v.to_string().into_bytes());
    let err = |code, m: &str| (code, "application/json", json!({ "error": m }).to_string().into_bytes());
    let file = param(&r.query, "path").and_then(|p| resolve(&p));
    match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/v1/manifest") => json(manifest()),
        ("GET", "/v1/sessions") => json(sessions()),
        ("GET", "/v1/power") => json(power()),
        ("POST", "/v1/power") => match serde_json::from_slice::<Value>(&r.body).map_err(|e| e.to_string()).and_then(|v| set_power(&v)) {
            Ok(v) => json(v),
            Err(e) => err(400, &e),
        },
        ("GET", "/v1/file") => {
            let Some((path, _)) = file else { return err(404, "no such file") };
            let from = param(&r.query, "from").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            match fs::File::open(&path).and_then(|mut f| {
                use std::io::{Seek, SeekFrom};
                f.seek(SeekFrom::Start(from))?;
                let mut b = Vec::new();
                f.read_to_end(&mut b).map(|_| b)
            }) {
                Ok(b) => (200, "application/octet-stream", b),
                Err(_) => err(404, "no such file"),
            }
        }
        ("PUT", "/v1/file") => {
            let Some((path, true)) = file else { return err(403, "this file can't be written") };
            if serde_json::from_slice::<Value>(&r.body).map_or(true, |v| !v.is_object()) {
                return err(400, "not a JSON object");
            }
            let _ = fs::create_dir_all(path.parent().unwrap_or(&path));
            let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
            match fs::write(&tmp, &r.body).and_then(|_| fs::rename(&tmp, &path)) {
                Ok(_) => json(json!({ "hash": fnv(&r.body) })),
                Err(e) => err(500, &e.to_string()),
            }
        }
        ("DELETE", "/v1/file") => match file {
            Some((path, true)) if r.query.contains("path=profiles") && fs::remove_file(&path).is_ok() => json(json!({})),
            _ => err(403, "only a profile can be deleted"),
        },
        _ => err(404, "not found"),
    }
}

/// `GET /v1/ssh` (a WebSocket): a pipe to this machine's sshd, so a terminal can be opened over the same HTTPS the
/// agent is reached by, where a network lets nothing else through. Needs the token like everything else.
fn ssh_tunnel(s: TcpStream, token: &str) {
    use tungstenite::{handshake::server::{ErrorResponse, Request, Response}, Message};
    let check = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        let ok = req.headers().get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).is_some_and(|t| same(t.trim(), token));
        if ok { Ok(resp) } else { Err(tungstenite::http::Response::builder().status(401).body(None).unwrap()) }
    };
    let _ = s.set_read_timeout(None);
    let Ok(mut ws) = tungstenite::accept_hdr(s, check) else { return };
    let Ok(mut ssh) = TcpStream::connect(std::env::var("CCAT_SSHD").unwrap_or_else(|_| "127.0.0.1:22".into())) else { return };
    let _ = ws.get_mut().set_read_timeout(Some(Duration::from_millis(10)));
    let _ = ssh.set_read_timeout(Some(Duration::from_millis(10)));
    let mut buf = [0u8; 16 << 10];
    let idle = |e: &std::io::Error| matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut);
    loop {
        match ws.read() {
            Ok(Message::Binary(b)) => {
                if ssh.write_all(&b).is_err() {
                    break;
                }
            }
            Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => break,
            Err(tungstenite::Error::Io(e)) if idle(&e) => {}
            Ok(_) => {}
            Err(_) => break,
        }
        match ssh.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if ws.send(Message::Binary(buf[..n].to_vec().into())).is_err() {
                    break;
                }
            }
            Err(e) if idle(&e) => {}
            Err(_) => break,
        }
    }
    let _ = ws.close(None);
}

pub(crate) fn serve(mut s: TcpStream, token: &str) -> std::io::Result<()> {
    let mut head = [0u8; 16];
    if s.peek(&mut head).is_ok_and(|n| head[..n].starts_with(b"GET /v1/ssh")) {
        ssh_tunnel(s, token);
        return Ok(());
    }
    s.set_read_timeout(Some(Duration::from_secs(15)))?;
    let (code, kind, body) = match read_req(&s, token) {
        Ok(r) if r.authorized => answer(&r),
        Ok(_) => (401, "application/json", br#"{"error":"wrong token"}"#.to_vec()),
        Err(_) => return Ok(()),
    };
    write!(s, "HTTP/1.1 {code} X\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", body.len())?;
    s.write_all(&body)
}

pub fn run(addr: Option<String>) {
    let addr = addr.unwrap_or_else(|| DEFAULT_ADDR.into());
    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        eprintln!("ccat: cannot listen on {addr}: {e}");
        std::process::exit(1);
    });
    let token = token();
    eprintln!("ccat agent on {addr}; token: {}", dir().join("agent-token").display());
    crate::usage::spawn_refresher();
    crate::gateway::spawn();
    for stream in listener.incoming().flatten() {
        let token = token.clone();
        std::thread::spawn(move || {
            let _ = serve(stream, &token);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_files_the_window_shows_resolve() {
        for ok in ["config.json", "usage.json", "tokens.jsonl", "profiles/Default.json", "profiles/My config.json", "live/123.json"] {
            assert!(resolve(ok).is_some(), "{ok}");
        }
        for bad in ["chatgpt/x.json", "../config.json", "profiles/../config.json", "profiles/a/b.json", "profiles/.hidden.json", "profiles/x.txt", "agent-token", "live/1.running", "/etc/passwd", ""] {
            assert!(resolve(bad).is_none(), "{bad}");
        }
        assert!(resolve("profiles/a.json").unwrap().1 && resolve("config.json").unwrap().1);
        assert!(!resolve("usage.json").unwrap().1 && !resolve("live/1.json").unwrap().1 && !resolve("tokens.jsonl").unwrap().1);
    }

    #[test]
    fn the_hard_stop_is_the_next_2300_in_vietnam() {
        let noon_utc = 1_793_275_200; // 2026-10-29 12:00 UTC = 19:00 in Vietnam
        assert_eq!(hard_stop(noon_utc), noon_utc + 4 * 3600);
        assert_eq!(hard_stop(noon_utc + 4 * 3600), noon_utc + 28 * 3600, "at 23:00 it is tomorrow's");
    }

    #[test]
    fn tokens_are_compared_whole() {
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
    }
}
