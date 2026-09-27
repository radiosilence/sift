//! The library index: every track in the library, as its file describes it.
//!
//! The files are the source of truth. The index is derived from them by a
//! scan and holds nothing they do not, so it can be deleted and rebuilt at
//! any time. It exists because answering "which albums have no cover" or
//! "what is by this artist" by reading 50,000 files each time is minutes.
//!
//! Queries use beets' syntax and are evaluated here rather than translated
//! to SQL: at this size a full pass is milliseconds, and it keeps the
//! semantics exactly beets' instead of an approximation of them.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;
use rusqlite::{Connection, params};

use crate::meta::{self, Track};

#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error("index: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("{0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("query: {0}")]
    Query(String),
}

/// One indexed file.
#[derive(Debug, Clone)]
pub struct Item {
    pub track: Track,
    pub size: u64,
    /// Modification time in nanoseconds since the epoch; with `size`, what
    /// decides whether a file has to be read again.
    pub mtime: i64,
    /// When the file was first indexed, in seconds since the epoch.
    pub added: i64,
}

/// An album is a directory of items, as beets files them.
#[derive(Debug, Clone)]
pub struct Album {
    pub dir: PathBuf,
    pub items: Vec<Item>,
}

/// What a value is compared as.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Text(String),
    Number(f64),
}

impl Value {
    fn text(&self) -> String {
        match self {
            Value::Text(s) => s.clone(),
            Value::Number(n) if n.fract() == 0.0 => format!("{n:.0}"),
            Value::Number(n) => n.to_string(),
        }
    }
}

const ITEM_FIELDS: &[&str] = &[
    "title",
    "artist",
    "album",
    "albumartist",
    "track",
    "tracktotal",
    "disc",
    "disctotal",
    "year",
    "date",
    "genre",
    "mb_trackid",
    "mb_albumid",
    "format",
    "bitrate",
    "samplerate",
    "bitdepth",
    "length",
    "path",
    "added",
    "filesize",
];

/// Fields a bare term (no `field:`) is matched against, as in beets.
const ITEM_DEFAULT: &[&str] = &["artist", "albumartist", "album", "title", "genre"];
const ALBUM_DEFAULT: &[&str] = &["albumartist", "album", "genre"];

impl Item {
    pub fn field(&self, name: &str) -> Option<Value> {
        let t = &self.track;
        let text = |v: &Option<String>| v.clone().map(Value::Text);
        let num = |v: Option<u32>| v.map(|n| Value::Number(n as f64));
        match name {
            "title" => text(&t.title),
            "artist" => text(&t.artist),
            "album" => text(&t.album),
            "albumartist" => text(&t.album_artist).or_else(|| text(&t.artist)),
            "track" => num(t.track),
            "tracktotal" => num(t.track_total),
            "disc" => num(t.disc),
            "disctotal" => num(t.disc_total),
            "year" => t
                .date
                .as_deref()
                .and_then(|d| d.get(..4))
                .and_then(|y| y.parse::<f64>().ok())
                .map(Value::Number),
            "date" => text(&t.date),
            "genre" => text(&t.genre),
            "mb_trackid" => text(&t.mb_recording_id),
            "mb_albumid" => text(&t.mb_album_id),
            "format" => Some(Value::Text(t.format.clone())),
            "bitrate" => num(t.bitrate),
            "samplerate" => num(t.sample_rate),
            "bitdepth" => num(t.bit_depth.map(u32::from)),
            "length" => Some(Value::Number(t.duration.as_secs_f64())),
            "path" => Some(Value::Text(t.path.to_string_lossy().into_owned())),
            "added" => Some(Value::Number(self.added as f64)),
            "filesize" => Some(Value::Number(self.size as f64)),
            _ => None,
        }
    }
}

impl Album {
    /// Album-level values come from the album's first track, as they are
    /// the same on every track of an album sift filed.
    pub fn field(&self, name: &str) -> Option<Value> {
        match name {
            "path" => Some(Value::Text(self.dir.to_string_lossy().into_owned())),
            "tracks" => Some(Value::Number(self.items.len() as f64)),
            "length" => Some(Value::Number(
                self.items
                    .iter()
                    .map(|i| i.track.duration.as_secs_f64())
                    .sum(),
            )),
            "filesize" => Some(Value::Number(
                self.items.iter().map(|i| i.size as f64).sum(),
            )),
            "added" => self
                .items
                .iter()
                .map(|i| i.added)
                .min()
                .map(|a| Value::Number(a as f64)),
            _ => self.items.first()?.field(name),
        }
    }
}

