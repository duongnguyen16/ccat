//! Ollama provider: Ollama Cloud (https://ollama.com) and local Ollama model serving for Claude Code.
//! Protocol: translates Anthropic Messages API -> Ollama's native `/api/chat` (NDJSON stream: the OpenAI-compatible
//! `/v1` ignores `num_ctx` and `keep_alive`), quota/balance tracking via `/api/balance`, and web search via `/api/web_search`.

use crate::common::{self, Failure};
use crate::providers::antigravity::proxy::messages;
use crate::proxy::http::{record, respond, respond_error, Req};
use crate::proxy::sse::{Stop, Tx};
use crate::proxy::{Ctx, Fail, SEARCH_MODEL};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader},
    net::TcpStream,
    sync::{LazyLock, Mutex},
    time::Duration,
};

/// Models Ollama answered "does not support thinking" for: `think` is left out of their requests from then on.
/// ponytail: per model name for the process lifetime (not per server), so a same-named model elsewhere that does think
/// loses it until ccat restarts.
static NO_THINK: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

pub const DEFAULT_URL: &str = "https://ollama.com";
pub const _LOCAL_URL: &str = "http://localhost:11434";

/// Normalize base URL ensuring http/https scheme, stripped trailing slashes and any `/v1` saved for the old
/// OpenAI-compatible route (every call here is under `/api`).
pub fn normalize_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    if trimmed.is_empty() {
        return DEFAULT_URL.to_string();
    }
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        if trimmed.starts_with("localhost") || trimmed.starts_with("127.0.0.1") {
            format!("http://{trimmed}")
        } else {
            format!("https://{trimmed}")
        }
    } else {
        trimmed.to_string()
    }
}

/// Fetch list of models from Ollama (Cloud or Local).
/// Tries `/api/tags` first (standard for ollama.com and local); falls back to `/v1/models`.
pub fn list_models(base_url: &str, key: Option<&str>) -> Result<Vec<String>, String> {
    let url = normalize_url(base_url);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();

    let mut req_tags = agent.get(format!("{url}/api/tags"));
    if let Some(k) = key.filter(|k| !k.trim().is_empty()) {
        req_tags = req_tags.header("Authorization", format!("Bearer {k}"));
    }

    if let Ok(mut res) = req_tags.call() {
        if let Ok(body) = res.body_mut().read_to_string() {
            if let Ok(v) = serde_json::from_str::<Value>(&body) {
                if let Some(arr) = v["models"].as_array() {
                    let mut models: Vec<String> = arr
                        .iter()
                        .filter_map(|m| m["name"].as_str().or(m["model"].as_str()).map(crate::bare_id))
                        .collect();
                    if !models.is_empty() {
                        models.sort();
                        models.dedup();
                        return Ok(models);
                    }
                }
            }
        }
    }

    let mut req_v1 = agent.get(format!("{url}/v1/models"));
    if let Some(k) = key.filter(|k| !k.trim().is_empty()) {
        req_v1 = req_v1.header("Authorization", format!("Bearer {k}"));
    }
    let mut res = req_v1.call().map_err(|e| format!("{url}: {e}"))?;
    let body = res.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("response is not JSON: {e}"))?;
    let data_arr = v["data"].as_array().ok_or("response has no `data` or `models` array")?;
    let mut models: Vec<String> = data_arr
        .iter()
        .filter_map(|m| m["id"].as_str().map(crate::bare_id))
        .collect();
    if models.is_empty() {
        return Err("No models found from Ollama.".into());
    }
    models.sort();
    models.dedup();
    Ok(models)
}

/// Fetch quota/balance from Ollama Cloud (`GET /api/balance`).
pub fn fetch_balance(base_url: &str, key: &str) -> Result<Vec<crate::usage::Bar>, String> {
    if key.trim().is_empty() {
        return Ok(Vec::new());
    }
    let url = format!("{}/api/balance", normalize_url(base_url));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();

    let mut res = agent
        .get(&url)
        .header("Authorization", format!("Bearer {key}"))
        .header("Accept", "application/json")
        .call()
        .map_err(|e| format!("{url}: {e}"))?;

    let body = res.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("balance response is not JSON: {e}"))?;
    Ok(parse_balance(&v))
}

