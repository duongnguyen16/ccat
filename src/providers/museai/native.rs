//! Muse wire protocol adapted from nikships/muse-cli (MIT; notice in THIRD_PARTY.md).
//! One owner per connection: Noise nonces must never be read concurrently.
use prost::Message;
use serde_json::{json, Value};
use std::io::Read;
use std::{net::{TcpStream, ToSocketAddrs}, sync::{Arc, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};
use tungstenite::{client::IntoClientRequest, stream::MaybeTlsStream, protocol::WebSocketConfig, WebSocket, Message as WsMessage};
use crate::common::Failure;

pub(super) const MAX_BODY: usize = 16 * 1024 * 1024;
const CHUNK: usize = 60_000; // Leave room for protobuf metadata and the AES-GCM tag.

#[derive(Clone, PartialEq, Message)]
struct NoiseFrame {
    #[prost(int64, tag="1")] chunk_id: i64,
    #[prost(uint32, tag="2")] chunk_index: u32,
    #[prost(uint32, tag="3")] total_chunks: u32,
    #[prost(bytes="vec", tag="4")] payload: Vec<u8>,
}
#[derive(Clone, PartialEq, Message)]
struct ServiceRequest {
    #[prost(int32, tag="1")] service: i32,
    #[prost(bytes="vec", tag="2")] payload: Vec<u8>,
}
#[derive(Clone, PartialEq, Message)]
struct ServiceResponse { #[prost(bytes="vec", tag="1")] payload: Vec<u8> }
#[derive(Clone, PartialEq, Message)]
struct ServiceFrame {
    #[prost(int64, tag="1")] stream_id: i64,
    #[prost(oneof="FrameKind", tags="2, 3, 4, 5")] kind: Option<FrameKind>,
}
#[derive(Clone, PartialEq, prost::Oneof)]
enum FrameKind {
    #[prost(message, tag="2")] Request(ApplicationRequest),
    #[prost(message, tag="3")] Response(ApplicationResponse),
    #[prost(message, tag="4")] Body(BodyChunk),
    #[prost(message, tag="5")] Reset(Reset),
}
#[derive(Clone, PartialEq, Message)]
struct Header {
    #[prost(string, tag="1")] key: String,
    #[prost(string, tag="2")] value: String,
}
#[derive(Clone, PartialEq, Message)]
struct ApplicationRequest {
    #[prost(string, tag="1")] verb: String,
    #[prost(string, tag="2")] path: String,
    #[prost(message, repeated, tag="3")] headers: Vec<Header>,
    #[prost(bytes="vec", tag="4")] body: Vec<u8>,
    #[prost(bool, tag="5")] end_body: bool,
}
#[derive(Clone, PartialEq, Message)]
struct ApplicationResponse {
    #[prost(int32, tag="1")] status: i32,
    #[prost(message, repeated, tag="2")] headers: Vec<Header>,
    #[prost(bytes="vec", tag="3")] body: Vec<u8>,
    #[prost(bool, tag="4")] end_body: bool,
}
#[derive(Clone, PartialEq, Message)]
struct BodyChunk {
    #[prost(bytes="vec", tag="1")] data: Vec<u8>,
    #[prost(bool, tag="2")] end_body: bool,
}
#[derive(Clone, PartialEq, Message)]
struct Reset {
    #[prost(int32, tag="1")] code: i32,
    #[prost(string, tag="2")] reason: String,
}

pub(super) fn fail(status: u16, message: impl Into<String>) -> Failure {
    Failure { status, message: message.into(), retry_after: None }
}
fn protocol_error() -> Failure { fail(502, "MuseAI: invalid or unauthenticated gateway frame") }

fn pack(noise: &mut snow::TransportState, body: &[u8]) -> Result<Vec<Vec<u8>>, Failure> {
    if body.is_empty() || body.len() > MAX_BODY { return Err(fail(413, "MuseAI gateway payload exceeds limit")); }
    let mut id = [0; 8];
    getrandom::fill(&mut id).map_err(|_| fail(500, "Could not generate gateway chunk ID"))?;
    let chunk_id = i64::from_le_bytes(id);
    let total_chunks = body.len().div_ceil(CHUNK) as u32;
    body.chunks(CHUNK).enumerate().map(|(i, part)| {
        let frame = NoiseFrame { chunk_id, chunk_index: i as u32, total_chunks, payload: part.to_vec() }.encode_to_vec();
        let mut wire = vec![0; 65_535];
        let n = noise.write_message(&frame, &mut wire).map_err(|_| protocol_error())?;
        wire.truncate(n);
        Ok(wire)
    }).collect()
}
fn unpack(noise: &mut snow::TransportState, wire: &[u8]) -> Result<NoiseFrame, Failure> {
    let mut plain = vec![0; 65_535];
    let n = noise.read_message(wire, &mut plain).map_err(|_| protocol_error())?;
    NoiseFrame::decode(&plain[..n]).map_err(|_| protocol_error())
}

#[derive(Default)]
struct Assembly { id: i64, total: u32, next: u32, body: Vec<u8> }
impl Assembly {
    fn push(&mut self, frame: NoiseFrame) -> Result<Option<Vec<u8>>, Failure> {
        if frame.total_chunks == 0 || frame.total_chunks > 4096 || frame.chunk_index != self.next
            || (self.next != 0 && (frame.chunk_id != self.id || frame.total_chunks != self.total))
            || self.body.len().saturating_add(frame.payload.len()) > MAX_BODY {
            return Err(protocol_error());
        }
        if self.next == 0 { self.id = frame.chunk_id; self.total = frame.total_chunks; }
        self.body.extend_from_slice(&frame.payload);
        self.next += 1;
        if self.next == self.total {
            self.next = 0;
            Ok(Some(std::mem::take(&mut self.body)))
        } else { Ok(None) }
    }
}

pub(super) fn muse_domain(domain: &str) -> bool {
    let d = domain.trim_start_matches('.').to_ascii_lowercase();
    d == "muse.ai" || d.ends_with(".muse.ai")
}

pub(super) fn cookies(raw: &str) -> Result<String, Failure> {
    if raw.len() > 65_536 { return Err(fail(401, "MuseAI cookie file is too large")); }
    let raw = raw.trim();
    let mut pairs = Vec::new();
    if raw.starts_with('{') {
        let value: Value = serde_json::from_str(raw).map_err(|_| fail(401, "MuseAI: invalid cookie JSON"))?;
        let map = value.get("cookies").unwrap_or(&value).as_object().ok_or_else(|| fail(401, "MuseAI: expected cookie object"))?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        for (name, cookie) in map {
            if value["expires"][name].as_u64().is_some_and(|expiry| expiry > 0 && expiry <= now) { continue; }
            pairs.push((name.to_string(), cookie.as_str().ok_or_else(|| fail(401, "MuseAI: invalid cookie value"))?.to_string()));
        }
    } else if raw.contains('\t') {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        for line in raw.lines() {
            let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
            if line.starts_with('#') || line.is_empty() { continue; }
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 7 { return Err(fail(401, "MuseAI: malformed Netscape cookie file")); }
            let expiry = fields[4].parse::<u64>().map_err(|_| fail(401, "MuseAI: invalid cookie expiry"))?;
            if muse_domain(fields[0]) && (expiry == 0 || expiry > now) { pairs.push((fields[5].into(), fields[6].into())); }
        }
    } else {
        for item in raw.split(';') {
            let (name, value) = item.trim().split_once('=').ok_or_else(|| fail(401, "MuseAI: malformed cookie header"))?;
            pairs.push((name.into(), value.into()));
        }
    }
    if !pairs.iter().any(|(k, v)| k == "hatch_sess" && !v.is_empty()) { return Err(fail(401, "MuseAI: no valid hatch_sess cookie; sign in again")); }
    for (k, v) in &pairs {
        if k.is_empty() || !k.bytes().all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)) || v.bytes().any(|c| c <= 32 || c >= 127 || c == b';') {
            return Err(fail(401, "MuseAI: invalid cookie characters"));
        }
    }
    Ok(pairs.into_iter().map(|(k,v)| format!("{k}={v}")).collect::<Vec<_>>().join("; "))
}

