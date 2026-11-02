//! The family pool on a real folder: one file, links per member, the trash, better copies,
//! moving a song into the shared library, and copies where links do not work.

use std::path::Path;

use octo_core::models::domain::Song;
use serde_json::json;

use super::*;
use crate::services::family::family_playlist_store::FamilyPlaylistEntry;
use crate::services::family::test_support::{World, eventually, navidrome_with_songs, song_row, write_file};

const SONG: &str = "Artist/Album/01 Song.mp3";

fn song() -> Song {
    Song {
        title: "Song".into(),
        artist: "Artist".into(),
        album: "Album".into(),
        isrc: Some("USUM71703861".into()),
        ..Default::default()
    }
}

fn is_link(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

#[tokio::test]
async fn two_members_one_file() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 1_000);
    let alex = world.pool.link(&pooled, "alex").await.expect("linked");
    let sam = world.pool.link(&pooled, "Sam").await.expect("linked");
    assert_eq!(alex, format!("{}/alex/{SONG}", world.members()));
    assert_eq!(sam, format!("{}/sam/{SONG}", world.members()));
    assert!(is_link(&alex) && is_link(&sam));
    assert_eq!(std::fs::read(&alex).expect("reads through").len(), 1_000);
    assert_eq!(std::fs::read(&sam).expect("reads through").len(), 1_000);
    // One real audio file under the members folder: the pool's.
    let real: Vec<_> = walkdir(&world.members())
        .into_iter()
        .filter(|p| !is_link(p) && p.ends_with(".mp3"))
        .collect();
    assert_eq!(real, std::slice::from_ref(&pooled));
    assert_eq!(
        world.pool.store().get(&pooled).expect("recorded").refs,
        ["alex", "sam"]
    );
    assert_eq!(world.library.targeted(), [3, 4]);
    assert_eq!(world.index.placed_library_of(&alex), Some(3));

    // Linking again changes nothing.
    world.pool.link(&pooled, "alex").await.expect("still linked");
    assert_eq!(world.pool.store().get(&pooled).expect("recorded").refs.len(), 2);
}

#[tokio::test]
async fn a_member_who_kept_a_capitalized_account_name_links_into_the_lower_case_folder() {
    let world = World::new(None);
    world
        .family
        .upsert_member(crate::services::family::test_support::member(
            "Jamie",
            octo_core::family::abilities::Preset::Member,
            5,
        ));
    let pooled = world.pool_file(SONG, 1_000);
    let folder = format!("{}/jamie", world.members());

    let link = world.pool.link(&pooled, "JAMIE").await.expect("linked");
    assert_eq!(link, format!("{folder}/{SONG}"));
    assert!(is_link(&link));
    for spelling in ["Jamie", "jamie", " JAMIE "] {
        assert_eq!(world.pool.member_root(spelling), folder, "{spelling}");
        assert_eq!(
            world.pool.link_path(&pooled, spelling).as_deref(),
            Some(link.as_str())
        );
    }

    world.pool.let_go(&pooled, "jamie").await;
    assert!(std::fs::symlink_metadata(&link).is_err(), "the link is gone");
}

#[test]
fn a_folder_name_is_only_copied_when_it_has_capitals() {
    assert!(matches!(folder_of("alex"), Cow::Borrowed("alex")));
    assert!(matches!(folder_of(" alex "), Cow::Borrowed("alex")));
    assert_eq!(folder_of("Jamie.B"), "jamie.b");
    assert!(has_valid_folder("Jamie"));
    assert!(!has_valid_folder("Jamie/../x"));
}

#[tokio::test]
async fn link_is_relative() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    let link = world.pool.link(&pooled, "alex").await.expect("linked");
    let target = std::fs::read_link(&link).expect("a link");
    assert!(target.is_relative(), "{target:?}");
    assert_eq!(target.to_string_lossy(), format!("../../../.pool/{SONG}"));
    assert_eq!(relative_target("a.flac"), "../.pool/a.flac");
    // A relative link still works when the whole members folder moves.
    let moved = world.dir.path().join("moved");
    std::fs::rename(world.members(), &moved).expect("moved");
    assert_eq!(
        std::fs::read(moved.join("alex").join(SONG)).expect("reads").len(),
        10
    );
}

#[tokio::test]
async fn lyrics_and_cover_go_with_the_song() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    write_file(&pooled.replace(".mp3", ".lrc"), 3);
    write_file(
        &format!("{}/Artist/Album/cover.jpg", world.pool.pool_root_path()),
        3,
    );
    let link = world.pool.link(&pooled, "alex").await.expect("linked");
    assert!(is_link(&link.replace(".mp3", ".lrc")));
    assert!(is_link(&format!(
        "{}/alex/Artist/Album/cover.jpg",
        world.members()
    )));
    world.pool.unlink(&pooled, "alex").await.expect("unlinked");
    assert!(!Path::new(&format!("{}/alex/Artist", world.members())).exists());
}

#[tokio::test]
async fn remove_last_ref_trashes_file() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    world.library.map("test", "1", &pooled);
    world.pool.link(&pooled, "alex").await.expect("linked");
    world.pool.link(&pooled, "sam").await.expect("linked");

    world.pool.unlink(&pooled, "sam").await.expect("unlinked");
    assert!(Path::new(&pooled).is_file(), "Alex still has it");
    assert!(!Path::new(&format!("{}/sam/{SONG}", world.members())).exists());
    assert_eq!(world.pool.store().get(&pooled).expect("kept").refs, ["alex"]);

    world.pool.unlink(&pooled, "alex").await.expect("unlinked");
    assert!(!Path::new(&pooled).exists());
    assert!(world.pool.store().get(&pooled).is_none());
    let trashed: Vec<_> = walkdir(&format!("{}/.octo-trash", world.members()))
        .into_iter()
        .filter(|p| p.ends_with("01 Song.mp3"))
        .collect();
    assert_eq!(trashed.len(), 1, "in the trash, kept for the usual days");
    assert_eq!(
        world.library.forgotten.lock().as_slice(),
        std::slice::from_ref(&pooled)
    );
    assert!(!Path::new(&format!("{}/Artist", world.pool.pool_root_path())).exists());
}

#[tokio::test]
async fn copy_mode_when_links_fail() {
    let world = World::new(None);
    assert_eq!(world.pool.probe_links().links, LinkMode::Symlink);
    assert_eq!(world.pool.link_status().message, None);

    // A members folder that cannot be made (a file is in the way) cannot link either.
    let blocked = world.dir.path().join("blocked");
    std::fs::write(&blocked, b"x").expect("a file");
    let mut settings = world.settings.current().as_ref().clone();
    settings.family.members_path = blocked.join("members").to_string_lossy().into_owned();
    world.settings.set(settings);
    let status = world.pool.probe_links();
    assert_eq!(status.links, LinkMode::Copy);
    assert_eq!(status.message.as_deref(), Some(COPY_MODE_MESSAGE));
    assert!(
        status
            .fix
            .as_deref()
            .is_some_and(|fix| fix.contains("--vfs-links"))
    );
}

#[tokio::test]
async fn copies_replace_links_when_navidrome_never_shows_them() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    let link = world.pool.link(&pooled, "alex").await.expect("linked");
    assert!(!world.pool.links_seen());
    assert_eq!(world.pool.fall_back_to_copies().await, 1);
    assert!(!is_link(&link) && Path::new(&link).is_file());
    assert_eq!(std::fs::read(&link).expect("a copy").len(), 10);
    assert!(
        !Path::new(&format!("{link}.octo-copy")).exists(),
        "the copy took the link's place"
    );
    assert_eq!(world.pool.mode(), LinkMode::Copy);
    assert!(world.pool.store().link_state().copies, "kept for the next start");
    let other = world.pool_file("Artist/Album/02 Other.mp3", 5);
    let copy = world.pool.link(&other, "sam").await.expect("copied");
    assert!(!is_link(&copy) && Path::new(&copy).is_file());
    assert!(world.pool.links_seen(), "copy mode needs no links");
}

#[tokio::test]
async fn a_slow_scan_never_turns_everyones_links_into_copies() {
    let world = World::new(None);
    let first = world.pool_file(SONG, 10);
    let second = world.pool_file("Artist/Album/02 Other.mp3", 5);
    let alex = world.pool.link(&first, "alex").await.expect("linked");
    let sam = world.pool.link(&second, "sam").await.expect("linked");

    // Alex's link did not show in time: only that one link becomes a copy.
    assert!(world.pool.copy_unshown_link("alex", &alex).await);
    assert!(!is_link(&alex) && Path::new(&alex).is_file());
    assert_eq!(std::fs::read(&alex).expect("a copy").len(), 10);
    assert!(is_link(&sam), "Sam's link is left alone");
    assert_eq!(world.pool.mode(), LinkMode::Symlink);
    // Navidrome showed the copy: once is not enough to give up on links, and the same song
    // missed again still counts once.
    assert!(!world.pool.copy_shown_where_link_was_not(&first).await);
    assert!(!world.pool.copy_shown_where_link_was_not(&first).await);
    assert_eq!(world.pool.store().link_state().unshown, 1);
    assert_eq!(world.pool.mode(), LinkMode::Symlink);
    assert!(is_link(&sam));

    // A second song, with no link ever shown: links still work in the members folder, so
    // nobody's links become copies.
    assert!(world.pool.copy_unshown_link("sam", &sam).await);
    assert!(!world.pool.copy_shown_where_link_was_not(&second).await);
    assert_eq!(world.pool.store().link_state().unshown, 2);
    assert_eq!(world.pool.mode(), LinkMode::Symlink);
    assert!(!world.pool.store().link_state().copies);
}

