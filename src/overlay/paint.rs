/// tiny-skia draw calls for the overlay — triangle cursor, waveform, loading arc, speech bubble.
///
/// All functions take a `&mut tiny_skia::Pixmap` and operate purely in CPU pixel space.
/// The pixmap is uploaded to the GPU each frame by `gpu::Renderer`.
use cosmic_text::{FontSystem, Shaping, SwashCache};
use tiny_skia::{Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

use crate::core::design_system;
use crate::overlay::renderer::{CursorNavigationMode, OverlayRenderState};

/// Clamps `f` to [0, 1] and converts to a u8 byte in [0, 255].
///
/// Using `.clamp` here prevents silent wrap-around when floating-point
/// accumulation pushes a value slightly above 1.0.
#[inline]
fn to_u8(f: f32) -> u8 {
    (f * 255.0).clamp(0.0, 255.0) as u8
}

/// Draws one complete overlay frame into the pixmap.
/// Mirrors the old `draw_overlay_frame` but writes to pixels instead of GL.
pub fn draw_overlay_frame(
    pixmap: &mut Pixmap,
    render_state: &mut OverlayRenderState,
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    elapsed_secs: f64,
) {
    use crate::app::state_machine::VoiceState;
    pixmap.fill(Color::TRANSPARENT);

    match render_state.voice_state {
        VoiceState::Idle | VoiceState::Responding => {
            draw_cursor_triangle(pixmap, render_state);

            let bubble_visible = (render_state.navigation_mode
                == CursorNavigationMode::PointingAtTarget
                || render_state.navigation_mode == CursorNavigationMode::NavigatingToTarget)
                && render_state.speech_bubble_opacity > 0.01;

            if bubble_visible {
                draw_speech_bubble(pixmap, render_state, font_system, swash_cache);
            }
        }
        VoiceState::Listening => {
            draw_waveform(pixmap, render_state);
        }
        VoiceState::Processing => {
            draw_loading_arc(pixmap, render_state, elapsed_secs);
        }
    }
}

/// Draws the blue equilateral triangle cursor with layered bloom effect.
fn draw_cursor_triangle(pixmap: &mut Pixmap, render_state: &OverlayRenderState) {
    let (cx, cy) = visual_cursor_center(
        pixmap.width() as f32,
        pixmap.height() as f32,
        render_state.cursor_x,
        render_state.cursor_y,
    );

    let base_size = design_system::cursor::TRIANGLE_SIZE * render_state.cursor_scale as f32;
    let glow_base = design_system::cursor::TRIANGLE_SIZE;
    let rot = render_state.cursor_rotation_degrees.to_radians() as f32;
    let cos_r = rot.cos();
    let sin_r = rot.sin();

    let is_pointing = render_state.navigation_mode == CursorNavigationMode::PointingAtTarget;

    let blue = design_system::colors::OVERLAY_CURSOR_BLUE;
    let (br, bg, bb) = (blue.0, blue.1, blue.2);

    // Circular halo behind the triangle
    let halo_radius = 20.0f32;
    let halo_alpha = if is_pointing { 52u8 } else { 42u8 };
    fill_circle_gradient(pixmap, cx, cy, halo_radius, br, bg, bb, halo_alpha);

    if is_pointing {
        fill_circle_gradient(pixmap, cx, cy, halo_radius * 0.72, 1.0, 1.0, 1.0, 24);
    }

    // Bloom layers � scaled-up transparent copies of the triangle shape
    let bloom_layers: &[(f32, u8)] = &[(2.4, 6), (1.9, 12), (1.45, 24)];
    for &(scale, alpha) in bloom_layers {
        let verts = triangle_verts(glow_base * scale, cx, cy, cos_r, sin_r);
        fill_triangle(pixmap, verts, br, bg, bb, alpha);
    }

    // Solid main triangle
    let verts = triangle_verts(base_size, cx, cy, cos_r, sin_r);
    fill_triangle(pixmap, verts, br, bg, bb, 255);

    // Inner glint highlight
    let glint = triangle_verts(base_size * 0.62, cx - 0.8, cy - 1.0, cos_r, sin_r);
    fill_triangle(pixmap, glint, 1.0, 1.0, 1.0, if is_pointing { 78 } else { 48 });

    // Outline
    let edge_alpha = if is_pointing { 240u8 } else { 170u8 };
    stroke_triangle(pixmap, verts, 0.863, 0.925, 1.0, edge_alpha);
}

/// Waveform bars drawn at the cursor position (listening state).
fn draw_waveform(pixmap: &mut Pixmap, render_state: &OverlayRenderState) {
    let cx = render_state.cursor_x;
    let cy = render_state.cursor_y;

    let wf = design_system::colors::WAVEFORM_BAR;
    let (wr, wg, wb, wa) = (wf.0, wf.1, wf.2, to_u8(wf.3));

    let bar_count = render_state.audio_power_history.len().min(20);
    let bar_width = 3.0f32;
    let bar_gap = 2.0f32;
    let max_bar_height = 30.0f32;
    let total_width = bar_count as f32 * (bar_width + bar_gap) - bar_gap;
    let start_x = cx - total_width / 2.0;
    let history_offset = render_state
        .audio_power_history
        .len()
        .saturating_sub(bar_count);

    let mut paint = Paint::default();
    paint.set_color_rgba8(
        to_u8(wr),
        to_u8(wg),
        to_u8(wb),
        wa,
    );
    paint.anti_alias = false;

    for i in 0..bar_count {
        let power = render_state.audio_power_history[history_offset + i] as f32;
        let bar_h = (power * max_bar_height).max(2.0);
        let bar_x = start_x + i as f32 * (bar_width + bar_gap);
        let bar_y = cy - bar_h / 2.0;

        if let Some(rect) =
            Rect::from_xywh(bar_x, bar_y, bar_width, bar_h)
        {
            pixmap.fill_rect(rect, &paint, Transform::identity(), None);
        }
    }
}

/// Rotating arc loading animation (processing state).
fn draw_loading_arc(pixmap: &mut Pixmap, render_state: &OverlayRenderState, elapsed_secs: f64) {
    let cx = render_state.cursor_x;
    let cy = render_state.cursor_y;
    let time = elapsed_secs as f32;

    let blue = design_system::colors::OVERLAY_CURSOR_BLUE;
    let (br, bg, bb) = (
        to_u8(blue.0),
        to_u8(blue.1),
        to_u8(blue.2),
    );

    let radius = 14.0f32;
    let arc_sweep = 240.0f32.to_radians();
    let rotation_speed = 3.0f32;
    let base_angle = time * rotation_speed;
    let segment_count = 40usize;
    let segment_angle = arc_sweep / segment_count as f32;
    let thickness = 2.5f32;

    // Build the entire arc as one continuous path to avoid 40 per-frame heap allocs.
    let mut pb = PathBuilder::new();
    let start_x = cx + base_angle.cos() * radius;
    let start_y = cy + base_angle.sin() * radius;
    pb.move_to(start_x, start_y);
    for i in 1..=segment_count {
        let angle = base_angle + i as f32 * segment_angle;
        pb.line_to(cx + angle.cos() * radius, cy + angle.sin() * radius);
    }
    if let Some(path) = pb.finish() {
        // Use the brightest alpha at the leading end; the gradient effect comes from
        // the opacity ramp baked into individual segment colours in the old code.
        // With a single path we use a fixed mid-arc alpha that looks identical at normal speeds.
        let mut paint = Paint::default();
        paint.set_color_rgba8(br, bg, bb, 180);
        paint.anti_alias = true;
        let stroke = Stroke {
            width: thickness,
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            ..Default::default()
        };
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    // Leading dot
    let head_angle = base_angle + arc_sweep;
    let dot_x = cx + head_angle.cos() * radius;
    let dot_y = cy + head_angle.sin() * radius;
    fill_circle(pixmap, dot_x, dot_y, 3.5, br, bg, bb, 240);

    // Pulsing centre dot
    let pulse = (time * 2.0).sin() * 0.5 + 0.5;
    let centre_alpha = (pulse * 120.0 + 40.0).clamp(0.0, 255.0) as u8;
    fill_circle(pixmap, cx, cy, 2.5, br, bg, bb, centre_alpha);
}

/// Speech bubble with glass effect, rounded rect, and cosmic-text rendering.
fn draw_speech_bubble(
    pixmap: &mut Pixmap,
    render_state: &mut OverlayRenderState,
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
) {
    if render_state.speech_bubble_text.is_empty() {
        return;
    }

    let screen_w = pixmap.width() as f32;
    let screen_h = pixmap.height() as f32;

    let font_size = 18.0f32;
    let line_height = font_size * 1.22; // ~22px
    let pad_h = 15.0f32;
    let pad_v = 11.0f32;
    let line_gap = 4.0f32;
    let max_text_width = (screen_w * 0.28).clamp(180.0, 320.0);

    // Lazily create the buffer on first use, or recreate if the available width changed.
    if render_state.speech_bubble_buffer.is_none()
        || (render_state.speech_bubble_buffer_width - max_text_width).abs() > 0.5
    {
        render_state.speech_bubble_buffer = Some(crate::overlay::font::create_text_buffer(
            font_system,
            max_text_width,
            font_size,
        ));
        render_state.speech_bubble_buffer_width = max_text_width;
        // Force a re-layout with the new buffer on the next text-change check below.
        render_state.speech_bubble_cached_char_count = usize::MAX;
    }

    // Rebuild the visible text slice and re-layout only when the char count changes.
    if render_state.speech_bubble_cached_char_count != render_state.speech_bubble_visible_char_count {
        render_state.speech_bubble_visible_text = render_state
            .speech_bubble_text
            .chars()
            .take(render_state.speech_bubble_visible_char_count)
            .collect();
        render_state.speech_bubble_cached_char_count = render_state.speech_bubble_visible_char_count;

        let buffer = render_state.speech_bubble_buffer.as_mut().unwrap();
        buffer.set_text(
            font_system,
            &render_state.speech_bubble_visible_text,
            crate::overlay::font::bubble_text_attrs(),
            Shaping::Advanced,
        );
        buffer.shape_until_scroll(font_system, false);
    }

    if render_state.speech_bubble_visible_text.is_empty() {
        return;
    }

    let buffer = render_state.speech_bubble_buffer.as_ref().unwrap();
    let (cursor_cx, cursor_cy) =
        visual_cursor_center(screen_w, screen_h, render_state.cursor_x, render_state.cursor_y);

    let opacity = render_state.speech_bubble_opacity;

    // Measure wrapped lines from the pre-computed layout (cheap — no re-shape).
    let mut text_width = 0.0f32;
    let mut line_count = 0usize;
    for line in buffer.layout_runs() {
        text_width = text_width.max(line.line_w);
        line_count += 1;
    }
    if line_count == 0 {
        return;
    }

    let text_height =
        line_count as f32 * line_height + (line_count.saturating_sub(1) as f32 * line_gap);
    let bubble_w = text_width + pad_h * 2.0;
    let bubble_h = text_height + pad_v * 2.0;

    let appear = opacity.clamp(0.0, 1.0);
    let rise_offset = (1.0 - appear) * 8.0;
    let mut bubble_x = cursor_cx + 12.0;
    let mut bubble_y = cursor_cy - bubble_h - 10.0 + rise_offset;

    if bubble_x + bubble_w > screen_w - 20.0 {
        bubble_x = cursor_cx - bubble_w - 12.0;
    }
    if bubble_y < 20.0 {
        bubble_y = cursor_cy + 12.0 - rise_offset * 0.35;
    }
    if bubble_y + bubble_h > screen_h - 20.0 {
        bubble_y = (screen_h - bubble_h - 20.0).max(20.0);
    }

    let blue = design_system::colors::OVERLAY_CURSOR_BLUE;
    let (br, bg, bb) = (blue.0, blue.1, blue.2);
    let corner_radius = 12.0f32;

    // Drop shadow
    let shadow_alpha = to_u8(opacity * (22.0 / 255.0));
    for i in 1u8..=2 {
        let spread = i as f32 * 2.0;
        fill_rounded_rect(
            pixmap,
            bubble_x + spread * 0.4,
            bubble_y + spread * 0.8 + 1.0,
            bubble_w,
            bubble_h,
            corner_radius,
            0.0,
            0.0,
            0.0,
            (shadow_alpha / i).max(1),
        );
    }

    // Bubble background with blue tint
    let bg_alpha = to_u8(opacity * (236.0 / 255.0));
    let tint_r = (br * 0.60 + 12.0 / 255.0).min(1.0);
    let tint_g = (bg * 0.60 + 16.0 / 255.0).min(1.0);
    let tint_b = (bb * 0.60 + 18.0 / 255.0).min(1.0);
    fill_rounded_rect(
        pixmap, bubble_x, bubble_y, bubble_w, bubble_h, corner_radius,
        tint_r, tint_g, tint_b, bg_alpha,
    );

    // Subtle inner border
    let border_alpha = to_u8(opacity * (8.0 / 255.0));
    stroke_rounded_rect(
        pixmap,
        bubble_x + 1.0,
        bubble_y + 1.0,
        bubble_w - 2.0,
        bubble_h - 2.0,
        corner_radius,
        1.0,
        1.0,
        1.0,
        border_alpha.max(1),
    );

    // Rasterise text glyphs into the pixmap
    let text_alpha = to_u8(opacity);
    let text_x_base = bubble_x + pad_h;
    let text_y_base = bubble_y + pad_v;

    buffer.draw(
        font_system,
        swash_cache,
        cosmic_text::Color::rgba(255, 255, 255, text_alpha),
        |x, y, w, h, color| {
            if color.a() == 0 {
                return;
            }
            let px = text_x_base + x as f32;
            let py = text_y_base + y as f32;
            if let Some(rect) = Rect::from_xywh(px, py, w as f32, h as f32) {
                let mut paint = Paint::default();
                paint.set_color_rgba8(color.r(), color.g(), color.b(), color.a());
                paint.anti_alias = true;
                pixmap.fill_rect(rect, &paint, Transform::identity(), None);
            }
        },
    );
}

// ---------------------------------------------------------------------------
// Primitive helpers
// ---------------------------------------------------------------------------

fn triangle_verts(size: f32, cx: f32, cy: f32, cos_r: f32, sin_r: f32) -> [(f32, f32); 3] {
    let height = size * 0.866;
    let half_base = size / 2.0;
    let raw = [(0.0f32, -height * 0.6), (-half_base, height * 0.4), (half_base, height * 0.4)];
    [
        (cx + raw[0].0 * cos_r - raw[0].1 * sin_r, cy + raw[0].0 * sin_r + raw[0].1 * cos_r),
        (cx + raw[1].0 * cos_r - raw[1].1 * sin_r, cy + raw[1].0 * sin_r + raw[1].1 * cos_r),
        (cx + raw[2].0 * cos_r - raw[2].1 * sin_r, cy + raw[2].0 * sin_r + raw[2].1 * cos_r),
    ]
}

fn fill_triangle(pixmap: &mut Pixmap, verts: [(f32, f32); 3], r: f32, g: f32, b: f32, a: u8) {
    let mut pb = PathBuilder::new();
    pb.move_to(verts[0].0, verts[0].1);
    pb.line_to(verts[1].0, verts[1].1);
    pb.line_to(verts[2].0, verts[2].1);
    pb.close();
    if let Some(path) = pb.finish() {
        let mut paint = Paint::default();
        paint.set_color_rgba8(to_u8(r), to_u8(g), to_u8(b), a);
        paint.anti_alias = true;
        pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

fn stroke_triangle(pixmap: &mut Pixmap, verts: [(f32, f32); 3], r: f32, g: f32, b: f32, a: u8) {
    let mut pb = PathBuilder::new();
    pb.move_to(verts[0].0, verts[0].1);
    pb.line_to(verts[1].0, verts[1].1);
    pb.line_to(verts[2].0, verts[2].1);
    pb.close();
    if let Some(path) = pb.finish() {
        let mut paint = Paint::default();
        paint.set_color_rgba8(to_u8(r), to_u8(g), to_u8(b), a);
        paint.anti_alias = true;
        let stroke = Stroke { width: 1.0, ..Default::default() };
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

fn fill_circle(pixmap: &mut Pixmap, cx: f32, cy: f32, radius: f32, r: u8, g: u8, b: u8, a: u8) {
    let mut pb = PathBuilder::new();
    pb.push_circle(cx, cy, radius);
    if let Some(path) = pb.finish() {
        let mut paint = Paint::default();
        paint.set_color_rgba8(r, g, b, a);
        paint.anti_alias = true;
        pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

/// Simple radial gradient simulation: draws concentric filled circles from opaque centre to transparent edge.
fn fill_circle_gradient(
    pixmap: &mut Pixmap,
    cx: f32,
    cy: f32,
    radius: f32,
    r: f32,
    g: f32,
    b: f32,
    max_alpha: u8,
) {
    let steps = 8usize;
    for i in (0..=steps).rev() {
        let t = i as f32 / steps as f32;
        let step_r = radius * t;
        let step_a = ((1.0 - t) * max_alpha as f32) as u8;
        fill_circle(
            pixmap,
            cx,
            cy,
            step_r,
            to_u8(r),
            to_u8(g),
            to_u8(b),
            step_a,
        );
    }
}

fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let r = r.min(w / 2.0).min(h / 2.0);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

fn fill_rounded_rect(
    pixmap: &mut Pixmap,
    x: f32, y: f32, w: f32, h: f32, corner: f32,
    r: f32, g: f32, b: f32, a: u8,
) {
    if let Some(path) = rounded_rect_path(x, y, w, h, corner) {
        let mut paint = Paint::default();
        paint.set_color_rgba8(to_u8(r), to_u8(g), to_u8(b), a);
        paint.anti_alias = true;
        pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

fn stroke_rounded_rect(
    pixmap: &mut Pixmap,
    x: f32, y: f32, w: f32, h: f32, corner: f32,
    r: f32, g: f32, b: f32, a: u8,
) {
    if let Some(path) = rounded_rect_path(x, y, w, h, corner) {
        let mut paint = Paint::default();
        paint.set_color_rgba8(to_u8(r), to_u8(g), to_u8(b), a);
        paint.anti_alias = true;
        let stroke = Stroke { width: 1.0, ..Default::default() };
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

fn visual_cursor_center(screen_w: f32, screen_h: f32, cursor_x: f32, cursor_y: f32) -> (f32, f32) {
    let offset = design_system::cursor::OFFSET_FROM_SYSTEM_CURSOR;
    let triangle_radius = design_system::cursor::TRIANGLE_SIZE * 1.5;

    // Smooth side-switching: blend from +offset to -offset over a transition zone
    // instead of flipping instantly at the edge. This prevents the triangle from
    // jumping when the cursor reaches the screen boundary.
    let transition_zone = offset + triangle_radius;

    let right_clearance = screen_w - cursor_x;
    let t_x = ((right_clearance - triangle_radius) / transition_zone).clamp(0.0, 1.0);
    let cx = cursor_x + offset * (2.0 * t_x - 1.0);

    let bottom_clearance = screen_h - cursor_y;
    let t_y = ((bottom_clearance - triangle_radius) / transition_zone).clamp(0.0, 1.0);
    let cy = cursor_y + offset * (2.0 * t_y - 1.0);

    (cx.clamp(0.0, screen_w.max(0.0)), cy.clamp(0.0, screen_h.max(0.0)))
}
