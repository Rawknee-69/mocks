/// Deepgram Nova-3 real-time streaming transcription via WebSocket.
///
/// Drop-in replacement for the former AssemblyAI implementation.
/// The public types (`TranscriptUpdate`, `StreamingTranscriptionSession`) and
/// method signatures are identical so `handlers/transcription.rs` needs no changes.
///
/// Flow:
/// 1. Open a WebSocket to Deepgram with auth in the URL query-string.
/// 2. Stream PCM16 audio frames as binary messages.
/// 3. Receive JSON transcription results — `speech_final: true` marks a sentence end.
/// 4. On hotkey release, send `{"type":"CloseStream"}` and wait for the server to
///    flush remaining words and close the connection.
use crate::error::AppError;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, warn};

const DEEPGRAM_WS_URL: &str = "wss://api.deepgram.com/v1/listen";
const AUDIO_SAMPLE_RATE: u32 = 16000;
const AUDIO_CHANNELS: u32 = 1;

/// Incremental transcription events emitted by the session.
#[derive(Debug, Clone)]
pub enum TranscriptUpdate {
    Partial(String),
    Final(String),
    Error(String),
}

enum ControlMessage {
    CloseStream,
}

/// Manages a single Deepgram streaming transcription session.
///
/// Send PCM16 audio via `audio_sender()`. Receive transcript updates from
/// `transcript_receiver`. Call `request_final_transcript()` on hotkey release.
pub struct StreamingTranscriptionSession {
    audio_sender: mpsc::Sender<Vec<u8>>,
    pub transcript_receiver: mpsc::Receiver<TranscriptUpdate>,
    control_sender: mpsc::Sender<ControlMessage>,
}

