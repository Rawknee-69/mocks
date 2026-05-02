# Project Agents

This file is the persistent Codex context for `clicky-desktop`. Keep it focused on stable architecture, conventions, and workflow notes that are useful every turn.

## Primary References

- Read [CLAUDE.md](/Users/tgomareli/Development/clicky-desktop/CLAUDE.md) when deeper product context or implementation phase notes are needed.
- Rust-focused assistance should use the installed skills under `~/.codex/skills`.
- Preferred Rust skills for this repo:
  - `rust-router` for general Rust questions and routing
  - `rust-learner` for Rust/crate version lookups
  - `coding-guidelines` for style and conventions
  - `unsafe-checker` for unsafe code and FFI review
  - `m01-ownership` through `m15-anti-pattern` for focused Rust problem-solving

## Product Overview

`clicky-desktop` is a system tray desktop app with push-to-talk voice input.

High-level flow:

1. User holds a global hotkey.
2. Microphone audio is captured and streamed to AssemblyAI for transcription.
3. On release, the app captures screenshots of all monitors.
4. Transcript plus screenshots are sent to Claude.
5. Claude Computer Use can return precise screen coordinates.
6. Response audio is synthesized with ElevenLabs, with `espeak-ng` fallback.
7. A blue triangle overlay animates to the detected UI target and returns to the cursor.

## Tech Stack

- Rust
- `winit 0.30` for the native window (Win32/X11/Wayland), transparent + undecorated
- `wgpu 22` for GPU presentation (Vulkan/DX12/Metal/GL backends)
- `tiny-skia 0.11` for CPU rasterisation of triangles, arcs, bezier paths (uploaded as wgpu texture each frame)
- `cosmic-text 0.12` for font shaping and text rendering
- `tokio` for async runtime
- `reqwest` and `tokio-tungstenite` for HTTP and WebSocket APIs
- `cpal` for microphone capture
- `rodio` for MP3 playback
- `xcap` and `grim` for screenshots
- `tray-icon` for the system tray
- `global-hotkey` and `evdev` for push-to-talk hotkeys

## Code Layout

- `src/main.rs`
  - winit event loop wiring together hotkeys, async tasks, audio, overlay, and state transitions.
- `src/app/`
  - Platform detection and the `VoiceState` state machine.
- `src/core/`
  - Portable logic: bezier flight, POINT tag parsing, bubble text, RMS tracking, PCM16 conversion, coordinate mapping, conversation history, design tokens.
- `src/api/`
  - Claude SSE, Claude Computer Use, ElevenLabs, and AssemblyAI integrations.
- `src/overlay/`
  - wgpu + winit transparent overlay: `window.rs` (winit `OverlayApp`), `renderer.rs` (animation state), `paint.rs` (tiny-skia draw calls), `gpu.rs` (wgpu device/surface/pipeline init).
- `src/platform/`
  - Per-OS window setup: `windows.rs` (DWM glass, `WS_EX_LAYERED|TRANSPARENT`, `DwmExtendFrameIntoClientArea`), `linux_x11.rs`, `linux_wayland.rs`.
- `src/tray/`
  - Tray icon and menu integration.
- `src/panel/`
  - egui setup and settings window.
- `src/hotkey/`
  - Global push-to-talk implementations.
- `src/audio/`
  - Microphone capture and MP3 playback.
- `src/screenshot/`
  - Multi-monitor capture.
- `src/cursor_tracker/`
  - Global cursor tracking across Wayland, X11, and Windows.

## Core Runtime Patterns

- Voice state machine:
  - `VoiceState` transitions are `Idle -> Listening -> Processing -> Responding -> Idle`.
- Async bridge:
  - Tokio tasks communicate back to the synchronous render loop through `std::sync::mpsc::channel<UiEvent>`.
- LLM pipeline:
  - transcript + screenshots -> vision model -> parse POINT tags -> optional Computer Use coordinate detection -> UI event back to render loop.
- TTS pipeline:
  - spoken response -> ElevenLabs synthesis -> `UiEvent::TtsAudio` -> `AudioPlayer::play_mp3()`.
- Computer Use:
  - `api::computer_use::detect_element_location()` is used for precise coordinates and is preferred over POINT tags when available.
- Cursor animation:
  - `FollowingMouse -> NavigatingToTarget -> PointingAtTarget -> ReturningToMouse -> FollowingMouse`.

## Platform Notes

- Wayland cursor tracking uses `evdev` because winit mouse tracking does not work reliably with click-through passthrough windows.
- X11 uses a fallback cursor tracker based on atomics (`RaylibFallbackTracker` — legacy name, no Raylib dependency).
- Screenshot capture differs by compositor and display server.
- Windows overlay renders via `UpdateLayeredWindow` (not wgpu surface present directly); wgpu drives the pipeline, output pixels are RGBA→BGRA copied to the layered window bitmap.
- On Windows, only the Vulkan backend exposes `PreMultiplied` alpha mode; DX12 only gives `Opaque`. The overlay requires Vulkan for real per-pixel transparency.
- `DwmFlush()` must be called at the **start** of each frame (before cursor sample), not at the end — it gates the loop to vsync. Adding `thread::sleep(frame_budget)` after `DwmFlush` double-sleeps and halves the frame rate.
- Debug builds with tiny-skia render at ~54ms/frame on a 1920×1080 overlay without optimizations. Fixed by `[profile.dev] opt-level=1` + `[profile.dev.package."*"] opt-level=2` in Cargo.toml (already present in the repo).

## Config

Environment variables used by the app:

- `ASSEMBLYAI_API_KEY`
- `ANTHROPIC_API_KEY`
- `ELEVENLABS_API_KEY`
- `ELEVENLABS_VOICE_ID`

`dotenvy` loads `.env` automatically in `cargo run`.

## Build And Run

```bash
cargo build
cargo run
```

## Rust Defaults

When creating new Rust crates or editing package metadata, prefer:

```toml
[package]
edition = "2024"
rust-version = "1.85"

[lints.rust]
unsafe_code = "warn"

[lints.clippy]
all = "warn"
pedantic = "warn"
```

Note: the current repo `Cargo.toml` may still be on older settings. Follow the repo’s current state unless the task includes modernizing it.

## Project Conventions

- Keep code simple.
- Simplicity is more important than cleverness.
- If something is uncertain and the risk is meaningful, stop and ask.
- Use clear variable names, no unnecessary abbreviations.
- Comments should explain why, not what.
- Do not add features beyond the request.
- Do not fix warnings unless asked.
- Read files before editing.
- Treat `UiEvent` as the async-to-render-loop boundary.

## Current Product Status

- Phase 1 complete: app shell, platform detection, tray, hotkey, overlay basics, cursor tracking.
- Phase 2 complete: mic capture, PCM conversion, AssemblyAI streaming, waveform, hotkey-to-transcript pipeline.
- Phase 3 complete: screenshots, vision APIs, POINT parsing, coordinate mapping, conversation history, cursor flight, Computer Use.
- Phase 4 complete: ElevenLabs TTS, playback, fallback behavior, lifecycle integration.
- Phase 5 complete: cursor animation polish, return flight, bubble styling, font, loading animation.
- Phase 6 in progress: packaging, release docs, and CI.

## Workspace Structure (Cargo Workspace)

This is a **Cargo workspace** with three crates at the repo root:

- `.` (`clicky-desktop`) — the frontend: Raylib overlay, tray, hotkey, audio capture, egui settings, screenshot capture.
- `backend/` (`clicky-backend`) — the backend pipeline server: AssemblyAI, Claude vision, Claude Computer Use, ElevenLabs TTS, error handling, structured logging.
- `clicky-types/` — shared WebSocket message types (`ClientMessage`, `ServerMessage`, `ScreenshotData`, `DisplayInfo`, `ErrorCode`, `PointingInstruction`) used by both crates.

Root `Cargo.toml` declares `members = [".", "backend", "clicky-types"]`.

## Frontend ↔ Backend Communication

- Protocol: **WebSocket on `ws://127.0.0.1:9001`** (port constant `BACKEND_PORT = 9001` in `src/api/adapter.rs`).
- Frontend sends `ClientMessage` JSON frames; backend replies with `ServerMessage` JSON frames.
- `src/api/adapter.rs` — `ApiAdapter` struct: non-blocking bridge between the 60fps Raylib render loop and the backend WS task. Auto-reconnects with exponential back-off.
- Frontend captures screenshots and sends them to the backend via `ClientMessage::Screenshots`; the backend owns all API calls (AssemblyAI, Claude, ElevenLabs).
- All API keys and environment variables live **only in the backend** `.env`; the frontend never reads `.env`.

## Build And Run (Updated)

Run both processes; each in its own terminal:

```bash
# Terminal 1 – backend pipeline server
cargo run -p clicky-backend

# Terminal 2 – frontend desktop overlay
cargo run -p clicky-desktop
```

Backend `.env` (at `backend/.env`): `ASSEMBLYAI_API_KEY`, `ANTHROPIC_API_KEY`, `ELEVENLABS_API_KEY`, `ELEVENLABS_VOICE_ID`, `LOG_LEVEL=debug`, `PORT=9001`.

## Learned User Preferences

- User prefers the backend to own all API keys and env vars; the frontend must never read `.env` or API keys.
- User requires full type safety — no type casting allowed anywhere in the codebase.
- User wants the backend to only process data; screenshots must be captured by the frontend and sent to the backend, not captured server-side.
- When presenting architectural options, explain first and ask which approach to implement before building.

## Learned Workspace Facts

- `clicky-types` crate is the single source of truth for all shared WS message types; never duplicate these types in frontend or backend.
- `backend/src/error.rs` and `backend/src/logger.rs` implement centralised error handling and structured logging (log level controlled via `LOG_LEVEL` env var; off in prod).
- `src/api/adapter.rs` is the only file in the frontend that knows about the WebSocket connection; all other frontend code communicates through `UiEvent` as before.
- Windows build requires CMake (`winget install Kitware.CMake`) and LLVM (`winget install LLVM.LLVM`) with `LIBCLANG_PATH=C:\Program Files\LLVM\bin` set; `bindgen` needs these at build time.
- The Tokio render-loop pattern: call `let _tokio_guard = tokio_rt.enter()` before `ApiAdapter::connect` so all `tokio::spawn` / `spawn_blocking` calls in the synchronous event loop work without "no reactor running" panics.
- Windows transparent overlay correctness: keep `WS_EX_LAYERED` (required for `UpdateLayeredWindow`), add `WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE` for click-through, call `DwmExtendFrameIntoClientArea` with all-(-1) MARGINS for DWM "sheet of glass" compositing. Do NOT add `WS_EX_NOREDIRECTIONBITMAP` alongside `WS_EX_LAYERED` — they conflict and break DWM compositing.
- `pollster::block_on` must be used (not `.await` or `tokio::Handle::current().block_on`) for async wgpu init calls inside the winit event loop, since the event loop runs inside a tokio context and nesting `block_on` panics.
