//! The import service: every person's imported lists, their matching and their playlists.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use indexmap::IndexMap;
use octo_core::common::Clock;
use octo_core::common::acquisition_tracker::user_safe;
use octo_core::common::dotnet::{eq_ignore_case, ordinal_ignore_case_key, to_lower_invariant};
use octo_core::settings::SettingsStore;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use super::import_file_reader::{FileList, FileRead, FileReadError, ImportFileReader};
use super::import_matcher::ImportMatcher;
use super::import_models::{import_sources, import_track_states};
use super::import_playlists::ImportPlaylists;
use super::import_store::ImportStore;
use super::import_views::{
    ApprovalView, ImportActionResult, ImportOverview, ListDetail, ListSummary, TrackView, TrickleView,
};
use super::spotify_link_reader::{SpotifyException, SpotifyLinkReader};
use super::text_list::{self, TextLine, TextList};
use super::trickle_queue::TrickleQueue;
use super::trickle_worker::TrickleWorker;
use super::tunemymusic::ImportServices;
use super::{ImportList, ImportTrack, TrickleJob};
use crate::services::common::acquisition_tracker::{
    AcquisitionSnapshot, AcquisitionState, AcquisitionTracker, key_of,
};
use crate::services::family::request_service::{RequestFacts, RequestService};
use crate::services::family::{LibraryWriteGate, WriteDecision};

/// What a list saved from a Spotify account says: it is kept and matched, never read again.
pub const ACCOUNT_LIST_NOTE: &str = "Octo does not read Spotify accounts, so this list stays as it was. Export it with TuneMyMusic and import the file to bring it up to date, or remove it.";

/// Importing from a public link, an exported file or a pasted list, for the dashboard and the
/// Octo apps alike: reading each list, matching each song against the library, keeping a list
/// as a Navidrome playlist, and handing missing songs to the trickle.
///
/// Also the loop that keeps it all current: links kept as playlists or fetched in full are read
/// again every few hours, a song the trickle fetched is matched and put in its playlists, and a
/// playlist that changed is written. Everything slow runs here, never inside a request.
pub struct ImportService {
    store: Arc<ImportStore>,
    queue: Arc<TrickleQueue>,
    trickle: Arc<TrickleWorker>,
    matcher: Arc<ImportMatcher>,
    playlists: ImportPlaylists,
    links: Arc<SpotifyLinkReader>,
    settings: Arc<SettingsStore>,
    tracker: Option<Arc<AcquisitionTracker>>,

    gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// People whose lists need matching again: the trickle brought them something.
    rematch: Mutex<Vec<String>>,
    /// People whose lists hold ISRCs the music database has not answered yet: asked about in a
    /// round beside the loop.
    learning: Mutex<Vec<String>>,
    /// The round of learning running beside the loop, if one is.
    learner: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Whether this run has looked for people to learn about yet: the first pass after a start
    /// picks up every list that still waits for answers.
    learning_seeded: AtomicBool,
    /// A song a streaming list misses landed: the next open or pass reads the library again.
    reread_library: AtomicBool,
    /// Library songs as Navidrome described them to each listener for a streaming list, by
    /// lowercased listener and song id, with when they were read.
    songs: Mutex<HashMap<(String, String), (serde_json::Value, DateTime<Utc>)>>,
    /// Lists whose playlist needs writing, as "owner\nid".
    dirty_playlists: Mutex<Vec<String>>,
    /// The back-off for a playlist that would not write: failures so far and when to try
    /// again, by "owner\nid".
    playlist_retries: Mutex<HashMap<String, (u32, DateTime<Utc>)>>,
    clock: Mutex<Clock>,
    /// Family's write gate and requests: a member who must ask first asks for a list's missing
    /// songs instead of fetching them. Unset, or with Family off, everyone fetches as always.
    family: std::sync::OnceLock<ImportFamily>,
    /// Runs between the held look and the forget in `forget_unheld_answers`. Only tests set it.
    #[cfg(test)]
    after_held_look: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

/// What imports use for family members.
#[derive(Clone)]
pub struct ImportFamily {
    pub gate: Arc<LibraryWriteGate>,
    pub requests: std::sync::Weak<RequestService>,
}

/// What a family member reads once their list's missing songs were asked for.
const LIST_ASKED: &str = "Asked for a copy of this list's missing songs. You'll hear back once it's decided.";

/// What import lists do for this person: fetch, or a request, or a refusal.
enum ListWrite {
    Fetch,
    Ask(Option<String>),
    Refuse(String),
}

/// What the import is built over.
pub struct ImportParts {
    pub store: Arc<ImportStore>,
    pub queue: Arc<TrickleQueue>,
    pub trickle: Arc<TrickleWorker>,
    pub matcher: Arc<ImportMatcher>,
    pub playlists: ImportPlaylists,
    pub links: Arc<SpotifyLinkReader>,
    pub settings: Arc<SettingsStore>,
    pub tracker: Option<Arc<AcquisitionTracker>>,
}

impl ImportService {
    pub const LOOP_EVERY: Duration = Duration::from_secs(30);

    /// Lists one person may keep, from files and links.
    pub const MAX_LISTS: usize = 500;

    /// Songs across one person's lists.
    pub const MAX_SONGS: usize = 250_000;

    /// The back-off for a playlist Navidrome would not take: the first try again after a minute,
    /// then twice as long each time, at most an hour, never every loop.
    pub const PLAYLIST_RETRY_FIRST: TimeDelta = TimeDelta::minutes(1);
    pub const PLAYLIST_RETRY_MOST: TimeDelta = TimeDelta::hours(1);

