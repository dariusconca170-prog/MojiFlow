//! Live subtitle clock for the `whisper_live` source: transcribes the capture ring
//! locally with whisper.cpp (the `whisper` cargo feature) and advances media position
//! by the transcribed lines — subtitles "timestamped on arrival", from the user's own
//! audio, with no subtitle file needed.
//!
//! Reading the capture [`AudioRing`] concurrently with the export path is safe: the
//! ring is a *last-heard audio* buffer whose reads are absolute-sample windows against
//! a monotonic write horizon, with no exclusive cursor (see `src/audio/ring.rs`).
//!
//! Pipeline per poll tick (dedicated worker thread, never the UI thread):
//! 1. drain new samples from the ring (skip the tiny tail block),
//! 2. gate each ~100 ms block with the energy VAD (`crate::stt::is_speech`); speech
//!    starts a chunk, trailing silence ≥ `END_GAP_MS` closes it (below-minimum blips
//!    are discarded, never flushed to the model),
//! 3. resample the chunk to 16 kHz and `SttEngine::transcribe` it,
//! 4. emit each line as `CoreEvent::LiveCue` (App appends it to its live track) and
//!    advance the shared position to the newest segment end.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::app::{CoreEvent, RepaintHandle};
use crate::audio::ring::AudioRing;
use crate::clock::{PlaybackClock, SyncState};
use crate::config::SttConfig;
use crate::stt::{is_speech, resample_to_16k, SttEngine, SttError};
use crate::subs::Cue;

/// Poll cadence for new audio plus trailing-silence accounting.
const POLL: Duration = Duration::from_millis(100);
/// Minimum speech in a chunk before it is worth transcribing (ms).
const MIN_SPEECH_MS: u64 = 500;
/// Trailing silence that closes a speech chunk (ms).
const END_GAP_MS: u64 = 900;
/// Hard cap on a chunk's duration (ms) so a pathological "speech" stretch still flushes.
const MAX_CHUNK_MS: u64 = 30_000;
/// `playing` stays true for this long after the last speech block (status strip).
const PLAYING_HOLD: Duration = Duration::from_secs(2);

struct Shared {
    position: Duration,
    playing: bool,
}

/// Worker-scoped handles the reader thread owns. Bundled so `reader_loop`/`flush` stay
/// under clippy's argument ceiling and the owned-data story for the thread is explicit.
struct WorkerCtx {
    vad: f32,
    language: Option<String>,
    events: crossbeam_channel::Sender<CoreEvent>,
    repaint: Arc<RepaintHandle>,
    shared: Arc<Mutex<Shared>>,
    shutdown: Arc<AtomicBool>,
}

pub struct WhisperLiveClock {
    shared: Arc<Mutex<Shared>>,
    shutdown: Arc<AtomicBool>,
}

impl WhisperLiveClock {
    /// Load the model and spawn the reader thread. Fails typed (with a toast from App)
    /// when the model file is missing or the feature is off — the manual clock keeps the
    /// overlay usable meanwhile.
    pub fn spawn(
        ring: Arc<AudioRing>,
        config: &SttConfig,
        events: crossbeam_channel::Sender<CoreEvent>,
        repaint: Arc<RepaintHandle>,
    ) -> Result<Self, SttError> {
        let engine = SttEngine::load(Path::new(&config.whisper_model_path), false)?;
        // Copy the pieces the worker needs out of `config` — the thread must own them.
        let language = if config.language.trim().is_empty() {
            None
        } else {
            Some(config.language.clone())
        };
        let vad = config.vad_threshold.max(0.01);
        let shared = Arc::new(Mutex::new(Shared {
            position: Duration::ZERO,
            playing: false,
        }));
        let shutdown = Arc::new(AtomicBool::new(false));
        let ctx = WorkerCtx {
            vad,
            language,
            events,
            repaint: Arc::clone(&repaint),
            shared: Arc::clone(&shared),
            shutdown: Arc::clone(&shutdown),
        };

        std::thread::Builder::new()
            .name("whisper-live-clock".to_owned())
            .spawn(move || reader_loop(&engine, &ring, &ctx))
            .map_err(|err| SttError::Transcribe(err.to_string()))?;
        Ok(Self { shared, shutdown })
    }
}

impl PlaybackClock for WhisperLiveClock {
    fn now(&self) -> Duration {
        self.shared
            .lock()
            .map(|s| s.position)
            .unwrap_or(Duration::ZERO)
    }

    fn is_playing(&self) -> bool {
        self.shared.lock().map(|s| s.playing).unwrap_or(false)
    }

