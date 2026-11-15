//! Converted copies: one build per copy whoever waits, a build that outlives the request that
//! started it, stalls, permits, a disk floor, a feed that breaks, and the sweep. A fake ffmpeg
//! held at a gate makes every race happen in order; the real one checks the bytes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, TimeZone, Utc};
use futures::StreamExt;
use octo_core::common::Clock;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, watch};

use super::convert_rules::{Codec, Plan, Target};
use super::converted_copies::{
    BuildInput, BuildState, ConvertedCopies, FfmpegRunner, Runner, RunningJob, Timings, build_key,
    source_key_file, source_key_video,
};
use super::preview_copies::CopyFeed;
use crate::services::test_audio::audio_or_skip;

const MP3_128: Plan = Plan::Convert(Target {
    codec: Codec::Mp3,
    kbps: 128,
});

#[derive(Default)]
struct Gate {
    open: AtomicBool,
    notify: Notify,
}

impl Gate {
    async fn wait(&self) {
        loop {
            let opened = self.notify.notified();
            tokio::pin!(opened);
            opened.as_mut().enable();
            if self.open.load(Ordering::SeqCst) {
                return;
            }
            opened.await;
        }
    }

    fn release(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Takes its whole input, then writes its output and ends well.
    Writes,
    /// Never writes and never ends.
    NeverWrites,
    /// Writes half its output and ends badly.
    ExitFails,
}

/// A stand-in for ffmpeg: held at `gate` once started, then answering as `mode` says.
struct FakeRunner {
    gate: Arc<Gate>,
    mode: Mode,
    output: Vec<u8>,
    runs: AtomicUsize,
    started: Arc<(AtomicUsize, Notify)>,
    inputs: Mutex<Vec<String>>,
    fed: Arc<Mutex<Vec<u8>>>,
}

impl FakeRunner {
    fn new(mode: Mode, output: Vec<u8>, gated: bool) -> Arc<Self> {
        let gate = Arc::new(Gate::default());
        if !gated {
            gate.release();
        }
        Arc::new(FakeRunner {
            gate,
            mode,
            output,
            runs: AtomicUsize::new(0),
            started: Arc::new((AtomicUsize::new(0), Notify::new())),
            inputs: Mutex::new(Vec::new()),
            fed: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }

    async fn wait_started(&self, n: usize) {
        loop {
            let started = self.started.1.notified();
            tokio::pin!(started);
            started.as_mut().enable();
            if self.started.0.load(Ordering::SeqCst) >= n {
                return;
            }
            started.await;
        }
    }
}

#[async_trait]
impl Runner for FakeRunner {
    async fn spawn(&self, args: Vec<String>) -> std::io::Result<RunningJob> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let input = args
            .iter()
            .position(|arg| arg == "-i")
            .and_then(|at| args.get(at + 1))
            .cloned()
            .unwrap_or_default();
        self.inputs.lock().push(input.clone());
        let piped = input == "pipe:0";
        let (stdin_write, mut stdin_read) = tokio::io::duplex(1 << 16);
        let (mut stdout_write, stdout_read) = tokio::io::duplex(1 << 16);
        let output = self.output.clone();
        let (gate, mode, started, fed) = (
            Arc::clone(&self.gate),
            self.mode,
            Arc::clone(&self.started),
            Arc::clone(&self.fed),
        );
        let task = tokio::spawn(async move {
            started.0.fetch_add(1, Ordering::SeqCst);
            started.1.notify_waiters();
            if piped {
                let mut got = Vec::new();
                if stdin_read.read_to_end(&mut got).await.is_err() {
                    return false;
                }
                fed.lock().extend_from_slice(&got);
            }
            gate.wait().await;
            match mode {
                Mode::Writes => stdout_write.write_all(&output).await.is_ok(),
                Mode::NeverWrites => {
                    std::future::pending::<()>().await;
                    false
                }
                Mode::ExitFails => {
                    let _ = stdout_write.write_all(&output[..output.len() / 2]).await;
                    false
                }
            }
        });
        let abort = task.abort_handle();
        Ok(RunningJob {
            stdin: piped.then(|| Box::new(stdin_write) as Box<dyn AsyncWrite + Send + Unpin>),
            stdout: Box::new(stdout_read),
            done: Box::pin(async move { Ok(task.await.unwrap_or(false)) }),
            kill: Box::new(move || abort.abort()),
        })
    }
}

fn at(hours: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 14, 12, 0, 0).unwrap() + chrono::Duration::hours(hours)
}

fn clock() -> (Clock, Arc<Mutex<DateTime<Utc>>>) {
    let now = Arc::new(Mutex::new(at(0)));
    let read = Arc::clone(&now);
    (Clock::new(move || *read.lock()), now)
}

fn timings() -> Timings {
    Timings {
        stall: Duration::from_secs(5),
        ..Timings::default()
    }
}

fn store(
    dir: &Path,
    runner: Arc<dyn Runner>,
    permits: usize,
    timings: Timings,
    clock: Clock,
) -> Arc<ConvertedCopies> {
    ConvertedCopies::with(
        dir.join("converted"),
        clock,
        || 0,
        runner,
        permits,
        u64::MAX,
        timings,
    )
}

fn input(dir: &Path, name: &str) -> BuildInput {
    let path = dir.join(name);
    std::fs::write(&path, b"fLaC source").unwrap();
    BuildInput::File(path)
}

fn key(name: &str) -> String {
    build_key(&source_key_video(name, 1000), &MP3_128)
}

async fn ended(mut rx: watch::Receiver<BuildState>) -> Result<u64, String> {
    tokio::time::timeout(Duration::from_secs(5), async move {
        loop {
            if let Some(ended) = rx.borrow().ended.clone() {
                return ended;
            }
            if rx.changed().await.is_err() {
                return rx
                    .borrow()
                    .ended
                    .clone()
                    .unwrap_or_else(|| Err("the build vanished".to_string()));
            }
        }
    })
    .await
    .expect("the build ended in time")
}

fn output() -> Vec<u8> {
    (0..200_000).map(|i| (i % 253) as u8).collect()
}

fn names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir.join("converted"))
        .map(|dir| {
            dir.flatten()
                .map(|item| item.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn two_requests_for_one_copy_make_one_build() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::new(Mode::Writes, output(), true);
    let (clock, _) = clock();
    let copies = store(dir.path(), runner.clone(), 2, timings(), clock);
    let first = copies.join_or_start(&key("aaaaaaaaaaa"), input(dir.path(), "a.flac"), MP3_128, true);
    runner.wait_started(1).await;
    let second = copies.join_or_start(&key("aaaaaaaaaaa"), input(dir.path(), "a.flac"), MP3_128, true);
    runner.gate.release();
    assert_eq!(ended(first).await, Ok(200_000));
    assert_eq!(ended(second).await, Ok(200_000));
    assert_eq!(runner.runs(), 1);
    let (_, len) = copies.complete(&key("aaaaaaaaaaa")).expect("the finished copy");
    assert_eq!(len, 200_000);
}

#[tokio::test]
async fn a_build_goes_on_when_the_request_that_started_it_hangs_up() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::new(Mode::Writes, output(), true);
    let (clock, _) = clock();
    let copies = store(dir.path(), runner.clone(), 2, timings(), clock);
    let first = copies.join_or_start(&key("aaaaaaaaaaa"), input(dir.path(), "a.flac"), MP3_128, true);
    runner.wait_started(1).await;
    drop(first);
    runner.gate.release();
    let again = copies.join_or_start(&key("aaaaaaaaaaa"), input(dir.path(), "a.flac"), MP3_128, true);
    assert_eq!(ended(again).await, Ok(200_000));
    assert_eq!(runner.runs(), 1);
    // Done: a later request is answered from the file with no new build.
    let later = copies.join_or_start(&key("aaaaaaaaaaa"), input(dir.path(), "a.flac"), MP3_128, true);
    assert_eq!(ended(later).await, Ok(200_000));
    assert_eq!(runner.runs(), 1);
}

#[tokio::test]
async fn a_build_with_no_output_is_stopped_and_remembered() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::new(Mode::NeverWrites, output(), false);
    let (clock, now) = clock();
    let stalls = Timings {
        stall: Duration::from_millis(50),
        ..Timings::default()
    };
    let copies = store(dir.path(), runner.clone(), 2, stalls, clock);
    let k = key("aaaaaaaaaaa");
    let why = ended(copies.join_or_start(&k, input(dir.path(), "a.flac"), MP3_128, true))
        .await
        .unwrap_err();
    assert!(why.contains("no output"), "{why}");
    assert!(names(dir.path()).is_empty(), "the part file went");
    assert!(
        copies.failed(&k),
        "the failure is remembered for the callers that check it"
    );
    *now.lock() = at(1);
    assert!(!copies.failed(&k), "the failure is forgotten after its time");
}

