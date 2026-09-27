//! Changing what is already in the library: re-filing albums where the
//! current rules say they belong, and finding albums held twice.
//!
//! Every operation plans each destination before any file moves and refuses
//! an album whose plan collides with anything, the same guarantees import
//! gives. Albums are the unit: half an album moved is worse than none.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::library::{Album, Item, Library, LibraryError};
use crate::meta::{self, Tags, Track};
use crate::paths;

/// One file's move.
#[derive(Debug, Clone, PartialEq)]
pub struct Move {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// What re-filing an album would do.
#[derive(Debug)]
pub enum Plan {
    /// Already where the rules put it.
    InPlace,
    /// Every audio file's move, then the album's other files (cover, cue,
    /// log) when the whole album changes directory.
    Moves(Vec<Move>),
    /// Left alone, and why.
    Refused(String),
}

/// The tags an indexed file was written with, as far as the path template
/// can see them. Import wrote these fields; rendering them again gives the
/// path import chose, so an album filed under the current rules plans no
/// moves.
fn tags_of(t: &Track) -> Tags {
    Tags {
        title: t.title.clone().unwrap_or_default(),
        artist: t.artist.clone().unwrap_or_default(),
        album: t.album.clone().unwrap_or_default(),
        album_artist: t
            .album_artist
            .clone()
            .or_else(|| t.artist.clone())
            .unwrap_or_default(),
        track: t.track.unwrap_or_default(),
        track_total: t.track_total.unwrap_or_default(),
        disc: t.disc.unwrap_or(1),
        disc_total: t.disc_total.unwrap_or(1),
        date: t.date.clone(),
        original_date: t.original_date.clone(),
        compilation: t.compilation,
        mb_album_id: t.mb_album_id.clone(),
        ..Default::default()
    }
}

fn destination(cfg: &Config, t: &Track) -> Result<PathBuf, String> {
    let rel = paths::render(cfg, &tags_of(t), t, None).map_err(|e| e.to_string())?;
    let ext = t
        .path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    paths::under(&cfg.directory, &format!("{rel}.{ext}")).map_err(|e| e.to_string())
}

/// Where the current rules put `album`.
pub fn plan_move(cfg: &Config, album: &Album) -> Plan {
    let mut moves = Vec::new();
    let mut claimed: HashMap<String, &Path> = HashMap::new();
    for item in &album.items {
        let from = &item.track.path;
        let to = match destination(cfg, &item.track) {
            Ok(to) => to,
            Err(e) => return Plan::Refused(format!("{}: {e}", from.display())),
        };
        let key = paths::collision_key(&to);
        if let Some(other) = claimed.insert(key.clone(), from) {
            return Plan::Refused(format!(
                "{} and {} would both be filed as {}",
                other.display(),
                from.display(),
                to.display()
            ));
        }
        // A change of case alone is the same file on a case-insensitive
        // disk; it is left as it is rather than renamed through a temporary.
        if key == paths::collision_key(from) {
            continue;
        }
        if to.exists() {
            return Plan::Refused(format!("{} already exists", to.display()));
        }
        moves.push(Move {
            from: from.clone(),
            to,
        });
    }
    if moves.is_empty() {
        return Plan::InPlace;
    }

    // The album's other files follow it only when all of it moves to one
    // new directory; otherwise nothing says which part a cover belongs to.
    let dirs: std::collections::HashSet<PathBuf> = moves
        .iter()
        .filter_map(|m| m.to.parent().map(Path::to_path_buf))
        .collect();
    if moves.len() == album.items.len()
        && dirs.len() == 1
        && let Some(new_dir) = dirs.into_iter().next()
        && new_dir != album.dir
        && let Ok(entries) = std::fs::read_dir(&album.dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if !entry.file_type().is_ok_and(|t| t.is_file()) || meta::is_audio(&path) {
                continue;
            }
            let to = new_dir.join(entry.file_name());
            if to.exists() {
                return Plan::Refused(format!("{} already exists", to.display()));
            }
            moves.push(Move { from: path, to });
        }
    }
    Plan::Moves(moves)
}

