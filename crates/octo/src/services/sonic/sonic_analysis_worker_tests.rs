//! Sounds alike's pass over the library: when it holds, what it sends to octo-sonic, what it
//! skips without touching the disk, how failures and crashes are remembered, and new arrivals
//! first.

use std::sync::atomic::{AtomicBool, AtomicUsize};

use chrono::TimeZone;
use octo_core::settings::{AppSettings, RadioSourceSettings};

use super::*;
use crate::services::sonic::sonic_client::{SonicDropped, SonicFeatures, SonicHealth};
use crate::services::sonic::transition_store::TransitionLookup;

#[derive(Default)]
struct Activity(AtomicBool);

impl IAcquisitionActivity for Activity {
    fn is_busy(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// octo-sonic: its health, and per file name an answer (features, an error, or a dropped
/// connection).
struct Sidecar {
    version: Mutex<Option<i32>>,
    /// The transition profile version it writes; `None` for one from before profiles.
    transition: Mutex<Option<i32>>,
    /// The profile revision it says and writes; `None` for one from before revisions, whose
    /// profiles do not say theirs.
    revision: Mutex<Option<i32>>,
    /// What each reading asked for.
    parts: Mutex<Vec<SonicParts>>,
    /// The published tempo each reading was handed.
    bpms: Mutex<Vec<Option<f64>>>,
    /// Its music folder is empty or unreadable: health answers 503.
    blind: AtomicBool,
    /// Sounds alike is off in .env, on an octo-sonic from before profiles: health answers 503
    /// with off.
    off: AtomicBool,
    /// Sounds alike is off in .env: it makes profiles only, and refuses to read the sound.
    profiles_only: AtomicBool,
    analysed: Mutex<Vec<String>>,
    errors: Mutex<HashMap<String, String>>,
    drops: Mutex<HashSet<String>>,
    /// Files it cannot find (404), as when it mounts another folder than Octo.
    missing: Mutex<HashSet<String>>,
    /// Files it answers with something that is not its JSON.
    garbled: Mutex<HashSet<String>>,
}

impl Default for Sidecar {
    fn default() -> Self {
        Sidecar {
            version: Mutex::new(Some(2)),
            transition: Mutex::new(None),
            revision: Mutex::new(None),
            parts: Mutex::default(),
            bpms: Mutex::default(),
            blind: AtomicBool::new(false),
            off: AtomicBool::new(false),
            profiles_only: AtomicBool::new(false),
            analysed: Mutex::default(),
            errors: Mutex::default(),
            drops: Mutex::default(),
            missing: Mutex::default(),
            garbled: Mutex::default(),
        }
    }
}

impl Sidecar {
    fn analysed(&self) -> Vec<String> {
        self.analysed.lock().clone()
    }
}

#[async_trait]
impl ISonicClient for Sidecar {
    async fn health(&self) -> Option<SonicHealth> {
        let version = (*self.version.lock())?;
        let transition_version = *self.transition.lock();
        let transition_revision = *self.revision.lock();
        if self.off.load(Ordering::SeqCst) {
            return Some(SonicHealth {
                version,
                transition_version,
                transition_revision,
                problem: None,
                off: true,
                profiles_only: false,
            });
        }
        Some(SonicHealth {
            version,
            transition_version,
            transition_revision,
            problem: self
                .blind
                .load(Ordering::SeqCst)
                .then(|| "cannot see the music folder".to_string()),
            off: false,
            profiles_only: self.profiles_only.load(Ordering::SeqCst),
        })
    }

    async fn analyze(
        &self,
        path: &str,
        parts: SonicParts,
        bpm: Option<f64>,
    ) -> Result<SonicAnswer, SonicDropped> {
        self.bpms.lock().push(bpm);
        self.parts.lock().push(parts);
        let name = std::path::Path::new(path)
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .into_owned();
        self.analysed.lock().push(name.clone());
        if self.drops.lock().contains(&name) {
            return Err(SonicDropped("connection reset".into()));
        }
        let answer = |error: &str, status: u16| SonicAnswer {
            error: Some(error.to_string()),
            status,
            ..Default::default()
        };
        if self.missing.lock().contains(&name) {
            return Ok(answer("no such file", 404));
        }
        if self.garbled.lock().contains(&name) {
            return Ok(answer(
                "octo-sonic answered something Octo could not read (HTTP 200)",
                200,
            ));
        }
        if let Some(error) = self.errors.lock().get(&name) {
            return Ok(answer(error, 422));
        }
        if parts.sound && self.profiles_only.load(Ordering::SeqCst) {
            panic!("asked octo-sonic for the sound of {name} with Sounds alike off in .env");
        }
        let transition = self.transition.lock().unwrap_or(1);
        let revision = *self.revision.lock();
        Ok(SonicAnswer {
            features: parts.sound.then(|| SonicFeatures {
                values: (0..23).map(|i| i as f32 / 100.0).collect(),
                version: self.version.lock().unwrap_or(2),
            }),
            transition: parts.transition.then(|| {
                let mut profile =
                    serde_json::json!({ "version": transition, "durationMs": 200_000, "file": name });
                if let Some(revision) = revision {
                    profile["revision"] = revision.into();
                }
                profile
            }),
            error: None,
            status: 200,
        })
    }
}

struct Library {
    directory: std::path::PathBuf,
    rows: Mutex<Vec<LibrarySongRow>>,
    arrivals: Mutex<HashMap<String, LibrarySongRow>>,
    file_lookups: AtomicUsize,
    /// Times the list was asked for from its start.
    lists: AtomicUsize,
    listable: AtomicBool,
    /// A page that fails, by its start.
    failing_page: Mutex<Option<i32>>,
    /// Songs Navidrome lists as missing: counted in the page, left out of its rows.
    missing_ids: Mutex<HashSet<String>>,
}

impl Library {
    fn row(
        &self,
        id: &str,
        path: std::path::PathBuf,
        size: i64,
        title: &str,
        seconds: Option<i32>,
    ) -> LibrarySongRow {
        LibrarySongRow {
            id: id.into(),
            path: path.to_string_lossy().into_owned(),
            size,
            suffix: "flac".into(),
            bit_rate: 900,
            title: title.into(),
            artist: "Artist".into(),
            duration: seconds,
            album: Some("Album".into()),
            ..Default::default()
        }
    }

    /// A row with no file behind it, for listing tests.
    fn add_listed(&self, id: &str) {
        let path = self.directory.join("none").join(format!("{id}.flac"));
        let row = self.row(id, path, 1000, id, Some(200));
        self.rows.lock().push(row);
    }

    /// A song with a file; listed unless `listed` is false.
    fn add(&self, id: &str, name: &str, seconds: Option<i32>, listed: bool) -> LibrarySongRow {
        let path = self.directory.join(name);
        std::fs::write(&path, vec![0u8; 1000 + id.len()]).expect("writes");
        let size = std::fs::metadata(&path).expect("there").len() as i64;
        let row = self.row(id, path, size, name, seconds);
        if listed {
            self.rows.lock().push(row.clone());
        }
        row
    }
}

#[async_trait]
impl ISonicLibrary for Library {
    /// As Navidrome's native API answers: the count is the page's rows before missing songs were
    /// left out, not the library's.
    async fn list(&self, start: i32, count: i32) -> Option<(Vec<LibrarySongRow>, usize)> {
        if start == 0 {
            self.lists.fetch_add(1, Ordering::SeqCst);
        }
        if !self.listable.load(Ordering::SeqCst) || *self.failing_page.lock() == Some(start) {
            return None;
        }
        let rows = self.rows.lock();
        let page: Vec<LibrarySongRow> = rows
            .iter()
            .skip(start as usize)
            .take(count as usize)
            .cloned()
            .collect();
        let missing = self.missing_ids.lock();
        let len = page.len();
        Some((
            page.into_iter()
                .filter(|row| !missing.contains(&row.id))
                .collect(),
            len,
        ))
    }

    async fn arrived(&self, id: &str) -> Option<LibrarySongRow> {
        self.arrivals.lock().get(id).cloned()
    }

    fn local_file(&self, row: &LibrarySongRow) -> Option<SonicFile> {
        self.file_lookups.fetch_add(1, Ordering::SeqCst);
        SonicFile::stat(&row.path)
    }
}

struct T {
    _dir: tempfile::TempDir,
    worker: Arc<SonicAnalysisWorker>,
    store: Arc<SonicStore>,
    transitions: Arc<TransitionStore>,
    sidecar: Arc<Sidecar>,
    library: Arc<Library>,
    activity: Arc<Activity>,
    now: Arc<Mutex<DateTime<Utc>>>,
}

impl T {
    fn advance(&self, by: TimeDelta) {
        *self.now.lock() += by;
    }

    async fn tick(&self) {
        self.worker.tick().await;
    }

    fn state(&self) -> (String, Option<String>) {
        let status = self.worker.status();
        (status.state, status.reason)
    }

    async fn ticks_until_done(&self) -> usize {
        for tick in 1..=20 {
            self.tick().await;
            if self.worker.status().state == "Done" {
                return tick;
            }
        }
        panic!("the pass never finished");
    }

    /// Ticks, without moving the clock, until `pass` passes have finished.
    async fn ticks_until_pass(&self, pass: i32) {
        for _ in 0..30 {
            self.tick().await;
            if self.store.read(|s| s.pass) >= pass {
                return;
            }
        }
        panic!("pass {pass} never finished");
    }

    /// Octo stopped and started again: a new worker reads back what was written, with the same
    /// octo-sonic, library and clock.
    fn restarted(self) -> T {
        self.store.flush();
        self.transitions.flush();
        let store = Arc::new(SonicStore::new(self._dir.path().join("state.json")));
        let transitions = Arc::new(TransitionStore::new(self._dir.path().join("profiles")));
        let clock_now = self.now.clone();
        let worker = SonicAnalysisWorker::new(
            store.clone(),
            transitions.clone(),
            self.sidecar.clone(),
            self.library.clone(),
            self.activity.clone(),
            self.worker.settings.clone(),
            None,
            Clock::new(move || *clock_now.lock()),
        );
        T {
            worker,
            store,
            transitions,
            ..self
        }
    }

    /// A file from outside the library landing in the music folder.
    fn lands(&self, key: &str, name: &str) {
        let landed = self.library.directory.join(name);
        std::fs::write(&landed, b"some sound").expect("writes");
        self.worker.arrived_outside(key, &landed.to_string_lossy());
    }
}

fn make(settings: Option<RadioSourceSettings>) -> T {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Arc::new(SonicStore::new(dir.path().join("state.json")));
    let transitions = Arc::new(TransitionStore::new(dir.path().join("profiles")));
    let sidecar = Arc::new(Sidecar::default());
    let library = Arc::new(Library {
        directory: dir.path().to_path_buf(),
        rows: Mutex::default(),
        arrivals: Mutex::default(),
        file_lookups: AtomicUsize::new(0),
        lists: AtomicUsize::new(0),
        listable: AtomicBool::new(true),
        failing_page: Mutex::default(),
        missing_ids: Mutex::default(),
    });
    let activity = Arc::new(Activity::default());
    let now = Arc::new(Mutex::new(Utc.with_ymd_and_hms(2026, 10, 4, 12, 0, 0).unwrap()));
    let clock_now = now.clone();
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        radio_sources: settings.unwrap_or(RadioSourceSettings {
            sonic_url: "http://sonic.test".into(),
            sonic_pause_seconds: 0,
            ..Default::default()
        }),
        ..Default::default()
    }));
    let worker = SonicAnalysisWorker::new(
        store.clone(),
        transitions.clone(),
        sidecar.clone(),
        library.clone(),
        activity.clone(),
        settings,
        None,
        Clock::new(move || *clock_now.lock()),
    );
    T {
        _dir: dir,
        worker,
        store,
        transitions,
        sidecar,
        library,
        activity,
        now,
    }
}

