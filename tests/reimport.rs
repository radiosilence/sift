#![cfg(feature = "tagger")]

//! Re-importing an album already in the library against a local server
//! standing in for MusicBrainz.

use std::path::Path;

use sift::musicbrainz::MusicBrainz;
use sift::{Config, Importer, Outcome, meta};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const RELEASE: &str = r#"{
  "id": "rel", "title": "Monster", "date": "1994-09-27", "country": "US",
  "artist-credit": [{"name": "R.E.M.", "joinphrase": "", "artist": {"id": "a", "name": "R.E.M."}}],
  "release-group": {"id": "rg", "primary-type": "Album", "first-release-date": "1994-09-27"},
  "label-info": [],
  "media": [{"position": 1, "format": "CD", "tracks": [
    {"id": "t1", "position": 1, "title": "What's the Frequency, Kenneth?", "length": 1000, "recording": {"id": "r1"}},
    {"id": "t2", "position": 2, "title": "Crush With Eyeliner", "length": 1000, "recording": {"id": "r2"}}
  ]}]
}"#;

/// Answers every request with `RELEASE`.
async fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/ws/2", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let mut req = Vec::new();
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{RELEASE}",
                RELEASE.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    base
}

/// One second of silent 16-bit mono WAV.
fn wav(path: &Path) {
    let data = 88_200u32;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    for v in [
        16u32.to_le_bytes().as_slice(),
        &1u16.to_le_bytes(),
        &1u16.to_le_bytes(),
    ] {
        b.extend_from_slice(v);
    }
    b.extend_from_slice(&44_100u32.to_le_bytes());
    b.extend_from_slice(&88_200u32.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    b.resize(b.len() + data as usize, 0);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b).unwrap();
}

fn tag(path: &Path, album: &str, title: &str, n: u32) {
    meta::write(
        path,
        &meta::Tags {
            title: title.into(),
            artist: "R.E.M.".into(),
            album: album.into(),
            album_artist: "R.E.M.".into(),
            track: n,
            track_total: 2,
            disc: 1,
            disc_total: 1,
            ..Default::default()
        },
        None,
        false,
    )
    .unwrap();
}

async fn importer(lib: &Path) -> Importer {
    let cfg = Config {
        directory: lib.to_path_buf(),
        fetch_art: false,
        move_files: false,
        cache_dir: None,
        ..Config::default()
    };
    Importer::with_musicbrainz(cfg, MusicBrainz::with_base(&serve().await, "test"))
}

#[tokio::test]
async fn a_mistagged_album_is_retagged_and_refiled_with_its_cover() {
    let root = tempfile::tempdir().unwrap();
    let lib = root.path().join("lib");
    let old = lib.join("REM").join("Monstre");
    for (n, title) in [(1, "Kenneth"), (2, "Crush")] {
        let f = old.join(format!("{n}.wav"));
        wav(&f);
        tag(&f, "Monstre", title, n);
    }
    std::fs::write(old.join("cover.jpg"), b"jpg").unwrap();

    let outcome = importer(&lib)
        .await
        .reimport(&old, Some("rel"))
        .await
        .unwrap();
    let Outcome::Imported { dir, .. } = outcome else {
        panic!("expected an import, got {outcome:?}");
    };
    assert_eq!(dir, lib.join("R.E.M_").join("Monster"));
    assert!(
        dir.join("cover.jpg").exists(),
        "the cover follows the album"
    );
    assert!(
        !lib.join("REM").exists(),
        "no empty directories are left behind"
    );
    let track = meta::read(&dir.join("02 Crush With Eyeliner.wav")).unwrap();
    assert_eq!(track.album.as_deref(), Some("Monster"));
    assert_eq!(track.mb_album_id.as_deref(), Some("rel"));
}

