//! Find songs, and the `UpgradeGate` and `LibraryActionStates` beside it.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use futures::FutureExt;
use futures::future::BoxFuture;
use octo_core::common::acquisition_tracker::user_safe;
use octo_core::common::{Clock, dotnet};
use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, WaitingFor};
use octo_core::fingerprint::recording_identity::RecordingKind;
use octo_core::lidarr::{LidarrRelease, LidarrTrackRequest};
use octo_core::models::download::AcquisitionCandidate;
use octo_core::settings::{
    DownloadSource, HeartDownloadSource, LibraryAction, LibraryActionSettings, SettingsStore,
    SoulseekSettings,
};
use octo_core::soulseek::soulseek_link::{OFFLINE_TEXT, SoulseekLinkState};
use octo_core::soulseek::{RoutingKind, SoulseekFileHit};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::acquisition_tracker::{AcquisitionState, AcquisitionTracker, acquisition_kinds, key_of};
use super::download_picks::{DownloadPicks, LIDARR_SOURCE, PickedCopy, SOULSEEK_SOURCE};
use super::heart_acquisition_coordinator::{GateStart, HeartGates};
use super::replace_anyway::{self, OtherTake, PickCompare, ReplaceCheck};
use super::soulseek_hold_store::Holder;
use super::track_acquisition_queue::{AcquisitionHeld, TrackAcquisitionQueue};
use crate::services::common::AcquisitionTarget;
use crate::services::family::LibraryWriteGate;
use crate::services::library::library_action_executor::LibraryActionExecutor;
use crate::services::library::library_action_journal::LibraryActionJournal;
use crate::services::library::upgrade_queue::{AudioSummary, UpgradeAsk, UpgradeQueue};
use crate::services::library::{NavidromeSongPathResolver, UpgradeSources};
use crate::services::lidarr::{LidarrArtistMusic, LidarrClient, LidarrTrackFetcher};
use crate::services::local::ILocalLibraryService;
use crate::services::soulseek::rejected_peer_registry::RejectedPeerRegistry;
use crate::services::soulseek::soulseek_candidates::{self, Judged};
use crate::services::soulseek::soulseek_download_service::SoulseekDownloadService;
use crate::services::soulseek::{ExternalIdRegistry, ISoulseekLink, SoulseekMetadataService};

/// The song Find songs looks for: an outside song by its Octo id, or a library song by its
/// Navidrome id, with what the library holds of it now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FindTarget {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration: Option<i32>,
    pub cover_art: String,
    pub external_id: Option<String>,
    pub library_id: Option<String>,
    pub owned_format: Option<String>,
    pub owned_quality: Option<String>,
    pub owned_size: Option<i64>,
    pub owned_path: Option<String>,
    /// What take the library song's own file is by its audio, when that was asked: a live take
    /// or an instrumental is looked for as the plain song.
    pub audio_take: Option<RecordingKind>,
}

impl FindTarget {
    /// Whether the library song's own file was found to be another take than the studio one.
    pub fn is_another_take(&self) -> bool {
        self.audio_take.is_some_and(|kind| kind != RecordingKind::Studio)
    }
}

impl FindTarget {
    /// The song to look for; the rest starts empty.
    pub fn new(
        artist: impl Into<String>,
        title: impl Into<String>,
        album: Option<&str>,
        duration: Option<i32>,
        cover_art: impl Into<String>,
    ) -> Self {
        FindTarget {
            artist: artist.into(),
            title: title.into(),
            album: album.map(str::to_string),
            duration,
            cover_art: cover_art.into(),
            ..Default::default()
        }
    }
}

/// The states a look and each of its sources go through.
pub mod find_states {
    pub const SEARCHING: &str = "searching";
    pub const DONE: &str = "done";
    pub const FAILED: &str = "failed";
    /// A source that is not set up, or not one of the person's download sources.
    pub const OFF: &str = "off";
}

/// The states a pick, or an app's library action, answers with on the wire.
pub mod library_action_states {
    pub const QUEUED: &str = "queued";
    pub const SKIPPED: &str = "skipped";
    /// The pick waits for the person's word: the copy is no better than the library's, or the
    /// library's is another take of the song. Picking again with `anyway` replaces it.
    pub const CONFIRM: &str = "confirm";
    /// Asked first (`anyway=false`) about a better copy of the right song: nothing is fetched yet,
    /// and picking again with `anyway=true` replaces the library's copy with it.
    pub const READY: &str = "ready";
}

/// How one source's look went: searching, done, failed or off, in words, and what was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindSource {
    pub name: String,
    pub state: String,
    pub text: Option<String>,
    pub queries: Vec<String>,
}

/// One copy on the list: what the apps show, what picking it fetches, and its id on the list,
/// given when it is added and never reused within one look, which pickFoundSong takes.
#[derive(Debug, Clone, PartialEq)]
pub struct FoundCopy {
    pub shown: AcquisitionCandidate,
    pub pick: Arc<PickedCopy>,
    pub id: String,
}

impl FoundCopy {
    pub fn new(shown: AcquisitionCandidate, pick: PickedCopy) -> Self {
        FoundCopy {
            shown,
            pick: Arc::new(pick),
            id: String::new(),
        }
    }
}

/// A look as it stands, copied out so it never changes under a reader.
#[derive(Debug, Clone, PartialEq)]
pub struct FindSnapshot {
    pub id: String,
    pub state: String,
    pub target: FindTarget,
    pub sources: Vec<FindSource>,
    pub copies: Vec<FoundCopy>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub error: Option<String>,
    /// For a library song, how its file is another take of the song, read once the search ends.
    pub take: Option<OtherTake>,
}

/// What picking a copy did: queued (with the downloads row to follow, when there is one) or
/// skipped, with the words why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickOutcome {
    pub state: String,
    pub detail: String,
    pub key: Option<String>,
    /// For a pick that asks first, the library's copy and the picked one side by side.
    pub compare: Option<PickCompare>,
}

impl PickOutcome {
    pub fn new(state: &str, detail: impl Into<String>) -> Self {
        PickOutcome {
            state: state.to_string(),
            detail: detail.into(),
            key: None,
            compare: None,
        }
    }

    pub fn skipped(detail: impl Into<String>) -> Self {
        Self::new(library_action_states::SKIPPED, detail)
    }
}

/// Whether someone may have a higher quality copy fetched, read now, so an app hears a reason
/// straight away. The upgrade queue's executor checks all of it again when the job runs.
pub struct UpgradeGate;

impl UpgradeGate {
    pub fn refusal(
        settings: Option<&LibraryActionSettings>,
        username: Option<&str>,
        available: bool,
        ready: bool,
        source_name: &str,
    ) -> Option<String> {
        let Some(username) = username.filter(|u| !dotnet::is_blank(u)) else {
            return Some(
                "Sign in with a username to upgrade songs; an API key alone does not say who is asking."
                    .to_string(),
            );
        };
        let settings = match settings {
            Some(settings) if available && settings.enabled => settings,
            _ => return Some("Library actions are off.".to_string()),
        };
        if !settings.is_allowed(Some(username)) {
            return Some(format!("{username} is not on the library actions allowed list."));
        }
        if !settings
            .effective_actions()
            .iter()
            .any(|a| a.action == LibraryAction::BetterQuality && a.enabled)
        {
            return Some("Better quality is not switched on.".to_string());
        }
        if settings.dry_run {
            return Some(
                "Library actions only rehearse while dry run is on, so nothing would change.".to_string(),
            );
        }
        if !ready {
            return Some(format!(
                "Better quality looks for copies on {source_name}, which is not set up on this server."
            ));
        }
        None
    }
}

/// What one source's search found: the copies, the searches run, and a line about it.
pub type SearchAnswer = (Vec<FoundCopy>, Vec<String>, Option<String>);

/// Finds the song a look is for, by its id.
pub type ResolveFn =
    Arc<dyn Fn(String, CancellationToken) -> BoxFuture<'static, Option<FindTarget>> + Send + Sync>;

/// One source's search.
pub type SearchFn = Arc<
    dyn Fn(FindTarget, CancellationToken) -> BoxFuture<'static, anyhow::Result<SearchAnswer>> + Send + Sync,
