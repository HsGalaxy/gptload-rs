#![allow(clippy::result_large_err)]

use crate::config::UpstreamFormat;
use base64::Engine as _;
use bytes::Bytes;
use hyper::header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE};
use hyper::{Body, Method, Response};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_stream::wrappers::ReceiverStream;

pub enum AuthStyle {
    OpenAiBearer,
    AnthropicKey,
    None,
}

pub struct AdaptedRequest {
    pub method: Method,
    pub path_and_query: http::uri::PathAndQuery,
    pub body: Bytes,
    pub auth_style: AuthStyle,
}

pub fn adapt_request(
    format: UpstreamFormat,
    original_pq: &http::uri::PathAndQuery,
    method: &Method,
    body: &Bytes,
    model: &str,
    key: &str,
) -> Result<AdaptedRequest, Response<Body>> {
    match format {
        UpstreamFormat::Openai => Ok(AdaptedRequest {
            method: method.clone(),
            path_and_query: original_pq.clone(),
            body: body.clone(),
            auth_style: AuthStyle::OpenAiBearer,
        }),
        UpstreamFormat::Anthropic => adapt_anthropic_request(original_pq, body),
        UpstreamFormat::Gemini => adapt_gemini_request(original_pq, body, model, key),
    }
}

pub async fn adapt_response(
    format: UpstreamFormat,
    up_resp: Response<Body>,
    stream_request: bool,
    model: Option<String>,
) -> Response<Body> {
    match format {
        UpstreamFormat::Openai => up_resp,
        UpstreamFormat::Anthropic => {
            if stream_request {
                transform_sse_response(up_resp, model, anthropic_sse_to_openai)
            } else {
                transform_json_response(up_resp, model, anthropic_json_to_openai).await
            }
        }
        UpstreamFormat::Gemini => {
            if stream_request {
                transform_sse_response(up_resp, model, gemini_sse_to_openai)
            } else {
                transform_json_response(up_resp, model, gemini_json_to_openai).await
            }
        }
    }
}

fn adapt_anthropic_request(
    original_pq: &http::uri::PathAndQuery,
    body: &Bytes,
) -> Result<AdaptedRequest, Response<Body>> {
    if !is_chat_completions_path(original_pq.path()) {
        return Err(format_error(
            http::StatusCode::BAD_REQUEST,
            "anthropic format only supports /v1/chat/completions",
            "unsupported_format_path",
        ));
    }
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|_| {
        format_error(
            http::StatusCode::BAD_REQUEST,
            "request body must be valid json",
            "bad_request",
        )
    })?;
    let model = v.get("model").and_then(|m| m.as_str()).unwrap_or_default();
    let max_tokens = v
        .get("max_tokens")
        .or_else(|| v.get("max_completion_tokens"))
        .and_then(|n| n.as_u64())
        .unwrap_or(1024);
    let stream = v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    let mut system_parts = Vec::new();
    let mut messages = Vec::new();
    if let Some(input_messages) = v.get("messages").and_then(|m| m.as_array()) {
        for msg in input_messages {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = msg
                .get("content")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            if role == "system" {
                let text = content_to_text(&content);
                if !text.is_empty() {
                    system_parts.push(text);
                }
                continue;
            }
            let out_role = if role == "assistant" {
                "assistant"
            } else {
                "user"
            };
            messages.push(serde_json::json!({
                "role": out_role,
                "content": content_to_anthropic_blocks(&content),
            }));
        }
    }

    let mut out = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
        "stream": stream,
    });
    if !system_parts.is_empty() {
        out["system"] = serde_json::Value::String(system_parts.join("\n\n"));
    }
    if let Some(tools) = openai_tools_to_anthropic(v.get("tools")) {
        out["tools"] = tools;
    }
    if let Some(tool_choice) = openai_tool_choice_to_anthropic(v.get("tool_choice")) {
        out["tool_choice"] = tool_choice;
    }
    copy_number(&v, &mut out, "temperature", "temperature");
    copy_number(&v, &mut out, "top_p", "top_p");
    if let Some(stop) = v.get("stop") {
        out["stop_sequences"] = match stop {
            serde_json::Value::Array(_) => stop.clone(),
            serde_json::Value::String(_) => serde_json::json!([stop.clone()]),
            _ => serde_json::Value::Null,
        };
    }

    Ok(AdaptedRequest {
        method: Method::POST,
        path_and_query: http::uri::PathAndQuery::from_static("/v1/messages"),
        body: Bytes::from(out.to_string()),
        auth_style: AuthStyle::AnthropicKey,
    })
}

