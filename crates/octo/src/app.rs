//! The application's shared state: one `Arc` per service, built once at startup and shared by
//! every handler and worker.
//!
//! # Adding a service
//!
//! 1. Add a field to [`AppInner`], holding the service as `Arc<YourService>` (or a type that is
//!    already a cheap handle). Name it after the service type in snake case
//!    (`SubsonicProxyService` → `subsonic_proxy`), and put a `///` line on it saying what it
//!    is for.
//! 2. Construct it in [`AppState::build`] (production) **and** in [`AppState::for_tests`]
//!    (handler tests), in dependency order: a service is built after everything it holds.
//!    Every service is built eagerly here, which is fine unless its constructor does I/O.
//! 3. A service that also runs in the background is one `Arc` field plus a worker: register its
//!    loop with `state.workers.register("Name", ...)` in [`AppState::build`], capturing a clone
//!    of the `Arc`. Never start work from a constructor.
//! 4. Settings: services read `state.settings.current()` at the point of use (they follow
//!    changes live), or copy a value out in their constructor and say so (it applies on
//!    restart).
//!
//! Handlers take `State(state): State<AppState>` and reach services as `state.settings`,
//! `state.restart_tracker`, ... through [`Deref`](std::ops::Deref).

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use octo_core::common::Clock;
use octo_core::places::PlaceIndex;
use octo_core::settings::{AppSettings, RestartTracker, SettingsFileWriter, SettingsStore};
use octo_core::tagging::{CatalogLookup, FingerprintVerifier, ReleaseIdentifier, ReleaseLookup, TagPreview};
use octo_media::audio::{AudioFingerprinter, ILoudnessMeter, LoudnessMeter, SpectrumAnalyzer};
use octo_media::tags::TagWriterExtras;
use octo_subsonic::SubsonicResponseBuilder;
use tokio_util::sync::CancellationToken;

use crate::services::admin::{
    AdminRecoveryCode, AdminRoleCheck, BrowseSessionStore, DirectoryBrowser, SignInThrottle,
};
use crate::services::common::{
    AcquisitionActivity, AcquisitionTracker, AcquisitionWorker, CacheCleanupService, CatalogBlanks,
    DownloadArrivals, DownloadConcurrency, DownloadCore, DownloadLogKeeper, DownloadPicks, DownloadServices,
    ExternalSearchService, FamilyHooks, HeartAcquisitionCoordinator, HeartCoordinatorExtras, MeterPreview,
    SongFinder, SongFinderServices, SoulseekHoldResumer, SoulseekHoldStore, StarNavidrome, StarOnArrival,
    TrackAcquisitionQueue, TrackerServices,
};
use crate::services::cover_art::{
    AlbumCoverFinder, CoverArtAggregator, CoverArtArchiveLookup, CoverUpgradeJournal, CoverUpgradeServices,
    CoverUpgradeStore, CoverUpgradeWorker, DeezerCoverArtLookup, DownloadCoverResolver, IAlbumCoverFinder,
    ICoverArtSource, ITunesCoverArtLookup, LastFmCoverArtLookup, LocalCoverCache,
};
use octo_media::cover::CoverArtService;

use crate::services::charts::{AppleMusicPages, ChartCountryStore, ChartsWarmWorker, GenreNames};
use crate::services::common::IAcquisitionActivity;
use crate::services::common::TopSongsService;
use crate::services::cover_art::{AnimatedArt, ArtworkHints, HintedArtwork};
use crate::services::family::LibraryWriteGate as FamilyWriteGate;
use crate::services::family::request_service::{
    FamilyEvents, LiveRequestCatalog, LiveRequestFetcher, RequestParts, RequestService,
};
use crate::services::family::{
    AnswerRules, AudioGate, DeviceRegistry, FamilyPlacement, FamilyPlaylistStore, FamilyPool,
    FamilyPoolStore, FamilyStore, FamilyVault, LibraryIndex, MemberBridge, MemberCopies, MemberCredentials,
    NavidromeAdmin, NavidromeMemberStars, PersonalLibrary, PlayerCaps, PoolParts, RequestStore, SavedStore,
    ScopeResolver, family_service::FamilyService,
};
use crate::services::fingerprint::preview_check::PreviewSource;
use crate::services::fingerprint::{
    AcoustIdClient, AcoustIdRateLimitHandler, AcoustIdRateLimiter, DownloadVerificationService,
    MusicBrainzClient,
};
use crate::services::health::keep_marks::{KeepMarks, KeepMarksParts};
use crate::services::health::library_health_service::HealthDuplicateFixer;
use crate::services::health::member_marks::NavidromeMemberMarks;
use crate::services::health::{HealthServices, LibraryHealthService};
use crate::services::http_client_factory;
use crate::services::i_download_service::IDownloadService;
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::last_fm::{
    FfmpegLastFmRadioAudioTranscoder, LastFmRadioRecommendationService, LastFmRadioRefreshQueue,
    LastFmRadioRefreshWorker, LastFmRadioStateStore, LastFmRadioStreamService, LastFmRadioStreamServiceParts,
    LastFmRadioStreamSessionStore, LastFmRadioTrackCache, LastFmRadioWarmupService, LastFmScrobbleService,
    LastFmService, RandomRadioTuneInSelector,
};
use crate::services::last_fm::{ILastFmRadioAudioTranscoder, IRadioTuneInSelector};
use crate::services::library::artist_country::{self, ArtistCountryLookup, CountryLibrary};
use crate::services::library::explicit_backfill::{
    ExplicitBackfill, ExplicitBackfillServices, ExplicitBackfillStore,
};
use crate::services::library::for_you_lists::NewReleasesBuilder;
use crate::services::library::generated_playlist_service::ForYouServices;
use crate::services::library::library_edit_service::{
    LibraryEditService, LibraryEditServices, LibraryRescan,
};
use crate::services::library::library_review_sweep_worker::SweepVerification;
use crate::services::library::library_tag_edits::TagEditJournal;
use crate::services::library::library_write_gate::LibraryWriteGate;
use crate::services::library::library_youtube_check::{
    AppYoutubeChecker, LibraryYoutubeCheck, YoutubeCheckStore,
};
use crate::services::library::navidrome_song_list;
use crate::services::library::tidy::worker::ListSongs;
use crate::services::library::tidy::{TidyJournal, TidyServices, TidyStore, TidyWorker};
use crate::services::library::{AlbumOwnership, GeneratedPlaylistService, LibrarySnapshot};
use crate::services::library::{ChartPlaylistService, LibraryMatchCache};
use crate::services::library::{
    DuplicateScanWorker, EditServiceSweepEdits, ExecutorSweepReplacer, FingerprintSweepVerifier,
    IdentitySweepStore, LibraryIdentitySweep, LibraryIdentitySweepParts, LibraryReviewSweepParts,
    LibraryReviewSweepWorker, MusicBrainzRecordingFacts, MusicBrainzReleaseTracks, NavidromeOtherListeners,
    NoticePlaylistWorker, QualityUpgradeStore, QualityUpgradeWorker, ReviewSweepStore, SnapshotSweepLibrary,
    UpgradeWorker, UpgradeWorkerParts,
};
use crate::services::library::{
    HeartOwnership, LibraryActionExecutor, LibraryActionExecutorParts, LibraryActionJournal,
    LibraryActionPlaylistWorker, LibraryActionQuarantine, LibraryActionRatingWorker, LibraryOwnership,
    MissingRows, NavidromePlaylistApi, NavidromeSongPathResolver, NoticeQueue, OwnershipNavidrome,
    RejectionStore, SongAnswers, UpgradeQueue, UpgradeSources,
};
use crate::services::lidarr::{
    ILidarrHeartAcquisitionService, ILidarrTrackFetcher, LidarrAlbumClaims, LidarrClient,
    LidarrHeartAcquisitionService, LidarrHeartExtras, LidarrImportHandoff, LidarrPendingAlbums,
    LidarrTrackFetcher,
};
use crate::services::listen_brainz::{ListenBrainzRadioClient, ListenBrainzService};
use crate::services::local::DownloadHistoryService;
use crate::services::local::{ILocalLibraryService, LocalLibraryService};
use crate::services::lyrics::{
    LyricsChoiceService, LyricsChoiceStore, LyricsLibraryStore, LyricsLibraryWorker, LyricsService,
    LyricsSidecarWriter, LyricsUndoJournal, lyrics_http,
};
use crate::services::metadata::{
    DeezerExplicitCatalog, DeezerMetadataService, DeezerPublishedTempo, DeezerRateLimitHandler,
    DeezerRateLimiter,
};
use crate::services::metadata::{GenreBackfillJournal, GenreBackfillStore, GenreBackfillWorker};
use crate::services::notifications::{DiscordSink, INotificationSink, NotificationService, NtfySink};
use crate::services::prefetch::{
    AppKeepReadySources, KeepReadyService, KeepReadyStore, PrefetchParts, PrefetchService, PrefetchStore,
    PrefetchedFiles, PreviewCopies,
};
use crate::services::radio::{
    IRadioSource, LastFmRadioSource, ListenBrainzRadioSource, RadioOutcomeStore, RadioSourceSet,
    SongRadioService, SoundsAlikeRadioSource, YouTubeMusicRadioSource,
};
use crate::services::sonic::{
    ISonicClient, NavidromeSonicLibrary, SonicAnalysisWorker, SonicClient, SonicStore, TransitionStore,
};
use crate::services::soulseek::soulseek_download_service::{SoulseekDownloadParts, SoulseekDownloadService};
use crate::services::soulseek::soulseek_metadata_service::LastFmTrackLengths;
use crate::services::soulseek::{
    ExternalIdRegistry, ISlskdShares, ISoulseekLink, RadioQueueStore, RejectedPeerRegistry, SoulseekClient,
    SoulseekLink, SoulseekMetadataService, SoulseekPortCheck, SoulseekShareSwitch, SoulseekSharing,
    SoulseekStartupValidator,
};
use crate::services::spotiflac::SpotiflacClient;
use crate::services::subsonic::{
    CredentialCheck, NavidromeIdentityService, RecentScrobbles, RequestIdentity, SearchSongOrderCache,
    SubsonicDiscoveryService, SubsonicProxyService, SyncCatalogService,
};
use crate::services::updates::ReleaseCheck;
use crate::services::updates::UpdateHost;
use crate::services::validation::{
    IStartupValidator, StartupValidationOrchestrator, SubsonicStartupValidator,
};
use crate::services::you_tube::{PinRecheck, StreamSizeWarmer, YouTubeMusicClient, YouTubeResolver};
use crate::workers::WorkerSupervisor;

/// Cheap to clone; handlers and workers receive it by value.
#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<AppInner>,
}

impl Deref for AppState {
    type Target = AppInner;

    fn deref(&self) -> &AppInner {
        &self.inner
    }
}

/// The services.
pub struct AppInner {
    /// The live settings: every section typed, and the raw configuration by key.
    pub settings: Arc<SettingsStore>,
    /// `RestartTracker`, snapshotted when the state is built, before anything can change a
    /// setting, so it holds the values this process actually started with.
    pub restart_tracker: Arc<RestartTracker>,
    /// `SettingsFileWriter`, writing the store's settings.json.
    pub settings_writer: Arc<SettingsFileWriter>,
    /// The background workers.
    pub workers: Arc<WorkerSupervisor>,
    /// `LyricsChoiceStore`: the lyrics pins, `<config>/lyrics-choices.json`.
    pub lyrics_choice_store: Arc<LyricsChoiceStore>,
    /// `LyricsService`, over the `ILyricsSource`s (KuGou on the `kugou` client; LRCLIB,
    /// NetEase and lyrics.ovh on the `lyrics` client).
    pub lyrics_service: Arc<LyricsService>,
    /// `LyricsChoiceService`: choosing lyrics by hand, over the pins.
    pub lyrics_choice_service: Arc<LyricsChoiceService>,
    /// `LyricsSidecarWriter`: its queue is the "LyricsSidecarWriter" worker.
    pub lyrics_sidecar_writer: Arc<LyricsSidecarWriter>,
    /// `LyricsUndoJournal`: what the lyrics page's Save wrote over, `<config>/lyrics-undo.jsonl`.
    pub lyrics_undo_journal: Arc<LyricsUndoJournal>,
    /// `LyricsLibraryStore`: the "find lyrics for the library" run,
    /// `<config>/lyrics-library.json`. Its coalescing flush is the "LyricsLibraryStore" worker.
    pub lyrics_library_store: Arc<LyricsLibraryStore>,
    /// `LyricsLibraryWorker`: its queue is the "LyricsLibraryWorker" worker.
    pub lyrics_library_worker: Arc<LyricsLibraryWorker>,
    /// `GenreBackfillStore`: the genre backfill run, `<config>/genre-backfill.json`. Its
    /// coalescing flush is the "GenreBackfillStore" worker.
    pub genre_backfill_store: Arc<GenreBackfillStore>,
    /// `GenreBackfillJournal`: the genre undo log, `<config>/genre-backfill-journal.jsonl`.
    pub genre_backfill_journal: Arc<GenreBackfillJournal>,
    /// `GenreBackfillWorker`: its queue is the "GenreBackfillWorker" worker.
    pub genre_backfill_worker: Arc<GenreBackfillWorker>,
    /// "Tidy artists": its run and plan beside the settings (`tidy-run.json`, `tidy-plan.json`)
    /// and its undo journal (`tidy-journal.jsonl`); singleton and hosted.
    pub tidy_worker: Arc<TidyWorker>,
    /// `AlbumCoverFinder`: the largest cover of an album.
    pub album_cover_finder: Arc<dyn IAlbumCoverFinder>,
    /// `CoverUpgradeStore`: the cover upgrade run, `<config>/cover-upgrade.json`, and the found
    /// covers in `<config>/cover-upgrade-found/`. Its coalescing flush is the "CoverUpgradeStore"
    /// worker.
    pub cover_upgrade_store: Arc<CoverUpgradeStore>,
    /// `CoverUpgradeJournal`: the cover undo log, `<config>/cover-upgrade-journal.jsonl`, with the
    /// old pictures in `<config>/cover-backups/`.
    pub cover_upgrade_journal: Arc<CoverUpgradeJournal>,
    /// `CoverUpgradeWorker`: its queue is the "CoverUpgradeWorker" worker.
    pub cover_upgrade_worker: Arc<CoverUpgradeWorker>,
    /// `ExplicitBackfill` ("Mark explicit songs"): its queue is the "ExplicitBackfill" worker,
    /// and its store's coalescing flush the "ExplicitBackfillStore" worker,
    /// `<config>/explicit-backfill.json`.
    pub explicit_backfill: Arc<ExplicitBackfill>,
    /// `UpdateHost`: the handshake files with the host updater, `<config>/update/`.
    pub update_host: Arc<UpdateHost>,
    /// The process lifetime: cancel it (see [`AppInner::stop_application`]) to shut the
    /// process down gracefully.
    pub lifetime: CancellationToken,
    /// The default HTTP client.
    pub http: reqwest::Client,
    /// `SubsonicProxyService`. This one has no request (background work); handlers make their
    /// request's own with `with_request`.
    pub subsonic_proxy: SubsonicProxyService,
    /// `NavidromeIdentityService` (a cheap handle).
    pub navidrome_identity: NavidromeIdentityService,
    /// `SubsonicDiscoveryService`.
    pub subsonic_discovery: Arc<SubsonicDiscoveryService>,
    /// `CredentialCheck`.
    pub credential_check: Arc<CredentialCheck>,
    /// `RequestIdentity`.
    pub request_identity: Arc<RequestIdentity>,
    /// `SearchSongOrderCache`.
    pub search_song_order_cache: Arc<SearchSongOrderCache>,
    /// `LocalLibraryService`: `.mappings.json` in the configured download directory, and the
    /// scan trigger.
    pub local_library: Arc<dyn ILocalLibraryService>,
    /// `NavidromeSongPathResolver`.
    pub navidrome_song_path_resolver: Arc<NavidromeSongPathResolver>,
    /// `NavidromePlaylistApi`.
    pub navidrome_playlist_api: Arc<NavidromePlaylistApi>,
    /// `LibrarySnapshot`: the whole library through Navidrome's song list, kept 5 minutes.
    pub library_snapshot: Arc<LibrarySnapshot>,
    /// `AlbumOwnership`: how much of an outside album the library holds.
    pub album_ownership: Arc<AlbumOwnership>,
    /// `StartupValidationOrchestrator` over the `IStartupValidator`s (`SubsonicStartupValidator`,
    /// then `SoulseekStartupValidator`). Run by
    /// [`crate::host::run`] before the listener binds.
    pub startup_validation: Arc<StartupValidationOrchestrator>,

