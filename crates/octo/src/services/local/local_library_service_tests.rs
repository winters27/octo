//! Tests of the local library service and the `.mappings.json` contract. A wiremock server
//! stands in for Subsonic, answering every call with the scan status.

use std::sync::Arc;

use octo_core::models::domain::Song;
use octo_core::settings::{AppSettings, SettingsStore, SubsonicSettings};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::services::soulseek::ExternalIdRegistry;
use crate::services::subsonic::NavidromeIdentityService;

const SCAN_STATUS: &str =
    r#"{"subsonic-response":{"status":"ok","scanStatus":{"scanning":false,"count":100}}}"#;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/rust-migration/fixtures/state/music/.mappings.json"
);

struct Fixture {
    dir: tempfile::TempDir,
    server: MockServer,
    service: LocalLibraryService,
}

impl Fixture {
    async fn new() -> Fixture {
        Self::answering(SCAN_STATUS).await
    }

    async fn answering(body: &str) -> Fixture {
        let server = MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("temp dir");
        let service = service_for(&server.uri(), dir.path());
        Fixture { dir, server, service }
    }

    fn path(&self, name: &str) -> String {
        self.dir.path().join(name).to_string_lossy().into_owned()
    }

    fn mappings_file(&self) -> std::path::PathBuf {
        self.dir.path().join(".mappings.json")
    }
}

fn service_for(url: &str, download_path: &std::path::Path) -> LocalLibraryService {
    let store = SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(url.to_string()),
            ..Default::default()
        },
        ..Default::default()
    });
    store.set_raw("Library:DownloadPath", Some(&download_path.to_string_lossy()));
    let settings = Arc::new(store);
    let http = crate::services::http_client_factory::default_client();
    LocalLibraryService::new(
        settings.clone(),
        http.clone(),
        Arc::new(ExternalIdRegistry::in_memory()),
        NavidromeIdentityService::new(settings, http),
    )
}

fn song(id: &str, title: &str, provider: Option<&str>, external_id: Option<&str>) -> Song {
    Song {
        id: id.into(),
        title: title.into(),
        artist: "Test Artist".into(),
        album: "Test Album".into(),
        external_provider: provider.map(str::to_string),
        external_id: external_id.map(str::to_string),
        ..Default::default()
    }
}

#[tokio::test]
async fn parse_song_id_with_external_id_returns_correct_parts() {
    let f = Fixture::new().await;
    let (is_external, provider, external_id) = f.service.parse_song_id("ext-deezer-123456");
    assert!(is_external);
    assert_eq!(provider.as_deref(), Some("deezer"));
    assert_eq!(external_id.as_deref(), Some("123456"));
}

#[tokio::test]
async fn parse_song_id_with_local_id_returns_not_external() {
    let f = Fixture::new().await;
    assert_eq!(f.service.parse_song_id("local-789"), (false, None, None));
}

#[tokio::test]
async fn parse_song_id_with_numeric_id_returns_not_external() {
    let f = Fixture::new().await;
    assert_eq!(f.service.parse_song_id("12345"), (false, None, None));
}

#[tokio::test]
async fn get_local_path_for_external_song_when_not_registered_returns_null() {
    let f = Fixture::new().await;
    assert_eq!(
        f.service
            .get_local_path_for_external_song("deezer", "nonexistent")
            .await,
        None
    );
}

#[tokio::test]
async fn register_downloaded_song_then_get_local_path_returns_path() {
    let f = Fixture::new().await;
    let local_path = f.path("test-song.mp3");
    std::fs::write(&local_path, "fake audio content").expect("write");

    f.service
        .register_downloaded_song(
            &song("ext-deezer-123456", "Test Song", Some("deezer"), Some("123456")),
            &local_path,
        )
        .await
        .expect("registered");

    assert_eq!(
        f.service
            .get_local_path_for_external_song("deezer", "123456")
            .await,
        Some(local_path)
    );
}

