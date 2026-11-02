//! `Services/Common`: the shared services with state or I/O.

pub mod acquisition_activity;
pub mod acquisition_tracker;
pub mod acquisition_worker;
pub mod album_gates;
pub mod base_download_service;
pub mod cache_cleanup_service;
pub mod download_arrivals;
pub mod download_concurrency;
pub mod download_log_keeper;
pub mod download_picks;
pub mod external_search_service;
pub mod heart_acquisition_coordinator;
pub mod lastfm_country_names;
pub mod replace_anyway;
pub mod song_finder;
pub mod soulseek_hold_resumer;
pub mod soulseek_hold_store;
pub mod star_on_arrival;
pub mod top_songs_service;
pub mod track_acquisition_queue;

pub use acquisition_activity::{AcquisitionActivity, IAcquisitionActivity};
pub use acquisition_tracker::{
    AcquisitionEnd, AcquisitionSnapshot, AcquisitionState, AcquisitionTracker, TrackerServices, WatchSettings,
};
pub use acquisition_worker::AcquisitionWorker;
pub use album_gates::{AlbumGate, AlbumGateTrack, AlbumGates};
pub use base_download_service::{
    BaseDownloadService, CatalogBlanks, DownloadBackend, DownloadCore, DownloadOptions, DownloadServices,
    FileNotFoundException, LayoutChoice, MeterPreview, OperationCanceled, Placement, PlaylistTrackSync,
    RequestedIdentity, ReviewQueue, TrackDownload,
};
pub use cache_cleanup_service::CacheCleanupService;
pub use download_arrivals::{ArrivalListener, DownloadArrivals};
pub use download_concurrency::{DownloadConcurrency, TransferLimiter, TransferSlot};
pub use download_log_keeper::DownloadLogKeeper;
pub use download_picks::{DownloadPicks, PickedCopy};
pub use external_search_service::ExternalSearchService;
pub use heart_acquisition_coordinator::{
    FamilyHooks, GateStart, HeartAcquisitionCoordinator, HeartCoordinatorExtras, HeartGates, TrackOutcome,
};
pub use song_finder::{SongFinder, SongFinderServices};
pub use soulseek_hold_resumer::SoulseekHoldResumer;
pub use soulseek_hold_store::{HeldAcquisition, HeldKind, Holder, SoulseekHoldStore};
pub use star_on_arrival::{StarNavidrome, StarOnArrival, WaitingStar};
pub use top_songs_service::{ChartFetch, TopList, TopSong, TopSongsService};
pub use track_acquisition_queue::{
    AcquisitionHeld, AcquisitionOutcome, AcquisitionPlace, AcquisitionRequest, AcquisitionTarget, Completion,
    GateLedger, GateOutcome, PlayOutcome, SharedGates, SoundDeny, TrackAcquisitionQueue, acquisition_key,
};

#[cfg(test)]
mod download_attribution_tests;
#[cfg(test)]
pub(crate) mod download_log_tests;
#[cfg(test)]
mod find_songs_pick_tests;
#[cfg(test)]
pub(crate) mod test_fakes;
