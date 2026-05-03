//! Vision-based UI element coordinate detection — two-pass, pixel-precise.
//!
//! Pass 1 – coarse:  full screenshot at model resolution → approximate location.
//! Pass 2 – fine:    25% zoom crop centred on pass-1 result → pixel-precise location.
//!
//! Architectural invariant
//! ───────────────────────
//! Prompts are built BEFORE entering the retry / network layer.
//! `call_with_retry` and `call_once_*` accept a pre-built `prompt: &str` and
//! send it verbatim.  They never call `build_prompt` themselves.
//! This prevents the double-wrap bug where a full prompt was re-embedded inside
//! another prompt's "User request:" field.

use crate::api::Provider;
use std::{io::Cursor, sync::Arc, time::Duration};

use base64::{engine::general_purpose::STANDARD, Engine as _};
#[allow(unused_imports)]
use image::{imageops::FilterType, ImageEncoder as _};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

// ─── Constants ────────────────────────────────────────────────────────────────

pub const DEFAULT_CU_MODEL: &str = "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free";
const OPENROUTER_API_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";

/// Longest edge sent to the model — enough resolution for element detection
/// while keeping base64 payloads reasonable.
const MAX_LONG_EDGE: u32 = 1_920;

/// Pass-2 crop covers this fraction of each display dimension.
/// 0.25 → 25% of display → ~4× zoom → ~4× finer coordinate precision.
const ZOOM_FRACTION: f64 = 0.25;

const JPEG_QUALITY: u8 = 85;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_ATTEMPTS: u32 = 3;
const BACKOFF_BASE_MS: u64 = 600;

// ─── Public types ─────────────────────────────────────────────────────────────

/// Detected coordinate in display-local logical points.
#[derive(Debug, Clone, PartialEq)]
pub struct ComputerUseCoordinate {
    pub display_local_x: f64,
    pub display_local_y: f64,
}

// ─── Internal types ───────────────────────────────────────────────────────────

/// Pixel dimensions of the image that was sent to the model.
/// A named struct makes it impossible to accidentally swap model-space and
/// display-space when projecting coordinates back.
#[derive(Debug, Clone, Copy)]
struct ModelDims {
    width: u32,
    height: u32,
}

impl ModelDims {
    /// Map a model-space pixel to the given logical coordinate space.
    #[inline]
    fn unproject(self, mx: f64, my: f64, target_w: f64, target_h: f64) -> (f64, f64) {
        (
            mx / self.width as f64 * target_w,
            my / self.height as f64 * target_h,
        )
    }
}

/// Crop region expressed in display logical-point coordinates.
#[derive(Debug, Clone, Copy)]
struct Crop {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Crop {
    /// Build a crop centred on `(cx, cy)` covering `fraction` of each display
    /// dimension.  Clamps so the box never overflows the display.
    fn centred_on(cx: f64, cy: f64, display_w: f64, display_h: f64, fraction: f64) -> Self {
        let w = (display_w * fraction).max(32.0);
        let h = (display_h * fraction).max(32.0);
        let max_x = (display_w - w).max(0.0);
        let max_y = (display_h - h).max(0.0);
        let x = (cx - w / 2.0).clamp(0.0, max_x);
        let y = (cy - h / 2.0).clamp(0.0, max_y);
        // Final guard: crop width/height can never exceed remaining display space.
        let w = w.min(display_w - x);
        let h = h.min(display_h - y);
        Self { x, y, w, h }
    }

