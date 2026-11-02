//! The family pool's guarantees under pressure: changes that claim the same files in any order
//! always finish, a change that is dropped or panics lets go of its files, the records lock never
//! crosses an `.await`, the marker file is written only where the pool plainly is, sweeps keep
//! their spacing whatever the wall clock does, a promote and a link on one file settle one way,
//! and with Family off and nobody's data on disk the pool does nothing at all.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use octo_core::models::domain::Song;
use serde_json::json;

use super::*;
use crate::services::family::family_service::PersonalFolders;
use crate::services::family::hooks::PoolFolders;
use crate::services::family::test_support::{World, navidrome_with_songs, write_file};

const SONG: &str = "Artist/Album/01 Song.mp3";
const BETTER: &str = "Artist/Album/01 Song.flac";

/// A small seeded random source, so a failing run can be repeated.
struct Dice(u64);

impl Dice {
    fn new(seed: u64) -> Self {
        Dice(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    /// A number from 0 up to (not including) `sides`.
    fn roll(&mut self, sides: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % sides.max(1)
    }
}

fn is_audio(path: &str) -> bool {
    path.ends_with(".mp3") || path.ends_with(".flac")
}

/// Every file and link under `root`, folders left out.
fn walk(root: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(root)];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                stack.push(path);
            } else {
                found.push(path.to_string_lossy().into_owned());
            }
        }
    }
    found.sort();
    found
}

/// What disagrees between the pool's records, the pool folder and the members' folders: a
/// record whose file is not there, a record nobody has, a member a record names with no link
/// that reads, a song in a member's library that no record gives them, and a song in the pool
/// with no record.
fn problems(world: &World) -> Vec<String> {
    let mut found = Vec::new();
    let records = world.pool.store().list();
    for file in &records {
        if !Path::new(&file.path).is_file() {
            found.push(format!("{} is recorded but not there", file.path));
        }
        if file.refs.is_empty() {
            found.push(format!("{} is recorded for nobody", file.path));
        }
        for user in &file.refs {
            let reads = world
                .pool
                .link_path(&file.path, user)
                .is_some_and(|link| std::fs::metadata(link).is_ok());
            if !reads {
                found.push(format!("{user} has no link that reads to {}", file.path));
            }
        }
    }
    for user in ["alex", "sam"] {
        for link in walk(&format!("{}/{user}", world.members())) {
            if !is_audio(&link) {
                continue;
            }
            let given = world
                .pool
                .pool_path_of_link(user, &link)
                .and_then(|pool_path| records.iter().find(|file| file.path == pool_path))
                .is_some_and(|file| file.refs.iter().any(|r| r.eq_ignore_ascii_case(user)));
            if !given {
                found.push(format!(
                    "{link} is in {user}'s library but no record gives it to them"
                ));
            }
            if std::fs::metadata(&link).is_err() {
                found.push(format!("{link} leads nowhere"));
            }
        }
    }
    for file in walk(&world.pool.pool_root_path()) {
        if is_audio(&file) && !records.iter().any(|record| record.path == file) {
            found.push(format!("{file} is in the pool with no record"));
        }
    }
    found
}

fn assert_consistent(world: &World, when: &str) {
    let found = problems(world);
    assert!(found.is_empty(), "{when}: {found:#?}");
}

