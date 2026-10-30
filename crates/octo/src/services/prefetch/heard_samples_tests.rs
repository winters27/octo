//! The heard samples: prints, the `fpcalc` queue, the scratch cap, copy holds and the shim's
//! rounds.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use futures::StreamExt;
use octo_core::common::Clock;
use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, HeardRecord, HeardSource, WaitingFor};
use octo_core::settings::{AppSettings, SettingsStore};
use octo_media::audio::RawFingerprint;
use parking_lot::Mutex;
use wiremock::matchers::{path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::common::soulseek_hold_store::{HeldAcquisition, HeldKind, Holder, SoulseekHoldStore};
use crate::services::prefetch::preview_copies::PreviewCopies;
use crate::services::prefetch::store::PrefetchStore;
use crate::services::soulseek::ExternalIdRegistry;
use crate::services::you_tube::YouTubeResolver;

/// Prints a file from its bytes: an `fpcalc -raw -json` answer is read as it is, anything else
/// as little-endian items, cut to `length_seconds` when one is asked.
pub(crate) struct BytePrinter;

pub(crate) fn items_of(path: &Path) -> Vec<u32> {
    let bytes = std::fs::read(path).unwrap_or_default();
    if let Ok(root) = serde_json::from_slice::<serde_json::Value>(&bytes)
        && let Some(items) = root.get("fingerprint").and_then(|f| f.as_array())
    {
        return items
            .iter()
            .filter_map(|i| i.as_u64())
            .map(|i| i as u32)
            .collect();
    }
    let (words, _) = bytes.as_chunks::<4>();
    words.iter().map(|w| u32::from_le_bytes(*w)).collect()
}

#[async_trait]
impl Printer for BytePrinter {
    async fn raw(&self, path: &Path, length_seconds: i32, _: i32) -> Option<RawFingerprint> {
        let items = items_of(path);
        if items.is_empty() {
            return None;
        }
        let seconds = items.len() as f64 * ITEM_SECONDS;
        let kept = if length_seconds > 0 {
            items
                .iter()
                .copied()
                .take((f64::from(length_seconds) / ITEM_SECONDS) as usize)
                .collect()
        } else {
            items
        };
        Some(RawFingerprint { items: kept, seconds })
    }

    async fn compressed(&self, path: &Path, _: i32, _: i32) -> Option<(String, i32)> {
        let items = items_of(path);
        (!items.is_empty()).then(|| ("AQAD".to_string(), (items.len() as f64 * ITEM_SECONDS) as i32))
    }
}

/// `seconds` of items, as bytes a [`BytePrinter`] reads back.
pub(crate) fn song_bytes(seconds: f64, seed: u32) -> Vec<u8> {
    let count = (seconds / ITEM_SECONDS) as usize;
    let mut x = seed;
    (0..count)
        .flat_map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            x.to_le_bytes()
        })
        .collect()
}

pub(crate) struct Rig {
    pub samples: Arc<HeardSamples>,
    pub copies: Arc<PreviewCopies>,
    pub registry: Arc<ExternalIdRegistry>,
    pub resumed: Arc<Mutex<Vec<String>>>,
}

/// Heard samples in `dir`, asking the shim at `shim`, printing with [`BytePrinter`].
pub(crate) fn rig_in(dir: &Path, shim: &str, holds: Arc<SoulseekHoldStore>, files: usize, bytes: u64) -> Rig {
    let youtube = Arc::new(YouTubeResolver::with_base_url(Some(shim)));
    let copies = PreviewCopies::new(dir.join("copies"), youtube.clone(), Clock::system(), || 0);
    let prefetch_dir = dir.join("prefetch");
    let prefetch = Arc::new(PrefetchStore::new(
        Arc::new(move || prefetch_dir.clone()),
        Clock::system(),
    ));
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let samples = HeardSamples::with_caps(
        HeardSampleParts {
            folder: dir.join("heard"),
            scratch: dir.join("copies").join("scratch"),
            settings: Arc::new(SettingsStore::for_tests(AppSettings::default())),
            registry: registry.clone(),
            youtube,
            copies: copies.clone(),
            prefetch,
            holds,
            clock: Clock::system(),
        },
        files,
        bytes,
    );
    samples.use_printer(Arc::new(BytePrinter));
    let resumed: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = resumed.clone();
    samples.on_resume(Arc::new(move |key: &str| seen.lock().push(key.to_string())));
    Rig {
        samples,
        copies,
        registry,
        resumed,
    }
}

