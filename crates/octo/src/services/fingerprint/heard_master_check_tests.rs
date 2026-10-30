//! The heard-master runner: rounds, heard copies, decodes and their timeout.

use std::f64::consts::PI;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use octo_core::fingerprint::heart_gate::{HeardMaster, HeardRecord, HeardSource};
use parking_lot::Mutex;
use wiremock::matchers::{path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::common::soulseek_hold_store::SoulseekHoldStore;
use crate::services::prefetch::heard_samples::tests::{Rig, rig_in};

/// A decoder whose every decode ends at once with nothing: each check is quick and undecided.
struct Empty;

impl Decoder for Empty {
    fn open(&self, _: &Path) -> std::io::Result<DecodeRun> {
        Ok(DecodeRun {
            output: Box::new(std::io::empty()),
            stop: Box::new(|| {}),
            wait: Box::new(|| true),
        })
    }
}

/// A decoder that never sends a byte and never ends until it is stopped, as an ffmpeg stuck on a
/// broken file: a read waits until the stop.
struct Stuck;

struct NeverEnds(Arc<std::sync::atomic::AtomicBool>);

impl std::io::Read for NeverEnds {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        while !self.0.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(0)
    }
}

impl Decoder for Stuck {
    fn open(&self, _: &Path) -> std::io::Result<DecodeRun> {
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = stopped.clone();
        Ok(DecodeRun {
            output: Box::new(NeverEnds(stopped)),
            stop: Box::new(move || stop.store(true, std::sync::atomic::Ordering::SeqCst)),
            wait: Box::new(|| false),
        })
    }
}

const VIDEO: &str = "AAAAAAAAAA1";

fn heard_video() -> HeardAudio {
    HeardAudio::Heard(HeardRecord {
        heard_at: Utc::now(),
        source: HeardSource::Video {
            id: VIDEO.into(),
            size: Some(4096),
            content_type: "audio/mp4".into(),
            itag: None,
        },
        sample_id: None,
    })
}

/// A shim whose fresh `/stream` of any video sends 4 KB, after `delay`.
async fn shim(delay: Duration) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(path("/stream"))
        .and(query_param("fresh", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("X-Octo-Fresh", "1")
                .set_body_raw(vec![7u8; 4096], "audio/mp4")
                .set_delay(delay),
        )
        .mount(&server)
        .await;
    server
}

fn check_on(rig: &Rig) -> HeardMasterCheck {
    let check = HeardMasterCheck::new(rig.samples.clone());
    check.use_decoder(Arc::new(Empty));
    check
}

fn rig(dir: &Path, shim: &str) -> Rig {
    rig_in(
        dir,
        shim,
        Arc::new(SoulseekHoldStore::new(None::<PathBuf>)),
        16,
        64 << 20,
    )
}

async fn fetches(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == "/stream")
        .count()
}

