//! A folder in, an album in the library out — or candidates for a person to
//! choose from.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

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
        release: Candidate,
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
        let mut log = String::new();
        let tracks = read_dir(dir).await?;
        let _ = writeln!(log, "{} audio files in {}", tracks.len(), dir.display());

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
            "best: {} — {} ({}), distance {:.3}",
            best.release.artist(),
            best.release.title,
            best.release.id,
            best.distance
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
        let dest = self.apply(&tracks, &best, dir, &mut log).await?;
        Ok(Outcome::Imported {
            dir: dest,
            release: Candidate::from(&best),
            log,
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
        for hit in hits {
            if releases.len() > LOOKUPS || hit.score < 50 {
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
        log: &mut String,
    ) -> Result<PathBuf, ImportError> {
        let release = &m.release;
        let remote: Vec<_> = release.tracks().collect();
        let mut plan = Vec::with_capacity(m.pairs.len());
        let mut claimed = std::collections::HashMap::new();
        for &(l, r) in &m.pairs {
            let (medium, rt) = remote[r];
            let tags = self.tags(release, r, medium.position, rt, remote.len());
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
            if dest.exists() {
                return Err(ImportError::Conflict(format!(
                    "{} is already in the library",
                    dest.display()
                )));
            }
            plan.push((l, tags, dest));
        }

        let cover = if self.cfg.fetch_art {
            self.cover(release, tracks).await
        } else {
            None
        };
        let _ = writeln!(
            log,
            "cover art: {}",
            if cover.is_some() { "yes" } else { "none" }
        );
        for (l, tags, dest) in &plan {
            let local = &tracks[*l];
            let (cover, tags) = (cover.clone(), tags.clone());
            // Moving: the source is ours to change, so tag it and move it.
            // Copying: the source must come out untouched, so copy first and
            // tag the copy, taking it back out if tagging fails.
            if self.cfg.move_files {
                let path = local.path.clone();
                tokio::task::spawn_blocking(move || meta::write(&path, &tags, cover.as_deref()))
                    .await
                    .expect("tag writer panicked")?;
                paths::transfer(&local.path, dest, true).await?;
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
            if !m.pairs.iter().any(|&(l, _)| l == i) {
                let _ = writeln!(log, "left behind (no matching track): {}", t.path.display());
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

    /// The release's front cover from the Cover Art Archive at the largest
    /// thumbnail within the configured width, then the release group's, then
    /// whatever the files already carry.
    async fn cover(&self, release: &Release, tracks: &[Track]) -> Option<Vec<u8>> {
        let size = match self.cfg.art_max_width {
            w if w >= 1200 => "1200",
            w if w >= 500 => "500",
            _ => "250",
        };
        let mut urls = vec![format!(
            "https://coverartarchive.org/release/{}/front-{size}",
            release.id
        )];
        if let Some(g) = &release.release_group {
            urls.push(format!(
                "https://coverartarchive.org/release-group/{}/front-{size}",
                g.id
            ));
        }
        for url in urls {
            if let Ok(resp) = self.http.get(&url).send().await
                && resp.status().is_success()
                && let Ok(bytes) = resp.bytes().await
                && !bytes.is_empty()
            {
                return Some(bytes.to_vec());
            }
        }
        let first = tracks.first()?.path.clone();
        tokio::task::spawn_blocking(move || meta::embedded_cover(&first))
            .await
            .ok()
            .flatten()
    }
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
