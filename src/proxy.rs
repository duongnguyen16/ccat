//! A local Anthropic Messages endpoint for Claude Code that routes each model tier to its own provider:
//! Antigravity (Claude Code → Cloud Code Assist `streamGenerateContent` → Anthropic SSE), an Anthropic-compatible
//! endpoint (forwarded with its key), or the official API (forwarded with Claude Code's own login).
//! Also serves `/mcp` (see `mcp`) for Antigravity abilities Claude Code has no built-in tool for.
//! Binds 127.0.0.1 on a random port. Routes that spend ccat's accounts or keys need the per-launch token;
//! official routes only ever carry the caller's own credentials, so they don't.

use crate::antigravity::{self, family, Account, Failure, Pool};
use crate::chatgpt::{self, ChatgptAccount, ChatgptModel};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Map, Value};
use std::{
    collections::{hash_map::DefaultHasher, HashMap, HashSet},
    hash::{Hash, Hasher},
    io::{self, BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, RwLock,
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

#[derive(Clone)]
pub struct Proxy {
    pub url: String,
    pub token: String,
    /// Swappable while the session runs: a model switched in ccat's settings reaches the next request.
    pub routes: Arc<RwLock<Vec<Route>>>,
    /// Emails of the Antigravity accounts to skip; swappable like `routes`.
    pub disabled: Arc<RwLock<HashSet<String>>>,
    /// The ChatGPT accounts (their on/off switches included); swappable like `routes`.
    pub chatgpt: Arc<RwLock<Vec<ChatgptAccount>>>,
    /// What the config knows of each ChatGPT model (efforts, fast tier); swappable too.
    pub chatgpt_models: Arc<RwLock<Vec<ChatgptModel>>>,
    /// Answer usage limits at once instead of holding the request until a route can run; switchable too.
    pub fail_on_limit: Arc<AtomicBool>,
    ctx: Arc<Ctx>,
}

impl Proxy {
    /// Fresh usage (the tray's `usage.json`): an account it shows with quota again, fetched after its block was set,
    /// goes back into rotation now rather than when the block's guess runs out. So a tier returns to its first
    /// route as soon as that resets, and held requests try again at once.
    pub fn recheck(&self, u: &crate::usage::Usage) {
        let ctx = &self.ctx;
        let mut lifted = false;
        for (email, src) in u.antigravity.iter().filter(|(_, s)| s.error.is_empty()) {
            // The second group's buckets cover Claude and GPT-OSS alike.
            for (group, fams) in [("Gemini", &["gemini"][..]), ("Claude", &["claude", "gpt"][..])] {
                let bars: Vec<_> = src.bars.iter().filter(|b| b.label.starts_with(group)).collect();
                if !bars.is_empty() && bars.iter().all(|b| !b.out()) {
                    for f in fams {
                        lifted |= ctx.pool.unblock(email, f, src.fetched);
                    }
                }
            }
        }
        let g = &u.chatgpt;
        if g.error.is_empty() && !g.bars.is_empty() && g.bars.iter().all(|b| !b.out()) {
            lifted |= ctx.gpt.unblock(g.fetched);
        }
        *ctx.plan_backs.lock().unwrap() = [plan_back(&u.claude), plan_back(&u.chatgpt)];
        if lifted {
            ctx.wake.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Rewrites this session's live file, so the usage window knows it still runs.
    pub fn heartbeat(&self) {
        self.ctx.note(|_| {});
    }
}

/// When a plan is back by its usage bars: the latest reset among those used up (a used-up week outlasts the 5
/// hours); 0 when none is, or the fetch failed.
fn plan_back(src: &crate::usage::Source) -> u64 {
    if !src.error.is_empty() {
        return 0;
    }
    src.bars.iter().filter(|b| b.out()).map(|b| b.resets).max().unwrap_or(0)
}

/// The config's name for a route: provider ("" = Claude) and model.
fn hop(u: &Upstream, model: &str) -> crate::live::Hop {
    let provider = match u {
        Upstream::Official => String::new(),
        Upstream::Endpoint { name, .. } => name.clone(),
        Upstream::Antigravity => crate::AG_NAME.into(),
        Upstream::ChatGPT => crate::GPT_NAME.into(),
    };
    crate::live::Hop { provider, model: model.to_string() }
}

/// A request held at a usage limit, counted in the live file while it waits.
struct Held<'a> {
    ctx: &'a Ctx,
    tier: &'static str,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.ctx.note(|s| {
            let t = s.tiers.entry(self.tier.to_string()).or_default();
            t.held = t.held.saturating_sub(1);
            if t.held == 0 {
                t.waiting.clear();
            }
        });
    }
}

/// The official API: requests go there with the login Claude Code itself sends.
const ANTHROPIC_API: &str = "https://api.anthropic.com";

/// The official API, or in tests a fake one.
fn anthropic_api() -> String {
    #[cfg(test)]
    if let Some(u) = tests::FAKE_API.lock().unwrap().clone() {
        return u;
    }
    ANTHROPIC_API.into()
}

#[derive(Clone, PartialEq)]
pub enum Upstream {
    Official,
    /// `name` is the provider's, for the token log.
    Endpoint { name: String, url: String, key: String },
    Antigravity,
    /// The ChatGPT plan: Anthropic Messages translated to the Responses API.
    ChatGPT,
}

/// Names the provider and never the key, so a route can be logged.
impl std::fmt::Debug for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            Upstream::Official => "Claude",
            Upstream::Endpoint { name, .. } => name,
            Upstream::Antigravity => "Antigravity",
            Upstream::ChatGPT => "ChatGPT",
        })
    }
}

/// One model tier: the names Claude Code may send for it (fixed at launch), where it goes, the model it runs there
/// ("" keeps the asked name: an official tier left at Claude Code's own pick), and where it goes when that fails.
#[derive(Clone, Debug)]
pub struct Route {
    pub tier: &'static str,
    pub names: Vec<String>,
    pub upstream: Upstream,
    pub model: String,
    /// Tried in order when the one before fails before anything streamed.
    pub fallbacks: Vec<(Upstream, String)>,
}

