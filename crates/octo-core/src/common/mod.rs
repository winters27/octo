//! Shared helpers used across the services.

pub mod acquisition_tracker;
pub mod album_fill_in;
pub mod artist_credit;
pub mod clock;
pub mod countries;
pub mod cover_names;
pub mod dotnet;
pub mod dotnet_random;
pub mod error;
pub mod fuzzy;
pub mod layout;
pub mod live_version;
pub mod log_redaction;
pub mod navidrome_artist_key;
pub mod octo_user_agent;
pub mod path_helper;
pub mod playlist_id_helper;
pub mod romanize;
pub mod single_flight;
pub mod skipped_takes;
pub mod song_identity;
pub mod song_versions;
pub mod supersedable_build_coordinator;
pub mod version_gap;
pub mod version_variant;

pub use album_fill_in::{AlbumFillIn, LibraryTrack};
pub use clock::Clock;
pub use error::{Error, ErrorType};
pub use layout::{Layout, LayoutKind, LayoutValues, RenderedName, TagSnapshot};
pub use path_helper::{AlbumHook, PathHelper, SettleHooks, Settled};
pub use single_flight::SingleFlight;
pub use song_identity::{
    ArtistAgreement, Lookup, SongArtists, SongIdentity, SongMatch, SongMatchOptions, SongQuery, SongRef,
    SongTitle, SongVerdict,
};
pub use supersedable_build_coordinator::SupersedableBuildCoordinator;