>;

/// For each source, in order, None when it may be searched, or the words for why not.
pub type SourcesFn = Arc<dyn Fn(&FindTarget) -> Vec<(String, Option<String>)> + Send + Sync>;

/// Fetches a picked copy for a person.
pub type FetchFn = Arc<
    dyn Fn(FindTarget, FoundCopy, String, PickAsk) -> BoxFuture<'static, anyhow::Result<PickOutcome>>
        + Send
        + Sync,
>;

/// What comes with a pick besides the copy: whether the person was asked, and the look's list.
#[derive(Debug, Clone, Default)]
pub struct PickAsk {
    /// None from an app that cannot ask (it hears the old refusal); Some(false) for a first
    /// pick, which may answer [`library_action_states::CONFIRM`]; Some(true) once the person
    /// said to replace their copy anyway.
    pub anyway: Option<bool>,
    /// Every copy the look listed.
    pub listed: Vec<FoundCopy>,
}

/// What take a library song's own file is, by its audio. None when it cannot be told.
pub type TakeFn = Arc<dyn Fn(FindTarget) -> BoxFuture<'static, Option<RecordingKind>> + Send + Sync>;

/// Whether Lidarr is left alone for the song's artist, who has music in it already.
pub type LeftAloneFn = Arc<dyn Fn(FindTarget) -> BoxFuture<'static, bool> + Send + Sync>;

/// The seams, so tests run a look without slskd, Lidarr or Navidrome.
#[derive(Clone)]
pub struct SongFinderSeams {
    pub resolve: ResolveFn,
    pub search_soulseek: SearchFn,
    pub search_lidarr: SearchFn,
    pub sources_for: SourcesFn,
    pub fetch: FetchFn,
    pub take_of: TakeFn,
    /// Whether Lidarr is left alone for the song's artist: an artist with music in Lidarr gets no
    /// Lidarr copy, listed or picked. Read from the user's Lidarr when the services hold what
    /// reads it; never left alone otherwise.
    pub lidarr_left_alone: LeftAloneFn,
}

/// A pick whose file AcoustID did not answer for: it is not kept, and the person can pick again.
pub const ACOUST_ID_BUSY: &str = "AcoustID is busy, try again";
/// A pick whose song MusicBrainz did not answer for.
pub const MUSIC_BRAINZ_BUSY: &str = "MusicBrainz is busy, try again";
/// A lossy copy picked for a lossless file the person said is the wrong version.
pub const LOSSY_OVER_LOSSLESS: &str = "Your copy is lossless and this one is not. Replace it anyway?";
/// Lidarr, for an artist with music in it already: no copy of it is listed or fetched.
pub const LIDARR_LEFT_ALONE: &str = "Lidarr was left alone: this artist already has music in your Lidarr";

/// The services a look uses when it needs them. Each may be missing.
#[derive(Default, Clone)]
pub struct SongFinderServices {
    pub settings: Option<Arc<SettingsStore>>,
    pub upgrade_queue: Option<Arc<UpgradeQueue>>,
    pub upgrade_sources: Option<Arc<UpgradeSources>>,
    /// Whether the library actions executor is there (the gate's "available").
    pub executor: bool,
    pub tracker: Option<Arc<AcquisitionTracker>>,
    pub acquisitions: Option<Arc<TrackAcquisitionQueue>>,
    pub picks: Option<Arc<DownloadPicks>>,
    pub local_library: Option<Arc<dyn ILocalLibraryService>>,
    pub id_registry: Option<Arc<ExternalIdRegistry>>,
    pub resolver: Option<Arc<NavidromeSongPathResolver>>,
    pub soulseek: Option<Arc<SoulseekDownloadService>>,
    pub soulseek_link: Option<Arc<dyn ISoulseekLink>>,
    pub lidarr_fetcher: Option<Arc<LidarrTrackFetcher>>,
    pub lidarr_client: Option<Arc<LidarrClient>>,
    /// What the user's Lidarr holds of an artist, which says when Lidarr is left alone.
    pub lidarr_music: Option<LidarrArtistMusic>,
    /// Family's write gate, once Family's services exist: a family member's pick lands in
    /// their own library. Unset, or with Family off, every pick fills the shared library.
    pub family: std::sync::OnceLock<Arc<LibraryWriteGate>>,
    /// Builds a pick's gate: the recording the song is, which a pick's file is held to by the
    /// import rule. Without it a pick keeps the shipped checks alone.
    pub gates: Option<Arc<dyn HeartGates>>,
    /// The library actions' journal, which says which songs carry the "wrong version asked" mark.
    pub journal: Option<Arc<LibraryActionJournal>>,
}

impl SongFinderServices {
    fn soulseek_settings(&self) -> SoulseekSettings {
        self.settings
            .as_ref()
            .map_or_else(SoulseekSettings::default, |s| s.current().soulseek.clone())
    }

    fn rejected(&self) -> Option<&RejectedPeerRegistry> {
        self.soulseek.as_ref().map(|s| s.rejected_peers().as_ref())
    }

    fn remembers(&self) -> bool {
        self.soulseek.as_ref().is_some_and(|s| s.remembers_rejections())
    }

    /// Whether the library song `library_id` carries the "wrong version asked" mark.
    fn wrong_version_asked(&self, library_id: &str) -> bool {
        self.journal.as_ref().is_some_and(|journal| {
            journal.wrong_version_asked(library_id, LibraryActionExecutor::WRONG_VERSION_FIND_SONGS)
        })
    }

    /// Why this person may not have a library song replaced right now, or None.
    fn upgrade_refusal(&self, user: &str) -> Option<String> {
        let settings = self
            .settings
            .as_ref()
            .map(|s| s.current().library_actions.clone());
        UpgradeGate::refusal(
            settings.as_ref(),
            Some(user),
            self.upgrade_queue.is_some() && self.executor,
            self.upgrade_sources.as_ref().is_some_and(|s| s.ready()),
            &self
                .upgrade_sources
                .as_ref()
                .map_or_else(|| "Soulseek".to_string(), |s| s.name()),
        )
    }
}

struct JobState {
    state: String,
    sources: Vec<FindSource>,
    copies: Vec<FoundCopy>,
    /// The last copy id given out on this look.
    last_copy: i32,
    updated_at: DateTime<Utc>,
    error: Option<String>,
    take: Option<OtherTake>,
    /// What take the library song's own file is, by its audio, once it was asked.
    audio_take: Option<RecordingKind>,
}

struct Job {
    id: String,
    user: String,
    target: FindTarget,
    started_at: DateTime<Utc>,
    state: Mutex<JobState>,
}

impl Job {
    /// The song looked for, with what its own file was found to be.
    fn target_now(&self) -> FindTarget {
        FindTarget {
            audio_take: self.state.lock().audio_take,
            ..self.target.clone()
        }
    }

    fn snapshot(&self) -> FindSnapshot {
        let target = self.target_now();
        let state = self.state.lock();
        FindSnapshot {
            id: self.id.clone(),
            state: state.state.clone(),
            target,
            sources: state.sources.clone(),
            copies: state.copies.clone(),
            started_at: self.started_at,
            updated_at: state.updated_at,
            error: state.error.clone(),
            take: state.take,
        }
    }

    fn same_user(&self, user: &str) -> bool {
        dotnet::eq_ignore_case(&self.user, user.trim())
    }

    fn searching(&self) -> bool {
        self.state.lock().state == find_states::SEARCHING
    }
}

/// Find songs: a person runs a song's search again, by hand, across the download sources they
/// use (Soulseek and Lidarr), sees every copy found with its details, and picks the one to fetch.
///
/// A look runs in the background and is read by its id while it goes, the way the apps already
/// follow downloads. It never downloads anything; a pick does, through the same pipeline and
/// checks as any download, with the picked copy instead of a search. A pick for a song already
/// in the library goes through Better quality, so it replaces a copy with a lossless one, or
/// with one no better only once the person says so (see [`super::replace_anyway`]), and the
/// original stays until the new one passes. Looks are kept in memory for half an hour, then
/// forgotten.
pub struct SongFinder {
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    /// Held while a look is counted and added, so two starts never both take the last place.
    starting: Mutex<()>,
    seams: Mutex<SongFinderSeams>,
    services: Arc<SongFinderServices>,
    clock: Clock,
}

