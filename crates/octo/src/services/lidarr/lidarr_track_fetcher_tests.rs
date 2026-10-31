//! One song through Lidarr for a replacement, against the fake Lidarr: Lidarr only for an artist
//! with no music in it and an album that is unmonitored with no queue item, monitoring never
//! turned on, the search followed as a heart's is, the song's own file of Octo's import handed to
//! the replacement, and everything else of Octo's held out of the library and put back with the
//! pick, a late import included.

use octo_core::lidarr::{LidarrReleasePick, LidarrStep, LidarrTrackRequest};
use wiremock::ResponseTemplate;

use super::*;
use crate::services::lidarr::fake_lidarr::GrabFile;
use crate::services::lidarr::lidarr_track_fetcher::{
    ILidarrTrackFetcher, LidarrFetched, LidarrTrackFetcher, MANAGED_TEXT,
};

/// A recording of Teardrop's, as a MusicBrainz id.
const TEARDROP_RECORDING: &str = "4b3f0c1e-7d2a-4a55-9b1e-1d7e5a0c3f11";

fn destination(f: &Fixture) -> String {
    f.root
        .path()
        .join(".octo-incoming/lidarr/job")
        .to_string_lossy()
        .into_owned()
}

/// The fetcher over the fake Lidarr, its pick and follow the service's.
fn fetcher(
    f: &Fixture,
    service: &LidarrHeartAcquisitionService,
    import_timeout_seconds: i32,
) -> LidarrTrackFetcher {
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        lidarr: LidarrSettings {
            base_url: Some(f.server.uri()),
            api_key: Some("k".into()),
            root_folder_path: Some("/data/music".into()),
            quality_profile_id: 1,
            metadata_profile_id: 1,
            import_timeout_seconds,
            ..Default::default()
        },
        subsonic: SubsonicSettings {
            url: Some("http://navidrome.test".into()),
            auto_detect_download_path: false,
            ..Default::default()
        },
        ..Default::default()
    }));
    settings.set_raw("Library:DownloadPath", Some(&f.root.path().to_string_lossy()));
    let identity = NavidromeIdentityService::new(settings.clone(), reqwest::Client::new());
    let fetcher = LidarrTrackFetcher::new(
        Arc::new(LidarrClient::new(settings.clone())),
        settings,
        identity,
        f.claims.clone(),
    );
    fetcher.use_replacements(Arc::new(service.clone()));
    fetcher
}

fn teardrop(lossless_only: bool, original: Option<String>) -> LidarrTrackRequest {
    LidarrTrackRequest {
        artist: "Massive Attack".into(),
        title: "Teardrop".into(),
        album: Some("Mezzanine".into()),
        duration_seconds: Some(330),
        lossless_only,
        original_path: original,
        release: None,
        step: None,
        song: Some("soulseek:teardrop-replacement".into()),
        recordings: Vec::new(),
    }
}

async fn fetch(
    f: &Fixture,
    fetcher: &LidarrTrackFetcher,
    request: LidarrTrackRequest,
) -> Result<LidarrFetched, LidarrError> {
    tokio::time::timeout(
        Duration::from_secs(20),
        fetcher.fetch(&request, &destination(f), &CancellationToken::new()),
    )
    .await
    .expect("the fetch ends")
}

/// How many files lie under a folder of the octo root, at any depth.
fn files_under(f: &Fixture, folder: &str) -> usize {
    fn count(dir: &Path) -> usize {
        std::fs::read_dir(dir).map_or(0, |entries| {
            entries
                .flatten()
                .map(|e| if e.path().is_dir() { count(&e.path()) } else { 1 })
                .sum()
        })
    }
    count(&f.root.path().join(folder))
}

/// The pick is over: its entry forgotten.
async fn put_back(f: &Fixture) {
    until(|| f.pending.get(ALBUM_ID).is_none()).await;
}

fn album_deleted(f: &Fixture) -> bool {
    f.changes()
        .iter()
        .any(|change| change.starts_with("DELETE /api/v1/album/7?deleteFiles=false"))
}

