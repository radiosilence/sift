//! A folder in, an album in the library out — or candidates for a person to
//! choose from.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::art::{self, ArtRules};
use crate::config::Config;
use crate::credits::{self, Known};
use crate::discogs::{Discogs, DiscogsError};
use crate::matching::{self, Match};
use crate::meta::{self, Tags, Track};
use crate::musicbrainz::credited;
use crate::musicbrainz::{MbError, MusicBrainz, Release};
use crate::paths;

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("no audio files in {0}")]
    Empty(PathBuf),
    #[error(transparent)]
    Meta(#[from] meta::MetaError),
    #[error(transparent)]
    MusicBrainz(#[from] MbError),
    #[error(transparent)]
    Discogs(#[from] DiscogsError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Path(#[from] crate::paths::PathError),
    #[error("{0}")]
    Conflict(String),
    /// The album is already filed at this directory: its destinations are
    /// taken, by files not close enough to these to be the same copy. Filed
    /// by the same rules into the same format's folder, it is the same album.
    #[error("{} is already in the library", .0.display())]
    Exists(PathBuf),
    /// The files' own tags do not describe one album well enough to file
    /// it by them.
    #[error("the files' tags do not describe one album: {0}")]
    Untagged(String),
}

impl ImportError {
    /// Whether the same import could succeed later without anything
    /// changing here.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::MusicBrainz(e) if e.is_transient())
            || matches!(self, Self::Discogs(e) if e.is_transient())
    }
}

/// A release that might be the album, for someone to choose between.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Candidate {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub date: Option<String>,
    pub country: Option<String>,
    pub media: Option<String>,
    pub disambiguation: Option<String>,
    pub tracks: usize,
    pub distance: f64,
    pub missing: usize,
    pub extra: usize,
}

impl From<&Match> for Candidate {
    fn from(m: &Match) -> Self {
        let r = &m.release;
        let mut formats: Vec<String> = r.media.iter().filter_map(|m| m.format.clone()).collect();
        formats.dedup();
        Self {
            id: r.id.clone(),
            title: r.title.clone(),
            artist: r.artist(),
            date: r.date.clone(),
            country: r.country.clone(),
            media: (!formats.is_empty())
                .then(|| format!("{}×{}", r.media.len(), formats.join("+"))),
            disambiguation: r.disambiguation.clone().filter(|d| !d.is_empty()),
            tracks: r.track_count(),
            distance: (m.distance * 1000.0).round() / 1000.0,
            missing: m.missing,
            extra: m.extra,
        }
    }
}

/// Where an import's files go relative to what is already filed.
#[derive(Clone, Copy)]
enum Filing<'a> {
    /// Into the library, refusing a destination another copy holds.
    New,
    /// Retagged, and moved only if their path changes: the album is
    /// already in the library.
    InPlace,
    /// Into the library, a copy already there moved into this bin first.
    Replacing(&'a Path),
}

/// Move an album folder out of the library into `bin`, at its path relative
/// to the library root, under a new name if the bin already holds one by
/// that name. Nothing is deleted: putting it back is a move, which
/// `journal` does if the import does not finish.
fn set_aside(
    dir: &Path,
    root: &Path,
    bin: &Path,
    journal: &mut paths::Journal,
) -> Result<PathBuf, ImportError> {
    let rel = dir
        .strip_prefix(root)
        .ok()
        .filter(|r| r.components().count() > 0)
        .ok_or_else(|| {
            ImportError::Conflict(format!(
                "{} is not an album folder in the library",
                dir.display()
            ))
        })?;
    let mut dest = bin.join(rel);
    let mut n = 1;
    while dest.exists() {
        n += 1;
        let name = format!(
            "{} (replaced {n})",
            rel.file_name().unwrap_or_default().to_string_lossy()
        );
        dest = bin.join(rel).with_file_name(name);
    }
    for entry in std::fs::read_dir(dir)?.flatten() {
        let from = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_file()) && !paths::is_scratch(&from) {
            journal.transfer(&from, &dest.join(entry.file_name()), true)?;
        }
    }
    let _ = std::fs::remove_dir(dir);
    Ok(dest)
}

#[derive(Debug)]
pub enum Outcome {
    Imported {
        dir: PathBuf,
        /// The release applied; none for an import as-is.
        release: Option<Candidate>,
        log: String,
    },
    /// Nothing was changed. `candidates` is best first; importing again with
    /// one of their ids applies it.
    Review {
        reason: String,
        candidates: Vec<Candidate>,
        log: String,
    },
}

/// Which service a release id belongs to. `discogs:<number>` ids are
/// namespaced; a MusicBrainz id is a UUID and never contains a colon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    MusicBrainz,
    Discogs,
}

pub fn source_of(id: &str) -> Source {
    if id.starts_with("discogs:") {
        Source::Discogs
    } else {
        Source::MusicBrainz
    }
}

pub struct Importer {
    pub cfg: Config,
    mb: MusicBrainz,
    discogs: Option<Discogs>,
    http: reqwest::Client,
    lyrics: crate::lyrics::Client,
}

/// What [`Importer::enrich`] added.
#[derive(Debug, Default)]
pub struct Enriched {
    pub gain_db: Option<f64>,
    pub genres: Vec<String>,
    /// Tracks given lyrics.
    pub lyrics: usize,
    /// Artists credited one by one where the files named them only together.
    pub artists: Vec<String>,
    pub problems: Vec<String>,
}

/// What [`Importer::credit`] wrote.
#[derive(Debug, Default)]
pub struct Credited {
    /// Files given artist credits.
    pub files: usize,
    /// Each artist credited, once.
    pub artists: Vec<String>,
    pub problems: Vec<String>,
}

/// What [`Importer::credit`] would write. See [`Importer::credit_plan`].
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct CreditPlan {
    pub files: Vec<PlannedCredit>,
    pub problems: Vec<String>,
}

/// The credits one file would be given. A list left empty is not written:
/// the file already has it, or nothing was found for it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PlannedCredit {
    pub path: PathBuf,
    pub artists: Vec<String>,
    pub artist_ids: Vec<String>,
    pub album_artists: Vec<String>,
    pub album_artist_ids: Vec<String>,
    /// The MusicBrainz release the credits come from; none when the files'
    /// own credit was split by the artists the library knows.
    pub release: Option<String>,
}

/// What [`Importer::enrich`] would add. See [`Importer::enrich_plan`].
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct EnrichPlan {
    pub credits: CreditPlan,
    /// Written to every track; empty when the album has genres already or
    /// MusicBrainz has none.
    pub genres: Vec<String>,
    /// Each track that has none and would be given lyrics.
    pub lyrics: Vec<PlannedLyrics>,
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PlannedLyrics {
    pub path: PathBuf,
    /// Timed lines (LRC) rather than plain text.
    pub synced: bool,
    pub text: String,
}

#[derive(Default)]
struct Lookups {
    genres: Vec<String>,
    lyrics: Vec<PlannedLyrics>,
    problems: Vec<String>,
}

/// What re-tagging an album from one release would do. See
/// [`Importer::retag_plan`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct Retag {
    pub comparison: Comparison,
    pub files: Vec<RetagFile>,
    /// Files matched to no track of the release, and second copies of a
    /// track, left as they are.
    pub untouched: Vec<PathBuf>,
    /// Whether every file's front cover would be replaced by the one found
    /// for the release (or the album's own, when none is), as `fetch_art`
    /// does.
    pub cover: bool,
    /// The album's other files (cover, cue, log), which follow it when it
    /// changes directory.
    pub companions: Vec<Companion>,
    /// Why applying it would be refused: two files on one path, a path
    /// another file already holds, or a path that cannot be made.
    pub refused: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RetagFile {
    pub path: PathBuf,
    /// Where it would be filed, its own path when it stays; `None` when no
    /// path could be made for it.
    pub to: Option<PathBuf>,
    pub changes: Vec<meta::TagChange>,
}

/// A file beside the album's tracks, and where it would go.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Companion {
    pub from: PathBuf,
    pub to: PathBuf,
    /// Something is already at `to`, so this one stays where it is.
    pub stays: bool,
}

/// Where one matched file would be filed.
struct Destination {
    local: usize,
    tags: Tags,
    to: PathBuf,
    /// `to` is the file's own path, differing at most in case or Unicode
    /// form: it is retagged where it is.
    stays: bool,
    /// Another file already holds `to`.
    occupied: bool,
}

/// What [`Importer::reimport`] would match an album to. See
/// [`Importer::rematch`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct Rematch {
    /// The closest release and how the files line up against it.
    pub best: Option<Comparison>,
    /// Why a re-import without a release id would stop for a person to
    /// choose rather than apply `best`.
    pub review: Option<String>,
    /// Every release considered, best first.
    pub candidates: Vec<Candidate>,
}

/// How many search hits are looked up in full. Each is a rate-limited
/// request, so this is the time an import spends at MusicBrainz.
const LOOKUPS: usize = 5;

impl Importer {
    pub fn new(cfg: Config) -> Self {
        let mut mb = MusicBrainz::new(&cfg.musicbrainz_contact);
        if let Some(dir) = &cfg.cache_dir {
            mb = mb.with_cache(dir.join("musicbrainz"));
        }
        Self::with_musicbrainz(cfg, mb)
    }

