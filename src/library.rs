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

/// A field's value as sorting compares it, worked out once per row rather
/// than on every comparison: two numbers by value, anything else by its
/// text without case, and a missing value first.
struct SortKey {
    number: Option<f64>,
    text: Option<String>,
}

impl SortKey {
    fn of(v: Option<Value>) -> Self {
        Self {
            number: match &v {
                Some(Value::Number(n)) => Some(*n),
                _ => None,
            },
            text: v.map(|v| v.text().to_lowercase()),
        }
    }

    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self.number, other.number, &self.text, &other.text) {
            (Some(a), Some(b), _, _) => a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal),
            (_, _, Some(a), Some(b)) => a.cmp(b),
            _ => self.text.is_some().cmp(&other.text.is_some()),
        }
    }
}

/// Sort `rows` by `keys`, each a field and whether it ascends.
fn sort_by_fields<T>(
    rows: Vec<T>,
    keys: &[(String, bool)],
    field: impl Fn(&T, &str) -> Option<Value>,
) -> Vec<T> {
    let mut keyed: Vec<(Vec<SortKey>, T)> = rows
        .into_iter()
        .map(|r| {
            (
                keys.iter()
                    .map(|(f, _)| SortKey::of(field(&r, f)))
                    .collect(),
                r,
            )
        })
        .collect();
    keyed.sort_by(|(a, _), (b, _)| {
        a.iter()
            .zip(b)
            .zip(keys)
            .map(|((a, b), (_, asc))| {
                let o = a.cmp(b);
                if *asc { o } else { o.reverse() }
            })
            .find(|o| o.is_ne())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    keyed.into_iter().map(|(_, r)| r).collect()
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
    /// Files that are audio by extension but could not be read, and folders
    /// under the root that could not be listed, with why.
    pub failed: Vec<(PathBuf, String)>,
}

pub struct Library {
    conn: Connection,
    workers: usize,
    /// Stamp a newly seen file's `added` with its own modification time
    /// rather than the scan time, as beets' `importadded` plugin.
    import_added: bool,
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

/// How long a scratch copy may sit before it counts as left by an edit
/// that never finished rather than one still running.
const STALE_SCRATCH: Duration = Duration::from_secs(3600);

/// Every audio file under `root`, skipping hidden entries (`.part` downloads,
/// macOS `._` resource forks, `.Trash`), and the directories below `root`
/// that could not be read, with why. A scratch copy left by an interrupted
/// tag write or transfer is removed on the way.
fn audio_files(root: &Path, stale: Duration) -> Result<Walk, LibraryError> {
    let mut out = Vec::new();
    let mut unread = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if dir == root => return Err(LibraryError::Io(dir, e)),
            // Gone since it was listed (an album moved mid-scan), or not ours
            // to read: the rest of the library is still scanned.
            Err(e) => {
                unread.push((dir.clone(), format!("{}: {e}", dir.display())));
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_name().to_string_lossy().starts_with('.') {
                if crate::paths::is_scratch(&path)
                    && entry
                        .metadata()
                        .ok()
                        .and_then(|m| crate::paths::scratch_age(&m))
                        .is_some_and(|age| age > stale)
                {
                    let _ = std::fs::remove_file(&path);
                }
                continue;
            }
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                Ok(t) if t.is_file() && meta::is_audio(&path) => out.push(path),
                _ => {}
            }
        }
    }
    Ok(Walk { files: out, unread })
}

struct Walk {
    files: Vec<PathBuf>,
    unread: Vec<(PathBuf, String)>,
}

/// Rows keyed by a file's path, each of which follows the file when it moves
/// and goes when it leaves.
const PATH_TABLES: [&str; 4] = ["items", "checks", "lyrics_misses", "unreadable"];

/// Move every row for `from` to `to`. Where `from` has a row, it replaces
/// any stale row already at `to`; where it has none, a stale one there is
/// left for the next scan's sweep.
fn rename_rows(conn: &Connection, from: &Path, to: &Path) -> rusqlite::Result<()> {
    for table in PATH_TABLES {
        conn.prepare_cached(&format!(
            "UPDATE OR REPLACE {table} SET path = ?2 WHERE path = ?1"
        ))?
        .execute(params![from.to_string_lossy(), to.to_string_lossy()])?;
    }
    Ok(())
}

/// Drop every row for `path`.
fn forget_rows(conn: &Connection, path: &str) -> rusqlite::Result<()> {
    for table in PATH_TABLES {
        conn.prepare_cached(&format!("DELETE FROM {table} WHERE path = ?1"))?
            .execute([path])?;
    }
    Ok(())
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
            import_added: false,
        }
    }

    /// How many files a scan reads at once. A tag read is mostly waiting
    /// on the disk, but a file lofty cannot parse can cost tens of MB while
    /// it tries, so a memory-limited caller wants few.
    pub fn with_workers(mut self, workers: usize) -> Self {
        self.workers = workers.max(1);
        self
    }

    /// When set, a file's `added` is its own modification time rather than
    /// the moment the scan found it, as beets' `importadded` plugin.
    pub fn with_import_added(mut self, on: bool) -> Self {
        self.import_added = on;
        self
    }

    pub fn in_memory() -> Result<Self, LibraryError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self::with(conn))
    }

    /// Bring the index in line with the files under `root`: read files that
    /// are new or whose size or modification time changed, and drop rows for
    /// files that are gone. Scratch copies left under `root` by interrupted
    /// writes are removed. Reads run on every core; a first scan of a large
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
        let Walk { files, unread } = audio_files(root, STALE_SCRATCH)?;
        report.failed.extend(unread.iter().cloned());
        for path in files {
            let Some(key) = path.to_str().map(str::to_string) else {
                let why = format!("{}: path is not valid UTF-8", path.display());
                report.failed.push((path, why));
                continue;
            };
            // Listed but not stat'd this time (a NAS hiccup): still there,
            // so nothing recorded for it is dropped.
            let Ok(m) = std::fs::metadata(&path) else {
                seen.insert(key);
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
                    to_read.push((path, stat, false));
                }
                None => {
                    report.added += 1;
                    to_read.push((path, stat, true));
                }
            }
            seen.insert(key);
        }

        self.read_into(&to_read, &mut report)?;

        // A file under `root` that the walk did not see has left, unless it
        // is under a directory that could not be read this time.
        let gone = |path: &str| {
            let p = Path::new(path);
            p.starts_with(root)
                && !seen.contains(path)
                && !unread.iter().any(|(dir, _)| p.starts_with(dir))
        };
        let tx = self.conn.transaction()?;
        for path in failed.keys() {
            if gone(path) {
                forget_rows(&tx, path)?;
            }
        }
        for path in known.keys() {
            if gone(path) {
                forget_rows(&tx, path)?;
                report.removed += 1;
            }
        }
        // A verdict or a miss is only ever recorded for an indexed file; one
        // whose file has left the index, however, goes with it.
        for table in ["checks", "lyrics_misses"] {
            tx.execute(
                &format!(
                    "DELETE FROM {table} WHERE NOT EXISTS
                     (SELECT 1 FROM items i WHERE i.path = {table}.path)"
                ),
                [],
            )?;
        }
        tx.commit()?;
        Ok(report)
    }

    /// Read each `(file, (size, mtime), is new)` and record what it holds,
    /// or why it could not be read. A batch at a time is read and
    /// committed, so memory stays flat however large the library, and an
    /// interrupted first scan keeps what it had read.
    fn read_into(
        &mut self,
        to_read: &[(PathBuf, (u64, i64), bool)],
        report: &mut ScanReport,
    ) -> Result<(), LibraryError> {
        let workers = self.workers;
        let added = now_secs();
        for batch in to_read.chunks(BATCH) {
            let read = crate::par::map(batch, workers, |(p, _, _)| {
                meta::read(p).map_err(|e| e.to_string())
            });
            // `meta::read` contains its own panics; one that escapes it is
            // left unrecorded, to be read again next time.
            let results = batch
                .iter()
                .zip(read)
                .filter_map(|((p, stat, is_new), r)| Some((p, *stat, *is_new, r.ok()?)));
            let tx = self.conn.transaction()?;
            {
                let mut upsert = tx.prepare(UPSERT)?;
                for (path, (size, mtime), is_new, track) in results {
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
                    let row_added = if is_new && self.import_added {
                        mtime / 1_000_000_000
                    } else {
                        added
                    };
                    upsert.execute(params![
                        path.to_str(),
                        size as i64,
                        mtime,
                        row_added,
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

        Ok(())
    }

    /// Read each of `paths` again after sift changed them, without walking
    /// the library. A path no longer there is dropped.
    pub fn refresh(&mut self, paths: &[PathBuf]) -> Result<ScanReport, LibraryError> {
        let mut report = ScanReport::default();
        let mut to_read = Vec::new();
        for path in paths {
            // The index keys files by UTF-8 path, as a scan does.
            if path.to_str().is_none() {
                let why = format!("{}: path is not valid UTF-8", path.display());
                report.failed.push((path.clone(), why));
                continue;
            }
            let m = match std::fs::metadata(path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    self.forget(path)?;
                    report.removed += 1;
                    continue;
                }
                // Not stat'd this time, but not gone: its rows stay.
                Err(e) => {
                    report
                        .failed
                        .push((path.clone(), format!("{}: {e}", path.display())));
                    continue;
                }
            };
            let known: bool = self
                .conn
                .prepare_cached("SELECT EXISTS(SELECT 1 FROM items WHERE path = ?1)")?
                .query_row([path.to_string_lossy()], |r| r.get(0))?;
            if known {
                report.changed += 1;
            } else {
                report.added += 1;
            }
            to_read.push((path.clone(), (m.len(), mtime_nanos(&m)), !known));
        }
        self.read_into(&to_read, &mut report)?;
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
            let checked = crate::par::map(batch, workers, |i| crate::check::check(&i.track.path));
            let tx = self.conn.transaction()?;
            for (i, v) in batch.iter().zip(checked) {
                // A panic is more often the decoder's fault than the file's:
                // reported, but not kept, so a fixed decoder checks it again.
                let v = match v {
                    Ok(v) => v,
                    Err(panic) => {
                        out.push((
                            i.track.path.clone(),
                            Verdict::Bad(format!("the decoder panicked: {panic}")),
                        ));
                        continue;
                    }
                };
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
        let tx = self.conn.unchecked_transaction()?;
        forget_rows(&tx, &path.to_string_lossy())?;
        tx.commit()?;
        Ok(())
    }

    /// Record that a file moved. Its check verdict and lyrics miss move
    /// with it: they are keyed by size and mtime, which a move keeps.
    pub fn rename(&self, from: &Path, to: &Path) -> Result<(), LibraryError> {
        let tx = self.conn.unchecked_transaction()?;
        rename_rows(&tx, from, to)?;
        tx.commit()?;
        Ok(())
    }

    /// [`Library::rename`] for many files, in one transaction.
    pub fn rename_all<'a>(
        &mut self,
        moves: impl IntoIterator<Item = (&'a Path, &'a Path)>,
    ) -> Result<(), LibraryError> {
        let tx = self.conn.transaction()?;
        for (from, to) in moves {
            rename_rows(&tx, from, to)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// [`Library::forget`] for many files, in one transaction.
    pub fn forget_all<'a>(
        &mut self,
        paths: impl IntoIterator<Item = &'a Path>,
    ) -> Result<(), LibraryError> {
        let tx = self.conn.transaction()?;
        for path in paths {
            forget_rows(&tx, &path.to_string_lossy())?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Stamp every file indexed under `dir` as added at `added`, in seconds
    /// since the epoch. For an album moved outside [`Library::rename`] (a
    /// re-import), whose files a scan finds as new: it keeps the date it
    /// first arrived.
    pub fn set_added(&self, dir: &Path, added: i64) -> Result<(), LibraryError> {
        let (from, to) = under_bounds(dir);
        self.conn.execute(
            "UPDATE items SET added = ?3 WHERE path >= ?1 AND path < ?2",
            params![from, to, added],
        )?;
        Ok(())
    }

    fn all(&self) -> Result<Vec<Item>, LibraryError> {
        self.select("", [])
    }

    /// Items whose rows match `filter`, a `WHERE` clause or nothing.
    fn select(&self, filter: &str, args: impl rusqlite::Params) -> Result<Vec<Item>, LibraryError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT path, size, mtime, added, title, artist, album, albumartist, track,
                tracktotal, disc, disctotal, date, genre, mb_trackid, mb_albumid, format,
                bitrate, samplerate, bitdepth, length, original_date, compilation FROM items {filter}"
        ))?;
        let rows = stmt.query_map(args, |r| {
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
                    ..Default::default()
                },
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Items matching `query`, in its sort order or beets' default.
    pub fn items(&self, query: &Query) -> Result<Vec<Item>, LibraryError> {
        let items: Vec<Item> = self
            .all()?
            .into_iter()
            .filter(|i| query.matches_item(i))
            .collect();
        Ok(sort_by_fields(items, &sort_keys(query), Item::field))
    }

    /// Albums matching `query`: directories of items, each item in disc and
    /// track order.
    pub fn albums(&self, query: &Query) -> Result<Vec<Album>, LibraryError> {
        let albums: Vec<Album> = group(self.all()?)
            .into_iter()
            .filter(|a| query.matches_album(a))
            .collect();
        Ok(sort_by_fields(albums, &sort_keys(query), Album::field))
    }

    /// The albums in `dirs`, each in disc and track order, read without the
    /// rest of the index.
    pub fn albums_in<'a>(
        &self,
        dirs: impl IntoIterator<Item = &'a Path>,
    ) -> Result<Vec<Album>, LibraryError> {
        let mut items = Vec::new();
        for dir in dirs {
            let (from, to) = under_bounds(dir);
            let found = self.select("WHERE path >= ?1 AND path < ?2", [from, to])?;
            items.extend(
                found
                    .into_iter()
                    .filter(|i| i.track.path.parent() == Some(dir)),
            );
        }
        Ok(group(items))
    }
}

/// The range of indexed paths under `dir`, at any depth: from `dir/` to just
/// before `dir0`, as '0' follows '/'. A range the primary key answers.
fn under_bounds(dir: &Path) -> (String, String) {
    let dir = dir.to_string_lossy();
    let dir = dir.trim_end_matches('/');
    (format!("{dir}/"), format!("{dir}0"))
}

/// Items gathered into the directories that hold them, each in disc and
/// track order.
fn group(items: Vec<Item>) -> Vec<Album> {
    let mut dirs: BTreeMap<PathBuf, Vec<Item>> = BTreeMap::new();
    for item in items {
        let dir = item
            .track
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        dirs.entry(dir).or_default().push(item);
    }
    dirs.into_iter()
        .map(|(dir, mut items)| {
            items.sort_by_key(|i| (i.track.disc, i.track.track));
            Album { dir, items }
        })
        .collect()
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
            false,
        )
        .unwrap();
    }

    #[test]
    fn a_scan_of_one_root_leaves_a_sibling_root_whose_name_it_prefixes() {
        let root = tempfile::tempdir().unwrap();
        let (music, music2) = (root.path().join("music"), root.path().join("music2"));
        std::fs::create_dir_all(&music).unwrap();
        std::fs::create_dir_all(&music2).unwrap();
        tagged_wav(&music.join("x.wav"), "A", "X", "x", 1);
        tagged_wav(&music2.join("y.wav"), "B", "Y", "y", 1);
        let mut lib = Library::in_memory().unwrap();
        lib.update(&music).unwrap();
        lib.update(&music2).unwrap();
        let r = lib.update(&music).unwrap();
        assert_eq!(r.removed, 0);
        assert_eq!(lib.items(&Query::default()).unwrap().len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_folder_is_reported_and_its_rows_kept() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let locked = root.path().join("A");
        std::fs::create_dir_all(&locked).unwrap();
        tagged_wav(&locked.join("x.wav"), "A", "X", "x", 1);
        tagged_wav(&root.path().join("y.wav"), "B", "Y", "y", 1);
        let mut lib = Library::in_memory().unwrap();
        lib.update(root.path()).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable = std::fs::read_dir(&locked).is_ok(); // root reads anything
        let r = lib.update(root.path());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if readable {
            return;
        }
        let r = r.unwrap();
        assert_eq!(r.removed, 0);
        assert!(r.failed.iter().any(|(p, _)| p == &locked));
        assert_eq!(lib.items(&Query::default()).unwrap().len(), 2);
    }

    #[test]
    fn a_stale_scratch_copy_is_swept_and_a_fresh_one_left() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join(".sift-1-1.tmp");
        std::fs::write(&scratch, b"x").unwrap();
        // A copy carries its source's mtime: an old one says nothing.
        std::fs::File::options()
            .write(true)
            .open(&scratch)
            .unwrap()
            .set_modified(SystemTime::now() - 2 * STALE_SCRATCH)
            .unwrap();
        audio_files(root.path(), STALE_SCRATCH).unwrap();
        assert!(scratch.exists(), "made just now");
        audio_files(root.path(), Duration::ZERO).unwrap();
        assert!(!scratch.exists());
    }

    #[test]
    fn a_moved_file_keeps_its_check_and_a_gone_one_loses_it() {
        let root = tempfile::tempdir().unwrap();
        let (a, b) = (root.path().join("a.wav"), root.path().join("b.wav"));
        tagged_wav(&a, "A", "X", "a", 1);
        tagged_wav(&b, "A", "X", "b", 2);
        let mut lib = Library::in_memory().unwrap();
        lib.update(root.path()).unwrap();
        let items = lib.items(&Query::default()).unwrap();
        lib.check(&items).unwrap();
        let checked = |lib: &Library| -> Vec<String> {
            let mut stmt = lib
                .conn
                .prepare("SELECT path FROM checks ORDER BY path")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        let c = root.path().join("c.wav");
        std::fs::rename(&a, &c).unwrap();
        lib.rename(&a, &c).unwrap();
        std::fs::remove_file(&b).unwrap();
        lib.update(root.path()).unwrap();
        assert_eq!(checked(&lib), [c.to_string_lossy().into_owned()]);
    }

    #[test]
    fn albums_in_reads_just_the_folders_named() {
        let root = tempfile::tempdir().unwrap();
        let (a, sibling, below) = (
            root.path().join("A"),
            root.path().join("A B"),
            root.path().join("A").join("CD2"),
        );
        for dir in [&a, &sibling, &below] {
            std::fs::create_dir_all(dir).unwrap();
            tagged_wav(&dir.join("t.wav"), "A", "X", "t", 1);
        }
        let mut lib = Library::in_memory().unwrap();
        lib.update(root.path()).unwrap();
        let albums = lib.albums_in([a.as_path()]).unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].dir, a);
        assert_eq!(albums[0].items.len(), 1);
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

    #[test]
    fn writing_tags_with_preserve_mtime_keeps_the_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        wav(&path, 44_100);
        let past = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(past)
            .unwrap();
        meta::write(
            &path,
            &meta::Tags {
                title: "T".into(),
                ..Default::default()
            },
            None,
            true,
        )
        .unwrap();
        let got = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(
            got.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            past.duration_since(UNIX_EPOCH).unwrap().as_secs()
        );
    }

    #[test]
    fn import_added_stamps_a_new_row_with_the_files_mtime() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.wav");
        tagged_wav(&path, "A", "B", "T", 1);
        let past = SystemTime::now() - Duration::from_secs(86_400);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(past)
            .unwrap();

        let mut lib = Library::in_memory().unwrap().with_import_added(true);
        lib.update(root.path()).unwrap();
        let item = &lib.items(&Query::default()).unwrap()[0];
        assert_eq!(
            item.added,
            past.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
        );
    }

    #[test]
    fn set_added_stamps_only_the_album_under_its_directory() {
        let root = tempfile::tempdir().unwrap();
        for d in ["A", "A 2"] {
            std::fs::create_dir(root.path().join(d)).unwrap();
        }
        tagged_wav(&root.path().join("A/x.wav"), "A", "B", "T", 1);
        tagged_wav(&root.path().join("A 2/y.wav"), "A", "C", "T", 1);
        let mut lib = Library::in_memory().unwrap();
        lib.update(root.path()).unwrap();
        lib.set_added(&root.path().join("A"), 42).unwrap();
        let added = |name: &str| {
            lib.items(&Query::default())
                .unwrap()
                .into_iter()
                .find(|i| i.track.path.ends_with(name))
                .unwrap()
                .added
        };
        assert_eq!(added("x.wav"), 42);
        assert_ne!(
            added("y.wav"),
            42,
            "a directory sharing the prefix is not touched"
        );
    }
}
