//! MusicBrainz, over its JSON web service.
//!
//! The service allows one request per second per client and rejects anonymous
//! user agents, so every request goes through one gate and carries a name and
//! contact. A 503 is the service saying slow down; it is retried after a
//! pause rather than reported.

use std::time::Duration;

use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::time::Instant;

const BASE: &str = "https://musicbrainz.org/ws/2";
const SPACING: Duration = Duration::from_millis(1100);

#[derive(Debug, thiserror::Error)]
pub enum MbError {
    #[error("MusicBrainz request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("MusicBrainz is rate limiting and did not recover")]
    RateLimited,
    #[error("MusicBrainz sent something unreadable: {0}")]
    Decode(String),
}

pub struct MusicBrainz {
    http: reqwest::Client,
    base: String,
    last: Mutex<Option<Instant>>,
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
            last: Mutex::new(None),
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
        // The limit is per address, and anything else on the same network
        // (a laptop running beets, a player looking up art) spends from the
        // same budget. So a 503 is waited out patiently — the Retry-After the
        // service sends, or an exponential backoff to a minute — and the gate
        // is held meanwhile so no other request from here makes it worse.
        for attempt in 0..10u32 {
            let mut last = self.last.lock().await;
            if let Some(t) = *last {
                let wait = SPACING.saturating_sub(t.elapsed());
                tokio::time::sleep(wait).await;
            }
            *last = Some(Instant::now());
            let resp = self
                .http
                .get(format!("{}/{path}", self.base))
                .query(query)
                .query(&[("fmt", "json")])
                .send()
                .await?;
            if resp.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
                let wait = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(2u64 << attempt.min(5))
                    .clamp(1, 60);
                tokio::time::sleep(Duration::from_secs(wait)).await;
                *last = Some(Instant::now());
                continue;
            }
            drop(last);
            return Ok(resp.error_for_status()?.bytes().await?);
        }
        Err(MbError::RateLimited)
    }

    /// Releases matching an artist and album title, best first.
    pub async fn search_releases(
        &self,
        artist: &str,
        album: &str,
        limit: usize,
    ) -> Result<Vec<ReleaseHit>, MbError> {
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

/// Escape the characters Lucene gives meaning to, so a title like
/// `AC/DC: Live!` is a phrase rather than syntax.
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
