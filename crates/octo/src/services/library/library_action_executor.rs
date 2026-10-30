//! `LibraryActionRequest`, `LibraryActionOutcome`, `LibraryActionCodes` and the executor.
//!
//! [`LibraryActionExecutor::apply`] takes no cancellation token: once started, an action runs to
//! its end, and the workers stop between actions instead. When the mappings file cannot be saved
//! after a delete, `apply` answers `Err`.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::Utc;
use futures::FutureExt;
use futures::future::BoxFuture;
use indexmap::IndexMap;
use octo_core::library::generated_playlist_service::dotnet_ticks;
use octo_core::lidarr::lidarr_client::LidarrError;
use octo_core::lyrics::{LyricsQuery, LyricsResult, LyricsTiming};
use octo_core::settings::{DownloadSource, LibraryAction, MetadataSettings, SettingsStore};
use octo_core::soulseek::{RoutingKind, SoulseekRouting};
use octo_media::audio::spectrum_analyzer::{SpectrumAnalyzer, SpectrumReport};
use octo_media::tags::{KeptIdentity, KeptIdentityTags};
use octo_subsonic::SubsonicCredential;
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use super::duplicate_fixer::DuplicateFixer;
use super::library_action_journal::{
    DeniedSource, LibraryActionEntry, LibraryActionJournal, LibraryActionState, action_name,
};
use super::library_action_quarantine::LibraryActionQuarantine;
use super::navidrome_song_path_resolver::get_full_path;
use super::notice_queue::NoticeQueue;
use super::rejections::RejectionStore;
use super::replacement_handoff::{
    BeforeReveal, OnRevealed, ReplacementHandoff, ReplacementRejectedException,
};
use super::upgrade_queue::{KeptFor, KeptRechecks};
use super::upgrade_sources::UpgradeSources;
use super::{NavidromeSongPathResolver, ResolvedSongFile};
use crate::services::common::download_picks::AnywaySong;
use crate::services::common::{StarOnArrival, TrackAcquisitionQueue};
use crate::services::cover_art::{CoverUpgradeWorker, RejectedCovers};
use crate::services::library::library_tag_edits::{self, song_tag_fields};
use crate::services::library::replacement_handoff::ArrivedTags;
use crate::services::local::{ILocalLibraryService, LocalSongMapping};
use crate::services::lyrics::{LyricsChoiceService, LyricsService, LyricsSidecarWriter};
use crate::services::soulseek::soulseek_link::{OFFLINE_TEXT, SoulseekLinkState};
use crate::services::soulseek::{
    ExternalIdRegistry, ISoulseekLink, RejectedPeerRegistry, SoulseekMetadataService,
};

/// Told the provider and external id of the replacement download the moment it is queued, so the
/// upgrade queue can follow that download's progress.
pub type OnReplacementQueued = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// One request for a library action.
#[derive(Clone)]
pub struct LibraryActionRequest {
    pub action: LibraryAction,
    pub navidrome_id: String,
    pub username: String,
    pub credential: Option<SubsonicCredential>,
    pub on_replacement_queued: Option<OnReplacementQueued>,
    /// A Delete of one copy of a song the library keeps another of (Library health's
    /// duplicates). The song is still wanted, so it is not refused when asked for again.
    pub only_a_copy: bool,
    /// The one source to fetch the replacement from, for a copy someone picked in Find songs or a
    /// Wrong song replacement the sweep holds to a source its preview checks; no other source is
    /// tried after it.
    pub only_source: Option<DownloadSource>,
    /// Better quality for a copy someone picked and said to take whatever its quality: the song
    /// the replacement is held to, and no quality check on it.
    pub anyway: Option<AnywaySong>,
}

impl LibraryActionRequest {
    pub fn new(action: LibraryAction, navidrome_id: impl Into<String>, username: impl Into<String>) -> Self {
        LibraryActionRequest {
            action,
            navidrome_id: navidrome_id.into(),
            username: username.into(),
            credential: None,
            on_replacement_queued: None,
            only_a_copy: false,
            only_source: None,
            anyway: None,
        }
    }

    /// `OnlyACopy: true`, the `copy=true` of an app's remove.
    pub fn only_a_copy(mut self, only_a_copy: bool) -> Self {
        self.only_a_copy = only_a_copy;
        self
    }

    pub fn with_credential(mut self, credential: Option<SubsonicCredential>) -> Self {
        self.credential = credential;
        self
    }
}

/// What happened to one library action request.
///
/// Code says WHY, for callers that act on the reason: the upgrade queue waits for Soulseek on
/// one and reports "no FLAC found" on the other. Detail stays the words for people.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LibraryActionOutcome {
    pub state: LibraryActionState,
    pub detail: Option<String>,
    pub code: Option<String>,

    /// For a replacement that went in: the new file, so a caller can say what it is.
    pub new_path: Option<String>,

    /// For a replacement that went in: where the original waits in quarantine.
    pub quarantine_path: Option<String>,
}

impl LibraryActionOutcome {
    pub fn new(state: LibraryActionState, detail: Option<String>) -> Self {
        Self {
            state,
            detail,
            ..Default::default()
        }
    }

    fn said(state: LibraryActionState, detail: impl Into<String>) -> Self {
        Self::new(state, Some(detail.into()))
    }

    fn coded(state: LibraryActionState, detail: impl Into<String>, code: Option<&str>) -> Self {
        Self {
            code: code.map(str::to_string),
            ..Self::said(state, detail)
        }
    }

    /// Whether the request has been consumed. Anything else leaves the track in the playlist
    /// and the rating set, so a request is never silently swallowed and retries if the
    /// operator fixes whatever blocked it.
    pub fn consumed(&self) -> bool {
        matches!(
            self.state,
            LibraryActionState::Applied | LibraryActionState::Skipped
        )
    }
}

/// `LibraryActionCodes`.
pub struct LibraryActionCodes;

impl LibraryActionCodes {
    /// slskd is not logged in to Soulseek, so a replacement could only fail.
    pub const SOULSEEK_OFFLINE: &'static str = "soulseekOffline";

    /// The search found no copy good enough to replace the song with.
    pub const NO_REPLACEMENT: &'static str = "noReplacement";
}

/// Navidrome shows this id at this file. Tests set it; otherwise the resolver.
pub type ShowsAt = Arc<dyn Fn(String, String) -> BoxFuture<'static, bool> + Send + Sync>;

/// Asks Navidrome for a forced scan. Tests set it; otherwise the library service.
pub type ForceScan = Arc<dyn Fn() -> BoxFuture<'static, bool> + Send + Sync>;

/// The executor's seams, which tests change: the history poll, its attempts, `ShowsAt` and
/// `ForceScan`.
#[derive(Clone)]
pub struct ExecutorSeams {
    pub history_poll: Duration,
    pub history_attempts: i32,
    pub shows_at: Option<ShowsAt>,
    pub force_scan: Option<ForceScan>,
}

impl Default for ExecutorSeams {
    fn default() -> Self {
        ExecutorSeams {
            history_poll: Duration::from_secs(15),
            history_attempts: 40,
            shows_at: None,
            force_scan: None,
        }
    }
}

/// What Wrong lyrics and Wrong cover work with: the lyrics lookups and pins, the lyrics files
/// beside songs, the cover upgrade's journaled writes, and what people turned down.
#[derive(Clone)]
pub struct SongAnswers {
    pub lyrics: Arc<LyricsService>,
    pub choices: Arc<LyricsChoiceService>,
    pub sidecars: Arc<LyricsSidecarWriter>,
    pub covers: Arc<CoverUpgradeWorker>,
    pub rejections: Arc<RejectionStore>,
}

/// What the executor is built from, the optional services last.
pub struct LibraryActionExecutorParts {
    pub resolver: Arc<NavidromeSongPathResolver>,
    pub quarantine: Arc<LibraryActionQuarantine>,
    pub journal: Arc<LibraryActionJournal>,
    pub library: Arc<dyn ILocalLibraryService>,
    pub ids: Arc<ExternalIdRegistry>,
    pub rejected_peers: Arc<RejectedPeerRegistry>,
    pub acquisitions: Arc<TrackAcquisitionQueue>,
    /// The library action, Soulseek and Subsonic settings, read at use.
    pub settings: Arc<SettingsStore>,
    pub notices: Option<Arc<NoticeQueue>>,
    pub spectrum: Option<Arc<SpectrumAnalyzer>>,
    pub stars: Option<Arc<StarOnArrival>>,
    pub soulseek_link: Option<Arc<dyn ISoulseekLink>>,
    pub sources: Option<Arc<UpgradeSources>>,
    pub answers: Option<SongAnswers>,
}

/// How a replacement ended: the outcome, where the original waits, and the new file.
struct Replaced {
    outcome: LibraryActionOutcome,
    quarantine_path: Option<String>,
    new_path: Option<String>,
    /// The replacement's own title and album, before it took the original's identity.
    arrived: Option<ArrivedTags>,
}