    pub fn new(parts: ImportParts) -> Arc<Self> {
        let service = Arc::new(ImportService {
            store: parts.store,
            queue: parts.queue,
            trickle: parts.trickle,
            matcher: parts.matcher,
            playlists: parts.playlists,
            links: parts.links,
            settings: parts.settings,
            tracker: parts.tracker,
            gates: Mutex::new(HashMap::new()),
            rematch: Mutex::new(Vec::new()),
            learning: Mutex::new(Vec::new()),
            learner: Mutex::new(None),
            learning_seeded: AtomicBool::new(false),
            reread_library: AtomicBool::new(false),
            songs: Mutex::new(HashMap::new()),
            dirty_playlists: Mutex::new(Vec::new()),
            playlist_retries: Mutex::new(HashMap::new()),
            clock: Mutex::new(Clock::system()),
            family: std::sync::OnceLock::new(),
            #[cfg(test)]
            after_held_look: Mutex::new(None),
        });
        let weak = Arc::downgrade(&service);
        service.trickle.on_finished(Arc::new(move |job| {
            if let Some(service) = weak.upgrade() {
                service.on_fetched(job);
            }
        }));
        // A song held for MusicBrainz puts its owner in the next learning round, whose first
        // answer ends the hold.
        let weak = Arc::downgrade(&service);
        service.trickle.on_held(Arc::new(move |owner| {
            if let Some(service) = weak.upgrade() {
                service.learn_later(owner);
            }
        }));
        // A heart can bring a song a streaming list is missing. Once Navidrome shows it, the
        // owners of the lists that miss it are matched again on the next pass, which reads the
        // library once however many songs landed in between.
        if let Some(tracker) = &service.tracker {
            let weak = Arc::downgrade(&service);
            tracker.subscribe_ended(Arc::new(move |end| {
                if !end.done || end.library_id.is_none() {
                    return;
                }
                let Some(service) = weak.upgrade() else {
                    return;
                };
                let owners = service
                    .store
                    .streaming_owners_missing(end.artist.as_deref(), end.title.as_deref());
                if owners.is_empty() {
                    return;
                }
                service.matcher.forget();
                service.reread_library.store(true, Ordering::Relaxed);
                let mut rematch = service.rematch.lock();
                for owner in owners {
                    if !rematch.iter().any(|user| eq_ignore_case(user, &owner)) {
                        rematch.push(owner);
                    }
                }
            }));
        }
        service
    }

    pub fn set_clock(&self, clock: Clock) {
        *self.clock.lock() = clock;
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.lock().now()
    }

    /// Every person's lists, for the version check's look at what each song points at.
    pub fn all_lists(&self) -> Vec<super::import_models::ImportList> {
        self.store.all()
    }

    /// Matches a person's lists again from a fresh read of the library: after a file was put
    /// back or replaced, so each song points at the version it names.
    pub async fn rematch(&self, user: &str) {
        self.matcher.forget();
        self.match_and_follow(user).await;
    }

    pub fn matcher(&self) -> &Arc<ImportMatcher> {
        &self.matcher
    }

    /// How long a library song read for a streaming list is used again.
    const KEEP_SONG_FOR: TimeDelta = TimeDelta::minutes(10);
    /// The most library songs kept for streaming lists across everyone.
    const MOST_SONGS_KEPT: usize = 20_000;

    /// A library song read for this listener's streaming list in the last ten minutes.
    pub fn cached_song(&self, user: &str, id: &str) -> Option<serde_json::Value> {
        let now = self.now();
        self.songs
            .lock()
            .get(&(to_lower_invariant(user), id.to_string()))
            .filter(|(_, at)| now - *at < Self::KEEP_SONG_FOR)
            .map(|(song, _)| song.clone())
    }

    pub fn cache_song(&self, user: &str, id: &str, song: serde_json::Value) {
        let now = self.now();
        let mut songs = self.songs.lock();
        if songs.len() >= Self::MOST_SONGS_KEPT {
            songs.retain(|_, (_, at)| now - *at < Self::KEEP_SONG_FOR);
            if songs.len() >= Self::MOST_SONGS_KEPT {
                songs.clear();
            }
        }
        songs.insert((to_lower_invariant(user), id.to_string()), (song, now));
    }

    /// A heart or rating changed these songs: everyone's kept copy of them is read again.
    pub fn forget_cached_songs(&self, ids: &[String]) {
        let mut songs = self.songs.lock();
        if songs.is_empty() {
            return;
        }
        songs.retain(|(_, id), _| !ids.iter().any(|changed| changed == id));
    }

    pub fn preview_routing(&self, id: &str) -> Option<octo_core::soulseek::SoulseekRouting> {
        self.store.preview_routing(id)
    }

    /// Toggle only the virtual playlist: never starts the trickle or writes to Navidrome.
    pub fn set_streaming(&self, user: &str, id: &str, on: bool) -> ImportActionResult {
        if !self
            .store
            .update(user, id, |list| list.stream_without_downloading = on)
        {
            return ImportActionResult::failed("There is no such list.");
        }
        ImportActionResult::ok(if on {
            "This snapshot is now a playlist in Octo. Missing songs stream from YouTube; heart a song to keep it. Get missing songs remains a separate choice."
        } else {
            "The streaming playlist is no longer shown in Octo."
        })
    }

    pub fn streaming_lists(&self, user: &str) -> Vec<ImportList> {
        if user.trim().is_empty() {
            return Vec::new();
        }
        self.store
            .lists_of(user)
            .into_iter()
            .filter(|list| list.stream_without_downloading)
            .collect()
    }

    /// Resolve only this authenticated listener's snapshot, including after a restart.
    pub fn streaming_list(&self, user: &str, playlist_id: &str) -> Option<ImportList> {
        self.streaming_lists(user)
            .into_iter()
            .find(|list| super::import_streaming::playlist_id(list) == playlist_id)
    }

    /// Re-match local copies on read without refreshing the source. Persist only an actual
    /// change, so the playlist's changed time is stable and an app can notice a landed copy.
    pub async fn matched_streaming_list(&self, user: &str, playlist_id: &str) -> Option<ImportList> {
        let mut list = self.streaming_list(user, playlist_id)?;
        let before = list.tracks.clone();
        let landed = self.reread_library.load(Ordering::Relaxed);
        if !self
            .matcher
            .match_streaming_tracks_for(user, &mut list.tracks, landed)
            .await
        {
            // The stored matches answer now; the next pass reads the library and matches again.
            let mut rematch = self.rematch.lock();
            if !rematch.iter().any(|u| eq_ignore_case(u, user)) {
                rematch.push(user.to_string());
            }
            return Some(list);
        }
        if list.tracks != before {
            list.matched_utc = Some(self.now());
            self.store.update(user, &list.id, |saved| {
                // A simultaneous explicit reimport must keep its new snapshot.
                if saved.tracks == before {
                    saved.tracks.clone_from(&list.tracks);
                    saved.matched_utc = list.matched_utc;
                }
            });
        }
        Some(list)
    }

