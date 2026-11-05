//! OpenCode's anonymous free tier (no user API key), adapted from 12errh/zen-proxy (MIT). The gateway serves its free
//! models only to traffic shaped like the OpenCode CLI, so on top of the plain OpenAI-compatible route (`openai.rs`)
//! this adds the CLI's identity headers, its harness preamble ahead of the system prompt and its own tool manifest
//! ahead of Claude Code's tools. The model then calls either set; replies keep only the calls Claude Code can run,
//! manifest ones renamed back (`read` -> `Read`, `path` -> `file_path`).
//! `opencode/preamble.txt` and `opencode/tools.json` are captures of OpenCode 2.x's request; the gate fingerprints
//! the preamble, so they are kept verbatim (re-capture both when OpenCode changes them).

pub(crate) mod egress;
pub(crate) mod ui;

use serde_json::{json, Map, Value};
use std::io::{self, BufRead, Read};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Where the CLI sends chat requests; `/models` is only served on `/inference/v1`.
pub(crate) const URL: &str = "https://opencode.ai/inference/openai/v1";
fn models_endpoint(url: &str) -> String {
    let base = url.trim().trim_end_matches('/');
    // The inference catalog is shared across wire formats; other proxy bases expose their own /models.
    match base.strip_suffix("/openai/v1") {
        Some(root) => format!("{root}/v1/models"),
        None => format!("{base}/models"),
    }
}
const UA: &str = "opencode/latest/2.1.4/cli";
const MARKER: &str = "You are an AI agent running in OpenCode";
/// A session id carries its creation time; upstream refuses stale ones, so the pair is re-minted on this schedule.
const ROTATE: Duration = Duration::from_secs(30 * 60);
/// A burst of requests from one session is refused (403 FreeTierError); ~0.4 s apart passes.
const PACE: Duration = Duration::from_millis(400);

/// git may check the capture out with CRLF; the gate wants the CLI's exact text.
static PREAMBLE: LazyLock<String> = LazyLock::new(|| include_str!("opencode/preamble.txt").replace("\r\n", "\n"));
static MANIFEST: LazyLock<Vec<Value>> = LazyLock::new(|| serde_json::from_str(include_str!("opencode/tools.json")).expect("opencode/tools.json"));

/// Manifest tool -> Claude Code tool and its argument names; an argument not listed is dropped (Claude Code rejects
/// unknown ones).
const MAP: &[(&str, &str, &[(&str, &str)])] = &[
    ("read", "Read", &[("path", "file_path"), ("offset", "offset"), ("limit", "limit")]),
    ("edit", "Edit", &[("path", "file_path"), ("oldString", "old_string"), ("newString", "new_string"), ("replaceAll", "replace_all")]),
    ("write", "Write", &[("path", "file_path"), ("content", "content")]),
    ("shell", "Bash", &[("command", "command"), ("timeout", "timeout"), ("background", "run_in_background")]),
    ("grep", "Grep", &[("pattern", "pattern"), ("path", "path"), ("include", "glob"), ("limit", "head_limit")]),
    ("glob", "Glob", &[("pattern", "pattern"), ("path", "path")]),
];

/// Whether an OpenAI-compatible endpoint is this free tier (its URL, as the preset fills it in).
pub(crate) fn is_free_tier(url: &str) -> bool {
    url.trim().trim_end_matches('/').eq_ignore_ascii_case(URL)
}

/// The free models of the public catalog: priced 0 in and out (`big-pickle` has no `-free` in its name).
pub(crate) fn fetch_models(url: &str) -> Result<Vec<String>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).build().into();
    let body = agent.get(models_endpoint(url)).call().map_err(|e| e.to_string())?.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("response is not JSON: {e}"))?;
    Ok(free_ids(&v))
}

fn free_ids(v: &Value) -> Vec<String> {
    let free = |m: &Value| ["input", "output"].iter().all(|k| m["cost"]["text"][k]["price"].as_f64() == Some(0.0));
    v["data"].as_array().into_iter().flatten().filter(|m| free(m)).filter_map(|m| m["id"].as_str().map(String::from)).collect()
}

