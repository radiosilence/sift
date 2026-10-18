//! Crediting each artist on a collaboration whose tags name them only in one
//! string: "Orchestre Tout Puissant Marcel Duchamp × Frànçois & the Atlas
//! Mountains".
//!
//! Separators alone cannot be trusted, since "&", ",", ";" and " x " all
//! occur inside real names ("Earth, Wind & Fire", "Frànçois & the Atlas
//! Mountains"). A credit is split only when every part is an artist the
//! library already holds an album by, and never when the whole credit is
//! one: a band that shares a word with a separator stays whole unless its
//! parts are each artists in their own right.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::library::Album;
use crate::matching::normalise;

/// Separators, lowercased, longest first so that " feat. " is found before
/// " feat ".
const SEPARATORS: &[&str] = &[
    " featuring ",
    " feat. ",
    " feat ",
    " ft. ",
    " ft ",
    " with ",
    " vs. ",
    " vs ",
    " and ",
    " × ",
    " x ",
    " & ",
    " + ",
    " / ",
    ", ",
    "; ",
];

/// A credit has at most this many parts before it is left alone: every way
/// of grouping them is tried, which doubles with each one.
const MAX_PARTS: usize = 10;

/// The artists a library holds albums by, with the albums each is on: what a
/// credit's parts are checked against.
#[derive(Debug, Default, Clone)]
pub struct Known {
    artists: HashMap<String, HashSet<PathBuf>>,
}

impl Known {
    /// Record `name` as an artist on the album in `dir`.
    pub fn add(&mut self, name: &str, dir: &Path) {
        let key = normalise(name);
        if !key.is_empty() {
            self.artists
                .entry(key)
                .or_default()
                .insert(dir.to_path_buf());
        }
    }

    /// Whether `name` is an artist on some album other than the one in
    /// `dir`. The album being credited names its own credit as an artist,
    /// which proves nothing.
    pub fn is_artist(&self, name: &str, dir: &Path) -> bool {
        self.artists
            .get(&normalise(name))
            .is_some_and(|dirs| dirs.iter().any(|d| d != dir))
    }

    /// Every track and album artist in `albums`, as the files name them.
    pub fn from_albums(albums: &[Album]) -> Self {
        let mut known = Self::default();
        for album in albums {
            for item in &album.items {
                let t = &item.track;
                for name in t
                    .artist
                    .iter()
                    .chain(&t.album_artist)
                    .chain(&t.artists)
                    .chain(&t.album_artists)
                {
                    known.add(name, &album.dir);
                }
            }
        }
        known
    }

    pub fn is_empty(&self) -> bool {
        self.artists.is_empty()
    }
}

/// The artists `credit` names, when it names more than one and each is known
/// as an artist apart from the album in `dir`. Of the groupings that work,
/// the one with fewest parts is taken.
pub fn split(credit: &str, known: &Known, dir: &Path) -> Option<Vec<String>> {
    if known.is_artist(credit, dir) {
        return None;
    }
    let parts = pieces(credit);
    if parts.len() < 2 || parts.len() > MAX_PARTS {
        return None;
    }
    // Each bit of `mask` says whether the separator after that piece splits
    // two artists (1) or belongs to a name (0).
    let gaps = parts.len() - 1;
    let mut best: Option<Vec<String>> = None;
    for mask in 1..(1u32 << gaps) {
        let mut names = Vec::new();
        let mut start = 0;
        for gap in 0..=gaps {
            if gap == gaps || mask & (1 << gap) != 0 {
                let end = parts[gap].end;
                names.push(credit[parts[start].start..end].trim().to_string());
                start = gap + 1;
            }
        }
        if names.iter().all(|n| known.is_artist(n, dir))
            && best.as_ref().is_none_or(|b| names.len() < b.len())
        {
            best = Some(names);
        }
    }
    best
}

/// The byte ranges of `credit` between separators.
fn pieces(credit: &str) -> Vec<std::ops::Range<usize>> {
    // Lowercasing must keep every byte where it was for the ranges to slice
    // the original, so only ASCII is folded; "×" is matched as it is.
    let lower = credit.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut start = 0;
    let mut at = 0;
    while at < lower.len() {
        match SEPARATORS.iter().find(|s| lower[at..].starts_with(*s)) {
            Some(sep) => {
                out.push(start..at);
                at += sep.len();
                start = at;
            }
            None => at += lower[at..].chars().next().map_or(1, char::len_utf8),
        }
    }
    out.push(start..lower.len());
    out.retain(|r| !credit[r.clone()].trim().is_empty());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(names: &[&str]) -> Known {
        let mut k = Known::default();
        for n in names {
            k.add(n, Path::new(&format!("/music/{n}")));
        }
        k
    }

    const HERE: &str = "/music/this album";

    #[test]
    fn splits_a_collaboration_whose_parts_are_both_known() {
        let k = known(&[
            "Orchestre Tout Puissant Marcel Duchamp",
            "Frànçois & the Atlas Mountains",
        ]);
        assert_eq!(
            split(
                "Orchestre Tout Puissant Marcel Duchamp × Frànçois & the Atlas Mountains",
                &k,
                Path::new(HERE)
            ),
            Some(vec![
                "Orchestre Tout Puissant Marcel Duchamp".to_string(),
                "Frànçois & the Atlas Mountains".to_string(),
            ])
        );
    }

    #[test]
    fn leaves_a_credit_alone_unless_every_part_is_known() {
        let k = known(&["Orchestre Tout Puissant Marcel Duchamp"]);
        assert_eq!(
            split(
                "Orchestre Tout Puissant Marcel Duchamp × Frànçois & the Atlas Mountains",
                &k,
                Path::new(HERE)
            ),
            None
        );
        assert_eq!(
            split("Hardcore X Die", &known(&["Die"]), Path::new(HERE)),
            None
        );
    }

    #[test]
    fn never_splits_a_name_that_is_an_artist_itself() {
        let k = known(&["Earth, Wind & Fire", "Earth", "Wind", "Fire"]);
        assert_eq!(split("Earth, Wind & Fire", &k, Path::new(HERE)), None);
    }

    #[test]
    fn the_album_being_credited_does_not_count_as_knowing_its_credit() {
        let mut k = known(&["Lauer", "The Golden Filter"]);
        k.add("Lauer & The Golden Filter", Path::new(HERE));
        assert_eq!(
            split("Lauer & The Golden Filter", &k, Path::new(HERE)),
            Some(vec!["Lauer".to_string(), "The Golden Filter".to_string()])
        );
    }

    #[test]
    fn splits_featuring_commas_and_semicolons() {
        let k = known(&["Jimi Jules", "Black Coffee", "Anouk", "Jesaya"]);
        let here = Path::new(HERE);
        assert_eq!(
            split("Jimi Jules; Black Coffee", &k, here),
            Some(vec!["Jimi Jules".to_string(), "Black Coffee".to_string()])
        );
        assert_eq!(
            split("Jimi Jules feat. Anouk & Jesaya", &k, here),
            Some(vec![
                "Jimi Jules".to_string(),
                "Anouk".to_string(),
                "Jesaya".to_string()
            ])
        );
        assert_eq!(split("Jimi Jules", &k, here), None);
    }
}
