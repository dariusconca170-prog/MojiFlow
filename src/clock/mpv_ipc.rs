//! mpv JSON IPC clock (`--input-ipc-server=/tmp/mpv-ipc.sock`).
//!
//! A dedicated OS thread connects to the socket, observes `time-pos` / `pause` / `speed`,
//! and updates a shared state that the UI reads lock-ly every frame — no I/O on the UI
//! thread. Between property updates the position is extrapolated with the wall clock, so
//! `now()` stays smooth (frame-accurate within the observer dispatch interval).
//!
//! Reconnects with backoff if mpv restarts. On Windows the named-pipe transport lands in
//! milestone M7; until then the clock reports [`SyncState::Stale`] with that exact
//! message (visible in the status strip) instead of pretending to be connected.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{PlaybackClock, SyncState};
use crate::app::{CoreEvent, RepaintHandle};

/// How often the UI may repaint while the mpv clock is live (10 fps keeps idle CPU low;
/// `now()` extrapolates between updates).
const REPAINT_INTERVAL: Duration = Duration::from_millis(100);
/// Reconnect attempt interval when mpv is not (yet) running.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct Shared {
    /// Last known media position (at `updated`).
    position: Duration,
    /// Wall-clock instant of the last update.
    updated: Instant,
    /// `!pause` from mpv.
    playing: bool,
    /// Playback rate (`speed`) for extrapolation.
    rate: f64,
    connected: bool,
    /// Why we're stale, if we are (shown in the status strip).
    stale_reason: Option<String>,
}

impl Shared {
    fn extrapolated(&self) -> Duration {
        if !self.playing {
            return self.position;
        }
        let elapsed = self.updated.elapsed().as_secs_f64() * self.rate;
        let extra = Duration::from_secs_f64(elapsed.max(0.0));
        self.position.saturating_add(extra)
    }
}

pub struct MpvIpcClock {
    shared: Arc<Mutex<Shared>>,
    shutdown: Arc<AtomicBool>,
}

impl MpvIpcClock {
    /// Spawn the IPC reader thread and return immediately (UI thread never blocks).
    pub fn spawn(
        socket_path: String,
        events: crossbeam_channel::Sender<CoreEvent>,
        repaint: Arc<RepaintHandle>,
    ) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            position: Duration::ZERO,
            updated: Instant::now(),
            playing: false,
            rate: 1.0,
            connected: false,
            stale_reason: Some("connecting to mpv…".to_owned()),
        }));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shared = Arc::clone(&shared);
        let thread_shutdown = Arc::clone(&shutdown);

        std::thread::Builder::new()
            .name("mpv-ipc-clock".to_owned())
            .spawn(move || {
                reader_thread(socket_path, thread_shared, thread_shutdown, events, repaint)
            })
            .expect("spawn mpv IPC thread");

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

impl PlaybackClock for MpvIpcClock {
    fn now(&self) -> Duration {
        self.with(|s| s.extrapolated())
    }

    fn is_playing(&self) -> bool {
        self.with(|s| s.playing)
    }

    fn source_name(&self) -> &'static str {
        "mpv-ipc"
    }

    fn sync_state(&self) -> SyncState {
        self.with(|s| {
            if s.connected {
                SyncState::Live
            } else {
                SyncState::Stale(
                    s.stale_reason
                        .clone()
                        .unwrap_or_else(|| "mpv not connected".to_owned()),
                )
            }
        })
    }

    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// Reader thread
// ---------------------------------------------------------------------------

fn reader_thread(
    socket_path: String,
    shared: Arc<Mutex<Shared>>,
    shutdown: Arc<AtomicBool>,
    events: crossbeam_channel::Sender<CoreEvent>,
    repaint: Arc<RepaintHandle>,
) {
    #[cfg(unix)]
    unix_reader_loop(socket_path, &shared, &shutdown, &events, &repaint);
    #[cfg(windows)]
    windows_unsupported(&socket_path, &shared, &shutdown, &events, &repaint);
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (socket_path, shared, shutdown, events, repaint);
    }
}