#[tokio::test]
async fn everyone_gets_copies_only_when_the_members_folder_fails_a_links_probe() {
    let world = World::new(None);
    let first = world.pool_file(SONG, 10);
    let second = world.pool_file("Artist/Album/02 Other.mp3", 5);
    assert!(!world.pool.copy_shown_where_link_was_not(&first).await);
    // The members folder can no longer take links: a file is in the way of it.
    let blocked = world.dir.path().join("blocked");
    std::fs::write(&blocked, b"x").expect("a file");
    let mut settings = world.settings.current().as_ref().clone();
    settings.family.members_path = blocked.join("members").to_string_lossy().into_owned();
    world.settings.set(settings);
    assert!(world.pool.copy_shown_where_link_was_not(&second).await);
    assert_eq!(world.pool.mode(), LinkMode::Copy);
    assert!(world.pool.store().link_state().copies);
}

#[tokio::test]
async fn links_once_shown_are_never_given_up_and_that_survives_a_restart() {
    let world = World::new(None);
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = dir.path().join("family-pool.json");
    let store = std::sync::Arc::new(FamilyPoolStore::new(Some(&file)));
    let pool = pool_over(&world, store);
    let pooled = world.pool_file(SONG, 10);
    let link = pool.link(&pooled, "alex").await.expect("linked");
    pool.confirm_links_seen();
    assert!(!pool.copy_unshown_link("alex", &link).await, "a slow scan");
    assert!(is_link(&link));
    assert!(!pool.copy_shown_where_link_was_not(&pooled).await);

    let restarted = pool_over(&world, std::sync::Arc::new(FamilyPoolStore::new(Some(&file))));
    assert!(restarted.links_seen());
    assert_eq!(restarted.probe_links().links, LinkMode::Symlink);

    // Copies learned before a restart stay copies after it, whatever the disk can do.
    let copies = tempfile::tempdir().expect("a temp dir");
    let copies_file = copies.path().join("family-pool.json");
    let learned = pool_over(
        &world,
        std::sync::Arc::new(FamilyPoolStore::new(Some(&copies_file))),
    );
    learned.fall_back_to_copies().await;
    let restarted = pool_over(
        &world,
        std::sync::Arc::new(FamilyPoolStore::new(Some(&copies_file))),
    );
    assert_eq!(restarted.mode(), LinkMode::Copy);
    assert_eq!(restarted.probe_links().links, LinkMode::Copy);
}

/// Another pool over the world's folders and services, keeping its records in `store`.
fn pool_over(world: &World, store: std::sync::Arc<FamilyPoolStore>) -> std::sync::Arc<FamilyPool> {
    let shared = world.music();
    FamilyPool::with_timing(
        PoolParts {
            settings: world.settings.clone(),
            store,
            family: world.family.clone(),
            index: world.index.clone(),
            library: world.library.clone(),
            playlists: world.playlists.clone(),
            stars: world.stars.clone(),
            shared_root: Box::new(move || shared.clone()),
        },
        world.pool.timing(),
    )
}

#[tokio::test]
async fn upgrade_repoints_every_link() {
    let world = World::new(None);
    let mp3 = world.pool_file(SONG, 10);
    world.pool.record(PoolFile {
        path: mp3.clone(),
        size: 10,
        format: "mp3".into(),
        outside_ids: vec!["test:1".into()],
        isrc: Some("USUM71703861".into()),
        title: "Song".into(),
        artist: "Artist".into(),
        album: "Album".into(),
        refs: Vec::new(),
        transcoded_from: None,
    });
    let alex_old = world.pool.link(&mp3, "alex").await.expect("linked");
    let sam_old = world.pool.link(&mp3, "sam").await.expect("linked");
    let flac_rel = SONG.replace(".mp3", ".flac");
    let alex_new = format!("{}/alex/{flac_rel}", world.members());
    let sam_new = format!("{}/sam/{flac_rel}", world.members());

    let members = world.members();
    let navidrome = navidrome_with_songs(json!([
        song_row("old-alex", 3, &format!("{members}/alex"), SONG, 10),
        song_row("old-sam", 4, &format!("{members}/sam"), SONG, 10),
        song_row("new-alex", 3, &format!("{members}/alex"), &flac_rel, 30),
        song_row("new-sam", 4, &format!("{members}/sam"), &flac_rel, 30),
    ]))
    .await;
    world.point_at(&navidrome.uri());
    world.index.refresh().await.expect("read");
    world
        .stars
        .starred
        .lock()
        .insert(("alex".into(), "old-alex".into()));
    let road_trip = world.playlists.create("Road trip", "sam", chrono::Utc::now());
    world.playlists.add_entries(
        &road_trip.id,
        vec![FamilyPlaylistEntry {
            library_id: Some("old-sam".into()),
            scope: "Personal:sam".into(),
            outside_id: None,
            added_by: "sam".into(),
            added: chrono::Utc::now(),
            pending: false,
            duration: None,
            explicit_status: None,
        }],
    );

    let flac = world.pool_file(&flac_rel, 30);
    let first = world.pool.placed(&flac, &song(), "test", "1", &[], false).await;
    assert_eq!(first, None, "nobody new asked");

    assert!(is_link(&alex_new) && is_link(&sam_new));
    assert!(!Path::new(&alex_old).exists() && !Path::new(&sam_old).exists());
    assert_eq!(std::fs::read(&alex_new).expect("reads").len(), 30);
    assert!(!Path::new(&mp3).exists(), "the old copy went to the trash");
    let better = world.pool.store().get(&flac).expect("recorded");
    assert_eq!(better.refs, ["alex", "sam"]);
    assert!(better.outside_ids.contains(&"test:1".to_string()));
    assert!(world.pool.store().get(&mp3).is_none());

    assert!(
        eventually(|| world
            .stars
            .asked
            .lock()
            .contains(&("alex".into(), "new-alex".into())))
        .await,
        "Alex's star follows the song"
    );
    assert!(!world.stars.asked.lock().iter().any(|(user, _)| user == "sam"));
    assert!(
        eventually(|| {
            world.playlists.get(&road_trip.id).expect("there").entries[0]
                .library_id
                .as_deref()
                == Some("new-sam")
        })
        .await
    );
}

#[tokio::test]
async fn promote_removes_links() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    write_file(&pooled.replace(".mp3", ".lrc"), 3);
    world.pool.link(&pooled, "alex").await.expect("linked");
    world.pool.link(&pooled, "sam").await.expect("linked");

    let shared = world.pool.promote(&pooled).await.expect("moved");
    assert_eq!(shared, format!("{}/{SONG}", world.music()));
    assert_eq!(std::fs::read(&shared).expect("there").len(), 10);
    assert!(
        Path::new(&shared.replace(".mp3", ".lrc")).is_file(),
        "lyrics went with it"
    );
    assert!(!Path::new(&pooled).exists());
    for user in ["alex", "sam"] {
        assert!(!Path::new(&format!("{}/{user}/Artist", world.members())).exists());
    }
    assert!(world.pool.store().list().is_empty());
    assert_eq!(
        world.library.restored.lock().as_slice(),
        std::slice::from_ref(&shared)
    );
    assert_eq!(world.library.full_scans(), 1);
}

#[tokio::test]
async fn settle_links_members_and_moves_a_shared_ask() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    let kept = world
        .pool
        .settle(&pooled, &["alex".to_string()], false, None)
        .await;
    assert_eq!(kept, pooled);
    assert!(is_link(&format!("{}/alex/{SONG}", world.members())));
    let moved = world
        .pool
        .settle(&pooled, &["alex".to_string()], true, None)
        .await;
    assert_eq!(moved, format!("{}/{SONG}", world.music()));
    // A path outside the pool is left alone.
    assert_eq!(
        world.pool.settle(&moved, &["sam".to_string()], false, None).await,
        moved
    );
}

#[tokio::test]
async fn songs_are_found_by_outside_id_isrc_or_name() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    world.pool.placed(&pooled, &song(), "test", "1", &[], false).await;
    assert_eq!(
        world
            .pool
            .find_song("test", "1", &Song::default())
            .await
            .map(|f| f.path),
        Some(pooled.clone())
    );
    let by_isrc = Song {
        isrc: Some("usum71703861".into()),
        ..Default::default()
    };
    assert!(world.pool.find_song("test", "2", &by_isrc).await.is_some());
    let by_name = Song {
        artist: "artist".into(),
        title: "SONG".into(),
        ..Default::default()
    };
    assert!(world.pool.find_song("test", "3", &by_name).await.is_some());
    let live = Song {
        artist: "Artist".into(),
        title: "Song (Live)".into(),
        ..Default::default()
    };
    assert!(world.pool.find_song("test", "4", &live).await.is_none());
}

#[tokio::test]
async fn family_off_places_nothing() {
    let world = World::with_family(None, false);
    assert_eq!(world.pool.pool_root(), None);
    assert!(
        !world
            .pool
            .is_pool_path(&format!("{}/.pool/{SONG}", world.members()))
    );
    assert!(world.pool.find_song("test", "1", &song()).await.is_none());
}

/// Every file and link under `root`.
fn walkdir(root: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(root)];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let kind = entry.file_type().expect("a kind");
            if kind.is_dir() {
                stack.push(path);
            } else {
                found.push(path.to_string_lossy().into_owned());
            }
        }
    }
    found.sort();
    found
}

#[tokio::test]
async fn a_pool_file_deleted_by_hand_takes_its_links_with_it() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    world.library.map("test", "1", &pooled);
    let link = world.pool.link(&pooled, "alex").await.expect("linked");
    // Something else stays in the pool, so the pool folder is plainly there.
    world.pool_file("Other/Album/01 Other.mp3", 5);
    std::fs::remove_file(&pooled).expect("deleted by hand");
    assert_eq!(world.pool.sweep_dangling().await, 0, "missing once is not enough");
    assert!(world.pool.store().get(&pooled).is_some());
    world.later(11);
    assert_eq!(world.pool.sweep_dangling().await, 1, "missing ten minutes apart");
    assert!(
        std::fs::symlink_metadata(&link).is_err(),
        "the dangling link went"
    );
    assert!(world.pool.store().list().is_empty());
    assert_eq!(world.library.forgotten.lock().as_slice(), [pooled]);
    world.later(11);
    assert_eq!(world.pool.sweep_dangling().await, 0);
}