#[tokio::test]
async fn get_local_path_for_external_song_when_file_deleted_returns_null() {
    let f = Fixture::new().await;
    let local_path = f.path("deleted-song.mp3");
    std::fs::write(&local_path, "fake audio content").expect("write");
    f.service
        .register_downloaded_song(
            &song(
                "ext-deezer-999999",
                "Deleted Song",
                Some("deezer"),
                Some("999999"),
            ),
            &local_path,
        )
        .await
        .expect("registered");
    std::fs::remove_file(&local_path).expect("delete");

    assert_eq!(
        f.service
            .get_local_path_for_external_song("deezer", "999999")
            .await,
        None
    );
}

#[tokio::test]
async fn register_downloaded_song_with_null_provider_does_nothing() {
    let f = Fixture::new().await;
    f.service
        .register_downloaded_song(
            &song("local-123", "Local Song", None, None),
            &f.path("local-song.mp3"),
        )
        .await
        .expect("does not fail");
    assert!(!f.mappings_file().exists());
}

#[tokio::test]
async fn trigger_library_scan_returns_true() {
    let f = Fixture::new().await;
    assert!(f.service.trigger_library_scan(false).await);
}

#[tokio::test]
async fn get_scan_status_returns_scan_status() {
    let f = Fixture::new().await;
    let status = f.service.get_scan_status().await.expect("a status");
    assert!(!status.scanning);
    assert_eq!(status.count, Some(100));
}

#[tokio::test]
async fn parse_song_id_various_inputs_returns_expected() {
    let f = Fixture::new().await;
    let cases: [(&str, bool, Option<&str>, Option<&str>); 8] = [
        ("ext-deezer-123", true, Some("deezer"), Some("123")),
        ("ext-spotify-abc123", true, Some("spotify"), Some("abc123")),
        ("ext-tidal-999-888", true, Some("tidal"), Some("999-888")),
        // New format - extracts numeric ID
        ("ext-deezer-song-123456", true, Some("deezer"), Some("123456")),
        ("123456", false, None, None),
        ("", false, None, None),
        ("ext-", false, None, None),
        ("ext-deezer", false, None, None),
    ];
    for (id, external, provider, external_id) in cases {
        let (is_external, p, e) = f.service.parse_song_id(id);
        assert_eq!(
            (is_external, p.as_deref(), e.as_deref()),
            (external, provider, external_id),
            "{id:?}"
        );
    }
}

#[tokio::test]
async fn parse_external_id_various_inputs_returns_expected() {
    let f = Fixture::new().await;
    type Case<'a> = (&'a str, bool, Option<&'a str>, Option<&'a str>, Option<&'a str>);
    let cases: [Case; 10] = [
        (
            "ext-deezer-song-123456",
            true,
            Some("deezer"),
            Some("song"),
            Some("123456"),
        ),
        (
            "ext-deezer-album-789012",
            true,
            Some("deezer"),
            Some("album"),
            Some("789012"),
        ),
        (
            "ext-deezer-artist-259",
            true,
            Some("deezer"),
            Some("artist"),
            Some("259"),
        ),
        (
            "ext-spotify-song-abc123",
            true,
            Some("spotify"),
            Some("song"),
            Some("abc123"),
        ),
        // Legacy format defaults to song
        ("ext-deezer-123", true, Some("deezer"), Some("song"), Some("123")),
        ("ext-tidal-999", true, Some("tidal"), Some("song"), Some("999")),
        ("123456", false, None, None, None),
        ("", false, None, None, None),
        ("ext-", false, None, None, None),
        ("ext-deezer", false, None, None, None),
    ];
    for (id, external, provider, kind, external_id) in cases {
        let (is_external, p, t, e) = f.service.parse_external_id(id);
        assert_eq!(
            (is_external, p.as_deref(), t.as_deref(), e.as_deref()),
            (external, provider, kind, external_id),
            "{id:?}"
        );
    }
}

// ---- One song id, several versions ------------------------------------------------------------

