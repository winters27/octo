//! Hearts fetched through Lidarr. Which song an imported track is (`match_song`) is
//! `octo_core::lidarr::lidarr_heart_acquisition_service`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use octo_core::common::SongIdentity;
use octo_core::common::dotnet;
use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, HeardRecording, HeartGate};
use octo_core::fingerprint::heart_release::{HeardIdentity, HeartSong};
use octo_core::lidarr::lidarr_client::LidarrAlbum;
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

use super::lidarr_pick::{
    ALREADY_LOOKING, LidarrArtistMusic, delete_moved_record, put_back_unsearched,
    put_back_unsearched_after_restart, retry_moved_records, users_queue_item, wait_for_rescans,
};
use super::{
    LidarrAlbumClaims, LidarrClient, LidarrImportHandoff, LidarrPendingAlbums, PendingAdd, PendingAlbum,
};
use crate::services::common::song_finder::LIDARR_LEFT_ALONE;
use crate::services::common::{AcquisitionState, AcquisitionTracker};
use crate::services::fingerprint::MusicBrainzClient;
use crate::services::fingerprint::download_verification_service::{pick_contexts, song_in};
use crate::services::fingerprint::music_brainz_client::{
    CatalogQuestions, HeartBusy, HeartQuestions, Lane, heart_song,
};
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

    /// Whether Lidarr's last try for the song left Lidarr alone: every album it would have
    /// searched belongs to an artist that already has music in the user's Lidarr, so Lidarr
    /// cannot look for the song.
    fn left_alone(&self, _provider: &str, _external_id: &str) -> bool {
        false
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
    /// The catalog side of a song's identity and walk; Deezer when None.
    pub catalog: Option<Arc<dyn CatalogQuestions>>,
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
    catalog: Arc<dyn CatalogQuestions>,
    /// What the user's Lidarr holds of an artist.
    music: LidarrArtistMusic,
    /// The songs whose last try left Lidarr alone, by provider and id ignoring case.
    left_alone: Mutex<HashSet<String>>,

    /// By foreign album id, ignoring case.
    jobs: Mutex<HashMap<String, Arc<AlbumJob>>>,
    timing: Mutex<Timing>,
}

/// What a pick's album needs for its pin: the release the person listened from when the album is
/// its group, and the recordings whose carriers name the album's releases.
#[derive(Debug, Clone, Default)]
struct PinHint {
    listened_release: Option<String>,
    recordings: Vec<String>,
    lane: Option<Lane>,
}

/// How one pick ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PickEnd {
    /// The song is in the library.
    Landed,
    /// Lidarr was not used for the album, with the downloads line that says why when there is
    /// one.
    Skipped(Option<&'static str>),
    /// Lidarr searched and the song did not land.
    Missed,
}

