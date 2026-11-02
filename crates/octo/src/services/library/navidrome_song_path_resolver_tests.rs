//! The resolver is the last thing standing between a Navidrome song id and anything that acts on
//! a file.
//!
//! Navidrome's Subsonic `path` is SYNTHESISED FROM TAGS unless the calling player has
//! ReportRealPath set, which defaults off. Verified against the production library on
//! 2026-09-16: six random tracks, six different answers, because that library is flat
//! (`Artist - Title.flac`) while the API reports `Artist/Album/Title.flac`. On a flat library the
//! fake path resolves to nothing, which fails safe. On an Organized library it can name a real
//! file that is a DIFFERENT recording, and the byte-size check is what catches that.

use super::*;
use crate::services::local::test_support::FakeLocalLibrary;
use octo_core::settings::AppSettings;

struct Fixture {
    _dir: tempfile::TempDir,
    root: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().to_string_lossy().into_owned();
        Fixture { _dir: dir, root }
    }

    fn write_file(&self, relative: &str, bytes: usize) -> String {
        let full = format!("{}/{relative}", self.root);
        if let Some(parent) = Path::new(&full).parent() {
            std::fs::create_dir_all(parent).expect("create dirs");
        }
        std::fs::write(&full, vec![0u8; bytes]).expect("write file");
        full
    }
}

fn candidate(path: Option<&str>, size: i64, suffix: &str, library_path: Option<&str>) -> Candidate {
    Candidate {
        id: "song-id".into(),
        raw_path: path.map(str::to_string),
        library_path: library_path.map(str::to_string),
        size,
        title: "Title".into(),
        artist: "Artist".into(),
        album: "Album".into(),
        suffix: suffix.into(),
        duration: Some(180),
        source: PathSource::SubsonicGetSong,
        missing: false,
        album_artist: None,
    }
}

fn resolver() -> NavidromeSongPathResolver {
    let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
    let http = crate::services::http_client_factory::default_client();
    NavidromeSongPathResolver::new(
        NavidromeIdentityService::new(Arc::clone(&settings), http.clone()),
        Arc::new(FakeLocalLibrary::default()),
        http,
        settings,
    )
}

#[test]
fn verify_path_and_size_agree_resolves() {
    let f = Fixture::new();
    f.write_file("Artist/Album/Song.flac", 2048);

    let resolved = resolver()
        .verify(
            &candidate(Some("Artist/Album/Song.flac"), 2048, "flac", None),
            &f.root,
            false,
        )
        .expect("resolved");

    assert_eq!(resolved.size_bytes, 2048);
    assert!(resolved.absolute_path.ends_with("Song.flac"));
}

/// The fakePath case, and the reason the size check exists. The file at the reported path is
/// real; it is simply not the recording that was asked for. Without this check that is a
/// silent action on the wrong file.
#[test]
fn verify_size_mismatch_refuses_even_though_the_file_exists() {
    let f = Fixture::new();
    f.write_file("Artist/Album/Song.flac", 1024);

    assert!(
        resolver()
            .verify(
                &candidate(Some("Artist/Album/Song.flac"), 9999, "flac", None),
                &f.root,
                false
            )
            .is_none()
    );
}

#[test]
fn verify_suffix_mismatch_is_refused() {
    let f = Fixture::new();
    f.write_file("Artist/Album/Song.mp3", 2048);

    assert!(
        resolver()
            .verify(
                &candidate(Some("Artist/Album/Song.mp3"), 2048, "flac", None),
                &f.root,
                false
            )
            .is_none()
    );
}

/// A path from a differently-mounted Navidrome must never reach outside the music root, which
/// is where Octo's own config and the quarantine directory live.
#[test]
fn verify_path_escaping_the_music_root_is_rejected() {
    let f = Fixture::new();
    for path in ["../../etc/passwd", "Artist/../../../outside.flac"] {
        assert!(
            resolver()
                .verify(&candidate(Some(path), 2048, "flac", None), &f.root, false)
                .is_none(),
            "{path}"
        );
    }
}

#[test]
fn verify_no_path_reported_resolves_nothing() {
    let f = Fixture::new();
    assert!(
        resolver()
            .verify(&candidate(None, 2048, "flac", None), &f.root, false)
            .is_none()
    );
    assert!(
        resolver()
            .verify(&candidate(Some(""), 2048, "flac", None), &f.root, false)
            .is_none()
    );
}

/// Navidrome reporting size 0 means it did not say, not that the file is empty. The other three
/// checks still have to hold.
#[test]
fn verify_no_size_reported_falls_back_to_the_other_checks() {
    let f = Fixture::new();
    f.write_file("Artist/Album/Song.flac", 2048);

    assert!(
        resolver()
            .verify(
                &candidate(Some("Artist/Album/Song.flac"), 0, "flac", None),
                &f.root,
                false
            )
            .is_some()
    );
    assert!(
        resolver()
            .verify(
                &candidate(Some("Artist/Album/Missing.flac"), 0, "flac", None),
                &f.root,
                false
            )
            .is_none()
    );
}

/// libraryPath is Navidrome's own root and wins when the two containers share a mount. Octo's
/// root is the fallback, which is what the shipped compose file produces since both mount the
/// same directory at /music.
#[test]
fn candidate_paths_prefers_library_path_then_the_music_root() {
    let f = Fixture::new();
    let paths = NavidromeSongPathResolver::candidate_paths(
        &candidate(Some("Artist/Album/Song.flac"), 1, "flac", Some(&f.root)),
        &f.root,
    );

    assert_eq!(paths.len(), 2);
    assert!(
        paths.iter().all(|p| p.ends_with("Artist/Album/Song.flac")),
        "{paths:?}"
    );
}

