//! Lock-free SPSC ring buffer for the "last-heard audio".
//!
//! One producer (the real-time audio callback) writes mono `f32` samples; one consumer (the
//! export path on a worker thread) reads arbitrary ranges back out. The callback must not
//! allocate, lock, or block, so samples live in a fixed `Box<[AtomicU32]>` holding `f32`
//! bit patterns and the write cursor is a single [`AtomicUsize`] published with release
//! ordering. Reads observe it with acquire ordering, which establishes the happens-before
//! needed to see the sample stores.
//!
//! Writes are *total*: the producer always pushes forward and older samples are overwritten
//! once the buffer wraps. A small slack of one sample is kept so the slot the producer is
//! currently writing is never handed back to a reader at the wrap boundary.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::error::AudioError;

/// A fixed-capacity, overwrite-on-wrap buffer of mono `f32` samples.
pub struct AudioRing {
    samples: Box<[AtomicU32]>,
    /// Total number of samples ever written (monotonic). The "horizon" of captured audio.
    written: AtomicUsize,
    sample_rate: u32,
}

impl AudioRing {
    /// Create a ring holding at least `seconds` of audio at `sample_rate`.
    pub fn new(sample_rate: u32, seconds: f32) -> Self {
        let frames = (seconds.max(0.05) * sample_rate.max(1) as f32).ceil() as usize;
        Self::with_capacity(sample_rate, frames.max(2))
    }

    /// Create a ring with an explicit sample capacity (at least 2).
    pub fn with_capacity(sample_rate: u32, capacity: usize) -> Self {
        let capacity = capacity.max(2);
        let samples = (0..capacity)
            .map(|_| AtomicU32::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            samples,
            written: AtomicUsize::new(0),
            sample_rate: sample_rate.max(1),
        }
    }

    /// Sample rate the ring was created for.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Number of sample slots (including the one-sample slack).
    pub fn capacity(&self) -> usize {
        self.samples.len()
    }

    /// Total samples written so far (the read horizon).
    pub fn written(&self) -> usize {
        self.written.load(Ordering::Acquire)
    }

    /// Seconds of audio currently held (bounded by the usable capacity).
    pub fn buffered_seconds(&self) -> f32 {
        let usable = self.usable_capacity();
        self.written().min(usable) as f32 / self.sample_rate as f32
    }

    fn usable_capacity(&self) -> usize {
        // One slot is reserved so the producer's current write position is never read.
        (self.capacity() - 1).max(1)
    }

    /// Producer: append one mono sample. Real-time safe (no allocation, no locks).
    pub fn push(&self, sample: f32) {
        let index = self.written.load(Ordering::Relaxed);
        let slot = index % self.capacity();
        self.samples[slot].store(sample.to_bits(), Ordering::Relaxed);
        // Release publishes the sample store above to any reader that acquires `written`.
        self.written.store(index.wrapping_add(1), Ordering::Release);
    }

    /// Producer: append a run of mono samples.
    pub fn push_mono(&self, samples: &[f32]) {
        for &sample in samples {
            self.push(sample);
        }
    }

    /// Producer: append one interleaved frame, down-mixed to mono by averaging channels.
    pub fn push_frame(&self, frame: &[f32]) {
        if frame.is_empty() {
            return;
        }
        let sum: f32 = frame.iter().copied().sum();
        self.push(sum / frame.len() as f32);
    }

    /// Producer: append `frames` interleaved samples with `channels` channels.
    pub fn push_interleaved(&self, samples: &[f32], channels: usize) {
        if channels <= 1 {
            self.push_mono(samples);
            return;
        }
        for frame in samples.chunks_exact(channels) {
            self.push_frame(frame);
        }
    }

    /// Consumer: read the samples in `[start, end)` (absolute sample indices, 0 = first ever).
    ///
    /// `start` is clamped forward to the oldest sample still held; if the whole window has been
    /// overwritten this returns [`AudioError::TooOld`], and an empty/wholly-future window
    /// returns [`AudioError::EmptyRange`].
    pub fn read(&self, start: usize, end: usize) -> Result<Vec<f32>, AudioError> {
        let horizon = self.written();
        let (start, end) = resolve_range(horizon, self.capacity(), self.sample_rate, start, end)?;
        let mut out = Vec::with_capacity(end - start);
        for index in start..end {
            let slot = index % self.capacity();
            out.push(f32::from_bits(self.samples[slot].load(Ordering::Relaxed)));
        }
        Ok(out)
    }

    /// Consumer: read the samples covering `[start_s, end_s)` seconds of captured audio.
    pub fn read_seconds(&self, start_s: f64, end_s: f64) -> Result<Vec<f32>, AudioError> {
        let rate = self.sample_rate as f64;
        let start = (start_s.max(0.0) * rate).round() as usize;
        let end = (end_s.max(0.0) * rate).round() as usize;
        self.read(start, end)
    }
}

