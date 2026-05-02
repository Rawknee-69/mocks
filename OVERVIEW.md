# Clicky Desktop — Full Technical Overview

Clicky Desktop is a system tray desktop companion for Linux and Windows. The user holds a hotkey to speak; the app transcribes their voice, captures their screen(s), asks Claude to understand the request, flies a blue triangle cursor to the relevant UI element, and speaks the answer aloud. Everything runs locally except the three cloud API calls (AssemblyAI, Anthropic, ElevenLabs).

---

## How It Works End to End

```
[User holds hotkey]
       │
       ▼
[Mic capture starts — cpal]
       │  PCM16 audio chunks
       ▼
[AssemblyAI WebSocket — streaming transcription]
       │  partial + final transcripts
       ▼
[User releases hotkey]
       │
       ▼
[Screenshot all monitors — xcap / grim / GNOME portal]
       │  JPEG images + display geometry
       ▼
[Claude Vision API — SSE streaming] ─────────────────┐
       │  spoken text + optional POINT tag            │
       ▼                                              │
[Claude Computer Use API]          [ElevenLabs TTS]   │
       │  precise screen coordinate    │              │
       ▼                              ▼               │
[UiEvent::LlmResponse]    [UiEvent::TtsAudio]        │
       │                              │               │
       └──────────────────────────────┘               │
                        │                             │
                        ▼                             │
           [60fps Raylib render loop] ◄───────────────┘
                        │
         ┌──────────────┼──────────────┐
         ▼              ▼              ▼
  [Blue triangle  [Bezier flight  [Speech bubble
   cursor tracks   to target       with phrase
   mouse pos]      element]        + TTS audio]
```

---

## Architecture

The app is structured as a single Rust binary. The main thread runs a 60fps Raylib render loop that owns all display state. Tokio async tasks run on a background runtime; they communicate back to the render loop via a `std::sync::mpsc` channel carrying `UiEvent` values.

```
src/
├── main.rs                 ← 60fps render loop, main event dispatch
├── app/
│   ├── platform.rs         ← OS / display server / compositor detection
│   └── state_machine.rs    ← VoiceState enum + transition rules
├── api/
│   ├── assemblyai.rs       ← WebSocket streaming transcription
│   ├── claude.rs           ← SSE streaming vision API
│   ├── computer_use.rs     ← Computer Use API for precise coordinates
│   └── elevenlabs.rs       ← TTS synthesis → MP3 bytes
├── audio/
│   ├── capture.rs          ← cpal mic capture → PCM chunks
│   ├── playback.rs         ← rodio MP3 playback
│   └── mod.rs              ← UiEvent + AudioError types
├── autostart/
│   └── mod.rs              ← Login autostart (Linux .desktop / Windows registry)
├── config/
│   └── mod.rs              ← AppConfig (TOML + env override)
├── core/
│   ├── audio_rms.rs        ← RMS power level tracking for waveform
│   ├── bezier_flight.rs    ← Quadratic bezier cursor flight math
│   ├── bubble_text.rs      ← Curated speech bubble phrases
│   ├── conversation.rs     ← Bounded conversation history (last 10 exchanges)
│   ├── coordinate_mapper.rs← Screenshot pixels → global display coordinates
│   ├── design_system.rs    ← Color/size/animation tokens
│   ├── pcm16_converter.rs  ← Float32 audio → PCM16 16kHz mono
│   └── point_parser.rs     ← Parse Claude's [POINT:x,y:label] tags
├── cursor_tracker/
│   ├── evdev_tracker.rs    ← Wayland cursor position via evdev
│   ├── fallback.rs         ← Raylib mouse position fallback
│   └── win32_tracker.rs    ← Windows GetCursorPos
├── hotkey/
│   ├── evdev_hotkey.rs     ← Wayland hotkey via evdev /dev/input/
│   ├── global_hotkey_backend.rs ← X11/Windows via global-hotkey crate
│   └── mod.rs              ← Platform routing
├── overlay/
│   ├── mod.rs              ← (re-exports)
│   └── renderer.rs         ← All Raylib drawing: triangle, waveform, arc, bubble
├── panel/
│   └── mod.rs              ← egui settings window (Linux/macOS) / Win32 fallback
├── runtime/
│   └── mod.rs              ← Async pipeline: transcription bridge + LLM pipeline
├── screenshot/
│   └── mod.rs              ← Multi-monitor capture, coordinate grid overlay
└── tray/
    └── mod.rs              ← System tray icon + Quit / Settings / Toggle menu
```

