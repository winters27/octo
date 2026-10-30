//! What each person heard of an outside song, kept as prints. While a song is served from its
//! first byte, its bytes go to a scratch file until the song ends or 240 seconds of it are there;
//! `fpcalc` then prints it twice (the raw items proof 2 and the clip check compare, and the
//! compressed fingerprint an AcoustID lookup sends), the scratch file goes, and only the prints are
//! kept. Every `fpcalc` run of a heart or play goes through one queue here, two at a time, hearts
//! first, and every scratch file stays under one cap.
//!
//! Beside the prints: copies a hold keeps of what was heard (a whole copy of the video's stream, a
//! song fetched ahead), and the shim's rounds, in which held hearts that wait on YouTube are tried
//! again once the shim is proven to work.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::{Stream, StreamExt};
use octo_core::common::Clock;
use octo_core::fingerprint::heart_gate::{HeardRecord, HeardSource, WaitingFor};
use octo_core::settings::SettingsStore;
use octo_media::audio::{AudioFingerprinter, FingerprintOutcome, FingerprintResult, RawFingerprint};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info};

use super::preview_copies::PreviewCopies;
use super::store::PrefetchStore;
use crate::services::common::soulseek_hold_store::{HeldAcquisition, ShimTries, SoulseekHoldStore};
use crate::services::i_download_service::AudioStream;
use crate::services::soulseek::ExternalIdRegistry;
use crate::services::you_tube::you_tube_resolver::{Failure, Lookup};
use crate::services::you_tube::{ShimStream, YouTubeResolver};

/// How much of a song a heard sample keeps, in seconds.
pub const SAMPLE_SECONDS: i32 = 240;
/// A sample's byte cap when the stream's rate is not known: 240 seconds at 320 kb/s and the
/// container around it.
pub const SAMPLE_CAP_BYTES: u64 = 12 * 1024 * 1024;
/// The most bytes a whole-song fetch of the heard audio takes.
pub const WHOLE_SONG_CAP_BYTES: u64 = 40 * 1024 * 1024;
/// How many samples the store keeps; the least recently heard goes first.
pub const MAX_SAMPLES: usize = 2_000;
/// How many scratch files may wait at once.
pub const SCRATCH_FILES: usize = 16;
/// How many bytes the scratch files may hold at once.
pub const SCRATCH_BYTES: u64 = 200 * 1024 * 1024;
/// How many `fpcalc` runs go at once.
pub const FPCALC_AT_ONCE: usize = 2;
/// Seconds one raw chromaprint item covers.
pub const ITEM_SECONDS: f64 = 4096.0 / 3.0 / 11025.0;
/// How far short of what it should cover a print may fall and still be whole: `fpcalc` leaves the
/// last part-frame out.
const WHOLE_SLACK_SECONDS: f64 = 3.0;
/// How many probe videos a round asks at most before it gives up on proving the shim works.
pub const PROBE_RUNS: usize = 3;
/// Probe videos for an install that has heard nothing yet: two labels' uploads, so one region
/// block never leaves it without a proof.
pub const FIXED_PROBES: [&str; 2] = ["t1Jm5epJr10", "q9NtWkikVAE"];
/// Rounds with a working shim in which one heard video answered busy before its fetch attempt
/// ends.
pub const STUCK_ROUNDS: u32 = 3;
/// The downloads line of a heart whose heard video stayed busy for [`STUCK_ROUNDS`] rounds.
pub const STUCK_LINE: &str = "Play it once more so Octo can keep the version you hear";
/// A fetch with no new bytes for this long is given up, as a preview copy's fill is.
const FETCH_STALL: Duration = Duration::from_secs(30);
/// The store's index, beside the prints.
const INDEX: &str = "index.json";

// ---------------------------------------------------------------------------------------
// Prints
// ---------------------------------------------------------------------------------------

/// The two prints of one heard sample, the only part of it ever stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeardPrints {
    /// `fpcalc -raw -json` items, what prints are compared by.
    pub raw: Vec<u32>,
    /// `fpcalc -json`'s compressed fingerprint, what an AcoustID lookup sends.
    pub compressed: String,
    /// The seconds `fpcalc` read.
    pub seconds: i32,
    /// The print covers min(240 s, the song's played length).
    pub whole: bool,
    /// The sample ended before the 240 s cap, so the print reaches the song's end.
    pub reaches_end: bool,
}

impl HeardPrints {
    /// The seconds the raw items cover.
    pub fn covered_seconds(&self) -> f64 {
        self.raw.len() as f64 * ITEM_SECONDS
    }
}

/// Whether a print of `items` covers what it should: the song's end when the sample reached it
/// (`read_seconds` being what `fpcalc` read), else min(240 s, the song's length), with a few
/// seconds' slack for the part-frame `fpcalc` leaves out. A song of unknown length that did not
/// reach its end needs the whole 240 s.
pub fn covers_whole(items: usize, read_seconds: i32, reaches_end: bool, song_seconds: Option<i32>) -> bool {
    let covered = items as f64 * ITEM_SECONDS;
    let target = if reaches_end {
        read_seconds.min(SAMPLE_SECONDS)
    } else {
        song_seconds
            .filter(|s| *s > 0)
            .unwrap_or(SAMPLE_SECONDS)
            .min(SAMPLE_SECONDS)
    };
    items > 0 && covered + WHOLE_SLACK_SECONDS >= f64::from(target)
}

/// The id a heard source's prints are stored under: the same for the same source (the same video
/// at the same size, type and format, the same copy, the same file), whoever heard it.
pub fn sample_id_of(source: &HeardSource) -> String {
    let named = match source {
        HeardSource::Copy {
            key,
            ready_at,
            source_file,
        } => format!(
            "copy|{key}|{}|{}",
            ready_at.map(|at| at.timestamp_millis()).unwrap_or_default(),
            source_file.as_deref().unwrap_or("")
        ),
        HeardSource::Video {
            id,
            size,
            content_type,
            itag,
        } => format!(
            "video|{id}|{}|{content_type}|{}",
            size.unwrap_or_default(),
            itag.unwrap_or_default()
        ),
        HeardSource::Library { path } => format!("library|{path}"),
        HeardSource::Acquired { path } => format!("acquired|{path}"),
    };
    let digest = Sha256::digest(named.as_bytes());
    hex::encode(&digest[..12])
}

/// Prints audio files with `fpcalc`.
#[async_trait]
pub trait Printer: Send + Sync {
    /// The raw items of the first `length_seconds` (0: the whole file), and the seconds read.
    async fn raw(&self, path: &Path, length_seconds: i32, timeout_seconds: i32) -> Option<RawFingerprint>;

