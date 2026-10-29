//! Fingerprinting: comparers, the AcoustID answer records and their reading, the pure half of the
//! MusicBrainz client, and the verification types (`Services/Fingerprint`, its pure parts).

pub mod acoust_id_client;
pub mod check_line;
pub mod clip_check;
pub mod heard_master;
pub mod heart_gate;
pub mod heart_release;
pub mod music_brainz_client;
pub mod recording_identity;
pub mod speed_variant;
pub mod track_match_comparer;
pub mod verification;

pub use acoust_id_client::{
    AcoustIdCredit, AcoustIdLookup, AcoustIdRecording, AcoustIdRelease, AcoustIdResult,
};
pub use track_match_comparer::TrackMatchComparer;
pub use verification::{InconclusiveReason, VerificationResult, VerificationVerdict};
