//! Anthropic Messages conversion for Gemini Web.
//! Translates Anthropic `/v1/messages` request structures to Gemini Web conversation prompts
//! and attachments, and maps Gemini Web wire replies back to Anthropic message responses.

use crate::common::{self, fail, Failure};
use super::wire::{InputFile, Reply};
use serde_json::{json, Value};

/// Returns a file extension hint corresponding to a MIME type.
fn ext_for_mime(mime: &str) -> &'static str {
    match mime.to_ascii_lowercase().as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/html" => "html",
        "text/css" => "css",
        "text/javascript" | "application/javascript" => "js",
        "application/json" => "json",
        _ => "bin",
    }
}

/// Sanitizes a filename hint or generates a safe random filename.
fn sanitize_name(hint: &str, ext: &str) -> String {
    let mut safe = String::new();
    for c in hint.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            safe.push(c);
        }
    }
    let trimmed = safe.trim_matches(&['.', '_', '-'][..]);
    if trimmed.is_empty() || trimmed.len() > 100 {
        format!("attachment_{}.{}", common::random_hex(4), ext)
    } else if !trimmed.contains('.') {
        format!("{trimmed}.{ext}")
    } else {
        trimmed.to_string()
    }
}

/// Extracts an InputFile from an attachment source object, rejecting arbitrary URLs and paths.
fn extract_input_file(name_hint: &str, source: &Value) -> Result<InputFile, Failure> {
    let Value::Object(src) = source else {
        return Err(fail(400, "Attachment source must be a JSON object"));
    };

    if src.contains_key("url") || src.contains_key("path") {
        return Err(fail(
            400,
            "Unsupported attachment source: arbitrary URLs and local paths are not allowed, only base64 is supported",
        ));
    }

    let source_type = src.get("type").and_then(Value::as_str).unwrap_or("");
    if source_type != "base64" {
        return Err(fail(
            400,
            "Unsupported attachment source type: only base64 is supported",
        ));
    }

    let media_type = src
        .get("media_type")
        .and_then(Value::as_str)
        .ok_or_else(|| fail(400, "Attachment source missing media_type"))?;

    let data_b64 = src
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| fail(400, "Attachment source missing data"))?;

    let filename = sanitize_name(name_hint, ext_for_mime(media_type));
    InputFile::from_base64(&filename, media_type, data_b64)
}

