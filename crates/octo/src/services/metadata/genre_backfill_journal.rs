//! The genre backfill's undo journal (`genre-backfill-journal.jsonl`).

use std::io::Write;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use octo_core::common::dotnet::is_blank;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::services::state_file::{self, null_as_default};

/// One changed genre frame. Short property names because this file gets one line per changed
/// file and a whole-library run writes thousands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenreJournalEntry {
    #[serde(rename = "p", default, deserialize_with = "null_as_default")]
    pub path: String,
    #[serde(rename = "b", default, deserialize_with = "null_as_default")]
    pub before: Vec<String>,
    #[serde(rename = "a", default, deserialize_with = "null_as_default")]
    pub after: Vec<String>,
    #[serde(
        rename = "t",
        with = "octo_core::json::datetime::utc",
        default = "octo_core::json::datetime::min_value"
    )]
    pub at_utc: DateTime<Utc>,
    #[serde(rename = "r", default, deserialize_with = "null_as_default")]
    pub run_id: String,
}

impl GenreJournalEntry {
    pub fn new(
        path: impl Into<String>,
        before: Vec<String>,
        after: Vec<String>,
        at_utc: DateTime<Utc>,
        run_id: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            before,
            after,
            at_utc,
            run_id: run_id.into(),
        }
    }
}

/// Append-only record of every genre frame the backfill changed, and the only thing that makes
/// an apply reversible in practice rather than in principle.
///
/// One JSON object per line rather than one array, so an interrupted run leaves a readable
/// file instead of an unparseable one, and appending never rewrites what is already there.
/// A torn final line is skipped on read rather than failing the whole journal.
///
/// What this CANNOT undo, and the UI has to say so:
///  - Saving a file's tags rewrites the whole tag block. Anything it does not round-trip was lost
///    on the first save and no amount of journal replay brings it back.
///  - Entries are keyed by path, so a file moved or renamed since the run stays rewritten.
///  - No journal, no undo. /app/config is a bind mount the user may not back up.
pub struct GenreBackfillJournal {
    path: Option<PathBuf>,
    lock: Mutex<()>,
}