    /// The compressed fingerprint of the first `length_seconds`, and the seconds decoded.
    async fn compressed(
        &self,
        path: &Path,
        length_seconds: i32,
        timeout_seconds: i32,
    ) -> Option<(String, i32)>;

    /// The compressed fingerprint with how the run went: a file whose audio could not be decoded
    /// is told apart from a run that said nothing.
    async fn fingerprint(&self, path: &Path, length_seconds: i32, timeout_seconds: i32) -> FingerprintResult {
        match self.compressed(path, length_seconds, timeout_seconds).await {
            Some((print, seconds)) => FingerprintResult::new(FingerprintOutcome::Ok, Some(print), seconds),
            None => FingerprintResult::new(FingerprintOutcome::Unavailable, None, 0),
        }
    }
}

#[async_trait]
impl Printer for AudioFingerprinter {
    async fn fingerprint(&self, path: &Path, length_seconds: i32, timeout_seconds: i32) -> FingerprintResult {
        AudioFingerprinter::fingerprint(self, path, length_seconds, timeout_seconds).await
    }

    async fn raw(&self, path: &Path, length_seconds: i32, timeout_seconds: i32) -> Option<RawFingerprint> {
        self.raw_fingerprint(path, length_seconds, timeout_seconds).await
    }

    async fn compressed(
        &self,
        path: &Path,
        length_seconds: i32,
        timeout_seconds: i32,
    ) -> Option<(String, i32)> {
        let result = self.fingerprint(path, length_seconds, timeout_seconds).await;
        match (result.outcome, result.fingerprint) {
            (FingerprintOutcome::Ok, Some(fingerprint)) => Some((fingerprint, result.decoded_seconds)),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------------------
// The fpcalc queue
// ---------------------------------------------------------------------------------------

/// Whose `fpcalc` run this is: hearts' runs go first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FpcalcClass {
    Heart,
    Other,
}

struct SlotQueue {
    free: usize,
    waiting: VecDeque<(FpcalcClass, oneshot::Sender<FpcalcSlot>)>,
}

/// One of the `fpcalc` queue's places, held until it drops.
pub struct FpcalcSlot {
    queue: Option<Arc<Mutex<SlotQueue>>>,
}

impl Drop for FpcalcSlot {
    fn drop(&mut self) {
        if let Some(queue) = self.queue.take() {
            hand_on(queue);
        }
    }
}

/// Gives a freed place to the next waiter, a heart's first, or back to the queue.
fn hand_on(queue: Arc<Mutex<SlotQueue>>) {
    loop {
        let next = {
            let mut q = queue.lock();
            let at = q
                .waiting
                .iter()
                .position(|(class, _)| *class == FpcalcClass::Heart)
                .or(if q.waiting.is_empty() { None } else { Some(0) });
            match at {
                Some(at) => q.waiting.remove(at).map(|(_, sender)| sender),
                None => {
                    q.free += 1;
                    return;
                }
            }
        };
        let Some(sender) = next else {
            continue;
        };
        let slot = FpcalcSlot {
            queue: Some(Arc::clone(&queue)),
        };
        match sender.send(slot) {
            Ok(()) => return,
            // The waiter left: the place goes to the next one.
            Err(mut slot) => slot.queue = None,
        }
    }
}

// ---------------------------------------------------------------------------------------
// Scratch files
// ---------------------------------------------------------------------------------------

struct ScratchEntry {
    serial: u64,
    class: FpcalcClass,
    path: PathBuf,
    bytes: AtomicU64,
    /// A heart's sample took this one's place: its writer stops and its file is gone.
    cancelled: AtomicBool,
}

#[derive(Default)]
struct ScratchState {
    entries: Vec<Arc<ScratchEntry>>,
    serial: u64,
}

struct ScratchShared {
    folder: PathBuf,
    max_files: usize,
    max_bytes: u64,
    state: Mutex<ScratchState>,
}

impl ScratchShared {
    fn used_bytes(&self) -> u64 {
        self.state
            .lock()
            .entries
            .iter()
            .map(|e| e.bytes.load(Ordering::SeqCst))
            .sum()
    }
}

/// A file under the scratch folder, within its cap, deleted when this drops.
pub struct ScratchFile {
    entry: Arc<ScratchEntry>,
    shared: Arc<ScratchShared>,
}

impl ScratchFile {
    pub fn path(&self) -> &Path {
        &self.entry.path
    }

    /// Takes room for `more` bytes under the scratch folder's cap; false when it would pass it,
    /// or when a heart's sample has taken this file's place.
    pub fn reserve(&self, more: u64) -> bool {
        if self.is_cancelled() {
            return false;
        }
        let state = self.shared.state.lock();
        let used: u64 = state.entries.iter().map(|e| e.bytes.load(Ordering::SeqCst)).sum();
        if used + more > self.shared.max_bytes {
            return false;
        }
        self.entry.bytes.fetch_add(more, Ordering::SeqCst);
        true
    }

    /// Whether a heart's sample took this file's place.
    pub fn is_cancelled(&self) -> bool {
        self.entry.cancelled.load(Ordering::SeqCst)
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.entry.path);
        self.shared
            .state
            .lock()
            .entries
            .retain(|e| e.serial != self.entry.serial);
    }
}

// ---------------------------------------------------------------------------------------
// The store of prints
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexEntry {
    last_heard: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    video: Option<String>,
    whole: bool,
}

#[derive(Default)]
struct SampleIndex {
    entries: HashMap<String, IndexEntry>,
}

// ---------------------------------------------------------------------------------------
// Shim rounds
// ---------------------------------------------------------------------------------------

/// One held heart's wait on the shim: its hold, whose it is, and the video they heard.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ShimWait {
    hold_key: String,
    user: String,
    video: String,
}

#[derive(Default)]
struct Rounds {
    waiting: VecDeque<ShimWait>,
    running: bool,
    again: bool,
    /// A play's stream succeeded since the last round began.
    stream_seen: bool,
    /// Counts the rounds whose probe proved the shim works.
    epoch: u64,
    /// The epoch at which a video first answered gone, while that answer stands.
    first_gone: HashMap<String, u64>,
    /// Videos that answered gone twice with a proof between.
    gone: HashSet<String>,
}

/// What topping up a short or missing heard print came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopUp {
    /// A whole print is stored under this sample id.
    Ready(String),
    /// The shim did not answer for the heard video.
    Busy,
    /// The heard audio is gone.
    Gone,
}