pub fn parse_balance(v: &Value) -> Vec<crate::usage::Bar> {
    let mut bars = Vec::new();
    let root = v.as_object();
    let included = root.and_then(|r| r.get("included")).and_then(Value::as_object);

    if let Some(inc) = included {
        let allowance = inc.get("allowance_usd").and_then(Value::as_f64).unwrap_or(0.0);
        let balance = inc.get("balance_usd").and_then(Value::as_f64).unwrap_or(0.0);
        let reset_str = inc.get("period").and_then(|p| p.get("until")).and_then(Value::as_str).unwrap_or("");
        if allowance > 0.0 {
            let used = ((1.0 - balance / allowance) * 100.0).clamp(0.0, 100.0);
            let resets = crate::usage::parse_time(reset_str);
            bars.push(crate::usage::Bar {
                label: "Monthly".into(),
                used,
                resets,
                window: 30 * 86_400,
                part: String::new(),
                ..Default::default()
            });
        }

        for (k, label, window) in [("session", "Session", 5 * 3600), ("weekly", "Weekly", 7 * 86_400)] {
            if let Some(w) = inc.get(k).and_then(Value::as_object) {
                if let Some(rem) = w.get("remaining_percent").and_then(Value::as_f64) {
                    let used = (100.0 - rem).clamp(0.0, 100.0);
                    let resets = w.get("resets_at").and_then(Value::as_str).map_or(0, crate::usage::parse_time);
                    bars.push(crate::usage::Bar {
                        label: label.into(),
                        used,
                        resets,
                        window,
                        part: String::new(),
                        ..Default::default()
                    });
                }
            }
        }
    }

    if let Some(root_obj) = root {
        if let Some(credits) = root_obj.get("purchased").and_then(|p| p.get("balance_usd")).and_then(Value::as_f64) {
            if credits > 0.0 {
                bars.push(crate::usage::Bar {
                    label: "Credits".into(),
                    used: 0.0,
                    resets: 0,
                    window: 0,
                    part: format!("${credits:.2}"),
                    ..Default::default()
                });
            }
        }
    }

    bars
}

/// Query Ollama Cloud's Web Search API (`POST /api/web_search`).
pub fn web_search(base_url: &str, key: &str, query: &str) -> Result<Vec<(String, String)>, String> {
    let url = format!("{}/api/web_search", normalize_url(base_url));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into();

    let mut req = agent.post(&url).header("Content-Type", "application/json");
    if !key.trim().is_empty() {
        req = req.header("Authorization", format!("Bearer {key}"));
    }

    let payload = json!({
        "query": query,
        "max_results": 5
    });

    let mut res = req.send(payload.to_string()).map_err(|e| format!("{url}: {e}"))?;
    let body = res.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("web_search response is not JSON: {e}"))?;
    let results_arr = v["results"].as_array().ok_or("response has no `results` array")?;
    let links: Vec<(String, String)> = results_arr
        .iter()
        .filter_map(|item| {
            let u = item["url"].as_str()?.to_string();
            let t = item["title"].as_str().unwrap_or(&u).to_string();
            Some((u, t))
        })
        .collect();
    Ok(links)
}

/// How long Ollama keeps the model (and its KV cache, the prompt-cache hit) loaded after a request. Default is 5 min.
const KEEP_ALIVE: &str = "30m";
/// Context window, the same on every request: a different `num_ctx` reloads the model and drops its cache.
/// Ollama's default is too small for Claude Code's prompt; only the native API honours this, not `/v1`.
const NUM_CTX: u64 = 65536;

/// Lines injected by Claude Code or Agent SDK that degrade prompt caching or cause issues on local models.
const FILTERED_LINES: [&str; 2] = [
    "x-anthropic-billing-header:",
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.",
];

fn clean_text(text: &str) -> String {
    let mut out = Vec::new();
    for line in text.lines() {
        if !FILTERED_LINES.iter().any(|f| line.starts_with(f)) {
            out.push(line);
        }
    }
    out.join("\n")
}

