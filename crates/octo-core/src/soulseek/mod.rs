//! Soulseek: the pure parts of `Services/Soulseek` (the routing model, the shown-length rules,
//! the search profiles, slskd's records and answers, the candidate matching and the album
//! folder picker). The clients, stores and services that do I/O live in
//! `octo::services::soulseek`.

pub mod album_folder_picker;
pub mod search_profile;
pub mod song_length;
pub mod soulseek_client;
pub mod soulseek_download_service;
pub mod soulseek_link;
pub mod soulseek_metadata_service;
pub mod soulseek_share_switch;
pub mod soulseek_sharing;
pub mod stream_facts;

pub use album_folder_picker::{AlbumFolderChoice, AlbumFolderPicker, AlbumTrack};
pub use search_profile::SearchProfile;
pub use song_length::{LengthSource, SongLength};
pub use soulseek_client::{
    BatchEnqueue, SearchStatus, SoulseekFileHit, SoulseekTransferProgress, SoulseekTransferState,
    TransferWatch,
};
pub use soulseek_link::{SoulseekLinkState, SoulseekServerReading};
pub use soulseek_metadata_service::{PlayBinding, RoutingKind, SoulseekRouting};
pub use soulseek_sharing::{
    PortCheckResult, PortState, ShareControl, ShareSwitchState, SharedFolder, SharingReport, SharingWarning,
    UploadActivity,
};
pub use stream_facts::{DeclaredStream, StreamFacts};