/// What printing the heard audio over the whole song came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WholePrint {
    Ready(Vec<u32>),
    Busy,
    Gone,
}

/// A local file of the heard audio: one already there (a copy, a library file), or one fetched
/// into a scratch file, deleted when this drops.
pub enum HeardFile {
    Held(PathBuf),
    Fetched(ScratchFile),
}

impl HeardFile {
    pub fn path(&self) -> &Path {
        match self {
            HeardFile::Held(path) => path,
            HeardFile::Fetched(scratch) => scratch.path(),
        }
    }
}

/// What asking for a local file of the heard audio came to.
pub enum HeardFetch {
    Ready(HeardFile),
    Busy,
    Gone,
}

/// Called with a hold's key when the shim answered for what it waits on.
pub type ResumeHold = Arc<dyn Fn(&str) + Send + Sync>;
/// Called with a hold that moved, so its downloads row says so.
pub type HoldRow = Arc<dyn Fn(&HeldAcquisition) + Send + Sync>;

/// What a heard-sample store is built from.
pub struct HeardSampleParts {
    /// Where the prints are kept, under Octo's config folder.
    pub folder: PathBuf,
    /// Where scratch files wait, beside the preview copies.
    pub scratch: PathBuf,
    pub settings: Arc<SettingsStore>,
    pub registry: Arc<ExternalIdRegistry>,
    pub youtube: Arc<YouTubeResolver>,
    pub copies: Arc<PreviewCopies>,
    pub prefetch: Arc<PrefetchStore>,
    pub holds: Arc<SoulseekHoldStore>,
    pub clock: Clock,
}

/// The heard samples, the `fpcalc` queue, the scratch folder and the shim's rounds.
pub struct HeardSamples {
    folder: PathBuf,
    settings: Arc<SettingsStore>,
    registry: Arc<ExternalIdRegistry>,
    youtube: Arc<YouTubeResolver>,
    copies: Arc<PreviewCopies>,
    prefetch: Arc<PrefetchStore>,
    holds: Arc<SoulseekHoldStore>,
    clock: Clock,
    printer: Mutex<Arc<dyn Printer>>,
    slots: Arc<Mutex<SlotQueue>>,
    scratch: Arc<ScratchShared>,
    index: Mutex<SampleIndex>,
    max_samples: Mutex<usize>,
    /// Samples a play holds while its acquisition runs, by holder.
    play_held: Mutex<HashMap<String, HashSet<String>>>,
    rounds: Mutex<Rounds>,
    /// Ends each round, for whoever waits on one.
    round_done: tokio::sync::Notify,
    resume: Mutex<Option<ResumeHold>>,
    row: Mutex<Option<HoldRow>>,
    /// Held while the index is written, so a save never overwrites a newer one.
    save_lock: Mutex<()>,
}

impl HeardSamples {
    pub fn new(parts: HeardSampleParts) -> Arc<Self> {
        Self::with_caps(parts, SCRATCH_FILES, SCRATCH_BYTES)
    }

    /// As `new`, with the scratch folder held under `files` files and `bytes` bytes.
    pub fn with_caps(parts: HeardSampleParts, files: usize, bytes: u64) -> Arc<Self> {
        let _ = std::fs::create_dir_all(&parts.folder);
        let _ = std::fs::create_dir_all(&parts.scratch);
        let samples = HeardSamples {
            folder: parts.folder,
            settings: parts.settings,
            registry: parts.registry,
            youtube: parts.youtube,
            copies: parts.copies,
            prefetch: parts.prefetch,
            holds: parts.holds,
            clock: parts.clock,
            printer: Mutex::new(Arc::new(AudioFingerprinter::new())),
            slots: Arc::new(Mutex::new(SlotQueue {
                free: FPCALC_AT_ONCE,
                waiting: VecDeque::new(),
            })),
            scratch: Arc::new(ScratchShared {
                folder: parts.scratch,
                max_files: files,
                max_bytes: bytes,
                state: Mutex::new(ScratchState::default()),
            }),
            index: Mutex::new(SampleIndex::default()),
            max_samples: Mutex::new(MAX_SAMPLES),
            play_held: Mutex::new(HashMap::new()),
            rounds: Mutex::new(Rounds::default()),
            round_done: tokio::sync::Notify::new(),
            resume: Mutex::new(None),
            row: Mutex::new(None),
            save_lock: Mutex::new(()),
        };
        samples.load();
        samples.sweep_scratch();
        Arc::new(samples)
    }

    /// Prints with `printer` from now on, in place of `fpcalc`.
    pub fn use_printer(&self, printer: Arc<dyn Printer>) {
        *self.printer.lock() = printer;
    }

    /// Keeps at most `max` samples from now on.
    pub fn set_max_samples(&self, max: usize) {
        *self.max_samples.lock() = max;
    }

    /// Resumes a hold when the shim answered for what it waits on.
    pub fn on_resume(&self, resume: ResumeHold) {
        *self.resume.lock() = Some(resume);
    }

    /// Shows a hold that moved on its downloads row.
    pub fn on_hold_row(&self, row: HoldRow) {
        *self.row.lock() = Some(row);
    }

    /// What prints audio files: `fpcalc`, or a test's printer.
    pub(crate) fn printer(&self) -> Arc<dyn Printer> {
        self.printer.lock().clone()
    }

    fn timeout_seconds(&self) -> i32 {
        self.settings
            .current()
            .soulseek
            .effective_fingerprint_timeout_seconds()
    }

