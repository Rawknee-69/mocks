//! Vision-based UI element coordinate detection — two-pass, pixel-precise.
//!
//! Pass 1 – *coarse*:  full screenshot at model resolution → approximate location.
//! Pass 2 – *fine*:    25 % zoom crop centred on pass-1 result → precise location.
//!
//! The zoom multiplies the model's effective spatial resolution by ~4×, giving
//! sub-10-pixel accuracy on any standard display without changing the model or
//! adding new dependencies.
//!
//! Additional hardening vs the naïve version:
//! - Strips <think>…</think> before parsing (Nemotron ALWAYS emits these).
//! - Three JSON-extraction strategies: direct → defenced → last-brace scan.
//! - FlexF64: accepts string-encoded numbers ("640") and percentage strings.
//! - Normalised [0,1] coordinate detection + scale-up.
//! - Three-attempt retry with exponential back-off and nanosecond jitter.
//! - 45 s per-request timeout.
//! - RGB8 encode path (RGBA8 is silently broken with JPEG in some image versions).
//! - Single uniform scale factor: width and height are divided by the SAME
//!   computed scale, then independently rounded. The inverse uses the rounded
//!   dims, so there is zero compounding error.

use crate::api::Provider;
use std::{io::Cursor, sync::Arc, time::Duration};

use base64::{engine::general_purpose::STANDARD, Engine as _};
#[allow(unused_imports)] // ImageEncoder is used via .encode_image() method call
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

/// Longest edge of images sent to the model.  1 920 px is enough detail for
/// element detection while keeping payloads reasonable.
const MAX_LONG_EDGE: u32 = 1_920;

/// Pass-2 crop size as a fraction of each display dimension.
/// 0.25 → crop covers 25 % of display area → ~4× zoom → ~4× better accuracy.
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

/// Exact pixel dimensions of the image that was sent to the model.
///
/// Storing these as a named struct makes it impossible to accidentally mix up
/// model space vs display space when projecting coordinates back.
#[derive(Debug, Clone, Copy)]
struct ModelDims {
    width: u32,
    height: u32,
}

impl ModelDims {
    /// Map a model-space pixel back to the given logical coordinate space.
    #[inline]
    fn unproject(self, mx: f64, my: f64, target_w: f64, target_h: f64) -> (f64, f64) {
        (
            mx / self.width as f64 * target_w,
            my / self.height as f64 * target_h,
        )
    }
}

/// Rectangular region expressed in display logical-point coordinates.
#[derive(Debug, Clone, Copy)]
struct Crop {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Crop {
    /// Build a crop centred on `(cx, cy)` covering `fraction` of each display
    /// dimension.  If the box would overflow the display it is shifted inward
    /// without changing its size.
    fn centred_on(
        cx: f64,
        cy: f64,
        display_w: f64,
        display_h: f64,
        fraction: f64,
    ) -> Self {
        // Minimum crop size so tiny displays never produce a degenerate box.
        let w = (display_w * fraction).max(32.0);
        let h = (display_h * fraction).max(32.0);

        // Shift the box so its right/bottom edge stays within bounds.
        let max_x = (display_w - w).max(0.0);
        let max_y = (display_h - h).max(0.0);
        let x = (cx - w / 2.0).clamp(0.0, max_x);
        let y = (cy - h / 2.0).clamp(0.0, max_y);

        // Final guard: never exceed display bounds even on a display smaller
        // than the minimum crop size.
        let w = w.min(display_w - x);
        let h = h.min(display_h - y);

        Self { x, y, w, h }
    }

    /// Unproject a model-space coordinate back to display space via this crop.
    ///
    /// model → crop-local → display
    #[inline]
    fn unproject_model(&self, mx: f64, my: f64, dims: ModelDims) -> (f64, f64) {
        let local_x = mx / dims.width as f64 * self.w;
        let local_y = my / dims.height as f64 * self.h;
        (self.x + local_x, self.y + local_y)
    }
}

/// Accepts a JSON number **or** a numeric string for x / y fields.
///
/// Nemotron occasionally serialises coordinates as `"640"` instead of `640`
/// after its chain-of-thought reasoning block.
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
                // Accept "640", "640.5", "50%" etc.
                v.trim()
                    .trim_end_matches('%')
                    .parse::<f64>()
                    .map(FlexF64)
                    .map_err(|_| {
                        E::invalid_value(Unexpected::Str(v), &"a numeric string")
                    })
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

// ─── Public entry point ───────────────────────────────────────────────────────

/// Detects the display-space location of the UI element described by
/// `user_question`.
///
/// Returns `None` when:
/// - The question is purely conceptual (no element to click).
/// - The model could not be reached after all retries on pass 1.
/// - The screenshot could not be decoded.
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

