//! Converted copies of outside songs' audio: each source made into each asked format once, from
//! ffmpeg's output as it comes, kept in a folder of their own, so a later range, a request that
//! needs the length, and the next play all read the very same bytes.
//!
//! A build is owned by its own task: the requests that wait on it only watch it, so one that
//! hangs up never stops it. A build fed by a copy still filling reads the copy as it grows and
//! fails when the copy does, so a copy cut short never becomes a short song.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use axum::body::Body;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use futures::future::BoxFuture;
use octo_core::common::Clock;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, watch};
use tracing::{info, warn};

use super::convert_rules::{
    Codec, Container, MAX_KBPS, MIN_KBPS, Plan, SourceAudio, SourceCodec, build_args, sniff_file,
};
use super::preview_copies::CopyFeed;
use super::store::free_bytes;

/// The folder of converted copies, inside the copies' own folder.
pub const FOLDER: &str = "converted";
/// The converted copies never take more than this; the ones used longest ago go first.
pub const CAP_BYTES: u64 = 1024 * 1024 * 1024;
/// How much of ffmpeg's output is read at once.
const READ_CHUNK: usize = 64 * 1024;

/// How ffmpeg is started. Tests start a fake that is held at a gate.
#[async_trait]
pub trait Runner: Send + Sync {
    async fn spawn(&self, args: Vec<String>) -> std::io::Result<RunningJob>;
}

/// One running ffmpeg: its input (when it reads `pipe:0`), its output, whether it ended well,
/// and a way to stop it.
pub struct RunningJob {
    pub stdin: Option<Box<dyn AsyncWrite + Send + Unpin>>,
    pub stdout: Box<dyn AsyncRead + Send + Unpin>,
    pub done: BoxFuture<'static, std::io::Result<bool>>,
    pub kill: Box<dyn FnOnce() + Send>,
}

/// The real ffmpeg. Dropping its job kills it.
pub struct FfmpegRunner;

#[async_trait]
impl Runner for FfmpegRunner {
    async fn spawn(&self, args: Vec<String>) -> std::io::Result<RunningJob> {
        let piped = args.iter().any(|arg| arg == "pipe:0");
        let mut child = tokio::process::Command::new("ffmpeg")
            .args(&args)
            .stdin(if piped { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .map(|stdin| Box::new(stdin) as Box<dyn AsyncWrite + Send + Unpin>);
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("ffmpeg gave no output"))?;
        // Drained as it comes, so ffmpeg never waits on a full pipe; kept for the log.
        let said = child.stderr.take().map(|mut stderr| {
            tokio::spawn(async move {
                let mut text = String::new();
                let _ = stderr.read_to_string(&mut text).await;
                text
            })
        });
        let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
        let killed = async move {
            if kill_rx.await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        let done = Box::pin(async move {
            let status = tokio::select! {
                status = child.wait() => Some(status),
                () = killed => None,
            };
            let Some(status) = status else {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Ok(false);
            };
            let status = status?;
            if !status.success() {
                let words = match said {
                    Some(said) => said.await.unwrap_or_default(),
                    None => String::new(),
                };
                warn!("ffmpeg ended with {status}: {}", words.trim());
            }
            Ok(status.success())
        });
        Ok(RunningJob {
            stdin,
            stdout: Box::new(stdout),
            done,
            kill: Box::new(move || {
                let _ = kill_tx.send(());
            }),
        })
    }
}

/// The store's waits and lifetimes, all settable so tests run in seconds.
#[derive(Debug, Clone)]
pub struct Timings {
    /// A build with no new output (and no new input fed) for this long is stopped.
    pub stall: Duration,
    /// A failed build is not tried again by a new play for this long.
    pub failure_memory: Duration,
    /// How long a first-byte request waits for a build's first output.
    pub first_output: Duration,
    /// How long a client other than an Octo app waits for what it asked.
    pub others: Duration,
    /// How long an answer from a later time waits for its input to reach that time.
    pub seek_input: Duration,
    /// A copy used this recently is never swept.
    pub recent_use: Duration,
    /// A copy unused for this long is swept.
    pub unused_for: chrono::Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            stall: Duration::from_secs(30),
            failure_memory: Duration::from_secs(600),
            first_output: Duration::from_secs(10),
            others: Duration::from_secs(30),
            seek_input: Duration::from_secs(12),
            recent_use: Duration::from_secs(3600),
            unused_for: chrono::Duration::hours(24),
        }
    }
}

/// A wait as a span of the store's clock.
fn on_clock(wait: Duration) -> chrono::Duration {
    chrono::Duration::from_std(wait).unwrap_or(chrono::Duration::MAX)
}