#[tokio::test]
async fn an_album_already_in_place_is_retagged_where_it_is() {
    let root = tempfile::tempdir().unwrap();
    let lib = root.path().join("lib");
    let dir = lib.join("R.E.M_").join("Monster");
    for (n, title) in [
        (1, "What's the Frequency, Kenneth_"),
        (2, "Crush With Eyeliner"),
    ] {
        let f = dir.join(format!("0{n} {title}.wav"));
        wav(&f);
        tag(&f, "Monster", title, n);
    }
    let outcome = importer(&lib)
        .await
        .reimport(&dir, Some("rel"))
        .await
        .unwrap();
    assert!(matches!(outcome, Outcome::Imported { .. }), "{outcome:?}");
    let track = meta::read(&dir.join("02 Crush With Eyeliner.wav")).unwrap();
    assert_eq!(
        track.mb_album_id.as_deref(),
        Some("rel"),
        "tags were rewritten in place"
    );
}

#[tokio::test]
async fn a_replacing_import_sets_the_filed_copy_aside() {
    let root = tempfile::tempdir().unwrap();
    let (lib, bin) = (root.path().join("lib"), root.path().join("bin"));
    let fetched = |name: &str| {
        let dir = root.path().join(name);
        for (n, title) in [(1, "Kenneth"), (2, "Crush")] {
            let f = dir.join(format!("{n}.wav"));
            wav(&f);
            tag(&f, "Monster", title, n);
        }
        dir
    };
    let importer = importer(&lib).await;
    let Outcome::Imported { dir: filed, .. } = importer
        .import(&fetched("first"), Some("rel"))
        .await
        .unwrap()
    else {
        panic!("the first copy should import");
    };
    // Stands for whatever marks the filed copy as the damaged one.
    std::fs::write(filed.join("old.txt"), b"old").unwrap();

    let Outcome::Imported { dir, log, .. } = importer
        .import_replacing(&fetched("second"), Some("rel"), &bin)
        .await
        .unwrap()
    else {
        panic!("the second copy should import");
    };
    assert_eq!(dir, filed);
    assert!(
        !dir.join("old.txt").exists(),
        "the old copy left the library"
    );
    assert!(
        dir.join("02 Crush With Eyeliner.wav").exists(),
        "the new copy is filed"
    );
    let aside = bin.join(filed.strip_prefix(&lib).unwrap());
    assert!(
        aside.join("old.txt").exists(),
        "the old copy is in the bin: {log}"
    );
    assert!(aside.join("02 Crush With Eyeliner.wav").exists());
}

#[tokio::test]
async fn a_rematch_names_the_release_a_reimport_would_apply_and_changes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let lib = root.path().join("lib");
    let dir = lib.join("REM").join("Monster");
    for (n, title) in [
        (1, "What's the Frequency, Kenneth?"),
        (2, "Crush With Eyeliner"),
    ] {
        let f = dir.join(format!("{n}.wav"));
        wav(&f);
        meta::write(
            &f,
            &meta::Tags {
                title: title.into(),
                artist: "R.E.M.".into(),
                album: "Monster".into(),
                album_artist: "R.E.M.".into(),
                track: n,
                track_total: 2,
                mb_album_id: Some("rel".into()),
                ..Default::default()
            },
            None,
            false,
        )
        .unwrap();
    }
    let rematch = importer(&lib).await.rematch(&dir).await.unwrap();
    let best = rematch.best.expect("the tagged release is found");
    assert_eq!(best.release.id, "rel");
    assert_eq!(best.pairs.len(), 2);
    assert!(best.missing.is_empty() && best.extra.is_empty());
    assert_eq!(
        rematch.candidates.first().map(|c| c.id.as_str()),
        Some("rel")
    );
    assert!(dir.join("1.wav").exists(), "nothing moved");
}

