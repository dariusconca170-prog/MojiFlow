//! AnkiConnect client, note/media preparation, and the offline export queue.
//!
//! This is the M6 export half: it renders user field templates (`.field_mapping` in
//! `config.toml`) from a mined [`note::CardData`], uploads audio/image media, and calls
//! AnkiConnect `addNote`. When Anki is unreachable the rendered note lands in an on-disk
//! [`queue::OfflineQueue`] and is retried the next time the user triggers an export.
//!
//! AnkiConnect speaks a jsonrpc-style protocol over plain HTTP (`POST` with
//! `{"action", "version", "params"}`); see [`connect::AnkiConnect`]. All wall-clock work is
//! async (reqwest + tokio), so nothing here ever blocks the UI thread.

pub mod connect;
pub mod media;
pub mod note;
pub mod queue;