fn adapt_gemini_request(
    original_pq: &http::uri::PathAndQuery,
    body: &Bytes,
    model: &str,
    key: &str,
) -> Result<AdaptedRequest, Response<Body>> {
    if !is_chat_completions_path(original_pq.path()) {
        return Err(format_error(
            http::StatusCode::BAD_REQUEST,
            "gemini format only supports /v1/chat/completions",
            "unsupported_format_path",
        ));
    }
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|_| {
        format_error(
            http::StatusCode::BAD_REQUEST,
            "request body must be valid json",
            "bad_request",
        )
    })?;
    let stream = v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    let mut system_parts = Vec::new();
    let mut contents = Vec::new();
    if let Some(input_messages) = v.get("messages").and_then(|m| m.as_array()) {
        for msg in input_messages {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = msg
                .get("content")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            if role == "system" {
                let text = content_to_text(&content);
                if !text.is_empty() {
                    system_parts.push(text);
                }
                continue;
            }
            let parts = content_to_gemini_parts(&content);
            if parts.is_empty() {
                continue;
            }
            let out_role = if role == "assistant" { "model" } else { "user" };
            contents.push(serde_json::json!({
                "role": out_role,
                "parts": parts,
            }));
        }
    }

    let mut out = serde_json::json!({ "contents": contents });
    if !system_parts.is_empty() {
        out["systemInstruction"] = serde_json::json!({
            "parts": [{"text": system_parts.join("\n\n")}]
        });
    }

    let mut generation = serde_json::Map::new();
    if let Some(n) = v
        .get("max_tokens")
        .or_else(|| v.get("max_completion_tokens"))
        .and_then(|n| n.as_u64())
    {
        generation.insert("maxOutputTokens".to_string(), serde_json::json!(n));
    }
    if let Some(n) = v.get("temperature").and_then(|n| n.as_f64()) {
        generation.insert("temperature".to_string(), serde_json::json!(n));
    }
    if let Some(n) = v.get("top_p").and_then(|n| n.as_f64()) {
        generation.insert("topP".to_string(), serde_json::json!(n));
    }
    if let Some(stop) = v.get("stop") {
        let stops = match stop {
            serde_json::Value::Array(a) => a.clone(),
            serde_json::Value::String(_) => vec![stop.clone()],
            _ => Vec::new(),
        };
        if !stops.is_empty() {
            generation.insert("stopSequences".to_string(), serde_json::Value::Array(stops));
        }
    }
    if !generation.is_empty() {
        out["generationConfig"] = serde_json::Value::Object(generation);
    }

    let model_path = if model.starts_with("models/") {
        model.to_string()
    } else {
        format!("models/{model}")
    };
    let action = if stream {
        "streamGenerateContent"
    } else {
        "generateContent"
    };
    let encoded_key = utf8_percent_encode(key, NON_ALPHANUMERIC).to_string();
    let path = if stream {
        format!("/v1beta/{model_path}:{action}?alt=sse&key={encoded_key}")
    } else {
        format!("/v1beta/{model_path}:{action}?key={encoded_key}")
    };

    let path_and_query = path.parse().map_err(|_| {
        format_error(
            http::StatusCode::BAD_GATEWAY,
            "invalid gemini upstream path",
            "invalid_upstream_uri",
        )
    })?;
    Ok(AdaptedRequest {
        method: Method::POST,
        path_and_query,
        body: Bytes::from(out.to_string()),
        auth_style: AuthStyle::None,
    })
}

fn is_chat_completions_path(path: &str) -> bool {
    path == "/v1/chat/completions" || path == "/v1/chat/completions/"
}

fn copy_number(src: &serde_json::Value, dst: &mut serde_json::Value, src_key: &str, dst_key: &str) {
    if let Some(v) = src.get(src_key).and_then(|n| n.as_f64()) {
        dst[dst_key] = serde_json::json!(v);
    }
}

