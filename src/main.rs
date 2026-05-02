mod api;
mod app;
mod audio;
mod autostart;
mod config;
mod core;
mod cursor_tracker;
mod hotkey;
mod overlay;
mod panel;
mod screenshot;
mod tray;

use api::adapter::{ApiAdapter, BACKEND_PORT};
use app::state_machine::{VoiceState, VoiceStateTransition};
use audio::capture::MicCapture;
use audio::UiEvent;
use clicky_types::ClientMessage;
use core::audio_rms::AudioPowerLevelTracker;
use hotkey::PushToTalkTransition;
use log::info;
use overlay::renderer::{self, CursorNavigationMode, OverlayRenderState};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tray::TrayMenuEvent;

fn main() {
    // The frontend's .env only needs non-API-key settings (hotkey, autostart, etc.)
    let _ = dotenvy::dotenv();
    env_logger::init();
    configure_windows_dpi_awareness();

    let mut app_config = config::AppConfig::load();

    let platform = app::platform::detect();
    info!("Clicky Desktop starting on {}", platform);

    #[cfg(target_os = "linux")]
    {
        gtk::init().expect("Failed to initialize GTK");
    }

    // Tokio runtime for the ApiAdapter background WS task
    let tokio_rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");

    let rms_tracker = Arc::new(Mutex::new(AudioPowerLevelTracker::new()));
    let mut mic_capture = MicCapture::new(Arc::clone(&rms_tracker));

    #[cfg(debug_assertions)]
    let simulation_mode = std::env::var("CLICKY_SIMULATE")
        .map(|v| v != "0")
        .unwrap_or(false);
    #[cfg(not(debug_assertions))]
    let simulation_mode = false;

    if simulation_mode {
        info!("SIMULATION MODE enabled — skipping real API calls");
    }

    if let Some(config_path) = config::config_file_path() {
        info!("Config file path: {}", config_path.display());
    }

    let mut audio_player = match audio::playback::AudioPlayer::new() {
        Ok(player) => {
            info!("Audio output ready");
            Some(player)
        }
        Err(err) => {
            log::warn!("Audio output not available: {}", err);
            None
        }
    };

    let tray_icon = match tray::ClickyTrayIcon::new() {
        Ok(tray) => {
            info!("System tray icon ready");
            Some(tray)
        }
        Err(err) => {
            log::warn!("System tray not available: {}", err);
            None
        }
    };

    let mut hotkey_manager = hotkey::create(&platform, app_config.push_to_talk_hotkey);
    let mut last_config_poll_at = Instant::now();
    let mut last_config_modified_at = config::config_file_modified_at();

    let (overlay_x, overlay_y, screen_w, screen_h) = screenshot::detect_overlay_bounds(&platform);
    info!(
        "Overlay bounds: origin=({}, {}) size={}x{}",
        overlay_x, overlay_y, screen_w, screen_h
    );

    let (mut raylib_handle, raylib_thread) =
        renderer::create_overlay_window(overlay_x, overlay_y, screen_w, screen_h, &platform);

    let actual_w = raylib_handle.get_screen_width();
    let actual_h = raylib_handle.get_screen_height();
    info!(
        "Raylib actual window: {}x{} (requested {}x{})",
        actual_w, actual_h, screen_w, screen_h
    );

    #[cfg(target_os = "linux")]
    let cursor_tracker = cursor_tracker::create(&platform, screen_w, screen_h);
    #[cfg(not(target_os = "linux"))]
    let cursor_tracker = cursor_tracker::create(&platform);

    let mut render_state = OverlayRenderState::new();
    render_state.bubble_font = renderer::load_bubble_font(&mut raylib_handle, &raylib_thread);
    let mut voice_state = VoiceState::Idle;

    // Verify the backend is reachable before entering the render loop.
    // A missing backend would previously cause an opaque panic (exit 101) deep
    // inside the ws_task reconnect loop; this gives a clear error and exits cleanly.
    // This check runs before tokio_rt.enter() because block_on cannot be called
    // from within an entered runtime context.
    let backend_url = format!("ws://127.0.0.1:{}", BACKEND_PORT);
    match tokio_rt.block_on(tokio_tungstenite::connect_async(&backend_url)) {
        Ok(_) => {
            info!("Backend reachable at {}", backend_url);
        }
        Err(err) => {
            log::error!(
                "Cannot connect to backend at {} — is `cargo run -p clicky-backend` running? ({})",
                backend_url,
                err
            );
            std::process::exit(1);
        }
    }

    // Enter the tokio runtime for the remainder of main so that tokio::spawn
    // and spawn_blocking calls from the synchronous render loop work correctly.
    let _tokio_guard = tokio_rt.enter();

    // Connect the WebSocket adapter — must be done inside the tokio runtime
    let api_adapter = ApiAdapter::connect(BACKEND_PORT);
    info!("ApiAdapter connected to backend on port {}", BACKEND_PORT);

    info!("Entering main render loop at 60fps");

    let mut frame_count: u64 = 0;
    let mut last_transcript: Option<String> = None;
    let mut processing_since: Option<Instant> = None;
    let mut claude_pipeline_active = false;
    let mut responding_since: Option<Instant> = None;
    let mut pointing_hold_since: Option<Instant> = None;

    while !raylib_handle.window_should_close() {
        frame_count += 1;
        let delta_seconds = raylib_handle.get_frame_time() as f64;

        // Reload config on file change
        if last_config_poll_at.elapsed() >= Duration::from_millis(750) {
            last_config_poll_at = Instant::now();
            let current_modified_at = config::config_file_modified_at();
            if current_modified_at != last_config_modified_at {
                last_config_modified_at = current_modified_at;
                let reloaded = config::AppConfig::load();
                let hotkey_changed = reloaded.push_to_talk_hotkey != app_config.push_to_talk_hotkey;
                app_config = reloaded;
                log::info!("Reloaded settings from config file");
                if hotkey_changed {
                    hotkey_manager = hotkey::create(&platform, app_config.push_to_talk_hotkey);
                    log::info!(
                        "Applied new push-to-talk hotkey: {}",
                        app_config.push_to_talk_hotkey.display_name()
                    );
                }
            }
        }

        // Poll tray menu
        if let Some(tray) = &tray_icon {
            if let Some(event) = tray.poll_menu_event() {
                match event {
                    TrayMenuEvent::Quit => {
                        info!("Quit requested from tray menu");
                        break;
                    }
                    TrayMenuEvent::ToggleOverlay => {
                        info!("Toggle overlay requested");
                    }
                    TrayMenuEvent::OpenSettings => {
                        info!("Open settings requested");
                        let latest_config = config::AppConfig::load();
                        panel::open_settings_window(latest_config.clone());
                    }
                }
            }
        }

        // Poll hotkey
        if let Some(ref hotkey) = hotkey_manager {
            if let Some(transition) = hotkey.poll_hotkey_event() {
                match transition {
                    PushToTalkTransition::Pressed => {
                        info!("Push-to-talk: PRESSED");
                        if let Some(new_state) =
                            voice_state.apply(VoiceStateTransition::HotkeyPressed)
                        {
                            voice_state = new_state;
                            render_state.voice_state = voice_state;
                            claude_pipeline_active = false;
                            responding_since = None;
                            pointing_hold_since = None;

                            if let Some(ref mut player) = audio_player {
                                player.stop();
                            }
                            if let Ok(mut tracker) = rms_tracker.lock() {
                                tracker.reset();
                            }

                            match mic_capture.start() {
                                Ok(audio_rx) => {
                                    info!("Mic capture started");

                                    // Notify backend
                                    api_adapter.send(ClientMessage::HotkeyPressed);

                                    // Bridge audio chunks to the backend via the adapter
                                    let adapter_audio_tx = api_adapter.client_tx_clone();
                                    tokio_rt.spawn_blocking(move || {
                                        while let Ok(chunk) = audio_rx.recv() {
                                            let pcm16 = crate::core::pcm16_converter::convert_float32_to_pcm16_mono(
                                                &chunk.samples,
                                                chunk.sample_rate,
                                                chunk.channels,
                                            );
                                            if adapter_audio_tx
                                                .send(ClientMessage::AudioChunk { pcm16_bytes: pcm16 })
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                    });
                                }
                                Err(err) => {
                                    log::error!("Mic capture failed: {}", err);
                                    if let Some(s) = voice_state.apply(VoiceStateTransition::Error) {
                                        voice_state = s;
                                        render_state.voice_state = voice_state;
                                    }
                                }
                            }
                        }
                    }

                    PushToTalkTransition::Released => {
                        info!("Push-to-talk: RELEASED");
                        mic_capture.stop();

                        if simulation_mode {
                            if let Some(new_state) =
                                voice_state.apply(VoiceStateTransition::HotkeyReleased)
                            {
                                voice_state = new_state;
                                render_state.voice_state = voice_state;
                            }
                            processing_since = Some(Instant::now());

                            let sim_tx = api_adapter.client_tx_clone();
                            let sw = screen_w;
                            let sh = screen_h;
                            tokio_rt.spawn(async move {
                                tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;
                                let margin_x = sw as f64 * 0.2;
                                let margin_y = sh as f64 * 0.2;
                                let range_x = sw as f64 * 0.6;
                                let range_y = sh as f64 * 0.6;
                                let rand_seed = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .subsec_nanos();
                                let _target_x = margin_x + (rand_seed as f64 % range_x);
                                let _target_y = margin_y + ((rand_seed / 7) as f64 % range_y);
                                drop(sim_tx);
                            });
                        } else {
                            api_adapter.send(ClientMessage::HotkeyReleased);

                            let cursor_pos = cursor_tracker.get_position();
                            let platform_clone = platform.clone();
                            let adapter_tx = api_adapter.client_tx_clone();
                            tokio_rt.spawn_blocking(move || {
                                match screenshot::capture_all_screens(
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
                                    Err(err) => {
                                        log::error!("Screenshot capture failed: {}", err);
                                    }
                                }
                            });

                            if let Some(new_state) =
                                voice_state.apply(VoiceStateTransition::HotkeyReleased)
                            {
                                voice_state = new_state;
                                render_state.voice_state = voice_state;
                            }
                        }
                    }
                }
            }
        }

        // Update waveform while listening
        if voice_state == VoiceState::Listening {
            if let Ok(tracker) = rms_tracker.lock() {
                render_state.audio_power_history = tracker.history().to_vec();
                render_state.audio_power_level = tracker.current_level();
                if frame_count % 30 == 0 {
                    info!("Audio RMS: {:.4}", tracker.current_level());
                }
            }
        }

        // Send cursor position to backend every frame
        let mouse_position = raylib_handle.get_mouse_position();
        cursor_tracker.update_from_window(mouse_position.x, mouse_position.y);
        let current_mouse_position = cursor_tracker.get_position();
        let current_mouse_local = (
            current_mouse_position.0 - overlay_x as f32,
            current_mouse_position.1 - overlay_y as f32,
        );

        // Only send cursor position updates periodically (not every 60fps frame) to avoid flooding
        if frame_count % 6 == 0 {
            api_adapter.send(ClientMessage::CursorPosition {
                x: current_mouse_position.0,
                y: current_mouse_position.1,
            });
        }

        // Poll events from the backend adapter
        while let Some(event) = api_adapter.try_recv() {
            match event {
                UiEvent::PartialTranscript(text) => {
                    info!("Partial transcript: {}", text);
                    last_transcript = Some(text);
                    processing_since = None;
                }
                UiEvent::FinalTranscript(text) => {
                    let transcript = if text.is_empty() {
                        last_transcript.clone().unwrap_or_default()
                    } else {
                        text
                    };

                    if transcript.is_empty() {
                        voice_state = VoiceState::Idle;
                        render_state.voice_state = voice_state;
                        processing_since = None;
                    } else {
                        info!("FINAL transcript: {}", transcript);
                        last_transcript = Some(transcript);
                        processing_since = Some(Instant::now());
                        claude_pipeline_active = true;
                    }
                }
                UiEvent::TranscriptionError(err) => {
                    log::error!("Transcription error: {}", err);
                    voice_state = VoiceState::Idle;
                    render_state.voice_state = voice_state;
                    processing_since = None;
                }
                UiEvent::LlmResponse {
                    spoken_text,
                    pointing_instruction,
                    display_infos,
                    computer_use_global_coordinate,
                } => {
                    if voice_state != VoiceState::Processing {
                        continue;
                    }

                    info!("LLM response: {}", spoken_text);

                    if let Some(new_state) = voice_state.apply(VoiceStateTransition::ResponseReady) {
                        voice_state = new_state;
                        render_state.voice_state = voice_state;
                    }

                    if let Some((global_x, global_y)) = computer_use_global_coordinate {
                        render_state.start_flight_to(
                            global_x - overlay_x as f64,
                            global_y - overlay_y as f64,
                            spoken_text.clone(),
                        );
                    } else if let Some(ref instruction) = pointing_instruction {
                        let target = core::coordinate_mapper::find_target_display(
                            instruction.screen_number,
                            &display_infos,
                        );
                        if let Some(display) = target {
                            let coord =
                                core::coordinate_mapper::map_screenshot_pixels_to_global_display_coordinates(
                                    instruction.screenshot_x,
                                    instruction.screenshot_y,
                                    display,
                                );
                            render_state.start_flight_to(
                                coord.x - overlay_x as f64,
                                coord.y - overlay_y as f64,
                                spoken_text.clone(),
                            );
                        }
                    }

                    let is_pointing =
                        computer_use_global_coordinate.is_some() || pointing_instruction.is_some();
                    render_state.speech_bubble_text =
                        core::bubble_text::pick_bubble_phrase(is_pointing);

                    responding_since = Some(Instant::now());
                    claude_pipeline_active = false;
                    processing_since = None;
                }
                UiEvent::TtsAudio(mp3_bytes) => {
                    if let Some(ref mut player) = audio_player {
                        player.play_mp3(mp3_bytes);
                        info!("TTS playback started");
                    }
                }
                UiEvent::TtsError(err) => {
                    log::warn!("TTS failed: {} — falling back to espeak-ng", err);
                    let spoken = render_state.speech_bubble_text.clone();
                    if !spoken.is_empty() {
                        std::thread::spawn(move || {
                            let _ = std::process::Command::new("espeak-ng").arg(&spoken).spawn();
                        });
                    }
                }
                UiEvent::PipelineError(err) => {
                    log::error!("Pipeline error: {}", err);
                    voice_state = VoiceState::Idle;
                    render_state.voice_state = voice_state;
                    processing_since = None;
                    claude_pipeline_active = false;
                }
            }
        }

        // Processing timeout
        if voice_state == VoiceState::Processing {
            let now = Instant::now();
            let since = processing_since.get_or_insert(now);
            let timeout_secs = if claude_pipeline_active { 30 } else { 3 };
            if now.duration_since(*since).as_secs() >= timeout_secs {
                log::warn!("Processing timeout — returning to Idle");
                voice_state = VoiceState::Idle;
                render_state.voice_state = voice_state;
                processing_since = None;
                claude_pipeline_active = false;
            }
        }

        // Responding → hold → return flight → Idle
        if voice_state == VoiceState::Responding {
            let now = Instant::now();
            let since = responding_since.get_or_insert(now);
            let elapsed_total = now.duration_since(*since).as_secs();
            let tts_done = audio_player.as_ref().map_or(true, |p| !p.is_playing());

            match render_state.navigation_mode {
                CursorNavigationMode::NavigatingToTarget => {}
                CursorNavigationMode::PointingAtTarget => {
                    if tts_done {
                        let hold_since = pointing_hold_since.get_or_insert(now);
                        if now.duration_since(*hold_since).as_secs_f64() >= 2.0 {
                            let mouse_pos = cursor_tracker.get_position();
                            render_state.start_return_flight(mouse_pos.0, mouse_pos.1);
                            pointing_hold_since = None;
                        }
                    }
                }
                CursorNavigationMode::ReturningToMouse => {}
                CursorNavigationMode::FollowingMouse => {
                    if let Some(new_state) = voice_state.apply(VoiceStateTransition::ResponseComplete) {
                        voice_state = new_state;
                        render_state.voice_state = voice_state;
                    }
                    responding_since = None;
                    pointing_hold_since = None;
                }
            }

            if elapsed_total >= 45 {
                log::warn!("Responding safety timeout — returning to Idle");
                if let Some(new_state) = voice_state.apply(VoiceStateTransition::ResponseComplete) {
                    voice_state = new_state;
                    render_state.voice_state = voice_state;
                    render_state.return_to_following_mouse();
                }
                responding_since = None;
                pointing_hold_since = None;
            }
        }

        // Update cursor position
        if render_state.navigation_mode == CursorNavigationMode::FollowingMouse {
            render_state.cursor_x = current_mouse_local.0;
            render_state.cursor_y = current_mouse_local.1;
        }

        render_state.update(delta_seconds, Some(current_mouse_local));

        let mut draw_handle = raylib_handle.begin_drawing(&raylib_thread);
        renderer::draw_overlay_frame(&mut draw_handle, &render_state);
    }

    info!("Clicky Desktop shutting down");
}

#[cfg(target_os = "windows")]
fn configure_windows_dpi_awareness() {
    use windows_sys::Win32::UI::HiDpi::{
        SetProcessDpiAwareness, SetProcessDpiAwarenessContext,
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, PROCESS_PER_MONITOR_DPI_AWARE,
    };

    let aware_v2 =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    if aware_v2 != 0 {
        log::info!("Enabled Windows DPI awareness: Per-Monitor V2");
        return;
    }

    let per_monitor = unsafe { SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE) };
    if per_monitor >= 0 {
        log::info!("Enabled Windows DPI awareness: Per-Monitor");
    } else {
        log::warn!("Could not enable Windows DPI awareness before overlay setup");
    }
}

#[cfg(not(target_os = "windows"))]
fn configure_windows_dpi_awareness() {}
