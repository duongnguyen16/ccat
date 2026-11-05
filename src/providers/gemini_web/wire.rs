//! Native Gemini Web wire protocol, ported from Argon Local's gemini-web-to-agy bridge.
//! The undocumented Web protocol is not an official Google API.
/*
MIT License
Copyright (c) 2025 Nguyễn Tuấn Thành

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
*/

use crate::common::{fail, Failure};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

pub const ENDPOINT_GOOGLE: &str = "https://www.google.com";
pub const ENDPOINT_INIT: &str = "https://gemini.google.com/app";
pub const ENDPOINT_GENERATE: &str = "https://gemini.google.com/_/BardChatUi/data/assistant.lamda.BardFrontendService/StreamGenerate";
pub const ENDPOINT_ROTATE_COOKIES: &str = "https://accounts.google.com/RotateCookies";
pub const ENDPOINT_UPLOAD: &str = "https://content-push.googleapis.com/upload";
pub const ENDPOINT_BATCH_EXEC: &str = "https://gemini.google.com/_/BardChatUi/data/batchexecute";

pub const HEADER_REQUEST_KEY: &str = "x-goog-ext-525005358-jspb";
pub const HEADER_AUX_1: &str = "x-goog-ext-73010989-jspb";
pub const HEADER_AUX_2: &str = "x-goog-ext-73010990-jspb";

pub const MAX_UPLOAD_RESPONSE_SIZE: u64 = 64 * 1024;
pub const MAX_FILE_BYTES: usize = 100 * 1024 * 1024;
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
pub const UPLOAD_AUTH: &str = "Basic c2F2ZXM6cyNMdGhlNmxzd2F2b0RsN3J1d1U=";

