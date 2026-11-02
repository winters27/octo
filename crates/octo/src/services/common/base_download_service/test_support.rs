//! What the download base's tests share: one test backend whose transfer lands whatever file
//! the test says, and a builder for the service around it.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use octo_core::models::domain::Song;
use octo_core::settings::{AppSettings, SettingsStore};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use super::{BaseDownloadService, DownloadBackend, DownloadCore, DownloadServices, TrackDownload};
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::local::{DownloadHistoryService, ILocalLibraryService};
use crate::services::notifications::NotificationService;
use crate::services::subsonic::NavidromeIdentityService;

/// How the test catalog's album ids start; the rest is the album's own id.
pub(crate) const TEST_ALBUM: &str = "test-album:";

/// What the transfer does with the song: correct it the way a backend would, and say where the
/// file landed.
pub(crate) type Landing = Arc<dyn Fn(&mut Song, &str) -> String + Send + Sync>;

/// The provider "test": the transfer is stubbed out, so placement and tagging can be driven
/// against real files in a temp folder.
#[derive(Default)]
pub(crate) struct TestBackend {
    pub landing: Mutex<Option<Landing>>,
    pub transfers: AtomicUsize,
    /// What the download's own gates say of a file that landed for someone else while it waited:
    /// None judges nothing, else each gate accepts it (true) or refuses it (false).
    pub landed_verdict: Mutex<Option<bool>>,
    /// The files of the song the download's gates judged that way.
    pub judged: Mutex<Vec<String>>,
}

impl TestBackend {
    pub fn land(&self, landing: impl Fn(&mut Song, &str) -> String + Send + Sync + 'static) {
        *self.landing.lock() = Some(Arc::new(landing));
    }

    pub fn transfers(&self) -> usize {
        self.transfers.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl DownloadBackend for TestBackend {
    fn provider_name(&self) -> &str {
        "test"
    }

    async fn is_available(&self, _: &BaseDownloadService) -> bool {
        true
    }

    async fn download_track(
        &self,
        _: &BaseDownloadService,
        download: &mut TrackDownload,
        _: &CancellationToken,
    ) -> anyhow::Result<String> {
        self.transfers.fetch_add(1, Ordering::SeqCst);
        let landing = self.landing.lock().clone();
        match landing {
            Some(landing) => Ok(landing(&mut download.song, &download.track_id)),
            None => anyhow::bail!("Specified method is not supported."),
        }
    }

    /// An album id the test catalog gives as `test-album:<id>`.
    fn extract_external_id_from_album_id(&self, album_id: &str) -> Option<String> {
        album_id.strip_prefix(TEST_ALBUM).map(str::to_string)
    }

    async fn judge_landed(
        &self,
        _: &BaseDownloadService,
        download: &mut TrackDownload,
        path: &str,
    ) -> Option<bool> {
        let passes = (*self.landed_verdict.lock())?;
        self.judged.lock().push(path.to_string());
        for gate in &download.gates {
            let outcome = if passes {
                crate::services::common::GateOutcome::Accepted
            } else {
                crate::services::common::GateOutcome::NotAccepted("another version".into())
            };
            download.ledger.record(&gate.user, gate.origin, outcome);
        }
        Some(passes)
    }
}

/// One service under test and what the tests read back.
pub(crate) struct Harness {
    pub service: Arc<BaseDownloadService>,
    pub backend: Arc<TestBackend>,
    pub history: Arc<DownloadHistoryService>,
}

/// The service over `root`, its music folder, with these settings (auto-detect off) and these
/// services. A library or catalog left out knows nothing.
pub(crate) fn build(
    root: &Path,
    mut settings: AppSettings,
    library: Option<Arc<dyn ILocalLibraryService>>,
    metadata: Option<Arc<dyn IMusicMetadataService>>,
    services: DownloadServices,
) -> Harness {
    settings.subsonic.auto_detect_download_path = false;
    let settings = Arc::new(SettingsStore::for_tests(settings));
    settings.set_raw("Library:DownloadPath", Some(&root.to_string_lossy()));
    let history = Arc::new(DownloadHistoryService::new(root.join("history.json")));
    let backend = Arc::new(TestBackend::default());
    let core = DownloadCore {
        settings: Arc::clone(&settings),
        local_library: library.unwrap_or_else(|| Arc::new(FakeLocalLibrary::default())),
        metadata: metadata.unwrap_or_else(|| Arc::new(FakeMetadata::default())),
        navidrome_identity: NavidromeIdentityService::new(Arc::clone(&settings), reqwest::Client::new()),
        history: Arc::clone(&history),
        notifications: Arc::new(NotificationService::new(Vec::new(), Arc::clone(&settings), None)),
    };
    let service = BaseDownloadService::new(core, services, Arc::clone(&backend) as Arc<dyn DownloadBackend>);
    Harness {
        service,
        backend,
        history,
    }
}

/// A catalog that knows these songs by id under the provider "test".
pub(crate) fn catalog(songs: Vec<(&str, Song)>) -> Arc<FakeMetadata> {
    let metadata = FakeMetadata::default();
    for (id, song) in songs {
        metadata
            .songs
            .lock()
            .insert(("test".to_string(), id.to_string()), song);
    }
    Arc::new(metadata)
}