---

## The Voice State Machine

`VoiceState` is the central driver of all UI and pipeline behavior.

```
Idle ──[hotkey press]──► Listening ──[hotkey release]──► Processing ──[response ready]──► Responding ──[response complete]──► Idle
                              ▲                                 │
                              └─────────[hotkey press]──────────┘  (cancel and restart)

Any state ──[error]──► Idle
```

| State | What the overlay shows | What is active |
|---|---|---|
| `Idle` | Blue triangle cursor tracking mouse | Nothing |
| `Listening` | Waveform bars at cursor | Mic capture, AssemblyAI WebSocket |
| `Processing` | Rotating arc loader at cursor | Waiting for transcript + Claude + Computer Use |
| `Responding` | Triangle flying to target + speech bubble | TTS playback, bezier animation |

Transitions are applied with `VoiceState::apply(VoiceStateTransition)` which returns `Option<VoiceState>`. Invalid transitions return `None` (silently ignored).

---

## Platform Support

### Operating Systems

| Feature | Linux X11 | Linux Wayland (Hyprland/Sway) | Linux Wayland (GNOME) | Windows | macOS |
|---|---|---|---|---|---|
| Overlay window | ✓ | ✓ | ✓ | ✓ | builds |
| Hotkey | global-hotkey | evdev | evdev | global-hotkey | global-hotkey |
| Cursor tracking | Raylib fallback | evdev | evdev | Win32 GetCursorPos | Raylib |
| Screenshots | xcap | grim | xcap + animation suppress | xcap | xcap |
| Autostart | XDG .desktop | XDG .desktop | XDG .desktop | Registry Run key | not implemented |
| Settings UI | egui | egui | egui | Win32 MessageBox fallback | egui |

### Platform Detection

At startup, `app::platform::detect()` reads environment variables to identify the display server and compositor:

- `XDG_SESSION_TYPE` → `x11` or `wayland`
- `HYPRLAND_INSTANCE_SIGNATURE` → Hyprland
- `SWAYSOCK` → Sway
- Fallback to `WAYLAND_DISPLAY` / `DISPLAY` checks

### Windows Overlay

The overlay window uses Win32 extended styles: `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`. This makes it fully click-through and prevents it from appearing in the taskbar. DPI awareness is set to Per-Monitor V2 at startup so the overlay coordinates match screenshot pixels exactly.

The overlay spans the full virtual desktop (from `GetSystemMetrics(SM_XVIRTUALSCREEN)`) to cover multi-monitor layouts with negative coordinates (monitors to the left of primary).

---

## Features

### 1. Push-to-Talk Voice Input

Hold the configured hotkey (`Ctrl+Space` by default, or `Ctrl+\``) to start recording. Release to send the transcript to Claude. Pressing the hotkey again while processing cancels the current session and starts a new one.

### 2. Real-Time Transcription (AssemblyAI)

Audio pipeline:
1. `cpal` captures mic input at the device's native sample rate
2. `core::pcm16_converter` resamples to 16kHz mono PCM16 (via `rubato`)
3. Chunks are buffered to at least 3200 bytes before sending
4. An AssemblyAI WebSocket session receives turn-based transcript updates
5. Partial transcripts update the internal state; the final transcript triggers the LLM pipeline
6. On hotkey release the audio channel drains fully before `ForceEndpoint` is sent

### 3. Screen Capture

On hotkey release, `screenshot::capture_all_screens()` captures all monitors. Each screenshot:
- Is resized to max 1280px wide (Lanczos3) to reduce API payload
- Has a subtle red coordinate grid baked in at every 200px intersection with labels — this helps Claude estimate coordinates
- Is JPEG-encoded at quality 80
- Gets a human-readable label: `"screen 1 of 2 (image dimensions: 1280x720 pixels) — cursor is on this screen (primary focus)"`

