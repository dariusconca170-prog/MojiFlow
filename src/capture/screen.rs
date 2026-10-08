//! Screen capture: a video window (matched by title) or a manual screen region → JPEG.
//!
//! Screenshots are downscaled to `max_width` and JPEG-encoded so an Anki card carries a
//! small, self-contained image. Window capture uses `xcap`, which on X11 reads the window's
//! pixels directly; when the target window cannot be found we fall back to the configured
//! manual region so a mining session never hard-fails.
//!
//! The caller is responsible for hiding the overlay for the capture frame when it would
//! otherwise sit over the target (see AGENTS.md); this module only reads pixels.

use image::codecs::jpeg::JpegEncoder;
use image::{ExtendedColorType, RgbaImage};
use xcap::{Monitor, Window};

use crate::config::CaptureConfig;
use crate::error::CaptureError;

/// A capturable top-level window, as offered by the window picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: u32,
    pub title: String,
    pub app_name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl WindowInfo {
    /// Human label for a picker: `"mpv — video.mp4"`.
    pub fn label(&self) -> String {
        if self.app_name.trim().is_empty() {
            self.title.clone()
        } else {
            format!("{} — {}", self.app_name, self.title)
        }
    }
}

/// List capturable windows that have a non-empty title.
pub fn list_windows() -> Result<Vec<WindowInfo>, CaptureError> {
    let windows = Window::all().map_err(|err| CaptureError::Screen(err.to_string()))?;
    let mut out = Vec::new();
    for window in windows {
        let title = window.title().unwrap_or_default();
        if title.trim().is_empty() {
            continue;
        }
        out.push(WindowInfo {
            id: window.id().unwrap_or(0),
            title,
            app_name: window.app_name().unwrap_or_default(),
            x: window.x().unwrap_or(0),
            y: window.y().unwrap_or(0),
            width: window.width().unwrap_or(0),
            height: window.height().unwrap_or(0),
        });
    }
    Ok(out)
}

/// Capture the first window whose title contains `title_contains` (case-insensitive).
pub fn capture_window_jpeg(
    title_contains: &str,
    max_width: u32,
    quality: u8,
) -> Result<Vec<u8>, CaptureError> {
    let needle = title_contains.to_ascii_lowercase();
    let windows = Window::all().map_err(|err| CaptureError::Screen(err.to_string()))?;
    let window = windows.into_iter().find(|window| {
        window
            .title()
            .map(|title| title.to_ascii_lowercase().contains(&needle))
            .unwrap_or(false)
    });
    match window {
        Some(window) => {
            let image = window
                .capture_image()
                .map_err(|err| CaptureError::Screen(err.to_string()))?;
            encode_jpeg(&image, max_width, quality)
        }
        None => Err(CaptureError::WindowGone(title_contains.to_owned())),
    }
}

/// Capture `rect` (global screen coordinates `[x, y, w, h]`) and encode it as JPEG.
///
/// The rectangle is clamped to the monitor containing its top-left corner.
pub fn capture_region_jpeg(
    rect: [f32; 4],
    max_width: u32,
    quality: u8,
) -> Result<Vec<u8>, CaptureError> {
    let x = rect[0].max(0.0) as i32;
    let y = rect[1].max(0.0) as i32;
    let width = rect[2].max(1.0) as u32;
    let height = rect[3].max(1.0) as u32;

    let monitor = Monitor::from_point(x, y).map_err(|err| CaptureError::Screen(err.to_string()))?;
    let monitor_x = monitor
        .x()
        .map_err(|err| CaptureError::Screen(err.to_string()))?;
    let monitor_y = monitor
        .y()
        .map_err(|err| CaptureError::Screen(err.to_string()))?;
    let monitor_w = monitor
        .width()
        .map_err(|err| CaptureError::Screen(err.to_string()))?;
    let monitor_h = monitor
        .height()
        .map_err(|err| CaptureError::Screen(err.to_string()))?;

    let rel_x = (x - monitor_x).clamp(0, monitor_w.saturating_sub(1) as i32) as u32;
    let rel_y = (y - monitor_y).clamp(0, monitor_h.saturating_sub(1) as i32) as u32;
    let clipped_w = width.min(monitor_w.saturating_sub(rel_x)).max(1);
    let clipped_h = height.min(monitor_h.saturating_sub(rel_y)).max(1);

    let image = monitor
        .capture_region(rel_x, rel_y, clipped_w, clipped_h)
        .map_err(|err| CaptureError::Screen(err.to_string()))?;
    encode_jpeg(&image, max_width, quality)
}