fn version_of(recording: &str) -> Song {
    Song {
        music_brainz_recording_id: Some(recording.into()),
        ..song("ext-soulseek-help", "Help!", Some("soulseek"), Some("help"))
    }
}

fn kept_for(user: &str) -> Option<KeptHeart> {
    Some(KeptHeart {
        users: vec![user.into()],
        ..Default::default()
    })
}

/// Two people hearted one song id and heard two mixes: each kept file is its own mapping, and
/// each person's lookup finds their own mix.
#[tokio::test]
async fn two_people_with_two_mixes_of_one_song_id_each_find_their_own() {
    let f = Fixture::new().await;
    let (mix_2009, mix_2015) = (f.path("Help 2009.flac"), f.path("Help 2015.flac"));
    std::fs::write(&mix_2009, "2009").expect("write");
    std::fs::write(&mix_2015, "2015").expect("write");
    f.service
        .register_downloaded_song_as(&version_of("rec-2009"), &mix_2009, kept_for("ana"))
        .await
        .expect("registered");
    f.service
        .register_downloaded_song_as(&version_of("rec-2015"), &mix_2015, kept_for("bo"))
        .await
        .expect("registered");

    let mine = |user: &'static str| {
        f.service
            .get_local_path_for_external_song_as(Some(user), "soulseek", "help")
    };
    assert_eq!(mine("ana").await, Some(mix_2009.clone()));
    assert_eq!(mine("bo").await, Some(mix_2015.clone()));
    // Someone who kept neither gets no other person's mix; with nobody named, the newest.
    assert_eq!(mine("cy").await, None);
    assert_eq!(
        f.service
            .get_local_path_for_external_song("soulseek", "help")
            .await,
        Some(mix_2015)
    );
    let keys: Vec<String> = f.service.read(|map| map.keys().cloned().collect()).expect("read");
    assert_eq!(
        keys,
        [
            "soulseek:help#recording:rec-2009",
            "soulseek:help#recording:rec-2015"
        ]
    );
}

/// A version kept for nobody in particular (an import, a download before hearts kept versions)
/// is anyone's: a person with no version of their own finds it, and their own wins when there is
/// one.
#[tokio::test]
async fn a_version_kept_for_nobody_is_anyones_and_a_persons_own_comes_first() {
    let f = Fixture::new().await;
    let (plain, theirs) = (f.path("Help.mp3"), f.path("Help heard.m4a"));
    std::fs::write(&plain, "plain").expect("write");
    std::fs::write(&theirs, "theirs").expect("write");
    f.service
        .register_downloaded_song(&version_of("rec-2009"), &plain)
        .await
        .expect("registered");
    f.service
        .register_downloaded_song_as(&version_of("rec-2015"), &theirs, kept_for("ana"))
        .await
        .expect("registered");

    let mine = |user: &'static str| {
        f.service
            .get_local_path_for_external_song_as(Some(user), "soulseek", "help")
    };
    assert_eq!(mine("ana").await, Some(theirs));
    assert_eq!(mine("bo").await, Some(plain));
}

/// The same version downloaded again replaces its own mapping, whatever key it was saved under,
/// and a file that took another mapping's place on disk takes its mapping too: no song keeps two
/// mappings of one file or one version.
#[tokio::test]
async fn the_same_version_again_replaces_its_mapping_and_never_doubles_it() {
    let f = Fixture::new().await;
    let (first, again) = (f.path("Help.mp3"), f.path("Help.flac"));
    std::fs::write(&first, "mp3").expect("write");
    std::fs::write(&again, "flac").expect("write");
    // Saved as today's code saves it: the plain key, with its recording.
    {
        let mut cached = f.service.mappings.lock();
        let map = f.service.loaded(&mut cached).expect("loaded");
        map.set(
            "soulseek:help".to_string(),
            LocalSongMapping {
                external_provider: "soulseek".into(),
                external_id: "help".into(),
                local_path: first.clone(),
                music_brainz_recording_id: Some("rec-2009".into()),
                ..Default::default()
            },
        );
    }
    f.service
        .register_downloaded_song(&version_of("REC-2009"), &again)
        .await
        .expect("registered");

    let mappings = f.service.get_mappings().await;
    assert_eq!(mappings.len(), 1, "{mappings:?}");
    assert_eq!(mappings[0].local_path, again);

    // An unproven copy placed over that very file takes its place, mapping and all.
    f.service
        .register_downloaded_song(&song("x", "Help!", Some("soulseek"), Some("help")), &again)
        .await
        .expect("registered");
    let mappings = f.service.get_mappings().await;
    assert_eq!(mappings.len(), 1, "{mappings:?}");
    assert_eq!(mappings[0].music_brainz_recording_id, None);
}

