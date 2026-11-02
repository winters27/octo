//! The lyrics writer: writing beside a song, saving inside it, upgrades, choices and undo, and
//! the queue worker. The tags are held in memory ([`FakeTags`]), the seam the writer reads
//! them through.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use octo_core::lyrics::{
    ILyricsSource, LyricsLookup, LyricsResult, LyricsTiming, SongLyrics, SongLyricsPlace,
};
use octo_core::settings::{AppSettings, LyricsSaveTo, MetadataSettings, SettingsStore};
use tokio_util::sync::CancellationToken;

use super::{LyricsJob, LyricsPathRemap, LyricsSidecarWriter, LyricsWriteOutcome};
use crate::services::library::library_write_gate::LibraryWriteGate;
use crate::services::lyrics::lyrics_service::LyricsService;
use crate::services::lyrics::lyrics_undo_journal::{LyricsUndoEntry, LyricsUndoJournal};
use crate::services::lyrics::song_lyrics;
use crate::services::lyrics::test_support::{FakeSource, FakeTags};

const MARK: &str = LyricsSidecarWriter::OCTO_MARK;
const WORDS: &str = "[00:01.00]<00:01.00>word <00:01.50>by word<00:02.00>";
const LINES: &str = "[00:01.00]line by line";

struct Song {
    _dir: tempfile::TempDir,
    root: PathBuf,
    tags: Arc<FakeTags>,
}

impl Song {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("a temp dir");
        Self {
            root: dir.path().to_path_buf(),
            _dir: dir,
            tags: Arc::new(FakeTags::default()),
        }
    }

    /// `Audio()` / `Mp3()`: a song file, with lyrics in its tags when given.
    fn mp3(&self, name: &str, tag_lyrics: Option<&str>) -> PathBuf {
        let path = self.root.join(name);
        std::fs::write(&path, b"ID3 not really audio").expect("written");
        if let Some(lyrics) = tag_lyrics {
            self.tags.set(&path, lyrics);
        }
        path
    }

    fn writer(&self, order: &str, save_to: &str, source: Arc<FakeSource>) -> LyricsSidecarWriter {
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            metadata: MetadataSettings {
                lyrics_sources: order.to_string(),
                save_lyrics_to: save_to.to_string(),
                ..MetadataSettings::default()
            },
            ..AppSettings::default()
        }));
        let lyrics = Arc::new(LyricsService::new(
            vec![source as Arc<dyn ILyricsSource>],
            settings.clone(),
        ));
        LyricsSidecarWriter::with_tags(lyrics, settings, self.tags.clone())
    }

    /// A writer on LRCLIB only, saving beside.
    fn lrclib_writer(
        &self,
        answer: impl Fn() -> LyricsLookup + Send + Sync + 'static,
    ) -> LyricsSidecarWriter {
        self.writer("lrclib", LyricsSaveTo::BESIDE, FakeSource::new("lrclib", answer))
    }

    /// A writer on KuGou after the song's own, saving to `save_to`.
    fn kugou_writer(&self, save_to: &str, found: LyricsResult) -> LyricsSidecarWriter {
        self.writer(
            "song,kugou",
            save_to,
            FakeSource::new("kugou", move || LyricsLookup::new(Some(found.clone()), false)),
        )
    }
}

fn job(path: &Path) -> LyricsJob {
    LyricsJob::new(path, "Artist", "Song", None, Some(200))
}

fn found(source: &str, synced: Option<&str>, plain: Option<&str>) -> LyricsLookup {
    LyricsLookup::new(
        Some(LyricsResult::new(
            source,
            synced.map(String::from),
            plain.map(String::from),
            false,
        )),
        false,
    )
}

fn beside(path: &Path, extension: &str) -> PathBuf {
    path.with_extension(extension)
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("written")
}

fn none() -> CancellationToken {
    CancellationToken::new()
}

// ---- Writing beside the song ----------------------------------------------------------------