/// MusicBrainz answering one question: the carriers of Teardrop's recording, one release of a
/// Mezzanine group.
async fn carriers_of_teardrop(group: &str, release: &str) -> (Arc<MusicBrainzClient>, MockServer) {
    let server = MockServer::builder().start().await;
    let page = serde_json::json!({
        "release-count": 1,
        "releases": [{
            "id": release, "title": "Mezzanine", "date": "1998-04-20", "country": "GB",
            "release-group": {
                "id": group, "title": "Mezzanine", "primary-type": "Album", "secondary-types": []
            },
            "artist-credit": [{ "name": "Massive Attack", "artist": { "id": ARTIST_ID, "name": "Massive Attack" } }],
        }],
    });
    Mock::given(wiremock::matchers::query_param("recording", TEARDROP_RECORDING))
        .respond_with(ResponseTemplate::new(200).set_body_json(page))
        .mount(&server)
        .await;
    (Arc::new(quick(&server, Duration::from_secs(5))), server)
}

/// The song comes from Octo's own grab: moved out to the replacement with its labels read from
/// the file itself (Lidarr writes no tags), never through the library. Once Lidarr is done, the
/// rest of Octo's import is gone, and the album and the artist Octo's add created are deleted
/// from Lidarr with their files left alone; monitoring was never turned on.
#[tokio::test]
async fn copies_the_song_from_octos_own_grab_then_puts_the_album_back() {
    let f = Fixture::new().await;
    f.grabs_at_once("dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);
    let service = f.service(None, 30);

    let fetched = fetch(&f, &fetcher(&f, &service, 30), teardrop(true, None))
        .await
        .expect("a copy");

    assert!(fetched.path.starts_with(&destination(&f)));
    assert!(fetched.path.ends_with(".flac"));
    assert_eq!(
        std::fs::read_to_string(&fetched.path).expect("the copy"),
        "Teardrop from dl-1"
    );
    assert_eq!(fetched.labels, LabelSource::TheFile);
    put_back(&f).await;
    assert_eq!(f.lidarr.searches(), 1);
    assert!(f.taken().is_empty(), "nothing went into the library");
    assert_eq!(files_under(&f, "Massive Attack"), 0, "Angel is gone too");
    assert!(album_deleted(&f));
    assert!(
        f.changes()
            .iter()
            .any(|change| change.starts_with("DELETE /api/v1/artist/5?deleteFiles=false"))
    );
    assert!(f.lidarr.monitor_changes().is_empty(), "monitoring never touched");
}

/// An artist with a track file in the user's Lidarr: never searched, nothing in Lidarr changes,
/// and the file is untouched; the upgrade goes on to its next source.
#[tokio::test]
async fn an_artist_with_any_file_is_never_searched_and_its_files_untouched() {
    let f = Fixture::new().await;
    f.lidarr.add_album(false);
    let owners = f.lidarr.import(1, "Angel", "FLAC");
    let service = f.service(None, 30);

    let refused = fetch(&f, &fetcher(&f, &service, 30), teardrop(true, None)).await;

    assert_eq!(
        refused,
        Err(LidarrError::InvalidOperation(LIDARR_LEFT_ALONE.into()))
    );
    assert_eq!(f.lidarr.searches(), 0);
    assert!(f.changes().is_empty(), "{:?}", f.changes());
    assert!(Path::new(&owners).is_file());
}

/// A monitored album of an artist with no files is the user's: never searched, and nothing about
/// it changes.
#[tokio::test]
async fn a_monitored_album_of_a_no_file_artist_is_never_searched() {
    let f = Fixture::new().await;
    f.lidarr.add_album(true);
    let service = f.service(None, 30);

    let refused = fetch(&f, &fetcher(&f, &service, 30), teardrop(true, None)).await;

    assert_eq!(
        refused,
        Err(LidarrError::InvalidOperation(ALREADY_LOOKING.into()))
    );
    assert_eq!(f.lidarr.searches(), 0);
    assert!(f.changes().is_empty(), "{:?}", f.changes());
}

/// The user imports Teardrop from a folder while Octo's download runs: that file is never the
/// replacement's copy, Octo steps aside, and the file survives the put-back with its album.
#[tokio::test]
async fn a_users_folder_import_during_a_fetch_is_never_the_candidate_and_survives() {
    let f = Fixture::new().await;
    f.lidarr
        .plan_grab(7, Grab::of("dl-1", &[(3, "Teardrop", "FLAC")]));
    let service = f.service(None, 10);
    let fetching = {
        let fetcher = fetcher(&f, &service, 10);
        let destination = destination(&f);
        tokio::spawn(async move {
            fetcher
                .fetch(&teardrop(true, None), &destination, &CancellationToken::new())
                .await
        })
    };
    until(|| {
        f.pending
            .get(ALBUM_ID)
            .is_some_and(|entry| entry.owns_grab("dl-1"))
    })
    .await;

    f.lidarr.user_folder_import(
        &[GrabFile {
            album_id: 7,
            track_id: 3,
            title: "Teardrop".into(),
            quality: "FLAC".into(),
            rejection: None,
        }],
        None,
    );

    let missed = fetching.await.expect("the task");
    assert!(matches!(missed, Err(LidarrError::FileNotFound(_))), "{missed:?}");
    assert_eq!(files_under(&f, ".octo-incoming/lidarr/job"), 0);
    put_back(&f).await;
    assert_eq!(files_under(&f, "Massive Attack"), 1, "the user's file survives");
    assert!(!album_deleted(&f), "an album holding the user's file stays");
}

/// The caller's import timeout passes while Octo's download still moves: the fetch gives up, the
/// follow goes on, and the late import is still Octo's, held out of the library (never handed to
/// the pipeline) and deleted when the pick is put back.
#[tokio::test]
async fn a_fetch_that_times_out_and_imports_late_is_held_and_put_back() {
    let f = Fixture::new().await;
    f.lidarr.plan_grab(
        7,
        Grab::of("dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]),
    );
    let service = f.service(None, 1);

    let missed = fetch(&f, &fetcher(&f, &service, 1), teardrop(true, None)).await;

    let Err(LidarrError::FileNotFound(message)) = missed else {
        panic!("not found expected, got {missed:?}");
    };
    assert!(message.contains("within"), "{message}");
    assert!(
        f.pending.get(ALBUM_ID).is_some_and(|entry| entry.following()),
        "the follow outlives the wait"
    );

    f.lidarr.complete("dl-1");
    f.lidarr.refresh();

    put_back(&f).await;
    assert!(f.taken().is_empty(), "nothing went into the library");
    assert_eq!(files_under(&f, "Massive Attack"), 0);
    assert_eq!(files_under(&f, ".octo-incoming/lidarr-held"), 0);
    let mut deleted = f.lidarr.deleted();
    deleted.sort_unstable();
    assert_eq!(
        deleted,
        [100, 101],
        "Octo took both out and deleted their records"
    );
    assert!(album_deleted(&f));
}

/// The user's Lidarr holds a Mezzanine unmonitored with two releases, any release welcome: the
/// fetch pins it to the release that carries the original's recording, and once it is over the
/// album gets its release choice back exactly and stays, the user's own.
#[tokio::test]
async fn the_pin_points_at_a_carrier_of_the_original_and_its_release_choice_is_restored() {
    let f = Fixture::new().await;
    let mut mezzanine = CatalogAlbum::new(
        "rg-mezzanine-2",
        "Mezzanine",
        ARTIST_ID,
        9,
        &[(91, "Angel", 1), (93, "Teardrop", 3)],
    );
    mezzanine.releases = vec![(900, "m2-uk".into()), (901, "m2-us".into())];
    f.lidarr.catalog_album(mezzanine);
    f.lidarr.user_album("rg-mezzanine-2", false);
    f.lidarr.plan_grab(
        9,
        Grab::of("dl-1", &[(93, "Teardrop", "FLAC")])
            .into_album(9)
            .finished(),
    );
    let (music_brainz, _server) = carriers_of_teardrop("rg-mezzanine-2", "m2-us").await;
    let service = f.service_with(Options {
        timeout_seconds: 30,
        music_brainz: Some(music_brainz),
        ..Default::default()
    });

    fetch(
        &f,
        &fetcher(&f, &service, 30),
        LidarrTrackRequest {
            album: None,
            recordings: vec![TEARDROP_RECORDING.into()],
            ..teardrop(true, None)
        },
    )
    .await
    .expect("a copy");

    until(|| f.pending.get("rg-mezzanine-2").is_none()).await;
    let puts: Vec<Value> = f
        .lidarr
        .changes()
        .into_iter()
        .filter(|r| r.method == "PUT" && r.path == "/api/v1/album/9")
        .filter_map(|r| r.body)
        .collect();
    let monitored = |body: &Value| -> Vec<i64> {
        body["releases"]
            .as_array()
            .expect("releases")
            .iter()
            .filter(|release| release["monitored"] == true)
            .filter_map(|release| release["id"].as_i64())
            .collect()
    };
    assert_eq!(puts.len(), 2, "the pin, then its restore");
    assert_eq!(monitored(&puts[0]), [901]);
    assert_eq!(puts[0]["anyReleaseOk"], false);
    assert_eq!(monitored(&puts[1]), [900]);
    assert_eq!(puts[1]["anyReleaseOk"], true, "the release choice as it was");
    assert!(
        !f.changes()
            .iter()
            .any(|change| change.starts_with("DELETE /api/v1/album/9")),
        "the user's album stays"
    );
}

/// A Wrong song or a Wrong version through Lidarr names no recording: the original is the
/// recording the person rejected, so the fetch never asks what it sounds like and sends no pin
/// for it. The album its own release group tag names is searched with Lidarr's own choice of
/// release, and its release choice is never touched.
#[tokio::test]
async fn a_fetch_that_names_no_recording_is_never_pinned_to_the_originals() {
    let f = Fixture::new().await;
    let mut mezzanine = CatalogAlbum::new(
        "rg-mezzanine-2",
        "Mezzanine",
        ARTIST_ID,
        9,
        &[(91, "Angel", 1), (93, "Teardrop", 3)],
    );
    mezzanine.releases = vec![(900, "m2-uk".into()), (901, "m2-us".into())];
    f.lidarr.catalog_album(mezzanine);
    f.lidarr.user_album("rg-mezzanine-2", false);
    f.lidarr.plan_grab(
        9,
        Grab::of("dl-1", &[(93, "Teardrop", "FLAC")])
            .into_album(9)
            .finished(),
    );
    let (music_brainz, _server) = carriers_of_teardrop("rg-mezzanine-2", "m2-us").await;
    let service = f.service_with(Options {
        timeout_seconds: 30,
        music_brainz: Some(music_brainz),
        ..Default::default()
    });
    let fetcher = fetcher(&f, &service, 30);
    // The original names its album's release group, and its own sound reads clear as the
    // recording the person rejected.
    let original = f.root.path().join("owned/Teardrop.flac");
    std::fs::create_dir_all(original.parent().expect("a folder")).expect("made");
    std::fs::write(
        &original,
        crate::services::fingerprint::heart_test_support::flac_header(330),
    )
    .expect("written");
    let mut tags = octo_media::tags::TagFile::open(&original).expect("the FLAC opens");
    octo_media::tags::tag_writer_extras::set_text(
        &mut tags,
        octo_media::tags::TagFields::RELEASE_GROUP_ID,
        Some("rg-mezzanine-2"),
    );
    tags.save().expect("the tag is saved");
    let asked = Arc::new(Mutex::new(0));
    let counted = asked.clone();
    fetcher.use_original_sound(Arc::new(
        move |_: String, _: Option<i32>| -> futures::future::BoxFuture<'static, Vec<String>> {
            *counted.lock() += 1;
            Box::pin(async { vec![TEARDROP_RECORDING.to_string()] })
        },
    ));

    fetch(
        &f,
        &fetcher,
        LidarrTrackRequest {
            album: None,
            ..teardrop(false, Some(original.to_string_lossy().into_owned()))
        },
    )
    .await
    .expect("a copy");

    until(|| f.pending.get("rg-mezzanine-2").is_none()).await;
    assert_eq!(*asked.lock(), 0, "the original's sound is never asked");
    let puts: Vec<String> = f
        .lidarr
        .changes()
        .into_iter()
        .filter(|r| r.method == "PUT" && r.path == "/api/v1/album/9")
        .map(|r| r.path)
        .collect();
    assert!(puts.is_empty(), "no pin and nothing to restore: {puts:?}");
    assert_eq!(f.searched_albums(), [9]);
}

/// A Better quality of an original whose tags name no release group and whose album name finds
/// nothing, but whose recording is known: its caller asks what the original is, and the album is
/// the walk's first pick for that recording, a carrier of it.
#[tokio::test]
async fn a_replacement_with_an_identifiable_original_picks_a_carrier_of_it() {
    let f = Fixture::new().await;
    f.grabs_at_once("dl-1", &[(3, "Teardrop", "FLAC")]);
    let (music_brainz, _server) = carriers_of_teardrop(ALBUM_ID, "mezzanine-uk").await;
    let service = f.service_with(Options {
        timeout_seconds: 30,
        music_brainz: Some(music_brainz),
        ..Default::default()
    });
    let fetcher = fetcher(&f, &service, 30);
    // The original's own sound reads clear as Teardrop's recording.
    let original = f.root.path().join("owned/Teardrop.mp3");
    std::fs::create_dir_all(original.parent().expect("a folder")).expect("made");
    std::fs::write(&original, b"the original").expect("written");
    fetcher.use_original_sound(Arc::new(
        |_: String, _: Option<i32>| -> futures::future::BoxFuture<'static, Vec<String>> {
            Box::pin(async { vec![TEARDROP_RECORDING.to_string()] })
        },
    ));

    let mut request = LidarrTrackRequest {
        album: None,
        ..teardrop(true, Some(original.to_string_lossy().into_owned()))
    };
    request.recordings = fetcher.original_recordings(&request).await;
    assert_eq!(request.recordings, [TEARDROP_RECORDING]);
    let fetched = fetch(&f, &fetcher, request).await.expect("a copy");

    assert_eq!(
        std::fs::read_to_string(&fetched.path).expect("the copy"),
        "Teardrop from dl-1"
    );
    assert_eq!(f.searched_albums(), [7]);
    assert!(
        f.lidarr
            .requests()
            .iter()
            .all(|r| !r.path.starts_with("/api/v1/album/lookup") || r.path.contains("lidarr%3A")),
        "never by name"
    );
}

