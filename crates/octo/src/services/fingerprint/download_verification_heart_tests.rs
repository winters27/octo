//! The heart gate built in Octo: what the heard audio's own answer says of R, the clip check in its
//! place, holds before any source runs, a backed-off AcoustID never waited out, a family request
//! that never holds, the context pick, and a held lookup asked again at the limiter's pace.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use octo_core::fingerprint::acoust_id_client::parse_answer;
use octo_core::fingerprint::heart_release::{CatalogAlbum, CatalogAlbumHit, CatalogTrack};
use octo_core::settings::{AppSettings, SoulseekSettings};
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use parking_lot::Mutex;
use url::Url;
use wiremock::matchers::{any, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::common::{HeldAcquisition, HeldKind};
use crate::services::fingerprint::acoust_id_rate_limit_handler::AcoustIdRateLimitHandler;
use crate::services::fingerprint::acoust_id_rate_limiter::{AcoustIdRateLimiter, BusyKind, CallEnd, KeyLane};
use crate::services::fingerprint::music_brainz_client::MusicBrainzClient;
use crate::services::metadata::deezer_metadata_service::{DeezerAsked, DeezerMetadataService};
use crate::services::metadata::deezer_rate_limit_handler::DeezerRateLimitHandler;
use crate::services::metadata::deezer_rate_limiter::DeezerRateLimiter;
use crate::services::prefetch::heard_samples::HeardPrints;
use crate::services::prefetch::heard_samples::tests::{Rig, rig_in, song_bytes};
use crate::services::you_tube::YouTubeResolver;

const OCTO_KEY: &str = "octo-key";
const VIDEO: &str = "AAAAAAAAAA1";
const HELP_2009: &str = "4416d38e-0f68-4c75-93b9-c1a793fccb49";
const HELP_2015: &str = "01b6e4de-ff48-4ec0-862e-cd623c924d7b";

/// Deezer, never asked by these tests' identities.
struct NoCatalog;

#[async_trait]
impl CatalogQuestions for NoCatalog {
    async fn album_search(&self, _: &str, _: &str, _: Lane) -> DeezerAsked<Vec<CatalogAlbumHit>> {
        DeezerAsked::NoData
    }

    async fn album(&self, _: &str, _: Lane) -> DeezerAsked<(CatalogAlbum, Vec<CatalogTrack>)> {
        DeezerAsked::NoData
    }
}

struct Heart {
    service: Arc<DownloadVerificationService>,
    rig: Rig,
    holds: Arc<SoulseekHoldStore>,
    /// AcoustID.
    acoust_id: MockServer,
    /// The shim and Deezer.
    _shim: MockServer,
    _deezer: MockServer,
    limiter: Arc<AcoustIdRateLimiter>,
    _dir: tempfile::TempDir,
}

/// The gate's service over Octo's own AcoustID key at `acoust_id`, the limiter's person gap
/// starting at `person`, MusicBrainz at `music_brainz` when given, and a Deezer that has no data.
async fn heart(person: Duration, music_brainz: Option<Arc<MusicBrainzClient>>) -> Heart {
    let acoust_id = MockServer::start().await;
    let shim = MockServer::start().await;
    let deezer = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"error":{"type":"DataException","message":"no data","code":800}}"#,
            "application/json",
        ))
        .mount(&deezer)
        .await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let holds = Arc::new(SoulseekHoldStore::new(None::<PathBuf>));
    let rig = rig_in(dir.path(), &shim.uri(), holds.clone(), 16, 64 << 20);
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        soulseek: SoulseekSettings {
            acoust_id_timeout_seconds: 2,
            ..Default::default()
        },
        ..Default::default()
    }));
    let limiter =
        Arc::new(AcoustIdRateLimiter::with_gaps(person, Duration::from_secs(20)).with_jitter(|| 1.0));
    let base = Url::parse(&format!("{}/", acoust_id.uri())).expect("parses");
    let client = Arc::new(AcoustIdClient::with_built_in_key(
        Arc::new(AcoustIdRateLimitHandler::with_base_url(limiter.clone(), base)),
        settings.clone(),
        Some(OCTO_KEY),
    ));
    let service = Arc::new(DownloadVerificationService::new(
        Arc::new(AudioFingerprinter::new()),
        client,
        settings.clone(),
        music_brainz,
        None,
    ));
    let clip = CatalogClip::new(
        Arc::new(DeezerMetadataService::with_base_url(
            Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new()))),
            settings,
            deezer.uri(),
        )),
        rig.samples.clone(),
        Arc::new(YouTubeResolver::with_base_url(Some(&shim.uri()))),
    );
    service.use_heart(HeartParts {
        registry: rig.registry.clone(),
        samples: rig.samples.clone(),
        master: Arc::new(HeardMasterCheck::new(rig.samples.clone())),
        clip: Arc::new(clip),
        catalog: Arc::new(NoCatalog),
        holds: holds.clone(),
        rejected: Arc::new(RejectedPeerRegistry::in_memory()),
    });
    Heart {
        service,
        rig,
        holds,
        acoust_id,
        _shim: shim,
        _deezer: deezer,
        limiter,
        _dir: dir,
    }
}

