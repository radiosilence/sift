//! What an import is doing, as it happens, for a caller that shows it.
//!
//! A caller runs an import inside [`scope`]; the importer and its clients
//! report each stage they reach to the sink given there. Outside a scope,
//! reporting does nothing. The sink is held per task, so it follows the
//! import's own future and not tasks it spawns.

use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    SearchingMusicBrainz,
    /// Looking up the releases a search found, to compare against the files.
    Comparing(usize),
    /// Looking up a release already named, by the files or the caller.
    AskingMusicBrainz,
    /// Paused before asking MusicBrainz again.
    WaitingOnMusicBrainz {
        why: Wait,
        retry_in: Duration,
    },
    Fingerprinting,
    AskingDiscogs,
    CoverArt,
    WritingTags,
    ReplayGain,
    Genres,
    Lyrics,
    Moving,
}

/// Why MusicBrainz is being waited on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// Refused with this status.
    Refused(u16),
    /// Searches shed across the whole service, not this client's doing.
    Shed,
    /// No connection, or no answer in time.
    Unreachable,
    /// Keeping to its pace, or to a nearly spent budget.
    Pacing,
}

impl std::fmt::Display for Wait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(429) => f.write_str("rate limited"),
            Self::Refused(s) => write!(f, "HTTP {s}"),
            Self::Shed => f.write_str("searches shed under load"),
            Self::Unreachable => f.write_str("unreachable"),
            Self::Pacing => f.write_str("pacing"),
        }
    }
}

impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SearchingMusicBrainz => f.write_str("searching MusicBrainz"),
            Self::Comparing(n) => write!(
                f,
                "comparing {n} candidate{}",
                if *n == 1 { "" } else { "s" }
            ),
            Self::AskingMusicBrainz => f.write_str("asking MusicBrainz"),
            Self::WaitingOnMusicBrainz { why, retry_in } => {
                write!(
                    f,
                    "waiting on MusicBrainz ({why}), retrying in {}s",
                    retry_in.as_secs_f64().ceil() as u64
                )
            }
            Self::Fingerprinting => f.write_str("fingerprinting on AcoustID"),
            Self::AskingDiscogs => f.write_str("asking Discogs"),
            Self::CoverArt => f.write_str("fetching cover art"),
            Self::WritingTags => f.write_str("writing tags"),
            Self::ReplayGain => f.write_str("measuring ReplayGain"),
            Self::Genres => f.write_str("fetching genres"),
            Self::Lyrics => f.write_str("fetching lyrics"),
            Self::Moving => f.write_str("moving into the library"),
        }
    }
}

type Sink = Arc<dyn Fn(&Stage) + Send + Sync>;

struct Progress {
    sink: Sink,
    /// The stage a wait interrupts, reported again once the wait is over.
    current: Mutex<Option<Stage>>,
}

tokio::task_local! {
    static PROGRESS: Progress;
}

/// Run `fut`, passing each stage it reaches to `sink`.
pub async fn scope<F: Future>(sink: impl Fn(&Stage) + Send + Sync + 'static, fut: F) -> F::Output {
    PROGRESS
        .scope(
            Progress {
                sink: Arc::new(sink),
                current: Mutex::new(None),
            },
            fut,
        )
        .await
}

pub(crate) fn report(stage: Stage) {
    let _ = PROGRESS.try_with(|p| {
        (p.sink)(&stage);
        *p.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(stage);
    });
}

/// A pause inside the current stage, which [`resume`] ends.
pub(crate) fn waiting(stage: Stage) {
    let _ = PROGRESS.try_with(|p| (p.sink)(&stage));
}

/// The scope's sink, for work leaving the task, such as a blocking one.
pub(crate) fn sink() -> Option<Sink> {
    PROGRESS.try_with(|p| p.sink.clone()).ok()
}

pub(crate) fn resume() {
    let _ = PROGRESS.try_with(|p| {
        if let Some(stage) = &*p.current.lock().unwrap_or_else(|e| e.into_inner()) {
            (p.sink)(stage);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_wait_is_reported_and_then_the_stage_it_interrupted() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        scope(
            move |s: &Stage| log.lock().unwrap().push(s.to_string()),
            async {
                report(Stage::SearchingMusicBrainz);
                waiting(Stage::WaitingOnMusicBrainz {
                    why: Wait::Refused(503),
                    retry_in: Duration::from_millis(1500),
                });
                resume();
            },
        )
        .await;
        assert_eq!(
            *seen.lock().unwrap(),
            [
                "searching MusicBrainz",
                "waiting on MusicBrainz (HTTP 503), retrying in 2s",
                "searching MusicBrainz",
            ]
        );
        // Outside a scope nothing is reported, and nothing fails.
        report(Stage::Moving);
    }
}
