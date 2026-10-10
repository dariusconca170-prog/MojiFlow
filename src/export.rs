//! Sentence-mining export (M7): cue + dictionary + ring audio + screenshot → Anki note.
//!
//! Two halves, split so the UI thread never blocks on I/O and the per-thread [`Dictionary`]
//! never crosses threads:
//!
//! 1. **Gather (UI thread, [`crate::app::App::export_current_card`])** — resolve the chosen
//!    token against the dictionary, compute the audio window around the cue, and package a
//!    plain [`ExportSource`] with everything a worker needs (no `Dictionary` inside).
//! 2. **Export (worker thread, its own tokio runtime)** — slice the ring → MP3, screenshot
//!    the video window → JPEG, render the `field_mapping` templates, `ensureModel`, upload
//!    media, `addNote`. When Anki is unreachable the rendered note (media included) is
//!    persisted to the [`OfflineQueue`] and up to a few queued notes are retried after the
//!    next success. A duplicate note is *success* with a message — never queued offline.
//!
//! Media that cannot be gathered (buffer too short, window gone) is skipped with a note in
//! the result message — the card itself is still exported.

use std::path::PathBuf;
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, TrySendError};

use crate::anki::connect::AnkiConnect;
use crate::anki::media::MediaFile;
use crate::anki::note::{CardData, PendingNote};
use crate::anki::queue::OfflineQueue;
use crate::audio::encode::encode_mono;
use crate::audio::ring::AudioRing;
use crate::capture::screen::capture_jpeg;
use crate::config::{AnkiConfig, CaptureConfig};
use crate::dict::Resolution;
use crate::error::{AnkiError, ExportError};

/// Everything the export worker needs — all plain data, no locks, no display handles.
pub struct ExportSource {
    /// Rendered-from-dictionary card contents (word, reading, definition, …).
    pub card: MinedCard,
    pub anki: AnkiConfig,
    /// Screenshot target (window title or manual region).
    pub capture: CaptureConfig,
    /// Directory of the offline JSON queue (config dir + `/queue`).
    pub queue_dir: PathBuf,
    /// Current loopback ring, if the capture thread has one running.
    pub ring: Option<Arc<AudioRing>>,
    /// Audio window in seconds `(start, end)` relative to the media timeline; `None` when
    /// the cue has no usable window.
    pub audio_window: Option<(f64, f64)>,
    /// Per-export nonce for media filenames (avoids Anki-side collisions).
    pub nonce: u64,
}

/// Plain card contents gathered on the UI thread (mirrors `KNOWN_PLACEHOLDERS` minus media).
#[derive(Debug, Clone, Default)]
pub struct MinedCard {
    pub word: String,
    pub reading: String,
    pub definition: String,
    pub sentence: String,
    pub sentence_furigana: String,
    pub pitch: String,
    pub frequency: String,
    pub source: String,
}

/// Build a [`MinedCard`] from the dictionary resolution of one surface. Called on the UI
/// thread where the [`Dictionary`] lives; the result is plain data for the worker.
/// ``resolution` is `None` when the dictionary is absent or the surface did not resolve —
/// the word itself is still exported, never silently dropped.
pub fn mine_card(
    surface: &str,
    resolution: Option<&Resolution>,
    sentence: &str,
    sentence_furigana: &str,
    cue_start_s: f64,
    source_name: &str,
) -> MinedCard {
    let source = format!("{} @ {}", source_name, format_timestamp(cue_start_s));
    let Some(entry) = resolution.and_then(|res| res.entries.first()) else {
        return MinedCard {
            word: surface.to_owned(),
            sentence: sentence.to_owned(),
            sentence_furigana: sentence_furigana.to_owned(),
            source,
            ..MinedCard::default()
        };
    };
    MinedCard {
        word: entry.term.clone(),
        reading: entry.reading.clone(),
        definition: entry.glosses.join("; "),
        sentence: sentence.to_owned(),
        sentence_furigana: sentence_furigana.to_owned(),
        pitch: entry.pitch.clone().unwrap_or_default(),
        frequency: entry
            .frequency_rank
            .map(|rank| format!("##{rank}"))
            .unwrap_or_default(),
        source,
    }
}

