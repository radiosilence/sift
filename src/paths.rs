//! Where a track goes, and getting it there.

use std::path::Path;

use unicode_normalization::UnicodeNormalization;

use crate::config::Config;
use crate::format::{self, FormatError};
use crate::meta::{Tags, Track};
use crate::musicbrainz::Release;

/// A path component's byte ceiling, extension included. Common filesystems
/// cap a name at 255 bytes; this leaves room for the extension and for a
/// `.part` or temporary suffix during a move.
const COMPONENT_BYTES: usize = 240;

/// The year of a date, if it has one. Beets writes `0000` for an unknown
/// original date, and means nothing by it.
fn year(date: Option<&str>) -> Option<String> {
    date.and_then(|d| d.get(..4))
        .filter(|y| y.chars().all(|c| c.is_ascii_digit()) && *y != "0000")
        .map(str::to_string)
}

/// The library-relative path for one track, without extension.
///
/// Field values are cleaned before they are substituted, so a slash in a
/// title ("AC/DC") cannot become a directory; the template's own slashes are
/// the only separators. Components are then cleaned again for the rules that
/// only make sense at the edges of a name, and NFC-normalised so the same
/// title written on macOS and on Linux is the same bytes.
pub fn render(
    cfg: &Config,
    tags: &Tags,
    local: &Track,
    release: Option<&Release>,
) -> Result<String, PathError> {
    let date_year = year(tags.date.as_deref());
    let original_year = year(tags.original_date.as_deref());
    let shown_year = if cfg.original_date {
        original_year.clone().or(date_year.clone())
    } else {
        date_year.clone()
    };
    let album_type = release
        .and_then(|r| r.release_group.as_ref())
        .and_then(|g| g.primary_type.clone());
    let fields = |name: &str| -> Option<String> {
        let raw = match name {
            "album artist" | "albumartist" => Some(tags.album_artist.clone()),
            "artist" => Some(tags.artist.clone()),
            "album" => Some(tags.album.clone()),
            "title" => Some(tags.title.clone()),
            "tracknumber" | "track" => Some(tags.track.to_string()),
            "totaltracks" => Some(tags.track_total.to_string()),
            "discnumber" | "disc" => Some(tags.disc.to_string()),
            "totaldiscs" => Some(tags.disc_total.to_string()),
            "year" => shown_year.clone(),
            "original year" => original_year.clone(),
            "date" => {
                if cfg.original_date {
                    tags.original_date.clone().or(tags.date.clone())
                } else {
                    tags.date.clone()
                }
            }
            "codec" | "format" => Some(local.format.clone()),
            "label" => tags.label.clone(),
            "catalog number" => tags.catalog_number.clone(),
            "country" => tags.country.clone(),
            "media" => tags.media.clone(),
            "album type" => album_type.clone(),
            "musicbrainz album id" => tags.mb_album_id.clone(),
            "bitdepth" => local.bit_depth.map(|b| b.to_string()),
            "samplerate" => local.sample_rate.map(|r| r.to_string()),
            _ => None,
        }?;
        let v = clean_value(cfg, &raw);
        (!v.is_empty()).then_some(v)
    };
    let template = if tags.compilation {
        &cfg.path_comp
    } else {
        &cfg.path_default
    };
    let rendered = format::format(template, &fields)?;
    let parts: Vec<String> = rendered
        .split('/')
        .map(|c| truncate(&clean(cfg, c), COMPONENT_BYTES))
        .collect();
    // An empty component means a field the template needed was missing.
    // Dropping it would file tracks one level up, and a typo'd template
    // could collapse a whole album onto one name, so it is refused.
    if let Some(bad) = parts
        .iter()
        .find(|c| c.is_empty() || *c == "." || *c == "..")
    {
        return Err(PathError::Component {
            template: template.clone(),
            rendered: rendered.clone(),
            part: bad.clone(),
        });
    }
    Ok(parts.join("/"))
}

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("template: {0}")]
    Template(#[from] FormatError),
    #[error(
        "template {template:?} rendered {rendered:?}, which has an empty or relative component {part:?}"
    )]
    Component {
        template: String,
        rendered: String,
        part: String,
    },
    #[error("{0} is outside the library")]
    Escapes(std::path::PathBuf),
}

