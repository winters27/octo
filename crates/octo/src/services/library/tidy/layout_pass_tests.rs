//! The File layout plan: what moves into the layout, what is left alone and why, and that a
//! library placement filed is already in it.

use octo_core::common::LayoutKind;
use octo_core::common::layout::song_year;
use octo_core::fingerprint::acoust_id_client::AcoustIdRecording;
use octo_core::fingerprint::verification::{VerificationResult, VerificationVerdict};
use octo_core::models::domain::Song;
use octo_core::settings::FolderStructure;

use super::*;
use crate::services::common::base_download_service::{BaseDownloadService, RequestedIdentity};

const ROOT: &str = "/music";

fn at(relative: &str) -> String {
    format!("{ROOT}/{relative}")
}

/// Tags of one track of an album.
fn tags(artist: &str, album: &str, title: &str, track: i32) -> LayoutTags {
    LayoutTags {
        title: Some(title.to_string()),
        performers: vec![artist.to_string()],
        album: (!album.is_empty()).then(|| album.to_string()),
        album_artist: Some(artist.to_string()),
        track: Some(track),
        release_type: Some("album".to_string()),
        ..LayoutTags::default()
    }
}

trait Edit {
    fn year(self, year: i32) -> Self;
    fn disc(self, disc: i32, count: Option<i32>) -> Self;
    fn no_album_artist(self) -> Self;
    fn group(self, id: &str) -> Self;
}

impl Edit for LayoutTags {
    fn year(mut self, year: i32) -> Self {
        self.year = Some(year);
        self
    }
    fn disc(mut self, disc: i32, count: Option<i32>) -> Self {
        self.disc = Some(disc);
        self.disc_count = count;
        self
    }
    fn no_album_artist(mut self) -> Self {
        self.album_artist = None;
        self
    }
    fn group(mut self, id: &str) -> Self {
        self.release_group = Some(id.to_string());
        self
    }
}

fn file(relative: &str, tags: LayoutTags) -> LayoutFile {
    LayoutFile {
        path: at(relative),
        size: 1000,
        in_scope: true,
        tags: Some(tags),
        mapping: None,
    }
}

/// The listings of every folder the files are in, and of the folders above them, with `extra`
/// files (covers, lyrics) beside them.
fn disk_of(files: &[LayoutFile], extra: &[&str]) -> LayoutDisk {
    let mut folders: BTreeMap<String, BTreeSet<(String, bool)>> = BTreeMap::new();
    folders.entry(ROOT.to_string()).or_default();
    let paths: Vec<String> = files
        .iter()
        .map(|file| file.path.clone())
        .chain(extra.iter().map(|rel| at(rel)))
        .collect();
    for path in paths {
        let mut child = path.clone();
        let mut is_dir = false;
        while let Some(cut) = child.rfind('/') {
            let parent = child[..cut].to_string();
            if parent.len() < ROOT.len() {
                break;
            }
            folders
                .entry(parent.clone())
                .or_default()
                .insert((child[cut + 1..].to_string(), is_dir));
            if parent == ROOT {
                break;
            }
            child = parent;
            is_dir = true;
        }
    }
    LayoutDisk {
        folders: folders
            .into_iter()
            .map(|(folder, entries)| (folder, entries.into_iter().collect()))
            .collect(),
        ignored: Vec::new(),
    }
}

fn custom(album: &str) -> Layout {
    Layout::custom_or_preset(FolderStructure::Custom, album, "", "")
}

fn preset(structure: FolderStructure) -> Layout {
    Layout::preset(structure)
}

fn plan_in(files: &[LayoutFile], extra: &[&str], layout: Layout) -> LayoutPlan {
    plan(ROOT, files, &disk_of(files, extra), &LayoutOptions::new(layout))
}

fn rel(path: &str) -> String {
    path.strip_prefix(&format!("{ROOT}/")).unwrap_or(path).to_string()
}

/// Every move as (from, to) under the root, folders first.
fn moves(plan: &LayoutPlan) -> Vec<(String, String)> {
    plan.plan
        .moves
        .iter()
        .map(|m| (rel(&m.from), rel(&m.to)))
        .chain(plan.plan.file_moves.iter().map(|m| (rel(&m.from), rel(&m.to))))
        .collect()
}

fn layout_rows(plan: &LayoutPlan) -> Vec<&TidyRow> {
    plan.plan
        .rows
        .iter()
        .filter(|row| row.kind == TidyKind::Layout)
        .collect()
}

fn skip_saying<'a>(plan: &'a LayoutPlan, words: &str) -> Option<&'a TidyRow> {
    plan.plan
        .rows
        .iter()
        .find(|row| row.kind == TidyKind::Skip && row.evidence.contains(words))
}

fn pair(from: &str, to: &str) -> (String, String) {
    (from.to_string(), to.to_string())
}

const DISCOVERY: &str = "Daft Punk/Discovery";