// ---- No deadlock ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claims_taken_in_opposite_orders_never_deadlock_and_never_overlap() {
    const FILES: [&str; 4] = ["a.flac", "b.flac", "c.flac", "d.flac"];
    let claims = Arc::new(Claims::default());
    let busy: Arc<Vec<AtomicUsize>> = Arc::new(FILES.iter().map(|_| AtomicUsize::new(0)).collect());
    let tasks: Vec<_> = (0..64u64)
        .map(|seed| {
            let (claims, busy) = (Arc::clone(&claims), Arc::clone(&busy));
            tokio::spawn(async move {
                let mut dice = Dice::new(seed + 1);
                for _ in 0..100 {
                    let first = dice.roll(4) as usize;
                    let second = (first + 1 + dice.roll(3) as usize) % 4;
                    let order = if dice.roll(2) == 0 {
                        [FILES[first], FILES[second]]
                    } else {
                        [FILES[second], FILES[first]]
                    };
                    let _claim = claims.claim(&order).await;
                    for held in [first, second] {
                        assert_eq!(
                            busy[held].fetch_add(1, Ordering::SeqCst),
                            0,
                            "two changes held one file"
                        );
                    }
                    if dice.roll(3) == 0 {
                        tokio::time::sleep(Duration::from_micros(dice.roll(500))).await;
                    } else {
                        tokio::task::yield_now().await;
                    }
                    for held in [first, second] {
                        busy[held].fetch_sub(1, Ordering::SeqCst);
                    }
                }
            })
        })
        .collect();
    let all = async {
        for task in tasks {
            task.await.expect("a task finished");
        }
    };
    tokio::time::timeout(Duration::from_secs(60), all)
        .await
        .expect("every change finished: no deadlock");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changes_on_two_files_in_opposite_orders_always_finish_and_agree() {
    let world = World::new(None);
    for round in 0..8u64 {
        let (song, better) = (
            format!("{}/{SONG}", world.pool.pool_root_path()),
            format!("{}/{BETTER}", world.pool.pool_root_path()),
        );
        for (path, user) in [(&song, "alex"), (&better, "sam")] {
            if !Path::new(path).is_file() {
                write_file(path, 10);
            }
            world.pool.link(path, user).await.expect("linked");
        }
        let tasks: Vec<_> = (0..24u64)
            .map(|n| {
                let (pool, song, better) = (world.pool.clone(), song.clone(), better.clone());
                let mut dice = Dice::new(round * 100 + n + 1);
                tokio::spawn(async move {
                    let (one, other) = if dice.roll(2) == 0 {
                        (&song, &better)
                    } else {
                        (&better, &song)
                    };
                    let user = if dice.roll(2) == 0 { "alex" } else { "sam" };
                    tokio::time::sleep(Duration::from_micros(dice.roll(2_000))).await;
                    match dice.roll(4) {
                        0 | 1 => {
                            pool.repoint(one, other).await;
                        }
                        2 => {
                            let _ = pool.link(one, user).await;
                        }
                        _ => {
                            let _ = pool.unlink(one, user).await;
                        }
                    }
                })
            })
            .collect();
        let all = async {
            for task in tasks {
                task.await.expect("a change finished");
            }
        };
        tokio::time::timeout(Duration::from_secs(60), all)
            .await
            .expect("every change finished: no deadlock");
        assert_consistent(&world, &format!("round {round}"));
    }
}

