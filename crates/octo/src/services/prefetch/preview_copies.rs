//! Whole copies of outside songs' previews. A player that reads a preview in many ranges (AVPlayer
//! steps through a long song's fragmented m4a before it plays) gets them from one copy made at
//! full speed instead of one trip to YouTube each, and the songs of a playlist kept ready start
//! from their copy at once. The bytes are the preview's own, so the size and type every listing
//! declared still hold.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};
use futures::{StreamExt, TryStreamExt};
use octo_core::common::Clock;
use octo_core::fingerprint::heart_gate::HeardSource;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::watch;
use tracing::{info, warn};

use super::store::free_bytes;
use crate::http::static_files::{RangeOutcome, parse_range};
use crate::services::i_download_service::{AudioStream, DirectStreamInfo};
use crate::services::you_tube::YouTubeResolver;
use crate::services::you_tube::you_tube_resolver::{Failure, Lookup};

/// Copies unused for this long are swept, unless a playlist kept ready holds them.
const UNUSED_FOR: Duration = Duration::hours(24);
/// The copies made for plays never take more than this; the ones used longest ago go first.
pub const CAP_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// A fill with no new bytes for this long is given up.
const FILL_STALL: StdDuration = StdDuration::from_secs(30);
/// A request waiting on a filling copy with no new bytes for this long ends its body; longer than
/// a fill waits, so a fill that is still going never cuts a reader off.
const SERVE_STALL: StdDuration = StdDuration::from_secs(35);
/// How many fills run at once.
const FILLS_AT_ONCE: usize = 4;
/// How many of those may be for playlists kept ready, so the rest are always there for plays.
const KEPT_FILLS_AT_ONCE: usize = 2;
/// A range from the first byte that ends within this is a probe.
const PROBE_BYTES: u64 = 64 * 1024;
/// The index of the copies, beside them.
const INDEX: &str = "previews.json";
/// How much one read of a copy sends at most.
const READ_CHUNK: u64 = 64 * 1024;
/// The folder under the copies that holds their FLAC stand-ins.
const FLAC_FOLDER: &str = "flac";

/// Whether `id` is a YouTube video id: eleven letters, digits, dashes or underscores. Only such ids
/// name a copy's file.
pub fn is_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Whether a request reads a preview in pieces: a range from the first byte that ends within 64 KB
/// (a probe), or any range that starts past the first byte.
pub fn asks_for_pieces(range: &str) -> bool {
    let Some(spec) = range.trim().strip_prefix("bytes=") else {
        return false;
    };
    // Several ranges in one request go to the live answer.
    if spec.contains(',') {
        return false;
    }
    let (from, to) = spec.split_once('-').unwrap_or((spec, ""));
    match from.trim().parse::<u64>() {
        Ok(0) => to.trim().parse::<u64>().is_ok_and(|end| end < PROBE_BYTES),
        Ok(_) => true,
        // A suffix range (`bytes=-N`) reads the end.
        Err(_) => from.trim().is_empty() && !to.trim().is_empty(),
    }
}

/// Whether a preview the shim answered with `status` for `range` should be copied whole: an
/// answer with audio, to a request that reads the preview in pieces.
pub fn copy_after(status: u16, range: Option<&str>) -> bool {
    matches!(status, 200 | 206) && range.is_some_and(asks_for_pieces)
}

/// What asking for a copy came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Copying {
    /// The copy is complete.
    Ready,
    /// A fill is running for it, started now or before.
    Started,
    /// Every fill it may use is taken: ask again later.
    Busy,
    /// No copy can be made: not a video id, or the disk is nearly full.
    Refused,
}

/// A preview's whole stream, as a fill reads it.
pub struct WholePreview {
    pub content_type: String,
    pub total: u64,
    pub body: AudioStream,
}

/// Where a fill reads a whole preview from.
#[async_trait]
pub trait PreviewSource: Send + Sync {
    async fn open_whole(&self, video_id: &str) -> Option<WholePreview>;

    /// Whether YouTube still serves the video, by a fresh `/facts`. A source that cannot tell
    /// says no, so a held copy is never let go on its word.
    async fn still_served(&self, _video_id: &str) -> bool {
        false
    }

    /// The video's own stream from its first byte, resolved now for this request alone at the
    /// format `itag` when given: what a heard copy is made from.
    async fn open_heard(&self, _video_id: &str, _itag: Option<u32>) -> Result<WholePreview, Failure> {
        Err(Failure::Other)
    }
}

/// The shim's stream from its first byte, which it fetches from YouTube at full speed.
#[async_trait]
impl PreviewSource for YouTubeResolver {
    async fn still_served(&self, video_id: &str) -> bool {
        matches!(self.facts_fresh(video_id).await, Lookup::Found(_))
    }

