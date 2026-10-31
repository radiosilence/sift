#[cfg(feature = "tagger")]
pub mod art;
#[cfg(feature = "tagger")]
pub mod check;
#[cfg(feature = "tagger")]
pub mod config;
#[cfg(feature = "tagger")]
pub mod credits;
#[cfg(feature = "tagger")]
pub mod discogs;
pub mod format;
#[cfg(feature = "tagger")]
pub mod ftintitle;
#[cfg(feature = "tagger")]
pub mod import;
#[cfg(feature = "tagger")]
pub mod library;
#[cfg(feature = "tagger")]
pub mod lyrics;
#[cfg(feature = "tagger")]
pub mod manage;
#[cfg(feature = "tagger")]
pub mod matching;
#[cfg(feature = "tagger")]
pub mod meta;
#[cfg(feature = "tagger")]
pub mod musicbrainz;
#[cfg(feature = "tagger")]
mod par;
#[cfg(feature = "tagger")]
pub mod paths;
#[cfg(feature = "tagger")]
pub mod replaygain;

#[cfg(feature = "tagger")]
pub use config::Config;
#[cfg(feature = "tagger")]
pub use import::{
    Candidate, Companion, Comparison, CreditPlan, Credited, Edits, EnrichPlan, Enriched,
    ImportError, Importer, Outcome, PlannedCredit, PlannedLyrics, Rematch, Retag, RetagFile,
    TrackEdit,
};
