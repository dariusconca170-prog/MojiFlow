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

    /// Set (or clear) EWMH always-on-top on every top-level window whose
    /// `_NET_WM_NAME` equals `title`.
    ///
    /// eframe 0.36 silently drops `ViewportBuilder::with_window_level`/`with_always_on_top`
    /// (its winit integration has no `WindowLevel` handling at all), so the hint never
    /// becomes `_NET_WM_STATE_ABOVE` and the overlay sits at Z-order-luck. The app calls
    /// this periodically (self-healing); it only sends a ClientMessage when the WM state
    /// actually differs from `enable`, so it is cheap to re-run.
    pub fn set_always_on_top(&self, title: &str, enable: bool) -> Result<(), WindowError> {
        platform_set_always_on_top(self, title, enable)
    }

    /// True when any top-level window (other than our own overlay) is currently
    /// fullscreen — the signal `App` uses for fullscreen-follow (expand the overlay to
    /// fullscreen so subtitles stay visible over the video).
    pub fn is_any_fullscreen(&self) -> Result<bool, WindowError> {
        platform_any_fullscreen(self)
    }

    /// Primary screen size in physical pixels (X11 root geometry). The app divides by
    /// `pixels_per_point` to get logical points for placement math.
    pub fn primary_screen_size(&self) -> Result<(u32, u32), WindowError> {
        platform_screen_size(self)
    }
}
#[cfg(target_os = "linux")]
fn platform_any_fullscreen(pointer: &GlobalPointer) -> Result<bool, WindowError> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::ConnectionExt as _;

    let conn = pointer.conn.as_ref().ok_or(WindowError::Unsupported(
        "no X11 connection for fullscreen query",
    ))?;
    let root = conn.setup().roots[0].root;
    let intern = |name: &[u8]| -> Result<u32, WindowError> {
        Ok(conn
            .intern_atom(false, name)
            .map_err(|err| WindowError::X11(err.to_string()))?
            .reply()
            .map_err(|err| WindowError::X11(err.to_string()))?
            .atom)
    };
    let name_atom = intern(b"_NET_WM_NAME")?;
    let state_atom = intern(b"_NET_WM_STATE")?;
    let full_atom = intern(b"_NET_WM_STATE_FULLSCREEN")?;

    // Walk the tree like `platform_set_always_on_top` (WM frames reparent the client
    // windows). Skip our own overlay by title: once we put it fullscreen it would
    // otherwise match itself forever and never restore.
    let mut queue = vec![root];
    while let Some(w) = queue.pop() {
        let children = conn
            .query_tree(w)
            .map_err(|err| WindowError::X11(err.to_string()))?
            .reply()
            .map_err(|err| WindowError::X11(err.to_string()))?
            .children;
        for child in children {
            let name = conn
                .get_property(false, child, name_atom, 0u32, 0, 4096)
                .map_err(|err| WindowError::X11(err.to_string()))?
                .reply()
                .map_err(|err| WindowError::X11(err.to_string()))?;
            if !name.value.is_empty()
                && String::from_utf8_lossy(&name.value).trim_end_matches('\0')
                    == crate::platform::OVERLAY_TITLE
            {
                continue;
            }
            let state = conn
                .get_property(false, child, state_atom, 0u32, 0, 32)
                .map_err(|err| WindowError::X11(err.to_string()))?
                .reply()
                .map_err(|err| WindowError::X11(err.to_string()))?;
            if state
                .value32()
                .is_some_and(|mut atoms| atoms.any(|atom| atom == full_atom))
            {
                return Ok(true);
            }
            queue.push(child);
        }
    }
    Ok(false)
}

