//! MP3 encoding for the "last-heard audio" export.
//!
//! Wraps [`mp3lame_encoder`] (LAME) with mono `f32` input. The export path resolves a time
//! range against [`super::ring::AudioRing`], optionally normalizes it, then calls
//! [`encode_mono`] to produce a self-contained MP3 (headers + ID3-free frame stream) that Anki
//! can embed directly.

use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, MonoPcm, Quality};

use crate::error::AudioError;

/// A LAME `Bitrate` closest to `kbps`, falling back to 192 kbps for unknown values.
fn nearest_bitrate(kbps: u32) -> Bitrate {
    const ALLOWED: &[u32] = &[
        8, 16, 24, 32, 40, 48, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    let chosen = ALLOWED
        .iter()
        .copied()
        .min_by_key(|&b| (b as i64 - kbps as i64).abs())
        .unwrap_or(192);
    match chosen {
        8 => Bitrate::Kbps8,
        16 => Bitrate::Kbps16,
        24 => Bitrate::Kbps24,
        32 => Bitrate::Kbps32,
        40 => Bitrate::Kbps40,
        48 => Bitrate::Kbps48,
        64 => Bitrate::Kbps64,
        80 => Bitrate::Kbps80,
        96 => Bitrate::Kbps96,
        112 => Bitrate::Kbps112,
        128 => Bitrate::Kbps128,
        160 => Bitrate::Kbps160,
        192 => Bitrate::Kbps192,
        224 => Bitrate::Kbps224,
        256 => Bitrate::Kbps256,
        _ => Bitrate::Kbps320,
    }
}

/// Peak-normalize `samples` to `target` (clamped to 1.0). Silence is left untouched so a
/// quiet-but-real clip is not blown up into noise.
pub fn normalize(samples: &mut [f32], target: f32) {
    let peak = samples.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()));
    if peak < 1e-6 {
        return;
    }
    let gain = target.min(1.0) / peak;
    for sample in samples.iter_mut() {
        *sample = (*sample * gain).clamp(-1.0, 1.0);
    }
}

/// Encode mono `f32` samples (nominally in `[-1, 1]`) as a complete MP3 stream.
///
/// Samples are clamped to `[-1, 1]` first, so callers may pass raw capture data. Returns the
/// encoded bytes; an empty input still yields a valid (silent) stream header.
pub fn encode_mono(
    samples: &[f32],
    sample_rate: u32,
    bitrate_kbps: u32,
) -> Result<Vec<u8>, AudioError> {
    let mut builder = Builder::new()
        .ok_or_else(|| AudioError::Encode("could not create LAME encoder".to_owned()))?;
    builder
        .set_num_channels(1)
        .map_err(|err| AudioError::Encode(format!("channels: {err}")))?;
    builder
        .set_sample_rate(sample_rate.max(1))
        .map_err(|err| AudioError::Encode(format!("sample rate: {err}")))?;
    builder
        .set_brate(nearest_bitrate(bitrate_kbps))
        .map_err(|err| AudioError::Encode(format!("bitrate: {err}")))?;
    builder
        .set_quality(Quality::Best)
        .map_err(|err| AudioError::Encode(format!("quality: {err}")))?;
    let mut encoder = builder
        .build()
        .map_err(|err| AudioError::Encode(format!("build: {err}")))?;

    let mut out: Vec<u8> = Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(
        samples.len().max(1),
    ));
    // Feed in bounded chunks so a long (≥10 s) capture never needs one huge scratch buffer.
    const CHUNK: usize = 8192;
    let clamped: Vec<f32> = samples.iter().map(|s| s.clamp(-1.0, 1.0)).collect();
    for chunk in clamped.chunks(CHUNK) {
        out.reserve(mp3lame_encoder::max_required_buffer_size(chunk.len()));
        encoder
            .encode_to_vec(MonoPcm(chunk), &mut out)
            .map_err(|err| AudioError::Encode(format!("encode: {err}")))?;
    }
    out.reserve(7200);
    encoder
        .flush_to_vec::<FlushNoGap>(&mut out)
        .map_err(|err| AudioError::Encode(format!("flush: {err}")))?;
    Ok(out)
}

/// True when `bytes` contains at least one MPEG (Layer III) frame sync word. Used to assert
/// encoder output is a real, decodable frame stream rather than an empty buffer.
pub fn contains_frame_sync(bytes: &[u8]) -> bool {
    bytes
        .windows(2)
        .any(|w| w[0] == 0xFF && (w[1] & 0xE0) == 0xE0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, sample_rate: u32, seconds: f32) -> Vec<f32> {
        let count = (sample_rate as f32 * seconds) as usize;
        (0..count)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                (2.0 * std::f32::consts::PI * freq * t).sin() * 0.6
            })
            .collect()
    }

    #[test]
    fn encodes_a_playable_frame_stream() {
        let pcm = sine(440.0, 44_100, 0.5);
        let mp3 = encode_mono(&pcm, 44_100, 192).expect("encode");
        assert!(mp3.len() > 512, "suspiciously small mp3: {}", mp3.len());
        assert!(
            contains_frame_sync(&mp3),
            "no MPEG frame sync found in encoded output"
        );
    }

    #[test]
    fn encodes_silence_without_error() {
        let pcm = vec![0.0_f32; 44_100];
        let mp3 = encode_mono(&pcm, 44_100, 96).expect("encode silence");
        assert!(contains_frame_sync(&mp3));
    }

    #[test]
    fn normalize_scales_peak_to_target() {
        let mut pcm = vec![0.1, -0.2, 0.05];
        normalize(&mut pcm, 0.95);
        let peak = pcm.iter().fold(0.0_f32, |a, s| a.max(s.abs()));
        assert!((peak - 0.95).abs() < 1e-4, "peak = {peak}");
    }

    #[test]
    fn normalize_leaves_silence_alone() {
        let mut pcm = vec![0.0_f32; 8];
        normalize(&mut pcm, 0.95);
        assert!(pcm.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn nearest_bitrate_matches_request_and_falls_back() {
        assert!(matches!(nearest_bitrate(192), Bitrate::Kbps192));
        assert!(matches!(nearest_bitrate(200), Bitrate::Kbps192));
        assert!(matches!(nearest_bitrate(100), Bitrate::Kbps96));
        // 999 clamps to the highest allowed bitrate.
        assert!(matches!(nearest_bitrate(999), Bitrate::Kbps320));
    }
}
