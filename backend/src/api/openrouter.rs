/// OpenRouter chat completions API client with SSE streaming.
///
/// Drop-in replacement for the former Anthropic Claude client.
/// Uses the OpenAI-compatible endpoint with Nemotron Omni as the model.
/// The public function signatures are kept identical to `claude.rs` so
/// `handlers/llm_pipeline.rs` only needs import swaps.
///
/// Rate-limit handling:
/// - A caller-supplied `Arc<Semaphore>` enforces the 20 RPM OpenRouter cap.
/// - On HTTP 429, retries with exponential back-off (1 s → 2 s → 4 s, max 3 attempts).
use crate::error::AppError;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{debug, error, warn};

pub const DEFAULT_OPENROUTER_MODEL: &str = "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free";
const OPENROUTER_API_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const MAX_RETRIES: u32 = 3;

/// The system prompt that defines Clicky's personality and behaviour.
/// Identical content to the former Claude prompt — only the constant name changes.
pub const COMPANION_VOICE_RESPONSE_SYSTEM_PROMPT: &str = r#"you're clicky, a friendly always-on companion that lives in the user's system tray. the user just spoke to you via push-to-talk and you can see their screen(s). your reply will be spoken aloud via text-to-speech, so write the way you'd actually talk. this is an ongoing conversation — you remember everything they've said before.

rules:
- default to one or two sentences. be direct and dense. BUT if the user asks you to explain more, go deeper, or elaborate, then go all out — give a thorough, detailed explanation with no length limit.
- all lowercase, casual, warm. no emojis.
- write for the ear, not the eye. short sentences. no lists, bullet points, markdown, or formatting — just natural speech.
- don't use abbreviations or symbols that sound weird read aloud. write "for example" not "e.g.", spell out small numbers.
- if the user's question relates to what's on their screen, reference specific things you see.
- if the screenshot doesn't seem relevant to their question, just answer the question directly.
- you can help with anything — coding, writing, general knowledge, brainstorming.
- never say "simply" or "just".
- don't read out code verbatim. describe what the code does or what needs to change conversationally.
- focus on giving a thorough, useful explanation. don't end with simple yes/no questions like "want me to explain more?" or "should i show you?" — those are dead ends that force the user to just say yes.
- instead, when it fits naturally, end by planting a seed — mention something bigger or more ambitious they could try, a related concept that goes deeper, or a next-level technique that builds on what you just explained. make it something worth coming back for, not a question they'd just nod to. it's okay to not end with anything extra if the answer is complete on its own.
- if you receive multiple screen images, the one labeled "primary focus" is where the cursor is — prioritize that one but reference others if relevant.

element pointing:
you have a small blue triangle cursor that can fly to and point at things on screen. use it whenever pointing would genuinely help the user — if they're asking how to do something, looking for a menu, trying to find a button, or need help navigating an app, point at the relevant element. err on the side of pointing rather than not pointing, because it makes your help way more useful and concrete.

don't point at things when it would be pointless — like if the user asks a general knowledge question, or the conversation has nothing to do with what's on screen, or you'd just be pointing at something obvious they're already looking at. but if there's a specific UI element, menu, button, or area on screen that's relevant to what you're helping with, point at it.

when you point, append a coordinate tag at the very end of your response, AFTER your spoken text. the screenshot images are labeled with their pixel dimensions. use those dimensions as the coordinate space. the origin (0,0) is the top-left corner of the image. x increases rightward, y increases downward. the screenshot has a subtle red coordinate grid overlaid every 200 pixels with labeled coordinates at intersections. use these grid markers as reference points to estimate coordinates more precisely — for example, if an element is roughly halfway between the grid line at x=400 and x=600, estimate x=500.

format: [POINT:x,y:label] where x,y are integer pixel coordinates in the screenshot's coordinate space, and label is a short 1-3 word description of the element (like "search bar" or "save button"). if the element is on the cursor's screen you can omit the screen number. if the element is on a DIFFERENT screen, append :screenN where N is the screen number from the image label (e.g. :screen2). this is important — without the screen number, the cursor will point at the wrong place.

if pointing wouldn't help, append [POINT:none].

examples:
- user asks how to color grade in final cut: "you'll want to open the color inspector — it's right up in the top right area of the toolbar. click that and you'll get all the color wheels and curves. [POINT:1100,42:color inspector]"
- user asks what html is: "html stands for hypertext markup language, it's basically the skeleton of every web page. curious how it connects to the css you're looking at? [POINT:none]"
- user asks how to commit in xcode: "see that source control menu up top? click that and hit commit, or you can use command option c as a shortcut. [POINT:285,11:source control]"
- element is on screen 2 (not where cursor is): "that's over on your other monitor — see the terminal window? [POINT:400,300:terminal:screen2]""#;