/// Where one request goes: upstream, model, tier (for the token log), and the fallbacks.
type Target = (Upstream, String, &'static str, Vec<(Upstream, String)>);

struct Ctx {
    pool: Pool,
    gpt: chatgpt::Pool,
    token: String,
    agent_id: String,
    trajectory: String,
    step: AtomicU64,
    /// Gemini puts thought signatures on function calls, which Anthropic tool_use blocks can't carry: tool id → tagged signature.
    sigs: Mutex<HashMap<String, String>>,
    routes: Arc<RwLock<Vec<Route>>>,
    /// For official and endpoint tiers: one agent, so their connections (and TLS sessions) are reused.
    http: ureq::Agent,
    /// The folder Claude Code runs in: MCP tools resolve relative paths against it.
    folder: PathBuf,
    /// Tokens per text byte of an Anthropic request, learned per model family from Google's real prompt counts.
    ratio: Mutex<HashMap<&'static str, f64>>,
    fail_on_limit: Arc<AtomicBool>,
    /// What this session's tiers are doing, written to its live file (see `live`).
    live: Mutex<crate::live::Session>,
    /// Bumped when a block is lifted early: held requests try again at once.
    wake: AtomicU64,
    /// When Claude Code's plan and the ChatGPT plan are back, by the last usage seen (see `plan_back`).
    plan_backs: Mutex<[u64; 2]>,
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
    /// When route `u`'s plan should be back by its usage bars, if that is still ahead; else 0.
    fn plan_back(&self, u: &Upstream, now: u64) -> u64 {
        let [claude, gpt] = *self.plan_backs.lock().unwrap();
        let at = match u {
            Upstream::Official => claude,
            Upstream::ChatGPT => gpt,
            _ => 0,
        };
        if at > now { at } else { 0 }
    }

    fn note(&self, f: impl FnOnce(&mut crate::live::Session)) {
        let mut g = self.live.lock().unwrap();
        f(&mut g);
        g.updated = crate::now();
        crate::live::write(&g);
    }

    /// Where a model name goes. An exact name wins, then an explicit `@tier` suffix, then the tier named in it
    /// (`claude-haiku-…` → Haiku), then — for names Claude Code picks on its own — the official API when a tier
    /// uses it (the login is there), else the first tier. No routes at all: the name runs on Antigravity as-is.
    fn resolve(&self, asked: &str) -> Target {
        let routes = self.routes.read().unwrap();
        let (base, suffix) = asked.rsplit_once('@').unwrap_or((asked, ""));
        let low = base.to_lowercase();
        let route = (routes.iter().find(|r| r.names.iter().any(|n| n == asked)))
            .or_else(|| routes.iter().find(|r| !suffix.is_empty() && r.tier == suffix))
            .or_else(|| routes.iter().find(|r| low.contains(r.tier)));
        // An empty model is Claude Code's pick: the name it sent. Fallbacks too (an empty one reached the API as "").
        let pick = |m: &str| if m.is_empty() { asked.to_string() } else { m.to_string() };
        match route {
            Some(r) => (r.upstream.clone(), pick(&r.model), r.tier, r.fallbacks.iter().map(|(u, m)| (u.clone(), pick(m))).collect()),
            // The official API knows (or rejects) the name itself.
            None if routes.iter().any(|r| r.upstream == Upstream::Official) => (Upstream::Official, asked.to_string(), "", Vec::new()),
            None => routes.first().map_or((Upstream::Antigravity, asked.to_string(), "", Vec::new()), |r| (r.upstream.clone(), r.model.clone(), r.tier, r.fallbacks.clone())),
        }
    }

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

/// `accounts` serve the Antigravity routes, `chatgpt` the ChatGPT ones; `routes` say where each tier goes (see `Ctx::resolve`).
pub fn start(accounts: Vec<Account>, chatgpt: Vec<ChatgptAccount>, models: Vec<ChatgptModel>, routes: Vec<Route>, folder: PathBuf) -> io::Result<Proxy> {
    serve_with(Pool::new(accounts), chatgpt::Pool::new(chatgpt, models), routes, folder)
}

fn serve_with(pool: Pool, gpt: chatgpt::Pool, routes: Vec<Route>, folder: PathBuf) -> io::Result<Proxy> {
    let routes = Arc::new(RwLock::new(routes));
    let fail_on_limit = Arc::new(AtomicBool::new(false));
    let (disabled, chatgpt, chatgpt_models) = (pool.disabled.clone(), gpt.accounts.clone(), gpt.models.clone());
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let url = format!("http://{}", listener.local_addr()?);
    let token = antigravity::random_hex(24);
    let ctx = Arc::new(Ctx {
        pool,
        gpt,
        token: token.clone(),
        agent_id: antigravity::uuid(),
        trajectory: antigravity::uuid(),
        step: AtomicU64::new(0),
        sigs: Mutex::new(HashMap::new()),
        routes: routes.clone(),
        // A held request may wait for hours (ccat lifts Claude Code's timeout), so an upstream that never answers
        // must fail on its own: as long as Claude Code's default wait for an answer.
        http: ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(600)))
            .build()
            .into(),
        folder,
        ratio: Mutex::new(HashMap::new()),
        fail_on_limit: fail_on_limit.clone(),
        live: Default::default(),
        wake: AtomicU64::new(0),
        plan_backs: Default::default(),
    });
    let handle = ctx.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let ctx = ctx.clone();
            std::thread::spawn(move || serve(conn, &ctx));
        }
    });
    Ok(Proxy { url, token, routes, disabled, chatgpt, chatgpt_models, fail_on_limit, ctx: handle })
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
    // With an official tier Claude Code keeps its login in `authorization`, so the token comes as a custom header.
    let bearer = req.header("authorization").and_then(|a| a.strip_prefix("Bearer "));
    let authed = [req.header("x-ccat-token"), req.header("x-api-key"), bearer].contains(&Some(ctx.token.as_str()));
    let denied = |s: &mut TcpStream| respond_error(s, &Failure { status: 401, message: "ccat proxy: wrong token".into(), retry_after: None });
    match (req.method.as_str(), req.path.split('?').next().unwrap_or("")) {
        ("POST", path @ ("/v1/messages" | "/v1/messages/count_tokens")) => {
            let body: Value = match serde_json::from_slice(&req.body) {
                Ok(v) => v,
                Err(e) => return respond_error(&mut s, &Failure { status: 400, message: format!("body is not JSON: {e}"), retry_after: None }),
            };
            // Claude Code may tag a name with a context hint such as `[1m]`.
            let asked = body["model"].as_str().unwrap_or("").split('[').next().unwrap_or("").trim().to_string();
            let counted = path == "/v1/messages";

            // In order, and only while nothing has reached Claude Code: a stream that breaks halfway can't start over.
            // Every try out of quota: the request waits, open, until one can run (or Claude Code stops waiting), so an
            // agent pauses at a usage limit instead of stopping.
            let hold = !ctx.fail_on_limit.load(Ordering::Relaxed);
            let mut held: Option<Held> = None;
            let mut passes = 0u32;
            loop {
                // Resolved on every pass: a route changed in Settings reaches a request already waiting.
                let (up, model, tier, fallbacks) = ctx.resolve(&asked);
                // Each try spends ccat's keys or accounts unless it's Claude: those need the token.
                let tries: Vec<(Upstream, String)> = std::iter::once((up, model)).chain(fallbacks).filter(|(u, _)| *u == Upstream::Official || authed).collect();
                if tries.is_empty() {
                    return denied(&mut s);
                }
                let mut fails = Vec::new();
                for (n, (u, m)) in tries.iter().enumerate() {
                    let last = n + 1 == tries.len();
                    if n > 0 {
                        antigravity::log(&format!("{asked} [{tier}]: falling back to {m} on {u:?}"));
                    }
                    let may_fail = if !last { Fail::Fallback } else if hold { Fail::Limit } else { Fail::Never };
                    match run(&mut s, ctx, &req, &body, &asked, u, m, tier, counted, may_fail) {
                        Ok(()) => {
                            if !tier.is_empty() {
                                ctx.note(|s| {
                                    let t = s.tiers.entry(tier.to_string()).or_default();
                                    (t.used, t.at) = (Some(hop(u, m)), crate::now());
                                });
                            }
                            return;
                        }
                        Err(f) => {
                            antigravity::log(&format!("{asked} [{tier}]: {m} failed with {} {}", f.status, f.message));
                            fails.push(f);
                        }
                    }
                }
                let Some(last) = fails.last() else { return };
                if fails.iter().any(|f| f.status != 429) {
                    return respond_error(&mut s, last);
                }
                // When each route should be back: what it said, else what the tray's usage shows for its plan (the
                // 5-hour reset, or the weekly one once the week is used up), else unknown. The request waits for the
                // soonest; a route with no idea (Claude's bare "429 Error" can pass in seconds) is tried again soon,
                // backing off 15 s, 30 s, 60 s … and every route at least every 5 minutes.
                let now = crate::now();
                let backs: Vec<u64> = tries.iter().zip(&fails).map(|((u, _), f)| f.retry_after.map(|r| now + r).unwrap_or_else(|| ctx.plan_back(u, now))).collect();
                let soon = 15u64 << passes.min(4);
                passes += 1;
                let wait = backs.iter().map(|b| if *b > now { b - now } else { soon }).min().unwrap_or(soon).clamp(15, 300);
                antigravity::log(&format!("{asked} [{tier}]: every route is at its usage limit; holding the request, next try in {wait}s"));
                // Tell the usage window what this tier waits for, and when each route should be back.
                if !tier.is_empty() {
                    let waiting = tries.iter().zip(&backs).map(|((u, m), b)| (hop(u, m), *b)).collect();
                    let first = held.is_none();
                    ctx.note(|s| {
                        let t = s.tiers.entry(tier.to_string()).or_default();
                        t.held += first as u32;
                        t.waiting = waiting;
                    });
                    if first {
                        held = Some(Held { ctx, tier });
                    }
                }
                if !still_waiting(&s, wait, &ctx.wake) {
                    antigravity::log(&format!("{asked} [{tier}]: Claude Code stopped waiting; request dropped"));
                    return;
                }
            }
        }
        (_, "/mcp") if !authed => denied(&mut s),
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
        // Anything else Claude Code asks its API for, with its own login, goes to the real one.
        _ if bearer.is_some_and(|b| b != ctx.token) => drop(forward(&mut s, &ctx.http, &req, &anthropic_api(), None, req.body.clone(), None, Fail::Never)),
        _ if !authed => denied(&mut s),
        _ => {
            antigravity::log(&format!("404 {} {} (Claude Code asked for something the proxy doesn't serve)", req.method, req.path));
            respond_error(&mut s, &Failure { status: 404, message: format!("ccat proxy has no {} {}", req.method, req.path), retry_after: None })
        }
    }
}

/// Sleeps `secs` while Claude Code keeps the connection open; false as soon as it closes it (Esc, or a timeout). Ends
/// early when `wake` moves: a route came back.
fn still_waiting(s: &TcpStream, secs: u64, wake: &AtomicU64) -> bool {
    let until = std::time::Instant::now() + Duration::from_secs(secs);
    let at = wake.load(Ordering::Relaxed);
    if s.set_nonblocking(true).is_err() {
        return false;
    }
    let open = loop {
        match s.peek(&mut [0u8; 1]) {
            Ok(0) => break false,
            Err(e) if e.kind() != io::ErrorKind::WouldBlock => break false,
            _ if std::time::Instant::now() >= until || wake.load(Ordering::Relaxed) != at => break true,
            _ => std::thread::sleep(Duration::from_secs(2)),
        }
    };
    open && s.set_nonblocking(false).is_ok()
}

/// Runs a request on one upstream. With `may_fail` a failure that left nothing written comes back as `Err`
/// (so a fallback can take over); without, it answers Claude Code with the upstream's own error.
#[allow(clippy::too_many_arguments)]
fn run(s: &mut TcpStream, ctx: &Ctx, req: &Req, body: &Value, asked: &str, up: &Upstream, model: &str, tier: &'static str, counted: bool, may_fail: Fail) -> Result<(), Failure> {
    match up {
        Upstream::Official => {
            let meter = counted.then_some((tier, "Claude", model));
            forward(s, &ctx.http, req, &anthropic_api(), None, upstream_body(body.clone(), model), meter, may_fail)
        }
        Upstream::Endpoint { name, url, key } => {
            let base = url.trim().trim_end_matches('/');
            let meter = counted.then_some((tier, name.as_str(), model));
            forward(s, &ctx.http, req, base.strip_suffix("/v1").unwrap_or(base), Some(key), upstream_body(body.clone(), model), meter, may_fail)
        }
        Upstream::Antigravity if !counted => {
            count_tokens(s, ctx, body, model);
            Ok(())
        }
        Upstream::Antigravity => messages(s, ctx, body, asked, model.to_string(), tier, may_fail),
        // The count has no ChatGPT endpoint: the estimate, calibrated on this session's real answers.
        Upstream::ChatGPT if !counted => {
            respond(s, 200, &json!({ "input_tokens": ctx.estimate("gpt", body) }), "");
            Ok(())
        }
        Upstream::ChatGPT => chatgpt_messages(s, ctx, req, body, asked, model, tier, may_fail),
    }
}

