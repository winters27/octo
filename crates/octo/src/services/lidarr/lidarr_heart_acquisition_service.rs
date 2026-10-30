//! Hearts fetched through Lidarr. Which song an imported track is (`match_song`) is
//! `octo_core::lidarr::lidarr_heart_acquisition_service`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use octo_core::common::SongIdentity;
use octo_core::common::dotnet;
use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, HeartGate};
use octo_core::lidarr::lidarr_heart_acquisition_service::match_song;
use octo_core::lidarr::lidarr_track_fetcher::is_lossless;
use octo_core::lidarr::{LidarrAlbumCandidate, LidarrError, LidarrImportedTrack};
use octo_core::models::domain::{Album, Song};
use octo_core::notifications::{NotificationEvent, NotificationEventType};
use octo_core::settings::{DownloadSource, LidarrCompletionMode, LidarrSettings, SettingsStore};
use octo_core::soulseek::{RoutingKind, SoulseekRouting};
use parking_lot::Mutex;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::{LidarrAlbumClaims, LidarrClient, LidarrImportHandoff, LidarrPendingAlbums, PendingAlbum};
use crate::services::common::AcquisitionTracker;
use crate::services::fingerprint::MusicBrainzClient;
use crate::services::i_download_service::IDownloadService;
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::library::navidrome_song_path_resolver::get_full_path;
use crate::services::library::{LibraryActionJournal, LibraryOwnership, OwnedCopy, UpgradeAsk, UpgradeQueue};
use crate::services::metadata::DeezerMetadataService;
use crate::services::notifications::NotificationService;
use crate::services::soulseek::{ExternalIdRegistry, SoulseekMetadataService};
use crate::services::subsonic::NavidromeIdentityService;

/// Hearts through Lidarr.
#[async_trait]
pub trait ILidarrHeartAcquisitionService: Send + Sync {
    /// True once the song is in the library: Lidarr brought it and it passed Octo's checks, or it
    /// was there already. False when Lidarr could not get it, so the heart's next source tries.
    /// Waits for Lidarr's import, up to Lidarr's import timeout. `Err` when the attempt itself
    /// fails. `origin` says why it runs and `gates` are the gates of the hearts or plays that
    /// asked, each judged on its own.
    async fn try_acquire_track(
        &self,
        provider: &str,
        external_id: &str,
        notify_failure: bool,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
        gates: &[HeartGate],
    ) -> anyhow::Result<bool>;

    /// True once every song of the album is in the library; false leaves the rest to
    /// the next source, which skips the songs that did land.
    async fn try_acquire_album(
        &self,
        provider: &str,
        external_id: &str,
        notify_failure: bool,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
        gates: &[HeartGate],
    ) -> anyhow::Result<bool>;

    /// Lidarr says it imported files of an album. A heart waiting on that album looks at once;
    /// an album Octo asked for whose heart stopped waiting (or that Octo restarted since) has
    /// its new files taken through the download pipeline; any other album is left alone.
    async fn lidarr_imported(&self, _event: LidarrImportEvent) -> LidarrImportOutcome {
        LidarrImportOutcome::NotOurs
    }
}

/// What Lidarr's webhook says it imported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LidarrImportEvent {
    /// The album's MusicBrainz release group id.
    pub foreign_album_id: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// The imported files, as Lidarr sees them.
    pub paths: Vec<String>,
    /// The files replaced files Lidarr already had.
    pub is_upgrade: bool,
}

/// What became of a webhook's import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LidarrImportOutcome {
    /// A heart was waiting on the album and looks now.
    Woke,
    /// An album Octo asked for earlier: its new files go through the pipeline.
    Late,
    /// Not an album Octo is waiting for.
    NotOurs,
}

/// The optional services the heart service works with. The download pipeline, the journal, the
/// ownership check and the upgrade queue sit on top of the acquisition queue that the heart
/// coordinator, and so this service, feeds; each is None where the service is not there. The
/// library-action settings are read from the settings store.
#[derive(Default, Clone)]
pub struct LidarrHeartExtras {
    /// The live progress list. Lidarr reports no bytes, so it only hears accepted from here;
    /// the pipeline moves each song's row on from there.
    pub tracker: Option<Arc<AcquisitionTracker>>,
    pub music_brainz: Option<Arc<MusicBrainzClient>>,
    pub claims: Option<Arc<LidarrAlbumClaims>>,
    /// A fresh one when None.
    pub imports: Option<Arc<LidarrImportHandoff>>,
    pub ids: Option<Arc<ExternalIdRegistry>>,
    pub downloads: Option<Arc<dyn IDownloadService>>,
    pub journal: Option<Arc<LibraryActionJournal>>,
    pub ownership: Option<Arc<LibraryOwnership>>,
    pub upgrades: Option<Arc<UpgradeQueue>>,
    /// The albums asked for whose files have not all come in, for imports that come late.
    pub pending: Option<Arc<LidarrPendingAlbums>>,
}

/// Hearts through Lidarr. Lidarr only fetches whole albums, so a heart becomes an album search;
/// what Lidarr imports is then handed, song by song, to the download pipeline a Soulseek file
/// goes through (AcoustID, the spectrum, release matching, tags, placing, history), with Lidarr
/// as the place the file came from (LidarrImportHandoff). A song already in the library or
/// removed with a library action is deleted from what Lidarr brought instead.
///
/// One job per album: a second heart on an album Lidarr is already fetching joins it, and its
/// songs are followed too. The job ends when every track is dealt with, or at the timeout, and
/// tells each heart whether its songs are in the library.
#[derive(Clone)]
pub struct LidarrHeartAcquisitionService {
    inner: Arc<Inner>,
}

struct Inner {
    client: Arc<LidarrClient>,
    metadata: Arc<dyn IMusicMetadataService>,
    deezer: Arc<DeezerMetadataService>,
    /// The Lidarr, Subsonic and library action settings and `Library:DownloadPath`, all read at
    /// use.
    settings: Arc<SettingsStore>,
    nav_identity: NavidromeIdentityService,
    notifications: Arc<NotificationService>,
    tracker: Option<Arc<AcquisitionTracker>>,
    music_brainz: Option<Arc<MusicBrainzClient>>,
    claims: Option<Arc<LidarrAlbumClaims>>,
    imports: Arc<LidarrImportHandoff>,
    ids: Option<Arc<ExternalIdRegistry>>,
    downloads: Option<Arc<dyn IDownloadService>>,
    journal: Option<Arc<LibraryActionJournal>>,
    ownership: Option<Arc<LibraryOwnership>>,
    upgrades: Option<Arc<UpgradeQueue>>,
    pending: Option<Arc<LidarrPendingAlbums>>,

