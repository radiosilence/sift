//! Reading and writing tags.
//!
//! Tags are written through lofty's generic keys, which it maps to each
//! format's native frame — Vorbis comments for FLAC and Ogg, ID3v2 for MP3,
//! atoms for MP4 — so the same [`Tags`] lands correctly everywhere.

use std::path::{Path, PathBuf};
use std::time::Duration;

use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::tag::{Accessor, ItemKey, ItemValue, Tag, TagExt, TagItem};

#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    #[error("{path}: {source}")]
    Lofty {
        path: PathBuf,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("{0}: no tag could be created for this file type")]
    NoTag(PathBuf),
}

const AUDIO: &[&str] = &[
    "flac", "mp3", "m4a", "mp4", "aac", "alac", "ogg", "oga", "opus", "wav", "aiff", "aif", "ape",
    "wv",
];

pub fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO.contains(&e.to_ascii_lowercase().as_str()))
}

/// What a file says about itself.
#[derive(Debug, Clone, Default)]
pub struct Track {
    pub path: PathBuf,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track: Option<u32>,
    pub track_total: Option<u32>,
    pub disc: Option<u32>,
    pub disc_total: Option<u32>,
    pub date: Option<String>,
    pub original_date: Option<String>,
    /// Filed with the compilation template.
    pub compilation: bool,
    pub genre: Option<String>,
    /// Each artist the credit names, as Picard's `ARTISTS` holds them.
    pub artists: Vec<String>,
    pub album_artists: Vec<String>,
    pub mb_recording_id: Option<String>,
    pub mb_track_id: Option<String>,
    pub mb_album_id: Option<String>,
    pub mb_artist_ids: Vec<String>,
    pub mb_album_artist_ids: Vec<String>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub country: Option<String>,
    pub media: Option<String>,
    pub mb_release_group_id: Option<String>,
    pub duration: Duration,
    /// `FLAC`, `MP3`, `AAC`… as beets' `$format` spells it.
    pub format: String,
    pub bitrate: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
}

fn lofty<E: std::error::Error + Send + Sync + 'static>(
    path: &Path,
) -> impl FnOnce(E) -> MetaError + '_ {
    move |source| MetaError::Lofty {
        path: path.to_path_buf(),
        source: Box::new(source),
    }
}

/// lofty refuses any tag that would allocate past a global cap, 16 MB by
/// default — small enough that one record with 4000×4000 art in its Vorbis
/// comments fails to parse at all. The cap exists to stop a hostile file
/// exhausting memory, which 256 MB still does.
fn raise_allocation_limit() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        lofty::config::apply_global_options(
            lofty::config::GlobalOptions::new().allocation_limit(256 * 1024 * 1024),
        );
    });
}

/// Read one file's tags. A corrupt tag can panic inside the parser; that is
/// contained here and reported as this file's error, rather than taking down
/// whatever was reading a whole folder.
pub fn read(path: &Path) -> Result<Track, MetaError> {
    raise_allocation_limit();
    std::panic::catch_unwind(|| read_inner(path)).unwrap_or_else(|_| {
        Err(MetaError::Lofty {
            path: path.to_path_buf(),
            source: "the tag parser panicked on this file".into(),
        })
    })
}

fn read_inner(path: &Path) -> Result<Track, MetaError> {
    let file = lofty::probe::Probe::open(path)
        .map_err(lofty(path))?
        .options(ParseOptions::new().read_cover_art(false))
        .read()
        .map_err(lofty(path))?;
    let props = file.properties();
    let mut t = Track {
        path: path.to_path_buf(),
        duration: props.duration(),
        // An MP4 carries AAC or ALAC; lofty reports a bit depth only for
        // the lossless one, so that is what tells them apart here.
        format: match (file.file_type(), props.bit_depth()) {
            (lofty::file::FileType::Mp4, Some(_)) => "ALAC",
            (ft, _) => format_name(ft),
        }
        .to_string(),
        bitrate: props.audio_bitrate(),
        sample_rate: props.sample_rate(),
        bit_depth: props.bit_depth(),
        ..Default::default()
    };
    if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
        read_tag(&mut t, tag);
    }
    Ok(t)
}

