/// Strict coordinate type system — one type per coordinate space, no raw tuples.
///
/// Pipeline:
///   MODEL (0–1 normalised)
///   → SCREENSHOT (bitmap pixels)
///   → LOGICAL (desktop logical points, per-monitor origin-relative)
///   → PHYSICAL (actual hardware pixels after DPI scaling)
///
/// Rules:
/// - Every transform is applied EXACTLY once.
/// - No back-conversion.
/// - No mixed spaces.
/// - No raw `(x, y)` anywhere near coordinate math.

// ─── Coordinate types ─────────────────────────────────────────────────────────

/// Normalised model coordinate — u,v ∈ [0, 1].
/// Emitted by the vision model; never stored after conversion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointM {
    pub u: f64,
    pub v: f64,
}

/// Screenshot pixel coordinate — origin at top-left of the bitmap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointS {
    pub x: f64,
    pub y: f64,
}

/// Logical desktop coordinate — the universal internal space.
/// All pipeline stages after screenshot → logical use ONLY this type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointL {
    pub x: f64,
    pub y: f64,
}

/// Physical pixel coordinate after DPI scaling.
/// Used ONLY at the final cursor execution step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointP {
    pub x: f64,
    pub y: f64,
}

/// A rectangle in logical desktop space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RectL {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl RectL {
    #[inline]
    pub fn contains(&self, p: PointL) -> bool {
        p.x >= self.x
            && p.x <= self.x + self.width
            && p.y >= self.y
            && p.y <= self.y + self.height
    }

    #[inline]
    pub fn center(&self) -> PointL {
        PointL {
            x: self.x + self.width / 2.0,
            y: self.y + self.height / 2.0,
        }
    }
}

// ─── Capture descriptor ───────────────────────────────────────────────────────

/// Everything needed to map coordinates for ONE captured screenshot.
///
/// This is the single source of truth for coordinate transforms.
/// If any field is wrong, ALL coordinates derived from this display are wrong.
#[derive(Debug, Clone)]
pub struct CaptureDescriptor {
    /// Human-readable display label (e.g. "Display 1").
    pub id: String,

    /// Position and logical size of this display in the global virtual-desktop.
    pub logical_rect: RectL,

    /// Bitmap dimensions of the screenshot taken for this display.
    pub bitmap_width: u32,
    pub bitmap_height: u32,

    /// DPI scale factor for this monitor (e.g. 1.0, 1.25, 1.5, 2.0).
    /// logical × dpi_scale = physical pixels.
    pub dpi_scale: f64,
}

impl CaptureDescriptor {
    /// Creates a `CaptureDescriptor` from `clicky_types::DisplayInfo` and a known DPI scale.
    pub fn from_display_info(
        info: &clicky_types::DisplayInfo,
        dpi_scale: f64,
    ) -> Self {
        Self {
            id: format!("display_{}", info.screen_number),
            logical_rect: RectL {
                x: info.global_origin_x,
                y: info.global_origin_y,
                width: info.display_width_points,
                height: info.display_height_points,
            },
            bitmap_width: info.screenshot_width_pixels,
            bitmap_height: info.screenshot_height_pixels,
            dpi_scale,
        }
    }
}

// ─── Mathematical transforms (EXACT — single application each) ───────────────

/// MODEL → SCREENSHOT
///
/// x_S = u × W_bitmap
/// y_S = v × H_bitmap
#[inline]
pub fn model_to_screenshot(p: PointM, cap: &CaptureDescriptor) -> PointS {
    PointS {
        x: p.u * cap.bitmap_width as f64,
        y: p.v * cap.bitmap_height as f64,
    }
}

/// SCREENSHOT → LOGICAL (critical — includes per-monitor origin offset)
///
/// x_L = origin_x + x_S × (W_logical / W_bitmap)
/// y_L = origin_y + y_S × (H_logical / H_bitmap)
#[inline]
pub fn screenshot_to_logical(p: PointS, cap: &CaptureDescriptor) -> PointL {
    let scale_x = cap.logical_rect.width / cap.bitmap_width as f64;
    let scale_y = cap.logical_rect.height / cap.bitmap_height as f64;
    PointL {
        x: cap.logical_rect.x + p.x * scale_x,
        y: cap.logical_rect.y + p.y * scale_y,
    }
}

