//! Cloud settings, the Settings window's side. A machine here is a ccat running `ccat --serve` (see `agent`). The
//! Settings window started for one (`CCAT_MACHINE=<name>`) works on a local mirror of that machine's config, profiles,
//! usage, request log and live sessions: `config_path` points at the mirror, so every page works unchanged, and a
//! background thread keeps the mirror and the machine in step (what the machine writes comes down, what the window
//! saves goes up). Terminals of the machine open as `ssh` windows here.

use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, OnceLock},
    time::Duration,
};

pub const MACHINE_ENV: &str = "CCAT_MACHINE";
pub const TUNNEL_FLAG: &str = "--ccat-tunnel";
pub const FORWARD_FLAG: &str = "--ccat-forward";
/// How often the mirror and the machine are compared.
const TICK: Duration = Duration::from_secs(2);
/// The most of the request log's end that is brought down on the first look.
const TOKENS_TAIL: u64 = 16 << 20;

#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
#[serde(default)]
pub struct Machine {
    pub name: String,
    /// The agent: `host:port` (a Tailscale address, say).
    pub addr: String,
    pub token: String,
    /// What `ssh` is given to open a terminal there: `user@host` or a `~/.ssh/config` alias.
    pub ssh: String,
    /// An Azure VM (`resource-group/name`), so Settings can turn it on with the Azure CLI.
    pub vm: String,
}

fn machines_path() -> PathBuf {
    crate::user_config_path().with_file_name("machines.json")
}

pub fn machines() -> Vec<Machine> {
    fs::read_to_string(machines_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn save_machines(all: &[Machine]) -> Result<(), String> {
    let path = machines_path();
    fs::create_dir_all(path.parent().unwrap_or(&path)).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_string_pretty(all).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let _ = crate::common::secure_file(&tmp);
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// A name that is safe as a folder and as a window title.
pub fn check_name(all: &[Machine], name: &str, own: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        Err("Give it a name.".into())
    } else if name.len() > 40 || !name.chars().all(|c| c.is_alphanumeric() || " _-".contains(c)) {
        Err("Letters, digits, spaces, _ and - only.".into())
    } else if name.eq_ignore_ascii_case("This PC") || all.iter().any(|m| m.name.eq_ignore_ascii_case(name) && m.name != own) {
        Err("Another machine has that name.".into())
    } else {
        Ok(name.into())
    }
}

// ---------------------------------------------------------------- the mirror

static ROOT: OnceLock<PathBuf> = OnceLock::new();
static ATTACHED: OnceLock<Machine> = OnceLock::new();
static STATUS: Mutex<String> = Mutex::new(String::new());
static INSTANCES: Mutex<Option<Result<(bool, Vec<Instance>), String>>> = Mutex::new(None);
static POWER: Mutex<Option<Result<Power, String>>> = Mutex::new(None);
static STARTING: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// Where this process keeps its data when it is not the user's own: the mirror of the machine it is attached to, or
/// `CCAT_ROOT` (tests, a second profile). `None`: the usual place.
pub fn root() -> Option<PathBuf> {
    ROOT.get().cloned().or_else(|| std::env::var_os("CCAT_ROOT").map(PathBuf::from))
}

/// The machine this process shows, if it is not this PC.
pub fn attached() -> Option<&'static Machine> {
    ATTACHED.get()
}

/// "Connected" or why not, for the sidebar.
pub fn status() -> String {
    STATUS.lock().unwrap().clone()
}

/// What the machine's terminals were at the last look: tmux there?, the sessions.
pub fn instances() -> Option<Result<(bool, Vec<Instance>), String>> {
    INSTANCES.lock().unwrap().clone()
}

/// Makes this process work on `name`'s mirror and keeps it in step: the first pull is waited for (a window opens with
/// the machine's data), the rest runs in the background. An unreachable machine opens on what was mirrored last time.
pub fn attach(name: &str) -> Result<(), String> {
    let m = machines().into_iter().find(|m| m.name == name).ok_or_else(|| format!("no machine named {name}"))?;
    let dir = crate::user_config_path().with_file_name("remote").join(&m.name);
    fs::create_dir_all(dir.join("profiles")).and_then(|_| fs::create_dir_all(dir.join("live"))).map_err(|e| e.to_string())?;
    let _ = ROOT.set(dir.clone());
    let _ = ATTACHED.set(m.clone());
    let mut st = State::default();
    let first = sync(&m, &dir, &mut st);
    set_status(&first);
    std::thread::spawn(move || loop {
        std::thread::sleep(TICK);
        set_status(&sync(&m, &dir, &mut st));
        *INSTANCES.lock().unwrap() = Some(list_instances(&m));
        *POWER.lock().unwrap() = Some(get_power(&m));
    });
    Ok(())
}

fn set_status(r: &Result<(), String>) {
    *STATUS.lock().unwrap() = match r {
        Ok(()) => "Connected".into(),
        Err(e) if e.contains("timeout") || e.contains("onnect") => "Offline: asleep or unreachable (Terminals > Turn on)".into(),
        Err(e) => format!("Offline: {e}"),
    };
}

/// What a synced path is: `Some(writable)` for the few files the window shows (`config.json`, `profiles/<name>.json`,
/// `live/<n>.json`, usage, pricing, the request log), `None` for anything else. Both sides use it: the agent to decide
/// what it serves, and the window so a machine can never name a file outside its own mirror.
pub(crate) fn rel_kind(rel: &str) -> Option<bool> {
    let safe = |f: &str| f.ends_with(".json") && f.len() <= 120 && !f.starts_with('.') && f.chars().all(|c| c.is_alphanumeric() || " ._-".contains(c));
    match rel.split_once('/') {
        None => match rel {
            "config.json" => Some(true),
            "usage.json" | "pricing.json" | "tokens.jsonl" => Some(false),
            _ => None,
        },
        Some(("profiles", f)) if safe(f) => Some(true),
        Some(("live", f)) if safe(f) => Some(false),
        _ => None,
    }
}

/// A 64-bit FNV-1a hash in hex: enough to tell a config that changed from one that did not.
pub fn fnv(bytes: &[u8]) -> String {
    format!("{:016x}", bytes.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3)))
}