#[tokio::test]
async fn sweeps_run_at_most_every_ten_minutes_and_a_file_goes_only_after_ten_minutes_missing() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    world.pool_file("Other/Album/01 Other.mp3", 5);
    world.pool.link(&pooled, "alex").await.expect("linked");
    std::fs::remove_file(&pooled).expect("deleted by hand");
    // Placements come seconds apart: only the first one sweeps.
    assert_eq!(world.pool.sweep_dangling().await, 0, "the first miss");
    world.later(1);
    assert_eq!(world.pool.sweep_dangling().await, 0, "no sweep a minute later");
    world.later(8);
    assert_eq!(world.pool.sweep_dangling().await, 0, "no sweep nine minutes in");
    assert_eq!(world.pool.store().get(&pooled).expect("kept").refs, ["alex"]);
    world.later(2);
    assert_eq!(
        world.pool.sweep_dangling().await,
        1,
        "missing eleven minutes apart"
    );
    assert!(world.pool.store().get(&pooled).is_none());
}

#[tokio::test]
async fn a_pool_folder_without_its_marker_is_never_swept_and_takes_no_member_download() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    world.pool.link(&pooled, "alex").await.expect("linked");
    assert!(world.pool.pool_ready());
    assert!(world.pool.placement_ready().await);
    let marker = format!("{}/{POOL_MARKER}", world.pool.pool_root_path());
    std::fs::remove_file(&marker).expect("the marker went");
    assert!(
        !world.pool.placement_ready().await,
        "no member download lands here"
    );
    std::fs::remove_file(&pooled).expect("gone");
    for _ in 0..3 {
        assert_eq!(world.pool.sweep_dangling().await, 0);
        world.later(11);
    }
    assert_eq!(world.pool.store().get(&pooled).expect("kept").refs, ["alex"]);
    // Records with none of their files there: the marker is not written back.
    assert!(!world.pool.keep_marker());
    assert!(!Path::new(&marker).exists());
    // A recorded file is there: the folder is the pool, and the marker is written back.
    world.pool_file(SONG, 10);
    assert!(world.pool.keep_marker());
    assert!(Path::new(&marker).is_file());
    assert!(world.pool.placement_ready().await);
}

#[tokio::test]
async fn a_pool_with_no_records_gets_its_marker_and_family_off_is_always_ready() {
    let world = World::new(None);
    let marker = format!("{}/{POOL_MARKER}", world.pool.pool_root_path());
    std::fs::remove_file(&marker).expect("the marker went");
    assert!(world.pool.keep_marker(), "nothing recorded, nothing to forget");
    assert!(Path::new(&marker).is_file());
    let off = World::with_family(None, false);
    std::fs::remove_file(format!("{}/{POOL_MARKER}", off.pool.pool_root_path())).expect("gone");
    assert!(off.pool.placement_ready().await);
}

#[tokio::test]
async fn a_file_back_before_the_second_sweep_is_kept() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    world.pool_file("Other/Album/01 Other.mp3", 5);
    world.pool.link(&pooled, "alex").await.expect("linked");
    let aside = format!("{pooled}.aside");
    std::fs::rename(&pooled, &aside).expect("gone for a moment");
    assert_eq!(world.pool.sweep_dangling().await, 0);
    std::fs::rename(&aside, &pooled).expect("back");
    world.later(11);
    assert_eq!(world.pool.sweep_dangling().await, 0);
    world.later(11);
    assert_eq!(world.pool.sweep_dangling().await, 0);
    assert_eq!(world.pool.store().get(&pooled).expect("kept").refs, ["alex"]);
}

#[tokio::test]
async fn a_members_disk_that_is_gone_wipes_no_pool_records() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    let link = world.pool.link(&pooled, "alex").await.expect("linked");
    // The disk is not mounted: the pool folder is missing altogether.
    let moved = world.dir.path().join("unmounted");
    std::fs::rename(world.pool.pool_root_path(), &moved).expect("moved away");
    for _ in 0..3 {
        assert_eq!(world.pool.sweep_dangling().await, 0);
        world.later(11);
    }
    // The mount point is there but empty.
    std::fs::create_dir_all(world.pool.pool_root_path()).expect("an empty mount point");
    for _ in 0..3 {
        assert_eq!(world.pool.sweep_dangling().await, 0);
        world.later(11);
    }
    assert_eq!(world.pool.store().get(&pooled).expect("kept").refs, ["alex"]);
    assert!(std::fs::symlink_metadata(&link).is_ok(), "Alex's link is kept");
    assert!(world.library.forgotten.lock().is_empty());
}

#[tokio::test]
async fn unrecorded_file_is_not_trashed_while_another_member_links_it() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    world.pool.link(&pooled, "alex").await.expect("linked");
    let sam = world.pool.link(&pooled, "sam").await.expect("linked");
    // The record is gone (family-pool.json read back empty after a crash).
    assert!(world.pool.store().remove(&pooled));
    world.pool.unlink(&pooled, "alex").await.expect("unlinked");
    assert!(Path::new(&pooled).is_file(), "Sam still links it");
    assert_eq!(std::fs::read(&sam).expect("reads through").len(), 10);
    assert_eq!(
        world.pool.store().get(&pooled).expect("recorded again").refs,
        ["sam"]
    );
    // Once Sam lets it go too, it goes to the trash as usual.
    world.pool.unlink(&pooled, "sam").await.expect("unlinked");
    assert!(!Path::new(&pooled).exists());
}

#[tokio::test]
async fn member_named_like_the_pool_folder_never_gets_a_library() {
    use crate::services::family::family_service::valid_username;
    for name in [".pool", "..", ".kept", ".octo-trash", "-x", "_x"] {
        assert!(!valid_username(name), "{name}");
    }
    assert!(valid_username("alex") && valid_username("dj.k-9_x"));

    let world = World::new(None);
    // A name the store holds anyway (an old or hand-edited family.json).
    world
        .family
        .upsert_member(crate::services::family::test_support::member(
            ".pool",
            octo_core::family::abilities::Preset::Member,
            9,
        ));
    let pooled = world.pool_file(SONG, 10);
    world.pool.link(&pooled, "alex").await.expect("linked");
    assert!(matches!(
        world.pool.link(&pooled, ".pool").await,
        Err(PoolError::NotMember(_))
    ));
    assert!(matches!(
        world.pool.unlink(&pooled, ".pool").await,
        Err(PoolError::NotMember(_))
    ));
    assert!(
        world
            .pool
            .release_member(".pool", FilesChoice::Delete)
            .await
            .is_err()
    );
    assert!(Path::new(&pooled).is_file(), "Alex still has it");
    assert_eq!(world.pool.store().get(&pooled).expect("kept").refs, ["alex"]);
}

#[tokio::test]
async fn promote_racing_a_link_never_leaves_a_link_to_a_moved_file() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    world.pool.link(&pooled, "alex").await.expect("linked");
    // Promote waits on Navidrome for Alex's stars, the moment another change could slip in.
    *world.stars.delay.lock() = std::time::Duration::from_millis(150);
    let promoting = {
        let pool = world.pool.clone();
        let pooled = pooled.clone();
        tokio::spawn(async move { pool.promote(&pooled).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let linked = world.pool.link(&pooled, "sam").await;
    let shared = promoting.await.expect("joins").expect("moved");
    assert!(Path::new(&shared).is_file());
    assert!(
        matches!(linked, Err(PoolError::NotInPool(_))),
        "the link waited for the move and found the file gone: {linked:?}"
    );
    let sam = format!("{}/sam/{SONG}", world.members());
    assert!(
        std::fs::symlink_metadata(&sam).is_err(),
        "no link to a file that moved"
    );
    assert!(world.pool.store().list().is_empty());
}

#[tokio::test]
async fn promote_keeps_the_pool_file_for_a_member_who_cannot_see_the_shared_library() {
    let world = World::new(None);
    let navidrome = navidrome_with_songs(json!([song_row(
        "shared-1",
        1,
        &world.music(),
        "Some/Album/01 Shared.mp3",
        10
    )]))
    .await;
    world.point_at(&navidrome.uri());
    world.index.refresh().await.expect("read");
    let mut sam = world.family.member("sam").expect("sam");
    sam.shared_libraries = crate::services::family::SharedLibraries::Ids(vec![2]);
    world.family.upsert_member(sam);
    let pooled = world.pool_file(SONG, 10);
    world.pool.link(&pooled, "alex").await.expect("linked");
    let sam_link = world.pool.link(&pooled, "sam").await.expect("linked");

    let shared = world.pool.promote(&pooled).await.expect("in the shared library");
    assert_eq!(std::fs::read(&shared).expect("there").len(), 10);
    assert!(Path::new(&pooled).is_file(), "kept in the pool for Sam");
    assert_eq!(std::fs::read(&sam_link).expect("Sam still has it").len(), 10);
    assert!(!Path::new(&format!("{}/alex/{SONG}", world.members())).exists());
    assert_eq!(world.pool.store().get(&pooled).expect("kept").refs, ["sam"]);
}

#[tokio::test]
async fn a_failed_promote_while_letting_a_member_go_leaves_their_file_as_it_was() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    let other = world.pool_file("Other/Album/01 Other.mp3", 5);
    let link = world.pool.link(&pooled, "alex").await.expect("linked");
    world.pool.link(&other, "alex").await.expect("linked");
    // The shared library cannot take this artist: a file is in the way of its folder.
    std::fs::write(format!("{}/Artist", world.music()), b"in the way").expect("a file");

    let released = world.pool.release_member("alex", FilesChoice::Promote).await;
    assert!(released.is_err());
    assert!(Path::new(&pooled).is_file(), "the song is where it was");
    assert_eq!(std::fs::read(&link).expect("still linked").len(), 10);
    assert_eq!(world.pool.store().get(&pooled).expect("recorded").refs, ["alex"]);
    // The other file did move: nothing is left without a record.
    assert!(world.pool.store().get(&other).is_none());
    assert!(Path::new(&format!("{}/Other/Album/01 Other.mp3", world.music())).is_file());
    for file in world.pool.store().list() {
        assert!(!file.refs.is_empty(), "{} has nobody", file.path);
    }
}

/// The members disk is not mounted, and a download lands on its empty mount point: the pool
/// folder can be read and is not empty, but it has no marker file, so no sweep forgets the
/// files on the real disk, however far apart the sweeps are.
#[tokio::test]
async fn a_download_onto_an_unmounted_members_disk_keeps_the_pool_records() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    world.pool.link(&pooled, "alex").await.expect("linked");
    let moved = world.dir.path().join("unmounted");
    std::fs::rename(world.pool.pool_root_path(), &moved).expect("moved away");
    // A new download lands on the mount point.
    world.pool_file("New/Album/01 New.mp3", 5);
    world.pool.sweep_dangling().await;
    world.later(11);
    world.pool.sweep_dangling().await;
    assert_eq!(
        world.pool.store().get(&pooled).map(|f| f.refs),
        Some(vec!["alex".to_string()]),
        "the record of a file on the unmounted disk went"
    );
}