    /// Unproject a model-space coordinate back to display space through this crop.
    /// model-px → crop-local logical pts → display logical pts
    #[inline]
    fn unproject_model(&self, mx: f64, my: f64, dims: ModelDims) -> (f64, f64) {
        let local_x = mx / dims.width as f64 * self.w;
        let local_y = my / dims.height as f64 * self.h;
        (self.x + local_x, self.y + local_y)
    }
}

/// Deserialises a JSON number **or** a numeric string for the x / y fields.
///
/// Reasoning models occasionally emit coordinates as `"640"` instead of `640`
/// after their chain-of-thought block.
#[derive(Debug, Clone, Copy)]
struct FlexF64(f64);

impl<'de> Deserialize<'de> for FlexF64 {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{self, Unexpected, Visitor};
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = FlexF64;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a finite number or numeric string")
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<FlexF64, E> {
                Ok(FlexF64(v as f64))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<FlexF64, E> {
                Ok(FlexF64(v as f64))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<FlexF64, E> {
                Ok(FlexF64(v))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<FlexF64, E> {
                v.trim()
                    .trim_end_matches('%')
                    .parse::<f64>()
                    .map(FlexF64)
                    .map_err(|_| E::invalid_value(Unexpected::Str(v), &"a numeric string"))
            }
        }
        d.deserialize_any(V)
    }
}

/// Wire format the model is asked to return.
#[derive(Deserialize, Debug)]
struct CoordJson {
    x: Option<FlexF64>,
    y: Option<FlexF64>,
    #[allow(dead_code)]
    label: Option<String>,
}

/// Three-state result from a single JSON parse attempt.
enum ParseResult {
    Found(f64, f64),
    /// Model returned `{"x":null,"y":null}` — element genuinely absent.
    /// Callers MUST NOT retry on this variant.
    NoElement,
    /// Unparseable response — a retry may succeed.
    ParseFailed,
}

// ─── Public entry point ───────────────────────────────────────────────────────

/// Detects the display-space location of the UI element described by
/// `user_question`.
///
/// Returns `None` when:
/// - The question is purely conceptual (no element to interact with).
/// - The model could not be reached after all retries.
/// - The screenshot could not be decoded or re-encoded.
pub async fn detect_element_location(
    http_client: &Client,
    provider: Arc<Provider>,
    screenshot_jpeg: &[u8],
    user_question: &str,
    display_width_points: f64,
    display_height_points: f64,
    rate_limiter: Arc<Semaphore>,
) -> Option<ComputerUseCoordinate> {
    // ── Pass 1: coarse location on the full screenshot ────────────────────

    let full_dims = compute_model_dims(display_width_points, display_height_points);

    info!(
        "ComputerUse pass 1 — display {:.0}×{:.0} → model {}×{}",
        display_width_points, display_height_points, full_dims.width, full_dims.height,
    );

    let full_jpeg = resize_screenshot(screenshot_jpeg, full_dims)?;

    // Build the prompt HERE — never inside the network layer.
    let pass1_prompt = build_prompt(user_question, full_dims.width, full_dims.height);

    let (raw1_x, raw1_y) = call_with_retry(
        http_client,
        &provider,
        &full_jpeg,
        &pass1_prompt,
        Arc::clone(&rate_limiter),
        1,
    )
    .await?;

    let (raw1_x, raw1_y) = denormalise_if_needed(raw1_x, raw1_y, full_dims);
    let (mx1, my1) = clamp_to_dims(raw1_x, raw1_y, full_dims);
    let (coarse_dx, coarse_dy) =
        full_dims.unproject(mx1, my1, display_width_points, display_height_points);

    info!("ComputerUse pass 1 → ({coarse_dx:.1}, {coarse_dy:.1}) display pts");

    // ── Pass 2: fine location in a 4× zoom crop ───────────────────────────

    let crop = Crop::centred_on(
        coarse_dx,
        coarse_dy,
        display_width_points,
        display_height_points,
        ZOOM_FRACTION,
    );

    // Always upscale the crop to MAX_LONG_EDGE — that's the entire point of pass 2.
    let zoom_dims = compute_model_dims_allow_upscale(crop.w, crop.h);

    let zoom_jpeg = match crop_and_resize(
        screenshot_jpeg,
        &crop,
        display_width_points,
        display_height_points,
        zoom_dims,
    ) {
        Some(j) => j,
        None => {
            warn!("ComputerUse: zoom crop failed — using pass-1 result");
            return Some(ComputerUseCoordinate {
                display_local_x: coarse_dx,
                display_local_y: coarse_dy,
            });
        }
    };

    info!(
        "ComputerUse pass 2 — crop ({:.0},{:.0} {:.0}×{:.0}) → model {}×{} ({:.1}× zoom)",
        crop.x, crop.y, crop.w, crop.h,
        zoom_dims.width, zoom_dims.height,
        zoom_dims.width as f64 / crop.w,
    );

    // Build the pass-2 prompt HERE with the raw user question.
    // The crop context is embedded in the prompt body — not in the question arg.
    let pass2_prompt = build_pass2_prompt(
        user_question,
        zoom_dims.width,
        zoom_dims.height,
        &crop,
        display_width_points,
        display_height_points,
    );

    // Pass 2 failure is non-fatal — fall back to the pass-1 coarse result.
    let (final_dx, final_dy) = match call_with_retry(
        http_client,
        &provider,
        &zoom_jpeg,
        &pass2_prompt,
        rate_limiter,
        2,
    )
    .await
    {
        Some((raw2_x, raw2_y)) => {
            let (raw2_x, raw2_y) = denormalise_if_needed(raw2_x, raw2_y, zoom_dims);
            let (mx2, my2) = clamp_to_dims(raw2_x, raw2_y, zoom_dims);
            let (fine_dx, fine_dy) = crop.unproject_model(mx2, my2, zoom_dims);
            info!("ComputerUse pass 2 → ({fine_dx:.1}, {fine_dy:.1}) display pts");
            (fine_dx, fine_dy)
        }
        None => {
            warn!("ComputerUse: pass 2 returned no element — keeping pass-1 result");
            (coarse_dx, coarse_dy)
        }
    };

    Some(ComputerUseCoordinate {
        display_local_x: final_dx,
        display_local_y: final_dy,
    })
}

// ─── Resolution ───────────────────────────────────────────────────────────────

/// Compute model image dimensions with:
///
/// 1. A **single uniform scale factor** for both axes — the aspect ratio is
///    preserved exactly and the inverse transform has zero compounding error.
/// 2. Long edge capped at `MAX_LONG_EDGE` (never upscales pass-1 images).
/// 3. Even pixel counts (JPEG MCU alignment).
fn compute_model_dims(w: f64, h: f64) -> ModelDims {
    let w = w.max(1.0);
    let h = h.max(1.0);
    let scale = (MAX_LONG_EDGE as f64 / w.max(h)).min(1.0);
    ModelDims {
        width: align2(((w * scale).round() as u32).max(2)),
        height: align2(((h * scale).round() as u32).max(2)),
    }
}

/// Like `compute_model_dims` but always upscales to `MAX_LONG_EDGE`.
/// Used for pass-2 crops which are intentionally smaller than the display.
fn compute_model_dims_allow_upscale(w: f64, h: f64) -> ModelDims {
    let w = w.max(1.0);
    let h = h.max(1.0);
    let scale = MAX_LONG_EDGE as f64 / w.max(h); // No .min(1.0)
    ModelDims {
        width: align2(((w * scale).round() as u32).max(2)),
        height: align2(((h * scale).round() as u32).max(2)),
    }
}

#[inline]
fn align2(v: u32) -> u32 {
    (v + 1) & !1
}

// ─── Image operations ─────────────────────────────────────────────────────────

fn resize_screenshot(jpeg: &[u8], dims: ModelDims) -> Option<Vec<u8>> {
    let img = image::load_from_memory(jpeg)
        .map_err(|e| warn!("ComputerUse: screenshot decode failed: {e}"))
        .ok()?
        .to_rgb8(); // JPEG has no alpha — to_rgba8() is wrong here
    encode_jpeg(&image::imageops::resize(&img, dims.width, dims.height, FilterType::Lanczos3))
}

/// Crop a region from the original screenshot mapped through its native pixel
/// dimensions (correct for any pixel density: 1×, 2× Retina, etc.), then
/// resize to `target_dims`.
fn crop_and_resize(
    screenshot_jpeg: &[u8],
    crop: &Crop,
    display_w: f64,
    display_h: f64,
    target_dims: ModelDims,
) -> Option<Vec<u8>> {
    let img = image::load_from_memory(screenshot_jpeg)
        .map_err(|e| warn!("ComputerUse: screenshot decode (crop) failed: {e}"))
        .ok()?
        .to_rgb8();

    let (iw, ih) = img.dimensions();

    // Map display logical-point crop → screenshot native pixels.
    let px = (crop.x / display_w * iw as f64).round() as u32;
    let py = (crop.y / display_h * ih as f64).round() as u32;
    let pw = (crop.w / display_w * iw as f64).ceil() as u32;
    let ph = (crop.h / display_h * ih as f64).ceil() as u32;

    let px = px.min(iw.saturating_sub(1));
    let py = py.min(ih.saturating_sub(1));
    let pw = pw.min(iw - px).max(1);
    let ph = ph.min(ih - py).max(1);

    if pw < 2 || ph < 2 {
        warn!("ComputerUse: crop too small ({pw}×{ph} px) — skipping pass 2");
        return None;
    }

    debug!("ComputerUse: crop native pixels ({px},{py} {pw}×{ph}) from {iw}×{ih}");

    let cropped = image::imageops::crop_imm(&img, px, py, pw, ph).to_image();
    encode_jpeg(&image::imageops::resize(
        &cropped,
        target_dims.width,
        target_dims.height,
        FilterType::Lanczos3,
    ))
}

fn encode_jpeg(img: &image::RgbImage) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(Cursor::new(&mut buf), JPEG_QUALITY)
        .encode_image(img)
        .map_err(|e| warn!("ComputerUse: JPEG encode failed: {e}"))
        .ok()?;
    debug!(
        "ComputerUse: encoded JPEG {}×{} → {:.1} KB",
        img.width(), img.height(), buf.len() as f64 / 1024.0,
    );
    Some(buf)
}

// ─── Coordinate utilities ─────────────────────────────────────────────────────

/// Detects and corrects non-pixel coordinate spaces returned by some models.
///
/// Strategy 1 — [0, 1] normalised:
///   If both values are in [0, 1] and at least one is non-integer (so we
///   can't confuse pixel-0 / pixel-1 with a normalised coord), scale to pixels.
///
/// Strategy 2 — Gemini [0, 1000] bounding-box space:
///   Gemini sometimes returns coordinates in a virtual 0-1000 grid regardless
///   of actual image size.  The definitive signal is that one or both values
///   EXCEED the model image's actual pixel bounds — genuine pixel coords can
///   never exceed the image.  We use bounds-exceedance rather than an
///   `model_is_small` flag, which was prone to false positives on displays
///   narrower than 1000 px (e.g. 800×600 returning (640, 400)).
fn denormalise_if_needed(x: f64, y: f64, dims: ModelDims) -> (f64, f64) {
    // ── Strategy 1: [0, 1] unit normalisation ────────────────────────────
    let both_in_unit = (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y);
    let are_integer_pixels =
        x.fract() == 0.0 && y.fract() == 0.0 && dims.width > 10 && dims.height > 10;

    if both_in_unit && !are_integer_pixels {
        info!(
            "ComputerUse: [0,1] normalised ({x:.4}, {y:.4}) → scaling to {}×{}",
            dims.width, dims.height,
        );
        return (x * dims.width as f64, y * dims.height as f64);
    }

    // ── Strategy 2: [0, 1000] Gemini bounding-box space ──────────────────
    // Only apply when the coord exceeds actual image bounds — that's the only
    // unambiguous signal that the model used a virtual grid, not real pixels.
    let exceeds_bounds = x > dims.width as f64 || y > dims.height as f64;
    let both_under_1000 = x <= 1000.0 && y <= 1000.0;

    if exceeds_bounds && both_under_1000 {
        info!(
            "ComputerUse: [0,1000] Gemini coords ({x:.1}, {y:.1}) → scaling to {}×{}",
            dims.width, dims.height,
        );
        return (
            x / 1000.0 * dims.width as f64,
            y / 1000.0 * dims.height as f64,
        );
    }

    (x, y)
}

#[inline]
fn clamp_to_dims(x: f64, y: f64, dims: ModelDims) -> (f64, f64) {
    (
        x.clamp(0.0, dims.width as f64),
        y.clamp(0.0, dims.height as f64),
    )
}

// ─── Network ──────────────────────────────────────────────────────────────────

/// Accepts a fully-built prompt string.  Never builds prompts internally.
async fn call_with_retry(
    client: &Client,
    provider: &Provider,
    jpeg: &[u8],
    prompt: &str,
    rate_limiter: Arc<Semaphore>,
    pass: u8,
) -> Option<(f64, f64)> {
    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            let base = BACKOFF_BASE_MS * (1u64 << attempt.min(4));
            let jitter = subsec_jitter_ms(base / 2);
            tokio::time::sleep(Duration::from_millis(base + jitter)).await;
            warn!("ComputerUse: retry {}/{MAX_ATTEMPTS}", attempt + 1);
        }

