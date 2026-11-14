//! The sonic analysis worker: Sounds alike (multi-source radio) asks octo-sonic about every
//! library song once, one at a time with a pause between, only while nothing downloads, then
//! keeps up with new and changed songs on a pass every [`PASS_INTERVAL`]. A song that arrived
//! through a tracked download goes first.
//!
//! The same reading makes each song's transition profile, when octo-sonic makes them: a song
//! read before profiles existed is read once more for its profile alone. A song from outside
//! the library whose file just landed in the music folder gets its profile too, under its
//! acquisition key, so the apps find it by the id they play it by. Profiles are made whether
//! or not Sounds alike is on; with it off (on the Radio page or in .env) each song is read for
//! its profile alone, and how it sounds is never read.
//!
//! When octo-sonic says a newer revision of its profiles than the last complete pass read, the
//! next pass starts at once and reads every song with a lower one again, for its profile alone,
//! one at a time like any other reading, while its profile is still served. Songs from outside
//! the library with a lower one are listed once at the pass's start. New songs, changed files
//! and missing features or profiles go before any song read again.
//!
//! A pass that runs longer than [`PASS_INTERVAL`] lists the library again every
//! [`PASS_INTERVAL`]: songs added meanwhile, which no download announced, go first, and a
//! changed file is read before the songs read again.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use futures::FutureExt;
use octo_core::common::Clock;
use octo_core::json::datetime;
use octo_core::library::generated_playlist_service::dotnet_ticks;
use octo_core::settings::SettingsStore;
use parking_lot::Mutex;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use super::sonic_client::{ISonicClient, SonicAnswer, SonicParts};
use super::sonic_store::{SonicFailure, SonicSong, SonicStore};
use super::transition_store::{FIRST_REVISION, RevisionProgress, TransitionEntry, TransitionStore};
use crate::services::common::{AcquisitionTracker, DownloadArrivals, IAcquisitionActivity};
use crate::services::library::navidrome_song_path_resolver::{Candidate, PathSource};
use crate::services::library::quality_upgrade_worker::LibrarySongRow;
use crate::services::library::{NavidromePlaylistApi, NavidromeSongPathResolver};

/// A local file of a library song, as the analysis stamps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SonicFile {
    /// The full path.
    pub full_name: String,
    /// The size in bytes.
    pub length: i64,
    /// The last write time, in 100-nanosecond ticks ([`dotnet_ticks`]).
    pub last_write_ticks: i64,
}

impl SonicFile {
    /// The file as it is on disk now; None when it cannot be read.
    pub fn stat(path: &str) -> Option<SonicFile> {
        let metadata = std::fs::metadata(path).ok()?;
        let modified: DateTime<Utc> = metadata.modified().ok()?.into();
        Some(SonicFile {
            full_name: path.to_string(),
            length: metadata.len() as i64,
            last_write_ticks: dotnet_ticks(modified),
        })
    }
}

/// What the analysis reads of the library: Navidrome's song list, a song that just arrived, and
/// the local file of a song. A trait so the worker can be tested without one.
#[async_trait]
pub trait ISonicLibrary: Send + Sync {
    /// One page of the song list, and how many rows the page had before missing and pathless
    /// songs were left out. None when the page could not be read.
    async fn list(&self, start: i32, count: i32) -> Option<(Vec<LibrarySongRow>, usize)>;
    async fn arrived(&self, id: &str) -> Option<LibrarySongRow>;
    fn local_file(&self, row: &LibrarySongRow) -> Option<SonicFile>;
}

/// A song's published tempo by its ISRCs. A trait so the worker can be tested without a
/// catalog.
#[async_trait]
pub trait IPublishedTempo: Send + Sync {
    /// The first published tempo found for these ISRCs; None when none has one or the catalog
    /// does not answer.
    async fn bpm(&self, isrcs: &[String]) -> Option<f64>;
}

/// The tempo in a file's BPM tag and its ISRC, read from the file itself.
pub fn file_tempo_tags(path: &str) -> (Option<i32>, Vec<String>) {
    match octo_media::tags::TagFile::open(path) {
        Ok(mut file) => (
            i32::try_from(file.bpm()).ok().filter(|&bpm| bpm > 0),
            file.isrc().into_iter().collect(),
        ),
        Err(_) => (None, Vec::new()),
    }
}

/// The library as Navidrome lists it, with each file checked where Octo can read it.
pub struct NavidromeSonicLibrary {
    api: Arc<NavidromePlaylistApi>,
    resolver: Arc<NavidromeSongPathResolver>,
}

impl NavidromeSonicLibrary {
    pub fn new(api: Arc<NavidromePlaylistApi>, resolver: Arc<NavidromeSongPathResolver>) -> Self {
        NavidromeSonicLibrary { api, resolver }
    }
}

#[async_trait]
impl ISonicLibrary for NavidromeSonicLibrary {
    async fn list(&self, start: i32, count: i32) -> Option<(Vec<LibrarySongRow>, usize)> {
        self.api.list_songs(start, count).await
    }

    /// A song that just arrived, with the BPM tag and ISRC from its file, which the resolver
    /// does not carry.
    async fn arrived(&self, id: &str) -> Option<LibrarySongRow> {
        let file = self.resolver.resolve(id).await?;
        let path = file.absolute_path.clone();
        let (bpm, isrcs) = tokio::task::spawn_blocking(move || file_tempo_tags(&path))
            .await
            .unwrap_or_default();
        Some(LibrarySongRow {
            bpm,
            isrcs,
            id: id.to_string(),
            path: file.absolute_path,
            library_path: None,
            size: file.size_bytes,
            suffix: file.suffix,
            bit_rate: 0,
            title: file.title,
            artist: file.artist,
            duration: file.duration_seconds,
            album: Some(file.album),
            ..Default::default()
        })
    }

