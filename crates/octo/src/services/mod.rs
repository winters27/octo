//! The services: the parts with I/O, state or wiring: stores, clients and workers. The pure
//! halves are in `octo_core` under the same module paths, and `framework` holds the general
//! building blocks they share.

pub mod admin;
pub mod apple;
pub mod charts;
pub mod common;
pub mod cover_art;
pub mod family;
pub mod fingerprint;
pub mod framework;
pub mod health;
pub mod http_client_factory;
pub mod i_download_service;
pub mod i_music_metadata_service;
pub mod imports;
pub mod last_fm;
pub mod library;
pub mod lidarr;
pub mod listen_brainz;
pub mod listening;
pub mod local;
pub mod lyrics;
pub mod metadata;
pub mod notifications;
pub mod prefetch;
pub mod radio;
pub mod sonic;
pub mod soulseek;
pub mod spotiflac;
pub mod state_file;
pub mod subsonic;
pub mod updates;
pub mod validation;
pub mod you_tube;

#[cfg(test)]
pub(crate) mod test_audio;
#[cfg(test)]
pub(crate) mod test_support;
