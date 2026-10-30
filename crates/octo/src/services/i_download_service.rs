//! The music download service's interface and `DirectStreamInfo`. The service itself is
//! `BaseDownloadService` and `SoulseekDownloadService`.
//!
//! Where a download cannot be done the methods answer `Err`, and a caller that branches on the
//! kind downcasts (`ReplacementRejectedException`).

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use octo_core::models::domain::Album;
use octo_core::models::download::DownloadInfo;
use octo_core::settings::DownloadSource;
use tokio_util::sync::CancellationToken;

use crate::services::common::AcquisitionPlace;
use crate::services::library::ReplacementHandoff;
use crate::services::lidarr::HeldPlacement;

/// An audio body being read, chunk by chunk. Dropping it closes the upstream response it reads
/// from.
pub type AudioStream = Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>;

/// Interface for the music download service (Deezspot or other)
#[async_trait]
pub trait IDownloadService: Send + Sync {
    /// Downloads a song from an external provider: the provider (deezer, spotify), the ID on
    /// the external provider and a cancellation token. The path to the downloaded file.
    async fn download_song(
        &self,
        external_provider: &str,
        external_id: &str,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<String>;

    /// Downloads a song and streams the result progressively: a stream of the audio file.
    async fn download_and_stream(
        &self,
        external_provider: &str,
        external_id: &str,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<AudioStream>;

    /// Downloads remaining tracks from an album in background (excluding the specified track,
    /// already downloaded).
    fn download_remaining_album_tracks_in_background(
        &self,
        external_provider: &str,
        album_external_id: &str,
        exclude_track_external_id: &str,
    );

    /// Run one queued acquisition to completion. Called only by the acquisition worker,
    /// which supplies a token unrelated to any HTTP request: that separation is the whole
    /// point, since a client giving up must never cancel a transfer slskd will finish.
    ///
    /// `requested_by`: the users who asked for this track, for the history entry and the
    /// notification. Empty when Octo started the acquisition itself or when attribution is
    /// switched off. `upgrade_search`: search Soulseek the slow, wide way (Better quality,
    /// weekly upgrade). `replacement`: a library action's replacement, revealed in the
    /// original's place.
    #[allow(clippy::too_many_arguments)] // one argument per input an acquisition takes
    async fn execute_acquisition(
        &self,
        external_provider: &str,
        external_id: &str,
        trigger_album_download: bool,
        force_permanent: bool,
        source_override: Option<DownloadSource>,
        cancellation_token: &CancellationToken,
        requested_by: Option<Vec<String>>,
        upgrade_search: bool,
        replacement: Option<Arc<ReplacementHandoff>>,
    ) -> anyhow::Result<String>;

    /// [`IDownloadService::execute_acquisition`] with where the file goes and what it must be
    /// (`place`): the family pool for members, and a real FLAC copy only. The default ignores
    /// `place`, as a service without the family pool does.
    #[allow(clippy::too_many_arguments)] // execute_acquisition's arguments and the place
    async fn execute_acquisition_to(
        &self,
        external_provider: &str,
        external_id: &str,
        trigger_album_download: bool,
        force_permanent: bool,
        source_override: Option<DownloadSource>,
        cancellation_token: &CancellationToken,
        requested_by: Option<Vec<String>>,
        upgrade_search: bool,
        replacement: Option<Arc<ReplacementHandoff>>,
        place: AcquisitionPlace,
    ) -> anyhow::Result<String> {
        let _ = place;
        self.execute_acquisition(
            external_provider,
            external_id,
            trigger_album_download,
            force_permanent,
            source_override,
            cancellation_token,
            requested_by,
            upgrade_search,
            replacement,
        )
        .await
    }

    /// Runs one direct source for every missing track in an album.
    async fn download_album_with_source(
        &self,
        external_provider: &str,
        album_external_id: &str,
        source: DownloadSource,
        suppress_summary: bool,
        cancellation_token: &CancellationToken,
        requested_by: Option<Vec<String>>,
    ) -> anyhow::Result<bool>;

    /// Holds an album's own listing while its tracks are fetched one by one outside a walk (a
    /// member's album, song by song into their library): each listed track is filed at its
    /// place on that album, and its tracks settle one set of album-level tags. Ended with
    /// [`IDownloadService::finish_album_listing`]. The default holds nothing.
    fn hold_album_listing(&self, external_provider: &str, album: &Album) {
        let _ = (external_provider, album);
    }

    /// Ends the hold of [`IDownloadService::hold_album_listing`]: every track fetched under it
    /// gets the album-level tags its tracks settled on.
    async fn finish_album_listing(&self, external_provider: &str, album: &Album) {
        let _ = (external_provider, album);
    }

    /// Fetches a song ahead of its play: a real FLAC copy only, from the lossless sources
    /// (Soulseek and SpotiFLAC as its settings place it), never YouTube or Lidarr. Checked and
    /// tagged like any download, then kept in `folder` as `<provider>_<id>.<ext>` beside its
    /// facts, outside the library: nothing is registered, logged as fetched or scanned. A song
    /// the listener already has answers [`Ahead::AlreadyYours`]. The default fetches nothing.
    async fn download_ahead(
        &self,
        external_provider: &str,
        external_id: &str,
        folder: &Path,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<Ahead> {
        let _ = (external_provider, external_id, folder, cancellation_token);
        anyhow::bail!("This download service does not fetch songs ahead")
    }

    /// Checks if a song is currently being downloaded
    fn get_download_status(&self, song_id: &str) -> Option<DownloadInfo>;

    /// A transfer is running or finishing. The library Review sweep waits for none to be.
    fn has_active_downloads(&self) -> bool {
        false
    }

    /// Places a file a Lidarr follow held back from the library, now that no follow holds it:
    /// moved where the layout files it, with the library's record moved along. Where it landed,
    /// or None when it could not be placed (it then stays where it plays from).
    async fn place_held(&self, _held: &HeldPlacement) -> Option<String> {
        None
    }

    /// Gets the local path for a song if it has been downloaded already: the provider
    /// (deezer, qobuz, etc.) and the ID on the external provider. The local file path if it
    /// exists, `None` otherwise.
    async fn get_local_path_if_exists(&self, external_provider: &str, external_id: &str) -> Option<String>;

    /// Checks if the service is properly configured and functional
    async fn is_available(&self) -> bool;

    /// Gets a direct stream from the provider CDN (true streaming, no disk).
    /// Returns the stream and content type for proxying to client.
    /// `range_header` if present is the verbatim HTTP Range
    /// header from the incoming client request: it gets forwarded upstream so
    /// the response can be a 206 with Content-Range. iOS Subsonic clients
    /// require Range support for non-FLAC audio or they refuse to play.
    async fn get_direct_stream(
        &self,
        external_provider: &str,
        external_id: &str,
        range_header: Option<&str>,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<Option<DirectStreamInfo>>;
}

/// What [`IDownloadService::download_ahead`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ahead {
    /// The song is ready in the folder, here.
    Kept { path: PathBuf },
    /// The library (or the family) already has the song, here; nothing was kept.
    AlreadyYours { path: PathBuf },
}

/// Information about a direct stream from a provider CDN.
pub struct DirectStreamInfo {
    pub audio_stream: AudioStream,
    pub content_type: String,
    pub content_length: Option<u64>,
    pub quality: Option<String>,
    /// 200 for full body, 206 for partial content.
    pub status_code: u16,
    /// Verbatim Content-Range header to forward to the client (only set when StatusCode == 206).
    pub content_range: Option<String>,
    /// The video that played, when the stream is a video's.
    pub video_id: Option<String>,
    /// That video's audio format, when the routing is pinned to that same video and knows it.
    pub itag: Option<u32>,
}