    /// By foreign album id, ignoring case.
    jobs: Mutex<HashMap<String, Arc<AlbumJob>>>,
    timing: Mutex<Timing>,
}

/// The service's poll intervals.
#[derive(Clone, Copy)]
struct Timing {
    claim_poll: Duration,
    poll_override: Option<Duration>,
}

/// An error a joined heart sees as the creator's job saw it (a faulted task, awaited twice).
type SharedResult = Result<(), Arc<anyhow::Error>>;

/// The keys of the songs in the library when the job ended: a hearted song's own id, and every
/// song's artist and title, so a heart that joined late still finds its song among the album's
/// other tracks. Shared with the job's last handoffs.
type Landed = Arc<Mutex<HashSet<String>>>;

/// One album Lidarr is fetching for one or more hearts.
struct AlbumJob {
    candidate: LidarrAlbumCandidate,
    album: Album,
    songs: Mutex<JobSongs>,
    /// Who asked, first spelling kept, compared ignoring case.
    requesters: Mutex<Vec<String>>,
    submitted: OnceLock<Shared<BoxFuture<'static, SharedResult>>>,
    ended: watch::Sender<Option<Landed>>,
    /// Why a hearted song did not land, by its key, for the progress list.
    missed: Mutex<HashMap<String, String>>,
    /// Lidarr said it imported something: look now rather than at the next poll.
    wake: Notify,
    /// Taking an import Lidarr finished after the heart stopped waiting. Nothing is deleted
    /// through Lidarr then: by that time a file may be the owner's.
    late: bool,
}

#[derive(Default)]
struct JobSongs {
    songs: Vec<Song>,
    match_by_number: bool,
    album_heart: bool,
}

impl AlbumJob {
    fn new(candidate: LidarrAlbumCandidate, album: Album) -> Self {
        AlbumJob {
            candidate,
            album,
            songs: Mutex::new(JobSongs::default()),
            requesters: Mutex::new(Vec::new()),
            submitted: OnceLock::new(),
            ended: watch::channel(None).0,
            missed: Mutex::new(HashMap::new()),
            wake: Notify::new(),
            late: false,
        }
    }

    /// A job for an import that came after its heart stopped waiting.
    fn late(candidate: LidarrAlbumCandidate, album: Album) -> Self {
        AlbumJob {
            late: true,
            ..AlbumJob::new(candidate, album)
        }
    }

    fn join(&self, songs: &[Song], match_by_number: bool, album_heart: bool, requested_by: Option<&str>) {
        {
            let mut known = self.songs.lock();
            for song in songs {
                if !known.songs.iter().any(|k| key_of(k) == key_of(song)) {
                    known.songs.push(song.clone());
                }
            }
            known.match_by_number |= match_by_number;
            known.album_heart |= album_heart;
        }
        if let Some(name) = requested_by.filter(|r| !dotnet::is_blank(r)) {
            let name = name.trim();
            let mut requesters = self.requesters.lock();
            if !requesters.iter().any(|r| dotnet::eq_ignore_case(r, name)) {
                requesters.push(name.to_string());
            }
        }
    }

    fn match_by_number(&self) -> bool {
        self.songs.lock().match_by_number
    }

    fn album_heart(&self) -> bool {
        self.songs.lock().album_heart
    }

    fn view(&self) -> Album {
        Album {
            title: self.album.title.clone(),
            artist: self.album.artist.clone(),
            year: self.album.year,
            songs: self.songs.lock().songs.clone(),
            ..Default::default()
        }
    }

    fn asked_by(&self) -> Option<Vec<String>> {
        let mut names = self.requesters.lock().clone();
        if names.is_empty() {
            return None;
        }
        names.sort_by(|a, b| dotnet::compare_ordinal_ignore_case(a, b));
        Some(names)
    }

    fn missed_for(&self, key: &str) -> Option<String> {
        self.missed.lock().get(key).cloned()
    }

    async fn wait_ended(&self) -> HashSet<String> {
        let mut rx = self.ended.subscribe();
        let landed = match rx.wait_for(Option::is_some).await {
            Ok(value) => value.clone(),
            Err(_) => None,
        };
        landed.map(|l| l.lock().clone()).unwrap_or_default()
    }
}

fn key_of(song: &Song) -> String {
    match song.external_id.as_deref() {
        Some(id) if !dotnet::is_blank(id) => {
            format!("{}:{id}", song.external_provider.as_deref().unwrap_or(""))
        }
        _ => title_key(Some(&song.artist), Some(&song.title)),
    }
}

fn title_key(artist: Option<&str>, title: Option<&str>) -> String {
    format!(
        "t:{}",
        SongIdentity::match_key(artist.unwrap_or(""), title.unwrap_or(""))
    )
}

fn tracked_keys(album: &Album) -> Vec<(String, String)> {
    album
        .songs
        .iter()
        .filter_map(
            |s| match (s.external_provider.as_deref(), s.external_id.as_deref()) {
                (Some(p), Some(id)) if !dotnet::is_blank(p) && !dotnet::is_blank(id) => {
                    Some((p.to_string(), id.to_string()))
                }
                _ => None,
            },
        )
        .collect()
}

fn add_keys(landed: &Landed, song: Option<&Song>, track: &LidarrImportedTrack, album: &Album) {
    let mut landed = landed.lock();
    if let Some(song) = song {
        landed.insert(key_of(song));
    }
    landed.insert(title_key(
        Some(track.artist.as_deref().unwrap_or(&album.artist)),
        Some(&track.title),
    ));
    if let Some(song) = song {
        landed.insert(title_key(Some(&song.artist), Some(&song.title)));
    }
}

/// Why an imported file should not join the library.
struct DropReason {
    reason: String,
    owned: Option<OwnedCopy>,
}

