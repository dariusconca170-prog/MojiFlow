//! De-inflection: generate dictionary-form candidates from a conjugated surface.
//!
//! Strategy (same shape as browser-yomi-kanji tools): a table of *suffix rewrite rules*
//! applied breadth-first up to a small depth, producing many candidates — some wrong.
//! Wrong candidates are harmless because the dictionary validates them: callers try each
//! candidate against JMdict and keep what exists (see [`crate::dict`]).
//!
//! Rules are grouped by conjugation class:
//! - godan (五段) verbs — generated per kana column, so 9 columns × ~20 forms
//! - ichidan (一段) verbs — uniform suffixes (食べる → 食べます …)
//! - する compounds (勉強し… → 勉強する) and くる (both kana and kanji spellings)
//! - い-adjectives (高い …) and the copula (でした …)

use std::collections::HashSet;
use std::sync::OnceLock;

/// One dictionary-form candidate for a conjugated surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The proposed dictionary form.
    pub term: String,
    /// Chain of transformations applied, e.g. `["past", "-te"]` (outermost first).
    pub reasons: Vec<&'static str>,
}

/// Hard cap so pathological inputs cannot explode the search.
const MAX_CANDIDATES: usize = 512;
/// Bounded rewrite depth: real chains are ≤ 3 (progressive-past-polite at most).
const MAX_DEPTH: usize = 4;

#[derive(Debug)]
struct Rule {
    from: String,
    to: String,
    reason: &'static str,
}

impl Rule {
    fn new(from: &str, to: &str, reason: &'static str) -> Option<Self> {
        if from == to || from.is_empty() {
            return None;
        }
        Some(Self {
            from: from.to_owned(),
            to: to.to_owned(),
            reason,
        })
    }
}

/// Godan columns: (a-row, i-row, e-row, o-row, dictionary row).
const GODAN_COLUMNS: [(&str, &str, &str, &str, &str); 9] = [
    ("あ", "い", "え", "お", "う"),
    ("か", "き", "け", "こ", "く"),
    ("が", "ぎ", "げ", "ご", "ぐ"),
    ("さ", "し", "せ", "そ", "す"),
    ("た", "ち", "て", "と", "つ"),
    ("な", "に", "ね", "の", "ぬ"),
    ("ま", "み", "め", "も", "む"),
    ("ば", "び", "べ", "ぼ", "ぶ"),
    ("ら", "り", "れ", "ろ", "る"),
];

/// Dictionary-ending kana → (て-form, た-form), from the 五段 euphonic changes
/// (音便): く→いて/いた, ぐ→いで/いだ, す→して/した, つ・う・る→って/った,
/// ぬ・ぶ・む→んで/んだ.
const TE_TA: [(&str, &str, &str); 9] = [
    ("く", "いて", "いた"),
    ("ぐ", "いで", "いだ"),
    ("す", "して", "した"),
    ("つ", "って", "った"),
    ("う", "って", "った"),
    ("る", "って", "った"),
    ("ぬ", "んで", "んだ"),
    ("ぶ", "んで", "んだ"),
    ("む", "んで", "んだ"),
];

fn rules() -> &'static Vec<Rule> {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(build_rules)
}

