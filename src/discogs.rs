//! Discogs, over its JSON web service — consulted when MusicBrainz has no
//! strong match.
//!
//! Discogs releases are mapped into the same [`Release`] shape MusicBrainz
//! releases use, so matching, tagging and filing do not need to know which
//! service a release came from. The id is namespaced `discogs:<number>`,
//! since MusicBrainz ids are UUIDs and never contain a colon; fields that
//! only MusicBrainz has (recording ids, artist ids, the release group's
//! mbid) are left empty.

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::musicbrainz::{
    Artist, ArtistCredit, Label, LabelInfo, Medium, Recording, Release, ReleaseGroup, ReleaseTrack,
};

const BASE: &str = "https://api.discogs.com";
const SPACING: Duration = Duration::from_millis(1050);
const MAX_SPACING: Duration = Duration::from_secs(10);
const ATTEMPTS: u32 = 10;
const RELEASE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const SEARCH_TTL: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, thiserror::Error)]
pub enum DiscogsError {
    #[error("Discogs request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Discogs is refusing requests (HTTP {0}) and did not recover")]
    Unavailable(u16),
    #[error("Discogs sent something unreadable: {0}")]
    Decode(String),
}

impl DiscogsError {
    /// Whether trying again later could succeed.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Unavailable(_) => true,
            Self::Http(e) => e.is_timeout() || e.is_connect() || e.status().is_some_and(retryable),
            Self::Decode(_) => false,
        }
    }
}

pub struct Discogs {
    http: reqwest::Client,
    token: String,
    base: String,
    gate: Mutex<Gate>,
    /// Responses kept on disk, same cache directory as MusicBrainz's but
    /// with a `discogs-` prefixed filename, so the two never collide.
    cache: Option<std::path::PathBuf>,
}

impl Discogs {
    pub fn new(token: &str, contact: &str) -> Self {
        Self::with_base(BASE, token, contact)
    }

    pub fn with_base(base: &str, token: &str, contact: &str) -> Self {
        let agent = format!("sift/{} ( {contact} )", env!("CARGO_PKG_VERSION"));
        Self {
            http: reqwest::Client::builder()
                .user_agent(agent)
                .timeout(Duration::from_secs(30))
                .build()
                .expect("static client config"),
            token: token.to_string(),
            base: base.trim_end_matches('/').to_string(),
            gate: Mutex::new(Gate::default()),
            cache: None,
        }
    }

    /// Keep responses under `dir`.
    pub fn with_cache(mut self, dir: std::path::PathBuf) -> Self {
        self.cache = Some(dir);
        self
    }

