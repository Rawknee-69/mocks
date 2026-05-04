/// UI-map builder and intent matcher.
///
/// Builds a map of interactive UI elements from a screenshot using edge detection
/// (Sobel) and connected-component labelling (union-find).  Matches a structured
/// `ElementIntent` against the map and returns the best candidate, or `None` when
/// confidence is below the mandatory threshold (triggers Computer Use fallback).
///
/// All element coordinates are stored in SCREENSHOT pixel space (PointS).
/// Callers must use `CaptureDescriptor` + `screenshot_to_logical` to convert to
/// logical desktop space before any further processing.
///
/// No external crates beyond `image` (already in Cargo.toml with `jpeg` feature) are
/// required for this module.
use crate::core::coords::PointS;
use clicky_types::ElementIntent;
use image::{GrayImage, Luma, RgbaImage};

// ─── Public types ─────────────────────────────────────────────────────────────

/// A single detected interactive UI element.
/// All coordinates are in SCREENSHOT pixel space (PointS).
#[derive(Debug, Clone)]
pub struct UiElement {
    /// From OCR or heuristic; `None` in Phase 1 (no OCR crate).
    pub label: Option<String>,
    /// Center of the element's bounding box in screenshot pixel space.
    pub center: PointS,
    pub width: f64,
    pub height: f64,
}

/// The full UI map for one screenshot.
pub struct UiMap {
    pub elements: Vec<UiElement>,
    pub screenshot_width: u32,
    pub screenshot_height: u32,
    /// Binary edge mask (0 = no edge, 255 = edge) produced by the Sobel step.
    /// Retained so safety validation can call `no_edge_pixels_nearby` without
    /// re-running edge detection.
    pub edge_mask: GrayImage,
}

/// Result of a single intent-match attempt.
pub struct MatchResult {
    pub element: UiElement,
    pub score: f32,
}

/// Errors that can occur when building a UI map.
#[derive(Debug)]
pub enum UiMapError {
    DecodeError(String),
}

impl std::fmt::Display for UiMapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UiMapError::DecodeError(msg) => write!(f, "UI map decode error: {msg}"),
        }
    }
}

// ─── Tuning constants ─────────────────────────────────────────────────────────

/// Sobel edge threshold.  Higher = fewer edges, fewer false-positive blobs.
const EDGE_THRESHOLD: u8 = 30;
/// Discard blobs smaller than this area (px²).  Lowered to catch micro-icons
/// like + / − / × which can be as small as 8–14px per side (~64px² area).
const MIN_BLOB_AREA: u32 = 4;
/// Discard blobs larger than this area (px²).
const MAX_BLOB_AREA: u32 = 200_000;
/// Acceptable aspect ratio range: width / height.  Widened to keep long thin
/// toolbar separators, pill-shaped tabs, and narrow icon strips.
const MIN_ASPECT: f32 = 0.1;
const MAX_ASPECT: f32 = 10.0;
/// Mandatory minimum score for a match to be accepted.
pub const MATCH_CONFIDENCE_THRESHOLD: f32 = 0.65;
/// Radius in screenshot pixels for `snap_to_nearest_ui_element`.
pub const SNAP_RADIUS_PX: f64 = 90.0;
/// Maximum y-coordinate (screenshot pixels) for the "top strip" heuristic.
/// Elements above this line are treated as toolbar/tab-bar candidates.
const TOP_STRIP_Y: f64 = 120.0;
/// Two elements are in the same row if their y-centres differ by less than this.
const ROW_MERGE_TOLERANCE: f64 = 12.0;
/// Minimum number of elements in a row for it to qualify as a tab/toolbar row.
const MIN_TAB_ROW_LEN: usize = 5;

// ─── Entry points ─────────────────────────────────────────────────────────────

