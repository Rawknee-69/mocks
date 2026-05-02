use clicky_types::DisplayInfo;

/// A coordinate in the global (virtual desktop) coordinate system.
#[derive(Debug, Clone, Copy)]
pub struct GlobalDisplayCoordinate {
    pub x: f64,
    pub y: f64,
}

/// Maps a screenshot-pixel coordinate to a global display coordinate.
///
/// Steps:
/// 1. Clamp to screenshot pixel bounds.
/// 2. Scale from screenshot pixels to logical display points.
/// 3. Add the display's global origin offset.
pub fn map_screenshot_pixels_to_global(
    screenshot_pixel_x: f64,
    screenshot_pixel_y: f64,
    display: &DisplayInfo,
) -> GlobalDisplayCoordinate {
    let clamped_x = screenshot_pixel_x.clamp(0.0, display.screenshot_width_pixels as f64 - 1.0);
    let clamped_y = screenshot_pixel_y.clamp(0.0, display.screenshot_height_pixels as f64 - 1.0);

    let scale_x = display.display_width_points / display.screenshot_width_pixels as f64;
    let scale_y = display.display_height_points / display.screenshot_height_pixels as f64;

    GlobalDisplayCoordinate {
        x: display.global_origin_x + clamped_x * scale_x,
        y: display.global_origin_y + clamped_y * scale_y,
    }
}

/// Finds the display for a POINT tag's screen number.
///
/// `None` screen number → cursor's display. `Some(n)` → display with that number.
pub fn find_target_display<'a>(
    screen_number: Option<u32>,
    displays: &'a [DisplayInfo],
) -> Option<&'a DisplayInfo> {
    match screen_number {
        Some(n) => displays.iter().find(|d| d.screen_number == n),
        None => displays.iter().find(|d| d.is_cursor_display),
    }
}