    // =========================================================================================
    // Reading what there is
    // =========================================================================================

    pub fn overview(&self, user: &str) -> ImportOverview {
        let lists = self.store.lists_of(user);
        let jobs = self.jobs(user);
        ImportOverview {
            lists: lists.iter().map(|list| summary(list, &jobs)).collect(),
            trickle: self.trickle_view(user, &jobs),
            library_problem: self.matcher.problem(),
            approval: self.approval_for(user),
        }
    }

    /// Who approves the missing songs of `user` before they are fetched, when someone must.
    fn approval_for(&self, user: &str) -> Option<ApprovalView> {
        let gate = &self.family.get()?.gate;
        gate.waits_for_approval(user).then(|| ApprovalView {
            needed: true,
            by: gate.approver_name(),
        })
    }

    /// Where people can bring their music from, for the apps and pages.
    pub fn services(&self) -> ImportServices {
        ImportServices::new()
    }

    pub fn detail(&self, user: &str, id: &str) -> Option<ListDetail> {
        let list = self.store.get(user, id)?;
        let jobs = self.jobs(user);
        let rows = self.rows();
        Some(ListDetail {
            list: summary(&list, &jobs),
            tracks: list
                .tracks
                .iter()
                .map(|track| track_view(track, &jobs, &rows))
                .collect(),
        })
    }

    /// One person's jobs by song, oldest first.
    fn jobs(&self, user: &str) -> IndexMap<String, TrickleJob> {
        let mut jobs = IndexMap::new();
        for job in self.queue.snapshot(Some(user)) {
            jobs.entry(job.key.clone()).or_insert(job);
        }
        jobs
    }

    fn rows(&self) -> HashMap<String, AcquisitionSnapshot> {
        let mut rows = HashMap::new();
        for row in self.tracker.as_ref().map(|t| t.all()).unwrap_or_default() {
            rows.entry(ordinal_ignore_case_key(&key_of(&row.provider, &row.external_id)))
                .or_insert(row);
        }
        rows
    }

    fn trickle_view(&self, user: &str, jobs: &IndexMap<String, TrickleJob>) -> TrickleView {
        let status = self.trickle.status_for(user);
        let rows = self.rows();
        let all: Vec<&TrickleJob> = jobs.values().collect();
        let count = |state: &str| all.iter().filter(|job| job.state == state).count() as i32;
        // MaxBy: the first of the latest.
        let mut current: Option<&TrickleJob> = None;
        for job in all
            .iter()
            .filter(|job| job.state == import_track_states::DOWNLOADING)
        {
            if current.is_none_or(|c| job.started_utc > c.started_utc) {
                current = Some(job);
            }
        }
        let current = current.map(|job| job_view(job, &rows));
        let mut recent: Vec<&&TrickleJob> = all
            .iter()
            .filter(|job| !import_track_states::open(&job.state))
            .collect();
        recent.sort_by_key(|job| std::cmp::Reverse(job.updated_utc));
        let next = all
            .iter()
            .filter(|job| job.state == import_track_states::QUEUED)
            .take(5)
            .map(|job| job_view(job, &rows))
            .collect();
        TrickleView {
            state: status.state.to_string(),
            label: super::trickle_worker::trickle_states::label(
                status.state,
                count(import_track_states::DOWNLOADING) > 0,
            )
            .to_string(),
            next_utc: status.next_utc,
            per_hour: status.per_hour,
            queued: count(import_track_states::QUEUED),
            downloading: count(import_track_states::DOWNLOADING),
            done: count(import_track_states::DONE),
            not_found: count(import_track_states::NOT_FOUND),
            skipped: count(import_track_states::SKIPPED),
            current,
            recent: recent
                .into_iter()
                .take(20)
                .map(|job| job_view(job, &rows))
                .collect(),
            next,
        }
    }

    // =========================================================================================
    // Adding lists
    // =========================================================================================

    /// A public playlist or album, by its link. No sign-in needed. `Err` when the link could not
    /// be read for a reason other than Spotify's own refusal.
    pub async fn add_link(&self, user: &str, link: &str) -> anyhow::Result<ImportActionResult> {
        let Some((kind, id)) = SpotifyLinkReader::parse(Some(link)) else {
            return Ok(ImportActionResult::failed(
                "That is not a link to a Spotify playlist or album.",
            ));
        };
        let gate = self.gate(user);
        let _held = gate.lock().await;
        let read = match self.links.read(link).await {
            Ok(read) => read,
            Err(e) => {
                return match e.downcast_ref::<SpotifyException>() {
                    Some(refused) => Ok(ImportActionResult::failed(refused.message.clone())),
                    None => Err(e),
                };
            }
        };
        let list_id = format!("link-{kind}-{id}");
        if let Some(full) = self.room_for(user, &[(list_id.clone(), read.tracks.len())]) {
            return Ok(ImportActionResult::failed(full));
        }
        let count = read.tracks.len();
        self.store.save(ImportList {
            id: list_id.clone(),
            owner: user.to_string(),
            name: read.name.clone(),
            source: import_sources::LINK.to_string(),
            source_ref: Some(format!("https://open.spotify.com/{kind}/{id}")),
            image_url: read.image_url.clone(),
            total: count as i32,
            tracks: read.tracks,
            read_utc: self.now(),
            partial: read.capped.then(|| {
                format!(
                    "A link shows only the first {} songs. Export the list with TuneMyMusic and import the file for the whole list.",
                    SpotifyLinkReader::EMBED_CAP
                )
            }),
            ..Default::default()
        });
        self.forget_unheld_answers();
        self.match_and_follow(user).await;
        Ok(
            ImportActionResult::ok(format!("Added {}, {count} songs.", read.name))
                .with_list(Some(list_id))
                .with_count(count as i32),
        )
    }

