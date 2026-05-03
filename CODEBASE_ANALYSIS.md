# Clicky Desktop — Deep Codebase Analysis

> Senior systems engineer analysis. Generated from full source read of all 29 backend and 46 frontend source files.

---

## 1. SYSTEM OVERVIEW

**What it does:** Clicky is a push-to-talk AI desktop assistant. The user holds a hotkey, speaks, and the system transcribes speech, takes screenshots, asks a vision LLM what to say and where to click, speaks the response via TTS, and flies a blue triangle cursor overlay to the identified UI element along a bezier arc.

**Problem it solves:** Hands-free, voice-driven GUI automation — bridging natural language instructions to precise screen coordinates without requiring the user to touch a mouse.

**Core workflows:**
1. **Voice → Text:** cpal mic capture → PCM16 16kHz → Deepgram Nova-3 WebSocket STT → transcript
2. **Text + Screen → Coordinate + Response:** transcript + JPEG screenshots → LLM vision (spoken text) + Computer Use vision (pixel coordinate) in parallel
3. **Text → Audio:** LLM spoken text → Deepgram Aura-2 TTS → MP3 → rodio playback
4. **Coordinate → Animation:** display-space `(x, y)` → bezier flight of the overlay triangle

---

## 2. ARCHITECTURE

```
┌─────────────────────────────────────────────────────────┐
│  FRONTEND  (clicky-desktop)                             │
│                                                         │
│  winit event loop (monitor refresh Hz)                  │
│  ┌──────────────┐  ┌──────────────┐  ┌───────────────┐ │
│  │ OverlayApp   │  │ AudioCapture │  │ HotkeyHandler │ │
│  │ wgpu/skia    │  │ cpal         │  │ global-hotkey │ │
│  │ renderer     │  │ PCM16 conv.  │  │ evdev(Wayland)│ │
│  └──────┬───────┘  └──────┬───────┘  └───────┬───────┘ │
│         │                 │                   │         │
│         └────────── ApiAdapter ───────────────┘         │
│                  (WS client, auto-reconnect)            │
│                  ws://127.0.0.1:9001                    │
└─────────────────────────────┬───────────────────────────┘
                              │  JSON text frames
                              │  ClientMessage / ServerMessage
┌─────────────────────────────▼───────────────────────────┐
│  BACKEND  (clicky-backend)                              │
│                                                         │
│  tokio async runtime                                    │
│  ┌──────────────────────────────────────────────────┐   │
│  │  server.rs — WebSocket accept + connection FSM   │   │
│  │                                                  │   │
│  │  ┌─────────────────┐  ┌────────────────────────┐ │   │
│  │  │ transcription.rs│  │  llm_pipeline.rs       │ │   │
│  │  │ Deepgram Nova-3 │  │  tokio::join!(LLM, CU) │ │   │
│  │  │ WS STT bridge   │  │  then TTS              │ │   │
│  │  └────────┬────────┘  └──────────┬─────────────┘ │   │
│  │           │ FinalTranscript       │ PipelineReady  │   │
│  └───────────┼───────────────────────┼───────────────┘   │
│              │                       │                   │
│  ┌───────────▼──────┐  ┌─────────────▼──────────────┐   │
│  │  api/deepgram.rs │  │  api/computer_use.rs       │   │
│  │  PCM16 → STT     │  │  2-pass zoom CU            │   │
│  └──────────────────┘  └────────────────────────────┘   │
│  ┌───────────────────┐  ┌────────────────────────────┐   │
│  │ api/openrouter.rs │  │  api/deepgram_tts.rs       │   │
│  │ api/gemini.rs     │  │  Aura-2 TTS → MP3          │   │
│  └───────────────────┘  └────────────────────────────┘   │
│                                                         │
│  ┌─────────────────────────────────────────────────┐    │
│  │  core/  — coordinate_mapper, point_parser,      │    │
│  │           context_compactor, pcm16_converter     │    │
│  └─────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────┘

SHARED: clicky-types/  — ClientMessage, ServerMessage, DisplayInfo,
                         ScreenshotData, PointingInstruction, ErrorCode
```

