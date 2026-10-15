//! MPRIS v2.2 playback clock over the session D-Bus (zbus), M7.
//!
//! Watches `org.mpris.MediaPlayer2.*` services: picks the first player matching the
//! configured name filter (or any player when the filter is empty), reads its
//! `PlaybackStatus`/`Position` properties, and extrapolates with the wall clock between
//! updates exactly like the mpv clock. D-Bus signals (`PropertiesChanged` on
//! `/org/mpris/MediaPlayer2`, `NameOwnerChanged` for the player namespace) keep the local
//! state fresh without polling the bus.
//!
//! Degradation is explicit and typed: no session bus, no player, or a player that quits
//! → [`SyncState::Stale`] with a human reason in the status strip; the app keeps running
//! with the manual clock as the effective fallback (see `build_clock`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{PlaybackClock, SyncState};
use crate::app::{CoreEvent, RepaintHandle};
use crate::error::ClockError;

/// How often the UI may repaint while the clock is live (position is extrapolated between
/// updates, so this only affects smoothness).
const REPAINT_INTERVAL: Duration = Duration::from_millis(100);
/// Reconnect/scan interval when the bus or a player is unavailable.
const RETRY_INTERVAL: Duration = Duration::from_secs(3);
/// How often the reader wakes to check shutdown and smooth extrapolation.
const WAKE_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug)]
struct Shared {
    position: Duration,
    updated: Instant,
    playing: bool,
    connected: bool,
    player: Option<String>,
    stale_reason: Option<String>,
}

impl Shared {
    fn extrapolated(&self) -> Duration {
        if !self.playing {
            return self.position;
        }
        let elapsed = self.updated.elapsed();
        self.position.saturating_add(elapsed)
    }
}

pub struct MprisClock {
    shared: Arc<Mutex<Shared>>,
    shutdown: Arc<AtomicBool>,
}

impl MprisClock {
    /// Spawn the D-Bus observer thread (with its own tokio runtime) and return immediately.
    /// A failed thread spawn is a typed error, never a panic — this runs from `build_clock`,
    /// including a live source switch in the settings card.
    pub fn spawn(
        player_filter: String,
        events: crossbeam_channel::Sender<CoreEvent>,
        repaint: Arc<RepaintHandle>,
    ) -> Result<Self, ClockError> {
        let shared = Arc::new(Mutex::new(Shared {
            position: Duration::ZERO,
            updated: Instant::now(),
            playing: false,
            connected: false,
            player: None,
            stale_reason: Some("connecting to the session bus…".to_owned()),
        }));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shared = Arc::clone(&shared);
        let thread_shutdown = Arc::clone(&shutdown);

        std::thread::Builder::new()
            .name("mpris-clock".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => runtime.block_on(reader_loop(
                        player_filter,
                        thread_shared,
                        thread_shutdown,
                        events,
                        repaint,
                    )),
                    Err(err) => {
                        let text = format!("mpris: tokio runtime failed: {err}");
                        set_stale(&thread_shared, &text);
                        tracing::error!(error = %err, "mpris clock runtime failed");
                    }
                }
            })
            .map_err(|err| ClockError::Unavailable {
                component: "mpris".to_owned(),
                reason: format!("could not spawn the MPRIS thread: {err}"),
            })?;

        Ok(Self { shared, shutdown })
    }

    fn with<R>(&self, f: impl FnOnce(&mut Shared) -> R) -> R {
        let mut guard = self
            .shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut guard)
    }
}

impl PlaybackClock for MprisClock {
    fn now(&self) -> Duration {
        self.with(|s| s.extrapolated())
    }

    fn is_playing(&self) -> bool {
        self.with(|s| s.playing)
    }

    fn source_name(&self) -> &'static str {
        "mpris"
    }

    fn sync_state(&self) -> SyncState {
        self.with(|s| {
            if s.connected {
                SyncState::Live
            } else {
                SyncState::Stale(
                    s.stale_reason
                        .clone()
                        .unwrap_or_else(|| "mpris not connected".to_owned()),
                )
            }
        })
    }

    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// Reader loop (zbus)
