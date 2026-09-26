//! How far a folder of files is from a MusicBrainz release.
//!
//! The shape is beets': a weighted average of per-field distances in 0..1,
//! where 0 is identical. Album title and artist weigh most; each track adds
//! its title and length; tracks the release has and the folder lacks, or the
//! reverse, are penalties of their own. What differs is how tracks are
//! paired: by disc and track number when the files carry them, since that is
//! what the numbers are for, and by cheapest title-and-length pairing only
//! when they do not.

use std::collections::HashMap;

use unicode_normalization::UnicodeNormalization;

use crate::meta::Track;
use crate::musicbrainz::{Release, credit_name};

const W_ALBUM: f64 = 3.0;
const W_ARTIST: f64 = 3.0;
const W_TITLE: f64 = 3.0;
const W_LENGTH: f64 = 2.0;
const W_MISSING: f64 = 0.9;
const W_EXTRA: f64 = 0.6;
/// Length differences below this are rounding and encoder padding.
const LENGTH_GRACE_SECS: f64 = 10.0;
const LENGTH_MAX_SECS: f64 = 30.0;

/// Lower-case, accents stripped, punctuation dropped, `&` read as "and",
/// a leading "the" ignored — the differences no one means.
pub fn normalise(s: &str) -> String {
    let folded: String = s
        .nfkd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect::<String>()
        .to_lowercase()
        .replace('&', " and ");
    let words: Vec<&str> = folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let words = match words.first() {
        Some(&"the") if words.len() > 1 => &words[1..],
        _ => &words[..],
    };
    words.join(" ")
}

/// A title without the edition markers taggers and shops append —
/// "Monster (25th Anniversary Edition)" is "Monster" on MusicBrainz, which
/// keeps the edition in a separate disambiguation.
pub fn base_title(s: &str) -> String {
    const EDITION: &[&str] = &[
        "edition",
        "remaster",
        "deluxe",
        "anniversary",
        "expanded",
        "bonus",
        "reissue",
        "version",
        "special",
        "collector",
        "explicit",
        "clean",
        "mono",
        "stereo",
        "hi-res",
        "24-bit",
        "24bit",
        "flac",
        "web",
        "vinyl",
        "cd",
        "lp",
        "ep",
    ];
    let mut out = s.trim().to_string();
    loop {
        let trimmed = out.trim_end();
        let Some(close) = trimmed.chars().last().filter(|c| *c == ')' || *c == ']') else {
            break;
        };
        let open = if close == ')' { '(' } else { '[' };
        let Some(start) = trimmed.rfind(open) else {
            break;
        };
        let inner = trimmed[start + 1..trimmed.len() - 1].to_lowercase();
        let is_edition = EDITION.iter().any(|k| {
            inner
                .split(|c: char| !c.is_alphanumeric() && c != '-')
                .any(|w| w == *k || w.starts_with(k))
        }) || inner.chars().all(|c| c.is_ascii_digit());
        if start == 0 || !is_edition {
            break;
        }
        out = trimmed[..start]
            .trim_end()
            .trim_end_matches(['-', ':', '–'])
            .trim_end()
            .to_string();
    }
    out
}

pub fn string_distance(a: &str, b: &str) -> f64 {
    let (a, b) = (normalise(a), normalise(b));
    if a == b {
        return 0.0;
    }
    if a.is_empty() || b.is_empty() {
        return 1.0;
    }
    1.0 - strsim::normalized_levenshtein(&a, &b)
}

fn length_distance(local_secs: f64, remote_ms: Option<u64>) -> f64 {
    let Some(ms) = remote_ms else { return 0.0 };
    let diff = (local_secs - ms as f64 / 1000.0).abs();
    ((diff - LENGTH_GRACE_SECS) / (LENGTH_MAX_SECS - LENGTH_GRACE_SECS)).clamp(0.0, 1.0)
}

#[derive(Debug, Clone)]
pub struct Match {
    pub release: Release,
    pub distance: f64,
    /// (index into the local tracks, index into `release.tracks()`)
    pub pairs: Vec<(usize, usize)>,
    /// Release tracks with no file.
    pub missing: usize,
    /// Files with no release track.
    pub extra: usize,
}