    /// The lists in an exported file: CSV, Spotify's data export (JSON), its zip, or a plain list
    /// of one song a line, read whichever way round names more of the person's library.
    pub async fn add_file(&self, user: &str, file_name: &str, content: &[u8]) -> ImportActionResult {
        let lists = match ImportFileReader::read_parts(file_name, content) {
            Ok(FileRead::Lists(lists)) => lists,
            Ok(FileRead::Text(lines)) => self.oriented(user, lines).await,
            Err(e) => {
                // Octo's own words for a file it refuses; a parser's are cut to their first
                // plain sentence.
                let words = match &e {
                    FileReadError::Format(message) => message.clone(),
                    other => format!(
                        "{}.",
                        user_safe(Some(&other.to_string()))
                            .unwrap_or_else(|| "it is not a file Octo knows".to_string())
                    ),
                };
                return ImportActionResult::failed(format!("Octo could not read that file: {words}"));
            }
        };
        if lists.is_empty() {
            return ImportActionResult::failed(
                "That file held no songs Octo could read. It needs a title and an artist for each song.",
            );
        }
        let source_ref = file_name.rsplit('/').next().unwrap_or(file_name).to_string();
        self.save_file_lists(user, &source_ref, lists).await
    }

    /// The name a pasted list gets when it is given none.
    pub const PASTED_LIST: &str = "Pasted list";

    /// A pasted list, one song a line, read whichever way round names more of the person's
    /// library. Kept as a list named `name`, like a file of that name.
    pub async fn add_text(&self, user: &str, name: Option<&str>, text: &str) -> ImportActionResult {
        let name = name
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(Self::PASTED_LIST);
        if text.len() as u64 > ImportFileReader::MAX_BYTES {
            return ImportActionResult::failed(
                "That is more text than Octo reads at once. Split it into smaller lists.",
            );
        }
        let lines = match text_list::read_lines(name, text) {
            Ok(lines) => lines,
            Err(e) => return ImportActionResult::failed(e.to_string()),
        };
        let lists = self.oriented(user, lines).await;
        if lists.is_empty() {
            return ImportActionResult::failed(text_list::NO_SONGS);
        }
        self.save_file_lists(user, name, lists).await
    }

    /// A plain list's lines read the way round that names more of `user`'s library.
    async fn oriented(&self, user: &str, lists: Vec<TextList>) -> Vec<FileList> {
        let sample: Vec<TextLine> = lists
            .iter()
            .flat_map(|list| list.lines.iter())
            .filter(|line| line.right.is_some())
            .take(text_list::ORIENTATION_SAMPLE)
            .cloned()
            .collect();
        let orientation = self.matcher.orientation_for(user, &sample).await;
        text_list::to_file_lists(lists, orientation)
    }

    /// Keeps lists read from a file or pasted text, each by its name, and matches them.
    async fn save_file_lists(
        &self,
        user: &str,
        source_ref: &str,
        lists: Vec<FileList>,
    ) -> ImportActionResult {
        let gate = self.gate(user);
        let _held = gate.lock().await;
        let incoming: Vec<(String, usize)> = lists
            .iter()
            .map(|list| (format!("file-{}", hash(&list.name)), list.tracks.len()))
            .collect();
        if let Some(full) = self.room_for(user, &incoming) {
            return ImportActionResult::failed(full);
        }
        let mut first: Option<String> = None;
        let source_ref = source_ref.to_string();
        let songs: usize = lists.iter().map(|list| list.tracks.len()).sum();
        let count = lists.len();
        let first_name = lists[0].name.clone();
        for list in lists {
            // By name, so a newer export of the same playlist updates it rather than adding a second.
            let id = format!("file-{}", hash(&list.name));
            first.get_or_insert_with(|| id.clone());
            self.store.save(ImportList {
                id,
                owner: user.to_string(),
                name: list.name,
                source: import_sources::FILE.to_string(),
                source_ref: Some(source_ref.clone()),
                total: list.tracks.len() as i32,
                tracks: list.tracks,
                read_utc: self.now(),
                ..Default::default()
            });
        }
        self.forget_unheld_answers();
        self.match_and_follow(user).await;
        ImportActionResult::ok(if count == 1 {
            format!("Added {first_name}, {songs} songs.")
        } else {
            format!("Added {count} lists, {songs} songs.")
        })
        .with_list(first)
        .with_count(songs as i32)
    }

    /// Why these lists would not fit beside the person's others, or None. A list that takes the
    /// place of one with the same id counts once.
    fn room_for(&self, user: &str, incoming: &[(String, usize)]) -> Option<String> {
        let mut ids: Vec<&str> = Vec::new();
        for (id, _) in incoming {
            if !ids.contains(&id.as_str()) {
                ids.push(id);
            }
        }
        let kept: Vec<ImportList> = self
            .store
            .lists_of(user)
            .into_iter()
            .filter(|list| !ids.contains(&list.id.as_str()))
            .collect();
        if kept.len() + ids.len() > Self::MAX_LISTS {
            return Some(format!(
                "That would make more than {} lists, the most Octo keeps for one person. Remove some first.",
                Self::MAX_LISTS
            ));
        }
        let songs: usize = kept.iter().map(|list| list.tracks.len()).sum::<usize>()
            + incoming.iter().map(|(_, n)| n).sum::<usize>();
        if songs > Self::MAX_SONGS {
            return Some(
                "That would make more than 250,000 songs across your lists, the most Octo keeps for one person. Remove some first."
                    .to_string(),
            );
        }
        None
    }

    /// Reads one list again from where it came from: the page for a link. Any other list is
    /// matched against the library again.
    pub async fn refresh(self: &Arc<Self>, user: &str, id: &str) -> anyhow::Result<ImportActionResult> {
        let Some(list) = self.store.get(user, id) else {
            return Ok(ImportActionResult::failed("There is no such list."));
        };
        match list.source.as_str() {
            import_sources::LINK if list.source_ref.is_some() => {
                self.add_link(user, list.source_ref.as_deref().unwrap_or_default())
                    .await
            }
            _ => {
                let gate = self.gate(user);
                let _held = gate.lock().await;
                self.match_and_follow(user).await;
                Ok(ImportActionResult::ok("Matched against your library again."))
            }
        }
    }

    // =========================================================================================
    // What to do with a list
    // =========================================================================================

