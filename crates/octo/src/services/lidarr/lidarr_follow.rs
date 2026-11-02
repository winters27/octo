//! The follow of Octo's search, from its grab until every download of Octo's is accounted for.
//!
//! - **Whose grabs.** A grab of the album is Octo's when its row falls inside one of Octo's own
//!   search windows (the AlbumSearch Octo posted, and Lidarr's own new search of the album after a
//!   failed grab of Octo's, each from its start to its end) and came from a search, with no other
//!   search of the album queued or running in that window; or when it is the release Lidarr held
//!   back for the album and later grabbed. Any other grab or queue item of the artist is the
//!   user's, and Octo steps aside: its own items are removed and the pick is put back.
//! - **Whose files, one by one.** A track file is decided by its own `trackFileImported` row: it
//!   is Octo's when the row carries one of Octo's download ids and names a download client. A row
//!   with no client (a folder import, even with a guessed id), a row of another download, or no
//!   row at all once Lidarr's import commands are idle, makes the file the user's. A file of the
//!   user's seen before any file of Octo's has its row makes Octo step aside, so its own import
//!   cannot replace it. Octo's files are handed to the download pipeline, on whichever album of
//!   the artist Lidarr put them.
//! - **The watch list.** Each download of Octo's stays watched until Lidarr writes its
//!   download-level row, Octo removes it, Octo's own manual import of it ends (with Lidarr's
//!   imports off), or its absence is settled: missing from a queue read after a refresh that
//!   started since Lidarr's own start, with another item of its client listed, so a client that
//!   failed one refresh is never read as done. A download missing while its client lists nothing
//!   else cannot be proven gone: it stays watched, and the dashboard's Lidarr card lists its
//!   follow with the admin's "Put Lidarr back", which counts it gone and ends the follow.
//! - **Octo's own copies.** A copy Octo placed while a follow on its artist stays open (a kept
//!   copy whose person stopped waiting) is Octo's, never the user's: a rescan linking it never
//!   makes Octo step aside.
//! - **Octo's turn.** While a download of Octo's is finished and waits for its import, the
//!   commands are read at a sub-second pace; a rescan that covers the artist, or a manual import
//!   or scan that is not Octo's, makes Octo remove its items before its import could replace
//!   what that command links.
//! - **The end.** The follow ends on Lidarr's own states (a failed, blocked or ignored download,
//!   one that left without an import), on the work of people and other sources (a proven file
//!   of another source for the artist, a kept copy with a download that makes no progress, the
//!   person's "Stop waiting for Lidarr", every heart taken back, the admin's "Put Lidarr back"),
//!   or when every download is accounted for. Octo then removes its items, places the files it held back, and once the
//!   watch list is empty and no search of the album is queued or running puts the pick back,
//!   unless the song landed.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use octo_core::common::dotnet;
use octo_core::lidarr::lidarr_client::{
    LidarrCommand, LidarrDeletion, LidarrHistoryEvent, LidarrHistoryRecord, LidarrQueueItem, LidarrTrackFile,
};
use octo_core::lidarr::{LidarrError, LidarrImportedTrack};
use octo_core::notifications::{NotificationEvent, NotificationEventType};
use octo_core::settings::{LidarrCompletionMode, LidarrSettings};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::{AlbumJob, Inner, translate_imported_path};
use crate::services::common::acquisition_tracker::acquisition_event_kinds;
use crate::services::lidarr::lidarr_pending_albums::{
    FollowEnd, FollowedGrab, FollowedItem, HeldPlacement, PROVEN_ELSEWHERE, PendingAlbum, REPLACED_YOURS,
};
use crate::services::lidarr::lidarr_pick::{put_back_searched, restore_after_landing, retry_moved_records};

/// The commands inside which Lidarr writes a file and its row, holding its disk lock.
const IMPORT_COMMANDS: [&str; 3] = [
    "ProcessMonitoredDownloads",
    "ManualImport",
    "DownloadedAlbumsScan",
];
/// Lidarr's states of a download it will never import.
const FAILED_STATES: [&str; 4] = [
    "importBlocked",
    "importFailed",
    "downloadFailed",
    "downloadFailedPending",
];
/// The states whose removal also blocklists the release.
const BLOCKLISTED: [&str; 2] = ["downloadFailed", "downloadFailedPending"];

/// One read of Lidarr, in the order that settles a file with no row: the artist's track files,
/// then the commands, then the queue, then the history.
struct Reading {
    start_time: Option<DateTime<Utc>>,
    /// The latest the time on Lidarr's clock can have been when the read began: every time a read
    /// compares with Lidarr's dates is Lidarr's own.
    now: DateTime<Utc>,
    files: Vec<LidarrTrackFile>,
    commands: Vec<LidarrCommand>,
    /// Every queue item of every artist: another item of a download's client proves the client
    /// answered.
    queue: Vec<LidarrQueueItem>,
    /// The artist's rows, newest first.
    history: Vec<LidarrHistoryRecord>,
}

impl Reading {
    fn command(&self, id: i32) -> Option<&LidarrCommand> {
        self.commands.iter().find(|command| command.id == id)
    }

    /// Whether Lidarr's import commands were idle at the command read, so a file the earlier file
    /// read saw without a row in the later history read will never get one from them.
    fn imports_idle(&self) -> bool {
        !self
            .commands
            .iter()
            .any(|command| command.is_active() && IMPORT_COMMANDS.contains(&command.name.as_str()))
    }

    /// When the latest refresh of the downloads that ran since Lidarr started began, if one did:
    /// the queue read follows it. Before any, the queue is empty whatever the clients hold.
    fn queue_refreshed(&self) -> Option<DateTime<Utc>> {
        self.commands
            .iter()
            .filter(|command| {
                command.name == "RefreshMonitoredDownloads"
                    && command.ended.is_some()
                    && match (command.started, self.start_time) {
                        (Some(started), Some(start)) => started > start,
                        (Some(_), None) => true,
                        _ => false,
                    }
            })
            .filter_map(|command| command.started)
            .max()
    }

    /// The date of the newest grab row of a download in the artist's history, on Lidarr's clock.
    fn grabbed_at(&self, download_id: &str) -> Option<DateTime<Utc>> {
        self.history
            .iter()
            .filter(|row| row.event == LidarrHistoryEvent::Grabbed)
            .filter(|row| {
                row.download_id
                    .as_deref()
                    .is_some_and(|id| dotnet::eq_ignore_case(id, download_id))
            })
            .map(|row| row.date)
            .max()
    }

    /// The download-level row of Octo's grab of a download: Lidarr writes it after every file's
    /// own row. A row older than the grab is of an earlier grab of the same id.
    fn download_row(&self, grab: &FollowedGrab) -> Option<&LidarrHistoryRecord> {
        self.history.iter().find(|row| {
            row.event.ends_download()
                && grab.grabbed_at.is_none_or(|grabbed| row.date >= grabbed)
                && row
                    .download_id
                    .as_deref()
                    .is_some_and(|id| dotnet::eq_ignore_case(id, &grab.download_id))
        })
    }
}

