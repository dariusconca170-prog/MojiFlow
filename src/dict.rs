//! Dictionary lookups: three SQLite databases (JMdict glosses, Kanjium pitch accent,
//! word frequency) behind prepared statements plus an LRU cache.
//!
//! Built by `cargo xtask build-dict` (see `xtask/`); the databases are gitignored build
//! artifacts. A missing pitch/frequency DB degrades gracefully (lookups just lack those
//! fields) — only a missing JMdict DB is a hard, typed error telling the user to run the
//! build command.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::Path;

use rusqlite::Connection;

use crate::config::DictionaryConfig;
use crate::deinflect::{deinflect, Candidate};
use crate::error::DictionaryError;

/// One JMdict entry, senses merged, with pitch/frequency attached when known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictEntry {
    /// Primary display form (kanji form, or the reading for kana-only entries).
    pub term: String,
    /// Kana reading.
    pub reading: String,
    /// Parts of speech (JMdict `pos` values, deduplicated, e.g. `["n", "vs"]`).
    pub pos: Vec<String>,
    /// All glosses across senses, in JMdict order.
    pub glosses: Vec<String>,
    /// Pitch accent pattern from Kanjium (e.g. `["0"]` or `["1", "3"]`), if present.
    pub pitch: Option<String>,
    /// Corpus frequency rank (1 = most common), if present.
    pub frequency_rank: Option<i64>,
}

/// A surface form plus the de-inflection candidate that matched it and the entries found.
///
/// Returned by [`Dictionary::resolve_detailed`] so the M4 popover can show *why* a surface
/// resolved (e.g. 読んだ → 読む via `past`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// The dictionary-form candidate whose lookup succeeded (the surface itself when it is
    /// already a dictionary form, with an empty `reasons` chain).
    pub candidate: Candidate,
    /// Dictionary entries for `candidate.term`.
    pub entries: Vec<DictEntry>,
}

/// Lookup engine. Not `Sync` (rusqlite connections are per-thread); construct one per
/// thread that needs it. Construction is cheap — all heavy work is on first query.
pub struct Dictionary {
    jmdict: Connection,
    pitch: Option<Connection>,
    frequency: Option<Connection>,
    cache: lru::LruCache<String, Vec<DictEntry>>,
}

fn open_required(path: &Path) -> Result<Connection, DictionaryError> {
    if !path.exists() {
        return Err(DictionaryError::MissingDatabase {
            path: path.to_path_buf(),
        });
    }
    Connection::open(path).map_err(|source| DictionaryError::Open {
        path: path.to_path_buf(),
        source,
    })
}

fn open_optional(path: &Path) -> Result<Option<Connection>, DictionaryError> {
    if !path.exists() {
        return Ok(None);
    }
    Connection::open(path)
        .map(Some)
        .map_err(|source| DictionaryError::Open {
            path: path.to_path_buf(),
            source,
        })
}

impl Dictionary {
    /// Open the databases from config. `jmdict.sqlite` is required; pitch and frequency
    /// are optional (lookups degrade to `None` fields when absent).
    pub fn open(config: &DictionaryConfig) -> Result<Self, DictionaryError> {
        let jmdict = open_required(&config.jmdict_path)?;
        let pitch = open_optional(&config.pitch_path)?;
        let frequency = open_optional(&config.frequency_path)?;
        // Config validation requires lru_capacity >= 1; clamp defensively instead of
        // panicking if a caller bypassed validation.
        let capacity = NonZeroUsize::new(config.lru_capacity).unwrap_or(NonZeroUsize::MIN);
        Ok(Self {
            jmdict,
            pitch,
            frequency,
            cache: lru::LruCache::new(capacity),
        })
    }

