//! A heart through Lidarr keeps the promises a Soulseek heart does. What Lidarr imports of Octo's
//! own downloads goes through the download pipeline song by song, so it meets the same checks;
//! a song already in the
//! library is taken back out (an owned MP3 is queued for Better quality instead), a song removed
//! with a library action stays removed, and the album's monitoring is never turned on. The heart
//! learns whether its songs landed, so the next source can try what Lidarr could not get. Lidarr
//! is used only for an artist with no music in the user's Lidarr, and a pick is added, waited
//! for and pinned before its search, or put back.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeDelta, Utc};
use futures::FutureExt;
use octo_core::common::Clock;
use octo_core::fingerprint::heart_gate::AlbumContext;
use octo_core::models::domain::{Album, Song};
use octo_core::models::download::DownloadInfo;
use octo_core::settings::{
    AppSettings, DownloadSource, LibraryAction, LibraryActionDefinition, LibraryActionSettings,
    LidarrSettings, SettingsStore, SubsonicSettings,
};
use octo_core::soulseek::{RoutingKind, SoulseekRouting};
use parking_lot::Mutex;
use serde_json::Value;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer};

use super::*;
use crate::services::common::test_fakes::{FakeMetadata, until};
use crate::services::fingerprint::music_brainz_client::tests::{
    Recorded, RecordedCatalog, quick, recorded_folder,
};
use crate::services::i_download_service::{AudioStream, DirectStreamInfo, IDownloadService};
use crate::services::library::library_ownership::Candidate;
use crate::services::library::{
    LibraryActionEntry, LibraryActionJournal, LibraryActionState, ReplacementHandoff,
};
use crate::services::lidarr::HeldPlacement;
use crate::services::lidarr::fake_lidarr::{
    ALBUM_ID, ARTIST_ID, CatalogAlbum, CommandMode, FakeLidarr, Grab,
};
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::metadata::{DeezerRateLimitHandler, DeezerRateLimiter};

/// One file the pipeline took: its id, its song's title, and whether it landed quietly.
type Taken = Arc<Mutex<Vec<(String, String, bool)>>>;

/// What the pipeline was asked to judge a file as: the song's title, the offered file's name
/// without its extension, why the acquisition ran, whose gates judged it, and where the file's
/// labels are read from.
#[derive(Debug, Clone, PartialEq)]
struct Asked {
    title: String,
    file: String,
    origin: Option<AcquisitionOrigin>,
    users: Vec<String>,
    labels: Option<LabelSource>,
}

/// The pipeline as far as Lidarr's files go: take the offered file, or refuse it the way
/// AcoustID refuses a wrong recording, which leaves it where Lidarr put it (or where Octo holds it).
/// A file a hearted song's gates judge is accepted for each of them, refused when its title is in
/// `refused_for_hearts`, and waits in the one hold path when its title is in `holding`. A file a
/// follow held back is placed at its target, and recorded; a kept copy dropped for a proven one is
/// deleted, and recorded.
struct Pipeline {
    root: std::path::PathBuf,
    ids: Arc<ExternalIdRegistry>,
    imports: Arc<LidarrImportHandoff>,
    refused: Arc<Mutex<HashSet<String>>>,
    refused_for_hearts: Arc<Mutex<HashSet<String>>>,
    holding: Arc<Mutex<HashSet<String>>>,
    /// People whose own gate refuses every file while another's passes it.
    refused_users: Arc<Mutex<HashSet<String>>>,
    taken: Taken,
    asked: Arc<Mutex<Vec<Asked>>>,
    placed: Arc<Mutex<Vec<HeldPlacement>>>,
    dropped: Arc<Mutex<Vec<(HeldPlacement, Vec<String>)>>>,
}

impl Pipeline {
    fn title_of(&self, id: &str) -> String {
        self.ids
            .lookup(id)
            .and_then(|r| r.snapshot().title)
            .unwrap_or_else(|| id.to_string())
    }

    /// The offered file's name without its extension, and where its labels are read from, the
    /// offer left in place.
    fn offered(&self, id: &str) -> (String, Option<LabelSource>) {
        let Some(import) = self.imports.take(id) else {
            return (String::new(), None);
        };
        let file = Path::new(&import.path)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let labels = import.labels.clone();
        self.imports.offer_import(id, import);
        (file, Some(labels))
    }

    /// Takes the offered file into the library, or refuses it as AcoustID refuses a wrong
    /// recording, leaving it where it was.
    fn take_offered(&self, id: &str) -> anyhow::Result<String> {
        let title = self.title_of(id);
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
        let title = self.title_of(id);
        let (file, labels) = self.offered(id);
        self.asked.lock().push(Asked {
            title,
            file,
            origin: None,
            users: Vec::new(),
            labels,
        });
        self.take_offered(id)
    }

    async fn execute_acquisition_to(
        &self,
        provider: &str,
        id: &str,
        trigger_album_download: bool,
        force_permanent: bool,
        source_override: Option<DownloadSource>,
        token: &CancellationToken,
        requested_by: Option<Vec<String>>,
        upgrade_search: bool,
        replacement: Option<Arc<ReplacementHandoff>>,
        place: crate::services::common::AcquisitionPlace,
    ) -> anyhow::Result<String> {
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
        let _ = (token, requested_by);
        let title = self.title_of(id);
        let (file, labels) = self.offered(id);
        self.asked.lock().push(Asked {
            title: title.clone(),
            file: file.clone(),
            origin: place.origin,
            users: place.gates.iter().map(|gate| gate.user.clone()).collect(),
            labels,
        });
        // The file judged is the one offered: a take of the song is refused by its own name.
        let named = |set: &HashSet<String>| set.iter().any(|title| file.ends_with(&format!(" - {title}")));
        if !place.gates.is_empty() {
            if named(&self.refused_for_hearts.lock()) {
                for gate in &place.gates {
                    place.ledger.record(
                        &gate.user,
                        gate.origin,
                        crate::services::common::GateOutcome::NotAccepted("not the version heard".into()),
                    );
                }
                anyhow::bail!("Lidarr's file for '{title}' is not kept: it does not sound like what played");
            }
            if named(&self.holding.lock()) {
                let import = self
                    .imports
                    .take(id)
                    .ok_or_else(|| anyhow::anyhow!("nothing offered for {id}"))?;
                let folder = self.root.join(".octo-incoming/held/Track_soulseek_held");
                std::fs::create_dir_all(&folder)?;
                let held = folder.join(format!("{title}.flac"));
                std::fs::rename(&import.path, &held)?;
                return Err(crate::services::common::AcquisitionHeld {
                    waiting_for: octo_core::fingerprint::heart_gate::WaitingFor::AcoustId,
                    line: "Waiting for AcoustID".into(),
                    files: vec![held.to_string_lossy().into_owned()],
                    users: place.gates.iter().map(|gate| gate.user.clone()).collect(),
                }
                .into());
            }
        }
        let landed = self.take_offered(id)?;
        for gate in &place.gates {
            if self.refused_users.lock().contains(&gate.user) {
                place.ledger.record(
                    &gate.user,
                    gate.origin,
                    crate::services::common::GateOutcome::NotAccepted("not the version heard".into()),
                );
            } else {
                place.ledger.record_accepted(
                    &gate.user,
                    gate.origin,
                    octo_core::fingerprint::heart_gate::Acceptance::Sound,
                );
            }
        }
        Ok(landed)
    }