#[derive(Default)]
struct State {
    /// Writable files: the content hash both sides last agreed on.
    last: HashMap<String, String>,
    /// Read-only files: the machine's modified time at the last pull.
    seen: HashMap<String, u64>,
    /// How much of the request log has come down (an offset in the machine's file).
    tokens: u64,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).build().into()
}

/// `addr` is `host:port` (plain HTTP, a private network) or a full `https://host/prefix` URL (through a proxy).
fn url(m: &Machine, path: &str) -> String {
    let a = m.addr.trim().trim_end_matches('/');
    if a.starts_with("http://") || a.starts_with("https://") { format!("{a}{path}") } else { format!("http://{a}{path}") }
}

fn encode(rel: &str) -> String {
    rel.replace(' ', "%20").replace('/', "%2F")
}

fn get(m: &Machine, path: &str) -> Result<Vec<u8>, String> {
    let mut r = agent().get(&url(m, path)).header("authorization", format!("Bearer {}", m.token)).call().map_err(|e| e.to_string())?;
    r.body_mut().read_to_vec().map_err(|e| e.to_string())
}

fn put(m: &Machine, rel: &str, body: &[u8]) -> Result<(), String> {
    agent().put(&url(m, &format!("/v1/file?path={}", encode(rel)))).header("authorization", format!("Bearer {}", m.token)).send(body).map(|_| ()).map_err(|e| e.to_string())
}

fn delete(m: &Machine, rel: &str) -> Result<(), String> {
    agent().delete(&url(m, &format!("/v1/file?path={}", encode(rel)))).header("authorization", format!("Bearer {}", m.token)).call().map(|_| ()).map_err(|e| e.to_string())
}

/// Written whole and renamed into place, so a window reading it never sees half a file.
fn write_mirror(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, bytes).and_then(|_| fs::rename(&tmp, path)).map_err(|e| e.to_string())
}