/// Today's mappings file loads as it is, every mapping under its own key: a second version of one
/// of its songs is added beside it, and the file's own entries are written back unchanged.
#[tokio::test]
async fn todays_mappings_file_loads_unchanged_beside_a_second_version() {
    let text = std::fs::read_to_string(FIXTURE).expect("the fixture is in the repo");
    let f = Fixture::new().await;
    std::fs::write(f.mappings_file(), &text).expect("copy the fixture");
    let other = f.path("Svefn-g-englar (live).flac");
    std::fs::write(&other, "live").expect("write");

    f.service
        .register_downloaded_song_as(
            &Song {
                music_brainz_recording_id: Some("live-take".into()),
                ..song(
                    "ext-soulseek-7YzXwVu6TsRq5PoN4mLk3J",
                    "Svefn-g-englar",
                    Some("soulseek"),
                    Some("7YzXwVu6TsRq5PoN4mLk3J"),
                )
            },
            &other,
            kept_for("ana"),
        )
        .await
        .expect("registered");

    let before: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&text).expect("the fixture parses");
    let after: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(f.mappings_file()).expect("written")).expect("parses");
    for (key, entry) in &before {
        assert_eq!(after.get(key), Some(entry), "{key} is written as it was");
    }
    assert!(after.contains_key("soulseek:7YzXwVu6TsRq5PoN4mLk3J#recording:live-take"));
    assert_eq!(after.len(), before.len() + 1);
}

// ---- The .mappings.json contract and the rest of the interface -----------------------------

/// The fixture is read and written back byte for byte, in its own key order.
#[tokio::test]
async fn the_mappings_fixture_round_trips_byte_for_byte() {
    let text = std::fs::read_to_string(FIXTURE).expect("the fixture is in the repo");
    let f = Fixture::new().await;
    std::fs::write(f.mappings_file(), &text).expect("copy the fixture");

    let mappings = f.service.get_mappings().await;
    assert_eq!(mappings.len(), 2);
    assert_eq!(mappings[0].artist, "Sigur Rós");
    assert_eq!(mappings[1].source_peer, None);

    // A forget of a path nobody has writes nothing; registering again writes the same file.
    assert!(
        f.service
            .forget_mapping("/nowhere.flac")
            .await
            .expect("no write")
            .is_empty()
    );
    {
        let mut cached = f.service.mappings.lock();
        let map = f.service.loaded(&mut cached).expect("loaded");
        f.service.save(map).expect("saved");
    }
    assert_eq!(
        std::fs::read_to_string(f.mappings_file()).expect("written"),
        text.trim_end_matches('\n')
    );
}

