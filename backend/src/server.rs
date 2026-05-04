/// WebSocket server — accepts one desktop client at a time and drives the full pipeline.
///
/// Message framing: JSON text frames using `ClientMessage` / `ServerMessage` from `clicky-types`.
///
/// One connection at a time: a new WebSocket connection resets all pipeline state.
/// The previous connection's tasks are not explicitly cancelled but will fail
/// when they try to send on the closed channel.
use crate::api::{self, computer_use, openrouter, Provider};
use crate::error::AppError;
use crate::handlers::{llm_pipeline, transcription};
use clicky_types::{ClientMessage, DisplayInfo, ScreenshotData, ServerMessage};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Semaphore};
use tokio_tungstenite::{
    accept_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
};
use tracing::{error, info, warn};

/// How often the server sends a WebSocket Ping frame to keep TCP alive and
/// detect dead connections before the OS's 10054 / ECONNRESET fires silently.
const KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// RPM cap. One permit is consumed per LLM or Computer Use request.
const RPM_CAP: usize = 20;
/// Deepgram concurrent TTS streams cap.
const DEEPGRAM_TTS_CONCURRENT_CAP: usize = 45;

/// Binds on `127.0.0.1:port` and accepts incoming connections.
pub async fn run(port: u16) {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("invalid address");
    let listener = TcpListener::bind(addr).await.expect("failed to bind port");
    info!("WebSocket server listening on ws://{addr}");

    // One shared HTTP client for the entire server lifetime — connection-pool reuse.
    // Creating a new Client per connection wastes sockets and disables keep-alive pooling.
    let http_client: Arc<reqwest::Client> = Arc::new(reqwest::Client::new());

    // Build providers once at startup — env vars are read a single time here.
    let ai_provider_str = std::env::var("AI_PROVIDER").unwrap_or_else(|_| "openrouter".into());
    let gemini_api_key = std::env::var("GEMINI_API_KEY").ok();
    let openrouter_api_key = std::env::var("OPENROUTER_API_KEY").ok();
    let llm_model = std::env::var("LLM_MODEL").ok();
    let cu_model = std::env::var("CU_MODEL").ok();

    let llm_provider: Arc<Provider> = Arc::new(build_provider(
        &ai_provider_str,
        gemini_api_key.clone(),
        openrouter_api_key.clone(),
        llm_model,
        openrouter::DEFAULT_OPENROUTER_MODEL,
        "gemini-3-flash-preview",
    ));

    let cu_provider: Arc<Provider> = Arc::new(build_provider(
        &ai_provider_str,
        gemini_api_key.clone(),
        openrouter_api_key.clone(),
        cu_model,
        computer_use::DEFAULT_CU_MODEL,
        "gemini-3-flash-preview",
    ));

    // Fail fast with a clear message rather than silently using an empty key.
    if matches!(&*llm_provider, Provider::Gemini { api_key, .. } if api_key.is_empty()) {
        panic!("AI_PROVIDER=gemini requires GEMINI_API_KEY to be set");
    }
    if matches!(&*llm_provider, Provider::OpenRouter { api_key, .. } if api_key.is_empty()) {
        warn!("AI_PROVIDER=openrouter but OPENROUTER_API_KEY is empty — requests will fail");
    }

    // Global semaphores shared across all concurrent connections
    let llm_rate_limiter: Arc<Semaphore> = Arc::new(Semaphore::new(RPM_CAP));
    let tts_rate_limiter: Arc<Semaphore> = Arc::new(Semaphore::new(DEEPGRAM_TTS_CONCURRENT_CAP));

    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                info!("new connection from {peer_addr}");
                tokio::spawn(handle_connection(
                    stream,
                    Arc::clone(&llm_rate_limiter),
                    Arc::clone(&tts_rate_limiter),
                    Arc::clone(&llm_provider),
                    Arc::clone(&cu_provider),
                    Arc::clone(&http_client),
                ));
            }
            Err(err) => {
                error!("accept error: {err}");
            }
        }
    }
}