**Key layers:**
- **Frontend:** winit event loop, wgpu/tiny-skia rendering, cpal audio, hotkey handling, screenshot capture. Zero API keys.
- **Backend:** tokio async pipeline server. All AI API calls. All env vars.
- **Shared types:** `clicky-types` crate — single source of truth for the WS wire protocol.

---

## 3. CORE PIPELINE — Step by Step

```
1. User presses hotkey
   └─ Frontend: VoiceState → Listening
   └─ Frontend sends: ClientMessage::HotkeyPressed
   └─ Backend: spawns StreamingTranscriptionSession (Deepgram Nova-3 WS)

2. Mic audio flows
   └─ cpal float32 → PCM16 16kHz mono (pcm16_converter)
   └─ Buffered in sync_channel(128), batched at MIN_CHUNK_BYTES=3200
   └─ Frontend sends: ClientMessage::AudioChunk { pcm16_bytes }
   └─ Backend: forwards to Deepgram WS as binary frames
   └─ Deepgram emits partials → ServerMessage::PartialTranscript → UiEvent::PartialTranscript

3. User releases hotkey
   └─ Frontend: captures screenshots of all monitors via xcap/grim
   └─ Frontend sends: ClientMessage::HotkeyReleased
   └─ Frontend sends: ClientMessage::Screenshots { screenshots, display_infos }
   └─ Backend: sets audio_tx = None (drains remaining audio, 800ms timeout)
   └─ Backend: requests_final_transcript(), waits 4000ms for Deepgram speech_final

4. Final transcript arrives
   └─ Backend server.rs intercepts ServerMessage::FinalTranscript
   └─ Retrieves pending_screenshots (race risk — see §7)
   └─ Calls context_compactor::maybe_compact() if history ≥ 80% of 256K tokens
   └─ Spawns: tokio::spawn(run_llm_pipeline(...))

5. LLM Pipeline (parallel)
   ├─ LLM call (OpenRouter or Gemini, non-streaming JSON)
   │   └─ transcript + screenshot base64 + conversation history → vision model
   │   └─ Response contains spoken text + optional [POINT:x,y:label:screenN] tag
   │   └─ point_parser::parse_claude_response() splits spoken_text / pointing coord
   │
   └─ Computer Use call (same or different model)
       └─ detect_element_location() — 2-pass zoom (see §5)
       └─ Returns Option<(global_x, global_y)>

   tokio::join!(llm_future, cu_future) — both fire simultaneously

6. TTS (after LLM text known)
   └─ deepgram_tts::synthesize_speech() → MP3 bytes
   └─ Fires after join! resolves (CU typically slower, so effectively parallel)

7. Pipeline complete
   └─ Coordinate: cu_result.or(point_tag_coordinate)  ← CU takes precedence
   └─ Backend sends: ServerMessage::PipelineReady { spoken_text, mp3_bytes, coordinate, display_infos }
   └─ Frontend UiEvent::PipelineReady
   └─ AudioPlayer::play_mp3(mp3_bytes)
   └─ VoiceState → Responding
   └─ Overlay: NavigatingToTarget bezier flight to coordinate
   └─ 2s hold at target → ReturningToMouse → FollowingMouse
   └─ VoiceState → Idle
```

---

## 4. MODULE BREAKDOWN

### Frontend

| Module | Purpose | Inputs | Outputs | Key Functions |
|---|---|---|---|---|
| `src/overlay/window.rs` | 60fps render loop, all UI state | winit events, UiEvent | wgpu frame | `about_to_wait()`, `handle_hotkey()`, `handle_ui_event()` |
| `src/app/state_machine.rs` | Pure VoiceState FSM | `VoiceStateTransition` | `Option<VoiceState>` | `VoiceState::apply()` |
| `src/api/adapter.rs` | Non-blocking WS bridge | `ClientMessage` | `UiEvent` | `try_connect()`, `send()`, `try_recv()` |
| `src/audio/capture` | cpal mic capture | mic device | PCM16 chunks → channel | — |
| `src/audio/playback` | rodio MP3 player | MP3 bytes | audio output | `play_mp3()`, `is_playing()`, `stop()` |
| `src/screenshot/` | Multi-monitor JPEG capture | display list | `Vec<ScreenshotData>` | grim (Wayland), xcap (X11/Win) |
| `src/core/bezier_flight.rs` | Bezier arc math | start/end points | position per frame | — |
| `src/core/pcm16_converter.rs` | float32 → PCM16 | cpal samples | `Vec<u8>` | `convert_float32_to_pcm16_mono()` |

