/// Vision-based UI element coordinate detection via Nemotron Omni on OpenRouter.
///
/// Replaces the former Anthropic `computer_20251124` tool-use protocol with a
/// structured JSON prompt. Nemotron's vision reasoning is purpose-built for GUI
/// tasks; we send the screenshot and ask it to return a JSON coordinate object.
///
/// Public API (`ComputerUseCoordinate`, `detect_element_location`) is unchanged
/// so `handlers/llm_pipeline.rs` needs no code changes.
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

const COMPUTER_USE_MODEL: &str = "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free";
const OPENROUTER_API_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

/// Nemotron supports full HD natively — added alongside the legacy CU resolutions.
const SUPPORTED_RESOLUTIONS: [(u32, u32, f64); 4] = [
    (1024, 768, 1024.0 / 768.0),   // 4:3
    (1280, 800, 1280.0 / 800.0),   // 16:10
    (1366, 768, 1366.0 / 768.0),   // ~16:9
    (1920, 1080, 1920.0 / 1080.0), // 16:9 full HD
];

/// Detected coordinate in display-local logical points.
#[derive(Debug, Clone)]
pub struct ComputerUseCoordinate {
    pub display_local_x: f64,
    pub display_local_y: f64,
}

/// Detected element from the JSON response.
#[derive(Deserialize)]
struct ElementCoordinate {
    x: Option<f64>,
    y: Option<f64>,
    #[allow(dead_code)]
    label: Option<String>,
}

/// Detects the screen location of a UI element using Nemotron Omni vision reasoning.
///
/// Returns `None` if no element was detected (conceptual question) or on any error.
/// Acquires one permit from `rate_limiter` for the duration of the request.
pub async fn detect_element_location(
    http_client: &Client,
    api_key: &str,
    screenshot_jpeg: &[u8],
    user_question: &str,
    display_width_points: f64,
    display_height_points: f64,
    rate_limiter: Arc<Semaphore>,
) -> Option<ComputerUseCoordinate> {
    let (cu_width, cu_height) =
        best_resolution(display_width_points, display_height_points);

    info!(
        "Computer Use: display {:.0}x{:.0} (ratio {:.3}), using resolution {}x{}",
        display_width_points,
        display_height_points,
        display_width_points / display_height_points,
        cu_width,
        cu_height,
    );

    let resized_jpeg = resize_screenshot(screenshot_jpeg, cu_width, cu_height)?;

    let raw_coordinate = call_vision_api(
        http_client,
        api_key,
        &resized_jpeg,
        user_question,
        cu_width,
        cu_height,
        rate_limiter,
    )
    .await?;

    let clamped_x = raw_coordinate.0.clamp(0.0, cu_width as f64);
    let clamped_y = raw_coordinate.1.clamp(0.0, cu_height as f64);

    let scaled_x = (clamped_x / cu_width as f64) * display_width_points;
    let scaled_y = (clamped_y / cu_height as f64) * display_height_points;

    info!(
        "Computer Use: mapped ({:.0}, {:.0}) in {}x{} → ({:.1}, {:.1}) in {:.0}x{:.0}",
        clamped_x, clamped_y, cu_width, cu_height,
        scaled_x, scaled_y, display_width_points, display_height_points,
    );

    Some(ComputerUseCoordinate {
        display_local_x: scaled_x,
        display_local_y: scaled_y,
    })
}

fn best_resolution(display_width: f64, display_height: f64) -> (u32, u32) {
    let display_ratio = display_width / display_height.max(1.0);
    let mut best_width = 1280u32;
    let mut best_height = 800u32;
    let mut smallest_diff = f64::MAX;

    for &(width, height, ratio) in &SUPPORTED_RESOLUTIONS {
        let diff = (display_ratio - ratio).abs();
        if diff < smallest_diff {
            smallest_diff = diff;
            best_width = width;
            best_height = height;
        }
    }

    (best_width, best_height)
}

