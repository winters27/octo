//! Preview copies: when a copy is made and kept, and that every answer from one carries the same
//! bytes and headers as the preview itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Duration, TimeZone, Utc};
use futures::StreamExt;
use octo_core::common::Clock;
use parking_lot::Mutex;

use super::preview_copies::{
    Copying, PreviewCopies, PreviewSource, WholePreview, asks_for_pieces, copy_after, is_video_id,
};
use crate::services::i_download_service::DirectStreamInfo;

const VIDEO: &str = "osN-dGs2d2A";

fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// A preview served in chunks, each after `delay`, breaking after `break_after` bytes when set.
struct FakeSource {
    data: Vec<u8>,
    chunk: usize,
    delay: StdDuration,
    break_after: Option<usize>,
    total_said: Option<u64>,
    opens: AtomicUsize,
}

impl FakeSource {
    fn new(data: Vec<u8>) -> Self {
        FakeSource {
            data,
            chunk: 1000,
            delay: StdDuration::ZERO,
            break_after: None,
            total_said: None,
            opens: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl PreviewSource for FakeSource {
    async fn open_whole(&self, _video_id: &str) -> Option<WholePreview> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        let end = self.break_after.unwrap_or(self.data.len());
        let pieces: Vec<std::io::Result<Bytes>> = self.data[..end]
            .chunks(self.chunk)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .chain(
                self.break_after
                    .map(|_| Err(std::io::Error::other("connection reset"))),
            )
            .collect();
        let delay = self.delay;
        let body = futures::stream::iter(pieces).then(move |piece| async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            piece
        });
        Some(WholePreview {
            content_type: "audio/mp4".to_string(),
            total: self.total_said.unwrap_or(self.data.len() as u64),
            body: Box::pin(body),
        })
    }
}

fn at(minutes: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 21, 12, 0, 0).unwrap() + Duration::minutes(minutes)
}

/// A clock the test moves.
fn clock() -> (Clock, Arc<Mutex<DateTime<Utc>>>) {
    let now = Arc::new(Mutex::new(at(0)));
    let read = now.clone();
    (Clock::new(move || *read.lock()), now)
}

fn copies(dir: &tempfile::TempDir, source: Arc<FakeSource>, clock: Clock, cap: u64) -> Arc<PreviewCopies> {
    PreviewCopies::with_cap(dir.path().join("previews"), source, clock, || 0, cap)
}

async fn body(answer: DirectStreamInfo) -> Vec<u8> {
    let mut out = Vec::new();
    let mut stream = answer.audio_stream;
    while let Some(piece) = stream.next().await {
        out.extend_from_slice(&piece.expect("a whole body"));
    }
    out
}

async fn wait_ready(copies: &PreviewCopies, id: &str) {
    for _ in 0..500 {
        if copies.is_ready(id) {
            return;
        }
        tokio::time::sleep(StdDuration::from_millis(10)).await;
    }
    panic!("the copy of {id} was never ready");
}

#[test]
fn a_request_reads_in_pieces_when_it_probes_or_starts_past_the_first_byte() {
    assert!(asks_for_pieces("bytes=0-1"));
    assert!(asks_for_pieces("bytes=131072-"));
    assert!(asks_for_pieces("bytes=131072-20112002"));
    assert!(asks_for_pieces("bytes=-500"));
    assert!(!asks_for_pieces("bytes=0-"));
    assert!(!asks_for_pieces("bytes=0-20112002"));
    assert!(!asks_for_pieces("items=5-"));
}

#[test]
fn a_preview_is_copied_after_an_answer_to_a_request_in_pieces() {
    // AVPlayer's probe, and its steps past the start.
    assert!(copy_after(206, Some("bytes=0-1")));
    assert!(copy_after(206, Some("bytes=131072-20112002")));
    // A player reading the song from its start in one go, or no Range at all.
    assert!(!copy_after(206, Some("bytes=0-")));
    assert!(!copy_after(200, None));
    // No audio came back.
    assert!(!copy_after(416, Some("bytes=99999999-")));
    assert!(!copy_after(502, Some("bytes=0-1")));
}

