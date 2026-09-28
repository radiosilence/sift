//! Folds a featured artist out of the track artist and into the title, as
//! beets' `ftintitle` plugin does: "Burial feat. Four Tet" under album
//! artist "Burial" becomes artist "Burial", title "Song feat. Four Tet".

const TOKENS: &[&str] = &[
    "feat.",
    "feat",
    "featuring",
    "ft.",
    "ft",
    "with",
    "vs.",
    "vs",
    "&",
    "and",
];

/// `None` when the track artist doesn't name a featured artist beyond the
/// album artist, or when folding it in would change nothing.
pub fn apply(
    artist: &str,
    album_artist: &str,
    title: &str,
    drop: bool,
    format: &str,
) -> Option<(String, String)> {
    if artist == album_artist {
        return None;
    }
    // ASCII lowercasing keeps every byte where it was, so positions found in
    // the lowercased copy slice the original. Full Unicode lowercasing can
    // change a string's length ("İ"), and the slice would then land mid-char.
    let lower_artist = artist.to_ascii_lowercase();
    let lower_album_artist = album_artist.to_ascii_lowercase();
    let start = lower_artist.find(&lower_album_artist)?;
    let after = start + lower_album_artist.len();

    let featured =
        find_token(&lower_artist[after..]).map(|(_, tok_end)| artist[after + tok_end..].trim())?;
    if featured.is_empty() {
        return None;
    }

    let lower_title = title.to_lowercase();
    let already_credited = ["feat.", "ft.", "featuring"]
        .iter()
        .any(|t| lower_title.contains(t));
    let new_title = if drop || already_credited {
        title.to_string()
    } else {
        format!("{title} {}", format.replace("{0}", featured))
    };
    Some((album_artist.to_string(), new_title))
}

/// The byte range (within `s`) of the first whole-word featuring token, if
/// any.
fn find_token(s: &str) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for tok in TOKENS {
        let mut from = 0;
        while let Some(rel) = s[from..].find(tok) {
            let tok_start = from + rel;
            let tok_end = tok_start + tok.len();
            let before_ok = s[..tok_start]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric());
            let after_ok = s[tok_end..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric());
            if before_ok && after_ok && best.is_none_or(|(bs, _)| tok_start < bs) {
                best = Some((tok_start, tok_end));
            }
            from = tok_start + 1;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_ascii_names_slice_where_they_were_found() {
        assert_eq!(
            apply("İlhan feat. Björk", "İlhan", "Song", false, "feat. {0}"),
            Some(("İlhan".into(), "Song feat. Björk".into()))
        );
    }

    #[test]
    fn feat_dot() {
        assert_eq!(
            apply(
                "Burial feat. Four Tet",
                "Burial",
                "Song",
                false,
                "feat. {0}"
            ),
            Some(("Burial".into(), "Song feat. Four Tet".into()))
        );
    }

    #[test]
    fn drop_keeps_title() {
        assert_eq!(
            apply("Burial feat. Four Tet", "Burial", "Song", true, "feat. {0}"),
            Some(("Burial".into(), "Song".into()))
        );
    }

    #[test]
    fn title_already_credited_unchanged() {
        assert_eq!(
            apply(
                "Burial feat. Four Tet",
                "Burial",
                "Song (feat. Someone Else)",
                false,
                "feat. {0}"
            ),
            Some(("Burial".into(), "Song (feat. Someone Else)".into()))
        );
    }

    #[test]
    fn album_artist_not_a_prefix_is_none() {
        assert_eq!(
            apply("Four Tet", "Burial", "Song", false, "feat. {0}"),
            None
        );
    }

    #[test]
    fn ampersand() {
        assert_eq!(
            apply("Burial & Four Tet", "Burial", "Song", false, "feat. {0}"),
            Some(("Burial".into(), "Song feat. Four Tet".into()))
        );
    }

    #[test]
    fn custom_format() {
        assert_eq!(
            apply(
                "Burial feat. Four Tet",
                "Burial",
                "Song",
                false,
                "(ft. {0})"
            ),
            Some(("Burial".into(), "Song (ft. Four Tet)".into()))
        );
    }

    #[test]
    fn case_insensitive_token() {
        assert_eq!(
            apply(
                "Burial FEAT. Four Tet",
                "Burial",
                "Song",
                false,
                "feat. {0}"
            ),
            Some(("Burial".into(), "Song feat. Four Tet".into()))
        );
    }

    #[test]
    fn same_as_album_artist_is_none() {
        assert_eq!(apply("Burial", "Burial", "Song", false, "feat. {0}"), None);
    }

    #[test]
    fn no_token_after_album_artist_is_none() {
        assert_eq!(
            apply("Burial and", "Burial", "Song", false, "feat. {0}"),
            None
        );
    }
}