fn analyzed_song() -> SonicSong {
    SonicSong {
        stamp: "1:1".into(),
        version: 2,
        f: vec![0.0; 23],
        ..Default::default()
    }
}

#[tokio::test]
async fn it_holds_when_off_paused_down_or_busy() {
    let off = make(Some(RadioSourceSettings {
        sounds_alike: false,
        ..Default::default()
    }));
    off.tick().await;
    assert_eq!(off.worker.status().state, "Off");

    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.worker.set_paused(true);
    t.tick().await;
    assert_eq!(t.worker.status().state, "Paused");

    t.worker.set_paused(false);
    *t.sidecar.version.lock() = None;
    t.tick().await;
    assert_eq!(
        t.state(),
        (
            "Waiting".to_string(),
            Some("octo-sonic is not answering.".to_string())
        )
    );

    *t.sidecar.version.lock() = Some(2);
    t.activity.0.store(true, Ordering::SeqCst);
    t.tick().await;
    assert_eq!(t.state().1.as_deref(), Some("Waiting for a download to finish."));
    assert!(t.sidecar.analysed().is_empty());
}

#[tokio::test]
async fn without_a_navidrome_sign_in_it_says_so() {
    let t = make(None);
    t.library.listable.store(false, Ordering::SeqCst);
    t.tick().await;
    assert!(
        t.state()
            .1
            .unwrap()
            .contains("Octo needs its own Navidrome sign-in")
    );
}

#[tokio::test]
async fn it_analyzes_each_song_once_then_finishes_the_pass() {
    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.library.add("b", "b.flac", Some(200), true);

    t.tick().await;
    t.tick().await;
    assert_eq!(t.sidecar.analysed(), ["a.flac", "b.flac"]);
    assert_eq!(t.store.read(|s| s.songs["a"].f.len()), 23);
    assert_eq!(t.store.read(|s| s.songs["a"].version), 2);

    let lookups = t.library.file_lookups.load(Ordering::SeqCst);
    t.tick().await;
    assert_eq!(t.worker.status().state, "Done");
    assert_eq!(t.store.read(|s| s.pass), 1);
    // Analyzed songs at the same size are passed over without asking for their files.
    assert_eq!(t.library.file_lookups.load(Ordering::SeqCst), lookups);
    assert_eq!(t.sidecar.analysed().len(), 2);
}

#[tokio::test]
async fn a_new_feature_version_is_analyzed_again() {
    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.tick().await;
    t.tick().await;
    t.advance(PASS_INTERVAL + TimeDelta::minutes(1));

    *t.sidecar.version.lock() = Some(3);
    t.tick().await;
    assert_eq!(t.sidecar.analysed(), ["a.flac", "a.flac"]);
    assert_eq!(t.store.read(|s| s.songs["a"].version), 3);
}

#[tokio::test]
async fn an_unreadable_file_is_remembered_and_tried_again_a_week_later() {
    let t = make(None);
    t.library.add("bad", "bad.flac", Some(200), true);
    t.sidecar
        .errors
        .lock()
        .insert("bad.flac".into(), "empty or too short song".into());

    t.tick().await;
    assert_eq!(
        t.store.read(|s| s.failed["bad"].error.clone()),
        "empty or too short song"
    );
    let lookups = t.library.file_lookups.load(Ordering::SeqCst);
    t.tick().await;
    assert_eq!(t.sidecar.analysed().len(), 1);
    assert_eq!(t.library.file_lookups.load(Ordering::SeqCst), lookups);

    t.advance(RETRY_FAILED_AFTER + TimeDelta::hours(1));
    t.tick().await;
    t.tick().await;
    assert_eq!(t.sidecar.analysed().len(), 2);
}