    fn cache_path(&self, path: &str, query: &[(&str, &str)]) -> Option<std::path::PathBuf> {
        use std::hash::{Hash, Hasher};
        let dir = self.cache.as_ref()?;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (path, query).hash(&mut h);
        Some(dir.join(format!("discogs-{:016x}.json", h.finish())))
    }

    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, DiscogsError> {
        let ttl = if path.starts_with("releases/") {
            RELEASE_TTL
        } else {
            SEARCH_TTL
        };
        let cached = self.cache_path(path, query);
        if let Some(file) = &cached {
            let fresh = tokio::fs::metadata(file)
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age < ttl);
            if fresh
                && let Ok(body) = tokio::fs::read(file).await
                && let Ok(v) = serde_json::from_slice(&body)
            {
                return Ok(v);
            }
        }
        let body: bytes::Bytes = self.fetch(path, query).await?;
        let value =
            serde_json::from_slice(&body).map_err(|e| DiscogsError::Decode(e.to_string()))?;
        if let Some(file) = cached {
            if let Some(dir) = file.parent() {
                let _ = tokio::fs::create_dir_all(dir).await;
            }
            let _ = tokio::fs::write(&file, &body).await;
        }
        Ok(value)
    }

    async fn fetch(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<bytes::Bytes, DiscogsError> {
        let mut gate = self.gate.lock().await;
        let mut attempt = 0;
        loop {
            gate.wait().await;
            let sent = self
                .http
                .get(format!("{}/{path}", self.base))
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Discogs token={}", self.token),
                )
                .query(query)
                .send()
                .await;
            attempt += 1;
            let backoff = Duration::from_secs(2u64 << attempt.min(5));
            let (wait, failure) = match sent {
                Ok(resp) if retryable(resp.status()) => {
                    let wait = resp
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map_or(backoff, Duration::from_secs);
                    (wait, DiscogsError::Unavailable(resp.status().as_u16()))
                }
                Ok(resp) => {
                    gate.succeeded();
                    return Ok(resp.error_for_status()?.bytes().await?);
                }
                Err(e) if e.is_timeout() || e.is_connect() => (backoff, DiscogsError::Http(e)),
                Err(e) => return Err(e.into()),
            };
            if attempt >= ATTEMPTS {
                return Err(failure);
            }
            gate.refused(wait.clamp(Duration::from_secs(1), Duration::from_secs(60)));
        }
    }

    /// Release ids matching an artist and album title, best first.
    pub async fn search(&self, artist: &str, album: &str) -> Result<Vec<u64>, DiscogsError> {
        #[derive(Debug, Deserialize, Default)]
        struct Results {
            #[serde(default)]
            results: Vec<Hit>,
        }
        #[derive(Debug, Deserialize)]
        struct Hit {
            id: u64,
        }
        let found: Results = self
            .get(
                "database/search",
                &[
                    ("type", "release"),
                    ("artist", artist),
                    ("release_title", album),
                    ("per_page", "10"),
                ],
            )
            .await?;
        Ok(found.results.into_iter().map(|h| h.id).collect())
    }

    /// One release, mapped into the same shape a MusicBrainz release takes.
    /// With `index_tracks`, a medley grouped under an index track has the
    /// index's own title prefixed onto each of its parts.
    pub async fn release(&self, id: u64, index_tracks: bool) -> Result<Release, DiscogsError> {
        let r: DRelease = self.get(&format!("releases/{id}"), &[]).await?;
        Ok(map_release(id, r, index_tracks))
    }

    /// An image already resolved to a URL (as `Release::cover_url` gives it),
    /// fetched with the same token used for the API itself.
    pub async fn image(&self, url: &str) -> Result<Vec<u8>, DiscogsError> {
        let resp = self
            .http
            .get(url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Discogs token={}", self.token),
            )
            .send()
            .await?;
        Ok(resp.error_for_status()?.bytes().await?.to_vec())
    }
}

struct Gate {
    next: Option<Instant>,
    spacing: Duration,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            next: None,
            spacing: SPACING,
        }
    }
}

impl Gate {
    async fn wait(&self) {
        if let Some(t) = self.next {
            tokio::time::sleep_until(t).await;
        }
    }

    fn succeeded(&mut self) {
        self.spacing = self.spacing.mul_f64(0.9).max(SPACING);
        self.next = Some(Instant::now() + self.spacing);
    }

    fn refused(&mut self, wait: Duration) {
        self.spacing = (self.spacing * 2).min(MAX_SPACING);
        self.next = Some(Instant::now() + wait.max(self.spacing));
    }
}