fn memory_holds() -> Arc<SoulseekHoldStore> {
    Arc::new(SoulseekHoldStore::new(None::<PathBuf>))
}

fn video(id: &str, size: u64) -> HeardSource {
    HeardSource::Video {
        id: id.into(),
        size: Some(size),
        content_type: "audio/mp4".into(),
        itag: None,
    }
}

fn heard(source: HeardSource) -> HeardRecord {
    HeardRecord {
        heard_at: Utc::now(),
        source,
        sample_id: None,
    }
}

fn prints(raw: Vec<u32>, whole: bool) -> HeardPrints {
    HeardPrints {
        raw,
        compressed: "AQAD".into(),
        seconds: 200,
        whole,
        reaches_end: false,
    }
}

// ---- The fresh shim ---------------------------------------------------------------------

fn fresh_ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(r#"{"fresh":true,"size":1000}"#, "application/json")
}

fn fresh_busy() -> ResponseTemplate {
    ResponseTemplate::new(503).set_body_raw(r#"{"error":"busy","fresh":true}"#, "application/json")
}

fn fresh_gone() -> ResponseTemplate {
    ResponseTemplate::new(410).set_body_raw(
        r#"{"error":"video_unavailable","fresh":true}"#,
        "application/json",
    )
}

/// An older shim answers from its cache, without saying it resolved anything.
fn cached_ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(r#"{"size":1000}"#, "application/json")
}

async fn facts(server: &MockServer, video: &str, answer: ResponseTemplate) {
    Mock::given(path("/facts"))
        .and(query_param("id", video))
        .respond_with(answer)
        .mount(server)
        .await;
}

/// How often the shim was asked `endpoint` for `video`, in order of asking.
async fn asked(server: &MockServer, endpoint: &str) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.url.path() == endpoint)
        .filter_map(|r| {
            r.url
                .query_pairs()
                .find(|(k, _)| k == "id")
                .map(|(_, v)| v.into_owned())
        })
        .collect()
}

/// A heart held for the shim: `user` heard `video`.
fn shim_hold(holds: &SoulseekHoldStore, id: &str, user: &str, video_id: &str) -> String {
    holds
        .hold(HeldAcquisition {
            kind: HeldKind::Track,
            provider: "soulseek".into(),
            external_id: id.into(),
            requested_by: Some(user.into()),
            held_since_utc: Utc::now(),
            origin: Some(AcquisitionOrigin::Heart),
            waiting_for: Some(WaitingFor::Shim),
            holders: vec![Holder {
                user: user.into(),
                heard: Some(heard(video(video_id, 1000))),
                ..Default::default()
            }],
            ..Default::default()
        })
        .key()
}

async fn one_round(samples: &Arc<HeardSamples>) {
    samples.shim_event();
    tokio::time::timeout(Duration::from_secs(20), samples.rounds_settled())
        .await
        .expect("the round ended");
}

// ---- Prints -------------------------------------------------------------------------------

#[test]
fn a_print_is_whole_when_it_covers_the_song_or_240_seconds() {
    let items = |seconds: f64| (seconds / ITEM_SECONDS) as usize;
    assert!(covers_whole(items(240.0), 300, false, Some(300)));
    assert!(!covers_whole(items(200.0), 300, false, Some(300)));
    assert!(covers_whole(items(150.0), 150, true, None));
    assert!(covers_whole(items(150.0), 300, false, Some(150)));
    assert!(
        !covers_whole(items(60.0), 60, false, None),
        "an unknown length needs 240 s"
    );
    assert!(!covers_whole(0, 0, true, Some(10)));
}

#[test]
fn one_heard_source_has_one_sample_id_and_another_copy_another() {
    let a = sample_id_of(&video("AAAAAAAAAA1", 100));
    assert_eq!(a, sample_id_of(&video("AAAAAAAAAA1", 100)));
    assert_ne!(a, sample_id_of(&video("AAAAAAAAAA1", 101)));
    let copy = |ready_at| HeardSource::Copy {
        key: "soulseek:x".into(),
        ready_at,
        source_file: Some("a.flac".into()),
    };
    let now = Utc::now();
    assert_ne!(
        sample_id_of(&copy(Some(now))),
        sample_id_of(&copy(Some(now + chrono::Duration::seconds(1)))),
        "a re-fetch under the same key is another copy"
    );
}

