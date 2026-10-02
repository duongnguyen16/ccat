//! A local Anthropic Messages endpoint for Claude Code, served from Google Antigravity accounts.
//! Claude Code → `POST /v1/messages` → Cloud Code Assist `streamGenerateContent` (Gemini wire) → Anthropic SSE.
//! Also serves `/mcp` (see `mcp`) for Antigravity abilities Claude Code has no built-in tool for.
//! Binds 127.0.0.1 on a random port and only answers requests carrying the per-launch token.

use crate::antigravity::{self, family, Account, Failure, Pool};
use serde_json::{json, Map, Value};
use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
    io::{self, BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const MAX_BODY: usize = 64 << 20;
/// System-prompt lines Cloud Code refuses with a fake quota 429 ("Resource has been exhausted"): Claude Code's
/// per-request billing header and the Agent SDK identity line. Neither carries anything a model needs.
const FILTERED_SYSTEM_LINES: [&str; 2] = ["x-anthropic-billing-header:", "You are a Claude agent, built on Anthropic's Claude Agent SDK."];
/// Cloud Code rejects a Gemini 3 turn whose first function call is unsigned; this documented sentinel stands in.
const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";
/// `labels.model_enum` the real client tags Gemini requests with (telemetry only).
const MODEL_ENUM: [(&str, &str); 5] = [
    ("gemini-3.5-flash-extra-low", "MODEL_PLACEHOLDER_M187"),
    ("gemini-3.5-flash-low", "MODEL_PLACEHOLDER_M20"),
    ("gemini-3-flash-agent", "MODEL_PLACEHOLDER_M132"),
    ("gemini-3.1-pro-low", "MODEL_PLACEHOLDER_M36"),
    ("gemini-pro-agent", "MODEL_PLACEHOLDER_M16"),
];
/// Google Search grounding only runs on Gemini; a web search asked of a Claude/GPT route goes here instead.
/// (Gemini 3.5 Flash was retired in Oct 2026: it now answers with a "switch models" notice and no finish reason.)
const SEARCH_MODEL: &str = "gemini-3.8-flash-low";

pub struct Proxy {
    pub url: String,
    pub token: String,
}

struct Ctx {
    pool: Pool,
    token: String,
    agent_id: String,
    trajectory: String,
    step: AtomicU64,
    /// Gemini puts thought signatures on function calls, which Anthropic tool_use blocks can't carry: tool id → tagged signature.
    sigs: Mutex<HashMap<String, String>>,
    /// Spoofed Claude name → Antigravity model. With aliases set, any other name falls back to the default alias's model.
    aliases: HashMap<String, String>,
    fallback: Option<String>,
    /// The folder Claude Code runs in: MCP tools resolve relative paths against it.
    folder: PathBuf,
    /// Tokens per text byte of an Anthropic request, learned per model family from Google's real prompt counts.
    ratio: Mutex<HashMap<&'static str, f64>>,
}

/// Tokens one image costs. ponytail: a flat middle-of-the-road figure (Gemini 3 default resolution ≈1120, a
/// ~1000px Claude image ≈1300); size images properly if `/context` ever needs to be exact for screenshots.
const IMAGE_TOKENS: u64 = 1100;
/// Before the first real response: the classic bytes/4.
const DEFAULT_RATIO: f64 = 0.25;

/// Text bytes and image count of an Anthropic request: base64 media (`data`) would swamp a bytes-per-token ratio.
fn weigh(v: &Value) -> (u64, u64) {
    match v {
        Value::Object(m) => m.iter().fold((0, 0), |(b, i), (k, v)| match (k.as_str(), v) {
            ("data", Value::String(_)) => (b, i + 1),
            _ => {
                let (vb, vi) = weigh(v);
                (b + k.len() as u64 + vb, i + vi)
            }
        }),
        Value::Array(a) => a.iter().map(weigh).fold((0, 0), |(b, i), (vb, vi)| (b + vb, i + vi)),
        Value::String(s) => (s.len() as u64, 0),
        other => (other.to_string().len() as u64, 0),
    }
}

impl Ctx {
    /// Remembers how a real request of `fam` tokenized; a moving average so one odd turn doesn't swing it.
    fn learn(&self, fam: &'static str, req: &Value, prompt_tokens: u64) {
        let (bytes, images) = weigh(req);
        if bytes == 0 || prompt_tokens == 0 {
            return;
        }
        let r = prompt_tokens.saturating_sub(images * IMAGE_TOKENS) as f64 / bytes as f64;
        let mut g = self.ratio.lock().unwrap();
        let next = g.get(fam).map_or(r, |old| old * 0.7 + r * 0.3);
        g.insert(fam, next);
    }

    fn estimate(&self, fam: &str, req: &Value) -> u64 {
        let (bytes, images) = weigh(req);
        let r = self.ratio.lock().unwrap().get(fam).copied().unwrap_or(DEFAULT_RATIO);
        (bytes as f64 * r).round() as u64 + images * IMAGE_TOKENS
    }
}

/// `aliases` maps the Claude names Claude Code was told about to real Antigravity models; `fallback` catches names it
/// picks on its own (background tasks). Both empty means names pass through unchanged.
pub fn start(accounts: Vec<Account>, aliases: HashMap<String, String>, fallback: Option<String>, folder: PathBuf) -> io::Result<Proxy> {
    serve_pool(Pool::new(accounts), aliases, fallback, folder)
}

fn serve_pool(pool: Pool, aliases: HashMap<String, String>, fallback: Option<String>, folder: PathBuf) -> io::Result<Proxy> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let url = format!("http://{}", listener.local_addr()?);
    let token = antigravity::random_hex(24);
    let ctx = Arc::new(Ctx {
        pool,
        token: token.clone(),
        agent_id: antigravity::uuid(),
        trajectory: antigravity::uuid(),
        step: AtomicU64::new(0),
        sigs: Mutex::new(HashMap::new()),
        aliases,
        fallback,
        folder,
        ratio: Mutex::new(HashMap::new()),
    });
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let ctx = ctx.clone();
            std::thread::spawn(move || serve(conn, &ctx));
        }
    });
    Ok(Proxy { url, token })
}

// ---------------------------------------------------------------- HTTP

struct Req {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn read_request(s: &TcpStream) -> io::Result<Req> {
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut it = line.split_whitespace();
    let (method, path) = (it.next().ok_or_else(|| bad("empty request"))?.to_string(), it.next().unwrap_or("/").to_string());
    let mut headers = Vec::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 || line.trim_end().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let mut req = Req { method, path, headers, body: Vec::new() };
    if req.header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        loop {
            line.clear();
            r.read_line(&mut line)?;
            let n = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16).map_err(|_| bad("bad chunk size"))?;
            if n == 0 {
                break;
            }
            if req.body.len() + n > MAX_BODY {
                return Err(bad("body too large"));
            }
            let at = req.body.len();
            req.body.resize(at + n, 0);
            r.read_exact(&mut req.body[at..])?;
            line.clear();
            r.read_line(&mut line)?;
        }
    } else if let Some(n) = req.header("content-length") {
        let n: usize = n.parse().map_err(|_| bad("bad content-length"))?;
        if n > MAX_BODY {
            return Err(bad("body too large"));
        }
        req.body.resize(n, 0);
        r.read_exact(&mut req.body)?;
    }
    Ok(req)
}

fn respond(s: &mut TcpStream, status: u16, body: &Value, extra: &str) {
    let body = body.to_string();
    let _ = write!(
        s,
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
        if status == 200 { "OK" } else { "Error" },
        body.len()
    );
}

fn error_kind(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        503 | 529 => "overloaded_error",
        _ => "api_error",
    }
}

fn respond_error(s: &mut TcpStream, f: &Failure) {
    let extra = f.retry_after.map(|r| format!("Retry-After: {r}\r\n")).unwrap_or_default();
    respond(s, f.status, &json!({ "type": "error", "error": { "type": error_kind(f.status), "message": f.message } }), &extra);
}

fn serve(mut s: TcpStream, ctx: &Ctx) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(120)));
    let req = match read_request(&s) {
        Ok(r) => r,
        Err(e) => return respond_error(&mut s, &Failure { status: 400, message: format!("bad request: {e}"), retry_after: None }),
    };
    // Claude Code probes connectivity with an unauthenticated `HEAD /api/hello` before its first request.
    if req.path == "/api/hello" {
        return respond(&mut s, 200, &json!({}), "");
    }
    let bearer = req.header("authorization").and_then(|a| a.strip_prefix("Bearer "));
    if req.header("x-api-key") != Some(&ctx.token) && bearer != Some(&ctx.token) {
        return respond_error(&mut s, &Failure { status: 401, message: "ccat proxy: wrong token".into(), retry_after: None });
    }
    match (req.method.as_str(), req.path.split('?').next().unwrap_or("")) {
        ("POST", "/v1/messages") => messages(s, ctx, &req.body),
        ("POST", "/v1/messages/count_tokens") => count_tokens(s, ctx, &req.body),
        ("POST", "/mcp") => match serde_json::from_slice::<Value>(&req.body) {
            Ok(msg) => match crate::mcp::handle(&ctx.pool, &ctx.folder, &msg) {
                Some(reply) => respond(&mut s, 200, &reply, ""),
                None => {
                    let _ = s.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                }
            },
            Err(e) => respond(&mut s, 400, &json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": e.to_string() } }), ""),
        },
        // No server-initiated stream and no sessions: the MCP spec's answer for both is 405.
        ("GET" | "DELETE", "/mcp") => respond_error(&mut s, &Failure { status: 405, message: "no SSE stream".into(), retry_after: None }),
        _ => {
            antigravity::log(&format!("404 {} {} (Claude Code asked for something the proxy doesn't serve)", req.method, req.path));
            respond_error(&mut s, &Failure { status: 404, message: format!("ccat proxy has no {} {}", req.method, req.path), retry_after: None })
        }
    }
}

/// The Antigravity model a request runs on. Claude Code may tag a model with a context hint such as `[1m]`.
fn resolve_model(ctx: &Ctx, req: &Value) -> (String, String) {
    let asked = req["model"].as_str().unwrap_or("").split('[').next().unwrap_or("").trim().to_string();
    let model = ctx.aliases.get(&asked).or(ctx.fallback.as_ref()).cloned().unwrap_or_else(|| asked.clone());
    (asked, model)
}

/// The `web_search_*` server tool, when it is the request's only tool — Claude Code's WebSearch sub-request.
/// ponytail: mixed with client tools it is still dropped; Google grounding and function calls don't combine reliably.
fn web_search(req: &Value) -> Option<&Value> {
    let tools = req["tools"].as_array()?;
    let search = tools.iter().find(|t| t["type"].as_str().is_some_and(|k| k.starts_with("web_search")))?;
    tools.iter().all(|t| t.get("input_schema").is_none()).then_some(search)
}