async fn call_vision_api(
    http_client: &Client,
    api_key: &str,
    resized_jpeg: &[u8],
    user_question: &str,
    declared_width: u32,
    declared_height: u32,
    rate_limiter: Arc<Semaphore>,
) -> Option<(f64, f64)> {
    let base64_screenshot =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, resized_jpeg);

    let prompt = format!(
        "Look at the screenshot ({}x{} pixels). \
         The user asked: \"{user_question}\"\n\n\
         If there is a specific UI element (button, link, menu item, text field, icon, etc.) \
         that the user should interact with or is asking about, respond with ONLY valid JSON \
         (no markdown, no explanation):\n\
         {{\"x\": <integer_pixels>, \"y\": <integer_pixels>, \"label\": \"<short label>\"}}\n\n\
         If the question is purely conceptual and there is no specific element to point to, \
         respond with ONLY valid JSON:\n\
         {{\"x\": null, \"y\": null, \"label\": \"none\"}}",
        declared_width, declared_height,
    );

    let payload_mb = (base64_screenshot.len() as f64 / 1_048_576.0) * 1.35;
    debug!("Computer Use: sending ~{payload_mb:.1}MB (declared {declared_width}x{declared_height})");

    let request_body = json!({
        "model": COMPUTER_USE_MODEL,
        "max_tokens": 64,
        "messages": [
            {
                "role": "user",
                "content": [
                    {
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:image/jpeg;base64,{base64_screenshot}"),
                        }
                    },
                    { "type": "text", "text": prompt }
                ]
            }
        ]
    });

    // Acquire rate-limit permit
    let _permit = match rate_limiter.acquire().await {
        Ok(p) => p,
        Err(_) => {
            warn!("Computer Use: rate limiter semaphore closed");
            return None;
        }
    };

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
            warn!("Computer Use: request failed: {err}");
            return None;
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        warn!(
            "Computer Use: API error {status}: {}",
            &body[..body.len().min(200)]
        );
        return None;
    }

    let body: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(err) => {
            warn!("Computer Use: failed to parse response: {err}");
            return None;
        }
    };

    let raw_text = body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();

    debug!("Computer Use raw response: {raw_text}");
    parse_coordinate_from_text(&raw_text)
}

/// Strips optional markdown fences and parses the JSON coordinate object.
fn parse_coordinate_from_text(text: &str) -> Option<(f64, f64)> {
    // Strip ```json ... ``` or ``` ... ``` fences if present
    let stripped = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let coordinate: ElementCoordinate = match serde_json::from_str(stripped) {
        Ok(c) => c,
        Err(err) => {
            warn!("Computer Use: JSON parse error: {err} — text: {stripped}");
            return None;
        }
    };

    match (coordinate.x, coordinate.y) {
        (Some(x), Some(y)) => {
            info!("Computer Use: raw coordinate ({x:.0}, {y:.0})");
            Some((x, y))
        }
        _ => {
            info!("Computer Use: no specific element detected (conceptual question)");
            None
        }
    }
}

fn resize_screenshot(jpeg_data: &[u8], target_width: u32, target_height: u32) -> Option<Vec<u8>> {
    let img = image::load_from_memory(jpeg_data).ok()?.to_rgba8();

    let resized = image::imageops::resize(
        &img,
        target_width,
        target_height,
        image::imageops::FilterType::Lanczos3,
    );

    let mut jpeg_buf = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
        std::io::Cursor::new(&mut jpeg_buf),
        85,
    );
    encoder.encode_image(&resized).ok()?;

    Some(jpeg_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_resolution_for_16_10_display() {
        let (w, h) = best_resolution(2560.0, 1600.0);
        assert_eq!((w, h), (1280, 800));
    }

    #[test]
    fn best_resolution_for_16_9_display() {
        let (w, h) = best_resolution(1920.0, 1080.0);
        assert_eq!((w, h), (1920, 1080));
    }

    #[test]
    fn best_resolution_for_4_3_display() {
        let (w, h) = best_resolution(1024.0, 768.0);
        assert_eq!((w, h), (1024, 768));
    }

    #[test]
    fn parse_coordinate_json() {
        let text = r#"{"x": 640, "y": 400, "label": "save button"}"#;
        let coord = parse_coordinate_from_text(text).unwrap();
        assert_eq!(coord, (640.0, 400.0));
    }

    #[test]
    fn parse_coordinate_with_markdown_fence() {
        let text = "```json\n{\"x\": 320, \"y\": 200, \"label\": \"menu\"}\n```";
        let coord = parse_coordinate_from_text(text).unwrap();
        assert_eq!(coord, (320.0, 200.0));
    }

    #[test]
    fn parse_null_coordinate() {
        let text = r#"{"x": null, "y": null, "label": "none"}"#;
        assert!(parse_coordinate_from_text(text).is_none());
    }
}
