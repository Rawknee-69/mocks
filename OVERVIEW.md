# Clicky Desktop — Full Technical Overview

Clicky Desktop is a push-to-talk voice assistant that lives in the system tray. The user holds a hotkey to speak; the app transcribes their voice, captures their screen(s), sends everything to an LLM to understand the request, flies a blue triangle cursor to the relevant UI element, and speaks the answer aloud.

The system runs as **two separate processes** communicating over a local WebSocket on `ws://127.0.0.1:9001`:

- **`clicky-desktop`** (frontend) — the winit/wgpu transparent overlay, hotkey, mic capture, screenshot, UIA extraction, UI-map matching, and cursor animation.
- **`clicky-backend`** (backend) — the pipeline server that owns all external API calls: Deepgram STT, OpenRouter/Gemini LLM, Deepgram TTS, and Computer Use.

Shared WebSocket message types live in the **`clicky-types`** crate.

---

## Cargo Workspace

```
Cargo.toml                     ← workspace members = [".", "backend", "clicky-types"]
clicky-types/src/lib.rs        ← shared ClientMessage / ServerMessage / ElementIntent / UIElementSummary
backend/                       ← pipeline server  (clicky-backend)
src/                           ← desktop overlay  (clicky-desktop)
```

### Run

```bash
# Terminal 1 — backend pipeline server
cargo run -p clicky-backend

# Terminal 2 — frontend desktop overlay
cargo run -p clicky-desktop
```

---

## End-to-End Data Flow

```
[User holds hotkey]
       │
       ▼
[Frontend: cpal mic capture starts]
       │  PCM16 16kHz mono chunks → ClientMessage::AudioChunk
       ▼
[Backend: run_transcription_bridge()]
       │  pre-buffers audio during Deepgram WS connect
       │  Deepgram Nova-3 WebSocket (nova-3, linear16, 16kHz, punctuate, smart_format)
       │  is_final=true → accumulate; speech_final=true after CloseStream → emit Final
       │  ServerMessage::PartialTranscript / FinalTranscript → frontend
       ▼
[User releases hotkey]
       │  ClientMessage::HotkeyReleased → drain audio (1500ms) → CloseStream
       ▼
[Frontend: capture_all_screens()]
       │  Hyprland/Sway: grim -o <output> per monitor
       │  GNOME: xcap + gsettings animations off/on around capture
       │  Windows/X11: xcap
       │  → scale to MAX_WIDTH=1280 (Lanczos3), draw 200px red coordinate grid, JPEG quality 80
       │  ClientMessage::Screenshots { screenshots, display_infos }
       ▼
[Frontend: extract_ui_tree() — Windows only]
       │  COM IUIAutomation::FindAll(TreeScope_Subtree), spawn_blocking
       │  filter interactive types, sort by area asc
       │  ClientMessage::UIElementsSummary { elements }
       ▼
[Backend: run_llm_pipeline()]
       │  inject UIA summaries as user turn (up to 30 elements)
       │  build vision message: screenshots as base64 images
       │  non-streaming LLM call (Gemini generateContent / OpenRouter chat/completions)
       │  parse trailing intent JSON: {"label","action","region_hint","screen_number"}
       │  strip JSON block from spoken_text (TTS must not read raw JSON)
       │  Deepgram Aura-2 TTS synthesis
       │  ServerMessage::PipelineReady { spoken_text, mp3_bytes, coordinate:None, display_infos, intent }
       ▼
[Frontend: coordinate resolution pipeline]
       │
       ├─ 1. UIA scored_match()  [Windows only, threshold 0.75]
       │       name_sim×0.70 + role×0.20 + region×0.10 + keyword_boost + topbar_heuristic
       │       → PointL (logical desktop, from UIA rect center)
       │
       ├─ 2. ui_map::match_intent()  [threshold 0.65 / soft 0.55 with region_hint]
       │       Structural rules: new_tab→rightmost in tab row, close→top-right, zoom→adjacent pair
       │       Fallback: region(0.50)+size(0.20)+position_bias(0.15)+cursor_capped(0.15)+row(0.10)
       │       → PointS (screenshot px) → PointL via CaptureDescriptor
       │
       └─ 3. Computer Use fallback (on-demand)
               ClientMessage::RequestCUCoordinate { intent, screenshot_index }
               Backend: detect_element_location() two-pass zoom
                 Pass 1: resize full screenshot to MAX_LONG_EDGE=1920, coarse coord
                 Pass 2: 25% crop centred on pass-1, upscale to 1920 (no .min(1.0)), fine coord
                 Coord normalisation: [0,1] → scale to pixels; [0,1000] Gemini → scale
                 5-strategy parse: first-last-brace, strip_reasoning, strip_fences, last_brace_span, extract_coords
                 Backend hallucination guard: reject if within 3% of display centre or outside bounds
               ServerMessage::CUCoordinate → frontend
       │
       ▼
[Frontend: enhanced_snap(coord, model, uia_radius, vision_radius)]
       │  Priority 1: inside UIA rect → accept as-is
       │  Priority 2: nearest UIA center < uia_radius → snap
       │  Priority 3: nearest vision blob center < vision_radius (90px) → snap
       │  None → reject
       ▼
[Frontend: validate]
       │  is_within_any_display()
       │  is_screen_centre_hallucination() — reject within 3% of display centre
       ▼
[Frontend: pipeline_log [PIPELINE][SNAP][VALIDATE][EXEC]]
       ▼
[Frontend: render_state.start_flight_to(target_x, target_y, bubble_text)]
       ▼
[Frontend: bezier flight NavigatingToTarget → PointingAtTarget (2s hold)]
[Frontend: AudioPlayer::play_mp3(mp3_bytes)]
[Frontend: speech bubble typewriter reveal]
       ▼
[start_return_flight → ReturningToMouse → FollowingMouse → Idle]
```

---

## Frontend — `clicky-desktop` (`src/`)

### Module Map

