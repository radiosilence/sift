//! Input sift does not control — tags, file contents, templates — thrown at
//! it at random. None of it may panic, and no tag may move a file outside the
//! library.

use std::path::Path;

use proptest::prelude::*;
use sift::config::{Config, translate};
use sift::meta::{self, Tags, Track};
use sift::musicbrainz::Release;
use sift::paths;

fn release() -> Release {
    serde_json::from_value(serde_json::json!({"id": "r", "title": "x", "media": []})).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Whatever the tags say, the path stays under the library or is refused.
    #[test]
    fn tags_never_escape_the_library(
        artist in ".{0,80}", album in ".{0,80}", title in ".{0,80}",
        track in 0u32..500, disc in 0u32..20, year in "[0-9./-]{0,10}",
    ) {
        let cfg = Config { directory: "/lib".into(), path_default: translate("$albumartist/%if{$year,($year) }$album [$format]/$disc$track. $artist - $title"), ..Config::default() };
        let tags = Tags { album_artist: artist.clone(), artist, album, title, track, disc, date: Some(year), ..Default::default() };
        let local = Track { format: "FLAC".into(), ..Default::default() };
        if let Ok(rel) = paths::render(&cfg, &tags, &local, &release()) {
            let dest = paths::under(Path::new("/lib"), &format!("{rel}.flac"));
            if let Ok(p) = dest {
                prop_assert!(p.starts_with("/lib"));
                prop_assert!(!p.components().any(|c| matches!(c, std::path::Component::ParentDir)));
                for part in rel.split('/') {
                    prop_assert!(part.len() <= 240, "component over the byte ceiling: {}", part.len());
                }
            }
        }
    }

    /// Arbitrary templates, beets or fb2k, never panic the translator or the
    /// engine.
    #[test]
    fn templates_never_panic(template in ".{0,200}") {
        let t = translate(&template);
        let fields = |_: &str| Some("x".to_string());
        let _ = sift::format::format(&t, &fields);
        let _ = sift::format::format(&template, &fields);
    }

    /// Garbage with an audio extension is an error, not a crash.
    #[test]
    fn corrupt_files_are_errors(bytes in proptest::collection::vec(any::<u8>(), 0..4096), ext in prop::sample::select(vec!["flac", "mp3", "m4a", "ogg", "opus", "wav"])) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("x.{ext}"));
        std::fs::write(&path, &bytes).unwrap();
        let _ = meta::read(&path);
    }
}

/// A real FLAC header followed by garbage: past the magic, into the parser.
#[test]
fn truncated_flac_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    for len in [4usize, 8, 42, 100, 1000] {
        let mut bytes = b"fLaC".to_vec();
        bytes.extend(std::iter::repeat_n(0xAB, len));
        let path = dir.path().join(format!("t{len}.flac"));
        std::fs::write(&path, &bytes).unwrap();
        assert!(meta::read(&path).is_err());
    }
}