/// Convert an Anthropic Messages request body to an Ollama native `/api/chat` payload.
pub fn chat_body(body: &Value, model: &str, stream: bool) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    // 1. System prompt
    let mut system_text = String::new();
    if let Some(s) = body["system"].as_str() {
        system_text = clean_text(s);
    } else if let Some(arr) = body["system"].as_array() {
        let parts: Vec<String> = arr
            .iter()
            .filter_map(|b| b["text"].as_str().map(clean_text))
            .filter(|s| !s.is_empty())
            .collect();
        system_text = parts.join("\n");
    }
    if !system_text.trim().is_empty() {
        messages.push(json!({
            "role": "system",
            "content": system_text,
        }));
    }

    // Ollama matches a tool result to its call by function name, not id.
    let names: HashMap<&str, &str> = body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .filter(|b| b["type"] == "tool_use")
        .filter_map(|b| Some((b["id"].as_str()?, b["name"].as_str()?)))
        .collect();

    // 2. Messages
    if let Some(msgs) = body["messages"].as_array() {
        for msg in msgs {
            let role = msg["role"].as_str().unwrap_or("user");
            // Mid-array "system" messages (e.g. from Claude Code hooks) must be demoted to "user"
            let effective_role = if role == "system" && !messages.is_empty() { "user" } else { role };

            if let Some(text) = msg["content"].as_str() {
                messages.push(json!({
                    "role": effective_role,
                    "content": text,
                }));
                continue;
            }

            let Some(blocks) = msg["content"].as_array() else {
                continue;
            };

            if effective_role == "assistant" {
                let mut text_parts = Vec::new();
                let mut tool_calls = Vec::new();

                for b in blocks {
                    match b["type"].as_str() {
                        Some("text") => {
                            if let Some(t) = b["text"].as_str() {
                                text_parts.push(t);
                            }
                        }
                        Some("tool_use") => {
                            tool_calls.push(json!({
                                "id": b["id"].as_str().unwrap_or(""),
                                "type": "function",
                                "function": {
                                    "index": tool_calls.len(),
                                    "name": b["name"].as_str().unwrap_or(""),
                                    // an object here, not the JSON string the OpenAI schema wants
                                    "arguments": b.get("input").cloned().unwrap_or_else(|| json!({})),
                                }
                            }));
                        }
                        _ => {}
                    }
                }

                let mut assistant_msg = json!({ "role": "assistant", "content": text_parts.join("") });
                if !tool_calls.is_empty() {
                    assistant_msg["tool_calls"] = json!(tool_calls);
                }
                messages.push(assistant_msg);
            } else {
                // user turn: can contain text, image, and tool_result blocks
                let mut texts = Vec::new();
                let mut images = Vec::new();
                let mut tool_results = Vec::new();

                for b in blocks {
                    match b["type"].as_str() {
                        Some("text") => {
                            if let Some(t) = b["text"].as_str() {
                                texts.push(t);
                            }
                        }
                        Some("image") => {
                            let src = &b["source"];
                            if src["type"] == "base64" {
                                images.push(src["data"].as_str().unwrap_or(""));
                            }
                        }
                        Some("tool_result") => {
                            let call_id = b["tool_use_id"].as_str().unwrap_or("");
                            let content_str = if let Some(s) = b["content"].as_str() {
                                s.to_string()
                            } else if let Some(arr) = b["content"].as_array() {
                                arr.iter().filter_map(crate::proxy::sse::tool_result_text).collect::<Vec<_>>().join("")
                            } else {
                                b["content"].to_string()
                            };
                            let mut result = json!({ "role": "tool", "tool_call_id": call_id, "content": content_str });
                            if let Some(name) = names.get(call_id) {
                                result["tool_name"] = json!(name);
                            }
                            tool_results.push(result);
                        }
                        _ => {}
                    }
                }

                messages.extend(tool_results);

                if !texts.is_empty() || !images.is_empty() {
                    let mut user = json!({ "role": "user", "content": texts.join("\n") });
                    if !images.is_empty() {
                        user["images"] = json!(images);
                    }
                    messages.push(user);
                }
            }
        }
    }

    let mut payload = json!({
        "model": model,
        "messages": messages,
        "stream": stream,
        "keep_alive": KEEP_ALIVE,
        "options": { "num_ctx": NUM_CTX },
    });

    if let Some(max_tokens) = body.get("max_tokens").and_then(Value::as_u64) {
        payload["options"]["num_predict"] = json!(max_tokens);
    }
    if let Some(temp) = body.get("temperature").and_then(Value::as_f64) {
        payload["options"]["temperature"] = json!(temp);
    }

    // Thinking effort: /api/chat takes only low, medium or high, so Claude Code's xhigh and max ask for high.
    let effort = body["output_config"]["effort"]
        .as_str()
        .or_else(|| body["reasoning_effort"].as_str());
    if let Some(e) = effort {
        let e = e.to_lowercase();
        payload["think"] = json!(if matches!(e.as_str(), "low" | "medium") { e.as_str() } else { "high" });
    } else if let Some(budget) = body["thinking"]["budget_tokens"].as_u64() {
        let e = if budget <= 1024 {
            "low"
        } else if budget <= 10240 {
            "medium"
        } else {
            "high"
        };
        payload["think"] = json!(e);
    }

    // 3. Tools
    if let Some(tools_arr) = crate::proxy::sse::client_tools(body).as_array() {
        let mut functions = Vec::new();
        for t in tools_arr {
            let name = t["name"].as_str().unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let desc = t["description"].as_str().unwrap_or("");
            let params = t.get("input_schema").cloned().unwrap_or_else(|| json!({
                "type": "object",
                "properties": {}
            }));
            functions.push(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": desc,
                    "parameters": params,
                }
            }));
        }
        if !functions.is_empty() {
            payload["tools"] = json!(functions);
        }
    }

    payload
}

