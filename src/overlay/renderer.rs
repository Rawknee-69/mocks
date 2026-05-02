/// Overlay render state and animation logic � no Raylib, no GPU types.
///
/// `OverlayRenderState` holds all data the paint layer needs each frame.
/// `draw_overlay_frame` is now in `paint.rs`; this module is pure state.
use crate::app::state_machine::VoiceState;
use crate::core::{bezier_flight, design_system};

/// Navigation mode for the blue cursor triangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CursorNavigationMode {
    /// Cursor follows the real mouse with spring animation.
    FollowingMouse,
    /// Cursor is flying to a target element via bezier arc.
    NavigatingToTarget,
    /// Cursor has arrived at target, showing speech bubble.
    PointingAtTarget,
    /// Cursor is flying back to the mouse position.
    ReturningToMouse,
}

/// State for an active bezier flight animation.
pub struct ActiveFlightAnimation {
    pub start_x: f64,
    pub start_y: f64,
    pub control_x: f64,
    pub control_y: f64,
    pub end_x: f64,
    pub end_y: f64,
    pub duration_seconds: f64,
    pub elapsed_seconds: f64,
    pub is_return: bool,
}

impl ActiveFlightAnimation {
    pub fn new(start_x: f64, start_y: f64, end_x: f64, end_y: f64, is_return: bool) -> Self {
        let (control_x, control_y) =
            bezier_flight::compute_control_point(start_x, start_y, end_x, end_y, is_return);
        let duration_seconds = bezier_flight::compute_flight_duration_seconds(
            start_x, start_y, end_x, end_y, is_return,
        );
        Self {
            start_x,
            start_y,
            control_x,
            control_y,
            end_x,
            end_y,
            duration_seconds,
            elapsed_seconds: 0.0,
            is_return,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.elapsed_seconds >= self.duration_seconds
    }

    pub fn linear_progress(&self) -> f64 {
        (self.elapsed_seconds / self.duration_seconds).clamp(0.0, 1.0)
    }
}

/// All mutable state needed to render the overlay each frame.
pub struct OverlayRenderState {
    pub voice_state: VoiceState,
    pub navigation_mode: CursorNavigationMode,
    pub cursor_x: f32,
    pub cursor_y: f32,
    pub cursor_rotation_degrees: f64,
    pub cursor_scale: f64,
    pub active_flight: Option<ActiveFlightAnimation>,
    pub audio_power_level: f64,
    pub audio_power_history: Vec<f64>,
    pub speech_bubble_text: String,
    pub speech_bubble_visible_char_progress: f32,
    pub speech_bubble_visible_char_count: usize,
    pub speech_bubble_opacity: f32,
    /// Cached visible text string — rebuilt only when `speech_bubble_visible_char_count` changes.
    pub speech_bubble_visible_text: String,
    /// The char count at which `speech_bubble_visible_text` was last built.
    /// Set to `usize::MAX` to force a re-layout on next draw.
    pub speech_bubble_cached_char_count: usize,
    /// Persistent cosmic-text layout buffer — allocated once, reused every frame.
    /// `None` until the first draw call; recreated if `speech_bubble_buffer_width` changes.
    pub speech_bubble_buffer: Option<cosmic_text::Buffer>,
    /// The `max_text_width` the buffer was created for; used to detect when a resize is needed.
    pub speech_bubble_buffer_width: f32,
}

impl OverlayRenderState {
    pub fn new() -> Self {
        Self {
            voice_state: VoiceState::Idle,
            navigation_mode: CursorNavigationMode::FollowingMouse,
            cursor_x: 0.0,
            cursor_y: 0.0,
            cursor_rotation_degrees: design_system::cursor::DEFAULT_ROTATION_DEGREES,
            cursor_scale: 1.0,
            active_flight: None,
            audio_power_level: 0.0,
            audio_power_history: vec![0.02; 44],
            speech_bubble_text: String::new(),
            speech_bubble_visible_char_progress: 0.0,
            speech_bubble_visible_char_count: 0,
            speech_bubble_opacity: 0.0,
            speech_bubble_visible_text: String::new(),
            speech_bubble_cached_char_count: usize::MAX,
            speech_bubble_buffer: None,
            speech_bubble_buffer_width: 0.0,
        }
    }

    /// Updates animation state for the current frame.
    pub fn update(&mut self, delta_seconds: f64, current_mouse_position: Option<(f32, f32)>) {
        if self.navigation_mode == CursorNavigationMode::NavigatingToTarget {
            self.advance_flight_animation(delta_seconds);
        } else if self.navigation_mode == CursorNavigationMode::ReturningToMouse {
            if let Some((mx, my)) = current_mouse_position {
                self.advance_return_to_mouse(delta_seconds, mx, my);
            }
        }

        let is_visible = self.navigation_mode == CursorNavigationMode::PointingAtTarget
            || self.navigation_mode == CursorNavigationMode::NavigatingToTarget;

        if is_visible && !self.speech_bubble_text.is_empty() {
            self.speech_bubble_opacity =
                (self.speech_bubble_opacity + delta_seconds as f32 * 4.0).min(1.0);
            if self.speech_bubble_visible_char_count < self.speech_bubble_text.len() {
                let char_speed = 30.0f32;
                self.speech_bubble_visible_char_progress = (self
                    .speech_bubble_visible_char_progress
                    + delta_seconds as f32 * char_speed)
                    .min(self.speech_bubble_text.len() as f32);
                let new_count = self.speech_bubble_visible_char_progress.floor() as usize;
                if new_count != self.speech_bubble_visible_char_count {
                    self.speech_bubble_visible_char_count = new_count;
                    // Invalidate cached visible text so paint.rs rebuilds it.
                    self.speech_bubble_cached_char_count = usize::MAX;
                }
            }
        } else if self.navigation_mode == CursorNavigationMode::ReturningToMouse
            || self.navigation_mode == CursorNavigationMode::FollowingMouse
        {
            self.speech_bubble_opacity =
                (self.speech_bubble_opacity - delta_seconds as f32 * 5.0).max(0.0);
            if self.speech_bubble_opacity <= 0.0 {
                self.speech_bubble_visible_char_progress = 0.0;
                self.speech_bubble_visible_char_count = 0;
                self.speech_bubble_cached_char_count = usize::MAX;
            }
        }
    }