#[tokio::test]
async fn a_file_that_drops_the_connection_twice_is_marked_unreadable_and_the_pass_moves_on() {
    let t = make(None);
    t.library.add("crash", "crash.flac", Some(200), true);
    t.library.add("fine", "fine.flac", Some(200), true);
    t.sidecar.drops.lock().insert("crash.flac".into());

    t.tick().await;
    assert_eq!(
        t.state().1.as_deref(),
        Some("octo-sonic stopped answering; trying again.")
    );
    assert!(t.store.read(|s| s.failed.is_empty()));

    t.tick().await;
    assert_eq!(
        t.store.read(|s| s.failed["crash"].error.clone()),
        "octo-sonic stopped while reading it"
    );

    t.tick().await;
    assert_eq!(t.sidecar.analysed(), ["crash.flac", "crash.flac", "fine.flac"]);
}

#[tokio::test]
async fn a_song_that_just_arrived_goes_first_even_if_the_pass_has_not_listed_it() {
    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.library.add("b", "b.flac", Some(200), true);
    t.tick().await;

    let arrival = t.library.add("new", "new.flac", Some(200), false);
    t.library.arrivals.lock().insert("new".into(), arrival);
    t.worker.arrived_first("new");
    t.tick().await;

    assert_eq!(t.sidecar.analysed(), ["a.flac", "new.flac"]);
}

#[tokio::test]
async fn a_long_mix_is_never_read() {
    let t = make(None);
    t.library.add("mix", "mix.flac", Some(2 * 3600), true);
    t.tick().await;
    assert!(t.sidecar.analysed().is_empty());
    assert_eq!(t.worker.status().state, "Done");
}

#[tokio::test]
async fn at_the_pass_end_gone_songs_are_forgotten_and_start_over_clears_everything() {
    let t = make(None);
    t.store.write(|s| {
        s.songs.insert("gone".into(), analyzed_song());
    });
    t.library.add("a", "a.flac", Some(200), true);
    t.tick().await;
    t.tick().await;
    assert!(!t.store.read(|s| s.songs.contains_key("gone")));
    assert!(t.store.read(|s| s.songs.contains_key("a")));

    t.worker.reset();
    assert!(t.store.read(|s| s.songs.is_empty()));
    assert_eq!(t.store.read(|s| s.pass), 0);
}

#[tokio::test]
async fn a_page_that_fails_forgets_nothing() {
    let t = make(None);
    for i in 0..PAGE_SIZE + 5 {
        t.library.add_listed(&format!("s{i}"));
    }
    t.store.write(|s| {
        s.songs.insert(format!("s{}", PAGE_SIZE + 2), analyzed_song());
    });
    *t.library.failing_page.lock() = Some(PAGE_SIZE);

    t.tick().await;
    assert_eq!(t.worker.status().state, "Waiting");
    assert_eq!(t.store.read(|s| s.songs.len()), 1);
    assert_eq!(t.store.read(|s| s.pass), 0);
}

#[tokio::test]
async fn a_page_with_a_missing_song_is_not_the_end_of_the_list() {
    let t = make(None);
    for i in 0..PAGE_SIZE + 5 {
        t.library.add_listed(&format!("s{i}"));
    }
    t.library.missing_ids.lock().insert("s3".into());
    let later = format!("s{}", PAGE_SIZE + 2);
    t.store.write(|s| {
        s.songs.insert(later.clone(), analyzed_song());
    });

    t.ticks_until_done().await;
    assert!(
        t.store.read(|s| s.songs.contains_key(&later)),
        "a song on the second page is still known"
    );
    // Every listed row has no file: left out, and counted.
    assert_eq!(t.worker.status().skipped, PAGE_SIZE + 4);
}

#[tokio::test]
async fn a_song_that_arrived_during_a_pass_is_kept_at_its_end() {
    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.tick().await;
    let arrival = t.library.add("new", "new.flac", Some(200), false);
    t.library.arrivals.lock().insert("new".into(), arrival);
    t.worker.arrived_first("new");

    t.ticks_until_done().await;
    assert!(t.store.read(|s| s.songs.contains_key("new")));
}

#[tokio::test]
async fn a_song_that_arrives_between_passes_is_read_without_a_new_pass() {
    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.ticks_until_done().await;
    let arrival = t.library.add("new", "new.flac", Some(200), false);
    t.library.arrivals.lock().insert("new".into(), arrival);
    t.worker.arrived_first("new");

    t.tick().await;
    t.tick().await;
    assert_eq!(t.sidecar.analysed(), ["a.flac", "new.flac"]);
    assert_eq!(t.store.read(|s| s.pass), 1);
    assert_eq!(t.worker.status().state, "Done");
}

#[tokio::test]
async fn turned_off_in_env_it_is_off_not_waiting_or_failing() {
    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.sidecar.off.store(true, Ordering::SeqCst);
    t.tick().await;
    assert_eq!(
        t.state(),
        ("Off".to_string(), Some(TURNED_OFF_IN_ENV.to_string()))
    );
    assert!(t.sidecar.analysed().is_empty());
    assert!(t.store.read(|s| s.failed.is_empty()));
}

#[tokio::test]
async fn when_octo_sonic_cannot_see_the_music_it_waits_and_blames_no_song() {
    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.sidecar.blind.store(true, Ordering::SeqCst);
    t.tick().await;
    assert_eq!(t.state().1.as_deref(), Some(CANNOT_SEE_MUSIC));
    assert!(t.sidecar.analysed().is_empty());

    // It sees a folder, but not Octo's: every file is missing to it.
    t.sidecar.blind.store(false, Ordering::SeqCst);
    t.sidecar.missing.lock().insert("a.flac".into());
    t.tick().await;
    assert_eq!(t.state().1.as_deref(), Some(CANNOT_SEE_MUSIC));
    assert!(t.store.read(|s| s.failed.is_empty()));

    t.sidecar.missing.lock().clear();
    t.tick().await;
    assert!(t.store.read(|s| s.songs.contains_key("a")));
}

#[tokio::test]
async fn an_answer_octo_cannot_read_is_that_songs_failure() {
    let t = make(None);
    t.library.add("odd", "odd.flac", Some(200), true);
    t.library.add("fine", "fine.flac", Some(200), true);
    t.sidecar.garbled.lock().insert("odd.flac".into());
    t.ticks_until_done().await;
    assert!(t.store.read(|s| s.failed["odd"].error.contains("could not read")));
    assert!(t.store.read(|s| s.songs.contains_key("fine")));
}

#[tokio::test]
async fn a_song_with_no_length_is_left_out_and_counted() {
    let t = make(None);
    t.library.add("zero", "zero.flac", Some(0), true);
    t.library.add("none", "none.flac", None, true);
    t.library.add("fine", "fine.flac", Some(200), true);
    t.ticks_until_done().await;
    assert_eq!(t.sidecar.analysed(), ["fine.flac"]);
}

#[tokio::test]
async fn pause_and_start_over_are_saved_at_once() {
    let t = make(None);
    t.worker.set_paused(true);
    let path = t.store.path().to_path_buf();
    assert!(
        std::fs::read_to_string(&path)
            .expect("written")
            .contains("\"Paused\":true")
    );
    t.worker.reset();
    assert!(
        std::fs::read_to_string(&path)
            .expect("written")
            .contains("\"Pass\":0")
    );
}

/// Before its first look the analysis says it is about to start, and the status names what the
/// pass is reading.
#[tokio::test]
async fn before_its_first_look_it_says_it_is_about_to_start() {
    let t = make(None);
    assert_eq!(
        t.state(),
        (
            "Waiting".to_string(),
            Some("Starts a couple of minutes after Octo does.".to_string())
        )
    );
    t.library.add("a", "a.flac", Some(200), true);
    t.library.add("b", "b.flac", Some(200), true);
    t.tick().await;
    let status = t.worker.status();
    assert_eq!(
        (status.state.as_str(), status.analysed, status.total),
        ("Running", 1, 2)
    );
    assert_eq!(
        serde_json::to_value(&status).unwrap()["analysed"],
        serde_json::json!(1)
    );
}

