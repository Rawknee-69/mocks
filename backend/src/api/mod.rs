pub mod computer_use;
pub mod deepgram;
pub mod deepgram_tts;
pub mod gemini;
pub mod openrouter;

/// Selects which AI backend to use for LLM, CU, and compaction.
/// Constructed once at server startup from env vars and `Arc`'d into every connection.
/// All provider-specific logic dispatches through this enum — no string-based branching
/// exists anywhere below `server.rs`.
#[derive(Clone)]
pub enum Provider {
    OpenRouter { api_key: String, model: String },
    Gemini { api_key: String, model: String },
}
