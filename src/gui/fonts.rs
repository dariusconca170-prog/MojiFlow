//! CJK font loading for the overlay.
//!
//! egui ships no CJK glyphs, so we must inject a Japanese font before the first frame:
//! 1. probe well-known system font paths (Noto Sans CJK JP, Yu Gothic, Meiryo, IPAGothic),
//! 2. fall back to the embedded OFL font in `assets/fonts/` (a copy of Noto Sans CJK JP).
//!
//! TrueType Collections (`.ttc`) are supported through [`FontData::index`]; the correct
//! Japanese face is found by scanning the collection's `name` table (no guessing).

use std::path::PathBuf;

use egui::{FontDefinitions, FontFamily};

use crate::error::FontError;

/// Where the CJK font bytes actually came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FontSource {
    System(PathBuf),
    Embedded,
}

impl std::fmt::Display for FontSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FontSource::System(path) => write!(f, "system: {}", path.display()),
            FontSource::Embedded => write!(f, "embedded assets/fonts/NotoSansCJK-Regular.ttc"),
        }
    }
}

/// Embedded OFL fallback: Noto Sans CJK JP (SIL OFL 1.1, see THIRD_PARTY.md).
const EMBEDDED_FONT: &[u8] = include_bytes!("../../assets/fonts/NotoSansCJK-Regular.ttc");

/// Candidate system font paths, probed in order.
fn system_font_candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    #[cfg(target_os = "linux")]
    {
        paths.extend(
            [
                "/usr/share/fonts/opentype/noto/NotoSansCJKjp-Regular.ttc",
                "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/noto-cjk/NotoSansCJKjp-Regular.ttc",
                "/usr/share/fonts/truetype/noto/NotoSansCJKjp-Regular.ttf",
                "/usr/share/fonts/opentype/ipafont/IPAGothic.ttf",
                "/usr/share/fonts/truetype/ipafont-gothic/u00300-gothic.ttf",
                "/usr/share/fonts/ipafont-gothic/ipagp.ttf",
                "/usr/local/share/fonts/NotoSansCJKjp-Regular.ttc",
            ]
            .iter()
            .map(PathBuf::from),
        );
        if let Ok(home) = std::env::var("HOME") {
            paths.push(PathBuf::from(format!(
                "{home}/.local/share/fonts/NotoSansCJKjp-Regular.ttc"
            )));
            paths.push(PathBuf::from(format!(
                "{home}/.fonts/NotoSansCJKjp-Regular.ttc"
            )));
        }
    }
    #[cfg(target_os = "windows")]
    {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".to_owned());
        for name in [
            "NotoSansCJKjp-Regular.otf",
            "YuGothR.ttc",
            "meiryo.ttc",
            "msgothic.ttc",
            "msgothic.ttc",
        ] {
            paths.push(PathBuf::from(format!(r"{windir}\fonts\{name}")));
        }
    }
    paths
}

/// Install a Japanese font into the egui context. Called once at startup, before the
/// first frame. Returns which source was used so the About/status UI can display it.
pub fn install_cjk_fonts(ctx: &egui::Context) -> Result<FontSource, FontError> {
    let candidates = system_font_candidates();
    let mut probed = 0usize;

    for path in &candidates {
        probed += 1;
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(FontError::Read {
                    path: path.clone(),
                    source,
                })
            }
        };
        match face_index_for_japanese(&bytes) {
            Some(index) => {
                let source = FontSource::System(path.clone());
                apply_font(ctx, bytes.into(), index);
                return Ok(source);
            }
            None => continue,
        }
    }

    // System probe failed: embedded fallback (OFL-licensed Noto Sans CJK JP).
    probed += 1;
    let index = face_index_for_japanese(EMBEDDED_FONT)
        .ok_or(FontError::NoCjkFont { probed })?;
    apply_font(ctx, std::borrow::Cow::Borrowed(EMBEDDED_FONT), index);
    Ok(FontSource::Embedded)
}

/// Merge the CJK face into the existing default font definitions, appended to both the
/// proportional and monospace families so latin keeps egui's defaults and any glyph the
/// default font lacks falls back to the CJK face.
fn apply_font(ctx: &egui::Context, font: std::borrow::Cow<'static, [u8]>, index: u32) {
    let mut definitions = FontDefinitions::default();
    definitions.font_data.insert(
        "cjk-jp".to_owned(),
        std::sync::Arc::new(egui::FontData { font, index, ..Default::default() }),
    );
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        if let Some(list) = definitions.families.get_mut(&family) {
            if !list.iter().any(|name| name == "cjk-jp") {
                list.push("cjk-jp".to_owned());
            }
        }
    }
    ctx.set_fonts(definitions);
}

/// Find the face index of the Japanese face inside a TTF/OTF/TTC file.
/// Plain fonts return `0`. Collections are scanned via the `name` table.
pub fn face_index_for_japanese(bytes: &[u8]) -> Option<u32> {
    if bytes.len() < 12 {
        return None;
    }
    if &bytes[0..4] != b"ttcf" {
        return valid_sfnt(bytes).then_some(0);
    }
    let num_fonts = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    let mut jp_index = None;
    let mut first_valid = None;
    for i in 0..num_fonts {
        let offset_pos = 12 + i * 4;
        if offset_pos + 4 > bytes.len() {
            break;
        }
        let offset = u32::from_be_bytes([
            bytes[offset_pos],
            bytes[offset_pos + 1],
            bytes[offset_pos + 2],
            bytes[offset_pos + 3],
        ]) as usize;
        let Some(names) = face_names(bytes, offset) else {
            continue;
        };
        if first_valid.is_none() {
            first_valid = Some(i as u32);
        }
        if names.iter().any(|n| n.contains("JP")) {
            jp_index = Some(i as u32);
            break;
        }
    }
    jp_index.or(first_valid)
}