#[tokio::test]
async fn a_failed_key_is_built_again_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::new(Mode::ExitFails, output(), false);
    let (clock, _) = clock();
    let copies = store(dir.path(), runner.clone(), 1, timings(), clock);
    let k = key("aaaaaaaaaaa");
    assert!(
        ended(copies.join_or_start(&k, input(dir.path(), "a.flac"), MP3_128, true))
            .await
            .is_err()
    );
    assert!(copies.failed(&k));
    // The funnel decides when a failed key is built again (one rebuild for a converted play):
    // asked, the store builds it.
    assert!(
        ended(copies.join_or_start(&k, input(dir.path(), "a.flac"), MP3_128, true))
            .await
            .is_err()
    );
    assert_eq!(runner.runs(), 2);
    assert!(
        ended(copies.retry(&k, input(dir.path(), "a.flac"), MP3_128, true))
            .await
            .is_err()
    );
    assert_eq!(runner.runs(), 3);
}

/// A copy's bytes arriving every 20 ms.
fn slow_feed(chunks: usize, fail_after: Option<usize>) -> CopyFeed {
    let pieces: Vec<std::io::Result<Bytes>> = (0..chunks)
        .map(|i| match fail_after {
            Some(after) if i >= after => Err(std::io::Error::other("connection reset")),
            _ => Ok(Bytes::from(vec![i as u8; 1000])),
        })
        .collect();
    let body = futures::stream::iter(pieces).then(|piece| async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        piece
    });
    CopyFeed {
        video: "aaaaaaaaaaa".to_string(),
        total: (chunks * 1000) as u64,
        content_type: "audio/mp4".to_string(),
        body: Box::pin(body),
        complete_path: None,
        hold: None,
    }
}

