//! The album identity: one answer to "is this the same album?" for placement, the album sibling
//! and the File layout task.

use std::path::{Path, PathBuf};

use octo_core::common::LayoutValues;
use octo_media::tags::TagFile;
use tempfile::TempDir;

use super::*;

/// What one test file carries.
#[derive(Default, Clone)]
struct Tags<'a> {
    album: &'a str,
    artist: &'a str,
    album_artist: Option<&'a str>,
    year: u32,
    disc: u32,
    release_group: Option<&'a str>,
}

fn mp3(path: &Path, tags: &Tags) -> PathBuf {
    let mut bytes = vec![0u8; 417 * 20];
    for frame in 0..20 {
        bytes[frame * 417..frame * 417 + 4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
    }
    std::fs::create_dir_all(path.parent().expect("a folder")).expect("folders");
    std::fs::write(path, bytes).expect("written");
    let mut file = TagFile::open(path).expect("an MP3");
    file.set_title(Some(path.file_stem().and_then(|s| s.to_str()).unwrap_or("x")));
    file.set_album(Some(tags.album));
    file.set_performers(&[tags.artist.to_string()]);
    if let Some(album_artist) = tags.album_artist {
        file.set_album_artists(&[album_artist.to_string()]);
    }
    file.set_year(tags.year);
    file.set_disc(tags.disc);
    file.set_music_brainz_release_group_id(tags.release_group);
    file.save().expect("saved");
    path.to_path_buf()
}

fn song(album: &str, artist: &str, year: Option<i32>, release_group: Option<&str>) -> AlbumIdentity {
    AlbumIdentity::of_song(&LayoutValues {
        artist: artist.to_string(),
        track_artist: artist.to_string(),
        album: album.to_string(),
        year,
        release_group: release_group.map(str::to_string),
        ..LayoutValues::default()
    })
}

const YEAR: Shown = Shown {
    year: true,
    album_type: false,
};

#[test]
fn a_remaster_of_the_same_release_group_joins_the_album_whatever_its_year() {
    let root = TempDir::new().expect("a folder");
    let beatles = |year| Tags {
        album: "Abbey Road",
        artist: "The Beatles",
        album_artist: Some("The Beatles"),
        year,
        release_group: Some("rg-abbey"),
        ..Tags::default()
    };
    mp3(
        &root.path().join("Abbey Road (1969)/01 - Come Together.mp3"),
        &beatles(1969),
    );
    let remaster = song("Abbey Road", "The Beatles", Some(2009), Some("RG-ABBEY"));
    assert_eq!(
        find_album_folder(root.path(), &remaster, YEAR, None).as_deref(),
        Some("Abbey Road (1969)")
    );
}

#[test]
fn two_release_groups_of_one_title_stay_apart() {
    let root = TempDir::new().expect("a folder");
    let weezer = |year, group| Tags {
        album: "Weezer",
        artist: "Weezer",
        album_artist: Some("Weezer"),
        year,
        release_group: Some(group),
        ..Tags::default()
    };
    mp3(
        &root.path().join("Weezer (1994)/01 - My Name Is Jonas.mp3"),
        &weezer(1994, "rg-blue"),
    );
    let green = song("Weezer", "Weezer", Some(2001), Some("rg-green"));
    assert_eq!(find_album_folder(root.path(), &green, YEAR, None), None);
}

/// A year without a release group is weak: it only chooses between folders that already match.
#[test]
fn a_track_without_a_release_group_joins_its_album_under_year() {
    let root = TempDir::new().expect("a folder");
    mp3(
        &root.path().join("Rumours (1977)/01 - Second Hand News.mp3"),
        &Tags {
            album: "Rumours",
            artist: "Fleetwood Mac",
            album_artist: Some("Fleetwood Mac"),
            year: 1977,
            release_group: Some("rg-rumours"),
            ..Tags::default()
        },
    );
    let reissue_dated = song("Rumours", "Fleetwood Mac", Some(2004), None);
    assert_eq!(
        find_album_folder(root.path(), &reissue_dated, YEAR, None).as_deref(),
        Some("Rumours (1977)")
    );

    // With two such folders, the shown year chooses.
    mp3(
        &root.path().join("Rumours (2004)/01 - Second Hand News.mp3"),
        &Tags {
            album: "Rumours",
            artist: "Fleetwood Mac",
            album_artist: Some("Fleetwood Mac"),
            year: 2004,
            ..Tags::default()
        },
    );
    assert_eq!(
        find_album_folder(root.path(), &reissue_dated, YEAR, None).as_deref(),
        Some("Rumours (2004)")
    );
}

#[test]
fn a_folder_whose_first_file_is_the_minority_year_still_joins() {
    let root = TempDir::new().expect("a folder");
    let track = |year| Tags {
        album: "Abbey Road",
        artist: "The Beatles",
        album_artist: Some("The Beatles"),
        year,
        ..Tags::default()
    };
    mp3(
        &root.path().join("Abbey Road (1969)/01 - Come Together.mp3"),
        &track(2009),
    );
    mp3(
        &root.path().join("Abbey Road (1969)/02 - Something.mp3"),
        &track(1969),
    );
    mp3(
        &root.path().join("Abbey Road (1969)/03 - Maxwell.mp3"),
        &track(1969),
    );
    let files: Vec<LayoutTags> = album_files(&root.path().join("Abbey Road (1969)"))
        .iter()
        .filter_map(|file| read_layout_tags(file))
        .collect();
    assert_eq!(AlbumIdentity::of_files(&files).and_then(|id| id.year), Some(1969));
    let next = song("Abbey Road", "The Beatles", Some(1969), None);
    assert_eq!(
        find_album_folder(root.path(), &next, YEAR, None).as_deref(),
        Some("Abbey Road (1969)")
    );
}

#[test]
fn a_various_artists_rip_without_album_artist_is_one_album() {
    let root = TempDir::new().expect("a folder");
    for (n, artist) in ["Miley Cyrus", "Harry Styles", "Lizzo"].iter().enumerate() {
        mp3(
            &root.path().join(format!("Now 115/0{n} - Song.mp3")),
            &Tags {
                album: "Now That's What I Call Music! 115",
                artist,
                ..Tags::default()
            },
        );
    }
    let files: Vec<LayoutTags> = album_files(&root.path().join("Now 115"))
        .iter()
        .filter_map(|file| read_layout_tags(file))
        .collect();
    let identity = AlbumIdentity::of_files(&files).expect("one album");
    assert_eq!(identity.album_artist, VARIOUS_ARTISTS);
    let compilation_track = song("Now That's What I Call Music! 115", "Various Artists", None, None);
    assert_eq!(
        find_album_folder(root.path(), &compilation_track, Shown::default(), None).as_deref(),
        Some("Now 115")
    );
}

#[test]
fn another_kinds_folder_never_joins_on_title_alone() {
    let root = TempDir::new().expect("a folder");
    for artist in ["ABBA", "Queen"] {
        mp3(
            &root.path().join(format!("Greatest Hits/{artist}.mp3")),
            &Tags {
                album: "Greatest Hits",
                artist,
                ..Tags::default()
            },
        );
    }
    // Queen's own album, trying the compilations template's folder.
    let queen = AlbumIdentity {
        album_artist: VARIOUS_ARTISTS.to_string(),
        ..song("Greatest Hits", "Queen", None, None)
    };
    assert_eq!(
        find_album_folder(root.path(), &queen, Shown::default(), Some("Queen")),
        None
    );
}

#[test]
fn a_folder_of_mixed_albums_is_never_an_album() {
    let root = TempDir::new().expect("a folder");
    let tags = |album| Tags {
        album,
        artist: "Daft Punk",
        album_artist: Some("Daft Punk"),
        ..Tags::default()
    };
    mp3(&root.path().join("Mixed/01.mp3"), &tags("Discovery"));
    mp3(&root.path().join("Mixed/02.mp3"), &tags("Homework"));
    let discovery = song("Discovery", "Daft Punk", None, None);
    assert_eq!(
        find_album_folder(root.path(), &discovery, Shown::default(), None),
        None
    );
}

#[test]
fn a_disc_folder_is_known_by_its_files_whatever_its_name() {
    let root = TempDir::new().expect("a folder");
    let album = root.path().join("The Wall");
    let tags = |disc| Tags {
        album: "The Wall",
        artist: "Pink Floyd",
        disc,
        ..Tags::default()
    };
    mp3(&album.join("CD1/01 - In the Flesh.mp3"), &tags(1));
    mp3(&album.join("CD2/01 - Hey You.mp3"), &tags(2));
    mp3(&album.join("Extras/01 - Demo.mp3"), &tags(0));
    assert_eq!(
        disc_folders(&album),
        vec![("CD1".to_string(), 1), ("CD2".to_string(), 2)]
    );
}

#[test]
fn the_album_search_reads_each_file_once_per_walk() {
    let root = TempDir::new().expect("a folder");
    for n in 0..60 {
        mp3(
            &root.path().join(format!("Album {n:02}/01 - Song.mp3")),
            &Tags {
                album: &format!("Album {n:02}"),
                artist: "Prolific",
                ..Tags::default()
            },
        );
    }
    let last = song("Album 59", "Prolific", None, None);
    // Counted on this thread, so the tests reading tags beside it add nothing.
    let reads = || TAG_READS_HERE.with(|reads| reads.get());
    let before = reads();
    assert_eq!(
        find_album_folder(root.path(), &last, Shown::default(), None).as_deref(),
        Some("Album 59"),
        "past the fiftieth folder"
    );
    let first = reads() - before;
    assert!(first <= 61, "one read a folder, then the passing one: {first}");
    find_album_folder(root.path(), &last, Shown::default(), None);
    assert_eq!(reads() - before, first, "the second search reads nothing");
}

#[test]
fn an_edited_file_is_read_again() {
    let root = TempDir::new().expect("a folder");
    let path = mp3(
        &root.path().join("A/01.mp3"),
        &Tags {
            album: "Before",
            artist: "X",
            ..Tags::default()
        },
    );
    assert_eq!(
        read_layout_tags(&path).and_then(|t| t.album).as_deref(),
        Some("Before")
    );
    let mut file = TagFile::open(&path).expect("open");
    file.set_album(Some("After, longer"));
    file.save().expect("saved");
    assert_eq!(
        read_layout_tags(&path).and_then(|t| t.album).as_deref(),
        Some("After, longer")
    );
}

#[test]
fn featured_artists_keep_one_album_artist() {
    let tags = |artist: &str| LayoutTags {
        album: Some("DAMN.".into()),
        performers: vec![artist.to_string()],
        ..LayoutTags::default()
    };
    let identity = AlbumIdentity::of_files(&[
        tags("Kendrick Lamar"),
        tags("Kendrick Lamar feat. Rihanna"),
        tags("Kendrick Lamar feat. U2"),
    ])
    .expect("one album");
    assert_eq!(identity.album_artist, SongIdentity::key("Kendrick Lamar"));
}

#[test]
fn various_artists_spellings_are_one() {
    assert_eq!(artist_key("VA"), VARIOUS_ARTISTS);
    assert_eq!(artist_key("Various Artists"), VARIOUS_ARTISTS);
    assert_ne!(artist_key("Vanessa"), VARIOUS_ARTISTS);
}

#[test]
fn a_featured_guest_is_left_out_and_a_band_name_kept() {
    assert_eq!(without_featured("Kendrick Lamar feat. Rihanna"), "Kendrick Lamar");
    assert_eq!(without_featured("Calvin Harris (ft. Rihanna)"), "Calvin Harris");
    assert_eq!(without_featured("Santana featuring Rob Thomas"), "Santana");
    assert_eq!(without_featured("Earth, Wind & Fire"), "Earth, Wind & Fire");
    assert_eq!(without_featured("Featurecast"), "Featurecast");
}
