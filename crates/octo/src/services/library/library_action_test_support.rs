//! What the library action tests share: a person Octo asked about one track, a recording
//! `ILocalLibraryService`, and an executor whose parts are real but idle, for tests that never
//! reach them.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use octo_core::fingerprint::verification::{InconclusiveReason, VerificationResult};
use octo_core::models::domain::Song;
use octo_core::models::subsonic::ScanStatus;
use octo_core::settings::{AppSettings, SettingsStore};
use parking_lot::Mutex;

use super::library_action_executor::{LibraryActionExecutor, LibraryActionExecutorParts};
use super::{
    LibraryActionJournal, LibraryActionQuarantine, NavidromeSongPathResolver, NoticeQueue, UpgradeSources,
};
use crate::services::common::{StarOnArrival, TrackAcquisitionQueue};
use crate::services::local::{ILocalLibraryService, LocalSongMapping, ParsedExternalId, ParsedSongId};
use crate::services::soulseek::{ExternalIdRegistry, ISoulseekLink, RejectedPeerRegistry};
use crate::services::subsonic::NavidromeIdentityService;

/// A queue that asked `user` about Teardrop, Navidrome's `navidrome_id`.
pub(crate) fn asked_about(user: &str, navidrome_id: &str) -> Arc<NoticeQueue> {
    let queue = NoticeQueue::new();
    let unknown = VerificationResult {
        reason: InconclusiveReason::NoEntry,
        fingerprint: Some("AQADtEqk".into()),
        duration_seconds: 330,
        ..Default::default()
    };
    queue.add_review(
        user,
        "/music/teardrop.flac",
        &Song {
            artist: "Massive Attack".into(),
            title: "Teardrop".into(),
            album: "Mezzanine".into(),
            ..Default::default()
        },
        &unknown,
    );
    let key = NoticeQueue::review_key(user, "/music/teardrop.flac");
    queue.set_navidrome_id(&key, navidrome_id);
    queue.mark_queued(&[key]);
    Arc::new(queue)
}

/// A temp folder as the server's paths spell it: forward slashes, which Windows also accepts.
pub(crate) fn slash_root(dir: &Path) -> String {
    dir.to_string_lossy().replace('\\', "/")
}

pub(crate) fn store(settings: AppSettings) -> Arc<SettingsStore> {
    Arc::new(SettingsStore::for_tests(settings))
}

/// A stand-in `ILocalLibraryService`: answers nothing but the mappings it is given, records the
/// paths forgotten, and hands a forgotten path's mappings back as the real library does, taking
/// them again on a restore.
#[derive(Default)]
pub(crate) struct RecordingLibrary {
    pub forgotten: Mutex<Vec<String>>,
    pub mappings: Mutex<Vec<LocalSongMapping>>,
}

#[async_trait]
impl ILocalLibraryService for RecordingLibrary {
    async fn get_local_path_for_external_song(&self, _: &str, _: &str) -> Option<String> {
        None
    }

    async fn register_downloaded_song(&self, _: &Song, _: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn get_local_id_for_external_song(&self, _: &str, _: &str) -> Option<String> {
        None
    }

    fn parse_song_id(&self, _: &str) -> ParsedSongId {
        (false, None, None)
    }

    fn parse_external_id(&self, _: &str) -> ParsedExternalId {
        (false, None, None, None)
    }

    async fn get_mappings(&self) -> Vec<LocalSongMapping> {
        self.mappings.lock().clone()
    }

    async fn find_mapping_by_tags(
        &self,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Option<LocalSongMapping> {
        None
    }

    async fn forget_mapping(
        &self,
        local_path: &str,
    ) -> anyhow::Result<Vec<crate::services::local::LocalSongMapping>> {
        self.forgotten.lock().push(local_path.to_string());
        let wanted = super::navidrome_song_path_resolver::get_full_path(local_path);
        let mut mappings = self.mappings.lock();
        let (gone, kept): (Vec<LocalSongMapping>, Vec<LocalSongMapping>) =
            mappings.drain(..).partition(|mapping| {
                octo_core::common::dotnet::eq_ignore_case(
                    &super::navidrome_song_path_resolver::get_full_path(&mapping.local_path),
                    &wanted,
                )
            });
        *mappings = kept;
        Ok(gone)
    }

    async fn restore_mappings(&self, mappings: &[LocalSongMapping], local_path: &str) -> anyhow::Result<()> {
        self.mappings
            .lock()
            .extend(mappings.iter().cloned().map(|mapping| LocalSongMapping {
                local_path: local_path.to_string(),
                ..mapping
            }));
        Ok(())
    }

    async fn trigger_library_scan(&self, _force: bool) -> bool {
        false
    }

    async fn get_scan_status(&self) -> Option<ScanStatus> {
        None
    }
}

/// The optional parts, and the ones a test watches.
#[derive(Default)]
pub(crate) struct Extras {
    pub journal: Option<Arc<LibraryActionJournal>>,
    pub library: Option<Arc<RecordingLibrary>>,
    pub queue: Option<Arc<TrackAcquisitionQueue>>,
    pub ids: Option<Arc<ExternalIdRegistry>>,
    pub notices: Option<Arc<NoticeQueue>>,
    pub stars: Option<Arc<StarOnArrival>>,
    pub soulseek_link: Option<Arc<dyn ISoulseekLink>>,
    pub sources: Option<Arc<UpgradeSources>>,
    pub answers: Option<super::SongAnswers>,
}

/// An executor over `settings`; whatever `extras` leaves out is real and idle.
pub(crate) fn executor(settings: &Arc<SettingsStore>, extras: Extras) -> Arc<LibraryActionExecutor> {
    let http = crate::services::http_client_factory::default_client();
    let library: Arc<dyn ILocalLibraryService> = extras.library.unwrap_or_default();
    Arc::new(LibraryActionExecutor::new(LibraryActionExecutorParts {
        resolver: Arc::new(NavidromeSongPathResolver::new(
            NavidromeIdentityService::new(settings.clone(), http.clone()),
            library.clone(),
            http,
            settings.clone(),
        )),
        quarantine: Arc::new(LibraryActionQuarantine::new(settings.clone())),
        journal: extras.journal.unwrap_or_default(),
        library,
        ids: extras
            .ids
            .unwrap_or_else(|| Arc::new(ExternalIdRegistry::new(None::<&Path>))),
        rejected_peers: Arc::new(RejectedPeerRegistry::new(None::<&Path>, None)),
        acquisitions: extras.queue.unwrap_or_default(),
        settings: settings.clone(),
        notices: extras.notices,
        spectrum: None,
        stars: extras.stars,
        soulseek_link: extras.soulseek_link,
        sources: extras.sources,
        answers: extras.answers,
    }))
}
