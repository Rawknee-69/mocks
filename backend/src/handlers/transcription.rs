/// Transcription bridge — connects Deepgram WebSocket to the pipeline state machine.
///
/// Receives PCM16 audio from the frontend, forwards it to Deepgram Nova-3, relays
/// partial/final transcripts to the connection handler, and handles cancel/finalize.
use crate::api::deepgram::{self, TranscriptUpdate};
use crate::error::AppError;
use clicky_types::ServerMessage;
use std::sync::mpsc as std_mpsc;
use tokio::sync::mpsc as async_mpsc;
use tracing::{info, warn};

/// Minimum PCM16 bytes to buffer before forwarding to Deepgram.
const MIN_CHUNK_BYTES: usize = 3200;

/// Starts a Deepgram streaming transcription session.
///
/// `audio_rx` — channel of raw PCM16 byte chunks from the frontend adapter.
/// `server_tx` — channel to send `ServerMessage` events back to the connection task.
/// `cancel_rx` — fires when the frontend releases the hotkey.
///
/// The function drains all remaining audio after the cancel signal fires, then
/// requests a final transcript and waits up to 4000ms for it.
pub async fn run_transcription_bridge(
    audio_rx: std_mpsc::Receiver<Vec<u8>>,
    server_tx: async_mpsc::Sender<ServerMessage>,
    _http_client: reqwest::Client,
    api_key: String,
    cancel_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let mut session = match deepgram::StreamingTranscriptionSession::start(&api_key).await {
        Ok(s) => s,
        Err(err) => {
            let _ = server_tx.send(err.into_server_message()).await;
            return;
        }
    };

    info!("Deepgram transcription session started");

    let mut transcript_rx = session.take_transcript_receiver();
    let audio_sender = session.audio_sender();

    // Bridge blocking std_mpsc audio chunks into async channel
    let (async_audio_tx, mut async_audio_rx) = async_mpsc::channel::<Vec<u8>>(64);
    tokio::task::spawn_blocking(move || {
        let mut buffer = Vec::with_capacity(MIN_CHUNK_BYTES * 2);
        while let Ok(pcm16_chunk) = audio_rx.recv() {
            buffer.extend_from_slice(&pcm16_chunk);
            if buffer.len() >= MIN_CHUNK_BYTES {
                if async_audio_tx
                    .blocking_send(std::mem::take(&mut buffer))
                    .is_err()
                {
                    break;
                }
                buffer = Vec::with_capacity(MIN_CHUNK_BYTES * 2);
            }
        }
        if !buffer.is_empty() {
            let _ = async_audio_tx.blocking_send(buffer);
        }
    });

    // Transcript relay task
    let server_tx_for_transcripts = server_tx.clone();
    let (final_tx, final_rx) = tokio::sync::oneshot::channel::<()>();
    let transcript_handle = tokio::spawn(async move {
        while let Some(update) = transcript_rx.recv().await {
            let is_final = matches!(update, TranscriptUpdate::Final(_));
            let message = match update {
                TranscriptUpdate::Partial(text) => ServerMessage::PartialTranscript { text },
                TranscriptUpdate::Final(text) => ServerMessage::FinalTranscript { text },
                TranscriptUpdate::Error(message) => {
                    AppError::Transcription(message).into_server_message()
                }
            };
            if server_tx_for_transcripts.send(message).await.is_err() {
                break;
            }
            if is_final {
                let _ = final_tx.send(());
                break;
            }
        }
    });

    // Wait for cancel (hotkey released), then drain remaining audio.
    let _ = cancel_rx.await;
    let drain_result = tokio::time::timeout(
        tokio::time::Duration::from_millis(800),
        async {
            while let Some(pcm16) = async_audio_rx.recv().await {
                if audio_sender.send(pcm16).await.is_err() {
                    break;
                }
            }
        },
    )
    .await;
    if drain_result.is_err() {
        warn!("audio drain timed out after 800ms, proceeding to finalize");
    }

    info!("audio drained, requesting final transcript");
    let _ = session.request_final_transcript().await;

    match tokio::time::timeout(tokio::time::Duration::from_millis(4000), final_rx).await {
        Ok(_) => {}
        Err(_) => {
            warn!("final transcript timeout — sending empty final");
            let _ = server_tx
                .send(ServerMessage::FinalTranscript {
                    text: String::new(),
                })
                .await;
            transcript_handle.abort();
        }
    }
}
