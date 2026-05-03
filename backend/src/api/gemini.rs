/// Gemini generative language API client.
///
/// Mirrors the public interface of `openrouter.rs` so `llm_pipeline.rs` can
/// dispatch to either backend via a simple `match` on `Provider`.
///
/// Key differences from OpenRouter:
/// - URL: `.../generateContent?key={api_key}` (plain JSON, not SSE).
/// - Auth: API key in the URL query param — no `Authorization` header.
/// - Request body uses `contents` + `systemInstruction` instead of `messages`.
/// - Images use `inlineData` with base64, not `image_url`.
/// - Response text lives at `candidates[0].content.parts[0].text`.
use crate::error::AppError;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

pub use clicky_types::ScreenshotData;

const MAX_RETRIES: u32 = 3;
const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";

/// Sends a request to Gemini and returns the full response text.
///
/// Only the current turn is sent — no conversation history — to prevent context
/// pollution from previous transcript/screenshot pairs contaminating responses.
/// `messages` must contain exactly one entry: the current user turn.
pub async fn stream_llm_response(
    http_client: &Client,
    api_key: &str,
    model: &str,
    system_prompt: &str,
    messages: Vec<Value>,
    rate_limiter: Arc<Semaphore>,
) -> Result<String, AppError> {
    let url = format!("{GEMINI_API_BASE}/{model}:generateContent?key={api_key}");

    // Always take only the last (most recent) user message regardless of how many
    // messages the pipeline passes in — this is the single-turn, no-history contract.
    let current_message = messages
        .into_iter()
        .last()
        .ok_or_else(|| AppError::Llm("no messages provided".into()))?;

    let contents = convert_messages_to_gemini_contents(vec![current_message]);

    let request_body = json!({
        "systemInstruction": {
            "parts": [{ "text": system_prompt }]
        },
        "contents": contents,
        "generationConfig": { "maxOutputTokens": 8192 }
    });

    // Log the outgoing request (omit base64 image data to keep logs readable).
    // contents_count must always be 1.
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

        let json: Value = response
            .json()
            .await
            .map_err(|err| AppError::Llm(format!("response JSON parse error: {err}")))?;

        let text = json["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_owned();

        info!("Gemini response complete — {} chars", text.len());

        if text.is_empty() {
            warn!("Gemini: response text was empty");
        }

        return Ok(text);
    }

    Err(last_error)
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