fn read_tag(t: &mut Track, tag: &Tag) {
    let s = |k: ItemKey| {
        tag.get_string(k)
            .map(str::to_string)
            .filter(|v| !v.trim().is_empty())
    };
    t.title = tag.title().map(|c| c.to_string());
    t.artist = tag.artist().map(|c| c.to_string());
    t.album = tag.album().map(|c| c.to_string());
    t.album_artist = s(ItemKey::AlbumArtist);
    // Taggers write 0 for a number they lack, disc 0 for single-disc
    // releases; it means none.
    let some = |n: Option<u32>| n.filter(|n| *n > 0);
    t.track = some(tag.track());
    t.track_total = some(tag.track_total());
    t.disc = some(tag.disk());
    t.disc_total = some(tag.disk_total());
    t.date = s(ItemKey::RecordingDate).or_else(|| s(ItemKey::Year));
    t.original_date = s(ItemKey::OriginalReleaseDate);
    t.compilation = s(ItemKey::FlagCompilation).is_some_and(|v| v == "1");
    t.genre = tag.genre().map(|c| c.to_string());
    let all = |k: ItemKey| {
        tag.get_strings(k)
            .flat_map(|v| v.split('\0'))
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    t.artists = all(ItemKey::TrackArtists);
    // Vorbis comments may repeat ARTIST itself instead of carrying ARTISTS.
    if t.artists.is_empty() {
        let repeated = all(ItemKey::TrackArtist);
        if repeated.len() > 1 {
            t.artists = repeated;
        }
    }
    t.album_artists = all(ItemKey::AlbumArtists);
    t.mb_recording_id = s(ItemKey::MusicBrainzRecordingId);
    t.mb_track_id = s(ItemKey::MusicBrainzTrackId);
    t.mb_album_id = s(ItemKey::MusicBrainzReleaseId);
    t.mb_artist_ids = all(ItemKey::MusicBrainzArtistId);
    t.mb_album_artist_ids = all(ItemKey::MusicBrainzReleaseArtistId);
    t.label = s(ItemKey::Label);
    t.catalog_number = s(ItemKey::CatalogNumber);
    t.country = s(ItemKey::ReleaseCountry);
    t.media = s(ItemKey::OriginalMediaType);
    t.mb_release_group_id = s(ItemKey::MusicBrainzReleaseGroupId);
}

fn format_name(ft: lofty::file::FileType) -> &'static str {
    use lofty::file::FileType as F;
    match ft {
        F::Flac => "FLAC",
        F::Mpeg => "MP3",
        F::Mp4 => "AAC",
        F::Vorbis => "OGG",
        F::Opus => "Opus",
        F::Wav => "WAV",
        F::Aiff => "AIFF",
        F::Ape => "APE",
        F::WavPack => "WavPack",
        _ => "Unknown",
    }
}

/// Everything sift writes. `None` removes the field, so a retag never leaves
/// a stale value from the source behind.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tags {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub track: u32,
    pub track_total: u32,
    pub disc: u32,
    pub disc_total: u32,
    pub date: Option<String>,
    pub original_date: Option<String>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub country: Option<String>,
    pub media: Option<String>,
    pub compilation: bool,
    pub mb_recording_id: Option<String>,
    pub mb_track_id: Option<String>,
    pub mb_album_id: Option<String>,
    /// Each artist the track's credit names, and the album's, in credit
    /// order: what lets a player list a collaboration under every artist on
    /// it without parsing `artist`. Empty removes the field.
    pub artists: Vec<String>,
    pub album_artists: Vec<String>,
    pub mb_artist_ids: Vec<String>,
    pub mb_album_artist_ids: Vec<String>,
    pub mb_release_group_id: Option<String>,
}

/// Write `tags` (and a front cover, if given) into `path`, replacing
/// whatever was there. With `preserve_mtime`, the file's modification time
/// is restored afterwards, as beets' `importadded` plugin: retagging on
/// import should not make an old file look newly changed.
pub fn write(
    path: &Path,
    tags: &Tags,
    cover: Option<&[u8]>,
    preserve_mtime: bool,
) -> Result<(), MetaError> {
    raise_allocation_limit();
    let original_mtime = if preserve_mtime {
        path.metadata().ok().and_then(|m| m.modified().ok())
    } else {
        None
    };
    repair_id3v2_languages(path)?;
    let mut file = lofty::probe::Probe::open(path)
        .map_err(lofty(path))?
        .read()
        .map_err(lofty(path))?;
    let tag_type = file.primary_tag_type();
    if file.tag(tag_type).is_none() {
        file.insert_tag(Tag::new(tag_type));
    }
    let tag = file
        .tag_mut(tag_type)
        .ok_or_else(|| MetaError::NoTag(path.to_path_buf()))?;

    tag.set_title(tags.title.clone());
    tag.set_artist(tags.artist.clone());
    tag.set_album(tags.album.clone());
    tag.insert_text(ItemKey::AlbumArtist, tags.album_artist.clone());
    tag.set_track(tags.track);
    tag.set_track_total(tags.track_total);
    tag.set_disk(tags.disc);
    tag.set_disk_total(tags.disc_total);
    let optional = [
        (ItemKey::RecordingDate, &tags.date),
        (ItemKey::OriginalReleaseDate, &tags.original_date),
        (ItemKey::Label, &tags.label),
        (ItemKey::CatalogNumber, &tags.catalog_number),
        (ItemKey::ReleaseCountry, &tags.country),
        (ItemKey::OriginalMediaType, &tags.media),
        (ItemKey::MusicBrainzRecordingId, &tags.mb_recording_id),
        (ItemKey::MusicBrainzTrackId, &tags.mb_track_id),
        (ItemKey::MusicBrainzReleaseId, &tags.mb_album_id),
        (
            ItemKey::MusicBrainzReleaseGroupId,
            &tags.mb_release_group_id,
        ),
    ];
    tag.remove_key(ItemKey::Year);
    for (key, value) in optional {
        tag.remove_key(key);
        if let Some(v) = value {
            tag.insert_text(key, v.clone());
        }
    }
    set_credits(
        tag,
        &Credits {
            artists: &tags.artists,
            album_artists: &tags.album_artists,
            artist_ids: &tags.mb_artist_ids,
            album_artist_ids: &tags.mb_album_artist_ids,
        },
        false,
    );
    tag.remove_key(ItemKey::FlagCompilation);
    if tags.compilation {
        tag.insert_text(ItemKey::FlagCompilation, "1".into());
    }
    if let Some(data) = cover {
        tag.remove_picture_type(PictureType::CoverFront);
        tag.push_picture(front_cover(data));
    }
    tag.save_to_path(path, WriteOptions::default())
        .map_err(lofty(path))?;
    if let Some(modified) = original_mtime {
        std::fs::File::options()
            .write(true)
            .open(path)
            .and_then(|f| f.set_modified(modified))
            .map_err(lofty(path))?;
    }
    Ok(())
}