/// Background exporter: one worker thread with its own tokio runtime; jobs arrive over a
/// bounded channel and results/errors come back as `CoreEvent`s.
pub struct ExportWorker {
    tx: Option<Sender<ExportSource>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl ExportWorker {
    /// Spawn the worker. `events`/`repaint` are the app's usual worker wiring.
    pub fn spawn(
        events: crossbeam_channel::Sender<crate::app::CoreEvent>,
        repaint: Arc<crate::app::RepaintHandle>,
    ) -> Self {
        let (tx, rx) = crossbeam_channel::bounded::<ExportSource>(8);
        let join = std::thread::Builder::new()
            .name("export-worker".to_owned())
            .spawn(move || export_thread(rx, events, repaint))
            .ok();
        Self { tx: Some(tx), join }
    }

    /// Hand a job to the worker. Returns `false` (and the source is lost) only when the
    /// channel is full or the worker already exited — surfaced by the caller as a toast.
    pub fn submit(&self, source: ExportSource) -> bool {
        match self.tx.as_ref() {
            Some(tx) => match tx.try_send(source) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
            },
            None => false,
        }
    }

    /// Ask the worker to drain its channel and exit (drops the sender).
    pub fn stop(&mut self) {
        self.tx = None;
    }

    /// Join the worker thread, waiting for the current job to finish.
    pub fn join(&mut self) {
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

fn export_thread(
    rx: Receiver<ExportSource>,
    events: crossbeam_channel::Sender<crate::app::CoreEvent>,
    repaint: Arc<crate::app::RepaintHandle>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            let _ = events.send(crate::app::CoreEvent::Warning(
                crate::error::MlError::Export(ExportError::Worker(format!(
                    "tokio runtime failed: {err}"
                ))),
            ));
            tracing::error!(error = %err, "export worker runtime failed");
            return;
        }
    };
    runtime.block_on(worker_loop(rx, events, repaint));
}

async fn worker_loop(
    rx: Receiver<ExportSource>,
    events: crossbeam_channel::Sender<crate::app::CoreEvent>,
    repaint: Arc<crate::app::RepaintHandle>,
) {
    // crossbeam's blocking `recv` is fine here: the current-thread runtime parks while
    // waiting, and `import_one` is the only task, running to its await points.
    while let Ok(source) = rx.recv() {
        let (note, skipped) = build_note(&source);
        let outcome = send_note(&source, &note).await;
        match outcome {
            Ok(()) => {
                let suffix = if skipped.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", skipped.join("; "))
                };
                let _ = events.send(crate::app::CoreEvent::Status {
                    component: "export",
                    text: format!(
                        "exported '{}' → {} ({}){}",
                        source.card.word, source.anki.deck, note.model, suffix
                    ),
                });
                repaint.request();

                // Anki is reachable — retry a few queued notes.
                let drained = drain_queued(source.queue_dir.clone(), source.anki.clone())
                    .await
                    .unwrap_or(0);
                if drained > 0 {
                    let _ = events.send(crate::app::CoreEvent::Status {
                        component: "export",
                        text: format!("drained {drained} queued note(s)"),
                    });
                }
            }
            Err(AnkiError::Duplicate(scope)) => {
                let _ = events.send(crate::app::CoreEvent::Status {
                    component: "export",
                    text: format!(
                        "'{}' already mined (duplicate, scope: {scope})",
                        source.card.word
                    ),
                });
                repaint.request();
            }
            Err(err) => {
                // Still renderable — persist it (media included) and retry later.
                match OfflineQueue::new(source.queue_dir.clone()).push(&note) {
                    Ok(()) => {
                        let _ = events.send(crate::app::CoreEvent::Status {
                            component: "export",
                            text: format!(
                                "'{}' queued offline ({err}); retried after the next successful export",
                                source.card.word
                            ),
                        });
                        repaint.request();
                    }
                    Err(queue_err) => {
                        let _ = events.send(crate::app::CoreEvent::Warning(
                            crate::error::MlError::Export(ExportError::Queue(format!(
                                "could not persist '{word}': {queue_err}",
                                word = source.card.word
                            ))),
                        ));
                    }
                }
            }
        }
    }
}

