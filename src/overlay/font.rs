/// cosmic-text font system — loaded once at startup, never reloaded per frame.
use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping};

/// System font paths for speech bubbles (priority order, first match wins).
#[cfg(target_os = "windows")]
const FONT_PATHS: &[&str] = &[
    "C:\\Windows\\Fonts\\segoeui.ttf",
    "C:\\Windows\\Fonts\\arial.ttf",
    "C:\\Windows\\Fonts\\verdana.ttf",
];

#[cfg(target_os = "linux")]
const FONT_PATHS: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation2/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/opentype/urw-base35/NimbusSans-Regular.otf",
    "/usr/share/fonts/truetype/ubuntu/Ubuntu-R.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/adwaita-sans-fonts/AdwaitaSans-Regular.ttf",
];

#[cfg(target_os = "macos")]
const FONT_PATHS: &[&str] = &[
    "/System/Library/Fonts/SFNS.ttf",
    "/System/Library/Fonts/HelveticaNeue.ttc",
    "/System/Library/Fonts/Supplemental/Arial.ttf",
];

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
const FONT_PATHS: &[&str] = &[];

/// Creates a `FontSystem` pre-loaded with the best available system font.
/// Falls back to cosmic-text's built-in font search if no system path matches.
pub fn create_font_system() -> FontSystem {
    let mut font_system = FontSystem::new();

    for path in FONT_PATHS {
        if std::path::Path::new(path).exists() {
            match std::fs::read(path) {
                Ok(bytes) => {
                    font_system.db_mut().load_font_data(bytes);
                    log::info!("Loaded bubble font: {}", path);
                    return font_system;
                }
                Err(e) => {
                    log::debug!("Failed to read font {}: {}", path, e);
                }
            }
        }
    }

    log::warn!("No system font loaded — cosmic-text will use its built-in font search");
    font_system
}

/// Creates a reusable `Buffer` for rendering one line of speech bubble text.
/// The buffer is sized to the given pixel dimensions and font size.
pub fn create_text_buffer(
    font_system: &mut FontSystem,
    width_px: f32,
    height_px: f32,
    font_size_px: f32,
    line_height_px: f32,
) -> Buffer {
    let metrics = Metrics::new(font_size_px, line_height_px);
    let mut buffer = Buffer::new(font_system, metrics);
    buffer.set_size(font_system, Some(width_px), Some(height_px));
    buffer.set_text(font_system, "", Attrs::new(), Shaping::Advanced);
    buffer
}
