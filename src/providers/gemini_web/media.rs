//! Native media, generation and research operations for Gemini Web.
//! Ports Canvas, Music, Video, and Deep Research capabilities from Argon Local bridge source
//! directly into ccat without local HTTP sidecars or network bridges.

use super::wire::{self, Session, CANVAS_MODEL_SUFFIX, MUSIC_MODEL_SUFFIX};
use crate::common::{fail, Failure};
use serde_json::{json, Value};

pub const OPERATION_CANVAS: &str = "canvas";
pub const OPERATION_GENERATE_MUSIC: &str = "generate_music";
pub const OPERATION_START_VIDEO: &str = "start_video";
pub const OPERATION_GET_VIDEO: &str = "get_video";
pub const OPERATION_DEEP_RESEARCH: &str = "deep_research";

/// Dispatches media and research operations natively against an active authenticated Session.
pub fn execute(session: &mut Session, operation: &str, arguments: &Value) -> Result<Value, Failure> {
    match operation {
        OPERATION_CANVAS => execute_canvas(session, arguments),
        OPERATION_GENERATE_MUSIC => execute_music(session, arguments),
        OPERATION_START_VIDEO => execute_start_video(session, arguments),
        OPERATION_GET_VIDEO => execute_get_video(session, arguments),
        OPERATION_DEEP_RESEARCH => execute_deep_research(session, arguments),
        _ => Err(fail(400, format!("Unsupported Gemini Web operation '{operation}'"))),
    }
}

// ---------------------------------------------------------------- Canvas

fn execute_canvas(session: &mut Session, args: &Value) -> Result<Value, Failure> {
    let prompt = args.get("prompt").and_then(Value::as_str).ok_or_else(|| fail(400, "prompt is required"))?;
    let model = args.get("model").and_then(Value::as_str).unwrap_or("gemini-4-argon");
    let model = if model.ends_with(CANVAS_MODEL_SUFFIX) {
        model.to_string()
    } else {
        format!("{model}{CANVAS_MODEL_SUFFIX}")
    };

    let reply = wire::generate(session, &model, prompt, &[], &json!({"canvas": true}))?;
    let canvases: Vec<Value> = reply.canvases.into_iter().map(|c| {
        json!({
            "id": c.id,
            "title": c.title,
            "file_name": c.file_name,
            "type": c.canvas_type,
            "language": c.language,
            "content": c.content,
        })
    }).collect();

    Ok(json!({
        "text": reply.text,
        "thinking": reply.thinking,
        "model": reply.model,
        "conversation_id": reply.conversation_id,
        "canvases": canvases,
    }))
}

// ---------------------------------------------------------------- Music

fn execute_music(session: &mut Session, args: &Value) -> Result<Value, Failure> {
    let prompt = args.get("prompt").and_then(Value::as_str).ok_or_else(|| fail(400, "prompt is required"))?;
    let model = args.get("model").and_then(Value::as_str).unwrap_or("gemini-4-argon");
    let model = model.strip_suffix(MUSIC_MODEL_SUFFIX).unwrap_or(model);

    // Apply native Music tool mode (geminiMusicToolMode = 3, message_options = [null,null,null,null,null,null,[]])
    let options = json!({
        "inner_overrides": {
            "49": 3,
            "67": 0
        },
        "message_options": [Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, []],
        "web_tools": true
    });

    let reply = wire::generate(session, model, prompt, &[], &options)?;
    let audios: Vec<Value> = reply.audios.into_iter().map(|a| {
        json!({
            "url": a.url,
            "file_name": a.file_name,
            "mime_type": a.mime_type,
            "duration": a.duration,
        })
    }).collect();

    Ok(json!({
        "text": reply.text,
        "conversation_id": reply.conversation_id,
        "audios": audios,
    }))
}

// ---------------------------------------------------------------- Video

