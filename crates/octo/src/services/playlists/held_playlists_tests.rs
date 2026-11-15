//! The held service against an in-memory Navidrome and library: syncs and merges, the two-phase
//! mirror write, binding, the lock and its deadline, saves, gone playlists, the keeper and
//! keep-marks paths, caps and seed recovery. Every wait is on a park hook, a join handle or a
//! completion handle; nothing sleeps.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use octo_core::common::Clock;
use octo_core::models::domain::Song;
use octo_core::soulseek::{RoutingKind, SoulseekRouting};
use parking_lot::Mutex;

use super::*;
use crate::services::common::acquisition_tracker::{AcquisitionEnd, AcquisitionTracker};
use crate::services::imports::{ImportList, ImportPlaylists, ImportTrack, PlaylistNavidrome};
use crate::services::library::navidrome_playlist_api::{PlaylistInfo, SongFacts, TrackFacts};
use crate::services::local::ParsedExternalId;

const OWNER: &str = "alice";

fn facts(id: &str, title: &str, artist: &str) -> SongFacts {
    SongFacts {
        id: id.to_string(),
        title: title.to_string(),
        artist: artist.to_string(),
        album: "Album".to_string(),
        seconds: Some(200),
        isrcs: Vec::new(),
        explicit_status: None,
        library_id: None,
        missing: false,
    }
}

#[derive(Default, Clone)]
struct FakeList {
    owner: String,
    name: String,
    rows: Vec<String>,
}

#[derive(Default)]
struct FakeState {
    lists: HashMap<String, FakeList>,
    songs: HashMap<String, SongFacts>,
    calls: Vec<String>,
    /// Operations that fail once each: "read", "info", "facts", "add", "remove".
    fail_once: Vec<&'static str>,
    /// Song ids Navidrome silently drops on an add.
    drop: HashSet<String>,
    /// After Octo's next `op` on a playlist, another writer appends these songs.
    after: Vec<(&'static str, String, Vec<String>)>,
    made: usize,
}

fn take(state: &mut FakeState, op: &str) -> bool {
    match state.fail_once.iter().position(|failing| *failing == op) {
        Some(at) => {
            state.fail_once.remove(at);
            true
        }
        None => false,
    }
}

fn foreign(state: &mut FakeState, op: &str, id: &str) {
    if let Some(at) = state
        .after
        .iter()
        .position(|(after, playlist, _)| *after == op && playlist == id)
    {
        let (_, _, rows) = state.after.remove(at);
        if let Some(list) = state.lists.get_mut(id) {
            list.rows.extend(rows);
        }
    }
}

/// Navidrome's admin API in memory.
#[derive(Default)]
struct FakeNavidrome(Mutex<FakeState>);

impl FakeNavidrome {
    fn song(&self, id: &str, title: &str, artist: &str) {
        self.0
            .lock()
            .songs
            .insert(id.to_string(), facts(id, title, artist));
    }

    fn song_facts_are(&self, facts: SongFacts) {
        self.0.lock().songs.insert(facts.id.clone(), facts);
    }

    fn forget_song(&self, id: &str) {
        self.0.lock().songs.remove(id);
    }

    fn playlist(&self, id: &str, owner: &str, rows: &[&str]) {
        self.0.lock().lists.insert(
            id.to_string(),
            FakeList {
                owner: owner.to_string(),
                name: "Road trip".to_string(),
                rows: rows.iter().map(|row| row.to_string()).collect(),
            },
        );
    }

    fn delete_list(&self, id: &str) {
        self.0.lock().lists.remove(id);
    }

    fn rows(&self, id: &str) -> Vec<String> {
        self.0
            .lock()
            .lists
            .get(id)
            .map(|list| list.rows.clone())
            .unwrap_or_default()
    }

    fn set_rows(&self, id: &str, rows: &[&str]) {
        if let Some(list) = self.0.lock().lists.get_mut(id) {
            list.rows = rows.iter().map(|row| row.to_string()).collect();
        }
    }

    fn writes(&self) -> Vec<String> {
        self.0
            .lock()
            .calls
            .iter()
            .filter(|call| call.starts_with("add ") || call.starts_with("remove "))
            .cloned()
            .collect()
    }

    fn clear_calls(&self) {
        self.0.lock().calls.clear();
    }

    fn fail_once(&self, op: &'static str) {
        self.0.lock().fail_once.push(op);
    }

    fn after(&self, op: &'static str, id: &str, rows: &[&str]) {
        self.0.lock().after.push((
            op,
            id.to_string(),
            rows.iter().map(|row| row.to_string()).collect(),
        ));
    }

    fn drops(&self, id: &str) {
        self.0.lock().drop.insert(id.to_string());
    }
}

#[async_trait]
impl HeldNavidrome for FakeNavidrome {
    async fn playlist_info(&self, id: &str) -> Result<Option<PlaylistInfo>, String> {
        let mut state = self.0.lock();
        if take(&mut state, "info") {
            return Err("no answer".to_string());
        }
        Ok(state.lists.get(id).map(|list| PlaylistInfo {
            id: id.to_string(),
            name: list.name.clone(),
            owner: list.owner.clone(),
            smart: false,
            synced: false,
            changed: None,
            song_count: list.rows.len() as i64,
            duration: 0,
        }))
    }

    async fn read_track_facts(&self, id: &str) -> Result<Option<Vec<TrackFacts>>, String> {
        let mut state = self.0.lock();
        if take(&mut state, "read") {
            return Err("no answer".to_string());
        }
        let Some(list) = state.lists.get(id) else {
            return Ok(None);
        };
        Ok(Some(
            list.rows
                .iter()
                .enumerate()
                .map(|(at, row)| {
                    let song = state.songs.get(row);
                    TrackFacts {
                        position: (at + 1).to_string(),
                        media_file_id: row.clone(),
                        title: song.map(|song| song.title.clone()).unwrap_or_default(),
                        artist: song.map(|song| song.artist.clone()).unwrap_or_default(),
                        seconds: song.and_then(|song| song.seconds),
                        missing: song.is_none_or(|song| song.missing),
                    }
                })
                .collect(),
        ))
    }

