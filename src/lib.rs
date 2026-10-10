//! MediaLingual-Native library crate.
//!
//! `main.rs` is a thin binary wrapper (logging, config load, eframe launch); everything
//! else lives here so integration tests in `tests/` can link against it.

pub mod anki;
pub mod app;
pub mod audio;
pub mod capture;
pub mod clock;
pub mod config;
pub mod deinflect;
pub mod dict;
pub mod error;
pub mod explain;
pub mod export;
pub mod gui;
pub mod hotkey;
pub mod platform;
pub mod stt;
pub mod subs;
pub mod tokenize;
