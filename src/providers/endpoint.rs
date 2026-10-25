//! A custom Anthropic-compatible endpoint: forwarded with its own key (see `proxy::http::forward`); here its model list.

use crate::bare_id;
use serde_json::Value;
use std::time::Duration;

fn models_endpoint(url: &str) -> String {
    let base = url.trim().trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    format!("{base}/v1/models?limit=1000")
}

/// Works for Anthropic-style and OpenAI-style `/v1/models`: both return `{ data: [{ id }] }`.
pub(crate) fn fetch_models(url: &str, key: &str) -> Result<Vec<String>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).build().into();
    let mut res = agent
        .get(&models_endpoint(url))
        .header("x-api-key", key)
        .header("authorization", format!("Bearer {key}"))
        .header("anthropic-version", "2023-06-01")
        .call()
        .map_err(|e| e.to_string())?;
    let body = res.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("response is not JSON: {e}"))?;
    let data = v["data"].as_array().ok_or("response has no `data` array")?;
    Ok(data.iter().filter_map(|m| m["id"].as_str().map(bare_id)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_normalizes_v1() {
        assert_eq!(models_endpoint("https://x.io/"), "https://x.io/v1/models?limit=1000");
        assert_eq!(models_endpoint("https://x.io/api/v1"), "https://x.io/api/v1/models?limit=1000");
    }
}