fn set_stale(
    shared: &Arc<Mutex<Shared>>,
    events: &crossbeam_channel::Sender<CoreEvent>,
    repaint: &Arc<RepaintHandle>,
    reason: &str,
    announce: bool,
) {
    {
        let mut guard = shared.lock().unwrap_or_else(|p| p.into_inner());
        guard.connected = false;
        guard.stale_reason = Some(reason.to_owned());
    }
    if announce {
        let _ = events.send(CoreEvent::Status {
            component: "clock",
            text: reason.to_owned(),
        });
    }
    repaint.request();
}

fn set_live(
    shared: &Arc<Mutex<Shared>>,
    events: &crossbeam_channel::Sender<CoreEvent>,
    repaint: &Arc<RepaintHandle>,
) {
    {
        let mut guard = shared.lock().unwrap_or_else(|p| p.into_inner());
        guard.connected = true;
        guard.stale_reason = None;
        guard.updated = Instant::now();
    }
    let _ = events.send(CoreEvent::Status {
        component: "clock",
        text: "mpv IPC connected".to_owned(),
    });
    repaint.request();
}

/// Apply one JSON line from mpv to the shared state. Returns `false` when mpv signaled
/// shutdown. Extracted from the reader loop so it can be unit-tested with recorded IPC
/// traffic (see tests).
fn apply_message(shared: &mut Shared, line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return true; // Non-JSON chatter (e.g. log lines) — ignore, never panic.
    };
    let obj = match value.as_object() {
        Some(obj) => obj,
        None => return true,
    };
    match obj.get("event").and_then(|e| e.as_str()) {
        Some("property-change") => {
            let id = obj.get("id").and_then(|v| v.as_i64()).unwrap_or(-1);
            let data = obj.get("data");
            match id {
                1 => {
                    if let Some(secs) = data.and_then(|d| d.as_f64()) {
                        if secs.is_finite() && secs >= 0.0 {
                            shared.position = Duration::from_secs_f64(secs);
                            shared.updated = Instant::now();
                        }
                    }
                }
                2 => {
                    if let Some(paused) = data.and_then(|d| d.as_bool()) {
                        // Rebase at the transition so extrapolation doesn't double-count.
                        shared.position = shared.extrapolated();
                        shared.updated = Instant::now();
                        shared.playing = !paused;
                    }
                }
                3 => {
                    if let Some(rate) = data.and_then(|d| d.as_f64()) {
                        if rate.is_finite() && rate >= 0.0 {
                            shared.position = shared.extrapolated();
                            shared.updated = Instant::now();
                            shared.rate = rate;
                        }
                    }
                }
                _ => {}
            }
            true
        }
        Some("shutdown") => false,
        Some("end-file") | Some("start-file") | Some("playback-restart") => {
            // Seek/restart: mpv follows with fresh time-pos observations; mark live again
            // in case we were stale.
            shared.connected = true;
            shared.stale_reason = None;
            shared.updated = Instant::now();
            true
        }
        _ => true,
    }
}