### 4. Claude Vision API (SSE Streaming)

`api::claude::stream_claude_response()` sends the transcript + all screenshots to `claude-sonnet-4-6` via the Anthropic Messages API with `"stream": true`. The SSE stream is parsed for `content_block_delta` events to extract text progressively.

The system prompt instructs Claude to:
- Respond in conversational spoken-word style (for TTS)
- Reference things visible on screen
- Append a `[POINT:x,y:label]` coordinate tag when pointing would help
- End with something interesting to seed follow-up questions

### 5. Element Coordinate Detection (Computer Use API)

After the vision response, `api::computer_use::detect_element_location()` makes a second API call using Claude's `computer_20251124` tool. This is significantly more accurate than parsing POINT tag coordinates from the text response.

The screenshot is resized to the Anthropic-recommended Computer Use resolution that best matches the display's aspect ratio:

| Display aspect ratio | Used resolution |
|---|---|
| ~4:3 (legacy displays) | 1024×768 |
| ~16:10 (most laptops) | 1280×800 |
| ~16:9 (external monitors) | 1366×768 |

The returned pixel coordinate is scaled back to display logical points and converted to global screen coordinates. Computer Use coordinates take priority over POINT tag coordinates.

### 6. Bezier Cursor Flight Animation

When a target coordinate is available, the blue cursor triangle detaches from the mouse and flies to the target along a smooth quadratic bezier arc.

**Forward flight** (to target):
- Speed: 600 px/s, duration clamped 0.8–1.8 seconds
- Arc height: 25% of distance, max 100px
- Scale pulse: ±15% at midpoint
- Easing: smoothstep (accelerate then decelerate)

**Return flight** (back to mouse):
- Speed: 900 px/s, duration clamped 0.4–1.0 seconds
- Arc height: 15% of distance, max 50px
- Scale pulse: ±8% at midpoint

After landing at the target, the cursor holds for 2 seconds after TTS finishes, then returns. The return flight is a direct linear chase (not a bezier arc) at 900 px/s.

Navigation mode cycle: `FollowingMouse → NavigatingToTarget → PointingAtTarget → ReturningToMouse → FollowingMouse`

### 7. Text-to-Speech (ElevenLabs)

`api::elevenlabs::synthesize_speech()` sends the spoken text to ElevenLabs `eleven_multilingual_v2` model at MP3 44100 Hz / 128kbps. TTS fires in parallel with the Computer Use API call so both complete at roughly the same time.

The MP3 bytes arrive as `UiEvent::TtsAudio`, which the render loop passes to `AudioPlayer::play_mp3()` (via `rodio`). If ElevenLabs fails, `espeak-ng` is spawned as a subprocess fallback.

Pressing the hotkey while TTS is playing stops playback immediately.

### 8. Speech Bubble

While the cursor is navigating to or pointing at a target, a glass-effect speech bubble appears near the cursor tip. Features:
- Fade-in opacity animation
- Typewriter text reveal (~30 chars/second)
- Rise-on-appear animation (slides up 8px as it fades in)
- Dark blue glass background (`#3380FF` at 60% blend + offsets)
- System font (Segoe UI on Windows, AdwaitaSans/Ubuntu/DejaVu on Linux, SF on macOS)
- Auto word-wrapping at 28% of screen width (180–320px)
- Screen-edge clamping so the bubble never goes off-screen
- Short curated phrase (`"right here!"`, `"found it!"`, etc.) — the full response is in TTS audio

### 9. Waveform Visualization

During `Listening` state, 20 vertical bars are drawn at the cursor position. Bar height is proportional to the RMS power level of the most recent audio samples. The `core::audio_rms::AudioPowerLevelTracker` maintains a rolling history of 44 RMS values.

### 10. Loading Arc

During `Processing` state, a rotating partial arc (240°) with a fading tail and a leading dot is drawn at the cursor position. A subtle pulsing center dot adds depth. The arc sweeps at 3 radians/second.

### 11. Conversation History

