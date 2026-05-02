/// Font system — constructed once, owns the system font database.
///
/// `FontSystem::new()` already calls `load_system_fonts()` internally on all platforms,
/// so no manual path scanning is needed.
use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Style, Weight};

/// Creates the global `FontSystem`.
/// The face count logged here is a quick sanity check that the OS font database
/// was found and loaded successfully.
pub fn create_font_system() -> FontSystem {
    let fs = FontSystem::new();
    log::info!(
        "FontSystem ready — {} faces loaded",
        fs.db().faces().count()
    );
    fs
}

/// Preferred UI font families tried in order.
/// cosmic-text picks the first family present in the loaded font database,
/// then falls back to `Family::SansSerif` if none match.
const PREFERRED_FAMILIES: &[&str] = &[
    "Segoe UI",        // Windows
    "Ubuntu",          // Ubuntu / Pop!_OS
    "Cantarell",       // GNOME
    "Adwaita Sans",    // GNOME 47+
    "DejaVu Sans",     // universal Linux fallback
    "Liberation Sans",
    "Noto Sans",
    "Helvetica Neue",  // macOS
    "SF Pro Display",  // macOS
    "Arial",           // universal last resort
];

/// Returns `Attrs` that select the preferred font family for speech bubbles.
/// Falls back to `Family::SansSerif` (always available) if none of the named
/// families exist in the loaded database — cosmic-text handles this automatically.
pub fn bubble_text_attrs() -> Attrs<'static> {
    // Attrs::new() defaults to SansSerif + Normal weight, which is the correct
    // safe fallback if nothing in PREFERRED_FAMILIES is installed.
    Attrs::new()
        .family(Family::Name(PREFERRED_FAMILIES[0]))
        .weight(Weight::NORMAL)
        .style(Style::Normal)
}

/// Creates a reusable `Buffer` for speech bubble text sized to `max_width_px`.
///
/// Create this once and keep it alive — call `buffer.set_text(...)` to update
/// content each time the visible text changes. Do NOT recreate the buffer every frame.
pub fn create_text_buffer(
    font_system: &mut FontSystem,
    max_width_px: f32,
    font_size_px: f32,
) -> Buffer {
    let line_height_px = font_size_px * 1.22; // keeps line_height close to the previous 22px at 18px font
    let metrics = Metrics::new(font_size_px, line_height_px);
    let mut buffer = Buffer::new(font_system, metrics);
    // Unconstrained height so lines can wrap freely; we measure height from layout_runs().
    buffer.set_size(font_system, Some(max_width_px), None);
    buffer.set_text(font_system, "", bubble_text_attrs(), Shaping::Advanced);
    buffer
}