    /// `DeezerRateLimiter`: the budget every Deezer call spends.
    pub deezer_rate_limiter: Arc<DeezerRateLimiter>,
    /// The "deezer" HTTP client with `DeezerRateLimitHandler` in its chain. Every Deezer
    /// caller goes through it.
    pub deezer_client: Arc<DeezerRateLimitHandler>,
    /// `DeezerMetadataService`.
    pub deezer_metadata: Arc<DeezerMetadataService>,
    /// `MusicBrainzClient` with its own HTTP client.
    pub music_brainz: Arc<MusicBrainzClient>,
    /// `AcoustIdRateLimiter`.
    pub acoust_id_rate_limiter: Arc<AcoustIdRateLimiter>,
    /// `AcoustIdClient`, over the "acoustid" HTTP client and its handler.
    pub acoust_id: Arc<AcoustIdClient>,
    /// `ITunesCoverArtLookup`, also held on its own: downloads and the cover upgrade ask it
    /// for an album's master. Its matches persist to itunes-masters.json beside settings.json.
    pub itunes_cover_art: Arc<ITunesCoverArtLookup>,
    /// `CoverArtAggregator` over the `ICoverArtSource`s in the order they are asked: Deezer,
    /// iTunes, Last.fm.
    pub cover_art_aggregator: Arc<CoverArtAggregator>,
    /// `CoverArtArchiveLookup` with its own HTTP client.
    pub cover_art_archive: Arc<CoverArtArchiveLookup>,
    /// `CoverArtService`: list covers, the placeholder and the Octo badge. Pictures in
    /// `<config>/covers` replace the generated cover of the playlist they are named after.
    /// Warmed in the background at startup by the "CoverArtService" worker.
    pub cover_art_service: Arc<CoverArtService>,
    /// `AmbientPaletteService` ("ambient-palette" client, 6 s): the colors of the fetched songs'
    /// covers, the dashboard's light.
    pub ambient_palettes: Arc<crate::services::admin::AmbientPaletteService>,
    /// `DownloadHistoryService`: the fetched-songs log, `downloads-history.json` beside settings.json.
    pub download_history: Arc<DownloadHistoryService>,
    /// `DownloadConcurrency`: how many downloads transfer at once.
    pub download_concurrency: Arc<DownloadConcurrency>,
    /// `DownloadPicks`: copies picked in Find songs, waiting for their download.
    pub download_picks: Arc<DownloadPicks>,
    /// `SoulseekHoldStore`: hearts waiting out a Soulseek outage, `soulseek-holds.json`.
    pub soulseek_holds: Arc<SoulseekHoldStore>,
    /// `YouTubeResolver`: the yt-dlp shim client (its address captured at startup).
    pub you_tube_resolver: Arc<YouTubeResolver>,
    /// `ExternalIdRegistry`: short external ids, `external-ids.json` (flushed by a worker).
    pub external_id_registry: Arc<ExternalIdRegistry>,
    /// `StreamSizeWarmer`: learns the true size of outside songs listed without one; a worker.
    pub stream_size_warmer: Arc<StreamSizeWarmer>,
    /// `LocalCoverCache`: library covers kept on disk in `<config>/cover-cache/`, each size cut
    /// from one copy.
    pub local_cover_cache: Arc<LocalCoverCache>,
    /// `RadioQueueStore`: recent search and radio queues for prewarming, in memory.
    pub radio_queues: Arc<RadioQueueStore>,
    /// `PrefetchService`: upcoming songs fetched ahead into the prefetch folder
    /// (`Prefetch:Folder`, or `<config>/prefetch`); its loop is the "PrefetchService" worker.
    pub prefetch: Arc<PrefetchService>,
    /// `PreviewCopies`: whole copies of outside songs' previews, `<config>/preview-copies`.
    pub preview_copies: Arc<PreviewCopies>,
    /// `KeepReadyService`: each listener's playlists kept ready, `keep-ready.json`; its loop is the
    /// "KeepReady" worker.
    pub keep_ready: Arc<KeepReadyService>,
    /// `DirectoryBrowser`: the dashboard's folder picker.
    pub directory_browser: Arc<DirectoryBrowser>,
    /// `BrowseSessionStore`: dashboard sign-ins, `browse-sessions.json`.
    pub browse_sessions: Arc<BrowseSessionStore>,
    /// `SignInThrottle`: ten wrong sign-ins per address per fifteen minutes, then a wait.
    pub sign_in_throttle: Arc<SignInThrottle>,
    /// Wrong Lidarr webhook passwords, throttled per address like sign-ins (apart from them).
    pub lidarr_webhook_throttle: Arc<SignInThrottle>,
    /// `AdminRecoveryCode`: the one-time way in when Navidrome cannot vouch for anyone,
    /// `<config>/admin-recovery-code`.
    pub admin_recovery_code: Arc<AdminRecoveryCode>,
    /// `AdminRoleCheck`: ends the sign-ins of someone Navidrome no longer counts as an admin.
    pub admin_role_check: Arc<AdminRoleCheck>,
    /// The signed-out `GET /api/admin/status` answer, kept ten seconds.
    pub anonymous_status: crate::controllers::admin::system::AnonymousStatusCache,
    /// `RejectedPeerRegistry`: peer files that proved wrong, `rejected-peers.json` (flushed by
    /// a worker), with `Soulseek:RejectedPeerTtlDays` read live.
    pub rejected_peers: Arc<RejectedPeerRegistry>,
    /// `ReleaseCheck`: whether a newer release is out, `update/release.json`; also a worker.
    pub release_check: Arc<ReleaseCheck>,
    /// `LastFmService`, with its own HTTP client: the web service the radio and the search bar
    /// read, its Accept-Language captured at startup.
    pub last_fm: Arc<LastFmService>,
    /// `LastFmScrobbleService` ("lastfm-scrobble" client): the scrobble queue and the
    /// dashboard's Connect flow, saving sessions through `settings_writer`.
    pub last_fm_scrobbles: Arc<LastFmScrobbleService>,
    /// `ListenBrainzService` ("listenbrainz" client).
    pub listen_brainz: Arc<ListenBrainzService>,
    /// `NotificationService` over the `INotificationSink`s in send order (ntfy, then
    /// Discord), sharing the "notifications" client, with Deezer for missing covers.
    pub notifications: Arc<NotificationService>,
    /// `RecentScrobbles`: completed plays reported lately, so one sent twice is learned from once.
    pub recent_scrobbles: Arc<RecentScrobbles>,
    /// `SubsonicResponseBuilder`: every Subsonic answer Octo writes itself, over the external id
    /// registry, with `Subsonic:WaitForLosslessOnPlay` captured at startup. The app-type answers
    /// come with `services::subsonic::SubsonicResponseBuilderExt`.
    pub subsonic_response_builder: Arc<SubsonicResponseBuilder>,
    /// `SoulseekClient`: slskd's REST client (a cheap handle), its address and login captured
    /// at startup.
    pub soulseek_client: SoulseekClient,
    /// `SoulseekLink`, also used as `ISoulseekLink`: slskd's Soulseek login, read live, for the
    /// dashboard and for downloads that wait out an outage.
    pub soulseek_link: Arc<SoulseekLink>,
    /// `SoulseekPortCheck`: Soulseek's port test for the Sharing card, one answer kept six hours.
    pub soulseek_port_check: Arc<SoulseekPortCheck>,
    /// `SoulseekShareSwitch`, also a worker: the Share my library switch, which keeps
    /// slskd.yml's share list to it and checks every 10 minutes.
    pub soulseek_share_switch: Arc<SoulseekShareSwitch>,
    /// `SoulseekSharing`: what this server shares back with Soulseek, and whether people can
    /// connect to it, for the dashboard.
    pub soulseek_sharing: Arc<SoulseekSharing>,
    /// `SoulseekMetadataService`, the music metadata service.
    pub music_metadata: Arc<SoulseekMetadataService>,
    /// `IMusicMetadataService` as the controllers use it: `music_metadata` in production,
    /// a test's fake in handler tests.
    pub metadata_service: Arc<dyn IMusicMetadataService>,
    /// `SoulseekDownloadService`: the download base over the Soulseek backend, which hearts,
    /// plays, upgrades, Lidarr and library actions all go through (a test's fake in handler
    /// tests that swap it).
    pub download_service: Arc<dyn IDownloadService>,
    /// `LidarrClient`: the Lidarr v1 client, its address and key read live.
    pub lidarr_client: Arc<LidarrClient>,
    /// The person's own SpotiFLAC server, its address and token read live.
    pub spotiflac: Arc<SpotiflacClient>,
    /// `LidarrAlbumClaims`: which Lidarr albums a heart or an upgrade is working on.
    pub lidarr_album_claims: Arc<LidarrAlbumClaims>,
    /// `LidarrImportHandoff`: the files a Lidarr heart brought in, waiting for the pipeline.
    pub lidarr_import_handoff: Arc<LidarrImportHandoff>,
    /// `ILidarrTrackFetcher` (`LidarrTrackFetcher`): one song through Lidarr, for a replacement.
    pub lidarr_track_fetcher: Arc<dyn ILidarrTrackFetcher>,
    /// `ILidarrHeartAcquisitionService` (`LidarrHeartAcquisitionService`): hearts through Lidarr.
    pub lidarr_hearts: Arc<dyn ILidarrHeartAcquisitionService>,
    /// `TrackAcquisitionQueue`: permanent-copy fetches, drained by the "AcquisitionWorker" worker.
    pub track_acquisition_queue: Arc<TrackAcquisitionQueue>,
    /// `AcquisitionActivity`: whether anything is downloading.
    pub acquisition_activity: Arc<AcquisitionActivity>,
    /// `AcquisitionTracker`: where each hearted download has got to, in memory.
    pub acquisition_tracker: Arc<AcquisitionTracker>,
    /// `SongFinder`: Find songs, a song's search run again by hand and a copy picked from it.
    pub song_finder: Arc<SongFinder>,
    /// `StarOnArrival`, built with the state so it is listening when the first download
    /// finishes.
    pub star_on_arrival: Arc<StarOnArrival>,
    /// `UpgradeQueue`: songs asked to be found in higher quality, `upgrades.json`.
    pub upgrade_queue: Arc<UpgradeQueue>,
    /// `UpgradeSources`: where Better quality looks, over the Soulseek link.
    pub upgrade_sources: Arc<UpgradeSources>,
    /// `LibraryOwnership`: whether a song is already in the library.
    pub library_ownership: Arc<LibraryOwnership>,
    /// `HeartOwnership`: asked before a heart goes anywhere.
    pub heart_ownership: Arc<HeartOwnership>,
    /// `HeartAcquisitionCoordinator`; `SoulseekHoldResumer` is its "SoulseekHoldResumer" worker.
    pub heart_acquisition_coordinator: Arc<HeartAcquisitionCoordinator>,
    /// `ExternalSearchService`: the discovery half of a search, once per query.
    pub external_search: Arc<ExternalSearchService>,
    /// `LastFmRadioStateStore`: per-listener plays and stations, `lastfm-radio-state.json`.
    pub last_fm_radio_state: Arc<LastFmRadioStateStore>,
    /// `LastFmRadioRefreshQueue`: the rebuilds waiting for the refresh worker.
    pub last_fm_radio_refresh_queue: Arc<LastFmRadioRefreshQueue>,
    /// `LastFmRadioStreamSessionStore`: the radio stream tokens, in memory.
    pub last_fm_radio_stream_sessions: Arc<LastFmRadioStreamSessionStore>,
    /// `LastFmRadioTrackCache`: `<temp>/octo-cache/radio`.
    pub last_fm_radio_track_cache: Arc<LastFmRadioTrackCache>,
    /// `LastFmRadioRecommendationService` (it holds nothing between builds).
    pub last_fm_radio_recommendations: Arc<LastFmRadioRecommendationService>,
    /// `LastFmRadioStreamService` over the background proxy, with the shared transcoder
    /// (`FfmpegLastFmRadioAudioTranscoder`) and tune-in selector (`RandomRadioTuneInSelector`).
    /// A handler makes its request's own with `scoped(proxy)`.
    pub last_fm_radio_streams: LastFmRadioStreamService,
    /// `LastFmRadioWarmupService`: its loop is the "LastFmRadioWarmupService" worker.
    pub last_fm_radio_warmup: Arc<LastFmRadioWarmupService>,
    /// `LastFmRadioRefreshWorker`: the "LastFmRadioRefreshWorker" worker, and the only
    /// subscriber to settings changes.
    pub last_fm_radio_refresh_worker: Arc<LastFmRadioRefreshWorker>,
    /// `RadioSourceSet`: every radio source (`IRadioSource`), asked together.
    pub radio_sources: Arc<RadioSourceSet>,
    /// `SongRadioService`: getSimilarSongs over the sources; each request hands it its own
    /// resolver.
    pub song_radio: Arc<SongRadioService>,
    /// `RadioOutcomeStore`: what radio learns from listening, `radio-outcomes.json`. Its flush on
    /// the way out is the "RadioOutcomeStore" worker.
    pub radio_outcomes: Arc<RadioOutcomeStore>,
    /// `SonicStore`: what every library song sounds like, `sonic-features.json`.
    pub sonic_store: Arc<SonicStore>,
    /// `SonicAnalysisWorker`: the "SonicAnalysisWorker" worker.
    pub sonic_worker: Arc<SonicAnalysisWorker>,
    /// `GeneratedPlaylistService`: the genre and decade mixes, `generated-playlists.json`.
    pub generated_playlists: Arc<GeneratedPlaylistService>,
    /// `PlaceIndex`: postal codes and cities as coordinates (getPlace), read from the place data
    /// a block at a time.
    pub places: Arc<PlaceIndex>,
    /// `TopSongsService`: an artist's top songs and every chart (Apple Music's pages, Deezer and
    /// Last.fm behind Popular right now).
    pub top_songs: TopSongsService,
    /// The chart lists as read-only playlists for every listener.
    pub chart_playlists: Arc<ChartPlaylistService>,
    /// What each listener's library has already said it lacks, shared by the chart lists and the
    /// top songs answers.
    pub library_matches: Arc<LibraryMatchCache>,
    /// Apple Music's animated album art for the Octo apps, kept in `animated-art.json`.
    pub animated_art: Arc<AnimatedArt>,
    /// `SyncCatalogService`: the discovery catalog appended to a syncing client's library walk,
    /// built over the background proxy.
    pub sync_catalog: Arc<SyncCatalogService>,
    /// `LibraryActionJournal`: the write-ahead journal, `<config>/library-actions.json`. Its
    /// 10-second flush is the "LibraryActionJournal" worker.
    pub library_action_journal: Arc<LibraryActionJournal>,
    /// `TagEditJournal`: the tag edits made from the apps and by the explicit marking,
    /// `<config>/tag-edits.json`.
    pub tag_edit_journal: Arc<TagEditJournal>,
    /// The music folder's one-writer gate, shared by every job and placement that writes it.
    pub library_write_gate: LibraryWriteGate,
    /// `LibraryActionQuarantine`.
    pub library_action_quarantine: Arc<LibraryActionQuarantine>,
    /// `LibraryActionExecutor`: the only code in library actions that touches a file.
    pub library_action_executor: Arc<LibraryActionExecutor>,
    /// `LibraryActionRatingWorker`: the controller enqueues into the "LibraryActionRatingWorker"
    /// worker. (`LibraryActionPlaylistWorker` is a worker only, and
    /// `LibraryActionPlaylistProvisioner` is built per request over its proxy.)
    pub library_action_rating_worker: Arc<LibraryActionRatingWorker>,
    /// `NoticeQueue`: what Octo asked each person, `notice-queue.json`. Its 5-second flush is
    /// the "NoticeQueue" worker.
    pub notice_queue: Arc<NoticeQueue>,
    /// `ILoudnessMeter` (`LoudnessMeter`): ReplayGain for downloads and the tag preview.
    pub loudness_meter: Arc<dyn ILoudnessMeter>,
    /// `AudioFingerprinter`: fpcalc, for download verification.
    pub audio_fingerprinter: Arc<AudioFingerprinter>,
    /// `SpectrumAnalyzer`: the transcode check.
    pub spectrum_analyzer: Arc<SpectrumAnalyzer>,
    /// `ReleaseIdentifier`: what a downloaded file is, for the download base and the preview.
    pub release_identifier: Arc<ReleaseIdentifier>,
    /// `TagPreview`: "Try it on a song".
    pub tag_preview: Arc<TagPreview>,
    /// `DownloadVerificationService`: the fingerprint and ISRC checks of a finished download.
    pub download_verification: Arc<DownloadVerificationService>,
    /// `DownloadCoverResolver`: the download-time cover chain (#51).
    pub download_cover_resolver: Arc<DownloadCoverResolver>,
    /// `UpgradeWorker`: the "UpgradeWorker" worker.
    pub upgrade_worker: Arc<UpgradeWorker>,
    /// `QualityUpgradeStore`: what the weekly upgrade tried, `quality-upgrade.json`.
    pub quality_upgrade_store: Arc<QualityUpgradeStore>,
    /// `QualityUpgradeWorker`: the "QualityUpgradeWorker" worker.
    pub quality_upgrade_worker: Arc<QualityUpgradeWorker>,
    /// `DuplicateScanWorker`, one instance held here and run as a worker, so "Scan now" reaches
    /// the running instance: the "DuplicateScanWorker" worker.
    pub duplicate_scan_worker: Arc<DuplicateScanWorker>,
    /// `ReviewSweepStore`: the library sweep's cursor, `review-sweep.json`. Its 5-second flush
    /// is the "ReviewSweepStore" worker.
    pub review_sweep_store: Arc<ReviewSweepStore>,
    /// `LibraryReviewSweepWorker`: the "LibraryReviewSweepWorker" worker.
    pub library_review_sweep_worker: Arc<LibraryReviewSweepWorker>,
    /// `LibraryYoutubeCheck`: the one-time comparison of every library song with its YouTube
    /// preview; its state is `youtube-check.json`. The "LibraryYoutubeCheck" worker.
    pub library_youtube_check: Arc<LibraryYoutubeCheck>,
    /// `IdentitySweepStore`: the library tag check's place and answers, `identity-sweep.json`.
    /// Its 5-second flush is the "IdentitySweepStore" worker.
    pub identity_sweep_store: Arc<IdentitySweepStore>,
    /// `LibraryIdentitySweep`: every library song's tags held against its audio, for Library
    /// health: the "LibraryIdentitySweep" worker.
    pub library_identity_sweep: Arc<LibraryIdentitySweep>,
    /// `FingerprintSweepVerifier`: the sweep's check, over the download verification service.
    pub fingerprint_sweep_verifier: Arc<FingerprintSweepVerifier>,
    /// `LibraryRescan`: one normal Navidrome scan a few seconds after a run of library changes.
    pub library_rescan: Arc<LibraryRescan>,
    /// `LibraryEditService`: the apps' Library health fixes, tags written in place, albums
    /// joined, covers added, and a removed song put back.
    pub library_edits: Arc<LibraryEditService>,
    /// `LibraryHealthService`: Library health for the dashboard, the app's checks and fixes run
    /// on the server.
    pub library_health: Arc<LibraryHealthService>,
    /// `ImportService`, over the import stores (`imports.json`, `imports-trickle.json`) and the
    /// trickle (`TrickleWorker`): the "ImportService" and "TrickleWorker" workers.
    pub imports: Arc<crate::services::imports::ImportService>,
    /// `family.key`: seals members' device secrets and hidden Navidrome passwords, and signs
    /// their browser tokens.
    pub family_vault: Arc<FamilyVault>,
    /// `family.json`: the owner, members, devices, invites and pairing codes.
    pub family_store: Arc<FamilyStore>,
    /// `family-requests.json`: members' requests for a copy.
    pub family_requests: Arc<RequestStore>,
    /// `family-saved.json`: outside songs and albums members saved.
    pub family_saved: Arc<SavedStore>,
    /// `family-playlists.json`: playlists every member can add to.
    pub family_playlists: Arc<FamilyPlaylistStore>,
    /// Where family playlist additions that wait for approval are reported.
    pub family_playlist_requests: crate::services::family::family_playlists::PlaylistAddHook,
    /// `family-pool.json`: the one copy of each member download and who links it.
    pub family_pool: Arc<FamilyPoolStore>,
    /// `listening/`: each person's completed plays, for their Listening page.
    pub listening: Arc<crate::services::listening::ListeningLog>,
    /// Octo's admin calls to Navidrome for the family (accounts, libraries, players, scans).
    pub navidrome_admin: Arc<NavidromeAdmin>,
    /// Which library every song is in, read from Navidrome while Family is on.
    pub library_index: Arc<LibraryIndex>,
    /// Whether a song is shared or a member's own, for work that must keep to one or the other.
    pub library_scope: ScopeResolver,
    /// The family pool: one file per member download, linked into each member's library.
    pub family_pool_service: Arc<FamilyPool>,
    /// Members' own libraries: their folders, their storage, removing a song.
    pub personal_library: Arc<PersonalLibrary>,
    /// What becomes of a family member's library writes (add now, request, or save only).
    pub family_write_gate: Arc<FamilyWriteGate>,
    /// Members' requests for a copy.
    pub family_request_service: Arc<RequestService>,
    /// Request events, for pushes and the apps.
    pub family_events: Arc<FamilyEvents>,
    /// Sign-ins made from members' current hidden passwords, for work done later as a member.
    pub family_credentials: Arc<MemberCredentials>,
    /// Members' devices and the devices seen for everyone else.
    pub family_devices: Arc<DeviceRegistry>,
    /// The family gate's failure throttle and password repairs.
    pub family_bridge: Arc<MemberBridge>,
    /// The rules that rewrite answers for the family, by endpoint.
    pub family_answer_rules: Arc<AnswerRules>,
    /// The rules about audio while Family is on, and the live streams.
    pub family_audio: Arc<AudioGate>,
    /// The Family page's and the apps' family actions, and the hooks they reach other work by.
    pub family: Arc<FamilyService>,
    /// Whether Family's background work has been started (see [`AppInner::start_family`]).
    family_started: AtomicBool,
    /// Whether the library index is being read (see [`AppInner::start_library_index`]).
    library_index_started: AtomicBool,
}

/// Library health for the dashboard: the app's checks over the library snapshot, and its fixes
/// through the edit service and the library actions executor.
#[allow(clippy::too_many_arguments)]
fn library_health(
    settings: &Arc<SettingsStore>,
    snapshot: &Arc<LibrarySnapshot>,
    rescan: &Arc<LibraryRescan>,
    edits: &Arc<LibraryEditService>,
    executor: &Arc<LibraryActionExecutor>,
    upgrade_sources: &Arc<UpgradeSources>,
    navidrome: &NavidromeServices,
    stores: &Stores,
    quarantine: &Arc<LibraryActionQuarantine>,
    scope: &ScopeResolver,
) -> Arc<LibraryHealthService> {
    // The admin's marks are read with Octo's admin sign-in, each member's with their own.
    let members = NavidromeMemberMarks::new(
        Arc::clone(settings),
        navidrome.http.clone(),
        Arc::clone(&stores.family_store),
        Arc::new(MemberCredentials::new(
            Arc::clone(&stores.family_store),
            Arc::clone(&stores.family_vault),
            Arc::clone(settings),
        )),
    );
    let keep_marks = Arc::new(KeepMarks::new(KeepMarksParts {
        api: navidrome.navidrome_playlist_api.clone(),
        resolver: navidrome.navidrome_song_path_resolver.clone(),
        quarantine: Arc::clone(quarantine),
        journal: stores.library_action_journal.clone(),
        library: navidrome.local_library.clone(),
        executor: Arc::clone(executor),
        settings: Arc::clone(settings),
        members: Some(Arc::new(members)),
        write_gate: Some(stores.library_write_gate.clone()),
    }));
    let health = Arc::new(LibraryHealthService::new(
        Arc::clone(snapshot),
        Arc::clone(rescan),
        Arc::clone(settings),
        HealthServices {
            edits: Some(Arc::clone(edits)),
            executor: Some(Arc::clone(executor)),
            upgrade_sources: Some(Arc::clone(upgrade_sources)),
            keep_marks: Some(keep_marks),
            scope: Some(scope.clone()),
        },
        Clock::system(),
    ));
    // Fix duplicates, from a star or its action playlist, runs Library health's duplicate fix.
    executor.use_duplicate_fixer(Arc::new(HealthDuplicateFixer::new(&health)));
    health
}

/// The library tag check: Navidrome's song list from the snapshot, the sweep's fingerprint
/// check, the edit path stray spaces go through, and MusicBrainz for a release's tracks and what a
/// recording is.
#[allow(clippy::too_many_arguments)]
fn library_identity_sweep(
    settings: &Arc<SettingsStore>,
    stores: &Stores,
    navidrome: &NavidromeServices,
    music_brainz: &Arc<MusicBrainzClient>,
    activity: Arc<dyn IAcquisitionActivity>,
    verifier: Arc<FingerprintSweepVerifier>,
    snapshot: &Arc<LibrarySnapshot>,
    edits: &Arc<LibraryEditService>,
    scope: &ScopeResolver,
    versions: Option<Arc<dyn crate::services::library::version_check::VersionSources>>,
) -> Arc<LibraryIdentitySweep> {
    Arc::new(LibraryIdentitySweep::new(
        LibraryIdentitySweepParts {
            store: stores.identity_sweep_store.clone(),
            verifier,
            activity,
            library: Arc::new(SnapshotSweepLibrary {
                snapshot: Arc::clone(snapshot),
                resolver: navidrome.navidrome_song_path_resolver.clone(),
                scope: Some(scope.clone()),
            }),
            edits: Some(Arc::new(EditServiceSweepEdits(Arc::clone(edits)))),
            releases: Some(Arc::new(MusicBrainzReleaseTracks(Arc::clone(music_brainz)))),
            recordings: Some(Arc::new(MusicBrainzRecordingFacts(Arc::clone(music_brainz)))),
            versions,
            settings: Arc::clone(settings),
        },
        Clock::system(),
    ))
}

/// The apps' Library health fixes: the rescan and the edit service, over the resolver, the
/// quarantine, both journals and the services a lookup and a cover ask.
fn library_edits(
    settings: &Arc<SettingsStore>,
    stores: &Stores,
    navidrome: &NavidromeServices,
    quarantine: &Arc<LibraryActionQuarantine>,
    tagging: &Tagging,
    cover_finder: Arc<dyn IAlbumCoverFinder>,
) -> (Arc<LibraryRescan>, Arc<LibraryEditService>) {
    let rescan = Arc::new(LibraryRescan::new(Some(navidrome.local_library.clone())));
    let edits = Arc::new(LibraryEditService::new(
        navidrome.navidrome_song_path_resolver.clone(),
        quarantine.clone(),
        stores.library_action_journal.clone(),
        stores.tag_edit_journal.clone(),
        rescan.clone(),
        settings.clone(),
        LibraryEditServices {
            library: Some(navidrome.local_library.clone()),
            identifier: Some(tagging.release_identifier.clone()),
            verification: Some(tagging.download_verification.clone()),
            cover_finder: Some(cover_finder),
            write_gate: Some(stores.library_write_gate.clone()),
            rejected_peers: Some(stores.rejected_peers.clone()),
            notices: Some(stores.notice_queue.clone()),
        },
    ));
    (rescan, edits)
}

/// Getting upcoming songs ahead, over the acquisition pipeline: the download service fetches them,
/// the heart queue goes first, and the coordinator starts them as plays with DownloadOnPlay on.
fn prefetch_service(
    settings: &Arc<SettingsStore>,
    stores: &Stores,
    navidrome: &NavidromeServices,
    acquisition: &Acquisition,
    soulseek_link: Arc<dyn ISoulseekLink>,
) -> Arc<PrefetchService> {
    PrefetchService::new(PrefetchParts {
        settings: settings.clone(),
        store: stores.prefetch_store.clone(),
        downloads: acquisition.download_service.clone(),
        queue: acquisition.track_acquisition_queue.clone(),
        coordinator: acquisition.heart_acquisition_coordinator.clone(),
        radio_queues: stores.radio_queues.clone(),
        write_gate: navidrome.family_write_gate.clone(),
        library: navidrome.local_library.clone(),
        concurrency: Some(stores.download_concurrency.clone()),
        names: Some(stores.external_id_registry.clone()),
        soulseek: Some(soulseek_link),
        clock: Clock::system(),
    })
}

/// Playlists kept ready, over the lists, the plays and getting songs ahead.
#[allow(clippy::too_many_arguments)]
fn keep_ready_service(
    settings: &Arc<SettingsStore>,
    config_dir: &Path,
    stores: &Stores,
    imports: &Arc<crate::services::imports::ImportService>,
    mixes: &Arc<GeneratedPlaylistService>,
    charts: &Arc<ChartPlaylistService>,
    metadata: &Arc<dyn IMusicMetadataService>,
    library: &Arc<dyn ILocalLibraryService>,
    prefetch: &Arc<PrefetchService>,
) -> Arc<KeepReadyService> {
    let sources = AppKeepReadySources {
        settings: settings.clone(),
        imports: imports.clone(),
        registry: stores.external_id_registry.clone(),
        mixes: mixes.clone(),
        charts: charts.clone(),
        family: stores.family_store.clone(),
        family_playlists: stores.family_playlists.clone(),
        listening: stores.listening.clone(),
        metadata: metadata.clone(),
        library: library.clone(),
    };
    KeepReadyService::new(
        settings.clone(),
        Arc::new(KeepReadyStore::new(Some(config_dir.join("keep-ready.json")))),
        Arc::new(sources),
        prefetch.clone(),
        stores.preview_copies.clone(),
        Clock::system(),
    )
}

/// The tagging and verification services, which the download base
/// (`services::common::BaseDownloadService`) and the tag preview are built over.
struct Tagging {
    loudness_meter: Arc<dyn ILoudnessMeter>,
    audio_fingerprinter: Arc<AudioFingerprinter>,
    spectrum_analyzer: Arc<SpectrumAnalyzer>,
    release_identifier: Arc<ReleaseIdentifier>,
    tag_preview: Arc<TagPreview>,
    download_verification: Arc<DownloadVerificationService>,
    download_cover_resolver: Arc<DownloadCoverResolver>,
}