fn discovery(names: &[(&str, &str, i32)]) -> Vec<LayoutFile> {
    names
        .iter()
        .map(|(name, title, track)| {
            file(
                &format!("{DISCOVERY}/{name}"),
                tags("Daft Punk", "Discovery", title, *track).year(2001),
            )
        })
        .collect()
}

// ---- moves ------------------------------------------------------------------------------------

#[test]
fn a_file_is_renamed_in_its_folder() {
    let files = discovery(&[("1 - One More Time.flac", "One More Time", 1)]);
    let plan = plan_in(&files, &[], preset(FolderStructure::Organized));
    assert_eq!(
        moves(&plan),
        [pair(
            "Daft Punk/Discovery/1 - One More Time.flac",
            "Daft Punk/Discovery/01 - One More Time.flac"
        )]
    );
    let rows = layout_rows(&plan);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, DISCOVERY);
    assert_eq!(rows[0].files, 1);
    assert!(rows[0].ticked && !rows[0].opt_in);
    assert!(
        rows[0]
            .evidence
            .starts_with("1 song moves into the layout, for example ")
    );
    assert!(plan.row_hashes.contains_key(&rows[0].id));
}

#[test]
fn a_whole_folder_is_renamed_and_carries_its_cover() {
    let files = discovery(&[
        ("01 - One More Time.flac", "One More Time", 1),
        ("02 - Aerodynamic.flac", "Aerodynamic", 2),
    ]);
    let plan = plan_in(
        &files,
        &["Daft Punk/Discovery/cover.jpg"],
        custom("{Artist}/{Album} ({Year})/{Track:00} - {Title}"),
    );
    assert_eq!(
        moves(&plan),
        [pair("Daft Punk/Discovery", "Daft Punk/Discovery (2001)")]
    );
    assert!(plan.plan.copies.is_empty(), "the cover goes with its folder");
    assert_eq!(layout_rows(&plan)[0].files, 2);
}

#[test]
fn organized_to_custom_renames_the_files() {
    let files = discovery(&[("01 - One More Time.flac", "One More Time", 1)]);
    let plan = plan_in(
        &files,
        &["Daft Punk/Discovery/01 - One More Time.lrc"],
        custom("{Artist}/{Album}/{Artist} - {Album} - {Track:00} - {Title}"),
    );
    assert_eq!(
        moves(&plan),
        [
            pair(
                "Daft Punk/Discovery/01 - One More Time.flac",
                "Daft Punk/Discovery/Daft Punk - Discovery - 01 - One More Time.flac"
            ),
            pair(
                "Daft Punk/Discovery/01 - One More Time.lrc",
                "Daft Punk/Discovery/Daft Punk - Discovery - 01 - One More Time.lrc"
            ),
        ]
    );
    assert_eq!(
        plan.plan.file_moves[1].song.as_deref(),
        Some(at("Daft Punk/Discovery/01 - One More Time.flac").as_str())
    );
}

#[test]
fn flat_to_organized_makes_the_folders() {
    let files = vec![file(
        "Daft Punk - One More Time.flac",
        tags("Daft Punk", "Discovery", "One More Time", 1),
    )];
    let plan = plan_in(&files, &[], preset(FolderStructure::Organized));
    assert_eq!(
        moves(&plan),
        [pair(
            "Daft Punk - One More Time.flac",
            "Daft Punk/Discovery/01 - One More Time.flac"
        )]
    );
    assert_eq!(layout_rows(&plan)[0].path, "");
}

#[test]
fn a_folder_keeps_the_spelling_it_has() {
    let files = vec![file(
        "Beyonce/Lemonade/01 - Formation.flac",
        tags("Beyonc\u{e9}", "Lemonade", "Formation", 1),
    )];
    let plan = plan_in(&files, &[], preset(FolderStructure::Organized));
    assert!(moves(&plan).is_empty(), "{:?}", moves(&plan));
    assert!(layout_rows(&plan).is_empty());
}

#[test]
fn a_folder_one_of_its_songs_renders_keeps_its_name() {
    // A track dated by a reissue joined the album's folder: the folder is never renamed for it.
    let mut files = vec![
        file(
            "Fleetwood Mac/Rumours (1977)/01 - Second Hand News.flac",
            tags("Fleetwood Mac", "Rumours", "Second Hand News", 1).year(1977),
        ),
        file(
            "Fleetwood Mac/Rumours (1977)/02 - Dreams.flac",
            tags("Fleetwood Mac", "Rumours", "Dreams", 2).year(2004),
        ),
    ];
    let template = custom("{Artist}/{Album} ({Year})/{Track:00} - {Title}");
    assert!(moves(&plan_in(&files, &[], template.clone())).is_empty());
    // Without a song rendering the folder's name, the folder takes the name most render.
    files[0].tags = Some(tags("Fleetwood Mac", "Rumours", "Second Hand News", 1).year(2004));
    assert_eq!(
        moves(&plan_in(&files, &[], template)),
        [pair(
            "Fleetwood Mac/Rumours (1977)",
            "Fleetwood Mac/Rumours (2004)"
        )]
    );
}