/// Constructs a `Provider` from env var values.
///
/// Centralises all `match`-on-string logic so no other function needs to know
/// about the string names of providers.
fn build_provider(
    provider_name: &str,
    gemini_api_key: Option<String>,
    openrouter_api_key: Option<String>,
    model_override: Option<String>,
    openrouter_default_model: &str,
    gemini_default_model: &str,
) -> Provider {
    match provider_name.to_ascii_lowercase().as_str() {
        "gemini" => Provider::Gemini {
            api_key: gemini_api_key.unwrap_or_default(),
            model: model_override.unwrap_or_else(|| gemini_default_model.to_string()),
        },
        _ => Provider::OpenRouter {
            api_key: openrouter_api_key.unwrap_or_default(),
            model: model_override.unwrap_or_else(|| openrouter_default_model.to_string()),
        },
    }
}

/// Handles a single WebSocket connection end-to-end.
async fn handle_connection(
    tcp_stream: TcpStream,
    llm_rate_limiter: Arc<Semaphore>,
    _tts_rate_limiter: Arc<Semaphore>,
    llm_provider: Arc<api::Provider>,
    cu_provider: Arc<api::Provider>,
    http_client: Arc<reqwest::Client>,
) {
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(32 * 1024 * 1024))
        .max_frame_size(Some(32 * 1024 * 1024));
    let ws_stream = match accept_async_with_config(tcp_stream, Some(ws_config)).await {
        Ok(ws) => ws,
        Err(err) => {
            warn!("WebSocket handshake failed: {err}");
            return;
        }
    };

    let (ws_sink, mut ws_source) = ws_stream.split();

    // All `ServerMessage` values flow through this channel.
    let (server_tx, server_rx) = mpsc::channel::<ServerMessage>(64);
    let forwarder_handle = tokio::spawn(forward_to_websocket(server_rx, ws_sink));

    // Load Deepgram API key and TTS settings from environment
    let deepgram_key = std::env::var("DEEPGRAM_API_KEY").ok();
    let deepgram_tts_voice = std::env::var("DEEPGRAM_TTS_VOICE").ok();
    let tts_enabled = deepgram_key.is_some();

    let mut active_cancel: Option<tokio::sync::oneshot::Sender<()>> = None;
    let mut audio_tx: Option<std::sync::mpsc::SyncSender<Vec<u8>>> = None;
    let mut conversation_history: Vec<(String, String)> = Vec::new();
    let mut last_transcript: Option<String> = None;
    let mut cursor_position: (f32, f32) = (0.0, 0.0);
    // Screenshots are retained across turns so RequestCUCoordinate can use them later.
    let mut last_screenshots: Option<(Vec<ScreenshotData>, Vec<DisplayInfo>)> = None;

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

    // Keepalive ticker — sends a WebSocket Ping every KEEPALIVE_INTERVAL so the OS
    // TCP stack detects dead connections promptly instead of silently sitting on them
    // until the kernel fires ECONNRESET / 10054.
    let server_tx_ping = server_tx.clone();
    let keepalive_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(KEEPALIVE_INTERVAL);
        interval.tick().await; // skip the immediate first tick
        loop {
            interval.tick().await;
            if server_tx_ping.send(ServerMessage::Ping).await.is_err() {
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
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
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
                    &mut last_screenshots,
                    &deepgram_key,
                    &http_client,
                    pipeline_tx.clone(),
                    &server_tx,
                    Arc::clone(&cu_provider),
                    Arc::clone(&llm_rate_limiter),
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

                    if transcript.is_empty() {
                        // Nothing was said (silence or mic not captured) — tell the
                        // frontend to return to Idle so it doesn't stay frozen.
                        warn!("empty transcript received — sending abort to frontend");
                        let _ = relay_tx
                            .send(
                                AppError::Transcription("no speech detected".into())
                                    .into_server_message(),
                            )
                            .await;
                    } else {
                        info!("final transcript: '{}' — launching LLM pipeline", transcript);

                        let config = llm_pipeline::LlmPipelineConfig {
                            llm_provider: Arc::clone(&llm_provider),
                            deepgram_api_key: deepgram_key.clone(),
                            deepgram_tts_voice: deepgram_tts_voice.clone(),
                            tts_enabled,
                            llm_rate_limiter: Arc::clone(&llm_rate_limiter),
                        };
                        let client = Arc::clone(&http_client);
                        let tx = pipeline_tx.clone();
                        // Stateless context: pass empty history so each request is
                        // fully determined by the current transcript + screenshot only.
                        // This eliminates non-determinism from stale conversation state
                        // and prevents context pollution from prior turns.
                        let history: Vec<(String, String)> = Vec::new();
                        let (screenshots, display_infos) =
                            last_screenshots.clone().unwrap_or_default();
                        tokio::spawn(async move {
                            llm_pipeline::run_llm_pipeline(
                                (*client).clone(),
                                config,
                                transcript,
                                screenshots,
                                display_infos,
                                history,
                                tx,
                            )
                            .await;
                        });
                    }
                }

                // Track PipelineReady to update conversation history (kept for future use)
                if let ServerMessage::PipelineReady { ref spoken_text, .. } = message {
                    if let Some(ref transcript) = last_transcript {
                        conversation_history.push((transcript.clone(), spoken_text.clone()));
                        // Keep last 10 exchanges
                        if conversation_history.len() > 10 {
                            conversation_history.remove(0);
                        }
                    }
                }

                // Ping frames are an internal keepalive signal, not a user message.
                if let ServerMessage::Ping = message {
                    let _ = relay_tx.send(message).await;
                    continue;
                }

                // Forward everything else to the WebSocket
                let _ = relay_tx.send(message).await;
            }
        }
    }

    info!("WebSocket connection closed");
    forwarder_handle.abort();
    relay_handle.abort();
    keepalive_handle.abort();
}