impl Tagging {
    fn build(settings: &Arc<SettingsStore>, clients: &MetadataClients) -> Tagging {
        let loudness_meter: Arc<dyn ILoudnessMeter> = Arc::new(LoudnessMeter::new());
        let audio_fingerprinter = Arc::new(AudioFingerprinter::new());
        let spectrum_analyzer = Arc::new(SpectrumAnalyzer::new());
        let release_identifier = Arc::new(ReleaseIdentifier::new(
            Arc::clone(settings),
            Arc::new(TagWriterExtras),
            Some(Arc::clone(&clients.music_brainz) as Arc<dyn ReleaseLookup>),
            Some(Arc::clone(&clients.deezer_metadata) as Arc<dyn CatalogLookup>),
        ));
        let download_verification = Arc::new(DownloadVerificationService::new(
            Arc::clone(&audio_fingerprinter),
            Arc::clone(&clients.acoust_id),
            Arc::clone(settings),
            Some(Arc::clone(&clients.music_brainz)),
            Some(Arc::clone(&spectrum_analyzer)),
        ));
        let tag_preview = Arc::new(TagPreview::new(
            Arc::clone(settings),
            Arc::clone(&release_identifier),
            Arc::new(TagWriterExtras),
            Some(Arc::clone(&download_verification) as Arc<dyn FingerprintVerifier>),
            Some(Arc::new(MeterPreview(Arc::clone(&loudness_meter)))),
            Arc::new(CatalogBlanks),
        ));
        let download_cover_resolver = Arc::new(DownloadCoverResolver::new(
            Arc::clone(&clients.cover_art_archive),
            Arc::clone(&clients.cover_art_aggregator),
            http_client_factory::default_client(),
            Arc::clone(settings),
            Some(Arc::clone(&clients.itunes_cover_art)),
        ));
        Tagging {
            loudness_meter,
            audio_fingerprinter,
            spectrum_analyzer,
            release_identifier,
            tag_preview,
            download_verification,
            download_cover_resolver,
        }
    }
}

/// What the download service is built over beyond the acquisition pipeline's own services: the
/// tagging and verification services, the lyrics writer and slskd's client.
struct DownloadDeps<'a> {
    tagging: &'a Tagging,
    lyrics: &'a Arc<LyricsSidecarWriter>,
    soulseek_client: &'a SoulseekClient,
}

/// The acquisition pipeline, over the services it routes to. Two links would close a cycle, so
/// they are weak: `AcquisitionActivity` holds the download service weakly, set once it exists,
/// and `StarOnArrival` listens to the tracker through a weak reference. The tracker's own
/// services (the song path resolver, the identity, the library service) depend on nothing of
/// this, so they are handed over at construction.
struct Acquisition {
    download_service: Arc<dyn IDownloadService>,
    lidarr_client: Arc<LidarrClient>,
    spotiflac: Arc<SpotiflacClient>,
    lidarr_album_claims: Arc<LidarrAlbumClaims>,
    lidarr_import_handoff: Arc<LidarrImportHandoff>,
    lidarr_track_fetcher: Arc<dyn ILidarrTrackFetcher>,
    lidarr_hearts: Arc<dyn ILidarrHeartAcquisitionService>,
    track_acquisition_queue: Arc<TrackAcquisitionQueue>,
    acquisition_activity: Arc<AcquisitionActivity>,
    acquisition_tracker: Arc<AcquisitionTracker>,
    song_finder: Arc<SongFinder>,
    star_on_arrival: Arc<StarOnArrival>,
    upgrade_queue: Arc<UpgradeQueue>,
    upgrade_sources: Arc<UpgradeSources>,
    library_ownership: Arc<LibraryOwnership>,
    heart_ownership: Arc<HeartOwnership>,
    heart_acquisition_coordinator: Arc<HeartAcquisitionCoordinator>,
    external_search: Arc<ExternalSearchService>,
    family_pool_service: Arc<FamilyPool>,
}

impl Acquisition {
    fn build(
        settings: &Arc<SettingsStore>,
        stores: &Stores,
        navidrome: &NavidromeServices,
        integrations: &Integrations,
        clients: &MetadataClients,
        music_metadata: Arc<dyn IMusicMetadataService>,
        soulseek_link: Arc<dyn ISoulseekLink>,
        downloads: DownloadDeps<'_>,
        download_service: Option<Arc<dyn IDownloadService>>,
    ) -> Acquisition {
        // Stars take turns per person while Family is on, and keep their order otherwise.
        let family_on = Arc::clone(settings);
        let track_acquisition_queue = Arc::new(
            TrackAcquisitionQueue::new()
                .with_fair_turns(Arc::new(move || family_on.current().family.enabled)),
        );
        let acquisition_activity = Arc::new(AcquisitionActivity::new(track_acquisition_queue.clone()));
        let acquisition_tracker = Arc::new(AcquisitionTracker::new(
            Some(TrackerServices {
                identity: navidrome.navidrome_identity.clone(),
                resolver: navidrome.navidrome_song_path_resolver.clone(),
                library: navidrome.local_library.clone(),
            }),
            Clock::system(),
        ));
        // A download's lyrics look reports to its row in the downloads log, and each finished
        // download's log is saved with its fetched-songs entry, for the dashboard.
        downloads.lyrics.set_tracker(acquisition_tracker.clone());
        DownloadLogKeeper::attach(&acquisition_tracker, stores.download_history.clone());
        let star_on_arrival = StarOnArrival::new(
            acquisition_tracker.clone(),
            Some(StarNavidrome {
                proxy: navidrome.subsonic_proxy.clone(),
                identity: navidrome.navidrome_identity.clone(),
                resolver: navidrome.navidrome_song_path_resolver.clone(),
            }),
            settings.clone(),
            Clock::system(),
        );
        let upgrade_queue = stores.upgrade_queue.clone();
        let upgrade_sources = Arc::new(UpgradeSources::new(settings.clone(), Some(soulseek_link.clone())));
        let library_ownership = Arc::new(LibraryOwnership::new(
            settings.clone(),
            Some(OwnershipNavidrome {
                identity: navidrome.navidrome_identity.clone(),
                http: navidrome.http.clone(),
                resolver: navidrome.navidrome_song_path_resolver.clone(),
            }),
            navidrome.local_library.clone(),
        ));
        // With Family on, a member's own songs are never the shared library's copy.
        library_ownership.set_scope(navidrome.library_scope.clone());

        // Lidarr. What the heart service needs besides (the download pipeline, the journal, the
        // ownership check, the upgrade queue) is built by the time it is.
        let lidarr_client = Arc::new(LidarrClient::new(settings.clone()));
        let lidarr_album_claims = Arc::new(LidarrAlbumClaims::new());
        let lidarr_import_handoff = Arc::new(LidarrImportHandoff::new());
        let lidarr_fetcher = Arc::new(LidarrTrackFetcher::new(
            lidarr_client.clone(),
            settings.clone(),
            navidrome.navidrome_identity.clone(),
            lidarr_album_claims.clone(),
            Some(clients.music_brainz.clone()),
        ));
        let lidarr_track_fetcher: Arc<dyn ILidarrTrackFetcher> = lidarr_fetcher.clone();
        let spotiflac = Arc::new(SpotiflacClient::new(settings.clone()));

        // The download service: the base is built over the services above, and the Soulseek
        // backend's optional ones are the link and Lidarr's two. A handler test may swap in its
        // own.
        let (download_service, soulseek_backend): (Arc<dyn IDownloadService>, _) = match download_service {
            Some(fake) => (fake, None),
            None => {
                let (download_base, backend) = SoulseekDownloadService::build(
                    DownloadCore {
                        settings: settings.clone(),
                        local_library: navidrome.local_library.clone(),
                        metadata: music_metadata.clone(),
                        navidrome_identity: navidrome.navidrome_identity.clone(),
                        history: stores.download_history.clone(),
                        notifications: integrations.notifications.clone(),
                    },
                    DownloadServices {
                        concurrency: Some(stores.download_concurrency.clone()),
                        tracker: Some(acquisition_tracker.clone()),
                        ownership: Some(library_ownership.clone()),
                        upgrade_queue: Some(upgrade_queue.clone()),
                        upgrade_sources: Some(upgrade_sources.clone()),
                        action_journal: Some(stores.library_action_journal.clone()),
                        review_queue: Some(stores.notice_queue.clone()),
                        // No playlist sync service is built.
                        playlist_sync: None,
                        deezer: Some(clients.deezer_metadata.clone()),
                        music_brainz: Some(clients.music_brainz.clone()),
                        release_identifier: Some(downloads.tagging.release_identifier.clone()),
                        loudness_meter: Some(downloads.tagging.loudness_meter.clone()),
                        cover_resolver: Some(downloads.tagging.download_cover_resolver.clone()),
                        lyrics: Some(downloads.lyrics.clone()),
                        last_fm: Some(integrations.last_fm.clone()),
                        picks: Some(stores.download_picks.clone()),
                        write_gate: Some(stores.library_write_gate.clone()),
                        family: Some(navidrome.family_pool_service.clone() as Arc<dyn FamilyPlacement>),
                        arrivals: Some(stores.download_arrivals.clone()),
                        prefetched: Some(stores.prefetch_store.clone() as Arc<dyn PrefetchedFiles>),
                    },
                    SoulseekDownloadParts {
                        slskd: downloads.soulseek_client.clone(),
                        rejected_peers: stores.rejected_peers.clone(),
                        verification: downloads.tagging.download_verification.clone(),
                        youtube: stores.you_tube_resolver.clone(),
                        id_registry: stores.external_id_registry.clone(),
                        soulseek_link: Some(soulseek_link.clone()),
                        lidarr_imports: Some(lidarr_import_handoff.clone()),
                        lidarr_fetcher: Some(lidarr_track_fetcher.clone()),
                        spotiflac: Some(spotiflac.clone()),
                        preview_copies: Some(stores.preview_copies.clone()),
                    },
                );
                (download_base as Arc<dyn IDownloadService>, Some(backend))
            }
        };
        acquisition_activity.set_downloads(&download_service);

        let lidarr_hearts: Arc<dyn ILidarrHeartAcquisitionService> =
            Arc::new(LidarrHeartAcquisitionService::new(
                lidarr_client.clone(),
                music_metadata.clone(),
                clients.deezer_metadata.clone(),
                settings.clone(),
                navidrome.navidrome_identity.clone(),
                integrations.notifications.clone(),
                LidarrHeartExtras {
                    tracker: Some(acquisition_tracker.clone()),
                    music_brainz: Some(clients.music_brainz.clone()),
                    claims: Some(lidarr_album_claims.clone()),
                    imports: Some(lidarr_import_handoff.clone()),
                    ids: Some(stores.external_id_registry.clone()),
                    downloads: Some(download_service.clone()),
                    journal: Some(stores.library_action_journal.clone()),
                    ownership: Some(library_ownership.clone()),
                    upgrades: Some(upgrade_queue.clone()),
                    pending: Some(stores.lidarr_pending.clone()),
                },
            ));
        let heart_ownership = Arc::new(HeartOwnership::new(
            library_ownership.clone(),
            music_metadata.clone(),
            settings.clone(),
            Some(upgrade_queue.clone()),
            Some(upgrade_sources.clone()),
        ));
        let heart_acquisition_coordinator = HeartAcquisitionCoordinator::new(
            settings.clone(),
            track_acquisition_queue.clone(),
            download_service.clone(),
            lidarr_hearts.clone(),
            HeartCoordinatorExtras {
                tracker: Some(acquisition_tracker.clone()),
                id_registry: Some(stores.external_id_registry.clone()),
                soulseek: Some(soulseek_link.clone()),
                holds: Some(stores.soulseek_holds.clone()),
                owned: Some(heart_ownership.clone()),
                stars: Some(star_on_arrival.clone()),
                ahead: Some(stores.prefetch_store.clone() as Arc<dyn PrefetchedFiles>),
            },
        );
        heart_acquisition_coordinator.use_family(FamilyHooks {
            gate: navidrome.family_write_gate.clone(),
            pool: navidrome.family_pool_service.clone(),
            metadata: music_metadata.clone(),
        });
        let song_finder = Arc::new(SongFinder::new(SongFinderServices {
            settings: Some(settings.clone()),
            upgrade_queue: Some(upgrade_queue.clone()),
            upgrade_sources: Some(upgrade_sources.clone()),
            executor: true,
            tracker: Some(acquisition_tracker.clone()),
            acquisitions: Some(track_acquisition_queue.clone()),
            picks: Some(stores.download_picks.clone()),
            local_library: Some(navidrome.local_library.clone()),
            id_registry: Some(stores.external_id_registry.clone()),
            resolver: Some(navidrome.navidrome_song_path_resolver.clone()),
            soulseek: soulseek_backend,
            soulseek_link: Some(soulseek_link.clone()),
            lidarr_fetcher: Some(lidarr_fetcher),
            lidarr_client: Some(lidarr_client.clone()),
            family: Default::default(),
        }));
        song_finder.use_family(navidrome.family_write_gate.clone());
        let external_search = Arc::new(ExternalSearchService::new(
            music_metadata,
            Some(integrations.last_fm.clone()),
            Some(settings.clone()),
        ));
        Acquisition {
            download_service,
            lidarr_client,
            spotiflac,
            lidarr_album_claims,
            lidarr_import_handoff,
            lidarr_track_fetcher,
            lidarr_hearts,
            track_acquisition_queue,
            acquisition_activity,
            acquisition_tracker,
            song_finder,
            star_on_arrival,
            upgrade_queue,
            upgrade_sources,
            library_ownership,
            heart_ownership,
            heart_acquisition_coordinator,
            external_search,
            family_pool_service: navidrome.family_pool_service.clone(),
        }
    }

    /// `AcquisitionWorker`, `SoulseekHoldResumer` and `CacheCleanupService`, as workers.
    fn register_workers(
        &self,
        workers: &WorkerSupervisor,
        settings: &Arc<SettingsStore>,
        stores: &Stores,
        notifications: &Arc<NotificationService>,
    ) {
        let worker = Arc::new(
            AcquisitionWorker::new(
                self.track_acquisition_queue.clone(),
                self.download_service.clone(),
                stores.external_id_registry.clone(),
                notifications.clone(),
                Some(stores.download_concurrency.clone()),
            )
            .with_family(self.family_pool_service.clone() as Arc<dyn FamilyPlacement>),
        );
        workers.register("AcquisitionWorker", move |stopping| worker.clone().run(stopping));
        let resumer = Arc::new(SoulseekHoldResumer::new(
            stores.soulseek_holds.clone(),
            self.heart_acquisition_coordinator.clone(),
        ));
        workers.register("SoulseekHoldResumer", move |stopping| {
            resumer.clone().run(stopping)
        });
        let cleanup = Arc::new(CacheCleanupService::new(settings.clone()));
        workers.register("CacheCleanupService", move |stopping| {
            cleanup.clone().run(stopping)
        });
    }
}

/// The Last.fm, ListenBrainz and notification services. None runs in the background until it is
/// used: the scrobble queue drains on a task of its own while plays wait.
struct Integrations {
    last_fm: Arc<LastFmService>,
    last_fm_scrobbles: Arc<LastFmScrobbleService>,
    listen_brainz: Arc<ListenBrainzService>,
    notifications: Arc<NotificationService>,
    recent_scrobbles: Arc<RecentScrobbles>,
}

impl Integrations {
    fn build(
        settings: &Arc<SettingsStore>,
        settings_writer: &Arc<SettingsFileWriter>,
        deezer: &Arc<DeezerMetadataService>,
        last_fm_base_url: Option<&str>,
    ) -> Self {
        let notifications_client = NotificationService::client();
        // A list of sinks, so adding a transport is one line here.
        let sinks: Vec<Arc<dyn INotificationSink>> = vec![
            Arc::new(NtfySink::new(notifications_client.clone(), Arc::clone(settings))),
            Arc::new(DiscordSink::new(notifications_client, Arc::clone(settings))),
        ];
        Self {
            last_fm: Arc::new(match last_fm_base_url {
                Some(url) => LastFmService::with_base_url(Arc::clone(settings), url),
                None => LastFmService::new(Arc::clone(settings)),
            }),
            last_fm_scrobbles: Arc::new(LastFmScrobbleService::new(
                Arc::clone(settings),
                Arc::clone(settings_writer),
            )),
            listen_brainz: Arc::new(ListenBrainzService::new(Arc::clone(settings))),
            notifications: Arc::new(NotificationService::new(
                sinks,
                Arc::clone(settings),
                Some(Arc::clone(deezer)),
            )),
            recent_scrobbles: Arc::new(RecentScrobbles::new()),
        }
    }
}

/// The Navidrome-facing services, built in dependency order.
struct NavidromeServices {
    http: reqwest::Client,
    subsonic_proxy: SubsonicProxyService,
    navidrome_identity: NavidromeIdentityService,
    local_library: Arc<dyn ILocalLibraryService>,
    navidrome_song_path_resolver: Arc<NavidromeSongPathResolver>,
    navidrome_playlist_api: Arc<NavidromePlaylistApi>,
    startup_validation: Arc<StartupValidationOrchestrator>,
    navidrome_admin: Arc<NavidromeAdmin>,
    library_index: Arc<LibraryIndex>,
    library_scope: ScopeResolver,
    family_pool_service: Arc<FamilyPool>,
    personal_library: Arc<PersonalLibrary>,
    family_write_gate: Arc<FamilyWriteGate>,
}

impl NavidromeServices {
    /// `create_download_directory`: whether to create the download directory (not for handler
    /// tests). `more_validators` run after Subsonic's, in the order given.
    fn build(
        settings: &Arc<SettingsStore>,
        stores: &Stores,
        create_download_directory: bool,
        more_validators: Vec<Arc<dyn IStartupValidator>>,
    ) -> Self {
        let http = http_client_factory::default_client();
        let navidrome_identity = NavidromeIdentityService::new(Arc::clone(settings), http.clone());
        let library_index = Arc::new(LibraryIndex::new(
            navidrome_identity.clone(),
            http.clone(),
            Arc::clone(settings),
            Arc::new(tokio::sync::Notify::new()),
        ));
        let family_store = Arc::clone(&stores.family_store);
        library_index.keep_reading_while(move || family_store.has_personal_libraries());
        let library_scope = ScopeResolver::new(
            Arc::clone(settings),
            Arc::clone(&stores.family_store),
            Arc::clone(&library_index),
        );
        let library = LocalLibraryService::new(
            Arc::clone(settings),
            http.clone(),
            Arc::clone(&stores.external_id_registry),
            navidrome_identity.clone(),
        )
        .with_scan_notice(library_index.scan_notice());
        if create_download_directory {
            library.create_download_directory();
        }
        let local_library: Arc<dyn ILocalLibraryService> = Arc::new(library);
        let mut validators: Vec<Arc<dyn IStartupValidator>> = vec![Arc::new(SubsonicStartupValidator::new(
            // Deliberately the URL Octo started with, not the live one.
            settings.current().subsonic.url.clone(),
            http.clone(),
        ))];
        validators.extend(more_validators);
        // The family pool and members' own libraries. Nothing of it acts while Family is off.
        let configured_download_path = settings
            .raw("Library:DownloadPath")
            .unwrap_or_else(|| "./downloads".to_string());
        let shared_identity = navidrome_identity.clone();
        let family_pool_service = FamilyPool::new(PoolParts {
            settings: Arc::clone(settings),
            store: Arc::clone(&stores.family_pool),
            family: Arc::clone(&stores.family_store),
            index: Arc::clone(&library_index),
            library: Arc::clone(&local_library),
            playlists: Arc::clone(&stores.family_playlists),
            stars: Arc::new(NavidromeMemberStars::new(
                Arc::clone(settings),
                http.clone(),
                Arc::new(MemberCredentials::new(
                    Arc::clone(&stores.family_store),
                    Arc::clone(&stores.family_vault),
                    Arc::clone(settings),
                )),
            )),
            shared_root: Box::new(move || shared_identity.effective_download_path(&configured_download_path)),
        });
        let personal_library = Arc::new(PersonalLibrary::new(
            Arc::clone(settings),
            Arc::clone(&stores.family_store),
            Arc::clone(&library_index),
            Arc::clone(&family_pool_service),
        ));
        let family_write_gate = Arc::new(FamilyWriteGate::new(
            Arc::clone(settings),
            Arc::clone(&stores.family_store),
            Arc::clone(&personal_library),
        ));
        NavidromeServices {
            family_pool_service,
            personal_library,
            family_write_gate,
            subsonic_proxy: SubsonicProxyService::new(Arc::clone(settings)),
            navidrome_song_path_resolver: Arc::new(NavidromeSongPathResolver::new(
                navidrome_identity.clone(),
                Arc::clone(&local_library),
                http.clone(),
                Arc::clone(settings),
            )),
            navidrome_playlist_api: Arc::new(NavidromePlaylistApi::new(
                http.clone(),
                navidrome_identity.clone(),
                Arc::clone(settings),
            )),
            startup_validation: Arc::new(StartupValidationOrchestrator::new(validators)),
            navidrome_admin: Arc::new(NavidromeAdmin::new(
                navidrome_identity.clone(),
                http.clone(),
                Arc::clone(settings),
            )),
            library_index,
            library_scope,
            navidrome_identity,
            local_library,
            http,
        }
    }
}

/// The dashboard sign-in's services: the throttle, the recovery code beside the settings, and
/// the hourly role check.
struct AdminSignIn {
    throttle: Arc<SignInThrottle>,
    recovery_code: Arc<AdminRecoveryCode>,
    role_check: Arc<AdminRoleCheck>,
}

impl AdminSignIn {
    fn build(
        settings: &Arc<SettingsStore>,
        stores: &Stores,
        navidrome: &NavidromeServices,
        config_dir: &Path,
    ) -> AdminSignIn {
        AdminSignIn {
            throttle: Arc::new(SignInThrottle::new()),
            recovery_code: Arc::new(AdminRecoveryCode::new(config_dir.join("admin-recovery-code"))),
            role_check: Arc::new(AdminRoleCheck::new(
                navidrome.navidrome_identity.clone(),
                stores.browse_sessions.clone(),
                navidrome.http.clone(),
                settings.clone(),
            )),
        }
    }
}

/// The stores and clients, over the config directory every state file sits in (the directory
/// of settings.json).
struct Stores {
    download_history: Arc<DownloadHistoryService>,
    /// Told of every outside song whose file lands in the music folder.
    download_arrivals: Arc<DownloadArrivals>,
    download_concurrency: Arc<DownloadConcurrency>,
    download_picks: Arc<DownloadPicks>,
    soulseek_holds: Arc<SoulseekHoldStore>,
    you_tube_resolver: Arc<YouTubeResolver>,
    stream_size_warmer: Arc<StreamSizeWarmer>,
    local_cover_cache: Arc<LocalCoverCache>,
    external_id_registry: Arc<ExternalIdRegistry>,
    radio_queues: Arc<RadioQueueStore>,
    prefetch_store: Arc<PrefetchStore>,
    preview_copies: Arc<PreviewCopies>,
    directory_browser: Arc<DirectoryBrowser>,
    browse_sessions: Arc<BrowseSessionStore>,
    rejected_peers: Arc<RejectedPeerRegistry>,
    release_check: Arc<ReleaseCheck>,
    subsonic_response_builder: Arc<SubsonicResponseBuilder>,
    library_action_journal: Arc<LibraryActionJournal>,
    tag_edit_journal: Arc<TagEditJournal>,
    library_write_gate: LibraryWriteGate,
    upgrade_queue: Arc<UpgradeQueue>,
    quality_upgrade_store: Arc<QualityUpgradeStore>,
    notice_queue: Arc<NoticeQueue>,
    review_sweep_store: Arc<ReviewSweepStore>,
    youtube_check_store: Arc<YoutubeCheckStore>,
    identity_sweep_store: Arc<IdentitySweepStore>,
    lidarr_pending: Arc<LidarrPendingAlbums>,
    family_vault: Arc<FamilyVault>,
    family_store: Arc<FamilyStore>,
    family_requests: Arc<RequestStore>,
    family_saved: Arc<SavedStore>,
    family_playlists: Arc<FamilyPlaylistStore>,
    family_pool: Arc<FamilyPoolStore>,
    listening: Arc<crate::services::listening::ListeningLog>,
}

