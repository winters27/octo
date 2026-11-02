//! Tests for track paths, folder layouts, Windows drive path detection and the path and string
//! rules underneath them.

use super::*;

// ---- Track paths: build_track_path is what the Organized folder layout is built on, so an
// album lands as one folder on disk instead of a folder per track. ----

// Relative so the test reads the same on Windows and on the Linux CI runner.
const ROOT: &str = "musicroot";

#[test]
fn build_track_path_groups_by_album_and_prefixes_track_number() {
    let path = PathHelper::build_track_path(
        ROOT,
        "Boards Of Canada",
        "In A Beautiful Place Out In The Country",
        "Kid For Today",
        Some(1),
        ".mp3",
    );

    assert_eq!(
        path,
        "musicroot/Boards Of Canada/In A Beautiful Place Out In The Country/01 - Kid For Today.mp3"
    );
}

#[test]
fn build_track_path_pads_track_number_to_two_digits() {
    let path = PathHelper::build_track_path(ROOT, "A", "B", "C", Some(7), ".flac");
    assert!(path.ends_with("07 - C.flac"), "{path}");
}

#[test]
fn build_track_path_double_digit_track_is_not_padded_further() {
    let path = PathHelper::build_track_path(ROOT, "A", "B", "C", Some(12), ".flac");
    assert!(path.ends_with("12 - C.flac"), "{path}");
}

#[test]
fn build_track_path_no_track_number_omits_prefix() {
    // A standalone single has no position; it must not get a "00 - " prefix.
    let path = PathHelper::build_track_path(ROOT, "A", "B", "Song", None, ".mp3");
    assert!(path.ends_with("Song.mp3"), "{path}");
    assert!(!path.contains(" - Song.mp3"), "{path}");
}

#[test]
fn build_track_path_empty_extension_produces_no_trailing_dot() {
    // The yt-dlp shim appends .mp3 itself, so it is handed a path with no extension.
    let path = PathHelper::build_track_path(ROOT, "A", "B", "Song", Some(3), "");
    assert!(path.ends_with("03 - Song"), "{path}");
}

#[test]
fn build_track_path_sanitizes_path_hostile_names() {
    for nasty in ["AC/DC", "Sigur Rós: Ágætis", "What?<>|", "Nul\0Byte"] {
        let path = PathHelper::build_track_path(ROOT, nasty, "Album", "Title", Some(1), ".mp3");

        // Every character Windows or Linux refuses is gone, whatever the host.
        let segments: Vec<&str> = path.split('/').collect();

        // An artist containing a slash must not silently become an extra directory level.
        assert_eq!(segments.len(), 4, "{nasty}: {path}");
        assert_eq!(segments[0], ROOT, "{nasty}");
        assert_eq!(segments[2], "Album", "{nasty}");
        assert_eq!(segments[3], "01 - Title.mp3", "{nasty}");

        for c in INVALID_FILE_NAME_CHARS {
            assert!(
                !segments[1].contains(c),
                "{nasty}: artist segment kept illegal char {}",
                c as u32
            );
        }
    }
}

#[test]
fn build_track_path_accented_characters_survive() {
    // Sanitizing must not mangle legitimate non-ASCII names.
    let path = PathHelper::build_track_path(
        ROOT,
        "Sigur Rós",
        "Ágætis Byrjun",
        "Svefn-g-englar",
        Some(2),
        ".mp3",
    );

    assert!(path.contains("Sigur Rós"), "{path}");
    assert!(path.contains("Ágætis Byrjun"), "{path}");
    assert!(path.ends_with("02 - Svefn-g-englar.mp3"), "{path}");
}

#[test]
fn build_track_path_blank_album_or_artist_falls_back_to_unknown() {
    let path = PathHelper::build_track_path(ROOT, "", "   ", "Song", None, ".mp3");
    assert!(path.contains("Unknown"), "{path}");
}

#[test]
fn sanitize_folder_name_trims_trailing_dots() {
    // Windows silently drops trailing dots on folder names.
    assert_eq!(PathHelper::sanitize_folder_name("Album..."), "Album");
}

/// Every one of these names a different recording. Naming used to strip all brackets, so a
/// live take and the studio cut shared one path and the second download deleted the first.
#[test]
fn file_title_keeps_what_names_a_different_recording() {
    for (title, expected) in [
        ("Song (Live)", "Song (Live)"),
        ("Song [Remix]", "Song [Remix]"),
        ("Song (feat. Guest)", "Song (feat. Guest)"),
        ("Song (Acoustic Version)", "Song (Acoustic Version)"),
    ] {
        assert_eq!(PathHelper::file_title(title, "Artist"), expected, "{title}");
    }
}

