//! Global hotkey manager (M7).
//!
//! Registers every configured `HotkeysConfig` entry with the `global-hotkey` crate and
//! translates pressed events into [`AppAction`]s the UI applies every frame.
//!
//! Platform reality (documented in AGENTS.md): the crate only supports X11 on Linux.
//! Native Wayland makes manager init (or every registration) fail with a typed error; the
//! app logs that once and keeps the *local* keys (overlay focused / popover hovered) as the
//! fallback — a graceful degradation, never a silent loss. An empty spec string in the
//! config means "disabled" and is skipped without an error.

use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

use crate::config::HotkeysConfig;
use crate::error::HotkeyError;

/// Everything a hotkey can do in the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppAction {
    ExportCard,
    /// Mine the whole active line as one sentence card (audio + line + translation).
    ExportSentence,
    OffsetBack,
    OffsetForward,
    ToggleLock,
    ToggleVisibility,
    OpenSubtitle,
    ClockStartPause,
    ClockSeekBack,
    ClockSeekForward,
    ToggleEditMode,
    ToggleStatus,
    ToggleDashboard,
}

impl AppAction {
    /// Human label for status/toast/dashboard text.
    pub fn label(self) -> &'static str {
        match self {
            AppAction::ExportCard => "export card",
            AppAction::ExportSentence => "export sentence",
            AppAction::OffsetBack => "offset back",
            AppAction::OffsetForward => "offset forward",
            AppAction::ToggleLock => "toggle lock",
            AppAction::ToggleVisibility => "toggle visibility",
            AppAction::OpenSubtitle => "open subtitle",
            AppAction::ClockStartPause => "clock start/pause",
            AppAction::ClockSeekBack => "clock seek back",
            AppAction::ClockSeekForward => "clock seek forward",
            AppAction::ToggleEditMode => "toggle edit mode",
            AppAction::ToggleStatus => "toggle status",
            AppAction::ToggleDashboard => "toggle dashboard",
        }
    }
}

/// Registered global hotkeys: the manager plus the id → action mapping for events.
pub struct GlobalHotkeys {
    manager: GlobalHotKeyManager,
    actions: Vec<(u32, AppAction)>,
}

/// All `(config spec, action)` pairs the app supports, in registration order.
pub fn configured_actions(config: &HotkeysConfig) -> [(&str, AppAction); 13] {
    [
        (&config.export_card, AppAction::ExportCard),
        (&config.export_sentence, AppAction::ExportSentence),
        (&config.offset_back, AppAction::OffsetBack),
        (&config.offset_forward, AppAction::OffsetForward),
        (&config.toggle_lock, AppAction::ToggleLock),
        (&config.toggle_visibility, AppAction::ToggleVisibility),
        (&config.open_subtitle, AppAction::OpenSubtitle),
        (&config.clock_start_pause, AppAction::ClockStartPause),
        (&config.clock_seek_back, AppAction::ClockSeekBack),
        (&config.clock_seek_forward, AppAction::ClockSeekForward),
        (&config.toggle_edit_mode, AppAction::ToggleEditMode),
        (&config.toggle_status, AppAction::ToggleStatus),
        (&config.toggle_dashboard, AppAction::ToggleDashboard),
    ]
}

/// `(config spec, action)` for the *local* keys — bare keys that only fire while the overlay
/// is focused (global registration is impossible for them: `global-hotkey` is X11-only, and
/// binding bare letters globally would steal them from every other app).
pub fn local_actions(config: &HotkeysConfig) -> [(&str, AppAction); 7] {
    [
        (&config.local_export, AppAction::ExportCard),
        (&config.local_export_sentence, AppAction::ExportSentence),
        (&config.local_offset_back, AppAction::OffsetBack),
        (&config.local_offset_forward, AppAction::OffsetForward),
        (&config.local_clock_start_pause, AppAction::ClockStartPause),
        (&config.local_seek_back, AppAction::ClockSeekBack),
        (&config.local_seek_forward, AppAction::ClockSeekForward),
    ]
}

/// A local keybinding: one key plus the exact modifier state it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalKey {
    pub key: egui::Key,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

