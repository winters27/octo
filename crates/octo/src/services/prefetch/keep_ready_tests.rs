//! Playlists kept ready: what a listener picks, which lists count as played from most, and what
//! one pass copies and queues.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, TimeZone, Utc};
use octo_core::common::Clock;
use octo_core::settings::{AppSettings, PrefetchSettings, SettingsStore};
use parking_lot::Mutex;

use super::keep_ready::{
    KeepFlac, KeepReadyService, KeepReadySources, KeepReadyStore, ListSongs, Picks, UnknownList,
    first_and_random, most_played, week_seed,
};
use super::preview_copies::{PreviewCopies, PreviewSource, WholePreview};

fn ids(prefix: &str, n: usize) -> Vec<String> {
    (0..n).map(|i| format!("{prefix}{i}")).collect()
}

fn list(id: &str, songs: Vec<String>) -> ListSongs {
    ListSongs {
        id: id.to_string(),
        name: format!("List {id}"),
        kind: "import",
        songs,
    }
}

#[test]
fn a_listener_who_never_chose_keeps_their_most_played_lists_ready() {
    let store = KeepReadyStore::new(None);
    assert_eq!(store.picks("dedeham"), Picks::default());
    assert!(store.picks("dedeham").most_played);
    assert!(store.users().is_empty());
}

#[test]
fn picks_are_kept_by_listener_and_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keep-ready.json");
    let store = KeepReadyStore::new(Some(path.clone()));
    store.set(
        "Dedeham",
        Picks {
            lists: vec!["oi-weekly".into()],
            most_played: false,
        },
    );
    let again = KeepReadyStore::new(Some(path));
    assert_eq!(again.picks("dedeham").lists, ["oi-weekly"]);
    assert!(!again.picks(" DEDEHAM ").most_played);
    assert_eq!(again.users(), ["dedeham"]);
}

#[test]
fn most_played_is_the_top_three_lists_with_at_least_three_plays() {
    let lists = vec![
        list("a", ids("a", 10)),
        list("b", ids("b", 10)),
        list("c", ids("c", 10)),
        list("d", ids("d", 10)),
        list("e", ids("e", 10)),
    ];
    let mut plays = Vec::new();
    plays.extend(["a1", "a2", "a3", "a1"].map(String::from));
    plays.extend(["b1", "b2"].map(String::from));
    plays.extend(["c0", "c0", "c0", "c5", "c6"].map(String::from));
    plays.extend(["d1", "d2", "d3"].map(String::from));
    plays.extend(["e1", "e2", "e3"].map(String::from));

    // c (5), a (4), then d and e tie at 3: the first in the listener's lists wins. b has 2.
    assert_eq!(most_played(&lists, &plays), ["c", "a", "d"]);
    assert!(most_played(&lists, &[]).is_empty());
}

#[test]
fn a_list_is_kept_from_its_start_and_at_random_each_song_once() {
    let songs = ids("s", 200);
    let picked = first_and_random(&songs, 50, 50, 7);
    assert_eq!(picked.len(), 100);
    assert_eq!(picked[..50], songs[..50]);
    let unique: HashSet<&String> = picked.iter().collect();
    assert_eq!(unique.len(), 100);
    assert!(picked[50..].iter().all(|id| songs[50..].contains(id)));

    let short = ids("t", 30);
    assert_eq!(first_and_random(&short, 50, 50, 7), short);
    let doubled: Vec<String> = ["x", "x", "y"].map(String::from).to_vec();
    assert_eq!(first_and_random(&doubled, 50, 50, 7), ["x", "y"]);
}

#[test]
fn the_songs_a_list_keeps_at_random_change_once_a_week() {
    let songs = ids("s", 200);
    let monday = Utc.with_ymd_and_hms(2026, 10, 19, 8, 0, 0).unwrap();
    let friday = Utc.with_ymd_and_hms(2026, 10, 23, 22, 0, 0).unwrap();
    let next_week = Utc.with_ymd_and_hms(2026, 10, 26, 8, 0, 0).unwrap();
    let pick = |at| first_and_random(&songs, 5, 20, week_seed("oi-weekly", at));
    assert_eq!(pick(monday), pick(friday), "the same all week");
    assert_ne!(pick(monday), pick(next_week), "new ones the next week");
    assert_ne!(
        first_and_random(&songs, 5, 20, week_seed("oi-other", monday)),
        pick(monday),
        "each list its own"
    );
}

