use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use octo_core::fingerprint::{InconclusiveReason, VerificationResult, VerificationVerdict};
use octo_media::audio::AudioFingerprinter;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

/// A fingerprint of `seconds` of made-up sound: the same seed is the same sound.
fn sound(seed: u64, seconds: f64) -> Vec<u32> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..(seconds / ITEM_SECONDS) as usize)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u32
        })
        .collect()
}

/// The same sound as heard through another encoder: every item with `flips` of its 32 bits
/// turned over.
fn reencoded(items: &[u32], flips: u32) -> Vec<u32> {
    items
        .iter()
        .enumerate()
        .map(|(i, item)| item ^ (0..flips).fold(0u32, |mask, k| mask | 1 << ((i as u32 * 7 + k * 5) % 32)))
        .collect()
}

fn request(seconds: Option<i32>) -> PreviewRequest<'static> {
    PreviewRequest {
        artist: "Drake",
        title: "One Dance",
        seconds,
        video_id: None,
        background: false,
    }
}

fn facts(title: &str, artist: Option<&str>, seconds: Option<i32>) -> PreviewFacts {
    PreviewFacts {
        video_id: "ki0Ocze98U8".into(),
        title: Some(title.into()),
        artist: artist.map(str::to_string),
        seconds,
        album: None,
    }
}

fn verdict(verdict: VerificationVerdict, reason: InconclusiveReason) -> VerificationResult {
    VerificationResult {
        verdict,
        reason,
        ..Default::default()
    }
}

// ---- When the preview is asked ----------------------------------------------------------

#[test]
fn a_sound_acoustid_never_heard_is_always_compared() {
    let no_entry = verdict(VerificationVerdict::Inconclusive, InconclusiveReason::NoEntry);
    assert!(wanted(&no_entry, true));
    assert!(wanted(&no_entry, false));
}

#[test]
fn a_lone_link_is_always_compared() {
    let lone = verdict(VerificationVerdict::Inconclusive, InconclusiveReason::LoneLink);
    assert!(wanted(&lone, true));
    assert!(wanted(&lone, false));
}

#[test]
fn an_unknown_length_is_compared_unless_acoustid_named_the_song() {
    for reason in [
        InconclusiveReason::BelowThreshold,
        InconclusiveReason::LookupFailed,
        InconclusiveReason::NotFingerprinted,
        InconclusiveReason::Disabled,
    ] {
        let unsettled = verdict(VerificationVerdict::Inconclusive, reason);
        assert!(wanted(&unsettled, false), "{reason:?}");
        assert!(!wanted(&unsettled, true), "{reason:?} with a known length");
    }
    for reason in [InconclusiveReason::SourceDisagreed, InconclusiveReason::LengthOff] {
        assert!(
            !wanted(&verdict(VerificationVerdict::Inconclusive, reason), false),
            "{reason:?}"
        );
    }
    for settled in [VerificationVerdict::Confirmed, VerificationVerdict::Mismatch] {
        assert!(
            !wanted(&verdict(settled, InconclusiveReason::None), false),
            "{settled:?}"
        );
    }
}

// ---- Which previews are believed ----------------------------------------------------------

#[test]
fn a_preview_that_names_the_song_at_its_length_is_believed() {
    // A YouTube Music row credits the artist; the artist's own channel posts the rest.
    assert!(trustworthy(
        &request(Some(174)),
        &facts("One Dance", Some("Drake"), Some(175))
    ));
    assert!(trustworthy(
        &request(Some(174)),
        &facts("One Dance", Some("Drake - Topic"), Some(182))
    ));
    assert!(trustworthy(
        &request(Some(174)),
        &facts(
            "Drake - One Dance (Official Audio) ft. Wizkid & Kyla",
            Some("DrakeVEVO"),
            Some(173)
        )
    ));
}

/// The play path's own pick for "Drake - One Dance" is a lyrics channel's upload: the right
/// title, and still not the artist vouching for the audio.
#[test]
fn an_upload_by_anyone_but_the_artist_is_not_believed() {
    assert!(!trustworthy(
        &request(Some(174)),
        &facts(
            "Drake - One Dance (Lyrics) ft. Wizkid & Kyla",
            Some("Billion Stars"),
            Some(175)
        )
    ));
}

