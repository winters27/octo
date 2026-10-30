//! A hearted album at Soulseek, through the real download service: each track's file is filed by
//! its own sound against its own recording; another recording is refused and the next peer is
//! tried; a version AcoustID cannot tell apart, or a file AcoustID knows nothing of that fits its
//! track by its own title tag and length, waits in the hold path for the album's chain to file;
//! and a family album request's track keeps the import rule against its track's recording.

use async_trait::async_trait;
use octo_core::fingerprint::heart_gate::{GateVerdict, WaitingFor};

use super::*;
use crate::services::common::album_gates::{AlbumGate, AlbumGateTrack, AlbumGates};

const INTRO: &str = r"C\Drake\01 - Intro.flac";
const OTHER_INTRO: &str = r"D\Drake\01 - Intro.flac";

/// The recording each track of "Nothing Was the Same" is in these tests.
fn recording_of(title: &str) -> String {
    format!("rec-{}", title.to_lowercase().replace([' ', ',', '\''], ""))
}

/// `user`'s gate of "Nothing Was the Same" for `origin`: each track and its own recording.
fn album_gate(g: &Gated, user: &str, origin: AcquisitionOrigin) -> AlbumGate {
    AlbumGate {
        user: user.into(),
        origin,
        tracks: g
            .walk
            .songs
            .iter()
            .map(|song| AlbumGateTrack {
                catalog_track_id: song.external_id.clone().expect("an id"),
                position: song.track,
                title: song.title.clone(),
                artist: song.artist.clone(),
                duration_seconds: song.duration,
                r: HeardRecording {
                    ids: vec![recording_of(&song.title)],
                    duplicates: vec![recording_of(&song.title)],
                    isrcs: vec![format!("USCM5140000{}", song.track.unwrap_or_default())],
                    title: song.title.clone(),
                    artist: "Drake".into(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .collect(),
    }
}

/// AcoustID's clear answer naming `id`, a recording of `title` by Drake.
fn clear(id: &str, title: &str, seconds: i32) -> String {
    json!({"status": "ok", "results": [{"id": "fp", "score": 0.97, "recordings": [{
        "id": id, "title": title, "artists": [{"name": "Drake"}], "duration": seconds, "sources": 40,
    }]}]})
    .to_string()
}

/// A FLAC of `seconds` whose sound is named `print`, with its own title tag and ISRC tag, and
/// enough audio after its tags that its length reads as `seconds`.
fn titled_flac(seconds: u64, print: &str, title: &str, isrc: Option<&str>) -> Vec<u8> {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("titled.flac");
    let mut sound = sound_bytes(print, print, print);
    sound.extend(std::iter::repeat_n(0u8, 64 * 1024));
    sound_flac(&path, seconds, &sound, isrc);
    let mut file = octo_media::tags::TagFile::open(&path).expect("the FLAC opens");
    file.set_title(Some(title));
    file.save().expect("the title is saved");
    std::fs::read(&path).expect("the titled file")
}

/// What a titled peer file reads back as, so a test's file is known to fit its track.
#[test]
fn a_titled_flac_reads_back_its_title_and_length() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("t.flac");
    std::fs::write(&path, titled_flac(90, "p", "Intro", None)).expect("written");
    assert_eq!(
        crate::services::common::album_gates::title_and_length(&path.to_string_lossy()),
        (Some("Intro".to_string()), Some(90))
    );
}

/// A peer offering one file of the track named `title`.
fn track_peer(
    user: &'static str,
    file: &'static str,
    bytes: Vec<u8>,
    title: &'static str,
    seconds: i64,
) -> Peer {
    Peer {
        user,
        file,
        bytes,
        speed: 1_000_000,
        title,
        seconds,
    }
}

/// The album walk of "album-1" from Soulseek, for `users`.
async fn walk_album_for(g: &Gated, users: &[&str]) -> bool {
    tokio::time::timeout(
        Duration::from_secs(60),
        g.walk.service.download_album_with_source(
            "soulseek",
            "album-1",
            DownloadSource::Soulseek,
            false,
            &CancellationToken::new(),
            Some(users.iter().map(|user| user.to_string()).collect()),
        ),
    )
    .await
    .expect("within a minute")
    .expect("walked")
}

/// One track's acquisition, run for `origin` with its album track's gate.
async fn acquire_track(g: &Gated, track: usize, gate: HeartGate) -> anyhow::Result<String> {
    let id = g.walk.songs[track].external_id.clone().expect("an id");
    tokio::time::timeout(
        Duration::from_secs(60),
        g.walk.service.execute_acquisition_to(
            "soulseek",
            &id,
            false,
            true,
            Some(DownloadSource::Soulseek),
            &CancellationToken::new(),
            Some(vec![gate.user.clone()]),
            false,
            None,
            AcquisitionPlace {
                origin: Some(gate.origin),
                gates: vec![gate],
                ..Default::default()
            },
        ),
    )
    .await
    .expect("within a minute")
}

/// An album heart with Lidarr off, offered at Soulseek another recording of one track before the
/// track's own: that file is refused for this heart and the next peer's is kept. A track whose file
/// AcoustID has no entry for, and whose own title and length fit it, is held for the album's chain,
/// never placed.
#[tokio::test]
async fn an_album_hearts_walk_refuses_another_recording_tries_the_next_peer_and_holds_an_unknown_track() {
    let root = Root::new("octo-album-gate-walk-");
    let gates = Arc::new(AlbumGates::new());
    let g = gated_with(
        &root,
        DownloadSource::Soulseek,
        vec![
            peer(
                "othermix",
                FIRST,
                named_flac("started-other", "other-master", "started-other", None),
                2_000_000,
            ),
            peer(
                "rightpeer",
                SECOND,
                named_flac("started", "started-master", "started", None),
                1_000_000,
            ),
            track_peer(
                "intropeer",
                INTRO,
                titled_flac(90, "intro-unknown", "Intro", None),
                "Intro",
                90,
            ),
        ],
        Options {
            key: "key",
            album_gates: Some(gates.clone()),
            ..Default::default()
        },
    )
    .await;
    g.answers.answer("started", &clear("rec-started", "Started", 180));
    g.answers
        .answer("started-other", &clear("rec-started-remix", "Started", 180));
    let gate = album_gate(&g, "brandon", AcquisitionOrigin::AlbumHeart);
    let _hold = gates.register("soulseek", "album-1", gate.clone(), Vec::new());

    let all = walk_album_for(&g, &["brandon"]).await;

    assert!(!all, "the two Hold On tracks found no copy");
    let users = g.batch_users();
    let other_at = users.iter().position(|user| user == "othermix").expect("tried");
    let right_at = users
        .iter()
        .position(|user| user == "rightpeer")
        .expect("then tried");
    assert!(other_at < right_at, "{users:?}");
    let started = g.started();
    let r = gate.track(&started).expect("listed").r.clone();
    assert!(
        g.walk
            .rejected
            .is_denied_for_heart(&started, &r, Some("othermix"), Some(FIRST))
    );
    assert!(!g.walk.rejected.is_denied(Some("othermix"), Some(FIRST)));
    let intro = g.walk.songs[0].external_id.clone().expect("an id");
    let held = gates.take_held("soulseek", "album-1", "brandon");
    assert_eq!(
        held.len(),
        1,
        "{held:?} after {users:?}, asked {:?}, placed {}",
        g.answers.asked(),
        placed_flacs(&root)
    );
    assert_eq!(held[0].0, intro);
    assert_eq!(held[0].1.waiting_for, WaitingFor::NotYetProven);
    assert_eq!(held[0].1.line, "1 track waits: play it once, or keep it anyway");
    assert!(
        held[0].1.files[0].contains(".octo-incoming"),
        "{:?}",
        held[0].1.files
    );
    assert!(std::path::Path::new(&held[0].1.files[0]).is_file());
    assert_eq!(placed_flacs(&root), 1, "only Started is placed");
    assert!(
        gates.settled("soulseek", "album-1", &["brandon".to_string()], &intro),
        "a later walk leaves the held track alone"
    );
}

/// The frozen 2009 Help! answer read against a track whose recording is the 2009 one is a version
/// AcoustID cannot tell apart: the track is held, never refused.
#[tokio::test]
async fn the_frozen_2009_help_answer_holds_its_track_never_refuses_it() {
    let root = Root::new("octo-album-gate-help-");
    let g = gated(
        &root,
        DownloadSource::Soulseek,
        "key",
        vec![peer(
            "mix2009",
            FIRST,
            named_flac("help-2009-120", "help-2009-master", "help-2009-120", None),
            1_000_000,
        )],
    )
    .await;
    let frozen = |name: &str| items_of(&gate_fixture(&format!("prints/{name}.json")));
    g.printer.register("help-2009-120", frozen("help-2009-120"));
    g.answers.answer("help-2009-120", &frozen_body("help-2009-120"));
    let help = HeardRecording {
        ids: vec!["4416d38e-0f68-4c75-93b9-c1a793fccb49".into()],
        duplicates: vec!["4416d38e-0f68-4c75-93b9-c1a793fccb49".into()],
        isrcs: vec!["GBAYE0601465".into()],
        title: "Help! (Remastered 2009)".into(),
        artist: "The Beatles".into(),
        ..Default::default()
    };
    let gate = HeartGate {
        origin: AcquisitionOrigin::AlbumHeart,
        r: Some(help),
        ..started_gate()
    };

    let held = acquire_track(&g, 3, gate).await.expect_err("held, never landed");

    let held = held.downcast_ref::<AcquisitionHeld>().expect("a hold");
    assert_eq!(held.waiting_for, WaitingFor::NotYetProven);
    assert_eq!(held.files.len(), 1);
    assert!(
        std::path::Path::new(&held.files[0]).is_file(),
        "kept in the hold path"
    );
    assert_eq!(g.batch_users(), names(&["mix2009"]), "no other peer was tried");
    assert_eq!(placed_flacs(&root), 0);
}

/// A tagged album whose every file carries its own track's ISRC, each reading clear for its own
/// track: every track lands, and none is refused for its label.
#[tokio::test]
async fn a_tagged_album_whose_files_carry_their_own_isrc_lands_every_clear_track() {
    let root = Root::new("octo-album-gate-tagged-");
    let gates = Arc::new(AlbumGates::new());
    let g = gated_with(
        &root,
        DownloadSource::Soulseek,
        vec![
            track_peer(
                "intropeer",
                INTRO,
                titled_flac(90, "intro", "Intro", Some("USCM51400001")),
                "Intro",
                90,
            ),
            peer(
                "startedpeer",
                FIRST,
                named_flac("started", "started-master", "started", Some("USCM51400004")),
                1_000_000,
            ),
        ],
        Options {
            key: "key",
            album_gates: Some(gates.clone()),
            ..Default::default()
        },
    )
    .await;
    g.answers.answer("intro", &clear("rec-intro", "Intro", 90));
    g.answers.answer("started", &clear("rec-started", "Started", 180));
    let _hold = gates.register(
        "soulseek",
        "album-1",
        album_gate(&g, "brandon", AcquisitionOrigin::AlbumHeart),
        Vec::new(),
    );

    walk_album_for(&g, &["brandon"]).await;

    assert_eq!(placed_flacs(&root), 2, "both tagged tracks land");
    assert!(gates.take_held("soulseek", "album-1", "brandon").is_empty());
    let started = g.started();
    let gate = album_gate(&g, "brandon", AcquisitionOrigin::AlbumHeart);
    let r = gate.track(&started).expect("listed").r.clone();
    assert!(
        !g.walk
            .rejected
            .is_denied_for_heart(&started, &r, Some("startedpeer"), Some(FIRST))
    );
}

/// With AcoustID off nothing reads an album heart's track file: it is unknown, held when its own
/// title and length fit the track, and refused when they fit nothing.
#[tokio::test]
async fn with_acoust_id_off_a_fitting_track_file_is_held_and_one_that_fits_nothing_is_refused() {
    let root = Root::new("octo-album-gate-off-");
    let g = gated(
        &root,
        DownloadSource::Soulseek,
        "off",
        vec![
            track_peer(
                "intropeer",
                INTRO,
                titled_flac(90, "intro", "Intro", None),
                "Intro",
                90,
            ),
            Peer {
                speed: 2_000_000,
                ..track_peer(
                    "elsewhere",
                    OTHER_INTRO,
                    titled_flac(90, "something", "Something Else", None),
                    "Intro",
                    90,
                )
            },
        ],
    )
    .await;
    let gate = album_gate(&g, "brandon", AcquisitionOrigin::AlbumHeart);
    let intro = gate.gate_for(&gate.tracks[0]);

    let held = acquire_track(&g, 0, intro).await.expect_err("held");

    let held = held
        .downcast_ref::<AcquisitionHeld>()
        .unwrap_or_else(|| panic!("a hold: {held:#} after {:?}", g.batch_users()));
    assert_eq!(held.waiting_for, WaitingFor::NotYetProven);
    assert!(g.answers.asked().is_empty(), "AcoustID is off: nothing asked");
    // The peer whose file fits nothing was refused first, and the fitting one held.
    let users = g.batch_users();
    assert_eq!(users.len(), 2, "{users:?}");
    assert_eq!(placed_flacs(&root), 0);
}

/// The catalog's recording of each track of "Nothing Was the Same", as a heart's gate takes it, over
/// the real heart gate for everything else.
struct CatalogGates {
    inner: Arc<DownloadVerificationService>,
    registry: Arc<ExternalIdRegistry>,
}

#[async_trait]
impl crate::services::common::HeartGates for CatalogGates {
    async fn build(
        &self,
        _: &str,
        id: &str,
        holder: &crate::services::common::Holder,
        origin: AcquisitionOrigin,
    ) -> crate::services::common::GateStart {
        let routing = self
            .registry
            .resolve_song(id, false)
            .map(|shared| shared.snapshot())
            .unwrap_or_default();
        let title = routing.title.unwrap_or_default();
        crate::services::common::GateStart::Ready(HeartGate {
            user: holder.user.clone(),
            origin,
            r: Some(HeardRecording {
                ids: vec![recording_of(&title)],
                duplicates: vec![recording_of(&title)],
                title,
                artist: "Drake".into(),
                ..Default::default()
            }),
            heard: None,
            heard_seconds: routing.duration,
            reading: HeardReading::NotHeard,
            stand_in: None,
        })
    }

    async fn judge_held(&self, path: &str, acquisition_key: &str, gates: &[HeartGate]) -> Vec<GateVerdict> {
        self.inner.judge_held(path, acquisition_key, gates).await
    }
}

/// A family album request and a member's album heart, each through the member's album path (one
/// chain per song, each carrying the album's gate), offered another recording of one track: it is
/// refused there and the next peer's file lands, the other track with a copy lands, and the tracks
/// no source had a copy of fail without holding anything.
#[tokio::test]
async fn a_family_album_request_and_a_members_album_heart_refuse_another_recording_and_land_the_rest() {
    use crate::services::common::{
        AcquisitionWorker, FamilyHooks, HeartAcquisitionCoordinator, HeartCoordinatorExtras, HeartGates,
        TrackAcquisitionQueue,
    };
    for origin in [AcquisitionOrigin::Request, AcquisitionOrigin::AlbumHeart] {
        let root = Root::new("octo-album-gate-member-");
        let gates = Arc::new(AlbumGates::new());
        let g = gated_with(
            &root,
            DownloadSource::Soulseek,
            vec![
                peer(
                    "othermix",
                    FIRST,
                    named_flac("started-other", "other-master", "started-other", None),
                    2_000_000,
                ),
                peer(
                    "rightpeer",
                    SECOND,
                    named_flac("started", "started-master", "started", None),
                    1_000_000,
                ),
                track_peer(
                    "intropeer",
                    INTRO,
                    titled_flac(90, "intro", "Intro", None),
                    "Intro",
                    90,
                ),
            ],
            Options {
                key: "key",
                verify: origin == AcquisitionOrigin::Request,
                album_gates: Some(gates.clone()),
                ..Default::default()
            },
        )
        .await;
        g.answers.answer("started", &clear("rec-started", "Started", 180));
        g.answers
            .answer("started-other", &clear("rec-started-remix", "Started", 180));
        g.answers.answer("intro", &clear("rec-intro", "Intro", 90));
        let queue = Arc::new(TrackAcquisitionQueue::new());
        let coordinator = HeartAcquisitionCoordinator::new(
            g.walk.settings.clone(),
            queue.clone(),
            g.walk.service.clone(),
            Arc::new(crate::services::common::test_fakes::FakeLidarr::default()),
            HeartCoordinatorExtras {
                id_registry: Some(g.walk.registry.clone()),
                holds: Some(g.holds.clone()),
                gates: Some(Arc::new(CatalogGates {
                    inner: g.verification.clone(),
                    registry: g.walk.registry.clone(),
                }) as Arc<dyn HeartGates>),
                album_gates: Some(gates.clone()),
                ..Default::default()
            },
        );
        let listing = Arc::new(FakeMetadata::default());
        listing.albums.lock().insert(
            ("soulseek".into(), "album-1".into()),
            Album {
                id: "album-1".into(),
                title: "Nothing Was the Same".into(),
                artist: "Drake".into(),
                songs: g.walk.songs.clone(),
                ..Default::default()
            },
        );
        let world = crate::services::family::test_support::World::new(None);
        coordinator.use_family(FamilyHooks {
            gate: world.gate(),
            pool: world.pool.clone(),
            metadata: listing,
        });
        let stop = CancellationToken::new();
        let worker = Arc::new(AcquisitionWorker::new(
            queue.clone(),
            g.walk.service.clone(),
            g.walk.registry.clone(),
            Arc::new(NotificationService::new(
                Vec::new(),
                g.walk.settings.clone(),
                None,
            )),
            None,
        ));
        tokio::spawn(worker.run(stop.clone()));

        let landed = tokio::time::timeout(
            Duration::from_secs(60),
            coordinator.acquire_album_for_member("soulseek", "album-1", Some("alex"), "alex", false, origin),
        )
        .await
        .expect("within a minute");

        assert_eq!(
            landed.len(),
            2,
            "{origin:?}: Intro and Started land: {landed:?} after {:?}, asked {:?}",
            g.batch_users(),
            g.answers.asked()
        );
        assert_eq!(placed_flacs(&root), 2, "{origin:?}");
        let users = g.batch_users();
        let other_at = users.iter().position(|user| user == "othermix").expect("tried");
        let right_at = users
            .iter()
            .position(|user| user == "rightpeer")
            .expect("then tried");
        assert!(other_at < right_at, "{origin:?}: {users:?}");
        assert!(
            g.holds.snapshot().is_empty(),
            "{origin:?}: a track no source had is not held"
        );
        assert!(
            gates.gate("soulseek", "album-1", "alex").is_none(),
            "{origin:?}: the gate goes with the chain"
        );
        stop.cancel();
    }
}

/// A family album request's track keeps the import rule against its track's own recording: another
/// recording of the track is refused there and the next peer's file lands; nothing is held.
#[tokio::test]
async fn a_family_album_requests_track_refuses_another_recording_and_lands_the_next() {
    let root = Root::new("octo-album-gate-request-");
    let g = gated_with(
        &root,
        DownloadSource::Soulseek,
        vec![
            peer(
                "othermix",
                FIRST,
                named_flac("started-other", "other-master", "started-other", None),
                2_000_000,
            ),
            peer(
                "rightpeer",
                SECOND,
                named_flac("started", "started-master", "started", None),
                1_000_000,
            ),
        ],
        Options {
            key: "key",
            verify: true,
            ..Default::default()
        },
    )
    .await;
    g.answers.answer("started", &clear("rec-started", "Started", 180));
    g.answers
        .answer("started-other", &clear("rec-started-remix", "Started", 180));
    let gate = album_gate(&g, "anna", AcquisitionOrigin::Request);
    let started = gate.gate_for(&gate.tracks[3]);

    let landed = acquire_track(&g, 3, started).await.expect("the track lands");

    assert!(std::path::Path::new(&landed).is_file(), "{landed}");
    assert_eq!(g.batch_users(), names(&["othermix", "rightpeer"]));
    assert!(g.holds.snapshot().is_empty(), "a family request never holds");
    assert_eq!(placed_flacs(&root), 1);
}
