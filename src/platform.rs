//! Session/desktop environment detection (M7).
//!
//! The Linux desktop is split between X11 and native Wayland, and several platform paths
//! behave very differently on each (global hotkeys, global cursor queries, screenshots):
//!
//! - **X11 / XWayland** (`XDG_SESSION_TYPE=x11`): the full path — `global-hotkey`
//!   registration, x11rb cursor queries, xcap window capture.
//! - **native Wayland** (`XDG_SESSION_TYPE=wayland` or `WAYLAND_DISPLAY` set): global
//!   hotkeys cannot register, the protocol offers no global cursor query, and screenshots
//!   fall back to `xdg-desktop-portal` or manual-region mode. Each of those subsystems
//!   degrades with a typed error that is *logged once* at startup and shown in the status
//!   strip, never silently.
//! - **Unknown** env: we attempt the X11 paths anyway; failures are surfaced per-subsystem.
//!
//! Everything here is pure environment classification — cheap, testable, and called exactly
//! once at startup for the session-mode log line.

/// Which desktop session the app found itself in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    X11,
    Wayland,
    Unknown,
}

/// Classify `XDG_SESSION_TYPE` + `WAYLAND_DISPLAY` into a [`SessionKind`].
/// `WAYLAND_DISPLAY` counts as native Wayland only when the session type is unset or empty
/// (an X11 session can legitimately export `WAYLAND_DISPLAY` when running under XWayland).
pub fn classify_session(
    xdg_session_type: Option<&str>,
    wayland_display: Option<&str>,
) -> SessionKind {
    let xdg = xdg_session_type.unwrap_or("").trim();
    if xdg.eq_ignore_ascii_case("x11") {
        SessionKind::X11
    } else if xdg.eq_ignore_ascii_case("wayland")
        || (xdg.is_empty() && wayland_display.is_some_and(|display| !display.trim().is_empty()))
    {
        SessionKind::Wayland
    } else {
        SessionKind::Unknown
    }
}

/// Detect the session from the environment.
pub fn detect() -> SessionKind {
    classify_session(
        std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
    )
}

/// Title of the overlay window. The always-on-top enforcer matches on this
/// (`_NET_WM_NAME`), so it must match `main.rs`'s `ViewportBuilder` title exactly.
pub const OVERLAY_TITLE: &str = "MediaLingual";

/// Title of the control-room dashboard window. The skip-taskbar hint matches on this,
/// so it must match the dashboard `ViewportBuilder` title exactly.
pub const DASHBOARD_TITLE: &str = "MojiFlow — Control Room";

impl SessionKind {
    /// Short label for the startup log line and status strip.
    pub fn label(self) -> &'static str {
        match self {
            SessionKind::X11 => "x11",
            SessionKind::Wayland => "wayland",
            SessionKind::Unknown => "unknown",
        }
    }

    /// Whether `global-hotkey` registration is expected to succeed on this session.
    /// X11 only — on Wayland the manager init fails and the app falls back to local keys.
    pub fn supports_global_hotkeys(self) -> bool {
        matches!(self, SessionKind::X11)
    }

    /// Whether the overlay can query the global cursor position (hover hit-testing while
    /// click-through). X11 only today; Windows/macOS paths are compiled separately.
    pub fn supports_global_cursor(self) -> bool {
        matches!(self, SessionKind::X11)
    }
}

/// Log the session mode once at startup (M7 gate: every platform path logs its active mode
/// and degrades gracefully).
pub fn log_session(session: SessionKind) {
    match session {
        SessionKind::X11 => {
            tracing::info!(session = %session.label(), "session type: X11 — full path (global hotkeys, global cursor, window capture)")
        }
        SessionKind::Wayland => {
            tracing::warn!(
                session = %session.label(),
                "native Wayland: global hotkeys/cursor unavailable — manual-region mode + local keys; screenshots prefer xdg-desktop-portal when available"
            )
        }
        SessionKind::Unknown => {
            tracing::info!(session = %session.label(), "session type unknown — attempting X11 paths, degrading per-subsystem on failure")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x11_is_recognized_case_insensitively() {
        assert_eq!(classify_session(Some("x11"), None), SessionKind::X11);
        assert_eq!(classify_session(Some("X11"), None), SessionKind::X11);
        assert!(SessionKind::X11.supports_global_hotkeys());
        assert!(SessionKind::X11.supports_global_cursor());
    }

    #[test]
    fn wayland_is_recognized_by_session_type_or_display_env() {
        assert_eq!(
            classify_session(Some("wayland"), None),
            SessionKind::Wayland
        );
        assert_eq!(
            classify_session(Some("Wayland"), Some("wayland-0")),
            SessionKind::Wayland
        );
        assert_eq!(
            classify_session(None, Some("wayland-0")),
            SessionKind::Wayland
        );
        assert!(!SessionKind::Wayland.supports_global_hotkeys());
        assert!(!SessionKind::Wayland.supports_global_cursor());
    }

    #[test]
    fn wayland_display_does_not_override_an_explicit_x11_session() {
        // Running under XWayland still has DISPLAY and usually WAYLAND_DISPLAY exported,
        // but the session type is authoritative.
        assert_eq!(
            classify_session(Some("x11"), Some("wayland-0")),
            SessionKind::X11
        );
    }

    #[test]
    fn unknown_env_falls_back_to_attempting_x11() {
        assert_eq!(classify_session(None, None), SessionKind::Unknown);
        assert_eq!(classify_session(Some("tty"), None), SessionKind::Unknown);
        assert_eq!(classify_session(Some(""), Some("")), SessionKind::Unknown);
    }
}