#[test]
fn an_album_partly_in_the_run_is_held_whole() {
    let mut files = discovery(&[
        ("01 - One More Time.flac", "One More Time", 1),
        ("02 - Aerodynamic.flac", "Aerodynamic", 2),
    ]);
    files[1].in_scope = false;
    let plan = plan_in(
        &files,
        &[],
        custom("{Artist}/{Album} ({Year})/{Track:00} - {Title}"),
    );
    assert!(moves(&plan).is_empty(), "{:?}", moves(&plan));
    assert!(skip_saying(&plan, "an album only moves whole").is_some());
}

#[test]
fn a_numbered_copy_beside_another_song_is_in_the_layout() {
    let files = vec![
        file(
            "Artist/Album/01 - Intro.flac",
            tags("Artist", "Album", "Intro", 1),
        ),
        file(
            "Artist/Album/01 - Intro (1).flac",
            tags("Artist", "Album", "Intro", 1),
        ),
    ];
    let plan = plan_in(&files, &[], preset(FolderStructure::Organized));
    assert!(moves(&plan).is_empty(), "{:?}", moves(&plan));
    assert!(layout_rows(&plan).is_empty());
}

#[test]
fn a_name_another_song_leaves_is_taken_after_it_is_left() {
    let files = vec![
        file("A/X/03 - Song.flac", tags("A", "X", "Song", 2)),
        file("A/X/02 - Song.flac", tags("A", "X", "Song", 1)),
    ];
    let plan = plan_in(&files, &[], preset(FolderStructure::Organized));
    assert_eq!(
        moves(&plan),
        [
            pair("A/X/02 - Song.flac", "A/X/01 - Song.flac"),
            pair("A/X/03 - Song.flac", "A/X/02 - Song.flac"),
        ]
    );
}

#[test]
fn the_cover_is_copied_when_an_album_leaves_its_folder() {
    let files = vec![file(
        "Daft Punk/One More Time.flac",
        tags("Daft Punk", "Discovery", "One More Time", 1),
    )];
    let plan = plan_in(
        &files,
        &["Daft Punk/cover.jpg"],
        preset(FolderStructure::Organized),
    );
    assert_eq!(
        moves(&plan),
        [pair(
            "Daft Punk/One More Time.flac",
            "Daft Punk/Discovery/01 - One More Time.flac"
        )]
    );
    assert_eq!(
        plan.plan
            .copies
            .iter()
            .map(|c| (rel(&c.from), rel(&c.to)))
            .collect::<Vec<_>>(),
        [pair("Daft Punk/cover.jpg", "Daft Punk/Discovery/cover.jpg")]
    );
}

#[test]
fn a_folder_longer_than_the_layout_allows_is_shortened() {
    // An album named at the old 100-unit cap, past Organized's 91.
    let long = "Wordy ".repeat(17);
    let album = &long[..100];
    let files = vec![file(
        &format!("Artist/{album}/01 - Song.flac"),
        tags("Artist", album, "Song", 1),
    )];
    let plan = plan_in(&files, &[], preset(FolderStructure::Organized));
    let moved = moves(&plan);
    assert_eq!(moved.len(), 1, "{moved:?}");
    let new_name = moved[0].1.trim_start_matches("Artist/").to_string();
    assert!(utf16_len(&new_name) <= 91, "{new_name}");
    assert!(album.starts_with(new_name.trim_end()));
}

#[test]
fn a_hidden_folder_octo_made_is_renamed_visible() {
    let mut files = vec![file(
        ".38 Special/Wild-Eyed Southern Boys/01 - Hold On Loosely.flac",
        tags(".38 Special", "Wild-Eyed Southern Boys", "Hold On Loosely", 1),
    )];
    files[0].mapping = Some(LayoutMapping {
        title: "Hold On Loosely".into(),
        album: "Wild-Eyed Southern Boys".into(),
        artist: ".38 Special".into(),
        values: None,
    });
    let plan = plan_in(&files, &[], preset(FolderStructure::Organized));
    assert_eq!(
        moves(&plan),
        [pair(
            ".38 Special/Wild-Eyed Southern Boys",
            "\u{ff0e}38 Special/Wild-Eyed Southern Boys"
        )]
    );
}

// ---- left alone -------------------------------------------------------------------------------

#[test]
fn two_songs_for_one_name_both_stay() {
    let files = vec![
        file("A/X/1 - Song.flac", tags("A", "X", "Song", 1)),
        file("A/X/Song.flac", tags("A", "X", "Song", 1)),
    ];
    let plan = plan_in(&files, &[], custom("{Artist}/{Album}/{Track:00} {Title}"));
    assert!(plan.plan.file_moves.is_empty(), "{:?}", moves(&plan));
    assert!(skip_saying(&plan, "would both be named").is_some());
}

