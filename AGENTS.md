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
- User prefers cost-reduction and vendor unlock: replace proprietary paid APIs (ElevenLabs, AssemblyAI, Claude) with OpenRouter + Deepgram alternatives when asked to reduce costs.
- On Windows, use a temp file for git commit messages — PowerShell does not support heredoc syntax.
- When editing plan files, always merge/add new content — never overwrite the existing file. The user explicitly corrected this: "donot overwrite this just make sure that it is added in the plan".

## Learned Workspace Facts

- `clicky-types` crate is the single source of truth for all shared WS message types; never duplicate these types in frontend or backend.
- `backend/src/error.rs` and `backend/src/logger.rs` implement centralised error handling and structured logging (log level controlled via `LOG_LEVEL` env var; off in prod).
- `src/api/adapter.rs` is the only file in the frontend that knows about the WebSocket connection; all other frontend code communicates through `UiEvent` as before. `ApiAdapter::try_connect` is the real entry point used by `main.rs`; `connect` is unused dead code and carries `#[allow(dead_code)]`.
- Windows build requires CMake (`winget install Kitware.CMake`) and LLVM (`winget install LLVM.LLVM`) with `LIBCLANG_PATH=C:\Program Files\LLVM\bin` set; `bindgen` needs these at build time.
- The Tokio render-loop pattern: call `let _tokio_guard = tokio_rt.enter()` before `ApiAdapter::try_connect` so all `tokio::spawn` / `spawn_blocking` calls in the synchronous event loop work without "no reactor running" panics.
- Windows transparent overlay correctness: keep `WS_EX_LAYERED` (required for `UpdateLayeredWindow`), add `WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE` for click-through, call `DwmExtendFrameIntoClientArea` with all-(-1) MARGINS for DWM "sheet of glass" compositing. Do NOT add `WS_EX_NOREDIRECTIONBITMAP` alongside `WS_EX_LAYERED` — they conflict and break DWM compositing.
- `pollster::block_on` must be used (not `.await` or `tokio::Handle::current().block_on`) for async wgpu init calls inside the winit event loop, since the event loop runs inside a tokio context and nesting `block_on` panics.
- The backend supports both Deepgram (Nova-3 STT, Aura-2 TTS) and a switchable AI provider (OpenRouter or Gemini) controlled by `AI_PROVIDER` env var; `LLM_MODEL` and `CU_MODEL` override model names; `GEMINI_API_KEY` / `OPENROUTER_API_KEY` hold keys. The `Provider` enum (`OpenRouter`/`Gemini`) is constructed once in `server::run()`, wrapped in `Arc`, and cloned cheaply into connections — never reconstructed per-request.
- `reqwest::Client` must be created once and shared across requests; never call `Client::new()` inside a per-request handler.
- `WebSocketConfig` in tungstenite is non-exhaustive; use `WebSocketConfig::default()` and then set fields — struct literal construction panics.
- Computer Use uses a two-pass zoom strategy: Pass 1 resizes full screenshot to `MAX_LONG_EDGE=1920` for a coarse coordinate; Pass 2 uses `compute_model_dims_allow_upscale` (no `.min(1.0)` cap, always upscales the crop to MAX_LONG_EDGE for ~4× pixel density). Pass 2 failure falls back to Pass 1. `call_with_retry` uses a 3-state `ParseResult` enum (`Found`/`NoElement`/`ParseFailed`) so valid model nulls don't trigger retries.
- `is_animating` in `window.rs` must include `|| voice_state == Processing` and cursor position updates must run during `Processing` state — otherwise the spinning arc freezes and doesn't follow the mouse when the mic is not active.
- LLM models may hallucinate the exact screen centre `(screen_width/2, screen_height/2)` when unsure; treat a result matching the geometric screen centre as a detection failure and discard it.
- The backend pipeline emits `ServerMessage::PipelineReady { spoken_text, mp3_bytes, display_infos }` (no coordinate field); `PipelineReady.coordinate` is always `None` — coordinate resolution is fully client-side via the hybrid UI-map flow. LLM and CU ran in parallel via `tokio::join!`; TTS fires after LLM text is known.
- Deepgram `speech_final=true` must emit `TranscriptUpdate::Final` immediately and break the WebSocket loop — do NOT wait for the socket to close. Final-wait timeout is 4000 ms; audio drain timeout is 800 ms. Emitting Final early prevents the timeout from firing before Deepgram delivers the real transcript.
- Frontend `coordinate_mapper` module and `PointingInstruction` struct have been deleted; the backend owns all coordinate mapping and returns a ready-to-use global coordinate in `ServerMessage::PipelineReady`. The frontend must not re-implement this logic.
- LLM calls use plain non-streaming JSON: Gemini uses `generateContent` (not `streamGenerateContent?alt=sse`), OpenRouter uses `"stream":false`. SSE parsing is removed entirely since TTS needs the full text before synthesis and the POINT tag is always at the end.
- Hybrid UI-map coordinate detection: `src/ui_map/mod.rs` implements `build_ui_map` (Sobel + union-find), `match_intent` (confidence gate 0.65), `snap_to_nearest_ui_element` (radius=90px). `ElementIntent` lives in `clicky-types` — LLM emits structured JSON intent (never raw coordinates). `ClientMessage::RequestCUCoordinate` / `ServerMessage::CUCoordinate` handle on-demand CU fallback. `UiEvent::PipelineReadyWithIntent` and `UiEvent::CUCoordinateReady` are the primary new event types. `cu_coordinate_pending` flag prevents double execution of CU requests. `last_screenshots` in server.rs is never cleared (retained for CU fallback). `screenshot_local_tx/rx` pair in `OverlayApp` delivers screenshots from `spawn_blocking` to the render loop for UI-map building.
- Multi-monitor coordinate mapping 4-step rule: (1) identify display by `screenshot_index`; (2) normalize pixel coordinate to `[0, 1]` within that screenshot; (3) scale to logical display size; (4) add global display offset.
- `call_with_retry` and `call_once_*` in `computer_use.rs` receive a pre-built `prompt: &str` and use it verbatim — never call `build_prompt(question, ...)` inside those functions (double-wrap bug). Prompts are always built at the call site in `detect_element_location`.
- Strict coordinate type system lives in `src/core/coords.rs`: `PointM` (model 0–1), `PointS` (screenshot pixels), `PointL` (logical desktop), `PointP` (physical/DPI pixels), `RectL` (logical rect), `CaptureDescriptor` (single source of truth for a display's geometry). Raw `(x, y)` tuples must not be used in coordinate math.
- `RefinementView` struct in `computer_use.rs` wraps the pass-2 zoom crop so the crop offset can never be silently dropped when unprojecting the model coordinate back to screenshot space.
- LLM context is fully stateless: `gemini.rs` `stream_llm_response` drops all history entries — only the current user turn (`contents.len() == 1`) is forwarded to Gemini. `context_compactor` has been removed; there is no per-connection history compaction.
- Computer Use now uses a single-attempt call (no retry loop); `call_with_retry` was replaced by a single `call_once` invocation. The 5th coordinate parsing strategy `extract_coords` handles truncated JSON responses by scanning for `x`/`y` keys without a regex crate.
- `display_infos` received in `UiEvent::PipelineReadyWithIntent` must be stored on `OverlayApp` so `CUCoordinateReady` can access them for drift compensation — relying solely on `last_display_infos` causes a race condition when screenshots were just cleared.
- `ServerMessage::Ping` is annotated `#[serde(skip)]`; it is a transport-level signal only — never serialised to JSON. The frontend's adapter match arm must ignore it.
- Gemini default model is `gemini-3-flash-preview`; controlled by `LLM_MODEL` env var override.
