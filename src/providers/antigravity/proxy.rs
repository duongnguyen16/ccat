//! Antigravity (Cloud Code Assist): Claude Code's Anthropic request translated to `streamGenerateContent`, and the
//! stream translated back. Account rotation, quota blocks and the wire calls are in the parent module's `Pool`.

use crate::common::{self, Failure};
use crate::providers::antigravity::{self, family};
use crate::proxy::http::{record, respond, respond_error, Meta};
use crate::proxy::sse::{blocks, client_tools, tag, tool_result_text, untag, Stop, Tx};
use crate::proxy::{system_parts, web_search, Ctx, Fail, SEARCH_MODEL};
use serde_json::{json, Map, Value};
use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
    io::BufRead,
    net::TcpStream,
    sync::atomic::Ordering,
};

/// Cloud Code rejects a Gemini 3 turn whose first function call is unsigned; this documented sentinel stands in.
pub(crate) const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";
/// `labels.model_enum` the real client tags Gemini requests with (telemetry only).
const MODEL_ENUM: [(&str, &str); 5] = [
    ("gemini-3.5-flash-extra-low", "MODEL_PLACEHOLDER_M187"),
    ("gemini-3.5-flash-low", "MODEL_PLACEHOLDER_M20"),
    ("gemini-3-flash-agent", "MODEL_PLACEHOLDER_M132"),
    ("gemini-3.1-pro-low", "MODEL_PLACEHOLDER_M36"),
    ("gemini-pro-agent", "MODEL_PLACEHOLDER_M16"),
];

// ---------------------------------------------------------------- Antigravity: routing (token counts, request loop)

/// Gemini routes: Google counts exactly what `build` would send. Cloud Code's counter only takes `contents`, so the
/// translated system prompt and function declarations lead them as text. Everything else (Claude/GPT routes, a
/// failed count) gets the estimate calibrated on this session's real responses.
pub(crate) fn count_tokens(s: &mut TcpStream, ctx: &Ctx, req: &Value, model: &str) {
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
            common::log(&format!("countTokens failed ({} {}); using the calibrated estimate", f.status, f.message));
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn messages(s: &mut TcpStream, ctx: &Ctx, req: &Value, asked: &str, model: String, tier: &str, may_fail: Fail) -> Result<(), Failure> {
    if model.is_empty() {
        respond_error(s, &Failure { status: 400, message: "no model in request".into(), retry_after: None });
        return Ok(());
    }
    let model = if web_search(req).is_some() && family(&model) != "gemini" { SEARCH_MODEL.to_string() } else { model };
    let started = std::time::Instant::now();
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
                record(tier, "Antigravity", &model, &msg["usage"], Meta { started, stop: msg["stop_reason"].as_str(), acct: &ctx.pool.alias(i) });
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
                common::log(&format!("{model}{kind}: {} after {} {}", if fatal { "giving up" } else { "retrying" }, f.status, f.message));
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

// ---------------------------------------------------------------- Anthropic → Cloud Code

fn media(b: &Value) -> Option<Value> {
    let src = &b["source"];
    (src["type"] == "base64").then(|| json!({ "inlineData": { "mimeType": src["media_type"], "data": src["data"] } }))
}

pub(crate) fn function_response(b: &Value, names: &HashMap<String, String>, fam: &str) -> Value {
    let id = b["tool_use_id"].as_str().unwrap_or("");
    let (mut text, mut images) = (Vec::new(), Vec::new());
    for c in blocks(&b["content"]) {
        match c["type"].as_str() {
            Some("image") => images.extend(media(&c)),
            _ => text.extend(tool_result_text(&c)),
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

/// Client tools as Cloud Code `functionDeclarations`. Server tools (web search…) have no input_schema and are skipped.
pub(crate) fn declarations(req: &Value) -> Vec<Value> {
    client_tools(req)
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t.get("input_schema").is_some())
        .map(|t| json!({ "name": t["name"], "description": t["description"].as_str().unwrap_or(""), "parameters": clean_schema(&t["input_schema"]) }))
        .collect()
}

/// The Cloud Code request (without `project`, which the pool fills per account) and whether the Claude thinking beta header applies.
pub(crate) fn build(req: &Value, model: &str, ctx: &Ctx) -> (Value, bool) {
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
pub(crate) fn clean_schema(schema: &Value) -> Value {
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

// ---------------------------------------------------------------- Cloud Code stream → Anthropic SSE

impl<'a> Tx<'a> {
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
                let id = given.map_or_else(|| format!("toolu_{}", common::random_hex(12)), String::from);
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

    pub(crate) fn run(&mut self, reader: Box<dyn BufRead + Send>) -> Result<(), Stop> {
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
            if line.trim_start().starts_with(':') || line.strip_prefix("event:").is_some_and(|event| event.trim() == "ping") {
                self.heartbeat()?;
                continue;
            }
            if let Some(d) = line.strip_prefix("data:") {
                data.push_str(d.trim_start());
            } else if line.trim().is_empty() && !data.is_empty() {
                let v: Value = serde_json::from_str(&std::mem::take(&mut data)).unwrap_or_default();
                if v["type"] == "ping" { self.heartbeat()?; } else { self.chunk(&v)?; }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::tests::ctx;

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
}
