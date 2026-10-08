# MediaLingual-Native

A universal Japanese sentence-mining overlay for Linux and Windows. It sits transparently on
top of your video player, highlights the words in each subtitle line, and shows dictionary
entries (definition, reading, pitch accent, frequency) on hover — the fastest path from
"what does that word mean?" to an Anki card.

> Status: **pre-release, milestone M5 of M8 in progress.** See [`PLAN.md`](PLAN.md) for the
> milestone checklist and the authoritative progress log, and `AGENTS.md` for build notes,
> architecture, and platform caveats.

## What it does

- **Overlay that stays out of the way.** The window is transparent, always-on-top and
  click-through by default: clicks pass straight to the video underneath. Moving the pointer
  over a subtitle token (or holding Shift) makes just that region interactive so you can click
  it. See `App::apply_passthrough` and `src/capture/window.rs`.
- **Tokenized subtitles** — SRT / WebVTT / ASS, Shift-JIS and UTF-8 auto-detected. MeCab
  (lindera, embedded IPADIC) splits each line into words with byte offsets.
- **Dictionary popover** — hover a token for term, reading, pitch accent contour, corpus
  frequency, part of speech and glosses; conjugated surfaces show their de-inflection chain.
  Data from JMdict + Kanjium (see `cargo xtask build-dict`).
- **Capture (in progress, M5)** — a lock-free "last-heard audio" ring buffer (≥10 s) with MP3
  export, plus window/region screenshots encoded as JPEG, for building Anki cards.
- **Playback clocks** — manual hotkeys, mpv JSON-IPC, MPRIS (Linux), and Whisper-live modes.

## Building

Requirements: Rust 1.85+, a C toolchain, and on Linux the usual X11/Wayland + audio dev
packages. The overlay is developed and verified on **X11**; native Wayland and Windows/macOS
global-cursor paths are tracked in `PLAN.md` (M7).

```bash
cargo build --release          # overlay binary
cargo build --features whisper # optional local STT (off by default)
```

The Japanese dictionary databases are not checked in. Build them once (needs network):

```bash
cargo xtask build-dict         # JMdict + Kanjium pitch/frequency → assets/dict/*.sqlite
```

Run the overlay:

```bash
cargo run --release
```

Configuration lives at `~/.config/medialingual/config.toml`; logs at
`~/.config/medialingual/medialingual.log`.

## Development gate

Every milestone must pass:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
```

Architecture and platform caveats are documented in `AGENTS.md`; the milestone checklist and
status log are in `PLAN.md`.

## License

MIT (see `Cargo.toml`). JMdict is used under the EDRDG license; Kanjium is CC BY-SA 4.0 —
attribution for dictionary data sources is recorded in `AGENTS.md` and will be consolidated in
`THIRD_PARTY.md` (M8).