/// One of Octo's search windows, and whether another search of the album ran in it.
struct Window {
    start: DateTime<Utc>,
    end: Option<DateTime<Utc>>,
    overlapped: bool,
}

impl Window {
    fn holds(&self, date: DateTime<Utc>) -> bool {
        date >= self.start && self.end.is_none_or(|end| date <= end)
    }
}

/// Octo's search windows as Lidarr's commands say now.
struct Windows {
    octo: Vec<Window>,
    /// Octo's search has ended.
    search_ended: bool,
    /// One of Lidarr's own new searches of the album is queued or running.
    research_running: bool,
}

/// What a running follow keeps between its reads.
struct FollowRun {
    key: String,
    album_id: i32,
    artist_id: i32,
    artist_path: Option<String>,
    /// The pipeline's work on the files handed to it.
    handed: Vec<JoinHandle<()>>,
    /// Every watched download was accounted for at the end of the last read, so this read began
    /// after the last accounting event and saw what came of it.
    quiet: bool,
    /// Another Lidarr is set now: nothing is put back in it, since nothing of it is Octo's.
    lidarr_gone: bool,
}

/// What a read decided.
enum Next {
    Wait { ready: bool },
    Finish,
}

impl Inner {
    /// Follows a searched pick until every download of Octo's is accounted for, then puts it back
    /// unless its song landed. One follow per album runs at a time.
    pub(super) async fn follow(self: Arc<Self>, job: Arc<AlbumJob>, album_id: i32) {
        let key = job.candidate.foreign_album_id.clone();
        let Some(wake) = self.pending.start_running(&key) else {
            return;
        };
        let artist_id = self.pending.get(&key).map_or(0, |entry| entry.lidarr_artist_id);
        let mut run = FollowRun {
            key: key.clone(),
            album_id,
            artist_id,
            artist_path: None,
            handed: Vec::new(),
            quiet: false,
            lidarr_gone: false,
        };
        let finished = loop {
            if self.stopping.is_cancelled() {
                break false;
            }
            let settings = self.settings.current().lidarr.clone();
            match self
                .pending
                .get(&key)
                .map(|entry| lidarr_moved(&entry, &settings))
            {
                Some(LidarrMoved::Cleared) => {
                    // Octo no longer has a Lidarr: nobody's copy waits on it.
                    for held in self.pending.release(&key) {
                        self.place_held(held, FollowEnd::Failed).await;
                    }
                    tokio::select! {
                        () = tokio::time::sleep(self.poll(&settings)) => {}
                        () = wake.notified() => {}
                        () = self.stopping.cancelled() => break false,
                    }
                    continue;
                }
                Some(LidarrMoved::Swapped) => {
                    info!(
                        "Octo's Lidarr is another one now; the follow of {key} ends and nothing of the new one is touched"
                    );
                    self.pending.update(&key, |entry| {
                        for grab in &mut entry.grabs {
                            grab.accounted = true;
                            grab.accounted_at = Some(DateTime::<Utc>::MIN_UTC);
                        }
                        entry.pending_items.clear();
                        if entry.ended.is_none() {
                            entry.ended = Some(FollowEnd::Failed);
                        }
                    });
                    run.lidarr_gone = true;
                    break true;
                }
                _ => {}
            }
            retry_moved_records(&self.client, &self.pending, &self.music, &key).await;
            let next = match self.follow_once(&job, &mut run, &settings).await {
                Ok(next) => next,
                Err(e) => {
                    warn!("Could not read Lidarr for the follow of {key}: {e}");
                    // A follow asked to end lets go of the files it held even when Lidarr cannot
                    // be read: nobody's kept copy waits on Lidarr answering.
                    if let Some(entry) = self.pending.get(&key)
                        && let Some(why) = entry.ended
                        && !entry.released
                    {
                        for handoff in run.handed.drain(..) {
                            let _ = handoff.await;
                        }
                        self.let_go(&key, why).await;
                    }
                    Next::Wait { ready: false }
                }
            };
            let pace = match next {
                Next::Finish => break true,
                Next::Wait { ready: true } => self.turn_pace(&settings),
                Next::Wait { ready: false } => self.poll(&settings),
            };
            tokio::select! {
                () = tokio::time::sleep(pace) => {}
                () = wake.notified() => {}
                () = self.stopping.cancelled() => break false,
            }
        };
        if finished {
            self.finish_follow(&job, run).await;
        } else {
            // Stopped as a restart stops it: what lives in memory goes, and the saved entry is
            // followed on by whoever starts next.
            {
                let job_key = dotnet::ordinal_ignore_case_key(&key);
                let mut jobs = self.jobs.lock();
                if jobs
                    .get(&job_key)
                    .is_some_and(|current| Arc::ptr_eq(current, &job))
                {
                    jobs.remove(&job_key);
                }
            }
            if let Some(claims) = &self.claims {
                claims.heart_ended(&key);
            }
        }
        self.pending.stop_running(&key);
    }

    /// One read of Lidarr for the follow, and everything it decides.
    async fn follow_once(
        self: &Arc<Self>,
        job: &Arc<AlbumJob>,
        run: &mut FollowRun,
        settings: &LidarrSettings,
    ) -> Result<Next, LidarrError> {
        let Some(mut entry) = self.pending.get(&run.key) else {
            return Ok(Next::Finish);
        };
        if entry.completed_download_handling.is_none() {
            entry.completed_download_handling = Some(self.client.completed_download_handling().await?);
        }
        if entry.roots.is_empty() {
            entry.roots = self.root_folders(settings).await?;
        }
        if run.artist_id <= 0 {
            run.artist_id = entry.lidarr_artist_id;
        }
        if run.artist_path.is_none() && run.artist_id > 0 {
            run.artist_path = self.client.artist(run.artist_id).await?.and_then(|a| a.path);
        }
        let reading = self.read_lidarr(run.artist_id).await?;
        let mut end: Option<FollowEnd> = None;
        if entry.first_poll.is_none() {
            entry.first_poll = Some(reading.now);
        }

        let windows = self.windows(&mut entry, run, &reading).await?;
        self.record_grabs(&mut entry, run, &reading, &windows, &mut end);
        let ready = self.read_queue(&mut entry, run, &reading, &mut end);
        if ready && entry.following() && self.turn_lost(&entry, run, &reading) {
            info!(
                "A rescan or an import of the user's is coming before Octo's own import of {}; Octo steps aside",
                run.key
            );
            end.get_or_insert(FollowEnd::SteppedAside);
        }
        self.judge_files(job, &mut entry, run, &reading, settings, &mut end)
            .await?;
        self.tell_replaced(&mut entry, &reading).await;
        self.account(&mut entry, run, &reading, &mut end).await?;
        let all_accounted = entry.watched().next().is_none() && entry.pending_items.is_empty();
        if end.is_none() && entry.following() && !windows.research_running {
            if entry.grabs.is_empty() && entry.pending_items.is_empty() && windows.search_ended {
                info!("Lidarr's search of {} grabbed nothing of Octo's", run.key);
                end = Some(FollowEnd::NoGrab);
            } else if !entry.grabs.is_empty() && all_accounted && run.quiet {
                end = Some(FollowEnd::Done);
            }
        }
        self.save_follow(&run.key, entry, end);

        let Some(entry) = self.pending.get(&run.key) else {
            return Ok(Next::Finish);
        };
        if let Some(why) = entry.ended {
            self.remove_items(&run.key, &entry, &reading).await;
            if why == FollowEnd::Unhearted || entry.unseen_let_go {
                self.absent_let_go(&run.key, &reading);
            }
            if !entry.released {
                // What the hold folder is judging settles first: a kept copy that a proven import
                // of its song replaces is dropped, never placed.
                for handoff in run.handed.drain(..) {
                    let _ = handoff.await;
                }
                self.let_go(&run.key, why).await;
            }
            let Some(entry) = self.pending.get(&run.key) else {
                return Ok(Next::Finish);
            };
            // Nothing is put back while a search of the album is queued or running: what it grabs
            // in a window of Octo's is followed and removed like any grab of Octo's.
            let searching = reading.commands.iter().any(|command| {
                command.is_active()
                    && command.name == "AlbumSearch"
                    && command.album_ids().contains(&run.album_id)
            });
            let accounted = entry.watched().next().is_none() && entry.pending_items.is_empty() && !searching;
            if accounted && (run.quiet || why == FollowEnd::Done) {
                return Ok(Next::Finish);
            }
            run.quiet = accounted;
            return Ok(Next::Wait { ready: false });
        }
        run.quiet = all_accounted && !entry.grabs.is_empty();
        Ok(Next::Wait { ready })
    }