/// Routes a single `ClientMessage` from the frontend to the appropriate pipeline.
async fn handle_client_message(
    message: ClientMessage,
    active_cancel: &mut Option<tokio::sync::oneshot::Sender<()>>,
    audio_tx: &mut Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    cursor_position: &mut (f32, f32),
    last_screenshots: &mut Option<(Vec<ScreenshotData>, Vec<DisplayInfo>)>,
    deepgram_key: &Option<String>,
    http_client: &Arc<reqwest::Client>,
    pipeline_tx: mpsc::Sender<ServerMessage>,
    server_tx: &mpsc::Sender<ServerMessage>,
    cu_provider: Arc<api::Provider>,
    llm_rate_limiter: Arc<Semaphore>,
) {
    match message {
        ClientMessage::HotkeyPressed => {
            info!("[HOTKEY] pressed — starting STT session");

            if let Some(cancel) = active_cancel.take() {
                let _ = cancel.send(());
            }
            *audio_tx = None;

            let Some(api_key) = deepgram_key.clone() else {
                let _ = server_tx
                    .send(
                        AppError::Config("DEEPGRAM_API_KEY not set".into())
                            .into_server_message(),
                    )
                    .await;
                return;
            };

            let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
            *active_cancel = Some(cancel_tx);

            let (audio_std_tx, audio_std_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(128);
            *audio_tx = Some(audio_std_tx);

            tokio::spawn(transcription::run_transcription_bridge(
                audio_std_rx,
                pipeline_tx,
                (**http_client).clone(),
                api_key,
                cancel_rx,
            ));
        }

        ClientMessage::AudioChunk { pcm16_bytes } => {
            if let Some(tx) = audio_tx.as_ref() {
                let _ = tx.try_send(pcm16_bytes);
            }
        }

        ClientMessage::HotkeyReleased => {
            info!("[HOTKEY] released — draining audio, waiting for final transcript");
            *audio_tx = None;
            if let Some(cancel) = active_cancel.take() {
                let _ = cancel.send(());
            }
        }

        ClientMessage::CancelPipeline => {
            info!("[PIPELINE] cancelled by frontend");
            if let Some(cancel) = active_cancel.take() {
                let _ = cancel.send(());
            }
            *audio_tx = None;
        }

        ClientMessage::CursorPosition { x, y } => {
            *cursor_position = (x, y);
        }

        ClientMessage::Screenshots { screenshots, display_infos } => {
            info!(
                "[SCREENSHOTS] received {} screenshots from frontend (displays: {})",
                screenshots.len(),
                display_infos.len()
            );
            for (i, d) in display_infos.iter().enumerate() {
                info!(
                    "  display[{i}]: origin=({:.0},{:.0}) logical={:.0}x{:.0} ss={}x{} cursor={}",
                    d.global_origin_x, d.global_origin_y,
                    d.display_width_points, d.display_height_points,
                    d.screenshot_width_pixels, d.screenshot_height_pixels,
                    d.is_cursor_display,
                );
            }
            *last_screenshots = Some((screenshots, display_infos));
        }

        ClientMessage::RequestCUCoordinate { intent, screenshot_index } => {
            info!("[CU] RequestCUCoordinate — label='{}' display_index={}", intent.label, screenshot_index);

            let Some((ref screenshots, ref display_infos)) = *last_screenshots else {
                warn!("[CU] no screenshots available, sending None");
                let _ = server_tx.send(ServerMessage::CUCoordinate { coordinate: None }).await;
                return;
            };

            let Some(display_info) = display_infos.get(screenshot_index) else {
                warn!("[CU] screenshot_index {screenshot_index} out of range");
                let _ = server_tx.send(ServerMessage::CUCoordinate { coordinate: None }).await;
                return;
            };

            let Some(screenshot) = screenshots.get(screenshot_index) else {
                warn!("[CU] screenshot_index {screenshot_index} out of range");
                let _ = server_tx.send(ServerMessage::CUCoordinate { coordinate: None }).await;
                return;
            };

            let jpeg_data = screenshot.jpeg_data.clone();
            let display_w = display_info.display_width_points;
            let display_h = display_info.display_height_points;
            let origin_x = display_info.global_origin_x;
            let origin_y = display_info.global_origin_y;
            let label = intent.label.clone();

            info!("[CU] launching detection: '{}' on {:.0}x{:.0} display at ({:.0},{:.0})",
                label, display_w, display_h, origin_x, origin_y);

            let http = Arc::clone(http_client);
            let server_tx_clone = server_tx.clone();
            tokio::spawn(async move {
                info!("[CU] detection started for '{label}'");
                let result = computer_use::detect_element_location(
                    &http,
                    cu_provider,
                    &jpeg_data,
                    &label,
                    display_w,
                    display_h,
                    llm_rate_limiter,
                )
                .await;

                let coordinate = result.and_then(|coord| {
                    let gx = origin_x + coord.display_local_x;
                    let gy = origin_y + coord.display_local_y;
                    info!("[CU] detection result: local=({:.1},{:.1}) global=({gx:.1},{gy:.1})",
                        coord.display_local_x, coord.display_local_y);

                    // Backend-side screen-centre hallucination guard:
                    // Reject if the result is within 3% of the display centre.
                    let cx = origin_x + display_w / 2.0;
                    let cy = origin_y + display_h / 2.0;
                    let threshold = display_w.min(display_h) * 0.03;
                    let dx = gx - cx;
                    let dy = gy - cy;
                    if (dx * dx + dy * dy).sqrt() < threshold {
                        warn!("[CU] ({gx:.0},{gy:.0}) matches screen centre ({cx:.0},{cy:.0}) — rejecting hallucination");
                        return None;
                    }

                    // Reject coordinates outside display bounds.
                    if gx < origin_x || gy < origin_y
                        || gx > origin_x + display_w
                        || gy > origin_y + display_h
                    {
                        warn!("[CU] ({gx:.0},{gy:.0}) outside display bounds — rejecting");
                        return None;
                    }

                    Some((gx, gy))
                });

                if coordinate.is_none() {
                    warn!("[CU] detection returned no element for '{label}'");
                }

                info!("[CU] sending CUCoordinate to frontend: {:?}", coordinate);
                let send_result = server_tx_clone
                    .send(ServerMessage::CUCoordinate { coordinate })
                    .await;
                if send_result.is_err() {
                    warn!("[CU] failed to send CUCoordinate — frontend channel closed");
                }
            });
        }
    }
}

/// Serialises `ServerMessage` values to JSON and writes them to the WebSocket sink.
///
/// `ServerMessage::Ping` is converted to a WebSocket Ping frame (no JSON encoding).
async fn forward_to_websocket(
    mut server_rx: mpsc::Receiver<ServerMessage>,
    mut ws_sink: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<TcpStream>,
        Message,
    >,
) {
    while let Some(message) = server_rx.recv().await {
        // Ping is a transport keepalive — send as a native WS Ping, not JSON.
        if let ServerMessage::Ping = message {
            if ws_sink.send(Message::Ping(vec![].into())).await.is_err() {
                break;
            }
            continue;
        }

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
