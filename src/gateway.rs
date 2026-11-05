//! The gateway: ccat's tiers offered to other apps (Cursor, Cline, opencode, scripts…) at a fixed local URL, in both
//! dialects: Anthropic Messages (`/v1/messages`) and OpenAI Chat Completions (`/v1/chat/completions`), with
//! `/v1/models` for their pickers. Each tier shows under a name of its own (Settings → Gateway; by default its model
//! id); a request runs that tier's route and fallbacks through an in-process proxy (see `proxy`), reached over
//! loopback with its token, so the gateway's keys never travel further. Claude tiers are not served: the gateway has
//! no Claude Code login. The tray (and `ccat --serve`) runs it, following the config while it runs.

use crate::common::{self, Failure};
use crate::proxy::http::{error_kind, forward, read_request, respond, respond_error, Req};
use crate::proxy::{self, Fail, Upstream};
use crate::{Config, TIERS};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{
    collections::HashMap,
    io::{self, BufRead, BufReader, ErrorKind, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

pub(crate) const DEFAULT_PORT: u16 = 8789;
/// Anthropic requires `max_tokens`; Chat Completions clients often leave it out. The tiers already take Claude Code's
/// 32k, so this is known to pass.
const MAX_TOKENS: u64 = 32_000;

/// Settings → Gateway.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub(crate) struct Gateway {
    pub(crate) on: bool,
    pub(crate) port: u16,
    /// Any key, or none, is let in.
    pub(crate) any_key: bool,
    pub(crate) keys: Vec<String>,
    /// The model name each tier (TIERS order) shows under; "" = its default (see `default_name`).
    pub(crate) names: [String; 4],
}

impl Default for Gateway {
    fn default() -> Self {
        Self { on: false, port: DEFAULT_PORT, any_key: false, keys: Vec::new(), names: Default::default() }
    }
}

pub(crate) fn new_key() -> String {
    format!("sk-ccat-{}", common::random_hex(24))
}

/// Tier `t`'s name when none is set: its model id, else its key (`opus`) for a tier left at Claude Code's pick.
pub(crate) fn default_name(cfg: &Config, t: usize) -> String {
    let model = crate::bare_id(&cfg.tiers[t].model);
    let model = model.split('[').next().unwrap_or("").trim();
    if model.is_empty() { TIERS[t].1.to_string() } else { model.to_string() }
}

pub(crate) fn names(cfg: &Config) -> [String; 4] {
    std::array::from_fn(|t| Some(cfg.gateway.names[t].trim()).filter(|n| !n.is_empty()).map_or_else(|| default_name(cfg, t), String::from))
}

/// The tier a model name asks for: a tier's name, then its key (`opus`, or an `@opus` suffix), then a name with a
/// tier's key in it (`claude-opus-…`, what Claude Code itself sends).
fn tier_of(names: &[String; 4], asked: &str) -> Option<usize> {
    let a = asked.split('[').next().unwrap_or("").trim().to_lowercase();
    let (base, suffix) = a.rsplit_once('@').unwrap_or((&a, ""));
    (0..4)
        .find(|&t| names[t].to_lowercase() == a)
        .or_else(|| (0..4).find(|&t| TIERS[t].1 == a || TIERS[t].1 == suffix))
        .or_else(|| (0..4).find(|&t| base.contains(TIERS[t].1)))
}

fn allowed(g: &Gateway, req: &Req) -> bool {
    if g.any_key {
        return true;
    }
    let bearer = req.header("authorization").and_then(|a| a.strip_prefix("Bearer "));
    let given = [req.header("x-api-key"), bearer];
    g.keys.iter().map(|k| k.trim()).filter(|k| !k.is_empty()).any(|k| given.contains(&Some(k)))
}

fn status_path() -> PathBuf {
    crate::config_path().with_file_name("gateway-status.txt")
}

/// What the gateway last said about itself ("" = never ran), for Settings.
pub(crate) fn status() -> String {
    std::fs::read_to_string(status_path()).unwrap_or_default()
}

fn set_status(text: &str) {
    let _ = std::fs::write(status_path(), text);
}

/// Runs the gateway in the background while the config has it on, moving it when the port changes.
pub fn spawn() {
    std::thread::spawn(|| {
        let mut px: Option<proxy::Proxy> = None;
        // The address listened on (or tried), and the switch that stops its loop.
        let mut bound: Option<(String, Arc<AtomicBool>)> = None;
        loop {
            let cfg = crate::usage::read_config();
            let want = cfg.gateway.on.then(|| format!("127.0.0.1:{}", cfg.gateway.port));
            if bound.as_ref().map(|b| &b.0) != want.as_ref() {
                if let Some((_, stop)) = bound.take() {
                    stop.store(true, Ordering::Relaxed);
                }
                match want {
                    None => set_status("off"),
                    Some(addr) => {
                        let stop = Arc::new(AtomicBool::new(false));
                        match start(&mut px, &cfg, &addr, stop.clone()) {
                            Ok(()) => set_status(&format!("listening on {addr}")),
                            Err(e) => {
                                common::log(&format!("gateway: cannot listen on {addr}: {e}"));
                                set_status(&format!("cannot listen on {addr}: {e}"));
                            }
                        }
                        // ponytail: a failed address is tried again only once the setting changes (or the tray restarts).
                        bound = Some((addr, stop));
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

fn start(px: &mut Option<proxy::Proxy>, cfg: &Config, addr: &str, stop: Arc<AtomicBool>) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    if px.is_none() {
        // Claude Code's tier names never reach it: the gateway sends `@tier` (see `ask`). No Claude fallbacks: no login here.
        let mut routes: Vec<proxy::Route> = (0..4).map(|t| proxy::Route { tier: TIERS[t].1, names: Vec::new(), upstream: Upstream::Official, model: String::new(), fallbacks: Vec::new() }).collect();
        // A tier that can't be served stays on Claude, which the gateway refuses with a reason.
        crate::retarget(&mut routes, &cfg.tiers, &cfg.providers, &cfg.claude_models, true);
        let started = crate::start_proxy(&cfg.providers, (cfg.fail_on_limit, cfg.no_paid_fallback, cfg.compress_tool_results), routes, PathBuf::new(), true)?;
        *px = Some(started);
    }
    let px = px.clone().expect("started above");
    let http: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).timeout_connect(Some(Duration::from_secs(5))).build().into();
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((s, _)) => {
                    let (px, http) = (px.clone(), http.clone());
                    std::thread::spawn(move || {
                        let _ = s.set_nonblocking(false);
                        serve(s, &px, &http, &crate::usage::read_config());
                    });
                }
                // ponytail: polled, so a new connection waits up to 50 ms; fine for a local API.
                Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    });
    Ok(())
}

fn fail(s: &mut TcpStream, openai: bool, f: Failure) {
    if openai {
        respond(s, f.status, &json!({ "error": { "message": f.message, "type": error_kind(f.status), "code": f.status } }), "");
    } else {
        respond_error(s, &f);
    }
}

fn failure(status: u16, message: String) -> Failure {
    Failure { status, message, retry_after: None }
}

fn serve(mut s: TcpStream, px: &proxy::Proxy, http: &ureq::Agent, cfg: &Config) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(120)));
    let req = match read_request(&s) {
        Ok(r) => r,
        Err(e) => return fail(&mut s, false, failure(400, format!("bad request: {e}"))),
    };
    let path = req.path.split('?').next().unwrap_or("").to_string();
    let openai = path.ends_with("/chat/completions");
    // Claude Code probes connectivity before its first request.
    if path == "/api/hello" {
        return respond(&mut s, 200, &json!({}), "");
    }
    // Web pages are kept out: a page can post to localhost (or rebind its own name to it), and with any key let in it
    // would spend the user's plans. Apps send no `Origin`, and reach it by 127.0.0.1, localhost, or tailnet.
    // ponytail: no browser-based app can use the gateway; an allow-list of origins if one must.
    let host = req.header("host").unwrap_or("").rsplit_once(':').map_or(req.header("host").unwrap_or(""), |(h, _)| h);
    let allowed_host = matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "desktop-mpfdrrg")
        || host.starts_with("100.")
        || host.ends_with(".ts.net")
        || host.starts_with("192.168.")
        || host.starts_with("10.")
        || host.starts_with("172.");
    if req.header("origin").is_some() || !allowed_host {
        return fail(&mut s, openai, failure(403, "ccat gateway: only apps on this machine or tailnet (no browser pages)".into()));
    }
    if !cfg.gateway.on {
        return fail(&mut s, openai, failure(503, "ccat gateway is turned off (Settings → Gateway)".into()));
    }
    if !allowed(&cfg.gateway, &req) {
        return fail(&mut s, openai, failure(401, "ccat gateway: unknown API key (Settings → Gateway)".into()));
    }
    let names = names(cfg);
    let served = |t: usize| px.routes.read().unwrap().get(t).is_some_and(|r| r.upstream != Upstream::Official);
    match (req.method.as_str(), path.as_str()) {
        ("GET", "/v1/models" | "/models") => {
            let mut data: Vec<Value> = Vec::new();
            for (t, n) in names.iter().enumerate().filter(|(t, _)| served(*t)) {
                if !data.iter().any(|d| d["id"] == *n) {
                    data.push(json!({ "id": n, "object": "model", "type": "model", "display_name": n, "created": 0, "created_at": "1970-01-01T00:00:00Z", "owned_by": "ccat", "tier": TIERS[t].1 }));
                }
            }
            let (first, last) = (data.first().map(|d| d["id"].clone()), data.last().map(|d| d["id"].clone()));
            respond(&mut s, 200, &json!({ "object": "list", "data": data, "has_more": false, "first_id": first, "last_id": last }), "");
        }
        ("POST", "/v1/messages" | "/v1/messages/count_tokens" | "/v1/chat/completions" | "/chat/completions") => {
            let body: Value = match serde_json::from_slice(&req.body) {
                Ok(v) => v,
                Err(e) => return fail(&mut s, openai, failure(400, format!("body is not JSON: {e}"))),
            };
            let asked = body["model"].as_str().unwrap_or("").to_string();
            let Some(t) = tier_of(&names, &asked) else {
                return fail(&mut s, openai, failure(404, format!("ccat gateway has no model \"{asked}\"; it serves {}", names.join(", "))));
            };
            if !served(t) {
                return fail(&mut s, openai, failure(400, format!("{} runs on Claude (or can't be served as set), which the gateway can't use: it has no Claude Code login. Pick another provider for it in ccat.", TIERS[t].0)));
            }
            // The tier by its suffix: the proxy's route for it, whatever the name (see `proxy::Ctx::resolve`).
            let model = format!("{}@{}", names[t], TIERS[t].1);
            if openai {
                chat(&mut s, px, http, &body, &model, &asked);
            } else {
                let mut body = body;
                body["model"] = model.into();
                // The proxy's token replaces whatever key the app sent.
                let _ = forward(&mut s, http, &req, &px.url, Some(&px.token), body.to_string().into_bytes(), None, Fail::Never);
            }
        }
        _ => fail(&mut s, openai, failure(404, format!("ccat gateway has no {} {}", req.method, req.path))),
    }
}