/// Carry out a plan from [`plan_move`], keeping the index in step, then
/// remove the directories it emptied (the album's, then its artist's). A
/// directory is only ever removed empty.
pub async fn execute(lib: &mut Library, album: &Album, moves: &[Move]) -> Result<(), LibraryError> {
    for m in moves {
        paths::transfer(&m.from, &m.to, true)
            .await
            .map_err(|e| LibraryError::Io(m.from.clone(), e))?;
        lib.rename(&m.from, &m.to)?;
    }
    if std::fs::remove_dir(&album.dir).is_ok()
        && let Some(parent) = album.dir.parent()
    {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}

/// Move an album into `bin`, at its path relative to the library root, and
/// drop it from the index. Nothing is deleted: putting it back is a move.
pub async fn bin(
    lib: &mut Library,
    library_root: &Path,
    bin: &Path,
    album: &Album,
) -> Result<PathBuf, LibraryError> {
    let rel = album
        .dir
        .strip_prefix(library_root)
        .map_err(|_| {
            LibraryError::Io(
                album.dir.clone(),
                std::io::Error::other("not under the library root"),
            )
        })?
        .to_path_buf();
    let dest = bin.join(rel);
    if dest.exists() {
        return Err(LibraryError::Io(
            dest,
            std::io::Error::from(std::io::ErrorKind::AlreadyExists),
        ));
    }
    let entries =
        std::fs::read_dir(&album.dir).map_err(|e| LibraryError::Io(album.dir.clone(), e))?;
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let from = entry.path();
        paths::transfer(&from, &dest.join(entry.file_name()), true)
            .await
            .map_err(|e| LibraryError::Io(from.clone(), e))?;
        lib.forget(&from)?;
    }
    if std::fs::remove_dir(&album.dir).is_ok()
        && let Some(parent) = album.dir.parent()
    {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(dest)
}

/// Split beets' `modify` arguments into query terms and changes:
/// `field=value` sets, `field!` clears, anything else is part of the query.
pub fn split_modify_args(args: &[String]) -> Result<(Vec<String>, Vec<meta::Change>), String> {
    let (mut query, mut changes) = (Vec::new(), Vec::new());
    for a in args {
        if let Some((f, v)) = a.split_once('=')
            && !f.is_empty()
            && f.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            if !meta::EDITABLE.contains(&f) {
                return Err(format!(
                    "{f} cannot be modified; editable: {}",
                    meta::EDITABLE.join(", ")
                ));
            }
            if matches!(f, "track" | "tracktotal" | "disc" | "disctotal")
                && v.parse::<u32>().is_err()
            {
                return Err(format!("{f} takes a number, not {v:?}"));
            }
            changes.push((f.to_string(), Some(v.to_string())));
        } else if let Some(f) = a.strip_suffix('!')
            && meta::EDITABLE.contains(&f)
        {
            changes.push((f.to_string(), None));
        } else {
            query.push(a.clone());
        }
    }
    Ok((query, changes))
}

/// What `modify` did.
#[derive(Debug, Default)]
pub struct ModifyReport {
    pub files: Vec<PathBuf>,
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// Albums whose tags changed but whose new place was refused, and why.
    pub left: Vec<(PathBuf, String)>,
}

/// Change fields on the files `query` matches (every file of each matching
/// album with `albums`), then re-file the albums whose path that changes.
/// With `pretend`, only report which files would change.
pub async fn modify(
    cfg: &Config,
    lib: &mut Library,
    query: &crate::library::Query,
    albums: bool,
    changes: &[meta::Change],
    pretend: bool,
) -> Result<ModifyReport, LibraryError> {
    let files: Vec<PathBuf> = if albums {
        lib.albums(query)?
            .into_iter()
            .flat_map(|a| a.items.into_iter().map(|i| i.track.path))
            .collect()
    } else {
        lib.items(query)?
            .into_iter()
            .map(|i| i.track.path)
            .collect()
    };
    let mut report = ModifyReport {
        files: files.clone(),
        ..Default::default()
    };
    if pretend || changes.is_empty() {
        return Ok(report);
    }
    for f in &files {
        meta::set(f, changes).map_err(|e| LibraryError::Io(f.clone(), std::io::Error::other(e)))?;
    }
    lib.update(&cfg.directory)?;
    let dirs: std::collections::HashSet<PathBuf> = files
        .iter()
        .filter_map(|f| f.parent().map(Path::to_path_buf))
        .collect();
    for album in lib.albums(&crate::library::Query::default())? {
        if !dirs.contains(&album.dir) {
            continue;
        }
        match plan_move(cfg, &album) {
            Plan::InPlace => {}
            Plan::Refused(why) => report.left.push((album.dir.clone(), why)),
            Plan::Moves(moves) => {
                let to = moves
                    .first()
                    .and_then(|m| m.to.parent())
                    .map(Path::to_path_buf)
                    .unwrap_or_default();
                execute(lib, &album, &moves).await?;
                report.moved.push((album.dir.clone(), to));
            }
        }
    }
    Ok(report)
}