/// One round: what changed on the machine comes down, what changed here goes up. Returns the first error.
fn sync(m: &Machine, dir: &Path, st: &mut State) -> Result<(), String> {
    let manifest: serde_json::Value = serde_json::from_slice(&get(m, "/v1/manifest")?).map_err(|e| e.to_string())?;
    let files = manifest["files"].as_array().cloned().unwrap_or_default();
    let remote: Vec<String> = files.iter().filter_map(|f| f["path"].as_str().filter(|p| rel_kind(p).is_some()).map(String::from)).collect();
    let mut first_err: Option<String> = None;
    let mut note = |r: Result<(), String>| {
        if let Err(e) = r {
            first_err.get_or_insert(e);
        }
    };

    // A profile deleted here is deleted there; one deleted there goes from here.
    let mut gone: Vec<String> = Vec::new();
    for rel in st.last.keys().filter(|r| r.starts_with("profiles/")).cloned().collect::<Vec<_>>() {
        if !dir.join(&rel).exists() && remote.contains(&rel) {
            note(delete(m, &rel));
            st.last.remove(&rel);
            gone.push(rel);
        } else if !remote.contains(&rel) {
            let _ = fs::remove_file(dir.join(&rel));
            st.last.remove(&rel);
        }
    }
    for f in &files {
        let rel = f["path"].as_str().unwrap_or("");
        // Never trust a name the machine sends: only the known files, each under the mirror.
        let Some(writable) = rel_kind(rel) else { continue };
        if gone.iter().any(|g| g == rel) {
            continue; // deleted just now: the manifest still lists it
        }
        let (mtime, size) = (f["mtime"].as_u64().unwrap_or(0), f["size"].as_u64().unwrap_or(0));
        let local = dir.join(rel);
        if rel == "tokens.jsonl" {
            note(pull_tokens(m, &local, size, st));
        } else if writable {
            let remote_hash = f["hash"].as_str().unwrap_or("").to_string();
            let local_hash = fs::read(&local).ok().map(|b| fnv(&b));
            let last = st.last.get(rel).cloned();
            match (&local_hash, &last) {
                // Edited here since the last agreement: it goes up.
                (Some(l), Some(last)) if l != last => {
                    let r = put(m, rel, &fs::read(&local).unwrap_or_default());
                    if r.is_ok() {
                        st.last.insert(rel.into(), l.clone());
                    }
                    note(r);
                }
                (Some(l), _) if *l == remote_hash => {
                    st.last.insert(rel.into(), remote_hash);
                }
                // Not here, or the machine changed it: it comes down.
                _ => {
                    let r = get(m, &format!("/v1/file?path={}", encode(rel))).and_then(|b| write_mirror(&local, &b).map(|_| fnv(&b)));
                    match r {
                        Ok(h) => {
                            st.last.insert(rel.into(), h);
                        }
                        Err(e) => note(Err(e)),
                    }
                }
            }
        } else if st.seen.get(rel) != Some(&mtime) || !local.exists() {
            let r = get(m, &format!("/v1/file?path={}", encode(rel))).and_then(|b| write_mirror(&local, &b));
            if r.is_ok() {
                st.seen.insert(rel.into(), mtime);
            }
            note(r);
        }
    }
    // Live sessions that ended there.
    for e in fs::read_dir(dir.join("live")).into_iter().flatten().flatten() {
        if !remote.contains(&format!("live/{}", e.file_name().to_string_lossy())) {
            let _ = fs::remove_file(e.path());
        }
    }
    first_err.map_or(Ok(()), Err)
}

/// The request log: the end of it on the first look, then only what was appended (whole lines).
fn pull_tokens(m: &Machine, local: &Path, size: u64, st: &mut State) -> Result<(), String> {
    use std::io::Write;
    if size < st.tokens || (st.tokens == 0 && local.exists()) {
        let _ = fs::remove_file(local); // it shrank there, or this is a new window: start again
        st.tokens = 0;
    }
    if size == st.tokens && local.exists() {
        return Ok(());
    }
    let first = st.tokens == 0;
    let from = if first { size.saturating_sub(TOKENS_TAIL) } else { st.tokens };
    let body = get(m, &format!("/v1/file?path=tokens.jsonl&from={from}"))?;
    let start = if first && from > 0 { body.iter().position(|&b| b == b'\n').map_or(body.len(), |i| i + 1) } else { 0 };
    let end = body.iter().rposition(|&b| b == b'\n').map_or(start, |i| i + 1).max(start);
    let mut f = fs::OpenOptions::new().create(true).append(true).open(local).map_err(|e| e.to_string())?;
    f.write_all(&body[start..end]).map_err(|e| e.to_string())?;
    st.tokens = from + end as u64;
    Ok(())
}