/// `rel` under `root`, refusing anything that would land outside it. The
/// renderer already prevents this; tags are untrusted input, so it is checked
/// again where it matters.
pub fn under(root: &Path, rel: &str) -> Result<std::path::PathBuf, PathError> {
    let path = root.join(rel);
    let clean = path.components().all(|c| {
        matches!(
            c,
            std::path::Component::Normal(_)
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    });
    if !clean || !path.starts_with(root) {
        return Err(PathError::Escapes(path));
    }
    Ok(existing_case(root, &path))
}

/// `path` with each directory below `root` spelled as one already on disk
/// when the two differ only in case.
///
/// Tags spell an act "The Squire Of Gothos" on one record and "of" on the
/// next. On a case-sensitive disk that files them in two folders; on a
/// case-insensitive one it makes the second import collide with the first.
/// Reusing the folder that is already there keeps an artist in one place, in
/// the spelling it was first filed under.
fn existing_case(root: &Path, path: &Path) -> std::path::PathBuf {
    let Ok(rel) = path.strip_prefix(root) else {
        return path.to_path_buf();
    };
    let parts: Vec<_> = rel.components().collect();
    let mut out = root.to_path_buf();
    // Below a directory that does not exist yet, nothing does.
    let mut absent = false;
    for (i, part) in parts.iter().enumerate() {
        let name = part.as_os_str();
        if absent || i + 1 == parts.len() {
            out.push(name);
            continue;
        }
        // On a case-sensitive disk a directory of exactly this name is the
        // answer, for one stat. On a case-insensitive one it "exists" in any
        // case, and the question is how it is spelled, so the listing is read.
        if CASE_SENSITIVE && out.join(name).is_dir() {
            out.push(name);
            continue;
        }
        let fold =
            |n: &std::ffi::OsStr| n.to_string_lossy().nfc().collect::<String>().to_lowercase();
        let want = fold(name);
        // Names first: only an entry that matches is asked whether it is a
        // directory, so a listing of thousands of artists costs one read.
        let dirs: Vec<std::ffi::OsString> = std::fs::read_dir(&out)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| fold(&e.file_name()) == want && e.path().is_dir())
                    .map(|e| e.file_name())
                    .collect()
            })
            .unwrap_or_default();
        absent = dirs.is_empty();
        let spelled = dirs
            .iter()
            .find(|d| d.as_os_str() == name)
            .or_else(|| dirs.first())
            .cloned()
            .unwrap_or_else(|| name.to_os_string());
        out.push(spelled);
    }
    out
}

/// Whether the disk tells `Rain` from `RAIN`: the same split as
/// [`collision_key`].
const CASE_SENSITIVE: bool = !cfg!(any(target_os = "macos", windows));

/// How two destination names compare on the filesystem they land on:
/// case-insensitively on macOS, where `Rain.flac` and `RAIN.flac` are one
/// file.
pub fn collision_key(path: &Path) -> String {
    let s: String = path.to_string_lossy().nfc().collect();
    if cfg!(target_os = "macos") || cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

/// A field value before it is substituted: normalised, and with any
/// separator made safe, since a separator inside a value is never meant as
/// one. The configured replacements wait for the whole component, as in
/// beets: `^\.` and `\.$` describe the edges of a name, and "E.P." in the
/// middle of an album folder's name is not at one.
fn clean_value(cfg: &Config, s: &str) -> String {
    let mut s: String = s.nfc().collect();
    if cfg.asciify_paths {
        s = deunicode::deunicode(&s);
    }
    s.replace(['/', '\0'], "-")
}

/// A rendered path component, with the configured replacements applied.
fn clean(cfg: &Config, s: &str) -> String {
    let mut s = clean_value(cfg, s);
    for (re, with) in &cfg.replace {
        s = re.replace_all(&s, with.as_str()).into_owned();
    }
    s
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].trim_end().to_string()
}

