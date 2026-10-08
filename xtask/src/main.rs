//! Build helpers: `cargo xtask build-dict` downloads the dictionary sources and
//! compiles the three SQLite databases consumed by `medialingual_native::dict`.
//!
//! Sources and licenses (kept current with the live files; both are redistribution-
//! friendly with attribution):
//!
//! - **JMdict_e** — EDRDG, CC BY-SA 3.0-compatible EDRDG terms.
//!   <http://ftp.edrdg.org/pub/Nihongo/JMdict_e.gz>
//! - **Kanjium** (`mifunetoshiro/kanjium`, CC BY-SA 4.0):
//!   `data/source_files/raw/accents.txt` (pitch accent) and
//!   `data/source_files/raw/novels_freq.txt` (word frequency from 5,000+ novels).
//!
//! Every database is built next to its final path and renamed into place, so an
//! interrupted run never leaves a half-written `.sqlite` file where the app would
//! pick it up. Downloads are cached in the output directory (`--force` refreshes).
//!
//! Default output paths mirror `DictionaryConfig::default()` in `src/config.rs` —
//! keep the two in sync (the integration test `tests/m3_lookup.rs` exercises the
//! round trip).

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use rusqlite::{params, Connection, Statement};

const JMDICT_URL: &str = "http://ftp.edrdg.org/pub/Nihongo/JMdict_e.gz";
const ACCENTS_URL: &str =
    "https://raw.githubusercontent.com/mifunetoshiro/kanjium/master/data/source_files/raw/accents.txt";
const NOVELS_FREQ_URL: &str =
    "https://raw.githubusercontent.com/mifunetoshiro/kanjium/master/data/source_files/raw/novels_freq.txt";

const JMDICT_SCHEMA: &str = "
    PRAGMA journal_mode = OFF;
    PRAGMA synchronous = OFF;
    CREATE TABLE entries (
        id       INTEGER NOT NULL,
        term     TEXT    NOT NULL,
        reading  TEXT    NOT NULL,
        pos      TEXT    NOT NULL,
        gloss    TEXT    NOT NULL,
        sense_no INTEGER NOT NULL
    );
    CREATE INDEX idx_entries_term ON entries(term);
    CREATE INDEX idx_entries_reading ON entries(reading);
";

const PITCH_SCHEMA: &str = "
    PRAGMA journal_mode = OFF;
    PRAGMA synchronous = OFF;
    CREATE TABLE pitch (
        word    TEXT NOT NULL,
        reading TEXT NOT NULL,
        pattern TEXT NOT NULL
    );
    CREATE INDEX idx_pitch_word_reading ON pitch(word, reading);
";

const FREQUENCY_SCHEMA: &str = "
    PRAGMA journal_mode = OFF;
    PRAGMA synchronous = OFF;
    CREATE TABLE frequency (
        word  TEXT    NOT NULL,
        rank  INTEGER NOT NULL,
        count INTEGER NOT NULL
    );
    CREATE INDEX idx_frequency_word ON frequency(word);
";

#[derive(Parser)]
#[command(about = "Build helpers for MediaLingual-Native")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download JMdict + Kanjium data and build the SQLite dictionary databases
    BuildDict {
        /// Output directory, relative to the directory cargo runs from
        #[arg(long, default_value = "assets/dict")]
        out_dir: PathBuf,
        /// Re-download sources even when a cached copy exists
        #[arg(long)]
        force: bool,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::BuildDict { out_dir, force } => build_dict(&out_dir, force),
    }
}

fn build_dict(out_dir: &Path, force: bool) -> Result<()> {
    fs::create_dir_all(out_dir).with_context(|| format!("create {}", out_dir.display()))?;

    let jm_gz = out_dir.join("JMdict_e.gz");
    let accents = out_dir.join("kanjium_accents.txt");
    let novels = out_dir.join("kanjium_novels_freq.txt");
    download(JMDICT_URL, &jm_gz, force)?;
    download(ACCENTS_URL, &accents, force)?;
    download(NOVELS_FREQ_URL, &novels, force)?;

    build_jmdict(&jm_gz, &out_dir.join("jmdict.sqlite"))?;
    build_pitch(&accents, &out_dir.join("pitch.sqlite"))?;
    build_frequency(&novels, &out_dir.join("frequency.sqlite"))?;
    eprintln!("build-dict: all databases written to {}", out_dir.display());
    Ok(())
}