#[tokio::test]
async fn writer_synced_writes_an_lrc_beside_the_file() {
    let song = Song::new();
    let audio = song.mp3("Artist - Song.mp3", None);

    let write = song
        .lrclib_writer(|| found("lrclib", Some("[00:01.00]line"), Some("line")))
        .write(&job(&audio), false, &none())
        .await
        .expect("written");

    assert_eq!(write.outcome, LyricsWriteOutcome::Written);
    assert_eq!(read(&beside(&audio, "lrc")), format!("{MARK}\n[00:01.00]line\n"));
    assert!(!beside(&audio, "txt").exists());
}

#[tokio::test]
async fn writer_plain_only_writes_a_txt() {
    let song = Song::new();
    let audio = song.mp3("Artist - Song.mp3", None);

    song.lrclib_writer(|| found("ovh", None, Some("words")))
        .write(&job(&audio), false, &none())
        .await
        .expect("written");

    assert_eq!(read(&beside(&audio, "txt")), "words\n");
}

#[tokio::test]
async fn writer_instrumental_writes_nothing() {
    let song = Song::new();
    let audio = song.mp3("Artist - Song.mp3", None);

    let write = song
        .lrclib_writer(|| LyricsLookup::new(Some(LyricsResult::new("lrclib", None, None, true)), false))
        .write(&job(&audio), false, &none())
        .await
        .expect("written");

    assert_eq!(write.outcome, LyricsWriteOutcome::Instrumental);
    let lyrics_files = std::fs::read_dir(&song.root)
        .expect("listed")
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            name.ends_with(".lrc") || name.ends_with(".txt")
        })
        .count();
    assert_eq!(lyrics_files, 0);
}

#[tokio::test]
async fn writer_existing_sidecar_is_left_alone() {
    let song = Song::new();
    let audio = song.mp3("Artist - Song.mp3", None);
    let existing = beside(&audio, "lrc");
    std::fs::write(&existing, "mine").expect("written");

    let write = song
        .lrclib_writer(|| found("lrclib", Some("[00:01.00]theirs"), None))
        .write(&job(&audio), false, &none())
        .await
        .expect("written");

    assert_eq!(write.outcome, LyricsWriteOutcome::AlreadyThere);
    assert_eq!(read(&existing), "mine");
}

#[tokio::test]
async fn writer_embedded_lyrics_are_left_alone() {
    let song = Song::new();
    let audio = song.mp3("Artist - Song.mp3", Some("already here"));

    let write = song
        .lrclib_writer(|| found("lrclib", Some("[00:01.00]x"), None))
        .write(&job(&audio), false, &none())
        .await
        .expect("written");

    assert_eq!(write.outcome, LyricsWriteOutcome::AlreadyThere);
}

#[tokio::test]
async fn writer_busy_service_retries_and_then_gives_up() {
    let song = Song::new();
    let audio = song.mp3("Artist - Song.mp3", None);
    let writer = song.lrclib_writer(LyricsLookup::failed);

    let first = writer.write(&job(&audio).with_attempt(1), false, &none()).await;
    let last = writer.write(&job(&audio).with_attempt(3), false, &none()).await;

    assert_eq!(first.expect("written").outcome, LyricsWriteOutcome::Retrying);
    assert_eq!(last.expect("written").outcome, LyricsWriteOutcome::GaveUp);
}

#[tokio::test]
async fn writer_a_song_that_is_gone_is_gone() {
    let song = Song::new();
    let write = song
        .lrclib_writer(|| found("lrclib", Some("[00:01.00]x"), None))
        .write(&job(&song.root.join("missing.mp3")), false, &none())
        .await
        .expect("written");
    assert_eq!(write.outcome, LyricsWriteOutcome::Gone);
}

// ---- Saving inside, and upgrades ------------------------------------------------------------