/// The multi-valued artist fields, each written one value per item: repeated
/// Vorbis comments, a null-separated ID3v2.4 `TXXX`, repeated MP4 atoms.
pub struct Credits<'a> {
    pub artists: &'a [String],
    pub album_artists: &'a [String],
    pub artist_ids: &'a [String],
    pub album_artist_ids: &'a [String],
}

/// With `keep_unset`, a field given no values is left as it is rather than
/// removed.
fn set_credits(tag: &mut Tag, credits: &Credits, keep_unset: bool) {
    for (key, values) in [
        (ItemKey::TrackArtists, credits.artists),
        (ItemKey::AlbumArtists, credits.album_artists),
        (ItemKey::MusicBrainzArtistId, credits.artist_ids),
        (
            ItemKey::MusicBrainzReleaseArtistId,
            credits.album_artist_ids,
        ),
    ] {
        if keep_unset && values.is_empty() {
            continue;
        }
        tag.remove_key(key);
        for v in values {
            tag.push(TagItem::new(key, ItemValue::Text(v.clone())));
        }
    }
}

/// Write only the multi-valued artist fields given values, leaving every
/// other tag as it is: for crediting each artist on files already filed.
pub fn write_credits(path: &Path, credits: &Credits) -> Result<(), MetaError> {
    raise_allocation_limit();
    repair_id3v2_languages(path)?;
    let mut file = lofty::probe::Probe::open(path)
        .map_err(lofty(path))?
        .read()
        .map_err(lofty(path))?;
    let tag_type = file.primary_tag_type();
    if file.tag(tag_type).is_none() {
        file.insert_tag(Tag::new(tag_type));
    }
    let tag = file
        .tag_mut(tag_type)
        .ok_or_else(|| MetaError::NoTag(path.to_path_buf()))?;
    set_credits(tag, credits, true);
    tag.save_to_path(path, WriteOptions::default())
        .map_err(lofty(path))?;
    Ok(())
}

/// A field to set, by its beets name, or to clear with `None`.
pub type Change = (String, Option<String>);

/// A field `modify` can change, by its beets name.
pub const EDITABLE: &[&str] = &[
    "title",
    "artist",
    "album",
    "albumartist",
    "track",
    "tracktotal",
    "disc",
    "disctotal",
    "date",
    "year",
    "original_date",
    "genre",
    "label",
    "catalognum",
    "comp",
    "artists",
    "albumartists",
];

/// A list field as typed: names separated by "; ".
fn split_list(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(';')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .collect()
}

impl Track {
    /// A field [`set`] can change, by its beets name, as the file has it.
    pub fn field(&self, name: &str) -> Option<String> {
        let num = |n: Option<u32>| n.map(|n| n.to_string());
        match name {
            "title" => self.title.clone(),
            "artist" => self.artist.clone(),
            "album" => self.album.clone(),
            "albumartist" => self.album_artist.clone(),
            "track" => num(self.track),
            "tracktotal" => num(self.track_total),
            "disc" => num(self.disc),
            "disctotal" => num(self.disc_total),
            "date" | "year" => self.date.clone(),
            "original_date" => self.original_date.clone(),
            "genre" => self.genre.clone(),
            "label" => self.label.clone(),
            "catalognum" => self.catalog_number.clone(),
            "comp" => self.compilation.then(|| "1".to_string()),
            "artists" => (!self.artists.is_empty()).then(|| self.artists.join("; ")),
            "albumartists" => {
                (!self.album_artists.is_empty()).then(|| self.album_artists.join("; "))
            }
            _ => None,
        }
    }

