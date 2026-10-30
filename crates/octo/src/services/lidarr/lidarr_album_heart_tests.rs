//! Album hearts through Lidarr, against the fake Lidarr: the album a heart names is found by its
//! catalog album's barcode (the catalog album found from its title first when the heart has no
//! catalog id) and pinned to that barcode release, Lidarr is used only for an artist with no music
//! in it, and each file of Octo's import is filed among the album's tracks by its own sound
//! against each track's own recording, never by where Lidarr put it.

use std::collections::{HashMap, HashSet};

use octo_core::fingerprint::VerificationResult;
use octo_core::fingerprint::heart_gate::{Acceptance, HeardRecording, Refusal, TagClaim, WaitingFor};

use super::*;
use crate::services::common::album_gates::{AlbumGate, AlbumGateTrack, TrackFiling};
use crate::services::common::soulseek_hold_store::held_root;
use crate::services::common::{GateStart, Holder};

/// The Beatles, as MusicBrainz and Lidarr know them.
const BEATLES: &str = "b10bbbfc-cf9e-42e0-be17-e2c3e1d2600d";
/// The UK Help! release group, which the barcode of "Help! (Remastered 2009)" names.
const HELP_GROUP: &str = "0d44e1cb-c6e0-3453-8b68-4d2082f05421";
/// The barcode release of "Help! (Remastered 2009)".
const HELP_RELEASE: &str = "94acd741-5f84-4dd7-9355-b885996af0f1";
/// Deezer's "Help! (Remastered 2009)".
const HELP_CATALOG: &str = "12047944";

/// Files a file of an album's import by its "sound": the title its contents start with (the fake
/// Lidarr writes "<title> from <download>" into each downloaded file), mapped to a recording.
/// It lands at the first unfilled track whose own recording holds that recording, waits "not yet
/// proven" at the first unfilled track for a held title, and is refused otherwise. Every file and
/// the recordings of the gate's tracks it was filed against are recorded.
#[derive(Default)]
struct SoundFiler {
    sounds: HashMap<String, String>,
    holding: HashSet<String>,
    seen: Mutex<Vec<(String, Vec<Vec<String>>)>>,
}

#[async_trait]
impl HeartGates for SoundFiler {
    async fn build(&self, _: &str, _: &str, _: &Holder, _: AcquisitionOrigin) -> GateStart {
        unreachable!("an album heart's import is only filed")
    }

    async fn file_album_file(
        &self,
        path: &std::path::Path,
        _: LabelSource,
        gate: &AlbumGate,
        unfilled: &[usize],
        _: &str,
    ) -> (TrackFiling, Option<VerificationResult>) {
        let contents = std::fs::read_to_string(path).unwrap_or_default();
        let title = contents.split(" from ").next().unwrap_or_default().to_string();
        self.seen.lock().push((
            title.clone(),
            gate.tracks.iter().map(|track| track.r.ids.clone()).collect(),
        ));
        if self.holding.contains(&title)
            && let Some(first) = unfilled.first()
        {
            return (
                TrackFiling::Hold {
                    track: *first,
                    waiting_for: WaitingFor::NotYetProven,
                },
                None,
            );
        }
        let landing = self.sounds.get(&title).and_then(|recording| {
            unfilled
                .iter()
                .find(|index| gate.tracks[**index].r.holds(recording))
                .map(|index| (*index, recording.clone()))
        });
        match landing {
            Some((track, id)) => (
                TrackFiling::Land {
                    track,
                    by: Acceptance::Sound,
                    tags: TagClaim::Recording {
                        id,
                        isrcs: Vec::new(),
                    },
                },
                Some(VerificationResult::default()),
            ),
            None => (TrackFiling::Refuse(Refusal::AnotherRecording), None),
        }
    }
}