#[test]
fn a_name_taken_by_a_song_that_stays_holds_the_song() {
    let mut files = vec![
        file("A/X/Song.flac", tags("A", "X", "Song", 1)),
        file("A/X/01 - Song.flac", tags("A", "X", "Other", 2)),
    ];
    files[1].in_scope = false;
    let plan = plan_in(&files, &[], preset(FolderStructure::Organized));
    assert!(plan.plan.file_moves.is_empty(), "{:?}", moves(&plan));
    assert!(skip_saying(&plan, "01 - Song.flac is already there, so this song stays.").is_some());
}

#[test]
fn a_target_folder_of_another_spelling_holds_the_songs() {
    let files = vec![
        file(
            "Daft Punk - One More Time.flac",
            tags("Daft Punk", "Discovery", "One More Time", 1),
        ),
        file(
            "daft punk/Homework/01 - Da Funk.flac",
            tags("Someone Else", "Homework", "Da Funk", 1),
        ),
    ];
    let mut plan = plan_in(&files, &[], preset(FolderStructure::Organized));
    plan.plan.file_moves.retain(|m| m.from.contains("One More Time"));
    assert!(plan.plan.file_moves.is_empty());
    assert!(
        skip_saying(
            &plan,
            "daft punk is already there with other capitals or characters"
        )
        .is_some()
    );
}

#[test]
fn a_file_whose_tags_cannot_be_read_says_so() {
    let files = vec![LayoutFile {
        path: at("A/X/01 - Song.flac"),
        size: 1,
        in_scope: true,
        tags: None,
        mapping: None,
    }];
    let plan = plan_in(&files, &[], preset(FolderStructure::Organized));
    assert!(moves(&plan).is_empty());
    assert!(skip_saying(&plan, "Its tags could not be read").is_some());
}

#[test]
fn a_lidarr_root_that_is_the_music_folder_still_moves_octos_songs() {
    let files = vec![
        file("Mine/1 - Song.flac", tags("Mine", "Mine", "Song", 1)),
        file("Theirs/Album/1 - Track.flac", tags("Theirs", "Album", "Track", 1)),
    ];
    let answer = TidyLidarr {
        roots: vec!["/music/".to_string()],
        artists: vec![(1, "/music/Theirs".to_string())],
        files: vec![("/music/Theirs/Album/1 - Track.flac".to_string(), 1000)],
    };
    let mut options = LayoutOptions::new(preset(FolderStructure::Organized));
    options.lidarr = lidarr_view(ROOT, None, &answer, &files);
    let plan = plan(ROOT, &files, &disk_of(&files, &[]), &options);
    let moved = moves(&plan);
    assert!(
        moved.contains(&pair("Mine/1 - Song.flac", "Mine/Mine/01 - Song.flac")),
        "{moved:?}"
    );
    assert!(!moved.iter().any(|(from, _)| from.starts_with("Theirs")));
    assert!(skip_saying(&plan, "Lidarr keeps this song, so Lidarr names it.").is_some());
}

#[test]
fn a_song_lidarr_keeps_and_its_folder_stay() {
    let files = discovery(&[
        ("01 - One More Time.flac", "One More Time", 1),
        ("02 - Aerodynamic.flac", "Aerodynamic", 2),
    ]);
    let answer = TidyLidarr {
        roots: vec!["/data/music".to_string()],
        artists: vec![(1, "/data/music/Daft Punk".to_string())],
        files: vec![(
            "/data/music/Daft Punk/Discovery/02 - Aerodynamic.flac".to_string(),
            1000,
        )],
    };
    let mut options = LayoutOptions::new(custom("{Artist}/{Album} ({Year})/{Track:00} - {Title}"));
    options.lidarr = lidarr_view(ROOT, Some("/data/music"), &answer, &files);
    let plan = plan(ROOT, &files, &disk_of(&files, &[]), &options);
    assert!(moves(&plan).is_empty(), "{:?}", moves(&plan));
    assert!(skip_saying(&plan, "Lidarr keeps this song").is_some());
}

#[test]
fn a_folder_with_an_ndignore_is_left_alone() {
    let files: Vec<LayoutFile> = Vec::new();
    let mut disk = disk_of(&files, &[]);
    disk.ignored.push(at("Private"));
    let plan = plan(
        ROOT,
        &files,
        &disk,
        &LayoutOptions::new(preset(FolderStructure::Organized)),
    );
    let row = skip_saying(&plan, ".ndignore").expect("a row");
    assert_eq!(row.path, "Private");
    assert_eq!(
        row.evidence,
        "Navidrome is told to leave this folder out (.ndignore), so the tidy leaves it too."
    );
}

#[test]
fn lyrics_two_songs_share_stay_where_they_are() {
    let files = vec![
        file("A/X/1 - Song.flac", tags("A", "X", "Song", 1)),
        file("A/X/1 - Song.mp3", tags("A", "X", "Song", 1)),
    ];
    let plan = plan_in(
        &files,
        &["A/X/1 - Song.lrc"],
        custom("{Artist}/{Album}/{Track:00} {Title}"),
    );
    assert!(!plan.plan.file_moves.iter().any(|m| m.from.ends_with(".lrc")));
    assert!(
        skip_saying(
            &plan,
            "Two songs share the name 1 - Song, so their lyrics stay where they are."
        )
        .is_some()
    );
}

