//! Screen/audio capture and OS-level window queries.
//!
//! Modules land as their milestones do: [`window`] (global pointer/keyboard queries) arrives
//! with M4; screenshot capture joins in M5, loopback audio in M5/M6.
//!
//! The overlay window is click-through by default, so it receives no mouse events of its
//! own. To decide when to re-enable hit-testing it asks the *whole OS* where the pointer is
//! — that is what [`window::GlobalPointer`] wraps (X11 `QueryPointer`/`QueryKeymap` on
//! Linux; see that module for the per-platform fallbacks).

pub mod screen;
pub mod window;