impl Stores {
    fn build(settings: &Arc<SettingsStore>, config_dir: &Path) -> Stores {
        Self::build_with(settings, config_dir, None)
    }

    /// `registry`: a test's own registry in place of the one over `external-ids.json`.
    fn build_with(
        settings: &Arc<SettingsStore>,
        config_dir: &Path,
        registry: Option<Arc<ExternalIdRegistry>>,
    ) -> Stores {
        let ttl_settings = settings.clone();
        let external_id_registry = registry.unwrap_or_else(|| {
            Arc::new(ExternalIdRegistry::new(Some(
                config_dir.join("external-ids.json"),
            )))
        });
        let you_tube_resolver = Arc::new(YouTubeResolver::new(settings));
        // Every row the response builder sends without a size asks this to learn one.
        let warm_settings = settings.clone();
        let stream_size_warmer = Arc::new(
            StreamSizeWarmer::new(external_id_registry.clone(), you_tube_resolver.clone()).with_per_minute(
                Arc::new(move || {
                    warm_settings
                        .current()
                        .subsonic
                        .effective_stream_size_warm_per_minute()
                }),
            ),
        );
        // The songs fetched ahead: the Prefetch folder, or `prefetch` beside settings.json.
        let prefetch_settings = settings.clone();
        let default_prefetch = config_dir.join("prefetch");
        let prefetch_store = Arc::new(PrefetchStore::new(
            Arc::new(move || {
                prefetch_settings
                    .current()
                    .prefetch
                    .effective_folder()
                    .map_or_else(|| default_prefetch.clone(), PathBuf::from)
            }),
            Clock::system(),
        ));
        // Whole copies of previews, beside settings.json. No new copy starts while the disk has less
        // free than getting songs ahead keeps.
        let copies_settings = settings.clone();
        let preview_copies = PreviewCopies::new(
            config_dir.join("preview-copies"),
            you_tube_resolver.clone(),
            Clock::system(),
            move || copies_settings.current().prefetch.effective_min_free_bytes(),
        );
        let cover_settings = settings.clone();
        let local_cover_cache = Arc::new(LocalCoverCache::new(
            config_dir.join("cover-cache"),
            Arc::new(move || cover_settings.current().subsonic.effective_cover_cache_bytes()),
        ));
        Stores {
            download_history: Arc::new(DownloadHistoryService::new(
                config_dir.join("downloads-history.json"),
            )),
            download_arrivals: Arc::new(DownloadArrivals::default()),
            download_concurrency: Arc::new(DownloadConcurrency::new(settings.clone())),
            download_picks: Arc::new(DownloadPicks::default()),
            soulseek_holds: Arc::new(SoulseekHoldStore::new(Some(
                config_dir.join("soulseek-holds.json"),
            ))),
            you_tube_resolver,
            subsonic_response_builder: Arc::new(SubsonicResponseBuilder::new(
                stream_size_warmer.clone(),
                &settings.current().subsonic,
            )),
            stream_size_warmer,
            local_cover_cache,
            external_id_registry,
            radio_queues: Arc::new(RadioQueueStore::new()),
            prefetch_store,
            preview_copies,
            directory_browser: Arc::new(DirectoryBrowser::new()),
            browse_sessions: Arc::new(BrowseSessionStore::new(Some(
                config_dir.join("browse-sessions.json"),
            ))),
            rejected_peers: Arc::new(RejectedPeerRegistry::new(
                Some(config_dir.join("rejected-peers.json")),
                // Read at every use, so changing the TTL takes effect without a restart.
                Some(Arc::new(move || {
                    ttl_settings.current().soulseek.effective_rejected_peer_ttl_days()
                })),
            )),
            release_check: Arc::new(ReleaseCheck::new(
                config_dir.join("update").join("release.json"),
                settings.clone(),
            )),
            library_action_journal: Arc::new(LibraryActionJournal::with_path(Some(
                config_dir.join("library-actions.json"),
            ))),
            tag_edit_journal: Arc::new(TagEditJournal::new(Some(config_dir.join("tag-edits.json")))),
            library_write_gate: LibraryWriteGate::new(),
            upgrade_queue: Arc::new(UpgradeQueue::with_path(Some(config_dir.join("upgrades.json")))),
            quality_upgrade_store: Arc::new(QualityUpgradeStore::with_path(Some(
                config_dir.join("quality-upgrade.json"),
            ))),
            notice_queue: Arc::new(NoticeQueue::with_path(Some(config_dir.join("notice-queue.json")))),
            review_sweep_store: Arc::new(ReviewSweepStore::new(Some(config_dir.join("review-sweep.json")))),
            youtube_check_store: Arc::new(YoutubeCheckStore::new(Some(
                config_dir.join("youtube-check.json"),
            ))),
            identity_sweep_store: Arc::new(IdentitySweepStore::new(Some(
                config_dir.join("identity-sweep.json"),
            ))),
            lidarr_pending: Arc::new(LidarrPendingAlbums::new(Some(
                config_dir.join("lidarr-pending.json"),
            ))),
            family_vault: Arc::new(FamilyVault::open(config_dir)),
            family_store: Arc::new(FamilyStore::new(Some(config_dir.join("family.json")))),
            family_requests: Arc::new(RequestStore::new(Some(config_dir.join("family-requests.json")))),
            family_saved: Arc::new(SavedStore::new(Some(config_dir.join("family-saved.json")))),
            family_playlists: Arc::new(FamilyPlaylistStore::new(Some(
                config_dir.join("family-playlists.json"),
            ))),
            family_pool: Arc::new(FamilyPoolStore::new(Some(config_dir.join("family-pool.json")))),
            listening: Arc::new(crate::services::listening::ListeningLog::new(
                config_dir.join("listening"),
                settings.clone(),
            )),
        }
    }

    /// The flushers of the registries, journals and queues (each flushes on a timer and once
    /// more on shutdown), and the release check.
    fn register_workers(&self, workers: &WorkerSupervisor) {
        let registry = self.external_id_registry.clone();
        workers.register("ExternalIdRegistry", move |token| {
            registry.clone().run_flusher(token)
        });
        let warmer = self.stream_size_warmer.clone();
        workers.register("StreamSizeWarmer", move |token| warmer.clone().run(token));
        let pin_recheck = Arc::new(PinRecheck::new(
            self.external_id_registry.clone(),
            self.you_tube_resolver.clone(),
        ));
        workers.register("PinRecheck", move |token| pin_recheck.clone().run(token));
        let rejected = self.rejected_peers.clone();
        workers.register("RejectedPeerRegistry", move |token| {
            rejected.clone().run_flusher(token)
        });
        let release_check = self.release_check.clone();
        workers.register("ReleaseCheck", move |token| release_check.clone().run(token));
        let journal = self.library_action_journal.clone();
        workers.register("LibraryActionJournal", move |token| {
            journal.clone().run_flusher(token)
        });
        let notices = self.notice_queue.clone();
        workers.register("NoticeQueue", move |token| notices.clone().run_flusher(token));
        let sweep = self.review_sweep_store.clone();
        workers.register("ReviewSweepStore", move |token| sweep.clone().run_flusher(token));
        let youtube_check = self.youtube_check_store.clone();
        workers.register("YoutubeCheckStore", move |token| {
            youtube_check.clone().run_flusher(token)
        });
        let identities = self.identity_sweep_store.clone();
        workers.register("IdentitySweepStore", move |token| {
            identities.clone().run_flusher(token)
        });
    }
}

/// The metadata clients, built in dependency order: each rate limiter before the client that
/// spends it, and the cover sources before the aggregator that asks them.
struct MetadataClients {
    deezer_rate_limiter: Arc<DeezerRateLimiter>,
    deezer_client: Arc<DeezerRateLimitHandler>,
    deezer_metadata: Arc<DeezerMetadataService>,
    music_brainz: Arc<MusicBrainzClient>,
    acoust_id_rate_limiter: Arc<AcoustIdRateLimiter>,
    acoust_id: Arc<AcoustIdClient>,
    itunes_cover_art: Arc<ITunesCoverArtLookup>,
    cover_art_aggregator: Arc<CoverArtAggregator>,
    cover_art_archive: Arc<CoverArtArchiveLookup>,
    /// The covers the charts named for their songs, served first by the aggregator.
    artwork_hints: Arc<ArtworkHints>,
}

impl MetadataClients {
    /// `itunes_cache`: where the iTunes matches persist, or None to keep them in memory only.
    fn build(settings: &Arc<SettingsStore>, itunes_cache: Option<PathBuf>) -> Self {
        Self::build_with(settings, itunes_cache, None)
    }

    /// `deezer_base_url`: a test's mock server in place of api.deezer.com.
    fn build_with(
        settings: &Arc<SettingsStore>,
        itunes_cache: Option<PathBuf>,
        deezer_base_url: Option<&str>,
    ) -> Self {
        let deezer_rate_limiter = Arc::new(DeezerRateLimiter::new());
        let deezer_client = Arc::new(DeezerRateLimitHandler::new(Arc::clone(&deezer_rate_limiter)));
        let deezer_metadata = Arc::new(match deezer_base_url {
            Some(base) => {
                DeezerMetadataService::with_base_url(Arc::clone(&deezer_client), Arc::clone(settings), base)
            }
            None => DeezerMetadataService::new(Arc::clone(&deezer_client), Arc::clone(settings)),
        });
        let acoust_id_rate_limiter = Arc::new(AcoustIdRateLimiter::new());
        let acoust_id = Arc::new(AcoustIdClient::new(Arc::new(AcoustIdRateLimitHandler::new(
            Arc::clone(&acoust_id_rate_limiter),
        ))));

        // The cover sources take their settings once: captured here, at construction.
        let current = settings.current();
        let itunes_cover_art = Arc::new(ITunesCoverArtLookup::new(itunes_cache));
        // Cover-art sources, in fallback order. The aggregator queries them sequentially; adding
        // or removing a source is a one-line change here.
        let artwork_hints = Arc::new(ArtworkHints::new());
        let sources: Vec<Arc<dyn ICoverArtSource>> = vec![
            // A chart's own artwork first: it names the cover exactly; the rest search by name.
            Arc::new(HintedArtwork::new(Arc::clone(&artwork_hints))) as Arc<dyn ICoverArtSource>,
            Arc::new(DeezerCoverArtLookup::new(
                Arc::clone(&deezer_client),
                &current.metadata,
            )),
            Arc::clone(&itunes_cover_art) as Arc<dyn ICoverArtSource>,
            Arc::new(LastFmCoverArtLookup::new(&current.last_fm, &current.metadata)),
        ];

        Self {
            deezer_rate_limiter,
            deezer_client,
            deezer_metadata,
            music_brainz: Arc::new(MusicBrainzClient::new()),
            acoust_id_rate_limiter,
            acoust_id,
            itunes_cover_art,
            cover_art_aggregator: Arc::new(CoverArtAggregator::new(sources)),
            cover_art_archive: Arc::new(CoverArtArchiveLookup::new()),
            artwork_hints,
        }
    }
}

/// The lyrics services. `config_dir` None keeps the pins and the undo journal in memory (handler
/// tests).
struct Lyrics {
    choice_store: Arc<LyricsChoiceStore>,
    service: Arc<LyricsService>,
    choice_service: Arc<LyricsChoiceService>,
    sidecar_writer: Arc<LyricsSidecarWriter>,
    undo_journal: Arc<LyricsUndoJournal>,
    /// The lyrics and covers people turned down, `<config>/rejections.json`.
    rejections: Arc<RejectionStore>,
}

impl Lyrics {
    fn build(settings: &Arc<SettingsStore>, config_dir: Option<&Path>) -> Lyrics {
        let sources = LyricsService::default_sources(
            lyrics_http::lyrics_http_client(),
            lyrics_http::kugou_http_client(),
        );
        let service = Arc::new(LyricsService::new(sources, settings.clone()));
        let choice_store = Arc::new(LyricsChoiceStore::new(
            config_dir.map(|dir| dir.join("lyrics-choices.json")),
        ));
        // The writer is given the library service (for the scan after writing inside songs)
        // with `set_library` once that is built.
        let sidecar_writer = Arc::new(LyricsSidecarWriter::new(service.clone(), settings.clone()));
        let undo_journal = Arc::new(LyricsUndoJournal::new(
            config_dir.map(|dir| dir.join("lyrics-undo.jsonl")),
        ));
        sidecar_writer.set_undo_journal(undo_journal.clone());
        Lyrics {
            choice_service: Arc::new(LyricsChoiceService::new(service.clone(), choice_store.clone())),
            choice_store,
            service,
            sidecar_writer,
            undo_journal,
            rejections: Arc::new(RejectionStore::new(
                config_dir.map(|dir| dir.join("rejections.json")),
            )),
        }
    }

    /// What Wrong lyrics and Wrong cover work with.
    fn song_answers(&self, covers: &Arc<CoverUpgradeWorker>) -> SongAnswers {
        SongAnswers {
            lyrics: self.service.clone(),
            choices: self.choice_service.clone(),
            sidecars: self.sidecar_writer.clone(),
            covers: covers.clone(),
            rejections: self.rejections.clone(),
        }
    }

    /// The writer held in the state is the same instance the worker runs, so downloads enqueue
    /// into the running worker.
    fn register_workers(&self, workers: &WorkerSupervisor) {
        let writer = self.sidecar_writer.clone();
        workers.register("LyricsSidecarWriter", move |stopping| {
            writer.clone().run(stopping)
        });
    }

    /// The library job, over the library service and the music folder, which exist only once
    /// Navidrome's services are built. `config_dir` None keeps the run in memory.
    fn library_job(
        &self,
        settings: &Arc<SettingsStore>,
        config_dir: Option<&Path>,
        navidrome: &NavidromeServices,
    ) -> LyricsLibraryJob {
        let store = Arc::new(LyricsLibraryStore::new(
            config_dir.map(|dir| dir.join("lyrics-library.json")),
        ));
        let resolver = navidrome.navidrome_song_path_resolver.clone();
        let worker = Arc::new(
            LyricsLibraryWorker::new(
                store.clone(),
                self.sidecar_writer.clone(),
                settings.clone(),
                navidrome.local_library.clone(),
                move || resolver.music_root(),
                self.undo_journal.clone(),
            )
            .with_navidrome(
                Arc::new(navidrome.navidrome_identity.clone()),
                navidrome.http.clone(),
            ),
        );
        LyricsLibraryJob { store, worker }
    }
}

/// `LyricsLibraryStore` and `LyricsLibraryWorker`.
struct LyricsLibraryJob {
    store: Arc<LyricsLibraryStore>,
    worker: Arc<LyricsLibraryWorker>,
}

impl LyricsLibraryJob {
    /// The store's flusher (on a timer and once more on shutdown), and the worker, the same
    /// instance the state holds: the dashboard enqueues into the running loop.
    fn register_workers(&self, workers: &WorkerSupervisor) {
        let store = self.store.clone();
        workers.register("LyricsLibraryStore", move |stopping| {
            store.clone().run_flusher(stopping)
        });
        let worker = self.worker.clone();
        workers.register("LyricsLibraryWorker", move |stopping| {
            worker.clone().run(stopping)
        });
    }
}

/// "Tidy artists", over the same services the downloads and the other library jobs use, so the
/// records of a moved file follow it. `config_dir` None keeps its run, plan and journal in
/// memory (handler tests).
fn build_tidy(
    settings: &Arc<SettingsStore>,
    config_dir: Option<&Path>,
    navidrome: &NavidromeServices,
    clients: &MetadataClients,
    stores: &Stores,
    jobs: &GenreAndCoverJobs,
    lyrics_undo: &Arc<LyricsUndoJournal>,
) -> Arc<TidyWorker> {
    let (identity, http, list_settings) = (
        navidrome.navidrome_identity.clone(),
        navidrome.http.clone(),
        settings.clone(),
    );
    let list_songs: ListSongs = Arc::new(move || {
        let (identity, http, settings) = (identity.clone(), http.clone(), list_settings.clone());
        Box::pin(async move {
            let base = settings.current().subsonic.url.clone();
            let root = settings
                .raw("Library:DownloadPath")
                .unwrap_or_else(|| "./downloads".to_string());
            navidrome_song_list::list(Some(&identity), Some(&http), base.as_deref(), &root).await
        })
    });
    Arc::new(
        TidyWorker::new(
            Arc::new(TidyStore::new(
                config_dir.map(|dir| dir.join("tidy-run.json")),
                config_dir.map(|dir| dir.join("tidy-plan.json")),
            )),
            Arc::new(TidyJournal::new(
                config_dir.map(|dir| dir.join("tidy-journal.jsonl")),
            )),
            settings.clone(),
            TidyServices {
                library: Some(navidrome.local_library.clone()),
                music_brainz: Some(clients.music_brainz.clone()),
                notices: Some(stores.notice_queue.clone()),
                genre_journal: Some(jobs.genre_backfill_journal.clone()),
                tag_edits: Some(stores.tag_edit_journal.clone()),
                actions: Some(stores.library_action_journal.clone()),
                history: Some(stores.download_history.clone()),
                list_songs: Some(list_songs),
                cover_journal: Some(jobs.cover_upgrade_journal.clone()),
                lyrics_undo: Some(lyrics_undo.clone()),
                explicit: Some(jobs.explicit_backfill_store.clone()),
                quarantine: Some(Arc::new(LibraryActionQuarantine::new(settings.clone()))),
            },
        )
        .with_write_gate(stores.library_write_gate.clone()),
    )
}

/// The genre backfill and the cover upgrade. `config_dir` None keeps their runs and journals in
/// memory (handler tests).
struct GenreAndCoverJobs {
    genre_backfill_store: Arc<GenreBackfillStore>,
    genre_backfill_journal: Arc<GenreBackfillJournal>,
    genre_backfill_worker: Arc<GenreBackfillWorker>,
    album_cover_finder: Arc<dyn IAlbumCoverFinder>,
    cover_upgrade_store: Arc<CoverUpgradeStore>,
    cover_upgrade_journal: Arc<CoverUpgradeJournal>,
    cover_upgrade_worker: Arc<CoverUpgradeWorker>,
    explicit_backfill_store: Arc<ExplicitBackfillStore>,
    explicit_backfill: Arc<ExplicitBackfill>,
}

impl GenreAndCoverJobs {
    fn build(
        settings: &Arc<SettingsStore>,
        config_dir: Option<&Path>,
        navidrome: &NavidromeServices,
        clients: &MetadataClients,
        tag_edit_journal: &Arc<TagEditJournal>,
        write_gate: &LibraryWriteGate,
    ) -> GenreAndCoverJobs {
        // Genre backfill state lives beside the other config-dir files so a run survives a
        // restart and can be resumed deliberately rather than silently restarting.
        let genre_backfill_store = Arc::new(GenreBackfillStore::new(
            config_dir.map(|dir| dir.join("genre-backfill.json")),
        ));
        let genre_backfill_journal = Arc::new(GenreBackfillJournal::new(
            config_dir.map(|dir| dir.join("genre-backfill-journal.jsonl")),
        ));
        let genre_backfill_worker = Arc::new(
            GenreBackfillWorker::new(
                genre_backfill_store.clone(),
                genre_backfill_journal.clone(),
                settings.clone(),
                Some(navidrome.local_library.clone()),
            )
            .with_write_gate(write_gate.clone()),
        );

        // The cover upgrade: same shape as the genre backfill. Its journal keeps every replaced
        // picture (once per distinct picture, in cover-backups/) so a run can be undone.
        let cover_upgrade_store = Arc::new(CoverUpgradeStore::new(
            config_dir.map(|dir| dir.join("cover-upgrade.json")),
        ));
        let cover_upgrade_journal = Arc::new(CoverUpgradeJournal::new(
            config_dir.map(|dir| dir.join("cover-upgrade-journal.jsonl")),
        ));
        let album_cover_finder: Arc<dyn IAlbumCoverFinder> = Arc::new(AlbumCoverFinder::new(
            clients.itunes_cover_art.clone(),
            clients.cover_art_archive.clone(),
            clients.deezer_metadata.clone(),
            navidrome.http.clone(),
        ));
        let cover_upgrade_worker = Arc::new(CoverUpgradeWorker::new(
            cover_upgrade_store.clone(),
            cover_upgrade_journal.clone(),
            album_cover_finder.clone(),
            settings.clone(),
            CoverUpgradeServices {
                library: Some(navidrome.local_library.clone()),
                identity: Some(Arc::new(navidrome.navidrome_identity.clone())),
                http: Some(navidrome.http.clone()),
                write_gate: Some(write_gate.clone()),
            },
        ));
        // Marking the library explicit or clean: a preview, then the writes it found, journaled
        // in tag-edits.json like the apps' edits so a run (or one song) can be undone.
        let explicit_backfill_store = Arc::new(ExplicitBackfillStore::new(
            config_dir.map(|dir| dir.join("explicit-backfill.json")),
        ));
        let explicit_backfill = Arc::new(
            ExplicitBackfill::new(
                explicit_backfill_store.clone(),
                tag_edit_journal.clone(),
                Arc::new(DeezerExplicitCatalog::new(clients.deezer_metadata.clone())),
                settings.clone(),
                ExplicitBackfillServices {
                    identity: Some(Arc::new(navidrome.navidrome_identity.clone())),
                    http: Some(navidrome.http.clone()),
                    library: Some(navidrome.local_library.clone()),
                },
            )
            .with_write_gate(write_gate.clone())
            .with_library_scope(navidrome.library_scope.clone()),
        );
        GenreAndCoverJobs {
            genre_backfill_store,
            genre_backfill_journal,
            genre_backfill_worker,
            album_cover_finder,
            cover_upgrade_store,
            cover_upgrade_journal,
            cover_upgrade_worker,
            explicit_backfill_store,
            explicit_backfill,
        }
    }

