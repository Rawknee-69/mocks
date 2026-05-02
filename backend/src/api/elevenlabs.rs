/// ElevenLabs TTS client — sends text, returns raw MP3 bytes.
use crate::error::AppError;
use reqwest::Client;
use serde_json::json;
use tracing::{debug, error};

const TTS_MODEL_ID: &str = "eleven_multilingual_v2";
const TTS_STABILITY: f64 = 0.5;
const TTS_SIMILARITY_BOOST: f64 = 0.75;
const TTS_OUTPUT_FORMAT: &str = "mp3_44100_128";
const DEFAULT_VOICE_ID: &str = "kPzsL2i3teMYv0FxEYQ6";

/// Sends `text` to ElevenLabs TTS and returns the raw MP3 audio bytes.
pub async fn synthesize_speech(
    http_client: &Client,
    api_key: &str,
    voice_id: Option<&str>,
    text: &str,
) -> Result<Vec<u8>, AppError> {
    let request_body = json!({
        "text": text,
        "model_id": TTS_MODEL_ID,
        "voice_settings": {
            "stability": TTS_STABILITY,
            "similarity_boost": TTS_SIMILARITY_BOOST,
        },
    });

    debug!("requesting TTS for {} chars of text", text.len());

    let voice = voice_id.unwrap_or(DEFAULT_VOICE_ID);
    let url = format!(
        "https://api.elevenlabs.io/v1/text-to-speech/{}?output_format={}",
        voice, TTS_OUTPUT_FORMAT,
    );

    let response = http_client
        .post(&url)
        .header("xi-api-key", api_key)
        .header("content-type", "application/json")
        .header("accept", "audio/mpeg")
        .json(&request_body)
        .send()
        .await
        .map_err(|err| AppError::ElevenLabs(format!("network error: {err}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "unable to read error body".into());
        error!("ElevenLabs TTS error {status}: {body}");
        return Err(AppError::ElevenLabs(format!("API error {status}: {body}")));
    }

    let audio_bytes = response
        .bytes()
        .await
        .map_err(|err| AppError::ElevenLabs(format!("body read error: {err}")))?;

    debug!("TTS response: {} bytes of audio", audio_bytes.len());
    Ok(audio_bytes.to_vec())
}