    /// The track as it would read after [`write`] with `tags`.
    pub fn written(&self, tags: &Tags) -> Track {
        let text = |s: &str| (!s.is_empty()).then(|| s.to_string());
        let num = |n: u32| (n > 0).then_some(n);
        Track {
            title: text(&tags.title),
            artist: text(&tags.artist),
            album: text(&tags.album),
            album_artist: text(&tags.album_artist),
            track: num(tags.track),
            track_total: num(tags.track_total),
            disc: num(tags.disc),
            disc_total: num(tags.disc_total),
            date: tags.date.clone(),
            original_date: tags.original_date.clone(),
            label: tags.label.clone(),
            catalog_number: tags.catalog_number.clone(),
            country: tags.country.clone(),
            media: tags.media.clone(),
            compilation: tags.compilation,
            mb_recording_id: tags.mb_recording_id.clone(),
            mb_track_id: tags.mb_track_id.clone(),
            mb_album_id: tags.mb_album_id.clone(),
            artists: tags.artists.clone(),
            album_artists: tags.album_artists.clone(),
            mb_artist_ids: tags.mb_artist_ids.clone(),
            mb_album_artist_ids: tags.mb_album_artist_ids.clone(),
            mb_release_group_id: tags.mb_release_group_id.clone(),
            ..self.clone()
        }
    }

    /// The track as it would read after [`set`] with `changes`: for working
    /// out what an edit changes ([`diff`]) and where it files an album
    /// before making it. Setting `artist` keeps the artist list; setting or
    /// clearing `artists` drops its MusicBrainz ids.
    pub fn apply(&mut self, changes: &[Change]) {
        for (field, value) in changes {
            let number = || value.as_deref().and_then(|v| v.parse::<u32>().ok());
            match field.as_str() {
                "title" => self.title = value.clone(),
                "artist" => self.artist = value.clone(),
                "album" => self.album = value.clone(),
                "albumartist" => self.album_artist = value.clone(),
                "track" => self.track = number(),
                "tracktotal" => self.track_total = number(),
                "disc" => self.disc = number(),
                "disctotal" => self.disc_total = number(),
                "date" | "year" => self.date = value.clone(),
                "original_date" => self.original_date = value.clone(),
                "genre" => self.genre = value.clone(),
                "label" => self.label = value.clone(),
                "catalognum" => self.catalog_number = value.clone(),
                "comp" => {
                    self.compilation = value.as_deref().is_some_and(|v| v == "1" || v == "true")
                }
                "artists" => {
                    self.artists = split_list(value.as_deref());
                    self.mb_artist_ids.clear();
                }
                "albumartists" => {
                    self.album_artists = split_list(value.as_deref());
                    self.mb_album_artist_ids.clear();
                }
                _ => {}
            }
        }
    }
}

/// One field a write would change in a file, by the name a person reads.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TagChange {
    pub field: &'static str,
    pub old: Option<String>,
    pub new: Option<String>,
}

/// Every field [`write`] with `new` would change in a file that reads as
/// `old`.
pub fn changes(old: &Track, new: &Tags) -> Vec<TagChange> {
    diff(old, &old.written(new))
}

/// Every tag field that differs between a file read as `old` and as `new`:
/// for a hand edit, `new` is `old` after [`Track::apply`]. Lists are
/// compared whole and shown joined by "; ".
pub fn diff(old: &Track, new: &Track) -> Vec<TagChange> {
    let num = |n: Option<u32>| n.map(|n| n.to_string());
    let list = |v: &[String]| (!v.is_empty()).then(|| v.join("; "));
    let flag = |b: bool| b.then(|| "1".to_string());
    let pairs: Vec<(&'static str, Option<String>, Option<String>)> = vec![
        ("title", old.title.clone(), new.title.clone()),
        ("artist", old.artist.clone(), new.artist.clone()),
        ("album", old.album.clone(), new.album.clone()),
        (
            "albumartist",
            old.album_artist.clone(),
            new.album_artist.clone(),
        ),
        ("track", num(old.track), num(new.track)),
        ("tracktotal", num(old.track_total), num(new.track_total)),
        ("disc", num(old.disc), num(new.disc)),
        ("disctotal", num(old.disc_total), num(new.disc_total)),
        ("date", old.date.clone(), new.date.clone()),
        (
            "original_date",
            old.original_date.clone(),
            new.original_date.clone(),
        ),
        ("genre", old.genre.clone(), new.genre.clone()),
        ("label", old.label.clone(), new.label.clone()),
        (
            "catalognum",
            old.catalog_number.clone(),
            new.catalog_number.clone(),
        ),
        ("country", old.country.clone(), new.country.clone()),
        ("media", old.media.clone(), new.media.clone()),
        ("comp", flag(old.compilation), flag(new.compilation)),
        ("artists", list(&old.artists), list(&new.artists)),
        (
            "albumartists",
            list(&old.album_artists),
            list(&new.album_artists),
        ),
        (
            "mb_trackid",
            old.mb_recording_id.clone(),
            new.mb_recording_id.clone(),
        ),
        (
            "mb_releasetrackid",
            old.mb_track_id.clone(),
            new.mb_track_id.clone(),
        ),
        (
            "mb_albumid",
            old.mb_album_id.clone(),
            new.mb_album_id.clone(),
        ),
        (
            "mb_artistids",
            list(&old.mb_artist_ids),
            list(&new.mb_artist_ids),
        ),
        (
            "mb_albumartistids",
            list(&old.mb_album_artist_ids),
            list(&new.mb_album_artist_ids),
        ),
        (
            "mb_releasegroupid",
            old.mb_release_group_id.clone(),
            new.mb_release_group_id.clone(),
        ),
    ];
    pairs
        .into_iter()
        .filter(|(_, o, n)| o != n)
        .map(|(field, old, new)| TagChange { field, old, new })
        .collect()
}

