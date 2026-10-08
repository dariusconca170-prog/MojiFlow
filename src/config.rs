//! Typed application config: `config.toml` sections, defaults, load/save, validation.
//!
//! Loading never crashes the app: a missing file yields defaults, and a broken file yields
//! defaults plus a [`ConfigError`] that the UI reports as a toast.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// A single parsed configuration. Every section has `#[serde(default)]` so partial files work.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub window: WindowConfig,
    pub subtitle: SubtitleConfig,
    pub clock: ClockConfig,
    pub hotkeys: HotkeysConfig,
    pub audio: AudioConfig,
    pub capture: CaptureConfig,
    pub stt: SttConfig,
    pub anki: AnkiConfig,
    pub dictionary: DictionaryConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowConfig {
    /// Overlay position and size in logical pixels, `[x, y, w, h]`.
    pub rect: [f32; 4],
    pub always_on_top: bool,
    pub show_decorations: bool,
    /// Semi-transparent backing box behind subtitle text.
    pub backing_box: bool,
    /// Whether the control-room dashboard window is open (hotkey Ctrl+Alt+D).
    pub dashboard_open: bool,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            rect: [96.0, 96.0, 1100.0, 320.0],
            always_on_top: true,
            show_decorations: false,
            backing_box: true,
            dashboard_open: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubtitleConfig {
    pub font_size: f32,
    /// `"bottom_center" | "top_center" | "custom"`
    pub position: String,
    pub custom_y: f32,
    pub show_furigana: bool,
    /// Offset step for `[` / `]` in milliseconds.
    pub offset_step_ms: i64,
    /// Persisted user offset in milliseconds (clock.effective = clock.now + offset).
    pub user_offset_ms: i64,
    pub outline: bool,
}

impl Default for SubtitleConfig {
    fn default() -> Self {
        Self {
            font_size: 34.0,
            position: "bottom_center".to_owned(),
            custom_y: 0.5,
            show_furigana: false,
            offset_step_ms: 200,
            user_offset_ms: 0,
            outline: true,
        }
    }
}

/// Which [`crate::clock::PlaybackClock`] implementation drives subtitle timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ClockSource {
    #[default]
    Manual,
    MpvIpc,
    Mpris,
    WhisperLive,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClockConfig {
    pub source: ClockSource,
    /// mpv JSON IPC endpoint. Linux: unix socket path. Windows: `\\.\pipe\...` name.
    pub mpv_socket: String,
    /// MPRIS bus name filter; empty means auto-detect the first playing player.
    pub mpris_player: String,
    /// Live STT wall-clock alignment: seconds to shift generated cues.
    pub live_cue_delay_ms: i64,
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            source: ClockSource::Manual,
            #[cfg(target_os = "linux")]
            mpv_socket: "/tmp/mpv-ipc.sock".to_owned(),
            #[cfg(target_os = "windows")]
            mpv_socket: r"\\.\pipe\mpv-ipc".to_owned(),
            mpris_player: String::new(),
            live_cue_delay_ms: 0,
        }
    }
}

/// Global hotkeys. Values are egui `Modifiers+Key` strings, e.g. `"Ctrl+Alt+S"`.
/// Bare letters are intentionally absent from the global set (see AGENTS.md).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeysConfig {
    pub export_card: String,
    pub offset_back: String,
    pub offset_forward: String,
    pub toggle_lock: String,
    pub toggle_visibility: String,
    pub open_subtitle: String,
    pub clock_start_pause: String,
    pub clock_seek_back: String,
    pub clock_seek_forward: String,
    pub toggle_edit_mode: String,
    pub toggle_status: String,
    /// Open/close the control-room dashboard window.
    pub toggle_dashboard: String,
    /// Bare keys accepted only while the overlay is focused or its popover is hovered.
    pub local_export: String,
    pub local_offset_back: String,
    pub local_offset_forward: String,
}