// ---------------------------------------------------------------- Chat Completions ⇄ Anthropic Messages

/// The text of a Chat Completions `content`: a string, or the text parts of an array.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// A user message's `content` as Anthropic blocks: text and images (data URLs inline, others by URL).
fn user_blocks(content: &Value) -> Vec<Value> {
    let parts = match content {
        Value::Array(parts) => parts.clone(),
        other => vec![json!({ "type": "text", "text": text_of(other) })],
    };
    parts
        .iter()
        .filter_map(|p| match p["type"].as_str()? {
            "text" => p["text"].as_str().filter(|t| !t.is_empty()).map(|t| json!({ "type": "text", "text": t })),
            "image_url" => {
                let url = p["image_url"]["url"].as_str().or(p["image_url"].as_str())?;
                let source = match url.strip_prefix("data:").and_then(|d| d.split_once(";base64,")) {
                    Some((media, data)) => json!({ "type": "base64", "media_type": media, "data": data }),
                    None => json!({ "type": "url", "url": url }),
                };
                Some(json!({ "type": "image", "source": source }))
            }
            _ => None,
        })
        .collect()
}

/// A Chat Completions request as Anthropic Messages, for the model `model`.
fn to_messages(o: &Value, model: &str, stream: bool) -> Value {
    let mut system: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    // Neighbours of one role become one message (tool results after a call, then the user's text).
    let mut push = |role: &str, blocks: Vec<Value>| {
        if blocks.is_empty() {
            return;
        }
        match messages.last_mut() {
            Some(m) if m["role"] == role => m["content"].as_array_mut().unwrap().extend(blocks),
            _ => messages.push(json!({ "role": role, "content": blocks })),
        }
    };
    for m in o["messages"].as_array().into_iter().flatten() {
        match m["role"].as_str().unwrap_or("") {
            "system" | "developer" => system.push(text_of(&m["content"])),
            "assistant" => {
                let mut blocks: Vec<Value> = Some(text_of(&m["content"])).filter(|t| !t.is_empty()).map(|t| json!({ "type": "text", "text": t })).into_iter().collect();
                for c in m["tool_calls"].as_array().into_iter().flatten() {
                    let args = c["function"]["arguments"].as_str().unwrap_or("");
                    let input = serde_json::from_str::<Value>(args).ok().filter(Value::is_object).unwrap_or_else(|| json!({}));
                    blocks.push(json!({ "type": "tool_use", "id": c["id"], "name": c["function"]["name"], "input": input }));
                }
                push("assistant", blocks);
            }
            "tool" => push("user", vec![json!({ "type": "tool_result", "tool_use_id": m["tool_call_id"], "content": text_of(&m["content"]) })]),
            _ => push("user", user_blocks(&m["content"])),
        }
    }
    let max = o["max_completion_tokens"].as_u64().or(o["max_tokens"].as_u64()).unwrap_or(MAX_TOKENS);
    let mut body = json!({ "model": model, "max_tokens": max, "messages": messages, "stream": stream });
    let system: Vec<String> = system.into_iter().filter(|s| !s.is_empty()).collect();
    if !system.is_empty() {
        body["system"] = system.join("\n\n").into();
    }
    for k in ["temperature", "top_p"] {
        if o[k].is_number() {
            body[k] = o[k].clone();
        }
    }
    match &o["stop"] {
        Value::String(s) => body["stop_sequences"] = json!([s]),
        Value::Array(a) if !a.is_empty() => body["stop_sequences"] = json!(a),
        _ => {}
    }
    let tools: Vec<Value> = o["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["type"] == "function")
        .map(|t| {
            let f = &t["function"];
            let schema = if f["parameters"].is_object() { f["parameters"].clone() } else { json!({ "type": "object", "properties": {} }) };
            json!({ "name": f["name"], "description": f["description"].as_str().unwrap_or(""), "input_schema": schema })
        })
        .collect();
    if !tools.is_empty() {
        let choice = match &o["tool_choice"] {
            Value::String(c) if c == "none" => Some(json!({ "type": "none" })),
            Value::String(c) if c == "required" => Some(json!({ "type": "any" })),
            Value::Object(c) => c.get("function").and_then(|f| f["name"].as_str()).map(|n| json!({ "type": "tool", "name": n })),
            _ => None,
        };
        body["tools"] = tools.into();
        if let Some(c) = choice {
            body["tool_choice"] = c;
        }
    }
    body
}

fn finish_reason(stop: &Value) -> &'static str {
    match stop.as_str().unwrap_or("") {
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        _ => "stop",
    }
}