/// Set or clear only the named fields in `path`, leaving every other tag as
/// it is. [`write`] replaces everything sift knows about, which is right for
/// an import and wrong for correcting one field of a filed album.
pub fn set(path: &Path, changes: &[Change]) -> Result<(), MetaError> {
    raise_allocation_limit();
    repair_id3v2_languages(path)?;
    let mut file = lofty::probe::Probe::open(path)
        .map_err(lofty(path))?
        .read()
        .map_err(lofty(path))?;
    let tag_type = file.primary_tag_type();
    if file.tag(tag_type).is_none() {
        file.insert_tag(Tag::new(tag_type));
    }
    let tag = file
        .tag_mut(tag_type)
        .ok_or_else(|| MetaError::NoTag(path.to_path_buf()))?;
    set_tag(tag, changes);
    tag.save_to_path(path, WriteOptions::default())
        .map_err(lofty(path))?;
    Ok(())
}

fn set_tag(tag: &mut Tag, changes: &[Change]) {
    for (field, value) in changes {
        let number = || value.as_deref().and_then(|v| v.parse::<u32>().ok());
        if matches!(field.as_str(), "artist" | "artists") {
            settle_repeated_artist(tag);
        }
        match field.as_str() {
            "track" => match number() {
                Some(n) => tag.set_track(n),
                None => tag.remove_track(),
            },
            "tracktotal" => match number() {
                Some(n) => tag.set_track_total(n),
                None => tag.remove_track_total(),
            },
            "disc" => match number() {
                Some(n) => tag.set_disk(n),
                None => tag.remove_disk(),
            },
            "disctotal" => match number() {
                Some(n) => tag.set_disk_total(n),
                None => tag.remove_disk_total(),
            },
            "comp" => {
                tag.remove_key(ItemKey::FlagCompilation);
                if value.as_deref().is_some_and(|v| v == "1" || v == "true") {
                    tag.insert_text(ItemKey::FlagCompilation, "1".into());
                }
            }
            // A list of artists, "; " between them. Their MusicBrainz ids
            // go with them: ids for the old names would no longer line up.
            "artists" | "albumartists" => {
                let (names, ids) = if field == "artists" {
                    (ItemKey::TrackArtists, ItemKey::MusicBrainzArtistId)
                } else {
                    (ItemKey::AlbumArtists, ItemKey::MusicBrainzReleaseArtistId)
                };
                tag.remove_key(names);
                tag.remove_key(ids);
                for v in split_list(value.as_deref()) {
                    tag.push(TagItem::new(names, ItemValue::Text(v)));
                }
            }
            name => {
                let key = match name {
                    "title" => ItemKey::TrackTitle,
                    "artist" => ItemKey::TrackArtist,
                    "album" => ItemKey::AlbumTitle,
                    "albumartist" => ItemKey::AlbumArtist,
                    "date" | "year" => ItemKey::RecordingDate,
                    "original_date" => ItemKey::OriginalReleaseDate,
                    "genre" => ItemKey::Genre,
                    "label" => ItemKey::Label,
                    "catalognum" => ItemKey::CatalogNumber,
                    _ => continue,
                };
                tag.remove_key(key);
                // `read` falls back to the year, so a date set or cleared
                // leaves none behind.
                if matches!(name, "year" | "date") {
                    tag.remove_key(ItemKey::Year);
                }
                if let Some(v) = value {
                    tag.insert_text(key, v.clone());
                }
            }
        }
    }
}

/// [`read`] takes a repeated `ARTIST` as the artist list when there is no
/// `ARTISTS`. Before either is changed, the list moves to `ARTISTS` and
/// `ARTIST` keeps its first value, which reads the same: setting one then
/// leaves the other as it read, and clearing the list clears it.
fn settle_repeated_artist(tag: &mut Tag) {
    let repeated: Vec<String> = tag
        .get_strings(ItemKey::TrackArtist)
        .flat_map(|v| v.split('\0'))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .collect();
    if repeated.len() < 2 {
        return;
    }
    let listed = tag
        .get_strings(ItemKey::TrackArtists)
        .any(|v| v.split('\0').any(|v| !v.trim().is_empty()));
    if !listed {
        tag.remove_key(ItemKey::TrackArtists);
        for v in &repeated {
            tag.push(TagItem::new(
                ItemKey::TrackArtists,
                ItemValue::Text(v.clone()),
            ));
        }
    }
    tag.remove_key(ItemKey::TrackArtist);
    tag.insert_text(ItemKey::TrackArtist, repeated[0].clone());
}

