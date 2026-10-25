//! The Anthropic SSE writer (`Tx`) every translated provider streams through, the OpenAI-chat stream parser
//! MuseAI uses, and the request helpers all translations need: signature tags, content blocks,
//! tool declarations and the token estimate.

use super::http::error_kind;
use super::Ctx;
use crate::providers::antigravity::family;
use crate::common::{self, Failure};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, Write},
    net::TcpStream,
    sync::Mutex,
};


/// Tokens one image costs. ponytail: a flat middle-of-the-road figure (Gemini 3 default resolution ≈1120, a
/// ~1000px Claude image ≈1300); size images properly if `/context` ever needs to be exact for screenshots.
pub(crate) const IMAGE_TOKENS: u64 = 1100;
/// Before the first real response: the classic bytes/4.
pub(crate) const DEFAULT_RATIO: f64 = 0.25;

/// Text bytes and image count of an Anthropic request: base64 media (`data`) would swamp a bytes-per-token ratio.
pub(crate) fn weigh(v: &Value) -> (u64, u64) {
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
    pub(crate) fn learn(&self, fam: &'static str, req: &Value, prompt_tokens: u64) {
        let (bytes, images) = weigh(req);
        if bytes == 0 || prompt_tokens == 0 {
            return;
        }
        let r = prompt_tokens.saturating_sub(images * IMAGE_TOKENS) as f64 / bytes as f64;
        let mut g = self.ratio.lock().unwrap();
        let next = g.get(fam).map_or(r, |old| old * 0.7 + r * 0.3);
        g.insert(fam, next);
    }

    pub(crate) fn estimate(&self, fam: &str, req: &Value) -> u64 {
        let (bytes, images) = weigh(req);
        let r = self.ratio.lock().unwrap().get(fam).copied().unwrap_or(DEFAULT_RATIO);
        (bytes as f64 * r).round() as u64 + images * IMAGE_TOKENS
    }
}

/// Signatures handed to Claude Code are tagged with the model family that made them, so a resumed session
/// on another model (or Anthropic's own signatures) never reaches Google as a "corrupted" signature.
pub(crate) fn tag(fam: &str, sig: &str) -> String {
    format!("ag:{fam}:{sig}")
}

pub(crate) fn untag<'a>(sig: Option<&'a str>, fam: &str) -> Option<&'a str> {
    let s = sig?.strip_prefix("ag:")?.strip_prefix(fam)?.strip_prefix(':')?;
    let base64 = !s.is_empty() && s.len() % 4 == 0 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b));
    base64.then_some(s)
}

/// Anthropic content: a string or a block array.
pub(crate) fn blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::String(t) => vec![json!({ "type": "text", "text": t })],
        Value::Array(a) => a.clone(),
        _ => vec![],
    }
}

/// Resolve Claude's on-demand tool declarations once for every translated provider.
pub(crate) fn client_tools(req: &Value) -> Value {
    let mut tools = req.get("tools").cloned().unwrap_or_else(|| json!([]));
    let mut loaded = HashSet::new();
    if req["tool_choice"]["type"] == "tool" {
        if let Some(name) = req["tool_choice"]["name"].as_str() { loaded.insert(name); }
    }
    for block in req["messages"].as_array().into_iter().flatten().flat_map(|m| m["content"].as_array().into_iter().flatten()) {
        if block["type"] == "tool_addition" {
            let tool = &block["tool"];
            if tool["type"] == "tool_definition" {
                let definition = &tool["definition"];
                if let Some(name) = definition["name"].as_str().filter(|s| !s.is_empty()) {
                    loaded.insert(name);
                    if let Some(list) = tools.as_array_mut() {
                        if let Some(previous) = list.iter_mut().find(|t| t["name"] == name) { *previous = definition.clone(); }
                        else { list.push(definition.clone()); }
                    }
                }
            } else if tool["type"] == "tool_reference" {
                if let Some(name) = tool["name"].as_str() { loaded.insert(name); }
            }
        } else if block["type"] == "tool_result" {
            for reference in block["content"].as_array().into_iter().flatten().filter(|v| v["type"] == "tool_reference") {
                if let Some(name) = reference["tool_name"].as_str().or_else(|| reference["name"].as_str()) { loaded.insert(name); }
            }
        }
    }
    if let Some(list) = tools.as_array_mut() {
        list.retain(|tool| tool["defer_loading"] != true || tool["name"].as_str().is_some_and(|name| loaded.contains(name)));
    }
    tools
}