    pub fn with_musicbrainz(cfg: Config, mb: MusicBrainz) -> Self {
        let discogs = cfg.discogs.as_ref().map(|d| {
            let discogs = Discogs::new(&d.token, &cfg.musicbrainz_contact);
            match &cfg.cache_dir {
                Some(dir) => discogs.with_cache(dir.clone()),
                None => discogs,
            }
        });
        Self::with_clients(cfg, mb, discogs)
    }

    fn with_clients(cfg: Config, mb: MusicBrainz, discogs: Option<Discogs>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(format!(
                "sift/{} ( {} )",
                env!("CARGO_PKG_VERSION"),
                cfg.musicbrainz_contact
            ))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("static client config");
        Self {
            cfg,
            mb,
            discogs,
            http,
            lyrics: crate::lyrics::Client::new(),
        }
    }

    /// Match and import `dir`. With `release_id`, that release is applied
    /// whatever its distance — the way a person answers a review.
    pub async fn import(
        &self,
        dir: &Path,
        release_id: Option<&str>,
    ) -> Result<Outcome, ImportError> {
        self.import_with(dir, release_id, Filing::New).await
    }

    /// [`Importer::import`], replacing a copy already filed: a folder these
    /// files would land in is moved into `bin` first, at its path relative
    /// to the library, rather than refusing the import or taking the files
    /// for a repeat of it. For a new copy fetched because the filed one is
    /// damaged, which looks the same as a repeat by format and length.
    pub async fn import_replacing(
        &self,
        dir: &Path,
        release_id: Option<&str>,
        bin: &Path,
    ) -> Result<Outcome, ImportError> {
        self.import_with(dir, release_id, Filing::Replacing(bin))
            .await
    }

    /// Credit each artist on the album's files that name only one credit
    /// (`ARTISTS`, `ALBUMARTISTS` and their MusicBrainz ids), leaving files
    /// that already carry them alone. A release on MusicBrainz names its
    /// artists itself; otherwise a credit is split only where
    /// [`credits::split`] finds every part among the `known` artists.
    /// `artist` and `albumartist` are never changed. Writes what
    /// [`Importer::credit_plan`] lists.
    pub async fn credit(&self, dir: &Path, known: &Known) -> Result<Credited, ImportError> {
        let plan = self.credit_plan(dir, known).await?;
        let mut out = Credited {
            problems: plan.problems,
            ..Credited::default()
        };
        let files = plan.files;
        let written = tokio::task::spawn_blocking(move || {
            files
                .into_iter()
                .map(|c| {
                    let credits = meta::Credits {
                        artists: &c.artists,
                        artist_ids: &c.artist_ids,
                        album_artists: &c.album_artists,
                        album_artist_ids: &c.album_artist_ids,
                    };
                    let result = meta::write_credits(&c.path, &credits);
                    (c, result)
                })
                .collect::<Vec<_>>()
        })
        .await
        .expect("tag writer panicked");
        for (c, result) in written {
            match result {
                Ok(()) => {
                    out.files += 1;
                    for a in c.artists.iter().chain(&c.album_artists) {
                        if !out.artists.contains(a) {
                            out.artists.push(a.clone());
                        }
                    }
                }
                Err(e) => out.problems.push(e.to_string()),
            }
        }
        Ok(out)
    }

    /// What [`Importer::credit`] would write to each file of the album, and
    /// where each credit comes from, without writing anything. Files that
    /// already carry their credits, or whose credit cannot be split, are
    /// not listed.
    pub async fn credit_plan(&self, dir: &Path, known: &Known) -> Result<CreditPlan, ImportError> {
        let tracks = read_dir(dir).await?;
        self.credit_plan_for(dir, &tracks, known).await
    }

    async fn credit_plan_for(
        &self,
        dir: &Path,
        tracks: &[Track],
        known: &Known,
    ) -> Result<CreditPlan, ImportError> {
        let mut out = CreditPlan::default();
        let pending = |t: &Track| t.artists.is_empty() || t.album_artists.is_empty();
        if !tracks.iter().any(pending) {
            return Ok(out);
        }
        let release = match tracks.iter().find_map(|t| t.mb_album_id.clone()) {
            Some(id) if source_of(&id) == Source::MusicBrainz => match self.mb.release(&id).await {
                Ok(r) => Some(r),
                Err(e) => {
                    out.problems.push(format!("artist credits: {e}"));
                    return Ok(out);
                }
            },
            _ => None,
        };
        for t in tracks.iter().filter(|t| pending(t)) {
            let empty = Vec::new;
            let (artists, artist_ids, album_artists, album_artist_ids) = match &release {
                Some(r) => {
                    let rt = r.tracks().map(|(_, rt)| rt).find(|rt| {
                        t.mb_track_id.as_ref() == Some(&rt.id)
                            || t.mb_recording_id.as_ref() == Some(&rt.recording.id)
                    });
                    // A file matched to no track of the release takes the
                    // release's artists, unless the release is a compilation,
                    // whose tracks each have their own.
                    let (artists, artist_ids) = match rt {
                        Some(rt) if !rt.artist_credit.is_empty() => {
                            credited(&rt.artist_credit).unzip()
                        }
                        None if r.is_compilation() => (empty(), empty()),
                        _ => credited(&r.artist_credit).unzip(),
                    };
                    let (album_artists, album_artist_ids) = credited(&r.artist_credit).unzip();
                    (artists, artist_ids, album_artists, album_artist_ids)
                }
                None => {
                    let split = |credit: Option<&str>| {
                        credit
                            .and_then(|c| credits::split(c, known, dir))
                            .unwrap_or_default()
                    };
                    let album_artist = t.album_artist.as_deref().or(t.artist.as_deref());
                    (
                        split(t.artist.as_deref()),
                        empty(),
                        split(album_artist),
                        empty(),
                    )
                }
            };
            let keep =
                |had: &Vec<String>, new: Vec<String>| if had.is_empty() { new } else { empty() };
            let artists = keep(&t.artists, artists);
            let album_artists = keep(&t.album_artists, album_artists);
            if artists.is_empty() && album_artists.is_empty() {
                continue;
            }
            out.files.push(PlannedCredit {
                path: t.path.clone(),
                artist_ids: if artists.is_empty() {
                    empty()
                } else {
                    artist_ids
                },
                album_artist_ids: if album_artists.is_empty() {
                    empty()
                } else {
                    album_artist_ids
                },
                artists,
                album_artists,
                release: release.as_ref().map(|r| r.id.clone()),
            });
        }
        Ok(out)
    }

    /// What beets' `replaygain`, `lastgenre` and `lyrics` plugins add after
    /// an import: album and track gain, genres from MusicBrainz when the
    /// album has none, and lyrics for tracks without them; and each artist
    /// on a collaboration credited one by one ([`Importer::credit`]). Each
    /// is written only to its own tags. Each file is written once, with
    /// everything found for it; a file that cannot be written is reported
    /// with what it was not given, and the rest are still written.
    pub async fn enrich(&self, dir: &Path, known: &Known) -> Result<Enriched, ImportError> {
        let tracks = read_dir(dir).await?;
        let credits = self.credit_plan_for(dir, &tracks, known).await?;
        let mut out = Enriched {
            problems: credits.problems,
            ..Enriched::default()
        };
        let paths: Vec<PathBuf> = tracks.iter().map(|t| t.path.clone()).collect();

        let measured = {
            let paths = paths.clone();
            tokio::task::spawn_blocking(move || crate::replaygain::album(&paths, 2))
                .await
                .expect("replaygain panicked")
        };
        let gains: Vec<Option<_>> = match measured {
            Ok((gains, album)) => {
                out.gain_db = Some(album.db);
                gains.into_iter().map(|g| Some((g, album))).collect()
            }
            Err(e) => {
                out.problems.push(format!("replaygain: {e}"));
                vec![None; paths.len()]
            }
        };

        let found = self.lookups(&tracks).await;
        out.problems.extend(found.problems);
        let genre: Vec<meta::Change> = if found.genres.is_empty() {
            Vec::new()
        } else {
            vec![("genre".into(), Some(found.genres.join("; ")))]
        };
        out.genres = found.genres;
        let lyrics = found.lyrics;
        let files = credits.files;

        let written = tokio::task::spawn_blocking(move || {
            paths
                .iter()
                .zip(gains)
                .filter_map(|(p, gain)| {
                    let credit = files.iter().find(|c| &c.path == p);
                    let lyric = lyrics.iter().find(|l| &l.path == p);
                    let credits = credit.map(|c| meta::Credits {
                        artists: &c.artists,
                        artist_ids: &c.artist_ids,
                        album_artists: &c.album_artists,
                        album_artist_ids: &c.album_artist_ids,
                    });
                    let edit = meta::Edit {
                        credits: credits.as_ref(),
                        replaygain: gain,
                        changes: &genre,
                        lyrics: lyric.map(|l| l.text.as_str()),
                    };
                    let fields: Vec<&str> = [
                        (edit.credits.is_some(), "artist credits"),
                        (edit.replaygain.is_some(), "gain"),
                        (!edit.changes.is_empty(), "genre"),
                        (edit.lyrics.is_some(), "lyrics"),
                    ]
                    .into_iter()
                    .filter_map(|(on, name)| on.then_some(name))
                    .collect();
                    if fields.is_empty() {
                        return None;
                    }
                    let result = meta::apply(p, &edit)
                        .map_err(|e| format!("{e} (not written: {})", fields.join(", ")));
                    let artists: Vec<String> = credit
                        .map(|c| c.artists.iter().chain(&c.album_artists).cloned().collect())
                        .unwrap_or_default();
                    Some((result, artists, lyric.is_some()))
                })
                .collect::<Vec<_>>()
        })
        .await
        .expect("tag writer panicked");
        for (result, artists, lyric) in written {
            match result {
                Ok(()) => {
                    out.lyrics += usize::from(lyric);
                    for a in artists {
                        if !out.artists.contains(&a) {
                            out.artists.push(a);
                        }
                    }
                }
                Err(e) => out.problems.push(e),
            }
        }
        Ok(out)
    }

