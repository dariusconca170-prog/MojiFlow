//! cpal loopback capture into a [`AudioRing`].
//!
//! Capture runs on a dedicated thread (cpal streams are not `Send` on every backend), writes
//! into a lock-free ring shared with the export path, and reconnects with exponential backoff
//! when the device disappears (unplugged monitor, PipeWire restart). The UI only ever reads
//! atomics or a short-lived `RwLock` on the ring handle, never the audio thread.
//!
//! Selection prefers a *loopback/monitor* source when no device is configured, which is how
//! Linux PipeWire/PulseAudio expose "what's playing" and the Windows WASAPI equivalent appears.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::audio::ring::AudioRing;
use crate::config::AudioConfig;
use crate::error::AudioError;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// A selectable input device, as offered by the settings/monitor picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDeviceInfo {
    /// Stable backend id (persisted in config when the name is ambiguous).
    pub id: String,
    pub name: String,
    /// True when the name looks like a loopback/monitor of an output device.
    pub is_loopback: bool,
}

/// A handle to a running capture thread. Dropping it stops capture and joins the thread.
pub struct AudioCapture {
    ring: Arc<Mutex<Option<Arc<AudioRing>>>>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<CaptureStatus>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Last-known capture state, read by the UI for the status strip.
#[derive(Debug, Clone, Default)]
pub struct CaptureStatus {
    pub device: String,
    pub sample_rate: u32,
    pub channels: u16,
    /// Set while capture is failing and retrying.
    pub last_error: Option<String>,
}

impl AudioCapture {
    /// Start capturing `config.audio` in the background. Never fails immediately: an
    /// unavailable device is retried and reported through [`CaptureStatus`].
    pub fn start(config: &AudioConfig) -> Self {
        let ring = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(CaptureStatus::default()));

        let thread = {
            let ring = Arc::clone(&ring);
            let stop = Arc::clone(&stop);
            let status = Arc::clone(&status);
            let wanted = config.device.clone();
            let seconds = config.buffer_seconds;
            std::thread::Builder::new()
                .name("audio-capture".to_owned())
                .spawn(move || capture_loop(&wanted, seconds, &ring, &status, &stop))
                .ok()
        };

        Self {
            ring,
            stop,
            status,
            thread,
        }
    }

    /// The ring currently being filled, if a device was opened successfully.
    pub fn ring(&self) -> Option<Arc<AudioRing>> {
        self.ring.lock().ok().and_then(|slot| slot.clone())
    }

    /// Snapshot of the current capture state.
    pub fn status(&self) -> CaptureStatus {
        self.status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_default()
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// List input devices, flagging likely loopback/monitor sources.
pub fn list_input_devices() -> Result<Vec<AudioDeviceInfo>, AudioError> {
    let host = cpal::default_host();
    let devices = host
        .input_devices()
        .map_err(|err| AudioError::Stream(err.to_string()))?;
    let mut out = Vec::new();
    for device in devices {
        let name = device
            .description()
            .map(|description| description.name().to_owned())
            .unwrap_or_else(|_| device.to_string());
        let id = device
            .id()
            .map(|id| id.to_string())
            .unwrap_or_else(|_| name.clone());
        out.push(AudioDeviceInfo {
            id,
            is_loopback: is_loopback_name(&name),
            name,
        });
    }
    Ok(out)
}

/// Heuristic: does this device name look like an output-loopback / monitor source?
///
/// Covers PipeWire/PulseAudio `*.monitor`, Windows "Stereo Mix" / "What U Hear", macOS
/// BlackHole/Loopback, and generic "loopback" labels.
pub fn is_loopback_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "monitor",
        "loopback",
        "stereo mix",
        "what u hear",
        "what you hear",
        "blackhole",
        "soundflower",
        "vb-audio",
        "cable output",
    ];
    NEEDLES.iter().any(|needle| lower.contains(needle))
}