/// A forgotten path's slot is taken by the next download, as `Dictionary` reused it, and the
/// path matches ignoring case.
#[tokio::test]
async fn forget_mapping_drops_the_path_ignoring_case() {
    let f = Fixture::new().await;
    for (id, name) in [("1", "one.flac"), ("2", "two.flac"), ("3", "three.flac")] {
        f.service
            .register_downloaded_song(&song(id, name, Some("soulseek"), Some(id)), &f.path(name))
            .await
            .expect("registered");
    }

    let forgotten = f
        .service
        .forget_mapping(&f.path("TWO.FLAC"))
        .await
        .expect("written");
    assert_eq!(
        forgotten
            .iter()
            .map(|m| m.external_id.as_str())
            .collect::<Vec<_>>(),
        ["2"]
    );
    assert!(f.service.forget_mapping("  ").await.expect("nothing").is_empty());
    f.service
        .register_downloaded_song(
            &song("4", "four", Some("soulseek"), Some("4")),
            &f.path("four.flac"),
        )
        .await
        .expect("registered");

    let written = std::fs::read_to_string(f.mappings_file()).expect("written");
    let keys: Vec<&str> = written
        .lines()
        .filter(|line| line.starts_with("  \""))
        .map(|line| line.trim().split('"').nth(1).unwrap_or_default())
        .collect();
    assert_eq!(keys, ["soulseek:1", "soulseek:4", "soulseek:3"]);
}

/// One match by tags, however they are written; none when two files match, or when the file
/// is gone; the album only narrows when both sides have one.
#[tokio::test]
async fn find_mapping_by_tags_answers_only_an_unambiguous_file() {
    let f = Fixture::new().await;
    let first = f.path("Too Good.flac");
    std::fs::write(&first, [1]).expect("write");
    let mut too_good = song("1", "Too Good (feat. Rihanna)", Some("soulseek"), Some("1"));
    too_good.artist = "Drake".into();
    too_good.album = "Views".into();
    f.service
        .register_downloaded_song(&too_good, &first)
        .await
        .expect("registered");

    let found = f
        .service
        .find_mapping_by_tags(Some("Drake feat. Rihanna"), Some("Too Good"), None)
        .await
        .expect("found");
    assert_eq!(found.local_path, first);
    assert!(
        f.service
            .find_mapping_by_tags(Some("Drake"), Some("Too Good (feat. Rihanna)"), Some("Views"))
            .await
            .is_some()
    );
    assert!(
        f.service
            .find_mapping_by_tags(Some("Drake"), Some("Too Good (feat. Rihanna)"), Some("Scorpion"))
            .await
            .is_none()
    );
    assert!(
        f.service
            .find_mapping_by_tags(None, Some("Too Good"), None)
            .await
            .is_none()
    );

    // A second copy under another id makes it ambiguous.
    let second = f.path("Too Good (2).flac");
    std::fs::write(&second, [1]).expect("write");
    f.service
        .register_downloaded_song(
            &song("2", "Too Good (feat. Rihanna)", Some("youtube"), Some("2")),
            &second,
        )
        .await
        .expect("registered");
    let mut by_drake = song("2", "Too Good (feat. Rihanna)", Some("youtube"), Some("2"));
    by_drake.artist = "Drake".into();
    f.service
        .register_downloaded_song(&by_drake, &second)
        .await
        .expect("registered");
    assert!(
        f.service
            .find_mapping_by_tags(Some("Drake"), Some("Too Good"), None)
            .await
            .is_none()
    );
    std::fs::remove_file(&second).expect("delete");
    assert!(
        f.service
            .find_mapping_by_tags(Some("Drake"), Some("Too Good"), None)
            .await
            .is_some()
    );
}

/// A file that does not parse is moved aside, and the service starts empty rather than failing
/// every caller.
#[tokio::test]
async fn a_corrupt_mappings_file_is_moved_aside() {
    let f = Fixture::new().await;
    std::fs::write(f.mappings_file(), "{ not json").expect("write");

    assert!(f.service.get_mappings().await.is_empty());
    assert!(!f.mappings_file().exists());
    let aside: Vec<String> = std::fs::read_dir(f.dir.path())
        .expect("list")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".mappings.json.corrupt-"))
        .collect();
    assert_eq!(aside.len(), 1, "{aside:?}");
}