    /// Keep the list as a Navidrome playlist, or stop. Stopping leaves the playlist where it is.
    pub async fn set_playlist(&self, user: &str, id: &str, on: bool) -> ImportActionResult {
        if !self.store.update(user, id, |list| list.keep_playlist = on) {
            return ImportActionResult::failed("There is no such list.");
        }
        if !on {
            return ImportActionResult::ok(
                "Octo no longer changes this playlist. It stays in Navidrome as it is.",
            );
        }
        self.write_playlist(user, id).await.unwrap_or_else(|| {
            ImportActionResult::failed(
                "Navidrome could not be reached to make the playlist. Octo tries again shortly.",
            )
        })
    }

    /// Gives imports Family's write gate and requests. Once only.
    pub fn use_family(&self, family: ImportFamily) {
        let _ = self.family.set(family);
    }

    /// What a list's missing songs do for `user`: fetch (everyone outside the family, members
    /// who add songs at once, and a list an approver said yes to), become one request for the
    /// list, or nothing (an account that only saves songs).
    fn list_write(&self, user: &str, list: &ImportList) -> ListWrite {
        let Some(family) = self.family.get() else {
            return ListWrite::Fetch;
        };
        match family.gate.decide(Some(user)) {
            WriteDecision::Shared | WriteDecision::Member(_) => ListWrite::Fetch,
            WriteDecision::Request { .. } if list.family_approved => ListWrite::Fetch,
            WriteDecision::Request { full, .. } => ListWrite::Ask(full),
            WriteDecision::Refuse { message, .. } => ListWrite::Refuse(message),
        }
    }

    /// One request for a whole list's missing songs.
    fn ask_for_list(&self, user: &str, list: &ImportList, full: Option<String>) -> ImportActionResult {
        let Some(requests) = self.family.get().and_then(|f| f.requests.upgrade()) else {
            return ImportActionResult::failed("Requests are not running.");
        };
        let facts = RequestFacts {
            title: list.name.clone(),
            artist: list.by.clone().unwrap_or_default(),
            album: String::new(),
            cover_art: list.image_url.clone().unwrap_or_default(),
        };
        match requests.create_import(user, &list.id, facts) {
            Ok(_) => ImportActionResult::ok(match full {
                Some(full) => format!("{full}. {LIST_ASKED}"),
                None => LIST_ASKED.to_string(),
            })
            .with_list(Some(list.id.clone())),
            Err(e) => ImportActionResult::failed(e.message),
        }
    }

    /// An approver said yes to a family member's list: it fetches its missing songs, and every
    /// song added to it later, like anyone's.
    pub fn approve_family_list(&self, user: &str, id: &str) -> ImportActionResult {
        if self.store.get(user, id).is_none() {
            return ImportActionResult::failed("That list is gone.");
        }
        self.store.update(user, id, |l| l.family_approved = true);
        self.set_get_missing(user, id, true)
    }

    /// Fetch the list's missing songs, and every song added to it later, or stop.
    pub fn set_get_missing(&self, user: &str, id: &str, on: bool) -> ImportActionResult {
        let Some(list) = self.store.get(user, id) else {
            return ImportActionResult::failed("There is no such list.");
        };
        if on {
            match self.list_write(user, &list) {
                ListWrite::Fetch => {}
                ListWrite::Ask(full) => return self.ask_for_list(user, &list, full),
                ListWrite::Refuse(message) => return ImportActionResult::failed(message),
            }
        }
        // Never on a guess: a list nobody could match against the library would fetch songs
        // already there.
        if on && list.matched_utc.is_none() {
            return ImportActionResult::failed(self.matcher.problem().unwrap_or_else(|| {
                "Octo has not checked this list against your library yet. Try again in a moment.".to_string()
            }));
        }
        self.store.update(user, id, |l| l.get_missing = on);
        if on {
            let queued = self.queue.add(
                user,
                list.tracks.iter().filter(|track| track.library_id.is_none()),
            );
            return if queued == 0 {
                ImportActionResult::ok(
                    "Nothing new to fetch: every song is in your library or already queued.",
                )
            } else {
                ImportActionResult::ok(format!(
                    "Queued {queued} {}. Octo fetches up to {} an hour.",
                    if queued == 1 { "song" } else { "songs" },
                    self.settings.current().imports.effective_songs_per_hour()
                ))
                .with_count(queued)
            };
        }
        // Songs still waiting that no other fetching list wants go back out of the queue.
        let wanted: Vec<String> = self
            .store
            .lists_of(user)
            .into_iter()
            .filter(|l| l.get_missing)
            .flat_map(|l| l.tracks.into_iter().map(|t| t.key))
            .collect();
        let keys: Vec<String> = list
            .tracks
            .iter()
            .map(|t| t.key.clone())
            .filter(|key| !wanted.contains(key))
            .collect();
        let cancelled = self.queue.cancel_queued(user, Some(&keys));
        if cancelled == 0 {
            ImportActionResult::ok("Stopped fetching this list.")
        } else {
            ImportActionResult::ok(format!(
                "Stopped fetching this list; {cancelled} waiting {} taken back.",
                if cancelled == 1 { "song was" } else { "songs were" }
            ))
            .with_count(cancelled)
        }
    }

    /// Queue chosen songs of a list, whether or not the list fetches the rest.
    pub fn get_songs(&self, user: &str, id: &str, keys: &[String]) -> ImportActionResult {
        let Some(list) = self.store.get(user, id) else {
            return ImportActionResult::failed("There is no such list.");
        };
        match self.list_write(user, &list) {
            ListWrite::Fetch => {}
            ListWrite::Ask(full) => return self.ask_for_list(user, &list, full),
            ListWrite::Refuse(message) => return ImportActionResult::failed(message),
        }
        let songs: Vec<&ImportTrack> = list
            .tracks
            .iter()
            .filter(|track| track.library_id.is_none() && keys.contains(&track.key))
            .collect();
        let chosen: Vec<&str> = songs.iter().map(|song| song.key.as_str()).collect();
        self.queue.retry(user, &chosen);
        let queued = self.queue.add(user, songs.iter().copied());
        ImportActionResult::ok(format!(
            "Queued {} {}.",
            songs.len(),
            if songs.len() == 1 { "song" } else { "songs" }
        ))
        .with_count(queued)
    }