/// Write ReplayGain track and album gain, leaving every other tag alone.
pub fn set_replaygain(
    path: &Path,
    track: crate::replaygain::Gain,
    album: crate::replaygain::Gain,
) -> Result<(), MetaError> {
    raise_allocation_limit();
    repair_id3v2_languages(path)?;
    let mut file = lofty::probe::Probe::open(path)
        .map_err(lofty(path))?
        .read()
        .map_err(lofty(path))?;
    let tag_type = file.primary_tag_type();
    if file.tag(tag_type).is_none() {
        file.insert_tag(Tag::new(tag_type));
    }
    let tag = file
        .tag_mut(tag_type)
        .ok_or_else(|| MetaError::NoTag(path.to_path_buf()))?;
    for (key, value) in [
        (ItemKey::ReplayGainTrackGain, format!("{:.2} dB", track.db)),
        (ItemKey::ReplayGainTrackPeak, format!("{:.6}", track.peak)),
        (ItemKey::ReplayGainAlbumGain, format!("{:.2} dB", album.db)),
        (ItemKey::ReplayGainAlbumPeak, format!("{:.6}", album.peak)),
    ] {
        tag.remove_key(key);
        tag.insert_text(key, value);
    }
    tag.save_to_path(path, WriteOptions::default())
        .map_err(lofty(path))?;
    Ok(())
}

/// Write lyrics, leaving every other tag alone.
pub fn set_lyrics(path: &Path, lyrics: &str) -> Result<(), MetaError> {
    raise_allocation_limit();
    repair_id3v2_languages(path)?;
    let mut file = lofty::probe::Probe::open(path)
        .map_err(lofty(path))?
        .read()
        .map_err(lofty(path))?;
    let tag_type = file.primary_tag_type();
    if file.tag(tag_type).is_none() {
        file.insert_tag(Tag::new(tag_type));
    }
    let tag = file
        .tag_mut(tag_type)
        .ok_or_else(|| MetaError::NoTag(path.to_path_buf()))?;
    tag.remove_key(ItemKey::Lyrics);
    tag.insert_text(ItemKey::Lyrics, lyrics.to_string());
    tag.save_to_path(path, WriteOptions::default())
        .map_err(lofty(path))?;
    Ok(())
}

/// Whether `path` already carries lyrics.
pub fn has_lyrics(path: &Path) -> bool {
    lofty::read_from_path(path).is_ok_and(|f| {
        f.primary_tag().or_else(|| f.first_tag()).is_some_and(|t| {
            t.get_string(ItemKey::Lyrics)
                .is_some_and(|l| !l.trim().is_empty())
        })
    })
}

/// Whether `path` already carries an album gain.
pub fn has_album_gain(path: &Path) -> bool {
    lofty::read_from_path(path).is_ok_and(|f| {
        f.primary_tag()
            .or_else(|| f.first_tag())
            .is_some_and(|t| t.get_string(ItemKey::ReplayGainAlbumGain).is_some())
    })
}

pub fn embedded_cover(path: &Path) -> Option<Vec<u8>> {
    let file = lofty::read_from_path(path).ok()?;
    let tag = file.primary_tag().or_else(|| file.first_tag())?;
    let pic = tag
        .pictures()
        .iter()
        .find(|p| p.pic_type() == PictureType::CoverFront)
        .or_else(|| tag.pictures().first())?;
    Some(pic.data().to_vec())
}

fn front_cover(data: &[u8]) -> Picture {
    let mime = if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        MimeType::Png
    } else {
        MimeType::Jpeg
    };
    Picture::unchecked(data.to_vec())
        .pic_type(PictureType::CoverFront)
        .mime_type(mime)
        .build()
}

/// A picture embedded in a file's tags.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedded {
    /// What the picture is for, as lofty names it: `CoverFront`, `Other`, …
    pub kind: String,
    pub mime: Option<String>,
    pub bytes: usize,
    /// Read from the image itself; absent when it cannot be decoded.
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Every picture embedded in `path`, from every tag it carries.
pub fn pictures(path: &Path) -> Result<Vec<Embedded>, MetaError> {
    raise_allocation_limit();
    let file = lofty::read_from_path(path).map_err(lofty(path))?;
    Ok(file
        .tags()
        .iter()
        .flat_map(|t| t.pictures())
        .map(|p| {
            let size = image::ImageReader::new(std::io::Cursor::new(p.data()))
                .with_guessed_format()
                .ok()
                .and_then(|r| r.into_dimensions().ok());
            Embedded {
                kind: format!("{:?}", p.pic_type()),
                mime: p.mime_type().map(|m| m.as_str().to_string()),
                bytes: p.data().len(),
                width: size.map(|(w, _)| w),
                height: size.map(|(_, h)| h),
            }
        })
        .collect())
}