fn content_to_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(|t| t.as_str())
                    .or_else(|| part.get("content").and_then(|t| t.as_str()))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn openai_tools_to_anthropic(tools: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    let tools = tools.and_then(|v| v.as_array())?;
    let converted: Vec<serde_json::Value> = tools
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            let name = function.get("name").and_then(|v| v.as_str())?;
            let mut out = serde_json::Map::new();
            out.insert("name".to_string(), serde_json::json!(name));
            if let Some(description) = function.get("description").and_then(|v| v.as_str()) {
                out.insert("description".to_string(), serde_json::json!(description));
            }
            out.insert(
                "input_schema".to_string(),
                function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            );
            Some(serde_json::Value::Object(out))
        })
        .collect();
    if converted.is_empty() {
        None
    } else {
        Some(serde_json::Value::Array(converted))
    }
}

fn openai_tool_choice_to_anthropic(
    tool_choice: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    match tool_choice? {
        serde_json::Value::String(choice) => match choice.as_str() {
            "auto" => Some(serde_json::json!({ "type": "auto" })),
            "required" | "any" => Some(serde_json::json!({ "type": "any" })),
            "none" => None,
            _ => None,
        },
        serde_json::Value::Object(obj) => {
            let name = obj
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())?;
            Some(serde_json::json!({ "type": "tool", "name": name }))
        }
        _ => None,
    }
}

const MAX_DECODED_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

struct BinaryAttachment {
    mime: String,
    data: String,
}

fn content_to_anthropic_blocks(content: &serde_json::Value) -> serde_json::Value {
    match content {
        serde_json::Value::Array(parts) => {
            let out: Vec<serde_json::Value> =
                parts.iter().filter_map(openai_part_to_anthropic).collect();
            serde_json::Value::Array(out)
        }
        _ => serde_json::json!([{"type": "text", "text": content_to_text(content)}]),
    }
}

fn openai_part_to_anthropic(part: &serde_json::Value) -> Option<serde_json::Value> {
    let part_type = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if let Some(text) = text_from_openai_part(part) {
        if !matches!(part_type, "image_url" | "input_audio" | "file") {
            return Some(serde_json::json!({ "type": "text", "text": text }));
        }
    }

    if part_type == "input_audio" {
        warn_dropped_content_part("anthropic", part_type, "audio is not supported");
        return None;
    }

    let attachment = extract_binary_attachment(part, "anthropic")?;
    let block_type = if attachment.mime.starts_with("image/") {
        "image"
    } else {
        "document"
    };
    Some(serde_json::json!({
        "type": block_type,
        "source": {
            "type": "base64",
            "media_type": attachment.mime,
            "data": attachment.data
        }
    }))
}

fn content_to_gemini_parts(content: &serde_json::Value) -> Vec<serde_json::Value> {
    match content {
        serde_json::Value::Array(parts) => parts.iter().filter_map(openai_part_to_gemini).collect(),
        serde_json::Value::String(text) => vec![serde_json::json!({ "text": text })],
        _ => {
            let text = content_to_text(content);
            if text.is_empty() {
                Vec::new()
            } else {
                vec![serde_json::json!({ "text": text })]
            }
        }
    }
}

fn openai_part_to_gemini(part: &serde_json::Value) -> Option<serde_json::Value> {
    let part_type = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if let Some(text) = text_from_openai_part(part) {
        if !matches!(part_type, "image_url" | "input_audio" | "file") {
            return Some(serde_json::json!({ "text": text }));
        }
    }

    let attachment = extract_binary_attachment(part, "gemini")?;
    Some(serde_json::json!({
        "inlineData": {
            "mimeType": attachment.mime,
            "data": attachment.data
        }
    }))
}

fn text_from_openai_part(part: &serde_json::Value) -> Option<&str> {
    part.get("text")
        .and_then(|t| t.as_str())
        .or_else(|| part.get("content").and_then(|t| t.as_str()))
}