struct Identity {
    at: Instant,
    session: String,
    project: String,
}
static IDENTITY: Mutex<Option<Identity>> = Mutex::new(None);
static LAST_SENT: Mutex<Option<Instant>> = Mutex::new(None);
/// The CLI keeps one trace root per process; spans change per request.
static TRACE: LazyLock<String> = LazyLock::new(|| crate::common::random_hex(16));

/// The CLI's id format: `ses_` + 12 hex of !(ms * 0x1000 + counter) + 14 base62. Upstream decodes the hex as the
/// creation time and refuses an implausible one (403), so a random id does not pass.
fn session_id(ms: u64) -> String {
    const B62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut tail = [0u8; 14];
    getrandom::fill(&mut tail).expect("OS randomness");
    let tail: String = tail.iter().map(|b| B62[*b as usize % 62] as char).collect();
    format!("ses_{:012x}{tail}", !(ms * 0x1000 + 1) & 0xffff_ffff_ffff)
}

/// The CLI's request headers: one session+project pair (one CLI conversation in one project), rotated together.
/// Also spaces requests `PACE` apart, as Claude Code fires its title and main requests at once.
pub(crate) fn headers() -> Vec<(&'static str, String)> {
    {
        let mut last = LAST_SENT.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(wait) = last.and_then(|t| PACE.checked_sub(t.elapsed())) {
            std::thread::sleep(wait);
        }
        *last = Some(Instant::now());
    }
    let mut id = IDENTITY.lock().unwrap_or_else(|e| e.into_inner());
    if id.as_ref().is_none_or(|i| i.at.elapsed() >= ROTATE) {
        let ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        // ponytail: random 40 hex, the shape of the CLI's sha1("directory:<path>"); upstream cannot tell them apart.
        *id = Some(Identity { at: Instant::now(), session: session_id(ms), project: crate::common::random_hex(20) });
    }
    let Identity { session, project, .. } = id.as_ref().expect("minted above");
    let (span, parent) = (crate::common::random_hex(8), crate::common::random_hex(8));
    let mut h = vec![("Accept", "application/json, text/event-stream".to_string()), ("User-Agent", UA.into())];
    for k in ["x-opencode-session", "x-opencode-session-id", "x-session-affinity", "x-session-id"] {
        h.push((k, session.clone()));
    }
    h.push(("x-opencode-client", "cli".into()));
    h.push(("x-opencode-project", project.clone()));
    h.push(("b3", format!("{}-{span}-1-{parent}", *TRACE)));
    h.push(("traceparent", format!("00-{}-{span}-01", *TRACE)));
    h
}

/// The names a reply may call: Claude Code's tools, or just the pinned one.
pub(crate) fn offered(payload: &Value) -> Vec<String> {
    if let Some(pin) = payload["tool_choice"]["function"]["name"].as_str() {
        return vec![pin.to_string()];
    }
    payload["tools"].as_array().into_iter().flatten().filter_map(|t| t["function"]["name"].as_str().map(String::from)).collect()
}

/// Shapes a Chat Completions request (from `openai::chat_body`) like the CLI's. Call `offered` before this.
pub(crate) fn shape(payload: &mut Value, body: &Value) {
    let messages = payload["messages"].as_array_mut().expect("chat_body builds messages");
    // One system message, the preamble first: a second system turn or a cut preamble is refused.
    match messages.first_mut().filter(|m| m["role"] == "system") {
        Some(m) if !m["content"].as_str().unwrap_or("").starts_with(MARKER) => {
            m["content"] = json!(format!("{}\n\n{}", *PREAMBLE, m["content"].as_str().unwrap_or("")));
        }
        Some(_) => {}
        None => messages.insert(0, json!({ "role": "system", "content": *PREAMBLE })),
    }
    let theirs = payload["tools"].as_array().cloned().unwrap_or_default();
    if theirs.is_empty() {
        // A title or summary request: nothing to run, so no call is wanted (and any is dropped on the way back).
        payload["tool_choice"] = json!("none");
    }
    let ours: Vec<&str> = MANIFEST.iter().filter_map(|t| t["function"]["name"].as_str()).collect();
    let mut tools = MANIFEST.clone();
    tools.extend(theirs.into_iter().filter(|t| !t["function"]["name"].as_str().is_some_and(|n| ours.contains(&n))));
    payload["tools"] = json!(tools);
    let max = body["max_tokens"].as_u64().unwrap_or(32_000).clamp(1, 32_000);
    payload["max_completion_tokens"] = json!(max);
}