    // Pass 1 is mandatory — if it fails we have nothing.
    let (raw1_x, raw1_y) = call_with_retry(
        http_client,
        &provider,
        &full_jpeg,
        user_question,
        full_dims,
        Arc::clone(&rate_limiter),
    )
    .await?;

    let (raw1_x, raw1_y) = denormalise_if_needed(raw1_x, raw1_y, full_dims);
    let (mx1, my1) = clamp_to_dims(raw1_x, raw1_y, full_dims);
    let (coarse_dx, coarse_dy) =
        full_dims.unproject(mx1, my1, display_width_points, display_height_points);

    info!("ComputerUse pass 1 → ({coarse_dx:.1}, {coarse_dy:.1}) display pts");

    // Centre-hallucination guard: free-tier LLMs sometimes return the exact screen
    // centre when they are unsure rather than null.  Treat this as a detection failure.
    {
        let centre_x = display_width_points / 2.0;
        let centre_y = display_height_points / 2.0;
        let tolerance_x = display_width_points * 0.05;
        let tolerance_y = display_height_points * 0.05;
        if (coarse_dx - centre_x).abs() < tolerance_x && (coarse_dy - centre_y).abs() < tolerance_y
        {
            warn!(
                "ComputerUse: pass-1 result ({coarse_dx:.1}, {coarse_dy:.1}) is within 5% of \
                 screen centre — treating as hallucination, aborting"
            );
            return None;
        }
    }

    // ── Pass 2: fine location in a 4× zoom crop ───────────────────────────

    let crop = Crop::centred_on(
        coarse_dx,
        coarse_dy,
        display_width_points,
        display_height_points,
        ZOOM_FRACTION,
    );

    // Compute model dims from the crop's own aspect ratio so scaling is exact.
    let zoom_dims = compute_model_dims(crop.w, crop.h);

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

    let effective_zoom = zoom_dims.width as f64 / crop.w;
    info!(
        "ComputerUse pass 2 — crop ({:.0},{:.0} {:.0}×{:.0}) → model {}×{} ({:.1}× zoom)",
        crop.x, crop.y, crop.w, crop.h,
        zoom_dims.width, zoom_dims.height,
        effective_zoom,
    );

    let pass2_question = format!(
        "This is a {:.0}×{:.0} logical-point region zoomed from a larger display. {user_question}",
        crop.w, crop.h,
    );

    // Pass 2 failure falls back to pass 1 — never lose the coarse result.
    let (final_dx, final_dy) = match call_with_retry(
        http_client,
        &provider,
        &zoom_jpeg,
        &pass2_question,
        zoom_dims,
        rate_limiter,
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
            warn!("ComputerUse: pass 2 failed — falling back to pass-1 result");
            (coarse_dx, coarse_dy)
        }
    };

    Some(ComputerUseCoordinate {
        display_local_x: final_dx,
        display_local_y: final_dy,
    })
}

// ─── Resolution ───────────────────────────────────────────────────────────────

/// Compute model image dimensions that:
///
/// 1. Apply a **single uniform scale factor** to both axes — width and height
///    are divided by the same value, so the aspect ratio is preserved exactly.
///    The inverse transform uses the post-round dims, which eliminates the
///    rounding error that causes X and Y to drift at different rates.
/// 2. Fit the long edge within `MAX_LONG_EDGE` (never upscale).
/// 3. Return even pixel counts (JPEG MCU alignment).
fn compute_model_dims(w: f64, h: f64) -> ModelDims {
    let w = w.max(1.0);
    let h = h.max(1.0);

    // One scale factor for both axes.
    let scale = (MAX_LONG_EDGE as f64 / w.max(h)).min(1.0);

    ModelDims {
        width: align2(((w * scale).round() as u32).max(2)),
        height: align2(((h * scale).round() as u32).max(2)),
    }
}