/// Validates whether a tool name is syntactically allowed.
fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Checks whether a JSON Schema value contains any external/remote `$ref` values.
fn check_no_external_refs(val: &Value) -> Result<(), Failure> {
    match val {
        Value::Object(map) => {
            if let Some(ref_val) = map.get("$ref").and_then(Value::as_str) {
                // Local refs start with '#' (e.g. #/$defs/MyType or #/definitions/MyType)
                if !ref_val.starts_with('#') {
                    return Err(fail(
                        400,
                        format!("External or non-local $ref '{ref_val}' is not allowed in offline schemas"),
                    ));
                }
            }
            for v in map.values() {
                check_no_external_refs(v)?;
            }
        }
        Value::Array(arr) => {
            for v in arr {
                check_no_external_refs(v)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Compiles and validates a tool schema offline using `jsonschema`.
fn compile_tool_schema(tool_name: &str, schema: &Value) -> Result<jsonschema::Validator, Failure> {
    if !schema.is_object() {
        return Err(fail(
            400,
            format!("Tool '{tool_name}' input_schema must be a JSON object"),
        ));
    }

    check_no_external_refs(schema)?;

    jsonschema::options()
        .offline()
        .build(schema)
        .map_err(|e| fail(400, format!("Tool '{tool_name}' schema error: {e}")))
}

/// Validates tool arguments against a tool's input_schema using `jsonschema`.
fn validate_tool_args(tool: &Value, name: &str, args: &Value) -> Result<(), Failure> {
    if !args.is_object() {
        return Err(fail(
            502,
            format!("Tool call '{name}' arguments must be a JSON object"),
        ));
    }

    let schema = tool.get("input_schema").unwrap_or(&Value::Null);
    if schema.is_null() {
        return Ok(());
    }

    let validator = compile_tool_schema(name, schema)?;
    if let Err(error) = validator.validate(args) {
        return Err(fail(
            502,
            format!("Tool call '{name}' invalid arguments: {error}"),
        ));
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ToolChoiceMode {
    Auto,
    None,
    Any,
    Tool(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedToolChoice {
    mode: ToolChoiceMode,
    disable_parallel: bool,
}

/// Normalizes tool_choice consistently across build and finish.
fn normalize_tool_choice(
    tc_val: Option<&Value>,
    available_tools: &[Value],
) -> Result<NormalizedToolChoice, Failure> {
    let Some(val) = tc_val else {
        return Ok(NormalizedToolChoice {
            mode: ToolChoiceMode::Auto,
            disable_parallel: false,
        });
    };

    if val.is_null() {
        return Ok(NormalizedToolChoice {
            mode: ToolChoiceMode::Auto,
            disable_parallel: false,
        });
    }

    if let Some(s) = val.as_str() {
        let mode = match s {
            "auto" => ToolChoiceMode::Auto,
            "none" => ToolChoiceMode::None,
            "any" => {
                if available_tools.is_empty() {
                    return Err(fail(
                        400,
                        "tool_choice 'any' requires at least one available tool",
                    ));
                }
                ToolChoiceMode::Any
            }
            other => {
                return Err(fail(
                    400,
                    format!("Unsupported or invalid tool_choice string: '{other}'"),
                ));
            }
        };
        return Ok(NormalizedToolChoice {
            mode,
            disable_parallel: false,
        });
    }

    let Value::Object(map) = val else {
        return Err(fail(400, "tool_choice must be a JSON object or string"));
    };

    let type_str = map
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| fail(400, "tool_choice object missing 'type' field"))?;

    let disable_parallel = map
        .get("disable_parallel_tool_use")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mode = match type_str {
        "auto" => ToolChoiceMode::Auto,
        "none" => ToolChoiceMode::None,
        "any" => {
            if available_tools.is_empty() {
                return Err(fail(
                    400,
                    "tool_choice 'any' requires at least one available tool",
                ));
            }
            ToolChoiceMode::Any
        }
        "tool" => {
            let name = map
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| fail(400, "tool_choice with type 'tool' requires a 'name' field"))?;
            if !available_tools
                .iter()
                .any(|t| t.get("name").and_then(Value::as_str) == Some(name))
            {
                return Err(fail(
                    400,
                    format!("tool_choice specifies tool '{name}' which is not in available tools"),
                ));
            }
            ToolChoiceMode::Tool(name.to_string())
        }
        other => {
            return Err(fail(
                400,
                format!("Unsupported or invalid tool_choice type: '{other}'"),
            ));
        }
    };

    Ok(NormalizedToolChoice {
        mode,
        disable_parallel,
    })
}

/// Builds the tool bridge system instruction prompt.
fn build_tool_bridge_prompt(
    tools: &[Value],
    normalized_tc: &NormalizedToolChoice,
    base_prompt: &str,
) -> String {
    let mut b = String::new();
    b.push_str("You are a Claude-compatible assistant running behind a bridge that supports tool use.\n");
    b.push_str("You MUST respond with JSON only. Do not output markdown code fences.\n");
    b.push_str("Output schema:\n");
    b.push_str("{\"status\":\"tool_use\",\"tool_calls\":[{\"id\":\"<unique_id>\",\"name\":\"<tool_name>\",\"input\":{}}]} OR {\"status\":\"text\",\"content\":\"<assistant_text>\"}\n");
    b.push_str("Rules:\n");
    b.push_str("- Use only tool names listed below.\n");
    b.push_str("- input must be valid JSON object.\n");

    match &normalized_tc.mode {
        ToolChoiceMode::Any => {
            b.push_str("- You MUST call at least one tool. Do not return status 'text'.\n");
        }
        ToolChoiceMode::Tool(tname) => {
            b.push_str(&format!(
                "- You MUST call the tool '{tname}'. Do not return status 'text' or call other tools.\n"
            ));
        }
        _ => {}
    }

    if normalized_tc.disable_parallel {
        b.push_str("- Parallel tool use is disabled: you MUST call at most one tool in tool_calls.\n");
    }

    b.push_str("Available tools:\n");
    for t in tools {
        let name = t["name"].as_str().unwrap_or("");
        b.push_str(&format!("- name: {name}"));
        if let Some(desc) = t["description"].as_str().filter(|s| !s.is_empty()) {
            b.push_str(&format!(" | description: {desc}"));
        }
        if let Some(schema) = t.get("input_schema").filter(|s| !s.is_null()) {
            b.push_str(&format!(" | input_schema: {schema}"));
        }
        b.push('\n');
    }

    b.push_str("\nConversation:\n");
    b.push_str(base_prompt);
    b
}

/// Strips markdown code fence if it strictly encloses the entire string.
fn strip_outer_code_fence(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    if !trimmed.starts_with("```") {
        return None;
    }
    let after_prefix = trimmed.strip_prefix("```")?;
    let after_lang = if let Some(stripped) = after_prefix.strip_prefix("json") {
        stripped
    } else if let Some(stripped) = after_prefix.strip_prefix("JSON") {
        stripped
    } else {
        after_prefix
    };
    let last_idx = after_lang.rfind("```")?;
    let trailing = after_lang[last_idx + 3..].trim();
    if trailing.is_empty() {
        Some(after_lang[..last_idx].trim())
    } else {
        None
    }
}

/// Parses ONLY a complete bridge JSON document or a single complete outer JSON fence.
/// Returns None if the input contains surrounding prose or is not a strict bridge JSON document.
fn parse_strict_bridge_document(raw_text: &str) -> Option<Value> {
    let trimmed = raw_text.trim();

    // 1. Direct JSON document
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if v.is_object()
            && (v.get("status") == Some(&json!("tool_use"))
                || v.get("status") == Some(&json!("text")))
        {
            return Some(v);
        }
    }

    // 2. Single complete outer code fence wrapping the entire output
    if let Some(inner) = strip_outer_code_fence(trimmed) {
        if let Ok(v) = serde_json::from_str::<Value>(inner) {
            if v.is_object()
                && (v.get("status") == Some(&json!("tool_use"))
                    || v.get("status") == Some(&json!("text")))
            {
                return Some(v);
            }
        }
    }

    None
}

/// Builds the Gemini Web prompt and extracted files from an Anthropic Messages request.
pub fn build(body: &Value) -> Result<(String, Vec<InputFile>), Failure> {
    let Value::Object(req_obj) = body else {
        return Err(fail(400, "Request body must be a JSON object"));
    };

    // System prompt
    let system_text = match req_obj.get("system") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for block in arr {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    parts.push(t.trim());
                } else {
                    return Err(fail(400, "Invalid system block format: expected text block"));
                }
            }
            parts.join("\n")
        }
        Some(_) => return Err(fail(400, "System prompt must be a string or array of text blocks")),
    };

    // Messages
    let messages = req_obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| fail(400, "messages field must be an array"))?;

    if messages.is_empty() {
        return Err(fail(400, "messages array cannot be empty"));
    }

    let mut files = Vec::new();
    let mut conversation_lines = Vec::new();
    let mut has_any_content = false;
    let mut file_counter = 0;

    for msg in messages {
        let Value::Object(msg_map) = msg else {
            return Err(fail(400, "Each message must be a JSON object"));
        };

        let role_raw = msg_map
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(400, "Message missing role"))?;

        let role_label = match role_raw.to_ascii_lowercase().as_str() {
            "assistant" | "model" => "Model",
            "user" => "User",
            "system" => "System",
            other => return Err(fail(400, format!("Invalid message role: '{other}'"))),
        };

        let mut turn_text_parts = Vec::new();
        let mut turn_tool_calls = Vec::new();
        let mut turn_files_count = 0usize;

        match msg_map.get("content") {
            Some(Value::String(s)) => {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    has_any_content = true;
                    turn_text_parts.push(trimmed.to_string());
                }
            }
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    let Value::Object(b_obj) = block else {
                        return Err(fail(400, "Content block must be a JSON object"));
                    };
                    let b_type = b_obj
                        .get("type")
                        .and_then(Value::as_str)
                        .ok_or_else(|| fail(400, "Content block missing type"))?;

                    match b_type {
                        "text" => {
                            let text = b_obj
                                .get("text")
                                .and_then(Value::as_str)
                                .ok_or_else(|| fail(400, "Text block missing text field"))?;
                            if !text.trim().is_empty() {
                                has_any_content = true;
                                turn_text_parts.push(text.trim().to_string());
                            }
                        }
                        "thinking" => {
                            if let Some(t) = b_obj.get("thinking").and_then(Value::as_str) {
                                if !t.trim().is_empty() {
                                    has_any_content = true;
                                    turn_text_parts.push(format!("[Thinking: {}]", t.trim()));
                                }
                            }
                        }
                        "image" => {
                            has_any_content = true;
                            file_counter += 1;
                            turn_files_count += 1;
                            let src = b_obj
                                .get("source")
                                .ok_or_else(|| fail(400, "Image block missing source"))?;
                            let name_hint = format!("image_{file_counter}");
                            let file = extract_input_file(&name_hint, src)?;
                            files.push(file);
                        }
                        "document" => {
                            has_any_content = true;
                            file_counter += 1;
                            turn_files_count += 1;
                            let src = b_obj
                                .get("source")
                                .ok_or_else(|| fail(400, "Document block missing source"))?;
                            let title = b_obj
                                .get("title")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let name_hint = if !title.is_empty() {
                                title.to_string()
                            } else {
                                format!("document_{file_counter}")
                            };
                            let file = extract_input_file(&name_hint, src)?;
                            files.push(file);
                        }
                        "tool_use" => {
                            has_any_content = true;
                            let id = b_obj
                                .get("id")
                                .and_then(Value::as_str)
                                .ok_or_else(|| fail(400, "tool_use block missing id"))?;
                            let name = b_obj
                                .get("name")
                                .and_then(Value::as_str)
                                .ok_or_else(|| fail(400, "tool_use block missing name"))?;
                            let input = b_obj
                                .get("input")
                                .ok_or_else(|| fail(400, "tool_use block missing input"))?;
                            if !input.is_object() {
                                return Err(fail(400, "tool_use input must be a JSON object"));
                            }
                            turn_tool_calls.push(json!({
                                "id": id,
                                "name": name,
                                "input": input
                            }));
                        }
                        "tool_result" => {
                            has_any_content = true;
                            let tool_use_id = b_obj
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .ok_or_else(|| fail(400, "tool_result block missing tool_use_id"))?;
                            let is_error = b_obj
                                .get("is_error")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);

                            let mut res_text_parts = Vec::new();
                            match b_obj.get("content") {
                                Some(Value::String(s)) => {
                                    if !s.trim().is_empty() {
                                        res_text_parts.push(s.trim().to_string());
                                    }
                                }
                                Some(Value::Array(nested_blocks)) => {
                                    for nb in nested_blocks {
                                        if let Some(nb_map) = nb.as_object() {
                                            match nb_map.get("type").and_then(Value::as_str) {
                                                Some("text") => {
                                                    if let Some(t) = nb_map.get("text").and_then(Value::as_str) {
                                                        if !t.trim().is_empty() {
                                                            res_text_parts.push(t.trim().to_string());
                                                        }
                                                    }
                                                }
                                                Some("tool_reference") => {
                                                    if let Some(tname) = nb_map
                                                        .get("tool_name")
                                                        .and_then(Value::as_str)
                                                        .or_else(|| nb_map.get("name").and_then(Value::as_str))
                                                    {
                                                        res_text_parts.push(format!("Tool {tname} is available."));
                                                    }
                                                }
                                                Some("image") => {
                                                    file_counter += 1;
                                                    turn_files_count += 1;
                                                    let src = nb_map.get("source").ok_or_else(|| fail(400, "Nested image missing source"))?;
                                                    let file = extract_input_file(&format!("result_image_{file_counter}"), src)?;
                                                    let fname = file.name.clone();
                                                    files.push(file);
                                                    res_text_parts.push(format!("[Image attached: {fname}]"));
                                                }
                                                Some("document") => {
                                                    file_counter += 1;
                                                    turn_files_count += 1;
                                                    let src = nb_map.get("source").ok_or_else(|| fail(400, "Nested document missing source"))?;
                                                    let file = extract_input_file(&format!("result_doc_{file_counter}"), src)?;
                                                    let fname = file.name.clone();
                                                    files.push(file);
                                                    res_text_parts.push(format!("[Document attached: {fname}]"));
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                }
                                Some(other) if !other.is_null() => {
                                    res_text_parts.push(other.to_string());
                                }
                                _ => {}
                            }

                            let content_summary = if res_text_parts.is_empty() {
                                if is_error {
                                    "[error: empty output]".to_string()
                                } else {
                                    "[empty output]".to_string()
                                }
                            } else {
                                res_text_parts.join("\n")
                            };

                            let prefix = if is_error { "Tool error" } else { "Tool result" };
                            turn_text_parts.push(format!("[{prefix} (tool_use_id: {tool_use_id})]: {content_summary}"));
                        }
                        other => {
                            if b_obj.contains_key("url") || b_obj.contains_key("path") {
                                return Err(fail(400, format!("Unsupported attachment/block '{other}' with external url/path")));
                            }
                            if let Some(t) = b_obj.get("text").and_then(Value::as_str) {
                                if !t.trim().is_empty() {
                                    has_any_content = true;
                                    turn_text_parts.push(t.trim().to_string());
                                }
                            }
                        }
                    }
                }
            }
            Some(Value::Null) | None => {}
            Some(_) => {
                return Err(fail(400, "Message content must be a string or array of blocks"));
            }
        }

        // Aggregate parallel historical tool calls from this assistant turn
        if !turn_tool_calls.is_empty() {
            let aggregated = json!({
                "status": "tool_use",
                "tool_calls": turn_tool_calls
            });
            turn_text_parts.push(aggregated.to_string());
        }

        let mut turn_content_parts = Vec::new();
        if turn_text_parts.is_empty() && turn_files_count > 0 {
            turn_content_parts.push(format!("[{turn_files_count} file(s) attached]"));
        } else {
            turn_content_parts.extend(turn_text_parts);
        }

        let turn_content = turn_content_parts.join("\n");
        if !turn_content.is_empty() {
            conversation_lines.push(format!("{role_label}: {turn_content}"));
        }
    }

    if !has_any_content && files.is_empty() {
        return Err(fail(400, "all messages have empty content"));
    }

    let mut base_prompt = String::new();
    if !system_text.is_empty() {
        base_prompt.push_str(&format!("System: {system_text}\n\n"));
    }
    base_prompt.push_str(&conversation_lines.join("\n"));

    // Tool extraction & validation
    let resolved_tools_val = crate::proxy::sse::client_tools(body);
    let raw_tools = resolved_tools_val.as_array().map(|v| v.as_slice()).unwrap_or(&[]);

    let mut valid_tools = Vec::new();
    for t in raw_tools {
        let name = t.get("name").and_then(Value::as_str).unwrap_or("");
        if !is_valid_tool_name(name) {
            return Err(fail(400, format!("Disallowed or invalid tool name: '{name}'")));
        }
        if let Some(schema) = t.get("input_schema") {
            compile_tool_schema(name, schema)?;
        }
        valid_tools.push(t.clone());
    }

    let normalized_tc = normalize_tool_choice(req_obj.get("tool_choice"), &valid_tools)?;
    let has_tools = !valid_tools.is_empty() && normalized_tc.mode != ToolChoiceMode::None;

    let final_prompt = if has_tools {
        build_tool_bridge_prompt(&valid_tools, &normalized_tc, &base_prompt)
    } else {
        base_prompt
    };

    Ok((final_prompt, files))
}