/// Mezzanine's album gate for alice: Angel and Teardrop, each with the recording its catalog ISRC
/// names, never the one Lidarr matches its tracks to (`rec-1`, `rec-3`).
fn mezzanine_gate(f: &Fixture) -> AlbumGate {
    let track = |title: &str, recording: &str, isrc: &str| AlbumGateTrack {
        catalog_track_id: f.id(title),
        title: title.into(),
        artist: "Massive Attack".into(),
        duration_seconds: Some(330),
        r: HeardRecording {
            ids: vec![recording.into()],
            isrcs: vec![isrc.into()],
            title: title.into(),
            artist: "Massive Attack".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    AlbumGate {
        user: "alice".into(),
        origin: AcquisitionOrigin::AlbumHeart,
        tracks: vec![
            track("Angel", "rec-angel", "GBAAA9800001"),
            track("Teardrop", "rec-teardrop", "GBAAA9800003"),
        ],
    }
}

/// alice's heart of Mezzanine.
async fn heart_mezzanine(service: &LidarrHeartAcquisitionService) -> bool {
    within(service.try_acquire_album(
        "soulseek",
        "mezzanine",
        false,
        Some("alice"),
        AcquisitionOrigin::AlbumHeart,
        &[],
    ))
    .await
}

/// The service with this filer and album gates, Mezzanine's gate registered for alice.
fn filing_service(
    f: &Fixture,
    filer: Arc<SoundFiler>,
) -> (LidarrHeartAcquisitionService, Arc<AlbumGates>, AlbumGateHold) {
    let store = Arc::new(AlbumGates::new());
    let hold = store.register("soulseek", "mezzanine", mezzanine_gate(f), Vec::new());
    let service = f.service_with(Options {
        timeout_seconds: 30,
        album_gates: Some(store.clone()),
        gates: Some(filer as Arc<dyn HeartGates>),
        ..Default::default()
    });
    (service, store, hold)
}

/// The Beatles' Help! as the catalog lists it, under `id`: the album routing with or without its
/// Deezer id, and its fourteen tracks, Yesterday (with its ISRC) at 13.
fn help_album(f: &Fixture, catalog_id: Option<&str>) -> String {
    let id = f.ids.register(SoulseekRouting {
        kind: RoutingKind::Album,
        artist: Some("The Beatles".into()),
        title: Some("Help! (Remastered 2009)".into()),
        album: Some("Help! (Remastered 2009)".into()),
        external_album_id: catalog_id.map(str::to_string),
        ..Default::default()
    });
    let songs = (1..=14)
        .map(|n| Song {
            title: if n == 13 {
                "Yesterday (Remastered 2009)".into()
            } else {
                format!("Track {n}")
            },
            artist: "The Beatles".into(),
            album: "Help! (Remastered 2009)".into(),
            track: Some(n),
            disc_number: Some(1),
            duration: Some(if n == 13 { 123 } else { 150 }),
            isrc: (n == 13).then(|| "GBAYE0601477".to_string()),
            external_provider: Some("soulseek".into()),
            external_id: Some(format!("help-{n}")),
            ..Default::default()
        })
        .collect();
    f.metadata.albums.lock().insert(
        ("soulseek".into(), id.clone()),
        Album {
            title: "Help! (Remastered 2009)".into(),
            artist: "The Beatles".into(),
            songs,
            ..Default::default()
        },
    );
    id
}

/// Lidarr's metadata knows the UK Help! (its first release the barcode release) and Yesterday and
/// Today, a repackage a lookup by name can land on.
fn beatles_albums(f: &Fixture) {
    f.lidarr.catalog_artist(BEATLES, "The Beatles", 80);
    let mut yesterday_and_today = CatalogAlbum::new(
        "yesterday-and-today",
        "Yesterday and Today",
        BEATLES,
        82,
        &[(820, "Yesterday", 1)],
    );
    yesterday_and_today.release_date = "1966-06-20".into();
    f.lidarr.catalog_album(yesterday_and_today);
    let mut help = CatalogAlbum::new(HELP_GROUP, "Help!", BEATLES, 81, &[(813, "Yesterday", 13)]);
    help.releases = vec![(810, "help-us".into()), (811, HELP_RELEASE.into())];
    f.lidarr.catalog_album(help);
}

/// The service asking MusicBrainz the Help! row's recorded answers and the recorded Deezer
/// catalog.
async fn help_service(f: &Fixture) -> (LidarrHeartAcquisitionService, MockServer) {
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(Recorded(recorded_folder("rows/beatles-yesterday_a0")))
        .mount(&server)
        .await;
    let service = f.service_with(Options {
        timeout_seconds: 1,
        music_brainz: Some(Arc::new(quick(&server, Duration::from_secs(5)))),
        catalog: Some(RecordedCatalog::open() as Arc<dyn CatalogQuestions>),
        ..Default::default()
    });
    (service, server)
}

/// Every file under a folder, at any depth.
fn walkdir_files(folder: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walkdir_files(&path));
        } else {
            files.push(path);
        }
    }
    files
}