#[tokio::test]
async fn a_lossy_copy_is_not_worth_waiting_for() {
    let f = Fixture::new().await;
    f.grabs_at_once("dl-1", &[(3, "Teardrop", "MP3-320")]);
    let service = f.service(None, 1);

    let missed = fetch(&f, &fetcher(&f, &service, 1), teardrop(true, None)).await;

    let Err(LidarrError::FileNotFound(message)) = missed else {
        panic!("not found expected, got {missed:?}");
    };
    assert!(message.contains("lossless"), "{message}");
    assert_eq!(files_under(&f, ".octo-incoming/lidarr/job"), 0);
    put_back(&f).await;
    assert_eq!(files_under(&f, "Massive Attack"), 0);
}

#[tokio::test]
async fn any_copy_will_do_when_lossless_is_not_asked() {
    let f = Fixture::new().await;
    f.grabs_at_once("dl-1", &[(3, "Teardrop", "MP3-320")]);
    let service = f.service(None, 30);

    let fetched = fetch(&f, &fetcher(&f, &service, 30), teardrop(false, None))
        .await
        .expect("a copy");

    assert!(fetched.path.ends_with(".mp3"));
}

#[tokio::test]
async fn a_lossless_copy_lidarr_already_has_in_the_library_is_a_duplicate() {
    let f = Fixture::new().await;
    f.lidarr.add_album(true);
    f.lidarr.import(3, "Teardrop", "FLAC");
    let service = f.service(None, 30);

    let refused = fetch(&f, &fetcher(&f, &service, 30), teardrop(true, None)).await;

    let Err(LidarrError::InvalidOperation(message)) = refused else {
        panic!("refusal expected, got {refused:?}");
    };
    assert!(message.contains("duplicate"), "{message}");
    assert_eq!(f.lidarr.searches(), 0);
    assert!(f.lidarr.deleted().is_empty());
}