#[tokio::test]
async fn a_slow_feed_that_keeps_coming_is_no_stall() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::new(Mode::Writes, output(), false);
    let (clock, _) = clock();
    let stalls = Timings {
        stall: Duration::from_millis(60),
        ..Timings::default()
    };
    let copies = store(dir.path(), runner.clone(), 1, stalls, clock);
    let rx = copies.join_or_start(
        &key("aaaaaaaaaaa"),
        BuildInput::Copy(slow_feed(10, None)),
        MP3_128,
        true,
    );
    assert_eq!(ended(rx).await, Ok(200_000));
    assert_eq!(runner.fed.lock().len(), 10_000, "ffmpeg took the whole copy");
    assert_eq!(runner.inputs.lock().as_slice(), ["pipe:0".to_string()]);
}

#[tokio::test]
async fn a_feed_that_breaks_fails_the_build_and_cuts_its_readers() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::new(Mode::Writes, output(), false);
    let (clock, _) = clock();
    let copies = store(dir.path(), runner.clone(), 1, timings(), clock);
    let k = key("aaaaaaaaaaa");
    let rx = copies.join_or_start(&k, BuildInput::Copy(slow_feed(10, Some(3))), MP3_128, true);
    let body = copies.growing(&k, rx.clone());
    let why = ended(rx).await.unwrap_err();
    assert!(why.contains("the copy stopped"), "{why}");
    assert!(
        axum::body::to_bytes(body, usize::MAX).await.is_err(),
        "a reader sees a cut transfer"
    );
    assert!(names(dir.path()).is_empty(), "no copy is kept");
    assert!(copies.complete(&k).is_none());
}

