//! Templates: the built-in layouts give the paths the old fixed builder gave, empty names leave
//! no stray text, every template rule has its words, and a song's kind picks its template.

use super::*;
use crate::common::dotnet::utf16_len;
use crate::common::dotnet_random::DotnetRandom;
use crate::common::path_helper::PATH_BUDGET;

fn values(artist: &str, album: &str, title: &str, track: Option<i32>) -> LayoutValues {
    LayoutValues {
        artist: artist.to_string(),
        track_artist: artist.to_string(),
        album: album.to_string(),
        title: title.to_string(),
        track,
        ..LayoutValues::default()
    }
}

fn names(template: &Template, values: &LayoutValues) -> Vec<String> {
    template
        .render(values, ".flac", PATH_BUDGET)
        .into_iter()
        .map(|rendered| rendered.name)
        .collect()
}

fn render(template: &str, values: &LayoutValues) -> String {
    names(&Template::parse(template).expect("the template parses"), values).join("/")
}

fn refused(template: &str) -> String {
    Template::parse(template).expect_err("the template is refused").0
}

// ---- The built-in layouts are the old builder ----------------------------------------------

/// The builder at 475ec631, copied word for word apart from `Self::` and the constants' names.
mod oracle {
    use crate::common::dotnet::{is_blank, utf16_len};
    use crate::settings::subsonic::FolderStructure;

    const INVALID_FILE_NAME_CHARS: [char; 2] = ['\0', '/'];
    const INVALID_FOLDER_NAME_CHARS: [char; 2] = ['\0', '/'];

    pub fn build_layout_path(
        structure: FolderStructure,
        download_path: &str,
        artist: &str,
        album: &str,
        title: &str,
        track_number: Option<i32>,
        extension: &str,
    ) -> String {
        let safe_artist = sanitize_folder_name(artist);
        let safe_title = sanitize_file_name(title);
        let effective_album = if is_blank(album) { title } else { album };
        match structure {
            FolderStructure::Flat => {
                combine(download_path, &format!("{safe_artist} - {safe_title}{extension}"))
            }
            FolderStructure::ByArtist => combine(
                &combine(download_path, &safe_artist),
                &format!("{safe_title}{extension}"),
            ),
            _ => build_track_path(
                download_path,
                artist,
                effective_album,
                title,
                track_number,
                extension,
            ),
        }
    }

    fn build_track_path(
        download_path: &str,
        artist: &str,
        album: &str,
        title: &str,
        track_number: Option<i32>,
        extension: &str,
    ) -> String {
        let safe_artist = sanitize_folder_name(artist);
        let safe_album = sanitize_folder_name(album);
        let safe_title = sanitize_file_name(title);
        let artist_folder = combine(download_path, &safe_artist);
        let album_folder = combine(&artist_folder, &safe_album);
        let track_prefix = match track_number {
            Some(n) if n < 0 => format!("-{:02} - ", n.unsigned_abs()),
            Some(n) => format!("{n:02} - "),
            None => String::new(),
        };
        let file_name = format!("{track_prefix}{safe_title}{extension}");
        combine(&album_folder, &file_name)
    }

