//! "Tidy artists" against real files in a temp folder, with Navidrome stood in for. The order is
//! the point: tags, a scan that started after them, then whole album folders, and nothing that
//! could lose a file or a song's stars.

use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::time::SystemTime;

use async_trait::async_trait;
use octo_core::models::domain::Song;
use octo_core::models::download::DownloadHistoryEntry;
use octo_core::models::subsonic::ScanStatus;
use octo_core::settings::AppSettings;

use super::*;
use crate::services::library::explicit_backfill_store::{ExplicitBackfillRow, ExplicitBackfillSong};
use crate::services::library::library_tag_edits::TagEditEntry;
use crate::services::local::{LocalSongMapping, ParsedExternalId, ParsedSongId, ScanHold};
use crate::services::metadata::GenreJournalEntry;
use crate::services::subsonic::navidrome_version::ServerInfo;
use crate::services::test_audio::audio_or_skip;

/// Navidrome as the tidy sees it: a version, a scan that finishes as soon as it is started
/// (unless told not to), Octo's mappings, and its own hold on scans.
struct FakeNavidrome {
    info: Mutex<Option<ServerInfo>>,
    mappings: Mutex<Vec<LocalSongMapping>>,
    /// The last scan's start and finish.
    last_scan: Mutex<(DateTime<Utc>, DateTime<Utc>)>,
    scans_finish: AtomicBool,
    /// Navidrome says it is scanning, and never stops.
    scanning: AtomicBool,
    starts: AtomicUsize,
    hold: ScanHold,
    on_start: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl FakeNavidrome {
    fn new() -> Self {
        let long_ago = Utc::now() - chrono::TimeDelta::minutes(5);
        Self {
            info: Mutex::new(Some(ServerInfo::new(Some("navidrome"), Some("0.64.2 (abc)")))),
            mappings: Mutex::new(Vec::new()),
            last_scan: Mutex::new((long_ago, long_ago + chrono::TimeDelta::seconds(1))),
            scans_finish: AtomicBool::new(true),
            scanning: AtomicBool::new(false),
            starts: AtomicUsize::new(0),
            hold: ScanHold::new(),
            on_start: Mutex::new(None),
        }
    }

    fn starts(&self) -> usize {
        self.starts.load(Ordering::SeqCst)
    }

    fn own(&self, path: &str) {
        self.mappings.lock().push(LocalSongMapping {
            external_provider: "deezer".into(),
            external_id: path.to_string(),
            local_path: path.to_string(),
            ..LocalSongMapping::default()
        });
    }
}

#[async_trait]
impl ILocalLibraryService for FakeNavidrome {
    async fn get_local_path_for_external_song(&self, _: &str, _: &str) -> Option<String> {
        None
    }
    async fn register_downloaded_song(&self, _: &Song, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn get_local_id_for_external_song(&self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn parse_song_id(&self, _: &str) -> ParsedSongId {
        (false, None, None)
    }
    fn parse_external_id(&self, _: &str) -> ParsedExternalId {
        (false, None, None, None)
    }
    async fn get_mappings(&self) -> Vec<LocalSongMapping> {
        self.mappings.lock().clone()
    }
    async fn find_mapping_by_tags(
        &self,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Option<LocalSongMapping> {
        None
    }
    async fn forget_mapping(&self, _: &str) -> anyhow::Result<Vec<LocalSongMapping>> {
        Ok(Vec::new())
    }
    async fn trigger_library_scan(&self, _: bool) -> bool {
        if self.hold.defer() {
            return true;
        }
        self.start_scan().await
    }
    async fn start_scan(&self) -> bool {
        self.starts.fetch_add(1, Ordering::SeqCst);
        if let Some(hook) = self.on_start.lock().as_ref() {
            hook();
        }
        if self.scans_finish.load(Ordering::SeqCst) {
            let now = Utc::now();
            *self.last_scan.lock() = (now, now + chrono::TimeDelta::milliseconds(1));
        }
        true
    }
    fn scan_hold(&self) -> Option<ScanHold> {
        Some(self.hold.clone())
    }
    async fn get_scan_status(&self) -> Option<ScanStatus> {
        let (start, finish) = *self.last_scan.lock();
        Some(ScanStatus {
            scanning: self.scanning.load(Ordering::SeqCst),
            count: Some(1),
            last_scan: Some(finish.to_rfc3339()),
            elapsed_time: (finish - start).num_nanoseconds(),
            folder_count: None,
        })
    }
    async fn get_server_info(&self) -> Option<ServerInfo> {
        self.info.lock().clone()
    }
    async fn update_local_path(&self, old_path: &str, new_path: &str) -> anyhow::Result<usize> {
        let mut moved = 0;
        for mapping in self.mappings.lock().iter_mut() {
            if let Some(now) = PathHelper::moved_path(&mapping.local_path, old_path, new_path) {
                mapping.local_path = now;
                moved += 1;
            }
        }
        Ok(moved)
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    root: String,
    config: PathBuf,
    settings: Arc<SettingsStore>,
    worker: TidyWorker,
    navidrome: Arc<FakeNavidrome>,
    /// Run when the tidy lists Navidrome's songs, just before an apply's first change.
    on_list: Arc<Mutex<Option<Box<dyn Fn() + Send + Sync>>>>,
    notices: Arc<NoticeQueue>,
    genre: Arc<GenreBackfillJournal>,
    tag_edits: Arc<TagEditJournal>,
    history: Arc<DownloadHistoryService>,
    covers: Arc<CoverUpgradeJournal>,
    lyrics_undo: Arc<LyricsUndoJournal>,
    explicit: Arc<ExplicitBackfillStore>,
    songs: Arc<Mutex<HashMap<String, NavidromeSongEntry>>>,
    flac: Vec<u8>,
}

fn quick() -> ScanWaitTiming {
    ScanWaitTiming {
        poll: Duration::from_millis(1),
        timeout: Duration::from_secs(3),
    }
}

impl Rig {
    fn new(flac: Vec<u8>) -> Self {
        Self::with(flac, None, |worker| worker)
    }

    fn with(
        flac: Vec<u8>,
        music_brainz: Option<Arc<MusicBrainzClient>>,
        configure: impl FnOnce(TidyWorker) -> TidyWorker,
    ) -> Self {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("music");
        let config = dir.path().join("config");
        std::fs::create_dir_all(&root).expect("made");
        let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
        settings.set_raw("Library:DownloadPath", Some(&root.to_string_lossy()));
        let covers = Arc::new(CoverUpgradeJournal::new(Some(config.join("cover-journal.jsonl"))));
        let lyrics_undo = Arc::new(LyricsUndoJournal::new(Some(config.join("lyrics-undo.jsonl"))));
        let explicit = Arc::new(ExplicitBackfillStore::new(Some(config.join("explicit.json"))));
        let navidrome = Arc::new(FakeNavidrome::new());
        let notices = Arc::new(NoticeQueue::new());
        let genre = Arc::new(GenreBackfillJournal::new(Some(config.join("genre.jsonl"))));
        let tag_edits = Arc::new(TagEditJournal::new(Some(config.join("tag-edits.json"))));
        let history = Arc::new(DownloadHistoryService::new(config.join("history.json")));
        let songs: Arc<Mutex<HashMap<String, NavidromeSongEntry>>> = Arc::default();
        let on_list: Arc<Mutex<Option<Box<dyn Fn() + Send + Sync>>>> = Arc::default();
        let (listed, hook) = (songs.clone(), on_list.clone());
        let list_songs: ListSongs = Arc::new(move || {
            if let Some(hook) = hook.lock().as_ref() {
                hook();
            }
            let songs = listed.lock().clone();
            Box::pin(async move { songs })
        });
        let worker = TidyWorker::new(
            Arc::new(TidyStore::new(
                Some(config.join("tidy-run.json")),
                Some(config.join("tidy-plan.json")),
            )),
            Arc::new(TidyJournal::new(Some(config.join("tidy-journal.jsonl")))),
            settings.clone(),
            TidyServices {
                library: Some(navidrome.clone()),
                music_brainz,
                notices: Some(notices.clone()),
                genre_journal: Some(genre.clone()),
                tag_edits: Some(tag_edits.clone()),
                actions: None,
                history: Some(history.clone()),
                list_songs: Some(list_songs),
                cover_journal: Some(covers.clone()),
                lyrics_undo: Some(lyrics_undo.clone()),
                explicit: Some(explicit.clone()),
                quarantine: Some(Arc::new(LibraryActionQuarantine::new(settings.clone()))),
                lidarr: Some(Arc::new(LidarrClient::new(settings.clone()))),
                lidarr_pending: None,
                identity: None,
                family_pool: None,
                lyrics_remap: Some(LyricsPathRemap::new()),
            },
        )
        .with_timing(quick());
        let worker = configure(worker);
        Rig {
            _dir: dir,
            root: root.to_string_lossy().into_owned(),
            config,
            settings,
            worker,
            navidrome,
            on_list,
            notices,
            genre,
            tag_edits,
            history,
            covers,
            lyrics_undo,
            explicit,
            songs,
            flac,
        }
    }

    fn at(&self, relative: &str) -> String {
        format!("{}/{relative}", self.root)
    }

    /// A real, tagged FLAC, last written ten minutes ago, before Navidrome's last scan.
    fn song(&self, relative: &str, artist: &str, artists: &[&str], title: &str) -> String {
        let path = self.at(relative);
        std::fs::create_dir_all(Path::new(&path).parent().expect("a folder")).expect("made");
        std::fs::write(&path, &self.flac).expect("written");
        let mut file = TagFile::open(&path).expect("opens");
        file.set_performers(&[artist.to_string()]);
        file.set_album_artists(&[artist.to_string()]);
        file.set_title(Some(title));
        if !artists.is_empty() {
            let values: Vec<String> = artists.iter().map(|v| v.to_string()).collect();
            extras::set_multi_value(&mut file, "ARTISTS", &values);
        }
        file.save().expect("saved");
        age(&path);
        path
    }

    fn collab(&self) -> String {
        self.song(
            "Bizarrap, Rauw Alejandro/Album/01 - Session.flac",
            "Bizarrap, Rauw Alejandro",
            &["Bizarrap", "Rauw Alejandro"],
            "Session (feat. Guest)",
        )
    }

    async fn preview(&self, scope: TidyScope) {
        self.run(TidyRequest {
            mode: TidyMode::Preview,
            scope,
            ..TidyRequest::default()
        })
        .await;
    }

    async fn apply(&self, scope: TidyScope, ticked: &[String]) {
        self.run(TidyRequest {
            mode: TidyMode::Apply,
            scope,
            ticked: ticked.to_vec(),
            ..TidyRequest::default()
        })
        .await;
    }

    async fn run(&self, request: TidyRequest) {
        self.worker.run_once(request, &CancellationToken::new()).await;
    }

    fn run_state(&self) -> TidyRun {
        self.worker.current()
    }
}

fn age(path: &str) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(SystemTime::now() - Duration::from_secs(600)))
        .expect("aged");
}

fn tags(path: &str) -> TidyTags {
    read_tags(&TagFile::open(path).expect("opens"))
}

fn exists(path: &str) -> bool {
    Path::new(path).exists()
}

const WHOLE: TidyScope = TidyScope::WholeLibrary;

// ---- preview --------------------------------------------------------------------------------

#[tokio::test]
async fn preview_writes_nothing() {
    let rig = Rig::new(audio_or_skip!("flac"));
    let source = rig.collab();
    let bytes = std::fs::read(&source).expect("read");

    rig.preview(WHOLE).await;

    let run = rig.run_state();
    assert_eq!(run.status, TidyStatus::Completed, "{:?}", run.reason);
    assert_eq!((run.planned_retags, run.planned_moves), (1, 1));
    let plan = rig.worker.store().plan().expect("the plan is kept");
    assert!(plan.plan.rows.iter().any(|row| row.kind == TidyKind::Split));
    assert_eq!(std::fs::read(&source).expect("read"), bytes);
    assert!(!exists(&rig.at("Bizarrap")));
    assert!(!rig.worker.journal().exists());
    assert_eq!(rig.navidrome.starts(), 0);
}

#[tokio::test]
async fn a_semicolon_credit_naming_the_title_guest_is_left_as_it_is() {
    let rig = Rig::new(audio_or_skip!("flac"));
    let source = rig.song(
        "SAYGRACE/You Don't Own Me/You Don't Own Me.flac",
        "SAYGRACE; G\u{2010}Eazy",
        &[],
        "You Don't Own Me (feat. G-Eazy)",
    );
    let bytes = std::fs::read(&source).expect("read");

    rig.preview(WHOLE).await;
    let plan = rig.worker.store().plan().expect("the plan is kept");
    assert!(!plan.plan.rows.iter().any(|row| row.kind == TidyKind::Guest));
    assert!(plan.plan.retags.is_empty());
    rig.apply(WHOLE, &[]).await;

    assert_eq!(std::fs::read(&source).expect("read"), bytes);
    assert!(tags(&source).artists.is_empty());
}

// ---- apply ----------------------------------------------------------------------------------

#[tokio::test]
async fn apply_writes_tags_waits_for_a_scan_then_moves() {
    let rig = Rig::new(audio_or_skip!("flac"));
    let source = rig.collab();
    let target = rig.at("Bizarrap/Album/01 - Session.flac");
    let seen: Arc<Mutex<Vec<(bool, bool)>>> = Arc::default();
    {
        let (seen, source) = (seen.clone(), source.clone());
        *rig.navidrome.on_start.lock() = Some(Box::new(move || {
            let tagged = exists(&source) && tags(&source).artists.contains(&"Guest".to_string());
            seen.lock().push((exists(&source), tagged));
        }));
    }

    rig.preview(WHOLE).await;
    rig.apply(WHOLE, &[]).await;

    let run = rig.run_state();
    assert_eq!(run.status, TidyStatus::Completed, "{:?}", run.reason);
    // The first scan began with the file re-tagged and still where it was.
    assert_eq!(seen.lock().first(), Some(&(true, true)));
    assert!(exists(&target));
    assert!(!exists(&rig.at("Bizarrap, Rauw Alejandro")));
    let after = tags(&target);
    assert_eq!(after.artists, ["Bizarrap", "Rauw Alejandro", "Guest"]);
    assert_eq!(after.artist, ["Bizarrap, Rauw Alejandro"]);
    assert_eq!(
        TagFile::open(&target).expect("opens").title().as_deref(),
        Some("Session (feat. Guest)")
    );
    // One scan between the phases, one after the moves.
    assert_eq!(rig.navidrome.starts(), 2);
    assert_eq!((run.retagged, run.moved), (1, 1));
}

#[tokio::test]
async fn apply_refused_before_navidrome_055() {
    let rig = Rig::new(audio_or_skip!("flac"));
    *rig.navidrome.info.lock() = Some(ServerInfo::new(Some("navidrome"), Some("0.54.5 (abc)")));
    let source = rig.collab();
    let bytes = std::fs::read(&source).expect("read");

    rig.preview(WHOLE).await;
    assert_eq!(rig.run_state().status, TidyStatus::Completed);
    rig.apply(WHOLE, &[]).await;

    let run = rig.run_state();
    assert_eq!(run.status, TidyStatus::Failed);
    assert!(
        run.reason.as_deref().is_some_and(|why| why.contains("0.55")),
        "{:?}",
        run.reason
    );
    assert_eq!(std::fs::read(&source).expect("read"), bytes);
    assert!(!rig.worker.journal().exists());
}

#[tokio::test]
async fn apply_without_a_preview_is_refused() {
    let rig = Rig::new(audio_or_skip!("flac"));
    rig.collab();

    rig.apply(WHOLE, &[]).await;

    let run = rig.run_state();
    assert_eq!(run.status, TidyStatus::Failed);
    assert!(
        run.reason
            .as_deref()
            .is_some_and(|why| why.contains("Preview first"))
    );
}

#[tokio::test]
async fn apply_library_changed_since_the_preview_touches_nothing() {
    let rig = Rig::new(audio_or_skip!("flac"));
    let source = rig.collab();
    rig.preview(WHOLE).await;
    let other = rig.song("Beyonce/A/01 - S.flac", "Beyonc\u{e9}", &[], "S");

    rig.apply(WHOLE, &[]).await;

    let run = rig.run_state();
    assert_eq!(run.status, TidyStatus::Failed);
    assert!(
        run.reason
            .as_deref()
            .is_some_and(|why| why.contains("changed since the preview"))
    );
    assert!(exists(&source) && exists(&other));
    assert!(!rig.worker.journal().exists());
}

#[tokio::test]
async fn apply_a_taken_album_folder_keeps_both() {
    let rig = Rig::new(audio_or_skip!("flac"));
    let source = rig.song("Beyonce/A/01 - S.flac", "Beyonc\u{e9}", &[], "S");
    rig.preview(WHOLE).await;
    // Something lands at the target after the preview: no audio, so the plan is the same.
    std::fs::create_dir_all(rig.at("Beyonc\u{e9}/A")).expect("made");
    std::fs::write(rig.at("Beyonc\u{e9}/A/notes.txt"), "mine").expect("written");

    rig.apply(WHOLE, &[]).await;

    let run = rig.run_state();
    assert_eq!(run.status, TidyStatus::Completed, "{:?}", run.reason);
    assert!(exists(&source));
    assert!(exists(&rig.at("Beyonc\u{e9}/A/notes.txt")));
    assert!(
        run.held_rows
            .iter()
            .any(|row| row.evidence.contains("already there"))
    );
}

#[tokio::test]
async fn apply_lyrics_the_cover_and_the_cue_travel_with_the_folder() {
    let rig = Rig::new(audio_or_skip!("flac"));
    let source = rig.song("Beyonce/A/01 - S.flac", "Beyonc\u{e9}", &[], "S");
    for (name, body) in [
        ("01 - S.lrc", "[00:01.00]hello"),
        ("cover.jpg", "jpg"),
        ("A.cue", "FILE"),
    ] {
        std::fs::write(rig.at(&format!("Beyonce/A/{name}")), body).expect("written");
    }

    rig.preview(WHOLE).await;
    rig.apply(WHOLE, &[]).await;

    assert_eq!(
        rig.run_state().status,
        TidyStatus::Completed,
        "{:?}",
        rig.run_state().reason
    );
    for name in ["01 - S.flac", "01 - S.lrc", "cover.jpg", "A.cue"] {
        assert!(exists(&rig.at(&format!("Beyonc\u{e9}/A/{name}"))), "{name}");
    }
    assert!(!exists(&source));
    // Emptied, so gone, and never above the music folder.
    assert!(!exists(&rig.at("Beyonce")));
    assert!(exists(&rig.root));
}

#[tokio::test]
async fn apply_octos_records_follow_the_file() {
    let rig = Rig::new(audio_or_skip!("flac"));
    let source = rig.song("Beyonce/A/01 - S.flac", "Beyonc\u{e9}", &[], "S");
    let target = rig.at("Beyonc\u{e9}/A/01 - S.flac");
    rig.navidrome.own(&source);
    rig.notices.add_review(
        "alice",
        &source,
        &Song {
            artist: "Beyonc\u{e9}".into(),
            title: "S".into(),
            ..Song::default()
        },
        &octo_core::fingerprint::VerificationResult::default(),
    );
    rig.genre.append(&GenreJournalEntry::new(
        &source,
        vec!["Pop".into()],
        vec!["R&B".into()],
        Utc::now(),
        "g1",
    ));
    rig.tag_edits.record(
        TagEditEntry {
            path: source.clone(),
            ..TagEditEntry::default()
        },
        true,
    );
    rig.history.record(DownloadHistoryEntry {
        path: source.clone(),
        ..DownloadHistoryEntry::default()
    });

    rig.preview(TidyScope::OctoDownloads).await;
    rig.apply(TidyScope::OctoDownloads, &[]).await;

    assert_eq!(
        rig.run_state().status,
        TidyStatus::Completed,
        "{:?}",
        rig.run_state().reason
    );
    assert!(exists(&target));
    assert_eq!(rig.navidrome.mappings.lock()[0].local_path, target);
    assert_eq!(rig.notices.recent(10)[0].local_path, target);
    assert_eq!(rig.genre.read_all()[0].path, target);
    assert_eq!(rig.tag_edits.last("").expect("kept").path, target);
    assert_eq!(
        TagEditJournal::new(Some(rig.config.join("tag-edits.json")))
            .last("")
            .expect("on disk")
            .path,
        target
    );
    assert_eq!(rig.history.get_recent(10)[0].path, target);
}

#[tokio::test]
async fn apply_the_cover_lyrics_explicit_and_trash_records_follow_the_file() {
    let rig = Rig::new(audio_or_skip!("flac"));
    let source = rig.song("Beyonce/A/01 - S.flac", "Beyonc\u{e9}", &[], "S");
    let target = rig.at("Beyonc\u{e9}/A/01 - S.flac");
    let gone = rig.at("Beyonce/A/02 - Gone.flac");
    let moved_gone = rig.at("Beyonc\u{e9}/A/02 - Gone.flac");
    let lyrics = rig.at("Beyonce/A/01 - S.lrc");

    assert!(
        rig.covers
            .record(&source, CoverUpgradeJournal::EMBEDDED, Some(b"old"), "c1")
    );
    rig.lyrics_undo
        .record(&lyrics, LyricsUndoJournal::BESIDE, None, "l1")
        .expect("recorded");
    rig.explicit.update(|run| {
        run.rows.push(ExplicitBackfillRow {
            path: source.clone(),
            ..ExplicitBackfillRow::default()
        });
        run.queue.push(ExplicitBackfillSong {
            path: source.clone(),
            ..ExplicitBackfillSong::default()
        });
    });
    // A song of the album removed earlier waits in the trash with its note.
    let trashed = rig.at(".octo-trash/2026-10-01/Beyonce/A/02 - Gone.flac");
    std::fs::create_dir_all(Path::new(&trashed).parent().expect("a folder")).expect("made");
    std::fs::write(&trashed, &rig.flac).expect("written");
    std::fs::write(
        format!("{trashed}{}", LibraryActionQuarantine::MANIFEST_SUFFIX),
        serde_json::json!({
            "OriginalPath": gone,
            "NavidromeId": "nd-2",
            "Action": "Delete",
            "Username": "alice",
            "AtUtc": "2026-10-01T00:00:00Z",
        })
        .to_string(),
    )
    .expect("written");

    rig.preview(WHOLE).await;
    rig.apply(WHOLE, &[]).await;

    assert_eq!(
        rig.run_state().status,
        TidyStatus::Completed,
        "{:?}",
        rig.run_state().reason
    );
    assert!(exists(&target));
    assert_eq!(rig.covers.read_all()[0].path, target);
    assert_eq!(
        rig.lyrics_undo.read_all().expect("read")[0].path,
        rig.at("Beyonc\u{e9}/A/01 - S.lrc")
    );
    let run = ExplicitBackfillStore::new(Some(rig.config.join("explicit.json"))).current();
    assert_eq!(run.rows[0].path, target);
    assert_eq!(run.queue[0].path, target);
    let quarantine = LibraryActionQuarantine::new(Arc::new(SettingsStore::for_tests(AppSettings::default())));
    assert_eq!(
        quarantine.read_manifest(&trashed).expect("a note").original_path,
        moved_gone
    );
}

#[test]
fn a_hidden_file_beside_the_songs_is_not_one_of_the_albums_songs() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let album = dir.path().join("A");
    std::fs::create_dir_all(album.join(".hidden")).expect("made");
    std::fs::write(album.join("01 - S.flac"), b"x").expect("written");
    std::fs::write(album.join(".octo-keep-0a1b.flac"), b"x").expect("written");
    std::fs::write(album.join(".hidden/02 - T.flac"), b"x").expect("written");

    let contents = walk_all(&album, &HashSet::new()).expect("walked");
    assert_eq!(
        contents.audio,
        [format!("{}/01 - S.flac", album.to_string_lossy())]
    );
}

#[tokio::test]
async fn octo_downloads_scope_leaves_other_files_alone() {
    let rig = Rig::new(audio_or_skip!("flac"));
    let mine = rig.song("Beyonce/A/01 - S.flac", "Beyonc\u{e9}", &[], "S");
    let theirs = rig.song("Beyonce/A/02 - T.flac", "Beyonc\u{e9}", &[], "T");
    let own_album = rig.song("Beyonce/B/01 - U.flac", "Beyonc\u{e9}", &[], "U");
    rig.navidrome.own(&mine);
    rig.navidrome.own(&own_album);

    rig.preview(TidyScope::OctoDownloads).await;
    rig.apply(TidyScope::OctoDownloads, &[]).await;

    assert_eq!(
        rig.run_state().status,
        TidyStatus::Completed,
        "{:?}",
        rig.run_state().reason
    );
    // A folder only moves whole: the album holding someone else's song stays.
    assert!(exists(&mine) && exists(&theirs));
    assert!(exists(&rig.at("Beyonc\u{e9}/B/01 - U.flac")));
}

#[tokio::test]
async fn apply_a_file_changed_after_the_last_scan_stays_where_it_is() {
    let rig = Rig::new(audio_or_skip!("flac"));
    let source = rig.song("Beyonce/A/01 - S.flac", "Beyonc\u{e9}", &[], "S");
    rig.preview(WHOLE).await;
    std::fs::File::options()
        .write(true)
        .open(&source)
        .and_then(|file| file.set_modified(SystemTime::now()))
        .expect("touched");

    rig.apply(WHOLE, &[]).await;

    assert!(exists(&source));
    assert!(
        rig.run_state()
            .held_rows
            .iter()
            .any(|row| row.evidence.contains("last scan"))
    );
}

#[tokio::test]
async fn apply_a_scan_that_never_finishes_moves_nothing() {
    let rig = Rig::new(audio_or_skip!("flac"));
    rig.navidrome.scans_finish.store(false, Ordering::SeqCst);
    let source = rig.collab();

    rig.preview(WHOLE).await;
    rig.apply(WHOLE, &[]).await;

    let run = rig.run_state();
    assert_eq!(run.status, TidyStatus::Failed);
    assert!(
        run.reason
            .as_deref()
            .is_some_and(|why| why.contains("did not finish a scan")),
        "{:?}",
        run.reason
    );
    assert!(exists(&source));
    assert!(tags(&source).artists.contains(&"Guest".to_string()));
}

#[tokio::test]
async fn journal_write_failure_stops_the_run() {
    let rig = Rig::new(audio_or_skip!("flac"));
    let source = rig.collab();
    let bytes = std::fs::read(&source).expect("read");
    rig.preview(WHOLE).await;
    rig.worker.journal().refuse_writes();

    rig.apply(WHOLE, &[]).await;

    let run = rig.run_state();
    assert_eq!(run.status, TidyStatus::Failed);
    assert!(
        run.reason.as_deref().is_some_and(|why| why.contains("journal")),
        "{:?}",
        run.reason
    );
    assert_eq!(std::fs::read(&source).expect("read"), bytes);
    assert_eq!(run.retagged, 0);
}

#[tokio::test]
async fn try_enqueue_a_second_run_waits_its_turn() {
    let rig = Rig::new(audio_or_skip!("flac"));
    assert!(rig.worker.try_enqueue(TidyRequest::default()));
    assert!(!rig.worker.try_enqueue(TidyRequest::default()));
}

#[tokio::test]
async fn star_loss_rows_need_a_tick_to_apply() {
    let rig = Rig::new(audio_or_skip!("flac"));
    rig.song("Beyonc\u{e9}/A/01 - S.flac", "Beyonc\u{e9}", &[], "S");
    rig.song("Beyonc\u{e9}/A/02 - T.flac", "Beyonc\u{e9}", &[], "T");
    let loser = rig.song("Beyonce/B/01 - U.flac", "Beyonce", &[], "U");

    rig.preview(WHOLE).await;
    let plan = rig.worker.store().plan().expect("kept").plan;
    let rename = plan
        .rows
        .iter()
        .find(|row| row.kind == TidyKind::Rename)
        .expect("a rename row")
        .clone();
    assert!(rename.opt_in && !rename.ticked);
    rig.apply(WHOLE, &[]).await;
    assert_eq!(
        rig.run_state().status,
        TidyStatus::Completed,
        "{:?}",
        rig.run_state().reason
    );
    assert!(exists(&loser));
    assert_eq!(tags(&loser).artist, ["Beyonce"]);

    rig.preview(WHOLE).await;
    rig.apply(WHOLE, &[rename.id]).await;
    let moved = rig.at("Beyonc\u{e9}/B/01 - U.flac");
    assert_eq!(
        rig.run_state().status,
        TidyStatus::Completed,
        "{:?}",
        rig.run_state().reason
    );
    assert!(exists(&moved));
    assert_eq!(tags(&moved).artist, ["Beyonc\u{e9}"]);
}

#[tokio::test]
async fn files_tagged_from_musicbrainz_are_untouched() {
    let rig = Rig::new(audio_or_skip!("flac"));
    let tagged = rig.song("Drake/A/01 - Work.flac", "Drake", &[], "Work (feat. Rihanna)");
    {
        let mut file = TagFile::open(&tagged).expect("opens");
        extras::set_multi(
            &mut file,
            TagFields::ARTIST_ID,
            &["9fff2f8a-21e6-47de-a2b8-7f449929d43f".to_string()],
        );
        file.save().expect("saved");
        age(&tagged);
    }
    let bytes = std::fs::read(&tagged).expect("read");

    rig.preview(WHOLE).await;
    rig.apply(WHOLE, &[]).await;

    assert_eq!(rig.run_state().planned_retags, 0);
    assert_eq!(std::fs::read(&tagged).expect("read"), bytes);
}

#[tokio::test]
async fn playlists_follow_the_moved_songs_and_are_journaled() {
    let rig = Rig::new(audio_or_skip!("flac"));
    rig.song("Beyonce/A/01 - S.flac", "Beyonc\u{e9}", &[], "S");
    let playlist = rig.at("Mixes/road.m3u8");
    std::fs::create_dir_all(rig.at("Mixes")).expect("made");
    let text = format!(
        "#EXTM3U\r\n../Beyonce/A/01 - S.flac\r\n{}/Beyonce/A/01 - S.flac\r\n",
        rig.root
    );
    std::fs::write(&playlist, &text).expect("written");

    rig.preview(WHOLE).await;
    rig.apply(WHOLE, &[]).await;

    let now = std::fs::read_to_string(&playlist).expect("read");
    assert_eq!(
        now,
        format!(
            "#EXTM3U\r\n../Beyonc\u{e9}/A/01 - S.flac\r\n{}/Beyonc\u{e9}/A/01 - S.flac\r\n",
            rig.root
        )
    );
    assert!(rig.worker.journal().read_all().iter().any(|entry| matches!(
        entry,
        TidyJournalEntry::Playlist { before, .. } if *before == text
    )));
}

/// A playlist is written whole beside itself and moved into place, so a stop part way never
/// leaves half of it: the old file is never written into.
#[tokio::test]
async fn a_playlist_is_replaced_whole_never_written_into() {
    let rig = Rig::new(audio_or_skip!("flac"));
    rig.song("Beyonce/A/01 - S.flac", "Beyonc\u{e9}", &[], "S");
    let playlist = rig.at("Mixes/road.m3u");
    std::fs::create_dir_all(rig.at("Mixes")).expect("made");
    let text = "#EXTM3U\n../Beyonce/A/01 - S.flac\n";
    std::fs::write(&playlist, text).expect("written");
    // A second name for the same file: an in-place write would change what it reads too.
    let other_name = rig.config.join("road-link.m3u");
    std::fs::create_dir_all(&rig.config).expect("made");
    std::fs::hard_link(&playlist, &other_name).expect("linked");

    rig.preview(WHOLE).await;
    rig.apply(WHOLE, &[]).await;

    assert_eq!(
        std::fs::read_to_string(&playlist).expect("read"),
        "#EXTM3U\n../Beyonc\u{e9}/A/01 - S.flac\n"
    );
    assert_eq!(std::fs::read_to_string(&other_name).expect("read"), text);
}

#[tokio::test]
async fn a_moved_song_navidrome_lost_track_of_is_listed() {
    let rig = Rig::new(audio_or_skip!("flac"));
    let source = rig.song("Beyonce/A/01 - S.flac", "Beyonc\u{e9}", &[], "S");
    let entry = |path: &str, id: &str, missing: bool| NavidromeSongEntry {
        full_path: path.to_string(),
        album_id: None,
        artist: None,
        title: None,
        album: None,
        lyrics: None,
        id: Some(id.to_string()),
        duration: None,
        explicit_status: None,
        missing,
    };
    rig.songs
        .lock()
        .insert(source.clone(), entry(&source, "nd-1", false));
    {
        let (songs, source) = (rig.songs.clone(), source.clone());
        *rig.navidrome.on_start.lock() = Some(Box::new(move || {
            // Navidrome sees the old path gone and the new one as a different song.
            let mut songs = songs.lock();
            if songs.len() == 1 && !Path::new(&source).exists() {
                songs.insert(source.clone(), entry(&source, "nd-1", true));
            }
        }));
    }

    rig.preview(WHOLE).await;
    rig.apply(WHOLE, &[]).await;

    let run = rig.run_state();
    assert_eq!(run.status, TidyStatus::Completed, "{:?}", run.reason);
    assert!(
        run.held_rows
            .iter()
            .any(|row| row.evidence.contains("did not match this moved song"))
    );
}

#[tokio::test]
async fn the_one_writer_gate_refuses_an_apply_while_another_job_writes() {
    let gate = LibraryWriteGate::new();
    let shared = gate.clone();
    let rig = Rig::with(audio_or_skip!("flac"), None, move |worker| {
        worker.with_write_gate(shared)
    });
    let source = rig.collab();
    rig.preview(WHOLE).await;
    let genre = gate.begin_job("The genre re-tag").await.expect("free");

    rig.apply(WHOLE, &[]).await;

    let run = rig.run_state();
    assert_eq!(run.status, TidyStatus::Failed);
    assert!(
        run.reason
            .as_deref()
            .is_some_and(|why| why.contains("The genre re-tag is changing"))
    );
    assert!(exists(&source));
    drop(genre);
}

/// While the tidy waits for Navidrome's scan between the tags and the moves, a download can be
/// placed; no other job can start meanwhile.
#[tokio::test]
async fn a_download_is_placed_while_the_tidy_waits_for_a_scan() {
    let gate = LibraryWriteGate::new();
    let shared = gate.clone();
    let rig = Rig::with(audio_or_skip!("flac"), None, move |worker| {
        worker.with_write_gate(shared)
    });
    rig.navidrome.scans_finish.store(false, Ordering::SeqCst);
    rig.collab();
    rig.preview(WHOLE).await;

    let placed = async {
        while rig.run_state().phase != "Waiting for Navidrome to scan" {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let share = tokio::time::timeout(Duration::from_secs(1), gate.write()).await;
        let other_job = gate.begin_job("The genre re-tag").await.err();
        (share.is_ok(), gate.job(), other_job)
    };
    let ((placed, holder, other_job), ()) = tokio::join!(placed, rig.apply(WHOLE, &[]));

    assert!(placed, "a download waited for the whole scan");
    assert_eq!(holder.as_deref(), Some(TidyWorker::JOB_NAME));
    assert_eq!(other_job.as_deref(), Some(TidyWorker::JOB_NAME));
}

#[tokio::test]
async fn resume_carries_on_a_stopped_apply() {
    let rig = Rig::new(audio_or_skip!("flac"));
    rig.navidrome.scans_finish.store(false, Ordering::SeqCst);
    let source = rig.collab();
    rig.preview(WHOLE).await;
    rig.apply(WHOLE, &[]).await;
    assert_eq!(rig.run_state().status, TidyStatus::Failed);
    assert!(exists(&source));

    rig.navidrome.scans_finish.store(true, Ordering::SeqCst);
    rig.run(TidyRequest {
        mode: TidyMode::Apply,
        scope: WHOLE,
        resume: true,
        ..TidyRequest::default()
    })
    .await;

    let run = rig.run_state();
    assert_eq!(run.status, TidyStatus::Completed, "{:?}", run.reason);
    assert!(exists(&rig.at("Bizarrap/Album/01 - Session.flac")));
    assert_eq!(run.moved, 1);
}

// ---- MusicBrainz ----------------------------------------------------------------------------

/// A folder whose songs carry no credit splits only when its parts are library artists and
/// MusicBrainz does not know the whole. Lookups past a preview's budget wait for the next
/// preview, and an apply asks nothing new: it works from the answers the preview kept.
#[tokio::test]
async fn musicbrainz_lookups_are_budgeted_and_apply_asks_nothing_new() {
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let flac = audio_or_skip!("flac");
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"artists":[]}"#))
        .mount(&server)
        .await;
    let base = url::Url::parse(&format!("{}/ws/2/", server.uri())).expect("parses");
    let client = Arc::new(MusicBrainzClient::with_base_url(base));
    let rig = Rig::with(flac, Some(client), |worker| worker);
    rig.worker.set_max_lookups(0);
    rig.song("Foo/A/01 - X.flac", "Foo", &[], "X");
    rig.song("Bar/A/01 - Y.flac", "Bar", &[], "Y");
    let source = rig.song("Foo, Bar/B/01 - S.flac", "Foo, Bar", &[], "S");

    rig.preview(WHOLE).await;
    assert_eq!(rig.run_state().planned_moves, 0);
    assert_eq!(rig.run_state().lookups_not_asked, 1);
    assert!(server.received_requests().await.unwrap_or_default().is_empty());

    rig.worker.set_max_lookups(TidyWorker::MAX_LOOKUPS);
    rig.preview(WHOLE).await;
    assert_eq!(rig.run_state().planned_moves, 1);
    let asked = server.received_requests().await.unwrap_or_default().len();
    assert_eq!(asked, 1);

    rig.apply(WHOLE, &[]).await;
    assert_eq!(
        rig.run_state().status,
        TidyStatus::Completed,
        "{:?}",
        rig.run_state().reason
    );
    assert_eq!(server.received_requests().await.unwrap_or_default().len(), asked);
    assert!(!exists(&source));
    assert_eq!(tags(&rig.at("Foo/B/01 - S.flac")).artists, ["Foo", "Bar"]);
}

// ---- pure helpers ---------------------------------------------------------------------------

#[test]
fn a_playlist_line_outside_the_moved_folders_is_left_as_it_is() {
    let moved = vec![("/m/Beyonce/A".to_string(), "/m/Beyonc\u{e9}/A".to_string())];
    assert_eq!(
        rewrite_playlist("/m/list.m3u", "#EXTM3U\nOther/x.flac\n", &moved),
        None
    );
    assert_eq!(
        rewrite_playlist("/m/list.m3u", "Beyonce/A/x.flac\n", &moved).as_deref(),
        Some("Beyonc\u{e9}/A/x.flac\n")
    );
}

#[test]
fn a_case_sensitive_folder_is_told_apart_without_writing() {
    let dir = tempfile::tempdir().expect("a temp dir");
    std::fs::create_dir_all(dir.path().join("Artist")).expect("made");
    let before: Vec<_> = std::fs::read_dir(dir.path())
        .expect("listed")
        .flatten()
        .map(|e| e.file_name())
        .collect();
    let insensitive = case_insensitive(&dir.path().to_string_lossy());
    let after: Vec<_> = std::fs::read_dir(dir.path())
        .expect("listed")
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(before, after);
    assert_eq!(insensitive, dir.path().join("ARTIST").exists());
}

// ---- undo -----------------------------------------------------------------------------------

impl Rig {
    async fn undo(&self, all_runs: bool) {
        self.run(TidyRequest {
            mode: TidyMode::Undo,
            all_runs,
            ..TidyRequest::default()
        })
        .await;
    }
}

#[tokio::test]
async fn undo_puts_the_folder_back_then_its_tags() {
    let rig = Rig::new(audio_or_skip!("flac"));
    let source = rig.collab();
    let before = tags(&source);
    rig.navidrome.own(&source);
    rig.preview(WHOLE).await;
    rig.apply(WHOLE, &[]).await;
    let target = rig.at("Bizarrap/Album/01 - Session.flac");
    assert!(exists(&target));
    let starts = rig.navidrome.starts();
    let seen: Arc<Mutex<Vec<(bool, bool)>>> = Arc::default();
    {
        let (seen, source) = (seen.clone(), source.clone());
        *rig.navidrome.on_start.lock() = Some(Box::new(move || {
            let still_new = exists(&source) && tags(&source).artists.contains(&"Guest".to_string());
            seen.lock().push((exists(&source), still_new));
        }));
    }

    rig.undo(false).await;

    let run = rig.run_state();
    assert_eq!(run.status, TidyStatus::Completed, "{:?}", run.reason);
    assert!(exists(&source));
    assert!(!exists(&target));
    assert_eq!(tags(&source), before);
    // The scan that saw the folder back came before the tags changed.
    assert_eq!(seen.lock().first(), Some(&(true, true)));
    assert_eq!(rig.navidrome.mappings.lock()[0].local_path, source);
    assert!(!rig.worker.journal().exists());
    assert!(rig.navidrome.starts() >= starts + 2);
}

#[tokio::test]
async fn undo_never_moves_onto_a_taken_path_and_keeps_the_entry() {
    let rig = Rig::new(audio_or_skip!("flac"));
    let source = rig.song("Beyonce/A/01 - S.flac", "Beyonc\u{e9}", &[], "S");
    rig.preview(WHOLE).await;
    rig.apply(WHOLE, &[]).await;
    let target = rig.at("Beyonc\u{e9}/A/01 - S.flac");
    assert!(exists(&target));
    // Something new lands at the old path.
    std::fs::create_dir_all(Path::new(&source).parent().expect("a folder")).expect("made");
    std::fs::write(&source, b"a newcomer").expect("written");

    rig.undo(false).await;

    assert!(exists(&target));
    assert_eq!(std::fs::read(&source).expect("read"), b"a newcomer");
    assert!(rig.worker.journal().read_all().iter().any(|entry| matches!(
        entry,
        TidyJournalEntry::Move { to, .. } if *to == rig.at("Beyonc\u{e9}/A")
    )));
    assert!(
        rig.run_state()
            .held_rows
            .iter()
            .any(|row| row.evidence.contains("Something else is at its old path"))
    );
}

#[tokio::test]
async fn undo_covers_the_last_run_by_default() {
    let rig = Rig::new(audio_or_skip!("flac"));
    let first = rig.song("Beyonce/A/01 - S.flac", "Beyonc\u{e9}", &[], "S");
    rig.preview(WHOLE).await;
    rig.apply(WHOLE, &[]).await;
    let second = rig.song("Adele/B/01 - T.flac", "Adele", &[], "T (feat. Guest)");
    rig.preview(WHOLE).await;
    rig.apply(WHOLE, &[]).await;
    assert_eq!(tags(&second).artists, ["Adele", "Guest"]);

    rig.undo(false).await;
    assert!(tags(&second).artists.is_empty());
    assert!(
        exists(&rig.at("Beyonc\u{e9}/A/01 - S.flac")),
        "the first run stays"
    );
    assert!(rig.worker.journal().exists());

    rig.undo(false).await;
    assert!(exists(&first));
    assert!(!rig.worker.journal().exists());
}

#[tokio::test]
async fn undo_leaves_tags_someone_edited_since() {
    let rig = Rig::new(audio_or_skip!("flac"));
    let song = rig.song("Adele/B/01 - T.flac", "Adele", &[], "T (feat. Guest)");
    rig.preview(WHOLE).await;
    rig.apply(WHOLE, &[]).await;
    let mut file = TagFile::open(&song).expect("opens");
    extras::set_multi_value(
        &mut file,
        "ARTISTS",
        &["Adele".to_string(), "Someone".to_string()],
    );
    file.save().expect("saved");

    rig.undo(false).await;

    assert_eq!(tags(&song).artists, ["Adele", "Someone"]);
    assert!(rig.worker.journal().exists());
}

#[test]
fn remaining_a_restored_tag_entry_supersedes_newer_ones() {
    let tags_of = |run: &str| TidyJournalEntry::Tags {
        path: "/m/a.flac".into(),
        before: TidyTags::default(),
        after: TidyTags::default(),
        run: run.into(),
        at: Utc::now(),
    };
    let movement = TidyJournalEntry::Move {
        from: "/m/B".into(),
        to: "/m/C".into(),
        run: "r2".into(),
        at: Utc::now(),
        file: false,
        pool: false,
    };
    let oldest_first = vec![tags_of("r1"), movement.clone(), tags_of("r2")];

    let left = remaining(&oldest_first, &[0].into());

    assert_eq!(left, [movement]);
}

#[test]
fn walk_reads_a_three_dot_folder() {
    let dir = tempfile::tempdir().expect("a temp dir");
    for name in ["...And Justice for All/01 - One.flac", ".hidden/02 - Two.flac"] {
        let file = dir.path().join(name);
        std::fs::create_dir_all(file.parent().expect("a folder")).expect("made");
        std::fs::write(&file, b"x").expect("written");
    }
    let found = walk(dir.path(), &AUDIO_EXTENSIONS).expect("walked");
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].ends_with("...And Justice for All/01 - One.flac"),
        "{found:?}"
    );
}