#[tokio::test]
async fn save_inside_writes_the_tags_marked_as_octos_and_no_file() {
    let song = Song::new();
    let audio = song.mp3("Artist - Song.mp3", None);

    let write = song
        .kugou_writer(
            LyricsSaveTo::INSIDE,
            LyricsResult::new("KuGou", Some(WORDS.into()), None, false),
        )
        .write(&job(&audio), false, &none())
        .await
        .expect("written");

    assert_eq!(write.outcome, LyricsWriteOutcome::Written);
    assert_eq!(song.tags.get(&audio), Some(format!("{MARK}\n{WORDS}")));
    assert!(!beside(&audio, "lrc").exists());
    assert_eq!(
        song_lyrics::of_with_tag_lyrics(&audio, song.tags.get(&audio).as_deref()),
        SongLyrics::new(SongLyricsPlace::Inside, LyricsTiming::Word, true, false)
    );
}

#[tokio::test]
async fn save_both_writes_the_tags_and_a_file() {
    let song = Song::new();
    let audio = song.mp3("Artist - Song.mp3", None);

    song.kugou_writer(
        LyricsSaveTo::BOTH,
        LyricsResult::new("KuGou", Some(WORDS.into()), None, false),
    )
    .write(&job(&audio), false, &none())
    .await
    .expect("written");

    assert!(song.tags.get(&audio).expect("tagged").starts_with(MARK));
    assert!(read(&beside(&audio, "lrc")).starts_with(MARK));
}

#[tokio::test]
async fn upgrade_someone_elses_tag_lyrics_stay_and_better_ones_go_beside() {
    let song = Song::new();
    let audio = song.mp3("Artist - Song.mp3", Some(LINES));

    let write = song
        .kugou_writer(
            LyricsSaveTo::INSIDE,
            LyricsResult::new("KuGou", Some(WORDS.into()), None, false),
        )
        .write(&job(&audio), true, &none())
        .await
        .expect("written");

    assert_eq!(write.outcome, LyricsWriteOutcome::Upgraded);
    assert_eq!(song.tags.get(&audio).as_deref(), Some(LINES));
    assert_eq!(read(&beside(&audio, "lrc")), format!("{MARK}\n{WORDS}\n"));
}

#[tokio::test]
async fn upgrade_octos_own_tag_lyrics_are_replaced_in_place() {
    let song = Song::new();
    let audio = song.mp3("Artist - Song.mp3", Some(&format!("{MARK}\n{LINES}")));

    let write = song
        .kugou_writer(
            LyricsSaveTo::INSIDE,
            LyricsResult::new("KuGou", Some(WORDS.into()), None, false),
        )
        .write(&job(&audio), true, &none())
        .await
        .expect("written");

    assert_eq!(write.outcome, LyricsWriteOutcome::Upgraded);
    assert_eq!(song.tags.get(&audio), Some(format!("{MARK}\n{WORDS}")));
    assert!(!beside(&audio, "lrc").exists());
}

#[tokio::test]
async fn upgrade_nothing_better_leaves_the_song_alone() {
    let song = Song::new();
    let audio = song.mp3("Artist - Song.mp3", Some(LINES));

    let write = song
        .kugou_writer(
            LyricsSaveTo::BESIDE,
            LyricsResult::new("LRCLIB", Some("[00:01.00]other lines".into()), None, false),
        )
        .write(&job(&audio), true, &none())
        .await
        .expect("written");

    assert_eq!(write.outcome, LyricsWriteOutcome::AlreadyThere);
    assert!(!beside(&audio, "lrc").exists());
}

#[tokio::test]
async fn upgrade_plain_tag_lyrics_get_timed_ones_beside() {
    let song = Song::new();
    let audio = song.mp3("Artist - Song.mp3", Some("just the words"));

    let write = song
        .kugou_writer(
            LyricsSaveTo::BESIDE,
            LyricsResult::new("LRCLIB", Some(LINES.into()), None, false),
        )
        .write(&job(&audio), true, &none())
        .await
        .expect("written");

    assert_eq!(write.outcome, LyricsWriteOutcome::Upgraded);
    assert_eq!(song.tags.get(&audio).as_deref(), Some("just the words"));
    assert!(beside(&audio, "lrc").exists());
}