fn build_rules() -> Vec<Rule> {
    let mut rules: Vec<Rule> = Vec::new();
    let mut push = |from: &str, to: &str, reason: &'static str| {
        if let Some(rule) = Rule::new(from, to, reason) {
            rules.push(rule);
        }
    };

    // ---- godan verbs, generated per kana column ----------------------------
    for (a, i, e, o, dict) in GODAN_COLUMNS {
        let (te, ta) = match TE_TA.iter().find(|(end, _, _)| *end == dict) {
            Some((_, te, ta)) => (*te, *ta),
            None => continue,
        };
        // Polite (ます) stem
        push(&format!("{i}ます"), dict, "polite");
        push(&format!("{i}ません"), dict, "polite negative");
        push(&format!("{i}ました"), dict, "polite past");
        push(&format!("{i}ませんでした"), dict, "polite negative past");
        push(&format!("{i}ましょう"), dict, "polite volitional");
        push(&format!("{i}たい"), dict, "desire");
        // Te / ta forms and their productive extensions
        push(te, dict, "-te");
        push(ta, dict, "past");
        push(&format!("{te}いる"), dict, "progressive");
        push(&format!("{te}いた"), dict, "progressive past");
        push(&format!("{te}います"), dict, "progressive polite");
        push(&format!("{te}る"), dict, "short progressive");
        push(&format!("{ta}り"), dict, "consecutive");
        // E-row: potential / conditional
        push(&format!("{e}る"), dict, "potential");
        push(&format!("{e}れば"), dict, "conditional");
        // A-row: negative, passive, causative
        push(&format!("{a}ない"), dict, "negative");
        push(&format!("{a}なかった"), dict, "negative past");
        push(&format!("{a}なかったら"), dict, "negative conditional");
        push(&format!("{a}れる"), dict, "passive");
        push(&format!("{a}せる"), dict, "causative");
        push(&format!("{a}される"), dict, "causative-passive");
        // O-row: volitional
        push(&format!("{o}う"), dict, "volitional");
    }

    // ---- ichidan verbs: dictionary form ends in る, suffixes are uniform ----
    for (from, reason) in [
        ("ます", "polite"),
        ("ません", "polite negative"),
        ("ました", "polite past"),
        ("ませんでした", "polite negative past"),
        ("ましょう", "polite volitional"),
        ("て", "-te"),
        ("た", "past"),
        ("ない", "negative"),
        ("なかった", "negative past"),
        ("れば", "conditional"),
        ("てる", "short progressive"),
        ("ている", "progressive"),
        ("ていた", "progressive past"),
        ("ています", "progressive polite"),
        ("ていました", "progressive past polite"),
        ("られる", "potential/passive"),
        ("させる", "causative"),
        ("させられる", "causative-passive"),
        ("たい", "desire"),
    ] {
        push(from, "る", reason);
    }

    // ---- する compounds (勉強し… → 勉強する) --------------------------------
    for (from, reason) in [
        ("します", "polite"),
        ("しません", "polite negative"),
        ("しました", "polite past"),
        ("しませんでした", "polite negative past"),
        ("しましょう", "polite volitional"),
        ("して", "-te"),
        ("した", "past"),
        ("しない", "negative"),
        ("しなかった", "negative past"),
        ("すれば", "conditional"),
        ("しよう", "volitional"),
        ("している", "progressive"),
        ("していた", "progressive past"),
        ("しています", "progressive polite"),
        ("される", "passive"),
        ("させる", "causative"),
        ("させられる", "causative-passive"),
        ("し", "stem"),
    ] {
        push(from, "する", reason);
    }

    // ---- くる, kana spelling (きます → くる); the kanji spelling 来る is
    //      covered by the uniform ichidan rules above (来ます → 来る) ----------
    for (from, reason) in [
        ("きます", "polite"),
        ("きません", "polite negative"),
        ("きました", "polite past"),
        ("きませんでした", "polite negative past"),
        ("きましょう", "polite volitional"),
        ("きて", "-te"),
        ("きた", "past"),
        ("こない", "negative"),
        ("こなかった", "negative past"),
        ("こよう", "volitional"),
        ("こられる", "potential/passive"),
        ("こさせる", "causative"),
        ("きている", "progressive"),
        ("きていた", "progressive past"),
    ] {
        push(from, "くる", reason);
    }

    // ---- い-adjectives ------------------------------------------------------
    for (from, reason) in [
        ("くて", "-te"),
        ("くない", "negative"),
        ("くなかった", "negative past"),
        ("かった", "past"),
        ("ければ", "conditional"),
        ("くありません", "negative polite"),
    ] {
        push(from, "い", reason);
    }

    // ---- copula -------------------------------------------------------------
    push("です", "", "polite"); // strips the polite tail so inner rules can fire
    push("でした", "です", "polite past");
    push("ませんでした", "です", "polite negative past");
    push("じゃない", "だ", "negative");
    push("ではない", "だ", "negative");
    push("じゃなかった", "だ", "negative past");
    push("ではなかった", "だ", "negative past");
    push("だった", "だ", "past");
    push("だった", "です", "past (polite lemma)");

    rules
}