/// Select the device to open: configured name/id if given, else the first loopback source,
/// else the system default input.
fn select_device(wanted: &str) -> Result<cpal::Device, AudioError> {
    let host = cpal::default_host();
    let mut devices = host
        .input_devices()
        .map_err(|err| AudioError::Stream(err.to_string()))?;

    if wanted.trim().is_empty() {
        let loopback = devices.find(|device| {
            device
                .description()
                .map(|description| is_loopback_name(description.name()))
                .unwrap_or(false)
        });
        if let Some(device) = loopback {
            return Ok(device);
        }
        // The iterator is consumed; rebuild it to find the default.
        return host
            .default_input_device()
            .ok_or_else(|| AudioError::InputUnavailable("no input device present".to_owned()));
    }

    let lower = wanted.to_ascii_lowercase();
    let found = devices.find(|device| {
        let name = device
            .description()
            .map(|description| description.name().to_ascii_lowercase())
            .unwrap_or_default();
        let id = device.id().map(|id| id.to_string()).unwrap_or_default();
        name.contains(&lower) || id == wanted
    });
    found
        .ok_or_else(|| AudioError::InputUnavailable(format!("no input device matching '{wanted}'")))
}

fn capture_loop(
    wanted: &str,
    seconds: f32,
    ring: &Arc<Mutex<Option<Arc<AudioRing>>>>,
    status: &Arc<Mutex<CaptureStatus>>,
    stop: &Arc<AtomicBool>,
) {
    let mut backoff = Duration::from_secs(1);
    while !stop.load(Ordering::Acquire) {
        match run_device(wanted, seconds, ring, status, stop) {
            Ok(()) => break,
            Err(err) => {
                tracing::warn!(error = %err, "audio capture failed; retrying with backoff");
                if let Ok(mut status) = status.lock() {
                    status.last_error = Some(err.to_string());
                }
                if sleep_interruptible(backoff, stop) {
                    break;
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// Open the selected device, publish the ring, and pump until stopped or the stream errors.
fn run_device(
    wanted: &str,
    seconds: f32,
    ring_slot: &Arc<Mutex<Option<Arc<AudioRing>>>>,
    status: &Arc<Mutex<CaptureStatus>>,
    stop: &Arc<AtomicBool>,
) -> Result<(), AudioError> {
    let device = select_device(wanted)?;
    let supported = device
        .default_input_config()
        .map_err(|err| AudioError::Stream(err.to_string()))?;
    let config = supported.config();
    let sample_rate = config.sample_rate;
    let channels = config.channels as usize;

    let ring = Arc::new(AudioRing::new(sample_rate, seconds.max(1.0)));
    {
        let mut slot = ring_slot
            .lock()
            .map_err(|_| AudioError::Stream("ring slot poisoned".to_owned()))?;
        *slot = Some(Arc::clone(&ring));
    }

    let device_name = device
        .description()
        .map(|description| description.name().to_owned())
        .unwrap_or_else(|_| device.to_string());
    if let Ok(mut status) = status.lock() {
        status.device = device_name.clone();
        status.sample_rate = sample_rate;
        status.channels = channels as u16;
        status.last_error = None;
    }
    tracing::info!(
        device = %device_name,
        sample_rate,
        channels,
        format = ?supported.sample_format(),
        "audio capture started"
    );

    let stream_error = Arc::new(AtomicBool::new(false));
    let stream_ring = Arc::clone(&ring);
    let error_flag = Arc::clone(&stream_error);
    let stream = device
        .build_input_stream_raw(
            config,
            supported.sample_format(),
            move |data: &cpal::Data, _info| push_data(data, channels, &stream_ring),
            move |err| {
                tracing::error!(error = %err, "audio input stream error");
                error_flag.store(true, Ordering::Release);
            },
            None,
        )
        .map_err(|err| AudioError::Stream(err.to_string()))?;
    stream
        .play()
        .map_err(|err| AudioError::Stream(err.to_string()))?;

    while !stop.load(Ordering::Acquire) && !stream_error.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(100));
    }
    if stream_error.load(Ordering::Acquire) {
        return Err(AudioError::Stream(
            "input stream stopped unexpectedly".to_owned(),
        ));
    }
    Ok(())
}

/// Sleep up to `total`, waking early so shutdown is responsive. Returns true if stopped.
fn sleep_interruptible(total: Duration, stop: &Arc<AtomicBool>) -> bool {
    let step = Duration::from_millis(100);
    let mut slept = Duration::ZERO;
    while slept < total {
        if stop.load(Ordering::Acquire) {
            return true;
        }
        std::thread::sleep(step);
        slept += step;
    }
    stop.load(Ordering::Acquire)
}

/// Down-mix one cpal buffer to mono and append it to the ring.
fn push_data(data: &cpal::Data, channels: usize, ring: &AudioRing) {
    use cpal::SampleFormat;
    macro_rules! dispatch {
        ($($variant:path => $ty:ty),* $(,)?) => {
            match data.sample_format() {
                $( $variant => {
                    if let Some(samples) = data.as_slice::<$ty>() {
                        downmix_slice(samples, channels, ring);
                    }
                } )*
                other => {
                    tracing::warn!(format = ?other, "unsupported audio sample format; dropping buffer");
                }
            }
        };
    }
    dispatch! {
        SampleFormat::F32 => f32,
        SampleFormat::F64 => f64,
        SampleFormat::I8 => i8,
        SampleFormat::I16 => i16,
        SampleFormat::I32 => i32,
        SampleFormat::I64 => i64,
        SampleFormat::U8 => u8,
        SampleFormat::U16 => u16,
        SampleFormat::U32 => u32,
        SampleFormat::U64 => u64,
        SampleFormat::I24 => cpal::I24,
        SampleFormat::U24 => cpal::U24,
    }
}

/// Converts a native PCM sample into the `[-1, 1]` `f32` range the ring stores.
trait ToFloat {
    fn to_float(self) -> f32;
}

impl ToFloat for f32 {
    fn to_float(self) -> f32 {
        self
    }
}
impl ToFloat for f64 {
    fn to_float(self) -> f32 {
        self as f32
    }
}
impl ToFloat for i8 {
    fn to_float(self) -> f32 {
        self as f32 / 128.0
    }
}
impl ToFloat for i16 {
    fn to_float(self) -> f32 {
        self as f32 / 32_768.0
    }
}
impl ToFloat for i32 {
    fn to_float(self) -> f32 {
        self as f32 / 2_147_483_648.0
    }
}
impl ToFloat for i64 {
    fn to_float(self) -> f32 {
        self as f32 / 9_223_372_036_854_775_808.0
    }
}
impl ToFloat for u8 {
    fn to_float(self) -> f32 {
        (self as f32 - 128.0) / 128.0
    }
}
impl ToFloat for u16 {
    fn to_float(self) -> f32 {
        (self as f32 - 32_768.0) / 32_768.0
    }
}
impl ToFloat for u32 {
    fn to_float(self) -> f32 {
        (self as f32 - 2_147_483_648.0) / 2_147_483_648.0
    }
}
impl ToFloat for u64 {
    fn to_float(self) -> f32 {
        (self as f32 - 9_223_372_036_854_775_808.0) / 9_223_372_036_854_775_808.0
    }
}
impl ToFloat for cpal::I24 {
    fn to_float(self) -> f32 {
        self.inner() as f32 / 8_388_608.0
    }
}
impl ToFloat for cpal::U24 {
    fn to_float(self) -> f32 {
        (self.inner() as f32 - 8_388_608.0) / 8_388_608.0
    }
}

/// Average each interleaved frame to one `f32` and push it. Extracted so the conversion is
/// testable with typed slices and no `unsafe`.
fn downmix_slice<T: ToFloat + Copy>(samples: &[T], channels: usize, ring: &AudioRing) {
    if channels == 0 {
        return;
    }
    for frame in samples.chunks(channels) {
        let sum: f32 = frame.iter().map(|sample| sample.to_float()).sum();
        ring.push(sum / frame.len() as f32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_loopback_names() {
        assert!(is_loopback_name(
            "alsa_output.pci-0000_00.1.analog-stereo.monitor"
        ));
        assert!(is_loopback_name("Stereo Mix (Realtek Audio)"));
        assert!(is_loopback_name("BlackHole 2ch"));
        assert!(is_loopback_name("CABLE Output (VB-Audio Virtual Cable)"));
        assert!(!is_loopback_name("Built-in Microphone"));
        assert!(!is_loopback_name("USB Headset"));
    }

    #[test]
    fn downmix_float_frames_averages_channels() {
        let ring = AudioRing::with_capacity(1, 16);
        downmix_slice(&[1.0_f32, 0.0, 0.5, 0.5, -1.0, 1.0], 2, &ring);
        assert_eq!(ring.read(0, 3).unwrap(), vec![0.5, 0.5, 0.0]);
    }

    #[test]
    fn downmix_integer_samples_normalizes() {
        let ring = AudioRing::with_capacity(1, 16);
        // i16::MAX ~= 1.0, and a centered stereo pair averages to ~0.
        downmix_slice(&[i16::MAX, -i16::MAX, i16::MIN, i16::MIN], 2, &ring);
        let got = ring.read(0, 2).unwrap();
        assert!(got[0].abs() < 1e-4, "got {got:?}");
        assert!((got[1] + 1.0).abs() < 1e-3, "got {got:?}");
    }
}
