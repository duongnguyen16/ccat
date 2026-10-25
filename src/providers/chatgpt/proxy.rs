//! ChatGPT (Responses API): Claude Code's Anthropic request translated to a Responses request, and the event stream
//! translated back. Sign-in, the account pool and the usage-limit bookkeeping are in the parent module.

use crate::common::{self, Failure};
use crate::providers::chatgpt::{self};
use crate::proxy::http::{record, respond, respond_error, Meta, Req};
use crate::proxy::sse::{blocks, client_tools, tool_result_text, Stop, Tx};
use crate::proxy::{system_parts, web_search, Ctx, Fail};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Value};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    io::BufRead,
    net::TcpStream,
    sync::atomic::Ordering,
};

/// What the incoming HTTP request says beyond its body.
pub(crate) struct Hints {
    /// Claude Code's session id (else a hash of the system prompt): the prompt-cache key, so a session's turns share a cache.
    pub(crate) session: String,
}

pub(crate) fn gpt_hints(req: &Req, body: &Value) -> Hints {
    let session = req.header("x-claude-code-session-id").filter(|s| !s.is_empty()).map_or_else(
        || {
            let mut h = DefaultHasher::new();
            body["system"].to_string().hash(&mut h);
            format!("ccat-{:x}", h.finish())
        },
        String::from,
    );
    Hints { session }
}

/// The reasoning effort to send: Claude Code's `output_config.effort` (low … max), else its thinking budget, else the
/// model's own default (as in Codex); always a level the model takes, and never Codex's "ultra".
pub(crate) fn wanted_effort(req: &Value, meta: Option<&chatgpt::ChatgptModel>) -> String {
    let asked = match req["output_config"]["effort"].as_str() {
        Some(e @ ("low" | "medium" | "high" | "xhigh" | "max")) => Some(e),
        _ => match req["thinking"]["type"].as_str() {
            Some("enabled") => Some(match req["thinking"]["budget_tokens"].as_u64() {
                Some(b) if b < 4096 => "low",
                Some(b) if b < 16384 => "medium",
                Some(_) => "high",
                None => "medium",
            }),
            Some("disabled") => Some("low"),
            _ => None,
        },
    };
    let efforts = meta.map_or(&[][..], |m| &m.efforts[..]);
    match (asked, meta.map(|m| m.default_effort.as_str()).filter(|d| !d.is_empty() && *d != "ultra")) {
        (Some(e), _) => chatgpt::clamp_effort(e, efforts),
        (None, Some(d)) => d.to_string(),
        (None, None) => chatgpt::clamp_effort("medium", efforts),
    }
}

fn input_image(b: &Value) -> Option<Value> {
    let src = &b["source"];
    let url = match src["type"].as_str()? {
        "base64" => format!("data:{};base64,{}", src["media_type"].as_str()?, src["data"].as_str()?),
        "url" => src["url"].as_str()?.to_string(),
        _ => return None,
    };
    Some(json!({ "type": "input_image", "image_url": url }))
}

fn input_file(b: &Value) -> Option<Value> {
    let src = &b["source"];
    match src["type"].as_str()? {
        "base64" => Some(json!({ "type": "input_file", "filename": b["title"].as_str().unwrap_or("document.pdf"), "file_data": format!("data:{};base64,{}", src["media_type"].as_str()?, src["data"].as_str()?) })),
        "text" => Some(json!({ "type": "input_text", "text": src["data"] })),
        _ => None,
    }
}