#[cfg(unix)]
fn unix_reader_loop(
    socket_path: String,
    shared: &Arc<Mutex<Shared>>,
    shutdown: &Arc<AtomicBool>,
    events: &crossbeam_channel::Sender<CoreEvent>,
    repaint: &Arc<RepaintHandle>,
) {
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    let mut announced_failure = false;
    while !shutdown.load(Ordering::SeqCst) {
        let mut stream = match UnixStream::connect(&socket_path) {
            Ok(stream) => {
                announced_failure = false;
                stream
            }
            Err(err) => {
                if !announced_failure {
                    announced_failure = true;
                    set_stale(
                        shared,
                        events,
                        repaint,
                        &format!(
                            "mpv IPC not reachable at {socket_path} ({err}); retrying — start mpv with --input-ipc-server={socket_path}"
                        ),
                        true,
                    );
                }
                sleep_checking_shutdown(shutdown, RECONNECT_INTERVAL);
                continue;
            }
        };

        if let Err(err) = configure_stream(&stream) {
            set_stale(
                shared,
                events,
                repaint,
                &format!("mpv IPC configure failed: {err}"),
                true,
            );
            sleep_checking_shutdown(shutdown, RECONNECT_INTERVAL);
            continue;
        }
        set_live(shared, events, repaint);

        let mut buffer: Vec<u8> = Vec::with_capacity(8192);
        let mut chunk = [0u8; 4096];
        'read: loop {
            if shutdown.load(Ordering::SeqCst) {
                return;
            }
            match stream.read(&mut chunk) {
                Ok(0) => {
                    set_stale(
                        shared,
                        events,
                        repaint,
                        "mpv IPC closed the connection",
                        true,
                    );
                    break 'read;
                }
                Ok(n) => {
                    buffer.extend_from_slice(&chunk[..n]);
                    while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = buffer.drain(..=pos).collect();
                        let text = String::from_utf8_lossy(&line[..line.len() - 1]);
                        let mut guard = shared.lock().unwrap_or_else(|p| p.into_inner());
                        if !apply_message(&mut guard, &text) {
                            let _ = events.send(CoreEvent::Status {
                                component: "clock",
                                text: "mpv signaled shutdown".to_owned(),
                            });
                            return;
                        }
                        drop(guard);
                        repaint.request_after(REPAINT_INTERVAL);
                    }
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if shutdown.load(Ordering::SeqCst) {
                        return;
                    }
                }
                Err(err) => {
                    set_stale(
                        shared,
                        events,
                        repaint,
                        &format!("mpv IPC read error: {err}"),
                        true,
                    );
                    break 'read;
                }
            }
        }
        sleep_checking_shutdown(shutdown, RECONNECT_INTERVAL);
    }
}

#[cfg(unix)]
fn configure_stream(stream: &std::os::unix::net::UnixStream) -> std::io::Result<()> {
    use std::io::Write;
    use std::time::Duration as StdDuration;

    stream.set_read_timeout(Some(StdDuration::from_millis(250)))?;
    stream.set_nonblocking(false)?;
    let mut stream = stream.try_clone()?;
    for command in [
        r#"{"command":["observe_property",1,"time-pos"]}"#,
        r#"{"command":["observe_property",2,"pause"]}"#,
        r#"{"command":["observe_property",3,"speed"]}"#,
        r#"{"command":["get_property","time-pos"]}"#,
        r#"{"command":["get_property","pause"]}"#,
    ] {
        stream.write_all(command.as_bytes())?;
        stream.write_all(b"\n")?;
    }
    stream.flush()
}

