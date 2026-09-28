//! The Discogs client against a local server that answers from a script, the
//! same shape as `tests/musicbrainz.rs`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use sift::discogs::Discogs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn serve(script: Vec<(u16, &'static str)>) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let (status, body) = script[n.min(script.len() - 1)];
            let mut buf = [0u8; 4096];
            let mut req = Vec::new();
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                let read = sock.read(&mut buf).await.unwrap();
                if read == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..read]);
            }
            let resp = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    (base, hits)
}

#[tokio::test]
async fn search_returns_result_ids() {
    let body = r#"{"results":[{"id":249504},{"id":1811056}]}"#;
    let (base, _) = serve(vec![(200, body)]).await;
    let discogs = Discogs::with_base(&base, "token", "test");
    let ids = discogs
        .search("Boards of Canada", "Geogaddi")
        .await
        .unwrap();
    assert_eq!(ids, [249504, 1811056]);
}

#[tokio::test]
async fn a_vinyl_tracklist_is_one_disc_numbered_in_order() {
    let body = r#"{
        "title": "Geogaddi",
        "artists": [{"name": "Boards of Canada", "join": ""}],
        "released": "2002-02-18",
        "country": "UK",
        "labels": [{"name": "Warp", "catno": "WARPLP101"}],
        "tracklist": [
            {"position": "A1", "type_": "track", "title": "Ready Lets Go", "duration": "0:59"},
            {"position": "A2", "type_": "track", "title": "Music Is Math", "duration": "5:21"},
            {"position": "B1", "type_": "track", "title": "Beware the Friendly Stranger", "duration": "0:37"}
        ]
    }"#;
    let (base, _) = serve(vec![(200, body)]).await;
    let discogs = Discogs::with_base(&base, "token", "test");
    let release = discogs.release(1811056, false).await.unwrap();
    assert_eq!(release.id, "discogs:1811056");
    assert_eq!(release.date.as_deref(), Some("2002-02-18"));
    assert_eq!(release.media.len(), 1);
    let positions: Vec<u32> = release.media[0].tracks.iter().map(|t| t.position).collect();
    assert_eq!(positions, [1, 2, 3]);
    assert_eq!(release.media[0].tracks[0].length, Some(59_000));
    assert_eq!(release.media[0].tracks[1].length, Some(321_000));
    assert_eq!(
        release.label_info[0].catalog_number.as_deref(),
        Some("WARPLP101")
    );
}

#[tokio::test]
async fn an_index_tracks_sub_tracks_are_flattened_and_optionally_prefixed() {
    let body = r#"{
        "title": "A Mix",
        "artists": [{"name": "DJ Someone", "join": ""}],
        "released": "2019-00-00",
        "tracklist": [
            {"type_": "index", "title": "Continuous Mix", "sub_tracks": [
                {"position": "1", "title": "Track One", "duration": "3:00"},
                {"position": "2", "title": "Track Two", "duration": "4:00"}
            ]}
        ]
    }"#;
    let (base, _) = serve(vec![(200, body)]).await;

    let discogs = Discogs::with_base(&base, "token", "test");
    let plain = discogs.release(1, false).await.unwrap();
    assert_eq!(plain.date.as_deref(), Some("2019"));
    let titles: Vec<String> = plain.media[0]
        .tracks
        .iter()
        .map(|t| t.title.clone())
        .collect();
    assert_eq!(titles, ["Track One", "Track Two"]);

    let (base, _) = serve(vec![(200, body)]).await;
    let discogs = Discogs::with_base(&base, "token", "test");
    let prefixed = discogs.release(1, true).await.unwrap();
    let titles: Vec<String> = prefixed.media[0]
        .tracks
        .iter()
        .map(|t| t.title.clone())
        .collect();
    assert_eq!(
        titles,
        ["Continuous Mix: Track One", "Continuous Mix: Track Two"]
    );
}

#[tokio::test]
async fn an_artists_disambiguation_suffix_is_dropped() {
    let body = r#"{
        "title": "Some Album",
        "artists": [{"name": "Artist (2)", "join": ""}],
        "released": "2020-01-01",
        "tracklist": []
    }"#;
    let (base, _) = serve(vec![(200, body)]).await;
    let discogs = Discogs::with_base(&base, "token", "test");
    let release = discogs.release(1, false).await.unwrap();
    assert_eq!(release.artist(), "Artist");
}