/// Anthropic usage as OpenAI's: the prompt counts cached tokens too.
fn usage_of(u: &Value) -> Value {
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    let prompt = n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens");
    let out = n("output_tokens");
    json!({ "prompt_tokens": prompt, "completion_tokens": out, "total_tokens": prompt + out, "prompt_tokens_details": { "cached_tokens": n("cache_read_input_tokens") } })
}

/// A finished Anthropic message as a `chat.completion`.
fn completion(m: &Value, model: &str) -> Value {
    let blocks = m["content"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let join = |kind: &str, field: &str| blocks.iter().filter(|b| b["type"] == kind).filter_map(|b| b[field].as_str()).collect::<String>();
    let (text, reasoning) = (join("text", "text"), join("thinking", "thinking"));
    let calls: Vec<Value> = blocks
        .iter()
        .filter(|b| b["type"] == "tool_use")
        .map(|b| json!({ "id": b["id"], "type": "function", "function": { "name": b["name"], "arguments": b["input"].to_string() } }))
        .collect();
    let mut msg = json!({ "role": "assistant", "content": if text.is_empty() && !calls.is_empty() { Value::Null } else { text.into() } });
    if !reasoning.is_empty() {
        msg["reasoning_content"] = reasoning.into();
    }
    if !calls.is_empty() {
        msg["tool_calls"] = calls.into();
    }
    json!({
        "id": format!("chatcmpl-{}", m["id"].as_str().unwrap_or("")), "object": "chat.completion", "created": crate::now(), "model": model,
        "choices": [{ "index": 0, "message": msg, "finish_reason": finish_reason(&m["stop_reason"]) }], "usage": usage_of(&m["usage"]),
    })
}

/// An Anthropic event stream turned into `chat.completion.chunk`s, one event at a time.
struct Chunks {
    id: String,
    model: String,
    created: u64,
    /// Content block index → tool call index.
    tools: HashMap<u64, usize>,
    finish: &'static str,
    /// The latest of each usage count (Anthropic's are running totals).
    usage: Map<String, Value>,
    include_usage: bool,
    done: bool,
}

impl Chunks {
    fn new(model: &str, include_usage: bool) -> Self {
        Self { id: format!("chatcmpl-{}", common::random_hex(12)), model: model.into(), created: crate::now(), tools: HashMap::new(), finish: "stop", usage: Map::new(), include_usage, done: false }
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({ "id": self.id, "object": "chat.completion.chunk", "created": self.created, "model": self.model, "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] })
    }

    fn add_usage(&mut self, u: &Value) {
        for (k, v) in u.as_object().into_iter().flatten().filter(|(_, v)| v.is_u64()) {
            self.usage.insert(k.clone(), v.clone());
        }
    }

    fn event(&mut self, ev: &Value) -> Vec<Value> {
        let delta = &ev["delta"];
        match ev["type"].as_str().unwrap_or("") {
            "message_start" => {
                if let Some(id) = ev["message"]["id"].as_str() {
                    self.id = format!("chatcmpl-{id}");
                }
                self.add_usage(&ev["message"]["usage"]);
                vec![self.chunk(json!({ "role": "assistant", "content": "" }), None)]
            }
            "content_block_start" => {
                let b = &ev["content_block"];
                match b["type"].as_str() {
                    Some("tool_use") => {
                        let k = self.tools.len();
                        self.tools.insert(ev["index"].as_u64().unwrap_or(0), k);
                        vec![self.chunk(json!({ "tool_calls": [{ "index": k, "id": b["id"], "type": "function", "function": { "name": b["name"], "arguments": "" } }] }), None)]
                    }
                    Some("text") if b["text"].as_str().is_some_and(|t| !t.is_empty()) => vec![self.chunk(json!({ "content": b["text"] }), None)],
                    _ => Vec::new(),
                }
            }
            "content_block_delta" => match delta["type"].as_str().unwrap_or("") {
                "text_delta" => vec![self.chunk(json!({ "content": delta["text"] }), None)],
                "thinking_delta" => vec![self.chunk(json!({ "reasoning_content": delta["thinking"] }), None)],
                "input_json_delta" => match self.tools.get(&ev["index"].as_u64().unwrap_or(0)) {
                    Some(k) => vec![self.chunk(json!({ "tool_calls": [{ "index": k, "function": { "arguments": delta["partial_json"] } }] }), None)],
                    None => Vec::new(),
                },
                _ => Vec::new(),
            },
            "message_delta" => {
                self.finish = finish_reason(&delta["stop_reason"]);
                self.add_usage(&ev["usage"]);
                Vec::new()
            }
            "message_stop" => self.finish(),
            "error" => {
                self.done = true;
                vec![json!({ "error": { "message": ev["error"]["message"], "type": ev["error"]["type"] } })]
            }
            _ => Vec::new(),
        }
    }

    /// The closing chunk (and the usage one, when asked for); nothing once done.
    fn finish(&mut self) -> Vec<Value> {
        if std::mem::replace(&mut self.done, true) {
            return Vec::new();
        }
        let mut out = vec![self.chunk(json!({}), Some(self.finish))];
        if self.include_usage {
            out.push(json!({ "id": self.id, "object": "chat.completion.chunk", "created": self.created, "model": self.model, "choices": [], "usage": usage_of(&Value::Object(self.usage.clone())) }));
        }
        out
    }
}

/// A Chat Completions request: run as Anthropic Messages on the proxy, answered in OpenAI's shape. `shown` is the
/// name the app asked for, which the answer carries.
fn chat(s: &mut TcpStream, px: &proxy::Proxy, http: &ureq::Agent, o: &Value, model: &str, shown: &str) {
    let stream = o["stream"].as_bool().unwrap_or(false);
    let body = to_messages(o, model, stream);
    let sent = http.post(format!("{}/v1/messages", px.url)).header("x-api-key", &px.token).header("content-type", "application/json").send(body.to_string());
    let mut res = match sent {
        Ok(r) => r,
        Err(e) => return fail(s, true, failure(502, format!("ccat gateway: {e}"))),
    };
    let status = res.status().as_u16();
    if status != 200 || !stream {
        let text = res.body_mut().with_config().limit(64 << 20).read_to_string().unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        return match status {
            200 => respond(s, 200, &completion(&v, shown), ""),
            _ => fail(s, true, failure(status, v["error"]["message"].as_str().map_or(text.clone(), String::from))),
        };
    }
    if s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n").is_err() {
        return;
    }
    let mut chunks = Chunks::new(shown, o["stream_options"]["include_usage"].as_bool().unwrap_or(false));
    let send = |s: &mut TcpStream, out: Vec<Value>| out.iter().all(|c| s.write_all(format!("data: {c}\n\n").as_bytes()).is_ok());
    for line in BufReader::new(res.into_body().into_reader()).lines() {
        let Ok(line) = line else { break };
        let Some(ev) = line.strip_prefix("data:").and_then(|d| serde_json::from_str::<Value>(d.trim()).ok()) else { continue };
        if !send(s, chunks.event(&ev)) {
            return; // the app hung up
        }
    }
    let rest = chunks.finish();
    if send(s, rest) {
        let _ = s.write_all(b"data: [DONE]\n\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(models: [&str; 4]) -> Config {
        let mut cfg = Config::default();
        for (t, m) in models.iter().enumerate() {
            cfg.tiers[t].model = m.to_string();
            cfg.tiers[t].provider = if m.is_empty() { String::new() } else { "OR".into() };
        }
        cfg
    }

    #[test]
    fn tiers_show_under_their_model_id_or_the_name_set() {
        let mut cfg = cfg_with(["", "anthropic/gpt-x[1m]", "gemini-3.8-flash-high", ""]);
        cfg.gateway.names[3] = " fast ".into();
        let n = names(&cfg);
        assert_eq!(n, ["fable", "gpt-x", "gemini-3.8-flash-high", "fast"]);
        assert_eq!(tier_of(&n, "gemini-3.8-flash-high"), Some(2));
        assert_eq!(tier_of(&n, "FAST"), Some(3), "names match whatever the case");
        assert_eq!(tier_of(&n, "haiku"), Some(3), "a tier's key always works");
        assert_eq!(tier_of(&n, "whatever@opus"), Some(1));
        assert_eq!(tier_of(&n, "claude-sonnet-5-5[1m]"), Some(2), "Claude Code's own names");
        assert_eq!(tier_of(&n, "gpt-4o"), None);
    }

    #[test]
    fn chat_requests_become_anthropic_messages() {
        let o = json!({ "model": "m", "max_tokens": 50, "temperature": 0.2, "stop": "END", "messages": [
            { "role": "system", "content": "Be brief." },
            { "role": "developer", "content": [{ "type": "text", "text": "Use tools." }] },
            { "role": "user", "content": [{ "type": "text", "text": "look" }, { "type": "image_url", "image_url": { "url": "data:image/png;base64,QUJD" } }] },
            { "role": "assistant", "content": null, "tool_calls": [{ "id": "c1", "type": "function", "function": { "name": "read", "arguments": "{\"path\":\"a\"}" } }] },
            { "role": "tool", "tool_call_id": "c1", "content": "file a" },
            { "role": "user", "content": "thanks" } ],
            "tools": [{ "type": "function", "function": { "name": "read", "description": "Reads", "parameters": { "type": "object", "properties": { "path": { "type": "string" } } } } }],
            "tool_choice": "required" });
        let a = to_messages(&o, "m@opus", true);
        assert_eq!(a["system"], "Be brief.\n\nUse tools.");
        assert_eq!((&a["max_tokens"], &a["temperature"], &a["stop_sequences"], &a["stream"]), (&json!(50), &json!(0.2), &json!(["END"]), &json!(true)));
        let m = a["messages"].as_array().unwrap();
        assert_eq!(m.len(), 3, "the tool result and the next user text share a message");
        assert_eq!(m[0]["content"][1]["source"], json!({ "type": "base64", "media_type": "image/png", "data": "QUJD" }));
        assert_eq!(m[1]["content"], json!([{ "type": "tool_use", "id": "c1", "name": "read", "input": { "path": "a" } }]));
        assert_eq!(m[2]["content"][0], json!({ "type": "tool_result", "tool_use_id": "c1", "content": "file a" }));
        assert_eq!(m[2]["content"][1]["text"], "thanks");
        assert_eq!(a["tools"][0]["input_schema"]["properties"]["path"]["type"], "string");
        assert_eq!(a["tool_choice"], json!({ "type": "any" }));
        let bare = to_messages(&json!({ "messages": [{ "role": "user", "content": "hi" }] }), "m", false);
        assert_eq!(bare["max_tokens"], MAX_TOKENS, "Anthropic needs one");
        assert!(bare.get("system").is_none() && bare.get("tools").is_none());
    }

    #[test]
    fn anthropic_answers_become_chat_completions() {
        let m = json!({ "id": "msg_1", "stop_reason": "tool_use", "usage": { "input_tokens": 10, "cache_read_input_tokens": 90, "output_tokens": 5 },
            "content": [{ "type": "thinking", "thinking": "hmm" }, { "type": "tool_use", "id": "t1", "name": "read", "input": { "path": "a" } }] });
        let c = completion(&m, "shown");
        assert_eq!((&c["model"], &c["choices"][0]["finish_reason"]), (&json!("shown"), &json!("tool_calls")));
        let msg = &c["choices"][0]["message"];
        assert_eq!((&msg["content"], &msg["reasoning_content"]), (&Value::Null, &json!("hmm")));
        assert_eq!(msg["tool_calls"][0]["function"], json!({ "name": "read", "arguments": "{\"path\":\"a\"}" }));
        assert_eq!((&c["usage"]["prompt_tokens"], &c["usage"]["total_tokens"], &c["usage"]["prompt_tokens_details"]["cached_tokens"]), (&json!(100), &json!(105), &json!(90)));

        let mut ch = Chunks::new("shown", true);
        let events = [
            json!({ "type": "message_start", "message": { "id": "msg_2", "usage": { "input_tokens": 7, "output_tokens": 1 } } }),
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hi" } }),
            json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "t2", "name": "ls", "input": {} } }),
            json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{\"d\":1}" } }),
            json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 9 } }),
            json!({ "type": "message_stop" }),
        ];
        let out: Vec<Value> = events.iter().flat_map(|e| ch.event(e)).collect();
        assert_eq!(out[0]["id"], "chatcmpl-msg_2");
        assert_eq!(out[1]["choices"][0]["delta"]["content"], "Hi");
        assert_eq!(out[2]["choices"][0]["delta"]["tool_calls"][0], json!({ "index": 0, "id": "t2", "type": "function", "function": { "name": "ls", "arguments": "" } }));
        assert_eq!(out[3]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "{\"d\":1}");
        assert_eq!(out[4]["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!((&out[5]["usage"]["prompt_tokens"], &out[5]["usage"]["completion_tokens"]), (&json!(7), &json!(9)), "the latest running totals");
        assert!(ch.finish().is_empty(), "closed once");
    }

    /// A fake Anthropic-compatible endpoint: a streamed or whole answer with a tool call, and what reached it.
    fn fake_endpoint() -> (String, Arc<std::sync::Mutex<Vec<(Value, String)>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let req = read_request(&s).unwrap();
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                let auth = format!("{} {}", req.header("x-api-key").unwrap_or("-"), req.header("authorization").unwrap_or("-"));
                log.lock().unwrap().push((body.clone(), auth));
                let (kind, text) = if body["stream"] == true {
                    let ev = |v: Value| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap());
                    let text = [
                        json!({ "type": "message_start", "message": { "id": "msg_s", "usage": { "input_tokens": 3 } } }),
                        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "t1", "name": "read", "input": {} } }),
                        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"path\":\"x\"}" } }),
                        json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 4 } }),
                        json!({ "type": "message_stop" }),
                    ].map(ev).concat();
                    ("text/event-stream", text)
                } else {
                    ("application/json", json!({ "id": "msg_j", "type": "message", "role": "assistant", "stop_reason": "end_turn", "usage": { "input_tokens": 3, "output_tokens": 1 }, "content": [{ "type": "text", "text": "pong" }] }).to_string())
                };
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\r\n{text}", text.len());
            }
        });
        (url, seen)
    }

    #[test]
    fn gateway_serves_both_dialects_with_its_own_keys() {
        let (url, seen) = fake_endpoint();
        let up = Upstream::Endpoint { name: "OR".into(), url, key: "endpoint-key".into() };
        let mut routes: Vec<proxy::Route> = (0..4).map(|t| proxy::Route { tier: TIERS[t].1, names: Vec::new(), upstream: Upstream::Official, model: String::new(), fallbacks: Vec::new() }).collect();
        routes[2] = proxy::Route { upstream: up, model: "vendor/m".into(), ..routes[2].clone() };
        let px = proxy::start(Vec::new(), Vec::new(), Vec::new(), routes, PathBuf::new()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        // Read for each request, as the real one reads the config.
        let shared = Arc::new(std::sync::Mutex::new(Gateway { on: true, keys: vec!["sk-ccat-good".into()], ..Default::default() }));
        let (gpx, gate) = (px.clone(), shared.clone());
        std::thread::spawn(move || {
            let http: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
            for s in listener.incoming().flatten() {
                let mut cfg = cfg_with(["", "", "vendor/m", ""]);
                cfg.gateway = gate.lock().unwrap().clone();
                serve(s, &gpx, &http, &cfg);
            }
        });
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
        let post = |path: &str, key: &str, body: Value| {
            let mut r = agent.post(format!("{base}{path}")).header("authorization", format!("Bearer {key}")).send(body.to_string()).unwrap();
            (r.status().as_u16(), r.body_mut().read_to_string().unwrap())
        };
        let hi = |model: &str, stream: bool| json!({ "model": model, "stream": stream, "messages": [{ "role": "user", "content": "ping" }] });

        assert_eq!(post("/v1/chat/completions", "sk-ccat-bad", hi("vendor/m", false)).0, 401);
        let (status, text) = post("/v1/chat/completions", "sk-ccat-good", hi("vendor/m", false));
        assert_eq!(status, 200, "{text}");
        let c: Value = serde_json::from_str(&text).unwrap();
        assert_eq!((&c["choices"][0]["message"]["content"], &c["model"]), (&json!("pong"), &json!("vendor/m")));
        let (body, auth) = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!(body["model"], "vendor/m");
        assert_eq!(auth, "endpoint-key Bearer endpoint-key", "the endpoint gets its own key; the app's key stays in the gateway");

        let (status, text) = post("/v1/chat/completions", "sk-ccat-good", hi("sonnet", true));
        assert_eq!(status, 200, "{text}");
        assert!(text.contains(r#""tool_calls":[{"index":0,"id":"t1","type":"function","function":{"name":"read","arguments":""}}]"#), "{text}");
        assert!(text.contains(r#""finish_reason":"tool_calls""#) && text.trim_end().ends_with("data: [DONE]"), "{text}");

        let anthropic = json!({ "model": "vendor/m", "max_tokens": 9, "messages": [{ "role": "user", "content": "ping" }] });
        let mut r = agent.post(format!("{base}/v1/messages")).header("x-api-key", "sk-ccat-good").header("anthropic-version", "2023-06-01").send(anthropic.to_string()).unwrap();
        assert_eq!(r.status().as_u16(), 200);
        assert!(r.body_mut().read_to_string().unwrap().contains("pong"));
        assert!(!seen.lock().unwrap().iter().any(|(_, a)| a.contains("sk-ccat")), "no gateway key ever reaches the endpoint");

        assert_eq!(post("/v1/chat/completions", "sk-ccat-good", hi("gpt-4o", false)).0, 404, "unknown names are refused, never sent on");
        let (status, text) = post("/v1/chat/completions", "sk-ccat-good", hi("opus", false));
        assert_eq!(status, 400, "a Claude tier has no login here: {text}");

        let mut r = agent.get(format!("{base}/v1/models")).header("x-api-key", "sk-ccat-good").call().unwrap();
        let models: Value = serde_json::from_str(&r.body_mut().read_to_string().unwrap()).unwrap();
        assert_eq!(models["data"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect::<Vec<_>>(), ["vendor/m"], "only the tiers it serves");

        let page = agent.post(format!("{base}/v1/chat/completions")).header("authorization", "Bearer sk-ccat-good").header("origin", "https://evil.example").send(hi("vendor/m", false).to_string()).unwrap();
        assert_eq!(page.status().as_u16(), 403, "browser pages are kept out");
        let rebound = agent.post(format!("{base}/v1/chat/completions")).header("host", "evil.example").header("authorization", "Bearer sk-ccat-good").send(hi("vendor/m", false).to_string()).unwrap();
        assert_eq!(rebound.status().as_u16(), 403, "a rebound name too");

        shared.lock().unwrap().any_key = true;
        assert_eq!(post("/v1/chat/completions", "anything", hi("vendor/m", false)).0, 200, "any key");
        shared.lock().unwrap().on = false;
        assert_eq!(post("/v1/chat/completions", "sk-ccat-good", hi("vendor/m", false)).0, 503);
    }
}
