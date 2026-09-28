//! MusicBrainz, over its JSON web service.
//!
//! The service allows one request per second per client and rejects anonymous
//! user agents, so every request goes through one gate and carries a name and
//! contact. It also has a global budget shared by every client, reported in
//! `X-RateLimit-*` headers; when that runs dry everyone is refused, however
//! politely they ask. So the gate paces itself: refusals widen the spacing,
//! successes relax it, and a nearly spent budget is waited out until it
//! resets. Refusals, server errors and dropped connections are retried rather
//! than reported.

use std::time::Duration;

use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::time::Instant;

const BASE: &str = "https://musicbrainz.org/ws/2";
const SPACING: Duration = Duration::from_millis(1100);
const MAX_SPACING: Duration = Duration::from_secs(10);
const ATTEMPTS: u32 = 10;

#[derive(Debug, thiserror::Error)]
pub enum MbError {
    #[error("MusicBrainz request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("MusicBrainz is refusing requests (HTTP {0}) and did not recover")]
    Unavailable(u16),
    #[error("MusicBrainz sent something unreadable: {0}")]
    Decode(String),
}

pub struct MusicBrainz {
    http: reqwest::Client,
    base: String,
    gate: Mutex<Gate>,
    /// Responses kept on disk. Releases barely change and a retried import
    /// asks for the same ones again, so a cache is most of what stands
    /// between an import and the rate limit.
    cache: Option<std::path::PathBuf>,
}

const RELEASE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const SEARCH_TTL: Duration = Duration::from_secs(24 * 3600);

impl MusicBrainz {
    pub fn new(contact: &str) -> Self {
        Self::with_base(BASE, contact)
    }

    pub fn with_base(base: &str, contact: &str) -> Self {
        let agent = format!("sift/{} ( {contact} )", env!("CARGO_PKG_VERSION"));
        Self {
            http: reqwest::Client::builder()
                .user_agent(agent)
                .timeout(Duration::from_secs(30))
                .build()
                .expect("static client config"),
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
        Some(dir.join(format!("{:016x}.json", h.finish())))
    }

    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, MbError> {
        let ttl = if path.starts_with("release/") {
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
        let value = serde_json::from_slice(&body).map_err(|e| MbError::Decode(e.to_string()))?;
        if let Some(file) = cached {
            if let Some(dir) = file.parent() {
                let _ = tokio::fs::create_dir_all(dir).await;
            }
            let _ = tokio::fs::write(&file, &body).await;
        }
        Ok(value)
    }

    async fn fetch(&self, path: &str, query: &[(&str, &str)]) -> Result<bytes::Bytes, MbError> {
        // The gate is held for the whole exchange, waits included, so a
        // refusal slows every request from here and not just this one.
        let mut gate = self.gate.lock().await;
        let mut attempt = 0;
        loop {
            gate.wait().await;
            let sent = self
                .http
                .get(format!("{}/{path}", self.base))
                .query(query)
                .query(&[("fmt", "json")])
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
                    (wait, MbError::Unavailable(resp.status().as_u16()))
                }
                Ok(resp) => {
                    gate.succeeded(resp.headers());
                    return Ok(resp.error_for_status()?.bytes().await?);
                }
                Err(e) if e.is_timeout() || e.is_connect() => (backoff, MbError::Http(e)),
                Err(e) => return Err(e.into()),
            };
            if attempt >= ATTEMPTS {
                return Err(failure);
            }
            gate.refused(wait.clamp(Duration::from_secs(1), Duration::from_secs(60)));
        }
    }

    /// Releases matching an artist and album title, best first.
    pub async fn search_releases(
        &self,
        artist: &str,
        album: &str,
        limit: usize,
    ) -> Result<Vec<ReleaseHit>, MbError> {
        // Taggers abbreviate; MusicBrainz credits every compilation to one
        // artist spelled in full.
        let artist = match crate::matching::normalise(artist).as_str() {
            "va" | "v a" | "various" | "various artist" => "Various Artists",
            _ => artist,
        };
        let mut query = format!("release:\"{}\"", lucene(album));
        if !artist.is_empty() {
            query.push_str(&format!(" AND artist:\"{}\"", lucene(artist)));
        }
        let limit = limit.to_string();
        let found: SearchResults = self
            .get("release", &[("query", &query), ("limit", &limit)])
            .await?;
        Ok(found.releases)
    }