// ---- the file layout ------------------------------------------------------------------------

use octo_core::settings::FolderStructure;

impl Rig {
    fn set_layout(&self, structure: FolderStructure, template: &str) {
        let mut settings = (*self.settings.current()).clone();
        settings.subsonic.folder_structure = structure;
        settings.subsonic.folder_template = template.to_string();
        self.settings.set(settings);
    }

    /// Lidarr at `url`, its root folder the music folder.
    fn set_lidarr(&self, url: &str) {
        let mut settings = (*self.settings.current()).clone();
        settings.lidarr.base_url = Some(url.to_string());
        settings.lidarr.api_key = Some("key".to_string());
        settings.lidarr.root_folder_path = Some(self.root.clone());
        self.settings.set(settings);
    }

    /// A real, tagged FLAC of one album track, last written ten minutes ago.
    fn track(&self, relative: &str, artist: &str, album: &str, title: &str, track: u32, year: u32) -> String {
        let path = self.at(relative);
        std::fs::create_dir_all(Path::new(&path).parent().expect("a folder")).expect("made");
        std::fs::write(&path, &self.flac).expect("written");
        let mut file = TagFile::open(&path).expect("opens");
        file.set_performers(&[artist.to_string()]);
        file.set_album_artists(&[artist.to_string()]);
        file.set_album(Some(album));
        file.set_title(Some(title));
        file.set_track(track);
        if year > 0 {
            file.set_year(year);
        }
        file.save().expect("saved");
        age(&path);
        path
    }