impl Replaced {
    fn failed(detail: impl Into<String>) -> Self {
        Replaced {
            outcome: LibraryActionOutcome::said(LibraryActionState::Failed, detail),
            quarantine_path: None,
            new_path: None,
            arrived: None,
        }
    }
}

/// Applies one library action. The only code in this feature that touches a file.
///
/// Every path through here either proves what it is acting on or does nothing. The resolver
/// supplies the proof, the quarantine makes a wrong answer recoverable, and the journal makes
/// a half-finished action reconcilable rather than repeatable.
pub struct LibraryActionExecutor {
    resolver: Arc<NavidromeSongPathResolver>,
    quarantine: Arc<LibraryActionQuarantine>,
    journal: Arc<LibraryActionJournal>,
    library: Arc<dyn ILocalLibraryService>,
    ids: Arc<ExternalIdRegistry>,
    rejected_peers: Arc<RejectedPeerRegistry>,
    acquisitions: Arc<TrackAcquisitionQueue>,
    settings: Arc<SettingsStore>,
    notices: Option<Arc<NoticeQueue>>,
    spectrum: Option<Arc<SpectrumAnalyzer>>,
    stars: Option<Arc<StarOnArrival>>,
    soulseek_link: Option<Arc<dyn ISoulseekLink>>,
    sources: Option<Arc<UpgradeSources>>,
    answers: Option<SongAnswers>,
    reconciled: AtomicBool,

    // The songs a library action is working on right now, by the original's full path. The
    // rating, playlist and weekly upgrade workers all call apply, and two replacements of
    // one song at once share one download: the second could quarantine or delete what the
    // first had just put in place. A set rather than a lock per song, because nobody waits for
    // it (the second action is skipped), so an entry can simply be removed when the action ends.
    // Paths compare ordinally, as on every platform but Windows.
    busy: Mutex<HashSet<String>>,
    seams: Mutex<ExecutorSeams>,
    /// Library health's duplicate fix, which Fix duplicates hands each request to.
    fixer: OnceLock<Arc<dyn DuplicateFixer>>,
    /// What a file a heart kept is replaced by: only a copy that heart's own gate passes.
    kept: OnceLock<Arc<KeptRechecks>>,
    /// The library's version check, which Check version hands each request to.
    version_checker: OnceLock<Arc<dyn super::version_check::VersionChecker>>,
    /// What the last Check version requests found, newest first.
    version_checks: Mutex<std::collections::VecDeque<String>>,
}

/// Takes a song out of the busy set when the action on it ends, however it ends.
struct BusyGuard<'a> {
    busy: &'a Mutex<HashSet<String>>,
    key: String,
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.busy.lock().remove(&self.key);
    }
}

/// Songs another fix holds in the executor's busy set; dropping it frees them.
pub struct SongsClaim<'a> {
    busy: &'a Mutex<HashSet<String>>,
    keys: Vec<String>,
}

impl Drop for SongsClaim<'_> {
    fn drop(&mut self) {
        let mut busy = self.busy.lock();
        for key in &self.keys {
            busy.remove(key);
        }
    }
}

/// The error types the replacement loop tells apart.
fn is_rejected(error: &anyhow::Error) -> Option<&ReplacementRejectedException> {
    error.downcast_ref::<ReplacementRejectedException>()
}

/// `FileNotFoundException`: a search that found nothing usable. A download reports it as an
/// `io::Error` of kind `NotFound`, Lidarr's `FileNotFound`, or the download base's
/// `FileNotFoundException` (a file gone before it could be kept).
fn is_file_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        || error
            .downcast_ref::<crate::services::common::FileNotFoundException>()
            .is_some()
        || matches!(
            error.downcast_ref::<LidarrError>(),
            Some(LidarrError::FileNotFound(_))
        )
}

/// `InvalidOperationException`, as Lidarr's client and fetcher report it, and as the Soulseek
/// download service does (no artist or title, no Lidarr, no download path).
fn is_invalid_operation(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<LidarrError>(),
        Some(LidarrError::InvalidOperation(_))
    ) || error
        .downcast_ref::<crate::services::soulseek::soulseek_download_service::InvalidOperationException>()
        .is_some()
}

/// Everything after the last separator.
fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// The extension: the name's last dot onward, empty when there is none or it ends there.
fn extension(path: &str) -> String {
    let name = file_name(path);
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => name[dot..].to_string(),
        _ => String::new(),
    }
}

/// The file's last write time in 100 ns ticks: 1601-01-01 for a file that is not there.
fn last_write_ticks(path: &str) -> i64 {
    const MISSING: i64 = 504_911_232_000_000_000;
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|modified| dotnet_ticks(chrono::DateTime::<Utc>::from(modified)))
        .unwrap_or(MISSING)
}

fn file_exists(path: &str) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

fn try_delete(path: &str) {
    if file_exists(path) {
        // Best effort.
        let _ = std::fs::remove_file(path);
    }
}

fn describe(action: LibraryAction) -> &'static str {
    match action {
        LibraryAction::Delete => "remove",
        LibraryAction::WrongSong => "replace (wrong song)",
        LibraryAction::WrongVersion => "replace (wrong version)",
        LibraryAction::BetterQuality => "upgrade",
        LibraryAction::Keep => "keep",
        LibraryAction::WrongLyrics => "look for other lyrics for",
        LibraryAction::WrongCover => "look for another cover for",
        LibraryAction::FixDuplicates => "fix the copies of",
        LibraryAction::CheckVersion => "check the version of",
    }
}