/// Worth trying the fallback for: the upstream is down, out of quota, or doesn't know the model or the key.
/// A malformed request (400) would fail there too.
fn worth_falling_back(status: u16) -> bool {
    matches!(status, 401 | 403 | 404 | 408 | 429) || status >= 500
}

/// Which failures `run` hands back instead of answering Claude Code with them (only while nothing was written).
#[derive(Clone, Copy)]
enum Fail {
    /// None: the upstream's own answer goes through as it is (the last try).
    Never,
    /// Those a fallback can help with (see `worth_falling_back`).
    Fallback,
    /// Usage limits only: the last try of a request that waits them out. Anything else still goes through as it is
    /// (Claude Code acts on its own errors: compacting on a too-long prompt, refreshing its login on a 401).
    Limit,
}

impl Fail {
    fn takes(self, status: u16) -> bool {
        match self {
            Fail::Never => false,
            Fail::Fallback => worth_falling_back(status),
            Fail::Limit => status == 429,
        }
    }
}

/// The request body for an official or endpoint route: the tier's real model, and no thinking blocks another
/// provider signed (Antigravity's `ag:` and ChatGPT's `oa:` tags, or none at all): Anthropic rejects a signature it didn't make.
/// ponytail: an assistant turn that was nothing but such thinking would be left empty; Claude Code always follows
/// thinking with text or a tool call, so it hasn't come up.
fn upstream_body(mut body: Value, model: &str) -> Vec<u8> {
    if body.is_null() {
        return Vec::new();
    }
    body["model"] = json!(model);
    for m in body["messages"].as_array_mut().into_iter().flatten() {
        if let Some(blocks) = m["content"].as_array_mut() {
            blocks.retain(|b| b["type"] != "thinking" || b["signature"].as_str().is_some_and(|s| !s.is_empty() && !s.starts_with("ag:") && !s.starts_with("oa:")));
        }
    }
    body.to_string().into_bytes()
}

/// Token usage read off an answer as it passes through: SSE `message_start` / `message_delta` events, or a plain
/// JSON message. The highest value seen per field wins (`output_tokens` grows through the stream).
#[derive(Default)]
struct Meter {
    sse: bool,
    tail: Vec<u8>,
    usage: Map<String, Value>,
}

