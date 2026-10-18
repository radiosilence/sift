//! Crediting each artist on a collaboration whose tags name them only in one
//! string: "Orchestre Tout Puissant Marcel Duchamp × Frànçois & the Atlas
//! Mountains".
//!
//! Separators alone cannot be trusted, since "&", ",", ";" and " x " all
//! occur inside real names ("Earth, Wind & Fire", "Frànçois & the Atlas
//! Mountains", "Malcolm X"). A credit is split only when every part is an
//! artist the library holds an album by elsewhere, so a band that shares a
//! word with a separator stays whole unless each part is an artist in its
//! own right.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::library::Album;
use crate::matching::normalise;

/// Words that join two artists, matched case-insensitively and only as whole
/// words: whitespace (or an opening bracket, for "(feat. B)") before them and
/// whitespace after. Longest first, so "feat." is found before "feat".
const WORDS: &[&str] = &[
    "featuring",
    "feat.",
    "feat",
    "ft.",
    "ft",
    "with",
    "vs.",
    "vs",
    "and",
    "x",
    "×",
    "&",
    "+",
    "/",
];

/// Punctuation that joins two artists straight after a name: "A, B", "A; B",
/// and "A;B" as some taggers flatten a list. A comma needs whitespace after
/// it, since numbers and odd names carry bare ones.
const MARKS: &[(char, bool)] = &[(',', true), (';', false)];

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
/// the one with fewest parts is taken, so a known "Frànçois & the Atlas
/// Mountains" is never split further into an equally known "Frànçois".
pub fn split(credit: &str, known: &Known, dir: &Path) -> Option<Vec<String>> {
    let parts = pieces(credit);
    if parts.len() < 2 || parts.len() > MAX_PARTS {
        return None;
    }
    // Each bit of `mask` says whether the separator after that piece splits
    // two artists (1) or belongs to a name (0).
    let gaps = parts.len() - 1;
    let mut best: Option<Vec<String>> = None;
    'groupings: for mask in 1..(1u32 << gaps) {
        let mut names = Vec::new();
        let mut start = 0;
        for gap in 0..=gaps {
            if gap == gaps || mask & (1 << gap) != 0 {
                let name = tidy(&credit[parts[start].start..parts[gap].end]);
                if name.is_empty() || !known.is_artist(name, dir) {
                    continue 'groupings;
                }
                names.push(name.to_string());
                start = gap + 1;
            }
        }
        if best.as_ref().is_none_or(|b| names.len() < b.len()) {
            best = Some(names);
        }
    }
    best
}

/// A part as a name: trimmed of whitespace and control characters (a stray
/// NUL from a flattened list), without a bracket the split left unbalanced
/// ("B)" of "A (feat. B)").
fn tidy(part: &str) -> &str {
    let mut s = part.trim_matches(|c: char| c.is_whitespace() || c.is_control());
    loop {
        let before = s;
        for (open, close) in [('(', ')'), ('[', ']')] {
            if s.ends_with(close) && !s.contains(open) {
                s = s[..s.len() - close.len_utf8()].trim_end();
            }
            if s.starts_with(open) && !s.contains(close) {
                s = s[open.len_utf8()..].trim_start();
            }
            if s.ends_with(open) {
                s = s[..s.len() - open.len_utf8()].trim_end();
            }
        }
        s = s.trim_matches(|c: char| c.is_whitespace() || c.is_control());
        if s == before {
            return s;
        }
    }
}

/// The byte ranges of `credit` between separators. A range may hold only
/// whitespace ("Malcolm X & Y" has one between "X" and "&"): it is kept, so
/// that grouping across it can still rebuild "Malcolm X".
fn pieces(credit: &str) -> Vec<Range<usize>> {
    let chars: Vec<(usize, char)> = credit.char_indices().collect();
    let at = |i: usize| chars.get(i).map(|&(b, _)| b).unwrap_or(credit.len());
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < chars.len() {
        let (byte, c) = chars[i];
        let before = i.checked_sub(1).map(|j| chars[j].1);
        if let Some(&(_, needs_space)) = MARKS.iter().find(|(m, _)| *m == c)
            && (!needs_space || chars.get(i + 1).is_none_or(|(_, n)| n.is_whitespace()))
        {
            out.push(start..byte);
            start = at(i + 1);
            i += 1;
            continue;
        }
        if before.is_none_or(|b| b.is_whitespace() || b == '(' || b == '[')
            && let Some(len) = word_at(&chars[i..])
            && chars.get(i + len).is_some_and(|(_, n)| n.is_whitespace())
            && i > 0
        {
            // A bracket before the word belongs to it: "A (feat. B)".
            let from = match before {
                Some('(' | '[') => chars[i - 1].0,
                _ => byte,
            };
            out.push(start..from);
            start = at(i + len);
            i += len;
            continue;
        }
        i += 1;
    }
    out.push(start..credit.len());
    // A separator at either end ("Robyn &") joins nothing.
    let blank = |r: &Range<usize>| tidy(&credit[r.clone()]).is_empty();
    while out.last().is_some_and(blank) {
        out.pop();
    }
    let lead = out.iter().take_while(|r| blank(r)).count();
    out.drain(..lead);
    out
}