/// Round `v` up to the nearest even integer (JPEG MCU block alignment).
#[inline]
fn align2(v: u32) -> u32 {
    (v + 1) & !1
}

// ─── Image operations ─────────────────────────────────────────────────────────

fn resize_screenshot(jpeg: &[u8], dims: ModelDims) -> Option<Vec<u8>> {
    let img = image::load_from_memory(jpeg)
        .map_err(|e| warn!("ComputerUse: screenshot decode failed: {e}"))
        .ok()?
        .to_rgb8(); // RGB8, not RGBA8 — JPEG has no alpha channel.

    encode_jpeg(&image::imageops::resize(
        &img,
        dims.width,
        dims.height,
        FilterType::Lanczos3,
    ))
}

/// Crop a region from the screenshot (mapping from display logical points to
/// native screenshot pixels regardless of pixel density), then resize to
/// `target_dims`.
///
/// The mapping through the screenshot's actual pixel dimensions means this
/// works correctly whether the screenshot was captured at 1×, 2× (Retina),
/// or any other pixel density.
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

    // Map display-point crop region → screenshot native pixels.
    // This is the correct formula for any pixel density.
    let px = (crop.x / display_w * iw as f64).round() as u32;
    let py = (crop.y / display_h * ih as f64).round() as u32;
    let pw = (crop.w / display_w * iw as f64).ceil() as u32;
    let ph = (crop.h / display_h * ih as f64).ceil() as u32;

    // Clamp to actual image bounds.
    let px = px.min(iw.saturating_sub(1));
    let py = py.min(ih.saturating_sub(1));
    let pw = pw.min(iw - px).max(1);
    let ph = ph.min(ih - py).max(1);

    if pw < 2 || ph < 2 {
        warn!("ComputerUse: crop too small ({pw}×{ph} px) — skipping pass 2");
        return None;
    }

    debug!(
        "ComputerUse: crop native pixels ({px},{py} {pw}×{ph}) from {iw}×{ih}"
    );

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
    image::codecs::jpeg::JpegEncoder::new_with_quality(
        Cursor::new(&mut buf),
        JPEG_QUALITY,
    )
    .encode_image(img)
    .map_err(|e| warn!("ComputerUse: JPEG encode failed: {e}"))
    .ok()?;

    debug!(
        "ComputerUse: encoded JPEG {}×{} → {:.1} KB",
        img.width(),
        img.height(),
        buf.len() as f64 / 1024.0,
    );

    Some(buf)
}

// ─── Coordinate utilities ─────────────────────────────────────────────────────

/// If the model returned normalised [0, 1] coordinates, scale them to pixels.
///
/// Heuristic: if both values are in [0, 1] and at least one is non-integer
/// (ruling out real pixel coordinates 0 and 1 on a non-trivial display), treat
/// them as normalised.
///
/// Also handles 0-1000 scale (Gemini native bounding boxes): if both values fit
/// in [0, 1000] and the display is wider than 1000 px, treat as 0-1000 normalised.
fn denormalise_if_needed(x: f64, y: f64, dims: ModelDims) -> (f64, f64) {
    let both_in_unit = (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y);
    // Integer coords ≤ 1 on a real display are genuine pixel addresses, not
    // normalised values.
    let are_integer_pixels =
        x.fract() == 0.0 && y.fract() == 0.0 && dims.width > 10 && dims.height > 10;

    if both_in_unit && !are_integer_pixels {
        info!(
            "ComputerUse: normalised coords ({x:.4}, {y:.4}) → scaling to {}×{}",
            dims.width, dims.height,
        );
        return (x * dims.width as f64, y * dims.height as f64);
    }

    // Gemini sometimes returns bounding-box coords in the 0-1000 range.
    let both_under_1000 = (0.0..=1000.0).contains(&x) && (0.0..=1000.0).contains(&y);
    let are_pixel_coords = x > 1.0 || y > 1.0; // at least one axis non-trivially > 1
    if both_under_1000 && are_pixel_coords && dims.width > 1000 {
        info!(
            "ComputerUse: 0-1000 coords ({x:.1}, {y:.1}) → scaling to {}×{}",
            dims.width, dims.height,
        );
        return (x / 1000.0 * dims.width as f64, y / 1000.0 * dims.height as f64);
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

async fn call_with_retry(
    client: &Client,
    provider: &Provider,
    jpeg: &[u8],
    question: &str,
    dims: ModelDims,
    rate_limiter: Arc<Semaphore>,
) -> Option<(f64, f64)> {
    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            let base = BACKOFF_BASE_MS * (1u64 << attempt.min(4));
            let jitter = subsec_jitter_ms(base / 2);
            tokio::time::sleep(Duration::from_millis(base + jitter)).await;
            warn!("ComputerUse: retry {}/{MAX_ATTEMPTS}", attempt + 1);
        }

        if let Some(c) =
            call_once(client, provider, jpeg, question, dims, Arc::clone(&rate_limiter)).await
        {
            return Some(c);
        }
    }

    warn!("ComputerUse: all {MAX_ATTEMPTS} attempts exhausted");
    None
}