#[tokio::test]
async fn no_upgrade_a_song_with_lyrics_is_never_looked_up() {
    let song = Song::new();
    let audio = song.mp3("Artist - Song.mp3", Some(LINES));
    let kugou = FakeSource::new("kugou", || found("KuGou", Some(WORDS), None));
    let writer = song.writer("kugou", LyricsSaveTo::BESIDE, kugou.clone());

    let write = writer.write(&job(&audio), false, &none()).await.expect("written");

    assert_eq!(write.outcome, LyricsWriteOutcome::AlreadyThere);
    assert_eq!(kugou.calls(), 0);
}

// ---- Upgrading Octo's own sidecar -----------------------------------------------------------

#[tokio::test]
async fn sidecar_upgrade_replaces_octos_line_timed_file_but_never_the_owners() {
    let song = Song::new();
    let ours = song.mp3("01 Ours.mp3", None);
    let theirs = song.mp3("02 Theirs.mp3", None);
    std::fs::write(beside(&ours, "lrc"), format!("{MARK}\n[00:01.00]line only\n")).expect("written");
    std::fs::write(beside(&theirs, "lrc"), "[00:01.00]the owner's line\n").expect("written");
    let words = FakeSource::new("kugou", || {
        found("KuGou", Some("[00:01.00]<00:01.00>word<00:02.00>"), None)
    });
    let writer = song.writer("kugou", LyricsSaveTo::BESIDE, words);

    let upgraded = writer
        .write(
            &LyricsJob::new(&ours, "Artist", "Ours", None, Some(1)),
            true,
            &none(),
        )
        .await
        .expect("written");
    let kept = writer
        .write(
            &LyricsJob::new(&theirs, "Artist", "Theirs", None, Some(1)),
            true,
            &none(),
        )
        .await
        .expect("written");

    assert_eq!(upgraded.outcome, LyricsWriteOutcome::Upgraded);
    assert_eq!(
        read(&beside(&ours, "lrc")),
        format!("{MARK}\n[00:01.00]<00:01.00>word<00:02.00>\n")
    );
    assert_eq!(kept.outcome, LyricsWriteOutcome::AlreadyThere);
    assert_eq!(read(&beside(&theirs, "lrc")), "[00:01.00]the owner's line\n");
}

// ---- Saving what someone chose, and undoing it ----------------------------------------------

#[tokio::test]
async fn save_chosen_records_what_each_write_replaces_and_restore_puts_it_back() {
    let song = Song::new();
    let audio = song.mp3("Artist - Song.mp3", Some(&format!("{MARK}\nold words")));
    std::fs::write(beside(&audio, "lrc"), format!("{MARK}\n[00:01.00]old\n")).expect("written");
    let writer = song.kugou_writer(LyricsSaveTo::BOTH, LyricsResult::new("x", None, None, false));
    let journal = LyricsUndoJournal::new(None);
    let record = |path: &Path, kind: &str, before: Option<&str>| -> anyhow::Result<()> {
        journal.record(&path.to_string_lossy(), kind, before, "run-1")?;
        Ok(())
    };
    let chosen = LyricsResult::new("KuGou", Some(WORDS.into()), None, false);

    assert!(
        writer
            .save_chosen(&audio, &chosen, &record, &none())
            .await
            .expect("saved")
    );

    let entries = journal.read_all().expect("read");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].kind, LyricsUndoJournal::INSIDE);
    assert_eq!(
        entries[0].before.as_deref(),
        Some(format!("{MARK}\nold words").as_str())
    );
    assert_eq!(entries[1].kind, LyricsUndoJournal::BESIDE);
    assert_eq!(entries[1].path, beside(&audio, "lrc").to_string_lossy());
    assert_eq!(
        entries[1].before.as_deref(),
        Some(format!("{MARK}\n[00:01.00]old\n").as_str())
    );
    assert_eq!(song.tags.get(&audio), Some(format!("{MARK}\n{WORDS}")));

    for entry in entries.iter().rev() {
        assert!(writer.restore(entry), "{entry:?}");
    }
    assert_eq!(song.tags.get(&audio), Some(format!("{MARK}\nold words")));
    assert_eq!(read(&beside(&audio, "lrc")), format!("{MARK}\n[00:01.00]old\n"));
}

