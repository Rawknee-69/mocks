/// winit ApplicationHandler � owns the event loop, GPU renderer, and all per-frame logic.
///
/// All state that was previously in `main()`'s `while !window_should_close()` loop
/// now lives in `OverlayApp::about_to_wait()`.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cosmic_text::FontSystem;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId, WindowLevel};

use crate::api::adapter::ApiAdapter;
use crate::app::platform::PlatformInfo;
use crate::app::state_machine::{VoiceState, VoiceStateTransition};
use crate::audio::capture::MicCapture;
use crate::audio::playback::AudioPlayer;
use crate::audio::UiEvent;
use crate::config::AppConfig;
use crate::core::audio_rms::AudioPowerLevelTracker;
use crate::core::coords::{
    self, CaptureDescriptor, PointL, PointS,
};
use crate::cursor_tracker::CursorTracker;
use crate::hotkey::PushToTalkTransition;
use crate::overlay::gpu::Renderer;
use crate::overlay::renderer::{CursorNavigationMode, OverlayRenderState};
use crate::screen_model::ScreenModel;
use crate::tray::TrayMenuEvent;
use crate::uia::UIElement;
use crate::ui_map::{snap_to_nearest_ui_element, UiMap, SNAP_RADIUS_PX};
use clicky_types::{ClientMessage, DisplayInfo, ElementIntent, ScreenshotData};

/// All app state that the winit event loop drives.
pub struct OverlayApp {
    pub platform: PlatformInfo,
    pub app_config: AppConfig,
    pub api_adapter: ApiAdapter,
    pub font_system: FontSystem,
    pub rms_tracker: Arc<Mutex<AudioPowerLevelTracker>>,
    pub mic_capture: MicCapture,
    pub audio_player: Option<AudioPlayer>,
    pub tray_icon: Option<crate::tray::ClickyTrayIcon>,
    pub hotkey_manager: Option<Box<dyn crate::hotkey::HotkeyBackend>>,
    pub cursor_tracker: Box<dyn CursorTracker>,
    pub tokio_rt: Arc<tokio::runtime::Runtime>,
    pub render_state: OverlayRenderState,
    pub voice_state: VoiceState,
    pub overlay_x: i32,
    pub overlay_y: i32,
    pub screen_w: i32,
    pub screen_h: i32,
    // Background task handles — aborted on re-press to prevent stale tasks.
    pub audio_forward_task: Option<tokio::task::JoinHandle<()>>,
    pub screenshot_task: Option<tokio::task::JoinHandle<()>>,
    // Per-frame mutable fields
    pub frame_count: u64,
    pub last_transcript: Option<String>,
    pub processing_since: Option<Instant>,
    pub claude_pipeline_active: bool,
    pub responding_since: Option<Instant>,
    pub pointing_hold_since: Option<Instant>,
    pub last_config_poll_at: Instant,
    pub last_config_modified_at: Option<std::time::SystemTime>,
    pub cu_coordinate_pending: bool,
    pub cu_pending_since: Option<Instant>,
    pub simulation_mode: bool,
    pub frame_start: Instant,
    pub elapsed_secs: f64,
    pub refresh_hz: u32,
    pub last_mouse_pos: (f32, f32),
    // Last cursor position actually sent to the backend — used for delta filtering.
    pub last_sent_cursor_pos: (f32, f32),
    // Mouse position at the moment the hotkey was released — used to drift-compensate
    // CU coordinates, which are anchored to the screenshot taken at release time.
    pub mouse_pos_at_release: (f32, f32),
    // Exponentially smoothed mouse velocity (pixels/second) used for prediction.
    pub mouse_velocity: (f32, f32),
    // Screenshots kept locally for UI-map matching and CU fallback.
    pub last_screenshots: Option<Vec<ScreenshotData>>,
    pub last_display_infos: Option<Vec<DisplayInfo>>,
    // UI map built from the most recent screenshots.
    pub last_ui_map: Option<UiMap>,
    // UIA elements extracted from the desktop (Windows only).
    pub last_uia_elements: Vec<UIElement>,
    // Merged screen model (UIA + vision).
    pub last_screen_model: Option<ScreenModel>,
    // Pending intent waiting for a CUCoordinateReady reply.
    pub pending_intent: Option<ElementIntent>,
    // Channel used by the screenshot task to deliver a local copy to the render loop.
    pub screenshot_local_tx: std::sync::mpsc::SyncSender<(Vec<ScreenshotData>, Vec<DisplayInfo>)>,
    pub screenshot_local_rx: std::sync::mpsc::Receiver<(Vec<ScreenshotData>, Vec<DisplayInfo>)>,
    // Per-second timing diagnostics
    pub diag_last_log_at: Instant,
    pub diag_frame_intervals_ms: Vec<f32>,
    pub diag_flush_ms: Vec<f32>,
    pub diag_render_ms: Vec<f32>,
    pub diag_cursor_delta_px: Vec<f32>,
    // Per-phase render breakdown (Windows only; always 0 on other platforms)
    pub diag_paint_ms: Vec<f32>,
    pub diag_copy_ms: Vec<f32>,
    pub diag_ulw_ms: Vec<f32>,
    // Used on non-Windows platforms for frame pacing; dead on Windows where DwmFlush handles it.
    #[allow(dead_code)]
    pub frame_budget: std::time::Duration,
    // winit / GPU � None until `resumed` creates the window
    pub window: Option<Arc<Window>>,
    pub renderer: Option<Renderer>,
    pub is_occluded: bool,
}

