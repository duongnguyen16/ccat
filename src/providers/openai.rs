//! An OpenAI-compatible endpoint (NVIDIA NIM, OpenRouter, Groq, Gemini's compat layer, ...): Anthropic Messages
//! translated to Chat Completions (`{url}/chat/completions`, streamed) and back through `Tx::run_openai_chat`.
//! The URL is the API base including its version (`https://integrate.api.nvidia.com/v1`), as these providers publish it.

use crate::common::{self, Failure};
use crate::proxy::http::{record, respond, respond_error, Meta};
use crate::proxy::sse::{Stop, Tx};
use crate::providers::opencode;
use crate::proxy::{Ctx, Fail};
use serde_json::{json, Value};
use std::{io::{BufRead, BufReader}, net::TcpStream, time::Duration};

/// A provider people commonly add, filled in from the "Add endpoint" page.
pub(crate) struct Preset {
    pub name: &'static str,
    pub url: &'static str,
    /// Where the key is made.
    pub keys: &'static str,
    pub openai: bool,
    /// Filled in as the key; empty: the user brings one.
    pub key: &'static str,
}

/// Base URLs as the providers document them for OpenAI-compatible clients.
pub(crate) const PRESETS: &[Preset] = &[
    Preset { name: "NVIDIA NIM", url: "https://integrate.api.nvidia.com/v1", keys: "https://build.nvidia.com/settings/api-keys", openai: true, key: "" },
    Preset { name: "OpenRouter", url: "https://openrouter.ai/api/v1", keys: "https://openrouter.ai/keys", openai: true, key: "" },
    Preset { name: "Groq", url: "https://api.groq.com/openai/v1", keys: "https://console.groq.com/keys", openai: true, key: "" },
    Preset { name: "Google AI Studio", url: "https://generativelanguage.googleapis.com/v1beta/openai", keys: "https://aistudio.google.com/apikey", openai: true, key: "" },
    Preset { name: "Cerebras", url: "https://api.cerebras.ai/v1", keys: "https://cloud.cerebras.ai/", openai: true, key: "" },
    Preset { name: "Mistral", url: "https://api.mistral.ai/v1", keys: "https://console.mistral.ai/api-keys", openai: true, key: "" },
    Preset { name: "OpenCode Zen", url: "https://opencode.ai/zen/v1", keys: "https://opencode.ai/auth", openai: true, key: "" },
    Preset { name: "OpenCode Go", url: "https://opencode.ai/zen/go/v1", keys: "https://opencode.ai/auth", openai: true, key: "" },
];

/// A model id that runs on a provider's free tier by its naming (OpenRouter's `:free`, the `-free` suffix others use).
pub(crate) fn is_free(id: &str) -> bool {
    id.ends_with(":free") || id.ends_with("-free") || id.ends_with("/free")
}

fn base(url: &str) -> &str {
    url.trim().trim_end_matches('/')
}

/// `{ data: [{ id }] }` from `{url}/models`.
pub(crate) fn fetch_models(url: &str, key: &str) -> Result<Vec<String>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).build().into();
    let mut req = agent.get(format!("{}/models", base(url)));
    if !key.trim().is_empty() {
        req = req.header("authorization", format!("Bearer {}", key.trim()));
    }
    let mut res = req.call().map_err(|e| e.to_string())?;
    let body = res.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("response is not JSON: {e}"))?;
    let data = v["data"].as_array().ok_or("response has no `data` array")?;
    // Ids as given: OpenRouter's `anthropic/…` is a real id here, not a prefix to strip.
    let mut ids: Vec<String> = data.iter().filter_map(|m| m["id"].as_str().map(|s| s.trim().to_string())).collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(crate::proxy::sse::tool_result_text).collect::<Vec<_>>().join(""),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