// ---- transition profiles ----------------------------------------------------------------

fn sound_and_profile() -> SonicParts {
    SonicParts {
        sound: true,
        transition: true,
    }
}

fn profile_only() -> SonicParts {
    SonicParts {
        sound: false,
        transition: true,
    }
}

fn profile_of(t: &T, key: &str) -> Option<serde_json::Value> {
    match t.transitions.lookup(key) {
        TransitionLookup::Ready(profile) => Some(profile),
        TransitionLookup::NotReady => None,
    }
}

#[tokio::test]
async fn one_reading_keeps_a_songs_sound_and_its_profile() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    t.library.add("a", "a.flac", Some(200), true);
    t.tick().await;
    assert_eq!(*t.sidecar.parts.lock(), [sound_and_profile()]);
    assert_eq!(t.store.read(|s| s.songs["a"].f.len()), 23);
    assert_eq!(
        profile_of(&t, "a").map(|p| p["file"].clone()),
        Some("a.flac".into())
    );
    assert_eq!(t.transitions.current_version(), Some(1));

    let lookups = t.library.file_lookups.load(Ordering::SeqCst);
    t.tick().await;
    assert_eq!(t.worker.status().state, "Done");
    // Both kept at this size: passed over without asking for the file.
    assert_eq!(t.library.file_lookups.load(Ordering::SeqCst), lookups);
    assert_eq!(t.sidecar.analysed().len(), 1);
}

#[tokio::test]
async fn an_octo_sonic_from_before_profiles_is_asked_for_the_sound_alone() {
    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.tick().await;
    assert_eq!(*t.sidecar.parts.lock(), [SonicParts::SOUND]);
    assert_eq!(profile_of(&t, "a"), None);
    assert_eq!(t.transitions.current_version(), None);
}

#[tokio::test]
async fn a_song_read_before_profiles_is_read_again_for_its_profile_alone() {
    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.tick().await;
    t.tick().await;
    assert_eq!(t.worker.status().state, "Done");
    t.advance(PASS_INTERVAL + TimeDelta::minutes(1));

    // octo-sonic is updated and now makes profiles.
    *t.sidecar.transition.lock() = Some(1);
    t.tick().await;
    assert_eq!(*t.sidecar.parts.lock(), [SonicParts::SOUND, profile_only()]);
    assert!(profile_of(&t, "a").is_some());
    // The features it had are kept as they were.
    assert_eq!(t.store.read(|s| s.songs["a"].f.len()), 23);
    assert!(t.store.read(|s| s.failed.is_empty()));
}

#[tokio::test]
async fn a_new_profile_version_is_read_again_and_the_old_one_is_not_served() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    t.library.add("a", "a.flac", Some(200), true);
    t.tick().await;
    t.tick().await;
    t.advance(PASS_INTERVAL + TimeDelta::minutes(1));

    *t.sidecar.transition.lock() = Some(2);
    t.library.listable.store(false, Ordering::SeqCst);
    t.tick().await;
    // Until it is read again, the old profile is not handed out.
    assert_eq!(profile_of(&t, "a"), None);
    t.library.listable.store(true, Ordering::SeqCst);
    t.tick().await;
    assert_eq!(t.sidecar.parts.lock().last(), Some(&profile_only()));
    assert_eq!(profile_of(&t, "a").map(|p| p["version"].clone()), Some(2.into()));
}

#[tokio::test]
async fn an_outside_song_that_lands_gets_its_profile_under_its_key_first() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    t.library.add("a", "a.flac", Some(200), true);
    let landed = t.library.directory.join("landed.mp3");
    std::fs::write(&landed, b"some sound").expect("writes");
    let landed = landed.to_string_lossy().into_owned();
    let arrivals = DownloadArrivals::default();
    t.worker.listen_to(&arrivals);
    arrivals.arrived("youtube:abc", &landed);

    t.tick().await;
    assert_eq!(t.sidecar.analysed(), ["landed.mp3"]);
    assert_eq!(*t.sidecar.parts.lock(), [profile_only()]);
    assert!(profile_of(&t, "youtube:abc").is_some());
    assert_eq!(
        t.transitions.entry("youtube:abc").and_then(|e| e.path),
        Some(landed.clone())
    );
    // Nothing of it goes into the library's sounds.
    assert!(t.store.read(|s| !s.songs.contains_key("youtube:abc")));

    // The same file landing again is not read again.
    arrivals.arrived("youtube:abc", &landed);
    t.tick().await;
    assert_eq!(t.sidecar.analysed(), ["landed.mp3", "a.flac"]);
}

#[tokio::test]
async fn an_outside_song_between_passes_is_read_without_a_new_pass() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    t.library.add("a", "a.flac", Some(200), true);
    t.ticks_until_done().await;
    let landed = t.library.directory.join("landed.mp3");
    std::fs::write(&landed, b"some sound").expect("writes");
    t.worker
        .arrived_outside("soulseek:xyz", &landed.to_string_lossy());
    t.tick().await;
    assert!(profile_of(&t, "soulseek:xyz").is_some());
    assert_eq!(t.store.read(|s| s.pass), 1);
}

#[tokio::test]
async fn an_outside_song_octo_sonic_cannot_see_blames_nothing_and_moves_on() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    let landed = t.library.directory.join("cached.mp3");
    std::fs::write(&landed, b"some sound").expect("writes");
    t.sidecar.missing.lock().insert("cached.mp3".into());
    t.library.add("a", "a.flac", Some(200), true);
    t.worker.arrived_outside("youtube:abc", &landed.to_string_lossy());
    t.tick().await;
    assert_eq!(t.worker.status().state, "Running");
    t.tick().await;
    assert_eq!(t.sidecar.analysed(), ["cached.mp3", "a.flac"]);
    assert!(t.store.read(|s| s.failed.is_empty()));
}

#[tokio::test]
async fn an_outside_song_waits_while_octo_sonic_makes_no_profiles() {
    let t = make(None);
    let landed = t.library.directory.join("landed.mp3");
    std::fs::write(&landed, b"some sound").expect("writes");
    t.worker.arrived_outside("youtube:abc", &landed.to_string_lossy());
    t.ticks_until_done().await;
    assert!(t.sidecar.analysed().is_empty());
}

#[tokio::test]
async fn at_the_pass_end_gone_songs_lose_their_profiles() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    t.transitions
        .put("gone", "1:1", None, &serde_json::json!({ "version": 1 }));
    t.library.add("a", "a.flac", Some(200), true);
    t.ticks_until_done().await;
    assert!(t.transitions.entry("gone").is_none());
    assert!(profile_of(&t, "a").is_some());
}

// ---- profile revisions -------------------------------------------------------------------

fn read_count(t: &T, name: &str) -> usize {
    t.sidecar.analysed().iter().filter(|read| *read == name).count()
}

#[tokio::test]
async fn a_profile_of_a_lower_revision_is_read_again_once_and_served_meanwhile() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    t.library.add("a", "a.flac", Some(200), true);
    t.library.add("b", "b.flac", Some(200), true);
    t.ticks_until_pass(1).await;
    assert_eq!(t.transitions.entry("a").map(|e| e.revision), Some(1));

    // octo-sonic now makes revision 2 of the same version.
    *t.sidecar.revision.lock() = Some(2);
    t.tick().await;
    // Without waiting out the pass interval, for the profile alone.
    assert_eq!(t.sidecar.analysed(), ["a.flac", "b.flac", "a.flac"]);
    assert_eq!(t.sidecar.parts.lock().last(), Some(&profile_only()));
    assert_eq!(t.transitions.entry("a").map(|e| e.revision), Some(2));
    // The song not read yet keeps its profile served, and Sounds alike stays done, without
    // saying every song is analyzed.
    assert_eq!(
        profile_of(&t, "b").map(|p| (p["file"].clone(), p.get("revision").cloned())),
        Some(("b.flac".into(), None))
    );
    let status = t.worker.status();
    assert_eq!(
        (status.state.as_str(), status.reason.as_deref()),
        ("Done", Some(UPDATING))
    );
    assert_eq!(
        serde_json::to_value(&status).unwrap()["updatingProfiles"],
        serde_json::json!({ "done": 1, "total": 2 })
    );

    t.ticks_until_pass(2).await;
    assert_eq!(t.sidecar.analysed(), ["a.flac", "b.flac", "a.flac", "b.flac"]);
    assert_eq!(t.transitions.revision(), 2);
    assert_eq!(t.worker.status().updating_profiles, None);
    // Once: the passes after leave them be.
    t.advance(PASS_INTERVAL + TimeDelta::minutes(1));
    t.ticks_until_pass(3).await;
    assert_eq!(t.sidecar.analysed().len(), 4);
    assert!(t.store.read(|s| s.failed.is_empty()));
}