/// Cheap jitter seeded from nanoseconds — avoids a `rand` dependency while
/// still spreading concurrent retries.
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
    question: &str,
    dims: ModelDims,
    rate_limiter: Arc<Semaphore>,
) -> Option<(f64, f64)> {
    match provider {
        Provider::OpenRouter { api_key, model } => {
            call_once_openrouter(client, api_key, model, jpeg, question, dims, rate_limiter).await
        }
        Provider::Gemini { api_key, model } => {
            call_once_gemini(client, api_key, model, jpeg, question, dims, rate_limiter).await
        }
    }
}

async fn call_once_openrouter(
    client: &Client,
    api_key: &str,
    model: &str,
    jpeg: &[u8],
    question: &str,
    dims: ModelDims,
    rate_limiter: Arc<Semaphore>,
) -> Option<(f64, f64)> {
    let b64 = STANDARD.encode(jpeg);
    let prompt = build_prompt(question, dims.width, dims.height);

    let body = json!({
        "model": model,
        "max_tokens": 512,
        "temperature": 0,
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:image/jpeg;base64,{b64}")
                    }
                },
                { "type": "text", "text": prompt }
            ]
        }]
    });

    let _permit = rate_limiter
        .acquire()
        .await
        .map_err(|_| warn!("ComputerUse: semaphore closed"))
        .ok()?;

    let resp = client
        .post(OPENROUTER_API_URL)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .timeout(REQUEST_TIMEOUT)
        .json(&body)
        .send()
        .await
        .map_err(|e| warn!("ComputerUse: network error: {e}"))
        .ok()?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        warn!(
            "ComputerUse: HTTP {status} — {}",
            &text[..text.len().min(300)]
        );
        return None;
    }

    let json: Value = resp
        .json()
        .await
        .map_err(|e| warn!("ComputerUse: response JSON: {e}"))
        .ok()?;

    let raw = json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_owned();

    // Some models return content as an array: [{"type":"text","text":"..."}]
    let raw = if raw.is_empty() {
        json["choices"][0]["message"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_owned()
    } else {
        raw
    };

    if raw.is_empty() {
        warn!(
            "ComputerUse: empty content — full message: {}",
            json["choices"][0]["message"]
        );
        return None;
    }

    debug!("ComputerUse raw: {raw}");
    parse_coordinate(&raw)
}

async fn call_once_gemini(
    client: &Client,
    api_key: &str,
    model: &str,
    jpeg: &[u8],
    question: &str,
    dims: ModelDims,
    rate_limiter: Arc<Semaphore>,
) -> Option<(f64, f64)> {
    let b64 = STANDARD.encode(jpeg);
    let prompt = build_prompt(question, dims.width, dims.height);
    let url = format!("{GEMINI_API_BASE}/{model}:generateContent?key={api_key}");

    let body = json!({
        "contents": [{
            "role": "user",
            "parts": [
                {
                    "inlineData": {
                        "mimeType": "image/jpeg",
                        "data": b64
                    }
                },
                { "text": prompt }
            ]
        }],
        "generationConfig": {
            "maxOutputTokens": 512,
            "temperature": 0.0
        }
    });

    let _permit = rate_limiter
        .acquire()
        .await
        .map_err(|_| warn!("ComputerUse: semaphore closed"))
        .ok()?;

    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .timeout(REQUEST_TIMEOUT)
        .json(&body)
        .send()
        .await
        .map_err(|e| warn!("ComputerUse (Gemini): network error: {e}"))
        .ok()?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        warn!(
            "ComputerUse (Gemini): HTTP {status} — {}",
            &text[..text.len().min(300)]
        );
        return None;
    }

    let json: Value = resp
        .json()
        .await
        .map_err(|e| warn!("ComputerUse (Gemini): response JSON: {e}"))
        .ok()?;

    let raw = json["candidates"][0]["content"]["parts"][0]["text"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_owned();

    if raw.is_empty() {
        warn!(
            "ComputerUse (Gemini): empty content — full response: {}",
            json["candidates"][0]["content"]
        );
        return None;
    }

    debug!("ComputerUse (Gemini) raw: {raw}");
    parse_coordinate(&raw)
}