#[tokio::test]
async fn restore_leaves_a_file_that_is_no_longer_octos() {
    let song = Song::new();
    let audio = song.mp3("Artist - Song.mp3", Some("the owner's words"));
    let lrc = beside(&audio, "lrc");
    std::fs::write(&lrc, "[00:01.00]the owner's now\n").expect("written");
    let writer = song.kugou_writer(LyricsSaveTo::BESIDE, LyricsResult::new("x", None, None, false));
    let at = chrono::Utc::now();

    let added = LyricsUndoEntry::new(lrc.to_string_lossy(), LyricsUndoJournal::BESIDE, None, "r", at);
    assert!(!writer.restore(&added));
    assert!(lrc.exists());
    let inside = LyricsUndoEntry::new(audio.to_string_lossy(), LyricsUndoJournal::INSIDE, None, "r", at);
    assert!(!writer.restore(&inside));
    assert_eq!(song.tags.get(&audio).as_deref(), Some("the owner's words"));

    // A .txt Octo added is removed while it is there, whoever's it reads as.
    let txt = beside(&audio, "TXT");
    std::fs::write(&txt, "words\n").expect("written");
    let added = LyricsUndoEntry::new(txt.to_string_lossy(), LyricsUndoJournal::BESIDE, None, "r", at);
    assert!(writer.restore(&added));
    assert!(!txt.exists());
    assert!(!writer.restore(&added));
}

#[tokio::test]
async fn replace_writes_only_where_octo_may() {
    let song = Song::new();
    let mine = song.mp3("01 Mine.mp3", Some("the owner's words"));
    let free = song.mp3("02 Free.mp3", None);
    let writer = song.kugou_writer(LyricsSaveTo::BESIDE, LyricsResult::new("x", None, None, false));
    let chosen = LyricsResult::new("KuGou", None, Some("chosen words".into()), false);

    assert!(!writer.replace(&mine, &chosen, &none()).await.expect("checked"));
    assert!(writer.replace(&free, &chosen, &none()).await.expect("written"));
    assert_eq!(read(&beside(&free, "txt")), "chosen words\n");
    let empty = LyricsResult::new("KuGou", None, Some(" ".into()), false);
    assert!(!writer.replace(&free, &empty, &none()).await.expect("checked"));
}

#[tokio::test]
async fn look_up_ranks_the_songs_own_lyrics_and_saves_nothing() {
    let song = Song::new();
    let audio = song.mp3("Artist - Song.mp3", None);
    let writer = song.kugou_writer(
        LyricsSaveTo::BESIDE,
        LyricsResult::new("KuGou", Some(LINES.into()), None, false),
    );

    let lookup = writer.look_up(&job(&audio), LyricsTiming::Word, &none()).await;

    assert!(lookup.result.expect("an answer").is_songs_own());
    assert!(!beside(&audio, "lrc").exists());
}

// ---- The queue worker -----------------------------------------------------------------------

