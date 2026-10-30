//! The library action journal, and the round trip of its state-file fixture.

use std::cell::{Cell, RefCell};

use super::*;

fn entry(
    action: LibraryAction,
    id: &str,
    state: LibraryActionState,
    artist: &str,
    title: &str,
    dry_run: bool,
) -> LibraryActionEntry {
    LibraryActionEntry {
        key: LibraryActionJournal::make_key(action, id, "1:2"),
        action,
        navidrome_id: id.into(),
        username: "alice".into(),
        title: title.into(),
        artist: artist.into(),
        album: "Album".into(),
        source_path: Some(format!("/music/{id}.flac")),
        quarantine_path: None,
        resolution: Some(PathSource::NativeApi),
        state,
        detail: None,
        dry_run,
        at_utc: Utc::now(),
        history_kept: None,
        revealed_path: None,
        copy: false,
        download_allowed: false,
        denied_source: None,
    }
}

fn plain(action: LibraryAction, id: &str, state: LibraryActionState) -> LibraryActionEntry {
    entry(action, id, state, "Artist", "Title", false)
}

fn temp_file(dir: &tempfile::TempDir) -> String {
    let path = dir
        .path()
        .join(format!("octo-recon-{}.flac", uuid::Uuid::new_v4()));
    std::fs::write(&path, [0u8; 8]).expect("written");
    path.to_string_lossy().into_owned()
}

fn detail(journal: &LibraryActionJournal) -> String {
    journal.recent(1)[0].detail.clone().unwrap_or_default()
}

#[test]
fn already_applied_same_action_and_file_content_is_true() {
    let journal = LibraryActionJournal::new();
    journal.record(plain(
        LibraryAction::Delete,
        "song-a",
        LibraryActionState::Applied,
    ));

    assert!(journal.already_applied(LibraryAction::Delete, "song-a", "1:2"));
}

/// After a Better quality upgrade the SAME Navidrome id points at a NEW file, and the user is
/// entitled to ask for a better copy of that one too. Fingerprinting on size and mtime rather
/// than the id alone is what allows that.
#[test]
fn already_applied_same_id_different_file_content_is_false() {
    let journal = LibraryActionJournal::new();
    journal.record(plain(
        LibraryAction::BetterQuality,
        "song-a",
        LibraryActionState::Applied,
    ));

    assert!(!journal.already_applied(LibraryAction::BetterQuality, "song-a", "9:9"));
}

/// The one that bit on the first live run. A rehearsal writes an entry under the same key, so
/// counting it would mean every dry run permanently disarmed the real action for that file, and
/// the sweep would answer Skipped and consume the request having done nothing.
#[test]
fn already_applied_dry_run_entry_does_not_suppress_the_real_action() {
    let journal = LibraryActionJournal::new();
    journal.record(entry(
        LibraryAction::Delete,
        "song-a",
        LibraryActionState::Applied,
        "Artist",
        "Title",
        true,
    ));

    assert!(!journal.already_applied(LibraryAction::Delete, "song-a", "1:2"));
}

#[test]
fn already_applied_failed_entry_is_retried() {
    let journal = LibraryActionJournal::new();
    journal.record(plain(LibraryAction::Delete, "song-a", LibraryActionState::Failed));

    assert!(!journal.already_applied(LibraryAction::Delete, "song-a", "1:2"));
}

/// A delete means the user does not want it back.
#[test]
fn is_never_requested_after_an_applied_delete_is_true() {
    let journal = LibraryActionJournal::new();
    journal.record(entry(
        LibraryAction::Delete,
        "song-a",
        LibraryActionState::Applied,
        "Drake",
        "Rich Flex",
        false,
    ));

    assert!(journal.is_never_requested(Some("Drake"), Some("Rich Flex")));
    assert!(journal.is_never_requested(Some(" drake "), Some("RICH FLEX")));
    assert!(!journal.is_never_requested(Some("Drake"), Some("Something Else")));
}

/// A rehearsal must not silently stop a track being downloadable.
#[test]
fn is_never_requested_dry_run_delete_does_not_block_anything() {
    let journal = LibraryActionJournal::new();
    journal.record(entry(
        LibraryAction::Delete,
        "song-a",
        LibraryActionState::Applied,
        "Drake",
        "Rich Flex",
        true,
    ));

    assert!(!journal.is_never_requested(Some("Drake"), Some("Rich Flex")));
}

