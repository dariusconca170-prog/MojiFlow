//! Media payloads for Anki export: base64-encoded bytes plus the `[sound:…]` / `<img>` field
//! references Anki understands once the file is in the collection media folder.

use serde::{Deserialize, Serialize};

use crate::error::AnkiError;

/// Bytes destined for Anki's media folder, carried base64 across the wire and on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaFile {
    pub filename: String,
    /// Base64 (standard alphabet) of the file contents.
    pub data_base64: String,
}

impl MediaFile {
    /// Wrap raw bytes; base64-encodes eagerly so the wire/queue formats stay simple.
    pub fn from_bytes(filename: String, bytes: Vec<u8>) -> Self {
        use base64::Engine as _;
        Self {
            filename,
            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }

    /// Decode back to raw bytes (used by tests and by upload retries).
    pub fn bytes(&self) -> Result<Vec<u8>, AnkiError> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(&self.data_base64)
            .map_err(|err| AnkiError::Malformed(format!("invalid base64 media: {err}")))
    }

    /// The `[sound:…]` marker for audio fields.
    pub fn sound_ref(&self) -> String {
        format!("[sound:{}]", self.filename)
    }

    /// An `<img>` tag for image fields.
    pub fn image_ref(&self) -> String {
        format!("<img src=\"{}\">", self.filename)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        let file = MediaFile::from_bytes("clip.mp3".to_owned(), vec![0, 1, 2, 250, 255]);
        assert_eq!(file.bytes().unwrap(), vec![0, 1, 2, 250, 255]);
    }

    #[test]
    fn field_references_are_anki_shaped() {
        let file = MediaFile::from_bytes("a.mp3".to_owned(), vec![1]);
        assert_eq!(file.sound_ref(), "[sound:a.mp3]");
        let image = MediaFile::from_bytes("s.jpg".to_owned(), vec![1]);
        assert_eq!(image.image_ref(), "<img src=\"s.jpg\">");
    }
}
