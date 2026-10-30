//! One song through Lidarr for a replacement. Lidarr fetches whole albums, so Octo borrows the
//! album: it copies out the song it asked for, deletes every file that search brought in, and
//! puts the album's monitoring back. It never touches a file Lidarr had before, a song Lidarr
//! manages is left to Lidarr, and a heart working on the album means not now.

use std::sync::Arc;
use std::time::Duration;

use octo_core::settings::{AppSettings, LidarrSettings, SettingsStore, SubsonicSettings};
use tempfile::TempDir;
use wiremock::MockServer;

use super::*;
use crate::services::common::test_fakes::until;
use crate::services::lidarr::fake_lidarr::{ALBUM_ID, FakeLidarr};
use octo_core::lidarr::LidarrStep;

struct Fixture {
    root: TempDir,
    lidarr: FakeLidarr,
    server: MockServer,
    claims: Arc<LidarrAlbumClaims>,
}

impl Fixture {
    async fn new() -> Fixture {
        let root = tempfile::tempdir().expect("a temp dir");
        let (lidarr, server) = FakeLidarr::start(root.path()).await;
        Fixture {
            root,
            lidarr,
            server,
            claims: Arc::new(LidarrAlbumClaims::new()),
        }
    }

    fn root(&self) -> String {
        self.root.path().to_string_lossy().into_owned()
    }

    fn destination(&self) -> String {
        self.root
            .path()
            .join(".octo-incoming/lidarr/job")
            .to_string_lossy()
            .into_owned()
    }

    fn fetcher(&self, import_timeout_seconds: i32) -> LidarrTrackFetcher {
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            lidarr: LidarrSettings {
                base_url: Some(self.server.uri()),
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
        settings.set_raw("Library:DownloadPath", Some(&self.root()));
        let identity = NavidromeIdentityService::new(settings.clone(), reqwest::Client::new());
        let fetcher = LidarrTrackFetcher::new(
            Arc::new(LidarrClient::new(settings.clone())),
            settings,
            identity,
            self.claims.clone(),
        );
        fetcher.set_poll(Duration::from_millis(10));
        fetcher
    }

    async fn fetch(
        &self,
        fetcher: &LidarrTrackFetcher,
        request: LidarrTrackRequest,
    ) -> Result<String, LidarrError> {
        tokio::time::timeout(
            Duration::from_secs(20),
            fetcher.fetch(&request, &self.destination(), &CancellationToken::new()),
        )
        .await
        .expect("the fetch ends")
    }

    fn files_under(&self, folder: &str) -> usize {
        fn count(dir: &Path) -> usize {
            std::fs::read_dir(dir).map_or(0, |entries| {
                entries
                    .flatten()
                    .map(|e| if e.path().is_dir() { count(&e.path()) } else { 1 })
                    .sum()
            })
        }
        count(&self.root.path().join(folder))
    }
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
    }
}