/// Gemini routes: Google counts exactly what `build` would send. Cloud Code's counter only takes `contents`, so the
/// translated system prompt and function declarations lead them as text. Everything else (Claude/GPT routes, a
/// failed count) gets the estimate calibrated on this session's real responses.
fn count_tokens(mut s: TcpStream, ctx: &Ctx, body: &[u8]) {
    let req: Value = serde_json::from_slice(body).unwrap_or_default();
    let (_, model) = resolve_model(ctx, &req);
    let fam = family(&model);
    let exact = (fam == "gemini").then(|| google_count(ctx, &req, &model)).flatten();
    respond(&mut s, 200, &json!({ "input_tokens": exact.unwrap_or_else(|| ctx.estimate(fam, &req)) }), "");
}

fn google_count(ctx: &Ctx, req: &Value, model: &str) -> Option<u64> {
    let mut lead = system_parts(req);
    let decls = declarations(req);
    if !decls.is_empty() {
        lead.push(json!({ "text": Value::Array(decls).to_string() }));
    }
    let mut parts = contents(req, "gemini", &ctx.sigs.lock().unwrap());
    if !lead.is_empty() {
        parts.insert(0, json!({ "role": "user", "parts": lead }));
    }
    if parts.is_empty() {
        return Some(0);
    }
    match ctx.pool.post("/v1internal:countTokens", &json!({ "request": { "model": format!("models/{model}"), "contents": parts } })) {
        Ok(v) => v["totalTokens"].as_u64(),
        Err(f) => {
            antigravity::log(&format!("countTokens failed ({} {}); using the calibrated estimate", f.status, f.message));
            None
        }
    }
}

fn messages(mut s: TcpStream, ctx: &Ctx, body: &[u8]) {
    let req: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return respond_error(&mut s, &Failure { status: 400, message: format!("body is not JSON: {e}"), retry_after: None }),
    };
    let (asked, model) = resolve_model(ctx, &req);
    if model.is_empty() {
        return respond_error(&mut s, &Failure { status: 400, message: "no model in request".into(), retry_after: None });
    }
    let model = if web_search(&req).is_some() && family(&model) != "gemini" { SEARCH_MODEL.to_string() } else { model };
    let stream = req["stream"].as_bool().unwrap_or(false);
    let (cca, beta) = build(&req, &model, ctx);
    let mut last = None;
    // Up to three tries while nothing has reached Claude Code: empty answers and in-band quota errors.
    for _ in 0..3 {
        let (i, reader) = match ctx.pool.stream(&model, &cca, beta) {
            Ok(x) => x,
            Err(f) => {
                last = Some(f);
                break;
            }
        };
        let mut tx = Tx::new(&model, &asked, stream.then_some(&mut s), &ctx.sigs);
        match tx.run(reader) {
            Ok(()) => {
                let msg = tx.message();
                let prompt = tx.usage.0;
                drop(tx);
                // Search answers carry Google's page text, which the request never had: they'd skew the ratio.
                if web_search(&req).is_none() {
                    ctx.learn(family(&model), &req, prompt);
                }
                if !stream {
                    respond(&mut s, 200, &msg, "");
                }
                return;
            }
            Err(Stop::Retry(f, block)) => {
                let kind = if web_search(&req).is_some() { " [web search]" } else { "" };
                // A client error won't heal on a retry.
                let fatal = (400..500).contains(&f.status) && f.status != 429;
                antigravity::log(&format!("{model}{kind}: {} after {} {}", if fatal { "giving up" } else { "retrying" }, f.status, f.message));
                if let Some(secs) = block {
                    ctx.pool.block(i, family(&model), secs);
                }
                last = Some(f);
                if fatal {
                    break;
                }
            }
            Err(Stop::Gone) => return,
        }
    }
    if let Some(f) = last {
        respond_error(&mut s, &f);
    }
}

// ---------------------------------------------------------------- Anthropic → Cloud Code

/// Signatures handed to Claude Code are tagged with the model family that made them, so a resumed session
/// on another model (or Anthropic's own signatures) never reaches Google as a "corrupted" signature.
fn tag(fam: &str, sig: &str) -> String {
    format!("ag:{fam}:{sig}")
}

fn untag<'a>(sig: Option<&'a str>, fam: &str) -> Option<&'a str> {
    let s = sig?.strip_prefix("ag:")?.strip_prefix(fam)?.strip_prefix(':')?;
    let base64 = !s.is_empty() && s.len() % 4 == 0 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b));
    base64.then_some(s)
}

/// Anthropic content: a string or a block array.
fn blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::String(t) => vec![json!({ "type": "text", "text": t })],
        Value::Array(a) => a.clone(),
        _ => vec![],
    }
}

fn media(b: &Value) -> Option<Value> {
    let src = &b["source"];
    (src["type"] == "base64").then(|| json!({ "inlineData": { "mimeType": src["media_type"], "data": src["data"] } }))
}

fn function_response(b: &Value, names: &HashMap<String, String>, fam: &str) -> Value {
    let id = b["tool_use_id"].as_str().unwrap_or("");
    let (mut text, mut images) = (Vec::new(), Vec::new());
    for c in blocks(&b["content"]) {
        match c["type"].as_str() {
            Some("text") => text.extend(c["text"].as_str().map(String::from)),
            Some("image") => images.extend(media(&c)),
            _ => {}
        }
    }
    let out = if !text.is_empty() { text.join("\n") } else if !images.is_empty() { "(see attached image)".into() } else { String::new() };
    let key = if b["is_error"] == true { "error" } else { "output" };
    let mut fr = json!({ "name": names.get(id).map_or("tool", String::as_str), "response": { key: out } });
    if !images.is_empty() {
        fr["parts"] = json!(images);
    }
    if fam != "gemini" {
        fr["id"] = json!(id);
    }
    json!({ "functionResponse": fr })
}

fn contents(req: &Value, fam: &str, sigs: &HashMap<String, String>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut names = HashMap::new();
    for m in req["messages"].as_array().into_iter().flatten() {
        let content = blocks(&m["content"]);
        if m["role"] == "assistant" {
            let mut parts = Vec::new();
            let mut first_call = true;
            for b in &content {
                match b["type"].as_str() {
                    Some("text") => parts.extend(b["text"].as_str().filter(|t| !t.trim().is_empty()).map(|t| json!({ "text": t }))),
                    // Unsigned or foreign thinking is dropped: Claude rejects it and Gemini has no use for it.
                    Some("thinking") => {
                        if let (Some(t), Some(sig)) = (b["thinking"].as_str().filter(|t| !t.trim().is_empty()), untag(b["signature"].as_str(), fam)) {
                            parts.push(json!({ "thought": true, "text": t, "thoughtSignature": sig }));
                        }
                    }
                    Some("tool_use") => {
                        let (id, name) = (b["id"].as_str().unwrap_or(""), b["name"].as_str().unwrap_or(""));
                        names.insert(id.to_string(), name.to_string());
                        let args = if b["input"].is_object() { b["input"].clone() } else { json!({}) };
                        let mut call = json!({ "name": name, "args": args });
                        if fam != "gemini" {
                            call["id"] = json!(id);
                        }
                        let mut part = json!({ "functionCall": call });
                        match sigs.get(id).and_then(|s| untag(Some(s), fam)) {
                            Some(sig) => part["thoughtSignature"] = json!(sig),
                            None if fam == "gemini" && first_call => part["thoughtSignature"] = json!(SKIP_SIGNATURE),
                            None => {}
                        }
                        first_call = false;
                        parts.push(part);
                    }
                    _ => {}
                }
            }
            if !parts.is_empty() {
                out.push(json!({ "role": "model", "parts": parts }));
            }
        } else {
            let (mut results, mut rest) = (Vec::new(), Vec::new());
            for b in &content {
                match b["type"].as_str() {
                    Some("tool_result") => results.push(function_response(b, &names, fam)),
                    Some("text") => rest.extend(b["text"].as_str().filter(|t| !t.trim().is_empty()).map(|t| json!({ "text": t }))),
                    Some("image") | Some("document") => rest.extend(media(b)),
                    _ => {}
                }
            }
            if !results.is_empty() {
                // Cloud Code wants every response to one turn's calls inside a single user turn.
                let merge = out.last().is_some_and(|c| c["role"] == "user" && c["parts"][0].get("functionResponse").is_some());
                match out.last_mut().and_then(|c| c["parts"].as_array_mut()).filter(|_| merge) {
                    Some(parts) => parts.extend(results),
                    None => out.push(json!({ "role": "user", "parts": results })),
                }
            }
            if !rest.is_empty() {
                out.push(json!({ "role": "user", "parts": rest }));
            }
        }
    }
    out
}

/// Stable per conversation: the real client derives it from the first user text.
fn session_id(req: &Value) -> String {
    let first = req["messages"].as_array().and_then(|m| m.iter().find(|m| m["role"] == "user"));
    let text = first.map(|m| blocks(&m["content"]).iter().filter_map(|b| b["text"].as_str().map(String::from)).collect::<String>()).unwrap_or_default();
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    format!("-{}", h.finish() & (u64::MAX >> 1))
}

/// The system prompt as Cloud Code `systemInstruction` parts, minus the lines it refuses.
fn system_parts(req: &Value) -> Vec<Value> {
    match &req["system"] {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str().map(String::from)).collect(),
        _ => vec![],
    }
    .into_iter()
    .map(|t| t.lines().filter(|l| !FILTERED_SYSTEM_LINES.iter().any(|f| l.trim_start().starts_with(f))).collect::<Vec<_>>().join("\n"))
    .filter(|t| !t.trim().is_empty())
    .map(|t| json!({ "text": t }))
    .collect()
}