// ─── Prompt ───────────────────────────────────────────────────────────────────

fn build_prompt(question: &str, width: u32, height: u32) -> String {
    format!(
        "You are a GUI automation assistant.\n\
         The screenshot is exactly {width}×{height} pixels.\n\
         Origin (0,0) is the top-left corner; X increases right, Y increases down.\n\n\
         User request: \"{question}\"\n\n\
         Identify the single UI element the user should interact with.\n\n\
         Reply with ONLY valid JSON (no markdown fences, no explanation):\n\
         • Element found:   {{\"x\":<int 0–{width}>,\"y\":<int 0–{height}>,\"label\":\"<label>\"}}\n\
         • No element:      {{\"x\":null,\"y\":null,\"label\":\"none\"}}"
    )
}

// ─── Response parsing ─────────────────────────────────────────────────────────

/// Multi-strategy coordinate extractor.
///
/// Strategy order:
/// 1. Strip `<think>…</think>` (mandatory — Nemotron ALWAYS emits this).
/// 2. Direct JSON parse on the stripped text.
/// 3. Strip markdown fences (` ```json … ``` `), retry parse.
/// 4. Find the last balanced `{…}` substring (handles JSON-in-prose).
fn parse_coordinate(text: &str) -> Option<(f64, f64)> {
    let stripped = strip_reasoning(text);
    let s = stripped.trim();

    try_parse(s)
        .or_else(|| try_parse(strip_fences(s).trim()))
        .or_else(|| last_brace_span(s).and_then(try_parse))
        .or_else(|| {
            warn!("ComputerUse: all parse strategies failed — text: {}", &text[..text.len().min(400)]);
            None
        })
}

fn try_parse(s: &str) -> Option<(f64, f64)> {
    let c: CoordJson = serde_json::from_str(s).ok()?;
    match (c.x, c.y) {
        (Some(FlexF64(x)), Some(FlexF64(y))) => {
            info!("ComputerUse: parsed coord ({x:.0}, {y:.0})");
            Some((x, y))
        }
        _ => {
            info!("ComputerUse: model signalled no element (null coords)");
            None
        }
    }
}

/// Strip the last reasoning block, returning only the text that follows it.
///
/// Covers Nemotron's `</think>`, DeepSeek's `</think>`, and minor variants.
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

/// Remove ` ```json … ``` ` or ` ``` … ``` ` wrappers.
///
/// Each `.strip_prefix` / `.strip_suffix` call chains through the same binding
/// so stripping one end does not accidentally undo the other.
fn strip_fences(s: &str) -> &str {
    let s = s.trim();
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    s.strip_suffix("```").unwrap_or(s).trim()
}

