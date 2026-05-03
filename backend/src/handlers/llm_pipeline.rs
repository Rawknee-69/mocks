/// LLM pipeline handler — OpenRouter vision + Computer Use + TTS.
///
/// Screenshots arrive from the frontend over WebSocket. The backend no longer captures them.
use crate::api::{computer_use, openrouter};
use crate::core::{coordinate_mapper, point_parser};
use crate::error::AppError;
use crate::handlers::tts;
use clicky_types::{DisplayInfo, ScreenshotData, ServerMessage};
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};
use tracing::{error, info, warn};

pub struct LlmPipelineConfig {
    pub openrouter_api_key: String,
    pub deepgram_api_key: Option<String>,
    pub deepgram_tts_voice: Option<String>,
    pub tts_enabled: bool,
    /// Shared 20-permit semaphore enforcing the OpenRouter RPM cap.
    pub llm_rate_limiter: Arc<Semaphore>,
}

/// Runs the full LLM pipeline for a completed transcript.
///
/// 1. Uses screenshots provided by the frontend.
/// 2. Calls OpenRouter vision API (streaming, Nemotron Omni).
/// 3. Parses POINT tag from response.
/// 4. Spawns Deepgram TTS in parallel with Computer Use coordinate detection.
/// 5. Runs Computer Use for precise coordinates.
/// 6. Sends `ServerMessage::LlmResponse`.
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

    let user_content = openrouter::build_vision_message_content(&transcript, &screenshots);
    messages.push(serde_json::json!({"role": "user", "content": user_content}));

    let model = openrouter::DEFAULT_OPENROUTER_MODEL;
    info!("sending to OpenRouter ({model})...");

    let result = openrouter::stream_llm_response(
        &http_client,
        &config.openrouter_api_key,
        model,
        openrouter::COMPANION_VOICE_RESPONSE_SYSTEM_PROMPT,
        messages,
        Arc::clone(&config.llm_rate_limiter),
        |_chunk| {},
    )
    .await;

    let response_text = match result {
        Ok(text) => text,
        Err(err) => {
            error!("LLM API error: {err}");
            let _ = server_tx.send(err.into_server_message()).await;
            return;
        }
    };

    let parsed = point_parser::parse_claude_response(&response_text);

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

    let computer_use_coordinate = detect_computer_use_coordinate(
        &http_client,
        &screenshots,
        &display_infos,
        &config.openrouter_api_key,
        &transcript,
        Arc::clone(&config.llm_rate_limiter),
    )
    .await;

    let final_coordinate = if let Some(coord) = computer_use_coordinate {
        Some(coord)
    } else if let Some(ref pointing) = parsed.pointing {
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
    } else {
        None
    };

    let _ = server_tx
        .send(ServerMessage::LlmResponse {
            spoken_text: parsed.spoken_text,
            coordinate: final_coordinate,
            display_infos,
        })
        .await;
}

async fn detect_computer_use_coordinate(
    http_client: &reqwest::Client,
    screenshots: &[ScreenshotData],
    display_infos: &[DisplayInfo],
    openrouter_api_key: &str,
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
        openrouter_api_key,
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