/// Client tools as Cloud Code `functionDeclarations`. Server tools (web search…) have no input_schema and are skipped.
fn declarations(req: &Value) -> Vec<Value> {
    req["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t.get("input_schema").is_some())
        .map(|t| json!({ "name": t["name"], "description": t["description"].as_str().unwrap_or(""), "parameters": clean_schema(&t["input_schema"]) }))
        .collect()
}

/// The Cloud Code request (without `project`, which the pool fills per account) and whether the Claude thinking beta header applies.
fn build(req: &Value, model: &str, ctx: &Ctx) -> (Value, bool) {
    let fam = family(model);
    let claude = fam == "claude";
    let contents = contents(req, fam, &ctx.sigs.lock().unwrap());

    let mut system = system_parts(req);
    let search = web_search(req).filter(|_| fam == "gemini");
    // Google Search has no domain filter on this wire, so the model is asked to honour Claude Code's.
    for (key, rule) in [("allowed_domains", "Only use web results from these domains"), ("blocked_domains", "Never use web results from these domains")] {
        let list: Vec<&str> = search.and_then(|t| t[key].as_array()).into_iter().flatten().filter_map(Value::as_str).collect();
        if !list.is_empty() {
            system.push(json!({ "text": format!("{rule}: {}.", list.join(", ")) }));
        }
    }

    let decls = declarations(req);
    // Antigravity's own client sends VALIDATED; Claude routes always get it, even without tools.
    let choice = &req["tool_choice"];
    let tool_mode = if claude {
        Some(json!({ "mode": "VALIDATED" }))
    } else if decls.is_empty() {
        None
    } else {
        Some(match choice["type"].as_str() {
            Some("any") => json!({ "mode": "ANY" }),
            Some("tool") => json!({ "mode": "ANY", "allowedFunctionNames": [choice["name"]] }),
            Some("none") => json!({ "mode": "NONE" }),
            _ => json!({ "mode": "VALIDATED" }),
        })
    };

    // Claude on Cloud Code 400s above 64000 output tokens.
    let cap = if claude { 64_000 } else { 65_535 };
    let max = req["max_tokens"].as_u64().unwrap_or(cap).clamp(1, cap);
    let mut gen = json!({ "maxOutputTokens": max });
    for (from, to) in [("temperature", "temperature"), ("top_p", "topP"), ("top_k", "topK"), ("stop_sequences", "stopSequences")] {
        if let Some(v) = req.get(from).filter(|v| !v.is_null()) {
            gen[to] = v.clone();
        }
    }
    let budget = match req["thinking"]["type"].as_str() {
        Some("enabled") => req["thinking"]["budget_tokens"].as_u64(),
        Some("adaptive") => Some(match req["output_config"]["effort"].as_str() {
            Some("low") => 4_000,
            Some("medium") => 10_000,
            Some("max") => max,
            _ => 24_000,
        }),
        _ => None,
    }
    .filter(|_| max > 1024);
    if let Some(b) = budget {
        gen["thinkingConfig"] = json!({ "includeThoughts": true, "thinkingBudget": b.clamp(1024, max - 1) });
    }
    // Structured output (current `output_config.format` or the older `output_format` beta).
    // ponytail: Gemini only; Claude/GPT routes still answer, just without the schema enforced.
    let format = req["output_config"].get("format").filter(|f| f.is_object()).unwrap_or(&req["output_format"]);
    if fam == "gemini" && format["type"] == "json_schema" {
        gen["responseMimeType"] = json!("application/json");
        gen["responseSchema"] = clean_schema(&format["schema"]);
    }

    let step = ctx.step.fetch_add(1, Ordering::Relaxed) + 2;
    let mut labels = json!({
        "last_step_index": (step - 1).to_string(),
        "trajectory_id": ctx.trajectory,
        "used_claude": claude.to_string(),
        "used_claude_conservative": claude.to_string(),
    });
    if let Some((_, e)) = MODEL_ENUM.iter().find(|(m, _)| *m == model) {
        labels["model_enum"] = json!(e);
    }
    let mut request = json!({ "contents": contents, "generationConfig": gen, "sessionId": session_id(req), "labels": labels });
    if !system.is_empty() {
        request["systemInstruction"] = json!({ "role": "user", "parts": system });
    }
    if search.is_some() {
        request["tools"] = json!([{ "googleSearch": {} }]);
    } else if !decls.is_empty() {
        request["tools"] = json!([{ "functionDeclarations": decls }]);
    }
    if let Some(m) = tool_mode {
        request["toolConfig"] = json!({ "functionCallingConfig": m });
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let body = json!({
        "model": model,
        "userAgent": "antigravity",
        "requestType": "agent",
        "requestId": format!("agent/{}/{now}/{}/{step}", ctx.agent_id, ctx.trajectory),
        "request": request,
    });
    (body, claude && budget.is_some())
}

// ---------------------------------------------------------------- tool schema → Cloud Code `parameters`

/// Keywords that only narrow values: kept for the model as description text, dropped from the wire schema.
const SPILL: [&str; 16] = [
    "pattern", "format", "minLength", "maxLength", "minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum", "multipleOf", "minItems",
    "maxItems", "uniqueItems", "minProperties", "maxProperties", "default", "examples",
];

/// JSON Schema → the subset Cloud Code's `parameters` accepts for every model family: no type arrays, no `null`,
/// no `nullable`, no combiners, no refs — only type/description/enum/properties/required/items/title.
/// ponytail: local `$ref`s and simple unions only; oh-my-pi's 2.4k-line normalizer covers exotic schemas if one ever 400s.
fn clean_schema(schema: &Value) -> Value {
    let out = clean(schema, schema, 0).0;
    if out["type"] == "object" {
        out
    } else {
        json!({ "type": "object", "properties": {} })
    }
}

/// Follows local `$ref`s (`#/$defs/…`); a cycle stops after a few hops and reads as an empty schema.
fn resolve<'a>(root: &'a Value, mut v: &'a Value) -> &'a Value {
    for _ in 0..8 {
        match v["$ref"].as_str().and_then(|r| r.strip_prefix('#')) {
            Some(ptr) => v = root.pointer(ptr).unwrap_or(&Value::Null),
            None => return v,
        }
    }
    &Value::Null
}

/// Folds `anyOf`/`oneOf`/`allOf` into one schema. Returns the merged (still raw) schema and whether a `null` branch was seen.
fn collapse(o: &Map<String, Value>, root: &Value) -> (Value, bool) {
    let mut base: Map<String, Value> = o.iter().filter(|(k, _)| !["anyOf", "oneOf", "allOf"].contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    let mut nullable = false;
    let merge_objects = |base: &mut Map<String, Value>, branches: &[&Value], all: bool| {
        let mut props = base.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
        let mut required: Option<Vec<Value>> = None;
        for b in branches {
            props.extend(b["properties"].as_object().cloned().unwrap_or_default());
            let r = b["required"].as_array().cloned().unwrap_or_default();
            required = Some(match required {
                None => r,
                Some(prev) if all => prev.into_iter().chain(r).collect(),
                Some(prev) => prev.into_iter().filter(|x| r.contains(x)).collect(),
            });
        }
        let mut req: Vec<Value> = base.get("required").and_then(Value::as_array).cloned().unwrap_or_default();
        req.extend(required.unwrap_or_default());
        base.insert("type".into(), json!("object"));
        base.insert("properties".into(), Value::Object(props));
        base.insert("required".into(), json!(req));
    };
    if let Some(all) = o.get("allOf").and_then(Value::as_array) {
        let branches: Vec<&Value> = all.iter().map(|b| resolve(root, b)).collect();
        if branches.iter().all(|b| b["type"] == "object" || b.get("properties").is_some()) {
            merge_objects(&mut base, &branches, true);
        } else if let Some(first) = branches.first().and_then(|b| b.as_object()) {
            for (k, v) in first {
                base.entry(k.clone()).or_insert(v.clone());
            }
        }
    }
    for key in ["anyOf", "oneOf"] {
        let Some(list) = o.get(key).and_then(Value::as_array) else { continue };
        let branches: Vec<&Value> = list.iter().map(|b| resolve(root, b)).filter(|b| if b["type"] == "null" { nullable = true; false } else { true }).collect();
        if branches.len() > 1 && branches.iter().all(|b| b["type"] == "object" || b.get("properties").is_some()) {
            merge_objects(&mut base, &branches, false);
        } else if let Some(first) = branches.first().and_then(|b| b.as_object()) {
            // Mixed types can't be expressed: keep the first, which loses nothing for the common `T | null`.
            for (k, v) in first {
                base.entry(k.clone()).or_insert(v.clone());
            }
            if branches.len() > 1 {
                let kinds: Vec<String> = branches.iter().filter_map(|b| b["type"].as_str().map(String::from)).collect();
                if !kinds.is_empty() {
                    let d = base.get("description").and_then(Value::as_str).unwrap_or("").to_string();
                    base.insert("description".into(), json!(format!("{d} (one of: {})", kinds.join(", ")).trim().to_string()));
                }
            }
        }
    }
    (Value::Object(base), nullable)
}

/// Returns the cleaned schema and whether the value may be null (so the parent drops it from `required`).
fn clean(v: &Value, root: &Value, depth: u32) -> (Value, bool) {
    let v = resolve(root, v);
    let Some(o) = v.as_object().filter(|_| depth < 32) else { return (json!({}), false) };
    if ["anyOf", "oneOf", "allOf"].iter().any(|k| o.contains_key(*k)) {
        let (merged, nullable) = collapse(o, root);
        let (c, n) = clean(&merged, root, depth + 1);
        return (c, nullable || n);
    }
    let mut out = Map::new();
    let mut nullable = o.get("nullable") == Some(&json!(true));
    let mut spill = Vec::new();
    match o.get("type") {
        Some(Value::String(t)) if t == "null" => nullable = true,
        Some(Value::String(t)) => {
            out.insert("type".into(), json!(t));
        }
        Some(Value::Array(ts)) => {
            nullable |= ts.iter().any(|t| t == "null");
            let rest: Vec<&Value> = ts.iter().filter(|t| *t != "null").collect();
            if let Some(t) = rest.first() {
                out.insert("type".into(), (*t).clone());
            }
            if rest.len() > 1 {
                spill.push(format!("type: {}", rest.iter().filter_map(|t| t.as_str()).collect::<Vec<_>>().join(" or ")));
            }
        }
        _ => {}
    }
    for k in ["description", "title"] {
        if let Some(Value::String(s)) = o.get(k) {
            out.insert(k.into(), json!(s));
        }
    }
    let values = o.get("enum").cloned().or_else(|| o.get("const").map(|c| json!([c])));
    if let Some(Value::Array(vals)) = values {
        // Google rejects null and empty-string enum members.
        let vals: Vec<&Value> = vals.iter().filter(|x| !x.is_null() && x.as_str() != Some("")).collect();
        if !vals.is_empty() && vals.iter().all(|x| x.is_string()) {
            out.insert("enum".into(), json!(vals));
            out.entry("type").or_insert(json!("string"));
        } else if !vals.is_empty() {
            spill.push(format!("allowed: {}", vals.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", ")));
        }
    }
    if let Some(Value::Object(props)) = o.get("properties") {
        let mut clean_props = Map::new();
        let mut optional = Vec::new();
        for (k, p) in props {
            let (c, n) = clean(p, root, depth + 1);
            if n {
                optional.push(k.clone());
            }
            clean_props.insert(k.clone(), c);
        }
        let required: Vec<&Value> = o
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|r| r.as_str().is_some_and(|r| clean_props.contains_key(r) && !optional.iter().any(|o| o == r)))
            .collect();
        if !required.is_empty() {
            out.insert("required".into(), json!(required));
        }
        out.insert("properties".into(), Value::Object(clean_props));
        out.entry("type").or_insert(json!("object"));
    }
    if out.get("type") == Some(&json!("object")) {
        out.entry("properties").or_insert(json!({}));
    }
    match o.get("items") {
        Some(Value::Array(tuple)) => {
            out.insert("items".into(), tuple.first().map_or(json!({}), |t| clean(t, root, depth + 1).0));
        }
        Some(items) => {
            out.insert("items".into(), clean(items, root, depth + 1).0);
        }
        None => {}
    }
    // Google requires `items` on every array.
    if out.get("type") == Some(&json!("array")) {
        out.entry("items").or_insert(json!({}));
    }
    for k in SPILL {
        if let Some(x) = o.get(k) {
            spill.push(format!("{k}: {x}"));
        }
    }
    if !spill.is_empty() {
        let d = out.get("description").and_then(Value::as_str).unwrap_or("").to_string();
        out.insert("description".into(), json!(format!("{d} ({})", spill.join(", ")).trim().to_string()));
    }
    (Value::Object(out), nullable)
}

// ---------------------------------------------------------------- Cloud Code SSE → Anthropic

enum Stop {
    /// Nothing reached Claude Code yet: try again (optionally blocking the account for that many seconds).
    Retry(Failure, Option<u64>),
    /// The client hung up, or the error already went out as an SSE `error` event.
    Gone,
}

struct Tx<'a> {
    out: Option<&'a mut TcpStream>,
    sigs: &'a Mutex<HashMap<String, String>>,
    model: String,
    fam: &'static str,
    id: String,
    /// Some content was produced (and, when streaming, sent).
    started: bool,
    blocks: Vec<Value>,
    open: Option<&'static str>,
    /// Signature for the open thinking block, emitted as `signature_delta` when it closes.
    sig: String,
    /// Leading whitespace held back so an empty answer can still be retried.
    pending: String,
    usage: (u64, u64, u64),
    finish: Option<String>,
    finish_note: String,
    stop_reason: &'static str,
    /// Google Search grounding: the queries run and the pages cited, (url, title), replayed as Anthropic web-search blocks.
    queries: Vec<String>,
    links: Vec<(String, String)>,
}