#[tokio::test]
async fn a_revision_rise_starts_a_pass_at_every_start_until_one_completes() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    for id in ["a", "b", "c"] {
        t.library.add(id, &format!("{id}.flac"), Some(200), true);
    }
    t.ticks_until_pass(1).await;
    let next = t
        .store
        .read(|s| s.next_pass_utc)
        .expect("the next pass is hours away");

    // The update replaces Octo and octo-sonic together.
    *t.sidecar.revision.lock() = Some(2);
    let t = t.restarted();
    assert_eq!(t.transitions.revision(), 1);
    t.tick().await;
    // The first look starts the pass now, and keeps the revision for the pass's end.
    assert_eq!(t.store.read(|s| s.next_pass_utc), None);
    assert_eq!(t.sidecar.analysed()[3..], ["a.flac"]);
    assert_eq!(t.transitions.revision(), 1);

    // Octo stops in the middle of the pass, before the cleared wait reached the disk.
    t.store.write(|s| s.next_pass_utc = Some(next));
    let t = t.restarted();
    t.tick().await;
    assert_eq!(t.store.read(|s| s.next_pass_utc), None);
    // The pass goes on with the songs still at the lower revision.
    assert_eq!(t.sidecar.analysed()[3..], ["a.flac", "b.flac"]);
    t.ticks_until_pass(2).await;
    assert_eq!(t.sidecar.analysed()[3..], ["a.flac", "b.flac", "c.flac"]);
    assert_eq!(t.transitions.revision(), 2);

    // Started again with 2 kept and octo-sonic still at 2: the wait stands.
    let t = t.restarted();
    t.tick().await;
    let now = *t.now.lock();
    assert!(t.store.read(|s| s.next_pass_utc).is_some_and(|next| next > now));
    assert_eq!(t.sidecar.analysed().len(), 6);
    assert_eq!(t.worker.status().state, "Done");
}

#[tokio::test]
async fn a_new_index_keeps_the_revision_its_first_pass_began_with() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    *t.sidecar.revision.lock() = Some(2);
    t.library.add("a", "a.flac", Some(200), true);
    t.library.add("b", "b.flac", Some(200), true);
    assert_eq!(t.transitions.revision(), 1);
    t.tick().await;
    // Songs read for the first time: nothing is being updated.
    let status = t.worker.status();
    assert_eq!(
        (status.state.as_str(), status.updating_profiles),
        ("Running", None)
    );
    t.ticks_until_pass(1).await;
    assert_eq!(t.transitions.revision(), 2);
    assert_eq!(t.transitions.entry("a").map(|e| e.revision), Some(2));
    assert_eq!(t.sidecar.analysed().len(), 2);
}

#[tokio::test]
async fn a_rise_in_the_middle_of_a_pass_is_kept_by_the_next_which_starts_at_once() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    for id in ["a", "b", "c"] {
        t.library.add(id, &format!("{id}.flac"), Some(200), true);
    }
    t.ticks_until_pass(1).await;
    t.advance(PASS_INTERVAL + TimeDelta::minutes(1));
    t.library.add("d", "d.flac", Some(200), true);
    t.tick().await;
    assert_eq!(t.sidecar.analysed().last().map(String::as_str), Some("d.flac"));

    *t.sidecar.revision.lock() = Some(2);
    t.ticks_until_pass(2).await;
    // That pass began without a revision, so it keeps none.
    assert_eq!(t.transitions.revision(), 1);
    // The clock stands still: the next pass starts at once, and keeps 2 at its end.
    t.ticks_until_pass(3).await;
    assert_eq!(t.transitions.revision(), 2);
    for id in ["a", "b", "c", "d"] {
        assert_eq!(t.transitions.entry(id).map(|e| e.revision), Some(2), "{id}");
        assert_eq!(read_count(&t, &format!("{id}.flac")), 2, "{id}");
    }
}

#[tokio::test]
async fn outside_songs_of_a_lower_revision_are_read_again_once_a_pass() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    t.library.add("a", "a.flac", Some(200), true);
    t.lands("youtube:abc", "landed.mp3");
    t.ticks_until_pass(1).await;
    assert_eq!(t.sidecar.analysed(), ["landed.mp3", "a.flac"]);

    *t.sidecar.revision.lock() = Some(2);
    t.ticks_until_pass(2).await;
    // Listed at the pass's start and read for its profile alone, as the library's song is.
    assert_eq!(t.sidecar.analysed()[2..], ["landed.mp3", "a.flac"]);
    assert_eq!(
        *t.sidecar.parts.lock(),
        [
            profile_only(),
            sound_and_profile(),
            profile_only(),
            profile_only()
        ]
    );
    assert_eq!(t.transitions.entry("youtube:abc").map(|e| e.revision), Some(2));
    t.advance(PASS_INTERVAL + TimeDelta::minutes(1));
    t.ticks_until_pass(3).await;
    assert_eq!(t.sidecar.analysed().len(), 4);
}

#[tokio::test]
async fn a_failed_outside_reading_waits_a_week_and_outlasts_the_pass() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    t.library.add("a", "a.flac", Some(200), true);
    t.lands("youtube:abc", "landed.mp3");
    t.ticks_until_pass(1).await;

    *t.sidecar.revision.lock() = Some(2);
    t.sidecar
        .errors
        .lock()
        .insert("landed.mp3".into(), "the file is damaged".into());
    t.ticks_until_pass(2).await;
    // Tried once in the pass. The failure is kept on its entry, not with the library's songs.
    assert_eq!(t.sidecar.analysed()[2..], ["landed.mp3", "a.flac"]);
    let entry = t.transitions.entry("youtube:abc").expect("kept");
    let failed = entry.failed.clone().expect("the failure is kept");
    assert_eq!(
        (failed.stamp.as_str(), failed.revision),
        (entry.stamp.as_str(), 2)
    );
    assert_eq!((entry.revision, failed.at_utc), (1, *t.now.lock()));
    assert!(t.store.read(|s| s.failed.is_empty()));
    assert!(profile_of(&t, "youtube:abc").is_some());

    // The next pass lists it again and leaves it, until a week has gone by.
    t.advance(PASS_INTERVAL + TimeDelta::minutes(1));
    t.ticks_until_pass(3).await;
    assert_eq!(t.sidecar.analysed().len(), 4);
    assert!(
        t.transitions
            .entry("youtube:abc")
            .and_then(|e| e.failed)
            .is_some()
    );

    t.sidecar.errors.lock().clear();
    t.advance(RETRY_FAILED_AFTER);
    t.ticks_until_pass(4).await;
    assert_eq!(t.sidecar.analysed()[4..], ["landed.mp3"]);
    let entry = t.transitions.entry("youtube:abc").expect("kept");
    assert_eq!((entry.revision, entry.failed), (2, None));
}

