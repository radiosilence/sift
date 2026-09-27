//! The LRCLIB client against a local server standing in for it.

use sift::lyrics::{Client, Lyrics};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Answers by the track name in the query string.
async fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            let (status, body) = if req.contains("track_name=Synced") {
                (
                    "200 OK",
                    r#"{"instrumental":false,"syncedLyrics":"[00:01.00] hi","plainLyrics":"hi"}"#,
                )
            } else if req.contains("track_name=Plain") {
                (
                    "200 OK",
                    r#"{"instrumental":false,"syncedLyrics":null,"plainLyrics":"words"}"#,
                )
            } else if req.contains("track_name=Beat") {
                (
                    "200 OK",
                    r#"{"instrumental":true,"syncedLyrics":null,"plainLyrics":null}"#,
                )
            } else {
                ("404 Not Found", r#"{"message":"not found"}"#)
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    base
}

#[tokio::test]
async fn synced_is_preferred_then_plain_and_instrumentals_have_none() {
    let client = Client::with_base(&serve().await);
    assert_eq!(
        client.get("A", "Synced", "B", 100).await.unwrap(),
        Lyrics::Synced("[00:01.00] hi".into())
    );
    assert_eq!(
        client.get("A", "Plain", "B", 100).await.unwrap(),
        Lyrics::Plain("words".into())
    );
    assert_eq!(
        client.get("A", "Beat", "B", 100).await.unwrap(),
        Lyrics::Instrumental
    );
    assert_eq!(
        client.get("A", "Nope", "B", 100).await.unwrap(),
        Lyrics::NotFound
    );
}