### Backend

| Module | Purpose | Inputs | Outputs | Key Functions |
|---|---|---|---|---|
| `backend/src/server.rs` | WS accept + connection FSM | TCP stream | `ServerMessage` to WS | `run()`, `handle_connection()`, `handle_client_message()` |
| `backend/src/handlers/llm_pipeline.rs` | LLM + CU + TTS orchestration | transcript, screenshots | `PipelineReady` | `run_llm_pipeline()` |
| `backend/src/handlers/transcription.rs` | Deepgram STT bridge | PCM16 channel | `PartialTranscript`, `FinalTranscript` | `run_transcription_bridge()` |
| `backend/src/api/computer_use.rs` | 2-pass zoom element detection | JPEG, question | `Option<ComputerUseCoordinate>` | `detect_element_location()` |
| `backend/src/api/openrouter.rs` | OpenRouter LLM calls | messages + screenshots | response text | `stream_llm_response()` |
| `backend/src/api/gemini.rs` | Gemini LLM calls | messages + screenshots | response text | `stream_llm_response()` |
| `backend/src/api/deepgram.rs` | Deepgram Nova-3 WS STT | audio bytes | `TranscriptUpdate` | `StreamingTranscriptionSession` |
| `backend/src/api/deepgram_tts.rs` | Deepgram Aura-2 TTS | text | MP3 bytes | `synthesize_speech()` |
| `backend/src/core/coordinate_mapper.rs` | Screenshot px → global coords | screenshot px, `DisplayInfo` | global `(x, y)` | `map_screenshot_pixels_to_global()` |
| `backend/src/core/point_parser.rs` | `[POINT:x,y]` tag extraction | LLM response text | `PointingParseResult` | `parse_claude_response()` |
| `backend/src/core/context_compactor.rs` | LLM history summarisation | conversation history | compacted history | `maybe_compact()` |

---

## 5. COORDINATE SYSTEM LOGIC

### How coordinates are generated

**Path A — Computer Use (primary, preferred):**
```
Screenshot JPEG (native pixels)
  → resize to MAX_LONG_EDGE=1920 (compute_model_dims, aspect-preserving, no upscale)
  → Pass 1 model call → raw (x, y) in model pixel space
  → denormalise_if_needed() — handles 3 coordinate spaces:
      [0,1] unit normalised  → scale to model dims
      [0,1000] Gemini grid   → scale to model dims (detected by bounds-exceedance)
      raw pixels             → no-op
  → clamp_to_dims() → (mx1, my1) in model pixels
  → ModelDims::unproject() → (coarse_dx, coarse_dy) in display logical points

  → Crop::centred_on(coarse_dx, coarse_dy, ZOOM_FRACTION=0.25)
      — 25% of display size, clamped to display bounds
  → crop_and_resize():
      display logical-point crop → native pixel crop (via display_w/h ratio)
      resize crop to MAX_LONG_EDGE=1920 (compute_model_dims_allow_upscale, always upscales)
  → Pass 2 model call → raw (x, y) in zoomed crop pixel space
  → denormalise_if_needed() → clamp_to_dims()
  → Crop::unproject_model() → (fine_dx, fine_dy) in display logical points

  → global_x = cursor_display.global_origin_x + fine_dx
  → global_y = cursor_display.global_origin_y + fine_dy
```

**Path B — POINT tag fallback (when CU returns None):**
```
LLM response text
  → point_parser: regex extracts [POINT:x,y:label:screenN]
  → x, y are screenshot pixel coordinates (not display logical points)
  → coordinate_mapper::map_screenshot_pixels_to_global():
      scale = display_points / screenshot_pixels (handles HiDPI)
      global = origin + clamped_px * scale
```

### Key transformations
1. **Model space → display logical points:** `ModelDims::unproject()` — linear scale, single uniform factor, no distortion
2. **Crop model space → display logical points:** `Crop::unproject_model()` — maps through crop origin offset
3. **Screenshot pixels → global:** `map_screenshot_pixels_to_global()` — accounts for DPI scale and multi-monitor origin