/// A shared file outside the pool, for a member who cannot see its library to get a copy of.
fn shared_file(world: &World, relative: &str, bytes: usize) -> String {
    let path = format!("{}/{relative}", world.music());
    write_file(&path, bytes);
    path
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_copy_does_not_block_another_members_link() {
    let world = World::new(None);
    let shared = shared_file(&world, "Shared/Album/01 Shared.mp3", 10);
    let pooled = world.pool_file(SONG, 10);
    world.pool.slow_copies(std::time::Duration::from_millis(800));
    let copying = {
        let (pool, shared) = (world.pool.clone(), shared.clone());
        tokio::spawn(async move { pool.copy_shared_for(&shared, "sam", None, None).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let started = std::time::Instant::now();
    let link = world.pool.link(&pooled, "alex").await.expect("linked");
    let took = started.elapsed();
    assert!(
        took < std::time::Duration::from_millis(400),
        "Alex's link waited {took:?} for Sam's copy"
    );
    assert!(!copying.is_finished(), "the copy was still going");
    assert!(is_link(&link));
    let copy = copying.await.expect("joins").expect("copied");
    assert_eq!(std::fs::read(&copy).expect("Sam's copy").len(), 10);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_promote_and_a_link_on_different_files_run_side_by_side() {
    let world = World::new(None);
    let promoted = world.pool_file(SONG, 10);
    let other = world.pool_file("Other/Album/01 Other.mp3", 5);
    world.pool.link(&promoted, "alex").await.expect("linked");
    // Sam cannot see the shared library, so the promote copies the file, slowly.
    let mut sam = world.family.member("sam").expect("sam");
    sam.shared_libraries = crate::services::family::SharedLibraries::Ids(vec![2]);
    world.family.upsert_member(sam);
    world.pool.link(&promoted, "sam").await.expect("linked");
    world.pool.slow_copies(std::time::Duration::from_millis(800));
    let promoting = {
        let (pool, promoted) = (world.pool.clone(), promoted.clone());
        tokio::spawn(async move { pool.promote(&promoted).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let started = std::time::Instant::now();
    let link = world.pool.link(&other, "alex").await.expect("linked");
    let took = started.elapsed();
    assert!(
        took < std::time::Duration::from_millis(400),
        "a link to another file waited {took:?} for the promote"
    );
    assert!(!promoting.is_finished(), "the promote was still copying");
    assert!(is_link(&link));
    let shared = promoting.await.expect("joins").expect("promoted");
    assert_eq!(std::fs::read(&shared).expect("in the shared library").len(), 10);
    assert_eq!(world.pool.store().get(&promoted).expect("kept").refs, ["sam"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_promote_and_a_remove_on_the_same_file_take_turns() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    world.pool.link(&pooled, "alex").await.expect("linked");
    let mut sam = world.family.member("sam").expect("sam");
    sam.shared_libraries = crate::services::family::SharedLibraries::Ids(vec![2]);
    world.family.upsert_member(sam);
    let sam_link = world.pool.link(&pooled, "sam").await.expect("linked");
    // The promote copies the file into the shared library for Alex and keeps it for Sam,
    // slowly; Sam lets it go meanwhile.
    world.pool.slow_copies(std::time::Duration::from_millis(500));
    let promoting = {
        let (pool, pooled) = (world.pool.clone(), pooled.clone());
        tokio::spawn(async move { pool.promote(&pooled).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let removing = {
        let (pool, pooled) = (world.pool.clone(), pooled.clone());
        tokio::spawn(async move { pool.unlink(&pooled, "sam").await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!removing.is_finished(), "the remove waits for the promote");
    let shared = promoting.await.expect("joins").expect("promoted");
    removing.await.expect("joins").expect("unlinked");
    // The promote finished first: the shared library has the song, and Sam's remove then
    // found the pool file only Sam had and sent it to the trash.
    assert_eq!(std::fs::read(&shared).expect("in the shared library").len(), 10);
    assert!(!Path::new(&pooled).exists(), "nobody has the pool file");
    assert!(std::fs::symlink_metadata(&sam_link).is_err());
    assert!(!Path::new(&format!("{}/alex/{SONG}", world.members())).exists());
    assert!(world.pool.store().list().is_empty());
}

#[tokio::test]
async fn a_download_that_lands_for_a_member_who_left_is_let_go_of() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    // Alex left the family while the download was on its way, so the placement links nobody.
    world.family.remove_member("alex");
    assert_eq!(
        world
            .pool
            .placed(&pooled, &song(), "test", "1", &["alex".to_string()], false)
            .await,
        None
    );
    assert!(world.pool.store().get(&pooled).expect("recorded").refs.is_empty());
    world.pool.let_go(&pooled, "alex").await;
    assert!(!Path::new(&pooled).exists(), "in the trash");
    assert!(world.pool.store().list().is_empty());

    // Someone else still has another song: letting go of it for Alex leaves it for Sam.
    let shared = world.pool_file("Other/Album/01 Other.mp3", 5);
    world.pool.link(&shared, "sam").await.expect("linked");
    world.pool.let_go(&shared, "alex").await;
    assert!(Path::new(&shared).is_file());
    assert_eq!(world.pool.store().get(&shared).expect("kept").refs, ["sam"]);
}

// ---- Names made safe, folders joined, and the File layout task's moves -----------------------

/// A FLAC at `path` tagged with `artist`, `album` and `title`.
fn tagged_flac(path: &str, artist: &str, album: &str, title: &str) {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent).expect("folders");
    }
    std::fs::write(path, crate::services::test_support::flac()).expect("written");
    let mut file = octo_media::tags::TagFile::open(Path::new(path)).expect("opens");
    file.set_performers(&[artist.to_string()]);
    file.set_album(Some(album));
    file.set_title(Some(title));
    file.save().expect("saved");
}

/// The names in `folder`, sorted.
fn names_in(folder: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// A pool path an older rule made, with characters Windows refuses and a folder ending in a dot,
/// lands in the shared library under the names the layout makes now, its lyrics beside it under
/// the song's new name.
#[cfg(unix)]
#[tokio::test]
async fn promote_cleans_an_old_rule_relative_path() {
    let world = World::new(None);
    let pooled = world.pool_file("Kendrick Lamar/DAMN./03 - Are You Ready?.mp3", 10);
    write_file(&pooled.replace(".mp3", ".lrc"), 3);
    world.pool.link(&pooled, "alex").await.expect("linked");

    let shared = world.pool.promote(&pooled).await.expect("moved");

    let expected = format!(
        "{}/Kendrick Lamar/DAMN/03 - Are You Ready\u{FF1F}.mp3",
        world.music()
    );
    assert_eq!(shared, expected);
    assert_eq!(std::fs::read(&shared).expect("there").len(), 10);
    assert!(
        Path::new(&expected.replace(".mp3", ".lrc")).is_file(),
        "the lyrics took the song's new name"
    );
    assert_eq!(names_in(&format!("{}/Kendrick Lamar", world.music())), ["DAMN"]);
    assert!(!Path::new(&pooled).exists());
}

/// The shared library already has the song's folders in other capitals: the song joins them
/// rather than making twins a Windows share shows as one.
#[tokio::test]
async fn promote_joins_an_equivalent_folder() {
    let world = World::new(None);
    write_file(&format!("{}/artist/album/02 Other.mp3", world.music()), 5);
    let pooled = world.pool_file(SONG, 10);
    world.pool.link(&pooled, "alex").await.expect("linked");

    let shared = world.pool.promote(&pooled).await.expect("moved");

    assert_eq!(shared, format!("{}/artist/album/01 Song.mp3", world.music()));
    // The staging folder (where a copy keeps its record while it runs) is Octo's own.
    let shown: Vec<String> = names_in(&world.music())
        .into_iter()
        .filter(|name| !name.starts_with('.'))
        .collect();
    assert_eq!(shown, ["artist"]);
    assert_eq!(
        names_in(&format!("{}/artist/album", world.music())),
        ["01 Song.mp3", "02 Other.mp3"]
    );
}

/// The shared library keeps the song's album in a folder of another name: under Organized, the
/// song joins the folder that holds its album, found by the album's tags, named by the layout.
#[tokio::test]
async fn promote_joins_the_album_folder_by_another_name() {
    let world = World::new(None);
    let mut settings = world.settings.current().as_ref().clone();
    settings.subsonic.folder_structure = FolderStructure::Organized;
    world.settings.set(settings);
    tagged_flac(
        &format!("{}/Artist/Album (2004)/02 Other.flac", world.music()),
        "Artist",
        "Album",
        "Other",
    );
    let pooled = format!("{}/Artist/Album/01 Song.flac", world.pool.pool_root_path());
    tagged_flac(&pooled, "Artist", "Album", "Song");
    let mut numbered = octo_media::tags::TagFile::open(Path::new(&pooled)).expect("opens");
    numbered.set_track(1);
    numbered.save().expect("saved");
    world.pool.link(&pooled, "alex").await.expect("linked");

    let shared = world.pool.promote(&pooled).await.expect("moved");

    // Named by the layout from its tags, as a new song is.
    assert_eq!(
        shared,
        format!("{}/Artist/Album (2004)/01 - Song.flac", world.music())
    );
    assert_eq!(names_in(&format!("{}/Artist", world.music())), ["Album (2004)"]);
}

/// A shared file's pool copy takes the names the layout makes now, and the next member who needs
/// it gets that same copy, found by its record. A pool file of another song where a copy would
/// go is never taken for the copy.
#[cfg(unix)]
#[tokio::test]
async fn copy_shared_for_cleans_and_finds_the_copy_by_record() {
    let world = World::new(None);
    let shared = shared_file(&world, "Artist/Album?/01 Song?.mp3", 10);
    let pool = world.pool.pool_root_path();

    let sam = world
        .pool
        .copy_shared_for(&shared, "sam", None, None)
        .await
        .expect("copied");
    let copy = format!("{pool}/Artist/Album\u{FF1F}/01 Song\u{FF1F}.mp3");
    assert_eq!(
        sam,
        format!("{}/sam/Artist/Album\u{FF1F}/01 Song\u{FF1F}.mp3", world.members())
    );
    assert_eq!(std::fs::read(&copy).expect("the copy").len(), 10);

    let alex = world
        .pool
        .copy_shared_for(&shared, "alex", None, None)
        .await
        .expect("linked");
    assert_eq!(
        alex,
        format!(
            "{}/alex/Artist/Album\u{FF1F}/01 Song\u{FF1F}.mp3",
            world.members()
        )
    );
    assert_eq!(
        names_in(&format!("{pool}/Artist/Album\u{FF1F}")),
        ["01 Song\u{FF1F}.mp3"]
    );
    assert_eq!(
        world.pool.store().get(&copy).expect("recorded").refs,
        ["sam", "alex"]
    );

    // Another song already sits where this shared file's copy would go.
    let other = world.pool_file("Other/Album/01 Song.mp3", 3);
    let shared_other = shared_file(&world, "Other/Album/01 Song.mp3", 7);
    let link = world
        .pool
        .copy_shared_for(&shared_other, "sam", Some("test:7"), None)
        .await
        .expect("copied");
    assert_eq!(
        link,
        format!("{}/sam/Other/Album/01 Song (1).mp3", world.members())
    );
    assert_eq!(std::fs::read(&link).expect("Sam's song").len(), 7);
    assert_eq!(std::fs::read(&other).expect("left alone").len(), 3);
}

// ---- Promotions and shared copies filed by their tags, with the album's disc folders --------

/// `world` filing every kind by disc folders.
fn by_discs(world: &World) {
    let mut settings = world.settings.current().as_ref().clone();
    settings.subsonic.folder_structure = FolderStructure::Custom;
    settings.subsonic.folder_template = "{Artist}/{Album}/Disc {Disc}/{Track:00} - {Title}".into();
    world.settings.set(settings);
}

/// A FLAC at `path`, tagged as track `track` of disc `disc` of Pink Floyd's The Wall, of two.
fn wall_flac(path: &str, title: &str, track: u32, disc: u32) {
    tagged_flac(path, "Pink Floyd", "The Wall", title);
    let mut file = octo_media::tags::TagFile::open(Path::new(path)).expect("opens");
    file.set_track(track);
    file.set_disc(disc);
    file.set_disc_count(2);
    file.save().expect("saved");
}

/// A pool song of disc 2 promoted into a shared album whose disc 1 lies loose and is the
/// person's own: it lands loose beside it, so the album keeps one shape.
#[tokio::test]
async fn promote_lands_loose_beside_your_own_loose_disc_one() {
    let world = World::new(None);
    by_discs(&world);
    let theirs = format!("{}/Pink Floyd/The Wall/01 - In the Flesh.flac", world.music());
    wall_flac(&theirs, "In the Flesh", 1, 1);
    let pooled = in_pool(&world, "Pink Floyd/The Wall/Disc 2/01 - Hey You.flac");
    wall_flac(&pooled, "Hey You", 1, 2);
    world.pool.link(&pooled, "alex").await.expect("linked");

    let shared = world.pool.promote(&pooled).await.expect("moved");

    assert_eq!(
        shared,
        format!("{}/Pink Floyd/The Wall/01 - Hey You.flac", world.music())
    );
    assert!(Path::new(&theirs).is_file());
}

/// Another pool over the world's folders and services, with `library` as its library service and
/// a store of its own.
fn pool_with_library(
    world: &World,
    library: &std::sync::Arc<crate::services::local::test_support::FakeLocalLibrary>,
) -> (std::sync::Arc<FamilyPool>, std::sync::Arc<FamilyPoolStore>) {
    let store = std::sync::Arc::new(FamilyPoolStore::in_memory());
    let music = world.music();
    let pool = FamilyPool::with_timing(
        PoolParts {
            settings: world.settings.clone(),
            store: store.clone(),
            family: world.family.clone(),
            index: world.index.clone(),
            library: library.clone(),
            playlists: world.playlists.clone(),
            stars: world.stars.clone(),
            shared_root: Box::new(move || music.clone()),
        },
        world.pool.timing(),
    );
    (pool, store)
}

/// A pool song of disc 2 promoted into a shared album whose disc 1 Octo placed loose lands loose
/// beside it too: the album keeps one folder, and the loose song stays where it is.
#[tokio::test]
async fn promote_lands_loose_beside_octos_own_loose_disc_one() {
    let world = World::new(None);
    by_discs(&world);
    let loose = format!("{}/Pink Floyd/The Wall/01 - In the Flesh.flac", world.music());
    wall_flac(&loose, "In the Flesh", 1, 1);
    let library = std::sync::Arc::new(crate::services::local::test_support::FakeLocalLibrary::default());
    library.by_tags.lock().push((
        "Pink Floyd".into(),
        "In the Flesh".into(),
        None,
        crate::services::local::LocalSongMapping {
            local_path: loose.clone(),
            ..Default::default()
        },
    ));
    let (pool, _) = pool_with_library(&world, &library);
    let pooled = in_pool(&world, "Pink Floyd/The Wall/Disc 2/01 - Hey You.flac");
    wall_flac(&pooled, "Hey You", 1, 2);
    pool.link(&pooled, "alex").await.expect("linked");

    let shared = pool.promote(&pooled).await.expect("moved");

    assert_eq!(
        shared,
        format!("{}/Pink Floyd/The Wall/01 - Hey You.flac", world.music())
    );
    assert!(Path::new(&loose).is_file(), "the loose song stays where it is");
    assert_eq!(
        names_in(&format!("{}/Pink Floyd/The Wall", world.music())),
        ["01 - Hey You.flac", "01 - In the Flesh.flac"],
        "one folder, no disc folder"
    );
}

/// A pool song with no disc folder of its own promoted into a shared album filed by disc: it
/// joins its disc's folder.
#[tokio::test]
async fn promote_joins_the_albums_disc_folder() {
    let world = World::new(None);
    by_discs(&world);
    let music = world.music();
    wall_flac(
        &format!("{music}/Pink Floyd/The Wall/Disc 1/01 - In the Flesh.flac"),
        "In the Flesh",
        1,
        1,
    );
    wall_flac(
        &format!("{music}/Pink Floyd/The Wall/Disc 2/01 - Hey You.flac"),
        "Hey You",
        1,
        2,
    );
    let pooled = in_pool(&world, "Pink Floyd/The Wall/02 - The Thin Ice.flac");
    wall_flac(&pooled, "The Thin Ice", 2, 1);
    world.pool.link(&pooled, "alex").await.expect("linked");

    let shared = world.pool.promote(&pooled).await.expect("moved");

    assert_eq!(
        shared,
        format!("{music}/Pink Floyd/The Wall/Disc 1/02 - The Thin Ice.flac")
    );
    assert_eq!(
        names_in(&format!("{music}/Pink Floyd/The Wall")),
        ["Disc 1", "Disc 2"]
    );
}

/// A shared song copied into the pool for a member who cannot see its library joins the pool
/// album's disc folder, as a member's download of it would.
#[tokio::test]
async fn copy_shared_for_joins_the_pool_albums_disc_folder() {
    let world = World::new(None);
    by_discs(&world);
    wall_flac(
        &in_pool(&world, "Pink Floyd/The Wall/Disc 1/01 - In the Flesh.flac"),
        "In the Flesh",
        1,
        1,
    );
    wall_flac(
        &in_pool(&world, "Pink Floyd/The Wall/Disc 2/01 - Hey You.flac"),
        "Hey You",
        1,
        2,
    );
    let shared = format!("{}/Pink Floyd/The Wall/02 - The Thin Ice.flac", world.music());
    wall_flac(&shared, "The Thin Ice", 2, 1);

    let link = world
        .pool
        .copy_shared_for(&shared, "sam", None, None)
        .await
        .expect("copied");

    assert_eq!(
        link,
        format!(
            "{}/sam/Pink Floyd/The Wall/Disc 1/02 - The Thin Ice.flac",
            world.members()
        )
    );
    assert!(
        Path::new(&in_pool(
            &world,
            "Pink Floyd/The Wall/Disc 1/02 - The Thin Ice.flac"
        ))
        .is_file()
    );
    assert_eq!(
        names_in(&in_pool(&world, "Pink Floyd/The Wall")),
        ["Disc 1", "Disc 2"]
    );
}

/// A shared song of disc 2 copied into the pool beside its album's loose disc 1 there (the
/// pool's files are all Octo's) lands loose beside it, so the pool album keeps one folder.
#[tokio::test]
async fn copy_shared_for_beside_loose_disc_one_lands_loose() {
    let world = World::new(None);
    by_discs(&world);
    let loose = in_pool(&world, "Pink Floyd/The Wall/01 - In the Flesh.flac");
    wall_flac(&loose, "In the Flesh", 1, 1);
    let shared = format!("{}/Pink Floyd/The Wall/Disc 2/01 - Hey You.flac", world.music());
    wall_flac(&shared, "Hey You", 1, 2);

    world
        .pool
        .copy_shared_for(&shared, "sam", None, None)
        .await
        .expect("copied");

    let copy = in_pool(&world, "Pink Floyd/The Wall/01 - Hey You.flac");
    assert!(Path::new(&copy).is_file());
    assert!(Path::new(&loose).is_file(), "the loose song stays where it is");
    assert_eq!(
        names_in(&in_pool(&world, "Pink Floyd/The Wall")),
        ["01 - Hey You.flac", "01 - In the Flesh.flac"],
        "one folder, no disc folder"
    );
}

/// A promoted song Octo placed is named as the File layout task names it: from the values its
/// mapping saved, each field kept while its tag still says what was written (a year the tags
/// never carried stays), and from its tag where that was edited since (a new title).
#[tokio::test]
async fn promote_names_an_octo_song_from_its_saved_values_while_its_tags_hold() {
    let world = World::new(None);
    let mut settings = world.settings.current().as_ref().clone();
    settings.subsonic.folder_structure = FolderStructure::Custom;
    settings.subsonic.folder_template = "{Artist}/{Album} ({Year})/{Track:00} - {Title}".into();
    world.settings.set(settings);
    let library = std::sync::Arc::new(crate::services::local::test_support::FakeLocalLibrary::default());
    let (pool, _) = pool_with_library(&world, &library);
    // Placed as Rumours' tracks of 1977, tagged with no year; Songbird's title edited since.
    let mut placed = Vec::new();
    for (title, tagged_title, track) in [("Dreams", "Dreams", 2), ("Songbird", "Songbird (Demo)", 3)] {
        let pooled = in_pool(&world, &format!("Fleetwood Mac/Rumours/0{track} - {title}.flac"));
        tagged_flac(&pooled, "Fleetwood Mac", "Rumours", tagged_title);
        let mut file = octo_media::tags::TagFile::open(Path::new(&pooled)).expect("opens");
        file.set_track(track);
        file.set_year(0);
        file.save().expect("saved");
        let saved = LayoutValues {
            artist: "Fleetwood Mac".into(),
            track_artist: "Fleetwood Mac".into(),
            album: "Rumours".into(),
            title: title.into(),
            track: Some(track as i32),
            year: Some(1977),
            album_type: Some("Album".into()),
            kind: LayoutKind::Album,
            tags: Some(octo_core::common::TagSnapshot {
                title: SongIdentity::key(title),
                album: SongIdentity::key("Rumours"),
                artist: SongIdentity::key("Fleetwood Mac"),
                track: Some(track as i32),
                ..Default::default()
            }),
            ..LayoutValues::default()
        };
        library.by_tags.lock().push((
            "Fleetwood Mac".into(),
            title.into(),
            Some("Rumours".into()),
            crate::services::local::LocalSongMapping {
                local_path: pooled.clone(),
                artist: "Fleetwood Mac".into(),
                title: title.into(),
                album: "Rumours".into(),
                layout: Some(saved),
                ..Default::default()
            },
        ));
        pool.link(&pooled, "alex").await.expect("linked");
        placed.push(pool.promote(&pooled).await.expect("moved"));
    }

    let music = world.music();
    assert_eq!(
        placed,
        [
            format!("{music}/Fleetwood Mac/Rumours (1977)/02 - Dreams.flac"),
            format!("{music}/Fleetwood Mac/Rumours (1977)/03 - Songbird (Demo).flac"),
        ]
    );
}

/// A promotion puts right a replace of a library file a stop left half done before it moves
/// anything, as every route that places does: the old copy is back under its name.
#[tokio::test]
async fn promote_puts_a_stopped_replace_right_first() {
    let world = World::new(None);
    let folder = format!("{}/Daft Punk/Discovery", world.music());
    std::fs::create_dir_all(&folder).expect("made");
    let original = format!("{folder}/01 - One More Time.mp3");
    let backup = format!("{folder}/.octo-replacing-ggg");
    std::fs::write(&backup, b"the old One More Time").expect("written");
    let records = format!("{}/.octo-incoming/replacing", world.music());
    std::fs::create_dir_all(&records).expect("made");
    let record = json!({ "original": original, "backup": backup });
    std::fs::write(format!("{records}/ggg.json"), record.to_string()).expect("written");
    let pooled = world.pool_file(SONG, 10);
    world.pool.link(&pooled, "alex").await.expect("linked");

    world.pool.promote(&pooled).await.expect("moved");

    assert_eq!(
        std::fs::read(&original).ok().as_deref(),
        Some(&b"the old One More Time"[..]),
        "put back"
    );
    assert!(!Path::new(&backup).exists());
    assert!(holds_no_record(Path::new(&records)));
}

/// Whether the records folder `records` holds no record of a move or a copy under way (the
/// folder itself stays).
fn holds_no_record(records: &Path) -> bool {
    std::fs::read_dir(records).map_or(true, |entries| {
        !entries.flatten().any(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
        })
    })
}

/// A pool file moved across disks takes its name only once the whole copy stands beside it, and
/// the file it came from goes last.
#[test]
fn a_pool_move_across_disks_takes_its_name_only_once_whole() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let from = dir.path().join("Artist/Album/01 Song.mp3");
    write_file(&from.to_string_lossy(), 10);
    let to = dir.path().join("elsewhere/Artist/Album/01 Song.mp3");
    let staging = dir.path().join(".octo-incoming");

    move_with(&staging, &from, &to, Staged::MoveOrNothing, |_, _| {
        Err(io::Error::from(io::ErrorKind::CrossesDevices))
    })
    .expect("moved");

    assert_eq!(std::fs::read(&to).expect("there").len(), 10);
    assert!(!from.exists());
    assert_eq!(
        names_in(&dir.path().join("elsewhere/Artist/Album").to_string_lossy()),
        ["01 Song.mp3"]
    );
    let seen: Vec<(bool, bool, bool)> = STAGED_COPIES
        .lock()
        .iter()
        .filter(|(name, ..)| *name == to)
        .map(|(_, taken, whole, recorded)| (*taken, *whole, *recorded))
        .collect();
    assert_eq!(
        seen,
        [(false, true, true)],
        "the copy stood beside the free name, its record under way marking it whole"
    );
    assert!(
        holds_no_record(&staging.join("replacing")),
        "its record went with it"
    );
}

/// Every file under `root` whose name holds `name`, hidden copies too.
fn files_named_like(root: &Path, name: &str) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut folders = vec![root.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                folders.push(path);
            } else if entry.file_name().to_string_lossy().contains(name) {
                found.push(path);
            }
        }
    }
    found
}

/// A pool move across disks whose source the disk refuses to remove is an error, and its copy is
/// taken back, so a caller that takes the error as nothing moved is right: the next recovery
/// leaves exactly one copy, where the song was.
#[tokio::test]
async fn a_pool_move_whose_source_cannot_go_fails_and_leaves_one_copy() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let from = dir.path().join("Artist/Album/01 Song.mp3");
    write_file(&from.to_string_lossy(), 10);
    let to = dir.path().join("elsewhere/Artist/Album/01 Song.mp3");
    let staging = dir.path().join(".octo-incoming");
    PLACEMENT.refuse_removals(&[&from]);

    let moved = move_with(&staging, &from, &to, Staged::MoveOrNothing, |_, _| {
        Err(io::Error::from(io::ErrorKind::CrossesDevices))
    });
    PLACEMENT.allow_all();
    drop(PLACEMENT.placing(&staging).await);

    assert!(moved.is_err(), "the move fails");
    assert_eq!(
        files_named_like(dir.path(), "01 Song.mp3"),
        std::slice::from_ref(&from),
        "exactly one copy, where the song was"
    );
    assert_eq!(std::fs::read(&from).ok().map(|bytes| bytes.len()), Some(10));
    assert!(holds_no_record(&staging.join("replacing")));
}

/// A pool move across disks whose source and copy the disk both refuse to remove is done: the
/// copy is the song's, and its record is kept. The next recovery flushes the copy's folder, then
/// removes the source though it is outside the staging folder, and the record goes.
#[tokio::test]
async fn a_pool_move_it_cannot_take_back_is_finished_by_recovery() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let from = dir.path().join("Artist/Album/01 Song.mp3");
    write_file(&from.to_string_lossy(), 10);
    let to = dir.path().join("elsewhere/Artist/Album/01 Song.mp3");
    let staging = dir.path().join(".octo-incoming");
    let records = staging.join("replacing");
    PLACEMENT.refuse_removals(&[&from, &to]);

    let moved = move_with(&staging, &from, &to, Staged::MoveOrNothing, |_, _| {
        Err(io::Error::from(io::ErrorKind::CrossesDevices))
    });
    PLACEMENT.allow_all();

    assert!(moved.is_ok(), "the copy is the song's: {moved:?}");
    assert!(from.is_file(), "its source waits for the next recovery");
    assert!(!holds_no_record(&records), "its record stays");
    PLACEMENT.take_settling();

    drop(PLACEMENT.placing(&staging).await);

    let folder = to.parent().expect("a folder").to_string_lossy().into_owned();
    assert_eq!(
        PLACEMENT.take_settling(),
        [
            format!("flush {folder}"),
            format!("remove {}", from.to_string_lossy())
        ],
        "the copy's folder is flushed before its source goes"
    );
    assert_eq!(
        files_named_like(dir.path(), "01 Song.mp3"),
        std::slice::from_ref(&to)
    );
    assert_eq!(std::fs::read(&to).ok().map(|bytes| bytes.len()), Some(10));
    assert!(holds_no_record(&records));
}

/// A pool move across disks writes its record, marks it whole and marks it moved each with the
/// records folder flushed, before the move goes on: a power cut never takes back what a record
/// said once the move acted on it.
#[test]
fn a_pool_moves_record_and_its_marks_last_before_the_move_goes_on() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let from = dir.path().join("Artist/Album/01 Song.mp3");
    write_file(&from.to_string_lossy(), 10);
    let to = dir.path().join("elsewhere/Artist/Album/01 Song.mp3");
    let staging = dir.path().join(".octo-incoming");
    PLACEMENT.refuse_removals(&[&from, &to]);
    PLACEMENT.take_settling();

    let moved = move_with(&staging, &from, &to, Staged::MoveOrNothing, |_, _| {
        Err(io::Error::from(io::ErrorKind::CrossesDevices))
    });
    let log = PLACEMENT.take_settling();
    PLACEMENT.allow_all();

    assert!(moved.is_ok(), "{moved:?}");
    let records = format!("flush {}", staging.join("replacing").to_string_lossy());
    let folder = to.parent().expect("a folder").to_string_lossy().into_owned();
    assert_eq!(
        log,
        [
            format!("flush {}", staging.to_string_lossy()),
            format!("flush {}", dir.path().to_string_lossy()),
            records.clone(),
            records.clone(),
            format!("flush {folder}"),
            format!("remove {}", from.to_string_lossy()),
            format!("remove {}", to.to_string_lossy()),
            records,
        ],
        "the records and staging folders, the record, its whole mark and its moved mark each flushed before the move goes on"
    );
}

/// A pool move across disks whose new folder the disk refuses to flush takes its copy back and
/// fails; while that folder still cannot be flushed, the take-back may not last, so the record
/// stays for the next recovery, which leaves exactly the source.
#[tokio::test]
async fn a_take_back_whose_folder_cannot_be_flushed_keeps_its_record() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let from = dir.path().join("Artist/Album/01 Song.mp3");
    write_file(&from.to_string_lossy(), 10);
    let to = dir.path().join("elsewhere/Artist/Album/01 Song.mp3");
    let staging = dir.path().join(".octo-incoming");
    let records = staging.join("replacing");
    PLACEMENT.refuse_flushes(&[&to]);

    let moved = move_with(&staging, &from, &to, Staged::MoveOrNothing, |_, _| {
        Err(io::Error::from(io::ErrorKind::CrossesDevices))
    });
    let kept = !holds_no_record(&records);
    PLACEMENT.allow_all();
    drop(PLACEMENT.placing(&staging).await);

    assert!(moved.is_err(), "the move fails");
    assert!(kept, "its record stays while the take-back may not last");
    assert_eq!(
        files_named_like(dir.path(), "01 Song.mp3"),
        std::slice::from_ref(&from),
        "exactly the source"
    );
    assert!(holds_no_record(&records));
}

/// A member's own copy (or a song's lyrics) moved across disks, whose source the disk refuses to
/// remove, is done: the caller follows the copy whatever the answer, so it is never taken back,
/// and the next recovery removes the source.
#[tokio::test]
async fn a_members_copy_move_whose_source_cannot_go_is_done() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let from = dir.path().join("alex/Artist/Album/01 Song.mp3");
    write_file(&from.to_string_lossy(), 10);
    let to = dir.path().join("alex/Artist/Album (2004)/01 Song.mp3");
    let staging = dir.path().join(".octo-incoming");
    PLACEMENT.refuse_removals(&[&from]);

    let moved = move_with(&staging, &from, &to, Staged::Move, |_, _| {
        Err(io::Error::from(io::ErrorKind::CrossesDevices))
    });
    PLACEMENT.allow_all();
    let waits = from.is_file();
    drop(PLACEMENT.placing(&staging).await);

    assert!(moved.is_ok(), "{moved:?}");
    assert!(waits, "its source waits for the next recovery");
    assert_eq!(
        files_named_like(dir.path(), "01 Song.mp3"),
        std::slice::from_ref(&to)
    );
    assert!(holds_no_record(&staging.join("replacing")));
}

/// Octo stopped after a member's own copy moved across disks took its new name and before its
/// source went: the member's records already name the new place, so the next recovery keeps the
/// copy there and removes the source, never taking the copy back.
#[tokio::test]
async fn a_members_copy_move_stopped_after_its_rename_keeps_its_new_place() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let from = dir.path().join("alex/Artist/Album/01 Song.mp3");
    write_file(&from.to_string_lossy(), 10);
    let to = dir.path().join("alex/Artist/Album (2004)/01 Song.mp3");
    let staging = dir.path().join(".octo-incoming");
    let (_, mut record) = PLACEMENT
        .copy_under_way(&staging, &to, Some(&from), false)
        .expect("recorded");
    record.mark_whole().expect("marked whole");
    write_file(&to.to_string_lossy(), 10);
    record.keep();

    drop(PLACEMENT.placing(&staging).await);

    assert_eq!(
        files_named_like(dir.path(), "01 Song.mp3"),
        std::slice::from_ref(&to),
        "the copy keeps its new place"
    );
    assert!(holds_no_record(&staging.join("replacing")));
}

/// A pool move across disks whose source and copy the disk both refuse to remove, and whose
/// record cannot be marked moved, is an error: its record stays as it was, so the next recovery
/// takes the copy back and leaves exactly the source.
#[tokio::test]
async fn a_pool_move_whose_record_cannot_say_moved_fails_and_is_taken_back() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let from = dir.path().join("Artist/Album/01 Song.mp3");
    write_file(&from.to_string_lossy(), 10);
    let to = dir.path().join("elsewhere/Artist/Album/01 Song.mp3");
    let staging = dir.path().join(".octo-incoming");
    PLACEMENT.refuse_removals(&[&from, &to]);
    PLACEMENT.refuse_moved_marks();

    let moved = move_with(&staging, &from, &to, Staged::MoveOrNothing, |_, _| {
        Err(io::Error::from(io::ErrorKind::CrossesDevices))
    });
    PLACEMENT.allow_all();
    drop(PLACEMENT.placing(&staging).await);

    assert!(moved.is_err(), "the move fails");
    assert_eq!(
        files_named_like(dir.path(), "01 Song.mp3"),
        std::slice::from_ref(&from),
        "exactly the source"
    );
    assert!(holds_no_record(&staging.join("replacing")));
}

/// Octo stopped after a pool move across disks gave its copy the name and before its source went:
/// the move was never answered as done, so the pool's record, the song's mapping and the member's
/// link all still name the source. The next recovery takes the copy back and leaves exactly the
/// source, which they all still point at.
#[tokio::test]
async fn a_pool_move_stopped_before_its_source_went_is_taken_back() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    world.library.map("test", "1", &pooled);
    let link = world.pool.link(&pooled, "alex").await.expect("linked");
    let to = format!("{}/{SONG}", world.music());
    write_file(&to, 10);
    let staging = Path::new(&world.music()).join(".octo-incoming");
    let records = staging.join("replacing");
    std::fs::create_dir_all(&records).expect("made");
    let record = json!({
        "original": to,
        "staged": format!("{to}.octo-copy"),
        "source": pooled,
        "size": 10,
        "whole": true,
        "undo": true,
    });
    std::fs::write(records.join("sss.json"), record.to_string()).expect("written");

    drop(PLACEMENT.placing(&staging).await);

    assert!(!Path::new(&to).exists(), "the copy is taken back");
    assert!(
        files_named_like(Path::new(&world.music()), "01 Song.mp3").is_empty(),
        "nothing of the move is left in the shared folder"
    );
    assert_eq!(
        std::fs::read(&pooled).ok().map(|bytes| bytes.len()),
        Some(10),
        "the source stays"
    );
    assert_eq!(world.pool.store().get(&pooled).expect("recorded").refs, ["alex"]);
    assert_eq!(
        world.library.mappings.lock().get("test:1").cloned(),
        Some(pooled.clone()),
        "the mapping still names the source"
    );
    assert!(Path::new(&link).is_file(), "the member's link reads a file");
    #[cfg(unix)]
    assert_eq!(
        std::fs::canonicalize(&link).ok(),
        std::fs::canonicalize(&pooled).ok(),
        "the member's link points at the source"
    );
    assert!(holds_no_record(&records));
}