/// A hidden, unique name beside `path` for a file being written: short,
/// since a name already at the length limit has no room for a suffix, and
/// hidden, so a library scan skips it.
pub(crate) fn scratch_beside(path: &Path) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_file_name(format!(".sift-{}-{n}.tmp", std::process::id()))
}

/// Whether `path` is a file [`scratch_beside`] named: it belongs to a write
/// in progress, so it never moves with an album's other files.
pub(crate) fn is_scratch(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(".sift-") && n.ends_with(".tmp"))
}

/// Move (or copy) `from` to `to`, creating directories, never replacing an
/// existing file.
///
/// The destination name is claimed with `create_new` first, so nothing can
/// appear in the gap between checking and moving. Within a filesystem the
/// file is then renamed over its own claim. Across filesystems it is copied
/// to a temporary sibling, synced, length-checked and renamed into place, so
/// a crash leaves either the old state or the whole file, never a truncated
/// one under the real name.
///
/// The work runs on a blocking thread and finishes even if this future is
/// dropped; a failure removes the temporary file and the claim.
pub async fn transfer(from: &Path, to: &Path, move_file: bool) -> std::io::Result<()> {
    let (from, to) = (from.to_path_buf(), to.to_path_buf());
    tokio::task::spawn_blocking(move || transfer_blocking(&from, &to, move_file))
        .await
        .expect("transfer panicked")
}

/// What a failed transfer leaves behind to remove: its claim on the
/// destination, while still empty, and its temporary copy.
struct Claim<'a> {
    to: &'a Path,
    tmp: Option<std::path::PathBuf>,
    /// Folders made for `to`, deepest first.
    made: Vec<std::path::PathBuf>,
    kept: bool,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if self.kept {
            return;
        }
        if let Some(tmp) = &self.tmp {
            let _ = std::fs::remove_file(tmp);
        }
        if std::fs::metadata(self.to).is_ok_and(|m| m.len() == 0) {
            let _ = std::fs::remove_file(self.to);
        }
        remove_made(&self.made);
    }
}

/// Remove folders a transfer made, deepest first, each only if empty.
fn remove_made(made: &[std::path::PathBuf]) {
    for dir in made {
        if std::fs::remove_dir(dir).is_err() {
            break;
        }
    }
}

/// [`transfer`], on the calling thread.
pub(crate) fn transfer_blocking(from: &Path, to: &Path, move_file: bool) -> std::io::Result<()> {
    transfer_making(from, to, move_file).map(drop)
}