/// The length in chars of the separator word starting `rest`, if one does.
fn word_at(rest: &[(usize, char)]) -> Option<usize> {
    WORDS.iter().find_map(|w| {
        let n = w.chars().count();
        let matches = rest.len() >= n
            && w.chars()
                .zip(rest)
                .all(|(a, &(_, b))| a.eq_ignore_ascii_case(&b));
        matches.then_some(n)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HERE: &str = "/music/this album";

    fn known(names: &[&str]) -> Known {
        let mut k = Known::default();
        for n in names {
            k.add(n, Path::new(&format!("/music/{n}")));
        }
        k
    }

    /// A library of artists whose names carry every separator there is.
    fn library() -> Known {
        known(&[
            "Orchestre Tout Puissant Marcel Duchamp",
            "Frànçois & the Atlas Mountains",
            "Frànçois",
            "Jimi Jules",
            "Black Coffee",
            "Anouk",
            "Jesaya",
            "Malcolm X",
            "Lauer",
            "The Golden Filter",
            "Earth, Wind & Fire",
            "Earth",
            "Fire",
            "AC/DC",
            "Simon & Garfunkel",
            "Florence + the Machine",
            "Bob Dylan",
            "Beyoncé",
            "Sigur Rós",
            "Röyksopp",
            "Robyn",
            "Daft Punk",
            "Pharrell Williams",
            "Nile Rodgers",
            "Die",
            "Tyler, the Creator",
        ])
    }

    fn check(credit: &str, want: Option<&[&str]>) {
        let got = split(credit, &library(), Path::new(HERE));
        let want = want.map(|w| w.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(got, want, "{credit:?}");
    }

    #[test]
    fn splits_collaborations_however_they_are_joined() {
        let cases: &[(&str, &[&str])] = &[
            (
                "Orchestre Tout Puissant Marcel Duchamp × Frànçois & the Atlas Mountains",
                &[
                    "Orchestre Tout Puissant Marcel Duchamp",
                    "Frànçois & the Atlas Mountains",
                ],
            ),
            ("Röyksopp & Robyn", &["Röyksopp", "Robyn"]),
            ("RÖYKSOPP AND ROBYN", &["RÖYKSOPP", "ROBYN"]),
            ("röyksopp and robyn", &["röyksopp", "robyn"]),
            ("Lauer x The Golden Filter", &["Lauer", "The Golden Filter"]),
            ("Lauer X The Golden Filter", &["Lauer", "The Golden Filter"]),
            ("Lauer × The Golden Filter", &["Lauer", "The Golden Filter"]),
            (
                "Lauer vs. The Golden Filter",
                &["Lauer", "The Golden Filter"],
            ),
            (
                "Lauer VS The Golden Filter",
                &["Lauer", "The Golden Filter"],
            ),
            (
                "Lauer with The Golden Filter",
                &["Lauer", "The Golden Filter"],
            ),
            ("Lauer + The Golden Filter", &["Lauer", "The Golden Filter"]),
            ("Lauer / The Golden Filter", &["Lauer", "The Golden Filter"]),
            ("Jimi Jules, Black Coffee", &["Jimi Jules", "Black Coffee"]),
            ("Jimi Jules; Black Coffee", &["Jimi Jules", "Black Coffee"]),
            ("Jimi Jules;Black Coffee", &["Jimi Jules", "Black Coffee"]),
            (
                "Jimi Jules  &  Black Coffee",
                &["Jimi Jules", "Black Coffee"],
            ),
            (
                "Jimi Jules\t&\tBlack Coffee",
                &["Jimi Jules", "Black Coffee"],
            ),
            (
                "Jimi Jules\u{00A0}&\u{00A0}Black Coffee",
                &["Jimi Jules", "Black Coffee"],
            ),
            (
                "  Jimi Jules & Black Coffee  ",
                &["Jimi Jules", "Black Coffee"],
            ),
            (
                "Daft Punk feat. Pharrell Williams",
                &["Daft Punk", "Pharrell Williams"],
            ),
            (
                "Daft Punk Feat Pharrell Williams",
                &["Daft Punk", "Pharrell Williams"],
            ),
            (
                "Daft Punk featuring Pharrell Williams",
                &["Daft Punk", "Pharrell Williams"],
            ),
            (
                "Daft Punk ft. Pharrell Williams",
                &["Daft Punk", "Pharrell Williams"],
            ),
            (
                "Daft Punk (feat. Pharrell Williams)",
                &["Daft Punk", "Pharrell Williams"],
            ),
            (
                "Daft Punk [ft. Pharrell Williams]",
                &["Daft Punk", "Pharrell Williams"],
            ),
            (
                "Daft Punk feat. Pharrell Williams & Nile Rodgers",
                &["Daft Punk", "Pharrell Williams", "Nile Rodgers"],
            ),
            (
                "Daft Punk, Pharrell Williams, Nile Rodgers",
                &["Daft Punk", "Pharrell Williams", "Nile Rodgers"],
            ),
            (
                "Jimi Jules feat. Anouk & Jesaya",
                &["Jimi Jules", "Anouk", "Jesaya"],
            ),
            (
                "Bob Dylan & Beyoncé & Sigur Rós",
                &["Bob Dylan", "Beyoncé", "Sigur Rós"],
            ),
            ("Malcolm X & Robyn", &["Malcolm X", "Robyn"]),
            ("Robyn x Malcolm X", &["Robyn", "Malcolm X"]),
            (
                "Simon & Garfunkel & Bob Dylan",
                &["Simon & Garfunkel", "Bob Dylan"],
            ),
            (
                "Tyler, the Creator & Robyn",
                &["Tyler, the Creator", "Robyn"],
            ),
            (
                "Earth, Wind & Fire feat. Robyn",
                &["Earth, Wind & Fire", "Robyn"],
            ),
            ("AC/DC & Robyn", &["AC/DC", "Robyn"]),
            (
                "Florence + the Machine + Robyn",
                &["Florence + the Machine", "Robyn"],
            ),
            // Decomposed "ö", as some taggers write it, still names Röyksopp.
            ("Ro\u{0308}yksopp & Robyn", &["Ro\u{0308}yksopp", "Robyn"]),
        ];
        for (credit, want) in cases {
            check(credit, Some(want));
        }
    }

    #[test]
    fn leaves_names_alone_that_only_look_like_collaborations() {
        for credit in [
            "Earth, Wind & Fire",
            "AC/DC",
            "Simon & Garfunkel",
            "Florence + the Machine",
            "Tyler, the Creator",
            "Frànçois & the Atlas Mountains",
            "Malcolm X",
            // Only one part is an artist.
            "Hardcore X Die",
            "Robyn & Somebody Unknown",
            "Daft Punk feat. A Stranger",
            // No whitespace, no separator.
            "Röyksopp×Robyn",
            "Röyksopp&Robyn",
            "Röyksopp,Robyn",
            "Röyksopp/Robyn",
            "Röyksopp+Robyn",
            "RöyksoppxRobyn",
            "Robynx Röyksopp",
            // A word that only contains a separator.
            "Robyn Andersson",
            "Robyn Xavier",
            "Robyn Feather",
            "Robyn Withers",
            "Robyn & & Röyksopp",
        ] {
            check(credit, None);
        }
    }

    #[test]
    fn junk_is_never_split_and_never_panics() {
        for credit in [
            "",
            " ",
            "\t\n",
            "&",
            " & ",
            "& & &",
            ",,,",
            ";;;",
            "; ; ;",
            "feat.",
            " feat. ",
            "x",
            "×",
            "x x x",
            "Robyn &",
            "& Robyn",
            "Robyn,",
            ", Robyn",
            "Robyn;",
            ";Robyn",
            "Robyn x",
            "x Robyn",
            "(feat. Robyn)",
            "Robyn (",
            "Robyn ()",
            "Robyn [feat.]",
            "((((Robyn & Röyksopp",
            "Robyn & Röyksopp))))",
            "😀 & 🎸",
            "\u{202E}Robyn & Röyksopp",
            "\u{0}Robyn & Röyksopp",
            "Robyn\u{0}Röyksopp",
            "Röyksopp & Robyn & ",
        ] {
            let got = split(credit, &library(), Path::new(HERE));
            if let Some(names) = &got {
                assert!(names.len() >= 2, "{credit:?} gave {names:?}");
                for n in names {
                    assert!(
                        library().is_artist(n, Path::new(HERE)),
                        "{credit:?} gave {n:?}"
                    );
                }
            }
        }
        check("Röyksopp & Robyn & ", Some(&["Röyksopp", "Robyn"]));
    }

    #[test]
    fn a_credit_with_too_many_parts_is_left_alone_quickly() {
        let credit = vec!["Robyn"; 60].join(" & ");
        let start = std::time::Instant::now();
        check(&credit, None);
        assert!(start.elapsed() < std::time::Duration::from_millis(100));
    }

    #[test]
    fn the_album_being_credited_does_not_count_as_knowing_its_credit() {
        let mut k = known(&["Lauer", "The Golden Filter"]);
        k.add("Lauer & The Golden Filter", Path::new(HERE));
        assert_eq!(
            split("Lauer & The Golden Filter", &k, Path::new(HERE)),
            Some(vec!["Lauer".to_string(), "The Golden Filter".to_string()])
        );
        k.add("Lone Ranger", Path::new(HERE));
        assert_eq!(split("Lauer & Lone Ranger", &k, Path::new(HERE)), None);
    }

    #[test]
    fn a_repeated_collaboration_is_still_split() {
        let k = known(&["Lauer", "The Golden Filter", "Lauer & The Golden Filter"]);
        assert_eq!(
            split("Lauer & The Golden Filter", &k, Path::new(HERE)),
            Some(vec!["Lauer".to_string(), "The Golden Filter".to_string()])
        );
    }

    #[test]
    fn known_artists_come_from_every_field_of_every_album() {
        let track = |dir: &str, artist: &str, album_artist: &str| crate::library::Album {
            dir: PathBuf::from(dir),
            items: vec![crate::library::Item {
                track: crate::meta::Track {
                    artist: Some(artist.into()),
                    album_artist: Some(album_artist.into()),
                    artists: vec!["Listed".into()],
                    ..Default::default()
                },
                size: 0,
                mtime: 0,
                added: 0,
            }],
        };
        let k = Known::from_albums(&[track("/a", "Robyn", "Röyksopp")]);
        for name in ["Robyn", "robyn", "ROYKSOPP", "Listed"] {
            assert!(k.is_artist(name, Path::new("/b")), "{name}");
            assert!(!k.is_artist(name, Path::new("/a")), "{name} only on /a");
        }
        assert!(!k.is_artist("", Path::new("/b")));
    }

    /// Random credits built from separators, names and awkward characters:
    /// whatever comes back is made of known artists, each a non-empty,
    /// trimmed piece of the credit.
    #[test]
    fn fuzzed_credits_split_only_into_known_pieces() {
        let atoms = [
            "Robyn",
            "Röyksopp",
            "Malcolm X",
            "AC/DC",
            "Earth, Wind & Fire",
            "Lauer",
            " & ",
            " x ",
            " X ",
            " × ",
            ", ",
            "; ",
            ";",
            " feat. ",
            " (feat. ",
            ")",
            "[",
            "]",
            " ",
            "  ",
            "\u{00A0}",
            "&",
            "x",
            "×",
            "😀",
            "\u{0308}",
            "\u{0}",
            "/",
            " / ",
            " + ",
            "and",
            " and ",
            "Feat",
            "(",
            "Unknown",
            ",",
        ];
        let mut seed: u64 = 0x5eed;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        let k = library();
        let here = Path::new(HERE);
        for _ in 0..20_000 {
            let n = 1 + next() % 8;
            let credit: String = (0..n).map(|_| atoms[next() % atoms.len()]).collect();
            if let Some(names) = split(&credit, &k, here) {
                assert!(names.len() >= 2, "{credit:?}");
                for name in &names {
                    assert_eq!(name.trim(), name, "{credit:?}");
                    assert!(!name.is_empty(), "{credit:?}");
                    assert!(credit.contains(name.as_str()), "{credit:?} gave {name:?}");
                    assert!(k.is_artist(name, here), "{credit:?} gave {name:?}");
                }
            }
        }
    }
}