/// A parsed beets query: alternatives separated by a lone `,`, each a
/// conjunction of terms, plus sort keys.
#[derive(Debug, Clone, Default)]
pub struct Query {
    any_of: Vec<Vec<Term>>,
    pub sort: Vec<(String, bool)>,
}

#[derive(Debug, Clone)]
struct Term {
    negate: bool,
    /// `None` matches the default fields.
    field: Option<String>,
    pattern: Pattern,
}

#[derive(Debug, Clone)]
enum Pattern {
    Substring(String),
    Exact(String),
    Regex(Regex),
    Range(Option<f64>, Option<f64>),
}

impl Query {
    /// Parse the terms beets takes on its command line, one per argument:
    /// `artist:foo`, `field::regex`, `field:=exact`, `year:2000..2010`,
    /// `^term` or `-term` to negate, `field+`/`field-` to sort, and `,` to
    /// start an alternative.
    pub fn parse<S: AsRef<str>>(terms: &[S]) -> Result<Self, LibraryError> {
        let mut query = Query::default();
        let mut current = Vec::new();
        for raw in terms {
            let raw = raw.as_ref();
            if raw == "," {
                query.any_of.push(std::mem::take(&mut current));
                continue;
            }
            if let Some(field) = raw.strip_suffix('+').or_else(|| raw.strip_suffix('-'))
                && ITEM_FIELDS.contains(&field)
            {
                query.sort.push((field.to_string(), raw.ends_with('+')));
                continue;
            }
            let (negate, body) = match raw.strip_prefix('^').or_else(|| raw.strip_prefix('-')) {
                Some(rest) if !rest.is_empty() => (true, rest),
                _ => (false, raw),
            };
            let (field, value) = match body.split_once(':') {
                Some((f, v)) if ITEM_FIELDS.contains(&f) || f == "tracks" => {
                    (Some(f.to_string()), v)
                }
                _ => (None, body),
            };
            let pattern = if let Some(re) = value.strip_prefix(':') {
                Pattern::Regex(
                    Regex::new(re).map_err(|e| LibraryError::Query(format!("{raw}: {e}")))?,
                )
            } else if let Some(exact) = value.strip_prefix('=') {
                Pattern::Exact(exact.to_string())
            } else if let Some((lo, hi)) = value.split_once("..")
                && field.is_some()
                && [lo, hi]
                    .iter()
                    .all(|s| s.is_empty() || s.parse::<f64>().is_ok())
            {
                Pattern::Range(lo.parse().ok(), hi.parse().ok())
            } else {
                Pattern::Substring(value.to_lowercase())
            };
            current.push(Term {
                negate,
                field,
                pattern,
            });
        }
        query.any_of.push(current);
        Ok(query)
    }

    fn matches(&self, get: impl Fn(&str) -> Option<Value>, defaults: &[&str]) -> bool {
        self.any_of.is_empty()
            || self.any_of.iter().any(|terms| {
                terms.iter().all(|t| {
                    let hit = match &t.field {
                        Some(f) => get(f).is_some_and(|v| t.pattern.matches(&v)),
                        None => defaults
                            .iter()
                            .any(|f| get(f).is_some_and(|v| t.pattern.matches(&v))),
                    };
                    hit != t.negate
                })
            })
    }

    pub fn matches_item(&self, item: &Item) -> bool {
        self.matches(|f| item.field(f), ITEM_DEFAULT)
    }

    pub fn matches_album(&self, album: &Album) -> bool {
        self.matches(|f| album.field(f), ALBUM_DEFAULT)
    }
}

impl Pattern {
    fn matches(&self, v: &Value) -> bool {
        match self {
            Pattern::Substring(s) => v.text().to_lowercase().contains(s),
            Pattern::Exact(s) => v.text() == *s,
            Pattern::Regex(re) => re.is_match(&v.text()),
            Pattern::Range(lo, hi) => {
                let n = match v {
                    Value::Number(n) => *n,
                    Value::Text(t) => match t.parse() {
                        Ok(n) => n,
                        Err(_) => return false,
                    },
                };
                lo.is_none_or(|lo| n >= lo) && hi.is_none_or(|hi| n <= hi)
            }
        }
    }
}