    /// What [`Importer::enrich`] would add to the album, without writing
    /// anything: the artist credits, the genres and each track's lyrics,
    /// looked up as enriching would. Gain is not measured: that takes
    /// decoding every file, and is only worth doing to write it.
    pub async fn enrich_plan(&self, dir: &Path, known: &Known) -> Result<EnrichPlan, ImportError> {
        let credits = self.credit_plan(dir, known).await?;
        let tracks = read_dir(dir).await?;
        let found = self.lookups(&tracks).await;
        let mut problems = credits.problems.clone();
        problems.extend(found.problems);
        Ok(EnrichPlan {
            credits,
            genres: found.genres,
            lyrics: found.lyrics,
            problems,
        })
    }

    /// Genres from MusicBrainz when no track has any, and lyrics for each
    /// track without them.
    async fn lookups(&self, tracks: &[Track]) -> Lookups {
        let mut out = Lookups::default();
        let untagged = tracks
            .iter()
            .all(|t| t.genre.as_deref().is_none_or(|g| g.trim().is_empty()));
        if untagged && let Some(id) = tracks.iter().find_map(|t| t.mb_album_id.clone()) {
            match self.mb.genres(&id).await {
                Ok(g) => out.genres = g,
                Err(e) => out.problems.push(format!("genres: {e}")),
            }
        }
        // Only a track with an artist and a title can be looked up.
        let wanted: Vec<(&Track, &String, &String)> = tracks
            .iter()
            .filter_map(|t| Some((t, t.artist.as_ref()?, t.title.as_ref()?)))
            .collect();
        let paths: Vec<PathBuf> = wanted.iter().map(|(t, _, _)| t.path.clone()).collect();
        let has: Vec<bool> = tokio::task::spawn_blocking(move || {
            paths.iter().map(|p| meta::has_lyrics(p)).collect()
        })
        .await
        .expect("tag reader panicked");
        for ((t, artist, title), has) in wanted.into_iter().zip(has) {
            if has {
                continue;
            }
            let album = t.album.as_deref().unwrap_or("");
            match self
                .lyrics
                .get(artist, title, album, t.duration.as_secs())
                .await
            {
                Ok(crate::lyrics::Lyrics::Synced(text)) => out.lyrics.push(PlannedLyrics {
                    path: t.path.clone(),
                    synced: true,
                    text,
                }),
                Ok(crate::lyrics::Lyrics::Plain(text)) => out.lyrics.push(PlannedLyrics {
                    path: t.path.clone(),
                    synced: false,
                    text,
                }),
                Ok(_) => {}
                Err(e) => out.problems.push(format!("lyrics: {e}")),
            }
        }
        out
    }

    /// Match an album already in the library again, as beets' `import -L`
    /// does: retag it, and move it if the new tags file it elsewhere. A
    /// file that stays where it is is retagged in place, and the album's
    /// other files (cover, cue, log) follow it to a new directory.
    pub async fn reimport(
        &self,
        dir: &Path,
        release_id: Option<&str>,
    ) -> Result<Outcome, ImportError> {
        self.import_with(dir, release_id, Filing::InPlace).await
    }

    async fn import_with(
        &self,
        dir: &Path,
        release_id: Option<&str>,
        filing: Filing<'_>,
    ) -> Result<Outcome, ImportError> {
        let mut log = String::new();
        let tracks = read_dir(dir).await?;
        let _ = writeln!(log, "{} audio files in {}", tracks.len(), dir.display());
        let (tracks, spare) = one_copy_each(tracks);
        for t in &spare {
            let _ = writeln!(
                log,
                "left behind (another copy of the same track): {}",
                t.path.display()
            );
        }

        let mut matches = match release_id {
            Some(id) => vec![matching::score(&tracks, &self.release(id).await?)],
            None => self.candidates(dir, &tracks, &mut log).await?,
        };
        matches.sort_by(|a, b| a.distance.total_cmp(&b.distance));
        let Some(best) = matches.first() else {
            return Ok(Outcome::Review {
                reason: "no candidates found on MusicBrainz".into(),
                candidates: Vec::new(),
                log,
            });
        };
        let _ = writeln!(
            log,
            "best: {} — {} ({}), distance {:.3} ({})",
            best.release.artist(),
            best.release.title,
            best.release.id,
            best.distance,
            best.parts
        );

        if release_id.is_none()
            && let Some(reason) = self.review_reason(best)
        {
            return Ok(Outcome::Review {
                reason,
                candidates: matches.iter().take(5).map(Candidate::from).collect(),
                log,
            });
        }
        let best = best.clone();
        let dest = self.apply(&tracks, &best, dir, filing, &mut log).await?;
        Ok(Outcome::Imported {
            dir: dest,
            release: Some(Candidate::from(&best)),
            log,
        })
    }