/// Builds a `UiMap` from raw JPEG bytes.
///
/// Algorithm:
/// 1. Decode JPEG → RGBA → greyscale.
/// 2. 3×3 box blur to suppress JPEG block artefacts.
/// 3. Sobel edge detection.
/// 4. Threshold → binary edge mask.
/// 5. Connected-component labelling (union-find).
/// 6. Bounding-box each component; apply size + aspect-ratio filters.
/// 7. Each surviving blob becomes a `UiElement`.
pub fn build_ui_map(jpeg_bytes: &[u8]) -> Result<UiMap, UiMapError> {
    println!("--- UI MAP DEBUG START ---");

    let img = image::load_from_memory(jpeg_bytes)
        .map_err(|e| UiMapError::DecodeError(e.to_string()))?;
    let rgba: RgbaImage = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    println!("Image size: {}x{}", width, height);

    let gray = rgba_to_gray(&rgba);
    println!("Converted to grayscale");

    let blurred = box_blur_3x3(&gray);
    println!("Applied blur");

    let edges = sobel_edge_magnitude(&blurred);
    println!("Sobel edge detection complete");

    let bboxes = connected_components(&edges, EDGE_THRESHOLD);
    println!("Total blobs detected: {}", bboxes.len());

    println!("Threshold applied (EDGE_THRESHOLD={})", EDGE_THRESHOLD);

    let mut rejected_count = 0usize;
    let elements: Vec<UiElement> = bboxes
        .into_iter()
        .filter_map(|bb| {
            let area = bb.area();
            let aspect = bb.width as f32 / bb.height.max(1) as f32;

            if area < MIN_BLOB_AREA || area > MAX_BLOB_AREA {
                println!(
                    "Rejected blob: area={} aspect_ratio={:.2} (area out of range [{},{}])",
                    area, aspect, MIN_BLOB_AREA, MAX_BLOB_AREA
                );
                rejected_count += 1;
                return None;
            }
            if aspect < MIN_ASPECT || aspect > MAX_ASPECT {
                println!(
                    "Rejected blob: area={} aspect_ratio={:.2} (aspect out of range [{:.1},{:.1}])",
                    area, aspect, MIN_ASPECT, MAX_ASPECT
                );
                rejected_count += 1;
                return None;
            }
            Some(UiElement {
                label: None,
                center: PointS {
                    x: bb.x as f64 + bb.width as f64 / 2.0,
                    y: bb.y as f64 + bb.height as f64 / 2.0,
                },
                width: bb.width as f64,
                height: bb.height as f64,
            })
        })
        .collect();

    println!("Elements after filtering: {} (rejected {})", elements.len(), rejected_count);

    for (i, el) in elements.iter().enumerate().take(10) {
        println!(
            "Element {}: x={:.0} y={:.0} w={:.0} h={:.0}",
            i, el.center.x, el.center.y, el.width, el.height
        );
    }
    if elements.len() > 10 {
        println!("  ... and {} more elements", elements.len() - 10);
    }

    println!("--- UI MAP DEBUG END ---");

    Ok(UiMap { elements, screenshot_width: width, screenshot_height: height, edge_mask: edges })
}