fn compare(a: Option<Value>, b: Option<Value>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Some(Value::Number(a)), Some(Value::Number(b))) => {
            a.partial_cmp(&b).unwrap_or(Ordering::Equal)
        }
        (Some(a), Some(b)) => a.text().to_lowercase().cmp(&b.text().to_lowercase()),
        (a, b) => a.is_some().cmp(&b.is_some()),
    }
}

/// Beets' default order: by album artist, album, disc and track.
const DEFAULT_SORT: &[&str] = &["albumartist", "album", "disc", "track"];

/// What a scan changed.
#[derive(Debug, Default)]
pub struct ScanReport {
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
    pub unchanged: usize,
    /// Files that are audio by extension but could not be read, with why.
    pub failed: Vec<(PathBuf, String)>,
}

pub struct Library {
    conn: Connection,
    workers: usize,
}

/// Files read and committed together during a scan.
const BATCH: usize = 2000;

const UPSERT: &str =
    "INSERT INTO items (path, size, mtime, added, title, artist, album, albumartist,
    track, tracktotal, disc, disctotal, date, genre, mb_trackid, mb_albumid,
    format, bitrate, samplerate, bitdepth, length, original_date, compilation)
 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
    ?17, ?18, ?19, ?20, ?21, ?22, ?23)
 ON CONFLICT(path) DO UPDATE SET size = ?2, mtime = ?3, title = ?5, artist = ?6,
    album = ?7, albumartist = ?8, track = ?9, tracktotal = ?10, disc = ?11,
    disctotal = ?12, date = ?13, genre = ?14, mb_trackid = ?15, mb_albumid = ?16,
    format = ?17, bitrate = ?18, samplerate = ?19, bitdepth = ?20, length = ?21,
    original_date = ?22, compilation = ?23";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS items (
    path TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    added INTEGER NOT NULL,
    title TEXT, artist TEXT, album TEXT, albumartist TEXT,
    track INTEGER, tracktotal INTEGER, disc INTEGER, disctotal INTEGER,
    date TEXT, original_date TEXT, compilation INTEGER NOT NULL DEFAULT 0, genre TEXT, mb_trackid TEXT, mb_albumid TEXT,
    format TEXT NOT NULL, bitrate INTEGER, samplerate INTEGER, bitdepth INTEGER,
    length REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS lyrics_misses (
    path TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS checks (
    path TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    verdict TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS unreadable (
    path TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    error TEXT NOT NULL
);
";

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn mtime_nanos(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or_default()
}

/// Every audio file under `root`, skipping hidden entries (`.part` downloads,
/// macOS `._` resource forks, `.Trash`).
fn audio_files(root: &Path) -> Result<Vec<PathBuf>, LibraryError> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| LibraryError::Io(dir.clone(), e))?;
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                Ok(t) if t.is_file() && meta::is_audio(&path) => out.push(path),
                _ => {}
            }
        }
    }
    Ok(out)
}