`core::conversation::ConversationHistory` stores the last 10 user–assistant exchange pairs in a `VecDeque`. Each new exchange appends; when the queue hits 10, the oldest is dropped. These exchanges are prepended to every Claude API request as prior `user`/`assistant` messages so Claude maintains context across the session.

### 12. Settings and Config

Config is persisted as TOML at the platform config directory (e.g., `~/.config/clicky-desktop/config.toml` on Linux, `%APPDATA%\clicky-desktop\config.toml` on Windows).

Fields:
- `assemblyai_api_key`
- `anthropic_api_key`
- `elevenlabs_api_key`
- `elevenlabs_voice_id` (optional, falls back to a default voice)
- `push_to_talk_hotkey` — `CtrlSpace` or `CtrlGrave`
- `autostart_enabled`

Environment variables override the config file. The render loop polls the config file's modification time every 750ms and hot-reloads changes (including re-creating the hotkey manager if the hotkey changed).

If any required key is missing at startup, the onboarding window opens before Raylib initializes (since both need the main thread on some platforms).

### 13. Settings Window (egui)

On Linux/macOS the settings window is a native `eframe`/`egui` window that can be opened from the tray menu. It shows a setup checklist with the status of each API key, password fields for all credentials, a hotkey dropdown, and an autostart checkbox.

On Windows, egui is disabled due to rendering conflicts with the Raylib overlay. A Win32 `MessageBoxW` shows the config file path instead.

### 14. Autostart

`autostart::set_enabled(bool)`:
- **Linux**: Creates/removes `~/.config/autostart/clicky-desktop.desktop`
- **Windows**: Creates/removes `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\ClickyDesktop`

### 15. System Tray

The tray icon uses the `tray-icon` crate. Menu items:
- **Quit** — breaks the main render loop and exits
- **Open Settings** — opens the egui settings window (or Win32 dialog)
- **Toggle Overlay** — logged but not yet fully implemented

### 16. Simulation Mode

Set `CLICKY_SIMULATE=1` in the environment to skip real API calls. On hotkey release, a fake `LlmResponse` is sent after 1.5 seconds with a random target coordinate within the center 60% of the screen. Useful for testing the overlay animation without API keys.

---

## Data Flow Detail

### Voice → Transcript

```
cpal mic callback
  → AudioChunk { samples: Vec<f32>, sample_rate, channels }
  → std_mpsc channel to audio_rx
  → tokio::spawn_blocking (audio_rx consumer)
      → pcm16_converter: resample to 16kHz mono, convert to i16 bytes
      → buffer until ≥ 3200 bytes
      → tokio_mpsc channel to WebSocket sender
  → AssemblyAI WebSocket (binary PCM16 frames)
  → AssemblyAI → JSON transcript updates
  → TranscriptUpdate::{Partial, Final, Error}
  → UiEvent::{PartialTranscript, FinalTranscript, TranscriptionError}
  → std_mpsc to render loop
```

### Transcript → LLM Response

```
FinalTranscript received by render loop
  → tokio_rt.spawn(run_llm_pipeline)
      → capture_all_screens()
          → per platform: grim / xcap / GNOME no-flash
          → scale to ≤1280px, draw coordinate grid, JPEG encode
      → build_vision_message_content(transcript, screenshots)
      → stream_claude_response() — SSE stream
      → parse_claude_response() → spoken_text + optional POINT tag
      ├── if TTS enabled: tokio::spawn(synthesize_speech)
      │       → ElevenLabs API → MP3 bytes → UiEvent::TtsAudio
      └── detect_computer_use_coordinate()
              → best_computer_use_resolution() (aspect ratio match)
              → resize screenshot to CU resolution (Lanczos3)
              → call_computer_use_api() with computer_20251124 tool
              → parse_coordinate_from_response()
              → scale CU pixels → display logical points
              → + display global origin → global screen coordinate
  → UiEvent::LlmResponse { spoken_text, pointing_instruction, display_infos, computer_use_global_coordinate }
```

### LLM Response → Overlay

