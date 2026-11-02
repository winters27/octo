//! The download history, the store behind `downloads-history.json`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};

use chrono::{DateTime, TimeDelta, Utc};
use octo_core::models::download::acquisition_event::acquisition_event_kinds;
use octo_core::models::download::{AcquisitionEvent, DownloadHistoryEntry};
use parking_lot::Mutex;
use tracing::warn;

use crate::services::state_file;

/// Persistent, bounded log of songs Octo has fetched. Written to a JSON file next
/// to the settings file so it survives restarts and container recreation (the
/// config dir is a host bind-mount). Newest entries first; capped so it can't grow
/// without limit. Best-effort: a write failure never breaks a download.
pub struct DownloadHistoryService {
    path: PathBuf,
    /// Loaded on first use.
    cache: Mutex<Option<Vec<DownloadHistoryEntry>>>,
    /// [`Self::MAX_LOG_BYTES`], smaller in tests.
    log_room: AtomicI64,
}

impl DownloadHistoryService {
    pub const MAX_ENTRIES: usize = 500;

    /// Lines kept per saved log. The first line stays, and the newest ones after it.
    pub const MAX_LOG_LINES: usize = 80;

    /// Copies kept on one saved line: the best ones, which are what a search line is read for.
    pub const MAX_SAVED_CANDIDATES: usize = 12;

    /// Longest text, detail or file name kept on a saved line.
    pub const MAX_SAVED_TEXT: usize = 300;

    /// Room for every saved log together. Past it, the oldest entries let go of their logs (the
    /// entries themselves stay), so the file stays small enough to read whole on every change.
    pub const MAX_LOG_BYTES: i64 = 2 * 1024 * 1024;

    pub fn new(path: impl AsRef<Path>) -> Self {
        DownloadHistoryService {
            path: path.as_ref().to_path_buf(),
            cache: Mutex::new(None),
            log_room: AtomicI64::new(Self::MAX_LOG_BYTES),
        }
    }

    /// The room for every saved log together (set by tests).
    pub fn set_log_room(&self, bytes: i64) {
        self.log_room.store(bytes, Ordering::Relaxed);
    }

    // ---- A download's log, kept with its entry (the dashboard's Fetched songs) ----------------

    /// Saves a download's log with the entry it made: the newest entry with this key written
    /// since the run began, so a log never lands on an older fetch of the same song. Called
    /// again when a line arrives after the end (lyrics, an upgrade's verdict), and each time the
    /// whole log is saved anew. False when there is no such entry.
    pub fn attach_log(&self, key: &str, started_utc: DateTime<Utc>, events: &[AcquisitionEvent]) -> bool {
        if octo_core::common::dotnet::is_blank(key) || events.is_empty() {
            return false;
        }
        let mut cache = self.cache.lock();
        let list = self.load_locked(&mut cache);
        let since = started_utc - TimeDelta::seconds(1);
        let Some(entry) = list
            .iter_mut()
            .find(|e| e.key.as_deref() == Some(key) && Self::written_at(e).is_some_and(|at| at >= since))
        else {
            return false;
        };
        entry.log = Some(Self::trimmed(events));
        self.keep_logs_within_room(list);
        self.save_locked(list);
        true
    }

    /// The entry written at this moment for this key, as listed (DownloadedAt), or None.
    pub fn find(&self, key: &str, downloaded_at: Option<&str>) -> Option<DownloadHistoryEntry> {
        if octo_core::common::dotnet::is_blank(key) {
            return None;
        }
        let mut cache = self.cache.lock();
        let list = self.load_locked(&mut cache);
        list.iter()
            .find(|e| e.key.as_deref() == Some(key) && downloaded_at.is_none_or(|at| e.downloaded_at == at))
            .cloned()
    }