    async fn drop_held(&self, held: &HeldPlacement, users: &[String]) -> bool {
        let _ = std::fs::remove_file(&held.held_path);
        self.dropped.lock().push((held.clone(), users.to_vec()));
        true
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

    async fn place_held(&self, held: &HeldPlacement) -> Option<String> {
        if let Some(dir) = Path::new(&held.target).parent() {
            std::fs::create_dir_all(dir).ok()?;
        }
        std::fs::rename(&held.held_path, &held.target).ok()?;
        self.placed.lock().push(held.clone());
        Some(held.target.clone())
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
    /// Titles whose file a hearted song's gates refuse.
    refused_for_hearts: Arc<Mutex<HashSet<String>>>,
    /// Titles whose file a hearted song's gates hold for AcoustID.
    holding: Arc<Mutex<HashSet<String>>>,
    /// People whose own gate refuses every file while another's passes it.
    refused_users: Arc<Mutex<HashSet<String>>>,
    taken: Taken,
    /// Every file the pipeline was asked to judge, in order.
    asked: Arc<Mutex<Vec<Asked>>>,
    owned: Arc<Mutex<Vec<Candidate>>>,
    pending: Arc<LidarrPendingAlbums>,
    metadata: Arc<FakeMetadata>,
    tracker: Arc<AcquisitionTracker>,
    /// The held files the pipeline placed.
    placed: Arc<Mutex<Vec<HeldPlacement>>>,
    /// The kept copies the pipeline dropped for a proven one, and for whom.
    dropped: Arc<Mutex<Vec<(HeldPlacement, Vec<String>)>>>,
}

/// How a test's service is built beyond the defaults.
#[derive(Default)]
struct Options {
    actions: Option<LibraryActionSettings>,
    timeout_seconds: i32,
    music_brainz: Option<Arc<MusicBrainzClient>>,
    catalog: Option<Arc<dyn CatalogQuestions>>,
    /// Where Octo's library is, when it is not where Lidarr's root folder lies: Octo then sees
    /// none of the files lying in Lidarr's folders.
    octo_root: Option<PathBuf>,
    rounds: Option<PauseRounds>,
    album_gates: Option<Arc<AlbumGates>>,
    gates: Option<Arc<dyn HeartGates>>,
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
            refused_for_hearts: Arc::default(),
            holding: Arc::default(),
            refused_users: Arc::default(),
            taken: Arc::default(),
            asked: Arc::default(),
            owned: Arc::default(),
            pending: Arc::new(LidarrPendingAlbums::new(None)),
            metadata: Arc::new(FakeMetadata::default()),
            tracker: Arc::new(AcquisitionTracker::new(None, Clock::system())),
            placed: Arc::default(),
            dropped: Arc::default(),
        }
    }

    /// Lidarr's next search of Mezzanine grabs one download of these tracks, finished at once and
    /// imported by the refresh the grab queues.
    fn grabs_at_once(&self, download_id: &str, tracks: &[(i32, &str, &str)]) {
        self.lidarr.plan_grab(7, Grab::of(download_id, tracks).finished());
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
        self.service_with(Options {
            actions,
            timeout_seconds,
            ..Default::default()
        })
    }

