//! Ruby annotation for Japanese text (furigana).
//!
//! Two consumers share this module so the overlay and the Anki card never disagree about
//! how a reading is attached to its surface:
//!
//! - the **Anki card field** (`{sentence_furigana}`) needs a plain, serializable string —
//!   Anki renders HTML, so [`sentence_furigana`] emits `<ruby>base<rt>reading</rt></ruby>`
//!   markup with okurigana left outside the ruby element;
//! - the **overlay** (`subtitle.show_furigana`) needs just the reading of each kanji-bearing
//!   token to paint above it, which is [`reading_for`].
//!
//! IPADIC readings are katakana (`私` → `ワタシ`), so they are converted to hiragana first.

use crate::tokenize::Token;

/// True for CJK ideographs plus the iteration mark 々, which is read as part of the word.
pub fn is_kanji(ch: char) -> bool {
    matches!(ch as u32,
        0x3400..=0x4DBF      // CJK Extension A
        | 0x4E00..=0x9FFF    // CJK Unified Ideographs
        | 0xF900..=0xFAFF    // CJK Compatibility Ideographs
        | 0x3005             // 々 iteration mark
    )
}

/// True for hiragana (0x3041–0x3096) and katakana (0x30A1–0x30F6), i.e. the kana that can
/// sit outside a ruby element as okurigana.
fn is_kana(ch: char) -> bool {
    matches!(ch as u32, 0x3041..=0x3096 | 0x30A1..=0x30F6)
}

/// Convert a katakana string to hiragana (the same codepoint shifted by 0x60). Anything
/// outside the katakana block — including ー and ・ — is left alone, so `コーヒー` stays
/// `こーひー` rather than drifting into unrelated codepoints.
pub fn katakana_to_hiragana(input: &str) -> String {
    input
        .chars()
        .map(|ch| match ch as u32 {
            0x30A1..=0x30F6 => char::from_u32(ch as u32 - 0x60).unwrap_or(ch),
            _ => ch,
        })
        .collect()
}

/// The kana reading to show above `surface`, or `None` when there is nothing to annotate:
/// kana-only surfaces (rendering `かな` over `かな` is noise), readings equal to the surface,
/// or words IPADIC gave no reading for.
pub fn reading_for(surface: &str, reading: &str) -> Option<String> {
    if !surface.chars().any(is_kanji) {
        return None;
    }
    let hiragana = katakana_to_hiragana(reading);
    if hiragana.is_empty() || hiragana == surface {
        return None;
    }
    Some(hiragana)
}

