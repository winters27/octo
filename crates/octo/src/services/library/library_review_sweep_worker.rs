//! The library Review sweep (#72), its state (`<config>/review-sweep.json`) and the fingerprint
//! check it asks. With the switch on, a file AcoustID clearly hears as another song is replaced
//! through Wrong song, and Octo's own downloads can be checked again in one pass.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use futures::FutureExt;
use octo_core::common::dotnet;
use octo_core::common::live_version;
use octo_core::common::song_identity::ArtistAgreement;
use octo_core::common::{Clock, SongIdentity};
use octo_core::fingerprint::verification::{
    InconclusiveReason, VerificationResult, VerificationVerdict, whole_percent,
};
use octo_core::json::datetime;
use octo_core::library::generated_playlist_service::dotnet_ticks;
use octo_core::models::domain::Song;
use octo_core::settings::{
    AppSettings, DownloadSource, LibraryAction, LibraryActionSettings, SettingsStore, SoulseekSettings,
};
use octo_media::tags::TagFile;
use octo_media::tags::tag_writer_extras::{self, TagFields};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::NavidromeSongPathResolver;
use super::library_action_executor::{
    LibraryActionCodes, LibraryActionExecutor, LibraryActionOutcome, LibraryActionRequest,
};
use super::library_action_journal::LibraryActionState;
use super::navidrome_playlist_api::{NavidromePlaylistApi, SongMarks};
use super::navidrome_song_path_resolver::get_full_path;
use super::notice_queue::{NoticeOrigin, NoticeQueue, set_aside};
use super::quality_upgrade_worker::compare_ordinal;
use crate::services::common::IAcquisitionActivity;
use crate::services::family::family_store::FamilyStore;
use crate::services::family::{LibraryScope, NavidromeAdmin, ScopeResolver, ScopeView};
use crate::services::fingerprint::AcoustIdRateLimiter;
use crate::services::fingerprint::preview_check::{Heard, PreviewOutcome, PreviewRequest, PreviewSource};
use crate::services::framework::DotnetDictionary;
use crate::services::health::keep_marks::MemberMarks;
use crate::services::library::library_walk;
use crate::services::local::ILocalLibraryService;
use crate::services::soulseek::soulseek_download_service::excluded_folder_names;
use crate::services::state_file;

/// The one question the sweep asks of a file, behind a seam so tests need no fpcalc.
#[async_trait]
pub trait IReviewSweepVerifier: Send + Sync {
    fn is_ready(&self) -> bool;
    async fn verify(&self, path: &str, artist: Option<&str>, title: Option<&str>) -> VerificationResult;

    /// Whether the music database lists `isrc` on the recording AcoustID heard in `question`:
    /// Some(true) when it does, Some(false) when it lists codes for those recordings and none is
    /// this one, None when that cannot be told.
    async fn isrc_agrees(&self, question: &VerificationResult, isrc: &str) -> Option<bool>;

    /// Whether the file sounds like the song's YouTube preview, for a sound AcoustID has never
    /// heard (the same question a new download is asked, [`preview_check`]). NotAsked where there
    /// is no preview source to ask.
    ///
    /// [`preview_check`]: crate::services::fingerprint::preview_check
    async fn hear_preview(
        &self,
        _path: &str,
        _request: &PreviewRequest<'_>,
        _question: &VerificationResult,
    ) -> PreviewOutcome {
        PreviewOutcome::NotAsked
    }
}

/// What the sweep needs of `DownloadVerificationService`: whether a lookup can happen
/// (`is_fingerprinting_enabled`) and `verify(path, artist, title)` with no ISRC and `refuse_live`
/// off.
///
/// `DownloadVerificationService` implements it; `AppState::build` hands it to the
/// [`FingerprintSweepVerifier`] with `set_service`.
#[async_trait]
pub trait SweepVerification: Send + Sync {
    fn is_fingerprinting_enabled(&self) -> bool;
    async fn verify(&self, path: &str, artist: Option<&str>, title: Option<&str>) -> VerificationResult;

    /// See [`IReviewSweepVerifier::isrc_agrees`].
    async fn isrc_agrees(&self, question: &VerificationResult, isrc: &str) -> Option<bool>;

    /// See [`IReviewSweepVerifier::hear_preview`]; `source` is where previews come from.
    async fn hear_preview(
        &self,
        _source: &dyn PreviewSource,
        _path: &str,
        _request: &PreviewRequest<'_>,
        _question: &VerificationResult,
    ) -> PreviewOutcome {
        PreviewOutcome::NotAsked
    }
}

/// The real check, in AcoustID's background lane. Resolved late, because the workers are built
/// before the download service is first used: the service is set once it exists, and
/// until then the sweep is not ready.
#[derive(Default)]
pub struct FingerprintSweepVerifier {
    service: OnceLock<Arc<dyn SweepVerification>>,
    /// Where a song's YouTube preview comes from, once the shim's client exists.
    preview: OnceLock<Arc<dyn PreviewSource>>,
}

impl FingerprintSweepVerifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// The verification service, once built. A second call is ignored.
    pub fn set_service(&self, service: Arc<dyn SweepVerification>) {
        let _ = self.service.set(service);
    }

    /// Where previews come from. A second call is ignored; without one a sound AcoustID has
    /// never heard is a question, as it always was.
    pub fn set_preview(&self, source: Arc<dyn PreviewSource>) {
        let _ = self.preview.set(source);
    }
}

#[async_trait]
impl IReviewSweepVerifier for FingerprintSweepVerifier {
    fn is_ready(&self) -> bool {
        self.service.get().is_some_and(|s| s.is_fingerprinting_enabled())
    }

    // No ISRC: the file's own tag held against itself proves nothing, and WithTaggedIsrc would
    // turn a lookup AcoustID never answered into a confirmation.
    async fn verify(&self, path: &str, artist: Option<&str>, title: Option<&str>) -> VerificationResult {
        match self.service.get() {
            Some(service) => AcoustIdRateLimiter::in_background(service.verify(path, artist, title)).await,
            None => VerificationResult::inconclusive(),
        }
    }

    async fn isrc_agrees(&self, question: &VerificationResult, isrc: &str) -> Option<bool> {
        self.service.get()?.isrc_agrees(question, isrc).await
    }

    async fn hear_preview(
        &self,
        path: &str,
        request: &PreviewRequest<'_>,
        question: &VerificationResult,
    ) -> PreviewOutcome {
        let (Some(service), Some(source)) = (self.service.get(), self.preview.get()) else {
            return PreviewOutcome::NotAsked;
        };
        service
            .hear_preview(source.as_ref(), path, request, question)
            .await
    }
}

/// How the sweep replaces a file that clearly is another song: the file's Navidrome id, and Wrong
/// song through the library action executor. Behind a seam so tests need no Navidrome.
#[async_trait]
pub trait SweepReplacer: Send + Sync {
    /// The Navidrome id of the library file at `path`, or None when Navidrome does not show it.
    async fn song_id(&self, artist: &str, title: &str, path: &str) -> Option<String>;

    /// Whether anyone starred or rated the song `song_id`: Some(false) only when everyone's marks
    /// were read and none is there, None when anyone's could not be read.
    async fn marked(&self, song_id: &str) -> Option<bool>;

    /// Runs the request to its end.
    async fn replace(&self, request: LibraryActionRequest) -> anyhow::Result<LibraryActionOutcome>;
}

/// The people besides the admin who can star or rate songs, whose marks the admin's own sign-in
/// does not see.
#[async_trait]
pub trait OtherListeners: Send + Sync {
    /// Their names, in lower case and each once, or None when they cannot be listed.
    async fn names(&self) -> Option<Vec<String>>;
}

/// Counts the other listeners as Navidrome's users other than Octo's admin identity, together
/// with every Family member, each name once.
pub struct NavidromeOtherListeners {
    admin: Arc<NavidromeAdmin>,
    family: Arc<FamilyStore>,
}

impl NavidromeOtherListeners {
    pub fn new(admin: Arc<NavidromeAdmin>, family: Arc<FamilyStore>) -> Self {
        NavidromeOtherListeners { admin, family }
    }