impl LidarrHeartAcquisitionService {
    pub fn new(
        client: Arc<LidarrClient>,
        metadata: Arc<dyn IMusicMetadataService>,
        deezer: Arc<DeezerMetadataService>,
        settings: Arc<SettingsStore>,
        nav_identity: NavidromeIdentityService,
        notifications: Arc<NotificationService>,
        extras: LidarrHeartExtras,
    ) -> Self {
        LidarrHeartAcquisitionService {
            inner: Arc::new(Inner {
                client,
                metadata,
                deezer,
                settings,
                nav_identity,
                notifications,
                tracker: extras.tracker,
                music_brainz: extras.music_brainz,
                claims: extras.claims,
                imports: extras.imports.unwrap_or_default(),
                ids: extras.ids,
                downloads: extras.downloads,
                journal: extras.journal,
                ownership: extras.ownership,
                upgrades: extras.upgrades,
                pending: extras.pending,
                jobs: Mutex::new(HashMap::new()),
                timing: Mutex::new(Timing {
                    claim_poll: Duration::from_secs(10),
                    poll_override: None,
                }),
            }),
        }
    }

    /// How often a heart looks whether an upgrade let go of the album.
    pub fn set_claim_poll(&self, poll: Duration) {
        self.inner.timing.lock().claim_poll = poll;
    }

    /// How often Lidarr is asked what it imported, instead of the timeout's thirtieth.
    pub fn set_poll_override(&self, poll: Option<Duration>) {
        self.inner.timing.lock().poll_override = poll;
    }

    /// The people a running job answers to, for tests that join one.
    #[cfg(test)]
    pub(crate) fn askers_of(&self, foreign_album_id: &str) -> Option<Vec<String>> {
        let job = self
            .inner
            .jobs
            .lock()
            .get(&dotnet::ordinal_ignore_case_key(foreign_album_id))
            .cloned()?;
        job.asked_by()
    }
}

#[async_trait]
impl ILidarrHeartAcquisitionService for LidarrHeartAcquisitionService {
    async fn lidarr_imported(&self, event: LidarrImportEvent) -> LidarrImportOutcome {
        self.inner.clone().lidarr_imported(event)
    }

    async fn try_acquire_track(
        &self,
        provider: &str,
        external_id: &str,
        notify_failure: bool,
        requested_by: Option<&str>,
        _origin: AcquisitionOrigin,
        _gates: &[HeartGate],
    ) -> anyhow::Result<bool> {
        let work = self
            .inner
            .acquire_track(provider, external_id, notify_failure, requested_by);
        Ok(self
            .inner
            .try_acquire(work, "track", provider, external_id, notify_failure)
            .await)
    }

    async fn try_acquire_album(
        &self,
        provider: &str,
        external_id: &str,
        notify_failure: bool,
        requested_by: Option<&str>,
        _origin: AcquisitionOrigin,
        _gates: &[HeartGate],
    ) -> anyhow::Result<bool> {
        let work = self
            .inner
            .acquire_album(provider, external_id, notify_failure, requested_by);
        Ok(self
            .inner
            .try_acquire(work, "album", provider, external_id, notify_failure)
            .await)
    }
}

impl Inner {
    async fn acquire_track(
        self: &Arc<Self>,
        provider: &str,
        external_id: &str,
        notify_failure: bool,
        requested_by: Option<&str>,
    ) -> anyhow::Result<bool> {
        let mut song = self
            .metadata
            .get_song(provider, external_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("The starred external track is no longer available."))?;

        // Deezer names the release a hit came out on first, usually the single, which Lidarr
        // then cannot match. MusicBrainz knows which studio album the song belongs to.
        let studio_album_id = match &self.music_brainz {
            Some(music_brainz) => music_brainz.find_studio_album(&song.artist, &song.title).await?,
            None => None,
        };
        let studio = match studio_album_id {
            Some(id) => self.client.resolve_album_by_foreign_id(&id).await?,
            None => None,
        };
        let album = if let Some(studio) = &studio {
            song.album = studio.title.clone();
            Album {
                title: studio.title.clone(),
                artist: studio.artist.clone(),
                year: studio.year,
                songs: vec![song.clone()],
                ..Default::default()
            }
        } else {
            let enriched = self
                .deezer
                .enrich_track(&song.artist, &song.title, true, false)
                .await;
            let mut album_title = enriched.as_ref().and_then(|e| e.album_title.clone());
            if dotnet::is_null_or_white_space(album_title.as_deref()) {
                album_title = Some(song.album.clone());
            }
            let album_title = album_title.unwrap_or_default();
            if dotnet::is_blank(&album_title) {
                anyhow::bail!(
                    "Could not resolve an album for '{} - {}'.",
                    song.artist,
                    song.title
                );
            }
            song.album = album_title.clone();
            if song.cover_art_url.is_none() {
                song.cover_art_url = enriched.as_ref().and_then(|e| e.album_cover_url.clone());
            }
            if song.year.is_none() {
                song.year = enriched.as_ref().and_then(|e| e.year);
            }
            Album {
                title: album_title,
                artist: enriched
                    .as_ref()
                    .and_then(|e| e.artist_name.clone())
                    .unwrap_or_else(|| song.artist.clone()),
                year: enriched.as_ref().and_then(|e| e.year),
                cover_art_url: enriched.as_ref().and_then(|e| e.album_cover_url.clone()),
                songs: vec![song.clone()],
                ..Default::default()
            }
        };
        let job = self
            .queue_resolved_album(&album, requested_by, studio, false, false)
            .await?;
        let landed = job.wait_ended().await;
        if landed.contains(&key_of(&song))
            || landed.contains(&title_key(Some(&song.artist), Some(&song.title)))
        {
            return Ok(true);
        }
        if notify_failure && let Some(tracker) = &self.tracker {
            let reason = job
                .missed_for(&key_of(&song))
                .unwrap_or_else(|| "Lidarr did not bring this song.".to_string());
            tracker.fail(provider, external_id, Some(&reason));
        }
        Ok(false)
    }

