//! The lyrics undo journal (`lyrics-undo.jsonl`): every write a Save made, so Undo can put it
//! back.

use std::io::{self, Write};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use octo_core::common::dotnet::is_blank;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::services::state_file::{self, null_as_default};

/// One write a Save made. `Kind` is [`LyricsUndoJournal::BESIDE`] (`Path` is the lyrics file,
/// `Before` its earlier text or None when there was none) or
/// [`LyricsUndoJournal::INSIDE`] (`Path` is the song, `Before` its earlier lyrics tag). Written
/// with full property names, unlike the other journals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct LyricsUndoEntry {
    // The required strings read a JSON null (or nothing) as empty.
    #[serde(default, deserialize_with = "null_as_default")]
    pub path: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub kind: String,
    #[serde(default)]
    pub before: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub run_id: String,
    #[serde(
        with = "octo_core::json::datetime::utc",
        default = "octo_core::json::datetime::min_value"
    )]
    pub at_utc: DateTime<Utc>,
}

impl LyricsUndoEntry {
    pub fn new(
        path: impl Into<String>,
        kind: impl Into<String>,
        before: Option<String>,
        run_id: impl Into<String>,
        at_utc: DateTime<Utc>,
    ) -> Self {
        Self {
            path: path.into(),
            kind: kind.into(),
            before,
            run_id: run_id.into(),
            at_utc,
        }
    }
}

/// What Save wrote over, so Undo can put it back: a lyrics file beside a song (Before None
/// when there was none) or the lyrics in its tags. Kept beside the run state, one line per
/// write, appended and never rewritten.
pub struct LyricsUndoJournal {
    path: Option<PathBuf>,
    /// The entries when there is no file (tests), and the lock every file access takes.
    memory: Mutex<Vec<LyricsUndoEntry>>,
}

impl LyricsUndoJournal {
    pub const BESIDE: &'static str = "beside";
    pub const INSIDE: &'static str = "inside";

