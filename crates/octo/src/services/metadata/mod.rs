//! Metadata with I/O (`Services/Metadata`): the Deezer catalog client with the named client and
//! rate limiter every Deezer call goes through, Deezer's reach and the Apple stand-in for what it
//! hides, and the genre backfill's run state, undo journal and worker. The genre normalizer, the
//! Accept-Language header and Apple's answers in Deezer's shape are `octo_core::metadata`.

pub mod deezer_metadata_service;
pub mod deezer_rate_limit_handler;
pub mod deezer_rate_limiter;
pub mod deezer_reach;
pub mod deezer_stand_in;
pub mod genre_backfill_journal;
pub mod genre_backfill_state;
pub mod genre_backfill_worker;

pub use deezer_metadata_service::{DeezerExplicitCatalog, DeezerMetadataService, DeezerPublishedTempo};
pub use deezer_rate_limit_handler::{AskFor, DeezerRateLimitHandler};
pub use deezer_rate_limiter::DeezerRateLimiter;
pub use deezer_reach::{DeezerReach, Reach};
pub use deezer_stand_in::{DeezerStandIn, DoorInfos};
pub use genre_backfill_journal::{GenreBackfillJournal, GenreJournalEntry};
pub use genre_backfill_state::{
    GenreBackfillChange, GenreBackfillRun, GenreBackfillScope, GenreBackfillStatus, GenreBackfillStore,
};
pub use genre_backfill_worker::{GenreBackfillRequest, GenreBackfillWorker};
