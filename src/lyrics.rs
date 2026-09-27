//! Lyrics from LRCLIB, as beets' `lyrics` plugin fetches them: synced (LRC)
//! where it has them, so players can scroll them, and plain otherwise.
//! LRCLIB needs no key and matches on artist, title, album and duration,
//! which is what keeps a live version or a remix from getting the album
//! cut's words.

use serde::Deserialize;

const BASE: &str = "https://lrclib.net/api";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Found {
    #[serde(default)]
    instrumental: bool,
    synced_lyrics: Option<String>,
    plain_lyrics: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Lyrics {
    Synced(String),
    Plain(String),
    Instrumental,
    NotFound,
}

pub struct Client {
    http: reqwest::Client,
    base: String,
}

impl Client {
    pub fn new() -> Self {
        Self::with_base(BASE)
    }

    pub fn with_base(base: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent(format!(
                    "sift/{} (https://github.com/radiosilence/sift)",
                    env!("CARGO_PKG_VERSION")
                ))
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .expect("static client config"),
            base: base.trim_end_matches('/').to_string(),
        }
    }

    pub async fn get(
        &self,
        artist: &str,
        title: &str,
        album: &str,
        secs: u64,
    ) -> Result<Lyrics, reqwest::Error> {
        let resp = self
            .http
            .get(format!("{}/get", self.base))
            .query(&[
                ("artist_name", artist),
                ("track_name", title),
                ("album_name", album),
                ("duration", &secs.to_string()),
            ])
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Lyrics::NotFound);
        }
        let found: Found = resp.error_for_status()?.json().await?;
        let nonempty = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
        Ok(if found.instrumental {
            Lyrics::Instrumental
        } else if let Some(s) = nonempty(found.synced_lyrics) {
            Lyrics::Synced(s)
        } else if let Some(p) = nonempty(found.plain_lyrics) {
            Lyrics::Plain(p)
        } else {
            Lyrics::NotFound
        })
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}