    async fn layout_run(&self, mode: TidyMode, resume: bool) {
        self.run(TidyRequest {
            mode,
            scope: WHOLE,
            task: TidyTask::FileLayout,
            resume,
            ..TidyRequest::default()
        })
        .await;
    }

    fn on_list(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.on_list.lock() = Some(Box::new(hook));
    }
}

fn completed(run: &TidyRun) {
    assert_eq!(
        run.status,
        TidyStatus::Completed,
        "{:?} {:?}",
        run.reason,
        run.errors
    );
}

fn held_saying(run: &TidyRun, words: &str) -> bool {
    run.held_rows.iter().any(|row| row.evidence.contains(words))
}

#[tokio::test]
async fn file_layout_apply_moves_files_and_undo_puts_them_back() {
    let rig = Rig::new(audio_or_skip!("flac"));
    rig.set_layout(FolderStructure::Organized, "");
    let source = rig.track(
        "Daft Punk/Discovery/1 - One More Time.flac",
        "Daft Punk",
        "Discovery",
        "One More Time",
        1,
        2001,
    );
    let lyrics = rig.at("Daft Punk/Discovery/1 - One More Time.lrc");
    std::fs::write(&lyrics, "[00:01.00]one").expect("written");
    rig.navidrome.own(&source);
    rig.lyrics_undo
        .record(&lyrics, LyricsUndoJournal::BESIDE, None, "l1")
        .expect("recorded");
    let target = rig.at("Daft Punk/Discovery/01 - One More Time.flac");
    let moved_lyrics = rig.at("Daft Punk/Discovery/01 - One More Time.lrc");

    rig.layout_run(TidyMode::Preview, false).await;
    let run = rig.run_state();
    completed(&run);
    assert_eq!((run.task, run.planned_file_moves), (TidyTask::FileLayout, 1));
    rig.layout_run(TidyMode::Apply, false).await;

    let run = rig.run_state();
    completed(&run);
    assert_eq!(run.moved_files, 1);
    assert!(exists(&target) && exists(&moved_lyrics));
    assert!(!exists(&source) && !exists(&lyrics));
    assert_eq!(rig.navidrome.mappings.lock()[0].local_path, target);
    assert_eq!(rig.lyrics_undo.read_all().expect("read")[0].path, moved_lyrics);
    assert_eq!(rig.worker.journal().last_run_task(), Some(TidyTask::FileLayout));

    rig.undo(false).await;
    let run = rig.run_state();
    completed(&run);
    assert_eq!(run.task, TidyTask::FileLayout);
    assert!(exists(&source) && exists(&lyrics), "{:?}", run.held_rows);
    assert!(!exists(&target) && !exists(&moved_lyrics));
    assert_eq!(rig.navidrome.mappings.lock()[0].local_path, source);
    assert_eq!(rig.lyrics_undo.read_all().expect("read")[0].path, lyrics);
    assert!(!rig.worker.journal().exists());
}