impl Meter {
    fn feed(&mut self, bytes: &[u8]) {
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

    fn take(&mut self, v: &Value) {
        let u = if v["message"]["usage"].is_object() { &v["message"]["usage"] } else { &v["usage"] };
        for (k, n) in u.as_object().into_iter().flatten().filter(|(_, n)| n.is_u64()) {
            if self.usage.get(k).and_then(Value::as_u64) < n.as_u64() {
                self.usage.insert(k.clone(), n.clone());
            }
        }
    }

    fn finish(mut self) -> Value {
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
fn record(tier: &str, provider: &str, model: &str, usage: &Value) {
    let n = |k: &str| usage[k].as_u64().unwrap_or(0);
    if cfg!(test) || n("input_tokens") + n("output_tokens") + n("cache_read_input_tokens") == 0 {
        return;
    }
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let line = json!({ "t": t, "tier": tier, "provider": provider, "model": model, "input": n("input_tokens"), "output": n("output_tokens"),
                       "cache_read": n("cache_read_input_tokens"), "cache_write": n("cache_creation_input_tokens") });
    let path = antigravity::log_path().with_file_name("tokens.jsonl");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(format!("{line}\n").as_bytes()); // one write per line: several sessions can append at once
    }
}

/// Official and endpoint routes: the request goes on unchanged except for `body` (and the endpoint's own key),
/// and the answer, SSE included, streams back as it arrives, rate-limit headers and all. With `meter` (tier,
/// provider, model) the answer's token usage goes to the token log.
#[allow(clippy::too_many_arguments)]
fn forward(s: &mut TcpStream, agent: &ureq::Agent, req: &Req, base: &str, key: Option<&str>, body: Vec<u8>, meter: Option<(&str, &str, &str)>, may_fail: Fail) -> Result<(), Failure> {
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
            antigravity::log(&format!("{base} answered 429: {}", said.join(" ")));
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
fn count_tokens(s: &mut TcpStream, ctx: &Ctx, req: &Value, model: &str) {
    let fam = family(model);
    let exact = (fam == "gemini").then(|| google_count(ctx, req, model)).flatten();
    respond(s, 200, &json!({ "input_tokens": exact.unwrap_or_else(|| ctx.estimate(fam, req)) }), "");
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

#[allow(clippy::too_many_arguments)]
fn messages(s: &mut TcpStream, ctx: &Ctx, req: &Value, asked: &str, model: String, tier: &str, may_fail: Fail) -> Result<(), Failure> {
    if model.is_empty() {
        respond_error(s, &Failure { status: 400, message: "no model in request".into(), retry_after: None });
        return Ok(());
    }
    let model = if web_search(req).is_some() && family(&model) != "gemini" { SEARCH_MODEL.to_string() } else { model };
    let stream = req["stream"].as_bool().unwrap_or(false);
    let (cca, beta) = build(req, &model, ctx);
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
        let mut tx = Tx::new(&model, asked, stream.then_some(&mut *s), &ctx.sigs);
        match tx.run(reader) {
            Ok(()) => {
                let msg = tx.message();
                let prompt = tx.usage.0;
                drop(tx);
                record(tier, "Antigravity", &model, &msg["usage"]);
                // Search answers carry Google's page text, which the request never had: they'd skew the ratio.
                if web_search(req).is_none() {
                    ctx.learn(family(&model), req, prompt);
                }
                if !stream {
                    respond(s, 200, &msg, "");
                }
                return Ok(());
            }
            Err(Stop::Retry(f, block)) => {
                let kind = if web_search(req).is_some() { " [web search]" } else { "" };
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
            Err(Stop::Gone) => return Ok(()),
        }
    }
    match last {
        Some(f) if may_fail.takes(f.status) => Err(f),
        Some(f) => Ok(respond_error(s, &f)),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------- Anthropic → ChatGPT (Responses API)

/// What the incoming HTTP request says beyond its body.
struct Hints {
    /// Claude Code's session id (else a hash of the system prompt): the prompt-cache key, so a session's turns share a cache.
    session: String,
}

fn gpt_hints(req: &Req, body: &Value) -> Hints {
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
fn wanted_effort(req: &Value, meta: Option<&chatgpt::ChatgptModel>) -> String {
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
fn response_input(req: &Value, wrap: bool) -> Vec<Value> {
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
                                Some("text") => text.extend(c["text"].as_str().map(String::from)),
                                Some("image") => images.extend(input_image(&c)),
                                _ => {}
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

/// Client tools as function tools (or inside one namespace when the API refused them bare); a web-search server tool as
/// `web_search`; other server tools have no equivalent and are dropped.
fn response_tools(req: &Value, wrap: bool) -> Vec<Value> {
    let tools = req["tools"].as_array().cloned().unwrap_or_default();
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
fn responses_body(req: &Value, model: &str, meta: Option<&chatgpt::ChatgptModel>, hints: &Hints, pool: &chatgpt::Pool, codex: bool) -> Value {
    // What the official route refused is learned per process, but only that route is affected by it.
    let wrap = pool.wrap.load(Ordering::Relaxed) && !codex;
    let instructions = system_parts(req).iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n\n");
    let mut body = json!({
        "model": model, "instructions": instructions, "input": response_input(req, wrap), "store": false, "stream": true,
        "include": ["reasoning.encrypted_content"], "reasoning": { "effort": wanted_effort(req, meta), "summary": "auto" },
    });
    let tools = response_tools(req, wrap);
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        let choice = &req["tool_choice"];
        body["tool_choice"] = match choice["type"].as_str() {
            Some("any") => json!("required"),
            Some("tool") => json!({ "type": "function", "name": choice["name"] }),
            Some("none") => json!("none"),
            _ => json!("auto"),
        };
    }
    if codex || !pool.no_cache_key.load(Ordering::Relaxed) {
        body["prompt_cache_key"] = json!(hints.session);
    }
    if codex {
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
fn chatgpt_messages(s: &mut TcpStream, ctx: &Ctx, req: &Req, body: &Value, asked: &str, model: &str, tier: &str, may_fail: Fail) -> Result<(), Failure> {
    if model.is_empty() {
        respond_error(s, &Failure { status: 400, message: "no model in request".into(), retry_after: None });
        return Ok(());
    }
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
        let payload = responses_body(body, model, meta.as_ref(), &hints, pool, acc.codex());
        // (what an unsupported_capability named, seconds to block the account, the failure)
        let (refused, block, failure) = match pool.open(&acc, &payload, &hints.session) {
            Err(d) => (d.param, d.block, d.failure),
            Ok(reader) => {
                let mut tx = Tx::new(model, asked, stream.then_some(&mut *s), &ctx.sigs);
                tx.wrap = pool.wrap.load(Ordering::Relaxed) && !acc.codex();
                match tx.run_responses(reader) {
                    Ok(()) => {
                        let msg = tx.message();
                        let prompt = tx.usage.0;
                        drop(tx);
                        record(tier, "ChatGPT", model, &msg["usage"]);
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
        antigravity::log(&format!("{model} via {}: {} {}", acc.email, failure.status, failure.message));
        // An optional field the plan route doesn't take: drop it for the rest of this process and try again.
        let has_fns = payload["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["type"] == "function"));
        let flag = match refused.as_str() {
            _ if acc.codex() => None,
            p if p.contains("tools") && has_fns => Some(&pool.wrap),
            p if p.contains("prompt_cache_key") => Some(&pool.no_cache_key),
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
    /// Signature for the open thinking block (already tagged: `ag:…` or `oa:…`), emitted as `signature_delta` when it closes.
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
    // ---- ChatGPT (Responses API) only
    /// Arguments streamed so far into the open function call.
    args: String,
    /// `response.completed` / `response.incomplete` arrived.
    done: bool,
    max_tokens: bool,
    /// Tools go out in a namespace, so a returned call name may carry its prefix.
    wrap: bool,
    /// What an `unsupported_capability` failure named (`error.param`): the caller may retry without it.
    param: String,
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
            args: String::new(),
            done: false,
            max_tokens: false,
            wrap: false,
            param: String::new(),
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
    fn open_block(&mut self, kind: &'static str) -> Result<(), Stop> {
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

    fn delta(&mut self, kind: &'static str, text: &str) -> Result<(), Stop> {
        self.open_block(kind)?;
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

    // ---- ChatGPT: Responses API events → Anthropic. Nothing is written until the first output event, so a failure
    // before it is still a retry (account rotation, the tier's fallbacks).

    /// A function call opens a `tool_use` block; its arguments stream into it.
    fn call_start(&mut self, id: &str, name: &str) -> Result<(), Stop> {
        self.close()?;
        self.begin()?;
        let index = self.blocks.len();
        let block = json!({ "type": "tool_use", "id": id, "name": name, "input": {} });
        self.send("content_block_start", json!({ "type": "content_block_start", "index": index, "content_block": block }))?;
        self.blocks.push(block);
        (self.open, self.args) = (Some("tool"), String::new());
        Ok(())
    }

    fn call_args(&mut self, part: &str) -> Result<(), Stop> {
        if self.open != Some("tool") || part.is_empty() {
            return Ok(());
        }
        self.args.push_str(part);
        let index = self.blocks.len() - 1;
        self.send("content_block_delta", json!({ "type": "content_block_delta", "index": index, "delta": { "type": "input_json_delta", "partial_json": part } }))
    }

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
                let id = i["call_id"].as_str().or(i["id"].as_str()).filter(|x| !x.is_empty()).map_or_else(|| format!("toolu_{}", antigravity::random_hex(12)), String::from);
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
                        other => return Err(self.fail(Failure { status: 400, message: format!("ChatGPT stopped generating ({other})"), retry_after: None }, None)),
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
                Err(e) => return Err(self.fail(Failure { status: 502, message: format!("ChatGPT stream broke: {e}"), retry_after: None }, Some(0))),
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
            return Err(self.fail(Failure { status: 502, message: "ChatGPT stream ended before the response finished".into(), retry_after: None }, Some(0)));
        }
        if !self.started && self.queries.is_empty() && self.links.is_empty() {
            return Err(self.fail(Failure { status: 529, message: "ChatGPT returned an empty response".into(), retry_after: None }, Some(0)));
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
                        self.sig = tag(self.fam, s);
                    }
                }
                Some(t) => self.text(t)?,
                // A bare signature part belongs to the thinking block it follows.
                None if self.open == Some("thinking") => {
                    if let Some(s) = sig {
                        self.sig = tag(self.fam, s);
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

    pub static FAKE_API: Mutex<Option<String>> = Mutex::new(None);

    fn serve_pool(pool: Pool, routes: Vec<Route>, folder: PathBuf) -> io::Result<Proxy> {
        serve_with(pool, chatgpt::Pool::new(Vec::new(), Vec::new()), routes, folder).inspect(|p| p.fail_on_limit.store(true, Ordering::Relaxed))
    }

    /// One Antigravity tier that Claude Code knows as `name`.
    fn ag(name: &str, model: &str) -> Vec<Route> {
        vec![route("opus", &[name], Upstream::Antigravity, model)]
    }

    fn ctx() -> Ctx {
        Ctx {
            pool: Pool::fake("http://127.0.0.1:1", &["a"]),
            gpt: chatgpt::Pool::new(Vec::new(), Vec::new()),
            token: "t".into(),
            agent_id: "agent".into(),
            trajectory: "traj".into(),
            step: AtomicU64::new(0),
            sigs: Mutex::new(HashMap::new()),
            routes: Default::default(),
            http: ureq::Agent::new_with_defaults(),
            folder: PathBuf::new(),
            ratio: Mutex::new(HashMap::new()),
            fail_on_limit: Default::default(),
            live: Default::default(),
            wake: AtomicU64::new(0),
            plan_backs: Default::default(),
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
        let proxy = serve_pool(Pool::fake(&url, &["quota", "inband", "good"]), Vec::new(), PathBuf::new()).unwrap();
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
    fn turned_off_accounts_are_skipped_live() {
        let (url, seen) = fake_upstream(reply());
        let proxy = serve_pool(Pool::fake(&url, &["first", "second"]), Vec::new(), PathBuf::new()).unwrap();
        let req = json!({ "model": "gemini-3.1-pro-low", "max_tokens": 1000, "stream": true, "messages": [{ "role": "user", "content": "hi" }] });
        assert_eq!(post(&proxy, &proxy.token, &req).0, 200);
        assert_eq!(seen.lock().unwrap().last().unwrap(), "first p-first");

        // Switched off while running: the next request goes to the other account, the one after to none.
        proxy.disabled.write().unwrap().insert("first".into());
        assert_eq!(post(&proxy, &proxy.token, &req).0, 200);
        assert_eq!(seen.lock().unwrap().last().unwrap(), "second p-second");
        proxy.disabled.write().unwrap().insert("second".into());
        let n = seen.lock().unwrap().len();
        let (status, body) = post(&proxy, &proxy.token, &req);
        assert!(status >= 400 && body.contains("turned off"), "{status} {body}");
        assert_eq!(seen.lock().unwrap().len(), n, "nothing reached Google");

        // And back on.
        proxy.disabled.write().unwrap().clear();
        assert_eq!(post(&proxy, &proxy.token, &req).0, 200);
    }

    #[test]
    fn empty_answers_are_retried_then_reported() {
        let (url, seen) = fake_upstream(reply());
        let proxy = serve_pool(Pool::fake(&url, &["empty"]), Vec::new(), PathBuf::new()).unwrap();
        let req = json!({ "model": "gemini-3.1-pro-low", "max_tokens": 1000, "stream": true, "messages": [{ "role": "user", "content": "hi" }] });
        let (status, body) = post(&proxy, &proxy.token, &req);
        assert_eq!(status, 529, "nothing was streamed, so Claude Code gets a retryable status: {body}");
        assert!(body.contains("overloaded_error") && body.contains("empty response"), "{body}");
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[test]
    fn translates_stream_and_non_stream() {
        let (url, seen) = fake_upstream(reply());
        let aliases = ag("claude-opus-5-5", "gemini-3.1-pro-low");
        let proxy = serve_pool(Pool::fake(&url, &["good", "quota"]), aliases, PathBuf::new()).unwrap();
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

    fn route(tier: &'static str, names: &[&str], upstream: Upstream, model: &str) -> Route {
        Route { tier, names: names.iter().map(|n| n.to_string()).collect(), upstream, model: model.into(), fallbacks: Vec::new() }
    }

    #[test]
    fn a_held_request_runs_as_soon_as_its_route_is_back() {
        let ep = |name: &str, url: String| Upstream::Endpoint { name: name.into(), url, key: "k".into() };
        let proxy = serve_pool(Pool::fake("http://127.0.0.1:1", &["a"]), vec![route("sonnet", &["m@sonnet"], ep("Busy", fake_endpoint(429, "busy")), "m")], PathBuf::new()).unwrap();
        proxy.fail_on_limit.store(false, Ordering::Relaxed);
        let body = json!({ "model": "m@sonnet", "max_tokens": 10, "messages": [{ "role": "user", "content": "hi" }] });
        let (p2, b2) = (proxy.clone(), body.clone());
        let t = std::time::Instant::now();
        let waiting = std::thread::spawn(move || post(&p2, &p2.token, &b2));
        std::thread::sleep(Duration::from_millis(800));
        assert!(!waiting.is_finished(), "held, not answered with the 429");
        // The route comes back (here: switched in Settings) and the pools say so: it runs at once, not at the next try.
        proxy.routes.write().unwrap()[0].upstream = ep("Good", fake_endpoint(200, "good"));
        proxy.ctx.wake.fetch_add(1, Ordering::Relaxed);
        let (status, text) = waiting.join().unwrap();
        assert_eq!(status, 200, "{text}");
        assert!(t.elapsed() < Duration::from_secs(10), "well before the 15 s floor");

        // Holding doesn't touch any other error: Claude Code still gets the upstream's own answer.
        for (code, who) in [(400, "bad"), (503, "down")] {
            proxy.routes.write().unwrap()[0].upstream = ep(who, fake_endpoint(code, who));
            let (status, text) = post(&proxy, &proxy.token, &body);
            assert_eq!(status, code);
            assert!(text.contains(&format!("{who} says {code}")), "{text}");
        }
    }

    #[test]
    fn a_held_request_stops_waiting_when_claude_code_hangs_up() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (server, _) = l.accept().unwrap();
        let wake = AtomicU64::new(0);
        assert!(still_waiting(&server, 0, &wake), "open: keep holding");
        drop(client);
        let t = std::time::Instant::now();
        assert!(!still_waiting(&server, 60, &wake), "closed: give up");
        assert!(t.elapsed() < Duration::from_secs(5), "at once, not after the wait");
    }

    #[test]
    fn tiers_resolve_by_name_then_tier_then_family_then_login() {
        let ctx = ctx();
        *ctx.routes.write().unwrap() = vec![
            route("fable", &["claude-fable-5-1"], Upstream::Official, "claude-fable-5-1"),
            route("opus", &[], Upstream::Official, ""),
            route("sonnet", &["gemini-x@sonnet", "claude-sonnet-5-5"], Upstream::Antigravity, "gemini-x"),
            route("haiku", &["m@haiku"], Upstream::Endpoint { name: "OR".into(), url: "u".into(), key: "k".into() }, "vendor/m"),
        ];
        let got = |asked: &str| {
            let (u, m, _, _) = ctx.resolve(asked);
            (u, m)
        };
        assert_eq!(got("gemini-x@sonnet"), (Upstream::Antigravity, "gemini-x".into()), "a spoofed tier's override target");
        assert_eq!(got("claude-sonnet-5-5"), (Upstream::Antigravity, "gemini-x".into()), "its Claude name, if Claude Code sends that");
        assert_eq!(got("claude-opus-4-6"), (Upstream::Official, "claude-opus-4-6".into()), "an official tier left at Claude Code's pick keeps the name");
        assert_eq!(got("claude-haiku-4-5-20251001").1, "vendor/m", "a Claude Code pick goes to its family's tier");
        assert_eq!(got("claude-opus-x@haiku").1, "vendor/m", "an explicit @tier beats the family in the name");
        assert_eq!(got("something-else"), (Upstream::Official, "something-else".into()), "unknown names go to the login");
        ctx.routes.write().unwrap().retain(|r| r.upstream != Upstream::Official);
        assert_eq!(got("something-else"), (Upstream::Antigravity, "gemini-x".into()), "…or, with no official tier, to the first one");

        // A Default fallback ("" = Claude Code's pick) goes out with the name Claude Code sent.
        ctx.routes.write().unwrap()[0].fallbacks = vec![(Upstream::Official, String::new())];
        assert_eq!(ctx.resolve("claude-sonnet-5-5").3, [(Upstream::Official, "claude-sonnet-5-5".to_string())]);

        // A switch made while the session runs: same names, new upstream.
        ctx.routes.write().unwrap()[0].upstream = Upstream::Official;
        ctx.routes.write().unwrap()[0].model = "claude-sonnet-5-5".into();
        assert_eq!(got("gemini-x@sonnet"), (Upstream::Official, "claude-sonnet-5-5".into()));
    }

    /// An endpoint that answers `status` with a body naming itself, for fallback tests.
    fn fake_endpoint(status: u16, who: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let _ = read_request(&s);
                let text = json!({ "type": "message", "who": who, "error": { "message": format!("{who} says {status}") }, "usage": { "input_tokens": 1, "output_tokens": 1 } }).to_string();
                let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{text}", text.len());
            }
        });
        url
    }

    #[test]
    fn a_failing_tier_falls_back_once() {
        let ep = |name: &str, url: String| Upstream::Endpoint { name: name.into(), url, key: "k".into() };
        let (busy, good) = (fake_endpoint(429, "busy"), fake_endpoint(200, "good"));
        let mut r = route("sonnet", &["m@sonnet"], ep("Busy", busy.clone()), "m");
        r.fallbacks = vec![(ep("Down", fake_endpoint(503, "down")), "second".into()), (ep("Good", good), "other".into())];
        let proxy = serve_pool(Pool::fake("http://127.0.0.1:1", &["a"]), vec![r], PathBuf::new()).unwrap();
        let body = json!({ "model": "m@sonnet", "max_tokens": 10, "messages": [{ "role": "user", "content": "hi" }] });
        let (status, text) = post(&proxy, &proxy.token, &body);
        assert_eq!(status, 200, "{text}");
        assert!(text.contains("\"who\":\"good\""), "the second fallback answered after the first was down too: {text}");

        // No fallback: the upstream's own 429 reaches Claude Code untouched.
        proxy.routes.write().unwrap()[0].fallbacks.clear();
        let (status, text) = post(&proxy, &proxy.token, &body);
        assert_eq!(status, 429);
        assert!(text.contains("busy says 429"), "{text}");

        // A bad request isn't the upstream's fault: no fallback for a 400.
        proxy.routes.write().unwrap()[0] = Route { fallbacks: vec![(ep("Good", fake_endpoint(200, "good")), "x".into())], ..route("sonnet", &["m@sonnet"], ep("Bad", fake_endpoint(400, "bad")), "m") };
        assert_eq!(post(&proxy, &proxy.token, &body).0, 400);

        // A Claude tier (no token needed) whose fallback spends a key: without the token, no fallback.
        proxy.routes.write().unwrap()[0] = Route { fallbacks: vec![(ep("Good", fake_endpoint(200, "good")), "x".into())], ..route("sonnet", &["m@sonnet"], Upstream::Official, "m") };
        *FAKE_API.lock().unwrap() = Some(busy);
        let (status, text) = post(&proxy, "not-the-token", &body);
        assert_ne!(status, 200, "{text}");
        assert!(!text.contains("good"), "{text}");
        assert_eq!(format!("{:?}", ep("OR", "u".into())), "OR", "logs name the provider, never the key");
    }

    #[test]
    fn endpoint_tiers_get_their_key_model_and_clean_history() {
        // Echoes what reached the endpoint, in a streamed answer with a rate-limit header.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let req = read_request(&s).unwrap();
                let h = |k: &str| req.header(k).unwrap_or("-").to_string();
                let seen = json!({ "path": req.path, "key": h("x-api-key"), "auth": h("authorization"), "beta": h("anthropic-beta"),
                                   "token": h("x-ccat-token"), "body": serde_json::from_slice::<Value>(&req.body).unwrap() });
                let text = format!("event: seen\ndata: {seen}\n\n");
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nanthropic-ratelimit-unified-5h-utilization: 0.25\r\nContent-Length: {}\r\n\r\n{text}", text.len());
            }
        });
        let routes = vec![route("sonnet", &["m@sonnet"], Upstream::Endpoint { name: "OR".into(), url, key: "ek".into() }, "vendor/m")];
        let proxy = serve_pool(Pool::fake("http://127.0.0.1:1", &["a"]), routes, PathBuf::new()).unwrap();
        let body = json!({ "model": "m@sonnet[1m]", "max_tokens": 10, "stream": true, "messages": [
            { "role": "user", "content": "hi" },
            { "role": "assistant", "content": [
                { "type": "thinking", "thinking": "from gemini", "signature": "ag:gemini:U0lH" },
                { "type": "thinking", "thinking": "unsigned", "signature": "" },
                { "type": "thinking", "thinking": "from anthropic", "signature": "EqQBCkg" },
                { "type": "text", "text": "hello" } ] },
            { "role": "user", "content": "again" } ] });
        let call = |token: &str| {
            let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
            agent.post(format!("{}/v1/messages?beta=true", proxy.url))
                .header("authorization", "Bearer sk-ant-oat01-login")
                .header("anthropic-beta", "oauth-2025-04-20,interleaved-thinking-2025-05-14")
                .header("x-ccat-token", token)
                .send(body.to_string())
                .unwrap()
        };
        assert_eq!(call("nope").status().as_u16(), 401, "an endpoint tier spends ccat's key, so it needs the token");

        let mut res = call(&proxy.token);
        assert_eq!(res.status().as_u16(), 200);
        assert_eq!(res.headers()["anthropic-ratelimit-unified-5h-utilization"], "0.25", "upstream headers come back");
        let text = res.body_mut().read_to_string().unwrap();
        let seen: Value = serde_json::from_str(text.lines().find_map(|l| l.strip_prefix("data: ")).unwrap()).unwrap();
        assert_eq!(seen["path"], "/v1/messages?beta=true", "the /v1 the URL ended with isn't doubled");
        assert_eq!((&seen["key"], &seen["auth"]), (&json!("ek"), &json!("Bearer ek")), "the endpoint's key, never the Claude login");
        assert_eq!(seen["beta"], "interleaved-thinking-2025-05-14", "the login's beta flag stays home");
        assert_eq!(seen["token"], "-", "the proxy token stays home");
        assert_eq!(seen["body"]["model"], "vendor/m");
        let kept: Vec<&str> = seen["body"]["messages"][1]["content"].as_array().unwrap().iter().map(|b| b["thinking"].as_str().or(b["text"].as_str()).unwrap()).collect();
        assert_eq!(kept, ["from anthropic", "hello"], "thinking another provider signed is dropped");
    }

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

    #[test]
    fn all_blocked_is_a_429_with_retry_after() {
        let (url, _) = fake_upstream(reply());
        let proxy = serve_pool(Pool::fake(&url, &["quota"]), Vec::new(), PathBuf::new()).unwrap();
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
        let aliases = ag("claude-opus-5-5", "claude-opus-4-6-thinking");
        let proxy = serve_pool(Pool::fake(&url, &["search"]), aliases, PathBuf::new()).unwrap();
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

        let proxy = serve_pool(Pool::fake(&url, &["retired"]), Vec::new(), PathBuf::new()).unwrap();
        let (status, body) = post(&proxy, &proxy.token, &req(false));
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("invalid_request_error") && body.contains("no longer available"), "Google's words reach the user: {body}");
        assert_eq!(seen.lock().unwrap().len(), 1, "a client error is not retried");
        // Streaming: the notice already went out as text, so the error rides in-band — as a non-retryable kind.
        let (_, body) = post(&proxy, &proxy.token, &req(true));
        assert!(body.contains("event: error") && body.contains("invalid_request_error"), "{body}");

        // Nothing said and no finish reason: still a retryable hiccup.
        let proxy = serve_pool(Pool::fake(&url, &["silent"]), Vec::new(), PathBuf::new()).unwrap();
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
        let proxy = serve_pool(Pool::fake(&url, &["good"]), Vec::new(), PathBuf::new()).unwrap();
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
        let proxy = serve_pool(Pool::fake(&url, &["image"]), Vec::new(), dir.clone()).unwrap();
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

    // ---- ChatGPT: end to end against a fake Responses API

    fn rsse(events: &[Value]) -> String {
        events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or("x"))).collect()
    }

    /// Answers every POST with `reply(bearer, body, how many came before)`; the requests it saw are the second value.
    fn fake_responses(reply: impl Fn(&str, &Value, usize) -> (u16, String) + Send + 'static) -> (String, Arc<Mutex<Vec<(String, Value)>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let req = read_request(&s).unwrap();
                assert!(req.path.ends_with("/responses"));
                let who = req.header("authorization").unwrap_or("").trim_start_matches("Bearer ").to_string();
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                let n = {
                    let mut l = log.lock().unwrap();
                    l.push((who.clone(), body.clone()));
                    l.len() - 1
                };
                let (status, text) = reply(&who, &body, n);
                let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
            }
        });
        (url, seen)
    }

    fn completed(input: u64, cached: u64, output: u64) -> Value {
        json!({ "type": "response.completed", "response": { "usage": { "input_tokens": input, "input_tokens_details": { "cached_tokens": cached }, "output_tokens": output } } })
    }

    /// A streamed text answer.
    fn gpt_text(parts: &[&str]) -> String {
        let mut ev = vec![json!({ "type": "response.created" }), json!({ "type": "response.output_item.added", "item": { "type": "message", "role": "assistant" } })];
        ev.extend(parts.iter().map(|p| json!({ "type": "response.output_text.delta", "delta": p })));
        ev.push(json!({ "type": "response.output_item.done", "item": { "type": "message" } }));
        ev.push(completed(100, 40, 7));
        rsse(&ev)
    }

    fn failed(code: &str) -> Value {
        json!({ "type": "response.failed", "response": { "error": { "code": code, "message": format!("{code}!") } } })
    }

    fn gpt_proxy(base: &str, accounts: &[&str], models: Vec<chatgpt::ChatgptModel>) -> Proxy {
        let routes = vec![route("opus", &["claude-opus-5-5"], Upstream::ChatGPT, "gpt-test")];
        let gpt = chatgpt::Pool::fake(base, accounts);
        *gpt.models.write().unwrap() = models;
        let p = serve_with(Pool::fake("http://127.0.0.1:1", &["a"]), gpt, routes, PathBuf::new()).unwrap();
        p.fail_on_limit.store(true, Ordering::Relaxed);
        p
    }

    fn gpt_req(stream: bool) -> Value {
        json!({ "model": "claude-opus-5-5", "max_tokens": 1000, "stream": stream, "messages": [{ "role": "user", "content": "hi" }] })
    }

    fn post_h(proxy: &Proxy, headers: &[(&str, &str)], body: &Value) -> (u16, String) {
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
        let mut r = agent.post(format!("{}/v1/messages?beta=true", proxy.url)).header("authorization", format!("Bearer {}", proxy.token));
        for (k, v) in headers {
            r = r.header(*k, *v);
        }
        let mut res = r.send(body.to_string()).unwrap();
        (res.status().as_u16(), res.body_mut().read_to_string().unwrap())
    }

    fn sse_events(body: &str) -> Vec<Value> {
        body.lines().filter_map(|l| l.strip_prefix("data: ")).map(|d| serde_json::from_str(d).unwrap()).collect()
    }

    fn types(events: &[Value]) -> Vec<&str> {
        events.iter().map(|e| e["type"].as_str().unwrap()).collect()
    }

    fn model(slug: &str, efforts: &[&str], default: &str) -> chatgpt::ChatgptModel {
        chatgpt::ChatgptModel { slug: slug.into(), name: slug.into(), efforts: efforts.iter().map(|e| e.to_string()).collect(), default_effort: default.into() }
    }

    #[test]
    fn chatgpt_text_answer_becomes_anthropic_sse() {
        let (url, seen) = fake_responses(|_, _, _| (200, gpt_text(&["Hel", "lo"])));
        let proxy = gpt_proxy(&url, &["good"], Vec::new());
        assert_eq!(post(&proxy, "nope", &gpt_req(true)).0, 401, "ChatGPT spends the plan: it needs the token");
        let (status, body) = post(&proxy, &proxy.token, &gpt_req(true));
        assert_eq!(status, 200, "{body}");
        let ev = sse_events(&body);
        assert_eq!(types(&ev), ["message_start", "content_block_start", "content_block_delta", "content_block_delta", "content_block_stop", "message_delta", "message_stop"]);
        assert_eq!(ev[0]["message"]["model"], "claude-opus-5-5");
        assert_eq!((ev[2]["delta"]["text"].as_str(), ev[3]["delta"]["text"].as_str()), (Some("Hel"), Some("lo")));
        assert_eq!(ev[5]["delta"]["stop_reason"], "end_turn");
        assert_eq!(ev[5]["usage"], json!({ "input_tokens": 60, "cache_read_input_tokens": 40, "cache_creation_input_tokens": 0, "output_tokens": 7 }));
        assert_eq!(seen.lock().unwrap()[0].0, "good", "the account's access token went upstream");

        let (status, body) = post(&proxy, &proxy.token, &gpt_req(false));
        assert_eq!(status, 200, "{body}");
        let msg: Value = serde_json::from_str(&body).unwrap();
        assert_eq!((msg["content"][0]["text"].as_str(), msg["stop_reason"].as_str()), (Some("Hello"), Some("end_turn")));
        assert_eq!(msg["usage"]["cache_read_input_tokens"], 40);
        let before = seen.lock().unwrap().len();
        assert!(count_tokens_of(&proxy) > 0);
        assert_eq!(seen.lock().unwrap().len(), before, "the count is a local estimate");
    }

    /// `count_tokens` for a ChatGPT tier: the local estimate, nothing sent upstream.
    fn count_tokens_of(proxy: &Proxy) -> u64 {
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
        let mut res = agent.post(format!("{}/v1/messages/count_tokens", proxy.url)).header("x-api-key", &proxy.token).send(gpt_req(false).to_string()).unwrap();
        serde_json::from_str::<Value>(&res.body_mut().read_to_string().unwrap()).unwrap()["input_tokens"].as_u64().unwrap()
    }

    #[test]
    fn chatgpt_function_calls_round_trip() {
        let (url, seen) = fake_responses(|_, body, _| {
            if body["input"].as_array().unwrap().iter().any(|i| i["type"] == "function_call_output") {
                return (200, gpt_text(&["done"]));
            }
            let call = json!({ "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "Read", "arguments": "" });
            let done = json!({ "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "Read", "arguments": "{\"p\":\"a\"}" });
            let ev = [
                json!({ "type": "response.output_item.added", "item": call }),
                json!({ "type": "response.function_call_arguments.delta", "delta": "{\"p\":" }),
                json!({ "type": "response.function_call_arguments.delta", "delta": "\"a\"}" }),
                json!({ "type": "response.output_item.done", "item": done }),
                completed(10, 0, 3),
            ];
            (200, rsse(&ev))
        });
        let proxy = gpt_proxy(&url, &["good"], Vec::new());
        let mut req = gpt_req(true);
        req["tools"] = json!([{ "name": "Read", "description": "d", "input_schema": { "type": "object", "properties": { "p": { "type": "string" } } } }]);
        let (status, body) = post(&proxy, &proxy.token, &req);
        assert_eq!(status, 200, "{body}");
        let ev = sse_events(&body);
        assert_eq!(types(&ev), ["message_start", "content_block_start", "content_block_delta", "content_block_delta", "content_block_stop", "message_delta", "message_stop"]);
        assert_eq!(ev[1]["content_block"], json!({ "type": "tool_use", "id": "call_1", "name": "Read", "input": {} }));
        assert_eq!((ev[2]["delta"]["partial_json"].as_str(), ev[3]["delta"]["partial_json"].as_str()), (Some("{\"p\":"), Some("\"a\"}")));
        assert_eq!(ev[5]["delta"]["stop_reason"], "tool_use");

        // The next turn replays the call and its result under the same call id.
        req["messages"] = json!([
            { "role": "user", "content": "hi" },
            { "role": "assistant", "content": [{ "type": "text", "text": "reading" }, { "type": "tool_use", "id": "call_1", "name": "Read", "input": { "p": "a" } }] },
            { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "call_1", "content": [{ "type": "text", "text": "contents" }] }] }
        ]);
        let (status, body) = post(&proxy, &proxy.token, &req);
        assert_eq!(status, 200, "{body}");
        let input = &seen.lock().unwrap()[1].1["input"];
        assert_eq!(input[0], json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "hi" }] }));
        assert_eq!(input[1], json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "reading" }] }));
        assert_eq!(input[2], json!({ "type": "function_call", "call_id": "call_1", "name": "Read", "arguments": "{\"p\":\"a\"}" }));
        assert_eq!(input[3], json!({ "type": "function_call_output", "call_id": "call_1", "output": "contents" }));
    }

    #[test]
    fn chatgpt_reasoning_is_signed_and_replayed() {
        let (url, seen) = fake_responses(|_, _, _| {
            let ev = [
                json!({ "type": "response.output_item.added", "item": { "type": "reasoning", "id": "rs_1" } }),
                json!({ "type": "response.reasoning_summary_part.added", "summary_index": 0 }),
                json!({ "type": "response.reasoning_summary_text.delta", "delta": "think" }),
                json!({ "type": "response.output_item.done", "item": { "type": "reasoning", "id": "rs_1", "encrypted_content": "ENC", "summary": [] } }),
                json!({ "type": "response.output_item.added", "item": { "type": "message" } }),
                json!({ "type": "response.output_text.delta", "delta": "Hello" }),
                json!({ "type": "response.output_item.done", "item": { "type": "message" } }),
                completed(10, 0, 3),
            ];
            (200, rsse(&ev))
        });
        let proxy = gpt_proxy(&url, &["good"], Vec::new());
        let (_, body) = post(&proxy, &proxy.token, &gpt_req(true));
        let ev = sse_events(&body);
        assert_eq!(
            types(&ev),
            ["message_start", "content_block_start", "content_block_delta", "content_block_delta", "content_block_stop", "content_block_start", "content_block_delta", "content_block_stop", "message_delta", "message_stop"]
        );
        assert_eq!(ev[1]["content_block"]["type"], "thinking");
        assert_eq!(ev[2]["delta"], json!({ "type": "thinking_delta", "thinking": "think" }));
        let sig = ev[3]["delta"]["signature"].as_str().unwrap().to_string();
        let held: Value = serde_json::from_slice(&B64.decode(sig.strip_prefix("oa:").unwrap()).unwrap()).unwrap();
        assert_eq!(held, json!({ "id": "rs_1", "encrypted_content": "ENC" }));

        let mut req = gpt_req(true);
        req["messages"] = json!([
            { "role": "user", "content": "hi" },
            { "role": "assistant", "content": [{ "type": "thinking", "thinking": "think", "signature": sig }, { "type": "thinking", "thinking": "g", "signature": "ag:gemini:U0lH" }, { "type": "text", "text": "Hello" }] },
            { "role": "user", "content": "more" }
        ]);
        post(&proxy, &proxy.token, &req);
        let input = seen.lock().unwrap()[1].1["input"].clone();
        let kinds: Vec<&str> = input.as_array().unwrap().iter().map(|i| i["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["message", "reasoning", "message", "message"], "another provider's thinking is dropped");
        assert_eq!(input[1], json!({ "type": "reasoning", "id": "rs_1", "encrypted_content": "ENC", "summary": [] }));
    }

    #[test]
    fn chatgpt_usage_limit_rotates_to_the_next_account() {
        let (url, seen) = fake_responses(|who, _, _| match who {
            "limit" => (200, rsse(&[json!({ "type": "response.created" }), failed("subscription_sharing_usage_limit_exceeded")])),
            _ => (200, gpt_text(&["fine"])),
        });
        let proxy = gpt_proxy(&url, &["limit", "good"], Vec::new());
        let (status, body) = post(&proxy, &proxy.token, &gpt_req(true));
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("fine") && body.contains("message_stop") && !body.contains("event: error"), "{body}");
        let who = |s: &Arc<Mutex<Vec<(String, Value)>>>| s.lock().unwrap().iter().map(|(w, _)| w.clone()).collect::<Vec<_>>();
        assert_eq!(who(&seen), ["limit", "good"]);
        // The block is remembered: the next request goes straight to the second account.
        post(&proxy, &proxy.token, &gpt_req(true));
        assert_eq!(who(&seen), ["limit", "good", "good"]);

        // Everything over its usage: a 429 with a Retry-After (the tier's fallback would take over).
        let proxy = gpt_proxy(&url, &["limit"], Vec::new());
        let (status, body) = post(&proxy, &proxy.token, &gpt_req(false));
        assert_eq!(status, 429, "{body}");
        assert!(body.contains("rate_limit_error") && body.contains("over their plan usage"), "{body}");
    }

    #[test]
    fn chatgpt_failure_after_output_is_an_error_event() {
        let (url, seen) = fake_responses(|_, _, _| {
            let ev = [
                json!({ "type": "response.output_item.added", "item": { "type": "message" } }),
                json!({ "type": "response.output_text.delta", "delta": "Hi" }),
                failed("subscription_sharing_usage_limit_exceeded"),
            ];
            (200, rsse(&ev))
        });
        let proxy = gpt_proxy(&url, &["a", "b"], Vec::new());
        let (status, body) = post(&proxy, &proxy.token, &gpt_req(true));
        assert_eq!(status, 200);
        assert!(body.contains("Hi") && body.contains("event: error") && body.contains("rate_limit_error"), "{body}");
        assert_eq!(seen.lock().unwrap().len(), 1, "text had started: no second account");
    }

    #[test]
    fn oa_thinking_never_reaches_anthropic_or_endpoints() {
        let body = json!({ "model": "x", "messages": [{ "role": "assistant", "content": [
            { "type": "thinking", "thinking": "a", "signature": "oa:eyJ9" }, { "type": "thinking", "thinking": "b", "signature": "ag:claude:QUJD" },
            { "type": "thinking", "thinking": "c", "signature": "EqQBCkg" }, { "type": "text", "text": "t" }] }] });
        let sent: Value = serde_json::from_slice(&upstream_body(body, "claude-x")).unwrap();
        let kept: Vec<&str> = sent["messages"][0]["content"].as_array().unwrap().iter().map(|b| b["thinking"].as_str().or(b["text"].as_str()).unwrap()).collect();
        assert_eq!(kept, ["c", "t"]);
        assert_eq!(untag(Some("oa:eyJ9"), "gpt"), None, "an Antigravity family never takes a ChatGPT signature");
        assert_eq!(untag(Some("oa:QUJD"), "claude"), None);
    }

    #[test]
    fn chatgpt_request_is_built_from_scratch() {
        let (url, seen) = fake_responses(|_, _, _| (200, gpt_text(&["ok"])));
        let proxy = gpt_proxy(&url, &["good"], vec![model("gpt-test", &["low", "medium", "high"], "medium")]);
        let req = json!({
            "model": "claude-opus-5-5", "max_tokens": 1000, "stream": false, "temperature": 0.2, "top_p": 0.9, "top_k": 5, "stop_sequences": ["x"],
            "metadata": { "user_id": "u" }, "user": "u", "truncation": "auto", "service_tier": "auto", "background": true,
            "thinking": { "type": "enabled", "budget_tokens": 2000 },
            "system": [{ "type": "text", "text": "x-anthropic-billing-header: cc_version=2;" }, { "type": "text", "text": "Be brief." }],
            "messages": [{ "role": "user", "content": [
                { "type": "text", "text": "look" },
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "QUJD" } }] },
                { "role": "assistant", "content": [{ "type": "tool_use", "id": "t1", "name": "Shot", "input": {} }] },
                { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": [{ "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "REVG" } }] }] }],
            "tools": [{ "name": "Shot", "description": "d", "input_schema": { "$schema": "x", "type": "object", "properties": {} } }, { "type": "web_search_20250305", "name": "web_search" }, { "type": "bash_20250124", "name": "bash" }],
            "tool_choice": { "type": "any" }
        });
        let (status, body) = post_h(&proxy, &[("x-claude-code-session-id", "sess-9")], &req);
        assert_eq!(status, 200, "{body}");
        let sent = seen.lock().unwrap()[0].1.clone();
        let keys: Vec<&str> = sent.as_object().unwrap().keys().map(String::as_str).collect();
        for banned in ["background", "conversation", "max_output_tokens", "max_tool_calls", "metadata", "moderation", "multi_agent", "prompt", "prompt_cache_retention", "safety_identifier", "temperature", "top_logprobs", "top_p", "truncation", "user", "previous_response_id", "max_tokens", "top_k", "stop_sequences", "thinking", "system", "messages", "service_tier"] {
            assert!(!keys.contains(&banned), "{banned} leaked: {keys:?}");
        }
        assert_eq!((&sent["store"], &sent["stream"], &sent["model"]), (&json!(false), &json!(true), &json!("gpt-test")), "never stored, always streamed, the tier's model");
        assert_eq!(sent["instructions"], "Be brief.", "billing header filtered");
        assert_eq!(sent["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(sent["reasoning"], json!({ "effort": "low", "summary": "auto" }), "a 2000-token budget");
        assert_eq!(sent["tool_choice"], "required");
        assert_eq!(sent["tools"], json!([{ "type": "function", "name": "Shot", "description": "d", "parameters": { "type": "object", "properties": {} }, "strict": false }, { "type": "web_search" }]));
        assert_eq!(sent["input"][0]["content"][1], json!({ "type": "input_image", "image_url": "data:image/png;base64,QUJD" }));
        assert_eq!(sent["input"][2], json!({ "type": "function_call_output", "call_id": "t1", "output": "(see attached image)" }));
        assert_eq!(sent["input"][3], json!({ "type": "message", "role": "user", "content": [{ "type": "input_image", "image_url": "data:image/png;base64,REVG" }] }), "tool-result images ride in a user message");
        assert_eq!(sent["prompt_cache_key"], "sess-9");
        assert!(sent["input"].as_array().unwrap().iter().all(|i| i["role"] != "system"));
    }

    #[test]
    fn chatgpt_tools_are_namespaced_after_one_refusal() {
        let (url, seen) = fake_responses(|_, body, _| {
            if body["tools"][0]["type"] == "function" {
                let e = json!({ "error": { "code": "subscription_sharing_unsupported_capability", "message": "tools not supported", "param": "tools" } });
                return (400, e.to_string());
            }
            let item = json!({ "type": "function_call", "id": "fc", "call_id": "c1", "name": "claude_code.Read", "arguments": "{}" });
            (200, rsse(&[json!({ "type": "response.output_item.added", "item": item }), json!({ "type": "response.output_item.done", "item": item }), completed(1, 0, 1)]))
        });
        let proxy = gpt_proxy(&url, &["good"], Vec::new());
        let mut req = gpt_req(false);
        req["tools"] = json!([{ "name": "Read", "description": "d", "input_schema": { "type": "object" } }]);
        let (status, body) = post(&proxy, &proxy.token, &req);
        assert_eq!(status, 200, "{body}");
        let msg: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(msg["content"][0]["name"], "Read", "the namespace prefix is stripped");
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 2, "one retry");
            assert_eq!(seen[1].1["tools"], json!([{ "type": "namespace", "name": "claude_code", "description": "Claude Code tools", "tools": seen[0].1["tools"] }]));
        }
        // Remembered: the next request is wrapped at once.
        post(&proxy, &proxy.token, &req);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[2].1["tools"][0]["type"], "namespace");

        // What the official route refused is not the Codex route's business.
        let pool = chatgpt::Pool::new(Vec::new(), Vec::new());
        pool.wrap.store(true, Ordering::Relaxed);
        pool.no_cache_key.store(true, Ordering::Relaxed);
        let hints = Hints { session: "s".into() };
        let official = responses_body(&req, "m", None, &hints, &pool, false);
        let codex = responses_body(&req, "m", None, &hints, &pool, true);
        assert_eq!((official["tools"][0]["type"].as_str(), official.get("prompt_cache_key").is_none()), (Some("namespace"), true));
        assert_eq!((codex["tools"][0]["type"].as_str(), codex["prompt_cache_key"].as_str()), (Some("function"), Some("s")));
    }

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

    #[test]
    fn prompt_cache_key_is_sent_and_dropped_when_refused_and_service_tier_never() {
        let (url, seen) = fake_responses(|_, body, _| {
            if body["prompt_cache_key"] == "refused" {
                let e = json!({ "error": { "code": "subscription_sharing_unsupported_capability", "message": "no", "param": "prompt_cache_key" } });
                return (400, e.to_string());
            }
            (200, gpt_text(&["ok"]))
        });
        let proxy = gpt_proxy(&url, &["good"], vec![model("gpt-test", &["low"], "low")]);
        let mut req = gpt_req(false);
        req["speed"] = json!("fast"); // Claude Code's fast mode: ignored on purpose
        post_h(&proxy, &[("x-claude-code-session-id", "s1"), ("anthropic-beta", "fast-mode-2026-02-01")], &req);
        post_h(&proxy, &[], &req);
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen[0].1["prompt_cache_key"], "s1", "the session id keys the cache");
            assert!(seen[1].1["prompt_cache_key"].as_str().unwrap().starts_with("ccat-"), "no session header: a hash of the system prompt");
            assert!(seen.iter().all(|(_, b)| b.get("service_tier").is_none()), "fast mode is off: no service_tier, ever");
        }
        // Refused once: dropped for the rest of the process, and the request still succeeds.
        let (status, _) = post_h(&proxy, &[("x-claude-code-session-id", "refused")], &req);
        assert_eq!(status, 200);
        let n = seen.lock().unwrap().len();
        post_h(&proxy, &[("x-claude-code-session-id", "refused")], &req);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), n + 1, "no second refusal");
        assert!(seen[n - 2].1.get("prompt_cache_key").is_some(), "the refused try had it");
        assert!(seen[n - 1].1.get("prompt_cache_key").is_none() && seen[n].1.get("prompt_cache_key").is_none());
    }

    /// The Codex route against a fake backend: its own path, headers, and body shaping.
    #[test]
    fn codex_route_shapes_url_headers_and_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<(String, Vec<(String, String)>, Value)>>> = Arc::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let req = read_request(&s).unwrap();
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                log.lock().unwrap().push((req.path.clone(), req.headers.clone(), body));
                let text = gpt_text(&["codex says hi"]);
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
            }
        });
        let proxy = gpt_proxy(&url, &["cx1"], Vec::new());
        let mut req = gpt_req(false);
        req["tools"] = json!([{ "name": "Read", "description": "d", "input_schema": { "type": "object" } }]);
        let (status, body) = post_h(&proxy, &[("x-claude-code-session-id", "sess-7")], &req);
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("codex says hi"));
        let seen = seen.lock().unwrap();
        let (path, headers, sent) = &seen[0];
        assert_eq!(path, "/codex/responses");
        let h = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(h("authorization"), Some("Bearer cx1"));
        assert_eq!(h("chatgpt-account-id"), Some("acct-cx1"), "the workspace id from the token");
        assert_eq!((h("originator"), h("openai-beta")), (Some("ccat"), Some("responses=experimental")));
        assert_eq!((h("session_id"), h("conversation_id"), h("x-client-request-id")), (Some("sess-7"), Some("sess-7"), Some("sess-7")));
        assert_eq!(h("x-codex-routing-hint"), Some("model=gpt-test"));
        assert!(h("version").is_some() && h("user-agent").is_some_and(|u| u.starts_with("ccat/")));
        assert_eq!((&sent["store"], &sent["stream"], &sent["prompt_cache_key"], &sent["parallel_tool_calls"]), (&json!(false), &json!(true), &json!("sess-7"), &json!(true)));
        assert!(sent.get("service_tier").is_none() && sent.get("max_output_tokens").is_none() && sent.get("temperature").is_none());
        assert_eq!(sent["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(sent["reasoning"]["summary"], "auto");
        assert_eq!(sent["tools"][0]["name"], "Read");
    }

    #[test]
    fn codex_usage_limits_use_the_reset_time_they_carry() {
        let reset = now_secs() + 7200;
        let (url, _) = fake_responses(move |who, _, _| match who {
            "cxlimit" => (429, json!({ "error": { "type": "usage_limit_reached", "message": "limit", "resets_at": reset } }).to_string()),
            _ => (200, gpt_text(&["fine"])),
        });
        let proxy = gpt_proxy(&url, &["cxlimit", "cxgood"], Vec::new());
        let (status, body) = post(&proxy, &proxy.token, &gpt_req(false));
        assert_eq!(status, 200, "{body}");
        let proxy = gpt_proxy(&url, &["cxlimit"], Vec::new());
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
        let res = agent.post(format!("{}/v1/messages", proxy.url)).header("authorization", format!("Bearer {}", proxy.token)).send(gpt_req(false).to_string()).unwrap();
        assert_eq!(res.status().as_u16(), 429);
        let wait: u64 = res.headers()["retry-after"].to_str().unwrap().parse().unwrap();
        assert!((6000..=7200).contains(&wait), "blocked until the reset OpenAI named, not a guess: {wait}");
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
    }

    /// Live check against Google with the first Antigravity account in the ccat config: the proxy's `count_tokens`
    /// vs the `promptTokenCount` Google bills for the same request, implicit caching on a repeat, and the calibrated
    /// Claude-route estimate. Spends a few requests of quota. `cargo test -- --ignored live_ --nocapture`.
    #[test]
    #[ignore = "uses your real Antigravity account from the ccat config"]
    fn live_counts_and_caching_match_google() {
        let (cfg, _) = crate::load();
        let p = cfg.providers.iter().find(|p| p.antigravity && !p.accounts.is_empty()).expect("an Antigravity provider with an account in the ccat config");
        let proxy = start(p.accounts.clone(), Vec::new(), Vec::new(), Vec::new(), std::env::temp_dir()).unwrap();
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
        let aliases = ag("claude-sonnet-4-6", "gemini-3.1-pro-low");
        let proxy = serve_pool(Pool::fake(&upstream, &["acct"]), aliases, dir.clone()).unwrap();
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