/// Guard: fails when a missing `transitionRevision` stands for any revision, such as the one
/// the store last kept (the `revision` of `tick`), or when a pass that began without one changes
/// the stored revision.
#[tokio::test]
async fn an_octo_sonic_that_says_no_revision_has_nothing_read_again() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    *t.sidecar.revision.lock() = Some(2);
    t.library.add("a", "a.flac", Some(200), true);
    t.ticks_until_pass(1).await;
    assert_eq!(t.transitions.revision(), 2);

    // An octo-sonic from before revisions takes its place: its profiles say none.
    *t.sidecar.revision.lock() = None;
    t.library.add("b", "b.flac", Some(200), true);
    t.advance(PASS_INTERVAL + TimeDelta::minutes(1));
    t.ticks_until_pass(2).await;
    t.lands("youtube:abc", "landed.mp3");
    t.tick().await;
    assert_eq!(t.transitions.entry("b").map(|e| e.revision), Some(1));
    assert_eq!(t.transitions.entry("youtube:abc").map(|e| e.revision), Some(1));

    t.tick().await;
    let now = *t.now.lock();
    assert!(t.store.read(|s| s.next_pass_utc).is_some_and(|next| next > now));
    t.advance(PASS_INTERVAL + TimeDelta::minutes(1));
    t.ticks_until_pass(3).await;
    assert_eq!(t.sidecar.analysed(), ["a.flac", "b.flac", "landed.mp3"]);
    assert_eq!(t.transitions.revision(), 2);
    assert_eq!(t.worker.status().updating_profiles, None);
}

// ---- new songs while profiles are read again ---------------------------------------------

/// A library of `ids`, each read once with a profile of revision 1; octo-sonic now says 2.
async fn reread_pass(ids: &[&str]) -> T {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    for id in ids {
        t.library.add(id, &format!("{id}.flac"), Some(200), true);
    }
    t.ticks_until_pass(1).await;
    *t.sidecar.revision.lock() = Some(2);
    t
}

/// A song's file grows, and Navidrome lists it at its new size.
fn grows(t: &T, id: &str) {
    let path = t.library.directory.join(format!("{id}.flac"));
    std::fs::write(&path, vec![0u8; 5000]).expect("writes");
    for row in t.library.rows.lock().iter_mut().filter(|row| row.id == id) {
        row.size = 5000;
    }
}

/// Guard: fails when a pass that reads songs again walks its list in order, so a new song or a
/// changed file waits behind the songs read again before it.
#[tokio::test]
async fn new_and_changed_songs_in_the_list_go_before_any_song_read_again() {
    let t = reread_pass(&["a", "b", "c"]).await;
    grows(&t, "b");
    t.library.add("d", "d.flac", Some(200), true);
    t.ticks_until_pass(2).await;
    assert_eq!(
        t.sidecar.analysed()[3..],
        ["b.flac", "d.flac", "a.flac", "c.flac"]
    );
    assert_eq!(
        t.sidecar.parts.lock()[3..],
        [
            sound_and_profile(),
            sound_and_profile(),
            profile_only(),
            profile_only()
        ]
    );
    assert_eq!(t.transitions.revision(), 2);
}

/// Guard: fails when a pass that reads songs again lists the library only once: a song added
/// through Navidrome alone (by Lidarr, or a copied file) and a file changed meanwhile wait for
/// every song read again, days on a large library, while the dashboard says it is up to date.
#[tokio::test]
async fn a_song_added_during_a_pass_of_songs_read_again_is_read_before_the_rest() {
    let t = reread_pass(&["a", "b", "c", "d"]).await;
    t.tick().await;
    assert_eq!(t.sidecar.analysed()[4..], ["a.flac"]);
    assert_eq!(t.state(), ("Done".to_string(), Some(UPDATING.to_string())));
    // No download says so: the song is added in Navidrome, and d's file changes.
    t.library.add("new", "new.flac", Some(200), true);
    grows(&t, "d");
    t.advance(PASS_INTERVAL);
    t.tick().await;
    assert_eq!(t.sidecar.analysed()[4..], ["a.flac", "new.flac"]);
    assert_eq!(t.sidecar.parts.lock().last(), Some(&sound_and_profile()));
    assert_eq!(t.worker.status().total, 5);
    t.ticks_until_pass(2).await;
    assert_eq!(
        t.sidecar.analysed()[4..],
        ["a.flac", "new.flac", "d.flac", "b.flac", "c.flac"]
    );
    assert!(t.store.read(|s| s.songs.contains_key("new")));
    assert!(profile_of(&t, "new").is_some());
    assert_eq!(t.transitions.revision(), 2);
}

/// The second look at the library comes once the pass has run for PASS_INTERVAL, to the
/// second, and again each PASS_INTERVAL after; one that cannot read the list tries again after
/// RELIST_RETRY while the pass goes on.
#[tokio::test]
async fn a_long_pass_lists_the_library_again_each_pass_interval() {
    let t = reread_pass(&["a", "b", "c", "d", "e"]).await;
    let lists = || t.library.lists.load(Ordering::SeqCst);
    let read = || t.sidecar.analysed()[5..].to_vec();
    t.tick().await;
    assert_eq!(lists(), 2);
    t.library.add("new", "new.flac", Some(200), true);
    t.advance(PASS_INTERVAL - TimeDelta::seconds(1));
    t.tick().await;
    assert_eq!(lists(), 2);
    assert_eq!(read(), ["a.flac", "b.flac"]);
    t.advance(TimeDelta::seconds(1));
    t.tick().await;
    assert_eq!(lists(), 3);
    assert_eq!(read(), ["a.flac", "b.flac", "new.flac"]);

    // Navidrome's list cannot be read at the next look: the pass goes on.
    t.library.listable.store(false, Ordering::SeqCst);
    t.library.add("newer", "newer.flac", Some(200), true);
    t.advance(PASS_INTERVAL);
    t.tick().await;
    assert_eq!(lists(), 4);
    assert_eq!(read(), ["a.flac", "b.flac", "new.flac", "c.flac"]);
    assert_eq!(t.worker.status().state, "Running");
    t.library.listable.store(true, Ordering::SeqCst);
    t.advance(RELIST_RETRY - TimeDelta::seconds(1));
    t.tick().await;
    assert_eq!(lists(), 4);
    t.advance(TimeDelta::seconds(1));
    t.tick().await;
    assert_eq!(lists(), 5);
    t.ticks_until_pass(2).await;
    assert_eq!(
        read(),
        [
            "a.flac",
            "b.flac",
            "new.flac",
            "c.flac",
            "d.flac",
            "newer.flac",
            "e.flac"
        ]
    );
}

/// Guard: fails when the update's count takes in profiles the pass will never read again: a
/// song gone from the library but not yet forgotten, one from outside whose file is gone, and
/// one whose reading failed. Its N would never reach its M.
#[tokio::test]
async fn the_updates_count_is_of_the_songs_the_pass_can_still_read() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    for id in ["a", "b", "c", "d"] {
        t.library.add(id, &format!("{id}.flac"), Some(200), true);
    }
    t.lands("youtube:x", "landed.mp3");
    t.ticks_until_pass(1).await;
    // c leaves the library, the outside song's file is deleted, and b cannot be read again.
    t.library.rows.lock().retain(|row| row.id != "c");
    std::fs::remove_file(t.library.directory.join("landed.mp3")).expect("removes");
    t.sidecar
        .errors
        .lock()
        .insert("b.flac".into(), "the file is damaged".into());
    *t.sidecar.revision.lock() = Some(2);
    let updating = |t: &T| t.worker.status().updating_profiles.map(|u| (u.done, u.total));
    t.tick().await;
    assert_eq!(t.sidecar.analysed()[5..], ["a.flac"]);
    assert_eq!(updating(&t), Some((1, 3)));
    t.tick().await;
    assert_eq!(updating(&t), Some((1, 2)));
    t.tick().await;
    assert_eq!(t.sidecar.analysed()[5..], ["a.flac", "b.flac", "d.flac"]);
    // Every song it can read again is read, though the pass has not ended.
    assert_eq!(t.store.read(|s| s.pass), 1);
    assert_eq!(updating(&t), None);
}