    /// The distinct names among `users` and `members` that are not `admin`, ignoring case and
    /// blank names, in lower case and sorted.
    pub fn others_besides(admin: &str, users: &[String], members: &[String]) -> Vec<String> {
        let admin = admin.trim().to_lowercase();
        users
            .iter()
            .chain(members)
            .map(|name| name.trim().to_lowercase())
            .filter(|name| !name.is_empty() && *name != admin)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

#[async_trait]
impl OtherListeners for NavidromeOtherListeners {
    async fn names(&self) -> Option<Vec<String>> {
        let users = match self.admin.list_users().await {
            Ok(users) => users,
            Err(e) => {
                warn!("Library review sweep could not list Navidrome's users: {e}");
                return None;
            }
        };
        let admin = self.admin.admin_user_name()?;
        let users: Vec<String> = users.iter().map(|user| user.user_name().to_string()).collect();
        let members: Vec<String> = self
            .family
            .members()
            .into_iter()
            .map(|member| member.username)
            .collect();
        Some(Self::others_besides(&admin, &users, &members))
    }
}

/// Whether a song is starred or rated, from the admin's marks (None when they could not be
/// read), the other listeners' names (None when they could not be listed) and the marks read
/// with each member's own sign-in, by name (None for a member whose marks could not be read).
/// Any mark found is Some(true); Some(false) needs the admin's marks and a read for every other
/// listener by name, all without a star or a rating; anything left unread is None.
pub fn marked_verdict(
    admin: Option<&SongMarks>,
    others: Option<&[String]>,
    reads: &[(String, Option<SongMarks>)],
) -> Option<bool> {
    let marked = |marks: &SongMarks| marks.starred || marks.rating > 0;
    if admin.is_some_and(marked) || reads.iter().any(|(_, marks)| marks.as_ref().is_some_and(marked)) {
        return Some(true);
    }
    admin?;
    let read = |name: &str| {
        reads.iter().any(|(member, marks)| {
            marks.is_some() && member.trim().to_lowercase() == name.trim().to_lowercase()
        })
    };
    others?.iter().all(|name| read(name)).then_some(false)
}

/// The real replacement: the resolver finds the song's id, Navidrome's song page says whether it
/// is starred or rated, and the executor runs Wrong song.
pub struct ExecutorSweepReplacer {
    resolver: Arc<NavidromeSongPathResolver>,
    executor: Arc<LibraryActionExecutor>,
    /// The admin's view of a song's marks.
    marks: Arc<NavidromePlaylistApi>,
    /// The marks other listeners left, each read with their own sign-in, when they can be read.
    members: Option<Arc<dyn MemberMarks>>,
    /// Who else could have marked a song.
    others: Arc<dyn OtherListeners>,
}

impl ExecutorSweepReplacer {
    pub fn new(
        resolver: Arc<NavidromeSongPathResolver>,
        executor: Arc<LibraryActionExecutor>,
        marks: Arc<NavidromePlaylistApi>,
        members: Option<Arc<dyn MemberMarks>>,
        others: Arc<dyn OtherListeners>,
    ) -> Self {
        ExecutorSweepReplacer {
            resolver,
            executor,
            marks,
            members,
            others,
        }
    }
}

#[async_trait]
impl SweepReplacer for ExecutorSweepReplacer {
    async fn song_id(&self, artist: &str, title: &str, path: &str) -> Option<String> {
        self.resolver.find_id_by_path(artist, title, path).await
    }

    async fn marked(&self, song_id: &str) -> Option<bool> {
        let admin = self.marks.song_marks(song_id).await;
        if admin
            .as_ref()
            .is_none_or(|marks| marks.starred || marks.rating > 0)
        {
            return marked_verdict(admin.as_ref(), Some(&[]), &[]);
        }
        let others = self.others.names().await;
        let reads = match (&self.members, &others) {
            (Some(members), Some(names)) if !names.is_empty() => members.marks_of(song_id).await,
            _ => Vec::new(),
        };
        marked_verdict(admin.as_ref(), others.as_deref(), &reads)
    }

    // Spawned, so a host stopping while the sweep waits never cuts the action short.
    async fn replace(&self, request: LibraryActionRequest) -> anyhow::Result<LibraryActionOutcome> {
        let executor = self.executor.clone();
        tokio::spawn(async move { executor.apply(request).await })
            .await
            .map_err(|e| anyhow::anyhow!("the replacement stopped: {e}"))?
    }
}

fn one() -> i32 {
    1
}

fn is_zero(value: &i32) -> bool {
    *value == 0
}

/// A re-check of Octo's own downloads: one pass over them at the re-check rate, then it stops.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RecheckState {
    #[serde(default)]
    pub running: bool,
    /// Library-relative path of the last download dealt with; empty at the start.
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub cursor: String,
    #[serde(default)]
    pub total: i32,
    /// Downloads checked so far.
    #[serde(default)]
    pub checked: i32,
    /// Downloads asked about in Review.
    #[serde(default)]
    pub found: i32,
    /// Downloads replaced through Wrong song.
    #[serde(default)]
    pub replaced: i32,
    /// `Soulseek:MinMatchScore` when the re-check started.
    #[serde(default)]
    pub threshold: i32,
    #[serde(default, with = "datetime::utc_option")]
    pub started_utc: Option<DateTime<Utc>>,
    #[serde(default, with = "datetime::utc_option")]
    pub finished_utc: Option<DateTime<Utc>>,
}

/// The replacements the sweep made by itself on one UTC day, and the ones that failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ReplacedDay {
    /// Midnight UTC of the day counted.
    #[serde(default = "datetime::min_value", with = "datetime::utc")]
    pub day_utc: DateTime<Utc>,
    #[serde(default)]
    pub replaced: i32,
    #[serde(default)]
    pub failed: i32,
}

impl ReplacedDay {
    /// Midnight UTC of the day `now` falls in.
    pub fn day_of(now: DateTime<Utc>) -> DateTime<Utc> {
        now.date_naive().and_hms_opt(0, 0, 0).expect("midnight").and_utc()
    }
}

/// What the sweep has replaced by itself today, and how many replacements failed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Today {
    pub replaced: i32,
    pub failed: i32,
}

/// What the sweep read of a file: the song its tags name, whether its artist and title both came
/// from real tags (a title taken from the file name never counts), and the ISRC it is tagged
/// with.
#[derive(Debug, Clone, Default)]
pub struct SweptFile {
    pub song: Song,
    pub tagged: bool,
    pub isrc: Option<String>,
}

/// Whether a file the sweep would ask about is replaced instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoReplace {
    Replace,
    Ask,
}

/// What the song's YouTube preview said of a file it sounds nothing like, for deciding whether to
/// replace it: the share of differing bits, and whether the preview is a YouTube Music row from the
/// file's own album.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PreviewWitness {
    pub bit_error: f64,
    pub album_agrees: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ReviewSweepState {
    #[serde(default)]
    pub paused: bool,
    /// Library-relative path of the last file dealt with this pass; empty at its start.
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub cursor: String,
    /// Library-relative path to the size and modified time it had when checked. Relative, so a
    /// mount moving does not forget everything; stamped, so a replaced file is checked again.
    #[serde(default, with = "super::state_dictionary")]
    pub checked: DotnetDictionary<String>,
    #[serde(default = "one")]
    pub pass: i32,
    #[serde(default)]
    pub total: i32,
    #[serde(default)]
    pub found: i32,
    #[serde(default)]
    pub fine: i32,
    #[serde(default)]
    pub undecodable: i32,
    #[serde(default, with = "datetime::utc_option")]
    pub last_checked_utc: Option<DateTime<Utc>>,
    #[serde(default, with = "datetime::utc_option")]
    pub pass_finished_utc: Option<DateTime<Utc>>,
    #[serde(default = "datetime::min_value", with = "datetime::utc")]
    pub next_pass_utc: DateTime<Utc>,
    /// Files this pass replaced through Wrong song. Left out of the file while 0.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub replaced: i32,
    /// The last re-check of Octo's downloads. Left out of the file until one is started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recheck: Option<RecheckState>,
    /// The sweep's own replacements on the day it last made or failed one. Left out of the file
    /// until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaced_today: Option<ReplacedDay>,
}

impl ReviewSweepState {
    /// Today's replacements and failures, by `now`'s UTC day.
    pub fn today(&self, now: DateTime<Utc>) -> Today {
        match &self.replaced_today {
            Some(day) if day.day_utc == ReplacedDay::day_of(now) => Today {
                replaced: day.replaced,
                failed: day.failed,
            },
            _ => Today::default(),
        }
    }

    /// Counts one replacement, or one failed replacement, on `now`'s UTC day.
    fn count_today(&mut self, now: DateTime<Utc>, replaced: bool) {
        let day_utc = ReplacedDay::day_of(now);
        let day = self.replaced_today.get_or_insert(ReplacedDay {
            day_utc,
            replaced: 0,
            failed: 0,
        });
        if day.day_utc != day_utc {
            *day = ReplacedDay {
                day_utc,
                replaced: 0,
                failed: 0,
            };
        }
        if replaced {
            day.replaced += 1;
        } else {
            day.failed += 1;
        }
    }
}

impl Default for ReviewSweepState {
    fn default() -> Self {
        ReviewSweepState {
            paused: false,
            cursor: String::new(),
            checked: DotnetDictionary::new(),
            pass: 1,
            total: 0,
            found: 0,
            fine: 0,
            undecodable: 0,
            last_checked_utc: None,
            pass_finished_utc: None,
            next_pass_utc: datetime::min_value(),
            replaced: 0,
            recheck: None,
            replaced_today: None,
        }
    }
}

struct StoreInner {
    state: ReviewSweepState,
    dirty: bool,
}

/// A hook run between taking a snapshot and writing it (a test seam).
pub type BeforeWrite = Arc<dyn Fn() + Send + Sync>;

/// review-sweep.json. [`ReviewSweepState::checked`] holds a line per library file, so the file
/// is written at most every [`ReviewSweepStore::FLUSH_INTERVAL`], not on every change: at 50,000
/// files a write per song would rewrite hundreds of megabytes an hour.
///
/// [`ReviewSweepStore::run_flusher`] flushes every 5 s (a worker, which flushes once more on
/// shutdown), and `Drop` flushes what is left.
pub struct ReviewSweepStore {
    path: Option<PathBuf>,
    clock: Clock,
    inner: Mutex<StoreInner>,
    flush_lock: Mutex<()>,
    /// When the state last went to disk; None before the first write.
    last_write: Mutex<Option<DateTime<Utc>>>,
    before_write: Mutex<Option<BeforeWrite>>,
}

impl ReviewSweepStore {
    pub const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