#[tokio::test]
async fn a_reader_gets_the_bytes_as_they_are_written() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::new(Mode::Writes, output(), true);
    let (clock, _) = clock();
    let copies = store(dir.path(), runner.clone(), 1, timings(), clock);
    let k = key("aaaaaaaaaaa");
    let rx = copies.join_or_start(&k, input(dir.path(), "a.flac"), MP3_128, true);
    let body = copies.growing(&k, rx.clone());
    runner.gate.release();
    let got = axum::body::to_bytes(body, usize::MAX)
        .await
        .expect("the whole copy");
    assert_eq!(got.as_ref(), output().as_slice());
    assert_eq!(ended(rx).await, Ok(200_000));
}

#[tokio::test]
async fn a_build_that_ends_badly_keeps_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::new(Mode::ExitFails, output(), false);
    let (clock, _) = clock();
    let copies = store(dir.path(), runner.clone(), 1, timings(), clock);
    let why = ended(copies.join_or_start(&key("aaaaaaaaaaa"), input(dir.path(), "a.flac"), MP3_128, true))
        .await
        .unwrap_err();
    assert_eq!(why, "ffmpeg failed");
    assert!(names(dir.path()).is_empty());
}

#[tokio::test]
async fn builds_wait_for_a_permit_and_plays_go_first() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::new(Mode::Writes, output(), true);
    let (clock, _) = clock();
    let copies = store(dir.path(), runner.clone(), 1, timings(), clock);
    let a = copies.join_or_start(&key("aaaaaaaaaaa"), input(dir.path(), "a.flac"), MP3_128, true);
    runner.wait_started(1).await;
    let b = copies.join_or_start(&key("bbbbbbbbbbb"), input(dir.path(), "b.flac"), MP3_128, false);
    wait_until(|| copies.waiting_for_permits() == 1).await;
    let c = copies.join_or_start(&key("ccccccccccc"), input(dir.path(), "c.flac"), MP3_128, true);
    wait_until(|| copies.waiting_for_permits() == 2).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), runner.wait_started(2))
            .await
            .is_err(),
        "no second build runs while the only permit is held"
    );
    runner.gate.release();
    for rx in [a, b, c] {
        assert!(ended(rx).await.is_ok());
    }
    let inputs: Vec<String> = runner
        .inputs
        .lock()
        .iter()
        .map(|path| {
            Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        inputs,
        ["a.flac", "c.flac", "b.flac"],
        "the waiting play went first"
    );
}

#[tokio::test]
async fn a_time_seek_has_its_own_permits_and_a_wait_given_up_leaves_no_count() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::new(Mode::Writes, output(), false);
    let (clock, _) = clock();
    let copies = store(dir.path(), runner.clone(), 1, timings(), clock);
    let first = tokio::time::timeout(Duration::from_secs(2), copies.seek_permit())
        .await
        .expect("a free seek permit is taken at once");
    // The only seek permit is held: a second seek's wait gives up, and counts nothing after.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), copies.seek_permit())
            .await
            .is_err(),
        "a seek re-encode waits while every seek permit is taken"
    );
    assert_eq!(
        copies.waiting_for_seek_permits(),
        0,
        "the given-up wait left no count"
    );
    // A seek playing at its listener's pace never keeps a build from starting.
    let build = copies.join_or_start(&key("aaaaaaaaaaa"), input(dir.path(), "a.flac"), MP3_128, false);
    tokio::time::timeout(Duration::from_secs(2), runner.wait_started(1))
        .await
        .expect("the build started beside the held seek permit");
    assert!(ended(build).await.is_ok());
    drop(first);
    tokio::time::timeout(Duration::from_secs(2), copies.seek_permit())
        .await
        .expect("the permit came back when the seek's answer went");
}

