/// Deepgram TTS handler — synthesizes speech and sends the MP3 bytes back.
use crate::api::deepgram_tts;
use tracing::info;

/// Synthesizes `text` via Deepgram Aura-2 and returns the raw MP3 bytes.
///
/// Returns an empty `Vec` on error so callers can treat a zero-length buffer as
/// "TTS unavailable" without a separate error branch.
pub async fn synthesize_speech_bytes(
    http_client: &reqwest::Client,
    api_key: &str,
    voice_id: Option<&str>,
    text: &str,
) -> Vec<u8> {
    match deepgram_tts::synthesize_speech(http_client, api_key, voice_id, text).await {
        Ok(mp3_bytes) => {
            info!("TTS synthesized {} bytes", mp3_bytes.len());
            mp3_bytes
        }
        Err(err) => {
            tracing::warn!("TTS synthesis failed: {err} — pipeline will send empty mp3_bytes");
            Vec::new()
        }
    }
}