    pub fn remove(&self, user: &str, id: &str) -> ImportActionResult {
        let Some(list) = self.store.get(user, id) else {
            return ImportActionResult::failed("There is no such list.");
        };
        if list.get_missing {
            self.set_get_missing(user, id, false);
        }
        self.store.remove(user, id);
        self.forget_unheld_answers();
        ImportActionResult::ok(if list.playlist_id.is_none() {
            format!("Removed {}.", list.name)
        } else {
            format!(
                "Removed {}. Its playlist stays in Navidrome; delete it there if you no longer want it.",
                list.name
            )
        })
    }

    // ---- The trickle -------------------------------------------------------------------------

    pub fn pause(&self, user: &str, paused: bool) -> ImportActionResult {
        self.queue.set_paused(user, paused);
        ImportActionResult::ok(if paused {
            "Paused. A song already running finishes."
        } else {
            "Going again."
        })
    }

    pub fn retry(&self, user: &str, keys: Option<&[String]>) -> ImportActionResult {
        let chosen: Vec<String> = match keys.filter(|k| !k.is_empty()) {
            Some(keys) => keys.to_vec(),
            None => self
                .queue
                .snapshot(Some(user))
                .into_iter()
                .filter(|job| job.state == import_track_states::NOT_FOUND)
                .map(|job| job.key)
                .collect(),
        };
        let count = self.queue.retry(user, &chosen);
        if count == 0 {
            ImportActionResult::ok("Nothing to try again.")
        } else {
            ImportActionResult::ok(format!(
                "Trying {count} {} again.",
                if count == 1 { "song" } else { "songs" }
            ))
            .with_count(count)
        }
    }

    pub fn skip(&self, user: &str, keys: &[String]) -> ImportActionResult {
        let count = self.queue.skip(user, keys);
        if count == 0 {
            ImportActionResult::ok("Nothing was waiting to skip.")
        } else {
            ImportActionResult::ok(format!(
                "Skipped {count} {}.",
                if count == 1 { "song" } else { "songs" }
            ))
            .with_count(count)
        }
    }

    pub fn clear_finished(&self, user: &str) -> ImportActionResult {
        let count = self.queue.clear_finished(user);
        ImportActionResult::ok(format!(
            "Cleared {count} finished {}.",
            if count == 1 { "song" } else { "songs" }
        ))
        .with_count(count)
    }

    // =========================================================================================
    // Matching and following
    // =========================================================================================

    /// Matches every list of this person, queues new missing songs of fetching lists, and
    /// writes their playlists.
    pub async fn match_and_follow(&self, user: &str) {
        let lists = self.store.lists_of(user);
        let mut tracks: Vec<ImportTrack> = lists.into_iter().flat_map(|list| list.tracks).collect();
        if !self.matcher.match_tracks_for(user, &mut tracks).await {
            return;
        }
        let mut found: HashMap<String, Option<String>> = HashMap::new();
        for track in &tracks {
            found
                .entry(track.key.clone())
                .or_insert_with(|| track.library_id.clone());
        }
        let now = self.now();
        self.store.update_all(user, |list| {
            for track in &mut list.tracks {
                if let Some(id) = found.get(&track.key) {
                    track.library_id = id.clone();
                }
            }
            list.matched_utc = Some(now);
        });
        if self.matcher.has_pending_isrcs(&tracks) {
            self.learn_later(user);
        }
        for list in self.store.lists_of(user) {
            if list.get_missing {
                self.queue.add(
                    user,
                    list.tracks.iter().filter(|track| track.library_id.is_none()),
                );
            }
            if list.keep_playlist {
                self.mark_dirty(format!("{user}\n{}", list.id));
            }
        }
        self.write_dirty_playlists().await;
    }

    /// Asks about this person's waiting ISRCs in the loop's next pass.
    fn learn_later(&self, user: &str) {
        let mut learning = self.learning.lock();
        if !learning.iter().any(|u| eq_ignore_case(u, user)) {
            learning.push(user.to_string());
        }
    }

    /// Matches this person again in the loop's next pass.
    fn rematch_later(&self, user: &str) {
        let mut rematch = self.rematch.lock();
        if !rematch.iter().any(|u| eq_ignore_case(u, user)) {
            rematch.push(user.to_string());
        }
    }

    fn mark_dirty(&self, entry: String) {
        let mut dirty = self.dirty_playlists.lock();
        if !dirty.contains(&entry) {
            dirty.push(entry);
        }
    }

    async fn write_playlist(&self, user: &str, id: &str) -> Option<ImportActionResult> {
        let list = self.store.get(user, id).filter(|list| list.keep_playlist)?;
        let result = self.playlists.sync(&list).await;
        let entry = format!("{user}\n{id}");
        if !result.ok {
            // Keep the id of a playlist the sync left in Navidrome (one it could neither fill nor
            // delete), so the next try fills it rather than making another, and wait longer each
            // time rather than every 30 seconds.
            if result.playlist_id != list.playlist_id {
                self.store.update(user, id, |l| {
                    l.playlist_id = result.playlist_id.clone();
                    l.playlist_note = result.note.clone();
                });
            }
            self.back_off(&entry);
            self.mark_dirty(entry);
            return None;
        }
        self.playlist_retries.lock().remove(&entry);
        self.store.update(user, id, |l| {
            l.playlist_id = result.playlist_id.clone();
            l.playlist_note = result.note.clone();
            if result.deleted {
                l.keep_playlist = false;
            }
        });
        Some(if result.deleted {
            ImportActionResult::ok(
                "The playlist was deleted in Navidrome, so Octo stopped keeping it. Turn it on again to make a new one.",
            )
        } else {
            ImportActionResult::ok(format!(
                "The playlist holds the {} {} in your library. Octo adds the rest as they arrive.",
                result.songs,
                if result.songs == 1 { "song" } else { "songs" }
            ))
        })
    }