impl SongFinder {
    pub const KEEP: TimeDelta = TimeDelta::minutes(30);
    pub const CAPACITY: usize = 40;
    /// Looks one person may have running at once. Each one asks every peer on Soulseek.
    pub const RUNNING_PER_USER: usize = 2;
    /// Looks running at once across everyone, kept under [`Self::CAPACITY`] so a running look is
    /// never forgotten to make room.
    pub const RUNNING_AT_ONCE: usize = 20;
    pub const MAX_COPIES: usize = 300;
    /// Files passed over that the list keeps for each reason, however many fit.
    pub const SKIPPED_PER_REASON: usize = 5;
    /// Room on the list kept for those, so a popular song's list still says why files were
    /// passed over.
    pub const SKIPPED_ROOM: usize = 60;

    pub const STILL_SEARCHING_TEXT: &'static str =
        "Octo is still searching. Pick a copy once the search ends.";

    /// How long one source may search.
    const SEARCH_LIMIT: Duration = Duration::from_secs(4 * 60);

    /// How long hearing a library song's own file may hold up its search.
    const TAKE_LIMIT: Duration = Duration::from_secs(20);

    pub fn new(services: SongFinderServices) -> Self {
        Self::with_clock(services, Clock::system())
    }

    /// Gives Find songs Family's write gate. Once only.
    pub fn use_family(&self, gate: Arc<LibraryWriteGate>) {
        let _ = self.services.family.set(gate);
    }

    /// The song one of `user`'s looks is for, while the look is kept.
    pub fn look_of(&self, id: &str, user: &str) -> Option<FindTarget> {
        self.jobs
            .lock()
            .get(id)
            .filter(|job| job.same_user(user))
            .map(|job| job.target_now())
    }

    pub fn with_clock(services: SongFinderServices, clock: Clock) -> Self {
        let services = Arc::new(services);
        let seams = SongFinderSeams {
            resolve: {
                let services = services.clone();
                Arc::new(move |id, ct| {
                    let services = services.clone();
                    async move { resolve(&services, &id, &ct).await }.boxed()
                })
            },
            search_soulseek: {
                let services = services.clone();
                Arc::new(move |target, ct| {
                    let services = services.clone();
                    async move { search_soulseek(&services, &target, &ct).await }.boxed()
                })
            },
            search_lidarr: {
                let services = services.clone();
                Arc::new(move |target, ct| {
                    let services = services.clone();
                    async move { search_lidarr(&services, &target, &ct).await }.boxed()
                })
            },
            sources_for: {
                let services = services.clone();
                Arc::new(move |target| default_sources(&services, target))
            },
            fetch: {
                let services = services.clone();
                Arc::new(move |target, copy, user, ask| {
                    let services = services.clone();
                    async move { Ok(fetch(&services, &target, &copy, &user, &ask).await) }.boxed()
                })
            },
            lidarr_left_alone: {
                let services = services.clone();
                Arc::new(move |target| {
                    let services = services.clone();
                    async move { lidarr_left_alone(&services, &target).await }.boxed()
                })
            },
            take_of: {
                let services = services.clone();
                Arc::new(move |target| {
                    let services = services.clone();
                    async move {
                        let (Some(soulseek), Some(path)) = (&services.soulseek, &target.owned_path) else {
                            return None;
                        };
                        soulseek.take_of_file(path, &target.artist, &target.title).await
                    }
                    .boxed()
                })
            },
        };
        SongFinder {
            jobs: Mutex::new(HashMap::new()),
            starting: Mutex::new(()),
            seams: Mutex::new(seams),
            services,
            clock,
        }
    }

    /// Changes a seam (tests use it).
    pub fn seams(&self, change: impl FnOnce(&mut SongFinderSeams)) {
        change(&mut self.seams.lock());
    }