impl LibraryActionExecutor {
    /// Marks the files at `paths` busy for a fix made outside the executor, so no library action
    /// runs on them meanwhile, and none of them twice. None, and nothing marked, when one is
    /// busy already.
    pub fn claim_songs(&self, paths: &[&str]) -> Option<SongsClaim<'_>> {
        let mut busy = self.busy.lock();
        let keys: Vec<String> = paths.iter().map(|path| get_full_path(path)).collect();
        if keys.iter().any(|key| busy.contains(key)) {
            return None;
        }
        for key in &keys {
            busy.insert(key.clone());
        }
        Some(SongsClaim {
            busy: &self.busy,
            keys,
        })
    }

    pub const BUSY_TEXT: &'static str =
        "Another action on this song is still running, so this one was skipped.";
    pub const JOINED_TEXT: &'static str =
        "Another download of this song was already running, so nothing changed.";
    pub const HISTORY_KEPT_TEXT: &'static str =
        "Navidrome kept it as the same song, so its plays, favorites and playlist places stayed with it.";
    pub const HISTORY_LOST_TEXT: &'static str =
        "Navidrome took it for a new song, so its plays and playlist places stayed with the old entry.";

    pub fn new(parts: LibraryActionExecutorParts) -> Self {
        LibraryActionExecutor {
            resolver: parts.resolver,
            quarantine: parts.quarantine,
            journal: parts.journal,
            library: parts.library,
            ids: parts.ids,
            rejected_peers: parts.rejected_peers,
            acquisitions: parts.acquisitions,
            settings: parts.settings,
            notices: parts.notices,
            spectrum: parts.spectrum,
            stars: parts.stars,
            soulseek_link: parts.soulseek_link,
            sources: parts.sources,
            answers: parts.answers,
            reconciled: AtomicBool::new(false),
            busy: Mutex::new(HashSet::new()),
            fixer: OnceLock::new(),
            kept: OnceLock::new(),
            version_checker: OnceLock::new(),
            version_checks: Mutex::new(std::collections::VecDeque::new()),
            seams: Mutex::new(ExecutorSeams::default()),
        }
    }

    /// Sets the test seams.
    pub fn configure(&self, change: impl FnOnce(&mut ExecutorSeams)) {
        change(&mut self.seams.lock());
    }

    pub fn journal(&self) -> &Arc<LibraryActionJournal> {
        &self.journal
    }

    /// Hands every Fix duplicates request to Library health's duplicate fix.
    pub fn use_duplicate_fixer(&self, fixer: Arc<dyn DuplicateFixer>) {
        let _ = self.fixer.set(fixer);
    }

    /// Judges every Better quality of a file a heart kept by that heart's own gate. Once only.
    pub fn use_kept_rechecks(&self, kept: Arc<KeptRechecks>) {
        let _ = self.kept.set(kept);
    }

    /// Hands every Check version request to the library's version check.
    pub fn use_version_checker(&self, checker: Arc<dyn super::version_check::VersionChecker>) {
        let _ = self.version_checker.set(checker);
    }

    /// What the last few Check version requests found, newest first, for the action playlist's
    /// description.
    pub fn recent_version_checks(&self) -> Vec<String> {
        self.version_checks.lock().iter().cloned().collect()
    }

    /// How many Check version answers the playlist's description keeps.
    const VERSION_CHECKS_KEPT: usize = 5;

    /// Check version: the song's tags and version checked now, as Library health's Check now
    /// does. Never changes a file, so a dry run checks too. Consumed whatever it finds: what it
    /// found waits in Library health and on the playlist's description.
    async fn check_version(&self, request: &LibraryActionRequest) -> LibraryActionOutcome {
        let Some(checker) = self.version_checker.get() else {
            return LibraryActionOutcome::said(
                LibraryActionState::Skipped,
                "This server cannot check versions.",
            );
        };
        match checker.check(&request.navidrome_id).await {
            Ok(words) => {
                let mut kept = self.version_checks.lock();
                kept.push_front(words.clone());
                kept.truncate(Self::VERSION_CHECKS_KEPT);
                LibraryActionOutcome::said(LibraryActionState::Applied, words)
            }
            Err(why) => LibraryActionOutcome::said(LibraryActionState::Failed, why),
        }
    }

    /// Fix duplicates: every copy of the song is fixed as Library health's Fix these copies
    /// would, through the Delete action, so removing songs has to be on too. The song's
    /// questions in the Duplicates playlist are answered once the fix has started, or kept when
    /// every copy stays.
    async fn fix_duplicates(&self, request: &LibraryActionRequest) -> LibraryActionOutcome {
        let settings = self.settings.current().library_actions.clone();
        let removing = settings
            .effective_actions()
            .into_iter()
            .any(|entry| entry.action == LibraryAction::Delete && entry.enabled);
        if !removing {
            return LibraryActionOutcome::said(
                LibraryActionState::Skipped,
                "Removing songs is off: turn on the Delete action under Library actions.",
            );
        }
        if settings.dry_run {
            return LibraryActionOutcome::said(
                LibraryActionState::Rehearsed,
                format!(
                    "Dry run: would fix every copy of the song {} is one of.",
                    request.navidrome_id
                ),
            );
        }
        let Some(fixer) = self.fixer.get() else {
            return LibraryActionOutcome::said(
                LibraryActionState::Skipped,
                "This server cannot fix duplicates.",
            );
        };
        let answer = fixer
            .fix_copies_of(
                &request.navidrome_id,
                &request.username,
                request.credential.as_ref(),
            )
            .await;
        if answer.state == LibraryActionState::Applied
            && let Some(notices) = &self.notices
        {
            if answer.all_kept {
                notices.mark_kept(&request.username, &request.navidrome_id);
            } else {
                for copy in &answer.copies {
                    notices.mark_acted(copy);
                }
            }
        }
        LibraryActionOutcome::said(answer.state, answer.detail)
    }

    pub async fn apply(
        self: &Arc<Self>,
        request: LibraryActionRequest,
    ) -> anyhow::Result<LibraryActionOutcome> {
        let settings = self.settings.current().library_actions.clone();

        if !settings.enabled {
            return Ok(LibraryActionOutcome::said(
                LibraryActionState::Skipped,
                "Library actions are off.",
            ));
        }

        // Reconcile once per process before the first action. The playlist worker does this at
        // startup, but an install with only star ratings enabled never starts that worker, so an
        // interrupted action there was never looked at again.
        if !self.reconciled.swap(true, Ordering::SeqCst) {
            let quarantine = self.quarantine.clone();
            self.journal
                .reconcile(Some(&move |path: &str| quarantine.restore(path, None).moved));
        }
        if !settings.is_allowed(Some(&request.username)) {
            return Ok(LibraryActionOutcome::said(
                LibraryActionState::Skipped,
                format!("{} is not on the allowlist.", request.username),
            ));
        }

        let enabled = settings
            .effective_actions()
            .into_iter()
            .any(|entry| entry.action == request.action && entry.enabled);
        if !enabled {
            return Ok(LibraryActionOutcome::said(
                LibraryActionState::Skipped,
                format!("{} is not enabled.", action_name(request.action)),
            ));
        }

        if request.action == LibraryAction::FixDuplicates {
            return Ok(self.fix_duplicates(&request).await);
        }
        if request.action == LibraryAction::CheckVersion {
            return Ok(self.check_version(&request).await);
        }

        // Keep is an answer, not an operation. It touches no file, so there is nothing to rehearse
        // and nothing to prove about which file it is.
        if request.action == LibraryAction::Keep {
            let kept = self
                .notices
                .as_ref()
                .and_then(|notices| notices.mark_kept(&request.username, &request.navidrome_id));
            // Not something Octo asked about: nothing to answer and nothing to record. Five stars
            // on any other track is just a rating, and the journal holds real actions.
            let Some(kept) = kept else {
                return Ok(LibraryActionOutcome::said(
                    LibraryActionState::Skipped,
                    "Octo had not asked about this track.",
                ));
            };

            const KEPT_DETAIL: &str = "Kept. Octo will not ask about this track again.";
            self.journal.record(LibraryActionEntry {
                key: LibraryActionJournal::make_key(
                    request.action,
                    &request.navidrome_id,
                    &format!("keep:{}", dotnet_ticks(Utc::now())),
                ),
                title: kept.title.clone(),
                artist: kept.artist.clone(),
                album: kept.album.clone().unwrap_or_default(),
                ..Self::entry(
                    &request,
                    None,
                    LibraryActionState::Applied,
                    Some(KEPT_DETAIL),
                    false,
                )
            });
            info!("{} kept '{} - {}'", request.username, kept.artist, kept.title);
            return Ok(LibraryActionOutcome::said(
                LibraryActionState::Applied,
                KEPT_DETAIL,
            ));
        }

        let Some(resolved) = self.resolver.resolve(&request.navidrome_id).await else {
            // Never a success and never a delete. The request stays where it is, so fixing a
            // mount and waiting makes it work rather than requiring the user to ask again.
            let detail = "Could not work out which file this is, so nothing was touched.";
            self.journal.record(Self::entry(
                &request,
                None,
                LibraryActionState::Unresolved,
                Some(detail),
                settings.dry_run,
            ));
            return Ok(LibraryActionOutcome::said(LibraryActionState::Unresolved, detail));
        };

        // An answer about the lyrics or the cover can be given again and again, each time turning
        // down what shows now, so it is never "already done".
        if request.action.leaves_the_audio() {
            return Ok(self.answer(&request, &resolved, settings.dry_run).await);
        }

        let fingerprint =
            LibraryActionJournal::fingerprint(resolved.size_bytes, last_write_ticks(&resolved.absolute_path));
        if self
            .journal
            .already_applied(request.action, &request.navidrome_id, &fingerprint)
        {
            return Ok(LibraryActionOutcome::said(
                LibraryActionState::Skipped,
                "Already done.",
            ));
        }

        let key = LibraryActionJournal::make_key(request.action, &request.navidrome_id, &fingerprint);
        let pending = LibraryActionEntry {
            key: key.clone(),
            ..Self::entry(
                &request,
                Some(&resolved),
                LibraryActionState::Pending,
                None,
                settings.dry_run,
            )
        };

        if settings.dry_run {
            let detail = format!(
                "Dry run: would {} {}",
                describe(request.action),
                resolved.absolute_path
            );
            self.journal.record(LibraryActionEntry {
                state: LibraryActionState::Rehearsed,
                detail: Some(detail.clone()),
                ..pending
            });
            info!(
                "Library action {} by {} (DRY RUN): would {} {}",
                action_name(request.action),
                request.username,
                describe(request.action),
                resolved.absolute_path
            );
            // Deliberately NOT consumed: the same set replays every cycle so the operator sees
            // a stable list rather than a rehearsal that quietly emptied the playlist.
            return Ok(LibraryActionOutcome::said(LibraryActionState::Rehearsed, detail));
        }

        // Better quality with every source out could only fail, and before this it was journaled
        // as "no FLAC found" every sweep. Not consumed, so the request stays and runs once slskd
        // is back. Nothing is written and no file is touched. With Lidarr also set up, a Soulseek
        // outage just means Lidarr alone is asked. Wrong song and Wrong version have no source to
        // try only when a picked Soulseek is out, and wait the same way.
        if matches!(
            request.action,
            LibraryAction::BetterQuality | LibraryAction::WrongSong | LibraryAction::WrongVersion
        ) {
            let sources = self.sources_for(request.action, request.only_source).await;
            let replacing = request.action != LibraryAction::BetterQuality;
            if sources.is_empty() && (replacing || self.waiting_for_soulseek().await) {
                return Ok(LibraryActionOutcome::coded(
                    LibraryActionState::Failed,
                    OFFLINE_TEXT,
                    Some(LibraryActionCodes::SOULSEEK_OFFLINE),
                ));
            }
            if sources.is_empty() {
                return Ok(LibraryActionOutcome::said(
                    LibraryActionState::Failed,
                    "Better quality has no source set up: it needs slskd or Lidarr.",
                ));
            }
        }

        // Taken before the Pending entry is written: a second action on the same file content
        // has the same journal key and would overwrite the first one's entry.
        let song_key = get_full_path(&resolved.absolute_path);
        if !self.busy.lock().insert(song_key.clone()) {
            self.journal.record(LibraryActionEntry {
                key: LibraryActionJournal::make_key(
                    request.action,
                    &request.navidrome_id,
                    &format!("busy:{}", dotnet_ticks(Utc::now())),
                ),
                ..Self::entry(
                    &request,
                    Some(&resolved),
                    LibraryActionState::Skipped,
                    Some(Self::BUSY_TEXT),
                    false,
                )
            });
            info!(
                "Library action {} by {} skipped: another action on {} is still running",
                action_name(request.action),
                request.username,
                resolved.absolute_path
            );
            return Ok(LibraryActionOutcome::said(
                LibraryActionState::Skipped,
                Self::BUSY_TEXT,
            ));
        }
        let _busy = BusyGuard {
            busy: &self.busy,
            key: song_key,
        };
        self.apply_on_file(&request, &resolved, &key, pending).await
    }

    /// Wrong lyrics or Wrong cover on a song whose file is known. Recorded like Keep: one entry
    /// per press, never a pending one, since no file moves.
    async fn answer(
        &self,
        request: &LibraryActionRequest,
        resolved: &ResolvedSongFile,
        dry_run: bool,
    ) -> LibraryActionOutcome {
        let key = LibraryActionJournal::make_key(
            request.action,
            &request.navidrome_id,
            &format!("answer:{}", dotnet_ticks(Utc::now())),
        );
        let record = |outcome: &LibraryActionOutcome| {
            self.journal.record(LibraryActionEntry {
                key: key.clone(),
                ..Self::entry(
                    request,
                    Some(resolved),
                    outcome.state,
                    outcome.detail.as_deref(),
                    dry_run,
                )
            });
            self.journal.flush();
        };
        if dry_run {
            let outcome = LibraryActionOutcome::said(
                LibraryActionState::Rehearsed,
                format!(
                    "Dry run: would {} {}",
                    describe(request.action),
                    resolved.absolute_path
                ),
            );
            record(&outcome);
            return outcome;
        }
        let Some(answers) = &self.answers else {
            return LibraryActionOutcome::said(
                LibraryActionState::Failed,
                "This server cannot change lyrics or covers.",
            );
        };

        let song_key = get_full_path(&resolved.absolute_path);
        if !self.busy.lock().insert(song_key.clone()) {
            let outcome = LibraryActionOutcome::said(LibraryActionState::Skipped, Self::BUSY_TEXT);
            record(&outcome);
            return outcome;
        }
        let _busy = BusyGuard {
            busy: &self.busy,
            key: song_key,
        };
        let outcome = if request.action == LibraryAction::WrongLyrics {
            Self::wrong_lyrics(answers, request, resolved).await
        } else {
            Self::wrong_cover(answers, resolved).await
        };
        record(&outcome);
        info!(
            "Library action {} by {} on '{} - {}': {}",
            action_name(request.action),
            request.username,
            resolved.artist,
            resolved.title,
            outcome.detail.as_deref().unwrap_or("")
        );
        outcome
    }

    pub const NO_LYRICS_ANSWER_TEXT: &'static str =
        "No lyrics service answered, so the lyrics were left as they are. It will be tried again.";
    pub const NO_OTHER_LYRICS_TEXT: &'static str = "No other lyrics found; hidden.";
    pub const NO_OTHER_COVER_TEXT: &'static str = "No other cover found.";

    /// Turns down the lyrics the song shows now (its pin, or what the lookup would show) and
    /// shows the next source's, pinned so every player gets them. The song's audio is never
    /// touched and nothing is downloaded. With nothing else to show, its lyrics are hidden.
    async fn wrong_lyrics(
        answers: &SongAnswers,
        request: &LibraryActionRequest,
        resolved: &ResolvedSongFile,
    ) -> LibraryActionOutcome {
        let id = request.navidrome_id.as_str();
        let (artist, title) = (resolved.artist.as_str(), resolved.title.as_str());
        let audio = Path::new(&resolved.absolute_path);
        let query = LyricsQuery::new(
            artist,
            title,
            Some(resolved.album.clone()).filter(|album| !album.trim().is_empty()),
            resolved
                .duration_seconds
                .or_else(|| answers.sidecars.tags().duration_seconds(audio)),
        );
        let has = answers.sidecars.song_lyrics(audio);
        let (own, octos_own) = (has.timing, has.octos);
        let ct = tokio_util::sync::CancellationToken::new();
        let unanswered =
            || LibraryActionOutcome::said(LibraryActionState::Failed, Self::NO_LYRICS_ANSWER_TEXT);

        let mut skip = answers.rejections.rejected_lyrics(id);
        let mut turned_down: Vec<String> = Vec::new();
        match answers.choices.pin_for_song(id, Some(artist), Some(title)) {
            Some(pin) if pin.is_hidden() => {}
            Some(pin) => {
                turned_down.push(pin.choice);
                // Lyrics Octo wrote beside the song are a copy of the pin's.
                if octos_own {
                    turned_down.push(MetadataSettings::SONG_LYRICS_SOURCE.to_string());
                }
            }
            None => {
                let now = answers.lyrics.find_skipping(&query, &ct, own, &skip).await;
                if now.transient {
                    return unanswered();
                }
                match now.result {
                    Some(shown) if shown.is_songs_own() => {
                        turned_down.push(MetadataSettings::SONG_LYRICS_SOURCE.to_string());
                        // Lyrics Octo wrote beside the song came from a source; that one is
                        // turned down as well, or it would come straight back.
                        if octos_own {
                            let found = answers
                                .lyrics
                                .find_skipping(&query, &ct, LyricsTiming::None, &skip)
                                .await;
                            if found.transient {
                                return unanswered();
                            }
                            turned_down.extend(found.result.and_then(|found| found.candidate_id));
                        }
                    }
                    Some(shown) => turned_down.extend(shown.candidate_id),
                    None => {}
                }
            }
        }
        skip.extend(turned_down.iter().cloned());

        let next = answers.lyrics.find_skipping(&query, &ct, own, &skip).await;
        if next.transient {
            return unanswered();
        }
        // Written down before anything changes, so the next press never offers them again.
        for candidate in &turned_down {
            if !answers.rejections.reject_lyrics(id, candidate) {
                return LibraryActionOutcome::said(
                    LibraryActionState::Failed,
                    "Could not write down the rejected lyrics, so nothing was changed.",
                );
            }
        }

        let who = Some(request.username.as_str());
        let mut next = next.result;
        // A candidate whose lyrics cannot be fetched again is passed over for the one after it.
        for _ in 0..4 {
            match next.take() {
                Some(own) if own.is_songs_own() => {
                    answers.choices.clear_song(id, Some(artist), Some(title));
                    return LibraryActionOutcome::said(
                        LibraryActionState::Applied,
                        "Now showing the lyrics the song has.",
                    );
                }
                Some(found) if Self::showable(&found) => {
                    let candidate = found.candidate_id.clone().unwrap_or_default();
                    if answers
                        .choices
                        .pin(id, &candidate, Some(artist), Some(title), who, &ct)
                        .await
                    {
                        if let Err(error) = answers.sidecars.replace(audio, &found, &ct).await {
                            warn!(
                                "Wrong lyrics could not rewrite the lyrics beside {}: {error}",
                                resolved.absolute_path
                            );
                        }
                        return LibraryActionOutcome::said(
                            LibraryActionState::Applied,
                            format!("Now showing lyrics from {}.", found.source),
                        );
                    }
                    skip.insert(candidate);
                    let again = answers.lyrics.find_skipping(&query, &ct, own, &skip).await;
                    next = again.result;
                }
                _ => break,
            }
        }
        answers.choices.hide(id, Some(artist), Some(title), who);
        LibraryActionOutcome::said(LibraryActionState::Applied, Self::NO_OTHER_LYRICS_TEXT)
    }

    /// Lyrics a pin can name and show: words from a source, not an "instrumental".
    fn showable(found: &LyricsResult) -> bool {
        found.candidate_id.as_deref().is_some_and(|id| !id.is_empty())
            && !found.instrumental
            && (found.has_synced() || found.has_plain())
    }

    /// Turns down the picture the song's album shows now and puts another cover on the album,
    /// through the cover upgrade's journaled writes, so its Undo puts the old one back.
    /// Replace cover, from Library health's version check: the song's album cover is another
    /// artist's release's, so it is rejected and another found as Wrong cover finds one, whether
    /// or not that action is on. When no cover of the album by its own artist exists, `drawn`
    /// (Octo's own cover for it) goes in instead: never another artist's art. Undo puts the old
    /// picture back as it does for a cover run.
    pub async fn replace_cover_from_check(
        &self,
        navidrome_id: &str,
        drawn: impl FnOnce(&str, &str) -> Vec<u8>,
    ) -> LibraryActionOutcome {
        let Some(answers) = self.answers.as_ref() else {
            return LibraryActionOutcome::said(
                LibraryActionState::Skipped,
                "This server cannot change covers.",
            );
        };
        let Some(resolved) = self.resolver.resolve(navidrome_id).await else {
            return LibraryActionOutcome::said(
                LibraryActionState::Unresolved,
                "Octo could not find that song's file.",
            );
        };
        let outcome = Self::wrong_cover(answers, &resolved).await;
        if outcome.state != LibraryActionState::Skipped {
            return outcome;
        }
        let Some(album) = answers.covers.album_on_disk(&resolved.absolute_path).await else {
            return outcome;
        };
        let title = if resolved.album.trim().is_empty() {
            &resolved.title
        } else {
            &resolved.album
        };
        let by = resolved.album_artist.as_deref().unwrap_or(&resolved.artist);
        let bytes = drawn(title, by);
        match answers
            .covers
            .put_album_cover(&album, bytes, "Octo's drawn cover".to_string())
            .await
        {
            Ok(put) => LibraryActionOutcome::said(
                LibraryActionState::Applied,
                format!(
                    "No cover of this album by its own artist was found, so Octo's drawn cover went in on {} song(s).",
                    put.files
                ),
            ),
            Err(reason) => LibraryActionOutcome::said(LibraryActionState::Failed, reason),
        }
    }

    async fn wrong_cover(answers: &SongAnswers, resolved: &ResolvedSongFile) -> LibraryActionOutcome {
        let Some(album) = answers.covers.album_on_disk(&resolved.absolute_path).await else {
            return LibraryActionOutcome::said(
                LibraryActionState::Failed,
                "Could not read this song's tags, so its cover was left as it is.",
            );
        };
        // Written down first, so a stop part way can never bring this picture back.
        if let Some(shown) = album.shown.as_deref()
            && !answers.rejections.reject_cover(
                &album.key,
                &RejectedCovers::sha256(shown),
                octo_media::cover::cover_image::looks_hash(Some(shown)),
            )
        {
            return LibraryActionOutcome::said(
                LibraryActionState::Failed,
                "Could not write down the rejected cover, so nothing was changed.",
            );
        }
        let rejected = answers.rejections.rejected_covers(&album.key);
        match answers.covers.replace_album_cover(&album, &rejected).await {
            Err(reason) => LibraryActionOutcome::said(LibraryActionState::Failed, reason),
            Ok(None) => LibraryActionOutcome::said(LibraryActionState::Skipped, Self::NO_OTHER_COVER_TEXT),
            Ok(Some(replaced)) => LibraryActionOutcome::said(
                LibraryActionState::Applied,
                format!(
                    "Put in another cover from {} on {} song(s){}.",
                    replaced.source,
                    replaced.files,
                    if replaced.folder_cover {
                        " and the folder's cover file"
                    } else {
                        ""
                    }
                ),
            ),
        }
    }

    async fn apply_on_file(
        self: &Arc<Self>,
        request: &LibraryActionRequest,
        resolved: &ResolvedSongFile,
        key: &str,
        pending: LibraryActionEntry,
    ) -> anyhow::Result<LibraryActionOutcome> {
        // Read now, while Navidrome still knows the song here. Only the acting user's own
        // favorite can be read, with their own sign-in; anyone else's is out of reach.
        let carry_star = self.was_starred_by_requester(request).await;

        // Written BEFORE the file is touched. A crash between the two leaves this Pending, and
        // startup reconciles it against the filesystem rather than blindly re-running.
        self.journal.record(pending);
        if !self.journal.flush() {
            // The whole safety story rests on this entry being on disk before the file moves.
            const UNRECORDED: &str = "Could not write the action journal, so the file was not touched.";
            self.journal.complete(
                key,
                LibraryActionState::Failed,
                Some(UNRECORDED),
                None,
                None,
                None,
            );
            return Ok(LibraryActionOutcome::said(LibraryActionState::Failed, UNRECORDED));
        }

        let music_root = self.resolver.music_root();
        let outcome;
        let mut replaced: Option<Replaced> = None;
        let quarantine_path: Option<String>;
        if request.action == LibraryAction::Delete {
            // The song's own lyrics go with it, so nothing of it stays behind in the library.
            let moved = self.quarantine.move_file_with_sidecars(
                resolved,
                &music_root,
                request.action,
                &request.username,
                true,
            );
            if !moved.moved {
                self.journal.complete(
                    key,
                    LibraryActionState::Failed,
                    moved.error.as_deref(),
                    None,
                    None,
                    None,
                );
                return Ok(LibraryActionOutcome::new(LibraryActionState::Failed, moved.error));
            }
            quarantine_path = moved.quarantine_path;
            self.journal.complete(
                key,
                LibraryActionState::Pending,
                Some("Moved to quarantine; finishing."),
                quarantine_path.as_deref(),
                None,
                None,
            );
            self.journal.flush();
            // Kept in the trash note, so Put back links the outside song to the file again.
            let forgotten = self.library.forget_mapping(&resolved.absolute_path).await?;
            if let Some(trashed) = quarantine_path.as_deref() {
                self.quarantine.record_mappings(trashed, &forgotten);
            }
            outcome = LibraryActionOutcome::said(
                LibraryActionState::Applied,
                if request.only_a_copy {
                    "Removed this copy. The song stays in the library."
                } else {
                    "Removed. It will not be downloaded again."
                },
            );
        } else {
            // The original stays in place, playable, until its replacement has passed; the two
            // swap places in one moment.
            let done = self.reacquire(request, resolved, key, &music_root).await;
            outcome = done.outcome.clone();
            quarantine_path = done.quarantine_path.clone();
            replaced = Some(done);
        }

        self.journal.complete(
            key,
            outcome.state,
            outcome.detail.as_deref(),
            quarantine_path.as_deref(),
            None,
            None,
        );
        if outcome.state == LibraryActionState::Applied
            && let Some(notices) = &self.notices
        {
            notices.mark_acted(&request.navidrome_id);
        }
        self.journal.flush();
        let arrived = replaced.as_ref().and_then(|r| r.arrived.clone());
        let new_path = replaced.and_then(|r| r.new_path);
        // Off this call, which the rating and playlist workers wait on: it can take ten minutes.
        if outcome.state == LibraryActionState::Applied
            && let Some(new_path) = new_path.clone()
        {
            let executor = Arc::clone(self);
            let (request, original, key) = (request.clone(), resolved.clone(), key.to_string());
            tokio::spawn(async move {
                executor
                    .confirm_history_kept(&request, &original, &new_path, &key, carry_star)
                    .await;
                // Only now that the song's place is settled: the same path keeps the same song
                // in Navidrome whatever its tags say.
                if let Some(song) = request.anyway.as_ref().filter(|song| song.retitle) {
                    executor
                        .retitle(&original, &new_path, song, arrived.as_ref())
                        .await;
                }
            });
        }
        Ok(LibraryActionOutcome {
            new_path,
            quarantine_path,
            ..outcome
        })
    }

    async fn reacquire(
        self: &Arc<Self>,
        request: &LibraryActionRequest,
        original: &ResolvedSongFile,
        key: &str,
        music_root: &str,
    ) -> Replaced {
        if request.action == LibraryAction::WrongSong
            && let Some(denied) = self.blacklist_source(original, request).await
        {
            // Kept on the action's entry, so Put back lifts exactly this denial.
            self.journal.set_denied_source(key, denied);
        }

        // A file a heart kept (as heard, or anyway) is replaced only by a copy proven against it:
        // its own gate, from what it remembers of that heart, is the one every candidate meets.
        let kept = match (request.action, self.kept.get()) {
            (LibraryAction::BetterQuality, Some(kept)) => {
                kept.gate_for(&original.absolute_path, &request.username).await
            }
            _ => KeptFor::NotKept,
        };
        let kept_gate = match kept {
            KeptFor::NotKept => None,
            KeptFor::Gate(gate) => Some(*gate),
            KeptFor::Nothing(why) => {
                info!(
                    "Library action {}: nothing is tried for the kept {}: {why}",
                    action_name(request.action),
                    original.absolute_path
                );
                return Replaced {
                    outcome: LibraryActionOutcome::said(LibraryActionState::Skipped, why),
                    quarantine_path: None,
                    new_path: None,
                    arrived: None,
                };
            }
        };

        // Read while the original is in place: Navidrome built this song's ids from these tags.
        let identity = KeptIdentityTags::read(
            Path::new(&original.absolute_path),
            original.album_artist.as_deref(),
        );
        // What the original looked like when the action started. A download takes minutes, and
        // whatever is at that path by the end may no longer be the file this action was about.
        let admission = Arc::new(Admission {
            executor: Arc::clone(self),
            action: request.action,
            username: request.username.clone(),
            original: original.clone(),
            key: key.to_string(),
            music_root: music_root.to_string(),
            started_last_write: last_write_ticks(&original.absolute_path),
            quarantine_path: Mutex::new(None),
            any_quality: request.anyway.is_some(),
            forgotten: Mutex::new(Vec::new()),
        });
        // What each source tried before the last one said, for the words when all of them miss.
        let mut misses: Vec<String> = Vec::new();
        let earlier = |misses: &[String]| {
            if misses.is_empty() {
                String::new()
            } else {
                format!(" Before that, {}.", misses.join("; "))
            }
        };

        let mut handoff: Option<Arc<ReplacementHandoff>> = None;
        let attempt: Result<Option<String>, Arc<anyhow::Error>> = async {
            // Replaced anyway: the download looks for, and checks the copy against, the song the
            // person picked it as, which is the plain song when the library file is another take.
            let routing = match &request.anyway {
                Some(song) => {
                    info!(
                        "Library action {}: replacing '{} - {}' anyway for {}, held to '{}'{} and to no quality check",
                        action_name(request.action),
                        original.artist,
                        original.title,
                        request.username,
                        song.title,
                        song.length
                            .map_or_else(String::new, |length| format!(", {length} s long")),
                    );
                    SoulseekRouting {
                        kind: RoutingKind::Song,
                        artist: Some(original.artist.clone()),
                        title: Some(song.title.clone()),
                        album: song.album.clone(),
                        duration: song.length,
                        replaces_library_file: true,
                        ..Default::default()
                    }
                }
                None => SoulseekRouting {
                    kind: RoutingKind::Song,
                    artist: Some(original.artist.clone()),
                    title: Some(original.title.clone()),
                    album: Some(original.album.clone()),
                    duration: original.duration_seconds,
                    replaces_library_file: true,
                    ..Default::default()
                },
            };
            let external_id = self.ids.register(routing);
            if let Some(listener) = &request.on_replacement_queued {
                let told = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    listener(SoulseekMetadataService::PROVIDER_NAME, &external_id)
                }));
                if told.is_err() {
                    debug!("Replacement listener failed: it panicked");
                }
            }
            if identity.is_none() {
                warn!(
                    "Library action {}: could not read the tags of {}, so Navidrome will treat its replacement as a new song",
                    action_name(request.action),
                    original.absolute_path
                );
            }
            handoff = identity
                .clone()
                .map(|identity| Arc::new(admission.clone().handoff(identity)));
            // `download_song_internal` hands back a file it has a mapping for instead of
            // fetching. The handoff skips that, so only a download without one needs this.
            if handoff.is_none() {
                let forgotten = self
                    .library
                    .forget_mapping(&original.absolute_path)
                    .await
                    .map_err(Arc::new)?;
                admission.forgotten.lock().extend(forgotten);
            }

            // In order, and on to the next when one finds nothing or its copy fails the checks:
            // Soulseek first, then Lidarr, for Better quality with both set up.
            let sources = self.sources_for(request.action, request.only_source).await;
            let upgrade_search = request.action == LibraryAction::BetterQuality;
            let requested_by = self
                .settings
                .current()
                .subsonic
                .record_requested_by
                .then_some(request.username.as_str());
            let mut attempt = 0;
            // The file a replacement's check left waiting for AcoustID's bulk turn.
            let mut waiting: Vec<String> = Vec::new();
            loop {
                let Some(&source) = sources.get(attempt) else {
                    return Err(Arc::new(anyhow::anyhow!(
                        "Index was out of range. Must be non-negative and less than the size of the collection. (Parameter 'index')"
                    )));
                };
                // A replacement is work nobody waits on: its file's check takes the bulk turn, and
                // waits for it in the one hold path rather than keeping the download worker. A file
                // a heart kept is replaced only by a copy that heart's own gate passes.
                let completion = self.acquisitions.enqueue_holding(
                    Some(match &kept_gate {
                        Some(_) => octo_core::fingerprint::heart_gate::AcquisitionOrigin::Heart,
                        None => octo_core::fingerprint::heart_gate::AcquisitionOrigin::Replacement,
                    }),
                    kept_gate.iter().cloned().collect(),
                    std::mem::take(&mut waiting),
                    &crate::services::common::AcquisitionTarget::Shared,
                    false,
                    SoulseekMetadataService::PROVIDER_NAME,
                    &external_id,
                    true,
                    false,
                    true,
                    source,
                    false,
                    requested_by,
                    upgrade_search,
                    handoff.clone(),
                );
                let outcome = completion.wait().await;
                // Its file waits in the one hold path for AcoustID's bulk turn: once the lane gives
                // turns again, the same source goes on with that file.
                if let Err(e) = &outcome
                    && let Some(held) = e.downcast_ref::<crate::services::common::AcquisitionHeld>()
                    && self.acquisitions.wait_for_turn().await
                {
                    info!(
                        "Library action {}: the copy of '{} - {}' waited for AcoustID; going on with it",
                        action_name(request.action),
                        original.artist,
                        original.title
                    );
                    waiting = held.files.clone();
                    continue;
                }
                match outcome {
                    Ok(path) => return Ok(Some(path)),
                    Err(e)
                        if attempt + 1 < sources.len()
                            && handoff.as_ref().is_none_or(|h| h.revealed_path().is_none())
                            && (is_file_not_found(&e) || is_rejected(&e).is_some() || is_invalid_operation(&e)) =>
                    {
                        let why = is_rejected(&e)
                            .map(|rejected| rejected.problem.clone())
                            .unwrap_or_else(|| e.to_string());
                        let word = UpgradeSources::word(source.unwrap_or(DownloadSource::Soulseek));
                        misses.push(format!("{word}: {why}"));
                        info!(
                            "Library action {}: {word} had no copy of '{} - {}' ({why}); trying {}",
                            action_name(request.action),
                            original.artist,
                            original.title,
                            UpgradeSources::word(sources[attempt + 1].unwrap_or(DownloadSource::Soulseek))
                        );
                        attempt += 1;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        .await;

        let quarantine_path = || admission.quarantine_path.lock().clone();
        let replacement = match attempt {
            Ok(replacement) => replacement.unwrap_or_default(),
            Err(e) => {
                if let Some(rejected) = is_rejected(&e) {
                    // Refused before it was ever in the library; the original never moved.
                    self.log_refused(request, original, &rejected.problem);
                    return Replaced::failed(format!(
                        "The replacement {}, so nothing changed.{}",
                        rejected.problem,
                        earlier(&misses)
                    ));
                }
                if let Some(revealed) = handoff.as_ref().and_then(|h| h.revealed_path()) {
                    // The replacement already took the original's place and only the bookkeeping
                    // after it failed. Putting the original back now would undo a finished swap.
                    warn!(
                        "Library action {}: the replacement for '{} - {}' is in place at {revealed}, but what followed failed: {e}",
                        action_name(request.action),
                        original.artist,
                        original.title
                    );
                    let quarantined = quarantine_path();
                    if !self.settings.current().library_actions.keep_replaced_originals
                        && let Some(path) = &quarantined
                    {
                        try_delete(path);
                    }
                    return Replaced {
                        outcome: LibraryActionOutcome::said(
                            LibraryActionState::Applied,
                            format!("Replaced with {}.", file_name(&revealed)),
                        ),
                        quarantine_path: quarantined,
                        new_path: Some(revealed),
                        arrived: handoff.as_ref().and_then(|h| h.arrived()),
                    };
                }
                // A search that found nothing usable reports not found, passed through the queue
                // unchanged, and the upgrade queue reports exactly that case as "no FLAC found".
                let Some(quarantined) = quarantine_path() else {
                    return Replaced {
                        outcome: LibraryActionOutcome::coded(
                            LibraryActionState::Failed,
                            format!(
                                "Could not find a replacement ({e}), so nothing changed.{}",
                                earlier(&misses)
                            ),
                            is_file_not_found(&e).then_some(LibraryActionCodes::NO_REPLACEMENT),
                        ),
                        quarantine_path: None,
                        new_path: None,
                        arrived: None,
                    };
                };
                // Out, and nothing moved in: back to its exact path, whose row Navidrome still has.
                return Replaced {
                    outcome: self.restore_original(
                        &quarantined,
                        &format!("Could not finish the replacement ({e}), so nothing changed."),
                    ),
                    quarantine_path: Some(quarantined),
                    new_path: None,
                    arrived: None,
                };
            }
        };

        // A handoff that was never used: this joined a download of the same song already in
        // flight, whose file belongs to whoever started it. Judging it here could quarantine
        // or delete a replacement that action had just put in place, so nothing is touched.
        if handoff.as_ref().is_some_and(|h| h.revealed_path().is_none()) {
            self.log_refused(request, original, "was another action's download");
            return Replaced::failed(Self::JOINED_TEXT);
        }
        // Without the handoff the file is already in the library under its own name: judged now.
        if handoff.is_none() {
            match admission.admit(&replacement).await {
                Ok(None) => {}
                Ok(Some(late)) => {
                    self.log_refused(request, original, &late);
                    if !replacement.is_empty() {
                        try_delete(&replacement);
                    }
                    return Replaced::failed(format!("The replacement {late}, so nothing changed."));
                }
                Err(e) => {
                    // Any other failure after the download.
                    return match quarantine_path() {
                        None => Replaced::failed(format!(
                            "Could not find a replacement ({e}), so nothing changed.{}",
                            earlier(&misses)
                        )),
                        Some(quarantined) => Replaced {
                            outcome: self.restore_original(
                                &quarantined,
                                &format!("Could not finish the replacement ({e}), so nothing changed."),
                            ),
                            quarantine_path: Some(quarantined),
                            new_path: None,
                            arrived: None,
                        },
                    };
                }
            }
        }
        let quarantined = quarantine_path();
        if !self.settings.current().library_actions.keep_replaced_originals
            && let Some(path) = &quarantined
        {
            try_delete(path);
        }
        Replaced {
            outcome: LibraryActionOutcome::said(
                LibraryActionState::Applied,
                format!("Replaced with {}.", file_name(&replacement)),
            ),
            quarantine_path: quarantined,
            new_path: Some(replacement),
            arrived: handoff.as_ref().and_then(|h| h.arrived()),
        }
    }

    /// Gives a replacement that took another take's place the plain song's title and album, in
    /// place: its own, as the download tagged it, or else the ones it was held to. The album is
    /// left as it is when neither says one. Then scans, so the apps show the new names.
    pub async fn retitle(
        &self,
        original: &ResolvedSongFile,
        path: &str,
        song: &AnywaySong,
        arrived: Option<&ArrivedTags>,
    ) {
        let title = arrived
            .and_then(|tags| tags.title.clone())
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| song.title.clone());
        let album = arrived
            .and_then(|tags| tags.album.clone())
            .filter(|album| !album.trim().is_empty())
            .or_else(|| song.album.clone());
        let mut changes: IndexMap<String, Option<String>> = IndexMap::new();
        changes.insert(song_tag_fields::TITLE.to_string(), Some(title.clone()));
        if let Some(album) = &album {
            changes.insert(song_tag_fields::ALBUM.to_string(), Some(album.clone()));
        }
        let written = library_tag_edits::write(Path::new(path), &changes);
        if let Some(error) = &written.error {
            warn!(
                "Library action Better quality: '{} - {}' was replaced anyway, but its new title could not be written ({error})",
                original.artist, original.title
            );
            return;
        }
        info!(
            "Library action Better quality: '{} - {}' replaced anyway is now titled '{title}'{}",
            original.artist,
            original.title,
            album
                .as_deref()
                .map_or_else(String::new, |album| format!(" on '{album}'"))
        );
        let scan = self.seams.lock().force_scan.clone();
        match scan {
            Some(scan) => {
                scan().await;
            }
            None => {
                self.library.trigger_library_scan(false).await;
            }
        }
    }

    /// The handoff a replacement download takes: the original's place, its identity, the check
    /// before the swap and the note right after it.
    pub fn handoff_for(
        original: &ResolvedSongFile,
        identity: KeptIdentity,
        admit: BeforeReveal,
        revealed: Option<OnRevealed>,
    ) -> ReplacementHandoff {
        ReplacementHandoff::new(original.absolute_path.clone(), identity, admit, revealed)
    }

    fn log_refused(&self, request: &LibraryActionRequest, original: &ResolvedSongFile, problem: &str) {
        info!(
            "Library action {}: the replacement for '{} - {}' {problem}; the original stays",
            action_name(request.action),
            original.artist,
            original.title
        );
    }

    /// One scan, then watch the ORIGINAL id until it shows the replacement: proof that the
    /// plays, favorites and playlist places stayed with the song. About ten minutes, scanning
    /// again every two in case Navidrome was busy. If not shown, the rater's favorite.
    pub async fn confirm_history_kept(
        &self,
        request: &LibraryActionRequest,
        original: &ResolvedSongFile,
        new_path: &str,
        key: &str,
        carry_star: bool,
    ) -> bool {
        let kept = self.wait_until_shown(&original.navidrome_id, new_path).await;

        self.journal.complete(
            key,
            LibraryActionState::Applied,
            Some(&format!(
                "Replaced with {}. {}",
                file_name(new_path),
                if kept {
                    Self::HISTORY_KEPT_TEXT
                } else {
                    Self::HISTORY_LOST_TEXT
                }
            )),
            None,
            Some(kept),
            None,
        );
        self.journal.flush();
        if kept {
            info!(
                "Navidrome kept '{} - {}' as the same song after {}",
                original.artist,
                original.title,
                action_name(request.action)
            );
            return true;
        }
        warn!(
            "Navidrome did not keep '{} - {}' as the same song after {}; its plays and playlist places stay with the old entry",
            original.artist,
            original.title,
            action_name(request.action)
        );
        if carry_star && let (Some(stars), Some(credential)) = (&self.stars, &request.credential) {
            stars.star_when_visible(
                credential,
                &request.username,
                &original.artist,
                &original.title,
                new_path,
            );
        }
        false
    }

    /// Scans, then watches until Navidrome shows `navidrome_id` as a present song at exactly
    /// `path`: about ten minutes, scanning again every two in case Navidrome was busy.
    pub async fn wait_until_shown(&self, navidrome_id: &str, path: &str) -> bool {
        let seams = self.seams.lock().clone();
        let mut shown = false;
        let mut attempt = 0;
        while attempt < seams.history_attempts.max(1) && !shown {
            if attempt % 8 == 0 {
                match &seams.force_scan {
                    Some(scan) => {
                        scan().await;
                    }
                    None => {
                        self.library.trigger_library_scan(true).await;
                    }
                }
            }
            tokio::time::sleep(seams.history_poll).await;
            shown = match &seams.shows_at {
                Some(shows_at) => shows_at(navidrome_id.to_string(), path.to_string()).await,
                None => self.resolver.shows_at(navidrome_id, path).await,
            };
            attempt += 1;
        }
        shown
    }

    /// Where a replacement may come from, in the order tried. Better quality uses the upgrade
    /// sources that are set up and can search now (never YouTube, whose MP3 a lossless check
    /// refuses anyway), and searches the slow way, because nobody is waiting (#70). Wrong song and
    /// wrong version use the download source, except that Lidarr there means Lidarr too: before,
    /// those replacements went to Soulseek whatever was configured. A None entry is the default.
    /// A source picked by the request (`only`) is the one tried for any of the three, and none
    /// when it is Soulseek and Soulseek is out.
    pub async fn sources_for(
        &self,
        action: LibraryAction,
        only: Option<DownloadSource>,
    ) -> Vec<Option<DownloadSource>> {
        if matches!(
            action,
            LibraryAction::BetterQuality | LibraryAction::WrongSong | LibraryAction::WrongVersion
        ) && let Some(picked) = only
        {
            return if picked == DownloadSource::Soulseek && self.soulseek_out().await {
                Vec::new()
            } else {
                vec![Some(picked)]
            };
        }
        if action == LibraryAction::BetterQuality {
            // Without the plan (hosts that build the executor by hand): Soulseek, as it always was.
            let Some(sources) = &self.sources else {
                return if self.soulseek_out().await {
                    Vec::new()
                } else {
                    vec![Some(DownloadSource::Soulseek)]
                };
            };
            return sources.available().await.into_iter().map(Some).collect();
        }
        let lidarr_hearts = self.settings.current().subsonic.download_source == DownloadSource::Lidarr
            && self
                .sources
                .as_ref()
                .is_some_and(|s| s.plan().contains(&DownloadSource::Lidarr));
        if lidarr_hearts {
            vec![Some(DownloadSource::Lidarr), None]
        } else {
            vec![None]
        }
    }

    async fn waiting_for_soulseek(&self) -> bool {
        match &self.sources {
            Some(sources) => sources.waiting_for_soulseek().await,
            None => self.soulseek_out().await,
        }
    }

    async fn soulseek_out(&self) -> bool {
        match &self.soulseek_link {
            Some(link) => link
                .read(false)
                .await
                .is_some_and(|reading| reading.link == SoulseekLinkState::NotLoggedIn),
            None => false,
        }
    }

    /// Whether the person asking for a replacement had favorited the song. False when nothing
    /// will replace it, when the request carries no sign-in (playlist actions never do), or when
    /// Navidrome cannot say.
    pub async fn was_starred_by_requester(&self, request: &LibraryActionRequest) -> bool {
        if !matches!(
            request.action,
            LibraryAction::WrongSong | LibraryAction::WrongVersion | LibraryAction::BetterQuality
        ) {
            return false;
        }
        let (Some(credential), Some(stars)) = (&request.credential, &self.stars) else {
            return false;
        };
        stars.is_starred(credential, &request.navidrome_id).await
    }

    /// Why a replacement is not good enough to keep, or None when it is. With `any_quality`, a
    /// replacement the person said to take whatever its quality only has to have arrived.
    fn unacceptable(
        action: LibraryAction,
        path: &str,
        original: &ResolvedSongFile,
        any_quality: bool,
    ) -> Option<String> {
        if path.is_empty() || !file_exists(path) {
            return Some("never arrived".into());
        }

        let length = std::fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0);
        if length == 0 {
            return Some("was empty".into());
        }

        if action != LibraryAction::BetterQuality || any_quality {
            return None;
        }

        // Better quality has to actually be better, or the action quietly downgrades a library.
        const LOSSLESS: [&str; 6] = [".flac", ".wav", ".aiff", ".aif", ".alac", ".ape"];
        let ext = extension(path);
        if !LOSSLESS
            .iter()
            .any(|l| octo_core::common::dotnet::eq_ignore_case(l, &ext))
        {
            return Some("is not lossless".into());
        }
        if length <= original.size_bytes {
            return Some("is no larger than the original".into());
        }

        None
    }

    /// Better quality means genuinely lossless: a FLAC made from an MP3 is no upgrade over the MP3
    /// it replaces, and over a real FLAC it is a downgrade. None when the spectrum says nothing
    /// against it, which includes the check being off or unable to run.
    pub fn not_really_lossless(report: &SpectrumReport) -> Option<String> {
        report
            .is_likely_lossy()
            .then(|| format!("is {}", report.describe()))
    }

    async fn spectrum_of(&self, path: &str) -> SpectrumReport {
        let settings = self.settings.current().soulseek.clone();
        match &self.spectrum {
            Some(spectrum) if settings.detect_transcodes => {
                spectrum
                    .analyze(
                        Path::new(path),
                        settings.effective_transcode_check_timeout_seconds(),
                    )
                    .await
            }
            _ => SpectrumReport::unknown("not checked", 0),
        }
    }

    fn restore_original(&self, quarantine_path: &str, detail: &str) -> LibraryActionOutcome {
        let restored = self.quarantine.restore(quarantine_path, None);
        if restored.moved {
            LibraryActionOutcome::said(LibraryActionState::Failed, detail)
        } else {
            LibraryActionOutcome::said(
                LibraryActionState::Failed,
                format!(
                    "{detail} The original could not be put back automatically and is in the quarantine folder."
                ),
            )
        }
    }

    /// Remember the peer that delivered a wrong file, so the ranking never offers it again.
    ///
    /// Only possible when the download recorded who sent it, which Octo started doing alongside
    /// this feature. For a file downloaded before that, or one Octo never downloaded, there is
    /// nothing to blacklist and the re-acquire is an ordinary search.
    /// Denies the Soulseek file the original came from, when Octo knows who sent it, and says
    /// which one it denied.
    async fn blacklist_source(
        &self,
        original: &ResolvedSongFile,
        request: &LibraryActionRequest,
    ) -> Option<DeniedSource> {
        if !self.settings.current().soulseek.verify_downloads {
            return None;
        }

        let mapping = self
            .library
            .find_mapping_by_tags(
                Some(&original.artist),
                Some(&original.title),
                Some(&original.album),
            )
            .await;

        let peer = mapping
            .as_ref()
            .and_then(|m| m.source_peer.clone())
            .filter(|p| !p.is_empty());
        let file = mapping
            .as_ref()
            .and_then(|m| m.source_file.clone())
            .filter(|f| !f.is_empty());
        let (Some(peer), Some(file)) = (peer, file) else {
            info!(
                "Library action Wrong song: no record of who sent '{} - {}', so the search runs again without a blacklist",
                original.artist, original.title
            );
            return None;
        };

        // A file denied already was not denied by this action, so Put back leaves it denied.
        let already = self.rejected_peers.is_denied(Some(&peer), Some(&file));
        self.rejected_peers.deny(
            Some(&peer),
            Some(&file),
            &format!("reported as the wrong song by {}", request.username),
            &format!("{} - {}", original.artist, original.title),
        );
        (!already).then_some(DeniedSource {
            username: peer,
            filename: file,
        })
    }

    fn entry(
        request: &LibraryActionRequest,
        resolved: Option<&ResolvedSongFile>,
        state: LibraryActionState,
        detail: Option<&str>,
        dry_run: bool,
    ) -> LibraryActionEntry {
        LibraryActionEntry {
            key: LibraryActionJournal::make_key(
                request.action,
                &request.navidrome_id,
                if resolved.is_none() {
                    "unresolved"
                } else {
                    "pending"
                },
            ),
            action: request.action,
            navidrome_id: request.navidrome_id.clone(),
            username: request.username.clone(),
            title: resolved.map(|r| r.title.clone()).unwrap_or_default(),
            artist: resolved.map(|r| r.artist.clone()).unwrap_or_default(),
            album: resolved.map(|r| r.album.clone()).unwrap_or_default(),
            source_path: resolved.map(|r| r.absolute_path.clone()),
            quarantine_path: None,
            resolution: resolved.map(|r| r.source),
            state,
            detail: detail.map(str::to_string),
            dry_run,
            at_utc: Utc::now(),
            history_kept: None,
            revealed_path: None,
            copy: request.only_a_copy,
            download_allowed: false,
            denied_source: None,
        }
    }
}