#[test]
fn file_title_drops_only_upload_noise() {
    for (title, expected) in [
        ("Song (Official Video)", "Song"),
        ("Song (Official Music Video) [HD]", "Song"),
        ("Song [Official Audio]", "Song"),
        ("Song (Lyric Video)", "Song"),
        ("Song (Live) (Official Video)", "Song (Live)"),
    ] {
        assert_eq!(PathHelper::file_title(title, "Artist"), expected, "{title}");
    }
}

#[test]
fn file_title_drops_a_redundant_artist_prefix() {
    assert_eq!(
        PathHelper::file_title("Massive Attack - Teardrop", "Massive Attack"),
        "Teardrop"
    );
}

/// Mezzanine's "(Exchange)" is the whole title, not an annotation.
#[test]
fn file_title_title_that_is_only_a_bracket_is_kept() {
    for title in ["(Exchange)", "(Official Video)"] {
        assert_eq!(PathHelper::file_title(title, "Massive Attack"), title);
    }
}

// ---- Folder layouts ----

const LAYOUT_ROOT: &str = "/music";

#[test]
fn flat_puts_everything_in_one_directory() {
    let path = PathHelper::build_layout_path(
        FolderStructure::Flat,
        LAYOUT_ROOT,
        "Daft Punk",
        "Discovery",
        "Digital Love",
        Some(3),
        ".flac",
    );

    assert_eq!(path, "/music/Daft Punk - Digital Love.flac");
}

#[test]
fn by_artist_uses_an_artist_folder_and_no_album_folder() {
    // The point of the layout: a library built a track at a time gets one folder per
    // artist instead of one folder per single, or one directory of thousands of files.
    let path = PathHelper::build_layout_path(
        FolderStructure::ByArtist,
        LAYOUT_ROOT,
        "Daft Punk",
        "Discovery",
        "Digital Love",
        Some(3),
        ".flac",
    );

    assert_eq!(path, "/music/Daft Punk/Digital Love.flac");
}

#[test]
fn organized_keeps_the_album_folder_and_track_number() {
    let path = PathHelper::build_layout_path(
        FolderStructure::Organized,
        LAYOUT_ROOT,
        "Daft Punk",
        "Discovery",
        "Digital Love",
        Some(3),
        ".flac",
    );

    assert_eq!(path, "/music/Daft Punk/Discovery/03 - Digital Love.flac");
}

#[test]
fn organized_no_album_falls_back_to_the_title_as_the_folder() {
    // Reproduces the pre-album shape for a genuine single, and the rule lives in the
    // resolver so every caller gets it rather than each one remembering.
    let path = PathHelper::build_layout_path(
        FolderStructure::Organized,
        LAYOUT_ROOT,
        "Daft Punk",
        "",
        "Digital Love",
        None,
        ".flac",
    );

    assert_eq!(path, "/music/Daft Punk/Digital Love/Digital Love.flac");
}

#[test]
fn by_artist_no_album_is_unaffected() {
    let path = PathHelper::build_layout_path(
        FolderStructure::ByArtist,
        LAYOUT_ROOT,
        "Daft Punk",
        "",
        "Digital Love",
        None,
        ".flac",
    );

    assert_eq!(path, "/music/Daft Punk/Digital Love.flac");
}

#[test]
fn empty_extension_is_honored() {
    // The YouTube path passes no extension because the shim appends .mp3 itself.
    let path = PathHelper::build_layout_path(
        FolderStructure::ByArtist,
        LAYOUT_ROOT,
        "Daft Punk",
        "Discovery",
        "Digital Love",
        Some(3),
        "",
    );

    assert_eq!(path, "/music/Daft Punk/Digital Love");
}

/// No layout is silently picked for another: `build_layout_path`'s match has no default arm, so
/// a new layout fails to compile there. What is left to check is that every layout resolves to
/// its own shape.
#[test]
fn unknown_layout_throws_rather_than_silently_picking_one() {
    let shapes: Vec<String> = [
        FolderStructure::Organized,
        FolderStructure::Flat,
        FolderStructure::ByArtist,
    ]
    .into_iter()
    .map(|layout| PathHelper::build_layout_path(layout, LAYOUT_ROOT, "A", "B", "C", Some(1), ".flac"))
    .collect();
    assert_eq!(
        shapes,
        ["/music/A/B/01 - C.flac", "/music/A - C.flac", "/music/A/C.flac"]
    );
}