/// What a reply's call becomes: Claude Code's own name kept, a manifest name renamed with its arguments, else dropped.
fn keep(offered: &[String], name: &str, args: &str) -> Option<(String, String)> {
    if offered.iter().any(|o| o == name) {
        return Some((name.into(), args.into()));
    }
    let (_, to, keys) = MAP.iter().find(|(from, to, _)| *from == name && offered.iter().any(|o| o == to))?;
    let src: Map<String, Value> = serde_json::from_str(args).unwrap_or_default();
    let out: Map<String, Value> = keys.iter().filter_map(|(a, b)| Some((b.to_string(), src.get(*a)?.clone()))).collect();
    Some((to.to_string(), Value::Object(out).to_string()))
}

/// Rewrites the upstream Chat Completions SSE: text streams through as it comes; tool calls are held until the turn
/// finishes (an argument rename needs them whole) and then sent in one chunk, kept or renamed per `keep`.
pub(crate) struct Calls {
    lines: io::Lines<Box<dyn BufRead + Send>>,
    offered: Vec<String>,
    /// (index, id, name, arguments) as they streamed in.
    held: Vec<(u64, String, String, String)>,
    flushed: bool,
    /// When the last line went downstream: held calls send nothing, and a long `Write` must not read as a dead stream.
    beat: Instant,
    out: Vec<u8>,
    pos: usize,
}

impl Calls {
    pub(crate) fn new(reader: Box<dyn BufRead + Send>, offered: Vec<String>) -> Self {
        Calls { lines: reader.lines(), offered, held: Vec::new(), flushed: false, beat: Instant::now(), out: Vec::new(), pos: 0 }
    }

    fn emit(&mut self, data: &str) {
        self.out.extend_from_slice(format!("data: {data}\n\n").as_bytes());
        self.beat = Instant::now();
    }

    /// The held calls Claude Code can run, as one chunk; `false` when every call was dropped.
    fn flush(&mut self) -> bool {
        if self.flushed || self.held.is_empty() {
            return false;
        }
        self.flushed = true;
        let calls: Vec<Value> = std::mem::take(&mut self.held).into_iter().filter_map(|(_, id, name, args)| keep(&self.offered, &name, &args).map(|n| (id, n)))
            .enumerate()
            .map(|(i, (id, (name, args)))| json!({ "index": i, "id": id, "type": "function", "function": { "name": name, "arguments": args } }))
            .collect();
        if calls.is_empty() {
            return false;
        }
        self.emit(&json!({ "choices": [{ "index": 0, "delta": { "tool_calls": calls } }] }).to_string());
        true
    }

