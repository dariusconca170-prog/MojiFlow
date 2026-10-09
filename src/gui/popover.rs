//! The dictionary popover: a small panel anchored to a hovered/selected subtitle token.
//!
//! It shows the resolved entry (term, reading, pitch accent, frequency rank, part of speech,
//! glosses) and, for conjugated surfaces, the de-inflection chain that produced the match
//! (e.g. 読んだ → 読む via `past`). Rendering is painter-based like the rest of the overlay:
//! the root UI is a transparent full-window canvas, so regular egui widgets would draw a
//! background we do not want.

use std::sync::Arc;

use egui::{Color32, FontId, Pos2, Rect, Stroke, StrokeKind, Vec2};

use crate::dict::Resolution;

/// Fixed panel width in logical pixels.
const PANEL_WIDTH: f32 = 340.0;
/// Inner padding.
const PAD: f32 = 12.0;
/// Gap between the anchor token and the panel.
const OFFSET: f32 = 8.0;
/// Glosses beyond this count are summarised.
const MAX_GLOSSES: usize = 8;

/// Small kana and the long-vowel mark: they attach to the preceding mora rather than
/// forming one of their own (`きょう` → きょ + う, `コーヒー` → コー + ヒー).
const MORA_TAIL: [char; 10] = ['ゃ', 'ゅ', 'ょ', 'ぁ', 'ぃ', 'ぅ', 'ぇ', 'ぉ', 'ゎ', 'ー'];

/// Split a kana reading into mora.
pub fn moras(reading: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for ch in reading.chars() {
        if MORA_TAIL.contains(&ch) {
            if let Some(last) = out.last_mut() {
                last.push(ch);
                continue;
            }
        }
        out.push(ch.to_string());
    }
    out
}

/// Render a Kanjium accent pattern as a low/high pitch contour over the reading's mora.
///
/// `N == 0` (heiban) rises after the first mora; `N == 1` falls after the first; otherwise
/// the pitch stays high from the second mora through mora `N` and then drops. `_` marks a
/// low mora and `￣` a high one, so 猫 (accent 1) reads `￣_`.
pub fn pitch_line(reading: &str, pattern: u8) -> String {
    let count = moras(reading).len();
    let n = usize::from(pattern);
    let mut line = String::with_capacity(count * 3);
    for i in 1..=count {
        let high = if n == 0 {
            i >= 2
        } else if n == 1 {
            i == 1
        } else {
            i >= 2 && i <= n
        };
        line.push(if high { '￣' } else { '_' });
    }
    line
}

/// The first accent number in a Kanjium pattern string (`"0"`, `"1"`, `"1,3"`).
fn first_pattern(raw: &str) -> Option<u8> {
    raw.split([',', ' ', '、'])
        .find_map(|part| part.trim().parse::<u8>().ok())
}

/// Everything the popover draws, derived once per selected token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopoverData {
    /// The token surface as it appears in the subtitle.
    pub surface: String,
    /// The matched dictionary term (equals `surface` for already-dictionary forms).
    pub term: String,
    pub reading: String,
    pub pos: Vec<String>,
    pub glosses: Vec<String>,
    /// Rendered pitch contour with the numeric accent, e.g. `￣_ [1]`.
    pub pitch: Option<String>,
    pub frequency_rank: Option<i64>,
    /// `term ← surface (reason, …)` when a de-inflection was needed.
    pub chain: Option<String>,
    /// Shown instead of the fields above when nothing resolved.
    pub note: Option<String>,
}

impl PopoverData {
    /// Build the display model from a surface and its (optional) resolution.
    pub fn from_resolution(surface: &str, resolution: Option<&Resolution>) -> Self {
        let Some(resolution) = resolution else {
            return Self::unresolved(surface);
        };
        let Some(entry) = resolution.entries.first() else {
            return Self::unresolved(surface);
        };

        let pitch = entry
            .pitch
            .as_deref()
            .and_then(first_pattern)
            .map(|pattern| format!("{} [{pattern}]", pitch_line(&entry.reading, pattern)));
        let chain = (!resolution.candidate.reasons.is_empty()).then(|| {
            format!(
                "{} ← {} ({})",
                entry.term,
                surface,
                resolution.candidate.reasons.join(", ")
            )
        });

        Self {
            surface: surface.to_owned(),
            term: entry.term.clone(),
            reading: entry.reading.clone(),
            pos: entry.pos.clone(),
            glosses: entry.glosses.clone(),
            pitch,
            frequency_rank: entry.frequency_rank,
            chain,
            note: None,
        }
    }

    fn unresolved(surface: &str) -> Self {
        Self {
            surface: surface.to_owned(),
            term: surface.to_owned(),
            reading: String::new(),
            pos: Vec::new(),
            glosses: Vec::new(),
            pitch: None,
            frequency_rank: None,
            chain: None,
            note: Some("No dictionary entry".to_owned()),
        }
    }
}

struct Line {
    text: String,
    font: FontId,
    color: Color32,
    /// Vertical space added after this line.
    space: f32,
}

impl Line {
    fn new(text: String, font: FontId, color: Color32, space: f32) -> Self {
        Self {
            text,
            font,
            color,
            space,
        }
    }
}

