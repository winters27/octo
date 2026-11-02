//! Hand-written fakes for the acquisition tests: the download service, the Lidarr heart
//! acquisition service and the metadata service, an offline Soulseek link, and `until`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, HeartGate};
use octo_core::models::domain::{Album, Artist, Song};
use octo_core::models::download::DownloadInfo;
use octo_core::models::search::SearchResult;
use octo_core::models::subsonic::ExternalPlaylist;
use octo_core::settings::DownloadSource;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use crate::services::i_download_service::{AudioStream, DirectStreamInfo, IDownloadService};
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::library::ReplacementHandoff;
use crate::services::lidarr::ILidarrHeartAcquisitionService;
use crate::services::soulseek::ISoulseekLink;
use crate::services::soulseek::soulseek_link::{SoulseekLinkState, SoulseekServerReading};

/// Waits up to ten seconds for a condition.
pub(crate) async fn until(condition: impl Fn() -> bool) {
    for _ in 0..1000 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the condition never held");
}

type AlbumAnswer = Box<dyn FnMut(DownloadSource, bool) -> anyhow::Result<bool> + Send>;

/// One album call: provider, album, source, suppress summary, requested by.
pub(crate) type AlbumCall = (String, String, DownloadSource, bool, Option<Vec<String>>);

/// The download service: album walks answer `false` unless told otherwise, and every call is
/// recorded.
pub(crate) struct FakeDownloads {
    album: Mutex<AlbumAnswer>,
    pub album_calls: Mutex<Vec<AlbumCall>>,
    pub remaining_calls: Mutex<usize>,
}

impl Default for FakeDownloads {
    fn default() -> Self {
        FakeDownloads {
            album: Mutex::new(Box::new(|_, _| Ok(false))),
            album_calls: Mutex::new(Vec::new()),
            remaining_calls: Mutex::new(0),
        }
    }
}

impl FakeDownloads {
    /// Answers album walks with `answer(source, suppress_summary)`.
    pub fn albums(answer: impl FnMut(DownloadSource, bool) -> anyhow::Result<bool> + Send + 'static) -> Self {
        let fake = FakeDownloads::default();
        *fake.album.lock() = Box::new(answer);
        fake
    }

    pub fn album_sources(&self) -> Vec<DownloadSource> {
        self.album_calls.lock().iter().map(|c| c.2).collect()
    }
}

#[async_trait]
impl IDownloadService for FakeDownloads {
    async fn download_song(&self, _: &str, _: &str, _: &CancellationToken) -> anyhow::Result<String> {
        anyhow::bail!("not set up")
    }

    async fn download_and_stream(
        &self,
        _: &str,
        _: &str,
        _: &CancellationToken,
    ) -> anyhow::Result<AudioStream> {
        anyhow::bail!("not set up")
    }

    fn download_remaining_album_tracks_in_background(&self, _: &str, _: &str, _: &str) {
        *self.remaining_calls.lock() += 1;
    }

    async fn execute_acquisition(
        &self,
        _: &str,
        _: &str,
        _: bool,
        _: bool,
        _: Option<DownloadSource>,
        _: &CancellationToken,
        _: Option<Vec<String>>,
        _: bool,
        _: Option<Arc<ReplacementHandoff>>,
    ) -> anyhow::Result<String> {
        anyhow::bail!("not set up")
    }

    async fn download_album_with_source(
        &self,
        provider: &str,
        album: &str,
        source: DownloadSource,
        suppress_summary: bool,
        _: &CancellationToken,
        requested_by: Option<Vec<String>>,
    ) -> anyhow::Result<bool> {
        self.album_calls.lock().push((
            provider.to_string(),
            album.to_string(),
            source,
            suppress_summary,
            requested_by,
        ));
        (self.album.lock())(source, suppress_summary)
    }

    fn get_download_status(&self, _: &str) -> Option<DownloadInfo> {
        None
    }

    async fn get_local_path_if_exists(&self, _: &str, _: &str) -> Option<String> {
        None
    }

    async fn is_available(&self) -> bool {
        true
    }

    async fn get_direct_stream(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: &CancellationToken,
    ) -> anyhow::Result<Option<DirectStreamInfo>> {
        Ok(None)
    }
}

type LidarrAnswer = Box<dyn FnMut(&str, &str, bool) -> anyhow::Result<bool> + Send>;

/// One Lidarr call: provider, external id, notify on failure, requested by.
pub(crate) type LidarrCall = (String, String, bool, Option<String>);

