//! Recently removed: the trash and Put back on the dashboard, behind a Navidrome admin's own
//! sign-in and the library actions gates. The download log and Find songs are tested in
//! `admin_downloads_tests.rs`.

use std::path::{Path, PathBuf};

use axum::http::{Method, StatusCode};
use chrono::{TimeDelta, Utc};
use octo_core::settings::{AppSettings, LibraryAction, LibraryActionSettings};
use octo_media::tags::TagFile;

use crate::app::{AppState, TestServices};
use crate::controllers::admin::test_app::{Reply, app, send};
use crate::http::pipeline::App;
use crate::services::library::library_action_journal::{DeniedSource, action_name};
use crate::services::library::library_action_quarantine::QuarantinedSidecar;
use crate::services::library::library_action_test_support::slash_root;
use crate::services::library::{
    LibraryActionEntry, LibraryActionJournal, LibraryActionState, NoticeQueue, NoticeState, PathSource,
    QuarantineManifest,
};

/// The dashboard over a temporary music folder, library actions on (or in dry run), and the
/// admin on the allowed list or not.
struct Dashboard {
    state: AppState,
    app: App,
    root: PathBuf,
    _dir: tempfile::TempDir,
}

impl Dashboard {
    fn new(allowed: bool, dry_run: bool) -> Dashboard {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("music");
        std::fs::create_dir_all(&root).expect("the music folder");
        let settings = AppSettings {
            library_actions: LibraryActionSettings {
                enabled: true,
                dry_run,
                allowed_users: vec![if allowed { "admin" } else { "someone-else" }.to_string()],
                quarantine_retention_days: 30,
                ..Default::default()
            },
            ..Default::default()
        };
        let state = AppState::for_tests_with(
            settings,
            TestServices {
                config_dir: Some(dir.path().join("config")),
                raw_settings: vec![("Library:DownloadPath".into(), slash_root(&root))],
                ..Default::default()
            },
        );
        Dashboard {
            app: app(state.clone()),
            state,
            root,
            _dir: dir,
        }
    }

    async fn call(&self, method: Method, path: &str, body: Option<&str>) -> Reply {
        let token = self.state.browse_sessions.create("admin");
        send(
            &self.app,
            method,
            path,
            &[("X-Octo-Admin", "1"), ("X-Octo-Browse-Token", token.as_str())],
            body,
        )
        .await
    }
}

/// A song alice removed two days ago: its file (and lyrics) in the trash with the manifest, and
/// the removal's journal entry.
fn trashed(root: &Path, id: &str, with_lyrics: bool) -> (LibraryActionEntry, PathBuf, PathBuf) {
    let original = root
        .join("Daft Punk")
        .join("Discovery")
        .join(format!("{id}.flac"));
    let trash_dir = root.join(".octo-trash").join("2026-10-04");
    std::fs::create_dir_all(&trash_dir).expect("the trash");
    let trashed = trash_dir.join(format!("{id}.flac"));
    std::fs::write(&trashed, "audio").expect("written");
    let mut sidecars = Vec::new();
    if with_lyrics {
        let lrc = trash_dir.join(format!("{id}.lrc"));
        std::fs::write(&lrc, "[00:01.00]words").expect("written");
        sidecars.push(QuarantinedSidecar {
            original_path: slash_root(&original.with_extension("lrc")),
            quarantine_path: slash_root(&lrc),
        });
    }
    let manifest = QuarantineManifest {
        original_path: slash_root(&original),
        navidrome_id: id.to_string(),
        action: "Delete".to_string(),
        username: "alice".to_string(),
        at_utc: Utc::now(),
        sidecars: Some(sidecars),
        mappings: None,
    };
    let mut manifest_path = trashed.clone().into_os_string();
    manifest_path.push(".octo-action.json");
    std::fs::write(&manifest_path, octo_core::json::to_string(&manifest)).expect("written");
    let entry = LibraryActionEntry {
        key: LibraryActionJournal::make_key(LibraryAction::Delete, id, "1:2"),
        action: LibraryAction::Delete,
        navidrome_id: id.to_string(),
        username: "alice".to_string(),
        title: "Digital Love".to_string(),
        artist: "Daft Punk".to_string(),
        album: "Discovery".to_string(),
        source_path: Some(slash_root(&original)),
        quarantine_path: Some(slash_root(&trashed)),
        resolution: Some(PathSource::NativeApi),
        state: LibraryActionState::Applied,
        detail: Some("Removed.".to_string()),
        dry_run: false,
        at_utc: Utc::now() - TimeDelta::days(2),
        history_kept: None,
        revealed_path: None,
        copy: false,
        download_allowed: false,
        denied_source: None,
    };
    (entry, original, trashed)
}