#[tokio::test]
async fn a_song_lidarr_manages_is_left_to_lidarr() {
    let f = Fixture::new().await;
    f.lidarr.add_album(true);
    let managed = f.lidarr.import(3, "Teardrop", "MP3-320");
    let service = f.service(None, 30);

    let refused = fetch(
        &f,
        &fetcher(&f, &service, 30),
        teardrop(true, Some(managed.clone())),
    )
    .await;

    assert_eq!(refused, Err(LidarrError::InvalidOperation(MANAGED_TEXT.into())));
    assert_eq!(f.lidarr.searches(), 0);
    assert!(Path::new(&managed).exists());
}

#[tokio::test]
async fn a_heart_on_the_album_means_not_now() {
    let f = Fixture::new().await;
    f.claims.heart_started(ALBUM_ID);
    let service = f.service(None, 30);

    let refused = fetch(&f, &fetcher(&f, &service, 30), teardrop(true, None)).await;

    let Err(LidarrError::InvalidOperation(message)) = refused else {
        panic!("refusal expected, got {refused:?}");
    };
    assert!(message.contains("heart"), "{message}");
    assert_eq!(f.lidarr.searches(), 0);
    assert!(!f.claims.upgrade_busy(ALBUM_ID));
}

/// Two songs of one album share one search, and each gets its own file.
#[tokio::test]
async fn two_songs_of_one_album_share_one_search() {
    let f = Arc::new(Fixture::new().await);
    f.lidarr.plan_grab(
        7,
        Grab::of("dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]),
    );
    let service = f.service(None, 10);
    let fetcher = fetcher(&f, &service, 10);
    let started = |request: LidarrTrackRequest| {
        let (f, fetcher) = (f.clone(), fetcher.clone());
        tokio::spawn(async move { fetch(&f, &fetcher, request).await })
    };

    let teardrop_task = started(teardrop(true, None));
    until(|| f.lidarr.searches() == 1).await;
    let angel_task = started(LidarrTrackRequest {
        title: "Angel".into(),
        duration_seconds: Some(380),
        song: Some("soulseek:angel-replacement".into()),
        ..teardrop(true, None)
    });
    until(|| service.fetches_waiting(ALBUM_ID) == 2).await;
    f.lidarr.complete("dl-1");
    f.lidarr.refresh();

    let teardrop_copy = teardrop_task.await.expect("the task").expect("a copy");
    let angel_copy = angel_task.await.expect("the task").expect("a copy");
    assert_eq!(f.lidarr.searches(), 1);
    assert_eq!(
        std::fs::read_to_string(teardrop_copy.path).expect("the copy"),
        "Teardrop from dl-1"
    );
    assert_eq!(
        std::fs::read_to_string(angel_copy.path).expect("the copy"),
        "Angel from dl-1"
    );
    put_back(&f).await;
    assert!(!f.claims.upgrade_busy(ALBUM_ID));
}

#[tokio::test]
async fn an_album_lidarr_does_not_know_is_not_found() {
    let f = Fixture::new().await;
    f.lidarr.set_knows_album(false);
    let service = f.service(None, 30);

    let missed = fetch(&f, &fetcher(&f, &service, 30), teardrop(true, None)).await;

    assert!(matches!(missed, Err(LidarrError::FileNotFound(_))), "{missed:?}");
    assert_eq!(f.lidarr.searches(), 0);
}

// ---- Picked releases --------------------------------------------------------------------------

const RELEASE: &str = "Massive Attack - Mezzanine (1998) [FLAC]";

fn pick(guid: &str) -> LidarrReleasePick {
    LidarrReleasePick {
        guid: guid.into(),
        indexer_id: 3,
        title: Some(RELEASE.into()),
        size: Some(400_000_000),
    }
}

/// `Step = (text, _) => steps.Add(text)`.
fn recording_steps() -> (LidarrStep, Arc<Mutex<Vec<String>>>) {
    let steps = Arc::new(Mutex::new(Vec::new()));
    let sink = steps.clone();
    (
        LidarrStep::new(move |text, _| sink.lock().push(text.to_string())),
        steps,
    )
}

/// A release picked in Find songs is grabbed instead of Lidarr's own search, found again by its
/// title, indexer and size when Lidarr forgot it, and its grab is Octo's by its guid.
#[tokio::test]
async fn a_pick_lidarr_has_forgotten_is_found_again_and_its_grab_is_octos() {
    let f = Fixture::new().await;
    f.lidarr.offer("fresh-guid", 3, RELEASE, 400_000_000);
    f.lidarr.offer(
        "other-guid",
        3,
        "Massive Attack - Mezzanine (1998) [MP3]",
        90_000_000,
    );
    f.grabs_at_once("dl-1", &[(3, "Teardrop", "FLAC")]);
    let service = f.service(None, 30);
    let (step, steps) = recording_steps();

    let fetched = fetch(
        &f,
        &fetcher(&f, &service, 30),
        LidarrTrackRequest {
            release: Some(pick("stale-guid")),
            step: Some(step),
            ..teardrop(true, None)
        },
    )
    .await
    .expect("a copy");

    assert_eq!(
        std::fs::read_to_string(&fetched.path).expect("the copy"),
        "Teardrop from dl-1"
    );
    assert_eq!(f.lidarr.grabs(), vec!["fresh-guid".to_string()]);
    assert!(f.searched_albums().is_empty(), "no AlbumSearch");
    assert_eq!(*steps.lock(), vec!["Grabbing the release you picked".to_string()]);
}

#[tokio::test]
async fn a_pick_no_longer_offered_is_refused_in_plain_words() {
    let f = Fixture::new().await;
    f.lidarr.offer(
        "other-guid",
        3,
        "Massive Attack - Mezzanine (1998) [MP3]",
        90_000_000,
    );
    let service = f.service(None, 30);

    let refused = fetch(
        &f,
        &fetcher(&f, &service, 30),
        LidarrTrackRequest {
            release: Some(pick("stale-guid")),
            ..teardrop(true, None)
        },
    )
    .await;

    assert_eq!(
        refused,
        Err(LidarrError::InvalidOperation(
            octo_core::lidarr::lidarr_client::PICK_GONE_TEXT.into()
        ))
    );
    assert!(f.lidarr.grabs().is_empty());
    put_back(&f).await;
    assert!(album_deleted(&f), "the album Octo's add created is put back");
}

#[tokio::test]
async fn a_pick_for_an_album_already_being_fetched_says_it_was_not_grabbed() {
    let f = Arc::new(Fixture::new().await);
    f.lidarr.offer("guid", 3, RELEASE, 400_000_000);
    f.lidarr.plan_grab(
        7,
        Grab::of("dl-1", &[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]),
    );
    let service = f.service(None, 10);
    let fetcher = fetcher(&f, &service, 10);
    let (step, steps) = recording_steps();

    let teardrop_task = {
        let (f, fetcher) = (f.clone(), fetcher.clone());
        tokio::spawn(async move { fetch(&f, &fetcher, teardrop(true, None)).await })
    };
    until(|| f.lidarr.searches() == 1).await;
    let angel_task = {
        let (f, fetcher) = (f.clone(), fetcher.clone());
        tokio::spawn(async move {
            let angel = LidarrTrackRequest {
                title: "Angel".into(),
                duration_seconds: Some(380),
                release: Some(pick("guid")),
                step: Some(step),
                song: Some("soulseek:angel-replacement".into()),
                ..teardrop(true, None)
            };
            fetch(&f, &fetcher, angel).await
        })
    };
    until(|| steps.lock().len() == 1).await;
    until(|| service.fetches_waiting(ALBUM_ID) == 2).await;
    f.lidarr.complete("dl-1");
    f.lidarr.refresh();
    teardrop_task.await.expect("the task").expect("a copy");
    angel_task.await.expect("the task").expect("a copy");

    assert_eq!(
        *steps.lock(),
        vec!["Lidarr is already fetching this album, so the release you picked was not grabbed".to_string()]
    );
    assert!(f.lidarr.grabs().is_empty());
}

#[tokio::test]
async fn nothing_is_fetched_until_lidarr_is_set_up() {
    let f = Fixture::new().await;
    let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
    let fetcher = LidarrTrackFetcher::new(
        Arc::new(LidarrClient::new(settings.clone())),
        settings.clone(),
        NavidromeIdentityService::new(settings, reqwest::Client::new()),
        f.claims.clone(),
    );

    let refused = fetch(&f, &fetcher, teardrop(true, None)).await;

    assert!(
        matches!(refused, Err(LidarrError::InvalidOperation(_))),
        "{refused:?}"
    );
    assert!(f.server.received_requests().await.expect("recording").is_empty());
}

/// A caller that stops waiting lets go of the album at once; the follow goes on, and the pick is
/// put back once Lidarr is done with Octo's download.
#[tokio::test]
async fn a_canceled_fetch_lets_go_and_the_pick_is_still_put_back() {
    let f = Fixture::new().await;
    f.lidarr.plan_grab(7, Grab::of("dl-1", &[(1, "Angel", "FLAC")]));
    let service = f.service(None, 10);
    let fetcher = fetcher(&f, &service, 10);
    let ct = CancellationToken::new();
    let cancel = ct.clone();
    let fetching = {
        let destination = destination(&f);
        tokio::spawn(async move { fetcher.fetch(&teardrop(true, None), &destination, &ct).await })
    };
    until(|| f.lidarr.searches() == 1).await;
    cancel.cancel();

    let result = fetching.await.expect("the task");

    assert!(matches!(result, Err(LidarrError::Canceled(_))), "{result:?}");
    assert!(!f.claims.upgrade_busy(ALBUM_ID));
    f.lidarr.complete("dl-1");
    f.lidarr.refresh();
    put_back(&f).await;
    assert_eq!(f.lidarr.deleted(), [100]);
    assert!(album_deleted(&f));
}