// ---- Windows drive path detection: a Windows drive-letter
// path configured inside a Linux container is silently created as a literal directory name; the
// detector behind the startup warning must catch that shape and nothing else. ----

#[test]
fn drive_paths_are_detected() {
    for path in [r"E:\Media\Music", "E:/Media/Music", r"c:\music"] {
        assert!(PathHelper::looks_like_windows_drive_path(Some(path)), "{path}");
    }
}

#[test]
fn non_drive_paths_are_not() {
    for path in [
        Some("/music"),
        Some("./downloads"),
        Some("music"),
        Some("E:"),
        Some(""),
        None,
    ] {
        assert!(!PathHelper::looks_like_windows_drive_path(path), "{path:?}");
    }
}

// ---- Path and string details: UTF-16 cuts, track numbers, unique and cache paths, joins ----

#[test]
fn sanitize_cuts_at_one_hundred_utf16_units_then_trims() {
    let long = "a".repeat(99) + " b";
    assert_eq!(PathHelper::sanitize_file_name(&long), "a".repeat(99));
    // An astral character straddling the cut is left out whole.
    let astral = "a".repeat(99) + "\u{1F600}";
    assert_eq!(PathHelper::sanitize_file_name(&astral), "a".repeat(99));
    let dots = "a".repeat(98) + "..x";
    assert_eq!(PathHelper::sanitize_folder_name(&dots), "a".repeat(98));
    assert_eq!(PathHelper::sanitize_folder_name(" ... "), "Unknown");
    assert_eq!(PathHelper::sanitize_file_name("  "), "Unknown");
    assert_eq!(PathHelper::sanitize_file_name(" A/B "), "A_B");
}

#[test]
fn negative_track_numbers_keep_two_digits_after_the_sign() {
    assert!(PathHelper::build_track_path("r", "A", "B", "C", Some(-1), ".mp3").ends_with("/-01 - C.mp3"));
}

#[test]
fn resolve_unique_path_counts_up_past_existing_files() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let base = format!("{}/Song.flac", dir.path().display());
    assert_eq!(PathHelper::resolve_unique_path(&base), base);

    std::fs::write(&base, b"x").expect("write");
    let first = format!("{}/Song (1).flac", dir.path().display());
    assert_eq!(PathHelper::resolve_unique_path(&base), first);

    std::fs::write(&first, b"x").expect("write");
    assert_eq!(
        PathHelper::resolve_unique_path(&base),
        format!("{}/Song (2).flac", dir.path().display())
    );

    let bare = format!("{}/README", dir.path().display());
    std::fs::write(&bare, b"x").expect("write");
    assert_eq!(
        PathHelper::resolve_unique_path(&bare),
        format!("{}/README (1)", dir.path().display())
    );
}

#[test]
fn cache_path_is_under_the_temp_directory() {
    assert!(PathHelper::get_cache_path().ends_with("/octo-cache"));
}

#[test]
fn combine_follows_path_combine() {
    assert_eq!(combine("a/", "b"), "a/b");
    assert_eq!(combine("", "b"), "b");
    assert_eq!(combine("a", "/b"), "/b");
    assert_eq!(combine("a", ""), "a");
    assert_eq!(extension_of("/x/a.b.flac"), ".flac");
    assert_eq!(extension_of("/x.d/a"), "");
    assert_eq!(extension_of("a."), "");
    assert_eq!(file_name_without_extension("/x/a.b.flac"), "a.b");
    assert_eq!(directory_name("/a.flac"), "/");
    assert_eq!(directory_name("a.flac"), "");
}

#[test]
fn a_path_follows_the_folder_it_was_in() {
    let (from, to) = ("/music/Beyonce/A", "/music/Beyonc\u{e9}/A");
    assert_eq!(
        PathHelper::moved_path("/music/Beyonce/A/01 - S.flac", from, to).as_deref(),
        Some("/music/Beyonc\u{e9}/A/01 - S.flac")
    );
    assert_eq!(PathHelper::moved_path(from, from, to).as_deref(), Some(to));
    assert_eq!(
        PathHelper::moved_path("/music/Beyonce/AB/01.flac", from, to),
        None
    );
    assert_eq!(PathHelper::moved_path("/music/Other/01.flac", from, to), None);
}

// ---- Names that work on Windows, over SMB, on ext4 and in Navidrome (#135) ----