/// Anthropic Messages request -> Chat Completions request (always streamed, usage on the last chunk).
/// ponytail: no `max_tokens` (Claude Code asks for more than most of these models allow, a 400) and no reasoning effort
/// (non-reasoning models reject it); add per provider when one needs them.
pub(crate) fn chat_body(body: &Value, model: &str) -> Value {
    let mut messages = Vec::new();
    let system = match &body["system"] {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    };
    if !system.trim().is_empty() {
        messages.push(json!({ "role": "system", "content": system }));
    }
    for msg in body["messages"].as_array().into_iter().flatten() {
        let role = if msg["role"] == "assistant" { "assistant" } else { "user" };
        let Some(blocks) = msg["content"].as_array() else {
            messages.push(json!({ "role": role, "content": text_of(&msg["content"]) }));
            continue;
        };
        if role == "assistant" {
            let text: String = blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
            let calls: Vec<Value> = blocks.iter().filter(|b| b["type"] == "tool_use").map(|b| json!({
                "id": b["id"],
                "type": "function",
                "function": { "name": b["name"], "arguments": b.get("input").unwrap_or(&json!({})).to_string() },
            })).collect();
            // A turn that held only thinking (or was cut off) says nothing: strict servers reject an empty one.
            if text.is_empty() && calls.is_empty() {
                continue;
            }
            let mut m = json!({ "role": "assistant", "content": text });
            if !calls.is_empty() {
                m["tool_calls"] = json!(calls);
            }
            messages.push(m);
            continue;
        }
        // A user turn: tool results go first as `tool` messages (they answer the previous assistant turn), then
        // its text and images as one user message.
        let mut parts = Vec::new();
        for b in blocks {
            match b["type"].as_str() {
                Some("tool_result") => messages.push(json!({ "role": "tool", "tool_call_id": b["tool_use_id"], "content": text_of(&b["content"]) })),
                Some("text") => parts.push(json!({ "type": "text", "text": b["text"] })),
                Some("image") if b["source"]["type"] == "base64" => {
                    let url = format!("data:{};base64,{}", b["source"]["media_type"].as_str().unwrap_or("image/png"), b["source"]["data"].as_str().unwrap_or(""));
                    parts.push(json!({ "type": "image_url", "image_url": { "url": url } }));
                }
                _ => {}
            }
        }
        if parts.iter().all(|p| p["type"] == "text") {
            let text: Vec<&str> = parts.iter().filter_map(|p| p["text"].as_str()).collect();
            if !text.is_empty() {
                messages.push(json!({ "role": "user", "content": text.join("\n") }));
            }
        } else {
            messages.push(json!({ "role": "user", "content": parts }));
        }
    }
    let mut payload = json!({ "model": model, "messages": messages, "stream": true, "stream_options": { "include_usage": true } });
    // Client tools only: Anthropic server tools (`web_search_…`) carry a `type` and no schema.
    let tools: Vec<Value> = crate::proxy::sse::client_tools(body).as_array().into_iter().flatten()
        .filter(|t| t["type"].is_null() || t["type"] == "custom")
        .filter_map(|t| Some(json!({ "type": "function", "function": {
            "name": t["name"].as_str().filter(|n| !n.is_empty())?,
            "description": t["description"].as_str().unwrap_or(""),
            "parameters": t.get("input_schema").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
        } })))
        .collect();
    if !tools.is_empty() {
        payload["tool_choice"] = match (body["tool_choice"]["type"].as_str(), body["tool_choice"]["name"].as_str()) {
            (Some("any"), _) => json!("required"),
            (Some("tool"), Some(name)) => json!({ "type": "function", "function": { "name": name } }),
            (Some("none"), _) => json!("none"),
            _ => json!("auto"),
        };
        payload["tools"] = json!(tools);
    }
    if let Some(t) = body["temperature"].as_f64() {
        payload["temperature"] = json!(t);
    }
    payload
}

