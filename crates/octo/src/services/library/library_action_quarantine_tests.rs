//! The library action trash. Nothing in library actions deletes a file except the retention
//! sweep. These actions remove files Octo did NOT create, on one tap in a music client with no
//! confirmation dialog, and the whole point of the feature is that the user is correcting a
//! mistake, which means they can make one.

use octo_core::settings::{AppSettings, LibraryActionSettings};

use super::*;
use crate::services::library::PathSource;

struct Fixture {
    _dir: tempfile::TempDir,
    root: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("music").to_string_lossy().into_owned();
        std::fs::create_dir_all(&root).expect("music root");
        Fixture { _dir: dir, root }
    }

    fn write_track(&self, relative: &str, bytes: usize) -> ResolvedSongFile {
        let full = format!("{}/{relative}", self.root);
        std::fs::create_dir_all(Path::new(&full).parent().expect("a folder")).expect("folders");
        std::fs::write(&full, vec![0u8; bytes]).expect("written");
        track(&full, bytes as i64)
    }
}

fn track(full: &str, bytes: i64) -> ResolvedSongFile {
    ResolvedSongFile {
        navidrome_id: "song-id".into(),
        absolute_path: full.into(),
        size_bytes: bytes,
        title: "Title".into(),
        artist: "Artist".into(),
        album: "Album".into(),
        suffix: Path::new(full)
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default(),
        duration_seconds: Some(180),
        source: PathSource::NativeApi,
        album_artist: None,
    }
}

fn quarantine(settings: LibraryActionSettings) -> LibraryActionQuarantine {
    LibraryActionQuarantine::new(Arc::new(SettingsStore::for_tests(AppSettings {
        library_actions: settings,
        ..Default::default()
    })))
}

fn default_quarantine() -> LibraryActionQuarantine {
    quarantine(LibraryActionSettings::default())
}

fn exists(path: &str) -> bool {
    Path::new(path).exists()
}

#[test]
fn move_takes_the_file_out_of_the_library_without_deleting_it() {
    let f = Fixture::new();
    let track = f.write_track("Artist/Album/Song.flac", 1024);

    let result = default_quarantine().move_file(&track, &f.root, LibraryAction::Delete, "alice");

    assert!(result.moved);
    assert!(!exists(&track.absolute_path));
    assert!(exists(result.quarantine_path.as_deref().expect("a path")));
}

/// The layout is preserved so a restore is a straight move back.
#[test]
fn move_preserves_the_relative_layout_under_a_dated_folder() {
    let f = Fixture::new();
    let track = f.write_track("Artist/Album/Song.flac", 1024);

    let result = default_quarantine().move_file(&track, &f.root, LibraryAction::Delete, "alice");

    let path = result.quarantine_path.expect("a path");
    assert!(path.contains("Artist/Album/Song.flac"), "{path}");
    assert!(
        path.contains(&Utc::now().format("%Y-%m-%d").to_string()),
        "{path}"
    );
}

#[test]
fn restore_puts_the_file_back_where_it_came_from() {
    let f = Fixture::new();
    let track = f.write_track("Artist/Album/Song.flac", 1024);
    let quarantine = default_quarantine();

    let moved = quarantine.move_file(&track, &f.root, LibraryAction::Delete, "alice");
    let moved_to = moved.quarantine_path.expect("a path");
    let restored = quarantine.restore(&moved_to, None);

    assert!(restored.moved);
    assert!(exists(&track.absolute_path));
    assert!(!exists(&moved_to));
}

/// The manifest is written next to the quarantined file so a restore works even if the
/// journal is lost.
#[test]
fn restore_works_from_the_sidecar_manifest_alone() {
    let f = Fixture::new();
    let track = f.write_track("Artist/Album/Song.flac", 1024);
    let moved = default_quarantine().move_file(&track, &f.root, LibraryAction::Delete, "alice");
    let moved_to = moved.quarantine_path.expect("a path");

    assert!(exists(&format!("{moved_to}.octo-action.json")));

    // A completely separate instance, with no shared state at all.
    assert!(default_quarantine().restore(&moved_to, None).moved);
    assert!(exists(&track.absolute_path));
}