/// [`transfer_blocking`], returning the folders it made for `to`,
/// deepest first.
fn transfer_making(
    from: &Path,
    to: &Path,
    move_file: bool,
) -> std::io::Result<Vec<std::path::PathBuf>> {
    let made: Vec<std::path::PathBuf> = to
        .ancestors()
        .skip(1)
        .take_while(|dir| !dir.as_os_str().is_empty() && !dir.exists())
        .map(Path::to_path_buf)
        .collect();
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    let mut claim = Claim {
        to,
        tmp: None,
        made,
        kept: false,
    };
    if move_file {
        match std::fs::rename(from, to) {
            Ok(()) => {
                claim.kept = true;
                return Ok(std::mem::take(&mut claim.made));
            }
            Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {}
            Err(e) => return Err(e),
        }
    }
    let tmp = claim.tmp.insert(scratch_beside(to));
    let from_meta = std::fs::metadata(from)?;
    let copied = std::fs::copy(from, &*tmp)?;
    let file = std::fs::File::open(&*tmp)?;
    let expected = from_meta.len();
    if copied != expected || file.metadata()?.len() != expected {
        return Err(std::io::Error::other(format!(
            "short copy of {}",
            from.display()
        )));
    }
    // A copy gets a fresh mtime; a move of the same file should never look
    // newer than the original just because it crossed filesystems. Set
    // through the read handle: the copy has the source's mode, which may
    // not allow writing, and setting times needs only ownership.
    if let Ok(modified) = from_meta.modified() {
        file.set_modified(modified)?;
    }
    // A copy filed into the library is the library's to tag: a source
    // made read-only (a share, a finished download) is given back its
    // owner's write permission. A move keeps the file as it was.
    #[cfg(unix)]
    if !move_file {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = from_meta.permissions();
        perms.set_mode(perms.mode() | 0o200);
        file.set_permissions(perms)?;
    }
    file.sync_all()?;
    std::fs::rename(&*tmp, to)?;
    claim.kept = true;
    if move_file {
        // The new name must be on disk before the old one goes, or a crash
        // between the two could keep the unlink and lose the rename.
        if let Some(dir) = to.parent()
            && let Ok(dir) = std::fs::File::open(dir)
        {
            let _ = dir.sync_all();
        }
        // A source that cannot be removed is still whole: the move did not
        // happen, so the copy goes rather than leave the file in two places.
        if let Err(e) = std::fs::remove_file(from) {
            if std::fs::remove_file(to).is_err() {
                tracing::error!(
                    "{} could not be moved and is now also at {}",
                    from.display(),
                    to.display()
                );
            }
            // The folders made for it go with it.
            claim.kept = false;
            return Err(e);
        }
    }
    Ok(std::mem::take(&mut claim.made))
}

/// The transfers made so far toward one change of the library, undone
/// together if the change does not finish: an album half moved is worse
/// than one not moved at all. Dropped before [`Journal::commit`], it undoes
/// them, so a cancelled or failed change puts every file back where it was,
/// and removes the folders it made for them. Only locations are undone:
/// tags written to a file along the way stay.
#[derive(Default)]
pub(crate) struct Journal {
    done: Vec<Done>,
}

/// One transfer, and the folders it made.
struct Done {
    from: std::path::PathBuf,
    to: std::path::PathBuf,
    moved: bool,
    made: Vec<std::path::PathBuf>,
}

impl Journal {
    /// [`transfer_blocking`], recorded to be undone.
    pub(crate) fn transfer(
        &mut self,
        from: &Path,
        to: &Path,
        move_file: bool,
    ) -> std::io::Result<()> {
        let made = transfer_making(from, to, move_file)?;
        self.done.push(Done {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
            moved: move_file,
            made,
        });
        Ok(())
    }

    /// Each move made, in order.
    pub(crate) fn moves(&self) -> impl Iterator<Item = (&Path, &Path)> {
        self.done
            .iter()
            .filter(|d| d.moved)
            .map(|d| (d.from.as_path(), d.to.as_path()))
    }

    /// Keep every transfer.
    pub(crate) fn commit(mut self) {
        self.done.clear();
    }