/// Give every ID3v2 lyrics and comment frame in `path` whose language is
/// not three ASCII letters the unknown language, `XXX`. Some taggers write
/// a language of zero bytes; lofty reads such frames but refuses to write
/// them back, and keeps them out of reach of the generic tag, so every
/// change to the file would otherwise fail. Only MPEG files are checked,
/// and the file is written only when a frame needs repair.
fn repair_id3v2_languages(path: &Path) -> Result<(), MetaError> {
    use lofty::id3::v2::Frame;
    use lofty::mpeg::MpegFile;
    use lofty::tag::items::UNKNOWN_LANGUAGE;
    let is_mpeg = lofty::probe::Probe::open(path)
        .map_err(lofty(path))?
        .guess_file_type()
        .map_err(lofty(path))?
        .file_type()
        == Some(lofty::file::FileType::Mpeg);
    if !is_mpeg {
        return Ok(());
    }
    let mut file = std::fs::File::open(path).map_err(lofty(path))?;
    let mut mpeg = MpegFile::read_from(&mut file, ParseOptions::new()).map_err(lofty(path))?;
    let Some(tag) = mpeg.id3v2_mut() else {
        return Ok(());
    };
    let mut repaired = false;
    tag.retain_mut(|frame| {
        let language = match frame {
            Frame::Comment(f) => &mut f.language,
            Frame::UnsynchronizedText(f) => &mut f.language,
            _ => return true,
        };
        if !language.iter().all(u8::is_ascii_alphabetic) {
            *language = UNKNOWN_LANGUAGE;
            repaired = true;
        }
        true
    });
    if repaired {
        tag.save_to_path(path, WriteOptions::default())
            .map_err(lofty(path))?;
    }
    Ok(())
}

/// Remove every picture embedded in `path`, from every tag, leaving the rest
/// of the tags alone.
pub fn remove_pictures(path: &Path) -> Result<(), MetaError> {
    replace_pictures(path, None)
}

/// Make `cover` the only picture embedded in `path`, as its front cover.
pub fn set_cover(path: &Path, cover: &[u8]) -> Result<(), MetaError> {
    replace_pictures(path, Some(cover))
}