pub const CANVAS_MODEL_SUFFIX: &str = "-canvas";
pub const MUSIC_MODEL_SUFFIX: &str = "-music";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputFile {
    pub name: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UploadedFile {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Image {
    pub url: String,
    pub b64_json: Option<String>,
    pub generated: bool,
    pub title: Option<String>,
    pub alt_text: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub mime_type: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct Video {
    pub url: String,
    pub thumbnail_url: Option<String>,
    pub file_name: Option<String>,
    pub mime_type: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration: Option<f64>,
    pub size_bytes: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Canvas {
    pub id: String,
    pub title: Option<String>,
    pub file_name: Option<String>,
    pub canvas_type: String,
    pub language: Option<String>,
    pub content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct Audio {
    pub url: String,
    pub file_name: Option<String>,
    pub mime_type: String,
    pub duration: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct Reply {
    pub text: String,
    pub thinking: Option<String>,
    pub model: String,
    pub model_id: String,
    pub requested_model: String,
    pub selected_model: String,
    pub selected_model_id: String,
    pub actual_model: Option<String>,
    pub actual_model_id: Option<String>,
    #[serde(skip)]
    pub candidates: Vec<Value>,
    #[serde(skip)]
    pub payloads: Vec<Value>,
    pub conversation_id: Option<String>,
    pub response_id: Option<String>,
    pub choice_id: Option<String>,
    pub metadata: Option<Value>,
    pub images: Vec<Image>,
    pub videos: Vec<Video>,
    pub canvases: Vec<Canvas>,
    pub audios: Vec<Audio>,
    pub raw_response: Option<String>,
}

// Session intentionally does not derive Debug to prevent leaking auth cookies/tokens into logs
pub struct Session {
    pub cookie: String,
    pub auth_user: String,
    pub token: String,
    pub push_id: String,
    pub build_label: String,
    pub session_id: String,
    pub language: String,
    pub generation_id: String,
    pub models: Vec<Model>,
    pub(crate) agent: ureq::Agent,
}

impl Session {
    pub fn agent(&self) -> &ureq::Agent {
        &self.agent
    }

    pub fn models(&self) -> &[Model] {
        &self.models
    }

    pub fn trusted_request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Result<ureq::http::Response<ureq::Body>, Failure> {
        let uri = validate_google_url(url)?;
        let mut builder = ureq::http::Request::builder().method(method).uri(uri)
            .header("Cookie", &self.cookie)
            .header("User-Agent", USER_AGENT);
        for (k, v) in headers {
            if k.eq_ignore_ascii_case("cookie") || k.eq_ignore_ascii_case("x-api-key") {
                return Err(fail(400, "Caller credentials cannot override the Google session"));
            }
            builder = builder.header(*k, *v);
        }
        let result = match body {
            Some(bytes) => self.agent.run(builder.body(bytes).map_err(|_| fail(400, "Invalid Google request headers"))?),
            None => self.agent.run(builder.body(()).map_err(|_| fail(400, "Invalid Google request headers"))?),
        };
        result.map_err(|_| fail(502, "Google Web network request failed"))
    }

    pub fn post_form(&self, url: &str, form_fields: &[(&str, &str)], extra_headers: &[(&str, &str)]) -> Result<String, Failure> {
        let mut encoded = String::new();
        for (i, (k, v)) in form_fields.iter().enumerate() {
            if i > 0 { encoded.push('&'); }
            encoded.push_str(&urlencoding(k));
            encoded.push('=');
            encoded.push_str(&urlencoding(v));
        }
        let mut headers = vec![
            ("Content-Type", "application/x-www-form-urlencoded;charset=utf-8"),
            ("Origin", "https://gemini.google.com"),
            ("Referer", "https://gemini.google.com/"),
            ("X-Same-Domain", "1"),
        ];
        headers.extend_from_slice(extra_headers);
        let mut resp = self.trusted_request("POST", url, &headers, Some(encoded.into_bytes()))?;
        let status = resp.status().as_u16();
        if status < 200 || status >= 300 {
            return Err(fail(status, format!("Google Web form request returned status {status}")));
        }
        resp.body_mut().with_config().limit(MAX_REPLY_BYTES as u64).read_to_string()
            .map_err(|e| fail(502, format!("read Google Web form response: {}", sanitize_error(&e))))
    }

    pub fn execute_batch(&self, rpc_id: &str, req_payload: &str) -> Result<String, Failure> {
        if !safe_id(rpc_id) || req_payload.len() > MAX_REPLY_BYTES { return Err(fail(400, "Invalid Google RPC operation")); }
        let f_req = json!([[[rpc_id, req_payload, Value::Null, "generic"]]]).to_string();
        let query = format!("rpcids={}&hl={}&rt=c&source-path={}&bl={}&f.sid={}", urlencoding(rpc_id),
            urlencoding(&self.language), urlencoding(&gemini_source_path(&self.auth_user)), urlencoding(&self.build_label), urlencoding(&self.session_id));
        let rpc_url = format!("{}?{query}", gemini_account_url(ENDPOINT_BATCH_EXEC, &self.auth_user));
        let header = json!([1,null,null,null,null,null,null,null,[4,5,6,8],null,null,null,null,null,null,null,self.generation_id]).to_string();
        self.post_form(&rpc_url, &[("at", &self.token), ("f.req", &f_req)], &[(MODEL_HEADER, &header), (HEADER_AUX_1, "[0]")])
    }

    pub fn absorb_response_cookies(&mut self, headers: &ureq::http::HeaderMap) -> Result<(), Failure> {
        self.cookie = merge_response_cookies(&self.cookie, headers)?;
        Ok(())
    }

    pub fn upload_file(&self, file: &InputFile) -> Result<UploadedFile, Failure> {
        self.upload_file_to(ENDPOINT_UPLOAD, file)
    }

    pub fn upload_file_to(&self, endpoint: &str, file: &InputFile) -> Result<UploadedFile, Failure> {
        file.validate()?;
        validate_upload_url(endpoint)?;
        let filename = file.name.trim();
        let filename = if filename.is_empty() { "input_file.bin" } else { filename };
        let mime_type = file.mime_type.trim();
        let mime_type = if mime_type.is_empty() { "application/octet-stream" } else { mime_type };

        let push_id = if self.push_id.is_empty() { "feeds/mcudyrk2a4khkz" } else { &self.push_id };
        let size_str = file.data.len().to_string();

        let mut headers = ureq::http::HeaderMap::new();
        for (name, value) in [
            ("accept", "*/*"), ("authorization", UPLOAD_AUTH),
            ("content-type", "application/x-www-form-urlencoded;charset=UTF-8"),
            ("origin", "https://gemini.google.com"), ("referer", "https://gemini.google.com/"),
            ("push-id", push_id), ("x-goog-upload-command", "start"),
            ("x-goog-upload-header-content-length", size_str.as_str()),
            ("x-goog-upload-protocol", "resumable"), ("x-tenant-id", "bard-storage"),
            ("cookie", self.cookie.as_str()), ("user-agent", USER_AGENT),
        ] {
            headers.insert(name, ureq::http::HeaderValue::from_str(value)
                .map_err(|_| fail(400, "Invalid Google upload header"))?);
        }
        let send = |method: &str, url: &str, headers: &ureq::http::HeaderMap, body: Option<&[u8]>| -> Result<ureq::http::Response<ureq::Body>, Failure> {
            validate_upload_url(url)?;
            let mut request = ureq::http::Request::builder().method(method).uri(url);
            *request.headers_mut().ok_or_else(malformed)? = headers.clone();
            let result = match body {
                Some(body) => self.agent.run(request.body(body).map_err(|_| malformed())?),
                None => self.agent.run(request.body(()).map_err(|_| malformed())?),
            }.map_err(|_| fail(502, "Google upload transport failed"))?;
            let status = result.status().as_u16();
            if !(200..300).contains(&status) { return Err(fail(status, "Google upload stage failed")); }
            Ok(result)
        };
        drop(send("OPTIONS", endpoint, &headers, None)?);
        headers.insert("size", ureq::http::HeaderValue::from_str(&size_str).map_err(|_| malformed())?);
        let start_body = format!("File name: {filename}");
        let start = send("POST", endpoint, &headers, Some(start_body.as_bytes()))?;
        let upload_url = start.headers().get("x-goog-upload-url").and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty()).ok_or_else(|| fail(502, "Gemini did not return an upload URL"))?.to_string();
        drop(start);
        drop(send("OPTIONS", &upload_url, &headers, None)?);
        headers.insert("content-type", ureq::http::HeaderValue::from_str(mime_type).map_err(|_| malformed())?);
        headers.insert("x-goog-upload-command", ureq::http::HeaderValue::from_static("upload, finalize"));
        headers.insert("x-goog-upload-offset", ureq::http::HeaderValue::from_static("0"));
        let mut final_resp = send("POST", &upload_url, &headers, Some(&file.data))?;

        let id = final_resp
            .body_mut()
            .with_config()
            .limit(MAX_UPLOAD_RESPONSE_SIZE)
            .read_to_string()
            .map_err(|e| fail(502, format!("read upload response: {}", sanitize_error(&e))))?;

        let id = id.trim().to_string();
        if id.is_empty() {
            return Err(fail(502, "upload returned empty file id"));
        }

        Ok(UploadedFile {
            id,
            name: filename.to_string(),
        })
    }
}

pub fn gemini_account_url(endpoint: &str, auth_user: &str) -> String {
    let auth_user = auth_user.trim();
    if auth_user.is_empty() {
        return endpoint.to_string();
    }
    endpoint.replacen("https://gemini.google.com/", &format!("https://gemini.google.com/u/{auth_user}/"), 1)
}

pub fn gemini_source_path(auth_user: &str) -> String {
    let auth_user = auth_user.trim();
    if auth_user.is_empty() {
        "/app".to_string()
    } else {
        format!("/u/{auth_user}/app")
    }
}

pub fn clean_cookie(val: &str) -> String {
    val.trim().trim_matches('"').trim_matches('\'').trim_end_matches(';').trim().to_string()
}

pub fn merge_cookies(headers: &[&str]) -> String {
    let mut order = Vec::new();
    let mut map = HashMap::new();

    for h in headers {
        for part in h.split(';') {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            if let Some((name, value)) = p.split_once('=') {
                let name = name.trim();
                let value = value.trim();
                if name.is_empty() {
                    continue;
                }
                if !map.contains_key(name) {
                    order.push(name.to_string());
                }
                map.insert(name.to_string(), value.to_string());
            }
        }
    }

    order.into_iter().map(|n| format!("{n}={}", map.get(&n).unwrap())).collect::<Vec<_>>().join("; ")
}

pub fn is_trusted_google_host(host: &str) -> bool {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    h == "google.com"
        || h.ends_with(".google.com")
        || h == "googleusercontent.com"
        || h.ends_with(".googleusercontent.com")
        || h == "googleapis.com"
        || h.ends_with(".googleapis.com")
}

pub fn is_trusted_google_media_host(host: &str) -> bool {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    h == "google.com"
        || h.ends_with(".google.com")
        || h == "googleusercontent.com"
        || h.ends_with(".googleusercontent.com")
}

pub fn validate_upload_url(raw_url: &str) -> Result<(), Failure> {
    let uri = validate_google_url(raw_url)?;
    if uri.host() != Some("content-push.googleapis.com") {
        return Err(fail(400, "Gemini returned an untrusted upload host"));
    }
    Ok(())
}

fn sanitize_error(_err: &impl std::fmt::Display) -> &'static str {
    // Transport Display strings can contain signed URLs, headers or session tokens.
    "network or response transport failure"
}

pub fn build_model_headers(model: &Model, request_id: &str, generation_id: &str) -> Result<ureq::http::HeaderMap, Failure> {
    model_headers(model, request_id, generation_id, false)
}

fn model_headers(
    model: &Model,
    request_id: &str,
    generation_id: &str,
    has_web_tools: bool,
) -> Result<ureq::http::HeaderMap, Failure> {
    if model.model_id.is_empty() || model.model_number == 0 || model.capacity == 0 || (model.capacity_field != 12 && model.capacity_field != 13) {
        return Err(fail(400, format!("model '{}' has incomplete Gemini routing information", model.id)));
    }

    let offset = model.capacity_field - 12;
    let mut header = vec![Value::Null; 17 + offset];
    header[0] = json!(1);
    header[4] = json!(model.model_id);
    header[7] = json!(0);
    if has_web_tools {
        header[8] = json!([4, 5, 6, 8, 16]);
    } else {
        header[8] = json!([4, 5, 6, 8]);
    }
    header[model.capacity_field - 1] = json!(model.capacity);
    header[14 + offset] = json!(model.model_number);
    header[15 + offset] = json!(1);
    header[16 + offset] = json!(generation_id);

    let header_json = serde_json::to_string(&header).map_err(|e| fail(500, e.to_string()))?;
    let req_header_json = serde_json::to_string(&json!([request_id, 1])).map_err(|e| fail(500, e.to_string()))?;

    let mut headers = ureq::http::HeaderMap::new();
    for (name, value) in [(MODEL_HEADER, header_json), (HEADER_REQUEST_KEY, req_header_json),
        (HEADER_AUX_1, "[0]".into()), (HEADER_AUX_2, "[0,0,0]".into())] {
        headers.insert(name, ureq::http::HeaderValue::from_str(&value).map_err(|_| fail(400, "Invalid Google routing metadata"))?);
    }
    Ok(headers)
}

pub fn build_generate_inner(
    prompt: &str,
    files: &[UploadedFile],
    model_number: u32,
    language: &str,
    request_id: &str,
    options: &Value,
) -> Result<Vec<Value>, Failure> {
    let is_temporary = options.get("temporary").and_then(Value::as_bool).unwrap_or(false);
    let metadata = match options.get("metadata") {
        None | Some(Value::Null) => None,
        Some(Value::Array(values)) if values.len() <= 10 && within_json_limit(values, 65_536) => Some(values),
        Some(Value::Array(_)) => return Err(fail(413, "Google conversation metadata exceeds the size limit")),
        Some(_) => return Err(fail(400, "Google conversation metadata must be an array")),
    };
    let message_content = if files.is_empty() {
        vec![json!(prompt)]
    } else {
        let file_data: Vec<Value> = files.iter().map(|f| json!([[f.id], f.name])).collect();
        vec![json!(prompt), json!(0), Value::Null, json!(file_data), Value::Null, Value::Null, json!(0)]
    };

    let default_metadata = json!(["", "", "", null, null, null, null, null, null, ""]);

    let mut inner = vec![Value::Null; 81];
    inner[0] = json!(message_content);
    inner[1] = json!([language]);
    inner[2] = match metadata {
        Some(m) => json!(m),
        None => default_metadata,
    };
    inner[6] = json!([1]);
    inner[7] = json!(1);
    inner[10] = json!(1);
    inner[11] = json!(0);
    inner[17] = json!([[0]]);
    inner[18] = json!(0);
    inner[27] = json!(1);
    inner[30] = json!([4]);
    inner[41] = json!([1]);
    inner[53] = json!(0);
    inner[59] = json!(request_id);
    inner[61] = json!([]);
    inner[68] = json!(1);
    inner[79] = json!(model_number);
    inner[80] = json!(1);

    if is_temporary { inner[45] = json!(1); }
    if let Some(overrides) = options.get("inner_overrides") {
        let fields = overrides.as_object().ok_or_else(|| fail(400, "Google mode overrides must be an object"))?;
        if fields.len() > 16 || !within_json_limit(overrides, 65_536) { return Err(fail(400, "Google mode overrides exceed the size limit")); }
        for (field, value) in fields {
            let field: usize = field.parse().map_err(|_| fail(400, "Invalid Google mode field"))?;
            if ![30,45,49,55,67,68,91,96,98].contains(&field) {
                return Err(fail(400, "Google mode cannot override prompt, conversation or routing-owned fields"));
            }
            if inner.len() <= field { inner.resize(field + 1, Value::Null); }
            inner[field] = value.clone();
        }
    }
    if let Some(options) = options.get("message_options") {
        if !options.as_array().is_some_and(|a| a.len() == 7) || !within_json_limit(options, 16_384) {
            return Err(fail(400, "Invalid Google media message options"));
        }
        let message = inner[0].as_array_mut().ok_or_else(malformed)?;
        message.resize(10, Value::Null);
        message[1] = json!(0);
        message[6] = json!(0);
        message[9] = options.clone();
    }
    if !within_json_limit(&inner, 8 * 1024 * 1024) { return Err(fail(413, "Google generation payload exceeds the size limit")); }
    Ok(inner)
}

pub fn extract_thinking_and_text(raw: &str) -> (Option<String>, String) {
    let start_markers = ["<ctrl94>thought", "<ctrl94>"];
    let mut start_idx = None;
    let mut start_len = 0;

    for marker in &start_markers {
        if let Some(idx) = raw.find(marker) {
            start_idx = Some(idx);
            start_len = marker.len();
            break;
        }
    }

    let start_idx = match start_idx {
        Some(idx) => idx,
        None => return (None, raw.to_string()),
    };

    let end_marker = "<ctrl95>";
    let remainder = &raw[start_idx + start_len..];

    if let Some(rel_end) = remainder.find(end_marker) {
        let end_idx = start_idx + start_len + rel_end;
        let thinking = raw[start_idx + start_len..end_idx].trim().to_string();
        let before = &raw[..start_idx];
        let after = &raw[end_idx + end_marker.len()..];
        let text = format!("{before}{after}").trim().to_string();
        (if thinking.is_empty() { None } else { Some(thinking) }, text)
    } else {
        let thinking = remainder.trim().to_string();
        let before = raw[..start_idx].trim().to_string();
        (if thinking.is_empty() { None } else { Some(thinking) }, before)
    }
}

pub fn extract_bard_error(item: &[Value]) -> Option<String> {
    let mut codes = Vec::new();
    let mut found = false;

    fn walk(val: &Value, temp_codes: &mut Vec<i64>, found: &mut bool) {
        match val {
            Value::Array(arr) => {
                for child in arr {
                    walk(child, temp_codes, found);
                }
            }
            Value::String(s) => {
                if s == "type.googleapis.com/assistant.boq.bard.application.BardErrorInfo" {
                    *found = true;
                }
            }
            Value::Number(n) => {
                if let Some(num) = n.as_i64() {
                    temp_codes.push(num);
                }
            }
            _ => {}
        }
    }

    if let Some(el) = item.get(5) {
        let mut temp_codes = Vec::new();
        let mut cur_found = false;
        walk(el, &mut temp_codes, &mut cur_found);
        if cur_found {
            found = true;
            codes = temp_codes;
        }
    }

    if found {
        if !codes.is_empty() {
            let code_strs: Vec<String> = codes.into_iter().map(|c| c.to_string()).collect();
            Some(format!(
                "Google Gemini Web returned an error (BardErrorInfo code [{}]). This usually indicates session expiration, rate limits, context window limits, or bot protection/CAPTCHA block.",
                code_strs.join(" ")
            ))
        } else {
            Some("Google Gemini Web returned a BardErrorInfo block.".to_string())
        }
    } else {
        None
    }
}

pub fn normalize_image_url(raw_url: &str) -> String {
    let raw = raw_url.trim().trim_end_matches(&['.', ',', ';', ')', ']'][..]);
    let url = if raw.starts_with("//") { format!("https:{raw}") } else { raw.to_string() };
    let Ok(uri) = validate_google_url(&url) else { return String::new(); };
    if !is_trusted_google_media_host(uri.host().unwrap_or("")) || uri.path().ends_with(".svg") {
        return String::new();
    }
    url
}

pub fn extract_generated_images(candidate: &[Value]) -> Vec<Image> {
    if candidate.len() <= 12 {
        return Vec::new();
    }
    let candidate_media = match candidate[12].as_array() {
        Some(a) if a.len() > 7 => a,
        _ => return Vec::new(),
    };
    let media_groups = match candidate_media[7].as_array() {
        Some(a) if !a.is_empty() => a,
        _ => return Vec::new(),
    };
    let generated = match media_groups[0].as_array() {
        Some(a) => a,
        _ => return Vec::new(),
    };

    let mut images = Vec::new();
    for raw_img in generated {
        let img_node = match raw_img.as_array() {
            Some(a) if !a.is_empty() => a,
            _ => continue,
        };
        let wrapper = match img_node[0].as_array() {
            Some(a) if a.len() > 3 => a,
            _ => continue,
        };
        let metadata = match wrapper[3].as_array() {
            Some(a) if a.len() > 3 => a,
            _ => continue,
        };

        let img_url = match metadata[3].as_str() {
            Some(u) => normalize_image_url(u),
            None => continue,
        };
        if img_url.is_empty() {
            continue;
        }

        let mut title = None;
        if let Some(t) = metadata.get(2).and_then(|v| v.as_str()) {
            title = Some(t.to_string());
        }

        let mut mime_type = "image/png".to_string();
        let mut width = None;
        let mut height = None;

        for field in metadata {
            match field {
                Value::String(s) if s.starts_with("image/") => {
                    mime_type = s.clone();
                }
                Value::Array(arr) if arr.len() >= 2 => {
                    if let (Some(w), Some(h)) = (arr[0].as_f64(), arr[1].as_f64()) {
                        width = Some(w as u32);
                        height = Some(h as u32);
                    }
                }
                _ => {}
            }
        }

        images.push(Image {
            url: img_url,
            b64_json: None,
            generated: true,
            title,
            alt_text: None,
            width,
            height,
            mime_type,
        });
    }

    images
}

pub fn extract_canvases(candidate: &[Value]) -> Vec<Canvas> {
    if candidate.len() <= 30 {
        return Vec::new();
    }
    let entries = match candidate[30].as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };

    let mut canvases = Vec::new();
    for raw in entries {
        let entry = match raw.as_array() {
            Some(e) if e.len() >= 11 => e,
            _ => continue,
        };

        let content = match entry[4].as_str() {
            Some(c) if !c.is_empty() => c,
            _ => continue,
        };

        let id = entry.get(0).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let title = entry.get(2).and_then(|v| v.as_str()).map(|s| s.to_string());
        let file_name = entry.get(9).and_then(|v| v.as_str()).map(|s| s.to_string());

        let mut canvas_type = "document".to_string();
        if let Some(kind) = entry.get(10).and_then(|v| v.as_f64()) {
            if kind == 2.0 {
                canvas_type = "code".to_string();
            }
        }

        let mut language = None;
        if entry.len() > 14 {
            if let Some(details) = entry[14].as_array() {
                for &idx in &[1, 4] {
                    if let Some(lang) = details.get(idx).and_then(|v| v.as_str()) {
                        if !lang.is_empty() {
                            language = Some(lang.to_string());
                            break;
                        }
                    }
                }
            }
        }

        canvases.push(Canvas {
            id,
            title,
            file_name,
            canvas_type,
            language,
            content: content.to_string(),
        });
    }

    canvases
}

pub fn parse_response(text: &str) -> Result<Reply, Failure> {
    let mut reply = Reply::default();
    let mut images: BTreeMap<String, Image> = BTreeMap::new();
    for frame in parse_frames(text)? {
        let mut rows = Vec::new();
        records(&frame, None, &mut rows);
        for row in rows {
            let item = row.as_array().ok_or_else(malformed)?;
            if row[0] == "er" {
                let code = row[2].as_u64().unwrap_or(0);
                return Err(fail(if matches!(code, 7 | 16) { 401 } else if code == 8 { 429 } else { 502 }, "Google Web rejected the request"));
            }
            if let Some(error) = extract_bard_error(item) {
                // Unknown Bard codes are not proof that the user's quota is exhausted.
                return Err(fail(502, error));
            }
            if !row[5].is_null() { return Err(fail(502, "Google Web returned an error frame")); }
            let body = payload(row)?;
            if !body.is_array() { return Err(malformed()); }
            if let Some(id) = body[1].as_str().or_else(|| body[1][0].as_str()) {
                reply.conversation_id = Some(id.to_string());
            }
            if let Some(id) = body[1][1].as_str() { reply.response_id = Some(id.to_string()); }
            if let Some(text) = body[2]["7"][0].as_str().filter(|s| !s.is_empty()) {
                reply.text = text.to_string();
            }
            if let Some(candidates) = body[4].as_array().filter(|v| !v.is_empty()) {
                reply.candidates = candidates.clone();
                for candidate in candidates.iter().filter_map(Value::as_array) {
                    for image in extract_generated_images(candidate) { images.insert(image.url.clone(), image); }
                }
                if let Some(first) = candidates.first() {
                    let canvases = first.as_array().map(|v| extract_canvases(v)).unwrap_or_default();
                    if !canvases.is_empty() { reply.canvases = canvases; }
                    reply.choice_id = first[0].as_str().map(str::to_string);
                    if let Some(text) = first[1][0].as_str().filter(|s| !s.is_empty()) { reply.text = text.to_string(); }
                }
            }
            // Preserve parsed native frames for media/research without serializing them to clients.
            reply.payloads.push(body);
        }
    }
    reply.images = images.into_values().collect();
    let (thinking, text) = extract_thinking_and_text(&reply.text);
    reply.thinking = thinking;
    reply.text = text;
    if reply.text.is_empty() && reply.thinking.is_none() && reply.images.is_empty() && reply.canvases.is_empty() {
        return Err(malformed());
    }
    reply.metadata = Some(json!({"cid":reply.conversation_id,"rid":reply.response_id,"rcid":reply.choice_id}));
    Ok(reply)
}

pub fn initialize(cookie: &str, auth_user: &str) -> Result<Session, Failure> {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0)
        .timeout_connect(Some(Duration::from_secs(15))).timeout_global(Some(Duration::from_secs(60))).build().into();
    let mut session = initialize_with_agent(cookie, auth_user, agent)?;
    session.agent = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0)
        .timeout_connect(Some(Duration::from_secs(15))).timeout_global(Some(Duration::from_secs(300))).build().into();
    Ok(session)
}

fn initialize_with_agent(cookie: &str, auth_user: &str, agent: ureq::Agent) -> Result<Session, Failure> {
    let mut clean_c = normalize_cookie(cookie)?;
    if !auth_user.is_empty() && (auth_user.len() > 4 || !auth_user.bytes().all(|b| b.is_ascii_digit())) {
        return Err(fail(400, "Invalid Google account selector"));
    }

    // Reproduce the browser bootstrap, retaining rollout cookies as in the source.
    if let Ok(resp) = agent.get(ENDPOINT_GOOGLE).header("User-Agent", USER_AGENT).call() {
        clean_c = merge_response_cookies(&clean_c, resp.headers())?;
    }
    if let Ok((_, cookies)) = google_get(&agent, &clean_c, "https://gemini.google.com/?hl=en") {
        clean_c = cookies;
    }
    let init_url = format!("{}?hl=en", gemini_account_url(ENDPOINT_INIT, auth_user));
    let read_init = |cookies: &str| -> Result<(String, String), Failure> {
        let (mut response, cookies) = google_get(&agent, cookies, &init_url)?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) { return Err(fail(status, "Gemini Web session initialization failed")); }
        let html = response.body_mut().with_config().limit(MAX_MODEL_BYTES as u64).read_to_string()
            .map_err(|_| fail(502, "Cannot read Google session initialization"))?;
        Ok((html, cookies))
    };
    let initial = read_init(&clean_c);
    let needs_refresh = match &initial {
        Ok((html, _)) => html_field(html, "SNlM0e").is_none(),
        Err(error) => matches!(error.status, 401 | 403),
    };
    let (body, cookies) = if needs_refresh {
        let base = initial.as_ref().map(|(_, cookies)| cookies.as_str()).unwrap_or(&clean_c);
        match rotate_cookie(&agent, base) {
            Ok(rotated) => read_init(&rotated)?,
            Err(_) => initial?,
        }
    } else { initial? };
    clean_c = cookies;

    let token = html_field(&body, "SNlM0e")
        .ok_or_else(|| {
            if body.contains("Sign in") || body.contains("login") {
                fail(401, "authentication failed: cookies are invalid or expired; log in to Gemini Web again")
            } else {
                fail(401, "authentication failed: SNlM0e session token not found")
            }
        })?;

    let push_id = html_field(&body, "qKIAYe").unwrap_or_else(|| "feeds/mcudyrk2a4khkz".to_string());
    let build_label = html_field(&body, "cfb2h").unwrap_or_default();
    let session_id = html_field(&body, "FdrFJe").unwrap_or_default();
    let language = html_field(&body, "TuX5cc").unwrap_or_else(|| "en".to_string());
    let generation_id = crate::common::uuid().to_ascii_uppercase();

    let mut query = format!("rpcids={STATUS_RPC}&hl={}&rt=c&source-path={}", urlencoding(&language), urlencoding(&gemini_source_path(auth_user)));
    if !build_label.is_empty() { query.push_str(&format!("&bl={}", urlencoding(&build_label))); }
    if !session_id.is_empty() { query.push_str(&format!("&f.sid={}", urlencoding(&session_id))); }
    let rpc_url = format!("{}?{query}", gemini_account_url(ENDPOINT_BATCH_EXEC, auth_user));
    let form_body = format!("at={}&f.req={}", urlencoding(&token), urlencoding(r#"[[["otAQ7b","[]",null,"generic"]]]"#));

    let mut model_batch_header = vec![Value::Null; 17];
    model_batch_header[0] = json!(1);
    model_batch_header[8] = json!([4, 5, 6, 8]);
    model_batch_header[16] = json!(generation_id);
    let model_batch_json = serde_json::to_string(&model_batch_header).unwrap_or_default();

    let mut rpc_resp = agent
        .post(&rpc_url)
        .header("Content-Type", "application/x-www-form-urlencoded;charset=utf-8")
        .header("Origin", "https://gemini.google.com")
        .header("Referer", "https://gemini.google.com/")
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .header("X-Same-Domain", "1")
        .header("Cookie", &clean_c)
        .header(MODEL_HEADER, &model_batch_json)
        .header(HEADER_AUX_1, "[0]")
        .send(form_body.into_bytes())
        .map_err(|e| fail(502, format!("model discovery RPC failed: {}", sanitize_error(&e))))?;

    let rpc_status = rpc_resp.status().as_u16();
    if rpc_status < 200 || rpc_status >= 300 {
        return Err(fail(rpc_status, format!("model discovery RPC failed with status {rpc_status}")));
    }

    let rpc_body = rpc_resp
        .body_mut()
        .with_config()
        .limit(MAX_MODEL_BYTES as u64)
        .read_to_string()
        .map_err(|e| fail(502, format!("read model discovery: {}", sanitize_error(&e))))?;

    clean_c = merge_response_cookies(&clean_c, rpc_resp.headers())?;
    let models = parse_models(&rpc_body)?;

    Ok(Session {
        cookie: clean_c,
        auth_user: auth_user.to_string(),
        token,
        push_id,
        build_label,
        session_id,
        language,
        generation_id,
        models,
        agent,
    })
}

fn urlencoding(val: &str) -> String {
    let mut out = String::new();
    for b in val.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn merge_response_cookies(base: &str, headers: &ureq::http::HeaderMap) -> Result<String, Failure> {
    let updates: Vec<_> = headers.get_all("set-cookie").iter().filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next()).collect();
    let mut all = vec![base];
    all.extend(updates);
    normalize_cookie(&merge_cookies(&all))
}

fn google_get(agent: &ureq::Agent, cookie: &str, url: &str) -> Result<(ureq::http::Response<ureq::Body>, String), Failure> {
    let mut cookie = cookie.to_string();
    let mut url = url.to_string();
    for _ in 0..6 {
        let uri = validate_google_url(&url)?;
        let response = agent.get(&url).header("Cookie", &cookie).header("User-Agent", USER_AGENT)
            .header("Accept", "text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.8")
            .header("Accept-Language", "en-US,en;q=0.9").header("Referer", "https://gemini.google.com/")
            .header("X-Same-Domain", "1").call().map_err(|_| fail(502, "Cannot reach Google Web"))?;
        cookie = merge_response_cookies(&cookie, response.headers())?;
        if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) { return Ok((response, cookie)); }
        let location = response.headers().get("location").and_then(|v| v.to_str().ok())
            .ok_or_else(|| fail(502, "Google Web redirect omitted its location"))?;
        url = if location.starts_with("//") { format!("https:{location}") }
            else if location.starts_with('/') { format!("https://{}{location}", uri.authority().ok_or_else(malformed)?) }
            else { location.to_string() };
        validate_google_url(&url)?;
    }
    Err(fail(502, "Too many Google Web redirects"))
}

fn rotate_cookie(agent: &ureq::Agent, cookie: &str) -> Result<String, Failure> {
    let auth = cookie.split("; ").filter(|p| p.starts_with("__Secure-1PSID=") || p.starts_with("__Secure-1PSIDTS="))
        .collect::<Vec<_>>().join("; ");
    let request = ureq::http::Request::post(ENDPOINT_ROTATE_COOKIES).header("Cookie", auth).header("User-Agent", USER_AGENT)
        .header("Content-Type", "application/json").body(r#"[000,"-0000000000000000000"]"#).map_err(|_| malformed())?;
    let request = agent.configure_request(request).timeout_global(Some(Duration::from_secs(5))).build();
    let response = agent.run(request).map_err(|_| fail(502, "Google cookie refresh could not connect"))?;
    if response.status().as_u16() != 200 { return Err(fail(401, "Google session refresh requires sign-in")); }
    merge_response_cookies(cookie, response.headers())
}

pub fn generate(
    session: &mut Session,
    model: &str,
    prompt: &str,
    files: &[InputFile],
    options: &serde_json::Value,
) -> Result<Reply, Failure> {
    if prompt.is_empty() || prompt.len() > 8 * 1024 * 1024 || files.len() > 16
        || files.iter().try_fold(0usize, |n, f| n.checked_add(f.data.len())).is_none_or(|n| n > MAX_FILE_BYTES) {
        return Err(fail(413, "Google prompt or attachments exceed the supported size limit"));
    }
    let base_model = model.strip_suffix(CANVAS_MODEL_SUFFIX).unwrap_or(model);
    let is_canvas = model.ends_with(CANVAS_MODEL_SUFFIX) || options.get("canvas").and_then(|v| v.as_bool()).unwrap_or(false);

    let selected = resolve_model(base_model, &session.models)?.clone();

    // Check metadata/mode/effective payload bounds before any attachment upload.
    build_generate_inner(prompt, &[], selected.model_number, &session.language, "preflight", options)?;
    let mut uploaded = Vec::new();
    for f in files {
        let up = session.upload_file(f)?;
        uploaded.push(up);
    }

    let request_id = crate::common::uuid().to_ascii_uppercase();
    let lang = if session.language.is_empty() { "en" } else { &session.language };

    let mut inner = build_generate_inner(prompt, &uploaded, selected.model_number, lang, &request_id, options)?;

    if is_canvas {
        if inner.len() < 99 {
            inner.resize(99, Value::Null);
        }
        inner[30] = json!([4, 16]);
        inner[49] = json!(2);
        inner[67] = json!(0);
        inner[68] = json!(2);
        inner[91] = json!(0);
        inner[96] = json!(0);
        inner[98] = json!(1);
    }

    let inner_json = serde_json::to_string(&inner).map_err(|e| fail(500, e.to_string()))?;
    let outer = json!([null, inner_json]);
    let outer_json = serde_json::to_string(&outer).map_err(|e| fail(500, e.to_string()))?;

    let form_body = format!("at={}&f.req={}", urlencoding(&session.token), urlencoding(&outer_json));

    let request_number = u32::from_str_radix(&crate::common::random_hex(4), 16).unwrap_or(0) % 90000 + 10000;
    let mut query = format!("at={}&hl={}&_reqid={request_number}&rt=c", urlencoding(&session.token), urlencoding(lang));
    if !session.build_label.is_empty() { query.push_str(&format!("&bl={}", urlencoding(&session.build_label))); }
    if !session.session_id.is_empty() { query.push_str(&format!("&f.sid={}", urlencoding(&session.session_id))); }

    let generate_url = format!("{}?{query}", gemini_account_url(ENDPOINT_GENERATE, &session.auth_user));
    let model_headers = model_headers(&selected, &request_id, &session.generation_id,
        is_canvas || options.get("web_tools").and_then(Value::as_bool).unwrap_or(false))?;

    let mut req_builder = session.agent.post(&generate_url);
    req_builder = req_builder.header("Content-Type", "application/x-www-form-urlencoded;charset=utf-8");
    req_builder = req_builder.header("Origin", "https://gemini.google.com");
    req_builder = req_builder.header("Referer", "https://gemini.google.com/");
    req_builder = req_builder.header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36");
    req_builder = req_builder.header("X-Same-Domain", "1");
    req_builder = req_builder.header("Cookie", &session.cookie);

    for (k, v) in &model_headers {
        req_builder = req_builder.header(k, v);
    }

    let mut resp = req_builder
        .send(form_body.into_bytes())
        .map_err(|e| fail(502, format!("generate request failed: {}", sanitize_error(&e))))?;

    let status = resp.status().as_u16();
    if status < 200 || status >= 300 {
        return Err(fail(status, format!("generate request returned status {status}")));
    }

    let body = resp
        .body_mut()
        .with_config()
        .limit(MAX_REPLY_BYTES as u64)
        .read_to_string()
        .map_err(|e| fail(502, format!("read generate response: {}", sanitize_error(&e))))?;

    session.absorb_response_cookies(resp.headers())?;
    let mut reply = parse_response(&body)?;
    reply.model = selected.id.clone();
    reply.model_id = selected.model_id.clone();
    reply.requested_model = model.to_string();
    reply.selected_model = selected.id.clone();
    reply.selected_model_id = selected.model_id.clone();

    // The supplied source establishes no authoritative response model field.
    // Keep actual identity unknown; echoed or unrelated IDs are not routing proof.

    if is_canvas {
        if !reply.model.ends_with(CANVAS_MODEL_SUFFIX) {
            reply.model.push_str(CANVAS_MODEL_SUFFIX);
        }
        if !reply.requested_model.ends_with(CANVAS_MODEL_SUFFIX) {
            reply.requested_model.push_str(CANVAS_MODEL_SUFFIX);
        }
    }

    Ok(reply)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    pub display_name: String,
    pub model_id: String,
    pub capacity: u32,
    pub capacity_field: usize,
    pub model_number: u32,
    pub aliases: Vec<String>,
}

const MAX_MODEL_BYTES: usize = 8 << 20;
const MAX_REPLY_BYTES: usize = 32 << 20;
const STATUS_RPC: &str = "otAQ7b";
pub const MODEL_HEADER: &str = "x-goog-ext-525001261-jspb";
pub const BATCH_PATH: &str = "/_/BardChatUi/data/batchexecute";
pub const GENERATE_PATH: &str = "/_/BardChatUi/data/assistant.lamda.BardFrontendService/StreamGenerate";

fn malformed() -> Failure {
    // Never include upstream text: it can contain prompts, signed URLs and tokens.
    fail(502, "Google Web returned a malformed or empty protocol response")
}

fn within_json_limit(value: &impl Serialize, limit: usize) -> bool {
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.checked_sub(data.len()).ok_or_else(|| std::io::Error::other("JSON size limit"))?;
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    serde_json::to_writer(Budget(limit), value).is_ok()
}

/// Decode values, not lines: batchexecute frames can contain multiline JSON.
pub fn parse_frames(body: &str) -> Result<Vec<Value>, Failure> {
    if body.len() > MAX_REPLY_BYTES {
        return Err(fail(502, "Google Web response exceeded the size limit"));
    }
    let trimmed = body.trim_start();
    let body = trimmed.strip_prefix(")]}'").unwrap_or(trimmed);
    let mut frames = Vec::new();
    for value in serde_json::Deserializer::from_str(body).into_iter::<Value>() {
        let value = value.map_err(|_| malformed())?;
        if let Some(length) = value.as_u64() {
            if length > MAX_REPLY_BYTES as u64 { return Err(malformed()); }
            continue;
        }
        if !value.is_array() || frames.len() >= 4096 { return Err(malformed()); }
        frames.push(value);
    }
    if frames.is_empty() { return Err(malformed()); }
    Ok(frames)
}

fn records<'a>(value: &'a Value, rpc: Option<&str>, out: &mut Vec<&'a Value>) {
    let Some(items) = value.as_array() else { return };
    let marker = items.first().and_then(Value::as_str);
    if matches!(marker, Some("wrb.fr" | "er")) ||
        (items.len() >= 3 && items[0].is_null() && items[2].is_string()) {
        if rpc.is_none_or(|rpc| value[1].as_str() == Some(rpc)) { out.push(value); }
        return;
    }
    for child in items { records(child, rpc, out); }
}

fn payload(record: &Value) -> Result<Value, Failure> {
    match &record[2] {
        Value::String(s) => serde_json::from_str::<Value>(s).map_err(|_| malformed()),
        Value::Array(_) => Ok(record[2].clone()),
        _ => Err(malformed()),
    }
}

fn first_string(value: &Value, indices: &[usize]) -> String {
    indices.iter().filter_map(|&i| value[i].as_str()).map(str::trim)
        .find(|s| !s.is_empty()).unwrap_or_default().to_owned()
}

fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

fn account_capacity(tiers: &Value, capabilities: &Value) -> (u32, usize) {
    let has = |value: &Value, flag: u64| value.as_array().is_some_and(|v| v.iter().any(|v| v.as_u64() == Some(flag)));
    if has(tiers, 21) { (1, 13) }
    else if has(tiers, 22) { (2, 13) }
    else if has(capabilities, 115) { (4, 12) }
    else if has(tiers, 16) || has(capabilities, 106) { (3, 12) }
    else if has(tiers, 8) || has(capabilities, 19) { (2, 12) }
    else { (1, 12) }
}

fn slug(value: &str) -> String {
    let mut s = String::new();
    for b in value.trim().bytes() {
        if b.is_ascii_alphanumeric() || b == b'.' { s.push((b as char).to_ascii_lowercase()); }
        else if !s.ends_with('-') && !s.is_empty() { s.push('-'); }
    }
    s.trim_matches('-').to_owned()
}

fn model_names(id: &str, category: &str, display: &str) -> (String, Vec<String>) {
    use std::collections::BTreeSet;
    let prefix = |s: String| if s.starts_with("gemini-") { s } else { format!("gemini-{s}") };
    let mut aliases = BTreeSet::from([id.to_ascii_lowercase()]);
    for value in [category, display].into_iter().filter(|s| !s.is_empty()) {
        aliases.insert(value.to_ascii_lowercase());
        let s = slug(value);
        if !s.is_empty() { aliases.insert(s.clone()); aliases.insert(prefix(s)); }
    }
    if !category.is_empty() {
        if let Some(start) = display.char_indices().find_map(|(i, c)|
            (c.is_ascii_digit() && (i == 0 || !display.as_bytes()[i-1].is_ascii_alphanumeric())).then_some(i)) {
            let version: String = display[start..].chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
            let version = version.trim_end_matches('.');
            if !version.is_empty() {
                aliases.insert(format!("gemini-{version}-{}", slug(category)));
                aliases.insert(format!("gemini-{}-{}", version.split('.').next().unwrap_or(version), slug(category)));
            }
        }
    }
    let source = if !display.is_empty() { display } else if !category.is_empty() { category } else { id };
    let id = prefix(slug(source));
    aliases.insert(id.clone());
    (id, aliases.into_iter().collect())
}

pub fn parse_models(body: &str) -> Result<Vec<Model>, Failure> {
    if body.len() > MAX_MODEL_BYTES { return Err(fail(502, "Google Web model discovery exceeded the size limit")); }
    let frames = parse_frames(body)?;
    let mut found = Vec::new();
    let mut names = std::collections::HashSet::new();
    let mut ids = std::collections::HashSet::new();
    for frame in &frames {
        let mut rows = Vec::new();
        records(frame, Some(STATUS_RPC), &mut rows);
        for row in rows {
            if row[0] == "er" || row[5][0].as_i64().is_some_and(|v| v != 0) {
                return Err(fail(401, "Google Web model discovery rejected the authenticated session"));
            }
            let status = payload(row)?;
            if !status.is_array() { return Err(malformed()); }
            if !status[14].is_null() {
                let code = status[14].as_u64().ok_or_else(malformed)?;
                if code != 1000 { return Err(fail(401, "Google Web account cannot select models; verify Gemini access and sign-in")); }
            }
            let (capacity, capacity_field) = account_capacity(&status[16], &status[17]);
            for fields in status[15].as_array().into_iter().flatten() {
                if !fields.is_array() { continue; }
                let model_id = first_string(fields, &[0]);
                if !safe_id(&model_id) || ids.contains(&model_id) { continue; }
                let raw_number = if fields[17].is_null() { &fields[9] } else { &fields[17] };
                let number = if raw_number.is_null() { Some(1) } else { raw_number.as_u64().filter(|n| *n > 0 && *n <= u32::MAX as u64) };
                let Some(model_number) = number else { continue };
                let category = first_string(fields, &[1, 10]);
                let display_name = first_string(fields, &[11, 19, 1]);
                let (mut id, aliases) = model_names(&model_id, &category, &display_name);
                if names.contains(&id) { id.push('-'); id.push_str(&model_id[..model_id.len().min(8)].to_ascii_lowercase()); }
                if names.contains(&id) { id.push('-'); id.push_str(&model_id.to_ascii_lowercase()); }
                names.insert(id.clone());
                ids.insert(model_id.clone());
                found.push(Model { id, display_name, model_id, capacity, capacity_field, model_number: model_number as u32, aliases });
            }
        }
    }
    if found.is_empty() { return Err(fail(502, "Google Web model discovery returned no selectable models")); }
    Ok(found)
}

pub fn resolve_model<'a>(requested: &str, models: &'a [Model]) -> Result<&'a Model, Failure> {
    let requested = requested.trim().to_ascii_lowercase();
    if requested.is_empty() { return Err(fail(400, "Select an explicit Google Web model")); }
    let requested = if requested == "gemini-advanced" { "gemini-pro" } else { &requested };
    let exact: Vec<_> = models.iter().filter(|m| m.id.eq_ignore_ascii_case(requested) || m.model_id.eq_ignore_ascii_case(requested)).collect();
    let matches = if exact.is_empty() {
        models.iter().filter(|m| m.aliases.iter().any(|s| s.eq_ignore_ascii_case(requested))).collect::<Vec<_>>()
    } else { exact };
    match matches.as_slice() {
        [model] => Ok(model),
        [] => Err(fail(400, "Requested Google Web model is unavailable for this account; refresh the model list")),
        _ => Err(fail(400, "Requested Google Web model alias is ambiguous; use its canonical name or routing ID")),
    }
}