/// The Lidarr heart acquisition service: `false` unless told otherwise, every call recorded.
pub(crate) struct FakeLidarr {
    track: Mutex<LidarrAnswer>,
    album: Mutex<LidarrAnswer>,
    pub track_calls: Mutex<Vec<LidarrCall>>,
    pub album_calls: Mutex<Vec<LidarrCall>>,
    /// Why each call ran and how many gates it carried, track and album calls in order.
    pub origins: Mutex<Vec<(AcquisitionOrigin, usize)>>,
    /// Every song's artist already has music in Lidarr, which is left alone.
    pub left_alone: Mutex<bool>,
    /// What the next track call waits on when Lidarr is last, read once.
    pub waits: Mutex<Option<crate::services::lidarr::LidarrWaits>>,
}

impl Default for FakeLidarr {
    fn default() -> Self {
        FakeLidarr {
            track: Mutex::new(Box::new(|_, _, _| Ok(false))),
            album: Mutex::new(Box::new(|_, _, _| Ok(false))),
            track_calls: Mutex::new(Vec::new()),
            album_calls: Mutex::new(Vec::new()),
            origins: Mutex::new(Vec::new()),
            left_alone: Mutex::new(false),
            waits: Mutex::new(None),
        }
    }
}

impl FakeLidarr {
    pub fn tracks(
        self,
        answer: impl FnMut(&str, &str, bool) -> anyhow::Result<bool> + Send + 'static,
    ) -> Self {
        *self.track.lock() = Box::new(answer);
        self
    }

    pub fn albums(
        self,
        answer: impl FnMut(&str, &str, bool) -> anyhow::Result<bool> + Send + 'static,
    ) -> Self {
        *self.album.lock() = Box::new(answer);
        self
    }

    /// The calls with exactly these arguments (any asker).
    pub fn track_calls_for(&self, provider: &str, id: &str, notify: bool) -> usize {
        self.track_calls
            .lock()
            .iter()
            .filter(|c| c.0 == provider && c.1 == id && c.2 == notify)
            .count()
    }

    pub fn album_calls_for(&self, provider: &str, id: &str, notify: bool) -> usize {
        self.album_calls
            .lock()
            .iter()
            .filter(|c| c.0 == provider && c.1 == id && c.2 == notify)
            .count()
    }

    pub fn calls(&self) -> usize {
        self.track_calls.lock().len() + self.album_calls.lock().len()
    }
}

#[async_trait]
impl ILidarrHeartAcquisitionService for FakeLidarr {
    async fn try_acquire_track(
        &self,
        provider: &str,
        external_id: &str,
        notify_failure: bool,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
        gates: &[HeartGate],
    ) -> anyhow::Result<bool> {
        self.origins.lock().push((origin, gates.len()));
        self.track_calls.lock().push((
            provider.into(),
            external_id.into(),
            notify_failure,
            requested_by.map(str::to_string),
        ));
        (self.track.lock())(provider, external_id, notify_failure)
    }

    async fn try_acquire_album(
        &self,
        provider: &str,
        external_id: &str,
        notify_failure: bool,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
        gates: &[HeartGate],
    ) -> anyhow::Result<bool> {
        self.origins.lock().push((origin, gates.len()));
        self.album_calls.lock().push((
            provider.into(),
            external_id.into(),
            notify_failure,
            requested_by.map(str::to_string),
        ));
        (self.album.lock())(provider, external_id, notify_failure)
    }

    fn left_alone(&self, _: &str, _: &str) -> bool {
        *self.left_alone.lock()
    }

    fn waits_on(&self, _: &str, _: &str) -> Option<crate::services::lidarr::LidarrWaits> {
        self.waits.lock().take()
    }
}

type DurationsHook = Arc<dyn Fn(bool) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// The metadata service: songs and albums by id, a hit per (artist, title), and hooks
/// on the durations pass and the prewarm.
#[derive(Default)]
pub(crate) struct FakeMetadata {
    pub songs: Mutex<HashMap<(String, String), Song>>,
    pub albums: Mutex<HashMap<(String, String), Album>>,
    pub hits: Mutex<HashMap<(String, String), Song>>,
    pub durations: Mutex<Option<DurationsHook>>,
    pub prewarmed: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// The length the registry holds for a song now, by title, for a refresh to read.
    pub lengths_now: Mutex<HashMap<String, i32>>,
    /// What every album search answers, and how many were made.
    pub album_hits: Mutex<Vec<Album>>,
    pub album_searches: Mutex<usize>,
    /// What the catalog's own track search answers; None is "could not answer".
    pub catalog_tracks: Mutex<Option<Vec<(String, String)>>>,
}

