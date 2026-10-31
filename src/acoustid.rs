//! Acoustic fingerprints, looked up on AcoustID: what identifies a file
//! whose tags say nothing, as beets' `chroma` plugin does.
//!
//! A file's first two minutes are decoded and fingerprinted with
//! Chromaprint's algorithm 2, the one `fpcalc` and AcoustID use, and the
//! fingerprint is looked up for the MusicBrainz recordings it belongs to
//! and the releases those appear on.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use rusty_chromaprint::{Configuration, FingerprintCompressor, Fingerprinter};
use serde::Deserialize;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use tokio::sync::Mutex;
use tokio::time::Instant;

const BASE: &str = "https://api.acoustid.org/v2";
/// AcoustID asks for no more than three requests a second.
const SPACING: Duration = Duration::from_millis(350);
/// How much of a file is fingerprinted, as `fpcalc` does by default.
const SECONDS: u32 = 120;
/// Results scoring under this are coincidences more often than matches;
/// beets' `chroma` plugin draws the line in the same place.
const MIN_SCORE: f64 = 0.5;
const TTL: Duration = Duration::from_secs(30 * 24 * 3600);

#[derive(Debug, thiserror::Error)]
pub enum AcoustIdError {
    #[error("AcoustID request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("AcoustID refused the lookup: {message}")]
    Refused { code: Option<u32>, message: String },
    #[error("AcoustID is unavailable (HTTP {0})")]
    Unavailable(u16),
}

impl AcoustIdError {
    /// Whether the next lookup would fail the same way: the service is
    /// unreachable, or the key is wrong. A refusal of one fingerprint says
    /// nothing about the next.
    pub fn stops_all(&self) -> bool {
        match self {
            // A timeout is one slow answer; a refused connection is all of them.
            Self::Http(e) => e.is_connect(),
            Self::Unavailable(_) => true,
            // AcoustID's code for an invalid application key.
            Self::Refused { code, .. } => *code == Some(4),
        }
    }
}

/// A file's fingerprint, ready to look up.
#[derive(Debug, Clone)]
pub struct Fingerprint {
    /// Compressed and base64-encoded, as `fpcalc` prints it.
    pub encoded: String,
    /// The whole file's length, in seconds.
    pub duration: u32,
}

/// What AcoustID knows of one fingerprint.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Identified {
    /// MusicBrainz recordings the audio is.
    pub recordings: HashSet<String>,
    /// MusicBrainz releases those recordings appear on.
    pub releases: HashSet<String>,
}

/// Fingerprint `path`, whose whole length is `duration`. Decoding stops
/// after the first two minutes.
pub fn fingerprint(path: &Path, duration: Duration) -> Result<Fingerprint, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| e.to_string())?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or("no audio track")?;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .cloned()
        .ok_or("no audio parameters")?;
    let track_id = track.id;
    let rate = params.sample_rate.ok_or("unknown sample rate")?;
    let channels = params.channels.as_ref().map_or(2, |c| c.count()) as u32;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|e| e.to_string())?;
    let config = Configuration::preset_test2();
    let mut printer = Fingerprinter::new(&config);
    printer
        .start(rate, channels)
        .map_err(|e| format!("{e:?}"))?;
    let wanted = u64::from(rate) * u64::from(channels) * u64::from(SECONDS);
    let mut fed = 0u64;
    let mut samples: Vec<i16> = Vec::new();
    while fed < wanted {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.to_string()),
        };
        if packet.track_id != track_id {
            continue;
        }
        let audio = match decoder.decode(&packet) {
            Ok(a) => a,
            Err(Error::DecodeError(_)) => continue,
            Err(e) => return Err(e.to_string()),
        };
        if audio.spec().channels().count() as u32 != channels {
            return Err("channel count changes mid-stream".into());
        }
        audio.copy_to_vec_interleaved(&mut samples);
        let take = samples.len().min((wanted - fed) as usize);
        printer.consume(&samples[..take]);
        fed += take as u64;
    }
    printer.finish();
    if printer.fingerprint().is_empty() {
        return Err("too short to fingerprint".into());
    }
    let compressed = FingerprintCompressor::from(&config).compress(printer.fingerprint());
    Ok(Fingerprint {
        encoded: base64_url(&compressed),
        duration: duration.as_secs() as u32,
    })
}