/// Lists, plays and videos as a test sets them.
#[derive(Default)]
struct FakeSources {
    lists: Mutex<HashMap<String, Vec<ListSongs>>>,
    plays: Mutex<HashMap<String, Vec<String>>>,
    recording: bool,
    /// Songs with a video; any other song has none and can not be copied.
    videos: Mutex<HashMap<String, String>>,
}

#[async_trait]
impl KeepReadySources for FakeSources {
    async fn lists(&self, user: &str) -> Vec<ListSongs> {
        self.lists.lock().get(user).cloned().unwrap_or_default()
    }

    async fn plays_since(&self, user: &str, _since: DateTime<Utc>) -> Option<Vec<String>> {
        self.recording
            .then(|| self.plays.lock().get(user).cloned().unwrap_or_default())
    }

    fn recording(&self) -> bool {
        self.recording
    }

    async fn listeners(&self) -> Vec<String> {
        self.plays.lock().keys().cloned().collect()
    }

    fn is_outside(&self, id: &str) -> bool {
        id.starts_with("out")
    }

    async fn find_video(&self, id: &str) -> Option<String> {
        self.videos.lock().get(id).cloned()
    }

    fn known_video(&self, id: &str) -> Option<String> {
        self.videos.lock().get(id).cloned()
    }
}

#[derive(Default)]
struct FakeFlac {
    queued: Mutex<Vec<(String, Vec<String>)>>,
    ready: Mutex<HashSet<String>>,
    /// The bytes the songs fetched ahead take.
    bytes: Mutex<u64>,
}

#[async_trait]
impl KeepFlac for FakeFlac {
    async fn keep_ready(&self, songs: Vec<(String, Vec<String>)>) {
        *self.queued.lock() = songs;
    }

    async fn flac_ready(&self, id: &str) -> bool {
        self.ready.lock().contains(id)
    }

    fn flac_bytes(&self) -> u64 {
        *self.bytes.lock()
    }
}

/// Every preview is 1,000 bytes.
struct TinyPreviews;

#[async_trait]
impl PreviewSource for TinyPreviews {
    async fn open_whole(&self, _video_id: &str) -> Option<WholePreview> {
        let body = futures::stream::iter([Ok(Bytes::from(vec![7u8; 1_000]))]);
        Some(WholePreview {
            content_type: "audio/mp4".to_string(),
            total: 1_000,
            body: Box::pin(body),
        })
    }
}

struct World {
    service: Arc<KeepReadyService>,
    sources: Arc<FakeSources>,
    flac: Arc<FakeFlac>,
    copies: Arc<PreviewCopies>,
    now: Arc<Mutex<DateTime<Utc>>>,
    _dir: tempfile::TempDir,
}

fn world(enabled: bool, recording: bool) -> World {
    let dir = tempfile::tempdir().unwrap();
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        prefetch: PrefetchSettings {
            enabled,
            keep_ready_count: 2,
            min_free_gb: 1,
            ..Default::default()
        },
        ..Default::default()
    }));
    let sources = Arc::new(FakeSources {
        recording,
        ..Default::default()
    });
    let flac = Arc::new(FakeFlac::default());
    let now = Arc::new(Mutex::new(Utc.with_ymd_and_hms(2026, 10, 21, 12, 0, 0).unwrap()));
    let read = Arc::clone(&now);
    let clock = Clock::new(move || *read.lock());
    let copies = PreviewCopies::new(
        dir.path().join("previews"),
        Arc::new(TinyPreviews),
        clock.clone(),
        || 0,
    );
    let service = KeepReadyService::new(
        settings,
        Arc::new(KeepReadyStore::new(None)),
        sources.clone(),
        flac.clone(),
        copies.clone(),
        clock,
    );
    World {
        service,
        sources,
        flac,
        copies,
        now,
        _dir: dir,
    }
}

/// An eleven-letter video id for song `i`.
fn video(i: usize) -> String {
    format!("video{i:06}")
}

/// `n` outside songs with videos, and `library` songs from the library.
fn songs_with_videos(world: &World, prefix: &str, n: usize, library: usize) -> Vec<String> {
    let mut songs = Vec::new();
    for i in 0..n {
        let id = format!("out-{prefix}{i}");
        let mut videos = world.sources.videos.lock();
        let next = video(videos.len());
        videos.insert(id.clone(), next);
        songs.push(id);
    }
    songs.extend(ids("lib", library));
    songs
}

