/// AssemblyAI real-time streaming transcription via WebSocket.
///
/// Flow:
/// 1. Fetch a temporary token from AssemblyAI
/// 2. Open a WebSocket to AssemblyAI's streaming endpoint
/// 3. Stream PCM16 audio frames as binary messages
/// 4. Receive JSON turn-based transcript updates
/// 5. On hotkey release, send ForceEndpoint message and finalize
use crate::error::AppError;
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde::Deserialize;
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, warn};

const ASSEMBLYAI_WEBSOCKET_BASE_URL: &str = "wss://streaming.assemblyai.com/v3/ws";
const AUDIO_SAMPLE_RATE: u32 = 16000;
const AUDIO_ENCODING: &str = "pcm_s16le";
const SPEECH_MODEL: &str = "u3-rt-pro";

/// Fetches a temporary streaming token from AssemblyAI.
/// The token is valid for 480 seconds (8 minutes).
pub async fn fetch_temporary_streaming_token(
    http_client: &Client,
    api_key: &str,
) -> Result<String, AppError> {
    let response = http_client
        .get("https://streaming.assemblyai.com/v3/token?expires_in_seconds=480")
        .header("Authorization", api_key)
        .send()
        .await
        .map_err(|err| AppError::AssemblyAi(format!("token fetch network error: {err}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(AppError::AssemblyAi(format!("token fetch HTTP {status}: {body}")));
    }

    let token_response: TokenResponse = response
        .json()
        .await
        .map_err(|err| AppError::AssemblyAi(format!("token parse error: {err}")))?;

    debug!("fetched AssemblyAI streaming token");
    Ok(token_response.token)
}

/// Manages a single streaming transcription session.
///
/// Send PCM16 audio via `audio_sender()`. Receive transcript updates from
/// `transcript_receiver`. Call `request_final_transcript()` on hotkey release.
pub struct StreamingTranscriptionSession {
    audio_sender: mpsc::Sender<Vec<u8>>,
    pub transcript_receiver: mpsc::Receiver<TranscriptUpdate>,
    control_sender: mpsc::Sender<ControlMessage>,
}

#[derive(Debug, Clone)]
pub enum TranscriptUpdate {
    Partial(String),
    Final(String),
    Error(String),
}

enum ControlMessage {
    RequestFinalTranscript,
}

impl StreamingTranscriptionSession {
    /// Connects to AssemblyAI and starts background audio/transcript tasks.
    pub async fn start(temporary_auth_token: &str) -> Result<Self, AppError> {
        let websocket_url = format!(
            "{}?sample_rate={}&encoding={}&speech_model={}&token={}",
            ASSEMBLYAI_WEBSOCKET_BASE_URL,
            AUDIO_SAMPLE_RATE,
            AUDIO_ENCODING,
            SPEECH_MODEL,
            temporary_auth_token,
        );

        let request = tokio_tungstenite::tungstenite::http::Request::builder()
            .uri(&websocket_url)
            .header("Host", "streaming.assemblyai.com")
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                tokio_tungstenite::tungstenite::handshake::client::generate_key(),
            )
            .body(())
            .map_err(|err| AppError::AssemblyAi(format!("request builder error: {err}")))?;

        let (websocket_stream, _response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|err| AppError::AssemblyAi(format!("WebSocket connect error: {err}")))?;

        debug!("connected to AssemblyAI WebSocket");

        let (mut websocket_writer, mut websocket_reader) = websocket_stream.split();

        let (audio_sender, mut audio_receiver) = mpsc::channel::<Vec<u8>>(64);
        let (transcript_sender, transcript_receiver) = mpsc::channel::<TranscriptUpdate>(32);
        let (control_sender, mut control_receiver) = mpsc::channel::<ControlMessage>(4);

        // Audio sender task: forwards PCM16 data to the WebSocket as binary messages
        tokio::spawn(async move {
            debug!("audio sender task started");
            loop {
                tokio::select! {
                    biased; // prioritise audio to avoid starving it
                    audio_data = audio_receiver.recv() => {
                        match audio_data {
                            Some(data) => {
                                if let Err(err) = websocket_writer
                                    .send(Message::Binary(data.into()))
                                    .await
                                {
                                    error!("WebSocket send error: {err}");
                                    break;
                                }
                            }
                            None => {
                                debug!("audio channel closed, stopping sender");
                                break;
                            }
                        }
                    }
                    control_message = control_receiver.recv() => {
                        match control_message {
                            Some(ControlMessage::RequestFinalTranscript) => {
                                debug!("sending ForceEndpoint to AssemblyAI");
                                let msg = serde_json::json!({"type": "ForceEndpoint"});
                                if let Err(err) = websocket_writer
                                    .send(Message::Text(msg.to_string().into()))
                                    .await
                                {
                                    error!("ForceEndpoint send error: {err}");
                                }
                            }
                            None => {
                                debug!("control channel closed, sending Terminate");
                                let terminate = serde_json::json!({"type": "Terminate"});
                                let _ = websocket_writer
                                    .send(Message::Text(terminate.to_string().into()))
                                    .await;
                                break;
                            }
                        }
                    }
                }
            }
            debug!("audio sender task ended");
        });

        // Transcript receiver task: parses incoming WebSocket messages
        let transcript_sender_clone = transcript_sender.clone();
        tokio::spawn(async move {
            let mut stored_turns: BTreeMap<i64, StoredTurnTranscript> = BTreeMap::new();
            let mut has_delivered_final = false;

            while let Some(message_result) = websocket_reader.next().await {
                let message = match message_result {
                    Ok(msg) => msg,
                    Err(err) => {
                        warn!("WebSocket receive error: {err}");
                        break;
                    }
                };

                let text = match message {
                    Message::Text(text) => text.to_string(),
                    Message::Close(frame) => {
                        debug!("WebSocket close frame: {frame:?}");
                        break;
                    }
                    _ => continue,
                };

                debug!("WS recv: {}", &text[..text.len().min(200)]);

                let Ok(envelope) = serde_json::from_str::<MessageEnvelope>(&text) else {
                    debug!("could not parse message envelope, skipping");
                    continue;
                };

                match envelope.message_type.as_str() {
                    "Begin" => {
                        debug!("AssemblyAI session begun");
                    }
                    "SpeechStarted" => {
                        debug!("speech detected");
                    }
                    "Turn" | "turn" => {
                        if let Ok(turn) = serde_json::from_str::<TurnMessage>(&text) {
                            let turn_order = turn.turn_order.unwrap_or(0);
                            let transcript_text =
                                turn.transcript.as_deref().unwrap_or("").to_string();
                            let is_formatted = turn.turn_is_formatted.unwrap_or(false);

                            // Don't overwrite formatted text with unformatted
                            if let Some(existing) = stored_turns.get(&turn_order) {
                                if existing.is_formatted && !is_formatted {
                                    continue;
                                }
                            }

                            stored_turns.insert(
                                turn_order,
                                StoredTurnTranscript { transcript_text, is_formatted },
                            );

                            let full = compose_transcript_from_turns(&stored_turns);
                            let _ = transcript_sender_clone.send(TranscriptUpdate::Partial(full)).await;
                        }
                    }
                    "Termination" => {
                        if !has_delivered_final {
                            has_delivered_final = true;
                            let full = compose_transcript_from_turns(&stored_turns);
                            let _ = transcript_sender_clone.send(TranscriptUpdate::Final(full)).await;
                        }
                        break;
                    }
                    "Error" | "error" => {
                        if let Ok(error_msg) = serde_json::from_str::<ErrorMessage>(&text) {
                            let description = error_msg
                                .error
                                .or(error_msg.message)
                                .unwrap_or_else(|| "unknown error".to_string());
                            let _ = transcript_sender_clone
                                .send(TranscriptUpdate::Error(description))
                                .await;
                        }
                        break;
                    }
                    other => {
                        debug!("ignoring AssemblyAI message type: {other}");
                    }
                }
            }

            // Deliver whatever we have if no explicit Termination was received
            if !has_delivered_final {
                let full = compose_transcript_from_turns(&stored_turns);
                if !full.is_empty() {
                    let _ = transcript_sender_clone.send(TranscriptUpdate::Final(full)).await;
                }
            }
        });

        Ok(Self {
            audio_sender,
            transcript_receiver,
            control_sender,
        })
    }

    /// Signals AssemblyAI to flush pending audio and deliver a final transcript.
    pub async fn request_final_transcript(&self) -> Result<(), AppError> {
        self.control_sender
            .send(ControlMessage::RequestFinalTranscript)
            .await
            .map_err(|_| AppError::AssemblyAi("session closed before ForceEndpoint".into()))
    }

    /// Returns a clone of the audio sender so a separate task can forward audio.
    pub fn audio_sender(&self) -> mpsc::Sender<Vec<u8>> {
        self.audio_sender.clone()
    }

    /// Moves the transcript receiver out of the session, replacing it with a dummy.
    pub fn take_transcript_receiver(&mut self) -> mpsc::Receiver<TranscriptUpdate> {
        let (_dummy_tx, dummy_rx) = mpsc::channel(1);
        std::mem::replace(&mut self.transcript_receiver, dummy_rx)
    }
}

fn compose_transcript_from_turns(stored_turns: &BTreeMap<i64, StoredTurnTranscript>) -> String {
    stored_turns
        .values()
        .map(|turn| turn.transcript_text.as_str())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

// ─── JSON message types ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
}

#[derive(Deserialize)]
struct MessageEnvelope {
    #[serde(rename = "type")]
    message_type: String,
}

#[derive(Deserialize)]
struct TurnMessage {
    transcript: Option<String>,
    turn_order: Option<i64>,
    #[allow(dead_code)]
    end_of_turn: Option<bool>,
    turn_is_formatted: Option<bool>,
}

#[derive(Deserialize)]
struct ErrorMessage {
    error: Option<String>,
    message: Option<String>,
}

struct StoredTurnTranscript {
    transcript_text: String,
    is_formatted: bool,
}