#[tokio::test]
async fn a_retag_plan_lists_each_change_and_destination_and_writes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let lib = root.path().join("lib");
    let old = lib.join("REM").join("Monstre");
    for (n, title) in [(1, "Kenneth"), (2, "Crush")] {
        let f = old.join(format!("{n}.wav"));
        wav(&f);
        tag(&f, "Monstre", title, n);
    }
    std::fs::write(old.join("cover.jpg"), b"jpg").unwrap();
    let plan = importer(&lib).await.retag_plan(&old, "rel").await.unwrap();
    assert_eq!(plan.refused, None);
    let monster = lib.join("R.E.M_").join("Monster");
    assert_eq!(plan.companions.len(), 1);
    assert_eq!(plan.companions[0].to, monster.join("cover.jpg"));
    assert!(!plan.companions[0].stays);
    assert!(plan.untouched.is_empty());
    let crush = plan
        .files
        .iter()
        .find(|f| f.path.ends_with("2.wav"))
        .unwrap();
    assert_eq!(
        crush.to.as_deref(),
        Some(monster.join("02 Crush With Eyeliner.wav").as_path())
    );
    let album = crush.changes.iter().find(|c| c.field == "album").unwrap();
    assert_eq!(
        (album.old.as_deref(), album.new.as_deref()),
        (Some("Monstre"), Some("Monster"))
    );
    assert!(
        crush
            .changes
            .iter()
            .any(|c| c.field == "mb_albumid" && c.new.as_deref() == Some("rel"))
    );
    assert!(
        !crush.changes.iter().any(|c| c.field == "artist"),
        "an unchanged field is not listed"
    );
    assert_eq!(
        meta::read(&old.join("2.wav")).unwrap().album.as_deref(),
        Some("Monstre")
    );
}

#[tokio::test]
async fn a_refused_retag_plan_still_lists_each_file() {
    let root = tempfile::tempdir().unwrap();
    let lib = root.path().join("lib");
    let old = lib.join("REM").join("Monstre");
    for (n, title) in [(1, "Kenneth"), (2, "Crush")] {
        let f = old.join(format!("{n}.wav"));
        wav(&f);
        tag(&f, "Monstre", title, n);
    }
    let taken = lib
        .join("R.E.M_")
        .join("Monster")
        .join("02 Crush With Eyeliner.wav");
    wav(&taken);
    let plan = importer(&lib).await.retag_plan(&old, "rel").await.unwrap();
    assert!(
        plan.refused
            .is_some_and(|r| r.contains("already in the library"))
    );
    assert_eq!(plan.files.len(), 2);
    assert!(plan.files.iter().all(|f| !f.changes.is_empty()));
}

#[cfg(unix)]
#[tokio::test]
async fn an_import_that_fails_part_way_takes_back_the_copies_it_made() {
    let root = tempfile::tempdir().unwrap();
    let lib = root.path().join("lib");
    let src = root.path().join("in");
    for (n, title) in [(1, "Kenneth"), (2, "Crush")] {
        let f = src.join(format!("{n}.wav"));
        wav(&f);
        tag(&f, "Monster", title, n);
    }
    // A dangling link reads as nothing there when planning, but the second
    // track's copy cannot claim its name: the first copy must come back out.
    let album = lib.join("R.E.M_").join("Monster");
    std::fs::create_dir_all(&album).unwrap();
    let link = album.join("02 Crush With Eyeliner.wav");
    std::os::unix::fs::symlink(root.path().join("nowhere"), &link).unwrap();
    let result = importer(&lib).await.import(&src, Some("rel")).await;
    assert!(
        matches!(result, Err(sift::ImportError::Io(_))),
        "the second copy fails: {result:?}"
    );
    assert_eq!(walk(&lib), [link], "nothing but the link is left");
    assert!(src.join("1.wav").exists() && src.join("2.wav").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_read_only_source_is_copied_in_and_tagged() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let lib = root.path().join("lib");
    let src = root.path().join("in");
    for (n, title) in [(1, "Kenneth"), (2, "Crush")] {
        let f = src.join(format!("{n}.wav"));
        wav(&f);
        tag(&f, "Monster", title, n);
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o444)).unwrap();
    }
    let outcome = importer(&lib)
        .await
        .import(&src, Some("rel"))
        .await
        .unwrap();
    let Outcome::Imported { dir, .. } = outcome else {
        panic!("expected an import, got {outcome:?}");
    };
    let filed = walk(&dir);
    assert_eq!(filed.len(), 2);
    for f in &filed {
        assert_eq!(meta::read(f).unwrap().mb_album_id.as_deref(), Some("rel"));
        assert_eq!(
            std::fs::metadata(f).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }
    assert_eq!(
        std::fs::metadata(src.join("1.wav"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o444,
        "the source is left as it was"
    );
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}