// ---------------------------------------------------------------- terminals

/// A terminal (tmux session) running on the machine.
#[derive(Clone, Debug, PartialEq)]
pub struct Instance {
    pub name: String,
    pub attached: bool,
    /// What runs in it now (`claude`, `bash`…).
    pub command: String,
    pub path: String,
    pub created: u64,
}

fn list_instances(m: &Machine) -> Result<(bool, Vec<Instance>), String> {
    let v: serde_json::Value = serde_json::from_slice(&get(m, "/v1/sessions")?).map_err(|e| e.to_string())?;
    let text = |s: &serde_json::Value, k: &str| s[k].as_str().unwrap_or("").to_string();
    let list = v["sessions"].as_array().into_iter().flatten().map(|s| Instance { name: text(s, "name"), attached: s["attached"].as_bool().unwrap_or(false), command: text(s, "command"), path: text(s, "path"), created: s["created"].as_u64().unwrap_or(0) }).collect();
    Ok((v["tmux"].as_bool().unwrap_or(false), list))
}

/// When the machine sleeps and how it is kept awake (see `agent`'s power API).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Power {
    /// The machine has the idle watchdog this works with.
    pub supported: bool,
    pub now: u64,
    pub sleep_at: Option<u64>,
    pub stay_awake: bool,
    pub building: bool,
    pub hard_stop: u64,
}

fn parse_power(v: &serde_json::Value) -> Power {
    Power { supported: v["supported"].as_bool().unwrap_or(false), now: v["now"].as_u64().unwrap_or(0), sleep_at: v["sleep_at"].as_u64(), stay_awake: v["stay_awake"].as_bool().unwrap_or(false), building: v["building"].as_bool().unwrap_or(false), hard_stop: v["hard_stop"].as_u64().unwrap_or(0) }
}

fn get_power(m: &Machine) -> Result<Power, String> {
    serde_json::from_slice(&get(m, "/v1/power")?).map(|v| parse_power(&v)).map_err(|e| e.to_string())
}

/// The machine's power as of the last look.
pub fn power() -> Option<Result<Power, String>> {
    POWER.lock().unwrap().clone()
}

/// `{"action":"timer","minutes":30}`, `{"action":"stay","on":true}` or `{"action":"sleep_now"}`.
pub fn power_set(m: &Machine, body: &serde_json::Value) -> Result<(), String> {
    let r = agent().post(&url(m, "/v1/power")).header("authorization", format!("Bearer {}", m.token)).send(body.to_string().as_bytes()).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_reader(r.into_body().into_reader()).map_err(|e| e.to_string())?;
    *POWER.lock().unwrap() = Some(Ok(parse_power(&v)));
    Ok(())
}