/// Ruby-annotate one surface: okurigana stays outside the `<ruby>` element, so `食べる` +
/// `タベル` becomes `<ruby>食<rt>た</rt></ruby>べる`. Surfaces with no annotatable part are
/// returned unchanged (escaped as little as possible — this feeds an Anki HTML field).
pub fn annotate(surface: &str, reading: &str) -> String {
    let Some(hiragana) = reading_for(surface, reading) else {
        return surface.to_owned();
    };
    let s: Vec<char> = surface.chars().collect();
    let r: Vec<char> = hiragana.chars().collect();

    // Leading okurigana that the reading spells the same way (お金 → おかね).
    let mut prefix = 0;
    while prefix < s.len().min(r.len())
        && !is_kanji(s[prefix])
        && is_kana(s[prefix])
        && s[prefix] == r[prefix]
    {
        prefix += 1;
    }
    // Trailing okurigana, never overlapping the prefix we just consumed (食べる → べる).
    let mut suffix = 0;
    while suffix < (s.len() - prefix).min(r.len() - prefix)
        && !is_kanji(s[s.len() - 1 - suffix])
        && is_kana(s[s.len() - 1 - suffix])
        && s[s.len() - 1 - suffix] == r[r.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let base: String = s[prefix..s.len() - suffix].iter().collect();
    let rt: String = r[prefix..r.len() - suffix].iter().collect();
    // Both halves must survive: annotating 4 kana with 3 would be worse than no annotation.
    if !base.chars().any(is_kanji) || rt.is_empty() {
        return surface.to_owned();
    }
    let leading: String = s[..prefix].iter().collect();
    let trailing: String = s[s.len() - suffix..].iter().collect();
    format!("{leading}<ruby>{base}<rt>{rt}</rt></ruby>{trailing}")
}

/// Build the whole sentence with every token annotated — the `{sentence_furigana}` card
/// field. Token surfaces concatenate back into the original sentence, so this only inserts
/// markup.
pub fn sentence_furigana(tokens: &[Token]) -> String {
    let mut out = String::new();
    for token in tokens {
        out.push_str(&annotate(&token.surface, &token.reading));
    }
    out
}

/// The whole sentence in kana — `{sentence_reading}`, the `Reading` field of a sentence
/// card (the whole line has no single dictionary reading, and IPADIC already knows one for
/// every token). Kana-only tokens keep their surface, so particles and punctuation are not
/// rewritten; only tokens the tokenizer gives a real reading for are converted.
pub fn sentence_reading(tokens: &[Token]) -> String {
    let mut out = String::new();
    for token in tokens {
        if token.reading.is_empty() || !token.surface.chars().any(is_kanji) {
            out.push_str(&token.surface);
        } else {
            out.push_str(&katakana_to_hiragana(&token.reading));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentence_reading_is_kana_and_survives_every_token() {
        let tokenizer = crate::tokenize::JapaneseTokenizer::new().expect("tokenizer");
        let tokens = tokenizer.tokenize("私は学生です。").expect("tokenize");
        assert_eq!(sentence_reading(&tokens), "わたしはがくせいです。");
        // An unknown reading must never silently drop the word from the sentence.
        let mut broken = tokens.clone();
        broken[0].reading = String::new();
        assert_eq!(sentence_reading(&broken), "私はがくせいです。");
        assert_eq!(sentence_reading(&[]), "");
    }

    #[test]
    fn katakana_readings_become_hiragana_without_touching_the_choonpu() {
        assert_eq!(katakana_to_hiragana("ワタシ"), "わたし");
        assert_eq!(katakana_to_hiragana("コーヒー"), "こーひー");
        assert_eq!(katakana_to_hiragana("ヴァイオリン"), "ゔぁいおりん");
        // Non-katakana input passes through untouched.
        assert_eq!(katakana_to_hiragana("abc 12"), "abc 12");
    }

    #[test]
    fn kana_only_surfaces_are_not_annotated() {
        assert_eq!(
            reading_for("が", "ガ"),
            None,
            "particle: reading adds nothing"
        );
        assert_eq!(reading_for("コーヒー", "コーヒー"), None);
        assert_eq!(annotate("が", "ガ"), "が");
    }

    #[test]
    fn okurigana_stays_outside_the_ruby_element() {
        // Trailing kana (食べる) and leading kana (お金) both stay outside.
        assert_eq!(
            annotate("食べる", "タベル"),
            "<ruby>食<rt>た</rt></ruby>べる"
        );
        assert_eq!(annotate("お金", "オカネ"), "お<ruby>金<rt>かね</rt></ruby>");
        // No kana at all: the whole word is annotated.
        assert_eq!(
            annotate("学生", "ガクセイ"),
            "<ruby>学生<rt>がくせい</rt></ruby>"
        );
        assert_eq!(
            annotate("毎日", "マイニチ"),
            "<ruby>毎日<rt>まいにち</rt></ruby>"
        );
    }

    #[test]
    fn missing_readings_never_produce_empty_ruby() {
        assert_eq!(annotate("学生", ""), "学生");
        assert_eq!(reading_for("学生", ""), None);
        // A reading that is entirely okurigana would leave `<rt></rt>` behind; the surface
        // stays plain instead of emitting an empty annotation.
        assert_eq!(annotate("食べる", "ベル"), "食べる");
        // Marks outside the kanji ranges (〆, ヶ, ー) are deliberately not annotated.
        assert_eq!(annotate("〆", "シメ"), "〆");
    }

    #[test]
    fn sentence_furigana_annotates_only_the_words_that_need_it() {
        let tokenizer = crate::tokenize::JapaneseTokenizer::new().expect("tokenizer");
        let tokens = tokenizer.tokenize("私は学生です。").expect("tokenize");
        let annotated = sentence_furigana(&tokens);
        assert_eq!(
            annotated,
            "<ruby>私<rt>わたし</rt></ruby>は<ruby>学生<rt>がくせい</rt></ruby>です。"
        );
        // Every token surface survives in order once the readings are dropped, so the
        // markup only annotates — nothing is dropped, duplicated or reordered.
        assert_eq!(surface_only(&annotated), "私は学生です。");
    }

    /// Recover the plain surface text from ruby markup: drop every `<rt>…</rt>` element
    /// (which holds the reading), then the remaining tags.
    fn surface_only(input: &str) -> String {
        let mut without_rt = String::new();
        let mut rest = input;
        while let Some(start) = rest.find("<rt>") {
            without_rt.push_str(&rest[..start]);
            match rest[start..].find("</rt>") {
                Some(end) => rest = &rest[start + end + "</rt>".len()..],
                None => {
                    rest = "";
                    break;
                }
            }
        }
        without_rt.push_str(rest);

        let mut out = String::new();
        let mut rest = without_rt.as_str();
        while let Some(start) = rest.find('<') {
            out.push_str(&rest[..start]);
            match rest[start..].find('>') {
                Some(end) => rest = &rest[start + end + 1..],
                None => {
                    rest = "";
                    break;
                }
            }
        }
        out.push_str(rest);
        out
    }
}
