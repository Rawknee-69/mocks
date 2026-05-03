/// Deepgram TTS handler — synthesizes speech and sends the MP3 bytes back.
use crate::api::deepgram_tts;
use crate::error::AppError;
use clicky_types::ServerMessage;
use tokio::sync::mpsc;
use tracing::info;

/// Synthesizes `text` via Deepgram Aura-2 and sends the resulting MP3 bytes as a
/// `ServerMessage::TtsAudio` on `server_tx`. Falls back silently on error
/// (the `ServerMessage::Error` is sent so the frontend can do espeak-ng).
pub async fn run_tts(
    http_client: &reqwest::Client,
    api_key: &str,
    voice_id: Option<&str>,
    text: String,
    server_tx: mpsc::Sender<ServerMessage>,
) {
    match deepgram_tts::synthesize_speech(http_client, api_key, voice_id, &text).await {
        Ok(mp3_bytes) => {
            info!("TTS synthesized {} bytes", mp3_bytes.len());
            let _ = server_tx.send(ServerMessage::TtsAudio { mp3_bytes }).await;
        }
        Err(err) => {
            let _ = server_tx
                .send(AppError::Tts(err.to_string()).into_server_message())
                .await;
        }
    }
}