impl Default for HotkeysConfig {
    fn default() -> Self {
        Self {
            export_card: "Ctrl+Alt+S".to_owned(),
            offset_back: "Ctrl+Alt+[".to_owned(),
            offset_forward: "Ctrl+Alt+]".to_owned(),
            toggle_lock: "Ctrl+Alt+L".to_owned(),
            toggle_visibility: "Ctrl+Alt+H".to_owned(),
            open_subtitle: "Ctrl+Alt+O".to_owned(),
            clock_start_pause: "Ctrl+Alt+Space".to_owned(),
            clock_seek_back: "Ctrl+Alt+ArrowLeft".to_owned(),
            clock_seek_forward: "Ctrl+Alt+ArrowRight".to_owned(),
            toggle_edit_mode: "Ctrl+Alt+E".to_owned(),
            toggle_status: "Ctrl+Alt+P".to_owned(),
            toggle_dashboard: "Ctrl+Alt+D".to_owned(),
            local_export: "S".to_owned(),
            local_offset_back: "[".to_owned(),
            local_offset_forward: "]".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Device name to capture. Empty = auto-pick the first monitor/loopback source.
    pub device: String,
    /// Ring-buffer length in seconds (>= 10 recommended).
    pub buffer_seconds: f32,
    /// Padding added around a cue when exporting audio, in milliseconds.
    pub padding_ms: u32,
    pub normalize: bool,
    /// `"mp3" | "ogg"`
    pub format: String,
    pub mp3_bitrate_kbps: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            device: String::new(),
            buffer_seconds: 12.0,
            padding_ms: 200,
            normalize: false,
            format: "mp3".to_owned(),
            mp3_bitrate_kbps: 192,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
    /// Window title substring used to pick the video window. Empty = manual region.
    pub target_window_title: String,
    /// Manual region `[x, y, w, h]` used when no window matches.
    pub manual_rect: [f32; 4],
    pub max_width: u32,
    pub jpeg_quality: u8,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            target_window_title: "mpv".to_owned(),
            manual_rect: [0.0, 0.0, 1280.0, 720.0],
            max_width: 1280,
            jpeg_quality: 85,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SttBackend {
    /// In-process whisper.cpp via the `whisper` cargo feature.
    #[default]
    Disabled,
    Local,
    Http,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SttConfig {
    pub backend: SttBackend,
    /// OpenAI-compatible endpoint, e.g. `http://127.0.0.1:8000/v1/audio/transcriptions`.
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub language: String,
    /// Energy-VAD gate threshold (0..1) for chunking.
    pub vad_threshold: f32,
    pub whisper_model_path: String,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            backend: SttBackend::Disabled,
            endpoint: "http://127.0.0.1:8000/v1/audio/transcriptions".to_owned(),
            model: "whisper-1".to_owned(),
            api_key: String::new(),
            language: "ja".to_owned(),
            vad_threshold: 0.5,
            whisper_model_path: String::new(),
        }
    }
}

/// Placeholder → value templates for Anki note fields, e.g.
/// `"Expression" => "{word}"`, `"Sentence" => "{sentence}"`.
pub type FieldMapping = BTreeMap<String, String>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnkiConfig {
    pub url: String,
    pub deck: String,
    pub model: String,
    pub tags: Vec<String>,
    pub field_mapping: FieldMapping,
    /// `"none" | "deck" | "first_field" | "deck_tags"` — passed to `allowDuplicate` scope.
    pub duplicate_scope: String,
    pub connect_timeout_ms: u64,
    pub retry_interval_s: u64,
}

impl Default for AnkiConfig {
    fn default() -> Self {
        let mut field_mapping = FieldMapping::new();
        field_mapping.insert("Expression".to_owned(), "{word}".to_owned());
        field_mapping.insert("Reading".to_owned(), "{reading}".to_owned());
        field_mapping.insert("Glossary".to_owned(), "{definition}".to_owned());
        field_mapping.insert("Sentence".to_owned(), "{sentence}".to_owned());
        field_mapping.insert("Audio".to_owned(), "{audio}".to_owned());
        field_mapping.insert("Image".to_owned(), "{image}".to_owned());
        field_mapping.insert("Pitch".to_owned(), "{pitch}".to_owned());
        Self {
            url: "http://127.0.0.1:8765".to_owned(),
            deck: "Japanese::Mining".to_owned(),
            model: "Japanese Mining".to_owned(),
            tags: vec!["medialingual".to_owned()],
            field_mapping,
            duplicate_scope: "deck".to_owned(),
            connect_timeout_ms: 2_000,
            retry_interval_s: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DictionaryConfig {
    pub jmdict_path: PathBuf,
    pub pitch_path: PathBuf,
    pub frequency_path: PathBuf,
    pub lru_capacity: usize,
}

impl Default for DictionaryConfig {
    fn default() -> Self {
        Self {
            jmdict_path: PathBuf::from("assets/dict/jmdict.sqlite"),
            pitch_path: PathBuf::from("assets/dict/pitch.sqlite"),
            frequency_path: PathBuf::from("assets/dict/frequency.sqlite"),
            lru_capacity: 2048,
        }
    }
}

/// Result of a load attempt: always a usable [`Config`], plus any issues to show in the UI.
#[derive(Debug)]
pub struct LoadedConfig {
    pub config: Config,
    pub issues: Vec<ConfigError>,
    pub path: PathBuf,
}

impl Config {
    /// Default config file path for this app (`$XDG_CONFIG_HOME/medialingual/config.toml`).
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "medialingual")
            .map(|d| d.config_dir().join("config.toml"))
    }

    /// Load config, never failing hard. Missing file → defaults, no issues.
    /// Parse/validation failures → defaults + reported issues.
    pub fn load(path: &Path) -> LoadedConfig {
        let mut issues = Vec::new();
        let config = match std::fs::read_to_string(path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(err) => {
                issues.push(ConfigError::Read {
                    path: path.to_path_buf(),
                    source: err,
                });
                Config::default()
            }
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(parsed) => {
                    let mut validator = Validator::default();
                    validator.check(&parsed);
                    issues.extend(validator.issues);
                    parsed
                }
                Err(err) => {
                    issues.push(ConfigError::Parse {
                        path: path.to_path_buf(),
                        message: err.to_string(),
                    });
                    Config::default()
                }
            },
        };
        LoadedConfig {
            config,
            issues,
            path: path.to_path_buf(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let text =
            toml::to_string_pretty(self).map_err(|err| ConfigError::Invalid(err.to_string()))?;
        std::fs::write(path, text).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// Collects validation issues for a [`Config`]. Empty means the config is valid.
#[derive(Debug, Default)]
pub struct Validator {
    pub issues: Vec<ConfigError>,
}

/// Placeholders accepted in `anki.field_mapping` templates.
pub const KNOWN_PLACEHOLDERS: [&str; 11] = [
    "word",
    "reading",
    "definition",
    "sentence",
    "sentence_furigana",
    "audio",
    "image",
    "pitch",
    "pitch_svg",
    "frequency",
    "source",
];

/// Validate a field template: every `{name}` must be a known placeholder, and braces balanced.
pub fn validate_field_template(template: &str) -> std::result::Result<(), String> {
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        if let Some(escaped) = after.strip_prefix('{') {
            // `{{` is an escaped literal brace.
            rest = escaped;
            continue;
        }
        match after.find('}') {
            None => return Err(format!("unbalanced '{{' in {template:?}")),
            Some(end) => {
                let name = &after[..end];
                if name.is_empty() {
                    return Err(format!("empty placeholder '{{}}' in {template:?}"));
                }
                if !KNOWN_PLACEHOLDERS.contains(&name) {
                    return Err(format!(
                        "unknown placeholder '{{{name}}}'; known: {}",
                        KNOWN_PLACEHOLDERS.join(", ")
                    ));
                }
                rest = &after[end + 1..];
            }
        }
    }
    if rest.contains('}') {
        return Err(format!("unbalanced '}}' in {template:?}"));
    }
    Ok(())
}

impl Validator {
    pub fn check(&mut self, config: &Config) {
        if config.subtitle.font_size < 8.0 || config.subtitle.font_size > 160.0 {
            self.push(format!(
                "subtitle.font_size {} out of range 8..=160",
                config.subtitle.font_size
            ));
        }
        if config.subtitle.offset_step_ms == 0 {
            self.push("subtitle.offset_step_ms must be non-zero".to_owned());
        }
        if config.audio.buffer_seconds < 1.0 {
            self.push("audio.buffer_seconds must be >= 1".to_owned());
        }
        if config.audio.format != "mp3" && config.audio.format != "ogg" {
            self.push(format!(
                "audio.format '{}' must be \"mp3\" or \"ogg\"",
                config.audio.format
            ));
        }
        if !(32..=320).contains(&config.audio.mp3_bitrate_kbps) {
            self.push(format!(
                "audio.mp3_bitrate_kbps {} out of range 32..=320",
                config.audio.mp3_bitrate_kbps
            ));
        }
        if !(1..=100).contains(&config.capture.jpeg_quality) {
            self.push(format!(
                "capture.jpeg_quality {} out of range 1..=100",
                config.capture.jpeg_quality
            ));
        }
        if config.capture.max_width == 0 {
            self.push("capture.max_width must be >= 1".to_owned());
        }
        if !(0.0..=1.0).contains(&config.stt.vad_threshold) {
            self.push(format!(
                "stt.vad_threshold {} out of range 0..=1",
                config.stt.vad_threshold
            ));
        }
        if config.dictionary.lru_capacity == 0 {
            self.push("dictionary.lru_capacity must be >= 1".to_owned());
        }
        if config.anki.duplicate_scope != "none"
            && config.anki.duplicate_scope != "deck"
            && config.anki.duplicate_scope != "first_field"
            && config.anki.duplicate_scope != "deck_tags"
        {
            self.push(format!(
                "anki.duplicate_scope '{}' must be one of none|deck|first_field|deck_tags",
                config.anki.duplicate_scope
            ));
        }
        for (field, template) in &config.anki.field_mapping {
            if let Err(err) = validate_field_template(template) {
                self.push(format!("anki.field_mapping[{field}]: {err}"));
            }
        }
        if !matches!(
            config.subtitle.position.as_str(),
            "bottom_center" | "top_center" | "custom"
        ) {
            self.push(format!(
                "subtitle.position '{}' must be bottom_center|top_center|custom",
                config.subtitle.position
            ));
        }
    }

    fn push(&mut self, message: String) {
        self.issues.push(ConfigError::Invalid(message));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        let mut validator = Validator::default();
        validator.check(&Config::default());
        assert!(validator.issues.is_empty(), "{:?}", validator.issues);
    }

    #[test]
    fn partial_file_loads_with_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[subtitle]\nfont_size = 42.0\n").expect("write");
        let loaded = Config::load(&path);
        assert_eq!(loaded.config.subtitle.font_size, 42.0);
        assert_eq!(loaded.config.subtitle.position, "bottom_center");
        assert!(loaded.issues.is_empty(), "{:?}", loaded.issues);
    }

    #[test]
    fn broken_file_reports_issue_not_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this is not toml = = =").expect("write");
        let loaded = Config::load(&path);
        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.issues.len(), 1);
        assert!(matches!(loaded.issues[0], ConfigError::Parse { .. }));
    }

    #[test]
    fn invalid_values_are_reported() {
        let mut config = Config::default();
        config.subtitle.font_size = 2.0;
        config.capture.jpeg_quality = 0;
        config.anki.duplicate_scope = "wat".to_owned();
        let mut validator = Validator::default();
        validator.check(&config);
        assert_eq!(validator.issues.len(), 3, "{:?}", validator.issues);
    }

    #[test]
    fn roundtrips_through_toml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let mut config = Config::default();
        config.subtitle.user_offset_ms = -400;
        config.save(&path).expect("save");
        let loaded = Config::load(&path);
        assert_eq!(loaded.config, config);
        assert!(loaded.issues.is_empty());
    }
}
