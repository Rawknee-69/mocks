/// Deepgram Aura-2 TTS client — sends text, returns raw MP3 bytes.
///
/// Drop-in replacement for the former ElevenLabs implementation.
/// The `synthesize_speech` signature is identical so `handlers/tts.rs` needs no changes.
use crate::error::AppError;
use reqwest::Client;
use serde_json::json;
use tracing::{debug, error};

const DEFAULT_VOICE_MODEL: &str = "aura-2-thalia-en";
const TTS_ENDPOINT: &str = "https://api.deepgram.com/v1/speak";

/// Sends `text` to Deepgram Aura-2 TTS and returns the raw MP3 audio bytes.
///
/// `voice_id` maps to a Deepgram model name (e.g. `"aura-2-thalia-en"`).
/// When `None`, the `DEFAULT_VOICE_MODEL` is used.
pub async fn synthesize_speech(
    http_client: &Client,
    api_key: &str,
    voice_id: Option<&str>,
    text: &str,
) -> Result<Vec<u8>, AppError> {
    let voice = voice_id.unwrap_or(DEFAULT_VOICE_MODEL);
    let url = format!("{TTS_ENDPOINT}?model={voice}");

    let request_body = json!({ "text": text });

    debug!("requesting Deepgram TTS for {} chars of text", text.len());

    let response = http_client
        .post(&url)
        .header("Authorization", format!("Token {api_key}"))
        .header("Content-Type", "application/json")
        .json(&request_body)
        .send()
        .await
        .map_err(|err| AppError::Tts(format!("network error: {err}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "unable to read error body".into());
        error!("Deepgram TTS error {status}: {body}");
        return Err(AppError::Tts(format!("API error {status}: {body}")));
    }

    let audio_bytes = response
        .bytes()
        .await
        .map_err(|err| AppError::Tts(format!("body read error: {err}")))?;

    debug!("Deepgram TTS response: {} bytes of audio", audio_bytes.len());
    Ok(audio_bytes.to_vec())
}