#[test]
fn is_never_requested_other_actions_do_not_block_a_redownload() {
    let journal = LibraryActionJournal::new();
    journal.record(entry(
        LibraryAction::WrongSong,
        "song-a",
        LibraryActionState::Applied,
        "Drake",
        "Rich Flex",
        false,
    ));

    assert!(!journal.is_never_requested(Some("Drake"), Some("Rich Flex")));
}

/// The move landed and only the bookkeeping after it was lost, so the action is done. It must
/// not run again.
#[test]
fn reconcile_quarantined_but_not_recorded_becomes_applied() {
    let dir = tempfile::tempdir().expect("temp dir");
    let quarantine = temp_file(&dir);
    let journal = LibraryActionJournal::new();
    journal.record(LibraryActionEntry {
        source_path: Some("/music/gone.flac".into()),
        quarantine_path: Some(quarantine),
        ..plain(LibraryAction::Delete, "song-a", LibraryActionState::Pending)
    });

    assert_eq!(journal.reconcile(None), 1);
    assert!(journal.already_applied(LibraryAction::Delete, "song-a", "1:2"));
}

/// The file is still where it was, so nothing happened. Marking it Failed rather than leaving
/// it Pending stops it being treated as in flight forever.
#[test]
fn reconcile_source_still_present_becomes_failed_without_rerunning() {
    let dir = tempfile::tempdir().expect("temp dir");
    let source = temp_file(&dir);
    let journal = LibraryActionJournal::new();
    journal.record(LibraryActionEntry {
        source_path: Some(source),
        quarantine_path: None,
        ..plain(LibraryAction::Delete, "song-a", LibraryActionState::Pending)
    });

    assert_eq!(journal.reconcile(None), 1);
    assert!(!journal.already_applied(LibraryAction::Delete, "song-a", "1:2"));
    assert!(journal.pending().is_empty());
}

// ---- Reconcile after a crash mid-replacement --------------------------------------------
//
// The executor now records the quarantine path on the Pending entry as soon as the move
// lands, before a replacement is fetched. These are the states a crash in that window leaves.

#[test]
fn reconcile_interrupted_replacement_puts_the_original_back() {
    let dir = tempfile::tempdir().expect("temp dir");
    let quarantine = temp_file(&dir);
    let journal = LibraryActionJournal::new();
    journal.record(LibraryActionEntry {
        source_path: Some("/music/gone.flac".into()),
        quarantine_path: Some(quarantine.clone()),
        ..plain(LibraryAction::WrongVersion, "song-r", LibraryActionState::Pending)
    });
    let restored = RefCell::new(None::<String>);

    journal.reconcile(Some(&|path: &str| {
        *restored.borrow_mut() = Some(path.to_string());
        true
    }));

    assert_eq!(restored.into_inner(), Some(quarantine));
    assert!(detail(&journal).contains("original was put back"));
    assert!(journal.pending().is_empty());
}

/// The replacement had already moved in when Octo stopped. Putting the original back would
/// leave both in the library (or be refused at a shared path), so the action counts as done and
/// the original stays in quarantine.
#[test]
fn reconcile_replacement_already_in_place_is_applied_without_a_restore() {
    let dir = tempfile::tempdir().expect("temp dir");
    let quarantine = temp_file(&dir);
    let revealed = temp_file(&dir);
    let journal = LibraryActionJournal::new();
    journal.record(LibraryActionEntry {
        source_path: Some("/music/gone.mp3".into()),
        quarantine_path: Some(quarantine.clone()),
        revealed_path: Some(revealed.clone()),
        ..plain(LibraryAction::WrongVersion, "song-v", LibraryActionState::Pending)
    });
    let restore_called = Cell::new(false);

    journal.reconcile(Some(&|_: &str| {
        restore_called.set(true);
        true
    }));

    assert!(!restore_called.get());
    assert!(Path::new(&quarantine).exists());
    assert!(journal.already_applied(LibraryAction::WrongVersion, "song-v", "1:2"));
    assert!(detail(&journal).contains(&revealed));
}

#[test]
fn reconcile_interrupted_replacement_that_cannot_be_restored_says_where_the_original_is() {
    let dir = tempfile::tempdir().expect("temp dir");
    let quarantine = temp_file(&dir);
    let journal = LibraryActionJournal::new();
    journal.record(LibraryActionEntry {
        source_path: Some("/music/gone.flac".into()),
        quarantine_path: Some(quarantine.clone()),
        ..plain(LibraryAction::WrongSong, "song-s", LibraryActionState::Pending)
    });

    journal.reconcile(Some(&|_: &str| false));

    assert!(detail(&journal).contains(&quarantine));
}