    /// Lidarr's root folders as Octo sees them, the ones Octo can map.
    async fn root_folders(&self, settings: &LidarrSettings) -> Result<Vec<String>, LidarrError> {
        let octo_root = self.music.octo_root();
        Ok(self
            .client
            .root_folders()
            .await?
            .iter()
            .filter_map(|root| {
                translate_imported_path(&root.path, settings.root_folder_path.as_deref(), &octo_root).ok()
            })
            .collect())
    }

    async fn read_lidarr(&self, artist_id: i32) -> Result<Reading, LidarrError> {
        let (start_time, second) = self.client.status_times().await?;
        // Lidarr's Date header counts whole seconds: the read began by the end of that second.
        let now = second + chrono::TimeDelta::milliseconds(999);
        let files = if artist_id > 0 {
            self.client.artist_track_files(artist_id).await?
        } else {
            Vec::new()
        };
        let commands = self.client.commands().await?;
        let queue = self.client.queue().await?;
        let history = if artist_id > 0 {
            self.client.artist_history(artist_id, None).await?
        } else {
            Vec::new()
        };
        Ok(Reading {
            start_time,
            now,
            files,
            commands,
            queue,
            history,
        })
    }

    /// Octo's search windows: its own search, and each new search Lidarr started itself for the
    /// album after a failed grab of Octo's, newly found ones saved on the entry. A command Lidarr
    /// no longer lists runs, for Octo's own search, from its post to the follow's first read.
    async fn windows(
        &self,
        entry: &mut PendingAlbum,
        run: &FollowRun,
        reading: &Reading,
    ) -> Result<Windows, LidarrError> {
        let first_failure = reading
            .history
            .iter()
            .filter(|row| row.event == LidarrHistoryEvent::DownloadFailed)
            .filter(|row| {
                row.download_id
                    .as_deref()
                    .is_some_and(|id| entry.owns_grab_at(id, row.date))
            })
            .map(|row| row.date)
            .min();
        if let Some(failed) = first_failure {
            for command in &reading.commands {
                let lidarrs_own = command
                    .trigger
                    .as_deref()
                    .is_none_or(|trigger| !dotnet::eq_ignore_case(trigger, "manual"));
                if command.name == "AlbumSearch"
                    && lidarrs_own
                    && command.id != entry.search_command
                    && command.album_ids().contains(&run.album_id)
                    && command.queued.is_some_and(|queued| queued >= failed)
                    && !entry.research_commands.contains(&command.id)
                {
                    info!(
                        "Lidarr searches {} again after Octo's grab failed; that search's grabs are Octo's too",
                        run.key
                    );
                    entry.research_commands.push(command.id);
                }
            }
        }
        // A restart came after Octo's search went out and before Lidarr's answer was saved: Octo's
        // search is the first one of the album asked through Lidarr's API since Lidarr's time
        // saved right before it went out.
        if entry.search_command <= 0
            && entry.picked_guid.is_none()
            && let Some(pending_at) = entry.search_pending
            && let Some(command) = reading
                .commands
                .iter()
                .filter(|command| {
                    command.name == "AlbumSearch"
                        && command.album_ids().contains(&run.album_id)
                        && command
                            .trigger
                            .as_deref()
                            .is_some_and(|trigger| dotnet::eq_ignore_case(trigger, "manual"))
                        && command.queued.is_some_and(|queued| queued >= pending_at)
                })
                .min_by_key(|command| command.queued)
        {
            info!(
                "Octo's search of {} went out before a restart; Lidarr lists it as command {}",
                run.key, command.id
            );
            entry.search_command = command.id;
            entry.searched_at = command.queued;
        }
        let octo_ids: Vec<i32> = std::iter::once(entry.search_command)
            .chain(entry.research_commands.iter().copied())
            .filter(|id| *id > 0)
            .collect();
        let mut windows = Windows {
            octo: Vec::new(),
            search_ended: true,
            research_running: false,
        };
        // Lidarr lists no search of Octo's: its window runs from Lidarr's time saved before it
        // went out to the follow's first read.
        if entry.search_command <= 0
            && entry.picked_guid.is_none()
            && let Some(pending_at) = entry.search_pending
        {
            let overlapped = reading.commands.iter().any(|other| {
                other.name == "AlbumSearch"
                    && !octo_ids.contains(&other.id)
                    && other.album_ids().contains(&run.album_id)
                    && overlaps(other, pending_at, entry.first_poll)
            });
            windows.octo.push(Window {
                start: pending_at,
                end: entry.first_poll,
                overlapped,
            });
        }
        for id in &octo_ids {
            let command = match reading.command(*id) {
                Some(command) => Some(command.clone()),
                None => self.client.command(*id).await?,
            };
            let own_search = *id == entry.search_command;
            let active = command.as_ref().is_some_and(LidarrCommand::is_active);
            if own_search {
                windows.search_ended = !active;
            } else if active {
                windows.research_running = true;
            }
            let (start, end) = match &command {
                Some(command) => (
                    command.started.or(command.queued),
                    command
                        .ended
                        .or_else(|| (!command.is_active()).then_some(reading.now)),
                ),
                None if own_search => (entry.searched_at.or(entry.search_pending), entry.first_poll),
                None => continue,
            };
            let Some(start) = start else {
                continue;
            };
            let overlapped = reading.commands.iter().any(|other| {
                other.name == "AlbumSearch"
                    && !octo_ids.contains(&other.id)
                    && other.album_ids().contains(&run.album_id)
                    && overlaps(other, start, end)
            });
            if overlapped {
                info!(
                    "Another search of {} ran in Octo's search window; its grabs are the user's",
                    run.key
                );
            }
            windows.octo.push(Window {
                start,
                end,
                overlapped,
            });
        }
        Ok(windows)
    }

