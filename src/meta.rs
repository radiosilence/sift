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
use lofty::tag::{Accessor, ItemKey, Tag, TagExt};

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
    pub mb_recording_id: Option<String>,
    pub mb_album_id: Option<String>,
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
        let s = |k: ItemKey| {
            tag.get_string(k)
                .map(str::to_string)
                .filter(|v| !v.trim().is_empty())
        };
        t.title = tag.title().map(|c| c.to_string());
        t.artist = tag.artist().map(|c| c.to_string());
        t.album = tag.album().map(|c| c.to_string());
        t.album_artist = s(ItemKey::AlbumArtist);
        t.track = tag.track();
        t.track_total = tag.track_total();
        // Taggers write disc 0 for single-disc releases; it means no disc.
        t.disc = tag.disk().filter(|d| *d > 0);
        t.disc_total = tag.disk_total();
        t.date = s(ItemKey::RecordingDate).or_else(|| s(ItemKey::Year));
        t.original_date = s(ItemKey::OriginalReleaseDate);
        t.compilation = s(ItemKey::FlagCompilation).is_some_and(|v| v == "1");
        t.genre = tag.genre().map(|c| c.to_string());
        t.mb_recording_id = s(ItemKey::MusicBrainzRecordingId);
        t.mb_album_id = s(ItemKey::MusicBrainzReleaseId);
    }
    Ok(t)
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
    pub mb_artist_id: Option<String>,
    pub mb_album_artist_id: Option<String>,
    pub mb_release_group_id: Option<String>,
}

/// Write `tags` (and a front cover, if given) into `path`, replacing
/// whatever was there.
pub fn write(path: &Path, tags: &Tags, cover: Option<&[u8]>) -> Result<(), MetaError> {
    raise_allocation_limit();
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
        (ItemKey::MusicBrainzArtistId, &tags.mb_artist_id),
        (
            ItemKey::MusicBrainzReleaseArtistId,
            &tags.mb_album_artist_id,
        ),
        (
            ItemKey::MusicBrainzReleaseGroupId,
            &tags.mb_release_group_id,
        ),
    ];
    for (key, value) in optional {
        tag.remove_key(key);
        if let Some(v) = value {
            tag.insert_text(key, v.clone());
        }
    }
    tag.remove_key(ItemKey::FlagCompilation);
    if tags.compilation {
        tag.insert_text(ItemKey::FlagCompilation, "1".into());
    }
    if let Some(data) = cover {
        tag.remove_picture_type(PictureType::CoverFront);
        let mime = if data.starts_with(&[0x89, b'P', b'N', b'G']) {
            MimeType::Png
        } else {
            MimeType::Jpeg
        };
        tag.push_picture(
            Picture::unchecked(data.to_vec())
                .pic_type(PictureType::CoverFront)
                .mime_type(mime)
                .build(),
        );
    }
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
];

/// Set or clear only the named fields in `path`, leaving every other tag as
/// it is. [`write`] replaces everything sift knows about, which is right for
/// an import and wrong for correcting one field of a filed album.
pub fn set(path: &Path, changes: &[Change]) -> Result<(), MetaError> {
    raise_allocation_limit();
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
    for (field, value) in changes {
        let number = || value.as_deref().and_then(|v| v.parse::<u32>().ok());
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
                if name == "year" {
                    tag.remove_key(ItemKey::Year);
                }
                if let Some(v) = value {
                    tag.insert_text(key, v.clone());
                }
            }
        }
    }
    tag.save_to_path(path, WriteOptions::default())
        .map_err(lofty(path))?;
    Ok(())
}

/// Write ReplayGain track and album gain, leaving every other tag alone.
pub fn set_replaygain(
    path: &Path,
    track: crate::replaygain::Gain,
    album: crate::replaygain::Gain,
) -> Result<(), MetaError> {
    raise_allocation_limit();
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
