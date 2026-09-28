//! A folder in, an album in the library out — or candidates for a person to
//! choose from.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::art::{self, ArtRules};
use crate::config::Config;
use crate::matching::{self, Match};
use crate::meta::{self, Tags, Track};
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

pub struct Importer {
    pub cfg: Config,
    mb: MusicBrainz,
    http: reqwest::Client,
}

/// What [`Importer::enrich`] added.
#[derive(Debug, Default)]
pub struct Enriched {
    pub gain_db: Option<f64>,
    pub genres: Vec<String>,
    /// Tracks given lyrics.
    pub lyrics: usize,
    pub problems: Vec<String>,
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
        let http = reqwest::Client::builder()
            .user_agent(format!(
                "sift/{} ( {} )",
                env!("CARGO_PKG_VERSION"),
                cfg.musicbrainz_contact
            ))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("static client config");
        Self { cfg, mb, http }
    }

    /// Match and import `dir`. With `release_id`, that release is applied
    /// whatever its distance — the way a person answers a review.
    pub async fn import(
        &self,
        dir: &Path,
        release_id: Option<&str>,
    ) -> Result<Outcome, ImportError> {
        self.import_with(dir, release_id, false).await
    }

    /// What beets' `replaygain`, `lastgenre` and `lyrics` plugins add after
    /// an import: album and track gain, genres from MusicBrainz when the
    /// album has none, and lyrics for tracks without them. Each is written
    /// only to its own tags, and each failure is reported rather than
    /// stopping the rest.
    pub async fn enrich(&self, dir: &Path) -> Result<Enriched, ImportError> {
        let tracks = read_dir(dir).await?;
        let mut out = Enriched::default();
        let paths: Vec<PathBuf> = tracks.iter().map(|t| t.path.clone()).collect();

        let measured = {
            let paths = paths.clone();
            tokio::task::spawn_blocking(move || crate::replaygain::album(&paths, 2))
                .await
                .expect("replaygain panicked")
        };
        match measured {
            Ok((gains, album)) => {
                for (p, g) in paths.iter().zip(gains) {
                    if let Err(e) = meta::set_replaygain(p, g, album) {
                        out.problems.push(e.to_string());
                    }
                }
                out.gain_db = Some(album.db);
            }
            Err(e) => out.problems.push(format!("replaygain: {e}")),
        }

        let untagged = tracks
            .iter()
            .all(|t| t.genre.as_deref().is_none_or(|g| g.trim().is_empty()));
        if untagged && let Some(id) = tracks.iter().find_map(|t| t.mb_album_id.clone()) {
            match self.mb.genres(&id).await {
                Ok(g) if !g.is_empty() => {
                    let genre = g.join("; ");
                    for p in &paths {
                        if let Err(e) = meta::set(p, &[("genre".into(), Some(genre.clone()))]) {
                            out.problems.push(e.to_string());
                        }
                    }
                    out.genres = g;
                }
                Ok(_) => {}
                Err(e) => out.problems.push(format!("genres: {e}")),
            }
        }

        let lyrics = crate::lyrics::Client::new();
        for t in &tracks {
            let (Some(artist), Some(title)) = (&t.artist, &t.title) else {
                continue;
            };
            if meta::has_lyrics(&t.path) {
                continue;
            }
            let album = t.album.as_deref().unwrap_or("");
            match lyrics.get(artist, title, album, t.duration.as_secs()).await {
                Ok(crate::lyrics::Lyrics::Synced(l) | crate::lyrics::Lyrics::Plain(l)) => {
                    match meta::set_lyrics(&t.path, &l) {
                        Ok(()) => out.lyrics += 1,
                        Err(e) => out.problems.push(e.to_string()),
                    }
                }
                Ok(_) => {}
                Err(e) => out.problems.push(format!("lyrics: {e}")),
            }
        }
        Ok(out)
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
        self.import_with(dir, release_id, true).await
    }

    async fn import_with(
        &self,
        dir: &Path,
        release_id: Option<&str>,
        in_place: bool,
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
            Some(id) => vec![matching::score(&tracks, &self.mb.release(id).await?)],
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

        if release_id.is_none() {
            let reason = if best.distance > self.cfg.strong_threshold {
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
            };
            if let Some(reason) = reason {
                return Ok(Outcome::Review {
                    reason,
                    candidates: matches.iter().take(5).map(Candidate::from).collect(),
                    log,
                });
            }
        }
        let best = best.clone();
        let dest = self.apply(&tracks, &best, dir, in_place, &mut log).await?;
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
        let entries = as_is_tags(&tracks)?;
        let _ = writeln!(log, "as-is: filed by the files' own tags");
        let dest = self
            .file(&tracks, &entries, None, dir, false, &mut log)
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
        as_is_tags(&tracks).map(|_| ())
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
        let release = self.mb.release(release_id).await?;
        let m = matching::score(&tracks, &release);
        let remote: Vec<_> = release.tracks().collect();
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
        Ok(Comparison {
            release: Candidate::from(&m),
            parts: m.parts,
            pairs,
            missing,
            extra,
        })
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
        // An album usually has many pressings scoring alike; the ones with
        // as many tracks as the folder are looked up first, since the
        // lookups run out before the list does.
        hits.retain(|h| h.score >= 50);
        hits.sort_by_key(|h| h.track_count.abs_diff(tracks.len()));
        for hit in hits {
            if releases.len() > LOOKUPS {
                break;
            }
            if releases.iter().any(|r| r.id == hit.id) {
                continue;
            }
            releases.push(self.mb.release(&hit.id).await?);
        }
        Ok(releases
            .iter()
            .map(|r| matching::score(tracks, r))
            .collect())
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
        in_place: bool,
        log: &mut String,
    ) -> Result<PathBuf, ImportError> {
        let release = &m.release;
        let remote: Vec<_> = release.tracks().collect();
        let entries: Vec<(usize, Tags)> = m
            .pairs
            .iter()
            .map(|&(l, r)| {
                let (medium, rt) = remote[r];
                (l, self.tags(release, r, medium.position, rt, remote.len()))
            })
            .collect();
        self.file(tracks, &entries, Some(release), source_dir, in_place, log)
            .await
    }

    /// Tag and move each `(file, tags)` into place, with the release's cover
    /// when there is one and the files' own otherwise.
    async fn file(
        &self,
        tracks: &[Track],
        entries: &[(usize, Tags)],
        release: Option<&Release>,
        source_dir: &Path,
        in_place: bool,
        log: &mut String,
    ) -> Result<PathBuf, ImportError> {
        let itself =
            |from: &Path, to: &Path| paths::collision_key(from) == paths::collision_key(to);
        let mut plan = Vec::with_capacity(entries.len());
        let mut claimed = std::collections::HashMap::new();
        for (l, tags) in entries {
            let (l, tags) = (*l, tags.clone());
            let local = &tracks[l];
            let rel = paths::render(&self.cfg, &tags, local, release)?;
            // Not `with_extension`: it cuts at the last dot, and titles have dots.
            let ext = local
                .path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let dest = paths::under(&self.cfg.directory, &format!("{rel}.{ext}"))?;
            if let Some(other) = claimed.insert(paths::collision_key(&dest), l) {
                return Err(ImportError::Conflict(format!(
                    "{} and {} would both be filed as {}",
                    tracks[other].path.display(),
                    local.path.display(),
                    dest.display()
                )));
            }
            plan.push((l, tags, dest));
        }

        // Every destination already there, each the same recording as its
        // source, is this album filed once before: a repeated import (a
        // retry, a second request queued behind the first) succeeds and
        // moves nothing. Anything short of that is a conflict, and nothing
        // is overwritten either way.
        // In place, a file whose destination is itself is not "already
        // there": it is the file being retagged.
        let present: Vec<bool> = plan
            .iter()
            .map(|(l, _, d)| d.exists() && !(in_place && itself(&tracks[*l].path, d)))
            .collect();
        if !in_place && present.iter().all(|p| *p) && !plan.is_empty() {
            let same = plan.iter().all(|(l, _, d)| {
                meta::read(d).is_ok_and(|there| {
                    let here = &tracks[*l];
                    there.format == here.format
                        && (there.duration.as_secs_f64() - here.duration.as_secs_f64()).abs() < 1.0
                })
            });
            let dir = plan[0]
                .2
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
                    .2
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default(),
            ));
        }

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
        let cover = cover_art.map(|c| c.bytes);
        for (l, tags, dest) in &plan {
            let local = &tracks[*l];
            let (cover, tags) = (cover.clone(), tags.clone());
            // Moving: the source is ours to change, so tag it and move it.
            // Copying: the source must come out untouched, so copy first and
            // tag the copy, taking it back out if tagging fails.
            if self.cfg.move_files || in_place {
                let path = local.path.clone();
                tokio::task::spawn_blocking(move || meta::write(&path, &tags, cover.as_deref()))
                    .await
                    .expect("tag writer panicked")?;
                if !itself(&local.path, dest) {
                    paths::transfer(&local.path, dest, true).await?;
                }
            } else {
                paths::transfer(&local.path, dest, false).await?;
                let path = dest.clone();
                let written = tokio::task::spawn_blocking(move || {
                    meta::write(&path, &tags, cover.as_deref())
                })
                .await
                .expect("tag writer panicked");
                if let Err(e) = written {
                    let _ = tokio::fs::remove_file(dest).await;
                    return Err(e.into());
                }
            }
            let _ = writeln!(
                log,
                "{} → {}",
                local
                    .path
                    .strip_prefix(source_dir)
                    .unwrap_or(&local.path)
                    .display(),
                dest.display()
            );
        }
        for (i, t) in tracks.iter().enumerate() {
            if !entries.iter().any(|(l, _)| *l == i) {
                let _ = writeln!(log, "left behind (no matching track): {}", t.path.display());
            }
        }
        // A re-imported album that changed directory takes its other files
        // along, and leaves no empty directory behind.
        if in_place
            && let Some(new_dir) = plan.first().and_then(|(_, _, d)| d.parent())
            && !itself(source_dir, new_dir)
        {
            if let Ok(entries) = std::fs::read_dir(source_dir) {
                for entry in entries.flatten() {
                    let from = entry.path();
                    let to = new_dir.join(entry.file_name());
                    if entry.file_type().is_ok_and(|t| t.is_file())
                        && !meta::is_audio(&from)
                        && !to.exists()
                    {
                        paths::transfer(&from, &to, true).await?;
                    }
                }
            }
            if std::fs::remove_dir(source_dir).is_ok()
                && let Some(parent) = source_dir.parent()
            {
                let _ = std::fs::remove_dir(parent);
            }
        }
        Ok(plan
            .first()
            .and_then(|(_, _, d)| d.parent())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.cfg.directory.clone()))
    }

    fn tags(
        &self,
        release: &Release,
        index: usize,
        disc: u32,
        rt: &crate::musicbrainz::ReleaseTrack,
        total: usize,
    ) -> Tags {
        let medium = release.media.iter().find(|m| m.position == disc);
        let original = release
            .release_group
            .as_ref()
            .and_then(|g| g.first_release_date.clone())
            .filter(|d| !d.is_empty());
        let label = release.label_info.first();
        let (track, track_total) = if self.cfg.per_disc_numbering {
            (rt.position, medium.map_or(0, |m| m.tracks.len()) as u32)
        } else {
            (index as u32 + 1, total as u32)
        };
        Tags {
            title: rt.title.clone(),
            artist: matching::track_artist(release, index),
            album: release.title.clone(),
            album_artist: release.artist(),
            track,
            track_total,
            disc,
            disc_total: release.media.len() as u32,
            date: release.date.clone().filter(|d| !d.is_empty()),
            original_date: original,
            label: label.and_then(|l| l.label.as_ref()).map(|l| l.name.clone()),
            catalog_number: label.and_then(|l| l.catalog_number.clone()),
            country: release.country.clone(),
            media: medium.and_then(|m| m.format.clone()),
            compilation: release.is_compilation(),
            mb_recording_id: Some(rt.recording.id.clone()),
            mb_track_id: Some(rt.id.clone()),
            mb_album_id: Some(release.id.clone()),
            mb_artist_id: rt
                .artist_credit
                .first()
                .or(release.artist_credit.first())
                .map(|c| c.artist.id.clone()),
            mb_album_artist_id: release.artist_credit.first().map(|c| c.artist.id.clone()),
            mb_release_group_id: release.release_group.as_ref().map(|g| g.id.clone()),
        }
    }

    /// The release's front cover from the Cover Art Archive, then the
    /// release group's, then whatever the files already carry. When
    /// `art_high_resolution` is set, each source's full-size original is
    /// tried before its thumbnail. A candidate narrower than `art_min_width`
    /// or failing `art_ratio` is skipped for the next one; one wider than
    /// `art_max_width` is resized down and re-encoded as JPEG at
    /// `art_quality`.
    async fn cover(&self, release: Option<&Release>, tracks: &[Track]) -> Option<CoverArt> {
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
        let rules = ArtRules {
            min_width: self.cfg.art_min_width,
            max_width: self.cfg.art_max_width,
            quality: self.cfg.art_quality,
            ratio: self.cfg.art_ratio,
        };
        for (url, source) in candidates {
            if let Ok(resp) = self.http.get(&url).send().await
                && resp.status().is_success()
                && let Ok(bytes) = resp.bytes().await
                && !bytes.is_empty()
                && let Some(prepared) = prepare(bytes.to_vec(), rules).await
            {
                return Some(CoverArt {
                    bytes: prepared.bytes,
                    source,
                    resized: prepared.resized,
                });
            }
        }
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

/// Tags for filing each file as it is, or why its tags cannot be trusted to.
fn as_is_tags(tracks: &[Track]) -> Result<Vec<(usize, Tags)>, ImportError> {
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
        entries.push((
            i,
            Tags {
                title,
                artist: t.artist.clone().unwrap_or_else(|| album_artist.clone()),
                album: album.clone(),
                album_artist: album_artist.clone(),
                track,
                track_total: 0,
                disc,
                disc_total: 0,
                date: date.clone(),
                compilation: album_artist == "Various Artists",
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
        let entries = as_is_tags(&tracks).unwrap();
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
            matches!(as_is_tags(&tracks), Err(ImportError::Untagged(m)) if m.contains("both track 1"))
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
        assert!(matches!(as_is_tags(&tracks), Err(ImportError::Untagged(_))));
    }

    #[test]
    fn as_is_takes_a_coherent_album_and_fills_gaps_from_file_names() {
        let entries = as_is_tags(&[
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
        ])
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
        let entries = as_is_tags(&[
            tagged("a/01 x.flac", Some("Mix"), Some("One"), Some(1), Some("x")),
            tagged("a/02 y.flac", Some("Mix"), Some("Two"), Some(2), Some("y")),
        ])
        .unwrap();
        assert_eq!(entries[0].1.album_artist, "Various Artists");
        assert!(entries[0].1.compilation);
        assert_eq!(entries[1].1.artist, "Two");
    }

    #[test]
    fn as_is_refuses_tags_that_do_not_describe_one_album() {
        let refuse = |tracks: &[Track], why: &str| match as_is_tags(tracks) {
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
}
