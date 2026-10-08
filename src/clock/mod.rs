//! Playback clocks: one `PlaybackClock` trait, several implementations (Section 3.5).
//!
//! There is no universal media clock, so the source is selectable in `config.toml`:
//! - [`manual::ManualClock`] — hotkey-driven start/pause/seek.
//! - [`mpv_ipc::MpvIpcClock`] — frame-accurate sync over mpv's JSON IPC socket.
//! - `MprisClock` (Linux) — lands in M7 via `zbus`.
//! - `WhisperLiveClock` — lands in M8; cues are timestamped on arrival.
//!
//! Effective subtitle time is `clock.now() + user_offset`; the offset is applied by `App`.

pub mod manual;
pub mod mpv_ipc;

use std::time::Duration;

use crate::config::{ClockConfig, ClockSource};
use crate::error::ClockError;

/// How fresh the clock's idea of playback time is — drives the status strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncState {
    /// Connected to a real playback source and updating.
    Live,
    /// Local manual time; no external source expected.
    Local,
    /// External source is configured but not delivering updates (message shown in UI).
    Stale(String),
}

/// Read-only view of playback time that the UI thread can query every frame without
/// blocking. Implementations must never do I/O inside these methods.
pub trait PlaybackClock: Send {
    /// Current media position.
    fn now(&self) -> Duration;
    fn is_playing(&self) -> bool;
    /// Short label for the status strip, e.g. `"mpv-ipc"`.
    fn source_name(&self) -> &'static str;
    fn sync_state(&self) -> SyncState;
    /// Ask any worker threads owned by this clock to stop. Default: nothing to stop.
    fn shutdown(&self) {}
}

/// Build the clock selected by config. The caller keeps its own clone of `manual` as a
/// control handle; for manual/fallback sources the boxed clock shares that same state.
/// Falls back to the manual clock with a typed error note when the requested source is
/// unavailable on this build/platform — the note is surfaced as a toast, never swallowed.
pub fn build_clock(
    config: &ClockConfig,
    manual: manual::ManualClock,
    events: crossbeam_channel::Sender<crate::app::CoreEvent>,
    repaint: std::sync::Arc<crate::app::RepaintHandle>,
) -> (Box<dyn PlaybackClock>, Option<ClockError>) {
    match config.source {
        ClockSource::Manual => (Box::new(manual) as Box<dyn PlaybackClock>, None),
        ClockSource::MpvIpc => {
            let clock = mpv_ipc::MpvIpcClock::spawn(config.mpv_socket.clone(), events, repaint);
            (Box::new(clock), None)
        }
        ClockSource::Mpris => (
            Box::new(manual),
            Some(ClockError::Unavailable {
                component: "mpris".to_owned(),
                reason: "MPRIS clock arrives in milestone M7; using manual clock".to_owned(),
            }),
        ),
        ClockSource::WhisperLive => (
            Box::new(manual),
            Some(ClockError::Unavailable {
                component: "whisper-live".to_owned(),
                reason: "live STT clock arrives in milestone M8; using manual clock".to_owned(),
            }),
        ),
    }
}

/// `effective = clock.now() + offset_ms` (clamped at zero, so a large negative offset
/// never underflows).
pub fn effective_time(clock: &dyn PlaybackClock, offset_ms: i64) -> Duration {
    let now_ms = i64::try_from(clock.now().as_millis()).unwrap_or(i64::MAX);
    let effective = now_ms.saturating_add(offset_ms);
    if effective <= 0 {
        Duration::ZERO
    } else {
        Duration::from_millis(effective as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_time_applies_and_clamps_offset() {
        let clock = manual::ManualClock::new_at(Duration::from_millis(1_000), true);
        assert_eq!(effective_time(&clock, 0), Duration::from_millis(1_000));
        assert_eq!(effective_time(&clock, 200), Duration::from_millis(1_200));
        assert_eq!(effective_time(&clock, -400), Duration::from_millis(600));
        assert_eq!(
            effective_time(&clock, -5_000),
            Duration::ZERO,
            "must clamp at zero, not underflow"
        );
    }
}