/// A replacement may have landed at the original path. Restoring would overwrite it, so neither
/// file is touched and the user is told both exist.
#[test]
fn reconcile_replacement_with_a_file_back_at_the_source_touches_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let quarantine = temp_file(&dir);
    let source = temp_file(&dir);
    let journal = LibraryActionJournal::new();
    journal.record(LibraryActionEntry {
        source_path: Some(source),
        quarantine_path: Some(quarantine),
        ..plain(
            LibraryAction::BetterQuality,
            "song-q",
            LibraryActionState::Pending,
        )
    });
    let restore_called = Cell::new(false);

    journal.reconcile(Some(&|_: &str| {
        restore_called.set(true);
        true
    }));

    assert!(!restore_called.get());
    assert!(detail(&journal).contains("compare them"));
}

/// Used to read "nothing was changed" while the file was gone from its path.
#[test]
fn reconcile_source_gone_and_no_quarantine_recorded_does_not_claim_nothing_changed() {
    let journal = LibraryActionJournal::new();
    journal.record(LibraryActionEntry {
        source_path: Some(format!("/music/gone-{}.flac", uuid::Uuid::new_v4())),
        quarantine_path: None,
        ..plain(LibraryAction::Delete, "song-g", LibraryActionState::Pending)
    });

    journal.reconcile(None);

    assert!(!detail(&journal).contains("nothing was changed"));
    assert!(detail(&journal).contains("check the quarantine folder"));
}

#[test]
fn entries_survive_a_restart() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir
        .path()
        .join(format!("octo-actions-{}.json", uuid::Uuid::new_v4()));
    {
        let first = LibraryActionJournal::with_path(Some(path.clone()));
        first.record(entry(
            LibraryAction::Delete,
            "song-a",
            LibraryActionState::Applied,
            "Drake",
            "Rich Flex",
            false,
        ));
    }

    let second = LibraryActionJournal::with_path(Some(path));
    assert!(second.is_never_requested(Some("Drake"), Some("Rich Flex")));
}

/// The shared journal only ever holds actions an older build knows the numbers of; Wrong lyrics
/// and Wrong cover answers are kept in a file of their own.
#[test]
fn lyrics_and_cover_answers_are_kept_in_their_own_journal_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("library-actions.json");
    {
        let journal = LibraryActionJournal::with_path(Some(path.clone()));
        journal.record(plain(
            LibraryAction::Delete,
            "song-a",
            LibraryActionState::Applied,
        ));
        journal.record(plain(
            LibraryAction::WrongLyrics,
            "song-b",
            LibraryActionState::Applied,
        ));
        journal.record(plain(
            LibraryAction::WrongCover,
            "song-c",
            LibraryActionState::Applied,
        ));
        assert!(journal.flush());
    }

    let shared: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("the shared file")).expect("JSON");
    let actions: Vec<i64> = shared
        .as_array()
        .expect("a list")
        .iter()
        .map(|entry| entry["Action"].as_i64().expect("a number"))
        .collect();
    assert_eq!(actions, [0]);
    let answers =
        std::fs::read_to_string(dir.path().join("library-answer-journal.json")).expect("the answers file");
    assert!(
        answers.contains("song-b") && answers.contains("song-c"),
        "{answers}"
    );

    let reloaded = LibraryActionJournal::with_path(Some(path));
    let mut ids: Vec<String> = reloaded.recent(10).into_iter().map(|e| e.navidrome_id).collect();
    ids.sort();
    assert_eq!(ids, ["song-a", "song-b", "song-c"]);
}