#[test]
fn windows_characters_become_their_look_alikes() {
    assert_eq!(
        PathHelper::sanitize_file_name("Are You Ready?"),
        "Are You Ready\u{FF1F}"
    );
    assert_eq!(
        PathHelper::sanitize_file_name("LOVE: FEAT. \"ZACARI\" <*|>"),
        "LOVE\u{FF1A} FEAT. \u{FF02}ZACARI\u{FF02} \u{FF1C}\u{FF0A}\u{FF5C}\u{FF1E}"
    );
    // Path separators stay underscores, as AC/DC always was, so its folder keeps its name.
    assert_eq!(PathHelper::sanitize_folder_name("AC/DC"), "AC_DC");
    assert_eq!(PathHelper::sanitize_folder_name("Back\\Slash"), "Back_Slash");
    // Accents, umlauts and other scripts are kept.
    assert_eq!(
        PathHelper::sanitize_folder_name("Björk Guðmundsdóttir & Ärzte 日本"),
        "Björk Guðmundsdóttir & Ärzte 日本"
    );
}

#[test]
fn control_characters_become_underscores() {
    assert_eq!(PathHelper::sanitize_file_name("Tab\there\u{1}"), "Tab_here_");
}

#[test]
fn names_are_written_in_nfc() {
    // Sigur Ro and a combining acute: one name to a person, two names to a disk without NFC.
    assert_eq!(
        PathHelper::sanitize_folder_name("Sigur Ro\u{301}s"),
        "Sigur R\u{f3}s"
    );
}

#[test]
fn folders_lose_trailing_dots_and_spaces_and_files_keep_a_trailing_dot() {
    assert_eq!(PathHelper::sanitize_folder_name("Album . "), "Album");
    let path = PathHelper::build_layout_path(
        FolderStructure::ByArtist,
        LAYOUT_ROOT,
        "R.E.M.",
        "",
        "Why Not.",
        None,
        ".flac",
    );
    assert_eq!(path, "/music/R.E.M/Why Not..flac");
}

#[test]
fn a_capped_folder_never_ends_in_a_dot_and_a_bare_file_loses_one() {
    // 249 bytes of CJK, then "abVol. 2": the 255-byte cap lands right after "Vol.".
    let album = format!("{}abVol. 2", "日".repeat(83));
    let folder = PathHelper::safe_name(&album, None);
    assert!(folder.len() <= 255 && !folder.ends_with(['.', ' ']), "{folder}");
    assert_eq!(PathHelper::safe_name("Why Not.", Some("")), "Why Not");
    assert_eq!(PathHelper::safe_name("Why Not.", Some(".flac")), "Why Not.");
}

#[test]
fn flat_track_artist_loses_trailing_dots() {
    let path = PathHelper::build_layout_path(
        FolderStructure::Flat,
        LAYOUT_ROOT,
        "Mr.",
        "",
        "Title",
        None,
        ".flac",
    );
    assert_eq!(path, "/music/Mr - Title.flac");
}

#[test]
fn reserved_windows_names_get_an_underscore() {
    for (title, file) in [
        ("Con", "Con_.flac"),
        ("aux", "aux_.flac"),
        ("Nul.Point", "Nul_.Point.flac"),
        ("nul.tar.gz", "nul_.tar.gz.flac"),
        ("CON .x", "CON_ .x.flac"),
        ("COM0", "COM0_.flac"),
        ("COM1", "COM1_.flac"),
        ("LPT0", "LPT0_.flac"),
        ("lpt\u{b9}", "lpt\u{b9}_.flac"),
        ("com\u{b2}", "com\u{b2}_.flac"),
        ("Console", "Console.flac"),
        ("Conan", "Conan.flac"),
    ] {
        let path = PathHelper::build_layout_path(
            FolderStructure::ByArtist,
            LAYOUT_ROOT,
            "A",
            "",
            title,
            None,
            ".flac",
        );
        assert_eq!(path, format!("/music/A/{file}"), "{title}");
    }
    let folder = PathHelper::build_layout_path(
        FolderStructure::Organized,
        LAYOUT_ROOT,
        "Prn",
        "Aux",
        "T",
        None,
        ".flac",
    );
    assert_eq!(folder, "/music/Prn_/Aux_/T.flac");
    // Folders Navidrome never scans, in any capitals.
    for (album, folder) in [
        ("Lost+Found", "Lost+Found_"),
        ("@eaDir", "@eaDir_"),
        ("$RECYCLE.BIN", "$RECYCLE_.BIN"),
        ("#snapshot", "#snapshot_"),
        ("Lost and Found", "Lost and Found"),
    ] {
        let path = PathHelper::build_layout_path(
            FolderStructure::Organized,
            LAYOUT_ROOT,
            "A",
            album,
            "T",
            None,
            ".flac",
        );
        assert_eq!(path, format!("/music/A/{folder}/T.flac"), "{album}");
    }
}