/// Matches an `ElementIntent` against the elements in the map.
///
/// **Structure-first approach**: instead of scoring every blob independently (which
/// degenerates into a position lottery when text_score=0 for all candidates), we
/// first try to resolve the intent via layout reasoning:
///
/// 1. Group elements into horizontal rows.
/// 2. Detect tab-bar rows (≥5 elements, all y < TOP_STRIP_Y).
/// 3. Apply structural rules:
///    - "new tab" → rightmost element in the tab row.
///    - "zoom in/out" → detect adjacent same-size pair, pick left/right.
///    - "close" → top-right corner element.
/// 4. Only if no structural rule fires, fall back to the old scoring path
///    (but with cursor_score capped so it can never dominate).
pub fn match_intent(
    map: &UiMap,
    intent: &ElementIntent,
    cursor_pos: Option<PointS>,
) -> Option<MatchResult> {
    println!("--- MATCH DEBUG START ---");
    println!("Intent: {:?}", intent);
    println!("Total candidates: {}", map.elements.len());

    if map.elements.is_empty() {
        println!("No candidates — returning None immediately");
        println!("--- MATCH DEBUG END ---");
        return None;
    }

    let label_lower = intent.label.to_lowercase();

    // ── Step 1: Group elements into horizontal rows ──
    let rows = group_by_rows(&map.elements);
    println!("[STRUCTURE] row_count={}", rows.len());

    // ── Step 2: Detect tab rows (top strip, ≥ MIN_TAB_ROW_LEN elements) ──
    let tab_rows: Vec<&Vec<&UiElement>> = rows.iter().filter(|r| is_tab_row(r)).collect();
    let has_tab_row = !tab_rows.is_empty();
    println!("[STRUCTURE] tab_row_detected={}", has_tab_row);

    // ── Step 3: Structural rules (priority order) ──

    // Rule: "new tab" → rightmost element in the tab row
    if is_new_tab_intent(&label_lower) {
        if let Some(tab_row) = tab_rows.first() {
            if let Some(rightmost) = tab_row.iter().max_by(|a, b| {
                a.center.x.partial_cmp(&b.center.x).unwrap_or(std::cmp::Ordering::Equal)
            }) {
                println!(
                    "[STRUCTURE] intent='new tab' → rightmost in tab row x={:.0} y={:.0}",
                    rightmost.center.x, rightmost.center.y,
                );
                println!("[RESULT] intent=\"{}\" method=\"structure_rule:new_tab\"", intent.label);
                println!("--- MATCH DEBUG END ---");
                return Some(MatchResult { element: (*rightmost).clone(), score: 0.95 });
            }
        }
        println!("[STRUCTURE] new_tab rule: no tab row found, falling through");
    }

    // Rule: "close tab" / "close" → rightmost-but-one in tab row or top-right corner
    if is_close_intent(&label_lower) {
        // Try top-right corner element first
        if let Some(el) = find_top_right_element(&map.elements, map.screenshot_width, map.screenshot_height) {
            println!(
                "[STRUCTURE] intent='close' → top-right element x={:.0} y={:.0}",
                el.center.x, el.center.y,
            );
            println!("[RESULT] intent=\"{}\" method=\"structure_rule:close\"", intent.label);
            println!("--- MATCH DEBUG END ---");
            return Some(MatchResult { element: el.clone(), score: 0.95 });
        }
        println!("[STRUCTURE] close rule: no top-right element found, falling through");
    }

    // Rule: "zoom in" / "zoom out" → find adjacent same-size pair
    if is_zoom_intent(&label_lower) {
        let top_strip: Vec<&UiElement> = map.elements.iter().filter(|e| e.center.y < TOP_STRIP_Y).collect();
        if let Some((left, right)) = find_zoom_pair(&top_strip) {
            let pick = if label_lower.contains("in") || label_lower.contains('+') {
                right
            } else {
                left
            };
            println!(
                "[STRUCTURE] intent='zoom' → pair found, picked x={:.0} y={:.0}",
                pick.center.x, pick.center.y,
            );
            println!("[RESULT] intent=\"{}\" method=\"structure_rule:zoom\"", intent.label);
            println!("--- MATCH DEBUG END ---");
            return Some(MatchResult { element: pick, score: 0.95 });
        }
        println!("[STRUCTURE] zoom rule: no pair found, falling through");
    }

    // ── Step 4: Fallback scoring (cursor dominance removed) ──
    println!("[STRUCTURE] no structural rule matched, using fallback scoring");

    let max_distance = ((map.screenshot_width as f64).powi(2)
        + (map.screenshot_height as f64).powi(2))
    .sqrt()
    .max(1.0);

    // For toolbar/icon intents, restrict candidates to the top strip.
    let is_toolbar_intent = is_top_strip_intent(&label_lower);
    let candidates: Vec<&UiElement> = if is_toolbar_intent {
        let filtered: Vec<&UiElement> = map.elements.iter().filter(|e| e.center.y < TOP_STRIP_Y).collect();
        if !filtered.is_empty() {
            println!("[FILTER] top-strip filter: {} → {} candidates", map.elements.len(), filtered.len());
            filtered
        } else {
            map.elements.iter().collect()
        }
    } else {
        map.elements.iter().collect()
    };

    let mut best_score = 0.0f32;
    let mut best_element: Option<UiElement> = None;

    for (i, el) in candidates.iter().enumerate() {
        if el.width < 4.0 || el.height < 4.0 {
            continue;
        }

        let mut score = 0.0f32;

        // text_similarity — 0.0 in Phase 1 (no OCR labels).
        let text_sim = el
            .label
            .as_deref()
            .map(|l| jaro_winkler(l, &intent.label))
            .unwrap_or(0.0);
        score += text_sim;

        // region_hint_match: +0.50 if the element centre is in the hinted region.
        let region_score = if let Some(ref hint) = intent.region_hint {
            if is_in_region(el.center.x, el.center.y, map.screenshot_width, map.screenshot_height, hint) {
                score += 0.50;
                0.50f32
            } else {
                0.0f32
            }
        } else {
            0.0f32
        };

        // size_weight: prefer elements whose height falls in interactive size ranges.
        let h = el.height as f32;
        let size_w = if h >= 24.0 && h <= 80.0 {
            0.20
        } else if h >= 12.0 && h <= 24.0 {
            0.18
        } else if h > 80.0 && h <= 120.0 {
            0.10
        } else if h >= 4.0 && h < 12.0 {
            0.12
        } else {
            0.0
        };
        score += size_w;

        // position_bias: prefer elements in the vertical centre ±30% of the viewport.
        let rel_y = el.center.y / map.screenshot_height as f64;
        let in_action_zone = rel_y >= 0.20 && rel_y <= 0.80;
        let position_score = if in_action_zone {
            score += 0.15;
            0.15f32
        } else {
            0.0f32
        };

        // cursor_bias: CAPPED at 0.15 — cursor proximity must never dominate
        // selection when all candidates lack text labels.
        let cursor_score = if let Some(cp) = cursor_pos {
            let distance = dist_sq(el.center.x, el.center.y, cp.x, cp.y).sqrt();
            let s = (1.0 - distance / max_distance) as f32 * 0.15;
            score += s;
            s
        } else {
            0.0f32
        };

        // row-membership boost: elements that belong to a multi-element row
        // are more likely to be real interactive UI than isolated blobs.
        let row_score = if rows.iter().any(|row| row.len() >= 3 && row.iter().any(|r| std::ptr::eq(*r, *el))) {
            score += 0.10;
            0.10f32
        } else {
            0.0f32
        };

        if i < 10 || score > best_score {
            println!(
                "  Candidate {}: x={:.0} y={:.0} size={:.0}x{:.0} → text:{:.2} region:{:.2} size:{:.2} pos:{:.2} cursor:{:.2} row:{:.2} total:{:.2}",
                i, el.center.x, el.center.y, el.width, el.height,
                text_sim, region_score, size_w, position_score, cursor_score, row_score, score,
            );
        }

        if score > best_score {
            best_score = score;
            best_element = Some((*el).clone());
        }
    }

    println!("Best score: {:.2} (threshold: {:.2})", best_score, MATCH_CONFIDENCE_THRESHOLD);

    let has_region_hint = intent.region_hint.is_some();
    let soft_accept = has_region_hint && best_score >= 0.55;

    if best_score < MATCH_CONFIDENCE_THRESHOLD && !soft_accept {
        println!("Rejected due to low confidence");
        println!("[RESULT] intent=\"{}\" method=\"fallback_scoring\" result=REJECTED", intent.label);
        println!("--- MATCH DEBUG END ---");
        return None;
    }

    if soft_accept && best_score < MATCH_CONFIDENCE_THRESHOLD {
        println!("Soft-accepted: region_hint matched and score={:.2} >= 0.55", best_score);
    }

    println!("[RESULT] intent=\"{}\" method=\"fallback_scoring\" result=ACCEPTED", intent.label);
    println!("--- MATCH DEBUG END ---");
    best_element.map(|element| MatchResult { element, score: best_score })
}

