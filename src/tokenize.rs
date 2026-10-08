//! Morphological analysis: a thin, owned wrapper around lindera's IPADIC segmenter.
//!
//! Tokens are copied out of lindera's borrow-heavy API into plain owned structs so the
//! UI (and tests) can hold them without keeping the source sentence alive. Offsets are
//! byte offsets into the original sentence — M4 uses them for per-token hit-testing.

use std::borrow::Cow;

use lindera::dictionary::load_dictionary;
use lindera::mode::Mode;
use lindera::segmenter::Segmenter;

use crate::error::DictionaryError;

/// IPADIC detail layout (verified against lindera-ipadic 6.2.0):
/// 0=品詞 1=細分類1 2=細分類2 3=細分類3 4=活用型 5=活用形 6=基本形 7=読み 8=発音
const IDX_POS: usize = 0;
const IDX_POS_DETAIL: usize = 1;
const IDX_CONJ_TYPE: usize = 4;
const IDX_CONJ_FORM: usize = 5;
const IDX_BASE_FORM: usize = 6;
const IDX_READING: usize = 7;

/// One token with everything the overlay needs to display, look up and mine it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// Surface form as it appears in the sentence.
    pub surface: String,
    /// Byte range `[start, end)` of the surface in the source sentence.
    pub byte_start: usize,
    pub byte_end: usize,
    /// Coarse part of speech (品詞), e.g. `名詞`.
    pub pos: String,
    /// Fine-grained POS (品詞細分類), e.g. `普通名詞`.
    pub pos_detail: String,
    /// 活用型, e.g. `五段-カ行`; empty for non-conjugating words.
    pub conjugation_type: String,
    /// 活用形, e.g. `連用形`; empty for non-conjugating words.
    pub conjugation_form: String,
    /// 基本形 (dictionary form); IPADIC writes `*` when the word does not conjugate —
    /// normalized to an empty string here.
    pub base_form: String,
    /// 読み (kana reading); `*` normalized to empty.
    pub reading: String,
}

impl Token {
    /// True for words that carry meaning we would mine (nouns, verbs, adjectives,
    /// adverbs) — particles/conjunctions/etc. are skipped by callers.
    pub fn is_content_word(&self) -> bool {
        matches!(
            self.pos.as_str(),
            "名詞" | "動詞" | "形容詞" | "形容動詞" | "副詞" | "連体詞"
        )
    }
}

/// Process-wide tokenizer. Building it decodes the embedded IPADIC data, so it is built
/// once and reused (tests use a `OnceLock`).
pub struct JapaneseTokenizer {
    segmenter: Segmenter,
}

impl JapaneseTokenizer {
    /// Load the embedded IPADIC dictionary. Fails with a typed error if the embedded
    /// data is corrupt — never panics.
    pub fn new() -> Result<Self, DictionaryError> {
        let dictionary = load_dictionary("embedded://ipadic")
            .map_err(|err| DictionaryError::Tokenizer(err.to_string()))?;
        Ok(Self {
            segmenter: Segmenter::new(Mode::Normal, dictionary, None),
        })
    }

    /// Tokenize a sentence into owned [`Token`]s with byte offsets into `text`.
    pub fn tokenize(&self, text: &str) -> Result<Vec<Token>, DictionaryError> {
        let mut lindera_tokens = self
            .segmenter
            .segment(Cow::Borrowed(text))
            .map_err(|err| DictionaryError::Tokenizer(err.to_string()))?;

        Ok(lindera_tokens
            .iter_mut()
            .map(|token| {
                // Copy non-borrowed fields first: `details()` takes `&mut token`.
                let surface = token.surface.to_string();
                let byte_start = token.byte_start;
                let byte_end = token.byte_end;
                let details: Vec<String> = token
                    .details() // IPADIC order, see module docs
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
                let get = |idx: usize| -> String {
                    details
                        .get(idx)
                        .map(|d| d.trim())
                        .filter(|d| !d.is_empty() && *d != "*")
                        .unwrap_or("")
                        .to_owned()
                };
                Token {
                    surface,
                    byte_start,
                    byte_end,
                    pos: get(IDX_POS),
                    pos_detail: get(IDX_POS_DETAIL),
                    conjugation_type: get(IDX_CONJ_TYPE),
                    conjugation_form: get(IDX_CONJ_FORM),
                    base_form: get(IDX_BASE_FORM),
                    reading: get(IDX_READING),
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    fn tokenizer() -> &'static JapaneseTokenizer {
        static TOK: OnceLock<JapaneseTokenizer> = OnceLock::new();
        TOK.get_or_init(|| JapaneseTokenizer::new().expect("embedded IPADIC loads"))
    }

    #[test]
    fn tokenizes_a_simple_sentence() {
        let tokens = tokenizer().tokenize("私は学生です。").expect("tokenize");
        let surfaces: Vec<&str> = tokens.iter().map(|t| t.surface.as_str()).collect();
        assert_eq!(surfaces, ["私", "は", "学生", "です", "。"], "{surfaces:?}");
        let watashi = &tokens[0];
        assert_eq!(watashi.pos, "名詞");
        assert_eq!(watashi.reading, "ワタシ");
        assert_eq!(watashi.base_form, "私");
        assert_eq!(watashi.byte_start, 0);
        assert_eq!(watashi.byte_end, "私".len());
    }

    #[test]
    fn offsets_are_byte_accurate_for_mixed_text() {
        let text = "Cat は走った。";
        let tokens = tokenizer().tokenize(text).expect("tokenize");
        for token in &tokens {
            let slice = &text[token.byte_start..token.byte_end];
            assert_eq!(slice, token.surface, "offset mismatch for {token:?}");
        }
    }

    #[test]
    fn verb_tokens_carry_conjugation_info() {
        let tokens = tokenizer()
            .tokenize("山田さんは本を読んでいた。")
            .expect("tokenize");
        // MeCab splits euphonic changes: 読んでいた → 読ん + で + い + た.
        let yomu = tokens
            .iter()
            .find(|t| t.surface == "読ん")
            .expect("読ん tokenized");
        assert_eq!(yomu.pos, "動詞");
        assert_eq!(yomu.base_form, "読む");
        assert_eq!(yomu.reading, "ヨン");
        assert!(!yomu.conjugation_type.is_empty(), "活用型 must be set");
        assert!(yomu.is_content_word());
    }

    #[test]
    fn particles_are_not_content_words() {
        let tokens = tokenizer()
            .tokenize("猫が犬に追いかけられた。")
            .expect("tokenize");
        let particle = tokens
            .iter()
            .find(|t| t.surface == "が")
            .expect("が tokenized");
        assert!(!particle.is_content_word(), "{particle:?}");
    }

    #[test]
    fn empty_and_ascii_text_do_not_error() {
        assert!(tokenizer().tokenize("").expect("empty").is_empty());
        let tokens = tokenizer().tokenize("hello 123").expect("ascii");
        assert!(!tokens.is_empty());
        for token in &tokens {
            assert!(!token.surface.is_empty());
        }
    }
}
