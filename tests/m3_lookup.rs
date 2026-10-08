//! M3 gate: end-to-end sentence pipeline against the real downloaded databases —
//! tokenize 22 Japanese sentences, resolve every content word through de-inflection
//! into JMdict (with pitch + frequency attached), and enforce the warm-lookup
//! latency budget. Requires `cargo xtask build-dict` to have run; skips politely
//! otherwise so a fresh clone's `cargo test` stays green.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use medialingual_native::config::DictionaryConfig;
use medialingual_native::dict::Dictionary;
use medialingual_native::tokenize::JapaneseTokenizer;

const SENTENCES: [&str; 22] = [
    "私は毎朝コーヒーを飲みます。",
    "彼女は本を読んでいます。",
    "昨日、友達と映画を見ました。",
    "学校で日本語を勉強しました。",
    "明日の天気は大丈夫ですか。",
    "この料理はとてもおいしいです。",
    "彼は電車で東京へ行きました。",
    "私たちは毎週日曜日に公園を走ります。",
    "雨が降っていたので、家にいました。",
    "先生に質問しました。",
    "弟はゲームをしたいです。",
    "冷蔵庫に牛乳が残っています。",
    "誰が窓を開けましたか。",
    "この写真はとてもきれいですね。",
    "友達に電話をかけています。",
    "毎日、犬と公園を散歩します。",
    "お金がなくなったので、仕事を探しました。",
    "彼は早く起きることができます。",
    "私の家は駅から近いです。",
    "終電に間に合いました。",
    "読み終わった本を返しました。",
    "音楽を聞きながら勉強します。",
];

fn tokenizer() -> &'static JapaneseTokenizer {
    static TOK: OnceLock<JapaneseTokenizer> = OnceLock::new();
    TOK.get_or_init(|| JapaneseTokenizer::new().expect("embedded IPADIC loads"))
}

/// Open the downloaded databases, or return `None` when they haven't been built yet.
fn open_dictionary() -> Option<Dictionary> {
    let config = DictionaryConfig::default();
    if !config.jmdict_path.exists() {
        eprintln!(
            "dictionary missing at {} — run `cargo xtask build-dict`; skipping M3 gate",
            config.jmdict_path.display()
        );
        return None;
    }
    Some(Dictionary::open(&config).expect("databases open"))
}

#[test]
fn twenty_two_sentences_resolve_through_the_dictionary() {
    let Some(mut dict) = open_dictionary() else {
        return;
    };
    let tok = tokenizer();

    let mut total_content = 0usize;
    let mut resolved = 0usize;
    let mut sentences_with_hits = 0usize;
    let mut misses: Vec<String> = Vec::new();

    for sentence in SENTENCES {
        let tokens = tok.tokenize(sentence).expect("tokenize");
        let mut hits_this_sentence = 0usize;
        for token in tokens.iter().filter(|t| t.is_content_word()) {
            total_content += 1;
            let lookup_term = if token.base_form.is_empty() {
                token.surface.as_str()
            } else {
                token.base_form.as_str()
            };
            let entries = dict.resolve(lookup_term).expect("resolve");
            if entries.is_empty() {
                misses.push(format!("{sentence} ← {lookup_term} ({})", token.pos));
            } else {
                resolved += 1;
                hits_this_sentence += 1;
            }
        }
        if hits_this_sentence > 0 {
            sentences_with_hits += 1;
        }
    }

    let coverage = resolved as f64 / total_content as f64;
    eprintln!(
        "M3 coverage: {}/{} content words ({:.1}%), {}/{} sentences with hits",
        resolved,
        total_content,
        coverage * 100.0,
        sentences_with_hits,
        SENTENCES.len()
    );
    if coverage < 0.95 {
        eprintln!("unresolved words: {misses:#?}");
    }

    assert_eq!(
        sentences_with_hits,
        SENTENCES.len(),
        "every sentence must resolve at least one content word"
    );
    assert!(
        coverage >= 0.70,
        "content-word resolution coverage {:.1}% below the 70% gate; misses: {misses:#?}",
        coverage * 100.0
    );
}

#[test]
fn entries_carry_pitch_accent_and_frequency() {
    let Some(mut dict) = open_dictionary() else {
        return;
    };
    assert!(dict.has_pitch(), "pitch.sqlite should be present");
    assert!(dict.has_frequency(), "frequency.sqlite should be present");

    let entries = dict.lookup("学生").expect("lookup");
    assert!(!entries.is_empty());
    let gakusei = &entries[0];
    assert_eq!(gakusei.reading, "がくせい");
    assert!(gakusei.pitch.is_some(), "Kanjium pitch for 学生");
    assert!(
        gakusei.frequency_rank.is_some(),
        "novel-corpus rank for 学生"
    );
    assert!(
        gakusei.glosses.iter().any(|g| g.contains("student")),
        "gloss: {:?}",
        gakusei.glosses
    );

    // Conjugated surface → dictionary form → same enrichment.
    let entries = dict.resolve("食べました").expect("resolve");
    assert_eq!(entries[0].term, "食べる");
    assert!(entries[0].pitch.is_some(), "pitch for 食べる");
    assert!(entries[0].frequency_rank.is_some(), "rank for 食べる");
}

#[test]
fn warm_lookups_and_resolves_stay_under_five_millis() {
    let Some(mut dict) = open_dictionary() else {
        return;
    };

    // Cold pass populates caches (statement + LRU).
    dict.lookup("猫").expect("cold lookup");

    let start = Instant::now();
    for _ in 0..1_000 {
        dict.lookup("猫").expect("warm lookup");
    }
    let per_lookup = start.elapsed() / 1_000;
    assert!(
        per_lookup < Duration::from_millis(5),
        "warm lookup took {per_lookup:?}, budget is 5 ms"
    );

    // resolve() runs de-inflection on top; still must meet the budget warm.
    dict.resolve("食べました").expect("cold resolve");
    let start = Instant::now();
    for _ in 0..100 {
        dict.resolve("食べました").expect("warm resolve");
    }
    let per_resolve = start.elapsed() / 100;
    assert!(
        per_resolve < Duration::from_millis(5),
        "warm resolve took {per_resolve:?}, budget is 5 ms"
    );

    eprintln!("M3 timing: warm lookup {per_lookup:?}, warm resolve {per_resolve:?}");
}
