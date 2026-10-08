# AGENTS.md — build notes, architecture, conventions

## Build commands

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo fmt --manifest-path xtask/Cargo.toml --check
cargo clippy --manifest-path xtask/Cargo.toml -- -D warnings
cargo test
cargo build
cargo build --release
cargo build --features whisper        # optional STT (off by default)
cargo xtask build-dict                # download + build JMdict/pitch/frequency SQLite DBs
```

## Architecture summary

- **UI thread**: eframe/egui (`src/gui/`), never blocks. Workers push `CoreEvent`s over a bounded channel and call `ctx.request_repaint()`.
- **Tokio runtime** on a dedicated thread for HTTP (AnkiConnect, STT HTTP backend) and socket IPC (mpv).
- **Dedicated OS threads**: audio capture callback (real-time safe, no alloc/locks), whisper inference (feature-gated), screenshot capture.
- **Message bus**: `UiCommand` (UI → workers), `CoreEvent` (workers → UI), bounded `tokio::sync::mpsc` + `crossbeam-channel` where a sync boundary is needed.
- **Shutdown**: `CancellationToken` stops all workers; test asserts clean exit with no leaked threads.

## Conventions

- `thiserror` typed errors inside library modules; `anyhow` only at the app boundary (e.g. `main`).
- `tracing` for logs (`tracing-subscriber` to file + stderr); no `println!`.
- No `unwrap()`/`expect()` outside `#[cfg(test)]` and documented one-time startup invariants.
- No `todo!()`, `unimplemented!()`, `// TODO`, or empty stubs. Infeasible → real fallback + typed error surfaced in UI + entry here.
- Heavy crates behind Cargo features: `whisper`, `cuda`, `vulkan` (all default-off).

## Platform caveats (living document)

- **egui has no CJK glyphs.** A Japanese font is loaded at startup: system font probe (Noto Sans CJK JP, Yu Gothic, Meiryo, IPAGothic) with an embedded OFL fallback font in `assets/fonts/`. Covered by a render test asserting kanji, kana, `々ー〜`.
- **Transparent + click-through + hover conflict.** A passthrough window gets no mouse events. Mechanism: the overlay is click-through by default; each frame the UI computes the union of interactive rects (token rects, popover, toolbar) and queries the *global* cursor position from the OS; if the cursor is inside an interactive rect it issues a viewport command to re-enable hit-testing, otherwise it disables it. Global cursor query lives in `capture::window` per platform (X11 `x11rb`, Windows `windows-sys`/`GetCursorPos`; on native Wayland global cursor query is unavailable → fallback documented below).
- **Global hotkeys never bind bare letters.** Defaults: `Ctrl+Alt+S` export, `Ctrl+Alt+[`/`]` offset ∓200 ms, `Ctrl+Alt+L` lock, `Ctrl+Alt+H` hide/show, `Ctrl+Alt+O` open subtitle file. Bare keys work only while the overlay is focused or its popover is hovered.
- **Wayland.** Session type detected at startup (`XDG_SESSION_TYPE`/`WAYLAND_DISPLAY`) and logged. X11/XWayland is the primary path. On native Wayland: try `xdg-desktop-portal` via `ashpd` for GlobalShortcuts/Screenshot; otherwise run "manual region" mode (user drags overlay over the video; capture uses that rect). Global cursor query and always-on-top are restricted in native Wayland → manual region mode is the documented fallback.
- **No universal media clock.** `PlaybackClock` trait with implementations: `ManualClock` (hotkey-driven), `MpvIpcClock` (JSON IPC over Unix socket / named pipe), `MprisClock` (Linux, zbus), `WhisperLiveClock` (timestamped on arrival). Selected in `config.toml` / settings.
- **Linux loopback audio** uses PulseAudio/PipeWire *monitor* sources (device picker persists the choice); hot-unplug handled by a reconnect loop with backoff. **Windows** uses WASAPI loopback through cpal.
- **Screenshots** target the identified video window (title match or user-picked from a window list); the overlay hides itself for the capture frame when it would appear in the shot.
- **Audio export is "last-heard audio"**: the ring buffer reflects real playback, so if the user paused/replayed, buffered audio may not match the cue. Documented in the UI; user can re-trigger after the line replays.

## Known platform caveats / limits

(filled in as encountered)

## Dictionary data (built by `cargo xtask build-dict`, gitignored artifacts)

- Sources verified live on 2026-10-08 — re-verify before changing URLs:
  - **JMdict_e** `http://ftp.edrdg.org/pub/Nihongo/JMdict_e.gz` (EDRDG; CC BY-SA-compatible terms) → `assets/dict/jmdict.sqlite` (~113 MB, 218k entries, one row per gloss × kanji-form × reading).
  - **Kanjium** `github.com/mifunetoshiro/kanjium` (**CC BY-SA 4.0**, attribution required): `data/source_files/raw/accents.txt` → `pitch.sqlite` (124k rows), `raw/novels_freq.txt` → `frequency.sqlite` (286k words ranked by corpus count desc).
- JMdict **Rev 1.09** quirks: `ent_seq` is a *child element* (older revisions used an attribute — both handled; a silent id=0 breaks entry grouping), and all POS values are DTD entities (`&n;`) which quick-xml emits as `Event::GeneralRef` (266 entities parsed from the DOCTYPE and resolved manually).
- Entries group by `(entry id, primary form)` so distinct JMdict entries that share a surface (学生 'student' vs Heian-era senses) never merge, and alternate kanji forms stay separately displayable.
- MeCab splits euphonic changes across tokens (読んで → 読ん + で): dictionary lookups must use `Token::base_form`, or `Dictionary::resolve()` for raw surfaces.
- `assets/dict/` paths mirror `DictionaryConfig::default()` in `src/config.rs`; keep the two in sync when changing either.

## Manual QA checklist

(filled in during M4–M8)
