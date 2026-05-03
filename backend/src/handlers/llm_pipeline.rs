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
/// 1. Uses screenshots provided by the frontend.
/// 2. Calls the configured LLM provider (OpenRouter or Gemini) via streaming.
/// 3. Parses POINT tag from response.
/// 4. Sends `ServerMessage::LlmResponse` immediately (coordinate = POINT fallback or None).
/// 5. Spawns Deepgram TTS in parallel.
/// 6. Spawns Computer Use in parallel; sends `ServerMessage::ComputerUseCoordinate` when done.
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

    let result = match config.llm_provider.as_ref() {
        Provider::OpenRouter { api_key, model } => {
            info!("sending to OpenRouter ({model})...");
            openrouter::stream_llm_response(
                &http_client,
                api_key,
                model,
                openrouter::COMPANION_VOICE_RESPONSE_SYSTEM_PROMPT,
                messages,
                Arc::clone(&config.llm_rate_limiter),
                |_chunk| {},
            )
            .await
        }
        Provider::Gemini { api_key, model } => {
            info!("sending to Gemini ({model})...");
            gemini::stream_llm_response(
                &http_client,
                api_key,
                model,
                openrouter::COMPANION_VOICE_RESPONSE_SYSTEM_PROMPT,
                messages,
                Arc::clone(&config.llm_rate_limiter),
                |_chunk| {},
            )
            .await
        }
    };

    let response_text = match result {
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

    // Compute the POINT-tag fallback coordinate (cheap, synchronous).
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

    // Send LlmResponse immediately so the frontend transitions to Responding and TTS plays
    // without waiting for the Computer Use round-trip.
    let _ = server_tx
        .send(ServerMessage::LlmResponse {
            spoken_text: parsed.spoken_text.clone(),
            coordinate: point_tag_coordinate,
            display_infos: display_infos.clone(),
        })
        .await;

    // Spawn TTS in parallel — fires alongside Computer Use.
    if config.tts_enabled && !parsed.spoken_text.is_empty() {
        if let Some(ref deepgram_api_key) = config.deepgram_api_key {
            let tts_client = http_client.clone();
            let tts_key = deepgram_api_key.clone();
            let tts_voice = config.deepgram_tts_voice.clone();
            let tts_text = parsed.spoken_text.clone();
            let tts_tx = server_tx.clone();

            tokio::spawn(async move {
                tts::run_tts(&tts_client, &tts_key, tts_voice.as_deref(), tts_text, tts_tx).await;
            });
        }
    }

    // Spawn Computer Use in a separate task so it never blocks TTS or LlmResponse delivery.
    // If it finds a more precise coordinate it sends ComputerUseCoordinate to the frontend.
    {
        let cu_client = http_client.clone();
        let cu_screenshots = screenshots.clone();
        let cu_display_infos = display_infos.clone();
        let cu_provider = Arc::clone(&config.cu_provider);
        let cu_transcript = transcript.clone();
        let cu_rate_limiter = Arc::clone(&config.llm_rate_limiter);
        let cu_tx = server_tx.clone();

        tokio::spawn(async move {
            let result = detect_computer_use_coordinate(
                &cu_client,
                &cu_screenshots,
                &cu_display_infos,
                cu_provider,
                &cu_transcript,
                cu_rate_limiter,
            )
            .await;

            if let Some((x, y)) = result {
                append_to_log(&format!("[CU] coordinate: ({x:.1}, {y:.1})\n"));
                let _ = cu_tx
                    .send(ServerMessage::ComputerUseCoordinate { x, y })
                    .await;
            }
        });
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
