/// Unified screen model merging UIA and vision elements.
///
/// `ScreenModel` is the single data structure that holds all detected UI elements
/// from both sources (UIA tree + vision blob analysis) for one pipeline cycle.
/// Decision logic queries this model to find the best target for a given intent.
use crate::core::coords::{CaptureDescriptor, PointL, PointS};
use crate::uia::UIElement;
use crate::ui_map::UiMap;

/// Merged view of all detected UI elements for one pipeline cycle.
pub struct ScreenModel {
    pub uia_elements: Vec<UIElement>,
    pub visual_elements: Vec<VisualElement>,
    pub capture: Option<CaptureDescriptor>,
}

/// A vision-detected element (from Sobel edge detection + union-find).
#[derive(Debug, Clone)]
pub struct VisualElement {
    pub center_ss: PointS,
    pub width: f64,
    pub height: f64,
}

impl ScreenModel {
    /// Builds a `ScreenModel` from UIA elements and a vision UI map.
    pub fn from_sources(
        uia_elements: Vec<UIElement>,
        ui_map: Option<&UiMap>,
        capture: Option<CaptureDescriptor>,
    ) -> Self {
        let visual_elements = ui_map
            .map(|m| {
                m.elements
                    .iter()
                    .map(|el| VisualElement {
                        center_ss: el.center,
                        width: el.width,
                        height: el.height,
                    })
                    .collect()
            })
            .unwrap_or_default();

        Self {
            uia_elements,
            visual_elements,
            capture,
        }
    }

    /// UIA-first decision: try to find a UIA match for the given label.
    ///
    /// Returns the centre of the matched UIA element in logical desktop space.
    pub fn try_uia_match(&self, label: &str, action: &str, region_hint: Option<&str>, screen_w: f64, screen_h: f64) -> Option<(PointL, String)> {
        if self.uia_elements.is_empty() {
            return None;
        }

        let role_hint = action_to_role_hint(action);

        // Use the scored matcher which combines name + role + region.
        if let Some((el, score, desc)) = crate::uia::scored_match(
            &self.uia_elements,
            label,
            role_hint,
            region_hint,
            screen_w,
            screen_h,
        ) {
            if el.is_enabled && el.rect.width > 0.0 && el.rect.height > 0.0 {
                return Some((el.center(), format!("scored({score:.2}) {desc}")));
            }
        }

        // Legacy fallback for elements with no region hint.
        if let Some(role) = role_hint {
            if let Some(el) = crate::uia::match_by_label_and_role(&self.uia_elements, label, role) {
                if el.is_enabled && el.rect.width > 0.0 && el.rect.height > 0.0 {
                    return Some((el.center(), format!("UIA:label+role '{}'", el.name)));
                }
            }
        }

        if let Some(el) = crate::uia::match_by_label(&self.uia_elements, label) {
            if el.is_enabled && el.rect.width > 0.0 && el.rect.height > 0.0 {
                return Some((el.center(), format!("UIA:label '{}'", el.name)));
            }
        }

        None
    }
}

/// Maps action strings to UIA control type hints.
fn action_to_role_hint(action: &str) -> Option<&'static str> {
    match action {
        "click" => Some("Button"),
        "focus" => Some("Edit"),
        _ => None,
    }
}

/// Enhanced snap that considers both UIA rects and vision blobs.
///
/// Priority:
/// 1. If `coord` is inside any UIA rect → accept (pixel-perfect).
/// 2. If nearest UIA center < `uia_radius` → snap to UIA center.
/// 3. If nearest vision cluster center < `vision_radius` → snap to vision center.
/// 4. Otherwise → None (reject).
pub fn enhanced_snap(
    coord: PointL,
    model: &ScreenModel,
    uia_radius: f64,
    vision_radius: f64,
    ui_map: Option<&UiMap>,
    capture: Option<&CaptureDescriptor>,
) -> Option<PointL> {
    // Priority 1: inside any UIA rect
    for el in &model.uia_elements {
        if el.rect.contains(coord) {
            return Some(coord);
        }
    }

    // Priority 2: nearest UIA center within radius
    let mut best_uia_dist = f64::MAX;
    let mut best_uia_center: Option<PointL> = None;
    for el in &model.uia_elements {
        let center = el.center();
        let d = distance(coord, center);
        if d < uia_radius && d < best_uia_dist {
            best_uia_dist = d;
            best_uia_center = Some(center);
        }
    }
    if let Some(center) = best_uia_center {
        return Some(center);
    }

    // Priority 3: nearest vision cluster via existing snap
    if let (Some(map), Some(cap)) = (ui_map, capture) {
        use crate::core::coords;
        let ss_coord = coords::logical_to_screenshot(coord, cap);
        if let Some(snapped_ss) = crate::ui_map::snap_to_nearest_ui_element(ss_coord, map, vision_radius) {
            return Some(coords::screenshot_to_logical(snapped_ss, cap));
        }
    }

    None
}

/// Euclidean distance between two logical points.
pub fn distance(a: PointL, b: PointL) -> f64 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    (dx * dx + dy * dy).sqrt()
}