    /// The verified local file of a song from Navidrome's song list; None when Octo cannot read
    /// it here (Sounds alike sends only files it can name to octo-sonic).
    fn local_file(&self, row: &LibrarySongRow) -> Option<SonicFile> {
        let candidate = Candidate {
            id: row.id.clone(),
            raw_path: Some(row.path.clone()),
            library_path: row.library_path.clone(),
            size: row.size,
            title: row.title.clone(),
            artist: row.artist.clone(),
            album: row.album.clone().unwrap_or_default(),
            suffix: row.suffix.clone(),
            duration: row.duration,
            source: PathSource::NativeApi,
            missing: false,
            album_artist: row.album_artist.clone(),
        };
        let resolved = self
            .resolver
            .verify(&candidate, &self.resolver.music_root(), true)?;
        SonicFile::stat(&resolved.absolute_path)
    }
}

/// Where the analysis is, for the dashboard. Skipped: songs this pass leaves out, as Octo cannot
/// find their file, or they are too long or have no length.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SonicStatus {
    pub state: String,
    pub reason: Option<String>,
    /// Spelled "analysed" because that is the JSON field's name, and a JSON field keeps its name.
    pub analysed: i32,
    pub total: i32,
    pub failed: i32,
    pub pass: i32,
    #[serde(with = "datetime::utc_option")]
    pub next_pass_utc: Option<DateTime<Utc>>,
    pub paused: bool,
    pub skipped: i32,
    /// Songs with a transition profile of the version octo-sonic makes now.
    pub profiled: i32,
    /// Whether songs are read for how they sound; false while only profiles are made.
    pub reads_sound: bool,
    /// While a pass reads songs again for a newer revision of their profiles.
    pub updating_profiles: Option<ProfilesUpdating>,
}

/// Of the `total` transition profiles this pass has read again or can still read, `done` are of
/// the revision octo-sonic makes now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ProfilesUpdating {
    pub done: i32,
    pub total: i32,
}

pub const IDLE_CHECK: Duration = Duration::from_secs(60);
pub const BUSY_CHECK: Duration = Duration::from_secs(30);
pub const DOWN_CHECK: Duration = Duration::from_secs(10 * 60);
pub const PASS_INTERVAL: TimeDelta = TimeDelta::hours(6);
/// How soon a pass whose second look at the library could not read it tries again.
pub const RELIST_RETRY: TimeDelta = TimeDelta::minutes(10);
pub const RETRY_FAILED_AFTER: TimeDelta = TimeDelta::days(7);
pub const PAGE_SIZE: i32 = 1000;
/// Times octo-sonic dropped the connection on one song before it is marked unreadable.
pub const DROPS_BEFORE_FAILED: i32 = 2;
/// Longer than radio ever plays (RadioFiller), so not worth reading over the network.
pub const LONGEST_SECONDS: i32 = 45 * 60;

pub const TURNED_OFF_IN_ENV: &str =
    "Sounds alike is off in .env (RADIO_SOUNDS_ALIKE=false), so octo-sonic reads nothing.";
pub const OFF_ON_RADIO_PAGE: &str = "Sounds alike is off on the Radio page.";
pub const PROFILES_ONLY_RADIO_PAGE: &str =
    "Sounds alike is off on the Radio page, so only transition profiles are made.";
pub const PROFILES_ONLY_IN_ENV: &str =
    "Sounds alike is off in .env (RADIO_SOUNDS_ALIKE=false), so only transition profiles are made.";
pub const CANNOT_SEE_MUSIC: &str = "octo-sonic cannot see the music folder. Check that it mounts the same folder as Octo (DOWNLOAD_PATH) and can read it (SONIC_USER).";
const DONE: &str = "Every song has been analyzed. New or changed ones are looked for later.";
const DONE_PROFILES: &str =
    "Every song has its transition profile. New or changed ones are looked for later.";
/// While songs are read again for a newer revision of their profiles, which the dashboard
/// counts on its own line: Sounds alike stays done, and new songs are not kept waiting.
const UPDATING: &str = "New or changed songs are read first.";

/// This pass's list and what it has learned on the way.
#[derive(Default)]
struct Pass {
    rows: Option<Vec<LibrarySongRow>>,
    /// Whether this pass's list is the whole library: only then are songs no longer in it
    /// forgotten, so one failed page never costs the songs after it.
    complete: bool,
    /// Songs this pass leaves out, so each tick does not look for their files again.
    skipped: HashSet<String>,
    /// Times octo-sonic dropped the connection on a song, by id. Twice means the file crashes
    /// it; it is then marked unreadable instead of being tried forever.
    dropped: HashMap<String, i32>,
    /// The profile revision octo-sonic said when this pass's list was read: once the pass is
    /// over, every song it walked has a profile of at least that revision.
    revision: Option<i32>,
    /// Songs from outside the library, by key and file, with a profile of a lower revision than
    /// octo-sonic's when the pass began: each is looked at once in the pass.
    outside: VecDeque<(String, String)>,
    /// Whether this pass has read a song for more than a newer revision of its profile.
    analyzing: bool,
    /// When this pass lists the library again, should it still be going.
    relist_utc: Option<DateTime<Utc>>,
}