async fn wait_for(copies: &PreviewCopies, n: usize) {
    for _ in 0..500 {
        if copies.sizes().0 >= n {
            return;
        }
        tokio::time::sleep(StdDuration::from_millis(10)).await;
    }
    panic!("only {} copies of {n}", copies.sizes().0);
}

#[tokio::test]
async fn a_picked_list_has_its_outside_previews_copied_and_its_first_songs_queued_as_flac() {
    let world = world(true, true);
    let songs = songs_with_videos(&world, "w", 6, 3);
    world.sources.lists.lock().insert(
        "dedeham".into(),
        vec![
            list("oi-weekly", songs.clone()),
            list("oi-other", ids("out-o", 4)),
        ],
    );
    world
        .service
        .choose("dedeham", vec!["oi-weekly".into()], false)
        .await
        .expect("one of theirs");

    world.service.pass().await;
    wait_for(&world.copies, 6).await;

    for i in 0..6 {
        assert!(world.copies.is_ready(&video(i)), "song {i} copied");
    }
    let queued = world.flac.queued.lock().clone();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].0, "dedeham");
    // Two from the start and two at random, outside songs only, never the other list's.
    assert_eq!(queued[0].1.len(), 4);
    assert_eq!(queued[0].1[..2], songs[..2]);
    assert!(queued[0].1.iter().all(|id| id.starts_with("out-w")));

    let view = world.service.view("dedeham").await;
    let weekly = view.lists.iter().find(|l| l.id == "oi-weekly").unwrap();
    assert_eq!(
        (weekly.outside, weekly.previews_ready, weekly.picked),
        (6, 6, true)
    );
    let other = view.lists.iter().find(|l| l.id == "oi-other").unwrap();
    assert!(!other.picked && !other.by_most_played);
    assert_eq!(view.kept_bytes, 6_000);
}

#[tokio::test]
async fn the_lists_a_listener_plays_from_most_are_kept_ready_without_picking() {
    let world = world(true, true);
    let songs = songs_with_videos(&world, "m", 3, 0);
    world.sources.lists.lock().insert(
        "sam".into(),
        vec![list("og-mix", songs.clone()), list("oi-never", ids("out-n", 3))],
    );
    world.sources.plays.lock().insert(
        "sam".into(),
        [songs[0].clone(), songs[1].clone(), songs[0].clone()].to_vec(),
    );

    world.service.pass().await;
    wait_for(&world.copies, 3).await;

    let view = world.service.view("sam").await;
    assert!(
        view.lists
            .iter()
            .any(|l| l.id == "og-mix" && l.by_most_played && !l.picked)
    );
    assert!(view.lists.iter().any(|l| l.id == "oi-never" && !l.by_most_played));
}

#[tokio::test]
async fn with_most_played_off_or_plays_not_recorded_only_picks_count() {
    for (most_played, recording) in [(false, true), (true, false)] {
        let world = world(true, recording);
        let songs = songs_with_videos(&world, "m", 3, 0);
        world
            .sources
            .lists
            .lock()
            .insert("sam".into(), vec![list("og-mix", songs.clone())]);
        world
            .sources
            .plays
            .lock()
            .insert("sam".into(), vec![songs[0].clone(); 5]);
        world
            .service
            .choose("sam", Vec::new(), most_played)
            .await
            .unwrap();

        world.service.pass().await;
        tokio::time::sleep(StdDuration::from_millis(100)).await;

        assert_eq!(
            world.copies.sizes().0,
            0,
            "most_played {most_played}, recording {recording}"
        );
        assert!(world.flac.queued.lock().is_empty());
    }
}

#[tokio::test]
async fn nothing_is_kept_ready_while_getting_songs_ahead_is_off() {
    let world = world(false, true);
    let songs = songs_with_videos(&world, "w", 3, 0);
    world
        .sources
        .lists
        .lock()
        .insert("sam".into(), vec![list("oi-weekly", songs)]);
    world
        .service
        .choose("sam", vec!["oi-weekly".into()], false)
        .await
        .unwrap();

    world.service.pass().await;
    tokio::time::sleep(StdDuration::from_millis(100)).await;

    assert_eq!(world.copies.sizes().0, 0);
    assert!(!world.service.view("sam").await.enabled);
}