    /// `path` None (or blank) keeps the entries in memory.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path: path.filter(|path| !is_blank(&path.to_string_lossy())),
            memory: Mutex::new(Vec::new()),
        }
    }

    pub fn has_entries(&self) -> bool {
        let memory = self.memory.lock();
        match &self.path {
            None => !memory.is_empty(),
            Some(path) => std::fs::metadata(path).is_ok_and(|file| file.is_file() && file.len() > 0),
        }
    }

    /// Appends one entry, stamped now. An error (the folder cannot be made, the disk is full)
    /// is the caller's: a Save must not go ahead without its undo.
    pub fn record(&self, path: &str, kind: &str, before: Option<&str>, run_id: &str) -> io::Result<()> {
        let entry = LyricsUndoEntry::new(path, kind, before.map(str::to_string), run_id, Utc::now());
        let mut memory = self.memory.lock();
        let Some(file) = &self.path else {
            memory.push(entry);
            return Ok(());
        };
        if let Some(dir) = file.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let line = octo_core::json::to_string(&entry) + "\n";
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)?
            .write_all(line.as_bytes())
    }

    /// Every entry, oldest first. Blank lines and lines that do not read are skipped.
    pub fn read_all(&self) -> io::Result<Vec<LyricsUndoEntry>> {
        let memory = self.memory.lock();
        let Some(path) = &self.path else {
            return Ok(memory.clone());
        };
        if !path.exists() {
            return Ok(Vec::new());
        }
        let text = state_file::read_all_text(path)?;
        Ok(state_file::lines(&text)
            .into_iter()
            .filter(|line| !line.is_empty())
            .filter_map(|line| {
                serde_json::from_str::<Option<LyricsUndoEntry>>(line)
                    .ok()
                    .flatten()
            })
            .collect())
    }

    /// Follow a folder the library tidy moved: each entry names where its file is now. Answers
    /// how many entries followed.
    pub fn move_paths(&self, from: &str, to: &str) -> io::Result<usize> {
        self.move_paths_batch(&[(from.to_string(), to.to_string())])
    }

    /// [`Self::move_paths`] for each `(from, to)` in order, so `a` to `b` then `b` to `c` ends at
    /// `c`, with the journal read and written once. Answers how many entries followed, each move
    /// counted on its own.
    pub fn move_paths_batch(&self, moves: &[(String, String)]) -> io::Result<usize> {
        if moves.is_empty() {
            return Ok(0);
        }
        let mut entries = self.read_all()?;
        let mut moved = 0;
        for (from, to) in moves {
            for entry in &mut entries {
                if let Some(now) = octo_core::common::PathHelper::moved_path(&entry.path, from, to) {
                    entry.path = now;
                    moved += 1;
                }
            }
        }
        if moved == 0 {
            return Ok(0);
        }
        let mut memory = self.memory.lock();
        match &self.path {
            None => *memory = entries,
            Some(path) => {
                let text: String = entries
                    .iter()
                    .map(|entry| octo_core::json::to_string(entry) + "\n")
                    .collect();
                state_file::write_atomic(path, text.as_bytes())?;
            }
        }
        Ok(moved)
    }

    pub fn clear(&self) -> io::Result<()> {
        let mut memory = self.memory.lock();
        memory.clear();
        match &self.path {
            Some(path) if path.exists() => std::fs::remove_file(path),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> String {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/rust-migration/fixtures/state/lyrics-undo.jsonl"
        );
        std::fs::read_to_string(path).expect("the fixture is in the repo")
    }

    /// Every line of the fixture reads, and is written back byte for byte.
    #[test]
    fn the_fixture_round_trips_byte_for_byte() {
        let text = fixture();
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("lyrics-undo.jsonl");
        std::fs::write(&path, &text).expect("copied");

        let entries = LyricsUndoJournal::new(Some(path)).read_all().expect("read");

        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].path,
            "/music/宇多田ヒカル/First Love/01 - First Love.lrc"
        );
        assert_eq!(entries[0].kind, LyricsUndoJournal::BESIDE);
        assert_eq!(entries[0].before, None);
        assert_eq!(entries[1].kind, LyricsUndoJournal::INSIDE);
        assert_eq!(
            entries[1].before.as_deref(),
            Some("Tonight I'm gonna have myself\nA real good time")
        );
        assert_eq!(entries[1].run_id, "ly-20261003-2005");
        let written: String = entries
            .iter()
            .map(|entry| octo_core::json::to_string(entry) + "\n")
            .collect();
        assert_eq!(written, text);
    }

    #[test]
    fn record_appends_and_read_all_skips_what_does_not_read() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("sub").join("lyrics-undo.jsonl");
        let journal = LyricsUndoJournal::new(Some(path.clone()));
        assert!(!journal.has_entries());
        assert!(journal.read_all().expect("read").is_empty());

        journal
            .record("/m/a.lrc", LyricsUndoJournal::BESIDE, None, "run-1")
            .expect("recorded");
        journal
            .record("/m/b.mp3", LyricsUndoJournal::INSIDE, Some("old"), "run-1")
            .expect("recorded");
        let mut text = std::fs::read_to_string(&path).expect("written");
        text.push_str("\n{\"Path\":\"torn\n{\"Path\":null,\"Kind\":\"beside\"}\r\nnull\n");
        std::fs::write(&path, text).expect("written");

        assert!(journal.has_entries());
        let entries = journal.read_all().expect("read");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, "/m/a.lrc");
        assert_eq!(entries[1].before.as_deref(), Some("old"));
        // A null path reads as empty and the entry is still kept.
        assert_eq!(entries[2].path, "");
        assert_eq!(entries[2].at_utc, octo_core::json::datetime::min_value());

        journal.clear().expect("cleared");
        assert!(!path.exists());
        assert!(!journal.has_entries());
        journal.clear().expect("clearing nothing is fine");
    }

    #[test]
    fn without_a_path_the_entries_stay_in_memory() {
        let journal = LyricsUndoJournal::new(None);
        assert!(!journal.has_entries());
        journal
            .record("/m/a.txt", LyricsUndoJournal::BESIDE, Some("x"), "r")
            .expect("recorded");
        assert!(journal.has_entries());
        assert_eq!(journal.read_all().expect("read")[0].before.as_deref(), Some("x"));
        journal.clear().expect("cleared");
        assert!(!journal.has_entries());
    }
}