    /// When an entry was written, from its DownloadedAt; None when that does not read.
    pub fn written_at(entry: &DownloadHistoryEntry) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(entry.downloaded_at.trim())
            .ok()
            .map(|at| at.with_timezone(&Utc))
            .or_else(|| {
                chrono::NaiveDateTime::parse_from_str(entry.downloaded_at.trim(), "%Y-%m-%dT%H:%M:%S%.f")
                    .ok()
                    .map(|at| at.and_utc())
            })
    }

    /// A log as it is saved: at most [`Self::MAX_LOG_LINES`] lines (the first, a line saying how
    /// many were left out, and the newest), each line's words and copies capped.
    pub fn trimmed(events: &[AcquisitionEvent]) -> Vec<AcquisitionEvent> {
        fn cap(text: Option<&str>) -> Option<String> {
            let text = text?;
            if octo_core::common::dotnet::utf16_len(text) <= DownloadHistoryService::MAX_SAVED_TEXT {
                return Some(text.to_string());
            }
            Some(format!(
                "{}...",
                utf16_prefix(text, DownloadHistoryService::MAX_SAVED_TEXT - 3).trim_end()
            ))
        }
        fn line(event: &AcquisitionEvent) -> AcquisitionEvent {
            AcquisitionEvent {
                text: cap(Some(&event.text)).unwrap_or_default(),
                detail: cap(event.detail.as_deref()),
                candidates: event
                    .candidates
                    .as_ref()
                    .filter(|copies| !copies.is_empty())
                    .map(|copies| {
                        copies
                            .iter()
                            .take(DownloadHistoryService::MAX_SAVED_CANDIDATES)
                            .map(|copy| {
                                let mut copy = copy.clone();
                                copy.file = cap(copy.file.as_deref());
                                copy.folder = cap(copy.folder.as_deref());
                                copy.note = cap(copy.note.as_deref());
                                copy
                            })
                            .collect()
                    }),
                ..event.clone()
            }
        }

        if events.len() <= Self::MAX_LOG_LINES {
            return events.iter().map(line).collect();
        }
        let left = events.len() - (Self::MAX_LOG_LINES - 1);
        let tail = &events[events.len() - (Self::MAX_LOG_LINES - 2)..];
        let mut kept = vec![line(&events[0])];
        kept.push(AcquisitionEvent {
            at: tail[0].at,
            kind: acquisition_event_kinds::NOTE.to_string(),
            text: format!(
                "{left} {} left out to keep the saved log short",
                if left == 1 { "line" } else { "lines" }
            ),
            detail: None,
            candidates: None,
            heard: None,
        });
        kept.extend(tail.iter().map(line));
        kept
    }

    /// The newest entries keep their logs; once they fill the room, older ones let go. Caller
    /// holds the lock.
    fn keep_logs_within_room(&self, list: &mut [DownloadHistoryEntry]) {
        let room = self.log_room.load(Ordering::Relaxed);
        let mut used: i64 = 0;
        let mut full = false;
        for entry in list.iter_mut() {
            let Some(log) = &entry.log else {
                continue;
            };
            if !full {
                let size = octo_core::json::to_string_without_nulls(log).len() as i64;
                if used + size <= room {
                    used += size;
                    continue;
                }
                full = true;
            }
            entry.log = None;
        }
    }

    /// Follow a folder the library tidy moved, so a fetched song's row names where it is now.
    /// Answers how many entries followed.
    pub fn move_paths(&self, from: &str, to: &str) -> usize {
        let mut cache = self.cache.lock();
        let list = self.load_locked(&mut cache);
        let mut moved = 0;
        for entry in list.iter_mut() {
            if let Some(now) = octo_core::common::PathHelper::moved_path(&entry.path, from, to) {
                entry.path = now;
                moved += 1;
            }
        }
        if moved > 0 {
            self.save_locked(list);
        }
        moved
    }

    /// Append a fetched-song entry (newest first) and persist.
    pub fn record(&self, entry: DownloadHistoryEntry) {
        let mut cache = self.cache.lock();
        let list = self.load_locked(&mut cache);
        list.insert(0, entry);
        list.truncate(Self::MAX_ENTRIES);
        self.save_locked(list);
    }

    /// The most recent entries, newest first.
    pub fn get_recent(&self, limit: i32) -> Vec<DownloadHistoryEntry> {
        let mut cache = self.cache.lock();
        let list = self.load_locked(&mut cache);
        list.iter().take(limit.max(0) as usize).cloned().collect()
    }

    /// The entries whose artist, title, album or file path hold every word of `text`, ignoring
    /// case, newest first, at most `limit`. Searches every kept entry, not only the newest
    /// that the Fetched songs list shows. Blank text finds nothing.
    pub fn search(&self, text: &str, limit: usize) -> Vec<DownloadHistoryEntry> {
        let words: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
        if words.is_empty() {
            return Vec::new();
        }
        let mut cache = self.cache.lock();
        let list = self.load_locked(&mut cache);
        list.iter()
            .filter(|entry| {
                let album = entry.album.as_deref().unwrap_or_default();
                let said = [entry.artist.as_str(), &entry.title, album, &entry.path]
                    .join("\n")
                    .to_lowercase();
                words.iter().all(|word| said.contains(word.as_str()))
            })
            .take(limit)
            .cloned()
            .collect()
    }

    fn load_locked<'a>(
        &self,
        cache: &'a mut Option<Vec<DownloadHistoryEntry>>,
    ) -> &'a mut Vec<DownloadHistoryEntry> {
        cache.get_or_insert_with(|| {
            let loaded = (|| -> anyhow::Result<Vec<DownloadHistoryEntry>> {
                let Some(json) = state_file::read_text(&self.path)? else {
                    return Ok(Vec::new());
                };
                if json.trim().is_empty() {
                    return Ok(Vec::new());
                }
                Ok(serde_json::from_str::<Option<Vec<DownloadHistoryEntry>>>(&json)?.unwrap_or_default())
            })();
            loaded.unwrap_or_else(|e| {
                warn!("Download history load failed ({e}); starting fresh");
                Vec::new()
            })
        })
    }

    fn save_locked(&self, list: &[DownloadHistoryEntry]) {
        // Written without nulls: a saved log's copies are mostly missing figures, and the file
        // has to stay small enough to read whole.
        if let Err(e) = state_file::save_atomic(&self.path, &octo_core::json::to_string_without_nulls(list)) {
            warn!("Download history save failed: {e}");
        }
    }
}