pub(crate) fn tool_result_text(block: &Value) -> Option<String> {
    match block["type"].as_str()? {
        "text" => block["text"].as_str().map(String::from),
        "tool_reference" => block["tool_name"].as_str().or_else(|| block["name"].as_str()).map(|name| format!("Tool {name} is available.")),
        _ => None,
    }
}

#[derive(Debug)]
pub(crate) enum Stop {
    /// Nothing reached Claude Code yet: try again (optionally blocking the account for that many seconds).
    Retry(Failure, Option<u64>),
    /// The client hung up, or the error already went out as an SSE `error` event.
    Gone,
}

pub(crate) struct Tx<'a> {
    pub(crate) out: Option<&'a mut TcpStream>,
    pub(crate) sigs: &'a Mutex<HashMap<String, String>>,
    pub(crate) model: String,
    pub(crate) fam: &'static str,
    pub(crate) id: String,
    /// Some content was produced (and, when streaming, sent).
    pub(crate) started: bool,
    pub(crate) blocks: Vec<Value>,
    pub(crate) open: Option<&'static str>,
    /// Signature for the open thinking block (already tagged: `ag:…` or `oa:…`), emitted as `signature_delta` when it closes.
    pub(crate) sig: String,
    /// Leading whitespace held back so an empty answer can still be retried.
    pub(crate) pending: String,
    pub(crate) usage: (u64, u64, u64),
    pub(crate) finish: Option<String>,
    pub(crate) finish_note: String,
    pub(crate) stop_reason: &'static str,
    /// Google Search grounding: the queries run and the pages cited, (url, title), replayed as Anthropic web-search blocks.
    pub(crate) queries: Vec<String>,
    pub(crate) links: Vec<(String, String)>,
    // ---- ChatGPT (Responses API) only
    /// Arguments streamed so far into the open function call.
    pub(crate) args: String,
    /// `response.completed` / `response.incomplete` arrived.
    pub(crate) done: bool,
    pub(crate) max_tokens: bool,
    /// Tools go out in a namespace, so a returned call name may carry its prefix.
    pub(crate) wrap: bool,
    /// What an `unsupported_capability` failure named (`error.param`): the caller may retry without it.
    pub(crate) param: String,
}

impl<'a> Tx<'a> {
    /// `model` is what runs; replies name `shown`, the model Claude Code asked for.
    pub(crate) fn new(model: &str, shown: &str, out: Option<&'a mut TcpStream>, sigs: &'a Mutex<HashMap<String, String>>) -> Self {
        Tx {
            out,
            sigs,
            model: shown.to_string(),
            fam: family(model),
            id: format!("msg_{}", common::random_hex(12)),
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
            args: String::new(),
            done: false,
            max_tokens: false,
            wrap: false,
            param: String::new(),
        }
    }

    pub(crate) fn usage_json(&self) -> Value {
        let (prompt, cached, output) = self.usage;
        let mut u = json!({ "input_tokens": prompt - cached, "cache_read_input_tokens": cached, "cache_creation_input_tokens": 0, "output_tokens": output });
        if !self.queries.is_empty() {
            u["server_tool_use"] = json!({ "web_search_requests": self.queries.len() });
        }
        u
    }

    pub(crate) fn send(&mut self, name: &str, data: Value) -> Result<(), Stop> {
        let Some(s) = self.out.as_deref_mut() else { return Ok(()) };
        write!(s, "event: {name}\ndata: {data}\n\n").and_then(|_| s.flush()).map_err(|_| Stop::Gone)
    }