// ---------------------------------------------------------------------------

#[zbus::proxy(
    interface = "org.mpris.MediaPlayer2.Player",
    default_path = "/org/mpris/MediaPlayer2"
)]
trait MprisPlayer {
    fn position(&self) -> zbus::Result<i64>;
    #[zbus(property)]
    fn playback_status(&self) -> zbus::Result<String>;
}

async fn reader_loop(
    player_filter: String,
    shared: Arc<Mutex<Shared>>,
    shutdown: Arc<AtomicBool>,
    events: crossbeam_channel::Sender<CoreEvent>,
    repaint: Arc<RepaintHandle>,
) {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        match zbus::Connection::session().await {
            Ok(conn) => {
                let _ = events.send(CoreEvent::Status {
                    component: "clock",
                    text: "mpris: connected to the session bus".to_owned(),
                });
                if observe_connection(&conn, &player_filter, &shared, &shutdown, &events, &repaint)
                    .await
                    .is_err()
                {
                    set_stale(
                        &shared,
                        "mpris: connection lost; reconnecting (bus or player disappeared)",
                    );
                }
            }
            Err(err) => {
                set_stale(
                    &shared,
                    &format!("mpris: session bus unavailable ({err}); retrying"),
                );
            }
        }
        sleep_checking_shutdown(&shutdown, RETRY_INTERVAL).await;
    }
}

