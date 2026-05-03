/// Gemini generative language API client with SSE streaming.
///
/// Mirrors the public interface of `openrouter.rs` so `llm_pipeline.rs` can
/// dispatch to either backend via a simple `match` on `Provider`.
///
/// Key differences from OpenRouter:
/// - URL: `.../streamGenerateContent?key={api_key}&alt=sse`
///   The `alt=sse` param is REQUIRED — without it Gemini returns a raw JSON array
///   instead of SSE-framed events, and the `data: ` parser gets nothing.
/// - Auth: API key in the URL query param — no `Authorization` header.
/// - Request body uses `contents` + `systemInstruction` instead of `messages`.
/// - Images use `inlineData` with base64, not `image_url`.
/// - SSE text lives at `candidates[0].content.parts[0].text`.
/// - No `[DONE]` sentinel — the stream ends when the HTTP body closes.
use crate::error::AppError;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

pub use clicky_types::ScreenshotData;

const MAX_RETRIES: u32 = 3;
const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";

/// Streams a Gemini response for the given messages and collects the full text.
///
/// `messages` follow the same shape as the OpenRouter pipeline produces:
/// `[{"role":"user","content": <parts Value>}, ...]`.
/// This function translates them into the Gemini `contents` wire format.
pub async fn stream_llm_response(
    http_client: &Client,
    api_key: &str,
    model: &str,
    system_prompt: &str,
    messages: Vec<Value>,
    rate_limiter: Arc<Semaphore>,
    mut on_text_delta: impl FnMut(&str),
) -> Result<String, AppError> {
    // alt=sse is required — without it Gemini returns a raw JSON array, not SSE events.
    let url = format!("{GEMINI_API_BASE}/{model}:streamGenerateContent?key={api_key}&alt=sse");

    let contents = convert_messages_to_gemini_contents(messages);

    let request_body = json!({
        "systemInstruction": {
            "parts": [{ "text": system_prompt }]
        },
        "contents": contents,
        "generationConfig": { "maxOutputTokens": 8192 }
    });

    // Log the outgoing request (omit base64 image data to keep logs readable).
    info!(
        "Gemini request → model={model} contents_count={} system_prompt_len={}",
        request_body["contents"].as_array().map(|a| a.len()).unwrap_or(0),
        system_prompt.len(),
    );
    debug!(
        "Gemini request body (truncated): {}",
        truncate_for_log(&request_body.to_string(), 2000),
    );

    let mut last_error = AppError::Llm("no attempts made".into());

    for attempt in 0..MAX_RETRIES {
        let _permit = rate_limiter
            .acquire()
            .await
            .map_err(|_| AppError::Llm("rate limiter semaphore closed".into()))?;

        info!("Gemini: sending request (attempt {})", attempt + 1);

        let response = match http_client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(err) => {
                last_error = AppError::Llm(format!("network error: {err}"));
                warn!("Gemini: network error on attempt {}: {err}", attempt + 1);
                break;
            }
        };

        let status = response.status();
        info!("Gemini: HTTP {status}");

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let wait_secs = 1u64 << attempt;
            warn!(
                "Gemini 429 on attempt {} — waiting {}s before retry",
                attempt + 1,
                wait_secs
            );
            tokio::time::sleep(tokio::time::Duration::from_secs(wait_secs)).await;
            last_error = AppError::Llm("rate limit exceeded after retries".into());
            continue;
        }

        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "unable to read error body".into());
            error!("Gemini API error {status}: {body}");
            return Err(AppError::Llm(format!("API error {status}: {body}")));
        }

        return parse_gemini_sse_stream(response, &mut on_text_delta).await;
    }

    Err(last_error)
}