```
UiEvent::LlmResponse received by render loop
  → voice_state → Responding
  → if computer_use_global_coordinate:
        render_state.start_flight_to(global_x - overlay_x, global_y - overlay_y, bubble_text)
     elif pointing_instruction:
        coordinate_mapper.find_target_display()
        map_screenshot_pixels_to_global_display_coordinates()
        render_state.start_flight_to(coord.x - overlay_x, ...)
  → render_state.speech_bubble_text = pick_bubble_phrase(is_pointing)

UiEvent::TtsAudio received
  → audio_player.play_mp3(bytes)

Each frame (60fps):
  → advance_flight_animation(delta_seconds) — bezier evaluation
  → update speech bubble opacity + typewriter progress
  → draw_overlay_frame() → triangle / waveform / arc / bubble

After TTS finishes + 2 second hold:
  → start_return_flight(mouse_x, mouse_y)

After cursor reaches mouse (≤6px):
  → navigation_mode = FollowingMouse
  → voice_state → Idle
```

---

## Key Technical Details

### Transparent Click-Through Overlay

Raylib is initialized with `FLAG_WINDOW_TOPMOST | FLAG_WINDOW_MOUSE_PASSTHROUGH`. On Windows, Win32 extended styles `WS_EX_TRANSPARENT | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE` are applied after init. On Linux X11, `_NET_WM_STATE_SKIP_TASKBAR`, `_NET_WM_STATE_SKIP_PAGER`, and `_NET_WM_STATE_ABOVE` are set via x11rb to keep it off the taskbar and on top.

### Coordinate Spaces

There are four coordinate spaces in play:

1. **Global screen space** — origin at primary monitor top-left, extends to all monitors
2. **Overlay local space** — origin at the overlay window's top-left (= virtual desktop origin on Windows)
3. **Display logical points** — per-monitor coordinate space used by the OS window manager
4. **Screenshot pixels** — the JPEG image's pixel coordinates (after downscaling to ≤1280px)

`core::coordinate_mapper::map_screenshot_pixels_to_global_display_coordinates()` converts screenshot pixel coordinates from Claude into global screen coordinates by:
1. Finding which display the target is on (by screen number from the POINT tag)
2. Scaling: `global_x = display_origin_x + (screenshot_x / screenshot_width) * display_logical_width`

Computer Use coordinates arrive pre-scaled to display logical points and just need the display origin added.

### Audio Resampling

`cpal` captures at the device's native sample rate (often 44100 or 48000 Hz, any channel count). `core::pcm16_converter` uses `rubato`'s `FftFixedOut` resampler to convert to 16000 Hz mono, then casts to i16 PCM16 for AssemblyAI.

### AssemblyAI v3 Protocol

The WebSocket handshake includes `?sample_rate=16000&encoding=pcm_s16le&speech_model=u3-rt-pro`. Audio is streamed as binary WebSocket messages. Transcripts arrive as JSON with a `type` field. On hotkey release:
1. The audio sender channel is drained (remaining buffered audio is flushed)
2. A `{"type": "ForceEndpoint"}` message is sent to force transcript finalization
3. The session waits up to 1500ms for the final transcript before timing out

### Config Hot-Reload

Every 750ms the render loop checks `fs::metadata(config_path)?.modified()`. If the modification time has changed it reloads `AppConfig::load()` and applies changes. If the hotkey changed, a new hotkey manager is created and the old one is dropped. This allows editing the config file in a text editor with live effect.

---

## Configuration

### Environment Variables (highest priority)

| Variable | Purpose |
|---|---|
| `ASSEMBLYAI_API_KEY` | AssemblyAI real-time transcription |
| `ANTHROPIC_API_KEY` | Claude vision + Computer Use |
| `ELEVENLABS_API_KEY` | ElevenLabs TTS |
| `ELEVENLABS_VOICE_ID` | Specific voice (optional) |
| `CLICKY_SIMULATE` | `1` to skip API calls and test UI only |
| `CLICKY_FORCE_SETUP_WINDOW` | `1` to force onboarding on startup (debug only) |
| `CLICKY_GRID_OVERLAY` | `0` to disable the coordinate grid on screenshots |
| `RUST_LOG` | Log level (e.g., `info`, `debug`) |

### Config File