#[tokio::test]
async fn a_file_changed_after_the_scan_is_held() {
    let rig = Rig::new(audio_or_skip!("flac"));
    rig.set_layout(FolderStructure::Organized, "");
    let source = rig.track("A/X/1 - Song.flac", "A", "X", "Song", 1, 0);
    rig.layout_run(TidyMode::Preview, false).await;
    std::fs::File::options()
        .write(true)
        .open(&source)
        .and_then(|file| file.set_modified(SystemTime::now()))
        .expect("touched");

    rig.layout_run(TidyMode::Apply, false).await;

    let run = rig.run_state();
    completed(&run);
    assert!(exists(&source));
    assert!(held_saying(&run, "changed after Navidrome's last scan began"));
}

#[tokio::test]
async fn a_held_folder_holds_its_file_moves() {
    let rig = Rig::new(audio_or_skip!("flac"));
    rig.set_layout(
        FolderStructure::Custom,
        "{Artist}/{Album} ({Year})/{Artist} - {Title}",
    );
    let source = rig.track("A/X/01 - Song.flac", "A", "X", "Song", 1, 2001);
    rig.layout_run(TidyMode::Preview, false).await;
    let plan = rig.worker.store().plan().expect("kept").plan;
    assert_eq!(plan.moves.len(), 1);
    assert_eq!(plan.file_moves.len(), 1);
    // Something takes the folder's new name after the plan was checked again.
    let taken = rig.at("A/X (2001)");
    rig.on_list(move || {
        let _ = std::fs::create_dir_all(&taken);
    });

    rig.layout_run(TidyMode::Apply, false).await;

    let run = rig.run_state();
    completed(&run);
    assert!(exists(&source));
    assert!(held_saying(&run, "already there"));
    assert!(
        held_saying(&run, "Its folder could not be moved"),
        "{:?}",
        run.held_rows
    );
}

