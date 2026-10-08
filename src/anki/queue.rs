//! On-disk offline queue for exports that land while Anki is unreachable.
//!
//! Each entry is one [`PendingNote`] serialized as JSON in the queue directory. `push`
//! never fails on Anki state: it only needs a writable directory. `drain` hands entries to a
//! caller-supplied sender (the export worker) and removes each file after the sender
//! reports success, so a crash mid-drain loses nothing.

use std::fs;
use std::path::{Path, PathBuf};

use crate::anki::note::PendingNote;
use crate::error::AnkiError;

/// JSON-file backed queue. Create it with a subdirectory of the config dir, e.g.
/// `~/.config/medialingual/queue`.
pub struct OfflineQueue {
    dir: PathBuf,
}

impl OfflineQueue {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Path of the queue directory (created lazily by [`push`](Self::push)).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Persist `note` as `queue/<nanos>-<nonce>.json`.
    pub fn push(&self, note: &PendingNote) -> Result<(), AnkiError> {
        fs::create_dir_all(&self.dir).map_err(|err| AnkiError::FieldMapping(err.to_string()))?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let nonce: u64 = rand::random();
        let path = self.dir.join(format!("{nanos}-{nonce}.json"));
        let json =
            serde_json::to_vec_pretty(note).map_err(|err| AnkiError::Malformed(err.to_string()))?;
        fs::write(&path, json).map_err(|err| AnkiError::FieldMapping(err.to_string()))?;
        Ok(())
    }

    /// List pending entries oldest-first as `(path, note)`.
    pub fn pending(&self) -> Vec<(PathBuf, PendingNote)> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Ok(bytes) = fs::read(&path) {
                if let Ok(note) = serde_json::from_slice(&bytes) {
                    out.push((path, note));
                }
            }
        }
        out.sort_by(|(a, _), (b, _)| a.file_name().cmp(&b.file_name()));
        out
    }

    /// Number of entries waiting (used by the status strip / retry scheduling).
    pub fn pending_count(&self) -> usize {
        self.pending().len()
    }

    /// Remove a successfully exported entry.
    pub fn remove(&self, path: &Path) -> Result<(), AnkiError> {
        fs::remove_file(path).map_err(|err| AnkiError::FieldMapping(err.to_string()))
    }

    /// Send every pending entry through `send` (must return `Ok(())` on success) and drop
    /// each file afterwards. Returns the number exported.
    pub async fn drain<F, Fut>(&self, mut send: F) -> Result<usize, AnkiError>
    where
        F: FnMut(PendingNote) -> Fut,
        Fut: std::future::Future<Output = Result<(), AnkiError>>,
    {
        let mut exported = 0;
        for (path, note) in self.pending() {
            match send(note).await {
                Ok(()) => {
                    let _ = self.remove(&path);
                    exported += 1;
                }
                Err(AnkiError::Unreachable { .. }) => break, // keep the rest for later
                Err(_) => continue,
            }
        }
        Ok(exported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn note() -> PendingNote {
        PendingNote {
            deck: "Default".to_owned(),
            model: "Japanese".to_owned(),
            tags: vec!["mining".to_owned()],
            fields: BTreeMap::from([("Expression".to_owned(), "食べる".to_owned())]),
            media: vec![],
        }
    }

    fn queue() -> OfflineQueue {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir = dir.keep();
        OfflineQueue::new(dir)
    }

    #[test]
    fn push_then_pending_round_trips() {
        let queue = queue();
        assert_eq!(queue.pending_count(), 0);
        queue.push(&note()).expect("push");
        queue.push(&note()).expect("push");
        assert_eq!(queue.pending_count(), 2);
        let entries = queue.pending();
        assert_eq!(entries[0].1.fields["Expression"], "食べる");
    }

    #[tokio::test]
    async fn drain_calls_sender_for_each_and_removes_files() {
        let queue = queue();
        queue.push(&note()).expect("push");
        queue.push(&note()).expect("push");
        let exported = queue
            .drain(|note| {
                assert_eq!(note.fields["Expression"], "食べる");
                std::future::ready(Ok(()))
            })
            .await
            .expect("drain");
        assert_eq!(exported, 2);
        assert_eq!(queue.pending_count(), 0);
    }

    #[tokio::test]
    async fn drain_stops_on_unreachable_keeping_rest() {
        let queue = queue();
        queue.push(&note()).expect("push");
        queue.push(&note()).expect("push");
        let mut calls = 0;
        let exported = queue
            .drain(|_| {
                calls += 1;
                std::future::ready(Err(AnkiError::Unreachable {
                    url: "http://x".to_owned(),
                    reason: "down".to_owned(),
                }))
            })
            .await
            .expect("drain");
        assert_eq!(exported, 0);
        assert_eq!(calls, 1);
        assert_eq!(queue.pending_count(), 2);
    }
}