    /// How long one `fpcalc` run or one decode of the heard audio may take.
    pub fn fingerprint_timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_seconds().max(1) as u64)
    }

    // ---- The store ---------------------------------------------------------------------

    fn load(&self) {
        let index: HashMap<String, IndexEntry> = std::fs::read(self.folder.join(INDEX))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let kept: HashMap<String, IndexEntry> = index
            .into_iter()
            .filter(|(id, _)| self.print_path(id).is_file())
            .collect();
        self.index.lock().entries = kept;
    }

    fn print_path(&self, sample_id: &str) -> PathBuf {
        self.folder.join(format!("{sample_id}.json"))
    }

    fn persist(&self) {
        let _one = self.save_lock.lock();
        let entries = self.index.lock().entries.clone();
        let Ok(json) = serde_json::to_vec(&entries) else {
            return;
        };
        let temp = self.folder.join(format!("{INDEX}.tmp"));
        if std::fs::write(&temp, json).is_ok() {
            let _ = std::fs::rename(&temp, self.folder.join(INDEX));
        }
    }

    /// The stored sample `sample_id`'s prints.
    pub fn prints(&self, sample_id: &str) -> Option<HeardPrints> {
        if !self.index.lock().entries.contains_key(sample_id) {
            return None;
        }
        let bytes = std::fs::read(self.print_path(sample_id)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Stores `prints` as sample `sample_id`, unless a whole print is there and these are short:
    /// a short sample never replaces a whole one. Either way the sample counts as heard now.
    /// True when these prints were kept.
    pub fn store(&self, sample_id: &str, prints: &HeardPrints, video: Option<&str>) -> bool {
        let now = self.clock.now();
        let keeps_whole = {
            let mut index = self.index.lock();
            match index.entries.get_mut(sample_id) {
                Some(known) if known.whole && !prints.whole => {
                    known.last_heard = now;
                    true
                }
                _ => false,
            }
        };
        if keeps_whole {
            self.persist();
            return false;
        }
        let Ok(json) = serde_json::to_vec(prints) else {
            return false;
        };
        if std::fs::write(self.print_path(sample_id), json).is_err() {
            return false;
        }
        self.index.lock().entries.insert(
            sample_id.to_string(),
            IndexEntry {
                last_heard: now,
                video: video.map(str::to_string),
                whole: prints.whole,
            },
        );
        self.evict();
        self.persist();
        true
    }

    /// The least recently heard samples beyond the cap go, never one a hold or a play holds.
    fn evict(&self) {
        let max = *self.max_samples.lock();
        let live = self.live_samples();
        let gone: Vec<String> = {
            let mut index = self.index.lock();
            if index.entries.len() <= max {
                return;
            }
            let mut by_age: Vec<(String, DateTime<Utc>)> = index
                .entries
                .iter()
                .filter(|(id, _)| !live.contains(*id))
                .map(|(id, e)| (id.clone(), e.last_heard))
                .collect();
            by_age.sort_by_key(|(_, at)| *at);
            let excess = index.entries.len() - max;
            let gone: Vec<String> = by_age.into_iter().take(excess).map(|(id, _)| id).collect();
            for id in &gone {
                index.entries.remove(id);
            }
            gone
        };
        for id in gone {
            let _ = std::fs::remove_file(self.print_path(&id));
        }
    }

    /// Every sample a hold's holder or a running play names.
    fn live_samples(&self) -> HashSet<String> {
        let mut live: HashSet<String> = self
            .holds
            .snapshot()
            .iter()
            .flat_map(|record| record.holders.iter())
            .filter_map(|holder| holder.heard.as_ref())
            .flat_map(|heard| {
                heard
                    .sample_id
                    .clone()
                    .into_iter()
                    .chain(std::iter::once(sample_id_of(&heard.source)))
            })
            .collect();
        for held in self.play_held.lock().values() {
            live.extend(held.iter().cloned());
        }
        live
    }

    /// A play holds its sample while its acquisition runs, so the store keeps it.
    pub fn hold_sample(&self, sample_id: &str, holder: &str) {
        self.play_held
            .lock()
            .entry(holder.to_string())
            .or_default()
            .insert(sample_id.to_string());
    }

    /// The play's acquisition ended, whatever its outcome.
    pub fn release_samples(&self, holder: &str) {
        self.play_held.lock().remove(holder);
    }

    /// How many samples are stored.
    pub fn count(&self) -> usize {
        self.index.lock().entries.len()
    }

    /// The videos of the most recently heard samples, newest first.
    fn recent_videos(&self, limit: usize, except: &HashSet<String>) -> Vec<String> {
        let index = self.index.lock();
        let mut heard: Vec<(&String, DateTime<Utc>)> = index
            .entries
            .values()
            .filter_map(|e| e.video.as_ref().map(|v| (v, e.last_heard)))
            .filter(|(video, _)| !except.contains(*video))
            .collect();
        heard.sort_by_key(|(_, at)| std::cmp::Reverse(*at));
        let mut videos: Vec<String> = Vec::new();
        for (video, _) in heard {
            if !videos.contains(video) {
                videos.push(video.clone());
            }
            if videos.len() == limit {
                break;
            }
        }
        videos
    }

    // ---- The fpcalc queue and the scratch folder ---------------------------------------

    /// Waits for one of the `fpcalc` places, hearts first; dropping it frees it.
    pub async fn slot(&self, class: FpcalcClass) -> FpcalcSlot {
        let receiver = {
            let mut q = self.slots.lock();
            if q.free > 0 {
                q.free -= 1;
                return FpcalcSlot {
                    queue: Some(Arc::clone(&self.slots)),
                };
            }
            let (sender, receiver) = oneshot::channel();
            q.waiting.push_back((class, sender));
            receiver
        };
        match receiver.await {
            Ok(slot) => slot,
            // Never dropped while a waiter waits: the queue owns its senders.
            Err(_) => FpcalcSlot { queue: None },
        }
    }

    /// A file under the scratch folder within its cap (its files and its bytes), deleted when it
    /// drops. A heart's takes the newest play sample's place when the folder is full; None when
    /// there is no room.
    pub fn scratch(&self, class: FpcalcClass, extension: &str) -> Option<ScratchFile> {
        let shared = &self.scratch;
        let entry = {
            let mut state = shared.state.lock();
            if state.entries.len() >= shared.max_files {
                if class != FpcalcClass::Heart {
                    return None;
                }
                let newest = state
                    .entries
                    .iter()
                    .filter(|e| e.class == FpcalcClass::Other)
                    .max_by_key(|e| e.serial)
                    .cloned()?;
                newest.cancelled.store(true, Ordering::SeqCst);
                let _ = std::fs::remove_file(&newest.path);
                state.entries.retain(|e| e.serial != newest.serial);
            }
            state.serial += 1;
            let extension = extension.trim_start_matches('.');
            let name = if extension.is_empty() {
                format!("{}-{}", std::process::id(), state.serial)
            } else {
                format!("{}-{}.{extension}", std::process::id(), state.serial)
            };
            let entry = Arc::new(ScratchEntry {
                serial: state.serial,
                class,
                path: shared.folder.join(name),
                bytes: AtomicU64::new(0),
                cancelled: AtomicBool::new(false),
            });
            state.entries.push(Arc::clone(&entry));
            entry
        };
        Some(ScratchFile {
            entry,
            shared: Arc::clone(shared),
        })
    }

    /// How many scratch files wait, and their bytes.
    pub fn scratch_use(&self) -> (usize, u64) {
        let files = self.scratch.state.lock().entries.len();
        (files, self.scratch.used_bytes())
    }

    /// The scratch folder.
    pub fn scratch_folder(&self) -> &Path {
        &self.scratch.folder
    }

    /// At start nothing waits in the scratch folder: every file there is a crash's leftover.
    fn sweep_scratch(&self) {
        let Ok(listing) = std::fs::read_dir(&self.scratch.folder) else {
            return;
        };
        for item in listing.flatten() {
            let path = item.path();
            if path.is_file() {
                debug!("Removing a scratch file no hold names: {}", path.display());
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    // ---- Printing ------------------------------------------------------------------------

    /// Prints `path` (both prints) in one `fpcalc` place.
    async fn print_file(
        &self,
        path: &Path,
        class: FpcalcClass,
        reaches_end: bool,
        song_seconds: Option<i32>,
    ) -> Option<HeardPrints> {
        let _slot = self.slot(class).await;
        let printer = self.printer();
        let timeout = self.timeout_seconds();
        let raw = printer.raw(path, SAMPLE_SECONDS, timeout).await?;
        let (compressed, decoded) = printer.compressed(path, SAMPLE_SECONDS, timeout).await?;
        let seconds = if raw.seconds > 0.0 {
            raw.seconds.round() as i32
        } else {
            decoded
        };
        // A file longer than the sample's cap is printed to the cap: the print stops short of
        // its end.
        let reaches_end = reaches_end && seconds <= SAMPLE_SECONDS;
        let whole = covers_whole(raw.items.len(), seconds, reaches_end, song_seconds);
        Some(HeardPrints {
            raw: raw.items,
            compressed,
            seconds,
            whole,
            reaches_end,
        })
    }

    /// Prints a sample, stores it, and keeps the sample's id on the person's heard record.
    async fn finish_sample(&self, path: &Path, capture: &Capture, reaches_end: bool) {
        let Some(prints) = self
            .print_file(path, capture.class, reaches_end, capture.song_seconds)
            .await
        else {
            debug!(
                "fpcalc could not print the heard sample of {}",
                capture.external_id
            );
            return;
        };
        let sample_id = sample_id_of(&capture.record.source);
        self.store(&sample_id, &prints, capture.record.source.video_id());
        self.note_sample(capture, &sample_id);
    }

    /// The person's heard record names its sample, found by the song's id now.
    fn note_sample(&self, capture: &Capture, sample_id: &str) {
        let record = HeardRecord {
            sample_id: Some(sample_id.to_string()),
            ..capture.record.clone()
        };
        self.registry
            .remember_heard(&capture.external_id, &capture.user, record, None);
    }

    /// Prints a held file of the heard audio (a copy, a library file) as the person's sample, off
    /// the request.
    pub fn sample_file(self: &Arc<Self>, path: PathBuf, capture: Capture) {
        let samples = Arc::clone(self);
        tokio::spawn(async move {
            samples.finish_sample(&path, &capture, true).await;
        });
    }

    /// The stream's bytes, also written to a scratch file until the song ends or `capture.limit`
    /// bytes are there; then printed as the person's sample. A stream over the scratch cap is
    /// sent on untouched.
    pub fn capture(self: &Arc<Self>, body: AudioStream, capture: Capture, extension: &str) -> AudioStream {
        let Some(scratch) = self.scratch(capture.class, extension) else {
            debug!(
                "The scratch folder is full; no heard sample of {} is taken",
                capture.external_id
            );
            return body;
        };
        let (sender, receiver) = mpsc::unbounded_channel();
        let samples = Arc::clone(self);
        let limit = capture.limit;
        let total = capture.total;
        tokio::spawn(async move {
            samples.write_sample(scratch, receiver, capture).await;
        });
        Box::pin(Tee {
            body,
            sender: Some(sender),
            kept: 0,
            seen: 0,
            limit,
            total,
        })
    }

    async fn write_sample(
        &self,
        scratch: ScratchFile,
        mut receiver: mpsc::UnboundedReceiver<Piece>,
        capture: Capture,
    ) {
        let Ok(mut file) = tokio::fs::File::create(scratch.path()).await else {
            return;
        };
        let mut written = 0u64;
        let mut ended = false;
        let mut full = false;
        while let Some(piece) = receiver.recv().await {
            match piece {
                Piece::Bytes(bytes) => {
                    if full || scratch.is_cancelled() {
                        continue;
                    }
                    if !scratch.reserve(bytes.len() as u64) || file.write_all(&bytes).await.is_err() {
                        full = true;
                        continue;
                    }
                    written += bytes.len() as u64;
                }
                Piece::End { all } => ended = all,
            }
        }
        if file.flush().await.is_err() || written == 0 || scratch.is_cancelled() {
            return;
        }
        drop(file);
        let reaches_end = ended && !full && capture.total.is_some_and(|total| total == written);
        self.finish_sample(scratch.path(), &capture, reaches_end).await;
        drop(scratch);
    }

    // ---- The heard audio itself ----------------------------------------------------------

    /// A local file of the heard audio: the held file for a copy or a library file; for a video,
    /// its whole preview copy at the heard size, else one fresh fetch of it on the shim's
    /// background lane into a scratch file of at most `cap` bytes (the heard format first when
    /// known, then the current one). Busy while the shim does not answer; Gone once the heard
    /// audio is known gone.
    pub async fn heard_file(&self, heard: &HeardRecord, cap: u64) -> HeardFetch {
        match &heard.source {
            HeardSource::Copy {
                key,
                ready_at,
                source_file,
            } => match self.prefetch.copy_of(key, *ready_at, source_file.as_deref()) {
                Some(path) => HeardFetch::Ready(HeardFile::Held(path)),
                None => HeardFetch::Gone,
            },
            HeardSource::Library { path } | HeardSource::Acquired { path } => {
                if Path::new(path).is_file() {
                    HeardFetch::Ready(HeardFile::Held(PathBuf::from(path)))
                } else {
                    HeardFetch::Gone
                }
            }
            HeardSource::Video {
                id,
                size,
                content_type,
                itag,
            } => {
                if let Some(path) = self.copies.copy_path_of(id, *size, Some(content_type)) {
                    return HeardFetch::Ready(HeardFile::Held(path));
                }
                if self.is_gone(id) {
                    return HeardFetch::Gone;
                }
                let mut answer = self.fetch_fresh(id, *itag, cap).await;
                if itag.is_some() && matches!(answer, Err(Failure::Gone | Failure::Refused)) {
                    // Only the very file needs the heard format; its sound is the same master in
                    // the video's current one.
                    answer = self.fetch_fresh(id, None, cap).await;
                }
                match answer {
                    Ok(file) => HeardFetch::Ready(HeardFile::Fetched(file)),
                    Err(Failure::Gone | Failure::Refused) => {
                        if self.gone_answer(id) {
                            HeardFetch::Gone
                        } else {
                            HeardFetch::Busy
                        }
                    }
                    Err(_) => HeardFetch::Busy,
                }
            }
        }
    }

    /// One fresh fetch of a video's stream into a scratch file of at most `cap` bytes.
    async fn fetch_fresh(&self, video: &str, itag: Option<u32>, cap: u64) -> Result<ScratchFile, Failure> {
        let opened = self.youtube.open_stream_fresh(video, None, itag).await?;
        let Some(scratch) = self.scratch(FpcalcClass::Heart, extension_of(&opened.content_type)) else {
            return Err(Failure::Busy);
        };
        match save_stream(opened, &scratch, cap).await {
            Ok(()) => Ok(scratch),
            Err(()) => Err(Failure::Busy),
        }
    }

    /// Makes a whole print of the heard audio from the heard audio itself: the held file, or for
    /// a video one fresh fetch.
    pub async fn top_up(&self, heard: &HeardRecord, song_seconds: Option<i32>) -> TopUp {
        let sample_id = sample_id_of(&heard.source);
        if self.prints(&sample_id).is_some_and(|prints| prints.whole) {
            return TopUp::Ready(sample_id);
        }
        let cap = sample_cap(None, song_seconds);
        let file = match self.heard_file(heard, cap).await {
            HeardFetch::Ready(file) => file,
            HeardFetch::Busy => return TopUp::Busy,
            HeardFetch::Gone => return TopUp::Gone,
        };
        let held = matches!(file, HeardFile::Held(_));
        let Some(mut prints) = self
            .print_file(file.path(), FpcalcClass::Heart, held, song_seconds)
            .await
        else {
            return TopUp::Gone;
        };
        if !held {
            let fetched = std::fs::metadata(file.path()).map(|m| m.len()).unwrap_or(0);
            prints.reaches_end = fetched < cap;
            prints.whole = covers_whole(prints.raw.len(), prints.seconds, prints.reaches_end, song_seconds);
        }
        self.store(&sample_id, &prints, heard.source.video_id());
        if prints.whole {
            TopUp::Ready(sample_id)
        } else {
            TopUp::Gone
        }
    }

    /// The raw items of the first `length_seconds` of `path` (0: all of it), in one `fpcalc`
    /// place of `class`.
    pub async fn raw_print(&self, path: &Path, length_seconds: i32, class: FpcalcClass) -> Option<Vec<u32>> {
        let _slot = self.slot(class).await;
        self.printer()
            .raw(path, length_seconds, self.timeout_seconds())
            .await
            .map(|raw| raw.items)
    }

    /// The compressed print of the first `length_seconds` of `path`, what an AcoustID lookup
    /// sends, and the seconds decoded, in one `fpcalc` place of `class`.
    pub async fn compressed_print(
        &self,
        path: &Path,
        length_seconds: i32,
        class: FpcalcClass,
    ) -> Option<(String, i32)> {
        let _slot = self.slot(class).await;
        self.printer()
            .compressed(path, length_seconds, self.timeout_seconds())
            .await
            .filter(|(print, _)| !print.is_empty())
    }

    /// The heard audio's raw print over the whole song (`fpcalc -length 0`), from the held file,
    /// else one fresh fetch.
    pub async fn whole_song_print(&self, heard: &HeardRecord) -> WholePrint {
        let file = match self.heard_file(heard, WHOLE_SONG_CAP_BYTES).await {
            HeardFetch::Ready(file) => file,
            HeardFetch::Busy => return WholePrint::Busy,
            HeardFetch::Gone => return WholePrint::Gone,
        };
        let _slot = self.slot(FpcalcClass::Heart).await;
        match self.printer().raw(file.path(), 0, self.timeout_seconds()).await {
            Some(raw) => WholePrint::Ready(raw.items),
            None => WholePrint::Gone,
        }
    }

    // ---- Copies a hold keeps --------------------------------------------------------------

    /// Holds the copy of what was heard (a video's whole copy, a song fetched ahead) under
    /// `holder`; false when there is no copy.
    pub fn hold_heard(&self, source: &HeardSource, holder: &str) -> bool {
        match source {
            HeardSource::Video { id, .. } => self.copies.hold(id, holder),
            HeardSource::Copy { key, .. } => self.prefetch.hold(key, holder),
            HeardSource::Library { .. } | HeardSource::Acquired { .. } => false,
        }
    }

    /// Lets go of `holder`'s hold on the copy of what was heard.
    pub fn release_heard(&self, source: &HeardSource, holder: &str) {
        match source {
            HeardSource::Video { id, .. } => self.copies.release(id, holder),
            HeardSource::Copy { key, .. } => self.prefetch.release(key, holder),
            HeardSource::Library { .. } | HeardSource::Acquired { .. } => {}
        }
    }

    /// At start: every saved copy holder id no live hold names goes, and every live hold's
    /// holders hold their copies again (a song fetched ahead keeps its holds in memory only).
    pub fn rehold_copies(&self) {
        let mut live = HashSet::new();
        for record in self.holds.snapshot() {
            for holder in &record.holders {
                let id = record.copy_holder_id(&holder.user);
                if let Some(heard) = &holder.heard {
                    self.hold_heard(&heard.source, &id);
                }
                live.insert(id);
            }
        }
        self.copies.release_holders_except(&live);
    }

    // ---- The shim's rounds ----------------------------------------------------------------

    /// Whether the heard video answered gone twice, with a working-shim proof between.
    pub fn is_gone(&self, video: &str) -> bool {
        self.rounds.lock().gone.contains(video)
    }

    /// The shim answered gone (410, 403 or 404) for `video`: true when that makes it gone, which
    /// needs an earlier gone answer and a working-shim proof since.
    pub fn gone_answer(&self, video: &str) -> bool {
        let mut rounds = self.rounds.lock();
        if rounds.gone.contains(video) {
            return true;
        }
        let epoch = rounds.epoch;
        match rounds.first_gone.get(video) {
            Some(first) if *first < epoch => {
                rounds.gone.insert(video.to_string());
                true
            }
            Some(_) => false,
            None => {
                rounds.first_gone.insert(video.to_string(), epoch);
                false
            }
        }
    }

    /// Joins a hold that waits on the shim for `user`'s heard video to the one round queue. A
    /// gone answer saved with the hold counts as the first.
    pub fn wait_for_shim(&self, hold_key: &str, user: &str, video_id: &str) {
        let gone_once = self.holds.get(hold_key).is_some_and(|record| {
            record
                .holders
                .iter()
                .any(|h| h.user.trim().eq_ignore_ascii_case(user.trim()) && h.shim_tries.gone_once)
        });
        let wait = ShimWait {
            hold_key: hold_key.to_string(),
            user: user.trim().to_string(),
            video: video_id.to_string(),
        };
        let mut rounds = self.rounds.lock();
        if gone_once && !rounds.first_gone.contains_key(video_id) {
            let epoch = rounds.epoch;
            rounds.first_gone.insert(video_id.to_string(), epoch);
        }
        if !rounds
            .waiting
            .iter()
            .any(|w| w.hold_key == wait.hold_key && w.user.eq_ignore_ascii_case(&wait.user))
        {
            rounds.waiting.push_back(wait);
        }
    }

    /// How many holds wait on the shim.
    pub fn shim_waits(&self) -> usize {
        self.rounds.lock().waiting.len()
    }

    /// Every hold that waits on the shim joins the queue again, as at Octo's start.
    pub fn rejoin_holds(&self) {
        for record in self.holds.snapshot() {
            if record.waiting_for != Some(WaitingFor::Shim) {
                continue;
            }
            for holder in &record.holders {
                if let Some(video) = holder.heard.as_ref().and_then(|h| h.source.video_id()) {
                    self.wait_for_shim(&record.key(), &holder.user, video);
                }
            }
        }
    }

    /// A play or heart of a held song, Octo's start, or a play's first `/stream` success: one
    /// round runs, and events that arrive while it runs fold into one more.
    pub fn shim_event(self: &Arc<Self>) {
        {
            let mut rounds = self.rounds.lock();
            if rounds.waiting.is_empty() {
                return;
            }
            if rounds.running {
                rounds.again = true;
                return;
            }
            rounds.running = true;
        }
        let samples = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                samples.round().await;
                let mut rounds = samples.rounds.lock();
                if !rounds.again || rounds.waiting.is_empty() {
                    rounds.running = false;
                    rounds.again = false;
                    break;
                }
                rounds.again = false;
            }
            samples.round_done.notify_waiters();
        });
    }

    /// A play's stream from the shim succeeded: a hint that YouTube answers, which starts a round
    /// once per round.
    pub fn stream_ok(self: &Arc<Self>) {
        let first = {
            let mut rounds = self.rounds.lock();
            if rounds.waiting.is_empty() {
                return;
            }
            !std::mem::replace(&mut rounds.stream_seen, true)
        };
        if first {
            self.shim_event();
        }
    }

    /// Waits until no round runs.
    pub async fn rounds_settled(&self) {
        loop {
            let notified = self.round_done.notified();
            if !self.rounds.lock().running {
                return;
            }
            notified.await;
        }
    }

    /// The videos a round proves the shim with: the most recently heard other videos, else
    /// videos pinned to songs, else the fixed two.
    fn probe_videos(&self, held: &HashSet<String>) -> Vec<String> {
        let mut videos = self.recent_videos(PROBE_RUNS, held);
        if videos.is_empty() {
            videos = self.registry.pinned_videos(PROBE_RUNS, held);
        }
        if videos.is_empty() {
            videos = FIXED_PROBES.iter().map(|v| v.to_string()).collect();
        }
        videos.truncate(PROBE_RUNS);
        videos
    }

    /// One round: a proof that the shim works, then the held videos one at a time, oldest
    /// first, stopping at the first busy answer.
    async fn round(self: &Arc<Self>) {
        let held: HashSet<String> = {
            let mut rounds = self.rounds.lock();
            rounds.stream_seen = false;
            rounds.waiting.iter().map(|w| w.video.clone()).collect()
        };
        if held.is_empty() {
            return;
        }
        let mut proven = false;
        for video in self.probe_videos(&held) {
            if matches!(self.youtube.facts_fresh(&video).await, Lookup::Found(_)) {
                proven = true;
                break;
            }
        }
        if !proven {
            debug!("The shim's round found no proof that it works; the held hearts wait");
            return;
        }
        let order: Vec<ShimWait> = {
            let mut rounds = self.rounds.lock();
            rounds.epoch += 1;
            rounds.waiting.iter().cloned().collect()
        };
        for wait in order {
            if !self.rounds.lock().waiting.contains(&wait) {
                continue;
            }
            match self.youtube.facts_fresh(&wait.video).await {
                Lookup::Found(_) => {
                    self.leave_queue(&wait);
                    self.rounds.lock().first_gone.remove(&wait.video);
                    self.set_tries(&wait, |tries| *tries = Default::default());
                    info!("The shim answered for {}; {} resumes", wait.video, wait.hold_key);
                    self.resume(&wait.hold_key);
                }
                Lookup::Failed(Failure::Busy) => {
                    let busy = self.set_tries(&wait, |tries| tries.busy_rounds += 1);
                    {
                        let mut rounds = self.rounds.lock();
                        rounds.waiting.retain(|w| w != &wait);
                        if busy < STUCK_ROUNDS {
                            rounds.waiting.push_back(wait.clone());
                        }
                    }
                    if busy >= STUCK_ROUNDS {
                        self.stuck(&wait);
                    }
                    break;
                }
                Lookup::Failed(Failure::Gone | Failure::Refused) => {
                    if self.gone_answer(&wait.video) {
                        self.leave_queue(&wait);
                        info!("{} is gone; {} resumes without it", wait.video, wait.hold_key);
                        self.resume(&wait.hold_key);
                    } else {
                        self.set_tries(&wait, |tries| tries.gone_once = true);
                    }
                }
                Lookup::Failed(_) | Lookup::None => {
                    debug!("The shim stopped answering during a round; the rest wait");
                    break;
                }
            }
        }
    }

    fn leave_queue(&self, wait: &ShimWait) {
        self.rounds.lock().waiting.retain(|w| w != wait);
    }

    fn resume(&self, hold_key: &str) {
        let resume = self.resume.lock().clone();
        if let Some(resume) = resume {
            resume(hold_key);
        }
    }

    /// Changes the shim's count on the waiting holder and saves it; answers its busy rounds.
    fn set_tries(&self, wait: &ShimWait, change: impl FnOnce(&mut ShimTries)) -> u32 {
        let mut busy = 0;
        let mut change = Some(change);
        self.holds.update(&wait.hold_key, |record| {
            if let Some(holder) = record
                .holders
                .iter_mut()
                .find(|h| h.user.trim().eq_ignore_ascii_case(&wait.user))
                && let Some(change) = change.take()
            {
                change(&mut holder.shim_tries);
                busy = holder.shim_tries.busy_rounds;
            }
        });
        busy
    }

    /// A heard video that stayed busy for [`STUCK_ROUNDS`] rounds with a working shim: its fetch
    /// attempt ends and the person's heart waits for a new play, keeping any copy it holds.
    fn stuck(&self, wait: &ShimWait) {
        let Some(record) = self.holds.get(&wait.hold_key) else {
            return;
        };
        let Some(mut holder) = record
            .holders
            .iter()
            .find(|h| h.user.trim().eq_ignore_ascii_case(&wait.user))
            .cloned()
        else {
            return;
        };
        holder.shim_tries = Default::default();
        info!(
            "{} answered busy in {STUCK_ROUNDS} rounds; {} waits for a new play",
            wait.video, wait.hold_key
        );
        // A keep they asked for goes with them, so the new play keeps what it hears.
        let user = holder.user.trim().to_string();
        let asked = record
            .keep_requested
            .iter()
            .any(|asker| asker.trim().eq_ignore_ascii_case(&user));
        // The files of a hold nobody else waits in go with the last holder.
        let ended = self.holds.release_holder(&wait.hold_key, &wait.user).is_none();
        let held_files = if ended {
            record.held_files.clone()
        } else {
            Vec::new()
        };
        let moved = self.holds.hold(HeldAcquisition {
            waiting_for: Some(WaitingFor::NotYetProven),
            holders: vec![holder],
            held_files,
            keep_requested: Vec::new(),
            line: Some(STUCK_LINE.to_string()),
            ..record
        });
        let moved = self
            .holds
            .update(&moved.key(), |r| {
                r.line = Some(STUCK_LINE.to_string());
                if asked
                    && !r
                        .keep_requested
                        .iter()
                        .any(|asker| asker.trim().eq_ignore_ascii_case(&user))
                {
                    r.keep_requested.push(user.clone());
                }
            })
            .unwrap_or(moved);
        let row = self.row.lock().clone();
        if let Some(row) = row {
            row(&moved);
        }
    }
}

/// What a heard sample is taken for: whose play of which song, what played, how many bytes to
/// keep, the stream's whole size and the song's length when known, and whose `fpcalc` run it is.
#[derive(Debug, Clone)]
pub struct Capture {
    pub external_id: String,
    pub user: String,
    pub record: HeardRecord,
    pub limit: u64,
    pub total: Option<u64>,
    pub song_seconds: Option<i32>,
    pub class: FpcalcClass,
}

/// How many bytes a sample keeps: 240 seconds at the stream's rate when its size and the song's
/// length are known, else [`SAMPLE_CAP_BYTES`].
pub fn sample_cap(size: Option<u64>, song_seconds: Option<i32>) -> u64 {
    match (size, song_seconds) {
        (Some(size), Some(seconds)) if seconds > 0 && size > 0 => {
            let at_rate = size.saturating_mul(SAMPLE_SECONDS as u64) / seconds as u64;
            at_rate.max(1)
        }
        _ => SAMPLE_CAP_BYTES,
    }
}

/// A file extension for a stream's content type.
fn extension_of(content_type: &str) -> &'static str {
    if content_type.contains("webm") {
        "webm"
    } else if content_type.contains("mpeg") {
        "mp3"
    } else {
        "m4a"
    }
}