    fn line(&mut self, line: &str) {
        let Some(data) = line.trim().strip_prefix("data:").map(str::trim) else {
            self.out.extend_from_slice(format!("{line}\n").as_bytes());
            return;
        };
        if data == "[DONE]" {
            self.flush();
            return self.emit(data);
        }
        let Ok(mut v) = serde_json::from_str::<Value>(data) else {
            return self.emit(data);
        };
        let Some(choice) = v["choices"].get_mut(0) else {
            return self.emit(data);
        };
        if let Some(Value::Array(calls)) = choice["delta"].as_object_mut().and_then(|d| d.remove("tool_calls")) {
            for c in calls {
                let idx = c["index"].as_u64().unwrap_or(0);
                let id = c["id"].as_str().filter(|s| !s.is_empty());
                // A new id is a new call even at the same index (some servers send parallel calls whole).
                let at = self.held.iter().rposition(|h| h.0 == idx && id.is_none_or(|id| id == h.1)).unwrap_or_else(|| {
                    let id = id.map(String::from).unwrap_or_else(|| format!("call_{}", crate::common::random_hex(12)));
                    self.held.push((idx, id, String::new(), String::new()));
                    self.held.len() - 1
                });
                let h = &mut self.held[at];
                if let Some(n) = c["function"]["name"].as_str().filter(|s| !s.is_empty()) {
                    h.2 = n.into();
                }
                h.3.push_str(c["function"]["arguments"].as_str().unwrap_or(""));
            }
        }
        if !choice["finish_reason"].is_string() {
            if choice["delta"].as_object().is_some_and(|d| d.values().any(|x| !x.is_null())) || v.get("usage").is_some_and(|u| u.is_object()) {
                self.emit(&v.to_string());
            } else if self.beat.elapsed() >= Duration::from_secs(5) {
                // An SSE comment: `run_openai_chat` turns it into a ping.
                self.out.extend_from_slice(b":\n\n");
                self.beat = Instant::now();
            }
            return;
        }
        let kept = self.flush();
        if choice["finish_reason"] == "tool_calls" && !kept {
            choice["finish_reason"] = json!("stop");
        }
        self.emit(&v.to_string());
    }
}

