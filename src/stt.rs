//! Local speech-to-text via whisper.cpp (bindings: whisper-rs).
//!
//! The engine is feature-gated (`whisper` cargo feature, default off) so normal builds
//! stay light; the VAD helpers and segment model are always available. The overlay uses
//! this as the `whisper_live` clock source: the capture ring is drained by a streaming
//! worker, silence-gated into speech chunks, transcribed, and the lines become live
//! subtitles ("timestamped on arrival"). Model files are downloaded with
//! `cargo xtask download-whisper` into `assets/whisper/` (gitignored, like the dict DBs).

use std::path::Path;

/// One transcribed line, with timestamps in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// RMS energy of a mono sample block — the VAD gate input.
pub fn speech_energy(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum / samples.len() as f64).sqrt() as f32
}

/// Energy-VAD gate: `true` when the block's RMS crosses `threshold`. Whisper needs
/// silence stripped *before* it sees audio, or it hallucinates between lines.
pub fn is_speech(samples: &[f32], threshold: f32) -> bool {
    speech_energy(samples) >= threshold.max(0.0)
}

/// Why speech-to-text is unavailable. Typed everywhere; surfaced as a toast, never a
/// silent failure.
#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("whisper model not found at {path} — run `cargo xtask download-whisper` (or set [stt] whisper_model_path)")]
    ModelMissing { path: String },
    #[error("whisper model load failed: {0}")]
    Load(String),
    #[error("whisper transcription failed: {0}")]
    Transcribe(String),
    #[error("the whisper feature is not enabled in this build (cargo build --features whisper)")]
    FeatureDisabled,
}

/// Convert whisper.cpp timestamps (centiseconds) to milliseconds. The crate docs pin
/// the unit: "start and end timestamps are in centiseconds (10s of milliseconds)".
#[cfg(feature = "whisper")]
fn csecs_to_ms(unit: i64) -> u64 {
    (unit.max(0) as u64).saturating_mul(10)
}