#[tokio::test]
async fn apply_never_creates_a_case_twin_parent() {
    let rig = Rig::new(audio_or_skip!("flac"));
    rig.set_layout(FolderStructure::Organized, "");
    let source = rig.track(
        "Daft Punk - One More Time.flac",
        "Daft Punk",
        "Discovery",
        "One More Time",
        1,
        0,
    );
    rig.layout_run(TidyMode::Preview, false).await;
    let twin = rig.at("daft punk");
    rig.on_list(move || {
        let _ = std::fs::create_dir_all(&twin);
    });

    rig.layout_run(TidyMode::Apply, false).await;

    let run = rig.run_state();
    completed(&run);
    assert!(exists(&source));
    assert!(!exists(&rig.at("daft punk/Discovery")));
    assert!(
        held_saying(
            &run,
            "daft punk is already there with other capitals or characters"
        ),
        "{:?}",
        run.held_rows
    );
}

#[tokio::test]
async fn a_file_only_run_triggers_a_scan_and_the_check() {
    let rig = Rig::new(audio_or_skip!("flac"));
    rig.set_layout(FolderStructure::Organized, "");
    let source = rig.track("A/X/1 - Song.flac", "A", "X", "Song", 1, 0);
    let entry = |path: &str, missing: bool| NavidromeSongEntry {
        full_path: path.to_string(),
        album_id: None,
        artist: None,
        title: None,
        album: None,
        lyrics: None,
        id: Some("nd-1".to_string()),
        duration: None,
        explicit_status: None,
        missing,
    };
    rig.songs.lock().insert(source.clone(), entry(&source, false));
    {
        let (songs, source) = (rig.songs.clone(), source.clone());
        *rig.navidrome.on_start.lock() = Some(Box::new(move || {
            if !Path::new(&source).exists() {
                songs.lock().insert(source.clone(), entry(&source, true));
            }
        }));
    }
    rig.layout_run(TidyMode::Preview, false).await;
    let starts = rig.navidrome.starts();

    rig.layout_run(TidyMode::Apply, false).await;

    let run = rig.run_state();
    completed(&run);
    assert_eq!((run.moved, run.moved_files), (0, 1));
    assert!(rig.navidrome.starts() > starts);
    assert!(
        held_saying(&run, "did not match this moved song"),
        "{:?}",
        run.held_rows
    );
}