    /// The stores' flushers (on a timer and once more on shutdown), and the workers, the same
    /// instances the state holds, so the controllers enqueue into the running loops rather than
    /// a second copy of them.
    fn register_workers(&self, workers: &WorkerSupervisor) {
        let store = self.genre_backfill_store.clone();
        workers.register("GenreBackfillStore", move |stopping| {
            let store = store.clone();
            async move {
                store.run_flusher(stopping).await;
                Ok(())
            }
        });
        let worker = self.genre_backfill_worker.clone();
        workers.register("GenreBackfillWorker", move |stopping| {
            worker.clone().run(stopping)
        });
        let store = self.cover_upgrade_store.clone();
        workers.register("CoverUpgradeStore", move |stopping| {
            let store = store.clone();
            async move {
                store.run_flusher(stopping).await;
                Ok(())
            }
        });
        let worker = self.cover_upgrade_worker.clone();
        workers.register("CoverUpgradeWorker", move |stopping| worker.clone().run(stopping));
        let store = self.explicit_backfill_store.clone();
        workers.register("ExplicitBackfillStore", move |stopping| {
            let store = store.clone();
            async move {
                store.run_flusher(stopping).await;
                Ok(())
            }
        });
        let worker = self.explicit_backfill.clone();
        workers.register("ExplicitBackfill", move |stopping| worker.clone().run(stopping));
    }
}

/// Imports: the import service and the trickle that downloads into the library.
struct Imports {
    service: Arc<crate::services::imports::ImportService>,
    trickle: Arc<crate::services::imports::TrickleWorker>,
}

impl Imports {
    /// `config_dir`: where the two files live; None keeps them in memory (endpoint tests).
    fn build(
        settings: &Arc<SettingsStore>,
        config_dir: Option<&Path>,
        stores: &Stores,
        navidrome: &NavidromeServices,
        acquisition: &Acquisition,
        soulseek_link: Arc<dyn ISoulseekLink>,
        music_brainz: Option<Arc<MusicBrainzClient>>,
    ) -> Imports {
        use crate::services::imports::{
            ImportMatcher, ImportParts, ImportPlaylists, ImportService, ImportStore, NavidromePlaylists,
            SpotifyLinkReader, TrickleQueue, TrickleServices, TrickleWorker,
        };
        let file = |name: &str| config_dir.map(|dir| dir.join(name));
        let queue = Arc::new(TrickleQueue::new(file("imports-trickle.json")));
        let matcher = Arc::new({
            let matcher = ImportMatcher::new(navidrome.navidrome_playlist_api.clone())
                .with_library_scope(navidrome.library_scope.clone())
                .with_isrc_file(file("imports-isrc.json"));
            match music_brainz {
                Some(music_brainz) => matcher.with_isrc_recordings(
                    crate::services::imports::import_matcher::isrc_recordings_from(music_brainz),
                ),
                None => matcher,
            }
        });
        let trickle = TrickleWorker::wired(
            queue.clone(),
            settings.clone(),
            TrickleServices {
                owned: acquisition.library_ownership.clone(),
                matcher: Some(matcher.clone()),
                ids: stores.external_id_registry.clone(),
                hearts: acquisition.heart_acquisition_coordinator.clone(),
                downloads: acquisition.track_acquisition_queue.clone(),
                tracker: acquisition.acquisition_tracker.clone(),
                soulseek: soulseek_link,
            },
        );
        let service = ImportService::new(ImportParts {
            store: Arc::new(ImportStore::new(file("imports.json"))),
            queue,
            trickle: trickle.clone(),
            matcher,
            playlists: ImportPlaylists::new(Arc::new(NavidromePlaylists {
                api: navidrome.navidrome_playlist_api.clone(),
                identity: navidrome.navidrome_identity.clone(),
            })),
            links: Arc::new(SpotifyLinkReader::new(SpotifyLinkReader::client())),
            settings: settings.clone(),
            tracker: Some(acquisition.acquisition_tracker.clone()),
        });
        let weak = Arc::downgrade(&service);
        stores
            .external_id_registry
            .set_recovery(Arc::new(move |id| weak.upgrade()?.preview_routing(id)));
        Imports { service, trickle }
    }

    fn register_workers(&self, workers: &WorkerSupervisor) {
        let trickle = self.trickle.clone();
        workers.register("TrickleWorker", move |stopping| {
            trickle.clone().run_loop(stopping)
        });
        let service = self.service.clone();
        workers.register("ImportService", move |stopping| {
            service.clone().run_loop(stopping)
        });
    }
}

/// The Soulseek services, over the stores and clients they read.
struct SoulseekServices {
    client: SoulseekClient,
    link: Arc<SoulseekLink>,
    port_check: Arc<SoulseekPortCheck>,
    share_switch: Arc<SoulseekShareSwitch>,
    sharing: Arc<SoulseekSharing>,
    metadata: Arc<SoulseekMetadataService>,
    validator: Arc<dyn IStartupValidator>,
}

impl SoulseekServices {
    fn build(
        settings: &Arc<SettingsStore>,
        stores: &Stores,
        clients: &MetadataClients,
        last_fm: &Arc<LastFmService>,
    ) -> Self {
        // The client deliberately keeps the address and login Octo started with.
        let client = SoulseekClient::new(&settings.current().soulseek);
        let slskd: Arc<dyn ISlskdShares> = Arc::new(client.clone());
        let port_check = Arc::new(SoulseekPortCheck::new(Arc::clone(settings)));
        let share_switch = Arc::new(SoulseekShareSwitch::new(slskd.clone(), Arc::clone(settings)));
        SoulseekServices {
            sharing: Arc::new(SoulseekSharing::new(
                slskd,
                port_check.clone(),
                share_switch.clone(),
                Arc::clone(settings),
            )),
            port_check,
            share_switch,
            link: Arc::new(SoulseekLink::new(client.clone(), Arc::clone(settings))),
            metadata: Arc::new(SoulseekMetadataService::new(
                Arc::clone(&stores.you_tube_resolver),
                Arc::clone(&stores.external_id_registry),
                Arc::clone(&clients.deezer_metadata),
                Arc::clone(&clients.cover_art_aggregator),
                Some(Arc::clone(last_fm) as Arc<dyn LastFmTrackLengths>),
            )),
            validator: Arc::new(SoulseekStartupValidator::new(settings, client.clone())),
            client,
        }
    }
}

/// Last.fm radio and the generated mixes.
struct Radio {
    state: Arc<LastFmRadioStateStore>,
    refresh_queue: Arc<LastFmRadioRefreshQueue>,
    sessions: Arc<LastFmRadioStreamSessionStore>,
    cache: Arc<LastFmRadioTrackCache>,
    recommendations: Arc<LastFmRadioRecommendationService>,
    streams: LastFmRadioStreamService,
    warmup: Arc<LastFmRadioWarmupService>,
    refresh_worker: Arc<LastFmRadioRefreshWorker>,
    generated_playlists: Arc<GeneratedPlaylistService>,
    sources: Arc<RadioSourceSet>,
    song_radio: Arc<SongRadioService>,
    outcomes: Arc<RadioOutcomeStore>,
    sonic_store: Arc<SonicStore>,
    sonic_worker: Arc<SonicAnalysisWorker>,
}

impl Radio {
    fn build(
        settings: &Arc<SettingsStore>,
        config_dir: &Path,
        stores: &Stores,
        navidrome: &NavidromeServices,
        integrations: &Integrations,
        music_metadata: Arc<dyn IMusicMetadataService>,
        downloads: Arc<dyn IDownloadService>,
        overrides: &TestServices,
        deezer: &Arc<DeezerMetadataService>,
        acquisition: &Acquisition,
        music_brainz: &Arc<MusicBrainzClient>,
    ) -> Radio {
        // Where radio's state files go: Octo:StateDirectory when set (tests point it at their own
        // folder), else beside the settings.
        let state_dir = settings
            .raw("Octo:StateDirectory")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| config_dir.to_path_buf());
        // What radio learns from listening: which source's songs each listener plays through or
        // skips.
        let outcomes = Arc::new(RadioOutcomeStore::new(
            state_dir.join("radio-outcomes.json"),
            settings.clone(),
        ));
        // Sounds alike (multi-source radio): what every library song sounds like, from octo-sonic.
        let sonic_store = Arc::new(SonicStore::new(state_dir.join("sonic-features.json")));
        let sonic_client: Arc<dyn ISonicClient> = overrides
            .sonic_client
            .clone()
            .unwrap_or_else(|| Arc::new(SonicClient::new(settings.clone())));
        // Each song's transition profile, made in the same reading.
        let transition_store = Arc::new(TransitionStore::new(&state_dir));
        let sonic_worker = SonicAnalysisWorker::new(
            sonic_store.clone(),
            transition_store,
            sonic_client,
            Arc::new(NavidromeSonicLibrary::new(
                navidrome.navidrome_playlist_api.clone(),
                navidrome.navidrome_song_path_resolver.clone(),
            )),
            acquisition.acquisition_activity.clone(),
            settings.clone(),
            Some(&acquisition.acquisition_tracker),
            Clock::system(),
        );
        sonic_worker.listen_to(&stores.download_arrivals);
        sonic_worker.use_published_tempo(Arc::new(DeezerPublishedTempo::new(Arc::clone(deezer))));
        // Radio's suggestion sources, asked together and blended (multi-source radio).
        let listen_brainz = Arc::new(match &overrides.listen_brainz_radio_urls {
            Some((labs, api)) => ListenBrainzRadioClient::with_urls(settings.clone(), labs, api)
                .with_pacing(std::time::Duration::ZERO, ListenBrainzRadioClient::MAX_QUEUE_WAIT),
            None => ListenBrainzRadioClient::new(settings.clone()),
        });
        let source_list: Vec<Arc<dyn IRadioSource>> = vec![
            Arc::new(LastFmRadioSource::new(integrations.last_fm.clone())),
            Arc::new(YouTubeMusicRadioSource::new(
                Arc::new(YouTubeMusicClient::new(settings)),
                settings.clone(),
            )),
            Arc::new(ListenBrainzRadioSource::new(listen_brainz, settings.clone())),
            Arc::new(SoundsAlikeRadioSource::new(sonic_store.clone(), settings.clone())),
        ];
        let sources = Arc::new(RadioSourceSet::new(
            source_list,
            settings.clone(),
            Some(outcomes.clone()),
        ));
        let song_radio = Arc::new(
            SongRadioService::new(
                sources.clone(),
                settings.clone(),
                Some(sonic_store.clone()),
                Some(outcomes.clone()),
            )
            .with_family(stores.family_store.clone()),
        );
        let state = Arc::new(LastFmRadioStateStore::new(
            config_dir.join("lastfm-radio-state.json"),
            settings.clone(),
            stores.external_id_registry.clone(),
        ));
        let refresh_queue = Arc::new(LastFmRadioRefreshQueue::new());
        let sessions = Arc::new(LastFmRadioStreamSessionStore::new());
        let cache = Arc::new(match &overrides.radio_cache_dir {
            Some(dir) => LastFmRadioTrackCache::with_root(dir.clone()),
            None => LastFmRadioTrackCache::new(),
        });
        let recommendations = Arc::new(LastFmRadioRecommendationService::new(
            integrations.last_fm.clone(),
            state.clone(),
            settings.clone(),
            Some(sources.clone()),
        ));
        let streams = LastFmRadioStreamService::new(LastFmRadioStreamServiceParts {
            state: state.clone(),
            settings: settings.clone(),
            library: navidrome.local_library.clone(),
            proxy: navidrome.subsonic_proxy.clone(),
            downloads,
            transcoder: overrides
                .radio_transcoder
                .clone()
                .unwrap_or_else(|| Arc::new(FfmpegLastFmRadioAudioTranscoder)),
            cache: cache.clone(),
            sessions: sessions.clone(),
            registry: stores.external_id_registry.clone(),
            metadata: music_metadata,
            queues: stores.radio_queues.clone(),
            refresh_queue: refresh_queue.clone(),
            tune_in: overrides
                .radio_tune_in
                .clone()
                .unwrap_or_else(|| Arc::new(RandomRadioTuneInSelector)),
            last_fm: Some(integrations.last_fm.clone()),
            listen_brainz: Some(integrations.listen_brainz.clone()),
            last_fm_scrobbles: Some(integrations.last_fm_scrobbles.clone()),
            recommendations: Some(recommendations.clone()),
        });
        streams.use_listening(stores.listening.clone());
        let warmup = Arc::new(LastFmRadioWarmupService::new(streams.clone(), state.clone()));
        let refresh_worker = Arc::new(LastFmRadioRefreshWorker::new(
            refresh_queue.clone(),
            recommendations.clone(),
            state.clone(),
            settings.clone(),
            Some(warmup.clone()),
        ));
        // Made for you's New Releases: the listener's artists found in the catalog, and their latest.
        let new_releases = Arc::new(NewReleasesBuilder::new(
            Arc::clone(deezer),
            stores.external_id_registry.clone(),
        ));
        // Country mixes: one walk of the library as the Navidrome admin, shared by every
        // listener, and where its artists are from, learned from MusicBrainz in the background.
        let identity = navidrome.navidrome_identity.clone();
        let country_library = Arc::new(CountryLibrary::new(
            Arc::new(ArtistCountryLookup::new(
                Arc::clone(music_brainz) as Arc<dyn artist_country::CountrySource>,
                Some(config_dir.join(artist_country::STORE_FILE)),
            )),
            navidrome.subsonic_proxy.clone(),
            Arc::new(move || {
                identity.get_scan_auth().map(|(user, token, salt)| {
                    [
                        ("u", user),
                        ("t", token),
                        ("s", salt),
                        ("v", "1.16.1".to_string()),
                        ("c", "octo".to_string()),
                    ]
                    .into_iter()
                    .map(|(key, value)| (key.to_string(), value))
                    .collect()
                })
            }),
        ));
        let generated_playlists = Arc::new(
            GeneratedPlaylistService::new(
                Some(config_dir.join("generated-playlists.json")),
                navidrome.subsonic_proxy.clone(),
                settings.clone(),
            )
            .with_for_you(ForYouServices {
                builder: new_releases,
                responses: stores.subsonic_response_builder.clone(),
            })
            .with_countries(country_library)
            .with_family(stores.family_store.clone())
            .with_last_fm(integrations.last_fm.clone()),
        );
        Radio {
            state,
            refresh_queue,
            sessions,
            cache,
            recommendations,
            streams,
            warmup,
            refresh_worker,
            generated_playlists,
            sources,
            song_radio,
            outcomes,
            sonic_store,
            sonic_worker,
        }
    }

    /// The radio warmup (the instance the state holds), the refresh worker, the Sounds alike
    /// analysis and the outcome store's flusher.
    fn register_workers(&self, workers: &WorkerSupervisor) {
        let warmup = self.warmup.clone();
        workers.register("LastFmRadioWarmupService", move |stopping| {
            warmup.clone().run(stopping)
        });
        let refresh = self.refresh_worker.clone();
        workers.register("LastFmRadioRefreshWorker", move |stopping| {
            refresh.clone().run(stopping)
        });
        let sonic = self.sonic_worker.clone();
        workers.register("SonicAnalysisWorker", move |stopping| sonic.clone().run(stopping));
        let outcomes = self.outcomes.clone();
        workers.register("RadioOutcomeStore", move |stopping| {
            outcomes.clone().run_flusher(stopping)
        });
    }
}

/// Library actions: the quarantine, the executor and its two workers, over the journal
/// the stores hold.
struct LibraryActions {
    notice_queue: Arc<NoticeQueue>,
    quarantine: Arc<LibraryActionQuarantine>,
    executor: Arc<LibraryActionExecutor>,
    rating_worker: Arc<LibraryActionRatingWorker>,
    playlist_worker: Arc<LibraryActionPlaylistWorker>,
}

impl LibraryActions {
    fn build(
        settings: &Arc<SettingsStore>,
        stores: &Stores,
        navidrome: &NavidromeServices,
        acquisition: &Acquisition,
        soulseek_link: Arc<dyn ISoulseekLink>,
        spectrum: Arc<SpectrumAnalyzer>,
        answers: SongAnswers,
    ) -> Self {
        let notice_queue = stores.notice_queue.clone();
        let quarantine = Arc::new(LibraryActionQuarantine::new(settings.clone()));
        let executor = Arc::new(LibraryActionExecutor::new(LibraryActionExecutorParts {
            resolver: navidrome.navidrome_song_path_resolver.clone(),
            quarantine: quarantine.clone(),
            journal: stores.library_action_journal.clone(),
            library: navidrome.local_library.clone(),
            ids: stores.external_id_registry.clone(),
            rejected_peers: stores.rejected_peers.clone(),
            acquisitions: acquisition.track_acquisition_queue.clone(),
            settings: settings.clone(),
            notices: Some(notice_queue.clone()),
            // The same spectrum analyzer the downloads use.
            spectrum: Some(spectrum),
            stars: Some(acquisition.star_on_arrival.clone()),
            soulseek_link: Some(soulseek_link),
            sources: Some(acquisition.upgrade_sources.clone()),
            answers: Some(answers),
        }));
        let rating_worker = Arc::new(LibraryActionRatingWorker::new(
            executor.clone(),
            navidrome.subsonic_proxy.clone(),
            settings.clone(),
        ));
        let playlist_worker = Arc::new(
            LibraryActionPlaylistWorker::new(
                executor.clone(),
                stores.library_action_journal.clone(),
                quarantine.clone(),
                navidrome.navidrome_song_path_resolver.clone(),
                navidrome.navidrome_identity.clone(),
                navidrome.navidrome_playlist_api.clone(),
                settings.clone(),
            )
            .with_missing_rows(Arc::new(MissingRows::new(navidrome.navidrome_admin.clone()))),
        );
        LibraryActions {
            notice_queue,
            quarantine,
            executor,
            rating_worker,
            playlist_worker,
        }
    }

    /// The playlist worker and the rating worker, the latter the same instance the state holds.
    fn register_workers(&self, workers: &WorkerSupervisor) {
        let playlists = self.playlist_worker.clone();
        workers.register("LibraryActionPlaylistWorker", move |stopping| {
            playlists.clone().run(stopping)
        });
        let ratings = self.rating_worker.clone();
        workers.register("LibraryActionRatingWorker", move |stopping| {
            ratings.clone().run(stopping)
        });
    }
}

/// The charts: Apple Music's pages, the chart service over them, the chart playlists, and the
/// worker that fetches the charts at start and at each new period.
struct Charts {
    top_songs: TopSongsService,
    playlists: Arc<ChartPlaylistService>,
    library_matches: Arc<LibraryMatchCache>,
    warm: Arc<ChartsWarmWorker>,
    animated_art: Arc<AnimatedArt>,
}

impl Charts {
    #[allow(clippy::too_many_arguments)]
    fn build(
        settings: &Arc<SettingsStore>,
        stores: &Stores,
        clients: &MetadataClients,
        integrations: &Integrations,
        navidrome: &NavidromeServices,
        music_metadata: Arc<dyn IMusicMetadataService>,
        apple_base_url: Option<&str>,
        config_dir: &Path,
    ) -> Charts {
        let apple = Arc::new(match apple_base_url {
            Some(base) => {
                AppleMusicPages::with_base_url(base, crate::services::charts::apple_music_pages::INTERVAL)
            }
            None => AppleMusicPages::new(),
        });
        let animated_art = Arc::new(AnimatedArt::new(
            settings.clone(),
            Arc::clone(&clients.itunes_cover_art),
            Arc::clone(&apple),
            Some(config_dir.join("animated-art.json")),
        ));
        let top_songs = TopSongsService::new(
            (*clients.deezer_metadata).clone(),
            Some(integrations.last_fm.clone()),
            music_metadata,
            stores.external_id_registry.clone(),
            apple,
            Arc::clone(&clients.artwork_hints),
            settings.clone(),
        )
        .with_listeners(Arc::new(ChartCountryStore::new(Some(
            config_dir.join("chart-countries.json"),
        ))))
        .with_genre_names(Arc::new(GenreNames::new(Some(
            config_dir.join("genre-names.json"),
        ))));
        let library_matches = Arc::new(LibraryMatchCache::new());
        let playlists = ChartPlaylistService::new(
            top_songs.clone(),
            Arc::clone(&stores.subsonic_response_builder),
            stores.external_id_registry.clone(),
            navidrome.subsonic_proxy.clone(),
            settings.clone(),
            Clock::system(),
        )
        .with_family(stores.family_store.clone())
        .with_library_matches(Arc::clone(&library_matches));
        let warm = Arc::new(
            ChartsWarmWorker::new(top_songs.clone(), playlists.clone(), Clock::system())
                .with_people((*navidrome.navidrome_admin).clone()),
        );
        Charts {
            top_songs,
            playlists: Arc::new(playlists),
            library_matches,
            warm,
            animated_art,
        }
    }

    fn register_workers(&self, workers: &WorkerSupervisor) {
        let warm = self.warm.clone();
        workers.register("ChartsWarmWorker", move |stopping| warm.clone().run(stopping));
        let animated = self.animated_art.clone();
        workers.register("AnimatedArt", move |stopping| {
            let animated = animated.clone();
            async move { animated.run_flush_loop(stopping).await }
        });
    }
}

/// What a handler test swaps for fakes. Anything left `None` is the production service.
#[derive(Default)]
pub struct TestServices {
    /// `IMusicMetadataService` (a test's mock).
    pub metadata: Option<Arc<dyn IMusicMetadataService>>,
    /// Where `LastFmService` sends its calls (the fake `ws.audioscrobbler.com`).
    pub last_fm_base_url: Option<String>,
    /// `ILastFmRadioAudioTranscoder`.
    pub radio_transcoder: Option<Arc<dyn ILastFmRadioAudioTranscoder>>,
    /// `IRadioTuneInSelector`.
    pub radio_tune_in: Option<Arc<dyn IRadioTuneInSelector>>,
    /// `LastFmRadioTrackCache`'s folder.
    pub radio_cache_dir: Option<PathBuf>,
    /// The config folder the state files go in (a fresh temp folder when `None`).
    pub config_dir: Option<PathBuf>,
    /// Where `DeezerMetadataService` sends its calls (the fake `api.deezer.com`).
    pub deezer_base_url: Option<String>,
    /// `ExternalIdRegistry`, when a test's own metadata service shares it.
    pub external_id_registry: Option<Arc<ExternalIdRegistry>>,
    /// Raw configuration keys, such as `YouTube:ShimUrl`.
    pub raw_settings: Vec<(String, String)>,
    /// `IDownloadService` (the real one is `SoulseekDownloadService`).
    pub download_service: Option<Arc<dyn IDownloadService>>,
    /// `LibrarySnapshot`, with the test's own song pages.
    pub library_snapshot: Option<Arc<LibrarySnapshot>>,
    /// `IAlbumCoverFinder` as the library edits ask it (a fixed finder).
    pub album_cover_finder: Option<Arc<dyn IAlbumCoverFinder>>,
    /// octo-sonic (a fake sidecar).
    pub sonic_client: Option<Arc<dyn ISonicClient>>,
    /// Where `ListenBrainzRadioClient` sends its labs and API calls, unpaced.
    pub listen_brainz_radio_urls: Option<(String, String)>,
    /// `Admin:SignIn`. None runs the test with the sign-in off, for the tests of what an
    /// endpoint does rather than who may call it; the sign-in tests set it.
    pub sign_in: Option<String>,
    /// Where Apple Music's pages are read from (a test's mock server). None in a handler test
    /// is an address that refuses at once, so no test reaches music.apple.com.
    pub apple_music_base_url: Option<String>,
}

/// The queues and sweeps: the upgrade queue's worker, the weekly quality upgrade, the
/// notice playlists, the duplicate scan and the library Review sweep, over the stores the
/// [`Stores`] hold.
struct LibraryJobs {
    upgrade_worker: Arc<UpgradeWorker>,
    quality_upgrade_worker: Arc<QualityUpgradeWorker>,
    notice_playlist_worker: Arc<NoticePlaylistWorker>,
    duplicate_scan_worker: Arc<DuplicateScanWorker>,
    fingerprint_sweep_verifier: Arc<FingerprintSweepVerifier>,
    library_review_sweep_worker: Arc<LibraryReviewSweepWorker>,
    library_youtube_check: Arc<LibraryYoutubeCheck>,
}