/// The conversation as Responses `input` items: the full history, every time (nothing is stored upstream).
pub(crate) fn response_input(req: &Value, wrap: bool) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for m in req["messages"].as_array().into_iter().flatten() {
        let content = blocks(&m["content"]);
        if m["role"] == "assistant" {
            let mut parts: Vec<Value> = Vec::new();
            let flush = |out: &mut Vec<Value>, parts: &mut Vec<Value>| {
                if !parts.is_empty() {
                    out.push(json!({ "type": "message", "role": "assistant", "content": std::mem::take(parts) }));
                }
            };
            for b in &content {
                match b["type"].as_str() {
                    Some("text") => parts.extend(b["text"].as_str().filter(|t| !t.trim().is_empty()).map(|t| json!({ "type": "output_text", "text": t }))),
                    // Only reasoning ChatGPT itself produced comes back; anything else (Claude's, Gemini's) is dropped.
                    Some("thinking") => {
                        let held = b["signature"].as_str().and_then(|s| s.strip_prefix("oa:")).and_then(|s| B64.decode(s).ok()).and_then(|v| serde_json::from_slice::<Value>(&v).ok());
                        if let Some(r) = held.filter(|r| r["encrypted_content"].is_string()) {
                            flush(&mut out, &mut parts);
                            out.push(json!({ "type": "reasoning", "id": r["id"], "encrypted_content": r["encrypted_content"], "summary": [] }));
                        }
                    }
                    Some("tool_use") => {
                        flush(&mut out, &mut parts);
                        let args = if b["input"].is_object() { b["input"].to_string() } else { "{}".into() };
                        let mut call = json!({ "type": "function_call", "call_id": b["id"], "name": b["name"], "arguments": args });
                        if wrap {
                            call["namespace"] = json!("claude_code");
                        }
                        out.push(call);
                    }
                    _ => {}
                }
            }
            flush(&mut out, &mut parts);
        } else {
            let (mut results, mut rest) = (Vec::new(), Vec::new());
            for b in &content {
                match b["type"].as_str() {
                    Some("tool_result") => {
                        let (mut text, mut images) = (Vec::new(), Vec::new());
                        for c in blocks(&b["content"]) {
                            match c["type"].as_str() {
                                Some("image") => images.extend(input_image(&c)),
                                _ => text.extend(tool_result_text(&c)),
                            }
                        }
                        let output = if !text.is_empty() { text.join("\n") } else if !images.is_empty() { "(see attached image)".into() } else { String::new() };
                        results.push(json!({ "type": "function_call_output", "call_id": b["tool_use_id"], "output": output }));
                        rest.extend(images);
                    }
                    Some("text") => rest.extend(b["text"].as_str().filter(|t| !t.trim().is_empty()).map(|t| json!({ "type": "input_text", "text": t }))),
                    Some("image") => rest.extend(input_image(b)),
                    Some("document") => rest.extend(input_file(b)),
                    _ => {}
                }
            }
            out.extend(results);
            if !rest.is_empty() {
                out.push(json!({ "type": "message", "role": "user", "content": rest }));
            }
        }
    }
    out
}

/// Client tools as function tools (inside one namespace on the official route, which takes them no other way); a web-search server tool as
/// `web_search`; other server tools have no equivalent and are dropped.
pub(crate) fn response_tools(req: &Value, wrap: bool) -> Vec<Value> {
    let tools = client_tools(req).as_array().cloned().unwrap_or_default();
    let fns: Vec<Value> = tools
        .iter()
        .filter(|t| t.get("input_schema").is_some())
        .map(|t| {
            let mut params = t["input_schema"].clone();
            if let Some(o) = params.as_object_mut() {
                o.remove("$schema");
            }
            json!({ "type": "function", "name": t["name"], "description": t["description"].as_str().unwrap_or(""), "parameters": params, "strict": false })
        })
        .collect();
    let mut out = if wrap && !fns.is_empty() { vec![json!({ "type": "namespace", "name": "claude_code", "description": "Claude Code tools", "tools": fns })] } else { fns };
    if tools.iter().any(|t| t["type"].as_str().is_some_and(|k| k.starts_with("web_search"))) {
        out.push(json!({ "type": "web_search" }));
    }
    out
}