fn extract_binary_attachment(part: &serde_json::Value, provider: &str) -> Option<BinaryAttachment> {
    let part_type = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match part_type {
        "image_url" => {
            let url = part
                .get("image_url")
                .and_then(|u| u.get("url"))
                .and_then(|u| u.as_str())?;
            let Some((mime, data)) = parse_data_uri(url, MAX_DECODED_ATTACHMENT_BYTES) else {
                warn_dropped_content_part(
                    provider,
                    part_type,
                    "image_url must be a base64 data URI",
                );
                return None;
            };
            Some(BinaryAttachment { mime, data })
        }
        "input_audio" => {
            let audio = part.get("input_audio")?;
            let data = audio.get("data").and_then(|d| d.as_str()).unwrap_or("");
            let Some(data) = validate_base64_data(data, MAX_DECODED_ATTACHMENT_BYTES) else {
                warn_dropped_content_part(provider, part_type, "invalid or oversized base64 data");
                return None;
            };
            let format = audio
                .get("format")
                .and_then(|f| f.as_str())
                .unwrap_or("wav");
            Some(BinaryAttachment {
                mime: mime_from_audio_format(format),
                data,
            })
        }
        "file" => {
            let file = part.get("file")?;
            let file_data = file.get("file_data").and_then(|d| d.as_str()).unwrap_or("");
            let filename = file.get("filename").and_then(|f| f.as_str()).unwrap_or("");
            if let Some((mime, data)) = parse_data_uri(file_data, MAX_DECODED_ATTACHMENT_BYTES) {
                return Some(BinaryAttachment { mime, data });
            }
            let Some(data) = validate_base64_data(file_data, MAX_DECODED_ATTACHMENT_BYTES) else {
                warn_dropped_content_part(provider, part_type, "invalid or oversized base64 data");
                return None;
            };
            Some(BinaryAttachment {
                mime: mime_from_filename(filename),
                data,
            })
        }
        _ => None,
    }
}

fn parse_data_uri(uri: &str, max_bytes: usize) -> Option<(String, String)> {
    let stripped = uri.strip_prefix("data:")?;
    let (metadata, data) = stripped.split_once(',')?;
    let mut parts = metadata.split(';');
    let mime = parts.next()?.trim();
    let is_base64 = parts.any(|part| part.eq_ignore_ascii_case("base64"));
    if mime.is_empty() || !is_base64 {
        return None;
    }
    let data = validate_base64_data(data, max_bytes)?;
    Some((mime.to_string(), data))
}