    /// Genres for a release, most voted first: the release's own, else its
    /// release group's (where MusicBrainz users mostly tag albums), else
    /// the credited artist's. At most three, and none with under a third of
    /// the top genre's votes, so one stray tag does not make the list.
    pub async fn genres(&self, release_id: &str) -> Result<Vec<String>, MbError> {
        #[derive(serde::Deserialize, Default)]
        struct Genre {
            name: String,
            #[serde(default)]
            count: u32,
        }
        #[derive(serde::Deserialize, Default)]
        struct Tagged {
            #[serde(default)]
            genres: Vec<Genre>,
        }
        #[derive(serde::Deserialize)]
        struct Credit {
            artist: Tagged,
        }
        #[derive(serde::Deserialize)]
        struct R {
            #[serde(default)]
            genres: Vec<Genre>,
            #[serde(rename = "release-group", default)]
            release_group: Tagged,
            #[serde(rename = "artist-credit", default)]
            artist_credit: Vec<Credit>,
        }
        let r: R = self
            .get(
                &format!("release/{release_id}"),
                &[("inc", "genres+release-groups+artist-credits")],
            )
            .await?;
        let mut genres = [
            r.genres,
            r.release_group.genres,
            r.artist_credit
                .into_iter()
                .next()
                .map(|c| c.artist.genres)
                .unwrap_or_default(),
        ]
        .into_iter()
        .find(|g| !g.is_empty())
        .unwrap_or_default();
        genres.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));
        let top = genres.first().map_or(0, |g| g.count);
        Ok(genres
            .into_iter()
            .filter(|g| g.count * 3 >= top)
            .take(3)
            .map(|g| title_case(&g.name))
            .collect())
    }

    pub async fn release(&self, id: &str) -> Result<Release, MbError> {
        self.get(
            &format!("release/{id}"),
            &[(
                "inc",
                "recordings+artist-credits+release-groups+media+labels",
            )],
        )
        .await
    }
}