#[test]
fn a_preview_of_another_song_version_or_length_is_not_believed() {
    let cases = [
        ("another song", facts("Hotline Bling", Some("Drake"), Some(174))),
        ("another artist", facts("One Dance", Some("Vlad"), Some(174))),
        ("a remix", facts("One Dance (Remix)", Some("Drake"), Some(175))),
        (
            "a live take",
            facts("Drake - One Dance (Live at the O2)", Some("DrakeVEVO"), Some(176)),
        ),
        (
            "a performance",
            facts(
                "Drake - One Dance (AMAs Performance)",
                Some("DrakeVEVO"),
                Some(176),
            ),
        ),
        (
            "sped up",
            facts("Drake - One Dance (Sped Up)", Some("Billion Stars"), Some(170)),
        ),
        (
            "a channel that is not the artist",
            facts("One Dance (Lyrics)", Some("Billion Stars"), Some(175)),
        ),
        ("9 s longer", facts("One Dance", Some("Drake"), Some(183))),
        (
            "a music video's length",
            facts("Drake - One Dance (Official Video)", Some("DrakeVEVO"), Some(254)),
        ),
        ("no length", facts("One Dance", Some("Drake"), None)),
        ("bare", PreviewFacts::bare("ki0Ocze98U8")),
    ];
    for (case, preview) in cases {
        assert!(!trustworthy(&request(Some(174)), &preview), "{case}");
    }
    assert!(
        !trustworthy(&request(None), &facts("One Dance", Some("Drake"), Some(174))),
        "a song of unknown length has nothing to hold the preview to"
    );
}

// ---- Real fingerprints --------------------------------------------------------------------

/// A pair fpcalc -raw made: 120 s of a download and 240 s of a YouTube preview.
struct Pair {
    file: Vec<u32>,
    preview: Vec<u32>,
}

fn pair(json: &str) -> Pair {
    let value: serde_json::Value = serde_json::from_str(json).expect("a fixture");
    let items = |name: &str| -> Vec<u32> {
        value[name]
            .as_array()
            .expect("numbers")
            .iter()
            .map(|n| n.as_u64().expect("a number") as u32)
            .collect()
    };
    Pair {
        file: items("file"),
        preview: items("preview"),
    }
}

macro_rules! fixture {
    ($name:literal) => {
        pair(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/preview_check/",
            $name
        )))
    };
}

/// The right FLAC against its official video, which opens on six seconds of its own.
#[test]
fn a_right_file_matches_its_music_video() {
    let pair = fixture!("music-video.json");
    let comparison = compare(&pair.file, &pair.preview).expect("enough audio");
    assert!(comparison.bit_error < 0.05, "{comparison:?}");
    assert!((comparison.offset_seconds - 5.7).abs() < 1.0, "{comparison:?}");
    assert_eq!(judge(&comparison, true), Heard::Match);
}

/// #101: water drop noises kept for a well-known song AcoustID never heard.
#[test]
fn water_drops_are_a_clear_mismatch_against_a_believed_preview() {
    let pair = fixture!("water-drops.json");
    let comparison = compare(&pair.file, &pair.preview).expect("enough audio");
    assert!(comparison.bit_error >= MISMATCH_FROM, "{comparison:?}");
    assert_eq!(judge(&comparison, true), Heard::Mismatch);
}

#[test]
fn another_song_is_a_clear_mismatch_against_a_believed_preview() {
    let pair = fixture!("another-song.json");
    let comparison = compare(&pair.file, &pair.preview).expect("enough audio");
    assert_eq!(judge(&comparison, true), Heard::Mismatch, "{comparison:?}");
}

/// The right file against a live take of the song: never a refusal, believed or not.
#[test]
fn a_right_file_against_a_live_take_is_unclear() {
    let pair = fixture!("live-take.json");
    let comparison = compare(&pair.file, &pair.preview).expect("enough audio");
    assert_eq!(judge(&comparison, true), Heard::Unclear, "{comparison:?}");
    assert_eq!(judge(&comparison, false), Heard::Unclear, "{comparison:?}");
}

/// A skit over one loop: few different numbers, but a new one at nearly every item, so every
/// stretch is compared and the right file matches its preview.
#[test]
fn a_right_file_built_on_one_loop_matches_its_preview() {
    let pair = fixture!("loop-skit.json");
    let comparison = compare(&pair.file, &pair.preview).expect("enough audio");
    assert!(comparison.bit_error < 0.05, "{comparison:?}");
    assert_eq!(judge(&comparison, true), Heard::Match);
}