#[tokio::test]
async fn five_candidates_in_one_round_make_one_fetch() {
    let server = shim(Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(dir.path(), &server.uri());
    let check = check_on(&rig);
    let file = dir.path().join("candidate.flac");
    std::fs::write(&file, b"x").unwrap();
    let guard = check.open_round("Track:soulseek:help", "Anna", heard_video());
    let round = check
        .round("Track:soulseek:help", "anna")
        .expect("found by key and person");
    for _ in 0..5 {
        assert_eq!(round.check(&file).await, HeardMaster::Differs);
    }
    assert_eq!(fetches(&server).await, 1);
    assert_eq!(rig.samples.scratch_use().0, 1, "one heard copy for the round");
    drop(guard);
    assert_eq!(rig.samples.scratch_use().0, 0, "the round's copy goes with it");
    assert!(check.round("Track:soulseek:help", "anna").is_none());
    assert_eq!(check.free_copy_places(), HEARD_COPIES_AT_ONCE);
}

#[tokio::test]
async fn a_paused_round_frees_its_place_and_makes_its_copy_again() {
    let server = shim(Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(dir.path(), &server.uri());
    let check = check_on(&rig);
    let file = dir.path().join("candidate.flac");
    std::fs::write(&file, b"x").unwrap();
    let guard = check.open_round("Track:soulseek:help", "anna", heard_video());
    let round = guard.round();
    round.check(&file).await;
    assert_eq!(check.free_copy_places(), HEARD_COPIES_AT_ONCE - 1);
    assert!(round.has_copy());
    // The round waits on a Lidarr follow: its copy goes and its place is free.
    round.pause();
    assert_eq!(check.free_copy_places(), HEARD_COPIES_AT_ONCE);
    assert_eq!(rig.samples.scratch_use().0, 0);
    round.check(&file).await;
    assert_eq!(fetches(&server).await, 2, "the next check fetches it again");
}

#[tokio::test]
async fn fifty_held_hearts_rounds_never_leave_more_than_two_heard_copies() {
    let server = shim(Duration::from_millis(20)).await;
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(dir.path(), &server.uri());
    let check = Arc::new(check_on(&rig));
    let file = dir.path().join("candidate.flac");
    std::fs::write(&file, b"x").unwrap();
    let scratch = rig.samples.scratch_folder().to_path_buf();
    let peak = Arc::new(Mutex::new(0usize));
    let watching = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (seen, still, folder) = (peak.clone(), watching.clone(), scratch.clone());
    let watcher = tokio::spawn(async move {
        while still.load(std::sync::atomic::Ordering::SeqCst) {
            let files = std::fs::read_dir(&folder).map(|d| d.count()).unwrap_or(0);
            {
                let mut top = seen.lock();
                *top = (*top).max(files);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });
    let rounds = (0..50).map(|i| {
        let check = check.clone();
        let file = file.clone();
        async move {
            let guard = check.open_round(&format!("Track:soulseek:song{i}"), "anna", heard_video());
            let answer = guard.round().check(&file).await;
            drop(guard);
            answer
        }
    });
    let answers = futures::future::join_all(rounds).await;
    watching.store(false, std::sync::atomic::Ordering::SeqCst);
    watcher.await.unwrap();
    assert!(answers.iter().all(|a| *a == HeardMaster::Differs));
    assert!(
        *peak.lock() <= HEARD_COPIES_AT_ONCE,
        "{} heard copies at once",
        peak.lock()
    );
    assert_eq!(fetches(&server).await, 50);
    assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
}

#[tokio::test]
async fn a_busy_shim_holds_and_a_heard_file_that_is_gone_says_so() {
    let server = MockServer::start().await;
    Mock::given(path("/stream"))
        .respond_with(
            ResponseTemplate::new(503).set_body_raw(r#"{"error":"busy","fresh":true}"#, "application/json"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(dir.path(), &server.uri());
    let check = check_on(&rig);
    let file = dir.path().join("candidate.flac");
    std::fs::write(&file, b"x").unwrap();
    assert_eq!(check.check_once(heard_video(), &file).await, HeardMaster::Busy);
    assert_eq!(check.free_copy_places(), HEARD_COPIES_AT_ONCE);
    assert_eq!(
        check
            .check_once(HeardAudio::File(dir.path().join("gone.flac")), &file)
            .await,
        HeardMaster::HeardGone
    );
}

#[tokio::test]
async fn a_decode_that_never_ends_is_undecided_within_the_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(dir.path(), "http://127.0.0.1:1");
    let check = HeardMasterCheck::new(rig.samples.clone());
    check.use_decoder(Arc::new(Stuck));
    check.set_decode_timeout(Duration::from_millis(500));
    let heard = dir.path().join("heard.m4a");
    let file = dir.path().join("candidate.flac");
    std::fs::write(&heard, b"x").unwrap();
    std::fs::write(&file, b"x").unwrap();
    let started = Instant::now();
    assert_eq!(
        check.check_once(HeardAudio::File(heard), &file).await,
        HeardMaster::Undecided
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

// ---- The real decoder ------------------------------------------------------------------------

const SONG_RATE: u32 = 22_050;

/// A synthetic song: six beating sine voices panned across the field and drum hits of decaying
/// noise at irregular times, with `silence` (seconds from, to) muted.
fn song(seconds: f64, silence: Option<(f64, f64)>) -> Vec<[f32; 2]> {
    let rate = f64::from(SONG_RATE);
    let voices = [
        (110.0, 0.5, -0.8),
        (165.0, 0.75, -0.4),
        (220.0, 1.0, 0.0),
        (330.0, 1.25, 0.3),
        (440.0, 1.5, 0.6),
        (660.0, 2.0, 0.9),
    ];
    let n = (seconds * rate) as usize;
    let mut x: u32 = 99;
    let mut noise = || {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(x) / 4_294_967_296.0 * 2.0 - 1.0
    };
    let mut hits = vec![0.2];
    while *hits.last().unwrap() < seconds {
        let gap = 0.3 + 0.4 * (noise() + 1.0) / 2.0;
        hits.push(hits.last().unwrap() + gap);
    }
    let mut last = 0;
    (0..n)
        .map(|i| {
            let t = i as f64 / rate;
            let mut frame = [0.0f64; 2];
            for &(frequency, beat, pan) in &voices {
                let value =
                    0.15 * (0.5 + 0.5 * (2.0 * PI * beat * t).cos()) * (2.0 * PI * frequency * t).sin();
                frame[0] += value * ((pan + 1.0) * PI / 4.0).cos();
                frame[1] += value * ((pan + 1.0) * PI / 4.0).sin();
            }
            while last + 1 < hits.len() && hits[last + 1] <= t {
                last += 1;
            }
            if t >= hits[0] {
                let drum = noise() * 0.3 * (-(t - hits[last]) / 0.05).exp();
                frame[0] += drum;
                frame[1] += drum;
            }
            if silence.is_some_and(|(from, to)| t >= from && t < to) {
                frame = [0.0; 2];
            }
            [frame[0] as f32, frame[1] as f32]
        })
        .collect()
}

/// 16-bit stereo PCM in a WAV file.
fn write_wav(path: &Path, frames: &[[f32; 2]]) {
    let data = frames.len() as u32 * 4;
    let mut bytes = Vec::with_capacity(44 + data as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&SONG_RATE.to_le_bytes());
    bytes.extend_from_slice(&(SONG_RATE * 4).to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    for frame in frames {
        for sample in frame {
            let value = (sample.clamp(-1.0, 1.0) * 32_767.0) as i16;
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    std::fs::write(path, bytes).unwrap();
}

/// `wav` as an MP3 at 128 kb/s, by ffmpeg. Fails the test when ffmpeg is missing: the gate cannot
/// pass without it.
fn mp3_of(wav: &Path, mp3: &Path) {
    let status = std::process::Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-nostdin", "-i"])
        .arg(wav)
        .args(["-c:a", "libmp3lame", "-b:a", "128k"])
        .arg(mp3)
        .status()
        .expect("ffmpeg must be on the PATH for the heard-master check");
    assert!(status.success(), "ffmpeg could not make the MP3");
}

#[tokio::test]
async fn an_mp3_of_the_heard_master_passes_and_the_same_with_a_muted_moment_fails() {
    let dir = tempfile::tempdir().unwrap();
    let rig = rig(dir.path(), "http://127.0.0.1:1");
    let check = HeardMasterCheck::new(rig.samples.clone());
    check.set_decode_timeout(Duration::from_secs(60));
    let heard = dir.path().join("heard.wav");
    write_wav(&heard, &song(45.0, None));
    let copy = dir.path().join("copy.mp3");
    mp3_of(&heard, &copy);
    let muted_wav = dir.path().join("muted.wav");
    write_wav(&muted_wav, &song(45.0, Some((20.0, 20.6))));
    let muted = dir.path().join("muted.mp3");
    mp3_of(&muted_wav, &muted);

    assert_eq!(
        check.check_once(HeardAudio::File(heard.clone()), &copy).await,
        HeardMaster::Same
    );
    assert_eq!(
        check.check_once(HeardAudio::File(heard), &muted).await,
        HeardMaster::Differs
    );
}