        match call_once(client, provider, jpeg, prompt, Arc::clone(&rate_limiter), pass, attempt + 1).await {
            Ok(Some(coord)) => return Some(coord),
            Ok(None) => {
                // Valid null — element genuinely absent. Never retry.
                return None;
            }
            Err(()) => {
                // Network / parse failure → loop for retry.
            }
        }
    }

    warn!("ComputerUse: all {MAX_ATTEMPTS} attempts exhausted");
    None
}

fn subsec_jitter_ms(max_ms: u64) -> u64 {
    if max_ms == 0 {
        return 0;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos as u64) % max_ms
}

async fn call_once(
    client: &Client,
    provider: &Provider,
    jpeg: &[u8],
    prompt: &str,
    rate_limiter: Arc<Semaphore>,
    pass: u8,
    attempt: u32,
) -> Result<Option<(f64, f64)>, ()> {
    match provider {
        Provider::OpenRouter { api_key, model } => {
            call_once_openrouter(client, api_key, model, jpeg, prompt, rate_limiter, pass, attempt).await
        }
        Provider::Gemini { api_key, model } => {
            call_once_gemini(client, api_key, model, jpeg, prompt, rate_limiter, pass, attempt).await
        }
    }
}

async fn call_once_openrouter(
    client: &Client,
    api_key: &str,
    model: &str,
    jpeg: &[u8],
    prompt: &str,
    rate_limiter: Arc<Semaphore>,
    pass: u8,
    attempt: u32,
) -> Result<Option<(f64, f64)>, ()> {
    let b64 = STANDARD.encode(jpeg);

    let body = json!({
        "model": model,
        "max_tokens": 512,
        "temperature": 0,
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "image_url",
                    "image_url": { "url": format!("data:image/jpeg;base64,{b64}") }
                },
                { "type": "text", "text": prompt }
            ]
        }]
    });

    let _permit = rate_limiter.acquire().await
        .map_err(|_| warn!("ComputerUse: semaphore closed"))
        .map_err(|_| ())?;

    let resp = client
        .post(OPENROUTER_API_URL)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .timeout(REQUEST_TIMEOUT)
        .json(&body)
        .send()
        .await
        .map_err(|e| warn!("ComputerUse: network error: {e}"))
        .map_err(|_| ())?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        warn!("ComputerUse: HTTP {status} — {}", &text[..text.len().min(300)]);
        return Err(());
    }

    let json: Value = resp.json().await
        .map_err(|e| warn!("ComputerUse: response JSON: {e}"))
        .map_err(|_| ())?;

    let raw = extract_openrouter_text(&json);
    if raw.is_empty() {
        warn!("ComputerUse: empty content — full message: {}", json["choices"][0]["message"]);
        return Err(());
    }

    debug!("ComputerUse [pass{pass} attempt{attempt}] raw: {raw}");
    write_cu_debug(pass, attempt, jpeg, prompt, &raw);

    match parse_coordinate(&raw) {
        ParseResult::Found(x, y) => Ok(Some((x, y))),
        ParseResult::NoElement => Ok(None),
        ParseResult::ParseFailed => Err(()),
    }
}