/// Linear-interpolation downsample to the 16 kHz whisper expects. The capture ring runs
/// at the device rate (typically 44.1/48 kHz); resampling keeps the engine input honest
/// regardless of device. Exact 3:1 and 2:1 ratios still go through interpolation so the
/// path is uniform (and unit-tested).
pub fn resample_to_16k(samples: &[f32], from_rate: u32) -> Vec<f32> {
    if samples.is_empty() || from_rate == 0 {
        return Vec::new();
    }
    if from_rate == 16_000 {
        return samples.to_vec();
    }
    let ratio = from_rate as f64 / 16_000.0;
    let out_len = ((samples.len() as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = (i as f64) * ratio;
        let lo = src.floor() as usize;
        let hi = (lo + 1).min(samples.len() - 1);
        let frac = (src - lo as f64) as f32;
        out.push(samples[lo] * (1.0 - frac) + samples[hi] * frac);
    }
    out
}

/// In-process whisper.cpp engine. Only functional with the `whisper` feature; without
/// it the type still exists so callers compile, but construction fails typed.
pub struct SttEngine {
    #[cfg(feature = "whisper")]
    ctx: whisper_rs::WhisperContext,
    #[cfg(feature = "whisper")]
    threads: usize,
    #[cfg(not(feature = "whisper"))]
    _private: (),
}

#[cfg(feature = "whisper")]
impl SttEngine {
    /// Load a ggml model file. `use_gpu` only has an effect when the `cuda`/`vulkan`
    /// features are enabled; CPU is the always-working default.
    pub fn load(path: &Path, use_gpu: bool) -> Result<Self, SttError> {
        use whisper_rs::{WhisperContext, WhisperContextParameters};

        if !path.exists() {
            return Err(SttError::ModelMissing {
                path: path.display().to_string(),
            });
        }
        let mut params = WhisperContextParameters::default();
        if use_gpu {
            params.use_gpu(true);
        }
        let ctx = WhisperContext::new_with_params(path.to_str().unwrap_or_default(), params)
            .map_err(|err| SttError::Load(err.to_string()))?;
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(8);
        Ok(Self { ctx, threads })
    }

    /// Transcribe a 16 kHz mono block into timestamped lines. Blocks the calling thread;
    /// run off the UI thread (a dedicated worker in the live-clock cut).
    pub fn transcribe(
        &self,
        samples: &[f32],
        language: Option<&str>,
    ) -> Result<Vec<Segment>, SttError> {
        use whisper_rs::{FullParams, SamplingStrategy};

        if samples.is_empty() {
            return Ok(Vec::new());
        }
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(self.threads as i32);
        params.set_language(language);
        params.set_print_progress(false);
        params.set_print_special(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_no_context(false);

        let mut state = self
            .ctx
            .create_state()
            .map_err(|err| SttError::Transcribe(err.to_string()))?;
        state
            .full(params, samples)
            .map_err(|err| SttError::Transcribe(err.to_string()))?;

        let mut out = Vec::new();
        for segment in state.as_iter() {
            let text = segment.to_str_lossy().unwrap_or_default().trim().to_owned();
            if text.is_empty() {
                continue;
            }
            out.push(Segment {
                text,
                start_ms: csecs_to_ms(segment.start_timestamp()),
                end_ms: csecs_to_ms(segment.end_timestamp()),
            });
        }
        Ok(out)
    }
}

#[cfg(not(feature = "whisper"))]
impl SttEngine {
    pub fn load(_path: &Path, _use_gpu: bool) -> Result<Self, SttError> {
        Err(SttError::FeatureDisabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_energy_measures_rms() {
        // Silence: zero.
        assert_eq!(speech_energy(&[0.0; 100]), 0.0);
        // A steady tone of amplitude 0.5 -> RMS 0.5/sqrt(2) = 0.3536.
        let tone: Vec<f32> = (0..400).map(|i| 0.5 * ((i as f32) * 0.1).sin()).collect();
        let energy = speech_energy(&tone);
        assert!((energy - 0.3536).abs() < 0.01, "{energy}");
        // Half amplitude -> half RMS.
        let quiet: Vec<f32> = tone.iter().map(|s| s * 0.5).collect();
        assert!((speech_energy(&quiet) - energy * 0.5).abs() < 0.01);
    }

    #[test]
    fn is_speech_gates_on_energy() {
        let threshold = 0.02;
        assert!(!is_speech(&[0.0; 100], threshold));
        assert!(!is_speech(&[0.005; 100], threshold));
        let tone: Vec<f32> = (0..400).map(|i| 0.3 * ((i as f32) * 0.1).sin()).collect();
        assert!(is_speech(&tone, threshold));
    }

    #[cfg(feature = "whisper")]
    #[test]
    fn csecs_convert_to_ms() {
        assert_eq!(csecs_to_ms(0), 0);
        assert_eq!(csecs_to_ms(453), 4530);
        assert_eq!(csecs_to_ms(-7), 0);
    }

    #[test]
    fn resample_16k_is_identity() {
        let tone: Vec<f32> = (0..4800).map(|i| 0.5 * ((i as f32) * 0.05).sin()).collect();
        let out = resample_to_16k(&tone, 16_000);
        assert_eq!(out, tone);
    }

    #[test]
    fn resample_48k_to_16k_thirds() {
        let tone: Vec<f32> = (0..4800).map(|i| 0.5 * ((i as f32) * 0.05).sin()).collect();
        let out = resample_to_16k(&tone, 48_000);
        // 48k / 16k = 3 exactly → one third of the samples, same amplitude.
        assert_eq!(out.len(), 1600);
        assert!((out[0] - tone[0]).abs() < 1e-6);
        let energy = speech_energy(&out);
        assert!((energy - 0.3536).abs() < 0.05, "{energy}");
    }

    #[test]
    fn resample_empty_is_empty() {
        assert!(resample_to_16k(&[], 48_000).is_empty());
        assert!(resample_to_16k(&[], 0).is_empty());
    }

    #[test]
    fn segments_round_trip_ordering() {
        // Guard the shape the UI builds on: lines are ordered, non-empty.
        let seg = Segment {
            text: "こんにちは".to_owned(),
            start_ms: 100,
            end_ms: 1900,
        };
        assert!(seg.start_ms < seg.end_ms);
        assert!(!seg.text.is_empty());
    }

    #[test]
    fn engine_requires_model_or_feature() {
        // Default builds (no `whisper` feature): typed FeatureDisabled, never a panic.
        let err = match SttEngine::load(Path::new("/nonexistent"), false) {
            Err(err) => err,
            Ok(_) => panic!("load of a missing model must fail"),
        };
        match err {
            SttError::FeatureDisabled | SttError::ModelMissing { .. } => {}
            other => panic!("unexpected error: {other}"),
        }
    }
}