#[test]
fn a_leading_dot_is_replaced() {
    // Navidrome hides a name with one leading dot, and Samba marks every leading-dot name hidden.
    let hidden = PathHelper::build_layout_path(
        FolderStructure::ByArtist,
        LAYOUT_ROOT,
        ".38 Special",
        "",
        ".Hold On Loosely",
        None,
        ".flac",
    );
    assert_eq!(hidden, "/music/\u{FF0E}38 Special/\u{FF0E}Hold On Loosely.flac");
    let dots = PathHelper::build_layout_path(
        FolderStructure::Organized,
        LAYOUT_ROOT,
        "...And You Will Know Us by the Trail of Dead",
        "...And Justice for All",
        "One",
        Some(4),
        ".flac",
    );
    assert_eq!(
        dots,
        "/music/\u{FF0E}..And You Will Know Us by the Trail of Dead/\u{FF0E}..And Justice for All/04 - One.flac"
    );
    // The readers keep Navidrome's own rule: one dot, not two.
    assert!(PathHelper::is_hidden_name(".octo-incoming"));
    assert!(!PathHelper::is_hidden_name("...And Justice for All"));
    assert!(!PathHelper::is_hidden_name("."));
    assert!(!PathHelper::is_hidden_name("Album"));
}

#[test]
fn a_name_fits_255_bytes_with_its_extension() {
    let title = "日".repeat(100);
    let path = PathHelper::build_layout_path(
        FolderStructure::Organized,
        LAYOUT_ROOT,
        "A",
        "B",
        &title,
        Some(1),
        ".flac",
    );
    let file = path.rsplit('/').next().expect("a file name");
    assert!(file.len() <= 255, "{}", file.len());
    assert!(file.ends_with(".flac"));
    assert!(file.starts_with("01 - 日"));
}

#[test]
fn same_name_ignores_capitals_and_old_spellings() {
    assert!(PathHelper::same_name(
        "Are You Ready?",
        "are you ready\u{FF1F}",
        false
    ));
    assert!(PathHelper::same_name(
        "03 - Are You Ready?.mp3",
        "03 - are you ready\u{FF1F}.MP3",
        true
    ));
    assert!(PathHelper::same_name("Daft Punk", "daft punk", false));
    assert!(PathHelper::same_name("AC_DC", "AC_DC", false));
    assert!(PathHelper::same_name("Sigur Ro\u{301}s", "Sigur R\u{f3}s", false));
    assert!(PathHelper::same_name("...Albums", "\u{FF0E}..albums", false));
    assert!(!PathHelper::same_name("Who Are You?", "Who Are You", false));
    assert!(!PathHelper::same_name("Song.flac", "Song.mp3", true));
}

/// NTFS and Samba compare names by each character's simple upper case.
#[test]
fn fold_name_matches_smb() {
    assert_eq!(fold_name("\u{17F}ong", None), fold_name("Song", None));
    assert_eq!(fold_name("D\u{131}sco", None), fold_name("DISCO", None));
    // A character whose upper case is two characters stays as it is.
    assert_ne!(fold_name("Stra\u{DF}e", None), fold_name("STRASSE", None));
}

fn dir_text(dir: &tempfile::TempDir) -> String {
    dir.path().to_string_lossy().replace('\\', "/")
}

/// Rendered names as `Template::render` gives them, each uncut.
fn rendered(parts: &[&str]) -> Vec<RenderedName> {
    parts
        .iter()
        .enumerate()
        .map(|(segment, part)| RenderedName {
            name: part.to_string(),
            uncut: part.to_string(),
            segment,
            cap: 91,
            ..RenderedName::default()
        })
        .collect()
}

fn settled(root: &str, parts: &[&str], extension: &str) -> Settled {
    PathHelper::settle(
        root,
        &rendered(parts),
        extension,
        PATH_BUDGET,
        SettleHooks::default(),
    )
    .expect("settled")
}

/// Linux only, where Octo runs: Windows cannot make a folder named `Are You Ready?`.
#[cfg(unix)]
#[test]
fn settle_joins_a_folder_spelled_the_old_way() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = dir_text(&dir);
    std::fs::create_dir_all(format!("{root}/Example Artist/Are You Ready?")).expect("made");
    let settled = settled(
        &root,
        &["Example Artist", "Are You Ready\u{FF1F}", "03 - Song"],
        ".mp3",
    );
    assert_eq!(
        settled.path,
        format!("{root}/Example Artist/Are You Ready?/03 - Song.mp3")
    );
    assert!(settled.folder_existed);
    assert_eq!(settled.taken, None);
}

