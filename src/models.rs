use anyhow::{Context, Result};
use serde_json::Value;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

/// Parse model names from various JSON API responses (OpenAI, Anthropic, Ollama, etc.)
pub fn parse_model_list(json_str: &str) -> Vec<String> {
    let mut models = Vec::new();
    let Ok(val) = serde_json::from_str::<Value>(json_str) else {
        return models;
    };

    // 1. Check if root is array
    if let Some(arr) = val.as_array() {
        for item in arr {
            if let Some(id) = item.as_str() {
                models.push(id.to_string());
            } else if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                models.push(id.to_string());
            } else if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                models.push(name.to_string());
            }
        }
    } else if let Some(data) = val.get("data").and_then(|d| d.as_array()) {
        // 2. Check "data" field (OpenAI / Anthropic standard)
        for item in data {
            if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                models.push(id.to_string());
            } else if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                models.push(name.to_string());
            }
        }
    } else if let Some(data) = val.get("models").and_then(|d| d.as_array()) {
        // 3. Check "models" field (Ollama standard)
        for item in data {
            if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                models.push(name.to_string());
            } else if let Some(model) = item.get("model").and_then(|v| v.as_str()) {
                models.push(model.to_string());
            } else if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                models.push(id.to_string());
            }
        }
    }

    models.sort();
    models.dedup();
    models
}

/// Fetch list of models from given base_url/v1/models (synchronous)
pub fn fetch_models(base_url: &str, api_key: Option<&str>) -> Result<Vec<String>> {
    let mut url = base_url.trim().trim_end_matches('/').to_string();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        url = format!("http://{}", url);
    }
    let endpoint = if url.ends_with("/v1") {
        format!("{}/models", url)
    } else {
        format!("{}/v1/models", url)
    };

    let agent = ureq::builder()
        .timeout(Duration::from_secs(5))
        .build();

    let mut req = agent.get(&endpoint);
    if let Some(key) = api_key {
        if !key.is_empty() {
            req = req.set("Authorization", &format!("Bearer {}", key))
                     .set("x-api-key", key);
        }
    }

    let response = req.call()
        .with_context(|| format!("Failed to connect to {}", endpoint))?;

    let text = response.into_string()
        .with_context(|| "Failed to read response body")?;

    let models = parse_model_list(&text);
    if models.is_empty() {
        anyhow::bail!("No models found in response from {}", endpoint);
    }

    Ok(models)
}

/// Non-blocking background worker to fetch models
pub fn spawn_fetch(
    base_url: String,
    api_key: Option<String>,
) -> Receiver<Result<Vec<String>, String>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let res = fetch_models(&base_url, api_key.as_deref())
            .map_err(|e| e.to_string());
        let _ = tx.send(res);
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_openai_format() {
        let json = r#"{
            "object": "list",
            "data": [
                {"id": "gpt-4o", "object": "model"},
                {"id": "claude-3-7-sonnet", "object": "model"}
            ]
        }"#;
        let models = parse_model_list(json);
        assert_eq!(models, vec!["claude-3-7-sonnet", "gpt-4o"]);
    }

    #[test]
    fn test_parse_ollama_format() {
        let json = r#"{
            "models": [
                {"name": "llama3:latest", "model": "llama3:latest"},
                {"name": "qwen2.5-coder:7b", "model": "qwen2.5-coder:7b"}
            ]
        }"#;
        let models = parse_model_list(json);
        assert_eq!(models, vec!["llama3:latest", "qwen2.5-coder:7b"]);
    }

    #[test]
    fn test_parse_simple_array() {
        let json = r#"["claude-fable-5[1m]", "claude-opus-5[1m]", "claude-o11ama"]"#;
        let models = parse_model_list(json);
        assert_eq!(models, vec!["claude-fable-5[1m]", "claude-o11ama", "claude-opus-5[1m]"]);
    }

    #[test]
    fn test_parse_empty_or_invalid() {
        assert!(parse_model_list("invalid json").is_empty());
        assert!(parse_model_list("{}").is_empty());
    }

    #[test]
    fn test_spawn_fetch_channel() {
        let rx = spawn_fetch("http://127.0.0.1:9".to_string(), None);
        // Receiver should return a result (likely connection error for dummy port) within reasonable time
        let result = rx.recv_timeout(Duration::from_secs(6));
        assert!(result.is_ok());
    }
}