    async fn song_facts(&self, id: &str) -> Result<Option<SongFacts>, String> {
        let mut state = self.0.lock();
        if take(&mut state, "facts") {
            return Err("no answer".to_string());
        }
        Ok(state.songs.get(id).cloned())
    }

    async fn remove_positions(&self, id: &str, positions: &[String]) -> Result<bool, String> {
        let mut state = self.0.lock();
        state.calls.push(format!("remove {}", positions.join(",")));
        if take(&mut state, "remove") {
            return Ok(false);
        }
        let mut places: Vec<usize> = positions.iter().filter_map(|p| p.parse().ok()).collect();
        places.sort_unstable();
        places.dedup();
        let Some(list) = state.lists.get_mut(id) else {
            return Ok(false);
        };
        for place in places.into_iter().rev() {
            if (1..=list.rows.len()).contains(&place) {
                list.rows.remove(place - 1);
            }
        }
        foreign(&mut state, "remove", id);
        Ok(true)
    }

    async fn add_tracks(&self, id: &str, songs: &[String]) -> Result<bool, String> {
        let mut state = self.0.lock();
        state.calls.push(format!("add {}", songs.join(",")));
        if take(&mut state, "add") {
            return Ok(false);
        }
        let kept: Vec<String> = songs
            .iter()
            .filter(|song| state.songs.contains_key(*song) && !state.drop.contains(*song))
            .cloned()
            .collect();
        let Some(list) = state.lists.get_mut(id) else {
            return Ok(false);
        };
        list.rows.extend(kept);
        foreign(&mut state, "add", id);
        Ok(true)
    }

    async fn list_playlists(&self) -> Option<Vec<ListedPlaylist>> {
        let state = self.0.lock();
        Some(
            state
                .lists
                .iter()
                .map(|(id, list)| ListedPlaylist {
                    id: id.clone(),
                    name: list.name.clone(),
                    owner: list.owner.clone(),
                    created: None,
                })
                .collect(),
        )
    }
}

#[async_trait]
impl PlaylistNavidrome for FakeNavidrome {
    async fn create(&self, name: &str, _comment: Option<&str>) -> Option<String> {
        let mut state = self.0.lock();
        state.made += 1;
        let id = format!("made{}", state.made);
        state.lists.insert(
            id.clone(),
            FakeList {
                owner: "admin".to_string(),
                name: name.to_string(),
                rows: Vec::new(),
            },
        );
        Some(id)
    }

    async fn give_to(&self, id: &str, user: &str) -> bool {
        if let Some(list) = self.0.lock().lists.get_mut(id) {
            list.owner = user.to_string();
        }
        true
    }

    async fn exists(&self, id: &str) -> Option<bool> {
        Some(self.0.lock().lists.contains_key(id))
    }

    async fn read_tracks(&self, id: &str) -> Option<Vec<(String, String)>> {
        self.0.lock().lists.get(id).map(|list| {
            list.rows
                .iter()
                .enumerate()
                .map(|(at, row)| ((at + 1).to_string(), row.clone()))
                .collect()
        })
    }

    async fn remove(&self, id: &str, positions: &[String]) -> bool {
        matches!(
            HeldNavidrome::remove_positions(self, id, positions).await,
            Ok(true)
        )
    }

    async fn add(&self, id: &str, songs: &[String]) -> bool {
        matches!(HeldNavidrome::add_tracks(self, id, songs).await, Ok(true))
    }

    async fn delete(&self, id: &str) -> bool {
        self.0.lock().lists.remove(id).is_some()
    }

    fn admin_name(&self) -> Option<String> {
        Some("admin".to_string())
    }
}

/// The library and catalog in memory.
#[derive(Default)]
struct FakeLibrary {
    /// (owner, external id) -> the owner's own version's path.
    own: Mutex<HashMap<(String, String), String>>,
    /// path -> library id.
    at_path: Mutex<HashMap<String, String>>,
    personal: Mutex<Vec<(i64, String)>>,
    libraries: Mutex<HashMap<String, i64>>,
    not_ready: AtomicBool,
    catalog: Mutex<HashMap<String, Song>>,
}

impl FakeLibrary {
    fn own(&self, owner: &str, external_id: &str, path: &str, library_id: &str) {
        self.own
            .lock()
            .insert((owner.to_lowercase(), external_id.to_string()), path.to_string());
        self.at_path
            .lock()
            .insert(path.to_string(), library_id.to_string());
    }
}

#[async_trait]
impl HeldLibrary for FakeLibrary {
    async fn own_version_path(&self, owner: &str, _provider: &str, external_id: &str) -> Option<String> {
        self.own
            .lock()
            .get(&(owner.to_lowercase(), external_id.to_string()))
            .cloned()
    }

    async fn library_id_at(&self, _artist: &str, _title: &str, path: &str) -> Option<String> {
        self.at_path.lock().get(path).cloned()
    }

    async fn downloaded_from(&self, _library_id: &str) -> Option<(String, String)> {
        None
    }

    fn readable(&self, owner: &str, library_id: &str, library: Option<i64>) -> Readable {
        let library = library.or_else(|| self.libraries.lock().get(library_id).copied());
        readable_by(
            owner,
            &self.personal.lock(),
            library,
            !self.not_ready.load(Ordering::SeqCst),
        )
    }

    async fn catalog_song(&self, _provider: &str, external_id: &str) -> Option<Song> {
        self.catalog.lock().get(external_id).cloned()
    }

    fn parse_external_id(&self, id: &str) -> ParsedExternalId {
        let Some(rest) = id.strip_prefix("ext-") else {
            return (false, None, None, None);
        };
        let mut parts = rest.splitn(3, '-');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(provider), Some(kind), Some(external_id)) => (
                true,
                Some(provider.to_string()),
                Some(kind.to_string()),
                Some(external_id.to_string()),
            ),
            _ => (false, None, None, None),
        }
    }
}

