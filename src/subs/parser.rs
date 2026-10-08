//! Subtitle parsers: SRT, WebVTT, ASS/SSA.
//!
//! Design rules (Section 5.1): strip HTML/ASS override tags, convert `\N`, drop ASS
//! drawing commands, merge duplicate stacked cues (see `super::merge_stacked`), tolerate
//! malformed timestamps by skipping the cue with a warning — never panic.

use std::path::Path;
use std::time::Duration;

use crate::subs::{Cue, Format};

/// Decode raw subtitle bytes to text, auto-detecting the encoding.
///
/// Order: BOM (UTF-8/UTF-16) → strict UTF-8 → `chardetng` guess via `encoding_rs`.
/// Returns `(text, encoding_name)`.
pub fn decode_bytes(bytes: &[u8]) -> (String, &'static str) {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        let text = String::from_utf8_lossy(&bytes[3..]).into_owned();
        return (text, "utf-8-bom");
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let (cow, _) = encoding_rs::UTF_16LE.decode_with_bom_removal(bytes);
        return (cow.into_owned(), "utf-16le");
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let (cow, _) = encoding_rs::UTF_16BE.decode_with_bom_removal(bytes);
        return (cow.into_owned(), "utf-16be");
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return (text.to_owned(), "utf-8");
    }
    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(bytes, true);
    let encoding = detector.guess(None, false);
    let (cow, _, had_replacement) = encoding.decode(bytes);
    tracing::warn!(
        encoding = encoding.name(),
        had_replacement,
        "non-UTF-8 subtitle bytes detected"
    );
    let _ = had_replacement;
    (cow.into_owned(), encoding.name())
}

/// Dispatch to a format parser. Returns `(cues, skipped_count)`.
/// Malformed cues are skipped with a `tracing::warn`.
pub fn parse(format: Format, text: &str, path: &Path) -> (Vec<Cue>, usize) {
    match format {
        Format::Srt => parse_srt(text, path),
        Format::WebVtt => parse_vtt(text, path),
        Format::Ass => parse_ass(text, path),
    }
}

/// Parse a flexible timestamp: `H:MM:SS,mmm`, `HH:MM:SS.mmm`, `MM:SS.mmm`, `H:MM:SS.cc`
/// (ASS centiseconds). Fraction length determines the unit (1 digit = tenths,
/// 2 = centiseconds, 3 = milliseconds, more = truncated to milliseconds).
/// Returns `None` for anything malformed.
pub fn parse_timestamp(raw: &str) -> Option<Duration> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let parts: Vec<&str> = raw.split(':').collect();
    if parts.len() > 3 {
        return None;
    }
    let mut secs: u64 = 0;
    // Up to three components: [hours,] minutes, seconds-with-fraction.
    for (i, part) in parts.iter().enumerate() {
        let is_last = i + 1 == parts.len();
        if is_last {
            let (sec_str, frac_str) = match part.find([',', '.']) {
                Some(pos) => (&part[..pos], &part[pos + 1..]),
                None => (*part, ""),
            };
            if sec_str.is_empty() || !sec_str.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let seconds: u64 = sec_str.parse().ok()?;
            secs += seconds;
            if !frac_str.is_empty() {
                if !frac_str.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                let ms: u64 = if frac_str.len() >= 3 {
                    frac_str[..3].parse().ok()?
                } else {
                    let value: u64 = frac_str.parse().ok()?;
                    value * 10u64.pow(3 - frac_str.len() as u32)
                };
                secs = secs.checked_mul(1000)?.checked_add(ms)?;
                return Some(Duration::from_millis(secs));
            }
        } else {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let value: u64 = part.parse().ok()?;
            let multiplier = if parts.len() == 3 && i == 0 { 3600 } else { 60 };
            secs = secs.checked_add(value.checked_mul(multiplier)?)?;
        }
    }
    secs.checked_mul(1000).map(Duration::from_millis)
}