pub fn validate_google_url(url: &str) -> Result<ureq::http::Uri, Failure> {
    let invalid = || fail(400, "Refusing an invalid or untrusted Google Web URL");
    if url.len() > 16_384 || url.bytes().any(|b| b <= b' ' || b == b'\\' || b == b'#') {
        return Err(invalid());
    }
    let uri: ureq::http::Uri = url.parse().map_err(|_| invalid())?;
    let authority = uri.authority().ok_or_else(invalid)?;
    let host = uri.host().ok_or_else(invalid)?;
    if uri.scheme_str() != Some("https") || authority.as_str().contains('@') || host.ends_with('.')
        || uri.port_u16().is_some_and(|p| p != 443) || !is_trusted_google_host(host) {
        return Err(invalid());
    }
    Ok(uri)
}

impl InputFile {
    pub fn validate(&self) -> Result<(), Failure> {
        if self.name.is_empty() || self.name.len() > 255 || matches!(self.name.as_str(), "." | "..")
            || self.name.chars().any(|c| c.is_control() || "<>:\"/\\|?*".contains(c)) {
            return Err(fail(400, "Attachment name must be a safe filename"));
        }
        if self.mime_type.is_empty() || self.mime_type.len() > 128 || !self.mime_type.contains('/')
            || !self.mime_type.is_ascii() || self.mime_type.bytes().any(|b| b < b' ' || b == 127) {
            return Err(fail(400, "Invalid attachment media type"));
        }
        if self.data.is_empty() || self.data.len() > MAX_FILE_BYTES {
            return Err(fail(413, "Attachments must contain between 1 byte and 100 MiB"));
        }
        Ok(())
    }

