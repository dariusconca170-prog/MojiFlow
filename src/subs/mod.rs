//! Subtitle model: cues, sorted tracks, O(log n) active-cue lookup, file loading with
//! encoding detection and format dispatch.
//!
//! Timing model (Section 5.1): `effective_time = clock.now() + user_offset`. The offset is
//! applied by the caller (`App`) before querying the track, so the track itself stays a
//! pure data structure.

pub mod parser;

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::SubtitleError;

/// One subtitle cue. `end >= start` is guaranteed by the parsers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub start: Duration,
    pub end: Duration,
    pub text: String,
}

impl Cue {
    pub fn contains(&self, t: Duration) -> bool {
        t >= self.start && t < self.end
    }
}

/// The subtitle file formats we parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Srt,
    WebVtt,
    Ass,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::Srt => "SRT",
            Format::WebVtt => "WebVTT",
            Format::Ass => "ASS/SSA",
        }
    }
}

/// A parsed, sorted subtitle track.
#[derive(Debug, Clone)]
pub struct SubtitleTrack {
    cues: Vec<Cue>,
    path: PathBuf,
    format: Format,
    encoding: String,
    /// Count of malformed entries that were skipped (surfaced in the UI status).
    pub skipped: usize,
}

impl SubtitleTrack {
    /// Build a track from parsed cues. Sorts by start time (stable) and rejects empty
    /// tracks. Duplicate/stacked cues are merged here so every consumer sees the same view.
    pub fn new(
        mut cues: Vec<Cue>,
        path: PathBuf,
        format: Format,
        encoding: String,
        skipped: usize,
    ) -> Result<Self, SubtitleError> {
        if cues.is_empty() {
            return Err(SubtitleError::EmptyTrack { path });
        }
        cues.sort_by_key(|c| (c.start, c.end));
        let cues = merge_stacked(cues);
        Ok(Self {
            cues,
            path,
            format,
            encoding,
            skipped,
        })
    }

    /// Index of the cue active at `t` (start <= t < end), found by binary search in
    /// O(log n). Returns `None` during gaps between cues.
    pub fn active_at(&self, t: Duration) -> Option<usize> {
        if self.cues.is_empty() {
            return None;
        }
        // Find the last cue with start <= t.
        let idx = match self.cues.binary_search_by_key(&t, |c| c.start) {
            Ok(i) => i,
            Err(0) => return None,
            Err(i) => i - 1,
        };
        // Walk back over any earlier cue that also contains t (overlapping starts are
        // merged by `merge_stacked`, but hand-edited tracks can still share a start).
        let mut i = idx;
        loop {
            if self.cues[i].contains(t) {
                return Some(i);
            }
            if i == 0 || self.cues[i].start >= t {
                break;
            }
            i -= 1;
        }
        None
    }

    pub fn cue(&self, index: usize) -> Option<&Cue> {
        self.cues.get(index)
    }

    pub fn len(&self) -> usize {
        self.cues.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cues.is_empty()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn format(&self) -> Format {
        self.format
    }

    pub fn encoding(&self) -> &str {
        &self.encoding
    }

    /// All cue texts in order — used to build the sentence-furigana field and for tests.
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        self.cues.iter().map(|c| c.text.as_str())
    }
}

/// Merge cues that are "stacked": identical text whose windows touch or overlap (players
/// and converters frequently emit the same line as two adjacent cues).
fn merge_stacked(cues: Vec<Cue>) -> Vec<Cue> {
    let mut out: Vec<Cue> = Vec::with_capacity(cues.len());
    for cue in cues {
        if let Some(prev) = out.last_mut() {
            let touches = cue.start <= prev.end + Duration::from_millis(120);
            if touches && cue.text == prev.text {
                if cue.end > prev.end {
                    prev.end = cue.end;
                }
                continue;
            }
        }
        out.push(cue);
    }
    out
}

/// Load a subtitle file from disk: read bytes, detect encoding, detect format (by
/// extension, falling back to content sniffing), parse.
///
/// This performs file I/O — call it from a worker thread, never the UI thread.
pub fn load_path(path: &Path) -> Result<SubtitleTrack, SubtitleError> {
    let bytes = std::fs::read(path).map_err(|source| SubtitleError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let (text, encoding) = parser::decode_bytes(&bytes);
    let format = detect_format(path, &text).ok_or_else(|| SubtitleError::UnsupportedFormat {
        path: path.to_path_buf(),
    })?;
    let (cues, skipped) = parser::parse(format, &text, path);
    tracing::info!(
        path = %path.display(),
        format = format.label(),
        encoding,
        cues = cues.len(),
        skipped,
        "subtitle track loaded"
    );
    SubtitleTrack::new(
        cues,
        path.to_path_buf(),
        format,
        encoding.to_owned(),
        skipped,
    )
}

/// Format detection: extension first, then content sniffing for renamed files.
fn detect_format(path: &Path, text: &str) -> Option<Format> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default()
        .as_str()
    {
        "srt" => Some(Format::Srt),
        "vtt" => Some(Format::WebVtt),
        "ass" | "ssa" => Some(Format::Ass),
        _ => sniff_format(text),
    }
}