/// What the admission check and the reveal note share: one replacement's original, where it
/// started, and where it went once it was moved out.
struct Admission {
    executor: Arc<LibraryActionExecutor>,
    action: LibraryAction,
    username: String,
    original: ResolvedSongFile,
    key: String,
    music_root: String,
    started_last_write: i64,
    quarantine_path: Mutex<Option<String>>,
    /// The person said to take the replacement whatever its quality.
    any_quality: bool,
    /// The outside songs the original was mapped as, forgotten before a download that could not
    /// carry its identity: saved with the original once it is in the trash.
    forgotten: Mutex<Vec<LocalSongMapping>>,
}

impl Admission {
    /// The handoff over this admission's check and note.
    fn handoff(self: Arc<Self>, identity: KeptIdentity) -> ReplacementHandoff {
        let admitting = self.clone();
        let admit: BeforeReveal = Arc::new(move |candidate: String| {
            let admission = admitting.clone();
            async move {
                // The download cannot be told of a failure here, so it is logged and the swap
                // goes ahead: the original is already out.
                admission.admit(&candidate).await.unwrap_or_else(|e| {
                    warn!("Library action: the original's mapping could not be forgotten: {e}");
                    None
                })
            }
            .boxed()
        });
        let revealing = self.clone();
        let revealed: OnRevealed = Arc::new(move |path: &str| revealing.revealed(path));
        LibraryActionExecutor::handoff_for(&self.original, identity, admit, Some(revealed))
    }