/// Guard: fails when songs read again wait for the minute's save of the index: a crash in the
/// update loses them, they are read once more, and the dashboard's count goes back.
#[tokio::test]
async fn a_crash_while_profiles_are_read_again_loses_at_most_a_few() {
    let ids = ["a", "b", "c", "d", "e", "f", "g"];
    let t = reread_pass(&ids).await;
    for _ in 0..6 {
        t.tick().await;
    }
    assert_eq!(t.sidecar.analysed().len(), 13);
    // Octo stops without saving: a new start knows only what is on disk.
    let on_disk = TransitionStore::new(t._dir.path().join("profiles"));
    let kept = ids
        .iter()
        .filter(|id| on_disk.entry(id).is_some_and(|e| e.revision == 2))
        .count();
    assert_eq!(kept, 5);
}

/// Guard: fails when a song read again only for its profile counts among the songs Sounds
/// alike could not read.
#[tokio::test]
async fn a_failed_reading_again_is_kept_on_the_songs_profile_not_with_sounds_alike() {
    let t = reread_pass(&["a", "b"]).await;
    t.sidecar
        .errors
        .lock()
        .insert("a.flac".into(), "the file is damaged".into());
    t.ticks_until_pass(2).await;
    assert_eq!(t.sidecar.analysed()[2..], ["a.flac", "b.flac"]);
    // Sounds alike read a's sound, and still has it.
    let status = t.worker.status();
    assert_eq!((status.failed, status.analysed), (0, 2));
    assert!(t.store.read(|s| s.failed.is_empty()));
    let entry = t.transitions.entry("a").expect("kept");
    let failed = entry.failed.clone().expect("the failure is on its profile");
    assert_eq!(
        (entry.revision, failed.revision, failed.stamp == entry.stamp),
        (1, 2, true)
    );
    assert!(profile_of(&t, "a").is_some());
    // Not read again before a week has gone by, and then once.
    t.advance(PASS_INTERVAL + TimeDelta::minutes(1));
    t.ticks_until_pass(3).await;
    assert_eq!(t.sidecar.analysed().len(), 4);
    t.sidecar.errors.lock().clear();
    t.advance(RETRY_FAILED_AFTER);
    t.ticks_until_pass(4).await;
    assert_eq!(t.sidecar.analysed()[4..], ["a.flac"]);
    let entry = t.transitions.entry("a").expect("kept");
    assert_eq!((entry.revision, entry.failed), (2, None));
}

/// Guard: fails when the Radio page stops saying how far the profiles are updated, whether
/// `loadSonic` stops handing the status to `sonicStatusText` or the field that reads stops
/// matching the one the status serializes. The status line is run from `admin.js` itself with
/// Node.js where it is installed; the call and the field are checked either way.
#[tokio::test]
async fn the_dashboard_says_how_far_the_profiles_are_updated() {
    let t = reread_pass(&["a", "b"]).await;
    t.tick().await;
    let status = serde_json::to_value(t.worker.status()).expect("serialized");

    let js = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../octo/wwwroot/admin/admin.js"
    ))
    .expect("the dashboard script");
    let start = js.find("const sonicStates = ").expect("the analysis's states");
    let end = start
        + js[start..]
            .find("async function sonicPost(")
            .expect("sonicPost after loadSonic");
    let script = &js[start..end];
    let (text, load) = script.split_at(script.find("async function loadSonic() {").expect("loadSonic"));
    let mut wrong = Vec::new();
    // The field the status line reads is the one the status has.
    let field: String = text
        .split("const updating = s.")
        .nth(1)
        .unwrap_or_default()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if !status
        .get(&field)
        .is_some_and(|updating| updating["done"].is_number() && updating["total"].is_number())
    {
        wrong.push(format!(
            "sonicStatusText reads s.{field}, which the status lacks: {status}"
        ));
    }
    // loadSonic hands what the status endpoint answers to the status line.
    for call in [
        "await api('/api/admin/sonic')",
        "const s = await response.json();",
        "status.textContent = sonicStatusText(s);",
    ] {
        if !load.contains(call) {
            wrong.push(format!("loadSonic lacks {call}"));
        }
    }
    match status_line(script, &status) {
        Some(line) => {
            let says = "Up to date. 2 of 2 songs analyzed (pass 1). 2 have a transition profile. \
                        Updating song profiles, 1 of 2. New or changed songs are read first.";
            if line != says {
                wrong.push(format!("the page says {line:?}"));
            }
        }
        None => eprintln!("node is not installed; the status line's run is skipped"),
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// The Radio page's status line as `admin.js` writes it: `script`, its analysis section, run by
/// Node.js with `status` as `/api/admin/sonic` answers. None where Node.js is not installed.
fn status_line(script: &str, status: &serde_json::Value) -> Option<String> {
    let node = std::process::Command::new("node").arg("--version").output();
    if !node.is_ok_and(|out| out.status.success()) {
        return None;
    }
    let dir = tempfile::tempdir().expect("a temp dir");
    let write = |name: &str, text: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, text).expect("written");
        path
    };
    let harness = write("harness.js", STATUS_LINE_HARNESS);
    let section = write("sonic.js", script);
    let answer = write("status.json", &status.to_string());
    let out = std::process::Command::new("node")
        .arg(&harness)
        .arg(&section)
        .arg(&answer)
        .output()
        .expect("node ran");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    Some(String::from_utf8(out.stdout).expect("text"))
}

/// A stand-in page for the analysis section of `admin.js`: `api` answers the status, and the
/// status line's text is printed once `loadSonic` has run.
const STATUS_LINE_HARNESS: &str = r#"
'use strict';
const fs = require('fs');
const vm = require('vm');
const [section, answer] = process.argv.slice(2);
const elements = {};
const element = () => ({ textContent: '', dataset: {}, querySelector: () => element() });
const context = vm.createContext({
  api: async (path) => ({
    ok: path === '/api/admin/sonic',
    status: path === '/api/admin/sonic' ? 200 : 404,
    json: async () => JSON.parse(fs.readFileSync(answer, 'utf8')),
  }),
  document: { getElementById: (id) => (elements[id] = elements[id] || element()) },
});
vm.runInContext(fs.readFileSync(section, 'utf8') + '\nloadSonic();', context, { filename: section })
  .then(() => process.stdout.write(elements['sonic-status'].textContent));
"#;

// ---- profiles with Sounds alike off -----------------------------------------------------

fn sounds_alike_off() -> RadioSourceSettings {
    RadioSourceSettings {
        sounds_alike: false,
        sonic_url: "http://sonic.test".into(),
        sonic_pause_seconds: 0,
        ..Default::default()
    }
}

#[tokio::test]
async fn with_sounds_alike_off_on_the_radio_page_every_song_still_gets_its_profile() {
    let t = make(Some(sounds_alike_off()));
    *t.sidecar.transition.lock() = Some(1);
    t.library.add("a", "a.flac", Some(200), true);
    t.library.add("b", "b.flac", Some(200), true);
    t.tick().await;
    assert_eq!(
        t.state(),
        ("Running".to_string(), Some(PROFILES_ONLY_RADIO_PAGE.to_string()))
    );
    t.ticks_until_done().await;
    assert_eq!(t.sidecar.analysed(), ["a.flac", "b.flac"]);
    assert!(t.sidecar.parts.lock().iter().all(|&p| p == profile_only()));
    assert!(profile_of(&t, "a").is_some() && profile_of(&t, "b").is_some());
    // No bliss work: not a song's sound is kept, and none is counted as failed.
    assert!(t.store.read(|s| s.songs.is_empty() && s.failed.is_empty()));
    let status = t.worker.status();
    assert_eq!(
        (status.profiled, status.analysed, status.reads_sound),
        (2, 0, false)
    );
    assert_eq!(status.reason.as_deref(), Some(DONE_PROFILES));
    assert_eq!(t.transitions.current_version(), Some(1));
}