    /// A store kept in `path` (None keeps it in memory), on the system clock.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self::with_clock(path, Clock::system())
    }

    /// A store kept in `path` on the given clock. A file that cannot be read is set aside as
    /// `<file>.corrupt-<ticks>` and the sweep starts over.
    pub fn with_clock(path: Option<PathBuf>, clock: Clock) -> Self {
        let path = path.filter(|p| !dotnet::is_blank(&p.to_string_lossy()));
        let mut state = ReviewSweepState::default();
        if let Some(path) = &path
            && path.exists()
        {
            match Self::load(path) {
                Ok(loaded) => state = loaded,
                Err(e) => {
                    // Kept aside: losing it only means checking the library again, but it
                    // should be seen.
                    warn!("review sweep state could not be read ({e}); starting over");
                    set_aside(path);
                }
            }
        }
        ReviewSweepStore {
            path,
            clock,
            inner: Mutex::new(StoreInner { state, dirty: false }),
            flush_lock: Mutex::new(()),
            last_write: Mutex::new(None),
            before_write: Mutex::new(None),
        }
    }

    fn load(path: &Path) -> anyhow::Result<ReviewSweepState> {
        let text = state_file::read_all_text(path)?;
        Ok(serde_json::from_str::<Option<ReviewSweepState>>(&text)?.unwrap_or_default())
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Test seam: runs between taking a snapshot and writing it.
    pub fn set_before_write(&self, hook: Option<BeforeWrite>) {
        *self.before_write.lock() = hook;
    }

    pub fn read<T>(&self, read: impl FnOnce(&ReviewSweepState) -> T) -> T {
        read(&self.inner.lock().state)
    }

    /// Changes the state in memory. It reaches the disk now if nothing was written in the last
    /// few seconds, otherwise on the next timer tick or [`ReviewSweepStore::flush`].
    pub fn update(&self, change: impl FnOnce(&mut ReviewSweepState)) {
        {
            let mut inner = self.inner.lock();
            change(&mut inner.state);
            inner.dirty = true;
        }
        if self.path.is_none() {
            return;
        }
        let now = self.clock.now();
        let due = self.last_write.lock().is_none_or(|last| {
            now - last >= TimeDelta::from_std(Self::FLUSH_INTERVAL).expect("five seconds")
        });
        if due {
            self.flush();
        }
    }

    /// Writes the state if it changed. The snapshot is taken inside the same lock as the write,
    /// so an older snapshot can never land after a newer one (a Pause overwritten by the tick
    /// before it).
    pub fn flush(&self) -> bool {
        let Some(path) = self.path.as_deref() else {
            return true;
        };
        let _flushing = self.flush_lock.lock();
        let json = {
            let mut inner = self.inner.lock();
            if !inner.dirty {
                return true;
            }
            inner.dirty = false;
            octo_core::json::to_string(&inner.state)
        };
        let hook = self.before_write.lock().clone();
        if let Some(hook) = hook {
            hook();
        }
        *self.last_write.lock() = Some(self.clock.now());
        match state_file::save_atomic(path, &json) {
            Ok(()) => true,
            Err(e) => {
                self.inner.lock().dirty = true;
                warn!("review sweep state could not be written: {e}");
                false
            }
        }
    }

    /// The 5-second flush timer, which picks up a change made just after a write that no later
    /// change may come to flush, and the last flush on shutdown, as a worker.
    pub async fn run_flusher(self: Arc<Self>, token: CancellationToken) -> anyhow::Result<()> {
        let store = self.clone();
        state_file::flush_every(
            Self::FLUSH_INTERVAL,
            token,
            Arc::new(move || {
                store.flush();
            }),
        )
        .await
    }
}

impl Drop for ReviewSweepStore {
    /// The last flush.
    fn drop(&mut self) {
        self.flush();
    }
}

/// What the dashboard shows of the sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSweepStatus {
    pub state: String,
    pub reason: Option<String>,
    pub paused: bool,
    pub position: usize,
    pub total: i32,
    pub pass: i32,
    pub found: i32,
    pub open: usize,
    pub fine: i32,
    pub undecodable: i32,
    pub keeper: Option<String>,
    pub per_hour: i32,
    pub last_checked_utc: Option<DateTime<Utc>>,
    pub pass_finished_utc: Option<DateTime<Utc>>,
    /// Files this pass replaced through Wrong song.
    pub replaced: i32,
    /// The last re-check of Octo's downloads, with how far it has got; None before the first.
    pub recheck: Option<RecheckStatus>,
    /// Whether replacing is switched on, the day's cap, and what today brought.
    pub auto_replace: bool,
    pub auto_replace_preview: bool,
    pub auto_replace_per_day: i32,
    pub today: Today,
    /// `Soulseek:MinMatchScore` differs from the one the last re-check ran with (the default
    /// when there was none).
    pub threshold_changed: bool,
}

/// How far a re-check of Octo's downloads has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecheckStatus {
    pub running: bool,
    pub position: usize,
    pub total: i32,
    pub checked: i32,
    pub found: i32,
    pub replaced: i32,
    pub threshold: i32,
    pub started_utc: Option<DateTime<Utc>>,
    pub finished_utc: Option<DateTime<Utc>>,
}

/// How one file's check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Fine,
    Ask,
    Undecodable,
    NotFingerprinted,
    LookupFailed,
}

/// What came of replacing a file through Wrong song.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Replacing {
    /// The replacement went in.
    Done,
    /// It did not, so the file stays a question.
    Declined,
    /// Soulseek is out and the replacement may come from nowhere else: the song stays unchecked.
    Waits,
}

/// The audio files the sweep walks: the same extensions the cover upgrade walks.
const AUDIO_EXTENSIONS: [&str; 14] = crate::services::cover_art::CoverUpgradeWorker::AUDIO_EXTENSIONS;

/// What the sweep is built from, apart from the clock.
pub struct LibraryReviewSweepParts {
    pub store: Arc<ReviewSweepStore>,
    pub notices: Arc<NoticeQueue>,
    pub verifier: Arc<dyn IReviewSweepVerifier>,
    pub activity: Arc<dyn IAcquisitionActivity>,
    pub library: Arc<dyn ILocalLibraryService>,
    /// The library action and Subsonic settings, read at use.
    pub settings: Arc<SettingsStore>,
    /// The resolver's root, the same one review actions resolve inside.
    pub music_root: Box<dyn Fn() -> String + Send + Sync>,
}

/// The two walks a tick can take: the library pass, or a re-check of Octo's own downloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    Library,
    Recheck,
}

/// What one tick works with: its lane, the generation it started in, who is asked, its rate,
/// the music folder and the time.
#[derive(Clone, Copy)]
struct Step<'a> {
    lane: Lane,
    generation: i32,
    keeper: &'a str,
    interval: Duration,
    root: &'a str,
    now: DateTime<Utc>,
}

/// The file a tick picked, its stamp, the last file passed over on the way, and where the cursor
/// was.
struct Pick {
    target: Option<String>,
    stamp: Option<String>,
    passed: String,
    cursor: String,
}

/// Moves the cursor of `lane`.
fn set_cursor(s: &mut ReviewSweepState, lane: Lane, cursor: &str) {
    match lane {
        Lane::Library => s.cursor = cursor.to_string(),
        Lane::Recheck => {
            if let Some(recheck) = s.recheck.as_mut() {
                recheck.cursor = cursor.to_string();
            }
        }
    }
}

/// Asks about music that was already in the library (#72), the way Review asks about a download
/// (#47): a few files an hour, only while nothing is downloading, only of whoever keeps the
/// library, and never more than [`LibraryReviewSweepWorker::MAX_OPEN_QUESTIONS`] open at once.
/// It acts on a file only with `ReviewSweepAutoReplace` on, and then only through Wrong song on a
/// file AcoustID clearly hears as another song (see [`LibraryReviewSweepWorker::decide`]);
/// everything else is a question.
pub struct LibraryReviewSweepWorker {
    store: Arc<ReviewSweepStore>,
    notices: Arc<NoticeQueue>,
    verifier: Arc<dyn IReviewSweepVerifier>,
    activity: Arc<dyn IAcquisitionActivity>,
    library: Arc<dyn ILocalLibraryService>,
    settings: Arc<SettingsStore>,
    music_root: Box<dyn Fn() -> String + Send + Sync>,
    clock: Clock,

    files: Mutex<Option<Arc<Vec<String>>>>,
    octo_files: Mutex<Arc<HashSet<String>>>,
    /// Octo's own downloads, library-relative, while a re-check walks them.
    recheck_files: Mutex<Option<Arc<Vec<String>>>>,
    generation: AtomicI32,
    lookup_failures: AtomicI32,
    /// The AcoustID verdict of the song whose preview the shim did not send, kept for its retries:
    /// library-relative path, stamp, verdict.
    retry_verdict: Mutex<Option<(String, String, VerificationResult)>>,
    unfingerprinted: AtomicI32,
    hold: Mutex<(String, Option<String>)>,
    /// With Family on, the sweep keeps to the shared library: a member's own songs are never
    /// asked about.
    scope: Option<ScopeResolver>,
    /// Wrong song for a file that clearly is another song; None replaces nothing.
    replacer: Option<Arc<dyn SweepReplacer>>,
}

impl LibraryReviewSweepWorker {
    pub const MAX_OPEN_QUESTIONS: usize = 50;
    pub const MAX_LOOKUP_FAILURES_PER_FILE: i32 = 3;
    pub const MAX_UNFINGERPRINTED_IN_A_ROW: i32 = 5;
    pub const IDLE_CHECK: Duration = Duration::from_secs(60);
    pub const BUSY_CHECK: Duration = Duration::from_secs(30);
    /// After a full pass, how long before the library is walked again for new or changed files.
    pub const PASS_INTERVAL: TimeDelta = TimeDelta::hours(6);
    /// A re-check of Octo's downloads goes at least this many files an hour, even with the
    /// sweep's own rate at 0.
    pub const RECHECK_PER_HOUR: i32 = 60;
    /// After this many failed replacements in a UTC day, the rest of the day's are asked about.
    pub const MAX_FAILED_REPLACEMENTS_PER_DAY: i32 = 3;
    /// The share of differing bits from which a song its YouTube preview says is another one is
    /// replaced instead of asked about. Right files against a believed preview measured at most
    /// 0.11, an official video with re-mastered sound 0.37; another song 0.41 to 0.47 and noise 0.38
    /// to 0.48. Between the preview's own line and this, it stays a question.
    pub const PREVIEW_REPLACE_FROM: f64 = 0.42;