fn execute_start_video(session: &mut Session, args: &Value) -> Result<Value, Failure> {
    let prompt = args.get("prompt").and_then(Value::as_str).ok_or_else(|| fail(400, "prompt is required"))?;
    let model = args.get("model").and_then(Value::as_str).unwrap_or("gemini-4-argon");
    let aspect_str = args.get("aspect_ratio").and_then(Value::as_str).unwrap_or("16:9");
    let aspect_code: u32 = if aspect_str == "9:16" { 2 } else { 1 };
    let tool_id: u32 = if aspect_code == 2 { 17 } else { 16 };

    let options = json!({
        "inner_overrides": {
            "49": 2, // geminiVideoToolMode
            "55": [[tool_id]]
        },
        "message_options": [Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, [[Value::Null, Value::Null, Value::Null, aspect_code]]],
        "web_tools": true
    });

    let reply = wire::generate(session, model, prompt, &[], &options)?;
    let videos: Vec<Value> = reply.videos.into_iter().map(|v| {
        json!({
            "url": v.url,
            "thumbnail_url": v.thumbnail_url,
            "file_name": v.file_name,
            "mime_type": v.mime_type,
            "duration": v.duration,
        })
    }).collect();

    Ok(json!({
        "text": reply.text,
        "conversation_id": reply.conversation_id,
        "videos": videos,
        "pending": videos.is_empty(),
    }))
}

fn execute_get_video(session: &mut Session, args: &Value) -> Result<Value, Failure> {
    let conversation_id = args.get("conversation_id").and_then(Value::as_str)
        .ok_or_else(|| fail(400, "conversation_id is required"))?;

    // Read back conversation candidates via batchexecute or StreamGenerate query
    // In Gemini Web, conversation turns are fetched via get_conversation RPC or polling
    let rpc_payload = json!([conversation_id, 10]).to_string();
    let body = session.execute_batch("hSeuEf", &rpc_payload)?;
    let reply = wire::parse_response(&body).unwrap_or_default();

    let videos: Vec<Value> = reply.videos.into_iter().map(|v| {
        json!({
            "url": v.url,
            "thumbnail_url": v.thumbnail_url,
            "file_name": v.file_name,
            "mime_type": v.mime_type,
            "duration": v.duration,
        })
    }).collect();

    Ok(json!({
        "conversation_id": conversation_id,
        "videos": videos,
        "status": if videos.is_empty() { "in_progress" } else { "completed" }
    }))
}

// ---------------------------------------------------------------- Deep Research

fn execute_deep_research(session: &mut Session, args: &Value) -> Result<Value, Failure> {
    let query = args.get("query").and_then(Value::as_str).ok_or_else(|| fail(400, "query is required"))?;
    let model = args.get("model").and_then(Value::as_str).unwrap_or("gemini-4-argon");

    // Phase 1: generate structured plan
    let plan_prompt = format!(
        "You are a deep research specialist. Formulate a structured research plan for: {query}\n\
         Respond with valid JSON: {{\"sub_questions\": [\"...\"], \"outline\": [\"...\"]}}"
    );
    let plan_reply = wire::generate(session, model, &plan_prompt, &[], &json!({}))?;
    let plan_json = extract_json_block(&plan_reply.text);

    // Phase 2: research questions and synthesize
    let synth_prompt = format!(
        "Conduct comprehensive deep research based on this topic: {query}\n\
         Plan: {plan_json}\n\
         Provide an exhaustive, factual report with sections, cited sources, and key conclusions."
    );
    let final_reply = wire::generate(session, model, &synth_prompt, &[], &json!({}))?;

    Ok(json!({
        "query": query,
        "summary": final_reply.text,
        "thinking": final_reply.thinking,
        "model": final_reply.model,
        "conversation_id": final_reply.conversation_id,
    }))
}

fn extract_json_block(text: &str) -> String {
    let trimmed = text.trim();
    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            if end > start {
                return trimmed[start..=end].to_string();
            }
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_json_block() {
        let input = "Here is the plan:\n```json\n{\"sub_questions\": [\"q1\", \"q2\"]}\n```\nHope it helps!";
        assert_eq!(extract_json_block(input), "{\"sub_questions\": [\"q1\", \"q2\"]}");
    }

    #[test]
    fn test_unsupported_operation() {
        let mut session = wire::Session {
            cookie: "".into(),
            auth_user: "".into(),
            token: "".into(),
            push_id: "".into(),
            build_label: "".into(),
            session_id: "".into(),
            language: "".into(),
            generation_id: "".into(),
            models: vec![],
            agent: ureq::Agent::new_with_defaults(),
        };
        assert!(execute(&mut session, "invalid_op", &json!({})).is_err());
    }
}