/// What a build reads: a file, or a copy of a video (still filling, or complete).
pub enum BuildInput {
    File(PathBuf),
    Copy(CopyFeed),
}

/// How far a build has got: the bytes written, and how it ended (its length, or why not).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildState {
    pub written: u64,
    pub ended: Option<Result<u64, String>>,
}

#[derive(Default)]
struct StoreState {
    /// The running builds, each watched by the requests that wait on it.
    claims: HashMap<String, watch::Receiver<BuildState>>,
    /// The file each running build writes.
    parts: HashMap<String, PathBuf>,
    /// When each converted copy was last used.
    used: HashMap<String, DateTime<Utc>>,
    /// When each failed build failed.
    failures: HashMap<String, DateTime<Utc>>,
    /// What each file source holds, read once per source key.
    facts: HashMap<String, SourceAudio>,
    /// The encoders ffmpeg has, once read.
    encoders: Option<HashSet<Codec>>,
    /// The missing encoders already named in the log.
    warned: HashSet<Codec>,
}

#[derive(Default)]
struct Permits {
    free: usize,
    /// Builds for a play from the first byte waiting for a permit: they go first.
    plays_waiting: usize,
    /// Every build waiting for a permit.
    waiting: usize,
}

/// A build's permit, given back when dropped.
struct Permit<'a> {
    copies: &'a ConvertedCopies,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.copies.permits.lock().free += 1;
        self.copies.permit_freed.notify_waiters();
    }
}

/// A time seek's permit (see [`ConvertedCopies::seek_permit`]), held until the seek's first
/// sound and given back to the seeks' own permits when dropped.
pub struct OwnedPermit(Arc<ConvertedCopies>);

impl Drop for OwnedPermit {
    fn drop(&mut self) {
        self.0.seek_permits.lock().free += 1;
        self.0.seek_freed.notify_waiters();
    }
}

/// Counts one wait for a seek permit while it lasts, so a wait that is given up (its future
/// dropped) leaves the counts as they were.
struct PermitWait<'a> {
    copies: &'a ConvertedCopies,
    taken: bool,
}

impl Drop for PermitWait<'_> {
    fn drop(&mut self) {
        if !self.taken {
            self.copies.seek_permits.lock().waiting -= 1;
        }
    }
}

/// The converted copies, in a folder of their own.
pub struct ConvertedCopies {
    folder: PathBuf,
    clock: Clock,
    /// No build starts while the disk has less free than this.
    min_free: Box<dyn Fn() -> u64 + Send + Sync>,
    runner: Arc<dyn Runner>,
    cap: u64,
    timings: Timings,
    state: Mutex<StoreState>,
    permits: Mutex<Permits>,
    permit_freed: Notify,
    /// Time seeks that re-encode, which run at the pace their listener reads, have their own
    /// permits, as many as the builds', so a seek never keeps a build from starting.
    seek_permits: Mutex<Permits>,
    seek_freed: Notify,
    /// Numbers each build's part file, so two builds never share one.
    counter: AtomicU64,
}

/// A video's copy as a source: its id and the size of its copy, so a copy made again with
/// other bytes is another source.
pub fn source_key_video(video: &str, copy_size: u64) -> String {
    format!("v-{video}-{copy_size}")
}

/// A file as a source: its path, length and change time.
pub fn source_key_file(path: &Path, len: u64, mtime_secs: i64) -> String {
    let digest = Sha256::digest(format!("{}|{len}|{mtime_secs}", path.display()).as_bytes());
    format!("f-{}", &hex::encode(digest)[..24])
}

/// A converted copy's key: its source and what it is made as.
pub fn build_key(source_key: &str, plan: &Plan) -> String {
    format!(
        "{source_key}.{}",
        plan.name().unwrap_or_else(|| "as-is".to_string())
    )
}

/// The file ending of a target name (`mp3-128`, `ogg-copy`), when it is one a copy is made as.
fn ext_of_target(target: &str) -> Option<&'static str> {
    if target == "ogg-copy" {
        return Some("opus");
    }
    let (codec, kbps) = target.split_once('-')?;
    let kbps: u32 = kbps.parse().ok()?;
    if !(MIN_KBPS..=MAX_KBPS).contains(&kbps) {
        return None;
    }
    Codec::named(codec).map(Codec::ext)
}

/// The key a converted copy's file name holds (`<source>.<target>.<ext>`), when it is one.
fn key_of_file(name: &str) -> Option<String> {
    let (key, ext) = name.rsplit_once('.')?;
    let (source, target) = key.rsplit_once('.')?;
    if !(source.starts_with("v-") || source.starts_with("f-")) {
        return None;
    }
    (ext_of_target(target)? == ext).then(|| key.to_string())
}

