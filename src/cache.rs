//! Web service responses kept on disk, shared by the MusicBrainz and
//! Discogs clients.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Responses under one directory, each file named for its request.
#[derive(Clone)]
pub(crate) struct Cache {
    dir: PathBuf,
    /// Before each file's name, so two services can share a directory.
    prefix: &'static str,
    /// Also prune files named as before prefixes: sixteen hex digits.
    legacy: bool,
}

impl Cache {
    /// Responses in `dir`, older ones pruned once per process in the
    /// background: nothing reads a response past its time to live, so
    /// without pruning the directory only grows.
    pub(crate) fn new(dir: PathBuf, prefix: &'static str, legacy: bool, keep: Duration) -> Self {
        static PRUNED: std::sync::Mutex<Vec<(PathBuf, &str)>> = std::sync::Mutex::new(Vec::new());
        let first = {
            let mut pruned = PRUNED.lock().unwrap_or_else(|e| e.into_inner());
            let key = (dir.clone(), prefix);
            let first = !pruned.contains(&key);
            if first {
                pruned.push(key);
            }
            first
        };
        let cache = Self {
            dir,
            prefix,
            legacy,
        };
        if first {
            let c = cache.clone();
            std::thread::spawn(move || c.prune(keep));
        }
        cache
    }

    /// The file a request's response is kept in. Named by FNV-1a over the
    /// path and each query pair, which stays the same from one build to the
    /// next, as std's hasher does not promise to.
    pub(crate) fn path(&self, path: &str, query: &[(&str, &str)]) -> PathBuf {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut feed = |bytes: &[u8]| {
            for b in bytes {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        feed(path.as_bytes());
        for (k, v) in query {
            feed(b"\0");
            feed(k.as_bytes());
            feed(b"=");
            feed(v.as_bytes());
        }
        self.dir.join(format!("{}{h:016x}.json", self.prefix))
    }

    /// The response kept for `file`, if it is younger than `ttl`.
    pub(crate) async fn read(&self, file: &Path, ttl: Duration) -> Option<Vec<u8>> {
        let fresh = tokio::fs::metadata(file)
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age < ttl);
        if !fresh {
            return None;
        }
        tokio::fs::read(file).await.ok()
    }

    /// Keep `body` as `file`'s response, through a temporary file renamed
    /// into place, so a reader never sees half of it. Failures are ignored:
    /// the cache only saves requests.
    pub(crate) async fn write(&self, file: &Path, body: &[u8]) {
        let _ = tokio::fs::create_dir_all(&self.dir).await;
        let tmp = crate::paths::scratch_beside(file);
        if tokio::fs::write(&tmp, body).await.is_ok()
            && tokio::fs::rename(&tmp, file).await.is_err()
        {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
    }

    /// Remove this cache's responses older than `keep`, and temporary files
    /// left by a write that never finished.
    fn prune(&self, keep: Duration) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let legacy = self.legacy
                && name.strip_suffix(".json").is_some_and(|stem| {
                    stem.len() == 16 && stem.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                });
            let ours = (name.starts_with(self.prefix) && name.ends_with(".json")) || legacy;
            let scratch = crate::paths::is_scratch(&path);
            if !ours && !scratch {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let age = if scratch {
                crate::paths::scratch_age(&meta)
            } else {
                meta.modified().ok().and_then(|t| t.elapsed().ok())
            };
            let old = age.is_some_and(|age| age > keep);
            if old {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_named_the_same_every_time_and_apart_from_others() {
        let c = Cache {
            dir: "/c".into(),
            prefix: "x-",
            legacy: false,
        };
        let a = c.path("release/1", &[("inc", "recordings")]);
        assert_eq!(a, c.path("release/1", &[("inc", "recordings")]));
        assert_ne!(a, c.path("release/1", &[("inc", "labels")]));
        assert_ne!(
            c.path("ab", &[("c", "d")]),
            c.path("a", &[("bc", "d")]),
            "the separators keep fields apart"
        );
        assert!(a.file_name().unwrap().to_str().unwrap().starts_with("x-"));
    }

    #[tokio::test]
    async fn a_response_written_reads_back_and_only_old_ones_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cache {
            dir: dir.path().into(),
            prefix: "x-",
            legacy: true,
        };
        let file = c.path("a", &[]);
        c.write(&file, b"{}").await;
        assert_eq!(c.read(&file, Duration::from_secs(60)).await.unwrap(), b"{}");
        let other = dir.path().join("y-1.json");
        let unprefixed = dir.path().join("0123456789abcdef.json");
        std::fs::write(&other, b"{}").unwrap();
        std::fs::write(&unprefixed, b"{}").unwrap();
        c.prune(Duration::ZERO);
        assert!(!file.exists(), "expired");
        assert!(!unprefixed.exists(), "named as before prefixes");
        assert!(other.exists(), "another cache's file is left alone");
    }
}