    pub fn from_base64(name: &str, mime_type: &str, encoded: &str) -> Result<Self, Failure> {
        use base64::{Engine as _, engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD}};
        if encoded.len() > MAX_FILE_BYTES.div_ceil(3) * 4 + 4 {
            return Err(fail(413, "Encoded attachment exceeds 100 MiB"));
        }
        let data = [STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD].iter()
            .find_map(|engine| engine.decode(encoded.trim()).ok())
            .ok_or_else(|| fail(400, "Invalid base64 attachment"))?;
        let file = Self { name: name.to_string(), mime_type: mime_type.to_string(), data };
        file.validate()?;
        Ok(file)
    }
}

fn normalize_cookie(cookie: &str) -> Result<String, Failure> {
    let cookie = clean_cookie(cookie);
    if cookie.len() > 65_536 || !cookie.is_ascii() || cookie.bytes().any(|b| b < b' ' || b == 127) {
        return Err(fail(401, "Invalid Google session cookie"));
    }
    for part in cookie.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (name, _) = part.split_once('=').ok_or_else(|| fail(401, "Invalid Google session cookie"))?;
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)) {
            return Err(fail(401, "Invalid Google session cookie"));
        }
    }
    let merged = merge_cookies(&[&cookie]);
    if !merged.split("; ").any(|p| p.strip_prefix("__Secure-1PSID=").is_some_and(|v| !v.is_empty())) {
        return Err(fail(401, "Google session requires __Secure-1PSID; sign in again"));
    }
    Ok(merged)
}