pub(super) fn encode_query(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

pub(super) fn http_json(cookies: &str, path: &str, body: Option<&Value>, token: Option<&str>) -> Result<Value, Failure> {
    // Fixed Muse origin only; never forward cookies or bearer tokens across redirects.
    let agent: ureq::Agent = ureq::Agent::config_builder().max_redirects(0)
        .timeout_global(Some(Duration::from_secs(20))).build().into();
    let headers = |mut req: ureq::RequestBuilder<ureq::typestate::WithoutBody>| {
        req = req.header("User-Agent", "Mozilla/5.0 Chrome/153.0.0.0")
            .header("Referer", "https://muse.ai/").header("Origin", "https://muse.ai")
            .header("Sec-Fetch-Site", "same-origin").header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Dest", "empty").header("Accept", "application/json").header("Cookie", cookies);
        if let Some(t) = token { req = req.header("Authorization", format!("Bearer {t}")); }
        req
    };
    let url = format!("https://muse.ai{path}");
    let result = if let Some(value) = body {
        let mut req = agent.post(&url).header("Content-Type", "application/json")
            .header("User-Agent", "Mozilla/5.0 Chrome/153.0.0.0")
            .header("Referer", "https://muse.ai/").header("Origin", "https://muse.ai")
            .header("Sec-Fetch-Site", "same-origin").header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Dest", "empty").header("Accept", "application/json").header("Cookie", cookies);
        if let Some(t) = token { req = req.header("Authorization", format!("Bearer {t}")); }
        req.send(value.to_string())
    } else { headers(agent.get(&url)).call() };
    let response = result.map_err(|e| match e {
        ureq::Error::StatusCode(s) => fail(s, format!("MuseAI {path}: HTTP {s}")),
        _ => fail(502, format!("MuseAI {path}: HTTPS request failed")),
    })?;
    read_json_body(response.into_body().into_reader()).map_err(|e| fail(e.status, format!("MuseAI {path}: {}", e.message)))
}
fn read_json_body(reader: impl Read) -> Result<Value, Failure> {
    let mut bytes = Vec::new();
    reader.take((MAX_BODY + 1) as u64).read_to_end(&mut bytes).map_err(|_| fail(502, "invalid HTTPS response"))?;
    if bytes.len() > MAX_BODY { return Err(fail(502, "HTTPS response exceeds limit")); }
    serde_json::from_slice(&bytes).map_err(|_| fail(502, "expected JSON"))
}

// ---------------------------------------------------------------- subscription (live usage)

/// Server Action id of `fetchSubscriptionAction` in the build seen last; a deploy changes it, then `discover` finds the new one.
const KNOWN_ACTION: &str = "40034617c46db812e70eec8e362677944a0a532adf";
const MAX_CHUNKS: usize = 1200; // ponytail: ~1000 chunks exist, the action sits ~420 deep; raise if muse.ai grows past this
const MAX_CHUNK: usize = 4 * 1024 * 1024;
static ACTION: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
/// When a crawl last came back empty-handed.
static LAST_MISS: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// The headers muse.ai wants from a same-origin browser call (403 without them); never the cookie.
fn browser<B>(req: ureq::RequestBuilder<B>, referer: &str, accept: &str) -> ureq::RequestBuilder<B> {
    req.header("User-Agent", "Mozilla/5.0 Chrome/153.0.0.0").header("Referer", referer).header("Origin", "https://muse.ai")
        .header("Sec-Fetch-Site", "same-origin").header("Sec-Fetch-Mode", "cors").header("Sec-Fetch-Dest", "empty").header("Accept", accept)
}
fn text_failure(e: ureq::Error, what: &str) -> Failure {
    match e {
        ureq::Error::StatusCode(s) => fail(s, format!("MuseAI {what}: HTTP {s}")),
        ureq::Error::TooManyRedirects => fail(401, format!("MuseAI {what}: redirected away, sign in again")), // redirects are never followed with the cookie
        _ => fail(502, format!("MuseAI {what}: HTTPS request failed")),
    }
}
fn read_text(reader: impl Read, limit: usize) -> Result<String, Failure> {
    let mut bytes = Vec::new();
    reader.take((limit + 1) as u64).read_to_end(&mut bytes).map_err(|_| fail(502, "invalid HTTPS response"))?;
    if bytes.len() > limit { return Err(fail(502, "HTTPS response exceeds limit")); }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
fn web_agent() -> ureq::Agent {
    ureq::Agent::config_builder().max_redirects(0).timeout_global(Some(Duration::from_secs(20))).build().into()
}

/// The result of a Server Action from its RSC stream: `0:{"a":"$@N",..}` points at the line `N:` that carries it;
/// failing that, the first line whose JSON holds `subscription`.
pub(super) fn rsc_result(text: &str) -> Option<Value> {
    let lines: Vec<(&str, Value)> = text.lines().filter_map(|l| { let (id, json) = l.split_once(':')?; Some((id, serde_json::from_str(json).ok()?)) }).collect();
    let pointed = lines.iter().find(|(id, _)| *id == "0").and_then(|(_, v)| v["a"].as_str()?.strip_prefix("$@"))
        .and_then(|n| lines.iter().find(|(id, v)| *id == n && v.is_object()));
    pointed.or_else(|| lines.iter().find(|(_, v)| v.get("subscription").is_some())).map(|(_, v)| v.clone())
}

/// The id in `createServerReference("<hex>",…,"fetchSubscriptionAction")`, tolerant of minifier spacing; another action's reference never matches.
pub(super) fn action_id(js: &str) -> Option<String> {
    const CALL: &str = "createServerReference";
    js.match_indices(CALL).find_map(|(i, _)| {
        let rest = js[i + CALL.len()..].trim_start();
        let rest = rest.strip_prefix(')').unwrap_or(rest).trim_start().strip_prefix('(')?.trim_start().strip_prefix('"')?;
        let end = rest.find(|c: char| !matches!(c, '0'..='9' | 'a'..='f'))?;
        let args = rest[end..].strip_prefix('"')?;
        (end >= 40 && args[..args.find(')')?].contains("\"fetchSubscriptionAction\"")).then(|| rest[..end].to_string())
    })
}

/// `static/chunks/<name>.js` paths named in a page or a chunk (the route's lazy chunks are only named inside other chunks).
fn chunk_paths(s: &str) -> impl Iterator<Item = &str> {
    s.match_indices("static/chunks/").filter_map(|(i, _)| {
        let rest = &s[i..];
        let path = &rest[..rest.find(|c: char| !(c.is_ascii_alphanumeric() || "/_-.".contains(c)))?];
        (path.ends_with(".js") && !path.contains("..")).then_some(path)
    })
}
/// Finds the current action id: the page's script chunks and the chunks they name (public static files, fetched without
/// the cookie) breadth first, a batch at a time in parallel; the action sat ~420 chunks in when this was written.
fn discover(cookies: &str) -> Result<String, Failure> {
    let agent = web_agent();
    let page = browser(agent.get("https://muse.ai/hatch"), "https://muse.ai/hatch", "text/html").header("Cookie", cookies).call().map_err(|e| text_failure(e, "/hatch"))?;
    let mut queue: Vec<String> = Vec::new();
    let add = |queue: &mut Vec<String>, text: &str| for p in chunk_paths(text) { if !queue.iter().any(|q| q == p) { queue.push(p.into()); } };
    add(&mut queue, &read_text(page.into_body().into_reader(), MAX_BODY)?);
    let fetch = |path: &String| browser(agent.get(format!("https://muse.ai/_next/{path}")), "https://muse.ai/hatch", "*/*").call().ok().and_then(|r| read_text(r.into_body().into_reader(), MAX_CHUNK).ok());
    let mut next = 0;
    while next < queue.len().min(MAX_CHUNKS) {
        let end = queue.len().min(MAX_CHUNKS).min(next + 8);
        let bodies: Vec<Option<String>> = std::thread::scope(|s| queue[next..end].iter().map(|p| s.spawn(|| fetch(p))).collect::<Vec<_>>().into_iter().map(|h| h.join().ok().flatten()).collect());
        next = end;
        for js in bodies.into_iter().flatten() {
            if let Some(id) = action_id(&js) { return Ok(id); }
            add(&mut queue, &js);
        }
    }
    Err(fail(502, "MuseAI usage: could not locate the subscription action"))
}

/// One `fetchSubscriptionAction` call (read-only). 404 doubles as "no parsable result": an unknown id answers so.
fn call_action(cookies: &str, id: &str) -> Result<Value, Failure> {
    let response = browser(web_agent().post("https://muse.ai/hatch"), "https://muse.ai/hatch", "text/x-component").header("Next-Action", id)
        .header("Content-Type", "text/plain;charset=UTF-8").header("Cookie", cookies)
        .send(r#"[{"includeAgreement":true}]"#).map_err(|e| text_failure(e, "usage"))?;
    let text = read_text(response.into_body().into_reader(), MAX_BODY)?;
    rsc_result(&text).filter(|v| v["subscription"].is_object()).ok_or_else(|| fail(404, "MuseAI usage: response not understood"))
}

/// The account's subscription (plan, weekly and top-up usage) as muse.ai's own page loads it.
pub(super) fn subscription(cookies: &str) -> Result<Value, Failure> {
    let known = ACTION.lock().ok().and_then(|g| g.clone()).unwrap_or_else(|| KNOWN_ACTION.into());
    let remember = |id: &str| if let Ok(mut g) = ACTION.lock() { *g = Some(id.into()); };
    match call_action(cookies, &known) {
        Err(e) if e.status == 404 => {
            // A crawl is ~1000 requests: after one that found nothing, the next waits an hour (the tray polls often).
            // ponytail: per process, and a found id isn't saved, so each new process crawls once after a deploy.
            if LAST_MISS.lock().ok().and_then(|g| *g).is_some_and(|t| t.elapsed() < Duration::from_secs(3600)) { return Err(e); }
            let id = discover(cookies).inspect_err(|_| if let Ok(mut g) = LAST_MISS.lock() { *g = Some(std::time::Instant::now()); })?;
            if id == known { return Err(e); }
            let value = call_action(cookies, &id)?;
            remember(&id);
            Ok(value)
        }
        other => { if other.is_ok() { remember(&known); } other }
    }
}

pub(super) struct Identity { pub viewer_id: String, pub vm_id: String, pub token: String }
pub(super) fn identity(cookies: &str) -> Result<Identity, Failure> {
    let auth = http_json(cookies, "/api/auth/check", Some(&json!({})), None)?;
    let access = auth["access_token"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| fail(401, "MuseAI: sign in again"))?;
    let session = http_json(cookies, "/api/session", None, None)?;
    let vm_id = session["vm_id"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| fail(503, "MuseAI workspace is unavailable; open muse.ai and retry"))?;
    let hatch = http_json(cookies, "/api/hatch/token", Some(&json!({"vmAddress": format!("wss://{vm_id}.metaaivm.com/"), "vmName": vm_id})), Some(access))?;
    let token = hatch["token"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| fail(502, "MuseAI: missing gateway token"))?;
    Ok(Identity { viewer_id: auth["viewer_id"].as_str().unwrap_or("").into(), vm_id: vm_id.into(), token: token.into() })
}

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;
pub(super) fn socket(url: &str, host: &str, port: u16, max_size: usize) -> Result<Socket, Failure> {
    let addresses = (host, port).to_socket_addrs().map_err(|_| fail(502, "MuseAI: DNS lookup failed"))?;
    let stream = addresses.filter_map(|addr| TcpStream::connect_timeout(&addr, Duration::from_secs(5)).ok()).next()
        .ok_or_else(|| fail(502, "MuseAI: could not connect to gateway"))?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).map_err(|_| protocol_error())?;
    stream.set_write_timeout(Some(Duration::from_secs(3))).map_err(|_| protocol_error())?;
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(max_size);
    config.max_frame_size = Some(max_size);
    let request = url.into_client_request().map_err(|_| fail(502, "MuseAI: invalid WebSocket endpoint"))?;
    tungstenite::client_tls_with_config(request, stream, Some(config), None)
        .map(|(ws, _)| ws).map_err(|_| fail(502, "MuseAI: WebSocket/TLS handshake failed"))
}

pub(super) struct Gateway { ws: Socket, noise: snow::TransportState, stream: i64, chat_stream: Option<i64>, cancel: Arc<AtomicBool> }
impl Gateway {
    pub fn connect(id: &Identity, cancel: Arc<AtomicBool>) -> Result<Self, Failure> {
        let url = format!("wss://hatch.metaaivm.com/v1/noise?vm_id={}&auth_token={}", encode_query(&id.vm_id), encode_query(&id.token));
        let mut ws = socket(&url, "hatch.metaaivm.com", 443, 65_535)?;
        let params = "Noise_XX_25519_AESGCM_SHA256".parse().map_err(|_| protocol_error())?;
        let builder = snow::Builder::new(params);
        let key = builder.generate_keypair().map_err(|_| protocol_error())?;
        let mut handshake = builder.local_private_key(&key.private).build_initiator().map_err(|_| protocol_error())?;
        let mut wire = vec![0; 65_535];
        let n = handshake.write_message(&[], &mut wire).map_err(|_| protocol_error())?;
        ws.send(WsMessage::Binary(wire[..n].to_vec().into())).map_err(|_| protocol_error())?;
        let response = read_binary(&mut ws, &cancel, Instant::now() + Duration::from_secs(30))?;
        handshake.read_message(&response, &mut wire).map_err(|_| protocol_error())?;
        let n = handshake.write_message(&[], &mut wire).map_err(|_| protocol_error())?;
        ws.send(WsMessage::Binary(wire[..n].to_vec().into())).map_err(|_| protocol_error())?;
        Ok(Self { ws, noise: handshake.into_transport_mode().map_err(|_| protocol_error())?, stream: 0, chat_stream: None, cancel })
    }
    pub fn open(&mut self, verb: &str, path: &str, body: Option<&Value>) -> Result<i64, Failure> {
        if self.cancel.load(Ordering::Relaxed) { return Err(fail(499, "MuseAI request cancelled")); }
        self.stream += 1;
        let request = ApplicationRequest { verb: verb.into(), path: path.into(), headers: vec![Header { key: "content-type".into(), value: "application/json".into() }, Header { key: "x-request-id".into(), value: crate::common::random_hex(16) }], body: body.map(|b| b.to_string().into_bytes()).unwrap_or_default(), end_body: true };
        let frame = ServiceFrame { stream_id: self.stream, kind: Some(FrameKind::Request(request)) };
        let envelope = ServiceRequest { service: 0, payload: frame.encode_to_vec() }.encode_to_vec();
        for packet in pack(&mut self.noise, &envelope)? {
            self.ws.send(WsMessage::Binary(packet.into())).map_err(|_| fail(502, "MuseAI send failed; delivery is uncertain, not retried"))?;
        }
        if path == "/chat/stream" { self.chat_stream = Some(self.stream); }
        Ok(self.stream)
    }
    pub fn cleanup(&mut self, session: &str) -> Result<(), Failure> {
        let previous = std::mem::replace(&mut self.cancel, Arc::new(AtomicBool::new(false)));
        let result = self.request_until("POST", "/session/delete", Some(&json!({"method":"/api/session/delete","session_id":session})), Instant::now() + Duration::from_secs(3));
        self.cancel = previous;
        result.map(|_| ())
    }
    pub fn request(&mut self, verb: &str, path: &str, body: Option<&Value>) -> Result<Value, Failure> {
        self.request_until(verb, path, body, Instant::now() + Duration::from_secs(30))
    }
    fn request_until(&mut self, verb: &str, path: &str, body: Option<&Value>, deadline: Instant) -> Result<Value, Failure> {
        let sid = self.open(verb, path, body)?;
        let mut chunks = Assembly::default();
        let mut output = Vec::new();
        let mut status = None;
        loop {
            if self.cancel.load(Ordering::Relaxed) { return Err(fail(499, "MuseAI request cancelled")); }
            if Instant::now() >= deadline { return Err(fail(504, "MuseAI gateway response timed out")); }
            let wire = read_binary(&mut self.ws, &self.cancel, deadline)?;
            let Some(bytes) = chunks.push(unpack(&mut self.noise, &wire)?)? else { continue };
            let response = ServiceResponse::decode(bytes.as_slice()).map_err(|_| protocol_error())?;
            let frame = ServiceFrame::decode(response.payload.as_slice()).map_err(|_| protocol_error())?;
            if frame.stream_id != sid { check_other_frame(frame, self.chat_stream)?; continue; }
            let (bytes, done) = match frame.kind {
                Some(FrameKind::Response(r)) => { status = Some(r.status); (r.body, r.end_body) },
                Some(FrameKind::Body(b)) => (b.data, b.end_body),
                Some(FrameKind::Reset(_)) => return Err(fail(502, "MuseAI gateway reset the request")),
                _ => return Err(protocol_error()),
            };
            if output.len().saturating_add(bytes.len()) > MAX_BODY { return Err(protocol_error()); }
            output.extend_from_slice(&bytes);
            if done {
                let code = status.ok_or_else(protocol_error)?;
                if !(200..300).contains(&code) { return Err(fail(u16::try_from(code).unwrap_or(502), format!("MuseAI gateway: HTTP {code}"))); }
                if output.is_empty() { return Ok(Value::Null); }
                return application_json(&output);
            }
        }
    }
}
fn check_other_frame(frame: ServiceFrame, chat_stream: Option<i64>) -> Result<(), Failure> {
    if Some(frame.stream_id) != chat_stream { return Err(protocol_error()); }
    match frame.kind {
        Some(FrameKind::Response(r)) if (200..300).contains(&r.status) => Ok(()),
        Some(FrameKind::Response(r)) => Err(fail(502, format!("MuseAI chat send rejected: HTTP {}", r.status))),
        Some(FrameKind::Body(_)) => Ok(()),
        Some(FrameKind::Reset(_)) => Err(fail(502, "MuseAI chat send was reset")),
        _ => Err(protocol_error()),
    }
}
fn application_json(bytes: &[u8]) -> Result<Value, Failure> {
    let mut value: Value = serde_json::from_slice(bytes).map_err(|_| fail(502, "MuseAI gateway returned invalid JSON"))?;
    match value["ok"].as_bool() {
        Some(false) => Err(fail(502, "MuseAI rejected the application request")),
        Some(true) => value.get_mut("result").map(Value::take).ok_or_else(|| fail(502, "MuseAI application result is missing")),
        None => Ok(value),
    }
}

fn read_binary(ws: &mut Socket, cancel: &AtomicBool, deadline: Instant) -> Result<Vec<u8>, Failure> {
    loop {
        if cancel.load(Ordering::Relaxed) { return Err(fail(499, "MuseAI request cancelled by client")); }
        if Instant::now() >= deadline { return Err(fail(504, "MuseAI gateway response deadline exceeded")); }
        match ws.read() {
            Ok(WsMessage::Binary(b)) => return Ok(b.to_vec()),
            Ok(WsMessage::Ping(_)) => ws.flush().map_err(|_| protocol_error())?,
            Ok(WsMessage::Pong(_)) => {},
            // A socket polling interval is not the request deadline. Keep reading; never resend.
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted) => {},
            Err(_) | Ok(WsMessage::Close(_)) => return Err(fail(502, "MuseAI gateway connection closed or failed")),
            _ => return Err(protocol_error()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_mock_gateway<S, C>(server: S, client: C)
    where S: FnOnce(WebSocket<TcpStream>, snow::TransportState) + Send + 'static,
          C: FnOnce(&mut Gateway) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (client_noise, server_noise) = noise_pair();
        let thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            server(tungstenite::accept(stream).unwrap(), server_noise);
        });
        let stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (ws, _) = tungstenite::client(format!("ws://{address}").as_str(), MaybeTlsStream::Plain(stream)).unwrap();
        let mut gateway = Gateway { ws, noise: client_noise, stream: 0, chat_stream: None, cancel: Arc::new(AtomicBool::new(false)) };
        client(&mut gateway);
        thread.join().unwrap();
    }

    #[test]
    fn socket_read_timeout_does_not_abort_a_still_active_request() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            ws.send(WsMessage::Binary(vec![1, 2, 3].into())).unwrap();
        });
        let stream = TcpStream::connect(address).unwrap();
        let (mut client, _) = tungstenite::client(format!("ws://{address}").as_str(), MaybeTlsStream::Plain(stream)).unwrap();
        if let MaybeTlsStream::Plain(stream) = client.get_mut() { stream.set_read_timeout(Some(Duration::from_millis(10))).unwrap(); }
        let cancel = AtomicBool::new(false);
        let result = read_binary(&mut client, &cancel, Instant::now() + Duration::from_secs(2));
        server.join().unwrap();
        assert_eq!(result.unwrap(), vec![1, 2, 3]);
        assert_eq!(read_binary(&mut client, &cancel, Instant::now()).unwrap_err().status, 504);
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(read_binary(&mut client, &cancel, Instant::now() + Duration::from_secs(1)).unwrap_err().status, 499);
    }

    #[test]
    fn transport_round_trip_large_and_tampered_frames() {
        let (mut client, mut server) = noise_pair();
        let body = vec![42; 200_000];
        let packets = pack(&mut client, &body).unwrap();
        assert!(packets.len() > 1);
        let mut chunks = Assembly::default();
        let mut result = None;
        for packet in packets {
            assert!(packet.len() <= 65_535);
            result = chunks.push(unpack(&mut server, &packet).unwrap()).unwrap();
        }
        assert_eq!(result.unwrap(), body);
        let mut packet = pack(&mut client, b"hello").unwrap().remove(0);
        packet[0] ^= 1;
        assert!(unpack(&mut server, &packet).is_err());
    }

    #[test]
    fn invalid_chunks_fail_closed() {
        let mut a = Assembly::default();
        assert!(a.push(NoiseFrame { chunk_id: 1, chunk_index: 1, total_chunks: 2, payload: vec![1] }).is_err());
        assert!(a.push(NoiseFrame { chunk_id: 1, chunk_index: 0, total_chunks: u32::MAX, payload: vec![] }).is_err());
    }

    #[test]
    fn service_response_uses_payload_tag_one() {
        use prost::Message;
        let bytes = [0x0a, 0x02, 0x08, 0x07];
        let response = ServiceResponse::decode(&bytes[..]).unwrap();
        assert_eq!(ServiceFrame::decode(response.payload.as_slice()).unwrap().stream_id, 7);
    }

    #[test]
    fn only_known_chat_stream_frames_may_interleave_with_history() {
        let response = |id, status| ServiceFrame { stream_id: id, kind: Some(FrameKind::Response(ApplicationResponse { status, headers: vec![], body: vec![], end_body: true })) };
        assert!(check_other_frame(response(3, 200), Some(3)).is_ok());
        assert!(check_other_frame(response(9, 200), Some(3)).is_err());
        assert!(check_other_frame(response(3, 403), Some(3)).is_err());
        assert!(check_other_frame(ServiceFrame { stream_id: 3, kind: Some(FrameKind::Reset(Reset { code: 1, reason: String::new() })) }, Some(3)).is_err());
    }
    #[test]
    fn rsc_result_follows_the_pointer_then_falls_back() {
        let pointed = "0:{\"a\":\"$@1\",\"f\":\"\"}\n1:{\"success\":true,\"subscription\":{\"x\":1}}\n2:{\"subscription\":{\"x\":2}}\n";
        assert_eq!(rsc_result(pointed).unwrap()["subscription"]["x"], 1);
        let fallback = "0:{\"a\":\"$@9\"}\n1:I[\"chunk\",[]]\n2:{\"subscription\":{\"x\":2}}\n";
        assert_eq!(rsc_result(fallback).unwrap()["subscription"]["x"], 2);
        assert!(rsc_result("<html>nope</html>\n\n0:garbage").is_none());
        assert!(rsc_result("").is_none());
    }
    #[test]
    fn action_id_matches_only_the_subscription_reference() {
        let id = "40034617c46db812e70eec8e362677944a0a532adf";
        let js = format!("a=(0,r.createServerReference)( \"{id}\" ,r.callServer,void 0,r.findSourceMapURL,\"fetchSubscriptionAction\"),b=1");
        assert_eq!(action_id(&js).as_deref(), Some(id));
        let plain = format!("createServerReference(\"{id}\",r.callServer,void 0,\"fetchSubscriptionAction\")");
        assert_eq!(action_id(&plain).as_deref(), Some(id));
        let other = format!("(0,r.createServerReference)(\"{id}\",r.callServer,void 0,r.findSourceMapURL,\"somethingElse\"),x=\"fetchSubscriptionAction\"");
        assert!(action_id(&other).is_none());
        let both = format!("(0,r.createServerReference)(\"{}\",r.callServer,void 0,void 0,\"other\"),(0,r.createServerReference)(\"{id}\",r.callServer,void 0,void 0,\"fetchSubscriptionAction\")", "ab".repeat(21));
        assert_eq!(action_id(&both).as_deref(), Some(id));
        assert!(action_id("createServerReference(\"abc\",\"fetchSubscriptionAction\")").is_none());
    }
    #[test]
    fn https_json_body_rejects_oversize() {
        assert!(read_json_body(std::io::Cursor::new(vec![b'x'; MAX_BODY + 1])).is_err());
        assert_eq!(read_json_body(std::io::Cursor::new(br#"{"ok":true}"#)).unwrap()["ok"], true);
    }
    #[test]
    fn local_noise_gateway_reassembles_multichunk_response() {
        with_mock_gateway(|mut ws, mut noise| {
            let request = ws.read().unwrap().into_data();
            let frame = unpack(&mut noise, &request).unwrap();
            let envelope = ServiceRequest::decode(frame.payload.as_slice()).unwrap();
            let id = ServiceFrame::decode(envelope.payload.as_slice()).unwrap().stream_id;
            let response = ServiceFrame { stream_id: id, kind: Some(FrameKind::Response(ApplicationResponse { status: 200, headers: vec![], body: format!("{{\"text\":\"{}\"}}", "x".repeat(150_000)).into_bytes(), end_body: true })) };
            let bytes = ServiceResponse { payload: response.encode_to_vec() }.encode_to_vec();
            for wire in pack(&mut noise, &bytes).unwrap() { ws.send(WsMessage::Binary(wire.into())).unwrap(); }
        }, |gateway| {
            let result = gateway.request("GET", "/chat/history", None).unwrap();
            assert_eq!(result["text"].as_str().unwrap().len(), 150_000);
        });
    }
    #[test]
    fn local_noise_gateway_rejects_unknown_and_reset_streams() {
        for unknown in [true, false] {
            with_mock_gateway(move |mut ws, mut noise| {
                let request = ws.read().unwrap().into_data();
                let frame = unpack(&mut noise, &request).unwrap();
                let envelope = ServiceRequest::decode(frame.payload.as_slice()).unwrap();
                let id = ServiceFrame::decode(envelope.payload.as_slice()).unwrap().stream_id;
                let response = ServiceFrame { stream_id: if unknown { id + 1 } else { id }, kind: Some(FrameKind::Reset(Reset { code: 1, reason: String::new() })) };
                let bytes = ServiceResponse { payload: response.encode_to_vec() }.encode_to_vec();
                for wire in pack(&mut noise, &bytes).unwrap() { ws.send(WsMessage::Binary(wire.into())).unwrap(); }
            }, |gateway| {
                assert_eq!(gateway.request("GET", "/chat/history", None).unwrap_err().status, 502);
            });
        }
    }

    #[test]
    fn application_result_envelope_is_unwrapped_and_errors_fail() {
        assert_eq!(application_json(br#"{"ok":true,"result":{"session_id":"test"}}"#).unwrap()["session_id"], "test");
        assert!(application_json(br#"{"ok":false,"error":"private detail"}"#).is_err());
        assert_eq!(application_json(br#"{"chat_events":[]}"#).unwrap()["chat_events"], json!([]));
    }

    #[test]
    fn cookie_import_filters_domains_and_http_only_comments() {
        let jar = "# Netscape HTTP Cookie File\n#HttpOnly_.muse.ai\tTRUE\t/\tTRUE\t0\thatch_sess\tsecret\n.evilmuse.ai\tTRUE\t/\tTRUE\t0\tbad\tleak\n.muse.ai\tTRUE\t/\tTRUE\t1\texpired\told\n";
        assert_eq!(cookies(jar).unwrap(), "hatch_sess=secret");
        assert!(cookies("hatch_sess=x\r\nInjected: y").is_err());
        assert!(cookies("{\"cookies\":{\"datr\":\"x\"}}").is_err());
    }

    #[test]
    #[ignore = "explicit live-account opt-in only; creates and removes one test session"]
    fn live_native_round_trip() {
        let root = crate::usage::dirs_home().join(".config/muse-cli");
        let text = std::fs::read_to_string(root.join("cookies.txt")).expect("Existing Muse cookie jar required");
        let cookie = cookies(&text).unwrap();
        let id = identity(&cookie).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut gateway = Gateway::connect(&id, cancel).unwrap();
        let session = gateway.request("POST", "/session/start", Some(&json!({"method":"/api/session/start","params":{"origin":"fresh","lifecycle":"persistent","title":"ccat native transport verification"}}))).unwrap();
        let sid = session["session_id"].as_str().or_else(|| session["id"].as_str()).expect("Session ID").to_string();
        let path = format!("/chat/history?limit=10&session_id={}", encode_query(&sid));
        let outcome = (|| -> Result<(), Failure> {
            let baseline = gateway.request("GET", &path, None)?["chat_events"].as_array().map(|a| a.iter().filter_map(|e| e["seq"].as_i64()).max().unwrap_or(0)).unwrap_or(0);
            gateway.open("POST", "/chat/stream", Some(&json!({"items":[{"type":"text","text":"Reply with exactly CCAT_NATIVE_OK. Do not use tools."}],"node_id":crate::common::random_hex(8),"capabilities":{},"session_id":sid})))?;
            let deadline = Instant::now() + Duration::from_secs(120);
            while Instant::now() < deadline {
                let history = gateway.request("GET", &path, None)?;
                if let Some(events) = history["chat_events"].as_array() {
                    for e in events {
                        if e["seq"].as_i64().unwrap_or(0) > baseline && e["event_name"] == "message.assistant" {
                            let p = &e["payload"];
                            if p["status"].as_str().is_some_and(|s| !s.is_empty()) {
                                let text = p["display_text"].as_str().or_else(|| p["content"].as_str()).unwrap_or("");
                                if text.contains("CCAT_NATIVE_OK") { return Ok(()); }
                                return Err(fail(502, "Muse replied but did not return the verification marker"));
                            }
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(fail(504, "Muse live verification timed out"))
        })();
        let cleanup = gateway.request("POST", "/session/delete", Some(&json!({"method":"/api/session/delete","session_id":sid})));
        assert!(cleanup.is_ok(), "Could not clean up ccat native verification session");
        assert!(outcome.is_ok(), "Native round trip failed: {outcome:?}");
    }

    #[test]
    #[ignore = "explicit live-account opt-in; probes whether isolated history exposes partial replies"]
    fn live_isolated_history_progress() {
        let root = crate::usage::dirs_home().join(".config/muse-cli");
        let text = std::fs::read_to_string(root.join("cookies.txt")).expect("Existing Muse cookie jar required");
        let id = identity(&cookies(&text).unwrap()).unwrap();
        let mut gateway = Gateway::connect(&id, Arc::new(AtomicBool::new(false))).unwrap();
        let started = gateway.request("POST", "/session/start", Some(&json!({"method":"/api/session/start","params":{"origin":"fresh","lifecycle":"persistent","title":"ccat progress probe"}}))).unwrap();
        let sid = started["session_id"].as_str().unwrap().to_string();
        let path = format!("/chat/history?limit=20&session_id={}", encode_query(&sid));
        let result = (|| -> Result<Vec<(String, usize)>, Failure> {
            gateway.open("POST", "/chat/stream", Some(&json!({"items":[{"type":"text","text":"Write a short paragraph of about 100 words about a fictional garden."}],"node_id":crate::common::random_hex(8),"capabilities":{},"session_id":sid})))?;
            let mut snapshots = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(120);
            while Instant::now() < deadline {
                let history = gateway.request("GET", &path, None)?;
                for event in history["chat_events"].as_array().into_iter().flatten().filter(|e| e["event_name"] == "message.assistant") {
                    let payload = &event["payload"];
                    let status = payload["status"].as_str().unwrap_or("").to_string();
                    let length = payload["display_text"].as_str().or_else(|| payload["content"].as_str()).unwrap_or("").len();
                    let sample = (status, length);
                    if snapshots.last() != Some(&sample) { snapshots.push(sample.clone()); }
                    if sample.0 == "completed" { return Ok(snapshots); }
                }
                std::thread::sleep(Duration::from_millis(300));
            }
            Err(fail(504, "Progress probe timed out"))
        })();
        let cleanup = gateway.request("POST", "/session/delete", Some(&json!({"method":"/api/session/delete","session_id":sid})));
        assert!(cleanup.is_ok(), "Progress probe cleanup failed");
        let snapshots = result.unwrap();
        println!("Isolated history status and byte-length snapshots: {snapshots:?}");
    }

    fn noise_pair() -> (snow::TransportState, snow::TransportState) {
        let params: snow::params::NoiseParams = "Noise_XX_25519_AESGCM_SHA256".parse().unwrap();
        let keys_a = snow::Builder::new(params.clone()).generate_keypair().unwrap();
        let keys_b = snow::Builder::new(params.clone()).generate_keypair().unwrap();
        let mut a = snow::Builder::new(params.clone()).local_private_key(&keys_a.private).build_initiator().unwrap();
        let mut b = snow::Builder::new(params).local_private_key(&keys_b.private).build_responder().unwrap();
        let mut wire = [0; 1024];
        let mut plain = [0; 1024];
        let n = a.write_message(&[], &mut wire).unwrap();
        b.read_message(&wire[..n], &mut plain).unwrap();
        let n = b.write_message(&[], &mut wire).unwrap();
        a.read_message(&wire[..n], &mut plain).unwrap();
        let n = a.write_message(&[], &mut wire).unwrap();
        b.read_message(&wire[..n], &mut plain).unwrap();
        (a.into_transport_mode().unwrap(), b.into_transport_mode().unwrap())
    }
}