/// The water drops against a preview that may not be the song stay a question for a person.
#[test]
fn a_mismatch_against_a_preview_that_may_not_be_the_song_is_unclear() {
    let pair = fixture!("water-drops.json");
    let comparison = compare(&pair.file, &pair.preview).expect("enough audio");
    assert_eq!(judge(&comparison, false), Heard::Unclear, "{comparison:?}");
}

// ---- Lining the two up ------------------------------------------------------------------

#[test]
fn the_same_sound_through_another_encoder_matches() {
    let song = sound(1, 120.0);
    let comparison = compare(&song, &reencoded(&song, 3)).expect("enough audio");
    assert!(comparison.bit_error < 0.12, "{comparison:?}");
    assert!(comparison.offset_seconds.abs() < ITEM_SECONDS, "{comparison:?}");
    assert_eq!(judge(&comparison, false), Heard::Match);
}

/// A music video's intro, and a cut verse after it, still line the song up.
#[test]
fn an_intro_and_a_cut_do_not_hide_the_song() {
    let song = sound(1, 200.0);
    let at = |seconds: f64| (seconds / ITEM_SECONDS) as usize;
    let mut video = sound(9, 42.0);
    video.extend(reencoded(&song[..at(70.0)], 3));
    video.extend(reencoded(&song[at(100.0)..], 3));
    let comparison = compare(&song[..at(120.0)], &video).expect("enough audio");
    assert_eq!(judge(&comparison, true), Heard::Match, "{comparison:?}");
    assert!((comparison.offset_seconds - 42.0).abs() < 0.5, "{comparison:?}");
}

/// The album take's own intro runs before the preview starts.
#[test]
fn a_preview_that_starts_later_still_lines_up() {
    let song = sound(4, 180.0);
    let at = |seconds: f64| (seconds / ITEM_SECONDS) as usize;
    let comparison = compare(&song[..at(120.0)], &reencoded(&song[at(25.0)..], 2)).expect("enough audio");
    assert_eq!(judge(&comparison, true), Heard::Match, "{comparison:?}");
    assert!((comparison.offset_seconds + 25.0).abs() < 0.5, "{comparison:?}");
}

#[test]
fn another_sound_is_a_clear_mismatch_only_against_a_believed_preview() {
    let comparison = compare(&sound(1, 120.0), &sound(2, 200.0)).expect("enough audio");
    assert!(comparison.bit_error > 0.4, "{comparison:?}");
    assert_eq!(judge(&comparison, true), Heard::Mismatch);
    assert_eq!(judge(&comparison, false), Heard::Unclear);
}

#[test]
fn between_the_thresholds_is_unclear() {
    let comparison = Comparison {
        bit_error: (MATCH_AT_MOST + MISMATCH_FROM) / 2.0,
        offset_seconds: 0.0,
    };
    assert_eq!(judge(&comparison, true), Heard::Unclear);
}

#[test]
fn too_little_audio_is_not_compared() {
    assert_eq!(compare(&sound(1, 20.0), &sound(1, 200.0)), None);
    assert_eq!(compare(&sound(1, 120.0), &[]), None);
}

/// A held tone: one number over and over.
fn tone(seconds: f64) -> Vec<u32> {
    vec![0x5A5A_5A5A; (seconds / ITEM_SECONDS) as usize]
}

/// Silence in both is not the same song: the best stretch of a song with a quiet gap has to be
/// one with sound in it.
#[test]
fn a_silent_stretch_in_both_does_not_pass_for_the_same_song() {
    let mut file = tone(40.0);
    file.extend(sound(1, 80.0));
    let mut preview = tone(40.0);
    preview.extend(sound(2, 150.0));
    let comparison = compare(&file, &preview).expect("enough audio");
    // A stretch may be up to 30% silence, and that part agrees by itself: unclear, never a match.
    assert!(comparison.bit_error > MATCH_AT_MOST, "{comparison:?}");
    assert_ne!(judge(&comparison, true), Heard::Match);
}

/// The sound after a silent gap still lines up with the song.
#[test]
fn a_right_file_with_a_silent_gap_still_matches() {
    let song = sound(3, 150.0);
    let mut file = tone(35.0);
    file.extend(&song);
    let comparison = compare(&file, &reencoded(&song, 2)).expect("enough audio");
    assert_eq!(judge(&comparison, true), Heard::Match, "{comparison:?}");
}