/// A JSON `null` file is no mappings, as `?? new Dictionary` made it.
#[tokio::test]
async fn a_null_mappings_file_is_empty() {
    let f = Fixture::new().await;
    std::fs::write(f.mappings_file(), "null").expect("write");
    assert!(f.service.get_mappings().await.is_empty());
    assert!(f.mappings_file().exists());
}

/// The second scan within 30 s is debounced (and reported as triggered); a forced one is not.
#[tokio::test]
async fn a_scan_soon_after_another_is_debounced_unless_forced() {
    let f = Fixture::new().await;
    assert!(f.service.trigger_library_scan(false).await);
    assert!(f.service.trigger_library_scan(false).await);
    assert_eq!(
        crate::services::test_support::received(&f.server)
            .await
            .iter()
            .filter(|r| r.url.path() == "/rest/startScan")
            .count(),
        1
    );
    assert!(f.service.trigger_library_scan(true).await);
    assert_eq!(
        crate::services::test_support::received(&f.server)
            .await
            .iter()
            .filter(|r| r.url.path() == "/rest/startScan")
            .count(),
        2
    );
}

/// A refused scan is false; a status of the wrong shape is none.
#[tokio::test]
async fn a_refused_scan_and_an_odd_status() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("temp dir");
    let service = service_for(&server.uri(), dir.path());
    assert!(!service.trigger_library_scan(true).await);
    assert!(service.get_scan_status().await.is_none());

    let odd = Fixture::answering(r#"{"subsonic-response":{"scanStatus":{"scanning":"no"}}}"#).await;
    assert!(odd.service.get_scan_status().await.is_none());
    let bare = Fixture::answering(r#"{"subsonic-response":{"status":"ok"}}"#).await;
    assert!(bare.service.get_scan_status().await.is_none());
}

/// Navidrome answers getScanStatus only with a sign-in, so it carries the admin identity
/// startScan uses; without one it is the bare call.
#[tokio::test]
async fn get_scan_status_signs_in_as_the_admin_identity() {
    let f = Fixture::new().await;
    let store = SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(f.server.uri()),
            ..Default::default()
        },
        ..Default::default()
    });
    store.set_raw("Library:DownloadPath", Some(&f.dir.path().to_string_lossy()));
    let settings = Arc::new(store);
    let http = crate::services::http_client_factory::default_client();
    let identity = NavidromeIdentityService::new(settings.clone(), http.clone());
    identity.capture_login(
        br#"{"token":"jwt-1","isAdmin":true,"username":"the admin","subsonicToken":"tok","subsonicSalt":"salt"}"#,
    );
    let service = LocalLibraryService::new(
        settings,
        http,
        Arc::new(ExternalIdRegistry::in_memory()),
        identity,
    );
    assert_eq!(
        service.get_scan_status().await.expect("a status").count,
        Some(100)
    );
    assert!(f.service.get_scan_status().await.is_some());

    let asked: Vec<String> = crate::services::test_support::received(&f.server)
        .await
        .iter()
        .filter(|r| r.url.path() == "/rest/getScanStatus")
        .map(|r| r.url.query().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        asked,
        ["f=json&c=octo&v=1.16.1&u=the%20admin&t=tok&s=salt", "f=json"]
    );
}