/// Watch the player namespace until the observer errors out or shutdown is requested.
async fn observe_connection(
    conn: &zbus::Connection,
    player_filter: &str,
    shared: &Arc<Mutex<Shared>>,
    shutdown: &Arc<AtomicBool>,
    events: &crossbeam_channel::Sender<CoreEvent>,
    repaint: &Arc<RepaintHandle>,
) -> zbus::Result<()> {
    use futures_util::TryStreamExt as _;
    use zbus::match_rule::MatchRule;
    use zbus::message::Type as MsgType;

    scan_players(conn, player_filter, shared, events, repaint).await;

    let props_rule = MatchRule::builder()
        .msg_type(MsgType::Signal)
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .path("/org/mpris/MediaPlayer2")?
        .build();
    let names_rule = MatchRule::builder()
        .msg_type(MsgType::Signal)
        .sender("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .arg0ns("org.mpris.MediaPlayer2")?
        .build();

    let mut props = zbus::MessageStream::for_match_rule(props_rule, conn, Some(16)).await?;
    let mut names = zbus::MessageStream::for_match_rule(names_rule, conn, Some(16)).await?;
    let mut wake = tokio::time::interval(WAKE_INTERVAL);

    loop {
        tokio::select! {
            _ = wake.tick() => {
                if shutdown.load(Ordering::SeqCst) {
                    return Ok(());
                }
            }
            message = props.try_next() => {
                let Some(message) = message? else { return Ok(()) };
                if handle_properties(message, shared, events, repaint) {
                    // The signal says playback may have moved (seek, pause/resume, track
                    // change) but carries no position, so read it now — otherwise a seek
                    // in the player leaves the overlay desynced until the player changes.
                    refresh_position(conn, shared, events, repaint).await;
                }
            }
            message = names.try_next() => {
                let Some(message) = message? else { return Ok(()) };
                handle_name_change(message, conn, player_filter, shared, events, repaint).await;
            }
        }
    }
}

/// A player sent a `PropertiesChanged` for its `org.mpris.MediaPlayer2.Player` interface.
///
/// Returns `true` when the position may have moved and should be re-read from the player.
fn handle_properties(
    message: zbus::Message,
    shared: &Arc<Mutex<Shared>>,
    events: &crossbeam_channel::Sender<CoreEvent>,
    repaint: &Arc<RepaintHandle>,
) -> bool {
    let sender = message.header().sender().map(|name| name.to_string());
    let Ok((interface, changed, invalidated)) = message.body().deserialize::<(
        String,
        HashMap<String, zbus::zvariant::OwnedValue>,
        Vec<String>,
    )>() else {
        return false;
    };
    if interface != "org.mpris.MediaPlayer2.Player" {
        return false;
    }
    let status = changed
        .get("PlaybackStatus")
        .and_then(|value| value.downcast_ref::<&str>().ok())
        .map(str::to_owned);
    let position_us = changed
        .get("Position")
        .and_then(|value| value.downcast_ref::<i64>().ok());

    let mut guard = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let ours = sender.is_none()
        || guard
            .player
            .as_ref()
            .is_none_or(|player| player == sender.as_deref().unwrap_or(""));
    if !ours {
        return false;
    }
    guard.connected = true;
    guard.stale_reason = None;
    apply_properties(&mut guard, status.as_deref(), position_us);
    drop(guard);
    if position_us.is_some() || status.is_some() {
        let _ = events.send(CoreEvent::Status {
            component: "clock",
            text: "mpris: synced player position".to_owned(),
        });
    }
    repaint.request_after(REPAINT_INTERVAL);
    let keys: Vec<&str> = changed.keys().map(String::as_str).collect();
    signal_moves_position(&keys, &invalidated)
}

/// True when a `PropertiesChanged` signal can have moved the playback position.
///
/// `Position` is normally *not* in this list: MPRIS v2.2 forbids players from including
/// it in the signal, so a seek arrives as an invalidation instead. It is accepted in
/// `changed_keys` too for the players that send it anyway. `Metadata` covers a track
/// change, which restarts playback at zero while `PlaybackStatus` stays `Playing`.
fn signal_moves_position(changed_keys: &[&str], invalidated: &[String]) -> bool {
    changed_keys
        .iter()
        .any(|key| matches!(*key, "PlaybackStatus" | "Metadata" | "Position"))
        || invalidated.iter().any(|key| key == "Position")
}

/// A player service appeared or vanished. Rescan and possibly re-home the clock.
async fn handle_name_change(
    message: zbus::Message,
    conn: &zbus::Connection,
    player_filter: &str,
    shared: &Arc<Mutex<Shared>>,
    events: &crossbeam_channel::Sender<CoreEvent>,
    repaint: &Arc<RepaintHandle>,
) {
    let Ok((name, _old, new)) = message.body().deserialize::<(String, String, String)>() else {
        return;
    };
    tracing::info!(name = %name, new_owner = %new, "mpris player ownership changed");
    let leaving = new.is_empty();
    let is_ours = {
        let guard = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.player.as_deref() == Some(name.as_str())
    };
    if is_ours && leaving {
        set_stale(
            shared,
            &format!("mpris: player '{name}' quit; watching for the next player"),
        );
    }
    scan_players(conn, player_filter, shared, events, repaint).await;
}

/// Find a matching player (keep the current one when still alive) and sync its state.
async fn scan_players(
    conn: &zbus::Connection,
    player_filter: &str,
    shared: &Arc<Mutex<Shared>>,
    events: &crossbeam_channel::Sender<CoreEvent>,
    repaint: &Arc<RepaintHandle>,
) {
    let proxy = match zbus::fdo::DBusProxy::new(conn).await {
        Ok(proxy) => proxy,
        Err(err) => {
            set_stale(shared, &format!("mpris: dbus proxy failed ({err})"));
            return;
        }
    };
    let names = match proxy.list_names().await {
        Ok(names) => names,
        Err(err) => {
            set_stale(shared, &format!("mpris: list_names failed ({err})"));
            return;
        }
    };
    let needle = player_filter.trim().to_lowercase();
    let mut players: Vec<String> = names
        .into_iter()
        .map(|name| name.to_string())
        .filter(|name| name.starts_with("org.mpris.MediaPlayer2."))
        .filter(|name| needle.is_empty() || name.to_lowercase().contains(&needle))
        .collect();
    players.sort();

    let current = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .player
        .clone();
    let chosen = match &current {
        Some(cursor) if players.contains(cursor) => Some(cursor.clone()),
        _ => players.first().cloned(),
    };

    let Some(service) = chosen else {
        set_stale(
            shared,
            "mpris: no MPRIS player found (start a MediaPlayer2-compatible player)",
        );
        return;
    };

    match query_player(conn, &service).await {
        Ok((position, playing)) => {
            set_live(shared, events, repaint, service, position, playing);
        }
        Err(err) => {
            set_stale(
                shared,
                &format!("mpris: could not read player '{service}': {err}"),
            );
        }
    }
}

/// Read a player's position and status.
///
/// The position is **optional**: not every player implements the `Position` getter, and
/// the spec allows it to be absent. An unknown position must stay unknown — defaulting it
/// to zero snapped the overlay back to the first line of the file mid-playback.
/// `PlaybackStatus` is mandatory, so a failure to read it is a real error and is reported
/// as such instead of silently reading as "paused".
async fn query_player(
    conn: &zbus::Connection,
    service: &str,
) -> zbus::Result<(Option<Duration>, bool)> {
    let proxy = MprisPlayerProxy::new(conn, service).await?;
    let position = proxy
        .position()
        .await
        .ok()
        .filter(|micros| *micros >= 0)
        .map(|micros| Duration::from_micros(micros as u64));
    let status = proxy.playback_status().await?;
    Ok((position, status == "Playing"))
}

/// Re-read the player's position after a signal that may have moved it. A failed or
/// negative read is ignored (the clock keeps extrapolating) rather than treated as zero.
async fn refresh_position(
    conn: &zbus::Connection,
    shared: &Arc<Mutex<Shared>>,
    events: &crossbeam_channel::Sender<CoreEvent>,
    repaint: &Arc<RepaintHandle>,
) {
    let service = {
        let guard = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.player.clone()
    };
    let Some(service) = service else { return };
    let Ok(proxy) = MprisPlayerProxy::new(conn, service.as_str()).await else {
        return;
    };
    let Ok(position_us) = proxy.position().await else {
        return;
    };
    if position_us < 0 {
        return;
    }
    {
        let mut guard = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.position = Duration::from_micros(position_us as u64);
        guard.updated = Instant::now();
    }
    repaint.request_after(REPAINT_INTERVAL);
    let _ = events.send(CoreEvent::Status {
        component: "clock",
        text: "mpris: re-synced player position".to_owned(),
    });
}

/// Apply one player state update. Pure so it is unit-testable without a bus.
fn apply_properties(shared: &mut Shared, status: Option<&str>, position_us: Option<i64>) {
    if let Some(position) = position_us {
        if position >= 0 {
            shared.position = Duration::from_micros(position as u64);
            shared.updated = Instant::now();
        }
    }
    if let Some(status) = status {
        // Rebase at the transition so extrapolation never double-counts.
        shared.position = shared.extrapolated();
        shared.updated = Instant::now();
        shared.playing = status == "Playing";
    }
}

fn set_stale(shared: &Arc<Mutex<Shared>>, reason: &str) {
    let mut guard = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.connected = false;
    guard.playing = false;
    guard.stale_reason = Some(reason.to_owned());
}

fn set_live(
    shared: &Arc<Mutex<Shared>>,
    events: &crossbeam_channel::Sender<CoreEvent>,
    repaint: &Arc<RepaintHandle>,
    player: String,
    position: Option<Duration>,
    playing: bool,
) {
    {
        let mut guard = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.connected = true;
        guard.stale_reason = None;
        guard.player = Some(player.clone());
        match position {
            Some(position) => {
                guard.position = position;
                guard.updated = Instant::now();
            }
            // Unknown position: keep whatever we had and rebase at the transition, so
            // switching to playing never extrapolates from a stale timestamp.
            None => {
                guard.position = guard.extrapolated();
                guard.updated = Instant::now();
            }
        }
        guard.playing = playing;
    }
    let _ = events.send(CoreEvent::Status {
        component: "clock",
        text: format!("mpris: synced to {player}"),
    });
    repaint.request();
}

async fn sleep_checking_shutdown(shutdown: &Arc<AtomicBool>, total: Duration) {
    let mut slept = Duration::ZERO;
    while slept < total {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(WAKE_INTERVAL).await;
        slept += WAKE_INTERVAL;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Shared {
        Shared {
            position: Duration::ZERO,
            updated: Instant::now(),
            playing: false,
            connected: false,
            player: None,
            stale_reason: None,
        }
    }

    #[test]
    fn playback_status_and_position_apply() {
        let mut shared = fresh();
        apply_properties(&mut shared, Some("Playing"), Some(1_250_000));
        assert!(shared.playing);
        assert_eq!(shared.position, Duration::from_millis(1_250));

        apply_properties(&mut shared, Some("Paused"), None);
        assert!(!shared.playing);

        // Stopped keeps the position but halts extrapolation.
        apply_properties(&mut shared, Some("Stopped"), None);
        assert!(!shared.playing);
    }

    #[test]
    fn negative_position_is_ignored() {
        let mut shared = fresh();
        apply_properties(&mut shared, None, Some(-5));
        assert_eq!(shared.position, Duration::ZERO);
    }

    #[test]
    fn seeks_and_track_changes_trigger_a_position_refresh() {
        // A seek arrives as an invalidation, not as a value (MPRIS forbids putting
        // Position in the signal), so without this a seek never re-synced the overlay.
        assert!(signal_moves_position(&[], &["Position".to_owned()]));
        assert!(signal_moves_position(&["PlaybackStatus"], &[]));
        // A track change restarts playback at zero while the status stays "Playing".
        assert!(signal_moves_position(&["Metadata"], &[]));
        // Players that send the value anyway are handled too.
        assert!(signal_moves_position(&["Position"], &[]));
        // Unrelated chatter must not cost a D-Bus round trip.
        assert!(!signal_moves_position(&["Volume", "LoopStatus"], &[]));
        assert!(!signal_moves_position(&[], &["Metadata".to_owned()]));
    }

    #[test]
    fn unknown_position_keeps_the_clock_instead_of_resetting_it() {
        // A player that does not implement the Position getter used to read as 0 s, which
        // snapped the overlay back to the first line of the file mid-playback.
        let shared = Arc::new(Mutex::new(fresh()));
        let (events, _rx) = crossbeam_channel::bounded(4);
        let repaint = Arc::new(RepaintHandle::for_tests());
        {
            let mut guard = shared.lock().unwrap();
            guard.position = Duration::from_secs(90);
            guard.playing = true;
        }
        set_live(
            &shared,
            &events,
            &repaint,
            "org.mpris.MediaPlayer2.unknown".to_owned(),
            None,
            true,
        );
        // The guard is scoped (a `Mutex` is not reentrant and `set_live` locks it too).
        {
            let guard = shared.lock().unwrap();
            assert!(
                guard.position >= Duration::from_secs(90),
                "position was reset: {:?}",
                guard.position
            );
            assert!(guard.connected && guard.playing);
        }

        // A reported position still wins.
        set_live(
            &shared,
            &events,
            &repaint,
            "org.mpris.MediaPlayer2.known".to_owned(),
            Some(Duration::from_secs(5)),
            false,
        );
        {
            let guard = shared.lock().unwrap();
            assert_eq!(guard.position, Duration::from_secs(5));
            assert!(!guard.playing);
        }
    }

    #[test]
    fn extrapolation_stops_while_paused() {
        let mut shared = fresh();
        apply_properties(&mut shared, Some("Playing"), Some(10_000_000));
        let at = shared.extrapolated();
        std::thread::sleep(Duration::from_millis(20));
        assert!(shared.extrapolated() > at, "playing clock advances");
        apply_properties(&mut shared, Some("Paused"), None);
        let paused = shared.position;
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(shared.extrapolated(), paused, "paused clock is frozen");
    }

    #[test]
    fn clock_reports_stale_before_any_connection() {
        let clock = MprisClock::spawn(
            String::new(),
            crossbeam_channel::bounded(4).0,
            Arc::new(RepaintHandle::for_tests()),
        )
        .expect("spawn clock");
        match clock.sync_state() {
            // The reason varies by environment (connecting… → session bus unavailable,
            // or "no player found" when a bus exists); what matters is the initial state
            // is never Live before a player is actually seen.
            SyncState::Stale(_) => {}
            other => panic!("expected Stale before connect, got {other:?}"),
        }
        clock.shutdown();
    }
}