    /// The seams as they stand, for a test that builds a second finder over the same ones.
    pub fn current_seams(&self) -> SongFinderSeams {
        self.seams.lock().clone()
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    // ---------------------------------------------------------------------------------------
    // Looking
    // ---------------------------------------------------------------------------------------

    /// Starts a look for a song by its id, and answers it at once, still searching. None when no
    /// song has this id. A look that may not run answers failed, with the words why.
    pub async fn start(
        self: &Arc<Self>,
        id: &str,
        user: &str,
        ct: &CancellationToken,
    ) -> Option<FindSnapshot> {
        self.prune();
        let resolve = self.seams.lock().resolve.clone();
        let target = resolve(id.to_string(), ct.clone()).await?;
        let now = self.now();
        let mut state = JobState {
            state: find_states::SEARCHING.to_string(),
            sources: Vec::new(),
            copies: Vec::new(),
            last_copy: 0,
            updated_at: now,
            error: None,
            take: None,
            audio_take: None,
        };
        // A library song's look leads only to replacing its file, which only the people on the
        // library actions allowed list may do: they hear why before a search is spent on it.
        let refused = if target.library_id.is_some() {
            self.services.upgrade_refusal(user)
        } else {
            None
        };
        let allowed = if refused.is_none() {
            let sources_for = self.seams.lock().sources_for.clone();
            sources_for(&target)
        } else {
            Vec::new()
        };
        for (name, refusal) in allowed {
            state.sources.push(FindSource {
                name,
                state: if refusal.is_none() {
                    find_states::SEARCHING
                } else {
                    find_states::OFF
                }
                .to_string(),
                text: refusal,
                queries: Vec::new(),
            });
        }
        if let Some(refused) = refused {
            state.state = find_states::FAILED.to_string();
            state.error = Some(refused);
        } else if state.sources.iter().all(|s| s.state == find_states::OFF) {
            state.state = find_states::FAILED.to_string();
            state.error = Some(
                "None of your download sources can search: Soulseek or Lidarr has to be set up and switched on for hearts."
                    .to_string(),
            );
        }
        let id: String = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let job = Arc::new(Job {
            id: id.clone(),
            user: user.to_string(),
            target,
            started_at: now,
            state: Mutex::new(state),
        });
        {
            let _starting = self.starting.lock();
            if job.searching() {
                let running: Vec<Arc<Job>> = self
                    .jobs
                    .lock()
                    .values()
                    .filter(|other| other.searching())
                    .cloned()
                    .collect();
                let busy = if running.iter().filter(|other| other.same_user(user)).count()
                    >= Self::RUNNING_PER_USER
                {
                    Some(format!(
                        "You already have {} searches running. Search again once one of them ends.",
                        Self::RUNNING_PER_USER
                    ))
                } else if running.len() >= Self::RUNNING_AT_ONCE {
                    Some("Octo is running a lot of searches right now. Try again in a minute.".to_string())
                } else {
                    None
                };
                if let Some(busy) = busy {
                    let mut state = job.state.lock();
                    state.state = find_states::FAILED.to_string();
                    state.error = Some(busy);
                    for source in state.sources.iter_mut() {
                        if source.state == find_states::SEARCHING {
                            source.state = find_states::OFF.to_string();
                            source.text = Some("Not searched".to_string());
                        }
                    }
                }
            }
            self.jobs.lock().insert(id, job.clone());
        }
        if job.searching() {
            let finder = self.clone();
            let running = job.clone();
            tokio::spawn(async move { finder.run(running).await });
        }
        Some(job.snapshot())
    }

    /// A look by its id, for the person who started it.
    pub fn get(&self, id: &str, user: &str) -> Option<FindSnapshot> {
        self.prune();
        let job = self.jobs.lock().get(id).cloned()?;
        job.same_user(user).then(|| job.snapshot())
    }

    /// This person's looks, newest first, for the dashboard's progress cards.
    pub fn for_user(&self, user: &str) -> Vec<FindSnapshot> {
        self.prune();
        let mut mine: Vec<FindSnapshot> = self
            .jobs
            .lock()
            .values()
            .filter(|job| job.same_user(user))
            .map(|job| job.snapshot())
            .collect();
        mine.sort_by_key(|found| std::cmp::Reverse(found.started_at));
        mine
    }

    /// Waits for a look to end. Only tests need it.
    pub async fn wait(&self, id: &str, limit: Duration) -> Option<FindSnapshot> {
        let until = std::time::Instant::now() + limit;
        while std::time::Instant::now() < until {
            if let Some(job) = self.jobs.lock().get(id).cloned()
                && !job.searching()
            {
                return Some(job.snapshot());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.jobs.lock().get(id).map(|job| job.snapshot())
    }

    async fn run(&self, job: Arc<Job>) {
        let names: Vec<String> = job
            .state
            .lock()
            .sources
            .iter()
            .filter(|s| s.state == find_states::SEARCHING)
            .map(|s| s.name.clone())
            .collect();
        // A library song's own file is heard first: a live take or an instrumental filed under
        // a plain name is then looked for as the plain song, and its copies judged against it.
        if job.target.library_id.is_some() && job.target.owned_path.is_some() {
            let take_of = self.seams.lock().take_of.clone();
            if let Ok(found) = tokio::time::timeout(Self::TAKE_LIMIT, take_of(job.target.clone())).await {
                job.state.lock().audio_take = found;
            }
        }
        futures::future::join_all(names.iter().map(|name| self.search_one(&job, name))).await;
        let target = job.target_now();
        let take = if target.library_id.is_some() {
            let listed = job.state.lock().copies.clone();
            let settings = self.services.soulseek_settings();
            replace_anyway::library_take(
                &target,
                replace_anyway::CheckContext {
                    settings: &settings,
                    rejected: self.services.rejected(),
                    remembers: self.services.remembers(),
                    listed: &listed,
                },
            )
            .map(|(take, _)| take)
        } else {
            None
        };
        let now = self.now();
        let mut state = job.state.lock();
        state.take = take;
        state.state = if state.sources.iter().any(|s| s.state == find_states::DONE) {
            find_states::DONE
        } else {
            find_states::FAILED
        }
        .to_string();
        if state.state == find_states::FAILED && state.error.is_none() {
            state.error = Some(
                state
                    .sources
                    .iter()
                    .find_map(|s| s.text.clone())
                    .unwrap_or_else(|| "No source could search.".to_string()),
            );
        }
        state.updated_at = now;
    }

    async fn search_one(&self, job: &Job, source: &str) {
        let (search, left_alone) = {
            let seams = self.seams.lock();
            if source == SOULSEEK_SOURCE {
                (seams.search_soulseek.clone(), None)
            } else {
                (seams.search_lidarr.clone(), Some(seams.lidarr_left_alone.clone()))
            }
        };
        // Lidarr is left alone for an artist with music in it: it lists no copy, so nobody picks
        // one its import could put over the artist's own files.
        let answer = if let Some(left_alone) = left_alone
            && left_alone(job.target_now()).await
        {
            Ok((Vec::new(), Vec::new(), Some(LIDARR_LEFT_ALONE.to_string())))
        } else {
            let limit = CancellationToken::new();
            match tokio::time::timeout(Self::SEARCH_LIMIT, search(job.target_now(), limit.clone())).await {
                Ok(answer) => answer,
                Err(_) => {
                    limit.cancel();
                    Err(anyhow::anyhow!("The operation was canceled."))
                }
            }
        };
        let result = match answer {
            Ok((copies, queries, text)) => {
                merge(&mut job.state.lock(), copies);
                FindSource {
                    name: source.to_string(),
                    state: find_states::DONE.to_string(),
                    text,
                    queries,
                }
            }
            Err(e) => {
                info!(
                    "Find songs on {source} for '{} - {}' failed: {e}",
                    job.target.artist, job.target.title
                );
                FindSource {
                    name: source.to_string(),
                    state: find_states::FAILED.to_string(),
                    text: Some(
                        user_safe(Some(&e.to_string())).unwrap_or_else(|| format!("{source} did not answer")),
                    ),
                    queries: Vec::new(),
                }
            }
        };
        let now = self.now();
        let mut state = job.state.lock();
        if let Some(at) = state.sources.iter().position(|s| s.name == source) {
            state.sources[at] = result;
        }
        state.updated_at = now;
    }

    // ---------------------------------------------------------------------------------------
    // Picking
    // ---------------------------------------------------------------------------------------

    /// Fetches exactly the copy with this id on the look's list, for an app that cannot ask.
    pub async fn pick_by_id(&self, id: &str, copy_id: &str, user: &str) -> PickOutcome {
        self.pick_copy(id, copy_id, user, None).await
    }

    /// Fetches exactly the copy with this id on the look's list. `anyway` as [`PickAsk`] has it.
    pub async fn pick_copy(&self, id: &str, copy_id: &str, user: &str, anyway: Option<bool>) -> PickOutcome {
        let copy_id = copy_id.trim().to_string();
        self.pick_with(id, user, anyway, |copies| {
            copies.iter().find(|copy| copy.id == copy_id).cloned()
        })
        .await
    }

    /// Fetches the copy at this place on the look's list, for an app that cannot ask.
    pub async fn pick_at(&self, id: &str, index: i32, user: &str) -> PickOutcome {
        self.pick_place(id, index, user, None).await
    }

    /// Fetches the copy at this place on the look's list. Kept for apps that pick by place; it is
    /// only safe once the search has ended, so a pick while it runs is refused.
    pub async fn pick_place(&self, id: &str, index: i32, user: &str, anyway: Option<bool>) -> PickOutcome {
        self.pick_with(id, user, anyway, |copies| {
            usize::try_from(index).ok().and_then(|at| copies.get(at)).cloned()
        })
        .await
    }

    async fn pick_with(
        &self,
        id: &str,
        user: &str,
        anyway: Option<bool>,
        find: impl FnOnce(&[FoundCopy]) -> Option<FoundCopy>,
    ) -> PickOutcome {
        let Some(job) = self
            .jobs
            .lock()
            .get(id)
            .cloned()
            .filter(|job| job.same_user(user))
        else {
            return PickOutcome::skipped("This search is gone. Search again.");
        };
        let (copy, listed) = {
            let state = job.state.lock();
            // A source that answers later adds copies to the list, so a pick waits for the end.
            if state.state == find_states::SEARCHING {
                return PickOutcome::skipped(Self::STILL_SEARCHING_TEXT);
            }
            (find(&state.copies), state.copies.clone())
        };
        let Some(copy) = copy else {
            return PickOutcome::skipped("That copy is not on the list.");
        };
        // A Lidarr copy listed before its artist gained music in Lidarr is not fetched now; the
        // person's other copies stay on the list.
        if !copy.pick.is_soulseek() {
            let left_alone = self.seams.lock().lidarr_left_alone.clone();
            if left_alone(job.target_now()).await {
                return PickOutcome::skipped(LIDARR_LEFT_ALONE);
            }
        }
        let fetch = self.seams.lock().fetch.clone();
        let ask = PickAsk { anyway, listed };
        match fetch(job.target_now(), copy, user.to_string(), ask).await {
            Ok(outcome) => outcome,
            Err(e) => {
                warn!(
                    "Picking a copy of '{} - {}' failed: {e}",
                    job.target.artist, job.target.title
                );
                PickOutcome::skipped(
                    user_safe(Some(&e.to_string()))
                        .unwrap_or_else(|| "The copy could not be asked for.".to_string()),
                )
            }
        }
    }

    /// Why a picked copy cannot replace a library song, or None when it can: only a lossless
    /// copy replaces a lossy one, which is what Better quality checks again when the file
    /// arrives, and a Soulseek file must pass the download's own name, length and version checks
    /// (see [`soulseek_candidates::pick_refusal`]).
    pub fn replace_refusal(
        target: &FindTarget,
        copy: &FoundCopy,
        settings: Option<&SoulseekSettings>,
        rejected: Option<&RejectedPeerRegistry>,
        remembers: bool,
    ) -> Option<String> {
        if is_lossless(target.owned_format.as_deref()) {
            return Some("Your copy is already lossless, so nothing would be better.".to_string());
        }
        if copy.pick.is_soulseek() && !is_lossless(copy.shown.format.as_deref()) {
            return Some("Only a lossless copy can take the place of the one in your library.".to_string());
        }
        if !copy.pick.is_soulseek()
            && copy.pick.release().is_some()
            && !is_lossless(first_word(copy.shown.format.as_deref()))
        {
            return Some(
                "Only a lossless release can take the place of the one in your library.".to_string(),
            );
        }
        Self::copy_refusal(target, copy, settings, rejected, remembers)
    }

    /// Why a picked Soulseek file may not take a library song's place by the download's own name,
    /// length and version checks, whatever its quality, or None.
    pub fn copy_refusal(
        target: &FindTarget,
        copy: &FoundCopy,
        settings: Option<&SoulseekSettings>,
        rejected: Option<&RejectedPeerRegistry>,
        remembers: bool,
    ) -> Option<String> {
        let defaults = SoulseekSettings::default();
        let hit = copy.pick.to_hit()?;
        let why = soulseek_candidates::pick_refusal(
            &hit,
            judged(target, settings.unwrap_or(&defaults), rejected, remembers),
            true,
        )?;
        Some(format!(
            "{why}, so it cannot take the place of the one in your library."
        ))
    }

    /// Why a picked copy of a song not in the library may not be fetched, or None.
    pub fn pick_refusal(
        target: &FindTarget,
        copy: &FoundCopy,
        rejected: Option<&RejectedPeerRegistry>,
        remembers: bool,
    ) -> Option<String> {
        let defaults = SoulseekSettings::default();
        let hit = copy.pick.to_hit()?;
        let why =
            soulseek_candidates::pick_refusal(&hit, judged(target, &defaults, rejected, remembers), false)?;
        Some(format!("{why}, so it is not fetched again."))
    }

    // ---------------------------------------------------------------------------------------
    // The Soulseek list
    // ---------------------------------------------------------------------------------------

    /// "998 files from 490 peers; 40 fit the song", counting the same files the list marks as
    /// Octo's choices or "Fits too". When the list has no room for all of them, it says how many
    /// it shows: "306 fit the song; the best 240 are listed". The whole count stays, since it is
    /// what Soulseek really has.
    pub fn soulseek_summary(hits: &[SoulseekFileHit], fit: usize, listed: usize) -> String {
        if hits.is_empty() {
            return "Nothing found".to_string();
        }
        let peers = crate::services::soulseek::soulseek_download_service::distinct_peers(hits);
        let text = format!(
            "{} files from {peers} {}; {fit} {} the song",
            hits.len(),
            if peers == 1 { "peer" } else { "peers" },
            if fit == 1 { "fits" } else { "fit" }
        );
        if listed < fit {
            format!("{text}; the best {listed} are listed")
        } else {
            text
        }
    }

    /// Soulseek's files as the list shows them: the ones the download would try first, in its
    /// order, then every other file, the likeliest first, each with the reason it was passed
    /// over.
    pub fn soulseek_copies(
        hits: &[SoulseekFileHit],
        ranked: &[SoulseekFileHit],
        target: &FindTarget,
        settings: &SoulseekSettings,
        rejected: Option<&RejectedPeerRegistry>,
        remembers: bool,
    ) -> Vec<FoundCopy> {
        Self::soulseek_found(hits, ranked, target, settings, rejected, remembers).0
    }

    /// The list, how many files fit the song, and how many of those are on it: every check the
    /// download makes passed, which is exactly the rows with no reason against them (Octo's
    /// choices and "Fits too"). Past [`Self::MAX_COPIES`], a few files of each reason a file was
    /// passed over stay on the list, and the fitting copies, best first, fill the rest.
    pub fn soulseek_found(
        hits: &[SoulseekFileHit],
        ranked: &[SoulseekFileHit],
        target: &FindTarget,
        settings: &SoulseekSettings,
        rejected: Option<&RejectedPeerRegistry>,
        remembers: bool,
    ) -> (Vec<FoundCopy>, usize, usize) {
        let mut order: HashMap<(&str, &str), i32> = HashMap::new();
        for (i, hit) in ranked.iter().enumerate() {
            // ToDictionary refused a second copy of a key; the ranking never repeats one.
            order
                .entry((hit.username.as_str(), hit.filename.as_str()))
                .or_insert(i as i32 + 1);
        }
        let song = judged(target, settings, rejected, remembers);
        struct Row<'a> {
            hit: &'a SoulseekFileHit,
            rank: Option<i32>,
            note: Option<String>,
            tier: i32,
        }
        let mut rows: Vec<Row> = hits
            .iter()
            .filter(|hit| !hit.username.is_empty() && !hit.filename.is_empty())
            .map(|hit| {
                let rank = order
                    .get(&(hit.username.as_str(), hit.filename.as_str()))
                    .copied();
                let (why, tier) = if rank.is_some() {
                    (None, 0)
                } else {
                    soulseek_candidates::judge(hit, song)
                };
                let note = if rank.is_none() {
                    Some(why.unwrap_or_else(|| "Fits too; a download tries the first few".to_string()))
                } else {
                    None
                };
                Row {
                    hit,
                    rank,
                    note,
                    tier,
                }
            })
            .collect();
        // A stable sort: rows that tie keep their order.
        rows.sort_by_key(|row| {
            (
                row.rank.unwrap_or(i32::MAX),
                row.tier,
                std::cmp::Reverse(soulseek_candidates::is_lossless(row.hit)),
                std::cmp::Reverse(row.hit.has_free_upload_slot == Some(true)),
                row.hit.queue_length.unwrap_or(i32::MAX),
                std::cmp::Reverse(row.hit.size),
            )
        });
        let fit = rows.iter().filter(|row| row.tier == 0).count();

        let mut kept: HashSet<usize> = HashSet::new();
        if rows.len() > Self::MAX_COPIES {
            // Grouped by note in order of first appearance, each group's first few, then the
            // room.
            let mut reasons: Vec<(Option<String>, Vec<usize>)> = Vec::new();
            for (at, row) in rows.iter().enumerate().filter(|(_, row)| row.tier > 0) {
                match reasons.iter_mut().find(|(note, _)| *note == row.note) {
                    Some((_, ats)) => ats.push(at),
                    None => reasons.push((row.note.clone(), vec![at])),
                }
            }
            for at in reasons
                .iter()
                .flat_map(|(_, ats)| ats.iter().take(Self::SKIPPED_PER_REASON))
                .take(Self::SKIPPED_ROOM)
            {
                kept.insert(*at);
            }
        }
        let mut at = 0;
        while at < rows.len() && kept.len() < Self::MAX_COPIES {
            kept.insert(at);
            at += 1;
        }

        let listed = rows
            .iter()
            .enumerate()
            .filter(|(at, row)| row.tier == 0 && kept.contains(at))
            .count();
        let copies = rows
            .iter()
            .enumerate()
            .filter(|(at, _)| kept.contains(at))
            .map(|(_, row)| {
                FoundCopy::new(
                    soulseek_candidates::of(row.hit, row.rank, row.note.as_deref()),
                    PickedCopy {
                        source: SOULSEEK_SOURCE.to_string(),
                        peer: Some(row.hit.username.clone()),
                        file: Some(row.hit.filename.clone()),
                        size: row.hit.size,
                        format: Some(soulseek_candidates::extension(row.hit)),
                        bit_rate: row.hit.bit_rate,
                        bit_depth: row.hit.bit_depth,
                        sample_rate: row.hit.sample_rate,
                        length: row.hit.length,
                        ..Default::default()
                    },
                )
            })
            .collect();
        (copies, fit, listed)
    }

    /// Lidarr's releases as the list shows them, Lidarr's own order kept. A release it would take
    /// on its own is ranked after Lidarr's choice; one it rejects says why.
    pub fn lidarr_copies(
        releases: &[LidarrRelease],
        target: &FindTarget,
        album_title: &str,
    ) -> Vec<FoundCopy> {
        let mut rank = 1;
        releases
            .iter()
            .take(Self::MAX_COPIES)
            .map(|release| {
                let note = if release.rejected {
                    release
                        .rejections
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "Lidarr would not take this release on its own".to_string())
                } else {
                    [
                        release.protocol.clone(),
                        release
                            .seeders
                            .map(|s| format!("{s} {}", if s == 1 { "seeder" } else { "seeders" })),
                        release
                            .age_days
                            .map(|d| format!("{d} {} old", if d == 1 { "day" } else { "days" })),
                    ]
                    .into_iter()
                    .flatten()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ")
                };
                let shown_rank = if release.rejected {
                    None
                } else {
                    rank += 1;
                    Some(rank)
                };
                FoundCopy::new(
                    AcquisitionCandidate {
                        source: LIDARR_SOURCE.to_string(),
                        peer: release.indexer.clone(),
                        file: Some(release.title.clone()),
                        format: release.quality.clone(),
                        size: release.size,
                        rank: shown_rank,
                        note: Some(note),
                        title: Some(target.title.clone()),
                        album: Some(album_title.to_string()),
                        ..Default::default()
                    },
                    PickedCopy {
                        source: LIDARR_SOURCE.to_string(),
                        release_guid: Some(release.guid.clone()),
                        indexer_id: Some(release.indexer_id),
                        release_title: Some(release.title.clone()),
                        size: release.size.unwrap_or(0),
                        ..Default::default()
                    },
                )
            })
            .collect()
    }

    fn prune(&self) {
        let now = self.now();
        let mut jobs = self.jobs.lock();
        jobs.retain(|_, job| {
            let state = job.state.lock();
            !(now - state.updated_at >= Self::KEEP && state.state != find_states::SEARCHING)
        });
        if jobs.len() < Self::CAPACITY {
            return;
        }
        // The oldest finished looks make room; a running look is someone's, and is never dropped.
        let mut finished: Vec<(DateTime<Utc>, String)> = jobs
            .iter()
            .filter(|(_, job)| !job.searching())
            .map(|(id, job)| (job.state.lock().updated_at, id.clone()))
            .collect();
        finished.sort_by_key(|(at, _)| *at);
        let excess = jobs.len() - Self::CAPACITY + 1;
        for (_, id) in finished.into_iter().take(excess) {
            jobs.remove(&id);
        }
    }
}

/// Where a copy goes on the list: each source's own first choices, then the rest, Soulseek
/// before Lidarr where they tie.
fn place(copy: &FoundCopy) -> (i32, i32) {
    (
        copy.shown.rank.unwrap_or(i32::MAX),
        if copy.shown.source == SOULSEEK_SOURCE {
            0
        } else {
            1
        },
    )
}

/// Adds one source's copies to the list, each with an id of its own, in their place. The copies
/// already listed keep their order and their ids: a source that answers later only adds rows
/// between them, so nothing a person is looking at moves past another. Caller holds the job's
/// lock. Each source caps its own list.
fn merge(job: &mut JobState, mut copies: Vec<FoundCopy>) {
    // A stable sort by place.
    copies.sort_by_key(place);
    for mut copy in copies {
        job.last_copy += 1;
        copy.id = format!("c{}", job.last_copy);
        let at = job
            .copies
            .iter()
            .rposition(|listed| place(listed) <= place(&copy))
            .map_or(0, |last| last + 1);
        job.copies.insert(at, copy);
    }
}

fn judged<'a>(
    target: &'a FindTarget,
    settings: &'a SoulseekSettings,
    rejected: Option<&'a RejectedPeerRegistry>,
    remembers: bool,
) -> Judged<'a> {
    // A library file that is another take than the studio one is replaced by the plain song,
    // whose album and length are not that take's.
    let another_take = target.is_another_take();
    Judged {
        title: &target.title,
        album: if another_take {
            None
        } else {
            target.album.as_deref()
        },
        duration: if another_take { None } else { target.duration },
        settings,
        rejected,
        remembers_rejections: remembers,
        artist: Some(&target.artist),
    }
}