#[test]
fn restore_something_already_at_the_original_path_refuses_rather_than_overwriting() {
    let f = Fixture::new();
    let track = f.write_track("Artist/Album/Song.flac", 1024);
    let quarantine = default_quarantine();
    let moved = quarantine.move_file(&track, &f.root, LibraryAction::Delete, "alice");
    let moved_to = moved.quarantine_path.expect("a path");

    // A replacement arrived in the meantime.
    f.write_track("Artist/Album/Song.flac", 2048);

    let restored = quarantine.restore(&moved_to, None);
    assert!(!restored.moved);
    assert!(exists(&moved_to));
}

#[test]
fn move_collision_keeps_both_rather_than_overwriting() {
    let f = Fixture::new();
    let quarantine = default_quarantine();
    let first = f.write_track("Artist/Album/Song.flac", 1024);
    let first_moved = quarantine.move_file(&first, &f.root, LibraryAction::Delete, "alice");

    let second = f.write_track("Artist/Album/Song.flac", 2048);
    let second_moved = quarantine.move_file(&second, &f.root, LibraryAction::Delete, "alice");

    assert_ne!(first_moved.quarantine_path, second_moved.quarantine_path);
    assert!(exists(first_moved.quarantine_path.as_deref().expect("a path")));
    let second_path = second_moved.quarantine_path.expect("a path");
    assert!(exists(&second_path));
    assert!(second_path.ends_with("Song (2).flac"), "{second_path}");
}

#[test]
fn move_file_outside_the_music_root_is_refused() {
    let f = Fixture::new();
    let outside = f
        ._dir
        .path()
        .join(format!("octo-outside-{}.flac", uuid::Uuid::new_v4()));
    std::fs::write(&outside, [0u8; 16]).expect("written");
    let outside = outside.to_string_lossy().into_owned();

    let result =
        default_quarantine().move_file(&track(&outside, 16), &f.root, LibraryAction::Delete, "alice");

    assert!(!result.moved);
    assert!(exists(&outside));
}

/// The only code in this feature that really deletes, and it goes on age.
#[test]
fn sweep_removes_only_folders_past_the_retention_window() {
    let f = Fixture::new();
    let quarantine = quarantine(LibraryActionSettings {
        quarantine_retention_days: 30,
        ..Default::default()
    });
    let root = quarantine.root_for(&f.root);

    let old_day = format!("{root}/{}", (Utc::now() - TimeDelta::days(40)).format("%Y-%m-%d"));
    let recent = format!(
        "{root}/{}/b",
        (Utc::now() - TimeDelta::days(2)).format("%Y-%m-%d")
    );
    std::fs::create_dir_all(format!("{old_day}/a")).expect("old");
    std::fs::create_dir_all(&recent).expect("recent");
    std::fs::write(format!("{old_day}/a/gone.flac"), [0u8; 8]).expect("written");
    std::fs::write(format!("{old_day}/a/gone.flac.octo-action.json"), "{}").expect("written");
    std::fs::write(format!("{recent}/kept.flac"), [0u8; 8]).expect("written");

    assert_eq!(quarantine.sweep(&f.root), 1);
    assert!(!exists(&old_day));
    assert!(exists(&format!("{recent}/kept.flac")));
}