    /// Notes one more failure of a playlist's write and when it may be tried again.
    fn back_off(&self, entry: &str) {
        let now = self.now();
        let mut retries = self.playlist_retries.lock();
        let failures = retries.get(entry).map_or(0, |(n, _)| *n) + 1;
        let wait = Self::PLAYLIST_RETRY_FIRST
            .checked_mul(1 << (failures - 1).min(10))
            .unwrap_or(Self::PLAYLIST_RETRY_MOST)
            .min(Self::PLAYLIST_RETRY_MOST);
        warn!(
            "Could not write the playlist for {}; trying again in {} minutes",
            entry.replace('\n', "/"),
            wait.num_minutes()
        );
        retries.insert(entry.to_string(), (failures, now + wait));
    }

    async fn write_dirty_playlists(&self) {
        let entries: Vec<String> = self.dirty_playlists.lock().clone();
        let now = self.now();
        for entry in entries {
            // Not yet: a playlist that would not write waits out its back-off, still marked.
            if self
                .playlist_retries
                .lock()
                .get(&entry)
                .is_some_and(|(_, next)| *next > now)
            {
                continue;
            }
            {
                let mut dirty = self.dirty_playlists.lock();
                let Some(at) = dirty.iter().position(|e| *e == entry) else {
                    continue;
                };
                dirty.remove(at);
            }
            let Some((user, id)) = entry.split_once('\n') else {
                continue;
            };
            self.write_playlist(user, id).await;
        }
    }

    /// A song the trickle finished: when it was fetched, the library is read again on the next
    /// pass.
    fn on_fetched(&self, job: &TrickleJob) {
        if job.state != import_track_states::DONE {
            return;
        }
        self.matcher.forget();
        let mut rematch = self.rematch.lock();
        if !rematch.iter().any(|u| eq_ignore_case(u, &job.owner)) {
            rematch.push(job.owner.clone());
        }
    }

    fn gate(&self, user: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.gates
            .lock()
            .entry(ordinal_ignore_case_key(user))
            .or_default()
            .clone()
    }

    /// Forgets what the music database said about ISRCs no list holds and no waiting song
    /// carries any more.
    fn forget_unheld_answers(&self) {
        // What was known before looking at what is held: an answer learned meanwhile, for a
        // list saved meanwhile, is not dropped.
        let known = self.matcher.known_isrcs();
        let mut held = self.store.held_isrcs();
        held.extend(self.queue.open_isrcs());
        #[cfg(test)]
        if let Some(hook) = self.after_held_look.lock().take() {
            hook();
        }
        self.matcher.forget_unheld(&known, &held);
    }

    /// One round of asking about the ISRCs of everyone whose songs wait for answers. An answer
    /// that names a recording a library file is tagged with matches that person again on the
    /// next pass. Answers for ISRCs nothing holds any more are forgotten before and after; a
    /// round stops at the first question the music database cannot answer, and that person goes
    /// behind everyone else. A person leaves the round's list only once nothing of theirs waits,
    /// so a round that stops halfway loses nobody.
    pub async fn learn_once(&self) {
        self.forget_unheld_answers();
        let waiting: Vec<String> = self.learning.lock().clone();
        for user in waiting {
            let tracks = self.learning_tracks(&user);
            let learned = self.matcher.learn_isrcs(&tracks).await;
            if learned.in_library > 0 {
                self.rematch_later(&user);
            }
            if !self.matcher.has_pending_isrcs(&tracks) {
                self.learning.lock().retain(|u| !eq_ignore_case(u, &user));
            }
            // The music database could not answer: the rest wait for the next round, this person
            // behind them, so an ISRC that never gets an answer keeps nobody else waiting.
            if learned.failed {
                let mut learning = self.learning.lock();
                if let Some(at) = learning.iter().position(|u| eq_ignore_case(u, &user)) {
                    let me = learning.remove(at);
                    learning.push(me);
                }
                break;
            }
        }
        // An answer that came in for a list removed meanwhile is not kept.
        self.forget_unheld_answers();
    }

    /// This person's songs whose ISRCs may wait for an answer: every list's, and the songs the
    /// trickle still has queued, which may carry an ISRC no list holds any more.
    fn learning_tracks(&self, user: &str) -> Vec<ImportTrack> {
        let mut tracks: Vec<ImportTrack> = self
            .store
            .lists_of(user)
            .into_iter()
            .flat_map(|list| list.tracks)
            .collect();
        tracks.extend(
            self.queue
                .snapshot(Some(user))
                .iter()
                .filter(|job| job.state == import_track_states::QUEUED)
                .map(TrickleJob::as_track),
        );
        tracks
    }