impl ApplicationHandler for OverlayApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return; // already created
        }

        let mut attrs = Window::default_attributes()
            .with_title("clicky-overlay")
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_position(winit::dpi::PhysicalPosition::new(
                self.overlay_x,
                self.overlay_y,
            ))
            .with_inner_size(winit::dpi::PhysicalSize::new(
                self.screen_w as u32,
                self.screen_h as u32,
            ));

        // Hide from taskbar on supported platforms
        #[cfg(target_os = "windows")]
        {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs = attrs.with_skip_taskbar(true);
        }

        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("Failed to create overlay window"),
        );

        // Platform-specific overlay setup
        #[cfg(target_os = "windows")]
        {
            use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let hwnd = match window.as_ref().window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(h)) => isize::from(h.hwnd),
                _ => 0isize,
            };
            if hwnd != 0 {
                crate::platform::windows::setup_windows_overlay(hwnd);
            }
        }

        #[cfg(target_os = "linux")]
        {
            use crate::app::platform::DisplayServer;
            match self.platform.display_server {
                Some(DisplayServer::X11) => {
                    use winit::platform::x11::WindowExtX11;
                    let xid = window.xid() as u32;
                    if let Err(e) = crate::platform::linux_x11::setup_x11_overlay(xid) {
                        log::warn!("X11 overlay setup failed: {}", e);
                    }
                }
                Some(DisplayServer::Wayland) => {
                    crate::platform::linux_wayland::setup_wayland_overlay();
                }
                None => {}
            }
        }

        let renderer = Renderer::new(Arc::clone(&window));
        let (actual_w, actual_h) = renderer.window_size();
        log::info!(
            "Overlay window created: {}x{} (requested {}x{})",
            actual_w, actual_h, self.screen_w, self.screen_h
        );

        self.window = Some(window);
        self.renderer = Some(renderer);
        self.frame_start = Instant::now();

        // Request continuous redraws so about_to_wait fires every frame
        event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size);
                    log::info!("Overlay resized to {}x{}", size.width, size.height);
                }
            }
            WindowEvent::Occluded(occluded) => {
                self.is_occluded = occluded;
            }
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.renderer.is_none() {
            return;
        }

        // ---- Phase 1: sync to display vsync ----
        // DwmFlush blocks until DWM starts compositing the next frame.
        // Calling it HERE (not at the end) means GetCursorPos runs in the first
        // microseconds after vsync fires -- the minimum possible sample-to-display lag.
        let t_vsync = Instant::now();
        crate::platform::sync_to_display();
        let flush_ms = t_vsync.elapsed().as_secs_f32() * 1000.0;

        self.frame_count += 1;
        let now = Instant::now();
        let delta_seconds = now.duration_since(self.frame_start).as_secs_f64().min(0.1);
        self.frame_start = now;
        self.elapsed_secs += delta_seconds;

        // Config hot-reload
        if now.duration_since(self.last_config_poll_at) >= Duration::from_millis(750) {
            self.last_config_poll_at = now;
            let current_modified = crate::config::config_file_modified_at();
            if current_modified != self.last_config_modified_at {
                self.last_config_modified_at = current_modified;
                let reloaded = AppConfig::load();
                let hotkey_changed =
                    reloaded.push_to_talk_hotkey != self.app_config.push_to_talk_hotkey;
                self.app_config = reloaded;
                log::info!("Reloaded settings from config file");
                if hotkey_changed {
                    self.hotkey_manager =
                        crate::hotkey::create(&self.platform, self.app_config.push_to_talk_hotkey);
                    log::info!(
                        "Applied new push-to-talk hotkey: {}",
                        self.app_config.push_to_talk_hotkey.display_name()
                    );
                }
            }
        }

        // Poll tray menu
        if let Some(tray) = &self.tray_icon {
            if let Some(ev) = tray.poll_menu_event() {
                match ev {
                    TrayMenuEvent::Quit => {
                        log::info!("Quit requested from tray menu");
                        event_loop.exit();
                        return;
                    }
                    TrayMenuEvent::ToggleOverlay => {}
                    TrayMenuEvent::OpenSettings => {
                        let cfg = AppConfig::load();
                        crate::panel::open_settings_window(cfg);
                    }
                }
            }
        }

        // Poll hotkeys
        if let Some(hk) = &self.hotkey_manager {
            if let Some(transition) = hk.poll_hotkey_event() {
                self.handle_hotkey(transition);
            }
        }

        // Update waveform while listening — update in-place to avoid a per-frame heap alloc.
        if self.voice_state == VoiceState::Listening {
            if let Ok(tracker) = self.rms_tracker.lock() {
                let history = tracker.history();
                self.render_state.audio_power_history.resize(history.len(), 0.0);
                self.render_state.audio_power_history.copy_from_slice(history);
                self.render_state.audio_power_level = tracker.current_level();
            }
        }

        // Cursor position � winit fallback tracker gets updated via CursorMoved events.
        // The win32 tracker reads GetCursorPos directly, so update_from_window is a no-op.
        let current_mouse = self.cursor_tracker.get_position();
        // Send cursor position to backend — only when the cursor has moved > 1 pixel to avoid
        // flooding the channel at 60fps with identical values.
        if self.frame_count % 6 == 0 {
            let dx = current_mouse.0 - self.last_sent_cursor_pos.0;
            let dy = current_mouse.1 - self.last_sent_cursor_pos.1;
            if dx * dx + dy * dy > 1.0 {
                self.api_adapter.send(ClientMessage::CursorPosition {
                    x: current_mouse.0,
                    y: current_mouse.1,
                });
                self.last_sent_cursor_pos = current_mouse;
            }
        }

        // Drain backend events
        while let Some(event) = self.api_adapter.try_recv() {
            self.handle_ui_event(event);
        }

        // Drain locally captured screenshots — build UI map on arrival.
        while let Ok((screenshots, display_infos)) = self.screenshot_local_rx.try_recv() {
            // Log display metrics so DPI/scaling issues are immediately visible.
            for (i, d) in display_infos.iter().enumerate() {
                let scale_x = if d.display_width_points > 0.0 {
                    d.screenshot_width_pixels as f64 / d.display_width_points
                } else {
                    0.0
                };
                let scale_y = if d.display_height_points > 0.0 {
                    d.screenshot_height_pixels as f64 / d.display_height_points
                } else {
                    0.0
                };
                log::info!(
                    "[DISPLAY] display[{i}]: origin=({:.0},{:.0}) logical={:.0}x{:.0} \
                     ss_px={}x{} scale_x={:.3} scale_y={:.3} cursor={}",
                    d.global_origin_x, d.global_origin_y,
                    d.display_width_points, d.display_height_points,
                    d.screenshot_width_pixels, d.screenshot_height_pixels,
                    scale_x, scale_y,
                    d.is_cursor_display,
                );
            }
            // Build the UI map from the cursor display's screenshot.
            let cursor_ss = screenshots
                .iter()
                .zip(display_infos.iter())
                .find(|(_, d)| d.is_cursor_display)
                .map(|(s, _)| s);
            if let Some(ss) = cursor_ss {
                match crate::ui_map::build_ui_map(&ss.jpeg_data) {
                    Ok(map) => {
                        log::info!(
                            "UI map built: {} elements from {}×{} screenshot",
                            map.elements.len(),
                            map.screenshot_width,
                            map.screenshot_height
                        );
                        crate::pipeline_log::log_vision(
                            map.elements.len(),
                            &map.elements.iter().take(10).map(|el| {
                                (el.center.x, el.center.y, el.width, el.height)
                            }).collect::<Vec<_>>(),
                        );
                        self.last_ui_map = Some(map);
                        self.rebuild_screen_model();
                    }
                    Err(e) => log::warn!("UI map build failed: {e}"),
                }
            }
            self.last_screenshots = Some(screenshots);
            self.last_display_infos = Some(display_infos.clone());
            for (i, d) in display_infos.iter().enumerate() {
                crate::pipeline_log::log_screen(
                    i,
                    d.display_width_points,
                    d.display_height_points,
                    d.global_origin_x,
                    d.global_origin_y,
                    if d.display_width_points > 0.0 {
                        d.screenshot_width_pixels as f64 / d.display_width_points
                    } else {
                        1.0
                    },
                );
            }
        }

        // Processing timeout — always 30s regardless of pipeline state.
        // The previous 3s short-circuit was firing before screenshots arrived, causing
        // premature Idle resets. Pipeline flag is cleared only on PipelineReady/Error,
        // so 30s is the correct upper bound for the full LLM round-trip.
        if self.voice_state == VoiceState::Processing {
            let since = self.processing_since.get_or_insert(now);
            if now.duration_since(*since).as_secs() >= 30 {
                log::warn!(
                    "[TIMEOUT] Processing >30s (pipeline_active={}) — forcing Idle",
                    self.claude_pipeline_active
                );
                self.voice_state = VoiceState::Idle;
                self.render_state.voice_state = self.voice_state;
                self.processing_since = None;
                self.claude_pipeline_active = false;
            }
        }

        // Responding → hold → return flight → Idle
        if self.voice_state == VoiceState::Responding {
            let since = self.responding_since.get_or_insert(now);
            let elapsed_total = now.duration_since(*since).as_secs();
            let tts_done = self
                .audio_player
                .as_ref()
                .map_or(true, |p| !p.is_playing());

            // CU-specific safety timeout: 12s — enough for two-pass CU at 45s/request,
            // but short enough that a dropped WS response doesn't freeze the UI for 30s.
            if self.cu_coordinate_pending {
                let cu_elapsed = self
                    .cu_pending_since
                    .map_or(0, |t| now.duration_since(t).as_secs());
                if cu_elapsed >= 12 {
                    log::warn!("[CU] safety timeout (12s) — clearing cu_coordinate_pending");
                    self.cu_coordinate_pending = false;
                    self.cu_pending_since = None;
                }
            }

            match self.render_state.navigation_mode {
                CursorNavigationMode::NavigatingToTarget => {}
                CursorNavigationMode::PointingAtTarget => {
                    if tts_done {
                        let hold_since = self.pointing_hold_since.get_or_insert(now);
                        if now.duration_since(*hold_since).as_secs_f64() >= 2.0 {
                            let mp = self.cursor_tracker.get_position();
                            self.render_state.start_return_flight(mp.0, mp.1);
                            self.pointing_hold_since = None;
                        }
                    }
                }
                CursorNavigationMode::ReturningToMouse => {}
                CursorNavigationMode::FollowingMouse => {
                    // Only transition to Idle when CU has resolved (or timed out) AND TTS is done.
                    if !self.cu_coordinate_pending && tts_done {
                        if let Some(s) =
                            self.voice_state.apply(VoiceStateTransition::ResponseComplete)
                        {
                            self.voice_state = s;
                            self.render_state.voice_state = self.voice_state;
                        }
                        self.responding_since = None;
                        self.pointing_hold_since = None;
                    }
                }
            }

            if elapsed_total >= 45 {
                log::warn!("Responding safety timeout — returning to Idle");
                if let Some(s) =
                    self.voice_state.apply(VoiceStateTransition::ResponseComplete)
                {
                    self.voice_state = s;
                    self.render_state.voice_state = self.voice_state;
                    self.render_state.return_to_following_mouse();
                }
                self.responding_since = None;
                self.pointing_hold_since = None;
                self.cu_coordinate_pending = false;
                self.cu_pending_since = None;
            }
        }

        // --- Cursor sample + velocity tracking + prediction ---
        //
        // Sample GetCursorPos as late as possible (right before draw) so the triangle
        // position is as fresh as the hardware can supply.
        //
        // Velocity is tracked with a 1-pole low-pass filter (alpha=0.2) so short
        // acceleration spikes don't overshoot. We then predict one frame ahead:
        //
        //   predicted_pos = current_pos + velocity * frame_time
        //
        // This shifts the triangle slightly forward in the direction of travel,
        // compensating for the ~1 frame pipeline delay between GetCursorPos and the
        // pixels reaching the display. At 240fps that is only ~4ms, but it removes
        // the last perceptible "chasing" artefact.
        let fresh = self.cursor_tracker.get_position();

        // Update smoothed velocity (px/s) with a low-pass filter.
        // Alpha 0.15 keeps the estimate stable enough that direction changes don't
        // produce visible overshoot. We only predict half a frame ahead: the first
        // 0.5 frame compensates for pipeline delay; beyond that prediction noise
        // becomes perceptible on tight curves.
        let dt = delta_seconds as f32;
        let raw_vel_x = if dt > 0.0 { (fresh.0 - self.last_mouse_pos.0) / dt } else { 0.0 };
        let raw_vel_y = if dt > 0.0 { (fresh.1 - self.last_mouse_pos.1) / dt } else { 0.0 };
        const VELOCITY_ALPHA: f32 = 0.15;
        const PREDICTION_FRAMES: f32 = 0.5;
        self.mouse_velocity.0 = self.mouse_velocity.0 + VELOCITY_ALPHA * (raw_vel_x - self.mouse_velocity.0);
        self.mouse_velocity.1 = self.mouse_velocity.1 + VELOCITY_ALPHA * (raw_vel_y - self.mouse_velocity.1);

        // Predict half a frame ahead and convert to window-local coords.
        let predict_x = fresh.0 + self.mouse_velocity.0 * dt * PREDICTION_FRAMES;
        let predict_y = fresh.1 + self.mouse_velocity.1 * dt * PREDICTION_FRAMES;
        let fresh_local = (
            fresh.0 - self.overlay_x as f32,
            fresh.1 - self.overlay_y as f32,
        );
        let predicted_local = (
            predict_x - self.overlay_x as f32,
            predict_y - self.overlay_y as f32,
        );

        // Capture delta BEFORE updating last_mouse_pos so the diag log is correct.
        let cursor_delta_px = {
            let dx = fresh.0 - self.last_mouse_pos.0;
            let dy = fresh.1 - self.last_mouse_pos.1;
            (dx * dx + dy * dy).sqrt()
        };
        let cursor_moved = cursor_delta_px > 0.5;
        self.last_mouse_pos = fresh;

        // In FollowingMouse mode, draw at the predicted position.
        // In all other modes, state machine owns cursor_x/cursor_y.
        // Also update during Listening so the waveform stays anchored to the real cursor.
        if self.render_state.navigation_mode == CursorNavigationMode::FollowingMouse
            || self.render_state.voice_state == VoiceState::Listening
            || self.render_state.voice_state == VoiceState::Processing
        {
            self.render_state.cursor_x = predicted_local.0;
            self.render_state.cursor_y = predicted_local.1;
        }

        self.render_state.update(delta_seconds, Some(fresh_local));

        // Determine whether anything visible changed this frame.
        // Listening state always repaints so waveform bars animate every frame.
        let is_animating = self.render_state.navigation_mode != CursorNavigationMode::FollowingMouse
            || self.render_state.speech_bubble_opacity > 0.0
            || self.render_state.voice_state == VoiceState::Listening
            || self.render_state.voice_state == VoiceState::Processing;
        let needs_repaint = cursor_moved || is_animating;

        // ---- Phase 3: render ----
        let t_render_start = Instant::now();
        #[allow(unused_mut)]
        let mut render_paint_ms = 0.0f32;
        #[allow(unused_mut)]
        let mut render_copy_ms  = 0.0f32;
        #[allow(unused_mut)]
        let mut render_ulw_ms   = 0.0f32;

        if !self.is_occluded && needs_repaint {
            if let Some(renderer) = &mut self.renderer {
                #[cfg(target_os = "windows")]
                {
                    use crate::overlay::windows_impl::RenderTimings;
                    let t: RenderTimings = renderer.render_frame(
                        &mut self.render_state,
                        &mut self.font_system,
                        self.elapsed_secs,
                    );
                    render_paint_ms = t.paint_ms;
                    render_copy_ms  = t.copy_ms;
                    render_ulw_ms   = t.present_ms;
                }
                #[cfg(not(target_os = "windows"))]
                renderer.render_frame(
                    &mut self.render_state,
                    &mut self.font_system,
                    self.elapsed_secs,
                );
            }
        }
        let render_ms = t_render_start.elapsed().as_secs_f32() * 1000.0;

        // ---- Accumulate diagnostics ----
        let frame_interval_ms = delta_seconds as f32 * 1000.0;
        self.diag_frame_intervals_ms.push(frame_interval_ms);
        self.diag_flush_ms.push(flush_ms);
        self.diag_render_ms.push(render_ms);
        self.diag_cursor_delta_px.push(cursor_delta_px);
        self.diag_paint_ms.push(render_paint_ms);
        self.diag_copy_ms.push(render_copy_ms);
        self.diag_ulw_ms.push(render_ulw_ms);

        // ---- Emit diagnostic log once per second ----
        if now.duration_since(self.diag_last_log_at).as_secs_f32() >= 1.0 {
            self.diag_last_log_at = now;
            let count = self.diag_frame_intervals_ms.len() as f32;
            if count > 0.0 {
                let avg = |v: &Vec<f32>| v.iter().copied().sum::<f32>() / v.len() as f32;
                let max = |v: &Vec<f32>| v.iter().copied().fold(0.0f32, f32::max);
                let min = |v: &Vec<f32>| v.iter().copied().fold(f32::MAX, f32::min);

                let fi  = &self.diag_frame_intervals_ms;
                let fl  = &self.diag_flush_ms;
                let rn  = &self.diag_render_ms;
                let cd  = &self.diag_cursor_delta_px;

                let target_ms = 1000.0 / self.refresh_hz as f32;
                let missed_frames = self.diag_frame_intervals_ms.iter()
                    .filter(|&&t| t > target_ms * 1.5)
                    .count();
                let ap = &self.diag_paint_ms;
                let ac = &self.diag_copy_ms;
                let au = &self.diag_ulw_ms;
                log::info!(
                    "[TIMING {}/s tgt={:.1}ms] \
                     interval={:.1}/{:.1}/{:.1}ms (missed={}) \
                     flush={:.1}/{:.1}ms \
                     paint={:.2}/{:.2}ms copy={:.2}/{:.2}ms ulw={:.2}/{:.2}ms \
                     render_total={:.1}/{:.1}ms \
                     cursor={:.1}px/f",
                    count as u32,
                    target_ms,
                    min(fi), avg(fi), max(fi), missed_frames,
                    min(fl), avg(fl),
                    min(ap), avg(ap),
                    min(ac), avg(ac),
                    min(au), avg(au),
                    min(rn), avg(rn),
                    avg(cd),
                );
            }
            self.diag_frame_intervals_ms.clear();
            self.diag_flush_ms.clear();
            self.diag_render_ms.clear();
            self.diag_cursor_delta_px.clear();
            self.diag_paint_ms.clear();
            self.diag_copy_ms.clear();
            self.diag_ulw_ms.clear();
        }

        // ---- Frame pacing ----
        // On Windows: DwmFlush() at the top of the next iteration provides vsync pacing
        // for both active and idle frames. NO extra sleep -- adding one here would
        // double the wait time (DwmFlush + sleep = 2 vsyncs = half the refresh rate).
        //
        // On non-Windows: no DwmFlush, so use frame budget timing.
        #[cfg(not(target_os = "windows"))]
        {
            let frame_elapsed = now.elapsed();
            if frame_elapsed < self.frame_budget {
                spin_sleep::sleep(self.frame_budget - frame_elapsed);
            }
        }
    }
}