    async fn open_heard(&self, video_id: &str, itag: Option<u32>) -> Result<WholePreview, Failure> {
        let opened = self.open_stream_fresh(video_id, Some("bytes=0-"), itag).await?;
        let range = opened.content_range.as_deref();
        let total = match opened.status_code {
            200 => opened.content_length,
            206 if range.is_some_and(|r| r.trim_start().starts_with("bytes 0-")) => range
                .and_then(|r| r.rsplit_once('/'))
                .and_then(|(_, total)| total.trim().parse().ok()),
            _ => None,
        }
        .ok_or(Failure::Other)?;
        Ok(WholePreview {
            content_type: opened.content_type,
            total,
            body: Box::pin(opened.response.bytes_stream().map_err(std::io::Error::other)),
        })
    }

    async fn open_whole(&self, video_id: &str) -> Option<WholePreview> {
        let opened = self.open_stream(video_id, Some("bytes=0-")).await?;
        let range = opened.content_range.as_deref();
        let total = match opened.status_code {
            200 => opened.content_length,
            // Only a partial answer from the first byte is the whole preview.
            206 if range.is_some_and(|r| r.trim_start().starts_with("bytes 0-")) => range
                .and_then(|r| r.rsplit_once('/'))
                .and_then(|(_, total)| total.trim().parse().ok()),
            _ => None,
        }?;
        Some(WholePreview {
            content_type: opened.content_type,
            total,
            body: Box::pin(opened.response.bytes_stream().map_err(std::io::Error::other)),
        })
    }
}

/// One complete copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    size: u64,
    content_type: String,
    last_use: DateTime<Utc>,
    /// A playlist kept ready holds it until then.
    #[serde(default)]
    kept_until: Option<DateTime<Utc>>,
    /// The hearts holding it as what a person heard, by holder id. Plays' holds are in memory
    /// only.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    holders: BTreeSet<String>,
    /// The bytes of its FLAC stand-in, when one is built.
    #[serde(default, skip_serializing_if = "is_zero")]
    flac_size: u64,
}

impl Entry {
    fn kept(&self, now: DateTime<Utc>) -> bool {
        self.kept_until.is_some_and(|until| until > now)
    }

    /// The bytes it takes: the copy and its FLAC stand-in.
    fn bytes(&self) -> u64 {
        self.size + self.flac_size
    }
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// What asking for a whole copy of the heard video's own stream came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeardCopy {
    /// A whole copy with the heard format and byte size is here.
    Ready(PathBuf),
    /// The shim did not answer.
    Busy,
    /// YouTube no longer serves that video at the heard format and size.
    Gone,
}

/// Whether a holder id is a heart's, which is saved with the index; any other is a play's, kept
/// in memory only.
fn is_saved_holder(holder: &str) -> bool {
    holder.starts_with("heart:")
}

/// How far a fill has got.
#[derive(Debug, Clone, Default)]
struct Progress {
    total: Option<u64>,
    content_type: Option<String>,
    written: u64,
    /// The fill is over, whether the copy is complete or not.
    ended: bool,
}

struct Fill {
    progress: watch::Receiver<Progress>,
    kept_until: Option<DateTime<Utc>>,
}

#[derive(Default)]
struct State {
    index: HashMap<String, Entry>,
    fills: HashMap<String, Fill>,
    /// Plays holding a copy as what they heard, by video id.
    play_holders: HashMap<String, HashSet<String>>,
}

impl State {
    /// Whether anyone holds the copy as what they heard.
    fn held(&self, video_id: &str) -> bool {
        self.index
            .get(video_id)
            .is_some_and(|entry| !entry.holders.is_empty())
            || self
                .play_holders
                .get(video_id)
                .is_some_and(|holders| !holders.is_empty())
    }
}

