//! How the settings store layers, binds and reloads the configuration.

use super::*;
use crate::settings::{SettingsFileWriter, StorageMode};

fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    path: PathBuf,
}

fn settings_file(content: Option<&str>) -> Fixture {
    let tmp = tempfile::tempdir().expect("temp dir");
    let path = tmp.path().join("settings.json");
    if let Some(content) = content {
        std::fs::write(&path, content).expect("write settings");
    }
    Fixture { _tmp: tmp, path }
}

#[test]
fn the_built_in_appsettings_bind_without_warnings() {
    let tree = base_layers(&[]);
    let (settings, warnings) = AppSettings::bind(&tree);
    assert!(warnings.is_empty(), "{warnings:?}");
    // appsettings.json disagrees with the class defaults on these three.
    assert_eq!(settings.subsonic.storage_mode, StorageMode::Stream);
    assert!(!settings.subsonic.enable_external_playlists);
    assert_eq!(settings.metadata.lyrics_sources, "kugou,lrclib,lyricsovh");
    // And the literal U+1F6E0 escape in it arrives as the character.
    assert_eq!(settings.library_actions.playlist_prefix, "\u{1F6E0} ");
}

#[test]
fn class_default_then_appsettings_then_env_then_settings_json() {
    let file = settings_file(Some(r#"{"Subsonic": {"FolderStructure": "ByArtist"}}"#));
    let store = SettingsStore::from_env(
        env(&[
            ("Subsonic__FolderStructure", "Organized"),
            ("Subsonic__ExplicitFilter", "CleanOnly"),
            ("Subsonic__AdminUsername", "admin"),
        ]),
        Some(file.path.clone()),
    );
    let s = store.current();
    // settings.json beats env.
    assert_eq!(
        s.subsonic.folder_structure,
        crate::settings::FolderStructure::ByArtist
    );
    // env beats appsettings.json ("All").
    assert_eq!(
        s.subsonic.explicit_filter,
        crate::settings::ExplicitFilter::CleanOnly
    );
    // appsettings.json beats the class default (Permanent).
    assert_eq!(s.subsonic.storage_mode, StorageMode::Stream);
    // A key only env supplies still arrives next to the file's keys: merging is key by key.
    assert_eq!(s.subsonic.admin_username.as_deref(), Some("admin"));
    // A key nothing supplies keeps the class default.
    assert_eq!(s.server.public_url, "");
    assert!(s.listen_brainz.submit_external_plays);
}

#[test]
fn env_names_map_double_underscores_to_sections_ignoring_case() {
    let store = SettingsStore::from_env(
        env(&[
            ("SUBSONIC__URL", "http://nd:4533"),
            ("soulseek__searchwaitseconds", "45"),
            ("Genre__Blocklist__1", "b"),
            ("Genre__Blocklist__0", "a"),
            ("LISTENBRAINZ__USERTOKENS__alice", "tok"),
            ("LastFm__UserSessions__bob__SessionKey", "sk"),
            ("Single_Underscore", "x"),
        ]),
        None,
    );
    let s = store.current();
    assert_eq!(s.subsonic.url.as_deref(), Some("http://nd:4533"));
    assert_eq!(s.soulseek.search_wait_seconds, 45);
    assert_eq!(s.genre.blocklist, ["a", "b"]);
    assert_eq!(s.listen_brainz.token_for("ALICE").as_deref(), Some("tok"));
    assert_eq!(
        s.last_fm.session_for("Bob").map(|x| x.session_key.as_str()),
        Some("sk")
    );
    assert_eq!(store.raw("subsonic:url").as_deref(), Some("http://nd:4533"));
    assert_eq!(store.raw("Single_Underscore").as_deref(), Some("x"));
}

#[test]
fn raw_keys_read_through_every_layer() {
    let file = settings_file(Some(r#"{"YouTube": {"ShimUrl": "http://shim:1"}}"#));
    let store = SettingsStore::from_env(env(&[]), Some(file.path.clone()));
    assert_eq!(store.raw("Library:DownloadPath").as_deref(), Some("/music"));
    assert_eq!(store.raw("youtube:shimurl").as_deref(), Some("http://shim:1"));
    assert_eq!(store.raw("Nope:Missing"), None);
    // A list has no scalar value of its own.
    assert_eq!(store.raw("Genre:Blocklist"), None);
}

/// A cleared number field is saved as null. For a typed setting that falls through to the
/// layer below; for a string it is "" and shadows the layers below.
#[test]
fn null_in_settings_json_falls_back_for_typed_settings_and_shadows_strings() {
    let file = settings_file(Some(
        r#"{"Soulseek": {"SearchWaitSeconds": null, "PreferredExtension": null, "VerifyDownloads": null}}"#,
    ));
    let store = SettingsStore::from_env(
        env(&[
            ("Soulseek__SearchWaitSeconds", "45"),
            ("Soulseek__PreferredExtension", "mp3"),
        ]),
        Some(file.path.clone()),
    );
    let s = store.current();
    assert_eq!(s.soulseek.search_wait_seconds, 45);
    // appsettings.json's own value, beneath the cleared one.
    assert!(s.soulseek.verify_downloads);
    assert_eq!(s.soulseek.preferred_extension, "");
    assert_eq!(store.raw("Soulseek:SearchWaitSeconds").as_deref(), Some(""));
}

#[test]
fn an_unconvertible_value_keeps_the_lower_layer_and_its_neighbours() {
    let file = settings_file(Some(r#"{"Genre": {"MaxGenres": "lots", "Enabled": true}}"#));
    let store = SettingsStore::from_env(env(&[]), Some(file.path.clone()));
    let s = store.current();
    assert_eq!(s.genre.max_genres, 10);
    assert!(s.genre.enabled);
}

#[test]
fn development_adds_appsettings_development_json() {
    let key = "Logging:LogLevel:Microsoft.AspNetCore";
    assert_eq!(SettingsStore::from_env(env(&[]), None).raw(key), None);
    let dev = SettingsStore::from_env(env(&[("ASPNETCORE_ENVIRONMENT", "Development")]), None);
    assert_eq!(dev.raw(key).as_deref(), Some("Warning"));
    let dotnet = SettingsStore::from_env(env(&[("DOTNET_ENVIRONMENT", "development")]), None);
    assert_eq!(dotnet.raw(key).as_deref(), Some("Warning"));
    // ASPNETCORE_ENVIRONMENT wins over DOTNET_ENVIRONMENT.
    let both = SettingsStore::from_env(
        env(&[
            ("DOTNET_ENVIRONMENT", "Development"),
            ("ASPNETCORE_ENVIRONMENT", "Production"),
        ]),
        None,
    );
    assert_eq!(both.raw(key), None);
    // The prefix-stripped host keys are readable too.
    assert_eq!(dev.raw("environment").as_deref(), Some("Development"));
}

#[test]
fn a_settings_json_that_does_not_parse_at_startup_is_ignored() {
    let file = settings_file(Some(r#"{"Subsonic": {"Url": "x""#));
    let store = SettingsStore::from_env(env(&[("Subsonic__Url", "from-env")]), Some(file.path.clone()));
    assert_eq!(store.current().subsonic.url.as_deref(), Some("from-env"));
}

#[test]
fn case_only_duplicate_keys_in_the_file_are_a_parse_error() {
    assert!(parse_config_json(r#"{"Subsonic": {"Url": "a", "url": "b"}}"#).is_err());
    assert!(parse_config_json("[]").is_err());
    assert!(parse_config_json(r#"{"A": {"N": 1.50, "On": true, "E": [], "O": {}}}"#).is_ok());
    let tree = parse_config_json(r#"{"A": {"N": 1.50, "On": false, "E": []}}"#).unwrap();
    assert_eq!(tree.get("a:n"), Some("1.50"));
    assert_eq!(tree.get("A:On"), Some("False"));
    assert!(tree.contains("A:E"));
    assert_eq!(tree.get("A:E"), None);
}

#[test]
fn reload_now_picks_up_a_rewrite_and_keeps_the_last_good_snapshot_on_a_bad_one() {
    let file = settings_file(Some(r#"{"Genre": {"MaxGenres": 3}}"#));
    let store = SettingsStore::from_env(env(&[]), Some(file.path.clone()));
    let mut rx = store.subscribe();
    assert_eq!(store.current().genre.max_genres, 3);

    std::fs::write(&file.path, r#"{"Genre": {"MaxGenres": 4}}"#).unwrap();
    store.reload_now().unwrap();
    assert_eq!(store.current().genre.max_genres, 4);
    assert!(rx.has_changed().unwrap());
    assert_eq!(rx.borrow_and_update().genre.max_genres, 4);

    std::fs::write(&file.path, r#"{"Genre": {"MaxGenres": "#).unwrap();
    assert!(store.reload_now().is_err());
    assert_eq!(store.current().genre.max_genres, 4);
    assert_eq!(store.raw("Genre:MaxGenres").as_deref(), Some("4"));
    assert!(!rx.has_changed().unwrap());

    // A deleted file is an empty layer, not an error.
    std::fs::remove_file(&file.path).unwrap();
    store.reload_now().unwrap();
    assert_eq!(store.current().genre.max_genres, 10);
}

#[test]
fn only_a_change_to_the_spotiflac_section_counts_as_a_spotiflac_change() {
    // Through a set.
    let store = SettingsStore::for_tests(AppSettings::default());
    let mut other = AppSettings::default();
    other.genre.max_genres = 2;
    store.set(other.clone());
    assert_eq!(store.spotiflac_changes(), 0, "another section changed");
    let mut on = other.clone();
    on.spotiflac.enabled = true;
    store.set(on);
    store.set(other);
    assert_eq!(store.spotiflac_changes(), 2, "switched on, then off again");

    // Through a reload of settings.json.
    let file = settings_file(Some(r#"{"Genre": {"MaxGenres": 3}}"#));
    let store = SettingsStore::from_env(env(&[]), Some(file.path.clone()));
    std::fs::write(&file.path, r#"{"Genre": {"MaxGenres": 4}}"#).unwrap();
    store.reload_now().unwrap();
    store.reload_now().unwrap();
    assert_eq!(
        store.spotiflac_changes(),
        0,
        "another section, and a reload with no change"
    );
    std::fs::write(
        &file.path,
        r#"{"Genre": {"MaxGenres": 4}, "Spotiflac": {"Enabled": true}}"#,
    )
    .unwrap();
    store.reload_now().unwrap();
    assert_eq!(store.spotiflac_changes(), 1);
}

#[test]
fn for_tests_set_notifies_subscribers() {
    let store = SettingsStore::for_tests(AppSettings::default());
    let mut rx = store.subscribe();

    let mut next = AppSettings::default();
    next.soulseek.parallel_downloads = 5;
    store.set(next);

    assert!(rx.has_changed().unwrap());
    assert_eq!(rx.borrow_and_update().soulseek.parallel_downloads, 5);
    assert_eq!(store.current().soulseek.parallel_downloads, 5);
    // A test store has nothing to reload from, so a reload keeps what set gave it.
    store.reload_now().unwrap();
    assert_eq!(store.current().soulseek.parallel_downloads, 5);

    store.set_raw("Library:DownloadPath", Some("/x"));
    assert_eq!(store.raw("library:downloadpath").as_deref(), Some("/x"));
}

#[test]
fn restart_tracker_reads_the_live_store() {
    let file = settings_file(Some(r#"{"Soulseek": {"Password": "a"}}"#));
    let store = SettingsStore::from_env(env(&[]), Some(file.path.clone()));
    let tracker = crate::settings::RestartTracker::new(&store);
    assert!(tracker.pending(&store).is_empty());

    std::fs::write(&file.path, r#"{"Soulseek": {"Password": "b"}}"#).unwrap();
    store.reload_now().unwrap();
    assert_eq!(tracker.pending(&store), ["Soulseek:Password"]);
}

#[test]
fn the_default_path_can_be_overridden_for_local_runs() {
    assert_eq!(
        SettingsStore::default_settings_path_from([]),
        PathBuf::from("/app/config/settings.json")
    );
    assert_eq!(
        SettingsStore::default_settings_path_from([(SETTINGS_PATH_ENV, "/tmp/s.json")]),
        PathBuf::from("/tmp/s.json")
    );
    assert_eq!(
        SettingsStore::default_settings_path_from([(SETTINGS_PATH_ENV, " ")]),
        PathBuf::from("/app/config/settings.json")
    );
}

async fn wait_for(rx: &mut watch::Receiver<Arc<AppSettings>>, check: impl Fn(&AppSettings) -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if check(&rx.borrow_and_update()) {
                return;
            }
            rx.changed().await.expect("store alive");
        }
    })
    .await
    .expect("the change arrives within 10 s");
}

/// The dashboard's save path end to end: the writer's tmp + rename is seen and reloaded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_writer_save_is_picked_up_by_the_watcher() {
    let file = settings_file(Some(r#"{"Genre": {"MaxGenres": 3}}"#));
    let store = SettingsStore::from_env(env(&[]), Some(file.path.clone()));
    store.start_watching().unwrap();
    let mut rx = store.subscribe();

    let writer = SettingsFileWriter::new(&file.path);
    let mut patch = crate::settings::JsonObject::new();
    patch.insert("Genre".into(), Node::parse(r#"{"MaxGenres": 7}"#).expect("json"));
    writer.merge(&patch, &[]).unwrap();

    wait_for(&mut rx, |s| s.genre.max_genres == 7).await;
    assert_eq!(store.raw("Genre:MaxGenres").as_deref(), Some("7"));
}

/// /app/config may not exist when Octo starts; a settings.json created there later still counts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settings_file_in_a_directory_created_later_is_picked_up() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config").join("settings.json");
    let store = SettingsStore::from_env(env(&[]), Some(path.clone()));
    store.start_watching().unwrap();
    let mut rx = store.subscribe();
    assert_eq!(store.current().genre.max_genres, 10);

    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, r#"{"Genre": {"MaxGenres": 2}}"#).unwrap();
    wait_for(&mut rx, |s| s.genre.max_genres == 2).await;

    // And the directory is watched from then on.
    std::fs::write(&path, r#"{"Genre": {"MaxGenres": 5}}"#).unwrap();
    wait_for(&mut rx, |s| s.genre.max_genres == 5).await;
}

#[test]
fn the_search_duration_wait_binds_from_the_environment_and_is_clamped() {
    let none = settings_file(None);
    let default = SettingsStore::from_env(Vec::new(), Some(none.path.clone()));
    assert_eq!(default.current().subsonic.search_duration_wait_ms, 1000);
    for (value, expected) in [("250", 250), ("0", 0), ("90000", 10_000)] {
        let file = settings_file(None);
        let store = SettingsStore::from_env(
            env(&[("Subsonic__SearchDurationWaitMs", value)]),
            Some(file.path.clone()),
        );
        assert_eq!(
            store.current().subsonic.effective_search_duration_wait(),
            std::time::Duration::from_millis(expected),
            "{value}"
        );
    }
    let file = settings_file(Some(r#"{"Subsonic": {"WaitForSearchDurations": false}}"#));
    let store = SettingsStore::from_env(
        env(&[("Subsonic__SearchDurationWaitMs", "5000")]),
        Some(file.path.clone()),
    );
    assert_eq!(
        store.current().subsonic.effective_search_duration_wait(),
        std::time::Duration::ZERO
    );
}