fn open(url: &str, key: &str, payload: &Value, headers: Vec<(&str, String)>) -> Result<Box<dyn BufRead + Send>, Failure> {
    let url = format!("{}/chat/completions", base(url));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(600)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut req = agent.post(&url).header("Content-Type", "application/json");
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("accept")) {
        req = req.header("Accept", "text/event-stream");
    }
    if !key.trim().is_empty() {
        req = req.header("Authorization", format!("Bearer {}", key.trim()));
    }
    for (k, v) in headers {
        req = req.header(k, v);
    }
    let res = req.send(payload.to_string()).map_err(|e| Failure { status: 502, message: format!("{url}: {e}"), retry_after: None })?;
    let status = res.status().as_u16();
    if status == 200 {
        return Ok(Box::new(BufReader::new(res.into_body().into_reader())));
    }
    let retry_after = res.headers().get("retry-after").and_then(|v| v.to_str().ok()?.trim().parse().ok());
    Err(Failure { status, message: format!("Upstream returned HTTP {status}."), retry_after })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn messages(s: &mut TcpStream, ctx: &Ctx, request: &crate::proxy::http::Req, body: &Value, asked: &str, name: &str, model: &str, url: &str, key: &str, free: bool, egress: Option<&opencode::egress::Config>, tier: &str, may_fail: Fail) -> Result<(), Failure> {
    let started = std::time::Instant::now();
    let stream = body["stream"].as_bool().unwrap_or(false);
    let mut payload = chat_body(body, model);
    let offered = opencode::offered(&payload);
    if free {
        opencode::shape(&mut payload, body);
    }
    let default_egress = opencode::egress::Config::default();
    let conversation = request.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("x-claude-code-session-id"))
        .map(|(_, v)| v.as_str()).unwrap_or(&ctx.agent_id);
    let scope = format!("{name}:{}", ctx.token);
    let opened = if free {
        opencode::egress::open(url, &payload, opencode::headers(), egress.unwrap_or(&default_egress), &scope, conversation)
    } else { open(url, key, &payload, Vec::new()) };
    let reader = match opened {
        Ok(r) if free => Box::new(BufReader::new(opencode::Calls::new(r, offered))),
        Ok(r) => r,
        Err(f) => {
            common::log(&format!("{model} via {name}: {} {}", f.status, f.message));
            if may_fail.takes(f.status) {
                return Err(f);
            }
            respond_error(s, &f);
            return Ok(());
        }
    };
    let mut tx = Tx::new(model, asked, stream.then_some(&mut *s), &ctx.sigs);
    match tx.run_openai_chat(reader, name) {
        Ok(()) => {
            let msg = tx.message();
            let prompt = tx.usage.0;
            drop(tx);
            record(tier, name, model, &msg["usage"], Meta { started, stop: msg["stop_reason"].as_str(), acct: "" });
            // The free tier's ~7k tokens of preamble and manifest would skew the ratio other endpoints estimate by.
            if !free {
                ctx.learn("openai", body, prompt);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_round_trip_and_images_become_chat_completions() {
        let body = json!({
            "system": [{ "type": "text", "text": "be brief" }],
            "tools": [
                { "name": "Read", "description": "read a file", "input_schema": { "type": "object", "properties": { "path": { "type": "string" } } } },
                { "type": "web_search_20250305", "name": "web_search" },
            ],
            "tool_choice": { "type": "any" },
            "messages": [
                { "role": "user", "content": "open a.txt" },
                { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "hm" },
                    { "type": "text", "text": "Reading." },
                    { "type": "tool_use", "id": "call_1", "name": "Read", "input": { "path": "a.txt" } },
                ] },
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "call_1", "content": [{ "type": "text", "text": "hello" }] },
                    { "type": "text", "text": "and this" },
                    { "type": "image", "source": { "type": "base64", "media_type": "image/jpeg", "data": "AAA" } },
                ] },
            ],
        });
        let p = chat_body(&body, "meta/llama");
        let m = p["messages"].as_array().unwrap();
        assert_eq!((m[0]["role"].as_str(), m[0]["content"].as_str()), (Some("system"), Some("be brief")));
        assert_eq!(m[1]["content"], "open a.txt");
        assert_eq!(m[2]["content"], "Reading.", "thinking is dropped");
        assert_eq!(m[2]["tool_calls"][0]["function"]["arguments"], r#"{"path":"a.txt"}"#, "arguments as a JSON string");
        assert_eq!((m[3]["role"].as_str(), m[3]["tool_call_id"].as_str(), m[3]["content"].as_str()), (Some("tool"), Some("call_1"), Some("hello")));
        assert_eq!(m[4]["content"][1]["image_url"]["url"], "data:image/jpeg;base64,AAA");
        assert_eq!(p["tools"].as_array().unwrap().len(), 1, "server tools are not offered");
        assert_eq!(p["tool_choice"], "required");
        assert_eq!((p["stream"].as_bool(), p.get("max_tokens")), (Some(true), None));
    }

    #[test]
    fn parallel_calls_without_index_stay_apart_and_end_as_tool_use() {
        let sse = [
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"a","function":{"name":"Read","arguments":"{\"path\":\"1\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"b","function":{"name":"Read","arguments":"{\"path\":\"2\"}"}}]},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ].iter().map(|e| format!("data: {e}\n\n")).collect::<String>();
        let sigs = std::sync::Mutex::new(std::collections::HashMap::new());
        let mut tx = Tx::new("gemini", "asked", None, &sigs);
        assert!(tx.run_openai_chat(Box::new(std::io::Cursor::new(sse.into_bytes())), "Gemini").is_ok());
        let msg = tx.message();
        let calls: Vec<(&str, &str)> = msg["content"].as_array().unwrap().iter().map(|b| (b["id"].as_str().unwrap(), b["input"]["path"].as_str().unwrap())).collect();
        assert_eq!(calls, [("a", "1"), ("b", "2")]);
        assert_eq!(msg["stop_reason"], "tool_use", "\"stop\" after tool calls");
    }

    #[test]
    fn upstream_error_bodies_never_reach_logs_or_clients() {
        use std::{io::Write, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            crate::proxy::http::read_request(&s).unwrap();
            let body = "private-token-and-user-prompt";
            write!(s, "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 60\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let failure = match open(&url, "key", &json!({}), Vec::new()) {
            Err(f) => f,
            Ok(_) => panic!("upstream rejection must fail"),
        };
        assert_eq!(failure.status, 429);
        assert_eq!(failure.retry_after, Some(60));
        assert_eq!(failure.message, "Upstream returned HTTP 429.");
        server.join().unwrap();
    }

    #[test]
    fn free_models_by_name() {
        assert!(is_free("deepseek/deepseek-v4:free") && is_free("big-pickle-free") && is_free("kilo-auto/free"));
        assert!(!is_free("meta/llama-3.3-70b-instruct") && !is_free("freeform"));
    }
    #[test]
    #[ignore]
    fn live_free_tier() {
        let body = json!({ "system": "be brief", "max_tokens": 32_000, "messages": [{ "role": "user", "content": "Reply with exactly OK1 and nothing else." }], "tools": [{ "name": "Read", "input_schema": { "type": "object" } }] });
        let mut p = chat_body(&body, "deepseek-v4.1-flash-free");
        opencode::shape(&mut p, &body);
        let config = opencode::egress::Config { read_timeout_seconds: 30, ..Default::default() };
        let reader = opencode::egress::open(opencode::URL, &p, opencode::headers(), &config, "live-contract", "synthetic")
            .unwrap_or_else(|f| panic!("{} {:?} {}", f.status, f.retry_after, f.message));
        let reader = BufReader::new(opencode::Calls::new(reader, vec!["Read".into()]));
        let sigs = std::sync::Mutex::new(std::collections::HashMap::new());
        let mut tx = Tx::new("deepseek-v4.1-flash-free", "asked", None, &sigs);
        tx.run_openai_chat(Box::new(reader), "OpenCode Free").unwrap();
        let message = tx.message();
        let text: String = message["content"].as_array().unwrap().iter().filter_map(|b| b["text"].as_str()).collect();
        assert!(text.contains("OK1"), "the real free model must reply: {text}");
    }

}