/// Converts a Gemini Web wire Reply into a standard Anthropic Messages response JSON Value.
pub fn finish(body: &Value, reply: &Reply, shown: &str) -> Result<Value, Failure> {
    // Hidden reasoning is not a replacement for final content.
    if reply.text.trim().is_empty() && reply.images.is_empty() && reply.canvases.is_empty() {
        return Err(fail(
            502,
            "Google Web response contained thinking or empty content without final output",
        ));
    }

    let resolved_tools_val = crate::proxy::sse::client_tools(body);
    let raw_tools = resolved_tools_val.as_array().map(|v| v.as_slice()).unwrap_or(&[]);
    let valid_tools: Vec<Value> = raw_tools
        .iter()
        .filter(|t| is_valid_tool_name(t.get("name").and_then(Value::as_str).unwrap_or("")))
        .cloned()
        .collect();

    let normalized_tc = normalize_tool_choice(body.get("tool_choice"), &valid_tools)?;
    let has_tools = !valid_tools.is_empty() && normalized_tc.mode != ToolChoiceMode::None;

    let mut content_blocks = Vec::new();
    if let Some(thinking) = &reply.thinking {
        if !thinking.trim().is_empty() {
            content_blocks.push(json!({
                "type": "thinking",
                "thinking": thinking.trim()
            }));
        }
    }

    let mut stop_reason = "end_turn";

    if has_tools {
        let parsed_doc = parse_strict_bridge_document(&reply.text);

        match parsed_doc {
            Some(doc) => {
                let status = doc.get("status").and_then(Value::as_str).unwrap_or("");
                if status == "tool_use" {
                    let tool_calls = doc
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .ok_or_else(|| fail(502, "tool_use status returned without tool_calls array"))?;

                    if tool_calls.is_empty() {
                        return Err(fail(502, "Model returned empty tool_calls when tool_use was signaled"));
                    }

                    if normalized_tc.disable_parallel && tool_calls.len() > 1 {
                        return Err(fail(
                            502,
                            "Parallel tool use is disabled but model returned multiple tool calls",
                        ));
                    }

                    let mut uses = Vec::new();
                    for tc in tool_calls {
                        let tc_map = tc.as_object().ok_or_else(|| {
                            fail(502, "Malformed tool call entry: expected JSON object")
                        })?;
                        let name = tc_map
                            .get("name")
                            .and_then(Value::as_str)
                            .ok_or_else(|| fail(502, "Tool call missing name"))?;

                        let tool_def = valid_tools
                            .iter()
                            .find(|t| t["name"] == name)
                            .ok_or_else(|| {
                                fail(502, format!("Model attempted to call unlisted tool '{name}'"))
                            })?;

                        if let ToolChoiceMode::Tool(required) = &normalized_tc.mode {
                            if name != required {
                                return Err(fail(
                                    502,
                                    format!("Model called tool '{name}' but tool_choice required '{required}'"),
                                ));
                            }
                        }

                        let input = tc_map
                            .get("input")
                            .ok_or_else(|| fail(502, format!("Tool call '{name}' missing input")))?;
                        validate_tool_args(tool_def, name, input)?;

                        let id = tc_map
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("toolu_{}", common::random_hex(12)));

                        uses.push(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input
                        }));
                    }

                    stop_reason = "tool_use";
                    for tu in uses {
                        content_blocks.push(tu);
                    }
                } else if status == "text" {
                    if matches!(normalized_tc.mode, ToolChoiceMode::Any | ToolChoiceMode::Tool(_)) {
                        return Err(fail(
                            502,
                            "Model returned text response when tool use was required by tool_choice",
                        ));
                    }
                    let content = doc.get("content").and_then(Value::as_str).unwrap_or(&reply.text);
                    content_blocks.push(json!({
                        "type": "text",
                        "text": content
                    }));
                } else {
                    if matches!(normalized_tc.mode, ToolChoiceMode::Any | ToolChoiceMode::Tool(_)) {
                        return Err(fail(
                            502,
                            "Model did not return required tool call",
                        ));
                    }
                    content_blocks.push(json!({
                        "type": "text",
                        "text": reply.text
                    }));
                }
            }
            None => {
                // Not a strict bridge document (could be conversational prose containing quoted JSON or normal text)
                if matches!(normalized_tc.mode, ToolChoiceMode::Any | ToolChoiceMode::Tool(_)) {
                    return Err(fail(
                        502,
                        "Model returned prose or unparsed output when tool use was required by tool_choice",
                    ));
                }
                content_blocks.push(json!({
                    "type": "text",
                    "text": reply.text
                }));
            }
        }
    } else {
        content_blocks.push(json!({
            "type": "text",
            "text": reply.text
        }));
    }

    // Token estimates: using ccat proxy::sse::weigh helper (excludes base64 data to prevent phantom millions)
    let (prompt_bytes, prompt_images) = crate::proxy::sse::weigh(body);
    let input_tokens = ((prompt_bytes as f64 * crate::proxy::sse::DEFAULT_RATIO).round() as u64
        + prompt_images * crate::proxy::sse::IMAGE_TOKENS)
        .max(1);

    let (out_bytes, out_images) = crate::proxy::sse::weigh(&json!({
        "text": &reply.text,
        "thinking": &reply.thinking
    }));
    let output_tokens = ((out_bytes as f64 * crate::proxy::sse::DEFAULT_RATIO).round() as u64
        + out_images * crate::proxy::sse::IMAGE_TOKENS)
        .max(1);

    let usage = json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "estimated": true
    });

    let mut meta = json!({
        "estimated_usage": true,
        "requested_model": shown,
        "selected_model": reply.selected_model,
        "selected_model_id": reply.selected_model_id,
    });
    if let Some(act) = &reply.actual_model {
        meta["actual_model"] = json!(act);
    }
    if let Some(act_id) = &reply.actual_model_id {
        meta["actual_model_id"] = json!(act_id);
    }
    if let Some(cid) = &reply.conversation_id {
        meta["conversation_id"] = json!(cid);
    }
    if let Some(rid) = &reply.response_id {
        meta["response_id"] = json!(rid);
    }
    if let Some(rcid) = &reply.choice_id {
        meta["choice_id"] = json!(rcid);
    }

    let msg_id = format!("msg_{}", common::random_hex(12));

    let mut res = json!({
        "id": msg_id,
        "type": "message",
        "role": "assistant",
        "model": shown,
        "content": content_blocks,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": usage,
        "metadata": meta,
    });

    if !reply.canvases.is_empty() {
        let canvases_json: Vec<Value> = reply
            .canvases
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "title": c.title,
                    "file_name": c.file_name,
                    "type": c.canvas_type,
                    "language": c.language,
                    "content": c.content,
                })
            })
            .collect();
        res["canvases"] = json!(canvases_json);
    }

    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_text_conversation_build_and_finish() {
        let req = json!({
            "model": "gemini-3.1-pro",
            "system": "You are a helpful assistant.",
            "messages": [
                {"role": "user", "content": "Hello!"},
                {"role": "assistant", "content": "Hi there!"},
                {"role": "user", "content": "How are you?"}
            ]
        });

        let (prompt, files) = build(&req).expect("build");
        assert!(files.is_empty());
        assert!(prompt.contains("System: You are a helpful assistant."));
        assert!(prompt.contains("User: Hello!"));
        assert!(prompt.contains("Model: Hi there!"));
        assert!(prompt.contains("User: How are you?"));

        let reply = Reply {
            text: "I am doing well, thank you!".to_string(),
            selected_model: "gemini-pro".to_string(),
            selected_model_id: "pro-123".to_string(),
            actual_model: Some("gemini-pro".to_string()),
            actual_model_id: Some("pro-123".to_string()),
            conversation_id: Some("conv-1".to_string()),
            ..Default::default()
        };

        let res = finish(&req, &reply, "gemini-3.1-pro").expect("finish");
        assert_eq!(res["type"], "message");
        assert_eq!(res["role"], "assistant");
        assert_eq!(res["model"], "gemini-3.1-pro");
        assert_eq!(res["stop_reason"], "end_turn");
        assert_eq!(res["content"][0]["type"], "text");
        assert_eq!(res["content"][0]["text"], "I am doing well, thank you!");
        assert_eq!(res["usage"]["estimated"], true);
        assert_eq!(res["metadata"]["selected_model"], "gemini-pro");
        assert_eq!(res["metadata"]["actual_model"], "gemini-pro");
        assert_eq!(res["metadata"]["conversation_id"], "conv-1");
    }

    #[test]
    fn test_attachments_base64_images_and_documents() {
        let req = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "text",
                            "text": "What is in this image and doc?"
                        },
                        {
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": "image/png",
                                "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAAAAAA6fptVAAAACklEQVR4nGNiAAAABgADNjd8qAAAAABJRU5ErkJggg=="
                            }
                        },
                        {
                            "type": "document",
                            "title": "sample.pdf",
                            "source": {
                                "type": "base64",
                                "media_type": "application/pdf",
                                "data": "JVBERi0xLjQK"
                            }
                        }
                    ]
                }
            ]
        });

        let (prompt, files) = build(&req).expect("build");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].mime_type, "image/png");
        assert_eq!(files[1].mime_type, "application/pdf");
        assert_eq!(files[1].name, "sample.pdf");
        assert!(prompt.contains("User: What is in this image and doc?"));
    }

    #[test]
    fn test_attachments_rejects_arbitrary_url_and_path() {
        let req_url = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image",
                    "source": {
                        "type": "url",
                        "url": "https://example.com/test.png"
                    }
                }]
            }]
        });
        assert!(build(&req_url).is_err());

        let req_path = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "document",
                    "source": {
                        "type": "file",
                        "path": "/etc/passwd"
                    }
                }]
            }]
        });
        assert!(build(&req_path).is_err());
    }

    #[test]
    fn test_tool_result_with_nested_image_and_document() {
        let req = json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "toolu_view",
                            "name": "view_image",
                            "input": {"id": "1"}
                        }
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_view",
                            "content": [
                                {"type": "text", "text": "Screenshot captured:"},
                                {
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": "image/png",
                                        "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAAAAAA6fptVAAAACklEQVR4nGNiAAAABgADNjd8qAAAAABJRU5ErkJggg=="
                                    }
                                }
                            ]
                        }
                    ]
                }
            ]
        });

        let (prompt, files) = build(&req).expect("build");
        assert_eq!(files.len(), 1);
        assert!(prompt.contains("[Tool result (tool_use_id: toolu_view)]:"));
        assert!(prompt.contains("Screenshot captured:"));
        assert!(prompt.contains("[Image attached:"));
    }

    #[test]
    fn test_tool_result_is_error_and_empty_output() {
        let req = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_err",
                            "is_error": true,
                            "content": ""
                        }
                    ]
                }
            ]
        });

        let (prompt, _) = build(&req).expect("build");
        assert!(prompt.contains("[Tool error (tool_use_id: toolu_err)]: [error: empty output]"));
    }

    #[test]
    fn test_tool_definitions_and_disallowed_names() {
        let req_bad_name = json!({
            "messages": [{"role": "user", "content": "Run tool"}],
            "tools": [
                {
                    "name": "bad name with spaces",
                    "input_schema": {"type": "object"}
                }
            ]
        });
        assert!(build(&req_bad_name).is_err());

        let req_empty_name = json!({
            "messages": [{"role": "user", "content": "Run tool"}],
            "tools": [
                {
                    "name": "",
                    "input_schema": {"type": "object"}
                }
            ]
        });
        assert!(build(&req_empty_name).is_err());
    }

    #[test]
    fn test_standard_tool_schemas_with_union_and_local_ref() {
        let req = json!({
            "messages": [{"role": "user", "content": "Run tool"}],
            "tools": [
                {
                    "name": "search",
                    "input_schema": {
                        "type": "object",
                        "$defs": {
                            "Filter": {
                                "type": "string"
                            }
                        },
                        "properties": {
                            "query": {
                                "anyOf": [
                                    {"type": "string"},
                                    {"type": "array", "items": {"type": "string"}}
                                ]
                            },
                            "filter": {
                                "$ref": "#/$defs/Filter"
                            }
                        },
                        "required": ["query"]
                    }
                }
            ]
        });

        // Offline compilation must succeed for anyOf, oneOf, allOf, and local $defs/$ref
        let (prompt, _) = build(&req).expect("build with anyOf and local $ref");
        assert!(prompt.contains("search"));

        // Valid arguments: string query
        let reply_str = Reply {
            text: json!({
                "status": "tool_use",
                "tool_calls": [{"name": "search", "input": {"query": "rust", "filter": "active"}}]
            }).to_string(),
            ..Default::default()
        };
        let res_str = finish(&req, &reply_str, "gemini-pro").expect("finish str query");
        assert_eq!(res_str["stop_reason"], "tool_use");

        // Valid arguments: array query
        let reply_arr = Reply {
            text: json!({
                "status": "tool_use",
                "tool_calls": [{"name": "search", "input": {"query": ["rust", "ccat"]}}]
            }).to_string(),
            ..Default::default()
        };
        let res_arr = finish(&req, &reply_arr, "gemini-pro").expect("finish arr query");
        assert_eq!(res_arr["stop_reason"], "tool_use");

        // Invalid arguments: query is boolean (not string or array)
        let reply_invalid = Reply {
            text: json!({
                "status": "tool_use",
                "tool_calls": [{"name": "search", "input": {"query": true}}]
            }).to_string(),
            ..Default::default()
        };
        assert!(finish(&req, &reply_invalid, "gemini-pro").is_err());
    }

    #[test]
    fn test_schema_rejects_external_ref() {
        let req_ext = json!({
            "messages": [{"role": "user", "content": "Run tool"}],
            "tools": [
                {
                    "name": "remote_tool",
                    "input_schema": {
                        "type": "object",
                        "$ref": "https://example.com/schemas/tool.json"
                    }
                }
            ]
        });
        assert!(build(&req_ext).is_err());

        let req_file = json!({
            "messages": [{"role": "user", "content": "Run tool"}],
            "tools": [
                {
                    "name": "file_tool",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "payload": {
                                "$ref": "file:///etc/schema.json"
                            }
                        }
                    }
                }
            ]
        });
        assert!(build(&req_file).is_err());
    }

    #[test]
    fn test_negative_non_fenced_quoted_json_in_prose() {
        let req_auto = json!({
            "messages": [{"role": "user", "content": "Can you show me an example of a tool call?"}],
            "tools": [
                {"name": "calc", "input_schema": {"type": "object"}}
            ]
        });

        let prose_with_raw_json = "Sure, here is how a tool call would look like:\n{\"status\":\"tool_use\",\"tool_calls\":[{\"name\":\"calc\",\"input\":{}}]}\nHope that helps!";
        let reply = Reply {
            text: prose_with_raw_json.to_string(),
            ..Default::default()
        };

        // In auto mode: surrounding prose remains non-executable text!
        let res = finish(&req_auto, &reply, "gemini-pro").expect("finish auto prose");
        assert_eq!(res["stop_reason"], "end_turn");
        assert_eq!(res["content"][0]["type"], "text");
        assert!(res["content"][0]["text"].as_str().unwrap().contains("Sure, here is how a tool call"));

        // In forced mode: surrounding prose is rejected as invalid pre-content output!
        let req_forced = json!({
            "messages": [{"role": "user", "content": "Calculate 2+2"}],
            "tools": [
                {"name": "calc", "input_schema": {"type": "object"}}
            ],
            "tool_choice": {"type": "any"}
        });
        assert!(finish(&req_forced, &reply, "gemini-pro").is_err());
    }

    #[test]
    fn test_fenced_json_tool_output() {
        let req = json!({
            "messages": [{"role": "user", "content": "Calculate 2+2"}],
            "tools": [
                {
                    "name": "calc",
                    "input_schema": {
                        "type": "object",
                        "properties": {"expr": {"type": "string"}},
                        "required": ["expr"]
                    }
                }
            ]
        });

        let raw_fenced = "```json\n{\"status\":\"tool_use\",\"tool_calls\":[{\"id\":\"call_1\",\"name\":\"calc\",\"input\":{\"expr\":\"2+2\"}}]}\n```";
        let reply = Reply {
            text: raw_fenced.to_string(),
            ..Default::default()
        };

        let res = finish(&req, &reply, "gemini-pro").expect("finish");
        assert_eq!(res["stop_reason"], "tool_use");
        assert_eq!(res["content"][0]["type"], "tool_use");
        assert_eq!(res["content"][0]["name"], "calc");
        assert_eq!(res["content"][0]["id"], "call_1");
        assert_eq!(res["content"][0]["input"]["expr"], "2+2");
    }

    #[test]
    fn test_quoted_json_in_prose_not_executed() {
        let req = json!({
            "messages": [{"role": "user", "content": "How do tool calls look?"}],
            "tools": [
                {"name": "calc", "input_schema": {"type": "object"}}
            ]
        });

        let prose_with_fence = "Here is an example of what a tool call looks like:\n```json\n{\"status\":\"tool_use\",\"tool_calls\":[{\"name\":\"calc\",\"input\":{}}]}\n```\nDo not execute this example.";
        let reply = Reply {
            text: prose_with_fence.to_string(),
            ..Default::default()
        };

        let res = finish(&req, &reply, "gemini-pro").expect("finish");
        // Quoted inside prose code fence must NOT become an executable tool call!
        assert_eq!(res["stop_reason"], "end_turn");
        assert_eq!(res["content"][0]["type"], "text");
        assert!(res["content"][0]["text"].as_str().unwrap().contains("Here is an example"));
    }

    #[test]
    fn test_tool_choice_normalization() {
        let tools = vec![json!({"name": "calc", "input_schema": {"type": "object"}})];

        // Normal object forms
        assert_eq!(
            normalize_tool_choice(Some(&json!({"type": "auto"})), &tools).unwrap().mode,
            ToolChoiceMode::Auto
        );
        assert_eq!(
            normalize_tool_choice(Some(&json!({"type": "none"})), &tools).unwrap().mode,
            ToolChoiceMode::None
        );
        assert_eq!(
            normalize_tool_choice(Some(&json!({"type": "any"})), &tools).unwrap().mode,
            ToolChoiceMode::Any
        );
        assert_eq!(
            normalize_tool_choice(Some(&json!({"type": "tool", "name": "calc"})), &tools).unwrap().mode,
            ToolChoiceMode::Tool("calc".to_string())
        );

        // String forms
        assert_eq!(
            normalize_tool_choice(Some(&json!("auto")), &tools).unwrap().mode,
            ToolChoiceMode::Auto
        );
        assert_eq!(
            normalize_tool_choice(Some(&json!("none")), &tools).unwrap().mode,
            ToolChoiceMode::None
        );
        assert_eq!(
            normalize_tool_choice(Some(&json!("any")), &tools).unwrap().mode,
            ToolChoiceMode::Any
        );

        // Unknown/invalid forms must error rather than silently fallback
        assert!(normalize_tool_choice(Some(&json!("unknown")), &tools).is_err());
        assert!(normalize_tool_choice(Some(&json!({"type": "unknown"})), &tools).is_err());
        assert!(normalize_tool_choice(Some(&json!({"type": "tool"})), &tools).is_err());
        assert!(normalize_tool_choice(Some(&json!({"type": "tool", "name": "missing"})), &tools).is_err());
    }

    #[test]
    fn test_disable_parallel_tool_use() {
        let req = json!({
            "messages": [{"role": "user", "content": "Calculate 2+2 and 3+3"}],
            "tools": [
                {"name": "calc", "input_schema": {"type": "object"}}
            ],
            "tool_choice": {
                "type": "auto",
                "disable_parallel_tool_use": true
            }
        });

        let (prompt, _) = build(&req).expect("build");
        assert!(prompt.contains("Parallel tool use is disabled"));

        let reply_multi = Reply {
            text: json!({
                "status": "tool_use",
                "tool_calls": [
                    {"name": "calc", "input": {}},
                    {"name": "calc", "input": {}}
                ]
            }).to_string(),
            ..Default::default()
        };
        assert!(finish(&req, &reply_multi, "gemini-pro").is_err());
    }

    #[test]
    fn test_usage_estimation_excludes_base64() {
        // Construct a huge base64 payload (~60KB)
        let large_b64 = "A".repeat(60_000);
        let req = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "Short prompt"},
                        {
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": "image/png",
                                "data": large_b64
                            }
                        }
                    ]
                }
            ]
        });

        let reply = Reply {
            text: "Short answer".to_string(),
            ..Default::default()
        };

        let res = finish(&req, &reply, "gemini-pro").expect("finish");
        let input_tokens = res["usage"]["input_tokens"].as_u64().unwrap();
        // Base64 data string must NOT produce 15,000+ phantom tokens!
        // It should weigh text (~200 bytes) + 1100 per image = ~1150 tokens.
        assert!(input_tokens < 2000, "Input tokens was {input_tokens}, expected < 2000");
    }

    #[test]
    fn test_nested_tool_reference_in_tool_result() {
        let req = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_search",
                            "content": [
                                {
                                    "type": "tool_reference",
                                    "tool_name": "specialized_calculator"
                                }
                            ]
                        }
                    ]
                }
            ]
        });

        let (prompt, _) = build(&req).expect("build");
        assert!(prompt.contains("Tool specialized_calculator is available."));
    }

    #[test]
    fn test_per_turn_attachment_count() {
        let req = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": "image/png",
                                "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAAAAAA6fptVAAAACklEQVR4nGNiAAAABgADNjd8qAAAAABJRU5ErkJggg=="
                            }
                        }
                    ]
                },
                {
                    "role": "assistant",
                    "content": "I see the image."
                },
                {
                    "role": "user",
                    "content": "Followup question"
                }
            ]
        });

        let (prompt, files) = build(&req).expect("build");
        assert_eq!(files.len(), 1);
        // Turn 1 had the image, Turn 3 has text "Followup question".
        // Turn 3 must NOT say "[1 file(s) attached]"!
        assert!(!prompt.contains("User: Followup question\n[1 file(s) attached]"));
        assert!(prompt.contains("User: [1 file(s) attached]"));
        assert!(prompt.contains("Model: I see the image."));
        assert!(prompt.contains("User: Followup question"));
    }

    #[test]
    fn test_parallel_tool_use_aggregation() {
        let req = json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "I will calculate both."},
                        {"type": "tool_use", "id": "call_1", "name": "calc", "input": {"expr": "1+1"}},
                        {"type": "tool_use", "id": "call_2", "name": "calc", "input": {"expr": "2+2"}}
                    ]
                }
            ]
        });

        let (prompt, _) = build(&req).expect("build");
        assert!(prompt.contains("Model: I will calculate both."));
        // Both tool calls must be aggregated into one tool_calls array, not separate JSON objects
        assert!(prompt.contains("\"tool_calls\":[{\"id\":\"call_1\",\"name\":\"calc\",\"input\":{\"expr\":\"1+1\"}},{\"id\":\"call_2\",\"name\":\"calc\",\"input\":{\"expr\":\"2+2\"}}]"));
    }

    #[test]
    fn test_thinking_and_hidden_reasoning_not_replacing_content() {
        let req = json!({
            "messages": [{"role": "user", "content": "Explain relativity"}]
        });

        // Only thinking, no final content: must error!
        let reply_only_thinking = Reply {
            thinking: Some("Let me consider Einstein's papers...".to_string()),
            text: "".to_string(),
            ..Default::default()
        };
        assert!(finish(&req, &reply_only_thinking, "gemini-pro").is_err());

        // Thinking + text: both present
        let reply_with_both = Reply {
            thinking: Some("Thinking plan".to_string()),
            text: "Relativity is the theory of gravitation...".to_string(),
            ..Default::default()
        };
        let res = finish(&req, &reply_with_both, "gemini-pro").expect("finish");
        assert_eq!(res["content"][0]["type"], "thinking");
        assert_eq!(res["content"][0]["thinking"], "Thinking plan");
        assert_eq!(res["content"][1]["type"], "text");
        assert_eq!(res["content"][1]["text"], "Relativity is the theory of gravitation...");
    }

    #[test]
    fn test_estimated_usage_and_model_metadata() {
        let req = json!({
            "messages": [{"role": "user", "content": "Hello world"}]
        });

        let reply = Reply {
            text: "Greetings!".to_string(),
            selected_model: "gemini-4-argon".to_string(),
            selected_model_id: "argon-model-id".to_string(),
            actual_model: None, // wire now intentionally leaves actual unknown without evidence
            actual_model_id: None,
            conversation_id: Some("c-123".to_string()),
            response_id: Some("r-456".to_string()),
            choice_id: Some("rc-789".to_string()),
            ..Default::default()
        };

        let res = finish(&req, &reply, "gemini-3.1-pro").expect("finish");
        assert_eq!(res["model"], "gemini-3.1-pro");
        assert_eq!(res["usage"]["estimated"], true);
        assert!(res["usage"]["input_tokens"].as_u64().unwrap() > 0);
        assert!(res["usage"]["output_tokens"].as_u64().unwrap() > 0);

        let meta = &res["metadata"];
        assert_eq!(meta["estimated_usage"], true);
        assert_eq!(meta["requested_model"], "gemini-3.1-pro");
        assert_eq!(meta["selected_model"], "gemini-4-argon");
        assert_eq!(meta["selected_model_id"], "argon-model-id");
        assert!(meta.get("actual_model").is_none());
        assert!(meta.get("actual_model_id").is_none());
        assert_eq!(meta["conversation_id"], "c-123");
        assert_eq!(meta["response_id"], "r-456");
        assert_eq!(meta["choice_id"], "rc-789");
    }
}
