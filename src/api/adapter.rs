/// WebSocket adapter — connects the frontend render loop to the backend pipeline server.
///
/// `ApiAdapter::connect()` spawns a background tokio task that maintains a WebSocket
/// connection to `ws://127.0.0.1:PORT`.  The render loop calls `send()` and `try_recv()`
/// every frame without blocking.
///
/// Auto-reconnect: if the connection drops the background task tries to reconnect
/// with an exponential back-off (up to 5 seconds between attempts).
use crate::audio::UiEvent;
use clicky_types::{ClientMessage, ErrorCode, ServerMessage};
use std::sync::mpsc as std_mpsc;
use tokio::sync::mpsc as async_mpsc;
use tracing::{error, info, warn};

/// The port the backend listens on — compile-time constant so the frontend
/// never needs to read `.env` or any API key files.
pub const BACKEND_PORT: u16 = 9001;

/// Non-blocking interface between the 60fps render loop and the backend server.
pub struct ApiAdapter {
    /// Send `ClientMessage` values to the background WS task.
    client_tx: async_mpsc::Sender<ClientMessage>,
    /// Receive `UiEvent` values produced by the background WS task.
    ui_event_rx: std_mpsc::Receiver<UiEvent>,
    /// Send `UiEvent` values directly from local tasks (e.g. UIA extraction).
    ui_event_tx: std_mpsc::Sender<UiEvent>,
}

impl ApiAdapter {
    /// Connects to `ws://127.0.0.1:port`, spawning the background WS task.
    ///
    /// Must be called from within a tokio runtime context.
    #[allow(dead_code)]
    pub fn connect(port: u16) -> Self {
        let (client_tx, client_rx) = async_mpsc::channel::<ClientMessage>(128);
        let (ui_event_tx, ui_event_rx) = std_mpsc::channel::<UiEvent>();

        tokio::spawn(ws_task(port, client_rx, ui_event_tx.clone()));

        Self { client_tx, ui_event_rx, ui_event_tx }
    }

    /// Queues a `ClientMessage` for delivery to the backend (non-blocking).
    pub fn send(&self, msg: ClientMessage) {
        // Silently drop on disconnect or when the channel is full — the ws_task will reconnect.
        let _ = self.client_tx.try_send(msg);
    }

    /// Returns a clone of the underlying sender for use in background tasks
    /// (e.g. forwarding audio chunks from a blocking thread).
    pub fn client_tx_clone(&self) -> async_mpsc::Sender<ClientMessage> {
        self.client_tx.clone()
    }

    /// Returns a clone of the UiEvent sender for local background tasks
    /// (e.g. UIA tree extraction) that need to post events to the render loop
    /// without going through the backend WebSocket.
    pub fn ui_event_tx_clone(&self) -> std_mpsc::Sender<UiEvent> {
        self.ui_event_tx.clone()
    }


    /// Attempts to connect to the backend with a 2-second timeout.
    ///
    /// Returns an error if the backend is unreachable within the timeout window.
    /// Must be called from within a tokio runtime context.
    pub async fn try_connect(port: u16) -> Result<Self, std::io::Error> {
        let url = format!("ws://127.0.0.1:{port}");
        let connect_future = tokio_tungstenite::connect_async(&url);
        let (ws_stream, _) = tokio::time::timeout(std::time::Duration::from_secs(2), connect_future)
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("backend at {url} did not respond within 2 seconds"),
                )
            })?
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e.to_string()))?;

        // Reuse the already-established connection rather than discarding it and opening a second one.
        let (client_tx, client_rx) = async_mpsc::channel::<ClientMessage>(128);
        let (ui_event_tx, ui_event_rx) = std_mpsc::channel::<UiEvent>();
        tokio::spawn(ws_task_with_stream(ws_stream, port, client_rx, ui_event_tx.clone()));
        Ok(Self { client_tx, ui_event_rx, ui_event_tx })
    }

    /// Returns the next `UiEvent` if one is available, without blocking.
    pub fn try_recv(&self) -> Option<UiEvent> {
        self.ui_event_rx.try_recv().ok()
    }
}

/// Background task: maintains the WebSocket connection and translates messages.
///
/// On disconnect it waits briefly then reconnects, preserving the channel handles
/// so the render loop never needs to know about reconnects.
async fn ws_task(
    port: u16,
    mut client_rx: async_mpsc::Receiver<ClientMessage>,
    ui_event_tx: std_mpsc::Sender<UiEvent>,
) {
    let url = format!("ws://127.0.0.1:{port}");
    let mut backoff_ms: u64 = 250;

    loop {
        info!("adapter: connecting to {url}");
        let (ws_stream, _) = match tokio_tungstenite::connect_async(&url).await {
            Ok(ws) => ws,
            Err(err) => {
                warn!("adapter: connection failed ({err}), retrying in {backoff_ms}ms");
                tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(5000);
                continue;
            }
        };

        backoff_ms = 250; // reset on successful connection
        info!("adapter: connected to {url}");

        if !run_ws_connection(ws_stream, &mut client_rx, &ui_event_tx).await {
            return; // channel closed — adapter was dropped
        }

        warn!("adapter: disconnected, retrying in {backoff_ms}ms");
        tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;
        backoff_ms = (backoff_ms * 2).min(5000);
    }
}