/// A real name always comes from the listing, so a disk that ignores capitals never records the
/// request's capitals for a folder spelled otherwise.
#[test]
fn settle_takes_real_names_from_the_listing_and_never_a_hidden_one() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = dir_text(&dir);
    std::fs::create_dir_all(format!("{root}/daft punk")).expect("made");
    std::fs::create_dir_all(format!("{root}/.38 Special")).expect("made");
    let joined = settled(&root, &["Daft Punk", "Discovery", "01 - One More Time"], ".flac");
    assert_eq!(
        joined.path,
        format!("{root}/daft punk/Discovery/01 - One More Time.flac")
    );
    assert!(!joined.folder_existed);
    let visible = settled(&root, &["\u{FF0E}38 Special", "Hold On Loosely"], ".flac");
    assert_eq!(
        visible.path,
        format!("{root}/\u{FF0E}38 Special/Hold On Loosely.flac")
    );
}

/// A folder an older cap cut (here Organized's 91 units, now rendered under a three-folder
/// template's 60) is the same folder, and the album folder comes from the album search.
#[test]
fn settle_joins_a_folder_an_older_cap_cut_and_asks_the_album_hook() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = dir_text(&dir);
    let long = "Wiener Philharmoniker and the Singverein der Gesellschaft der Musikfreunde in Wien und Chor der Staatsoper";
    let older = super::cut_name(long, 91, None);
    assert!(
        older.chars().count() <= 91 && older.chars().count() < long.chars().count(),
        "{older}"
    );
    std::fs::create_dir_all(format!("{root}/{older}/Discovery (Remastered)")).expect("made");
    let names = vec![
        RenderedName {
            name: super::cut_name(long, 60, None),
            uncut: long.to_string(),
            segment: 0,
            cap: 60,
            ..RenderedName::default()
        },
        RenderedName {
            name: "Discovery".to_string(),
            uncut: "Discovery".to_string(),
            segment: 1,
            cap: 60,
            ..RenderedName::default()
        },
        RenderedName {
            name: "01 - One More Time".to_string(),
            uncut: "01 - One More Time".to_string(),
            segment: 2,
            ..RenderedName::default()
        },
    ];
    let find = |parent: &str| {
        assert!(parent.ends_with(&older), "{parent}");
        Some("Discovery (Remastered)".to_string())
    };
    let settled = PathHelper::settle(
        &root,
        &names,
        ".flac",
        PATH_BUDGET,
        SettleHooks {
            album: Some(AlbumHook { at: 1, find: &find }),
            ..SettleHooks::default()
        },
    )
    .expect("settled");
    assert_eq!(
        settled.path,
        format!("{root}/{older}/Discovery (Remastered)/01 - One More Time.flac")
    );
    assert!(settled.folder_existed);
    assert_eq!(settled.existing, 2);
}

const LONG_ARTIST: &str = "Orchestra and Chorus of the Royal Opera House Covent Garden with Soloists of the Vienna State Opera and Friends";

/// An artist folder cut under a Custom template's cap (albums or singles, in the music folder or
/// the family pool) is the same folder once the settings switch to a built-in layout, given the
/// caps the settings remember, so no second artist folder is made. Without them it is another
/// folder: the caps come from the settings alone.
#[test]
fn a_folder_a_saved_template_cut_is_known_after_a_switch() {
    use crate::common::layout::{LayoutKind, Template};
    use crate::settings::subsonic::SubsonicSettings;

    let albums = "{Artist}/{Album}/{Disc:00}-{Track:00} - {Track Artist} - {Title}";
    let singles = "{Artist}/Singles/{Track Artist} - {Title} ({Year})";
    let saved: Vec<Template> = [albums, singles]
        .into_iter()
        .map(|text| Template::parse(text).expect("the saved template parses"))
        .collect();
    let switched = SubsonicSettings {
        folder_structure: FolderStructure::Organized,
        ..SubsonicSettings::default()
    };
    let remembering = SubsonicSettings {
        layout_caps: saved.iter().flat_map(Template::own_caps).collect(),
        ..switched.clone()
    };
    let song = LayoutValues {
        artist: LONG_ARTIST.to_string(),
        track_artist: LONG_ARTIST.to_string(),
        album: "Tosca".to_string(),
        title: "Vissi d'arte".to_string(),
        track: Some(1),
        ..LayoutValues::default()
    };
    for template in &saved {
        for budget in [PATH_BUDGET, POOL_BUDGET] {
            let cap = template.folder_cap(budget);
            let older = super::cut_name(LONG_ARTIST, cap, None);
            assert!(
                super::built_in_caps().all(|built_in| super::cut_name(LONG_ARTIST, built_in, None) != older),
                "{cap} cuts the name as a built-in cap does: {older}"
            );
            let dir = tempfile::tempdir().expect("a temp dir");
            let root = dir_text(&dir);
            std::fs::create_dir_all(format!("{root}/{older}/Tosca")).expect("made");
            let settle = |settings: &SubsonicSettings| {
                let names = Layout::from_settings(settings)
                    .template(LayoutKind::Album)
                    .render(&song, ".flac", budget);
                assert_ne!(names[0].name, older);
                PathHelper::settle(&root, &names, ".flac", budget, SettleHooks::default()).expect("settled")
            };
            let joined = settle(&remembering);
            assert_eq!(joined.folders[0], older, "cap {cap} in a budget of {budget}");
            assert_eq!(joined.existing, 2);
            let apart = settle(&switched);
            assert_ne!(apart.folders[0], older, "cap {cap} in a budget of {budget}");
            assert_eq!(apart.existing, 0);
        }
    }
}

