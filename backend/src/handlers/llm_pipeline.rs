/// LLM pipeline handler — screenshot + Claude vision + Computer Use + TTS.
///
/// This is the backend equivalent of `src/runtime::run_llm_pipeline`.
use crate::api::{claude, computer_use};
use crate::core::{coordinate_mapper, point_parser};
use crate::error::AppError;
use crate::handlers::tts;
use crate::platform::PlatformInfo;
use crate::screenshot;
use clicky_types::ServerMessage;
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
/// 1. Captures screenshots
/// 2. Calls Claude vision API (streaming)
/// 3. Parses POINT tag
/// 4. Spawns ElevenLabs TTS in parallel with Computer Use
/// 5. Runs Computer Use for precise coordinates
/// 6. Sends `ServerMessage::LlmResponse`
pub async fn run_llm_pipeline(
    http_client: reqwest::Client,
    config: LlmPipelineConfig,
    transcript: String,
    cursor_position: (f32, f32),
    history_exchanges: Vec<(String, String)>,
    platform: PlatformInfo,
    server_tx: mpsc::Sender<ServerMessage>,
) {
    let (cursor_x, cursor_y) = cursor_position;

    // Capture screenshots on a blocking thread (OS calls)
    let capture = match tokio::task::spawn_blocking(move || {
        screenshot::capture_all_screens(cursor_x, cursor_y, &platform)
    })
    .await
    {
        Ok(Ok(capture)) => capture,
        Ok(Err(err)) => {
            error!("screenshot failed: {err}");
            let _ = server_tx.send(AppError::Screenshot(err.to_string()).into_server_message()).await;
            return;
        }
        Err(err) => {
            error!("screenshot task panicked: {err}");
            let _ = server_tx.send(AppError::Internal("screenshot task panicked".into()).into_server_message()).await;
            return;
        }
    };

    log_debug_screenshots(&capture);

    // Build conversation history messages
    let mut messages: Vec<serde_json::Value> = Vec::new();
    for (user_text, assistant_text) in &history_exchanges {
        messages.push(serde_json::json!({"role": "user", "content": user_text}));
        messages.push(serde_json::json!({"role": "assistant", "content": assistant_text}));
    }

    let user_content =
        claude::build_vision_message_content(&transcript, &capture.screenshots);
    messages.push(serde_json::json!({"role": "user", "content": user_content}));

    let model = claude::DEFAULT_CLAUDE_MODEL;
    info!("sending to Claude ({model})...");

    let result = claude::stream_claude_response(
        &http_client,
        &config.anthropic_api_key,
        model,
        claude::COMPANION_VOICE_RESPONSE_SYSTEM_PROMPT,
        messages,
        |_chunk| {}, // future: forward LlmTextDelta events
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

    // Spawn TTS in parallel with Computer Use (matches original behaviour)
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

    // Determine global coordinate — prefer Computer Use over POINT tag
    let computer_use_coordinate = detect_computer_use_coordinate(
        &http_client,
        &capture,
        &config.anthropic_api_key,
        &transcript,
    )
    .await;

    let final_coordinate = if let Some(coord) = computer_use_coordinate {
        Some(coord)
    } else if let Some(ref pointing) = parsed.pointing {
        // Fall back to POINT tag coordinate mapping
        coordinate_mapper::find_target_display(pointing.screen_number, &capture.display_infos)
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
            display_infos: capture.display_infos,
        })
        .await;
}

async fn detect_computer_use_coordinate(
    http_client: &reqwest::Client,
    capture: &screenshot::CaptureResult,
    anthropic_api_key: &str,
    transcript: &str,
) -> Option<(f64, f64)> {
    let cursor_display = capture.display_infos.iter().find(|d| d.is_cursor_display)?;
    let cursor_screenshot = capture
        .screenshots
        .iter()
        .zip(capture.display_infos.iter())
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
fn log_debug_screenshots(capture: &screenshot::CaptureResult) {
    for (index, screenshot) in capture.screenshots.iter().enumerate() {
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
    for (i, d) in capture.display_infos.iter().enumerate() {
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
fn log_debug_screenshots(_capture: &screenshot::CaptureResult) {}