/// Open an HTTP connection to Ollama's native `/api/chat` endpoint (NDJSON stream).
pub fn open(base_url: &str, key: Option<&str>, payload: &Value) -> Result<Box<dyn BufRead + Send>, Failure> {
    let url = format!("{}/api/chat", normalize_url(base_url));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(600)))
        // The status and body of a refusal are what the caller acts on (a 429 waits, a 400 may be retried).
        .http_status_as_error(false)
        .build()
        .into();

    let mut req = agent
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/x-ndjson");

    if let Some(k) = key.filter(|k| !k.trim().is_empty()) {
        req = req.header("Authorization", format!("Bearer {k}"));
    }

    let mut res = req.send(payload.to_string()).map_err(|e| Failure {
        status: 502,
        message: format!("Ollama ({url}): {e}"),
        retry_after: None,
    })?;

    let status = res.status().as_u16();
    if status == 200 {
        Ok(Box::new(BufReader::new(res.into_body().into_reader())))
    } else {
        let err_body = res.body_mut().read_to_string().unwrap_or_default();
        Err(Failure {
            status,
            message: format!("Ollama returned {status}: {err_body}"),
            retry_after: None,
        })
    }
}

// ---------------------------------------------------------------- proxy: Anthropic → Ollama /api/chat

impl Tx<'_> {
    /// Ollama's native NDJSON chat stream -> Anthropic events. Tool calls arrive whole; the last line says
    /// `done_reason: "stop"` even after one, so a call seen means `tool_use`.
    pub(crate) fn run_ollama_chat(&mut self, reader: Box<dyn BufRead + Send>) -> Result<(), Stop> {
        let (mut calls, mut done) = (false, false);
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => return Err(self.fail(Failure { status: 502, message: format!("Ollama stream broke: {e}"), retry_after: None }, Some(0))),
            };
            let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if let Some(e) = v["error"].as_str() {
                return Err(self.fail(Failure { status: 500, message: format!("Ollama: {e}"), retry_after: None }, None));
            }
            let m = &v["message"];
            if let Some(t) = m["thinking"].as_str().filter(|t| !t.is_empty()) {
                self.delta("thinking", t)?;
            }
            if let Some(t) = m["content"].as_str().filter(|t| !t.is_empty()) {
                self.text(t)?;
            }
            for tc in m["tool_calls"].as_array().into_iter().flatten() {
                let f = &tc["function"];
                let id = tc["id"].as_str().filter(|s| !s.is_empty()).map_or_else(|| format!("call_{}", common::random_hex(12)), String::from);
                let args = match &f["arguments"] {
                    Value::String(s) => serde_json::from_str(s).ok(),
                    a => Some(a.clone()),
                }
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({}));
                self.tool("tool_use", id, f["name"].as_str().unwrap_or(""), args)?;
                calls = true;
            }
            if v["done"] == true {
                let n = |k: &str| v[k].as_u64().unwrap_or(0);
                // `prompt_eval_count` includes the tokens served from the KV cache (the cached count is a subset).
                let prompt = n("prompt_eval_count");
                self.usage = (prompt, n("prompt_eval_cached_count").min(prompt), n("eval_count"));
                self.stop_reason = if calls { "tool_use" } else if v["done_reason"] == "length" { "max_tokens" } else { "end_turn" };
                done = true;
                break;
            }
        }
        if !done {
            return Err(self.fail(Failure { status: 502, message: "Ollama stream ended before `done`".into(), retry_after: None }, Some(0)));
        }

        self.close()?;
        self.begin()?;
        let (usage, stop) = (self.usage_json(), self.stop_reason);
        self.send("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": stop, "stop_sequence": null }, "usage": usage }))?;
        self.send("message_stop", json!({ "type": "message_stop" }))
    }
}

