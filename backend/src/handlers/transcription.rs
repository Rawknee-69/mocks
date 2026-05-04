/// Transcription bridge — connects Deepgram WebSocket to the pipeline state machine.
///
/// Receives PCM16 audio from the frontend, forwards it to Deepgram Nova-3, relays
/// partial/final transcripts to the connection handler, and handles cancel/finalize.
///
/// Audio arriving before the Deepgram WebSocket connects is buffered and replayed
/// once the session is ready so no early speech is lost.
use crate::api::deepgram::{self, TranscriptUpdate};
use crate::error::AppError;
use clicky_types::ServerMessage;
use std::sync::mpsc as std_mpsc;
use tokio::sync::mpsc as async_mpsc;
use tracing::{debug, info, warn};

/// Minimum PCM16 bytes to buffer before forwarding to Deepgram.
const MIN_CHUNK_BYTES: usize = 3200;

/// Starts a Deepgram streaming transcription session.
///
/// `audio_rx` — channel of raw PCM16 byte chunks from the frontend adapter.
/// `server_tx` — channel to send `ServerMessage` events back to the connection task.
/// `cancel_rx` — fires when the frontend releases the hotkey.
///
/// Audio is pre-buffered during Deepgram connection so nothing is lost.
/// After the cancel signal, remaining audio is drained, then we request a final
/// transcript and wait up to 6s for it.
pub async fn run_transcription_bridge(
    audio_rx: std_mpsc::Receiver<Vec<u8>>,
    server_tx: async_mpsc::Sender<ServerMessage>,
    _http_client: reqwest::Client,
    api_key: String,
    cancel_rx: tokio::sync::oneshot::Receiver<()>,
) {
    // Start buffering audio immediately, before Deepgram connects. The blocking
    // bridge collects chunks into `async_audio_rx` which we drain into Deepgram
    // once the session is ready.
    let (async_audio_tx, mut async_audio_rx) = async_mpsc::channel::<Vec<u8>>(256);
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

    let mut session = match deepgram::StreamingTranscriptionSession::start(&api_key).await {
        Ok(s) => s,
        Err(err) => {
            let _ = server_tx.send(err.into_server_message()).await;
            return;
        }
    };

    info!("Deepgram transcription session started");

    // Replay any audio that arrived while the WebSocket was connecting.
    let mut pre_buffer_bytes: usize = 0;
    while let Ok(pcm16) = async_audio_rx.try_recv() {
        pre_buffer_bytes += pcm16.len();
        if session.audio_sender().send(pcm16).await.is_err() {
            warn!("Deepgram audio sender closed during pre-buffer replay");
            return;
        }
    }
    if pre_buffer_bytes > 0 {
        debug!("replayed {pre_buffer_bytes} pre-buffered bytes to Deepgram");
    }

    let mut transcript_rx = session.take_transcript_receiver();
    let audio_sender = session.audio_sender();

    // Forward live audio to Deepgram
    let audio_fwd_sender = audio_sender.clone();
    let audio_fwd_handle = tokio::spawn(async move {
        while let Some(pcm16) = async_audio_rx.recv().await {
            if audio_fwd_sender.send(pcm16).await.is_err() {
                break;
            }
        }
    });
    let audio_fwd_abort = audio_fwd_handle.abort_handle();

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
        tokio::time::Duration::from_millis(1500),
        async {
            audio_fwd_handle.await.ok();
        },
    )
    .await;
    if drain_result.is_err() {
        warn!("audio drain timed out after 1500ms, proceeding to finalize");
        audio_fwd_abort.abort();
    }

    info!("audio drained, requesting final transcript");
    let _ = session.request_final_transcript().await;

    match tokio::time::timeout(tokio::time::Duration::from_millis(6000), final_rx).await {
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