/// Turns the machine's VM on with the Azure CLI (signed in there); it takes a minute or two.
pub fn start_vm(m: &Machine) -> Result<(), String> {
    let (rg, name) = m.vm.split_once('/').filter(|(a, b)| !a.is_empty() && !b.is_empty() && !m.vm.contains(char::is_whitespace)).ok_or("Set the Azure VM as resource-group/name first.")?;
    let az = ["C:\\Program Files\\Microsoft SDKs\\Azure\\CLI2\\wbin\\az.cmd", "az"].into_iter().find(|p| *p == "az" || Path::new(p).exists()).unwrap_or("az");
    #[cfg(windows)]
    let mut cmd = {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd");
        c.args(["/c", az]).creation_flags(0x0800_0000);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = Command::new(az);
    cmd.args(["vm", "start", "-g", rg, "-n", name, "--no-wait"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    cmd.spawn().map_err(|e| format!("could not run the Azure CLI: {e}"))?;
    *STARTING.lock().unwrap() = Some(std::time::Instant::now());
    Ok(())
}

/// A start was asked for in the last two minutes.
pub fn starting() -> bool {
    STARTING.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_secs(120))
}

/// `ccat --ccat-tunnel <machine>`, an `ssh` ProxyCommand: stdin and stdout are the machine's sshd, carried over the
/// agent's HTTPS (a WebSocket), for networks that let nothing but the web through.
pub fn tunnel(name: &str) -> Result<(), String> {
    use std::io::Read;
    let ws = connect_ssh(name)?;
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 16 << 10];
        let mut stdin = std::io::stdin().lock();
        while let Ok(n @ 1..) = stdin.read(&mut buf) {
            if tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    pump(ws, rx, &mut std::io::stdout())
}

/// `ccat --ccat-forward <machine> <port>`: every connection to 127.0.0.1:<port> is a pipe to the machine's sshd (as
/// `tunnel`), for programs whose ssh cannot run a ProxyCommand (SSHFS-Win's).
pub fn forward(name: &str, port: u16) -> Result<(), String> {
    use std::io::Read;
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("cannot listen on {port}: {e}"))?;
    for conn in listener.incoming().flatten() {
        let name = name.to_string();
        std::thread::spawn(move || {
            let Ok(ws) = connect_ssh(&name) else { return };
            let Ok(mut reader) = conn.try_clone() else { return };
            let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
            std::thread::spawn(move || {
                let mut buf = [0u8; 16 << 10];
                while let Ok(n @ 1..) = reader.read(&mut buf) {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            });
            let mut out = conn;
            let _ = pump(ws, rx, &mut out);
        });
    }
    Ok(())
}

type Ws = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

fn connect_ssh(name: &str) -> Result<Ws, String> {
    use tungstenite::{client::IntoClientRequest, stream::MaybeTlsStream};
    let m = machines().into_iter().find(|m| m.name == name).ok_or_else(|| format!("no machine named {name}"))?;
    let ws_url = url(&m, "/v1/ssh").replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
    let mut req = ws_url.into_client_request().map_err(|e| e.to_string())?;
    req.headers_mut().insert("authorization", format!("Bearer {}", m.token).parse().map_err(|_| "bad token")?);
    let (mut ws, _) = tungstenite::connect(req).map_err(|e| format!("could not reach {}: {e}", m.addr))?;
    let slow = Some(Duration::from_millis(10));
    match ws.get_mut() {
        MaybeTlsStream::Plain(s) => s.set_read_timeout(slow),
        MaybeTlsStream::Rustls(s) => s.get_mut().set_read_timeout(slow),
        _ => Ok(()),
    }
    .map_err(|e| e.to_string())?;
    Ok(ws)
}

/// Carries bytes both ways until either side ends: what `rx` gets goes to the machine, what it sends goes to `out`.
fn pump(mut ws: Ws, rx: std::sync::mpsc::Receiver<Vec<u8>>, out: &mut dyn std::io::Write) -> Result<(), String> {
    use tungstenite::Message;
    loop {
        match rx.try_recv() {
            Ok(b) => ws.send(Message::Binary(b.into())).map_err(|e| e.to_string())?,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break, // ssh closed its side
            Err(_) => {}
        }
        match ws.read() {
            Ok(Message::Binary(b)) => {
                out.write_all(&b).and_then(|_| out.flush()).map_err(|e| e.to_string())?;
            }
            Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => break,
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Ok(_) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    let _ = ws.close(None);
    Ok(())
}

/// A session name is passed to a shell over there: only plain characters.
pub fn session_ok(name: &str) -> bool {
    !name.is_empty() && name.len() <= 40 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A name not used by `existing`: `term`, `term-2`…
pub fn new_session_name(existing: &[Instance]) -> String {
    std::iter::once("term".to_string()).chain((2..).map(|i| format!("term-{i}"))).find(|n| !existing.iter().any(|s| s.name == *n)).unwrap()
}

/// Opens a window here that is the machine's terminal `session` (made if it does not exist; a running one is
/// attached, so leaving the window leaves it running there). `ccat-term` is the machine's wrapper (see `docs`).
pub fn open_terminal(m: &Machine, session: &str) -> Result<(), String> {
    if !session_ok(session) {
        return Err("A session name has letters, digits, - and _ only.".into());
    }
    let target = m.ssh.trim();
    if target.is_empty() || target.contains(char::is_whitespace) || target.starts_with('-') {
        return Err("Set the machine's SSH target (user@host) first.".into());
    }
    let title = format!("{}: {session}", m.name);
    // A machine reached by URL has no open SSH port: ssh goes through this very program (see `tunnel`).
    let proxy = if m.addr.trim().starts_with("http") {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?.display().to_string();
        if exe.contains(char::is_whitespace) || m.name.contains(char::is_whitespace) {
            return Err("A machine reached over HTTPS needs a name and a ccat path without spaces.".into());
        }
        format!("ProxyCommand={exe} {} {}", TUNNEL_FLAG, m.name)
    } else {
        "ProxyCommand=none".into()
    };
    #[cfg(windows)]
    let r = Command::new("cmd").args(["/c", "start", &title, "ssh", "-t", "-o", "ServerAliveInterval=30", "-o", &proxy, target, "ccat-term", session]).spawn();
    #[cfg(target_os = "macos")]
    let r = {
        let script = format!(
            "tell application \"Terminal\" to do script \"printf '\\\\e]2;{}\\\\a' && ssh -t -o ServerAliveInterval=30 -o {} {} ccat-term {}\"",
            title, proxy, target, session
        );
        Command::new("osascript").args(["-e", &script, "-e", "tell application \"Terminal\" to activate"]).spawn()
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = Command::new("x-terminal-emulator").args(["-T", &title, "-e", "ssh", "-t", "-o", "ServerAliveInterval=30", "-o", &proxy, target, "ccat-term", session]).spawn();
    r.map(|_| ()).map_err(|e| format!("could not open a terminal: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_checked() {
        let all = [Machine { name: "azure".into(), ..Default::default() }];
        assert_eq!(check_name(&all, " vm 2 ", ""), Ok("vm 2".into()));
        assert!(check_name(&all, "Azure", "").is_err(), "taken, whatever the case");
        assert!(check_name(&all, "azure", "azure").is_ok(), "its own name stays");
        assert!(check_name(&all, "this pc", "").is_err() && check_name(&all, "", "").is_err() && check_name(&all, "a/b", "").is_err());
    }

    #[test]
    fn sessions_are_named_safely() {
        for ok in ["main", "term-2", "a_b"] {
            assert!(session_ok(ok), "{ok}");
        }
        for bad in ["", "a b", "a;rm", "$(x)", "a/b", &"x".repeat(41)] {
            assert!(!session_ok(bad), "{bad}");
        }
        let one = |n: &str| Instance { name: n.into(), attached: false, command: String::new(), path: String::new(), created: 0 };
        assert_eq!(new_session_name(&[one("term"), one("main")]), "term-2");
    }

    /// The mirror and a machine (an agent on a local port) follow each other: pulls, pushes, deletes, appends, a wrong token.
    #[test]
    fn the_mirror_and_the_machine_follow_each_other() {
        let _g = crate::tests::CONFIG_FILE.lock().unwrap_or_else(|e| e.into_inner());
        let machine_dir = crate::config_path().parent().unwrap().to_path_buf();
        fs::create_dir_all(machine_dir.join("profiles")).unwrap();
        let (cfg, prof, log) = (machine_dir.join("config.json"), machine_dir.join("profiles/Default.json"), machine_dir.join("tokens.jsonl"));
        fs::write(&cfg, r#"{"bypass":false}"#).unwrap();
        fs::write(&prof, r#"{"tiers":[]}"#).unwrap();
        fs::write(&log, "{\"t\":1}\n{\"t\":2}\n").unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            for s in listener.incoming().flatten() {
                std::thread::spawn(|| {
                    let _ = crate::agent::serve(s, "tok");
                });
            }
        });
        let m = Machine { name: "t".into(), addr, token: "tok".into(), ..Default::default() };
        let mirror = std::env::temp_dir().join(format!("ccat-mirror-{}", std::process::id()));
        fs::create_dir_all(mirror.join("profiles")).unwrap();
        fs::create_dir_all(mirror.join("live")).unwrap();
        let mut st = State::default();

        sync(&m, &mirror, &mut st).unwrap();
        assert_eq!(fs::read_to_string(mirror.join("config.json")).unwrap(), r#"{"bypass":false}"#, "the config comes down");
        assert!(mirror.join("profiles/Default.json").exists());
        assert_eq!(fs::read_to_string(mirror.join("tokens.jsonl")).unwrap().lines().count(), 2, "the log comes down");

        fs::write(mirror.join("config.json"), r#"{"bypass":true}"#).unwrap();
        sync(&m, &mirror, &mut st).unwrap();
        assert_eq!(fs::read_to_string(&cfg).unwrap(), r#"{"bypass":true}"#, "an edit here goes up");

        fs::write(&cfg, r#"{"bypass":false,"hold_hours":5}"#).unwrap();
        sync(&m, &mirror, &mut st).unwrap();
        assert_eq!(fs::read_to_string(mirror.join("config.json")).unwrap(), r#"{"bypass":false,"hold_hours":5}"#, "an edit there comes down");

        fs::write(&log, "{\"t\":1}\n{\"t\":2}\n{\"t\":3}\n{\"t\":4").unwrap(); // the last line is still being written
        sync(&m, &mirror, &mut st).unwrap();
        assert_eq!(fs::read_to_string(mirror.join("tokens.jsonl")).unwrap().lines().count(), 3, "only whole lines are appended");

        fs::remove_file(mirror.join("profiles/Default.json")).unwrap();
        sync(&m, &mirror, &mut st).unwrap();
        assert!(!prof.exists(), "a profile deleted here is deleted there");

        let bad = Machine { token: "nope".into(), ..m.clone() };
        assert!(sync(&bad, &mirror, &mut State::default()).is_err(), "a wrong token is refused");

        for f in [&cfg, &log] {
            let _ = fs::remove_file(f);
        }
        let _ = fs::remove_dir_all(&mirror);
    }

    #[test]
    fn a_machine_can_only_name_the_known_files() {
        assert_eq!((rel_kind("config.json"), rel_kind("profiles/A b.json"), rel_kind("live/1.json"), rel_kind("tokens.jsonl")), (Some(true), Some(true), Some(false), Some(false)));
        for bad in ["../x.json", "profiles/../../x.json", "profiles/..\\x.json", "C:/x.json", "/etc/passwd", "profiles/a/b.json", "chatgpt/k.json", "profiles/.json", "agent-token"] {
            assert_eq!(rel_kind(bad), None, "{bad}");
        }
    }

    #[test]
    fn power_is_read_from_what_the_agent_says() {
        let p = parse_power(&serde_json::json!({ "supported": true, "now": 100, "sleep_at": 460, "stay_awake": false, "building": false, "hard_stop": 999 }));
        assert_eq!((p.supported, p.sleep_at, p.stay_awake, p.hard_stop), (true, Some(460), false, 999));
        assert_eq!(parse_power(&serde_json::json!({ "sleep_at": null })).sleep_at, None);
    }

    /// The ssh pipe over the agent: a WebSocket with the token reaches the machine's sshd (here an echo server), one without does not.
    #[test]
    fn the_ssh_pipe_needs_the_token_and_carries_bytes() {
        use std::io::{Read, Write};
        let echo = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std::env::set_var("CCAT_SSHD", echo.local_addr().unwrap().to_string());
        std::thread::spawn(move || {
            for mut s in echo.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut b = [0u8; 256];
                    while let Ok(n @ 1..) = s.read(&mut b) {
                        if s.write_all(&b[..n]).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        let agent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = agent.local_addr().unwrap();
        std::thread::spawn(move || {
            for s in agent.incoming().flatten() {
                std::thread::spawn(|| {
                    let _ = crate::agent::serve(s, "tok");
                });
            }
        });
        let connect = |token: &str| {
            use tungstenite::client::IntoClientRequest;
            let mut req = format!("ws://{addr}/v1/ssh").into_client_request().unwrap();
            req.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
            tungstenite::connect(req)
        };
        assert!(connect("wrong").is_err(), "a wrong token is refused");
        let (mut ws, _) = connect("tok").unwrap();
        ws.send(tungstenite::Message::Binary(b"SSH-2.0-test\r\n".to_vec().into())).unwrap();
        match ws.read().unwrap() {
            tungstenite::Message::Binary(b) => assert_eq!(&b[..], b"SSH-2.0-test\r\n", "bytes go through and come back"),
            m => panic!("unexpected {m:?}"),
        }
    }

    #[test]
    fn the_hash_tells_changes_apart() {
        assert_eq!(fnv(b"a"), fnv(b"a"));
        assert_ne!(fnv(b"a"), fnv(b"b"));
    }
}