/// An Ollama route: the request goes to Ollama's native `/api/chat`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ollama_messages(
    s: &mut TcpStream,
    ctx: &Ctx,
    _req: &Req,
    body: &Value,
    asked: &str,
    model: &str,
    url: &str,
    key: &str,
    tier: &str,
    may_fail: Fail,
) -> Result<(), Failure> {
    if model.is_empty() {
        respond_error(s, &Failure { status: 400, message: "no model in request".into(), retry_after: None });
        return Ok(());
    }
    let stream = body["stream"].as_bool().unwrap_or(false);

    // WebSearch support:
    // If an Ollama Cloud API key is available, query Ollama's native web search endpoint!
    // Otherwise, delegate to Gemini Flash if Antigravity is active.
    if crate::proxy::web_search(body).is_some() {
        let q = body["messages"].as_array().and_then(|m| m.last()).and_then(|m| {
            if let Some(s) = m["content"].as_str() {
                Some(s.to_string())
            } else if let Some(arr) = m["content"].as_array() {
                arr.iter().find_map(|b| b["text"].as_str()).map(String::from)
            } else {
                None
            }
        }).unwrap_or_default();

        if !key.trim().is_empty() {
            if let Ok(links) = crate::providers::ollama::web_search(url, key, &q) {
                let mut tx = Tx::new(model, asked, stream.then_some(&mut *s), &ctx.sigs);
                tx.queries = vec![q];
                tx.links = links;
                let _ = tx.search_results();
                let _ = tx.begin();
                let (usage, stop) = (tx.usage_json(), "end_turn");
                let _ = tx.send("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": stop, "stop_sequence": null }, "usage": usage }));
                let _ = tx.send("message_stop", json!({ "type": "message_stop" }));
                if !stream {
                    let msg = tx.message();
                    respond(s, 200, &msg, "");
                }
                return Ok(());
            }
        }
        if !ctx.pool.is_empty() {
            return messages(s, ctx, body, asked, SEARCH_MODEL.to_string(), tier, may_fail);
        }
    }

    let mut payload = crate::providers::ollama::chat_body(body, model, true);
    if NO_THINK.lock().unwrap().contains(model) {
        payload.as_object_mut().map(|o| o.remove("think"));
    }
    let key_opt = (!key.trim().is_empty()).then_some(key);
    let opened = match crate::providers::ollama::open(url, key_opt, &payload) {
        // A model without thinking support rejects `think` with a 400: once more without it, and remembered.
        Err(f) if f.status == 400 && f.message.contains("does not support thinking") && payload.get("think").is_some() => {
            common::log(&format!("{model} via Ollama: no thinking support, retrying without `think`"));
            NO_THINK.lock().unwrap().insert(model.to_string());
            payload.as_object_mut().map(|o| o.remove("think"));
            crate::providers::ollama::open(url, key_opt, &payload)
        }
        r => r,
    };
    let reader = match opened {
        Ok(r) => r,
        Err(f) => {
            common::log(&format!("{model} via Ollama ({url}): {} {}", f.status, f.message));
            if may_fail.takes(f.status) {
                return Err(f);
            }
            respond_error(s, &f);
            return Ok(());
        }
    };

    let mut tx = Tx::new(model, asked, stream.then_some(&mut *s), &ctx.sigs);
    match tx.run_ollama_chat(reader) {
        Ok(()) => {
            let msg = tx.message();
            let prompt = tx.usage.0;
            drop(tx);
            record(tier, crate::providers::OLLAMA_NAME, model, &msg["usage"]);
            ctx.learn("ollama", body, prompt);
            if !stream {
                respond(s, 200, &msg, "");
            }
            Ok(())
        }
        Err(Stop::Retry(f, _)) if may_fail.takes(f.status) => Err(f),
        Err(Stop::Retry(f, _)) => {
            respond_error(s, &f);
            Ok(())
        }
        Err(Stop::Gone) => Ok(()),
    }
}

