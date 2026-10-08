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
pub fn configured_actions(config: &HotkeysConfig) -> [(&str, AppAction); 12] {
    [
        (&config.export_card, AppAction::ExportCard),
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
}