    pub fn sanitize_file_name(file_name: &str) -> String {
        if is_blank(file_name) {
            return "Unknown".to_string();
        }
        let mut sanitized: String = file_name
            .chars()
            .map(|c| {
                if INVALID_FILE_NAME_CHARS.contains(&c) {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        if utf16_len(&sanitized) > 100 {
            sanitized = truncate_utf16(&sanitized, 100);
        }
        sanitized.trim().to_string()
    }

    pub fn sanitize_folder_name(folder_name: &str) -> String {
        if is_blank(folder_name) {
            return "Unknown".to_string();
        }
        let sanitized: String = folder_name
            .chars()
            .map(|c| {
                if INVALID_FOLDER_NAME_CHARS.contains(&c) {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let mut sanitized = sanitized.trim().trim_end_matches('.').to_string();
        if utf16_len(&sanitized) > 100 {
            sanitized = truncate_utf16(&sanitized, 100).trim_end_matches('.').to_string();
        }
        if is_blank(&sanitized) {
            return "Unknown".to_string();
        }
        sanitized
    }

    fn combine(a: &str, b: &str) -> String {
        if b.is_empty() {
            return a.to_string();
        }
        if a.is_empty() || b.starts_with('/') {
            return b.to_string();
        }
        if a.ends_with('/') {
            format!("{a}{b}")
        } else {
            format!("{a}/{b}")
        }
    }

    fn truncate_utf16(s: &str, units: usize) -> String {
        let mut taken = 0;
        s.chars()
            .take_while(|c| {
                taken += c.len_utf16();
                taken <= units
            })
            .collect()
    }
}

/// Pieces an ordinary name is drawn from: letters with accents (NFC), other scripts, digits, and
/// every punctuation mark that was already safe on Windows, with dots and dashes in the middle.
const ORDINARY: &[&str] = &[
    "a",
    "b",
    "e",
    "k",
    "n",
    "o",
    "s",
    "t",
    "A",
    "D",
    "M",
    "T",
    "Z",
    "é",
    "ö",
    "ñ",
    "ß",
    "Å",
    "ø",
    "1",
    "7",
    "0",
    " ",
    " ",
    " ",
    "'",
    "&",
    "!",
    ",",
    "(",
    ")",
    "[",
    "]",
    "-",
    ".",
    "+",
    "#",
    "@",
    "$",
    "%",
    "=",
    "~",
    "^",
    "{",
    "}",
    ";",
    "`",
    "日",
    "本",
    "ж",
    "/",
    "\u{1F600}",
];

/// A value from the pool: mostly short, sometimes blank or only spaces, sometimes starting with
/// "- ", sometimes long enough to meet the 100-unit cut with an emoji across it.
fn ordinary(random: &mut DotnetRandom) -> String {
    match random.next(20) {
        0 => String::new(),
        1 => "   ".to_string(),
        2 => format!("- {}", piece(random, 30)),
        3 => format!("{}\u{1F600}x", "a".repeat(98 + random.next(3) as usize)),
        _ => piece(random, 40),
    }
}

fn piece(random: &mut DotnetRandom, longest: i32) -> String {
    let length = random.next(longest + 1);
    (0..length)
        .map(|_| ORDINARY[random.next(ORDINARY.len() as i32) as usize])
        .collect()
}

/// What the old builder wrote that the new one changes on purpose, each with its own test in
/// `path_helper_tests`: a name with a leading dot, a reserved Windows name, a name over 255 bytes,
/// a folder name over the template's cap, an artist or album that kept a trailing space.
fn changed_on_purpose(
    structure: FolderStructure,
    path: &str,
    artist: &str,
    album: &str,
    title: &str,
) -> bool {
    let album = if album.trim().is_empty() { title } else { album };
    let cap = Layout::preset(structure)
        .template(LayoutKind::Album)
        .folder_cap(PATH_BUDGET);
    let names: Vec<&str> = path.split('/').skip(2).collect();
    let folders = &names[..names.len() - 1];
    oracle::sanitize_folder_name(artist).ends_with(' ')
        || oracle::sanitize_folder_name(album).ends_with(' ')
        || folders.iter().any(|folder| utf16_len(folder) > cap)
        || names.iter().any(|name| {
            let stem = name.split('.').next().unwrap_or("").trim_end().to_uppercase();
            name.starts_with('.') || ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str()) || name.len() > 255
        })
}

fn track(random: &mut DotnetRandom) -> Option<i32> {
    match random.next(6) {
        0 => None,
        1 => Some(0),
        2 => Some(-(random.next(12) + 1)),
        3 => Some(100 + random.next(900)),
        _ => Some(random.next(30)),
    }
}

const STRUCTURES: [FolderStructure; 3] = [
    FolderStructure::Flat,
    FolderStructure::ByArtist,
    FolderStructure::Organized,
];

#[test]
fn presets_match_the_old_builder() {
    let mut random = DotnetRandom::new(135);
    let mut compared = [0usize; 3];
    for _ in 0..20_000 {
        let (artist, title) = (ordinary(&mut random), ordinary(&mut random));
        let album = if random.next(5) == 0 {
            String::new()
        } else {
            ordinary(&mut random)
        };
        let number = track(&mut random);
        for (at, structure) in STRUCTURES.into_iter().enumerate() {
            let old =
                oracle::build_layout_path(structure, "/music", &artist, &album, &title, number, ".flac");
            if changed_on_purpose(structure, &old, &artist, &album, &title) {
                continue;
            }
            let new =
                PathHelper::build_layout_path(structure, "/music", &artist, &album, &title, number, ".flac");
            assert_eq!(
                new, old,
                "{structure:?} {artist:?} {album:?} {title:?} {number:?}"
            );
            compared[at] += 1;
        }
    }
    assert!(
        compared.iter().all(|count| *count > 17_000),
        "compared {compared:?}"
    );
}

/// What placement used to do around the builder (`layout_target_in` at 475ec631): Flat names the
/// file after the whole credit, the others the folder after the first artist, and a blank artist
/// is "Unknown Artist".
#[test]
fn presets_match_the_old_artist_choice() {
    let mut random = DotnetRandom::new(1350);
    let mut compared = 0;
    for _ in 0..20_000 {
        let folder_artist = ordinary(&mut random);
        let file_artist = match random.next(3) {
            0 => folder_artist.clone(),
            1 => format!("{folder_artist}, {}", piece(&mut random, 12)),
            _ => ordinary(&mut random),
        };
        let (album, title, number) = (ordinary(&mut random), ordinary(&mut random), track(&mut random));
        let named = |name: &str| {
            if name.trim().is_empty() {
                "Unknown Artist".to_string()
            } else {
                name.to_string()
            }
        };
        for structure in STRUCTURES {
            let artist = if structure == FolderStructure::Flat {
                &file_artist
            } else {
                &folder_artist
            };
            let old = oracle::build_layout_path(
                structure,
                "/music",
                &named(artist),
                &album,
                &title,
                number,
                ".flac",
            );
            if changed_on_purpose(structure, &old, &named(artist), &album, &title) {
                continue;
            }
            let song = LayoutValues {
                artist: named(&folder_artist),
                track_artist: named(&file_artist),
                album: album.clone(),
                title: title.clone(),
                track: number,
                ..LayoutValues::default()
            };
            let new = PathHelper::join_names(
                "/music",
                &Layout::preset(structure).render_names(&song, ".flac"),
                ".flac",
            );
            assert_eq!(
                new, old,
                "{structure:?} {folder_artist:?} {file_artist:?} {album:?} {title:?}"
            );
            compared += 1;
        }
    }
    assert!(compared > 50_000, "only {compared} compared");
}

#[test]
fn custom_with_a_preset_template_gives_the_preset() {
    let song = values("Daft Punk", "Discovery", "Digital Love", Some(3));
    for (structure, text) in [
        (FolderStructure::Flat, FLAT),
        (FolderStructure::ByArtist, BY_ARTIST),
        (FolderStructure::Organized, ORGANIZED),
    ] {
        let custom = Layout::custom_or_preset(FolderStructure::Custom, text, "", "");
        assert!(custom.problems().is_empty());
        assert_eq!(
            custom.render_names(&song, ".flac"),
            Layout::preset(structure).render_names(&song, ".flac")
        );
    }
}

// ---- Rendering ------------------------------------------------------------------------------

#[test]
fn the_issues_own_example_renders() {
    let template = "{Artist}/{Album}/{Artist} - {Album} - {Track:00} - {Title}";
    assert_eq!(
        render(
            template,
            &values("Daft Punk", "Discovery", "One More Time", Some(1))
        ),
        "Daft Punk/Discovery/Daft Punk - Discovery - 01 - One More Time"
    );
    // A single: the folder takes the title, the file name has no empty album in it.
    assert_eq!(
        render(template, &values("Daft Punk", "", "One More Time", None)),
        "Daft Punk/One More Time/Daft Punk - One More Time"
    );
}

#[test]
fn empty_names_leave_no_stray_text() {
    let song = values("Daft Punk", "Discovery", "One More Time", None);
    for (template, expected) in [
        (
            "{Artist}/{Album}/{Track:00} - {Title}",
            "Daft Punk/Discovery/One More Time",
        ),
        (
            "{Artist}/{Year} - {Album}/{Title}",
            "Daft Punk/Discovery/One More Time",
        ),
        (
            "{Artist}/{Album}/{Track}. {Title}",
            "Daft Punk/Discovery/One More Time",
        ),
        (
            "{Artist}/{Album} - {Year}/{Title}",
            "Daft Punk/Discovery/One More Time",
        ),
        (
            "{Artist}/{Album}/{Artist} - {Album} - {Track:00} - {Title}",
            "Daft Punk/Discovery/Daft Punk - Discovery - One More Time",
        ),
        (
            "{Artist}/{Album} ({Year})/{Title}",
            "Daft Punk/Discovery/One More Time",
        ),
        (
            "{Artist}/{Album} [{Year}]/{Title}",
            "Daft Punk/Discovery/One More Time",
        ),
        (
            "{Artist}/({Year}) {Album}/{Title}",
            "Daft Punk/Discovery/One More Time",
        ),
        (
            "{Artist}/{Album} [{Album Type}]/{Title}",
            "Daft Punk/Discovery/One More Time",
        ),
        (
            "{Artist}/{Album} ({Year} Remaster)/{Title}",
            "Daft Punk/Discovery/One More Time",
        ),
        (
            "{Artist}/{Album} ({Year}, {Album Type})/{Title}",
            "Daft Punk/Discovery/One More Time",
        ),
        (
            "{Artist}/{Album}/Disc {Disc}/{Track:00} - {Title}",
            "Daft Punk/Discovery/One More Time",
        ),
        (
            "{Artist}/{Album}/{Disc}-{Track:00} - {Title}",
            "Daft Punk/Discovery/One More Time",
        ),
        (
            "{Artist}/{Album}/{Track:00} - {Year} - {Title}",
            "Daft Punk/Discovery/One More Time",
        ),
        (
            "{Artist}/{Album}{ (Release Year)}/{Title}",
            "Daft Punk/Discovery/One More Time",
        ),
        ("Library/{Artist}/{Title}", "Library/Daft Punk/One More Time"),
    ] {
        assert_eq!(render(template, &song), expected, "{template}");
    }
    let typed = LayoutValues {
        album_type: Some("Album".to_string()),
        ..song.clone()
    };
    assert_eq!(
        render("{Artist}/{Album} ({Year}, {Album Type})/{Title}", &typed),
        "Daft Punk/Discovery (Album)/One More Time"
    );
    assert_eq!(
        render("{Artist}/{Album} ({Album Type}, {Year})/{Title}", &typed),
        "Daft Punk/Discovery (Album)/One More Time"
    );
}

#[test]
fn names_with_values_keep_their_text() {
    let song = LayoutValues {
        year: Some(2001),
        disc: Some(2),
        multi_disc: true,
        album_type: Some("Album".to_string()),
        ..values("Daft Punk", "Discovery", "One More Time", Some(1))
    };
    for (template, expected) in [
        (
            "{Artist}/{Album} ({Year})/{Title}",
            "Daft Punk/Discovery (2001)/One More Time",
        ),
        (
            "{Artist}/{Album}/Disc {Disc}/{Title}",
            "Daft Punk/Discovery/Disc 2/One More Time",
        ),
        (
            "{Artist}/{Album} ({Year}, {Album Type})/{Title}",
            "Daft Punk/Discovery (2001, Album)/One More Time",
        ),
        (
            "{Artist}/{Album}/{Disc}-{Track:00} - {Title}",
            "Daft Punk/Discovery/2-01 - One More Time",
        ),
        (
            "{Artist}/{Album}/Disc {Disc:00}/{Track:000} {Title}",
            "Daft Punk/Discovery/Disc 02/001 One More Time",
        ),
        (
            "{Artist}/{Album}{ (Release Year)}/{-Track Title}",
            "Daft Punk/Discovery (2001)/-One More Time",
        ),
    ] {
        assert_eq!(render(template, &song), expected, "{template}");
    }
}

/// The rules read the template, so a title's own text is never trimmed.
#[test]
fn a_title_that_starts_with_a_dash_keeps_it() {
    assert_eq!(
        render(ORGANIZED, &values("A", "B", "- Intro", None)),
        "A/B/- Intro"
    );
}

#[test]
fn a_value_cannot_make_a_folder_or_escape_the_root() {
    let song = values("AC/DC", "../..", "Back\\In/Black", Some(1));
    let names = names(&Template::parse(ORGANIZED).expect("parses"), &song);
    assert_eq!(
        names,
        ["AC_DC", "\u{FF0E}._", "01 - Back_In_Black"].map(String::from)
    );
    for dots in [".", ".."] {
        let names = names_of(&values("A", dots, dots, None));
        assert!(
            names.iter().all(|name| !matches!(name.as_str(), "" | "." | "..")),
            "{names:?}"
        );
    }
}

fn names_of(song: &LayoutValues) -> Vec<String> {
    names(&Template::parse(ORGANIZED).expect("parses"), song)
}

#[test]
fn the_other_names_render() {
    let song = LayoutValues {
        track_artist: "Bizarrap, Rauw Alejandro".to_string(),
        album_artist: Some("Various Artists".to_string()),
        album_type: Some("Compilation".to_string()),
        year: Some(2023),
        ..values("Bizarrap", "Now 100", "Session", Some(4))
    };
    assert_eq!(
        render(
            "{Album Artist}/{Album} ({Year}, {Album Type})/{Track Artist} - {Title}",
            &song
        ),
        "Various Artists/Now 100 (2023, Compilation)/Bizarrap, Rauw Alejandro - Session"
    );
    // Lidarr's names: {Artist Name} is the album's artist, as in Lidarr.
    assert_eq!(
        render(
            "{Artist Name}/{Album Title} ({Release Year})/{track:00} - {Track Title}",
            &song
        ),
        "Various Artists/Now 100 (2023)/04 - Session"
    );
    let plain = values("Daft Punk", "Discovery", "Aerodynamic", Some(2));
    assert_eq!(
        render("{AlbumArtist}/{Album}/{Title}", &plain),
        "Daft Punk/Discovery/Aerodynamic"
    );
}

#[test]
fn no_album_files_a_song_under_its_own_title() {
    assert_eq!(
        render(ORGANIZED, &values("Daft Punk", "", "Digital Love", None)),
        "Daft Punk/Digital Love/Digital Love"
    );
}

/// The cap is the template's, never the title's, so one album's tracks share one folder name.
#[test]
fn folder_names_are_cut_by_the_template_alone() {
    let artist =
        "Wiener Philharmoniker, Herbert von Karajan and the Singverein der Gesellschaft der Musikfreunde";
    let album =
        "Beethoven: Symphony No. 9 in D minor, Op. 125 Choral, live at the Grosser Musikvereinssaal 1955";
    let template = Template::parse(ORGANIZED).expect("parses");
    let cap = template.folder_cap(PATH_BUDGET);
    assert_eq!(cap, 91);
    let short = names(&template, &values(artist, album, "IV. Presto", Some(4)));
    let long = names(
        &template,
        &values(
            artist,
            album,
            &"Allegro assai vivace, alla marcia ".repeat(3),
            Some(4),
        ),
    );
    assert_eq!(short[..2], long[..2]);
    assert!(
        short
            .iter()
            .take(2)
            .all(|name| utf16_len(name) <= cap && !name.ends_with(['.', ' ']))
    );
    let rendered = template.render(&values(artist, album, "x", None), ".flac", PATH_BUDGET);
    assert!(rendered[0].uncut.len() > rendered[0].name.len());
}

// ---- Folders that name an album -------------------------------------------------------------

#[test]
fn an_album_folder_needs_an_album_and_an_artist_in_the_folders() {
    for (template, expected) in [
        (ORGANIZED, Some(1)),
        (FLAT, None),
        (BY_ARTIST, None),
        ("{Album Artist}/{Year} - {Album}/{Title}", Some(1)),
        ("{Album}/{Title}", None),
        ("{Artist}/{Title} - {Album}", None),
        ("Music/{Artist} - {Album}/{Title}", Some(1)),
        ("{Artist}/{Album}/Disc {Disc}/{Title}", Some(1)),
    ] {
        assert_eq!(
            Template::parse(template).expect("parses").album_folder_segment(),
            expected,
            "{template}"
        );
    }
}

#[test]
fn the_tidy_s_artist_folders_follow_the_top_folder() {
    for (template, expected) in [
        (ORGANIZED, true),
        (BY_ARTIST, true),
        (FLAT, true),
        ("{Artist} - {Album}/{Title}", false),
        ("{Album Artist}/{Album}/{Title}", false),
        ("Music/{Artist}/{Title}", false),
        ("{-Artist}/{Title}", false),
    ] {
        assert_eq!(
            Template::parse(template)
                .expect("parses")
                .top_folder_is_artist_or_none(),
            expected,
            "{template}"
        );
    }
}

// ---- Template rules -------------------------------------------------------------------------

#[test]
fn each_rule_has_its_words() {
    for (template, words) in [
        ("   ", "Type a layout"),
        ("/{Artist}/{Title}", "Start with a name"),
        ("{Artist}/{Title}/", "End with the file name"),
        ("{Artist}//{Title}", "Two / in a row"),
        ("{Artist}/../{Title}", "can't be named . or .."),
        ("{Artist}/./{Title}", "can't be named . or .."),
        ("{Artist}/{Album}", "Put {Title} in the file name"),
        ("{Title}/{Artist}", "Put {Title} in the file name"),
        ("{Artist}/{Title}.flac", "Leave out .flac"),
        ("{Artist}/{Title}.MP3", "Leave out .mp3"),
        ("{Artst}/{Title}", "{Artst} isn't a name Octo knows"),
        ("{Art{ist}/{Title}", "A { is still open"),
        ("{Artist/{Title}", "A { has no closing }"),
        ("{Artist}/{Title", "A { has no closing }"),
        ("{Artist}}/{Title}", "A } has no { before it"),
        ("{Year:00}/{Title}", "Only {Track} and {Disc}"),
        ("{Artist}/{Track:2} {Title}", "Write the number format as zeros"),
        (
            "{Artist}/{Track:00000} {Title}",
            "Write the number format as zeros",
        ),
        ("{Artist}: {Album}/{Title}", "Windows can't have : in a name"),
        ("{Artist}/{Title}?", "Windows can't have ? in a name"),
        ("{Artist}/{Title} \"live\"", "Windows can't have \" in a name"),
        ("{Artist}/{Title}\u{7}x", "invisible control character"),
        (".hidden/{Artist}/{Title}", "starts with a dot"),
        ("...Albums/{Artist}/{Title}", "starts with a dot"),
        ("Music./{Artist}/{Title}", "at the end of a folder name"),
        ("Music /{Artist}/{Title}", "at the end of a folder name"),
        ("CON/{Artist}/{Title}", "keeps the name CON"),
        ("lost+found/{Artist}/{Title}", "keeps the name lost+found"),
        (
            "{Year}/{Artist}/{Album}/{Title}",
            "{Year} can differ track by track, so it can't be above the album folder.",
        ),
        (
            "{Album Type}/{Artist}/{Album}/{Title}",
            "{Album Type} can differ track by track, so it can't be above the album folder.",
        ),
        (
            "{Artist}/Disc {Disc}/{Album}/{Title}",
            "Put the disc in its own folder below the album: {Album}/Disc {Disc}.",
        ),
        (
            "{Artist}/{Album} (Disc {Disc})/{Title}",
            "{Disc} changes track by track, so it can't be in the album folder's name or above it.",
        ),
        (
            "{Artist}/{Album} - {Track:00}/{Title}",
            "{Track} changes track by track, so it can only be in the file name or a folder below the album.",
        ),
        (
            "{Artist}/{Album} - {Title}/{Title}",
            "{Title} changes track by track",
        ),
        (
            "{Track Artist}/{Artist}/{Album}/{Title}",
            "{Track Artist} changes track by track",
        ),
        (
            "{Artist}/{Album}/{Medium Format} {medium:00}/{Title}",
            "Octo doesn't know {Medium Format}",
        ),
    ] {
        let message = refused(template);
        assert!(message.contains(words), "{template:?}: {message}");
    }
    assert!(refused(&format!("{{Artist}}/{}{{Title}}", "x".repeat(300))).contains("under 300 characters"));
}

/// A dot or a space typed inside a name's braces lands on the name as one typed outside them
/// would, so it gets the same words.
#[test]
fn a_dot_inside_the_braces_is_refused_as_one_outside() {
    let leading = refused(".{Artist}/{Title}");
    assert!(leading.contains("starts with a dot"), "{leading}");
    for template in [
        "{.Artist}/{Title}",
        "{ .Artist}/{Title}",
        "Music/ {.Artist}/{Title}",
        "{Artist}/{.Title}",
    ] {
        assert_eq!(refused(template), leading, "{template}");
    }
    let trailing = refused("{Artist}/{Album}./{Title}");
    assert!(trailing.contains("at the end of a folder name"), "{trailing}");
    for template in [
        "{Artist}/{Album.}/{Title}",
        "{Artist}/{Album }/{Title}",
        "{Artist}/{Album}{ (Year).}/{Title}",
        "{Artist.}/{Album}/{Title}",
    ] {
        assert_eq!(refused(template), trailing, "{template}");
    }
    // Lidarr's names, and a dot or space inside braces away from a name's ends, stay taken.
    for template in [
        "{Artist Name}/{Album Title}{ (Release Year)}/{track:00} - {Track Title}",
        "{Artist.Name}/{Album.Title}/{Track.Title}",
        "{Artist}/{Album}{ - Year}/{Track:00}{. Title}",
        "{-Artist}/{Title}",
    ] {
        assert!(
            Template::parse(template).is_ok(),
            "{template}: {:?}",
            Template::parse(template)
        );
    }
}

#[test]
fn templates_octo_should_take() {
    for template in [
        "{Artist}/{Album}/{Track:00} - {Title}",
        "{artist}/{ALBUM}/{track:00} - {title}",
        "{Album Artist}\\{Album}\\{Title}",
        "Music/{Artist}/{Title}",
        "{Artist}/{Album}/Disc {Disc:0}/{Title}",
        "{Artist}/{Album}/Disc {Disc}/{Title}",
        "{Artist}/{Album} ({Year}) [{Album Type}]/{Track:00} - {Title}",
        // No album folder: every name may sit in any folder.
        "{Year}/{Artist} - {Title}",
        "{Year}/{Artist}/{Title}",
        "{Track Artist}/{Disc}/{Title}",
        "{Artist Name}/{Album Title}{ (Release Year)}/{Artist Name} - {Album Title} - {track:00} - {Track Title}",
        FLAT,
        BY_ARTIST,
    ] {
        assert!(
            Template::parse(template).is_ok(),
            "{template}: {:?}",
            Template::parse(template)
        );
    }
    assert_eq!(
        Template::parse("{Album Artist}\\{Album}\\{Title}")
            .expect("parses")
            .text(),
        "{Album Artist}/{Album}/{Title}"
    );
}

// ---- Settings and kinds ---------------------------------------------------------------------

/// A layout's cut caps are the built-in ones, its own templates' in both budgets, and the ones
/// the settings remember; every name its templates render carries them, and a template parsed
/// alone carries none.
#[test]
fn a_layout_knows_the_caps_a_folder_may_have_been_cut_to() {
    let built_in: Vec<usize> = crate::common::path_helper::built_in_caps().collect();
    let custom = "{Artist}/{Album}/{Track:00} - {Track Artist} - {Title}";
    assert_eq!(Template::parse(custom).expect("parses").own_caps(), [86, 69]);
    assert!(Template::parse(FLAT).expect("parses").own_caps().is_empty());

    let organized = Layout::preset(FolderStructure::Organized);
    let mut expected = built_in.clone();
    expected.sort_unstable();
    expected.dedup();
    assert_eq!(organized.cut_caps(), expected);

    let own = Layout::custom_or_preset(FolderStructure::Custom, custom, "", "");
    assert!(own.cut_caps().contains(&86) && own.cut_caps().contains(&69));

    let settings = SubsonicSettings {
        folder_structure: FolderStructure::Organized,
        layout_caps: vec![83, 67],
        ..SubsonicSettings::default()
    };
    let remembering = Layout::from_settings(&settings);
    assert!(remembering.cut_caps().contains(&83) && remembering.cut_caps().contains(&67));
    assert!(!organized.cut_caps().contains(&83));
    let song = values("A", "B", "C", Some(1));
    for kind in [LayoutKind::Album, LayoutKind::Single, LayoutKind::Compilation] {
        for name in remembering.template(kind).render(&song, ".flac", PATH_BUDGET) {
            assert_eq!(name.cut_caps, remembering.cut_caps());
        }
    }
    let alone = Template::parse(ORGANIZED).expect("parses");
    assert!(
        alone
            .render(&song, ".flac", PATH_BUDGET)
            .iter()
            .all(|name| name.cut_caps.is_empty())
    );

    // The settings store remembers only what the built-in caps do not already cover.
    assert!(caps_to_remember([ORGANIZED, BY_ARTIST, ""]).is_empty());
    assert_eq!(
        caps_to_remember([ORGANIZED, custom, "{Broken"])
            .into_iter()
            .collect::<Vec<_>>(),
        [69, 86]
    );
}

#[test]
fn a_broken_custom_template_falls_back_and_says_why() {
    let song = values("A", "B", "C", Some(1));
    let layout = Layout::custom_or_preset(FolderStructure::Custom, "{Artst}/{Title}", "", "");
    assert_eq!(layout.problems().len(), 1);
    assert_eq!(layout.problems()[0].setting, "FolderTemplate");
    assert!(layout.problems()[0].message.contains("{Artst}"));
    assert_eq!(
        layout.render_names(&song, ".flac"),
        ["A", "B", "01 - C"].map(String::from)
    );

    let no_title = Layout::custom_or_preset(FolderStructure::Custom, "{Artist}/{Album}", "", "");
    assert_eq!(no_title.problems()[0].setting, "FolderTemplate");
    assert_eq!(
        no_title.render_names(&song, ".flac"),
        ["A", "B", "01 - C"].map(String::from)
    );

    let singles = Layout::custom_or_preset(FolderStructure::Custom, "{Artist}/{Title}", "{Nope}", "");
    assert_eq!(singles.problems()[0].setting, "FolderTemplateSingles");
    let single = LayoutValues {
        kind: LayoutKind::Single,
        ..song.clone()
    };
    assert_eq!(
        singles.render_names(&single, ".flac"),
        ["A", "C"].map(String::from)
    );
}

#[test]
fn a_preset_never_reads_the_custom_templates() {
    let layout = Layout::custom_or_preset(FolderStructure::Flat, "{Broken", "{Broken", "{Broken");
    assert!(layout.problems().is_empty());
    assert_eq!(
        layout.render_names(&values("A", "B", "C", None), ".flac"),
        ["A - C"].map(String::from)
    );
}

#[test]
fn each_kind_takes_its_template_and_blank_means_the_album_one() {
    let layout = Layout::custom_or_preset(
        FolderStructure::Custom,
        "{Artist}/{Album}/{Track:00} - {Title}",
        "{Artist}/Singles/{Title}",
        "Compilations/{Album}/{Track:00} - {Track Artist} - {Title}",
    );
    assert!(layout.problems().is_empty());
    let mut song = values("Miley Cyrus", "Now 115", "Flowers", Some(3));
    assert_eq!(
        layout.render_names(&song, ".flac").join("/"),
        "Miley Cyrus/Now 115/03 - Flowers"
    );
    song.kind = LayoutKind::Single;
    assert_eq!(
        layout.render_names(&song, ".flac").join("/"),
        "Miley Cyrus/Singles/Flowers"
    );
    song.kind = LayoutKind::Compilation;
    assert_eq!(
        layout.render_names(&song, ".flac").join("/"),
        "Compilations/Now 115/03 - Miley Cyrus - Flowers"
    );
    let only_albums = Layout::custom_or_preset(FolderStructure::Custom, "{Artist}/{Title}", " ", "");
    assert_eq!(
        only_albums.render_names(&song, ".flac").join("/"),
        "Miley Cyrus/Flowers"
    );
}

/// An unset FOLDER_TEMPLATE reaches the settings as an empty value: Custom with nothing typed
/// files by Organized, with no problem to show.
#[test]
fn a_blank_album_template_is_organized() {
    for blank in ["", "   "] {
        let layout = Layout::custom_or_preset(FolderStructure::Custom, blank, "", "");
        assert!(layout.problems().is_empty(), "{blank:?}");
        let song = values("Daft Punk", "Discovery", "One More Time", Some(1));
        assert_eq!(
            layout.render_names(&song, ".flac").join("/"),
            "Daft Punk/Discovery/01 - One More Time"
        );
    }
}

#[test]
fn a_release_s_kind_and_type_word() {
    assert_eq!(LayoutKind::of(Some("album"), false, false), LayoutKind::Album);
    assert_eq!(LayoutKind::of(Some("ep"), false, false), LayoutKind::Album);
    assert_eq!(LayoutKind::of(Some("single"), false, false), LayoutKind::Single);
    assert_eq!(
        LayoutKind::of(Some("album; compilation"), false, false),
        LayoutKind::Compilation
    );
    assert_eq!(
        LayoutKind::of(Some("single"), true, false),
        LayoutKind::Compilation
    );
    assert_eq!(LayoutKind::of(None, false, true), LayoutKind::Single);
    assert_eq!(LayoutKind::of(None, false, false), LayoutKind::Album);
    assert_eq!(
        album_type_word(Some("album; compilation"), false).as_deref(),
        Some("Compilation")
    );
    assert_eq!(album_type_word(Some("EP"), false).as_deref(), Some("EP"));
    assert_eq!(album_type_word(Some("single"), false).as_deref(), Some("Single"));
    assert_eq!(
        album_type_word(Some("broadcast"), false).as_deref(),
        Some("Broadcast")
    );
    assert_eq!(
        album_type_word(Some("audiobook"), false).as_deref(),
        Some("Other")
    );
    assert_eq!(album_type_word(None, true).as_deref(), Some("Compilation"));
    assert_eq!(album_type_word(None, false), None);
}

// ---- Names on disk --------------------------------------------------------------------------

#[test]
fn rendered_names_are_safe_on_disk() {
    let song = values(".38 Special", "Con", "Are You Ready?", Some(3));
    assert_eq!(
        names_of(&song),
        ["\u{FF0E}38 Special", "Con_", "03 - Are You Ready\u{FF1F}"].map(String::from)
    );
    let long = values("A", "B", &"日".repeat(100), None);
    let rendered = names(
        &Template::parse("{Artist}/{Album}/{Title} {Title}").expect("parses"),
        &long,
    );
    assert!(rendered[2].len() + ".flac".len() <= 255, "{}", rendered[2].len());
}

#[test]
fn layout_values_keep_through_json() {
    let song = LayoutValues {
        album_artist: Some("Various Artists".to_string()),
        disc: Some(2),
        year: Some(2001),
        album_type: Some("Compilation".to_string()),
        kind: LayoutKind::Compilation,
        ..values("A", "B", "C", Some(1))
    };
    let json = serde_json::to_string(&song).expect("written");
    assert_eq!(serde_json::from_str::<LayoutValues>(&json).expect("read"), song);
    assert_eq!(
        serde_json::from_str::<LayoutValues>("{}").expect("read"),
        LayoutValues::default()
    );
}

/// A long file name gives up its title first, then the other names from the right, each down to
/// 20 units; the fixed text and the track number stay.
#[test]
fn the_title_shrinks_before_the_other_names() {
    let template =
        Template::parse("{Artist}/{Album}/{Artist} - {Album} - {Track:00} - {Title}").expect("parses");
    let song = values(
        "Godspeed You! Black Emperor",
        "Lift Your Skinny Fists Like Antennas to Heaven",
        "Storm: Lift Yr Skinny Fists, Like Antennas to Heaven / Gathering Storm / Il Pleut à Mourir",
        Some(1),
    );
    let whole = template.render_file_within(&song, ".flac", 1000);
    assert!(whole.ends_with("Il Pleut à Mourir"), "{whole}");

    let fitted = template.render_file_within(&song, ".flac", 120);
    assert!(utf16_len(&fitted) <= 120, "{fitted}");
    assert!(
        fitted.starts_with(
            "Godspeed You! Black Emperor - Lift Your Skinny Fists Like Antennas to Heaven - 01 - "
        ),
        "the other names keep their length while the title can give: {fitted}"
    );

    let tight = template.render_file_within(&song, ".flac", 70);
    assert!(
        tight.contains(" - 01 - "),
        "the fixed text and track stay: {tight}"
    );
    let parts: Vec<&str> = tight.split(" - ").collect();
    assert!(parts.iter().all(|part| utf16_len(part) >= 2), "{tight}");
}

/// A title longer than a value may be (100 units) is cut to fit on its own: the names beside it
/// keep their length when the title alone can give the room.
#[test]
fn a_title_past_the_value_cap_alone_gives_the_room() {
    let template = Template::parse("{Artist}/{Album}/{Track:00} - {Artist} - {Title}").expect("parses");
    let title = "Words ".repeat(30);
    let song = values("An Artist Of Some Length", "An Album", title.trim(), Some(1));
    let fitted = template.render_file_within(&song, ".mp3", 90);
    assert!(utf16_len(&fitted) <= 90, "{fitted}");
    assert!(
        fitted.starts_with("01 - An Artist Of Some Length - Words"),
        "{fitted}"
    );
}

#[test]
fn the_album_folder_is_the_first_album_name_below_the_artist() {
    let at = |text: &str| Template::parse(text).expect("parses").album_folder_segment();
    assert_eq!(at("{Artist}/{Album}/{Track:00} - {Title}"), Some(1));
    assert_eq!(at("{Artist}/{Album}/Disc {Disc} - {Album}/{Title}"), Some(1));
    assert_eq!(at("Music/{Album Artist}/{Album} ({Year})/{Title}"), Some(2));
    assert_eq!(
        at("Compilations/{Album}/{Title}"),
        None,
        "no artist name, no album folder of its own"
    );
    assert_eq!(
        at("{Album}/{Artist}/{Title}"),
        None,
        "the album above the artist splits by artist"
    );
}

#[test]
fn the_album_folder_shows_its_year_and_type() {
    let shows = |text: &str| Template::parse(text).expect("parses").album_folder_shows();
    assert_eq!(shows("{Artist}/{Album} ({Year})/{Title}"), (true, false));
    assert_eq!(shows("{Artist}/{Album} [{Album Type}]/{Title}"), (false, true));
    assert_eq!(
        shows("{Artist}/{Album}/{Year} - {Title}"),
        (false, false),
        "the file name tells no folders apart"
    );
    assert_eq!(shows("{Year}/{Artist} - {Title}"), (false, false));
}

/// Every path a template allows fits, in the family pool's smaller budget too; a template that
/// cannot is refused with words.
#[test]
fn a_layout_with_no_room_for_names_is_refused() {
    let crowded = format!("{{Artist}}/{}/{{Album}}/{{Title}}", "x".repeat(80));
    assert!(refused(&crowded).contains("too little room for names"));
    let wordy = "{Artist} {Artist} {Artist}/{Album} {Album}/{Album Artist} - {Album} - {Artist} - {Track Artist} - {Year} - {Album Type} - {Title}";
    assert!(refused(wordy).contains("too little room for names"));
    let organized = Template::parse(ORGANIZED).expect("parses");
    assert_eq!(organized.folder_cap(PATH_BUDGET), 91);
    assert_eq!(organized.folder_cap(crate::common::path_helper::POOL_BUDGET), 74);
}

/// What placement asks a template: which names it uses, its disc folder, and whether the album
/// folder sits in an artist's own folder.
#[test]
fn a_template_says_its_disc_folder_and_whether_an_artist_folder_holds_the_album() {
    let parse = |text: &str| Template::parse(text).expect("parses");
    let disc = parse("{Artist}/{Album}/Disc {Disc}/{Track:00} - {Title}");
    assert_eq!(disc.disc_folder_segment(), Some(2));
    assert!(disc.uses(Field::Disc));
    assert!(disc.artist_above_album_folder());
    let in_the_name = parse("{Artist}/{Album}/{Disc}-{Track:00} - {Title}");
    assert_eq!(
        in_the_name.disc_folder_segment(),
        None,
        "the disc is in the file name"
    );
    assert!(in_the_name.uses(Field::Disc));
    let organized = parse(ORGANIZED);
    assert!(!organized.uses(Field::Disc));
    assert!(!organized.uses(Field::Year));
    assert!(organized.artist_above_album_folder());
    let joined = parse("{Album Artist} - {Album}/{Track:00} - {Title}");
    assert!(
        !joined.artist_above_album_folder(),
        "every artist's albums sit side by side"
    );
    assert!(!parse(BY_ARTIST).artist_above_album_folder(), "no album folder");
}