impl LibraryJobs {
    fn build(
        settings: &Arc<SettingsStore>,
        stores: &Stores,
        navidrome: &NavidromeServices,
        clients: &MetadataClients,
        acquisition: &Acquisition,
        executor: &Arc<LibraryActionExecutor>,
        soulseek_link: Arc<dyn ISoulseekLink>,
    ) -> LibraryJobs {
        let upgrade_worker = Arc::new(UpgradeWorker::new(
            stores.upgrade_queue.clone(),
            UpgradeWorkerParts {
                executor: executor.clone(),
                resolver: navidrome.navidrome_song_path_resolver.clone(),
                concurrency: Some(stores.download_concurrency.clone()),
                soulseek: Some(soulseek_link.clone()),
                tracker: Some(acquisition.acquisition_tracker.clone()),
                attempts: Some(stores.quality_upgrade_store.clone()),
                settings: Some(settings.clone()),
                sources: Some(acquisition.upgrade_sources.clone()),
                picks: Some(stores.download_picks.clone()),
            },
        ));
        let quality_upgrade_worker = Arc::new(
            QualityUpgradeWorker::new(
                stores.quality_upgrade_store.clone(),
                executor.clone(),
                acquisition.acquisition_activity.clone(),
                navidrome.navidrome_playlist_api.clone(),
                navidrome.navidrome_identity.clone(),
                settings.clone(),
                Some(soulseek_link),
                Some(stores.upgrade_queue.clone()),
                Some(acquisition.upgrade_sources.clone()),
            )
            .with_library_scope(navidrome.library_scope.clone()),
        );
        let notice_playlist_worker = Arc::new(NoticePlaylistWorker::new(
            stores.notice_queue.clone(),
            navidrome.navidrome_playlist_api.clone(),
            navidrome.navidrome_song_path_resolver.clone(),
            navidrome.navidrome_identity.clone(),
            clients.acoust_id.clone(),
            clients.music_brainz.clone(),
            settings.clone(),
        ));
        let duplicate_scan_worker = Arc::new(
            DuplicateScanWorker::new(
                stores.notice_queue.clone(),
                navidrome.navidrome_identity.clone(),
                navidrome.http.clone(),
                settings.clone(),
                Some(navidrome.navidrome_song_path_resolver.clone()),
                Some(Arc::new(SpectrumAnalyzer::new())),
            )
            .with_library_scope(navidrome.library_scope.clone()),
        );
        let fingerprint_sweep_verifier = Arc::new(FingerprintSweepVerifier::new());
        let resolver = navidrome.navidrome_song_path_resolver.clone();
        let library_review_sweep_worker = Arc::new(
            LibraryReviewSweepWorker::new(
                LibraryReviewSweepParts {
                    store: stores.review_sweep_store.clone(),
                    notices: stores.notice_queue.clone(),
                    verifier: fingerprint_sweep_verifier.clone(),
                    activity: acquisition.acquisition_activity.clone(),
                    library: navidrome.local_library.clone(),
                    settings: settings.clone(),
                    // The resolver's root, the same one review actions resolve inside.
                    music_root: Box::new(move || resolver.music_root()),
                },
                Clock::system(),
            )
            .with_library_scope(navidrome.library_scope.clone())
            // The admin's marks are read with Octo's admin sign-in and each Family member's with
            // their own. Anyone else on the server, or a member whose marks cannot be read, keeps
            // the song a question.
            .with_replacer(Arc::new(ExecutorSweepReplacer::new(
                navidrome.navidrome_song_path_resolver.clone(),
                executor.clone(),
                navidrome.navidrome_playlist_api.clone(),
                Some(Arc::new(NavidromeMemberMarks::new(
                    Arc::clone(settings),
                    navidrome.http.clone(),
                    Arc::clone(&stores.family_store),
                    Arc::new(MemberCredentials::new(
                        Arc::clone(&stores.family_store),
                        Arc::clone(&stores.family_vault),
                        Arc::clone(settings),
                    )),
                ))),
                Arc::new(NavidromeOtherListeners::new(
                    navidrome.navidrome_admin.clone(),
                    stores.family_store.clone(),
                )),
            ))),
        );
        // Walks the library once, when a person starts it, and keeps to the shared library. A song
        // AcoustID confirms, by the tag check's answer or the review sweep's lookup, is passed over.
        let resolver = navidrome.navidrome_song_path_resolver.clone();
        let library_youtube_check = Arc::new(
            LibraryYoutubeCheck::new(
                stores.youtube_check_store.clone(),
                stores.notice_queue.clone(),
                Arc::new(AppYoutubeChecker {
                    fingerprinter: Arc::new(AudioFingerprinter::new()),
                    youtube: stores.you_tube_resolver.clone(),
                    settings: settings.clone(),
                }),
                acquisition.acquisition_activity.clone(),
                settings.clone(),
                Box::new(move || resolver.music_root()),
                Clock::system(),
            )
            .with_library_scope(navidrome.library_scope.clone())
            .with_acoustid(
                stores.identity_sweep_store.clone(),
                fingerprint_sweep_verifier.clone(),
            )
            // Replacing, when switched on, by the sweep's own rule and toward its day's count.
            .with_replacing(library_review_sweep_worker.clone()),
        );
        LibraryJobs {
            upgrade_worker,
            quality_upgrade_worker,
            notice_playlist_worker,
            duplicate_scan_worker,
            fingerprint_sweep_verifier,
            library_review_sweep_worker,
            library_youtube_check,
        }
    }

    /// The notice playlists, and the duplicate scan, the upgrade worker, the weekly upgrade and
    /// the review sweep, each the same instance the state holds.
    fn register_workers(&self, workers: &WorkerSupervisor) {
        let notices = self.notice_playlist_worker.clone();
        workers.register("NoticePlaylistWorker", move |stopping| {
            notices.clone().run(stopping)
        });
        let duplicates = self.duplicate_scan_worker.clone();
        workers.register("DuplicateScanWorker", move |stopping| {
            duplicates.clone().run(stopping)
        });
        let upgrades = self.upgrade_worker.clone();
        workers.register("UpgradeWorker", move |stopping| upgrades.clone().run(stopping));
        let weekly = self.quality_upgrade_worker.clone();
        workers.register("QualityUpgradeWorker", move |stopping| {
            weekly.clone().run(stopping)
        });
        let sweep = self.library_review_sweep_worker.clone();
        workers.register("LibraryReviewSweepWorker", move |stopping| {
            sweep.clone().run(stopping)
        });
        let youtube_check = self.library_youtube_check.clone();
        workers.register("LibraryYoutubeCheck", move |stopping| {
            youtube_check.clone().run(stopping)
        });
    }
}

/// Members' requests, over the acquisition services and the imports (which ask for a list's
/// missing songs through them).
struct FamilyRequests {
    service: Arc<RequestService>,
    events: Arc<FamilyEvents>,
}

impl FamilyRequests {
    fn build(
        settings: &Arc<SettingsStore>,
        stores: &Stores,
        navidrome: &NavidromeServices,
        acquisition: &Acquisition,
        metadata: Arc<dyn IMusicMetadataService>,
        imports: &Arc<crate::services::imports::ImportService>,
    ) -> FamilyRequests {
        let events = Arc::new(FamilyEvents::new());
        let fetcher = Arc::new(LiveRequestFetcher::new(
            acquisition.track_acquisition_queue.clone(),
            acquisition.heart_acquisition_coordinator.clone(),
            acquisition.acquisition_tracker.clone(),
            acquisition.upgrade_queue.clone(),
        ));
        fetcher.use_imports(imports);
        let service = RequestService::new(RequestParts {
            settings: Arc::clone(settings),
            family: Arc::clone(&stores.family_store),
            requests: Arc::clone(&stores.family_requests),
            personal: navidrome.personal_library.clone(),
            catalog: Arc::new(LiveRequestCatalog {
                metadata,
                ownership: acquisition.library_ownership.clone(),
                library: navidrome.local_library.clone(),
                registry: stores.external_id_registry.clone(),
            }),
            fetcher,
            stars: Arc::new(NavidromeMemberStars::new(
                Arc::clone(settings),
                navidrome.http.clone(),
                Arc::new(MemberCredentials::new(
                    Arc::clone(&stores.family_store),
                    Arc::clone(&stores.family_vault),
                    Arc::clone(settings),
                )),
            )),
            events: Arc::clone(&events),
            playlists: Arc::clone(&stores.family_playlists),
        });
        imports.use_family(crate::services::imports::ImportFamily {
            gate: navidrome.family_write_gate.clone(),
            requests: Arc::downgrade(&service),
        });
        FamilyRequests { service, events }
    }
}

/// What the family gate works with, built from the family stores.
struct FamilyEdge {
    credentials: Arc<MemberCredentials>,
    devices: Arc<DeviceRegistry>,
    bridge: Arc<MemberBridge>,
    answer_rules: Arc<AnswerRules>,
    audio: Arc<AudioGate>,
}

impl FamilyEdge {
    fn build(
        settings: &Arc<SettingsStore>,
        store: &Arc<FamilyStore>,
        vault: &Arc<FamilyVault>,
        index: &Arc<LibraryIndex>,
    ) -> Self {
        FamilyEdge {
            credentials: Arc::new(MemberCredentials::new(
                Arc::clone(store),
                Arc::clone(vault),
                Arc::clone(settings),
            )),
            devices: Arc::new(DeviceRegistry::new(Arc::clone(store))),
            bridge: Arc::new(MemberBridge::default()),
            answer_rules: Arc::new(AnswerRules::standard()),
            audio: Arc::new(AudioGate::new(
                Arc::clone(settings),
                Arc::clone(store),
                Arc::clone(index),
            )),
        }
    }
}

impl AppInner {
    /// Hands the member sign-ins to everything that keeps a sign-in for later, so each asks
    /// for a fresh one when it uses it. Favorites on arrival also go on a member's own copy of
    /// a download that landed where they cannot see it.
    fn use_member_credentials(&self) {
        let credentials = &self.family_credentials;
        self.star_on_arrival
            .use_member_credentials(Arc::clone(credentials));
        self.star_on_arrival
            .use_member_copies(self.family_pool_service.clone() as Arc<dyn MemberCopies>);
        self.last_fm_radio_stream_sessions
            .use_member_credentials(Arc::clone(credentials));
        self.library_action_rating_worker
            .use_member_credentials(Arc::clone(credentials));
        self.credential_check
            .use_member_credentials(Arc::clone(credentials));
        self.listen_brainz.use_member_credentials(Arc::clone(credentials));
        self.last_fm_radio_streams
            .use_audio_gate(Arc::clone(&self.family_audio));
    }

    /// Begins a graceful shutdown.
    pub fn stop_application(&self) {
        self.lifetime.cancel();
    }

    /// Starts Family's background work once Family is on: makes `family.key` when there is
    /// none and nothing sealed is waiting for an old one, and writes devices' last-seen details
    /// every 30 seconds (and once more on the way out), and keeps members' Navidrome players at
    /// their family limit ([`PlayerCaps`]). Does nothing while Family is off, so an
    /// install that never turns it on runs no extra worker; turning it on later calls this
    /// again, and only the first call after that does anything.
    /// Answers whether it started the work now.
    pub fn start_family(&self) -> bool {
        if !self.settings.current().family.enabled || self.family_started.swap(true, Ordering::AcqRel) {
            return false;
        }
        if !self.family_vault.has_key()
            && !self.family_store.has_sealed_secrets()
            && let Err(e) = self.family_vault.create_key_if_missing()
        {
            tracing::warn!("FAMILY family.key could not be made: {e}");
        }
        self.start_library_index();
        let caps = Arc::new(PlayerCaps::new(
            self.settings.clone(),
            self.family_store.clone(),
            self.navidrome_admin.clone(),
            self.family_devices.clone(),
        ));
        self.workers
            .register("FamilyPlayerCaps", move |token| caps.clone().run(token));
        let store = self.family_store.clone();
        self.workers.register("FamilyStore", move |token| {
            let store = store.clone();
            crate::services::state_file::flush_every(
                Duration::from_secs(30),
                token,
                Arc::new(move || store.flush_if_dirty()),
            )
        });
        // Sign-in hand-overs, and boxes nobody fetched, leave memory once their time is up.
        let family = self.family.clone();
        self.workers.register("FamilyHandover", move |token| {
            let family = family.clone();
            crate::services::state_file::flush_every(
                crate::services::family::handover::SWEEP_EVERY,
                token,
                Arc::new(move || family.handover.sweep()),
            )
        });
        true
    }

    /// Starts reading the library index (once): while Family is on, and while members' own
    /// libraries are still there after it was turned off, since library-wide work keeps to the
    /// shared library as long as they are and tells songs apart by the index.
    pub fn start_library_index(&self) {
        if self.library_index_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let index = self.library_index.clone();
        let library = self.local_library.clone();
        self.workers.register("LibraryIndex", move |token| {
            index.clone().run(token, library.clone())
        });
    }
}

/// Waits until Family is on (its setting changed by any means: the Family page, the dashboard,
/// or `settings.json` edited by hand), then starts its work. Ends then, when the state is gone,
/// or on shutdown.
async fn watch_for_family(state: std::sync::Weak<AppInner>, token: CancellationToken) -> anyhow::Result<()> {
    let Some(mut changes) = state.upgrade().map(|inner| inner.settings.subscribe()) else {
        return Ok(());
    };
    loop {
        match state.upgrade() {
            None => return Ok(()),
            Some(inner) if inner.settings.current().family.enabled => {
                AppState::from_inner(inner).start_family();
                return Ok(());
            }
            Some(_) => {}
        }
        tokio::select! {
            () = token.cancelled() => return Ok(()),
            changed = changes.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
            }
        }
    }
}