impl OverlayApp {
    fn handle_hotkey(&mut self, transition: PushToTalkTransition) {
        match transition {
            PushToTalkTransition::Pressed => {
                log::info!("[HOTKEY] PRESSED — current state={:?}", self.voice_state);
                if let Some(new_state) =
                    self.voice_state.apply(VoiceStateTransition::HotkeyPressed)
                {
                    self.voice_state = new_state;
                    self.render_state.voice_state = self.voice_state;
                    self.claude_pipeline_active = false;
                    self.responding_since = None;
                    self.pointing_hold_since = None;
                    self.cu_coordinate_pending = false;
                    self.cu_pending_since = None;

                    if let Some(p) = &mut self.audio_player {
                        p.stop();
                    }
                    if let Ok(mut t) = self.rms_tracker.lock() {
                        t.reset();
                    }

                    match self.mic_capture.start() {
                        Ok(audio_rx) => {
                            log::info!("[HOTKEY] mic capture started — forwarding audio to backend");
                            self.api_adapter.send(ClientMessage::HotkeyPressed);
                            // Abort any still-running audio-forward task from a previous press.
                            if let Some(handle) = self.audio_forward_task.take() {
                                handle.abort();
                            }
                            let adapter_tx = self.api_adapter.client_tx_clone();
                            let handle = self.tokio_rt.spawn_blocking(move || {
                                while let Ok(chunk) = audio_rx.recv() {
                                    let pcm16 = crate::core::pcm16_converter::convert_float32_to_pcm16_mono(
                                        &chunk.samples,
                                        chunk.sample_rate,
                                        chunk.channels,
                                    );
                                    if adapter_tx
                                        .try_send(ClientMessage::AudioChunk { pcm16_bytes: pcm16 })
                                        .is_err()
                                    {
                                        // Channel full or closed — drop this chunk and stop.
                                        break;
                                    }
                                }
                            });
                            self.audio_forward_task = Some(handle);
                        }
                        Err(e) => {
                            log::error!("[HOTKEY] mic capture failed: {}", e);
                            if let Some(s) = self.voice_state.apply(VoiceStateTransition::Error) {
                                self.voice_state = s;
                                self.render_state.voice_state = self.voice_state;
                            }
                        }
                    }
                } else {
                    log::warn!("[HOTKEY] PRESSED ignored — state {:?} doesn't accept HotkeyPressed", self.voice_state);
                }
            }

            PushToTalkTransition::Released => {
                log::info!("[HOTKEY] RELEASED — current state={:?}", self.voice_state);
                self.mic_capture.stop();
                // Snapshot mouse position so PipelineReady can drift-compensate.
                self.mouse_pos_at_release = self.cursor_tracker.get_position();
                log::info!("[HOTKEY] mouse pos at release: ({:.0},{:.0})", self.mouse_pos_at_release.0, self.mouse_pos_at_release.1);

                if self.simulation_mode {
                    if let Some(s) =
                        self.voice_state.apply(VoiceStateTransition::HotkeyReleased)
                    {
                        self.voice_state = s;
                        self.render_state.voice_state = self.voice_state;
                    }
                    self.processing_since = Some(Instant::now());
                } else {
                    self.api_adapter.send(ClientMessage::HotkeyReleased);

                    let cursor_pos = self.cursor_tracker.get_position();
                    let platform_clone = self.platform.clone();
                    let adapter_tx = self.api_adapter.client_tx_clone();
                    // Abort any previous screenshot task still holding screenshot data.
                    if let Some(handle) = self.screenshot_task.take() {
                        handle.abort();
                    }

                    // Clear stale UI map so we don't snap to elements from a previous scene.
                    self.last_ui_map = None;
                    self.last_screenshots = None;
                    self.last_display_infos = None;
                    self.pending_intent = None;
                    self.last_uia_elements = Vec::new();
                    self.last_screen_model = None;

                    // Fire UIA tree extraction in background (Windows only).
                    #[cfg(target_os = "windows")]
                    {
                        let uia_event_tx = self.api_adapter.ui_event_tx_clone();
                        self.tokio_rt.spawn_blocking(move || {
                            let start = std::time::Instant::now();
                            let elements = crate::uia::extract_ui_tree();
                            log::info!("[UIA] extraction took {:.0}ms — {} elements", start.elapsed().as_millis(), elements.len());
                            let _ = uia_event_tx.send(UiEvent::UIATreeReady(elements));
                        });
                    }

                    let local_tx = self.screenshot_local_tx.clone();
                    let handle = self.tokio_rt.spawn_blocking(move || {
                        log::info!("[SCREENSHOT] capturing all screens at cursor ({:.0},{:.0})", cursor_pos.0, cursor_pos.1);
                        match crate::screenshot::capture_all_screens(
                            cursor_pos.0,
                            cursor_pos.1,
                            &platform_clone,
                        ) {
                            Ok(capture) => {
                                log::info!(
                                    "[SCREENSHOT] captured {} screens — sending to backend and local",
                                    capture.screenshots.len()
                                );
                                // Send to backend
                                let _ = adapter_tx.try_send(ClientMessage::Screenshots {
                                    screenshots: capture.screenshots.clone(),
                                    display_infos: capture.display_infos.clone(),
                                });
                                // Also deliver locally for UI-map matching
                                let _ = local_tx.try_send((capture.screenshots, capture.display_infos));
                            }
                            Err(e) => {
                                log::error!("[SCREENSHOT] capture failed: {}", e);
                            }
                        }
                    });
                    self.screenshot_task = Some(handle);

                    if let Some(s) =
                        self.voice_state.apply(VoiceStateTransition::HotkeyReleased)
                    {
                        self.voice_state = s;
                        self.render_state.voice_state = self.voice_state;
                    }
                }
            }
        }
    }