/// How a pick's preparation ended.
enum Prepared {
    /// The search was posted for the album.
    Searched {
        album_id: i32,
    },
    Skipped(Option<&'static str>),
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
    /// What the album's pin reads, from the first heart whose walk picked it.
    hint: Mutex<Option<PinHint>>,
    /// Lidarr was not used for the album, and the line that says why.
    skipped: Mutex<Option<Option<&'static str>>>,
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
            hint: Mutex::new(None),
            skipped: Mutex::new(None),
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

/// The MusicBrainz id of the artist a lookup row files the album under: the album's own Lidarr
/// artist, which Lidarr's import keeps.
fn artist_of(candidate: &LidarrAlbumCandidate) -> Option<String> {
    let artist = candidate.resource.get("artist")?.as_object()?;
    ["foreignArtistId", "mbId"]
        .iter()
        .find_map(|key| artist.get(*key).and_then(|id| id.as_str()))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn left_alone_key(provider: &str, external_id: &str) -> String {
    dotnet::ordinal_ignore_case_key(&format!("{}:{}", provider.trim(), external_id.trim()))
}

/// The MusicBrainz lane a song's walk asks on: a person waiting on a pick asks in the foreground,
/// imports and replacements in the background, every other origin on the heart's lane.
fn lane_for(origin: AcquisitionOrigin) -> Lane {
    match origin {
        AcquisitionOrigin::SongFinderPick => Lane::Foreground,
        AcquisitionOrigin::Import | AcquisitionOrigin::Replacement => Lane::Background,
        _ => Lane::Heart,
    }
}

/// Where a song's walk starts.
enum WalkStart {
    Picks(Walk),
    /// No recording to walk from: the album is found by name.
    NoIdentity,
    /// MusicBrainz did not answer.
    Busy,
}

/// The release groups Lidarr is asked for, in the walk's order: the listened-from group first,
/// known without the carriers, which are asked only when the walk needs a second pick. Each group
/// is given once.
struct Walk {
    questions: HeartQuestions,
    song: HeartSong,
    identity: HeardIdentity,
    picks: VecDeque<String>,
    walked: bool,
    seen: HashSet<String>,
}

impl Walk {
    async fn next(&mut self) -> Option<String> {
        loop {
            if let Some(pick) = self.picks.pop_front() {
                if self.seen.insert(pick.to_ascii_lowercase()) {
                    return Some(pick);
                }
                continue;
            }
            if self.walked {
                return None;
            }
            self.walked = true;
            match self.questions.walk(&self.song, &self.identity).await {
                Ok(picks) => self.picks.extend(picks),
                Err(busy) => {
                    let what = match busy {
                        HeartBusy::MusicBrainz(question) => format!("MusicBrainz ({question})"),
                        HeartBusy::Deezer => "Deezer".to_string(),
                    };
                    warn!(
                        "{what} did not answer for the walk of '{} - {}'; no further pick",
                        self.song.artist, self.song.title
                    );
                    return None;
                }
            }
        }
    }

    /// What a pick's pin reads: the listened-from release on the listened-from group, and the
    /// heard recordings.
    fn hint(&self, pick: &str) -> PinHint {
        let listened = self
            .identity
            .listened_group
            .as_deref()
            .is_some_and(|group| dotnet::eq_ignore_case(group, pick));
        PinHint {
            listened_release: self.identity.listened_release.clone().filter(|_| listened),
            recordings: self.identity.recordings.clone(),
            lane: Some(self.questions.lane()),
        }
    }
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
        let catalog = extras
            .catalog
            .unwrap_or_else(|| deezer.clone() as Arc<dyn CatalogQuestions>);
        let music = LidarrArtistMusic::new(client.clone(), settings.clone(), nav_identity.clone());
        LidarrHeartAcquisitionService {
            inner: Arc::new(Inner {
                client,
                metadata,
                deezer,
                settings,
                nav_identity,
                notifications,
                catalog,
                music,
                left_alone: Mutex::new(HashSet::new()),
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

    /// What the user's Lidarr holds of an artist, read as this service reads it.
    pub fn artist_music(&self) -> LidarrArtistMusic {
        self.inner.music.clone()
    }

    /// Puts back, in the background, every pick a restart cut short before its search. Does
    /// nothing outside a runtime.
    pub fn resume_after_restart(&self) {
        let (Some(pending), Ok(runtime)) =
            (self.inner.pending.clone(), tokio::runtime::Handle::try_current())
        else {
            return;
        };
        let inner = self.inner.clone();
        runtime.spawn(async move {
            let poll = inner.poll(&inner.settings.current().lidarr);
            put_back_unsearched_after_restart(&inner.client, &pending, poll).await;
        });
    }

    /// The same, waited for, for tests.
    #[cfg(test)]
    pub(crate) async fn put_back_after_restart(&self) {
        if let Some(pending) = &self.inner.pending {
            let poll = self.inner.poll(&self.inner.settings.current().lidarr);
            put_back_unsearched_after_restart(&self.inner.client, pending, poll).await;
        }
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
        origin: AcquisitionOrigin,
        gates: &[HeartGate],
    ) -> anyhow::Result<bool> {
        let work =
            self.inner
                .acquire_track(provider, external_id, notify_failure, requested_by, origin, gates);
        Ok(self
            .inner
            .try_acquire(work, "track", provider, external_id, notify_failure)
            .await)
    }

    fn left_alone(&self, provider: &str, external_id: &str) -> bool {
        self.inner
            .left_alone
            .lock()
            .contains(&left_alone_key(provider, external_id))
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
    /// A song through Lidarr: the albums the walk picks for what the person heard, each in turn
    /// until the song lands (at most four), or, with no identity to walk from, the album found by
    /// the song's names. A pick Lidarr does not know, or whose artist already has music in the
    /// user's Lidarr, is passed over for the next.
    async fn acquire_track(
        self: &Arc<Self>,
        provider: &str,
        external_id: &str,
        notify_failure: bool,
        requested_by: Option<&str>,
        origin: AcquisitionOrigin,
        gates: &[HeartGate],
    ) -> anyhow::Result<bool> {
        let song = self
            .metadata
            .get_song(provider, external_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("The starred external track is no longer available."))?;

        let mut ends: Vec<PickEnd> = Vec::new();
        let mut last: Option<(Arc<AlbumJob>, Song)> = None;
        match self.walk(external_id, origin, gates, requested_by).await {
            WalkStart::Picks(mut walk) => {
                while let Some(pick) = walk.next().await {
                    let Some(candidate) = self.client.resolve_album_by_foreign_id(&pick).await? else {
                        info!("Lidarr does not know release group {pick}; the walk goes on");
                        continue;
                    };
                    let mut song = song.clone();
                    song.album = candidate.title.clone();
                    let album = Album {
                        title: candidate.title.clone(),
                        artist: candidate.artist.clone(),
                        year: candidate.year,
                        songs: vec![song.clone()],
                        ..Default::default()
                    };
                    let hint = walk.hint(&pick);
                    let job = self
                        .queue_resolved_album(&album, requested_by, Some(candidate), false, false, Some(hint))
                        .await?;
                    let end = self.pick_end(&job, &song).await;
                    if end == PickEnd::Landed {
                        self.set_left_alone(provider, external_id, false);
                        return Ok(true);
                    }
                    self.note_skip(provider, external_id, &end);
                    ends.push(end);
                    last = Some((job, song));
                }
            }
            WalkStart::Busy => {
                info!("MusicBrainz did not answer for {provider}:{external_id}; Lidarr is not asked");
            }
            WalkStart::NoIdentity => {
                let (album, song) = self.album_by_name(song).await?;
                let job = self
                    .queue_resolved_album(&album, requested_by, None, false, false, None)
                    .await?;
                let end = self.pick_end(&job, &song).await;
                if end == PickEnd::Landed {
                    self.set_left_alone(provider, external_id, false);
                    return Ok(true);
                }
                self.note_skip(provider, external_id, &end);
                ends.push(end);
                last = Some((job, song));
            }
        }
        let left_alone = !ends.is_empty()
            && ends
                .iter()
                .all(|end| *end == PickEnd::Skipped(Some(LIDARR_LEFT_ALONE)));
        self.set_left_alone(provider, external_id, left_alone);
        if notify_failure && let Some(tracker) = &self.tracker {
            let reason = match ends.last() {
                Some(PickEnd::Skipped(Some(line))) => Some(line.to_string()),
                _ => last.and_then(|(job, song)| job.missed_for(&key_of(&song))),
            }
            .unwrap_or_else(|| "Lidarr did not bring this song.".to_string());
            tracker.fail(provider, external_id, Some(&reason));
        }
        Ok(false)
    }

    /// The album the song's names find: the album Deezer files it under, else its own.
    async fn album_by_name(&self, mut song: Song) -> anyhow::Result<(Album, Song)> {
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
        let album = Album {
            title: album_title,
            artist: enriched
                .as_ref()
                .and_then(|e| e.artist_name.clone())
                .unwrap_or_else(|| song.artist.clone()),
            year: enriched.as_ref().and_then(|e| e.year),
            cover_art_url: enriched.as_ref().and_then(|e| e.album_cover_url.clone()),
            songs: vec![song.clone()],
            ..Default::default()
        };
        Ok((album, song))
    }

    /// How one pick's job ended for the song.
    async fn pick_end(&self, job: &AlbumJob, song: &Song) -> PickEnd {
        let landed = job.wait_ended().await;
        if landed.contains(&key_of(song))
            || landed.contains(&title_key(Some(&song.artist), Some(&song.title)))
        {
            return PickEnd::Landed;
        }
        match *job.skipped.lock() {
            Some(line) => PickEnd::Skipped(line),
            None => PickEnd::Missed,
        }
    }

    /// The downloads entry says why Lidarr was not used for a pick.
    fn note_skip(&self, provider: &str, external_id: &str, end: &PickEnd) {
        if let (PickEnd::Skipped(Some(line)), Some(tracker)) = (end, &self.tracker) {
            tracker.stage(
                provider,
                external_id,
                AcquisitionState::Searching,
                Some("Lidarr"),
                Some(line),
            );
        }
    }

    fn set_left_alone(&self, provider: &str, external_id: &str, left_alone: bool) {
        let key = left_alone_key(provider, external_id);
        let mut songs = self.left_alone.lock();
        if left_alone {
            songs.insert(key);
        } else {
            songs.remove(&key);
        }
    }

    /// The walk for the song as the person heard it: its routing and the album context the
    /// person listed it from (the one whose recordings are the gate's R, when the gate knows R),
    /// then the identity those give. When the gate's R is none of the identity's recordings,
    /// what played became another recording, and the walk follows it. No routing, no MusicBrainz,
    /// or no recording to walk from: the album is found by name. A family request MusicBrainz does
    /// not answer for goes on by name too; any other origin leaves Lidarr out.
    async fn walk(
        &self,
        external_id: &str,
        origin: AcquisitionOrigin,
        gates: &[HeartGate],
        requested_by: Option<&str>,
    ) -> WalkStart {
        let (Some(music_brainz), Some(ids)) = (self.music_brainz.clone(), self.ids.clone()) else {
            return WalkStart::NoIdentity;
        };
        let personal = matches!(
            origin,
            AcquisitionOrigin::Heart | AcquisitionOrigin::AlbumHeart | AcquisitionOrigin::Play
        );
        let Some(routing) = ids
            .resolve_song(external_id, personal)
            .map(|shared| shared.snapshot())
        else {
            return WalkStart::NoIdentity;
        };
        let busy = if origin == AcquisitionOrigin::Request {
            WalkStart::NoIdentity
        } else {
            WalkStart::Busy
        };
        let song = heart_song(&routing);
        let questions = HeartQuestions::new(music_brainz, self.catalog.clone(), lane_for(origin));
        let r: Option<HeardRecording> = gates
            .iter()
            .find_map(|gate| gate.r.clone())
            .filter(|r| !r.ids.is_empty());
        let user = gates
            .first()
            .map(|gate| gate.user.clone())
            .or_else(|| requested_by.map(str::to_string))
            .unwrap_or_default();
        let candidates = pick_contexts(&ids.contexts(external_id), &user);
        let mut chosen: Option<(HeartSong, HeardIdentity)> = None;
        if candidates.is_empty() {
            match questions.identity(&song, None).await {
                Ok(identity) => chosen = Some((song.clone(), identity)),
                Err(_) => return busy,
            }
        }
        for context in &candidates {
            let listed = song_in(&song, context);
            let identity = match questions.identity(&listed, Some(&context.catalog_album_id)).await {
                Ok(identity) => identity,
                Err(_) => return busy,
            };
            let fits = r
                .as_ref()
                .is_some_and(|r| identity.recordings.iter().any(|id| r.holds(id)));
            if chosen.is_none() || fits {
                chosen = Some((listed, identity));
            }
            if fits {
                break;
            }
        }
        let Some((song, mut identity)) = chosen else {
            return WalkStart::NoIdentity;
        };
        if let Some(r) = &r
            && !identity.recordings.iter().any(|id| r.holds(id))
        {
            identity.recordings = r.ids.clone();
            identity.recordings_uncapped = r.ids.clone();
            identity.heard_track = None;
            identity.listened_release = None;
            identity.listened_group = None;
        }
        if identity.recordings.is_empty() {
            return WalkStart::NoIdentity;
        }
        let picks: VecDeque<String> = identity.listened_group.iter().cloned().collect();
        WalkStart::Picks(Walk {
            questions,
            song,
            identity,
            picks,
            walked: false,
            seen: HashSet::new(),
        })
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
            .queue_resolved_album(&album, requested_by, None, true, true, None)
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
        hint: Option<PinHint>,
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
            if let Some(hint) = hint {
                job.hint.lock().get_or_insert(hint);
            }
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
        let album_id = match self.prepare(&job, &snapshot).await {
            Ok(Prepared::Searched { album_id }) => album_id,
            Ok(Prepared::Skipped(line)) => {
                self.end_skipped(&job, line);
                if let Some(claims) = &self.claims {
                    claims.heart_ended(&key);
                }
                return Ok(());
            }
            Err(e) => {
                if let Some(claims) = &self.claims {
                    claims.heart_ended(&key);
                }
                return Err(e.into());
            }
        };
        // The artist had no track file right before the search, so the album had none either:
        // every file it holds from here on came after the search.
        let before: HashSet<i32> = HashSet::new();

        let album = &job.album;
        info!(
            "Lidarr accepted AlbumSearch for '{} - {}' ({key}, local id {album_id})",
            album.artist, album.title
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
            pending.update(&key, |entry| {
                entry.files_before = files_before;
                entry.requested_by = job.asked_by().unwrap_or_default();
            });
        }

        let this = self.clone();
        let before = Arc::new(before);
        tokio::spawn(async move {
            let landed: Landed = Arc::default();
            let album = &job.album;
            let reconciled = this
                .reconcile_imports(&job, album_id, &snapshot, &before, &landed)
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
            if let Some(claims) = &this.claims {
                claims.heart_ended(&key);
            }
        });
        Ok(())
    }

    /// The poll pace of Lidarr's imports and commands: the timeout's thirtieth, 1 to 10 s.
    fn poll(&self, settings: &LidarrSettings) -> Duration {
        self.timing.lock().poll_override.unwrap_or_else(|| {
            Duration::from_secs((settings.import_timeout_seconds / 30).clamp(1, 10) as u64)
        })
    }

    /// A job whose album Lidarr was not used for ends at once with nothing landed, each song
    /// missed with the line that says why.
    fn end_skipped(&self, job: &Arc<AlbumJob>, line: Option<&'static str>) {
        *job.skipped.lock() = Some(line);
        {
            let mut missed = job.missed.lock();
            for song in &job.view().songs {
                missed.insert(
                    key_of(song),
                    line.unwrap_or("Lidarr did not bring this song.").to_string(),
                );
            }
        }
        let job_key = dotnet::ordinal_ignore_case_key(&job.candidate.foreign_album_id);
        {
            let mut jobs = self.jobs.lock();
            if jobs
                .get(&job_key)
                .is_some_and(|current| Arc::ptr_eq(current, job))
            {
                jobs.remove(&job_key);
            }
        }
        job.ended.send_if_modified(|ended| {
            if ended.is_some() {
                return false;
            }
            *ended = Some(Landed::default());
            true
        });
    }

    /// Everything a pick does up to its search, each read on work and never on a timer:
    /// - "When Lidarr is used": the album's Lidarr artist with music in the user's Lidarr (a track
    ///   file, or audio in its folder) leaves Lidarr alone, with nothing sent;
    /// - a monitored album is the user's, and Lidarr already looks for it; a queue item of the
    ///   user's on the artist is a grab whose files Octo's import could replace;
    /// - the entry is saved before anything is sent, with the add about to go out;
    /// - an album Lidarr lacks is added unmonitored (and its artist, monitoring nothing);
    /// - its track list, then the add's refresh and rescan, are waited for, and the artist is read
    ///   again: anything the rescan linked means the artist has music;
    /// - the album is pinned to a release that carries what was heard, the pin's rescan waited
    ///   for, and the artist read once more;
    /// - only then the search, whose command id is saved at once.
    ///
    /// A pick skipped after anything was sent is put back.
    async fn prepare(&self, job: &Arc<AlbumJob>, settings: &LidarrSettings) -> Result<Prepared, LidarrError> {
        let candidate = &job.candidate;
        let key = candidate.foreign_album_id.clone();
        let Some(foreign_artist_id) = artist_of(candidate) else {
            warn!("Lidarr's lookup of {key} names no artist; Lidarr is not asked for it");
            return Ok(Prepared::Skipped(None));
        };
        let music = self.music.read(&foreign_artist_id).await?;
        if music.has_music {
            info!(
                "Lidarr's artist of {key} already has music; Lidarr is left alone for '{} - {}'",
                job.album.artist, job.album.title
            );
            return Ok(Prepared::Skipped(Some(LIDARR_LEFT_ALONE)));
        }
        let existing = self.client.album_by_foreign_id(&key).await?;
        if existing.as_ref().is_some_and(|album| album.monitored) {
            return Ok(Prepared::Skipped(Some(ALREADY_LOOKING)));
        }
        if let Some(artist) = &music.artist
            && users_queue_item(&self.client, self.pending.as_deref(), artist.id).await?
        {
            info!("Lidarr is fetching something of the artist of {key} for the user; Octo stays out");
            return Ok(Prepared::Skipped(None));
        }

        let mut entry = PendingAlbum {
            foreign_album_id: key.clone(),
            lidarr_album_id: existing.as_ref().map_or(0, |album| album.id),
            lidarr_artist_id: music.artist.as_ref().map_or(0, |artist| artist.id),
            artist: job.album.artist.clone(),
            title: job.album.title.clone(),
            year: job.album.year,
            requested_by: job.asked_by().unwrap_or_default(),
            asked_at: Utc::now(),
            ..Default::default()
        };
        if existing.is_none() {
            entry.add = Some(PendingAdd {
                foreign_artist_id: foreign_artist_id.clone(),
                posted_at: Utc::now(),
                artists_before: self.client.artists().await?.iter().map(|a| a.id).collect(),
            });
        }
        self.save(&entry);

        let prepared = self
            .prepare_saved(job, settings, &mut entry, existing, &music)
            .await;
        let skipped = match &prepared {
            Ok(Prepared::Searched { .. }) => return prepared,
            Ok(Prepared::Skipped(_)) => true,
            Err(_) => false,
        };
        // Skipped, or Lidarr failed partway: put back what was sent. A put-back that fails is
        // left to the next start.
        let poll = self.poll(settings);
        match put_back_unsearched(&self.client, self.pending.as_deref(), &entry, poll).await {
            Ok(()) if skipped => info!("Put back Lidarr's album {key}: nothing was searched"),
            Ok(()) => {}
            Err(e) => warn!("Could not put back Lidarr's album {key}: {e}"),
        }
        prepared
    }

    async fn prepare_saved(
        &self,
        job: &Arc<AlbumJob>,
        settings: &LidarrSettings,
        entry: &mut PendingAlbum,
        existing: Option<LidarrAlbum>,
        music: &super::ArtistMusic,
    ) -> Result<Prepared, LidarrError> {
        let poll = self.poll(settings);
        let (album_id, artist_id) = match &existing {
            Some(album) => (album.id, album.artist_id),
            None => {
                let added = self
                    .client
                    .add_album(&job.candidate, music.artist.as_ref())
                    .await?;
                entry.lidarr_album_id = added.album_id;
                entry.lidarr_artist_id = added.artist_id;
                entry.album_added = true;
                if music.artist.is_none() {
                    entry.artist_added = added.artist_id;
                }
                self.save(entry);
                (added.album_id, added.artist_id)
            }
        };
        if !self.client.wait_for_tracks(album_id, artist_id).await? {
            warn!("Lidarr lists no tracks for album {album_id}; it is not searched");
            return Ok(Prepared::Skipped(None));
        }
        let artist_path = self.client.artist(artist_id).await?.and_then(|a| a.path);
        wait_for_rescans(&self.client, album_id, artist_id, artist_path.as_deref(), poll).await?;
        if entry.artist_added > 0 {
            entry.albums_after_add = self
                .client
                .artist_albums(artist_id)
                .await?
                .iter()
                .map(|album| album.id)
                .collect();
            self.save(entry);
        }
        let album = match self.read_again(album_id, artist_id).await? {
            Ok(album) => album,
            Err(line) => return Ok(Prepared::Skipped(line)),
        };

        if let Some(pin) = self.pin_for(job, &album).await {
            entry.release_choice = Some(album.release_choice());
            entry.pin = Some(pin.clone());
            self.save(entry);
            self.client.set_release_choice(album_id, &pin).await?;
            wait_for_rescans(&self.client, album_id, artist_id, artist_path.as_deref(), poll).await?;
            if let Err(line) = self.read_again(album_id, artist_id).await? {
                return Ok(Prepared::Skipped(line));
            }
        }

        let command = self.client.album_search(album_id).await?;
        entry.search_command = command.id;
        entry.searched_at = Some(command.queued.unwrap_or_else(Utc::now));
        self.save(entry);
        Ok(Prepared::Searched { album_id })
    }

    /// The artist and the album read again right before the pin and the search: any track
    /// file of the artist (one a rescan linked included) leaves Lidarr alone; a monitored album is
    /// the user's; a queue item of the user's keeps Octo out. The album as it stands otherwise.
    async fn read_again(
        &self,
        album_id: i32,
        artist_id: i32,
    ) -> Result<Result<LidarrAlbum, Option<&'static str>>, LidarrError> {
        if !self.client.artist_track_files(artist_id).await?.is_empty() {
            return Ok(Err(Some(LIDARR_LEFT_ALONE)));
        }
        let Some(album) = self.client.album(album_id).await? else {
            return Ok(Err(None));
        };
        if album.monitored {
            return Ok(Err(Some(ALREADY_LOOKING)));
        }
        if users_queue_item(&self.client, self.pending.as_deref(), artist_id).await? {
            return Ok(Err(None));
        }
        Ok(Ok(album))
    }

    /// The release choice that pins the album to one release carrying what was heard: on the
    /// listened-from group the release the person listened from, when Lidarr lists it; otherwise
    /// the first carrier of a heard recording in this group that Lidarr lists. None when Lidarr
    /// lists none (the album is searched unpinned), and when MusicBrainz does not answer.
    async fn pin_for(
        &self,
        job: &AlbumJob,
        album: &LidarrAlbum,
    ) -> Option<octo_core::lidarr::lidarr_client::LidarrReleaseChoice> {
        let hint = job.hint.lock().clone()?;
        if let Some(listened) = &hint.listened_release
            && let Some(release) = album.release(listened)
        {
            return album.pinned_to(release.id);
        }
        let music_brainz = self.music_brainz.clone()?;
        let questions = HeartQuestions::new(
            music_brainz,
            self.catalog.clone(),
            hint.lane.unwrap_or(Lane::Heart),
        );
        for recording in &hint.recordings {
            let carriers = questions.carriers(recording).await.ok()?;
            for carrier in carriers
                .releases
                .iter()
                .filter(|carrier| dotnet::eq_ignore_case(&carrier.group.id, &album.foreign_album_id))
            {
                if let Some(release) = album.release(&carrier.id) {
                    return album.pinned_to(release.id);
                }
            }
        }
        None
    }

    /// Saves a pick's entry: it replaces what was listed for the album.
    fn save(&self, entry: &PendingAlbum) {
        if let Some(pending) = &self.pending {
            pending.remember(entry.clone());
        }
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
            if let Some(pending) = &self.pending {
                retry_moved_records(&self.client, pending, &job.candidate.foreign_album_id).await;
            }
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
        let mut tracks_taken: Vec<i32> = Vec::new();
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
            tracks_taken.push(track.id);
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
            // A track Octo took counts as in, though Lidarr no longer has its record once Octo
            // moved the file.
            let mut tracks_taken: Vec<i32> =
                pending.tracks_taken.iter().copied().chain(tracks_taken).collect();
            tracks_taken.sort_unstable();
            tracks_taken.dedup();
            let complete = self
                .client
                .get_album_tracks(pending.lidarr_album_id)
                .await
                .is_ok_and(|now| {
                    !now.is_empty()
                        && now
                            .iter()
                            .all(|t| (t.has_file && t.track_file_id > 0) || tracks_taken.contains(&t.id))
                });
            if complete {
                pending_albums.forget(&pending.foreign_album_id);
            } else {
                let mut files_before = pending.files_before.clone();
                files_before.extend(dealt_with);
                files_before.sort_unstable();
                files_before.dedup();
                pending_albums.update(&pending.foreign_album_id, |entry| {
                    entry.files_before = files_before;
                    entry.tracks_taken = tracks_taken;
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
                } else {
                    // The pipeline moved the file into Octo's own layout: Lidarr's record of it
                    // goes too, so the artist reads as having no files and Lidarr stays usable.
                    delete_moved_record(
                        &self.client,
                        self.pending.as_deref(),
                        &job.candidate.foreign_album_id,
                        track.track_file_id,
                        &imported_path,
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
