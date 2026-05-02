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
mod platform;
mod screenshot;
mod tray;

use api::adapter::{ApiAdapter, BACKEND_PORT};
use app::state_machine::VoiceState;
use audio::capture::MicCapture;
use core::audio_rms::AudioPowerLevelTracker;
use log::info;
use overlay::renderer::OverlayRenderState;
use overlay::window::OverlayApp;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use winit::event_loop::EventLoop;

fn main() {
    let _ = dotenvy::dotenv();
    env_logger::init();
    configure_windows_dpi_awareness();

    let app_config = config::AppConfig::load();
    let platform = app::platform::detect();
    info!("Clicky Desktop starting on {}", platform);

    #[cfg(target_os = "linux")]
    {
        gtk::init().expect("Failed to initialize GTK");
    }

    let tokio_rt = Arc::new(
        tokio::runtime::Runtime::new().expect("Failed to create tokio runtime"),
    );

    let rms_tracker = Arc::new(Mutex::new(AudioPowerLevelTracker::new()));
    let mic_capture = MicCapture::new(Arc::clone(&rms_tracker));

    #[cfg(debug_assertions)]
    let simulation_mode = std::env::var("CLICKY_SIMULATE")
        .map(|v| v != "0")
        .unwrap_or(false);
    #[cfg(not(debug_assertions))]
    let simulation_mode = false;

    if simulation_mode {
        info!("SIMULATION MODE enabled -- skipping real API calls");
    }

    if let Some(p) = config::config_file_path() {
        info!("Config file path: {}", p.display());
    }

    let audio_player = match audio::playback::AudioPlayer::new() {
        Ok(p) => {
            info!("Audio output ready");
            Some(p)
        }
        Err(e) => {
            log::warn!("Audio output not available: {}", e);
            None
        }
    };

    let tray_icon = match tray::ClickyTrayIcon::new() {
        Ok(t) => {
            info!("System tray icon ready");
            Some(t)
        }
        Err(e) => {
            log::warn!("System tray not available: {}", e);
            None
        }
    };

    let hotkey_manager = hotkey::create(&platform, app_config.push_to_talk_hotkey);
    let last_config_poll_at = Instant::now();
    let last_config_modified_at = config::config_file_modified_at();

    let (overlay_x, overlay_y, screen_w, screen_h) =
        screenshot::detect_overlay_bounds(&platform);
    info!(
        "Overlay bounds: origin=({}, {}) size={}x{}",
        overlay_x, overlay_y, screen_w, screen_h
    );

    #[cfg(target_os = "linux")]
    let cursor_tracker = cursor_tracker::create(&platform, screen_w, screen_h);
    #[cfg(not(target_os = "linux"))]
    let cursor_tracker = cursor_tracker::create(&platform);

    // Verify the backend is reachable before entering the event loop.
    let api_adapter = tokio_rt
        .block_on(ApiAdapter::try_connect(BACKEND_PORT))
        .unwrap_or_else(|e| {
            log::error!(
                "Cannot connect to backend on port {} � is `cargo run -p clicky-backend` running? ({})",
                BACKEND_PORT,
                e
            );
            std::process::exit(1);
        });

    info!("Backend reachable, ApiAdapter connected on port {}", BACKEND_PORT);

    // Enter the tokio runtime so tokio::spawn / spawn_blocking work from the
    // synchronous winit callbacks.
    let _tokio_guard = tokio_rt.enter();

    let font_system = overlay::font::create_font_system();
    let render_state = OverlayRenderState::new();

    let mut app = OverlayApp {
        platform,
        app_config,
        api_adapter,
        font_system,
        rms_tracker,
        mic_capture,
        audio_player,
        tray_icon,
        hotkey_manager,
        cursor_tracker,
        tokio_rt,
        render_state,
        voice_state: VoiceState::Idle,
        overlay_x,
        overlay_y,
        screen_w,
        screen_h,
        frame_count: 0,
        last_transcript: None,
        processing_since: None,
        claude_pipeline_active: false,
        responding_since: None,
        pointing_hold_since: None,
        last_config_poll_at,
        last_config_modified_at,
        simulation_mode,
        frame_start: Instant::now(),
        elapsed_secs: 0.0,
        last_mouse_pos: (0.0, 0.0),
        mouse_velocity: (0.0, 0.0),
        window: None,
        renderer: None,
        is_occluded: false,
    };

    info!("Entering winit event loop at 60fps");
    let event_loop = EventLoop::new().expect("Failed to create event loop");
    event_loop.run_app(&mut app).expect("Event loop error");

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
