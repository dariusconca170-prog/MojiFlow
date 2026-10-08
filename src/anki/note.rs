//! Card data + field-template rendering and the serializable pending note.
//!
//! `config.anki.field_mapping` maps Anki note fields to templates over
//! [`KNOWN_PLACEHOLDERS`](crate::config::KNOWN_PLACEHOLDERS); [`render_field`] expands one
//! template against a [`CardData`]. `{{` renders as a literal `{` and unknown placeholders
//! render empty — the config validator elsewhere rejects templates with unknown names.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::anki::media::MediaFile;

/// Everything a mined line can contribute to a note, keyed by placeholder name.
#[derive(Debug, Clone, Default)]
pub struct CardData {
    pub word: String,
    pub reading: String,
    pub definition: String,
    pub sentence: String,
    pub sentence_furigana: String,
    pub pitch: String,
    pub pitch_svg: String,
    pub frequency: String,
    pub source: String,
    pub audio: Option<MediaFile>,
    pub image: Option<MediaFile>,
}

impl CardData {
    fn value_for(&self, name: &str) -> String {
        match name {
            "word" => self.word.clone(),
            "reading" => self.reading.clone(),
            "definition" => self.definition.clone(),
            "sentence" => self.sentence.clone(),
            "sentence_furigana" => self.sentence_furigana.clone(),
            "pitch" => self.pitch.clone(),
            "pitch_svg" => self.pitch_svg.clone(),
            "frequency" => self.frequency.clone(),
            "source" => self.source.clone(),
            "audio" => self
                .audio
                .as_ref()
                .map(MediaFile::sound_ref)
                .unwrap_or_default(),
            "image" => self
                .image
                .as_ref()
                .map(MediaFile::image_ref)
                .unwrap_or_default(),
            _ => String::new(),
        }
    }
}

/// Expand `template` against `data`: `{name}` → value, `{{` → literal `{`, `}}` → literal `}`,
/// an unmatched or unknown `{name}` renders empty (validated upstream).
pub fn render_field(template: &str, data: &CardData) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        push_literal(&mut out, &rest[..start]);
        let after = &rest[start + 1..];
        if let Some(after_literal) = after.strip_prefix('{') {
            out.push('{');
            rest = after_literal;
            continue;
        }
        match after.find('}') {
            None => {
                push_literal(&mut out, &rest[start..]);
                rest = after;
            }
            Some(end) => {
                out.push_str(&data.value_for(&after[..end]));
                rest = &after[end + 1..];
            }
        }
    }
    push_literal(&mut out, rest);
    out
}

/// Append literal text, collapsing a doubled `}}` to a single `}`.
fn push_literal(out: &mut String, mut text: &str) {
    while let Some(end) = text.find("}}") {
        out.push_str(&text[..end]);
        out.push('}');
        text = &text[end + 2..];
    }
    out.push_str(text);
}

/// A fully rendered note ready for AnkiConnect `addNote`, serializable for the offline queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingNote {
    pub deck: String,
    pub model: String,
    pub tags: Vec<String>,
    /// field name → rendered value (templates already expanded).
    pub fields: BTreeMap<String, String>,
    /// Media to upload before `addNote` (audio/image files).
    pub media: Vec<MediaFile>,
}

impl PendingNote {
    /// Render `field_mapping` (field name → template) into `fields`.
    pub fn from_mapping(
        deck: &str,
        model: &str,
        tags: &[String],
        field_mapping: &BTreeMap<String, String>,
        data: &CardData,
        media: Vec<MediaFile>,
    ) -> Self {
        let fields = field_mapping
            .iter()
            .map(|(field, template)| (field.clone(), render_field(template, data)))
            .collect();
        Self {
            deck: deck.to_owned(),
            model: model.to_owned(),
            tags: tags.to_vec(),
            fields,
            media,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CardData {
        CardData {
            word: "食べる".to_owned(),
            reading: "たべる".to_owned(),
            definition: "to eat".to_owned(),
            sentence: "毎日食べる。".to_owned(),
            sentence_furigana: "毎日<ruby>食べ<rt>たべ</rt></ruby>る。".to_owned(),
            pitch: "[たべる]".to_owned(),
            pitch_svg: "<svg>…</svg>".to_owned(),
            frequency: "12345".to_owned(),
            source: "episode 1 @ 00:12:34".to_owned(),
            audio: Some(MediaFile::from_bytes("c.mp3".to_owned(), vec![9])),
            image: Some(MediaFile::from_bytes("s.jpg".to_owned(), vec![9])),
        }
    }

    #[test]
    fn renders_known_placeholders() {
        let data = sample();
        let rendered = render_field(
            "{word}｜{reading}｜{definition}<br>{sentence_furigana}｜{pitch}｜{frequency}｜{source}",
            &data,
        );
        assert_eq!(
            rendered,
            "食べる｜たべる｜to eat<br>毎日<ruby>食べ<rt>たべ</rt></ruby>る。｜[たべる]｜12345｜episode 1 @ 00:12:34"
        );
    }

    #[test]
    fn renders_media_references() {
        let data = sample();
        assert_eq!(
            render_field("{audio}{image}", &data),
            "[sound:c.mp3]<img src=\"s.jpg\">"
        );
    }

    #[test]
    fn doubled_braces_are_literal_escapes() {
        let data = sample();
        // `{{`/`}}` escape single braces, so `{{word}}` renders the literal text `{word}`.
        assert_eq!(render_field("{{word}}", &data), "{word}");
        assert_eq!(render_field("a }} b", &data), "a } b");
        // ...while a single brace pair is still a placeholder.
        assert_eq!(render_field("{word}", &data), "食べる");
    }

    #[test]
    fn missing_data_renders_empty() {
        let data = CardData::default();
        assert_eq!(render_field("[{word}] [{audio}]", &data), "[] []");
    }

    #[test]
    fn from_mapping_collects_rendered_fields() {
        let mapping = BTreeMap::from([
            ("Expression".to_owned(), "{word}".to_owned()),
            ("Reading".to_owned(), "{reading} ({sentence})".to_owned()),
        ]);
        let note = PendingNote::from_mapping(
            "Default",
            "Japanese",
            &["sentence-mining".to_owned()],
            &mapping,
            &sample(),
            vec![],
        );
        assert_eq!(note.fields["Expression"], "食べる");
        assert_eq!(note.fields["Reading"], "たべる (毎日食べる。)");
        assert_eq!(note.tags, vec!["sentence-mining"]);
    }
}
