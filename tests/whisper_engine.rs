#![cfg(feature = "whisper")]

//! Real-model speech-to-text proof. Skips green unless `ML_WHISPER_MODEL` names a ggml
//! model file and `ML_WHISPER_AUDIO` a 16-bit PCM mono WAV (any sample rate): default
//! `cargo test` stays offline and model-free; the manual proof run sets both plus
//! `ML_WHISPER_LANG` (default "ja").

use medialingual_native::stt::{resample_to_16k, Segment, SttEngine};

/// Minimal RIFF/WAVE reader for the proof: 16-bit PCM mono or f32 mono, any sample
/// rate. Returns `(mono f32, sample rate)`.
fn read_wav_mono_f32(path: &std::path::Path) -> (Vec<f32>, u32) {
    let bytes = std::fs::read(path).expect("read fixture WAV");
    assert_eq!(&bytes[..4], b"RIFF", "not a RIFF file");
    assert_eq!(&bytes[8..12], b"WAVE", "not a WAVE file");
    let mut offset = 12usize;
    let mut sample_rate = 0u32;
    let mut channels = 0u16;
    let mut bits = 0u16;
    let mut data = Vec::new();
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(
            bytes[offset + 4..offset + 8]
                .try_into()
                .expect("chunk size"),
        ) as usize;
        offset += 8;
        match id {
            b"fmt " => {
                channels = u16::from_le_bytes(bytes[offset + 2..offset + 4].try_into().unwrap());
                sample_rate = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
                bits = u16::from_le_bytes(bytes[offset + 14..offset + 16].try_into().unwrap());
            }
            b"data" => data.extend_from_slice(&bytes[offset..offset + size]),
            _ => {}
        }
        offset += size;
    }
    assert!(sample_rate > 0 && channels >= 1, "unparseable WAV header");
    let mut out = Vec::with_capacity(data.len() * 2);
    let frame_len = channels as usize;
    match bits {
        16 => {
            for frame in data.chunks_exact(2 * frame_len) {
                for sample in frame.chunks_exact(2).take(1) {
                    out.push(i16::from_le_bytes([sample[0], sample[1]]) as f32 / 32768.0);
                }
            }
        }
        32 => {
            for frame in data.chunks_exact(4 * frame_len) {
                for sample in frame.chunks_exact(4).take(1) {
                    out.push(f32::from_le_bytes(sample.try_into().unwrap()));
                }
            }
        }
        other => panic!("unsupported bit depth {other}"),
    }
    (out, sample_rate)
}

#[test]
fn real_engine_transcribes_proof() {
    let (Ok(model), Ok(audio)) = (
        std::env::var("ML_WHISPER_MODEL"),
        std::env::var("ML_WHISPER_AUDIO"),
    ) else {
        eprintln!(
            "skipping real-model proof: set ML_WHISPER_MODEL (ggml file) and \
             ML_WHISPER_AUDIO (16-bit PCM mono WAV)"
        );
        return;
    };
    let lang = std::env::var("ML_WHISPER_LANG").unwrap_or_else(|_| "ja".to_owned());
    let (raw, rate) = read_wav_mono_f32(std::path::Path::new(&audio));
    assert!(!raw.is_empty(), "fixture audio is empty");
    let samples = resample_to_16k(&raw, rate);
    let engine = SttEngine::load(std::path::Path::new(&model), false)
        .expect("model must load in a proof run");
    let segments: Vec<Segment> = engine
        .transcribe(&samples, Some(&lang))
        .expect("transcribe must succeed in a proof run");
    assert!(
        !segments.is_empty(),
        "expected at least one transcribed line"
    );
    let total_ms = samples.len() as u64 * 1000 / 16_000;
    for seg in &segments {
        assert!(
            seg.start_ms <= seg.end_ms,
            "segment times out of order: {seg:?}"
        );
        assert!(
            seg.end_ms <= total_ms.saturating_add(2_000),
            "segment end {seg:?} beyond clip ({total_ms} ms)"
        );
    }
    let joined: Vec<&str> = segments.iter().map(|s| s.text.as_str()).collect();
    eprintln!("proof transcription ({joined:?})");
}
