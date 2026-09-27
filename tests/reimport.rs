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