// ---- Canceled and panicking changes ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_dropped_mid_copy_lets_the_next_change_to_its_file_go_ahead() {
    let world = World::new(None);
    let shared = format!("{}/Shared/Album/01 Shared.mp3", world.music());
    write_file(&shared, 10);
    let target = format!("{}/Shared/Album/01 Shared.mp3", world.pool.pool_root_path());
    world.pool.slow_copies(Duration::from_millis(400));
    let copying = {
        let (pool, shared) = (world.pool.clone(), shared.clone());
        tokio::spawn(async move { pool.copy_shared_for(&shared, "sam", None, None).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    copying.abort();
    let _ = copying.await;
    world.pool.slow_copies(Duration::ZERO);
    let linked = tokio::time::timeout(Duration::from_secs(5), world.pool.link(&target, "alex"))
        .await
        .expect("the next change to the file went ahead");
    assert!(linked.is_ok(), "the next change found the file whole: {linked:?}");
    // Whatever the dropped change left running on the disk has finished by now.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_consistent(&world, "after the dropped copy");
    assert!(
        world
            .pool
            .store()
            .get(&target)
            .is_some_and(|file| file.refs.iter().any(|r| r == "alex")),
        "the pool copy is recorded with Alex's link"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_that_panics_lets_go_of_its_files() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    world.library.panics.store(true, Ordering::SeqCst);
    let panicking = {
        let (pool, pooled) = (world.pool.clone(), pooled.clone());
        tokio::spawn(async move { pool.link(&pooled, "alex").await })
    };
    let ended = panicking.await;
    assert!(
        ended.as_ref().is_err() || ended.as_ref().is_ok_and(|linked| linked.is_err()),
        "the change did not finish cleanly: {ended:?}"
    );
    world.library.panics.store(false, Ordering::SeqCst);
    let next = tokio::time::timeout(Duration::from_secs(5), world.pool.link(&pooled, "sam"))
        .await
        .expect("the next change to the file went ahead");
    assert!(next.is_ok(), "{next:?}");
    let unlinked = tokio::time::timeout(Duration::from_secs(5), world.pool.unlink(&pooled, "alex"))
        .await
        .expect("and the one after it");
    assert!(unlinked.is_ok(), "{unlinked:?}");
    assert_consistent(&world, "after the panic");
}

// ---- The records lock never crosses an await ------------------------------------------------

/// Builds each change's future without running it and asks that it may move between threads.
/// The records lock's guard may not, so a change holding it across an `.await` would not build.
#[test]
fn every_change_to_the_pool_is_a_future_that_may_move_between_threads() {
    fn movable<T: Send>(_: T) {}
    let world = World::new(None);
    let pool = &world.pool;
    let (path, other, song) = ("/x/.pool/a.mp3", "/x/.pool/b.flac", Song::default());
    movable(pool.link(path, "alex"));
    movable(pool.unlink(path, "alex"));
    movable(pool.let_go(path, "alex"));
    movable(pool.sweep_dangling());
    movable(pool.gc(path));
    movable(pool.repoint(path, other));
    movable(pool.promote(path));
    movable(pool.for_owner(path));
    movable(pool.copy_shared_for(path, "alex", None, None));
    movable(pool.release_member("alex", FilesChoice::Keep));
    movable(pool.copy_unshown_link("alex", path));
    movable(pool.copy_shown_where_link_was_not(path));
    movable(pool.fall_back_to_copies());
    movable(pool.find(None, None, "a", "b"));
    movable(pool.after_finished_scan(path, None));
    movable(pool.wait_for_path(path, None));
    movable(FamilyPlacement::placed(
        pool.as_ref(),
        path,
        &song,
        "test",
        "1",
        &[],
        false,
    ));
    movable(FamilyPlacement::settle(pool.as_ref(), path, &[], false, None));
}

// ---- The marker file -----------------------------------------------------------------------

fn folders(world: &World) -> PoolFolders {
    PoolFolders {
        personal: world.personal(),
    }
}

fn marker(world: &World) -> String {
    format!("{}/{POOL_MARKER}", world.pool.pool_root_path())
}

#[tokio::test]
async fn an_upgrade_with_records_and_their_files_gets_its_marker() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    world.pool.link(&pooled, "alex").await.expect("linked");
    std::fs::remove_file(marker(&world)).expect("an install from before the marker");
    folders(&world).probe_links().await;
    assert!(Path::new(&marker(&world)).is_file());
    assert!(world.pool.placement_ready().await);
}

#[tokio::test]
async fn an_upgrade_with_records_but_no_disk_gets_no_marker_and_no_sweep() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    world.pool.link(&pooled, "alex").await.expect("linked");
    std::fs::remove_file(marker(&world)).expect("an install from before the marker");
    // The members disk is not mounted: the pool folder is an empty mount point.
    std::fs::remove_file(&pooled).expect("not there");
    folders(&world).probe_links().await;
    assert!(
        !Path::new(&marker(&world)).exists(),
        "no marker on an empty mount point"
    );
    assert!(!world.pool.placement_ready().await);
    for _ in 0..3 {
        assert_eq!(world.pool.sweep_dangling().await, 0);
        world.later(11);
    }
    assert_eq!(world.pool.store().get(&pooled).expect("kept").refs, ["alex"]);
}

#[tokio::test]
async fn turning_family_on_over_records_whose_disk_is_missing_writes_no_marker() {
    let world = World::new(None);
    let pooled = world.pool_file(SONG, 10);
    world.pool.link(&pooled, "alex").await.expect("linked");
    std::fs::remove_file(marker(&world)).expect("an install from before the marker");
    std::fs::remove_file(&pooled).expect("the disk is not mounted");
    let marked = folders(&world).mark_pool().await;
    assert!(marked.is_err(), "turning Family on says the disk is missing");
    assert!(
        !Path::new(&marker(&world)).exists(),
        "no marker on an empty mount point"
    );
    for _ in 0..3 {
        assert_eq!(world.pool.sweep_dangling().await, 0);
        world.later(11);
    }
    assert_eq!(world.pool.store().get(&pooled).expect("kept").refs, ["alex"]);
}

#[tokio::test]
async fn a_fresh_install_gets_its_marker_when_family_is_turned_on() {
    let world = World::with_family(None, false);
    std::fs::remove_dir_all(world.pool.pool_root_path()).expect("a fresh members folder");
    folders(&world).mark_pool().await.expect("marked");
    assert!(Path::new(&marker(&world)).is_file());
}

// ---- Sweep spacing and the wall clock --------------------------------------------------------

fn sweeps_run(world: &World) -> usize {
    world.pool.sweeps_run.load(Ordering::SeqCst)
}

/// A pool with two files linked by Alex, one of them deleted by hand. Answers the deleted one.
async fn one_file_deleted_by_hand(world: &World) -> String {
    let pooled = world.pool_file(SONG, 10);
    let kept = world.pool_file("Other/Album/01 Other.mp3", 5);
    world.pool.link(&pooled, "alex").await.expect("linked");
    world.pool.link(&kept, "alex").await.expect("linked");
    std::fs::remove_file(&pooled).expect("deleted by hand");
    pooled
}

#[tokio::test]
async fn a_wall_clock_jumping_hours_ahead_never_forgets_a_file_missing_for_seconds() {
    let world = World::new(None);
    let pooled = one_file_deleted_by_hand(&world).await;
    assert_eq!(world.pool.sweep_dangling().await, 0, "the first miss");
    world.jump_wall(180);
    assert_eq!(
        world.pool.sweep_dangling().await,
        0,
        "seconds later, whatever the wall clock says"
    );
    assert_eq!(sweeps_run(&world), 1, "no second sweep seconds after the first");
    assert!(world.pool.store().get(&pooled).is_some());
    world.later(11);
    assert_eq!(world.pool.sweep_dangling().await, 1, "missing eleven minutes");
    assert!(world.pool.store().get(&pooled).is_none());
}

#[tokio::test]
async fn a_wall_clock_set_back_neither_stalls_sweeps_nor_runs_them_closer() {
    let world = World::new(None);
    let pooled = one_file_deleted_by_hand(&world).await;
    assert_eq!(world.pool.sweep_dangling().await, 0, "the first miss");
    world.jump_wall(-120);
    assert_eq!(world.pool.sweep_dangling().await, 0);
    assert_eq!(sweeps_run(&world), 1, "no second sweep seconds after the first");
    world.later(11);
    assert_eq!(
        world.pool.sweep_dangling().await,
        1,
        "eleven minutes on, the sweep runs and the file has been missing that long"
    );
    assert_eq!(sweeps_run(&world), 2);
    assert!(world.pool.store().get(&pooled).is_none());
}

#[tokio::test]
async fn no_clock_movement_makes_a_sweep_forget_a_present_file_or_run_closer_than_its_spacing() {
    for seed in 1..=12u64 {
        let world = World::new(None);
        let mut dice = Dice::new(seed);
        let pooled = world.pool_file(SONG, 10);
        world.pool.link(&pooled, "alex").await.expect("linked");
        let aside = format!("{pooled}.aside");
        // Real minutes passed, the real minute of the last sweep that ran, and whether the last
        // sweep that ran found the file away.
        let (mut real, mut last_run): (i64, Option<i64>) = (0, None);
        let mut missed = false;
        for _ in 0..60 {
            match dice.roll(3) {
                0 => {
                    let minutes = dice.roll(15) as i64;
                    world.later(minutes);
                    real += minutes;
                }
                1 => world.jump_wall(dice.roll(600) as i64 - 300),
                _ => {}
            }
            // Now and then the file is away for one sweep, never for two running in a row: a
            // sweep that finds it there clears the earlier miss.
            if !missed && dice.roll(4) == 0 {
                std::fs::rename(&pooled, &aside).expect("away");
                let before = sweeps_run(&world);
                assert_eq!(
                    world.pool.sweep_dangling().await,
                    0,
                    "seed {seed}: missed once only"
                );
                if sweeps_run(&world) > before {
                    if let Some(at) = last_run {
                        assert!(real - at >= 10, "seed {seed}: sweeps {} minutes apart", real - at);
                    }
                    last_run = Some(real);
                    missed = true;
                }
                std::fs::rename(&aside, &pooled).expect("back");
            }
            let before = sweeps_run(&world);
            assert_eq!(
                world.pool.sweep_dangling().await,
                0,
                "seed {seed}: a present file went"
            );
            if sweeps_run(&world) > before {
                if let Some(at) = last_run {
                    assert!(real - at >= 10, "seed {seed}: sweeps {} minutes apart", real - at);
                }
                last_run = Some(real);
                missed = false;
            }
            assert_eq!(
                world.pool.store().get(&pooled).map(|file| file.refs),
                Some(vec!["alex".to_string()]),
                "seed {seed}"
            );
        }
    }
}

// ---- A promote racing a link ----------------------------------------------------------------

async fn promote_racing_a_link(sam_sees_shared: bool) {
    let world = World::new(None);
    if !sam_sees_shared {
        let mut sam = world.family.member("sam").expect("sam");
        sam.shared_libraries = crate::services::family::SharedLibraries::Ids(vec![2]);
        world.family.upsert_member(sam);
    }
    for round in 0..40u64 {
        let mut dice = Dice::new(round + 1);
        let pooled = world.pool_file(&format!("Artist/Album {round}/01 Song.mp3"), 10);
        world.pool.link(&pooled, "alex").await.expect("linked");
        let by_owner = dice.roll(2) == 0;
        let promoting = {
            let (pool, pooled) = (world.pool.clone(), pooled.clone());
            let wait = Duration::from_micros(dice.roll(3_000));
            tokio::spawn(async move {
                tokio::time::sleep(wait).await;
                if by_owner {
                    pool.for_owner(&pooled).await.ok_or(())
                } else {
                    pool.promote(&pooled).await.map_err(|_| ())
                }
            })
        };
        let linking = {
            let (pool, pooled) = (world.pool.clone(), pooled.clone());
            let wait = Duration::from_micros(dice.roll(3_000));
            tokio::spawn(async move {
                tokio::time::sleep(wait).await;
                pool.link(&pooled, "sam").await
            })
        };
        let (promoted, linked) = tokio::time::timeout(Duration::from_secs(20), async {
            (promoting.await.expect("joins"), linking.await.expect("joins"))
        })
        .await
        .expect("both finished");
        let shared = promoted.unwrap_or_else(|()| panic!("round {round}: the promote failed"));
        assert!(
            Path::new(&shared).is_file(),
            "round {round}: in the shared library"
        );
        let sam_link = format!("{}/sam/Artist/Album {round}/01 Song.mp3", world.members());
        match (&linked, sam_sees_shared) {
            // The link came second and found the file moved; or Sam sees the shared library,
            // so the promote took the link away again.
            (Err(PoolError::NotInPool(_)), _) | (Ok(_), true) => {
                assert!(!Path::new(&pooled).exists(), "round {round}: moved, not copied");
                assert!(world.pool.store().get(&pooled).is_none(), "round {round}");
                assert!(std::fs::symlink_metadata(&sam_link).is_err(), "round {round}");
            }
            // Sam linked first and cannot see the shared library: the pool keeps it for Sam.
            (Ok(_), false) => {
                assert_eq!(
                    world.pool.store().get(&pooled).expect("kept for Sam").refs,
                    ["sam"],
                    "round {round}"
                );
                assert_eq!(std::fs::read(&sam_link).expect("Sam's link reads").len(), 10);
            }
            (Err(e), _) => panic!("round {round}: the link failed another way: {e}"),
        }
        assert!(
            !Path::new(&format!(
                "{}/alex/Artist/Album {round}/01 Song.mp3",
                world.members()
            ))
            .exists(),
            "round {round}: Alex sees it in the shared library"
        );
        assert_consistent(&world, &format!("round {round}"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_promote_racing_a_link_on_one_file_settles_one_way() {
    promote_racing_a_link(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_promote_racing_a_link_for_someone_who_cannot_see_the_shared_library_settles_one_way() {
    promote_racing_a_link(false).await;
}

// ---- Family off ----------------------------------------------------------------------------

#[tokio::test]
async fn family_off_with_no_member_data_writes_no_marker_sweeps_nothing_and_reads_no_index() {
    let navidrome = navidrome_with_songs(json!([])).await;
    let world = World::with_family(Some(&navidrome.uri()), false);
    world.family.remove_member("alex");
    world.family.remove_member("sam");
    std::fs::remove_dir_all(world.pool.pool_root_path()).expect("no pool folder yet");
    let shared = format!("{}/Some/Album/01 Shared.mp3", world.music());
    write_file(&shared, 10);
    let song = Song {
        title: "Shared".into(),
        artist: "Some".into(),
        ..Default::default()
    };

    let placement: &dyn FamilyPlacement = world.pool.as_ref();
    assert_eq!(placement.pool_root(), None);
    assert!(placement.placement_ready().await);
    assert!(!placement.is_pool_path(&format!("{}/{SONG}", world.pool.pool_root_path())));
    assert!(placement.find_song("test", "1", &song).await.is_none());
    assert_eq!(
        placement.for_owner(&shared).await.as_deref(),
        Some(shared.as_str())
    );
    assert_eq!(placement.settle(&shared, &[], false, None).await, shared);
    assert_eq!(placement.settle(&shared, &[], true, None).await, shared);
    let copies: &dyn MemberCopies = world.pool.as_ref();
    assert!(copies.own_copy_of("winters", "test:1", "s1").await.is_none());
    assert!(copies.own_song_at("winters", &shared).await.is_none());

    assert!(
        !Path::new(&world.pool.pool_root_path()).exists(),
        "no pool folder, no marker"
    );
    assert_eq!(sweeps_run(&world), 0, "no sweep");
    assert!(world.library.targeted().is_empty() && world.library.full_scans() == 0);
    assert!(
        navidrome.received_requests().await.unwrap_or_default().is_empty(),
        "Navidrome was never asked anything"
    );
    assert!(Path::new(&shared).is_file());
}