#[tokio::test]
async fn a_build_fed_by_a_filling_copy_takes_no_permit() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::new(Mode::Writes, output(), true);
    let (clock, _) = clock();
    let copies = store(dir.path(), runner.clone(), 1, timings(), clock);
    let held = copies.join_or_start(&key("aaaaaaaaaaa"), input(dir.path(), "a.flac"), MP3_128, true);
    runner.wait_started(1).await;
    let fed = copies.join_or_start(
        &key("bbbbbbbbbbb"),
        BuildInput::Copy(slow_feed(3, None)),
        MP3_128,
        true,
    );
    tokio::time::timeout(Duration::from_secs(2), runner.wait_started(2))
        .await
        .expect("the fed build started beside the one holding the permit");
    runner.gate.release();
    assert!(ended(held).await.is_ok());
    assert!(ended(fed).await.is_ok());
}

#[tokio::test]
async fn the_store_reads_its_encoders_once_it_is_made() {
    let dir = tempfile::tempdir().unwrap();
    let (clock, _) = clock();
    let copies = ConvertedCopies::new(dir.path().join("converted"), clock, || 0);
    // The probe runs ffmpeg once: a short wait on that work, bounded.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !copies.encoders_known() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the store read its encoders");
    if std::env::var("OCTO_TESTS_NEED_FFMPEG").is_ok_and(|need| need == "1") {
        assert!(copies.encoders_ready(Codec::Mp3), "this ffmpeg makes MP3");
    }
}

// The disk's free space is read on Unix only, where the server runs.
#[cfg(unix)]
#[tokio::test]
async fn no_build_starts_on_a_nearly_full_disk() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::new(Mode::Writes, output(), false);
    let (clock, _) = clock();
    let copies = ConvertedCopies::with(
        dir.path().join("converted"),
        clock,
        || u64::MAX,
        runner.clone(),
        1,
        u64::MAX,
        timings(),
    );
    let why = ended(copies.join_or_start(&key("aaaaaaaaaaa"), input(dir.path(), "a.flac"), MP3_128, true))
        .await
        .unwrap_err();
    assert_eq!(why, "the disk is nearly full");
    assert_eq!(runner.runs(), 0);
}

#[tokio::test]
async fn the_source_as_it_is_is_never_built() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::new(Mode::Writes, output(), false);
    let (clock, _) = clock();
    let copies = store(dir.path(), runner.clone(), 1, timings(), clock);
    let as_is = build_key(&source_key_video("aaaaaaaaaaa", 1000), &Plan::AsIs);
    assert!(
        ended(copies.join_or_start(&as_is, input(dir.path(), "a.flac"), Plan::AsIs, true))
            .await
            .is_err()
    );
    assert_eq!(runner.runs(), 0);
}

#[tokio::test]
async fn a_missing_encoder_is_read_once_and_named() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::new(Mode::Writes, Vec::new(), false);
    let (clock, _) = clock();
    let copies = store(dir.path(), runner.clone(), 1, timings(), clock);
    assert!(
        copies.encoders_ready(Codec::Opus),
        "every encoder counts as there before the probe"
    );
    copies.note_encoders(
        "Encoders:\n ------\n A....D libmp3lame           libmp3lame MP3\n A....D aac                  AAC\n",
    );
    assert_eq!(
        runner.runs(),
        0,
        "the encoder listing never goes through the build runner"
    );
    assert!(copies.encoders_ready(Codec::Mp3));
    assert!(copies.encoders_ready(Codec::Aac));
    assert!(!copies.encoders_ready(Codec::Opus));
    assert!(!copies.encoders_ready(Codec::Opus));
}