#[tokio::test]
async fn playlists_follow_a_folder_and_a_file_move() {
    let rig = Rig::new(audio_or_skip!("flac"));
    rig.set_layout(
        FolderStructure::Custom,
        "{Artist}/{Album} ({Year})/{Artist} - {Title}",
    );
    rig.track("A/X/01 - Song.flac", "A", "X", "Song", 1, 2001);
    let playlist = rig.at("Mixes/road.m3u8");
    std::fs::create_dir_all(rig.at("Mixes")).expect("made");
    std::fs::write(&playlist, "#EXTM3U\n../A/X/01 - Song.flac\n").expect("written");

    rig.layout_run(TidyMode::Preview, false).await;
    rig.layout_run(TidyMode::Apply, false).await;

    completed(&rig.run_state());
    assert_eq!(
        std::fs::read_to_string(&playlist).expect("read"),
        "#EXTM3U\n../A/X (2001)/A - Song.flac\n"
    );
}

#[tokio::test]
async fn already_done_still_follows() {
    let calm = ScanWaitTiming {
        poll: Duration::from_millis(1),
        timeout: Duration::from_millis(200),
    };
    let rig = Rig::with(audio_or_skip!("flac"), None, move |worker| {
        worker.with_timing(calm)
    });
    rig.set_layout(FolderStructure::Organized, "");
    let source = rig.track("A/X/1 - Song.flac", "A", "X", "Song", 1, 0);
    let target = rig.at("A/X/01 - Song.flac");
    rig.navidrome.own(&source);
    rig.layout_run(TidyMode::Preview, false).await;
    // The apply stops before it moves anything; the file is then found moved, its records not.
    rig.navidrome.scanning.store(true, Ordering::SeqCst);
    rig.layout_run(TidyMode::Apply, false).await;
    assert_eq!(rig.run_state().status, TidyStatus::Failed);
    std::fs::rename(&source, &target).expect("moved");
    rig.navidrome.scanning.store(false, Ordering::SeqCst);

    rig.layout_run(TidyMode::Apply, true).await;

    completed(&rig.run_state());
    assert_eq!(rig.navidrome.mappings.lock()[0].local_path, target);
}

#[tokio::test]
async fn already_done_still_follows_for_the_artists_task() {
    let rig = Rig::new(audio_or_skip!("flac"));
    rig.navidrome.scans_finish.store(false, Ordering::SeqCst);
    let source = rig.collab();
    rig.navidrome.own(&source);
    rig.preview(WHOLE).await;
    rig.apply(WHOLE, &[]).await;
    assert_eq!(rig.run_state().status, TidyStatus::Failed);
    // The folder is found moved, its records not.
    std::fs::create_dir_all(rig.at("Bizarrap")).expect("made");
    std::fs::rename(rig.at("Bizarrap, Rauw Alejandro/Album"), rig.at("Bizarrap/Album")).expect("moved");
    rig.navidrome.scans_finish.store(true, Ordering::SeqCst);

    rig.run(TidyRequest {
        mode: TidyMode::Apply,
        scope: WHOLE,
        resume: true,
        ..TidyRequest::default()
    })
    .await;

    completed(&rig.run_state());
    assert_eq!(
        rig.navidrome.mappings.lock()[0].local_path,
        rig.at("Bizarrap/Album/01 - Session.flac")
    );
}

#[tokio::test]
async fn a_changed_row_is_held_and_others_go() {
    let rig = Rig::new(audio_or_skip!("flac"));
    rig.set_layout(FolderStructure::Organized, "");
    let first = rig.track("A/X/1 - Song.flac", "A", "X", "Song", 1, 0);
    let second = rig.track("B/Y/1 - Other.flac", "B", "Y", "Other", 1, 0);
    rig.layout_run(TidyMode::Preview, false).await;
    // A new song in B's folder after the preview changes that row only.
    rig.track("B/Y/2 - New.flac", "B", "Y", "New", 2, 0);

    rig.layout_run(TidyMode::Apply, false).await;

    let run = rig.run_state();
    completed(&run);
    assert!(!exists(&first) && exists(&rig.at("A/X/01 - Song.flac")));
    assert!(exists(&second));
    assert!(held_saying(
        &run,
        "Its songs changed since the preview. Preview again."
    ));
}

#[tokio::test]
async fn file_layout_apply_is_refused_for_another_task_or_a_changed_layout() {
    let rig = Rig::new(audio_or_skip!("flac"));
    rig.set_layout(FolderStructure::Organized, "");
    let source = rig.track("A/X/1 - Song.flac", "A", "X", "Song", 1, 0);
    rig.layout_run(TidyMode::Preview, false).await;
    // The Artists task never applies a File layout preview.
    rig.apply(WHOLE, &[]).await;
    assert!(
        rig.run_state()
            .reason
            .is_some_and(|why| why.contains("Preview first"))
    );

    rig.layout_run(TidyMode::Preview, false).await;
    rig.set_layout(FolderStructure::Flat, "");
    rig.layout_run(TidyMode::Apply, false).await;
    let run = rig.run_state();
    assert_eq!(run.status, TidyStatus::Failed);
    assert_eq!(
        run.reason.as_deref(),
        Some("The File layout changed since the preview. Preview again.")
    );
    assert!(exists(&source));

    // A layout with a problem is never planned with.
    rig.set_layout(FolderStructure::Custom, "{Artst}/{Title}");
    rig.layout_run(TidyMode::Preview, false).await;
    let run = rig.run_state();
    assert_eq!(run.status, TidyStatus::Failed);
    assert!(
        run.reason
            .is_some_and(|why| why.starts_with("The Albums layout has a problem"))
    );
}