impl GenreBackfillJournal {
    /// `path` None (or blank) records nothing.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path: path.filter(|path| !is_blank(&path.to_string_lossy())),
            lock: Mutex::new(()),
        }
    }

    pub fn exists(&self) -> bool {
        self.path.as_ref().is_some_and(|path| path.exists())
    }

    pub fn append(&self, entry: &GenreJournalEntry) {
        let Some(path) = &self.path else {
            return;
        };
        let line = octo_core::json::to_string(entry) + "\n";
        let appended = {
            let _guard = self.lock.lock();
            path.parent()
                .filter(|dir| !dir.as_os_str().is_empty())
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::OpenOptions::new().create(true).append(true).open(path))
                .and_then(|mut file| file.write_all(line.as_bytes()))
        };
        if let Err(error) = appended {
            // A journal write that fails costs the undo for that one file. It must never stop
            // the run, but it is a Warning because it silently reduces what can be recovered.
            warn!("genre backfill journal could not record {}: {error}", entry.path);
        }
    }

    /// Newest first, which is the order an undo replays them in: if a file was changed twice,
    /// the oldest entry holds the frame it started with, so the LAST one applied wins.
    pub fn read_all(&self) -> Vec<GenreJournalEntry> {
        let Some(path) = self.path.as_ref().filter(|path| path.exists()) else {
            return Vec::new();
        };

        let mut entries = Vec::new();
        let mut skipped = 0;
        let text = {
            let _guard = self.lock.lock();
            state_file::read_all_text(path)
        };
        let text = match text {
            Ok(text) => text,
            Err(error) => {
                warn!("genre backfill journal could not be read: {error}");
                return entries;
            }
        };
        for line in state_file::lines(&text) {
            if is_blank(line) {
                continue;
            }
            match serde_json::from_str::<Option<GenreJournalEntry>>(line) {
                Ok(Some(entry)) if !entry.path.is_empty() => entries.push(entry),
                Ok(_) => {}
                Err(_) => skipped += 1,
            }
        }

        // A run killed mid-append leaves a partial last line. Skipping it costs the undo for
        // one file; refusing to parse the file would cost the undo for all of them.
        if skipped > 0 {
            warn!("genre backfill journal had {skipped} unreadable line(s), skipped");
        }

        entries.reverse();
        entries
    }

    /// Replace the journal with these entries, oldest first. Used after a partial undo, so what
    /// was not restored stays undoable instead of vanishing with the entries that were. Written
    /// to a temporary file and moved into place, so a crash mid-write leaves the old journal.
    pub fn rewrite(&self, oldest_first: &[GenreJournalEntry]) {
        let Some(path) = &self.path else {
            return;
        };
        if oldest_first.is_empty() {
            self.clear();
            return;
        }

        // File.WriteAllLines: every line ends with a newline.
        let text: String = oldest_first
            .iter()
            .map(|entry| octo_core::json::to_string(entry) + "\n")
            .collect();
        let written = {
            let _guard = self.lock.lock();
            state_file::write_atomic(path, text.as_bytes())
        };
        if let Err(error) = written {
            warn!("genre backfill journal could not be rewritten: {error}");
        }
    }

    /// Follow a folder the library tidy moved, so an undo of a genre run still finds its
    /// files. Answers how many entries followed.
    pub fn move_paths(&self, from: &str, to: &str) -> usize {
        self.move_paths_batch(&[(from.to_string(), to.to_string())])
    }

    /// [`Self::move_paths`] for each `(from, to)` in order, so `a` to `b` then `b` to `c` ends at
    /// `c`, with the journal read and rewritten once. Answers how many entries followed, each
    /// move counted on its own.
    pub fn move_paths_batch(&self, moves: &[(String, String)]) -> usize {
        if moves.is_empty() || self.path.is_none() || !self.exists() {
            return 0;
        }
        let mut oldest_first = self.read_all();
        oldest_first.reverse();
        let mut moved = 0;
        for (from, to) in moves {
            for entry in &mut oldest_first {
                if let Some(now) = octo_core::common::PathHelper::moved_path(&entry.path, from, to) {
                    entry.path = now;
                    moved += 1;
                }
            }
        }
        if moved > 0 {
            self.rewrite(&oldest_first);
        }
        moved
    }

    pub fn clear(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let _guard = self.lock.lock();
        if path.exists()
            && let Err(error) = std::fs::remove_file(path)
        {
            warn!("genre backfill journal could not be cleared: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal() -> (tempfile::TempDir, PathBuf, GenreBackfillJournal) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("genre-backfill-journal.jsonl");
        let journal = GenreBackfillJournal::new(Some(path.clone()));
        (dir, path, journal)
    }

    fn entry(path: &str, before: &[&str], after: &[&str]) -> GenreJournalEntry {
        GenreJournalEntry::new(
            path,
            before.iter().map(|value| value.to_string()).collect(),
            after.iter().map(|value| value.to_string()).collect(),
            Utc::now(),
            "r1",
        )
    }

    fn fixture() -> String {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/rust-migration/fixtures/state/genre-backfill-journal.jsonl"
        );
        std::fs::read_to_string(path).expect("the fixture is in the repo")
    }

    /// Every line of the fixture reads and writes back byte for byte, and a rewrite of what
    /// was read reproduces the whole file.
    #[test]
    fn genre_backfill_journal_fixture_round_trips_byte_for_byte() {
        let text = fixture();
        for line in text.lines() {
            let entry: GenreJournalEntry = serde_json::from_str(line).expect("a line reads");
            assert_eq!(octo_core::json::to_string(&entry), line);
        }

        let (_dir, path, journal) = journal();
        std::fs::write(&path, &text).expect("copied");
        let mut entries = journal.read_all();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].before.is_empty(), "newest first");
        entries.reverse();
        journal.rewrite(&entries);
        assert_eq!(std::fs::read_to_string(&path).expect("rewritten"), text);
    }

    #[test]
    fn read_all_returns_newest_first() {
        let (_dir, _path, journal) = journal();
        journal.append(&entry("/music/a.flac", &["Music"], &["Pop"]));
        journal.append(&entry("/music/b.flac", &[], &["Rock"]));

        let entries = journal.read_all();
        assert_eq!(entries.len(), 2);
        // Newest first, so an undo replaying in order applies the OLDEST entry last and a
        // file changed twice ends up with the frame it originally had.
        assert_eq!(entries[0].path, "/music/b.flac");
        assert_eq!(entries[1].path, "/music/a.flac");
        assert_eq!(entries[1].before, ["Music"]);
    }

    #[test]
    fn read_all_torn_final_line_is_skipped_not_fatal() {
        let (_dir, path, journal) = journal();
        journal.append(&entry("/music/a.flac", &["Music"], &["Pop"]));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("opened");
        file.write_all(b"{\"p\":\"/music/b.flac\",\"b\":[\"Ro")
            .expect("appended");

        let entries = journal.read_all();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "/music/a.flac");
    }

    #[test]
    fn clear_removes_the_journal_and_with_it_the_undo() {
        let (_dir, _path, journal) = journal();
        journal.append(&entry("/music/a.flac", &["Music"], &["Pop"]));
        assert!(journal.exists());

        journal.clear();
        assert!(!journal.exists());
        assert!(journal.read_all().is_empty());
    }

    #[test]
    fn read_all_no_journal_is_empty_rather_than_throwing() {
        let (_dir, _path, journal) = journal();
        assert!(journal.read_all().is_empty());
    }

    /// An empty `before` means the file had no genre frame, so undo clears it.
    #[test]
    fn entry_empty_before_round_trips() {
        let (_dir, _path, journal) = journal();
        journal.append(&entry("/music/a.flac", &[], &["Pop"]));

        let entries = journal.read_all();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].before.is_empty());
    }

    #[test]
    fn blank_null_and_pathless_lines_are_passed_over_and_an_empty_rewrite_clears() {
        let (_dir, path, journal) = journal();
        std::fs::write(
            &path,
            "\n  \r\nnull\n{\"p\":\"\",\"b\":[],\"a\":[],\"t\":\"2026-10-03T14:25:00Z\",\"r\":\"x\"}\r{\"p\":\"/m/a.flac\"}\n5\n",
        )
        .expect("written");
        let entries = journal.read_all();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "/m/a.flac");

        journal.rewrite(&[]);
        assert!(!journal.exists());
        // Without a path nothing is recorded or read.
        let nowhere = GenreBackfillJournal::new(None);
        nowhere.append(&entry("/m/a.flac", &[], &[]));
        assert!(!nowhere.exists() && nowhere.read_all().is_empty());
    }
}