#[tokio::test]
async fn with_sounds_alike_off_in_env_only_profiles_are_asked_for() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    t.sidecar.profiles_only.store(true, Ordering::SeqCst);
    t.library.add("a", "a.flac", Some(200), true);
    t.tick().await;
    assert_eq!(
        t.state(),
        ("Running".to_string(), Some(PROFILES_ONLY_IN_ENV.to_string()))
    );
    t.ticks_until_done().await;
    assert_eq!(*t.sidecar.parts.lock(), [profile_only()]);
    assert!(profile_of(&t, "a").is_some());
    assert!(t.store.read(|s| s.songs.is_empty() && s.failed.is_empty()));
}

#[tokio::test]
async fn with_sounds_alike_off_an_outside_song_that_lands_still_gets_its_profile() {
    let t = make(Some(sounds_alike_off()));
    *t.sidecar.transition.lock() = Some(1);
    let landed = t.library.directory.join("landed.mp3");
    std::fs::write(&landed, b"some sound").expect("writes");
    t.worker.arrived_outside("youtube:abc", &landed.to_string_lossy());
    t.ticks_until_done().await;
    assert_eq!(t.sidecar.analysed(), ["landed.mp3"]);
    assert!(profile_of(&t, "youtube:abc").is_some());
}

#[tokio::test]
async fn turning_sounds_alike_on_reads_the_sound_of_songs_that_have_only_a_profile() {
    let t = make(Some(sounds_alike_off()));
    *t.sidecar.transition.lock() = Some(1);
    t.library.add("a", "a.flac", Some(200), true);
    t.ticks_until_done().await;
    assert!(t.store.read(|s| s.songs.is_empty()));

    let settings = t.worker.settings.clone();
    let mut on = (*settings.current()).clone();
    on.radio_sources.sounds_alike = true;
    settings.set(on);
    // Without waiting out the pass interval.
    t.ticks_until_done().await;
    assert_eq!(t.sidecar.parts.lock().last(), Some(&SonicParts::SOUND));
    assert!(t.store.read(|s| s.songs.contains_key("a")));
}

// ---- published tempo ---------------------------------------------------------------------

/// A catalog's tempos by ISRC, counting what it is asked.
#[derive(Default)]
struct Catalog {
    tempos: Mutex<HashMap<String, f64>>,
    asked: Mutex<Vec<Vec<String>>>,
}

#[async_trait]
impl IPublishedTempo for Catalog {
    async fn bpm(&self, isrcs: &[String]) -> Option<f64> {
        self.asked.lock().push(isrcs.to_vec());
        let tempos = self.tempos.lock();
        isrcs
            .iter()
            .filter_map(|isrc| octo_core::common::SongIdentity::normalize_isrc(isrc))
            .find_map(|isrc| tempos.get(&isrc).copied())
    }
}

fn with_catalog(t: &T) -> Arc<Catalog> {
    let catalog = Arc::new(Catalog::default());
    catalog.tempos.lock().insert("GBAAA9800001".into(), 77.1);
    t.worker.use_published_tempo(catalog.clone());
    catalog
}

fn listed_with(t: &T, id: &str, bpm: Option<i32>, isrcs: &[&str]) {
    let mut row = t.library.add(id, &format!("{id}.flac"), Some(200), false);
    row.bpm = bpm;
    row.isrcs = isrcs.iter().map(|isrc| isrc.to_string()).collect();
    t.library.rows.lock().push(row);
}

#[tokio::test]
async fn a_profile_is_handed_the_files_bpm_tag_without_asking_the_catalog() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    let catalog = with_catalog(&t);
    listed_with(&t, "a", Some(128), &["GBAAA9800001"]);
    t.tick().await;
    assert_eq!(*t.sidecar.bpms.lock(), [Some(128.0)]);
    assert!(catalog.asked.lock().is_empty());
}

#[tokio::test]
async fn without_a_bpm_tag_the_catalogs_tempo_for_the_isrc_goes_with_the_profile() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    let catalog = with_catalog(&t);
    listed_with(&t, "a", None, &["GBAAA9800001"]);
    listed_with(&t, "b", None, &["USAAA0000009"]);
    t.ticks_until_done().await;
    assert_eq!(*t.sidecar.bpms.lock(), [Some(77.1), None]);
    assert_eq!(catalog.asked.lock().len(), 2);
}

#[tokio::test]
async fn a_song_with_neither_a_bpm_tag_nor_an_isrc_asks_nothing() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    let catalog = with_catalog(&t);
    listed_with(&t, "a", None, &[]);
    t.tick().await;
    assert_eq!(*t.sidecar.bpms.lock(), [None]);
    assert!(catalog.asked.lock().is_empty());
}

#[tokio::test]
async fn a_reading_of_the_sound_alone_asks_for_no_tempo() {
    // An octo-sonic from before profiles.
    let t = make(None);
    let catalog = with_catalog(&t);
    listed_with(&t, "a", Some(128), &["GBAAA9800001"]);
    listed_with(&t, "b", None, &["GBAAA9800001"]);
    t.ticks_until_done().await;
    assert_eq!(*t.sidecar.bpms.lock(), [None, None]);
    assert!(catalog.asked.lock().is_empty());
}

#[tokio::test]
async fn a_song_already_profiled_asks_for_no_tempo() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    let catalog = with_catalog(&t);
    listed_with(&t, "a", None, &["GBAAA9800001"]);
    t.ticks_until_done().await;
    t.advance(PASS_INTERVAL + TimeDelta::minutes(1));
    t.ticks_until_done().await;
    assert_eq!(t.sidecar.analysed().len(), 1);
    assert_eq!(catalog.asked.lock().len(), 1);
}

#[tokio::test]
async fn an_outside_song_takes_its_tempo_and_isrc_from_its_own_tags() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    let catalog = with_catalog(&t);
    let tagged = |name: &str, bpm: Option<u32>, isrc: Option<&str>| {
        let path = t.library.directory.join(name);
        std::fs::write(&path, crate::services::test_support::flac()).expect("written");
        let mut file = octo_media::tags::TagFile::open(&path).expect("opens");
        if let Some(bpm) = bpm {
            file.set_bpm(bpm);
        }
        octo_media::tags::tag_writer_extras::set_text(&mut file, octo_media::tags::TagFields::ISRC, isrc);
        file.save().expect("saved");
        path.to_string_lossy().into_owned()
    };
    let with_tag = tagged("tagged.flac", Some(96), Some("GBAAA9800001"));
    let with_isrc = tagged("isrc.flac", None, Some("GB-AAA-98-00001"));
    let bare = tagged("bare.flac", None, None);
    t.worker.arrived_outside("soulseek:1", &with_tag);
    t.worker.arrived_outside("soulseek:2", &with_isrc);
    t.worker.arrived_outside("soulseek:3", &bare);
    for _ in 0..3 {
        t.tick().await;
    }
    assert_eq!(*t.sidecar.bpms.lock(), [Some(96.0), Some(77.1), None]);
    assert_eq!(*catalog.asked.lock(), [vec!["GB-AAA-98-00001".to_string()]]);
    assert_eq!(
        file_tempo_tags(&with_tag),
        (Some(96), vec!["GBAAA9800001".to_string()])
    );
}

#[test]
fn navidromes_bpm_of_0_is_no_bpm_tag() {
    let root = serde_json::json!([
        { "id": "a", "path": "A/a.flac", "bpm": 124 },
        { "id": "b", "path": "B/b.flac", "bpm": 0 },
        { "id": "c", "path": "C/c.flac" },
    ]);
    let (rows, _) = crate::services::library::quality_upgrade_worker::parse_songs(&root)
        .expect("read")
        .expect("rows");
    let bpms: Vec<Option<i32>> = rows.iter().map(|row| row.bpm).collect();
    assert_eq!(bpms, [Some(124), None, None]);
}