/// Stream a URL to `dest` via a `.part` file; skips when cached unless `force`.
fn download(url: &str, dest: &Path, force: bool) -> Result<()> {
    if dest.exists() && !force {
        eprintln!("build-dict: using cached {}", dest.display());
        return Ok(());
    }
    eprintln!("build-dict: downloading {url}");
    let tmp = dest.with_extension("part");
    let mut response = ureq::get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut reader = response.body_mut().as_reader();
    let mut file = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    std::io::copy(&mut reader, &mut file).with_context(|| format!("download {url}"))?;
    let _ = file.sync_all();
    replace(&tmp, dest)?;
    Ok(())
}

/// Rename `src` onto `dest`, replacing any previous file (Windows-safe order).
fn replace(src: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        fs::remove_file(dest).with_context(|| format!("remove {}", dest.display()))?;
    }
    fs::rename(src, dest).with_context(|| format!("rename {} -> {}", src.display(), dest.display()))
}

/// Read a file that may or may not still be gzip-compressed (the HTTP layer can
/// transparently decode `Content-Encoding: gzip`, so sniff the magic bytes).
fn read_text_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut magic = [0u8; 2];
    let probed = file.read_exact(&mut magic).is_ok();
    let gzipped = probed && magic == [0x1f, 0x8b];
    // The magic-byte probe advanced the cursor — always rewind before decoding.
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("rewind {}", path.display()))?;
    let reader: Box<dyn Read> = if gzipped {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut text = String::new();
    let mut reader = reader;
    reader
        .read_to_string(&mut text)
        .with_context(|| format!("decode {}", path.display()))?;
    Ok(text)
}

// ---------------------------------------------------------------------------
// JMdict
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Sense {
    pos: Vec<String>,
    glosses: Vec<String>,
}

/// Where accumulated text content should go.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    None,
    EntSeq,
    KeB,
    Reb,
    Pos,
    Gloss,
}

/// Collect `<!ENTITY name "value">` declarations from the raw document. JMdict uses
/// DTD entities for every part of speech (`&n;` → `noun (common) …`); quick-xml does
/// not resolve them, so we build the map ourselves and resolve `Event::GeneralRef`.
fn parse_dtd_entities(xml: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut rest = xml;
    while let Some(pos) = rest.find("<!ENTITY") {
        let after = &rest[pos + "<!ENTITY".len()..];
        let mut decl = after.trim_start();
        // Parameter-entity declarations (`<!ENTITY % name …>`) are not useful here.
        if let Some(stripped) = decl.strip_prefix('%') {
            decl = stripped.trim_start();
        }
        let Some(name_end) = decl.find(char::is_whitespace) else {
            break;
        };
        let name = &decl[..name_end];
        let value_part = decl[name_end..].trim_start();
        let Some(open_quote) = value_part.find('"') else {
            rest = after;
            continue;
        };
        let value_part = &value_part[open_quote + 1..];
        let Some(close_quote) = value_part.find('"') else {
            rest = after;
            continue;
        };
        map.insert(name.to_owned(), value_part[..close_quote].to_owned());
        rest = &value_part[close_quote + 1..];
    }
    map
}

/// Resolve one `&name;` reference (called for every `Event::GeneralRef`).
fn resolve_ref(name: &str, entities: &HashMap<String, String>) -> String {
    match name {
        "amp" => "&".to_owned(),
        "lt" => "<".to_owned(),
        "gt" => ">".to_owned(),
        "quot" => "\"".to_owned(),
        "apos" => "'".to_owned(),
        _ => {
            if let Some(number) = name.strip_prefix('#') {
                let code = match number.strip_prefix('x') {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => number.parse().ok(),
                };
                if let Some(c) = code.and_then(char::from_u32) {
                    return c.to_string();
                }
                return format!("&{name};");
            }
            // DTD entity → its definition; unknown entity → bare name (e.g. `n`),
            // which is what we want to store for POS anyway.
            entities
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.to_owned())
        }
    }
}

