//! The MusicBrainz client against a local server that answers from a script,
//! so retries and pacing are exercised without touching the real service.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use sift::musicbrainz::MusicBrainz;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const EMPTY: &str = r#"{"created":"2026-01-01T00:00:00Z","count":0,"offset":0,"releases":[]}"#;

/// Serves `script` in order, one response per connection, repeating the last
/// entry once it runs out. Returns the base URL and a request counter.
async fn serve(script: Vec<(u16, &'static str, &'static str)>) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/ws/2", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let (status, headers, body) = script[n.min(script.len() - 1)];
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
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{headers}\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    (base, hits)
}

#[tokio::test]
async fn refusals_and_server_errors_are_waited_out() {
    let (base, hits) = serve(vec![
        (503, "retry-after: 1\r\n", ""),
        (502, "retry-after: 1\r\n", ""),
        (429, "retry-after: 1\r\n", ""),
        (200, "", EMPTY),
    ])
    .await;
    let mb = MusicBrainz::with_base(&base, "test");
    let started = Instant::now();
    let found = mb.search_releases("a", "b", 5).await.unwrap();
    assert!(found.is_empty());
    assert_eq!(hits.load(Ordering::SeqCst), 4);
    assert!(started.elapsed() >= Duration::from_secs(3));
}

#[tokio::test]
async fn a_bad_request_is_not_retried() {
    let (base, hits) = serve(vec![(400, "", "{}")]).await;
    let mb = MusicBrainz::with_base(&base, "test");
    let err = mb.search_releases("a", "b", 5).await.unwrap_err();
    assert!(!err.is_transient());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_refusal_slows_the_requests_after_it() {
    let (base, _) = serve(vec![(503, "retry-after: 1\r\n", ""), (200, "", EMPTY)]).await;
    let mb = MusicBrainz::with_base(&base, "test");
    mb.search_releases("a", "b", 5).await.unwrap();
    // The refusal doubled the spacing and one success only relaxes it a
    // little, so the next request waits well over the usual 1.1 seconds.
    let started = Instant::now();
    mb.search_releases("a", "c", 5).await.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(1800));
}

#[tokio::test]
async fn an_unreachable_service_is_transient() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/ws/2", listener.local_addr().unwrap());
    drop(listener);
    let mb = MusicBrainz::with_base(&base, "test");
    let err = tokio::time::timeout(Duration::from_secs(1), mb.search_releases("a", "b", 5)).await;
    // Still retrying when the timeout fires: a refused connection is waited
    // out, not reported.
    assert!(err.is_err());
}

#[tokio::test]
async fn genres_come_from_the_release_group_when_the_release_has_none() {
    let body = r#"{"genres":[],
        "release-group":{"genres":[{"name":"dubstep","count":5},{"name":"future garage","count":4},{"name":"electronic","count":2},{"name":"hauntology","count":1}]},
        "artist-credit":[{"artist":{"genres":[{"name":"ambient","count":9}]}}]}"#;
    let (base, _) = serve(vec![(200, "", body)]).await;
    let mb = MusicBrainz::with_base(&base, "test");
    assert_eq!(
        mb.genres("rel").await.unwrap(),
        ["Dubstep", "Future Garage", "Electronic"],
        "top three, none under a third of the top's votes"
    );
}
