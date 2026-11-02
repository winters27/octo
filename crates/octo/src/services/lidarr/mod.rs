//! `Services/Lidarr`: the Lidarr client, hearts through Lidarr, and one song through Lidarr for
//! a replacement. The records and the pure matching are `octo_core::lidarr`.

pub mod lidarr_album_claims;
pub mod lidarr_client;
pub mod lidarr_heart_acquisition_service;
pub mod lidarr_import_handoff;
pub mod lidarr_pending_albums;
pub mod lidarr_pick;
pub mod lidarr_track_fetcher;

#[cfg(test)]
pub(crate) mod fake_lidarr;

#[cfg(test)]
mod lidarr_last_hold_tests;

pub use lidarr_album_claims::LidarrAlbumClaims;
pub use lidarr_client::LidarrClient;
pub use lidarr_heart_acquisition_service::{
    ILidarrHeartAcquisitionService, IndexersAwaited, IndexersBack, LidarrHeartAcquisitionService,
    LidarrHeartExtras, LidarrImportEvent, LidarrImportOutcome, LidarrWaits, NO_INDEXER_SEARCHES, PauseRounds,
};
pub use lidarr_import_handoff::{LidarrImport, LidarrImportHandoff};
pub use lidarr_pending_albums::{
    CANNOT_SEE, FinishedGrab, FollowEnd, FollowedGrab, FollowedItem, HELD_FOR_LIDARR, HeldPlacement,
    HeldTrack, LidarrPendingAlbums, MovedRecord, PROVEN_ELSEWHERE, PendingAdd, PendingAlbum, ProvenSong,
    REPLACED_YOURS, SongGate, UnsettledFollow,
};
pub use lidarr_pick::{ALREADY_LOOKING, ArtistMusic, LidarrArtistMusic};
pub use lidarr_track_fetcher::{
    ILidarrTrackFetcher, LidarrFetched, LidarrReplacements, LidarrTrackFetcher, OriginalSound,
};
pub use octo_core::lidarr::{
    LidarrAlbumCandidate, LidarrAlbumImportState, LidarrAlbumState, LidarrChoice, LidarrError,
    LidarrImportedTrack, LidarrOptions, LidarrRelease, LidarrReleasePick, LidarrRootFolder, LidarrStep,
    LidarrTrackRequest,
};