    /// Put every file back, newest first: a moved file to where it came
    /// from, a copy removed. Returns the files that could not be.
    fn undo(&mut self) -> Vec<std::path::PathBuf> {
        let mut stuck = Vec::new();
        for d in self.done.drain(..).rev() {
            let undone = if d.moved {
                transfer_blocking(&d.to, &d.from, true)
            } else {
                std::fs::remove_file(&d.to)
            };
            if undone.is_err() {
                stuck.push(d.to);
                continue;
            }
            remove_made(&d.made);
        }
        stuck
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        let stuck = self.undo();
        if !stuck.is_empty() {
            tracing::error!(
                "could not put back: {}",
                stuck
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_path_under_a_folder_not_yet_made_is_kept_as_written() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("Other")).unwrap();
        let got = under(root, "New Artist/(2020) Album [FLAC]/01. A.flac").unwrap();
        assert_eq!(got, root.join("New Artist/(2020) Album [FLAC]/01. A.flac"));
    }

    #[test]
    fn a_folder_already_filed_in_another_case_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("The Squire of Gothos").join("(2010) Old [MP3]"))
            .unwrap();
        let got = under(
            root,
            "The Squire Of Gothos/(2010) New [FLAC]/0101. Track.flac",
        )
        .unwrap();
        assert_eq!(
            got,
            root.join("The Squire of Gothos")
                .join("(2010) New [FLAC]")
                .join("0101. Track.flac")
        );
        // Nothing to match: filed as the tags spell it.
        let fresh = under(root, "Someone Else/Album/01. T.flac").unwrap();
        assert_eq!(
            fresh,
            root.join("Someone Else").join("Album").join("01. T.flac")
        );
    }

    use super::*;

    fn release() -> Release {
        serde_json::from_value(serde_json::json!({"id": "r", "title": "x", "media": []})).unwrap()
    }

    #[test]
    fn slashes_in_values_never_become_directories() {
        let cfg = Config {
            directory: "/m".into(),
            ..Config::default()
        };
        let tags = Tags {
            album_artist: "AC/DC".into(),
            album: "Live: 1991".into(),
            title: "T.N.T.".into(),
            track: 3,
            ..Default::default()
        };
        let local = Track {
            format: "FLAC".into(),
            ..Default::default()
        };
        assert_eq!(
            render(&cfg, &tags, &local, Some(&release())).unwrap(),
            "AC-DC/Live_ 1991/03 T.N.T_"
        );
    }

    #[test]
    fn edge_replacements_apply_to_the_edges_of_a_component_only() {
        let cfg = Config {
            directory: "/m".into(),
            path_default: "%album artist%/%album% x/%title%".into(),
            replace: vec![
                (regex::Regex::new(r"\.$").unwrap(), "-".into()),
                (regex::Regex::new(r"^\.").unwrap(), "-".into()),
            ],
            ..Config::default()
        };
        let tags = Tags {
            album_artist: "R.E.M.".into(),
            album: "The Vertigo E.P.".into(),
            title: "...Kill All Your Friends...".into(),
            ..Default::default()
        };
        assert_eq!(
            render(&cfg, &tags, &Track::default(), None).unwrap(),
            "R.E.M-/The Vertigo E.P. x/-..Kill All Your Friends..-"
        );
    }

    #[test]
    fn a_zero_original_year_falls_back_to_the_release_year() {
        let cfg = Config {
            directory: "/m".into(),
            path_default: "[(%year%) ]%album%".into(),
            original_date: true,
            ..Config::default()
        };
        let tags = Tags {
            album: "Swine Flu".into(),
            date: Some("2009-03-01".into()),
            original_date: Some("0000".into()),
            ..Default::default()
        };
        assert_eq!(
            render(&cfg, &tags, &Track::default(), None).unwrap(),
            "(2009) Swine Flu"
        );
    }

    #[test]
    fn long_names_are_cut_on_a_character_boundary() {
        let s = "é".repeat(200);
        let t = truncate(&s, 241);
        assert_eq!(t.len(), 240);
        assert!(t.is_char_boundary(t.len()));
    }

    #[test]
    fn empty_components_are_refused_not_dropped() {
        let cfg = Config {
            directory: "/m".into(),
            path_default: "%album artist%/%genre%/%title%".into(),
            ..Config::default()
        };
        let tags = Tags {
            album_artist: "A".into(),
            title: "T".into(),
            ..Default::default()
        };
        assert!(matches!(
            render(&cfg, &tags, &Track::default(), Some(&release())),
            Err(PathError::Component { .. })
        ));
    }

    #[test]
    fn nothing_escapes_the_library() {
        assert!(under(Path::new("/m"), "a/b.flac").is_ok());
        assert!(under(Path::new("/m"), "../etc/passwd").is_err());
    }

    #[tokio::test]
    async fn transfer_never_replaces_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.flac");
        let to = dir.path().join("b.flac");
        std::fs::write(&from, b"new").unwrap();
        std::fs::write(&to, b"precious").unwrap();
        assert!(transfer(&from, &to, true).await.is_err());
        assert_eq!(std::fs::read(&to).unwrap(), b"precious");
        assert!(from.exists());
    }

    #[tokio::test]
    async fn a_failed_transfer_leaves_no_claim_or_copy() {
        let dir = tempfile::tempdir().unwrap();
        let to = dir.path().join("lib/b.flac");
        assert!(
            transfer(&dir.path().join("gone.flac"), &to, true)
                .await
                .is_err()
        );
        assert!(
            transfer(&dir.path().join("gone.flac"), &to, false)
                .await
                .is_err()
        );
        assert!(
            !dir.path().join("lib").exists(),
            "nor the folder made for it"
        );
    }

    #[test]
    fn a_journal_dropped_unfinished_puts_every_file_back() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.flac"), dir.path().join("b.flac"));
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let mut journal = Journal::default();
        journal
            .transfer(&a, &dir.path().join("lib/a.flac"), true)
            .unwrap();
        journal
            .transfer(&b, &dir.path().join("lib/b.flac"), false)
            .unwrap();
        drop(journal);
        assert_eq!(std::fs::read(&a).unwrap(), b"a");
        assert_eq!(std::fs::read(&b).unwrap(), b"b");
        assert!(
            !dir.path().join("lib").exists(),
            "the folder it made goes too"
        );
    }

    #[test]
    fn undoing_removes_only_the_folders_a_transfer_made() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib");
        std::fs::create_dir_all(lib.join("Artist")).unwrap();
        let (a, b) = (dir.path().join("a.flac"), dir.path().join("b.flac"));
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let mut journal = Journal::default();
        journal
            .transfer(&a, &lib.join("Artist/Album/a.flac"), true)
            .unwrap();
        journal
            .transfer(&b, &lib.join("New/Album/b.flac"), true)
            .unwrap();
        drop(journal);
        assert!(lib.join("Artist").is_dir(), "it was there before");
        assert!(!lib.join("Artist/Album").exists());
        assert!(!lib.join("New").exists());
        assert!(lib.is_dir());
    }

    #[test]
    fn a_committed_journal_keeps_its_transfers() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.flac");
        std::fs::write(&a, b"a").unwrap();
        let mut journal = Journal::default();
        journal
            .transfer(&a, &dir.path().join("lib/a.flac"), true)
            .unwrap();
        journal.commit();
        assert!(!a.exists());
        assert!(dir.path().join("lib/a.flac").exists());
    }

    #[tokio::test]
    async fn transfer_moves_and_creates_directories() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.flac");
        std::fs::write(&from, b"x").unwrap();
        let to = dir.path().join("lib/Artist/Album/01 a.flac");
        transfer(&from, &to, true).await.unwrap();
        assert!(!from.exists());
        assert_eq!(std::fs::read(&to).unwrap(), b"x");
    }

    /// A copy takes the destination through a temporary file, which would
    /// otherwise leave it with a fresh mtime; `transfer` restores the
    /// source's. `move_file: false` always goes through the copy path, so a
    /// same-filesystem temp dir exercises it without needing two devices.
    #[tokio::test]
    async fn a_copy_keeps_the_source_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.flac");
        std::fs::write(&from, b"x").unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&from)
            .unwrap()
            .set_modified(past)
            .unwrap();
        let to = dir.path().join("lib/Artist/Album/01 a.flac");
        transfer(&from, &to, false).await.unwrap();
        let got = std::fs::metadata(&to).unwrap().modified().unwrap();
        assert_eq!(
            got.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
            past.duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        );
    }
}