/// Writes a fresh stream into a scratch file, at most `cap` bytes, giving up when it stalls.
async fn save_stream(opened: ShimStream, scratch: &ScratchFile, cap: u64) -> Result<(), ()> {
    let mut file = tokio::fs::File::create(scratch.path()).await.map_err(|_| ())?;
    let mut body = opened.response.bytes_stream();
    let mut written = 0u64;
    while written < cap {
        let chunk = match tokio::time::timeout(FETCH_STALL, body.next()).await {
            Err(_) => return Err(()),
            Ok(None) => break,
            Ok(Some(Err(_))) => return Err(()),
            Ok(Some(Ok(chunk))) => chunk,
        };
        let room = (cap - written).min(chunk.len() as u64);
        if !scratch.reserve(room) {
            break;
        }
        file.write_all(&chunk[..room as usize]).await.map_err(|_| ())?;
        written += room;
    }
    file.flush().await.map_err(|_| ())?;
    if written == 0 {
        return Err(());
    }
    Ok(())
}

enum Piece {
    Bytes(Bytes),
    /// The stream ended on its own; `all` when every byte it sent was kept.
    End {
        all: bool,
    },
}

/// A body that sends its bytes on and hands the first `limit` of them to a sample's writer.
struct Tee {
    body: AudioStream,
    sender: Option<mpsc::UnboundedSender<Piece>>,
    kept: u64,
    seen: u64,
    limit: u64,
    total: Option<u64>,
}