fn help(duration: Option<i32>) -> SoulseekRouting {
    SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("The Beatles".into()),
        title: Some("Help!".into()),
        duration,
        you_tube_id: Some(VIDEO.into()),
        ..Default::default()
    }
}

/// `user` heard `VIDEO`, whose whole print is stored.
fn heard_help(heart: &Heart, reaches_end: bool) -> HeardRecord {
    let source = HeardSource::Video {
        id: VIDEO.into(),
        size: Some(1000),
        content_type: "audio/mp4".into(),
        itag: None,
    };
    let sample_id = sample_id_of(&source);
    heart.rig.samples.store(
        &sample_id,
        &HeardPrints {
            raw: (0..1200u32).collect(),
            compressed: "AQADheard".into(),
            seconds: 139,
            whole: true,
            reaches_end,
        },
        Some(VIDEO),
    );
    HeardRecord {
        heard_at: Utc::now(),
        source,
        sample_id: Some(sample_id),
    }
}

async fn acoust_id_answers(heart: &Heart, answer: ResponseTemplate) {
    Mock::given(path("/v2/lookup"))
        .respond_with(answer)
        .mount(&heart.acoust_id)
        .await;
}

fn ok(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string(body)
}

async fn lookups(heart: &Heart) -> usize {
    heart
        .acoust_id
        .received_requests()
        .await
        .unwrap_or_default()
        .len()
}

async fn build(heart: &Heart, id: &str, origin: AcquisitionOrigin, heard: Option<HeardRecord>) -> GateBuild {
    heart
        .service
        .build_heart_gate(
            "soulseek",
            id,
            "brandon",
            origin,
            heard,
            &mut ClipPrints::default(),
        )
        .await
}

fn waits(built: &GateBuild) -> Option<(WaitingFor, String)> {
    match built {
        GateBuild::Hold {
            waiting_for, line, ..
        } => Some((*waiting_for, line.clone())),
        GateBuild::Ready(_) | GateBuild::End { .. } => None,
    }
}

fn gate_of(built: GateBuild) -> HeartGate {
    match built {
        GateBuild::Ready(gate) | GateBuild::Hold { gate, .. } | GateBuild::End { gate, .. } => gate,
    }
}

/// While the person gap is backed off by a busy answer, a heart whose heard lookup's turn is not
/// free at once holds for AcoustID at once, waiting nothing out and sending nothing more.
#[tokio::test]
async fn a_heart_behind_a_backed_off_person_gap_holds_at_once() {
    let heart = heart(AcoustIdRateLimiter::PERSON_GAP, None).await;
    acoust_id_answers(&heart, ResponseTemplate::new(503)).await;
    let id = heart.rig.registry.register(help(Some(139)));
    let record = heard_help(&heart, false);
    // Another heart's lookup met a busy AcoustID: the person gap doubled to 4 s.
    let first = build(&heart, &id, AcquisitionOrigin::Heart, Some(record.clone())).await;
    assert_eq!(
        waits(&first),
        Some((WaitingFor::AcoustId, HEARD_LOOKUP_BUSY.to_string()))
    );
    assert!(heart.limiter.backing_off(KeyLane::BuiltIn, LookupClass::Heart));
    assert_eq!(lookups(&heart).await, 1);

    let started = std::time::Instant::now();
    let second = build(&heart, &id, AcquisitionOrigin::Heart, Some(record)).await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "waited {:?}",
        started.elapsed()
    );
    assert_eq!(
        waits(&second),
        Some((WaitingFor::AcoustId, HEARD_LOOKUP_BUSY.to_string()))
    );
    assert_eq!(lookups(&heart).await, 1, "nothing more was sent");
}