    fn advance_return_to_mouse(&mut self, delta_seconds: f64, mouse_x: f32, mouse_y: f32) {
        let dx = mouse_x - self.cursor_x;
        let dy = mouse_y - self.cursor_y;
        let distance = (dx * dx + dy * dy).sqrt();

        if distance <= 6.0 {
            self.cursor_x = mouse_x;
            self.cursor_y = mouse_y;
            self.navigation_mode = CursorNavigationMode::FollowingMouse;
            self.cursor_rotation_degrees = design_system::cursor::DEFAULT_ROTATION_DEGREES;
            self.cursor_scale = 1.0;
            self.active_flight = None;
            return;
        }

        let speed = 900.0f32;
        let step = (speed * delta_seconds as f32).min(distance);
        let ux = dx / distance.max(0.001);
        let uy = dy / distance.max(0.001);
        self.cursor_x += ux * step;
        self.cursor_y += uy * step;
        self.cursor_rotation_degrees = (uy as f64).atan2(ux as f64).to_degrees();
        let pulse = (distance / 240.0).clamp(0.0, 1.0) as f64;
        self.cursor_scale = 1.0 + pulse * 0.05;
        self.active_flight = None;
    }

    fn advance_flight_animation(&mut self, delta_seconds: f64) {
        if let Some(flight) = &mut self.active_flight {
            flight.elapsed_seconds += delta_seconds;
            let progress = flight.linear_progress();
            let is_return = flight.is_return;

            let frame = bezier_flight::compute_flight_frame(
                progress,
                flight.start_x,
                flight.start_y,
                flight.control_x,
                flight.control_y,
                flight.end_x,
                flight.end_y,
                is_return,
            );

            self.cursor_x = frame.x as f32;
            self.cursor_y = frame.y as f32;
            self.cursor_rotation_degrees = frame.rotation_radians.to_degrees();
            self.cursor_scale = frame.scale;

            if flight.is_complete() {
                if is_return {
                    self.navigation_mode = CursorNavigationMode::FollowingMouse;
                    self.speech_bubble_opacity = 0.0;
                    self.speech_bubble_visible_char_progress = 0.0;
                    self.speech_bubble_visible_char_count = 0;
                    self.speech_bubble_visible_text.clear();
                    self.speech_bubble_cached_char_count = usize::MAX;
                } else {
                    self.navigation_mode = CursorNavigationMode::PointingAtTarget;
                }
                self.cursor_rotation_degrees = design_system::cursor::DEFAULT_ROTATION_DEGREES;
                self.cursor_scale = 1.0;
                self.active_flight = None;
            }
        }
    }

    /// Starts a bezier flight from the current cursor position to a target.
    pub fn start_flight_to(&mut self, target_x: f64, target_y: f64, bubble_text: String) {
        let offset = design_system::cursor::OFFSET_FROM_SYSTEM_CURSOR as f64;
        self.navigation_mode = CursorNavigationMode::NavigatingToTarget;
        self.active_flight = Some(ActiveFlightAnimation::new(
            self.cursor_x as f64,
            self.cursor_y as f64,
            target_x - offset,
            target_y - offset,
            false,
        ));
        self.speech_bubble_text = bubble_text;
        self.speech_bubble_visible_char_progress = 0.0;
        self.speech_bubble_visible_char_count = 0;
        self.speech_bubble_opacity = 0.0;
        self.speech_bubble_visible_text.clear();
        self.speech_bubble_cached_char_count = usize::MAX;
    }

    /// Starts a bezier return flight from current position back to mouse.
    pub fn start_return_flight(&mut self, mouse_x: f32, mouse_y: f32) {
        self.navigation_mode = CursorNavigationMode::ReturningToMouse;
        self.cursor_rotation_degrees = (mouse_y as f64 - self.cursor_y as f64)
            .atan2(mouse_x as f64 - self.cursor_x as f64)
            .to_degrees();
        self.active_flight = None;
        self.speech_bubble_opacity = 0.0;
        self.speech_bubble_visible_char_progress = 0.0;
        self.speech_bubble_visible_char_count = 0;
        self.speech_bubble_visible_text.clear();
        self.speech_bubble_cached_char_count = usize::MAX;
    }

    /// Returns the cursor to following the mouse (instant, no animation).
    pub fn return_to_following_mouse(&mut self) {
        self.navigation_mode = CursorNavigationMode::FollowingMouse;
        self.speech_bubble_opacity = 0.0;
        self.speech_bubble_visible_char_progress = 0.0;
        self.speech_bubble_visible_char_count = 0;
        self.speech_bubble_visible_text.clear();
        self.speech_bubble_cached_char_count = usize::MAX;
        self.cursor_rotation_degrees = design_system::cursor::DEFAULT_ROTATION_DEGREES;
        self.cursor_scale = 1.0;
    }
}