    async fn acquire_album(
        self: &Arc<Self>,
        provider: &str,
        external_id: &str,
        notify_failure: bool,
        requested_by: Option<&str>,
    ) -> anyhow::Result<bool> {
        let album = self
            .metadata
            .get_album(provider, external_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("The starred external album is no longer available."))?;
        // No walk runs on this path, so the track list is announced here instead.
        if let Some(tracker) = &self.tracker {
            let listed: Vec<_> = album
                .songs
                .iter()
                .filter_map(|s| {
                    s.external_id.as_deref().filter(|id| !id.is_empty()).map(|id| {
                        (
                            id.to_string(),
                            Some(s.artist.clone()),
                            Some(s.title.clone()),
                            Some(album.title.clone()),
                        )
                    })
                })
                .collect();
            tracker.announce(provider, Some(external_id), None, &listed);
        }
        let job = self
            .queue_resolved_album(&album, requested_by, None, true, true)
            .await?;
        let landed = job.wait_ended().await;
        let missing: Vec<&Song> = album
            .songs
            .iter()
            .filter(|song| {
                !landed.contains(&key_of(song))
                    && !landed.contains(&title_key(Some(&song.artist), Some(&song.title)))
            })
            .collect();
        if notify_failure && let Some(tracker) = &self.tracker {
            for song in &missing {
                let (Some(p), Some(id)) = (song.external_provider.as_deref(), song.external_id.as_deref())
                else {
                    continue;
                };
                if p.is_empty() || id.is_empty() {
                    continue;
                }
                let reason = job
                    .missed_for(&key_of(song))
                    .unwrap_or_else(|| "Lidarr finished the album without this song.".to_string());
                tracker.fail(p, id, Some(&reason));
            }
        }
        Ok(missing.is_empty())
    }