// ---------------------------------------------------------------- settings page

/// Settings → Ollama: its on/off switch, the URL (local, or Ollama Cloud) and key, and the model list.
pub(crate) fn page(ed: &mut crate::settings::providers::ProvidersEditor, ui: &mut eframe::egui::Ui, cfg: &mut crate::Config, feed: &crate::usage::Feed) {
    use crate::settings::providers::{count, head, name, note, row, INDENT};
    use eframe::egui::{self, RichText, Sense, TextEdit};
    let p = crate::pal(ui);
    let Some(ol) = cfg.providers.iter().position(|p| p.ollama) else { return };
    let pr = &mut cfg.providers[ol];
    let title = pr.name.clone();
    head(ui, |ui| {
        let mut on = !pr.disabled;
        if crate::switch(ui, &mut on, "").on_hover_text("Off: Ollama is not offered in the pickers and not used for routing.").changed() {
            pr.disabled = !on;
        }
        name(ui, &title, on);
    });
    let n = crate::model_count(pr);
    row(ui, |ui, _| {
        ed.fetch_btn(ui, pr);
        count(ui, n);
    });
    if let Some(e) = ed.fetch_errs.get(&pr.name) {
        note(ui, e, p.danger);
    }
    ui.horizontal(|ui| {
        ui.add_space(INDENT);
        ui.label(RichText::new("URL:").size(11.5).color(p.muted));
        let mut u = pr.url.clone();
        let te = ui.add(TextEdit::singleline(&mut u).desired_width(260.0));
        if te.changed() {
            pr.url = u;
        }
        if pr.url.is_empty() {
            pr.url = DEFAULT_URL.into();
        }
    });
    ui.horizontal(|ui| {
        ui.add_space(INDENT);
        ui.label(RichText::new("Key:").size(11.5).color(p.muted));
        let mut k = pr.key.clone();
        let te = ui.add(TextEdit::singleline(&mut k).password(true).desired_width(260.0));
        if te.changed() {
            pr.key = k;
        }
        let link = egui::Label::new(RichText::new("Get API key").size(11.0).color(p.subtle).underline()).sense(Sense::click());
        if ui.add(link).on_hover_text("https://ollama.com/settings/keys").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            crate::open_browser("https://ollama.com/settings/keys");
        }
    });
    if pr.models.is_empty() && pr.custom.is_empty() {
        note(ui, "Click Fetch (↻) to retrieve models from Ollama Cloud or local instance.", p.subtle);
    }
    // Only an Ollama Cloud key has an allowance to show.
    if crate::usage::ollama_active(cfg) {
        feed.limits(ui, 3, "", &feed.usage.ollama.bars, feed.usage.ollama.fetched, &feed.usage.ollama.error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_url() {
        assert_eq!(normalize_url(""), "https://ollama.com");
        assert_eq!(normalize_url("localhost:11434"), "http://localhost:11434");
        assert_eq!(normalize_url("http://127.0.0.1:11434/"), "http://127.0.0.1:11434");
        assert_eq!(normalize_url("ollama.com"), "https://ollama.com");
        assert_eq!(normalize_url("https://my-ollama.internal:11434"), "https://my-ollama.internal:11434");
    }

    #[test]
    fn test_parse_balance_monthly_and_session() {
        let data = json!({
            "included": {
                "allowance_usd": 20.0,
                "balance_usd": 15.0,
                "period": { "until": "2026-11-01T00:00:00Z" },
                "session": { "remaining_percent": 80.0, "resets_at": "2026-10-23T18:00:00Z" },
                "weekly": { "remaining_percent": 50.0, "resets_at": "2026-10-30T00:00:00Z" }
            },
            "purchased": {
                "balance_usd": 5.50
            }
        });
        let bars = parse_balance(&data);
        assert_eq!(bars.len(), 4);
        assert_eq!(bars[0].label, "Monthly");
        assert_eq!(bars[0].used, 25.0);
        assert_eq!(bars[1].label, "Session");
        assert_eq!(bars[1].used, 20.0);
        assert_eq!(bars[2].label, "Weekly");
        assert_eq!(bars[2].used, 50.0);
        assert_eq!(bars[3].label, "Credits");
        assert_eq!(bars[3].part, "$5.50");
    }


    #[test]
    fn url_saved_for_the_v1_route_still_works() {
        assert_eq!(normalize_url("http://localhost:11434/v1"), "http://localhost:11434");
        assert_eq!(normalize_url("https://ollama.com/v1/"), "https://ollama.com");
    }

    #[test]
    fn chat_body_is_native_and_stable() {
        let req = json!({
            "system": [{ "type": "text", "text": "x-anthropic-billing-header: cch=1;" }, { "type": "text", "text": "You are helpful." }],
            "messages": [{ "role": "user", "content": "Hello" }],
            "max_tokens": 1024,
            "thinking": { "type": "enabled", "budget_tokens": 5000 }
        });
        let body = chat_body(&req, "gpt-oss:120b", true);
        assert_eq!(body["model"], "gpt-oss:120b");
        assert_eq!(body["stream"], true);
        assert_eq!(body["keep_alive"], "30m");
        assert_eq!(body["options"]["num_ctx"], 65536);
        assert_eq!(body["options"]["num_predict"], 1024);
        assert_eq!(body["think"], "medium");
        assert!(body.get("max_tokens").is_none() && body.get("reasoning_effort").is_none(), "no OpenAI-only fields");
        assert_eq!(body["messages"], json!([{ "role": "system", "content": "You are helpful." }, { "role": "user", "content": "Hello" }]));
        // Another request must not move what keeps the model loaded.
        let other = chat_body(&json!({ "messages": [{ "role": "user", "content": "x" }], "max_tokens": 5 }), "gpt-oss:120b", true);
        assert_eq!((&other["keep_alive"], &other["options"]["num_ctx"]), (&body["keep_alive"], &body["options"]["num_ctx"]));
        assert!(other.get("think").is_none());
    }

    #[test]
    fn chat_body_tool_round_trip_and_images() {
        let req = json!({
            "messages": [
                { "role": "assistant", "content": [
                    { "type": "text", "text": "Let me run bash" },
                    { "type": "tool_use", "id": "call_1", "name": "Bash", "input": { "command": "ls" } },
                    { "type": "tool_use", "id": "call_2", "name": "Read", "input": { "path": "a" } }
                ] },
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "call_1", "content": "file1.txt" },
                    { "type": "tool_result", "tool_use_id": "call_2", "content": [{ "type": "text", "text": "A" }, { "type": "text", "text": "B" }] },
                    { "type": "text", "text": "what is this?" },
                    { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "QUJD" } }
                ] }
            ],
            "tools": [{ "name": "Bash", "description": "Run bash", "input_schema": { "type": "object", "properties": { "command": { "type": "string" } } } }]
        });
        let body = chat_body(&req, "qwen3", true);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"], "Let me run bash");
        assert_eq!(msgs[0]["tool_calls"][0]["function"]["name"], "Bash");
        assert_eq!(msgs[0]["tool_calls"][0]["function"]["arguments"], json!({ "command": "ls" }), "an object, not a JSON string");
        assert_eq!(msgs[0]["tool_calls"][1]["function"]["index"], 1);
        assert_eq!((msgs[1]["role"].as_str(), msgs[1]["tool_name"].as_str(), msgs[1]["content"].as_str()), (Some("tool"), Some("Bash"), Some("file1.txt")));
        assert_eq!((msgs[2]["tool_name"].as_str(), msgs[2]["content"].as_str()), (Some("Read"), Some("AB")));
        assert_eq!(msgs[3], json!({ "role": "user", "content": "what is this?", "images": ["QUJD"] }), "bare base64, no data: URL");

        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "Bash");
    }

    fn run(ndjson: &str) -> (Value, (u64, u64, u64)) {
        let sigs = std::sync::Mutex::new(HashMap::new());
        let mut tx = Tx::new("qwen3", "claude-sonnet-5-5", None, &sigs);
        tx.run_ollama_chat(Box::new(std::io::Cursor::new(ndjson.to_string()))).unwrap();
        (tx.message(), tx.usage)
    }

    #[test]
    fn ndjson_text_thinking_and_usage() {
        let (msg, usage) = run("\
{\"message\":{\"role\":\"assistant\",\"content\":\"\",\"thinking\":\"hm\"},\"done\":false}\n\
{\"message\":{\"role\":\"assistant\",\"content\":\"\",\"thinking\":\"m\"},\"done\":false}\n\
{\"message\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"done\":false}\n\
{\"message\":{\"role\":\"assistant\",\"content\":\"lo\"},\"done\":false}\n\
{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\",\"prompt_eval_count\":20,\"prompt_eval_cached_count\":12,\"eval_count\":5}\n");
        assert_eq!(msg["stop_reason"], "end_turn");
        let c = msg["content"].as_array().unwrap();
        assert_eq!((c[0]["type"].as_str(), c[0]["thinking"].as_str()), (Some("thinking"), Some("hmm")));
        assert_eq!((c[1]["type"].as_str(), c[1]["text"].as_str()), (Some("text"), Some("Hello")));
        assert_eq!(usage, (20, 12, 5));
        assert_eq!((msg["usage"]["input_tokens"].as_u64(), msg["usage"]["cache_read_input_tokens"].as_u64(), msg["usage"]["output_tokens"].as_u64()), (Some(8), Some(12), Some(5)));
    }

    #[test]
    fn ndjson_tool_call_and_length() {
        let (msg, _) = run("\
{\"message\":{\"role\":\"assistant\",\"content\":\"On it.\"},\"done\":false}\n\
{\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"index\":0,\"name\":\"Bash\",\"arguments\":{\"command\":\"ls\"}}},{\"id\":\"call_7\",\"function\":{\"index\":1,\"name\":\"Read\",\"arguments\":{\"path\":\"a\"}}}]},\"done\":false}\n\
{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\",\"prompt_eval_count\":9,\"eval_count\":3}\n");
        assert_eq!(msg["stop_reason"], "tool_use", "Ollama says `stop` after a call");
        let c = msg["content"].as_array().unwrap();
        assert_eq!(c.len(), 3);
        assert_eq!(c[0]["text"], "On it.");
        assert_eq!((c[1]["type"].as_str(), c[1]["name"].as_str(), &c[1]["input"]), (Some("tool_use"), Some("Bash"), &json!({ "command": "ls" })));
        assert!(c[1]["id"].as_str().unwrap().starts_with("call_"), "an id made up when Ollama sends none");
        assert_eq!(c[2]["id"], "call_7");

        let (msg, _) = run("{\"message\":{\"content\":\"cut\"},\"done\":false}\n{\"message\":{\"content\":\"\"},\"done\":true,\"done_reason\":\"length\",\"prompt_eval_count\":4,\"eval_count\":64}\n");
        assert_eq!(msg["stop_reason"], "max_tokens");
    }

    #[test]
    fn ndjson_cut_short_or_failed_is_an_error() {
        let sigs = std::sync::Mutex::new(HashMap::new());
        let mut tx = Tx::new("qwen3", "m", None, &sigs);
        let cut: Box<dyn BufRead + Send> = Box::new(BufReader::new(&b"{\"message\":{\"content\":\"a\"},\"done\":false}\n"[..]));
        assert!(tx.run_ollama_chat(cut).is_err());
        let mut tx = Tx::new("qwen3", "m", None, &sigs);
        let err: Box<dyn BufRead + Send> = Box::new(BufReader::new(&b"{\"error\":\"model not found\"}\n"[..]));
        assert!(matches!(tx.run_ollama_chat(err), Err(Stop::Retry(f, _)) if f.message.contains("model not found")));
    }
}