### Known failure points
- Pass 1 error > half the crop radius → Pass 2 crop misses target entirely
- `is_cursor_display` flag wrong → CU runs on wrong monitor's screenshot
- Model returns screen-centre `(width/2, height/2)` when uncertain — no guard exists
- Gemini `(0,0)` return (bottom-left confused with top-left origin) — partially caught by `denormalise_if_needed` but only when it exceeds bounds

---

## 6. MODEL INTERACTION

### Models used
| Role | Default | Override env var |
|---|---|---|
| LLM vision | `nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free` (OpenRouter) | `LLM_MODEL` |
| Computer Use | `nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free` (OpenRouter) | `CU_MODEL` |
| STT | Deepgram Nova-3 | — |
| TTS | Deepgram Aura-2 Thalia | `DEEPGRAM_TTS_VOICE` |

Provider switched by `AI_PROVIDER=gemini|openrouter`.

### Prompt structure (Computer Use)
**Pass 1 prompt (`build_prompt`):**
- Embeds exact pixel dimensions of the model image
- ASCII diagram of coordinate system
- User intent analysis rules (e.g., "close window" → innermost × button)
- One-element selection rules with element-type-specific aim points
- Strict output schema: `{"x": int, "y": int, "label": "1-4 words"}` or `{"x": null, "y": null, "label": "none"}`
- Pre-output checklist (reduces off-by-edge errors)
- Temperature: `0.0`

**Pass 2 prompt (`build_pass2_prompt`):**
- States that image is a high-zoom crop of a larger display
- Crop geometry provided explicitly
- Coordinate space is THIS image's pixels (0 to width/height)
- Same JSON output schema, shorter prompt (no checklist)

### Response parsing
Multi-strategy cascade in `parse_coordinate()`:
1. Strip `<think>` / `</reasoning>` / `[/THINK]` → direct JSON parse
2. Strip markdown fences → JSON parse
3. `last_brace_span()` — extract last balanced `{...}` from prose → JSON parse
- **`FlexF64`:** Handles string-encoded coords (`"640"`) from reasoning models
- **Retry logic:** Up to 3 attempts with exponential backoff (600ms base). `ParseResult::NoElement` (valid null) **never** retries — prevents burning quota on intentional "no element" answers
- **Temperature:** Always `0.0` for determinism

### Rate limiting
`RPM_CAP = 20` permits shared via `Arc<Semaphore>` across both LLM and CU calls. Since they run in `tokio::join!`, a single pipeline interaction consumes 2 permits simultaneously.

---

## 7. ERROR AND FAILURE ANALYSIS

### Hallucination risks

| Source | Type | Severity |
|---|---|---|
| CU model returns `(width/2, height/2)` | Screen-centre hallucination | **High** — triangle flies to screen centre, looks broken |
| CU model returns coordinates for wrong element | Wrong-element hallucination | Medium — triangle flies to wrong place |
| LLM `[POINT]` tag has wrong pixels | POINT-tag imprecision | Low — CU result takes precedence if found |
| CU pass 2 runs on wrong region | Compound hallucination | Low — only if pass 1 was badly wrong |

### Precision loss
- **Pass 1 → Pass 2 cascade:** If pass 1 lands more than 12.5% of display size away from the real element, the 25% crop will miss it entirely. Pass 2 then returns `NoElement`, and pass 1's imprecise result is used.
- **JPEG re-encoding quality loss (Q85):** For very small elements (small icons, close buttons), JPEG artifacts can degrade localization quality at the model side.
- **Integer rounding in crop pixel mapping:** `crop_and_resize` uses `.round()` and `.ceil()` in separate steps — potential ±1px discrepancy at display boundaries.

### Race conditions / async issues
- **Screenshots-before-transcript race:** Screenshots are sent on `HotkeyReleased`, stored in `pending_screenshots`. If `FinalTranscript` arrives before screenshots (network race), `pending_screenshots.take()` returns `None`, the pipeline aborts silently with `AppError::Screenshot`. Real risk on slow systems or Wi-Fi localhost.
- **Old transcription tasks:** A new `HotkeyPressed` fires the cancel oneshot but previously spawned transcription `tokio::spawn` tasks continue until their send fails on the dropped channel. Silent, harmless — but tasks can outlive their intent.
- **`audio_tx.try_send` drops audio chunks:** The `sync_channel(128)` will silently drop audio if the Deepgram session hasn't started yet or is backpressured.
- **`reqwest::Client::new()` per connection:** Fresh HTTP client per WebSocket connection means no TCP keep-alive reuse to Deepgram / OpenRouter. Acceptable for one steady frontend connection, wasteful under reconnection churn.