/// Resolve a requested absolute sample range against the ring's write horizon.
///
/// Returns the clamped `[start, end)` actually readable, or a typed error. Extracted as a
/// pure function so the wraparound / clamp / underflow behaviour is unit-testable without
/// threads.
pub fn resolve_range(
    horizon: usize,
    capacity: usize,
    sample_rate: u32,
    start: usize,
    end: usize,
) -> Result<(usize, usize), AudioError> {
    if end <= start {
        return Err(AudioError::EmptyRange);
    }
    let usable = capacity.saturating_sub(1).max(1);
    let oldest = horizon.saturating_sub(usable);
    if end <= oldest {
        return Err(AudioError::TooOld {
            buffer_len_s: usable as f32 / sample_rate.max(1) as f32,
        });
    }
    let start = start.max(oldest);
    let end = end.min(horizon);
    if end <= start {
        return Err(AudioError::EmptyRange);
    }
    Ok((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_samples_before_wrapping() {
        let ring = AudioRing::with_capacity(8, 8);
        ring.push_mono(&[0.0, 1.0, 2.0, 3.0, 4.0]);
        assert_eq!(ring.written(), 5);
        assert_eq!(ring.read(1, 4).unwrap(), vec![1.0, 2.0, 3.0]);
        assert_eq!(ring.read(0, 5).unwrap(), vec![0.0, 1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn wraparound_keeps_only_the_latest_samples() {
        let ring = AudioRing::with_capacity(1000, 8);
        let all: Vec<f32> = (0..20).map(|n| n as f32).collect();
        ring.push_mono(&all);
        // capacity 8 => usable 7, horizon 20 => oldest 13.
        assert_eq!(horizon_oldest(&ring), 13);
        let tail = ring.read(13, 20).expect("tail readable");
        assert_eq!(tail, (13..20).map(|n| n as f32).collect::<Vec<_>>());
        assert!(matches!(ring.read(0, 5), Err(AudioError::TooOld { .. })));
    }

    #[test]
    fn clamps_start_forward_to_oldest_available() {
        let ring = AudioRing::with_capacity(1, 8);
        ring.push_mono(&(0..20).map(|n| n as f32).collect::<Vec<_>>());
        // Requested [10, 16) but only [13, 20) survives; start clamps to 13.
        let got = ring.read(10, 16).expect("clamped read");
        assert_eq!(got, vec![13.0, 14.0, 15.0]);
    }

    #[test]
    fn zero_length_range_is_empty_not_old() {
        let ring = AudioRing::with_capacity(1, 8);
        for i in 0..20u32 {
            ring.push(i as f32);
        }
        assert!(matches!(ring.read(5, 5), Err(AudioError::EmptyRange)));
    }

    #[test]
    fn wholly_future_range_is_empty() {
        let ring = AudioRing::with_capacity(1, 8);
        ring.push_mono(&[0.0, 1.0, 2.0]);
        // horizon 3, oldest 0 (nothing wrapped): [10, 12) clamps end to 3 => empty.
        assert!(matches!(ring.read(10, 12), Err(AudioError::EmptyRange)));
    }

    #[test]
    fn too_old_reports_buffer_length() {
        let sample_rate = 100;
        let err = resolve_range(1000, 500, sample_rate, 0, 10).unwrap_err();
        match err {
            AudioError::TooOld { buffer_len_s } => {
                // usable = 499 samples at 100 Hz => 4.99 s
                assert!((buffer_len_s - 4.99).abs() < 1e-3, "got {buffer_len_s}");
            }
            other => panic!("expected TooOld, got {other:?}"),
        }
    }

    #[test]
    fn interleaved_frames_downmix_to_mono() {
        let ring = AudioRing::with_capacity(1, 16);
        ring.push_interleaved(&[1.0, 0.0, 0.5, 0.5, -1.0, 1.0], 2);
        assert_eq!(ring.read(0, 3).unwrap(), vec![0.5, 0.5, 0.0]);
    }

    #[test]
    fn read_seconds_maps_to_sample_indices() {
        let ring = AudioRing::new(1000, 1.0); // 1000 samples
        ring.push_mono(&(0..1000).map(|n| n as f32).collect::<Vec<_>>());
        let got = ring.read_seconds(0.1, 0.105).expect("read");
        assert_eq!(got, vec![100.0, 101.0, 102.0, 103.0, 104.0]);
    }

    #[test]
    fn buffered_seconds_saturates_at_capacity() {
        let ring = AudioRing::with_capacity(10, 11); // usable 10 samples
        ring.push_mono(&[0.0; 25]);
        assert!((ring.buffered_seconds() - 1.0).abs() < 1e-6);
    }

    /// Helper mirroring the module's `oldest` formula for readable assertions.
    fn horizon_oldest(ring: &AudioRing) -> usize {
        let usable = (ring.capacity() - 1).max(1);
        ring.written().saturating_sub(usable)
    }
}