async fn call_once_gemini(
    client: &Client,
    api_key: &str,
    model: &str,
    jpeg: &[u8],
    prompt: &str,
    rate_limiter: Arc<Semaphore>,
    pass: u8,
    attempt: u32,
) -> Result<Option<(f64, f64)>, ()> {
    let b64 = STANDARD.encode(jpeg);
    let url = format!("{GEMINI_API_BASE}/{model}:generateContent?key={api_key}");

    let body = json!({
        "contents": [{
            "role": "user",
            "parts": [
                { "inlineData": { "mimeType": "image/jpeg", "data": b64 } },
                { "text": prompt }
            ]
        }],
        "generationConfig": {
            "maxOutputTokens": 512,
            "temperature": 0.0
        }
    });

    let _permit = rate_limiter.acquire().await
        .map_err(|_| warn!("ComputerUse: semaphore closed"))
        .map_err(|_| ())?;

    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .timeout(REQUEST_TIMEOUT)
        .json(&body)
        .send()
        .await
        .map_err(|e| warn!("ComputerUse (Gemini): network error: {e}"))
        .map_err(|_| ())?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        warn!("ComputerUse (Gemini): HTTP {status} — {}", &text[..text.len().min(300)]);
        return Err(());
    }

    let json: Value = resp.json().await
        .map_err(|e| warn!("ComputerUse (Gemini): response JSON: {e}"))
        .map_err(|_| ())?;

    let raw = json["candidates"][0]["content"]["parts"][0]["text"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_owned();

    if raw.is_empty() {
        warn!("ComputerUse (Gemini): empty content — {}", json["candidates"][0]["content"]);
        return Err(());
    }

    debug!("ComputerUse (Gemini) [pass{pass} attempt{attempt}] raw: {raw}");
    write_cu_debug(pass, attempt, jpeg, prompt, &raw);

    match parse_coordinate(&raw) {
        ParseResult::Found(x, y) => Ok(Some((x, y))),
        ParseResult::NoElement => Ok(None),
        ParseResult::ParseFailed => Err(()),
    }
}