fn validate_base64_data(data: &str, max_bytes: usize) -> Option<String> {
    let compact: String = data.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if compact.is_empty() || compact.len() > max_base64_len(max_bytes) {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(compact.as_bytes())
        .ok()?;
    if decoded.len() > max_bytes {
        return None;
    }
    Some(compact)
}

fn max_base64_len(decoded_bytes: usize) -> usize {
    decoded_bytes.div_ceil(3) * 4
}

fn mime_from_audio_format(format: &str) -> String {
    match format.trim_start_matches('.').to_ascii_lowercase().as_str() {
        "mp3" => "audio/mpeg".to_string(),
        "wav" => "audio/wav".to_string(),
        "ogg" => "audio/ogg".to_string(),
        "flac" => "audio/flac".to_string(),
        "m4a" => "audio/mp4".to_string(),
        other if !other.is_empty() => format!("audio/{other}"),
        _ => "audio/wav".to_string(),
    }
}

fn mime_from_filename(filename: &str) -> String {
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => "application/pdf".to_string(),
        "doc" => "application/msword".to_string(),
        "docx" => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document".to_string()
        }
        "xls" => "application/vnd.ms-excel".to_string(),
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".to_string(),
        "ppt" => "application/vnd.ms-powerpoint".to_string(),
        "pptx" => {
            "application/vnd.openxmlformats-officedocument.presentationml.presentation".to_string()
        }
        "txt" => "text/plain".to_string(),
        "csv" => "text/csv".to_string(),
        "html" | "htm" => "text/html".to_string(),
        "json" => "application/json".to_string(),
        "xml" => "application/xml".to_string(),
        "zip" => "application/zip".to_string(),
        "mp3" => "audio/mpeg".to_string(),
        "mp4" => "video/mp4".to_string(),
        "wav" => "audio/wav".to_string(),
        "ogg" => "audio/ogg".to_string(),
        "webm" => "video/webm".to_string(),
        "png" => "image/png".to_string(),
        "jpg" | "jpeg" => "image/jpeg".to_string(),
        "gif" => "image/gif".to_string(),
        "webp" => "image/webp".to_string(),
        "svg" => "image/svg+xml".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

fn warn_dropped_content_part(provider: &str, part_type: &str, reason: &str) {
    tracing::warn!(
        provider = provider,
        part_type = part_type,
        reason = reason,
        "dropping content part during format conversion"
    );
}

async fn transform_json_response(
    up_resp: Response<Body>,
    model: Option<String>,
    f: fn(&serde_json::Value, Option<String>) -> serde_json::Value,
) -> Response<Body> {
    let (mut parts, body) = up_resp.into_parts();
    let body = match hyper::body::to_bytes(body).await {
        Ok(body) => body,
        Err(_) => {
            parts.status = http::StatusCode::BAD_GATEWAY;
            parts.headers.remove(CONTENT_LENGTH);
            parts.headers.remove(CONTENT_ENCODING);
            parts.headers.insert(
                CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
            return Response::from_parts(
                parts,
                Body::from(r#"{"error":{"message":"failed to read upstream response"}}"#),
            );
        }
    };
    if !parts.status.is_success() {
        return Response::from_parts(parts, Body::from(body));
    }
    parts.headers.remove(CONTENT_LENGTH);
    parts.headers.remove(CONTENT_ENCODING);
    parts.headers.insert(
        CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return Response::from_parts(parts, Body::from(body)),
    };
    let out = f(&value, model);
    Response::from_parts(parts, Body::from(out.to_string()))
}

fn transform_sse_response(
    up_resp: Response<Body>,
    model: Option<String>,
    f: fn(&serde_json::Value, Option<&str>) -> Vec<serde_json::Value>,
) -> Response<Body> {
    let (mut parts, body) = up_resp.into_parts();
    if !parts.status.is_success() {
        return Response::from_parts(parts, body);
    }
    parts.headers.remove(CONTENT_LENGTH);
    parts.headers.remove(CONTENT_ENCODING);
    parts.headers.insert(
        CONTENT_TYPE,
        http::HeaderValue::from_static("text/event-stream"),
    );
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, io::Error>>(32);
    tokio::spawn(async move {
        use hyper::body::HttpBody;
        let mut body = body;
        let mut buf = String::new();
        while let Some(chunk) = body.data().await {
            let Ok(chunk) = chunk else {
                break;
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].trim_end_matches('\r').to_string();
                buf.drain(..=pos);
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                    continue;
                };
                for chunk in f(&value, model.as_deref()) {
                    let msg = format!("data: {}\n\n", chunk);
                    if tx.send(Ok(Bytes::from(msg))).await.is_err() {
                        return;
                    }
                }
            }
            if buf.len() > 1024 * 1024 {
                buf.clear();
            }
        }
        let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
    });
    Response::from_parts(parts, Body::wrap_stream(ReceiverStream::new(rx)))
}

fn anthropic_json_to_openai(v: &serde_json::Value, model: Option<String>) -> serde_json::Value {
    let id = v
        .get("id")
        .and_then(|s| s.as_str())
        .unwrap_or("chatcmpl-anthropic");
    let model = v
        .get("model")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
        .or(model)
        .unwrap_or_default();
    let content = v
        .get("content")
        .and_then(|c| c.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let input = v
        .get("usage")
        .and_then(|u| u.get("input_tokens"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let output = v
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    chat_completion_json(id, &model, content, input, output)
}

fn gemini_json_to_openai(v: &serde_json::Value, model: Option<String>) -> serde_json::Value {
    let model = model.unwrap_or_default();
    let candidate = v
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let content = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let prompt = v
        .get("usageMetadata")
        .and_then(|u| u.get("promptTokenCount"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let candidates = v
        .get("usageMetadata")
        .and_then(|u| u.get("candidatesTokenCount"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let thought = v
        .get("usageMetadata")
        .and_then(|u| u.get("thoughtsTokenCount"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let completion = candidates.saturating_add(thought);
    let total = v
        .get("usageMetadata")
        .and_then(|u| u.get("totalTokenCount"))
        .and_then(|n| n.as_u64())
        .unwrap_or(prompt.saturating_add(completion));
    let mut resp = chat_completion_json("chatcmpl-gemini", &model, content, prompt, completion);
    resp["usage"]["thought_tokens"] = serde_json::json!(thought);
    resp["usage"]["total_tokens"] = serde_json::json!(total);
    resp
}

fn chat_completion_json(
    id: &str,
    model: &str,
    content: String,
    prompt_tokens: u64,
    completion_tokens: u64,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "object": "chat.completion",
        "created": unix_secs(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens
        }
    })
}

fn anthropic_sse_to_openai(v: &serde_json::Value, model: Option<&str>) -> Vec<serde_json::Value> {
    let ty = v.get("type").and_then(|s| s.as_str()).unwrap_or("");
    match ty {
        "message_start" => vec![chat_chunk_json(
            v.get("message")
                .and_then(|m| m.get("id"))
                .and_then(|s| s.as_str())
                .unwrap_or("chatcmpl-anthropic"),
            model.unwrap_or(""),
            serde_json::json!({"role": "assistant"}),
            None,
            None,
        )],
        "content_block_delta" => {
            let text = v
                .get("delta")
                .and_then(|d| d.get("text"))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            if text.is_empty() {
                Vec::new()
            } else {
                vec![chat_chunk_json(
                    "chatcmpl-anthropic",
                    model.unwrap_or(""),
                    serde_json::json!({"content": text}),
                    None,
                    None,
                )]
            }
        }
        "message_delta" => {
            let usage = v.get("usage").map(|u| {
                let output = u.get("output_tokens").and_then(|n| n.as_u64()).unwrap_or(0);
                serde_json::json!({
                    "prompt_tokens": 0,
                    "completion_tokens": output,
                    "total_tokens": output
                })
            });
            usage
                .map(|usage| {
                    vec![chat_chunk_json(
                        "chatcmpl-anthropic",
                        model.unwrap_or(""),
                        serde_json::json!({}),
                        None,
                        Some(usage),
                    )]
                })
                .unwrap_or_default()
        }
        "message_stop" => vec![chat_chunk_json(
            "chatcmpl-anthropic",
            model.unwrap_or(""),
            serde_json::json!({}),
            Some("stop"),
            None,
        )],
        _ => Vec::new(),
    }
}

fn gemini_sse_to_openai(v: &serde_json::Value, model: Option<&str>) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    if let Some(candidate) = v
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
    {
        let text = candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        if !text.is_empty() {
            out.push(chat_chunk_json(
                "chatcmpl-gemini",
                model.unwrap_or(""),
                serde_json::json!({"content": text}),
                None,
                None,
            ));
        }
        if candidate.get("finishReason").is_some() {
            out.push(chat_chunk_json(
                "chatcmpl-gemini",
                model.unwrap_or(""),
                serde_json::json!({}),
                Some("stop"),
                None,
            ));
        }
    }
    if let Some(usage) = v.get("usageMetadata") {
        let prompt = usage
            .get("promptTokenCount")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        let candidates = usage
            .get("candidatesTokenCount")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        let thought = usage
            .get("thoughtsTokenCount")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        let completion = candidates.saturating_add(thought);
        let total = usage
            .get("totalTokenCount")
            .and_then(|n| n.as_u64())
            .unwrap_or(prompt.saturating_add(completion));
        out.push(chat_chunk_json(
            "chatcmpl-gemini",
            model.unwrap_or(""),
            serde_json::json!({}),
            None,
            Some(serde_json::json!({
                "prompt_tokens": prompt,
                "completion_tokens": completion,
                "thought_tokens": thought,
                "total_tokens": total
            })),
        ));
    }
    out
}

fn chat_chunk_json(
    id: &str,
    model: &str,
    delta: serde_json::Value,
    finish_reason: Option<&str>,
    usage: Option<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": unix_secs(),
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason
        }],
        "usage": usage
    })
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn format_error(status: http::StatusCode, message: &str, code: &str) -> Response<Body> {
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": "proxy_error",
            "param": null,
            "code": code
        }
    });
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| Response::new(Body::from("proxy_error")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_request_moves_system_and_messages() {
        let body = Bytes::from_static(
            br#"{"model":"claude-3","messages":[{"role":"system","content":"sys"},{"role":"user","content":"hi"}],"max_tokens":7,"stream":true}"#,
        );
        let adapted = adapt_request(
            UpstreamFormat::Anthropic,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body,
            "claude-3",
            "sk-ant-test",
        )
        .unwrap();
        assert_eq!(adapted.path_and_query.as_str(), "/v1/messages");
        let v: serde_json::Value = serde_json::from_slice(&adapted.body).unwrap();
        assert_eq!(v["system"], "sys");
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["max_tokens"], 7);
    }

    #[test]
    fn anthropic_request_converts_tools_and_tool_choice() {
        let body = Bytes::from_static(
            br#"{"model":"claude-3","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"lookup","description":"Lookup data","parameters":{"type":"object","properties":{"q":{"type":"string"}}}}}],"tool_choice":{"type":"function","function":{"name":"lookup"}}}"#,
        );
        let adapted = adapt_request(
            UpstreamFormat::Anthropic,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body,
            "claude-3",
            "sk-ant-test",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&adapted.body).unwrap();
        assert_eq!(v["tools"][0]["name"], "lookup");
        assert_eq!(v["tools"][0]["description"], "Lookup data");
        assert_eq!(v["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(v["tool_choice"]["type"], "tool");
        assert_eq!(v["tool_choice"]["name"], "lookup");
    }

    #[test]
    fn anthropic_request_converts_image_data_uri() {
        let body = Bytes::from_static(
            br#"{"model":"claude-3","messages":[{"role":"user","content":[{"type":"text","text":"look"},{"type":"image_url","image_url":{"url":"data:image/png;base64,aGVsbG8="}}]}]}"#,
        );
        let adapted = adapt_request(
            UpstreamFormat::Anthropic,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body,
            "claude-3",
            "sk-ant-test",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&adapted.body).unwrap();
        let content = &v["messages"][0]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "aGVsbG8=");
    }

    #[test]
    fn gemini_request_uses_generate_content_path() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-1.5-pro","messages":[{"role":"user","content":"hi"}],"stream":false}"#,
        );
        let adapted = adapt_request(
            UpstreamFormat::Gemini,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body,
            "gemini-1.5-pro",
            "AIza test",
        )
        .unwrap();
        assert!(adapted
            .path_and_query
            .as_str()
            .starts_with("/v1beta/models/gemini-1.5-pro:generateContent?key="));
        let v: serde_json::Value = serde_json::from_slice(&adapted.body).unwrap();
        assert_eq!(v["contents"][0]["role"], "user");
    }

    #[test]
    fn gemini_request_converts_input_audio_part() {
        let body = Bytes::from_static(
            br#"{"model":"gemini-1.5-pro","messages":[{"role":"user","content":[{"type":"text","text":"transcribe"},{"type":"input_audio","input_audio":{"format":"mp3","data":"aGVsbG8="}}]}],"stream":false}"#,
        );
        let adapted = adapt_request(
            UpstreamFormat::Gemini,
            &"/v1/chat/completions".parse().unwrap(),
            &Method::POST,
            &body,
            "gemini-1.5-pro",
            "AIza test",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&adapted.body).unwrap();
        let parts = &v["contents"][0]["parts"];
        assert_eq!(parts[0]["text"], "transcribe");
        assert_eq!(parts[1]["inlineData"]["mimeType"], "audio/mpeg");
        assert_eq!(parts[1]["inlineData"]["data"], "aGVsbG8=");
    }

    #[test]
    fn gemini_json_usage_includes_thought_tokens() {
        let gemini_resp = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [{"text": "hello"}],
                    "role": "model"
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 15,
                "candidatesTokenCount": 25,
                "thoughtsTokenCount": 5,
                "totalTokenCount": 45
            }
        });

        let converted = gemini_json_to_openai(&gemini_resp, Some("gemini-2.0-flash".to_string()));

        assert_eq!(converted["usage"]["prompt_tokens"], 15);
        assert_eq!(converted["usage"]["completion_tokens"], 30);
        assert_eq!(converted["usage"]["thought_tokens"], 5);
        assert_eq!(converted["usage"]["total_tokens"], 45);
    }

    #[test]
    fn gemini_sse_usage_includes_thought_tokens() {
        let gemini_chunk = serde_json::json!({
            "usageMetadata": {
                "promptTokenCount": 8,
                "candidatesTokenCount": 11,
                "thoughtsTokenCount": 3,
                "totalTokenCount": 22
            }
        });

        let chunks = gemini_sse_to_openai(&gemini_chunk, Some("gemini-2.0-flash"));
        let usage = &chunks[0]["usage"];

        assert_eq!(usage["prompt_tokens"], 8);
        assert_eq!(usage["completion_tokens"], 14);
        assert_eq!(usage["thought_tokens"], 3);
        assert_eq!(usage["total_tokens"], 22);
    }
}