fn build_jmdict(source_gz: &Path, dest: &Path) -> Result<()> {
    eprintln!("build-dict: decoding {}", source_gz.display());
    let xml = read_text_file(source_gz)?;
    eprintln!(
        "build-dict: parsing JMdict ({} MiB uncompressed)",
        xml.len() / (1024 * 1024)
    );
    let entities = parse_dtd_entities(&xml);
    eprintln!("build-dict: {} DTD entities resolved", entities.len());

    let tmp = dest.with_extension("build");
    if tmp.exists() {
        fs::remove_file(&tmp).with_context(|| format!("remove {}", tmp.display()))?;
    }
    let mut db = Connection::open(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    db.execute_batch(JMDICT_SCHEMA).context("jmdict schema")?;

    let (entries, rows) = {
        let tx = db.transaction().context("jmdict transaction")?;
        let stats = {
            let mut stmt = tx
                .prepare("INSERT INTO entries VALUES (?1, ?2, ?3, ?4, ?5, ?6)")
                .context("jmdict insert stmt")?;
            parse_jmdict(&xml, &entities, &mut stmt)?
        };
        tx.commit().context("jmdict commit")?;
        stats
    };
    drop(db);
    replace(&tmp, dest)?;
    eprintln!("build-dict: jmdict.sqlite — {entries} entries, {rows} rows");
    Ok(())
}

/// Stream-parse the JMdict XML into `stmt`; returns (entries, rows) written.
fn parse_jmdict(
    xml: &str,
    entities: &HashMap<String, String>,
    stmt: &mut Statement<'_>,
) -> Result<(usize, usize)> {
    let mut reader = Reader::from_str(xml);
    let mut buf = String::new(); // current text accumulation
    let mut target = Target::None;

    let mut ent_seq: i64 = 0;
    let mut kebs: Vec<String> = Vec::new();
    let mut rebs: Vec<String> = Vec::new();
    let mut senses: Vec<Sense> = Vec::new();
    let mut entries = 0usize;
    let mut rows = 0usize;

    loop {
        let event = reader.read_event().context("jmdict xml parse")?;
        match event {
            Event::Eof => break,
            Event::Start(e) => {
                target = Target::None;
                match e.name().0 {
                    "entry" => {
                        ent_seq = 0;
                        kebs.clear();
                        rebs.clear();
                        senses.clear();
                        // Older JMdict revisions carried ent_seq as an attribute;
                        // Rev 1.09+ makes it a child element (handled below). Support
                        // both so a format flip doesn't silently zero the ids again.
                        for attr in e.attributes() {
                            let attr = attr.context("jmdict attribute")?;
                            if attr.key.0 == "ent_seq" {
                                ent_seq = attr
                                    .value
                                    .parse()
                                    .with_context(|| format!("ent_seq {}", attr.value))?;
                            }
                        }
                    }
                    "ent_seq" => {
                        buf.clear();
                        target = Target::EntSeq;
                    }
                    "keb" => {
                        buf.clear();
                        target = Target::KeB;
                    }
                    "reb" => {
                        buf.clear();
                        target = Target::Reb;
                    }
                    "sense" => senses.push(Sense::default()),
                    "pos" => {
                        buf.clear();
                        target = Target::Pos;
                    }
                    "gloss" => {
                        // Skip non-English glosses (multilingual JMdict variants).
                        let english = e.attributes().try_fold(true, |ok, attr| {
                            let attr = attr.context("gloss attribute")?;
                            Ok::<bool, anyhow::Error>(
                                ok && !(attr.key.0 == "xml:lang" && attr.value != "eng"),
                            )
                        })?;
                        if english {
                            buf.clear();
                            target = Target::Gloss;
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(t) => {
                if target != Target::None {
                    let piece = t.into_inner();
                    buf.push_str(&piece);
                }
            }
            Event::GeneralRef(r) => {
                if target != Target::None {
                    let name = r.into_inner();
                    let resolved = resolve_ref(&name, entities);
                    buf.push_str(&resolved);
                }
            }
            Event::End(e) => match e.name().0 {
                "ent_seq" => {
                    let value = std::mem::take(&mut buf);
                    ent_seq = value
                        .trim()
                        .parse()
                        .with_context(|| format!("ent_seq {value}"))?;
                    target = Target::None;
                }
                "keb" => {
                    let value = std::mem::take(&mut buf);
                    if !value.is_empty() {
                        kebs.push(value);
                    }
                    target = Target::None;
                }
                "reb" => {
                    let value = std::mem::take(&mut buf);
                    if !value.is_empty() {
                        rebs.push(value);
                    }
                    target = Target::None;
                }
                "pos" => {
                    let value = std::mem::take(&mut buf);
                    if !value.is_empty() {
                        if let Some(sense) = senses.last_mut() {
                            sense.pos.push(value);
                        }
                    }
                    target = Target::None;
                }
                "gloss" => {
                    if target == Target::Gloss {
                        let value = std::mem::take(&mut buf).trim().to_owned();
                        if !value.is_empty() {
                            if let Some(sense) = senses.last_mut() {
                                sense.glosses.push(value);
                            }
                        }
                    }
                    target = Target::None;
                }
                "entry" => {
                    entries += 1;
                    rows += insert_entry(stmt, ent_seq, &kebs, &rebs, &senses)?;
                    target = Target::None;
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok((entries, rows))
}

/// One row per gloss, for every (kanji form × reading) pair of the entry. `sense_no`
/// encodes (sense index, gloss index) so `ORDER BY id, sense_no` is deterministic.
fn insert_entry(
    stmt: &mut Statement<'_>,
    ent_seq: i64,
    kebs: &[String],
    rebs: &[String],
    senses: &[Sense],
) -> Result<usize> {
    if rebs.is_empty() || senses.is_empty() {
        return Ok(0);
    }
    // One display form per kanji variant; kana-only entries use their first reading.
    let terms: Vec<&str> = if kebs.is_empty() {
        vec![rebs[0].as_str()]
    } else {
        kebs.iter().map(String::as_str).collect()
    };
    let mut rows = 0usize;
    for term in terms {
        for reading in rebs {
            for (sense_index, sense) in senses.iter().enumerate() {
                if sense.glosses.is_empty() {
                    continue;
                }
                let mut pos = String::new();
                for (pos_index, part) in sense.pos.iter().enumerate() {
                    if pos_index > 0 {
                        pos.push(';');
                    }
                    pos.push_str(part);
                }
                for (gloss_index, gloss) in sense.glosses.iter().enumerate() {
                    stmt.execute(params![
                        ent_seq,
                        term,
                        reading,
                        pos,
                        gloss,
                        (sense_index * 100 + gloss_index) as i64
                    ])?;
                    rows += 1;
                }
            }
        }
    }
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Pitch accent (Kanjium accents.txt: word \t reading \t mora-drop patterns)
// ---------------------------------------------------------------------------

fn build_pitch(source: &Path, dest: &Path) -> Result<()> {
    eprintln!("build-dict: parsing {}", source.display());
    let text = read_text_file(source)?;
    let tmp = dest.with_extension("build");
    if tmp.exists() {
        fs::remove_file(&tmp).with_context(|| format!("remove {}", tmp.display()))?;
    }
    let mut db = Connection::open(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    db.execute_batch(PITCH_SCHEMA).context("pitch schema")?;

    let mut rows = 0usize;
    {
        let tx = db.transaction().context("pitch transaction")?;
        {
            let mut stmt = tx
                .prepare("INSERT INTO pitch VALUES (?1, ?2, ?3)")
                .context("pitch insert stmt")?;
            for line in text.lines() {
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let mut fields = line.split('\t');
                let (Some(word), Some(reading), Some(pattern)) =
                    (fields.next(), fields.next(), fields.next())
                else {
                    continue;
                };
                stmt.execute(params![word, reading, pattern])?;
                rows += 1;
            }
        }
        tx.commit().context("pitch commit")?;
    }
    drop(db);
    replace(&tmp, dest)?;
    eprintln!("build-dict: pitch.sqlite — {rows} rows");
    Ok(())
}

// ---------------------------------------------------------------------------
// Frequency (Kanjium novels_freq.txt: word \t count, ranked by count)
// ---------------------------------------------------------------------------

fn build_frequency(source: &Path, dest: &Path) -> Result<()> {
    eprintln!("build-dict: parsing {}", source.display());
    let text = read_text_file(source)?;
    let mut counts: Vec<(String, i64)> = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split('\t');
        let (Some(word), Some(count)) = (fields.next(), fields.next()) else {
            continue;
        };
        let Ok(count) = count.trim().parse::<i64>() else {
            continue;
        };
        counts.push((word.to_owned(), count));
    }
    // Most frequent first; stable sort keeps file order for ties (deterministic ranks).
    counts.sort_by_key(|&(_, count)| std::cmp::Reverse(count));

    let tmp = dest.with_extension("build");
    if tmp.exists() {
        fs::remove_file(&tmp).with_context(|| format!("remove {}", tmp.display()))?;
    }
    let mut db = Connection::open(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    db.execute_batch(FREQUENCY_SCHEMA)
        .context("frequency schema")?;

    {
        let tx = db.transaction().context("frequency transaction")?;
        {
            let mut stmt = tx
                .prepare("INSERT INTO frequency VALUES (?1, ?2, ?3)")
                .context("frequency insert stmt")?;
            for (rank, (word, count)) in counts.iter().enumerate() {
                stmt.execute(params![word, (rank + 1) as i64, count])?;
            }
        }
        tx.commit().context("frequency commit")?;
    }
    drop(db);
    replace(&tmp, dest)?;
    eprintln!(
        "build-dict: frequency.sqlite — {} words ranked",
        counts.len()
    );
    Ok(())
}
