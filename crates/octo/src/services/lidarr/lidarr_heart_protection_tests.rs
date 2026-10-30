//! A heart through Lidarr keeps the promises a Soulseek heart does. What Lidarr imports goes
//! through the download pipeline song by song, so it meets the same checks; a song already in the
//! library is taken back out (an owned MP3 is queued for Better quality instead), a song removed
//! with a library action stays removed, and the album's monitoring goes back to how it was. The
//! heart learns whether its songs landed, so the next source can try what Lidarr could not get.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures::FutureExt;
use octo_core::models::domain::{Album, Song};
use octo_core::models::download::DownloadInfo;
use octo_core::settings::{
    AppSettings, DownloadSource, LibraryAction, LibraryActionDefinition, LibraryActionSettings,
    LidarrSettings, SettingsStore, SubsonicSettings,
};
use octo_core::soulseek::{RoutingKind, SoulseekRouting};
use parking_lot::Mutex;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wiremock::MockServer;

use super::*;
use crate::services::common::test_fakes::{FakeMetadata, until};
use crate::services::i_download_service::{AudioStream, DirectStreamInfo, IDownloadService};
use crate::services::library::library_ownership::Candidate;
use crate::services::library::{
    LibraryActionEntry, LibraryActionJournal, LibraryActionState, ReplacementHandoff,
};
use crate::services::lidarr::fake_lidarr::{ALBUM_ID, FakeLidarr};
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::metadata::{DeezerRateLimitHandler, DeezerRateLimiter};

/// One file the pipeline took: its id, its song's title, and whether it landed quietly.
type Taken = Arc<Mutex<Vec<(String, String, bool)>>>;

/// The pipeline as far as Lidarr's files go: take the offered file, or refuse it the way
/// AcoustID refuses a wrong recording, which leaves it where Lidarr put it.
struct Pipeline {
    root: std::path::PathBuf,
    ids: Arc<ExternalIdRegistry>,
    imports: Arc<LidarrImportHandoff>,
    refused: Arc<Mutex<HashSet<String>>>,
    taken: Taken,
}

#[async_trait]
impl IDownloadService for Pipeline {
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

    fn download_remaining_album_tracks_in_background(&self, _: &str, _: &str, _: &str) {}

    async fn execute_acquisition(
        &self,
        provider: &str,
        id: &str,
        trigger_album_download: bool,
        force_permanent: bool,
        source_override: Option<DownloadSource>,
        _: &CancellationToken,
        _: Option<Vec<String>>,
        upgrade_search: bool,
        replacement: Option<Arc<ReplacementHandoff>>,
    ) -> anyhow::Result<String> {
        // The heart must ask the pipeline with exactly these arguments.
        assert_eq!(
            (
                provider,
                trigger_album_download,
                force_permanent,
                source_override,
                upgrade_search
            ),
            ("soulseek", false, true, Some(DownloadSource::Lidarr), false)
        );
        assert!(replacement.is_none());
        let title = self
            .ids
            .lookup(id)
            .and_then(|r| r.snapshot().title)
            .unwrap_or_else(|| id.to_string());
        if self.refused.lock().contains(&title) {
            anyhow::bail!("AcoustID identified Lidarr's file as a live recording");
        }
        let import = self
            .imports
            .take(id)
            .ok_or_else(|| anyhow::anyhow!("nothing offered for {id}"))?;
        let placed = self.root.join(format!("Massive Attack - {title}.flac"));
        std::fs::rename(&import.path, &placed)?;
        self.taken.lock().push((id.to_string(), title, import.quiet));
        Ok(placed.to_string_lossy().into_owned())
    }