fn html_field(html: &str, key: &str) -> Option<String> {
    let marker = format!("\"{key}\"");
    for (index, _) in html.match_indices(&marker) {
        let rest = html[index + marker.len()..].trim_start();
        let Some(rest) = rest.strip_prefix(':').or_else(|| rest.strip_prefix(',')) else { continue; };
        if let Some(Ok(Value::String(value))) = serde_json::Deserializer::from_str(rest.trim_start()).into_iter::<Value>().next() {
            if !value.is_empty() { return Some(value); }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_routing_uses_account_metadata_and_81_slot_payload() {
        let models = discovered_models();
        let selected = resolve_model("gemini-pro", &models).unwrap();
        let headers = build_model_headers(selected, "fresh-request", "stable-client-session").unwrap();
        let header: Value = serde_json::from_str(headers[MODEL_HEADER].to_str().unwrap()).unwrap();
        assert_eq!(header, json!([1,null,null,null,"2222222222222222",null,null,0,[4,5,6,8],null,null,2,null,null,3,1,"stable-client-session"]));
        assert_eq!(headers["x-goog-ext-525005358-jspb"], "[\"fresh-request\",1]");
        assert_eq!(headers["x-goog-ext-73010989-jspb"], "[0]");
        assert_eq!(headers["x-goog-ext-73010990-jspb"], "[0,0,0]");
        for temporary in [false, true] {
            let inner = build_generate_inner("Hello", &[], 3, "en", "fresh-request", &json!({"temporary":temporary})).unwrap();
            assert_eq!(inner.len(), 81);
            assert_eq!(inner[0], json!(["Hello"]));
            assert_eq!(inner[1], json!(["en"]));
            assert_eq!(inner[2], json!(["","","",null,null,null,null,null,null,""]));
            assert_eq!(inner[3], Value::Null);
            assert_eq!(inner[45], if temporary { json!(1) } else { Value::Null });
            assert_eq!(inner[59], "fresh-request");
            assert_eq!((inner[79].as_u64(),inner[80].as_u64()), (Some(3),Some(1)));
        }
        let files = [UploadedFile { id:"uploaded-id".into(), name:"image.png".into() }];
        let inner = build_generate_inner("Describe", &files, 3, "vi", "id", &json!({"metadata":["old-c","old-r","old-choice"]})).unwrap();
        assert_eq!(inner[0], json!(["Describe",0,null,[[["uploaded-id"],"image.png"]],null,null,0]));
        assert_eq!(inner[2], json!(["old-c","old-r","old-choice"]));
        let mut alternate = selected.clone();
        alternate.capacity_field = 13;
        let h = build_model_headers(&alternate,"req","session").unwrap();
        assert_eq!(serde_json::from_str::<Value>(h[MODEL_HEADER].to_str().unwrap()).unwrap(),
            json!([1,null,null,null,"2222222222222222",null,null,0,[4,5,6,8],null,null,null,2,null,null,3,1,"session"]));
        alternate.model_id.clear();
        assert!(build_model_headers(&alternate,"req","session").is_err());
        assert!(build_generate_inner("x", &[], 1, "en", "id", &json!({"metadata":"not-an-array"})).is_err());
    }

    #[test]
    fn cookie_bearing_urls_are_https_and_exact_trusted_authorities() {
        for url in ["https://gemini.google.com/app", "https://gemini.google.com:443/app", "https://accounts.google.com/RotateCookies",
            "https://content-push.googleapis.com/upload?upload_id=opaque", "https://lh3.googleusercontent.com/generated-image", "https://usercontent.google.com/download"] {
            assert!(validate_google_url(url).is_ok(), "rejected {url}");
        }
        for url in ["http://gemini.google.com/app", "https://gemini.google.com.evil.test/x", "https://evilgoogle.com/x", "https://googleusercontent.com.evil.test/x",
            "https://evil.test/a.png", "https://user:pass@gemini.google.com/x", "https://gemini.google.com:444/app", "https://gemini.google.com./app",
            "https://gemini.google.com\\@evil.test/x", "https://gemini.google.com/x#fragment", "file:///tmp/input", "//gemini.google.com/app", "https://127.0.0.1/x"] {
            let error = validate_google_url(url).unwrap_err();
            assert!(!error.message.contains(url));
        }
        assert!(validate_upload_url("https://content-push.googleapis.com/session?id=opaque").is_ok());
        for url in ["https://gemini.google.com/session", "https://content-push.googleapis.com.evil.test/session", "https://user@content-push.googleapis.com/session", "http://content-push.googleapis.com/session", "https://content-push.googleapis.com:444/session"] {
            assert!(validate_upload_url(url).is_err());
        }
    }

    #[test]
    fn inline_files_decode_all_reference_base64_encodings_and_reject_bad_inputs() {
        for encoded in ["/wA=", "/wA", "_wA=", "_wA"] {
            let file = InputFile::from_base64("image.png", "image/png", encoded).unwrap();
            assert_eq!(file.data, [255,0]);
            assert_eq!(file.name, "image.png");
            assert_eq!(file.mime_type, "image/png");
        }
        assert!(InputFile::from_base64("x","text/plain","not base64!").is_err());
        for name in ["../secret", "a/b", "a\\b", "line\r\nheader"] {
            assert!(InputFile::from_base64(name,"text/plain","eA==").is_err());
        }
        assert!(InputFile::from_base64("x","image/png\r\nCookie: x","eA==").is_err());
    }

    #[test]
    fn session_bootstrap_fields_decode_json_escapes_and_array_fallback() {
        let html = r#"<script>WIZ_global_data = {"SNlM0e":"token=","qKIAYe":"feeds/session","cfb2h":"build","FdrFJe":"sid","TuX5cc":"vi"};</script>"#;
        assert_eq!(html_field(html,"SNlM0e").as_deref(),Some("token="));
        assert_eq!(html_field(html,"TuX5cc").as_deref(),Some("vi"));
        assert_eq!(html_field(r#"["SNlM0e","fallback-token"]"#,"SNlM0e").as_deref(),Some("fallback-token"));
        assert_eq!(html_field(r#"{"SNlM0e": null}"#,"SNlM0e"),None);
        assert_eq!(normalize_cookie("NID=rollout; __Secure-1PSID=old; SIDCC=account; __Secure-1PSID=new; __Secure-1PSIDTS=fresh").unwrap(),
            "NID=rollout; __Secure-1PSID=new; SIDCC=account; __Secure-1PSIDTS=fresh");
        assert!(normalize_cookie("__Secure-1PSID=secret\r\nX-Evil: yes").is_err());
        assert!(normalize_cookie("").is_err());
    }

    #[test]
    fn response_frames_keep_thinking_and_conversation_ids() {
        let payload = json!([null,["c_fixture","r_fixture"],null,null,[["rc_fixture",["<ctrl94>thoughtPlan<ctrl95>Answer"]]]]);
        let frame = json!([["wrb.fr",null,payload.to_string()]]).to_string();
        let reply = parse_response(&format!(")]}}'\n\n{}\n{frame}\n", frame.len())).unwrap();
        assert_eq!(reply.text, "Answer");
        assert_eq!(reply.thinking.as_deref(), Some("Plan"));
        assert_eq!(reply.conversation_id.as_deref(), Some("c_fixture"));
        assert_eq!(reply.response_id.as_deref(), Some("r_fixture"));
        assert_eq!(reply.choice_id.as_deref(), Some("rc_fixture"));
        let update = json!([["wrb.fr",null,json!([null,["c_job","r_job"],{"7":["Video is being generated"],"44":true}]).to_string()]]).to_string();
        assert_eq!(parse_response(&update).unwrap().text, "Video is being generated");
    }

    #[test]
    fn echoed_or_unrelated_ids_are_not_authoritative_model_evidence() {
        let models = discovered_models();
        let text = json!([["wrb.fr",null,json!([null,null,null,null,[["choice",[models[1].model_id]]]]).to_string()]]).to_string();
        assert!(parse_response(&text).unwrap().actual_model.is_none());
        let unrelated = json!([["wrb.fr",null,json!([[models[1].model_id],null,null,null,[["choice",["Answer"]]]]).to_string()]]).to_string();
        assert!(parse_response(&unrelated).unwrap().actual_model.is_none());
        let synthetic = json!([["wrb.fr",null,json!([{"model_id":models[1].model_id},null,null,null,[["choice",["Answer"]]]]).to_string()]]).to_string();
        assert!(parse_response(&synthetic).unwrap().actual_model.is_none());
        assert!(initialize("invalid-cookie", "").is_err());
        assert!(initialize("__Secure-1PSID=fixture", "../wrong").is_err());
    }

    #[test]
    fn modes_cannot_override_routing_or_bypass_payload_limits() {
        for field in [0,1,2,59,79,80] {
            assert!(build_generate_inner("x", &[], 3, "en", "id", &json!({"inner_overrides":{field.to_string():1}})).is_err());
        }
        assert!(build_generate_inner("x", &[], 3, "en", "id", &json!({"metadata":vec![Value::Null;11]})).is_err());
        assert!(build_generate_inner("x", &[], 3, "en", "id", &json!({"metadata":["x".repeat(65_537)]})).is_err());
        let inner = build_generate_inner("x", &[], 3, "en", "id", &json!({"inner_overrides":{"49":2,"98":1},"message_options":[null,null,null,null,null,null,[[null,null,null,2]]]})).unwrap();
        assert_eq!(inner[79], 3);
        assert_eq!(inner[49], 2);
        assert_eq!(inner[0][0], "x");
        assert_eq!(inner[0][9][6][0][3], 2);
    }

    #[test]
    fn authenticated_init_retries_once_after_cookie_rotation_offline() {
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(Vec::new()));
        let history = seen.clone();
        let models = model_fixture(json!(1000), json!([8]), Value::Null, vec![model_entry("id-pro", "Pro", "Gemini 3.1 Pro", json!(3))]);
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0)
            .middleware(move |request: ureq::http::Request<ureq::SendBody>, _next: ureq::middleware::MiddlewareNext| {
                let path = request.uri().path().to_string();
                let mut history = history.lock().unwrap();
                let first_app = !history.iter().any(|p| p == "/app");
                history.push(path.clone());
                let mut response = ureq::http::Response::builder().status(200);
                let body = if path == "/app" && first_app {
                    response = response.status(401);
                    String::new()
                } else if path == "/app" {
                    r#"{"SNlM0e":"fixture-token","FdrFJe":"fixture-sid","cfb2h":"build","TuX5cc":"en"}"#.into()
                } else if path.ends_with("batchexecute") { models.clone() }
                else if path.ends_with("RotateCookies") {
                    response = response.header("Set-Cookie", "__Secure-1PSIDTS=refreshed; Path=/; Secure");
                    String::new()
                } else { String::new() };
                Ok(response.body(ureq::Body::builder().data(body)).unwrap())
            }).build().into();
        let session = initialize_with_agent("__Secure-1PSID=fixture; __Secure-1PSIDTS=old", "", agent).unwrap();
        assert!(session.cookie.contains("__Secure-1PSIDTS=refreshed"));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.iter().filter(|p| p.as_str() == "/app").count(), 2);
        assert_eq!(seen.iter().filter(|p| p.as_str() == "/RotateCookies").count(), 1);
    }

    #[test]
    fn upload_keeps_source_headers_in_all_four_offline_stages() {
        use std::{io::Read, sync::{Arc, Mutex}};
        let seen = Arc::new(Mutex::new(Vec::new()));
        let capture = seen.clone();
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0)
            .middleware(move |request: ureq::http::Request<ureq::SendBody>, _next: ureq::middleware::MiddlewareNext| {
                let (parts, body) = request.into_parts();
                let mut bytes = Vec::new();
                body.into_reader().read_to_end(&mut bytes).unwrap();
                let mut capture = capture.lock().unwrap();
                capture.push((parts.method, parts.headers, bytes));
                let mut response = ureq::http::Response::builder().status(200);
                if capture.len() == 2 { response = response.header("X-Goog-Upload-Url", "https://content-push.googleapis.com/session?opaque=fixture"); }
                Ok(response.body(ureq::Body::builder().data(if capture.len() == 4 { "file-id" } else { "" })).unwrap())
            }).build().into();
        let session = Session { cookie:"__Secure-1PSID=fixture".into(), auth_user:String::new(), token:String::new(), push_id:"feeds/fixture".into(),
            build_label:String::new(), session_id:String::new(), language:"en".into(), generation_id:"uuid".into(), models:Vec::new(), agent };
        let file = InputFile { name:"image.png".into(), mime_type:"image/png".into(), data:vec![1,2,3] };
        assert_eq!(session.upload_file(&file).unwrap().id, "file-id");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.iter().map(|s| s.0.as_str()).collect::<Vec<_>>(), ["OPTIONS","POST","OPTIONS","POST"]);
        for (_, headers, _) in seen.iter() {
            assert_eq!(headers["push-id"], "feeds/fixture");
            assert_eq!(headers["authorization"], UPLOAD_AUTH);
            assert_eq!(headers["x-tenant-id"], "bard-storage");
            assert_eq!(headers["x-goog-upload-protocol"], "resumable");
            assert_eq!(headers["x-goog-upload-header-content-length"], "3");
            assert_eq!(headers["user-agent"], USER_AGENT);
        }
        assert_eq!(seen[3].1["x-goog-upload-command"], "upload, finalize");
        assert_eq!(seen[3].2, [1,2,3]);
    }

    #[test]
    fn an_answer_may_explain_bard_error_info() {
        let answer = "BardErrorInfo is the name of Google's error envelope";
        let frame = json!([["wrb.fr",null,json!([null,null,null,null,[["choice",[answer]]]]).to_string()]]).to_string();
        assert_eq!(parse_response(&frame).unwrap().text, answer);
    }

    #[test]
    fn empty_and_error_frames_fail_without_returning_upstream_secrets() {
        let empty = json!([["wrb.fr",null,json!([null,null,null,null,[["id",[""]]]]).to_string()]]).to_string();
        assert!(parse_response(&empty).is_err());
        for body in ["secret-session-token in malformed reply", "[[\"er\",null,16,null,\"secret-session-token\"]]", "[]"] {
            let err = parse_response(body).unwrap_err();
            assert!(!err.message.contains("secret-session-token"));
        }
        assert_eq!(parse_response("[[\"er\",null,16]]").unwrap_err().status, 401);
    }

    // Shapes transcribed from gemini_models_test.go; no credentials or live requests.
    fn model_entry(id: &str, category: &str, display: &str, number: Value) -> Value {
        let mut fields = vec![Value::Null; 20];
        fields[0] = json!(id);
        fields[1] = json!(category);
        fields[11] = json!(display);
        fields[17] = number;
        json!(fields)
    }

    fn model_fixture(status: Value, tiers: Value, capabilities: Value, entries: Vec<Value>) -> String {
        let mut fields = vec![Value::Null; 18];
        fields[14] = status;
        fields[15] = json!(entries);
        fields[16] = tiers;
        fields[17] = capabilities;
        json!([["wrb.fr", "otAQ7b", json!(fields).to_string(), null, null, null, "generic"]]).to_string()
    }

    fn discovered_models() -> Vec<Model> {
        parse_models(&model_fixture(json!(1000), json!([8]), Value::Null, vec![
            model_entry("1111111111111111", "Fast", "Gemini 3.8 Flash", json!(1)),
            model_entry("2222222222222222", "Pro", "Gemini 3.1 Pro", json!(3)),
        ])).unwrap()
    }

    #[test]
    fn account_model_frames_preserve_real_routing_identity() {
        let frame = model_fixture(json!(1000), json!([8]), Value::Null, vec![
            model_entry("account-flash-id", "Fast", "Gemini 3.8 Flash", json!(1)),
            model_entry("account-pro-id", "Pro", "Gemini 3.1 Pro", json!(3)),
        ]);
        let body = format!(")]}}'\n\n42\n[[\"wrb.fr\",\"other-rpc\",\"[]\"]]\n{}\n{frame}\n12\n[[\"di\",0]]\n", frame.len());
        let models = parse_models(&body).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!((models[1].capacity, models[1].capacity_field, models[1].model_number), (2, 12, 3));
        for name in ["gemini-pro", "gemini-advanced", "gemini-3.1-pro", "Gemini 3.1 Pro", "account-pro-id"] {
            assert_eq!(resolve_model(name, &models).unwrap().model_id, "account-pro-id");
        }
        assert_eq!(resolve_model("gemini-fast", &models).unwrap().model_id, "account-flash-id");
        // Unlike the bridge's default-first behavior, native routing is always explicit.
        assert_eq!(resolve_model("", &models).unwrap_err().status, 400);
    }

    #[test]
    fn account_capacity_precedence_matches_source() {
        for (tiers, capabilities, capacity, field) in [
            (json!([]), json!([]), 1, 12),
            (json!([8]), json!([]), 2, 12),
            (json!([]), json!([19]), 2, 12),
            (json!([16, 8]), json!([]), 3, 12),
            (json!([]), json!([106, 19]), 3, 12),
            (json!([16]), json!([115, 19]), 4, 12),
            (json!([21, 22, 16]), json!([115]), 1, 13),
            (json!([22, 8]), json!([115]), 2, 13),
        ] {
            let body = model_fixture(json!(1000), tiers, capabilities, vec![model_entry("id", "Pro", "Pro", json!(3))]);
            let m = parse_models(&body).unwrap().remove(0);
            assert_eq!((m.capacity, m.capacity_field), (capacity, field));
        }
    }

    #[test]
    fn malformed_discovery_and_unavailable_accounts_fail_closed() {
        for body in ["", "<html>gemini-pro</html>", "[[\"wrb.fr\",\"wrong\",\"[]\"]]", "[[\"wrb.fr\",\"otAQ7b\",null,null,null,[7]]]", "[[\"wrb.fr\",\"otAQ7b\",\"broken\"]]"] {
            assert!(parse_models(body).is_err());
        }
        for status in [1014, 1016, 1021, 1033, 1040, 1042, 1054, 1057, 1060, 9999] {
            let body = model_fixture(json!(status), Value::Null, Value::Null, vec![model_entry("id", "Pro", "Pro", json!(3))]);
            assert_eq!(parse_models(&body).unwrap_err().status, 401);
        }
        for number in [json!(0), json!(-1), json!("3"), json!(3.5), json!(false)] {
            let body = model_fixture(json!(1000), Value::Null, Value::Null, vec![model_entry("id", "Pro", "Pro", number)]);
            assert!(parse_models(&body).is_err());
        }
        let body = model_fixture(json!(1000), Value::Null, Value::Null, vec![model_entry("id", "Flash", "Flash", Value::Null)]);
        assert_eq!(parse_models(&body).unwrap()[0].model_number, 1);
    }

    #[test]
    fn alternate_model_fields_and_multiline_json_are_supported() {
        let mut entry = model_entry("alternate-id", "", "", Value::Null);
        entry[9] = json!(3);
        entry[10] = json!("Pro");
        entry[19] = json!("Gemini 3 Pro");
        let body = model_fixture(Value::Null, Value::Null, Value::Null, vec![json!(null), json!([]), json!([123]), entry.clone(), entry]);
        let pretty = serde_json::to_string_pretty(&serde_json::from_str::<Value>(&body).unwrap()).unwrap();
        let models = parse_models(&pretty).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!((&*models[0].id, models[0].model_number), ("gemini-3-pro", 3));
    }

    #[test]
    fn explicit_selection_never_falls_back_or_invents_legacy_models() {
        let models = discovered_models();
        for name in ["gemini-3-pro-image-preview", "unknown", "gemini-99-flash"] {
            assert_eq!(resolve_model(name, &models).unwrap_err().status, 400);
        }
        assert!(resolve_model("gemini-pro", &models[..1]).is_err());
        let mut ambiguous = models.clone();
        ambiguous[0].aliases.push("shared".into());
        ambiguous[1].aliases.push("shared".into());
        assert!(resolve_model("shared", &ambiguous).unwrap_err().message.contains("ambiguous"));
        // Canonical names win over another entry's display aliases.
        ambiguous[0].aliases.push(models[1].id.clone());
        assert_eq!(resolve_model(&models[1].id, &ambiguous).unwrap().model_id, models[1].model_id);
    }
}
