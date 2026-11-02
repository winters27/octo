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