const LOSSLESS_FORMATS: [&str; 7] = ["flac", "wav", "alac", "ape", "aiff", "aif", "wv"];

pub fn is_lossless(format: Option<&str>) -> bool {
    format.is_some_and(|kind| {
        let kind = kind.trim().trim_start_matches('.');
        LOSSLESS_FORMATS
            .iter()
            .any(|lossless| dotnet::eq_ignore_case(lossless, kind))
    })
}

// Lidarr names qualities "FLAC", "FLAC 24bit", "MP3-320".
fn first_word(quality: Option<&str>) -> Option<&str> {
    quality?.split([' ', '-']).find(|part| !part.is_empty())
}

// ---------------------------------------------------------------------------------------
// The defaults behind the seams
// ---------------------------------------------------------------------------------------

/// Whether a listed copy is lossless: a Soulseek file by its format, a Lidarr release by its
/// quality; Lidarr's own choice searches for the profile's best and counts as lossless.
fn copy_is_lossless(copy: &FoundCopy) -> bool {
    if copy.pick.is_soulseek() {
        is_lossless(copy.shown.format.as_deref())
    } else {
        copy.pick.release().is_none() || is_lossless(first_word(copy.shown.format.as_deref()))
    }
}

async fn fetch(
    services: &SongFinderServices,
    target: &FindTarget,
    copy: &FoundCopy,
    user: &str,
    ask: &PickAsk,
) -> PickOutcome {
    if let Some(library_id) = &target.library_id {
        let settings = services.soulseek_settings();
        let mut pick = (*copy.pick).clone();
        let mut asked: Option<String> = None;
        let marked = services.wrong_version_asked(library_id);
        match ask.anyway {
            // The person said their copy is the wrong version, and Octo could not tell it apart
            // by sound: the copy they pick replaces it as Wrong version does, whatever its quality,
            // asked first when it would trade a lossless copy for a lossy one.
            _ if marked => {
                if let Some(refused) = SongFinder::copy_refusal(
                    target,
                    copy,
                    Some(&settings),
                    services.rejected(),
                    services.remembers(),
                ) {
                    return PickOutcome::skipped(refused);
                }
                if is_lossless(target.owned_format.as_deref()) && !copy_is_lossless(copy) {
                    match ask.anyway {
                        None => return PickOutcome::skipped(LOSSY_OVER_LOSSLESS),
                        Some(false) => {
                            if let Some(gate) = services.upgrade_refusal(user) {
                                return PickOutcome::skipped(gate);
                            }
                            return PickOutcome {
                                compare: Some(replace_anyway::compare(target, copy, None)),
                                ..PickOutcome::new(library_action_states::CONFIRM, LOSSY_OVER_LOSSLESS)
                            };
                        }
                        Some(true) => {}
                    }
                }
                pick.wrong_version = true;
            }
            None => {
                if let Some(refused) = SongFinder::replace_refusal(
                    target,
                    copy,
                    Some(&settings),
                    services.rejected(),
                    services.remembers(),
                ) {
                    return PickOutcome::skipped(refused);
                }
            }
            Some(anyway) => {
                let cx = replace_anyway::CheckContext {
                    settings: &settings,
                    rejected: services.rejected(),
                    remembers: services.remembers(),
                    listed: &ask.listed,
                };
                match replace_anyway::check(target, copy, cx) {
                    ReplaceCheck::Fits => {
                        // Asked first: nothing is fetched until the person says yes.
                        if !anyway {
                            if let Some(gate) = services.upgrade_refusal(user) {
                                return PickOutcome::skipped(gate);
                            }
                            return PickOutcome {
                                compare: Some(replace_anyway::compare(target, copy, None)),
                                ..PickOutcome::new(
                                    library_action_states::READY,
                                    "This copy is better than yours. Replace yours with it?",
                                )
                            };
                        }
                    }
                    ReplaceCheck::Refused(refused) => return PickOutcome::skipped(refused),
                    ReplaceCheck::Confirm { words, song, take } => {
                        // Someone who may not replace it hears that, never a question first.
                        if let Some(gate) = services.upgrade_refusal(user) {
                            return PickOutcome::skipped(gate);
                        }
                        if !anyway {
                            return PickOutcome {
                                compare: Some(replace_anyway::compare(target, copy, take)),
                                ..PickOutcome::new(library_action_states::CONFIRM, words)
                            };
                        }
                        pick.anyway = Some(song);
                        asked = Some(words);
                    }
                }
            }
        }
        if let Some(gate) = services.upgrade_refusal(user) {
            return PickOutcome::skipped(gate);
        }
        let Some(queue) = &services.upgrade_queue else {
            return PickOutcome::skipped("Library actions are off.");
        };
        if let (Some(words), Some(song)) = (&asked, &pick.anyway) {
            info!(
                "Find songs: {user} said to replace '{} - {}' anyway with {} ({words}); it is held to '{}'{}{} and to no quality check",
                target.artist,
                target.title,
                pick.describe(),
                song.title,
                song.album
                    .as_deref()
                    .map_or_else(String::new, |album| format!(" on '{album}'")),
                song.length
                    .map_or_else(String::new, |length| format!(", {length} s long")),
            );
        }
        let copy = &FoundCopy {
            pick: Arc::new(pick),
            ..copy.clone()
        };
        let (jobs, full) = queue.add(
            vec![UpgradeAsk {
                navidrome_id: library_id.clone(),
                title: Some(target.title.clone()),
                artist: Some(target.artist.clone()),
                album: target.album.clone(),
                suffix: target.owned_format.clone(),
                attempt_key: None,
                pick: Some((*copy.pick).clone()),
            }],
            user,
            "find",
        );
        let Some(job) = jobs.first() else {
            return PickOutcome::skipped(full.unwrap_or_else(|| "The upgrade queue is full.".to_string()));
        };
        if !copy.pick.same_as(job.pick.as_ref()) {
            // The song already waits in the queue. Picking again before it starts means the
            // person changed their mind, so the newer pick takes the old one's place. Once it
            // runs, or when it is someone else's, the copy it fetches is settled.
            if queue
                .replace_pick(library_id, user, (*copy.pick).clone())
                .is_none()
            {
                return PickOutcome::skipped(if job.pick.is_none() {
                    "A higher quality copy of this song is already being looked for. Pick again once it ends."
                } else {
                    "Another copy of this song is already on its way. Pick again once it ends."
                });
            }
            info!(
                "Find songs: {user} changed the pick for '{} - {}' to {}",
                target.artist,
                target.title,
                copy.pick.describe()
            );
            return PickOutcome::new(
                library_action_states::QUEUED,
                "Changed to the copy you picked. Your copy stays until the new one passes every check.",
            );
        }
        info!(
            "Find songs: {user} picked {} to replace '{} - {}'",
            copy.pick.describe(),
            target.artist,
            target.title
        );
        return PickOutcome::new(
            library_action_states::QUEUED,
            "Getting the copy you picked. Your copy stays until the new one passes every check.",
        );
    }

    if let Some(denied) = SongFinder::pick_refusal(target, copy, services.rejected(), services.remembers()) {
        return PickOutcome::skipped(denied);
    }
    let external_id = target.external_id.clone().unwrap_or_default();
    let provider = SoulseekMetadataService::PROVIDER_NAME;
    let tracker = services.tracker.clone();
    let key = key_of(provider, &external_id);
    if tracker.as_ref().is_some_and(|t| t.is_running(&key)) {
        return PickOutcome::skipped("This song is downloading right now. Pick again once it ends.");
    }
    let (Some(acquisitions), Some(picks)) = (&services.acquisitions, &services.picks) else {
        return PickOutcome::skipped("The copy could not be asked for.");
    };
    // The recording the song is, which the pick's file is held to by the import rule, asked now
    // at the person's pace. A pick is never held: with MusicBrainz not answering, the person
    // hears so and picks again.
    let gates = match &services.gates {
        Some(builder) => {
            let asker = Holder {
                user: user.to_string(),
                ..Default::default()
            };
            match builder
                .build(provider, &external_id, &asker, AcquisitionOrigin::SongFinderPick)
                .await
            {
                GateStart::Ready(gate) => vec![gate],
                GateStart::Hold {
                    waiting_for: WaitingFor::MusicBrainz,
                    ..
                } => return PickOutcome::skipped(MUSIC_BRAINZ_BUSY),
                // Nothing else of the catalog is needed to fetch it: the shipped checks decide.
                GateStart::Hold { .. } => Vec::new(),
            }
        }
        None => Vec::new(),
    };
    picks.pin(&external_id, copy.pick.clone());
    if let Some(tracker) = &tracker {
        tracker.begin_kind(
            provider,
            &external_id,
            Some(&external_id),
            Some(user),
            Some(&target.artist),
            Some(&target.title),
            target.album.as_deref(),
            acquisition_kinds::PICK,
        );
        tracker.stage(
            provider,
            &external_id,
            AcquisitionState::Queued,
            Some(&copy.pick.source),
            Some(&format!("Getting {}", copy.pick.describe())),
        );
    }
    info!(
        "Find songs: {user} picked {} for '{} - {}'",
        copy.pick.describe(),
        target.artist,
        target.title
    );
    // A family member's pick lands in their own library.
    let destination = services
        .family
        .get()
        .and_then(|gate| gate.decide(Some(user)).target())
        .unwrap_or(AcquisitionTarget::Shared);
    let download = acquisitions.enqueue_as(
        Some(AcquisitionOrigin::SongFinderPick),
        gates,
        &destination,
        false,
        provider,
        &external_id,
        true,
        false,
        true,
        Some(if copy.pick.is_soulseek() {
            DownloadSource::Soulseek
        } else {
            DownloadSource::Lidarr
        }),
        true,
        Some(user),
        false,
        None,
    );
    let (picks, pick, waited_id) = (picks.clone(), copy.pick.clone(), external_id.clone());
    tokio::spawn(async move {
        if let Err(e) = download.wait().await {
            // A pick is never held: a file its check could not judge now is thrown away, and its
            // row ends with the words the check gave.
            let reason = match e.downcast_ref::<AcquisitionHeld>() {
                Some(held) => {
                    for file in &held.files {
                        let _ = std::fs::remove_file(file);
                    }
                    held.line.clone()
                }
                None => e.root_cause().to_string(),
            };
            if let Some(tracker) = &tracker {
                tracker.fail(provider, &waited_id, Some(&reason));
            }
        }
        // However it ended, the pick was for this download and no later one.
        picks.forget(&waited_id, &pick);
    });
    PickOutcome {
        key: Some(key),
        ..PickOutcome::new(
            library_action_states::QUEUED,
            format!("Getting {}.", copy.pick.describe()),
        )
    }
}