impl AppState {
    /// Starts Family's background work once Family is on ([`AppInner::start_family`]), and the
    /// family work that needs the whole state: the link probe, the new-device notices and
    /// keeping members' libraries in step with Navidrome's.
    pub fn start_family(&self) {
        if !self.inner.start_family() {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        self.workers.register("FamilyWork", move |token| {
            crate::services::family::family_service::run_family_work(weak.clone(), token)
        });
    }

    /// While Family is off: reads the library index when members' own libraries are still
    /// there from a time it was on, and watches the settings, so Family turned on in
    /// `settings.json` by hand starts its work ([`Self::start_family`]) the moment Octo reads
    /// the file again. The watch ends once Family is on.
    pub fn watch_family_switch(&self) {
        if self.settings.current().family.enabled {
            return;
        }
        if self.family_store.has_personal_libraries() {
            self.inner.start_library_index();
        }
        let weak = Arc::downgrade(&self.inner);
        self.workers
            .register("FamilySwitch", move |token| watch_for_family(weak.clone(), token));
    }

    /// Connects the family service to the services that do its work, once the state is built:
    /// its hooks ([`crate::services::family::FamilyHooks::standard`]), and family playlist
    /// additions that wait for approval to the request service.
    fn wire_family(&self) {
        self.family.wire(self, |_| {});
        self.family_playlist_requests.set(Arc::new(
            crate::services::family::hooks::RequestPlaylistAdditions {
                service: Arc::downgrade(&self.family_request_service),
            },
        ));
    }

    /// The state around an existing inner, for work that keeps only a weak hold on it.
    pub(crate) fn from_inner(inner: Arc<AppInner>) -> AppState {
        AppState { inner }
    }

    /// The production state over a loaded settings store. Workers are registered here but not
    /// started; [`crate::host::run`] starts them once the listener is bound.
    ///
    /// Built in dependency order, each group over the ones before it: the stores and the
    /// metadata clients (every state file and outside lookup), the tagging and lyrics services,
    /// the Last.fm, ListenBrainz and notification services, the Soulseek services (the metadata
    /// service everything searches through), Navidrome's services (whose startup validation
    /// checks Subsonic, then Soulseek), the lyrics, genre and cover jobs (over the library
    /// service), the acquisition pipeline (over all of those), library actions and the queues and
    /// sweeps (over its executor and queues), radio and the charts, and last the library
    /// snapshot, the sign-in, the Library health services and the import, which read from the
    /// rest. Each group's workers are registered as it is built.
    pub fn build(settings: SettingsStore) -> AppState {
        let settings_path = settings
            .settings_path()
            .map(PathBuf::from)
            .unwrap_or_else(SettingsStore::default_settings_path);
        let restart_tracker = RestartTracker::new(&settings);
        let config_dir = settings_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let settings = Arc::new(settings);
        let stores = Stores::build(&settings, &config_dir);
        // Matches survive a restart, so a library is not sent back to Apple album by album.
        let clients = MetadataClients::build(&settings, Some(config_dir.join("itunes-masters.json")));
        let tagging = Tagging::build(&settings, &clients);
        let lyrics = Lyrics::build(&settings, Some(&config_dir));
        let workers = Arc::new(WorkerSupervisor::new());
        stores.register_workers(&workers);
        lyrics.register_workers(&workers);

        // The iTunes matches flush every 15 seconds, and once more on shutdown.
        let itunes = Arc::clone(&clients.itunes_cover_art);
        workers.register("ITunesCoverArtLookup", move |stopping| {
            let itunes = Arc::clone(&itunes);
            async move { itunes.run_flush_loop(stopping).await }
        });
        let settings_writer = Arc::new(SettingsFileWriter::new(settings_path));
        let cover_art_service = Arc::new(CoverArtService::new(Some(config_dir.join("covers"))));
        // The first list cover loads the fonts and finds the system's fallbacks, which takes a
        // second or two; done here in the background so no client waits for it.
        let warmed = Arc::clone(&cover_art_service);
        workers.register("CoverArtService", move |_stopping| {
            let service = Arc::clone(&warmed);
            async move {
                let _ = tokio::task::spawn_blocking(move || service.warm()).await;
                Ok(())
            }
        });
        let integrations = Integrations::build(&settings, &settings_writer, &clients.deezer_metadata, None);
        let soulseek = SoulseekServices::build(&settings, &stores, &clients, &integrations.last_fm);
        let metadata_service: Arc<dyn IMusicMetadataService> = soulseek.metadata.clone();
        // The Share my library switch: keeps slskd.yml's share list to it, and checks every 10
        // minutes.
        let share_switch = soulseek.share_switch.clone();
        workers.register("SoulseekShareSwitch", move |stopping| {
            share_switch.clone().run(stopping)
        });
        let navidrome = NavidromeServices::build(&settings, &stores, true, vec![soulseek.validator]);
        let family_navidrome = (
            navidrome.navidrome_admin.clone(),
            navidrome.library_index.clone(),
            navidrome.library_scope.clone(),
        );
        lyrics.sidecar_writer.set_library(navidrome.local_library.clone());
        lyrics
            .sidecar_writer
            .set_write_gate(stores.library_write_gate.clone());
        let lyrics_library = lyrics.library_job(&settings, Some(&config_dir), &navidrome);
        lyrics_library.register_workers(&workers);
        let genre_cover_jobs = GenreAndCoverJobs::build(
            &settings,
            Some(&config_dir),
            &navidrome,
            &clients,
            &stores.tag_edit_journal,
            &stores.library_write_gate,
        );
        genre_cover_jobs.register_workers(&workers);
        let tidy_worker = build_tidy(
            &settings,
            Some(&config_dir),
            &navidrome,
            &clients,
            &stores,
            &genre_cover_jobs,
            &lyrics.undo_journal,
        );
        {
            let worker = tidy_worker.clone();
            workers.register("TidyWorker", move |stopping| worker.clone().run(stopping));
        }
        let acquisition = Acquisition::build(
            &settings,
            &stores,
            &navidrome,
            &integrations,
            &clients,
            metadata_service.clone(),
            soulseek.link.clone(),
            DownloadDeps {
                tagging: &tagging,
                lyrics: &lyrics.sidecar_writer,
                soulseek_client: &soulseek.client,
            },
            None,
        );
        acquisition.register_workers(&workers, &settings, &stores, &integrations.notifications);
        let library_actions = LibraryActions::build(
            &settings,
            &stores,
            &navidrome,
            &acquisition,
            soulseek.link.clone(),
            tagging.spectrum_analyzer.clone(),
            lyrics.song_answers(&genre_cover_jobs.cover_upgrade_worker),
        );
        library_actions.register_workers(&workers);
        let library_jobs = LibraryJobs::build(
            &settings,
            &stores,
            &navidrome,
            &clients,
            &acquisition,
            &library_actions.executor,
            soulseek.link.clone(),
        );
        library_jobs.register_workers(&workers);
        let radio = Radio::build(
            &settings,
            &config_dir,
            &stores,
            &navidrome,
            &integrations,
            metadata_service.clone(),
            acquisition.download_service.clone(),
            &TestServices::default(),
            &clients.deezer_metadata,
            &acquisition,
            &clients.music_brainz,
        );
        radio.register_workers(&workers);
        let prefetch = prefetch_service(
            &settings,
            &stores,
            &navidrome,
            &acquisition,
            soulseek.link.clone(),
        );
        prefetch.use_mixes(radio.generated_playlists.clone());
        prefetch.use_preview_copies(stores.preview_copies.clone());
        {
            let prefetch = prefetch.clone();
            workers.register("PrefetchService", move |stopping| prefetch.clone().run(stopping));
        }
        // Listening: the radio history copied in once, then old plays pruned once a day.
        let listening = stores.listening.clone();
        let history = radio.state.clone();
        let listening_last_fm = integrations.last_fm.clone();
        workers.register("ListeningLog", move |stopping| {
            listening
                .clone()
                .run(history.clone(), listening_last_fm.clone(), stopping)
        });
        let charts = Charts::build(
            &settings,
            &stores,
            &clients,
            &integrations,
            &navidrome,
            metadata_service.clone(),
            None,
            &config_dir,
        );
        charts.register_workers(&workers);
        let sync_catalog = Arc::new(SyncCatalogService::new(
            navidrome.subsonic_proxy.clone(),
            metadata_service.clone(),
            stores.external_id_registry.clone(),
            settings.clone(),
            Clock::system(),
        ));
        sync_catalog.use_library_scope(navidrome.library_scope.clone());
        // The sweep's verifier and the download verification service are built in different
        // groups; the service is handed over once both exist.
        library_jobs
            .fingerprint_sweep_verifier
            .set_service(tagging.download_verification.clone() as Arc<dyn SweepVerification>);
        library_jobs
            .fingerprint_sweep_verifier
            .set_preview(stores.you_tube_resolver.clone() as Arc<dyn PreviewSource>);
        let library_snapshot = Arc::new(
            LibrarySnapshot::new(Arc::clone(&navidrome.navidrome_playlist_api))
                .with_scope(navidrome.library_scope.clone()),
        );
        // The animated art sweep reads the library from the same snapshot, at Apple's pace.
        let art_albums: crate::services::cover_art::animated_art::LibraryAlbums = {
            let snapshot = Arc::clone(&library_snapshot);
            Arc::new(move || {
                let snapshot = Arc::clone(&snapshot);
                Box::pin(async move {
                    let index = snapshot.current().await?;
                    Some(
                        index
                            .albums()
                            .filter_map(|album| {
                                let artist = album
                                    .album_artist
                                    .clone()
                                    .or_else(|| album.songs.first().map(|song| song.artist.clone()))?;
                                Some((artist, album.name.clone()))
                            })
                            .collect(),
                    )
                })
            })
        };
        let animated = Arc::clone(&charts.animated_art);
        workers.register("AnimatedArtSweep", move |stopping| {
            Arc::clone(&animated).run_sweep_loop(Arc::clone(&art_albums), stopping)
        });
        let album_ownership = Arc::new(AlbumOwnership::over(
            Arc::clone(&library_snapshot),
            Arc::clone(&clients.deezer_metadata),
            Arc::clone(&stores.external_id_registry),
        ));
        let admin_sign_in = AdminSignIn::build(&settings, &stores, &navidrome, &config_dir);
        let (library_rescan, library_edits) = library_edits(
            &settings,
            &stores,
            &navidrome,
            &library_actions.quarantine,
            &tagging,
            genre_cover_jobs.album_cover_finder.clone(),
        );
        let library_health = library_health(
            &settings,
            &library_snapshot,
            &library_rescan,
            &library_edits,
            &library_actions.executor,
            &acquisition.upgrade_sources,
            &navidrome,
            &stores,
            &library_actions.quarantine,
            &family_navidrome.2,
        );
        if let Some(keep) = library_health.keep_marks().cloned() {
            workers.register("KeepMarksStagingSweep", move |_stopping| {
                let keep = Arc::clone(&keep);
                async move {
                    keep.sweep_staging().await;
                    Ok(())
                }
            });
        }
        let library_identity_sweep = library_identity_sweep(
            &settings,
            &stores,
            &navidrome,
            &clients.music_brainz,
            acquisition.acquisition_activity.clone(),
            library_jobs.fingerprint_sweep_verifier.clone(),
            &library_snapshot,
            &library_edits,
            &family_navidrome.2,
            Some(Arc::new(
                crate::services::library::version_check::OnlineVersionSources {
                    http: reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(15))
                        .build()
                        .unwrap_or_default(),
                    music_brainz: clients.music_brainz.clone(),
                    deezer: Some(clients.deezer_metadata.clone()),
                    acoust_id: clients.acoust_id.clone(),
                    fingerprinter: Arc::new(AudioFingerprinter::new()),
                    settings: settings.clone(),
                },
            )),
        );
        library_actions
            .executor
            .use_version_checker(library_identity_sweep.clone()
                as Arc<dyn crate::services::library::version_check::VersionChecker>);
        let identity_sweep = library_identity_sweep.clone();
        workers.register("LibraryIdentitySweep", move |stopping| {
            identity_sweep.clone().run(stopping)
        });
        let imports = Imports::build(
            &settings,
            Some(&config_dir),
            &stores,
            &navidrome,
            &acquisition,
            soulseek.link.clone(),
            Some(clients.music_brainz.clone()),
        );
        imports.register_workers(&workers);
        let family_requests = FamilyRequests::build(
            &settings,
            &stores,
            &navidrome,
            &acquisition,
            metadata_service.clone(),
            &imports.service,
        );
        let family_edge = FamilyEdge::build(
            &settings,
            &stores.family_store,
            &stores.family_vault,
            &family_navidrome.1,
        );
        let keep_ready = keep_ready_service(
            &settings,
            &config_dir,
            &stores,
            &imports.service,
            &radio.generated_playlists,
            &charts.playlists,
            &metadata_service,
            &navidrome.local_library,
            &prefetch,
        );
        {
            let keep_ready = keep_ready.clone();
            workers.register("KeepReady", move |stopping| keep_ready.clone().run(stopping));
        }
        let inner = AppInner {
            settings_writer,
            restart_tracker: Arc::new(restart_tracker),
            settings,
            workers,
            lyrics_choice_store: lyrics.choice_store,
            lyrics_service: lyrics.service,
            lyrics_choice_service: lyrics.choice_service,
            lyrics_sidecar_writer: lyrics.sidecar_writer,
            lyrics_undo_journal: lyrics.undo_journal,
            lyrics_library_store: lyrics_library.store,
            lyrics_library_worker: lyrics_library.worker,
            genre_backfill_store: genre_cover_jobs.genre_backfill_store,
            genre_backfill_journal: genre_cover_jobs.genre_backfill_journal,
            genre_backfill_worker: genre_cover_jobs.genre_backfill_worker,
            tidy_worker,
            album_cover_finder: genre_cover_jobs.album_cover_finder,
            cover_upgrade_store: genre_cover_jobs.cover_upgrade_store,
            cover_upgrade_journal: genre_cover_jobs.cover_upgrade_journal,
            cover_upgrade_worker: genre_cover_jobs.cover_upgrade_worker,
            explicit_backfill: genre_cover_jobs.explicit_backfill,
            update_host: Arc::new(UpdateHost::new(config_dir.join("update"), Clock::system())),
            places: Arc::new(PlaceIndex::new(
                crate::controllers::subsonic::places::places_file(&config_dir),
            )),
            lifetime: CancellationToken::new(),
            download_history: stores.download_history,
            download_concurrency: stores.download_concurrency,
            download_picks: stores.download_picks,
            soulseek_holds: stores.soulseek_holds,
            you_tube_resolver: stores.you_tube_resolver,
            external_id_registry: stores.external_id_registry,
            stream_size_warmer: stores.stream_size_warmer,
            local_cover_cache: stores.local_cover_cache,
            radio_queues: stores.radio_queues,
            prefetch,
            preview_copies: stores.preview_copies,
            keep_ready,
            directory_browser: stores.directory_browser,
            browse_sessions: stores.browse_sessions,
            sign_in_throttle: admin_sign_in.throttle,
            lidarr_webhook_throttle: Arc::new(SignInThrottle::new()),
            admin_recovery_code: admin_sign_in.recovery_code,
            admin_role_check: admin_sign_in.role_check,
            anonymous_status: Default::default(),
            rejected_peers: stores.rejected_peers,
            release_check: stores.release_check,
            http: navidrome.http,
            subsonic_proxy: navidrome.subsonic_proxy,
            navidrome_identity: navidrome.navidrome_identity,
            subsonic_discovery: Arc::new(SubsonicDiscoveryService::new()),
            credential_check: Arc::new(CredentialCheck::new()),
            request_identity: Arc::new(RequestIdentity::new()),
            search_song_order_cache: Arc::new(SearchSongOrderCache::new()),
            local_library: navidrome.local_library,
            navidrome_song_path_resolver: navidrome.navidrome_song_path_resolver,
            navidrome_playlist_api: navidrome.navidrome_playlist_api,
            library_snapshot,
            album_ownership,
            startup_validation: navidrome.startup_validation,
            deezer_rate_limiter: clients.deezer_rate_limiter,
            deezer_client: clients.deezer_client,
            deezer_metadata: clients.deezer_metadata,
            music_brainz: clients.music_brainz,
            acoust_id_rate_limiter: clients.acoust_id_rate_limiter,
            acoust_id: clients.acoust_id,
            itunes_cover_art: clients.itunes_cover_art,
            cover_art_aggregator: clients.cover_art_aggregator,
            cover_art_archive: clients.cover_art_archive,
            cover_art_service,
            ambient_palettes: Arc::new(crate::services::admin::AmbientPaletteService::new()),
            last_fm: integrations.last_fm,
            last_fm_scrobbles: integrations.last_fm_scrobbles,
            listen_brainz: integrations.listen_brainz,
            notifications: integrations.notifications,
            recent_scrobbles: integrations.recent_scrobbles,
            subsonic_response_builder: stores.subsonic_response_builder,
            soulseek_client: soulseek.client,
            soulseek_link: soulseek.link,
            soulseek_port_check: soulseek.port_check,
            soulseek_share_switch: soulseek.share_switch,
            soulseek_sharing: soulseek.sharing,
            music_metadata: soulseek.metadata,
            metadata_service,
            download_service: acquisition.download_service,
            lidarr_client: acquisition.lidarr_client,
            spotiflac: acquisition.spotiflac,
            lidarr_album_claims: acquisition.lidarr_album_claims,
            lidarr_import_handoff: acquisition.lidarr_import_handoff,
            lidarr_track_fetcher: acquisition.lidarr_track_fetcher,
            lidarr_hearts: acquisition.lidarr_hearts,
            track_acquisition_queue: acquisition.track_acquisition_queue,
            acquisition_activity: acquisition.acquisition_activity,
            acquisition_tracker: acquisition.acquisition_tracker,
            song_finder: acquisition.song_finder,
            star_on_arrival: acquisition.star_on_arrival,
            upgrade_queue: acquisition.upgrade_queue,
            upgrade_sources: acquisition.upgrade_sources,
            library_ownership: acquisition.library_ownership,
            heart_ownership: acquisition.heart_ownership,
            heart_acquisition_coordinator: acquisition.heart_acquisition_coordinator,
            external_search: acquisition.external_search,
            last_fm_radio_state: radio.state,
            last_fm_radio_refresh_queue: radio.refresh_queue,
            last_fm_radio_stream_sessions: radio.sessions,
            last_fm_radio_track_cache: radio.cache,
            last_fm_radio_recommendations: radio.recommendations,
            last_fm_radio_streams: radio.streams,
            last_fm_radio_warmup: radio.warmup,
            last_fm_radio_refresh_worker: radio.refresh_worker,
            radio_sources: radio.sources,
            song_radio: radio.song_radio,
            radio_outcomes: radio.outcomes,
            sonic_store: radio.sonic_store,
            sonic_worker: radio.sonic_worker,
            generated_playlists: radio.generated_playlists,
            top_songs: charts.top_songs,
            chart_playlists: charts.playlists,
            library_matches: charts.library_matches,
            animated_art: charts.animated_art,
            sync_catalog,
            library_action_journal: stores.library_action_journal,
            tag_edit_journal: stores.tag_edit_journal,
            library_write_gate: stores.library_write_gate,
            library_action_quarantine: library_actions.quarantine,
            library_action_executor: library_actions.executor,
            library_action_rating_worker: library_actions.rating_worker,
            notice_queue: library_actions.notice_queue,
            loudness_meter: tagging.loudness_meter,
            audio_fingerprinter: tagging.audio_fingerprinter,
            spectrum_analyzer: tagging.spectrum_analyzer,
            release_identifier: tagging.release_identifier,
            tag_preview: tagging.tag_preview,
            download_verification: tagging.download_verification,
            download_cover_resolver: tagging.download_cover_resolver,
            upgrade_worker: library_jobs.upgrade_worker,
            quality_upgrade_store: stores.quality_upgrade_store,
            quality_upgrade_worker: library_jobs.quality_upgrade_worker,
            duplicate_scan_worker: library_jobs.duplicate_scan_worker,
            review_sweep_store: stores.review_sweep_store,
            identity_sweep_store: stores.identity_sweep_store,
            library_identity_sweep,
            library_review_sweep_worker: library_jobs.library_review_sweep_worker,
            library_youtube_check: library_jobs.library_youtube_check,
            fingerprint_sweep_verifier: library_jobs.fingerprint_sweep_verifier,
            library_rescan,
            library_edits,
            library_health,
            imports: imports.service,
            family_vault: stores.family_vault,
            family_store: stores.family_store,
            family_requests: stores.family_requests,
            family_saved: stores.family_saved,
            family_playlists: stores.family_playlists,
            family_playlist_requests: Default::default(),
            family_pool: stores.family_pool,
            listening: stores.listening,
            navidrome_admin: family_navidrome.0,
            library_index: family_navidrome.1,
            library_scope: family_navidrome.2,
            family_credentials: family_edge.credentials,
            family_devices: family_edge.devices,
            family_bridge: family_edge.bridge,
            family_answer_rules: family_edge.answer_rules,
            family_audio: family_edge.audio,
            family: Arc::new(FamilyService::default()),
            family_pool_service: navidrome.family_pool_service.clone(),
            personal_library: navidrome.personal_library.clone(),
            family_write_gate: navidrome.family_write_gate.clone(),
            family_request_service: family_requests.service,
            family_events: family_requests.events,
            family_started: AtomicBool::new(false),
            library_index_started: AtomicBool::new(false),
        };
        inner.use_member_credentials();
        let state = AppState {
            inner: Arc::new(inner),
        };
        state.wire_family();
        state.start_family();
        state.watch_family_switch();
        state
    }

    /// A state for handler tests: `settings` as the live snapshot, no configuration behind it,
    /// and a settings writer, the update folder and the state files pointed at a fresh directory in the temp
    /// directory that nothing creates until a test writes. No workers are registered.
    pub fn for_tests(settings: AppSettings) -> AppState {
        Self::for_tests_with(settings, TestServices::default())
    }

    /// [`AppState::for_tests`] with some services swapped for a test's fakes.
    pub fn for_tests_with(settings: AppSettings, overrides: TestServices) -> AppState {
        let mut settings = settings;
        settings.admin.sign_in = overrides.sign_in.clone().unwrap_or_else(|| "off".to_string());
        let store = SettingsStore::for_tests(settings);
        for (key, value) in &overrides.raw_settings {
            store.set_raw(key, Some(value));
        }
        let config_dir = overrides.config_dir.clone().unwrap_or_else(|| {
            std::env::temp_dir().join(format!("octo-test-{}", uuid::Uuid::new_v4().simple()))
        });
        // The download service makes its music folder when it is built. Without a configured one
        // that is "./downloads" beside the test binary's working directory, so a handler test's
        // goes in its own temp folder instead.
        if store.raw("Library:DownloadPath").is_none() {
            store.set_raw(
                "Library:DownloadPath",
                Some(&config_dir.join("music").to_string_lossy()),
            );
        }
        let restart_tracker = Arc::new(RestartTracker::new(&store));
        let settings = Arc::new(store);
        let stores = Stores::build_with(&settings, &config_dir, overrides.external_id_registry.clone());
        // The clients point at the real hosts but ask nothing until a test calls them, and
        // the iTunes matches stay in memory.
        let clients = MetadataClients::build_with(&settings, None, overrides.deezer_base_url.as_deref());
        let tagging = Tagging::build(&settings, &clients);
        let lyrics = Lyrics::build(&settings, None);
        // The Last.fm, ListenBrainz and notification clients point at the real hosts too, and
        // the default settings give none of them a key, token or URL to use.
        let settings_writer = Arc::new(SettingsFileWriter::new(config_dir.join("settings.json")));
        let cover_art_service = Arc::new(CoverArtService::new(Some(config_dir.join("covers"))));
        let integrations = Integrations::build(
            &settings,
            &settings_writer,
            &clients.deezer_metadata,
            overrides.last_fm_base_url.as_deref(),
        );
        let soulseek = SoulseekServices::build(&settings, &stores, &clients, &integrations.last_fm);
        let metadata_service: Arc<dyn IMusicMetadataService> = overrides
            .metadata
            .clone()
            .unwrap_or_else(|| soulseek.metadata.clone());
        let navidrome = NavidromeServices::build(&settings, &stores, false, vec![soulseek.validator]);
        let family_navidrome = (
            navidrome.navidrome_admin.clone(),
            navidrome.library_index.clone(),
            navidrome.library_scope.clone(),
        );
        lyrics.sidecar_writer.set_library(navidrome.local_library.clone());
        lyrics
            .sidecar_writer
            .set_write_gate(stores.library_write_gate.clone());
        let lyrics_library = lyrics.library_job(&settings, None, &navidrome);
        // The genre backfill and the cover upgrade keep their runs in memory.
        let genre_cover_jobs = GenreAndCoverJobs::build(
            &settings,
            None,
            &navidrome,
            &clients,
            &stores.tag_edit_journal,
            &stores.library_write_gate,
        );
        let tidy_worker = build_tidy(
            &settings,
            None,
            &navidrome,
            &clients,
            &stores,
            &genre_cover_jobs,
            &lyrics.undo_journal,
        );
        // No workers: the acquisition worker, the resumer and the cache cleanup are not run.
        let acquisition = Acquisition::build(
            &settings,
            &stores,
            &navidrome,
            &integrations,
            &clients,
            metadata_service.clone(),
            soulseek.link.clone(),
            DownloadDeps {
                tagging: &tagging,
                lyrics: &lyrics.sidecar_writer,
                soulseek_client: &soulseek.client,
            },
            overrides.download_service.clone(),
        );
        // Library actions keep their journal in the test's config directory; no worker is run.
        let library_actions = LibraryActions::build(
            &settings,
            &stores,
            &navidrome,
            &acquisition,
            soulseek.link.clone(),
            tagging.spectrum_analyzer.clone(),
            lyrics.song_answers(&genre_cover_jobs.cover_upgrade_worker),
        );
        // The queues and sweeps keep their files in the test's config directory; no worker is
        // run.
        let library_jobs = LibraryJobs::build(
            &settings,
            &stores,
            &navidrome,
            &clients,
            &acquisition,
            &library_actions.executor,
            soulseek.link.clone(),
        );
        // The radio's state files sit in the test's config directory; its workers are not run.
        let radio = Radio::build(
            &settings,
            &config_dir,
            &stores,
            &navidrome,
            &integrations,
            metadata_service.clone(),
            acquisition.download_service.clone(),
            &overrides,
            &clients.deezer_metadata,
            &acquisition,
            &clients.music_brainz,
        );
        let prefetch = prefetch_service(
            &settings,
            &stores,
            &navidrome,
            &acquisition,
            soulseek.link.clone(),
        );
        prefetch.use_mixes(radio.generated_playlists.clone());
        prefetch.use_preview_copies(stores.preview_copies.clone());
        // Apple's pages from the test's mock server, else an address that refuses at once (a
        // port the system handed out and let go: never port 1, which WSL's mirrored networking
        // lets time out). The warm worker is not run.
        let apple_base = overrides.apple_music_base_url.clone().unwrap_or_else(|| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free loopback port");
            let address = listener.local_addr().expect("its address");
            format!("http://{address}")
        });
        let charts = Charts::build(
            &settings,
            &stores,
            &clients,
            &integrations,
            &navidrome,
            metadata_service.clone(),
            Some(&apple_base),
            &config_dir,
        );
        let sync_catalog = Arc::new(SyncCatalogService::new(
            navidrome.subsonic_proxy.clone(),
            metadata_service.clone(),
            stores.external_id_registry.clone(),
            settings.clone(),
            Clock::system(),
        ));
        sync_catalog.use_library_scope(navidrome.library_scope.clone());
        // The sweep's verifier and the download verification service are built in different
        // groups; the service is handed over once both exist.
        library_jobs
            .fingerprint_sweep_verifier
            .set_service(tagging.download_verification.clone() as Arc<dyn SweepVerification>);
        library_jobs
            .fingerprint_sweep_verifier
            .set_preview(stores.you_tube_resolver.clone() as Arc<dyn PreviewSource>);
        let library_snapshot = overrides.library_snapshot.clone().unwrap_or_else(|| {
            Arc::new(
                LibrarySnapshot::new(Arc::clone(&navidrome.navidrome_playlist_api))
                    .with_scope(navidrome.library_scope.clone()),
            )
        });
        let album_ownership = Arc::new(AlbumOwnership::over(
            Arc::clone(&library_snapshot),
            Arc::clone(&clients.deezer_metadata),
            Arc::clone(&stores.external_id_registry),
        ));
        let admin_sign_in = AdminSignIn::build(&settings, &stores, &navidrome, &config_dir);
        let (library_rescan, library_edits) = library_edits(
            &settings,
            &stores,
            &navidrome,
            &library_actions.quarantine,
            &tagging,
            overrides
                .album_cover_finder
                .clone()
                .unwrap_or_else(|| genre_cover_jobs.album_cover_finder.clone()),
        );
        let library_health = library_health(
            &settings,
            &library_snapshot,
            &library_rescan,
            &library_edits,
            &library_actions.executor,
            &acquisition.upgrade_sources,
            &navidrome,
            &stores,
            &library_actions.quarantine,
            &family_navidrome.2,
        );
        let library_identity_sweep = library_identity_sweep(
            &settings,
            &stores,
            &navidrome,
            &clients.music_brainz,
            acquisition.acquisition_activity.clone(),
            library_jobs.fingerprint_sweep_verifier.clone(),
            &library_snapshot,
            &library_edits,
            &family_navidrome.2,
            None,
        );
        library_actions
            .executor
            .use_version_checker(library_identity_sweep.clone()
                as Arc<dyn crate::services::library::version_check::VersionChecker>);
        // The import's files stay in memory; no worker is run.
        let imports = Imports::build(
            &settings,
            None,
            &stores,
            &navidrome,
            &acquisition,
            soulseek.link.clone(),
            None,
        );
        let family_requests = FamilyRequests::build(
            &settings,
            &stores,
            &navidrome,
            &acquisition,
            metadata_service.clone(),
            &imports.service,
        );
        let family_edge = FamilyEdge::build(
            &settings,
            &stores.family_store,
            &stores.family_vault,
            &family_navidrome.1,
        );
        let keep_ready = keep_ready_service(
            &settings,
            &config_dir,
            &stores,
            &imports.service,
            &radio.generated_playlists,
            &charts.playlists,
            &metadata_service,
            &navidrome.local_library,
            &prefetch,
        );
        let inner = AppInner {
            restart_tracker,
            settings,
            settings_writer,
            workers: Arc::new(WorkerSupervisor::new()),
            lyrics_choice_store: lyrics.choice_store,
            lyrics_service: lyrics.service,
            lyrics_choice_service: lyrics.choice_service,
            lyrics_sidecar_writer: lyrics.sidecar_writer,
            lyrics_undo_journal: lyrics.undo_journal,
            lyrics_library_store: lyrics_library.store,
            lyrics_library_worker: lyrics_library.worker,
            genre_backfill_store: genre_cover_jobs.genre_backfill_store,
            genre_backfill_journal: genre_cover_jobs.genre_backfill_journal,
            genre_backfill_worker: genre_cover_jobs.genre_backfill_worker,
            tidy_worker,
            album_cover_finder: genre_cover_jobs.album_cover_finder,
            cover_upgrade_store: genre_cover_jobs.cover_upgrade_store,
            cover_upgrade_journal: genre_cover_jobs.cover_upgrade_journal,
            cover_upgrade_worker: genre_cover_jobs.cover_upgrade_worker,
            explicit_backfill: genre_cover_jobs.explicit_backfill,
            update_host: Arc::new(UpdateHost::new(config_dir.join("update"), Clock::system())),
            places: Arc::new(PlaceIndex::new(config_dir.join("places.bin"))),
            lifetime: CancellationToken::new(),
            download_history: stores.download_history,
            download_concurrency: stores.download_concurrency,
            download_picks: stores.download_picks,
            soulseek_holds: stores.soulseek_holds,
            you_tube_resolver: stores.you_tube_resolver,
            external_id_registry: stores.external_id_registry,
            stream_size_warmer: stores.stream_size_warmer,
            local_cover_cache: stores.local_cover_cache,
            radio_queues: stores.radio_queues,
            prefetch,
            preview_copies: stores.preview_copies,
            keep_ready,
            directory_browser: stores.directory_browser,
            browse_sessions: stores.browse_sessions,
            sign_in_throttle: admin_sign_in.throttle,
            lidarr_webhook_throttle: Arc::new(SignInThrottle::new()),
            admin_recovery_code: admin_sign_in.recovery_code,
            admin_role_check: admin_sign_in.role_check,
            anonymous_status: Default::default(),
            rejected_peers: stores.rejected_peers,
            release_check: stores.release_check,
            http: navidrome.http,
            subsonic_proxy: navidrome.subsonic_proxy,
            navidrome_identity: navidrome.navidrome_identity,
            subsonic_discovery: Arc::new(SubsonicDiscoveryService::new()),
            credential_check: Arc::new(CredentialCheck::new()),
            request_identity: Arc::new(RequestIdentity::new()),
            search_song_order_cache: Arc::new(SearchSongOrderCache::new()),
            local_library: navidrome.local_library,
            navidrome_song_path_resolver: navidrome.navidrome_song_path_resolver,
            navidrome_playlist_api: navidrome.navidrome_playlist_api,
            library_snapshot,
            album_ownership,
            startup_validation: navidrome.startup_validation,
            deezer_rate_limiter: clients.deezer_rate_limiter,
            deezer_client: clients.deezer_client,
            deezer_metadata: clients.deezer_metadata,
            music_brainz: clients.music_brainz,
            acoust_id_rate_limiter: clients.acoust_id_rate_limiter,
            acoust_id: clients.acoust_id,
            itunes_cover_art: clients.itunes_cover_art,
            cover_art_aggregator: clients.cover_art_aggregator,
            cover_art_archive: clients.cover_art_archive,
            cover_art_service,
            ambient_palettes: Arc::new(crate::services::admin::AmbientPaletteService::new()),
            last_fm: integrations.last_fm,
            last_fm_scrobbles: integrations.last_fm_scrobbles,
            listen_brainz: integrations.listen_brainz,
            notifications: integrations.notifications,
            recent_scrobbles: integrations.recent_scrobbles,
            subsonic_response_builder: stores.subsonic_response_builder,
            soulseek_client: soulseek.client,
            soulseek_link: soulseek.link,
            soulseek_port_check: soulseek.port_check,
            soulseek_share_switch: soulseek.share_switch,
            soulseek_sharing: soulseek.sharing,
            music_metadata: soulseek.metadata,
            metadata_service,
            download_service: acquisition.download_service,
            lidarr_client: acquisition.lidarr_client,
            spotiflac: acquisition.spotiflac,
            lidarr_album_claims: acquisition.lidarr_album_claims,
            lidarr_import_handoff: acquisition.lidarr_import_handoff,
            lidarr_track_fetcher: acquisition.lidarr_track_fetcher,
            lidarr_hearts: acquisition.lidarr_hearts,
            track_acquisition_queue: acquisition.track_acquisition_queue,
            acquisition_activity: acquisition.acquisition_activity,
            acquisition_tracker: acquisition.acquisition_tracker,
            song_finder: acquisition.song_finder,
            star_on_arrival: acquisition.star_on_arrival,
            upgrade_queue: acquisition.upgrade_queue,
            upgrade_sources: acquisition.upgrade_sources,
            library_ownership: acquisition.library_ownership,
            heart_ownership: acquisition.heart_ownership,
            heart_acquisition_coordinator: acquisition.heart_acquisition_coordinator,
            external_search: acquisition.external_search,
            last_fm_radio_state: radio.state,
            last_fm_radio_refresh_queue: radio.refresh_queue,
            last_fm_radio_stream_sessions: radio.sessions,
            last_fm_radio_track_cache: radio.cache,
            last_fm_radio_recommendations: radio.recommendations,
            last_fm_radio_streams: radio.streams,
            last_fm_radio_warmup: radio.warmup,
            last_fm_radio_refresh_worker: radio.refresh_worker,
            radio_sources: radio.sources,
            song_radio: radio.song_radio,
            radio_outcomes: radio.outcomes,
            sonic_store: radio.sonic_store,
            sonic_worker: radio.sonic_worker,
            generated_playlists: radio.generated_playlists,
            top_songs: charts.top_songs,
            chart_playlists: charts.playlists,
            library_matches: charts.library_matches,
            animated_art: charts.animated_art,
            sync_catalog,
            library_action_journal: stores.library_action_journal,
            tag_edit_journal: stores.tag_edit_journal,
            library_write_gate: stores.library_write_gate,
            library_action_quarantine: library_actions.quarantine,
            library_action_executor: library_actions.executor,
            library_action_rating_worker: library_actions.rating_worker,
            notice_queue: library_actions.notice_queue,
            loudness_meter: tagging.loudness_meter,
            audio_fingerprinter: tagging.audio_fingerprinter,
            spectrum_analyzer: tagging.spectrum_analyzer,
            release_identifier: tagging.release_identifier,
            tag_preview: tagging.tag_preview,
            download_verification: tagging.download_verification,
            download_cover_resolver: tagging.download_cover_resolver,
            upgrade_worker: library_jobs.upgrade_worker,
            quality_upgrade_store: stores.quality_upgrade_store,
            quality_upgrade_worker: library_jobs.quality_upgrade_worker,
            duplicate_scan_worker: library_jobs.duplicate_scan_worker,
            review_sweep_store: stores.review_sweep_store,
            identity_sweep_store: stores.identity_sweep_store,
            library_identity_sweep,
            library_review_sweep_worker: library_jobs.library_review_sweep_worker,
            library_youtube_check: library_jobs.library_youtube_check,
            fingerprint_sweep_verifier: library_jobs.fingerprint_sweep_verifier,
            library_rescan,
            library_edits,
            library_health,
            imports: imports.service,
            family_vault: stores.family_vault,
            family_store: stores.family_store,
            family_requests: stores.family_requests,
            family_saved: stores.family_saved,
            family_playlists: stores.family_playlists,
            family_playlist_requests: Default::default(),
            family_pool: stores.family_pool,
            listening: stores.listening,
            navidrome_admin: family_navidrome.0,
            library_index: family_navidrome.1,
            library_scope: family_navidrome.2,
            family_credentials: family_edge.credentials,
            family_devices: family_edge.devices,
            family_bridge: family_edge.bridge,
            family_answer_rules: family_edge.answer_rules,
            family_audio: family_edge.audio,
            family: Arc::new(FamilyService::default()),
            family_pool_service: navidrome.family_pool_service.clone(),
            personal_library: navidrome.personal_library.clone(),
            family_write_gate: navidrome.family_write_gate.clone(),
            family_request_service: family_requests.service,
            family_events: family_requests.events,
            family_started: AtomicBool::new(false),
            library_index_started: AtomicBool::new(false),
        };
        inner.use_member_credentials();
        let state = AppState {
            inner: Arc::new(inner),
        };
        state.wire_family();
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The restart tracker is built with the state, so it holds the values the process started
    /// with; a later change shows as pending.
    #[test]
    fn the_restart_tracker_snapshots_the_settings_the_state_was_built_with() {
        let env = vec![("Subsonic__Url".to_string(), "http://navidrome:4533".to_string())];
        let store = SettingsStore::from_env(env, None);
        let state = AppState::build(store);
        assert!(state.restart_tracker.pending(&*state.settings).is_empty());
        state
            .settings
            .set_raw("Subsonic:Url", Some("http://elsewhere:4533"));
        assert_eq!(
            state.restart_tracker.pending(&*state.settings),
            vec!["Subsonic:Url".to_string()]
        );
        assert_eq!(
            state.settings_writer.file_path(),
            SettingsStore::default_settings_path()
        );
    }