/// The caps a rendered name carries are that settle's alone: one call that knows an older cap
/// joins the folder it cut, and a call after it, for the same name without that cap, does not.
#[test]
fn a_cap_one_settle_carries_never_reaches_another() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = dir_text(&dir);
    let older = super::cut_name(LONG_ARTIST, 83, None);
    std::fs::create_dir_all(format!("{root}/{older}")).expect("made");
    let names = |cut_caps: Vec<usize>| {
        vec![
            RenderedName {
                name: super::cut_name(LONG_ARTIST, 91, None),
                uncut: LONG_ARTIST.to_string(),
                segment: 0,
                cap: 91,
                cut_caps,
                ..RenderedName::default()
            },
            RenderedName {
                name: "Vissi d'arte".to_string(),
                uncut: "Vissi d'arte".to_string(),
                segment: 1,
                ..RenderedName::default()
            },
        ]
    };
    let settle = |names: Vec<RenderedName>| {
        PathHelper::settle(&root, &names, ".flac", PATH_BUDGET, SettleHooks::default())
            .expect("settled")
            .folders
    };
    assert_eq!(settle(names(vec![83])), [older.clone()]);
    assert_ne!(settle(names(Vec::new())), [older.clone()]);
    assert_eq!(settle(names(vec![83])), [older]);
}

/// A shorter name that only starts the same is another folder: `Vol. 1` is not `Vol. 12`; and at
/// the album folder no shorter name is ever the album (the album search decides), so a studio
/// album is not its live one.
#[test]
fn a_prefix_joins_only_a_real_cut() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = dir_text(&dir);
    std::fs::create_dir_all(format!(
        "{root}/The Best of Nina Simone, The Colpix Years, Vol. 1"
    ))
    .expect("made");
    std::fs::create_dir_all(format!("{root}/Bach/Symphony No. 9 in D minor, Op. 125 Choral")).expect("made");
    let twelve = settled(
        &root,
        &["The Best of Nina Simone, The Colpix Years, Vol. 12", "T"],
        ".flac",
    );
    assert_eq!(
        twelve.path,
        format!("{root}/The Best of Nina Simone, The Colpix Years, Vol. 12/T.flac")
    );
    let none = |_: &str| None;
    let live = PathHelper::settle(
        &root,
        &rendered(&["Bach", "Symphony No. 9 in D minor, Op. 125 Choral (Live)", "T"]),
        ".flac",
        PATH_BUDGET,
        SettleHooks {
            album: Some(AlbumHook { at: 1, find: &none }),
            ..SettleHooks::default()
        },
    )
    .expect("settled");
    assert_eq!(
        live.path,
        format!("{root}/Bach/Symphony No. 9 in D minor, Op. 125 Choral (Live)/T.flac")
    );
}