async fn resolve(services: &SongFinderServices, id: &str, ct: &CancellationToken) -> Option<FindTarget> {
    let mut raw = id.trim();
    if dotnet::starts_with_ignore_case(raw, "find:") {
        raw = &raw[5..];
    }
    let provider = SoulseekMetadataService::PROVIDER_NAME;
    let prefix = format!("{provider}:");
    if dotnet::starts_with_ignore_case(raw, &prefix) {
        raw = &raw[prefix.len()..];
    }
    let library = services.local_library.as_ref()?;
    let mut raw = raw.to_string();
    if raw.starts_with("ext-")
        && let (true, _, Some(outside)) = library.parse_song_id(&raw)
    {
        raw = outside;
    }
    if raw.is_empty() {
        return None;
    }

    let routing = match services.id_registry.as_ref() {
        Some(registry) => registry.resolve_song(&raw, false).map(|shared| shared.snapshot()),
        None => SoulseekMetadataService::try_decode_external_id(Some(&raw)),
    };
    let resolver = services.resolver.as_ref();
    if let Some(routing) = routing.filter(|r| r.kind == RoutingKind::Song && r.has_artist_title()) {
        let artist = routing.artist.clone().unwrap_or_default();
        let title = routing.title.clone().unwrap_or_default();
        // Downloaded since it was found: the library's copy is the one a pick would replace.
        if let Some(local) = library.get_local_path_for_external_song(provider, &raw).await
            && std::path::Path::new(&local).is_file()
            && let Some(resolver) = resolver
            && let Some(library_id) = resolver.find_id_by_path(&artist, &title, &local).await
            && let Some(owned) = library_song(resolver, &library_id, ct).await
        {
            return Some(owned);
        }
        return Some(FindTarget {
            external_id: Some(raw.clone()),
            ..FindTarget::new(artist, title, routing.album.as_deref(), routing.duration, raw)
        });
    }
    library_song(resolver?, &raw, ct).await
}