/// A shared journal an earlier build of this version wrote answers into gives them up to the
/// answers file on the next flush.
#[test]
fn answers_found_in_the_shared_journal_move_to_their_own_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("library-actions.json");
    std::fs::write(
        &path,
        r#"[{"Key":"a","Action":0,"State":1},{"Key":"b","Action":5,"State":1}]"#,
    )
    .expect("written");
    let journal = LibraryActionJournal::with_path(Some(path.clone()));
    assert!(journal.flush());

    let shared = std::fs::read_to_string(&path).expect("the shared file");
    assert!(!shared.contains(r#""Key":"b""#), "{shared}");
    let answers =
        std::fs::read_to_string(dir.path().join("library-answer-journal.json")).expect("the answers file");
    assert!(answers.contains(r#""Key":"b""#), "{answers}");
    assert_eq!(journal.recent(10).len(), 2);
}

#[test]
fn recent_is_newest_first() {
    let journal = LibraryActionJournal::new();
    journal.record(plain(
        LibraryAction::Delete,
        "song-a",
        LibraryActionState::Applied,
    ));
    journal.record(plain(
        LibraryAction::Delete,
        "song-b",
        LibraryActionState::Applied,
    ));

    assert_eq!(journal.recent(200)[0].navidrome_id, "song-b");
}

// ---- The state file ------------------------------------------------------------------------

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/rust-migration/fixtures/state/library-actions.json")
}

/// Read, write, compare, byte for byte.
#[test]
fn the_fixture_round_trips_byte_for_byte() {
    let original = std::fs::read_to_string(fixture()).expect("the fixture");
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("library-actions.json");
    std::fs::write(&path, &original).expect("copied");

    let journal = LibraryActionJournal::with_path(Some(path.clone()));
    let recent = journal.recent(10);
    assert_eq!(recent.len(), 5);
    let applied = &recent[4];
    assert_eq!(applied.action, LibraryAction::Delete);
    assert_eq!(applied.artist, "Sigur Rós");
    assert_eq!(applied.resolution, Some(PathSource::NativeApi));
    assert_eq!(applied.state, LibraryActionState::Applied);
    assert!(applied.blocks_downloads());
    assert_eq!(recent[3].resolution, Some(PathSource::LocalMappings));
    assert_eq!(recent[3].history_kept, Some(true));
    assert!(journal.is_never_requested(Some("Sigur Rós"), Some("Svefn-g-englar")));
    // A removed copy, a song allowed again and a put back block nothing.
    assert!(recent[2].copy && !recent[2].blocks_downloads());
    assert!(recent[1].download_allowed && !recent[1].blocks_downloads());
    assert_eq!(recent[0].state, LibraryActionState::Restored);
    assert!(!journal.is_never_requested(Some("宇多田ヒカル"), Some("First Love")));
    assert!(!journal.is_never_requested(Some("Portishead"), Some("Roads")));

    // Nothing changed, so nothing is written; a change writes the whole file again.
    assert!(journal.flush());
    journal.dirty.store(true, Ordering::SeqCst);
    assert!(journal.flush());
    assert_eq!(std::fs::read_to_string(&path).expect("written"), original);
    assert!(!state_file::tmp_path(&path).exists());
}

/// The fixture's entries name no denied Soulseek file and still load; one that does keeps it
/// across a write, and one that does not writes no field for it.
#[test]
fn a_denied_source_is_optional_and_kept_when_there_is_one() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("library-actions.json");
    std::fs::copy(fixture(), &path).expect("copied");
    let journal = LibraryActionJournal::with_path(Some(path.clone()));
    assert_eq!(journal.recent(10).len(), 5);
    assert!(journal.recent(10).iter().all(|e| e.denied_source.is_none()));

    journal.record(plain(
        LibraryAction::WrongSong,
        "song-a",
        LibraryActionState::Pending,
    ));
    journal.set_denied_source(
        "WrongSong|song-a|1:2",
        DeniedSource {
            username: "peer".into(),
            filename: "Music\\a.flac".into(),
        },
    );
    assert!(journal.flush());
    let text = std::fs::read_to_string(&path).expect("written");
    assert_eq!(text.matches("\"DeniedSource\"").count(), 1, "{text}");

    let again = LibraryActionJournal::with_path(Some(path));
    let entry = again
        .recent(10)
        .into_iter()
        .find(|e| e.navidrome_id == "song-a")
        .expect("the Wrong song");
    assert_eq!(
        entry.denied_source,
        Some(DeniedSource {
            username: "peer".into(),
            filename: "Music\\a.flac".into(),
        })
    );
}

/// Entries with an empty key are skipped, and a file that is not JSON leaves the journal empty.
#[test]
fn a_bad_file_or_an_entry_without_a_key_is_skipped() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("library-actions.json");
    std::fs::write(
        &path,
        r#"[{"Key":"","Action":0},{"Key":"k","Action":4,"State":1}]"#,
    )
    .expect("written");
    let journal = LibraryActionJournal::with_path(Some(path.clone()));
    let entries = journal.recent(10);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].action, LibraryAction::Keep);
    assert_eq!(entries[0].at_utc, datetime::min_value());

    std::fs::write(&path, "not json").expect("written");
    assert!(LibraryActionJournal::with_path(Some(path)).recent(10).is_empty());
}

