//! Typed error hierarchy. `thiserror` inside library modules; `anyhow` only at the app
//! boundary (`main.rs`).
//!
//! Type-complete up front: error variants for subsystems that land in later milestones are
//! declared now so every module shares one hierarchy instead of churning.
#![allow(dead_code)]

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, MlError>;

/// Top-level typed error for every subsystem. Each variant is surfaced to the UI as a toast
/// or status-strip entry; nothing is silently swallowed.
///
/// The enum is declared type-complete up front so subsystem modules can adopt it as they
/// land; variants not yet constructed are expected until their milestone completes.
#[allow(dead_code, clippy::large_enum_variant)]
#[derive(Debug, thiserror::Error)]
pub enum MlError {
    #[error("config error: {0}")]
    Config(#[from] ConfigError),

    #[error("font error: {0}")]
    Font(#[from] FontError),

    #[error("subtitle error: {0}")]
    Subtitles(#[from] SubtitleError),

    #[error("clock error: {0}")]
    Clock(#[from] ClockError),

    #[error("dictionary error: {0}")]
    Dictionary(#[from] DictionaryError),

    #[error("capture error: {0}")]
    Capture(#[from] CaptureError),

    #[error("audio error: {0}")]
    Audio(#[from] AudioError),

    #[error("anki error: {0}")]
    Anki(#[from] AnkiError),

    #[error("speech-to-text error: {0}")]
    Stt(#[from] SttError),

    #[error("window tracking error: {0}")]
    Window(#[from] WindowError),

    #[error("channel closed while sending {0}")]
    ChannelClosed(&'static str),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config at {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse config at {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid config: {0}")]
    Invalid(String),
    #[error("failed to write config at {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum FontError {
    #[error("no CJK-capable font found (probed {probed} system paths and the embedded fallback)")]
    NoCjkFont { probed: usize },
    #[error("failed to read font {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("font file {path} is not a parseable TTF/OTF/TTC collection: {message}")]
    Parse { path: String, message: String },
}

#[derive(Debug, thiserror::Error)]
pub enum SubtitleError {
    #[error("failed to read subtitle file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("no cues found in {path}")]
    EmptyTrack { path: PathBuf },
    #[error("{path}: {line}: malformed cue skipped: {reason}")]
    MalformedCue {
        path: PathBuf,
        line: usize,
        reason: String,
    },
    #[error("unsupported subtitle format for {path}")]
    UnsupportedFormat { path: PathBuf },
}

#[derive(Debug, thiserror::Error)]
pub enum ClockError {
    #[error("clock source {component} unavailable: {reason}")]
    Unavailable { component: String, reason: String },
    #[error("mpv IPC socket {path} not found; start mpv with --input-ipc-server={path}")]
    MpvSocketMissing { path: PathBuf },
    #[error("mpv IPC I/O: {0}")]
    MpvIo(String),
    #[error("MPRIS: {0}")]
    Mpris(String),
    #[error("clock transport closed")]
    Closed,
}

#[derive(Debug, thiserror::Error)]
pub enum DictionaryError {
    #[error("dictionary database missing at {path}; run `cargo xtask build-dict`")]
    MissingDatabase { path: PathBuf },
    #[error("failed to open dictionary {path}: {source}")]
    Open {
        path: PathBuf,
        source: rusqlite::Error,
    },
    #[error("dictionary query failed: {0}")]
    Query(#[from] rusqlite::Error),
    #[error("tokenizer: {0}")]
    Tokenizer(String),
    #[error("no dictionary entry for '{0}'")]
    NotFound(String),
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("no capture target selected; pick a window or a manual region in settings")]
    NoTarget,
    #[error("window matching '{0}' was not found (it may have closed)")]
    WindowGone(String),
    #[error("screen capture failed: {0}")]
    Screen(String),
    #[error("JPEG encoding failed: {0}")]
    Encode(String),
    #[error("platform not supported for window capture: {0}")]
    Unsupported(&'static str),
}

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no audio output device available for loopback capture")]
    NoDevice,
    #[error("audio input unavailable: {0}")]
    InputUnavailable(String),
    #[error("selected audio device '{0}' disappeared; reconnecting")]
    DeviceLost(String),
    #[error("audio stream error: {0}")]
    Stream(String),
    #[error("requested audio range is older than the {buffer_len_s:.1}s ring buffer (cue is stale); replay the line and export again")]
    TooOld { buffer_len_s: f32 },
    #[error("no audio samples captured in the requested range")]
    EmptyRange,
    #[error("MP3 encoding failed: {0}")]
    Encode(String),
}

#[derive(Debug, thiserror::Error)]
pub enum AnkiError {
    #[error("AnkiConnect unreachable at {url}: {reason}")]
    Unreachable { url: String, reason: String },
    #[error("AnkiConnect returned error {code}: {message}")]
    Remote { code: i64, message: String },
    #[error("AnkiConnect response malformed: {0}")]
    Malformed(String),
    #[error("duplicate note (duplicate scope: {0})")]
    Duplicate(String),
    #[error("field mapping error: {0}")]
    FieldMapping(String),
}

#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("STT backend '{0}' is not available in this build")]
    BackendDisabled(&'static str),
    #[error("STT endpoint {url}: {reason}")]
    Endpoint { url: String, reason: String },
    #[error("STT inference failed: {0}")]
    Inference(String),
    #[error("audio capture needed for STT but unavailable: {0}")]
    NoAudio(String),
}

#[derive(Debug, thiserror::Error)]
pub enum WindowError {
    #[error("platform window tracking unavailable: {0}")]
    Unsupported(&'static str),
    #[error("X11 connection failed: {0}")]
    X11(String),
    #[error("Win32 call failed: {0}")]
    Win32(String),
    #[error(
        "global cursor query unavailable on this session type ({0}); using manual-region fallback"
    )]
    CursorQueryUnavailable(&'static str),
}