    /// `match_by_number`: whether an imported file may be matched to a song by its track
    /// number. Not for a track heart: its one song carries the number from whatever release it
    /// was found on, often the single, where it is track 1, which on the album is another song.
    async fn queue_resolved_album(
        self: &Arc<Self>,
        album: &Album,
        requested_by: Option<&str>,
        resolved: Option<LidarrAlbumCandidate>,
        match_by_number: bool,
        album_heart: bool,
    ) -> anyhow::Result<Arc<AlbumJob>> {
        if dotnet::is_blank(&album.artist) || dotnet::is_blank(&album.title) {
            anyhow::bail!("Lidarr requires an album artist and title.");
        }

        let candidate = match resolved {
            Some(candidate) => candidate,
            None => {
                self.client
                    .resolve_album(&album.artist, &album.title, album.year)
                    .await?
            }
        };
        let key = dotnet::ordinal_ignore_case_key(&candidate.foreign_album_id);
        let (job, created) = {
            let mut jobs = self.jobs.lock();
            let (job, created) = match jobs.get(&key) {
                Some(job) => (job.clone(), false),
                None => {
                    let job = Arc::new(AlbumJob::new(candidate.clone(), album.clone()));
                    jobs.insert(key.clone(), job.clone());
                    (job, true)
                }
            };
            job.join(&album.songs, match_by_number, album_heart, requested_by);
            if created {
                // Spawned: the submission runs whether or not anyone is still waiting.
                let handle = tokio::spawn(self.clone().submit(job.clone()));
                let submitted: BoxFuture<'static, SharedResult> = async move {
                    match handle.await {
                        Ok(result) => result.map_err(Arc::new),
                        Err(e) => Err(Arc::new(anyhow::anyhow!("{e}"))),
                    }
                }
                .boxed();
                let _ = job.submitted.set(submitted.shared());
            }
            (job, created)
        };
        if !created && let Some(tracker) = &self.tracker {
            // Joined a search already running: its songs are followed from here on.
            for (provider, id) in tracked_keys(album) {
                tracker.transfer(&provider, &id, None, None, None, Some("Lidarr"));
            }
        }
        let submitted = job
            .submitted
            .get()
            .cloned()
            .expect("set when the job was created");
        if let Err(e) = submitted.await {
            let mut jobs = self.jobs.lock();
            if jobs.get(&key).is_some_and(|current| Arc::ptr_eq(current, &job)) {
                jobs.remove(&key);
            }
            return Err(anyhow::anyhow!("{e}"));
        }
        Ok(job)
    }

    async fn submit(self: Arc<Self>, job: Arc<AlbumJob>) -> anyhow::Result<()> {
        let snapshot = self.settings.current().lidarr.clone();
        let key = job.candidate.foreign_album_id.clone();
        // An upgrade borrowing this album deletes what its search brings in when it ends; the heart
        // waits for it rather than lose its songs to that clean up.
        let wait_until = Instant::now() + Duration::from_secs(snapshot.import_timeout_seconds.max(60) as u64);
        while self.claims.as_ref().is_some_and(|c| c.upgrade_busy(&key)) && Instant::now() < wait_until {
            let poll = self.timing.lock().claim_poll;
            tokio::time::sleep(poll).await;
        }
        if let Some(claims) = &self.claims {
            claims.heart_started(&key);
        }
        let started_and_before = async {
            // The files Lidarr had before this search are the owner's; only new ones are ever deleted.
            let existing = self.client.find_album(&key).await?;
            let had_files: Vec<LidarrImportedTrack> = match existing {
                None => Vec::new(),
                Some(existing) => self
                    .client
                    .get_album_tracks(existing.id)
                    .await?
                    .into_iter()
                    .filter(|t| t.track_file_id > 0)
                    .collect(),
            };
            let before: HashSet<i32> = had_files.iter().map(|t| t.track_file_id).collect();
            let mut tracks_before: Vec<i32> = had_files.iter().map(|t| t.id).collect();
            tracks_before.sort_unstable();
            tracks_before.dedup();
            let started = self.client.start_album_search(&job.candidate, None).await?;
            Ok::<_, LidarrError>((started, before, tracks_before))
        }
        .await;
        let (started, before, tracks_before) = match started_and_before {
            Ok(value) => value,
            Err(e) => {
                if let Some(claims) = &self.claims {
                    claims.heart_ended(&key);
                }
                return Err(e.into());
            }
        };

        let album = &job.album;
        info!(
            "Lidarr accepted AlbumSearch for '{} - {}' ({key}, local id {})",
            album.artist, album.title, started.album_id
        );
        self.notifications.notify(NotificationEvent {
            artist: Some(album.artist.clone()),
            title: Some(album.title.clone()),
            album: Some(album.title.clone()),
            source: Some("Lidarr".into()),
            cover_art_url: album.cover_art_url.clone(),
            detail: Some("Album search accepted".into()),
            ..NotificationEvent::new(NotificationEventType::DownloadStarted)
        });
        // Accepted. Lidarr says nothing about bytes, so this is a download with no figure on it.
        if let Some(tracker) = &self.tracker {
            for (provider, id) in tracked_keys(&job.view()) {
                tracker.transfer(&provider, &id, None, None, None, Some("Lidarr"));
            }
        }

        if let Some(pending) = &self.pending {
            let mut files_before: Vec<i32> = before.iter().copied().collect();
            files_before.sort_unstable();
            pending.remember(PendingAlbum {
                foreign_album_id: key.clone(),
                lidarr_album_id: started.album_id,
                artist: album.artist.clone(),
                title: album.title.clone(),
                year: album.year,
                files_before,
                tracks_before,
                requested_by: job.asked_by().unwrap_or_default(),
                asked_at: Utc::now(),
            });
        }

        let this = self.clone();
        let before = Arc::new(before);
        tokio::spawn(async move {
            let landed: Landed = Arc::default();
            let album = &job.album;
            let reconciled = this
                .reconcile_imports(&job, started.album_id, &snapshot, &before, &landed)
                .await;
            // Every track dealt with: nothing can come late. A timeout or an error keeps the
            // album listed for Lidarr's webhook.
            if let (Ok(true), Some(pending)) = (&reconciled, &this.pending) {
                pending.forget(&key);
            }
            if let Err(e) = reconciled {
                error!(
                    "Lidarr import reconciliation failed for '{} - {}': {e:#}",
                    album.artist, album.title
                );
                if snapshot.completion_mode == LidarrCompletionMode::Imported {
                    this.notifications.notify(NotificationEvent {
                        artist: Some(album.artist.clone()),
                        title: Some(album.title.clone()),
                        album: Some(album.title.clone()),
                        source: Some("Lidarr".into()),
                        cover_art_url: album.cover_art_url.clone(),
                        detail: Some(format!("{e}")),
                        ..NotificationEvent::new(NotificationEventType::DownloadFailed)
                    });
                }
            }
            // finally
            {
                let job_key = dotnet::ordinal_ignore_case_key(&key);
                let mut jobs = this.jobs.lock();
                if jobs
                    .get(&job_key)
                    .is_some_and(|current| Arc::ptr_eq(current, &job))
                {
                    jobs.remove(&job_key);
                }
            }
            job.ended.send_if_modified(|ended| {
                if ended.is_some() {
                    return false;
                }
                *ended = Some(landed.clone());
                true
            });
            // Octo moves every import into its own layout, so Lidarr's next rescan finds the
            // files gone. An album Octo switched monitoring on for would then be fetched again
            // and again, so it goes back to how it was.
            if !started.was_monitored
                && let Err(e) = this.client.set_albums_monitored(&[started.album_id], false).await
            {
                warn!(
                    "Could not stop monitoring Lidarr album {} again: {e}",
                    started.album_id
                );
            }
            if let Some(claims) = &this.claims {
                claims.heart_ended(&key);
            }
        });
        Ok(())
    }

    async fn reconcile_imports(
        self: &Arc<Self>,
        job: &Arc<AlbumJob>,
        album_id: i32,
        settings: &LidarrSettings,
        before: &Arc<HashSet<i32>>,
        landed: &Landed,
    ) -> Result<bool, LidarrError> {
        let timeout = Duration::from_secs(settings.import_timeout_seconds.max(1) as u64);
        let poll = self.timing.lock().poll_override.unwrap_or_else(|| {
            Duration::from_secs((settings.import_timeout_seconds / 30).clamp(1, 10) as u64)
        });
        let deadline = Instant::now() + timeout;
        let octo_root = self.nav_identity.effective_download_path(
            &self
                .settings
                .raw("Library:DownloadPath")
                .unwrap_or_else(|| "/music".to_string()),
        );
        // Lidarr tracks dealt with: handed to the pipeline, or dropped. A dropped file no longer
        // counts as Lidarr's, so the album is done once every track is in one of the two.
        let mut handled: HashSet<i32> = HashSet::new();
        let mut handed = Vec::new();
        let mut expected;
        let timed_out;

        loop {
            let state = self.client.get_album_import_state(album_id).await?;
            expected = state.track_count;
            let mut seen_paths: HashSet<String> = HashSet::new();
            let visible: Vec<&LidarrImportedTrack> = state
                .tracks
                .iter()
                .filter(|t| {
                    t.has_file
                        && !dotnet::is_null_or_white_space(t.path.as_deref())
                        && !handled.contains(&t.id)
                })
                .filter(|t| {
                    seen_paths.insert(dotnet::ordinal_ignore_case_key(t.path.as_deref().unwrap_or("")))
                })
                .collect();
            for track in visible {
                let imported_path = translate_imported_path(
                    track.path.as_deref().unwrap_or(""),
                    settings.root_folder_path.as_deref(),
                    &octo_root,
                )?;
                if !Path::new(&imported_path).is_file() {
                    continue;
                }
                handled.insert(track.id);
                if let Some(handoff) = self.take_import(job, track, imported_path, before, landed).await {
                    handed.push(handoff);
                }
            }

            if expected > 0 && handled.len() as i64 >= i64::from(expected) {
                timed_out = false;
                break;
            }
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            tokio::select! {
                () = tokio::time::sleep(poll) => {}
                () = job.wake.notified() => {}
            }
        }

        // The pipeline runs each song's checks and tagging; the job ends when the last is done.
        for handoff in handed {
            let _ = handoff.await;
        }

        let album = job.view();
        if timed_out {
            let mut detail = format!(
                "Lidarr import timed out after {} minute(s)",
                timeout.as_secs() / 60
            );
            if expected > 0 {
                detail.push_str(&format!(" ({}/{expected} tracks arrived)", handled.len()));
            }
            {
                let mut missed = job.missed.lock();
                for song in &album.songs {
                    missed.entry(key_of(song)).or_insert_with(|| format!("{detail}."));
                }
            }
            warn!("{detail} for '{} - {}'", album.artist, album.title);
            if settings.completion_mode == LidarrCompletionMode::Imported {
                self.notifications.notify(NotificationEvent {
                    artist: Some(album.artist.clone()),
                    title: Some(album.title.clone()),
                    album: Some(album.title.clone()),
                    source: Some("Lidarr".into()),
                    cover_art_url: job.album.cover_art_url.clone(),
                    detail: Some(detail),
                    ..NotificationEvent::new(NotificationEventType::DownloadFailed)
                });
            }
            return Ok(false);
        }
        let in_library = landed.lock().iter().filter(|key| key.starts_with("t:")).count() as i32;
        if job.album_heart() || settings.completion_mode == LidarrCompletionMode::Imported {
            self.notifications.notify(NotificationEvent {
                artist: Some(album.artist.clone()),
                title: Some(album.title.clone()),
                cover_art_url: job.album.cover_art_url.clone(),
                track_count: Some(in_library),
                failed_count: Some((expected - in_library).max(0)),
                source: Some("Lidarr".into()),
                ..NotificationEvent::new(NotificationEventType::AlbumCompleted)
            });
        }
        Ok(true)
    }

    /// One file Lidarr imported for a job: dropped when the library must not take it, else
    /// handed to the pipeline, whose work is returned.
    async fn take_import(
        self: &Arc<Self>,
        job: &Arc<AlbumJob>,
        track: &LidarrImportedTrack,
        imported_path: String,
        before: &Arc<HashSet<i32>>,
        landed: &Landed,
    ) -> Option<JoinHandle<()>> {
        let album = job.view();
        let song = match_song(&album, track, job.match_by_number()).cloned();
        if let Some(drop) = self
            .should_drop(&album, track, song.as_ref(), &imported_path, job)
            .await
        {
            self.drop_import(job, track, &imported_path, before, &drop.reason)
                .await;
            // Already in the library: in, as far as the heart is concerned, and its row
            // closes on the library's own copy.
            if let Some(owned) = drop.owned {
                add_keys(landed, song.as_ref(), track, &album);
                if let (Some(song), Some(tracker)) = (&song, &self.tracker)
                    && let (Some(provider), Some(id)) =
                        (song.external_provider.as_deref(), song.external_id.as_deref())
                    && !provider.is_empty()
                    && !id.is_empty()
                {
                    tracker.imported(
                        provider,
                        id,
                        Some(&song.artist),
                        Some(&song.title),
                        Some(&owned.absolute_path),
                    );
                }
            } else if let Some(song) = &song {
                job.missed
                    .lock()
                    .insert(key_of(song), format!("Not taken: {}.", drop.reason));
            }
            return None;
        }
        Some(tokio::spawn(self.clone().hand_to_pipeline(
            job.clone(),
            track.clone(),
            song,
            album,
            imported_path,
            before.clone(),
            landed.clone(),
        )))
    }

    /// Lidarr's webhook named an album it imported files of.
    fn lidarr_imported(self: Arc<Self>, event: LidarrImportEvent) -> LidarrImportOutcome {
        let key = event.foreign_album_id.trim().to_string();
        if key.is_empty() {
            return LidarrImportOutcome::NotOurs;
        }
        let running = self
            .jobs
            .lock()
            .get(&dotnet::ordinal_ignore_case_key(&key))
            .cloned();
        if let Some(job) = running {
            job.wake.notify_one();
            return LidarrImportOutcome::Woke;
        }
        // An upgrade replaces files Lidarr already had: the owner's, never this heart's.
        if event.is_upgrade {
            return LidarrImportOutcome::NotOurs;
        }
        let Some(pending) = self.pending.as_ref().and_then(|pending| pending.get(&key)) else {
            return LidarrImportOutcome::NotOurs;
        };
        // An upgrade borrowing the album deletes what its own search brings in, and another
        // heart on it takes what is its own; what is left is looked at again once they let go.
        if self
            .claims
            .as_ref()
            .is_some_and(|claims| claims.upgrade_busy(&key) || claims.heart_busy(&key))
        {
            tokio::spawn(self.after_claims(key, event));
            return LidarrImportOutcome::Late;
        }
        if let Some(claims) = &self.claims {
            claims.heart_started(&key);
        }
        tokio::spawn(async move {
            let album = format!("{} - {}", pending.artist, pending.title);
            match self.take_late_import(&pending, &event.paths).await {
                Ok(taken) => info!("Lidarr imported '{album}' late; {taken} file(s) handed to the pipeline"),
                Err(e) => warn!("Lidarr's late import of '{album}' was not taken: {e}"),
            }
            if let Some(claims) = &self.claims {
                claims.heart_ended(&key);
            }
        });
        LidarrImportOutcome::Late
    }

    /// How long a late import waits for an upgrade or another heart to let go of its album.
    const CLAIM_WAIT: Duration = Duration::from_secs(6 * 60 * 60);

    /// Looks at a late import again once nothing else holds its album.
    async fn after_claims(self: Arc<Self>, key: String, event: LidarrImportEvent) {
        let until = Instant::now() + Self::CLAIM_WAIT;
        loop {
            let busy = self
                .claims
                .as_ref()
                .is_some_and(|claims| claims.upgrade_busy(&key) || claims.heart_busy(&key));
            if !busy {
                break;
            }
            if Instant::now() >= until {
                warn!("Lidarr's late import of {key} was not taken: the album stayed busy");
                return;
            }
            let poll = self.timing.lock().claim_poll;
            tokio::time::sleep(poll).await;
        }
        self.lidarr_imported(event);
    }

    /// The files of a late import that Lidarr names and that are new since Octo's search,
    /// each through the pipeline as an album track nobody is waiting on. The album leaves the
    /// pending list once every track has a file; until then it stays, with the files dealt with
    /// here added to those never taken again. Answers how many files were handed on.
    async fn take_late_import(
        self: &Arc<Self>,
        pending: &PendingAlbum,
        paths: &[String],
    ) -> Result<usize, LidarrError> {
        let settings = self.settings.current().lidarr.clone();
        let octo_root = self.nav_identity.effective_download_path(
            &self
                .settings
                .raw("Library:DownloadPath")
                .unwrap_or_else(|| "/music".to_string()),
        );
        let album = Album {
            title: pending.title.clone(),
            artist: pending.artist.clone(),
            year: pending.year,
            ..Default::default()
        };
        let candidate = LidarrAlbumCandidate {
            id: pending.lidarr_album_id,
            foreign_album_id: pending.foreign_album_id.clone(),
            title: pending.title.clone(),
            artist: pending.artist.clone(),
            year: pending.year,
            resource: Default::default(),
        };
        let job = Arc::new(AlbumJob::late(candidate, album));
        for name in &pending.requested_by {
            job.join(&[], false, false, Some(name));
        }
        let before: Arc<HashSet<i32>> = Arc::new(pending.files_before.iter().copied().collect());
        let tracks_before: HashSet<i32> = pending.tracks_before.iter().copied().collect();
        let landed: Landed = Arc::default();

        let tracks = self.client.get_album_tracks(pending.lidarr_album_id).await?;
        let mut seen_paths: HashSet<String> = HashSet::new();
        let mut handed = Vec::new();
        let mut dealt_with: Vec<i32> = Vec::new();
        for track in &tracks {
            if !track.has_file
                || track.track_file_id <= 0
                || before.contains(&track.track_file_id)
                || tracks_before.contains(&track.id)
            {
                continue;
            }
            let Some(path) = track
                .path
                .as_deref()
                .filter(|p| !dotnet::is_null_or_white_space(Some(p)))
            else {
                continue;
            };
            if !paths.iter().any(|named| same_path(named, path))
                || !seen_paths.insert(dotnet::ordinal_ignore_case_key(path))
            {
                continue;
            }
            let imported_path =
                translate_imported_path(path, settings.root_folder_path.as_deref(), &octo_root)?;
            if !Path::new(&imported_path).is_file() {
                continue;
            }
            dealt_with.push(track.track_file_id);
            if let Some(handoff) = self
                .take_import(&job, track, imported_path, &before, &landed)
                .await
            {
                handed.push(handoff);
            }
        }
        let taken = handed.len();
        for handoff in handed {
            let _ = handoff.await;
        }
        if let Some(pending_albums) = &self.pending {
            let complete = self
                .client
                .get_album_tracks(pending.lidarr_album_id)
                .await
                .is_ok_and(|now| !now.is_empty() && now.iter().all(|t| t.has_file && t.track_file_id > 0));
            if complete {
                pending_albums.forget(&pending.foreign_album_id);
            } else {
                let mut files_before = pending.files_before.clone();
                files_before.extend(dealt_with);
                files_before.sort_unstable();
                files_before.dedup();
                pending_albums.remember(PendingAlbum {
                    files_before,
                    ..pending.clone()
                });
            }
        }
        Ok(taken)
    }

    /// One imported file through the download pipeline, as the song it is: a hearted song by its
    /// own id, so its progress row moves on, and any other track of the album by an id minted for
    /// it. Whatever the pipeline did not take is deleted through Lidarr, so nothing is left behind
    /// in the library unchecked.
    #[allow(clippy::too_many_arguments)]
    async fn hand_to_pipeline(
        self: Arc<Self>,
        job: Arc<AlbumJob>,
        track: LidarrImportedTrack,
        song: Option<Song>,
        album: Album,
        imported_path: String,
        before: Arc<HashSet<i32>>,
        landed: Landed,
    ) {
        let provider = SoulseekMetadataService::PROVIDER_NAME;
        let mut id = song.as_ref().and_then(|s| {
            let p = s.external_provider.as_deref()?;
            let e = s.external_id.as_deref().filter(|e| !e.is_empty())?;
            dotnet::eq_ignore_case(p, provider).then(|| e.to_string())
        });
        if id.is_none()
            && let Some(ids) = &self.ids
        {
            id = Some(ids.register(SoulseekRouting {
                kind: RoutingKind::Song,
                artist: Some(track.artist.clone().unwrap_or_else(|| album.artist.clone())),
                title: if dotnet::is_blank(&track.title) {
                    song.as_ref().map(|s| s.title.clone())
                } else {
                    Some(track.title.clone())
                },
                album: Some(album.title.clone()),
                duration: track.duration_seconds,
                track: track.track_number,
                ..Default::default()
            }));
        }
        let (Some(id), Some(downloads)) = (id, self.downloads.clone()) else {
            warn!(
                "Lidarr brought '{} - {}', but there is no download pipeline to take it",
                track.artist.as_deref().unwrap_or(""),
                track.title
            );
            return;
        };

        // A song nobody hearted lands quietly; an album heart gets one notice for the album.
        self.imports
            .offer(&id, &imported_path, song.is_none() || job.album_heart());
        let result = downloads
            .execute_acquisition(
                provider,
                &id,
                false,
                true,
                Some(DownloadSource::Lidarr),
                &CancellationToken::new(),
                job.asked_by(),
                false,
                None,
            )
            .await;
        match result {
            Ok(_) => {
                if self.imports.withdraw(&id) {
                    // The pipeline found the song in the library and never took this file.
                    self.drop_import(
                        &job,
                        &track,
                        &imported_path,
                        &before,
                        "Octo already had this song",
                    )
                    .await;
                }
                add_keys(&landed, song.as_ref(), &track, &album);
            }
            Err(e) => {
                self.imports.withdraw(&id);
                info!(
                    "Lidarr's '{} - {}' was not taken: {e}",
                    track.artist.as_deref().unwrap_or(""),
                    track.title
                );
                if let Some(song) = &song {
                    job.missed.lock().insert(key_of(song), e.to_string());
                }
                if Path::new(&imported_path).is_file() {
                    self.drop_import(
                        &job,
                        &track,
                        &imported_path,
                        &before,
                        &format!("Octo did not take it ({e})"),
                    )
                    .await;
                }
            }
        }
    }

    /// Why an imported file should not join the library, or None when it should. A song removed
    /// with a library action stays removed, and a song already in the library is not added twice,
    /// the same rules as a Soulseek download. An owned lossy copy is queued for Better quality
    /// instead, which swaps it in place and keeps its plays.
    async fn should_drop(
        &self,
        album: &Album,
        track: &LidarrImportedTrack,
        song: Option<&Song>,
        imported_path: &str,
        job: &AlbumJob,
    ) -> Option<DropReason> {
        let artist = match song {
            Some(song) => song.artist.clone(),
            None => track.artist.clone().unwrap_or_else(|| album.artist.clone()),
        };
        let title = if dotnet::is_blank(&track.title) {
            song.map(|s| s.title.clone())
        } else {
            Some(track.title.clone())
        };
        let title = title.filter(|t| !dotnet::is_blank(t))?;
        if dotnet::is_blank(&artist) {
            return None;
        }

        if let Some(journal) = &self.journal
            && journal.is_never_requested(Some(&artist), Some(&title))
        {
            return Some(DropReason {
                reason: "it was removed with a library action".into(),
                owned: None,
            });
        }

        if !self.settings.current().subsonic.skip_owned_songs {
            return None;
        }
        let ownership = self.ownership.as_ref()?;
        let owned = ownership
            .find(
                Some(&artist),
                Some(&title),
                track.duration_seconds.or_else(|| song.and_then(|s| s.duration)),
                Some(&album.title),
            )
            .await?;
        // The import itself, once Navidrome has scanned it, is not a second copy.
        if same_path(&owned.absolute_path, imported_path) {
            return None;
        }
        let suffix = dotnet::to_upper_invariant(&owned.suffix);
        if !owned.lossless
            && is_lossless(track)
            && self.queue_upgrade(&owned, &artist, &title, &album.title, job)
        {
            return Some(DropReason {
                reason: format!("you have it as {suffix}, which is queued for a higher quality copy"),
                owned: Some(owned),
            });
        }
        Some(DropReason {
            reason: format!("it is already in your library ({suffix})"),
            owned: Some(owned),
        })
    }

    /// Queue Better quality for an owned lossy copy, when every gate of the action is open
    /// for the person who hearted the album.
    fn queue_upgrade(
        &self,
        owned: &OwnedCopy,
        artist: &str,
        title: &str,
        album: &str,
        job: &AlbumJob,
    ) -> bool {
        let Some(navidrome_id) = &owned.navidrome_id else {
            return false;
        };
        let Some(askers) = job.asked_by().filter(|a| !a.is_empty()) else {
            return false;
        };
        let Some(queue) = &self.upgrades else {
            return false;
        };
        if !LibraryOwnership::upgrade_allowed(&self.settings.current().library_actions, Some(&askers[0])) {
            return false;
        }
        queue.add(
            vec![UpgradeAsk {
                navidrome_id: navidrome_id.clone(),
                title: Some(title.to_string()),
                artist: Some(artist.to_string()),
                album: Some(album.to_string()),
                suffix: Some(owned.suffix.clone()),
                attempt_key: None,
                pick: None,
            }],
            &askers[0],
            "heart",
        );
        true
    }

    /// Take an import back out. A file this heart's search brought in is deleted through Lidarr, so
    /// Lidarr's own records stay true; one Lidarr had before is the owner's and is only left alone,
    /// and so is every file of a late import.
    async fn drop_import(
        &self,
        job: &AlbumJob,
        track: &LidarrImportedTrack,
        imported_path: &str,
        before: &HashSet<i32>,
        reason: &str,
    ) {
        let artist = track.artist.as_deref().unwrap_or("");
        if !job.late && track.track_file_id > 0 && !before.contains(&track.track_file_id) {
            match self.client.delete_track_file(track.track_file_id).await {
                Ok(()) => {
                    info!(
                        "Lidarr brought '{artist} - {}', but {reason}; deleted it ({imported_path})",
                        track.title
                    );
                    return;
                }
                Err(e) => {
                    warn!(
                        "Could not delete Lidarr's '{artist} - {}' ({reason}): {e}",
                        track.title
                    );
                }
            }
        }
        info!(
            "Lidarr has '{artist} - {}', but {reason}; left it out of the library records",
            track.title
        );
    }

    async fn try_acquire(
        &self,
        work: impl Future<Output = anyhow::Result<bool>>,
        kind: &str,
        provider: &str,
        external_id: &str,
        notify_failure: bool,
    ) -> bool {
        match work.await {
            Ok(result) => result,
            Err(e) => {
                error!("Lidarr {kind} heart failed for {external_id}: {e:#}");
                // notifyFailure is true only for the last source in the chain, which is also the
                // only failure the progress list may show.
                if notify_failure {
                    let message = e.to_string();
                    if let Some(tracker) = &self.tracker {
                        if kind == "album" {
                            tracker.fail_album(provider, external_id, Some(&message));
                        } else {
                            tracker.fail(provider, external_id, Some(&message));
                        }
                    }
                    self.notifications.notify(NotificationEvent {
                        source: Some("Lidarr".into()),
                        detail: Some(message),
                        ..NotificationEvent::new(NotificationEventType::DownloadFailed)
                    });
                }
                false
            }
        }
    }
}

