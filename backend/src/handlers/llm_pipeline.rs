/// LLM pipeline handler — vision + TTS.
///
/// Screenshots arrive from the frontend over WebSocket. The backend no longer captures them.
/// Computer Use is no longer run in the primary pipeline — the frontend requests it on-demand
/// via `ClientMessage::RequestCUCoordinate` when the local UI-map match score is too low.
use crate::api::{self, gemini, openrouter, Provider};
use crate::error::AppError;
use crate::handlers::tts;
use clicky_types::{DisplayInfo, ElementIntent, ScreenshotData, ServerMessage};
use std::io::Write;
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};
use tracing::{error, info, warn};

pub struct LlmPipelineConfig {
    pub llm_provider: Arc<api::Provider>,
    pub deepgram_api_key: Option<String>,
    pub deepgram_tts_voice: Option<String>,
    pub tts_enabled: bool,
    /// Shared semaphore enforcing the AI provider RPM cap.
    pub llm_rate_limiter: Arc<Semaphore>,
}

/// Runs the LLM + TTS pipeline for a completed transcript.
///
/// LLM call fires first; TTS fires as soon as LLM text is known.
/// Computer Use is NOT called here — the frontend requests it on-demand via
/// `ClientMessage::RequestCUCoordinate` when the UI-map confidence gate fails.
/// A single `ServerMessage::PipelineReady` is emitted when LLM + TTS are done.
pub async fn run_llm_pipeline(
    http_client: reqwest::Client,
    config: LlmPipelineConfig,
    transcript: String,
    screenshots: Vec<ScreenshotData>,
    display_infos: Vec<DisplayInfo>,
    history_exchanges: Vec<(String, String)>,
    uia_summaries: Vec<clicky_types::UIElementSummary>,
    server_tx: mpsc::Sender<ServerMessage>,
) {
    if screenshots.is_empty() {
        warn!("no screenshots received from frontend, skipping pipeline");
        let _ = server_tx
            .send(
                AppError::Screenshot("no screenshots received from frontend".into())
                    .into_server_message(),
            )
            .await;
        return;
    }

    info!(
        "[LLM] pipeline start — transcript='{}' screenshots={} displays={}",
        transcript,
        screenshots.len(),
        display_infos.len()
    );
    log_debug_screenshots(&screenshots, &display_infos);

    let mut messages: Vec<serde_json::Value> = Vec::new();
    for (user_text, assistant_text) in &history_exchanges {
        messages.push(serde_json::json!({"role": "user", "content": user_text}));
        messages.push(serde_json::json!({"role": "assistant", "content": assistant_text}));
    }

    // Prepend UIA element context as a user message so the LLM knows which named
    // elements are visible on screen before seeing the transcript + screenshots.
    if !uia_summaries.is_empty() {
        let mut element_text = String::from("visible UI elements on screen (from accessibility tree):\n");
        for el in uia_summaries.iter().take(30) {
            element_text.push_str(&format!(
                "- {} ({}) in {} region\n",
                el.name, el.control_type, el.region_hint
            ));
        }
        messages.push(serde_json::json!({"role": "user", "content": element_text}));
        info!("[LLM] injected {} UIA element summaries into prompt", uia_summaries.len().min(30));
    }

    let user_content = match config.llm_provider.as_ref() {
        Provider::OpenRouter { .. } => {
            openrouter::build_vision_message_content(&transcript, &screenshots)
        }
        Provider::Gemini { .. } => {
            gemini::build_vision_message_content(&transcript, &screenshots)
        }
    };
    messages.push(serde_json::json!({"role": "user", "content": user_content}));

    let llm_result = run_llm_call(&http_client, &config, messages).await;

    let response_text = match llm_result {
        Ok(text) => text,
        Err(err) => {
            error!("[LLM] API error: {err}");
            let _ = server_tx.send(err.into_server_message()).await;
            return;
        }
    };

    if response_text.is_empty() {
        warn!("[LLM] empty response — aborting pipeline");
        let _ = server_tx
            .send(AppError::Llm("empty response from model".into()).into_server_message())
            .await;
        return;
    }

    info!("[LLM] response received ({} chars)", response_text.len());

    let intent = parse_intent_json(&response_text);
    info!("[LLM] parsed intent: {:?}", intent.as_ref().map(|i| (&i.label, &i.region_hint)));

    // Strip the JSON block from the spoken text so TTS doesn't read raw JSON.
    let spoken_text = strip_intent_json(&response_text);
    info!("[LLM] spoken_text ({} chars): '{}'", spoken_text.len(), &spoken_text[..spoken_text.len().min(100)]);

    append_to_log(&format!(
        "[LLM] transcript: {transcript}\n[LLM] response: {response_text}\n[LLM] intent: {intent:?}\n"
    ));

    let mp3_bytes = if config.tts_enabled && !spoken_text.is_empty() {
        info!("[TTS] synthesis starting ({} chars)", spoken_text.len());
        if let Some(ref api_key) = config.deepgram_api_key {
            let bytes = tts::synthesize_speech_bytes(
                &http_client,
                api_key,
                config.deepgram_tts_voice.as_deref(),
                &spoken_text,
            )
            .await;
            info!("[TTS] synthesis done — {} bytes", bytes.len());
            bytes
        } else {
            Vec::new()
        }
    } else {
        if !config.tts_enabled {
            info!("[TTS] disabled");
        }
        Vec::new()
    };

    let final_spoken = if spoken_text.is_empty() {
        "I didn't quite get you".to_owned()
    } else {
        spoken_text
    };

    info!("[PIPELINE] sending PipelineReady — intent={:?}", intent.as_ref().map(|i| &i.label));
    let _ = server_tx
        .send(ServerMessage::PipelineReady {
            spoken_text: final_spoken,
            mp3_bytes,
            // coordinate is always None — the frontend resolves position via UI-map / CU
            coordinate: None,
            display_infos,
            intent,
        })
        .await;
}