    async fn download_album_with_source(
        &self,
        _: &str,
        _: &str,
        _: DownloadSource,
        _: bool,
        _: &CancellationToken,
        _: Option<Vec<String>>,
    ) -> anyhow::Result<bool> {
        anyhow::bail!("not set up")
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

struct Fixture {
    root: TempDir,
    lidarr: FakeLidarr,
    server: MockServer,
    claims: Arc<LidarrAlbumClaims>,
    journal: Arc<LibraryActionJournal>,
    upgrades: Arc<UpgradeQueue>,
    imports: Arc<LidarrImportHandoff>,
    ids: Arc<ExternalIdRegistry>,
    refused: Arc<Mutex<HashSet<String>>>,
    taken: Taken,
    owned: Arc<Mutex<Vec<Candidate>>>,
    pending: Arc<LidarrPendingAlbums>,
}

fn upgrades_on() -> LibraryActionSettings {
    LibraryActionSettings {
        enabled: true,
        dry_run: false,
        allowed_users: vec!["alice".into()],
        actions: vec![LibraryActionDefinition {
            action: LibraryAction::BetterQuality,
            enabled: true,
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn owned(id: &str, title: &str, suffix: &str, bit_rate: i32) -> Candidate {
    Candidate {
        id: id.into(),
        artist: "Massive Attack".into(),
        title: title.into(),
        album: Some("Mezzanine".into()),
        duration: Some(330),
        suffix: suffix.into(),
        bit_rate,
        isrcs: Vec::new(),
    }
}

impl Fixture {
    async fn new() -> Fixture {
        let root = tempfile::tempdir().expect("a temp dir");
        let (lidarr, server) = FakeLidarr::start(root.path()).await;
        Fixture {
            root,
            lidarr,
            server,
            claims: Arc::new(LidarrAlbumClaims::new()),
            journal: Arc::new(LibraryActionJournal::new()),
            upgrades: Arc::new(UpgradeQueue::new()),
            imports: Arc::new(LidarrImportHandoff::new()),
            ids: Arc::new(ExternalIdRegistry::in_memory()),
            refused: Arc::default(),
            taken: Arc::default(),
            owned: Arc::default(),
            pending: Arc::new(LidarrPendingAlbums::new(None)),
        }
    }

    fn id(&self, title: &str) -> String {
        self.ids.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("Massive Attack".into()),
            title: Some(title.into()),
            album: Some("Mezzanine".into()),
            ..Default::default()
        })
    }

    fn service(
        &self,
        actions: Option<LibraryActionSettings>,
        timeout_seconds: i32,
    ) -> LidarrHeartAcquisitionService {
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            lidarr: LidarrSettings {
                base_url: Some(self.server.uri()),
                api_key: Some("k".into()),
                root_folder_path: Some("/data/music".into()),
                quality_profile_id: 1,
                metadata_profile_id: 1,
                import_timeout_seconds: timeout_seconds,
                ..Default::default()
            },
            subsonic: SubsonicSettings {
                url: Some("http://navidrome.test".into()),
                auto_detect_download_path: false,
                skip_owned_songs: true,
                ..Default::default()
            },
            library_actions: actions.unwrap_or_default(),
            ..Default::default()
        }));
        settings.set_raw("Library:DownloadPath", Some(&self.root.path().to_string_lossy()));
        let identity = NavidromeIdentityService::new(settings.clone(), reqwest::Client::new());

        let metadata = Arc::new(FakeMetadata::default());
        let song = |title: &str, track: i32| Song {
            title: title.into(),
            artist: "Massive Attack".into(),
            album: "Mezzanine".into(),
            track: Some(track),
            external_provider: Some("soulseek".into()),
            external_id: Some(self.id(title)),
            ..Default::default()
        };
        // A track heart: Deezer found Teardrop on its single, where it is track 1.
        metadata
            .songs
            .lock()
            .insert(("soulseek".into(), "teardrop-heart".into()), song("Teardrop", 1));
        metadata.albums.lock().insert(
            ("soulseek".into(), "mezzanine".into()),
            Album {
                title: "Mezzanine".into(),
                artist: "Massive Attack".into(),
                songs: vec![song("Angel", 1), song("Teardrop", 3)],
                ..Default::default()
            },
        );

        let ownership = Arc::new(LibraryOwnership::new(
            settings.clone(),
            None,
            Arc::new(FakeLocalLibrary::default()),
        ));
        let library = self.owned.clone();
        ownership.set_search(Arc::new(move |_| {
            let candidates = library.lock().clone();
            async move { Ok(Some(candidates)) }.boxed()
        }));
        let dir = self.root.path().join("owned");
        ownership.set_resolve(Arc::new(move |id| {
            let dir = dir.clone();
            async move {
                std::fs::create_dir_all(&dir)?;
                let path = dir.join(format!("{id}.file"));
                std::fs::write(&path, [1])?;
                Ok(Some(path.to_string_lossy().into_owned()))
            }
            .boxed()
        }));

        // Deezer answers nothing here, so a track heart files the song under its own album.
        let deezer = Arc::new(DeezerMetadataService::with_base_url(
            Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new()))),
            settings.clone(),
            "http://127.0.0.1:1",
        ));
        let downloads: Arc<dyn IDownloadService> = Arc::new(Pipeline {
            root: self.root.path().to_path_buf(),
            ids: self.ids.clone(),
            imports: self.imports.clone(),
            refused: self.refused.clone(),
            taken: self.taken.clone(),
        });
        let service = LidarrHeartAcquisitionService::new(
            Arc::new(LidarrClient::new(settings.clone())),
            metadata,
            deezer,
            settings.clone(),
            identity,
            Arc::new(NotificationService::new(Vec::new(), settings, None)),
            LidarrHeartExtras {
                claims: Some(self.claims.clone()),
                imports: Some(self.imports.clone()),
                ids: Some(self.ids.clone()),
                downloads: Some(downloads),
                journal: Some(self.journal.clone()),
                ownership: Some(ownership),
                upgrades: Some(self.upgrades.clone()),
                pending: Some(self.pending.clone()),
                ..Default::default()
            },
        );
        service.set_poll_override(Some(Duration::from_millis(20)));
        service
    }

    async fn heart_the_album(&self, actions: Option<LibraryActionSettings>) -> bool {
        self.lidarr
            .on_search(&[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);
        let service = self.service(actions, 30);
        within(service.try_acquire_album(
            "soulseek",
            "mezzanine",
            true,
            Some("alice"),
            AcquisitionOrigin::AlbumHeart,
            &[],
        ))
        .await
    }

    fn taken(&self) -> Vec<String> {
        let mut titles: Vec<String> = self.taken.lock().iter().map(|t| t.1.clone()).collect();
        titles.sort();
        titles
    }

    fn quiet(&self, title: &str) -> bool {
        self.taken
            .lock()
            .iter()
            .find(|t| t.1 == title)
            .map(|t| t.2)
            .expect("taken")
    }

    /// The heart is answered as soon as its job ends; giving the album back to Lidarr's
    /// monitoring and letting go of the claim come right after, over HTTP, so they are waited
    /// for.
    async fn job_wound_up(&self, unmonitored: bool) {
        until(|| !self.claims.heart_busy(ALBUM_ID)).await;
        if unmonitored {
            until(|| self.lidarr.monitor_changes().contains(&false)).await;
        }
    }
}