/// What a file of an outside song holds, from its first bytes and, for its rate and to tell
/// ALAC from AAC, its headers. Blocking file work: async callers run it on `spawn_blocking`.
pub fn describe_file(path: &Path) -> SourceAudio {
    use std::io::Read;
    let mut head = vec![0u8; 4096];
    let read = std::fs::File::open(path)
        .and_then(|mut file| file.read(&mut head))
        .unwrap_or(0);
    head.truncate(read);
    let (codec, container) = sniff_file(&head);
    let headers = lofty::read_from_path(path).ok().map(|tagged| {
        use lofty::file::AudioFile;
        let properties = tagged.properties();
        (properties.audio_bitrate(), properties.bit_depth())
    });
    let codec = match (codec, container, headers) {
        (SourceCodec::Aac, Container::Mp4, Some((_, Some(_)))) => SourceCodec::Lossless,
        (codec, _, _) => codec,
    };
    let kbps = match codec {
        SourceCodec::Lossless | SourceCodec::Unknown => None,
        _ => headers.and_then(|(rate, _)| rate).filter(|rate| *rate > 0),
    };
    SourceAudio {
        codec,
        container,
        kbps,
    }
}

/// Whether a failure is still remembered at `now`; a forgotten one is dropped.
fn failed_in(state: &mut StoreState, key: &str, now: DateTime<Utc>, memory: Duration) -> bool {
    match state.failures.get(key) {
        Some(at) if now - *at < on_clock(memory) => true,
        Some(_) => {
            state.failures.remove(key);
            false
        }
        None => false,
    }
}

/// A receiver that has already ended.
fn ended(state: BuildState) -> watch::Receiver<BuildState> {
    let (tx, rx) = watch::channel(state);
    drop(tx);
    rx
}