    /// Exact lookup of `term` (matches the primary form or the reading), senses merged.
    /// Results are cached — repeated lookups are pure cache hits (the <5 ms warm gate).
    pub fn lookup(&mut self, term: &str) -> Result<Vec<DictEntry>, DictionaryError> {
        if term.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(hit) = self.cache.get(term) {
            return Ok(hit.clone());
        }

        // Prepared statements are cached per-connection by rusqlite (prepare_cached).
        let rows = {
            let mut stmt = self
                .jmdict
                .prepare_cached(
                    "SELECT id, term, reading, pos, gloss, sense_no \
                     FROM entries WHERE term = ?1 OR reading = ?1 \
                     ORDER BY id, sense_no",
                )
                .map_err(DictionaryError::Query)?;
            let rows = stmt
                .query_map([term], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })
                .map_err(DictionaryError::Query)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(DictionaryError::Query)?);
            }
            out
        };

        let entries = merge_rows(rows, &self.pitch, &self.frequency)?;
        self.cache.put(term.to_owned(), entries.clone());
        Ok(entries)
    }

    /// Resolve a surface form: try it verbatim, then every de-inflection candidate in
    /// order, returning the first candidate that has dictionary entries. This is the
    /// path the M3 integration gate uses.
    pub fn resolve(&mut self, surface: &str) -> Result<Vec<DictEntry>, DictionaryError> {
        Ok(self
            .resolve_detailed(surface)?
            .map(|resolution| resolution.entries)
            .unwrap_or_default())
    }

    /// Like [`Self::resolve`] but also returns which candidate matched and its de-inflection
    /// chain — the M4 popover uses this to explain conjugated surfaces.
    pub fn resolve_detailed(
        &mut self,
        surface: &str,
    ) -> Result<Option<Resolution>, DictionaryError> {
        for candidate in deinflect(surface) {
            let entries = self.lookup(&candidate.term)?;
            if !entries.is_empty() {
                return Ok(Some(Resolution { candidate, entries }));
            }
        }
        Ok(None)
    }

    /// True when the given databases exist (used by the UI status strip).
    pub fn has_pitch(&self) -> bool {
        self.pitch.is_some()
    }

    pub fn has_frequency(&self) -> bool {
        self.frequency.is_some()
    }
}

/// Group flat (id, term, reading, pos, gloss) rows — one per sense — into entries, and
/// attach pitch accent + frequency rank with reading-aware fallbacks.
fn merge_rows(
    rows: Vec<(i64, String, String, String, String)>,
    pitch: &Option<Connection>,
    frequency: &Option<Connection>,
) -> Result<Vec<DictEntry>, DictionaryError> {
    let mut grouped: HashMap<(i64, String), DictEntry> = HashMap::new();
    let mut order: Vec<(i64, String)> = Vec::new();
    for (id, term, reading, pos, gloss) in rows {
        // Group by (entry id, primary form): the same entry can surface under several
        // kanji forms (行く/行う…), each kept as its own displayable entry.
        let key = (id, term.clone());
        let entry = grouped.entry(key).or_insert_with(|| {
            order.push((id, term.clone()));
            DictEntry {
                term,
                reading,
                pos: Vec::new(),
                glosses: Vec::new(),
                pitch: None,
                frequency_rank: None,
            }
        });
        for pos in pos.split(';').filter(|p| !p.is_empty()) {
            if !entry.pos.iter().any(|existing| existing == pos) {
                entry.pos.push(pos.to_owned());
            }
        }
        entry.glosses.push(gloss);
    }

    let mut entries: Vec<DictEntry> = order
        .into_iter()
        .filter_map(|key| grouped.remove(&key))
        .collect();

    if let Some(conn) = pitch {
        for entry in &mut entries {
            entry.pitch = query_pitch(conn, &entry.term, &entry.reading)?;
        }
    }
    if let Some(conn) = frequency {
        for entry in &mut entries {
            entry.frequency_rank = query_frequency(conn, &entry.term)?;
            if entry.frequency_rank.is_none() {
                entry.frequency_rank = query_frequency(conn, &entry.reading)?;
            }
        }
    }
    Ok(entries)
}

fn query_pitch(
    conn: &Connection,
    term: &str,
    reading: &str,
) -> Result<Option<String>, DictionaryError> {
    let by_pair = {
        let mut stmt = conn
            .prepare_cached("SELECT pattern FROM pitch WHERE word = ?1 AND reading = ?2 LIMIT 1")
            .map_err(DictionaryError::Query)?;
        let mut rows = stmt
            .query_map(rusqlite::params![term, reading], |row| {
                row.get::<_, String>(0)
            })
            .map_err(DictionaryError::Query)?;
        match rows.next() {
            Some(row) => Some(row.map_err(DictionaryError::Query)?),
            None => None,
        }
    };
    if by_pair.is_some() {
        return Ok(by_pair);
    }
    // Fallback: pitch keyed by word alone (some rows only carry the word).
    let mut stmt = conn
        .prepare_cached("SELECT pattern FROM pitch WHERE word = ?1 LIMIT 1")
        .map_err(DictionaryError::Query)?;
    let mut rows = stmt
        .query_map([term], |row| row.get::<_, String>(0))
        .map_err(DictionaryError::Query)?;
    match rows.next() {
        Some(row) => Ok(Some(row.map_err(DictionaryError::Query)?)),
        None => Ok(None),
    }
}