/// Whether Lidarr was ever asked for an album by its name rather than its release group's id.
fn looked_up_by_name(f: &Fixture) -> bool {
    f.lidarr
        .requests()
        .iter()
        .any(|r| r.path.starts_with("/api/v1/album/lookup?term=") && !r.path.contains("lidarr%3A"))
}

/// Help! hearted from its catalog album: the barcode names the UK Help! group, which Lidarr is asked
/// for by its id, never Yesterday and Today and never by name; the search is pinned to the barcode
/// release.
#[tokio::test]
async fn help_hearted_from_its_catalog_album_resolves_the_uk_help_group() {
    let f = Fixture::new().await;
    beatles_albums(&f);
    let id = help_album(&f, Some(HELP_CATALOG));
    let (service, _server) = help_service(&f).await;

    let landed = within(service.try_acquire_album(
        "soulseek",
        &id,
        false,
        Some("alice"),
        AcquisitionOrigin::AlbumHeart,
        &[],
    ))
    .await;

    assert!(!landed, "nothing was grabbed");
    assert_eq!(f.searched_albums(), [81], "the UK Help!");
    assert!(!looked_up_by_name(&f));
    let pin = f
        .lidarr
        .changes()
        .into_iter()
        .find(|r| r.method == "PUT" && r.path == "/api/v1/album/81")
        .and_then(|r| r.body)
        .expect("pinned");
    assert_eq!(pin["anyReleaseOk"], false);
    let monitored: Vec<i64> = pin["releases"]
        .as_array()
        .expect("releases")
        .iter()
        .filter(|release| release["monitored"] == true)
        .filter_map(|release| release["id"].as_i64())
        .collect();
    assert_eq!(monitored, [811], "the barcode release alone");
}

/// With no catalog id, the catalog album is found from the album's title first (its Yesterday at
/// 13 carrying its ISRC), and its barcode names the same group.
#[tokio::test]
async fn with_no_catalog_id_the_catalog_album_is_found_from_its_title_first() {
    let f = Fixture::new().await;
    beatles_albums(&f);
    let id = help_album(&f, None);
    let (service, _server) = help_service(&f).await;

    within(service.try_acquire_album(
        "soulseek",
        &id,
        false,
        Some("alice"),
        AcquisitionOrigin::AlbumHeart,
        &[],
    ))
    .await;

    assert_eq!(f.searched_albums(), [81]);
    assert!(!looked_up_by_name(&f));
}

/// An album heart of an artist with music in the user's Lidarr leaves Lidarr alone: nothing is
/// searched and nothing in Lidarr changes.
#[tokio::test]
async fn an_album_heart_of_an_artist_with_music_in_lidarr_searches_nothing() {
    let f = Fixture::new().await;
    f.lidarr.add_album(false);
    f.lidarr.import(1, "Angel", "FLAC");
    let changes_before = f.lidarr.changes().len();
    let service = f.service(None, 1);

    assert!(!heart_mezzanine(&service).await);

    assert_eq!(f.lidarr.searches(), 0);
    assert_eq!(f.lidarr.changes().len(), changes_before);
}