/// The owner sees every entry.
struct AllServed;

#[async_trait]
impl ServedView for AllServed {
    async fn served(&self, entries: &[HeldEntry]) -> Option<Vec<usize>> {
        Some((0..entries.len()).collect())
    }
}

fn caller() -> HeldCaller {
    HeldCaller {
        username: OWNER.to_string(),
        user: OWNER.to_string(),
        client: "Symfonium".to_string(),
    }
}

fn ok_answer() -> RelayAnswer {
    RelayAnswer {
        ok: true,
        body: Vec::new(),
        content_type: None,
    }
}

struct World {
    navidrome: Arc<FakeNavidrome>,
    library: Arc<FakeLibrary>,
    registry: Arc<ExternalIdRegistry>,
    held: Arc<HeldPlaylists>,
    _dir: Option<tempfile::TempDir>,
}

impl World {
    fn new() -> World {
        World::with(
            false,
            PlaylistCaps::PERSON,
            PlaylistCaps::SERVER,
            Duration::from_secs(20),
        )
    }

    /// A world whose held store is a temp folder (gone files are files).
    fn on_disk() -> World {
        World::with(
            true,
            PlaylistCaps::PERSON,
            PlaylistCaps::SERVER,
            Duration::from_secs(20),
        )
    }

    fn with(on_disk: bool, person: usize, server: usize, lock_wait: Duration) -> World {
        let dir = on_disk.then(|| tempfile::tempdir().expect("a temp dir"));
        let navidrome = Arc::new(FakeNavidrome::default());
        let library = Arc::new(FakeLibrary::default());
        let registry = Arc::new(ExternalIdRegistry::in_memory());
        let seed_index = Arc::new(SeedIndex::new());
        registry.set_recovery_sources(vec![seed_index.recovery()]);
        let store = match &dir {
            Some(dir) => HeldStore::new(Some(dir.path().join("octo-playlists"))),
            None => HeldStore::in_memory(),
        };
        let held = HeldPlaylists::start(HeldParts {
            store,
            navidrome: navidrome.clone(),
            library: library.clone(),
            registry: Arc::clone(&registry),
            seed_index,
            caps: Arc::new(PlaylistCaps::with_limits(person, server)),
            family: None,
            timing: HeldTiming { lock_wait },
            clock: Clock::system(),
        });
        World {
            navidrome,
            library,
            registry,
            held,
            _dir: dir,
        }
    }