    /// Records Octo's own grabs on the watch list; any other grab in the artist since Octo's
    /// search makes Octo step aside.
    fn record_grabs(
        &self,
        entry: &mut PendingAlbum,
        run: &FollowRun,
        reading: &Reading,
        windows: &Windows,
        end: &mut Option<FollowEnd>,
    ) {
        let following = entry.following();
        let listed: HashSet<i32> = reading
            .queue
            .iter()
            .filter(|item| item.download_id.is_none())
            .map(|item| item.id)
            .collect();
        let gone: Vec<FollowedItem> = entry
            .pending_items
            .iter()
            .filter(|item| !listed.contains(&item.queue_id))
            .cloned()
            .collect();
        let since = entry.searched_at.or(entry.search_pending);
        for row in reading.history.iter().rev() {
            if row.event != LidarrHistoryEvent::Grabbed {
                continue;
            }
            let Some(id) = row.download_id.as_deref() else {
                continue;
            };
            // A release picked in Find songs is Octo's by its guid alone, the first time it is
            // grabbed.
            let picked = !entry.owns_grab(id)
                && entry
                    .picked_guid
                    .as_deref()
                    .is_some_and(|guid| row.guid().is_some_and(|own| dotnet::eq_ignore_case(own, guid)));
            if !picked && since.is_none_or(|since| row.date < since) {
                continue;
            }
            // A row of one of Octo's own grabs, inside that grab's span. A later grab of the same
            // id (a torrent grabbed again) is a new grab, judged below.
            if entry.owns_grab_at(id, row.date) || self.pending.is_octos_grab_at(id, row.date) {
                continue;
            }
            let client = row.data("downloadClientName").or_else(|| row.download_client());
            if row.album_id == run.album_id {
                let searched = matches!(row.release_source(), Some("Search" | "UserInvokedSearch"));
                let in_window = windows
                    .octo
                    .iter()
                    .find(|window| window.holds(row.date))
                    .map(|window| !window.overlapped);
                let from_held_back = gone.iter().any(|item| item.became(row));
                if picked || (searched && in_window == Some(true)) || from_held_back {
                    info!("Octo's grab {id} of {} is followed", run.key);
                    if entry.owns_grab(id) {
                        entry.watch_again(id, client, row.date);
                    } else {
                        entry.record_grab_at(id, client, Some(row.date));
                    }
                    continue;
                }
                if following {
                    info!(
                        "A grab of {} that is not Octo's ({}); Octo steps aside",
                        run.key,
                        row.release_source().unwrap_or("unknown")
                    );
                    end.get_or_insert(FollowEnd::SteppedAside);
                }
            } else if following && !self.pending.another_follows(row.album_id, row.date, &run.key) {
                info!(
                    "The user's grab of another album of the artist of {}; Octo steps aside",
                    run.key
                );
                end.get_or_insert(FollowEnd::SteppedAside);
            }
        }
        entry.pending_items.retain(|item| listed.contains(&item.queue_id));
    }

    /// Reads the queue for Octo's items: their client and queue id, a state that ends the pick,
    /// a stalled download with a kept copy waiting, a release held back for the album, and any
    /// item of the user's in the artist. True when an item of Octo's is finished and waits for its
    /// import.
    fn read_queue(
        &self,
        entry: &mut PendingAlbum,
        run: &FollowRun,
        reading: &Reading,
        end: &mut Option<FollowEnd>,
    ) -> bool {
        let following = entry.following();
        let kept_waits = self.pending.kept_waits(&run.key);
        let now = reading.now;
        let mut ready = false;
        for item in &reading.queue {
            let in_artist = item.artist_id == Some(run.artist_id) || item.album_id == Some(run.album_id);
            match item.download_id.as_deref() {
                Some(id) if entry.owns_grab(id) => {
                    let Some(grab) = entry.grab_mut(id) else {
                        continue;
                    };
                    grab.queue_id = item.id;
                    if item.download_client.is_some() {
                        grab.client = item.download_client.clone();
                    }
                    if grab.accounted {
                        continue;
                    }
                    let state = item.tracked_download_state.as_deref().unwrap_or("");
                    if following && FAILED_STATES.contains(&state) {
                        info!("Lidarr's download {id} of {} is {state}; the pick ends", run.key);
                        end.get_or_insert(FollowEnd::Failed);
                    }
                    let stalled = item
                        .tracked_download_status
                        .as_deref()
                        .is_some_and(|status| dotnet::eq_ignore_case(status, "warning"));
                    if following && stalled && kept_waits {
                        info!(
                            "Lidarr's download {id} of {} is stalled and a kept copy waits; Octo stops waiting",
                            run.key
                        );
                        end.get_or_insert(FollowEnd::DeadDownload);
                    }
                    if item
                        .status
                        .as_deref()
                        .is_some_and(|status| dotnet::eq_ignore_case(status, "completed"))
                        || state == "importPending"
                    {
                        ready = true;
                    }
                }
                Some(_) if !in_artist => {}
                // Another follow's download, or one Octo was done with lately, still listed: Octo's
                // while its newest grab row lies inside that grab's span.
                Some(id)
                    if self
                        .pending
                        .is_octos_grab_at(id, reading.grabbed_at(id).or(item.added).unwrap_or(now)) => {}
                Some(_) => {
                    let others = item
                        .album_id
                        .is_some_and(|album| self.pending.another_follows(album, now, &run.key));
                    if following && !others {
                        info!(
                            "A download of the user's in the artist of {}; Octo steps aside",
                            run.key
                        );
                        end.get_or_insert(FollowEnd::SteppedAside);
                    }
                }
                None if item.album_id == Some(run.album_id) => {
                    let seen = FollowedItem {
                        queue_id: item.id,
                        title: item.title.clone(),
                        indexer: item.indexer.clone(),
                        added: item.added,
                    };
                    match entry
                        .pending_items
                        .iter_mut()
                        .find(|known| known.queue_id == item.id)
                    {
                        Some(known) => *known = seen,
                        None => entry.pending_items.push(seen),
                    }
                }
                None if in_artist => {
                    let others = item
                        .album_id
                        .is_some_and(|album| self.pending.another_follows(album, now, &run.key));
                    if following && !others {
                        info!(
                            "Lidarr holds a release of the user's in the artist of {}; Octo steps aside",
                            run.key
                        );
                        end.get_or_insert(FollowEnd::SteppedAside);
                    }
                }
                None => {}
            }
        }
        ready
    }