impl Library {
    pub fn open(path: &Path) -> Result<Self, LibraryError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| LibraryError::Io(dir.to_path_buf(), e))?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self::with(conn))
    }

    fn with(conn: Connection) -> Self {
        Self {
            conn,
            workers: std::thread::available_parallelism().map_or(4, |n| n.get().min(4)),
        }
    }

    /// How many files a scan reads at once. A tag read is mostly waiting
    /// on the disk, but a file lofty cannot parse can cost tens of MB while
    /// it tries, so a memory-limited caller wants few.
    pub fn with_workers(mut self, workers: usize) -> Self {
        self.workers = workers.max(1);
        self
    }

    pub fn in_memory() -> Result<Self, LibraryError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self::with(conn))
    }

    /// Bring the index in line with the files under `root`: read files that
    /// are new or whose size or modification time changed, and drop rows for
    /// files that are gone. Reads run on every core; a first scan of a large
    /// library is bound by the disk.
    pub fn update(&mut self, root: &Path) -> Result<ScanReport, LibraryError> {
        let known: HashMap<String, (u64, i64)> = {
            let mut stmt = self.conn.prepare("SELECT path, size, mtime FROM items")?;
            stmt.query_map([], |r| {
                Ok((r.get(0)?, (r.get::<_, i64>(1)? as u64, r.get(2)?)))
            })?
            .collect::<Result<_, _>>()?
        };
        // Files that failed before are not read again until they change:
        // a broken file costs a full read each time it is tried.
        let failed: HashMap<String, ((u64, i64), String)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT path, size, mtime, error FROM unreadable")?;
            stmt.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    ((r.get::<_, i64>(1)? as u64, r.get(2)?), r.get(3)?),
                ))
            })?
            .collect::<Result<_, _>>()?
        };
        let mut report = ScanReport::default();
        let mut seen = std::collections::HashSet::new();
        let mut to_read = Vec::new();
        for path in audio_files(root)? {
            let Some(key) = path.to_str().map(str::to_string) else {
                let why = format!("{}: path is not valid UTF-8", path.display());
                report.failed.push((path, why));
                continue;
            };
            let Ok(m) = std::fs::metadata(&path) else {
                continue;
            };
            let stat = (m.len(), mtime_nanos(&m));
            if let Some((was, error)) = failed.get(&key)
                && *was == stat
            {
                report.failed.push((path, error.clone()));
                seen.insert(key);
                continue;
            }
            match known.get(&key) {
                Some(k) if *k == stat => report.unchanged += 1,
                Some(_) => {
                    report.changed += 1;
                    to_read.push((path, stat));
                }
                None => {
                    report.added += 1;
                    to_read.push((path, stat));
                }
            }
            seen.insert(key);
        }

        // Read and commit a batch at a time, so memory stays flat however
        // large the library, and an interrupted first scan keeps what it
        // had read.
        let workers = self.workers;
        let added = now_secs();
        for batch in to_read.chunks(BATCH) {
            let chunk = batch.len().div_ceil(workers).max(1);
            let results: Vec<_> = std::thread::scope(|s| {
                let handles: Vec<_> = batch
                    .chunks(chunk)
                    .map(|part| {
                        s.spawn(move || {
                            part.iter()
                                .map(|(p, stat)| {
                                    (p, *stat, meta::read(p).map_err(|e| e.to_string()))
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .flat_map(|h| h.join().unwrap_or_default())
                    .collect()
            });
            let tx = self.conn.transaction()?;
            {
                let mut upsert = tx.prepare(UPSERT)?;
                for (path, (size, mtime), track) in results {
                    let t = match track {
                        Ok(t) => t,
                        Err(e) => {
                            tx.execute(
                                "INSERT OR REPLACE INTO unreadable (path, size, mtime, error)
                                 VALUES (?1, ?2, ?3, ?4)",
                                params![path.to_str(), size as i64, mtime, e],
                            )?;
                            // What it was before it broke is not what it is.
                            tx.execute("DELETE FROM items WHERE path = ?1", [path.to_str()])?;
                            report.failed.push((path.clone(), e));
                            continue;
                        }
                    };
                    tx.execute("DELETE FROM unreadable WHERE path = ?1", [path.to_str()])?;
                    upsert.execute(params![
                        path.to_str(),
                        size as i64,
                        mtime,
                        added,
                        t.title,
                        t.artist,
                        t.album,
                        t.album_artist,
                        t.track,
                        t.track_total,
                        t.disc,
                        t.disc_total,
                        t.date,
                        t.genre,
                        t.mb_recording_id,
                        t.mb_album_id,
                        t.format,
                        t.bitrate,
                        t.sample_rate,
                        t.bit_depth,
                        t.duration.as_secs_f64(),
                        t.original_date,
                        t.compilation,
                    ])?;
                }
            }
            tx.commit()?;
        }

        let tx = self.conn.transaction()?;
        {
            let root = root.to_string_lossy();
            let mut forget = tx.prepare("DELETE FROM unreadable WHERE path = ?1")?;
            for path in failed.keys() {
                if path.starts_with(root.as_ref()) && !seen.contains(path) {
                    forget.execute([path])?;
                }
            }
            let mut delete = tx.prepare("DELETE FROM items WHERE path = ?1")?;
            for path in known.keys() {
                if path.starts_with(root.as_ref()) && !seen.contains(path) {
                    delete.execute([path])?;
                    report.removed += 1;
                }
            }
        }
        tx.commit()?;
        Ok(report)
    }

    /// Check the audio of `items` that have not been checked since they
    /// last changed, record the results, and return every item's verdict.
    /// A full decode is slow (a large library takes hours), so each result
    /// is kept until the file's size or modification time changes.
    pub fn check(
        &mut self,
        items: &[Item],
    ) -> Result<Vec<(PathBuf, crate::check::Verdict)>, LibraryError> {
        use crate::check::Verdict;
        let known: HashMap<String, ((u64, i64), String)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT path, size, mtime, verdict FROM checks")?;
            stmt.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    ((r.get::<_, i64>(1)? as u64, r.get(2)?), r.get(3)?),
                ))
            })?
            .collect::<Result<_, _>>()?
        };
        let decode = |s: &str| match s.split_once(':') {
            Some(("bad", why)) => Verdict::Bad(why.to_string()),
            Some(("unchecked", why)) => Verdict::Unchecked(why.to_string()),
            _ => Verdict::Ok,
        };
        let encode = |v: &Verdict| match v {
            Verdict::Ok => "ok".to_string(),
            Verdict::Bad(why) => format!("bad:{why}"),
            Verdict::Unchecked(why) => format!("unchecked:{why}"),
        };
        let mut out = Vec::new();
        let mut todo = Vec::new();
        for i in items {
            let key = i.track.path.to_string_lossy().into_owned();
            match known.get(&key) {
                Some((stat, v)) if *stat == (i.size, i.mtime) => {
                    out.push((i.track.path.clone(), decode(v)))
                }
                _ => todo.push(i),
            }
        }
        let workers = self.workers;
        for batch in todo.chunks(BATCH / 10) {
            let chunk = batch.len().div_ceil(workers).max(1);
            let results: Vec<_> = std::thread::scope(|s| {
                let handles: Vec<_> = batch
                    .chunks(chunk)
                    .map(|part| {
                        s.spawn(move || {
                            part.iter()
                                .map(|i| (*i, crate::check::check(&i.track.path)))
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .flat_map(|h| h.join().unwrap_or_default())
                    .collect()
            });
            let tx = self.conn.transaction()?;
            for (i, v) in results {
                tx.execute(
                    "INSERT OR REPLACE INTO checks (path, size, mtime, verdict) VALUES (?1, ?2, ?3, ?4)",
                    params![i.track.path.to_string_lossy(), i.size as i64, i.mtime, encode(&v)],
                )?;
                out.push((i.track.path.clone(), v));
            }
            tx.commit()?;
        }
        Ok(out)
    }

    /// Items lyrics were looked for and not found, unchanged since.
    pub fn lyrics_misses(&self) -> Result<HashMap<String, (u64, i64)>, LibraryError> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, size, mtime FROM lyrics_misses")?;
        Ok(stmt
            .query_map([], |r| {
                Ok((r.get(0)?, (r.get::<_, i64>(1)? as u64, r.get(2)?)))
            })?
            .collect::<Result<_, _>>()?)
    }

    pub fn record_lyrics_miss(&self, item: &Item) -> Result<(), LibraryError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO lyrics_misses (path, size, mtime) VALUES (?1, ?2, ?3)",
            params![
                item.track.path.to_string_lossy(),
                item.size as i64,
                item.mtime
            ],
        )?;
        Ok(())
    }

    /// Drop a file that left the library.
    pub fn forget(&self, path: &Path) -> Result<(), LibraryError> {
        self.conn.execute(
            "DELETE FROM items WHERE path = ?1",
            [path.to_string_lossy()],
        )?;
        Ok(())
    }

    /// Record that a file moved.
    pub fn rename(&self, from: &Path, to: &Path) -> Result<(), LibraryError> {
        self.conn.execute(
            "UPDATE items SET path = ?2 WHERE path = ?1",
            params![from.to_string_lossy(), to.to_string_lossy()],
        )?;
        Ok(())
    }

    fn all(&self) -> Result<Vec<Item>, LibraryError> {
        let mut stmt = self.conn.prepare(
            "SELECT path, size, mtime, added, title, artist, album, albumartist, track,
                tracktotal, disc, disctotal, date, genre, mb_trackid, mb_albumid, format,
                bitrate, samplerate, bitdepth, length, original_date, compilation FROM items",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Item {
                size: r.get::<_, i64>(1)? as u64,
                mtime: r.get(2)?,
                added: r.get(3)?,
                track: Track {
                    path: PathBuf::from(r.get::<_, String>(0)?),
                    title: r.get(4)?,
                    artist: r.get(5)?,
                    album: r.get(6)?,
                    album_artist: r.get(7)?,
                    track: r.get(8)?,
                    track_total: r.get(9)?,
                    disc: r.get(10)?,
                    disc_total: r.get(11)?,
                    date: r.get(12)?,
                    genre: r.get(13)?,
                    mb_recording_id: r.get(14)?,
                    mb_album_id: r.get(15)?,
                    format: r.get(16)?,
                    bitrate: r.get(17)?,
                    sample_rate: r.get(18)?,
                    bit_depth: r.get(19)?,
                    duration: Duration::from_secs_f64(r.get::<_, f64>(20)?.max(0.0)),
                    original_date: r.get(21)?,
                    compilation: r.get(22)?,
                },
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Items matching `query`, in its sort order or beets' default.
    pub fn items(&self, query: &Query) -> Result<Vec<Item>, LibraryError> {
        let mut items: Vec<Item> = self
            .all()?
            .into_iter()
            .filter(|i| query.matches_item(i))
            .collect();
        let keys = sort_keys(query);
        items.sort_by(|a, b| {
            keys.iter()
                .map(|(f, asc)| {
                    let o = compare(a.field(f), b.field(f));
                    if *asc { o } else { o.reverse() }
                })
                .find(|o| o.is_ne())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(items)
    }

    /// Albums matching `query`: directories of items, each item in disc and
    /// track order.
    pub fn albums(&self, query: &Query) -> Result<Vec<Album>, LibraryError> {
        let mut dirs: BTreeMap<PathBuf, Vec<Item>> = BTreeMap::new();
        for item in self.all()? {
            let dir = item
                .track
                .path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default();
            dirs.entry(dir).or_default().push(item);
        }
        let mut albums: Vec<Album> = dirs
            .into_iter()
            .map(|(dir, mut items)| {
                items.sort_by_key(|i| (i.track.disc, i.track.track));
                Album { dir, items }
            })
            .filter(|a| query.matches_album(a))
            .collect();
        let keys = sort_keys(query);
        albums.sort_by(|a, b| {
            keys.iter()
                .map(|(f, asc)| {
                    let o = compare(a.field(f), b.field(f));
                    if *asc { o } else { o.reverse() }
                })
                .find(|o| o.is_ne())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(albums)
    }
}

fn sort_keys(query: &Query) -> Vec<(String, bool)> {
    if query.sort.is_empty() {
        DEFAULT_SORT.iter().map(|f| (f.to_string(), true)).collect()
    } else {
        query.sort.clone()
    }
}

/// Render beets' `-f` format: `$field` or `${field}`, missing fields empty.
pub fn format(template: &str, get: impl Fn(&str) -> Option<Value>) -> String {
    static FIELD: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"\$\{(\w+)\}|\$(\w+)").expect("static regex"));
    FIELD
        .replace_all(template, |c: &regex::Captures| {
            let name = c.get(1).or(c.get(2)).map_or("", |m| m.as_str());
            get(name).map(|v| v.text()).unwrap_or_default()
        })
        .into_owned()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn item(artist: &str, album: &str, title: &str, year: &str, track: u32) -> Item {
        Item {
            track: Track {
                path: PathBuf::from(format!("/m/{artist}/{album}/{track:02} {title}.flac")),
                title: Some(title.into()),
                artist: Some(artist.into()),
                album: Some(album.into()),
                track: Some(track),
                date: Some(year.into()),
                format: "FLAC".into(),
                ..Default::default()
            },
            size: 1,
            mtime: 0,
            added: 0,
        }
    }

    fn q(terms: &[&str]) -> Query {
        Query::parse(terms).unwrap()
    }

    #[test]
    fn bare_terms_match_the_default_fields_case_insensitively() {
        let i = item("Octo Octa", "Where Are We Going?", "Adrift", "2017", 1);
        assert!(q(&["octa"]).matches_item(&i));
        assert!(q(&["adrift", "where"]).matches_item(&i));
        assert!(!q(&["adrift", "nowhere"]).matches_item(&i));
        assert!(
            !q(&["FLAC"]).matches_item(&i),
            "format is not a default field"
        );
    }

    #[test]
    fn field_terms_regex_exact_and_negation() {
        let i = item("Four Tet", "Rounds", "Hands", "2003", 2);
        assert!(q(&["artist:four"]).matches_item(&i));
        assert!(!q(&["title:four"]).matches_item(&i));
        assert!(q(&["title::^H.nds$"]).matches_item(&i));
        assert!(q(&["album:=Rounds"]).matches_item(&i));
        assert!(!q(&["album:=rounds"]).matches_item(&i));
        assert!(!q(&["^artist:four"]).matches_item(&i));
        assert!(q(&["-artist:burial"]).matches_item(&i));
    }

    #[test]
    fn numeric_ranges_are_inclusive_and_open_ended() {
        let i = item("Four Tet", "Rounds", "Hands", "2003-05-05", 2);
        assert!(q(&["year:2000..2010"]).matches_item(&i));
        assert!(q(&["year:2003..2003"]).matches_item(&i));
        assert!(q(&["year:..2003"]).matches_item(&i));
        assert!(!q(&["year:2004.."]).matches_item(&i));
        assert!(q(&["track:1..2"]).matches_item(&i));
    }

    #[test]
    fn a_lone_comma_separates_alternatives() {
        let a = item("Four Tet", "Rounds", "Hands", "2003", 1);
        let b = item("Burial", "Untrue", "Archangel", "2007", 1);
        let query = q(&["artist:four", ",", "artist:burial"]);
        assert!(query.matches_item(&a) && query.matches_item(&b));
    }

    #[test]
    fn sort_terms_are_not_filters() {
        let query = q(&["year-", "four"]);
        assert_eq!(query.sort, vec![("year".to_string(), false)]);
        assert!(query.matches_item(&item("Four Tet", "Rounds", "Hands", "2003", 1)));
    }

    #[test]
    fn format_substitutes_fields_and_blanks_missing_ones() {
        let i = item("Four Tet", "Rounds", "Hands", "2003", 2);
        assert_eq!(
            format("$artist - ${album} ($year) $nope#$track", |f| i.field(f)),
            "Four Tet - Rounds (2003) #2"
        );
    }

    /// A 16-bit mono WAV of `samples` silent frames: small enough to write
    /// in a test, and real audio to lofty.
    pub(crate) fn wav(path: &Path, samples: u32) {
        let data = samples * 2;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&44_100u32.to_le_bytes());
        b.extend_from_slice(&88_200u32.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data.to_le_bytes());
        b.resize(b.len() + data as usize, 0);
        std::fs::write(path, b).unwrap();
    }

    fn tagged_wav(path: &Path, artist: &str, album: &str, title: &str, n: u32) {
        wav(path, 44_100);
        meta::write(
            path,
            &meta::Tags {
                title: title.into(),
                artist: artist.into(),
                album: album.into(),
                album_artist: artist.into(),
                track: n,
                track_total: 2,
                disc: 1,
                disc_total: 1,
                ..Default::default()
            },
            None,
        )
        .unwrap();
    }

    #[test]
    fn update_indexes_new_files_rereads_changed_ones_and_drops_missing_ones() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("Burial").join("Untrue");
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("01 Archangel.wav");
        let b = dir.join("02 Near Dark.wav");
        tagged_wav(&a, "Burial", "Untrue", "Archangel", 1);
        tagged_wav(&b, "Burial", "Untrue", "Near Dark", 2);
        std::fs::write(dir.join(".01 Archangel.wav.part"), b"x").unwrap();
        std::fs::write(dir.join("cover.jpg"), b"x").unwrap();

        let mut lib = Library::in_memory().unwrap();
        let r = lib.update(root.path()).unwrap();
        assert_eq!((r.added, r.changed, r.removed, r.unchanged), (2, 0, 0, 0));
        let items = lib.items(&Query::default()).unwrap();
        assert_eq!(
            items
                .iter()
                .map(|i| i.track.title.clone().unwrap())
                .collect::<Vec<_>>(),
            ["Archangel", "Near Dark"]
        );
        assert_eq!(lib.albums(&q(&["untrue"])).unwrap()[0].items.len(), 2);

        let r = lib.update(root.path()).unwrap();
        assert_eq!((r.added, r.changed, r.unchanged), (0, 0, 2));

        tagged_wav(&b, "Burial", "Untrue", "Near Dark (edit)", 2);
        std::fs::remove_file(&a).unwrap();
        let r = lib.update(root.path()).unwrap();
        assert_eq!((r.changed, r.removed), (1, 1));
        let items = lib.items(&q(&["title:edit"])).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(lib.items(&Query::default()).unwrap().len(), 1);
    }
}
