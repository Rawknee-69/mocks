use clicky_types::{ErrorCode, ServerMessage};
use thiserror::Error;

/// Application-level errors for the backend pipeline.
///
/// Every subsystem maps to one variant so callers can use `?` throughout
/// and let the error bubble to the WebSocket connection handler, which
/// converts it to a `ServerMessage::Error` and sends it to the frontend.
#[derive(Debug, Error)]
pub enum AppError {
    #[error("AssemblyAI error: {0}")]
    AssemblyAi(String),

    #[error("Claude API error: {0}")]
    Claude(String),

    #[error("ElevenLabs TTS error: {0}")]
    ElevenLabs(String),

    #[error("Computer Use API error: {0}")]
    ComputerUse(String),

    #[error("WebSocket error: {0}")]
    Websocket(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Screenshot error: {0}")]
    Screenshot(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl AppError {
    /// Maps this error to its `ErrorCode` category for the frontend.
    pub fn error_code(&self) -> ErrorCode {
        match self {
            AppError::AssemblyAi(_) => ErrorCode::Transcription,
            AppError::Claude(_) => ErrorCode::LlmPipeline,
            AppError::ElevenLabs(_) => ErrorCode::Tts,
            AppError::ComputerUse(_) => ErrorCode::ComputerUse,
            AppError::Screenshot(_) => ErrorCode::Screenshot,
            AppError::Websocket(_)
            | AppError::Config(_)
            | AppError::Io(_)
            | AppError::Internal(_) => ErrorCode::Internal,
        }
    }

    /// Converts this error into a `ServerMessage::Error` ready to send over the WebSocket.
    pub fn into_server_message(self) -> ServerMessage {
        ServerMessage::Error {
            code: self.error_code(),
            message: self.to_string(),
        }
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for AppError {
    fn from(err: tokio_tungstenite::tungstenite::Error) -> Self {
        AppError::Websocket(err.to_string())
    }
}
