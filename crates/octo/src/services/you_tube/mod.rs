//! `Services/YouTube`: the yt-dlp shim, for search and streams, and YouTube Music through its
//! `/ytm` endpoints.

pub mod pin_recheck;
pub mod stream_size_warmer;
pub mod stream_versions;
pub mod you_tube_music_client;
pub mod you_tube_resolver;

pub use pin_recheck::PinRecheck;
pub use stream_size_warmer::StreamSizeWarmer;
pub use you_tube_music_client::{YouTubeMusicClient, YtmRow};
pub use you_tube_resolver::{ShimBusy, ShimStream, YouTubeHit, YouTubeResolver};