/// The write-ahead flush reports a failure, so the executor can refuse to touch the file.
#[test]
fn a_flush_that_cannot_write_says_so_and_stays_dirty() {
    let dir = tempfile::tempdir().expect("temp dir");
    // A directory where the file should be: the rename over it fails.
    let path = dir.path().join("library-actions.json");
    std::fs::create_dir_all(path.join("in-the-way")).expect("a directory");
    let journal = LibraryActionJournal::with_path(Some(path.clone()));
    journal.record(plain(
        LibraryAction::Delete,
        "song-a",
        LibraryActionState::Pending,
    ));

    assert!(!journal.flush());
    assert!(journal.dirty.load(Ordering::SeqCst));
    std::fs::remove_dir_all(&path).expect("cleared");
    assert!(journal.flush());
}

/// At most 2,000 entries, the oldest dropped first.
#[test]
fn the_journal_keeps_the_newest_two_thousand() {
    let journal = LibraryActionJournal::new();
    for i in 0..2005 {
        journal.record(plain(
            LibraryAction::Delete,
            &format!("song-{i}"),
            LibraryActionState::Applied,
        ));
    }
    let all = journal.recent(5000);
    assert_eq!(all.len(), 2000);
    assert_eq!(all.last().map(|e| e.navidrome_id.as_str()), Some("song-5"));
}

/// The "wrong version asked" mark is the Wrong version Octo answered with the Find songs words. A
/// pick of a copy that did not replace the file keeps it; a pick that replaced it, or any other
/// action on the song, ends it; another song's actions never touch it.
#[test]
fn the_wrong_version_mark_lasts_until_a_pick_replaces_the_file_or_another_action_runs() {
    const ASKED: &str = "pick the copy you want";
    let journal = LibraryActionJournal::new();
    let at = |seconds: i64| Utc::now() + chrono::TimeDelta::seconds(seconds);
    assert!(!journal.wrong_version_asked("nd-1", ASKED));

    journal.record(LibraryActionEntry {
        detail: Some(ASKED.into()),
        at_utc: at(1),
        ..plain(LibraryAction::WrongVersion, "nd-1", LibraryActionState::Skipped)
    });
    assert!(journal.wrong_version_asked("nd-1", ASKED));
    assert!(!journal.wrong_version_asked("nd-1", "other words"));

    let pick = |state: LibraryActionState, seconds: i64| LibraryActionEntry {
        key: LibraryActionJournal::make_key(
            LibraryAction::WrongVersion,
            "nd-1",
            &format!("1:2{PICK_SUFFIX}"),
        ),
        at_utc: at(seconds),
        ..plain(LibraryAction::WrongVersion, "nd-1", state)
    };
    journal.record(pick(LibraryActionState::Failed, 2));
    assert!(
        journal.wrong_version_asked("nd-1", ASKED),
        "a pick refused keeps the mark"
    );
    journal.record(LibraryActionEntry {
        at_utc: at(3),
        ..plain(LibraryAction::BetterQuality, "nd-2", LibraryActionState::Applied)
    });
    assert!(
        journal.wrong_version_asked("nd-1", ASKED),
        "another song's action"
    );

    journal.record(pick(LibraryActionState::Applied, 4));
    assert!(
        !journal.wrong_version_asked("nd-1", ASKED),
        "a pick replaced the file"
    );

    let other = LibraryActionJournal::new();
    other.record(LibraryActionEntry {
        detail: Some(ASKED.into()),
        at_utc: at(1),
        ..plain(LibraryAction::WrongVersion, "nd-1", LibraryActionState::Skipped)
    });
    other.record(LibraryActionEntry {
        at_utc: at(2),
        ..plain(LibraryAction::WrongSong, "nd-1", LibraryActionState::Failed)
    });
    assert!(
        !other.wrong_version_asked("nd-1", ASKED),
        "another action on the song"
    );
}

#[test]
fn keys_and_fingerprints_keep_their_format() {
    assert_eq!(
        LibraryActionJournal::make_key(
            LibraryAction::BetterQuality,
            "nd-1",
            &LibraryActionJournal::fingerprint(8650752, 639322000000000000)
        ),
        "BetterQuality|nd-1|8650752:639322000000000000"
    );
}
