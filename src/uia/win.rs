/// Windows UI Automation extraction using the `windows` crate.
///
/// Runs COM-based IUIAutomation calls to extract the full subtree of the desktop.
/// Must be called from `spawn_blocking` — COM calls block and the full tree scan
/// takes 50–200ms depending on desktop complexity.
use crate::core::coords::RectL;
use super::UIElement;

use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, CoCreateInstance,
    COINIT_MULTITHREADED, CLSCTX_ALL,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation,
    TreeScope_Subtree,
};

/// Maps UIA ControlType IDs to human-readable names.
fn control_type_name(id: i32) -> &'static str {
    match id {
        50000 => "Button",
        50001 => "Calendar",
        50002 => "CheckBox",
        50003 => "ComboBox",
        50004 => "Edit",
        50005 => "Hyperlink",
        50006 => "Image",
        50007 => "ListItem",
        50008 => "List",
        50009 => "Menu",
        50010 => "MenuBar",
        50011 => "MenuItem",
        50012 => "ProgressBar",
        50013 => "RadioButton",
        50014 => "ScrollBar",
        50015 => "Slider",
        50016 => "Spinner",
        50017 => "StatusBar",
        50018 => "Tab",
        50019 => "TabItem",
        50020 => "Text",
        50021 => "ToolBar",
        50022 => "ToolTip",
        50023 => "Tree",
        50024 => "TreeItem",
        50025 => "Custom",
        50026 => "Group",
        50027 => "Thumb",
        50028 => "DataGrid",
        50029 => "DataItem",
        50030 => "Document",
        50031 => "SplitButton",
        50032 => "Window",
        50033 => "Pane",
        50034 => "Header",
        50035 => "HeaderItem",
        50036 => "Table",
        50037 => "TitleBar",
        50038 => "Separator",
        _ => "Unknown",
    }
}

/// Interactive element types worth extracting for matching.
/// Excludes Text (50020) — text nodes return individual characters from editors/terminals.
/// Excludes Window (50032) — too broad, always matches cursor display.
/// Excludes ToolBar (50021) — container, not a click target.
fn is_interactive_type(id: i32) -> bool {
    matches!(id,
        50000 |  // Button
        50002 |  // CheckBox
        50003 |  // ComboBox
        50004 |  // Edit
        50005 |  // Hyperlink
        50007 |  // ListItem
        50009 |  // Menu
        50010 |  // MenuBar
        50011 |  // MenuItem
        50013 |  // RadioButton
        50015 |  // Slider
        50018 |  // Tab
        50019 |  // TabItem
        50024 |  // TreeItem
        50031    // SplitButton
    )
}

/// Returns true if the name looks like a real interactive element label, not
/// punctuation or single-character garbage from text editors.
fn is_meaningful_name(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.len() <= 2 && trimmed.chars().all(|c| c.is_ascii_punctuation() || c.is_whitespace()) {
        return false;
    }
    true
}

/// Extracts the full UIA element tree from the desktop.
///
/// Filters to interactive element types and non-degenerate bounding rects.
/// Returns elements sorted by area (smallest first) to prefer specific targets.
pub fn extract_ui_tree() -> Vec<UIElement> {
    unsafe { extract_ui_tree_inner() }.unwrap_or_else(|e| {
        log::warn!("[UIA] extraction failed: {}", e);
        Vec::new()
    })
}

unsafe fn extract_ui_tree_inner() -> Result<Vec<UIElement>, String> {
    CoInitializeEx(Some(std::ptr::null()), COINIT_MULTITHREADED)
        .ok()
        .map_err(|e| format!("CoInitializeEx: {e}"))?;

    let result = extract_elements();

    CoUninitialize();
    result
}

unsafe fn extract_elements() -> Result<Vec<UIElement>, String> {
    let automation: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_ALL)
        .map_err(|e| format!("CoCreateInstance CUIAutomation: {e}"))?;

    let root = automation
        .GetRootElement()
        .map_err(|e| format!("GetRootElement: {e}"))?;

    let condition = automation
        .CreateTrueCondition()
        .map_err(|e| format!("CreateTrueCondition: {e}"))?;

    let all = root
        .FindAll(TreeScope_Subtree, &condition)
        .map_err(|e| format!("FindAll: {e}"))?;

    let count = all.Length().unwrap_or(0);
    log::info!("[UIA] raw element count: {count}");

    let mut elements = Vec::with_capacity(count as usize);

    for i in 0..count {
        let Ok(elem) = all.GetElement(i) else { continue };

        // Use direct property accessors — safer than GetCurrentPropertyValue + VARIANT parsing.
        let control_type_id = elem.CurrentControlType()
            .map(|ct| ct.0)
            .unwrap_or(0);

        if !is_interactive_type(control_type_id) {
            continue;
        }

        let name = elem.CurrentName()
            .map(|s| s.to_string())
            .unwrap_or_default();

        let automation_id = elem.CurrentAutomationId()
            .map(|s| s.to_string())
            .unwrap_or_default();

        let is_enabled = elem.CurrentIsEnabled().map(|b| b.as_bool()).unwrap_or(false);

        let rect = match elem.CurrentBoundingRectangle() {
            Ok(r) => RectL {
                x: r.left as f64,
                y: r.top as f64,
                width: (r.right - r.left) as f64,
                height: (r.bottom - r.top) as f64,
            },
            Err(_) => continue,
        };

        if rect.width < 4.0 || rect.height < 4.0 {
            continue;
        }

        // Skip elements with garbage names (single punctuation, whitespace-only).
        if !is_meaningful_name(&name) && automation_id.is_empty() {
            continue;
        }

        elements.push(UIElement {
            name,
            control_type: control_type_name(control_type_id).to_string(),
            rect,
            is_enabled,
            automation_id,
        });
    }

    elements.sort_by(|a, b| {
        let area_a = a.rect.width * a.rect.height;
        let area_b = b.rect.width * b.rect.height;
        area_a.partial_cmp(&area_b).unwrap_or(std::cmp::Ordering::Equal)
    });

    log::info!("[UIA] extracted {} interactive elements", elements.len());
    Ok(elements)
}