    fn service_with(&self, options: Options) -> LidarrHeartAcquisitionService {
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            lidarr: LidarrSettings {
                base_url: Some(self.server.uri()),
                api_key: Some("k".into()),
                root_folder_path: Some("/data/music".into()),
                quality_profile_id: 1,
                metadata_profile_id: 1,
                import_timeout_seconds: options.timeout_seconds,
                ..Default::default()
            },
            subsonic: SubsonicSettings {
                url: Some("http://navidrome.test".into()),
                auto_detect_download_path: false,
                skip_owned_songs: true,
                ..Default::default()
            },
            library_actions: options.actions.unwrap_or_default(),
            ..Default::default()
        }));
        let octo_root = options
            .octo_root
            .unwrap_or_else(|| self.root.path().to_path_buf());
        settings.set_raw("Library:DownloadPath", Some(&octo_root.to_string_lossy()));
        let identity = NavidromeIdentityService::new(settings.clone(), reqwest::Client::new());

        let metadata = self.metadata.clone();
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
            refused_for_hearts: self.refused_for_hearts.clone(),
            holding: self.holding.clone(),
            refused_users: self.refused_users.clone(),
            taken: self.taken.clone(),
            asked: self.asked.clone(),
            placed: self.placed.clone(),
            dropped: self.dropped.clone(),
        });
        let client = Arc::new(LidarrClient::new(settings.clone()));
        client.set_metadata_poll(Duration::from_millis(20));
        let service = LidarrHeartAcquisitionService::new(
            client,
            metadata,
            deezer,
            settings.clone(),
            identity,
            Arc::new(NotificationService::new(Vec::new(), settings, None)),
            LidarrHeartExtras {
                tracker: Some(self.tracker.clone()),
                music_brainz: options.music_brainz,
                claims: Some(self.claims.clone()),
                imports: Some(self.imports.clone()),
                ids: Some(self.ids.clone()),
                downloads: Some(downloads),
                journal: Some(self.journal.clone()),
                ownership: Some(ownership),
                upgrades: Some(self.upgrades.clone()),
                pending: Some(self.pending.clone()),
                catalog: options.catalog,
                rounds: options.rounds,
                album_gates: options.album_gates,
                gates: options.gates,
            },
        );
        service.set_poll_override(Some(Duration::from_millis(20)));
        service
    }

    /// A client of the fake Lidarr, set up as the service's is.
    fn client(&self) -> LidarrClient {
        LidarrClient::new(Arc::new(SettingsStore::for_tests(AppSettings {
            lidarr: LidarrSettings {
                base_url: Some(self.server.uri()),
                api_key: Some("k".into()),
                root_folder_path: Some("/data/music".into()),
                quality_profile_id: 1,
                metadata_profile_id: 1,
                ..Default::default()
            },
            ..Default::default()
        })))
    }

    /// A heart of Teardrop with its downloads row begun, as the coordinator begins it; whether
    /// it landed.
    async fn heart_teardrop(&self, service: &LidarrHeartAcquisitionService) -> bool {
        self.tracker.begin(
            "soulseek",
            "teardrop-heart",
            None,
            Some("alice"),
            Some("Massive Attack"),
            Some("Teardrop"),
            Some("Mezzanine"),
        );
        within(service.try_acquire_track(
            "soulseek",
            "teardrop-heart",
            true,
            Some("alice"),
            AcquisitionOrigin::Heart,
            &[],
        ))
        .await
    }

    /// The albums Octo searched, by Lidarr's id, in order.
    fn searched_albums(&self) -> Vec<i64> {
        self.lidarr
            .changes()
            .iter()
            .filter_map(|r| r.body.clone())
            .filter(|body| body["name"] == "AlbumSearch")
            .filter_map(|body| body["albumIds"][0].as_i64())
            .collect()
    }

    /// Every request that could change Lidarr, as `METHOD path?query`.
    fn changes(&self) -> Vec<String> {
        self.lidarr
            .changes()
            .iter()
            .map(|r| format!("{} {}", r.method, r.path))
            .collect()
    }

    /// A real catalog row as a song Octo knows: its routing, the album the person listed it from,
    /// and the song the metadata gives its id; MusicBrainz answers from the row's recorded
    /// answers. Answers the song's id and the MusicBrainz client.
    async fn recorded_row(&self, row: &Row) -> (String, Arc<MusicBrainzClient>, MockServer) {
        let id = self.ids.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some(row.artist.into()),
            title: Some(row.title.into()),
            album: Some(row.album.into()),
            isrc: Some(row.isrc.into()),
            duration: Some(row.seconds),
            disc_number: Some(1),
            track: Some(row.position),
            total_tracks: Some(row.total),
            ..Default::default()
        });
        self.ids.remember_context(
            &id,
            AlbumContext {
                user: "alice".into(),
                client: None,
                catalog_album_id: row.catalog_album.into(),
                album: Some(row.album.into()),
                disc: Some(1),
                track: Some(row.position),
                total: Some(row.total),
                isrc: Some(row.isrc.into()),
                explicit: None,
                listed_at: Utc::now(),
                played_at: None,
            },
        );
        self.metadata.songs.lock().insert(
            ("soulseek".into(), id.clone()),
            Song {
                title: row.title.into(),
                artist: row.artist.into(),
                album: row.album.into(),
                external_provider: Some("soulseek".into()),
                external_id: Some(id.clone()),
                ..Default::default()
            },
        );
        let server = MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(Recorded(recorded_folder(row.folder)))
            .mount(&server)
            .await;
        let music_brainz = Arc::new(quick(&server, Duration::from_secs(5)));
        (id, music_brainz, server)
    }

    async fn heart_the_album(&self, actions: Option<LibraryActionSettings>) -> bool {
        self.grabs_at_once("dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);
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

    /// The heart is answered as soon as its song lands; the follow lets go of the album once
    /// Lidarr is done with Octo's download, so that is waited for. Monitoring was never turned on,
    /// so nothing gives it back.
    async fn job_wound_up(&self) {
        until(|| !self.claims.heart_busy(ALBUM_ID)).await;
        assert!(
            self.lidarr.monitor_changes().is_empty(),
            "Octo never changes monitoring"
        );
    }
}

/// One recorded catalog row, as `rows.tsv` and `positions-v3.json` give it.
struct Row {
    folder: &'static str,
    artist: &'static str,
    title: &'static str,
    album: &'static str,
    catalog_album: &'static str,
    isrc: &'static str,
    seconds: i32,
    position: i32,
    total: i32,
}

const EYE_OF_THE_TIGER: Row = Row {
    folder: "rows/survivor-eye-of-the-tiger_0",
    artist: "Survivor",
    title: "Eye of the Tiger",
    album: "Rocky IV",
    catalog_album: "74479",
    isrc: "USVR19600010",
    seconds: 245,
    position: 4,
    total: 11,
};

/// The walk the recorded answers give Eye of the Tiger heard from Rocky IV: Rocky IV, then three
/// more groups Survivor leads, of the 307 carrying it.
const EYE_OF_THE_TIGER_WALK: [&str; 4] = [
    "072e8ecb-680c-33fa-ad15-556771d849ad",
    "9554c59d-afff-3ea2-b732-19a459ea0d1b",
    "f403d325-8a20-4a5a-9d08-e01f50031332",
    "500c42d2-de09-4870-b364-55bbc77505c9",
];

const AGAINST_ALL_ODDS_AT_LIVE_AID: Row = Row {
    folder: "rows/collins-against-all-odds_2",
    artist: "Phil Collins",
    title: "Against All Odds (Take a Look at Me Now) (Live at Wembley Stadium, 13th July 1985)",
    album: "Phil Collins at Live Aid (Live at Wembley Stadium and John F. Kennedy Stadium, 13th July 1985)",
    catalog_album: "219784842",
    isrc: "UK6821836618",
    seconds: 234,
    position: 1,
    total: 2,
};

/// The Live Aid release group.
const LIVE_AID: &str = "cff2dbad-704e-38b6-afd3-0e6868312e70";

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
    // Octo moved both files into its own layout, and deleted Lidarr's records of them; the
    // files stay where Octo put them.
    let mut deleted = f.lidarr.deleted();
    deleted.sort_unstable();
    assert_eq!(deleted, [100, 101]);
    for title in ["Angel", "Teardrop"] {
        assert!(
            f.root
                .path()
                .join(format!("Massive Attack - {title}.flac"))
                .is_file()
        );
    }
    // An album heart gets one notice for the album, so its songs land quietly.
    assert!(f.taken.lock().iter().all(|t| t.2));
    f.job_wound_up().await;
    assert!(!f.claims.heart_busy(ALBUM_ID));
}

#[tokio::test]
async fn a_song_already_in_the_library_is_taken_back_out() {
    let f = Fixture::new().await;
    *f.owned.lock() = vec![owned("nd-9", "Teardrop", "flac", 1000)];

    assert!(f.heart_the_album(None).await);

    assert_eq!(f.taken(), ["Angel"]);
    // Teardrop deleted through Lidarr, and the record of the Angel Octo moved.
    assert_eq!(f.lidarr.deleted().len(), 2);
    assert!(!f.root.path().join(TEARDROP_FILE).exists());
    assert!(f.upgrades.snapshot().is_empty());
}

/// Where the fake's quick import puts Teardrop, on Octo's side.
const TEARDROP_FILE: &str = "Massive Attack/Mezzanine/03 - Teardrop.flac";