async fn library_song(
    resolver: &NavidromeSongPathResolver,
    id: &str,
    _ct: &CancellationToken,
) -> Option<FindTarget> {
    let song = resolver.resolve(id).await?;
    let (quality, _) = AudioSummary::describe(Some(&song.absolute_path));
    Some(FindTarget {
        library_id: Some(song.navidrome_id.clone()),
        owned_format: Some(song.suffix.clone()),
        owned_quality: quality.or_else(|| Some(dotnet::to_upper_invariant(&song.suffix))),
        owned_size: (song.size_bytes > 0).then_some(song.size_bytes),
        owned_path: Some(song.absolute_path.clone()),
        ..FindTarget::new(
            song.artist.clone(),
            song.title.clone(),
            (!dotnet::is_blank(&song.album)).then_some(song.album.as_str()),
            song.duration_seconds,
            song.navidrome_id.clone(),
        )
    })
}

/// Each source, and why it may not be searched. An outside song uses the sources hearts download
/// from; a library song the sources Better quality uses.
fn default_sources(services: &SongFinderServices, target: &FindTarget) -> Vec<(String, Option<String>)> {
    let current = services.settings.as_ref().map(|s| s.current());
    let soulseek_up = current
        .as_ref()
        .is_some_and(|c| UpgradeSources::soulseek_set_up(&c.soulseek));
    let lidarr_up = current
        .as_ref()
        .is_some_and(|c| UpgradeSources::lidarr_set_up(&c.lidarr));
    let (soulseek_used, lidarr_used) = if target.library_id.is_some() {
        let wanted = services
            .upgrade_sources
            .as_ref()
            .map_or_else(|| vec![DownloadSource::Soulseek], |s| s.wanted());
        (
            wanted.contains(&DownloadSource::Soulseek),
            wanted.contains(&DownloadSource::Lidarr),
        )
    } else {
        let steps = current
            .as_ref()
            .map(|c| c.subsonic.effective_heart_download_sources())
            .unwrap_or_default();
        (
            steps
                .iter()
                .any(|step| step.source == HeartDownloadSource::Soulseek && step.song_enabled == Some(true)),
            steps
                .iter()
                .any(|step| step.source == HeartDownloadSource::Lidarr && step.song_enabled == Some(true)),
        )
    };
    vec![
        (
            SOULSEEK_SOURCE.to_string(),
            if !soulseek_up {
                Some("Soulseek is not set up on this server".to_string())
            } else if !soulseek_used {
                Some("Soulseek is not one of your download sources".to_string())
            } else {
                None
            },
        ),
        (
            LIDARR_SOURCE.to_string(),
            if !lidarr_up {
                Some("Lidarr is not set up on this server".to_string())
            } else if !lidarr_used {
                Some("Lidarr is not one of your download sources".to_string())
            } else {
                None
            },
        ),
    ]
}