/// Variant of `ws_task` that starts with an already-open `ws_stream` (from `try_connect`)
/// then reconnects on disconnect like the normal `ws_task`.
async fn ws_task_with_stream(
    initial_stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    port: u16,
    mut client_rx: async_mpsc::Receiver<ClientMessage>,
    ui_event_tx: std_mpsc::Sender<UiEvent>,
) {
    info!("adapter: using pre-established connection");
    if !run_ws_connection(initial_stream, &mut client_rx, &ui_event_tx).await {
        return;
    }

    // Connection dropped — fall back to the normal reconnect loop
    ws_task(port, client_rx, ui_event_tx).await;
}

/// Drives a single WebSocket connection until it disconnects.
///
/// Returns `true` if we should reconnect, `false` if the adapter was dropped (channel closed).
async fn run_ws_connection(
    ws_stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    client_rx: &mut async_mpsc::Receiver<ClientMessage>,
    ui_event_tx: &std_mpsc::Sender<UiEvent>,
) -> bool {
    const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024; // 16 MiB

    let (mut ws_sink, mut ws_source) = futures_util::StreamExt::split(ws_stream);

    loop {
        tokio::select! {
            // Outgoing: client_rx → WebSocket
            outgoing = client_rx.recv() => {
                let Some(msg) = outgoing else {
                    // Channel closed — adapter was dropped, exit task
                    return false;
                };
                let json = match serde_json::to_string(&msg) {
                    Ok(j) => j,
                    Err(err) => {
                        error!("adapter: failed to serialise ClientMessage: {err}");
                        continue;
                    }
                };
                if futures_util::SinkExt::send(
                    &mut ws_sink,
                    tokio_tungstenite::tungstenite::Message::Text(json.into()),
                ).await.is_err() {
                    return true; // disconnect
                }
            }

            // Incoming: WebSocket → UiEvent
            incoming = futures_util::StreamExt::next(&mut ws_source) => {
                match incoming {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                        if text.len() > MAX_MESSAGE_BYTES {
                            warn!("adapter: oversized message ({} bytes), dropping", text.len());
                            continue;
                        }
                        match serde_json::from_str::<ServerMessage>(&text) {
                            Ok(server_msg) => {
                                if let Some(ui_event) = server_message_to_ui_event(server_msg) {
                                    // Render loop has exited → stop
                                    if ui_event_tx.send(ui_event).is_err() {
                                        return false;
                                    }
                                }
                            }
                            Err(err) => {
                                warn!("adapter: failed to parse ServerMessage: {err}");
                            }
                        }
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => {
                        return true; // disconnect
                    }
                    Some(Err(err)) => {
                        warn!("adapter: WS receive error: {err}");
                        return true;
                    }
                    Some(Ok(_)) => {} // ignore binary/ping/pong
                }
            }
        }
    }
}

/// Converts a `ServerMessage` from the backend into a `UiEvent` for the render loop.
///
/// Returns `None` for messages that don't map to a `UiEvent`
/// (e.g. `LlmTextDelta` which the render loop doesn't use yet).
fn server_message_to_ui_event(msg: ServerMessage) -> Option<UiEvent> {
    match msg {
        ServerMessage::PartialTranscript { text } => {
            Some(UiEvent::PartialTranscript(text))
        }

        ServerMessage::FinalTranscript { text } => {
            Some(UiEvent::FinalTranscript(text))
        }

        ServerMessage::LlmTextDelta { .. } => {
            // Not currently used by the render loop
            None
        }

        ServerMessage::PipelineReady { spoken_text, mp3_bytes, coordinate: _, display_infos, intent } => {
            Some(UiEvent::PipelineReadyWithIntent {
                spoken_text,
                mp3_bytes,
                intent,
                display_infos,
            })
        }

        ServerMessage::CUCoordinate { coordinate } => {
            Some(UiEvent::CUCoordinateReady { coordinate })
        }

        ServerMessage::Error { code, message } => {
            match code {
                ErrorCode::Transcription => Some(UiEvent::TranscriptionError(message)),
                ErrorCode::Tts => Some(UiEvent::TtsError(message)),
                _ => Some(UiEvent::PipelineError(message)),
            }
        }

        // Transport-level keepalive — never forwarded as a UiEvent.
        ServerMessage::Ping => None,
    }
}