/// At the starting gap a heart's heard lookup waits its turn inline, and its answer settles the
/// gate.
#[tokio::test]
async fn at_the_starting_gap_a_hearts_lookup_waits_its_turn() {
    let heart = heart(Duration::from_millis(300), None).await;
    acoust_id_answers(&heart, ok(r#"{"status":"ok","results":[]}"#)).await;
    let id = heart.rig.registry.register(help(Some(139)));
    let record = heard_help(&heart, false);
    let first = build(&heart, &id, AcquisitionOrigin::Heart, Some(record.clone())).await;
    assert_eq!(waits(&first), None);
    let started = std::time::Instant::now();
    let second = build(&heart, &id, AcquisitionOrigin::Heart, Some(record)).await;
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "it waited its turn"
    );
    assert_eq!(gate_of(second).reading, HeardReading::OnlyHeardOrMaster);
    assert_eq!(lookups(&heart).await, 2);
}

/// A family request never holds: with MusicBrainz busy it goes on with the routing's names alone,
/// where a heart holds for MusicBrainz naming the question.
#[tokio::test]
async fn a_family_request_never_holds_for_music_brainz() {
    let music_brainz_server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(503))
        .mount(&music_brainz_server)
        .await;
    let base = Url::parse(&format!("{}/ws/2/", music_brainz_server.uri())).expect("parses");
    let quick = Duration::from_millis(5);
    let music_brainz =
        Arc::new(MusicBrainzClient::with_base_url(base).with_pace(quick, Duration::from_millis(200), quick));
    let heart = heart(Duration::from_millis(10), Some(music_brainz)).await;
    let id = heart.rig.registry.register(SoulseekRouting {
        isrc: Some("GBAYE0601465".into()),
        ..help(Some(139))
    });
    let request = build(&heart, &id, AcquisitionOrigin::Request, None).await;
    let GateBuild::Ready(gate) = request else {
        panic!("a request held: {request:?}");
    };
    let r = gate.r.expect("the routing's names");
    assert!(r.ids.is_empty());
    assert_eq!(r.title, "Help!");

    let held = build(&heart, &id, AcquisitionOrigin::Heart, None).await;
    match held {
        GateBuild::Hold {
            waiting_for: WaitingFor::MusicBrainz,
            question: Some(question),
            ..
        } => assert!(question.contains("GBAYE0601465"), "{question}"),
        other => panic!("a heart did not hold for MusicBrainz: {other:?}"),
    }
}

/// The heard answer names another song by another artist: its video is refused for the song, the
/// heart keeps nothing, and with no catalog R to try it holds "not yet proven" with its line.
#[tokio::test]
async fn a_heard_answer_of_another_song_refuses_the_video_and_holds() {
    let heart = heart(Duration::from_millis(10), None).await;
    acoust_id_answers(
        &heart,
        ok(r#"{"status":"ok","results":[{"id":"fp","score":0.97,"recordings":[
            {"id":"wonderwall","title":"Wonderwall","artists":[{"id":"oasis","name":"Oasis"}],"duration":258,"sources":900},
            {"id":"help","title":"Help!","artists":[{"id":"beatles","name":"The Beatles"}],"duration":139,"sources":20}]}]}"#),
    )
    .await;
    let id = heart.rig.registry.register(help(Some(139)));
    let built = build(
        &heart,
        &id,
        AcquisitionOrigin::Heart,
        Some(heard_help(&heart, false)),
    )
    .await;
    assert_eq!(
        waits(&built),
        Some((WaitingFor::NotYetProven, ANOTHER_SONG_PLAYED.to_string()))
    );
    assert_eq!(gate_of(built).reading, HeardReading::AnotherSong);
    let routing = heart
        .rig
        .registry
        .resolve_song(&id, false)
        .expect("registered")
        .snapshot();
    assert!(
        routing.refuses_video(VIDEO),
        "the song's next play gets another video"
    );
}

