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
use clicky_types::ClientMessage;
use crate::config::AppConfig;
use crate::core::audio_rms::AudioPowerLevelTracker;
use crate::cursor_tracker::CursorTracker;
use crate::hotkey::PushToTalkTransition;
use crate::overlay::gpu::Renderer;
use crate::overlay::renderer::{CursorNavigationMode, OverlayRenderState};
use crate::tray::TrayMenuEvent;

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
    // Per-frame mutable fields
    pub frame_count: u64,
    pub last_transcript: Option<String>,
    pub processing_since: Option<Instant>,
    pub claude_pipeline_active: bool,
    pub responding_since: Option<Instant>,
    pub pointing_hold_since: Option<Instant>,
    pub last_config_poll_at: Instant,
    pub last_config_modified_at: Option<std::time::SystemTime>,
    pub simulation_mode: bool,
    pub frame_start: Instant,
    pub elapsed_secs: f64,
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

        // Update waveform while listening
        if self.voice_state == VoiceState::Listening {
            if let Ok(tracker) = self.rms_tracker.lock() {
                self.render_state.audio_power_history = tracker.history().to_vec();
                self.render_state.audio_power_level = tracker.current_level();
            }
        }

        // Cursor position � winit fallback tracker gets updated via CursorMoved events.
        // The win32 tracker reads GetCursorPos directly, so update_from_window is a no-op.
        let current_mouse = self.cursor_tracker.get_position();
                // Send cursor position to backend periodically
        if self.frame_count % 6 == 0 {
            self.api_adapter.send(ClientMessage::CursorPosition {
                x: current_mouse.0,
                y: current_mouse.1,
            });
        }

        // Drain backend events
        while let Some(event) = self.api_adapter.try_recv() {
            self.handle_ui_event(event);
        }

        // Processing timeout
        if self.voice_state == VoiceState::Processing {
            let since = self.processing_since.get_or_insert(now);
            let timeout = if self.claude_pipeline_active { 30 } else { 3 };
            if now.duration_since(*since).as_secs() >= timeout {
                log::warn!("Processing timeout � returning to Idle");
                self.voice_state = VoiceState::Idle;
                self.render_state.voice_state = self.voice_state;
                self.processing_since = None;
                self.claude_pipeline_active = false;
            }
        }

        // Responding ? hold ? return flight ? Idle
        if self.voice_state == VoiceState::Responding {
            let since = self.responding_since.get_or_insert(now);
            let elapsed_total = now.duration_since(*since).as_secs();
            let tts_done = self
                .audio_player
                .as_ref()
                .map_or(true, |p| !p.is_playing());

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

            if elapsed_total >= 45 {
                log::warn!("Responding safety timeout � returning to Idle");
                if let Some(s) =
                    self.voice_state.apply(VoiceStateTransition::ResponseComplete)
                {
                    self.voice_state = s;
                    self.render_state.voice_state = self.voice_state;
                    self.render_state.return_to_following_mouse();
                }
                self.responding_since = None;
                self.pointing_hold_since = None;
            }
        }

        // Re-read cursor right before rendering -- this second sample is as close to the
        // actual draw call as possible, minimising the visual lag between the OS cursor
        // and our triangle. On Windows, GetCursorPos returns the hardware-reported position
        // from the kernel input queue, not a buffered event, so it is always up to date.
        let fresh_mouse = self.cursor_tracker.get_position();
        let fresh_mouse_local = (
            fresh_mouse.0 - self.overlay_x as f32,
            fresh_mouse.1 - self.overlay_y as f32,
        );

        if self.render_state.navigation_mode == CursorNavigationMode::FollowingMouse {
            self.render_state.cursor_x = fresh_mouse_local.0;
            self.render_state.cursor_y = fresh_mouse_local.1;
        }

        self.render_state.update(delta_seconds, Some(fresh_mouse_local));

        if !self.is_occluded {
            if let Some(renderer) = &mut self.renderer {
                renderer.render_frame(
                    &self.render_state,
                    &mut self.font_system,
                    self.elapsed_secs,
                );
            }
        }

        // Cap at 240 fps (4 ms per frame). Sleeping here also yields the CPU so the OS
        // can service new hardware mouse reports before the next GetCursorPos call.
        spin_sleep::sleep(Duration::from_micros(4_000));
    }
}