impl LocalKey {
    /// True when this key is pressed *with exactly these modifiers*. Exact matching matters:
    /// the bare local `Space` must not also fire when the global `Ctrl+Alt+Space` is pressed.
    pub fn pressed(&self, input: &egui::InputState) -> bool {
        if !input.key_pressed(self.key) {
            return false;
        }
        let expected = egui::Modifiers {
            alt: self.alt,
            ctrl: self.ctrl,
            shift: self.shift,
            mac_cmd: false,
            command: self.ctrl,
        };
        input.modifiers == expected
    }
}

/// Parse one *local* key spec (`"S"`, `"Shift+S"`, `"Space"`, `"ArrowLeft"`). Empty ==
/// disabled; anything else must parse, or the config typo is a typed error the app reports.
pub fn parse_local_key(spec: &str) -> Result<Option<LocalKey>, HotkeyError> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let invalid = |reason: String| HotkeyError::Parse {
        spec: trimmed.to_owned(),
        action: "local key".to_owned(),
        reason,
    };
    let (mut shift, mut ctrl, mut alt) = (false, false, false);
    let mut key: Option<egui::Key> = None;
    for part in trimmed.split('+') {
        let part = part.trim();
        match part.to_ascii_lowercase().as_str() {
            "shift" => shift = true,
            "ctrl" | "control" => ctrl = true,
            "alt" => alt = true,
            _ => {
                let parsed = egui::Key::from_name(part)
                    .ok_or_else(|| invalid(format!("unknown key '{part}'")))?;
                if key.replace(parsed).is_some() {
                    return Err(invalid(format!("more than one key in '{trimmed}'")));
                }
            }
        }
    }
    let key = key.ok_or_else(|| invalid("no key name (only modifiers)".to_owned()))?;
    Ok(Some(LocalKey {
        key,
        shift,
        ctrl,
        alt,
    }))
}

/// Parse one config spec into a [`HotKey`]. Empty string == disabled (None). Everything
/// else must parse or the config typo is surfaced as a typed error.
pub fn parse_hotkey(spec: &str, action: AppAction) -> Result<Option<HotKey>, HotkeyError> {
    if spec.trim().is_empty() {
        return Ok(None);
    }
    spec.parse::<HotKey>()
        .map(Some)
        .map_err(|err| HotkeyError::Parse {
            spec: spec.to_owned(),
            action: action.label().to_owned(),
            reason: err.to_string(),
        })
}

impl GlobalHotkeys {
    /// Create the manager and register every configured non-empty hotkey. Registration
    /// issues are collected and returned (typed) so the caller can surface them once; a
    /// failed manager init fails the whole thing and the caller falls back to local keys.
    pub fn new(config: &HotkeysConfig) -> Result<Self, HotkeyError> {
        let manager =
            GlobalHotKeyManager::new().map_err(|err| HotkeyError::Init(err.to_string()))?;
        let mut hotkeys = Self {
            manager,
            actions: Vec::new(),
        };
        for (spec, action) in configured_actions(config) {
            let Some(hotkey) = parse_hotkey(spec, action)? else {
                continue;
            };
            let id = hotkey.id();
            hotkeys
                .manager
                .register(hotkey)
                .map_err(|err| HotkeyError::Register {
                    spec: spec.to_owned(),
                    reason: err.to_string(),
                })?;
            hotkeys.actions.push((id, action));
        }
        Ok(hotkeys)
    }

