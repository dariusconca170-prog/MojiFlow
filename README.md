# MediaLingual-Native

A universal Japanese sentence-mining overlay for Linux and Windows. It sits transparently on
top of your video player, highlights the words in each subtitle line, and shows dictionary
entries (definition, reading, pitch accent, frequency) on hover — the fastest path from
"what does that word mean?" to an Anki card.

> Status: **pre-release, milestone M8 in progress.** Milestones M0–M7 are complete
> (subtitles + clocks, tokenizer + dictionary, the interactive click-through overlay,
> audio/screenshot capture, AnkiConnect export with an offline queue, global hotkeys, the
> control-room dashboard, and the mining-export pipeline). Much of M8's polish is already
> shipped — the yt-dlp "grab video" flow, bottom-anchored subtitles, fullscreen-follow, and
> this README — with Whisper STT, the settings panel and a CI workflow still to come. See
> [`PLAN.md`](PLAN.md) for the milestone checklist and status log, and `AGENTS.md` for build
> notes, architecture, and platform caveats.

## What it does

- **Overlay that stays out of the way.** The window is transparent, always-on-top and
  click-through by default: clicks pass straight to the video underneath. Moving the pointer
  over a subtitle token (or holding Shift) makes just that region interactive so you can click
  it. See `App::apply_passthrough` and `src/capture/window.rs`.
- **Subtitles where subtitles belong.** The overlay auto-places at the bottom-center of the
  screen (no more sitting over the browser tabs), and *follows the player into fullscreen* —
  when any window goes fullscreen the overlay expands to match, then restores its position
  when the player exits. Both are on by default (`window.rect` sentinel,
  `window.follow_fullscreen`).
- **Tokenized subtitles** — SRT / WebVTT / ASS, Shift-JIS and UTF-8 auto-detected. MeCab
  (lindera, embedded IPADIC) splits each line into words with byte offsets.
- **Dictionary popover** — hover a token for term, reading, pitch accent contour, corpus
  frequency, part of speech and glosses; conjugated surfaces show their de-inflection chain.
  Data from JMdict + Kanjium (see `cargo xtask build-dict`).
- **Playback clocks** — manual hotkeys (a not-yet-started manual clock shows a visible
  "press Ctrl+Alt+Space when it starts" hint), mpv JSON-IPC (the native workflow — play /
  pause / seek / position sync automatically), MPRIS (Linux), and Whisper-live modes.
- **Capture (M5)** — a lock-free "last-heard audio" ring buffer (≥10 s) with MP3 export,
  plus window/region screenshots encoded as JPEG, for building Anki cards.
- **Mining export (M6–M7)** — one-trigger Anki cards from a hovered word: ensures the note
  model, uploads the audio clip + screenshot as media, respects Anki's duplicate check, and
  queues the rendered note when Anki is unreachable.
- **Control-room dashboard (M7.5)** — the overlay is deliberately invisible, so a second,
  opaque window makes it legible: clock source and sync state, the active cue's per-token
  dictionary hits, audio ring fill, the export log, and the configured hotkeys.
  (`Ctrl+Alt+D`).
- **Grab video (M8)** — paste a stream URL into the dashboard's *Grab video* card and the
  app shells out to the installed `yt-dlp` (never reimplementing site extraction itself),
  optionally fetching Japanese subtitles as `.srt`, then opens the result in mpv with the
  JSON-IPC socket so the overlay follows the download automatically.

## Building

Requirements: Rust 1.85+, a C toolchain, and on Linux the usual X11/Wayland + audio dev
packages. The overlay is developed and verified on **X11**; native Wayland and Windows/macOS
global-cursor paths are tracked in `PLAN.md`.

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

## The recommended watch-and-mine flow

The overlay is at its best on top of **mpv**, which it can fully synchronize with:

```bash
sudo apt install mpv yt-dlp ffmpeg
```

Then set the clock to follow mpv (one line in the config):

```toml
[clock]
source = "mpv_ipc"
```

Play the video with the IPC socket open:

```bash
mpv --input-ipc-server=/tmp/mpv-ipc.sock video.mkv     # or: mpv "https://…" (mpv embeds yt-dlp)
```

Load a Japanese `.srt` into the overlay (`Ctrl+Alt+O`), and hover/mine as usual. No download
button on the site? Grab it from the dashboard (`Ctrl+Alt+D` → Grab video): the file lands in
`~/Videos/Medialingual` (with `ja` subtitles when the site has them) and opens in mpv
automatically. DRM'd streams can't be downloaded by any tool — for those, the manual clock +
browser remains the fallback.

## Development gate

Every milestone must pass:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
cargo build --release
```

Architecture and platform caveats are documented in `AGENTS.md`; the milestone checklist and
status log are in `PLAN.md`.

## License

MIT (see `Cargo.toml`). JMdict is used under the EDRDG license; Kanjium is CC BY-SA 4.0 —
attribution for dictionary data sources is recorded in `AGENTS.md` and will be consolidated in
`THIRD_PARTY.md` (M8).