/// A pre-0.58 Navidrome stored absolute paths. If the two containers mount the library at
/// different places, the tail is the only part still worth trying.
#[test]
fn candidate_paths_absolute_path_also_tries_the_root_relative_tail() {
    let f = Fixture::new();
    let absolute = "/media/Artist/Album/Song.flac";

    let paths =
        NavidromeSongPathResolver::candidate_paths(&candidate(Some(absolute), 1, "flac", None), &f.root);

    assert!(paths.iter().any(|p| p == absolute), "{paths:?}");
    assert!(
        paths
            .iter()
            .any(|p| p.ends_with("Artist/Album/Song.flac") && p.starts_with(&f.root)),
        "{paths:?}"
    );
}

/// A flat library (`Artist - Title.flac` in the music folder) that Navidrome mounts somewhere
/// else, deeper: only the shortest tail is the file, and it still resolves.
#[test]
fn a_flat_file_on_another_mount_resolves() {
    let f = Fixture::new();
    f.write_file("Massive Attack - Teardrop.flac", 4096);

    let resolved = resolver()
        .verify(
            &candidate(
                Some("/data/media/music/Massive Attack - Teardrop.flac"),
                4096,
                "flac",
                None,
            ),
            &f.root,
            false,
        )
        .expect("resolved");

    assert!(
        resolved
            .absolute_path
            .ends_with("/Massive Attack - Teardrop.flac"),
        "{}",
        resolved.absolute_path
    );
    let paths = NavidromeSongPathResolver::candidate_paths(
        &candidate(Some("/data/media/A/B/Song.flac"), 1, "flac", None),
        &f.root,
    );
    let tails: Vec<&String> = paths.iter().filter(|p| p.starts_with(&f.root)).collect();
    assert_eq!(
        tails,
        [
            &format!("{}/data/media/A/B/Song.flac", f.root),
            &format!("{}/media/A/B/Song.flac", f.root),
            &format!("{}/A/B/Song.flac", f.root),
            &format!("{}/B/Song.flac", f.root),
            &format!("{}/Song.flac", f.root),
        ],
        "longest first"
    );
}

#[test]
fn is_inside_only_accepts_paths_under_the_root() {
    let f = Fixture::new();
    let inside = format!("{}/Artist/Song.flac", f.root);
    let sibling = format!("{}-other", f.root);

    assert!(is_inside(&inside, &f.root));
    assert!(!is_inside(&format!("{sibling}/Song.flac"), &f.root));
    assert!(!is_inside(&f.root, &f.root));
}

#[test]
fn shows_only_a_present_row_at_that_file_with_its_size() {
    let f = Fixture::new();
    let file = f.write_file("Massive Attack - Teardrop.flac", 4096);
    let row = |size: i64, missing: bool, path: &str| Candidate {
        id: "nd-1".into(),
        raw_path: Some(path.into()),
        library_path: None,
        size,
        title: "Teardrop".into(),
        artist: "Massive Attack".into(),
        album: "Mezzanine".into(),
        suffix: "flac".into(),
        duration: Some(330),
        source: PathSource::NativeApi,
        missing,
        album_artist: None,
    };
    let r = resolver();
    let path = "Massive Attack - Teardrop.flac";
    assert!(r.shows(Some(&row(4096, false, path)), &f.root, &file));
    assert!(!r.shows(Some(&row(4096, true, path)), &f.root, &file));
    assert!(!r.shows(Some(&row(1000, false, path)), &f.root, &file));
    assert!(!r.shows(Some(&row(4096, false, "Other.flac")), &f.root, &file));
    assert!(!r.shows(None, &f.root, &file));

    let json: Value =
        serde_json::from_str(r#"{"path":"a.flac","missing":true,"albumArtist":"Massive Attack"}"#)
            .expect("json");
    let parsed =
        NavidromeSongPathResolver::from_json(&json, "nd-1", PathSource::NativeApi, Some("libraryPath"))
            .expect("parsed")
            .expect("an object");
    assert_eq!(
        (parsed.missing, parsed.album_artist.as_deref()),
        (true, Some("Massive Attack"))
    );
}

#[test]
fn full_paths_resolve_dots_lexically() {
    assert_eq!(get_full_path("/a/b/../c/./d"), "/a/c/d");
    assert_eq!(get_full_path("/a//b/"), "/a/b/");
    assert_eq!(get_full_path("/../.."), "/");
    assert_eq!(extension("/x/Song.FLAC"), "FLAC");
    assert_eq!(extension("/x.d/Song"), "");
    assert_eq!(extension("/x/Song."), "");
}

/// A song's length is read whether Navidrome sends a fraction or whole seconds: Navidrome's
/// /api/song sends 301.37; Subsonic's getSong sends 301.
#[test]
fn a_songs_length_is_read_whether_navidrome_sends_a_fraction_or_whole_seconds() {
    for (body, seconds) in [
        (r#"{"path":"a.flac","duration":301.37}"#, Some(301)),
        (r#"{"path":"a.flac","duration":247.5}"#, Some(248)),
        (r#"{"path":"a.flac","duration":330}"#, Some(330)),
        (r#"{"path":"a.flac"}"#, None),
    ] {
        let json: Value = serde_json::from_str(body).expect("json");
        let parsed =
            NavidromeSongPathResolver::from_json(&json, "nd-1", PathSource::NativeApi, Some("libraryPath"))
                .expect("parsed")
                .expect("an object");
        assert_eq!(parsed.duration, seconds, "{body}");
    }
}
