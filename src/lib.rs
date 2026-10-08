//! MediaLingual-Native library crate.
//!
//! `main.rs` is a thin binary wrapper (logging, config load, eframe launch); everything
//! else lives here so integration tests in `tests/` can link against it.

pub mod app;
pub mod clock;
pub mod config;
pub mod error;
pub mod gui;
pub mod subs;