#[tokio::test]
async fn copies_the_song_then_takes_back_what_the_search_brought() {
    let f = Fixture::new().await;
    f.lidarr
        .on_search(&[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);

    let copy = f
        .fetch(&f.fetcher(10), teardrop(true, None))
        .await
        .expect("a copy");

    assert!(copy.starts_with(&f.destination()));
    assert!(copy.ends_with(".flac"));
    assert_eq!(std::fs::read_to_string(&copy).expect("the copy"), "Teardrop");
    assert_eq!(f.lidarr.searches(), 1);
    // Both files the search brought in are gone, through Lidarr, so its records stay true.
    assert_eq!(f.lidarr.deleted().len(), 2);
    assert_eq!(f.files_under("Massive Attack"), 0);
    // Octo added the album, so it stops monitoring it again.
    assert!(f.lidarr.monitor_changes().contains(&false));
}

#[tokio::test]
async fn a_lossy_copy_is_not_worth_waiting_for() {
    let f = Fixture::new().await;
    f.lidarr.on_search(&[(3, "Teardrop", "MP3-320")]);

    // Short: running out of time is what this is about.
    let missed = f.fetch(&f.fetcher(1), teardrop(true, None)).await;

    let Err(LidarrError::FileNotFound(message)) = missed else {
        panic!("not found expected, got {missed:?}");
    };
    assert!(message.contains("lossless"), "{message}");
    assert_eq!(f.files_under(".octo-incoming/lidarr/job"), 0);
    assert_eq!(f.lidarr.deleted().len(), 1);
}

#[tokio::test]
async fn any_copy_will_do_when_lossless_is_not_asked() {
    let f = Fixture::new().await;
    f.lidarr.on_search(&[(3, "Teardrop", "MP3-320")]);

    let copy = f
        .fetch(&f.fetcher(10), teardrop(false, None))
        .await
        .expect("a copy");

    assert!(copy.ends_with(".mp3"));
}

#[tokio::test]
async fn a_lossless_copy_lidarr_already_has_in_the_library_is_a_duplicate() {
    let f = Fixture::new().await;
    f.lidarr.add_album(true);
    f.lidarr.import(3, "Teardrop", "FLAC");

    let refused = f.fetch(&f.fetcher(10), teardrop(true, None)).await;

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

    let refused = f
        .fetch(&f.fetcher(10), teardrop(true, Some(managed.clone())))
        .await;

    assert_eq!(refused, Err(LidarrError::InvalidOperation(MANAGED_TEXT.into())));
    assert_eq!(f.lidarr.searches(), 0);
    assert!(Path::new(&managed).exists());
}

#[tokio::test]
async fn a_file_lidarr_had_before_is_never_deleted() {
    let f = Fixture::new().await;
    f.lidarr.add_album(true);
    let owners = f.lidarr.import(1, "Angel", "FLAC");
    f.lidarr.on_search(&[(3, "Teardrop", "FLAC")]);

    f.fetch(&f.fetcher(10), teardrop(true, None))
        .await
        .expect("a copy");

    assert!(Path::new(&owners).exists());
    assert_eq!(f.lidarr.deleted().len(), 1);
    // It was monitored before, so it stays monitored.
    assert!(!f.lidarr.monitor_changes().contains(&false));
}

#[tokio::test]
async fn a_heart_on_the_album_means_not_now() {
    let f = Fixture::new().await;
    f.claims.heart_started(ALBUM_ID);

    let refused = f.fetch(&f.fetcher(10), teardrop(true, None)).await;

    let Err(LidarrError::InvalidOperation(message)) = refused else {
        panic!("refusal expected, got {refused:?}");
    };
    assert!(message.contains("heart"), "{message}");
    assert_eq!(f.lidarr.searches(), 0);
    assert!(!f.claims.upgrade_busy(ALBUM_ID));
}

#[tokio::test]
async fn two_songs_of_one_album_share_one_search() {
    let f = Arc::new(Fixture::new().await);
    let fetcher = f.fetcher(10);

    let teardrop_task = {
        let (f, fetcher) = (f.clone(), fetcher.clone());
        tokio::spawn(async move { f.fetch(&fetcher, teardrop(true, None)).await })
    };
    let angel_task = {
        let (f, fetcher) = (f.clone(), fetcher.clone());
        tokio::spawn(async move {
            let angel = LidarrTrackRequest {
                title: "Angel".into(),
                duration_seconds: Some(380),
                ..teardrop(true, None)
            };
            f.fetch(&fetcher, angel).await
        })
    };
    until(|| f.lidarr.searches() == 1).await;
    // Both have joined the album's search before Lidarr brings anything.
    until(|| fetcher.users_of(ALBUM_ID) == 2).await;
    // The release: Lidarr imports both songs.
    f.lidarr.import(1, "Angel", "FLAC");
    f.lidarr.import(3, "Teardrop", "FLAC");
    let teardrop_copy = teardrop_task.await.expect("the task").expect("a copy");
    let angel_copy = angel_task.await.expect("the task").expect("a copy");

    assert_eq!(f.lidarr.searches(), 1);
    assert_eq!(
        std::fs::read_to_string(teardrop_copy).expect("the copy"),
        "Teardrop"
    );
    assert_eq!(std::fs::read_to_string(angel_copy).expect("the copy"), "Angel");
    assert_eq!(f.lidarr.deleted().len(), 2);
    assert!(!f.claims.upgrade_busy(ALBUM_ID));
}

#[tokio::test]
async fn an_album_lidarr_does_not_know_is_not_found() {
    let f = Fixture::new().await;
    f.lidarr.set_knows_album(false);

    let missed = f.fetch(&f.fetcher(10), teardrop(true, None)).await;

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
fn recording_steps() -> (LidarrStep, Arc<parking_lot::Mutex<Vec<String>>>) {
    let steps = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = steps.clone();
    (
        LidarrStep::new(move |text, _| sink.lock().push(text.to_string())),
        steps,
    )
}

#[tokio::test]
async fn a_pick_lidarr_has_forgotten_is_found_again_by_its_title_indexer_and_size() {
    let f = Fixture::new().await;
    f.lidarr.offer("fresh-guid", 3, RELEASE, 400_000_000);
    f.lidarr.offer(
        "other-guid",
        3,
        "Massive Attack - Mezzanine (1998) [MP3]",
        90_000_000,
    );
    f.lidarr.on_search(&[(3, "Teardrop", "FLAC")]);
    let (step, steps) = recording_steps();

    let copy = f
        .fetch(
            &f.fetcher(10),
            LidarrTrackRequest {
                release: Some(pick("stale-guid")),
                step: Some(step),
                ..teardrop(true, None)
            },
        )
        .await
        .expect("a copy");

    assert_eq!(std::fs::read_to_string(&copy).expect("the copy"), "Teardrop");
    assert_eq!(f.lidarr.grabs(), vec!["fresh-guid".to_string()]);
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

    let refused = f
        .fetch(
            &f.fetcher(10),
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
}

#[tokio::test]
async fn a_pick_for_an_album_already_being_fetched_says_it_was_not_grabbed() {
    let f = Arc::new(Fixture::new().await);
    f.lidarr.offer("guid", 3, RELEASE, 400_000_000);
    f.lidarr
        .on_search(&[(1, "Angel", "FLAC"), (3, "Teardrop", "FLAC")]);
    f.lidarr.hold_imports();
    let fetcher = f.fetcher(10);
    let (step, steps) = recording_steps();

    let teardrop_task = {
        let (f, fetcher) = (f.clone(), fetcher.clone());
        tokio::spawn(async move { f.fetch(&fetcher, teardrop(true, None)).await })
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
                ..teardrop(true, None)
            };
            f.fetch(&fetcher, angel).await
        })
    };
    until(|| steps.lock().len() == 1).await;
    f.lidarr.release_imports();
    teardrop_task.await.expect("the task").expect("a copy");
    angel_task.await.expect("the task").expect("a copy");

    assert_eq!(
        *steps.lock(),
        vec!["Lidarr is already fetching this album, so the release you picked was not grabbed".to_string()]
    );
    assert!(f.lidarr.grabs().is_empty());
}

// That the song is matched by title, never by number, is tested with match_track, in
// octo_core::lidarr::lidarr_track_fetcher.

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

    let refused = f.fetch(&fetcher, teardrop(true, None)).await;

    assert!(
        matches!(refused, Err(LidarrError::InvalidOperation(_))),
        "{refused:?}"
    );
    assert!(f.server.received_requests().await.expect("recording").is_empty());
}

#[tokio::test]
async fn a_canceled_fetch_still_cleans_up() {
    let f = Fixture::new().await;
    f.lidarr.on_search(&[(1, "Angel", "FLAC")]);
    let fetcher = f.fetcher(10);
    let ct = CancellationToken::new();
    let cancel = ct.clone();
    let fetching = {
        let fetcher = fetcher.clone();
        let destination = f.destination();
        tokio::spawn(async move { fetcher.fetch(&teardrop(true, None), &destination, &ct).await })
    };
    until(|| f.lidarr.searches() == 1).await;
    cancel.cancel();

    let result = fetching.await.expect("the task");

    assert!(matches!(result, Err(LidarrError::Canceled(_))), "{result:?}");
    assert_eq!(f.lidarr.deleted().len(), 1);
    assert!(!f.claims.upgrade_busy(ALBUM_ID));
}