    fn handle_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::PartialTranscript(text) => {
                log::debug!("Partial transcript: {}", text);
                self.last_transcript = Some(text);
                self.processing_since = None;
            }
            UiEvent::FinalTranscript(text) => {
                let transcript = if text.is_empty() {
                    self.last_transcript.clone().unwrap_or_default()
                } else {
                    text
                };
                if transcript.is_empty() {
                    self.voice_state = VoiceState::Idle;
                    self.render_state.voice_state = self.voice_state;
                    self.processing_since = None;
                } else {
                    log::debug!("FINAL transcript: {}", transcript);
                    self.last_transcript = Some(transcript);
                    self.processing_since = Some(Instant::now());
                    self.claude_pipeline_active = true;
                }
            }
            UiEvent::TranscriptionError(err) => {
                log::error!("Transcription error: {}", err);
                self.voice_state = VoiceState::Idle;
                self.render_state.voice_state = self.voice_state;
                self.processing_since = None;
            }
            UiEvent::PipelineReady { spoken_text, mp3_bytes, coordinate, display_infos } => {
                // Legacy path — no intent, treat like PipelineReadyWithIntent with intent: None
                if self.voice_state != VoiceState::Processing {
                    return;
                }
                log::info!("PipelineReady (legacy): {}", spoken_text);

                if let Some(s) = self.voice_state.apply(VoiceStateTransition::ResponseReady) {
                    self.voice_state = s;
                    self.render_state.voice_state = self.voice_state;
                }

                let current = self.cursor_tracker.get_position();
                let drift_x = (current.0 - self.mouse_pos_at_release.0) as f64;
                let drift_y = (current.1 - self.mouse_pos_at_release.1) as f64;

                let adjusted_coord = coordinate.and_then(|(cx, cy)| {
                    let ax = cx + drift_x;
                    let ay = cy + drift_y;
                    if ax.is_finite() && ay.is_finite() && ax >= 0.0 && ay >= 0.0
                        && ax < self.screen_w as f64 && ay < self.screen_h as f64
                    {
                        Some(PointL { x: ax, y: ay })
                    } else {
                        None
                    }
                });

                if let Some(coord) = adjusted_coord {
                    self.render_state.start_flight_to(
                        coord.x - self.overlay_x as f64,
                        coord.y - self.overlay_y as f64,
                        spoken_text.clone(),
                    );
                }

                let is_pointing = adjusted_coord.is_some();
                self.render_state.speech_bubble_text =
                    crate::core::bubble_text::pick_bubble_phrase(is_pointing);

                self.play_tts(mp3_bytes, &spoken_text);
                self.responding_since = Some(Instant::now());
                self.cu_coordinate_pending = false;
                self.cu_pending_since = None;
                self.claude_pipeline_active = false;
                self.processing_since = None;
                let _ = display_infos;
            }
            UiEvent::PipelineReadyWithIntent { spoken_text, mp3_bytes, intent, display_infos } => {
                if self.voice_state != VoiceState::Processing {
                    log::warn!(
                        "[PIPELINE] PipelineReadyWithIntent arrived but state={:?} — discarding",
                        self.voice_state
                    );
                    return;
                }
                log::info!(
                    "[PIPELINE] PipelineReadyWithIntent: spoken='{}' intent={:?}",
                    spoken_text,
                    intent.as_ref().map(|i| (&i.label, &i.region_hint))
                );

                if let Some(s) = self.voice_state.apply(VoiceStateTransition::ResponseReady) {
                    self.voice_state = s;
                    self.render_state.voice_state = self.voice_state;
                }

                self.play_tts(mp3_bytes, &spoken_text);
                self.responding_since = Some(Instant::now());
                self.claude_pipeline_active = false;
                self.processing_since = None;

                self.render_state.speech_bubble_text =
                    crate::core::bubble_text::pick_bubble_phrase(intent.is_some());

                if let Some(intent) = intent {
                    crate::pipeline_log::log_model_output(
                        Some(&intent.label),
                        intent.region_hint.as_deref(),
                        None,
                    );

                    // Determine which display (screenshot index) to use for CU fallback.
                    let screenshot_index = display_infos
                        .iter()
                        .position(|d| d.is_cursor_display)
                        .unwrap_or(0);

                    let cursor_ss_index = screenshot_index;

                    // ── UIA-first decision logic ──
                    // Rebuild ScreenModel with latest data before trying UIA match.
                    self.rebuild_screen_model();
                    let uia_had_elements = self.last_uia_elements.len() > 0;
                    if let Some(ref screen_model) = self.last_screen_model {
                        if let Some((uia_coord, match_desc)) = screen_model.try_uia_match(
                            &intent.label,
                            &intent.action,
                            intent.region_hint.as_deref(),
                            self.screen_w as f64,
                            self.screen_h as f64,
                        ) {
                            log::info!("[UIA-MATCH] {} → ({:.0},{:.0})", match_desc, uia_coord.x, uia_coord.y);
                            crate::pipeline_log::log_decision("UIA", &intent.label, &match_desc);

                            // Use enhanced snap (UIA priority).
                            let snapped = crate::screen_model::enhanced_snap(
                                uia_coord,
                                screen_model,
                                100.0,
                                SNAP_RADIUS_PX,
                                self.last_ui_map.as_ref(),
                                screen_model.capture.as_ref(),
                            ).unwrap_or(uia_coord);
                            crate::pipeline_log::log_snap("UIA", uia_coord, Some(snapped), 100.0);

                            // UIA coordinates come from the OS accessibility tree — they are
                            // authoritative. Only reject if outside display bounds or non-finite.
                            if self.is_valid_bounds_only(snapped, &display_infos) {
                                crate::pipeline_log::log_validate(snapped, true, "UIA path (trusted)");
                                log::info!(
                                    "[FINAL PIPELINE] UIA → logical=({:.0},{:.0}) label='{}'",
                                    snapped.x, snapped.y, intent.label,
                                );
                                self.start_flight_logical(snapped, &spoken_text);
                                self.cu_coordinate_pending = false;
                                self.cu_pending_since = None;
                                return;
                            } else {
                                crate::pipeline_log::log_validate(snapped, false, "UIA path outside display bounds");
                                log::warn!("[UIA-MATCH] coordinate ({:.0},{:.0}) outside bounds — falling through to vision", snapped.x, snapped.y);
                            }
                        } else {
                            crate::pipeline_log::log_decision("UIA", &intent.label, &format!(
                                "no match (uia_count={}, all below 0.75 threshold)", self.last_uia_elements.len()
                            ));
                        }
                    }

                    // ── Vision fallback ──
                    // Only use vision if UIA had no elements OR no UIA match met the
                    // strict 0.75 threshold. If UIA elements exist but couldn't match,
                    // skip vision (it picks random blobs) and go straight to CU.
                    let skip_vision = uia_had_elements;
                    if skip_vision {
                        log::info!(
                            "[VISION-SKIP] UIA had {} elements but no match — skipping vision (unreliable), going to CU",
                            self.last_uia_elements.len(),
                        );
                        crate::pipeline_log::log_decision("VISION", &intent.label, "skipped (UIA elements present but no match)");
                    } else if let Some(ui_map) = &self.last_ui_map {
                        let cursor_pos_ss = self.screenshot_space_cursor(cursor_ss_index, &display_infos);
                        let match_result = crate::ui_map::match_intent(ui_map, &intent, cursor_pos_ss);

                        if let Some(m) = match_result {
                            log::info!(
                                "UI-map match: '{}' score={:.2} at ss=({:.0},{:.0})",
                                intent.label, m.score, m.element.center.x, m.element.center.y
                            );
                            crate::pipeline_log::log_decision("VISION", &intent.label, &format!("score={:.2}", m.score));
                            if let Some(global_coord) = self.map_element_to_logical(&m.element, cursor_ss_index, &display_infos) {
                                // Use enhanced snap if ScreenModel is available.
                                let snapped = if let Some(ref sm) = self.last_screen_model {
                                    crate::screen_model::enhanced_snap(
                                        global_coord,
                                        sm,
                                        100.0,
                                        SNAP_RADIUS_PX,
                                        self.last_ui_map.as_ref(),
                                        sm.capture.as_ref(),
                                    ).unwrap_or(global_coord)
                                } else {
                                    global_coord
                                };
                                crate::pipeline_log::log_snap("VISION", global_coord, Some(snapped), SNAP_RADIUS_PX);

                                if self.is_valid_logical_coordinate(snapped, &display_infos) {
                                    crate::pipeline_log::log_validate(snapped, true, "vision path");
                                    log::info!(
                                        "[FINAL PIPELINE] vision ss=({:.0},{:.0}) → logical=({:.0},{:.0}) label='{}'",
                                        m.element.center.x, m.element.center.y,
                                        snapped.x, snapped.y, intent.label,
                                    );
                                    self.start_flight_logical(snapped, &spoken_text);
                                    self.cu_coordinate_pending = false;
                                    self.cu_pending_since = None;
                                    return;
                                } else {
                                    crate::pipeline_log::log_validate(snapped, false, "vision path failed validation");
                                }
                            }
                        } else {
                            crate::pipeline_log::log_decision("VISION", &intent.label, "below threshold");
                        }
                    }

                    // ── CU fallback ──
                    log::info!(
                        "[CU-FALLBACK] UI-map miss for '{}' — firing RequestCUCoordinate (display_index={})",
                        intent.label, screenshot_index
                    );
                    crate::pipeline_log::log_decision("CU-FALLBACK", &intent.label, &format!("display_index={}", screenshot_index));
                    log::info!("UI-map miss for '{}', requesting CU coordinate", intent.label);
                    self.pending_intent = Some(intent.clone());
                    self.api_adapter.send(ClientMessage::RequestCUCoordinate {
                        intent,
                        screenshot_index,
                    });
                    self.cu_coordinate_pending = true;
                    self.cu_pending_since = Some(Instant::now());
                    log::info!("[CU-FALLBACK] cu_coordinate_pending=true, timer started");
                } else {
                    self.cu_coordinate_pending = false;
                    self.cu_pending_since = None;
                }

                let _ = display_infos;
            }
            UiEvent::UIATreeReady(elements) => {
                log::info!("[UIA] received {} elements from extraction", elements.len());
                crate::pipeline_log::log_uia(
                    elements.len(),
                    &elements.iter().take(10).map(|el| {
                        (el.name.clone(), el.control_type.clone(), el.rect.x, el.rect.y, el.rect.width, el.rect.height)
                    }).collect::<Vec<_>>(),
                );

                // Send element summaries to backend for LLM prompt enrichment.
                // Only send interactive elements with meaningful names — skip
                // empty names, single-character garbage, and non-interactive types.
                let summaries: Vec<clicky_types::UIElementSummary> = elements.iter()
                    .filter(|el| {
                        if el.name.trim().is_empty() {
                            return false;
                        }
                        if el.name.trim().len() <= 2
                            && el.name.trim().chars().all(|c| c.is_ascii_punctuation() || c.is_whitespace())
                        {
                            return false;
                        }
                        matches!(
                            el.control_type.as_str(),
                            "Button" | "CheckBox" | "ComboBox" | "Edit" | "Hyperlink"
                            | "ListItem" | "Menu" | "MenuBar" | "MenuItem" | "RadioButton"
                            | "Slider" | "Tab" | "TabItem" | "TreeItem" | "SplitButton"
                        )
                    })
                    .take(50)
                    .map(|el| {
                        let region = rough_region_hint(el.rect.center(), self.screen_w as f64, self.screen_h as f64);
                        clicky_types::UIElementSummary {
                            name: el.name.clone(),
                            control_type: el.control_type.clone(),
                            region_hint: region,
                            rect_x: el.rect.x,
                            rect_y: el.rect.y,
                            rect_w: el.rect.width,
                            rect_h: el.rect.height,
                        }
                    })
                    .collect();
                if !summaries.is_empty() {
                    self.api_adapter.send(ClientMessage::UIElementsSummary { elements: summaries });
                }

                self.last_uia_elements = elements;
                self.rebuild_screen_model();
            }
            UiEvent::CUCoordinateReady { coordinate } => {
                log::info!("[CU] CUCoordinateReady received: {:?}", coordinate);

                // Guard against double execution: only act if CU was actually pending.
                if !self.cu_coordinate_pending {
                    log::warn!("[CU] CUCoordinateReady arrived but cu_coordinate_pending=false; discarding");
                    return;
                }
                self.cu_coordinate_pending = false;
                self.cu_pending_since = None;
                log::info!("[CU] cu_coordinate_pending cleared");

                let raw_logical = if let Some((raw_cx, raw_cy)) = coordinate {
                    PointL { x: raw_cx, y: raw_cy }
                } else {
                    // CU found no element — use last-resort fallback (current cursor position).
                    println!("All detection paths failed — using fallback");
                    log::warn!("CU found no element — using current cursor position as last-resort fallback");
                    let pos = self.cursor_tracker.get_position();
                    let lbl = self.pending_intent.as_ref().map(|i| i.label.clone()).unwrap_or_default();
                    self.start_flight_logical(PointL { x: pos.0 as f64, y: pos.1 as f64 }, &lbl);
                    return;
                };

                println!("CU raw coordinate: ({:.0},{:.0})", raw_logical.x, raw_logical.y);

                // Snap raw CU coordinate (logical) to nearest UI element.
                // On snap failure, fall back to the raw CU coordinate rather than silently doing nothing.
                let snapped_logical = if let Some(ui_map) = &self.last_ui_map {
                    let display_infos = self.last_display_infos.as_deref().unwrap_or(&[]);
                    let cap = self.make_capture_descriptor_for_cursor(display_infos);
                    if let Some(cap) = cap {
                        // Convert logical → screenshot space for snapping, then back.
                        let ss_coord = coords::logical_to_screenshot(raw_logical, &cap);
                        log::info!(
                            "[SNAP] CU raw=({:.0},{:.0}) → ss=({:.0},{:.0}) radius={}px",
                            raw_logical.x, raw_logical.y, ss_coord.x, ss_coord.y, SNAP_RADIUS_PX as u32
                        );
                        if let Some(snapped_ss) = snap_to_nearest_ui_element(ss_coord, ui_map, SNAP_RADIUS_PX) {
                            let snapped = coords::screenshot_to_logical(snapped_ss, &cap);
                            log::info!(
                                "[SNAP] snapped ss=({:.0},{:.0}) → logical=({:.0},{:.0}) Δ=({:.1},{:.1})",
                                ss_coord.x, ss_coord.y, snapped.x, snapped.y,
                                snapped.x - raw_logical.x, snapped.y - raw_logical.y,
                            );
                            println!("Snapped coordinate: ({:.0},{:.0})", snapped.x, snapped.y);
                            snapped
                        } else {
                            // No UI element within snap radius — use raw CU coordinate.
                            log::info!(
                                "[SNAP] no UI element within {}px of ss=({:.0},{:.0}) — using raw CU coordinate",
                                SNAP_RADIUS_PX as u32, ss_coord.x, ss_coord.y
                            );
                            println!("No nearby UI element for CU coordinate (snap radius {}px)", SNAP_RADIUS_PX);
                            raw_logical
                        }
                    } else {
                        log::warn!("[SNAP] could not build CaptureDescriptor — using raw coordinate");
                        raw_logical
                    }
                } else {
                    log::warn!("[SNAP] no UI map available — using raw CU coordinate ({:.0},{:.0})", raw_logical.x, raw_logical.y);
                    raw_logical
                };

                // Apply drift compensation using the freshest cursor position.
                let current = self.cursor_tracker.get_position();
                let drift_x = (current.0 - self.mouse_pos_at_release.0) as f64;
                let drift_y = (current.1 - self.mouse_pos_at_release.1) as f64;
                let drifted = PointL {
                    x: snapped_logical.x + drift_x,
                    y: snapped_logical.y + drift_y,
                };

                let display_infos_vec = self.last_display_infos.clone().unwrap_or_default();
                if display_infos_vec.is_empty() {
                    log::warn!("[CU] last_display_infos is empty during CUCoordinateReady — coordinate mapping will use (0,0) origin");
                }
                let final_coord = if self.is_valid_logical_coordinate(drifted, &display_infos_vec) {
                    log::info!(
                        "[CU] coordinate passed validation: snapped=({:.0},{:.0}) drift=({:.1},{:.1}) final=({:.0},{:.0})",
                        snapped_logical.x, snapped_logical.y, drift_x, drift_y, drifted.x, drifted.y
                    );
                    drifted
                } else {
                    // Safety validation failed — use snapped coordinate without drift rather
                    // than falling back to cursor position, which would completely miss the target.
                    log::warn!(
                        "[CU] drifted coordinate ({:.0},{:.0}) failed validation — using snapped ({:.0},{:.0}) without drift",
                        drifted.x, drifted.y, snapped_logical.x, snapped_logical.y
                    );
                    snapped_logical
                };

                // Use the speech bubble text that was set when PipelineReadyWithIntent arrived.
                let label = self.pending_intent.as_ref().map(|i| i.label.clone()).unwrap_or_default();
                log::info!(
                    "[FINAL PIPELINE] raw=({:.0},{:.0}) → snapped=({:.0},{:.0}) → drifted=({:.0},{:.0}) → cursor=({:.0},{:.0}) label='{}'",
                    raw_logical.x, raw_logical.y,
                    snapped_logical.x, snapped_logical.y,
                    drifted.x, drifted.y,
                    final_coord.x, final_coord.y,
                    label,
                );
                // Emit full pipeline trace.
                coords::log_pipeline(
                    None,
                    None,
                    Some(raw_logical),
                    Some(snapped_logical),
                    Some(final_coord),
                    None,
                    None,
                );
                println!("FINAL COORDINATE USED: x={:.0} y={:.0}", final_coord.x, final_coord.y);
                self.start_flight_logical(final_coord, &label);
            }
            UiEvent::TtsError(err) => {
                log::warn!("TTS error (legacy): {err}");
            }
            UiEvent::PipelineError(err) => {
                log::error!("Pipeline error: {}", err);
                self.voice_state = VoiceState::Idle;
                self.render_state.voice_state = self.voice_state;
                self.processing_since = None;
                self.claude_pipeline_active = false;
            }
        }
    }

    /// Plays TTS audio or falls back to espeak-ng.
    fn play_tts(&mut self, mp3_bytes: Vec<u8>, spoken_text: &str) {
        if mp3_bytes.is_empty() {
            log::warn!("TTS unavailable — falling back to espeak-ng");
            let spoken_clean: String = spoken_text
                .chars()
                .filter(|&c| c != '\0')
                .take(500)
                .collect();
            if !spoken_clean.is_empty() {
                std::thread::spawn(move || {
                    let _ = std::process::Command::new("espeak-ng")
                        .arg(&spoken_clean)
                        .status();
                });
            }
        } else if let Some(p) = &mut self.audio_player {
            p.play_mp3(mp3_bytes);
            log::info!("TTS playback started");
        }
    }

    /// Starts the bezier flight animation to a logical desktop coordinate.
    ///
    /// On Windows also physically moves the OS cursor to the target using
    /// `spawn_blocking` so it never blocks the 60fps render loop.
    fn start_flight_logical(&mut self, logical: PointL, spoken_text: &str) {
        let local_x = logical.x - self.overlay_x as f64;
        let local_y = logical.y - self.overlay_y as f64;
        log::info!(
            "[FINAL PIPELINE] logical=({:.0},{:.0}) overlay_origin=({},{}) → local=({:.0},{:.0}) label='{}'",
            logical.x, logical.y,
            self.overlay_x, self.overlay_y,
            local_x, local_y,
            spoken_text
        );
        self.render_state.start_flight_to(local_x, local_y, spoken_text.to_owned());

        // Physically move the OS cursor to the target (Windows only).
        // Runs in spawn_blocking so it never stalls the render loop.
        // Uses SetCursorPos with the LOGICAL coordinate — Windows handles DPI
        // automatically when the process is Per-Monitor V2 DPI-aware.
        #[cfg(target_os = "windows")]
        {
            let target_x = logical.x as i32;
            let target_y = logical.y as i32;
            self.tokio_rt.spawn_blocking(move || {
                execute_cursor_move_with_retry(target_x, target_y);
            });
        }
    }

    /// Maps a `UiElement` (in SCREENSHOT pixel space) to LOGICAL desktop coordinates.
    ///
    /// Transform (exact, single application):
    ///   norm_x  = center.x / screenshot_width_pixels          (0..1)
    ///   norm_y  = center.y / screenshot_height_pixels         (0..1)
    ///   local_x = norm_x * display_width_points               (logical pts on this display)
    ///   local_y = norm_y * display_height_points
    ///   global  = (origin_x + local_x, origin_y + local_y)   (global logical desktop)
    fn map_element_to_logical(
        &self,
        element: &crate::ui_map::UiElement,
        screenshot_index: usize,
        display_infos: &[DisplayInfo],
    ) -> Option<PointL> {
        let display = display_infos.get(screenshot_index)?;
        let cap = CaptureDescriptor::from_display_info(display, 1.0);
        if cap.bitmap_width == 0 || cap.bitmap_height == 0 {
            log::warn!("[MAP] display[{}] screenshot dims are zero — cannot map", screenshot_index);
            return None;
        }
        if cap.logical_rect.width <= 0.0 || cap.logical_rect.height <= 0.0 {
            log::warn!("[MAP] display[{}] logical size is zero — DPI info missing", screenshot_index);
            return None;
        }
        let result = coords::screenshot_to_logical(element.center, &cap);
        log::info!(
            "[MAP] ss=({:.0},{:.0}) ss_size={}x{} → logical=({:.0},{:.0})",
            element.center.x, element.center.y,
            cap.bitmap_width, cap.bitmap_height,
            result.x, result.y,
        );
        Some(result)
    }

    /// Builds a `CaptureDescriptor` for the cursor's current display.
    fn make_capture_descriptor_for_cursor(&self, display_infos: &[DisplayInfo]) -> Option<CaptureDescriptor> {
        let display = display_infos.iter().find(|d| d.is_cursor_display)?;
        Some(CaptureDescriptor::from_display_info(display, 1.0))
    }

    /// Returns the cursor position in SCREENSHOT pixel space on the given display.
    fn screenshot_space_cursor(
        &self,
        screenshot_index: usize,
        display_infos: &[DisplayInfo],
    ) -> Option<PointS> {
        let display = display_infos.get(screenshot_index)?;
        let cap = CaptureDescriptor::from_display_info(display, 1.0);
        if cap.logical_rect.width <= 0.0 || cap.logical_rect.height <= 0.0 {
            return None;
        }
        let cursor = self.cursor_tracker.get_position();
        let logical = PointL { x: cursor.0 as f64, y: cursor.1 as f64 };
        Some(coords::logical_to_screenshot(logical, &cap))
    }

    /// Safety validation gate — rejects coordinates that are likely hallucinated or unsafe.
    ///
    /// Lightweight bounds-only validation for UIA-sourced coordinates.
    ///
    /// UIA coordinates are authoritative (from the OS accessibility tree), so we only
    /// reject non-finite values and coordinates outside all display bounds.
    /// No edge-pixel, bounding-box, or hallucination guards.
    fn is_valid_bounds_only(&self, coord: PointL, display_infos: &[DisplayInfo]) -> bool {
        if !coord.x.is_finite() || !coord.y.is_finite() {
            return false;
        }
        let caps: Vec<CaptureDescriptor> = display_infos
            .iter()
            .map(|d| CaptureDescriptor::from_display_info(d, 1.0))
            .collect();
        if !caps.is_empty() && !coords::is_within_any_display(coord, &caps) {
            return false;
        }
        true
    }

    /// Rejects when:
    /// 1. The coordinate is not finite.
    /// 2. The coordinate matches the geometric screen centre of any display (hallucination guard).
    ///    Threshold: within 3% of the display's shorter dimension.
    /// 3. (Only when UI map is available) No edge pixels exist within 15px.
    /// 4. (Only when UI map is available) No UI element bounding box is within 20px.
    ///
    /// NOTE: guards 3 & 4 are skipped when `last_ui_map` is absent. A missing map means
    /// the Sobel processing hasn't finished yet — it is not safe to reject on absent data,
    /// since the CU coordinate is the result of a real model inference.
    fn is_valid_logical_coordinate(&self, coord: PointL, display_infos: &[DisplayInfo]) -> bool {
        if !coord.x.is_finite() || !coord.y.is_finite() {
            log::warn!("[COORD-VALIDATE] ({:.0},{:.0}) is not finite — rejecting", coord.x, coord.y);
            return false;
        }

        // Build CaptureDescriptors for hallucination check.
        let caps: Vec<CaptureDescriptor> = display_infos
            .iter()
            .map(|d| CaptureDescriptor::from_display_info(d, 1.0))
            .collect();

        // Guard 2: screen-centre hallucination.
        if coords::is_screen_centre_hallucination(coord, &caps) {
            log::warn!(
                "[COORD-VALIDATE] ({:.0},{:.0}) matches screen centre — rejecting hallucination",
                coord.x, coord.y
            );
            return false;
        }

        // Guard 1b: outside all display bounds.
        if !caps.is_empty() && !coords::is_within_any_display(coord, &caps) {
            log::warn!(
                "[COORD-VALIDATE] ({:.0},{:.0}) is outside all display bounds — rejecting",
                coord.x, coord.y
            );
            return false;
        }

        // Guards 3 & 4 require the UI map. Skip entirely when absent — never reject
        // a good CU coordinate because the UI map hasn't been built yet.
        if let Some(ui_map) = &self.last_ui_map {
            let cap = self.make_capture_descriptor_for_cursor(display_infos);
            if let Some(cap) = cap {
                let ss = coords::logical_to_screenshot(coord, &cap);
                if crate::ui_map::no_edge_pixels_nearby(ss, &ui_map.edge_mask, 25) {
                    log::warn!(
                        "[COORD-VALIDATE] ({:.0},{:.0}) → ss({:.0},{:.0}): no edge pixels within 25px — rejecting",
                        coord.x, coord.y, ss.x, ss.y
                    );
                    return false;
                }
                if crate::ui_map::no_bounding_box_nearby(ss, ui_map, 40.0) {
                    log::warn!(
                        "[COORD-VALIDATE] ({:.0},{:.0}) → ss({:.0},{:.0}): no UI element within 40px — rejecting",
                        coord.x, coord.y, ss.x, ss.y
                    );
                    return false;
                }
            }
        } else {
            log::info!(
                "[COORD-VALIDATE] ({:.0},{:.0}): no UI map yet — skipping edge/bbox guards",
                coord.x, coord.y
            );
        }

        log::info!("[COORD-VALIDATE] ({:.0},{:.0}) passed all guards", coord.x, coord.y);
        true
    }

    /// Rebuilds the merged ScreenModel from the latest UIA and vision data.
    fn rebuild_screen_model(&mut self) {
        let capture = self.last_display_infos.as_ref().and_then(|infos| {
            infos.iter().find(|d| d.is_cursor_display)
                .map(|d| CaptureDescriptor::from_display_info(d, 1.0))
        });
        self.last_screen_model = Some(ScreenModel::from_sources(
            self.last_uia_elements.clone(),
            self.last_ui_map.as_ref(),
            capture,
        ));
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Converts a logical coordinate to a rough region hint string.
fn rough_region_hint(p: crate::core::coords::PointL, screen_w: f64, screen_h: f64) -> String {
    let rel_x = if screen_w > 0.0 { p.x / screen_w } else { 0.5 };
    let rel_y = if screen_h > 0.0 { p.y / screen_h } else { 0.5 };
    let v = if rel_y < 0.33 { "top" } else if rel_y > 0.66 { "bottom" } else { "center" };
    let h = if rel_x < 0.33 { "left" } else if rel_x > 0.66 { "right" } else { "center" };
    if v == "center" && h == "center" {
        "center".to_string()
    } else {
        format!("{v} {h}")
    }
}

// ─── Windows cursor execution (freeze-safe) ───────────────────────────────────

/// Moves the OS cursor to `(x, y)` in logical desktop space on Windows.
///
/// Design notes:
/// - Called from `spawn_blocking` — never runs on the render thread.
/// - `SetCursorPos` takes LOGICAL coordinates when the process is
///   `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2` (set in `main()`).
/// - Verification sleep reduced from 300ms to 50ms per the spec.
/// - After SetCursorPos, sends a zero-delta mouse_event as a backup nudge.
/// - Every attempt is logged to logs.txt via the pipeline logger.
#[cfg(target_os = "windows")]
fn execute_cursor_move_with_retry(x: i32, y: i32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, SetCursorPos,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{mouse_event, MOUSEEVENTF_MOVE};
    use windows_sys::Win32::Foundation::POINT;

    const MAX_RETRIES: u32 = 2;
    const RETRY_DELAY_MS: u64 = 50;

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS));
        }

        let ok = unsafe { SetCursorPos(x, y) };
        if ok == 0 {
            log::warn!("[CURSOR] SetCursorPos({x},{y}) failed on attempt {attempt}");
            crate::pipeline_log::log_exec(x, y, 0, 0, attempt, "SET_FAILED");
            continue;
        }

        // Backup nudge: zero-delta mouse_event to ensure the cursor position update
        // is processed by the input queue even if another process is fighting for it.
        unsafe { mouse_event(MOUSEEVENTF_MOVE, 0, 0, 0, 0); }

        std::thread::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS));

        // Verify the cursor actually moved to within 2 pixels (tolerates DPI rounding).
        let mut actual = POINT { x: 0, y: 0 };
        let got = unsafe { GetCursorPos(&mut actual) };
        if got != 0 {
            let dx = (actual.x - x).abs();
            let dy = (actual.y - y).abs();
            if dx <= 2 && dy <= 2 {
                log::info!(
                    "[CURSOR] moved to ({x},{y}) on attempt {attempt} — actual=({},{})",
                    actual.x, actual.y
                );
                crate::pipeline_log::log_exec(x, y, actual.x, actual.y, attempt, "SUCCESS");
                return;
            }
            log::warn!(
                "[CURSOR] SetCursorPos({x},{y}) attempt {attempt}: cursor at ({},{}) — Δ={dx},{dy} > 2px",
                actual.x, actual.y
            );
            crate::pipeline_log::log_exec(x, y, actual.x, actual.y, attempt, "RETRY");
        }
    }

    log::warn!("[CURSOR] SetCursorPos({x},{y}) did not converge after {MAX_RETRIES} retries — giving up");
    crate::pipeline_log::log_exec(x, y, 0, 0, MAX_RETRIES, "FAIL");
}