    fn source_name(&self) -> &'static str {
        "whisper-live"
    }

    fn sync_state(&self) -> SyncState {
        if self.shutdown.load(Ordering::SeqCst) {
            SyncState::Stale("stopped".to_owned())
        } else {
            SyncState::Live
        }
    }

    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

fn reader_loop(engine: &SttEngine, ring: &AudioRing, ctx: &WorkerCtx) {
    let rate = ring.sample_rate().max(1) as u64;
    let block_len = (rate / 10).max(1) as usize; // ~100 ms at the device rate
    let block_ms = 100u64;

    let mut next = ring.written();
    let mut chunk: Vec<f32> = Vec::new();
    let mut chunk_start_sample: usize = 0; // ring-time absolute index of the first speech block
    let mut chunk_speech_ms: u64 = 0;
    let mut trailing_silence_ms: u64 = 0;
    let mut last_speech: Option<Instant> = None;

    loop {
        if ctx.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let horizon = ring.written();
        if horizon > next {
            let samples = match ring.read(next, horizon) {
                Ok(samples) => samples,
                Err(_) => {
                    // The ring wrapped past our cursor (we stalled too long, or capture
                    // restarted): transcription continuity is lost — jump to the horizon
                    // and reset the chunk, never flush stale audio.
                    next = horizon;
                    chunk.clear();
                    chunk_speech_ms = 0;
                    trailing_silence_ms = 0;
                    continue;
                }
            };
            next = horizon;

            let mut cur = horizon - samples.len();
            for block in samples.chunks(block_len) {
                let block_abs = cur;
                cur += block.len();
                if block.len() < block_len / 2 {
                    // Tiny tail block: treat as trailing silence only (never starts speech).
                    if !chunk.is_empty() {
                        chunk.extend_from_slice(block);
                        trailing_silence_ms += block_ms;
                    }
                    continue;
                }
                if is_speech(block, ctx.vad) {
                    if chunk.is_empty() {
                        chunk_start_sample = block_abs;
                    }
                    chunk.extend_from_slice(block);
                    chunk_speech_ms += block_ms;
                    trailing_silence_ms = 0;
                    last_speech = Some(Instant::now());
                    // Advance position with the live audio so the current line stays put
                    // even before the model has flushed this chunk.
                    if let Ok(mut s) = ctx.shared.lock() {
                        s.position =
                            Duration::from_secs_f64((block_abs + block.len()) as f64 / rate as f64);
                        s.playing = true;
                    }
                } else if !chunk.is_empty() {
                    chunk.extend_from_slice(block);
                    trailing_silence_ms += block_ms;
                }
            }
        }

        let chunk_ms = chunk.len() as u64 * 1000 / rate;
        let over_gap = !chunk.is_empty() && trailing_silence_ms >= END_GAP_MS;
        let too_long =
            chunk_speech_ms >= MIN_SPEECH_MS && chunk_ms >= MAX_CHUNK_MS && !chunk.is_empty();
        let flush_ready = over_gap || too_long;
        if flush_ready {
            if chunk_speech_ms >= MIN_SPEECH_MS {
                flush(&chunk, chunk_start_sample, rate, engine, ctx);
            }
            chunk.clear();
            chunk_speech_ms = 0;
            trailing_silence_ms = 0;
        }

        let playing = last_speech
            .map(|t| t.elapsed() < PLAYING_HOLD)
            .unwrap_or(false);
        if let Ok(mut s) = ctx.shared.lock() {
            s.playing = playing;
        }
        ctx.repaint.request();
        std::thread::sleep(POLL);
    }
}

fn flush(chunk: &[f32], chunk_start_sample: usize, rate: u64, engine: &SttEngine, ctx: &WorkerCtx) {
    let base_s = chunk_start_sample as f64 / rate as f64;
    let mut last_end_s = base_s;
    match engine.transcribe(
        &resample_to_16k(chunk, rate as u32),
        ctx.language.as_deref(),
    ) {
        Ok(segments) => {
            for seg in segments {
                let start_s = base_s + seg.start_ms as f64 / 1000.0;
                let end_s = base_s + seg.end_ms as f64 / 1000.0;
                last_end_s = last_end_s.max(end_s);
                let _ = ctx.events.send(CoreEvent::LiveCue {
                    cue: Cue {
                        start: Duration::from_secs_f64(start_s),
                        end: Duration::from_secs_f64(end_s),
                        text: seg.text,
                    },
                });
            }
            if let Ok(mut s) = ctx.shared.lock() {
                s.position = Duration::from_secs_f64(last_end_s);
                s.playing = true;
            }
            ctx.repaint.request();
        }
        Err(err) => {
            let _ = ctx.events.send(CoreEvent::Status {
                component: "whisper-live",
                text: format!("transcribe error: {err}"),
            });
        }
    }
}