// ---- Lidarr ---------------------------------------------------------------------------------

mod lidarr {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    async fn answers(server: &MockServer, root: &str, artists: serde_json::Value, files: serde_json::Value) {
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/rootfolder"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([{ "id": 1, "path": root }])),
            )
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/artist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(artists))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/trackfile"))
            .and(query_param("artistId", "7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(files))
            .mount(server)
            .await;
    }

    fn artist(root: &str) -> serde_json::Value {
        serde_json::json!([{
            "id": 7,
            "foreignArtistId": "mbid",
            "artistName": "Daft Punk",
            "path": format!("{root}/Daft Punk"),
            "monitored": true,
        }])
    }

    fn track_file(path: &str) -> serde_json::Value {
        serde_json::json!([{ "id": 1, "artistId": 7, "albumId": 1, "path": path, "size": 10 }])
    }

    #[tokio::test]
    async fn file_layout_preview_fails_when_lidarr_does_not_answer() {
        let rig = Rig::new(audio_or_skip!("flac"));
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        rig.set_layout(FolderStructure::Organized, "");
        rig.set_lidarr(&server.uri());
        rig.track("A/X/1 - Song.flac", "A", "X", "Song", 1, 0);

        rig.layout_run(TidyMode::Preview, false).await;

        let run = rig.run_state();
        assert_eq!(run.status, TidyStatus::Failed);
        assert_eq!(
            run.reason.as_deref(),
            Some(
                "Lidarr did not answer, so Octo cannot tell which songs Lidarr keeps. Preview again once it answers."
            )
        );
    }

    #[tokio::test]
    async fn a_move_into_a_lidarr_artist_added_since_the_preview_is_held() {
        let rig = Rig::new(audio_or_skip!("flac"));
        let server = MockServer::start().await;
        answers(&server, &rig.root, serde_json::json!([]), serde_json::json!([])).await;
        rig.set_layout(FolderStructure::Organized, "");
        rig.set_lidarr(&server.uri());
        let source = rig.track(
            "Daft Punk - One More Time.flac",
            "Daft Punk",
            "Discovery",
            "One More Time",
            1,
            0,
        );
        rig.layout_run(TidyMode::Preview, false).await;
        completed(&rig.run_state());
        // Lidarr adds the artist, with a song in the folder the move goes to.
        let lidarr_song = format!("{}/Daft Punk/Discovery/02 - Aerodynamic.flac", rig.root);
        answers(&server, &rig.root, artist(&rig.root), track_file(&lidarr_song)).await;

        rig.layout_run(TidyMode::Apply, false).await;

        let run = rig.run_state();
        completed(&run);
        assert!(exists(&source));
        assert!(
            held_saying(&run, "Lidarr keeps a song here now."),
            "{:?}",
            run.held_rows
        );
    }

    #[tokio::test]
    async fn resume_asks_lidarr_for_the_remaining_rows() {
        let calm = ScanWaitTiming {
            poll: Duration::from_millis(1),
            timeout: Duration::from_millis(200),
        };
        let rig = Rig::with(audio_or_skip!("flac"), None, move |worker| {
            worker.with_timing(calm)
        });
        let server = MockServer::start().await;
        answers(&server, &rig.root, artist(&rig.root), serde_json::json!([])).await;
        rig.set_layout(FolderStructure::Organized, "");
        rig.set_lidarr(&server.uri());
        let source = rig.track(
            "Daft Punk/Discovery/1 - One More Time.flac",
            "Daft Punk",
            "Discovery",
            "One More Time",
            1,
            0,
        );
        rig.layout_run(TidyMode::Preview, false).await;
        rig.navidrome.scanning.store(true, Ordering::SeqCst);
        rig.layout_run(TidyMode::Apply, false).await;
        assert_eq!(rig.run_state().status, TidyStatus::Failed);
        // Lidarr took the song over meanwhile.
        answers(&server, &rig.root, artist(&rig.root), track_file(&source)).await;
        rig.navidrome.scanning.store(false, Ordering::SeqCst);

        rig.layout_run(TidyMode::Apply, true).await;

        let run = rig.run_state();
        completed(&run);
        assert!(exists(&source));
        assert!(held_saying(&run, "Lidarr keeps a song here now."));
        let asked: Vec<String> = server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(|request| request.url.path().to_string())
            .collect();
        assert!(asked.iter().any(|path| path == "/api/v1/trackfile"));
    }

    #[tokio::test]
    async fn apply_asks_lidarr_again_while_paused() {
        let gate = LibraryWriteGate::new();
        let shared = gate.clone();
        let rig = Rig::with(audio_or_skip!("flac"), None, move |worker| {
            worker.with_write_gate(shared)
        });
        let server = MockServer::start().await;
        answers(&server, &rig.root, serde_json::json!([]), serde_json::json!([])).await;
        rig.set_layout(FolderStructure::Organized, "");
        rig.set_lidarr(&server.uri());
        rig.track("A/X/1 - Song.flac", "A", "X", "Song", 1, 0);
        rig.layout_run(TidyMode::Preview, false).await;
        // Lidarr answers slowly at apply, so a placement can be let in meanwhile.
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/rootfolder"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([{ "id": 1, "path": rig.root }]))
                    .set_delay(Duration::from_millis(600)),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/artist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;

        let placed = async {
            while rig.run_state().phase != "Asking Lidarr" {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            tokio::time::timeout(Duration::from_millis(400), gate.write())
                .await
                .is_ok()
        };
        let (placed, ()) = tokio::join!(placed, rig.layout_run(TidyMode::Apply, false));

        assert!(placed, "a download waited while the tidy asked Lidarr");
        completed(&rig.run_state());
    }
}

// ---- a loose album of two discs -----------------------------------------------------------------

impl Rig {
    /// A two-disc album with every song loose in its folder, as placement leaves a disc 2 song
    /// that comes after its album's loose disc 1: two songs of disc 1, one of disc 2, every file
    /// Octo's.
    fn loose_two_discs(&self) -> Vec<String> {
        [
            ("01 - One.flac", "One", 1, 1),
            ("02 - Two.flac", "Two", 2, 1),
            ("01 - Three.flac", "Three", 1, 2),
        ]
        .into_iter()
        .map(|(name, title, track, disc)| {
            let path = self.track(
                &format!("Artist/Album/{name}"),
                "Artist",
                "Album",
                title,
                track,
                0,
            );
            let mut file = TagFile::open(&path).expect("opens");
            file.set_disc(disc);
            file.set_disc_count(2);
            file.save().expect("saved");
            age(&path);
            self.navidrome.own(&path);
            path
        })
        .collect()
    }

    fn records(&self) -> Vec<String> {
        self.navidrome
            .mappings
            .lock()
            .iter()
            .map(|mapping| mapping.local_path.clone())
            .collect()
    }
}

/// The disc folders the preview offers a loose two-disc album, applied by the person: every song
/// moves into its disc's folder, with its lyrics, Octo's records and the playlists naming it, and
/// Undo puts every song back.
#[tokio::test]
async fn applied_disc_folders_move_every_song_and_undo_puts_them_back() {
    let rig = Rig::new(audio_or_skip!("flac"));
    rig.set_layout(
        FolderStructure::Custom,
        "{Artist}/{Album}/Disc {Disc}/{Track:00} - {Title}",
    );
    let loose = rig.loose_two_discs();
    let filed = [
        rig.at("Artist/Album/Disc 1/01 - One.flac"),
        rig.at("Artist/Album/Disc 1/02 - Two.flac"),
        rig.at("Artist/Album/Disc 2/01 - Three.flac"),
    ];
    let lyrics = rig.at("Artist/Album/01 - One.lrc");
    std::fs::write(&lyrics, "[00:01.00]one").expect("written");
    let playlist = rig.at("Mixes/road.m3u");
    std::fs::create_dir_all(rig.at("Mixes")).expect("made");
    std::fs::write(&playlist, "../Artist/Album/01 - Three.flac\n").expect("written");

    rig.layout_run(TidyMode::Preview, false).await;
    let run = rig.run_state();
    completed(&run);
    assert_eq!(run.planned_file_moves, 3);
    rig.layout_run(TidyMode::Apply, false).await;

    let run = rig.run_state();
    completed(&run);
    assert_eq!(run.moved_files, 3);
    for (was, now) in loose.iter().zip(&filed) {
        assert!(exists(now) && !exists(was), "{now} {:?}", run.held_rows);
    }
    assert!(exists(&rig.at("Artist/Album/Disc 1/01 - One.lrc")) && !exists(&lyrics));
    assert_eq!(rig.records(), filed);
    #[cfg(unix)]
    assert_eq!(
        std::fs::read_to_string(&playlist).expect("read"),
        "../Artist/Album/Disc 2/01 - Three.flac\n"
    );

    rig.undo(false).await;

    let run = rig.run_state();
    completed(&run);
    for (was, now) in loose.iter().zip(&filed) {
        assert!(exists(was) && !exists(now), "{was} {:?}", run.held_rows);
    }
    assert!(exists(&lyrics));
    assert_eq!(rig.records(), loose);
    assert_eq!(
        std::fs::read_to_string(&playlist).expect("read"),
        "../Artist/Album/01 - Three.flac\n"
    );
    assert!(!rig.worker.journal().exists());
}

/// An Undo the page showed for one run puts nothing back once the last run is another, and the
/// run it was shown for is put back when it is still the last.
#[tokio::test]
async fn an_undo_shown_for_another_run_puts_nothing_back() {
    let rig = Rig::new(audio_or_skip!("flac"));
    rig.set_layout(FolderStructure::Organized, "");
    let source = rig.track("A/X/1 - Song.flac", "A", "X", "Song", 1, 0);
    let target = rig.at("A/X/01 - Song.flac");
    rig.layout_run(TidyMode::Preview, false).await;
    rig.layout_run(TidyMode::Apply, false).await;
    completed(&rig.run_state());
    let last = rig.worker.journal().last_run().expect("journaled");
    let undo = |run: &str| TidyRequest {
        mode: TidyMode::Undo,
        run: Some(run.to_string()),
        ..TidyRequest::default()
    };

    rig.run(undo("an-older-run")).await;

    let run = rig.run_state();
    assert_eq!(run.status, TidyStatus::Failed);
    assert_eq!(
        run.reason.as_deref(),
        Some(
            "The last tidy changed since this page showed Undo, so nothing was put back. Look at what Undo puts back now."
        )
    );
    assert!(exists(&target) && !exists(&source));

    rig.run(undo(&last)).await;
    completed(&rig.run_state());
    assert!(exists(&source) && !exists(&target));
}

// ---- batched records ------------------------------------------------------------------------

#[tokio::test]
async fn one_save_per_group() {
    let rig = Rig::new(audio_or_skip!("flac"));
    rig.set_layout(FolderStructure::Organized, "");
    for (name, title, track) in [("1 - One.flac", "One", 1), ("2 - Two.flac", "Two", 2)] {
        let path = rig.track(&format!("A/X/{name}"), "A", "X", title, track, 0);
        rig.history.record(DownloadHistoryEntry {
            path,
            ..DownloadHistoryEntry::default()
        });
    }
    rig.layout_run(TidyMode::Preview, false).await;
    let saves = rig.history.saves();

    rig.layout_run(TidyMode::Apply, false).await;

    let run = rig.run_state();
    completed(&run);
    assert_eq!(run.moved_files, 2);
    assert_eq!(rig.history.saves(), saves + 1, "one folder, one save");
    let paths: Vec<String> = rig
        .history
        .get_recent(10)
        .into_iter()
        .map(|entry| entry.path)
        .collect();
    assert!(paths.contains(&rig.at("A/X/01 - One.flac")) && paths.contains(&rig.at("A/X/02 - Two.flac")));
}

// ---- the family pool --------------------------------------------------------------------------

/// Members' songs: the pool's files are filed by the layout too, and each member's library
/// joins the check after the moves.
#[cfg(unix)]
#[tokio::test]
async fn a_pool_move_is_checked_in_each_member_library() {
    use crate::services::family::test_support::World;

    let flac = audio_or_skip!("flac");
    let world = World::new(None);
    let mut settings = (*world.settings.current()).clone();
    settings.subsonic.folder_structure = FolderStructure::Organized;
    world.settings.set(settings);
    let old = world.pool_file("Artist/Album/1 - Song.flac", 1);
    std::fs::write(&old, &flac).expect("written");
    {
        let mut file = TagFile::open(&old).expect("opens");
        file.set_performers(&["Artist".to_string()]);
        file.set_album(Some("Album"));
        file.set_title(Some("Song"));
        file.set_track(1);
        file.save().expect("saved");
    }
    age(&old);
    world.pool.link(&old, "alex").await.expect("linked");
    let members = world.members();
    let (was, now) = (
        format!("{members}/alex/Artist/Album/1 - Song.flac"),
        format!("{members}/alex/Artist/Album/01 - Song.flac"),
    );
    let new = format!("{}/Artist/Album/01 - Song.flac", world.pool.pool_root_path());

    let navidrome = Arc::new(FakeNavidrome::new());
    let entry = |path: &str, id: &str, missing: bool| NavidromeSongEntry {
        full_path: path.to_string(),
        album_id: None,
        artist: None,
        title: None,
        album: None,
        lyrics: None,
        id: Some(id.to_string()),
        duration: None,
        explicit_status: None,
        missing,
    };
    let songs: Arc<Mutex<HashMap<String, NavidromeSongEntry>>> = Arc::default();
    songs.lock().insert(was.clone(), entry(&was, "nd-1", false));
    {
        // Navidrome sees the member's song gone from its old place and a new one at the new.
        let (songs, was, now) = (songs.clone(), was.clone(), now.clone());
        *navidrome.on_start.lock() = Some(Box::new(move || {
            if !Path::new(&was).exists() {
                let mut songs = songs.lock();
                songs.insert(was.clone(), entry(&was, "nd-1", true));
                songs.insert(now.clone(), entry(&now, "nd-2", false));
            }
        }));
    }
    let listed = songs.clone();
    let list_songs: ListSongs = Arc::new(move || {
        let songs = listed.lock().clone();
        Box::pin(async move { songs })
    });
    let worker = TidyWorker::new(
        Arc::new(TidyStore::new(None, None)),
        Arc::new(TidyJournal::new(None)),
        world.settings.clone(),
        TidyServices {
            library: Some(navidrome.clone()),
            list_songs: Some(list_songs),
            family_pool: Some(world.pool.clone()),
            ..TidyServices::default()
        },
    )
    .with_timing(quick());
    let run = |mode| TidyRequest {
        mode,
        scope: WHOLE,
        task: TidyTask::FileLayout,
        ..TidyRequest::default()
    };

    worker
        .run_once(run(TidyMode::Preview), &CancellationToken::new())
        .await;
    let plan = worker.store().plan().expect("kept").plan;
    assert_eq!(plan.pool_moves.len(), 1, "{:?}", plan.rows);
    assert!(plan.rows.iter().any(|row| row.path == "pool:Artist/Album"));
    worker
        .run_once(run(TidyMode::Apply), &CancellationToken::new())
        .await;

    let state = worker.current();
    completed(&state);
    assert_eq!(state.moved_files, 1);
    assert!(Path::new(&new).is_file() && !Path::new(&old).exists());
    assert!(std::fs::symlink_metadata(&now).is_ok(), "alex's link moved");
    assert!(
        state
            .held_rows
            .iter()
            .any(|row| row.evidence.contains("did not match this moved song")),
        "{:?}",
        state.held_rows
    );
}

/// The pool's files move while the job holds the gate whole: a download, a member's placement or
/// a lyrics write that takes its share meanwhile never sees a song move under it. A file another
/// pool change is working on (here one that then waits for the gate, as a promotion does) makes
/// the folder wait for it, with the gate let go, and the folder moves once it is free; the
/// lyrics jobs waiting are told where the songs went.
#[tokio::test]
async fn pool_moves_wait_for_a_busy_file_and_never_move_under_a_share() {
    use crate::services::family::test_support::World;

    let flac = audio_or_skip!("flac");
    let world = World::new(None);
    let mut settings = (*world.settings.current()).clone();
    settings.subsonic.folder_structure = FolderStructure::Organized;
    world.settings.set(settings);
    let pool_root = world.pool.pool_root_path();
    let mut olds = Vec::new();
    for (name, title, track) in [("1 - One.flac", "One", 1), ("2 - Two.flac", "Two", 2)] {
        let old = world.pool_file(&format!("Artist/Album/{name}"), 1);
        std::fs::write(&old, &flac).expect("written");
        let mut file = TagFile::open(&old).expect("opens");
        file.set_performers(&["Artist".to_string()]);
        file.set_album(Some("Album"));
        file.set_title(Some(title));
        file.set_track(track);
        file.save().expect("saved");
        age(&old);
        olds.push(old);
    }
    let news = [
        format!("{pool_root}/Artist/Album/01 - One.flac"),
        format!("{pool_root}/Artist/Album/02 - Two.flac"),
    ];
    let gate = LibraryWriteGate::new();
    world.pool.set_write_gate(gate.clone());
    let remap = LyricsPathRemap::new();
    let waiting = remap.hold_for_test();
    let worker = TidyWorker::new(
        Arc::new(TidyStore::new(None, None)),
        Arc::new(TidyJournal::new(None)),
        world.settings.clone(),
        TidyServices {
            library: Some(Arc::new(FakeNavidrome::new())),
            family_pool: Some(world.pool.clone()),
            lyrics_remap: Some(remap.clone()),
            ..TidyServices::default()
        },
    )
    .with_timing(quick())
    .with_write_gate(gate.clone());
    worker.set_pool_retry(Duration::from_millis(10));
    let run = |mode| TidyRequest {
        mode,
        scope: WHOLE,
        task: TidyTask::FileLayout,
        ..TidyRequest::default()
    };
    worker
        .run_once(run(TidyMode::Preview), &CancellationToken::new())
        .await;
    assert_eq!(worker.store().plan().expect("kept").plan.pool_moves.len(), 2);
    let on_disk = || -> Vec<bool> {
        olds.iter()
            .chain(&news)
            .map(|path| Path::new(path).exists())
            .collect()
    };
    // Another change holds the second song, as a promotion holds its file before the gate.
    let busy = world.pool.try_hold(&[&olds[1]]).expect("free");

    let other_change = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while worker.current().phase != "Moving members' songs" {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the apply reaches the members' songs");
        let share = tokio::time::timeout(Duration::from_secs(5), gate.write())
            .await
            .expect("the job lets the gate go while the song is busy");
        let before = on_disk();
        drop(busy);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let after = on_disk();
        drop(share);
        (before, after)
    };
    let token = CancellationToken::new();
    let applied = tokio::time::timeout(
        Duration::from_secs(10),
        worker.run_once(run(TidyMode::Apply), &token),
    );
    let ((before, after), applied) = tokio::join!(other_change, applied);

    applied.expect("the apply never waits on the busy song while it holds the gate");
    assert_eq!(before, after, "nothing moved while the share was held");
    let state = worker.current();
    completed(&state);
    assert_eq!(state.moved_files, 2, "{:?}", state.held_rows);
    assert!(news.iter().all(|path| Path::new(path).is_file()));
    assert!(olds.iter().all(|path| !Path::new(path).exists()));
    assert_eq!(remap.map(&olds[0]), news[0]);
    assert_eq!(remap.map(&olds[1]), news[1]);
    remap.release_for_test(waiting);
}