#[test]
fn a_compilation_without_album_artist_is_held() {
    let files: Vec<LayoutFile> = [("Miley Cyrus", "Flowers", 1), ("SZA", "Kill Bill", 2)]
        .iter()
        .map(|(artist, title, track)| {
            file(
                &format!("Now 115/{track:02} - {title}.flac"),
                tags(artist, "Now 115", title, *track).no_album_artist(),
            )
        })
        .collect();
    let plan = plan_in(&files, &[], preset(FolderStructure::Organized));
    assert!(moves(&plan).is_empty(), "{:?}", moves(&plan));
    assert!(
        skip_saying(
            &plan,
            "This compilation's songs would each go to their own artist's folder"
        )
        .is_some()
    );
}

// ---- album groups -----------------------------------------------------------------------------

#[test]
fn an_album_with_featured_artists_is_not_a_compilation() {
    let files: Vec<LayoutFile> = [
        ("Kendrick Lamar", "BLOOD.", 1),
        ("Kendrick Lamar feat. Rihanna", "LOYALTY.", 2),
    ]
    .iter()
    .map(|(artist, title, track)| {
        file(
            &format!("Kendrick Lamar/DAMN/{track:02} - {title}.flac"),
            tags(artist, "DAMN.", title, *track).no_album_artist(),
        )
    })
    .collect();
    let layout = Layout::custom_or_preset(
        FolderStructure::Custom,
        "{Artist}/{Album}/{Track:00} - {Title}",
        "",
        "Compilations/{Album}/{Track:00} - {Title}",
    );
    let plan = plan_in(&files, &[], layout);
    assert!(moves(&plan).is_empty(), "{:?}", moves(&plan));
}

#[test]
fn weezer_groups_split_by_year_as_placement_files_them() {
    let files = vec![
        file(
            "Weezer/Weezer/01 - My Name Is Jonas.flac",
            tags("Weezer", "Weezer", "My Name Is Jonas", 1)
                .year(1994)
                .group("blue"),
        ),
        file(
            "Weezer/Weezer/02 - Don't Let Go.flac",
            tags("Weezer", "Weezer", "Don't Let Go", 2)
                .year(2001)
                .group("green"),
        ),
    ];
    let plan = plan_in(
        &files,
        &[],
        custom("{Artist}/{Album} ({Year})/{Track:00} - {Title}"),
    );
    let moved = moves(&plan);
    assert!(moved.contains(&pair(
        "Weezer/Weezer/01 - My Name Is Jonas.flac",
        "Weezer/Weezer (1994)/01 - My Name Is Jonas.flac"
    )));
    assert!(moved.contains(&pair(
        "Weezer/Weezer/02 - Don't Let Go.flac",
        "Weezer/Weezer (2001)/02 - Don't Let Go.flac"
    )));
}

#[test]
fn disc_one_joins_its_disc_folder() {
    let template = custom("{Artist}/{Album}/Disc {Disc}/{Track:00} - {Title}");
    let files = vec![
        file(
            "Artist/Album/01 - One.flac",
            tags("Artist", "Album", "One", 1).disc(1, None),
        ),
        file(
            "Artist/Album/Disc 2/01 - Two.flac",
            tags("Artist", "Album", "Two", 1).disc(2, Some(2)),
        ),
    ];
    let plan = plan_in(&files, &[], template);
    assert_eq!(
        moves(&plan),
        [pair(
            "Artist/Album/01 - One.flac",
            "Artist/Album/Disc 1/01 - One.flac"
        )]
    );
}

#[test]
fn the_task_keeps_a_completed_disc_layout() {
    let template = custom("{Artist}/{Album}/Disc {Disc}/{Track:00} - {Title}");
    let files = vec![
        file(
            "Artist/Album/Disc 1/01 - One.flac",
            tags("Artist", "Album", "One", 1).disc(1, Some(2)),
        ),
        file(
            "Artist/Album/Disc 2/01 - Two.flac",
            tags("Artist", "Album", "Two", 1).disc(2, Some(2)),
        ),
    ];
    let plan = plan_in(&files, &[], template);
    assert!(moves(&plan).is_empty(), "{:?}", moves(&plan));
    // A disc folder of another name takes the template's.
    let files = vec![
        file(
            "Artist/Album/CD1/01 - One.flac",
            tags("Artist", "Album", "One", 1).disc(1, Some(2)),
        ),
        file(
            "Artist/Album/CD2/01 - Two.flac",
            tags("Artist", "Album", "Two", 1).disc(2, Some(2)),
        ),
    ];
    let plan = plan_in(
        &files,
        &[],
        custom("{Artist}/{Album}/Disc {Disc}/{Track:00} - {Title}"),
    );
    assert_eq!(
        moves(&plan),
        [
            pair("Artist/Album/CD1", "Artist/Album/Disc 1"),
            pair("Artist/Album/CD2", "Artist/Album/Disc 2"),
        ]
    );
}