impl StreamingTranscriptionSession {
    /// Connects to Deepgram and starts background audio/transcript tasks.
    pub async fn start(api_key: &str) -> Result<Self, AppError> {
        let websocket_url = format!(
            "{}?model=nova-3&encoding=linear16&sample_rate={}&channels={}&punctuate=true&interim_results=true&utterance_end_ms=1500&endpointing=400&vad_events=true&smart_format=true",
            DEEPGRAM_WS_URL, AUDIO_SAMPLE_RATE, AUDIO_CHANNELS,
        );

        let request = tokio_tungstenite::tungstenite::http::Request::builder()
            .uri(&websocket_url)
            .header("Host", "api.deepgram.com")
            .header("Authorization", format!("Token {api_key}"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                tokio_tungstenite::tungstenite::handshake::client::generate_key(),
            )
            .body(())
            .map_err(|err| AppError::Transcription(format!("request builder error: {err}")))?;

        let (websocket_stream, _response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|err| AppError::Transcription(format!("WebSocket connect error: {err}")))?;

        debug!("connected to Deepgram WebSocket");

        let (mut websocket_writer, mut websocket_reader) = websocket_stream.split();

        let (audio_sender, mut audio_receiver) = mpsc::channel::<Vec<u8>>(64);
        let (transcript_sender, transcript_receiver) = mpsc::channel::<TranscriptUpdate>(32);
        let (control_sender, mut control_receiver) = mpsc::channel::<ControlMessage>(4);
        let close_stream_sent = Arc::new(AtomicBool::new(false));
        let close_stream_sent_reader = Arc::clone(&close_stream_sent);

        // Audio sender task: forwards PCM16 data and handles CloseStream signal
        tokio::spawn(async move {
            debug!("Deepgram audio sender task started");
            loop {
                tokio::select! {
                    biased;
                    audio_data = audio_receiver.recv() => {
                        match audio_data {
                            Some(data) => {
                                if let Err(err) = websocket_writer
                                    .send(Message::Binary(data.into()))
                                    .await
                                {
                                    error!("Deepgram WebSocket send error: {err}");
                                    break;
                                }
                            }
                            None => {
                                debug!("audio channel closed, stopping Deepgram sender");
                                break;
                            }
                        }
                    }
                    control_msg = control_receiver.recv() => {
                        match control_msg {
                            Some(ControlMessage::CloseStream) => {
                                debug!("sending CloseStream to Deepgram");
                                close_stream_sent.store(true, Ordering::Release);
                                let close_msg = serde_json::json!({"type": "CloseStream"});
                                if let Err(err) = websocket_writer
                                    .send(Message::Text(close_msg.to_string().into()))
                                    .await
                                {
                                    error!("Deepgram CloseStream send error: {err}");
                                }
                            }
                            None => {
                                debug!("control channel closed, stopping Deepgram sender");
                                break;
                            }
                        }
                    }
                }
            }
            debug!("Deepgram audio sender task ended");
        });

        // Transcript receiver task: parses incoming Deepgram JSON messages
        let transcript_sender_clone = transcript_sender.clone();
        tokio::spawn(async move {
            let mut accumulated_finals: Vec<String> = Vec::new();
            let mut has_delivered_final = false;

            while let Some(message_result) = websocket_reader.next().await {
                let got_close = close_stream_sent_reader.load(Ordering::Acquire);
                let message = match message_result {
                    Ok(msg) => msg,
                    Err(err) => {
                        warn!("Deepgram WebSocket receive error: {err}");
                        break;
                    }
                };

                let text = match message {
                    Message::Text(t) => t.to_string(),
                    Message::Close(frame) => {
                        debug!("Deepgram close frame: {frame:?}");
                        break;
                    }
                    _ => continue,
                };

                debug!("Deepgram WS recv: {}", &text[..text.len().min(200)]);

                // Check for metadata/info messages first
                if let Ok(envelope) = serde_json::from_str::<MessageEnvelope>(&text) {
                    match envelope.message_type.as_deref() {
                        Some("Metadata") | Some("SpeechStarted") => {
                            debug!("Deepgram info message: {:?}", envelope.message_type);
                            continue;
                        }
                        Some("UtteranceEnd") => {
                            debug!("Deepgram UtteranceEnd — flushing accumulated finals");
                            if got_close && !accumulated_finals.is_empty() {
                                let full = accumulated_finals.join(" ");
                                debug!("Deepgram emitting Final after UtteranceEnd: '{}'", &full[..full.len().min(100)]);
                                has_delivered_final = true;
                                let _ = transcript_sender_clone
                                    .send(TranscriptUpdate::Final(full))
                                    .await;
                                break;
                            }
                            continue;
                        }
                        Some("Error") => {
                            let description = envelope
                                .description
                                .unwrap_or_else(|| "unknown Deepgram error".into());
                            let _ = transcript_sender_clone
                                .send(TranscriptUpdate::Error(description))
                                .await;
                            break;
                        }
                        _ => {}
                    }
                }

                // Parse as a transcription result
                let Ok(result) = serde_json::from_str::<TranscriptionResult>(&text) else {
                    debug!("Deepgram: could not parse as transcription result, skipping");
                    continue;
                };

                let transcript_text = result
                    .channel
                    .alternatives
                    .into_iter()
                    .next()
                    .map(|alt| alt.transcript)
                    .unwrap_or_default();

                // Skip empty results — Deepgram sometimes sends speech_final=true with
                // an empty transcript for silence segments. We must NOT treat these as
                // the authoritative final result.
                if transcript_text.is_empty() {
                    if result.speech_final {
                        debug!("Deepgram empty speech_final — ignoring (silence segment)");
                    }
                    continue;
                }

                if result.is_final {
                    accumulated_finals.push(transcript_text);
                    let full = accumulated_finals.join(" ");

                    if result.speech_final {
                        if got_close {
                            // CloseStream was already sent and this is a real speech_final
                            // with content — emit Final immediately.
                            debug!("Deepgram speech_final after CloseStream — emitting Final");
                            has_delivered_final = true;
                            let _ = transcript_sender_clone
                                .send(TranscriptUpdate::Final(full))
                                .await;
                            break;
                        }
                        // speech_final before CloseStream: this is a natural sentence boundary.
                        // Don't break — the user might still be speaking. Instead, send a
                        // Partial so the UI can show progress. The definitive Final will be
                        // emitted after CloseStream + speech_final or when the WS closes.
                        debug!("Deepgram speech_final — sending partial, waiting for more");
                        let _ = transcript_sender_clone
                            .send(TranscriptUpdate::Partial(full))
                            .await;
                    } else {
                        let _ = transcript_sender_clone
                            .send(TranscriptUpdate::Partial(full))
                            .await;
                    }
                } else {
                    // Interim result: show accumulated finals + current interim
                    let interim_full = if accumulated_finals.is_empty() {
                        transcript_text
                    } else {
                        format!("{} {}", accumulated_finals.join(" "), transcript_text)
                    };
                    let _ = transcript_sender_clone
                        .send(TranscriptUpdate::Partial(interim_full))
                        .await;
                }
            }

            // When the WebSocket closes after CloseStream, emit the accumulated final
            if !has_delivered_final {
                let full = accumulated_finals.join(" ");
                debug!("Deepgram WS closed — emitting accumulated Final: '{}'", &full[..full.len().min(100)]);
                let _ = transcript_sender_clone.send(TranscriptUpdate::Final(full)).await;
            }
        });

        Ok(Self {
            audio_sender,
            transcript_receiver,
            control_sender,
        })
    }

    /// Signals Deepgram to flush pending audio and deliver a final transcript.
    pub async fn request_final_transcript(&self) -> Result<(), AppError> {
        self.control_sender
            .send(ControlMessage::CloseStream)
            .await
            .map_err(|_| AppError::Transcription("session closed before CloseStream".into()))
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

// ─── JSON message types ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct MessageEnvelope {
    #[serde(rename = "type")]
    message_type: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct TranscriptionResult {
    #[serde(rename = "is_final")]
    is_final: bool,
    #[serde(rename = "speech_final")]
    speech_final: bool,
    channel: TranscriptionChannel,
}

#[derive(Deserialize)]
struct TranscriptionChannel {
    alternatives: Vec<TranscriptionAlternative>,
}

#[derive(Deserialize)]
struct TranscriptionAlternative {
    transcript: String,
}
