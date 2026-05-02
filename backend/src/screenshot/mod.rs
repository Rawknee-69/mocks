/// Multi-monitor screenshot capture for the backend pipeline.
///
/// Platform strategy (silent, no flash):
///   Hyprland/Sway  → grim (wlroots tool, outputs PNG to stdout)
///   GNOME Wayland  → xcap with animation suppression
///   X11 / Windows  → xcap
use crate::api::claude::ScreenshotForClaude;
use crate::error::AppError;
use crate::platform::{DisplayServer, PlatformInfo, WaylandCompositor};
use clicky_types::DisplayInfo;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use std::io::Cursor;

const MAX_WIDTH: u32 = 1280;
const JPEG_QUALITY: u8 = 80;

pub struct CaptureResult {
    pub screenshots: Vec<ScreenshotForClaude>,
    pub display_infos: Vec<DisplayInfo>,
}

#[cfg(target_os = "windows")]
fn windows_virtual_desktop_bounds() -> Option<(i32, i32, i32, i32)> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    let origin_x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let origin_y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    if width > 0 && height > 0 {
        Some((origin_x, origin_y, width, height))
    } else {
        None
    }
}

#[cfg(not(target_os = "windows"))]
fn windows_virtual_desktop_bounds() -> Option<(i32, i32, i32, i32)> {
    None
}

/// Captures all monitors using the best method for the current platform.
pub fn capture_all_screens(
    cursor_x: f32,
    cursor_y: f32,
    platform: &PlatformInfo,
) -> Result<CaptureResult, AppError> {
    if platform.display_server == Some(DisplayServer::Wayland) {
        match &platform.wayland_compositor {
            Some(WaylandCompositor::Hyprland | WaylandCompositor::Sway) => {
                if let Ok(result) = capture_with_grim(cursor_x, cursor_y) {
                    return Ok(result);
                }
                tracing::debug!("grim not available, falling back to xcap");
            }
            _ => {
                return capture_gnome_no_flash(cursor_x, cursor_y);
            }
        }
    }
    capture_with_xcap(cursor_x, cursor_y)
}

fn capture_gnome_no_flash(cursor_x: f32, cursor_y: f32) -> Result<CaptureResult, AppError> {
    let animations_on = gsettings_get_bool("org.gnome.desktop.interface", "enable-animations");
    let sounds_on = gsettings_get_bool("org.gnome.desktop.sound", "event-sounds");

    if animations_on {
        gsettings_set("org.gnome.desktop.interface", "enable-animations", "false");
    }
    if sounds_on {
        gsettings_set("org.gnome.desktop.sound", "event-sounds", "false");
    }

    let result = capture_with_xcap(cursor_x, cursor_y);

    if animations_on {
        gsettings_set("org.gnome.desktop.interface", "enable-animations", "true");
    }
    if sounds_on {
        gsettings_set("org.gnome.desktop.sound", "event-sounds", "true");
    }

    result
}

fn gsettings_get_bool(schema: &str, key: &str) -> bool {
    std::process::Command::new("gsettings")
        .args(["get", schema, key])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "true")
        .unwrap_or(true)
}

fn gsettings_set(schema: &str, key: &str, value: &str) {
    let _ = std::process::Command::new("gsettings")
        .args(["set", schema, key, value])
        .output();
}