async fn within(work: impl Future<Output = anyhow::Result<bool>>) -> bool {
    tokio::time::timeout(Duration::from_secs(20), work)
        .await
        .expect("the heart is answered")
        .expect("a heart never throws")
}

#[tokio::test]
async fn every_song_goes_through_the_pipeline() {
    let f = Fixture::new().await;

    assert!(f.heart_the_album(None).await);

    assert_eq!(f.taken(), ["Angel", "Teardrop"]);
    assert!(f.lidarr.deleted().is_empty());
    // An album heart gets one notice for the album, so its songs land quietly.
    assert!(f.taken.lock().iter().all(|t| t.2));
    f.job_wound_up(true).await;
    assert!(f.lidarr.monitor_changes().contains(&false));
    assert!(!f.claims.heart_busy(ALBUM_ID));
}

#[tokio::test]
async fn a_song_already_in_the_library_is_taken_back_out() {
    let f = Fixture::new().await;
    *f.owned.lock() = vec![owned("nd-9", "Teardrop", "flac", 1000)];

    assert!(f.heart_the_album(None).await);

    assert_eq!(f.taken(), ["Angel"]);
    assert_eq!(f.lidarr.deleted().len(), 1);
    assert!(f.upgrades.snapshot().is_empty());
}

#[tokio::test]
async fn an_owned_mp3_is_queued_for_better_quality_instead() {
    let f = Fixture::new().await;
    *f.owned.lock() = vec![owned("nd-9", "Teardrop", "mp3", 320)];

    assert!(f.heart_the_album(Some(upgrades_on())).await);

    assert_eq!(f.taken(), ["Angel"]);
    assert_eq!(f.lidarr.deleted().len(), 1);
    let jobs = f.upgrades.snapshot();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].navidrome_id, "nd-9");
    assert_eq!(jobs[0].requested_by, "alice");
}