/// A two-disc album whose songs all sit loose in its folder, as placement leaves a disc 2 song
/// that comes after its album's loose disc 1, is offered its disc folders: one row that moves
/// every song into its disc's folder.
#[test]
fn a_loose_two_disc_album_is_offered_disc_folders() {
    let files = vec![
        file(
            "Artist/Album/01 - One.flac",
            tags("Artist", "Album", "One", 1).disc(1, Some(2)),
        ),
        file(
            "Artist/Album/02 - Two.flac",
            tags("Artist", "Album", "Two", 2).disc(1, Some(2)),
        ),
        file(
            "Artist/Album/01 - Three.flac",
            tags("Artist", "Album", "Three", 1).disc(2, Some(2)),
        ),
    ];
    let plan = plan_in(
        &files,
        &[],
        custom("{Artist}/{Album}/Disc {Disc}/{Track:00} - {Title}"),
    );
    let mut moved = moves(&plan);
    moved.sort();
    assert_eq!(
        moved,
        [
            pair("Artist/Album/01 - One.flac", "Artist/Album/Disc 1/01 - One.flac"),
            pair(
                "Artist/Album/01 - Three.flac",
                "Artist/Album/Disc 2/01 - Three.flac"
            ),
            pair("Artist/Album/02 - Two.flac", "Artist/Album/Disc 1/02 - Two.flac"),
        ]
    );
    let rows = layout_rows(&plan);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, "Artist/Album");
    assert_eq!(rows[0].files, 3);
    assert!(rows[0].ticked && !rows[0].opt_in);
}

// ---- values -----------------------------------------------------------------------------------

/// A song as placement filed it: the values it named the file from, kept on its record with the
/// tags it wrote, and those tags on the file.
fn placed(song: &Song, requested: &RequestedIdentity, name_from_match: bool, layout: &Layout) -> LayoutFile {
    let mut values = BaseDownloadService::layout_values(song, requested, name_from_match);
    values.tags = Some(octo_core::common::TagSnapshot::of_song(song));
    let names = layout.render_names(&values, ".flac");
    LayoutFile {
        path: format!("{ROOT}/{}.flac", names.join("/")),
        size: 1000,
        in_scope: true,
        tags: Some(written(song)),
        mapping: Some(LayoutMapping {
            title: song.title.clone(),
            album: song.album.clone(),
            artist: song.artist.clone(),
            values: Some(values),
        }),
    }
}

/// The layout's tags as the tag writer writes them from `song`.
fn written(song: &Song) -> LayoutTags {
    LayoutTags {
        title: Some(song.title.clone()),
        performers: vec![song.artist.clone()],
        artists: Vec::new(),
        album: (!song.album.is_empty()).then(|| song.album.clone()),
        album_artist: song.album_artist.clone(),
        track: song.track,
        disc: song.disc_number,
        disc_count: song.total_discs,
        year: song_year(song),
        release_type: song.release_type.clone(),
        compilation: song.is_compilation,
        release_group: song.music_brainz_release_group_id.clone(),
    }
}

fn library() -> Vec<(Song, RequestedIdentity)> {
    let song = |artist: &str, album: &str, title: &str, track: i32| Song {
        artist: artist.to_string(),
        album: album.to_string(),
        title: title.to_string(),
        track: Some(track),
        year: Some(2018),
        album_artist: (!album.is_empty()).then(|| "Kendrick Lamar".to_string()),
        release_type: Some(if album.is_empty() { "single" } else { "album" }.to_string()),
        music_brainz_release_group_id: Some("rg-black-panther".to_string()),
        ..Song::default()
    };
    let mut stars = song(
        "Kendrick Lamar & SZA",
        "Black Panther The Album",
        "All The Stars",
        14,
    );
    stars.verification = Some(Box::new(VerificationResult {
        verdict: VerificationVerdict::Confirmed,
        r#match: Some(AcoustIdRecording::new(
            "rec-1",
            "All The Stars",
            ["Kendrick Lamar", "SZA"],
            Some("Black Panther The Album"),
            Some(2018),
        )),
        ..VerificationResult::default()
    }));
    let king = song("Kendrick Lamar", "Black Panther The Album", "King's Dead?", 4);
    let mut single = song("Kendrick Lamar", "", "The Heart Part 6", 1);
    single.music_brainz_release_group_id = None;
    vec![
        (
            stars,
            RequestedIdentity::new(
                "Kendrick Lamar, SZA",
                "All the Stars",
                "Black Panther The Album",
                Some(14),
            ),
        ),
        (
            king,
            RequestedIdentity::new(
                "Kendrick Lamar",
                "King's Dead?",
                "Black Panther The Album",
                Some(4),
            ),
        ),
        (
            single,
            RequestedIdentity::new("Kendrick Lamar", "The Heart Part 6", "", None),
        ),
    ]
}