/// URL-safe base64 without padding: Chromaprint's own encoding.
fn base64_url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

pub struct AcoustId {
    http: reqwest::Client,
    client: String,
    base: String,
    /// When the next request may go.
    next: Mutex<Instant>,
    cache: Option<crate::cache::Cache>,
}

#[derive(Deserialize)]
struct Response {
    status: String,
    #[serde(default)]
    error: Option<ErrorBody>,
    #[serde(default)]
    results: Vec<Result_>,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    code: Option<u32>,
    message: String,
}

#[derive(Deserialize)]
struct Result_ {
    score: f64,
    #[serde(default)]
    recordings: Vec<Recording>,
}

#[derive(Deserialize)]
struct Recording {
    id: String,
    #[serde(default)]
    releases: Vec<Id>,
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

impl AcoustId {
    /// `client` is an application's API key, registered at
    /// <https://acoustid.org/new-application>.
    pub fn new(client: &str, contact: &str) -> Self {
        Self::with_base(BASE, client, contact)
    }

    pub fn with_base(base: &str, client: &str, contact: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent(format!("sift/{} ( {contact} )", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("static client config"),
            client: client.to_string(),
            base: base.trim_end_matches('/').to_string(),
            next: Mutex::new(Instant::now()),
            cache: None,
        }
    }

    /// Keep responses under `dir`.
    pub fn with_cache(mut self, dir: std::path::PathBuf) -> Self {
        self.cache = Some(crate::cache::Cache::new(dir, "acoustid-", false, TTL));
        self
    }

    /// The recordings and releases `fp` is.
    pub async fn lookup(&self, fp: &Fingerprint) -> Result<Identified, AcoustIdError> {
        let duration = fp.duration.to_string();
        // Sent as a form body: a fingerprint is a few kilobytes, too long
        // for a URL. The key is left out of the cache's name for it.
        let form = [
            ("format", "json"),
            ("meta", "recordings releaseids"),
            ("duration", duration.as_str()),
            ("fingerprint", fp.encoded.as_str()),
        ];
        let cached = self.cache.as_ref().map(|c| (c, c.path("lookup", &form)));
        let hit = match &cached {
            Some((cache, file)) => cache.read(file, TTL).await,
            None => None,
        };
        let fetched = hit.is_none();
        let body = match hit {
            Some(body) => body,
            None => {
                {
                    let mut next = self.next.lock().await;
                    tokio::time::sleep_until(*next).await;
                    *next = Instant::now() + SPACING;
                }
                let resp = self
                    .http
                    .post(format!("{}/lookup", self.base))
                    .form(&[&[("client", self.client.as_str())][..], &form[..]].concat())
                    .send()
                    .await?;
                let status = resp.status();
                // A refusal of the lookup comes as JSON with its reason; an
                // overloaded service answers with a page of its own.
                if status.is_server_error() || status.as_u16() == 429 {
                    return Err(AcoustIdError::Unavailable(status.as_u16()));
                }
                resp.bytes().await?.to_vec()
            }
        };
        let resp: Response = serde_json::from_slice(&body).map_err(|e| AcoustIdError::Refused {
            code: None,
            message: format!("unreadable answer: {e}"),
        })?;
        if resp.status != "ok" {
            let (code, message) = resp
                .error
                .map_or((None, resp.status), |e| (e.code, e.message));
            return Err(AcoustIdError::Refused { code, message });
        }
        // Only a fresh answer is kept: rewriting a cached one would renew
        // it, and it would never expire.
        if fetched && let Some((cache, file)) = cached {
            cache.write(&file, &body).await;
        }
        let mut out = Identified::default();
        for r in resp.results.into_iter().filter(|r| r.score >= MIN_SCORE) {
            for rec in r.recordings {
                out.releases.extend(rec.releases.into_iter().map(|r| r.id));
                out.recordings.insert(rec.id);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_is_chromaprints_url_safe_unpadded_kind() {
        assert_eq!(base64_url(b""), "");
        assert_eq!(base64_url(b"f"), "Zg");
        assert_eq!(base64_url(b"fo"), "Zm8");
        assert_eq!(base64_url(b"foo"), "Zm9v");
        assert_eq!(base64_url(&[0xfb, 0xff]), "-_8");
    }
}