#[test]
fn only_a_video_id_names_a_copy() {
    assert!(is_video_id(VIDEO));
    assert!(is_video_id("W_z_3ipWt3E"));
    assert!(!is_video_id("../../etc/x"));
    assert!(!is_video_id("short"));
    assert!(!is_video_id("osN-dGs2d2A/"));
}

#[tokio::test]
async fn a_whole_copy_answers_every_range_with_the_previews_own_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let preview = data(10_000);
    let source = Arc::new(FakeSource::new(preview.clone()));
    let (clock, _) = clock();
    let copies = copies(&dir, source.clone(), clock, u64::MAX);

    assert!(copies.start(VIDEO, Some(10_000)));
    wait_ready(&copies, VIDEO).await;

    let whole = copies
        .serve(VIDEO, None, Some(10_000))
        .await
        .expect("from the copy");
    assert_eq!(whole.status_code, 200);
    assert_eq!(whole.content_length, Some(10_000));
    assert_eq!(whole.content_range, None);
    assert_eq!(whole.content_type, "audio/mp4");
    assert_eq!(body(whole).await, preview);

    let part = copies
        .serve(VIDEO, Some("bytes=1310-"), None)
        .await
        .expect("from the copy");
    assert_eq!(part.status_code, 206);
    assert_eq!(part.content_range.as_deref(), Some("bytes 1310-9999/10000"));
    assert_eq!(part.content_length, Some(8690));
    assert_eq!(body(part).await, preview[1310..]);

    let probe = copies
        .serve(VIDEO, Some("bytes=0-1"), None)
        .await
        .expect("from the copy");
    assert_eq!(body(probe).await, preview[..2]);

    // A range past the end is left to the live answer, which says the size.
    assert!(copies.serve(VIDEO, Some("bytes=20000-"), None).await.is_none());
    assert_eq!(
        source.opens.load(Ordering::SeqCst),
        1,
        "one fill, however many answers"
    );
}

#[tokio::test(start_paused = true)]
async fn a_filling_copy_answers_a_range_it_has_reached_and_follows_it_as_it_grows() {
    let dir = tempfile::tempdir().unwrap();
    let preview = data(10_000);
    let mut source = FakeSource::new(preview.clone());
    source.delay = StdDuration::from_millis(100);
    let (clock, _) = clock();
    let copies = copies(&dir, Arc::new(source), clock, u64::MAX);

    assert!(copies.start(VIDEO, None));
    tokio::time::sleep(StdDuration::from_millis(250)).await;

    // Two chunks are in: the start can be answered, the end not yet.
    assert!(copies.serve(VIDEO, Some("bytes=9000-"), None).await.is_none());
    let answer = copies
        .serve(VIDEO, Some("bytes=0-"), None)
        .await
        .expect("from the filling copy");
    assert_eq!(answer.status_code, 206);
    assert_eq!(answer.content_range.as_deref(), Some("bytes 0-9999/10000"));
    assert_eq!(
        body(answer).await,
        preview,
        "every byte, in order, as the copy grew"
    );
}

#[tokio::test]
async fn a_stream_of_another_size_than_the_play_had_makes_no_copy() {
    let dir = tempfile::tempdir().unwrap();
    let (clock, _) = clock();
    let copies = copies(&dir, Arc::new(FakeSource::new(data(5_000))), clock, u64::MAX);

    assert!(copies.start(VIDEO, Some(4_999)));
    tokio::time::sleep(StdDuration::from_millis(200)).await;

    assert!(!copies.is_ready(VIDEO));
    assert_eq!(std::fs::read_dir(dir.path().join("previews")).unwrap().count(), 0);
}

#[tokio::test]
async fn a_copy_of_another_size_than_the_song_is_known_to_be_is_not_served() {
    let dir = tempfile::tempdir().unwrap();
    let (clock, _) = clock();
    let copies = copies(&dir, Arc::new(FakeSource::new(data(5_000))), clock, u64::MAX);
    copies.start(VIDEO, None);
    wait_ready(&copies, VIDEO).await;

    assert!(
        copies
            .serve(VIDEO, Some("bytes=10-"), Some(6_000))
            .await
            .is_none()
    );
    assert!(
        copies
            .serve(VIDEO, Some("bytes=10-"), Some(5_000))
            .await
            .is_some()
    );
}