pub use clicky_types::ScreenshotData;

/// Streams an OpenRouter response for the given messages and collects the full text.
///
/// Acquires one permit from `rate_limiter` before the HTTP request and releases on
/// drop, enforcing the OpenRouter 20 RPM cap across all concurrent users.
///
/// `on_text_delta` is called with each incremental chunk as it arrives over SSE.
pub async fn stream_llm_response(
    http_client: &Client,
    api_key: &str,
    model: &str,
    system_prompt: &str,
    messages: Vec<serde_json::Value>,
    rate_limiter: Arc<Semaphore>,
    mut on_text_delta: impl FnMut(&str),
) -> Result<String, AppError> {
    let mut system_messages: Vec<serde_json::Value> =
        vec![json!({"role": "system", "content": system_prompt})];
    system_messages.extend(messages);

    let request_body = json!({
        "model": model,
        "max_tokens": 1024,
        "messages": system_messages,
        "stream": true,
    });

    let mut last_error = AppError::Llm("no attempts made".into());

    for attempt in 0..MAX_RETRIES {
        // Acquire permit — released when `_permit` is dropped at end of scope
        let _permit = rate_limiter
            .acquire()
            .await
            .map_err(|_| AppError::Llm("rate limiter semaphore closed".into()))?;

        debug!("sending OpenRouter request (attempt {})", attempt + 1);

        let response = match http_client
            .post(OPENROUTER_API_URL)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(err) => {
                last_error = AppError::Llm(format!("network error: {err}"));
                break;
            }
        };

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let wait_secs = 1u64 << attempt; // 1 s, 2 s, 4 s
            warn!(
                "OpenRouter 429 on attempt {} — waiting {}s before retry",
                attempt + 1,
                wait_secs
            );
            tokio::time::sleep(tokio::time::Duration::from_secs(wait_secs)).await;
            last_error = AppError::Llm("rate limit exceeded after retries".into());
            continue;
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "unable to read error body".into());
            error!("OpenRouter API error {status}: {body}");
            return Err(AppError::Llm(format!("API error {status}: {body}")));
        }

        return parse_sse_stream(response, &mut on_text_delta).await;
    }

    Err(last_error)
}

/// Reads the SSE byte stream, extracts `choices[0].delta.content` deltas,
/// and assembles the full response text.
async fn parse_sse_stream(
    response: reqwest::Response,
    on_text_delta: &mut impl FnMut(&str),
) -> Result<String, AppError> {
    let mut full_response_text = String::new();
    let mut byte_stream = response.bytes_stream();
    let mut buffer = String::new();

    while let Some(chunk_result) = byte_stream.next().await {
        let chunk =
            chunk_result.map_err(|err| AppError::Llm(format!("stream error: {err}")))?;
        let chunk_text = String::from_utf8_lossy(&chunk);
        buffer.push_str(&chunk_text);

        // SSE events are separated by double newlines
        while let Some(boundary) = buffer.find("\n\n") {
            let event_text = buffer[..boundary].to_string();
            buffer = buffer[boundary + 2..].to_string();

            for line in event_text.lines() {
                let Some(json_data) = line.strip_prefix("data: ") else {
                    continue;
                };
                if json_data.trim() == "[DONE]" {
                    continue;
                }
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_data) {
                    if let Some(delta_content) =
                        parsed["choices"][0]["delta"]["content"].as_str()
                    {
                        full_response_text.push_str(delta_content);
                        on_text_delta(delta_content);
                    }
                }
            }
        }
    }

    debug!(
        "OpenRouter response complete ({} chars)",
        full_response_text.len()
    );
    Ok(full_response_text)
}

/// Builds the `content` array for a user message that includes screenshots and a transcript.
/// Uses OpenAI multimodal format: `image_url` blocks with base64 data URIs.
pub fn build_vision_message_content(
    user_transcript: &str,
    screenshots: &[ScreenshotData],
) -> serde_json::Value {
    let mut content_blocks: Vec<serde_json::Value> = Vec::new();

    for screenshot in screenshots {
        let base64_image = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &screenshot.jpeg_data,
        );
        // Label as a text block before the image for context
        content_blocks.push(json!({ "type": "text", "text": screenshot.label }));
        content_blocks.push(json!({
            "type": "image_url",
            "image_url": {
                "url": format!("data:image/jpeg;base64,{base64_image}"),
            },
        }));
    }

    content_blocks.push(json!({ "type": "text", "text": user_transcript }));
    json!(content_blocks)
}