#[async_trait]
impl IMusicMetadataService for FakeMetadata {
    async fn search_songs(&self, _: &str, _: i32) -> Vec<Song> {
        Vec::new()
    }

    async fn search_songs_by_artist_title(
        &self,
        artist: &str,
        title: &str,
        _: i32,
        _: Option<i32>,
    ) -> Vec<Song> {
        self.hits
            .lock()
            .get(&(artist.to_string(), title.to_string()))
            .cloned()
            .into_iter()
            .collect()
    }

    async fn resolve_top_durations(&self, _songs: &mut [Song], background: bool) {
        let hook = self.durations.lock().clone();
        if let Some(hook) = hook {
            hook(background).await;
        }
    }

    async fn search_catalog_tracks(&self, _: &str, _: i32) -> Option<Vec<(String, String)>> {
        self.catalog_tracks.lock().clone()
    }

    fn refresh_outside_songs(&self, songs: &mut [Song]) {
        let lengths = self.lengths_now.lock();
        for song in songs {
            if let Some(&seconds) = lengths.get(&song.title) {
                song.duration = Some(seconds);
            }
        }
    }

    async fn prewarm_you_tube_ids(&self, _songs: &[Song], top_n: usize) {
        if top_n == 12
            && let Some(hook) = self.prewarmed.lock().clone()
        {
            hook();
        }
    }

    async fn search_albums(&self, _: &str, _: i32) -> Vec<Album> {
        *self.album_searches.lock() += 1;
        self.album_hits.lock().clone()
    }

    async fn search_artists(&self, _: &str, _: i32) -> Vec<Artist> {
        Vec::new()
    }

    async fn search_all(&self, _: &str, _: i32, _: i32, _: i32) -> SearchResult {
        SearchResult::default()
    }

    async fn get_song(&self, provider: &str, id: &str) -> Option<Song> {
        self.songs
            .lock()
            .get(&(provider.to_string(), id.to_string()))
            .cloned()
    }

    async fn get_album(&self, provider: &str, id: &str) -> Option<Album> {
        self.albums
            .lock()
            .get(&(provider.to_string(), id.to_string()))
            .cloned()
    }

    async fn get_artist(&self, _: &str, _: &str) -> Option<Artist> {
        None
    }

    async fn get_artist_albums(&self, _: &str, _: &str) -> Vec<Album> {
        Vec::new()
    }

    async fn search_playlists(&self, _: &str, _: i32) -> Vec<ExternalPlaylist> {
        Vec::new()
    }

    async fn get_playlist(&self, _: &str, _: &str) -> Option<ExternalPlaylist> {
        None
    }

    async fn get_playlist_tracks(&self, _: &str, _: &str) -> Vec<Song> {
        Vec::new()
    }
}

/// slskd is offline, and a Soulseek heart would wait up to six hours.
pub(crate) struct OfflineLink;

#[async_trait]
impl ISoulseekLink for OfflineLink {
    async fn read(&self, _fresh: bool) -> Option<SoulseekServerReading> {
        Some(SoulseekServerReading::new(
            SoulseekLinkState::NotLoggedIn,
            Some("Disconnecting".into()),
            None,
            None,
        ))
    }

    fn hold_limit(&self) -> TimeDelta {
        TimeDelta::hours(6)
    }

    fn utc_now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    async fn wait_for_login(&self, _deadline_utc: DateTime<Utc>) -> bool {
        std::future::pending::<()>().await;
        false
    }
}

/// slskd is logged in.
pub(crate) struct LoggedInLink;

#[async_trait]
impl ISoulseekLink for LoggedInLink {
    async fn read(&self, _fresh: bool) -> Option<SoulseekServerReading> {
        Some(SoulseekServerReading::new(
            SoulseekLinkState::LoggedIn,
            None,
            None,
            None,
        ))
    }

    fn hold_limit(&self) -> TimeDelta {
        TimeDelta::hours(6)
    }

    fn utc_now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    async fn wait_for_login(&self, _deadline_utc: DateTime<Utc>) -> bool {
        true
    }
}

/// Soulseek is switched off: no step uses it.
pub(crate) struct OffLink;

#[async_trait]
impl ISoulseekLink for OffLink {
    async fn read(&self, _fresh: bool) -> Option<SoulseekServerReading> {
        None
    }

    fn hold_limit(&self) -> TimeDelta {
        TimeDelta::hours(6)
    }

    fn utc_now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    async fn wait_for_login(&self, _deadline_utc: DateTime<Utc>) -> bool {
        false
    }

    fn is_off(&self) -> bool {
        true
    }
}