#[tokio::test]
async fn a_fill_that_breaks_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = FakeSource::new(data(5_000));
    source.break_after = Some(2_000);
    let (clock, _) = clock();
    let copies = copies(&dir, Arc::new(source), clock, u64::MAX);

    copies.start(VIDEO, None);
    tokio::time::sleep(StdDuration::from_millis(200)).await;

    assert!(!copies.is_ready(VIDEO));
    assert!(copies.serve(VIDEO, Some("bytes=0-"), None).await.is_none());
    assert_eq!(std::fs::read_dir(dir.path().join("previews")).unwrap().count(), 0);
}

#[tokio::test]
async fn an_id_that_is_not_a_video_makes_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let source = Arc::new(FakeSource::new(data(100)));
    let (clock, _) = clock();
    let copies = copies(&dir, source.clone(), clock, u64::MAX);

    assert!(!copies.start("../../etc/passwd", None));
    assert_eq!(source.opens.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_second_start_while_filling_makes_no_second_fill() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = FakeSource::new(data(5_000));
    source.delay = StdDuration::from_millis(20);
    let source = Arc::new(source);
    let (clock, _) = clock();
    let copies = copies(&dir, source.clone(), clock, u64::MAX);

    assert!(copies.start(VIDEO, None));
    assert!(copies.start(VIDEO, None));
    wait_ready(&copies, VIDEO).await;
    assert!(copies.start(VIDEO, None));
    assert_eq!(source.opens.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn copies_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let preview = data(3_000);
    let (clock, _) = clock();
    {
        let copies = copies(
            &dir,
            Arc::new(FakeSource::new(preview.clone())),
            clock.clone(),
            u64::MAX,
        );
        copies.start(VIDEO, None);
        wait_ready(&copies, VIDEO).await;
    }
    let source = Arc::new(FakeSource::new(Vec::new()));
    let again = copies(&dir, source.clone(), clock, u64::MAX);
    let answer = again
        .serve(VIDEO, None, None)
        .await
        .expect("kept across the restart");
    assert_eq!(body(answer).await, preview);
    assert_eq!(source.opens.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_sweep_drops_unused_copies_then_the_oldest_over_the_cap_but_never_a_kept_one() {
    let dir = tempfile::tempdir().unwrap();
    let (clock, now) = clock();
    let copies = copies(&dir, Arc::new(FakeSource::new(data(1_000))), clock, 2_500);
    let ids = ["aaaaaaaaaaa", "bbbbbbbbbbb", "ccccccccccc", "ddddddddddd"];
    for (minute, id) in ids.iter().enumerate() {
        *now.lock() = at(minute as i64);
        copies.start(id, None);
        wait_ready(&copies, id).await;
    }
    // The oldest is held by a playlist kept ready.
    copies.keep("aaaaaaaaaaa", at(60 * 48));
    *now.lock() = at(10);

    copies.sweep();
    // 3,000 bytes not kept over a 2,500 cap: the oldest of those goes.
    assert!(copies.is_ready("aaaaaaaaaaa"), "kept");
    assert!(!copies.is_ready("bbbbbbbbbbb"), "oldest unkept");
    assert!(copies.is_ready("ccccccccccc"));
    assert!(copies.is_ready("ddddddddddd"));

    // A day later the unkept copies are unused; the kept one stays until its hold ends.
    *now.lock() = at(60 * 25);
    copies.sweep();
    assert_eq!(copies.sizes(), (1, 1_000));
    *now.lock() = at(60 * 47);
    copies.sweep();
    assert!(copies.is_ready("aaaaaaaaaaa"), "still held");
    // Once the hold ends, a copy unused for a day goes like any other.
    *now.lock() = at(60 * 48 + 1);
    copies.sweep();
    assert_eq!(copies.sizes(), (0, 0));
}

#[tokio::test]
async fn dropping_a_copy_removes_it_and_its_file_but_never_one_a_playlist_holds() {
    let dir = tempfile::tempdir().unwrap();
    let (clock, now) = clock();
    let copies = copies(&dir, Arc::new(FakeSource::new(data(1_000))), clock, 10_000);
    *now.lock() = at(0);
    for id in ["aaaaaaaaaaa", "bbbbbbbbbbb"] {
        copies.start(id, None);
        wait_ready(&copies, id).await;
    }
    copies.keep("bbbbbbbbbbb", at(60));

    copies.drop_copy("aaaaaaaaaaa");
    assert!(!copies.is_ready("aaaaaaaaaaa"));
    assert!(!dir.path().join("previews").join("aaaaaaaaaaa.preview").exists());
    assert!(dir.path().join("previews").join("bbbbbbbbbbb.preview").exists());
    assert_eq!(copies.sizes(), (1, 1_000));

    copies.drop_copy("bbbbbbbbbbb");
    assert!(copies.is_ready("bbbbbbbbbbb"), "held by a playlist");
    copies.drop_copy("ccccccccccc");
    assert_eq!(copies.sizes(), (1, 1_000));

    // Once the hold has ended the copy can go.
    *now.lock() = at(61);
    copies.drop_copy("bbbbbbbbbbb");
    assert_eq!(copies.sizes(), (0, 0));
}

/// One second of a 440 Hz tone as a mono 16-bit WAV, a preview ffmpeg can read.
fn wav() -> Vec<u8> {
    let rate = 8_000u32;
    let samples: Vec<i16> = (0..rate)
        .map(|i| {
            ((f64::from(i) * 2.0 * std::f64::consts::PI * 440.0 / f64::from(rate)).sin() * 8_000.0) as i16
        })
        .collect();
    let data = samples.len() as u32 * 2;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&(rate * 2).to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

/// The FLAC stand-in is built once from the whole copy (made for it when there is none), counts
/// toward the copies' bytes, and goes with its copy. ffmpeg is needed: without it the test fails.
#[tokio::test]
async fn a_flac_stand_in_is_built_once_counts_toward_the_cap_and_goes_with_its_copy() {
    let dir = tempfile::tempdir().unwrap();
    let (clock, _) = clock();
    let preview = wav();
    let source = Arc::new(FakeSource::new(preview.clone()));
    let copies = copies(&dir, source.clone(), clock, u64::MAX);
    let flac = copies
        .flac_stand_in(VIDEO, Some(preview.len() as u64))
        .await
        .expect("ffmpeg made the stand-in");
    assert_eq!(
        flac,
        dir.path()
            .join("previews")
            .join("flac")
            .join(format!("{VIDEO}.flac"))
    );
    assert!(std::fs::read(&flac).unwrap().starts_with(b"fLaC"));
    let flac_bytes = std::fs::metadata(&flac).unwrap().len();
    assert_eq!(copies.sizes(), (1, preview.len() as u64 + flac_bytes));
    assert_eq!(copies.built_flac(VIDEO), Some(flac.clone()));
    let built = std::fs::metadata(&flac).unwrap().modified().unwrap();
    assert_eq!(copies.flac_stand_in(VIDEO, None).await, Some(flac.clone()));
    assert_eq!(
        std::fs::metadata(&flac).unwrap().modified().unwrap(),
        built,
        "built once"
    );
    assert_eq!(source.opens.load(Ordering::SeqCst), 1);

    copies.drop_copy(VIDEO);
    assert!(!flac.exists(), "it goes with its copy");
    assert_eq!(copies.built_flac(VIDEO), None);
}

#[tokio::test]
async fn kept_copies_stop_starting_once_they_fill_their_room() {
    let dir = tempfile::tempdir().unwrap();
    let (clock, _) = clock();
    let copies = copies(&dir, Arc::new(FakeSource::new(data(1_000))), clock, u64::MAX);

    assert!(copies.kept_room(1_500));
    assert_eq!(copies.start_kept("aaaaaaaaaaa", at(60)), Copying::Started);
    wait_ready(&copies, "aaaaaaaaaaa").await;
    assert_eq!(copies.kept_bytes(), 1_000);
    assert!(copies.kept_room(1_500));
    assert_eq!(copies.start_kept("bbbbbbbbbbb", at(60)), Copying::Started);
    wait_ready(&copies, "bbbbbbbbbbb").await;
    assert!(!copies.kept_room(1_500));
}