    /// The iTunes matches are flushed by a worker, and the Deezer metadata service and cover
    /// lookup share one client, and so one budget.
    #[test]
    fn the_metadata_clients_share_one_deezer_budget() {
        let state = AppState::build(SettingsStore::from_env(Vec::new(), None));
        assert!(
            state
                .workers
                .status()
                .iter()
                .any(|w| w.name == "ITunesCoverArtLookup")
        );
        assert!(Arc::ptr_eq(
            state.deezer_client.limiter(),
            &state.deezer_rate_limiter
        ));
    }

    /// The sinks in send order, ntfy then Discord, and nothing from the notification, Last.fm or
    /// ListenBrainz services sent anywhere with the default settings.
    #[tokio::test]
    async fn the_notification_sinks_and_scrobblers_send_nothing_by_default() {
        let state = AppState::for_tests(AppSettings::default());
        let results = state.notifications.send_test().await;
        let sinks: Vec<(&str, bool)> = results.iter().map(|r| (r.sink.as_str(), r.configured)).collect();
        assert_eq!(sinks, [("ntfy", false), ("discord", false)]);
        assert!(!state.last_fm.has_api_key());
        assert!(!state.last_fm_scrobbles.is_ready());
        assert!(!state.listen_brainz.is_enabled_for("alice"));
        assert!(
            state
                .recent_scrobbles
                .first_report("alice", "1", None, chrono::Utc::now())
        );
    }

    #[test]
    fn the_store_flushers_and_the_release_check_run_as_workers() {
        let state = AppState::build(SettingsStore::from_env(Vec::new(), None));
        let names: Vec<String> = state.workers.status().into_iter().map(|s| s.name).collect();
        for name in [
            "ExternalIdRegistry",
            "RejectedPeerRegistry",
            "ReleaseCheck",
            "LyricsSidecarWriter",
            "LyricsLibraryStore",
            "LyricsLibraryWorker",
        ] {
            assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
        }
        assert!(
            AppState::for_tests(AppSettings::default())
                .workers
                .status()
                .is_empty()
        );
    }

    /// The acquisition pipeline: its three background services run as workers, the activity
    /// sees the download service, and StarOnArrival is already listening to the tracker.
    #[tokio::test]
    async fn the_acquisition_pipeline_is_wired_with_its_workers() {
        let state = AppState::build(SettingsStore::from_env(Vec::new(), None));
        let names: Vec<String> = state.workers.status().into_iter().map(|s| s.name).collect();
        for name in [
            "AcquisitionWorker",
            "SoulseekHoldResumer",
            "CacheCleanupService",
            "PrefetchService",
        ] {
            assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
        }

        let state = AppState::for_tests(AppSettings::default());
        use crate::services::common::IAcquisitionActivity;
        assert!(!state.acquisition_activity.is_busy());
        state
            .acquisition_tracker
            .begin("soulseek", "abc", None, Some("alice"), None, None, None);
        assert_eq!(state.acquisition_tracker.for_user("alice").len(), 1);
        assert_eq!(state.star_on_arrival.held(), 0);
        assert!(state.local_library.parse_song_id("ext-deezer-1").0);
    }

    /// SoulseekClient reads its settings once: the address it started with stays, and the
    /// metadata service shares the registry the stores hold.
    #[tokio::test]
    async fn the_soulseek_client_keeps_the_address_it_started_with() {
        use crate::services::i_music_metadata_service::IMusicMetadataService;
        use octo_core::settings::SoulseekSettings;

        let state = AppState::for_tests(AppSettings {
            soulseek: SoulseekSettings {
                base_url: Some("http://slskd:5030/".into()),
                ..Default::default()
            },
            ..Default::default()
        });
        state.settings.set(AppSettings::default());
        assert_eq!(state.soulseek_client.base_url(), "http://slskd:5030");

        let songs = state
            .music_metadata
            .search_songs_by_artist_title("Justice", "Genesis", 1, None)
            .await;
        assert!(state.external_id_registry.lookup(&songs[0].id).is_some());
    }

    /// Lidarr: the client reads the live settings, so with none saved nothing is asked and
    /// the track fetcher refuses before any lookup.
    #[tokio::test]
    async fn the_lidarr_services_refuse_until_lidarr_is_set_up() {
        use crate::services::lidarr::{LidarrError, LidarrTrackRequest};

        let state = AppState::for_tests(AppSettings::default());
        assert!(!state.lidarr_client.is_reachable().await);
        let request = LidarrTrackRequest {
            artist: "Massive Attack".into(),
            title: "Teardrop".into(),
            album: None,
            duration_seconds: None,
            lossless_only: true,
            original_path: None,
            release: None,
            step: None,
        };
        let refused = state
            .lidarr_track_fetcher
            .fetch(&request, "/nonexistent", &CancellationToken::new())
            .await;
        assert!(
            matches!(refused, Err(LidarrError::InvalidOperation(_))),
            "{refused:?}"
        );
        assert!(!state.lidarr_album_claims.upgrade_busy("anything"));
        assert!(state.lidarr_import_handoff.take("anything").is_none());
    }

    /// Last.fm radio: the warmup and the refresh worker run as workers, the refresh worker
    /// hands its rebuilds to the warmup, and the stores share the external id registry.
    #[tokio::test]
    async fn the_radio_is_wired_with_its_workers() {
        let state = AppState::build(SettingsStore::from_env(Vec::new(), None));
        let names: Vec<String> = state.workers.status().into_iter().map(|s| s.name).collect();
        for name in [
            "LastFmRadioWarmupService",
            "LastFmRadioRefreshWorker",
            "SonicAnalysisWorker",
            "RadioOutcomeStore",
        ] {
            assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
        }

        let state = AppState::for_tests(AppSettings::default());
        assert!(state.last_fm_radio_state.known_users().is_empty());
        assert!(state.last_fm_radio_refresh_queue.enqueue("alice", None));
        assert!(state.last_fm_radio_warmup.queue_user("alice"));
        assert!(!state.last_fm_radio_warmup.queue_user("ALICE"));
        assert!(
            state
                .last_fm_radio_state
                .path()
                .ends_with("lastfm-radio-state.json")
        );
        assert!(state.generated_playlists.find("alice", "og1").is_none());
        // Radio's other state files sit beside the radio state, and every source has a weight.
        assert!(state.radio_outcomes.path().ends_with("radio-outcomes.json"));
        assert!(state.sonic_store.path().ends_with("sonic-features.json"));
        assert_eq!(state.sonic_worker.status().state, "Waiting");
        assert_eq!(state.radio_sources.weights(None, false).len(), 5);
        let token = state
            .last_fm_radio_stream_sessions
            .issue("alice", "or1", [("u", "alice")], None);
        let session = state.last_fm_radio_stream_sessions.get(&token, None).unwrap();
        assert!(state.last_fm_radio_streams.resolve(&session).is_none());
    }

    /// The sync catalog is one shared instance; nothing is built or pinned until a walk asks.
    #[test]
    fn the_sync_catalog_is_a_singleton_with_nothing_built() {
        use crate::services::subsonic::SyncCatalogKind;
        let state = AppState::for_tests(AppSettings::default());
        assert!(state.sync_catalog.try_get_song("alice", "ph-1").is_none());
        assert!(
            state
                .sync_catalog
                .pinned_catalog("alice", SyncCatalogKind::Song)
                .is_none()
        );
    }

    /// Library actions: the journal's flusher and both workers run, the journal sits beside
    /// settings.json, and the Lidarr heart shares it.
    #[tokio::test]
    async fn library_actions_are_wired_with_their_workers() {
        let state = AppState::build(SettingsStore::from_env(Vec::new(), None));
        let names: Vec<String> = state.workers.status().into_iter().map(|s| s.name).collect();
        for name in [
            "LibraryActionJournal",
            "LibraryActionPlaylistWorker",
            "LibraryActionRatingWorker",
        ] {
            assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
        }

        let state = AppState::for_tests(AppSettings::default());
        assert!(
            state
                .library_action_journal
                .path()
                .is_some_and(|p| p.ends_with("library-actions.json"))
        );
        assert!(Arc::ptr_eq(
            state.library_action_executor.journal(),
            &state.library_action_journal
        ));
        let off = state
            .library_action_executor
            .apply(crate::services::library::LibraryActionRequest::new(
                octo_core::settings::LibraryAction::Delete,
                "nd-1",
                "alice",
            ))
            .await
            .expect("an outcome");
        assert_eq!(off.detail.as_deref(), Some("Library actions are off."));
        assert_eq!(state.library_action_rating_worker.pending(), 0);
        assert!(!state.upgrade_sources.ready());
    }

    /// The genre backfill and the cover upgrade: their stores flush and their workers run
    /// as workers, and the handler-test state keeps their runs in memory.
    #[tokio::test]
    async fn the_genre_backfill_and_the_cover_upgrade_are_wired_with_their_workers() {
        let state = AppState::build(SettingsStore::from_env(Vec::new(), None));
        let names: Vec<String> = state.workers.status().into_iter().map(|s| s.name).collect();
        for name in [
            "GenreBackfillStore",
            "GenreBackfillWorker",
            "CoverUpgradeStore",
            "CoverUpgradeWorker",
        ] {
            assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
        }

        let state = AppState::for_tests(AppSettings::default());
        assert!(!state.genre_backfill_worker.is_running());
        assert!(!state.genre_backfill_worker.journal().exists());
        assert!(!state.cover_upgrade_worker.is_busy());
        assert!(!state.cover_upgrade_worker.can_undo());
        assert_eq!(
            state.cover_upgrade_worker.current().status,
            crate::services::cover_art::CoverUpgradeStatus::Idle
        );
    }

    /// The queues and sweeps: the stores sit beside settings.json, their flushers and the five
    /// background services run as workers, and the queues are the ones library actions and
    /// hearts already use.
    #[tokio::test]
    async fn the_queues_and_sweeps_are_wired_with_their_workers() {
        let state = AppState::build(SettingsStore::from_env(Vec::new(), None));
        let names: Vec<String> = state.workers.status().into_iter().map(|s| s.name).collect();
        for name in [
            "NoticeQueue",
            "ReviewSweepStore",
            "NoticePlaylistWorker",
            "DuplicateScanWorker",
            "UpgradeWorker",
            "QualityUpgradeWorker",
            "LibraryReviewSweepWorker",
            "YoutubeCheckStore",
            "LibraryYoutubeCheck",
            "IdentitySweepStore",
            "LibraryIdentitySweep",
        ] {
            assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
        }

        let state = AppState::for_tests(AppSettings::default());
        assert!(
            state
                .notice_queue
                .path()
                .is_some_and(|p| p.ends_with("notice-queue.json"))
        );
        assert!(
            state
                .upgrade_queue
                .path()
                .is_some_and(|p| p.ends_with("upgrades.json"))
        );
        assert!(
            state
                .review_sweep_store
                .path()
                .is_some_and(|p| p.ends_with("review-sweep.json"))
        );
        assert!(
            state
                .identity_sweep_store
                .path()
                .is_some_and(|p| p.ends_with("identity-sweep.json"))
        );
        assert!(state.upgrade_queue.snapshot().is_empty());
        assert_eq!(
            state.quality_upgrade_worker.tick().await,
            crate::services::library::quality_upgrade_worker::Tick::Off
        );
        assert_eq!(state.library_review_sweep_worker.status().state, "Off");
        // The one-time YouTube check waits for a person to start it.
        assert_eq!(state.library_youtube_check.status().state, "Idle");
        assert!(!state.library_youtube_check.store().read(|s| s.running));
        assert!(state.duplicate_scan_worker.last_result().is_none());
        // The verification service is set, but with no AcoustID key the sweep is not set up.
        use crate::services::library::IReviewSweepVerifier;
        assert!(!state.fingerprint_sweep_verifier.is_ready());
        assert_eq!(state.upgrade_worker.running(), 0);
    }

    /// With Family off nothing of it runs: no worker, no key file, no family file written.
    #[test]
    fn family_off_starts_nothing() {
        let state = AppState::build(SettingsStore::from_env(Vec::new(), None));
        let names: Vec<String> = state.workers.status().into_iter().map(|s| s.name).collect();
        assert!(!names.iter().any(|n| n == "FamilyStore"), "{names:?}");

        let dir = tempfile::tempdir().expect("a temp dir");
        let state = AppState::for_tests_with(
            AppSettings::default(),
            TestServices {
                config_dir: Some(dir.path().to_path_buf()),
                ..Default::default()
            },
        );
        state.start_family();
        assert!(state.workers.status().is_empty());
        assert!(!state.family_vault.has_key());
        assert!(!dir.path().join("family.key").exists());
        assert!(!dir.path().join("family.json").exists());
    }

    /// Turning Family on makes the key, the store's flush worker and the library index worker, once.
    #[test]
    fn family_on_makes_the_key_and_the_flush_worker_once() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut settings = AppSettings::default();
        settings.family.enabled = true;
        let state = AppState::for_tests_with(
            settings,
            TestServices {
                config_dir: Some(dir.path().to_path_buf()),
                ..Default::default()
            },
        );
        state.start_family();
        state.start_family();
        let flushers = state
            .workers
            .status()
            .into_iter()
            .filter(|s| s.name == "FamilyStore")
            .count();
        assert_eq!(flushers, 1);
        let sweepers = state
            .workers
            .status()
            .into_iter()
            .filter(|s| s.name == "FamilyHandover")
            .count();
        assert_eq!(sweepers, 1, "hand-overs are swept once Family is on");
        let indexes = state
            .workers
            .status()
            .into_iter()
            .filter(|s| s.name == "LibraryIndex")
            .count();
        assert_eq!(indexes, 1);
        assert!(state.family_vault.has_key());
        assert_eq!(
            std::fs::read(dir.path().join("family.key")).expect("made").len(),
            64
        );
        let sealed = state.family_vault.seal("hidden").expect("a key");
        assert_eq!(state.family_vault.open_sealed(&sealed).as_deref(), Some("hidden"));
    }

    fn worker_count(state: &AppState, name: &str) -> usize {
        state
            .workers
            .status()
            .into_iter()
            .filter(|s| s.name == name)
            .count()
    }

    /// Family turned on by editing `settings.json` (read again by the settings watcher) starts
    /// its work as the Family page's switch does.
    #[tokio::test]
    async fn family_turned_on_in_the_settings_file_starts_its_work() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let state = AppState::for_tests_with(
            AppSettings::default(),
            TestServices {
                config_dir: Some(dir.path().to_path_buf()),
                ..Default::default()
            },
        );
        let watching = tokio::spawn(watch_for_family(
            Arc::downgrade(&state.inner),
            CancellationToken::new(),
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(worker_count(&state, "FamilyStore"), 0);
        let mut on = state.settings.current().as_ref().clone();
        on.family.enabled = true;
        state.settings.set(on);
        tokio::time::timeout(Duration::from_secs(2), watching)
            .await
            .expect("the watch ends")
            .expect("joins")
            .expect("ok");
        assert_eq!(worker_count(&state, "FamilyStore"), 1);
        assert_eq!(worker_count(&state, "LibraryIndex"), 1);
        assert_eq!(worker_count(&state, "FamilyWork"), 1);
        assert!(state.family_vault.has_key());
    }

    /// With Family off, members' libraries kept from a time it was on keep the library index
    /// read, and Family being turned on is watched for.
    #[test]
    fn family_off_with_members_libraries_reads_the_index_and_watches_the_switch() {
        let state = AppState::for_tests(AppSettings::default());
        state.watch_family_switch();
        assert_eq!(worker_count(&state, "LibraryIndex"), 0, "nobody has a library");
        assert_eq!(worker_count(&state, "FamilySwitch"), 1);

        let state = AppState::for_tests(AppSettings::default());
        state
            .family_store
            .upsert_member(crate::services::family::test_support::member(
                "alex",
                octo_core::family::abilities::Preset::Member,
                3,
            ));
        state.watch_family_switch();
        assert_eq!(worker_count(&state, "LibraryIndex"), 1);
        state.inner.start_library_index();
        assert_eq!(worker_count(&state, "LibraryIndex"), 1, "read once");
    }

    #[test]
    fn stop_application_cancels_the_lifetime() {
        let state = AppState::for_tests(AppSettings::default());
        assert!(!state.lifetime.is_cancelled());
        state.stop_application();
        assert!(state.lifetime.is_cancelled());
    }
}