/// Render the note and gather media (ring slice → MP3, screenshot → JPEG). Media that is
/// unavailable is skipped with a human note; the card is still exported.
fn build_note(source: &ExportSource) -> (PendingNote, Vec<String>) {
    let filename_base = format!("ml_{}_{}", sanitize_label(&source.card.word), source.nonce);
    let mut media = Vec::new();
    let mut audio_file = None;
    let mut image_file = None;
    let mut skipped = Vec::new();

    if let (Some(ring), Some((start, end))) = (&source.ring, source.audio_window) {
        match ring.read_seconds(start, end) {
            Ok(samples) if !samples.is_empty() => {
                match encode_mono(&samples, ring.sample_rate(), 128) {
                    Ok(bytes) => {
                        let file = MediaFile::from_bytes(format!("{filename_base}.mp3"), bytes);
                        audio_file = Some(file.clone());
                        media.push(file);
                    }
                    Err(err) => skipped.push(format!("audio skipped (encode: {err})")),
                }
            }
            Ok(_) => skipped.push("audio skipped (no samples in window)".to_owned()),
            Err(err) => skipped.push(format!("audio skipped ({err})")),
        }
    }

    match capture_jpeg(&source.capture) {
        Ok(bytes) => {
            let file = MediaFile::from_bytes(format!("{filename_base}.jpg"), bytes);
            image_file = Some(file.clone());
            media.push(file);
        }
        Err(err) => skipped.push(format!("image skipped ({err})")),
    }

    let card = card_from(source, audio_file, image_file);
    let note = PendingNote::from_mapping(
        &source.anki.deck,
        &source.anki.model,
        &source.anki.tags,
        &source.anki.field_mapping,
        &card,
        media,
    );
    (note, skipped)
}

fn card_from(
    source: &ExportSource,
    audio: Option<MediaFile>,
    image: Option<MediaFile>,
) -> CardData {
    CardData {
        word: source.card.word.clone(),
        reading: source.card.reading.clone(),
        definition: source.card.definition.clone(),
        sentence: source.card.sentence.clone(),
        sentence_furigana: source.card.sentence_furigana.clone(),
        pitch: source.card.pitch.clone(),
        pitch_svg: String::new(),
        frequency: source.card.frequency.clone(),
        source: source.card.source.clone(),
        audio,
        image,
    }
}

/// Deliver one note: ensure the model, upload media, `addNote`. A duplicate rejection
/// stays `Err(AnkiError::Duplicate)` so the caller can report it distinctly (it is a
/// successful mine, never queued offline).
async fn send_note(source: &ExportSource, note: &PendingNote) -> Result<(), AnkiError> {
    let client = AnkiConnect::new(&source.anki)?;
    export_note(&client, &source.anki, note).await
}

async fn export_note(
    client: &AnkiConnect,
    anki: &AnkiConfig,
    note: &PendingNote,
) -> Result<(), AnkiError> {
    let fields: Vec<String> = note.fields.keys().cloned().collect();
    client.ensure_model(&anki.model, &fields).await?;
    // Decks are NOT auto-created by addNote — ensure it explicitly (idempotent).
    client.create_deck(&note.deck).await?;
    for file in &note.media {
        client
            .store_media_file(&file.filename, &file.data_base64)
            .await?;
    }
    client
        .add_note(
            &note.deck,
            &note.model,
            &note.fields,
            &note.tags,
            false,
            &anki.duplicate_scope,
        )
        .await
        .map(|_| ())
}

/// Retry notes still sitting in the offline queue; stops on `Unreachable` to keep the rest,
/// and treats duplicates as delivered (removed) so they do not loop forever.
async fn drain_queued(queue_dir: PathBuf, anki: AnkiConfig) -> Result<usize, AnkiError> {
    let queue = OfflineQueue::new(queue_dir);
    queue
        .drain(|note| {
            let anki = anki.clone();
            async move {
                let client = AnkiConnect::new(&anki).map_err(|err| AnkiError::Unreachable {
                    url: anki.url.clone(),
                    reason: err.to_string(),
                })?;
                match export_note(&client, &anki, &note).await {
                    Err(AnkiError::Duplicate(_)) => Ok(()),
                    other => other,
                }
            }
        })
        .await
}