/// Albums held more than once, and which copy to keep.
#[derive(Debug)]
pub struct Duplicate<'a> {
    pub keep: &'a Album,
    pub others: Vec<(&'a Album, &'static str)>,
}

/// Why one copy of an album is better than another, first difference wins:
/// lossless over lossy, then more of its tracks present, then higher
/// resolution. Equal copies keep the one filed under the current rules.
fn rank(cfg: &Config, a: &Album) -> (bool, usize, u32, u8, bool) {
    let lossless = a.items.iter().all(|i| {
        matches!(
            i.track.format.as_str(),
            "FLAC" | "ALAC" | "WAV" | "AIFF" | "APE" | "WavPack"
        )
    });
    let rate = a
        .items
        .iter()
        .filter_map(|i| i.track.sample_rate)
        .min()
        .unwrap_or(0);
    let depth = a
        .items
        .iter()
        .filter_map(|i| i.track.bit_depth)
        .min()
        .unwrap_or(0);
    let placed = matches!(plan_move(cfg, a), Plan::InPlace);
    (lossless, a.items.len(), rate, depth, placed)
}

fn why(keep: (bool, usize, u32, u8, bool), other: (bool, usize, u32, u8, bool)) -> &'static str {
    if keep.0 != other.0 {
        "lossy copy of a lossless album"
    } else if keep.1 != other.1 {
        "fewer tracks than the other copy"
    } else if keep.2 != other.2 || keep.3 != other.3 {
        "lower resolution than the other copy"
    } else if keep.4 != other.4 {
        "the same album filed under old naming rules"
    } else {
        "identical copy"
    }
}

/// Whether `album` has the recording `item` is: by MusicBrainz recording id
/// when both have one, by title otherwise.
fn contains(album: &Album, item: &Item) -> bool {
    let norm = |s: &Option<String>| s.as_deref().unwrap_or("").trim().to_lowercase();
    album.items.iter().any(
        |k| match (&k.track.mb_recording_id, &item.track.mb_recording_id) {
            (Some(a), Some(b)) => a == b,
            _ => norm(&k.track.title) == norm(&item.track.title),
        },
    )
}