/// At a folder named by an artist alone, another spelling of the name is the same folder.
#[test]
fn an_artist_folder_joins_another_spelling() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = dir_text(&dir);
    std::fs::create_dir_all(format!("{root}/Beyonce/Lemonade")).expect("made");
    let mut names = rendered(&["Beyonc\u{e9}", "Lemonade", "01 - Pray You Catch Me"]);
    names[0].artist_level = true;
    let joined =
        PathHelper::settle(&root, &names, ".flac", PATH_BUDGET, SettleHooks::default()).expect("settled");
    assert_eq!(
        joined.path,
        format!("{root}/Beyonce/Lemonade/01 - Pray You Catch Me.flac")
    );
    // An album folder never joins by spelling alone.
    std::fs::create_dir_all(format!("{root}/Beyonce/4")).expect("made");
    let names = rendered(&["Beyonce", "4!", "01 - 1+1"]);
    let album =
        PathHelper::settle(&root, &names, ".flac", PATH_BUDGET, SettleHooks::default()).expect("settled");
    assert_eq!(album.path, format!("{root}/Beyonce/4!/01 - 1+1.flac"));
}

#[test]
fn settle_never_guesses_when_a_folder_cannot_be_read() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = dir_text(&dir);
    // A file where the root should be: is_dir says no, so nothing is listed and nothing is made.
    let file_root = format!("{root}/not-a-folder");
    std::fs::write(&file_root, b"x").expect("written");
    let answer = PathHelper::settle(
        &file_root,
        &rendered(&["A", "T"]),
        ".flac",
        PATH_BUDGET,
        SettleHooks::default(),
    )
    .expect("settled");
    assert!(!answer.folder_existed);
    // A listing that fails twice is the answer.
    assert!(super::listing(&format!("{root}/missing")).is_err());
}

/// Linux only, where Octo runs: Windows cannot make a file named `Are You Ready?.mp3`.
#[cfg(unix)]
#[test]
fn settle_finds_a_file_spelled_the_old_way_or_in_other_capitals() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = dir_text(&dir);
    std::fs::create_dir_all(format!("{root}/A/B")).expect("made");
    std::fs::write(format!("{root}/A/B/03 - Are You Ready?.mp3"), b"x").expect("written");
    std::fs::write(format!("{root}/A/B/04 - song.mp3"), b"x").expect("written");
    std::fs::write(format!("{root}/A/B/.05 - Hidden.mp3"), b"x").expect("written");
    let old = settled(&root, &["A", "B", "03 - Are You Ready\u{FF1F}"], ".mp3");
    assert_eq!(old.taken, Some(format!("{root}/A/B/03 - Are You Ready?.mp3")));
    let capitals = settled(&root, &["A", "B", "04 - Song"], ".MP3");
    assert_eq!(capitals.taken, Some(format!("{root}/A/B/04 - song.mp3")));
    let hidden = settled(&root, &["A", "B", "\u{FF0E}05 - Hidden"], ".mp3");
    assert_eq!(hidden.taken, None);
}

#[test]
fn resolve_unique_path_counts_past_a_name_in_other_capitals() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = dir_text(&dir);
    std::fs::write(format!("{root}/song.flac"), b"x").expect("written");
    std::fs::write(format!("{root}/SONG (1).flac"), b"x").expect("written");
    assert_eq!(
        PathHelper::resolve_unique_path(&format!("{root}/Song.flac")),
        format!("{root}/Song (2).flac")
    );
}

#[test]
fn resolve_unique_path_skips_an_equivalent_numbered_name() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = dir_text(&dir);
    std::fs::write(format!("{root}/Song.flac"), b"x").expect("written");
    std::fs::write(format!("{root}/song (1).flac"), b"x").expect("written");
    assert_eq!(
        PathHelper::resolve_unique_path(&format!("{root}/Song.flac")),
        format!("{root}/Song (2).flac")
    );
}

#[test]
fn a_long_path_shrinks_only_the_file_name() {
    let folders = vec!["a".repeat(91), "b".repeat(91)];
    let mut stem = format!("01 - {}", "c".repeat(95));
    PathHelper::fit_stem(&folders, &mut stem, ".flac", PATH_BUDGET);
    assert!(91 + 91 + 2 + stem.chars().count() + 5 <= PATH_BUDGET);
    assert_eq!(stem.chars().count(), 41);

    // Folders already over the budget: the file name gives way to its floor, no further.
    let mut stem = "c".repeat(100);
    PathHelper::fit_stem(
        &["a".repeat(100), "b".repeat(100)],
        &mut stem,
        ".flac",
        PATH_BUDGET,
    );
    assert_eq!(stem.chars().count(), 40);

    // A cut lands on a word break when one is near.
    let mut stem = format!("{} tail words here", "x".repeat(210));
    PathHelper::fit_stem(&["Artist".to_string()], &mut stem, ".flac", PATH_BUDGET);
    assert!(!stem.ends_with(' '));
    assert!(6 + 1 + stem.chars().count() + 5 <= PATH_BUDGET);
}