fn put(dir: &Path, name: &str, bytes: usize) -> PathBuf {
    let folder = dir.join("converted");
    std::fs::create_dir_all(&folder).unwrap();
    let path = folder.join(name);
    std::fs::write(&path, vec![7u8; bytes]).unwrap();
    path
}

#[tokio::test]
async fn the_sweep_takes_unused_copies_then_the_oldest_over_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    let old = put(dir.path(), "v-aaaaaaaaaaa-1000.mp3-128.mp3", 10);
    let kept = put(dir.path(), "v-bbbbbbbbbbb-1000.mp3-128.mp3", 10);
    let part = put(dir.path(), "v-ccccccccccc-1000.mp3-128.5.part", 10);
    let stray = put(dir.path(), "notes.txt", 10);
    let (clock, now) = clock();
    let runner = FakeRunner::new(Mode::Writes, output(), false);
    let short = Timings {
        recent_use: Duration::from_secs(60),
        ..timings()
    };
    let copies = ConvertedCopies::with(dir.path().join("converted"), clock, || 0, runner, 1, 15, short);
    assert!(
        !part.exists() && !stray.exists(),
        "a stopped run's leftovers go at start"
    );
    *now.lock() = at(20);
    assert!(copies.complete("v-bbbbbbbbbbb-1000.mp3-128").is_some());
    *now.lock() = at(25);
    copies.sweep();
    assert!(!old.exists(), "unused for a day");
    assert!(kept.exists(), "used five hours ago");
    let newest = put(dir.path(), "v-ddddddddddd-1000.mp3-128.mp3", 10);
    assert!(copies.complete("v-ddddddddddd-1000.mp3-128").is_some());
    *now.lock() = at(26);
    copies.sweep();
    assert!(!kept.exists(), "over the cap, the one used longest ago goes");
    assert!(newest.exists());
    assert_eq!(copies.bytes(), 10);
}

#[tokio::test]
async fn a_copy_used_lately_is_never_swept_and_an_open_answer_reads_on() {
    let dir = tempfile::tempdir().unwrap();
    let path = put(dir.path(), "v-aaaaaaaaaaa-1000.mp3-128.mp3", 10);
    let (clock, now) = clock();
    let runner = FakeRunner::new(Mode::Writes, output(), false);
    let zero = Timings {
        recent_use: Duration::ZERO,
        ..timings()
    };
    let copies = ConvertedCopies::with(
        dir.path().join("converted"),
        clock,
        || 0,
        runner,
        1,
        u64::MAX,
        zero,
    );
    let (mut file, len) = copies.complete("v-aaaaaaaaaaa-1000.mp3-128").expect("the copy");
    *now.lock() = at(30);
    copies.sweep();
    assert!(!path.exists(), "swept once unused");
    let mut read = Vec::new();
    file.read_to_end(&mut read).await.unwrap();
    assert_eq!(
        read.len() as u64,
        len,
        "the open answer still reads the whole copy"
    );
}

#[test]
fn a_files_key_follows_its_path_size_and_change_time() {
    let one = source_key_file(Path::new("/music/a.flac"), 100, 5);
    assert!(one.starts_with("f-") && one.len() == 26, "{one}");
    assert_eq!(one, source_key_file(Path::new("/music/a.flac"), 100, 5));
    assert_ne!(one, source_key_file(Path::new("/music/a.flac"), 101, 5));
    assert_ne!(one, source_key_file(Path::new("/music/a.flac"), 100, 6));
    assert_eq!(source_key_video("aaaaaaaaaaa", 4096), "v-aaaaaaaaaaa-4096");
}