#[tokio::test]
async fn a_removed_song_stays_removed() {
    let f = Fixture::new().await;
    f.journal.record(LibraryActionEntry {
        key: "delete:angel".into(),
        action: LibraryAction::Delete,
        navidrome_id: "nd-1".into(),
        username: "alice".into(),
        title: "Angel".into(),
        artist: "Massive Attack".into(),
        album: "Mezzanine".into(),
        source_path: None,
        quarantine_path: None,
        resolution: None,
        state: LibraryActionState::Applied,
        detail: Some("Removed.".into()),
        dry_run: false,
        at_utc: Utc::now(),
        history_kept: None,
        revealed_path: None,
        copy: false,
        download_allowed: false,
        denied_source: None,
    });

    // Removed means not wanted: the album is not short of anything the next source should fetch.
    assert!(!f.heart_the_album(None).await);

    assert_eq!(f.taken(), ["Teardrop"]);
    assert_eq!(f.lidarr.deleted().len(), 1);
}

#[tokio::test]
async fn a_song_the_pipeline_refuses_is_deleted_and_left_to_the_next_source() {
    let f = Fixture::new().await;
    f.refused.lock().insert("Teardrop".into());

    assert!(!f.heart_the_album(None).await);

    assert_eq!(f.taken(), ["Angel"]);
    // Refused before it was taken, so it is still where Lidarr put it, and goes through Lidarr.
    assert_eq!(f.lidarr.deleted().len(), 1);
}