/// LOGICAL → SCREENSHOT (inverse of screenshot_to_logical — used ONLY for snap)
///
/// x_S = (x_L - origin_x) × (W_bitmap / W_logical)
/// y_S = (y_L - origin_y) × (H_bitmap / H_logical)
#[inline]
pub fn logical_to_screenshot(p: PointL, cap: &CaptureDescriptor) -> PointS {
    let scale_x = cap.bitmap_width as f64 / cap.logical_rect.width;
    let scale_y = cap.bitmap_height as f64 / cap.logical_rect.height;
    PointS {
        x: (p.x - cap.logical_rect.x) * scale_x,
        y: (p.y - cap.logical_rect.y) * scale_y,
    }
}

/// LOGICAL → PHYSICAL (final step — only at cursor execution)
///
/// x_P = x_L × dpi_scale
/// y_P = y_L × dpi_scale
///
/// NOTE: on Windows the OS already handles DPI for SetCursorPos when the process
/// is Per-Monitor V2 aware, so dpi_scale should be 1.0 in that scenario.
/// Pass the true scale only when submitting to an API that expects physical pixels.
#[inline]
pub fn logical_to_physical(p: PointL, scale: f64) -> PointP {
    PointP {
        x: p.x * scale,
        y: p.y * scale,
    }
}

/// MODEL → LOGICAL (convenience — composed from model_to_screenshot + screenshot_to_logical)
#[inline]
pub fn model_to_logical(p: PointM, cap: &CaptureDescriptor) -> PointL {
    screenshot_to_logical(model_to_screenshot(p, cap), cap)
}

// ─── Validation helpers ───────────────────────────────────────────────────────

/// Returns `true` if `p` falls within the bounds of any provided `CaptureDescriptor`.
pub fn is_within_any_display(p: PointL, displays: &[CaptureDescriptor]) -> bool {
    displays.iter().any(|d| d.logical_rect.contains(p))
}

/// Returns `true` if `p` is suspiciously close to the geometric centre of any display.
/// Models frequently hallucinate the exact screen centre when uncertain.
///
/// Threshold: within 3% of the shorter display dimension.
pub fn is_screen_centre_hallucination(p: PointL, displays: &[CaptureDescriptor]) -> bool {
    for d in displays {
        let cx = d.logical_rect.x + d.logical_rect.width / 2.0;
        let cy = d.logical_rect.y + d.logical_rect.height / 2.0;
        let dx = p.x - cx;
        let dy = p.y - cy;
        let threshold = d.logical_rect.width.min(d.logical_rect.height) * 0.03;
        if (dx * dx + dy * dy).sqrt() < threshold {
            return true;
        }
    }
    false
}

// ─── Debug pipeline log ───────────────────────────────────────────────────────

/// Emit a full pipeline trace log so every transform step is visible in logs.
#[allow(clippy::too_many_arguments)]
pub fn log_pipeline(
    model: Option<PointM>,
    screenshot: Option<PointS>,
    logical: Option<PointL>,
    refined: Option<PointL>,
    snapped: Option<PointL>,
    physical: Option<PointP>,
    cursor: Option<PointP>,
) {
    log::info!(
        "[PIPELINE] model={} screenshot={} logical={} refined={} snapped={} physical={} cursor={}",
        fmt_opt_m(model),
        fmt_opt_s(screenshot),
        fmt_opt_l(logical),
        fmt_opt_l(refined),
        fmt_opt_l(snapped),
        fmt_opt_p(physical),
        fmt_opt_p(cursor),
    );
}

fn fmt_opt_m(p: Option<PointM>) -> String {
    p.map_or("?".into(), |p| format!("({:.4},{:.4})", p.u, p.v))
}
fn fmt_opt_s(p: Option<PointS>) -> String {
    p.map_or("?".into(), |p| format!("({:.0},{:.0})", p.x, p.y))
}
fn fmt_opt_l(p: Option<PointL>) -> String {
    p.map_or("?".into(), |p| format!("({:.0},{:.0})", p.x, p.y))
}
fn fmt_opt_p(p: Option<PointP>) -> String {
    p.map_or("?".into(), |p| format!("({:.0},{:.0})", p.x, p.y))
}