/// A pool move across disks that a power cut stopped with its source gone and its whole copy
/// beside the name: the next recovery gives the copy its name and never removes it.
#[tokio::test]
async fn a_pool_move_whose_source_is_gone_gives_its_copy_the_name() {
    let world = World::new(None);
    let to = format!("{}/Artist/Album/01 Song.mp3", world.music());
    let staged = format!("{to}.octo-copy");
    write_file(&staged, 10);
    let from = in_pool(&world, "Artist/Album/01 Song.mp3");
    let staging = Path::new(&world.music()).join(".octo-incoming");
    let records = staging.join("replacing");
    std::fs::create_dir_all(&records).expect("made");
    let record = json!({ "original": to, "staged": staged, "source": from, "size": 10, "whole": true });
    std::fs::write(records.join("ooo.json"), record.to_string()).expect("written");

    drop(PLACEMENT.placing(&staging).await);

    assert_eq!(
        std::fs::read(&to).ok().map(|bytes| bytes.len()),
        Some(10),
        "the name holds the song"
    );
    assert!(!Path::new(&staged).exists());
    assert!(holds_no_record(&records));
}

/// A pool copy a stop left unfinished is removed by the next recovery, from its record in the
/// staging folder; the file it was to take the place of is untouched.
#[tokio::test]
async fn a_stray_pool_copy_is_removed_by_recovery() {
    let world = World::new(None);
    let link = format!("{}/alex/Artist/Album/01 Song.mp3", world.members());
    write_file(&link, 10);
    let stray = format!("{link}.octo-copy");
    write_file(&stray, 3);
    let staging = Path::new(&world.music()).join(".octo-incoming");
    let records = staging.join("replacing");
    std::fs::create_dir_all(&records).expect("made");
    let record = json!({ "original": link, "staged": stray });
    std::fs::write(records.join("hhh.json"), record.to_string()).expect("written");

    drop(PLACEMENT.placing(&staging).await);

    assert!(!Path::new(&stray).exists(), "the unfinished copy is gone");
    assert_eq!(
        std::fs::read(&link).ok().map(|bytes| bytes.len()),
        Some(10),
        "the original is untouched"
    );
    assert!(holds_no_record(&records));
}