impl ConvertedCopies {
    /// The store in `folder` (its own folder, `<copies>/converted`), with the real ffmpeg, half
    /// the cores as build permits, the 1 GB cap and the usual timings.
    pub fn new(
        folder: PathBuf,
        clock: Clock,
        min_free: impl Fn() -> u64 + Send + Sync + 'static,
    ) -> Arc<Self> {
        let cores = std::thread::available_parallelism().map_or(1, usize::from);
        let copies = Self::with(
            folder,
            clock,
            min_free,
            Arc::new(FfmpegRunner),
            (cores / 2).max(1),
            CAP_BYTES,
            Timings::default(),
        );
        // Which encoders this ffmpeg has, read once in the background, so a missing one plays
        // songs as they are and is named in the log. Built with no runtime (a plain test),
        // nothing runs and every encoder counts as there.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let probe = Arc::clone(&copies);
            runtime.spawn(async move { probe.probe_encoders().await });
        }
        copies
    }

    /// Whether the encoders this ffmpeg has are known yet.
    #[cfg(test)]
    pub(crate) fn encoders_known(&self) -> bool {
        self.state.lock().encoders.is_some()
    }

    /// As `new`, with every part named.
    pub fn with(
        folder: PathBuf,
        clock: Clock,
        min_free: impl Fn() -> u64 + Send + Sync + 'static,
        runner: Arc<dyn Runner>,
        permits: usize,
        cap: u64,
        timings: Timings,
    ) -> Arc<Self> {
        let copies = ConvertedCopies {
            folder,
            clock,
            min_free: Box::new(min_free),
            runner,
            cap,
            timings,
            state: Mutex::new(StoreState::default()),
            permits: Mutex::new(Permits {
                free: permits.max(1),
                plays_waiting: 0,
                waiting: 0,
            }),
            permit_freed: Notify::new(),
            seek_permits: Mutex::new(Permits {
                free: permits.max(1),
                plays_waiting: 0,
                waiting: 0,
            }),
            seek_freed: Notify::new(),
            counter: AtomicU64::new(0),
        };
        copies.load();
        Arc::new(copies)
    }

    /// Clears what a stopped run left (part files, names no copy has) and notes when each copy
    /// was last changed, as its last use.
    fn load(&self) {
        let _ = std::fs::create_dir_all(&self.folder);
        let now = self.clock.now();
        let Ok(dir) = std::fs::read_dir(&self.folder) else {
            return;
        };
        let mut used = HashMap::new();
        for item in dir.flatten() {
            let path = item.path();
            let name = item.file_name().to_string_lossy().into_owned();
            match key_of_file(&name) {
                Some(key) if !name.ends_with(".part") => {
                    let changed = item
                        .metadata()
                        .and_then(|meta| meta.modified())
                        .map(DateTime::<Utc>::from)
                        .unwrap_or(now);
                    used.insert(key, changed.min(now));
                }
                _ => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        self.state.lock().used = used;
    }

    /// The store's waits and lifetimes.
    pub fn timings(&self) -> &Timings {
        &self.timings
    }

    /// Where a key's finished copy lives.
    fn file_path(&self, key: &str) -> Option<PathBuf> {
        let (_, target) = key.rsplit_once('.')?;
        Some(self.folder.join(format!("{key}.{}", ext_of_target(target)?)))
    }

    /// Reads which encoders ffmpeg has, once at start, from the real ffmpeg (never the build
    /// runner, so a test runner sees only builds and time seeks). Until then every encoder
    /// counts as there; an ffmpeg that cannot run has none.
    pub async fn probe_encoders(&self) {
        // One listing a process: every store made in it (the app's one, every test's) reads the
        // same ffmpeg, so it runs once; stores made at the same moment wait for that one run.
        static LISTING: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
        let listing = LISTING
            .get_or_init(|| async {
                tokio::process::Command::new("ffmpeg")
                    .args(["-hide_banner", "-encoders"])
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .output()
                    .await
                    .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                    .unwrap_or_default()
            })
            .await;
        self.note_encoders(listing);
    }

    /// Notes the encoders an `ffmpeg -encoders` listing names.
    pub fn note_encoders(&self, listing: &str) {
        self.state.lock().encoders = Some(parse_encoders(listing));
    }

    /// The runner builds and time seeks start ffmpeg through.
    pub fn runner(&self) -> Arc<dyn Runner> {
        Arc::clone(&self.runner)
    }

    /// The key's finished copy's file, with its use noted (in memory and on the file), when it
    /// is there.
    pub fn built_path(&self, key: &str) -> Option<PathBuf> {
        let path = self.file_path(key)?;
        let len = std::fs::metadata(&path).ok().filter(|meta| meta.is_file())?.len();
        if len == 0 {
            return None;
        }
        let now = self.clock.now();
        self.state.lock().used.insert(key.to_string(), now);
        if let Ok(touch) = std::fs::File::options().append(true).open(&path) {
            let _ = touch.set_modified(SystemTime::now());
        }
        Some(path)
    }

    /// A file as a source: its key (path, length and change time) and what it holds, read once
    /// per key (lofty on a blocking thread, then ffprobe for a lossy file whose rate lofty
    /// cannot read, such as a WebM). None when the file cannot be read.
    pub async fn file_source(&self, path: &Path) -> Option<(String, SourceAudio)> {
        let meta = tokio::fs::metadata(path)
            .await
            .ok()
            .filter(|meta| meta.is_file())?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_secs() as i64);
        let key = source_key_file(path, meta.len(), mtime);
        if let Some(known) = self.state.lock().facts.get(&key).copied() {
            return Some((key, known));
        }
        let owned = path.to_path_buf();
        let mut source = tokio::task::spawn_blocking(move || describe_file(&owned))
            .await
            .ok()?;
        if source.kbps.is_none() && !matches!(source.codec, SourceCodec::Lossless | SourceCodec::Unknown) {
            source.kbps = probe_rate(path).await;
        }
        self.state.lock().facts.insert(key.clone(), source);
        Some((key, source))
    }

    /// Whether ffmpeg can make this format. A missing encoder is named in the log once.
    pub fn encoders_ready(&self, codec: Codec) -> bool {
        let mut state = self.state.lock();
        let has = state.encoders.as_ref().map(|found| found.contains(&codec));
        match has {
            None | Some(true) => true,
            Some(false) => {
                if state.warned.insert(codec) {
                    warn!(
                        "ffmpeg has no {} encoder, so songs found online asked as {} play as they are",
                        codec.encoder(),
                        codec.param()
                    );
                }
                false
            }
        }
    }

    /// Whether this key's build failed lately.
    pub fn failed(&self, key: &str) -> bool {
        let now = self.clock.now();
        failed_in(&mut self.state.lock(), key, now, self.timings.failure_memory)
    }

    /// The key's finished copy, opened, and its length; its use is noted (in memory and on the
    /// file, so a restart keeps it). The answer reads the handle, so a sweep after this never
    /// takes the file from under it.
    pub fn complete(&self, key: &str) -> Option<(tokio::fs::File, u64)> {
        let path = self.file_path(key)?;
        let file = std::fs::File::open(&path).ok()?;
        let len = file.metadata().ok()?.len();
        if len == 0 {
            return None;
        }
        let now = self.clock.now();
        self.state.lock().used.insert(key.to_string(), now);
        if let Ok(touch) = std::fs::File::options().append(true).open(&path) {
            let _ = touch.set_modified(SystemTime::now());
        }
        Some((tokio::fs::File::from_std(file), len))
    }

    /// The key's running build, to wait on, when one runs.
    pub fn building(&self, key: &str) -> Option<watch::Receiver<BuildState>> {
        self.state.lock().claims.get(key).cloned()
    }

    /// Joins the key's running build, or answers its finished copy as ended, or starts the
    /// build (owned by its own task), a key remembered as failed included: whether a failed
    /// key is built again is the caller's call (`failed`). `first_byte_play` builds take a free
    /// permit ahead of the others.
    pub fn join_or_start(
        self: &Arc<Self>,
        key: &str,
        input: BuildInput,
        plan: Plan,
        first_byte_play: bool,
    ) -> watch::Receiver<BuildState> {
        let mut state = self.state.lock();
        if let Some(running) = state.claims.get(key) {
            return running.clone();
        }
        if let Some(path) = self.file_path(key)
            && let Ok(meta) = std::fs::metadata(&path)
            && meta.is_file()
            && meta.len() > 0
        {
            return ended(BuildState {
                written: meta.len(),
                ended: Some(Ok(meta.len())),
            });
        }
        if plan == Plan::AsIs || self.file_path(key).is_none() {
            return ended(BuildState {
                written: 0,
                ended: Some(Err("nothing to build".to_string())),
            });
        }
        let (tx, rx) = watch::channel(BuildState::default());
        state.claims.insert(key.to_string(), rx.clone());
        drop(state);
        let copies = Arc::clone(self);
        let key = key.to_string();
        tokio::spawn(async move { copies.build(key, input, plan, first_byte_play, tx).await });
        rx
    }

    /// One more build of a key whose build failed: its failure is forgotten first.
    pub fn retry(
        self: &Arc<Self>,
        key: &str,
        input: BuildInput,
        plan: Plan,
        first_byte_play: bool,
    ) -> watch::Receiver<BuildState> {
        self.state.lock().failures.remove(key);
        self.join_or_start(key, input, plan, first_byte_play)
    }

    async fn build(
        self: Arc<Self>,
        key: String,
        input: BuildInput,
        plan: Plan,
        play: bool,
        tx: watch::Sender<BuildState>,
    ) {
        let began = std::time::Instant::now();
        let outcome = self.run(&key, input, &plan, play, &tx).await;
        {
            let now = self.clock.now();
            let mut state = self.state.lock();
            state.claims.remove(&key);
            state.parts.remove(&key);
            match &outcome {
                Ok(_) => {
                    state.used.insert(key.clone(), now);
                    state.failures.remove(&key);
                }
                Err(_) => {
                    state.failures.insert(key.clone(), now);
                }
            }
        }
        match &outcome {
            Ok(bytes) => info!(
                "Converted copy of {key} ready ({bytes} bytes, {} ms)",
                began.elapsed().as_millis()
            ),
            Err(why) => warn!("Converted copy of {key} given up: {why}"),
        }
        tx.send_modify(|state| state.ended = Some(outcome));
    }

    /// Waits for a permit: builds for a play from the first byte go before the others.
    async fn take_permit(&self, play: bool) -> Permit<'_> {
        {
            let mut permits = self.permits.lock();
            permits.waiting += 1;
            if play {
                permits.plays_waiting += 1;
            }
        }
        loop {
            let freed = self.permit_freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            {
                let mut permits = self.permits.lock();
                if permits.free > 0 && (play || permits.plays_waiting == 0) {
                    permits.free -= 1;
                    permits.waiting -= 1;
                    if play {
                        permits.plays_waiting -= 1;
                    }
                    return Permit { copies: self };
                }
            }
            freed.await;
        }
    }

    /// How many builds wait for a permit now.
    #[cfg(test)]
    pub(crate) fn waiting_for_permits(&self) -> usize {
        self.permits.lock().waiting
    }

    /// A permit for a time seek that re-encodes from the source, from the seeks' own permits (a
    /// seek runs at its listener's pace, so it never holds a build's); the caller bounds the wait.
    /// A wait given up leaves the counts as they were.
    pub async fn seek_permit(self: &Arc<Self>) -> OwnedPermit {
        self.seek_permits.lock().waiting += 1;
        let mut wait = PermitWait {
            copies: self,
            taken: false,
        };
        loop {
            let freed = self.seek_freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            {
                let mut permits = self.seek_permits.lock();
                if permits.free > 0 {
                    permits.free -= 1;
                    permits.waiting -= 1;
                    wait.taken = true;
                    return OwnedPermit(Arc::clone(self));
                }
            }
            freed.await;
        }
    }

    /// How many time seeks wait for a seek permit now.
    #[cfg(test)]
    pub(crate) fn waiting_for_seek_permits(&self) -> usize {
        self.seek_permits.lock().waiting
    }

    async fn run(
        &self,
        key: &str,
        input: BuildInput,
        plan: &Plan,
        play: bool,
        tx: &watch::Sender<BuildState>,
    ) -> Result<u64, String> {
        let final_path = self.file_path(key).ok_or("not a converted copy's name")?;
        if free_bytes(&self.folder).is_some_and(|free| free < (self.min_free)()) {
            return Err("the disk is nearly full".to_string());
        }
        // A copy still filling is read from its feed and needs no permit: its pace is the
        // download's. A complete copy (held by its feed for the build) and a file are read by
        // path, under a permit.
        let (input_arg, feed, _kept) = match input {
            BuildInput::File(path) => (path.to_string_lossy().into_owned(), None, None),
            BuildInput::Copy(feed) => match feed.complete_path.clone() {
                Some(path) => (path.to_string_lossy().into_owned(), None, Some(feed)),
                None => ("pipe:0".to_string(), Some(feed), None),
            },
        };
        let _permit = if feed.is_none() {
            Some(self.take_permit(play).await)
        } else {
            None
        };
        let args = build_args(plan, &input_arg);
        if args.is_empty() {
            return Err("nothing to build".to_string());
        }
        let job = self
            .runner
            .spawn(args)
            .await
            .map_err(|e| format!("ffmpeg could not run: {e}"))?;
        let RunningJob {
            stdin,
            mut stdout,
            done,
            kill,
        } = job;
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let part = self.folder.join(format!("{key}.{n}.part"));
        let mut file = match tokio::fs::File::create(&part).await {
            Ok(file) => file,
            Err(e) => return give_up(kill, &part, format!("the copy could not be made: {e}")).await,
        };
        self.state.lock().parts.insert(key.to_string(), part.clone());

        // The feed runs on its own task, so neither side's wait holds the other up.
        let fed = Arc::new(AtomicU64::new(0));
        let waiting = Arc::new(AtomicBool::new(false));
        let mut feeder = match (feed, stdin) {
            (Some(feed), Some(mut stdin)) => {
                let (fed, waiting) = (Arc::clone(&fed), Arc::clone(&waiting));
                let mut body = feed.body;
                Some(tokio::spawn(async move {
                    loop {
                        waiting.store(true, Ordering::SeqCst);
                        let next = body.next().await;
                        waiting.store(false, Ordering::SeqCst);
                        match next {
                            None => break,
                            Some(Err(e)) => return Err(format!("the copy stopped: {e}")),
                            Some(Ok(chunk)) => {
                                stdin
                                    .write_all(&chunk)
                                    .await
                                    .map_err(|e| format!("ffmpeg stopped reading: {e}"))?;
                                fed.fetch_add(chunk.len() as u64, Ordering::SeqCst);
                            }
                        }
                    }
                    stdin
                        .shutdown()
                        .await
                        .map_err(|e| format!("ffmpeg stopped reading: {e}"))?;
                    Ok::<(), String>(())
                }))
            }
            (Some(_), None) => return give_up(kill, &part, "ffmpeg took no input".to_string()).await,
            (None, _) => None,
        };
        let mut feed_result: Option<Result<(), String>> = if feeder.is_none() { Some(Ok(())) } else { None };

        let stall = self.timings.stall;
        let mut tick = tokio::time::interval((stall / 4).max(Duration::from_millis(1)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last = tokio::time::Instant::now();
        let mut seen_fed = 0u64;
        let mut written = 0u64;
        loop {
            tokio::select! {
                read = read_chunk(&mut stdout) => match read {
                    Ok(chunk) if chunk.is_empty() => break,
                    Ok(chunk) => {
                        let wrote = match file.write_all(&chunk).await {
                            Ok(()) => file.flush().await,
                            Err(e) => Err(e),
                        };
                        if let Err(e) = wrote {
                            return give_up(kill, &part, format!("the copy could not be written: {e}")).await;
                        }
                        written += chunk.len() as u64;
                        tx.send_modify(|state| state.written = written);
                        last = tokio::time::Instant::now();
                    }
                    Err(e) => return give_up(kill, &part, format!("ffmpeg's output broke: {e}")).await,
                },
                joined = async {
                    match feeder.as_mut() {
                        Some(handle) => handle.await,
                        None => std::future::pending().await,
                    }
                }, if feed_result.is_none() => {
                    let result = joined.unwrap_or_else(|e| Err(format!("the feed stopped: {e}")));
                    if let Err(why) = result {
                        return give_up(kill, &part, why).await;
                    }
                    feed_result = Some(Ok(()));
                },
                _ = tick.tick() => {
                    let now_fed = fed.load(Ordering::SeqCst);
                    // Waiting on the copy's fill is not ffmpeg's stall: the copy's own wait
                    // ends its body if the fill stops.
                    if now_fed != seen_fed || waiting.load(Ordering::SeqCst) {
                        seen_fed = now_fed;
                        last = tokio::time::Instant::now();
                    }
                    if last.elapsed() >= stall {
                        return give_up(kill, &part, format!("no output for {} s", stall.as_secs())).await;
                    }
                },
            }
        }
        // ffmpeg closed its output: it must have taken its input whole, and ended well.
        let feed_result = match feed_result {
            Some(result) => result,
            None => match feeder.take() {
                Some(handle) => match tokio::time::timeout(stall, handle).await {
                    Ok(joined) => joined.unwrap_or_else(|e| Err(format!("the feed stopped: {e}"))),
                    Err(_) => Err("ffmpeg ended before taking all of the copy".to_string()),
                },
                None => Ok(()),
            },
        };
        if let Err(why) = feed_result {
            return give_up(kill, &part, why).await;
        }
        let ended_well = match tokio::time::timeout(stall, done).await {
            Ok(Ok(well)) => well,
            Ok(Err(e)) => return give_up(kill, &part, format!("ffmpeg could not be waited on: {e}")).await,
            Err(_) => return give_up(kill, &part, "ffmpeg did not end".to_string()).await,
        };
        if !ended_well {
            return give_up(kill, &part, "ffmpeg failed".to_string()).await;
        }
        if written == 0 {
            return give_up(kill, &part, "ffmpeg wrote nothing".to_string()).await;
        }
        if let Err(e) = file.sync_data().await {
            return give_up(kill, &part, format!("the copy could not be kept: {e}")).await;
        }
        drop(file);
        if let Err(e) = tokio::fs::rename(&part, &final_path).await {
            return give_up(kill, &part, format!("the copy could not be kept: {e}")).await;
        }
        Ok(written)
    }

    /// The growing copy's bytes for one answer, as the build writes them: from the part file
    /// while it builds, its own file once done. The body ends with an error when the build
    /// fails, so a client sees a cut transfer, never a clean end.
    pub fn growing(self: &Arc<Self>, key: &str, rx: watch::Receiver<BuildState>) -> Body {
        let reader = Growing {
            copies: Arc::clone(self),
            key: key.to_string(),
            rx,
            file: None,
            pos: 0,
            closed: false,
        };
        let body = futures::stream::unfold(Some(reader), |reader| async move {
            let mut reader = reader?;
            match reader.next().await {
                Ok(Some(bytes)) => Some((Ok(bytes), Some(reader))),
                Ok(None) => None,
                Err(e) => Some((Err(e), None)),
            }
        });
        Body::from_stream(body)
    }

    /// Removes the copies unused for a day, then the ones used longest ago while the folder
    /// is over its cap. A copy being built, or used within the last hour, stays. Blocking file
    /// work.
    pub fn sweep(&self) {
        let now = self.clock.now();
        let Ok(dir) = std::fs::read_dir(&self.folder) else {
            return;
        };
        let listed: Vec<(String, PathBuf, u64)> = dir
            .flatten()
            .filter_map(|item| {
                let name = item.file_name().to_string_lossy().into_owned();
                let key = key_of_file(&name).filter(|_| !name.ends_with(".part"))?;
                let size = item.metadata().ok()?.len();
                Some((key, item.path(), size))
            })
            .collect();
        let mut files: Vec<(String, PathBuf, u64, DateTime<Utc>)> = {
            let state = self.state.lock();
            listed
                .into_iter()
                .filter(|(key, _, _)| !state.claims.contains_key(key))
                .map(|(key, path, size)| {
                    let last = state.used.get(&key).copied().unwrap_or(now);
                    (key, path, size, last)
                })
                .collect()
        };
        let recent_use = on_clock(self.timings.recent_use);
        let recent = |last: DateTime<Utc>| now - last < recent_use;
        let mut gone: Vec<(String, PathBuf)> = Vec::new();
        files.retain(|(key, path, _, last)| {
            if now - *last > self.timings.unused_for && !recent(*last) {
                gone.push((key.clone(), path.clone()));
                false
            } else {
                true
            }
        });
        files.sort_by_key(|(_, _, _, last)| *last);
        let mut bytes: u64 = files.iter().map(|(_, _, size, _)| size).sum();
        for (key, path, size, last) in &files {
            if bytes <= self.cap {
                break;
            }
            if recent(*last) {
                continue;
            }
            bytes = bytes.saturating_sub(*size);
            gone.push((key.clone(), path.clone()));
        }
        for (_, path) in &gone {
            let _ = std::fs::remove_file(path);
        }
        let mut state = self.state.lock();
        for (key, _) in &gone {
            state.used.remove(key);
            state.failures.remove(key);
        }
        let memory = on_clock(self.timings.failure_memory);
        state.failures.retain(|_, at| now - *at < memory);
        if state.facts.len() > 10_000 {
            state.facts.clear();
        }
    }

    /// The bytes the finished copies take, for the dashboard. Blocking file work.
    pub fn bytes(&self) -> u64 {
        let Ok(dir) = std::fs::read_dir(&self.folder) else {
            return 0;
        };
        dir.flatten()
            .filter(|item| {
                let name = item.file_name().to_string_lossy().into_owned();
                !name.ends_with(".part") && key_of_file(&name).is_some()
            })
            .filter_map(|item| item.metadata().ok().map(|meta| meta.len()))
            .sum()
    }
}

/// Stops a build: ffmpeg is stopped and its part file goes.
async fn give_up(kill: Box<dyn FnOnce() + Send>, part: &Path, why: String) -> Result<u64, String> {
    kill();
    let _ = tokio::fs::remove_file(part).await;
    Err(why)
}

/// One read of ffmpeg's output; empty at its end.
async fn read_chunk(stdout: &mut Box<dyn AsyncRead + Send + Unpin>) -> std::io::Result<Vec<u8>> {
    let mut buffer = vec![0u8; READ_CHUNK];
    let read = stdout.read(&mut buffer).await?;
    buffer.truncate(read);
    Ok(buffer)
}

/// A file's average bit rate in kbps by ffprobe, for a file whose headers lofty cannot read.
async fn probe_rate(path: &Path) -> Option<u32> {
    let output = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=bit_rate",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())?;
    let bits: u64 = String::from_utf8_lossy(&output.stdout).trim().parse().ok()?;
    u32::try_from(bits / 1000).ok().filter(|kbps| *kbps > 0)
}

/// The formats an `ffmpeg -encoders` listing has encoders for.
fn parse_encoders(text: &str) -> HashSet<Codec> {
    let mut found = HashSet::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let (Some(_flags), Some(name)) = (words.next(), words.next()) else {
            continue;
        };
        for codec in [Codec::Mp3, Codec::Opus, Codec::Aac] {
            if name == codec.encoder() {
                found.insert(codec);
            }
        }
    }
    found
}