### Weak assumptions
- **`is_cursor_display` correctness:** The frontend must correctly tag which display the cursor is on. If the cursor is on a display boundary, the wrong screenshot is sent to CU.
- **Single display for CU:** Multi-monitor setups where the target element is on a non-cursor display entirely bypass CU. Only the less-accurate POINT tag fallback covers cross-monitor targeting.
- **Token estimation:** `context_compactor` estimates token count from character count (not actual tokenization). Non-ASCII text (CJK, emoji) can overflow the 256K context window before the 80% threshold fires.
- **History size cap of 10:** Simple `pop_front()` drops oldest exchange without semantic awareness — important context (e.g., user preferences stated early in conversation) can be lost.

---

## 8. DESIGN TRADEOFFS

### Two-process split (frontend / backend)
- **Why:** Keeps API keys entirely off the frontend; clean separation of rendering concerns (winit/wgpu/tiny-skia) from async I/O (tokio/reqwest/websocket)
- **Pro:** Frontend can crash-restart without losing backend state; backend can be replaced or restarted independently
- **Con:** Screenshots travel over localhost as base64-encoded JSON — inefficient for large multi-monitor captures; introduces the screenshot-arrival race

### Non-streaming LLM
- **Why:** TTS needs the full response text before synthesis can begin; streaming partial tokens is wasted work
- **Pro:** Simpler implementation; no SSE parser; single atomic `PipelineReady` event
- **Con:** Perceived latency is higher — user hears nothing until full LLM response + TTS synthesis are complete (~2-4s)

### Two-pass Computer Use vs. single-pass
- **Why:** Single-pass at full resolution gives coarse coordinates (~20-50px error on a 1920px-wide display). Pass 2 provides ~4× zoom → ~5-12px error on the original display
- **Pro:** Significantly better precision for small elements (close buttons, icons)
- **Con:** Doubles model API calls and latency; if pass 1 is badly wrong, pass 2 is wasted. Worst case: 45s timeout × 2 passes × 3 retries = 270s (highly unlikely but theoretically possible)

### POINT-tag fallback
- **Why:** Gives the LLM a mechanism to communicate coordinates without a second specialised model call; useful when CU is unavailable or returns null
- **Con:** LLM POINT coordinates are raw screenshot pixels — requires a separate coordinate mapping step. Less precise than CU because the LLM isn't specialised for pixel-perfect element detection.

### `ParseResult` three-state enum
- **Why:** Prevents burning 3 retry attempts when the model correctly reports "no element found". A two-state design caused retries on valid "no element" responses, wasting API quota.
- **Pro:** Clean distinction between `NoElement` (legitimate null) and `ParseFailed` (malformed response worth retrying)
- **Con:** Model must output exactly `{"x": null, "y": null}` to signal this — any JSON parse failure falls through to retry regardless

---

## 9. IMPROVEMENT OPPORTUNITIES

### Immediate — High Impact

**1. Screen-centre hallucination guard**

The most common failure mode: model returns `(width/2, height/2)` when uncertain. No guard exists in the code. Add after unprojecting pass 1:

```rust
let is_screen_center =
    (coarse_dx - display_width_points / 2.0).abs() < display_width_points * 0.02
    && (coarse_dy - display_height_points / 2.0).abs() < display_height_points * 0.02;
if is_screen_center {
    warn!("ComputerUse: pass-1 returned screen centre — treating as hallucination");
    return None;
}
```

**2. Screenshots-before-transcript race**

In `server.rs`, the LLM pipeline fires on `FinalTranscript` and takes `pending_screenshots` at that moment. Screenshots arrive asynchronously after `HotkeyReleased`. Fix options:
- Add a monotonic request ID to both `HotkeyReleased` and `Screenshots` and wait for the matching pair
- Or: add a short `tokio::time::timeout(300ms)` wait for `pending_screenshots` to populate before spawning the pipeline

**3. `reqwest::Client::new()` per connection**