    /// An outside song the registry knows, by its registry id (kept for the test's whole run, so
    /// ids and literal library ids share one type).
    fn outside(&self, title: &str) -> &'static str {
        self.outside_by(title, None)
    }

    fn outside_by(&self, title: &str, explicit: Option<i32>) -> &'static str {
        let id = self.registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            title: Some(title.to_string()),
            artist: Some("Artist".to_string()),
            album: Some("Album".to_string()),
            duration: Some(200),
            explicit_content: explicit,
            you_tube_id: Some(format!("v-{title}")),
            ..Default::default()
        });
        Box::leak(id.into_boxed_str())
    }

    /// Whether the playlist's served ids hold `song`.
    fn has(&self, id: &str, song: &str) -> bool {
        self.shown(id).iter().any(|shown| shown == song)
    }

    fn songs(&self, ids: &[&str]) {
        for id in ids {
            self.navidrome.song(id, &format!("Song {id}"), "Artist");
        }
    }

    async fn add_as(&self, owner: &str, id: &str, songs: &[&str]) -> Result<UpdateOutcome, HeldError> {
        let ids: Vec<String> = songs.iter().map(|song| song.to_string()).collect();
        let built = self
            .held
            .build_entries(owner, &ids, &self.held.record_ids(id))
            .await?;
        let navidrome = Arc::clone(&self.navidrome);
        let playlist = id.to_string();
        let plain_ids = ids.clone();
        let plain: Relay = Box::pin(async move {
            let _ = HeldNavidrome::add_tracks(navidrome.as_ref(), &playlist, &plain_ids).await;
            Some(ok_answer())
        });
        self.held
            .update(UpdateRequest {
                id: id.to_string(),
                caller: HeldCaller {
                    username: owner.to_string(),
                    user: owner.to_string(),
                    client: "Symfonium".to_string(),
                },
                owner: owner.to_string(),
                remove_indexes: Vec::new(),
                remove_songs: Vec::new(),
                adds: built,
                fields: None,
                plain,
                view: Arc::new(AllServed),
            })
            .await
    }

    async fn add(&self, id: &str, songs: &[&str]) -> Result<UpdateOutcome, HeldError> {
        self.add_as(OWNER, id, songs).await
    }

    async fn remove_index(&self, id: &str, index: usize) -> Result<UpdateOutcome, HeldError> {
        self.held
            .update(UpdateRequest {
                id: id.to_string(),
                caller: caller(),
                owner: OWNER.to_string(),
                remove_indexes: vec![index],
                remove_songs: Vec::new(),
                adds: Vec::new(),
                fields: None,
                plain: Box::pin(async { Some(ok_answer()) }),
                view: Arc::new(AllServed),
            })
            .await
    }

    /// The entries as served ids: library id, else outside id.
    fn shown(&self, id: &str) -> Vec<String> {
        self.held
            .view_of(id)
            .map(|view| {
                view.entries
                    .iter()
                    .map(|entry| served_id(entry).to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn entry_of(&self, id: &str, outside: &str) -> Option<HeldEntry> {
        self.held
            .view_of(id)?
            .entries
            .into_iter()
            .find(|entry| entry.outside_id.as_deref() == Some(outside))
    }
}

fn strings(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

/// A held playlist `p1`: library songs `a`, then outside songs `s1`, `s2`, with `s1` bound to `l1`
/// (the owner's own version) by the start sweep. Answers (s1, s2).
async fn bound_world(w: &World) -> (&'static str, &'static str) {
    w.songs(&["a"]);
    w.navidrome.playlist("p1", OWNER, &["a"]);
    let s1 = w.outside("Song");
    let s2 = w.outside("Other");
    w.add("p1", &[s1, s2]).await.expect("added");
    w.navidrome.song("l1", "Song", "Artist");
    w.library.own(OWNER, s1, "/music/alice/song.flac", "l1");
    w.held.start_sweep().await;
    assert_eq!(w.shown("p1"), strings(&["a", "l1", s2]), "s1 bound in its place");
    assert_eq!(w.navidrome.rows("p1"), strings(&["a", "l1"]));
    (s1, s2)
}

// ---- the merge through a sync -----------------------------------------------------------------

#[tokio::test]
async fn t6_a_library_song_whose_file_went_turns_back_to_streamed() {
    let w = World::new();
    let (s1, s2) = bound_world(&w).await;
    w.navidrome.forget_song("l1");
    w.library.own.lock().clear();
    w.navidrome.set_rows("p1", &["a"]);
    w.held.serve("p1").await.expect("served");
    assert_eq!(
        w.shown("p1"),
        strings(&["a", s1, s2]),
        "s1 streams again in its place"
    );
    assert!(w.entry_of("p1", s1).expect("there").library_id.is_none());
    assert_eq!(w.held.stored("p1").expect("held").mirror, strings(&["a"]));
}

#[tokio::test]
async fn t6_a_presence_that_could_not_be_told_changes_nothing() {
    let w = World::new();
    let (s1, s2) = bound_world(&w).await;
    w.navidrome.forget_song("l1");
    w.library.own.lock().clear();
    w.navidrome.set_rows("p1", &["a"]);
    w.navidrome.fail_once("facts");
    let view = w.held.serve("p1").await.expect("served").expect("held");
    assert_eq!(view.entries.len(), 3, "the stored entries are served");
    assert_eq!(w.shown("p1"), strings(&["a", "l1", s2]), "nothing changed");
    assert_eq!(w.held.stored("p1").expect("held").mirror, strings(&["a", "l1"]));
    w.held.serve("p1").await.expect("served");
    assert_eq!(w.shown("p1"), strings(&["a", s1, s2]), "the next sync merges");
}

#[tokio::test]
async fn t7_a_replaced_file_keeps_its_place_and_its_fallback() {
    let w = World::new();
    let (s1, s2) = bound_world(&w).await;
    w.navidrome.song("l9", "Song", "Artist");
    w.navidrome.set_rows("p1", &["a", "l9"]);
    w.held.serve("p1").await.expect("served");
    assert_eq!(w.shown("p1"), strings(&["a", "l9", s2]));
    assert_eq!(
        w.entry_of("p1", s1)
            .expect("the entry kept its outside id")
            .library_id
            .as_deref(),
        Some("l9")
    );
}

#[tokio::test]
async fn t7_a_different_take_is_not_paired() {
    let w = World::new();
    let (s1, s2) = bound_world(&w).await;
    w.navidrome.song("l8", "Song (Live)", "Artist");
    w.navidrome.set_rows("p1", &["a", "l8"]);
    w.held.serve("p1").await.expect("served");
    assert!(
        w.entry_of("p1", s1).is_none(),
        "the old song is still in the library, so its removal stands"
    );
    assert_eq!(
        w.shown("p1"),
        strings(&["a", s2, "l8"]),
        "the live take is a new entry at the end"
    );
}

// ---- the two-phase write ----------------------------------------------------------------------

/// `p1` holds a, b, c and the outside song s, and removing `a` writes the mirror: remove
/// positions 1 to 3, then add b and c.
async fn written_world(w: &World) -> &'static str {
    w.songs(&["a", "b", "c", "x"]);
    w.navidrome.playlist("p1", OWNER, &["a", "b", "c"]);
    let s = w.outside("Song");
    w.add("p1", &[s]).await.expect("added");
    w.navidrome.clear_calls();
    s
}

#[tokio::test]
async fn t8_a_write_stopped_after_the_removal_resumes_on_the_next_sync() {
    let w = World::new();
    let s = written_world(&w).await;
    w.navidrome.fail_once("add");
    w.remove_index("p1", 0).await.expect("answered");
    assert!(
        w.navidrome.rows("p1").is_empty(),
        "the removal went through, the add did not"
    );
    assert!(
        w.held.stored("p1").expect("held").pending.is_some(),
        "the write is on record"
    );
    w.held.serve("p1").await.expect("served");
    assert_eq!(w.navidrome.rows("p1"), strings(&["b", "c"]));
    assert_eq!(w.shown("p1"), strings(&["b", "c", s]));
    assert!(w.held.stored("p1").expect("held").pending.is_none());
}

#[tokio::test]
async fn t8_a_stopped_write_and_a_foreign_add_keep_the_foreign_add() {
    let w = World::new();
    let s = written_world(&w).await;
    w.navidrome.fail_once("add");
    w.remove_index("p1", 0).await.expect("answered");
    w.navidrome.set_rows("p1", &["x"]);
    w.held.serve("p1").await.expect("served");
    assert_eq!(
        w.navidrome.rows("p1"),
        strings(&["x"]),
        "no write over the other writer's list"
    );
    assert!(w.has("p1", "x"), "their add is kept");
    assert!(w.has("p1", s));
    assert!(w.held.stored("p1").expect("held").pending.is_none());
}

#[tokio::test]
async fn t8_a_write_stopped_before_its_first_call_and_a_foreign_removal_keep_the_removal() {
    let w = World::new();
    written_world(&w).await;
    w.navidrome.fail_once("remove");
    w.remove_index("p1", 0).await.expect("answered");
    assert_eq!(w.navidrome.rows("p1"), strings(&["a", "b", "c"]));
    w.navidrome.set_rows("p1", &["a", "b"]);
    w.held.serve("p1").await.expect("served");
    assert!(!w.has("p1", "c"), "their removal is kept");
    assert!(!w.navidrome.rows("p1").iter().any(|row| row == "c"));
    assert!(w.held.stored("p1").expect("held").pending.is_none());
}

#[tokio::test]
async fn t9_a_foreign_add_during_the_write_survives_and_octo_writes_once() {
    let w = World::new();
    let s = written_world(&w).await;
    w.navidrome.after("add", "p1", &["x"]);
    w.remove_index("p1", 0).await.expect("removed");
    assert_eq!(
        w.navidrome.writes(),
        strings(&["remove 1,2,3", "add b,c"]),
        "one write"
    );
    assert_eq!(w.navidrome.rows("p1"), strings(&["b", "c", "x"]));
    assert_eq!(
        w.shown("p1"),
        strings(&["b", "c", s, "x"]),
        "their add lands at the end"
    );
    assert!(w.held.stored("p1").expect("held").pending.is_none());
}

#[tokio::test]
async fn t10_an_id_navidrome_drops_is_written_once_and_never_again() {
    let w = World::new();
    written_world(&w).await;
    w.navidrome.song("d", "Dropped", "Artist");
    w.navidrome.drops("d");
    w.add("p1", &["d"]).await.expect("answered");
    assert_eq!(w.navidrome.writes(), strings(&["add d"]));
    assert!(!w.has("p1", "d"), "Navidrome's refusal is reflected");
    assert!(w.held.stored("p1").expect("held").pending.is_none());
    w.navidrome.clear_calls();
    w.held.serve("p1").await.expect("served");
    assert!(w.navidrome.writes().is_empty(), "the next read writes nothing");
}

// ---- binding ----------------------------------------------------------------------------------

#[tokio::test]
async fn t11_the_owners_finished_download_binds_through_the_tracker_then_releases() {
    let w = World::new();
    w.songs(&["a"]);
    w.navidrome.playlist("p1", OWNER, &["a"]);
    let s = w.outside("Song");
    w.add("p1", &[s]).await.expect("added");
    let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
    w.held.listen(&tracker);
    let (ended_tx, ended_rx) = tokio::sync::oneshot::channel::<()>();
    let ended_tx = Mutex::new(Some(ended_tx));
    tracker.subscribe_ended(Arc::new(move |_| {
        if let Some(tx) = ended_tx.lock().take() {
            let _ = tx.send(());
        }
    }));
    tracker.configure_watch(|watch| {
        watch.library_lookup = Some(Arc::new(
            |_: String, _: String, _: String| -> BoxFuture<'static, anyhow::Result<Option<String>>> {
                Box::pin(async { Ok(Some("l1".to_string())) })
            },
        ));
    });
    w.navidrome.song("l1", "Song", "Artist");
    w.library.own(OWNER, s, "/music/alice/song.flac", "l1");
    tracker.begin(
        "soulseek",
        s,
        None,
        Some(OWNER),
        Some("Artist"),
        Some("Song"),
        None,
    );
    tracker.imported(
        "soulseek",
        s,
        Some("Artist"),
        Some("Song"),
        Some("/music/alice/song.flac"),
    );
    ended_rx.await.expect("the row ended");
    w.held.binds_settled().await;
    assert_eq!(
        w.navidrome.rows("p1"),
        strings(&["a", "l1"]),
        "the library song in the entry's place"
    );
    assert!(
        w.held.view_of("p1").is_none(),
        "nothing streams, so the playlist is released, same id"
    );
}

#[tokio::test]
async fn t12_another_members_version_does_not_bind() {
    let w = World::new();
    w.songs(&["a"]);
    w.navidrome.playlist("p1", OWNER, &["a"]);
    let s = w.outside("Song");
    w.add("p1", &[s]).await.expect("added");
    w.navidrome.song("l1", "Song", "Artist");
    w.library.own("sam", s, "/music/sam/song.flac", "l1");
    w.held.start_sweep().await;
    assert!(w.entry_of("p1", s).expect("there").library_id.is_none());
}

#[tokio::test]
async fn t12_a_library_the_owner_cannot_read_does_not_bind() {
    let w = World::new();
    w.songs(&["a"]);
    w.navidrome.playlist("p1", OWNER, &["a"]);
    let s = w.outside("Song");
    w.add("p1", &[s]).await.expect("added");
    w.navidrome.song("l1", "Song", "Artist");
    w.library.own(OWNER, s, "/music/sam/song.flac", "l1");
    w.library.personal.lock().push((7, "sam".to_string()));
    w.library.libraries.lock().insert("l1".to_string(), 7);
    w.held.start_sweep().await;
    assert!(w.entry_of("p1", s).expect("there").library_id.is_none());
}

#[tokio::test]
async fn t12_a_clean_take_landed_by_an_album_heart_does_not_bind_and_the_same_take_does() {
    for (landed, binds) in [("clean", false), ("explicit", true)] {
        let w = World::new();
        w.songs(&["a"]);
        w.navidrome.playlist("p1", OWNER, &["a", "z"]);
        w.navidrome.song("z", "Filler", "Artist");
        let s = w.outside_by("Song", Some(1));
        let other = w.outside("Keep held");
        w.add("p1", &[s, other]).await.expect("added");
        let mut l5 = facts("l5", "Song", "Artist");
        l5.explicit_status = Some(landed.to_string());
        w.navidrome.song_facts_are(l5);
        w.library.own(OWNER, "99", "/music/alice/album/song.flac", "l5");
        let end = AcquisitionEnd {
            key: "deezer:99".to_string(),
            artist: Some("Artist".to_string()),
            title: Some("Song".to_string()),
            done: true,
            library_id: Some("l5".to_string()),
            album_keys: Vec::new(),
            path: Some("/music/alice/album/song.flac".to_string()),
            already_there: false,
        };
        w.held.bind_now("p1", &[end]).await;
        assert_eq!(
            w.entry_of("p1", s).expect("there").library_id.is_some(),
            binds,
            "a {landed} take"
        );
    }
}

#[tokio::test]
async fn t13_binding_happens_at_a_locked_sync_and_at_the_start_sweep() {
    let w = World::new();
    w.songs(&["a", "b"]);
    w.navidrome.playlist("p1", OWNER, &["a"]);
    let s1 = w.outside("Song");
    let s2 = w.outside("Other");
    w.add("p1", &[s1, s2]).await.expect("added");
    w.navidrome.song("l1", "Song", "Artist");
    w.library.own(OWNER, s1, "/music/alice/song.flac", "l1");
    w.add("p1", &["b"]).await.expect("added");
    assert_eq!(
        w.entry_of("p1", s1).expect("there").library_id.as_deref(),
        Some("l1"),
        "the add's own sync bound it"
    );
    w.navidrome.song("l2", "Other", "Artist");
    w.library.own(OWNER, s2, "/music/alice/other.flac", "l2");
    w.held.start_sweep().await;
    assert!(
        w.held.view_of("p1").is_none(),
        "the sweep bound the last one and released the playlist"
    );
    assert_eq!(w.navidrome.rows("p1"), strings(&["a", "l1", "l2", "b"]));
}

// ---- gone playlists ---------------------------------------------------------------------------

const NAVIDROME_ID: &str = "0123456789abcdefABCDEF";
const FAMILY_ID: &str = "of0123456789abcdefABCD";
const IMPORT_ID: &str = "oi0123456789abcdefABCD";

#[tokio::test]
async fn t26_two_404s_keep_a_held_file_aside_and_never_drop_octo_placements() {
    let w = World::on_disk();
    w.songs(&["a"]);
    w.navidrome.playlist("p1", OWNER, &["a"]);
    let s = w.outside("Song");
    w.add("p1", &[s]).await.expect("added");
    w.navidrome.delete_list("p1");
    let placed = strings(&[FAMILY_ID, IMPORT_ID, NAVIDROME_ID]);
    let first = w
        .held
        .check_gone(OWNER, Vec::new(), placed.clone())
        .expect("started");
    let report = first.await.expect("ran");
    assert!(
        w.held.stored("p1").expect("still held").gone_seen,
        "the first 404 marks it"
    );
    assert!(report.confirmed.is_empty());
    let second = w.held.check_gone(OWNER, Vec::new(), placed).expect("started");
    let report = second.await.expect("ran");
    assert!(w.held.view_of("p1").is_none());
    assert!(w.held.is_aside("p1"), "the second keeps the file aside");
    assert_eq!(
        report.confirmed,
        strings(&[NAVIDROME_ID]),
        "never an of or oi placement"
    );
}

#[tokio::test]
async fn t26_a_playlist_found_again_clears_its_mark() {
    let w = World::on_disk();
    w.songs(&["a"]);
    w.navidrome.playlist("p1", OWNER, &["a"]);
    let s = w.outside("Song");
    w.add("p1", &[s]).await.expect("added");
    w.navidrome.delete_list("p1");
    w.held
        .check_gone(OWNER, Vec::new(), Vec::new())
        .expect("started")
        .await
        .expect("ran");
    assert!(w.held.stored("p1").expect("held").gone_seen);
    w.navidrome.playlist("p1", OWNER, &["a"]);
    w.held
        .check_gone(OWNER, strings(&["p1"]), Vec::new())
        .expect("started")
        .await
        .expect("ran");
    assert!(!w.held.stored("p1").expect("held").gone_seen);
}

async fn aside_world(w: &World) -> &'static str {
    w.songs(&["a", "y"]);
    w.navidrome.playlist("p1", OWNER, &["a"]);
    let s = w.outside("Song");
    w.add("p1", &[s]).await.expect("added");
    w.navidrome.delete_list("p1");
    for _ in 0..2 {
        w.held
            .check_gone(OWNER, Vec::new(), Vec::new())
            .expect("started")
            .await
            .expect("ran");
    }
    assert!(w.held.is_aside("p1"));
    s
}

#[tokio::test]
async fn t45_a_kept_aside_file_comes_back_when_its_id_answers_again() {
    let w = World::on_disk();
    let s = aside_world(&w).await;
    w.navidrome.playlist("p1", OWNER, &["a"]);
    w.held
        .check_gone(OWNER, strings(&["p1"]), Vec::new())
        .expect("started")
        .await
        .expect("ran");
    assert!(!w.held.is_aside("p1"));
    assert_eq!(w.shown("p1"), strings(&["a", s]));
}

#[tokio::test]
async fn t45_an_outside_add_first_merges_the_kept_aside_file_never_overwrites_it() {
    let w = World::on_disk();
    let s = aside_world(&w).await;
    w.navidrome.playlist("p1", OWNER, &["a", "y"]);
    let s3 = w.outside("Third");
    w.add("p1", &[s3]).await.expect("added");
    assert!(!w.held.is_aside("p1"));
    assert_eq!(w.shown("p1"), strings(&["a", s, "y", s3]));
}

// ---- the lock ---------------------------------------------------------------------------------

#[tokio::test]
async fn t27_the_lock_serializes_writers_and_both_changes_are_kept() {
    let w = Arc::new(World::new());
    w.songs(&["a"]);
    w.navidrome.playlist("p1", OWNER, &["a"]);
    let s0 = w.outside("Zero");
    w.add("p1", &[s0]).await.expect("held");
    let s1 = w.outside("One");
    let s2 = w.outside("Two");
    let (reached, go) = w.held.park_at(Park::AfterSync);
    let first = {
        let w = Arc::clone(&w);
        tokio::spawn(async move { w.add("p1", &[s1]).await })
    };
    reached.await.expect("the first writer synced");
    let second = {
        let w = Arc::clone(&w);
        tokio::spawn(async move { w.add("p1", &[s2]).await })
    };
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(!w.has("p1", s2), "the second writer waits for the lock");
    go.send(()).expect("released");
    first.await.expect("joined").expect("first added");
    second.await.expect("joined").expect("second added");
    assert_eq!(w.shown("p1"), strings(&["a", s0, s1, s2]));
}

#[tokio::test]
async fn t28_two_conversions_of_one_plain_playlist_keep_both_songs() {
    let w = Arc::new(World::new());
    w.songs(&["a"]);
    w.navidrome.playlist("p1", OWNER, &["a"]);
    let s1 = w.outside("One");
    let s2 = w.outside("Two");
    let (reached, go) = w.held.park_at(Park::AfterSync);
    let first = {
        let w = Arc::clone(&w);
        tokio::spawn(async move { w.add("p1", &[s1]).await })
    };
    reached.await.expect("the first writer found it plain");
    let second = {
        let w = Arc::clone(&w);
        tokio::spawn(async move { w.add("p1", &[s2]).await })
    };
    go.send(()).expect("released");
    first.await.expect("joined").expect("converted");
    second.await.expect("joined").expect("added");
    assert_eq!(w.shown("p1"), strings(&["a", s1, s2]));
}

#[tokio::test]
async fn t29_a_busy_write_answers_busy_and_a_busy_read_serves_the_stored_entries() {
    let w = World::with(
        false,
        PlaylistCaps::PERSON,
        PlaylistCaps::SERVER,
        Duration::from_secs(1),
    );
    w.songs(&["a", "x"]);
    w.navidrome.playlist("p1", OWNER, &["a"]);
    let s = w.outside("Song");
    w.add("p1", &[s]).await.expect("held");
    let guard = w.held.lock("p1").await.expect("locked");
    let s2 = w.outside("Two");
    assert!(matches!(w.add("p1", &[s2]).await, Err(HeldError::Busy)));
    assert_eq!(
        HeldError::Busy.message(),
        "This playlist is busy. Try again in a moment."
    );
    w.navidrome.set_rows("p1", &["a", "x"]);
    let view = w.held.serve("p1").await.expect("served").expect("held");
    assert_eq!(view.entries.len(), 2, "the stored entries, without a sync");
    drop(guard);
    assert!(!w.has("p1", s2), "the timed-out write never ran later");
}

#[tokio::test]
async fn t30_a_failed_pending_save_aborts_before_any_navidrome_call() {
    let w = World::new();
    let s = written_world(&w).await;
    w.held.fail_writes(true);
    assert!(matches!(
        w.remove_index("p1", 0).await,
        Err(HeldError::SaveFailed)
    ));
    assert!(w.navidrome.writes().is_empty(), "no Navidrome call");
    assert_eq!(w.navidrome.rows("p1"), strings(&["a", "b", "c"]));
    assert_eq!(w.shown("p1"), strings(&["a", "b", "c", s]), "memory is put back");
    assert!(w.held.stored("p1").expect("held").pending.is_none());
    w.held.fail_writes(false);
    w.held.serve("p1").await.expect("served");
    assert!(
        w.navidrome.writes().is_empty(),
        "no later sync writes the refused change"
    );
}

#[tokio::test]
async fn t41_a_read_before_a_bind_does_not_merge_its_stale_list_after_the_lock() {
    let w = Arc::new(World::new());
    w.songs(&["x"]);
    w.navidrome.playlist("p1", OWNER, &[]);
    let s1 = w.outside("Song");
    let s2 = w.outside("Other");
    w.add("p1", &[s1, s2]).await.expect("held");
    w.navidrome.set_rows("p1", &["x"]);
    let (reached, go) = w.held.park_at(Park::AfterPreRead);
    let reader = {
        let w = Arc::clone(&w);
        tokio::spawn(async move { w.held.serve("p1").await })
    };
    reached.await.expect("the reader read [x] without the lock");
    w.navidrome.song("l1", "Song", "Artist");
    w.library.own(OWNER, s1, "/music/alice/song.flac", "l1");
    w.held.start_sweep().await;
    assert!(w.has("p1", "l1"), "the sweep merged x and bound s1");
    go.send(()).expect("released");
    reader.await.expect("joined").expect("served");
    assert!(w.has("p1", "l1"), "the reader's locked sync read Navidrome again");
    assert!(w.navidrome.rows("p1").iter().any(|row| row == "l1"));
}

// ---- keep-marks and the keeper ----------------------------------------------------------------

#[tokio::test]
async fn t31_a_keep_marks_swap_on_a_held_playlist_keeps_place_and_fallback() {
    let w = World::new();
    let (s1, s2) = bound_world(&w).await;
    w.navidrome.song("l2", "Song", "Artist");
    let guard = w.held.lock("p1").await.expect("locked");
    assert!(w.held.is_held(&guard));
    let moved = w
        .held
        .swap_library(&guard, "l1", "l2", None)
        .await
        .expect("swapped");
    drop(guard);
    assert_eq!(moved, vec![2], "Navidrome's position of the swapped song");
    assert_eq!(w.shown("p1"), strings(&["a", "l2", s2]));
    assert_eq!(
        w.entry_of("p1", s1)
            .expect("the fallback is kept")
            .library_id
            .as_deref(),
        Some("l2")
    );
    assert_eq!(w.navidrome.rows("p1"), strings(&["a", "l2"]));
}

#[tokio::test]
async fn t31_a_plain_playlist_swap_holds_the_same_lock() {
    let w = World::with(
        false,
        PlaylistCaps::PERSON,
        PlaylistCaps::SERVER,
        Duration::from_secs(1),
    );
    w.songs(&["a"]);
    w.navidrome.playlist("p2", OWNER, &["a"]);
    let guard = w.held.lock("p2").await.expect("keep-marks holds it");
    assert!(!w.held.is_held(&guard));
    let s = w.outside("Song");
    assert!(
        matches!(w.add("p2", &[s]).await, Err(HeldError::Busy)),
        "a conversion waits for the swap"
    );
    drop(guard);
    w.add("p2", &[s]).await.expect("converted after it");
}

#[tokio::test]
async fn t32_the_keeper_on_a_held_kept_playlist_keeps_streamed_entries_by_their_neighbors() {
    let w = World::new();
    w.songs(&["a", "b", "c"]);
    w.navidrome.playlist("p1", OWNER, &["a"]);
    let s = w.outside("Song");
    w.add("p1", &[s, "b"]).await.expect("held");
    assert_eq!(w.shown("p1"), strings(&["a", s, "b"]));
    let slot: HeldSlot = Arc::new(RwLock::new(Some(Arc::clone(&w.held))));
    let keeper = ImportPlaylists::new(w.navidrome.clone() as Arc<dyn PlaylistNavidrome>).with_held(slot);
    let list = ImportList {
        id: "spotify-p1".to_string(),
        owner: OWNER.to_string(),
        name: "Road trip".to_string(),
        playlist_id: Some("p1".to_string()),
        tracks: ["a", "b", "c"]
            .iter()
            .map(|id| ImportTrack {
                key: (*id).to_string(),
                library_id: Some((*id).to_string()),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    let result = keeper.sync(&list).await;
    assert!(result.ok, "{result:?}");
    assert_eq!(w.shown("p1"), strings(&["a", s, "b", "c"]));
    assert_eq!(w.navidrome.rows("p1"), strings(&["a", "b", "c"]));
}

// ---- no registry call under the store's mutex -------------------------------------------------

#[test]
#[should_panic(expected = "held store's mutex")]
fn t40_a_registry_call_under_the_store_mutex_is_caught() {
    let w = World::new();
    w.held.store.while_locked(|| w.held.reg().count());
}

#[test]
#[should_panic(expected = "held store's mutex")]
fn t40_a_navidrome_call_under_the_store_mutex_is_caught() {
    let w = World::new();
    w.held.store.while_locked(|| {
        let _ = w.held.nav();
    });
}

// ---- caps and saves ---------------------------------------------------------------------------

#[tokio::test]
async fn t49_the_add_past_a_cap_is_refused_and_a_failed_save_reverses_its_count() {
    let w = World::with(false, 2, 3, Duration::from_secs(20));
    w.songs(&["a"]);
    w.navidrome.playlist("p1", OWNER, &["a"]);
    let (s1, s2, s3) = (w.outside("One"), w.outside("Two"), w.outside("Three"));
    w.add("p1", &[s1, s2]).await.expect("two fit");
    assert!(matches!(w.add("p1", &[s3]).await, Err(HeldError::PersonCap)));
    assert_eq!(HeldError::PersonCap.message(), PERSON_CAP);
    w.held.fail_writes(true);
    assert!(matches!(
        w.remove_index("p1", 1).await,
        Err(HeldError::SaveFailed)
    ));
    w.held.fail_writes(false);
    assert!(
        matches!(w.add("p1", &[s3]).await, Err(HeldError::PersonCap)),
        "the failed removal changed no count"
    );
    w.remove_index("p1", 1).await.expect("removed");
    w.add("p1", &[s3]).await.expect("the removal made room");
    w.songs(&["b"]);
    w.navidrome.playlist("p2", "bob", &["b"]);
    let s4 = w.outside("Four");
    w.add_as("bob", "p2", &[s4]).await.expect("the server's third");
    let s5 = w.outside("Five");
    assert!(matches!(
        w.add_as("bob", "p2", &[s5]).await,
        Err(HeldError::ServerCap)
    ));
    assert_eq!(HeldError::ServerCap.message(), SERVER_CAP);
}

#[tokio::test]
async fn t49_a_merge_is_counted_and_never_refused() {
    let w = World::with(false, 2, 10, Duration::from_secs(20));
    let (s1, s2) = bound_world(&w).await;
    let s3 = w.outside("Three");
    w.add("p1", &[s3])
        .await
        .expect("two streamed, one bound: room for the third");
    w.navidrome.forget_song("l1");
    w.library.own.lock().clear();
    w.navidrome.set_rows("p1", &["a"]);
    w.held.serve("p1").await.expect("served");
    assert!(
        w.entry_of("p1", s1).expect("there").library_id.is_none(),
        "merged back past the cap"
    );
    assert!(w.has("p1", s2));
    let s4 = w.outside("Four");
    assert!(
        matches!(w.add("p1", &[s4]).await, Err(HeldError::PersonCap)),
        "the merge was counted"
    );
}

// ---- a bound entry's outside id recovers from its seed ----------------------------------------

#[tokio::test]
async fn t53_a_bound_entrys_outside_id_comes_back_after_ten_thousand_newer_ids() {
    let w = World::new();
    w.songs(&["a"]);
    w.navidrome.playlist("p1", OWNER, &["a"]);
    w.navidrome.playlist("p2", OWNER, &["a"]);
    let s1 = w.outside("Song");
    let (k1, k2) = (w.outside("Keep one"), w.outside("Keep two"));
    w.add("p1", &[s1, k1]).await.expect("held");
    w.add("p2", &[s1, k2]).await.expect("held");
    w.navidrome.song("l1", "Song", "Artist");
    w.library.own(OWNER, s1, "/music/alice/song.flac", "l1");
    w.held.start_sweep().await;
    assert_eq!(
        w.entry_of("p1", s1).expect("there").library_id.as_deref(),
        Some("l1")
    );
    for n in 0..10_050 {
        w.registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            title: Some(format!("Filler {n}")),
            artist: Some("Artist".to_string()),
            ..Default::default()
        });
    }
    assert!(w.registry.lookup(s1).is_some(), "restored from the seed index");
    let at = w
        .shown("p1")
        .iter()
        .position(|id| id == "l1")
        .expect("bound in p1");
    w.remove_index("p1", at).await.expect("removed from p1");
    for n in 0..10_050 {
        w.registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            title: Some(format!("More {n}")),
            artist: Some("Artist".to_string()),
            ..Default::default()
        });
    }
    assert!(w.registry.lookup(s1).is_some(), "p2 still holds it");
}