#[tokio::test]
async fn an_owned_mp3_is_queued_for_better_quality_instead() {
    let f = Fixture::new().await;
    *f.owned.lock() = vec![owned("nd-9", "Teardrop", "mp3", 320)];

    assert!(f.heart_the_album(Some(upgrades_on())).await);

    assert_eq!(f.taken(), ["Angel"]);
    assert_eq!(f.lidarr.deleted().len(), 2);
    assert!(!f.root.path().join(TEARDROP_FILE).exists());
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
    // Angel deleted through Lidarr, and the record of the Teardrop Octo moved.
    assert_eq!(f.lidarr.deleted().len(), 2);
    assert!(
        !f.root
            .path()
            .join("Massive Attack/Mezzanine/01 - Angel.flac")
            .exists()
    );
}

#[tokio::test]
async fn a_song_the_pipeline_refuses_is_deleted_and_left_to_the_next_source() {
    let f = Fixture::new().await;
    f.refused.lock().insert("Teardrop".into());

    assert!(!f.heart_the_album(None).await);

    assert_eq!(f.taken(), ["Angel"]);
    // Refused before it was taken, so it is still where Lidarr put it, and goes through Lidarr;
    // so does the record of the Angel Octo moved.
    assert_eq!(f.lidarr.deleted().len(), 2);
    assert!(!f.root.path().join(TEARDROP_FILE).exists());
}