    /// Waits for the round of learning running now, if there is one.
    #[cfg(test)]
    pub async fn drain_learner(&self) {
        let task = self.learner.lock().take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    /// One pass of the loop: matches people the trickle brought songs, writes playlists, and
    /// reads stale links again.
    pub async fn loop_once(self: &Arc<Self>) -> anyhow::Result<()> {
        if self.reread_library.swap(false, Ordering::Relaxed) {
            self.matcher.forget();
        }
        // An answer the trickle's last look learned for a list removed meanwhile goes too.
        self.forget_unheld_answers();
        // The first pass after a start picks up every list that still waits for answers.
        if !self.learning_seeded.swap(true, Ordering::Relaxed) {
            for list in self.store.all() {
                if self.matcher.has_pending_isrcs(&list.tracks) {
                    self.learn_later(&list.owner);
                }
            }
        }
        // Learning runs beside the loop: one round at a time, never holding up a pass.
        let running = self
            .learner
            .lock()
            .as_ref()
            .is_some_and(|task| !task.is_finished());
        if !running && !self.learning.lock().is_empty() {
            let service = Arc::clone(self);
            *self.learner.lock() = Some(tokio::spawn(async move { service.learn_once().await }));
        }
        let users: Vec<String> = std::mem::take(&mut *self.rematch.lock());
        for user in users {
            let gate = self.gate(&user);
            let Ok(_held) = gate.try_lock() else {
                self.rematch.lock().push(user);
                continue;
            };
            self.match_and_follow(&user).await;
        }
        self.write_dirty_playlists().await;

        let hours = self.settings.current().imports.refresh_hours;
        if hours <= 0 {
            return Ok(());
        }
        let stale = self.now() - TimeDelta::hours(i64::from(hours));
        let followed: Vec<ImportList> = self
            .store
            .all()
            .into_iter()
            .filter(|list| {
                (list.keep_playlist || list.get_missing)
                    && !list.gone
                    && list.read_utc < stale
                    && list.source == import_sources::LINK
                    && list.source_ref.is_some()
            })
            .collect();
        for list in &followed {
            let link = list.source_ref.clone().unwrap_or_default();
            let read = self.add_link(&list.owner, &link).await?;
            // A link that no longer reads is tried again next time round, not every pass.
            if !read.ok {
                debug!("Could not read {link} again: {}", read.message);
                let now = self.now();
                self.store.update(&list.owner, &list.id, |l| l.read_utc = now);
            }
        }
        Ok(())
    }

    /// The hosted loop: a pass every 30 seconds until Octo stops.
    pub async fn run_loop(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        while !stopping.is_cancelled() {
            // Per-pass catch: one failed pass must not stop the loop.
            let service = self.clone();
            match tokio::spawn(async move { service.loop_once().await }).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => error!("Import loop failed: {e}"),
                Err(e) => error!("Import loop failed: {e}"),
            }
            tokio::select! {
                () = stopping.cancelled() => break,
                () = tokio::time::sleep(Self::LOOP_EVERY) => {}
            }
        }
        Ok(())
    }
}

fn hash(text: &str) -> String {
    let digest = Sha256::digest(to_lower_invariant(text).as_bytes());
    hex::encode(digest)[..12].to_string()
}

fn state_of(track: &ImportTrack, jobs: &IndexMap<String, TrickleJob>) -> String {
    if track.library_id.is_some() {
        return import_track_states::HAVE.to_string();
    }
    jobs.get(&track.key).map_or_else(
        || import_track_states::MISSING.to_string(),
        |job| job.state.clone(),
    )
}

fn summary(list: &ImportList, jobs: &IndexMap<String, TrickleJob>) -> ListSummary {
    let states: Vec<String> = list.tracks.iter().map(|track| state_of(track, jobs)).collect();
    let count = |state: &str| states.iter().filter(|s| *s == state).count() as i32;
    ListSummary {
        id: list.id.clone(),
        name: list.name.clone(),
        source: list.source.clone(),
        by: list.by.clone(),
        image_url: list.image_url.clone(),
        total: list.total.max(list.tracks.len() as i32),
        have: count(import_track_states::HAVE),
        missing: count(import_track_states::MISSING),
        queued: count(import_track_states::QUEUED),
        downloading: count(import_track_states::DOWNLOADING),
        done: count(import_track_states::DONE),
        not_found: count(import_track_states::NOT_FOUND),
        skipped: count(import_track_states::SKIPPED),
        keep_playlist: list.keep_playlist,
        stream_without_downloading: list.stream_without_downloading,
        streaming_playlist_id: list
            .stream_without_downloading
            .then(|| super::import_streaming::playlist_id(list)),
        playlist_id: list.playlist_id.clone(),
        playlist_note: list.playlist_note.clone(),
        get_missing: list.get_missing,
        partial: if import_sources::from_account(&list.source) {
            Some(ACCOUNT_LIST_NOTE.to_string())
        } else {
            list.partial.clone()
        },
        gone: list.gone,
        can_refresh: import_sources::can_refresh(&list.source),
        read_utc: list.read_utc,
        matched_utc: list.matched_utc,
    }
}

fn track_view(
    track: &ImportTrack,
    jobs: &IndexMap<String, TrickleJob>,
    rows: &HashMap<String, AcquisitionSnapshot>,
) -> TrackView {
    let state = state_of(track, jobs);
    let job = jobs.get(&track.key);
    let (detail, progress) = if state == import_track_states::HAVE {
        (None, None)
    } else {
        words(job, rows)
    };
    TrackView {
        key: track.key.clone(),
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        seconds: track.seconds,
        state,
        detail,
        library_id: track
            .library_id
            .clone()
            .or_else(|| job.and_then(|j| j.library_id.clone())),
        progress,
        updated_utc: job.map(|j| j.updated_utc),
    }
}

fn job_view(job: &TrickleJob, rows: &HashMap<String, AcquisitionSnapshot>) -> TrackView {
    let (detail, progress) = words(Some(job), rows);
    TrackView {
        key: job.key.clone(),
        title: job.title.clone(),
        artist: job.artist.clone(),
        album: job.album.clone(),
        seconds: job.seconds,
        state: job.state.clone(),
        detail,
        library_id: job.library_id.clone(),
        progress,
        updated_utc: Some(job.updated_utc),
    }
}

/// What a song is waiting for or what happened to it, from its download's own row while it
/// runs.
fn words(
    job: Option<&TrickleJob>,
    rows: &HashMap<String, AcquisitionSnapshot>,
) -> (Option<String>, Option<f64>) {
    let Some(job) = job else {
        return (None, None);
    };
    let row = (job.state == import_track_states::DOWNLOADING)
        .then_some(job.acquisition_key.as_ref())
        .flatten()
        .and_then(|key| rows.get(&ordinal_ignore_case_key(key)));
    let Some(row) = row else {
        return (
            job.detail.clone().or_else(|| {
                (job.state == import_track_states::DOWNLOADING).then(|| "Checking your library".to_string())
            }),
            None,
        );
    };
    let source = row.source.as_deref();
    let mut words = match row.state {
        AcquisitionState::Queued => Some("Waiting for a download slot".to_string()),
        AcquisitionState::Searching => {
            Some(source.map_or("Searching".to_string(), |s| format!("Searching {s}")))
        }
        AcquisitionState::Downloading => {
            Some(source.map_or("Downloading".to_string(), |s| format!("Downloading from {s}")))
        }
        AcquisitionState::Verifying => Some("Checking it is the right song".to_string()),
        AcquisitionState::Importing => Some("Waiting for Navidrome to show it".to_string()),
        _ => job.detail.clone(),
    };
    if let Some(note) = row.note.as_ref().filter(|n| !n.is_empty()) {
        words = Some(format!("{}. {note}", words.unwrap_or_default()));
    }
    (
        words,
        (row.state == AcquisitionState::Downloading)
            .then_some(row.progress)
            .flatten(),
    )
}

#[cfg(test)]
#[path = "import_service_tests.rs"]
mod tests;