#[test]
fn a_silent_file_is_not_the_song() {
    let comparison = compare(&tone(120.0), &sound(1, 200.0)).expect("a preview with sound");
    assert_eq!(judge(&comparison, true), Heard::Mismatch, "{comparison:?}");
}

#[test]
fn a_silent_preview_says_nothing() {
    assert_eq!(compare(&sound(1, 120.0), &tone(200.0)), None);
}

/// A download that is all held tone against a preview that opens on the same tone could be the
/// same recording, so it is not refused.
#[test]
fn a_quiet_download_says_nothing_against_a_preview_as_quiet_where_they_line_up() {
    let mut preview = tone(60.0);
    preview.extend(sound(1, 180.0));
    assert_eq!(compare(&tone(120.0), &preview), None);
}

/// A song built on one short loop: few different numbers, but a new one at nearly every item.
fn looped(seed: u64, seconds: f64) -> Vec<u32> {
    let pattern = sound(seed, 5.0);
    pattern
        .iter()
        .copied()
        .cycle()
        .take((seconds / ITEM_SECONDS) as usize)
        .collect()
}

/// A loop that keeps coming back, like a skit over one sample, is compared like any music.
#[test]
fn a_looping_song_is_still_compared() {
    let song = looped(5, 150.0);
    let comparison = compare(&song[..song.len() * 4 / 5], &reencoded(&song, 2)).expect("enough audio");
    assert_eq!(judge(&comparison, true), Heard::Match, "{comparison:?}");
    let other = compare(&looped(6, 120.0), &song).expect("enough audio");
    assert_eq!(judge(&other, true), Heard::Mismatch, "{other:?}");
}

// ---- The check end to end ---------------------------------------------------------------

/// A shim that knows the song's video but cannot send it, counting what it was asked.
struct DownShim {
    fetches: AtomicUsize,
}

#[async_trait]
impl PreviewSource for DownShim {
    async fn find(&self, _request: &PreviewRequest<'_>) -> Result<Option<PreviewFacts>, Failure> {
        Ok(Some(facts("One Dance", Some("Drake"), Some(174))))
    }

    async fn fetch(&self, _video_id: &str, _dest: &Path) -> bool {
        self.fetches.fetch_add(1, Ordering::Relaxed);
        false
    }
}

/// A file fpcalc cannot read is its own answer, whatever the shim can send.
#[tokio::test]
async fn a_file_fpcalc_cannot_read_is_its_own_answer() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let broken = dir.path().join("broken.flac");
    std::fs::write(&broken, b"not audio").expect("written");
    let shim = DownShim {
        fetches: AtomicUsize::new(0),
    };
    let outcome = check(
        &shim,
        &AudioFingerprinter::new(),
        &broken,
        &request(Some(174)),
        30,
    )
    .await;
    assert_eq!(outcome, PreviewOutcome::FileUnreadable);
    assert_eq!(shim.fetches.load(Ordering::Relaxed), 0);
}

/// A shim that is down changes nothing: the file is judged as it was before.
#[tokio::test]
async fn a_shim_that_cannot_send_the_preview_leaves_the_file_alone() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let Some(path) = noise_flac(dir.path()) else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    let shim = DownShim {
        fetches: AtomicUsize::new(0),
    };
    let outcome = check(&shim, &AudioFingerprinter::new(), &path, &request(Some(174)), 30).await;
    assert!(matches!(outcome, PreviewOutcome::NotSent(_)), "{outcome:?}");
    assert_eq!(shim.fetches.load(Ordering::Relaxed), 1);
}

/// A shim that counts which lane its preview was asked for in.
#[derive(Default)]
struct CountingShim {
    fetches: AtomicUsize,
    background_fetches: AtomicUsize,
}

#[async_trait]
impl PreviewSource for CountingShim {
    async fn find(&self, _request: &PreviewRequest<'_>) -> Result<Option<PreviewFacts>, Failure> {
        Ok(Some(facts("One Dance", Some("Drake"), Some(174))))
    }

    async fn fetch(&self, _video_id: &str, _dest: &Path) -> bool {
        self.fetches.fetch_add(1, Ordering::Relaxed);
        false
    }