/// What a library song needs, as far as is known without looking at its file.
enum Wanted {
    /// Nothing: it is read at this size, or its reading failed lately.
    Nothing,
    /// It is left out of the pass: it has no length, or runs longer than radio plays.
    LeftOut,
    /// A reading: it is new, its file changed, or it lacks features or a profile.
    Read,
    /// Only a newer revision of its profile, read once no song needs a reading.
    Again,
}

/// The next song to read: its row, its file, the file's stamp, and what to read of it. A song
/// from outside the library is read for its profile alone, kept under its acquisition key.
/// `reread` when the song is read again only for a newer revision of the profile it has.
struct Target {
    row: LibrarySongRow,
    file: String,
    stamp: String,
    parts: SonicParts,
    outside: bool,
    reread: bool,
}

pub struct SonicAnalysisWorker {
    store: Arc<SonicStore>,
    transitions: Arc<TransitionStore>,
    sonic: Arc<dyn ISonicClient>,
    library: Arc<dyn ISonicLibrary>,
    activity: Arc<dyn IAcquisitionActivity>,
    /// The radio source settings.
    settings: Arc<SettingsStore>,
    clock: Clock,
    first: Mutex<VecDeque<String>>,
    /// Songs from outside the library whose file just landed: their key and their file.
    outside: Mutex<VecDeque<(String, String)>>,
    pass: Mutex<Pass>,
    generation: AtomicU64,
    /// State and reason for the dashboard. Until the first tick, which waits for start-up to
    /// finish: not "Off", which would mislead.
    status: Mutex<(String, Option<String>)>,
    /// Whether the last look read how songs sound, or made profiles only.
    reads_sound: AtomicBool,
    /// The profile revision octo-sonic said at the last look; 0 when it said none.
    revision: AtomicI32,
    /// Songs' published tempos, for their profiles.
    published: OnceLock<Arc<dyn IPublishedTempo>>,
}