    const DONE: &'static str = "Every song has been checked. New or changed ones are looked for later.";
    const RECHECKING: &'static str = "Re-checking Octo's downloads.";
    const RECHECKED: &'static str = "Re-checked Octo's downloads.";

    pub fn new(parts: LibraryReviewSweepParts, clock: Clock) -> Self {
        LibraryReviewSweepWorker {
            store: parts.store,
            notices: parts.notices,
            verifier: parts.verifier,
            activity: parts.activity,
            library: parts.library,
            settings: parts.settings,
            music_root: parts.music_root,
            clock,
            files: Mutex::new(None),
            octo_files: Mutex::new(Arc::new(HashSet::new())),
            recheck_files: Mutex::new(None),
            generation: AtomicI32::new(0),
            lookup_failures: AtomicI32::new(0),
            retry_verdict: Mutex::new(None),
            unfingerprinted: AtomicI32::new(0),
            hold: Mutex::new(("Off".to_string(), None)),
            scope: None,
            replacer: None,
        }
    }

    /// Keeps the sweep to the shared library while Family is on.
    pub fn with_library_scope(mut self, scope: ScopeResolver) -> Self {
        self.scope = Some(scope);
        self
    }

    /// How a file that clearly is another song is replaced, once `ReviewSweepAutoReplace` is on.
    pub fn with_replacer(mut self, replacer: Arc<dyn SweepReplacer>) -> Self {
        self.replacer = Some(replacer);
        self
    }

    /// The scopes one pass of the sweep asks: None when every file may be asked about, else
    /// the shared library's while members have libraries of their own (Family on, or turned
    /// off with their libraries still there).
    fn shared_view(&self) -> Option<ScopeView> {
        self.scope
            .as_ref()
            .filter(|scope| scope.keeps_shared())
            .map(ScopeResolver::shared_view)
    }

    /// Whether the sweep may ask about the file at `full`, by the pass's `view`.
    fn in_shared(view: Option<&ScopeView>, full: &str) -> bool {
        view.is_none_or(|view| view.allows(&LibraryScope::Shared, None, Some(full)))
    }

    pub fn store(&self) -> &Arc<ReviewSweepStore> {
        &self.store
    }