Currently `http_client` is created inside `handle_connection()`. Move it to `server::run()` and pass as `Arc<reqwest::Client>`. One-line fix, meaningful for repeated reconnects.

### Medium-term — Robustness

**4. True token counting**

Replace character-heuristic token estimation in `context_compactor` with a proper tokenizer (e.g., `tiktoken-rs`). The heuristic fails for non-ASCII text and can overflow the model's context window before the 80% threshold fires.

**5. Pass 1 confidence scoring**

Before committing to a pass 2 crop, check whether the pass 1 coordinate looks like a guess:
- Near a display edge (model confusion about boundaries)
- A "round" number (multiple of 50 or 100) suggesting the model interpolated rather than detected

Discard or re-run pass 1 rather than proceeding to a bad crop.

**6. Multi-display Computer Use**

Run CU on all display screenshots in parallel (not just `is_cursor_display`), collect all non-null results, pick by heuristic (closest to cursor, highest confidence label, etc.). Currently cross-monitor targeting silently degrades to imprecise POINT tags.

**7. Adaptive JPEG quality for pass 2 crops**

Small crops covering a region of, say, 100×60 logical points should be encoded at Q95+ before sending to the model. The current fixed Q85 introduces visible artifacts at high zoom levels that can confuse the model on fine-grained elements.

### Strategic — Architecture Evolution

**8. Hybrid CV grounding layer**

Add a classical computer vision pre-pass (edge detection + connected-component bounding boxes via the `image` crate) to produce candidate element regions before calling the model. Feed the top-N candidate bounding boxes as structured hints alongside the screenshot. This constrains the model's search space and dramatically reduces hallucination on standard UI elements.

**9. Confidence-gated verification pass**

After CU returns a coordinate, take a 64×64 screenshot crop centred on that coordinate and run one lightweight binary model call: "Is there a clickable UI element at the centre of this image?" If "no", fall back to POINT tag or surface an error to the user. Adds ~200ms but eliminates misfires.

**10. Windows UI Automation integration**

On Windows, the UI Automation API (accessible via the `windows` crate) can enumerate all visible controls with their exact bounding rectangles. Pass this structured text alongside the screenshot as an additional context block. This essentially eliminates coordinate hallucination for standard Win32/WPF/UWP controls, making model inference a disambiguation step rather than a localisation step.

---

## 10. FINAL SUMMARY

**One-paragraph mental model:** Clicky is a two-process Rust pipeline where a winit/wgpu overlay frontend captures voice input and desktop screenshots and streams them to a local tokio backend over WebSocket. The backend orchestrates four AI services in an optimal parallel pattern: Deepgram STT for transcription, then (simultaneously) an LLM vision call to generate a spoken response and a Computer Use vision call to find the precise screen coordinate of the target element, followed by Deepgram TTS to synthesize the response audio. All three results converge into a single `PipelineReady` event that drives the overlay's bezier animation, audio playback, and voice state machine. The coordinate detection uses a sophisticated two-pass zoom strategy with robust multi-format coordinate parsing (raw pixels, [0,1] normalised, [0,1000] Gemini grid, string-encoded integers), but lacks a guard against the most common vision model failure mode: returning the geometric screen centre when uncertain.

**Key risks:**
1. Screen-centre hallucination causes the overlay triangle to fly to the middle of the screen — no guard exists in the code
2. Screenshot/transcript arrival race can silently skip the entire LLM pipeline on slow systems
3. CU is blind to all non-cursor displays — multi-monitor cross-display targeting silently degrades to less-precise POINT tags
4. Token counting in `context_compactor` is character-based, not tokenizer-based — can overflow model context on non-ASCII input

**Key strengths:**
1. Two-pass zoom gives ~4× coordinate precision improvement over a naive single-pass approach
2. Three-state `ParseResult` enum correctly prevents retrying valid "no element" responses, saving API quota
3. `denormalise_if_needed` handles three distinct model coordinate spaces robustly, backed by a regression test suite
4. `pending_screenshots` pattern cleanly separates screenshot capture (frontend) from all API calls (backend) with no shared mutable state crossing process boundaries
5. 41 unit tests covering coordinate math, crop geometry, JSON parsing, and denormalisation make `computer_use.rs` the most defensively tested module in the codebase