impl SonicAnalysisWorker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<SonicStore>,
        transitions: Arc<TransitionStore>,
        sonic: Arc<dyn ISonicClient>,
        library: Arc<dyn ISonicLibrary>,
        activity: Arc<dyn IAcquisitionActivity>,
        settings: Arc<SettingsStore>,
        tracker: Option<&Arc<AcquisitionTracker>>,
        clock: Clock,
    ) -> Arc<Self> {
        let worker = Arc::new(SonicAnalysisWorker {
            store,
            transitions,
            sonic,
            library,
            activity,
            settings,
            clock,
            first: Mutex::new(VecDeque::new()),
            outside: Mutex::new(VecDeque::new()),
            pass: Mutex::new(Pass::default()),
            generation: AtomicU64::new(0),
            reads_sound: AtomicBool::new(true),
            revision: AtomicI32::new(0),
            published: OnceLock::new(),
            status: Mutex::new((
                "Waiting".to_string(),
                Some("Starts a couple of minutes after Octo does.".to_string()),
            )),
        });
        if let Some(tracker) = tracker {
            let weak: Weak<SonicAnalysisWorker> = Arc::downgrade(&worker);
            tracker.subscribe_ended(Arc::new(move |end| {
                if let Some(worker) = weak.upgrade()
                    && end.done
                    && let Some(id) = end.library_id.as_deref().filter(|id| !id.is_empty())
                {
                    worker.arrived_first(id);
                }
            }));
        }
        worker
    }

    pub fn store(&self) -> &Arc<SonicStore> {
        &self.store
    }

    pub fn transitions(&self) -> &Arc<TransitionStore> {
        &self.transitions
    }

    /// A song that just arrived in the library is analyzed before the rest of the pass.
    pub fn arrived_first(&self, library_id: &str) {
        self.first.lock().push_back(library_id.to_string());
    }

    /// A song from outside the library whose file just landed at `path` gets its profile
    /// before the rest of the pass, kept under its acquisition key.
    pub fn arrived_outside(&self, key: &str, path: &str) {
        let mut outside = self.outside.lock();
        if !outside.iter().any(|(k, _)| k == key) {
            outside.push_back((key.to_string(), path.to_string()));
        }
    }

    /// Hears every song from outside the library that lands in the music folder.
    pub fn listen_to(self: &Arc<Self>, arrivals: &DownloadArrivals) {
        let weak: Weak<SonicAnalysisWorker> = Arc::downgrade(self);
        arrivals.subscribe(Arc::new(move |key, path| {
            if let Some(worker) = weak.upgrade() {
                worker.arrived_outside(key, path);
            }
        }));
    }

    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        // Never alongside startup, which already works the disk and Navidrome.
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(120)) => {}
            _ = stopping.cancelled() => {
                self.store.flush();
                self.transitions.flush();
                return Ok(());
            }
        }
        while !stopping.is_cancelled() {
            // Per-tick catch is mandatory: an unhandled exception would stop the host.
            let tick = tokio::select! {
                tick = std::panic::AssertUnwindSafe(self.tick()).catch_unwind() => tick,
                _ = stopping.cancelled() => break,
            };
            let wait = tick.unwrap_or_else(|_| {
                error!("Sounds alike analysis failed");
                IDLE_CHECK
            });
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = stopping.cancelled() => break,
            }
        }
        self.store.flush();
        self.transitions.flush();
        Ok(())
    }

    pub async fn tick(&self) -> Duration {
        let settings = self.settings.current().radio_sources.clone();
        if self.store.read(|s| s.paused) {
            return self.hold("Paused", Some("Paused from the dashboard."), IDLE_CHECK);
        }
        let Some(health) = self.sonic.health().await else {
            return self.hold("Waiting", Some("octo-sonic is not answering."), DOWN_CHECK);
        };
        if health.off {
            return self.hold("Off", Some(TURNED_OFF_IN_ENV), IDLE_CHECK);
        }
        if health.problem.is_some() {
            return self.hold("Waiting", Some(CANNOT_SEE_MUSIC), DOWN_CHECK);
        }
        if let Some(version) = health.transition_version {
            self.transitions.set_current_version(version);
        }
        // A profile revision newer than the last complete pass read: songs with a lower one are
        // read again from the next pass, which starts now instead of hours later.
        let revision = health.transition_version.and(health.transition_revision);
        self.revision.store(revision.unwrap_or(0), Ordering::SeqCst);
        if revision.is_some_and(|r| r > self.transitions.revision())
            && self.store.read(|s| s.next_pass_utc.is_some())
        {
            self.store.write(|s| s.next_pass_utc = None);
        }
        // How songs sound is read only while Sounds alike is on in both places; profiles are
        // made either way, when octo-sonic makes them.
        let profiles_only = if !settings.sounds_alike {
            Some(PROFILES_ONLY_RADIO_PAGE)
        } else if health.profiles_only {
            Some(PROFILES_ONLY_IN_ENV)
        } else {
            None
        };
        if profiles_only.is_some() && health.transition_version.is_none() {
            return self.hold("Off", Some(OFF_ON_RADIO_PAGE), IDLE_CHECK);
        }
        let reads_sound = profiles_only.is_none();
        if reads_sound && !self.reads_sound.swap(reads_sound, Ordering::SeqCst) {
            // Sounds alike just came on: songs read for their profiles alone need their sound
            // too, so the next pass starts now instead of hours later.
            self.store.write(|s| s.next_pass_utc = None);
        }
        self.reads_sound.store(reads_sound, Ordering::SeqCst);
        let sound_version = profiles_only.is_none().then_some(health.version);
        let done = if profiles_only.is_some() {
            DONE_PROFILES
        } else {
            DONE
        };
        if self.activity.is_busy() {
            return self.hold("Waiting", Some("Waiting for a download to finish."), BUSY_CHECK);
        }

        let now = self.clock.now();
        // Between passes only the songs that just arrived are read, without listing the library
        // again or counting a pass.
        let arrivals_only = self.pass.lock().rows.is_none()
            && self
                .store
                .read(|s| s.next_pass_utc.is_some_and(|next| next > now));
        let outside_waiting = health.transition_version.is_some() && !self.outside.lock().is_empty();
        if arrivals_only && self.first.lock().is_empty() && !outside_waiting {
            return self.hold("Done", Some(done), IDLE_CHECK);
        }

        let generation = self.generation.load(Ordering::SeqCst);
        if !arrivals_only && self.pass.lock().rows.is_none() {
            let Some((rows, complete)) = self.list().await else {
                return self.hold(
                    "Waiting",
                    Some("Navidrome's song list could not be read (Octo needs its own Navidrome sign-in); trying again."),
                    IDLE_CHECK,
                );
            };
            if generation != self.generation.load(Ordering::SeqCst) {
                return Duration::ZERO;
            }
            // Songs from outside the library reach the worker as their files land; those with a
            // profile of a lower revision are listed here, once a pass.
            let outside = revision
                .map(|r| self.transitions.outside_below(r))
                .unwrap_or_default();
            let mut pass = self.pass.lock();
            pass.rows = Some(rows);
            pass.complete = complete;
            pass.skipped.clear();
            pass.revision = revision;
            pass.outside = outside.into();
            pass.analyzing = false;
            pass.relist_utc = Some(now + PASS_INTERVAL);
        } else if !arrivals_only && self.pass.lock().relist_utc.is_some_and(|at| now >= at) {
            // A long pass (a whole library read again for a newer revision takes days) looks at
            // the library again, for songs no download announced.
            let listed = self.list().await;
            if generation != self.generation.load(Ordering::SeqCst) {
                return Duration::ZERO;
            }
            self.relisted(listed, now);
        }

        let Some(target) = self
            .next_target(
                sound_version,
                health.transition_version,
                revision,
                now,
                arrivals_only,
            )
            .await
        else {
            if arrivals_only {
                return self.hold("Done", Some(done), IDLE_CHECK);
            }
            if generation != self.generation.load(Ordering::SeqCst) {
                return Duration::ZERO;
            }
            let (present, complete, count, skipped, began_at) = {
                let pass = self.pass.lock();
                let rows = pass.rows.as_deref().unwrap_or_default();
                (
                    rows.iter().map(|row| row.id.clone()).collect::<HashSet<String>>(),
                    pass.complete,
                    rows.len(),
                    pass.skipped.len(),
                    pass.revision,
                )
            };
            if complete {
                self.transitions.keep_only(&present);
            }
            // Every song this pass walked has a profile of the revision it began with, or is
            // left for later as one that failed; a pass that began without one changes nothing.
            if let Some(revision) = began_at {
                self.transitions.set_revision(revision);
            }
            self.transitions.flush();
            self.store.write(|s| {
                if complete {
                    s.songs.retain(|id, _| present.contains(id));
                    s.failed.retain(|id, _| present.contains(id));
                }
                s.pass += 1;
                s.pass_finished_utc = Some(now);
                s.next_pass_utc = Some(now + PASS_INTERVAL);
            });
            self.store.flush();
            info!("octo-sonic finished a pass over {count} song(s), {skipped} left out");
            self.pass.lock().rows = None;
            return self.hold("Done", Some(done), IDLE_CHECK);
        };

        let Target {
            row,
            file,
            stamp,
            parts,
            outside,
            reread,
        } = target;
        let bpm = if parts.transition {
            self.published_tempo(&row, &file, outside).await
        } else {
            None
        };
        let answer = match self.sonic.analyze(&file, parts, bpm).await {
            Ok(answer) => answer,
            Err(_) => {
                // The connection dropped mid-file: octo-sonic restarted, or the file crashed its
                // reader. Once is chance; twice on the same file, it is that file.
                let drops = {
                    let mut pass = self.pass.lock();
                    let drops = pass.dropped.entry(row.id.clone()).or_insert(0);
                    *drops += 1;
                    *drops
                };
                if drops < DROPS_BEFORE_FAILED {
                    return self.hold(
                        "Waiting",
                        Some("octo-sonic stopped answering; trying again."),
                        DOWN_CHECK,
                    );
                }
                self.pass.lock().dropped.remove(&row.id);
                SonicAnswer {
                    error: Some("octo-sonic stopped while reading it".to_string()),
                    ..Default::default()
                }
            }
        };
        // An answer came, so octo-sonic is up: a drop counts only twice in a row.
        if answer.status != 0 {
            self.pass.lock().dropped.clear();
        }
        if generation != self.generation.load(Ordering::SeqCst) {
            return Duration::ZERO;
        }
        let pause = Duration::from_secs(settings.effective_sonic_pause_seconds().max(0) as u64);
        // A song read again only for a newer revision of its profile leaves Sounds alike done,
        // unless this pass has read songs for more.
        let analyzing = {
            let mut pass = self.pass.lock();
            pass.analyzing |= !reread;
            pass.analyzing
        };
        let (state, reason) = if analyzing {
            ("Running", profiles_only)
        } else if arrivals_only {
            ("Done", Some(done))
        } else {
            // Songs are read again for a newer revision of their profiles: Sounds alike stays
            // done, without saying every song is analyzed while the update goes on.
            ("Done", Some(UPDATING))
        };
        if outside {
            // A file outside octo-sonic's music folder (a cached copy) has no profile to make.
            let kept = match answer.transition {
                Some(profile) => self.transitions.put(&row.id, &stamp, Some(&file), &profile),
                None => {
                    info!(
                        "No transition profile for {} ({file}): {}",
                        row.id,
                        answer.error.as_deref().unwrap_or("")
                    );
                    // A profile it already has stays served; the file is not read again for this
                    // revision before RETRY_FAILED_AFTER. One octo-sonic cannot see blames nothing.
                    let blamed = answer.status != 403 && answer.status != 404;
                    if blamed {
                        let revision = revision.unwrap_or(FIRST_REVISION);
                        self.transitions.record_failure(&row.id, &stamp, revision, now);
                    }
                    blamed
                }
            };
            if reread && kept {
                self.transitions.reread_kept();
            }
            return self.hold(state, reason, pause);
        }
        // Octo sees the file and octo-sonic does not: its music mount is not Octo's. That is no
        // fault of the song, so nothing is recorded and the song is asked again once it is fixed.
        if answer.status == 403 || answer.status == 404 {
            return self.hold("Waiting", Some(CANNOT_SEE_MUSIC), DOWN_CHECK);
        }
        if let Some(profile) = &answer.transition {
            self.transitions.put(&row.id, &stamp, None, profile);
        }
        if reread {
            // A song read again for a newer revision of its profile keeps the profile it has
            // when that reading fails. The failure is kept on the profile's entry, as for a song
            // from outside the library: the song is not read again for this revision before
            // RETRY_FAILED_AFTER, and it is not one of the songs Sounds alike could not read.
            if answer.transition.is_none() {
                info!(
                    "No newer transition profile for '{} - {}': {}",
                    row.artist,
                    row.title,
                    answer.error.as_deref().unwrap_or("")
                );
                let revision = revision.unwrap_or(FIRST_REVISION);
                self.transitions.record_failure(&row.id, &stamp, revision, now);
            }
            self.transitions.reread_kept();
            return self.hold(state, reason, pause);
        }
        let (features, error) = (answer.features, answer.error);
        // A song read for its profile alone keeps the features it has.
        if features.is_none() && error.is_none() {
            return self.hold(state, reason, pause);
        }
        if features.is_none() {
            info!(
                "octo-sonic could not read '{} - {}': {}",
                row.artist,
                row.title,
                error.as_deref().unwrap_or("")
            );
        }
        self.store.write(|s| match features {
            Some(features) => {
                s.songs.insert(
                    row.id.clone(),
                    SonicSong {
                        stamp,
                        title: row.title.clone(),
                        artist: row.artist.clone(),
                        album: row.album.clone(),
                        duration: row.duration,
                        version: features.version,
                        f: features.values,
                    },
                );
                s.failed.shift_remove(&row.id);
            }
            None => {
                s.failed.insert(
                    row.id.clone(),
                    SonicFailure {
                        stamp,
                        error: error.unwrap_or_else(|| "unknown".to_string()),
                        at_utc: now,
                    },
                );
            }
        });
        self.hold(state, reason, pause)
    }

    /// A song first in line, else a song from outside the library that just landed, else the
    /// next one with no features or profile, a changed file, older features or a profile of an
    /// older version, or a failure old enough to try again; once none is left, one from outside
    /// listed for this pass, else the next song whose profile's revision is below `revision`.
    /// Features are looked at only when `version` is given.
    async fn next_target(
        &self,
        version: Option<i32>,
        transition: Option<i32>,
        revision: Option<i32>,
        now: DateTime<Utc>,
        arrivals_only: bool,
    ) -> Option<Target> {
        loop {
            let Some(id) = self.first.lock().pop_front() else {
                break;
            };
            // A song that just arrived is not in this pass's list yet: ask Navidrome for it, and
            // keep it in the list so the pass's end does not forget it again.
            let listed = if arrivals_only {
                None
            } else {
                self.pass
                    .lock()
                    .rows
                    .as_ref()
                    .and_then(|rows| rows.iter().find(|row| row.id == id).cloned())
            };
            let row = match listed {
                Some(row) => Some(row),
                None => match self.library.arrived(&id).await {
                    Some(arrived) => {
                        if !arrivals_only && let Some(rows) = self.pass.lock().rows.as_mut() {
                            rows.push(arrived.clone());
                        }
                        Some(arrived)
                    }
                    None => None,
                },
            };
            if let Some(row) = row
                && let Some(found) = self.candidate(
                    &row,
                    version,
                    transition,
                    revision,
                    now,
                    &mut self.pass.lock().skipped,
                )
            {
                return Some(found);
            }
        }
        if let Some(transition) = transition {
            loop {
                let Some((key, path)) = self.outside.lock().pop_front() else {
                    break;
                };
                if let Some(found) = self.outside_target(&key, &path, transition, revision, now) {
                    return Some(found);
                }
            }
        }
        if arrivals_only {
            return None;
        }
        // The rows are checked in place, under the pass's lock: the cheap checks first, and a
        // file's stat only for a song not already known. A song that needs only a newer revision
        // of its profile waits until no song needs a reading; the first is noted for later.
        let again_from = {
            let mut pass = self.pass.lock();
            let Pass { rows, skipped, .. } = &mut *pass;
            let mut again_from = None;
            for (at, row) in rows.as_deref()?.iter().enumerate() {
                if skipped.contains(&row.id) {
                    continue;
                }
                match self.wanted(row, version, transition, revision, now) {
                    Wanted::Nothing => {}
                    Wanted::LeftOut => {
                        skipped.insert(row.id.clone());
                    }
                    Wanted::Again => {
                        again_from.get_or_insert(at);
                    }
                    Wanted::Read => {
                        if let Some(found) = self.target(row, version, transition, revision, now, skipped) {
                            return Some(found);
                        }
                    }
                }
            }
            again_from
        };
        if let Some(transition) = transition {
            loop {
                let Some((key, path)) = self.pass.lock().outside.pop_front() else {
                    break;
                };
                if let Some(found) = self.outside_target(&key, &path, transition, revision, now) {
                    return Some(found);
                }
            }
        }
        let from = again_from?;
        let mut pass = self.pass.lock();
        let Pass { rows, skipped, .. } = &mut *pass;
        for row in rows.as_deref()?.get(from..).unwrap_or_default() {
            if !skipped.contains(&row.id)
                && matches!(
                    self.wanted(row, version, transition, revision, now),
                    Wanted::Again
                )
                && let Some(found) = self.target(row, version, transition, revision, now, skipped)
            {
                return Some(found);
            }
        }
        None
    }

    /// The library as listed again in the middle of a pass: a song whose row changed takes its
    /// new one, so a changed file is read before the songs read again, and a song not in the
    /// pass's list joins it, first in line. When the list could not be read, the pass goes on
    /// and tries again after [`RELIST_RETRY`].
    fn relisted(&self, listed: Option<(Vec<LibrarySongRow>, bool)>, now: DateTime<Utc>) {
        let added: Vec<String> = {
            let mut pass = self.pass.lock();
            let Some((listed, _)) = listed else {
                pass.relist_utc = Some(now + RELIST_RETRY);
                return;
            };
            pass.relist_utc = Some(now + PASS_INTERVAL);
            let Pass { rows, skipped, .. } = &mut *pass;
            let Some(rows) = rows.as_mut() else {
                return;
            };
            let place: HashMap<String, usize> = rows
                .iter()
                .enumerate()
                .map(|(at, row)| (row.id.clone(), at))
                .collect();
            let mut added = Vec::new();
            for row in listed {
                match place.get(&row.id) {
                    Some(&at) => {
                        if rows[at] != row {
                            // Looked at afresh, should Octo not have found its file before.
                            skipped.remove(&row.id);
                            rows[at] = row;
                        }
                    }
                    None => added.push(row),
                }
            }
            let ids = added.iter().map(|row| row.id.clone()).collect();
            rows.extend(added);
            ids
        };
        if !added.is_empty() {
            info!(
                "octo-sonic found {} song(s) added since its pass began",
                added.len()
            );
        }
        self.first.lock().extend(added);
    }

    /// The tempo a profile may take its octave from: the file's BPM tag, else the catalog's
    /// tempo for the song's ISRC (usually kept from the download's own lookup, else asked on
    /// the background lane). A song from outside the library has its file's tags read for
    /// both; a library song has them from Navidrome.
    async fn published_tempo(&self, row: &LibrarySongRow, file: &str, outside: bool) -> Option<f64> {
        let (bpm, isrcs) = if outside {
            let path = file.to_string();
            tokio::task::spawn_blocking(move || file_tempo_tags(&path))
                .await
                .unwrap_or_default()
        } else {
            (row.bpm, row.isrcs.clone())
        };
        if let Some(bpm) = bpm.filter(|&bpm| bpm > 0) {
            return Some(f64::from(bpm));
        }
        if isrcs.is_empty() {
            return None;
        }
        self.published.get()?.bpm(&isrcs).await
    }

    /// Where songs' published tempos come from, when a catalog is at hand.
    pub fn use_published_tempo(&self, published: Arc<dyn IPublishedTempo>) {
        let _ = self.published.set(published);
    }

    /// A file from outside the library, unless it already has this profile at `revision` or
    /// above, or its reading failed lately at this stamp for that revision.
    fn outside_target(
        &self,
        key: &str,
        path: &str,
        transition: i32,
        revision: Option<i32>,
        now: DateTime<Utc>,
    ) -> Option<Target> {
        let file = SonicFile::stat(path)?;
        let stamp = format!("{}:{}", file.length, file.last_write_ticks);
        let entry = self.transitions.entry(key);
        let same = entry
            .as_ref()
            .is_some_and(|e| e.version == transition && e.stamp == stamp);
        if same && !entry.as_ref().is_some_and(|e| behind(e, revision)) {
            return None;
        }
        let wanted = revision.unwrap_or(FIRST_REVISION);
        if entry.and_then(|e| e.failed).is_some_and(|failed| {
            failed.stamp == stamp && failed.revision >= wanted && now - failed.at_utc < RETRY_FAILED_AFTER
        }) {
            return None;
        }
        Some(Target {
            row: LibrarySongRow {
                id: key.to_string(),
                path: path.to_string(),
                size: file.length,
                ..Default::default()
            },
            file: file.full_name,
            stamp,
            parts: SonicParts {
                sound: false,
                transition: true,
            },
            outside: true,
            reread: same,
        })
    }

    /// The reading a library song needs, if any.
    fn candidate(
        &self,
        row: &LibrarySongRow,
        version: Option<i32>,
        transition: Option<i32>,
        revision: Option<i32>,
        now: DateTime<Utc>,
        skipped: &mut HashSet<String>,
    ) -> Option<Target> {
        match self.wanted(row, version, transition, revision, now) {
            Wanted::Nothing => None,
            Wanted::LeftOut => {
                skipped.insert(row.id.clone());
                None
            }
            Wanted::Read | Wanted::Again => self.target(row, version, transition, revision, now, skipped),
        }
    }

    /// What a library song needs, from what is known of it, without touching the disk (a stat
    /// over a network mount is slow): a song already analyzed (and profiled, when octo-sonic
    /// makes profiles) at this size and version needs nothing, nor one whose reading failed at
    /// this size lately.
    fn wanted(
        &self,
        row: &LibrarySongRow,
        version: Option<i32>,
        transition: Option<i32>,
        revision: Option<i32>,
        now: DateTime<Utc>,
    ) -> Wanted {
        // No length is a file Navidrome could not read, and a long one is more than radio plays.
        if !row.duration.is_some_and(|d| d > 0 && d <= LONGEST_SECONDS) {
            return Wanted::LeftOut;
        }
        let size_prefix = format!("{}:", row.size);
        let (sounded, failed) = self.store.read(|s| {
            let sounded = version.is_none_or(|version| {
                s.songs
                    .get(&row.id)
                    .is_some_and(|done| done.version == version && done.stamp.starts_with(&size_prefix))
            });
            let failed = s.failed.get(&row.id).is_some_and(|lately| {
                lately.stamp.starts_with(&size_prefix) && now - lately.at_utc < RETRY_FAILED_AFTER
            });
            (sounded, failed)
        });
        // Its profile of this version at this size.
        let profiled = transition.and_then(|transition| {
            self.transitions
                .entry(&row.id)
                .filter(|p| p.version == transition && p.stamp.starts_with(&size_prefix))
        });
        let has_profile = transition.is_none() || profiled.as_ref().is_some_and(|p| !behind(p, revision));
        if (sounded && has_profile) || failed {
            return Wanted::Nothing;
        }
        let Some(profile) = profiled.filter(|_| sounded) else {
            return Wanted::Read;
        };
        // All it lacks is a newer revision of its profile, unless reading that failed lately.
        let again_failed = profile.failed.is_some_and(|failed| {
            failed.stamp.starts_with(&size_prefix)
                && revision.is_some_and(|revision| failed.revision >= revision)
                && now - failed.at_utc < RETRY_FAILED_AFTER
        });
        if again_failed {
            Wanted::Nothing
        } else {
            Wanted::Again
        }
    }

    /// The reading a library song needs once its file is looked at: none when Octo cannot find
    /// the file (it is left out of this pass), or the file is the one already read.
    fn target(
        &self,
        row: &LibrarySongRow,
        version: Option<i32>,
        transition: Option<i32>,
        revision: Option<i32>,
        now: DateTime<Utc>,
        skipped: &mut HashSet<String>,
    ) -> Option<Target> {
        let Some(file) = self.library.local_file(row) else {
            skipped.insert(row.id.clone());
            return None;
        };
        let stamp = format!("{}:{}", file.length, file.last_write_ticks);
        let (done, failed) = self.store.read(|s| {
            (
                s.songs
                    .get(&row.id)
                    .map(|done| (done.version, done.stamp == stamp)),
                s.failed
                    .get(&row.id)
                    .is_some_and(|failed| failed.stamp == stamp && now - failed.at_utc < RETRY_FAILED_AFTER),
            )
        });
        let profiled = transition.and_then(|_| self.transitions.entry(&row.id));
        // A profile of this file and version: only a newer revision makes it worth reading again.
        let same_profile = transition.is_some_and(|transition| {
            profiled
                .as_ref()
                .is_some_and(|same| same.version == transition && same.stamp == stamp)
        });
        let current = same_profile && !profiled.as_ref().is_some_and(|p| behind(p, revision));
        let parts = SonicParts {
            sound: version.is_some_and(|version| done != Some((version, true))),
            transition: transition.is_some() && !current,
        };
        if (!parts.sound && !parts.transition) || failed {
            return None;
        }
        Some(Target {
            row: row.clone(),
            file: file.full_name,
            stamp,
            reread: !parts.sound && same_profile,
            parts,
            outside: false,
        })
    }

    /// The library, one song per id, and whether the list reached its end. None when any page
    /// could not be read: a list with a hole would look like songs gone.
    async fn list(&self) -> Option<(Vec<LibrarySongRow>, bool)> {
        let mut rows: Vec<LibrarySongRow> = Vec::new();
        for page in 0..200 {
            let (found, count) = self.library.list(page * PAGE_SIZE, PAGE_SIZE).await?;
            rows.extend(found);
            // Count is the page's rows before missing and pathless songs were left out: a page
            // shorter than asked is the end, a page with a missing song is not.
            if (count as i64) < i64::from(PAGE_SIZE) {
                return Some((distinct(rows), true));
            }
        }
        Some((distinct(rows), false))
    }

    pub fn set_paused(&self, paused: bool) {
        self.store.write(|s| s.paused = paused);
        self.store.flush();
    }

    /// Forget every song's features and start a new pass.
    pub fn reset(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        {
            let mut pass = self.pass.lock();
            pass.rows = None;
            pass.skipped.clear();
        }
        self.store.write(|s| {
            s.songs.clear();
            s.failed.clear();
            s.pass = 0;
            s.next_pass_utc = None;
            s.pass_finished_utc = None;
        });
        self.store.flush();
    }

    pub fn status(&self) -> SonicStatus {
        let (rows, skipped) = {
            let pass = self.pass.lock();
            // This pass's, or the last one's once it is done.
            (pass.rows.as_ref().map(Vec::len), pass.skipped.len())
        };
        let (state, reason) = self.status.lock().clone();
        let profiled = self.transitions.ready_count() as i32;
        // While a pass is on after octo-sonic's revision rose past the last complete one, and
        // songs with a lower one are left that it can still read.
        let revision = self.revision.load(Ordering::SeqCst);
        let updating_profiles = (rows.is_some() && revision > self.transitions.revision())
            .then(|| self.profiles_progress(revision, self.clock.now()))
            .filter(|progress| progress.done < progress.total)
            .map(|progress| ProfilesUpdating {
                done: progress.done as i32,
                total: progress.total as i32,
            });
        self.store.read(|s| SonicStatus {
            state,
            reason,
            analysed: s.songs.len() as i32,
            total: rows.unwrap_or(s.songs.len() + s.failed.len()) as i32,
            failed: s.failed.len() as i32,
            pass: s.pass,
            next_pass_utc: s.next_pass_utc,
            paused: s.paused,
            skipped: skipped as i32,
            profiled,
            reads_sound: self.reads_sound.load(Ordering::SeqCst),
            updating_profiles,
        })
    }

    /// How far this pass has come reading songs again for `revision`. A profile below it counts
    /// only while the pass can still read it: not a song no longer in the pass's list (forgotten
    /// at its end), nor one the pass left out, nor one from outside the library no longer in line
    /// (its file was not found), nor one whose reading for `revision` failed lately, nor a library
    /// song whose reading failed lately at its profile's size, which the walk leaves as `wanted`
    /// does.
    fn profiles_progress(&self, revision: i32, now: DateTime<Utc>) -> RevisionProgress {
        let pass = self.pass.lock();
        let listed: HashSet<&str> = pass.rows.iter().flatten().map(|row| row.id.as_str()).collect();
        let landed = self.outside.lock();
        let in_line: HashSet<&str> = pass
            .outside
            .iter()
            .chain(landed.iter())
            .map(|(key, _)| key.as_str())
            .collect();
        // The size in a file's stamp, `size:ticks`.
        fn size(stamp: &str) -> Option<&str> {
            stamp.split_once(':').map(|(size, _)| size)
        }
        self.store.read(|s| {
            self.transitions.progress(revision, |key, entry| {
                let failed = entry.failed.as_ref().is_some_and(|failed| {
                    failed.stamp == entry.stamp
                        && failed.revision >= revision
                        && now - failed.at_utc < RETRY_FAILED_AFTER
                });
                let reachable = if entry.path.is_some() {
                    in_line.contains(key)
                } else {
                    let sound_failed = s.failed.get(key).is_some_and(|lately| {
                        size(&lately.stamp).is_some_and(|at| size(&entry.stamp) == Some(at))
                            && now - lately.at_utc < RETRY_FAILED_AFTER
                    });
                    listed.contains(key) && !pass.skipped.contains(key) && !sound_failed
                };
                !failed && reachable
            })
        })
    }

    fn hold(&self, state: &str, reason: Option<&str>, wait: Duration) -> Duration {
        *self.status.lock() = (state.to_string(), reason.map(str::to_string));
        wait
    }
}

/// Whether a profile comes from a lower revision than `revision`, the one octo-sonic writes
/// now; never when it says none.
fn behind(entry: &TransitionEntry, revision: Option<i32>) -> bool {
    revision.is_some_and(|r| entry.revision < r)
}

/// One row per id (compared exactly), the first kept.
fn distinct(rows: Vec<LibrarySongRow>) -> Vec<LibrarySongRow> {
    let mut seen = HashSet::new();
    rows.into_iter()
        .filter(|row| seen.insert(row.id.clone()))
        .collect()
}

#[cfg(test)]
#[path = "sonic_analysis_worker_tests.rs"]
mod tests;