async fn search_soulseek(
    services: &SongFinderServices,
    target: &FindTarget,
    ct: &CancellationToken,
) -> anyhow::Result<SearchAnswer> {
    let Some(soulseek) = &services.soulseek else {
        anyhow::bail!("Soulseek is not available on this server.");
    };
    if let Some(link) = &services.soulseek_link
        && link
            .read(false)
            .await
            .is_some_and(|reading| reading.link == SoulseekLinkState::NotLoggedIn)
    {
        anyhow::bail!("{OFFLINE_TEXT}");
    }

    let found = soulseek
        .find_copies(
            &target.artist,
            &target.title,
            target.album.as_deref(),
            target.duration,
            ct,
        )
        .await?;
    let settings = services.soulseek_settings();
    let (copies, fit, listed) = SongFinder::soulseek_found(
        &found.hits,
        &found.ranked,
        target,
        &settings,
        services.rejected(),
        services.remembers(),
    );
    let text = SongFinder::soulseek_summary(&found.hits, fit, listed);
    Ok((copies, found.queries, Some(text)))
}

/// What Lidarr is asked for the song: the album a fetch finds it on.
fn lidarr_request(target: &FindTarget) -> LidarrTrackRequest {
    LidarrTrackRequest {
        artist: target.artist.clone(),
        title: target.title.clone(),
        album: target.album.clone(),
        duration_seconds: target.duration,
        lossless_only: target.library_id.is_some(),
        original_path: target.owned_path.clone(),
        release: None,
        step: None,
    }
}

/// Whether Lidarr is left alone for the song: the album Lidarr would search for it belongs to an
/// artist that already has music in the user's Lidarr (a track file, or audio in its folder).
/// False when Lidarr cannot be asked, or knows no album with the song; the search says so then.
async fn lidarr_left_alone(services: &SongFinderServices, target: &FindTarget) -> bool {
    let (Some(fetcher), Some(music)) = (&services.lidarr_fetcher, &services.lidarr_music) else {
        return false;
    };
    let album = match fetcher
        .find_album(&lidarr_request(target), &CancellationToken::new())
        .await
    {
        Ok(Some(album)) => album,
        Ok(None) => return false,
        Err(e) => {
            warn!(
                "Could not find Lidarr's album of '{} - {}': {e}",
                target.artist, target.title
            );
            return false;
        }
    };
    let Some(artist) = album
        .resource
        .get("artist")
        .and_then(|artist| artist.get("foreignArtistId"))
        .and_then(|id| id.as_str())
        .filter(|id| !id.trim().is_empty())
    else {
        return false;
    };
    match music.read(artist).await {
        Ok(read) => read.has_music,
        Err(e) => {
            warn!("Could not read what Lidarr holds of artist {artist}: {e}");
            false
        }
    }
}

async fn search_lidarr(
    services: &SongFinderServices,
    target: &FindTarget,
    ct: &CancellationToken,
) -> anyhow::Result<SearchAnswer> {
    let (Some(fetcher), Some(client)) = (&services.lidarr_fetcher, &services.lidarr_client) else {
        anyhow::bail!("Lidarr is not available on this server.");
    };
    let request = lidarr_request(target);
    let Some(album) = fetcher.find_album(&request, ct).await? else {
        return Ok((
            Vec::new(),
            Vec::new(),
            Some("Lidarr knows no album with this song on it".to_string()),
        ));
    };

    let record = match album.year.filter(|y| *y > 0) {
        Some(year) => format!("{} - {} ({year})", album.artist, album.title),
        None => format!("{} - {}", album.artist, album.title),
    };
    let mut copies = vec![FoundCopy::new(
        AcquisitionCandidate {
            source: LIDARR_SOURCE.to_string(),
            file: Some(record.clone()),
            rank: Some(1),
            title: Some(target.title.clone()),
            album: Some(album.title.clone()),
            note: Some(
                "Lidarr searches the album and takes the release your quality profile prefers".to_string(),
            ),
            ..Default::default()
        },
        PickedCopy {
            source: LIDARR_SOURCE.to_string(),
            ..Default::default()
        },
    )];
    let Some(existing) = client.find_album(&album.foreign_album_id).await? else {
        return Ok((
            copies,
            vec![record],
            Some(
                "This album is not in Lidarr yet, so its releases cannot be listed; Lidarr's choice still works"
                    .to_string(),
            ),
        ));
    };
    let releases = client.list_releases(existing.id).await?;
    copies.extend(SongFinder::lidarr_copies(&releases, target, &album.title));
    let text = if releases.is_empty() {
        "The indexers offered no release of this album".to_string()
    } else {
        format!(
            "{} {} of {}",
            releases.len(),
            if releases.len() == 1 {
                "release"
            } else {
                "releases"
            },
            album.title
        )
    };
    Ok((copies, vec![record], Some(text)))
}

// Tested in `download_log_tests.rs` and `find_songs_pick_tests.rs`.