/// A song heart: Lidarr still brings the whole album, the hearted song lands with its own
/// notice, the other song lands quietly, and Angel (track 1 on the album) is never taken
/// for Teardrop, which is track 1 on the single Deezer found it on.
#[tokio::test]
async fn a_song_heart_lands_its_song_with_a_notice_and_the_rest_quietly() {
    let f = Fixture::new().await;
    f.lidarr
        .on_search(&[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);

    let service = f.service(None, 30);
    assert!(
        within(service.try_acquire_track(
            "soulseek",
            "teardrop-heart",
            true,
            Some("alice"),
            AcquisitionOrigin::Heart,
            &[],
        ))
        .await
    );

    assert_eq!(f.taken(), ["Angel", "Teardrop"]);
    assert!(!f.quiet("Teardrop"));
    assert!(f.quiet("Angel"));
    let id_of = |title: &str| {
        f.taken
            .lock()
            .iter()
            .find(|t| t.1 == title)
            .map(|t| t.0.clone())
            .expect("taken")
    };
    assert_eq!(id_of("Teardrop"), f.id("Teardrop"));
    assert_ne!(id_of("Angel"), f.id("Teardrop"));
}

#[tokio::test]
async fn nothing_arriving_in_time_is_a_miss_for_the_next_source() {
    let f = Fixture::new().await;

    let service = f.service(None, 1);
    let landed = within(service.try_acquire_album(
        "soulseek",
        "mezzanine",
        true,
        Some("alice"),
        AcquisitionOrigin::AlbumHeart,
        &[],
    ))
    .await;

    assert!(!landed);
    assert!(f.taken.lock().is_empty());
    f.job_wound_up(true).await;
    assert!(!f.claims.heart_busy(ALBUM_ID));
}

#[tokio::test]
async fn a_second_heart_on_the_same_album_joins_and_is_answered_too() {
    let f = Fixture::new().await;
    let service = f.service(None, 30);

    let first = {
        let service = service.clone();
        tokio::spawn(async move {
            within(service.try_acquire_album(
                "soulseek",
                "mezzanine",
                true,
                Some("alice"),
                AcquisitionOrigin::AlbumHeart,
                &[],
            ))
            .await
        })
    };
    until(|| f.lidarr.searches() == 1).await;
    let second = {
        let service = service.clone();
        tokio::spawn(async move {
            within(service.try_acquire_album(
                "soulseek",
                "mezzanine",
                true,
                Some("bob"),
                AcquisitionOrigin::AlbumHeart,
                &[],
            ))
            .await
        })
    };
    // Bob has joined the running job before Lidarr brings anything.
    until(|| service.askers_of(ALBUM_ID).is_some_and(|a| a.len() == 2)).await;
    // The release: Lidarr imports both songs.
    f.lidarr.import(1, "Angel", "FLAC");
    f.lidarr.import(3, "Teardrop", "FLAC");

    assert!(first.await.expect("the task"));
    assert!(second.await.expect("the task"));
    assert_eq!(f.lidarr.searches(), 1);
    assert_eq!(f.taken(), ["Angel", "Teardrop"]);
}

// That a track heart never matches by track number is tested with match_song, in
// octo_core::lidarr::lidarr_heart_acquisition_service.

/// An upgrade borrowing the album holds the heart back until it lets go.
#[tokio::test]
async fn a_heart_waits_for_an_upgrade_on_the_album() {
    let f = Fixture::new().await;
    f.lidarr
        .on_search(&[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);
    f.claims.upgrade_started(ALBUM_ID);
    let service = f.service(None, 30);
    service.set_claim_poll(Duration::from_millis(10));

    let heart = {
        let service = service.clone();
        tokio::spawn(async move {
            within(service.try_acquire_album(
                "soulseek",
                "mezzanine",
                true,
                Some("alice"),
                AcquisitionOrigin::AlbumHeart,
                &[],
            ))
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(f.lidarr.searches(), 0);
    f.claims.upgrade_ended(ALBUM_ID);

    assert!(heart.await.expect("the task"));
    assert_eq!(f.lidarr.searches(), 1);
}

/// A heart Lidarr cannot place fails as itself, never as an error to the coordinator.
#[tokio::test]
async fn an_album_lidarr_does_not_know_is_a_plain_miss() {
    let f = Fixture::new().await;
    f.lidarr.set_knows_album(false);

    let service = f.service(None, 30);
    assert!(
        !within(service.try_acquire_album(
            "soulseek",
            "mezzanine",
            true,
            Some("alice"),
            AcquisitionOrigin::AlbumHeart,
            &[],
        ))
        .await
    );
    assert!(
        !within(service.try_acquire_track("soulseek", "gone", true, None, AcquisitionOrigin::Heart, &[]))
            .await
    );
    assert_eq!(f.lidarr.searches(), 0);
}

#[test]
fn relative_paths_and_imported_path_edge_cases() {
    for (from, to, expected) in [
        ("/data/music", "/data/music", "."),
        ("/data/music/", "/data/music/a/b.flac", "a/b.flac"),
        ("/data/music", "/data/other/x", "../other/x"),
        ("/", "/a", "a"),
    ] {
        assert_eq!(get_relative_path(from, to), expected, "{from} -> {to}");
    }
    // A folder whose name starts with two dots is inside the root, but the escape check on
    // Octo's side reads it as a climb out, since the path starts with "..".
    assert!(translate_imported_path("/data/music/..odd/x.flac", Some("/data/music"), "/music").is_err());
    assert_eq!(
        translate_imported_path("/data/music/x.flac", Some(" "), "/music"),
        Err(LidarrError::InvalidOperation(
            "Lidarr root folder is not configured.".into()
        ))
    );
}

// ---- Lidarr's webhook ------------------------------------------------------------------------

fn imported(paths: &[&str]) -> LidarrImportEvent {
    LidarrImportEvent {
        foreign_album_id: ALBUM_ID.into(),
        artist: Some("Massive Attack".into()),
        album: Some("Mezzanine".into()),
        paths: paths.iter().map(|p| p.to_string()).collect(),
        is_upgrade: false,
    }
}

const ANGEL: &str = "/data/music/Massive Attack/Mezzanine/01 - Angel.flac";
const TEARDROP: &str = "/data/music/Massive Attack/Mezzanine/03 - Teardrop.flac";

#[tokio::test]
async fn lidarrs_webhook_wakes_the_waiting_heart_at_once() {
    let f = Fixture::new().await;
    f.lidarr
        .on_search(&[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);
    f.lidarr.hold_imports();
    let service = f.service(None, 600);
    // Without the webhook the heart would look again only in an hour.
    service.set_poll_override(Some(Duration::from_secs(3600)));

    let heart = {
        let service = service.clone();
        tokio::spawn(async move {
            within(service.try_acquire_album(
                "soulseek",
                "mezzanine",
                true,
                Some("alice"),
                AcquisitionOrigin::AlbumHeart,
                &[],
            ))
            .await
        })
    };
    until(|| f.lidarr.searches() == 1).await;
    // Let the job take its first look, find nothing, and settle into its wait.
    tokio::time::sleep(Duration::from_millis(200)).await;
    f.lidarr.release_imports();

    assert_eq!(
        service.lidarr_imported(imported(&[ANGEL, TEARDROP])).await,
        LidarrImportOutcome::Woke
    );
    assert!(heart.await.expect("the task"));
    assert_eq!(f.taken(), ["Angel", "Teardrop"]);
}

#[tokio::test]
async fn an_import_after_the_heart_stopped_waiting_is_still_taken() {
    let f = Fixture::new().await;
    f.lidarr
        .on_search(&[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);
    f.lidarr.hold_imports();
    let service = f.service(None, 1);
    assert!(
        !within(service.try_acquire_album(
            "soulseek",
            "mezzanine",
            true,
            Some("alice"),
            AcquisitionOrigin::AlbumHeart,
            &[],
        ))
        .await
    );
    f.job_wound_up(true).await;
    assert!(f.pending.get(ALBUM_ID).is_some());

    // Lidarr finishes later, after a restart for all Octo knows: a new service over the same
    // pending list.
    f.lidarr.release_imports();
    let restarted = f.service(None, 1);
    assert_eq!(
        restarted.lidarr_imported(imported(&[ANGEL, TEARDROP])).await,
        LidarrImportOutcome::Late
    );

    until(|| f.taken.lock().len() == 2).await;
    assert_eq!(f.taken(), ["Angel", "Teardrop"]);
    // Nobody waits on these any more, so they land quietly.
    assert!(f.taken.lock().iter().all(|t| t.2));
    until(|| !f.claims.heart_busy(ALBUM_ID)).await;
    assert!(f.pending.get(ALBUM_ID).is_none());
}

#[tokio::test]
async fn a_late_import_takes_only_the_files_lidarr_names() {
    let f = Fixture::new().await;
    f.lidarr
        .on_search(&[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);
    f.lidarr.hold_imports();
    let service = f.service(None, 1);
    assert!(
        !within(service.try_acquire_album(
            "soulseek",
            "mezzanine",
            true,
            Some("alice"),
            AcquisitionOrigin::AlbumHeart,
            &[],
        ))
        .await
    );
    f.job_wound_up(true).await;
    f.lidarr.release_imports();

    assert_eq!(
        service.lidarr_imported(imported(&[ANGEL])).await,
        LidarrImportOutcome::Late
    );

    until(|| !f.claims.heart_busy(ALBUM_ID)).await;
    assert_eq!(f.taken(), ["Angel"]);
}

/// Angel in the library, as long as Lidarr says it runs.
fn owned_angel(suffix: &str, bit_rate: i32) -> Candidate {
    Candidate {
        duration: Some(380),
        ..owned("nd-1", "Angel", suffix, bit_rate)
    }
}

/// The owner already had Angel as an MP3 when Octo asked for the album; Teardrop is what the
/// heart waited for, until it gave up.
async fn heart_gave_up_with_angel_owned(f: &Fixture) -> LidarrHeartAcquisitionService {
    f.lidarr.add_album(true);
    f.lidarr.import(1, "Angel", "MP3-320");
    *f.owned.lock() = vec![owned_angel("mp3", 320)];
    f.lidarr.on_search(&[(3, "Teardrop", "FLAC")]);
    f.lidarr.hold_imports();
    let service = f.service(None, 1);
    assert!(
        !within(service.try_acquire_album(
            "soulseek",
            "mezzanine",
            true,
            Some("alice"),
            AcquisitionOrigin::AlbumHeart,
            &[],
        ))
        .await
    );
    f.job_wound_up(false).await;
    assert!(f.pending.get(ALBUM_ID).is_some());
    service
}

#[tokio::test]
async fn a_late_upgrade_of_the_owners_own_track_is_left_alone() {
    let f = Fixture::new().await;
    let service = heart_gave_up_with_angel_owned(&f).await;

    // Lidarr upgrades the owner's Angel to FLAC on its own: a new file id at a new path.
    let angel = f.lidarr.upgrade(1, "Angel", "FLAC");
    let mut event = imported(&[ANGEL]);
    event.is_upgrade = true;
    assert_eq!(service.lidarr_imported(event).await, LidarrImportOutcome::NotOurs);

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(f.lidarr.deleted().is_empty());
    assert!(Path::new(&angel).is_file());
    assert_eq!(f.taken(), Vec::<String>::new());
}

#[tokio::test]
async fn a_late_import_skips_tracks_that_had_a_file_before_the_search() {
    let f = Fixture::new().await;
    let service = heart_gave_up_with_angel_owned(&f).await;

    // One import names both: the owner's upgraded Angel, which Navidrome has not seen yet, and
    // the Teardrop Octo asked for.
    f.owned.lock().clear();
    f.lidarr.release_imports();
    let angel = f.lidarr.upgrade(1, "Angel", "FLAC");
    assert_eq!(
        service.lidarr_imported(imported(&[ANGEL, TEARDROP])).await,
        LidarrImportOutcome::Late
    );

    until(|| !f.claims.heart_busy(ALBUM_ID)).await;
    assert_eq!(f.taken(), ["Teardrop"]);
    assert!(f.lidarr.deleted().is_empty());
    assert!(Path::new(&angel).is_file());
}

#[tokio::test]
async fn a_late_import_never_deletes_through_lidarr() {
    let f = Fixture::new().await;
    f.lidarr
        .on_search(&[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);
    f.lidarr.hold_imports();
    let service = f.service(None, 1);
    assert!(
        !within(service.try_acquire_album(
            "soulseek",
            "mezzanine",
            true,
            Some("alice"),
            AcquisitionOrigin::AlbumHeart,
            &[],
        ))
        .await
    );
    f.job_wound_up(true).await;
    // Teardrop is refused by the pipeline, Angel is already in the library.
    f.refused.lock().insert("Teardrop".into());
    *f.owned.lock() = vec![owned_angel("flac", 1000)];
    f.lidarr.release_imports();

    assert_eq!(
        service.lidarr_imported(imported(&[ANGEL, TEARDROP])).await,
        LidarrImportOutcome::Late
    );

    until(|| !f.claims.heart_busy(ALBUM_ID)).await;
    assert_eq!(f.taken(), Vec::<String>::new());
    assert!(f.lidarr.deleted().is_empty());
}

#[tokio::test]
async fn a_late_import_of_part_of_the_album_keeps_it_listed_for_the_rest() {
    let f = Fixture::new().await;
    f.lidarr.on_search(&[(1, "Angel", "FLAC")]);
    f.lidarr.hold_imports();
    let service = f.service(None, 1);
    assert!(
        !within(service.try_acquire_album(
            "soulseek",
            "mezzanine",
            true,
            Some("alice"),
            AcquisitionOrigin::AlbumHeart,
            &[],
        ))
        .await
    );
    f.job_wound_up(true).await;
    f.lidarr.release_imports();

    assert_eq!(
        service.lidarr_imported(imported(&[ANGEL])).await,
        LidarrImportOutcome::Late
    );
    until(|| f.taken.lock().len() == 1).await;
    until(|| !f.claims.heart_busy(ALBUM_ID)).await;
    // Teardrop has not come in yet, so the album stays listed.
    assert!(f.pending.get(ALBUM_ID).is_some());

    f.lidarr.import(3, "Teardrop", "FLAC");
    assert_eq!(
        service.lidarr_imported(imported(&[ANGEL, TEARDROP])).await,
        LidarrImportOutcome::Late
    );
    until(|| f.taken.lock().len() == 2).await;
    until(|| !f.claims.heart_busy(ALBUM_ID)).await;
    assert_eq!(f.taken(), ["Angel", "Teardrop"]);
    assert!(f.pending.get(ALBUM_ID).is_none());
}

#[tokio::test]
async fn a_late_import_while_the_album_is_busy_is_taken_once_it_is_free() {
    let f = Fixture::new().await;
    f.lidarr
        .on_search(&[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);
    f.lidarr.hold_imports();
    let service = f.service(None, 1);
    service.set_claim_poll(Duration::from_millis(20));
    assert!(
        !within(service.try_acquire_album(
            "soulseek",
            "mezzanine",
            true,
            Some("alice"),
            AcquisitionOrigin::AlbumHeart,
            &[],
        ))
        .await
    );
    f.job_wound_up(true).await;
    f.lidarr.release_imports();

    f.claims.upgrade_started(ALBUM_ID);
    service.lidarr_imported(imported(&[ANGEL, TEARDROP])).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(f.taken().is_empty(), "taken while an upgrade held the album");

    f.claims.upgrade_ended(ALBUM_ID);
    until(|| f.taken.lock().len() == 2).await;
    assert_eq!(f.taken(), ["Angel", "Teardrop"]);
}

#[tokio::test]
async fn an_album_octo_is_not_waiting_for_is_left_alone() {
    let f = Fixture::new().await;
    let service = f.service(None, 30);

    // Never asked for.
    assert_eq!(
        service.lidarr_imported(imported(&[ANGEL])).await,
        LidarrImportOutcome::NotOurs
    );

    // Asked for, and every track came in: nothing is left to come late.
    assert!(f.heart_the_album(None).await);
    f.job_wound_up(true).await;
    assert!(f.pending.get(ALBUM_ID).is_none());
    assert_eq!(
        service.lidarr_imported(imported(&[ANGEL])).await,
        LidarrImportOutcome::NotOurs
    );
    assert_eq!(f.taken(), ["Angel", "Teardrop"]);
}