// ─── Structural layout analysis ──────────────────────────────────────────────

/// Groups elements into horizontal rows.  Two elements share a row when their
/// y-centres are within `ROW_MERGE_TOLERANCE` pixels.
fn group_by_rows<'a>(elements: &'a [UiElement]) -> Vec<Vec<&'a UiElement>> {
    let mut rows: Vec<Vec<&UiElement>> = Vec::new();

    for el in elements {
        let mut placed = false;
        for row in &mut rows {
            if (row[0].center.y - el.center.y).abs() < ROW_MERGE_TOLERANCE {
                row.push(el);
                placed = true;
                break;
            }
        }
        if !placed {
            rows.push(vec![el]);
        }
    }

    // Sort each row left-to-right by x-centre.
    for row in &mut rows {
        row.sort_by(|a, b| a.center.x.partial_cmp(&b.center.x).unwrap_or(std::cmp::Ordering::Equal));
    }

    rows
}

/// A tab/toolbar row: ≥ MIN_TAB_ROW_LEN elements all in the top strip.
fn is_tab_row(row: &[&UiElement]) -> bool {
    row.len() >= MIN_TAB_ROW_LEN && row.iter().all(|e| e.center.y < TOP_STRIP_Y)
}

/// Intent classification: "new tab"
fn is_new_tab_intent(label: &str) -> bool {
    label.contains("new tab")
        || label.contains("add tab")
        || (label.contains("tab") && label.contains('+'))
        || label == "+"
}