#[tokio::test]
async fn a_whole_play_then_a_short_probe_of_the_same_video_keeps_the_whole_print() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig_in(
        dir.path(),
        "http://127.0.0.1:1",
        memory_holds(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    let id = sample_id_of(&video("AAAAAAAAAA1", 100));
    assert!(
        rig.samples
            .store(&id, &prints(vec![1, 2, 3], true), Some("AAAAAAAAAA1"))
    );
    assert!(
        !rig.samples
            .store(&id, &prints(vec![9], false), Some("AAAAAAAAAA1"))
    );
    assert_eq!(rig.samples.prints(&id).unwrap().raw, [1, 2, 3]);
    // A whole one does replace a whole one.
    assert!(
        rig.samples
            .store(&id, &prints(vec![4, 5], true), Some("AAAAAAAAAA1"))
    );
    assert_eq!(rig.samples.prints(&id).unwrap().raw, [4, 5]);
}

#[tokio::test]
async fn the_store_keeps_its_cap_but_never_drops_a_sample_a_hold_or_a_play_holds() {
    let dir = tempfile::tempdir().unwrap();
    let holds = memory_holds();
    let rig = rig_in(
        dir.path(),
        "http://127.0.0.1:1",
        holds.clone(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    rig.samples.set_max_samples(2);
    let held = video("AAAAAAAAAA1", 1000);
    shim_hold(&holds, "song", "anna", "AAAAAAAAAA1");
    rig.samples
        .store(&sample_id_of(&held), &prints(vec![1], true), None);
    rig.samples.hold_sample("played", "play:1");
    rig.samples.store("played", &prints(vec![2], true), None);
    rig.samples.store("third", &prints(vec![3], true), None);
    rig.samples.store("fourth", &prints(vec![4], true), None);
    assert!(
        rig.samples.prints(&sample_id_of(&held)).is_some(),
        "a hold's sample stays"
    );
    assert!(
        rig.samples.prints("played").is_some(),
        "a running play's sample stays"
    );
    assert!(rig.samples.prints("third").is_none(), "the oldest free one went");
    rig.samples.release_samples("play:1");
    rig.samples.store("fifth", &prints(vec![5], true), None);
    assert!(rig.samples.prints("played").is_none());
}

#[tokio::test]
async fn samples_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig_in(
        dir.path(),
        "http://127.0.0.1:1",
        memory_holds(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    rig.samples
        .store("kept", &prints(vec![7, 8], true), Some("AAAAAAAAAA1"));
    drop(rig);
    let again = rig_in(
        dir.path(),
        "http://127.0.0.1:1",
        memory_holds(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    assert_eq!(again.samples.prints("kept").unwrap().raw, [7, 8]);
}

// ---- The fpcalc queue and the scratch folder ------------------------------------------------

#[tokio::test]
async fn the_fpcalc_queue_runs_two_at_once_and_gives_a_freed_place_to_a_heart_first() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig_in(
        dir.path(),
        "http://127.0.0.1:1",
        memory_holds(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    let first = rig.samples.slot(FpcalcClass::Other).await;
    let _second = rig.samples.slot(FpcalcClass::Other).await;
    let order: Arc<Mutex<Vec<&str>>> = Arc::default();
    let samples = rig.samples.clone();
    let seen = order.clone();
    let play = tokio::spawn(async move {
        let _slot = samples.slot(FpcalcClass::Other).await;
        seen.lock().push("play");
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let samples = rig.samples.clone();
    let seen = order.clone();
    let heart = tokio::spawn(async move {
        let slot = samples.slot(FpcalcClass::Heart).await;
        seen.lock().push("heart");
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(slot);
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(order.lock().is_empty(), "both places are taken");
    drop(first);
    heart.await.unwrap();
    play.await.unwrap();
    assert_eq!(*order.lock(), ["heart", "play"]);
}

#[tokio::test]
async fn a_hundred_first_byte_gets_leave_at_most_16_scratch_files_within_the_byte_cap() {
    let dir = tempfile::tempdir().unwrap();
    let cap = 2 * 1024 * 1024;
    let rig = rig_in(
        dir.path(),
        "http://127.0.0.1:1",
        memory_holds(),
        SCRATCH_FILES,
        cap,
    );
    let scratch = rig.samples.scratch_folder().to_path_buf();
    let mut bodies = Vec::new();
    for i in 0..100u32 {
        let chunks: Vec<std::io::Result<Bytes>> = (0..10)
            .map(|_| Ok(Bytes::from(vec![i as u8; 32 * 1024])))
            .collect();
        let delayed = futures::stream::iter(chunks).then(|chunk| async move {
            tokio::time::sleep(Duration::from_millis(2)).await;
            chunk
        });
        let source = video(&format!("A{i:010}"), 320 * 1024);
        let capture = Capture {
            external_id: format!("song-{i}"),
            user: "anna".into(),
            record: heard(source),
            limit: SAMPLE_CAP_BYTES,
            total: Some(320 * 1024),
            song_seconds: Some(200),
            class: FpcalcClass::Other,
        };
        bodies.push(rig.samples.capture(Box::pin(delayed), capture, "m4a"));
    }
    assert!(rig.samples.scratch_use().0 <= SCRATCH_FILES);
    let watching = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let peak: Arc<Mutex<(usize, u64)>> = Arc::default();
    let (watch, still, top) = (scratch.clone(), watching.clone(), peak.clone());
    let watcher = tokio::spawn(async move {
        while still.load(std::sync::atomic::Ordering::SeqCst) {
            let mut files = 0;
            let mut bytes = 0;
            if let Ok(listing) = std::fs::read_dir(&watch) {
                for item in listing.flatten() {
                    files += 1;
                    bytes += item.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
            {
                let mut seen = top.lock();
                seen.0 = seen.0.max(files);
                seen.1 = seen.1.max(bytes);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });
    let sent: Vec<usize> = futures::future::join_all(bodies.into_iter().map(|body| async move {
        body.map(|chunk| chunk.map(|c| c.len()).unwrap_or(0))
            .fold(0, |a, b| async move { a + b })
            .await
    }))
    .await;
    assert!(sent.iter().all(|&n| n == 320 * 1024), "every play got every byte");
    for _ in 0..200 {
        if rig.samples.scratch_use().0 == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    watching.store(false, std::sync::atomic::Ordering::SeqCst);
    watcher.await.unwrap();
    let (files, bytes) = *peak.lock();
    assert!(files <= SCRATCH_FILES, "{files} scratch files at once");
    assert!(bytes <= cap, "{bytes} scratch bytes at once");
    assert_eq!(
        rig.samples.scratch_use().0,
        0,
        "every scratch file went once printed"
    );
    assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
    assert!(rig.samples.count() > 0, "the samples that fit were printed");
}

#[tokio::test]
async fn a_hearts_scratch_file_takes_the_newest_play_samples_place() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig_in(dir.path(), "http://127.0.0.1:1", memory_holds(), 2, SCRATCH_BYTES);
    let older = rig.samples.scratch(FpcalcClass::Other, "m4a").unwrap();
    let newer = rig.samples.scratch(FpcalcClass::Other, "m4a").unwrap();
    std::fs::write(newer.path(), b"x").unwrap();
    assert!(
        rig.samples.scratch(FpcalcClass::Other, "m4a").is_none(),
        "a play waits its turn"
    );
    let heart = rig
        .samples
        .scratch(FpcalcClass::Heart, "m4a")
        .expect("a heart's goes first");
    assert!(newer.is_cancelled());
    assert!(!newer.path().exists(), "the newest play sample's file went");
    assert!(!older.is_cancelled());
    assert!(!heart.reserve(SCRATCH_BYTES + 1), "never past the byte cap");
}

#[tokio::test]
async fn a_scratch_file_no_hold_names_is_gone_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig_in(
        dir.path(),
        "http://127.0.0.1:1",
        memory_holds(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    let left = rig.samples.scratch_folder().join("left-by-a-crash.m4a");
    std::fs::write(&left, b"audio").unwrap();
    drop(rig);
    let _again = rig_in(
        dir.path(),
        "http://127.0.0.1:1",
        memory_holds(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    assert!(!left.exists());
}

// ---- Copy holds -----------------------------------------------------------------------------

const HEARD: &str = "AAAAAAAAAA1";

/// A whole copy of `video`, as a fill leaves it.
fn place_copy(dir: &Path, video: &str, bytes: usize) {
    let copies = dir.join("copies");
    std::fs::create_dir_all(&copies).unwrap();
    std::fs::write(copies.join(format!("{video}.preview")), vec![0u8; bytes]).unwrap();
}

#[tokio::test]
async fn two_holders_of_one_copy_each_hold_it_and_one_release_leaves_it_held() {
    let dir = tempfile::tempdir().unwrap();
    place_copy(dir.path(), HEARD, 10);
    let rig = rig_in(
        dir.path(),
        "http://127.0.0.1:1",
        memory_holds(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    let source = video(HEARD, 10);
    assert!(rig.samples.hold_heard(&source, "heart:Track:soulseek:song:anna"));
    assert!(rig.samples.hold_heard(&source, "heart:Track:soulseek:song:ben"));
    rig.samples
        .release_heard(&source, "heart:Track:soulseek:song:anna");
    assert!(rig.copies.is_held(HEARD));
    rig.copies.drop_copy(HEARD);
    assert!(rig.copies.is_ready(HEARD), "a held copy survives a re-pin's drop");
    rig.samples
        .release_heard(&source, "heart:Track:soulseek:song:ben");
    assert!(!rig.copies.is_held(HEARD));
    rig.copies.drop_copy(HEARD);
    assert!(!rig.copies.is_ready(HEARD));
}

#[tokio::test]
async fn a_hold_that_moves_from_acoust_id_to_shim_then_a_restart_keeps_its_copy_held() {
    let dir = tempfile::tempdir().unwrap();
    place_copy(dir.path(), HEARD, 10);
    let holds_file = dir.path().join("holds.json");
    let holds = Arc::new(SoulseekHoldStore::new(Some(&holds_file)));
    let record = holds.hold(HeldAcquisition {
        kind: HeldKind::Track,
        provider: "soulseek".into(),
        external_id: "song".into(),
        requested_by: Some("anna".into()),
        held_since_utc: Utc::now(),
        origin: Some(AcquisitionOrigin::Heart),
        waiting_for: Some(WaitingFor::AcoustId),
        holders: vec![Holder {
            user: "anna".into(),
            heard: Some(heard(video(HEARD, 10))),
            ..Default::default()
        }],
        ..Default::default()
    });
    let rig = rig_in(
        dir.path(),
        "http://127.0.0.1:1",
        holds.clone(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    assert!(
        rig.samples
            .hold_heard(&video(HEARD, 10), &record.copy_holder_id("anna"))
    );
    holds.move_to(&record.key(), Some(WaitingFor::Shim)).unwrap();
    drop(rig);

    let holds = Arc::new(SoulseekHoldStore::new(Some(&holds_file)));
    let again = rig_in(
        dir.path(),
        "http://127.0.0.1:1",
        holds,
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    again.samples.rehold_copies();
    assert!(
        again.copies.is_held(HEARD),
        "the hold's own id, whatever it waits for"
    );
}

#[tokio::test]
async fn a_plays_holder_id_after_a_restart_leaves_the_copy_free() {
    let dir = tempfile::tempdir().unwrap();
    place_copy(dir.path(), HEARD, 10);
    let rig = rig_in(
        dir.path(),
        "http://127.0.0.1:1",
        memory_holds(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    assert!(rig.samples.hold_heard(&video(HEARD, 10), "play:anna:1"));
    // A heart's id that no hold names any more, as a crash mid-release leaves it.
    assert!(
        rig.samples
            .hold_heard(&video(HEARD, 10), "heart:Track:soulseek:gone:anna")
    );
    assert!(rig.copies.is_held(HEARD));
    drop(rig);
    let again = rig_in(
        dir.path(),
        "http://127.0.0.1:1",
        memory_holds(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    again.samples.rehold_copies();
    assert!(!again.copies.is_held(HEARD));
}

#[tokio::test]
async fn held_copies_over_the_cap_let_go_of_the_oldest_live_one_never_a_gone_one() {
    let shim = MockServer::start().await;
    facts(&shim, "AAAAAAAAAA1", fresh_gone()).await;
    facts(&shim, "AAAAAAAAAA2", fresh_ok()).await;
    facts(&shim, "AAAAAAAAAA3", fresh_ok()).await;
    let dir = tempfile::tempdir().unwrap();
    for video in ["AAAAAAAAAA1", "AAAAAAAAAA2", "AAAAAAAAAA3"] {
        place_copy(dir.path(), video, 100);
    }
    let youtube = Arc::new(YouTubeResolver::with_base_url(Some(&shim.uri())));
    let copies = PreviewCopies::with_cap(dir.path().join("copies"), youtube, Clock::system(), || 0, 250);
    for video in ["AAAAAAAAAA1", "AAAAAAAAAA2", "AAAAAAAAAA3"] {
        assert!(copies.hold(video, &format!("heart:Track:soulseek:{video}:anna")));
        tokio::time::sleep(Duration::from_millis(5)).await;
        copies.serve(video, Some("bytes=0-1"), None).await;
    }
    copies.sweep();
    assert_eq!(copies.sizes().0, 3, "the sweep never takes a held copy");
    let released = copies.release_held_over_cap().await;
    assert_eq!(released, ["AAAAAAAAAA2"]);
    assert!(copies.is_ready("AAAAAAAAAA1"), "its video is gone: it stays");
    assert!(copies.is_ready("AAAAAAAAAA3"));
}

// ---- The shim's rounds ------------------------------------------------------------------------

const PROBE: &str = FIXED_PROBES[0];

#[tokio::test]
async fn an_old_shim_never_counts_an_attempt() {
    let shim = MockServer::start().await;
    Mock::given(path("/facts"))
        .respond_with(cached_ok())
        .mount(&shim)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let holds = memory_holds();
    let rig = rig_in(
        dir.path(),
        &shim.uri(),
        holds.clone(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    let key = shim_hold(&holds, "song", "anna", HEARD);
    rig.samples.wait_for_shim(&key, "anna", HEARD);
    for _ in 0..4 {
        one_round(&rig.samples).await;
    }
    let record = holds.get(&key).expect("still held for the shim");
    assert!(record.holders[0].shim_tries.is_clear());
    assert!(rig.resumed.lock().is_empty());
    assert!(
        asked(&shim, "/facts").await.iter().all(|v| v != HEARD),
        "no proof, no retry"
    );
}

#[tokio::test]
async fn a_full_gate_after_a_good_probe_is_not_a_gone_attempt() {
    let shim = MockServer::start().await;
    facts(&shim, PROBE, fresh_ok()).await;
    facts(&shim, HEARD, fresh_gone()).await;
    let dir = tempfile::tempdir().unwrap();
    let holds = memory_holds();
    let rig = rig_in(
        dir.path(),
        &shim.uri(),
        holds.clone(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    let key = shim_hold(&holds, "song", "anna", HEARD);
    rig.samples.wait_for_shim(&key, "anna", HEARD);
    one_round(&rig.samples).await;
    assert!(holds.get(&key).unwrap().holders[0].shim_tries.gone_once);
    // The gate is full on the next round: not a second gone answer.
    shim.reset().await;
    facts(&shim, PROBE, fresh_ok()).await;
    facts(&shim, HEARD, fresh_busy()).await;
    one_round(&rig.samples).await;
    assert!(!rig.samples.is_gone(HEARD));
    assert!(rig.resumed.lock().is_empty());
    let record = holds.get(&key).expect("still held");
    assert!(record.holders[0].shim_tries.gone_once);
    assert_eq!(record.holders[0].shim_tries.busy_rounds, 1);
}

#[tokio::test]
async fn cached_streams_of_other_songs_prove_nothing() {
    let shim = MockServer::start().await;
    // A play's stream works; a fresh resolve of every probe only gets the cached answer.
    Mock::given(path("/stream"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8; 10]))
        .mount(&shim)
        .await;
    Mock::given(path("/facts"))
        .respond_with(cached_ok())
        .mount(&shim)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let holds = memory_holds();
    let rig = rig_in(
        dir.path(),
        &shim.uri(),
        holds.clone(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    let key = shim_hold(&holds, "song", "anna", HEARD);
    rig.samples.wait_for_shim(&key, "anna", HEARD);
    rig.samples.stream_ok();
    tokio::time::timeout(Duration::from_secs(20), rig.samples.rounds_settled())
        .await
        .expect("the round ended");
    assert!(rig.resumed.lock().is_empty());
    assert!(
        asked(&shim, "/stream").await.is_empty(),
        "a round never takes a stream as proof"
    );
    assert_eq!(holds.get(&key).unwrap().waiting_for, Some(WaitingFor::Shim));
}

#[tokio::test]
async fn a_heard_video_that_answers_gone_twice_around_a_good_probe_is_gone() {
    for refusal in [
        fresh_gone(),
        ResponseTemplate::new(403)
            .set_body_raw(r#"{"error":"video_refused","fresh":true}"#, "application/json"),
    ] {
        let shim = MockServer::start().await;
        facts(&shim, PROBE, fresh_ok()).await;
        facts(&shim, HEARD, refusal).await;
        let dir = tempfile::tempdir().unwrap();
        let holds = memory_holds();
        let rig = rig_in(
            dir.path(),
            &shim.uri(),
            holds.clone(),
            SCRATCH_FILES,
            SCRATCH_BYTES,
        );
        let key = shim_hold(&holds, "song", "anna", HEARD);
        rig.samples.wait_for_shim(&key, "anna", HEARD);
        one_round(&rig.samples).await;
        assert!(!rig.samples.is_gone(HEARD), "one answer is not gone");
        assert!(rig.resumed.lock().is_empty());
        one_round(&rig.samples).await;
        assert!(rig.samples.is_gone(HEARD));
        assert_eq!(*rig.resumed.lock(), std::slice::from_ref(&key));
        assert_eq!(rig.samples.shim_waits(), 0);
        // Its fetch now answers gone at once, without asking the shim.
        let before = asked(&shim, "/stream").await.len();
        assert!(matches!(
            rig.samples.top_up(&heard(video(HEARD, 1000)), Some(200)).await,
            TopUp::Gone
        ));
        assert_eq!(asked(&shim, "/stream").await.len(), before);
    }
}

#[tokio::test]
async fn fifty_held_hearts_and_a_start_make_at_most_three_probes_and_retry_one_at_a_time() {
    // No probe answers: three tries and nothing else.
    let shim = MockServer::start().await;
    Mock::given(path("/facts"))
        .respond_with(fresh_busy())
        .mount(&shim)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let holds = memory_holds();
    let rig = rig_in(
        dir.path(),
        &shim.uri(),
        holds.clone(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    // Five other videos were heard lately: the probes.
    for i in 0..5 {
        rig.samples.store(
            &format!("s{i}"),
            &prints(vec![1], true),
            Some(&format!("P{i:010}")),
        );
    }
    let videos: Vec<String> = (0..50).map(|i| format!("H{i:010}")).collect();
    for (i, video) in videos.iter().enumerate() {
        let user = format!("u{i}");
        let key = shim_hold(&holds, &format!("song{i}"), &user, video);
        rig.samples.wait_for_shim(&key, &user, video);
    }
    rig.samples.rehold_copies();
    one_round(&rig.samples).await;
    let probed = asked(&shim, "/facts").await;
    assert_eq!(probed.len(), 3, "{probed:?}");
    assert!(probed.iter().all(|v| v.starts_with('P')));

    // A probe answers: the held videos one at a time, oldest first, up to the first busy one.
    shim.reset().await;
    facts(&shim, "P0000000004", fresh_ok()).await;
    for video in &videos[..10] {
        facts(&shim, video, fresh_ok()).await;
    }
    Mock::given(path("/facts"))
        .respond_with(fresh_busy())
        .mount(&shim)
        .await;
    one_round(&rig.samples).await;
    let asked_now = asked(&shim, "/facts").await;
    let held_asked: Vec<&String> = asked_now.iter().filter(|v| v.starts_with('H')).collect();
    assert_eq!(held_asked.len(), 11, "ten answered, the eleventh was busy");
    assert_eq!(*held_asked[0], videos[0]);
    assert_eq!(*held_asked[10], videos[10]);
    assert_eq!(rig.resumed.lock().len(), 10);
    assert_eq!(rig.samples.shim_waits(), 40);
}

#[tokio::test]
async fn an_always_busy_head_moves_to_the_tail_and_the_next_is_tried_on_the_next_event() {
    let shim = MockServer::start().await;
    facts(&shim, PROBE, fresh_ok()).await;
    facts(&shim, "AAAAAAAAAA1", fresh_busy()).await;
    facts(&shim, "AAAAAAAAAA2", fresh_ok()).await;
    let dir = tempfile::tempdir().unwrap();
    let holds = memory_holds();
    let rig = rig_in(
        dir.path(),
        &shim.uri(),
        holds.clone(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    let first = shim_hold(&holds, "one", "anna", "AAAAAAAAAA1");
    let second = shim_hold(&holds, "two", "ben", "AAAAAAAAAA2");
    rig.samples.wait_for_shim(&first, "anna", "AAAAAAAAAA1");
    rig.samples.wait_for_shim(&second, "ben", "AAAAAAAAAA2");
    one_round(&rig.samples).await;
    assert!(rig.resumed.lock().is_empty(), "the round ended at the busy head");
    one_round(&rig.samples).await;
    assert_eq!(*rig.resumed.lock(), [second]);
    let held: Vec<String> = asked(&shim, "/facts")
        .await
        .into_iter()
        .filter(|v| v != PROBE)
        .collect();
    assert_eq!(held, ["AAAAAAAAAA1", "AAAAAAAAAA2", "AAAAAAAAAA1"]);
}

#[tokio::test]
async fn a_third_busy_round_ends_the_fetch_and_holds_not_yet_proven_with_the_copy_kept() {
    let shim = MockServer::start().await;
    facts(&shim, PROBE, fresh_ok()).await;
    facts(&shim, HEARD, fresh_busy()).await;
    let dir = tempfile::tempdir().unwrap();
    place_copy(dir.path(), HEARD, 1000);
    let holds = memory_holds();
    let rig = rig_in(
        dir.path(),
        &shim.uri(),
        holds.clone(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    let rows: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = rows.clone();
    rig.samples.on_hold_row(Arc::new(move |record: &HeldAcquisition| {
        seen.lock().push(record.key())
    }));
    let key = shim_hold(&holds, "song", "anna", HEARD);
    let record = holds.get(&key).unwrap();
    assert!(
        rig.samples
            .hold_heard(&video(HEARD, 1000), &record.copy_holder_id("anna"))
    );
    rig.samples.wait_for_shim(&key, "anna", HEARD);
    for round in 1..=2 {
        one_round(&rig.samples).await;
        assert_eq!(holds.get(&key).unwrap().holders[0].shim_tries.busy_rounds, round);
    }
    one_round(&rig.samples).await;
    assert!(holds.get(&key).is_none(), "the fetch attempt ended");
    let moved = holds
        .get("Track:soulseek:song:NotYetProven")
        .expect("held as not yet proven");
    assert_eq!(moved.line.as_deref(), Some(STUCK_LINE));
    assert!(moved.holders[0].shim_tries.is_clear());
    assert_eq!(*rows.lock(), [moved.key()]);
    assert!(rig.copies.is_held(HEARD), "the held copy is kept");
    assert!(!rig.samples.is_gone(HEARD), "never ended as gone");
    assert!(rig.resumed.lock().is_empty());
    assert_eq!(rig.samples.shim_waits(), 0);
}

#[tokio::test]
async fn a_stub_shim_whose_cached_facts_answer_but_whose_fresh_resolve_fails_keeps_the_hold_busy() {
    let shim = MockServer::start().await;
    Mock::given(path("/facts"))
        .respond_with(cached_ok())
        .mount(&shim)
        .await;
    Mock::given(path("/stream"))
        .respond_with(fresh_busy())
        .mount(&shim)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let rig = rig_in(
        dir.path(),
        &shim.uri(),
        memory_holds(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    let answer = rig.samples.top_up(&heard(video(HEARD, 1000)), Some(200)).await;
    assert_eq!(answer, TopUp::Busy);
    assert_eq!(
        rig.samples.whole_song_print(&heard(video(HEARD, 1000))).await,
        WholePrint::Busy
    );
}

#[tokio::test]
async fn a_top_up_prints_the_heard_video_once_fetched_fresh() {
    let shim = MockServer::start().await;
    let bytes = song_bytes(200.0, 3);
    Mock::given(path("/stream"))
        .and(query_param("fresh", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("X-Octo-Fresh", "1")
                .set_body_raw(bytes.clone(), "audio/mp4"),
        )
        .mount(&shim)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let rig = rig_in(
        dir.path(),
        &shim.uri(),
        memory_holds(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    let record = heard(video(HEARD, bytes.len() as u64));
    let TopUp::Ready(id) = rig.samples.top_up(&record, Some(200)).await else {
        panic!("a whole print");
    };
    assert_eq!(id, sample_id_of(&record.source));
    let stored = rig.samples.prints(&id).unwrap();
    assert!(stored.whole && stored.reaches_end);
    assert_eq!(rig.samples.scratch_use().0, 0, "the fetched audio is gone");
    // Whole now: never fetched again.
    rig.samples.top_up(&record, Some(200)).await;
    assert_eq!(asked(&shim, "/stream").await.len(), 1);
}

#[path = "heard_samples_keep_tests.rs"]
mod keep;