fn replace_pictures(path: &Path, cover: Option<&[u8]>) -> Result<(), MetaError> {
    raise_allocation_limit();
    repair_id3v2_languages(path)?;
    let mut file = lofty::probe::Probe::open(path)
        .map_err(lofty(path))?
        .read()
        .map_err(lofty(path))?;
    let types: Vec<_> = file.tags().iter().map(|t| t.tag_type()).collect();
    for t in types {
        if let Some(tag) = file.tag_mut(t) {
            while !tag.pictures().is_empty() {
                tag.remove_picture(0);
            }
        }
    }
    if let Some(data) = cover {
        let tag_type = file.primary_tag_type();
        if file.tag(tag_type).is_none() {
            file.insert_tag(Tag::new(tag_type));
        }
        file.tag_mut(tag_type)
            .ok_or_else(|| MetaError::NoTag(path.to_path_buf()))?
            .push_picture(front_cover(data));
    }
    file.save_to_path(path, WriteOptions::default())
        .map_err(lofty(path))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_fields_that_differ_are_changes() {
        let old = Track {
            title: Some("Kenneth".into()),
            artist: Some("R.E.M.".into()),
            track: Some(1),
            artists: vec!["R.E.M.".into()],
            ..Default::default()
        };
        let new = Tags {
            title: "What's the Frequency, Kenneth?".into(),
            artist: "R.E.M.".into(),
            track: 1,
            artists: vec!["R.E.M.".into()],
            mb_album_id: Some("rel".into()),
            ..Default::default()
        };
        let fields: Vec<_> = changes(&old, &new).iter().map(|c| c.field).collect();
        assert_eq!(fields, ["title", "mb_albumid"]);
    }

    #[test]
    fn an_edit_applies_as_set_would_write_it() {
        let mut t = Track::default();
        t.apply(&[
            ("album".into(), Some("Monster".into())),
            ("track".into(), Some("3".into())),
            ("comp".into(), Some("1".into())),
            ("genre".into(), None),
        ]);
        assert_eq!(t.field("album").as_deref(), Some("Monster"));
        assert_eq!(t.field("track").as_deref(), Some("3"));
        assert_eq!(t.field("comp").as_deref(), Some("1"));
        assert_eq!(t.field("genre"), None);
    }

    /// An MP3 whose ID3v2.3 tag holds one lyrics frame with a language of
    /// zero bytes, followed by a few silent MPEG frames.
    fn mp3_with_zero_language_lyrics(dir: &Path) -> PathBuf {
        let body: &[u8] = b"\x00\x00\x00\x00\x00words";
        let mut frame = b"USLT".to_vec();
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(body);
        let mut bytes = b"ID3\x03\x00\x00".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, frame.len() as u8]);
        bytes.extend_from_slice(&frame);
        for _ in 0..8 {
            bytes.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
            bytes.extend_from_slice(&[0; 413]);
        }
        let path = dir.join("track.mp3");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_file_whose_lyrics_have_a_language_of_zero_bytes_can_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = mp3_with_zero_language_lyrics(dir.path());
        remove_pictures(&path).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        let mpeg = lofty::mpeg::MpegFile::read_from(&mut file, ParseOptions::new()).unwrap();
        let lyrics: Vec<_> = mpeg.id3v2().unwrap().unsync_text().collect();
        assert_eq!(lyrics.len(), 1);
        assert_eq!(&lyrics[0].language, b"XXX");
        assert_eq!(lyrics[0].content, "words");
    }

    /// A Vorbis comment crediting each artist by repeating `ARTIST`.
    fn repeated_artist() -> Tag {
        let mut tag = Tag::new(lofty::tag::TagType::VorbisComments);
        for name in ["A", "B"] {
            tag.push(TagItem::new(
                ItemKey::TrackArtist,
                ItemValue::Text(name.into()),
            ));
        }
        tag
    }

    /// What `set` writes, read back, against what `apply` predicts.
    fn set_and_apply(changes: &[Change]) -> (Track, Track) {
        let mut tag = repeated_artist();
        let mut predicted = Track::default();
        read_tag(&mut predicted, &tag);
        predicted.apply(changes);
        set_tag(&mut tag, changes);
        let mut written = Track::default();
        read_tag(&mut written, &tag);
        (written, predicted)
    }

    #[test]
    fn a_repeated_artist_reads_as_the_list_set_and_apply_agree_on() {
        let (written, predicted) = set_and_apply(&[("artists".into(), None)]);
        assert!(written.artists.is_empty());
        assert_eq!(written.artist.as_deref(), Some("A"));
        assert!(diff(&written, &predicted).is_empty());

        let (written, predicted) = set_and_apply(&[("artist".into(), Some("A & B".into()))]);
        assert_eq!(written.artists, ["A", "B"]);
        assert_eq!(written.artist.as_deref(), Some("A & B"));
        assert!(diff(&written, &predicted).is_empty());

        let (written, predicted) = set_and_apply(&[("artists".into(), Some("C; D".into()))]);
        assert_eq!(written.artists, ["C", "D"]);
        assert!(diff(&written, &predicted).is_empty());
    }

    #[test]
    fn a_hand_edit_diffs_against_the_file() {
        let old = Track {
            album: Some("Monstre".into()),
            genre: Some("Rock".into()),
            ..Default::default()
        };
        let mut new = old.clone();
        new.apply(&[
            ("album".into(), Some("Monster".into())),
            ("genre".into(), None),
        ]);
        let changed: Vec<_> = diff(&old, &new)
            .into_iter()
            .map(|c| (c.field, c.old, c.new))
            .collect();
        assert_eq!(
            changed,
            [
                ("album", Some("Monstre".into()), Some("Monster".into())),
                ("genre", Some("Rock".into()), None),
            ]
        );
    }

    #[test]
    fn a_zero_total_reads_as_none_and_is_no_change() {
        let mut tag = Tag::new(lofty::tag::TagType::VorbisComments);
        tag.set_track(1);
        tag.set_track_total(0);
        tag.set_disk_total(0);
        let mut t = Track::default();
        read_tag(&mut t, &tag);
        assert_eq!((t.track_total, t.disc_total), (None, None));
        let tags = Tags {
            track: 1,
            ..Default::default()
        };
        assert!(
            !changes(&t, &tags)
                .iter()
                .any(|c| c.field.ends_with("total"))
        );
    }

    #[test]
    fn clearing_artists_takes_their_ids_and_clearing_a_date_takes_the_year() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        crate::library::tests::wav(&path, 44_100);
        write(
            &path,
            &Tags {
                title: "T".into(),
                artist: "A × B".into(),
                artists: vec!["A".into(), "B".into()],
                mb_artist_ids: vec!["a".into(), "b".into()],
                date: Some("2020-01-01".into()),
                ..Default::default()
            },
            None,
            false,
        )
        .unwrap();
        set(&path, &[("artists".into(), None), ("date".into(), None)]).unwrap();
        let t = read(&path).unwrap();
        assert!(t.artists.is_empty() && t.mb_artist_ids.is_empty());
        assert_eq!(t.date, None);
        set(&path, &[("artists".into(), Some("A; B ;".into()))]).unwrap();
        assert_eq!(read(&path).unwrap().artists, ["A", "B"]);
    }

    #[test]
    fn a_cover_set_replaces_every_picture_and_removing_leaves_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        crate::library::tests::wav(&path, 44_100);
        write(
            &path,
            &Tags {
                title: "T".into(),
                ..Default::default()
            },
            Some(b"not an image"),
            false,
        )
        .unwrap();
        let before = pictures(&path).unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!((before[0].width, before[0].bytes), (None, 12));

        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(3, 2)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        set_cover(&path, &png).unwrap();
        let after = pictures(&path).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].kind, "CoverFront");
        assert_eq!(after[0].mime.as_deref(), Some("image/png"));
        assert_eq!((after[0].width, after[0].height), (Some(3), Some(2)));
        assert_eq!(read(&path).unwrap().title.as_deref(), Some("T"));

        remove_pictures(&path).unwrap();
        assert!(pictures(&path).unwrap().is_empty());
        assert_eq!(read(&path).unwrap().title.as_deref(), Some("T"));
    }
}