#[tokio::test]
async fn a_built_copy_is_found_by_its_path_and_its_use_noted() {
    let dir = tempfile::tempdir().unwrap();
    let path = put(dir.path(), "v-aaaaaaaaaaa-1000.mp3-128.mp3", 10);
    let (clock, now) = clock();
    let runner = FakeRunner::new(Mode::Writes, output(), false);
    let copies = ConvertedCopies::with(
        dir.path().join("converted"),
        clock,
        || 0,
        runner,
        1,
        u64::MAX,
        timings(),
    );
    assert!(copies.built_path("v-bbbbbbbbbbb-1000.mp3-128").is_none());
    *now.lock() = at(23);
    assert_eq!(
        copies.built_path("v-aaaaaaaaaaa-1000.mp3-128"),
        Some(path.clone())
    );
    *now.lock() = at(26);
    copies.sweep();
    assert!(path.exists(), "the use at hour 23 kept it");
}

#[tokio::test]
async fn a_file_source_is_keyed_and_described_once() {
    let mp3 = audio_or_skip!("mp3");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.mp3");
    std::fs::write(&path, &mp3).unwrap();
    let (clock, _) = clock();
    let copies = store(
        dir.path(),
        FakeRunner::new(Mode::Writes, output(), false),
        1,
        timings(),
        clock,
    );
    let (key, source) = copies.file_source(&path).await.expect("a readable file");
    assert!(key.starts_with("f-"), "{key}");
    assert_eq!(source.codec, super::convert_rules::SourceCodec::Mp3);
    assert!(source.kbps.is_some_and(|kbps| kbps > 0), "{source:?}");
    assert_eq!(copies.file_source(&path).await, Some((key, source)));
    assert!(copies.file_source(&dir.path().join("gone.mp3")).await.is_none());
}

async fn wait_until(check: impl Fn() -> bool) {
    for _ in 0..1000 {
        if check() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the condition never held");
}

/// Builds `plan` of `source` with the real ffmpeg, in a fresh store.
async fn real_build(source: &[u8], extension: &str, plan: Plan) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("source.{extension}"));
    std::fs::write(&path, source).unwrap();
    let (clock, _) = clock();
    let copies = store(dir.path(), Arc::new(FfmpegRunner), 1, timings(), clock);
    let k = build_key(&source_key_video("aaaaaaaaaaa", source.len() as u64), &plan);
    let rx = copies.join_or_start(&k, BuildInput::File(path), plan, true);
    assert!(ended(rx).await.is_ok(), "{plan:?} built");
    let (mut file, _) = copies.complete(&k).expect("the finished copy");
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await.unwrap();
    bytes
}

#[tokio::test]
async fn real_builds_hold_what_they_are_labeled_and_are_the_same_every_time() {
    let m4a = audio_or_skip!("m4a");
    let webm = audio_or_skip!("webm");
    let mp3 = real_build(&m4a, "m4a", MP3_128).await;
    assert_eq!(&mp3[..2], &[0xFF, 0xFB], "bare MPEG frames, no tag");
    let opus_plan = Plan::Convert(Target {
        codec: Codec::Opus,
        kbps: 96,
    });
    let opus = real_build(&m4a, "m4a", opus_plan).await;
    assert_eq!(&opus[..4], b"OggS");
    assert_eq!(&opus[28..36], b"OpusHead");
    let aac_plan = Plan::Convert(Target {
        codec: Codec::Aac,
        kbps: 128,
    });
    let aac = real_build(&m4a, "m4a", aac_plan).await;
    assert_eq!(aac[0], 0xFF);
    assert_eq!(aac[1] & 0xF6, 0xF0, "ADTS");
    let copied = real_build(&webm, "webm", Plan::CopyOgg).await;
    assert_eq!(&copied[..4], b"OggS");
    for (plan, first) in [(MP3_128, &mp3), (opus_plan, &opus), (aac_plan, &aac)] {
        assert_eq!(
            &real_build(&m4a, "m4a", plan).await,
            first,
            "{plan:?} built twice is the same bytes"
        );
    }
    assert_eq!(&real_build(&webm, "webm", Plan::CopyOgg).await, &copied);
}