#[tokio::test]
async fn only_a_listeners_own_lists_can_be_picked() {
    let world = world(true, true);
    world
        .sources
        .lists
        .lock()
        .insert("sam".into(), vec![list("oi-sams", ids("out-s", 2))]);
    world
        .sources
        .lists
        .lock()
        .insert("kim".into(), vec![list("oi-kims", ids("out-k", 2))]);

    assert_eq!(
        world.service.choose("sam", vec!["oi-kims".into()], true).await,
        Err(UnknownList("oi-kims".into()))
    );
    assert!(
        world
            .service
            .choose("sam", vec!["oi-sams".into(), "oi-sams".into()], true)
            .await
            .is_ok()
    );
    let view = world.service.view("sam").await;
    assert_eq!(view.lists.iter().filter(|l| l.picked).count(), 1);
}

#[tokio::test]
async fn songs_without_a_video_are_left_for_the_next_pass() {
    let world = world(true, true);
    let mut songs = songs_with_videos(&world, "w", 2, 0);
    songs.push("out-novideo".into());
    world
        .sources
        .lists
        .lock()
        .insert("sam".into(), vec![list("oi-weekly", songs)]);
    world
        .service
        .choose("sam", vec!["oi-weekly".into()], false)
        .await
        .unwrap();

    world.service.pass().await;
    wait_for(&world.copies, 2).await;
    let view = world.service.view("sam").await;
    assert_eq!((view.lists[0].outside, view.lists[0].previews_ready), (3, 2));
}

#[tokio::test]
async fn copies_already_made_stay_held_when_the_space_is_full() {
    let world = world(true, true);
    let kept = songs_with_videos(&world, "k", 3, 0);
    world.sources.lists.lock().insert(
        "sam".into(),
        vec![list("oi-kept", kept), list("oi-new", ids("out-n", 0))],
    );
    world
        .service
        .choose("sam", vec!["oi-kept".into()], false)
        .await
        .unwrap();
    world.service.pass().await;
    wait_for(&world.copies, 3).await;
    assert_eq!(world.copies.kept_bytes(), 3_000);

    // Later, the songs fetched ahead fill the space, and a new list is picked.
    *world.now.lock() += chrono::Duration::minutes(150);
    *world.flac.bytes.lock() = u64::MAX;
    let fresh = songs_with_videos(&world, "n", 2, 0);
    world.sources.lists.lock().insert(
        "sam".into(),
        vec![list("oi-kept", ids("out-k", 3)), list("oi-new", fresh)],
    );
    world
        .service
        .choose("sam", vec!["oi-kept".into(), "oi-new".into()], false)
        .await
        .unwrap();
    world.service.pass().await;
    tokio::time::sleep(StdDuration::from_millis(100)).await;

    // No room for the new list's copies, but the kept list's are held again, past their first hold.
    assert_eq!(world.copies.sizes().0, 3, "nothing new copied");
    *world.now.lock() += chrono::Duration::minutes(60);
    assert_eq!(world.copies.kept_bytes(), 3_000, "still held");
}

#[tokio::test]
async fn a_picked_list_hidden_for_now_stays_picked() {
    let world = world(true, true);
    world.sources.lists.lock().insert(
        "sam".into(),
        vec![list("oi-a", ids("out-a", 1)), list("oc-chart", ids("out-c", 1))],
    );
    world
        .service
        .choose("sam", vec!["oi-a".into(), "oc-chart".into()], true)
        .await
        .unwrap();
    // The chart is not made again yet after a restart.
    world
        .sources
        .lists
        .lock()
        .insert("sam".into(), vec![list("oi-a", ids("out-a", 1))]);
    assert!(
        world
            .service
            .choose("sam", vec!["oi-a".into(), "oc-chart".into()], false)
            .await
            .is_ok()
    );
    assert_eq!(
        world.service.choose("sam", vec!["oi-zzz".into()], false).await,
        Err(UnknownList("oi-zzz".into()))
    );
}

#[test]
fn keep_ready_knows_the_chosen_version_as_the_songs_video() {
    let registry = crate::services::soulseek::ExternalIdRegistry::in_memory();
    let id = registry.register(octo_core::soulseek::SoulseekRouting {
        kind: octo_core::soulseek::RoutingKind::Song,
        artist: Some("Drake".into()),
        title: Some("Started".into()),
        ..Default::default()
    });
    assert_eq!(super::keep_ready::known_video_in(&registry, &id), None);
    registry.set_you_tube_id(&id, "old");
    registry
        .choose_stream_video(
            &id,
            crate::services::soulseek::StreamChoice {
                video: "new".into(),
                by: "ede".into(),
                at: Utc::now(),
                ..Default::default()
            },
        )
        .expect("stored");
    assert_eq!(
        super::keep_ready::known_video_in(&registry, &id).as_deref(),
        Some("new")
    );
}