/// The longest prefix of at most `units` UTF-16 code units (`text[..units]`), never splitting a
/// character.
fn utf16_prefix(text: &str, units: usize) -> &str {
    let mut count = 0;
    for (index, c) in text.char_indices() {
        count += c.len_utf16();
        if count > units {
            return &text[..index];
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/rust-migration/fixtures/state/downloads-history.json"
    );

    fn entry(title: &str) -> DownloadHistoryEntry {
        DownloadHistoryEntry {
            artist: "A".into(),
            title: title.into(),
            downloaded_at: "2026-10-02T09:01:44.0807210Z".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_record_goes_first_and_the_rest_of_the_fixture_is_written_back_unchanged() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("downloads-history.json");
        let text = std::fs::read_to_string(FIXTURE).expect("the fixture is in the repo");
        std::fs::write(&path, &text).expect("writes");

        let service = DownloadHistoryService::new(&path);
        let before = service.get_recent(200).len();
        service.record(entry("New"));

        let written = std::fs::read_to_string(&path).expect("reads");
        let expected_tail = &text.trim_end_matches('\n')[1..];
        assert!(
            written.ends_with(expected_tail),
            "the old entries keep their bytes"
        );
        assert!(written.starts_with(r#"[{"Artist":"A","Title":"New","#));
        assert_eq!(service.get_recent(200).len(), before + 1);
        assert_eq!(service.get_recent(1)[0].title, "New");
        assert!(service.get_recent(-5).is_empty());
    }

    #[test]
    fn the_log_is_capped_at_500_newest_first() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("config").join("downloads-history.json");
        let service = DownloadHistoryService::new(&path);
        for i in 0..505 {
            service.record(entry(&i.to_string()));
        }
        let reopened = DownloadHistoryService::new(&path);
        let all = reopened.get_recent(1000);
        assert_eq!(all.len(), 500);
        assert_eq!(all[0].title, "504");
        assert_eq!(all[499].title, "5");
    }

    #[test]
    fn a_search_finds_every_word_in_any_field_ignoring_case_newest_first() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = DownloadHistoryService::new(dir.path().join("downloads-history.json"));
        for i in 0..300 {
            service.record(entry(&format!("Filler {i}")));
        }
        service.record(DownloadHistoryEntry {
            album: Some("Blue Album".into()),
            path: "/music/A/Blue Album/01 Night Drive.flac".into(),
            ..entry("Night Drive")
        });
        service.record(entry("Night Drive (Live)"));
        for i in 0..250 {
            service.record(entry(&format!("Later {i}")));
        }

        let found = service.search("  night DRIVE ", 50);
        assert_eq!(found.len(), 2, "older than the newest 200 is still found");
        assert_eq!(found[0].title, "Night Drive (Live)");
        assert_eq!(found[1].title, "Night Drive");
        assert_eq!(
            service.search("blue night", 50).len(),
            1,
            "album and title together"
        );
        assert_eq!(
            service.search("01 night drive.flac", 50).len(),
            1,
            "the file path"
        );
        assert!(
            service.search("night blues", 50).is_empty(),
            "every word must be there"
        );
        assert!(service.search("   ", 50).is_empty());
        assert_eq!(service.search("later", 7).len(), 7, "the limit holds");
    }

    // ---- The saved log --------------------------------------------------------------------

    fn at_text(at: chrono::DateTime<chrono::Utc>) -> String {
        format!(
            "{}.{:07}Z",
            at.format("%Y-%m-%dT%H:%M:%S"),
            at.timestamp_subsec_nanos() / 100
        )
    }

    fn keyed(title: &str, key: Option<&str>, at: chrono::DateTime<chrono::Utc>) -> DownloadHistoryEntry {
        DownloadHistoryEntry {
            key: key.map(str::to_string),
            downloaded_at: at_text(at),
            ..entry(title)
        }
    }

    fn line(at: chrono::DateTime<chrono::Utc>, kind: &str, text: &str, copies: usize) -> AcquisitionEvent {
        AcquisitionEvent {
            at,
            kind: kind.into(),
            text: text.into(),
            detail: None,
            candidates: (copies > 0).then(|| {
                (1..=copies)
                    .map(|i| octo_core::models::download::AcquisitionCandidate {
                        source: "Soulseek".into(),
                        peer: Some(format!("peer{i}")),
                        file: Some(format!("/share/{i}.flac")),
                        format: Some("flac".into()),
                        bit_depth: Some(16),
                        sample_rate: Some(44100),
                        size: Some(30_000_000),
                        rank: Some(i as i32),
                        ..Default::default()
                    })
                    .collect()
            }),
            heard: None,
        }
    }

    #[test]
    fn a_log_lands_on_the_entry_this_run_wrote_never_an_older_fetch_of_the_same_song() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("downloads-history.json");
        let history = DownloadHistoryService::new(&path);
        let start = chrono::Utc::now();
        history.record(keyed(
            "Digital Love",
            Some("soulseek:a"),
            start - TimeDelta::days(2),
        ));
        history.record(keyed("Old song", None, start - TimeDelta::days(1)));
        history.record(keyed(
            "Digital Love",
            Some("soulseek:a"),
            start + TimeDelta::seconds(30),
        ));
        let asked = [line(start, acquisition_event_kinds::QUEUED, "Asked for", 0)];

        assert!(history.attach_log("soulseek:a", start, &asked));
        // A run that began after every entry for it has no entry yet.
        assert!(!history.attach_log("soulseek:a", start + TimeDelta::minutes(5), &asked));

        let reopened = DownloadHistoryService::new(&path);
        let entries = reopened.get_recent(200);
        assert_eq!(entries[0].log.as_ref().map(Vec::len), Some(1));
        assert!(entries[1].log.is_none());
        assert!(entries[2].log.is_none());
        assert_eq!(
            reopened
                .find("soulseek:a", Some(&entries[0].downloaded_at))
                .and_then(|e| e.log)
                .map(|log| log[0].text.clone())
                .as_deref(),
            Some("Asked for")
        );
        assert!(
            reopened
                .find("soulseek:a", Some(&entries[2].downloaded_at))
                .is_some_and(|e| e.log.is_none())
        );
    }

    #[test]
    fn a_saved_log_keeps_its_beginning_and_newest_lines_and_the_best_copies_of_each_line() {
        let at = chrono::Utc::now();
        let mut lines = vec![line(at, acquisition_event_kinds::FOUND, "Found 30 copies", 30)];
        lines.extend((1..200).map(|i| {
            line(
                at + TimeDelta::seconds(i),
                acquisition_event_kinds::NOTE,
                &format!("line {i}"),
                0,
            )
        }));
        lines.push(line(
            at + TimeDelta::seconds(500),
            acquisition_event_kinds::NOTE,
            &"x".repeat(2000),
            0,
        ));

        let saved = DownloadHistoryService::trimmed(&lines);

        assert_eq!(saved.len(), DownloadHistoryService::MAX_LOG_LINES);
        assert_eq!(saved[0].text, "Found 30 copies");
        let copies = saved[0].candidates.as_ref().expect("copies");
        assert_eq!(copies.len(), DownloadHistoryService::MAX_SAVED_CANDIDATES);
        assert_eq!(copies[0].peer.as_deref(), Some("peer1"));
        assert!(saved[1].text.contains("left out"));
        assert_eq!(
            saved[saved.len() - 1].text.chars().count(),
            DownloadHistoryService::MAX_SAVED_TEXT
        );
        assert_eq!(saved[saved.len() - 2].text, "line 199");
    }

    #[test]
    fn when_the_logs_fill_their_room_the_oldest_let_go_and_their_entries_stay() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("downloads-history.json");
        let history = DownloadHistoryService::new(&path);
        history.set_log_room(1200);
        let start = chrono::Utc::now() - TimeDelta::minutes(10);
        for i in 0..4 {
            history.record(keyed(
                &format!("Song {i}"),
                Some(&format!("soulseek:{i}")),
                start + TimeDelta::minutes(i + 1),
            ));
            let steps: Vec<AcquisitionEvent> = (0..5)
                .map(|n| {
                    line(
                        start,
                        acquisition_event_kinds::NOTE,
                        &format!("step {n} of song {i}"),
                        0,
                    )
                })
                .collect();
            assert!(history.attach_log(&format!("soulseek:{i}"), start + TimeDelta::minutes(i), &steps));
        }

        let entries = DownloadHistoryService::new(&path).get_recent(200);
        assert_eq!(entries.len(), 4);
        assert!(entries[0].log.is_some());
        assert!(entries[3].log.is_none());
        assert!(std::fs::metadata(&path).expect("written").len() < 4000);
    }

    #[test]
    fn a_missing_blank_or_corrupt_file_starts_empty_and_is_overwritten() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("downloads-history.json");
        assert!(DownloadHistoryService::new(&path).get_recent(10).is_empty());
        std::fs::write(&path, "  \n").expect("writes");
        assert!(DownloadHistoryService::new(&path).get_recent(10).is_empty());
        std::fs::write(&path, "[{\"Artist\":").expect("writes");
        let service = DownloadHistoryService::new(&path);
        assert!(service.get_recent(10).is_empty());
        service.record(entry("x"));
        assert_eq!(DownloadHistoryService::new(&path).get_recent(10).len(), 1);
    }
}