    /// File the album by the files' own tags, without MusicBrainz: for a
    /// release it does not have. Refused, saying why, unless the tags
    /// describe one album: an album and an artist every file agrees on, and
    /// a title and a distinct track number for each file (taken from the
    /// file name where a tag is missing).
    pub async fn import_as_is(&self, dir: &Path, edits: &Edits) -> Result<Outcome, ImportError> {
        let mut log = String::new();
        let mut tracks = read_dir(dir).await?;
        edits.apply(&mut tracks)?;
        let _ = writeln!(log, "{} audio files in {}", tracks.len(), dir.display());
        let (tracks, spare) = one_copy_each(tracks);
        for t in &spare {
            let _ = writeln!(
                log,
                "left behind (another copy of the same track): {}",
                t.path.display()
            );
        }
        let mut entries = as_is_tags(&tracks, self.cfg.ft_in_title.as_ref())?;
        // The files carry no date the tagger trusts: MusicBrainz may still
        // have a year for this album and artist, even without a matching
        // release, as beets' `yearfixer` plugin looks one up.
        if let Some(first) = entries.first().map(|(_, t)| t.clone())
            && first.date.is_none()
        {
            match self
                .mb
                .release_group_year(&first.album_artist, &first.album)
                .await
            {
                Ok(Some(year)) => {
                    let _ = writeln!(log, "year {year} found on MusicBrainz for {}", first.album);
                    for (_, t) in &mut entries {
                        t.date = Some(year.clone());
                        t.original_date = Some(year.clone());
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = writeln!(log, "musicbrainz year lookup failed: {e}");
                }
            }
        }
        let _ = writeln!(log, "as-is: filed by the files' own tags");
        let dest = self
            .file(&tracks, &entries, None, dir, Filing::New, &mut log)
            .await?;
        Ok(Outcome::Imported {
            dir: dest,
            release: None,
            log,
        })
    }

    /// Whether `import_as_is` would accept the folder as its tags stand,
    /// and if not, why: the same check, without filing anything.
    pub async fn check_as_is(&self, dir: &Path) -> Result<(), ImportError> {
        let (tracks, _) = one_copy_each(read_dir(dir).await?);
        as_is_tags(&tracks, self.cfg.ft_in_title.as_ref()).map(|_| ())
    }

    /// The folder's files and the tags they carry, as an import would read
    /// them.
    pub async fn tracks(&self, dir: &Path) -> Result<Vec<Track>, ImportError> {
        read_dir(dir).await
    }

    /// How the folder lines up against one release, track by track: what a
    /// person or assistant needs to decide whether it is the right one, and
    /// what differs if it nearly is.
    pub async fn compare(&self, dir: &Path, release_id: &str) -> Result<Comparison, ImportError> {
        let (tracks, _) = one_copy_each(read_dir(dir).await?);
        let release = self.release(release_id).await?;
        Ok(comparison(&tracks, &matching::score(&tracks, &release)))
    }

    /// What [`Importer::reimport`] without a release id would match the
    /// album to, changing nothing: the closest release found, the files
    /// against it track by track, and why the re-import would stop for a
    /// choice rather than apply it.
    pub async fn rematch(&self, dir: &Path) -> Result<Rematch, ImportError> {
        let (tracks, _) = one_copy_each(read_dir(dir).await?);
        let mut matches = self.candidates(dir, &tracks, &mut String::new()).await?;
        matches.sort_by(|a, b| a.distance.total_cmp(&b.distance));
        Ok(Rematch {
            best: matches.first().map(|m| comparison(&tracks, m)),
            review: match matches.first() {
                Some(m) => self.review_reason(m),
                None => Some("no candidates found on MusicBrainz".into()),
            },
            candidates: matches.iter().take(5).map(Candidate::from).collect(),
        })
    }

    /// What [`Importer::reimport`] with `release_id` would do to an album
    /// already in the library, changing nothing: every field it would
    /// change in each file, where each file would go, the files it would
    /// leave untouched, and why it would be refused, if it would.
    pub async fn retag_plan(&self, dir: &Path, release_id: &str) -> Result<Retag, ImportError> {
        let (tracks, spare) = one_copy_each(read_dir(dir).await?);
        let release = self.release(release_id).await?;
        let m = matching::score(&tracks, &release);
        let remote: Vec<_> = release.tracks().collect();
        let entries: Vec<(usize, Tags)> = m
            .pairs
            .iter()
            .map(|&(l, r)| {
                let (medium, rt) = remote[r];
                (l, self.tags(&release, r, medium.position, rt))
            })
            .collect();
        let planned = self.destinations(&tracks, &entries, Some(&release), true);
        // A refused plan still shows each file's changes, and the path each
        // would take where one could be made, so the clash can be fixed.
        let mut listings = paths::Listings::default();
        let files = entries
            .iter()
            .enumerate()
            .map(|(i, (l, tags))| {
                let local = &tracks[*l];
                let to = match &planned {
                    Ok(plan) => Some((plan[i].to.clone(), plan[i].stays)),
                    Err(_) => self
                        .destination(local, tags, Some(&release), &mut listings)
                        .ok()
                        .map(|to| (to.clone(), same_file(&local.path, &to))),
                };
                RetagFile {
                    path: local.path.clone(),
                    to: to.map(|(to, stays)| if stays { local.path.clone() } else { to }),
                    changes: meta::changes(local, tags),
                }
            })
            .collect();
        let refused = match &planned {
            Ok(plan) => plan
                .iter()
                .find(|d| d.occupied)
                .map(|d| format!("{} is already in the library", d.to.display())),
            Err(e) => Some(e.to_string()),
        };
        let companions = match &planned {
            Ok(plan) => plan
                .first()
                .and_then(|d| d.to.parent())
                .map(|new_dir| companions(dir, new_dir))
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        Ok(Retag {
            comparison: comparison(&tracks, &m),
            files,
            untouched: unmatched(&tracks, &entries)
                .chain(&spare)
                .map(|t| t.path.clone())
                .collect(),
            cover: self.cfg.fetch_art,
            companions,
            refused,
        })
    }

    /// Why the closest match is not applied without a person choosing it.
    fn review_reason(&self, best: &Match) -> Option<String> {
        if best.distance > self.cfg.strong_threshold {
            Some(format!(
                "closest match is distance {:.3}, above {:.3}",
                best.distance, self.cfg.strong_threshold
            ))
        } else if !best.is_complete() {
            Some(format!(
                "{} tracks missing, {} files unmatched",
                best.missing, best.extra
            ))
        } else {
            None
        }
    }

    /// A release by id, dispatched to whichever service it names.
    async fn release(&self, id: &str) -> Result<Release, ImportError> {
        match source_of(id) {
            Source::MusicBrainz => Ok(self.mb.release(id).await?),
            Source::Discogs => {
                let num: u64 = id
                    .trim_start_matches("discogs:")
                    .parse()
                    .map_err(|_| ImportError::Conflict(format!("{id:?} is not a valid id")))?;
                let discogs = self
                    .discogs
                    .as_ref()
                    .ok_or_else(|| ImportError::Conflict("discogs is not configured".into()))?;
                let index_tracks = self.cfg.discogs.as_ref().is_some_and(|d| d.index_tracks);
                Ok(discogs.release(num, index_tracks).await?)
            }
        }
    }

    async fn candidates(
        &self,
        dir: &Path,
        tracks: &[Track],
        log: &mut String,
    ) -> Result<Vec<Match>, ImportError> {
        let mut releases: Vec<Release> = Vec::new();
        // Files that already name their release are the strongest evidence
        // there is.
        if let Some(id) = matching::consensus(tracks.iter().map(|t| t.mb_album_id.as_deref())) {
            let _ = writeln!(log, "files are tagged with release {id}");
            if let Ok(r) = self.mb.release(&id).await {
                releases.push(r);
            }
        }
        let (artist, album) = self.query_terms(dir, tracks);
        // The plain title first: an edition suffix is in the tags far more
        // often than in MusicBrainz's title. The title as tagged is the
        // fallback, for the release whose edition really is in its name.
        let base = matching::base_title(&album);
        let mut hits = Vec::new();
        for title in std::iter::once(base.as_str()).chain((base != album).then_some(album.as_str()))
        {
            let _ = writeln!(log, "searching for {artist:?} — {title:?}");
            hits = self.mb.search_releases(&artist, title, 10).await?;
            if hits.iter().any(|h| h.score >= 50) {
                break;
            }
        }
        // A credit spelled differently from MusicBrainz's ("x" for "×")
        // fails the artist phrase; the plain title by any of the artist's
        // words still finds it, and distance decides. Only the top-scoring
        // hits are kept: one sharing a single word of the credit scores
        // about half, and the sort below would otherwise put it first.
        let mut floor = 50;
        if !hits.iter().any(|h| h.score >= floor) {
            let _ = writeln!(log, "searching for {base:?} by any of {artist:?}");
            hits = self.mb.search_releases_loosely(&artist, &base, 10).await?;
            floor = 90;
        }
        // An album usually has many pressings scoring alike; the ones with
        // as many tracks as the folder are looked up first, since the
        // lookups run out before the list does.
        hits.retain(|h| h.score >= floor);
        hits.sort_by_key(|h| h.track_count.abs_diff(tracks.len()));
        let mut looked_up = 0;
        for hit in hits {
            if looked_up == LOOKUPS {
                break;
            }
            if releases.iter().any(|r| r.id == hit.id) {
                continue;
            }
            releases.push(self.mb.release(&hit.id).await?);
            looked_up += 1;
        }
        let mut matches: Vec<Match> = releases
            .iter()
            .map(|r| matching::score(tracks, r))
            .collect();

        // MusicBrainz found nothing worth applying on its own: ask Discogs
        // for the same artist and album, and let the two compete on distance.
        let best = matches
            .iter()
            .map(|m| m.distance)
            .fold(f64::INFINITY, f64::min);
        if let Some(discogs) = &self.discogs
            && best > self.cfg.strong_threshold
        {
            let _ = writeln!(log, "searching discogs for {artist:?} — {album:?}");
            match discogs.search(&artist, &album).await {
                Ok(ids) => {
                    let index_tracks = self.cfg.discogs.as_ref().is_some_and(|d| d.index_tracks);
                    for id in ids.into_iter().take(5) {
                        match discogs.release(id, index_tracks).await {
                            Ok(r) => matches.push(matching::score(tracks, &r)),
                            Err(e) => {
                                let _ = writeln!(log, "discogs release {id}: {e}");
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = writeln!(log, "discogs search failed: {e}");
                }
            }
        }
        Ok(matches)
    }

    /// Artist and album from the tags, or from the folder name when the
    /// files are untagged: `Artist - Album (Year)` and `(Year) Album` are the
    /// shapes folders on the network actually take.
    fn query_terms(&self, dir: &Path, tracks: &[Track]) -> (String, String) {
        let artist = matching::consensus(
            tracks
                .iter()
                .map(|t| t.album_artist.as_deref().or(t.artist.as_deref())),
        );
        let album = matching::consensus(tracks.iter().map(|t| t.album.as_deref()));
        if let (Some(a), Some(b)) = (&artist, &album) {
            return (a.clone(), b.clone());
        }
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let cleaned = strip_brackets(&name);
        match cleaned.split_once(" - ") {
            Some((a, b)) => (
                artist.unwrap_or_else(|| a.trim().to_string()),
                album.unwrap_or_else(|| b.trim().to_string()),
            ),
            None => {
                let parent = dir
                    .parent()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned());
                (
                    artist.or(parent).unwrap_or_default(),
                    album.unwrap_or(cleaned),
                )
            }
        }
    }

    /// Plan every destination first and refuse the album if any collides —
    /// with another track in it, or with a file already there — so a
    /// conflict on the fifth track does not leave four already moved.
    async fn apply(
        &self,
        tracks: &[Track],
        m: &Match,
        source_dir: &Path,
        filing: Filing<'_>,
        log: &mut String,
    ) -> Result<PathBuf, ImportError> {
        let release = &m.release;
        let remote: Vec<_> = release.tracks().collect();
        let entries: Vec<(usize, Tags)> = m
            .pairs
            .iter()
            .map(|&(l, r)| {
                let (medium, rt) = remote[r];
                (l, self.tags(release, r, medium.position, rt))
            })
            .collect();
        self.file(tracks, &entries, Some(release), source_dir, filing, log)
            .await
    }

    /// Where `local` would be filed with `tags`.
    fn destination(
        &self,
        local: &Track,
        tags: &Tags,
        release: Option<&Release>,
        listings: &mut paths::Listings,
    ) -> Result<PathBuf, ImportError> {
        let rel = paths::render(&self.cfg, tags, local, release)?;
        // Not `with_extension`: it cuts at the last dot, and titles have dots.
        let ext = local
            .path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        Ok(paths::under_with(
            &self.cfg.directory,
            &format!("{rel}.{ext}"),
            listings,
        )?)
    }

    /// Where each `(file, tags)` would be filed, refused if two would land
    /// on one path. In place, a file whose destination is itself is not
    /// occupying it: it is the file being retagged.
    fn destinations(
        &self,
        tracks: &[Track],
        entries: &[(usize, Tags)],
        release: Option<&Release>,
        in_place: bool,
    ) -> Result<Vec<Destination>, ImportError> {
        let mut plan = Vec::with_capacity(entries.len());
        let mut claimed = std::collections::HashMap::new();
        let mut listings = paths::Listings::default();
        for (l, tags) in entries {
            let local = &tracks[*l];
            let to = self.destination(local, tags, release, &mut listings)?;
            if let Some(other) = claimed.insert(paths::collision_key(&to), *l) {
                return Err(ImportError::Conflict(format!(
                    "{} and {} would both be filed as {}",
                    tracks[other].path.display(),
                    local.path.display(),
                    to.display()
                )));
            }
            let stays = same_file(&local.path, &to);
            plan.push(Destination {
                local: *l,
                tags: tags.clone(),
                occupied: to.exists() && !(in_place && stays),
                to,
                stays,
            });
        }
        Ok(plan)
    }

    /// Tag and move each `(file, tags)` into place, with the release's cover
    /// when there is one and the files' own otherwise.
    async fn file(
        &self,
        tracks: &[Track],
        entries: &[(usize, Tags)],
        release: Option<&Release>,
        source_dir: &Path,
        filing: Filing<'_>,
        log: &mut String,
    ) -> Result<PathBuf, ImportError> {
        let in_place = matches!(filing, Filing::InPlace);
        let plan = self.destinations(tracks, entries, release, in_place)?;
        // Every destination already there, each the same recording as its
        // source, is this album filed once before: a repeated import (a
        // retry, a second request queued behind the first) succeeds and
        // moves nothing. Anything short of that is a conflict, and nothing
        // is overwritten either way. A replacing import sets those copies
        // aside instead, once the cover is in hand.
        let present: Vec<bool> = plan.iter().map(|d| d.occupied).collect();
        let bin = match filing {
            Filing::Replacing(bin) => Some(bin.to_path_buf()),
            _ => None,
        };
        if bin.is_none() {
            if !in_place && present.iter().all(|p| *p) && !plan.is_empty() {
                let same = plan.iter().all(|d| {
                    meta::read(&d.to).is_ok_and(|there| {
                        let here = &tracks[d.local];
                        there.format == here.format
                            && (there.duration.as_secs_f64() - here.duration.as_secs_f64()).abs()
                                < 1.0
                    })
                });
                let dir = plan[0]
                    .to
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default();
                if same {
                    let _ = writeln!(
                        log,
                        "already in the library as {}; nothing moved",
                        dir.display()
                    );
                    return Ok(dir);
                }
            }
            if let Some(i) = present.iter().position(|p| *p) {
                return Err(ImportError::Exists(
                    plan[i]
                        .to
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_default(),
                ));
            }
        }
        // The folders a replacing import sets aside.
        let mut taken: Vec<PathBuf> = match bin {
            Some(_) => plan
                .iter()
                .zip(&present)
                .filter(|(_, p)| **p)
                .filter_map(|(d, _)| d.to.parent().map(Path::to_path_buf))
                .collect(),
            None => Vec::new(),
        };
        taken.sort();
        taken.dedup();

        let cover_art = if self.cfg.fetch_art {
            self.cover(release, tracks).await
        } else {
            None
        };
        let _ = writeln!(
            log,
            "cover art: {}",
            match &cover_art {
                Some(c) if c.resized => format!("{}, resized", c.source),
                Some(c) => c.source.to_string(),
                None => "none".to_string(),
            }
        );
        let cover: Option<std::sync::Arc<[u8]>> = cover_art.map(|c| c.bytes.into());
        // Owned for the blocking task; the log is written from `plan`.
        let work: Vec<(PathBuf, PathBuf, bool, Tags)> = plan
            .iter()
            .map(|d| {
                (
                    tracks[d.local].path.clone(),
                    d.to.clone(),
                    d.stays,
                    d.tags.clone(),
                )
            })
            .collect();
        let moving = self.cfg.move_files || in_place;
        let preserve_mtime = self.cfg.import_added;
        let root = self.cfg.directory.clone();
        let source = source_dir.to_path_buf();
        let new_dir = plan
            .first()
            .and_then(|d| d.to.parent())
            .map(Path::to_path_buf);
        // One blocking task sets aside and files the whole album, so a
        // dropped import future cannot stop it half way: it finishes, or
        // its journal puts every file back where it was. Tags already
        // written to a moved file's source stay written.
        let replaced = {
            let new_dir = new_dir.clone();
            tokio::task::spawn_blocking(move || -> Result<Vec<(PathBuf, PathBuf)>, ImportError> {
                let mut journal = paths::Journal::default();
                let mut replaced = Vec::new();
                if let Some(bin) = &bin {
                    for dir in taken {
                        let to = set_aside(&dir, &root, bin, &mut journal)?;
                        replaced.push((dir, to));
                    }
                    if let Some((_, to, _, _)) = work.iter().find(|(_, to, _, _)| to.exists()) {
                        return Err(ImportError::Exists(
                            to.parent().map(Path::to_path_buf).unwrap_or_default(),
                        ));
                    }
                }
                for (from, to, stays, tags) in &work {
                    // Moving: the source is ours to change, so tag it and
                    // move it. Copying: the source must come out untouched,
                    // so copy first and tag the copy.
                    if moving {
                        meta::write(from, tags, cover.as_deref(), preserve_mtime)?;
                        if !stays {
                            journal.transfer(from, to, true)?;
                        }
                    } else {
                        journal.transfer(from, to, false)?;
                        meta::write(to, tags, cover.as_deref(), preserve_mtime)?;
                    }
                }
                // A re-imported album that changed directory takes its
                // other files along, and leaves no empty directory behind.
                if in_place && let Some(new_dir) = &new_dir {
                    for c in companions(&source, new_dir) {
                        if !c.stays {
                            journal.transfer(&c.from, &c.to, true)?;
                        }
                    }
                }
                journal.commit();
                if in_place
                    && let Some(new_dir) = &new_dir
                    && !same_file(&source, new_dir)
                    && std::fs::remove_dir(&source).is_ok()
                    && let Some(parent) = source.parent()
                {
                    let _ = std::fs::remove_dir(parent);
                }
                Ok(replaced)
            })
            .await
            .expect("filing panicked")?
        };
        for (dir, to) in replaced {
            let _ = writeln!(log, "replaced: {} moved to {}", dir.display(), to.display());
        }
        for d in &plan {
            let from = &tracks[d.local].path;
            let _ = writeln!(
                log,
                "{} → {}",
                from.strip_prefix(source_dir).unwrap_or(from).display(),
                d.to.display()
            );
        }
        for t in unmatched(tracks, entries) {
            let _ = writeln!(log, "left behind (no matching track): {}", t.path.display());
        }
        Ok(new_dir.unwrap_or_else(|| self.cfg.directory.clone()))
    }

    fn tags(
        &self,
        release: &Release,
        index: usize,
        disc: u32,
        rt: &crate::musicbrainz::ReleaseTrack,
    ) -> Tags {
        let medium = release.media.iter().find(|m| m.position == disc);
        // A release with no date of its own (or MusicBrainz's placeholder
        // "0000") takes its release group's first release date instead, as
        // beets' `yearfixer` plugin does; a release group with none leaves
        // the release's own date standing in for it, so one missing side
        // never blanks the other.
        let release_date = release
            .date
            .clone()
            .filter(|d| !d.is_empty() && d != "0000");
        let group_date = release
            .release_group
            .as_ref()
            .and_then(|g| g.first_release_date.clone())
            .filter(|d| !d.is_empty());
        let date = release_date.clone().or_else(|| group_date.clone());
        let original = group_date.or_else(|| release_date.clone());
        let label = release.label_info.first();
        let (track, track_total) = if self.cfg.per_disc_numbering {
            (rt.position, medium.map_or(0, |m| m.tracks.len()) as u32)
        } else {
            (
                release.overall_number(index),
                release.media.iter().map(|m| m.tracks.len()).sum::<usize>() as u32,
            )
        };
        let album_artist = release.artist();
        let (artist, title) = self.fold_featuring(
            matching::track_artist(release, index),
            &album_artist,
            rt.title.clone(),
        );
        // A Discogs release carries none of MusicBrainz's own identifiers.
        let mb_ids = source_of(&release.id) == Source::MusicBrainz;
        let track_credit = if rt.artist_credit.is_empty() {
            &release.artist_credit
        } else {
            &rt.artist_credit
        };
        let ids = |credit: &[crate::musicbrainz::ArtistCredit]| -> Vec<String> {
            if mb_ids {
                credited(credit).map(|(_, id)| id).collect()
            } else {
                Vec::new()
            }
        };
        Tags {
            title,
            artist,
            album: release.title.clone(),
            album_artist,
            track,
            track_total,
            disc,
            disc_total: release.media.len() as u32,
            date,
            original_date: original,
            label: label.and_then(|l| l.label.as_ref()).map(|l| l.name.clone()),
            catalog_number: label.and_then(|l| l.catalog_number.clone()),
            country: release.country.clone(),
            media: medium.and_then(|m| m.format.clone()),
            compilation: release.is_compilation(),
            mb_recording_id: mb_ids.then(|| rt.recording.id.clone()),
            mb_track_id: mb_ids.then(|| rt.id.clone()),
            mb_album_id: mb_ids.then(|| release.id.clone()),
            artists: credited(track_credit).map(|(name, _)| name).collect(),
            album_artists: credited(&release.artist_credit)
                .map(|(name, _)| name)
                .collect(),
            mb_artist_ids: ids(track_credit),
            mb_album_artist_ids: ids(&release.artist_credit),
            mb_release_group_id: mb_ids
                .then(|| release.release_group.as_ref().map(|g| g.id.clone()))
                .flatten(),
        }
    }

    /// Folds a featured artist out of `artist` and into `title`, per the
    /// `ftintitle` plugin config; unchanged when it isn't configured or
    /// doesn't apply.
    fn fold_featuring(
        &self,
        artist: String,
        album_artist: &str,
        title: String,
    ) -> (String, String) {
        let Some(ft) = &self.cfg.ft_in_title else {
            return (artist, title);
        };
        match crate::ftintitle::apply(&artist, album_artist, &title, ft.drop, &ft.format) {
            Some((new_artist, new_title)) => (new_artist, new_title),
            None => (artist, title),
        }
    }

    /// The release's front cover: for a Discogs release, its primary image;
    /// otherwise the Cover Art Archive's, then the release group's. When
    /// `art_high_resolution` is set, each source's full-size original is
    /// tried before its thumbnail. Whatever the files already carry comes
    /// last. A candidate narrower than `art_min_width` or failing `art_ratio`
    /// is skipped for the next one; one wider than `art_max_width` is resized
    /// down and re-encoded as JPEG at `art_quality`.
    async fn cover(&self, release: Option<&Release>, tracks: &[Track]) -> Option<CoverArt> {
        let rules = ArtRules {
            min_width: self.cfg.art_min_width,
            max_width: self.cfg.art_max_width,
            quality: self.cfg.art_quality,
            ratio: self.cfg.art_ratio,
        };
        if let Some(release) = release
            && source_of(&release.id) == Source::Discogs
        {
            if let (Some(url), Some(discogs)) = (&release.cover_url, &self.discogs)
                && let Ok(bytes) = discogs.image(url).await
                && !bytes.is_empty()
                && let Some(prepared) = prepare(bytes, rules).await
            {
                return Some(CoverArt {
                    bytes: prepared.bytes,
                    source: "discogs",
                    resized: prepared.resized,
                });
            }
            return Self::embedded_cover(tracks, rules).await;
        }
        let size = match self.cfg.art_max_width {
            w if w >= 1200 => "1200",
            w if w >= 500 => "500",
            _ => "250",
        };
        let mut candidates: Vec<(String, &'static str)> = Vec::new();
        if let Some(release) = release {
            if self.cfg.art_high_resolution {
                candidates.push((
                    format!("https://coverartarchive.org/release/{}/front", release.id),
                    "release (original)",
                ));
                if let Some(g) = &release.release_group {
                    candidates.push((
                        format!("https://coverartarchive.org/release-group/{}/front", g.id),
                        "release group (original)",
                    ));
                }
            }
            candidates.push((
                format!(
                    "https://coverartarchive.org/release/{}/front-{size}",
                    release.id
                ),
                "release",
            ));
            if let Some(g) = &release.release_group {
                candidates.push((
                    format!(
                        "https://coverartarchive.org/release-group/{}/front-{size}",
                        g.id
                    ),
                    "release group",
                ));
            }
        }
        for (url, source) in candidates {
            if let Ok(resp) = self.http.get(&url).send().await
                && resp.status().is_success()
                && let Some(bytes) = art::read_capped(resp, art::MAX_FETCH_BYTES).await
                && !bytes.is_empty()
                && let Some(prepared) = prepare(bytes, rules).await
            {
                return Some(CoverArt {
                    bytes: prepared.bytes,
                    source,
                    resized: prepared.resized,
                });
            }
        }
        Self::embedded_cover(tracks, rules).await
    }

    /// Whatever the files already carry, under the same rules as fetched art.
    async fn embedded_cover(tracks: &[Track], rules: ArtRules) -> Option<CoverArt> {
        let first = tracks.first()?.path.clone();
        let embedded = tokio::task::spawn_blocking(move || meta::embedded_cover(&first))
            .await
            .ok()
            .flatten()?;
        let prepared = prepare(embedded, rules).await?;
        Some(CoverArt {
            bytes: prepared.bytes,
            source: "embedded",
            resized: prepared.resized,
        })
    }
}

/// A cover ready to embed, with what the import log should say about it.
struct CoverArt {
    bytes: Vec<u8>,
    source: &'static str,
    resized: bool,
}

/// Runs [`art::prepare`] off the async runtime.
async fn prepare(bytes: Vec<u8>, rules: ArtRules) -> Option<art::Prepared> {
    tokio::task::spawn_blocking(move || art::prepare(&bytes, &rules))
        .await
        .ok()
        .flatten()
}

fn strip_brackets(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in s.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Corrections to the files' own tags, applied before an import as-is and
/// checked by the same rules as the tags themselves: fields set here apply to
/// every file, and per-file fields to the file named.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Edits {
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub date: Option<String>,
    #[serde(default)]
    pub tracks: Vec<TrackEdit>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TrackEdit {
    /// The file's name in the folder, as `tracks` reports it.
    pub file: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
}

impl Edits {
    fn apply(&self, tracks: &mut [Track]) -> Result<(), ImportError> {
        for t in tracks.iter_mut() {
            if let Some(v) = &self.album {
                t.album = Some(v.clone());
            }
            if let Some(v) = &self.album_artist {
                t.album_artist = Some(v.clone());
            }
            if let Some(v) = &self.date {
                t.date = Some(v.clone());
            }
        }
        for e in &self.tracks {
            let t = tracks
                .iter_mut()
                .find(|t| {
                    t.path
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy() == e.file)
                })
                .ok_or_else(|| ImportError::Untagged(format!("no file named {:?}", e.file)))?;
            if let Some(v) = &e.title {
                t.title = Some(v.clone());
            }
            if let Some(v) = &e.artist {
                t.artist = Some(v.clone());
            }
            if e.track.is_some() {
                t.track = e.track;
            }
            if e.disc.is_some() {
                t.disc = e.disc;
            }
        }
        Ok(())
    }
}

/// A folder against one release. See [`Importer::compare`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct Comparison {
    pub release: Candidate,
    pub parts: matching::Parts,
    pub pairs: Vec<PairView>,
    /// Release tracks with no file.
    pub missing: Vec<ReleaseTrackView>,
    /// Files with no release track.
    pub extra: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PairView {
    pub file: String,
    pub file_title: Option<String>,
    pub disc: u32,
    pub position: u32,
    pub title: String,
    /// 0 is identical.
    pub title_distance: f64,
    /// The file's length less the release's, when the release gives one.
    pub length_delta_secs: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ReleaseTrackView {
    pub disc: u32,
    pub position: u32,
    pub title: String,
    pub length_secs: Option<u64>,
}

/// Whether two paths name one file, differing at most in case or Unicode
/// form.
fn same_file(a: &Path, b: &Path) -> bool {
    paths::collision_key(a) == paths::collision_key(b)
}

/// The tracks no entry is for.
fn unmatched<'a>(
    tracks: &'a [Track],
    entries: &[(usize, Tags)],
) -> impl Iterator<Item = &'a Track> {
    let matched: std::collections::HashSet<usize> = entries.iter().map(|(l, _)| *l).collect();
    tracks
        .iter()
        .enumerate()
        .filter(move |(i, _)| !matched.contains(i))
        .map(|(_, t)| t)
}

/// The album's other files in `from` (cover, cue, log) and where each goes
/// when the album moves to `to`; none when it stays where it is.
fn companions(from: &Path, to: &Path) -> Vec<Companion> {
    if same_file(from, to) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(from) else {
        return Vec::new();
    };
    let mut out: Vec<Companion> = entries
        .flatten()
        .filter(|e| {
            let path = e.path();
            e.file_type().is_ok_and(|t| t.is_file())
                && !meta::is_audio(&path)
                && !paths::is_scratch(&path)
        })
        .map(|e| {
            let dest = to.join(e.file_name());
            Companion {
                from: e.path(),
                stays: dest.exists(),
                to: dest,
            }
        })
        .collect();
    out.sort_by(|a, b| a.from.cmp(&b.from));
    out
}

/// The files against the release `m` pairs them with.
fn comparison(tracks: &[Track], m: &Match) -> Comparison {
    let remote: Vec<_> = m.release.tracks().collect();
    let name = |t: &Track| {
        t.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let pairs = m
        .pairs
        .iter()
        .map(|&(l, r)| {
            let (medium, rt) = remote[r];
            let local = &tracks[l];
            PairView {
                file: name(local),
                file_title: local.title.clone(),
                disc: medium.position,
                position: rt.position,
                title: rt.title.clone(),
                title_distance: (matching::string_distance(
                    local.title.as_deref().unwrap_or_default(),
                    &rt.title,
                ) * 1000.0)
                    .round()
                    / 1000.0,
                length_delta_secs: rt
                    .length
                    .or(rt.recording.length)
                    .map(|ms| (local.duration.as_secs_f64() - ms as f64 / 1000.0).round()),
            }
        })
        .collect();
    let missing = remote
        .iter()
        .enumerate()
        .filter(|(r, _)| !m.pairs.iter().any(|&(_, pr)| pr == *r))
        .map(|(_, (medium, rt))| ReleaseTrackView {
            disc: medium.position,
            position: rt.position,
            title: rt.title.clone(),
            length_secs: rt.length.or(rt.recording.length).map(|ms| ms / 1000),
        })
        .collect();
    let extra = tracks
        .iter()
        .enumerate()
        .filter(|(l, _)| !m.pairs.iter().any(|&(pl, _)| pl == *l))
        .map(|(_, t)| name(t))
        .collect();
    Comparison {
        release: Candidate::from(m),
        parts: m.parts,
        pairs,
        missing,
        extra,
    }
}

/// A file's artist list, kept only while every name in it is still in the
/// credit it lists: an edit to the credit, or a list from some other tagger
/// that disagrees with it, leaves the list to be credited again.
fn names_in(list: &[String], credit: &str) -> Vec<String> {
    let credit = matching::normalise(credit);
    let fits = list.iter().all(|n| {
        let n = matching::normalise(n);
        !n.is_empty() && format!(" {credit} ").contains(&format!(" {n} "))
    });
    if fits { list.to_vec() } else { Vec::new() }
}

/// Tags for filing each file as it is, or why its tags cannot be trusted to.
fn as_is_tags(
    tracks: &[Track],
    ft: Option<&crate::config::FtInTitle>,
) -> Result<Vec<(usize, Tags)>, ImportError> {
    let norm = |s: Option<&str>| s.map(matching::normalise).filter(|s| !s.is_empty());
    let album = matching::consensus(tracks.iter().map(|t| t.album.as_deref()))
        .ok_or_else(|| ImportError::Untagged("no album tag".into()))?;
    let disagree = tracks
        .iter()
        .filter(|t| norm(t.album.as_deref()) != Some(matching::normalise(&album)))
        .count();
    if disagree > 0 {
        return Err(ImportError::Untagged(format!(
            "{disagree} of {} files name a different album, or none",
            tracks.len()
        )));
    }
    let album_artist = matching::consensus(tracks.iter().map(|t| t.album_artist.as_deref()))
        .or_else(|| {
            let artists: std::collections::HashSet<_> = tracks
                .iter()
                .filter_map(|t| norm(t.artist.as_deref()))
                .collect();
            match artists.len() {
                0 => None,
                1 => matching::consensus(tracks.iter().map(|t| t.artist.as_deref())),
                _ => Some("Various Artists".into()),
            }
        })
        .ok_or_else(|| ImportError::Untagged("no artist tag".into()))?;
    let date = matching::consensus(tracks.iter().map(|t| t.date.as_deref()));

    let mut problems = Vec::new();
    let mut seen = HashMap::new();
    let mut entries = Vec::with_capacity(tracks.len());
    for (i, t) in tracks.iter().enumerate() {
        let (number, name) = number_and_title(&t.path);
        let title = t.title.clone().filter(|s| !s.trim().is_empty()).or(name);
        let track = t.track.or(number);
        let disc = t.disc.unwrap_or(1);
        let (Some(title), Some(track)) = (title, track) else {
            problems.push(format!(
                "{} has no title or track number",
                t.path.file_name().unwrap_or_default().to_string_lossy()
            ));
            continue;
        };
        if let Some(other) = seen.insert((disc, track), i) {
            problems.push(format!(
                "{} and {} are both track {track}",
                tracks[other]
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                t.path.file_name().unwrap_or_default().to_string_lossy()
            ));
            continue;
        }
        let artist = t.artist.clone().unwrap_or_else(|| album_artist.clone());
        let (artist, title) = match ft {
            Some(ft) => {
                match crate::ftintitle::apply(&artist, &album_artist, &title, ft.drop, &ft.format) {
                    Some((new_artist, new_title)) => (new_artist, new_title),
                    None => (artist, title),
                }
            }
            None => (artist, title),
        };
        entries.push((
            i,
            Tags {
                title,
                artist,
                album: album.clone(),
                album_artist: album_artist.clone(),
                track,
                track_total: 0,
                disc,
                disc_total: 0,
                date: date.clone(),
                compilation: album_artist == "Various Artists",
                // Against the credit before ftintitle moved a featured
                // artist into the title: that artist is still on the track.
                artists: names_in(&t.artists, t.artist.as_deref().unwrap_or(&album_artist)),
                album_artists: names_in(&t.album_artists, &album_artist),
                ..Tags::default()
            },
        ));
    }
    if !problems.is_empty() {
        return Err(ImportError::Untagged(problems.join("; ")));
    }
    let discs = entries.iter().map(|(_, t)| t.disc).max().unwrap_or(1);
    let per_disc: HashMap<u32, u32> = entries.iter().fold(HashMap::new(), |mut m, (_, t)| {
        *m.entry(t.disc).or_default() += 1;
        m
    });
    for (_, t) in &mut entries {
        t.track_total = per_disc[&t.disc];
        t.disc_total = discs;
    }
    Ok(entries)
}

/// "03 - Title.flac", "03. Title.flac", "Title.flac": the number and the
/// title a file name gives, for files whose tags lack them.
fn number_and_title(path: &Path) -> (Option<u32>, Option<String>) {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let digits: String = stem.chars().take_while(|c| c.is_ascii_digit()).collect();
    let rest = stem[digits.len()..]
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .trim();
    (
        digits.parse().ok().filter(|n| *n > 0),
        (!rest.is_empty()).then(|| rest.to_string()),
    )
}

/// One file per track, when a folder holds an album more than once: FLAC
/// and WAV side by side, or "Track (1).flac" beside "Track.flac". Matching
/// every copy counts the spares as extra tracks, and filing them splits the
/// album across formats. The best copy stays in the import; the rest are
/// returned to be left where they are.
fn one_copy_each(tracks: Vec<Track>) -> (Vec<Track>, Vec<Track>) {
    fn key(t: &Track) -> (u32, Option<u32>, String) {
        let stem = t
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        // "03. Digital Love (1)": the number and the copy marker are not the
        // title.
        let stem = stem.trim_end();
        let stem = match stem.rsplit_once(" (") {
            Some((head, tail))
                if tail.ends_with(')')
                    && tail[..tail.len() - 1].chars().all(|c| c.is_ascii_digit()) =>
            {
                head
            }
            _ => stem,
        };
        let digits: String = stem.chars().take_while(|c| c.is_ascii_digit()).collect();
        let from_name = stem[digits.len()..].trim_start_matches(|c: char| !c.is_alphanumeric());
        let title = t.title.as_deref().unwrap_or(from_name);
        (
            t.disc.unwrap_or(1),
            t.track.or_else(|| digits.parse().ok()),
            matching::normalise(title),
        )
    }
    fn rank(t: &Track) -> (u8, std::cmp::Reverse<u8>, std::cmp::Reverse<u32>) {
        let format = match t.format.as_str() {
            "FLAC" => 0,
            "WAV" | "AIFF" | "ALAC" | "APE" | "WavPack" => 1,
            _ => 2,
        };
        (
            format,
            std::cmp::Reverse(t.bit_depth.unwrap_or(0)),
            std::cmp::Reverse(t.bitrate.unwrap_or(0)),
        )
    }
    let mut best: HashMap<(u32, Option<u32>, String), Track> = HashMap::new();
    let mut spare = Vec::new();
    for t in tracks {
        match best.entry(key(&t)) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(t);
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                if rank(&t) < rank(e.get()) {
                    spare.push(e.insert(t));
                } else {
                    spare.push(t);
                }
            }
        }
    }
    let mut kept: Vec<Track> = best.into_values().collect();
    kept.sort_by(|a, b| a.path.cmp(&b.path));
    spare.sort_by(|a, b| a.path.cmp(&b.path));
    (kept, spare)
}

async fn read_dir(dir: &Path) -> Result<Vec<Track>, ImportError> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut files: Vec<PathBuf> = walk(&dir);
        files.sort();
        if files.is_empty() {
            return Err(ImportError::Empty(dir));
        }
        files
            .iter()
            .map(|p| meta::read(p).map_err(ImportError::from))
            .collect()
    })
    .await
    .expect("reader panicked")
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            out.extend(walk(&path));
        } else if meta::is_audio(&path) {
            out.push(path);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_artist_list_is_kept_only_while_the_credit_still_names_it() {
        let list = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let both = list(&["Lauer", "The Golden Filter"]);
        assert_eq!(names_in(&both, "Lauer & The Golden Filter"), both);
        assert_eq!(names_in(&both, "LAUER × the golden filter"), both);
        assert_eq!(
            names_in(&both, "Lauer"),
            Vec::<String>::new(),
            "edited away"
        );
        assert_eq!(names_in(&list(&["Ann"]), "Joanne"), Vec::<String>::new());
        assert_eq!(names_in(&list(&[""]), "Joanne"), Vec::<String>::new());
        assert_eq!(names_in(&[], "Anything"), Vec::<String>::new());
    }

    fn track(path: &str, format: &str, number: Option<u32>, title: Option<&str>) -> Track {
        Track {
            path: PathBuf::from(path),
            title: title.map(str::to_string),
            artist: None,
            album: None,
            album_artist: None,
            track: number,
            track_total: None,
            disc: None,
            disc_total: None,
            date: None,
            original_date: None,
            compilation: false,
            genre: None,
            mb_recording_id: None,
            mb_album_id: None,
            duration: std::time::Duration::from_secs(200),
            format: format.into(),
            bitrate: None,
            sample_rate: None,
            bit_depth: None,
            ..Default::default()
        }
    }

    fn tagged(
        path: &str,
        album: Option<&str>,
        artist: Option<&str>,
        n: Option<u32>,
        title: Option<&str>,
    ) -> Track {
        Track {
            album: album.map(str::to_string),
            artist: artist.map(str::to_string),
            ..track(path, "FLAC", n, title)
        }
    }

    #[test]
    fn edits_fix_what_the_tags_got_wrong_and_the_gate_still_holds() {
        let mut tracks = vec![
            tagged("a/01 x.flac", Some("Wrong"), Some("A"), Some(1), Some("x")),
            tagged("a/02 y.flac", Some("Other"), Some("A"), None, Some("y")),
        ];
        let edits = Edits {
            album: Some("Right".into()),
            tracks: vec![TrackEdit {
                file: "02 y.flac".into(),
                track: Some(2),
                title: Some("Why".into()),
                ..TrackEdit::default()
            }],
            ..Edits::default()
        };
        edits.apply(&mut tracks).unwrap();
        let entries = as_is_tags(&tracks, None).unwrap();
        assert!(entries.iter().all(|(_, t)| t.album == "Right"));
        assert_eq!(entries[1].1.title, "Why");

        // An edit cannot talk its way past the gate: two files made the same
        // track are still refused.
        let clash = Edits {
            tracks: vec![TrackEdit {
                file: "02 y.flac".into(),
                track: Some(1),
                ..TrackEdit::default()
            }],
            ..Edits::default()
        };
        clash.apply(&mut tracks).unwrap();
        assert!(
            matches!(as_is_tags(&tracks, None), Err(ImportError::Untagged(m)) if m.contains("both track 1"))
        );
    }

    #[test]
    fn edits_refuse_files_that_are_not_there() {
        let mut tracks = vec![tagged(
            "a/01 x.flac",
            Some("A"),
            Some("A"),
            Some(1),
            Some("x"),
        )];
        for file in [
            "missing.flac",
            "",
            "../01 x.flac",
            "a/01 x.flac",
            "01 X.FLAC",
        ] {
            let e = Edits {
                tracks: vec![TrackEdit {
                    file: file.into(),
                    title: Some("t".into()),
                    ..TrackEdit::default()
                }],
                ..Edits::default()
            };
            assert!(
                matches!(e.apply(&mut tracks), Err(ImportError::Untagged(_))),
                "{file:?} should name no file"
            );
        }
    }

    #[test]
    fn empty_edited_values_do_not_pass_for_tags() {
        let mut tracks = vec![tagged(
            "a/untitled.flac",
            None,
            Some("A"),
            Some(1),
            Some("x"),
        )];
        let e = Edits {
            album: Some("   ".into()),
            ..Edits::default()
        };
        e.apply(&mut tracks).unwrap();
        assert!(matches!(
            as_is_tags(&tracks, None),
            Err(ImportError::Untagged(_))
        ));
    }

    #[test]
    fn as_is_takes_a_coherent_album_and_fills_gaps_from_file_names() {
        let entries = as_is_tags(
            &[
                tagged(
                    "a/01 Intro.flac",
                    Some("Hidden"),
                    Some("ANNA"),
                    Some(1),
                    Some("Intro"),
                ),
                tagged(
                    "a/02 - Second Thing.flac",
                    Some("Hidden"),
                    Some("ANNA"),
                    None,
                    None,
                ),
            ],
            None,
        )
        .unwrap();
        let t: Vec<_> = entries
            .iter()
            .map(|(_, t)| (t.track, t.title.as_str(), t.track_total))
            .collect();
        assert_eq!(t, [(1, "Intro", 2), (2, "Second Thing", 2)]);
        assert_eq!(entries[0].1.album_artist, "ANNA");
    }

    #[test]
    fn as_is_calls_many_artists_a_compilation() {
        let entries = as_is_tags(
            &[
                tagged("a/01 x.flac", Some("Mix"), Some("One"), Some(1), Some("x")),
                tagged("a/02 y.flac", Some("Mix"), Some("Two"), Some(2), Some("y")),
            ],
            None,
        )
        .unwrap();
        assert_eq!(entries[0].1.album_artist, "Various Artists");
        assert!(entries[0].1.compilation);
        assert_eq!(entries[1].1.artist, "Two");
    }

    #[test]
    fn as_is_refuses_tags_that_do_not_describe_one_album() {
        let refuse = |tracks: &[Track], why: &str| match as_is_tags(tracks, None) {
            Err(ImportError::Untagged(msg)) => assert!(msg.contains(why), "{msg}"),
            other => panic!("expected a refusal about {why:?}, got {other:?}"),
        };
        refuse(
            &[tagged("a/01 x.flac", None, Some("A"), Some(1), Some("x"))],
            "no album",
        );
        refuse(
            &[
                tagged("a/01 x.flac", Some("One"), Some("A"), Some(1), Some("x")),
                tagged(
                    "a/02 y.flac",
                    Some("Another"),
                    Some("A"),
                    Some(2),
                    Some("y"),
                ),
            ],
            "different album",
        );
        refuse(
            &[
                tagged("a/01 x.flac", Some("One"), Some("A"), Some(1), Some("x")),
                tagged("a/01 y.flac", Some("One"), Some("A"), Some(1), Some("y")),
            ],
            "both track 1",
        );
        refuse(
            &[tagged(
                "a/untitled.flac",
                Some("One"),
                Some("A"),
                None,
                None,
            )],
            "no title or track number",
        );
    }

    #[test]
    fn tags_take_the_release_groups_year_when_the_release_has_none() {
        let release: Release = serde_json::from_value(serde_json::json!({
            "id": "r", "title": "Geogaddi",
            "artist-credit": [{"name": "Boards of Canada", "joinphrase": "", "artist": {"id": "a", "name": "Boards of Canada"}}],
            "release-group": {"id": "g", "first-release-date": "2002-02-18"},
            "media": [{"position": 1, "tracks": [
                {"id": "t1", "position": 1, "title": "Ready Lets Go", "recording": {"id": "r1"}}
            ]}]
        }))
        .unwrap();
        let importer = Importer::with_musicbrainz(Config::default(), MusicBrainz::new("test"));
        let rt = &release.media[0].tracks[0];
        let tags = importer.tags(&release, 0, 1, rt);
        assert_eq!(tags.date.as_deref(), Some("2002-02-18"));
        assert_eq!(tags.original_date.as_deref(), Some("2002-02-18"));
    }

    #[test]
    fn keeps_one_copy_of_each_track_preferring_flac() {
        let (kept, spare) = one_copy_each(vec![
            track("a/1. One More Time.wav", "WAV", None, None),
            track(
                "a/1. One More Time.flac",
                "FLAC",
                Some(1),
                Some("One More Time"),
            ),
            track("a/3. Digital Love (1).wav", "WAV", None, None),
            track("a/3. Digital Love.wav", "WAV", None, None),
            track(
                "a/3. Digital Love.flac",
                "FLAC",
                Some(3),
                Some("Digital Love"),
            ),
            track("a/10. Voyager.wav", "WAV", None, None),
        ]);
        let names = |ts: &[Track]| {
            ts.iter()
                .map(|t| t.path.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(&kept),
            [
                "a/1. One More Time.flac",
                "a/10. Voyager.wav",
                "a/3. Digital Love.flac"
            ]
        );
        assert_eq!(spare.len(), 3);
    }

    #[test]
    fn different_tracks_with_one_title_both_stay() {
        let (kept, spare) = one_copy_each(vec![
            track("a/01 Intro.flac", "FLAC", Some(1), Some("Intro")),
            track("a/09 Intro.flac", "FLAC", Some(9), Some("Intro")),
        ]);
        assert_eq!((kept.len(), spare.len()), (2, 0));
    }

    #[test]
    fn folder_names_yield_search_terms() {
        let imp = Importer::new(Config {
            directory: "/x".into(),
            ..Config::default()
        });
        let q = |p: &str| imp.query_terms(Path::new(p), &[]);
        assert_eq!(
            q("/s/Boards of Canada - Geogaddi (2002) [FLAC]"),
            ("Boards of Canada".into(), "Geogaddi".into())
        );
        assert_eq!(
            q("/s/Boards of Canada/(2002) Geogaddi [FLAC 24-96]"),
            ("Boards of Canada".into(), "Geogaddi".into())
        );
    }

    #[test]
    fn a_discogs_prefixed_id_is_dispatched_to_discogs() {
        assert_eq!(source_of("discogs:123"), Source::Discogs);
        assert_eq!(
            source_of("f2f5b8f4-8c1f-4e6b-9d6a-1234567890ab"),
            Source::MusicBrainz
        );
    }
}