impl OverlayApp {
    fn handle_hotkey(&mut self, transition: PushToTalkTransition) {
        match transition {
            PushToTalkTransition::Pressed => {
                log::info!("Push-to-talk: PRESSED");
                if let Some(new_state) =
                    self.voice_state.apply(VoiceStateTransition::HotkeyPressed)
                {
                    self.voice_state = new_state;
                    self.render_state.voice_state = self.voice_state;
                    self.claude_pipeline_active = false;
                    self.responding_since = None;
                    self.pointing_hold_since = None;

                    if let Some(p) = &mut self.audio_player {
                        p.stop();
                    }
                    if let Ok(mut t) = self.rms_tracker.lock() {
                        t.reset();
                    }

                    match self.mic_capture.start() {
                        Ok(audio_rx) => {
                            log::info!("Mic capture started");
                            self.api_adapter.send(ClientMessage::HotkeyPressed);
                            let adapter_tx = self.api_adapter.client_tx_clone();
                            self.tokio_rt.spawn_blocking(move || {
                                while let Ok(chunk) = audio_rx.recv() {
                                    let pcm16 = crate::core::pcm16_converter::convert_float32_to_pcm16_mono(
                                        &chunk.samples,
                                        chunk.sample_rate,
                                        chunk.channels,
                                    );
                                    if adapter_tx
                                        .send(ClientMessage::AudioChunk { pcm16_bytes: pcm16 })
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            log::error!("Mic capture failed: {}", e);
                            if let Some(s) = self.voice_state.apply(VoiceStateTransition::Error) {
                                self.voice_state = s;
                                self.render_state.voice_state = self.voice_state;
                            }
                        }
                    }
                }
            }

            PushToTalkTransition::Released => {
                log::info!("Push-to-talk: RELEASED");
                self.mic_capture.stop();

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
                    self.tokio_rt.spawn_blocking(move || {
                        match crate::screenshot::capture_all_screens(
                            cursor_pos.0,
                            cursor_pos.1,
                            &platform_clone,
                        ) {
                            Ok(capture) => {
                                let _ = adapter_tx.send(ClientMessage::Screenshots {
                                    screenshots: capture.screenshots,
                                    display_infos: capture.display_infos,
                                });
                            }
                            Err(e) => {
                                log::error!("Screenshot capture failed: {}", e);
                            }
                        }
                    });

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
                log::info!("Partial transcript: {}", text);
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
                    log::info!("FINAL transcript: {}", transcript);
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
            UiEvent::LlmResponse {
                spoken_text,
                pointing_instruction,
                display_infos,
                computer_use_global_coordinate,
            } => {
                if self.voice_state != VoiceState::Processing {
                    return;
                }
                log::info!("LLM response: {}", spoken_text);

                if let Some(s) = self.voice_state.apply(VoiceStateTransition::ResponseReady) {
                    self.voice_state = s;
                    self.render_state.voice_state = self.voice_state;
                }

                if let Some((gx, gy)) = computer_use_global_coordinate {
                    self.render_state.start_flight_to(
                        gx - self.overlay_x as f64,
                        gy - self.overlay_y as f64,
                        spoken_text.clone(),
                    );
                } else if let Some(ref instruction) = pointing_instruction {
                    let target = crate::core::coordinate_mapper::find_target_display(
                        instruction.screen_number,
                        &display_infos,
                    );
                    if let Some(display) = target {
                        let coord = crate::core::coordinate_mapper::map_screenshot_pixels_to_global_display_coordinates(
                            instruction.screenshot_x,
                            instruction.screenshot_y,
                            display,
                        );
                        self.render_state.start_flight_to(
                            coord.x - self.overlay_x as f64,
                            coord.y - self.overlay_y as f64,
                            spoken_text.clone(),
                        );
                    }
                }

                let is_pointing =
                    computer_use_global_coordinate.is_some() || pointing_instruction.is_some();
                self.render_state.speech_bubble_text =
                    crate::core::bubble_text::pick_bubble_phrase(is_pointing);

                self.responding_since = Some(Instant::now());
                self.claude_pipeline_active = false;
                self.processing_since = None;
            }
            UiEvent::TtsAudio(mp3_bytes) => {
                if let Some(p) = &mut self.audio_player {
                    p.play_mp3(mp3_bytes);
                    log::info!("TTS playback started");
                }
            }
            UiEvent::TtsError(err) => {
                log::warn!("TTS failed: {} � falling back to espeak-ng", err);
                let spoken = self.render_state.speech_bubble_text.clone();
                if !spoken.is_empty() {
                    std::thread::spawn(move || {
                        let _ = std::process::Command::new("espeak-ng").arg(&spoken).spawn();
                    });
                }
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
}