#[test]
fn freshly_placed_songs_are_already_in_the_layout() {
    for layout in [
        preset(FolderStructure::Flat),
        preset(FolderStructure::ByArtist),
        preset(FolderStructure::Organized),
        custom("{Album Artist}/{Album} ({Year})/{Track:00} - {Title}"),
        custom("{Artist}/{Album} [{Album Type}]/{Track Artist} - {Title}"),
    ] {
        for name_from_match in [false, true] {
            let files: Vec<LayoutFile> = library()
                .iter()
                .map(|(song, requested)| placed(song, requested, name_from_match, &layout))
                .collect();
            let plan = plan_in(&files, &[], layout.clone());
            assert!(
                moves(&plan).is_empty(),
                "{:?} with NameFromMatch {name_from_match}: {:?}",
                layout.texts(),
                moves(&plan)
            );
            assert!(layout_rows(&plan).is_empty());
        }
    }
}

#[test]
fn a_tag_edit_after_placement_wins_over_saved_values() {
    let layout = custom("{Artist}/{Album} ({Year})/{Track:00} - {Title}");
    let (song, requested) = library().remove(1);
    let mut placed = placed(&song, &requested, false, &layout);
    assert_eq!(
        rel(&placed.path),
        "Kendrick Lamar/Black Panther The Album (2018)/04 - King's Dead？.flac"
    );
    let tags = placed.tags.as_mut().expect("tags");
    tags.title = Some("King's Dead (Remix)".to_string());
    tags.year = Some(2017);
    let plan = plan_in(std::slice::from_ref(&placed), &[], layout);
    assert_eq!(
        moves(&plan),
        [
            pair(
                "Kendrick Lamar/Black Panther The Album (2018)",
                "Kendrick Lamar/Black Panther The Album (2017)"
            ),
            pair(
                "Kendrick Lamar/Black Panther The Album (2017)/04 - King's Dead？.flac",
                "Kendrick Lamar/Black Panther The Album (2017)/04 - King's Dead (Remix).flac"
            ),
        ]
    );
}

#[test]
fn the_task_files_by_the_saved_kind() {
    // A single placement filed into its album's folder by the album template stays there.
    let layout = Layout::custom_or_preset(
        FolderStructure::Custom,
        "{Artist}/{Album}/{Track:00} - {Title}",
        "{Artist}/Singles/{Title}",
        "",
    );
    let (mut song, requested) = library().remove(1);
    song.release_type = Some("single".to_string());
    let mut file = placed(&song, &requested, false, &layout);
    let values = file
        .mapping
        .as_mut()
        .and_then(|m| m.values.as_mut())
        .expect("values");
    values.kind = LayoutKind::Album;
    file.path = at("Kendrick Lamar/Black Panther The Album/04 - King's Dead？.flac");
    let plan = plan_in(std::slice::from_ref(&file), &[], layout.clone());
    assert!(moves(&plan).is_empty(), "{:?}", moves(&plan));
    // Without the saved kind, its own type files it as a single.
    file.mapping = None;
    let plan = plan_in(std::slice::from_ref(&file), &[], layout);
    assert_eq!(
        moves(&plan),
        [
            pair("Kendrick Lamar/Black Panther The Album", "Kendrick Lamar/Singles"),
            pair(
                "Kendrick Lamar/Singles/04 - King's Dead？.flac",
                "Kendrick Lamar/Singles/King's Dead？.flac"
            ),
        ]
    );
}

/// A file placed by an Octo older than the saved values: named from the request.
fn older(relative: &str, tags: LayoutTags, record: (&str, &str, &str)) -> LayoutFile {
    let (artist, album, title) = record;
    LayoutFile {
        path: at(relative),
        size: 1000,
        in_scope: true,
        tags: Some(tags),
        mapping: Some(LayoutMapping {
            title: title.into(),
            album: album.into(),
            artist: artist.into(),
            values: None,
        }),
    }
}

#[test]
fn an_older_octo_library_already_in_its_layout_gives_no_rows() {
    let tags = || {
        tags(
            "Kendrick Lamar & SZA",
            "Black Panther The Album",
            "All The Stars",
            14,
        )
        .no_album_artist()
    };
    let record = ("Kendrick Lamar & SZA", "Black Panther The Album", "All The Stars");
    for (structure, path) in [
        (FolderStructure::Flat, "Kendrick Lamar, SZA - All The Stars.flac"),
        (FolderStructure::ByArtist, "Kendrick Lamar/All The Stars.flac"),
        (
            FolderStructure::Organized,
            "Kendrick Lamar/Black Panther The Album/14 - All The Stars.flac",
        ),
    ] {
        let files = vec![older(path, tags(), record)];
        let plan = plan_in(&files, &[], preset(structure));
        assert!(moves(&plan).is_empty(), "{structure:?}: {:?}", moves(&plan));
    }
}

