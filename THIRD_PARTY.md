# THIRD_PARTY.md — third-party data, fonts, and tools (M8)

MediaLingual-Native itself is MIT (see `Cargo.toml`). This file records everything else
the app ships with, downloads at build time, or shells out to — so a license question
never requires archaeology. Exact dependency versions are pinned in `Cargo.lock`;
a machine-readable per-crate audit (`cargo license` / `cargo deny`) has not been run yet.

## Dictionary data (downloaded by `cargo xtask build-dict`, never checked in)

Built artifacts live in `assets/dict/*.sqlite` (gitignored). Sources re-verified live on
2026-10-08; re-verify before changing URLs.

- **JMdict_e** — `http://ftp.edrdg.org/pub/Nihongo/JMdict_e.gz`, Electronic Dictionary
  Research and Development Group (EDRDG, Monash). Used under the EDRDG licence terms
  (attribution + share-alike-style conditions for derived dictionary works — read the
  licence on the EDRDG site before redistributing the built SQLite files).
- **Kanjium pitch accents + word frequency** — `github.com/mifunetoshiro/kanjium`,
  `data/source_files/raw/accents.txt` → `pitch.sqlite`,
  `data/source_files/raw/novels_freq.txt` → `frequency.sqlite`. **CC BY-SA 4.0 —
  attribution required**, which this file (plus `AGENTS.md`) provides. Do not strip
  this entry when redistributing the built databases.

## Fonts (checked in under `assets/fonts/`)

- **Noto Sans CJK JP (Regular, `.ttc`)** — embedded fallback when no system CJK font is
  found (probe order: Noto Sans CJK JP, Yu Gothic, Meiryo, IPAGothic). SIL Open Font
  License 1.1. The OFL requires the license text to accompany redistribution of the
  font file — kept alongside the font (or re-added from upstream) before any release
  packaging that includes `assets/fonts/`.

## Tokenizer dictionary (Rust dependency data)

- **lindera + embedded IPADIC** (`src/tokenize.rs`) — Japanese morphological analysis
  runs on lindera's bundled IPADIC dictionary data. IPADIC derives from ChaSen/NAIST work
  with its own redistribution terms; consult the lindera project and the IPADIC licence
  notice before redistributing the app binary commercially.

## External tools (never linked — spawned as child processes, user-installed)

These are separate programs with their own licences. The app only executes them; their
licences do not apply to MediaLingual-Native itself, but the user installing them
accepts those terms from their own distro.

- **mpv** (native playback workflow, `MpvIpcClock`). GPLv2+.
- **llama.cpp server** (sentence explanations, OpenAI-compatible endpoint). MIT.

## Rust crates

All direct and transitive Rust dependencies are pinned in `Cargo.lock`. Most of the
working set (eframe/egui, tokio, reqwest, rusqlite, serde, thiserror, anyhow, cpal,
xcap, zbus, `global-hotkey`, `mp3lame_encoder`, …) is MIT/Apache-2.0 dual-licensed;
`whisper-rs` (optional `whisper` feature) pulls in whisper.cpp (MIT). Exception to be
aware of: linking choices for static system libs (SQLite via `libsqlite3-sys`,
LAME via `mp3lame-sys`) follow those C libraries' terms. Run a `cargo deny`-style
audit before any binary release.