/// Windows named-pipe transport lands in milestone M7. Until then, report exactly that
/// (typed, visible in the status strip) instead of silently pretending to sync.
#[cfg(windows)]
fn windows_unsupported(
    socket_path: &str,
    shared: &Arc<Mutex<Shared>>,
    shutdown: &Arc<AtomicBool>,
    events: &crossbeam_channel::Sender<CoreEvent>,
    repaint: &Arc<RepaintHandle>,
) {
    set_stale(
        shared,
        events,
        repaint,
        &format!(
            "mpv IPC on Windows (named pipe {socket_path}) arrives in milestone M7; clock is not syncing"
        ),
        true,
    );
    while !shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn sleep_checking_shutdown(shutdown: &Arc<AtomicBool>, total: Duration) {
    let step = Duration::from_millis(100);
    let mut slept = Duration::ZERO;
    while slept < total {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(step);
        slept += step;
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
            rate: 1.0,
            connected: false,
            stale_reason: None,
        }
    }

    #[test]
    fn applies_time_pos_and_pause_updates() {
        let mut shared = fresh();
        assert!(apply_message(
            &mut shared,
            r#"{"event":"property-change","id":1,"data":12.5}"#
        ));
        assert_eq!(shared.position, Duration::from_millis(12_500));

        assert!(apply_message(
            &mut shared,
            r#"{"event":"property-change","id":2,"data":true}"#
        ));
        assert!(!shared.playing, "pause=true means not playing");

        assert!(apply_message(
            &mut shared,
            r#"{"event":"property-change","id":2,"data":false}"#
        ));
        assert!(shared.playing);
        // Extrapolation must not jump while paused.
        assert!(apply_message(
            &mut shared,
            r#"{"event":"property-change","id":2,"data":true}"#
        ));
        let paused_at = shared.position;
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            shared.extrapolated(),
            paused_at,
            "paused clock must not advance"
        );
    }

    #[test]
    fn rate_change_rebases_position() {
        let mut shared = fresh();
        apply_message(
            &mut shared,
            r#"{"event":"property-change","id":1,"data":10.0}"#,
        );
        // Rate change while playing must rebase at the pre-change position — the elapsed
        // wall time since the last time-pos update must not be extrapolated at the old
        // rate across the transition (that would cause a jump).
        apply_message(
            &mut shared,
            r#"{"event":"property-change","id":2,"data":false}"#,
        );
        std::thread::sleep(Duration::from_millis(30));
        apply_message(
            &mut shared,
            r#"{"event":"property-change","id":3,"data":2.0}"#,
        );
        assert_eq!(shared.rate, 2.0);
        assert!(
            shared.position >= Duration::from_secs(10)
                && shared.position <= Duration::from_millis(10_050),
            "rebase jumped: {:?}",
            shared.position
        );
        // While paused the rebase is exact (set the position, then flip rate).
        apply_message(
            &mut shared,
            r#"{"event":"property-change","id":2,"data":true}"#,
        );
        apply_message(
            &mut shared,
            r#"{"event":"property-change","id":1,"data":10.0}"#,
        );
        apply_message(
            &mut shared,
            r#"{"event":"property-change","id":3,"data":2.0}"#,
        );
        assert_eq!(shared.position, Duration::from_secs(10));
        assert_eq!(shared.rate, 2.0);
    }

    #[test]
    fn ignores_null_data_and_garbage_without_panicking() {
        let mut shared = fresh();
        assert!(apply_message(
            &mut shared,
            r#"{"event":"property-change","id":1,"data":null}"#
        ));
        assert!(apply_message(&mut shared, "not json at all"));
        assert!(apply_message(&mut shared, r#"[1,2,3]"#));
        assert!(apply_message(
            &mut shared,
            r#"{"event":"property-change","id":99,"data":1}"#
        ));
        assert_eq!(shared.position, Duration::ZERO);
        assert!(
            !apply_message(&mut shared, r#"{"event":"shutdown"}"#),
            "shutdown returns false"
        );
    }

    #[test]
    fn negative_or_nan_time_pos_is_rejected() {
        let mut shared = fresh();
        assert!(apply_message(
            &mut shared,
            r#"{"event":"property-change","id":1,"data":-5.0}"#
        ));
        assert_eq!(shared.position, Duration::ZERO);
        assert!(apply_message(
            &mut shared,
            r#"{"event":"property-change","id":1,"data":1.0}"#
        ));
        assert_eq!(shared.position, Duration::from_secs(1));
    }

    #[test]
    fn clock_reports_stale_until_connected() {
        let clock = MpvIpcClock::spawn(
            "/nonexistent/medialingual-test.sock".to_owned(),
            crossbeam_channel::bounded(4).0,
            Arc::new(RepaintHandle::for_tests()),
        );
        match clock.sync_state() {
            SyncState::Stale(reason) => assert!(reason.contains("connecting"), "{reason}"),
            other => panic!("expected Stale, got {other:?}"),
        }
        clock.shutdown();
    }
}