/// `TryGetInt32`: a count past an Int32 or with a fraction is none, the scan still read; a
/// count that is not a number at all fails the read.
#[tokio::test]
async fn a_scan_count_that_is_not_an_int32_is_none() {
    for body in [
        r#"{"subsonic-response":{"scanStatus":{"scanning":true,"count":3000000000}}}"#,
        r#"{"subsonic-response":{"scanStatus":{"scanning":true,"count":1.5}}}"#,
    ] {
        let f = Fixture::answering(body).await;
        let status = f.service.get_scan_status().await.expect("a status");
        assert!(status.scanning, "{body}");
        assert_eq!(status.count, None, "{body}");
    }
    let f = Fixture::answering(r#"{"subsonic-response":{"scanStatus":{"scanning":true,"count":"5"}}}"#).await;
    assert!(f.service.get_scan_status().await.is_none());
}

/// A ping names the server even when Navidrome refused the sign-in; nothing answering is none.
#[tokio::test]
async fn get_server_info_reads_the_ping_envelope() {
    let f = Fixture::answering(
        r#"{"subsonic-response":{"status":"failed","version":"1.16.1","type":"navidrome","serverVersion":"0.64.2 (abc)","error":{"code":40}}}"#,
    )
    .await;
    let info = f.service.get_server_info().await.expect("an answer");
    assert!(info.is_navidrome());
    assert_eq!(info.version(), Some((0, 64, 2)));
    assert!(
        crate::services::test_support::received(&f.server)
            .await
            .iter()
            .any(|r| r.url.path() == "/rest/ping")
    );

    let not_subsonic = Fixture::answering("<html>not here</html>").await;
    assert!(not_subsonic.service.get_server_info().await.is_none());
}

/// A 200 whose envelope says "failed" (a sign-in Navidrome refused) started no scan.
#[tokio::test]
async fn a_scan_answer_that_failed_is_no_scan() {
    let f = Fixture::answering(r#"{"subsonic-response":{"status":"failed","error":{"code":40}}}"#).await;
    assert!(!f.service.trigger_library_scan(true).await);
    let not_json = Fixture::answering("<html>ok</html>").await;
    assert!(!not_json.service.trigger_library_scan(true).await);
}

/// Navidrome's last scan time and length, which say when that scan started.
#[tokio::test]
async fn scan_status_reads_when_the_last_scan_ran() {
    let f = Fixture::answering(
        r#"{"subsonic-response":{"status":"ok","scanStatus":{"scanning":false,"count":12,"folderCount":3,"lastScan":"2026-10-09T10:15:30+02:00","elapsedTime":30000000000}}}"#,
    )
    .await;
    let status = f.service.get_scan_status().await.expect("a status");
    assert_eq!(status.count, Some(12));
    assert_eq!(status.folder_count, Some(3));
    assert_eq!(status.elapsed_time, Some(30_000_000_000));
    assert_eq!(
        status.last_scan_started_at().expect("a start").to_rfc3339(),
        "2026-10-09T08:15:00+00:00"
    );
}

/// While the tidy holds scans, Octo's own triggers wait; the holder's own scan goes, and the
/// last release says a held one is owed.
#[tokio::test]
async fn held_scans_wait_and_the_holder_still_scans() {
    async fn starts(f: &Fixture) -> usize {
        crate::services::test_support::received(&f.server)
            .await
            .iter()
            .filter(|r| r.url.path() == "/rest/startScan")
            .count()
    }
    let f = Fixture::new().await;
    let hold = f.service.scan_hold().expect("a hold").hold();

    assert!(f.service.trigger_library_scan(true).await);
    assert_eq!(starts(&f).await, 0);
    assert!(f.service.start_scan().await);
    assert_eq!(starts(&f).await, 1);
    assert!(hold.release());
    assert!(f.service.trigger_library_scan(true).await);
    assert_eq!(starts(&f).await, 2);
}

/// The tidy moves a folder; every mapping inside it follows, on disk too, so the existing-file
/// check still finds the song after a restart.
#[tokio::test]
async fn update_local_path_the_mapping_follows_a_moved_folder() {
    let f = Fixture::new().await;
    let song = Song {
        title: "Session".into(),
        artist: "Bizarrap, Rauw Alejandro".into(),
        external_provider: Some("deezer".into()),
        external_id: Some("4242".into()),
        ..Song::default()
    };
    let from = f.path("Bizarrap, Rauw Alejandro/Album");
    let to = f.path("Bizarrap/Album");
    std::fs::create_dir_all(&to).expect("made");
    std::fs::write(format!("{to}/Session.flac"), "moved").expect("written");
    f.service
        .register_downloaded_song(&song, &format!("{from}/Session.flac"))
        .await
        .expect("registered");

    assert_eq!(f.service.update_local_path(&from, &to).await.expect("saved"), 1);
    assert_eq!(f.service.update_local_path(&from, &to).await.expect("saved"), 0);

    assert_eq!(
        f.service.get_local_path_for_external_song("deezer", "4242").await,
        Some(format!("{to}/Session.flac"))
    );
    let on_disk = std::fs::read_to_string(f.mappings_file()).expect("written");
    assert!(on_disk.contains("Bizarrap/Album/Session.flac"), "{on_disk}");
}

// ---- Scans of one library ----

/// A Navidrome that signs the admin in and counts `startScan` calls per library.
async fn scanning_navidrome() -> MockServer {
    use wiremock::matchers::{method, path};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"token":"jwt","isAdmin":true,"username":"admin","subsonicToken":"tok","subsonicSalt":"salt"}"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest/startScan"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SCAN_STATUS))
        .mount(&server)
        .await;
    server
}

