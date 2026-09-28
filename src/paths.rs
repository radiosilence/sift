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
    for (i, part) in parts.iter().enumerate() {
        let name = part.as_os_str();
        let last = i + 1 == parts.len();
        if last {
            out.push(name);
            continue;
        }
        // Read the directory rather than ask whether the name exists: on a
        // case-insensitive disk it "exists" in any case, and the question is
        // how it is spelled.
        let fold =
            |n: &std::ffi::OsStr| n.to_string_lossy().nfc().collect::<String>().to_lowercase();
        let want = fold(name);
        let dirs: Vec<std::ffi::OsString> = std::fs::read_dir(&out)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.path().is_dir() && fold(&e.file_name()) == want)
                    .map(|e| e.file_name())
                    .collect()
            })
            .unwrap_or_default();
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

/// Move (or copy) `from` to `to`, creating directories, never replacing an
/// existing file.
///
/// The destination name is claimed with `create_new` first, so nothing can
/// appear in the gap between checking and moving. Within a filesystem the
/// file is then renamed over its own claim. Across filesystems it is copied
/// to a temporary sibling, synced, length-checked and renamed into place, so
/// a crash leaves either the old state or the whole file, never a truncated
/// one under the real name.
pub async fn transfer(from: &Path, to: &Path, move_file: bool) -> std::io::Result<()> {
    if let Some(dir) = to.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)
        .await?;
    let result = async {
        if move_file {
            match tokio::fs::rename(from, to).await {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {}
                Err(e) => return Err(e),
            }
        }
        let tmp = to.with_file_name(format!(
            ".{}.sift-{}.tmp",
            to.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id()
        ));
        let from_meta = tokio::fs::metadata(from).await?;
        let copied = tokio::fs::copy(from, &tmp).await?;
        let file = tokio::fs::File::open(&tmp).await?;
        file.sync_all().await?;
        let expected = from_meta.len();
        if copied != expected || file.metadata().await?.len() != expected {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(std::io::Error::other(format!(
                "short copy of {}",
                from.display()
            )));
        }
        tokio::fs::rename(&tmp, to).await?;
        // A copy gets a fresh mtime; a move of the same file should never
        // look newer than the original just because it crossed filesystems.
        if let Ok(modified) = from_meta.modified() {
            let to = to.to_path_buf();
            tokio::task::spawn_blocking(move || {
                std::fs::File::options()
                    .write(true)
                    .open(&to)?
                    .set_modified(modified)
            })
            .await
            .expect("mtime setter panicked")?;
        }
        if move_file {
            tokio::fs::remove_file(from).await?;
        }
        Ok(())
    }
    .await;
    if result.is_err() {
        // Release the claim, but only if it is still the empty placeholder.
        if tokio::fs::metadata(to).await.is_ok_and(|m| m.len() == 0) {
            let _ = tokio::fs::remove_file(to).await;
        }
    }
    result
}

#[cfg(test)]
mod tests {

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
