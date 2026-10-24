//! Ollama provider: Ollama Cloud (https://ollama.com) and local Ollama model serving for Claude Code.
//! Protocol: translates Anthropic Messages API -> Ollama `/v1/chat/completions`, streaming SSE chunks,
//! quota/balance tracking via `/api/balance`, and web search via `/api/web_search`.

use crate::antigravity::Failure;
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader},
    time::Duration,
};

pub const DEFAULT_URL: &str = "https://ollama.com";
pub const _LOCAL_URL: &str = "http://localhost:11434";

/// Normalize base URL ensuring http/https scheme and stripped trailing slashes.
pub fn normalize_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
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

/// Convert an Anthropic Messages request body to an OpenAI Chat Completions payload for Ollama.
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
                            let id = b["id"].as_str().unwrap_or("");
                            let name = b["name"].as_str().unwrap_or("");
                            let input = b.get("input").cloned().unwrap_or_else(|| json!({}));
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": input.to_string(),
                                }
                            }));
                        }
                        _ => {}
                    }
                }

                let mut assistant_msg = json!({ "role": "assistant" });
                if !text_parts.is_empty() {
                    assistant_msg["content"] = json!(text_parts.join(""));
                } else if tool_calls.is_empty() {
                    assistant_msg["content"] = json!("");
                }
                if !tool_calls.is_empty() {
                    assistant_msg["tool_calls"] = json!(tool_calls);
                }
                messages.push(assistant_msg);
            } else {
                // user turn: can contain text, image, and tool_result blocks
                let mut user_content_parts = Vec::new();
                let mut tool_results = Vec::new();

                for b in blocks {
                    match b["type"].as_str() {
                        Some("text") => {
                            if let Some(t) = b["text"].as_str() {
                                user_content_parts.push(json!({ "type": "text", "text": t }));
                            }
                        }
                        Some("image") => {
                            let src = &b["source"];
                            if src["type"] == "base64" {
                                let media = src["media_type"].as_str().unwrap_or("image/png");
                                let data = src["data"].as_str().unwrap_or("");
                                user_content_parts.push(json!({
                                    "type": "image_url",
                                    "image_url": { "url": format!("data:{media};base64,{data}") }
                                }));
                            }
                        }
                        Some("tool_result") => {
                            let call_id = b["tool_use_id"].as_str().unwrap_or("");
                            let content_str = if let Some(s) = b["content"].as_str() {
                                s.to_string()
                            } else if let Some(arr) = b["content"].as_array() {
                                arr.iter().filter_map(crate::proxy::tool_result_text).collect::<Vec<_>>().join("")
                            } else {
                                b["content"].to_string()
                            };
                            tool_results.push((call_id.to_string(), content_str));
                        }
                        _ => {}
                    }
                }

                // In OpenAI schema, tool responses are separate messages with role: "tool"
                for (call_id, res_text) in tool_results {
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": res_text,
                    }));
                }

                if !user_content_parts.is_empty() {
                    if user_content_parts.len() == 1 && user_content_parts[0]["type"] == "text" {
                        messages.push(json!({
                            "role": "user",
                            "content": user_content_parts[0]["text"].as_str().unwrap_or(""),
                        }));
                    } else {
                        messages.push(json!({
                            "role": "user",
                            "content": user_content_parts,
                        }));
                    }
                }
            }
        }
    }

    let mut payload = json!({
        "model": model,
        "messages": messages,
        "stream": stream,
        "options": {
            // Ollama default is 2048 which truncates Claude Code context; 65536 gives plenty of room
            "num_ctx": 65536,
        }
    });

    if let Some(max_tokens) = body.get("max_tokens").and_then(Value::as_u64) {
        payload["max_tokens"] = json!(max_tokens);
    }
    if let Some(temp) = body.get("temperature").and_then(Value::as_f64) {
        payload["options"]["temperature"] = json!(temp);
    }

    // Thinking effort (low, medium, high, max)
    let effort = body["output_config"]["effort"]
        .as_str()
        .or_else(|| body["reasoning_effort"].as_str());
    if let Some(e) = effort {
        payload["reasoning_effort"] = json!(e.to_lowercase());
    } else if let Some(budget) = body["thinking"]["budget_tokens"].as_u64() {
        let e = if budget <= 1024 {
            "low"
        } else if budget <= 10240 {
            "medium"
        } else {
            "high"
        };
        payload["reasoning_effort"] = json!(e);
    }

    // 3. Tools
    if let Some(tools_arr) = crate::proxy::client_tools(body).as_array() {
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

/// Open an HTTP connection to Ollama's `/v1/chat/completions` endpoint.
pub fn open(base_url: &str, key: Option<&str>, payload: &Value) -> Result<Box<dyn BufRead + Send>, Failure> {
    let url = format!("{}/v1/chat/completions", normalize_url(base_url));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(600)))
        .build()
        .into();

    let mut req = agent
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream");

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
    fn test_chat_body_system_and_messages() {
        let req = json!({
            "system": "You are a helpful assistant.\nx-anthropic-billing-header: test",
            "messages": [
                { "role": "user", "content": "Hello" }
            ],
            "max_tokens": 1024,
            "thinking": { "type": "enabled", "budget_tokens": 5000 }
        });
        let body = chat_body(&req, "gpt-oss:120b", true);
        assert_eq!(body["model"], "gpt-oss:120b");
        assert_eq!(body["stream"], true);
        assert_eq!(body["options"]["num_ctx"], 65536);
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["reasoning_effort"], "medium");

        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "You are a helpful assistant.");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "Hello");
    }

    #[test]
    fn test_chat_body_tool_use_and_result() {
        let req = json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        { "type": "text", "text": "Let me run bash" },
                        { "type": "tool_use", "id": "call_1", "name": "Bash", "input": { "command": "ls" } }
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        { "type": "tool_result", "tool_use_id": "call_1", "content": "file1.txt" }
                    ]
                }
            ],
            "tools": [
                {
                    "name": "Bash",
                    "description": "Run bash",
                    "input_schema": { "type": "object", "properties": { "command": { "type": "string" } } }
                }
            ]
        });
        let body = chat_body(&req, "deepseek-v4-pro", true);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"], "Let me run bash");
        assert_eq!(msgs[0]["tool_calls"][0]["function"]["name"], "Bash");
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["tool_call_id"], "call_1");
        assert_eq!(msgs[1]["content"], "file1.txt");

        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "Bash");
    }
}
