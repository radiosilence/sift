#![cfg(feature = "tagger")]

//! The AcoustID client against a local server, and fingerprints of audio
//! made on the spot.

use std::path::Path;
use std::time::Duration;

use sift::acoustid::{AcoustId, Fingerprint, fingerprint};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Answers one request with `body`, returning what was asked.
async fn serve(body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v2", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut req = Vec::new();
        let mut buf = [0u8; 65536];
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            req.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&req);
            if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                let len = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if rest.len() >= len || n == 0 {
                    break;
                }
            }
        }
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(resp.as_bytes()).await.unwrap();
        String::from_utf8(req).unwrap()
    });
    (base, handle)
}

#[tokio::test]
async fn a_lookup_keeps_confident_recordings_and_their_releases() {
    let (base, server) = serve(
        r#"{"status":"ok","results":[
            {"id":"a","score":0.97,"recordings":[
                {"id":"rec1","releases":[{"id":"rel1"},{"id":"rel2"}]},
                {"id":"rec1b"}]},
            {"id":"b","score":0.2,"recordings":[{"id":"noise","releases":[{"id":"relx"}]}]}
        ]}"#,
    )
    .await;
    let client = AcoustId::with_base(&base, "key", "test");
    let found = client
        .lookup(&Fingerprint {
            encoded: "AQAA".into(),
            duration: 241,
        })
        .await
        .unwrap();
    let mut recordings: Vec<_> = found.recordings.into_iter().collect();
    recordings.sort();
    assert_eq!(recordings, ["rec1", "rec1b"]);
    let mut releases: Vec<_> = found.releases.into_iter().collect();
    releases.sort();
    assert_eq!(releases, ["rel1", "rel2"]);
    let req = server.await.unwrap();
    assert!(req.starts_with("POST /v2/lookup"), "{req}");
    for part in [
        "client=key",
        "duration=241",
        "fingerprint=AQAA",
        "meta=recordings+releaseids",
    ] {
        assert!(req.contains(part), "{part} missing from {req}");
    }
}

#[tokio::test]
async fn a_refused_lookup_says_why() {
    let (base, _server) =
        serve(r#"{"status":"error","error":{"code":4,"message":"invalid API key"}}"#).await;
    let err = AcoustId::with_base(&base, "bad", "test")
        .lookup(&Fingerprint {
            encoded: "AQAA".into(),
            duration: 1,
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invalid API key"), "{err}");
}

/// `secs` of a chord at 44.1 kHz, 16-bit stereo.
fn wav(path: &Path, secs: u32) {
    let rate = 44_100u32;
    let frames = rate * secs;
    let data = frames * 4;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 4).to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for n in 0..frames {
        let t = f64::from(n) / f64::from(rate);
        let note = if (n / rate) % 2 == 0 { 440.0 } else { 660.0 };
        let v = (0.3 * (2.0 * std::f64::consts::PI * note * t).sin() * f64::from(i16::MAX)) as i16;
        b.extend_from_slice(&v.to_le_bytes());
        b.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

#[test]
fn audio_fingerprints_the_same_each_time_and_differs_from_other_audio() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.wav"), dir.path().join("b.wav"));
    wav(&a, 12);
    wav(&b, 14);
    let fa = fingerprint(&a, Duration::from_secs(12)).unwrap();
    assert!(!fa.encoded.is_empty());
    assert!(
        fa.encoded
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    );
    assert_eq!(fa.duration, 12);
    assert_eq!(
        fingerprint(&a, Duration::from_secs(12)).unwrap().encoded,
        fa.encoded
    );
    assert_ne!(
        fingerprint(&b, Duration::from_secs(14)).unwrap().encoded,
        fa.encoded
    );
}