/// The Responses request, built from scratch so no field of Claude Code's can leak through (the plan route rejects a
/// list of them). Always `store: false` and `stream: true`; never a `service_tier` (fast mode is off on purpose).
pub(crate) fn responses_body(req: &Value, model: &str, meta: Option<&chatgpt::ChatgptModel>, hints: &Hints, pool: &chatgpt::Pool, legacy: bool) -> Value {
    // Official: function tools only inside a namespace, from the first request. Legacy (Codex) takes them bare.
    let wrap = !legacy;
    let instructions = system_parts(req).iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n\n");
    let mut body = json!({
        "model": model, "instructions": instructions, "input": response_input(req, wrap), "store": false, "stream": true,
        "include": ["reasoning.encrypted_content"], "reasoning": { "effort": wanted_effort(req, meta), "summary": "auto" },
    });
    let mut tools = response_tools(req, wrap);
    if !tools.is_empty() {
        let choice = &req["tool_choice"];
        body["tool_choice"] = match choice["type"].as_str() {
            Some("any") => json!("required"),
            // ponytail: OpenAI's docs show no namespaced form of a forced function (`{type, name}` only), so on the
            // official route the namespace is narrowed to the forced tool and the choice is `required`: same effect, no
            // shape the API may refuse. Legacy takes the plain form.
            Some("tool") if wrap => {
                let only = |t: &Value| t["name"] == choice["name"];
                let found = tools.iter().any(|t| t["type"] == "namespace" && t["tools"].as_array().is_some_and(|f| f.iter().any(only)));
                if found {
                    tools.retain(|t| t["type"] == "namespace");
                    tools[0]["tools"] = json!(tools[0]["tools"].as_array().into_iter().flatten().filter(|t| only(t)).collect::<Vec<_>>());
                }
                json!("required")
            }
            Some("tool") => json!({ "type": "function", "name": choice["name"] }),
            Some("none") => json!("none"),
            _ => json!("auto"),
        };
        body["tools"] = json!(tools);
    }
    if legacy || !pool.no_cache_key.load(Ordering::Relaxed) {
        body["prompt_cache_key"] = json!(hints.session);
    }
    if legacy {
        // The Codex client sends replayed items without their ids.
        for item in body["input"].as_array_mut().into_iter().flatten().filter(|i| i["type"] == "reasoning") {
            item.as_object_mut().map(|o| o.remove("id"));
        }
        // The Codex backend runs independent tool calls side by side, as the Codex CLI asks it to.
        body["parallel_tool_calls"] = json!(true);
    }
    body
}