/// A song heart: Lidarr still brings the whole album, the hearted song lands with its own
/// notice, the other song lands quietly, and Angel (track 1 on the album) is never taken
/// for Teardrop, which is track 1 on the single Deezer found it on.
#[tokio::test]
async fn a_song_heart_lands_its_song_with_a_notice_and_the_rest_quietly() {
    let f = Fixture::new().await;
    f.grabs_at_once("dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);

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

/// A search that grabs nothing of Octo's ends the pick at once, put back: the album and the
/// artist Octo's add created leave Lidarr, and the entry is forgotten.
#[tokio::test]
async fn a_search_that_grabs_nothing_is_a_miss_put_back_for_the_next_source() {
    let f = Fixture::new().await;

    let service = f.service(None, 30);
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
    f.job_wound_up().await;
    assert!(!f.claims.heart_busy(ALBUM_ID));
    assert!(f.pending.get(ALBUM_ID).is_none());
    let deletes: Vec<String> = f
        .changes()
        .into_iter()
        .filter(|c| c.starts_with("DELETE"))
        .collect();
    assert_eq!(
        deletes,
        [
            "DELETE /api/v1/album/7?deleteFiles=false&addImportListExclusion=false",
            "DELETE /api/v1/artist/5?deleteFiles=false&addImportListExclusion=false",
        ]
    );
}

#[tokio::test]
async fn a_second_heart_on_the_same_album_joins_and_is_answered_too() {
    let f = Fixture::new().await;
    f.lidarr.plan_grab(
        7,
        Grab::of("dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]),
    );
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
    // Bob has joined the running job before Lidarr brings anything, and his heart is on the
    // album's saved entry beside Alice's.
    until(|| service.askers_of(ALBUM_ID).is_some_and(|a| a.len() == 2)).await;
    until(|| {
        f.pending
            .get(ALBUM_ID)
            .is_some_and(|entry| entry.holders == ["alice", "bob"])
    })
    .await;
    // The release: Octo's download finishes, and Lidarr imports both songs.
    f.lidarr.complete("dl-1");
    f.lidarr.refresh();

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
    f.grabs_at_once("dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);
    f.claims.upgrade_started(ALBUM_ID);
    let service = f.service(None, 30);

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

/// Lidarr's webhook for an import of Octo's download `dl-1`, delivered by its client.
fn imported(paths: &[&str]) -> LidarrImportEvent {
    LidarrImportEvent {
        foreign_album_id: ALBUM_ID.into(),
        artist: Some("Massive Attack".into()),
        album: Some("Mezzanine".into()),
        paths: paths.iter().map(|p| p.to_string()).collect(),
        is_upgrade: false,
        download_id: Some("dl-1".into()),
        download_client: Some("qBittorrent".into()),
    }
}

const ANGEL: &str = "/data/music/Massive Attack/Mezzanine/01 - Angel.flac";
const TEARDROP: &str = "/data/music/Massive Attack/Mezzanine/03 - Teardrop.flac";

#[tokio::test]
async fn lidarrs_webhook_wakes_the_waiting_heart_at_once() {
    let f = Fixture::new().await;
    f.lidarr.plan_grab(
        7,
        Grab::of("dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]),
    );
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
    // Let the follow take its first look, find nothing imported, and settle into its wait.
    tokio::time::sleep(Duration::from_millis(200)).await;
    f.lidarr.complete("dl-1");
    f.lidarr.refresh();

    assert_eq!(
        service.lidarr_imported(imported(&[ANGEL, TEARDROP])).await,
        LidarrImportOutcome::Woke
    );
    assert!(heart.await.expect("the task"));
    assert_eq!(f.taken(), ["Angel", "Teardrop"]);
}

/// The album heart gave up at its timeout while Octo's download still moved, and stopped waiting
/// for it.
async fn heart_gave_up_on(
    f: &Fixture,
    download_id: &str,
    tracks: &[(i32, &str, &str)],
) -> LidarrHeartAcquisitionService {
    f.lidarr.plan_grab(7, Grab::of(download_id, tracks));
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
    assert!(f.pending.get(ALBUM_ID).is_some_and(|entry| entry.following()));
    service
}

/// Octo stops, as a restart stops it, while its download still moves.
async fn stopped(f: &Fixture, service: &LidarrHeartAcquisitionService) {
    until(|| {
        f.pending
            .get(ALBUM_ID)
            .is_some_and(|entry| entry.owns_grab("dl-1"))
    })
    .await;
    service.stop_following();
    until(|| !f.pending.is_running(ALBUM_ID)).await;
}

#[tokio::test]
async fn an_import_after_the_heart_stopped_waiting_is_still_taken() {
    let f = Fixture::new().await;
    let service = heart_gave_up_on(&f, "dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]).await;
    stopped(&f, &service).await;

    // Lidarr finishes later, after a restart for all Octo knows: a new service over the same
    // pending list.
    f.lidarr.complete("dl-1");
    f.lidarr.refresh();
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

/// Which files are Octo's is read from Lidarr's own rows, never from the webhook's list: a
/// webhook naming one file still has the follow take every file of Octo's download.
#[tokio::test]
async fn a_late_import_is_judged_by_its_rows_whatever_files_the_webhook_names() {
    let f = Fixture::new().await;
    let service = heart_gave_up_on(&f, "dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]).await;
    stopped(&f, &service).await;
    f.lidarr.complete("dl-1");
    f.lidarr.refresh();

    let restarted = f.service(None, 1);
    assert_eq!(
        restarted.lidarr_imported(imported(&[ANGEL])).await,
        LidarrImportOutcome::Late
    );

    until(|| !f.claims.heart_busy(ALBUM_ID) && f.pending.get(ALBUM_ID).is_none()).await;
    assert_eq!(f.taken(), ["Angel", "Teardrop"]);
}

/// Angel in the library, as long as Lidarr says it runs.
fn owned_angel(suffix: &str, bit_rate: i32) -> Candidate {
    Candidate {
        duration: Some(380),
        ..owned("nd-1", "Angel", suffix, bit_rate)
    }
}

/// Teardrop is what the heart waited for, until it gave up; the owner's own Angel, an MP3 the
/// owner has in the library, came into Lidarr afterwards with no import row (a rescan linked
/// it) while Octo's download still moved. (A file the artist had before the search can no longer
/// be: Lidarr is used only for an artist with no files.)
async fn heart_gave_up_then_the_owner_added_angel(f: &Fixture) -> LidarrHeartAcquisitionService {
    let service = heart_gave_up_on(f, "dl-1", &[(3, "Teardrop", "FLAC")]).await;
    until(|| {
        f.pending
            .get(ALBUM_ID)
            .is_some_and(|entry| entry.owns_grab("dl-1"))
    })
    .await;
    f.lidarr.import(1, "Angel", "MP3-320");
    *f.owned.lock() = vec![owned_angel("mp3", 320)];
    service
}

/// The owner's own file arriving during a follow, before any file of Octo's: Octo steps aside
/// (its download removed, never imported over the owner's file), puts the pick back keeping the
/// album that now holds the owner's file, and the owner's later upgrade is left alone.
#[tokio::test]
async fn the_owners_own_file_arriving_during_a_follow_is_left_alone_and_octo_steps_aside() {
    let f = Fixture::new().await;
    let service = heart_gave_up_then_the_owner_added_angel(&f).await;
    until(|| f.pending.get(ALBUM_ID).is_none() && !f.claims.heart_busy(ALBUM_ID)).await;
    assert!(
        f.changes()
            .iter()
            .any(|c| c.starts_with("DELETE /api/v1/queue/1?")),
        "Octo's own download is removed"
    );
    assert!(
        !f.changes().iter().any(|c| c.starts_with("DELETE /api/v1/album")),
        "the album holding the owner's file stays"
    );

    // Lidarr upgrades the owner's Angel to FLAC on its own: a new file id at a new path.
    let angel = f.lidarr.upgrade(1, "Angel", "FLAC");
    let mut event = imported(&[ANGEL]);
    event.is_upgrade = true;
    event.download_id = Some("owners-own".into());
    assert_eq!(service.lidarr_imported(event).await, LidarrImportOutcome::NotOurs);

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(f.lidarr.deleted().is_empty());
    assert!(Path::new(&angel).is_file());
    assert_eq!(f.taken(), Vec::<String>::new());
}

/// A late import of Octo's own download is Octo's by its rows: one the pipeline refuses, and one
/// of a song already in the library, are deleted through Lidarr, never left in its library.
#[tokio::test]
async fn a_late_import_octo_does_not_take_is_deleted_through_lidarr() {
    let f = Fixture::new().await;
    let _service = heart_gave_up_on(&f, "dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]).await;
    // Teardrop is refused by the pipeline, Angel is already in the library.
    f.refused.lock().insert("Teardrop".into());
    *f.owned.lock() = vec![owned_angel("flac", 1000)];
    f.lidarr.complete("dl-1");
    f.lidarr.refresh();

    until(|| !f.claims.heart_busy(ALBUM_ID)).await;
    assert_eq!(f.taken(), Vec::<String>::new());
    let mut deleted = f.lidarr.deleted();
    deleted.sort_unstable();
    assert_eq!(deleted, [100, 101]);
    assert!(!f.root.path().join(TEARDROP_FILE).exists());
    assert!(
        !f.root
            .path()
            .join("Massive Attack/Mezzanine/01 - Angel.flac")
            .exists()
    );
}

#[tokio::test]
async fn a_late_import_while_the_album_is_busy_is_taken_once_it_is_free() {
    let f = Fixture::new().await;
    let service = heart_gave_up_on(&f, "dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]).await;
    stopped(&f, &service).await;
    f.lidarr.complete("dl-1");
    f.lidarr.refresh();

    let restarted = f.service(None, 1);
    f.claims.upgrade_started(ALBUM_ID);
    assert_eq!(
        restarted.lidarr_imported(imported(&[ANGEL, TEARDROP])).await,
        LidarrImportOutcome::Late
    );
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
    f.job_wound_up().await;
    assert!(f.pending.get(ALBUM_ID).is_none());
    assert_eq!(
        service.lidarr_imported(imported(&[ANGEL])).await,
        LidarrImportOutcome::NotOurs
    );
    assert_eq!(f.taken(), ["Angel", "Teardrop"]);
}

// ---- When Lidarr is used, the add, the rescan waits and the pin -----------------------------

/// Another album of Massive Attack's, with one track.
fn protection() -> CatalogAlbum {
    CatalogAlbum::new(
        "rg-protection",
        "Protection",
        ARTIST_ID,
        9,
        &[(40, "Karmacoma", 1)],
    )
}

/// Where Teardrop lies in the artist's folder, in Lidarr's paths.
const TEARDROP_LYING: &str = "/data/music/Massive Attack/Mezzanine/03 - Teardrop.flac";

/// The heart's row says Lidarr was left alone, and the heart goes on to its next source.
fn left_alone_with_its_line(f: &Fixture, service: &LidarrHeartAcquisitionService) {
    assert!(service.left_alone("soulseek", "teardrop-heart"));
    assert_eq!(
        f.tracker.note_of("soulseek", "teardrop-heart").as_deref(),
        Some(LIDARR_LEFT_ALONE)
    );
}

/// One track file on another album is enough: the artist is never searched, nothing in Lidarr
/// changes, and the heart's next source (Soulseek) tries.
#[tokio::test]
async fn an_artist_with_one_track_file_on_another_album_is_never_searched_and_lidarr_is_untouched() {
    let f = Fixture::new().await;
    f.lidarr.catalog_album(protection());
    f.lidarr.user_album("rg-protection", false);
    f.lidarr.import(40, "Karmacoma", "FLAC");
    f.lidarr.on_search(&[(3, "Teardrop", "FLAC")]);
    let service = f.service(None, 30);

    assert!(!f.heart_teardrop(&service).await);

    assert_eq!(f.lidarr.searches(), 0);
    assert_eq!(f.changes(), Vec::<String>::new());
    left_alone_with_its_line(&f, &service);
}

/// The artist's statistic says no files while the track file list holds one: the list decides.
#[tokio::test]
async fn a_stale_file_count_of_zero_with_a_file_listed_is_left_alone() {
    let f = Fixture::new().await;
    f.lidarr.user_album(ALBUM_ID, false);
    f.lidarr.import(1, "Angel", "FLAC");
    f.lidarr.set_stale_statistics(5, 0);
    let service = f.service(None, 30);

    assert!(!f.heart_teardrop(&service).await);

    assert_eq!(f.changes(), Vec::<String>::new());
    assert!(
        f.lidarr
            .requests()
            .iter()
            .any(|r| r.path == "/api/v1/trackfile?artistId=5")
    );
    left_alone_with_its_line(&f, &service);
}

/// An artist Lidarr does not have yet, whose folder Octo can see and that holds a file: nothing
/// is added and nothing in Lidarr changes, so no rescan rewrites the file's tags.
#[tokio::test]
async fn an_artist_folder_holding_files_gets_no_add_and_no_changing_call() {
    let f = Fixture::new().await;
    f.lidarr.set_write_audio_tags("allFiles");
    let lying = f.lidarr.place_file(TEARDROP_LYING, b"the user's own");
    let service = f.service(None, 30);

    assert!(!f.heart_teardrop(&service).await);

    assert_eq!(f.changes(), Vec::<String>::new());
    assert_eq!(std::fs::read(&lying).expect("still there"), b"the user's own");
    assert!(
        f.lidarr
            .requests()
            .iter()
            .any(|r| r.path.starts_with("/api/v1/artist/lookup?term=lidarr"))
    );
    left_alone_with_its_line(&f, &service);
}

/// Octo's own earlier downloads, in a library shared with Lidarr, that Lidarr's daily scan
/// linked: they are the person's library now, and the artist is left alone.
#[tokio::test]
async fn a_shared_library_artist_whose_files_a_rescan_linked_is_left_alone() {
    let f = Fixture::new().await;
    f.lidarr.user_album(ALBUM_ID, false);
    f.lidarr.place_file(TEARDROP_LYING, b"Octo's earlier download");
    f.lidarr.queue_command(
        "RescanFolders",
        serde_json::json!({ "folders": null }),
        "scheduled",
    );
    let service = f.service(None, 30);

    assert!(!f.heart_teardrop(&service).await);

    assert_eq!(f.lidarr.searches(), 0);
    assert_eq!(f.changes(), Vec::<String>::new());
    left_alone_with_its_line(&f, &service);
}

/// An artist Lidarr does not have, whose folder holds files Octo cannot see (its library is
/// elsewhere): the add goes out, its rescan links the files, and the pick is put back with
/// nothing searched; Octo's album and artist are deleted from Lidarr and the files stay.
#[tokio::test]
async fn an_artist_whose_files_octo_cannot_see_is_added_rescanned_and_put_back_unsearched() {
    let f = Fixture::new().await;
    let lying = f.lidarr.place_file(TEARDROP_LYING, b"the user's own");
    f.lidarr.on_search(&[(3, "Teardrop", "FLAC")]);
    let elsewhere = tempfile::tempdir().expect("a temp dir");
    let service = f.service_with(Options {
        timeout_seconds: 30,
        octo_root: Some(elsewhere.path().to_path_buf()),
        ..Default::default()
    });

    assert!(!f.heart_teardrop(&service).await);

    assert_eq!(f.lidarr.searches(), 0);
    assert_eq!(
        f.changes(),
        [
            "POST /api/v1/album",
            "DELETE /api/v1/album/7?deleteFiles=false&addImportListExclusion=false",
            "DELETE /api/v1/artist/5?deleteFiles=false&addImportListExclusion=false",
        ]
    );
    assert!(lying.is_file());
    assert!(f.pending.get(ALBUM_ID).is_none());
    left_alone_with_its_line(&f, &service);
}

/// The same with an artist Lidarr has: the file the add's rescan adopted makes the pick skip and
/// survives; only the album Octo's add created goes, and the user's artist stays.
#[tokio::test]
async fn a_file_the_adds_rescan_adopts_makes_the_pick_skip_and_survives() {
    let f = Fixture::new().await;
    f.lidarr.user_artist(ARTIST_ID, false);
    let lying = f.lidarr.place_file(TEARDROP_LYING, b"the user's own");
    f.lidarr.on_search(&[(3, "Teardrop", "FLAC")]);
    let elsewhere = tempfile::tempdir().expect("a temp dir");
    let service = f.service_with(Options {
        timeout_seconds: 30,
        octo_root: Some(elsewhere.path().to_path_buf()),
        ..Default::default()
    });

    assert!(!f.heart_teardrop(&service).await);

    assert_eq!(f.lidarr.searches(), 0);
    assert_eq!(
        f.changes(),
        [
            "POST /api/v1/album",
            "DELETE /api/v1/album/7?deleteFiles=false&addImportListExclusion=false",
        ]
    );
    assert_eq!(std::fs::read(&lying).expect("still there"), b"the user's own");
    left_alone_with_its_line(&f, &service);
}

/// The tracks appear while the add's RefreshAlbum still runs, and the RescanFolders it queues
/// comes a poll later: Octo waits for both, reads the artist again, finds what the rescan linked,
/// and searches nothing.
#[tokio::test]
async fn tracks_visible_while_the_refresh_runs_wait_for_the_rescan_queued_after_it() {
    let f = Fixture::new().await;
    f.lidarr.user_artist(ARTIST_ID, false);
    f.lidarr.place_file(TEARDROP_LYING, b"the user's own");
    f.lidarr.on_search(&[(3, "Teardrop", "FLAC")]);
    f.lidarr.set_command_mode(CommandMode::PerPoll);
    let elsewhere = tempfile::tempdir().expect("a temp dir");
    let service = f.service_with(Options {
        timeout_seconds: 30,
        octo_root: Some(elsewhere.path().to_path_buf()),
        ..Default::default()
    });

    assert!(!f.heart_teardrop(&service).await);

    assert_eq!(f.lidarr.searches(), 0);
    assert!(f.searched_albums().is_empty());
    left_alone_with_its_line(&f, &service);
}

/// A track list that never appears: the pick is put back and nothing is searched. The entry was
/// saved before the add went out, and is forgotten once the pick is put back.
#[tokio::test]
async fn a_track_list_that_never_appears_is_put_back_unsearched() {
    let f = Fixture::new().await;
    let mut empty = CatalogAlbum::new("rg-empty", "Empty", ARTIST_ID, 8, &[(20, "Gone", 1)]);
    empty.tracks_appear = false;
    empty.in_profile = false;
    f.lidarr.catalog_album(empty);
    f.metadata.songs.lock().insert(
        ("soulseek".into(), "gone-heart".into()),
        Song {
            title: "Gone".into(),
            artist: "Massive Attack".into(),
            album: "Empty".into(),
            external_provider: Some("soulseek".into()),
            external_id: Some(f.id("Gone")),
            ..Default::default()
        },
    );
    let service = f.service(None, 1);

    let heart = {
        let service = service.clone();
        tokio::spawn(async move {
            within(service.try_acquire_track(
                "soulseek",
                "gone-heart",
                false,
                Some("alice"),
                AcquisitionOrigin::Heart,
                &[],
            ))
            .await
        })
    };
    // While the pick waits for the track list, its entry holds the add and what it created.
    until(|| {
        f.pending
            .get("rg-empty")
            .is_some_and(|entry| entry.album_added && entry.add.is_some() && entry.artist_added == 5)
    })
    .await;
    assert!(!heart.await.expect("the task"));

    assert_eq!(f.lidarr.searches(), 0);
    assert_eq!(
        f.changes()
            .into_iter()
            .filter(|c| c.starts_with("DELETE"))
            .collect::<Vec<_>>(),
        [
            "DELETE /api/v1/album/8?deleteFiles=false&addImportListExclusion=false",
            "DELETE /api/v1/artist/5?deleteFiles=false&addImportListExclusion=false",
        ]
    );
    assert!(f.pending.get("rg-empty").is_none());
}

/// An album of an artist with no files that the user's Lidarr created, unmonitored: it is
/// searched as it is, stays unmonitored, and the artist's other album is untouched.
#[tokio::test]
async fn an_unmonitored_album_the_users_lidarr_created_is_searched_and_stays_unmonitored() {
    let f = Fixture::new().await;
    f.lidarr.catalog_album(protection());
    f.lidarr.user_album(ALBUM_ID, false);
    f.lidarr.user_album("rg-protection", false);
    f.grabs_at_once("dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);
    let service = f.service(None, 30);

    assert!(f.heart_teardrop(&service).await);

    assert_eq!(f.searched_albums(), [7]);
    assert!(f.lidarr.monitor_changes().is_empty());
    let changes = f.changes();
    assert!(
        changes
            .iter()
            .all(|c| !c.starts_with("PUT") && !c.starts_with("POST /api/v1/album")),
        "{changes:?}"
    );
}

/// A monitored album is the user's: Lidarr already looks for it. It is never searched or pinned,
/// and the heart's row says why.
#[tokio::test]
async fn a_monitored_album_is_skipped_with_its_line_and_never_pinned() {
    let f = Fixture::new().await;
    f.lidarr.user_album(ALBUM_ID, true);
    let service = f.service(None, 30);

    assert!(!f.heart_teardrop(&service).await);

    assert_eq!(f.changes(), Vec::<String>::new());
    assert_eq!(
        f.tracker.note_of("soulseek", "teardrop-heart").as_deref(),
        Some(ALREADY_LOOKING)
    );
    assert!(!service.left_alone("soulseek", "teardrop-heart"));
}

/// Octo stopped right after the add went out, with nothing saved of what it created: after the
/// restart the album (absent at the read before the add) and the artist (added since the add was
/// sent, and not among the artists read before it) are Octo's, and both are put back.
#[tokio::test]
async fn a_crash_right_after_the_add_finds_what_it_created_and_puts_it_back() {
    let f = Fixture::new().await;
    let service = f.service(None, 30);
    f.pending.remember(PendingAlbum {
        foreign_album_id: ALBUM_ID.into(),
        artist: "Massive Attack".into(),
        title: "Mezzanine".into(),
        add: Some(PendingAdd {
            foreign_artist_id: ARTIST_ID.into(),
            posted_at: Utc::now() - TimeDelta::seconds(1),
            artists_before: Vec::new(),
        }),
        ..Default::default()
    });
    // The add went out and Lidarr created the album and its artist.
    let client = f.client();
    let candidate = client
        .resolve_album_by_foreign_id(ALBUM_ID)
        .await
        .expect("looked up")
        .expect("known");
    client.add_album(&candidate, None).await.expect("added");

    service.put_back_after_restart().await;

    assert_eq!(
        f.changes()
            .into_iter()
            .filter(|c| c.starts_with("DELETE"))
            .collect::<Vec<_>>(),
        [
            "DELETE /api/v1/album/7?deleteFiles=false&addImportListExclusion=false",
            "DELETE /api/v1/artist/5?deleteFiles=false&addImportListExclusion=false",
        ]
    );
    assert!(f.pending.get(ALBUM_ID).is_none());
}

/// The same crash for an album added under the user's own artist: the artist was there before the
/// add, so it stays.
#[tokio::test]
async fn a_crash_right_after_the_add_never_deletes_an_artist_lidarr_had() {
    let f = Fixture::new().await;
    f.lidarr.user_artist(ARTIST_ID, false);
    let service = f.service(None, 30);
    f.pending.remember(PendingAlbum {
        foreign_album_id: ALBUM_ID.into(),
        add: Some(PendingAdd {
            foreign_artist_id: ARTIST_ID.into(),
            posted_at: Utc::now() - TimeDelta::seconds(1),
            artists_before: vec![5],
        }),
        ..Default::default()
    });
    f.lidarr.user_album(ALBUM_ID, false);

    service.put_back_after_restart().await;

    assert_eq!(
        f.changes(),
        ["DELETE /api/v1/album/7?deleteFiles=false&addImportListExclusion=false"]
    );
}

/// Survivor's albums Lidarr's metadata knows, one per group of the walk, each with Eye of the
/// Tiger on it; the first, Rocky IV, also lists `releases`.
fn survivors_albums(f: &Fixture, releases: Option<Vec<(i32, String)>>) {
    f.lidarr.catalog_artist("survivor", "Survivor", 50);
    for (n, group) in EYE_OF_THE_TIGER_WALK.iter().enumerate() {
        let n = n as i32;
        let mut album = CatalogAlbum::new(
            group,
            &format!("Survivor {n}"),
            "survivor",
            60 + n,
            &[(600 + n, "Eye of the Tiger", 4)],
        );
        if n == 0
            && let Some(releases) = &releases
        {
            album.releases = releases.clone();
        }
        f.lidarr.catalog_album(album);
    }
}

/// Eye of the Tiger heard from Rocky IV: its walk asks Lidarr for at most four groups, all
/// Survivor's own (never a Various Artists compilation of the 307 carrying it), in the walk's
/// order, each searched once and none landing.
#[tokio::test]
async fn eye_of_the_tiger_searches_at_most_four_groups_of_its_own_in_the_walks_order() {
    let f = Fixture::new().await;
    survivors_albums(&f, None);
    let (id, music_brainz, _server) = f.recorded_row(&EYE_OF_THE_TIGER).await;
    let service = f.service_with(Options {
        timeout_seconds: 1,
        music_brainz: Some(music_brainz),
        catalog: Some(RecordedCatalog::open() as Arc<dyn CatalogQuestions>),
        ..Default::default()
    });

    let landed = within(service.try_acquire_track(
        "soulseek",
        &id,
        false,
        Some("alice"),
        AcquisitionOrigin::Heart,
        &[],
    ))
    .await;

    assert!(!landed);
    assert_eq!(f.searched_albums(), [60, 61, 62, 63]);
}

/// Against All Odds heard live at Live Aid: Phil Collins leads no group that carries it, so the
/// walk asks for the Live Aid release, which Lidarr files under Various Artists.
#[tokio::test]
async fn the_live_aid_row_searches_live_aid() {
    let f = Fixture::new().await;
    f.lidarr.catalog_artist("va", "Various Artists", 70);
    f.lidarr.catalog_album(CatalogAlbum::new(
        LIVE_AID,
        "Live Aid",
        "va",
        71,
        &[(710, "Against All Odds (Take a Look at Me Now)", 1)],
    ));
    let (id, music_brainz, _server) = f.recorded_row(&AGAINST_ALL_ODDS_AT_LIVE_AID).await;
    let service = f.service_with(Options {
        timeout_seconds: 1,
        music_brainz: Some(music_brainz),
        catalog: Some(RecordedCatalog::open() as Arc<dyn CatalogQuestions>),
        ..Default::default()
    });

    let landed = within(service.try_acquire_track(
        "soulseek",
        &id,
        false,
        Some("alice"),
        AcquisitionOrigin::Heart,
        &[],
    ))
    .await;

    assert!(!landed);
    assert_eq!(f.searched_albums(), [71]);
}

/// The listened-from album, which the user's Lidarr holds unmonitored with two releases: Octo
/// pins it to the release the person listened from, waits for the rescan the pin queues, and the
/// file that rescan links makes the pick skip with no search sent. The album gets its saved
/// release choice back, `anyReleaseOk` included, and the walk's other picks are left alone too.
#[tokio::test]
async fn the_pin_is_followed_by_its_rescan_and_a_file_it_links_makes_the_pick_skip() {
    let f = Fixture::new().await;
    let (id, music_brainz, _server) = f.recorded_row(&EYE_OF_THE_TIGER).await;
    let catalog = RecordedCatalog::open();
    let song = heart_song(&f.ids.resolve_song(&id, false).expect("registered").snapshot());
    let listened = HeartQuestions::new(music_brainz.clone(), catalog.clone(), Lane::Heart)
        .identity(&song, Some(EYE_OF_THE_TIGER.catalog_album))
        .await
        .expect("answered")
        .listened_release
        .expect("the release the person listened from");
    survivors_albums(
        &f,
        Some(vec![(600, "another-release".into()), (601, listened.clone())]),
    );
    f.lidarr.user_album(EYE_OF_THE_TIGER_WALK[0], false);
    let saved = serde_json::json!([[600, true], [601, false]]);
    f.lidarr.place_file(
        "/data/music/Survivor/Survivor 0/04 - Eye of the Tiger.flac",
        b"the user's own",
    );
    let elsewhere = tempfile::tempdir().expect("a temp dir");
    let service = f.service_with(Options {
        timeout_seconds: 1,
        music_brainz: Some(music_brainz),
        catalog: Some(catalog as Arc<dyn CatalogQuestions>),
        octo_root: Some(elsewhere.path().to_path_buf()),
        ..Default::default()
    });

    let landed = within(service.try_acquire_track(
        "soulseek",
        &id,
        false,
        Some("alice"),
        AcquisitionOrigin::Heart,
        &[],
    ))
    .await;

    assert!(!landed);
    assert!(f.searched_albums().is_empty());
    let puts: Vec<Value> = f
        .lidarr
        .changes()
        .into_iter()
        .filter(|r| r.method == "PUT" && r.path == "/api/v1/album/60")
        .filter_map(|r| r.body)
        .collect();
    let choice = |body: &Value| {
        (
            body["anyReleaseOk"].clone(),
            Value::Array(
                body["releases"]
                    .as_array()
                    .expect("releases")
                    .iter()
                    .map(|r| serde_json::json!([r["id"], r["monitored"]]))
                    .collect(),
            ),
        )
    };
    // The pin: the listened-from release alone; then the restore, exactly as it was.
    assert_eq!(puts.len(), 2);
    assert_eq!(
        choice(&puts[0]),
        (Value::Bool(false), serde_json::json!([[600, false], [601, true]]))
    );
    assert_eq!(choice(&puts[1]), (Value::Bool(true), saved));
    // Between the pin and the restore, Octo waited on Lidarr's commands.
    let asked: Vec<String> = f
        .lidarr
        .requests()
        .iter()
        .map(|r| format!("{} {}", r.method, r.path))
        .collect();
    let pin = asked
        .iter()
        .position(|a| a == "PUT /api/v1/album/60")
        .expect("pinned");
    assert!(asked[pin..].iter().any(|a| a == "GET /api/v1/command"));
    assert!(service.left_alone("soulseek", &id));
}

/// An artist Octo used before: Octo moved its earlier imports into its own layout and deleted
/// Lidarr's records of them, so the artist reads as having no files and is used again.
#[tokio::test]
async fn an_artist_octo_used_before_whose_records_octo_deleted_is_used_again() {
    let f = Fixture::new().await;
    assert!(f.heart_the_album(None).await);
    f.job_wound_up().await;
    let mut deleted = f.lidarr.deleted();
    deleted.sort_unstable();
    assert_eq!(deleted, [100, 101]);

    f.grabs_at_once("dl-2", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);
    let service = f.service(None, 30);
    assert!(f.heart_teardrop(&service).await);

    assert_eq!(f.lidarr.searches(), 2);
    assert!(!service.left_alone("soulseek", "teardrop-heart"));
}

#[path = "lidarr_follow_tests.rs"]
mod follow;

#[path = "lidarr_hold_folder_tests.rs"]
mod hold_folder;

#[path = "lidarr_album_heart_tests.rs"]
mod album_heart;

#[path = "lidarr_track_fetcher_tests.rs"]
mod fetcher;

#[path = "lidarr_health_tests.rs"]
mod health;
