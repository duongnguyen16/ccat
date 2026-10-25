//! A tiny MCP server (Streamable HTTP, plain JSON replies) on the proxy's port, for Antigravity abilities Claude Code
//! has no built-in tool for — today image generation/editing with Gemini's image model (Antigravity's `generate_image`).
//! ccat registers it with Claude Code as the `antigravity` server, so the tool shows up as `mcp__antigravity__generate_image`.

use crate::antigravity::Pool;
use crate::common;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Value};
use std::{
    fs,
    io::BufRead,
    path::{Path, PathBuf},
};

pub const IMAGE_MODEL: &str = "gemini-3.1-flash-image";
const RATIOS: [&str; 10] = ["1:1", "2:3", "3:2", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9"];

/// One JSON-RPC message → its reply, or `None` for a notification (answered with a bare 202).
pub fn handle(pool: &Pool, folder: &Path, msg: &Value) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let p = &msg["params"];
    let result = match msg["method"].as_str().unwrap_or("") {
        "initialize" => Ok(json!({
            "protocolVersion": p["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "antigravity", "version": env!("CARGO_PKG_VERSION") },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": [image_tool()] })),
        "tools/call" if p["name"] == "generate_image" => Ok(match generate_image(pool, folder, &p["arguments"]) {
            Ok(content) => json!({ "content": content }),
            Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
        }),
        "tools/call" => Err((-32602, format!("unknown tool {}", p["name"]))),
        m => Err((-32601, format!("method not found: {m}"))),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
    })
}

fn image_tool() -> Value {
    json!({
        "name": "generate_image",
        "description": "Generate a new image, or edit/combine existing images, with Google's Gemini image model (via Antigravity), \
and save it as a file. Write a detailed prompt (subject, style, composition, colours, any text to render). \
To edit, pass the source image files in `images` and describe the change.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "What to draw, or how to change the input images." },
                "path": { "type": "string", "description": "Where to save, relative to the project folder. Default: generated/image-<random>.png. The extension follows the format the model returns." },
                "images": { "type": "array", "items": { "type": "string" }, "description": "Existing image files to edit or use as references." },
                "aspect_ratio": { "type": "string", "enum": RATIOS, "description": "Output aspect ratio (default: the model's choice, usually 1:1)." },
            },
            "required": ["prompt"],
        },
    })
}

fn resolve(folder: &Path, p: &str) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        folder.join(p)
    }
}

fn mime_of(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "image/png",
    }
}

fn generate_image(pool: &Pool, folder: &Path, args: &Value) -> Result<Vec<Value>, String> {
    let prompt = args["prompt"].as_str().filter(|p| !p.trim().is_empty()).ok_or("`prompt` is required.")?;
    let mut parts = vec![json!({ "text": prompt })];
    for f in args["images"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        let path = resolve(folder, f);
        let bytes = fs::read(&path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        parts.push(json!({ "inlineData": { "mimeType": mime_of(&path), "data": B64.encode(bytes) } }));
    }
    let mut gen = json!({ "responseModalities": ["TEXT", "IMAGE"] });
    if let Some(r) = args["aspect_ratio"].as_str().filter(|r| RATIOS.contains(r)) {
        gen["imageConfig"] = json!({ "aspectRatio": r });
    }
    let body = json!({
        "model": IMAGE_MODEL,
        "userAgent": "antigravity",
        "requestType": "agent",
        "requestId": format!("agent/{}", common::uuid()),
        "request": { "contents": [{ "role": "user", "parts": parts }], "generationConfig": gen },
    });
    let (_, reader) = pool.stream(IMAGE_MODEL, &body, false).map_err(|f| f.message)?;
    let (text, image) = read_image(reader)?;
    let (mime, data) = image.ok_or_else(|| match text.is_empty() {
        true => "The model returned no image.".to_string(),
        false => format!("The model returned no image: {text}"),
    })?;
    let bytes = B64.decode(&data).map_err(|e| format!("The model returned a broken image: {e}"))?;
    let ext = match mime.as_str() {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        _ => "png",
    };
    let path = match args["path"].as_str().filter(|p| !p.trim().is_empty()) {
        Some(p) => resolve(folder, p).with_extension(ext),
        None => folder.join("generated").join(format!("image-{}.{ext}", common::random_hex(4))),
    };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    }
    fs::write(&path, &bytes).map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
    common::log(&format!("generate_image → {} ({} bytes)", path.display(), bytes.len()));
    let mut note = format!("Saved {} ({} KB, {mime}).", path.display(), bytes.len().div_ceil(1024));
    if !text.is_empty() {
        note.push_str(&format!("\nModel note: {text}"));
    }
    Ok(vec![json!({ "type": "text", "text": note }), json!({ "type": "image", "data": data, "mimeType": mime })])
}

/// The model's final (non-thought) image and any text, from a `streamGenerateContent` SSE stream.
/// Google sends each event's JSON on a single `data:` line.
fn read_image(reader: Box<dyn BufRead + Send>) -> Result<(String, Option<(String, String)>), String> {
    let (mut text, mut image) = (String::new(), None);
    for line in reader.lines() {
        let line = line.map_err(|e| format!("Antigravity stream broke: {e}"))?;
        let Some(d) = line.strip_prefix("data:") else { continue };
        let v: Value = serde_json::from_str(d.trim()).unwrap_or_default();
        if let Some(e) = v.get("error").filter(|e| e.is_object()) {
            return Err(format!("Antigravity: {}", e["message"].as_str().unwrap_or("stream error")));
        }
        let r = v.get("response").unwrap_or(&v);
        // Thinking image models stream draft images as thoughts; only the final answer counts.
        for part in r["candidates"][0]["content"]["parts"].as_array().into_iter().flatten().filter(|p| p["thought"] != true) {
            match part.get("inlineData") {
                Some(d) => image = Some((d["mimeType"].as_str().unwrap_or("image/png").to_string(), d["data"].as_str().unwrap_or("").to_string())),
                None => text.push_str(part["text"].as_str().unwrap_or("")),
            }
        }
    }
    Ok((text.trim().to_string(), image.filter(|(_, d)| !d.is_empty())))
}
