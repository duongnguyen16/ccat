//! The HTTP half of the proxy: reading Claude Code's request, answering it, and forwarding official / endpoint
//! routes byte for byte while metering their token usage.

use super::Fail;
use crate::common::{self, Failure};
use serde_json::{json, Map, Value};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::TcpStream,
};


const MAX_BODY: usize = 64 << 20;

/// The official API: requests go there with the login Claude Code itself sends.
pub(crate) const ANTHROPIC_API: &str = "https://api.anthropic.com";

/// The official API, or in tests a fake one.
pub(crate) fn anthropic_api() -> String {
    #[cfg(test)]
    if let Some(u) = super::tests::FAKE_API.lock().unwrap().clone() {
        return u;
    }
    ANTHROPIC_API.into()
}

pub(crate) struct Req {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Req {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

pub(crate) fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

pub(crate) fn read_request(s: &TcpStream) -> io::Result<Req> {
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

pub(crate) fn respond(s: &mut TcpStream, status: u16, body: &Value, extra: &str) {
    let body = body.to_string();
    let _ = write!(
        s,
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
        if status == 200 { "OK" } else { "Error" },
        body.len()
    );
}

pub(crate) fn error_kind(status: u16) -> &'static str {
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

pub(crate) fn respond_error(s: &mut TcpStream, f: &Failure) {
    let extra = f.retry_after.map(|r| format!("Retry-After: {r}\r\n")).unwrap_or_default();
    respond(s, f.status, &json!({ "type": "error", "error": { "type": error_kind(f.status), "message": f.message } }), &extra);
}

/// Drops Claude Code's per-request billing line (`cch=` changes every turn) from `system`, so an endpoint's prompt
/// cache sees a stable prefix. A text block left empty goes whole (Anthropic rejects empty ones); other blocks,
/// `cache_control` included, stay as they were. Whether anything changed is returned.
/// ponytail: a removed block's own `cache_control` goes with it; Claude Code puts none on the billing block.
fn strip_billing(body: &mut Value) -> bool {
    let billing = super::FILTERED_SYSTEM_LINES[0];
    let is = |l: &str| l.trim_start().starts_with(billing);
    let strip = |t: &str| t.split('\n').any(is).then(|| t.split('\n').filter(|l| !is(l)).collect::<Vec<_>>().join("\n"));
    let mut hit = false;
    match body.get_mut("system") {
        Some(Value::String(t)) => {
            if let Some(new) = strip(t) {
                (hit, *t) = (true, new);
            }
        }
        Some(Value::Array(blocks)) => blocks.retain_mut(|b| match b["text"].as_str().and_then(strip) {
            Some(new) => {
                let keep = !new.trim().is_empty();
                (hit, b["text"]) = (true, json!(new));
                keep
            }
            None => true,
        }),
        _ => {}
    }
    if hit && match &body["system"] { Value::String(t) => t.trim().is_empty(), Value::Array(a) => a.is_empty(), _ => false } {
        body.as_object_mut().map(|o| o.remove("system"));
    }
    hit
}

/// The request body for an official or endpoint route: the tier's real model, and no thinking blocks another
/// provider signed (Antigravity's `ag:` and ChatGPT's `oa:` tags, or none at all): Anthropic rejects a signature it didn't make.
/// When neither applies the original bytes go on untouched, so the request is exactly what Claude Code sent (key order,
/// number formatting, escapes); only a rewrite re-serializes `body` (which keeps its key order).
/// ponytail: an assistant turn that was nothing but such thinking would be left empty; Claude Code always follows
/// thinking with text or a tool call, so it hasn't come up.
pub(crate) fn upstream_body(body: &Value, raw: &[u8], model: &str, endpoint: bool) -> Vec<u8> {
    if body.is_null() {
        return Vec::new();
    }
    let keeps = |b: &Value| b["type"] != "thinking" || b["signature"].as_str().is_some_and(|s| !s.is_empty() && !s.starts_with("ag:") && !s.starts_with("oa:"));
    let blocks = || body["messages"].as_array().into_iter().flatten().filter_map(|m| m["content"].as_array()).flatten();
    let mut body = body.clone();
    // Only an endpoint loses the billing line: the official route forwards Claude Code's exact bytes.
    let billed = endpoint && strip_billing(&mut body);
    if !billed && body["model"] == model && blocks().all(keeps) {
        return raw.to_vec();
    }
    body["model"] = json!(model);
    for m in body.get_mut("messages").and_then(Value::as_array_mut).into_iter().flatten() {
        if let Some(blocks) = m.get_mut("content").and_then(Value::as_array_mut) {
            blocks.retain(keeps);
        }
    }
    body.to_string().into_bytes()
}

/// Token usage read off an answer as it passes through: SSE `message_start` / `message_delta` events, or a plain
/// JSON message. The highest value seen per field wins (`output_tokens` grows through the stream).
#[derive(Default)]
pub(crate) struct Meter {
    sse: bool,
    tail: Vec<u8>,
    usage: Map<String, Value>,
}

impl Meter {
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        if !self.sse {
            // A whole JSON message; past a few MB it isn't one worth counting.
            if self.tail.len() < 8 << 20 {
                self.tail.extend_from_slice(bytes);
            }
            return;
        }
        self.tail.extend_from_slice(bytes);
        while let Some(i) = self.tail.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.tail.drain(..=i).collect();
            if let Some(v) = line.strip_prefix(b"data: ").filter(|d| d.windows(7).any(|w| w == b"\"usage\"")).and_then(|d| serde_json::from_slice::<Value>(d).ok()) {
                self.take(&v);
            }
        }
    }