fn retryable(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

#[derive(Debug, Clone, Deserialize, Default)]
struct DArtist {
    #[serde(default)]
    name: String,
    /// The word (and its spacing) to place between this artist and the
    /// next, e.g. "&", "feat.".
    #[serde(default)]
    join: String,
}

#[derive(Debug, Deserialize, Default)]
struct DLabel {
    #[serde(default)]
    name: String,
    #[serde(default)]
    catno: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct DImage {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    uri: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct DTrack {
    #[serde(default)]
    position: String,
    #[serde(default)]
    type_: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    duration: String,
    #[serde(default)]
    artists: Vec<DArtist>,
    #[serde(default)]
    sub_tracks: Vec<DTrack>,
}

#[derive(Debug, Deserialize, Default)]
struct DRelease {
    #[serde(default)]
    title: String,
    #[serde(default)]
    artists: Vec<DArtist>,
    #[serde(default)]
    labels: Vec<DLabel>,
    released: Option<String>,
    country: Option<String>,
    #[serde(default)]
    tracklist: Vec<DTrack>,
    #[serde(default)]
    images: Vec<DImage>,
}

fn map_release(id: u64, r: DRelease, index_tracks: bool) -> Release {
    let date = r.released.as_deref().and_then(clean_date);
    let cover_url = r
        .images
        .iter()
        .find(|i| i.kind.as_deref() == Some("primary"))
        .or_else(|| r.images.first())
        .and_then(|i| i.uri.clone());
    Release {
        id: format!("discogs:{id}"),
        title: r.title,
        date: date.clone(),
        country: r.country.filter(|c| !c.is_empty()),
        status: None,
        disambiguation: None,
        artist_credit: artist_credits(&r.artists),
        release_group: Some(ReleaseGroup {
            id: String::new(),
            first_release_date: date,
            primary_type: None,
        }),
        label_info: r
            .labels
            .iter()
            .map(|l| LabelInfo {
                catalog_number: l.catno.clone().filter(|c| !c.is_empty()),
                label: Some(Label {
                    name: l.name.clone(),
                }),
            })
            .collect(),
        media: build_media(flatten_tracks(&r.tracklist, index_tracks)),
        cover_url,
    }
}

/// "Artist A (2)" credited to "Artist A" and "&"-joined to "Artist B" as
/// "Artist A & Artist B", the way MusicBrainz's `artist_credit` reads.
fn artist_credits(artists: &[DArtist]) -> Vec<ArtistCredit> {
    artists
        .iter()
        .map(|a| {
            let name = strip_disambiguation(&a.name);
            let join = a.join.trim();
            ArtistCredit {
                joinphrase: if join.is_empty() {
                    String::new()
                } else {
                    format!(" {join} ")
                },
                artist: Artist {
                    id: String::new(),
                    name: name.clone(),
                },
                name,
            }
        })
        .collect()
}

/// The disambiguation suffix Discogs appends when two artists share a name,
/// e.g. "Artist (2)".
fn strip_disambiguation(name: &str) -> String {
    static SUFFIX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(.*) \(\d+\)$").expect("static regex"));
    match SUFFIX.captures(name) {
        Some(c) => c[1].to_string(),
        None => name.to_string(),
    }
}

/// "2019-00-00" as "2019", "2019-03-00" as "2019-03": trailing all-zero
/// date parts, which mean "unknown", dropped rather than kept as zeroes.
fn clean_date(s: &str) -> Option<String> {
    let mut parts: Vec<&str> = s.trim().split('-').collect();
    while matches!(parts.last(), Some(p) if !p.is_empty() && p.chars().all(|c| c == '0')) {
        parts.pop();
    }
    (!parts.is_empty() && !parts[0].is_empty()).then(|| parts.join("-"))
}

/// "3:45" or "1:02:03" as milliseconds; blank is unknown.
fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<u64> = s
        .split(':')
        .map(|p| p.trim().parse().ok())
        .collect::<Option<_>>()?;
    let secs = match parts.as_slice() {
        [m, s] => m * 60 + s,
        [h, m, s] => h * 3600 + m * 60 + s,
        _ => return None,
    };
    Some(secs * 1000)
}

struct FlatTrack {
    position: String,
    title: String,
    duration: String,
    artists: Vec<DArtist>,
}

/// Headings carry no track; an index track's `sub_tracks` are the real
/// tracks, the index itself is not one.
fn flatten_tracks(tracks: &[DTrack], index_tracks: bool) -> Vec<FlatTrack> {
    let mut out = Vec::new();
    for t in tracks {
        match t.type_.as_str() {
            "heading" => {}
            "index" => {
                for sub in &t.sub_tracks {
                    let title = if index_tracks {
                        format!("{}: {}", t.title, sub.title)
                    } else {
                        sub.title.clone()
                    };
                    out.push(FlatTrack {
                        position: sub.position.clone(),
                        title,
                        duration: sub.duration.clone(),
                        artists: if sub.artists.is_empty() {
                            t.artists.clone()
                        } else {
                            sub.artists.clone()
                        },
                    });
                }
            }
            _ => out.push(FlatTrack {
                position: t.position.clone(),
                title: t.title.clone(),
                duration: t.duration.clone(),
                artists: t.artists.clone(),
            }),
        }
    }
    out
}

static DISC_DASH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[a-z]*(\d+)-(\d+)$").expect("static regex"));
static DISC_DOT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+)\.(\d+)$").expect("static regex"));
static DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d+)$").expect("static regex"));