/// Found by name (no catalog album to go by): each file of Octo's import is filed by its own sound
/// against the tracks' own recordings, which their catalog ISRCs gave, never Lidarr's match. The
/// Teardrop Lidarr filed at Angel's place lands as Teardrop; a file that is neither track is
/// deleted, whatever Lidarr matched it to.
#[tokio::test]
async fn by_name_each_file_is_filed_by_its_sound_against_the_catalog_tracks() {
    let f = Fixture::new().await;
    f.grabs_at_once("dl-1", &[(1, "Teardrop", "FLAC"), (3, "Angel live", "FLAC")]);
    let filer = Arc::new(SoundFiler {
        sounds: HashMap::from([("Teardrop".to_string(), "rec-teardrop".to_string())]),
        ..Default::default()
    });
    let (service, _store, _hold) = filing_service(&f, filer.clone());

    assert!(!heart_mezzanine(&service).await, "Angel did not land");

    until(|| filer.seen.lock().len() == 2).await;
    f.job_wound_up().await;
    assert_eq!(f.taken(), ["Teardrop"]);
    for (_, recordings) in filer.seen.lock().iter() {
        assert_eq!(
            *recordings,
            [vec!["rec-angel".to_string()], vec!["rec-teardrop".to_string()]],
            "the catalog's recordings, never Lidarr's"
        );
    }
    let left: Vec<_> = walkdir_files(&f.root.path().join("Massive Attack"));
    assert!(
        left.is_empty(),
        "the file that is neither track is deleted: {left:?}"
    );
    let mut deleted = f.lidarr.deleted();
    deleted.sort_unstable();
    assert_eq!(deleted, [100, 101], "Lidarr keeps no record of either");
    assert!(
        f.asked.lock().iter().all(|asked| asked.title == "Teardrop"),
        "the pipeline took only what the sound filed"
    );
}

/// No track proven: nothing lands, every file waits in the one hold path as a held album track of
/// alice's heart (handed to whoever files them), Lidarr's records of the moved files are deleted,
/// and the pick is put back.
#[tokio::test]
async fn with_no_track_proven_nothing_lands_lidarr_is_put_back_and_the_tracks_wait() {
    let f = Fixture::new().await;
    f.grabs_at_once("dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);
    let filer = Arc::new(SoundFiler {
        holding: HashSet::from(["Angel".to_string(), "Teardrop".to_string()]),
        ..Default::default()
    });
    let (service, store, _hold) = filing_service(&f, filer.clone());
    let told: Arc<Mutex<Vec<(String, String, String)>>> = Arc::default();
    {
        let told = told.clone();
        service.on_album_holds(Arc::new(
            move |provider: &str, album: &str, user: &str, origin: AcquisitionOrigin| {
                assert_eq!(origin, AcquisitionOrigin::AlbumHeart);
                told.lock()
                    .push((provider.to_string(), album.to_string(), user.to_string()));
            },
        ));
    }

    assert!(!heart_mezzanine(&service).await);

    until(|| f.pending.get(ALBUM_ID).is_none() && !f.claims.heart_busy(ALBUM_ID)).await;
    assert!(f.taken().is_empty(), "nothing lands");
    let held = store.take_held("soulseek", "mezzanine", "alice");
    let mut held_ids: Vec<String> = held.iter().map(|(id, _)| id.clone()).collect();
    held_ids.sort();
    let mut wanted = vec![f.id("Angel"), f.id("Teardrop")];
    wanted.sort();
    assert_eq!(held_ids, wanted);
    let root = held_root(f.root.path());
    for (_, waits) in &held {
        assert_eq!(waits.waiting_for, WaitingFor::NotYetProven);
        assert_eq!(waits.users, ["alice"]);
        let file = std::path::Path::new(&waits.files[0]);
        assert!(file.is_file(), "it waits in the one hold path");
        assert!(file.starts_with(&root));
    }
    assert_eq!(
        *told.lock(),
        [
            (
                "soulseek".to_string(),
                "mezzanine".to_string(),
                "alice".to_string()
            ),
            (
                "soulseek".to_string(),
                "mezzanine".to_string(),
                "alice".to_string()
            ),
        ]
    );
    let mut deleted = f.lidarr.deleted();
    deleted.sort_unstable();
    assert_eq!(deleted, [100, 101], "the records of the moved files");
    assert!(
        f.changes()
            .iter()
            .any(|change| change.starts_with("DELETE /api/v1/album/7?deleteFiles=false")),
        "the album Octo's add created is put back: {:?}",
        f.changes()
    );
}