    pub(crate) fn begin(&mut self) -> Result<(), Stop> {
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

    pub(crate) fn close(&mut self) -> Result<(), Stop> {
        let Some(kind) = self.open.take() else { return Ok(()) };
        let index = self.blocks.len() - 1;
        if kind == "tool" {
            let args = std::mem::take(&mut self.args);
            self.blocks[index]["input"] = serde_json::from_str(&args).ok().filter(Value::is_object).unwrap_or_else(|| json!({}));
        }
        if kind == "thinking" && !self.sig.is_empty() {
            let sig = std::mem::take(&mut self.sig);
            self.blocks[index]["signature"] = json!(sig);
            self.send("content_block_delta", json!({ "type": "content_block_delta", "index": index, "delta": { "type": "signature_delta", "signature": sig } }))?;
        }
        self.send("content_block_stop", json!({ "type": "content_block_stop", "index": index }))
    }

    /// Opens a `text` or `thinking` block unless one of that kind is open.
    pub(crate) fn open_block(&mut self, kind: &'static str) -> Result<(), Stop> {
        if self.open != Some(kind) {
            self.close()?;
            self.begin()?;
            let block = if kind == "text" { json!({ "type": "text", "text": "" }) } else { json!({ "type": "thinking", "thinking": "", "signature": "" }) };
            self.send("content_block_start", json!({ "type": "content_block_start", "index": self.blocks.len(), "content_block": block }))?;
            self.blocks.push(block);
            self.open = Some(kind);
        }
        Ok(())
    }

    pub(crate) fn delta(&mut self, kind: &'static str, text: &str) -> Result<(), Stop> {
        self.open_block(kind)?;
        let index = self.blocks.len() - 1;
        let block = &mut self.blocks[index][kind];
        *block = json!(format!("{}{text}", block.as_str().unwrap_or("")));
        let delta = if kind == "text" { json!({ "type": "text_delta", "text": text }) } else { json!({ "type": "thinking_delta", "thinking": text }) };
        self.send("content_block_delta", json!({ "type": "content_block_delta", "index": index, "delta": delta }))
    }

    pub(crate) fn text(&mut self, text: &str) -> Result<(), Stop> {
        if !self.started && text.trim().is_empty() {
            self.pending.push_str(text);
            return Ok(());
        }
        let text = format!("{}{text}", std::mem::take(&mut self.pending));
        self.delta("text", &text)
    }

    /// A `tool_use` (or `server_tool_use`) block, sent whole: Cloud Code hands over function calls complete.
    pub(crate) fn tool(&mut self, kind: &str, id: String, name: &str, args: Value) -> Result<(), Stop> {
        self.close()?;
        self.begin()?;
        let index = self.blocks.len();
        self.send("content_block_start", json!({ "type": "content_block_start", "index": index, "content_block": { "type": kind, "id": id, "name": name, "input": {} } }))?;
        self.send("content_block_delta", json!({ "type": "content_block_delta", "index": index, "delta": { "type": "input_json_delta", "partial_json": args.to_string() } }))?;
        self.send("content_block_stop", json!({ "type": "content_block_stop", "index": index }))?;
        self.blocks.push(json!({ "type": kind, "id": id, "name": name, "input": args }));
        Ok(())
    }

    /// A function call opens a `tool_use` block; its arguments stream into it.
    pub(crate) fn call_start(&mut self, id: &str, name: &str) -> Result<(), Stop> {
        self.close()?;
        self.begin()?;
        let index = self.blocks.len();
        let block = json!({ "type": "tool_use", "id": id, "name": name, "input": {} });
        self.send("content_block_start", json!({ "type": "content_block_start", "index": index, "content_block": block }))?;
        self.blocks.push(block);
        (self.open, self.args) = (Some("tool"), String::new());
        Ok(())
    }

    pub(crate) fn call_args(&mut self, part: &str) -> Result<(), Stop> {
        if self.open != Some("tool") || part.is_empty() {
            return Ok(());
        }
        self.args.push_str(part);
        let index = self.blocks.len() - 1;
        self.send("content_block_delta", json!({ "type": "content_block_delta", "index": index, "delta": { "type": "input_json_delta", "partial_json": part } }))
    }

    pub(crate) fn heartbeat(&mut self) -> Result<(), Stop> {
        self.begin()?;
        self.send("ping", json!({"type":"ping"}))
    }

    /// OpenAI-compatible Chat Completions SSE stream -> Anthropic events.
    pub(crate) fn run_openai_chat(&mut self, reader: Box<dyn BufRead + Send>, provider: &str) -> Result<(), Stop> {
        let mut in_think = false;
        let mut current_tool_idx: Option<u64> = None;

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => return Err(self.fail(Failure { status: 502, message: format!("{provider} stream broke: {e}"), retry_after: None }, Some(0))),
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with(':') || trimmed.strip_prefix("event:").is_some_and(|event| event.trim() == "ping") {
                self.heartbeat()?;
                continue;
            }
            let Some(d) = trimmed.strip_prefix("data:") else {
                continue;
            };
            let data = d.trim();
            if data == "[DONE]" {
                self.done = true;
                break;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if v["ccat_ping"] == true || v["type"] == "ping" {
                self.heartbeat()?;
                continue;
            }
            if let Some(err) = v.get("error").filter(|e| e.is_object()) {
                let msg = err["message"].as_str().unwrap_or("Upstream chat stream error");
                return Err(self.fail(Failure { status: 500, message: format!("{provider}: {msg}"), retry_after: None }, None));
            }
            if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                let prompt = u["prompt_tokens"].as_u64().unwrap_or(0);
                let comp = u["completion_tokens"].as_u64().unwrap_or(0);
                self.usage = (prompt, 0, comp);
            }
            let Some(choice) = v["choices"].as_array().and_then(|c| c.first()) else {
                continue;
            };
            let delta = &choice["delta"];

            // 1. Thinking / reasoning delta (DeepSeek-R1 / Qwen native field)
            if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                self.delta("thinking", reasoning)?;
            }

            // 2. Text content (with inline <think>...</think> tag handling)
            if let Some(content) = delta.get("content").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                let mut rem = content;
                while !rem.is_empty() {
                    if !in_think {
                        if let Some(start) = rem.find("<think>") {
                            let before = &rem[..start];
                            if !before.is_empty() {
                                self.text(before)?;
                            }
                            in_think = true;
                            rem = &rem[start + 7..];
                        } else {
                            self.text(rem)?;
                            break;
                        }
                    } else if let Some(end) = rem.find("</think>") {
                        let thinking_part = &rem[..end];
                        if !thinking_part.is_empty() {
                            self.delta("thinking", thinking_part)?;
                        }
                        in_think = false;
                        self.close()?;
                        rem = &rem[end + 8..];
                    } else {
                        self.delta("thinking", rem)?;
                        break;
                    }
                }
            }