/// Albums that are the same release. With MusicBrainz ids, the same release
/// id; without, the same album artist, album and track titles, compared
/// case-insensitively, so two editions with different track lists are not
/// called duplicates.
pub fn duplicates<'a>(cfg: &Config, albums: &'a [Album]) -> Vec<Duplicate<'a>> {
    let key = |a: &Album| -> Option<String> {
        let first: &Item = a.items.first()?;
        if let Some(id) = &first.track.mb_album_id {
            return Some(format!("mb:{id}"));
        }
        let norm = |s: &Option<String>| s.as_deref().unwrap_or("").trim().to_lowercase();
        let titles: Vec<String> = a.items.iter().map(|i| norm(&i.track.title)).collect();
        Some(format!(
            "tags:{}\u{1f}{}\u{1f}{}",
            norm(
                &first
                    .track
                    .album_artist
                    .clone()
                    .or(first.track.artist.clone())
            ),
            norm(&first.track.album),
            titles.join("\u{1f}")
        ))
    };
    let mut groups: HashMap<String, Vec<&Album>> = HashMap::new();
    for a in albums {
        if let Some(k) = key(a) {
            groups.entry(k).or_default().push(a);
        }
    }
    let mut out: Vec<Duplicate> = groups
        .into_values()
        .filter(|g| g.len() > 1)
        .filter_map(|mut g| {
            g.sort_by_key(|a| std::cmp::Reverse(rank(cfg, a)));
            let keep = g[0];
            let k = rank(cfg, keep);
            // A copy is spare only if the kept one has every track it has.
            // One release in several directories (a folder per disc) is an
            // album in pieces, which `move` puts back together; binning a
            // piece would lose music.
            let others: Vec<_> = g[1..]
                .iter()
                .filter(|a| a.items.iter().all(|i| contains(keep, i)))
                .map(|a| (*a, why(k, rank(cfg, a))))
                .collect();
            (!others.is_empty()).then_some(Duplicate { keep, others })
        })
        .collect();
    out.sort_by(|a, b| a.keep.dir.cmp(&b.keep.dir));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Query;

    fn config(root: &Path) -> Config {
        let yaml = root.join("config.yaml");
        std::fs::write(
            &yaml,
            format!(
                r#"directory: {}
original_date: true
per_disc_numbering: true
paths:
  default: $albumartist/%if{{$year,($year) }}$album [$format]/$disc$track. $artist - $title
replace:
  '[\\/]': "-"
  '\.$': "-"
  '[<>:"\?\*\|]': "-"
"#,
                root.join("lib").display()
            ),
        )
        .unwrap();
        Config::load(&yaml).unwrap()
    }

    fn file(path: &Path, artist: &str, album: &str, title: &str, n: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::library::tests::wav(path, 44_100);
        meta::write(
            path,
            &Tags {
                title: title.into(),
                artist: artist.into(),
                album: album.into(),
                album_artist: artist.into(),
                track: n,
                track_total: 2,
                disc: 1,
                disc_total: 1,
                date: Some("2011-09-26".into()),
                original_date: Some("1994-09-27".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    }

    fn album_of(lib: &Library, dir: &Path) -> Album {
        lib.albums(&Query::default())
            .unwrap()
            .into_iter()
            .find(|a| a.dir == dir)
            .unwrap()
    }

    #[tokio::test]
    async fn an_album_under_old_naming_rules_moves_with_its_cover_and_leaves_nothing_behind() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(root.path());
        let lib_root = root.path().join("lib");
        let old = lib_root.join("R.E.M_").join("Monster");
        file(
            &old.join("a.wav"),
            "R.E.M.",
            "Monster",
            "What's the Frequency, Kenneth?",
            1,
        );
        file(
            &old.join("b.wav"),
            "R.E.M.",
            "Monster",
            "Crush With Eyeliner",
            2,
        );
        std::fs::write(old.join("cover.jpg"), b"jpg").unwrap();

        let mut lib = Library::in_memory().unwrap();
        lib.update(&lib_root).unwrap();
        let album = album_of(&lib, &old);
        let Plan::Moves(moves) = plan_move(&cfg, &album) else {
            panic!("expected moves");
        };
        let new = lib_root.join("R.E.M-").join("(1994) Monster [WAV]");
        assert_eq!(
            moves.iter().map(|m| m.to.clone()).collect::<Vec<_>>(),
            [
                new.join("0101. R.E.M. - What's the Frequency, Kenneth-.wav"),
                new.join("0102. R.E.M. - Crush With Eyeliner.wav"),
                new.join("cover.jpg"),
            ]
        );
        execute(&mut lib, &album, &moves).await.unwrap();
        assert!(new.join("cover.jpg").exists());
        assert!(!lib_root.join("R.E.M_").exists(), "emptied directories go");
        let moved = album_of(&lib, &new);
        assert!(matches!(plan_move(&cfg, &moved), Plan::InPlace));
        let r = lib.update(&lib_root).unwrap();
        assert_eq!(
            (r.added, r.removed, r.changed),
            (0, 0, 0),
            "the index followed the moves"
        );
    }

    #[tokio::test]
    async fn an_album_whose_destination_is_taken_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(root.path());
        let lib_root = root.path().join("lib");
        let old = lib_root.join("R.E.M_").join("Monster");
        let new = lib_root.join("R.E.M-").join("(1994) Monster [WAV]");
        for dir in [&old, &new] {
            file(
                &dir.join("0101. R.E.M. - Crush.wav"),
                "R.E.M.",
                "Monster",
                "Crush",
                1,
            );
        }
        let mut lib = Library::in_memory().unwrap();
        lib.update(&lib_root).unwrap();
        let album = album_of(&lib, &old);
        assert!(
            matches!(plan_move(&cfg, &album), Plan::Refused(r) if r.contains("already exists"))
        );

        let albums = lib.albums(&Query::default()).unwrap();
        let dupes = duplicates(&cfg, &albums);
        assert_eq!(dupes.len(), 1);
        assert_eq!(
            dupes[0].keep.dir, new,
            "the copy under the current rules is kept"
        );
        assert_eq!(dupes[0].others[0].0.dir, old);
        assert_eq!(
            dupes[0].others[0].1,
            "the same album filed under old naming rules"
        );
    }

    #[test]
    fn a_release_split_across_folders_is_not_a_duplicate_of_itself() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(root.path());
        let lib_root = root.path().join("lib");
        let tag = |path: &Path, title: &str, n: u32, disc: u32| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            crate::library::tests::wav(path, 44_100);
            meta::write(
                path,
                &Tags {
                    title: title.into(),
                    artist: "A".into(),
                    album: "B".into(),
                    album_artist: "A".into(),
                    track: n,
                    disc,
                    mb_album_id: Some("release".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        };
        tag(&lib_root.join("B/CD1/1.wav"), "One", 1, 1);
        tag(&lib_root.join("B/CD1/2.wav"), "Two", 2, 1);
        tag(&lib_root.join("B/CD2/1.wav"), "Three", 1, 2);
        let mut lib = Library::in_memory().unwrap();
        lib.update(&lib_root).unwrap();
        let albums = lib.albums(&Query::default()).unwrap();
        assert!(duplicates(&cfg, &albums).is_empty());
    }

    #[test]
    fn modify_arguments_split_into_query_and_changes() {
        let args: Vec<String> = ["artist:burial", "album=Untrue", "genre!", "year:2007"]
            .map(String::from)
            .into();
        let (query, changes) = split_modify_args(&args).unwrap();
        assert_eq!(query, ["artist:burial", "year:2007"]);
        assert_eq!(
            changes,
            [
                ("album".to_string(), Some("Untrue".to_string())),
                ("genre".to_string(), None)
            ]
        );
        assert!(split_modify_args(&["path=/etc".to_string()]).is_err());
        assert!(split_modify_args(&["track=two".to_string()]).is_err());
    }

    #[tokio::test]
    async fn modify_changes_only_the_named_field_and_refiles_the_album() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(root.path());
        let lib_root = root.path().join("lib");
        let old = lib_root.join("Burial").join("(1994) Untru [WAV]");
        for (n, title) in [(1, "Archangel"), (2, "Near Dark")] {
            let path = old.join(format!("010{n}. Burial - {title}.wav"));
            file(&path, "Burial", "Untru", title, n);
            meta::set(&path, &[("genre".into(), Some("Garage".into()))]).unwrap();
        }
        let mut lib = Library::in_memory().unwrap();
        lib.update(&lib_root).unwrap();
        let query = crate::library::Query::parse(&["album:untru"]).unwrap();
        let changes = [("album".to_string(), Some("Untrue".to_string()))];
        let r = modify(&cfg, &mut lib, &query, true, &changes, false)
            .await
            .unwrap();
        assert_eq!(r.files.len(), 2);
        let new = lib_root.join("Burial").join("(1994) Untrue [WAV]");
        assert_eq!(r.moved, [(old.clone(), new.clone())]);
        let moved = new.join("0101. Burial - Archangel.wav");
        let after = meta::read(&moved).unwrap();
        assert_eq!(after.album.as_deref(), Some("Untrue"));
        assert_eq!(
            after.genre.as_deref(),
            Some("Garage"),
            "untouched fields survive"
        );
    }

    #[tokio::test]
    async fn different_track_lists_are_different_editions_not_duplicates() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(root.path());
        let lib_root = root.path().join("lib");
        file(&lib_root.join("x/1.wav"), "A", "B", "One", 1);
        file(&lib_root.join("y/1.wav"), "A", "B", "One", 1);
        file(&lib_root.join("y/2.wav"), "A", "B", "Bonus", 2);
        let mut lib = Library::in_memory().unwrap();
        lib.update(&lib_root).unwrap();
        let albums = lib.albums(&Query::default()).unwrap();
        assert!(duplicates(&cfg, &albums).is_empty());
    }
}