/// A pool path for the File layout task: under the pool folder.
fn in_pool(world: &World, relative: &str) -> String {
    format!("{}/{relative}", world.pool.pool_root_path())
}

/// The File layout task moves a pool file: the file and its lyrics move, the record follows, and
/// each member's link is made again at the new place, pointing at it, with the cover beside it.
#[cfg(unix)]
#[tokio::test]
async fn relayout_moves_the_file_and_every_link() {
    let world = World::new(None);
    let old = world.pool_file(SONG, 10);
    write_file(&old.replace(".mp3", ".lrc"), 3);
    write_file(&in_pool(&world, "Artist/Album/cover.jpg"), 4);
    world.pool.link(&old, "alex").await.expect("linked");
    world.pool.link(&old, "sam").await.expect("linked");
    let new_relative = "Artist/Album (2004)/01 - Song.mp3";
    let new = in_pool(&world, new_relative);

    let outcome = world.pool.relayout(&old, &new).await.expect("moved");

    assert_eq!(outcome, RelayoutOutcome::Moved);
    assert_eq!(std::fs::read(&new).expect("moved").len(), 10);
    assert!(
        Path::new(&new.replace(".mp3", ".lrc")).is_file(),
        "its lyrics went with it"
    );
    assert!(Path::new(&in_pool(&world, "Artist/Album (2004)/cover.jpg")).is_file());
    assert!(
        !Path::new(&in_pool(&world, "Artist/Album")).exists(),
        "the old folder went"
    );
    assert!(world.pool.store().get(&old).is_none());
    assert_eq!(
        world.pool.store().get(&new).expect("recorded").refs,
        ["alex", "sam"]
    );
    for user in ["alex", "sam"] {
        let link = format!("{}/{user}/{new_relative}", world.members());
        assert!(is_link(&link), "{user}'s link is at the new place");
        assert_eq!(
            std::fs::read_link(&link).expect("a link").to_string_lossy(),
            format!("../../../.pool/{new_relative}")
        );
        assert_eq!(std::fs::read(&link).expect("reads through").len(), 10);
        assert!(is_link(&link.replace(".mp3", ".lrc")));
        assert!(is_link(&format!(
            "{}/{user}/Artist/Album (2004)/cover.jpg",
            world.members()
        )));
        assert!(!Path::new(&format!("{}/{user}/Artist/Album", world.members())).exists());
    }
    assert!(world.stars.asked.lock().is_empty(), "no star is carried");
}