impl<'a> Tx<'a> {
    /// `model` is what runs; replies name `shown`, the model Claude Code asked for.
    fn new(model: &str, shown: &str, out: Option<&'a mut TcpStream>, sigs: &'a Mutex<HashMap<String, String>>) -> Self {
        Tx {
            out,
            sigs,
            model: shown.to_string(),
            fam: family(model),
            id: format!("msg_{}", antigravity::random_hex(12)),
            started: false,
            blocks: Vec::new(),
            open: None,
            sig: String::new(),
            pending: String::new(),
            usage: (0, 0, 0),
            finish: None,
            finish_note: String::new(),
            stop_reason: "end_turn",
            queries: Vec::new(),
            links: Vec::new(),
        }
    }

    fn usage_json(&self) -> Value {
        let (prompt, cached, output) = self.usage;
        let mut u = json!({ "input_tokens": prompt - cached, "cache_read_input_tokens": cached, "cache_creation_input_tokens": 0, "output_tokens": output });
        if !self.queries.is_empty() {
            u["server_tool_use"] = json!({ "web_search_requests": self.queries.len() });
        }
        u
    }

    fn send(&mut self, name: &str, data: Value) -> Result<(), Stop> {
        let Some(s) = self.out.as_deref_mut() else { return Ok(()) };
        write!(s, "event: {name}\ndata: {data}\n\n").and_then(|_| s.flush()).map_err(|_| Stop::Gone)
    }