    async fn fetch_in_background(&self, _video_id: &str, _dest: &Path) -> bool {
        self.background_fetches.fetch_add(1, Ordering::Relaxed);
        false
    }
}

/// Work nobody waits on fetches its preview in the shim's background lane, and a play's in the
/// foreground.
#[tokio::test]
async fn the_background_lane_is_used_only_when_asked_for() {
    let shim = CountingShim::default();
    let fingerprinter = AudioFingerprinter::new();
    assert!(
        fingerprint_preview(&shim, &fingerprinter, "vid", 30, true)
            .await
            .is_err()
    );
    assert_eq!(shim.background_fetches.load(Ordering::Relaxed), 1);
    assert_eq!(shim.fetches.load(Ordering::Relaxed), 0);
    assert!(
        fingerprint_preview(&shim, &fingerprinter, "vid", 30, false)
            .await
            .is_err()
    );
    assert_eq!(shim.fetches.load(Ordering::Relaxed), 1);
}

/// A shim whose fetch writes a few junk bytes and says it sent the video.
struct JunkShim;

#[async_trait]
impl PreviewSource for JunkShim {
    async fn find(&self, _request: &PreviewRequest<'_>) -> Result<Option<PreviewFacts>, Failure> {
        Ok(Some(facts("One Dance", Some("Drake"), Some(174))))
    }

    async fn fetch(&self, _video_id: &str, dest: &Path) -> bool {
        std::fs::write(dest, b"not audio at all").is_ok()
    }
}

/// Audio that arrives but cannot be read is a different failure from audio that never arrives.
#[tokio::test]
async fn a_preview_that_arrives_but_cannot_be_read_is_unavailable_not_retried() {
    let fingerprinter = AudioFingerprinter::new();
    let on_path = std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            dir.join(format!("fpcalc{}", std::env::consts::EXE_SUFFIX))
                .is_file()
        })
    });
    if !on_path {
        eprintln!("skipped: fpcalc is not on the PATH");
        return;
    }
    let failure = fingerprint_preview(&JunkShim, &fingerprinter, "vid", 30, false)
        .await
        .err();
    assert_eq!(failure, Some(PreviewFailure::Unreadable));
}

/// A shim whose lane is full says so with HTTP 503 and `{"error":"busy"}`; any other refusal is
/// a preview that was not sent. Neither needs fpcalc.
#[tokio::test]
async fn a_full_background_lane_is_busy_and_any_other_refusal_is_not_sent() {
    let shim = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/stream"))
        .and(query_param("id", "full"))
        .respond_with(ResponseTemplate::new(503).set_body_string(r#"{"error":"busy"}"#))
        .mount(&shim)
        .await;
    Mock::given(method("GET"))
        .and(path("/stream"))
        .and(query_param("id", "broken"))
        .respond_with(ResponseTemplate::new(502).set_body_string(r#"{"error":"ytdlp_failed"}"#))
        .mount(&shim)
        .await;
    Mock::given(method("GET"))
        .and(path("/stream"))
        .and(query_param("id", "unsaid"))
        .respond_with(ResponseTemplate::new(503).set_body_string("try later"))
        .mount(&shim)
        .await;
    let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
    let dest = std::env::temp_dir().join(format!("octo-preview-test-{}", std::process::id()));
    assert_eq!(
        resolver.fetch_preview("full", &dest, true).await,
        Err(PreviewFailure::Busy)
    );
    assert_eq!(
        resolver.fetch_preview("broken", &dest, true).await,
        Err(PreviewFailure::NotSent)
    );
    assert_eq!(
        resolver.fetch_preview("unsaid", &dest, true).await,
        Err(PreviewFailure::NotSent)
    );
    assert!(!dest.exists());
}

/// 40 seconds of pink noise as FLAC, or None where ffmpeg or fpcalc is missing.
fn noise_flac(dir: &Path) -> Option<std::path::PathBuf> {
    let on_path = |program: &str| {
        std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                dir.join(format!("{program}{}", std::env::consts::EXE_SUFFIX))
                    .is_file()
            })
        })
    };
    if !on_path("ffmpeg") || !on_path("fpcalc") {
        return None;
    }
    let path = dir.join("noise.flac");
    let made = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("anoisesrc=color=pink:duration=40:sample_rate=44100:amplitude=0.3")
        .arg(&path)
        .status()
        .is_ok_and(|status| status.success());
    made.then_some(path)
}