/// In copy mode each member's real copy is renamed, never copied again.
#[tokio::test]
async fn relayout_in_copy_mode_renames() {
    let world = World::new(None);
    let old = world.pool_file(SONG, 10);
    let link = world.pool.link(&old, "alex").await.expect("linked");
    world.pool.fall_back_to_copies().await;
    assert_eq!(world.pool.mode(), LinkMode::Copy);
    assert!(!is_link(&link) && Path::new(&link).is_file());
    // The member's copy, told apart from the pool's file by what it holds.
    std::fs::write(&link, b"alex's copy").expect("written");
    let new_relative = "Artist/Album/01 - Song.mp3";
    let new = in_pool(&world, new_relative);

    assert_eq!(
        world.pool.relayout(&old, &new).await.expect("moved"),
        RelayoutOutcome::Moved
    );

    let renamed = format!("{}/alex/{new_relative}", world.members());
    assert!(!is_link(&renamed));
    assert_eq!(std::fs::read(&renamed).expect("renamed"), b"alex's copy");
    assert!(!Path::new(&link).exists());
    assert_eq!(std::fs::read(&new).expect("moved").len(), 10);
}

/// Anything already at a member's new link path, by that name or another spelling of it, or a
/// folder on the way by another spelling, holds the move with its words, and nothing moves.
#[tokio::test]
async fn relayout_holds_when_a_member_path_is_taken() {
    let world = World::new(None);
    let old = world.pool_file(SONG, 10);
    let alex_link = world.pool.link(&old, "alex").await.expect("linked");
    world.pool.link(&old, "sam").await.expect("linked");
    write_file(&format!("{}/alex/Artist/Album/01 - SONG.mp3", world.members()), 2);

    let held = world
        .pool
        .relayout(&old, &in_pool(&world, "Artist/Album/01 - Song.mp3"))
        .await
        .expect("answered");

    assert_eq!(
        held,
        RelayoutOutcome::Held("alex's library already has 01 - SONG.mp3, so this song stays.".into())
    );
    assert!(Path::new(&old).is_file());
    assert!(Path::new(&alex_link).exists());
    assert_eq!(
        world.pool.store().get(&old).expect("still recorded").refs,
        ["alex", "sam"]
    );

    std::fs::create_dir_all(format!("{}/sam/new artist", world.members())).expect("a folder");
    let held = world
        .pool
        .relayout(&old, &in_pool(&world, "New Artist/Album/01 Song.mp3"))
        .await
        .expect("answered");
    assert_eq!(
        held,
        RelayoutOutcome::Held("sam's library already has new artist, so this song stays.".into())
    );
    assert!(Path::new(&old).is_file());
    assert!(!Path::new(&in_pool(&world, "New Artist")).exists());
}

