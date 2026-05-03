/// LLM pipeline handler — vision + Computer Use + TTS.
///
/// Screenshots arrive from the frontend over WebSocket. The backend no longer captures them.
use crate::api::{self, computer_use, gemini, openrouter, Provider};
use crate::core::{coordinate_mapper, point_parser};
use crate::error::AppError;
use crate::handlers::tts;
use clicky_types::{DisplayInfo, ScreenshotData, ServerMessage};
use std::io::Write;
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};
use tracing::{error, info, warn};

pub struct LlmPipelineConfig {
    pub llm_provider: Arc<api::Provider>,
    pub cu_provider: Arc<api::Provider>,
    pub deepgram_api_key: Option<String>,
    pub deepgram_tts_voice: Option<String>,
    pub tts_enabled: bool,
    /// Shared semaphore enforcing the AI provider RPM cap.
    pub llm_rate_limiter: Arc<Semaphore>,
}

/// Runs the full LLM pipeline for a completed transcript.
///
/// LLM and Computer Use fire simultaneously via `tokio::join!`.
/// TTS fires as soon as LLM text is known (overlapping whatever CU time remains).
/// A single `ServerMessage::PipelineReady` is emitted when all three are done.
pub async fn run_llm_pipeline(
    http_client: reqwest::Client,
    config: LlmPipelineConfig,
    transcript: String,
    screenshots: Vec<ScreenshotData>,
    display_infos: Vec<DisplayInfo>,
    history_exchanges: Vec<(String, String)>,
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

    log_debug_screenshots(&screenshots, &display_infos);

    let mut messages: Vec<serde_json::Value> = Vec::new();
    for (user_text, assistant_text) in &history_exchanges {
        messages.push(serde_json::json!({"role": "user", "content": user_text}));
        messages.push(serde_json::json!({"role": "assistant", "content": assistant_text}));
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

    // ── Fire LLM and Computer Use simultaneously ─────────────────────────────
    let llm_future = run_llm_call(&http_client, &config, messages);
    let cu_future = detect_computer_use_coordinate(
        &http_client,
        &screenshots,
        &display_infos,
        Arc::clone(&config.cu_provider),
        &transcript,
        Arc::clone(&config.llm_rate_limiter),
    );

    let (llm_result, cu_result) = tokio::join!(llm_future, cu_future);

    let response_text = match llm_result {
        Ok(text) => text,
        Err(err) => {
            error!("LLM API error: {err}");
            let _ = server_tx.send(err.into_server_message()).await;
            return;
        }
    };

    if response_text.is_empty() {
        warn!("LLM returned empty response, aborting pipeline");
        let _ = server_tx
            .send(AppError::Llm("empty response from model".into()).into_server_message())
            .await;
        return;
    }

    append_to_log(&format!("[LLM] transcript: {transcript}\n[LLM] response: {response_text}\n"));

    let parsed = point_parser::parse_claude_response(&response_text);

    // POINT-tag fallback coordinate — used when CU found nothing.
    let point_tag_coordinate =
        parsed.pointing.as_ref().and_then(|pointing| {
            coordinate_mapper::find_target_display(pointing.screen_number, &display_infos).map(
                |display| {
                    let global = coordinate_mapper::map_screenshot_pixels_to_global(
                        pointing.screenshot_pixel_x,
                        pointing.screenshot_pixel_y,
                        display,
                    );
                    (global.x, global.y)
                },
            )
        });

    // ── TTS fires after LLM text is known; CU is typically still running in
    // join! above and is already done by this point (CU wall-clock > LLM).
    // If CU finishes first, this await is effectively free.
    let mp3_bytes = if config.tts_enabled && !parsed.spoken_text.is_empty() {
        if let Some(ref api_key) = config.deepgram_api_key {
            tts::synthesize_speech_bytes(
                &http_client,
                api_key,
                config.deepgram_tts_voice.as_deref(),
                &parsed.spoken_text,
            )
            .await
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    let coordinate = cu_result.or(point_tag_coordinate);
    if let Some((x, y)) = coordinate {
        append_to_log(&format!("[CU] coordinate: ({x:.1}, {y:.1})\n"));
    }

    // When the LLM returned nothing useful (empty spoken text) and no coordinate
    // was found either, give the user audible feedback instead of silent Idle.
    let final_spoken = if parsed.spoken_text.is_empty() {
        "I didn't quite get you".to_owned()
    } else {
        parsed.spoken_text.clone()
    };

    let _ = server_tx
        .send(ServerMessage::PipelineReady {
            spoken_text: final_spoken,
            mp3_bytes,
            coordinate,
            display_infos,
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
                openrouter::COMPANION_VOICE_RESPONSE_SYSTEM_PROMPT,
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
                openrouter::COMPANION_VOICE_RESPONSE_SYSTEM_PROMPT,
                messages,
                Arc::clone(&config.llm_rate_limiter),
            )
            .await
        }
    }
}

async fn detect_computer_use_coordinate(
    http_client: &reqwest::Client,
    screenshots: &[ScreenshotData],
    display_infos: &[DisplayInfo],
    cu_provider: Arc<Provider>,
    transcript: &str,
    rate_limiter: Arc<Semaphore>,
) -> Option<(f64, f64)> {
    let cursor_display = display_infos.iter().find(|d| d.is_cursor_display)?;
    let cursor_screenshot = screenshots
        .iter()
        .zip(display_infos.iter())
        .find(|(_, d)| d.is_cursor_display)
        .map(|(s, _)| s)?;

    info!("running Computer Use for precise coordinate detection");

    let coordinate = computer_use::detect_element_location(
        http_client,
        cu_provider,
        &cursor_screenshot.jpeg_data,
        transcript,
        cursor_display.display_width_points,
        cursor_display.display_height_points,
        rate_limiter,
    )
    .await?;

    let global_x = cursor_display.global_origin_x + coordinate.display_local_x;
    let global_y = cursor_display.global_origin_y + coordinate.display_local_y;
    info!("Computer Use global coordinate: ({global_x:.1}, {global_y:.1})");
    Some((global_x, global_y))
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