impl Read for Calls {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.out.len() {
            self.out.clear();
            self.pos = 0;
            match self.lines.next() {
                Some(line) => self.line(&line?),
                // A stream cut before its finish chunk still delivers what it called.
                None if self.flush() => {}
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.out.len() - self.pos);
        buf[..n].copy_from_slice(&self.out[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(sse: &[&str], offered: &[&str]) -> Value {
        let src: String = sse.iter().map(|e| format!("data: {e}\n\n")).collect();
        let calls = Calls::new(Box::new(io::Cursor::new(src.into_bytes())), offered.iter().map(|s| s.to_string()).collect());
        let sigs = Mutex::new(std::collections::HashMap::new());
        let mut tx = crate::proxy::sse::Tx::new("m", "asked", None, &sigs);
        assert!(tx.run_openai_chat(Box::new(io::BufReader::new(calls)), "OpenCode").is_ok());
        tx.message()
    }

    #[test]
    fn interleaved_parallel_tool_arguments_keep_their_call_ids() {
        let msg = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"read-id","function":{"name":"read","arguments":"{\"path\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"shell-id","function":{"name":"shell","arguments":"{\"command\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"\"pwd\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ], &["Read", "Bash"]);
        let calls = msg["content"].as_array().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], "read-id");
        assert_eq!(calls[0]["input"]["file_path"], "a.rs");
        assert_eq!(calls[1]["id"], "shell-id");
        assert_eq!(calls[1]["input"]["command"], "pwd");
    }

    #[test]
    fn manifest_calls_come_back_as_claude_codes_and_the_rest_are_dropped() {
        let msg = run(&[
            r#"{"choices":[{"delta":{"content":"Reading."}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read","arguments":"{\"path\":\"a.rs\","}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"limit\":5,\"bogus\":1}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"question","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":2,"id":"c","function":{"name":"Bash","arguments":"{\"command\":\"ls\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ], &["Read", "Bash"]);
        let c = msg["content"].as_array().unwrap();
        assert_eq!(c[0]["text"], "Reading.");
        assert_eq!((c[1]["name"].as_str(), &c[1]["input"]), (Some("Read"), &json!({ "file_path": "a.rs", "limit": 5 })), "renamed, unknown argument dropped");
        assert_eq!((c[2]["id"].as_str(), c[2]["name"].as_str()), (Some("c"), Some("Bash")), "the caller's own tool kept as is");
        assert_eq!(c.len(), 3, "`question` has no counterpart");

        let src = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"write","arguments":"{"}}]}}]}"#;
        let mut calls = Calls::new(Box::new(io::Cursor::new(format!("{src}\n\n").into_bytes())), vec!["Write".into()]);
        calls.beat -= Duration::from_secs(6);
        let mut out = String::new();
        calls.read_to_string(&mut out).unwrap();
        assert!(out.starts_with(":\n\n"), "a held call still keeps the stream alive: {out}");
        assert_eq!(msg["stop_reason"], "tool_use");
    }

    #[test]
    fn a_reply_whose_calls_all_drop_ends_as_a_plain_stop() {
        let msg = run(&[
            r#"{"choices":[{"delta":{"content":"Title"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        ], &[]);
        assert_eq!((msg["content"].as_array().unwrap().len(), msg["stop_reason"].as_str()), (1, Some("end_turn")));
    }

    #[test]
    fn requests_carry_the_preamble_once_and_the_manifest_first() {
        let body = json!({ "system": "be brief", "max_tokens": 64_000, "messages": [{ "role": "user", "content": "hi" }],
            "tools": [{ "name": "Read", "input_schema": { "type": "object" } }] });
        let mut p = crate::providers::openai::chat_body(&body, "big-pickle");
        assert_eq!(offered(&p), ["Read"]);
        shape(&mut p, &body);
        let sys = p["messages"][0]["content"].as_str().unwrap();
        assert!(sys.starts_with(MARKER) && sys.ends_with("\n\nbe brief") && !sys.contains('\r'));
        let names: Vec<&str> = p["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["edit", "glob", "grep", "read", "shell", "write", "Read"]);
        assert_eq!(p["max_completion_tokens"], 32_000);
        assert!(p["tool_choice"] == "auto");
        let again = p["messages"][0]["content"].clone();
        shape(&mut p, &body);
        assert_eq!(p["messages"][0]["content"], again, "an OpenCode-shaped system text is left alone");

        let bare = json!({ "messages": [{ "role": "user", "content": "title?" }] });
        let mut p = crate::providers::openai::chat_body(&bare, "big-pickle");
        shape(&mut p, &bare);
        assert_eq!((p["messages"][0]["role"].as_str(), p["tool_choice"].as_str()), (Some("system"), Some("none")));
    }

    #[test]
    fn model_catalog_uses_the_entered_url() {
        use std::{io::Write, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/inference/openai/v1/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let req = crate::proxy::http::read_request(&s).unwrap();
            assert_eq!(req.path, "/inference/v1/models");
            assert!(!req.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization") || k.eq_ignore_ascii_case("x-api-key")));
            let body = r#"{"data":[{"id":"big-pickle","cost":{"text":{"input":{"price":0},"output":{"price":0}}}},{"id":"paid","cost":{"text":{"input":{"price":1},"output":{"price":0}}}}]}"#;
            let mut s = s;
            write!(s, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        assert_eq!(fetch_models(&url).unwrap(), ["big-pickle"]);
        server.join().unwrap();
        assert_eq!(models_endpoint("http://localhost:1234/v1/"), "http://localhost:1234/v1/models");
    }

    #[test]
    fn session_ids_have_the_clis_shape_and_free_means_priced_zero() {
        let id = session_id(1_793_431_002_823);
        assert_eq!(id.len(), 4 + 12 + 14);
        assert_eq!(&id[..16], format!("ses_{:012x}", !(1_793_431_002_823u64 * 0x1000 + 1) & 0xffff_ffff_ffff));
        let cat = json!({ "data": [
            { "id": "big-pickle", "cost": { "text": { "input": { "price": 0 }, "output": { "price": 0 } } } },
            { "id": "gpt-5", "cost": { "text": { "input": { "price": 1.07 }, "output": { "price": 8.5 } } } },
        ] });
        assert_eq!(free_ids(&cat), ["big-pickle"]);
        assert!(is_free_tier("https://opencode.ai/inference/openai/v1/") && !is_free_tier("https://opencode.ai/zen/v1"));
    }
}