fn valid_sfnt(bytes: &[u8]) -> bool {
    // Any of the four sfnt version tags: TrueType (`\0\x01\0\0`, `true`) or CFF (`OTTO`).
    matches!(&bytes[0..4], [0, 1, 0, 0] | b"true" | b"OTTO")
}

/// Read the `name` table strings (IDs 1/4/6) of the face at `offset`.
fn face_names(bytes: &[u8], offset: usize) -> Option<Vec<String>> {
    if offset + 12 > bytes.len() {
        return None;
    }
    let num_tables = u16::from_be_bytes([bytes[offset + 4], bytes[offset + 5]]) as usize;
    let mut name_offset = None;
    let mut name_len = 0usize;
    for t in 0..num_tables {
        let rec = offset + 12 + t * 16;
        if rec + 16 > bytes.len() {
            return None;
        }
        if &bytes[rec..rec + 4] == b"name" {
            name_offset = Some(u32::from_be_bytes([
                bytes[rec + 8],
                bytes[rec + 9],
                bytes[rec + 10],
                bytes[rec + 11],
            ]) as usize);
            name_len = u32::from_be_bytes([
                bytes[rec + 12],
                bytes[rec + 13],
                bytes[rec + 14],
                bytes[rec + 15],
            ]) as usize;
            break;
        }
    }
    let table_at = name_offset?;
    if table_at + name_len > bytes.len() || table_at + 6 > bytes.len() {
        return None;
    }
    let table = &bytes[table_at..table_at + name_len];
    let count = u16::from_be_bytes([table[2], table[3]]) as usize;
    let string_offset = u16::from_be_bytes([table[4], table[5]]) as usize;
    let mut out = Vec::new();
    for r in 0..count {
        let rec = 6 + r * 12;
        if rec + 12 > table.len() {
            break;
        }
        let platform = u16::from_be_bytes([table[rec], table[rec + 1]]);
        let name_id = u16::from_be_bytes([table[rec + 6], table[rec + 7]]);
        if !matches!(name_id, 1 | 4 | 6) {
            continue;
        }
        let length = u16::from_be_bytes([table[rec + 8], table[rec + 9]]) as usize;
        let offset = u16::from_be_bytes([table[rec + 10], table[rec + 11]]) as usize;
        let start = string_offset + offset;
        let end = start.checked_add(length)?;
        if end > table.len() {
            continue;
        }
        let raw = &table[start..end];
        let text = match platform {
            // UTF-16BE (Windows/Unicode platforms)
            0 | 3 => {
                let units: Vec<u16> = raw
                    .chunks_exact(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect();
                String::from_utf16_lossy(&units)
            }
            // Macintosh Roman, effectively ASCII for family names
            1 => raw.iter().map(|&b| b as char).collect(),
            _ => continue,
        };
        out.push(text);
    }
    Some(out)
}

/// The probe string that must render (used by tests and the M1 gate).
pub const DEMO_TEXT: &str = "日本語の文をマイニングしよう。漢字・かな・々・ー・〜 漢字仮名交じり文";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_font_has_japanese_face() {
        let index = face_index_for_japanese(EMBEDDED_FONT)
            .expect("embedded font must contain a Japanese face");
        let names = face_names(
            EMBEDDED_FONT,
            read_ttc_offset(EMBEDDED_FONT, index as usize).expect("ttc offset"),
        )
        .expect("name table");
        assert!(
            names.iter().any(|n| n.contains("JP")),
            "no JP face in {names:?}"
        );
    }

    fn read_ttc_offset(bytes: &[u8], i: usize) -> Option<usize> {
        let pos = 12 + i * 4;
        Some(u32::from_be_bytes([
            bytes[pos],
            bytes[pos + 1],
            bytes[pos + 2],
            bytes[pos + 3],
        ]) as usize)
    }

    /// Glyph coverage for every script character in `DEMO_TEXT` (kanji, kana, 々ー〜).
    #[test]
    fn embedded_font_covers_all_demo_codepoints() {
        use ttf_parser::Face;
        let index = face_index_for_japanese(EMBEDDED_FONT).expect("face index");
        let face = Face::parse(EMBEDDED_FONT, index).expect("parse embedded font");
        let cmap = face.tables().cmap.expect("cmap table");
        let mut missing = Vec::new();
        for ch in DEMO_TEXT.chars().filter(|c| !c.is_whitespace()) {
            if cmap.glyph_index(ch).is_none() {
                missing.push(ch);
            }
        }
        assert!(missing.is_empty(), "missing glyphs: {missing:?}");
    }

    /// Real render test: install the fonts into an egui context, run a frame, and lay the
    /// demo text out. The galley must contain glyphs and a non-zero width (i.e. the text
    /// was shaped with real metrics, not silently dropped).
    #[test]
    fn egui_layouts_demo_text_with_cjk_font() {
        let ctx = egui::Context::default();
        let source = install_cjk_fonts(&ctx).expect("font install");
        assert_eq!(source, FontSource::Embedded);

        let output = ctx.run(egui::RawInput::default(), |ctx| {
            ctx.fonts_mut(|fonts| {
                let galley = fonts.layout_no_wrap(
                    DEMO_TEXT.to_owned(),
                    egui::FontId::proportional(32.0),
                    egui::Color32::WHITE,
                );
                assert!(!galley.rows.is_empty(), "no rows laid out");
                assert!(galley.rect.width() > 100.0, "width {}", galley.rect.width());
                assert!(galley.rect.height() >= 32.0, "height {}", galley.rect.height());
            });
        });
        assert!(!output.shapes.is_empty(), "no shapes produced");
    }
}