fn query_frequency(conn: &Connection, word: &str) -> Result<Option<i64>, DictionaryError> {
    let mut stmt = conn
        .prepare_cached("SELECT rank FROM frequency WHERE word = ?1 LIMIT 1")
        .map_err(DictionaryError::Query)?;
    let mut rows = stmt
        .query_map([word], |row| row.get::<_, i64>(0))
        .map_err(DictionaryError::Query)?;
    match rows.next() {
        Some(row) => Ok(Some(row.map_err(DictionaryError::Query)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// In-memory fixture with the production schema — keeps these unit tests
    /// independent of the downloaded databases (the real-DB gate lives in
    /// `tests/m3_lookup.rs`).
    fn fixture() -> Dictionary {
        let jmdict = Connection::open_in_memory().expect("jmdict");
        jmdict
            .execute_batch(
                "CREATE TABLE entries (
                    id INTEGER NOT NULL,
                    term TEXT NOT NULL,
                    reading TEXT NOT NULL,
                    pos TEXT NOT NULL,
                    gloss TEXT NOT NULL,
                    sense_no INTEGER NOT NULL
                );
                CREATE INDEX idx_entries_term ON entries(term);
                CREATE INDEX idx_entries_reading ON entries(reading);
                INSERT INTO entries VALUES (1, '食べる', 'たべる', 'v1', 'to eat', 1);
                INSERT INTO entries VALUES (2, '猫', 'ねこ', 'n', 'cat', 1);
                INSERT INTO entries VALUES (2, '猫', 'ねこ', 'n', 'domestic cat', 2);
                INSERT INTO entries VALUES (3, 'する', 'する', 'vs', 'to do', 1);
                INSERT INTO entries VALUES (4, '学生', 'がくせい', 'n', 'student', 1);
                INSERT INTO entries VALUES (5, '本', 'ほん', 'n', 'book', 1);
                INSERT INTO entries VALUES (5, '本', 'ほん', 'n', 'origin', 2);",
            )
            .expect("jmdict rows");

        let pitch = Connection::open_in_memory().expect("pitch");
        pitch
            .execute_batch(
                "CREATE TABLE pitch (word TEXT NOT NULL, reading TEXT NOT NULL, pattern TEXT NOT NULL);
                 INSERT INTO pitch VALUES ('猫', 'ねこ', '1');
                 INSERT INTO pitch VALUES ('学生', 'がくせい', '0');",
            )
            .expect("pitch rows");

        let frequency = Connection::open_in_memory().expect("frequency");
        frequency
            .execute_batch(
                "CREATE TABLE frequency (word TEXT NOT NULL, rank INTEGER NOT NULL);
                 INSERT INTO frequency VALUES ('猫', 1500);
                 INSERT INTO frequency VALUES ('学生', 800);",
            )
            .expect("frequency rows");

        Dictionary {
            jmdict,
            pitch: Some(pitch),
            frequency: Some(frequency),
            cache: lru::LruCache::new(NonZeroUsize::new(16).expect("nonzero")),
        }
    }

    #[test]
    fn merges_senses_and_deduplicates_pos() {
        let mut d = fixture();
        let entries = d.lookup("猫").expect("lookup");
        assert_eq!(entries.len(), 1, "{entries:?}");
        let neko = &entries[0];
        assert_eq!(neko.term, "猫");
        assert_eq!(neko.reading, "ねこ");
        assert_eq!(neko.glosses, vec!["cat", "domestic cat"]);
        assert_eq!(neko.pos, vec!["n"]);
        assert_eq!(neko.pitch.as_deref(), Some("1"), "pitch attached");
        assert_eq!(neko.frequency_rank, Some(1500), "frequency attached");
    }

    #[test]
    fn matches_by_reading_too() {
        let mut d = fixture();
        let entries = d.lookup("たべる").expect("lookup");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].term, "食べる");
        assert_eq!(entries[0].glosses, vec!["to eat"]);
        assert!(entries[0].pitch.is_none(), "no pitch row for たべる");
        assert!(entries[0].frequency_rank.is_none());
    }

    #[test]
    fn unknown_terms_return_empty_not_error() {
        let mut d = fixture();
        assert!(d.lookup("存在しない語").expect("lookup").is_empty());
        assert!(d.lookup("").expect("empty").is_empty());
    }

    #[test]
    fn resolve_deinflects_to_dictionary_form() {
        let mut d = fixture();
        let entries = d.resolve("食べました").expect("resolve");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].term, "食べる");
        let entries = d.resolve("猫").expect("resolve");
        assert_eq!(entries[0].term, "猫");
        assert!(d.resolve("zzz-unknown").expect("resolve").is_empty());
    }

    #[test]
    fn resolve_detailed_reports_the_deinflection_chain() {
        let mut d = fixture();
        let resolution = d
            .resolve_detailed("食べました")
            .expect("resolve")
            .expect("entry");
        assert_eq!(resolution.candidate.term, "食べる");
        assert!(
            !resolution.candidate.reasons.is_empty(),
            "conjugated surface must carry a reason chain"
        );
        assert_eq!(resolution.entries[0].term, "食べる");

        // A word that is already a dictionary form has an empty chain.
        let verbatim = d.resolve_detailed("猫").expect("resolve").expect("entry");
        assert_eq!(verbatim.candidate.term, "猫");
        assert!(verbatim.candidate.reasons.is_empty());
    }

    #[test]
    fn warm_lookups_hit_the_cache_and_are_fast() {
        let mut d = fixture();
        d.lookup("猫").expect("cold");
        let start = Instant::now();
        for _ in 0..1_000 {
            d.lookup("猫").expect("warm");
        }
        let per_lookup = start.elapsed() / 1_000;
        assert!(
            per_lookup < std::time::Duration::from_millis(5),
            "warm lookup took {per_lookup:?}"
        );
    }

    #[test]
    fn lru_evicts_when_over_capacity() {
        let mut d = fixture();
        d.cache = lru::LruCache::new(NonZeroUsize::new(2).expect("nonzero"));
        d.lookup("猫").expect("a");
        d.lookup("学生").expect("b");
        d.lookup("本").expect("c"); // evicts 猫 (LRU order)
        assert!(d.cache.get("猫").is_none(), "猫 should have been evicted");
        assert!(d.cache.get("本").is_some());
    }

    #[test]
    fn missing_required_database_is_a_typed_error() {
        let config = DictionaryConfig {
            jmdict_path: std::path::PathBuf::from("/nonexistent/jmdict.sqlite"),
            ..Default::default()
        };
        match Dictionary::open(&config) {
            Err(DictionaryError::MissingDatabase { path }) => {
                assert!(path.to_string_lossy().contains("jmdict"));
                // The error text must tell the user the build command.
                let rendered = DictionaryError::MissingDatabase { path }.to_string();
                assert!(rendered.contains("cargo xtask build-dict"), "{rendered}");
            }
            other => match other {
                Ok(_) => panic!("expected MissingDatabase, got Ok(Dictionary)"),
                Err(err) => panic!("expected MissingDatabase, got {err}"),
            },
        }
    }

    #[test]
    fn optional_databases_may_be_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let jmdict = dir.path().join("jmdict.sqlite");
        // Create the file with the production schema (empty entries table).
        let conn = Connection::open(&jmdict).expect("create");
        conn.execute_batch("CREATE TABLE entries (id INTEGER, term TEXT, reading TEXT, pos TEXT, gloss TEXT, sense_no INTEGER);")
            .expect("schema");
        conn.close().expect("close");
        let config = DictionaryConfig {
            jmdict_path: jmdict,
            pitch_path: dir.path().join("nope-pitch.sqlite"),
            frequency_path: dir.path().join("nope-freq.sqlite"),
            lru_capacity: 8,
        };
        let mut dict = Dictionary::open(&config).expect("opens without optional dbs");
        assert!(!dict.has_pitch());
        assert!(!dict.has_frequency());
        assert!(dict.lookup("猫").expect("empty db").is_empty());
    }

    #[test]
    fn missing_pitch_db_degrades_to_none_not_error() {
        let mut d = fixture();
        d.pitch = None;
        let entries = d.lookup("猫").expect("lookup without pitch db");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].pitch.is_none());
        assert_eq!(
            entries[0].frequency_rank,
            Some(1500),
            "frequency still attached"
        );
    }
}