/// Strip HTML-ish tags (`<i>`, `</font>`, `<br>`…), converting `<br>` to a newline.
/// A `<` with no closing `>` within a reasonable span is kept literally so plain text
/// like `a < b` survives.
pub fn strip_html_tags(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let close = (i + 1..bytes.len().min(i + 64)).find(|&j| bytes[j] == b'>');
            match close {
                Some(j) => {
                    let tag = &input[i + 1..j];
                    if tag.eq_ignore_ascii_case("br") {
                        out.push('\n');
                    }
                    i = j + 1;
                }
                None => {
                    out.push('<');
                    i += 1;
                }
            }
        } else {
            // Safe because tags are ASCII and we only advanced over ASCII above;
            // copy the full char to stay UTF-8 correct.
            let ch_len = utf8_char_len(bytes[i]);
            out.push_str(&input[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

fn utf8_char_len(first_byte: u8) -> usize {
    match first_byte {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        // Invalid lead byte: consume one byte (lossy but never panics).
        _ => 1,
    }
}

fn warn_skipped(path: &Path, line: usize, reason: &str) {
    tracing::warn!(path = %path.display(), line, reason, "malformed cue skipped");
}

// ---------------------------------------------------------------------------
// SRT
// ---------------------------------------------------------------------------

fn parse_srt(text: &str, path: &Path) -> (Vec<Cue>, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let mut cues = Vec::new();
    let mut skipped = 0usize;
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();
        if line.is_empty() {
            i += 1;
            continue;
        }
        // Optional numeric counter: if this line has no `-->`, the next one must.
        let mut ts_line = line;
        if !line.contains("-->") {
            let next = lines.get(i + 1).map(|l| l.trim()).unwrap_or("");
            if next.contains("-->") {
                ts_line = next;
                i += 1;
            } else {
                skipped += 1;
                warn_skipped(path, i + 1, "expected timestamp line");
                i += 1;
                continue;
            }
        }
        let line_no = i + 1;
        if let Some((start, end)) = parse_arrow_pair(ts_line, path, line_no, &mut skipped) {
            i += 1;
            let mut body_lines: Vec<&str> = Vec::new();
            while i < lines.len() && !lines[i].trim().is_empty() {
                body_lines.push(lines[i]);
                i += 1;
            }
            i += 1; // consume the blank separator (or step past EOF)
            let body = body_lines.join("\n");
            let text = strip_html_tags(&body);
            let text = text.trim().to_owned();
            if text.is_empty() {
                skipped += 1;
                warn_skipped(path, line_no, "empty cue text");
                continue;
            }
            cues.push(Cue { start, end, text });
        } else {
            // Bad timestamp: consume this cue's body (but not the next cue's timing line),
            // so one broken cue counts as exactly one skipped entry.
            i += 1;
            while i < lines.len() && !lines[i].trim().is_empty() && !lines[i].contains("-->") {
                i += 1;
            }
            if i < lines.len() && lines[i].trim().is_empty() {
                i += 1; // consume the blank separator
            }
        }
    }
    (cues, skipped)
}

/// Parse `start --> end` (extra whitespace or trailing data tolerated).
fn parse_arrow_pair(
    line: &str,
    path: &Path,
    line_no: usize,
    skipped: &mut usize,
) -> Option<(Duration, Duration)> {
    let mut parts = line.splitn(2, "-->");
    let start_raw = parts.next()?.trim();
    let rest = parts.next()?;
    // The end side may carry cue settings (WebVTT) — take the first token.
    let end_raw = rest.split_whitespace().next().unwrap_or("");
    let start = parse_timestamp(start_raw);
    let end = parse_timestamp(end_raw);
    match (start, end) {
        (Some(s), Some(e)) if e >= s => Some((s, e)),
        (Some(_), Some(_)) => {
            *skipped += 1;
            warn_skipped(path, line_no, "end before start");
            None
        }
        _ => {
            *skipped += 1;
            warn_skipped(path, line_no, "malformed timestamp");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// WebVTT
// ---------------------------------------------------------------------------

fn parse_vtt(text: &str, path: &Path) -> (Vec<Cue>, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let mut cues = Vec::new();
    let mut skipped = 0usize;
    let mut i = 0;

    // Header: `WEBVTT` (optionally with trailing metadata) until the first blank line.
    if i < lines.len() {
        if lines[i].trim_start().starts_with("WEBVTT") {
            i += 1;
            while i < lines.len() && !lines[i].trim().is_empty() {
                i += 1;
            }
            i += 1;
        } else {
            skipped += 1;
            warn_skipped(path, 1, "missing WEBVTT header");
        }
    }

    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }
        if trimmed.starts_with("NOTE") || trimmed.starts_with("::") {
            // Comment / STYLE / REGION block: skip until blank line.
            while i < lines.len() && !lines[i].trim().is_empty() {
                i += 1;
            }
            i += 1;
            continue;
        }
        // Optional cue identifier line (no `-->`): skip it.
        let mut ts_line = trimmed;
        if !trimmed.contains("-->") {
            let next = lines.get(i + 1).map(|l| l.trim()).unwrap_or("");
            if next.contains("-->") {
                ts_line = next;
                i += 1;
            } else {
                skipped += 1;
                warn_skipped(path, i + 1, "expected cue timing line");
                i += 1;
                continue;
            }
        }
        let line_no = i + 1;
        if let Some((start, end)) = parse_arrow_pair(ts_line, path, line_no, &mut skipped) {
            i += 1;
            let mut body_lines: Vec<&str> = Vec::new();
            while i < lines.len() && !lines[i].trim().is_empty() {
                body_lines.push(lines[i]);
                i += 1;
            }
            i += 1;
            let body = body_lines.join("\n");
            let cue_text = strip_html_tags(&body);
            let cue_text = cue_text.trim().to_owned();
            if cue_text.is_empty() {
                skipped += 1;
                warn_skipped(path, line_no, "empty cue text");
                continue;
            }
            cues.push(Cue {
                start,
                end,
                text: cue_text,
            });
        } else {
            // Bad timing line: consume this cue's body (stopping before the next cue's
            // timing line) so one broken cue = exactly one skip.
            i += 1;
            while i < lines.len() && !lines[i].trim().is_empty() && !lines[i].contains("-->") {
                i += 1;
            }
            if i < lines.len() && lines[i].trim().is_empty() {
                i += 1;
            }
        }
    }
    (cues, skipped)
}

// ---------------------------------------------------------------------------
// ASS / SSA
// ---------------------------------------------------------------------------

/// Field order used when the file has no `Format:` line inside `[Events]`.
const ASS_DEFAULT_FORMAT: [&str; 10] = [
    "layer", "start", "end", "style", "name", "marginl", "marginr", "marginv", "effect", "text",
];

fn parse_ass(text: &str, path: &Path) -> (Vec<Cue>, usize) {
    let mut cues = Vec::new();
    let mut skipped = 0usize;
    let mut in_events = false;
    let mut fields: Vec<String> = ASS_DEFAULT_FORMAT.iter().map(|s| s.to_string()).collect();

    for (line_idx, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            in_events = line.eq_ignore_ascii_case("[events]");
            continue;
        }
        if !in_events {
            continue;
        }
        if let Some(rest) = strip_prefix_ignore_case(line, "Format:") {
            let parsed: Vec<String> = rest
                .split(',')
                .map(|f| f.trim().to_ascii_lowercase())
                .collect();
            if parsed.iter().any(|f| f == "start") && parsed.iter().any(|f| f == "text") {
                fields = parsed;
            } else {
                skipped += 1;
                warn_skipped(path, line_idx + 1, "unusable Format line");
            }
            continue;
        }
        let Some(dialogue) = strip_prefix_ignore_case(line, "Dialogue:") else {
            continue; // Comment:, Style:, etc.
        };
        let start_idx = fields.iter().position(|f| f == "start");
        let end_idx = fields.iter().position(|f| f == "end");
        let text_idx = fields.iter().position(|f| f == "text");
        let (Some(si), Some(ei), Some(ti)) = (start_idx, end_idx, text_idx) else {
            skipped += 1;
            warn_skipped(path, line_idx + 1, "Format line lacks start/end/text");
            continue;
        };
        let needed = si.max(ei).max(ti) + 1;
        let mut parts = dialogue.splitn(needed, ',');
        let values: Vec<&str> = parts.by_ref().collect();
        if values.len() < needed {
            skipped += 1;
            warn_skipped(path, line_idx + 1, "Dialogue line has too few fields");
            continue;
        }
        let Some(start) = parse_timestamp(values[si]) else {
            skipped += 1;
            warn_skipped(path, line_idx + 1, "malformed start timestamp");
            continue;
        };
        let Some(end) = parse_timestamp(values[ei]) else {
            skipped += 1;
            warn_skipped(path, line_idx + 1, "malformed end timestamp");
            continue;
        };
        if end < start {
            skipped += 1;
            warn_skipped(path, line_idx + 1, "end before start");
            continue;
        }
        // The text field is the last split part, so re-join anything after it that was
        // split away (splitn already keeps commas in the tail, but be explicit).
        let text_part = values[ti];
        let body = process_ass_text(text_part);
        let body = body.trim().to_owned();
        if body.is_empty() {
            // Drawing-only or empty dialogue: not an error, but nothing to show.
            continue;
        }
        cues.push(Cue {
            start,
            end,
            text: body,
        });
    }
    (cues, skipped)
}

fn strip_prefix_ignore_case<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    if line.len() >= prefix.len() && line[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&line[prefix.len()..])
    } else {
        None
    }
}

