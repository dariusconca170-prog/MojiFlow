//! Loopback audio capture, the "last-heard audio" ring buffer, and MP3 export.
//!
//! The capture callback runs on a real-time audio thread, so the shared buffer uses
//! lock-free atomics rather than a mutex (no allocation or blocking in the callback). See
//! [`ring::AudioRing`] for the SPSC design and [`capture`] for the cpal glue.

pub mod capture;
pub mod encode;
pub mod ring;
