/// WebSocket server — accepts one desktop client at a time and drives the full pipeline.
///
/// Message framing: JSON text frames using `ClientMessage` / `ServerMessage` from `clicky-types`.
///
/// One connection at a time: a new WebSocket connection resets all pipeline state.
/// The previous connection's tasks are not explicitly cancelled but will fail
/// when they try to send on the closed channel.
use crate::error::AppError;
use crate::handlers::{llm_pipeline, transcription};
use crate::platform;
use clicky_types::{ClientMessage, ServerMessage};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{error, info, warn};

/// Binds on `127.0.0.1:port` and accepts incoming connections.
pub async fn run(port: u16) {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("invalid address");
    let listener = TcpListener::bind(addr).await.expect("failed to bind port");
    info!("WebSocket server listening on ws://{addr}");

    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                info!("new connection from {peer_addr}");
                tokio::spawn(handle_connection(stream));
            }
            Err(err) => {
                error!("accept error: {err}");
            }
        }
    }
}

/// Handles a single WebSocket connection end-to-end.
async fn handle_connection(tcp_stream: TcpStream) {
    let ws_stream = match accept_async(tcp_stream).await {
        Ok(ws) => ws,
        Err(err) => {
            warn!("WebSocket handshake failed: {err}");
            return;
        }
    };

    let (ws_sink, mut ws_source) = ws_stream.split();

    // All `ServerMessage` values flow through this channel.
    // The forwarder task serialises them to JSON and writes them to the WebSocket.
    // The pipeline trigger also reads from a cloned sender to detect FinalTranscript.
    let (server_tx, server_rx) = mpsc::channel::<ServerMessage>(64);

    // Spawn the WebSocket forwarder
    let forwarder_handle = tokio::spawn(forward_to_websocket(server_rx, ws_sink));

    // Load API keys from environment once per connection
    let assemblyai_key = std::env::var("ASSEMBLYAI_API_KEY").ok();
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let elevenlabs_key = std::env::var("ELEVENLABS_API_KEY").ok();
    let elevenlabs_voice = std::env::var("ELEVENLABS_VOICE_ID").ok();
    let tts_enabled = elevenlabs_key.is_some();

    let platform = platform::detect();
    let http_client = reqwest::Client::new();

    // Mutable pipeline state owned by the WS receive loop
    let mut active_cancel: Option<tokio::sync::oneshot::Sender<()>> = None;
    let mut audio_tx: Option<std::sync::mpsc::SyncSender<Vec<u8>>> = None;
    let mut conversation_history: Vec<(String, String)> = Vec::new();
    let mut last_transcript: Option<String> = None;
    let mut cursor_position: (f32, f32) = (0.0, 0.0);

    // Internal channel: pipeline tasks send ServerMessages here; this loop also
    // intercepts FinalTranscript to trigger the LLM pipeline.
    let (pipeline_tx, mut pipeline_rx) = mpsc::channel::<ServerMessage>(64);

    // Relay task: copies pipeline_rx → server_tx while this loop is processing
    let server_tx_relay = server_tx.clone();
    let (relay_tx, mut relay_rx) = mpsc::channel::<ServerMessage>(64);
    let relay_handle = tokio::spawn(async move {
        while let Some(msg) = relay_rx.recv().await {
            if server_tx_relay.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Main receive loop
    loop {
        tokio::select! {
            // Incoming WebSocket message from the frontend
            ws_msg = ws_source.next() => {
                let raw_message = match ws_msg {
                    Some(Ok(Message::Text(text))) => text.to_string(),
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => continue,
                    Some(Err(err)) => {
                        warn!("WebSocket receive error: {err}");
                        break;
                    }
                };

                let client_message: ClientMessage = match serde_json::from_str(&raw_message) {
                    Ok(msg) => msg,
                    Err(err) => {
                        warn!("failed to parse ClientMessage: {err}");
                        continue;
                    }
                };

                handle_client_message(
                    client_message,
                    &mut active_cancel,
                    &mut audio_tx,
                    &mut cursor_position,
                    &assemblyai_key,
                    pipeline_tx.clone(),
                    &server_tx,
                ).await;
            }

            // Message from a pipeline task (transcription, LLM, TTS)
            pipeline_msg = pipeline_rx.recv() => {
                let Some(message) = pipeline_msg else { break };

                // Intercept FinalTranscript to trigger the LLM pipeline
                if let ServerMessage::FinalTranscript { ref text } = message {
                    let transcript = if text.is_empty() {
                        last_transcript.clone().unwrap_or_default()
                    } else {
                        text.clone()
                    };

                    last_transcript = if transcript.is_empty() {
                        None
                    } else {
                        Some(transcript.clone())
                    };

                    if !transcript.is_empty() {
                        if let Some(ref anthropic_api_key) = anthropic_key {
                            let config = llm_pipeline::LlmPipelineConfig {
                                anthropic_api_key: anthropic_api_key.clone(),
                                elevenlabs_api_key: elevenlabs_key.clone(),
                                elevenlabs_voice_id: elevenlabs_voice.clone(),
                                tts_enabled,
                            };
                            let client = http_client.clone();
                            let tx = pipeline_tx.clone();
                            let plat = platform.clone();
                            let history = conversation_history.clone();
                            let cursor = cursor_position;
                            tokio::spawn(llm_pipeline::run_llm_pipeline(
                                client, config, transcript, cursor, history, plat, tx,
                            ));
                        }
                    }
                }

                // Track LlmResponse to update conversation history
                if let ServerMessage::LlmResponse { ref spoken_text, .. } = message {
                    if let Some(ref transcript) = last_transcript {
                        conversation_history.push((transcript.clone(), spoken_text.clone()));
                        // Keep last 10 exchanges
                        if conversation_history.len() > 10 {
                            conversation_history.remove(0);
                        }
                    }
                }

                // Forward everything to the WebSocket
                let _ = relay_tx.send(message).await;
            }
        }
    }

    info!("WebSocket connection closed");
    forwarder_handle.abort();
    relay_handle.abort();
}

/// Routes a single `ClientMessage` from the frontend to the appropriate pipeline.
async fn handle_client_message(
    message: ClientMessage,
    active_cancel: &mut Option<tokio::sync::oneshot::Sender<()>>,
    audio_tx: &mut Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    cursor_position: &mut (f32, f32),
    assemblyai_key: &Option<String>,
    pipeline_tx: mpsc::Sender<ServerMessage>,
    server_tx: &mpsc::Sender<ServerMessage>,
) {
    match message {
        ClientMessage::HotkeyPressed => {
            info!("hotkey pressed");

            // Cancel any running pipeline
            if let Some(cancel) = active_cancel.take() {
                let _ = cancel.send(());
            }
            *audio_tx = None;

            let Some(api_key) = assemblyai_key.clone() else {
                let _ = server_tx
                    .send(AppError::Config("ASSEMBLYAI_API_KEY not set".into()).into_server_message())
                    .await;
                return;
            };

            let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
            *active_cancel = Some(cancel_tx);

            // Bounded sync channel for audio chunks
            let (audio_std_tx, audio_std_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(128);
            *audio_tx = Some(audio_std_tx);

            tokio::spawn(transcription::run_transcription_bridge(
                audio_std_rx,
                pipeline_tx,
                api_key,
                cancel_rx,
            ));
        }

        ClientMessage::AudioChunk { pcm16_bytes } => {
            if let Some(tx) = audio_tx.as_ref() {
                // try_send: drop chunk rather than block the WS receive loop
                let _ = tx.try_send(pcm16_bytes);
            }
        }

        ClientMessage::HotkeyReleased => {
            info!("hotkey released — waiting for final transcript");
            // Dropping audio_tx closes the std channel → transcription task drains and finalises
            *audio_tx = None;
            if let Some(cancel) = active_cancel.take() {
                let _ = cancel.send(());
            }
        }

        ClientMessage::CancelPipeline => {
            info!("pipeline cancelled by frontend");
            if let Some(cancel) = active_cancel.take() {
                let _ = cancel.send(());
            }
            *audio_tx = None;
        }

        ClientMessage::CursorPosition { x, y } => {
            *cursor_position = (x, y);
        }
    }
}

/// Serialises `ServerMessage` values to JSON and writes them to the WebSocket sink.
async fn forward_to_websocket(
    mut server_rx: mpsc::Receiver<ServerMessage>,
    mut ws_sink: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<TcpStream>,
        Message,
    >,
) {
    while let Some(message) = server_rx.recv().await {
        let json = match serde_json::to_string(&message) {
            Ok(j) => j,
            Err(err) => {
                error!("failed to serialise ServerMessage: {err}");
                continue;
            }
        };
        if ws_sink.send(Message::Text(json.into())).await.is_err() {
            break;
        }
    }
}