#[tokio::test]
async fn the_worker_writes_what_is_queued() {
    let song = Song::new();
    let audio = song.mp3("Artist - Song.mp3", None);
    let writer = Arc::new(song.lrclib_writer(|| found("lrclib", Some("[00:01.00]line"), None)));
    let stopping = CancellationToken::new();
    let worker = tokio::spawn(writer.clone().run(stopping.clone()));

    assert!(writer.try_enqueue(job(&audio)));
    let lrc = beside(&audio, "lrc");
    for _ in 0..200 {
        if lrc.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(read(&lrc), format!("{MARK}\n[00:01.00]line\n"));
    stopping.cancel();
    worker.await.expect("joined").expect("stopped cleanly");
}

#[tokio::test(start_paused = true)]
async fn the_worker_tries_a_busy_service_three_times_ten_minutes_apart() {
    let song = Song::new();
    let audio = song.mp3("Artist - Song.mp3", None);
    let busy = FakeSource::new("lrclib", LyricsLookup::failed);
    let writer = Arc::new(song.writer("lrclib", LyricsSaveTo::BESIDE, busy.clone()));
    let stopping = CancellationToken::new();
    let worker = tokio::spawn(writer.clone().run(stopping.clone()));

    assert!(writer.try_enqueue(job(&audio)));
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(busy.calls(), 1);
    tokio::time::sleep(LyricsSidecarWriter::RETRY_DELAY).await;
    assert_eq!(busy.calls(), 2);
    tokio::time::sleep(LyricsSidecarWriter::RETRY_DELAY * 3).await;
    // The third try gives up.
    assert_eq!(busy.calls(), 3);

    stopping.cancel();
    worker.await.expect("joined").expect("stopped cleanly");
}

#[test]
fn a_full_queue_drops_the_job() {
    let song = Song::new();
    let writer = song.lrclib_writer(LyricsLookup::miss);
    for n in 0..256 {
        assert!(writer.try_enqueue(job(&song.root.join(format!("{n}.mp3")))));
    }
    assert!(!writer.try_enqueue(job(&song.root.join("one too many.mp3"))));
}

// ---- Jobs that wait while the library tidy moves their songs ------------------------------

/// A song in its own folder under the root.
fn in_folder(song: &Song, folder: &str, name: &str) -> PathBuf {
    std::fs::create_dir_all(song.root.join(folder)).expect("a folder");
    song.mp3(&format!("{folder}/{name}"), None)
}

/// Moves a folder under the root as the tidy does, answering the move as the tidy records it.
fn move_folder(song: &Song, from: &str, to: &str) -> (String, String) {
    let (from, to) = (song.root.join(from), song.root.join(to));
    std::fs::rename(&from, &to).expect("moved");
    (
        from.to_string_lossy().into_owned(),
        to.to_string_lossy().into_owned(),
    )
}

/// Waits up to two seconds for `path` to be there.
async fn appears(path: &Path) -> bool {
    for _ in 0..100 {
        if path.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    path.exists()
}

fn pair(from: &str, to: &str) -> (String, String) {
    (from.to_string(), to.to_string())
}

/// A job still in the queue when the tidy moves its song's folder writes the lyrics where the
/// song is now, and lets go of the move once it is done.
#[tokio::test]
async fn queued_jobs_follow_a_move() {
    let song = Song::new();
    let audio = in_folder(&song, "Old", "Artist - Song.mp3");
    let writer = Arc::new(song.lrclib_writer(|| found("lrclib", Some("[00:01.00]line"), None)));
    let remap = writer.path_remap();
    assert!(writer.try_enqueue(job(&audio)));

    remap.record(&[move_folder(&song, "Old", "New")]);
    assert_eq!(remap.kept(), 1, "the waiting job needs the move");
    let stopping = CancellationToken::new();
    let worker = tokio::spawn(writer.clone().run(stopping.clone()));

    let lrc = song.root.join("New").join("Artist - Song.lrc");
    assert!(
        appears(&lrc).await,
        "the lyrics are beside the song where it is now"
    );
    assert_eq!(read(&lrc), format!("{MARK}\n[00:01.00]line\n"));
    for _ in 0..100 {
        if remap.waiting() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        (remap.waiting(), remap.kept()),
        (0, 0),
        "nothing waits, so nothing is kept"
    );
    stopping.cancel();
    worker.await.expect("joined").expect("stopped cleanly");
}

/// A job the worker took from the queue before the tidy started waits for its share of the gate,
/// and once it has it, follows the move the tidy made meanwhile.
#[tokio::test]
async fn a_job_dequeued_before_a_move_lands_at_the_new_path() {
    let song = Song::new();
    let audio = in_folder(&song, "Old", "Artist - Song.mp3");
    let writer = Arc::new(song.lrclib_writer(|| found("lrclib", Some("[00:01.00]line"), None)));
    let gate = LibraryWriteGate::new();
    writer.set_write_gate(gate.clone());
    let tidy = gate.begin_job("The library tidy").await.expect("the gate");
    let stopping = CancellationToken::new();
    let worker = tokio::spawn(writer.clone().run(stopping.clone()));

    assert!(writer.try_enqueue(job(&audio)));
    for _ in 0..100 {
        if writer.queued() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        writer.queued(),
        0,
        "the worker took the job and waits for its share"
    );
    writer.path_remap().record(&[move_folder(&song, "Old", "New")]);
    drop(tidy);

    let lrc = song.root.join("New").join("Artist - Song.lrc");
    assert!(
        appears(&lrc).await,
        "the lyrics are beside the song where it is now"
    );
    assert!(!song.root.join("Old").exists());
    stopping.cancel();
    worker.await.expect("joined").expect("stopped cleanly");
}

/// A move is kept while a job older than it waits, and goes once none does: a job queued after
/// a move never needs it.
#[test]
fn the_remap_is_dropped_once_no_older_job_waits() {
    let remap = LyricsPathRemap::new();
    remap.record(&[pair("/m/a", "/m/b")]);
    assert_eq!(remap.kept(), 0, "no job waits, so the move is not kept");

    let older = remap.hold_for_test();
    remap.record(&[pair("/m/b", "/m/c")]);
    let newer = remap.hold_for_test();
    remap.record(&[pair("/m/c", "/m/d")]);
    assert_eq!(remap.kept(), 2);
    assert_eq!(remap.map("/m/b/x.mp3"), "/m/d/x.mp3", "every move, oldest first");
    assert_eq!(remap.map("/m/other/x.mp3"), "/m/other/x.mp3");

    remap.release_for_test(older);
    assert_eq!(remap.kept(), 1, "only the newer job's move is left");
    assert_eq!(remap.map("/m/b/x.mp3"), "/m/b/x.mp3");
    assert_eq!(remap.map("/m/c/x.mp3"), "/m/d/x.mp3");
    remap.release_for_test(newer);
    assert_eq!((remap.kept(), remap.waiting()), (0, 0));
}

/// A job tried again follows each move made while it waited, in the order they were made: the
/// folder moved, then moved on and back (a move and its undo), and the lyrics land where the
/// song is.
#[tokio::test(start_paused = true)]
async fn a_retry_follows_a_move_then_its_undo() {
    let song = Song::new();
    let audio = in_folder(&song, "A", "Artist - Song.mp3");
    let tries = Arc::new(AtomicUsize::new(0));
    let source = FakeSource::new("lrclib", {
        let tries = tries.clone();
        move || {
            if tries.fetch_add(1, Ordering::SeqCst) < 2 {
                LyricsLookup::failed()
            } else {
                found("lrclib", Some("[00:01.00]line"), None)
            }
        }
    });
    let delay = Duration::from_secs(5);
    let writer = Arc::new(
        song.writer("lrclib", LyricsSaveTo::BESIDE, source.clone())
            .with_retry_delay(delay),
    );
    let gate = LibraryWriteGate::new();
    writer.set_write_gate(gate.clone());
    let remap = writer.path_remap();
    let stopping = CancellationToken::new();
    let worker = tokio::spawn(writer.clone().run(stopping.clone()));

    assert!(writer.try_enqueue(job(&audio)));
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(source.calls(), 1, "the first try found no service answering");
    {
        let _tidy = gate.begin_job("The library tidy").await.expect("the gate");
        remap.record(&[move_folder(&song, "A", "B")]);
    }
    tokio::time::sleep(delay).await;
    assert_eq!(source.calls(), 2, "the second try found the song where it moved");
    {
        let _tidy = gate.begin_job("The library tidy").await.expect("the gate");
        remap.record(&[move_folder(&song, "B", "C"), move_folder(&song, "C", "B")]);
    }
    tokio::time::sleep(delay).await;

    let lrc = song.root.join("B").join("Artist - Song.lrc");
    assert!(
        appears(&lrc).await,
        "the lyrics are beside the song where it is now"
    );
    assert_eq!(source.calls(), 3);
    stopping.cancel();
    worker.await.expect("joined").expect("stopped cleanly");
}

/// A try again that finds the queue full is dropped, counted, and lets go of its age, so the
/// moves it waited on are not kept for it.
#[tokio::test(start_paused = true)]
async fn a_retry_the_full_queue_drops_is_counted_and_lets_go() {
    let song = Song::new();
    let audio = song.mp3("Artist - Song.mp3", None);
    let busy = FakeSource::new("lrclib", LyricsLookup::failed);
    let delay = Duration::from_secs(5);
    let writer = Arc::new(
        song.writer("lrclib", LyricsSaveTo::BESIDE, busy.clone())
            .with_retry_delay(delay),
    );
    let gate = LibraryWriteGate::new();
    writer.set_write_gate(gate.clone());
    let remap = writer.path_remap();
    let stopping = CancellationToken::new();
    let worker = tokio::spawn(writer.clone().run(stopping.clone()));

    assert!(writer.try_enqueue(job(&audio)));
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(busy.calls(), 1);
    // The tidy holds the gate, so the worker waits with the next job and the queue fills.
    let tidy = gate.begin_job("The library tidy").await.expect("the gate");
    assert!(writer.try_enqueue(job(&song.root.join("next.mp3"))));
    for _ in 0..50 {
        if writer.queued() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for n in 0..256 {
        assert!(writer.try_enqueue(job(&song.root.join(format!("{n}.mp3")))));
    }
    assert_eq!(
        remap.waiting(),
        258,
        "the try again, the job taken and 256 queued"
    );

    tokio::time::sleep(delay).await;
    assert_eq!(writer.dropped_retries(), 1);
    assert_eq!(remap.waiting(), 257, "the dropped try again let go of its age");

    drop(tidy);
    stopping.cancel();
    worker.await.expect("joined").expect("stopped cleanly");
}

/// Counts the scans asked for.
type CountingLibrary = crate::services::local::test_support::FakeLocalLibrary;

#[tokio::test(start_paused = true)]
async fn lyrics_written_inside_ask_for_one_scan_a_little_later() {
    let song = Song::new();
    let first = song.mp3("01 A.mp3", None);
    let second = song.mp3("02 B.mp3", None);
    let writer = song.kugou_writer(
        LyricsSaveTo::INSIDE,
        LyricsResult::new("KuGou", Some(WORDS.into()), None, false),
    );
    let library = Arc::new(CountingLibrary::default());
    writer.set_library(library.clone());

    writer.write(&job(&first), false, &none()).await.expect("written");
    writer
        .write(&job(&second), false, &none())
        .await
        .expect("written");
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(library.scans(), 0);
    tokio::time::sleep(LyricsSidecarWriter::SCAN_DELAY).await;
    assert_eq!(library.scans(), 1);
}

#[test]
fn is_octos_reads_the_first_line() {
    let song = Song::new();
    let lrc = song.root.join("a.lrc");
    std::fs::write(&lrc, format!("\u{FEFF} {MARK} \r\n[00:01.00]x")).expect("written");
    assert!(LyricsSidecarWriter::is_octos(&lrc));
    std::fs::write(&lrc, "[00:01.00]x").expect("written");
    assert!(!LyricsSidecarWriter::is_octos(&lrc));
}
