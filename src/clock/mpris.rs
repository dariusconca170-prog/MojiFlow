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
    pub fn spawn(
        player_filter: String,
        events: crossbeam_channel::Sender<CoreEvent>,
        repaint: Arc<RepaintHandle>,
    ) -> Self {
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
            .expect("spawn mpris clock thread");

        Self { shared, shutdown }
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
                handle_properties(message, shared, events, repaint);
            }
            message = names.try_next() => {
                let Some(message) = message? else { return Ok(()) };
                handle_name_change(message, conn, player_filter, shared, events, repaint).await;
            }
        }
    }
}

/// A player sent a `PropertiesChanged` for its `org.mpris.MediaPlayer2.Player` interface.
fn handle_properties(
    message: zbus::Message,
    shared: &Arc<Mutex<Shared>>,
    events: &crossbeam_channel::Sender<CoreEvent>,
    repaint: &Arc<RepaintHandle>,
) {
    let sender = message.header().sender().map(|name| name.to_string());
    let Ok((interface, changed, _invalidated)) = message.body().deserialize::<(
        String,
        HashMap<String, zbus::zvariant::OwnedValue>,
        Vec<String>,
    )>() else {
        return;
    };
    if interface != "org.mpris.MediaPlayer2.Player" {
        return;
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
        return;
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

async fn query_player(conn: &zbus::Connection, service: &str) -> zbus::Result<(Duration, bool)> {
    let proxy = match MprisPlayerProxy::new(conn, service).await {
        Ok(proxy) => proxy,
        Err(err) => return Err(err),
    };
    let position = proxy.position().await.unwrap_or(0);
    let status = proxy.playback_status().await.unwrap_or_default();
    Ok((
        Duration::from_micros(position.max(0) as u64),
        status == "Playing",
    ))
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
    position: Duration,
    playing: bool,
) {
    {
        let mut guard = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.connected = true;
        guard.stale_reason = None;
        guard.player = Some(player.clone());
        guard.position = position;
        guard.updated = Instant::now();
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
        );
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