async fn run_llm_call(
    http_client: &reqwest::Client,
    config: &LlmPipelineConfig,
    messages: Vec<serde_json::Value>,
) -> Result<String, AppError> {
    match config.llm_provider.as_ref() {
        Provider::OpenRouter { api_key, model } => {
            info!("sending to OpenRouter ({model})...");
            openrouter::stream_llm_response(
                http_client,
                api_key,
                model,
                INTENT_SYSTEM_PROMPT,
                messages,
                Arc::clone(&config.llm_rate_limiter),
            )
            .await
        }
        Provider::Gemini { api_key, model } => {
            info!("sending to Gemini ({model})...");
            gemini::stream_llm_response(
                http_client,
                api_key,
                model,
                INTENT_SYSTEM_PROMPT,
                messages,
                Arc::clone(&config.llm_rate_limiter),
            )
            .await
        }
    }
}

/// System prompt that instructs the LLM to emit structured intent JSON instead of POINT tags.
///
/// The LLM must NEVER guess coordinates — all position data is resolved by the frontend.
const INTENT_SYSTEM_PROMPT: &str = r#"you're clicky, a friendly always-on companion that lives in the user's system tray. the user just spoke to you via push-to-talk and you can see their screen(s). your reply will be spoken aloud via text-to-speech, so write the way you'd actually talk. this is an ongoing conversation — you remember everything they've said before.

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
- focus on giving a thorough, useful explanation.
- if you receive multiple screen images, the one labeled "primary focus" is where the cursor is — prioritize that one but reference others if relevant.

element pointing:
you have a small blue triangle cursor that can fly to and point at things on screen. use it whenever pointing would genuinely help the user — if they're asking how to do something, looking for a menu, trying to find a button, or need help navigating an app, point at the relevant element. err on the side of pointing rather than not pointing.

IMPORTANT: you must NEVER guess or output pixel coordinates. instead, when pointing would help, append a JSON block at the very end of your response (after your spoken text), on its own line:

{"label":"<short element description>","action":"click","region_hint":"<top right|center|bottom left|etc>","screen_number":<N or null>}