/// Filename-safe label for a mined word (ascii alnum and `_` kept, other runs → `-`, capped).
/// Pure so it is unit-testable.
pub fn sanitize_label(input: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in input.chars() {
        if out.len() >= 32 {
            break;
        }
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    if out.is_empty() {
        "word".to_owned()
    } else {
        out
    }
}

/// `123.4` seconds → `00:02:03.4` (media-timeline source label).
pub fn format_timestamp(secs: f64) -> String {
    let total_ms = (secs.max(0.0) * 1000.0).round() as u64;
    let ms = total_ms % 1000;
    let total_s = total_ms / 1000;
    let s = total_s % 60;
    let m = (total_s / 60) % 60;
    let h = total_s / 3600;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deinflect::Candidate;
    use crate::dict::DictEntry;

    #[test]
    fn mine_card_resolves_word_reading_definition() {
        let resolution = Resolution {
            candidate: Candidate {
                term: "食べる".to_owned(),
                reasons: Vec::new(),
            },
            entries: vec![DictEntry {
                term: "食べる".to_owned(),
                reading: "たべる".to_owned(),
                pos: vec!["v1".to_owned()],
                glosses: vec!["to eat".to_owned(), "to live on (food)".to_owned()],
                pitch: Some("1".to_owned()),
                frequency_rank: Some(42),
            }],
        };
        let card = mine_card("食べた", Some(&resolution), "食べた。", "", 125.0, "ep.srt");
        assert_eq!(card.word, "食べる");
        assert_eq!(card.reading, "たべる");
        assert_eq!(card.definition, "to eat; to live on (food)");
        assert_eq!(card.pitch, "1");
        assert_eq!(card.frequency, "##42");
        assert_eq!(card.source, "ep.srt @ 00:02:05.000");
        assert_eq!(card.sentence, "食べた。");
    }

    #[test]
    fn mine_card_unresolved_exports_surface_only() {
        let card = mine_card("未解決", None, "未解決。", "", 0.0, "t.srt");
        assert_eq!(card.word, "未解決");
        assert_eq!(card.definition, "");
        assert_eq!(card.pitch, "");
        assert_eq!(card.sentence, "未解決。");
    }

    #[test]
    fn sanitize_label_keeps_ascii_and_collapses_separators() {
        assert_eq!(sanitize_label("食べる"), "word");
        assert_eq!(sanitize_label("taberu"), "taberu");
        assert_eq!(sanitize_label("Taberu!!"), "taberu");
        assert_eq!(sanitize_label("a b c"), "a-b-c");
        assert_eq!(sanitize_label("漢字-ABC_123"), "abc_123");
        assert_eq!(
            sanitize_label("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }

    #[test]
    fn format_timestamp_is_media_timeline_shaped() {
        assert_eq!(format_timestamp(0.0), "00:00:00.000");
        assert_eq!(format_timestamp(123.456), "00:02:03.456");
        assert_eq!(format_timestamp(3600.0 + 65.5), "01:01:05.500");
        assert_eq!(format_timestamp(-3.0), "00:00:00.000");
    }

    #[test]
    fn worker_submit_rejects_when_channel_is_closed() {
        let (tx, rx) = crossbeam_channel::bounded::<ExportSource>(1);
        let mut worker = ExportWorker {
            tx: Some(tx),
            join: None,
        };
        drop(rx); // pretend the thread exited
        let source = ExportSource {
            card: MinedCard::default(),
            anki: AnkiConfig::default(),
            capture: CaptureConfig::default(),
            queue_dir: PathBuf::from("/tmp/opencode"),
            ring: None,
            audio_window: None,
            nonce: 1,
        };
        assert!(!worker.submit(source));
        worker.stop();
        assert!(!worker.submit(ExportSource {
            card: MinedCard::default(),
            anki: AnkiConfig::default(),
            capture: CaptureConfig::default(),
            queue_dir: PathBuf::from("/tmp/opencode"),
            ring: None,
            audio_window: None,
            nonce: 2,
        }));
    }
}
