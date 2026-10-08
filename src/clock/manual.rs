//! Manual playback clock: the user drives it with hotkeys
//! (`Ctrl+Alt+Space` start/pause, `Ctrl+Alt+←/→` ±5 s). Fine offsets (`[`/`]`) are
//! layered on top by `App` via `effective_time`, not stored in the clock.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{PlaybackClock, SyncState};

#[derive(Debug)]
struct State {
    /// Position at the last play/pause transition.
    position: Duration,
    /// When the last transition happened (for extrapolating while playing).
    switched_at: Instant,
    playing: bool,
}

/// A wall-clock-advancing manual clock. Cheap to clone: clones share the same inner
/// state, so the UI keeps a control handle while the active [`PlaybackClock`] box points
/// at the same clock. `now()` is O(1) and never blocks on I/O.
#[derive(Clone)]
pub struct ManualClock {
    state: Arc<Mutex<State>>,
}

impl ManualClock {
    pub fn new() -> Self {
        Self::new_at(Duration::ZERO, false)
    }

    /// Construct at a known position/playing state (used by tests too).
    pub fn new_at(position: Duration, playing: bool) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                position,
                switched_at: Instant::now(),
                playing,
            })),
        }
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut guard)
    }

    fn position_now(state: &State) -> Duration {
        if state.playing {
            state.position.saturating_add(state.switched_at.elapsed())
        } else {
            state.position
        }
    }
    pub fn toggle_play_pause(&self) {
        self.with_state(|s| {
            let current = Self::position_now(s);
            s.position = current;
            s.switched_at = Instant::now();
            s.playing = !s.playing;
        });
    }

    /// Seek to an absolute media position (clamped at zero).
    pub fn seek_to(&self, position: Duration) {
        self.with_state(|s| {
            s.position = position;
            s.switched_at = Instant::now();
        });
    }

    /// Seek by a signed delta; clamps at zero.
    pub fn seek_by(&self, delta: Duration, forward: bool) {
        self.with_state(|s| {
            let current = Self::position_now(s);
            s.position = if forward {
                current.saturating_add(delta)
            } else {
                current.checked_sub(delta).unwrap_or(Duration::ZERO)
            };
            s.switched_at = Instant::now();
        });
    }

    pub fn set_playing(&self, playing: bool) {
        self.with_state(|s| {
            if s.playing != playing {
                s.position = Self::position_now(s);
                s.switched_at = Instant::now();
                s.playing = playing;
            }
        });
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackClock for ManualClock {
    fn now(&self) -> Duration {
        self.with_state(|state| Self::position_now(state))
    }

    fn is_playing(&self) -> bool {
        self.with_state(|s| s.playing)
    }

    fn source_name(&self) -> &'static str {
        "manual"
    }

    fn sync_state(&self) -> SyncState {
        SyncState::Local
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_paused_at_zero() {
        let clock = ManualClock::new();
        assert_eq!(clock.now(), Duration::ZERO);
        assert!(!clock.is_playing());
        assert_eq!(clock.sync_state(), SyncState::Local);
    }

    #[test]
    fn toggle_and_seek_adjust_position() {
        let clock = ManualClock::new_at(Duration::from_millis(5_000), false);
        // Seek while paused: exact positions, no wall-clock elapsed in the math.
        clock.seek_by(Duration::from_secs(5), true);
        assert_eq!(clock.now(), Duration::from_millis(10_000));
        clock.seek_by(Duration::from_secs(7), false);
        assert_eq!(clock.now(), Duration::from_millis(3_000));
        clock.seek_by(Duration::from_secs(10), false);
        assert_eq!(clock.now(), Duration::ZERO, "clamped at zero");
        clock.seek_to(Duration::from_millis(2_000));
        assert_eq!(clock.now(), Duration::from_millis(2_000));

        // While playing, time must advance (range assertions — never exact equality).
        clock.toggle_play_pause();
        assert!(clock.is_playing());
        std::thread::sleep(Duration::from_millis(30));
        let advanced = clock.now();
        assert!(
            advanced >= Duration::from_millis(2_030),
            "playing clock did not advance: {advanced:?}"
        );
        clock.set_playing(false);
        assert!(!clock.is_playing());
        let frozen = clock.now();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(clock.now(), frozen, "paused clock must not advance");
    }
}