```
src/
├── main.rs
│   Startup: DPI awareness, AppConfig::load(), platform detect, tokio runtime,
│   AudioPowerLevelTracker, MicCapture, AudioPlayer, ClickyTrayIcon, hotkey manager,
│   detect_overlay_bounds(), cursor_tracker, ApiAdapter::try_connect(),
│   FontSystem, OverlayRenderState, screenshot_local_tx/rx channel,
│   OverlayApp construction → EventLoop::run_app()
│
├── app/
│   ├── platform.rs      OS/display-server/compositor detection
│   │                    OperatingSystem { Linux, Windows, MacOS }
│   │                    DisplayServer { X11, Wayland }
│   │                    WaylandCompositor { Hyprland, Sway, Gnome, Other }
│   └── state_machine.rs VoiceState { Idle, Listening, Processing, Responding }
│                        VoiceStateTransition { HotkeyPressed, HotkeyReleased,
│                          ResponseReady, ResponseComplete, Error }
│                        VoiceState::apply(transition) → Option<VoiceState>
│
├── api/
│   └── adapter.rs       ApiAdapter { client_tx, ui_event_rx, ui_event_tx }
│                        try_connect(port): 2s timeout, reuses established stream
│                        send(ClientMessage): non-blocking try_send
│                        try_recv() → Option<UiEvent>
│                        client_tx_clone(): for audio forward tasks
│                        ui_event_tx_clone(): for local tasks (UIA extraction)
│                        ws_task: auto-reconnect, backoff 250ms→5s
│                        server_message_to_ui_event(): PipelineReady→PipelineReadyWithIntent,
│                          CUCoordinate→CUCoordinateReady, Ping→None
│
├── audio/
│   ├── capture.rs       MicCapture: cpal device open, callback → PCM chunks,
│   │                    pcm16_converter → 16kHz mono PCM16 bytes
│   ├── playback.rs      AudioPlayer: rodio Sink, play_mp3(bytes), stop(), is_playing()
│   └── mod.rs           UiEvent enum:
│                          PartialTranscript(String)
│                          FinalTranscript(String)
│                          TranscriptionError(String)
│                          PipelineReady { spoken_text, mp3_bytes, coordinate, display_infos }
│                          PipelineReadyWithIntent { spoken_text, mp3_bytes, intent, display_infos }
│                          CUCoordinateReady { coordinate: Option<(f64,f64)> }
│                          UIATreeReady(Vec<UIElement>)
│                          PipelineError(String)
│                          TtsError(String)
│
├── autostart/
│   └── mod.rs           set_enabled(bool)
│                        Linux: ~/.config/autostart/clicky-desktop.desktop
│                        Windows: HKCU\Software\Microsoft\Windows\CurrentVersion\Run
│
├── config/
│   └── mod.rs           AppConfig { push_to_talk_hotkey: PushToTalkHotkey, autostart_enabled: bool }
│                        PushToTalkHotkey { CtrlSpace (default), CtrlGrave }
│                        AppConfig::load() → load_from_disk() → TOML parse
│                        config_file_path() → ProjectDirs("com","tgomareli","clicky-desktop")
│                        config_file_modified_at() → fs::metadata mtime
│
├── core/
│   ├── audio_rms.rs     AudioPowerLevelTracker: rolling RMS history (44 values),
│   │                    update(samples), current_level() → f64
│   ├── bezier_flight.rs compute_control_point(sx,sy,ex,ey,is_return) → (cx,cy)
│   │                    compute_flight_duration_seconds(sx,sy,ex,ey,is_return)
│   │                    compute_flight_frame(progress,sx,sy,cx,cy,ex,ey,is_return)
│   │                    → FlightFrame { x, y, rotation_radians, scale }
│   │                    Forward: 600px/s, arc 25%, 0.8–1.8s, smoothstep easing
│   │                    Return: 900px/s, arc 15%, 0.4–1.0s
│   ├── bubble_text.rs   pick_bubble_phrase(is_pointing) → &'static str
│   │                    Curated friendly phrases: "right here!", "found it!", etc.
│   ├── coords.rs        PointM{u,v}  PointS{x,y}  PointL{x,y}  PointP{x,y}  RectL
│   │                    CaptureDescriptor { id, logical_rect, bitmap_w/h, dpi_scale }
│   │                    CaptureDescriptor::from_display_info(DisplayInfo, dpi_scale)
│   │                    model_to_screenshot(PointM, cap) → PointS
│   │                    screenshot_to_logical(PointS, cap) → PointL
│   │                    logical_to_screenshot(PointL, cap) → PointS
│   │                    logical_to_physical(PointL, scale) → PointP
│   │                    model_to_logical(PointM, cap) → PointL
│   │                    is_within_any_display(PointL, [CaptureDescriptor]) → bool
│   │                    is_screen_centre_hallucination(PointL, [CaptureDescriptor]) → bool
│   │                    log_pipeline(model,screenshot,logical,refined,snapped,physical,cursor)
│   ├── design_system.rs Color/size/animation tokens
│   │                    cursor::DEFAULT_ROTATION_DEGREES, OFFSET_FROM_SYSTEM_CURSOR
│   ├── mod.rs           re-exports
│   └── pcm16_converter.rs f32 audio → PCM16 16kHz mono via rubato FftFixedOut
│
├── cursor_tracker/
│   ├── evdev_tracker.rs Linux Wayland: read /dev/input/event* for REL_X/REL_Y
│   ├── fallback.rs      Atomic (x,y) updated by caller; CursorTracker trait impl
│   ├── win32_tracker.rs Windows: GetCursorPos() each frame
│   └── mod.rs           CursorTracker trait: get_position() → (f32,f32)
│                        create(platform) → Box<dyn CursorTracker>
│
├── hotkey/
│   ├── evdev_hotkey.rs  Linux Wayland: monitor /dev/input/event* for KEY_LEFTCTRL+KEY_SPACE
│   ├── global_hotkey_backend.rs X11/Windows: global-hotkey crate
│   └── mod.rs           HotkeyBackend trait: poll() → Option<PushToTalkTransition>
│                        PushToTalkTransition { Pressed, Released }
│                        create(platform, hotkey) → Option<Box<dyn HotkeyBackend>>
│
├── overlay/
│   ├── window.rs        OverlayApp: winit ApplicationHandler
│   │                    about_to_wait(): main 60fps loop body
│   │                    – DwmFlush() at frame start (Windows vsync gate)
│   │                    – cursor tracker poll → send CursorPosition to backend
│   │                    – hotkey poll → VoiceState transitions
│   │                    – tray menu events (Quit/Settings/Toggle)
│   │                    – ApiAdapter::try_recv() → handle UiEvent
│   │                    – screenshot_local_rx.try_recv() → build UiMap + ScreenModel
│   │                    – config mtime poll every 750ms (hot-reload)
│   │                    – render_state.update(delta, mouse_pos)
│   │                    – wgpu paint + UpdateLayeredWindow (Windows)
│   │                    – per-second timing diagnostics
│   │
│   │                    UiEvent handling:
│   │                      PipelineReadyWithIntent → coordinate resolution pipeline
│   │                        1. try_uia_match() → PointL
│   │                        2. match_intent() → PointS → PointL
│   │                        3. RequestCUCoordinate (cu_coordinate_pending guard)
│   │                      CUCoordinateReady → enhanced_snap → validate → start_flight_to
│   │                      UIATreeReady → last_uia_elements, UIElementsSummary to backend
│   │                      PartialTranscript/FinalTranscript → last_transcript
│   │
│   ├── renderer.rs      OverlayRenderState:
│   │                      navigation_mode: CursorNavigationMode
│   │                        { FollowingMouse, NavigatingToTarget, PointingAtTarget, ReturningToMouse }
│   │                      cursor_x/y, cursor_rotation_degrees, cursor_scale
│   │                      active_flight: Option<ActiveFlightAnimation>
│   │                      audio_power_level, audio_power_history: Vec<f64> (44 values)
│   │                      speech_bubble_text, _visible_char_progress, _visible_char_count
│   │                      speech_bubble_opacity, _visible_text, _cached_char_count
│   │                      speech_bubble_buffer: Option<cosmic_text::Buffer>
│   │                    update(delta, mouse_pos): advance flight, fade bubble
│   │                    start_flight_to(target_x, target_y, bubble_text)
│   │                    start_return_flight(mouse_x, mouse_y)
│   │                    return_to_following_mouse()
│   │                    advance_return_to_mouse(): linear chase 900px/s, done at ≤6px
│   │
│   ├── paint.rs         tiny-skia draw calls each frame:
│   │                    – triangle: blue filled shape + bloom (scaled transparent copies)
│   │                    – waveform: 20 bars proportional to RMS history (Listening)
│   │                    – arc: 240° rotating partial arc + fading tail + pulsing dot (Processing)
│   │                    – speech bubble: glass rect + cosmic-text layout, typewriter reveal
│   │                    Uploads pixel buffer as wgpu texture each frame
│   │
│   ├── gpu.rs           Renderer: wgpu Instance/Device/Queue/Surface/Pipeline
│   │                    create_surface(window): Vulkan backend on Windows (required for pre-mult alpha)
│   │                    submit_frame(pixmap): upload texture, draw quad, present
│   │
│   ├── font.rs          create_font_system() → cosmic_text::FontSystem
│   │                    loads system fonts (Segoe UI / AdwaitaSans / Ubuntu / DejaVu / SF)
│   │
│   ├── wgpu_impl.rs     wgpu frame submission helpers
│   ├── windows_impl.rs  Win32: setup_layered_window(), update_layered_window(hwnd, pixmap),
│   │                    DwmExtendFrameIntoClientArea(all -1 margins), DwmFlush()
│   └── mod.rs
│
├── panel/
│   └── mod.rs           egui settings window (opened from tray)
│                        Shows: hotkey selector, autostart checkbox, config file path
│
├── pipeline_log.rs      Append-only structured log to logs.txt
│                        log_screen(display_index, width, height, origin_x, origin_y, dpi_scale)
│                        log_uia(count, sample_elements)
│                        log_vision(count, sample_elements)
│                        log_model_input(query, elements_sent)
│                        log_model_output(intent_label, region_hint, confidence)
│                        log_pipeline_trace(model,screenshot,logical,refined,snapped,physical,cursor)
│                        log_snap(source, coord, snapped, radius)
│                        log_validate(coord, passed, reason)
│                        log_exec(target_x,target_y,actual_x,actual_y,attempt,status)
│                        log_decision(path, label, detail)
│                        log_uia_match(intent,candidate,norm_intent,norm_candidate,score,accepted)
│                        log_raw(section, message)
│
├── platform/
│   ├── mod.rs           query_refresh_hz() → u32
│   ├── windows.rs       setup_overlay_window(hwnd): WS_EX_LAYERED|TRANSPARENT|TOOLWINDOW|NOACTIVATE
│   │                    DwmExtendFrameIntoClientArea, DPI awareness
│   ├── linux_x11.rs     _NET_WM_STATE_SKIP_TASKBAR/PAGER/ABOVE via x11rb
│   └── linux_wayland.rs Wayland-specific window hints
│
├── screen_model.rs      ScreenModel { uia_elements, visual_elements, capture }
│                        VisualElement { center_ss: PointS, width, height }
│                        ScreenModel::from_sources(uia_elements, ui_map, capture)
│                        try_uia_match(label,action,region_hint,screen_w,screen_h) → Option<(PointL,desc)>
│                          1. scored_match() threshold 0.75
│                          2. legacy: match_by_label_and_role()
│                          3. legacy: match_by_label()
│                        enhanced_snap(coord,model,uia_radius,vision_radius,ui_map,capture) → Option<PointL>
│                          1. Inside UIA rect → accept
│                          2. Nearest UIA center < uia_radius → snap
│                          3. snap_to_nearest_ui_element() within vision_radius
│
├── screenshot/
│   └── mod.rs           capture_all_screens(cursor_x,cursor_y,platform) → CaptureResult
│                        Hyprland/Sway: grim -o <output> -t png -  (per monitor)
│                        GNOME: gsettings animations off → xcap → restore
│                        Windows/X11: xcap::Monitor::all() → capture_image()
│                        scale_image: MAX_WIDTH=1280, Lanczos3
│                        draw_coordinate_grid: red semi-transparent lines every 200px,
│                          3×5 bitmap font labels at intersections (CLICKY_GRID_OVERLAY env)
│                        JPEG quality 80
│                        detect_overlay_bounds: Windows SM_XVIRTUALSCREEN full virtual desktop
│                        Each capture produces: ScreenshotData { jpeg_data, label } +
│                          DisplayInfo { screen_number, global_origin_x/y,
│                            display_width/height_points, screenshot_width/height_pixels,
│                            is_cursor_display }
│
├── tray/
│   └── mod.rs           ClickyTrayIcon: tray-icon crate
│                        TrayMenuEvent { Quit, OpenSettings, ToggleOverlay }
│                        poll() → Option<TrayMenuEvent>
│
├── uia/
│   ├── mod.rs           UIElement { name, control_type, rect: RectL, is_enabled, automation_id }
│   │                    UIElement::center() → PointL
│   │                    scored_match(elements,label,role_hint,region_hint,screen_w,screen_h)
│   │                      → Option<(&UIElement, f64, String)>
│   │                      name_score: exact=1.0, norm_exact=0.95, contains=0.85, token×0.85
│   │                      role_score: matched=1.0, no_hint defaults by type=0.5
│   │                      region_score: point_in_region()=1.0
│   │                      combined: name×0.70 + role×0.20 + region×0.10
│   │                      keyword boost (+0.15): new tab,close,minimize,maximize,zoom,back,...
│   │                      top-bar heuristic (+0.10): region contains "top" && center.y<120
│   │                      threshold 0.75
│   │                    match_by_label(elements,label) → Option<&UIElement>
│   │                    match_by_role(elements,role) → Option<&UIElement>
│   │                    match_by_label_and_role(elements,label,role) → Option<&UIElement>
│   │                    normalize_label: strip "button","icon","tab", lowercase, collapse whitespace
│   │                    token_match(a,b): fraction of a-tokens found in b
│   │                    point_in_region(p,screen_w,screen_h,hint): left/right/top/bottom/center
│   │
│   └── win.rs           extract_ui_tree() → Vec<UIElement>  [Windows only]
│                        CoInitializeEx(COINIT_MULTITHREADED)
│                        CoCreateInstance(CUIAutomation)
│                        GetRootElement() → FindAll(TreeScope_Subtree, TrueCondition)
│                        Interactive types: Button(50000),CheckBox,ComboBox,Edit,Hyperlink,
│                          ListItem,Menu,MenuBar,MenuItem,RadioButton,Slider,Tab,TabItem,
│                          TreeItem,SplitButton
│                        Filters: is_interactive_type(), is_meaningful_name(), rect ≥ 4px
│                        Returns sorted by area ascending (specific targets first)
│                        Must be called from spawn_blocking (50–200ms, COM blocking)
│
└── ui_map/
    └── mod.rs           build_ui_map(jpeg_bytes) → Result<UiMap, UiMapError>
    │                    Algorithm: RGBA→gray→3×3 box blur→Sobel edges→threshold(30)
    │                    →union-find connected components→size filter(4–200000px²)
    │                    →aspect ratio filter(0.1–10.0)→UiElement{center:PointS,w,h}
    │                    UiMap retains binary edge_mask for safety validation
    │
    │                    match_intent(map,intent,cursor_pos) → Option<MatchResult>
    │                    Structural first:
    │                      group_by_rows(elements): y-centre tolerance 12px → sorted left→right
    │                      is_tab_row(row): ≥5 elements, all y<120px
    │                      is_new_tab_intent: rightmost in tab row, score=0.95
    │                      is_close_intent: find_top_right_element (rel_x>0.70, y<120), score=0.95
    │                      is_zoom_intent: find_zoom_pair (dx<50,dy<12,area_diff<30), pick left/right
    │                    Fallback scoring (cursor bias capped at 0.15):
    │                      text_sim(jaro_winkler): OCR label if available
    │                      region_hint: +0.50 if center in hinted region
    │                      size_weight: h∈[24,80]→0.20, h∈[12,24)→0.18, h∈(80,120]→0.10
    │                      position_bias: rel_y∈[0.20,0.80]→+0.15
    │                      cursor_score: (1-d/max_d)×0.15 capped
    │                      row_membership: element in row≥3 → +0.10
    │                    Threshold: MATCH_CONFIDENCE_THRESHOLD=0.65
    │                    Soft accept: region_hint present && score≥0.55
    │
    │                    snap_to_nearest_ui_element(coord:PointS,map,radius:90px) → Option<PointS>
    │                    Selection: area × (1 - dist/radius) composite score
    │
    │                    refine_with_crop(jpeg,coarse_center,crop_size) → Result<Option<PointS>>
    │                    Re-runs Sobel on a crop; picks element closest to crop centre
    │
    │                    no_edge_pixels_nearby(coord,edge_mask,radius) → bool (safety validation)
    │                    no_bounding_box_nearby(coord,map,radius) → bool
    │
    │                    jaro_winkler(a,b) → f32: inline implementation, no regex crate

---

## Backend — `clicky-backend` (`backend/src/`)

### Module Map

```
backend/src/
├── main.rs              dotenvy::dotenv(), logger::init(), tokio Runtime, server::run(PORT)
│
├── server.rs            WebSocket server on 127.0.0.1:9001
│                        Startup: Arc<reqwest::Client> (once, shared), build_provider×2
│                          (llm_provider, cu_provider from AI_PROVIDER/LLM_MODEL/CU_MODEL env)
│                          Arc<Semaphore>(RPM_CAP=20), Arc<Semaphore>(TTS_CONCURRENT_CAP=45)
│                        handle_connection per TCP stream:
│                          WebSocketConfig::default(), max_message_size=32MB
│                          (server_tx,server_rx) mpsc:64 for ServerMessage
│                          forward_to_websocket task: ServerMessage::Ping→WS Ping frame, else JSON
│                          (pipeline_tx,pipeline_rx) mpsc:64 for internal pipeline events
│                          relay task: pipeline_rx → server_tx
│                          keepalive task: Ping every 15s
│                          Per-connection state:
│                            last_screenshots: Option<(Vec<ScreenshotData>,Vec<DisplayInfo>)>
│                            last_uia_summaries: Vec<UIElementSummary>
│                            cursor_position: (f32,f32)
│                            conversation_history: Vec<(String,String)> max 10
│                            audio_tx: Option<SyncSender<Vec<u8>>>
│                            active_cancel: Option<oneshot::Sender<()>>
│                        handle_client_message(message, ...):
│                          HotkeyPressed → run_transcription_bridge task
│                          AudioChunk → audio_tx.try_send
│                          HotkeyReleased → drop audio_tx, fire cancel
│                          CancelPipeline → fire cancel
│                          CursorPosition → update cursor_position
│                          Screenshots → store last_screenshots
│                          UIElementsSummary → store last_uia_summaries
│                          RequestCUCoordinate → spawn detect_element_location task
│                            result → backend hallucination guard → bounds check → CUCoordinate
│                        FinalTranscript intercept → run_llm_pipeline task (stateless, empty history)
│                        PipelineReady → update conversation_history
│
├── error.rs             AppError { Config, Transcription, Llm, Tts, ComputerUse, Screenshot, Internal }
│                        AppError::into_server_message() → ServerMessage::Error { code, message }
│
├── logger.rs            tracing_subscriber init, LOG_LEVEL env var (default "info")
│
├── api/
│   ├── mod.rs           Provider enum:
│   │                      OpenRouter { api_key: String, model: String }
│   │                      Gemini    { api_key: String, model: String }
│   │                    Constructed once at server startup, wrapped in Arc
│   │
│   ├── gemini.rs        stream_llm_response(client,api_key,model,system_prompt,messages,rate_limiter)
│   │                    Uses generateContent (non-streaming): contents.len()==1 (stateless)
│   │                    Response: candidates[0].content.parts[0].text
│   │                    build_vision_message_content(transcript,screenshots)
│   │                    → Gemini inlineData parts array
│   │
│   ├── openrouter.rs    stream_llm_response(client,api_key,model,system_prompt,messages,rate_limiter)
│   │                    Uses chat/completions with "stream":false
│   │                    DEFAULT_OPENROUTER_MODEL: configurable via LLM_MODEL env
│   │                    build_vision_message_content(transcript,screenshots)
│   │                    → OpenRouter image_url parts array
│   │
│   ├── computer_use.rs  detect_element_location(client,provider,jpeg,question,display_w,display_h,rate_limiter)
│   │                    → Option<ComputerUseCoordinate { display_local_x, display_local_y }>
│   │
│   │                    DEFAULT_CU_MODEL: "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free"
│   │                    MAX_LONG_EDGE=1920, ZOOM_FRACTION=0.25, JPEG_QUALITY=85, TIMEOUT=45s
│   │
│   │                    compute_model_dims(w,h): scale=min(1920/max(w,h),1.0), JPEG MCU align2
│   │                    compute_model_dims_allow_upscale(w,h): no .min(1.0) — always upscales
│   │
│   │                    Pass 1: full screenshot → model dims → call_with_retry
│   │                    Pass 2: Crop::centred_on(coarse,ZOOM_FRACTION) → compute_model_dims_allow_upscale
│   │                            crop_and_resize: map logical→native px → crop → Lanczos3 → JPEG
│   │                            RefinementView::unproject(mx,my) adds crop offset back
│   │                    Pass 2 failure → keep Pass 1 result
│   │
│   │                    call_with_retry(client,provider,jpeg,prompt,rate_limiter,pass):
│   │                      single call_once attempt; Ok(Some)→coord, Ok(None)→no element, Err→None
│   │
│   │                    parse_coordinate(text) → ParseResult { Found(x,y), NoElement, ParseFailed }
│   │                    5 strategies:
│   │                      0. first_to_last_brace substring
│   │                      1. strip_reasoning (</think>,</reasoning>,[/THINK])
│   │                      2. strip_fences (```json)
│   │                      3. last_brace_span
│   │                      4. extract_coords: scan for "x": and "y": keys (no regex crate)
│   │                    FlexF64: deserialises numeric string OR number (reasoning models emit "640")
│   │                    denormalise_if_needed: [0,1]→scale×dims; [0,1000] Gemini→scale (bounds-exceedance)
│   │
│   │                    Prompts built at call site (never inside network layer — no double-wrap):
│   │                      build_prompt(question,w,h): coordinate system + task + output rules + checklist
│   │                      build_pass2_prompt(question,w,h,crop,display_w,display_h): HIGH-ZOOM CROP context
│   │
│   │                    Debug: write_cu_debug → test_ss/cu_{ts}_pass{p}_attempt{a}.{jpg,txt}
│   │
│   ├── deepgram.rs      StreamingTranscriptionSession:
│   │                    start(api_key): connect wss://api.deepgram.com/v1/listen?
│   │                      model=nova-3&encoding=linear16&sample_rate=16000&channels=1
│   │                      &punctuate=true&interim_results=true&utterance_end_ms=1500
│   │                      &endpointing=400&vad_events=true&smart_format=true
│   │                    audio_sender() → clone of mpsc::Sender<Vec<u8>>
│   │                    take_transcript_receiver() → mpsc::Receiver<TranscriptUpdate>
│   │                    request_final_transcript() → sends CloseStream JSON
│   │
│   │                    Transcript logic:
│   │                      is_final=true → accumulate in accumulated_finals
│   │                      speech_final=true after CloseStream → emit Final immediately + break
│   │                      speech_final=true before CloseStream → emit as Partial (user may still speak)
│   │                      UtteranceEnd + got_close + accumulated → emit Final + break
│   │                      WS close without Final → emit accumulated as Final
│   │                      Empty transcript with speech_final → skip (silence segment)
│   │
│   ├── deepgram_tts.rs  synthesize_speech_bytes(client,api_key,voice,text) → Vec<u8>
│   │                    POST https://api.deepgram.com/v1/speak
│   │                    model: DEEPGRAM_TTS_VOICE env (default "aura-2-thalia-en")
│   │                    → MP3 bytes
│   │
│   └── t.rs             internal test/type helpers
│
├── core/
│   ├── context_compactor.rs  (retained, unused — context is fully stateless)
│   ├── coordinate_mapper.rs  CU local coord → global: origin + scale
│   ├── pcm16_converter.rs    PCM16 conversion helpers
│   └── point_parser.rs       (legacy POINT tag parser — superseded by intent JSON)
│
└── handlers/
    ├── llm_pipeline.rs  run_llm_pipeline(client,config,transcript,screenshots,display_infos,
    │                      history,uia_summaries,server_tx)
    │                    INTENT_SYSTEM_PROMPT: instructs LLM to be conversational,
    │                      append JSON intent block at end, NEVER guess coordinates
    │                    1. inject uia_summaries (up to 30) as user turn
    │                    2. build_vision_message_content per provider
    │                    3. non-streaming LLM call via run_llm_call()
    │                    4. parse_intent_json(text): rfind '{', serde_json parse "label" key
    │                    5. strip_intent_json(text): remove trailing JSON block for TTS
    │                    6. synthesize_speech_bytes() → mp3_bytes
    │                    7. ServerMessage::PipelineReady { spoken_text, mp3_bytes, coordinate:None, ... }
    │                    Debug: append_to_log → clicky_llm_log.txt
    │                    Debug screenshot: CLICKY_DEBUG_SCREENSHOTS=1 → %TEMP%/clicky_debug_screenshot_N.jpg
    │
    ├── transcription.rs run_transcription_bridge(audio_rx,server_tx,http_client,api_key,cancel_rx)
    │                    MIN_CHUNK_BYTES=3200
    │                    1. spawn_blocking: audio_rx → buffer ≥3200 bytes → async_audio_tx
    │                    2. StreamingTranscriptionSession::start(api_key)
    │                    3. replay pre-buffered audio
    │                    4. spawn audio forward task: async_audio_rx → session.audio_sender()
    │                    5. spawn transcript relay: TranscriptUpdate → ServerMessage
    │                    6. await cancel_rx (hotkey released)
    │                    7. drain audio 1500ms timeout
    │                    8. request_final_transcript() (CloseStream)
    │                    9. wait final_rx 6000ms timeout → empty FinalTranscript on timeout
    │
    └── tts.rs           synthesize_speech_bytes(client,api_key,voice_opt,text) → Vec<u8>
                         thin wrapper around deepgram_tts::synthesize

---

## Shared Types — `clicky-types/src/lib.rs`

```rust
// ─── Frontend → Backend ──────────────────────────────────────────────────────
#[serde(tag="type", content="payload")]
pub enum ClientMessage {
    HotkeyPressed,
    AudioChunk          { pcm16_bytes: Vec<u8> },
    HotkeyReleased,
    CancelPipeline,
    CursorPosition      { x: f32, y: f32 },
    Screenshots         { screenshots: Vec<ScreenshotData>, display_infos: Vec<DisplayInfo> },
    RequestCUCoordinate { intent: ElementIntent, screenshot_index: usize },
    UIElementsSummary   { elements: Vec<UIElementSummary> },
}

// ─── Backend → Frontend ──────────────────────────────────────────────────────
#[serde(tag="type", content="payload")]
pub enum ServerMessage {
    PartialTranscript { text: String },
    FinalTranscript   { text: String },
    LlmTextDelta      { chunk: String },  // not currently used by render loop
    PipelineReady {
        spoken_text:   String,
        mp3_bytes:     Vec<u8>,
        coordinate:    Option<(f64,f64)>,   // always None — frontend resolves position
        display_infos: Vec<DisplayInfo>,
        intent:        Option<ElementIntent>,
    },
    CUCoordinate { coordinate: Option<(f64,f64)> },
    Error        { code: ErrorCode, message: String },
    #[serde(skip)]
    Ping,  // transport keepalive — WS Ping frame, never JSON
}

// ─── Shared data types ────────────────────────────────────────────────────────
pub struct ElementIntent {
    pub label:       String,        // "Submit button", "Search field"
    pub action:      String,        // "click" | "focus" | "hover"
    pub region_hint: Option<String>,// "top right", "center", "bottom left"
    pub screen_number: Option<u32>, // 1-based; None = cursor's screen
}

pub struct DisplayInfo {
    pub screen_number:             u32,
    pub global_origin_x:           f64,
    pub global_origin_y:           f64,
    pub display_width_points:      f64,
    pub display_height_points:     f64,
    pub screenshot_width_pixels:   u32,
    pub screenshot_height_pixels:  u32,
    pub is_cursor_display:         bool,
}

pub struct ScreenshotData {
    pub jpeg_data: Vec<u8>,
    pub label:     String,  // "screen N of M (WxH pixels) — cursor is on this screen (primary focus)"
}

pub struct UIElementSummary {
    pub name:         String,
    pub control_type: String,
    pub region_hint:  String,
    pub rect_x: f64, pub rect_y: f64, pub rect_w: f64, pub rect_h: f64,
}

pub struct PointingInstruction {   // legacy — used in clicky-types but superseded by ElementIntent
    pub screenshot_x:  f64,
    pub screenshot_y:  f64,
    pub label:         String,
    pub screen_number: Option<u32>,
}

pub enum ErrorCode { Transcription, LlmPipeline, Tts, ComputerUse, Screenshot, Internal }
```

---

## Voice State Machine

```
Idle ──[HotkeyPressed]──► Listening ──[HotkeyReleased]──► Processing ──[ResponseReady]──► Responding ──[ResponseComplete]──► Idle
  ▲                           │                │                  │
  └─────────────[Error]───────┘────────────────┘──────────────────┘

  Listening  ──[HotkeyPressed]──► Listening   (cancel + restart)
  Processing ──[HotkeyPressed]──► Listening   (cancel + restart)
  Responding ──[HotkeyPressed]──► Listening   (cancel + restart)
```

| State | Overlay shows | Active subsystems |
|---|---|---|
| `Idle` | Blue triangle tracking mouse, bloom | Nothing |
| `Listening` | 20-bar waveform at cursor position | cpal mic, PCM16 converter, Deepgram WS |
| `Processing` | 240° rotating arc + pulsing dot | Waiting for backend PipelineReady |
| `Responding` | Triangle flying to target, speech bubble | TTS playback, bezier animation |

VoiceState::apply(transition) → Option<VoiceState>; invalid transitions return None (silently ignored).

---

## Coordinate Type System (`src/core/coords.rs`)

All coordinate math uses named types — no raw `(x, y)` tuples anywhere in the pipeline.

```
Model output (0–1 normalised)
        PointM { u, v }
             │  model_to_screenshot(cap)
             ▼
Screenshot pixels (JPEG bitmap origin top-left)
        PointS { x, y }
             │  screenshot_to_logical(cap)
             ▼
Logical desktop (universal internal space — all pipeline stages use this)
        PointL { x, y }
             │  logical_to_physical(scale)
             ▼
Physical pixels (DPI-scaled — ONLY at final cursor execution)
        PointP { x, y }
```

`CaptureDescriptor { id, logical_rect: RectL, bitmap_width, bitmap_height, dpi_scale }`
is the single source of truth for one display. `from_display_info(DisplayInfo, dpi_scale)` creates it.

Transform formulae (applied exactly once, never composed):
- model → screenshot: `x_S = u × W_bitmap`
- screenshot → logical: `x_L = origin_x + x_S × (W_logical / W_bitmap)`
- logical → screenshot (inverse, only for snap): `x_S = (x_L - origin_x) × (W_bitmap / W_logical)`
- logical → physical: `x_P = x_L × dpi_scale`

Validation helpers:
- `is_within_any_display(p, displays)`: bounds check against all CaptureDescriptors
- `is_screen_centre_hallucination(p, displays)`: reject within 3% of shorter display dimension from centre

---

## Overlay Rendering Stack

```
winit EventLoop
     │ about_to_wait() — 60fps
     ▼
DwmFlush()                  ← vsync gate on Windows (MUST be at frame start)
cursor_tracker.get_position()
hotkey.poll()
tray.poll()
ApiAdapter::try_recv()      ← drain UiEvent channel
screenshot_local_rx.try_recv()
config mtime check (750ms)
OverlayRenderState::update(delta, mouse_pos)
     │
     ▼
paint::draw_overlay_frame(render_state, font_system)
     │ tiny-skia CPU rasterisation into Pixmap
     │ Triangle + bloom copies (always)
     │ Waveform bars (Listening)
     │ Rotating arc + dot (Processing)
     │ Speech bubble + typewriter text (NavigatingToTarget/PointingAtTarget)
     ▼
gpu::Renderer::submit_frame(pixmap)
     │ wgpu: upload texture, draw fullscreen quad, present
     ▼
windows_impl::update_layered_window(hwnd, pixmap)
     │ RGBA→BGRA, UpdateLayeredWindow with BLENDFUNCTION(AC_SRC_ALPHA)
```

**Windows requirements:**
- `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE` — click-through, no taskbar
- `DwmExtendFrameIntoClientArea` with all-(-1) MARGINS — DWM glass compositing
- Vulkan backend required (`wgpu::Backends::VULKAN`) — DX12 only gives `Opaque` alpha mode
- Do NOT add `WS_EX_NOREDIRECTIONBITMAP` alongside `WS_EX_LAYERED` — they conflict

**Cursor animation cycle:**
```
FollowingMouse
  →[start_flight_to()]→ NavigatingToTarget
  →[flight complete]→ PointingAtTarget (2s hold after TTS ends)
  →[start_return_flight()]→ ReturningToMouse (linear chase 900px/s, done at ≤6px)
  →[arrive]→ FollowingMouse
```

**Speech bubble:**
- Fade-in: opacity += 4.0×delta (≤1.0)
- Typewriter: visible_char_count += 30 chars/s
- Fade-out: opacity -= 5.0×delta on ReturningToMouse/FollowingMouse
- cosmic-text `Buffer` allocated once, reused; recreated on width change
- cached_char_count invalidation prevents per-frame layout rebuilds

---

## Deepgram STT Protocol Details

WebSocket URL parameters:
`model=nova-3&encoding=linear16&sample_rate=16000&channels=1&punctuate=true&interim_results=true&utterance_end_ms=1500&endpointing=400&vad_events=true&smart_format=true`

Audio framing: raw PCM16 as binary WS messages, buffered in ≥3200-byte chunks.

Message types received:
- `Metadata`, `SpeechStarted` → skip
- `UtteranceEnd` + got_close + accumulated → emit Final + break
- `is_final:true` → accumulate in accumulated_finals
- `speech_final:true` after CloseStream → emit Final immediately + break
- `speech_final:true` before CloseStream → emit as Partial (natural boundary, not done yet)
- `Error` → emit TranscriptUpdate::Error + break
- Empty `speech_final:true` → skip (silence segment)
- WS close without Final → emit accumulated_finals.join(" ") as Final

Finalization sequence on hotkey release:
1. drop audio_tx (closes send side, blocking bridge finishes)
2. drain async_audio_rx → session.audio_sender() (1500ms timeout)
3. `request_final_transcript()` → sends `{"type":"CloseStream"}`
4. wait `final_rx` up to 6000ms → empty FinalTranscript on timeout

---

## Computer Use Two-Pass Zoom (Backend)

```
Pass 1 — coarse
  full screenshot
  → compute_model_dims(display_w, display_h):
      scale = min(1920 / max(w,h), 1.0)   ← never upscales full screenshot
      ModelDims { width: align2(w×scale), height: align2(h×scale) }
  → resize_screenshot(jpeg, dims): Lanczos3, to_rgb8, JPEG quality 85
  → call_with_retry(prompt=build_prompt(question,w,h))
  → denormalise_if_needed([0,1] or [0,1000] Gemini)
  → (coarse_dx, coarse_dy) in display logical pts

Pass 2 — fine (4× zoom crop)
  Crop::centred_on(coarse_dx, coarse_dy, display_w, display_h, ZOOM_FRACTION=0.25)
    clamped so crop never overflows display
  → compute_model_dims_allow_upscale(crop.w, crop.h):
      scale = 1920 / max(crop.w, crop.h)   ← no .min(1.0), always upscales
  → crop_and_resize: logical px → native screenshot px → crop → Lanczos3 → JPEG
  → RefinementView { logical_x, logical_y, logical_w, logical_h, bitmap_w, bitmap_h }
  → call_with_retry(prompt=build_pass2_prompt(question,zoom_w,zoom_h,crop,...))
  → RefinementView::unproject(mx, my):
      x_L = logical_x + mx × (logical_w / bitmap_w)   ← adds crop offset back
      y_L = logical_y + my × (logical_h / bitmap_h)
  → (fine_dx, fine_dy) in display logical pts

Backend hallucination guard (in server.rs, not computer_use.rs):
  gx = origin_x + display_local_x
  gy = origin_y + display_local_y
  threshold = min(display_w, display_h) × 0.03
  if dist(gx,gy, cx,cy) < threshold → reject
  if gx/gy outside display bounds → reject
  → ServerMessage::CUCoordinate { coordinate: Option<(f64,f64)> }
```

---

## Screenshot Capture Details

Each captured monitor produces:
1. **ScreenshotData**: scaled JPEG (max 1280px wide, quality 80) with 200px coordinate grid
2. **DisplayInfo**: geometry metadata for coordinate mapping

Coordinate grid baked into screenshots:
- Semi-transparent red lines every 200px (`Rgba([255,0,0,50])`)
- 3×5 bitmap font labels at intersections (`Rgba([255,0,0,160])`)
- Labels on top and left edges for axis reference
- Controlled by `CLICKY_GRID_OVERLAY=0` to disable

Screenshot labels:
- Single monitor: `"screen 1 of 1 (image dimensions: WxH pixels)"`
- Multi-monitor cursor screen: `"screen N of M (WxH pixels) — cursor is on this screen (primary focus)"`
- Multi-monitor other: `"screen N of M (WxH pixels) — secondary screen"`

Platform specifics:
- **Windows**: `SM_XVIRTUALSCREEN / SM_CXVIRTUALSCREEN` spans negative-origin monitors
- **Hyprland**: `hyprctl monitors -j` → `physical_w / scale = logical_w`
- **Sway**: `swaymsg -t get_outputs --raw` → rect + scale
- **GNOME**: `gsettings set org.gnome.desktop.interface enable-animations false` around xcap capture

---

## LLM System Prompt (INTENT_SYSTEM_PROMPT)

Key instructions the LLM receives:
- Respond conversationally for TTS: all lowercase, casual, one or two sentences by default
- No markdown, lists, emojis, or code verbatim
- Reference specific things visible on screen
- **Never emit coordinates** — only append a JSON intent block when pointing would help
- JSON block format at the very end of response (after spoken text):
  ```json
  {"label":"<short description>","action":"click","region_hint":"<region>","screen_number":<N or null>}
  ```
- action ∈ { "click", "focus", "hover" }
- region_hint: "top right", "center", "bottom left", "top left", "top center", "bottom right"
- If pointing wouldn't help, omit the JSON entirely

LLM context is **fully stateless per turn** — `history_exchanges` is always `Vec::new()`.

---

## Environment Variables

### Backend (`backend/.env`)

| Variable | Required | Default | Description |
|---|---|---|---|
| `DEEPGRAM_API_KEY` | Yes | — | Nova-3 STT + Aura-2 TTS |
| `AI_PROVIDER` | No | `openrouter` | `openrouter` or `gemini` |
| `OPENROUTER_API_KEY` | If OpenRouter | — | OpenRouter API key |
| `GEMINI_API_KEY` | If Gemini | — | Gemini API key |
| `LLM_MODEL` | No | provider default | Override LLM model name |
| `CU_MODEL` | No | `nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free` | Override CU model |
| `DEEPGRAM_TTS_VOICE` | No | `aura-2-thalia-en` | Deepgram Aura-2 voice ID |
| `LOG_LEVEL` | No | `info` | `debug`/`info`/`warn`/`error` |
| `PORT` | No | `9001` | WebSocket listen port |

### Frontend

| Variable | Description |
|---|---|
| `CLICKY_SIMULATE` | `1` to skip real pipelines; fake PipelineReady after 1.5s |
| `CLICKY_GRID_OVERLAY` | `0` to disable the coordinate grid on screenshots |
| `CLICKY_DEBUG_SCREENSHOTS` | `1` (debug builds) to save screenshots to temp dir |
| `RUST_LOG` | Frontend log level |

---

## Config File (Frontend)

TOML at:
- Windows: `%APPDATA%\tgomareli\clicky-desktop\config\config.toml`
- Linux: `~/.config/clicky-desktop/config.toml`

```toml
push_to_talk_hotkey = "CtrlSpace"   # or "CtrlGrave"
autostart_enabled = false
```

The render loop polls config mtime every 750ms and hot-reloads. If the hotkey changes, a new `HotkeyBackend` is created immediately.

---

## Platform Support

| Feature | Linux X11 | Hyprland/Sway | GNOME Wayland | Windows | macOS |
|---|---|---|---|---|---|
| Overlay | ✓ | ✓ | ✓ | ✓ | builds |
| Hotkey | global-hotkey | evdev | evdev | global-hotkey | global-hotkey |
| Cursor tracking | atomic fallback | evdev | evdev | Win32 GetCursorPos | atomic fallback |
| Screenshots | xcap | grim per-output | xcap + anim suppress | xcap (virtual desktop) | xcap |
| UIA tree | — | — | — | IUIAutomation COM | — |
| Autostart | XDG .desktop | XDG .desktop | XDG .desktop | HKCU Run key | — |

---

## Key Technical Invariants

### Tokio guard before ApiAdapter
`let _tokio_guard = tokio_rt.enter()` must be held before `ApiAdapter::try_connect()` — the winit event loop runs inside a tokio context and `tokio::spawn` / `spawn_blocking` require an active runtime context.

### `pollster::block_on` for wgpu init
wgpu init is async. `tokio::Handle::current().block_on` panics with nested block_on inside the winit+tokio context. `pollster::block_on` avoids this.

### `DwmFlush()` at frame start
Must be called **before** cursor sampling, not at the end. It gates the loop to vsync; adding `thread::sleep(frame_budget)` after it double-sleeps and halves the frame rate.

### No `WS_EX_NOREDIRECTIONBITMAP` with `WS_EX_LAYERED`
These conflict and break DWM compositing. The overlay requires only `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`.

### Vulkan required on Windows
DX12 only provides `Opaque` alpha mode for wgpu. Real per-pixel transparency requires `PreMultiplied` alpha, which is only exposed on the Vulkan backend.

### reqwest::Client shared, never per-request
`Arc<reqwest::Client>` is created once at server startup and cloned into connections. Creating a new `Client` per connection wastes sockets and disables HTTP keep-alive pooling.

### WebSocketConfig non-exhaustive
Use `WebSocketConfig::default()` then set fields — struct literal construction panics because the type is `#[non_exhaustive]`.

### UIA extraction from spawn_blocking
COM `IUIAutomation::FindAll(TreeScope_Subtree)` takes 50–200ms. Must run on a blocking thread; calling it from async would stall the tokio executor.

### is_animating includes Processing state
`OverlayApp::is_animating()` must return `true` during `Processing` state so the spinning arc doesn't freeze and cursor position updates continue while the mic is not active.

### cu_coordinate_pending guard
`cu_coordinate_pending` flag prevents double-firing a `RequestCUCoordinate` message. It is set when the request is sent and cleared when `CUCoordinateReady` arrives or on any state reset.

### last_display_infos race condition fix
`display_infos` from `PipelineReadyWithIntent` must be stored on `OverlayApp.last_display_infos` so `CUCoordinateReady` can access them. Relying solely on the event's `display_infos` causes a race condition when the screenshots were cleared between events.

### Screenshot pipeline label convention
The label `"cursor is on this screen (primary focus)"` in `ScreenshotData.label` is used by the LLM system prompt instruction to prioritize the cursor's display when processing the intent.

---

## Debug Artifacts

| File | Producer | Contents |
|---|---|---|
| `logs.txt` | `src/pipeline_log.rs` | Full pipeline trace: SCREEN, UIA, VISION, PIPELINE, SNAP, VALIDATE, EXEC, DECISION, UIA_MATCH |
| `backend/clicky_llm_log.txt` | `backend/handlers/llm_pipeline.rs` | LLM transcript + response + parsed intent per turn |
| `test_ss/cu_{ts}_pass{p}_attempt{a}.jpg` | `backend/api/computer_use.rs` | Screenshot sent to CU model |
| `test_ss/cu_{ts}_pass{p}_attempt{a}.txt` | `backend/api/computer_use.rs` | CU prompt + raw model response |
| `%TEMP%/clicky_debug_screenshot_N.jpg` | `backend/handlers/llm_pipeline.rs` | LLM screenshots (debug build, CLICKY_DEBUG_SCREENSHOTS=1) |

---

## Current Status

| Phase | Status | What was built |
|---|---|---|
| Phase 1 | Done | App shell, platform detection, tray, hotkey, overlay basics, cursor tracking |
| Phase 2 | Done | Mic capture, PCM16 conversion, Deepgram Nova-3 STT, waveform, hotkey-to-transcript pipeline |
| Phase 3 | Done | Screenshots with coordinate grid, vision API, intent JSON parsing, cursor flight, CU coordinate detection |
| Phase 4 | Done | Deepgram Aura-2 TTS, rodio MP3 playback, lifecycle integration |
| Phase 5 | Done | Cursor animation polish, return flight, speech bubble, wgpu/tiny-skia/cosmic-text renderer |
| Phase 5.5 | Done | Frontend↔Backend split, WebSocket protocol, clicky-types crate, ApiAdapter with auto-reconnect |
| Phase 5.6 | Done | UIA tree extraction (Windows), Sobel+union-find UI map, ScreenModel, hybrid 3-layer coordinate pipeline (UIA→vision→CU), structured intent JSON, strict coord types, enhanced_snap, pipeline_log |
| Phase 6 | In progress | Settings UI, config persistence, autostart, CI, release prep |
