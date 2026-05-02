/// LLM pipeline handler — Claude vision + Computer Use + TTS.
///
/// Screenshots arrive from the frontend over WebSocket. The backend no longer captures them.
use crate::api::{claude, computer_use};
use crate::core::{coordinate_mapper, point_parser};
use crate::error::AppError;
use crate::handlers::tts;
use clicky_types::{DisplayInfo, ScreenshotData, ServerMessage};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub struct LlmPipelineConfig {
    pub anthropic_api_key: String,
    pub elevenlabs_api_key: Option<String>,
    pub elevenlabs_voice_id: Option<String>,
    pub tts_enabled: bool,
}

/// Runs the full LLM pipeline for a completed transcript.
///
/// 1. Uses screenshots provided by the frontend
/// 2. Calls Claude vision API (streaming)
/// 3. Parses POINT tag
/// 4. Spawns ElevenLabs TTS in parallel with Computer Use
/// 5. Runs Computer Use for precise coordinates
/// 6. Sends `ServerMessage::LlmResponse`
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
            .send(AppError::Screenshot("no screenshots received from frontend".into()).into_server_message())
            .await;
        return;
    }

    log_debug_screenshots(&screenshots, &display_infos);

    let mut messages: Vec<serde_json::Value> = Vec::new();
    for (user_text, assistant_text) in &history_exchanges {
        messages.push(serde_json::json!({"role": "user", "content": user_text}));
        messages.push(serde_json::json!({"role": "assistant", "content": assistant_text}));
    }

    let user_content =
        claude::build_vision_message_content(&transcript, &screenshots);
    messages.push(serde_json::json!({"role": "user", "content": user_content}));

    let model = claude::DEFAULT_CLAUDE_MODEL;
    info!("sending to Claude ({model})...");

    let result = claude::stream_claude_response(
        &http_client,
        &config.anthropic_api_key,
        model,
        claude::COMPANION_VOICE_RESPONSE_SYSTEM_PROMPT,
        messages,
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
        if let Some(ref elevenlabs_api_key) = config.elevenlabs_api_key {
            let tts_client = http_client.clone();
            let tts_key = elevenlabs_api_key.clone();
            let tts_voice = config.elevenlabs_voice_id.clone();
            let tts_text = parsed.spoken_text.clone();
            let tts_tx = server_tx.clone();

            tokio::spawn(async move {
                tts::run_tts(
                    &tts_client,
                    &tts_key,
                    tts_voice.as_deref(),
                    tts_text,
                    tts_tx,
                )
                .await;
            });
        }
    }

    let computer_use_coordinate = detect_computer_use_coordinate(
        &http_client,
        &screenshots,
        &display_infos,
        &config.anthropic_api_key,
        &transcript,
    )
    .await;

    let final_coordinate = if let Some(coord) = computer_use_coordinate {
        Some(coord)
    } else if let Some(ref pointing) = parsed.pointing {
        coordinate_mapper::find_target_display(pointing.screen_number, &display_infos)
            .map(|display| {
                let global = coordinate_mapper::map_screenshot_pixels_to_global(
                    pointing.screenshot_pixel_x,
                    pointing.screenshot_pixel_y,
                    display,
                );
                (global.x, global.y)
            })
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
    anthropic_api_key: &str,
    transcript: &str,
) -> Option<(f64, f64)> {
    let cursor_display = display_infos.iter().find(|d| d.is_cursor_display)?;
    let cursor_screenshot = screenshots
        .iter()
        .zip(display_infos.iter())
        .find(|(_, d)| d.is_cursor_display)
        .map(|(s, _)| s)?;

    info!("running Computer Use API for precise coordinate detection");

    let coordinate = computer_use::detect_element_location(
        http_client,
        anthropic_api_key,
        &cursor_screenshot.jpeg_data,
        transcript,
        cursor_display.display_width_points,
        cursor_display.display_height_points,
    )
    .await?;

    let global_x = cursor_display.global_origin_x + coordinate.display_local_x;
    let global_y = cursor_display.global_origin_y + coordinate.display_local_y;
    info!("Computer Use global coordinate: ({global_x:.1}, {global_y:.1})");
    Some((global_x, global_y))
}

#[cfg(debug_assertions)]
fn log_debug_screenshots(screenshots: &[ScreenshotData], display_infos: &[DisplayInfo]) {
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
            d.global_origin_x, d.global_origin_y,
            d.display_width_points, d.display_height_points,
            d.screenshot_width_pixels, d.screenshot_height_pixels,
            d.is_cursor_display,
        );
    }
}

#[cfg(not(debug_assertions))]
fn log_debug_screenshots(_screenshots: &[ScreenshotData], _display_infos: &[DisplayInfo]) {}