#[test]
fn a_lidarr_shaped_name_never_reads_as_a_title() {
    let files = vec![older(
        "Daft Punk/Discovery/Daft Punk - Discovery - 01 - One More Time.flac",
        tags("Daft Punk", "Discovery", "One More Time", 1),
        ("Daft Punk", "Discovery", "One More Time"),
    )];
    let plan = plan_in(&files, &[], custom("{Artist}/{Album}/{Track:00}. {Title}"));
    assert_eq!(
        moves(&plan),
        [pair(
            "Daft Punk/Discovery/Daft Punk - Discovery - 01 - One More Time.flac",
            "Daft Punk/Discovery/01. One More Time.flac"
        )]
    );
}

// ---- the family pool --------------------------------------------------------------------------

#[test]
fn pool_files_move_one_by_one_as_pool_rows() {
    let files = discovery(&[
        ("01 - One More Time.flac", "One More Time", 1),
        ("02 - Aerodynamic.flac", "Aerodynamic", 2),
    ]);
    let mut options = LayoutOptions::new(custom("{Artist}/{Album} ({Year})/{Track:00} - {Title}"));
    options.pool = true;
    options.budget = octo_core::common::path_helper::POOL_BUDGET;
    let disk = disk_of(
        &files,
        &[
            "Daft Punk/Discovery/01 - One More Time.lrc",
            "Daft Punk/Discovery/cover.jpg",
        ],
    );
    let planned = as_pool_rows(plan(ROOT, &files, &disk, &options));

    // No folder is renamed in the pool, and its lyrics and covers go with each file there.
    assert!(planned.plan.moves.is_empty() && planned.plan.file_moves.is_empty());
    assert!(planned.plan.copies.is_empty());
    let moved: Vec<(String, String)> = planned
        .plan
        .pool_moves
        .iter()
        .map(|m| (rel(&m.from), rel(&m.to)))
        .collect();
    assert_eq!(
        moved,
        [
            pair(
                "Daft Punk/Discovery/01 - One More Time.flac",
                "Daft Punk/Discovery (2001)/01 - One More Time.flac"
            ),
            pair(
                "Daft Punk/Discovery/02 - Aerodynamic.flac",
                "Daft Punk/Discovery (2001)/02 - Aerodynamic.flac"
            ),
        ]
    );
    let row = planned
        .plan
        .rows
        .iter()
        .find(|row| row.kind == TidyKind::Layout)
        .expect("a row");
    assert_eq!(row.id, "pool|Daft Punk/Discovery");
    assert_eq!(row.path, "pool:Daft Punk/Discovery");
    assert!(planned.plan.pool_moves.iter().all(|m| m.row == row.id));
    assert!(planned.row_hashes.contains_key(&row.id));
    // Leave out names a pool folder as its row's path does.
    assert_eq!(
        pool_excluded(&["pool:Daft Punk/Discovery".to_string(), "Daft Punk".to_string()]),
        ["Daft Punk/Discovery"]
    );
}

// ---- Lidarr under a root Octo cannot see ------------------------------------------------------

#[test]
fn a_lidarr_root_octo_cannot_see_is_ignored() {
    let files = vec![file("A/X/01 - Song.flac", tags("A", "X", "Song", 1))];
    let answer = TidyLidarr {
        roots: vec!["/data/music".to_string(), "/data/other".to_string()],
        artists: Vec::new(),
        files: vec![("/data/other/B/Y/01 - Else.flac".to_string(), 1000)],
    };
    let view = lidarr_view(ROOT, Some("/data/music"), &answer, &files);
    assert!(view.tracked.is_empty() && view.maybe.is_empty());
}

#[test]
fn a_lidarr_candidate_under_an_unseen_root_stays() {
    let mut files = vec![
        file("A/X/01 - Song.flac", tags("A", "X", "Song", 1)),
        file("B/Y/01 - Else.flac", tags("B", "Y", "Else", 1)),
        file("C/Z/01 - Third.flac", tags("C", "Z", "Third", 1)),
    ];
    // Octo placed the second and third; the third's size differs from Lidarr's file.
    for at in [1, 2] {
        files[at].mapping = Some(LayoutMapping::default());
    }
    files[2].size = 5;
    let answer = TidyLidarr {
        roots: vec!["/data/music".to_string(), "/data/other".to_string()],
        artists: Vec::new(),
        files: vec![
            ("/data/other/A/X/01 - Song.flac".to_string(), 7),
            ("/data/other/B/Y/01 - Else.flac".to_string(), 1000),
            ("/data/other/C/Z/01 - Third.flac".to_string(), 1000),
        ],
    };
    let view = lidarr_view(ROOT, Some("/data/music"), &answer, &files);
    // Octo has no record of the first, and the second is the same size: both may be Lidarr's.
    assert!(view.maybe.contains(&at("A/X/01 - Song.flac")));
    assert!(view.maybe.contains(&at("B/Y/01 - Else.flac")));
    assert!(!view.maybe.contains(&at("C/Z/01 - Third.flac")));
}