    pub(crate) fn take(&mut self, v: &Value) {
        let u = if v["message"]["usage"].is_object() { &v["message"]["usage"] } else { &v["usage"] };
        for (k, n) in u.as_object().into_iter().flatten().filter(|(_, n)| n.is_u64()) {
            if self.usage.get(k).and_then(Value::as_u64) < n.as_u64() {
                self.usage.insert(k.clone(), n.clone());
            }
        }
    }

    pub(crate) fn finish(mut self) -> Value {
        if !self.sse {
            if let Ok(v) = serde_json::from_slice::<Value>(&self.tail) {
                self.take(&v);
            }
        }
        Value::Object(self.usage)
    }
}

/// One line per answered request in `tokens.jsonl` next to the config: when, which tier and provider, which model,
/// and the tokens. Only routed launches pass through the proxy, so only they are counted. (Kept for future builds.)
pub(crate) fn record(tier: &str, provider: &str, model: &str, usage: &Value) {
    let n = |k: &str| usage[k].as_u64().unwrap_or(0);
    if cfg!(test) || n("input_tokens") + n("output_tokens") + n("cache_read_input_tokens") == 0 {
        return;
    }
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let line = json!({ "t": t, "tier": tier, "provider": provider, "model": model, "input": n("input_tokens"), "output": n("output_tokens"),
                       "cache_read": n("cache_read_input_tokens"), "cache_write": n("cache_creation_input_tokens") });
    let path = common::log_path().with_file_name("tokens.jsonl");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(format!("{line}\n").as_bytes()); // one write per line: several sessions can append at once
    }
}