/// The sweep names the Navidrome ids of what it deleted for good, each once, from the notes in
/// every folder it removed; a note that names none, or cannot be read, adds nothing, and what
/// stays in the trash is not named.
#[test]
fn sweep_gone_names_the_navidrome_ids_of_the_files_it_deleted() {
    let f = Fixture::new();
    let quarantine = quarantine(LibraryActionSettings {
        quarantine_retention_days: 30,
        ..Default::default()
    });
    let root = quarantine.root_for(&f.root);
    let day = |ago: i64| (Utc::now() - TimeDelta::days(ago)).format("%Y-%m-%d").to_string();
    let note =
        |id: &str| format!(r#"{{"OriginalPath":"/music/x.flac","NavidromeId":"{id}","Action":"Delete"}}"#);
    let put = |file: &str, body: &str| {
        let path = format!("{root}/{file}");
        std::fs::create_dir_all(Path::new(&path).parent().expect("folder")).expect("folders");
        std::fs::write(path, body).expect("written");
    };
    put(&format!("{}/A - One.flac", day(40)), "audio");
    put(
        &format!("{}/A - One.flac.octo-action.json", day(40)),
        &note("nd-one"),
    );
    put(&format!("{}/Album/02 - Two.flac", day(40)), "audio");
    put(
        &format!("{}/Album/02 - Two.flac.octo-action.json", day(40)),
        &note("nd-two"),
    );
    put(
        &format!("{}/Album/03 - Again.flac.octo-action.json", day(40)),
        &note("nd-one"),
    );
    put(
        &format!("{}/B - Blank.flac.octo-action.json", day(50)),
        &note(" "),
    );
    put(
        &format!("{}/B - Broken.flac.octo-action.json", day(50)),
        "not json",
    );
    put(&format!("{}/C - Kept.flac", day(2)), "audio");
    put(
        &format!("{}/C - Kept.flac.octo-action.json", day(2)),
        &note("nd-kept"),
    );

    let swept = quarantine.sweep_gone(&f.root);

    let mut ids = swept.navidrome_ids.clone();
    ids.sort();
    assert_eq!(ids, ["nd-one", "nd-two"]);
    assert_eq!(swept.files, 2);
    assert!(exists(&format!("{root}/{}/C - Kept.flac", day(2))));
}

/// Retention 0 means never sweep, for anyone who would rather manage the space by hand.
#[test]
fn sweep_retention_zero_deletes_nothing_ever() {
    let f = Fixture::new();
    let quarantine = quarantine(LibraryActionSettings {
        quarantine_retention_days: 0,
        ..Default::default()
    });
    let ancient = format!(
        "{}/{}",
        quarantine.root_for(&f.root),
        (Utc::now() - TimeDelta::days(5 * 365)).format("%Y-%m-%d")
    );
    std::fs::create_dir_all(&ancient).expect("ancient");
    std::fs::write(format!("{ancient}/kept.flac"), [0u8; 8]).expect("written");

    assert_eq!(quarantine.sweep(&f.root), 0);
    assert!(exists(&format!("{ancient}/kept.flac")));
}

#[test]
fn root_for_uses_the_configured_directory() {
    let f = Fixture::new();
    assert_eq!(
        quarantine(LibraryActionSettings {
            quarantine_directory: "my-bin".into(),
            ..Default::default()
        })
        .root_for(&f.root),
        format!("{}/my-bin", f.root)
    );
}

// ---- The manifest ------------------------------------------------------------------------

fn fixture_manifest() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(
        "../../docs/rust-migration/fixtures/state/music/.octo-trash/2026-10-03/Sigur Rós/Ágætis byrjun/02 - Svefn-g-englar.flac.octo-action.json",
    )
}

/// The manifest reads and writes back byte for byte.
#[test]
fn the_manifest_fixture_round_trips_byte_for_byte() {
    let original = std::fs::read_to_string(fixture_manifest()).expect("the fixture");
    let manifest: QuarantineManifest = serde_json::from_str(&original).expect("a manifest");
    assert_eq!(
        manifest.original_path,
        "/music/Sigur Rós/Ágætis byrjun/02 - Svefn-g-englar.flac"
    );
    assert_eq!(manifest.action, "Delete");
    assert_eq!(octo_core::json::to_string(&manifest), original);
}

