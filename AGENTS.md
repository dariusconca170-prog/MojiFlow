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
- **Capture (M5)**: cpal runs on a dedicated thread (`src/audio/capture.rs`) because `cpal::Stream` is `!Send` on ALSA; the callback pushes mono `f32` into a lock-free SPSC ring (`src/audio/ring.rs` — `AtomicU32` bit-slots + Release/Acquire horizon + one-sample slack, so no locks/allocs on the audio thread), export converts to MP3 via LAME (`src/audio/encode.rs`), screenshots use `xcap` and JPEG-encode via `image` (`src/capture/screen.rs`).
- **Anki export (M6)**: `src/anki/` — `AnkiConnect` (jsonrpc-style HTTP client, async reqwest; `request()` returns the typed `AnkiError` family), `ensure_model` (createModel with card templates when the model is missing), `storeMediaFile` (base64), `addNote` (allowDuplicate + duplicateScope, "duplicate" message → `AnkiError::Duplicate`). `note.rs` renders `field_mapping` templates (`{word}`/`{reading}`/…; `{{`/`}}` escape literal braces) into a serializable `PendingNote`; `queue.rs` persists them as JSON when Anki is down and `drain()`s them (stopping on `Unreachable`, removing each file only after success).
- **Mining export (M7)**: `src/export.rs` splits gather from send. The UI thread resolves the hovered/pinned token (else first content word) via the per-thread `Dictionary` and packages a plain `ExportSource` (card, Anki/Capture configs, queue dir, current ring `Arc`, audio window = cue ± 0.4 s, nonce). A dedicated worker thread (own tokio runtime) slices the ring → MP3 (LAME), screenshots the video window → JPEG (xcap), renders the `field_mapping` templates, then `ensureModel` → `storeMediaFile` → `addNote`. `Unreachable` → the *rendered note with media* is persisted to the `OfflineQueue` and up to all queued notes are retried on the next success; `Duplicate` is success with a "already mined" status (never queued). Media that cannot be produced is skipped with a note in the result message — the card still exports.
- **Overlay placement (M8)**: `window.rect` defaults to `[-1.0, -1.0, 1100.0, 320.0]` — a negative x/y is a sentinel meaning "auto bottom-center of the primary screen". `App` applies it once on the first frame from `GlobalPointer::primary_screen_size` (X11 root geometry, physical px ÷ `pixels_per_point`) via `ViewportCommand::OuterPosition`; `main.rs` passes `[0,0]` to the builder for sentinel rects. Never use `ctx.content_rect()`/`viewport_rect()` for this — they are window-local (that bug parked the overlay at +0+0). Pure helper `bottom_center_rect` (48 px bottom gap, clamped) is unit-tested.
- **Waiting hint (M8)**: a manual clock starts paused at zero, which used to look broken. `App.ever_started` flips on the first observed play (in `update_timing`); `waiting_for_start()` (track loaded + manual + never started) gates a dim `paint_waiting_hint` in `overlay.rs` (above the active sub, else bottom-pinned). Predicate + headless paint tests in `app.rs`/`overlay.rs`.
- **Grab video (M8)**: `src/grab.rs` shells out to the user-installed `yt-dlp` — site extraction is never reimplemented. `run()` polls `try_wait` (200 ms) so `shared.cancel` kills mid-download; the final path comes from `--print after_move:filepath`; `open_in_mpv` spawns detached mpv with the IPC socket. Dashboard card + startup `--version` probe (first "grab" status line becomes `grab_version`). Shutdown joins the grab thread after raising cancel. Tested with arg-shape, output-parse, tilde, missing-binary, and fake-shell-script end-to-end tests.
- **Global hotkeys (M7)**: `src/hotkey.rs` — `global-hotkey` manager (X11 only) mapping ids → 12 `AppAction`s, polled each frame (`poll_hotkeys` → `apply_action`); empty spec = disabled. File-open uses `rfd` with the `xdg-portal` backend (Linux only; no GTK).
- **Dashboard (M7)**: `src/gui/dashboard.rs` — a second, opaque egui viewport (`ViewportId::from_hash_of("mojiflow-dashboard")` via `Context::show_viewport_immediate`, opened while `config.window.dashboard_open` and closed when the call stops: title-bar ✕ or `Ctrl+Alt+D`). It makes the invisible overlay legible: clock source + `SyncState`, live time + transport buttons (manual clock only; live sources are read-only this milestone), per-token dictionary hits/misses for the active cue (green/amber chips, LRU-cached in `App::refresh_dashboard_tokens`), audio ring fill, export log (`App.status_log`, capped at 6 `CoreEvent::Status` lines), and the configured hotkeys. egui's style is per-`Ui` in 0.36, so the dashboard styles only its own viewport root ui (dark theme) without touching the overlay.
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
- **Transparent + click-through + hover conflict.** A passthrough window gets no mouse events. Mechanism (M4, implemented): the overlay is click-through by default (`ViewportBuilder::with_mouse_passthrough(true)`); each frame the UI computes the union of interactive rects (per-token rects derived from the shaped Galley by byte→char→glyph mapping, plus the open popover) and queries the *global* cursor position from the OS. `App::apply_passthrough` re-enables hit-testing (sends `ViewportCommand::MousePassthrough(false)`) only while the cursor is inside an interactive rect, otherwise disables it — and only when the desired state changes, to avoid a per-frame X round trip. A globally-held Shift forces interactive (Shift-lock). Because a click-through window receives no repaints of its own, the overlay polls the cursor at ~30 Hz (`request_repaint_after(33ms)`). Global cursor + modifier query lives in `capture::window` (`GlobalPointer`): Linux/X11 uses `x11rb` `QueryPointer` / `QueryKeymap` (modifier map cached) on a second connection and returns window-local egui points; **Windows uses `GetCursorPos`/`GetWindowRect`/`GetAsyncKeyState` (compile-gated in M7, not runtime-verified here) and macOS is still unimplemented** — the platform fallback returns a typed `WindowError` and the overlay stays interactive, so it is never silently broken.
- **Global hotkeys never bind bare letters.** Defaults: `Ctrl+Alt+S` export, `Ctrl+Alt+[`/`]` offset ±200 ms, `Ctrl+Alt+L` lock, `Ctrl+Alt+H` hide/show, `Ctrl+Alt+O` open subtitle file, `Ctrl+Alt+Space` clock start/pause, `Ctrl+Alt+←`/`→` seek ∓5 s, `Ctrl+Alt+E` edit mode, `Ctrl+Alt+P` status strip, `Ctrl+Alt+D` dashboard window. On X11 these are registered *globally* (fire from any app, via `global-hotkey`, polled per frame). On native Wayland manager init fails with a typed error, the app logs it once as a toast, and the *local* keys remain the fallback — bare keys work only while the overlay is focused or its popover is hovered. Note: `global-hotkey` canonicalizes letter keys in its id (`"Ctrl+Alt+S"` → `control+alt+KeyS`); registration uses the parsed id, so press-matching is unaffected.
- **Always-on-top is enforced at runtime, not by the window builder.** eframe 0.36 contains no `WindowLevel` handling at all, so `ViewportBuilder::with_window_level`/`with_always_on_top` are silently dropped and the overlay would sit at Z-order-luck. `App` re-asserts the EWMH hint every 750 ms via `GlobalPointer::set_always_on_top` (`src/capture/window.rs`): walks the X tree on the second x11rb connection, finds windows whose `_NET_WM_NAME` == `platform::OVERLAY_TITLE` ("MediaLingual", keep in sync with `main.rs`), and sends a `_NET_WM_STATE` ClientMessage (EWMH data32 `[action, ABOVE, 0, source=1, 0]`) only when the state differs. Honors `window.always_on_top`; errors log once (Windows/macOS return `WindowError::Unsupported` — not enforced there yet). Caveat: EWMH always-on-top does **not** beat fullscreen windows (KWin keeps fullscreen above everything) — so `window.follow_fullscreen` (default on) expands the overlay to fullscreen while any other window is fullscreen and restores the saved rect after (`GlobalPointer::is_any_fullscreen` walks the same X tree for `_NET_WM_STATE_FULLSCREEN`, skipping `OVERLAY_TITLE` windows so our own fullscreen state never loops; polled at the same 750 ms cadence).
- **Wayland.** Session detected at startup (`platform::detect` → X11/Wayland/Unknown) and logged in `main.rs`. X11/XWayland is the primary path. On native Wayland: global hotkeys are unavailable (`global-hotkey` is X11-only) → typed error + toast, local keys fall back; the global-cursor hover query isn't available → `GlobalPointer` fails typed and the overlay stays fully interactive; screenshots/portals are M8 territory. Always-on-top is EWMH-only (see above) — on Wayland the x11rb enforcer would only reach XWayland windows, so it degrades rather than breaking anything. Manual-region mode (user drags the overlay over the video; capture uses that rect) is the documented fallback for capture.
- **No universal media clock.** `PlaybackClock` trait with implementations: `ManualClock` (hotkey-driven), `MpvIpcClock` (JSON IPC over Unix socket / named pipe), `MprisClock` (Linux, zbus), `WhisperLiveClock` (timestamped on arrival). Selected in `config.toml` / settings.
- **Linux loopback audio** uses PulseAudio/PipeWire *monitor* sources (device picker persists the choice); hot-unplug handled by a reconnect loop with backoff. **Windows** uses WASAPI loopback through cpal. When no loopback/monitor source matches by name, capture falls back to the default input device and logs it; `AudioCapture::status()` reports retries and `App` toasts transitions.
- **Screenshots** target the identified video window (title match or user-picked from a window list); the overlay hides itself for the capture frame when it would appear in the shot.
- **Audio export is "last-heard audio"**: the ring buffer reflects real playback, so if the user paused/replayed, buffered audio may not match the cue. Documented in the UI; user can re-trigger after the line replays.