impl Match {
    pub fn is_complete(&self) -> bool {
        self.missing == 0 && self.extra == 0
    }
}

/// The value most tracks agree on.
pub fn consensus<'a>(values: impl Iterator<Item = Option<&'a str>>) -> Option<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for v in values.flatten() {
        if !v.trim().is_empty() {
            *counts.entry(v).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(v, n)| (*n, std::cmp::Reverse(v.len())))
        .map(|(v, _)| v.to_string())
}

pub fn score(local: &[Track], release: &Release) -> Match {
    let remote: Vec<_> = release.tracks().collect();
    let album = consensus(local.iter().map(|t| t.album.as_deref())).unwrap_or_default();
    let artist = consensus(
        local
            .iter()
            .map(|t| t.album_artist.as_deref().or(t.artist.as_deref())),
    )
    .unwrap_or_default();

    let d_album = string_distance(&base_title(&album), &base_title(&release.title));
    let d_artist = if release.is_compilation()
        && ["various artists", "various", "va"].contains(&normalise(&artist).as_str())
    {
        0.0
    } else {
        string_distance(&artist, &release.artist())
    };

    let cost = |l: usize, r: usize| -> (f64, f64) {
        let (medium, track) = remote[r];
        let _ = medium;
        let title = local[l].title.as_deref().unwrap_or_default();
        (
            string_distance(title, &track.title),
            length_distance(
                local[l].duration.as_secs_f64(),
                track.length.or(track.recording.length),
            ),
        )
    };

    let pairs = pair_by_number(local, release).unwrap_or_else(|| {
        pair_by_cost(local.len(), remote.len(), |l, r| {
            let (t, len) = cost(l, r);
            W_TITLE * t + W_LENGTH * len
        })
    });

    let mut num = W_ALBUM * d_album + W_ARTIST * d_artist;
    let mut den = W_ALBUM + W_ARTIST;
    for &(l, r) in &pairs {
        let (t, len) = cost(l, r);
        num += W_TITLE * t + W_LENGTH * len;
        den += W_TITLE + W_LENGTH;
    }
    let missing = remote.len() - pairs.len();
    let extra = local.len() - pairs.len();
    num += W_MISSING * missing as f64 + W_EXTRA * extra as f64;
    den += W_MISSING * missing as f64 + W_EXTRA * extra as f64;

    Match {
        release: release.clone(),
        distance: if den > 0.0 { num / den } else { 1.0 },
        pairs,
        missing,
        extra,
    }
}

/// Pair on (disc, track) when every file has a track number and no two
/// collide. A single-disc rip with no disc tag is disc 1.
fn pair_by_number(local: &[Track], release: &Release) -> Option<Vec<(usize, usize)>> {
    let mut by_number = HashMap::new();
    for (i, t) in local.iter().enumerate() {
        let key = (t.disc.unwrap_or(1), t.track?);
        if by_number.insert(key, i).is_some() {
            return None;
        }
    }
    let pairs: Vec<(usize, usize)> = release
        .tracks()
        .enumerate()
        .filter_map(|(r, (m, t))| by_number.get(&(m.position, t.position)).map(|&l| (l, r)))
        .collect();
    // Numbers that match nothing are a sign they mean something else — a
    // continuous count across discs, say. Fall back rather than trust them.
    (pairs.len() * 2 >= local.len()).then_some(pairs)
}

/// Cheapest pairs first, each file and track used once. Not optimal in
/// theory; indistinguishable in practice, where one pairing is far cheaper
/// than the rest.
fn pair_by_cost(
    locals: usize,
    remotes: usize,
    cost: impl Fn(usize, usize) -> f64,
) -> Vec<(usize, usize)> {
    let mut all: Vec<(f64, usize, usize)> = (0..locals)
        .flat_map(|l| (0..remotes).map(move |r| (l, r)))
        .map(|(l, r)| (cost(l, r), l, r))
        .collect();
    all.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (mut used_l, mut used_r) = (vec![false; locals], vec![false; remotes]);
    let mut pairs = Vec::new();
    for (c, l, r) in all {
        if used_l[l] || used_r[r] || c > (W_TITLE + W_LENGTH) * 0.6 {
            continue;
        }
        used_l[l] = true;
        used_r[r] = true;
        pairs.push((l, r));
    }
    pairs.sort_unstable();
    pairs
}