/// Process ASS dialogue text: drop `{...}` override tags (tracking `\p` drawing mode to
/// drop drawing commands), convert `\N`/`\n`/`\r` to newlines and `\h` to a space.
pub fn process_ass_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut drawing = false;
    let mut rest = input;

    while let Some(tag_start) = rest.find('{') {
        let (before, tail) = rest.split_at(tag_start);
        if !drawing {
            out.push_str(before);
        }
        let after_tag = &tail[1..];
        match after_tag.find('}') {
            Some(tag_end) => {
                let tag = &after_tag[..tag_end];
                if let Some(mode) = ass_drawing_mode(tag) {
                    tracing::trace!(mode, "ASS drawing mode");
                    drawing = mode;
                }
                rest = &after_tag[tag_end + 1..];
            }
            None => {
                // Unterminated override block: keep the `{` and everything after it as
                // literal text (unless drawing) rather than dropping the rest of the line.
                if !drawing {
                    out.push_str(tail);
                }
                rest = ""; // remainder already consumed — do not append it again below
                break;
            }
        }
    }
    if !drawing {
        out.push_str(rest);
    }

    out.replace("\\N", "\n")
        .replace("\\n", "\n")
        .replace("\\r", "\n")
        .replace("\\h", " ")
}

/// Extract a drawing-mode switch from an override tag: `\p1` on, `\p0` off.
/// `\pos`, `\pbo` etc. are ignored (next char is not a digit).
fn ass_drawing_mode(tag: &str) -> Option<bool> {
    let bytes = tag.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'\\'
            && (bytes[i + 1] == b'p' || bytes[i + 1] == b'P')
            && i + 2 < bytes.len()
            && bytes[i + 2].is_ascii_digit()
        {
            return Some(bytes[i + 2] != b'0');
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        let bytes = std::fs::read(&path).expect("fixture read");
        decode_bytes(&bytes).0
    }

    fn fixture_bytes(name: &str) -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        std::fs::read(&path).expect("fixture read")
    }

    #[test]
    fn timestamp_formats() {
        assert_eq!(
            parse_timestamp("00:00:01,500"),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(
            parse_timestamp("00:00:01.500"),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(
            parse_timestamp("1:02:03.25"),
            Some(Duration::from_millis(3_723_250))
        );
        assert_eq!(
            parse_timestamp("0:00:04.20"),
            Some(Duration::from_millis(4200))
        );
        assert_eq!(
            parse_timestamp("02:03.250"),
            Some(Duration::from_millis(123_250))
        );
        assert_eq!(parse_timestamp("00:00:00.000"), Some(Duration::ZERO));
        assert_eq!(
            parse_timestamp("0:00:05.5"),
            Some(Duration::from_millis(5500))
        );
        assert_eq!(parse_timestamp(""), None);
        assert_eq!(parse_timestamp("not:a:time"), None);
        assert_eq!(parse_timestamp("00:0x:00"), None);
        assert_eq!(parse_timestamp("::"), None);
        assert_eq!(parse_timestamp("abc"), None);
    }

    #[test]
    fn html_tag_stripping() {
        assert_eq!(strip_html_tags("<i>hello</i>"), "hello");
        assert_eq!(strip_html_tags("line<br>break"), "line\nbreak");
        assert_eq!(strip_html_tags("<font color=\"#fff\">x</font>"), "x");
        assert_eq!(strip_html_tags("a < b"), "a < b", "bare < must survive");
        assert_eq!(strip_html_tags("日本<i>語</i>"), "日本語");
    }

    #[test]
    fn ass_text_processing() {
        assert_eq!(process_ass_text("{\\an8}top line"), "top line");
        assert_eq!(process_ass_text("one\\Ntwo"), "one\ntwo");
        assert_eq!(process_ass_text("{\\pos(1,2)}pos removed"), "pos removed");
        // Drawing commands between {\p1} and {\p0} are dropped entirely.
        assert_eq!(
            process_ass_text("{\\p1}m 0 0 l 100 0 100 100{\\p0}visible"),
            "visible"
        );
        // \pos must not be mistaken for drawing mode.
        assert_eq!(process_ass_text("{\\pos(1,2)}text"), "text");
        assert_eq!(
            process_ass_text("under{"),
            "under{",
            "unterminated tag kept"
        );
    }

    #[test]
    fn parses_srt_fixture_with_html_and_crlf() {
        let text = fixture("simple.srt");
        let (cues, skipped) = parse_srt(&text, Path::new("simple.srt"));
        assert_eq!(skipped, 0, "{:?}", cues);
        assert_eq!(cues.len(), 3, "{:?}", cues);
        assert_eq!(cues[0].text, "こんにちは、世界！");
        assert_eq!(cues[1].text, "これは強調です", "HTML tags stripped");
        assert_eq!(cues[1].start, Duration::from_millis(2_100));
        assert_eq!(cues[2].text, "line one\nline two");
    }

    #[test]
    fn shift_jis_fixture_decodes_correctly() {
        let bytes = fixture_bytes("shiftjis.srt");
        let (text, encoding) = decode_bytes(&bytes);
        let upper = encoding.to_ascii_uppercase();
        assert!(
            upper.contains("SHIFT") || upper.contains("SJIS") || encoding == "utf-8",
            "detected {encoding}"
        );
        let (cues, skipped) = parse_srt(&text, Path::new("shiftjis.srt"));
        assert_eq!(skipped, 0);
        assert_eq!(cues.len(), 2, "{encoding}: {cues:?}");
        assert_eq!(cues[0].text, "こんにちは、世界！これはテストです。");
        assert_eq!(cues[1].text, "日本語の字幕を楽しんでください。");
    }

    #[test]
    fn parses_vtt_fixture_with_notes_and_settings() {
        let text = fixture("simple.vtt");
        let (cues, skipped) = parse_vtt(&text, Path::new("simple.vtt"));
        assert_eq!(skipped, 0, "{:?}", cues);
        assert_eq!(cues.len(), 3, "{:?}", cues);
        assert_eq!(cues[0].start, Duration::from_millis(1_000));
        assert_eq!(cues[0].text, "short form timestamp");
        assert_eq!(cues[1].text, "styled text kept readable", "tags stripped");
        assert_eq!(cues[2].end, Duration::from_millis(7_500));
    }

    #[test]
    fn parses_ass_fixture_with_tags_and_drawings() {
        let text = fixture("sample.ass");
        let (cues, skipped) = parse_ass(&text, Path::new("sample.ass"));
        assert_eq!(skipped, 0, "{:?}", cues);
        assert_eq!(cues.len(), 3, "{:?}", cues);
        assert_eq!(cues[0].text, "top line via an8");
        assert_eq!(cues[0].start, Duration::from_millis(500));
        assert_eq!(cues[1].text, "first\nsecond");
        // The drawing-only dialogue must be dropped, the text one kept.
        assert_eq!(cues[2].text, "after drawing");
    }

    #[test]
    fn malformed_ass_skips_without_panicking() {
        let text = fixture("malformed.ass");
        let (cues, skipped) = parse_ass(&text, Path::new("malformed.ass"));
        assert!(skipped >= 3, "expected skips, got {skipped}");
        // The unterminated-override cue is tolerated (rest kept as literal text) and the
        // one valid cue still comes through.
        assert_eq!(cues.len(), 2, "{:?}", cues);
        assert!(
            cues[0].text.starts_with("unterminated"),
            "tolerated literal: {:?}",
            cues[0].text
        );
        assert_eq!(cues[1].text, "the only good line");
    }

    #[test]
    fn malformed_srt_skips_bad_timestamps() {
        let text = "1\n00:00:00,000 --> bogus\nbad\n\n2\n00:00:05,000 --> 00:00:06,000\ngood\n";
        let (cues, skipped) = parse_srt(text, Path::new("mem.srt"));
        assert_eq!(skipped, 1);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "good");
    }

    #[test]
    fn end_before_start_is_skipped() {
        let text = "1\n00:00:10,000 --> 00:00:01,000\nbackwards\n";
        let (cues, skipped) = parse_srt(text, Path::new("mem.srt"));
        assert_eq!(skipped, 1);
        assert!(cues.is_empty());
    }
}