#[cfg(target_os = "windows")]
fn platform_any_fullscreen(_pointer: &GlobalPointer) -> Result<bool, WindowError> {
    Err(WindowError::Unsupported(
        "fullscreen-follow is X11-only in this build",
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn platform_any_fullscreen(_pointer: &GlobalPointer) -> Result<bool, WindowError> {
    Err(WindowError::Unsupported(
        "fullscreen-follow is X11-only in this build",
    ))
}

#[cfg(target_os = "linux")]
fn platform_screen_size(pointer: &GlobalPointer) -> Result<(u32, u32), WindowError> {
    use x11rb::connection::Connection as _;

    let conn = pointer.conn.as_ref().ok_or(WindowError::Unsupported(
        "no X11 connection for screen-size query",
    ))?;
    let screen = &conn.setup().roots[0];
    Ok((
        u32::from(screen.width_in_pixels),
        u32::from(screen.height_in_pixels),
    ))
}

#[cfg(target_os = "windows")]
fn platform_screen_size(_pointer: &GlobalPointer) -> Result<(u32, u32), WindowError> {
    Err(WindowError::Unsupported(
        "screen-size query is X11-only in this build",
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn platform_screen_size(_pointer: &GlobalPointer) -> Result<(u32, u32), WindowError> {
    Err(WindowError::Unsupported(
        "screen-size query is X11-only in this build",
    ))
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

/// EWMH `_NET_WM_STATE` client-message payload (32-bit data array):
/// `[action, property, second_property, source, unused]` — action 1 = add, 0 = remove,
/// source 1 = application. This is the canonical shape KWin/other EWMH WMs act on.
#[cfg(target_os = "linux")]
fn state_change_data(enable: bool, property_atom: u32) -> [u32; 5] {
    [if enable { 1 } else { 0 }, property_atom, 0, 1, 0]
}

#[cfg(target_os = "linux")]
fn platform_set_always_on_top(
    pointer: &GlobalPointer,
    title: &str,
    enable: bool,
) -> Result<(), WindowError> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{
        ClientMessageData, ClientMessageEvent, ConnectionExt as _, EventMask,
    };

    let conn = pointer.conn.as_ref().ok_or(WindowError::Unsupported(
        "no X11 connection for window level",
    ))?;
    let root = conn.setup().roots[0].root;

    let intern = |name: &[u8]| -> Result<u32, WindowError> {
        Ok(conn
            .intern_atom(false, name)
            .map_err(|err| WindowError::X11(err.to_string()))?
            .reply()
            .map_err(|err| WindowError::X11(err.to_string()))?
            .atom)
    };
    let name_atom = intern(b"_NET_WM_NAME")?;
    let state_atom = intern(b"_NET_WM_STATE")?;
    let above_atom = intern(b"_NET_WM_STATE_ABOVE")?;

    // The overlay's client window is reparented under a WM frame (a child of root), so
    // walk the tree from root, reading `_NET_WM_NAME` (type filter 0 = any type) to find
    // our own windows. One walk per check is a few dozen local-X round trips — negligible
    // against the overlay's 30 Hz repaint, and ClientMessages are only sent on mismatch.
    let mut windows = Vec::new();
    let mut queue = vec![root];
    while let Some(w) = queue.pop() {
        let children = conn
            .query_tree(w)
            .map_err(|err| WindowError::X11(err.to_string()))?
            .reply()
            .map_err(|err| WindowError::X11(err.to_string()))?
            .children;
        for child in children {
            let property = conn
                .get_property(false, child, name_atom, 0u32, 0, 4096)
                .map_err(|err| WindowError::X11(err.to_string()))?
                .reply()
                .map_err(|err| WindowError::X11(err.to_string()))?;
            if !property.value.is_empty()
                && String::from_utf8_lossy(&property.value).trim_end_matches('\0') == title
            {
                windows.push(child);
                continue; // our client window: no need to descend further
            }
            queue.push(child);
        }
    }

    let mut changed = false;
    for w in windows {
        let property = conn
            .get_property(false, w, state_atom, 0u32, 0, 32)
            .map_err(|err| WindowError::X11(err.to_string()))?
            .reply()
            .map_err(|err| WindowError::X11(err.to_string()))?;
        let already = property
            .value32()
            .is_some_and(|mut atoms| atoms.any(|atom| atom == above_atom));
        if already == enable {
            continue;
        }
        // `ClientMessageData` in x11rb 0.14 is a raw 20-byte payload; the EWMH data32
        // words are little-endian on the wire (the X11 byte order x11rb uses).
        let words = state_change_data(enable, above_atom);
        let mut bytes = [0u8; 20];
        for (slot, word) in words.iter().enumerate() {
            bytes[slot * 4..slot * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        let event = ClientMessageEvent::new(32, w, state_atom, ClientMessageData::from(bytes));
        conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )
        .map_err(|err| WindowError::X11(err.to_string()))?
        .ignore_error();
        changed = true;
    }
    if changed {
        conn.flush()
            .map_err(|err| WindowError::X11(err.to_string()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Windows implementation (M7): GetCursorPos + GetWindowRect + GetAsyncKeyState
// ---------------------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn platform_new() -> GlobalPointer {
    GlobalPointer {}
}

#[cfg(target_os = "windows")]
fn platform_cursor(
    _pointer: &mut GlobalPointer,
    window: RawWindowHandle,
    pixels_per_point: f32,
) -> Result<Pos2, WindowError> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetWindowRect};

    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: `point` is a writable out-parameter.
    if unsafe { GetCursorPos(&mut point) } == 0 {
        return Err(WindowError::Win32("GetCursorPos failed".to_owned()));
    }
    let hwnd = match window {
        RawWindowHandle::Win32(handle) => handle.hwnd.as_ptr(),
        _ => {
            return Err(WindowError::CursorQueryUnavailable(
                "no Win32 window handle",
            ))
        }
    };
    let mut rect = windows_sys::Win32::Foundation::RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: `rect` is a writable out-parameter, `hwnd` came from the live window.
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        return Err(WindowError::Win32("GetWindowRect failed".to_owned()));
    }
    let scale = if pixels_per_point > 0.0 {
        pixels_per_point
    } else {
        1.0
    };
    Ok(Pos2::new(
        f32::from(point.x - rect.left) / scale,
        f32::from(point.y - rect.top) / scale,
    ))
}

#[cfg(target_os = "windows")]
fn platform_shift_held(_pointer: &mut GlobalPointer) -> Result<bool, WindowError> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    // VK_SHIFT (0x10); bit 15 set = key is down.
    // SAFETY: GetAsyncKeyState takes only an int vKey.
    Ok(unsafe { GetAsyncKeyState(0x10) } & 0x8000 != 0)
}

#[cfg(target_os = "windows")]
fn platform_set_always_on_top(
    _pointer: &GlobalPointer,
    _title: &str,
    _enable: bool,
) -> Result<(), WindowError> {
    // eframe drops the builder hint on Windows too; SetWindowPos(HWND_TOPMOST) is M8
    // Windows work (compile-gated, unverifiable here).
    Err(WindowError::Unsupported(
        "always-on-top is not enforced on Windows yet",
    ))
}

// ---------------------------------------------------------------------------------------
// Fallback for platforms without an implementation yet (Wayland, macOS)
// ---------------------------------------------------------------------------------------

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn platform_new() -> GlobalPointer {
    GlobalPointer {}
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn platform_cursor(
    _pointer: &mut GlobalPointer,
    _window: RawWindowHandle,
    _pixels_per_point: f32,
) -> Result<Pos2, WindowError> {
    Err(WindowError::CursorQueryUnavailable(
        "global cursor query is only implemented for X11 and Windows in this build",
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn platform_shift_held(_pointer: &mut GlobalPointer) -> Result<bool, WindowError> {
    Ok(false)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn platform_set_always_on_top(
    _pointer: &GlobalPointer,
    _title: &str,
    _enable: bool,
) -> Result<(), WindowError> {
    Err(WindowError::Unsupported(
        "always-on-top is only enforced on X11 in this build",
    ))
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

    #[cfg(target_os = "linux")]
    #[test]
    fn net_wm_state_payload_adds_and_removes() {
        // `[action, property, second_property, source=app, unused]` per EWMH.
        assert_eq!(state_change_data(true, 42), [1, 42, 0, 1, 0]);
        assert_eq!(state_change_data(false, 0xBAD), [0, 0xBAD, 0, 1, 0]);
    }
}