/// Handles both `content: "string"` and `content: [{"type":"text","text":"..."}]`.
fn extract_openrouter_text(json: &Value) -> String {
    let msg = &json["choices"][0]["message"]["content"];
    if let Some(s) = msg.as_str() {
        return s.trim().to_owned();
    }
    if let Some(arr) = msg.as_array() {
        return arr
            .iter()
            .filter_map(|part| {
                if part["type"].as_str() == Some("text") {
                    part["text"].as_str()
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("")
            .trim()
            .to_owned();
    }
    String::new()
}

// ─── Prompts ──────────────────────────────────────────────────────────────────

/// Full-screen pass-1 prompt.  Accepts the raw user question.
/// Must never receive a pre-built prompt as the `question` argument.
fn build_prompt(question: &str, width: u32, height: u32) -> String {
    let cx = width / 2;
    let cy = height / 2;
    format!(
        r#"You are a pixel-precise GUI automation assistant. Your sole output must be one JSON object — nothing else.

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
COORDINATE SYSTEM
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Image dimensions : {width} px wide × {height} px tall
Top-left origin  : (0, 0)
X axis           : 0 = left edge → {width} = right edge
Y axis           : 0 = top edge  → {height} = bottom edge
Image centre     : roughly ({cx}, {cy})

  (0,0)──────────────────({width},0)
    │                         │
    │   ({cx},{cy}) = centre  │
    │                         │
  (0,{height})──────────({width},{height})

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
TASK
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
User request: "{question}"

Step 1 — Understand intent.
  Interpret what the user actually wants to accomplish, not just the literal words.
  Key rules:
  • "close this window / panel / dialog" → find the × of the INNERMOST visible
    overlay, sidebar, or floating panel — NOT the browser tab × or OS window ×
    unless the user is explicitly asking about those.
  • "click search" → search input or magnifier icon.
  • "submit / send / confirm" → the primary action button of the current form.

Step 2 — Identify the ONE best element.
  • If there are multiple close buttons (tab ×, panel ×, dialog ×), pick the one
    belonging to the innermost / most recently opened UI layer visible.
  • Return the VISUAL CENTER of that element — the pixel at the exact middle of
    its bounding box.

  Element type → where to aim:
    Button / pill   → horizontal and vertical centre of the button rectangle
    Text field      → horizontal centre, vertical middle of the input row
    Icon / glyph    → centre of the icon bounding box
    Close × button  → the centre of the × glyph itself

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
OUTPUT RULES
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Your entire response must be ONE JSON object.
First character : {{
Last character  : }}
Nothing before the opening brace. Nothing after the closing brace.

Schema — element found:
  {{"x": <int>, "y": <int>, "label": "<1-4 words>"}}

Schema — no specific element (conceptual question only):
  {{"x": null, "y": null, "label": "none"}}

Field constraints:
  x      → plain integer, 0 – {width}   (centre pixel, not edge)
  y      → plain integer, 0 – {height}  (centre pixel, not edge)
  label  → 1 to 4 words ("close panel button", "search field") — never null when x/y are set

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
CORRECT OUTPUT EXAMPLES
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
{{"x": 1398, "y": 173, "label": "close panel button"}}
{{"x": 960,  "y": 54,  "label": "search bar"}}
{{"x": 1437, "y": 22,  "label": "close window button"}}
{{"x": null, "y": null, "label": "none"}}

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
FORBIDDEN
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
```json{{"x":1}}```      ← NO markdown fences
Here is the element: ... ← NO leading prose
{{"x": 14.7}}            ← NO floats
{{"x": -1}}              ← NO negative values
{{"x": 9999}}            ← NO values beyond image bounds
{{"row": 1, "col": 2}}   ← NO renamed fields
{{"x":1,"y":2,"conf":0.9}} ← NO extra fields

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
PRE-OUTPUT CHECKLIST
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
[ ] I identified the element the user ACTUALLY wants (intent, not literal words)
[ ] I chose the innermost/foremost close target if multiple × buttons are visible
[ ] x and y point to the CENTRE of the element, not its edge
[ ] x is a plain integer between 0 and {width}
[ ] y is a plain integer between 0 and {height}
[ ] My response begins with {{ and ends with }} with nothing outside

Output the JSON now:"#
    )
}

/// Zoomed pass-2 prompt.  Accepts the raw user question + crop geometry.
/// Must never receive a pre-built prompt as the `question` argument.
fn build_pass2_prompt(
    question: &str,
    width: u32,
    height: u32,
    crop: &Crop,
    display_w: f64,
    display_h: f64,
) -> String {
    let cx = width / 2;
    let cy = height / 2;
    format!(
        r#"You are a pixel-precise GUI automation assistant. Your sole output must be one JSON object — nothing else.

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
CONTEXT
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
This image is a HIGH-ZOOM CROP extracted from a {display_w:.0}×{display_h:.0} display.
Crop region on the original display : ({:.0},{:.0}) → ({:.0},{:.0})
This image size                      : {width}×{height} px
The target element is expected to be visible near the centre of this crop.

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
COORDINATE SYSTEM (this image only)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Origin  : (0,0) = TOP-LEFT of THIS image
X axis  : 0 → {width}  (left to right)
Y axis  : 0 → {height} (top to bottom)
Centre  : roughly ({cx}, {cy})

IMPORTANT: return coordinates in THIS image's pixel space (0–{width}, 0–{height}).
Do NOT return coordinates from the original full display.

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
TASK
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
User request: "{question}"

Locate the exact centre pixel of the target UI element in THIS image.

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
OUTPUT RULES
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
ONE JSON object. First char {{, last char }}. Nothing else.

  Element found : {{"x": <int 0–{width}>, "y": <int 0–{height}>, "label": "<1-4 words>"}}
  No element    : {{"x": null, "y": null, "label": "none"}}

Output the JSON now:"#,
        crop.x, crop.y, crop.x + crop.w, crop.y + crop.h,
    )
}

// ─── Debug logging ────────────────────────────────────────────────────────────

fn write_cu_debug(pass: u8, attempt: u32, jpeg: &[u8], prompt: &str, response: &str) {
    let dir = std::path::Path::new("test_ss");
    let _ = std::fs::create_dir_all(dir);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let stem = format!("cu_{ts}_pass{pass}_attempt{attempt}");
    let _ = std::fs::write(dir.join(format!("{stem}.jpg")), jpeg);
    let _ = std::fs::write(
        dir.join(format!("{stem}.txt")),
        format!("=== PROMPT ===\n{prompt}\n\n=== RESPONSE ===\n{response}\n"),
    );
}

// ─── Response parsing ─────────────────────────────────────────────────────────

/// Multi-strategy coordinate extractor.
///
/// 1. Strip `<think>…</think>` reasoning blocks (Nemotron ALWAYS emits these).
/// 2. Direct JSON parse on the stripped text.
/// 3. Strip markdown fences, retry.
/// 4. Extract the last balanced `{…}` span (handles JSON embedded in prose).
///
/// Returns `NoElement` as soon as any strategy confirms null coords so the
/// caller never retries a valid "no element" response.
fn parse_coordinate(text: &str) -> ParseResult {
    let stripped = strip_reasoning(text);
    let s = stripped.trim();

    match try_parse_result(s) {
        ParseResult::ParseFailed => {}
        other => return other,
    }
    match try_parse_result(strip_fences(s).trim()) {
        ParseResult::ParseFailed => {}
        other => return other,
    }
    if let Some(span) = last_brace_span(s) {
        match try_parse_result(span) {
            ParseResult::ParseFailed => {}
            other => return other,
        }
    }

    warn!(
        "ComputerUse: all parse strategies failed — text: {}",
        &text[..text.len().min(400)]
    );
    ParseResult::ParseFailed
}

fn try_parse_result(s: &str) -> ParseResult {
    let Ok(c) = serde_json::from_str::<CoordJson>(s) else {
        return ParseResult::ParseFailed;
    };
    match (c.x, c.y) {
        (Some(FlexF64(x)), Some(FlexF64(y))) => {
            info!("ComputerUse: parsed coord ({x:.0}, {y:.0})");
            ParseResult::Found(x, y)
        }
        _ => {
            info!("ComputerUse: model signalled no element (null coords)");
            ParseResult::NoElement
        }
    }
}

fn strip_reasoning(text: &str) -> &str {
    const CLOSE_TAGS: &[&str] = &["</think>", "</reasoning>", "[/THINK]"];
    for tag in CLOSE_TAGS {
        if let Some(pos) = text.rfind(tag) {
            let after = &text[pos + tag.len()..];
            if !after.trim().is_empty() {
                return after;
            }
        }
    }
    text
}

fn strip_fences(s: &str) -> &str {
    let s = s.trim();
    let s = s.strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    s.strip_suffix("```").unwrap_or(s).trim()
}

fn last_brace_span(text: &str) -> Option<&str> {
    let start = text.rfind('{')?;
    let mut depth = 0i32;
    for (i, ch) in text[start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=start + i]);
                }
            }
            _ => {}
        }
    }
    None
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_opt(text: &str) -> Option<(f64, f64)> {
        match parse_coordinate(text) {
            ParseResult::Found(x, y) => Some((x, y)),
            _ => None,
        }
    }

    // ── Resolution ─────────────────────────────────────────────────────────

    #[test]
    fn exact_1080p_unchanged() {
        assert_eq!(compute_model_dims(1920.0, 1080.0), md(1920, 1080));
    }
    #[test]
    fn retina_4k_scaled_to_1080p() {
        assert_eq!(compute_model_dims(3840.0, 2160.0), md(1920, 1080));
    }
    #[test]
    fn portrait_display_exact() {
        assert_eq!(compute_model_dims(1080.0, 1920.0), md(1080, 1920));
    }
    #[test]
    fn ultrawide_aspect_ratio_preserved() {
        let d = compute_model_dims(2560.0, 1080.0);
        let got = d.width as f64 / d.height as f64;
        let want = 2560.0_f64 / 1080.0;
        assert!((got - want).abs() < 0.01, "ratio {got:.4} ≠ {want:.4}");
        assert!(d.width <= MAX_LONG_EDGE);
    }
    #[test]
    fn small_display_not_upscaled() {
        assert_eq!(compute_model_dims(800.0, 600.0), md(800, 600));
    }
    #[test]
    fn pass2_always_reaches_max_long_edge() {
        let cases: &[(f64, f64)] = &[
            (640.0, 480.0),
            (1280.0, 720.0),
            (1920.0, 1080.0),
            (2560.0, 1440.0),
            (3840.0, 2160.0),
            (7680.0, 4320.0),
        ];
        for &(dw, dh) in cases {
            let d = compute_model_dims_allow_upscale(dw * 0.25, dh * 0.25);
            assert_eq!(
                d.width.max(d.height), MAX_LONG_EDGE,
                "display {dw}×{dh}: long edge {} ≠ {MAX_LONG_EDGE}",
                d.width.max(d.height)
            );
        }
    }
    #[test]
    fn degenerate_single_pixel_no_panic() {
        let d = compute_model_dims(1.0, 1.0);
        assert!(d.width >= 2 && d.height >= 2);
    }
    #[test]
    fn align2_rounds_up_odd() {
        assert_eq!(align2(1), 2);
        assert_eq!(align2(100), 100);
        assert_eq!(align2(101), 102);
    }
    #[test]
    fn unproject_midpoint_roundtrip() {
        let d = compute_model_dims(2560.0, 1600.0);
        let (dx, dy) = d.unproject(d.width as f64 / 2.0, d.height as f64 / 2.0, 2560.0, 1600.0);
        assert!((dx - 1280.0).abs() < 1.0, "dx={dx}");
        assert!((dy - 800.0).abs() < 1.0, "dy={dy}");
    }

    // ── Crop ───────────────────────────────────────────────────────────────

    #[test]
    fn crop_centred_in_middle() {
        let c = Crop::centred_on(1280.0, 800.0, 2560.0, 1600.0, 0.25);
        assert!((c.x - 960.0).abs() < 1.0);
        assert!((c.y - 600.0).abs() < 1.0);
        assert!((c.w - 640.0).abs() < 1.0);
        assert!((c.h - 400.0).abs() < 1.0);
    }
    #[test]
    fn crop_clamped_at_origin() {
        let c = Crop::centred_on(0.0, 0.0, 2560.0, 1600.0, 0.25);
        assert_eq!(c.x, 0.0);
        assert_eq!(c.y, 0.0);
        assert!(c.w > 0.0 && c.h > 0.0);
    }
    #[test]
    fn crop_never_exceeds_display() {
        let c = Crop::centred_on(2560.0, 1600.0, 2560.0, 1600.0, 0.25);
        assert!(c.x + c.w <= 2560.0 + 0.5);
        assert!(c.y + c.h <= 1600.0 + 0.5);
    }
    #[test]
    fn crop_unproject_centre() {
        let c = Crop { x: 100.0, y: 200.0, w: 400.0, h: 300.0 };
        let dims = ModelDims { width: 800, height: 600 };
        let (dx, dy) = c.unproject_model(400.0, 300.0, dims);
        assert!((dx - 300.0).abs() < 1.0, "dx={dx}");
        assert!((dy - 350.0).abs() < 1.0, "dy={dy}");
    }

    // ── Parsing ────────────────────────────────────────────────────────────

    #[test]
    fn plain_json() {
        assert_eq!(parse_opt(r#"{"x":640,"y":400,"label":"save"}"#), Some((640.0, 400.0)));
    }
    #[test]
    fn think_block_stripped() {
        let t = "<think>\nI see the Submit button.\n</think>{\"x\":640,\"y\":400}";
        assert_eq!(parse_opt(t), Some((640.0, 400.0)));
    }
    #[test]
    fn reasoning_tag_variant() {
        assert_eq!(
            parse_opt("<reasoning>x</reasoning>{\"x\":50,\"y\":75}"),
            Some((50.0, 75.0))
        );
    }
    #[test]
    fn markdown_fences_stripped() {
        assert_eq!(parse_opt("```json\n{\"x\":320,\"y\":200}\n```"), Some((320.0, 200.0)));
    }
    #[test]
    fn json_in_prose() {
        assert_eq!(
            parse_opt(r#"The element is at {"x":50,"y":75,"label":"link"}."#),
            Some((50.0, 75.0))
        );
    }
    #[test]
    fn string_encoded_coords() {
        assert_eq!(parse_opt(r#"{"x":"512","y":"384"}"#), Some((512.0, 384.0)));
    }
    #[test]
    fn null_returns_none() {
        assert!(parse_opt(r#"{"x":null,"y":null,"label":"none"}"#).is_none());
    }
    #[test]
    fn null_is_no_element_not_parse_failed() {
        assert!(matches!(
            parse_coordinate(r#"{"x":null,"y":null,"label":"none"}"#),
            ParseResult::NoElement
        ));
    }

    // ── Denormalisation ────────────────────────────────────────────────────

    #[test]
    fn unit_normalised_scaled() {
        let d = ModelDims { width: 1920, height: 1080 };
        let (x, y) = denormalise_if_needed(0.5, 0.25, d);
        assert!((x - 960.0).abs() < 1.0);
        assert!((y - 270.0).abs() < 1.0);
    }
    #[test]
    fn integer_zero_not_normalised() {
        let d = ModelDims { width: 1920, height: 1080 };
        assert_eq!(denormalise_if_needed(0.0, 0.0, d), (0.0, 0.0));
    }
    #[test]
    fn large_pixel_coords_unchanged() {
        let d = ModelDims { width: 1920, height: 1080 };
        assert_eq!(denormalise_if_needed(640.0, 400.0, d), (640.0, 400.0));
    }
    /// Regression: (640, 400) on a small 800×600 model must NOT be treated as
    /// Gemini 0-1000 normalised. Old `model_is_small` heuristic got this wrong.
    #[test]
    fn small_display_pixel_coords_not_misclassified() {
        let d = ModelDims { width: 800, height: 600 };
        // 640 < 800 and 400 < 600 — within bounds, must be kept as-is.
        assert_eq!(denormalise_if_needed(640.0, 400.0, d), (640.0, 400.0));
    }
    /// Gemini returns (850, 250) on a 800×600 model — x exceeds width, so it
    /// IS a 0-1000 normalised coord and must be scaled.
    #[test]
    fn gemini_0_1000_detected_via_bounds_exceedance() {
        let d = ModelDims { width: 800, height: 600 };
        let (x, y) = denormalise_if_needed(850.0, 250.0, d);
        assert!((x - 680.0).abs() < 1.0, "x={x}"); // 850/1000*800 = 680
        assert!((y - 150.0).abs() < 1.0, "y={y}"); // 250/1000*600 = 150
    }

    // ── Prompt double-wrap regression ──────────────────────────────────────

    #[test]
    fn pass2_prompt_not_double_wrapped() {
        let crop = Crop { x: 0.0, y: 0.0, w: 480.0, h: 270.0 };
        let p2 = build_pass2_prompt("close the panel", 1920, 1080, &crop, 1920.0, 1080.0);
        assert!(!p2.contains("Identify the single UI element"), "double-wrapped:\n{p2}");
        assert!(p2.contains("HIGH-ZOOM CROP"), "missing crop context:\n{p2}");
    }
    #[test]
    fn pass2_prompt_contains_raw_question_not_wrapped() {
        let crop = Crop { x: 0.0, y: 0.0, w: 480.0, h: 270.0 };
        let q = "click the submit button";
        let p2 = build_pass2_prompt(q, 1920, 1080, &crop, 1920.0, 1080.0);
        // The raw question must appear under "User request:"
        assert!(p2.contains(&format!("User request: \"{q}\"")), "question not embedded:\n{p2}");
        // The pass-1 question wrapper must NOT appear
        assert!(!p2.contains("PRE-OUTPUT CHECKLIST"), "pass-1 boilerplate leaked:\n{p2}");
    }

    // ── Helper ─────────────────────────────────────────────────────────────

    fn md(w: u32, h: u32) -> ModelDims { ModelDims { width: w, height: h } }

    impl PartialEq for ModelDims {
        fn eq(&self, other: &Self) -> bool {
            self.width == other.width && self.height == other.height
        }
    }
}