            // 3. Tool calls
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for tc in tool_calls {
                    let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let tc_id = tc["id"].as_str().filter(|s| !s.is_empty());
                    let func_name = tc["function"]["name"].as_str().filter(|s| !s.is_empty());
                    let func_args = tc["function"]["arguments"].as_str().filter(|s| !s.is_empty());

                    if current_tool_idx != Some(idx) {
                        current_tool_idx = Some(idx);
                        let id = tc_id.map(String::from).unwrap_or_else(|| format!("call_{}", common::random_hex(12)));
                        let name = func_name.unwrap_or("");
                        self.call_start(&id, name)?;
                    } else {
                        if let Some(id) = tc_id {
                            let last = self.blocks.len().saturating_sub(1);
                            if self.open == Some("tool") && !self.blocks.is_empty() {
                                self.blocks[last]["id"] = json!(id);
                            }
                        }
                        if let Some(name) = func_name {
                            let last = self.blocks.len().saturating_sub(1);
                            if self.open == Some("tool") && !self.blocks.is_empty() {
                                self.blocks[last]["name"] = json!(name);
                            }
                        }
                    }
                    if let Some(args) = func_args {
                        self.call_args(args)?;
                    }
                }
            }

            // 4. Finish reason
            if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
                self.done = true;
                self.stop_reason = match finish {
                    "tool_calls" => "tool_use",
                    "length" => "max_tokens",
                    _ => "end_turn",
                };
            }
        }

        self.close()?;
        self.begin()?;
        let (usage, stop) = (self.usage_json(), self.stop_reason);
        self.send("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": stop, "stop_sequence": null }, "usage": usage }))?;
        self.send("message_stop", json!({ "type": "message_stop" }))
    }

    /// Fails the request: retryable while nothing was sent, otherwise an SSE `error` event (overloaded → Claude Code retries).
    pub(crate) fn fail(&mut self, f: Failure, block: Option<u64>) -> Stop {
        if self.started && self.out.is_some() {
            // Overloaded makes Claude Code retry; a client error must not, or it retries a dead request for ever.
            let kind = match f.status {
                429 => "rate_limit_error",
                400..=499 => error_kind(f.status),
                _ => "overloaded_error",
            };
            let _ = self.send("error", json!({ "type": "error", "error": { "type": kind, "message": f.message } }));
            common::log(&format!("{}: failed mid-stream: {}", self.model, f.message));
            return Stop::Gone;
        }
        Stop::Retry(f, block)
    }

    pub(crate) fn message(&self) -> Value {
        json!({
            "id": self.id, "type": "message", "role": "assistant", "model": self.model, "content": self.blocks,
            "stop_reason": self.stop_reason, "stop_sequence": null, "usage": self.usage_json(),
        })
    }

    /// Grounding comes back as metadata, not as a tool call; Claude Code's WebSearch reads it from these two blocks.
    /// ponytail: one search block for all queries, with the cited pages; links are Google's redirect URLs, not resolved.
    pub(crate) fn search_results(&mut self) -> Result<(), Stop> {
        if self.queries.is_empty() && self.links.is_empty() {
            return Ok(());
        }
        if self.queries.is_empty() {
            self.queries.push(String::new());
        }
        let id = format!("srvtoolu_{}", common::random_hex(12));
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[test]
    fn signatures_only_replay_on_their_family() {
        assert_eq!(untag(Some("ag:claude:QUJD"), "claude"), Some("QUJD"));
        assert_eq!(untag(Some("ag:claude:QUJD"), "gemini"), None);
        assert_eq!(untag(Some("EqQBCkgIBh"), "claude"), None, "Anthropic's own signature");
        assert_eq!(untag(Some("ag:claude:not base64!"), "claude"), None);
    }

    #[test]
    fn openai_chat_text_and_thinking_streams() {
        let sigs = Mutex::new(HashMap::new());
        let mut tx = Tx::new("llama3.2", "claude-sonnet-5-5", None, &sigs);
        let sse = "\
data: {\"id\":\"1\",\"choices\":[{\"delta\":{\"reasoning_content\":\"reason step 1\\n\"},\"finish_reason\":null}]}\n\
data: {\"id\":\"1\",\"choices\":[{\"delta\":{\"content\":\"<think>reason step 2</think>Hello \"},\"finish_reason\":null}]}\n\
data: {\"id\":\"1\",\"choices\":[{\"delta\":{\"content\":\"world!\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":15,\"completion_tokens\":8}}\n\
data: [DONE]\n\n";

        let reader: Box<dyn BufRead + Send> = Box::new(BufReader::new(sse.as_bytes()));
        tx.run_openai_chat(reader, "Ollama").unwrap();
        let msg = tx.message();
        assert_eq!(msg["stop_reason"], "end_turn");
        let content = msg["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "reason step 1\nreason step 2");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "Hello world!");
        assert_eq!(tx.usage.0, 15);
        assert_eq!(tx.usage.2, 8);
    }

    #[test]
    fn openai_chat_tool_calls_stream() {
        let sigs = Mutex::new(HashMap::new());
        let mut tx = Tx::new("qwen2.5-coder:32b", "claude-sonnet-5-5", None, &sigs);
        let sse = "\
data: {\"id\":\"1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_99\",\"type\":\"function\",\"function\":{\"name\":\"Bash\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\
data: {\"id\":\"1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"command\\\": \"}}]},\"finish_reason\":null}]}\n\
data: {\"id\":\"1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"ls\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\
data: [DONE]\n\n";

        let reader: Box<dyn BufRead + Send> = Box::new(BufReader::new(sse.as_bytes()));
        tx.run_openai_chat(reader, "Ollama").unwrap();
        let msg = tx.message();
        assert_eq!(msg["stop_reason"], "tool_use");
        let content = msg["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "tool_use");
        assert_eq!(content[0]["id"], "call_99");
        assert_eq!(content[0]["name"], "Bash");
        assert_eq!(content[0]["input"], json!({ "command": "ls" }));
    }
}
