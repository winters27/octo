//! Fingerprint lookups (`Services/Fingerprint`): the AcoustID client with its named client and
//! rate limiter, the MusicBrainz client, and the verification service a download is checked by.

pub mod acoust_id_client;
pub mod acoust_id_rate_limit_handler;
pub mod acoust_id_rate_limiter;
pub mod catalog_clip;
pub mod download_verification_service;
pub mod heard_master_check;
pub mod music_brainz_client;
pub mod preview_check;

pub use acoust_id_client::AcoustIdClient;
pub use acoust_id_rate_limit_handler::AcoustIdRateLimitHandler;
pub use acoust_id_rate_limiter::AcoustIdRateLimiter;
pub use download_verification_service::{DownloadVerificationService, RecordingIsrcs};
pub use music_brainz_client::{IsrcLookup, MusicBrainzClient, MusicBrainzError};

#[cfg(test)]
#[path = "heart_gate_cases_tests.rs"]
mod heart_gate_cases_tests;