#[tokio::test]
async fn recently_removed_lists_the_trash_and_put_back_restores_the_song_with_its_lyrics() {
    let dash = Dashboard::new(true, false);
    let (entry, original, trashed) = trashed(&dash.root, "nd-1", true);
    dash.state.library_action_journal.record(entry.clone());

    let list = dash.call(Method::GET, "/api/admin/trash", None).await.json();
    assert_eq!(list["keepDays"], 30);
    let songs = list["songs"].as_array().expect("songs");
    assert_eq!(songs.len(), 1);
    let song = &songs[0];
    assert_eq!(song["id"], "nd-1");
    assert_eq!(song["removedBy"], "alice");
    assert_eq!(song["sidecars"], serde_json::json!(["nd-1.lrc"]));
    assert_eq!(song["blocksDownloads"], true);
    assert_eq!(song["key"], entry.key.as_str());
    assert!(song["goneAt"].is_string());

    let restored = dash
        .call(Method::POST, "/api/admin/trash/restore", Some(r#"{"id":"nd-1"}"#))
        .await
        .json();
    assert_eq!(restored["state"], "applied");
    assert!(original.exists());
    assert!(original.with_extension("lrc").exists());
    assert!(!trashed.exists());
    let empty = dash.call(Method::GET, "/api/admin/trash", None).await.json();
    assert_eq!(empty["songs"].as_array().map(Vec::len), Some(0));
    let journaled = dash
        .state
        .library_action_journal
        .recent(10)
        .into_iter()
        .find(|e| e.key == entry.key)
        .expect("the removal");
    assert!(journaled.detail.unwrap_or_default().contains("admin"));
}

#[tokio::test]
async fn an_admin_off_the_allowed_list_sees_why_and_changes_nothing() {
    let dash = Dashboard::new(false, false);
    let (entry, original, trashed) = trashed(&dash.root, "nd-2", false);
    dash.state.library_action_journal.record(entry);

    let list = dash.call(Method::GET, "/api/admin/trash", None).await;
    assert_eq!(list.status, StatusCode::FORBIDDEN);
    assert!(list.text().contains("allowed list"));
    let restore = dash
        .call(Method::POST, "/api/admin/trash/restore", Some(r#"{"id":"nd-2"}"#))
        .await;
    assert_eq!(restore.status, StatusCode::FORBIDDEN);
    assert!(trashed.exists());
    assert!(!original.exists());
}

#[tokio::test]
async fn while_dry_run_is_on_put_back_only_says_what_it_would_do() {
    let dash = Dashboard::new(true, true);
    let (entry, original, trashed) = trashed(&dash.root, "nd-3", false);
    dash.state.library_action_journal.record(entry);

    let restored = dash
        .call(Method::POST, "/api/admin/trash/restore", Some(r#"{"id":"nd-3"}"#))
        .await
        .json();
    assert_eq!(restored["state"], "rehearsed");
    assert!(trashed.exists());
    assert!(!original.exists());
}

// ---- Put back for a replacement --------------------------------------------------------------

/// An MP3 of twenty silent MPEG-1 Layer III frames, titled and on an album.
fn mp3_titled(path: &Path, title: &str, album: &str) {
    let mut bytes = vec![0u8; 417 * 20];
    for frame in 0..20 {
        bytes[frame * 417..frame * 417 + 4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("folders");
    }
    std::fs::write(path, bytes).expect("written");
    let mut file = TagFile::open(path).expect("an MP3");
    file.set_title(Some(title));
    file.set_album(Some(album));
    file.set_album_artists(&["Massive Attack".to_string()]);
    file.save().expect("saved");
}

fn title_of(path: &Path) -> Option<String> {
    TagFile::open(path).ok().and_then(|file| file.title())
}

/// A song another file replaced: the old file (`old_title`) in the trash with its manifest, the
/// new one (`new_title`) at the song's path, and the replacement's journal entry.
fn replaced(
    root: &Path,
    id: &str,
    old_title: &str,
    new_title: &str,
) -> (LibraryActionEntry, PathBuf, PathBuf) {
    replaced_by(root, id, LibraryAction::BetterQuality, old_title, new_title)
}

/// [`replaced`] through `action`.
fn replaced_by(
    root: &Path,
    id: &str,
    action: LibraryAction,
    old_title: &str,
    new_title: &str,
) -> (LibraryActionEntry, PathBuf, PathBuf) {
    let song = root
        .join("Massive Attack")
        .join("Mezzanine")
        .join(format!("{id}.mp3"));
    let trash_dir = root.join(".octo-trash").join("2026-10-10");
    let old = trash_dir
        .join("Massive Attack")
        .join("Mezzanine")
        .join(format!("{id}.mp3"));
    mp3_titled(&old, old_title, "Mezzanine");
    mp3_titled(&song, new_title, "Mezzanine");
    let manifest = QuarantineManifest {
        original_path: slash_root(&song),
        navidrome_id: id.to_string(),
        action: action_name(action).to_string(),
        username: "alice".to_string(),
        at_utc: Utc::now(),
        sidecars: None,
        mappings: None,
    };
    let mut manifest_path = old.clone().into_os_string();
    manifest_path.push(".octo-action.json");
    std::fs::write(&manifest_path, octo_core::json::to_string(&manifest)).expect("written");
    let entry = LibraryActionEntry {
        key: LibraryActionJournal::make_key(action, id, "1:2"),
        action,
        navidrome_id: id.to_string(),
        username: "alice".to_string(),
        title: "Teardrop".to_string(),
        artist: "Massive Attack".to_string(),
        album: "Mezzanine".to_string(),
        source_path: Some(slash_root(&song)),
        quarantine_path: Some(slash_root(&old)),
        resolution: Some(PathSource::NativeApi),
        state: LibraryActionState::Applied,
        detail: Some("Replaced with nd.mp3.".to_string()),
        dry_run: false,
        at_utc: Utc::now() - TimeDelta::hours(1),
        history_kept: Some(true),
        revealed_path: Some(slash_root(&song)),
        copy: false,
        download_allowed: false,
        denied_source: None,
    };
    (entry, song, old)
}

async fn put_back(dash: &Dashboard, id: &str) -> serde_json::Value {
    dash.call(
        Method::POST,
        "/api/admin/trash/restore",
        Some(&format!(r#"{{"id":"{id}"}}"#)),
    )
    .await
    .json()
}

#[tokio::test]
async fn a_replacement_is_listed_with_both_files_and_put_back_swaps_them_again_and_again() {
    let dash = Dashboard::new(true, false);
    let (entry, song, old) = replaced(&dash.root, "nd-7", "Teardrop", "Teardrop");
    dash.state.library_action_journal.record(entry);

    let list = dash.call(Method::GET, "/api/admin/trash", None).await.json();
    let songs = list["songs"].as_array().expect("songs");
    assert_eq!(songs.len(), 1, "{list}");
    assert_eq!(songs[0]["kind"], "replaced");
    assert_eq!(songs[0]["was"], "MP3 128 kbps");
    assert_eq!(songs[0]["now"], "MP3 128 kbps");
    assert_eq!(songs[0]["blocksDownloads"], false);
    let old_bytes = std::fs::read(&old).expect("the old file");

    let first = put_back(&dash, "nd-7").await;
    assert_eq!(first["state"], "applied", "{first}");
    assert_eq!(
        first["detail"],
        "Put back your old copy. The one that replaced it is in Recently removed."
    );
    assert_eq!(
        std::fs::read(&song).expect("the song"),
        old_bytes,
        "the old file is back"
    );
    assert!(!old.exists());
    // The newer file is in the trash now, so the swap can be made again.
    let again = dash.call(Method::GET, "/api/admin/trash", None).await.json();
    let songs = again["songs"].as_array().expect("songs");
    assert_eq!(songs.len(), 1, "{again}");
    assert_eq!(songs[0]["kind"], "replaced");

    let second = put_back(&dash, "nd-7").await;
    assert_eq!(second["state"], "applied", "{second}");
    assert!(song.exists());
    let third = dash.call(Method::GET, "/api/admin/trash", None).await.json();
    assert_eq!(third["songs"].as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn putting_back_after_a_forced_replace_brings_back_the_old_names_at_the_same_path() {
    let dash = Dashboard::new(true, false);
    // The live take was replaced anyway and the new file renamed to the plain song.
    let (entry, song, _old) = replaced(&dash.root, "nd-8", "Teardrop (Live)", "Teardrop");
    dash.state.library_action_journal.record(entry);

    let answer = put_back(&dash, "nd-8").await;

    assert_eq!(answer["state"], "applied", "{answer}");
    assert_eq!(title_of(&song).as_deref(), Some("Teardrop (Live)"));
    let journal = dash.state.library_action_journal.recent(10);
    let swap = journal
        .iter()
        .find(|e| e.key.contains("putback"))
        .expect("the put back's own entry");
    assert_eq!(swap.revealed_path.as_deref(), Some(slash_root(&song).as_str()));
    let newer = swap.quarantine_path.clone().expect("the newer file's place");
    assert_eq!(title_of(Path::new(&newer)).as_deref(), Some("Teardrop"));
}

#[tokio::test]
async fn a_missing_old_file_says_why_and_touches_nothing() {
    let dash = Dashboard::new(true, false);
    let (entry, song, old) = replaced(&dash.root, "nd-9", "Teardrop", "Teardrop");
    dash.state.library_action_journal.record(entry);
    std::fs::remove_file(&old).expect("gone");

    let answer = put_back(&dash, "nd-9").await;

    assert_eq!(answer["state"], "failed");
    assert_eq!(
        answer["detail"],
        "The old file is no longer in the server's trash."
    );
    assert!(song.exists());
    let list = dash.call(Method::GET, "/api/admin/trash", None).await.json();
    assert_eq!(list["songs"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn with_replaced_files_not_kept_there_is_nothing_to_put_back() {
    let dash = Dashboard::new(true, false);
    let mut settings = (*dash.state.settings.current()).clone();
    settings.library_actions.keep_replaced_originals = false;
    dash.state.settings.set(settings);
    let (entry, song, old) = replaced(&dash.root, "nd-10", "Teardrop", "Teardrop");
    dash.state.library_action_journal.record(entry);
    std::fs::remove_file(&old).expect("deleted once replaced");

    let answer = put_back(&dash, "nd-10").await;

    assert_eq!(answer["state"], "skipped");
    assert_eq!(
        answer["detail"],
        "Your old copy was not kept on this server, so there is nothing to put back."
    );
    assert!(song.exists());
}

// ---- Put back for a Wrong song or Wrong version ----------------------------------------------

#[tokio::test]
async fn a_wrong_song_or_wrong_version_replacement_is_listed_and_put_back_restores_the_original() {
    for (action, id) in [
        (LibraryAction::WrongSong, "nd-11"),
        (LibraryAction::WrongVersion, "nd-12"),
    ] {
        let dash = Dashboard::new(true, false);
        let (entry, song, old) = replaced_by(&dash.root, id, action, "Teardrop", "Teardrop");
        dash.state.library_action_journal.record(entry);

        let list = dash.call(Method::GET, "/api/admin/trash", None).await.json();
        let songs = list["songs"].as_array().expect("songs");
        assert_eq!(songs.len(), 1, "{action:?}: {list}");
        assert_eq!(songs[0]["kind"], "replaced", "{action:?}");
        assert_eq!(songs[0]["blocksDownloads"], false, "{action:?}");
        let old_bytes = std::fs::read(&old).expect("the old file");

        let answer = put_back(&dash, id).await;

        assert_eq!(answer["state"], "applied", "{action:?}: {answer}");
        assert_eq!(
            answer["detail"],
            "Put back your old copy. The one that replaced it is in Recently removed."
        );
        assert_eq!(std::fs::read(&song).expect("the song"), old_bytes, "{action:?}");
        assert!(!old.exists(), "{action:?}");
    }
}

#[tokio::test]
async fn a_wrong_song_replacement_whose_old_file_is_gone_is_not_listed_and_says_why() {
    let dash = Dashboard::new(true, false);
    let (entry, song, old) = replaced_by(
        &dash.root,
        "nd-13",
        LibraryAction::WrongSong,
        "Teardrop",
        "Teardrop",
    );
    dash.state.library_action_journal.record(entry);
    std::fs::remove_file(&old).expect("gone");

    let list = dash.call(Method::GET, "/api/admin/trash", None).await.json();
    assert_eq!(list["songs"].as_array().map(Vec::len), Some(0));
    let answer = put_back(&dash, "nd-13").await;

    assert_eq!(answer["state"], "failed");
    assert_eq!(
        answer["detail"],
        "The old file is no longer in the server's trash."
    );
    assert!(song.exists());
}

/// The Soulseek file the Wrong song denied is allowed again, and only that one: the same user's
/// other file stays denied, and so does a swap back.
#[tokio::test]
async fn put_back_lifts_exactly_the_denial_the_wrong_song_made() {
    let dash = Dashboard::new(true, false);
    let rejected = &dash.state.rejected_peers;
    rejected.deny(
        Some("peer"),
        Some("Music\\teardrop.mp3"),
        "reported as the wrong song by alice",
        "",
    );
    rejected.deny(Some("peer"), Some("Music\\angel.mp3"), "another song", "");
    let (mut entry, _song, _old) = replaced_by(
        &dash.root,
        "nd-14",
        LibraryAction::WrongSong,
        "Teardrop",
        "Teardrop",
    );
    entry.denied_source = Some(DeniedSource {
        username: "peer".into(),
        filename: "Music\\teardrop.mp3".into(),
    });
    dash.state.library_action_journal.record(entry);

    let answer = put_back(&dash, "nd-14").await;

    assert_eq!(answer["state"], "applied", "{answer}");
    assert!(!rejected.is_denied(Some("peer"), Some("Music\\teardrop.mp3")));
    assert!(rejected.is_denied(Some("peer"), Some("Music\\angel.mp3")));
    let swap = dash
        .state
        .library_action_journal
        .recent(10)
        .into_iter()
        .find(|e| e.key.contains("putback"))
        .expect("the put back's own entry");
    assert_eq!(swap.denied_source, None);
}

/// A Wrong song journaled before the denied file was recorded loads and puts back, lifting
/// nothing.
#[tokio::test]
async fn an_older_wrong_song_entry_puts_back_without_lifting_any_denial() {
    let dash = Dashboard::new(true, false);
    let rejected = &dash.state.rejected_peers;
    rejected.deny(
        Some("peer"),
        Some("Music\\teardrop.mp3"),
        "reported as the wrong song by alice",
        "",
    );
    let (entry, song, old) = replaced_by(
        &dash.root,
        "nd-15",
        LibraryAction::WrongSong,
        "Teardrop",
        "Teardrop",
    );
    let written = octo_core::json::to_string(&entry);
    assert!(!written.contains("DeniedSource"), "{written}");
    let older: LibraryActionEntry = serde_json::from_str(&written).expect("an older entry loads");
    assert_eq!(older.denied_source, None);
    dash.state.library_action_journal.record(older);
    let old_bytes = std::fs::read(&old).expect("the old file");

    let answer = put_back(&dash, "nd-15").await;

    assert_eq!(answer["state"], "applied", "{answer}");
    assert_eq!(std::fs::read(&song).expect("the song"), old_bytes);
    assert!(rejected.is_denied(Some("peer"), Some("Music\\teardrop.mp3")));
}

/// The library sweep's note of the replacement says the original was put back, and still counts
/// as asked, so the sweep leaves the file alone.
#[tokio::test]
async fn the_sweep_s_note_reads_as_put_back_and_still_keeps_the_sweep_away() {
    let dash = Dashboard::new(true, false);
    let (entry, song, _old) = replaced_by(
        &dash.root,
        "nd-16",
        LibraryAction::WrongSong,
        "Teardrop",
        "Teardrop",
    );
    dash.state.library_action_journal.record(entry);
    let notices = &dash.state.notice_queue;
    let path = slash_root(&song);
    assert!(notices.add_replaced_because(
        "admin",
        &path,
        &octo_core::models::domain::Song {
            artist: "Massive Attack".into(),
            title: "Teardrop".into(),
            album: "Mezzanine".into(),
            ..Default::default()
        },
        &octo_core::fingerprint::verification::VerificationResult::default(),
        "nd-16",
        "Octo replaced this: it sounded nothing like its YouTube stream (50% of its bits differ)".into(),
        None,
    ));

    let answer = put_back(&dash, "nd-16").await;

    assert_eq!(answer["state"], "applied", "{answer}");
    let note = notices
        .for_user("admin", octo_core::settings::NoticeKind::Review)
        .into_iter()
        .find(|n| n.local_path == path)
        .expect("the note");
    assert_eq!(note.reason, NoticeQueue::PUT_BACK_REASON);
    assert_eq!(note.state, NoticeState::Acted);
    assert!(notices.reviewed_paths().contains(&path));
}

/// The note says what happened last: put back, then replaced again when the replacement comes in
/// again, then put back once more. It stays answered the whole time.
#[tokio::test]
async fn the_sweep_s_note_reads_as_replaced_again_after_a_second_swap() {
    let dash = Dashboard::new(true, false);
    let (entry, song, _old) = replaced_by(
        &dash.root,
        "nd-17",
        LibraryAction::WrongSong,
        "Teardrop",
        "Teardrop",
    );
    dash.state.library_action_journal.record(entry);
    let notices = &dash.state.notice_queue;
    let path = slash_root(&song);
    let first = "Octo replaced this: it sounded nothing like its YouTube stream (50% of its bits differ)";
    assert!(notices.add_replaced_because(
        "admin",
        &path,
        &octo_core::models::domain::Song {
            artist: "Massive Attack".into(),
            title: "Teardrop".into(),
            album: "Mezzanine".into(),
            ..Default::default()
        },
        &octo_core::fingerprint::verification::VerificationResult::default(),
        "nd-17",
        first.into(),
        None,
    ));
    let note = || {
        notices
            .for_user("admin", octo_core::settings::NoticeKind::Review)
            .into_iter()
            .find(|n| n.local_path == path)
            .expect("the note")
    };

    for (swap, expected) in [
        (1, NoticeQueue::PUT_BACK_REASON),
        (2, first),
        (3, NoticeQueue::PUT_BACK_REASON),
    ] {
        let answer = put_back(&dash, "nd-17").await;
        assert_eq!(answer["state"], "applied", "swap {swap}: {answer}");
        assert_eq!(note().reason, expected, "swap {swap}");
        assert_eq!(note().state, NoticeState::Acted, "swap {swap}");
        assert!(notices.reviewed_paths().contains(&path), "swap {swap}");
    }
}