/// The real shim client against an address nothing answers on could not be asked, which is not
/// "no video".
#[tokio::test]
async fn an_unreachable_shim_finds_no_preview() {
    let closed = format!("http://{}", crate::services::test_support::refused_address());
    let resolver = YouTubeResolver::with_base_url(Some(&closed));
    assert_eq!(
        resolver.find(&request(Some(174))).await,
        Err(Failure::Unreachable)
    );
    assert!(
        !resolver
            .fetch("ki0Ocze98U8", Path::new("/nonexistent/preview"))
            .await
    );

    // A pinned video is still the preview, known by its id alone.
    let pinned = PreviewRequest {
        video_id: Some("ki0Ocze98U8"),
        ..request(Some(174))
    };
    assert_eq!(
        resolver.find(&pinned).await,
        Ok(Some(PreviewFacts::bare("ki0Ocze98U8")))
    );
}

/// A shim with `/ytm/search` and `/search` answering as given.
async fn lookup_shim(ytm: ResponseTemplate, search: ResponseTemplate) -> MockServer {
    let shim = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ytm/search"))
        .respond_with(ytm)
        .mount(&shim)
        .await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(search)
        .mount(&shim)
        .await;
    shim
}

/// YouTube Music busy and yt-dlp failing: the shim could not be asked, which is never "no video".
#[tokio::test]
async fn a_shim_that_cannot_answer_is_an_error_not_no_video() {
    let shim = lookup_shim(
        ResponseTemplate::new(503).set_body_string(r#"{"error":"busy"}"#),
        ResponseTemplate::new(502).set_body_string(r#"{"error":"search_failed"}"#),
    )
    .await;
    let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
    assert_eq!(resolver.find(&request(Some(174))).await, Err(Failure::Busy));
    let outcome = check(
        &resolver,
        &AudioFingerprinter::new(),
        Path::new("/nonexistent/song.flac"),
        &request(Some(174)),
        30,
    )
    .await;
    assert_eq!(outcome, PreviewOutcome::Busy);
}

/// Neither answering failure is busy: the check says the shim could not be asked.
#[tokio::test]
async fn a_shim_that_fails_without_saying_busy_is_not_sent() {
    let shim = lookup_shim(
        ResponseTemplate::new(502).set_body_string(r#"{"error":"ytm_failed"}"#),
        ResponseTemplate::new(502).set_body_string(r#"{"error":"search_failed"}"#),
    )
    .await;
    let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
    assert_eq!(resolver.find(&request(Some(174))).await, Err(Failure::Other));
    let outcome = check(
        &resolver,
        &AudioFingerprinter::new(),
        Path::new("/nonexistent/song.flac"),
        &request(Some(174)),
        30,
    )
    .await;
    assert_eq!(outcome, PreviewOutcome::Unreachable);
}

/// Each way a lookup fails comes to the outcome the check defines for it.
#[test]
fn each_lookup_failure_maps_to_its_preview_outcome() {
    assert_eq!(outcome_of(Failure::Busy), PreviewOutcome::Busy);
    assert_eq!(outcome_of(Failure::Unreachable), PreviewOutcome::Unreachable);
    assert_eq!(outcome_of(Failure::Other), PreviewOutcome::Unreachable);
    assert_eq!(outcome_of(Failure::Gone), PreviewOutcome::Unreachable);
    assert_eq!(outcome_of(Failure::Refused), PreviewOutcome::Unreachable);
}

/// Both lookups answering with nothing is the one case that is "no video".
#[tokio::test]
async fn two_lookups_that_answer_with_nothing_are_no_video() {
    let shim = lookup_shim(
        ResponseTemplate::new(200).set_body_string(r#"{"tracks":[]}"#),
        ResponseTemplate::new(404).set_body_string(r#"{"error":"no_hit"}"#),
    )
    .await;
    let resolver = YouTubeResolver::with_base_url(Some(&shim.uri()));
    assert_eq!(resolver.find(&request(Some(174))).await, Ok(None));
    let outcome = check(
        &resolver,
        &AudioFingerprinter::new(),
        Path::new("/nonexistent/song.flac"),
        &request(Some(174)),
        30,
    )
    .await;
    assert_eq!(
        outcome,
        PreviewOutcome::Unavailable("no YouTube video for the song".into())
    );
}
