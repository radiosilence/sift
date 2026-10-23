#![cfg(feature = "tagger")]

//! Each artist on a collaboration credited in the files, against a local
//! server standing in for MusicBrainz.

use std::path::Path;

use sift::credits::Known;
use sift::musicbrainz::MusicBrainz;
use sift::{Config, Importer, Outcome, meta};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const RELEASE: &str = r#"{
  "id": "rel", "title": "Revenant du nord / Siilent", "date": "2025-08-01",
  "artist-credit": [
    {"name": "Orchestre Tout Puissant Marcel Duchamp", "joinphrase": " × ", "artist": {"id": "otpmd", "name": "Orchestre Tout Puissant Marcel Duchamp"}},
    {"name": "Frànçois & the Atlas Mountains", "joinphrase": "", "artist": {"id": "fatm", "name": "Frànçois & the Atlas Mountains"}}
  ],
  "release-group": {"id": "rg", "primary-type": "Single", "first-release-date": "2025-08-01"},
  "label-info": [],
  "media": [{"position": 1, "format": "Digital Media", "tracks": [
    {"id": "t1", "position": 1, "title": "Revenant du nord", "length": 1000, "recording": {"id": "r1"}},
    {"id": "t2", "position": 2, "title": "Siilent", "length": 1000, "recording": {"id": "r2"}}
  ]}]
}"#;

const CREDIT: &str = "Orchestre Tout Puissant Marcel Duchamp × Frànçois & the Atlas Mountains";

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
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
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

/// Two tracks tagged with the one-string credit, as files carry it before
/// each artist is credited; `release` adds the ids an older import wrote.
fn album(dir: &Path, release: bool) {
    for (n, title) in [(1, "Revenant du nord"), (2, "Siilent")] {
        let f = dir.join(format!("{n}.wav"));
        wav(&f);
        meta::write(
            &f,
            &meta::Tags {
                title: title.into(),
                artist: CREDIT.into(),
                album: "Revenant du nord / Siilent".into(),
                album_artist: CREDIT.into(),
                track: n,
                track_total: 2,
                disc: 1,
                disc_total: 1,
                mb_album_id: release.then(|| "rel".into()),
                mb_track_id: release.then(|| format!("t{n}")),
                mb_artist_ids: if release {
                    vec!["otpmd".into()]
                } else {
                    Vec::new()
                },
                ..Default::default()
            },
            None,
            false,
        )
        .unwrap();
    }
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

fn both() -> Vec<String> {
    vec![
        "Orchestre Tout Puissant Marcel Duchamp".into(),
        "Frànçois & the Atlas Mountains".into(),
    ]
}

#[tokio::test]
async fn an_import_credits_every_artist_on_the_release() {
    let root = tempfile::tempdir().unwrap();
    let lib = root.path().join("lib");
    let src = root.path().join("download");
    album(&src, false);
    let Outcome::Imported { dir, .. } = importer(&lib)
        .await
        .import(&src, Some("rel"))
        .await
        .unwrap()
    else {
        panic!("expected an import");
    };
    let file = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "wav"))
        .unwrap();
    let t = meta::read(&file).unwrap();
    assert_eq!(
        t.artist.as_deref(),
        Some(CREDIT),
        "the credit reads as before"
    );
    assert_eq!(t.artists, both());
    assert_eq!(t.album_artists, both());
    assert_eq!(t.mb_artist_ids, ["otpmd", "fatm"]);
    assert_eq!(t.mb_album_artist_ids, ["otpmd", "fatm"]);
}

#[tokio::test]
async fn files_from_an_older_import_are_credited_from_their_release() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("lib").join("album");
    album(&dir, true);
    let importer = importer(&root.path().join("lib")).await;
    let credited = importer.credit(&dir, &Known::default()).await.unwrap();
    assert_eq!(credited.files, 2, "{:?}", credited.problems);
    let t = meta::read(&dir.join("2.wav")).unwrap();
    assert_eq!(t.artists, both());
    assert_eq!(t.mb_artist_ids, ["otpmd", "fatm"]);
    assert_eq!(t.album.as_deref(), Some("Revenant du nord / Siilent"));

    let again = importer.credit(&dir, &Known::default()).await.unwrap();
    assert_eq!(again.files, 0, "credited files are left alone");
}

#[tokio::test]
async fn files_without_a_release_are_split_only_by_known_artists() {
    let root = tempfile::tempdir().unwrap();
    let lib = root.path().join("lib");
    let dir = lib.join("album");
    album(&dir, false);
    let importer = importer(&lib).await;

    let unknown = importer.credit(&dir, &Known::default()).await.unwrap();
    assert_eq!(unknown.files, 0, "nothing is guessed");
    assert!(meta::read(&dir.join("1.wav")).unwrap().artists.is_empty());

    let mut known = Known::default();
    known.add("Orchestre Tout Puissant Marcel Duchamp", &lib.join("a"));
    known.add("Frànçois & the Atlas Mountains", &lib.join("b"));
    let split = importer.credit(&dir, &known).await.unwrap();
    assert_eq!(split.files, 2);
    let t = meta::read(&dir.join("1.wav")).unwrap();
    assert_eq!(t.artists, both());
    assert!(t.mb_artist_ids.is_empty(), "no ids are invented");
    assert_eq!(t.artist.as_deref(), Some(CREDIT));
}

#[tokio::test]
async fn a_credit_plan_lists_each_write_and_makes_none() {
    let root = tempfile::tempdir().unwrap();
    let lib = root.path().join("lib");
    let dir = lib.join("album");
    album(&dir, true);
    let importer = importer(&lib).await;

    let plan = importer.credit_plan(&dir, &Known::default()).await.unwrap();
    assert_eq!(plan.files.len(), 2, "{:?}", plan.problems);
    let first = &plan.files[0];
    assert_eq!(first.artists, both());
    assert_eq!(first.album_artist_ids, ["otpmd", "fatm"]);
    assert_eq!(first.release.as_deref(), Some("rel"));
    assert!(
        meta::read(&dir.join("1.wav")).unwrap().artists.is_empty(),
        "the plan writes nothing"
    );

    let mut known = Known::default();
    known.add("Orchestre Tout Puissant Marcel Duchamp", &lib.join("a"));
    known.add("Frànçois & the Atlas Mountains", &lib.join("b"));
    let untagged = lib.join("untagged");
    album(&untagged, false);
    let split = importer.credit_plan(&untagged, &known).await.unwrap();
    assert_eq!(split.files.len(), 2);
    assert_eq!(split.files[0].release, None, "split from the known artists");
    assert!(split.files[0].artist_ids.is_empty());
}