/// "future garage" as "Future Garage", keeping the acronyms genre names
/// use in capitals ("UK Garage", "IDM"), and "drum and bass" as "Drum &
/// Bass", the spelling libraries tagged by other tools already use.
fn title_case(name: &str) -> String {
    const UPPER: &[&str] = &["uk", "us", "idm", "ebm", "edm", "dnb", "r&b", "rnb", "ost"];
    name.split(' ')
        .enumerate()
        .map(|(i, w)| {
            if w == "and" {
                "&".to_string()
            } else if i > 0 && ["of", "the", "n"].contains(&w) {
                w.to_string()
            } else if UPPER.contains(&w) {
                w.to_uppercase()
            } else {
                w.split('-')
                    .map(|p| {
                        let mut c = p.chars();
                        c.next()
                            .map(|f| f.to_uppercase().chain(c).collect())
                            .unwrap_or_default()
                    })
                    .collect::<Vec<String>>()
                    .join("-")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Escape the characters Lucene gives meaning to, so a title like
/// `AC/DC: Live!` is a phrase rather than syntax.
#[derive(Debug)]
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

    fn succeeded(&mut self, headers: &reqwest::header::HeaderMap) {
        self.spacing = self.spacing.mul_f64(0.9).max(SPACING);
        let mut next = Instant::now() + self.spacing;
        if let Some(reset) = budget_reset(headers) {
            next = next.max(Instant::now() + reset);
        }
        self.next = Some(next);
    }

    fn refused(&mut self, wait: Duration) {
        self.spacing = (self.spacing * 2).min(MAX_SPACING);
        self.next = Some(Instant::now() + wait.max(self.spacing));
    }
}

fn retryable(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// How long until the global budget resets, when less than a twentieth of it
/// is left.
fn budget_reset(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let num = |name: &str| -> Option<u64> { headers.get(name)?.to_str().ok()?.trim().parse().ok() };
    let limit = num("x-ratelimit-limit")?;
    let remaining = num("x-ratelimit-remaining")?;
    let reset = num("x-ratelimit-reset")?;
    if remaining.saturating_mul(20) >= limit {
        return None;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(Duration::from_secs(reset.saturating_sub(now).min(60)))
}

impl MbError {
    /// Whether trying again later could succeed: the service was busy or
    /// unreachable, not wrong about what was asked.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Unavailable(_) => true,
            Self::Http(e) => e.is_timeout() || e.is_connect() || e.status().is_some_and(retryable),
            Self::Decode(_) => false,
        }
    }
}

fn lucene(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "+-&|!(){}[]^\"~*?:\\/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[derive(Debug, Deserialize)]
struct SearchResults {
    #[serde(default)]
    releases: Vec<ReleaseHit>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseHit {
    pub id: String,
    #[serde(default)]
    pub score: u32,
    pub title: String,
    #[serde(default, rename = "track-count")]
    pub track_count: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArtistCredit {
    pub name: String,
    #[serde(default)]
    pub joinphrase: String,
    pub artist: Artist,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Artist {
    pub id: String,
    pub name: String,
}

/// "Artist A feat. Artist B", as the credit reads.
pub fn credit_name(credits: &[ArtistCredit]) -> String {
    credits
        .iter()
        .map(|c| format!("{}{}", c.name, c.joinphrase))
        .collect()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub id: String,
    pub title: String,
    pub date: Option<String>,
    pub country: Option<String>,
    pub status: Option<String>,
    pub disambiguation: Option<String>,
    #[serde(rename = "artist-credit", default)]
    pub artist_credit: Vec<ArtistCredit>,
    #[serde(rename = "release-group")]
    pub release_group: Option<ReleaseGroup>,
    #[serde(rename = "label-info", default)]
    pub label_info: Vec<LabelInfo>,
    #[serde(default)]
    pub media: Vec<Medium>,
    /// Discogs' own cover image, for a `discogs:`-sourced release; a
    /// MusicBrainz release has none, and its art comes from the Cover Art
    /// Archive instead.
    #[serde(default)]
    pub cover_url: Option<String>,
}

impl Release {
    pub fn artist(&self) -> String {
        credit_name(&self.artist_credit)
    }

    pub fn track_count(&self) -> usize {
        self.media.iter().map(|m| m.tracks.len()).sum()
    }

    pub fn tracks(&self) -> impl Iterator<Item = (&Medium, &ReleaseTrack)> {
        self.media
            .iter()
            .flat_map(|m| m.tracks.iter().map(move |t| (m, t)))
    }

    pub fn is_compilation(&self) -> bool {
        self.artist_credit
            .first()
            .is_some_and(|c| c.artist.id == VARIOUS_ARTISTS)
    }
}

pub const VARIOUS_ARTISTS: &str = "89ad4ac3-39f7-470e-963a-56509c546377";

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseGroup {
    pub id: String,
    #[serde(rename = "first-release-date")]
    pub first_release_date: Option<String>,
    #[serde(rename = "primary-type")]
    pub primary_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LabelInfo {
    #[serde(rename = "catalog-number")]
    pub catalog_number: Option<String>,
    pub label: Option<Label>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Medium {
    pub position: u32,
    pub format: Option<String>,
    #[serde(default)]
    pub tracks: Vec<ReleaseTrack>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseTrack {
    pub id: String,
    pub position: u32,
    pub title: String,
    /// Milliseconds.
    pub length: Option<u64>,
    #[serde(rename = "artist-credit", default)]
    pub artist_credit: Vec<ArtistCredit>,
    pub recording: Recording,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Recording {
    pub id: String,
    pub length: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genre_names_are_title_cased_with_acronyms_kept() {
        assert_eq!(title_case("future garage"), "Future Garage");
        assert_eq!(title_case("uk garage"), "UK Garage");
        assert_eq!(title_case("lo-fi house"), "Lo-Fi House");
        assert_eq!(title_case("idm"), "IDM");
        assert_eq!(title_case("drum and bass"), "Drum & Bass");
        assert_eq!(title_case("music of the andes"), "Music of the Andes");
    }

    #[test]
    fn a_nearly_spent_budget_is_waited_out() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let headers = |remaining: u64| {
            let mut h = reqwest::header::HeaderMap::new();
            h.insert("x-ratelimit-limit", "1900".parse().unwrap());
            h.insert(
                "x-ratelimit-remaining",
                remaining.to_string().parse().unwrap(),
            );
            h.insert("x-ratelimit-reset", (now + 5).to_string().parse().unwrap());
            h
        };
        assert_eq!(budget_reset(&headers(1800)), None);
        let wait = budget_reset(&headers(10)).unwrap();
        assert!(wait <= Duration::from_secs(5) && wait >= Duration::from_secs(4));
        assert_eq!(budget_reset(&reqwest::header::HeaderMap::new()), None);
    }

    #[test]
    fn spacing_widens_on_refusal_and_relaxes_on_success() {
        let mut gate = Gate::default();
        for _ in 0..10 {
            gate.refused(Duration::from_secs(1));
        }
        assert_eq!(gate.spacing, MAX_SPACING);
        for _ in 0..100 {
            gate.succeeded(&reqwest::header::HeaderMap::new());
        }
        assert_eq!(gate.spacing, SPACING);
    }

    #[test]
    fn escapes_lucene_syntax() {
        assert_eq!(lucene("AC/DC: Live!"), "AC\\/DC\\: Live\\!");
    }

    #[test]
    fn decodes_a_release_lookup() {
        let json = r#"{"id":"r1","title":"Geogaddi","date":"2002-02-18","country":"GB",
          "artist-credit":[{"name":"Boards of Canada","joinphrase":"","artist":{"id":"a1","name":"Boards of Canada"}}],
          "release-group":{"id":"g1","first-release-date":"2002-02-18","primary-type":"Album"},
          "label-info":[{"catalog-number":"WARPCD101","label":{"name":"Warp"}}],
          "media":[{"position":1,"format":"CD","tracks":[
            {"id":"t1","position":1,"title":"Ready Lets Go","length":59000,"artist-credit":[],"recording":{"id":"rec1","length":59000}}]}]}"#;
        let r: Release = serde_json::from_str(json).unwrap();
        assert_eq!(r.artist(), "Boards of Canada");
        assert_eq!(r.track_count(), 1);
        assert_eq!(r.label_info[0].catalog_number.as_deref(), Some("WARPCD101"));
    }
}