    /// Judges the new file and, only when it passes, moves the original out. Called by the
    /// download just before the replacement moves in, or by the executor for one that ran
    /// without the handoff, so a scan never sees the song missing for the length of a download.
    async fn admit(&self, candidate: &str) -> anyhow::Result<Option<String>> {
        let executor = &self.executor;
        let mut problem =
            LibraryActionExecutor::unacceptable(self.action, candidate, &self.original, self.any_quality);
        if problem.is_none() && self.action == LibraryAction::BetterQuality && !self.any_quality {
            problem = LibraryActionExecutor::not_really_lossless(&executor.spectrum_of(candidate).await);
        }
        if problem.is_some() {
            return Ok(problem);
        }
        let now = std::fs::metadata(&self.original.absolute_path)
            .ok()
            .filter(|m| m.is_file());
        let changed = match now {
            None => true,
            Some(now) => {
                now.len() as i64 != self.original.size_bytes
                    || last_write_ticks(&self.original.absolute_path) != self.started_last_write
            }
        };
        if changed {
            return Ok(Some("found the original changed while it was downloading".into()));
        }
        let moved =
            executor
                .quarantine
                .move_file(&self.original, &self.music_root, self.action, &self.username);
        if !moved.moved {
            return Ok(Some(format!(
                "could not take the original's place ({})",
                moved.error.unwrap_or_default()
            )));
        }
        *self.quarantine_path.lock() = moved.quarantine_path.clone();
        // On disk before the replacement moves in: a crash in between puts the original back.
        executor.journal.complete(
            &self.key,
            LibraryActionState::Pending,
            Some("Moved to quarantine; finishing."),
            moved.quarantine_path.as_deref(),
            None,
            None,
        );
        executor.journal.flush();
        // Only now that the original is out: a refused replacement leaves it with its
        // mapping, and so with the record of who sent it.
        let mut forgotten = std::mem::take(&mut *self.forgotten.lock());
        forgotten.extend(
            executor
                .library
                .forget_mapping(&self.original.absolute_path)
                .await?,
        );
        // Saved with the original, so Put back gives it its outside songs again, a kept-as-heard
        // file's heart with them.
        if let Some(trashed) = moved.quarantine_path.as_deref() {
            executor.quarantine.record_mappings(trashed, &forgotten);
        }
        Ok(None)
    }

    /// Written the moment the replacement is in the original's place, so a restart after it
    /// finds the swap done rather than putting the original back beside it.
    fn revealed(&self, path: &str) {
        let quarantine_path = self.quarantine_path.lock().clone();
        self.executor.journal.complete(
            &self.key,
            LibraryActionState::Pending,
            Some("Replacement placed; finishing."),
            quarantine_path.as_deref(),
            None,
            Some(path),
        );
        self.executor.journal.flush();
    }
}

#[cfg(test)]
#[path = "library_action_executor_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "library_action_one_at_a_time_tests.rs"]
mod one_at_a_time_tests;

#[cfg(test)]
#[path = "library_action_answer_tests.rs"]
mod answer_tests;