/// Intent classification: "close" / "close tab"
fn is_close_intent(label: &str) -> bool {
    label.contains("close")
}

/// Intent classification: "zoom in" / "zoom out"
fn is_zoom_intent(label: &str) -> bool {
    label.contains("zoom")
        || (label.contains("magnif"))
}

/// Returns true for intents that are typically in the top toolbar strip:
/// tabs, navigation, address bar, window controls.
fn is_top_strip_intent(label: &str) -> bool {
    label.contains("tab")
        || label.contains("address")
        || label.contains("url")
        || label.contains("reload")
        || label.contains("refresh")
        || label.contains("back")
        || label.contains("forward")
        || label.contains("minimize")
        || label.contains("maximize")
        || label.contains("close")
}

/// Find the element closest to the top-right corner of the screen.
fn find_top_right_element<'a>(elements: &'a [UiElement], screen_w: u32, _screen_h: u32) -> Option<&'a UiElement> {
    elements
        .iter()
        .filter(|e| {
            let rel_x = e.center.x / screen_w as f64;
            rel_x > 0.70 && e.center.y < TOP_STRIP_Y
        })
        .max_by(|a, b| {
            a.center.x.partial_cmp(&b.center.x).unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// Find a zoom pair: two elements close together, similar y, similar area.
/// Returns (left, right) sorted by x.
fn find_zoom_pair(elements: &[&UiElement]) -> Option<(UiElement, UiElement)> {
    let mut best: Option<(UiElement, UiElement)> = None;
    let mut best_dist = f64::MAX;

    for (i, a) in elements.iter().enumerate() {
        for b in elements.iter().skip(i + 1) {
            let dx = (a.center.x - b.center.x).abs();
            let dy = (a.center.y - b.center.y).abs();
            let a_area = a.width * a.height;
            let b_area = b.width * b.height;
            let area_diff = (a_area - b_area).abs();

            if dx < 50.0 && dy < 12.0 && area_diff < 30.0 {
                let d = dx + dy;
                if d < best_dist {
                    best_dist = d;
                    let (left, right) = if a.center.x < b.center.x {
                        ((*a).clone(), (*b).clone())
                    } else {
                        ((*b).clone(), (*a).clone())
                    };
                    best = Some((left, right));
                }
            }
        }
    }

    best
}

/// Snaps a raw coordinate to the nearest `UiElement` centre within `radius` pixels.
///
/// Input and output are in SCREENSHOT pixel space (PointS).
///
/// Selection rule:
/// 1. Candidates = elements whose centre is within `radius` of `coord`.
/// 2. No candidates → `None` (safe fail).
/// 3. One candidate → its centre.
/// 4. Multiple → the element with the highest `area × proximity_score`,
///    where `proximity_score = 1.0 − (distance / radius)`.
pub fn snap_to_nearest_ui_element(
    coord: PointS,
    map: &UiMap,
    radius: f64,
) -> Option<PointS> {
    let radius_sq = radius * radius;

    let candidates: Vec<(&UiElement, f64)> = map
        .elements
        .iter()
        .filter_map(|el| {
            let d2 = dist_sq(el.center.x, el.center.y, coord.x, coord.y);
            if d2 <= radius_sq {
                Some((el, d2.sqrt()))
            } else {
                None
            }
        })
        .collect();

    match candidates.len() {
        0 => None,
        1 => Some(candidates[0].0.center),
        _ => {
            let best = candidates.iter().max_by(|(a_el, a_dist), (b_el, b_dist)| {
                let a_area = a_el.width * a_el.height;
                let b_area = b_el.width * b_el.height;
                let a_prox = 1.0 - a_dist / radius;
                let b_prox = 1.0 - b_dist / radius;
                let a_score = a_area * a_prox;
                let b_score = b_area * b_prox;
                a_score.partial_cmp(&b_score).unwrap_or(std::cmp::Ordering::Equal)
            })?;
            Some(best.0.center)
        }
    }
}

/// Returns `true` when no edge pixels exist within `radius` pixels of `coord`.
///
/// Used in final safety validation — a coordinate with no nearby edges is likely
/// a hallucinated centre point rather than a real UI element boundary.
/// `coord` must be in SCREENSHOT pixel space (PointS).
pub fn no_edge_pixels_nearby(
    coord: PointS,
    edge_mask: &GrayImage,
    radius: u32,
) -> bool {
    let (cx, cy) = (coord.x as i64, coord.y as i64);
    let (width, height) = edge_mask.dimensions();
    let r = radius as i64;

    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy > r * r {
                continue;
            }
            let px = cx + dx;
            let py = cy + dy;
            if px < 0 || py < 0 || px >= width as i64 || py >= height as i64 {
                continue;
            }
            if edge_mask.get_pixel(px as u32, py as u32).0[0] > 0 {
                return false;
            }
        }
    }
    true
}