/// The copies, in a folder of their own.
pub struct PreviewCopies {
    folder: PathBuf,
    source: Arc<dyn PreviewSource>,
    clock: Clock,
    /// No new fill starts while the disk has less free than this.
    min_free: Box<dyn Fn() -> u64 + Send + Sync>,
    cap: u64,
    state: Mutex<State>,
    /// Held from reading the index to writing it, so a save never overwrites a newer one.
    save_lock: Mutex<()>,
    /// One FLAC stand-in build at a time per video.
    flac_builds: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl PreviewCopies {
    pub fn new(
        folder: PathBuf,
        source: Arc<dyn PreviewSource>,
        clock: Clock,
        min_free: impl Fn() -> u64 + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::with_cap(folder, source, clock, min_free, CAP_BYTES)
    }

    /// As `new`, with the copies made for plays held under `cap` bytes.
    pub fn with_cap(
        folder: PathBuf,
        source: Arc<dyn PreviewSource>,
        clock: Clock,
        min_free: impl Fn() -> u64 + Send + Sync + 'static,
        cap: u64,
    ) -> Arc<Self> {
        let copies = PreviewCopies {
            folder,
            source,
            clock,
            min_free: Box::new(min_free),
            cap,
            state: Mutex::new(State::default()),
            save_lock: Mutex::new(()),
            flac_builds: Mutex::new(HashMap::new()),
        };
        copies.load();
        Arc::new(copies)
    }

    /// Reads the index, keeping the copies whose file is there at its size. A copy the index
    /// does not list (an index that could not be read lists none) is listed again from its file;
    /// unfinished fills, and files no video id names, go.
    fn load(&self) {
        let _ = std::fs::create_dir_all(&self.folder);
        let mut index: HashMap<String, Entry> = std::fs::read(self.folder.join(INDEX))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        index.retain(|id, entry| {
            is_video_id(id) && std::fs::metadata(self.copy_path(id)).is_ok_and(|m| m.len() == entry.size)
        });
        // A stand-in is kept only when its file is there at its size; a half-built one goes.
        for (id, entry) in index.iter_mut() {
            if entry.flac_size > 0
                && !std::fs::metadata(self.flac_path(id)).is_ok_and(|m| m.len() == entry.flac_size)
            {
                entry.flac_size = 0;
            }
        }
        if let Ok(dir) = std::fs::read_dir(self.folder.join(FLAC_FOLDER)) {
            for item in dir.flatten() {
                let path = item.path();
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                let known = path.extension().is_some_and(|e| e == "flac")
                    && index.get(stem).is_some_and(|entry| entry.flac_size > 0);
                if !known {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        let now = self.clock.now();
        if let Ok(dir) = std::fs::read_dir(&self.folder) {
            for item in dir.flatten() {
                let path = item.path();
                let kind = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                if kind == "part" || kind == "heard" || (kind == "preview" && !is_video_id(&stem)) {
                    let _ = std::fs::remove_file(&path);
                } else if kind == "preview" && !index.contains_key(&stem) {
                    let Ok(meta) = std::fs::metadata(&path) else {
                        continue;
                    };
                    index.insert(
                        stem,
                        Entry {
                            size: meta.len(),
                            content_type: sniff_type(&path).to_string(),
                            last_use: now,
                            kept_until: None,
                            holders: BTreeSet::new(),
                            flac_size: 0,
                        },
                    );
                }
            }
        }
        self.state.lock().index = index;
    }

    fn copy_path(&self, id: &str) -> PathBuf {
        self.folder.join(format!("{id}.preview"))
    }

    fn part_path(&self, id: &str) -> PathBuf {
        self.folder.join(format!("{id}.part"))
    }

    /// Where a copy's FLAC stand-in is kept: `<copies>/flac/<video id>.flac`.
    pub fn flac_path(&self, id: &str) -> PathBuf {
        self.folder.join(FLAC_FOLDER).join(format!("{id}.flac"))
    }

    /// Writes the index as it is now. Blocking file work: async callers use `save`.
    fn persist(&self) {
        let _one = self.save_lock.lock();
        let index = self.state.lock().index.clone();
        let Ok(json) = serde_json::to_vec_pretty(&index) else {
            return;
        };
        let temp = self.folder.join(format!("{INDEX}.tmp"));
        if std::fs::write(&temp, json).is_ok() {
            let _ = std::fs::rename(&temp, self.folder.join(INDEX));
        }
    }

    /// Writes the index as it is now, off the async threads.
    pub async fn save(self: &Arc<Self>) {
        let copies = Arc::clone(self);
        let _ = tokio::task::spawn_blocking(move || copies.persist()).await;
    }

    /// Starts one fill of this video's preview for a play, unless a copy is there or filling.
    /// `expected_total` is the size the play's own answer gave; a stream of another size makes no
    /// copy. False when no fill could start.
    pub fn start(self: &Arc<Self>, video_id: &str, expected_total: Option<u64>) -> bool {
        matches!(
            self.begin(video_id, expected_total, None),
            Copying::Ready | Copying::Started
        )
    }

    /// Makes sure this video has a copy held until `until`, for a playlist kept ready: an existing
    /// copy is held, else a fill starts.
    pub fn start_kept(self: &Arc<Self>, video_id: &str, until: DateTime<Utc>) -> Copying {
        if self.is_ready(video_id) {
            self.keep(video_id, until);
            return Copying::Ready;
        }
        self.begin(video_id, None, Some(until))
    }

    fn begin(
        self: &Arc<Self>,
        video_id: &str,
        expected: Option<u64>,
        kept_until: Option<DateTime<Utc>>,
    ) -> Copying {
        if !is_video_id(video_id) {
            return Copying::Refused;
        }
        if free_bytes(&self.folder).is_some_and(|free| free < (self.min_free)()) {
            return Copying::Refused;
        }
        let (tx, rx) = watch::channel(Progress::default());
        {
            let mut state = self.state.lock();
            if state.index.contains_key(video_id) {
                return Copying::Ready;
            }
            if let Some(fill) = state.fills.get_mut(video_id) {
                if kept_until > fill.kept_until {
                    fill.kept_until = kept_until;
                }
                return Copying::Started;
            }
            let kept_running = state.fills.values().filter(|f| f.kept_until.is_some()).count();
            if state.fills.len() >= FILLS_AT_ONCE
                || (kept_until.is_some() && kept_running >= KEPT_FILLS_AT_ONCE)
            {
                return Copying::Busy;
            }
            state.fills.insert(
                video_id.to_string(),
                Fill {
                    progress: rx,
                    kept_until,
                },
            );
        }
        let copies = Arc::clone(self);
        let id = video_id.to_string();
        tokio::spawn(async move { copies.fill(id, expected, tx).await });
        Copying::Started
    }

    async fn fill(self: Arc<Self>, id: String, expected: Option<u64>, tx: watch::Sender<Progress>) {
        let began = Instant::now();
        let part = self.part_path(&id);
        let outcome = self.write_part(&id, expected, &part, &tx).await;
        let outcome = match outcome {
            Ok(done) => tokio::fs::rename(&part, self.copy_path(&id))
                .await
                .map(|()| done)
                .map_err(|e| format!("the copy could not be kept: {e}")),
            Err(why) => Err(why),
        };
        let now = self.clock.now();
        {
            let mut state = self.state.lock();
            let kept_until = state.fills.get(&id).and_then(|f| f.kept_until);
            if let Ok((size, content_type)) = &outcome {
                state.index.insert(
                    id.clone(),
                    Entry {
                        size: *size,
                        content_type: content_type.clone(),
                        last_use: now,
                        kept_until,
                        holders: BTreeSet::new(),
                        flac_size: 0,
                    },
                );
            }
            state.fills.remove(&id);
        }
        match outcome {
            Ok((size, _)) => {
                info!(
                    "Preview copy of {id} ready ({size} bytes, {} ms)",
                    began.elapsed().as_millis()
                );
                self.save().await;
            }
            Err(why) => {
                let _ = tokio::fs::remove_file(&part).await;
                warn!("Preview copy of {id} given up: {why}");
            }
        }
        tx.send_modify(|p| p.ended = true);
    }

    /// Writes the whole preview into its unfinished file, telling readers each step.
    async fn write_part(
        &self,
        id: &str,
        expected: Option<u64>,
        part: &Path,
        tx: &watch::Sender<Progress>,
    ) -> Result<(u64, String), String> {
        let whole = tokio::time::timeout(FILL_STALL, self.source.open_whole(id))
            .await
            .map_err(|_| "the shim did not answer".to_string())?
            .ok_or_else(|| "the shim had no stream".to_string())?;
        if let Some(expected) = expected
            && expected != whole.total
        {
            return Err(format!(
                "the stream is {} bytes, not the {expected} the play was sent",
                whole.total
            ));
        }
        if whole.total == 0 {
            return Err("the stream is empty".to_string());
        }
        tx.send_modify(|p| {
            p.total = Some(whole.total);
            p.content_type = Some(whole.content_type.clone());
        });
        let mut file = tokio::fs::File::create(part).await.map_err(|e| e.to_string())?;
        let mut body = whole.body;
        let mut written = 0u64;
        while written < whole.total {
            let chunk = match tokio::time::timeout(FILL_STALL, body.next()).await {
                Err(_) => return Err("no new bytes for 30 s".to_string()),
                Ok(None) => break,
                Ok(Some(Err(e))) => return Err(e.to_string()),
                Ok(Some(Ok(chunk))) => chunk,
            };
            let room = (whole.total - written).min(chunk.len() as u64) as usize;
            file.write_all(&chunk[..room]).await.map_err(|e| e.to_string())?;
            // Readers open the same file: a byte is told about only once it is written through.
            file.flush().await.map_err(|e| e.to_string())?;
            written += room as u64;
            tx.send_modify(|p| p.written = written);
        }
        file.sync_data().await.map_err(|e| e.to_string())?;
        if written != whole.total {
            return Err(format!("the stream ended at {written} of {} bytes", whole.total));
        }
        Ok((whole.total, whole.content_type))
    }

    /// The answer to a request from the copy, or None when it cannot come from there: no copy, a
    /// copy of another size than the song's `confirmed_size`, a range past the end (the live
    /// answer gives that), or a range whose first byte a filling copy has not reached.
    pub async fn serve(
        &self,
        video_id: &str,
        range: Option<&str>,
        confirmed_size: Option<u64>,
    ) -> Option<DirectStreamInfo> {
        if !is_video_id(video_id) || range.is_some_and(|r| r.contains(',')) {
            return None;
        }
        let now = self.clock.now();
        let (ready, filling) = {
            let mut state = self.state.lock();
            let ready = state.index.get_mut(video_id).map(|entry| {
                entry.last_use = now;
                entry.clone()
            });
            let filling = match ready {
                Some(_) => None,
                None => state.fills.get(video_id).map(|f| f.progress.clone()),
            };
            (ready, filling)
        };
        if let Some(entry) = ready {
            if confirmed_size.is_some_and(|size| size != entry.size) {
                return None;
            }
            let file = tokio::fs::File::open(self.copy_path(video_id)).await.ok()?;
            return answer(file, entry.size, entry.content_type, range, None, video_id).await;
        }
        let progress = filling?;
        let seen = progress.borrow().clone();
        let (Some(total), Some(content_type)) = (seen.total, seen.content_type) else {
            return None;
        };
        if seen.ended {
            // Over since it was looked up: complete if the index has it now.
            let entry = self.state.lock().index.get(video_id).cloned()?;
            if confirmed_size.is_some_and(|size| size != entry.size) {
                return None;
            }
            let file = tokio::fs::File::open(self.copy_path(video_id)).await.ok()?;
            return answer(file, entry.size, entry.content_type, range, None, video_id).await;
        }
        if confirmed_size.is_some_and(|size| size != total) {
            return None;
        }
        let file = match tokio::fs::File::open(self.part_path(video_id)).await {
            Ok(file) => file,
            // Complete since it was looked up: the file has its own name now.
            Err(_) => tokio::fs::File::open(self.copy_path(video_id)).await.ok()?,
        };
        answer(file, total, content_type, range, Some(progress), video_id).await
    }

    /// Holds this video's copy until `until`, for a playlist kept ready. Kept in memory: the
    /// caller saves once it has held all it holds (`save`).
    pub fn keep(&self, video_id: &str, until: DateTime<Utc>) {
        let mut state = self.state.lock();
        if let Some(entry) = state.index.get_mut(video_id)
            && entry.kept_until.is_none_or(|kept| kept < until)
        {
            entry.kept_until = Some(until);
        }
    }

    /// Removes this video's complete copy and its index entry, unless a playlist kept ready
    /// holds it or someone holds it as what they heard. A file still being read when it goes is
    /// read on to its end. Blocking file work.
    pub fn drop_copy(&self, video_id: &str) {
        let now = self.clock.now();
        {
            let mut state = self.state.lock();
            if state.held(video_id) || state.index.get(video_id).is_none_or(|entry| entry.kept(now)) {
                return;
            }
            state.index.remove(video_id);
        }
        self.remove_files(video_id);
        self.persist();
    }

    /// Deletes a copy's file and its FLAC stand-in.
    fn remove_files(&self, video_id: &str) {
        if is_video_id(video_id) {
            let _ = std::fs::remove_file(self.copy_path(video_id));
            let _ = std::fs::remove_file(self.flac_path(video_id));
        }
    }

    /// Holds this video's complete copy as what `holder` heard: no sweep, cap or re-pin removes
    /// it while anyone holds it. A heart's holder id is saved with the index; a play's is kept in
    /// memory only. False when there is no complete copy.
    pub fn hold(&self, video_id: &str, holder: &str) -> bool {
        let saved = {
            let mut state = self.state.lock();
            if !state.index.contains_key(video_id) {
                return false;
            }
            if is_saved_holder(holder) {
                state
                    .index
                    .get_mut(video_id)
                    .is_some_and(|entry| entry.holders.insert(holder.to_string()))
            } else {
                state
                    .play_holders
                    .entry(video_id.to_string())
                    .or_default()
                    .insert(holder.to_string());
                false
            }
        };
        if saved {
            self.persist();
        }
        true
    }

    /// Lets go of `holder`'s hold on this video's copy; the copy is free once nobody holds it.
    pub fn release(&self, video_id: &str, holder: &str) {
        let saved = {
            let mut state = self.state.lock();
            if let Some(plays) = state.play_holders.get_mut(video_id) {
                plays.remove(holder);
                if plays.is_empty() {
                    state.play_holders.remove(video_id);
                }
            }
            state
                .index
                .get_mut(video_id)
                .is_some_and(|entry| entry.holders.remove(holder))
        };
        if saved {
            self.persist();
        }
    }

    /// Removes every saved holder id that `live` does not name, from every copy: an id a crash
    /// left mid-release never pins a copy.
    pub fn release_holders_except(&self, live: &HashSet<String>) {
        let changed = {
            let mut state = self.state.lock();
            let mut changed = false;
            for entry in state.index.values_mut() {
                let before = entry.holders.len();
                entry.holders.retain(|holder| live.contains(holder));
                changed |= entry.holders.len() != before;
            }
            changed
        };
        if changed {
            self.persist();
        }
    }

    /// Whether anyone holds this video's copy as what they heard.
    pub fn is_held(&self, video_id: &str) -> bool {
        self.state.lock().held(video_id)
    }

    /// The size and type of this video's complete copy.
    pub fn copy_facts(&self, video_id: &str) -> Option<(u64, String)> {
        self.state
            .lock()
            .index
            .get(video_id)
            .map(|entry| (entry.size, entry.content_type.clone()))
    }

    /// The complete copy of this video, when it is `size` bytes (when known) of `content_type`
    /// (when given).
    pub fn copy_path_of(
        &self,
        video_id: &str,
        size: Option<u64>,
        content_type: Option<&str>,
    ) -> Option<PathBuf> {
        let entry = self.state.lock().index.get(video_id).cloned()?;
        if size.is_some_and(|size| size != entry.size)
            || content_type.is_some_and(|ct| !same_type(ct, &entry.content_type))
        {
            return None;
        }
        let path = self.copy_path(video_id);
        path.is_file().then_some(path)
    }

    /// A whole copy of the heard video's own stream at the heard format and size: the copy here
    /// when it is that, else one fresh fetch at the heard format. Another size or type from that
    /// fetch is not the heard file: it counts as gone, never as a match. Any other source has no
    /// stream copy.
    pub async fn heard_copy(self: &Arc<Self>, heard: &HeardSource) -> HeardCopy {
        let HeardSource::Video {
            id,
            size,
            content_type,
            itag,
        } = heard
        else {
            return HeardCopy::Gone;
        };
        if let Some(path) = self.copy_path_of(id, *size, Some(content_type)) {
            return HeardCopy::Ready(path);
        }
        if !is_video_id(id) {
            return HeardCopy::Gone;
        }
        let whole = match tokio::time::timeout(FILL_STALL, self.source.open_heard(id, *itag)).await {
            Ok(Ok(whole)) => whole,
            Ok(Err(Failure::Gone | Failure::Refused)) => return HeardCopy::Gone,
            Ok(Err(_)) | Err(_) => return HeardCopy::Busy,
        };
        if size.is_some_and(|size| size != whole.total) || !same_type(content_type, &whole.content_type) {
            return HeardCopy::Gone;
        }
        let part = self.folder.join(format!("{id}.heard.part"));
        let written = write_whole(whole, &part).await;
        let Some((total, fetched_type)) = written else {
            let _ = tokio::fs::remove_file(&part).await;
            return HeardCopy::Busy;
        };
        let now = self.clock.now();
        let path = self.copy_path(id);
        {
            let mut state = self.state.lock();
            if state.held(id) && state.index.contains_key(id) {
                // A copy someone holds stays as it is; the heard one waits beside it.
                drop(state);
                let aside = self.folder.join(format!("{id}.heard"));
                let _ = std::fs::rename(&part, &aside);
                return HeardCopy::Ready(aside);
            }
            if std::fs::rename(&part, &path).is_err() {
                let _ = std::fs::remove_file(&part);
                return HeardCopy::Busy;
            }
            let _ = std::fs::remove_file(self.flac_path(id));
            state.index.insert(
                id.clone(),
                Entry {
                    size: total,
                    content_type: fetched_type,
                    last_use: now,
                    kept_until: None,
                    holders: BTreeSet::new(),
                    flac_size: 0,
                },
            );
        }
        self.save().await;
        HeardCopy::Ready(path)
    }

    /// The FLAC stand-in already built for this video, for a HEAD that builds nothing.
    pub fn built_flac(&self, video_id: &str) -> Option<PathBuf> {
        let built = self
            .state
            .lock()
            .index
            .get(video_id)
            .is_some_and(|entry| entry.flac_size > 0);
        let path = self.flac_path(video_id);
        (built && path.is_file()).then_some(path)
    }

    /// The video's preview as a whole FLAC file, `<copies>/flac/<video id>.flac`: its whole copy
    /// (made now when there is none, `expected_total` bytes when known) converted by
    /// `ffmpeg -c:a flac`. Built once; later reads reuse it, and it goes with its copy. None when
    /// no copy could be made or ffmpeg failed.
    pub async fn flac_stand_in(
        self: &Arc<Self>,
        video_id: &str,
        expected_total: Option<u64>,
    ) -> Option<PathBuf> {
        if !is_video_id(video_id) {
            return None;
        }
        let build = {
            let mut builds = self.flac_builds.lock();
            Arc::clone(builds.entry(video_id.to_string()).or_default())
        };
        let _one = build.lock().await;
        if let Some(built) = self.built_flac(video_id) {
            self.touch(video_id);
            return Some(built);
        }
        if !self.is_ready(video_id) {
            if !matches!(
                self.begin(video_id, expected_total, None),
                Copying::Ready | Copying::Started
            ) {
                return None;
            }
            self.wait_for_fill(video_id).await;
        }
        let copy = self.copy_path_of(video_id, None, None)?;
        let flac = self.flac_path(video_id);
        let _ = tokio::fs::create_dir_all(flac.parent()?).await;
        let part = flac.with_extension("flac.part");
        if !to_flac(&copy, &part).await {
            let _ = tokio::fs::remove_file(&part).await;
            return None;
        }
        if tokio::fs::rename(&part, &flac).await.is_err() {
            let _ = tokio::fs::remove_file(&part).await;
            return None;
        }
        let size = std::fs::metadata(&flac).map(|m| m.len()).unwrap_or(0);
        let kept = {
            let mut state = self.state.lock();
            match state.index.get_mut(video_id) {
                Some(entry) => {
                    entry.flac_size = size;
                    entry.last_use = self.clock.now();
                    true
                }
                None => false,
            }
        };
        if !kept {
            // The copy went while its stand-in was built: the stand-in goes with it.
            let _ = tokio::fs::remove_file(&flac).await;
            return None;
        }
        self.save().await;
        Some(flac)
    }

    /// The copy was used now.
    fn touch(&self, video_id: &str) {
        let now = self.clock.now();
        if let Some(entry) = self.state.lock().index.get_mut(video_id) {
            entry.last_use = now;
        }
    }

    /// Waits until the video's fill, if any, is over.
    async fn wait_for_fill(&self, video_id: &str) {
        let Some(mut progress) = self
            .state
            .lock()
            .fills
            .get(video_id)
            .map(|fill| fill.progress.clone())
        else {
            return;
        };
        loop {
            if progress.borrow().ended {
                return;
            }
            match tokio::time::timeout(SERVE_STALL, progress.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => return,
            }
        }
    }

    /// When the copies held as what people heard take more than the cap on their own, the oldest
    /// held copy whose video YouTube still serves is let go and deleted, since that video stays
    /// the fallback for the very audio; a copy whose video no longer answers is never let go this
    /// way. Answers the videos let go.
    pub async fn release_held_over_cap(&self) -> Vec<String> {
        let now = self.clock.now();
        let mut held: Vec<(String, DateTime<Utc>, u64)> = {
            let state = self.state.lock();
            state
                .index
                .iter()
                .filter(|(id, e)| !e.kept(now) && state.held(id))
                .map(|(id, e)| (id.clone(), e.last_use, e.bytes()))
                .collect()
        };
        held.sort_by_key(|(_, last_use, _)| *last_use);
        let mut used: u64 = held.iter().map(|(_, _, bytes)| bytes).sum();
        let mut released = Vec::new();
        for (id, _, bytes) in held {
            if used <= self.cap {
                break;
            }
            if !self.source.still_served(&id).await {
                continue;
            }
            {
                let mut state = self.state.lock();
                state.play_holders.remove(&id);
                state.index.remove(&id);
            }
            self.remove_files(&id);
            used = used.saturating_sub(bytes);
            info!("Held copy of {id} let go: held copies passed the cap, and YouTube still serves it");
            released.push(id);
        }
        if !released.is_empty() {
            self.persist();
        }
        released
    }

    /// Whether this video's copy is complete.
    pub fn is_ready(&self, video_id: &str) -> bool {
        self.state.lock().index.contains_key(video_id)
    }

    /// Whether a new copy for a playlist kept ready fits under `kept_cap` bytes, counting the kept
    /// copies and the kept fills running.
    pub fn kept_room(&self, kept_cap: u64) -> bool {
        self.kept_bytes() < kept_cap
    }

    /// The bytes held for playlists kept ready, fills running included.
    pub fn kept_bytes(&self) -> u64 {
        let now = self.clock.now();
        let state = self.state.lock();
        let kept: u64 = state.index.values().filter(|e| e.kept(now)).map(|e| e.size).sum();
        let filling: u64 = state
            .fills
            .values()
            .filter(|f| f.kept_until.is_some())
            .map(|f| f.progress.borrow().total.unwrap_or(0))
            .sum();
        kept + filling
    }

    /// The complete copies and their bytes (their FLAC stand-ins included), for the dashboard.
    pub fn sizes(&self) -> (usize, u64) {
        let state = self.state.lock();
        (state.index.len(), state.index.values().map(Entry::bytes).sum())
    }

    /// A copy nobody holds goes when unused for a day, then the ones used longest ago while the
    /// copies take more than the cap; a held copy still counts toward it (see
    /// [`Self::release_held_over_cap`]). A copy a playlist holds stays until its hold ends, and
    /// one held as what someone heard until they let it go. A file still being read when it goes
    /// is read on to its end. Blocking file work.
    pub fn sweep(&self) {
        let now = self.clock.now();
        let gone = {
            let mut state = self.state.lock();
            for entry in state.index.values_mut() {
                if entry.kept_until.is_some_and(|until| until <= now) {
                    entry.kept_until = None;
                }
            }
            let mut gone: Vec<String> = state
                .index
                .iter()
                .filter(|(id, e)| !e.kept(now) && !state.held(id) && now - e.last_use > UNUSED_FOR)
                .map(|(id, _)| id.clone())
                .collect();
            for id in &gone {
                state.index.remove(id);
            }
            let mut unkept: Vec<(String, DateTime<Utc>, u64, bool)> = state
                .index
                .iter()
                .filter(|(_, e)| !e.kept(now))
                .map(|(id, e)| (id.clone(), e.last_use, e.bytes(), state.held(id)))
                .collect();
            unkept.sort_by_key(|(_, last_use, _, _)| *last_use);
            let mut used: u64 = unkept.iter().map(|(_, _, bytes, _)| bytes).sum();
            for (id, _, bytes, held) in unkept {
                if used <= self.cap {
                    break;
                }
                if held {
                    continue;
                }
                state.index.remove(&id);
                used -= bytes;
                gone.push(id);
            }
            gone
        };
        for id in &gone {
            self.remove_files(id);
        }
        self.persist();
    }
}

/// The type of a copy whose index entry was lost: WebM starts with its EBML mark, anything else is
/// the m4a YouTube serves first.
fn sniff_type(path: &Path) -> &'static str {
    use std::io::Read;
    let mut head = [0u8; 4];
    let webm = std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok_and(|()| head == [0x1A, 0x45, 0xDF, 0xA3]);
    if webm { "audio/webm" } else { "audio/mp4" }
}

/// Whether two content types name the same media type, parameters aside.
fn same_type(a: &str, b: &str) -> bool {
    let bare = |t: &str| t.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    bare(a) == bare(b)
}

/// Writes a whole stream to `part`, giving up when it stalls or ends short. Its size and type.
async fn write_whole(whole: WholePreview, part: &Path) -> Option<(u64, String)> {
    let mut file = tokio::fs::File::create(part).await.ok()?;
    let mut body = whole.body;
    let mut written = 0u64;
    while written < whole.total {
        let chunk = match tokio::time::timeout(FILL_STALL, body.next()).await {
            Ok(Some(Ok(chunk))) => chunk,
            Ok(None) => break,
            Ok(Some(Err(_))) | Err(_) => return None,
        };
        let room = (whole.total - written).min(chunk.len() as u64) as usize;
        file.write_all(&chunk[..room]).await.ok()?;
        written += room as u64;
    }
    file.sync_data().await.ok()?;
    (written == whole.total && written > 0).then_some((written, whole.content_type))
}

/// `ffmpeg -c:a flac` of `copy` into `out`, the audio alone. False when ffmpeg is missing or
/// failed.
async fn to_flac(copy: &Path, out: &Path) -> bool {
    let run = tokio::process::Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(copy)
        .args(["-vn", "-c:a", "flac", "-f", "flac"])
        .arg(out)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await;
    match run {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            warn!(
                "ffmpeg could not make a FLAC stand-in of {}: {}",
                copy.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
            false
        }
        Err(e) => {
            warn!("ffmpeg could not run for a FLAC stand-in: {e}");
            false
        }
    }
}

/// The answer for `range` of a copy `total` bytes long, read from `file`. With `growing`, the
/// copy is still filling: the body follows it as it grows.
async fn answer(
    mut file: tokio::fs::File,
    total: u64,
    content_type: String,
    range: Option<&str>,
    growing: Option<watch::Receiver<Progress>>,
    video_id: &str,
) -> Option<DirectStreamInfo> {
    if total == 0 {
        return None;
    }
    let (status, from, to) = match range.map(|r| parse_range(r, total)) {
        None | Some(RangeOutcome::Whole) => (200, 0, total - 1),
        Some(RangeOutcome::Partial(from, to)) => (206, from, to),
        Some(RangeOutcome::Unsatisfiable) => return None,
    };
    if growing.as_ref().is_some_and(|p| from > p.borrow().written) {
        return None;
    }
    if from > 0 {
        file.seek(SeekFrom::Start(from)).await.ok()?;
    }
    let reader = Reader {
        file,
        pos: from,
        end: to,
        total,
        growing,
    };
    let body = futures::stream::unfold(Some(reader), |reader| async move {
        let mut reader = reader?;
        match reader.next().await {
            Ok(Some(bytes)) => Some((Ok(bytes), Some(reader))),
            Ok(None) => None,
            Err(e) => Some((Err(e), None)),
        }
    });
    Some(DirectStreamInfo {
        audio_stream: Box::pin(body),
        content_type,
        content_length: Some(to - from + 1),
        quality: Some("youtube-m4a (copy)".to_string()),
        status_code: status,
        content_range: (status == 206).then(|| format!("bytes {from}-{to}/{total}")),
        video_id: Some(video_id.to_string()),
        itag: None,
    })
}

/// Reads one range of a copy, waiting on a filling copy for bytes it has not got yet.
struct Reader {
    file: tokio::fs::File,
    pos: u64,
    /// The last byte to send.
    end: u64,
    total: u64,
    growing: Option<watch::Receiver<Progress>>,
}

impl Reader {
    async fn next(&mut self) -> std::io::Result<Option<Bytes>> {
        if self.pos > self.end {
            return Ok(None);
        }
        let mut limit = self.end + 1;
        if let Some(progress) = self.growing.as_mut() {
            loop {
                let seen = progress.borrow().clone();
                if seen.written > self.pos || seen.written >= self.total {
                    limit = limit.min(seen.written);
                    break;
                }
                if seen.ended {
                    return Err(std::io::Error::other("the copy ended short"));
                }
                match tokio::time::timeout(SERVE_STALL, progress.changed()).await {
                    Ok(Ok(())) => {}
                    // The fill is over: what it wrote is all there is.
                    Ok(Err(_)) => {
                        let written = progress.borrow().written;
                        if written > self.pos {
                            limit = limit.min(written);
                            break;
                        }
                        return Err(std::io::Error::other("the copy ended short"));
                    }
                    Err(_) => return Err(std::io::Error::other("the copy stopped growing")),
                }
            }
        }
        let want = (limit - self.pos).min(READ_CHUNK) as usize;
        let mut buf = vec![0u8; want];
        let read = self.file.read(&mut buf).await?;
        if read == 0 {
            return Err(std::io::Error::other("the copy ended short"));
        }
        buf.truncate(read);
        self.pos += read as u64;
        Ok(Some(Bytes::from(buf)))
    }
}