fn sniff_format(text: &str) -> Option<Format> {
    let head = &text[..text.len().min(4096)];
    if head.trim_start().starts_with("WEBVTT") {
        return Some(Format::WebVtt);
    }
    if head.contains("[Script Info]") || head.contains("[Events]") {
        return Some(Format::Ass);
    }
    // SRT: first non-empty line is a number, second contains "-->".
    let mut lines = head.lines().filter(|l| !l.trim().is_empty());
    let first = lines.next()?;
    if first.trim().chars().all(|c| c.is_ascii_digit())
        && lines.next().is_some_and(|l| l.contains("-->"))
    {
        return Some(Format::Srt);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(start_ms: u64, end_ms: u64, text: &str) -> Cue {
        Cue {
            start: Duration::from_millis(start_ms),
            end: Duration::from_millis(end_ms),
            text: text.to_owned(),
        }
    }

    fn track(cues: Vec<Cue>) -> SubtitleTrack {
        SubtitleTrack::new(
            cues,
            PathBuf::from("test.srt"),
            Format::Srt,
            "utf-8".into(),
            0,
        )
        .expect("track")
    }

    #[test]
    fn active_at_binary_search_finds_cue_and_gaps() {
        let t = track(vec![
            cue(0_000, 1_000, "one"),
            cue(2_000, 3_000, "two"),
            cue(5_000, 6_000, "three"),
        ]);
        assert_eq!(t.active_at(Duration::from_millis(500)), Some(0));
        assert_eq!(t.active_at(Duration::from_millis(1_500)), None, "gap");
        assert_eq!(t.active_at(Duration::from_millis(2_999)), Some(1));
        assert_eq!(
            t.active_at(Duration::from_millis(6_000)),
            None,
            "end exclusive"
        );
        assert_eq!(t.active_at(Duration::from_millis(0)), Some(0));
    }

    #[test]
    fn active_at_works_on_unsorted_input() {
        // Construction sorts by start time, so binary search stays valid no matter what
        // order the file (or a hand-built vector) delivered the cues in.
        let t = track(vec![
            cue(5_000, 6_000, "third"),
            cue(0, 1_000, "first"),
            cue(2_000, 3_000, "second"),
        ]);
        let text_at = |ms: u64| {
            t.active_at(Duration::from_millis(ms))
                .and_then(|i| t.cue(i))
                .map(|c| c.text.clone())
        };
        assert_eq!(text_at(2_500), Some("second".to_owned()));
        assert_eq!(text_at(200), Some("first".to_owned()));
        assert_eq!(text_at(7_000), None, "past the end");
    }

    #[test]
    fn merges_stacked_duplicate_cues() {
        let t = track(vec![
            cue(0, 1_000, "same line"),
            cue(1_000, 1_500, "same line"),
            cue(1_600, 2_000, "other"),
        ]);
        assert_eq!(t.len(), 2, "{:?}", t);
        assert_eq!(t.cue(0).expect("cue").end, Duration::from_millis(1_500));
    }

    #[test]
    fn empty_track_is_an_error() {
        let err = SubtitleTrack::new(
            vec![],
            PathBuf::from("x.srt"),
            Format::Srt,
            "utf-8".into(),
            0,
        )
        .expect_err("empty must fail");
        assert!(matches!(err, SubtitleError::EmptyTrack { .. }));
    }

    #[test]
    fn sniffing_detects_renamed_files() {
        assert_eq!(
            sniff_format("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nhi"),
            Some(Format::WebVtt)
        );
        assert_eq!(
            sniff_format("[Script Info]\nTitle: x\n\n[Events]\n"),
            Some(Format::Ass)
        );
        assert_eq!(
            sniff_format("1\n00:00:01,000 --> 00:00:02,000\nhello\n"),
            Some(Format::Srt)
        );
        assert_eq!(sniff_format("random prose"), None);
    }
}