/// Capture according to the configuration: the named window if set, otherwise the manual
/// region. If the named window is gone the manual region is used as a fallback.
pub fn capture_jpeg(config: &CaptureConfig) -> Result<Vec<u8>, CaptureError> {
    let target = config.target_window_title.trim();
    if !target.is_empty() {
        match capture_window_jpeg(target, config.max_width, config.jpeg_quality) {
            Ok(bytes) => return Ok(bytes),
            Err(CaptureError::WindowGone(title)) => {
                tracing::warn!(title = %title, "target window not found; using manual region");
            }
            Err(err) => return Err(err),
        }
    }
    if config.manual_rect[2] <= 0.0 || config.manual_rect[3] <= 0.0 {
        return Err(CaptureError::NoTarget);
    }
    capture_region_jpeg(config.manual_rect, config.max_width, config.jpeg_quality)
}

/// Downscale `rgba` to at most `max_width` (preserving aspect) and encode it as JPEG.
pub fn encode_jpeg(rgba: &RgbaImage, max_width: u32, quality: u8) -> Result<Vec<u8>, CaptureError> {
    // JPEG has no alpha channel; drop it before encoding.
    let rgb = image::DynamicImage::ImageRgba8(rgba.clone()).to_rgb8();
    let rgb = if max_width > 0 && rgb.width() > max_width {
        let scale = max_width as f32 / rgb.width() as f32;
        let height = ((rgb.height() as f32 * scale).round() as u32).max(1);
        image::imageops::resize(
            &rgb,
            max_width,
            height,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        rgb
    };

    let mut out = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100));
    encoder
        .encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|err| CaptureError::Encode(err.to_string()))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba};

    fn gradient(width: u32, height: u32) -> RgbaImage {
        RgbaImage::from_fn(width, height, |x, y| {
            Rgba([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, 255])
        })
    }

    #[test]
    fn encoded_output_is_valid_jpeg() {
        let jpeg = encode_jpeg(&gradient(64, 48), 0, 85).expect("encode");
        // JPEG SOI marker.
        assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF]);
        let decoded = image::load_from_memory_with_format(&jpeg, ImageFormat::Jpeg)
            .expect("decode our own jpeg");
        assert_eq!((decoded.width(), decoded.height()), (64, 48));
    }

    #[test]
    fn downscales_to_max_width_preserving_aspect() {
        let jpeg = encode_jpeg(&gradient(100, 50), 50, 80).expect("encode");
        let decoded =
            image::load_from_memory_with_format(&jpeg, ImageFormat::Jpeg).expect("decode");
        assert_eq!((decoded.width(), decoded.height()), (50, 25));
    }

    #[test]
    fn does_not_upscale_smaller_images() {
        let jpeg = encode_jpeg(&gradient(20, 10), 1280, 80).expect("encode");
        let decoded =
            image::load_from_memory_with_format(&jpeg, ImageFormat::Jpeg).expect("decode");
        assert_eq!((decoded.width(), decoded.height()), (20, 10));
    }

    #[test]
    fn window_label_uses_app_name_when_present() {
        let info = WindowInfo {
            id: 1,
            title: "video.mkv".to_owned(),
            app_name: "mpv".to_owned(),
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        assert_eq!(info.label(), "mpv — video.mkv");
    }
}
