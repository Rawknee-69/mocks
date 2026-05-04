/// Windows UI Automation tree extraction.
///
/// Extracts the UIA element tree from the desktop root using `IUIAutomation::FindAll`
/// with `TreeScope_Subtree`.  Each element is stored as a `UIElement` with its name,
/// control type, bounding rectangle (logical desktop space), enabled state, and
/// automation ID.
///
/// UIA returns logical coordinates on a DPI-aware process, which matches our
/// coordinate pipeline (all work in logical desktop space).
///
/// On non-Windows platforms this module compiles to a no-op stub.

#[cfg(target_os = "windows")]
mod win;

#[cfg(target_os = "windows")]
pub use win::*;

use crate::core::coords::RectL;

/// A single UI Automation element extracted from the desktop tree.
#[derive(Debug, Clone)]
pub struct UIElement {
    pub name: String,
    pub control_type: String,
    pub rect: RectL,
    pub is_enabled: bool,
    pub automation_id: String,
}

impl UIElement {
    /// Returns the centre of the element's bounding rectangle.
    pub fn center(&self) -> crate::core::coords::PointL {
        self.rect.center()
    }
}

/// Stub for non-Windows — returns an empty tree.
#[cfg(not(target_os = "windows"))]
pub fn extract_ui_tree() -> Vec<UIElement> {
    Vec::new()
}

/// Scored UIA match — combines name similarity, control type match, and region proximity.
///
/// Returns the best-scoring element above the threshold (0.4).
/// This replaces the weak token-based substring matching that matched garbage elements.
pub fn scored_match<'a>(
    elements: &'a [UIElement],
    label: &str,
    role_hint: Option<&str>,
    region_hint: Option<&str>,
    screen_width: f64,
    screen_height: f64,
) -> Option<(&'a UIElement, f64, String)> {
    if elements.is_empty() || label.is_empty() {
        return None;
    }

    let label_lower = label.to_lowercase();
    let label_tokens: Vec<&str> = label_lower.split_whitespace().collect();

    let mut best_score = 0.0f64;
    let mut best_element: Option<&UIElement> = None;
    let mut best_desc = String::new();

    for el in elements {
        if el.name.is_empty() || !el.is_enabled {
            continue;
        }

        let name_lower = el.name.to_lowercase();

        // Name scoring: exact match = 1.0, contains full label = 0.85,
        // fraction of matching tokens otherwise
        let name_score = if name_lower == label_lower {
            1.0
        } else if name_lower.contains(&label_lower) || label_lower.contains(&name_lower) {
            0.85
        } else {
            let matching = label_tokens.iter().filter(|t| name_lower.contains(*t)).count();
            if matching == 0 {
                0.0
            } else {
                matching as f64 / label_tokens.len() as f64 * 0.7
            }
        };

        if name_score < 0.1 {
            continue;
        }

        // Role scoring: matching control type is a strong signal
        let role_score = if let Some(role) = role_hint {
            let role_lower = role.to_lowercase();
            if el.control_type.to_lowercase().contains(&role_lower) {
                1.0
            } else {
                0.0
            }
        } else {
            // Prefer interactive types even without a role hint
            match el.control_type.as_str() {
                "Button" | "MenuItem" | "TabItem" | "Hyperlink" | "CheckBox" => 0.5,
                _ => 0.0,
            }
        };

        // Region scoring
        let region_score = if let Some(hint) = region_hint {
            let center = el.center();
            if point_in_region(center, screen_width, screen_height, hint) {
                1.0
            } else {
                0.0
            }
        } else {
            0.0
        };

        let score = name_score * 0.50 + role_score * 0.30 + region_score * 0.20;

        if score > best_score {
            best_score = score;
            best_element = Some(el);
            best_desc = format!(
                "name={:.2} role={:.2} region={:.2} total={:.2} el='{}' type='{}'",
                name_score, role_score, region_score, score, el.name, el.control_type
            );
        }
    }

    if best_score < 0.4 {
        return None;
    }

    best_element.map(|el| (el, best_score, best_desc))
}

/// Returns true if a logical point is in the named screen region.
fn point_in_region(p: crate::core::coords::PointL, screen_w: f64, screen_h: f64, hint: &str) -> bool {
    let rel_x = p.x / screen_w;
    let rel_y = p.y / screen_h;
    let hint = hint.to_lowercase();

    let left   = rel_x < 0.40;
    let right  = rel_x > 0.60;
    let top    = rel_y < 0.35;
    let bottom = rel_y > 0.65;

    if hint.contains("top") && hint.contains("right") { return top && right; }
    if hint.contains("top") && hint.contains("left")  { return top && left;  }
    if hint.contains("bottom") && hint.contains("right") { return bottom && right; }
    if hint.contains("bottom") && hint.contains("left")  { return bottom && left;  }
    if hint.contains("top")    { return top;    }
    if hint.contains("bottom") { return bottom; }
    if hint.contains("right")  { return right;  }
    if hint.contains("left")   { return left;   }
    if hint.contains("center") || hint.contains("centre") || hint.contains("middle") {
        return !left && !right && !top && !bottom;
    }
    false
}

/// Case-insensitive substring match on element name.
pub fn match_by_label<'a>(elements: &'a [UIElement], label: &str) -> Option<&'a UIElement> {
    let lower = label.to_lowercase();
    let tokens: Vec<&str> = lower.split_whitespace().collect();

    elements.iter().filter(|el| !el.name.is_empty()).max_by_key(|el| {
        let name_lower = el.name.to_lowercase();
        let matching_tokens = tokens.iter().filter(|t| name_lower.contains(*t)).count();
        let exact_bonus: usize = if name_lower == lower { 100 } else { 0 };
        matching_tokens + exact_bonus
    }).filter(|el| {
        let name_lower = el.name.to_lowercase();
        tokens.iter().any(|t| name_lower.contains(t))
    })
}

/// Case-insensitive match on control type (e.g. "Button", "Edit", "MenuItem").
pub fn match_by_role<'a>(elements: &'a [UIElement], role: &str) -> Option<&'a UIElement> {
    let lower = role.to_lowercase();
    elements.iter().find(|el| el.control_type.to_lowercase().contains(&lower))
}

/// Match by both label and role — strongest signal.
pub fn match_by_label_and_role<'a>(
    elements: &'a [UIElement],
    label: &str,
    role: &str,
) -> Option<&'a UIElement> {
    let label_lower = label.to_lowercase();
    let role_lower = role.to_lowercase();
    let tokens: Vec<&str> = label_lower.split_whitespace().collect();

    elements.iter().find(|el| {
        let name_lower = el.name.to_lowercase();
        let type_lower = el.control_type.to_lowercase();
        let label_match = tokens.iter().any(|t| name_lower.contains(t));
        let role_match = type_lower.contains(&role_lower);
        label_match && role_match
    })
}