fn build_lines(data: &PopoverData) -> Vec<Line> {
    let accent = Color32::from_rgb(120, 200, 255);
    let muted = Color32::from_gray(180);
    let mut lines = vec![Line::new(
        data.term.clone(),
        FontId::proportional(22.0),
        Color32::WHITE,
        2.0,
    )];

    let mut sub = data.reading.clone();
    if let Some(pitch) = &data.pitch {
        if !sub.is_empty() {
            sub.push_str("   ");
        }
        sub.push_str(pitch);
    }
    if !sub.is_empty() {
        lines.push(Line::new(sub, FontId::proportional(14.0), accent, 6.0));
    }
    if !data.pos.is_empty() {
        lines.push(Line::new(
            data.pos.join(" · "),
            FontId::proportional(13.0),
            muted,
            4.0,
        ));
    }
    if let Some(rank) = data.frequency_rank {
        lines.push(Line::new(
            format!("frequency rank #{rank}"),
            FontId::proportional(13.0),
            muted,
            4.0,
        ));
    }
    for (index, gloss) in data.glosses.iter().take(MAX_GLOSSES).enumerate() {
        lines.push(Line::new(
            format!("{}. {gloss}", index + 1),
            FontId::proportional(15.0),
            Color32::from_gray(235),
            2.0,
        ));
    }
    if data.glosses.len() > MAX_GLOSSES {
        lines.push(Line::new(
            format!("… {} more", data.glosses.len() - MAX_GLOSSES),
            FontId::proportional(13.0),
            muted,
            2.0,
        ));
    }
    if let Some(chain) = &data.chain {
        lines.push(Line::new(
            chain.clone(),
            FontId::proportional(13.0),
            accent,
            4.0,
        ));
    }
    if let Some(note) = &data.note {
        lines.push(Line::new(
            note.clone(),
            FontId::proportional(14.0),
            Color32::from_gray(200),
            2.0,
        ));
    }
    lines
}

/// Where to place a `width × height` panel relative to `anchor`, kept inside `area`.
fn place(area: Rect, anchor: Rect, width: f32, height: f32) -> (f32, f32) {
    let margin = 6.0;
    let mut x = anchor.left();
    if x + width > area.right() - margin {
        x = area.right() - width - margin;
    }
    x = x.max(area.left() + margin);

    let below = anchor.bottom() + OFFSET;
    let y = if below + height <= area.bottom() - margin {
        below
    } else {
        (anchor.top() - OFFSET - height).max(area.top() + margin)
    };
    (x, y)
}

/// Paint the popover anchored to `anchor` and return the rectangle it occupies (used to
/// mark it interactive for hit-testing).
pub fn paint(ui: &mut egui::Ui, data: &PopoverData, anchor: Rect, area: Rect) -> Rect {
    let wrap = PANEL_WIDTH - PAD * 2.0;
    let lines = build_lines(data);

    let mut galleys: Vec<(Arc<egui::Galley>, f32)> = Vec::with_capacity(lines.len());
    let mut content_height = 0.0;
    for line in &lines {
        let galley = ui.fonts_mut(|fonts| {
            fonts.layout(line.text.clone(), line.font.clone(), line.color, wrap)
        });
        content_height += galley.rect.height() + line.space;
        galleys.push((galley, line.space));
    }

    let height = content_height + PAD * 2.0;
    let (x, y) = place(area, anchor, PANEL_WIDTH, height);
    let rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(PANEL_WIDTH, height));

    let painter = ui.painter();
    painter.rect_filled(rect, 8.0, Color32::from_rgba_unmultiplied(18, 18, 22, 240));
    painter.rect_stroke(
        rect,
        8.0,
        Stroke::new(1.0, Color32::from_gray(90)),
        StrokeKind::Inside,
    );

    let mut cursor_y = y + PAD;
    for (galley, space) in &galleys {
        painter.galley(
            Pos2::new(x + PAD, cursor_y),
            Arc::clone(galley),
            Color32::WHITE,
        );
        cursor_y += galley.rect.height() + space;
    }
    rect
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deinflect::Candidate;
    use crate::dict::DictEntry;

    #[test]
    fn mora_splitting_attaches_small_kana_and_long_vowels() {
        assert_eq!(moras("ねこ"), ["ね", "こ"]);
        assert_eq!(moras("きょう"), ["きょ", "う"]);
        assert_eq!(moras("コーヒー"), ["コー", "ヒー"]);
        assert_eq!(moras("がっこう"), ["が", "っ", "こ", "う"]);
        assert_eq!(moras(""), Vec::<String>::new());
    }

    #[test]
    fn pitch_contour_matches_the_accent_rules() {
        assert_eq!(pitch_line("ねこ", 1), "￣_"); // H L
        assert_eq!(pitch_line("ねこ", 0), "_￣"); // L H (heiban)
        assert_eq!(pitch_line("がくせい", 0), "_￣￣￣");
        assert_eq!(pitch_line("がくせい", 2), "_￣__");
        assert_eq!(pitch_line("あ", 1), "￣");
    }

    #[test]
    fn popover_reports_deinflection_chain() {
        let resolution = Resolution {
            candidate: Candidate {
                term: "読む".to_owned(),
                reasons: vec!["past"],
            },
            entries: vec![DictEntry {
                term: "読む".to_owned(),
                reading: "よむ".to_owned(),
                pos: vec!["v5m".to_owned()],
                glosses: vec!["to read".to_owned()],
                pitch: Some("1".to_owned()),
                frequency_rank: Some(420),
            }],
        };
        let data = PopoverData::from_resolution("読んだ", Some(&resolution));
        assert_eq!(data.term, "読む");
        assert_eq!(data.pitch.as_deref(), Some("￣_ [1]"));
        assert_eq!(data.chain.as_deref(), Some("読む ← 読んだ (past)"));
        assert_eq!(data.frequency_rank, Some(420));
    }

    #[test]
    fn popover_without_a_match_shows_a_note() {
        let data = PopoverData::from_resolution("ぬぬ", None);
        assert!(data.glosses.is_empty());
        assert_eq!(data.note.as_deref(), Some("No dictionary entry"));
    }
}