/// The credited artist for one track, falling back to the release's.
pub fn track_artist(release: &Release, index: usize) -> String {
    let (_, t) = release.tracks().nth(index).expect("index from pairs");
    if t.artist_credit.is_empty() {
        release.artist()
    } else {
        credit_name(&t.artist_credit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn release() -> Release {
        serde_json::from_value(serde_json::json!({
            "id": "r", "title": "Geogaddi",
            "artist-credit": [{"name": "Boards of Canada", "joinphrase": "", "artist": {"id": "a", "name": "Boards of Canada"}}],
            "media": [{"position": 1, "tracks": [
                {"id": "t1", "position": 1, "title": "Ready Lets Go", "length": 59000, "recording": {"id": "r1"}},
                {"id": "t2", "position": 2, "title": "Music Is Math", "length": 321000, "recording": {"id": "r2"}},
                {"id": "t3", "position": 3, "title": "Beware the Friendly Stranger", "length": 37000, "recording": {"id": "r3"}}
            ]}]
        }))
        .unwrap()
    }

    fn track(title: &str, n: Option<u32>, secs: u64) -> Track {
        Track {
            title: Some(title.into()),
            album: Some("Geogaddi".into()),
            artist: Some("Boards Of Canada".into()),
            track: n,
            duration: Duration::from_secs(secs),
            ..Default::default()
        }
    }

    #[test]
    fn edition_markers_come_off_titles() {
        assert_eq!(base_title("Monster (25th Anniversary Edition)"), "Monster");
        assert_eq!(base_title("Kill For Love (Deluxe)"), "Kill For Love");
        assert_eq!(base_title("OK Computer [Remastered] (2017)"), "OK Computer");
        assert_eq!(
            base_title("(What's the Story) Morning Glory?"),
            "(What's the Story) Morning Glory?"
        );
        assert_eq!(base_title("Live at Leeds (Live)"), "Live at Leeds (Live)");
        assert_eq!(
            base_title("Music Has the Right to Children"),
            "Music Has the Right to Children"
        );
    }

    #[test]
    fn normalises_the_differences_no_one_means() {
        assert_eq!(normalise("The Beatles"), "beatles");
        assert_eq!(normalise("Björk & Friends!"), "bjork and friends");
        assert!(string_distance("Ready Let's Go", "Ready Lets Go") < 0.1);
    }

    #[test]
    fn a_complete_tagged_album_is_very_close() {
        let local = vec![
            track("Ready Lets Go", Some(1), 59),
            track("Music Is Math", Some(2), 321),
            track("Beware the Friendly Stranger", Some(3), 37),
        ];
        let m = score(&local, &release());
        assert!(m.distance < 0.01, "{}", m.distance);
        assert!(m.is_complete());
    }

    #[test]
    fn untagged_files_pair_by_title_and_length() {
        let local = vec![
            track("music is math", None, 320),
            track("ready let's go", None, 60),
            track("beware the friendly stranger", None, 38),
        ];
        let m = score(&local, &release());
        assert_eq!(m.pairs, [(0, 1), (1, 0), (2, 2)]);
        assert!(m.distance < 0.05, "{}", m.distance);
    }

    #[test]
    fn missing_tracks_and_the_wrong_album_cost() {
        let local = vec![track("Ready Lets Go", Some(1), 59)];
        let m = score(&local, &release());
        assert_eq!(m.missing, 2);
        let mut wrong = vec![track("Roygbiv", Some(1), 150)];
        wrong[0].album = Some("Music Has the Right to Children".into());
        assert!(score(&wrong, &release()).distance > 0.4);
    }
}