/// Returns `true` when no `UiElement` bounding box centre is within `radius` of `coord`.
/// `coord` must be in SCREENSHOT pixel space (PointS).
pub fn no_bounding_box_nearby(coord: PointS, map: &UiMap, radius: f64) -> bool {
    let radius_sq = radius * radius;
    !map.elements
        .iter()
        .any(|el| dist_sq(el.center.x, el.center.y, coord.x, coord.y) <= radius_sq)
}

// ─── Private helpers ──────────────────────────────────────────────────────────

/// Converts an `RgbaImage` to greyscale using the standard luminance weights.
fn rgba_to_gray(img: &RgbaImage) -> GrayImage {
    let (w, h) = img.dimensions();
    let mut out = GrayImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels() {
        let r = px.0[0] as f32;
        let g = px.0[1] as f32;
        let b = px.0[2] as f32;
        let luma = (0.2126 * r + 0.7152 * g + 0.0722 * b) as u8;
        out.put_pixel(x, y, Luma([luma]));
    }
    out
}

/// Simple 3×3 box blur — reduces JPEG block-artefact noise before edge detection.
fn box_blur_3x3(img: &GrayImage) -> GrayImage {
    let (w, h) = img.dimensions();
    let mut out = GrayImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let mut sum: u32 = 0;
            let mut count: u32 = 0;
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let nx = x as i32 + dx;
                    let ny = y as i32 + dy;
                    if nx >= 0 && ny >= 0 && nx < w as i32 && ny < h as i32 {
                        sum += img.get_pixel(nx as u32, ny as u32).0[0] as u32;
                        count += 1;
                    }
                }
            }
            out.put_pixel(x, y, Luma([(sum / count) as u8]));
        }
    }
    out
}

/// Computes Sobel edge magnitude as an 8-bit greyscale image.
///
/// Each pixel value is clamped to [0, 255].
fn sobel_edge_magnitude(gray: &GrayImage) -> GrayImage {
    let (w, h) = gray.dimensions();
    let mut out = GrayImage::new(w, h);

    // Sobel kernels (3×3)
    const KX: [[i32; 3]; 3] = [[-1, 0, 1], [-2, 0, 2], [-1, 0, 1]];
    const KY: [[i32; 3]; 3] = [[-1, -2, -1], [0, 0, 0], [1, 2, 1]];

    for y in 1..(h - 1) {
        for x in 1..(w - 1) {
            let mut gx: i32 = 0;
            let mut gy: i32 = 0;
            for ky in 0..3usize {
                for kx in 0..3usize {
                    let px_val = gray
                        .get_pixel(x + kx as u32 - 1, y + ky as u32 - 1)
                        .0[0] as i32;
                    gx += KX[ky][kx] * px_val;
                    gy += KY[ky][kx] * px_val;
                }
            }
            let magnitude = ((gx * gx + gy * gy) as f32).sqrt().min(255.0) as u8;
            out.put_pixel(x, y, Luma([magnitude]));
        }
    }
    out
}