/// A lyrics job waiting for the gate while a pool file moves is told where the song and each
/// member's link went before the move lets anything go, so it writes beside the song where it is.
#[tokio::test]
async fn relayout_tells_waiting_lyrics_jobs_where_the_song_went() {
    let world = World::new(None);
    let old = world.pool_file(SONG, 10);
    let link = world.pool.link(&old, "alex").await.expect("linked");
    let new = in_pool(&world, "Artist/Album (2004)/01 - Song.mp3");
    let remap = crate::services::lyrics::LyricsPathRemap::new();
    let waiting = remap.hold_for_test();
    let held = world.pool.try_hold(&[&old, &new]).expect("free");

    let outcome = world
        .pool
        .relayout_held(&held, &old, &new, Some(remap.clone()))
        .await
        .expect("moved");

    assert_eq!(outcome, RelayoutOutcome::Moved);
    assert_eq!(remap.map(&old), new);
    assert_eq!(
        remap.map(&link),
        world.pool.link_path(&new, "alex").expect("alex's link")
    );
    remap.release_for_test(waiting);
}

/// While the tidy holds pool files, no other change touches them: another hold is refused, a
/// change that needs one of them waits until the hold goes, and only held files are moved.
#[tokio::test]
async fn relayout_holds_its_files_from_other_changes() {
    let world = World::new(None);
    let old = world.pool_file(SONG, 10);
    let new = in_pool(&world, "Artist/Album/01 - Song.mp3");
    let held = world.pool.try_hold(&[&old, &new]).expect("free");
    assert!(world.pool.try_hold(&[&old]).is_none(), "held by the tidy");
    let linking = {
        let (pool, old) = (world.pool.clone(), old.clone());
        tokio::spawn(async move { pool.link(&old, "alex").await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(!linking.is_finished(), "the link waits for the tidy's hold");
    let elsewhere = in_pool(&world, "Other/Album/01 - Song.mp3");
    assert!(
        world
            .pool
            .relayout_held(&held, &old, &elsewhere, None)
            .await
            .is_err(),
        "only held files move"
    );
    assert!(Path::new(&old).is_file());

    drop(held);

    linking.await.expect("ran").expect("linked");
    assert!(world.pool.try_hold(&[&old]).is_some());
}

/// `relayout(new, old)` puts everything back, and a move already done is done again as nothing.
#[tokio::test]
async fn relayout_undo() {
    let world = World::new(None);
    let old = world.pool_file(SONG, 10);
    write_file(&old.replace(".mp3", ".lrc"), 3);
    let link = world.pool.link(&old, "alex").await.expect("linked");
    let new = in_pool(&world, "Artist/Album (2004)/01 - Song.mp3");

    assert_eq!(
        world.pool.relayout(&old, &new).await.expect("moved"),
        RelayoutOutcome::Moved
    );
    assert_eq!(
        world.pool.relayout(&old, &new).await.expect("already done"),
        RelayoutOutcome::Moved
    );
    assert_eq!(
        world.pool.relayout(&new, &old).await.expect("back"),
        RelayoutOutcome::Moved
    );

    assert_eq!(std::fs::read(&old).expect("back").len(), 10);
    assert!(Path::new(&old.replace(".mp3", ".lrc")).is_file());
    assert!(!Path::new(&in_pool(&world, "Artist/Album (2004)")).exists());
    assert_eq!(world.pool.store().get(&old).expect("recorded").refs, ["alex"]);
    assert!(world.pool.store().get(&new).is_none());
    assert_eq!(std::fs::read(&link).expect("alex has it again").len(), 10);
    assert!(Path::new(&link.replace(".mp3", ".lrc")).exists());
    assert!(!Path::new(&format!("{}/alex/Artist/Album (2004)", world.members())).exists());
}