impl Stream for Tee {
    type Item = std::io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let polled = self.body.as_mut().poll_next(cx);
        match &polled {
            Poll::Ready(Some(Ok(bytes))) => {
                let bytes = bytes.clone();
                self.seen += bytes.len() as u64;
                let room = self.limit.saturating_sub(self.kept);
                if room > 0 {
                    let take = (bytes.len() as u64).min(room) as usize;
                    self.kept += take as u64;
                    if let Some(sender) = &self.sender {
                        let _ = sender.send(Piece::Bytes(bytes.slice(..take)));
                    }
                }
                if self.kept >= self.limit {
                    // Enough for the sample: the writer finishes while the song plays on.
                    let all = self.total.is_some_and(|total| total == self.kept);
                    if let Some(sender) = self.sender.take() {
                        let _ = sender.send(Piece::End { all });
                    }
                }
            }
            Poll::Ready(None) => {
                let all = self.seen == self.kept;
                if let Some(sender) = self.sender.take() {
                    let _ = sender.send(Piece::End { all });
                }
            }
            Poll::Ready(Some(Err(_))) => {
                self.sender = None;
            }
            Poll::Pending => {}
        }
        polled
    }
}

#[cfg(test)]
#[path = "heard_samples_tests.rs"]
pub(crate) mod tests;