    /// Drain pressed hotkey events since the last poll. The receiver is global (crate
    /// static); anything we did not register is ignored.
    pub fn poll(&mut self) -> Vec<AppAction> {
        let mut out = Vec::new();
        while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            if event.state != HotKeyState::Pressed {
                continue;
            }
            if let Some((_, action)) = self.actions.iter().find(|(id, _)| *id == event.id) {
                out.push(*action);
            }
        }
        out
    }

    /// How many hotkeys are currently registered (status strip).
    pub fn registered_count(&self) -> usize {
        self.actions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_global_specs_all_parse() {
        let config = HotkeysConfig::default();
        for (spec, action) in configured_actions(&config) {
            let hotkey = parse_hotkey(spec, action)
                .unwrap_or_else(|err| panic!("{spec} should parse: {err}"))
                .expect("defaults are not disabled");
            // Spot-check a couple of the trickier mappings (letter keys are canonicalized
            // by the crate as `KeyS`; symbols round-trip verbatim).
            match action {
                AppAction::ExportCard => {
                    assert_eq!(hotkey.to_string(), "control+alt+KeyS");
                }
                AppAction::OffsetBack => {
                    assert_eq!(hotkey.to_string(), "control+alt+BracketLeft");
                }
                AppAction::ClockStartPause => {
                    assert_eq!(hotkey.to_string(), "control+alt+Space");
                }
                AppAction::ClockSeekBack => {
                    assert_eq!(hotkey.to_string(), "control+alt+ArrowLeft");
                }
                _ => {}
            }
        }
    }

    #[test]
    fn empty_spec_means_disabled() {
        assert_eq!(
            parse_hotkey("", AppAction::ExportCard).expect("empty is fine"),
            None
        );
        assert_eq!(
            parse_hotkey("   ", AppAction::ExportCard).expect("whitespace is fine"),
            None
        );
    }

    #[test]
    fn parse_typos_are_typed_errors_not_panics() {
        let err = parse_hotkey("Ctrl+Alt+NotAKey", AppAction::ExportCard).expect_err("typo");
        assert!(matches!(err, HotkeyError::Parse { .. }));
        assert!(err.to_string().contains("export card"));
    }

    #[test]
    fn local_bare_key_specs_parse_too() {
        // The config schema permits bare keys for the local set; global registration of
        // them is the caller's business, but they must round-trip through the parser.
        let hotkey = parse_hotkey("S", AppAction::ExportCard)
            .expect("bare S parses")
            .expect("not disabled");
        assert_eq!(hotkey.to_string(), "KeyS");
    }

    #[test]
    fn local_key_specs_resolve_to_egui_keys_and_exact_modifiers() {
        assert_eq!(
            parse_local_key("S").expect("parses"),
            Some(LocalKey {
                key: egui::Key::S,
                shift: false,
                ctrl: false,
                alt: false,
            })
        );
        assert_eq!(
            parse_local_key("Shift+S").expect("parses"),
            Some(LocalKey {
                key: egui::Key::S,
                shift: true,
                ctrl: false,
                alt: false,
            })
        );
        // Defaults the app ships: `[`/`]` are written as symbols, `Space`/arrows by name.
        assert_eq!(
            parse_local_key("[").expect("parses").map(|k| k.key),
            Some(egui::Key::OpenBracket)
        );
        assert_eq!(
            parse_local_key("Space").expect("parses").map(|k| k.key),
            Some(egui::Key::Space)
        );
        assert_eq!(
            parse_local_key("ArrowLeft").expect("parses").map(|k| k.key),
            Some(egui::Key::ArrowLeft)
        );
        // Empty = disabled, same convention as the global specs.
        assert_eq!(parse_local_key("  ").expect("empty is fine"), None);
    }

    #[test]
    fn local_key_typos_are_typed_errors() {
        for spec in ["NotAKey", "Shift", "Shift+S+D"] {
            let err = parse_local_key(spec).expect_err(spec);
            assert!(matches!(err, HotkeyError::Parse { .. }), "{spec}");
        }
        let err = parse_local_key("Nope").expect_err("typo");
        assert!(err.to_string().contains("local key"), "{err}");
    }

    /// Every default local binding must parse: a typo here would silently disable a
    /// documented bare key rather than report itself.
    #[test]
    fn default_local_bindings_all_parse() {
        let config = HotkeysConfig::default();
        let actions = local_actions(&config);
        assert_eq!(actions.len(), 7);
        for (spec, action) in actions {
            let parsed = parse_local_key(spec)
                .unwrap_or_else(|err| panic!("{action:?} spec {spec:?}: {err}"));
            assert!(parsed.is_some(), "{action:?} must be enabled by default");
        }
    }
}