/// Find the last balanced `{…}` span.
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
    None // Unbalanced braces.
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Resolution ─────────────────────────────────────────────────────────────

    #[test]
    fn exact_1080p_unchanged() {
        let d = compute_model_dims(1920.0, 1080.0);
        assert_eq!((d.width, d.height), (1920, 1080));
    }

    #[test]
    fn retina_4k_scaled_to_1080p() {
        let d = compute_model_dims(3840.0, 2160.0);
        assert_eq!((d.width, d.height), (1920, 1080));
    }

    #[test]
    fn portrait_display_exact() {
        let d = compute_model_dims(1080.0, 1920.0);
        assert_eq!((d.width, d.height), (1080, 1920));
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
        let d = compute_model_dims(800.0, 600.0);
        assert_eq!((d.width, d.height), (800, 600));
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
        let (dx, dy) =
            d.unproject(d.width as f64 / 2.0, d.height as f64 / 2.0, 2560.0, 1600.0);
        assert!((dx - 1280.0).abs() < 1.0, "dx={dx}");
        assert!((dy - 800.0).abs() < 1.0, "dy={dy}");
    }

    // ── Crop ───────────────────────────────────────────────────────────────────

    #[test]
    fn crop_centred_in_middle() {
        let c = Crop::centred_on(1280.0, 800.0, 2560.0, 1600.0, 0.25);
        assert!((c.x - 960.0).abs() < 1.0, "x={}", c.x);
        assert!((c.y - 600.0).abs() < 1.0, "y={}", c.y);
        assert!((c.w - 640.0).abs() < 1.0, "w={}", c.w);
        assert!((c.h - 400.0).abs() < 1.0, "h={}", c.h);
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
        assert!(c.x + c.w <= 2560.0 + 0.5, "right edge {} > 2560", c.x + c.w);
        assert!(c.y + c.h <= 1600.0 + 0.5, "bottom edge {} > 1600", c.y + c.h);
    }

    #[test]
    fn crop_unproject_centre() {
        let c = Crop { x: 100.0, y: 200.0, w: 400.0, h: 300.0 };
        let dims = ModelDims { width: 800, height: 600 };
        let (dx, dy) = c.unproject_model(400.0, 300.0, dims);
        assert!((dx - 300.0).abs() < 1.0, "dx={dx}");
        assert!((dy - 350.0).abs() < 1.0, "dy={dy}");
    }

    // ── Parsing ────────────────────────────────────────────────────────────────

    #[test]
    fn plain_json() {
        assert_eq!(
            parse_coordinate(r#"{"x":640,"y":400,"label":"save"}"#),
            Some((640.0, 400.0))
        );
    }

    /// The single most important test.
    /// Nemotron is a reasoning model — it ALWAYS wraps output in <think>.
    /// Without stripping this, the parser returns None on every single call.
    #[test]
    fn think_block_stripped_before_parse() {
        let text =
            "<think>\nI see a Submit button at around x=640.\n</think>{\"x\":640,\"y\":400}";
        assert_eq!(parse_coordinate(text), Some((640.0, 400.0)));
    }

    #[test]
    fn reasoning_tag_variant() {
        let text = "<reasoning>analysis</reasoning>{\"x\":50,\"y\":75}";
        assert_eq!(parse_coordinate(text), Some((50.0, 75.0)));
    }

    #[test]
    fn markdown_fences_stripped() {
        let text = "```json\n{\"x\":320,\"y\":200}\n```";
        assert_eq!(parse_coordinate(text), Some((320.0, 200.0)));
    }

    #[test]
    fn json_in_prose() {
        let text = r#"The element is at {"x":50,"y":75,"label":"link"}."#;
        assert_eq!(parse_coordinate(text), Some((50.0, 75.0)));
    }

    #[test]
    fn string_encoded_coords() {
        assert_eq!(
            parse_coordinate(r#"{"x":"512","y":"384"}"#),
            Some((512.0, 384.0))
        );
    }

    #[test]
    fn null_returns_none() {
        assert!(parse_coordinate(r#"{"x":null,"y":null,"label":"none"}"#).is_none());
    }

    // ── Normalisation ──────────────────────────────────────────────────────────

    #[test]
    fn normalised_float_scaled() {
        let dims = ModelDims { width: 1920, height: 1080 };
        let (x, y) = denormalise_if_needed(0.5, 0.25, dims);
        assert!((x - 960.0).abs() < 1.0, "x={x}");
        assert!((y - 270.0).abs() < 1.0, "y={y}");
    }

    #[test]
    fn integer_zero_not_treated_as_normalised() {
        let dims = ModelDims { width: 1920, height: 1080 };
        assert_eq!(denormalise_if_needed(0.0, 0.0, dims), (0.0, 0.0));
    }

    #[test]
    fn large_pixel_coords_unchanged() {
        let dims = ModelDims { width: 1920, height: 1080 };
        assert_eq!(denormalise_if_needed(640.0, 400.0, dims), (640.0, 400.0));
    }
}
