# PLAN.md — MediaLingual-Native

Universal Japanese sentence-mining overlay (Rust, eframe/egui). Milestone checklist; this file is updated as milestones complete. Verification gate after **every** milestone:

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
```

## Milestones

- [x] **M0 — Plan** — `PLAN.md` + `AGENTS.md` created and kept current.
- [x] **M1 — Skeleton** — workspace, config, logging, error types, eframe transparent + always-on-top window rendering a hard-coded Japanese string with a loaded CJK font. Gate: kanji/kana/々ー〜 render, window transparent and on top.
- [x] **M2 — Subtitles + clocks** — SRT/WebVTT/ASS parsers, `PlaybackClock` trait, `ManualClock`, `MpvIpcClock`, subtitle display with `[`/`]` offset hotkeys. Gate: parser tests pass (incl. malformed ASS, Shift-JIS SRT); sample SRT plays against mpv.
- [x] **M3 — Tokenizer + dictionary** — lindera wrapper (`src/tokenize.rs`), `deinflect.rs` (rule table: godan/ichidan/する/くる/い-adj/copula; 30+ conjugation test cases), `cargo xtask build-dict` (JMdict + Kanjium pitch + frequency → SQLite), prepared-statement lookups + LRU (`src/dict.rs`). Gate: integration test 22 sentences passes (100% content-word resolution); warm lookup 1.1 µs ≪ 5 ms.
- [ ] **M4 — Interactive overlay** — per-token rects from laid-out Galley, dynamic hit-testing (click-through toggle), popover with definitions/de-inflection/pitch graph, Shift-lock. Gate: hover works while the rest of the overlay stays click-through.
- [ ] **M5 — Capture** — audio SPSC ring buffer (≥10 s) + MP3 encode, window/region screenshot → JPEG, device/window pickers. Gate: slice tests pass (wraparound/clamp/underflow); produces playable mp3 and valid jpg.
- [ ] **M6 — Anki** — AnkiConnect client, field mapping templates, media upload, addNote, duplicate handling, offline queue, `createModel`. Gate: wiremock tests pass; real card created in live Anki.
- [ ] **M7 — Global hotkeys + platform paths** — global-hotkey bindings, `windows-sys` / `x11rb` window tracking, Wayland detection + portal/manual mode, `MprisClock`. Gate: each platform path logs active mode and degrades gracefully.
- [ ] **M8 — Whisper (feature) + polish** — local (`whisper` feature) and HTTP STT, settings panel, status strip, `README.md`, `THIRD_PARTY.md`, CI workflow. Gate: `cargo build` with and without `--features whisper`.

## Status log

- 2026-10-07: M0 started. Crate versions probed from crates.io (`cargo search`): eframe/egui 0.36.2, lindera 6.2.0, rusqlite 0.40.2, cpal 0.18.2, xcap 0.9.8, global-hotkey 0.8.0, whisper-rs 0.16.0, tokio 1.53.2, thiserror 2.0.21, anyhow 1.0.104. Exact APIs to be verified from source/docs before use (Operating Rule 4).
- 2026-10-08: **M0 + M1 gate passed** — `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` (10/10: 5 config + 3 font incl. glyph-coverage + egui render, 2 misc), `cargo build` all clean. Committed (`9e717e0`, plus `.gitignore` secret guards `b4cb786`) and pushed.
- 2026-10-08: **M2 code complete, gate passed** — fmt/clippy/build clean, **31/31 tests pass** (SRT/VTT/ASS parsers + 5 test fixtures incl. Shift-JIS and malformed files, `SubtitleTrack` binary-search `active_at`/merge-stacked, `ManualClock`, `MpvIpcClock` JSON-IPC reducer, config, fonts). lib/bin split (`src/lib.rs`) done now so M3 integration tests can link the crate. Real bugs found & fixed by the gate: `thiserror` `source` field-name trap, `process_ass_text` unterminated-tag duplication, one-broken-cue-counts-as-N-skips in SRT/VTT, `TexturesDelta` drop panic in the render test. Manual mpv QA pending (mpv + `--input-ipc-server` smoke run).
- 2026-10-08: **M2 QA closed** — `tests/mpv_ipc.rs` spawns a real `mpv` playing a generated WAV over JSON IPC and asserts `SyncState::Live` + advancing clock + prompt shutdown (commit `dc2ea15`). Gotcha documented: mpv requires `--input-ipc-server=<path>`; the space-separated form is a fatal parse error that `--really-quiet` hides.
- 2026-10-08: **M3 complete, gate passed** — fmt/clippy clean for app **and** `xtask/`, **63/63 tests** (59 lib + 3 integration + 1 mpv). `cargo xtask build-dict` downloads JMdict_e (EDRDG, 218,875 entries → 750,860 rows), Kanjium `accents.txt` (124,137 pitch rows) and `novels_freq.txt` (285,718 ranked words) into `assets/dict/*.sqlite` in ~11 s (build artifacts gitignored). Sources verified live before use: JMdict Rev 1.09 made `ent_seq` a *child element* (old attribute parse silently produced id=0 for every entry — caught by spot-checking, not by the parser's own errors), Kanjium is CC BY-SA 4.0 (license read from repo), quick-xml 0.42 is string-based and emits `Event::GeneralRef` for the 266 DTD entities (`&n;` → `noun (common) …`). Gate numbers: 22 sentences, 87/87 content words resolved (100%), warm lookup 1.12 µs, warm `resolve()` 22.8 µs (budget 5 ms). MeCab splits euphonic forms (読んで → 読ん + で), so token tests use base forms, not surfaces.