/// Bounding box with top-left origin.
#[derive(Clone, Copy)]
struct BoundingBox {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl BoundingBox {
    fn area(self) -> u32 {
        self.width * self.height
    }
}

/// Simple union-find data structure for connected-component labelling.
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self { parent: (0..n).collect() }
    }

    fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            self.parent[x] = self.find(self.parent[x]);
        }
        self.parent[x]
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}

/// Performs connected-component labelling on the edge magnitude image.
///
/// Only pixels above `threshold` are considered edge pixels.
/// Returns one bounding box per connected component (4-connectivity).
fn connected_components(edges: &GrayImage, threshold: u8) -> Vec<BoundingBox> {
    let (w, h) = edges.dimensions();
    let n = (w * h) as usize;
    let mut uf = UnionFind::new(n);

    // First pass: link adjacent edge pixels.
    for y in 0..h {
        for x in 0..w {
            if edges.get_pixel(x, y).0[0] < threshold {
                continue;
            }
            let idx = (y * w + x) as usize;
            // Check left neighbour
            if x > 0 && edges.get_pixel(x - 1, y).0[0] >= threshold {
                uf.union(idx, (y * w + x - 1) as usize);
            }
            // Check top neighbour
            if y > 0 && edges.get_pixel(x, y - 1).0[0] >= threshold {
                uf.union(idx, ((y - 1) * w + x) as usize);
            }
        }
    }

    // Second pass: gather bounding boxes per root label.
    // We use a flat Vec indexed by component root; non-edge pixels keep their
    // initial bounds sentinel (u32::MAX / 0) and are filtered out at the end.
    let mut min_x = vec![u32::MAX; n];
    let mut min_y = vec![u32::MAX; n];
    let mut max_x = vec![0u32; n];
    let mut max_y = vec![0u32; n];
    let mut has_pixel = vec![false; n];

    for y in 0..h {
        for x in 0..w {
            if edges.get_pixel(x, y).0[0] < threshold {
                continue;
            }
            let idx = (y * w + x) as usize;
            let root = uf.find(idx);
            if x < min_x[root] {
                min_x[root] = x;
            }
            if y < min_y[root] {
                min_y[root] = y;
            }
            if x > max_x[root] {
                max_x[root] = x;
            }
            if y > max_y[root] {
                max_y[root] = y;
            }
            has_pixel[root] = true;
        }
    }

    // Collect one BoundingBox per root.
    (0..n)
        .filter(|&i| has_pixel[i] && uf.find(i) == i)
        .map(|i| BoundingBox {
            x: min_x[i],
            y: min_y[i],
            width: max_x[i].saturating_sub(min_x[i]) + 1,
            height: max_y[i].saturating_sub(min_y[i]) + 1,
        })
        .collect()
}

/// Returns `true` if (`cx`, `cy`) falls in the screen region described by `hint`.
///
/// The hint is matched case-insensitively against keywords.
fn is_in_region(cx: f64, cy: f64, img_w: u32, img_h: u32, hint: &str) -> bool {
    let rel_x = cx / img_w as f64;
    let rel_y = cy / img_h as f64;
    let hint = hint.to_lowercase();

    let left   = rel_x < 0.40;
    let right  = rel_x > 0.60;
    let top    = rel_y < 0.40;
    let bottom = rel_y > 0.60;
    let center = !left && !right && !top && !bottom;

    if hint.contains("top") && hint.contains("right") {
        return top && right;
    }
    if hint.contains("top") && hint.contains("left") {
        return top && left;
    }
    if hint.contains("bottom") && hint.contains("right") {
        return bottom && right;
    }
    if hint.contains("bottom") && hint.contains("left") {
        return bottom && left;
    }
    if hint.contains("top") {
        return top;
    }
    if hint.contains("bottom") {
        return bottom;
    }
    if hint.contains("right") {
        return right;
    }
    if hint.contains("left") {
        return left;
    }
    if hint.contains("center") || hint.contains("centre") || hint.contains("middle") {
        return center;
    }
    false
}