## Known platform caveats / limits

- **Global cursor query: Windows implemented (compile-gated), macOS not implemented.** The `capture::window` `GlobalPointer` on Windows uses `GetCursorPos`/`GetWindowRect`/`GetAsyncKeyState` — written in M7 but **unverifiable on this Linux machine** (Windows CI is M8). macOS still returns `WindowError::CursorQueryUnavailable`; the overlay logs it once as a toast and stays fully interactive (never silently dead). X11 is the verified path.
- **Idle CPU:** the overlay repaints at ~30 Hz even with the video paused, because a click-through window receives no mouse events to wake it. This is the cost of hover-while-click-through; the window is small and the paint is cheap.
- **Popover selection model:** it appears for the hovered token, stays open while the cursor is over the panel, and a primary click pins/unpins it; `Escape` clears the pin. Clicking "empty" overlay space cannot dismiss it (that space stays click-through on purpose) — hover away, click the token again, or Escape instead.
- **`libgbm-dev` is required to *link* on Linux.** Merely declaring `xcap` doesn't pull in `-lgbm`, but actually using it (M5 `screen.rs`) does — the linker needs `libgbm.so`, and runtime-only `libgbm1` is not enough. On Debian/Ubuntu: `sudo apt install libgbm-dev`.

## Manual QA checklist

- **M4 (X11, verified 2026-10-08 with python-xlib pointer warping):**
  - Launch; the overlay is click-through — the window underneath receives clicks on empty overlay areas.
  - Warp/move the pointer over a subtitle token → the token highlights and the window becomes interactive (`toggling overlay mouse passthrough interactive=true` at debug level).
  - Move off the token → click-through again (`interactive=false`).
  - Hold Shift while the pointer is elsewhere → interactive (`interactive=true, shift=true`); release → click-through.
  - Hover a token → dictionary popover (term/reading/pitch contour/frequency/POS/glosses); a conjugated surface shows its de-inflection chain.
- **M5 (smoke-verified 2026-10-08 on X11):**
  - Launch → toast `audio capture: <device> @ <rate> Hz`; the status strip shows `audio N.Ns buf` rising as the ring fills; log line `audio capture started device=... format=F32`.
  - Unplug/rename the capture device → reconnect retry toast and `audio retrying…` in the strip.
  - Unit gates: ring wraparound/clamp/underflow, MP3 frame-sync output, JPEG round-trip decode.

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
