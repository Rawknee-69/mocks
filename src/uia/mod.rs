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

/// Strips noise words ("button", "icon", "tab") so "New tab button" and "New tab"
/// compare on their semantic core tokens.
fn normalize_label(label: &str) -> String {
    label
        .to_lowercase()
        .replace("button", "")
        .replace("icon", "")
        .replace("tab", "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Token-level match: fraction of `a` tokens found in `b`.
fn token_match(a: &str, b: &str) -> f64 {
    let a_tokens: Vec<&str> = a.split_whitespace().collect();
    let b_tokens: Vec<&str> = b.split_whitespace().collect();

    if a_tokens.is_empty() {
        return 0.0;
    }

    let matches = a_tokens.iter().filter(|at| b_tokens.iter().any(|bt| *at == bt)).count();
    matches as f64 / a_tokens.len() as f64
}

/// Scored UIA match — combines token-based name similarity, control type, region
/// proximity, keyword boosting, and top-bar heuristic.
///
/// Returns the best-scoring element above the hard threshold (0.75).
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
    let label_norm = normalize_label(label);

    let mut best_score = 0.0f64;
    let mut best_element: Option<&UIElement> = None;
    let mut best_desc = String::new();

    for el in elements {
        if el.name.is_empty() || !el.is_enabled {
            continue;
        }

        let name_lower = el.name.to_lowercase();
        let name_norm = normalize_label(&el.name);

        // ── Name scoring ──
        // Exact raw match is perfect; exact normalized match is near-perfect.
        // Then token_match on normalized forms, and containment as baseline.
        let name_score = if name_lower == label_lower {
            1.0
        } else if name_norm == label_norm && !name_norm.is_empty() {
            0.95
        } else if name_lower.contains(&label_lower) || label_lower.contains(&name_lower) {
            0.85
        } else {
            let norm_tok = token_match(&label_norm, &name_norm);
            let raw_tok = token_match(&label_lower, &name_lower);
            let best_tok = norm_tok.max(raw_tok);
            if best_tok > 0.0 { best_tok * 0.85 } else { 0.0 }
        };

        if name_score < 0.1 {
            log::trace!("[UIA_MATCH] SKIP el='{}' name_score={:.2} (below 0.1)", el.name, name_score);
            continue;
        }

        // ── Role scoring ──
        let role_score = if let Some(role) = role_hint {
            let role_lower = role.to_lowercase();
            if el.control_type.to_lowercase().contains(&role_lower) {
                1.0
            } else {
                0.0
            }
        } else {
            match el.control_type.as_str() {
                "Button" | "MenuItem" | "TabItem" | "Hyperlink" | "CheckBox" | "SplitButton" => 0.5,
                _ => 0.0,
            }
        };

        // ── Region scoring ──
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

        let mut score = name_score * 0.70 + role_score * 0.20 + region_score * 0.10;

        // ── Keyword boost ──
        // High-confidence semantic keywords that are strong UIA labels.
        let keywords = [
            "new tab", "close", "minimize", "maximize", "zoom",
            "back", "forward", "reload", "refresh", "search",
            "settings", "menu", "file", "edit", "view", "help",
        ];
        for kw in &keywords {
            if name_lower.contains(kw) && label_lower.contains(kw) {
                score += 0.15;
                break;
            }
        }

        // ── Top-bar heuristic ──
        // Browser chrome elements live in the top ~120px. Boost when the region
        // hint mentions "top".
        if let Some(hint) = region_hint {
            if hint.to_lowercase().contains("top") && el.center().y < 120.0 {
                score += 0.10;
            }
        }

        log::trace!(
            "[UIA_MATCH] el='{}' type='{}' normalized='{}' intent_norm='{}' name={:.2} role={:.2} region={:.2} total={:.2}",
            el.name, el.control_type, name_norm, label_norm, name_score, role_score, region_score, score,
        );

        if score > best_score {
            best_score = score;
            best_element = Some(el);
            best_desc = format!(
                "name={:.2} role={:.2} region={:.2} total={:.2} el='{}' type='{}' norm='{}'→'{}'",
                name_score, role_score, region_score, score, el.name, el.control_type, label_norm, name_norm,
            );
        }
    }

    log::info!(
        "[UIA_MATCH] intent='{}' best_score={:.2} threshold=0.75 result={}",
        label, best_score,
        if best_score >= 0.75 { "ACCEPT" } else { "REJECT" },
    );

    if best_score < 0.75 {
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

/// Case-insensitive token-normalized match on element name.
pub fn match_by_label<'a>(elements: &'a [UIElement], label: &str) -> Option<&'a UIElement> {
    let label_norm = normalize_label(label);
    let label_lower = label.to_lowercase();
    let tokens: Vec<&str> = label_lower.split_whitespace().collect();

    elements.iter().filter(|el| !el.name.is_empty()).max_by_key(|el| {
        let name_lower = el.name.to_lowercase();
        let name_norm = normalize_label(&el.name);

        let exact_normalized: usize = if name_norm == label_norm && !label_norm.is_empty() { 200 } else { 0 };
        let exact_bonus: usize = if name_lower == label_lower { 100 } else { 0 };
        let matching_tokens = tokens.iter().filter(|t| name_lower.contains(*t)).count();

        matching_tokens + exact_bonus + exact_normalized
    }).filter(|el| {
        let name_lower = el.name.to_lowercase();
        let name_norm = normalize_label(&el.name);
        let norm_match = !label_norm.is_empty() && name_norm == label_norm;
        let token_match = tokens.iter().any(|t| name_lower.contains(t));
        norm_match || token_match
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