Stored at:
- Linux: `~/.config/clicky-desktop/config.toml`
- Windows: `%APPDATA%\clicky-desktop\config.toml`

```toml
assemblyai_api_key = "..."
anthropic_api_key = "..."
elevenlabs_api_key = "..."
elevenlabs_voice_id = "..."  # optional
push_to_talk_hotkey = "CtrlSpace"  # or "CtrlGrave"
autostart_enabled = false
```

Environment variables override config file values. If a variable is set to an empty string it is ignored (the config file value wins).

---

## Build and Run

### Prerequisites

- Rust toolchain (stable)
- **Linux**: `libx11-dev`, `libxrandr-dev`, `libxi-dev`, `libgl-dev`, `libasound2-dev`, `libgtk-3-dev`, `libglib2.0-dev` (and `grim` for Hyprland/Sway screenshots)
- **Windows**: MSVC build tools

### Commands

```bash
# Development run (loads .env automatically)
cargo run

# Release build
cargo build --release

# Run tests
cargo test
```

### Debug Features

- Set `CLICKY_SIMULATE=1` to test the UI without API calls
- In debug builds, JPEG screenshots are saved to `/tmp/clicky_debug_screenshot_N.jpg` (or `%TEMP%\` on Windows) after each pipeline run
- Set `RUST_LOG=debug` for verbose API call tracing

---

## Dependencies

| Crate | Purpose |
|---|---|
| `tokio` | Async runtime for API calls |
| `reqwest` | HTTP client (Claude, ElevenLabs, AssemblyAI token) |
| `reqwest-eventsource` | SSE stream parsing for Claude |
| `tokio-tungstenite` | WebSocket for AssemblyAI streaming |
| `cpal` | Cross-platform mic capture |
| `rodio` | MP3 playback |
| `rubato` | Audio resampling (float32 → PCM16 at 16kHz) |
| `raylib` | 60fps transparent overlay rendering |
| `eframe` / `egui` | Settings window (Linux/macOS only) |
| `tray-icon` | System tray icon + menu |
| `global-hotkey` | X11 / Windows push-to-talk hotkey |
| `evdev` | Wayland hotkey + cursor tracking (Linux only) |
| `xcap` | Cross-platform screenshot capture |
| `x11rb` | X11 window hints for overlay (Linux X11 only) |
| `image` | Image resizing, JPEG encode, coordinate grid |
| `serde` / `serde_json` | JSON API payloads |
| `toml` | Config file serialization |
| `directories` | Platform config/data directories |
| `base64` | Screenshot encoding for API payloads |
| `regex` | POINT tag parsing |
| `dotenvy` | `.env` file loading |
| `log` / `env_logger` | Structured logging |
| `windows-sys` | Win32 overlay + DPI APIs (Windows only) |
| `winreg` | Windows registry for autostart |
| `gtk` | GTK init for tray-icon menu (Linux only) |

---

## Current Status

| Phase | Status | What was built |
|---|---|---|
| Phase 1 | Done | App shell, platform detection, state machine, tray, hotkey, overlay basics, cursor tracking |
| Phase 2 | Done | Mic capture, PCM conversion, AssemblyAI streaming, waveform, hotkey-to-transcript pipeline |
| Phase 3 | Done | Screenshots, Claude vision, POINT parsing, coordinate mapping, conversation history, cursor flight, Computer Use |
| Phase 4 | Done | ElevenLabs TTS, playback, espeak-ng fallback, lifecycle integration |
| Phase 5 | Done | Cursor animation polish, return flight, bubble styling, loading arc, system font |
| Phase 6 | In progress | Settings UI (egui), config persistence, autostart, CI, release prep |

### Known Limitations (pre-0.1)

- **Windows settings UI**: egui is disabled; API keys must be set via `.env` or the config TOML file
- **Wayland overlay**: on compositors other than Hyprland/Sway, the overlay window may appear in the taskbar or dock (compositor-specific window type hints not supported through Raylib/GLFW)
- **macOS**: builds but is not a primary target; autostart is not implemented
- **Overlay in taskbar on GNOME Wayland**: the GLFW backend does not expose Wayland-native window type protocols to suppress dock presence