/// Reads one build's output as it grows.
struct Growing {
    copies: Arc<ConvertedCopies>,
    key: String,
    rx: watch::Receiver<BuildState>,
    file: Option<tokio::fs::File>,
    pos: u64,
    closed: bool,
}

impl Growing {
    /// The build's file: its part while it builds, its own name once done.
    fn open(&self) -> Option<tokio::fs::File> {
        let part = self.copies.state.lock().parts.get(&self.key).cloned();
        let path = part
            .filter(|part| part.is_file())
            .or_else(|| self.copies.file_path(&self.key))?;
        std::fs::File::open(path).ok().map(tokio::fs::File::from_std)
    }

    async fn next(&mut self) -> std::io::Result<Option<Bytes>> {
        loop {
            let state = self.rx.borrow().clone();
            if let Some(Err(why)) = &state.ended {
                return Err(std::io::Error::other(why.clone()));
            }
            if state.written > self.pos {
                if self.file.is_none() {
                    self.file = self.open();
                }
                let Some(file) = self.file.as_mut() else {
                    return Err(std::io::Error::other("the converted copy is gone"));
                };
                let want = (state.written - self.pos).min(READ_CHUNK as u64) as usize;
                let mut buffer = vec![0u8; want];
                let read = file.read(&mut buffer).await?;
                if read == 0 {
                    return Err(std::io::Error::other("the converted copy ended short"));
                }
                buffer.truncate(read);
                self.pos += read as u64;
                return Ok(Some(Bytes::from(buffer)));
            }
            if let Some(Ok(total)) = state.ended
                && self.pos >= total
            {
                return Ok(None);
            }
            if self.closed {
                return Err(std::io::Error::other("the converted copy's build stopped"));
            }
            if self.rx.changed().await.is_err() {
                // The build is over: what it last said stands.
                self.closed = true;
            }
        }
    }
}