/// A ChatGPT route: the request goes to the first usable account, and on a refusal to the next (see `chatgpt::classify`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn chatgpt_messages(s: &mut TcpStream, ctx: &Ctx, req: &Req, body: &Value, asked: &str, model: &str, tier: &str, may_fail: Fail) -> Result<(), Failure> {
    if model.is_empty() {
        respond_error(s, &Failure { status: 400, message: "no model in request".into(), retry_after: None });
        return Ok(());
    }
    let started = std::time::Instant::now();
    let pool = &ctx.gpt;
    let stream = body["stream"].as_bool().unwrap_or(false);
    let meta = pool.models.read().unwrap().iter().find(|m| m.slug == model).cloned();
    let hints = gpt_hints(req, body);
    let mut last: Option<Failure> = None;
    let mut turns = pool.accounts.read().unwrap().len() + 2;
    while turns > 0 {
        turns -= 1;
        let Some(acc) = pool.pick() else {
            last = Some(match last.take() {
                Some(f) if f.status != 429 => f,
                _ => pool.exhausted(),
            });
            break;
        };
        // The mode is read once per attempt: body, route and tool names must agree even if Settings flips it meanwhile.
        let legacy = pool.legacy();
        let payload = responses_body(body, model, meta.as_ref(), &hints, pool, legacy);
        // (what an unsupported_capability named, seconds to block the account, the failure)
        let (refused, block, failure) = match pool.open(&acc, &payload, &hints.session, legacy) {
            Err(d) => (d.param, d.block, d.failure),
            Ok(reader) => {
                let mut tx = Tx::new(model, asked, stream.then_some(&mut *s), &ctx.sigs);
                tx.wrap = !legacy;
                match tx.run_responses(reader) {
                    Ok(()) => {
                        let msg = tx.message();
                        let prompt = tx.usage.0;
                        drop(tx);
                        let acct = if acc.email.is_empty() { acc.key.chars().take(8).collect() } else { acc.email.split('@').next().unwrap_or_default().to_string() };
                        record(tier, "ChatGPT", model, &msg["usage"], Meta { started, stop: msg["stop_reason"].as_str(), acct: &acct });
                        if web_search(body).is_none() {
                            ctx.learn("gpt", body, prompt);
                        }
                        if !stream {
                            respond(s, 200, &msg, "");
                        }
                        return Ok(());
                    }
                    Err(Stop::Retry(f, block)) => (tx.param.clone(), block, f),
                    Err(Stop::Gone) => return Ok(()),
                }
            }
        };
        common::log(&format!("{model} via {}: {} {}", acc.email, failure.status, failure.message));
        // An optional field the official API doesn't take: drop it for the rest of this process and try again.
        let flag = match refused.as_str() {
            p if !legacy && p.contains("prompt_cache_key") => Some(&pool.no_cache_key),
            _ => None,
        };
        if let Some(f) = flag.filter(|f| !f.load(Ordering::Relaxed)) {
            f.store(true, Ordering::Relaxed);
            turns += 1;
            continue;
        }
        if let Some(secs) = block {
            pool.block(&acc.key, secs, &failure);
        }
        last = Some(failure);
        if block.is_none() {
            break;
        }
    }
    match last {
        Some(f) if may_fail.takes(f.status) => Err(f),
        Some(f) => Ok(respond_error(s, &f)),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------- Responses events → Anthropic SSE

impl<'a> Tx<'a> {
    // ---- ChatGPT: Responses API events → Anthropic. Nothing is written until the first output event, so a failure
    // before it is still a retry (account rotation, the tier's fallbacks).

    /// The model may hand back a name still carrying the namespace its tools were sent in.
    fn call_name(&self, name: &str) -> String {
        let rest = name.strip_prefix("claude_code").filter(|_| self.wrap);
        rest.and_then(|r| r.strip_prefix(['.', '/', ':']).or_else(|| r.strip_prefix("__"))).unwrap_or(name).to_string()
    }

    fn cite(&mut self, a: &Value) {
        if let Some(url) = a["url"].as_str().filter(|u| a["type"] == "url_citation" && !self.links.iter().any(|(x, _)| x == u)) {
            self.links.push((url.to_string(), a["title"].as_str().filter(|t| !t.is_empty()).unwrap_or(url).to_string()));
        }
    }

    fn item_done(&mut self, item: &Value) -> Result<(), Stop> {
        match item["type"].as_str() {
            Some("reasoning") => {
                self.open_block("thinking")?;
                if let Some(enc) = item["encrypted_content"].as_str().filter(|e| !e.is_empty()) {
                    self.sig = format!("oa:{}", B64.encode(json!({ "id": item["id"], "encrypted_content": enc }).to_string()));
                }
            }
            Some("function_call") => {
                if self.open != Some("tool") {
                    // Never announced (no `added` event): open it now, whole.
                    let id = item["call_id"].as_str().or(item["id"].as_str()).unwrap_or("");
                    self.call_start(id, &self.call_name(item["name"].as_str().unwrap_or("")))?;
                }
                if self.args.is_empty() {
                    self.call_args(item["arguments"].as_str().unwrap_or(""))?;
                }
            }
            Some("web_search_call") => {
                let a = &item["action"];
                for q in a["query"].as_str().into_iter().chain(a["queries"].as_array().into_iter().flatten().filter_map(Value::as_str)) {
                    if !q.is_empty() && !self.queries.iter().any(|x| x == q) {
                        self.queries.push(q.to_string());
                    }
                }
            }
            Some("message") => {
                for a in item["content"].as_array().into_iter().flatten().flat_map(|c| c["annotations"].as_array().into_iter().flatten()) {
                    self.cite(a);
                }
            }
            _ => {}
        }
        self.close()
    }

    /// A failure the API reported, before the stream (`status`) or inside it (0).
    fn gpt_fail(&mut self, status: u16, v: &Value) -> Stop {
        let (code, message, param) = chatgpt::error_fields(v);
        let d = chatgpt::with_reset(chatgpt::classify(status, &code, if message.is_empty() { "the request failed" } else { &message }, &param), v);
        self.param = d.param;
        self.fail(d.failure, d.block)
    }

    fn event(&mut self, v: &Value) -> Result<(), Stop> {
        let s = |k: &str| v[k].as_str().unwrap_or("");
        match s("type") {
            "response.output_item.added" if v["item"]["type"] == "function_call" => {
                let i = &v["item"];
                let id = i["call_id"].as_str().or(i["id"].as_str()).filter(|x| !x.is_empty()).map_or_else(|| format!("toolu_{}", common::random_hex(12)), String::from);
                self.call_start(&id, &self.call_name(i["name"].as_str().unwrap_or("")))
            }
            "response.output_text.delta" => self.text(s("delta")),
            "response.reasoning_summary_text.delta" => self.delta("thinking", s("delta")),
            "response.reasoning_summary_part.added" if v["summary_index"].as_u64().unwrap_or(0) > 0 && self.open == Some("thinking") => self.delta("thinking", "\n\n"),
            "response.function_call_arguments.delta" => self.call_args(s("delta")),
            "response.output_item.done" => self.item_done(&v["item"]),
            "response.output_text.annotation.added" => {
                self.cite(&v["annotation"]);
                Ok(())
            }
            "response.completed" | "response.incomplete" => {
                let u = &v["response"]["usage"];
                let n = |x: &Value| x.as_u64().unwrap_or(0);
                let input = n(&u["input_tokens"]);
                self.usage = (input, n(&u["input_tokens_details"]["cached_tokens"]).min(input), n(&u["output_tokens"]));
                self.done = true;
                if s("type") == "response.incomplete" {
                    match v["response"]["incomplete_details"]["reason"].as_str().unwrap_or("") {
                        "max_output_tokens" => self.max_tokens = true,
                        other => return Err(self.fail(Failure { status: 400, message: format!("Codex stopped generating ({other})"), retry_after: None }, None)),
                    }
                }
                Ok(())
            }
            "response.failed" => Err(self.gpt_fail(0, v)),
            "error" => Err(self.gpt_fail(0, v)),
            _ => Ok(()),
        }
    }

    fn run_responses(&mut self, reader: Box<dyn BufRead + Send>) -> Result<(), Stop> {
        let mut data = String::new();
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => return Err(self.fail(Failure { status: 502, message: format!("Codex stream broke: {e}"), retry_after: None }, Some(0))),
            };
            if let Some(d) = line.strip_prefix("data:") {
                data.push_str(d.trim_start());
            } else if line.trim().is_empty() && !data.is_empty() {
                let v: Value = serde_json::from_str(&std::mem::take(&mut data)).unwrap_or_default();
                self.event(&v)?;
            }
        }
        if !data.is_empty() {
            let v: Value = serde_json::from_str(&data).unwrap_or_default();
            self.event(&v)?;
        }
        if !self.done {
            return Err(self.fail(Failure { status: 502, message: "Codex stream ended before the response finished".into(), retry_after: None }, Some(0)));
        }
        if !self.started && self.queries.is_empty() && self.links.is_empty() {
            return Err(self.fail(Failure { status: 529, message: "Codex returned an empty response".into(), retry_after: None }, Some(0)));
        }
        let any_tool = self.blocks.iter().any(|b| b["type"] == "tool_use");
        self.stop_reason = if any_tool { "tool_use" } else if self.max_tokens { "max_tokens" } else { "end_turn" };
        self.search_results()?;
        self.close()?;
        self.begin()?;
        let (usage, stop) = (self.usage_json(), self.stop_reason);
        self.send("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": stop, "stop_sequence": null }, "usage": usage }))?;
        self.send("message_stop", json!({ "type": "message_stop" }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::tests::model;

    #[test]
    fn effort_follows_the_models_own_levels() {
        let with = |efforts: &[&str], default: &str| Some(model("m", efforts, default));
        let req = |e: &str| json!({ "output_config": { "effort": e } });
        let full = with(&["low", "medium", "high", "xhigh"], "low");
        assert_eq!(wanted_effort(&req("max"), full.as_ref()), "xhigh", "no max on this model: the nearest below");
        assert_eq!(wanted_effort(&req("high"), full.as_ref()), "high", "1:1 when supported");
        assert_eq!(wanted_effort(&json!({}), full.as_ref()), "low", "nothing asked: the model's default, as in Codex");
        let top = with(&["low", "medium", "high", "xhigh", "max", "ultra"], "low");
        assert_eq!(wanted_effort(&req("max"), top.as_ref()), "max");
        assert_ne!(wanted_effort(&json!({ "output_config": { "effort": "ultra" } }), top.as_ref()), "ultra", "Codex's task delegation is never sent");
        assert_eq!(wanted_effort(&json!({}), with(&["low", "ultra"], "ultra").as_ref()), "low", "an ultra default is not sent");
        let gappy = with(&["low", "high"], "high");
        assert_eq!(wanted_effort(&req("medium"), gappy.as_ref()), "low", "round down first");
        assert_eq!(wanted_effort(&req("low"), with(&["medium", "high"], "medium").as_ref()), "medium", "then up");
        // The thinking budget, clamped to the same list.
        let budget = |b: u64| json!({ "thinking": { "type": "enabled", "budget_tokens": b } });
        assert_eq!((wanted_effort(&budget(3000), None), wanted_effort(&budget(8000), None), wanted_effort(&budget(30000), None)), ("low".into(), "medium".into(), "high".into()));
        assert_eq!(wanted_effort(&budget(30000), with(&["low", "medium"], "low").as_ref()), "medium");
        assert_eq!(wanted_effort(&json!({ "thinking": { "type": "disabled" } }), None), "low");
        assert_eq!((wanted_effort(&json!({}), None), wanted_effort(&req("max"), None)), ("medium".into(), "xhigh".into()), "nothing known about the model");
    }
}