/// The stream stood in with another version of the song that AcoustID names alone: R becomes it.
#[tokio::test]
async fn the_recording_that_played_becomes_r() {
    let heart = heart(Duration::from_millis(10), None).await;
    acoust_id_answers(
        &heart,
        ok(r#"{"status":"ok","results":[{"id":"fp","score":0.97,"recordings":[
            {"id":"help-2015","title":"Help!","artists":[{"id":"beatles","name":"The Beatles"}],"duration":139,"sources":40,"isrcs":["GBUM71505900"]}]}]}"#),
    )
    .await;
    let id = heart.rig.registry.register(help(Some(139)));
    let built = build(
        &heart,
        &id,
        AcquisitionOrigin::Heart,
        Some(heard_help(&heart, false)),
    )
    .await;
    let GateBuild::Ready(gate) = built else {
        panic!("held: {built:?}");
    };
    let r = gate.r.expect("R");
    assert_eq!(r.ids, ["help-2015"]);
    assert_eq!(r.isrcs, ["GBUM71505900"]);
    assert_eq!(
        gate.reading,
        HeardReading::RBecame {
            tag: Some("help-2015".into())
        }
    );
}

/// A heard sample AcoustID answers with code 3 cannot be read by its sound: the catalog's clip
/// decides instead, and with no clip the heart holds for AcoustID being off, with Keep anyway's
/// line. The same request is never sent again.
#[tokio::test]
async fn a_heard_sample_answered_with_code_3_runs_the_clip_check() {
    let heart = heart(Duration::from_millis(10), None).await;
    acoust_id_answers(
        &heart,
        ok(r#"{"status":"error","error":{"code":3,"message":"invalid fingerprint"}}"#),
    )
    .await;
    let id = heart.rig.registry.register(help(Some(139)));
    let built = build(
        &heart,
        &id,
        AcquisitionOrigin::Heart,
        Some(heard_help(&heart, false)),
    )
    .await;
    assert_eq!(
        waits(&built),
        Some((
            WaitingFor::AcoustIdOff,
            "AcoustID is off, so Octo cannot check what played".to_string()
        ))
    );
    assert_eq!(gate_of(built).reading, HeardReading::Clip(ClipReading::NoClip));
    assert_eq!(lookups(&heart).await, 1);
}

/// A heard lookup is never sent with a guessed or zero length: with no length known and a stored
/// print that did not reach the song's end, the heard audio itself is printed whole once, and the
/// lookup is sent with that audio's own length (the whole print kept for the catalog's clip).
#[tokio::test]
async fn a_heard_lookup_is_never_sent_with_a_zero_length() {
    let heart = heart(Duration::from_millis(10), None).await;
    acoust_id_answers(&heart, ok(r#"{"status":"ok","results":[]}"#)).await;
    Mock::given(path("/stream"))
        .and(query_param("fresh", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("X-Octo-Fresh", "1")
                .set_body_raw(song_bytes(300.0, 7), "audio/mp4"),
        )
        .mount(&heart._shim)
        .await;
    let id = heart.rig.registry.register(help(None));
    let mut prints = ClipPrints::default();
    let built = heart
        .service
        .build_heart_gate(
            "soulseek",
            &id,
            "brandon",
            AcquisitionOrigin::Heart,
            Some(heard_help(&heart, false)),
            &mut prints,
        )
        .await;
    let sent = heart.acoust_id.received_requests().await.unwrap_or_default();
    assert_eq!(sent.len(), 1);
    let body = String::from_utf8_lossy(&sent[0].body).to_string();
    assert!(body.contains("duration=300"), "{body}");
    assert!(!body.contains("duration=0"), "{body}");
    assert!(
        prints.whole_heard.is_some(),
        "the whole print serves the clip too"
    );
    assert_eq!(gate_of(built).reading, HeardReading::OnlyHeardOrMaster);
}

/// A print that reached the song's end gives the length a lookup sends when the routing has none.
#[tokio::test]
async fn a_print_that_reached_the_end_gives_the_length() {
    let heart = heart(Duration::from_millis(10), None).await;
    acoust_id_answers(&heart, ok(r#"{"status":"ok","results":[]}"#)).await;
    let id = heart.rig.registry.register(help(None));
    build(
        &heart,
        &id,
        AcquisitionOrigin::Heart,
        Some(heard_help(&heart, true)),
    )
    .await;
    let sent = heart.acoust_id.received_requests().await.unwrap_or_default();
    assert_eq!(sent.len(), 1);
    assert!(String::from_utf8_lossy(&sent[0].body).contains("duration=139"));
}

/// A heart never played whose song names no recording and has no video to stand in can never be
/// proven: it ends with the plain line instead of waiting. With a video whose stand-in names
/// nothing it holds "not yet proven" for a play, with its line.
#[tokio::test]
async fn a_heart_never_played_with_nothing_to_prove_ends_and_with_a_video_holds_until_a_play() {
    let heart = heart(Duration::from_millis(10), None).await;
    let id = heart.rig.registry.register(SoulseekRouting {
        you_tube_id: None,
        ..help(Some(139))
    });
    let built = build(&heart, &id, AcquisitionOrigin::Heart, None).await;
    let GateBuild::End { gate, line } = built else {
        panic!("it did not end: {built:?}");
    };
    assert_eq!(
        line,
        crate::services::common::heart_acquisition_coordinator::HEARD_GONE
    );
    assert_eq!(gate.reading, HeardReading::NotHeard);

    let with_video = heart.rig.registry.register(SoulseekRouting {
        title: Some("Ticket to Ride".into()),
        ..help(Some(190))
    });
    let built = build(&heart, &with_video, AcquisitionOrigin::Heart, None).await;
    assert_eq!(
        waits(&built),
        Some((WaitingFor::NotYetProven, NEVER_PLAYED.to_string()))
    );
    assert_eq!(gate_of(built).reading, HeardReading::NotHeard);
}

/// A play is built as its own gate and judged by A1 alone when nothing played; an import's gate
/// carries R and no heard reading.
#[tokio::test]
async fn plays_and_imports_get_their_own_gates() {
    let heart = heart(Duration::from_millis(10), None).await;
    let id = heart.rig.registry.register(help(Some(139)));
    for origin in [AcquisitionOrigin::Play, AcquisitionOrigin::Import] {
        let GateBuild::Ready(gate) = build(&heart, &id, origin, None).await else {
            panic!("{origin:?} held");
        };
        assert_eq!(gate.reading, HeardReading::NotHeard);
        assert_eq!(gate.origin, origin);
        assert_eq!(gate.r.expect("R").title, "Help!");
    }
    assert_eq!(lookups(&heart).await, 0);
}

/// An unreachable AcoustID holds the heart; filed, its held lookup is asked again at the lane's
/// pace, and that resumes the hold.
#[tokio::test]
async fn an_unreachable_acoust_id_holds_and_is_asked_again_at_the_limiters_pace() {
    let heart = heart(Duration::from_millis(10), None).await;
    acoust_id_answers(&heart, ResponseTemplate::new(503)).await;
    let resumed: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = resumed.clone();
    heart
        .service
        .on_resume(Arc::new(move |key: &str| seen.lock().push(key.to_string())));
    let id = heart.rig.registry.register(help(Some(139)));
    let holder = Holder {
        user: "brandon".into(),
        heard: Some(heard_help(&heart, false)),
        ..Default::default()
    };
    let gates: &dyn HeartGates = heart.service.as_ref();
    let start = gates
        .build("soulseek", &id, &holder, AcquisitionOrigin::Heart)
        .await;
    assert!(matches!(
        start,
        GateStart::Hold {
            waiting_for: WaitingFor::AcoustId,
            ..
        }
    ));
    let record = heart.holds.hold(HeldAcquisition {
        kind: HeldKind::Track,
        provider: "soulseek".into(),
        external_id: id.clone(),
        requested_by: Some("brandon".into()),
        held_since_utc: Utc::now(),
        origin: Some(AcquisitionOrigin::Heart),
        waiting_for: Some(WaitingFor::AcoustId),
        holders: vec![holder],
        ..Default::default()
    });
    gates.on_filed(&record, "brandon");
    let key = record.key();
    for _ in 0..200 {
        if resumed.lock().contains(&key) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(*resumed.lock(), [key]);
}

fn answer(name: &str) -> AcoustIdAnswer {
    let text = std::fs::read_to_string(
        PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../octo-core/tests/fixtures/heart_album/gate/answers"
        ))
        .join(name),
    )
    .expect("the frozen answer");
    let root: serde_json::Value = serde_json::from_str(&text).expect("json");
    parse_answer(&root["answer"]).expect("reads").expect("status ok")
}

fn r_of_mix(id: &str, isrc: &str) -> HeardRecording {
    HeardRecording {
        ids: vec![id.into()],
        duplicates: vec![id.into()],
        isrcs: vec![isrc.into()],
        title: "Help!".into(),
        artist: "The Beatles".into(),
        ..Default::default()
    }
}

fn help_song() -> HeartSong {
    HeartSong {
        artist: "The Beatles".into(),
        title: "Help!".into(),
        ..Default::default()
    }
}

/// Contexts that disagree on the ISRC give their overlap with no heard answer, and with the heard
/// 2015 Help! answer too (led by the 2009 recording, it reads clear against neither); contexts
/// that agree give the first.
#[test]
fn contexts_that_disagree_give_their_overlap_unless_the_answer_reads_clear_for_one() {
    let min = 0.85;
    let candidates = || {
        vec![
            r_of_mix(HELP_2009, "GBAYE0601465"),
            r_of_mix(HELP_2015, "GBUM71505900"),
        ]
    };
    let none = settle_candidates(&help_song(), candidates(), None, min);
    assert!(none.ids.is_empty(), "{:?}", none.ids);
    assert_eq!(none.isrcs, ["GBAYE0601465", "GBUM71505900"]);
    let heard_2015 = AcoustIdOutcome::Answered(answer("help-2015-240.json"));
    let overlap = settle_candidates(&help_song(), candidates(), Some(&heard_2015), min);
    assert!(
        overlap.ids.is_empty(),
        "never the 2009 context: {:?}",
        overlap.ids
    );

    let clear_2009 = AcoustIdOutcome::Answered(answer("constructed-clear-2009-120.json"));
    let picked = settle_candidates(&help_song(), candidates(), Some(&clear_2009), min);
    assert_eq!(picked.ids, [HELP_2009]);

    let agreeing = vec![
        r_of_mix(HELP_2009, "GBAYE0601465"),
        r_of_mix("other", "GBAYE0601465"),
    ];
    assert_eq!(
        settle_candidates(&help_song(), agreeing, None, min).ids,
        [HELP_2009]
    );
}

fn context(user: &str, album: &str, listed: i64, played: Option<i64>) -> AlbumContext {
    let at = |seconds: i64| chrono::DateTime::from_timestamp(seconds, 0).expect("a time");
    AlbumContext {
        user: user.into(),
        catalog_album_id: album.into(),
        listed_at: at(listed),
        played_at: played.map(at),
        ..Default::default()
    }
}

/// List A, list B sharing the song, play from A, heart: A. A person's own most recent listed
/// context comes next, and with none of their own every context, each album once.
#[test]
fn the_context_a_heart_reads_is_the_persons_most_recent_played_then_listed() {
    let contexts = vec![
        context("brandon", "A", 100, Some(150)),
        context("brandon", "B", 200, None),
        context("anna", "C", 300, Some(310)),
    ];
    let picked = pick_contexts(&contexts, "Brandon");
    assert_eq!(picked.len(), 1);
    assert_eq!(picked[0].catalog_album_id, "A");
    let listed_only = vec![
        context("brandon", "A", 100, None),
        context("brandon", "B", 200, None),
    ];
    assert_eq!(pick_contexts(&listed_only, "brandon")[0].catalog_album_id, "B");
    let others = pick_contexts(&contexts, "carol");
    let albums: Vec<&str> = others.iter().map(|c| c.catalog_album_id.as_str()).collect();
    assert_eq!(albums, ["A", "B", "C"]);
}

/// A release's kind ranks a context: an album (a soundtrack or a live album too) first, a single
/// or an EP next, then a compilation, then anything else.
#[test]
fn a_contexts_release_ranks_by_its_kind() {
    let group = |primary: &str, secondary: &[&str]| GroupFacts {
        primary_type: primary.into(),
        secondary_types: secondary.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    assert_eq!(kind_of_group(&group("Album", &[])), 0);
    assert_eq!(kind_of_group(&group("Album", &["Soundtrack"])), 0);
    assert_eq!(kind_of_group(&group("Single", &[])), 1);
    assert_eq!(kind_of_group(&group("Album", &["Compilation"])), 2);
    assert_eq!(kind_of_group(&group("Album", &["Remix"])), 3);
}

/// A gate accepting a file as heard writes no recording id and no name from the match; one
/// accepting it by its sound writes the recording it proved.
#[test]
fn a_kept_files_verdict_claims_only_what_the_sound_proved() {
    let gate = HeartGate {
        user: "brandon".into(),
        origin: AcquisitionOrigin::Heart,
        r: None,
        heard: Some(HeardRecord {
            heard_at: Utc::now(),
            source: HeardSource::Video {
                id: VIDEO.into(),
                size: Some(1),
                content_type: "audio/mp4".into(),
                itag: None,
            },
            sample_id: None,
        }),
        heard_seconds: None,
        reading: HeardReading::OnlyHeardOrMaster,
        stand_in: None,
    };
    let shipped = VerificationResult {
        verdict: VerificationVerdict::Confirmed,
        recording_id: Some("2009".into()),
        matched_title: Some("Help!".into()),
        tags_authoritative: true,
        ..Default::default()
    };
    let as_heard = DownloadVerificationService::kept_verification(
        &HeartCheck {
            verdict: GateVerdict::Accept {
                by: octo_core::fingerprint::heart_gate::Acceptance::HeardMaster,
                tags: TagClaim::AsHeard,
            },
            reading: None,
            verification: shipped.clone(),
        },
        &gate,
    );
    assert_eq!(as_heard.recording_id, None);
    assert!(!as_heard.tags_authoritative);
    assert_eq!(
        as_heard.proof,
        RecordingProof::AsHeard {
            video_id: Some(VIDEO.into())
        }
    );
    let mut song = octo_core::models::domain::Song::default();
    as_heard.apply_tags_to(&mut song);
    assert_eq!(song.music_brainz_recording_id, None);

    let proven = DownloadVerificationService::kept_verification(
        &HeartCheck {
            verdict: GateVerdict::Accept {
                by: octo_core::fingerprint::heart_gate::Acceptance::Sound,
                tags: TagClaim::Recording {
                    id: "2015".into(),
                    isrcs: vec!["GBUM71505900".into()],
                },
            },
            reading: None,
            verification: shipped,
        },
        &gate,
    );
    assert_eq!(proven.recording_id.as_deref(), Some("2015"));
    assert_eq!(proven.proof, RecordingProof::Proven);
    proven.apply_tags_to(&mut song);
    assert_eq!(song.music_brainz_recording_id.as_deref(), Some("2015"));
}

/// A busy answer to another call backs a later heart's pace off too: the shared person gap.
#[tokio::test]
async fn a_busy_answer_backs_the_person_pace_off() {
    let heart = heart(Duration::from_millis(10), None).await;
    let turn = heart
        .limiter
        .permit(KeyLane::BuiltIn, LookupClass::Family, true)
        .await
        .expect("a turn");
    turn.end(CallEnd::Busy(BusyKind::Outage));
    assert!(heart.service.client.backing_off(LookupClass::Heart));
    assert!(!heart.service.client.backing_off(LookupClass::Bulk));
}