    /// Whether a command that is not Octo's import is coming while Octo's download waits for its
    /// own: a rescan covering the artist (Lidarr's daily scan of every root folder included), or a
    /// manual import or downloads scan that is not Octo's.
    fn turn_lost(&self, entry: &PendingAlbum, run: &FollowRun, reading: &Reading) -> bool {
        let own: Vec<i32> = entry
            .grabs
            .iter()
            .map(|grab| grab.manual_import)
            .filter(|id| *id > 0)
            .collect();
        reading
            .commands
            .iter()
            .filter(|command| command.is_active())
            .any(|command| {
                command.rescans_artist(run.artist_id, run.artist_path.as_deref())
                    || (matches!(command.name.as_str(), "ManualImport" | "DownloadedAlbumsScan")
                        && !own.contains(&command.id))
            })
    }

    /// Decides each new track file of the artist by its own import row, and hands Octo's to the
    /// pipeline.
    async fn judge_files(
        self: &Arc<Self>,
        job: &Arc<AlbumJob>,
        entry: &mut PendingAlbum,
        run: &mut FollowRun,
        reading: &Reading,
        settings: &LidarrSettings,
        end: &mut Option<FollowEnd>,
    ) -> Result<(), LidarrError> {
        let idle = reading.imports_idle();
        let mut rows: HashMap<i32, &LidarrHistoryRecord> = HashMap::new();
        for row in &reading.history {
            if row.event == LidarrHistoryEvent::TrackFileImported
                && let Some(file_id) = row.file_id()
            {
                rows.entry(file_id).or_insert(row);
            }
        }
        let octos = |row: &LidarrHistoryRecord| {
            row.download_client().is_some()
                && row
                    .download_id
                    .as_deref()
                    .is_some_and(|id| entry.owns_grab_at(id, row.date))
        };
        let own_row = !entry.octo_files.is_empty()
            || reading
                .files
                .iter()
                .any(|file| rows.get(&file.id).copied().is_some_and(octos));
        let following = entry.following();
        let octo_root = self.music.octo_root();
        // A copy Octo placed itself (a kept copy whose person stopped waiting while others still
        // wait on the follow) that a rescan linked is neither the user's nor this follow's import.
        let octos_copy = |file: &LidarrTrackFile| {
            file.path.as_deref().is_some_and(|path| {
                translate_imported_path(path, settings.root_folder_path.as_deref(), &octo_root)
                    .is_ok_and(|local| self.pending.is_octos_copy(&local))
            })
        };
        let mut mine: Vec<&LidarrTrackFile> = Vec::new();
        let mut users: Vec<i32> = Vec::new();
        for file in &reading.files {
            if entry.users_files.contains(&file.id) {
                continue;
            }
            if entry.octo_files.contains(&file.id) {
                mine.push(file);
                continue;
            }
            match rows.get(&file.id).copied() {
                Some(row) if octos(row) => mine.push(file),
                _ if octos_copy(file) => {}
                Some(row)
                    if row.download_client().is_some()
                        && row
                            .download_id
                            .as_deref()
                            .is_some_and(|id| self.pending.is_octos_grab_at(id, row.date)) => {}
                Some(_) => users.push(file.id),
                None if idle => users.push(file.id),
                None => {}
            }
        }
        for file in &mine {
            if !entry.octo_files.contains(&file.id) {
                entry.octo_files.push(file.id);
            }
        }
        if !users.is_empty() {
            info!(
                "{} file(s) of the user's came into the artist of {}",
                users.len(),
                run.key
            );
            if following && !own_row {
                info!("They came before any file of Octo's own; Octo steps aside");
                end.get_or_insert(FollowEnd::SteppedAside);
            }
            entry.users_files.extend(users);
        }

        let gated = job.gated();
        let by_sound = !gated && job.files_by_sound() && self.gates.is_some();
        let replacement_only = job.replacement_only();
        let mut held_any = false;
        let mut tracks: HashMap<i32, Vec<LidarrImportedTrack>> = HashMap::new();
        for file in mine {
            if entry.files_handed.contains(&file.id) {
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(slot) = tracks.entry(file.album_id) {
                slot.insert(self.client.get_album_tracks(file.album_id).await?);
                // Lidarr's import of Octo's own download may switch the album to the release it
                // matched: that release is noted, so a put back still gives the saved choice back.
                if file.album_id == run.album_id
                    && let Some(album) = self.client.album(file.album_id).await?
                {
                    for release in album.releases.iter().filter(|release| release.monitored) {
                        if !entry.imported_releases.contains(&release.id) {
                            entry.imported_releases.push(release.id);
                        }
                    }
                }
            }
            let Some(track) = tracks
                .get(&file.album_id)
                .and_then(|album| album.iter().find(|track| track.track_file_id == file.id))
                .cloned()
            else {
                continue;
            };
            let Some(path) = file.path.as_deref().or(track.path.as_deref()) else {
                continue;
            };
            let imported_path =
                match translate_imported_path(path, settings.root_folder_path.as_deref(), &octo_root) {
                    Ok(imported_path) => imported_path,
                    Err(e) => {
                        warn!("Octo cannot reach Lidarr's import of its own download at {path}: {e}");
                        continue;
                    }
                };
            if !Path::new(&imported_path).is_file() {
                continue;
            }
            entry.files_handed.push(file.id);
            if !entry.tracks_taken.contains(&track.id) {
                entry.tracks_taken.push(track.id);
            }
            // Saved at once: a read that fails later never hands the same file over twice.
            let (file_id, track_id) = (file.id, track.id);
            self.pending.update(&run.key, |saved| {
                if !saved.files_handed.contains(&file_id) {
                    saved.files_handed.push(file_id);
                }
                if !saved.tracks_taken.contains(&track_id) {
                    saved.tracks_taken.push(track_id);
                }
            });
            if file.album_id != run.album_id {
                info!(
                    "Lidarr imported Octo's download into another album ({}) of the artist of {}",
                    file.album_id, run.key
                );
            }
            let dropped = rows
                .get(&file.id)
                .and_then(|row| row.dropped_path())
                .and_then(|dropped| self.delivered(dropped, settings));
            // A replacement's fetch waiting for this song takes it first.
            if self
                .deliver_fetched(&run.key, &track, &imported_path, dropped.clone())
                .await
            {
                continue;
            }
            if replacement_only {
                // Nothing else of a replacement's album is the library's: held out of it, and
                // deleted with the pick once the follow ends.
                self.hold_track(&run.key, &track, file.album_id, &imported_path, dropped)
                    .await;
                continue;
            }
            if gated {
                // A hearted song's album: every track waits in the hold folder until one passes.
                if self
                    .hold_track(&run.key, &track, file.album_id, &imported_path, dropped)
                    .await
                {
                    held_any = true;
                }
                continue;
            }
            if by_sound {
                // A hearted album's import: each file is filed among the album's tracks by its own
                // sound, never by where Lidarr put it.
                if let Some(handoff) = self
                    .file_album_import(job, &run.key, &track, imported_path, dropped)
                    .await
                {
                    run.handed.push(handoff);
                }
                continue;
            }
            let labels = self.labels(&run.key, dropped).await;
            if let Some(handoff) = self.take_import(job, &track, imported_path, labels, true).await {
                run.handed.push(handoff);
            }
        }
        if held_any {
            run.handed.push(tokio::spawn(self.clone().settle(
                job.clone(),
                run.key.clone(),
                false,
            )));
        }
        Ok(())
    }

    /// Tells, on the downloads entry, of each file of the user's that Lidarr deleted as an upgrade
    /// while importing Octo's download: a file at a path no import of Octo's had filled before.
    async fn tell_replaced(&self, entry: &mut PendingAlbum, reading: &Reading) {
        let Some(since) = entry.searched_at else {
            return;
        };
        let octo_imports: Vec<(String, DateTime<Utc>, i32)> = reading
            .history
            .iter()
            .filter(|row| row.event == LidarrHistoryEvent::TrackFileImported)
            .filter(|row| {
                row.download_client().is_some()
                    && row
                        .download_id
                        .as_deref()
                        .is_some_and(|id| entry.owns_grab_at(id, row.date))
            })
            .filter_map(|row| {
                row.imported_path()
                    .map(|path| (path.to_string(), row.date, row.album_id))
            })
            .collect();
        let replaced: Vec<&LidarrHistoryRecord> = reading
            .history
            .iter()
            .filter(|row| row.event == LidarrHistoryEvent::TrackFileDeleted && row.date >= since)
            .filter(|row| {
                row.reason()
                    .is_some_and(|reason| dotnet::eq_ignore_case(reason, "Upgrade"))
            })
            .filter(|row| !entry.replaced_told.contains(&row.id))
            .filter(|row| {
                let path = row.source_title.as_str();
                let octos_before = octo_imports
                    .iter()
                    .any(|(own, date, _)| dotnet::eq_ignore_case(own, path) && *date < row.date);
                let by_octo = octo_imports
                    .iter()
                    .any(|(_, date, album)| *album == row.album_id && *date >= row.date);
                !octos_before && by_octo
            })
            .collect();
        if replaced.is_empty() {
            return;
        }
        let bin = self.client.recycle_bin().await.ok().flatten();
        for row in replaced {
            entry.replaced_told.push(row.id);
            let path = row.source_title.clone();
            warn!("{REPLACED_YOURS}: {path}");
            let detail = match &bin {
                Some(bin) => format!("{path}. Lidarr moved it to its recycle bin, {bin}"),
                None => path,
            };
            if let Some(tracker) = &self.tracker {
                for song in &entry.songs {
                    if let Some((provider, id)) = song.split_once(':') {
                        tracker.log(
                            provider,
                            id,
                            acquisition_event_kinds::NOTE,
                            REPLACED_YOURS,
                            Some(&detail),
                        );
                    }
                }
            }
        }
    }

    /// Accounts for each watched download that Lidarr is done with: its download-level row, the
    /// end of Octo's own manual import of it, or its settled absence. With Lidarr's imports off, a
    /// settled absence is the download finishing, and Octo imports its files as that download.
    async fn account(
        &self,
        entry: &mut PendingAlbum,
        run: &FollowRun,
        reading: &Reading,
        end: &mut Option<FollowEnd>,
    ) -> Result<(), LidarrError> {
        let following = entry.following();
        let refreshed = reading.queue_refreshed();
        let listed: HashSet<String> = reading
            .queue
            .iter()
            .filter_map(|item| item.download_id.as_deref())
            .map(dotnet::ordinal_ignore_case_key)
            .collect();
        let clients: HashSet<String> = reading
            .queue
            .iter()
            .filter_map(|item| item.download_client.as_deref())
            .map(dotnet::ordinal_ignore_case_key)
            .collect();
        let imports_on = entry.completed_download_handling.unwrap_or(true);
        let mut failed = false;
        let mut unseen = Vec::new();
        for index in 0..entry.grabs.len() {
            let grab = entry.grabs[index].clone();
            if grab.accounted {
                continue;
            }
            if let Some(row) = reading.download_row(&grab) {
                entry.grabs[index].account(row.date);
                if row.event != LidarrHistoryEvent::DownloadImported {
                    info!(
                        "Lidarr wrote {} for Octo's download {} of {}; the pick ends",
                        row.event.name(),
                        grab.download_id,
                        run.key
                    );
                    failed = true;
                }
                continue;
            }
            if grab.manual_import > 0 {
                let command = match reading.command(grab.manual_import) {
                    Some(command) => Some(command.clone()),
                    None => self.client.command(grab.manual_import).await?,
                };
                if command.as_ref().is_none_or(|command| !command.is_active()) {
                    let ended = command.and_then(|command| command.ended).unwrap_or(reading.now);
                    entry.grabs[index].account(ended);
                }
                continue;
            }
            let Some(refreshed_at) = refreshed else {
                continue;
            };
            let missing = !listed.contains(&dotnet::ordinal_ignore_case_key(&grab.download_id));
            let client_answered = grab
                .client
                .as_deref()
                .is_some_and(|client| clients.contains(&dotnet::ordinal_ignore_case_key(client)));
            // Missing from a refresh that began after its grab, its client listing nothing else:
            // Lidarr gives no proof it is gone, so it stays watched, and the Lidarr card says so.
            if missing && !client_answered && grab.grabbed_at.is_none_or(|grabbed| refreshed_at > grabbed) {
                unseen.push(grab.download_id.clone());
            }
            let absent = missing && client_answered;
            if !absent {
                continue;
            }
            if imports_on {
                info!(
                    "Octo's download {} of {} left Lidarr's queue without an import",
                    grab.download_id, run.key
                );
                entry.grabs[index].account(refreshed_at);
                failed = true;
                continue;
            }
            let items: Vec<_> = self
                .client
                .manual_import_candidates(&grab.download_id)
                .await?
                .into_iter()
                .filter(|item| item.album_id == run.album_id && item.rejections.is_empty())
                .map(|mut item| {
                    item.download_id = Some(grab.download_id.clone());
                    item
                })
                .collect();
            if items.is_empty() {
                entry.grabs[index].account(refreshed_at);
                continue;
            }
            info!(
                "Lidarr's imports are off; Octo imports its finished download {} of {} itself",
                grab.download_id, run.key
            );
            let command = self.client.manual_import(&items).await?;
            entry.grabs[index].manual_import = command.id;
            // Saved at once: a restart must find Octo's own import, never take it for the user's.
            let (key, download_id, command_id) = (run.key.clone(), grab.download_id.clone(), command.id);
            self.pending.update(&key, |saved| {
                if let Some(saved) = saved.grab_mut(&download_id) {
                    saved.manual_import = command_id;
                }
            });
        }
        entry.unseen = unseen;
        if failed && following {
            end.get_or_insert(FollowEnd::Failed);
        }
        Ok(())
    }

    /// Saves what this read found onto the entry as it stands now, keeping what others changed
    /// meanwhile: a song event's progress marks, a join's people, an end someone asked for.
    fn save_follow(&self, key: &str, entry: PendingAlbum, end: Option<FollowEnd>) {
        self.pending.update(key, move |current| {
            let mut grabs = entry.grabs;
            for grab in &mut grabs {
                if let Some(known) = current.grab_mut(&grab.download_id) {
                    // Only a play or a heart moves this mark; the read's copy may be older.
                    grab.size_left_at_event = known.size_left_at_event;
                    // Accounted for meanwhile (Octo removed it): kept, unless this read saw the
                    // id grabbed again after that.
                    let regrabbed = grab.grabbed_at > known.grabbed_at && !grab.accounted;
                    if known.accounted && !regrabbed {
                        grab.accounted = true;
                        grab.accounted_at = grab.accounted_at.or(known.accounted_at);
                    }
                    if grab.manual_import <= 0 {
                        grab.manual_import = known.manual_import;
                    }
                }
            }
            let mut grab_ids = entry.grab_ids;
            for id in &current.grab_ids {
                if !grab_ids.iter().any(|own| dotnet::eq_ignore_case(own, id)) {
                    grab_ids.push(id.clone());
                }
            }
            for grab in &current.grabs {
                if !grabs
                    .iter()
                    .any(|own| dotnet::eq_ignore_case(&own.download_id, &grab.download_id))
                {
                    grabs.push(grab.clone());
                }
            }
            current.grab_ids = grab_ids;
            current.grabs = grabs;
            current.research_commands = entry.research_commands;
            current.pending_items = entry.pending_items;
            current.octo_files = entry.octo_files;
            current.users_files = entry.users_files;
            current.files_handed = entry.files_handed;
            current.replaced_told = entry.replaced_told;
            current.tracks_taken = entry.tracks_taken;
            current.first_poll = entry.first_poll;
            current.unseen = entry.unseen;
            current.completed_download_handling = entry.completed_download_handling;
            current.roots = entry.roots;
            if current.search_command <= 0 {
                current.search_command = entry.search_command;
            }
            current.searched_at = current.searched_at.or(entry.searched_at);
            for release in entry.imported_releases {
                if !current.imported_releases.contains(&release) {
                    current.imported_releases.push(release);
                }
            }
            if current.ended.is_none() {
                current.ended = end;
            }
        });
    }

    /// Removes each watched item of Octo's the read listed: never skipping a new search, and
    /// blocklisting only a release whose download failed. A removal Lidarr took accounts for the
    /// download.
    async fn remove_items(&self, key: &str, entry: &PendingAlbum, reading: &Reading) {
        for item in &reading.queue {
            let ours = match item.download_id.as_deref() {
                Some(id) => entry
                    .watched()
                    .any(|grab| dotnet::eq_ignore_case(&grab.download_id, id)),
                None => entry
                    .pending_items
                    .iter()
                    .any(|pending| pending.queue_id == item.id),
            };
            if !ours {
                continue;
            }
            let state = item.tracked_download_state.as_deref().unwrap_or("");
            let blocklist = BLOCKLISTED.contains(&state);
            match self.client.remove_queue_item(item.id, blocklist).await {
                Ok(LidarrDeletion::Deleted) => {
                    info!("Removed Octo's item {} of {key} from Lidarr's queue", item.id);
                    let (download_id, queue_id) = (item.download_id.clone(), item.id);
                    // The read listed it just before: no grab of the same id came in between.
                    let at = reading.now;
                    self.pending.update(key, |saved| match &download_id {
                        Some(id) => {
                            if let Some(grab) = saved.grab_mut(id) {
                                grab.account(at);
                            }
                        }
                        None => saved.pending_items.retain(|pending| pending.queue_id != queue_id),
                    });
                }
                Ok(_) => {}
                Err(e) => warn!(
                    "Could not remove Octo's item {} of {key} from Lidarr's queue: {e}",
                    item.id
                ),
            }
        }
    }

    /// Nobody waits on proof of the follow's downloads any more (every heart on it was taken back,
    /// or the admin put Lidarr back for downloads Octo cannot see): a download of Octo's missing
    /// from a queue read made after a refresh since Lidarr's start is accounted for even when its
    /// client lists nothing else, and a release Lidarr no longer holds back is let go. One still
    /// listed stays watched, and its removal is asked again at every read.
    fn absent_let_go(&self, key: &str, reading: &Reading) {
        let listed: HashSet<String> = reading
            .queue
            .iter()
            .filter_map(|item| item.download_id.as_deref())
            .map(dotnet::ordinal_ignore_case_key)
            .collect();
        let held_back: HashSet<i32> = reading
            .queue
            .iter()
            .filter(|item| item.download_id.is_none())
            .map(|item| item.id)
            .collect();
        let refreshed = reading.queue_refreshed();
        self.pending.update(key, |saved| {
            if let Some(refreshed_at) = refreshed {
                for grab in saved.grabs.iter_mut().filter(|grab| !grab.accounted) {
                    if !listed.contains(&dotnet::ordinal_ignore_case_key(&grab.download_id)) {
                        grab.account(refreshed_at);
                    }
                }
            }
            saved
                .pending_items
                .retain(|item| held_back.contains(&item.queue_id));
        });
    }

    /// The follow lets go of the files it held: each one no other open follow holds is placed now.
    /// A file an earlier end could not place is tried again with them.
    async fn let_go(&self, key: &str, why: FollowEnd) {
        let mut free = self.pending.release(key);
        free.extend(self.pending.release_unfollowed());
        for held in free {
            self.place_held(held, why).await;
        }
        self.pending.update(key, |entry| entry.released = true);
    }

    /// Places a held file now. One that cannot be placed stays on the list, waiting for no follow,
    /// and is tried again when the next follow ends and at the next start.
    pub(super) async fn place_held(&self, held: HeldPlacement, why: FollowEnd) {
        let placed = match &self.downloads {
            Some(downloads) => downloads.place_held(&held).await,
            None => None,
        };
        match placed {
            Some(path) => {
                info!(
                    "Placed {} now that Octo no longer waits for Lidarr: {path}",
                    held.song
                );
                // A follow still open on its artist (others wait on it) may see a rescan link it:
                // while that follow is listed, it is Octo's own copy there, never the user's.
                self.pending.placed_copy(&path, &held.follows);
            }
            None => {
                warn!(
                    "Could not place {} from {}; it plays from there and is placed again later",
                    held.song, held.held_path
                );
                if Path::new(&held.held_path).exists() {
                    self.pending.hold_again(HeldPlacement {
                        follows: Vec::new(),
                        ..held.clone()
                    });
                }
            }
        }
        // The row stops saying it waits for Lidarr; a proven copy that ended the wait is its last
        // line.
        if let Some(tracker) = &self.tracker
            && let Some((provider, id)) = held.song.split_once(':')
        {
            let last = (why == FollowEnd::ProvenElsewhere).then_some(PROVEN_ELSEWHERE);
            tracker.end_aside(provider, id, last);
        }
    }

    /// The follow is over: the pipeline's work waited for, the album's hold folder settled for the
    /// last time (its tracks handed on when a hearted song passed, else deleted), the pick put back
    /// unless its song landed (the pin restored either way), and the hearts still waiting told.
    async fn finish_follow(self: &Arc<Self>, job: &Arc<AlbumJob>, run: FollowRun) {
        for handoff in run.handed {
            let _ = handoff.await;
        }
        let lidarr_gone = run.lidarr_gone;
        let key = run.key;
        self.clone().settle(job.clone(), key.clone(), true).await;
        let landed_titles = job
            .landed
            .lock()
            .iter()
            .filter(|landed| landed.starts_with("t:"))
            .count() as i32;
        if let Some(entry) = self.pending.get(&key) {
            if !entry.released {
                self.let_go(&key, entry.ended.unwrap_or(FollowEnd::Done)).await;
            }
            if landed_titles > 0 {
                self.pending.update(&key, |saved| saved.landed = true);
            }
            let entry = self.pending.get(&key).unwrap_or(entry);
            *job.no_grab.lock() = entry.ended == Some(FollowEnd::NoGrab);
            let poll = self.poll(&self.settings.current().lidarr);
            // A hearted song passed before a restart counts as landed as much as one passed now.
            // A put back Lidarr fails is tried again when Lidarr next answers and at the poll pace.
            if lidarr_gone {
                info!("Octo let go of Lidarr's album {key}: the Lidarr it was asked of is no longer Octo's");
                self.pending.forget(&key);
            } else if entry.ended == Some(FollowEnd::Done) && (entry.landed || !entry.passed.is_empty()) {
                match restore_after_landing(&self.client, &self.pending, &entry, poll).await {
                    Ok(()) => {
                        info!("Octo's import of Lidarr's album {key} landed; its release choice is as it was")
                    }
                    Err(e) => {
                        warn!("Could not give Lidarr's album {key} its release choice back: {e}");
                        self.owe_put_back(&key);
                    }
                }
            } else {
                match put_back_searched(
                    &self.client,
                    Some(self.pending.as_ref()),
                    &self.music,
                    &entry,
                    poll,
                )
                .await
                {
                    Ok(()) => info!("Put back Lidarr's album {key} once Octo's downloads were accounted for"),
                    Err(e) => {
                        warn!("Could not put back Lidarr's album {key}: {e}");
                        self.owe_put_back(&key);
                    }
                }
            }
        }
        let settings = self.settings.current().lidarr.clone();
        if landed_titles > 0
            && (job.album_heart() || settings.completion_mode == LidarrCompletionMode::Imported)
        {
            let asked = job.view().songs.len() as i32;
            self.notifications.notify(NotificationEvent {
                artist: Some(job.album.artist.clone()),
                title: Some(job.album.title.clone()),
                cover_art_url: job.album.cover_art_url.clone(),
                track_count: Some(landed_titles),
                failed_count: Some((asked - landed_titles).max(0)),
                source: Some("Lidarr".into()),
                ..NotificationEvent::new(NotificationEventType::AlbumCompleted)
            });
        }
        job.end();
        // Nothing more is filed against the hearted albums' gates.
        job.gate_holds.lock().clear();
        {
            let job_key = dotnet::ordinal_ignore_case_key(&key);
            let mut jobs = self.jobs.lock();
            if jobs
                .get(&job_key)
                .is_some_and(|current| Arc::ptr_eq(current, job))
            {
                jobs.remove(&job_key);
            }
        }
        if let Some(claims) = &self.claims {
            claims.heart_ended(&key);
        }
    }

    /// A play or a heart of a song: each open follow of it reads Lidarr's queue, and a download of
    /// Octo's that made no progress since the last such event, while a kept copy of the song
    /// waits, ends the follow.
    pub(super) fn song_event(self: &Arc<Self>, provider: &str, external_id: &str) {
        let song = format!("{provider}:{external_id}");
        let follows = self.pending.follows_of_song(&song);
        if follows.is_empty() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let inner = self.clone();
        runtime.spawn(async move {
            let queue = match inner.client.queue().await {
                Ok(queue) => queue,
                Err(e) => {
                    warn!("Could not read Lidarr's queue for a play of {song}: {e}");
                    return;
                }
            };
            for key in follows {
                inner.progress_check(&key, &queue);
            }
        });
    }

    fn progress_check(self: &Arc<Self>, key: &str, queue: &[LidarrQueueItem]) {
        let Some(entry) = self.pending.get(key) else {
            return;
        };
        let kept = self.pending.kept_waits(key);
        let mut dead = false;
        let mut marks: Vec<(String, i64)> = Vec::new();
        for grab in entry.watched() {
            let Some(left) = queue
                .iter()
                .find(|item| {
                    item.download_id
                        .as_deref()
                        .is_some_and(|id| dotnet::eq_ignore_case(id, &grab.download_id))
                })
                .and_then(|item| item.size_left_bytes)
            else {
                continue;
            };
            if kept && left > 0 && grab.size_left_at_event == Some(left) {
                dead = true;
            }
            marks.push((grab.download_id.clone(), left));
        }
        self.pending.update(key, |entry| {
            for (id, left) in marks {
                if let Some(grab) = entry.grab_mut(&id) {
                    grab.size_left_at_event = Some(left);
                }
            }
        });
        if dead && self.pending.request_end(key, FollowEnd::DeadDownload) {
            info!(
                "Lidarr's download for {key} made no progress between two plays or hearts, and a kept copy waits; Octo stops waiting"
            );
            self.ensure_following(key);
        }
    }
}

/// Whether the Lidarr Octo has now is the one a pick was made in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LidarrMoved {
    Same,
    /// No Lidarr is set.
    Cleared,
    /// Another Lidarr is set: the pick's ids name nothing of it.
    Swapped,
}

pub(super) fn lidarr_moved(entry: &PendingAlbum, settings: &LidarrSettings) -> LidarrMoved {
    match (super::lidarr_identity(settings), &entry.lidarr_url) {
        (None, _) => LidarrMoved::Cleared,
        (Some(now), Some(saved)) if now != *saved => LidarrMoved::Swapped,
        _ => LidarrMoved::Same,
    }
}

/// Whether another command ran or waited during a window: from its queuing to its end.
fn overlaps(other: &LidarrCommand, start: DateTime<Utc>, end: Option<DateTime<Utc>>) -> bool {
    let other_start = other.queued.or(other.started);
    let other_end = other.ended;
    let starts_before_end = match (other_start, end) {
        (Some(other_start), Some(end)) => other_start <= end,
        _ => true,
    };
    let ends_after_start = other_end.is_none_or(|other_end| other_end >= start);
    starts_before_end && ends_after_start
}