/// Official and endpoint routes: the request goes on unchanged except for `body` (and the endpoint's own key),
/// and the answer, SSE included, streams back as it arrives, rate-limit headers and all. With `meter` (tier,
/// provider, model) the answer's token usage goes to the token log.
#[allow(clippy::too_many_arguments)]
pub(crate) fn forward(s: &mut TcpStream, agent: &ureq::Agent, req: &Req, base: &str, key: Option<&str>, body: Vec<u8>, meter: Option<(&str, &str, &str)>, may_fail: Fail) -> Result<(), Failure> {
    let mut b = ureq::http::Request::builder().method(req.method.as_str()).uri(format!("{base}{}", req.path));
    for (k, v) in &req.headers {
        let hop = matches!(k.as_str(), "host" | "content-length" | "connection" | "transfer-encoding" | "accept-encoding" | "x-ccat-token");
        if hop || (key.is_some() && matches!(k.as_str(), "authorization" | "x-api-key")) {
            continue;
        }
        // An endpoint gets its key, not Claude Code's login, so the login's beta flag goes too.
        let v = match key {
            Some(_) if k == "anthropic-beta" => v.split(',').map(str::trim).filter(|f| !f.starts_with("oauth-")).collect::<Vec<_>>().join(","),
            _ => v.clone(),
        };
        if !v.is_empty() {
            b = b.header(k.as_str(), v);
        }
    }
    if let Some(key) = key {
        b = b.header("x-api-key", key).header("authorization", format!("Bearer {key}"));
    }
    let sent = if body.is_empty() { b.body(()).map(|r| agent.run(r)) } else { b.body(body).map(|r| agent.run(r)) };
    let fail = |s: &mut TcpStream, f: Failure| if may_fail.takes(f.status) { Err(f) } else { Ok(respond_error(s, &f)) };
    let mut res = match sent {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return fail(s, Failure { status: 502, message: format!("ccat proxy → {base}: {e}"), retry_after: None }),
        Err(e) => return fail(s, Failure { status: 400, message: format!("ccat proxy: {e}"), retry_after: None }),
    };
    let status = res.status();
    if may_fail.takes(status.as_u16()) {
        let retry_after = res.headers().get("retry-after").and_then(|v| v.to_str().ok()?.trim().parse().ok());
        if status.as_u16() == 429 {
            // Which limit it was: Anthropic says so in headers only.
            let said: Vec<String> = res.headers().iter().filter(|(k, _)| k.as_str() == "retry-after" || k.as_str().contains("ratelimit")).map(|(k, v)| format!("{k}={}", v.to_str().unwrap_or(""))).collect();
            common::log(&format!("{base} answered 429: {}", said.join(" ")));
        }
        let text = res.body_mut().with_config().limit(64 << 10).read_to_string().unwrap_or_default();
        let message = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["error"]["message"].as_str().map(String::from)).unwrap_or(text);
        return Err(Failure { status: status.as_u16(), message, retry_after });
    }
    let mut head = format!("HTTP/1.1 {} {}\r\n", status.as_u16(), status.canonical_reason().unwrap_or("Status"));
    for (k, v) in res.headers() {
        if !matches!(k.as_str(), "content-length" | "transfer-encoding" | "connection" | "content-encoding") {
            head.push_str(&format!("{k}: {}\r\n", v.to_str().unwrap_or("")));
        }
    }
    let bodyless = req.method == "HEAD" || matches!(status.as_u16(), 204 | 304);
    head.push_str(if bodyless { "Connection: close\r\n\r\n" } else { "Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n" });
    if s.write_all(head.as_bytes()).is_err() || bodyless {
        return Ok(());
    }
    let sse = res.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("event-stream"));
    let mut tokens = (meter.is_some() && status.as_u16() == 200).then(|| Meter { sse, ..Default::default() });
    let mut reader = res.into_body().into_reader();
    let mut buf = vec![0u8; 16 << 10];
    while let Ok(n @ 1..) = reader.read(&mut buf) {
        let chunk = s.write_all(format!("{n:x}\r\n").as_bytes()).and_then(|_| s.write_all(&buf[..n])).and_then(|_| s.write_all(b"\r\n"));
        if chunk.is_err() {
            return Ok(()); // Claude Code hung up
        }
        if let Some(m) = tokens.as_mut() {
            m.feed(&buf[..n]);
        }
    }
    let _ = s.write_all(b"0\r\n\r\n");
    if let (Some(m), Some((tier, provider, model))) = (tokens, meter) {
        record(tier, provider, model, &m.finish());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_reads_usage_from_streams_and_messages() {
        let mut m = Meter { sse: true, ..Default::default() };
        let stream = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"cache_read_input_tokens\":3000,\"output_tokens\":1}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}\n\n"
        );
        // Split mid-line, as network reads are.
        let (a, b) = stream.as_bytes().split_at(57);
        m.feed(a);
        m.feed(b);
        let u = m.finish();
        assert_eq!((u["input_tokens"].as_u64(), u["output_tokens"].as_u64(), u["cache_read_input_tokens"].as_u64()), (Some(12), Some(42), Some(3000)));

        let mut m = Meter::default();
        m.feed(br#"{"type":"message","usage":{"input_tokens":5,"output_tokens":7}}"#);
        assert_eq!(m.finish()["output_tokens"], 7, "a non-streamed message");
    }
}