async fn scans_of(server: &MockServer, library: &str) -> usize {
    server
        .received_requests()
        .await
        .expect("recorded")
        .iter()
        .filter(|r| r.url.path() == "/rest/startScan")
        .filter(|r| r.url.query_pairs().any(|(k, v)| k == "target" && v == library))
        .count()
}

fn admin_service(url: &str, dir: &std::path::Path) -> LocalLibraryService {
    let store = SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(url.to_string()),
            admin_username: Some("admin".into()),
            admin_password: Some("pw".into()),
            auto_detect_download_path: false,
            ..Default::default()
        },
        ..Default::default()
    });
    store.set_raw("Library:DownloadPath", Some(&dir.to_string_lossy()));
    let settings = Arc::new(store);
    let http = crate::services::http_client_factory::default_client();
    LocalLibraryService::new(
        settings.clone(),
        http.clone(),
        Arc::new(ExternalIdRegistry::new(None::<std::path::PathBuf>)),
        NavidromeIdentityService::new(settings, http),
    )
}

#[tokio::test]
async fn targeted_scan_is_debounced_per_library() {
    let server = scanning_navidrome().await;
    let dir = tempfile::tempdir().expect("temp dir");
    let notice = Arc::new(tokio::sync::Notify::new());
    let service = admin_service(&server.uri(), dir.path())
        .with_scan_window(std::time::Duration::from_millis(300))
        .with_scan_notice(notice.clone());

    // The first call for a library scans at once; another library is not held up by it.
    assert!(service.trigger_library_scan_of(3).await);
    assert!(service.trigger_library_scan_of(4).await);
    assert_eq!(scans_of(&server, "3:").await, 1);
    assert_eq!(scans_of(&server, "4:").await, 1);
    // The scan told the library index.
    tokio::time::timeout(std::time::Duration::from_secs(1), notice.notified())
        .await
        .expect("notified");

    // Calls inside the window are never dropped: they fold into one more scan after it.
    assert!(service.trigger_library_scan_of(3).await);
    assert!(service.trigger_library_scan_of(3).await);
    assert_eq!(
        scans_of(&server, "3:").await,
        1,
        "nothing more before the window ends"
    );
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert_eq!(
        scans_of(&server, "3:").await,
        2,
        "one trailing scan for both calls"
    );
    assert_eq!(scans_of(&server, "4:").await, 1);

    // After the trailing scan the window starts again from it.
    assert!(service.trigger_library_scan_of(3).await);
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert_eq!(scans_of(&server, "3:").await, 3);
}

#[tokio::test]
async fn a_full_scan_tells_the_library_index() {
    let f = Fixture::new().await;
    let notice = Arc::new(tokio::sync::Notify::new());
    let service = service_for(&f.server.uri(), f.dir.path()).with_scan_notice(notice.clone());
    assert!(service.trigger_library_scan(true).await);
    tokio::time::timeout(std::time::Duration::from_secs(1), notice.notified())
        .await
        .expect("notified");
}
