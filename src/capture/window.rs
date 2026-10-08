//! Global pointer and modifier queries used for overlay hit-testing.
//!
//! A click-through window (see [`crate::app::App`]) receives no mouse events at all, so the
//! overlay cannot learn where the cursor is from egui's input. Instead, every frame it asks
//! the windowing system:
//!
//! - where the pointer is, **relative to our window**, and
//! - whether Shift is currently held (the "Shift-lock": force the overlay interactive so it
//!   can be moved/clicked anywhere without relying on the current cursor position).
//!
//! On Linux/X11 this is a pair of cheap round trips on a second [`x11rb`] connection
//! (`QueryPointer` / `QueryKeymap` + one cached `GetModifierMapping`). On native Wayland the
//! protocol offers no global cursor query, so [`GlobalPointer::cursor`] returns a typed
//! [`WindowError::CursorQueryUnavailable`]; the overlay then degrades to *always interactive*
//! (documented in `AGENTS.md`) rather than silently doing nothing.

use egui::Pos2;
use raw_window_handle::RawWindowHandle;

use crate::error::WindowError;

/// Query handle for the OS pointer/modifier state. Cheap to keep alive for the whole app.
///
/// On unsupported platforms it is an empty handle whose queries return a typed error; the
/// caller surfaces that once in the UI and falls back to a non-click-through overlay.
pub struct GlobalPointer {
    #[cfg(target_os = "linux")]
    conn: Option<x11rb::rust_connection::RustConnection>,
    /// Cached keycodes that map to the Shift modifier (X11 modifier index 0).
    #[cfg(target_os = "linux")]
    shift_keycodes: Vec<u8>,
}

impl GlobalPointer {
    /// Connect (on X11) to the display. Never fails: a connection error is stored and
    /// surfaced on the first [`Self::cursor`] call so startup is not aborted by an
    /// unavailable display.
    pub fn new() -> Self {
        platform_new()
    }

    /// Pointer position in window-local egui points (origin = window top-left). Works even
    /// when the pointer is outside the window (coordinates may be negative).
    pub fn cursor(
        &mut self,
        window: RawWindowHandle,
        pixels_per_point: f32,
    ) -> Result<Pos2, WindowError> {
        platform_cursor(self, window, pixels_per_point)
    }

    /// Whether either Shift key is currently held, queried globally.
    pub fn shift_held(&mut self) -> Result<bool, WindowError> {
        platform_shift_held(self)
    }
}

impl Default for GlobalPointer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------------------
// X11 implementation
// ---------------------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn platform_new() -> GlobalPointer {
    match x11rb::connect(None) {
        Ok((conn, _screen)) => GlobalPointer {
            conn: Some(conn),
            shift_keycodes: Vec::new(),
        },
        Err(err) => {
            tracing::warn!(error = %err, "X11 connection for pointer queries failed");
            GlobalPointer {
                conn: None,
                shift_keycodes: Vec::new(),
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn platform_cursor(
    pointer: &mut GlobalPointer,
    window: RawWindowHandle,
    pixels_per_point: f32,
) -> Result<Pos2, WindowError> {
    use x11rb::protocol::xproto::ConnectionExt as _;

    let conn = pointer
        .conn
        .as_ref()
        .ok_or(WindowError::CursorQueryUnavailable("no X11 connection"))?;
    let window_id = x11_window_id(window)?;
    let reply = conn
        .query_pointer(window_id)
        .map_err(|err| WindowError::X11(err.to_string()))?
        .reply()
        .map_err(|err| WindowError::X11(err.to_string()))?;
    let scale = if pixels_per_point > 0.0 {
        pixels_per_point
    } else {
        1.0
    };
    Ok(Pos2::new(
        f32::from(reply.win_x) / scale,
        f32::from(reply.win_y) / scale,
    ))
}

#[cfg(target_os = "linux")]
fn platform_shift_held(pointer: &mut GlobalPointer) -> Result<bool, WindowError> {
    use x11rb::protocol::xproto::ConnectionExt as _;

    let conn = pointer
        .conn
        .as_ref()
        .ok_or(WindowError::CursorQueryUnavailable("no X11 connection"))?;

    if pointer.shift_keycodes.is_empty() {
        let mapping = conn
            .get_modifier_mapping()
            .map_err(|err| WindowError::X11(err.to_string()))?
            .reply()
            .map_err(|err| WindowError::X11(err.to_string()))?;
        // keycodes is 8 modifier rows concatenated; index 0 is ShiftMask.
        let per_modifier = (mapping.keycodes.len() / 8).max(1);
        pointer.shift_keycodes = mapping.keycodes[..per_modifier]
            .iter()
            .copied()
            .filter(|kc| *kc != 0)
            .collect();
    }

    let keys = conn
        .query_keymap()
        .map_err(|err| WindowError::X11(err.to_string()))?
        .reply()
        .map_err(|err| WindowError::X11(err.to_string()))?
        .keys;
    Ok(pointer
        .shift_keycodes
        .iter()
        .any(|&kc| key_bitmap_contains(&keys, kc)))
}

/// True if the X11 keycode is set in a `QueryKeymap` bitmap.
#[cfg(target_os = "linux")]
fn key_bitmap_contains(keys: &[u8; 32], keycode: u8) -> bool {
    let byte = usize::from(keycode) / 8;
    let bit = usize::from(keycode) % 8;
    keys.get(byte).is_some_and(|byte| byte & (1 << bit) != 0)
}

#[cfg(target_os = "linux")]
fn x11_window_id(window: RawWindowHandle) -> Result<u32, WindowError> {
    match window {
        RawWindowHandle::Xlib(handle) => Ok(handle.window as u32),
        RawWindowHandle::Xcb(handle) => Ok(handle.window.get()),
        _ => Err(WindowError::CursorQueryUnavailable(
            "no X11 window handle (native Wayland)",
        )),
    }
}

// ---------------------------------------------------------------------------------------
// Fallback for platforms without an implementation yet (Wayland, macOS, Windows-in-M4)
// ---------------------------------------------------------------------------------------

#[cfg(not(target_os = "linux"))]
fn platform_new() -> GlobalPointer {
    GlobalPointer {}
}

#[cfg(not(target_os = "linux"))]
fn platform_cursor(
    _pointer: &mut GlobalPointer,
    _window: RawWindowHandle,
    _pixels_per_point: f32,
) -> Result<Pos2, WindowError> {
    Err(WindowError::CursorQueryUnavailable(
        "global cursor query is only implemented for X11 in this build",
    ))
}

#[cfg(not(target_os = "linux"))]
fn platform_shift_held(_pointer: &mut GlobalPointer) -> Result<bool, WindowError> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_bits_are_read_little_endian_per_byte() {
        let mut keys = [0u8; 32];
        keys[0] = 0b0000_0010; // keycode 1 pressed
        keys[1] = 0b0000_0001; // keycode 8 pressed
        assert!(key_bitmap_contains(&keys, 1));
        assert!(!key_bitmap_contains(&keys, 0));
        assert!(key_bitmap_contains(&keys, 8));
        assert!(!key_bitmap_contains(&keys, 9));
    }

    #[test]
    fn cursor_reads_beyond_the_bitmap_safely() {
        // keycode 255 would index byte 31 (valid); 256 is impossible for u8 but the guard
        // must not panic for any 8-bit code.
        let keys = [0xFFu8; 32];
        assert!(key_bitmap_contains(&keys, u8::MAX));
    }
}