#[derive(Debug, Clone)]
struct WlrMonitorInfo {
    name: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

fn query_wlr_monitors() -> Option<Vec<WlrMonitorInfo>> {
    let output = std::process::Command::new("hyprctl")
        .args(["monitors", "-j"])
        .output()
        .ok()?;

    if !output.status.success() {
        let output = std::process::Command::new("swaymsg")
            .args(["-t", "get_outputs", "--raw"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        return parse_sway_monitors(&output.stdout);
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let monitors = json.as_array()?;

    let mut result = Vec::new();
    for monitor in monitors {
        let name = monitor["name"].as_str().unwrap_or("").to_string();
        let x = monitor["x"].as_f64().unwrap_or(0.0);
        let y = monitor["y"].as_f64().unwrap_or(0.0);
        let width = monitor["width"].as_f64().unwrap_or(1920.0);
        let height = monitor["height"].as_f64().unwrap_or(1080.0);
        let scale = monitor["scale"].as_f64().unwrap_or(1.0);
        result.push(WlrMonitorInfo {
            name,
            x,
            y,
            width: width / scale,
            height: height / scale,
        });
    }

    Some(result)
}

fn parse_sway_monitors(stdout: &[u8]) -> Option<Vec<WlrMonitorInfo>> {
    let json: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let outputs = json.as_array()?;

    let mut result = Vec::new();
    for output in outputs {
        if !output["active"].as_bool().unwrap_or(false) {
            continue;
        }
        let name = output["name"].as_str().unwrap_or("").to_string();
        let rect = &output["rect"];
        let x = rect["x"].as_f64().unwrap_or(0.0);
        let y = rect["y"].as_f64().unwrap_or(0.0);
        let width = rect["width"].as_f64().unwrap_or(1920.0);
        let height = rect["height"].as_f64().unwrap_or(1080.0);
        let scale = output["scale"].as_f64().unwrap_or(1.0);
        result.push(WlrMonitorInfo {
            name,
            x,
            y,
            width: width / scale,
            height: height / scale,
        });
    }

    Some(result)
}

fn capture_with_grim(cursor_x: f32, cursor_y: f32) -> Result<CaptureResult, AppError> {
    let monitors = query_wlr_monitors()
        .ok_or_else(|| AppError::Screenshot("cannot query monitors via hyprctl/swaymsg".into()))?;

    if monitors.is_empty() {
        return Err(AppError::Screenshot("no monitors found".into()));
    }

    let total = monitors.len();
    let mut screenshots = Vec::with_capacity(total);
    let mut display_infos = Vec::with_capacity(total);

    for (i, monitor) in monitors.iter().enumerate() {
        let screen_num = (i + 1) as u32;

        let output = std::process::Command::new("grim")
            .arg("-o")
            .arg(&monitor.name)
            .arg("-t")
            .arg("png")
            .arg("-")
            .output()
            .map_err(|e| AppError::Screenshot(format!("grim not found: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AppError::Screenshot(format!(
                "grim failed for {}: {stderr}",
                monitor.name
            )));
        }

        let img =
            image::load_from_memory_with_format(&output.stdout, image::ImageFormat::Png)
                .map_err(|e| AppError::Screenshot(format!("decode grim output: {e}")))?
                .to_rgba8();

        tracing::info!(
            "grim captured {}: {}x{} pixels (logical {}x{})",
            monitor.name, img.width(), img.height(), monitor.width, monitor.height,
        );

        let mut scaled = scale_image(&img);
        draw_coordinate_grid(&mut scaled);
        let (sw, sh) = (scaled.width(), scaled.height());
        let jpeg_data = encode_jpeg(&scaled)?;

        let is_cursor =
            cursor_x as f64 >= monitor.x
                && (cursor_x as f64) < monitor.x + monitor.width
                && cursor_y as f64 >= monitor.y
                && (cursor_y as f64) < monitor.y + monitor.height;

        screenshots.push(ScreenshotForClaude {
            jpeg_data,
            label: make_label(screen_num, total, sw, sh, is_cursor),
        });
        display_infos.push(DisplayInfo {
            screen_number: screen_num,
            global_origin_x: monitor.x,
            global_origin_y: monitor.y,
            display_width_points: monitor.width,
            display_height_points: monitor.height,
            screenshot_width_pixels: sw,
            screenshot_height_pixels: sh,
            is_cursor_display: is_cursor,
        });
    }

    tracing::info!(
        "Screenshot captured: {} screen(s)",
        total,
    );

    Ok(CaptureResult { screenshots, display_infos })
}

fn capture_with_xcap(cursor_x: f32, cursor_y: f32) -> Result<CaptureResult, AppError> {
    let monitors =
        xcap::Monitor::all().map_err(|e| AppError::Screenshot(e.to_string()))?;

    if monitors.is_empty() {
        return Err(AppError::Screenshot("no monitors found".into()));
    }

    let total = monitors.len();
    let mut screenshots = Vec::with_capacity(total);
    let mut display_infos = Vec::with_capacity(total);

    for (i, monitor) in monitors.iter().enumerate() {
        let screen_num = (i + 1) as u32;

        let mon_x = monitor.x().unwrap_or(0) as f32;
        let mon_y = monitor.y().unwrap_or(0) as f32;
        let mon_w = monitor.width().unwrap_or(1920) as f32;
        let mon_h = monitor.height().unwrap_or(1080) as f32;
        let is_cursor =
            cursor_x >= mon_x
                && cursor_x < mon_x + mon_w
                && cursor_y >= mon_y
                && cursor_y < mon_y + mon_h;

        let img = monitor
            .capture_image()
            .map_err(|e| AppError::Screenshot(format!("monitor {screen_num}: {e}")))?;

        let mut scaled = scale_image(&img);
        draw_coordinate_grid(&mut scaled);
        let (sw, sh) = (scaled.width(), scaled.height());
        let jpeg_data = encode_jpeg(&scaled)?;

        screenshots.push(ScreenshotForClaude {
            jpeg_data,
            label: make_label(screen_num, total, sw, sh, is_cursor),
        });
        display_infos.push(DisplayInfo {
            screen_number: screen_num,
            global_origin_x: mon_x as f64,
            global_origin_y: mon_y as f64,
            display_width_points: mon_w as f64,
            display_height_points: mon_h as f64,
            screenshot_width_pixels: sw,
            screenshot_height_pixels: sh,
            is_cursor_display: is_cursor,
        });
    }

    tracing::info!("Screenshot captured: {} screen(s)", total);
    Ok(CaptureResult { screenshots, display_infos })
}

fn make_label(screen_num: u32, total: usize, sw: u32, sh: u32, is_cursor: bool) -> String {
    if total == 1 {
        format!("screen 1 of 1 (image dimensions: {sw}x{sh} pixels)")
    } else if is_cursor {
        format!(
            "screen {screen_num} of {total} (image dimensions: {sw}x{sh} pixels) — cursor is on this screen (primary focus)"
        )
    } else {
        format!("screen {screen_num} of {total} (image dimensions: {sw}x{sh} pixels) — secondary screen")
    }
}

fn scale_image(img: &image::RgbaImage) -> image::RgbaImage {
    let (w, h) = (img.width(), img.height());
    if w > MAX_WIDTH {
        let new_h = (h as f64 * MAX_WIDTH as f64 / w as f64) as u32;
        image::imageops::resize(img, MAX_WIDTH, new_h, FilterType::Lanczos3)
    } else {
        img.clone()
    }
}

fn draw_coordinate_grid(img: &mut image::RgbaImage) {
    let enabled = std::env::var("CLICKY_GRID_OVERLAY")
        .map(|v| v != "0")
        .unwrap_or(true);
    if !enabled {
        return;
    }

    let (w, h) = (img.width(), img.height());
    let line_color = image::Rgba([255, 0, 0, 50]);
    let label_color = image::Rgba([255, 0, 0, 160]);

    for x in (200..w).step_by(200) {
        for y in 0..h {
            blend_pixel(img, x, y, line_color);
        }
    }
    for y in (200..h).step_by(200) {
        for x in 0..w {
            blend_pixel(img, x, y, line_color);
        }
    }
    for gx in (200..w).step_by(200) {
        for gy in (200..h).step_by(200) {
            draw_tiny_text(img, gx + 2, gy + 2, &format!("{gx},{gy}"), label_color);
        }
    }
    for gx in (200..w).step_by(200) {
        draw_tiny_text(img, gx + 2, 2, &format!("{gx}"), label_color);
    }
    for gy in (200..h).step_by(200) {
        draw_tiny_text(img, 2, gy + 2, &format!("{gy}"), label_color);
    }
}

fn blend_pixel(img: &mut image::RgbaImage, x: u32, y: u32, color: image::Rgba<u8>) {
    if x >= img.width() || y >= img.height() {
        return;
    }
    let bg = img.get_pixel(x, y);
    let alpha = color[3] as f32 / 255.0;
    let inv = 1.0 - alpha;
    img.put_pixel(
        x,
        y,
        image::Rgba([
            (color[0] as f32 * alpha + bg[0] as f32 * inv) as u8,
            (color[1] as f32 * alpha + bg[1] as f32 * inv) as u8,
            (color[2] as f32 * alpha + bg[2] as f32 * inv) as u8,
            255,
        ]),
    );
}

fn draw_tiny_text(img: &mut image::RgbaImage, x: u32, y: u32, text: &str, color: image::Rgba<u8>) {
    const GLYPHS: &[(char, [u8; 5])] = &[
        ('0', [0b111, 0b101, 0b101, 0b101, 0b111]),
        ('1', [0b010, 0b110, 0b010, 0b010, 0b111]),
        ('2', [0b111, 0b001, 0b111, 0b100, 0b111]),
        ('3', [0b111, 0b001, 0b111, 0b001, 0b111]),
        ('4', [0b101, 0b101, 0b111, 0b001, 0b001]),
        ('5', [0b111, 0b100, 0b111, 0b001, 0b111]),
        ('6', [0b111, 0b100, 0b111, 0b101, 0b111]),
        ('7', [0b111, 0b001, 0b001, 0b001, 0b001]),
        ('8', [0b111, 0b101, 0b111, 0b101, 0b111]),
        ('9', [0b111, 0b101, 0b111, 0b001, 0b111]),
        (',', [0b000, 0b000, 0b000, 0b010, 0b100]),
    ];

    let mut cx = x;
    for ch in text.chars() {
        if let Some((_, bitmap)) = GLYPHS.iter().find(|(c, _)| *c == ch) {
            for (row, bits) in bitmap.iter().enumerate() {
                for col in 0..3u32 {
                    if bits & (0b100 >> col) != 0 {
                        let px = cx + col;
                        let py = y + row as u32;
                        if px < img.width() && py < img.height() {
                            img.put_pixel(px, py, color);
                        }
                    }
                }
            }
        }
        cx += 4;
    }
}

fn encode_jpeg(img: &image::RgbaImage) -> Result<Vec<u8>, AppError> {
    let mut buf = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(Cursor::new(&mut buf), JPEG_QUALITY);
    encoder
        .encode_image(img)
        .map_err(|e| AppError::Screenshot(format!("JPEG encode: {e}")))?;
    Ok(buf)
}
