# PLAN.md — MediaLingual-Native

Universal Japanese sentence-mining overlay (Rust, eframe/egui). Milestone checklist; this file is updated as milestones complete. Verification gate after **every** milestone:

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
```

## Milestones

- [ ] **M0 — Plan** — `PLAN.md` + `AGENTS.md` created and kept current.
- [ ] **M1 — Skeleton** — workspace, config, logging, error types, eframe transparent + always-on-top window rendering a hard-coded Japanese string with a loaded CJK font. Gate: kanji/kana/々ー〜 render, window transparent and on top.
- [ ] **M2 — Subtitles + clocks** — SRT/WebVTT/ASS parsers, `PlaybackClock` trait, `ManualClock`, `MpvIpcClock`, subtitle display with `[`/`]` offset hotkeys. Gate: parser tests pass (incl. malformed ASS, Shift-JIS SRT); sample SRT plays against mpv.
- [ ] **M3 — Tokenizer + dictionary** — lindera wrapper, `deinflect.rs` (≥25 conjugation cases), `cargo xtask build-dict` (JMdict + Kanjium pitch + frequency → SQLite), prepared-statement lookups + LRU. Gate: integration test ≥20 sentences passes; warm lookup < 5 ms.
- [ ] **M4 — Interactive overlay** — per-token rects from laid-out Galley, dynamic hit-testing (click-through toggle), popover with definitions/de-inflection/pitch graph, Shift-lock. Gate: hover works while the rest of the overlay stays click-through.
- [ ] **M5 — Capture** — audio SPSC ring buffer (≥10 s) + MP3 encode, window/region screenshot → JPEG, device/window pickers. Gate: slice tests pass (wraparound/clamp/underflow); produces playable mp3 and valid jpg.
- [ ] **M6 — Anki** — AnkiConnect client, field mapping templates, media upload, addNote, duplicate handling, offline queue, `createModel`. Gate: wiremock tests pass; real card created in live Anki.
- [ ] **M7 — Global hotkeys + platform paths** — global-hotkey bindings, `windows-sys` / `x11rb` window tracking, Wayland detection + portal/manual mode, `MprisClock`. Gate: each platform path logs active mode and degrades gracefully.
- [ ] **M8 — Whisper (feature) + polish** — local (`whisper` feature) and HTTP STT, settings panel, status strip, `README.md`, `THIRD_PARTY.md`, CI workflow. Gate: `cargo build` with and without `--features whisper`.

## Status log

- 2026-10-07: M0 started. Crate versions probed from crates.io (`cargo search`): eframe/egui 0.36.2, lindera 6.2.0, rusqlite 0.40.2, cpal 0.18.2, xcap 0.9.8, global-hotkey 0.8.0, whisper-rs 0.16.0, tokio 1.53.2, thiserror 2.0.21, anyhow 1.0.104. Exact APIs to be verified from source/docs before use (Operating Rule 4).
