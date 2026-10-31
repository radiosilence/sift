#![cfg(feature = "tagger")]

//! Fields lofty has no generic key for survive every kind of sift write.

use std::path::{Path, PathBuf};

use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::flac::FlacFile;
use lofty::ogg::tag::VorbisComments;
use lofty::ogg::{OggPictureStorage, OpusFile};
use lofty::tag::TagExt;
use sift::meta::{self, Tags};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

/// A copy of fixture `name` carrying `FOO=bar` and `BAR` twice.
fn planted(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::copy(Path::new(FIXTURES).join(name), &path).unwrap();
    let mut vc = VorbisComments::default();
    vc.push("FOO".into(), "bar".into());
    vc.push("BAR".into(), "one".into());
    vc.push("BAR".into(), "two".into());
    vc.save_to_path(&path, WriteOptions::default()).unwrap();
    path
}

fn comments(path: &Path) -> VorbisComments {
    let mut file = std::fs::File::open(path).unwrap();
    let opts = ParseOptions::new();
    if path.extension().is_some_and(|e| e == "flac") {
        FlacFile::read_from(&mut file, opts)
            .unwrap()
            .vorbis_comments()
            .cloned()
            .unwrap()
    } else {
        OpusFile::read_from(&mut file, opts)
            .unwrap()
            .vorbis_comments()
            .clone()
    }
}

fn assert_kept(path: &Path) {
    let vc = comments(path);
    assert_eq!(vc.get("FOO"), Some("bar"), "{}", path.display());
    assert_eq!(
        vc.get_all("BAR").collect::<Vec<_>>(),
        ["one", "two"],
        "{}",
        path.display()
    );
}

#[test]
fn unmapped_comments_survive_every_write() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["silence.flac", "silence.opus"] {
        let path = planted(dir.path(), name);
        meta::set(&path, &[("title".into(), Some("New".into()))]).unwrap();
        assert_kept(&path);
        assert_eq!(meta::read(&path).unwrap().title.as_deref(), Some("New"));

        let tags = Tags {
            title: "Written".into(),
            artist: "A".into(),
            album: "B".into(),
            album_artist: "A".into(),
            track: 1,
            ..Default::default()
        };
        meta::write(&path, &tags, None, false).unwrap();
        assert_kept(&path);
        assert_eq!(meta::read(&path).unwrap().title.as_deref(), Some("Written"));

        meta::set_cover(&path, include_bytes!("fixtures/cover.png")).unwrap();
        assert_kept(&path);
        assert_eq!(comments(&path).pictures().len(), 1);
    }
}