/// Reads the Gemini SSE byte stream (`alt=sse`), extracting
/// `candidates[0].content.parts[0].text` from each JSON chunk.
/// Gemini streams end when the HTTP body closes — there is no `[DONE]` sentinel.
async fn parse_gemini_sse_stream(
    response: reqwest::Response,
    on_text_delta: &mut impl FnMut(&str),
) -> Result<String, AppError> {
    let mut full_response_text = String::new();
    let mut byte_stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut chunk_count = 0usize;

    while let Some(chunk_result) = byte_stream.next().await {
        let chunk =
            chunk_result.map_err(|err| AppError::Llm(format!("stream error: {err}")))?;
        let chunk_text = String::from_utf8_lossy(&chunk);
        chunk_count += 1;

        debug!(
            "Gemini raw chunk #{chunk_count} ({} bytes): {}",
            chunk.len(),
            truncate_for_log(chunk_text.trim(), 500),
        );

        buffer.push_str(&chunk_text);

        // SSE events are delimited by double newlines, each prefixed with "data: "
        while let Some(boundary) = buffer.find("\n\n") {
            let event_text = buffer[..boundary].to_string();
            buffer = buffer[boundary + 2..].to_string();

            for line in event_text.lines() {
                let Some(json_data) = line.strip_prefix("data: ") else {
                    continue;
                };
                let json_data = json_data.trim();
                if json_data.is_empty() {
                    continue;
                }
                extract_text_from_gemini_chunk(json_data, &mut full_response_text, on_text_delta);
            }
        }
    }

    // Drain any remaining data that lacked a trailing \n\n
    let remaining = buffer.trim();
    if !remaining.is_empty() {
        debug!(
            "Gemini: draining remaining buffer ({} bytes): {}",
            remaining.len(),
            truncate_for_log(remaining, 500),
        );
        for line in remaining.lines() {
            if let Some(json_data) = line.strip_prefix("data: ") {
                let json_data = json_data.trim();
                if !json_data.is_empty() {
                    extract_text_from_gemini_chunk(
                        json_data,
                        &mut full_response_text,
                        on_text_delta,
                    );
                }
            } else {
                // Fallback: the remaining buffer might be a raw JSON array (no alt=sse).
                // This should not happen with alt=sse, but log it so we can diagnose.
                let trimmed = remaining.trim_start_matches('[').trim_end_matches(']');
                if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
                    if let Some(text) =
                        parsed["candidates"][0]["content"]["parts"][0]["text"].as_str()
                    {
                        warn!("Gemini: got raw JSON array chunk (missing alt=sse?): extracted text={text:?}");
                        full_response_text.push_str(text);
                        on_text_delta(text);
                    }
                }
                break;
            }
        }
    }

    info!(
        "Gemini response complete — {} chars across {} chunks",
        full_response_text.len(),
        chunk_count,
    );

    if full_response_text.is_empty() {
        warn!("Gemini: response was empty — raw buffer at end: {remaining:?}");
    }

    Ok(full_response_text)
}

/// Parses one SSE JSON payload and appends any text content to the accumulator.
fn extract_text_from_gemini_chunk(
    json_data: &str,
    accumulator: &mut String,
    on_text_delta: &mut impl FnMut(&str),
) {
    match serde_json::from_str::<Value>(json_data) {
        Ok(parsed) => {
            if let Some(text) =
                parsed["candidates"][0]["content"]["parts"][0]["text"].as_str()
            {
                debug!("Gemini chunk text: {text:?}");
                accumulator.push_str(text);
                on_text_delta(text);
            } else {
                // Log non-text events (e.g. finishReason, usageMetadata) at debug level.
                debug!("Gemini chunk (no text): {}", truncate_for_log(json_data, 300));
            }
        }
        Err(err) => {
            warn!("Gemini: failed to parse SSE chunk JSON: {err} — raw: {}", truncate_for_log(json_data, 300));
        }
    }
}

/// Builds the Gemini `parts` array for a user message that includes screenshots and a
/// transcript.  Uses `inlineData` blocks (base64 JPEG) rather than `image_url`.
pub fn build_vision_message_content(
    user_transcript: &str,
    screenshots: &[ScreenshotData],
) -> Value {
    let mut parts: Vec<Value> = Vec::new();

    for screenshot in screenshots {
        let base64_image = STANDARD.encode(&screenshot.jpeg_data);
        parts.push(json!({ "text": screenshot.label }));
        parts.push(json!({
            "inlineData": {
                "mimeType": "image/jpeg",
                "data": base64_image
            }
        }));
    }

    parts.push(json!({ "text": user_transcript }));
    json!(parts)
}

/// Converts the OpenAI-style `messages` list (produced by `llm_pipeline.rs`) into
/// the Gemini `contents` wire format.
///
/// - `"role": "assistant"` → `"role": "model"`
/// - String `content` is wrapped in `[{"text": "..."}]`.
/// - Array `content` (vision parts) is passed through as-is; Gemini parts are a
///   superset of the OpenAI array shape for text blocks, and `build_vision_message_content`
///   already emits the correct Gemini `inlineData` format.
fn convert_messages_to_gemini_contents(messages: Vec<Value>) -> Vec<Value> {
    messages
        .into_iter()
        .map(|msg| {
            let role = match msg["role"].as_str().unwrap_or("user") {
                "assistant" => "model",
                other => other,
            };

            let parts = match &msg["content"] {
                Value::String(text) => json!([{ "text": text }]),
                Value::Array(_) => msg["content"].clone(),
                other => json!([{ "text": other.to_string() }]),
            };

            json!({ "role": role, "parts": parts })
        })
        .collect()
}

/// Truncates a string to `max_chars` for logging, appending `…` if cut.
fn truncate_for_log(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        s
    } else {
        &s[..s.floor_char_boundary(max_chars)]
    }
}