/// What `move_file` writes has the fixture's shape: the action by name, the time with its `Z`.
#[test]
fn a_move_writes_the_manifest_in_the_fixture_shape() {
    let f = Fixture::new();
    let track = f.write_track("Sigur Rós/Ágætis byrjun/02 - Svefn-g-englar.flac", 64);
    let moved = default_quarantine().move_file(&track, &f.root, LibraryAction::WrongSong, "brandon");
    let moved_to = moved.quarantine_path.expect("a path");

    let written = std::fs::read_to_string(format!("{moved_to}.octo-action.json")).expect("a manifest");
    let manifest = default_quarantine().read_manifest(&moved_to).expect("readable");
    assert_eq!(manifest.action, "WrongSong");
    assert_eq!(manifest.navidrome_id, "song-id");
    assert!(written.starts_with(r#"{"OriginalPath":""#), "{written}");
    assert!(written.contains(r#"Sigur R\u00F3s"#), "{written}");
    // The sidecars and links follow, both null here.
    assert!(
        written.ends_with(r#"Z","Sidecars":null,"Mappings":null}"#),
        "{written}"
    );
    assert!(written.contains(r#","Action":"WrongSong","Username":"brandon","AtUtc":""#));
}

/// A manifest that cannot be read refuses the restore rather than guessing.
#[test]
fn a_restore_without_a_readable_manifest_is_refused() {
    let f = Fixture::new();
    let lost = f.write_track(".octo-trash/2026-01-01/Song.flac", 8);
    let restored = default_quarantine().restore(&lost.absolute_path, None);
    assert_eq!(
        restored.error.as_deref(),
        Some("no manifest, so the original path is unknown")
    );
    assert!(!default_quarantine().restore("/nonexistent/x.flac", None).moved);
}

#[test]
fn days_parse_strictly_and_relative_paths_climb_with_dots() {
    assert!(parse_day("2026-10-03").is_some());
    assert!(parse_day("2026-1-03").is_none());
    assert!(parse_day("not-a-day").is_none());
    assert!(parse_day("2026-13-01").is_none());
    assert_eq!(get_relative_path("/music", "/music/A/B.flac"), "A/B.flac");
    assert_eq!(get_relative_path("/music", "/other/B.flac"), "../other/B.flac");
    // A `..` prefix check also refuses a top folder whose name starts with two dots.
    assert!(get_relative_path("/music", "/music/..odd/B.flac").starts_with(".."));
}

// ---- Containment: the last check before a file leaves the library: inside the music folder,
// not in the trash already, and a real file. -------------------------------------------------

#[test]
fn a_folder_that_only_starts_with_the_music_folders_name_is_outside() {
    let f = Fixture::new();
    let full = format!("{}-old/Song.flac", f.root);
    std::fs::create_dir_all(Path::new(&full).parent().expect("a folder")).expect("folders");
    std::fs::write(&full, [1u8, 2, 3]).expect("written");

    let moved = default_quarantine().move_file(&track(&full, 3), &f.root, LibraryAction::Delete, "alice");

    assert!(!moved.moved);
    assert_eq!(
        moved.error.as_deref(),
        Some("the file is not under the music root")
    );
    assert!(exists(&full));
}

#[test]
fn a_path_that_climbs_out_is_outside() {
    let f = Fixture::new();
    let real = f._dir.path().join("elsewhere").join("Song.flac");
    std::fs::create_dir_all(real.parent().expect("a folder")).expect("folders");
    std::fs::write(&real, [1u8, 2, 3]).expect("written");
    let climbing = format!("{}/../elsewhere/Song.flac", f.root);

    assert!(
        !default_quarantine()
            .move_file(&track(&climbing, 3), &f.root, LibraryAction::Delete, "alice")
            .moved
    );
    assert!(real.exists());
}

#[test]
fn a_file_in_the_trash_already_is_not_moved_again() {
    let f = Fixture::new();
    let file = f.write_track(".octo-trash/2026-10-01/Song.flac", 3);

    let moved = default_quarantine().move_file(&file, &f.root, LibraryAction::Delete, "alice");

    assert!(!moved.moved);
    assert_eq!(moved.error.as_deref(), Some("the file is in the trash already"));
    assert!(exists(&file.absolute_path));
}

#[test]
fn a_song_inside_the_music_folder_goes_to_the_trash() {
    let f = Fixture::new();
    let file = f.write_track("A/B/Song.flac", 3);

    let moved = default_quarantine().move_file(&file, &f.root, LibraryAction::Delete, "alice");

    assert!(moved.moved);
    assert!(!exists(&file.absolute_path));
    assert!(
        moved
            .quarantine_path
            .as_deref()
            .expect("a place in the trash")
            .starts_with(&format!("{}/.octo-trash", f.root))
    );
}

/// A link inside the music folder is not a song file, whatever it points at.
#[cfg(unix)]
#[test]
fn a_link_is_not_moved() {
    let f = Fixture::new();
    let target = f.write_track("A/B/Real.flac", 3);
    let link = format!("{}/A/B/Link.flac", f.root);
    std::os::unix::fs::symlink(&target.absolute_path, &link).expect("a link");

    let moved = default_quarantine().move_file(&track(&link, 3), &f.root, LibraryAction::Delete, "alice");

    assert!(!moved.moved);
    assert_eq!(
        moved.error.as_deref(),
        Some("the file is a link, not a song file")
    );
    assert!(Path::new(&link).exists());
}

/// The sidecars go with the song into the trash and come back with it, and a song with another
/// copy beside it keeps the lyrics they share.
#[test]
fn sidecars_go_to_the_trash_with_the_song_and_come_back() {
    let f = Fixture::new();
    let file = f.write_track("Daft Punk/Discovery/03 - Digital Love.flac", 3);
    let folder = format!("{}/Daft Punk/Discovery", f.root);
    for name in [
        "03 - Digital Love.lrc",
        "03 - Digital Love.TXT",
        "cover.jpg",
        "04 - Other.lrc",
    ] {
        std::fs::write(format!("{folder}/{name}"), name).expect("written");
    }
    let mut sidecars = LibraryActionQuarantine::sidecars_of(&file.absolute_path);
    sidecars.sort();
    assert_eq!(
        sidecars,
        [
            format!("{folder}/03 - Digital Love.TXT"),
            format!("{folder}/03 - Digital Love.lrc")
        ]
    );

    let quarantine = default_quarantine();
    let moved = quarantine.move_file_with_sidecars(&file, &f.root, LibraryAction::Delete, "alice", true);
    assert!(moved.moved);
    let trashed = moved.quarantine_path.expect("a place in the trash");
    let mut names = quarantine.sidecar_names(&trashed);
    names.sort();
    assert_eq!(names, ["03 - Digital Love.TXT", "03 - Digital Love.lrc"]);
    assert!(!exists(&format!("{folder}/03 - Digital Love.lrc")));
    assert!(exists(&format!("{folder}/cover.jpg")));

    // Back only into the music folder.
    assert!(!quarantine.restore(&trashed, Some("/elsewhere")).moved);
    let restored = quarantine.restore(&trashed, Some(&f.root));
    assert!(restored.moved);
    assert_eq!(
        std::fs::read_to_string(format!("{folder}/03 - Digital Love.lrc")).expect("back"),
        "03 - Digital Love.lrc"
    );
    assert!(quarantine.sidecar_names(&trashed).is_empty());

    std::fs::write(format!("{folder}/03 - Digital Love.mp3"), "the other copy").expect("written");
    assert!(LibraryActionQuarantine::sidecars_of(&file.absolute_path).is_empty());
}

/// Put back never makes a second name that Windows and SMB read as one already there: a file of
/// the song's name in other capitals, or (on a disk that tells capitals apart) its folder by
/// another spelling.
#[test]
fn restore_refuses_a_case_twin() {
    let f = Fixture::new();
    let track = f.write_track("Artist/Album/Song.flac", 1024);
    let quarantine = default_quarantine();
    let trashed = quarantine
        .move_file(&track, &f.root, LibraryAction::Delete, "alice")
        .quarantine_path
        .expect("a path");
    f.write_track("Artist/Album/SONG.flac", 2048);

    let restored = quarantine.restore(&trashed, None);
    assert!(!restored.moved);
    assert!(exists(&trashed), "it stays in the trash");
    assert!(!restored.error.unwrap_or_default().is_empty());

    #[cfg(unix)]
    {
        let track = f.write_track("Other/Album/Song.flac", 1024);
        let trashed = quarantine
            .move_file(&track, &f.root, LibraryAction::Delete, "alice")
            .quarantine_path
            .expect("a path");
        let _ = std::fs::remove_dir_all(format!("{}/Other", f.root));
        f.write_track("other/Album/Else.flac", 10);

        let restored = quarantine.restore(&trashed, None);
        assert!(!restored.moved);
        assert!(!exists(&format!("{}/Other", f.root)), "no twin folder made");
        assert_eq!(
            restored.error.as_deref(),
            Some("something of the same name in other capitals is already at the original path")
        );
    }
}