/// `SamePath`: the two full paths are one (ordinal, as on Linux).
pub(crate) fn same_path(a: &str, b: &str) -> bool {
    get_full_path(a) == get_full_path(b)
}

/// Where Lidarr's imported file is in Octo's library: its path under Lidarr's root folder, taken
/// onto Octo's root. Throws for a path outside that root, and for one that would land outside
/// Octo's.
pub fn translate_imported_path(
    lidarr_path: &str,
    lidarr_root: Option<&str>,
    octo_root: &str,
) -> Result<String, LidarrError> {
    let Some(lidarr_root) = lidarr_root.filter(|r| !dotnet::is_blank(r)) else {
        return Err(LidarrError::InvalidOperation(
            "Lidarr root folder is not configured.".into(),
        ));
    };
    let root = get_full_path(lidarr_root);
    let source = get_full_path(lidarr_path);
    let relative = get_relative_path(&root, &source);
    if relative == ".." || relative.starts_with("../") {
        return Err(LidarrError::InvalidOperation(format!(
            "Lidarr imported a path outside its configured root: {lidarr_path}"
        )));
    }
    let target_root = get_full_path(octo_root);
    let target = get_full_path(&path_combine(&target_root, &relative));
    if get_relative_path(&target_root, &target).starts_with("..") {
        return Err(LidarrError::InvalidOperation(
            "Translated Lidarr path escaped Octo's library root.".into(),
        ));
    }
    Ok(target)
}

/// Joins `b` onto `a` with one `/`; a `b` that starts with `/` replaces `a`.
fn path_combine(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_string()
    } else if a.is_empty() || a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// `path` relative to `relative_to`, for two full paths on Unix (case-sensitive): "."
/// for the same path, the rest of `path` under `relative_to`, otherwise a climb out with `..`.
fn get_relative_path(relative_to: &str, path: &str) -> String {
    let from: Vec<&str> = relative_to.split('/').filter(|s| !s.is_empty()).collect();
    let to: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    if common == from.len() && common == to.len() {
        return ".".to_string();
    }
    let mut parts: Vec<&str> = vec![".."; from.len() - common];
    parts.extend(&to[common..]);
    let mut relative = parts.join("/");
    // A trailing separator on the path survives.
    if path.ends_with('/') && !relative.is_empty() {
        relative.push('/');
    }
    relative
}

#[cfg(test)]
#[path = "lidarr_heart_protection_tests.rs"]
mod tests;