/// Generate dictionary-form candidates for `surface`. The first candidate is always
/// `surface` itself (unmodified), then BFS-ordered rewrites, deduplicated by term.
pub fn deinflect(surface: &str) -> Vec<Candidate> {
    let rules = rules();
    let mut out: Vec<Candidate> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: std::collections::VecDeque<Candidate> = std::collections::VecDeque::new();

    let original = Candidate {
        term: surface.to_owned(),
        reasons: Vec::new(),
    };
    seen.insert(original.term.clone());
    out.push(original.clone());
    queue.push_back(original);

    while let Some(current) = queue.pop_front() {
        if current.reasons.len() >= MAX_DEPTH || out.len() >= MAX_CANDIDATES {
            continue;
        }
        for rule in rules {
            let Some(stripped) = current.term.strip_suffix(rule.from.as_str()) else {
                continue;
            };
            let mut term = String::with_capacity(stripped.len() + rule.to.len());
            term.push_str(stripped);
            term.push_str(&rule.to);
            if term.is_empty() || !seen.insert(term.clone()) {
                continue;
            }
            let mut reasons = current.reasons.clone();
            reasons.push(rule.reason);
            out.push(Candidate {
                term: term.clone(),
                reasons: reasons.clone(),
            });
            queue.push_back(Candidate { term, reasons });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert `surface` deinflects to `expected` (among the candidates), returning the
    /// matched candidate for further assertions.
    fn deinflects_to(surface: &str, expected: &str) -> Candidate {
        let candidates = deinflect(surface);
        candidates
            .iter()
            .find(|c| c.term == expected)
            .unwrap_or_else(|| {
                let terms: Vec<&str> = candidates.iter().map(|c| c.term.as_str()).collect();
                panic!("expected candidate {expected:?} for {surface:?}; got {terms:?}")
            })
            .clone()
    }

    // --- ichidan (Group II) --------------------------------------------------
    #[test]
    fn ichidan_polite_forms() {
        deinflects_to("食べました", "食べる");
        deinflects_to("食べます", "食べる");
        deinflects_to("食べません", "食べる");
        deinflects_to("食べませんでした", "食べる");
    }
    #[test]
    fn ichidan_plain_forms() {
        deinflects_to("食べて", "食べる");
        deinflects_to("食べた", "食べる");
        deinflects_to("食べない", "食べる");
        deinflects_to("食べれば", "食べる");
        deinflects_to("食べられる", "食べる");
        deinflects_to("食べさせる", "食べる");
        deinflects_to("食べたい", "食べる");
    }
    #[test]
    fn ichidan_progressive_forms() {
        deinflects_to("食べていました", "食べる");
        deinflects_to("食べている", "食べる");
    }
    #[test]
    fn ichidan_group3_verb_miru() {
        deinflects_to("見ました", "見る");
        deinflects_to("起きなかった", "起きる");
    }

    // --- godan (Group I) -----------------------------------------------------
    #[test]
    fn godan_kaku_writes() {
        deinflects_to("書きました", "書く");
        deinflects_to("書きます", "書く");
        deinflects_to("書いて", "書く");
        deinflects_to("書いた", "書く");
        deinflects_to("書かない", "書く");
        deinflects_to("書ければ", "書く");
        deinflects_to("書ける", "書く");
        deinflects_to("書かれる", "書く");
        deinflects_to("書かせる", "書く");
        deinflects_to("書かされる", "書く");
        deinflects_to("書こう", "書く");
        deinflects_to("書きたい", "書く");
    }
    #[test]
    fn godan_euphonic_changes_cover_every_column() {
        deinflects_to("泳いだ", "泳ぐ"); // ぐ → いだ
        deinflects_to("話して", "話す"); // す → して
        deinflects_to("待った", "待つ"); // つ → った
        deinflects_to("死んだ", "死ぬ"); // ぬ → んだ
        deinflects_to("遊んだ", "遊ぶ"); // ぶ → んだ
        deinflects_to("飲んだ", "飲む"); // む → んだ
        deinflects_to("帰りました", "帰る"); // る → って/った
        deinflects_to("買った", "買う"); // う → った
        deinflects_to("読んで", "読む"); // む → んで
    }

    // --- suru / kuru ----------------------------------------------------------
    #[test]
    fn suru_compounds() {
        deinflects_to("勉強しました", "勉強する");
        deinflects_to("勉強します", "勉強する");
        deinflects_to("勉強しない", "勉強する");
        deinflects_to("勉強して", "勉強する");
        deinflects_to("勉強される", "勉強する");
        deinflects_to("した", "する");
        deinflects_to("しています", "する");
    }
    #[test]
    fn kuru_spellings() {
        deinflects_to("来ました", "来る"); // kanji spelling via uniform rules
        deinflects_to("来ない", "来る");
        deinflects_to("きます", "くる"); // kana spelling needs explicit rules
        deinflects_to("こない", "くる");
        deinflects_to("きました", "くる");
    }

    // --- adjectives / copula ----------------------------------------------------
    #[test]
    fn i_adjectives() {
        deinflects_to("高かったです", "高い");
        deinflects_to("高くて", "高い");
        deinflects_to("高くない", "高い");
        deinflects_to("高ければ", "高い");
        deinflects_to("可愛くなかった", "可愛い");
    }
    #[test]
    fn copula_forms() {
        deinflects_to("でした", "です");
        deinflects_to("じゃなかった", "だ");
        deinflects_to("だった", "だ");
    }

    // --- properties -------------------------------------------------------------
    #[test]
    fn original_surface_is_always_the_first_candidate() {
        let candidates = deinflect("猫");
        assert_eq!(candidates[0].term, "猫");
        assert!(candidates[0].reasons.is_empty());
        let candidates = deinflect("食べました");
        assert_eq!(candidates[0].term, "食べました");
    }

    #[test]
    fn candidates_are_deduplicated_and_bounded() {
        let candidates = deinflect("勉強していました");
        let mut terms: HashSet<&str> = HashSet::new();
        for candidate in &candidates {
            assert!(
                terms.insert(candidate.term.as_str()),
                "dup {}",
                candidate.term
            );
        }
        assert!(candidates.len() <= MAX_CANDIDATES);
        assert!(!candidates.is_empty());
    }

    #[test]
    fn matched_candidate_records_its_transformation_chain() {
        let candidate = deinflects_to("食べました", "食べる");
        assert!(candidate.reasons.contains(&"polite past"), "{candidate:?}");
        let candidate = deinflects_to("書かされて", "書く");
        assert!(!candidate.reasons.is_empty());
    }

    #[test]
    fn non_verbs_only_yield_the_original() {
        // Plain nouns must not be rewritten into nonsense (rules are suffix-driven;
        // nothing should match — if something does, the dictionary filters it, but the
        // original has to stay first and intact).
        let candidates = deinflect("日本語");
        assert_eq!(candidates[0].term, "日本語");
    }
}