the label must be a short human-readable description of the UI element (e.g. "Submit button", "Search field", "Settings menu").
action must be one of: "click", "focus", "hover".
region_hint is a rough screen region like "top right", "center", "bottom left", "top left", "top center", "bottom right".
screen_number is the 1-based screen index if the element is on a non-primary screen, otherwise null.

if pointing would NOT help, do not append any JSON block.

examples:
- user asks how to color grade in final cut: "you'll want to open the color inspector — it's right up in the top right area of the toolbar."
  {"label":"Color Inspector button","action":"click","region_hint":"top right","screen_number":null}
- user asks what html is: "html stands for hypertext markup language, it's basically the skeleton of every web page."
- user asks how to commit in xcode: "see that source control menu up top? click that and hit commit."
  {"label":"Source Control menu","action":"click","region_hint":"top left","screen_number":null}"#;

/// Parses a structured intent JSON block from the end of an LLM response.
///
/// Looks for the last `{...}` block containing at least a "label" key.
/// Returns `None` if no valid intent block is found.
fn parse_intent_json(text: &str) -> Option<ElementIntent> {
    // Find the last JSON object in the text — the intent block is always at the end.
    let last_brace = text.rfind('{')?;
    let after = &text[last_brace..];
    let end = after.find('}')? + 1;
    let json_slice = &after[..end];

    let value: serde_json::Value = serde_json::from_str(json_slice).ok()?;
    let label = value.get("label")?.as_str()?.to_string();
    if label.is_empty() {
        return None;
    }
    let action = value
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("click")
        .to_string();
    let region_hint = value
        .get("region_hint")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let screen_number = value
        .get("screen_number")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);

    Some(ElementIntent { label, action, region_hint, screen_number })
}

/// Removes the trailing intent JSON block from the LLM response so TTS doesn't read raw JSON.
fn strip_intent_json(text: &str) -> String {
    if let Some(last_brace) = text.rfind('{') {
        let after = &text[last_brace..];
        if after.contains('}') && after.contains("\"label\"") {
            return text[..last_brace].trim_end().to_string();
        }
    }
    text.trim().to_string()
}

#[cfg(debug_assertions)]
fn log_debug_screenshots(screenshots: &[ScreenshotData], display_infos: &[DisplayInfo]) {
    if std::env::var("CLICKY_DEBUG_SCREENSHOTS").as_deref() != Ok("1") {
        return;
    }

    for (index, screenshot) in screenshots.iter().enumerate() {
        let path = if cfg!(target_os = "windows") {
            format!(
                "{}\\clicky_debug_screenshot_{}.jpg",
                std::env::temp_dir().display(),
                index
            )
        } else {
            format!("/tmp/clicky_debug_screenshot_{index}.jpg")
        };
        if let Err(err) = std::fs::write(&path, &screenshot.jpeg_data) {
            warn!("failed to save debug screenshot: {err}");
        } else {
            info!("debug screenshot saved: {path} (label: {})", screenshot.label);
        }
    }
    for (i, d) in display_infos.iter().enumerate() {
        info!(
            "debug display {i}: origin=({},{}) size={}x{} screenshot={}x{} cursor={}",
            d.global_origin_x,
            d.global_origin_y,
            d.display_width_points,
            d.display_height_points,
            d.screenshot_width_pixels,
            d.screenshot_height_pixels,
            d.is_cursor_display,
        );
    }
}

#[cfg(not(debug_assertions))]
fn log_debug_screenshots(_screenshots: &[ScreenshotData], _display_infos: &[DisplayInfo]) {}

/// Appends `text` to `clicky_llm_log.txt` in the current working directory.
///
/// Uses `OpenOptions::append` so entries accumulate across sessions.
/// Errors are silently ignored — logging must never crash the pipeline.
fn append_to_log(text: &str) {
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("clicky_llm_log.txt")
        .and_then(|mut f| f.write_all(text.as_bytes()));
    if let Err(err) = result {
        warn!("failed to write LLM log: {err}");
    }
}
