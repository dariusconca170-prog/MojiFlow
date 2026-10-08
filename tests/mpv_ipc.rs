//! Live smoke test for the mpv JSON IPC clock: spawns a real `mpv` process playing a
//! generated WAV, and asserts the reader thread reaches `SyncState::Live` and tracks
//! real playback. Skips with a message when `mpv` or `python3` is unavailable (e.g. on
//! CI images without mpv) — the recorded-traffic unit tests in `src/clock/mpv_ipc.rs`
//! cover the message reducer either way.

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use medialingual_native::app::{CoreEvent, RepaintHandle};
use medialingual_native::clock::{mpv_ipc::MpvIpcClock, PlaybackClock, SyncState};

fn tool_exists(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 6 seconds of a 440 Hz sine tone as 16-bit mono WAV — no ffmpeg dependency.
fn generate_tone(path: &std::path::Path) {
    let script = r#"
import math, struct, wave, sys
with wave.open(sys.argv[1], "w") as w:
    w.setnchannels(1)
    w.setsampwidth(2)
    w.setframerate(8000)
    frames = b"".join(
        struct.pack("<h", int(12000 * math.sin(2 * math.pi * 440 * i / 8000)))
        for i in range(8000 * 6)
    )
    w.writeframes(frames)
"#;
    let status = Command::new("python3")
        .args(["-c", script])
        .arg(path)
        .status()
        .expect("spawn python3");
    assert!(status.success(), "wav generation failed");
}

#[cfg(unix)]
#[test]
fn mpv_ipc_clock_tracks_real_playback() {
    if !tool_exists("mpv") {
        eprintln!("mpv not available — skipping live IPC clock test");
        return;
    }
    if !tool_exists("python3") {
        eprintln!("python3 not available — skipping live IPC clock test");
        return;
    }

    let dir = std::env::temp_dir().join(format!("ml-mpv-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let media = dir.join("tone.wav");
    generate_tone(&media);

    let socket = dir.join("ipc.sock");
    let socket_str = socket.to_string_lossy().into_owned();
    // mpv requires the `--opt=value` form for value options — space-separated is a
    // fatal parse error (and --really-quiet would hide the message).
    let mut mpv = Command::new("mpv")
        .args([
            "--no-config",
            "--really-quiet",
            "--vo=null",
            "--ao=null",
            &format!("--input-ipc-server={socket_str}"),
        ])
        .arg(&media)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn mpv");

    let (tx, _rx) = crossbeam_channel::bounded::<CoreEvent>(16);
    let clock = MpvIpcClock::spawn(
        socket_str,
        tx,
        Arc::new(RepaintHandle::new(egui::Context::default())),
    );

    // 1) The reader thread must connect and go live (mpv may need a moment to create
    //    the socket).
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match clock.sync_state() {
            SyncState::Live => break,
            SyncState::Stale(reason) => {
                if Instant::now() > deadline {
                    let _ = mpv.kill();
                    let _ = mpv.wait();
                    panic!("mpv IPC never went live: {reason}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            SyncState::Local => panic!("mpv clock must never report Local"),
        }
    }

    // 2) The clock must track real playback: position advances while playing.
    std::thread::sleep(Duration::from_millis(300));
    assert!(clock.is_playing(), "mpv should be playing the tone");
    let before = clock.now();
    std::thread::sleep(Duration::from_millis(400));
    let after = clock.now();
    assert!(
        after > before,
        "clock did not advance with playback: {before:?} -> {after:?}"
    );

    // 3) Shutdown is honored promptly (no leaked reader thread).
    let shutdown_started = Instant::now();
    clock.shutdown();
    assert!(
        shutdown_started.elapsed() < Duration::from_millis(50),
        "shutdown() must not block the caller"
    );

    let _ = mpv.kill();
    let _ = mpv.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