    fn begin(&mut self) -> Result<(), Stop> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        if let Some(s) = self.out.as_deref_mut() {
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n").map_err(|_| Stop::Gone)?;
        }
        let message = json!({
            "id": self.id, "type": "message", "role": "assistant", "model": self.model, "content": [],
            "stop_reason": null, "stop_sequence": null, "usage": self.usage_json(),
        });
        self.send("message_start", json!({ "type": "message_start", "message": message }))
    }

    fn close(&mut self) -> Result<(), Stop> {
        let Some(kind) = self.open.take() else { return Ok(()) };
        let index = self.blocks.len() - 1;
        if kind == "thinking" && !self.sig.is_empty() {
            let sig = tag(self.fam, &std::mem::take(&mut self.sig));
            self.blocks[index]["signature"] = json!(sig);
            self.send("content_block_delta", json!({ "type": "content_block_delta", "index": index, "delta": { "type": "signature_delta", "signature": sig } }))?;
        }
        self.send("content_block_stop", json!({ "type": "content_block_stop", "index": index }))
    }

    fn delta(&mut self, kind: &'static str, text: &str) -> Result<(), Stop> {
        if self.open != Some(kind) {
            self.close()?;
            self.begin()?;
            let block = if kind == "text" { json!({ "type": "text", "text": "" }) } else { json!({ "type": "thinking", "thinking": "", "signature": "" }) };
            self.send("content_block_start", json!({ "type": "content_block_start", "index": self.blocks.len(), "content_block": block }))?;
            self.blocks.push(block);
            self.open = Some(kind);
        }
        let index = self.blocks.len() - 1;
        let block = &mut self.blocks[index][kind];
        *block = json!(format!("{}{text}", block.as_str().unwrap_or("")));
        let delta = if kind == "text" { json!({ "type": "text_delta", "text": text }) } else { json!({ "type": "thinking_delta", "thinking": text }) };
        self.send("content_block_delta", json!({ "type": "content_block_delta", "index": index, "delta": delta }))
    }

    fn text(&mut self, text: &str) -> Result<(), Stop> {
        if !self.started && text.trim().is_empty() {
            self.pending.push_str(text);
            return Ok(());
        }
        let text = format!("{}{text}", std::mem::take(&mut self.pending));
        self.delta("text", &text)
    }

    /// A `tool_use` (or `server_tool_use`) block, sent whole: Cloud Code hands over function calls complete.
    fn tool(&mut self, kind: &str, id: String, name: &str, args: Value) -> Result<(), Stop> {
        self.close()?;
        self.begin()?;
        let index = self.blocks.len();
        self.send("content_block_start", json!({ "type": "content_block_start", "index": index, "content_block": { "type": kind, "id": id, "name": name, "input": {} } }))?;
        self.send("content_block_delta", json!({ "type": "content_block_delta", "index": index, "delta": { "type": "input_json_delta", "partial_json": args.to_string() } }))?;
        self.send("content_block_stop", json!({ "type": "content_block_stop", "index": index }))?;
        self.blocks.push(json!({ "type": kind, "id": id, "name": name, "input": args }));
        Ok(())
    }

    /// Grounding comes back as metadata, not as a tool call; Claude Code's WebSearch reads it from these two blocks.
    /// ponytail: one search block for all queries, with the cited pages; links are Google's redirect URLs, not resolved.
    fn search_results(&mut self) -> Result<(), Stop> {
        if self.queries.is_empty() && self.links.is_empty() {
            return Ok(());
        }
        if self.queries.is_empty() {
            self.queries.push(String::new());
        }
        let id = format!("srvtoolu_{}", antigravity::random_hex(12));
        self.tool("server_tool_use", id.clone(), "web_search", json!({ "query": self.queries.join("; ") }))?;
        let results: Vec<Value> = self
            .links
            .iter()
            .map(|(url, title)| json!({ "type": "web_search_result", "url": url, "title": title, "encrypted_content": "", "page_age": null }))
            .collect();
        let block = json!({ "type": "web_search_tool_result", "tool_use_id": id, "content": results });
        let index = self.blocks.len();
        self.send("content_block_start", json!({ "type": "content_block_start", "index": index, "content_block": block }))?;
        self.send("content_block_stop", json!({ "type": "content_block_stop", "index": index }))?;
        self.blocks.push(block);
        Ok(())
    }

    /// Fails the request: retryable while nothing was sent, otherwise an SSE `error` event (overloaded → Claude Code retries).
    fn fail(&mut self, f: Failure, block: Option<u64>) -> Stop {
        if self.started && self.out.is_some() {
            // Overloaded makes Claude Code retry; a client error must not, or it retries a dead request for ever.
            let kind = match f.status {
                429 => "rate_limit_error",
                400..=499 => error_kind(f.status),
                _ => "overloaded_error",
            };
            let _ = self.send("error", json!({ "type": "error", "error": { "type": kind, "message": f.message } }));
            antigravity::log(&format!("{}: failed mid-stream: {}", self.model, f.message));
            return Stop::Gone;
        }
        Stop::Retry(f, block)
    }

    fn chunk(&mut self, v: &Value) -> Result<(), Stop> {
        if let Some(e) = v.get("error").filter(|e| e.is_object()) {
            let code = e["code"].as_u64().unwrap_or(500) as u16;
            let quota = code == 429 || e["status"] == "RESOURCE_EXHAUSTED";
            let message = format!("Antigravity: {}", e["message"].as_str().unwrap_or("stream error"));
            let block = quota.then(|| antigravity::retry_secs_in(e).unwrap_or(60));
            return Err(self.fail(Failure { status: code, message, retry_after: block }, block));
        }
        let r = v.get("response").unwrap_or(v);
        if let Some(u) = r.get("usageMetadata") {
            let get = |k: &str| u[k].as_u64().unwrap_or(0);
            let (cand, thought, tool) = (get("candidatesTokenCount"), get("thoughtsTokenCount"), get("toolUsePromptTokenCount"));
            // Pages a built-in tool (Google Search) fed the model are input too, as on Anthropic's web search.
            let prompt = match get("promptTokenCount") {
                0 => get("totalTokenCount").saturating_sub(cand + thought),
                p => p + tool,
            };
            self.usage = (prompt, get("cachedContentTokenCount").min(prompt), cand + thought);
        }
        let cand = &r["candidates"][0];
        if cand.is_null() {
            if let Some(reason) = r["promptFeedback"]["blockReason"].as_str() {
                return Err(self.fail(Failure { status: 400, message: format!("Google blocked the request ({reason})"), retry_after: None }, None));
            }
        }
        for part in cand["content"]["parts"].as_array().into_iter().flatten() {
            let sig = part["thoughtSignature"].as_str().filter(|s| !s.is_empty());
            if let Some(fc) = part.get("functionCall") {
                let given = fc["id"].as_str().filter(|id| !id.is_empty() && !self.blocks.iter().any(|b| b["id"] == *id));
                let id = given.map_or_else(|| format!("toolu_{}", antigravity::random_hex(12)), String::from);
                if let Some(sig) = sig {
                    self.sigs.lock().unwrap().insert(id.clone(), tag(self.fam, sig));
                }
                let args = if fc["args"].is_object() { fc["args"].clone() } else { json!({}) };
                self.tool("tool_use", id, fc["name"].as_str().unwrap_or(""), args)?;
                continue;
            }
            match part["text"].as_str().filter(|t| !t.is_empty()) {
                Some(t) if part["thought"] == true => {
                    self.delta("thinking", t)?;
                    if let Some(s) = sig {
                        self.sig = s.to_string();
                    }
                }
                Some(t) => self.text(t)?,
                // A bare signature part belongs to the thinking block it follows.
                None if self.open == Some("thinking") => {
                    if let Some(s) = sig {
                        self.sig = s.to_string();
                    }
                }
                None => {}
            }
        }
        let g = &cand["groundingMetadata"];
        for q in g["webSearchQueries"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if !self.queries.iter().any(|x| x == q) {
                self.queries.push(q.to_string());
            }
        }
        for web in g["groundingChunks"].as_array().into_iter().flatten().map(|c| &c["web"]) {
            if let Some(url) = web["uri"].as_str().filter(|u| !self.links.iter().any(|(x, _)| x == u)) {
                self.links.push((url.to_string(), web["title"].as_str().unwrap_or(url).to_string()));
            }
        }
        if let Some(f) = cand["finishReason"].as_str() {
            self.finish = Some(f.to_string());
            // Google's explanation of an odd finish (e.g. which call was malformed); only ever used in error text.
            self.finish_note = cand["finishMessage"].as_str().unwrap_or("").chars().take(300).collect();
        }
        Ok(())
    }

    fn run(&mut self, reader: Box<dyn BufRead + Send>) -> Result<(), Stop> {
        let mut data = String::new();
        // The last thing Google sent, quoted when a stream ends oddly (also catches a body that isn't SSE at all).
        let mut tail = String::new();
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => return Err(self.fail(Failure { status: 502, message: format!("Antigravity stream broke: {e}"), retry_after: None }, None)),
            };
            if !line.trim().is_empty() {
                tail = line.chars().take(400).collect();
            }
            if let Some(d) = line.strip_prefix("data:") {
                data.push_str(d.trim_start());
            } else if line.trim().is_empty() && !data.is_empty() {
                let v: Value = serde_json::from_str(&std::mem::take(&mut data)).unwrap_or_default();
                self.chunk(&v)?;
            }
        }
        if !data.is_empty() {
            let v: Value = serde_json::from_str(&data).unwrap_or_default();
            self.chunk(&v)?;
        }
        let any_tool = self.blocks.iter().any(|b| b["type"] == "tool_use");
        self.stop_reason = match self.finish.as_deref() {
            Some("MAX_TOKENS") => "max_tokens",
            Some("STOP") if !self.started => {
                return Err(self.fail(Failure { status: 529, message: "Antigravity returned an empty response".into(), retry_after: None }, None))
            }
            Some("STOP") if any_tool => "tool_use",
            Some("STOP") => "end_turn",
            Some(other) => {
                let note = if self.finish_note.is_empty() { String::new() } else { format!(": {}", self.finish_note) };
                let f = Failure { status: 529, message: format!("Antigravity stopped generating ({other}{note})"), retry_after: None };
                return Err(self.fail(f, None));
            }
            None => {
                // A real answer always ends with a finish reason. Words without one are Google talking, not the model
                // (e.g. "this model is no longer available"): a 400 shows them and stops Claude Code retrying for ever.
                let said: String = self.blocks.iter().filter_map(|b| b["text"].as_str()).collect::<String>() + self.pending.trim();
                let f = match said.trim() {
                    "" => Failure { status: 529, message: format!("Antigravity stream ended without a finish reason; last line: {tail}"), retry_after: None },
                    said => Failure { status: 400, message: format!("Antigravity ({}): {said}", self.fam), retry_after: None },
                };
                return Err(self.fail(f, None));
            }
        };
        self.search_results()?;
        self.close()?;
        self.begin()?;
        let usage = self.usage_json();
        let stop = self.stop_reason;
        self.send("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": stop, "stop_sequence": null }, "usage": usage }))?;
        self.send("message_stop", json!({ "type": "message_stop" }))
    }

    fn message(&self) -> Value {
        json!({
            "id": self.id, "type": "message", "role": "assistant", "model": self.model, "content": self.blocks,
            "stop_reason": self.stop_reason, "stop_sequence": null, "usage": self.usage_json(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        Ctx {
            pool: Pool::fake("http://127.0.0.1:1", &["a"]),
            token: "t".into(),
            agent_id: "agent".into(),
            trajectory: "traj".into(),
            step: AtomicU64::new(0),
            sigs: Mutex::new(HashMap::new()),
            aliases: HashMap::new(),
            fallback: None,
            folder: PathBuf::new(),
            ratio: Mutex::new(HashMap::new()),
        }
    }

    #[test]
    fn schema_is_cloud_code_safe() {
        let s = json!({
            "$schema": "http://json-schema.org/draft-07/schema#", "type": "object", "additionalProperties": false,
            "properties": {
                "path": { "type": "string", "minLength": 1, "description": "File" },
                "limit": { "type": ["integer", "null"], "exclusiveMinimum": 0 },
                "mode": { "anyOf": [{ "type": "string", "enum": ["a", "b"] }, { "type": "null" }] },
                "opts": { "anyOf": [{ "type": "object", "properties": { "x": { "type": "string" } }, "required": ["x"] },
                                     { "type": "object", "properties": { "y": { "type": "number" } }, "required": ["y"] }] },
                "kind": { "const": "fixed" },
                "node": { "$ref": "#/$defs/node" },
                "tags": { "type": "array", "items": { "type": "string", "format": "uri" } },
                "grid": { "type": "array", "items": { "type": "array" } },
                "link": { "type": "string", "enum": ["", "https"] }
            },
            "required": ["path", "limit", "mode", "ghost"],
            "$defs": { "node": { "type": "object", "properties": { "id": { "type": "string" } } } }
        });
        let c = clean_schema(&s);
        let text = c.to_string();
        for banned in ["$schema", "additionalProperties", "anyOf", "null", "nullable", "$ref", "$defs", "minLength", "exclusiveMinimum", "format", "const"] {
            assert!(!text.contains(&format!("\"{banned}\"")), "{banned} left in {text}");
        }
        assert_eq!(c["required"], json!(["path"]), "nullable and unknown props are not required");
        assert_eq!(c["properties"]["path"]["description"], "File (minLength: 1)");
        assert_eq!(c["properties"]["limit"]["type"], "integer");
        assert_eq!(c["properties"]["mode"]["enum"], json!(["a", "b"]));
        assert_eq!(c["properties"]["opts"]["properties"]["y"]["type"], "number");
        assert!(c["properties"]["opts"].get("required").is_none(), "anyOf requires only what every branch requires");
        assert_eq!(c["properties"]["kind"], json!({ "type": "string", "enum": ["fixed"] }));
        assert_eq!(c["properties"]["node"]["properties"]["id"]["type"], "string");
        assert_eq!(c["properties"]["tags"]["items"]["type"], "string");
        assert_eq!(c["properties"]["grid"]["items"], json!({ "type": "array", "items": {} }), "arrays always carry items");
        assert_eq!(c["properties"]["link"]["enum"], json!(["https"]), "no empty enum members");
        assert_eq!(clean_schema(&json!({ "type": "string" })), json!({ "type": "object", "properties": {} }));
        assert_eq!(clean_schema(&json!({ "$ref": "#/$defs/a", "$defs": { "a": { "$ref": "#/$defs/a" } } }))["type"], "object", "self-ref terminates");
    }

    #[test]
    fn signatures_only_replay_on_their_family() {
        assert_eq!(untag(Some("ag:claude:QUJD"), "claude"), Some("QUJD"));
        assert_eq!(untag(Some("ag:claude:QUJD"), "gemini"), None);
        assert_eq!(untag(Some("EqQBCkgIBh"), "claude"), None, "Anthropic's own signature");
        assert_eq!(untag(Some("ag:claude:not base64!"), "claude"), None);
    }

    #[test]
    fn gemini_history_round_trips() {
        let ctx = ctx();
        ctx.sigs.lock().unwrap().insert("call2".into(), tag("gemini", "U0lH"));
        let req = json!({
            "model": "gemini-3.1-pro-low", "max_tokens": 100000,
            "system": [{ "type": "text", "text": "x-anthropic-billing-header: cc_version=2.1; cc_entrypoint=cli;" },
                       { "type": "text", "text": "You are a Claude agent, built on Anthropic's Claude Agent SDK.
be brief" }, { "type": "text", "text": " " }],
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "hmm", "signature": "foreign" },
                    { "type": "tool_use", "id": "call1", "name": "Read", "input": { "p": 1 } },
                    { "type": "tool_use", "id": "call2", "name": "Grep", "input": {} } ] },
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "call1", "content": [{ "type": "text", "text": "ok" }] },
                    { "type": "tool_result", "tool_use_id": "call2", "content": "boom", "is_error": true },
                    { "type": "text", "text": "go on" } ] }
            ],
            "tools": [{ "name": "Read", "input_schema": { "type": "object", "properties": {} } }, { "type": "web_search_20250305", "name": "web_search" }],
            "thinking": { "type": "enabled", "budget_tokens": 8000 }
        });
        let (body, beta) = build(&req, "gemini-3.1-pro-low", &ctx);
        let r = &body["request"];
        assert!(!beta);
        assert_eq!(r["systemInstruction"], json!({ "role": "user", "parts": [{ "text": "be brief" }] }));
        let model = &r["contents"][1]["parts"];
        assert_eq!(model.as_array().unwrap().len(), 2, "foreign thinking dropped");
        assert_eq!(model[0]["thoughtSignature"], SKIP_SIGNATURE);
        assert!(model[0]["functionCall"].get("id").is_none(), "Gemini calls carry no id");
        assert_eq!(model[1]["thoughtSignature"], "U0lH");
        assert_eq!(r["contents"][2]["parts"][1]["functionResponse"], json!({ "name": "Grep", "response": { "error": "boom" } }));
        assert_eq!(r["contents"][3], json!({ "role": "user", "parts": [{ "text": "go on" }] }));
        assert_eq!(r["tools"][0]["functionDeclarations"].as_array().unwrap().len(), 1, "server tools skipped");
        assert_eq!(r["toolConfig"]["functionCallingConfig"]["mode"], "VALIDATED");
        assert_eq!(r["generationConfig"], json!({ "maxOutputTokens": 65535, "thinkingConfig": { "includeThoughts": true, "thinkingBudget": 8000 } }));
        assert_eq!(r["labels"]["model_enum"], "MODEL_PLACEHOLDER_M36");
        assert!(r["sessionId"].as_str().unwrap().starts_with('-'));
    }

    #[test]
    fn claude_request_shape() {
        let ctx = ctx();
        let req = json!({
            "model": "claude-opus-4-6-thinking", "max_tokens": 128000, "thinking": { "type": "adaptive" },
            "messages": [{ "role": "assistant", "content": [
                { "type": "thinking", "thinking": "plan", "signature": tag("claude", "QUJD") },
                { "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {} } ] },
                { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "toolu_1", "content": "" }] }]
        });
        let (body, beta) = build(&req, "claude-opus-4-6-thinking", &ctx);
        let r = &body["request"];
        assert!(beta);
        assert_eq!(r["contents"][0]["parts"][0], json!({ "thought": true, "text": "plan", "thoughtSignature": "QUJD" }));
        assert_eq!(r["contents"][0]["parts"][1]["functionCall"]["id"], "toolu_1");
        assert!(r["contents"][0]["parts"][1].get("thoughtSignature").is_none());
        assert_eq!(r["contents"][1]["parts"][0]["functionResponse"]["id"], "toolu_1");
        assert_eq!(r["toolConfig"]["functionCallingConfig"]["mode"], "VALIDATED", "Claude always VALIDATED");
        assert_eq!(r["generationConfig"]["maxOutputTokens"], 64000);
        assert_eq!(r["generationConfig"]["thinkingConfig"]["thinkingBudget"], 24000);
        assert_eq!(body["requestType"], "agent");
    }

    // ---- end to end against a fake Cloud Code server

    fn sse(chunks: &[Value]) -> String {
        chunks.iter().map(|c| format!("data: {c}\r\n\r\n")).collect()
    }

    /// Answers by bearer token: `quota` → 429, `inband` → in-stream RESOURCE_EXHAUSTED, `empty` → empty STOP, anything else → `ok`.
    fn fake_upstream(ok: String) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let req = read_request(&s).unwrap();
                let who = req.header("authorization").unwrap_or("").trim_start_matches("Bearer ").to_string();
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                log.lock().unwrap().push(match req.path.contains("countTokens") {
                    true => format!("count {body}"),
                    false => format!("{who} {}", body["project"].as_str().unwrap_or("")),
                });
                let (status, text) = match who.as_str() {
                    _ if req.path.contains("countTokens") => (200, json!({ "totalTokens": 42 }).to_string()),
                    "search" => (200, sse(&[
                        json!({ "response": { "candidates": [{ "content": { "parts": [{ "text": "Rust 1.99 is out." }] } }] } }),
                        json!({ "response": { "candidates": [{ "content": { "parts": [] }, "finishReason": "STOP", "groundingMetadata": {
                            "webSearchQueries": ["latest rust version"],
                            "groundingChunks": [{ "web": { "uri": "https://blog.rust-lang.org/x", "title": "rust-lang.org" } },
                                                { "web": { "uri": "https://blog.rust-lang.org/x", "title": "dup" } }] } }],
                                     "usageMetadata": { "promptTokenCount": 50, "toolUsePromptTokenCount": 30, "candidatesTokenCount": 5 } } }),
                    ])),
                    // What Google really sent for a retired model (Oct 2026): words, then the stream just ends.
                    "retired" => (200, sse(&[json!({ "response": { "candidates": [{ "content": { "parts": [
                        { "text": "Gemini 3.5 Flash is no longer available. Please switch to Gemini 3.7 Flash in the latest version of Antigravity." } ] } }] } })])),
                    "silent" => (200, sse(&[json!({ "response": { "candidates": [{ "content": { "parts": [] } }] } })])),
                    "image" => (200, sse(&[json!({ "response": { "candidates": [{ "content": { "parts": [
                        { "thought": true, "inlineData": { "mimeType": "image/png", "data": "ZHJhZnQ=" } },
                        { "text": "Here is your cat." },
                        { "inlineData": { "mimeType": "image/jpeg", "data": "/9j/AA==" } } ] }, "finishReason": "STOP" }] } })])),
                    "quota" =>(429, r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"exhausted","details":[{"retryDelay":"90s"}]}}"#.to_string()),
                    "inband" => (200, sse(&[json!({ "error": { "code": 429, "status": "RESOURCE_EXHAUSTED", "message": "quota" } })])),
                    "empty" => (200, sse(&[json!({ "response": { "candidates": [{ "content": { "parts": [{ "text": " " }] }, "finishReason": "STOP" }] } })])),
                    _ => (200, ok.clone()),
                };
                let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
            }
        });
        (url, seen)
    }

    fn post(proxy: &Proxy, token: &str, body: &Value) -> (u16, String) {
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
        let mut res = agent.post(format!("{}/v1/messages?beta=true", proxy.url)).header("authorization", format!("Bearer {token}")).send(body.to_string()).unwrap();
        (res.status().as_u16(), res.body_mut().read_to_string().unwrap())
    }

    fn reply() -> String {
        sse(&[
            json!({ "response": { "candidates": [{ "content": { "role": "model", "parts": [{ "thought": true, "text": "think" }] } }],
                    "usageMetadata": { "promptTokenCount": 100, "cachedContentTokenCount": 40 } } }),
            json!({ "response": { "candidates": [{ "content": { "parts": [{ "text": "", "thoughtSignature": "U0lH" }, { "text": "Hello" }] } }] } }),
            json!({ "response": { "candidates": [{ "content": { "parts": [{ "functionCall": { "name": "Read", "args": { "p": "a" } }, "thoughtSignature": "Q0FMTA==" }] },
                    "finishReason": "STOP" }], "usageMetadata": { "promptTokenCount": 100, "cachedContentTokenCount": 40, "candidatesTokenCount": 7, "thoughtsTokenCount": 3 } } }),
        ])
    }

    #[test]
    fn rotates_past_quota_and_in_band_errors() {
        let (url, seen) = fake_upstream(reply());
        let proxy = serve_pool(Pool::fake(&url, &["quota", "inband", "good"]), HashMap::new(), None, PathBuf::new()).unwrap();
        let req = json!({ "model": "gemini-3.1-pro-low", "max_tokens": 1000, "stream": true, "messages": [{ "role": "user", "content": "hi" }] });
        assert_eq!(post(&proxy, "nope", &req).0, 401);

        let (status, body) = post(&proxy, &proxy.token, &req);
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("message_stop"));
        // quota: HTTP 429 → next account. inband: 200 then an in-stream quota error → blocked, request retried.
        assert_eq!(*seen.lock().unwrap(), ["quota p-quota", "inband p-inband", "good p-good"].map(String::from));

        // Both stay blocked for this family, so the next request goes straight to the healthy account.
        post(&proxy, &proxy.token, &req);
        assert_eq!(seen.lock().unwrap().last().unwrap(), "good p-good");
        assert_eq!(seen.lock().unwrap().len(), 4);
    }

    #[test]
    fn empty_answers_are_retried_then_reported() {
        let (url, seen) = fake_upstream(reply());
        let proxy = serve_pool(Pool::fake(&url, &["empty"]), HashMap::new(), None, PathBuf::new()).unwrap();
        let req = json!({ "model": "gemini-3.1-pro-low", "max_tokens": 1000, "stream": true, "messages": [{ "role": "user", "content": "hi" }] });
        let (status, body) = post(&proxy, &proxy.token, &req);
        assert_eq!(status, 529, "nothing was streamed, so Claude Code gets a retryable status: {body}");
        assert!(body.contains("overloaded_error") && body.contains("empty response"), "{body}");
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[test]
    fn translates_stream_and_non_stream() {
        let (url, seen) = fake_upstream(reply());
        let aliases = HashMap::from([("claude-opus-5-5".to_string(), "gemini-3.1-pro-low".to_string())]);
        let proxy = serve_pool(Pool::fake(&url, &["good", "quota"]), aliases, Some("gemini-3.1-pro-low".into()), PathBuf::new()).unwrap();
        let req = json!({ "model": "claude-opus-5-5[1m]", "max_tokens": 1000, "stream": true, "messages": [{ "role": "user", "content": "hi" }] });
        let (status, body) = post(&proxy, &proxy.token, &req);
        assert_eq!(status, 200, "{body}");
        let events: Vec<Value> = body.lines().filter_map(|l| l.strip_prefix("data: ")).map(|d| serde_json::from_str(d).unwrap()).collect();
        let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            [
                "message_start", "content_block_start", "content_block_delta", "content_block_delta", "content_block_stop", "content_block_start",
                "content_block_delta", "content_block_stop", "content_block_start", "content_block_delta", "content_block_stop", "message_delta", "message_stop"
            ]
        );
        assert_eq!(events[0]["message"]["usage"]["input_tokens"], 60);
        assert_eq!(events[0]["message"]["model"], "claude-opus-5-5", "replies keep the spoofed name");
        assert_eq!(events[3]["delta"], json!({ "type": "signature_delta", "signature": "ag:gemini:U0lH" }));
        assert_eq!(events[6]["delta"]["text"], "Hello");
        let tool_id = events[8]["content_block"]["id"].as_str().unwrap().to_string();
        assert!(tool_id.starts_with("toolu_"));
        assert_eq!(events[9]["delta"]["partial_json"], r#"{"p":"a"}"#);
        assert_eq!(events[11]["delta"]["stop_reason"], "tool_use");
        assert_eq!(events[11]["usage"]["output_tokens"], 10);

        // Non-stream: one JSON message; the function-call signature is remembered for the next turn.
        let mut req = req;
        req["stream"] = json!(false);
        req["model"] = json!("claude-haiku-4-5-20251001"); // a name Claude Code picked itself → fallback model
        let (status, body) = post(&proxy, &proxy.token, &req);
        assert_eq!(status, 200, "{body}");
        let msg: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(msg["content"][0], json!({ "type": "thinking", "thinking": "think", "signature": "ag:gemini:U0lH" }));
        assert_eq!(msg["content"][2]["input"], json!({ "p": "a" }));
        assert_eq!(msg["stop_reason"], "tool_use");
        assert_eq!(msg["model"], "claude-haiku-4-5-20251001");
        assert_eq!(*seen.lock().unwrap(), ["good p-good", "good p-good"].map(String::from), "sticky on the healthy account");
    }

    #[test]
    fn all_blocked_is_a_429_with_retry_after() {
        let (url, _) = fake_upstream(reply());
        let proxy = serve_pool(Pool::fake(&url, &["quota"]), HashMap::new(), None, PathBuf::new()).unwrap();
        let req = json!({ "model": "claude-sonnet-4-6", "max_tokens": 10, "messages": [{ "role": "user", "content": "hi" }] });
        let (status, body) = post(&proxy, &proxy.token, &req);
        assert_eq!(status, 429);
        assert!(body.contains("rate_limit_error") && body.contains("claude quota"), "{body}");
    }

    fn web_search_req(model: &str) -> Value {
        json!({
            "model": model, "max_tokens": 1000, "stream": true,
            "messages": [{ "role": "user", "content": "Perform a web search for the query: latest rust version" }],
            "tools": [{ "type": "web_search_20250305", "name": "web_search", "max_uses": 8, "allowed_domains": ["rust-lang.org"] }]
        })
    }

    #[test]
    fn web_search_becomes_google_grounding() {
        let ctx = ctx();
        let (body, _) = build(&web_search_req("gemini-3.1-pro-low"), "gemini-3.1-pro-low", &ctx);
        let r = &body["request"];
        assert_eq!(r["tools"], json!([{ "googleSearch": {} }]));
        assert!(r.get("toolConfig").is_none(), "no function calling alongside grounding");
        assert_eq!(r["systemInstruction"]["parts"][0]["text"], "Only use web results from these domains: rust-lang.org.");

        // Mixed with client tools it is not a WebSearch sub-request: the server tool is dropped as before.
        let mut mixed = web_search_req("gemini-3.1-pro-low");
        mixed["tools"].as_array_mut().unwrap().push(json!({ "name": "Read", "input_schema": { "type": "object" } }));
        assert!(web_search(&mixed).is_none());
        let (body, _) = build(&mixed, "gemini-3.1-pro-low", &ctx);
        assert_eq!(body["request"]["tools"][0]["functionDeclarations"][0]["name"], "Read");
    }

    #[test]
    fn web_search_results_reach_claude_code() {
        let (url, _) = fake_upstream(reply());
        // The Claude route is swapped to the Gemini search model; replies keep the asked-for name.
        let aliases = HashMap::from([("claude-opus-5-5".to_string(), "claude-opus-4-6-thinking".to_string())]);
        let proxy = serve_pool(Pool::fake(&url, &["search"]), aliases, None, PathBuf::new()).unwrap();
        let (status, body) = post(&proxy, &proxy.token, &web_search_req("claude-opus-5-5"));
        assert_eq!(status, 200, "{body}");
        let events: Vec<Value> = body.lines().filter_map(|l| l.strip_prefix("data: ")).map(|d| serde_json::from_str(d).unwrap()).collect();
        let starts: Vec<&Value> = events.iter().filter(|e| e["type"] == "content_block_start").map(|e| &e["content_block"]).collect();
        assert_eq!(starts.iter().map(|b| b["type"].as_str().unwrap()).collect::<Vec<_>>(), ["text", "server_tool_use", "web_search_tool_result"]);
        assert_eq!(starts[1]["name"], "web_search");
        assert_eq!(starts[2]["tool_use_id"], starts[1]["id"]);
        assert_eq!(starts[2]["content"], json!([{ "type": "web_search_result", "url": "https://blog.rust-lang.org/x", "title": "rust-lang.org", "encrypted_content": "", "page_age": null }]));
        assert!(body.contains(r#"{\"query\":\"latest rust version\"}"#), "{body}");
        let end = events.iter().find(|e| e["type"] == "message_delta").unwrap();
        assert_eq!(end["delta"]["stop_reason"], "end_turn");
        assert_eq!(end["usage"]["server_tool_use"]["web_search_requests"], 1);
        assert_eq!(end["usage"]["input_tokens"], 80, "pages Google Search read count as input");
        assert_eq!(events[0]["message"]["model"], "claude-opus-5-5");

        let mut req = web_search_req("claude-opus-5-5");
        req["stream"] = json!(false);
        let msg: Value = serde_json::from_str(&post(&proxy, &proxy.token, &req).1).unwrap();
        let kinds: Vec<&str> = msg["content"].as_array().unwrap().iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["text", "server_tool_use", "web_search_tool_result"]);
    }

    #[test]
    fn structured_output_maps_to_response_schema() {
        let ctx = ctx();
        let schema = json!({ "type": "object", "properties": { "n": { "type": "integer" } }, "required": ["n"], "additionalProperties": false });
        let mut req = json!({ "model": "m", "max_tokens": 100, "messages": [{ "role": "user", "content": "hi" }],
                              "output_config": { "format": { "type": "json_schema", "schema": schema } } });
        let (body, _) = build(&req, "gemini-3.1-pro-low", &ctx);
        let gen = &body["request"]["generationConfig"];
        assert_eq!(gen["responseMimeType"], "application/json");
        assert_eq!(gen["responseSchema"], json!({ "type": "object", "properties": { "n": { "type": "integer" } }, "required": ["n"] }));
        let (body, _) = build(&req, "claude-sonnet-4-6", &ctx);
        assert!(body["request"]["generationConfig"].get("responseSchema").is_none(), "Gemini only");
        req.as_object_mut().unwrap().remove("output_config");
        req["output_format"] = json!({ "type": "json_schema", "schema": schema });
        assert!(build(&req, "gemini-3.1-pro-low", &ctx).0["request"]["generationConfig"].get("responseSchema").is_some(), "older beta field");
    }

    #[test]
    fn google_notices_surface_once_instead_of_looping() {
        let (url, seen) = fake_upstream(reply());
        let req = |stream: bool| json!({ "model": "gemini-3.5-flash-low", "max_tokens": 64, "stream": stream, "messages": [{ "role": "user", "content": "hi" }] });

        let proxy = serve_pool(Pool::fake(&url, &["retired"]), HashMap::new(), None, PathBuf::new()).unwrap();
        let (status, body) = post(&proxy, &proxy.token, &req(false));
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("invalid_request_error") && body.contains("no longer available"), "Google's words reach the user: {body}");
        assert_eq!(seen.lock().unwrap().len(), 1, "a client error is not retried");
        // Streaming: the notice already went out as text, so the error rides in-band — as a non-retryable kind.
        let (_, body) = post(&proxy, &proxy.token, &req(true));
        assert!(body.contains("event: error") && body.contains("invalid_request_error"), "{body}");

        // Nothing said and no finish reason: still a retryable hiccup.
        let proxy = serve_pool(Pool::fake(&url, &["silent"]), HashMap::new(), None, PathBuf::new()).unwrap();
        let (status, body) = post(&proxy, &proxy.token, &req(false));
        assert_eq!(status, 529, "{body}");
        assert!(body.contains("without a finish reason; last line: data:"), "{body}");
        assert_eq!(seen.lock().unwrap().len(), 2 + 3);
    }

    #[test]
    fn weigh_skips_media_but_counts_images() {
        let img = json!({ "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "A".repeat(50_000) } });
        let (bytes, images) = weigh(&json!({ "messages": [{ "role": "user", "content": [img.clone(), { "type": "text", "text": "hi" }] }] }));
        assert_eq!(images, 1);
        assert!(bytes < 200, "base64 is not text: {bytes}");
        let ctx = ctx();
        let req = json!({ "messages": [{ "role": "user", "content": [img] }] });
        assert_eq!(ctx.estimate("claude", &req), (weigh(&req).0 as f64 * DEFAULT_RATIO).round() as u64 + IMAGE_TOKENS);
    }

    #[test]
    fn count_tokens_is_exact_on_gemini_and_calibrated_elsewhere() {
        let (url, seen) = fake_upstream(reply());
        let proxy = serve_pool(Pool::fake(&url, &["good"]), HashMap::new(), None, PathBuf::new()).unwrap();
        let tools = json!([{ "name": "Read", "description": "Read a file", "input_schema": { "type": "object", "additionalProperties": false,
                             "properties": { "path": { "type": "string", "minLength": 1 } } } }, { "type": "web_search_20250305", "name": "web_search" }]);
        let req = |model: &str| json!({ "model": model, "system": [{ "type": "text", "text": "x-anthropic-billing-header: cch=1;" }, { "type": "text", "text": "be brief" }],
                                       "tools": tools, "messages": [{ "role": "user", "content": "hello there" }] });
        let count = |body: &Value| {
            let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
            let mut res = agent.post(format!("{}/v1/messages/count_tokens", proxy.url)).header("x-api-key", &proxy.token).send(body.to_string()).unwrap();
            let v: Value = serde_json::from_str(&res.body_mut().read_to_string().unwrap()).unwrap();
            v["input_tokens"].as_u64().unwrap()
        };

        // Gemini: Google counts the translated request — filtered system prompt, Cloud Code declarations, contents.
        assert_eq!(count(&req("gemini-3.1-pro-low")), 42);
        let sent: Value = serde_json::from_str(seen.lock().unwrap()[0].strip_prefix("count ").unwrap()).unwrap();
        let c = &sent["request"]["contents"];
        assert_eq!(sent["request"]["model"], "models/gemini-3.1-pro-low");
        assert_eq!(c[0]["parts"][0], json!({ "text": "be brief" }), "billing header filtered as on the real request");
        let decls: Value = serde_json::from_str(c[0]["parts"][1]["text"].as_str().unwrap()).unwrap();
        assert_eq!(decls, json!([{ "name": "Read", "description": "Read a file",
                                   "parameters": { "type": "object", "properties": { "path": { "type": "string", "description": "(minLength: 1)" } } } }]),
                   "what Gemini is really sent, not the raw Anthropic schema; server tools skipped");
        assert_eq!(c[1], json!({ "role": "user", "parts": [{ "text": "hello there" }] }));

        // Claude route, nothing learned yet: bytes/4 of the text.
        let claude = req("claude-sonnet-4-6");
        assert_eq!(count(&claude), (weigh(&claude).0 as f64 * DEFAULT_RATIO).round() as u64);
        assert_eq!(seen.lock().unwrap().len(), 1, "only the Gemini count went upstream");

        // One real Claude-route answer (fake upstream: promptTokenCount 100) calibrates the ratio for that family.
        let mut turn = req("claude-sonnet-4-6");
        turn["max_tokens"] = json!(100);
        assert_eq!(post(&proxy, &proxy.token, &turn).0, 200);
        let learned = 100.0 / weigh(&turn).0 as f64;
        assert_eq!(count(&claude), (weigh(&claude).0 as f64 * learned).round() as u64);
        let r2 = 100.0 / weigh(&turn).0 as f64;
        post(&proxy, &proxy.token, &turn);
        assert_eq!(count(&claude), (weigh(&claude).0 as f64 * (learned * 0.7 + r2 * 0.3)).round() as u64, "moving average");
    }

    #[test]
    fn mcp_generates_and_saves_images() {
        let (url, _) = fake_upstream(reply());
        let dir = std::env::temp_dir().join(format!("ccat-mcp-{}", antigravity::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("in.png"), b"png").unwrap();
        let proxy = serve_pool(Pool::fake(&url, &["image"]), HashMap::new(), None, dir.clone()).unwrap();
        let rpc = |body: Value| {
            let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
            let mut res = agent.post(format!("{}/mcp", proxy.url)).header("authorization", format!("Bearer {}", proxy.token)).send(body.to_string()).unwrap();
            (res.status().as_u16(), res.body_mut().read_to_string().unwrap())
        };
        let (status, body) = rpc(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } }));
        assert_eq!(status, 200);
        let init: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(rpc(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })), (202, String::new()));
        let list: Value = serde_json::from_str(&rpc(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).1).unwrap();
        assert_eq!(list["result"]["tools"][0]["name"], "generate_image");

        let call = json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                           "params": { "name": "generate_image", "arguments": { "prompt": "a cat", "path": "art/cat.png", "images": ["in.png"] } } });
        let out: Value = serde_json::from_str(&rpc(call).1).unwrap();
        let content = &out["result"]["content"];
        assert!(out["result"].get("isError").is_none(), "{out}");
        assert_eq!(content[1], json!({ "type": "image", "data": "/9j/AA==", "mimeType": "image/jpeg" }), "the final image, not the thought draft");
        assert!(content[0]["text"].as_str().unwrap().contains("Here is your cat."));
        assert_eq!(std::fs::read(dir.join("art/cat.jpg")).unwrap(), [0xff, 0xd8, 0xff, 0x00], "saved with the real format's extension");

        let bad = json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "generate_image", "arguments": { "prompt": "x", "images": ["nope.png"] } } });
        let out: Value = serde_json::from_str(&rpc(bad).1).unwrap();
        assert_eq!(out["result"]["isError"], true);
        let unknown: Value = serde_json::from_str(&rpc(json!({ "jsonrpc": "2.0", "id": 5, "method": "resources/list" })).1).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
        assert_eq!(rpc(json!({ "jsonrpc": "2.0", "id": 6, "method": "ping" })).0, 200);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Live check against Google with the first Antigravity account in the ccat config: the proxy's `count_tokens`
    /// vs the `promptTokenCount` Google bills for the same request, implicit caching on a repeat, and the calibrated
    /// Claude-route estimate. Spends a few requests of quota. `cargo test -- --ignored live_ --nocapture`.
    #[test]
    #[ignore = "uses your real Antigravity account from the ccat config"]
    fn live_counts_and_caching_match_google() {
        let cfg = crate::load();
        let p = cfg.providers.iter().find(|p| p.antigravity && !p.accounts.is_empty()).expect("an Antigravity provider with an account in the ccat config");
        let proxy = start(p.accounts.clone(), HashMap::new(), None, std::env::temp_dir()).unwrap();
        // Big enough (~5k tokens) for Gemini's implicit cache, which skips short prompts.
        let manual: String = (0..400).map(|i| format!("Rule {i}: keep functions short, name things for what they do, test the edge cases.\n")).collect();
        let req = |model: &str| json!({
            "model": model, "max_tokens": 1024, "stream": false,
            "system": [{ "type": "text", "text": "x-anthropic-billing-header: cch=live;" }, { "type": "text", "text": manual }],
            "tools": [{ "name": "Read", "description": "Read a file from disk", "input_schema": { "type": "object",
                        "properties": { "file_path": { "type": "string", "description": "Absolute path" }, "limit": { "type": ["integer", "null"] } }, "required": ["file_path"] } }],
            "messages": [{ "role": "user", "content": "Reply with the single word: ok" }]
        });
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
        let call = |path: &str, body: &Value| {
            let mut res = agent.post(format!("{}{path}", proxy.url)).header("x-api-key", &proxy.token).send(body.to_string()).unwrap();
            let status = res.status().as_u16();
            let text = res.body_mut().read_to_string().unwrap();
            assert_eq!(status, 200, "{path}: {text}");
            serde_json::from_str::<Value>(&text).unwrap()
        };
        let billed = |m: &Value| ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"].iter().map(|k| m["usage"][k].as_u64().unwrap_or(0)).sum::<u64>();
        let near = |a: u64, b: u64, pct: f64| (a as f64 - b as f64).abs() <= b as f64 * pct;

        let gemini = SEARCH_MODEL; // a live Gemini model ccat relies on anyway
        let counted = call("/v1/messages/count_tokens", &req(gemini))["input_tokens"].as_u64().unwrap();
        println!("gemini: count_tokens {counted}");
        // The first live run failed here with max_tokens 64 ("stream ended without a finish reason"): report what a tiny
        // budget gets back, without failing on it, so the cause shows up in the output.
        let mut tiny = req(gemini);
        tiny["max_tokens"] = json!(64);
        let mut res = agent.post(format!("{}/v1/messages", proxy.url)).header("x-api-key", &proxy.token).send(tiny.to_string()).unwrap();
        println!("gemini max_tokens 64: {} {}", res.status().as_u16(), res.body_mut().read_to_string().unwrap().chars().take(600).collect::<String>());
        let first = call("/v1/messages", &req(gemini));
        let real = billed(&first);
        println!("gemini: count_tokens {counted} vs Google prompt {real}");
        assert!(near(counted, real, 0.05), "Gemini count {counted} should be within 5% of Google's {real}");

        let mut cached = 0;
        for _ in 0..3 {
            std::thread::sleep(Duration::from_secs(3));
            let again = call("/v1/messages", &req(gemini));
            cached = again["usage"]["cache_read_input_tokens"].as_u64().unwrap_or(0);
            println!("gemini repeat: input {} cache_read {cached}", again["usage"]["input_tokens"]);
            if cached > 0 {
                break;
            }
        }
        assert!(cached > 0, "an identical repeat should hit Google's implicit cache");

        let claude = "claude-sonnet-4-6";
        let before = call("/v1/messages/count_tokens", &req(claude))["input_tokens"].as_u64().unwrap();
        let real = billed(&call("/v1/messages", &req(claude)));
        let after = call("/v1/messages/count_tokens", &req(claude))["input_tokens"].as_u64().unwrap();
        println!("claude: estimate before {before}, after learning {after}, real {real}");
        assert!(near(after, real, 0.02), "after one answer the Claude estimate {after} should match the real {real}");
    }

    /// The real `claude` CLI against this proxy over a scripted fake Cloud Code: proves Claude Code itself accepts the
    /// web-search blocks and the MCP server. No Google account, no network. `cargo test -- --ignored claude_code`.
    #[test]
    #[ignore = "needs the claude CLI on PATH"]
    fn claude_code_end_to_end() {
        const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let body: Value = serde_json::from_slice(&read_request(&s).unwrap().body).unwrap();
                let r = &body["request"];
                let decls: Vec<&str> = r["tools"][0]["functionDeclarations"].as_array().into_iter().flatten().filter_map(|d| d["name"].as_str()).collect();
                let last = r["contents"].as_array().and_then(|c| c.last()).cloned().unwrap_or_default();
                let first = r["contents"][0]["parts"].to_string();
                let parts = if body["model"] == crate::mcp::IMAGE_MODEL {
                    log.lock().unwrap().push("image".into());
                    json!([{ "text": "a cat" }, { "inlineData": { "mimeType": "image/png", "data": PNG } }])
                } else if r["tools"][0].get("googleSearch").is_some() {
                    log.lock().unwrap().push(format!("search via {}", body["model"]));
                    json!([{ "text": "Rust 1.99 is the latest stable release." }])
                } else if let Some(fr) = last["parts"][0].get("functionResponse") {
                    // Echo the tool result so the test can read what Claude Code made of it.
                    json!([{ "text": format!("FINAL {}", fr["response"]) }])
                } else if first.contains("SEARCHTEST") && decls.contains(&"WebSearch") {
                    json!([{ "functionCall": { "name": "WebSearch", "args": { "query": "latest rust version" } } }])
                } else if first.contains("IMAGETEST") && decls.contains(&"mcp__antigravity__generate_image") {
                    json!([{ "functionCall": { "name": "mcp__antigravity__generate_image", "args": { "prompt": "a cat", "path": "out/cat.png" } } }])
                } else {
                    log.lock().unwrap().push(format!("plain; tools: {}", decls.join(",")));
                    json!([{ "text": "ok" }])
                };
                let mut cand = json!({ "content": { "role": "model", "parts": parts }, "finishReason": "STOP" });
                if r["tools"][0].get("googleSearch").is_some() {
                    cand["groundingMetadata"] = json!({ "webSearchQueries": ["latest rust version"],
                        "groundingChunks": [{ "web": { "uri": "https://blog.rust-lang.org/2026/rust-1.99", "title": "blog.rust-lang.org" } }] });
                }
                let text = sse(&[json!({ "response": { "candidates": [cand], "usageMetadata": { "promptTokenCount": 10, "candidatesTokenCount": 5 } } })]);
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
            }
        });

        let dir = std::env::temp_dir().join(format!("ccat-e2e-{}", antigravity::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        // As ccat launches it: a Claude name for the main model, everything else falling back to it.
        let aliases = HashMap::from([("claude-sonnet-4-6".to_string(), "gemini-3.1-pro-low".to_string())]);
        let proxy = serve_pool(Pool::fake(&upstream, &["acct"]), aliases, Some("gemini-3.1-pro-low".into()), dir.clone()).unwrap();
        let cfg = dir.join("mcp.json");
        let servers = json!({ "mcpServers": { "antigravity": { "type": "http", "url": format!("{}/mcp", proxy.url), "headers": { "Authorization": format!("Bearer {}", proxy.token) } } } });
        std::fs::write(&cfg, servers.to_string()).unwrap();
        let run = |prompt: &str| {
            let cmd = |program: &str| {
                let mut c = std::process::Command::new(program);
                // A parent Claude Code session (e.g. the one running this test) passes host auth down; drop it all.
                for (k, _) in std::env::vars_os() {
                    let k = k.to_string_lossy().to_ascii_uppercase();
                    if k.starts_with("CLAUDE") || k.starts_with("ANTHROPIC") {
                        c.env_remove(&k);
                    }
                }
                c.current_dir(&dir)
                    // Not --bare (that mode ships a minimal toolset without WebSearch); no user settings, so no user hooks.
                    .args(["-p", prompt, "--no-session-persistence", "--model", "claude-sonnet-4-6", "--strict-mcp-config", "--setting-sources", "project"])
                    .args(["--allowedTools", "WebSearch", "mcp__antigravity__generate_image", "--mcp-config"])
                    .arg(&cfg)
                    .env("ANTHROPIC_BASE_URL", &proxy.url)
                    .env("ANTHROPIC_AUTH_TOKEN", &proxy.token)
                    .env_remove("ANTHROPIC_API_KEY");
                c
            };
            let out = cmd("claude").output().or_else(|_| cmd("claude.cmd").output()).expect("claude on PATH");
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
        };

        let out = run("SEARCHTEST");
        let log = seen.lock().unwrap().clone();
        assert!(out.contains("blog.rust-lang.org/2026/rust-1.99"), "WebSearch result should carry the cited link.\n{out}\n{log:?}");
        assert!(log.iter().any(|l| l.starts_with("search via")), "{log:?}");

        let out = run("IMAGETEST");
        assert!(out.contains("Saved"), "MCP tool result should reach the model.\n{out}\n{:?}", seen.lock().unwrap());
        assert!(dir.join("out/cat.png").exists(), "image saved in the launch folder");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
