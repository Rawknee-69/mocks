/// Shared WebSocket message types for clicky-desktop.
///
/// Used by both the frontend (src/) and backend (backend/) to ensure
/// both sides agree on message shapes without duplicating definitions.
use serde::{Deserialize, Serialize};

// ─── Frontend → Backend ──────────────────────────────────────────────────────

/// Messages the frontend (Raylib overlay) sends to the backend (pipeline server).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum ClientMessage {
    /// User pressed the push-to-talk hotkey — start transcription session.
    HotkeyPressed,

    /// Raw PCM16 mono 16kHz audio captured from the microphone.
    AudioChunk { pcm16_bytes: Vec<u8> },

    /// User released the hotkey — finalize transcript and run LLM pipeline.
    HotkeyReleased,

    /// User pressed the hotkey again while a pipeline was running — cancel it.
    CancelPipeline,

    /// Current global mouse cursor position, sent each frame (or on change).
    /// The backend uses this for screenshot cursor detection and Computer Use targeting.
    CursorPosition { x: f32, y: f32 },

    /// Screenshots captured by the frontend after hotkey release.
    /// The backend uses these for the Claude vision + Computer Use pipeline.
    Screenshots {
        screenshots: Vec<ScreenshotData>,
        display_infos: Vec<DisplayInfo>,
    },

    /// Frontend requests an on-demand Computer Use coordinate detection for a specific intent.
    /// Triggered when the local UI-map match score falls below the confidence threshold.
    RequestCUCoordinate {
        intent: ElementIntent,
        /// Index into the screenshots array (from the last Screenshots message) to use for CU.
        screenshot_index: usize,
    },
}

/// Structured intent emitted by the LLM — describes WHAT to interact with, not WHERE.
///
/// The frontend uses this to drive UI-map matching and, on fallback, Computer Use.
/// The LLM must never emit coordinates; all position data comes from the UI pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElementIntent {
    /// Human-readable element description, e.g. "Submit button", "Search field".
    pub label: String,
    /// Interaction type: "click", "focus", "hover".
    pub action: String,
    /// Coarse screen region hint, e.g. "top right", "center", "bottom left".
    pub region_hint: Option<String>,
    /// Which screen number (1-based) the element lives on; None = cursor's screen.
    pub screen_number: Option<u32>,
}

/// A captured screenshot to send to the backend for LLM processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenshotData {
    pub jpeg_data: Vec<u8>,
    pub label: String,
}

// ─── Backend → Frontend ──────────────────────────────────────────────────────

/// Messages the backend sends back to the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum ServerMessage {
    /// In-progress transcription text (will be updated as more audio arrives).
    PartialTranscript { text: String },

    /// Completed transcription — won't change. LLM pipeline begins after this.
    FinalTranscript { text: String },

    /// Incremental LLM text chunk from SSE streaming.
    LlmTextDelta { chunk: String },

    /// All pipeline results delivered atomically once LLM + TTS finish.
    /// The `coordinate` field is always `None` — the frontend now drives coordinate
    /// resolution via UI-map matching and on-demand CU fallback.
    PipelineReady {
        /// Text that was spoken aloud (also used for the speech bubble).
        spoken_text: String,

        /// Raw MP3 bytes from Deepgram TTS.  Empty Vec if TTS was disabled or failed.
        mp3_bytes: Vec<u8>,

        /// Always `None` — coordinate resolution moved to the frontend.
        coordinate: Option<(f64, f64)>,

        /// Metadata for every captured display (used for coordinate mapping).
        display_infos: Vec<DisplayInfo>,

        /// Structured element intent from the LLM.  `None` if the user's request
        /// didn't reference any specific UI element.
        intent: Option<ElementIntent>,
    },

    /// Response to `ClientMessage::RequestCUCoordinate`.
    /// The coordinate is in global display space (logical pixels).  `None` means
    /// Computer Use found no matching element.
    CUCoordinate {
        coordinate: Option<(f64, f64)>,
    },

    /// A pipeline stage failed. The frontend should return to Idle state.
    Error {
        code: ErrorCode,
        message: String,
    },

    /// Internal keepalive heartbeat — converted to a WebSocket Ping frame, never
    /// forwarded to the frontend as JSON.  Helps detect stale TCP connections before
    /// the OS fires ECONNRESET / error 10054.
    #[serde(skip)]
    Ping,
}

// ─── Shared data types ────────────────────────────────────────────────────────

/// Metadata about one captured monitor.
///
/// Identical to `core::coordinate_mapper::DisplayInfo` in the frontend crate —
/// we keep this copy here so neither side needs to depend on the other.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayInfo {
    /// 1-based index matching Claude's screen numbering.
    pub screen_number: u32,

    /// Top-left corner of this display in the global virtual-desktop space.
    pub global_origin_x: f64,
    pub global_origin_y: f64,

    /// Logical size of the display in points (may differ from pixels on HiDPI).
    pub display_width_points: f64,
    pub display_height_points: f64,

    /// Pixel dimensions of the screenshot taken for this display.
    pub screenshot_width_pixels: u32,
    pub screenshot_height_pixels: u32,

    /// Whether the user's mouse cursor was on this display at capture time.
    pub is_cursor_display: bool,
}

/// A parsed POINT-tag instruction from the LLM response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PointingInstruction {
    /// X coordinate in the screenshot's pixel space.
    pub screenshot_x: f64,

    /// Y coordinate in the screenshot's pixel space.
    pub screenshot_y: f64,

    /// Short human-readable label for the element (e.g. "save button").
    pub label: String,

    /// Which screen this element lives on (`None` = cursor's screen).
    pub screen_number: Option<u32>,
}

/// Categorises which pipeline stage produced an error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ErrorCode {
    /// AssemblyAI transcription failed (token fetch, WebSocket, etc.).
    Transcription,

    /// Claude LLM vision/streaming call failed.
    LlmPipeline,

    /// ElevenLabs TTS synthesis failed.
    Tts,

    /// Claude Computer Use coordinate detection failed.
    ComputerUse,

    /// Screenshot capture failed.
    Screenshot,

    /// Generic server-side error not covered by the above.
    Internal,
}