/// Jaro-Winkler string similarity in [0.0, 1.0].
///
/// Phase 1 placeholder — used when OCR labels become available.
/// Returns 0.0 when either string is empty.
fn jaro_winkler(a: &str, b: &str) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();

    let match_distance = (a.len().max(b.len()) / 2).saturating_sub(1);

    let mut a_matched = vec![false; a.len()];
    let mut b_matched = vec![false; b.len()];
    let mut matches = 0u32;

    for i in 0..a.len() {
        let start = i.saturating_sub(match_distance);
        let end = (i + match_distance + 1).min(b.len());
        for j in start..end {
            if b_matched[j] || a[i] != b[j] {
                continue;
            }
            a_matched[i] = true;
            b_matched[j] = true;
            matches += 1;
            break;
        }
    }

    if matches == 0 {
        return 0.0;
    }

    let mut transpositions = 0u32;
    let mut k = 0usize;
    for i in 0..a.len() {
        if !a_matched[i] {
            continue;
        }
        while !b_matched[k] {
            k += 1;
        }
        if a[i] != b[k] {
            transpositions += 1;
        }
        k += 1;
    }

    let m = matches as f32;
    let t = transpositions as f32 / 2.0;
    let jaro = (m / a.len() as f32 + m / b.len() as f32 + (m - t) / m) / 3.0;

    // Winkler prefix bonus (up to 4 chars)
    let prefix = a.iter().zip(b.iter()).take(4).take_while(|(x, y)| x == y).count() as f32;
    jaro + prefix * 0.1 * (1.0 - jaro)
}

#[inline]
fn dist_sq(ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let dx = ax - bx;
    let dy = ay - by;
    dx * dx + dy * dy
}

/// Multi-pass vision refinement with crop reprojection.
///
/// 1. Takes a coarse match centre (in screenshot pixel space).
/// 2. Crops a region of `crop_size` pixels around it.
/// 3. Re-runs edge detection on the crop for finer detail.
/// 4. Reprojects the refined centre back to full-screenshot space.
///
/// Returns `None` if the crop fails to produce any valid elements.
pub fn refine_with_crop(
    jpeg_bytes: &[u8],
    coarse_center: PointS,
    crop_size: u32,
) -> Result<Option<PointS>, UiMapError> {
    let img = image::load_from_memory(jpeg_bytes)
        .map_err(|e| UiMapError::DecodeError(e.to_string()))?;
    let rgba: RgbaImage = img.to_rgba8();
    let (full_w, full_h) = rgba.dimensions();

    let half = crop_size / 2;
    let crop_x = (coarse_center.x as u32).saturating_sub(half).min(full_w.saturating_sub(crop_size));
    let crop_y = (coarse_center.y as u32).saturating_sub(half).min(full_h.saturating_sub(crop_size));
    let crop_w = crop_size.min(full_w - crop_x);
    let crop_h = crop_size.min(full_h - crop_y);

    if crop_w < 20 || crop_h < 20 {
        return Ok(None);
    }

    let cropped = image::imageops::crop_imm(&rgba, crop_x, crop_y, crop_w, crop_h).to_image();
    let gray = rgba_to_gray(&cropped);
    let blurred = box_blur_3x3(&gray);
    let edges = sobel_edge_magnitude(&blurred);
    let bboxes = connected_components(&edges, EDGE_THRESHOLD);

    let elements: Vec<(f64, f64, u32)> = bboxes
        .into_iter()
        .filter(|bb| {
            let area = bb.area();
            let aspect = bb.width as f32 / bb.height.max(1) as f32;
            area >= 100 && area <= MAX_BLOB_AREA
                && aspect >= MIN_ASPECT && aspect <= MAX_ASPECT
        })
        .map(|bb| {
            let cx = bb.x as f64 + bb.width as f64 / 2.0;
            let cy = bb.y as f64 + bb.height as f64 / 2.0;
            let area = bb.area();
            (cx, cy, area)
        })
        .collect();

    if elements.is_empty() {
        return Ok(None);
    }

    // Find the element closest to the crop centre.
    let crop_cx = crop_w as f64 / 2.0;
    let crop_cy = crop_h as f64 / 2.0;
    let best = elements.iter().min_by(|a, b| {
        let da = dist_sq(a.0, a.1, crop_cx, crop_cy);
        let db = dist_sq(b.0, b.1, crop_cx, crop_cy);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(best.map(|(cx, cy, _)| {
        PointS {
            x: crop_x as f64 + cx,
            y: crop_y as f64 + cy,
        }
    }))
}