/// "1-3"/"CD1-3" as disc 1 track 3, "2.3" as disc 2 track 3, a bare "3" as
/// disc 1 track 3. Vinyl sides ("A1", "B2") and blank positions carry no
/// disc/track pair Discogs numbers consistently, so they are counted in the
/// order they appear, disc 1 throughout.
fn parse_position(pos: &str, seq: &mut u32) -> (u32, u32) {
    let pos = pos.trim();
    if let Some(c) = DISC_DASH.captures(pos) {
        return (c[1].parse().unwrap_or(1), c[2].parse().unwrap_or(1));
    }
    if let Some(c) = DISC_DOT.captures(pos) {
        return (c[1].parse().unwrap_or(1), c[2].parse().unwrap_or(1));
    }
    if let Some(c) = DIGITS.captures(pos) {
        return (1, c[1].parse().unwrap_or(1));
    }
    *seq += 1;
    (1, *seq)
}

fn build_media(flat: Vec<FlatTrack>) -> Vec<Medium> {
    let mut seq = 0u32;
    let mut by_disc: std::collections::BTreeMap<u32, Vec<ReleaseTrack>> = Default::default();
    for f in flat {
        let (disc, position) = parse_position(&f.position, &mut seq);
        by_disc.entry(disc).or_default().push(ReleaseTrack {
            id: String::new(),
            position,
            title: f.title,
            length: parse_duration(&f.duration),
            artist_credit: artist_credits(&f.artists),
            recording: Recording {
                id: String::new(),
                length: None,
            },
        });
    }
    by_disc
        .into_iter()
        .map(|(position, tracks)| Medium {
            position,
            format: None,
            tracks,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_discogs_disambiguation_suffix() {
        assert_eq!(strip_disambiguation("Boards of Canada"), "Boards of Canada");
        assert_eq!(strip_disambiguation("Artist (2)"), "Artist");
        assert_eq!(strip_disambiguation("Artist (12)"), "Artist");
    }

    #[test]
    fn drops_trailing_zero_date_parts() {
        assert_eq!(clean_date("2019-00-00"), Some("2019".into()));
        assert_eq!(clean_date("2019-03-00"), Some("2019-03".into()));
        assert_eq!(clean_date("2002-02-18"), Some("2002-02-18".into()));
        assert_eq!(clean_date(""), None);
        assert_eq!(clean_date("0000-00-00"), None);
    }

    #[test]
    fn parses_minute_and_hour_durations() {
        assert_eq!(parse_duration("3:45"), Some(225_000));
        assert_eq!(parse_duration("1:02:03"), Some(3_723_000));
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("garbage"), None);
    }

    #[test]
    fn vinyl_sides_are_counted_in_order_on_one_disc() {
        let flat = vec!["A1", "A2", "B1"]
            .into_iter()
            .map(|p| FlatTrack {
                position: p.into(),
                title: p.into(),
                duration: String::new(),
                artists: Vec::new(),
            })
            .collect();
        let media = build_media(flat);
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].position, 1);
        let positions: Vec<u32> = media[0].tracks.iter().map(|t| t.position).collect();
        assert_eq!(positions, [1, 2, 3]);
    }

    #[test]
    fn disc_and_track_come_from_the_numeric_forms() {
        let mut seq = 0;
        assert_eq!(parse_position("1-3", &mut seq), (1, 3));
        assert_eq!(parse_position("CD1-3", &mut seq), (1, 3));
        assert_eq!(parse_position("2.3", &mut seq), (2, 3));
        assert_eq!(parse_position("3", &mut seq), (1, 3));
    }

    #[test]
    fn an_index_tracks_sub_tracks_are_flattened() {
        let json = r#"[
            {"position":"1","type_":"track","title":"Intro","duration":"1:00"},
            {"type_":"index","title":"Medley","sub_tracks":[
                {"position":"2","title":"Part One","duration":"2:00"},
                {"position":"3","title":"Part Two","duration":"3:00"}
            ]}
        ]"#;
        let tracks: Vec<DTrack> = serde_json::from_str(json).unwrap();
        let plain = flatten_tracks(&tracks, false);
        assert_eq!(
            plain.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
            ["Intro", "Part One", "Part Two"]
        );
        let prefixed = flatten_tracks(&tracks, true);
        assert_eq!(
            prefixed
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            ["Intro", "Medley: Part One", "Medley: Part Two"]
        );
    }

    #[test]
    fn headings_carry_no_track() {
        let json = r#"[
            {"type_":"heading","title":"Side A"},
            {"position":"A1","type_":"track","title":"Song","duration":"3:00"}
        ]"#;
        let tracks: Vec<DTrack> = serde_json::from_str(json).unwrap();
        let flat = flatten_tracks(&tracks, false);
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].title, "Song");
    }
}
