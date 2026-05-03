pub mod capture;
pub mod playback;

use clicky_types::DisplayInfo;
use std::fmt;

/// Events sent from async background tasks to the synchronous render loop.
pub enum UiEvent {
    PartialTranscript(String),
    FinalTranscript(String),
    TranscriptionError(String),
    /// All pipeline results delivered atomically — replaces the old LlmResponse +
    /// ComputerUseCoordinate + TtsAudio triple.
    PipelineReady {
        spoken_text: String,
        /// Raw MP3 bytes from Deepgram TTS.  Empty if TTS was disabled or failed.
        mp3_bytes: Vec<u8>,
        /// Cursor target: CU result if found, POINT-tag fallback otherwise, or None.
        coordinate: Option<(f64, f64)>,
        display_infos: Vec<DisplayInfo>,
    },
    PipelineError(String),
    /// TTS synthesis failed — caller should fall back to espeak-ng.
    TtsError(String),
}

/// Audio subsystem errors. All are recoverable — the app continues without audio.
#[derive(Debug)]
pub enum AudioError {
    NoDevice(String),
    StreamError(String),
    PlaybackError(String),
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioError::NoDevice(msg) => write!(f, "No audio device: {}", msg),
            AudioError::StreamError(msg) => write!(f, "Audio stream error: {}", msg),
            AudioError::PlaybackError(msg) => write!(f, "Playback error: {}", msg),
        }
    }
}