    /// The worker loop until the host stops, then a last flush.
    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        // Never alongside startup, which already works the disk and Navidrome.
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(120)) => {}
            _ = stopping.cancelled() => {
                self.stop();
                return Ok(());
            }
        }
        while !stopping.is_cancelled() {
            // Per-tick catch is mandatory: a tick that panics must never stop the worker.
            let tick = tokio::select! {
                tick = std::panic::AssertUnwindSafe(self.tick()).catch_unwind() => tick,
                _ = stopping.cancelled() => break,
            };
            let wait = tick.unwrap_or_else(|_| {
                error!("Library review sweep failed");
                Self::IDLE_CHECK
            });
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = stopping.cancelled() => break,
            }
        }
        self.stop();
        Ok(())
    }

    /// The store writes every few seconds at most; whatever changed since goes down now.
    fn stop(&self) {
        self.store.flush();
    }

    pub async fn tick(&self) -> Duration {
        let current = self.settings.current();
        let settings = current.library_actions.clone();
        let rechecking = self.store.read(|s| s.recheck.as_ref().is_some_and(|r| r.running));
        let own_rate = settings.effective_review_sweep_per_hour();
        let per_hour = if rechecking {
            own_rate.max(Self::RECHECK_PER_HOUR)
        } else {
            own_rate
        };
        if !settings.enabled || !settings.review_enabled || per_hour == 0 {
            return self.hold(
                "Off",
                Some("Set how many songs an hour to check, with library actions and Review on."),
                Self::IDLE_CHECK,
            );
        }
        if self.store.read(|s| s.paused) {
            return self.hold("Paused", Some("Paused from the dashboard."), Self::IDLE_CHECK);
        }
        if !self.verifier.is_ready() {
            return self.hold(
                "Paused",
                Some("AcoustID is not set up: turn on download verification and add a key."),
                Self::IDLE_CHECK,
            );
        }
        let admin = current.subsonic.admin_username.clone();
        let Some(keeper) = Self::keeper(&settings, admin.as_deref()) else {
            return self.hold(
                "Paused",
                Some("Nobody is on the library actions allowlist to ask."),
                Self::IDLE_CHECK,
            );
        };
        let open = self.notices.open_count(NoticeOrigin::LibrarySweep);
        if open >= Self::MAX_OPEN_QUESTIONS {
            let reason =
                format!("{open} library questions are waiting in Review. Answer some and it carries on.");
            return self.hold("Paused", Some(&reason), Self::IDLE_CHECK);
        }
        if self.activity.is_busy() {
            return self.hold(
                "Waiting",
                Some("Waiting for a download to finish."),
                Self::BUSY_CHECK,
            );
        }

        let now = self.clock.now();
        let generation = self.generation.load(Ordering::SeqCst);
        let root = (self.music_root)();
        let step = Step {
            lane: Lane::Library,
            generation,
            keeper: &keeper,
            interval: Duration::from_secs_f64(3600.0 / f64::from(per_hour)),
            root: &root,
            now,
        };
        if rechecking {
            return self
                .recheck_tick(&Step {
                    lane: Lane::Recheck,
                    ..step
                })
                .await;
        }
        if self.files.lock().is_none() && self.store.read(|s| s.cursor.is_empty() && s.next_pass_utc > now) {
            return self.hold("Done", Some(Self::DONE), Self::IDLE_CHECK);
        }

        if self.files.lock().is_none() {
            if !Path::new(&root).is_dir() {
                return self.hold(
                    "Paused",
                    Some("The music folder cannot be read."),
                    Self::IDLE_CHECK,
                );
            }
            let trash = library_walk::trash_folder(Path::new(&root), &settings);
            let listed = Arc::new(Self::enumerate(&root, Some(&trash)));
            *self.files.lock() = Some(listed.clone());
            let octo_files: HashSet<String> = if settings.review_sweep_octo_downloads {
                HashSet::new()
            } else {
                self.library
                    .get_mappings()
                    .await
                    .into_iter()
                    .map(|mapping| mapping.local_path)
                    .filter(|path| !path.is_empty())
                    .map(|path| get_full_path(&path))
                    .collect()
            };
            *self.octo_files.lock() = Arc::new(octo_files);
            // Start over was pressed while the library was being listed. Keeping this listing
            // would leave the pass with a total of 0, so the next tick lists it again.
            if generation != self.generation.load(Ordering::SeqCst) {
                *self.files.lock() = None;
                return Duration::ZERO;
            }
            let total = i32::try_from(listed.len()).unwrap_or(i32::MAX);
            self.record(generation, |s| s.total = total);
        }

        let Some(files) = self.files.lock().clone() else {
            // Start over was pressed since: list again next tick.
            return Duration::ZERO;
        };
        let octo_files = self.octo_files.lock().clone();
        let cursor = self.store.read(|s| s.cursor.clone());
        let pick = self.next_target(&files, &root, cursor, &octo_files);

        let Some(target) = pick.target.clone() else {
            let present: HashSet<&str> = files.iter().map(String::as_str).collect();
            self.record(generation, |s| {
                let gone: Vec<String> = s
                    .checked
                    .keys()
                    .filter(|key| !present.contains(key.as_str()))
                    .cloned()
                    .collect();
                for key in gone {
                    s.checked.remove(&key);
                }
                s.cursor = String::new();
                s.pass += 1;
                s.pass_finished_utc = Some(now);
                s.next_pass_utc = now + Self::PASS_INTERVAL;
            });
            *self.files.lock() = None;
            info!(
                "Library review sweep finished a pass over {} file(s)",
                files.len()
            );
            return self.hold("Done", Some(Self::DONE), Self::IDLE_CHECK);
        };
        self.check(&step, &target, pick).await
    }

    /// One tick of a re-check of Octo's downloads: the next download not yet checked again, or
    /// the end of the re-check.
    async fn recheck_tick(&self, step: &Step<'_>) -> Duration {
        if self.recheck_files.lock().is_none() {
            if !Path::new(step.root).is_dir() {
                return self.hold(
                    "Paused",
                    Some("The music folder cannot be read."),
                    Self::IDLE_CHECK,
                );
            }
            let listed = self.octo_downloads(step.root).await;
            if step.generation != self.generation.load(Ordering::SeqCst) {
                return Duration::ZERO;
            }
            let total = i32::try_from(listed.len()).unwrap_or(i32::MAX);
            *self.recheck_files.lock() = Some(Arc::new(listed));
            self.record(step.generation, |s| {
                if let Some(recheck) = s.recheck.as_mut() {
                    recheck.total = total;
                }
            });
        }
        let Some(files) = self.recheck_files.lock().clone() else {
            return Duration::ZERO;
        };
        let cursor = self.store.read(|s| {
            s.recheck
                .as_ref()
                .map(|recheck| recheck.cursor.clone())
                .unwrap_or_default()
        });
        let pick = self.next_target(&files, step.root, cursor, &HashSet::new());
        let Some(target) = pick.target.clone() else {
            let now = step.now;
            self.record(step.generation, |s| {
                if let Some(recheck) = s.recheck.as_mut() {
                    recheck.running = false;
                    recheck.cursor = String::new();
                    recheck.finished_utc = Some(now);
                }
            });
            *self.recheck_files.lock() = None;
            info!(
                "Library review sweep re-checked {} of Octo's download(s)",
                files.len()
            );
            return self.hold("Done", Some(Self::RECHECKED), Self::IDLE_CHECK);
        };
        self.check(step, &target, pick).await
    }

    /// The next file after `cursor` to check, and the last one passed over on the way. A file
    /// that is gone, outside the shared library, in `skip`, already asked about, or checked
    /// since it last changed is passed over.
    fn next_target(&self, files: &[String], root: &str, cursor: String, skip: &HashSet<String>) -> Pick {
        let scope_view = self.shared_view();
        let reviewed = self.notices.reviewed_paths();
        let mut pick = Pick {
            target: None,
            stamp: None,
            passed: cursor.clone(),
            cursor,
        };
        let mut i = first_after(files, &pick.cursor);
        while i < files.len() && pick.target.is_none() {
            let full = full_path(root, &files[i]);
            let file_stamp = std::fs::metadata(&full)
                .ok()
                .filter(|m| m.is_file())
                .map(|m| format!("{}:{}", m.len(), modified_ticks(&m)));
            // One entry looked up at a time: copying the whole map each tick costs a line per
            // library file.
            let relative = &files[i];
            let passed_over = match &file_stamp {
                None => true,
                Some(file_stamp) => {
                    !Self::in_shared(scope_view.as_ref(), &full)
                        || skip.contains(&full)
                        || reviewed.contains(&full)
                        || self
                            .store
                            .read(|s| s.checked.get(relative).is_some_and(|seen| seen == file_stamp))
                }
            };
            if passed_over {
                pick.passed = files[i].clone();
            } else {
                pick.target = Some(files[i].clone());
                pick.stamp = file_stamp;
            }
            i += 1;
        }
        pick
    }

    /// Checks one file, then asks about it, replaces it, or counts it fine, on `step`'s lane.
    async fn check(&self, step: &Step<'_>, target: &str, pick: Pick) -> Duration {
        let (lane, generation, interval) = (step.lane, step.generation, step.interval);
        let running = match lane {
            Lane::Library => None,
            Lane::Recheck => Some(Self::RECHECKING),
        };
        let path = full_path(step.root, target);
        let read = path.clone();
        let Ok(file) = tokio::task::spawn_blocking(move || Self::read_tags(&read)).await else {
            return interval;
        };
        let song = &file.song;
        let stamp_text = pick.stamp.clone().unwrap_or_default();
        // The verdict kept for a song whose preview the shim did not send is its own, not asked
        // for again.
        let kept = {
            let mut slot = self.retry_verdict.lock();
            match slot.take() {
                Some((kept_path, kept_stamp, kept)) if kept_path == target && kept_stamp == stamp_text => {
                    Some(kept)
                }
                _ => None,
            }
        };
        let verdict = match kept {
            Some(kept) => kept,
            None => {
                self.verifier
                    .verify(&path, Some(&song.artist), Some(&song.title))
                    .await
            }
        };
        if generation != self.generation.load(Ordering::SeqCst) {
            return interval;
        }

        let acoustid_verdict = verdict.clone();
        let (outcome, question) = Self::classify(verdict);
        // A sound AcoustID has never heard is not left to a person while its song's YouTube
        // preview can say: a YouTube Music row that sounds alike settles it, whatever album it is
        // filed under (a single, another release or none), unless one of the two is live; one
        // that sounds nothing like it is a better question, and anything else stays the question
        // it was. A link only one person's submissions make is put to the
        // preview the same way, but one that sounds nothing like it is only ever asked about.
        let mut waiting_for = "AcoustID";
        let mut witness: Option<PreviewWitness> = None;
        let (outcome, question) = if outcome == Outcome::Ask
            && matches!(
                question.reason,
                InconclusiveReason::NoEntry | InconclusiveReason::LoneLink
            )
            && file.tagged
        {
            let request = PreviewRequest {
                artist: &song.artist,
                title: &song.title,
                seconds: song.duration,
                video_id: None,
                background: true,
            };
            let heard = self.verifier.hear_preview(&path, &request, &question).await;
            if generation != self.generation.load(Ordering::SeqCst) {
                return interval;
            }
            match heard {
                PreviewOutcome::Compared {
                    heard: Heard::Match,
                    trusted: true,
                    preview: facts,
                    ..
                } if Self::preview_album_settles(facts.album.as_deref(), &song.album) => {
                    info!(
                        "Library review sweep: '{} - {}' is {}, and it sounds like its YouTube preview",
                        song.artist,
                        song.title,
                        if question.reason == InconclusiveReason::LoneLink {
                            "tied to its song by one person's AcoustID submissions only"
                        } else {
                            "a sound AcoustID has never heard"
                        }
                    );
                    (Outcome::Fine, question)
                }
                PreviewOutcome::Compared {
                    heard: Heard::Mismatch,
                    comparison,
                    ..
                } if question.reason == InconclusiveReason::LoneLink => {
                    // One person's link against a preview that disagrees: two single words, so a
                    // question, and never a replacement.
                    (
                        Outcome::Ask,
                        VerificationResult {
                            evidence: Some(format!(
                                "{}% of its bits differ from its YouTube preview",
                                (comparison.bit_error * 100.0).round()
                            )),
                            ..question
                        },
                    )
                }
                PreviewOutcome::Compared {
                    heard: Heard::Mismatch,
                    comparison,
                    preview: facts,
                    ..
                } => {
                    // The re-check of Octo's own downloads only ever asks: Octo picked each of them,
                    // with the catalog's length and pinned video this check does not have.
                    if lane == Lane::Library {
                        witness = Some(PreviewWitness {
                            bit_error: comparison.bit_error,
                            album_agrees: Self::preview_album_agrees(facts.album.as_deref(), &song.album),
                        });
                    }
                    (
                        Outcome::Ask,
                        VerificationResult {
                            reason: InconclusiveReason::DiffersFromYouTube,
                            evidence: Some(format!(
                                "{}% of its bits differ",
                                (comparison.bit_error * 100.0).round()
                            )),
                            ..question
                        },
                    )
                }
                // The shim found the song's video but did not send it: tried again, and asked about
                // as before once it failed as often as an AcoustID lookup may.
                // Plays come first: a busy shim, or one that cannot be asked, is waited out
                // without spending one of the song's tries.
                waited @ (PreviewOutcome::Busy | PreviewOutcome::Unreachable) => {
                    if pick.passed != pick.cursor {
                        self.record(generation, |s| set_cursor(s, lane, &pick.passed));
                    }
                    *self.retry_verdict.lock() =
                        Some((target.to_string(), stamp_text.clone(), acoustid_verdict));
                    let reason = if waited == PreviewOutcome::Busy {
                        "YouTube is busy with plays; trying that song again."
                    } else {
                        "The YouTube shim is not answering; trying that song again."
                    };
                    return self.hold("Waiting", Some(reason), interval);
                }
                // The file could not be read for the comparison, and a video found but not sent
                // is no better: either is tried three times, and asked about as before.
                tried @ (PreviewOutcome::NotSent(_) | PreviewOutcome::FileUnreadable) => {
                    debug!(
                        "Library review sweep: no preview for '{} - {}' yet: {tried:?}",
                        song.artist, song.title
                    );
                    if self.lookup_failures.load(Ordering::SeqCst) + 1 < Self::MAX_LOOKUP_FAILURES_PER_FILE {
                        waiting_for = "The YouTube shim";
                        *self.retry_verdict.lock() =
                            Some((target.to_string(), stamp_text.clone(), acoustid_verdict));
                        (Outcome::LookupFailed, question)
                    } else {
                        (outcome, question)
                    }
                }
                PreviewOutcome::Unavailable(why) => {
                    debug!(
                        "Library review sweep: no preview check for '{} - {}': {why}",
                        song.artist, song.title
                    );
                    (outcome, question)
                }
                _ => (outcome, question),
            }
        } else {
            (outcome, question)
        };
        match outcome {
            Outcome::LookupFailed
                if self.lookup_failures.fetch_add(1, Ordering::SeqCst) + 1
                    < Self::MAX_LOOKUP_FAILURES_PER_FILE =>
            {
                // The songs skipped on the way here are kept; every other path moves the cursor
                // to the target itself, which is past them, so a tick writes once.
                if pick.passed != pick.cursor {
                    self.record(generation, |s| set_cursor(s, lane, &pick.passed));
                }
                return self.hold(
                    "Waiting",
                    Some(&format!("{waiting_for} did not answer; trying that song again.")),
                    interval,
                );
            }
            Outcome::LookupFailed => {
                self.lookup_failures.store(0, Ordering::SeqCst);
                self.record(generation, |s| set_cursor(s, lane, target));
                return self.hold("Running", running, interval);
            }
            Outcome::NotFingerprinted => {
                self.record(generation, |s| set_cursor(s, lane, target));
                if self.unfingerprinted.fetch_add(1, Ordering::SeqCst) + 1
                    < Self::MAX_UNFINGERPRINTED_IN_A_ROW
                {
                    return self.hold("Running", running, interval);
                }
                self.unfingerprinted.store(0, Ordering::SeqCst);
                return self.hold(
                    "Paused",
                    Some("fpcalc could not read five songs in a row. Is the music folder readable?"),
                    Duration::from_secs(3600),
                );
            }
            _ => {}
        }

        self.lookup_failures.store(0, Ordering::SeqCst);
        self.unfingerprinted.store(0, Ordering::SeqCst);
        let replaced = if outcome == Outcome::Ask
            // A file someone was asked about while this check looked at it stays theirs to answer.
            && !self.notices.reviewed_paths().contains(&path)
            && Self::decide(
                &question,
                &file,
                &self.settings.current(),
                step.keeper,
                self.store.read(|s| s.today(self.clock.now())),
                witness,
            ) == AutoReplace::Replace
        {
            match self.replace(step.keeper, &path, &file, &question).await {
                Replacing::Done => true,
                Replacing::Declined => false,
                // Soulseek is out: the song stays unchecked, no try or breaker count is spent, and
                // the sweep comes back to it.
                Replacing::Waits => {
                    if pick.passed != pick.cursor {
                        self.record(generation, |s| set_cursor(s, lane, &pick.passed));
                    }
                    return self.hold(
                        "Waiting",
                        Some("Soulseek is out; trying that song again."),
                        interval,
                    );
                }
            }
        } else {
            false
        };
        let asked = !replaced
            && outcome == Outcome::Ask
            && self
                .notices
                .add_review_from(step.keeper, &path, song, &question, NoticeOrigin::LibrarySweep);
        if asked {
            info!(
                "Asking {} about '{} - {}' from the library: {}",
                step.keeper,
                song.artist,
                song.title,
                question.reason.name()
            );
        }
        let stamp = pick.stamp.unwrap_or_default();
        let now = step.now;
        self.record(generation, |s| {
            set_cursor(s, lane, target);
            s.checked.set(target.to_string(), stamp);
            s.last_checked_utc = Some(now);
            match lane {
                Lane::Library if replaced => {
                    s.found += 1;
                    s.replaced += 1;
                }
                Lane::Library if asked => s.found += 1,
                Lane::Library if outcome == Outcome::Undecodable => s.undecodable += 1,
                Lane::Library => s.fine += 1,
                Lane::Recheck => {
                    if let Some(recheck) = s.recheck.as_mut() {
                        recheck.checked += 1;
                        if replaced {
                            recheck.replaced += 1;
                        } else if asked {
                            recheck.found += 1;
                        }
                    }
                }
            }
        });
        self.hold("Running", running, interval)
    }

    /// Wrong song on the file at `path`, as the keeper. True once the replacement went in, which
    /// leaves the keeper a note in Review; false leaves the file a question. A file whose ISRC
    /// tag the heard recording also carries, or one that cannot be checked, is asked about, and
    /// so is a song anyone starred or rated, or one whose marks cannot be read. A replacement
    /// that failed counts toward the day's breaker, except one that found Soulseek out, which
    /// waits instead.
    async fn replace(
        &self,
        keeper: &str,
        path: &str,
        file: &SweptFile,
        question: &VerificationResult,
    ) -> Replacing {
        let Some(replacer) = self.replacer.clone() else {
            return Replacing::Declined;
        };
        let song = &file.song;
        if let Some(isrc) = file.isrc.as_deref() {
            let why = match self.verifier.isrc_agrees(question, isrc).await {
                Some(false) => None,
                Some(true) => Some("the heard recording carries its ISRC too"),
                None => Some("its ISRC could not be checked against the heard recording"),
            };
            if let Some(why) = why {
                info!(
                    "Library review sweep: '{} - {}' is tagged {isrc} and {why}, so it is asked about instead of replaced",
                    song.artist, song.title
                );
                return Replacing::Declined;
            }
        }
        let Some(id) = replacer.song_id(&song.artist, &song.title, path).await else {
            info!(
                "Library review sweep: Navidrome does not show '{} - {}' yet, so it is asked about instead of replaced",
                song.artist, song.title
            );
            return Replacing::Declined;
        };
        match replacer.marked(&id).await {
            Some(false) => {}
            Some(true) => {
                info!(
                    "Library review sweep: '{} - {}' is starred or rated, so it is asked about instead of replaced",
                    song.artist, song.title
                );
                return Replacing::Declined;
            }
            None => {
                info!(
                    "Library review sweep: the stars and ratings on '{} - {}' could not be read, so it is asked about instead of replaced",
                    song.artist, song.title
                );
                return Replacing::Declined;
            }
        }
        let by_preview = question.reason == InconclusiveReason::DiffersFromYouTube;
        let replacing = if by_preview {
            format!(
                "Replacing '{} - {}', which sounds nothing like its YouTube preview.",
                song.artist, song.title
            )
        } else {
            let heard = question
                .matched_text()
                .unwrap_or_else(|| "another song".to_string());
            format!(
                "Replacing '{} - {}', which AcoustID heard as {heard}.",
                song.artist, song.title
            )
        };
        self.hold("Running", Some(&replacing), Duration::ZERO);
        // A replacement by the preview comes only from a source the preview checks, never the
        // YouTube fallback.
        let only_source = (by_preview
            && self.settings.current().subsonic.download_source == DownloadSource::SoulseekThenYouTube)
            .then_some(DownloadSource::Soulseek);
        let outcome = replacer
            .replace(LibraryActionRequest {
                only_source,
                ..LibraryActionRequest::new(LibraryAction::WrongSong, id.clone(), keeper)
            })
            .await;
        let now = self.clock.now();
        match outcome {
            Ok(outcome) if outcome.state == LibraryActionState::Applied => {
                self.count_today(now, true);
                let needs = self.settings.current().soulseek.effective_min_match_score();
                let note = if by_preview {
                    format!(
                        "Octo replaced this: it sounded nothing like its YouTube preview ({})",
                        question.evidence.as_deref().unwrap_or("not the same song")
                    )
                } else {
                    Self::replaced_note(question, needs)
                };
                if by_preview {
                    self.notices
                        .add_replaced_because(keeper, path, song, question, &id, note.clone(), None);
                } else {
                    self.notices
                        .add_replaced(keeper, path, song, question, &id, needs);
                }
                info!(
                    "Library review sweep replaced '{} - {}' through Wrong song as {keeper}: {note}",
                    song.artist, song.title
                );
                Replacing::Done
            }
            Ok(outcome) if outcome.code.as_deref() == Some(LibraryActionCodes::SOULSEEK_OFFLINE) => {
                info!(
                    "Library review sweep: '{} - {}' waits, because Soulseek is out and the replacement may come from nowhere else",
                    song.artist, song.title
                );
                Replacing::Waits
            }
            Ok(outcome) => {
                if outcome.state == LibraryActionState::Failed {
                    self.count_today(now, false);
                }
                info!(
                    "Library review sweep did not replace '{} - {}' ({}), so it is asked about",
                    song.artist,
                    song.title,
                    outcome.detail.as_deref().unwrap_or("no reason given")
                );
                Replacing::Declined
            }
            Err(e) => {
                self.count_today(now, false);
                warn!(
                    "Library review sweep could not replace '{} - {}' ({e}), so it is asked about",
                    song.artist, song.title
                );
                Replacing::Declined
            }
        }
    }

    /// Counts one replacement, or one that failed, and writes it at once: the day's cap and
    /// breaker must hold across a restart right after.
    fn count_today(&self, now: DateTime<Utc>, replaced: bool) {
        self.store.update(|s| s.count_today(now, replaced));
        self.store.flush();
    }

    /// Whether a file the sweep would ask about is replaced through Wrong song instead: no dry
    /// run, the keeper on the allowlist, Wrong song on, the day's cap and breaker not reached,
    /// and either `ReviewSweepAutoReplace` on with AcoustID clearly hearing another song, or
    /// `ReviewSweepAutoReplacePreview` on with the song's YouTube preview clearly not matching (at least [`Self::PREVIEW_REPLACE_FROM`] of its bits differ).
    /// Either needs `KeepReplacedOriginals` on. The preview's replacing also needs a download
    /// source other than YouTube alone, and Soulseek or SpotiFLAC switched on to fetch from. It
    /// also needs the preview to be a YouTube Music row from the file's own album.
    pub fn decide(
        question: &VerificationResult,
        file: &SweptFile,
        settings: &AppSettings,
        keeper: &str,
        today: Today,
        witness: Option<PreviewWitness>,
    ) -> AutoReplace {
        let actions = &settings.library_actions;
        let wrong_song_on = actions
            .effective_actions()
            .iter()
            .any(|entry| entry.action == LibraryAction::WrongSong && entry.enabled);
        let may = !actions.dry_run
            && actions.is_allowed(Some(keeper))
            && wrong_song_on
            && today.replaced < actions.effective_review_sweep_auto_replace_per_day()
            && today.failed < Self::MAX_FAILED_REPLACEMENTS_PER_DAY;
        // A replacement whose original is not kept could not be put back.
        let by_acoustid = actions.review_sweep_auto_replace
            && actions.keep_replaced_originals
            && Self::clearly_another_song(question, file, &settings.soulseek);
        // A YouTube download is the preview itself, so it cannot vouch for a replacement; every
        // other source is held to the preview. A replacement with no lossless source to fetch
        // from could only wait, and one whose original is not kept could not be put back.
        let by_preview = actions.review_sweep_auto_replace_preview
            && file.tagged
            && question.reason == InconclusiveReason::DiffersFromYouTube
            && witness.is_some_and(|witness| {
                witness.bit_error >= Self::PREVIEW_REPLACE_FROM && witness.album_agrees
            })
            && actions.keep_replaced_originals
            && settings.subsonic.download_source != DownloadSource::YouTube
            && (settings.soulseek.is_on() || settings.spotiflac.is_on());
        if may && (by_acoustid || by_preview) {
            AutoReplace::Replace
        } else {
            AutoReplace::Ask
        }
    }

    /// A `Mismatch` naming a recording, at or above `MinMatchScore`, on a file whose artist and
    /// title come from its tags, by another artist and with another title. Another take or a
    /// remaster of the same song, the same artist's song under a title in another script or
    /// translated, an untagged file, a wrong length and anything AcoustID was unsure of are not,
    /// and neither is a song fewer than two submissions tie the sound to.
    pub fn clearly_another_song(
        question: &VerificationResult,
        file: &SweptFile,
        soulseek: &SoulseekSettings,
    ) -> bool {
        file.tagged
            && question.verdict == VerificationVerdict::Mismatch
            && question.reason == InconclusiveReason::SoundsLikeAnother
            && question.recording_id.as_deref().is_some_and(|id| !id.is_empty())
            && Self::submissions_for_named(question) >= Self::MIN_SUBMISSIONS_TO_REPLACE
            && whole_percent(question.score) >= soulseek.effective_min_match_score()
            && question
                .matched_title
                .as_deref()
                .is_some_and(|heard| !Self::same_title(heard, &file.song.title))
            && !Self::shares_artist(question, &file.song.artist)
    }

    /// How many submissions a replacement by AcoustID needs behind the song it heard: one is one
    /// person's word.
    pub const MIN_SUBMISSIONS_TO_REPLACE: i32 = 2;

    /// How many submissions tie the sound to the song the question names: every recording with
    /// its title and artist in the result that holds it. 0 without AcoustID's answer.
    pub fn submissions_for_named(question: &VerificationResult) -> i32 {
        let (Some(lookup), Some(id)) = (
            &question.lookup,
            question.recording_id.as_deref().filter(|id| !id.is_empty()),
        ) else {
            return 0;
        };
        let key = |r: &octo_core::fingerprint::AcoustIdRecording| {
            (SongIdentity::key(&r.title), SongIdentity::key(&r.artist_credit()))
        };
        lookup
            .results
            .iter()
            .filter_map(|result| {
                let named = result
                    .recordings
                    .iter()
                    .find(|r| dotnet::eq_ignore_case(&r.recording_id, id))?;
                let song = key(named);
                Some(
                    result
                        .recordings
                        .iter()
                        .filter(|r| key(r) == song)
                        .fold(0i32, |total, r| total.saturating_add(r.sources.max(0))),
                )
            })
            .max()
            .unwrap_or(0)
    }

    /// Whether what AcoustID heard may be by the file's own artist: its named artist, or any
    /// recording in the result holding the one it named (its best result when none holds it),
    /// shares an artist with the file's by loose key. Nobody named on either side counts as
    /// shared, so it stays a question.
    pub fn shares_artist(question: &VerificationResult, artist: &str) -> bool {
        let mut heard: Vec<(String, Vec<String>)> = Vec::new();
        if let Some(named) = question.matched_artist.as_deref() {
            heard.push((named.to_string(), Vec::new()));
        }
        if let Some(lookup) = &question.lookup {
            let named_id = question.recording_id.as_deref().filter(|id| !id.is_empty());
            let holds_named = |result: &&octo_core::fingerprint::AcoustIdResult| {
                named_id.is_some_and(|id| {
                    result
                        .recordings
                        .iter()
                        .any(|r| dotnet::eq_ignore_case(&r.recording_id, id))
                })
            };
            let best = lookup
                .results
                .iter()
                .filter(holds_named)
                .max_by(|a, b| a.score.total_cmp(&b.score))
                .or_else(|| lookup.results.iter().max_by(|a, b| a.score.total_cmp(&b.score)));
            if let Some(best) = best {
                heard.extend(
                    best.recordings
                        .iter()
                        .map(|r| (r.artist_credit(), r.artists.clone())),
                );
            }
        }
        let own: Vec<String> = artist
            .split(';')
            .chain(std::iter::once(artist))
            .map(SongIdentity::loose_key)
            .filter(|key| !key.is_empty())
            .collect();
        if own.is_empty() || heard.is_empty() {
            return true;
        }
        heard.iter().any(|(credit, names)| {
            let keys: Vec<String> = names
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(credit.as_str()))
                .map(SongIdentity::loose_key)
                .filter(|key| !key.is_empty())
                .collect();
            keys.is_empty()
                || keys.iter().any(|key| own.contains(key))
                || !matches!(
                    SongIdentity::compare_artists_with_credits(artist, credit, names.as_slice()),
                    ArtistAgreement::None
                )
        })
    }

    /// Whether a YouTube Music row's album is the file's album: both named, the same by their exact
    /// key, and neither a live album. A row from another album may be another recording of the song.
    pub fn preview_album_agrees(row_album: Option<&str>, file_album: &str) -> bool {
        let Some(row_album) = row_album.filter(|album| !dotnet::is_blank(album)) else {
            return false;
        };
        if dotnet::is_blank(file_album)
            || live_version::mentions(Some(row_album))
            || live_version::mentions(Some(file_album))
        {
            return false;
        }
        let key = SongIdentity::key(row_album);
        !key.is_empty() && key == SongIdentity::key(file_album)
    }

    /// Whether a believed YouTube Music row that sounds like the file may settle it: any album, or
    /// none, unless the row or the file is from a live album. The same recording is often filed
    /// under its single, a compilation or no album at all, and the sound has already agreed; a
    /// live take is another recording that can sound close. Replacing still needs
    /// [`Self::preview_album_agrees`].
    pub fn preview_album_settles(row_album: Option<&str>, file_album: &str) -> bool {
        !live_version::mentions(row_album) && !live_version::mentions(Some(file_album))
    }

    /// The same song by title: the loose keys once guest credits are taken off, or the cores
    /// once version tails ("Live", "Remastered 2011") are taken off too. A title with no key is
    /// taken as the same, so it stays a question.
    pub fn same_title(heard: &str, own: &str) -> bool {
        let plain = |title: &str| SongIdentity::loose_key(&SongIdentity::strip_features(title));
        let core = |title: &str| SongIdentity::parse_title(title, None).loose_key;
        let (heard_plain, own_plain) = (plain(heard), plain(own));
        heard_plain.is_empty() || own_plain.is_empty() || heard_plain == own_plain || core(heard) == core(own)
    }

    /// The Review note a replaced file leaves its keeper: "Octo replaced this: AcoustID heard
    /// Artist - Title, 97% sure (needs 85%)".
    pub fn replaced_note(question: &VerificationResult, needs: i32) -> String {
        NoticeQueue::replaced_reason(question, needs)
    }

    /// Octo's own downloads inside the music folder, library-relative with `/`, in ordinal
    /// order. Hidden folders, the quarantine among them, are left out.
    async fn octo_downloads(&self, root: &str) -> Vec<String> {
        let base = get_full_path(root);
        let prefix = format!("{}/", base.trim_end_matches('/'));
        let mut found: Vec<String> = self
            .library
            .get_mappings()
            .await
            .into_iter()
            .map(|mapping| mapping.local_path)
            .filter(|path| !path.is_empty())
            .filter_map(|path| {
                let full = get_full_path(&path);
                full.strip_prefix(&prefix).map(str::to_string)
            })
            .filter(|relative| {
                !relative
                    .split('/')
                    .any(|segment| segment.is_empty() || segment.starts_with('.'))
            })
            .collect();
        found.sort_by(|a, b| compare_ordinal(a, b));
        found.dedup();
        found
    }

    /// "Re-check Octo's downloads": forgets when each of Octo's own downloads was checked and
    /// starts one pass over them, at least [`LibraryReviewSweepWorker::RECHECK_PER_HOUR`] an
    /// hour, which then stops. A paused check carries on. Answers how many downloads it holds.
    pub async fn recheck_octo(&self) -> usize {
        let root = (self.music_root)();
        let files = self.octo_downloads(&root).await;
        let threshold = self.settings.current().soulseek.effective_min_match_score();
        let now = self.clock.now();
        let total = i32::try_from(files.len()).unwrap_or(i32::MAX);
        self.generation.fetch_add(1, Ordering::SeqCst);
        *self.retry_verdict.lock() = None;
        self.lookup_failures.store(0, Ordering::SeqCst);
        self.unfingerprinted.store(0, Ordering::SeqCst);
        self.store.update(|s| {
            for file in &files {
                s.checked.remove(file);
            }
            s.paused = false;
            s.recheck = Some(RecheckState {
                running: true,
                total,
                threshold,
                started_utc: Some(now),
                ..Default::default()
            });
        });
        self.store.flush();
        let count = files.len();
        *self.recheck_files.lock() = Some(Arc::new(files));
        count
    }

    pub fn classify(verdict: VerificationResult) -> (Outcome, VerificationResult) {
        match verdict.verdict {
            VerificationVerdict::Confirmed
                if Self::is_length_off(
                    verdict.duration_seconds,
                    verdict.r#match.as_ref().and_then(|m| m.duration_seconds),
                ) =>
            {
                (
                    Outcome::Ask,
                    VerificationResult {
                        reason: InconclusiveReason::LengthOff,
                        ..verdict
                    },
                )
            }
            VerificationVerdict::Confirmed => (Outcome::Fine, verdict),
            // No recording id is "no decodable audio". On a network mount that can be a read
            // that failed this once, so it is counted and never asked about.
            VerificationVerdict::Mismatch if verdict.recording_id.as_deref().is_none_or(str::is_empty) => {
                (Outcome::Undecodable, verdict)
            }
            VerificationVerdict::Mismatch => (
                Outcome::Ask,
                VerificationResult {
                    reason: InconclusiveReason::SoundsLikeAnother,
                    ..verdict
                },
            ),
            _ if verdict.needs_review() => (Outcome::Ask, verdict),
            _ if verdict.reason == InconclusiveReason::NotFingerprinted => {
                (Outcome::NotFingerprinted, verdict)
            }
            _ => (Outcome::LookupFailed, verdict),
        }
    }

    /// Off by more than 20 seconds or a tenth of the recording, whichever is more. 20 seconds
    /// absorbs fades, pregaps and trailing silence; a tenth still catches a radio edit standing
    /// in for the album cut on a six-minute track, which a wider margin would let through.
    pub fn is_length_off(file_seconds: i32, recording_seconds: Option<i32>) -> bool {
        match recording_seconds {
            Some(expected) if file_seconds > 0 && expected > 0 => {
                f64::from((file_seconds - expected).abs()) > f64::max(20.0, f64::from(expected) / 10.0)
            }
            _ => false,
        }
    }

    /// The Navidrome admin when they may answer, otherwise the first allowed user. One person,
    /// because a library file has no requester to ask instead.
    pub fn keeper(settings: &LibraryActionSettings, admin_username: Option<&str>) -> Option<String> {
        if settings.is_allowed(admin_username) {
            return admin_username.map(|admin| admin.trim().to_string());
        }
        settings
            .allowed_users
            .iter()
            .map(|user| user.trim())
            .find(|user| !user.is_empty())
            .map(str::to_string)
    }

    /// Header only: the fingerprint reads the audio anyway, and a mount is slow. A file with no
    /// title tag is named by its file name, and is never taken as tagged.
    pub fn read_tags(path: &str) -> SweptFile {
        let fallback = file_name_without_extension(path);
        match TagFile::open(path) {
            Ok(file) => {
                let performers = file.performers();
                let title = file.title().filter(|title| !dotnet::is_blank(title));
                let tagged = title.is_some() && performers.iter().any(|p| !dotnet::is_blank(p));
                let isrc = [file.isrc(), tag_writer_extras::read_text(&file, TagFields::ISRC)]
                    .into_iter()
                    .flatten()
                    .flat_map(|value| {
                        value
                            .split([';', ',', '/', '\0'])
                            .filter_map(SongIdentity::normalize_isrc)
                            .collect::<Vec<_>>()
                    })
                    .next();
                SweptFile {
                    song: Song {
                        artist: performers.join("; "),
                        title: title.map_or(fallback, |title| title.trim().to_string()),
                        album: file.album().unwrap_or_default(),
                        duration: Some(file.duration_seconds()).filter(|seconds| *seconds > 0),
                        ..Default::default()
                    },
                    tagged,
                    isrc,
                }
            }
            Err(_) => SweptFile {
                song: Song {
                    artist: String::new(),
                    title: fallback,
                    album: String::new(),
                    ..Default::default()
                },
                tagged: false,
                isrc: None,
            },
        }
    }

    /// Every audio file under `root`, library-relative with `/`, in ordinal order. A leading dot
    /// is the quarantine and anything else kept out of Navidrome's scan; slskd's incomplete
    /// folder holds half-written downloads when it sits in the library. A folder that cannot be
    /// read is skipped, and hidden entries, whose names start with a dot, are not walked. The
    /// trash folder is left out by its configured name too, which may have no dot.
    pub fn enumerate(root: &str, trash: Option<&Path>) -> Vec<String> {
        let root = Path::new(root);
        let mut found: Vec<String> = Vec::new();
        let mut folders = vec![root.to_path_buf()];
        while let Some(folder) = folders.pop() {
            let Ok(entries) = std::fs::read_dir(&folder) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let path = entry.path();
                let is_dir = match entry.file_type() {
                    Ok(kind) if kind.is_symlink() => {
                        std::fs::metadata(&path).is_ok_and(|target| target.is_dir())
                    }
                    Ok(kind) => kind.is_dir(),
                    Err(_) => false,
                };
                if is_dir {
                    if !library_walk::is_trash(&path, trash) {
                        folders.push(path);
                    }
                    continue;
                }
                let extension = extension_of(&name);
                if !AUDIO_EXTENSIONS
                    .iter()
                    .any(|audio| dotnet::eq_ignore_case(audio, &extension))
                {
                    continue;
                }
                let Ok(relative) = path.strip_prefix(root) else {
                    continue;
                };
                let relative = relative.to_string_lossy().replace('\\', "/");
                // slskd's default incomplete folder and Octo's own staging.
                let incomplete_folders = excluded_folder_names(None);
                let kept_out = relative.split('/').any(|segment| {
                    segment.starts_with('.')
                        || incomplete_folders
                            .iter()
                            .any(|folder| dotnet::eq_ignore_case(folder, segment))
                });
                if !kept_out {
                    found.push(relative);
                }
            }
        }
        found.sort_by(|a, b| compare_ordinal(a, b));
        found
    }

    fn record(&self, generation: i32, change: impl FnOnce(&mut ReviewSweepState)) {
        self.store.update(|s| {
            if generation == self.generation.load(Ordering::SeqCst) {
                change(s);
            }
        });
    }

    fn hold(&self, state: &str, reason: Option<&str>, wait: Duration) -> Duration {
        *self.hold.lock() = (state.to_string(), reason.map(str::to_string));
        wait
    }

    pub fn set_paused(&self, paused: bool) {
        self.store.update(|s| s.paused = paused);
        self.store.flush();
    }

    pub fn reset(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        *self.retry_verdict.lock() = None;
        *self.files.lock() = None;
        *self.recheck_files.lock() = None;
        self.store.update(|s| {
            s.cursor = String::new();
            s.checked = DotnetDictionary::new();
            s.pass = 1;
            s.total = 0;
            s.found = 0;
            s.fine = 0;
            s.undecodable = 0;
            s.replaced = 0;
            s.last_checked_utc = None;
            s.pass_finished_utc = None;
            s.next_pass_utc = datetime::min_value();
        });
        self.store.flush();
    }

    pub fn status(&self) -> ReviewSweepStatus {
        let settings = self.settings.current();
        let files = self.files.lock().clone();
        let recheck_files = self.recheck_files.lock().clone();
        let (state, reason) = self.hold.lock().clone();
        let keeper = Self::keeper(
            &settings.library_actions,
            settings.subsonic.admin_username.as_deref(),
        );
        let now = self.clock.now();
        let threshold = settings.soulseek.effective_min_match_score();
        let default_threshold = SoulseekSettings::default().effective_min_match_score();
        self.store.read(|s| ReviewSweepStatus {
            state,
            reason,
            paused: s.paused,
            position: match &files {
                None if s.cursor.is_empty() => 0,
                None => s.checked.len(),
                Some(files) => first_after(files, &s.cursor),
            },
            total: s.total,
            pass: s.pass,
            found: s.found,
            open: self.notices.open_count(NoticeOrigin::LibrarySweep),
            fine: s.fine,
            undecodable: s.undecodable,
            keeper,
            per_hour: settings.library_actions.effective_review_sweep_per_hour(),
            last_checked_utc: s.last_checked_utc,
            pass_finished_utc: s.pass_finished_utc,
            replaced: s.replaced,
            recheck: s.recheck.as_ref().map(|recheck| RecheckStatus {
                running: recheck.running,
                position: match &recheck_files {
                    _ if !recheck.running => usize::try_from(recheck.total).unwrap_or(0),
                    Some(files) => first_after(files, &recheck.cursor),
                    None => usize::try_from(recheck.checked).unwrap_or(0),
                },
                total: recheck.total,
                checked: recheck.checked,
                found: recheck.found,
                replaced: recheck.replaced,
                threshold: recheck.threshold,
                started_utc: recheck.started_utc,
                finished_utc: recheck.finished_utc,
            }),
            auto_replace: settings.library_actions.review_sweep_auto_replace,
            auto_replace_preview: settings.library_actions.review_sweep_auto_replace_preview,
            auto_replace_per_day: settings
                .library_actions
                .effective_review_sweep_auto_replace_per_day(),
            today: s.today(now),
            threshold_changed: threshold
                != s.recheck
                    .as_ref()
                    .map_or(default_threshold, |recheck| recheck.threshold),
        })
    }
}

/// `relative` under `root`, made a full path.
fn full_path(root: &str, relative: &str) -> String {
    get_full_path(&format!("{root}/{relative}"))
}

/// A binary search for the cursor in ordinal order: the index after the cursor, or where it
/// would go.
fn first_after(files: &[String], cursor: &str) -> usize {
    match files.binary_search_by(|file| compare_ordinal(file, cursor)) {
        Ok(index) => index + 1,
        Err(index) => index,
    }
}

/// The file's last write time in 100 ns ticks since 0001-01-01.
fn modified_ticks(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .map(|modified| dotnet_ticks(DateTime::<Utc>::from(modified)))
        .unwrap_or(0)
}

/// The extension of a file name: from its last dot, or nothing.
fn extension_of(name: &str) -> String {
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => name[dot..].to_string(),
        _ => String::new(),
    }
}

/// The file name up to its last dot.
fn file_name_without_extension(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) => name[..dot].to_string(),
        None => name.to_string(),
    }
}

#[cfg(test)]
#[path = "library_review_sweep_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "library_review_sweep_replace_tests.rs"]
mod replace_tests;
