//! The external id registry, the store behind `external-ids.json`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use indexmap::IndexMap;
use octo_core::common::SongIdentity;
use octo_core::fingerprint::heart_gate::{AlbumContext, HeardRecord, PlayedMark};
use octo_core::soulseek::{LengthSource, RoutingKind, SongLength, SoulseekRouting, StreamFacts};
use parking_lot::{Mutex, MutexGuard};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use super::SoulseekMetadataService;
use super::stream_choices::StreamChoices;
use crate::services::state_file;

/// Told of every heard record the registry stores: the registered routing's short id, the user
/// and the record.
pub type HeardListener = Arc<dyn Fn(&str, &str, &HeardRecord) + Send + Sync>;

/// A song id resolved to its routing: the id it is (or would be) registered under, and whether
/// the registry holds it.
struct Resolved {
    id: String,
    routing: SharedRouting,
    registered: bool,
}

/// A routing the registry and its callers share. The registry keeps the very routing a caller
/// registered and hands the same one back from `lookup`, and callers change it in place
/// (pinning a video, settling a catalog artist), so it is one routing behind a mutex. The
/// mutex also guards the routing's [`SongLength`] updates.
///
/// Lock order: the registry never takes a routing's lock while holding its own, so a caller
/// may hold a routing locked and still call into the registry.
#[derive(Debug, Clone, Default)]
pub struct SharedRouting(Arc<Mutex<SoulseekRouting>>);

impl SharedRouting {
    pub fn new(routing: SoulseekRouting) -> Self {
        SharedRouting(Arc::new(Mutex::new(routing)))
    }

    /// The routing, locked for reading or changing in place.
    pub fn lock(&self) -> MutexGuard<'_, SoulseekRouting> {
        self.0.lock()
    }

    /// A copy of the routing as it is now.
    pub fn snapshot(&self) -> SoulseekRouting {
        self.0.lock().clone()
    }

    /// Whether both are the same routing, not merely equal ones.
    pub fn ptr_eq(&self, other: &SharedRouting) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// `SongLength::remember` under the routing's lock.
    pub fn remember_length(&self, seconds: Option<i32>, source: LengthSource) -> bool {
        SongLength::remember(&mut self.0.lock(), seconds, source)
    }

    /// `SongLength::shown` under the routing's lock.
    pub fn shown_length(&self) -> (Option<i32>, LengthSource) {
        SongLength::shown(&self.0.lock())
    }
}

impl From<SoulseekRouting> for SharedRouting {
    fn from(routing: SoulseekRouting) -> Self {
        SharedRouting::new(routing)
    }
}

/// Server-side registry that maps short opaque IDs (Navidrome-shaped 22-char base62)
/// to Soulseek/YouTube routing info. Subsonic clients are picky about song-id format:
/// some quietly drop entries with long pipe-delimited IDs from their play queues.
/// Translating to a short, alphabetic-looking key avoids that whole class of issue.
///
/// IDs are deterministic (sha256-derived) so the same routing always produces the
/// same id; this keeps caches/de-duplication on the client side stable across calls.
/// The dictionary is LRU-bounded so a single user can't grow it without limit.
///
/// ## Persistence
///
/// The registry is the ONLY thing that knows an id is ours: `parse_external_id` decides
/// "external" by looking it up here. So when this was memory-only, every restart made
/// every id a client still held look local, and those ids were relayed to Navidrome,
/// which has no such media and answers error 70 "data not found". Clients surface that
/// per play and per poll, which reads as a stream of errors from a working server.
///
/// Ids are deterministic, so re-running the search that minted one brings it back. That
/// is a recovery, not a design: a queue built before a restart has no reason to search
/// again. Writing the map down is what makes an id outlive the process.
///
/// Flushing: [`ExternalIdRegistry::flush`] writes when something changed. It runs every 10 s
/// from [`ExternalIdRegistry::run_flusher`] (a worker, which flushes once more on shutdown) and
/// once more on `Drop`.
///
/// ## Kept ids
///
/// Playlists keep songs found online by their registry id. Those ids are kept
/// ([`ExternalIdRegistry::set_kept`]): never trimmed, one live routing per id that every reader
/// shares, written in the same file with the keys that keep them. An id no key holds any more
/// goes back to the LRU with its own recency stamp.
pub struct ExternalIdRegistry {
    path: Option<PathBuf>,
    lru: Mutex<Lru>,
    dirty: AtomicBool,
    flush_lock: Mutex<()>,
    recovery: Mutex<Option<RoutingRecovery>>,
    /// Asked on a lookup miss before `recovery`, in order; what one answers is filed under the id
    /// asked for as it is ([`Self::restore_as`]), with no hash check.
    sources: Mutex<Vec<RoutingRecovery>>,
    /// The versions people chose for songs, and the ids each song played under.
    choices: StreamChoices,
    /// How many bodies of each bound play are streaming now, by the play's live key.
    live_plays: Arc<Mutex<HashMap<String, usize>>>,
    /// The videos found to be another recording than a song, by [`Self::song_key`], newest last:
    /// at most [`Self::MAX_REFUSED_PER_SONG`] a song and [`Self::MAX_REFUSED_SONGS`] songs, the
    /// oldest song going first. Written to `refused-videos.json` beside the registry's file.
    ///
    /// Lock order: a routing's lock may be held when this is taken; this is never held while
    /// taking a routing's lock.
    refused: Mutex<IndexMap<String, Vec<String>>>,
    refused_dirty: AtomicBool,
    /// Told after a heard record is stored, outside every lock.
    heard_listeners: Mutex<Vec<HeardListener>>,
    /// The written line of every kept id, reused by the next flush unless the id changed. Only a
    /// flush touches it, under `flush_lock`.
    kept_lines: Mutex<HashMap<String, KeptLine>>,
    /// How often [`Self::run_flusher`] writes.
    flush_interval: Duration,
    /// Kept lines written from their routing rather than from `kept_lines`.
    kept_renders: AtomicUsize,
    /// Writes of the registry's file that succeeded.
    file_writes: AtomicUsize,
    #[cfg(test)]
    before_insert: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

/// One line of `refused-videos.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct PersistedRefusal {
    #[serde(deserialize_with = "state_file::null_as_default")]
    song: String,
    #[serde(deserialize_with = "state_file::null_as_default")]
    videos: Vec<String>,
}

/// A durable source of song routings that outlives the registry's bounded cache.
pub type RoutingRecovery = Arc<dyn Fn(&str) -> Option<SoulseekRouting> + Send + Sync>;

/// The entries and their recency: a stamp per entry and a map from stamp to id keep the
/// most-recently-used order without a linear removal from a list on every touch. Kept ids live
/// beside them under the same mutex, so an id is in exactly one of the two maps.
#[derive(Default)]
struct Lru {
    by_id: HashMap<String, (SharedRouting, u64)>,
    /// Stamp → id; the highest stamp is the most recently used.
    order: BTreeMap<u64, String>,
    next: u64,
    /// Ids a playlist keeps, never trimmed: each with the keys that hold it and its stamp.
    kept: HashMap<String, Kept>,
    /// The ids each key holds.
    holders: HashMap<String, HashSet<String>>,
    /// Each key's last applied revision, remembered after the key empties.
    revisions: HashMap<String, u64>,
    /// Keys read from the file that no `set_kept` has named since.
    unconfirmed: HashSet<String>,
    /// Kept ids whose written line is out of date.
    kept_changed: HashSet<String>,
}

/// A kept id's routing, the keys that hold it and its recency stamp.
struct Kept {
    routing: SharedRouting,
    keys: BTreeSet<String>,
    stamp: u64,
}

/// A kept id's line as the last flush wrote it, with the stamp and keys it was written with.
struct KeptLine {
    stamp: u64,
    keys: Vec<String>,
    text: String,
}

impl Lru {
    fn touch(&mut self, id: &str) {
        let stamp = self.next;
        self.next += 1;
        if let Some((_, old)) = self.by_id.get_mut(id) {
            self.order.remove(old);
            *old = stamp;
            self.order.insert(stamp, id.to_string());
        }
    }

    fn trim(&mut self) {
        while self.by_id.len() > ExternalIdRegistry::MAX_ENTRIES {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            self.by_id.remove(&oldest);
        }
    }

    /// The next stamp.
    fn stamp(&mut self) -> u64 {
        let stamp = self.next;
        self.next += 1;
        stamp
    }

    /// The routing under `id` in either map.
    fn find(&self, id: &str) -> Option<SharedRouting> {
        self.kept
            .get(id)
            .map(|kept| kept.routing.clone())
            .or_else(|| self.by_id.get(id).map(|(r, _)| r.clone()))
    }

    /// Every entry of both maps, most recently used first.
    fn recent_first(&self) -> Vec<(String, SharedRouting)> {
        let mut all: Vec<(u64, &String, &SharedRouting)> = self
            .by_id
            .iter()
            .map(|(id, (routing, stamp))| (*stamp, id, routing))
            .chain(self.kept.iter().map(|(id, kept)| (kept.stamp, id, &kept.routing)))
            .collect();
        all.sort_unstable_by_key(|(stamp, _, _)| std::cmp::Reverse(*stamp));
        all.into_iter()
            .map(|(_, id, routing)| (id.clone(), routing.clone()))
            .collect()
    }

    /// Every entry of both maps, in no order.
    fn all(&self) -> Vec<(String, SharedRouting)> {
        self.by_id
            .iter()
            .map(|(id, (routing, _))| (id.clone(), routing.clone()))
            .chain(
                self.kept
                    .iter()
                    .map(|(id, kept)| (id.clone(), kept.routing.clone())),
            )
            .collect()
    }

    /// Makes `key` hold `id`, moving it out of the LRU with its stamp. False when neither map
    /// has it.
    fn hold(&mut self, id: &str, key: &str) -> bool {
        if let Some(kept) = self.kept.get_mut(id) {
            kept.keys.insert(key.to_string());
            self.kept_changed.insert(id.to_string());
            return true;
        }
        let Some((routing, stamp)) = self.by_id.remove(id) else {
            return false;
        };
        self.order.remove(&stamp);
        self.kept.insert(
            id.to_string(),
            Kept {
                routing,
                keys: BTreeSet::from([key.to_string()]),
                stamp,
            },
        );
        self.kept_changed.insert(id.to_string());
        true
    }

    /// Takes `key` off `id`; an id no key holds goes back to the LRU with its own stamp. True
    /// when it went back.
    fn let_go(&mut self, id: &str, key: &str) -> bool {
        let Some(kept) = self.kept.get_mut(id) else {
            return false;
        };
        kept.keys.remove(key);
        if !kept.keys.is_empty() {
            self.kept_changed.insert(id.to_string());
            return false;
        }
        if let Some(Kept { routing, stamp, .. }) = self.kept.remove(id) {
            self.order.insert(stamp, id.to_string());
            self.by_id.insert(id.to_string(), (routing, stamp));
        }
        self.kept_changed.remove(id);
        true
    }
}

/// One line of `external-ids.json` (the private record `Persisted(string Id, SoulseekRouting Routing)`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct Persisted {
    #[serde(deserialize_with = "state_file::null_as_default")]
    id: String,
    routing: Option<SoulseekRouting>,
    /// The keys that keep the id; left out for an id only the LRU holds.
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "state_file::null_as_default"
    )]
    kept: Vec<String>,
    /// The id's recency stamp. Files written before it carry none and are read by line order.
    #[serde(skip_serializing_if = "Option::is_none")]
    recency: Option<u64>,
}

const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

impl ExternalIdRegistry {
    pub const MAX_ENTRIES: usize = 10_000;

    /// How many refused videos a song keeps.
    pub const MAX_REFUSED_PER_SONG: usize = SoulseekRouting::MAX_REFUSED_VIDEOS;

    /// How many songs keep refused videos.
    pub const MAX_REFUSED_SONGS: usize = 20_000;

    /// A search registers well over a hundred routings, so flushing per write
    /// would turn one search into a hundred file writes. Coalesce instead: the window is
    /// short next to the restarts this exists to survive.
    pub const FLUSH_INTERVAL: Duration = Duration::from_secs(10);

    /// In memory only (`new ExternalIdRegistry()`), for tests.
    pub fn in_memory() -> Self {
        Self::new(None::<PathBuf>)
    }

    /// A registry kept in the file at `path` (loaded now), or in memory only when the path is
    /// absent or blank.
    pub fn new(path: Option<impl AsRef<Path>>) -> Self {
        let path = path
            .map(|p| p.as_ref().to_path_buf())
            .filter(|p| !p.as_os_str().to_string_lossy().trim().is_empty());
        let choices = StreamChoices::open(path.as_deref());
        let registry = ExternalIdRegistry {
            path,
            lru: Mutex::new(Lru::default()),
            dirty: AtomicBool::new(false),
            flush_lock: Mutex::new(()),
            recovery: Mutex::new(None),
            sources: Mutex::new(Vec::new()),
            choices,
            live_plays: Arc::new(Mutex::new(HashMap::new())),
            refused: Mutex::new(IndexMap::new()),
            refused_dirty: AtomicBool::new(false),
            heard_listeners: Mutex::new(Vec::new()),
            kept_lines: Mutex::new(HashMap::new()),
            flush_interval: Self::FLUSH_INTERVAL,
            kept_renders: AtomicUsize::new(0),
            file_writes: AtomicUsize::new(0),
            #[cfg(test)]
            before_insert: Mutex::new(None),
        };
        if registry.path.is_some() {
            registry.load_refused();
            registry.load();
        }
        registry
    }

    /// The same registry, flushed every `every` by [`Self::run_flusher`].
    pub fn with_flush_interval(mut self, every: Duration) -> Self {
        self.flush_interval = every;
        self
    }

    pub fn register(&self, routing: impl Into<SharedRouting>) -> String {
        self.register_handle(routing).0
    }

    /// [`Self::register`], also handing back the routing the registry holds under the id now: the
    /// kept routing when the id is kept (it took what the newcomer knows), else the newcomer.
    /// Kept or LRU is decided again under the lock when the routing goes in, so a `set_kept`
    /// meanwhile never leaves two copies.
    pub fn register_handle(&self, routing: impl Into<SharedRouting>) -> (String, SharedRouting) {
        let routing = routing.into();
        let id = Self::make_short_id(&routing.lock());
        let (existing, existing_kept) = {
            let lru = self.lru.lock();
            match lru.kept.get(&id) {
                Some(kept) => (Some(kept.routing.clone()), true),
                None => (lru.by_id.get(&id).map(|(r, _)| r.clone()), false),
            }
        };
        let registered_already = existing.as_ref().is_some_and(|p| p.ptr_eq(&routing));
        let previous = existing.filter(|p| !p.ptr_eq(&routing));

        {
            let mut r = routing.lock();
            // A video a routing arrives with was named by whoever minted it, not picked by a
            // ranking.
            if !registered_already && r.has_you_tube() && r.video_pick == 0 {
                r.video_pick = SoulseekRouting::VIDEO_NAMED;
            }
            self.apply_refusals(&mut r);
        }

        let merged_into = match previous {
            Some(previous) if existing_kept => {
                let guarded = self.pin_guarded(&previous.snapshot());
                Self::merge_into_kept(&previous, &routing, guarded);
                Some(previous)
            }
            Some(previous) => {
                // A video a person chose (or one still marked chosen) stays over a fresh mint's.
                let keeps_pin = {
                    let p = previous.snapshot();
                    p.video_pick == SoulseekRouting::VIDEO_CHOSEN || self.pin_guarded(&p)
                };
                Self::merge_into_newcomer(&routing, &previous, keeps_pin);
                None
            }
            None => None,
        };
        self.store(id, routing, merged_into)
    }

    /// Files `newcomer` under `id`, deciding kept or LRU under the lock. A kept id keeps its own
    /// routing, which takes what the newcomer knows (unless `merged_into` is that routing and
    /// took it already). Otherwise the LRU holds `merged_into` when there is one, else the
    /// newcomer, at the newest end. Hands back the routing the registry holds.
    fn store(
        &self,
        id: String,
        newcomer: SharedRouting,
        merged_into: Option<SharedRouting>,
    ) -> (String, SharedRouting) {
        #[cfg(test)]
        self.pause_before_insert();
        let mut guard = self.lru.lock();
        let lru = &mut *guard;
        let stamp = lru.stamp();
        if let Some(kept) = lru.kept.get_mut(&id) {
            kept.stamp = stamp;
            let handle = kept.routing.clone();
            drop(guard);
            let took = handle.ptr_eq(&newcomer) || merged_into.as_ref().is_some_and(|m| m.ptr_eq(&handle));
            if !took {
                let guarded = self.pin_guarded(&handle.snapshot());
                Self::merge_into_kept(&handle, &newcomer, guarded);
            }
            self.mark_changed(&id);
            return (id, handle);
        }
        let stored = merged_into.unwrap_or(newcomer);
        if let Some((_, old)) = lru.by_id.insert(id.clone(), (stored.clone(), stamp)) {
            lru.order.remove(&old);
        }
        lru.order.insert(stamp, id.clone());
        lru.trim();
        drop(guard);
        self.dirty.store(true, Ordering::SeqCst);
        (id, stored)
    }

    /// What a fresh routing takes from the one it replaces in the LRU under the same id. With
    /// `keeps_pin` (a person chose the previous routing's video, or it is still marked chosen),
    /// the previous video, its mark and its stream facts win over a video the newcomer was minted
    /// with. The plays bound to their videos always carry over.
    fn merge_into_newcomer(routing: &SharedRouting, previous: &SharedRouting, keeps_pin: bool) {
        // Read the earlier routing under its own lock, then let it go before taking the
        // new one's, so two registrations never hold both.
        let (prev_album_id, (seconds, source), prev_isrc, prev_artist_id, prev_explicit, prev_album) = {
            let p = previous.lock();
            (
                p.external_album_id.clone(),
                SongLength::shown(&p),
                p.isrc.clone(),
                p.external_artist_id.clone(),
                p.explicit_content,
                p.album.clone(),
            )
        };
        let (prev_video, prev_stream, prev_refused, prev_pick, prev_contexts, prev_heard, prev_plays) = {
            let p = previous.lock();
            (
                p.you_tube_id.clone().filter(|v| !v.is_empty()),
                p.stream_facts(),
                p.refused_videos.clone(),
                p.video_pick,
                p.contexts.clone(),
                p.heard.clone(),
                p.plays.clone(),
            )
        };
        let mut r = routing.lock();
        // A song row mints an album routing with no Deezer id (`convert_song_to_json`), and
        // it hashes to the same id as the one an album search already resolved. Registering
        // it must not erase that id. Best-effort only: this is an optimization, and losing
        // the race just sends `get_album` down its cached name-lookup fallback. A song never
        // carries the catalog's album id: the albums it was listed from are its contexts.
        if r.kind != RoutingKind::Song && r.external_album_id.is_none() && prev_album_id.is_some() {
            r.external_album_id = prev_album_id;
        }
        // The albums people listed the song from and what each person heard: a fresh mint
        // carries none, and a copy read from elsewhere may carry older ones, so each person
        // keeps the later of the two.
        if !prev_contexts.is_empty() || !prev_heard.is_empty() {
            r.merge_heard(&prev_contexts, &prev_heard);
        }
        // Every search and every station playlist mints its songs again, and each mint is a
        // fresh routing. Without this, the length a lookup found for a song lasted until the
        // next search for it, which is why a row that had a length could lose it again.
        if source > r.shown_duration_source {
            r.shown_duration = seconds;
            r.shown_duration_source = source;
        }
        // The same for the ISRC an album listing found: a search row for the same song
        // names none, and must not forget it.
        if r.isrc.is_none() {
            r.isrc = prev_isrc;
        }
        // And the catalog artist an artist search or page settled on: every album row
        // mints its artist again by name alone, and must not undo that choice.
        if r.external_artist_id.is_none() {
            r.external_artist_id = prev_artist_id;
        }
        // And what the catalog said of its words: a search row mints the song again before
        // the catalog is asked.
        if r.explicit_content.is_none() {
            r.explicit_content = prev_explicit;
        }
        // And the album, which a song's id leaves out: radio registers a song Last.fm named
        // without one, and it must not undo the album a search or a listing filed it under
        // (otherwise later answers lose the album until a search registers the song again).
        if r.kind == RoutingKind::Song
            && r.album.as_deref().is_none_or(|album| album.trim().is_empty())
            && prev_album
                .as_deref()
                .is_some_and(|album| !album.trim().is_empty())
        {
            r.album = prev_album;
        }
        // And the videos found to be another song: a fresh mint must not take one again.
        if !prev_refused.is_empty() {
            r.keep_refused_videos(&prev_refused);
        }
        // And the video a play or a lookup pinned, with what is known of its stream: a
        // search mints the song again with no video, and the size a client was told has to
        // stay the size of the video that plays.
        if let Some(prev_video) = prev_video {
            match r.you_tube_id.as_deref().filter(|v| !v.is_empty()) {
                None => {
                    if r.pin_video(&prev_video) {
                        r.video_pick = prev_pick;
                    }
                    r.merge_stream_facts(&prev_stream);
                }
                Some(video) if video == prev_video => {
                    r.video_pick = prev_pick;
                    if r.stream_facts().is_empty() {
                        r.merge_stream_facts(&prev_stream);
                    }
                }
                Some(_) if keeps_pin => {
                    if r.pin_video(&prev_video) {
                        r.video_pick = prev_pick;
                        r.merge_stream_facts(&prev_stream);
                    }
                }
                Some(_) => {}
            }
        }
        // And the plays running on the song, each on the video it started on: a re-registration
        // must never move one to another video.
        if !prev_plays.is_empty() {
            let own = std::mem::take(&mut r.plays);
            let mut plays = prev_plays;
            for play in own {
                plays.retain(|known| {
                    !(known.key == play.key && known.token == play.token && known.address == play.address)
                });
                plays.push(play);
            }
            let excess = plays.len().saturating_sub(SoulseekRouting::MAX_PLAYS);
            plays.drain(..excess);
            r.plays = plays;
        }
    }

    /// What a kept routing takes from a newcomer under the same id: the stronger shown length,
    /// blank catalog facts, the library-file mark, refusals, album contexts and heard records,
    /// and the newcomer's video (with its stream facts and pick) only when the kept routing has
    /// none and does not refuse it. The kept routing's own video and stream facts stay; for the
    /// same video, facts it lacks are filled. Holds one routing's lock at a time.
    /// With `guarded` (a person's choice, or an earlier one, holds the kept routing's video) no
    /// video is pinned onto it.
    fn merge_into_kept(kept: &SharedRouting, newcomer: &SharedRouting, guarded: bool) {
        let n = newcomer.snapshot();
        let mut k = kept.lock();
        let (seconds, source) = SongLength::shown(&n);
        if source > k.shown_duration_source {
            k.shown_duration = seconds;
            k.shown_duration_source = source;
        }
        if k.album.as_deref().is_none_or(|album| album.trim().is_empty())
            && n.album.as_deref().is_some_and(|album| !album.trim().is_empty())
        {
            k.album = n.album.clone();
        }
        if k.isrc.is_none() {
            k.isrc = n.isrc.clone();
        }
        if k.external_artist_id.is_none() {
            k.external_artist_id = n.external_artist_id.clone();
        }
        if k.explicit_content.is_none() {
            k.explicit_content = n.explicit_content;
        }
        if k.track.is_none() {
            k.track = n.track;
        }
        if k.disc_number.is_none() {
            k.disc_number = n.disc_number;
        }
        if k.total_tracks.is_none() {
            k.total_tracks = n.total_tracks;
        }
        if k.kind != RoutingKind::Song && k.external_album_id.is_none() {
            k.external_album_id = n.external_album_id.clone();
        }
        k.replaces_library_file |= n.replaces_library_file;
        for video in &n.refused_videos {
            k.refuse_video(video);
        }
        if !n.contexts.is_empty() || !n.heard.is_empty() {
            k.merge_heard(&n.contexts, &n.heard);
        }
        if let Some(video) = n.you_tube_id.as_deref().filter(|v| !v.is_empty()) {
            match k.you_tube_id.as_deref().filter(|v| !v.is_empty()) {
                None if !guarded && !k.refuses_video(video) => {
                    if k.pin_video(video) {
                        k.video_pick = n.video_pick;
                    }
                    k.merge_stream_facts(&n.stream_facts());
                }
                Some(own) if own == video && k.stream_facts().is_empty() => {
                    k.merge_stream_facts(&n.stream_facts());
                }
                _ => {}
            }
        }
    }

    pub fn lookup(&self, short_id: &str) -> Option<SharedRouting> {
        if let Some(routing) = self.get(short_id) {
            return Some(routing);
        }
        // Never call a source with the registry locked. Registering and restoring read only
        // the registry's own maps, so recovery cannot recurse, and unknown Navidrome ids still
        // pass through unchanged.
        let sources = self.sources.lock().clone();
        for source in sources {
            if let Some(routing) = source(short_id) {
                return Some(self.restore_as(short_id, routing));
            }
        }
        let recover = self.recovery.lock().clone()?;
        let routing = recover(short_id)?;
        if Self::make_short_id(&routing) != short_id {
            return None;
        }
        Some(self.register_handle(routing).1)
    }

    /// The routing under `id` in either map, marked used: a kept id's stamp moves as an LRU
    /// entry's does, and its line is written again with the new stamp.
    fn get(&self, id: &str) -> Option<SharedRouting> {
        let mut guard = self.lru.lock();
        let lru = &mut *guard;
        let stamp = lru.next;
        if let Some(kept) = lru.kept.get_mut(id) {
            kept.stamp = stamp;
            let routing = kept.routing.clone();
            lru.next += 1;
            lru.kept_changed.insert(id.to_string());
            drop(guard);
            self.dirty.store(true, Ordering::SeqCst);
            return Some(routing);
        }
        let routing = lru.by_id.get(id).map(|(r, _)| r.clone())?;
        lru.touch(id);
        Some(routing)
    }

    /// The routing under `id` in either map, not marked used and with no recovery source asked:
    /// for background reads (seed refreshes, start sweeps, scans), which must not make thousands
    /// of ids recent.
    pub fn peek(&self, id: &str) -> Option<SharedRouting> {
        self.lru.lock().find(id)
    }

    /// Whether a playlist keeps `id`.
    pub fn is_kept(&self, id: &str) -> bool {
        self.lru.lock().kept.contains_key(id)
    }

    pub fn set_recovery(&self, recover: RoutingRecovery) {
        *self.recovery.lock() = Some(recover);
    }

    /// Sources a lookup miss asks, in order, before the hook of [`Self::set_recovery`]. What one
    /// answers is filed under the id asked for with [`Self::restore_as`]: no hash check, since a
    /// seed whose video was pinned after its id was minted no longer hashes to it.
    pub fn set_recovery_sources(&self, sources: Vec<RoutingRecovery>) {
        *self.sources.lock() = sources;
    }

    /// Files `routing` under `id` exactly as given: no hash check and no re-keying. Every video
    /// the song refuses (the registry's and the routing's own) is applied with the routing's own
    /// `refuse_video`, so a refused pin goes. Kept or LRU is decided under the lock, at the newest
    /// end; when the registry already holds `id`, that routing takes what `routing` knows and is
    /// the one handed back.
    pub fn restore_as(&self, id: &str, routing: SoulseekRouting) -> SharedRouting {
        let mut routing = routing;
        let refused: Vec<String> = self
            .refusals_for(&routing)
            .into_iter()
            .chain(routing.refused_videos.clone())
            .collect();
        for video in &refused {
            routing.refuse_video(video);
        }
        // A seed is a copy from any time: the plays it names are long over, a chosen mark no
        // record backs is Octo's pick, and the chooser heard nothing older than their choice.
        self.settle_restored(&mut routing);
        let newcomer = SharedRouting::new(routing);
        let existing = self.peek(id);
        if let Some(existing) = &existing {
            let guarded = self.pin_guarded(&existing.snapshot());
            Self::merge_into_kept(existing, &newcomer, guarded);
        }
        let (_, stored) = self.store(id.to_string(), newcomer, existing);
        self.dirty.store(true, Ordering::SeqCst);
        stored
    }

    /// Makes `key` hold exactly the ids of `ids` as of `revision`. An id any key holds is kept:
    /// never trimmed, written with the keys that hold it. An id no key holds any more goes back
    /// to the LRU with its own recency stamp, and the LRU is trimmed. A call older than the key's
    /// last revision is ignored; the last revision is remembered after the key empties. An id the
    /// registry has lost is restored from its seed ([`Self::restore_as`]); one held without a
    /// video whose seed names one is pinned to it unless the song refuses it. Called outside
    /// every store mutex and routing lock.
    pub fn set_kept(&self, key: &str, revision: u64, ids: Vec<(String, SoulseekRouting)>) {
        if self
            .lru
            .lock()
            .revisions
            .get(key)
            .is_some_and(|&last| revision < last)
        {
            return;
        }
        let mut restored: HashMap<String, SharedRouting> = HashMap::new();
        for (id, seed) in &ids {
            if id.is_empty() {
                continue;
            }
            match self.peek(id) {
                None => {
                    restored.insert(id.clone(), self.restore_as(id, seed.clone()));
                }
                Some(routing) => {
                    let unpinned = !routing.lock().has_you_tube();
                    if unpinned
                        && let Some(video) = seed.you_tube_id.as_deref().filter(|v| !v.is_empty())
                        && self.pin_video(id, video, false)
                        && seed.video_pick > 0
                    {
                        self.remember_pick(id, video, seed.video_pick);
                    }
                }
            }
        }
        let wanted: HashSet<String> = ids
            .into_iter()
            .map(|(id, _)| id)
            .filter(|id| !id.is_empty())
            .collect();
        {
            let mut guard = self.lru.lock();
            let lru = &mut *guard;
            if lru.revisions.get(key).is_some_and(|&last| revision < last) {
                return;
            }
            lru.revisions.insert(key.to_string(), revision);
            lru.unconfirmed.remove(key);
            let before = lru.holders.remove(key).unwrap_or_default();
            for id in wanted.difference(&before) {
                if !lru.hold(id, key)
                    && let Some(routing) = restored.get(id)
                {
                    // Trimmed again since it was restored: filed straight into the kept map.
                    let stamp = lru.stamp();
                    lru.kept.insert(
                        id.clone(),
                        Kept {
                            routing: routing.clone(),
                            keys: BTreeSet::from([key.to_string()]),
                            stamp,
                        },
                    );
                    lru.kept_changed.insert(id.clone());
                }
            }
            let mut released = false;
            for id in before.difference(&wanted) {
                released |= lru.let_go(id, key);
            }
            if !wanted.is_empty() {
                lru.holders.insert(key.to_string(), wanted);
            }
            if released {
                lru.trim();
            }
        }
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// Ends the start: every key read from the file that no `set_kept` named since lets its ids
    /// go (back to the LRU with their stamps), so keys a crash left behind keep nothing.
    pub fn finish_start(&self) {
        let mut guard = self.lru.lock();
        let lru = &mut *guard;
        let keys: Vec<String> = lru.unconfirmed.drain().collect();
        if keys.is_empty() {
            return;
        }
        for key in keys {
            for id in lru.holders.remove(&key).unwrap_or_default() {
                lru.let_go(&id, &key);
            }
        }
        lru.trim();
        drop(guard);
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// Records that the routing under `id` changed in place, so the next flush writes it.
    /// Never called while holding a routing's lock.
    pub fn mark_changed(&self, id: &str) {
        {
            let mut lru = self.lru.lock();
            if lru.kept.contains_key(id) {
                lru.kept_changed.insert(id.to_string());
            }
        }
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// [`Self::mark_changed`] for a routing known by its handle.
    pub fn mark_changed_routing(&self, routing: &SharedRouting) {
        {
            let mut guard = self.lru.lock();
            let lru = &mut *guard;
            let found = lru
                .kept
                .iter()
                .find(|(_, kept)| kept.routing.ptr_eq(routing))
                .map(|(id, _)| id.clone());
            if let Some(id) = found {
                lru.kept_changed.insert(id);
            }
        }
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// [`Self::mark_changed`] for several ids at once.
    fn mark_changed_ids(&self, ids: &[String]) {
        {
            let mut guard = self.lru.lock();
            let lru = &mut *guard;
            for id in ids {
                if lru.kept.contains_key(id) {
                    lru.kept_changed.insert(id.clone());
                }
            }
        }
        self.dirty.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn pause_before_insert(&self) {
        let hook = self.before_insert.lock().clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Runs `hook` in every registration and restore right before it files its routing.
    #[cfg(test)]
    pub(crate) fn pause_inserts_with(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.before_insert.lock() = Some(hook);
    }

    /// The routing a song id stands for: the one registered under `id`, else the one registered
    /// under the id its decoded routing (`yt|...`) hashes to. A decoded routing found under
    /// neither is registered only with `register` (a stream or a heart); otherwise it is handed
    /// back unsaved, with the song's refused videos, so ids a client sends never fill the
    /// registry.
    pub fn resolve_song(&self, id: &str, register: bool) -> Option<SharedRouting> {
        self.resolve(id, register).map(|found| found.routing)
    }

    fn resolve(&self, id: &str, register: bool) -> Option<Resolved> {
        if id.trim().is_empty() {
            return None;
        }
        if let Some(routing) = self.lookup(id) {
            return Some(Resolved {
                id: id.to_string(),
                routing,
                registered: true,
            });
        }
        let mut decoded = SoulseekMetadataService::try_decode_external_id(Some(id))?;
        let short = Self::make_short_id(&decoded);
        if let Some(routing) = self.lookup(&short) {
            return Some(Resolved {
                id: short,
                routing,
                registered: true,
            });
        }
        if register {
            let (id, routing) = self.register_handle(decoded);
            return Some(Resolved {
                id,
                routing,
                registered: true,
            });
        }
        self.apply_refusals(&mut decoded);
        Some(Resolved {
            id: short,
            routing: SharedRouting::new(decoded),
            registered: false,
        })
    }

    /// Keeps `record` as what `user` heard of the song `id` names, resolved now (registering a
    /// decoded id), so a write that outlived the routing it started from lands on the one
    /// registered. With `played`, the user's context it names is marked played. One record per
    /// user: a later one replaces, an earlier one never does. Every [`Self::on_heard`] listener is
    /// told of a stored record. True when anything changed.
    pub fn remember_heard(
        &self,
        id: &str,
        user: &str,
        record: HeardRecord,
        played: Option<PlayedMark>,
    ) -> bool {
        let Some(found) = self.resolve(id, true) else {
            return false;
        };
        let (stored, marked) = {
            let mut r = found.routing.lock();
            if r.kind != RoutingKind::Song {
                return false;
            }
            let stored = r.remember_heard(user, record.clone());
            let marked = played.is_some_and(|mark| r.mark_played(user, &mark, record.heard_at));
            (stored, marked)
        };
        if !stored && !marked {
            return false;
        }
        self.mark_changed(&found.id);
        if stored {
            let listeners = self.heard_listeners.lock().clone();
            for listener in listeners {
                listener(&found.id, user.trim(), &record);
            }
        }
        true
    }

    /// What `user` heard of the song `id` names, when it is kept.
    pub fn heard_by(&self, id: &str, user: &str) -> Option<HeardRecord> {
        let found = self.resolve(id, false)?;
        let r = found.routing.lock();
        r.heard_by(user).cloned()
    }

    /// Adds or moves a person's album context on the registered song `id`: one per person and
    /// catalog album, at most [`SoulseekRouting::MAX_CONTEXTS`]. True when anything changed.
    pub fn remember_context(&self, id: &str, context: AlbumContext) -> bool {
        let Some(found) = self.resolve(id, false).filter(|found| found.registered) else {
            return false;
        };
        let changed = {
            let mut r = found.routing.lock();
            r.kind == RoutingKind::Song && r.remember_context(context)
        };
        if changed {
            self.mark_changed(&found.id);
        }
        changed
    }

    /// The albums people listed or played the song `id` names from, the least recently listed
    /// first.
    pub fn contexts(&self, id: &str) -> Vec<AlbumContext> {
        self.resolve(id, false)
            .map(|found| found.routing.lock().contexts.clone())
            .unwrap_or_default()
    }

    /// Adds a listener told after [`Self::remember_heard`] stores a record, outside the
    /// registry's locks, with the registered routing's short id, the user and the record.
    pub fn on_heard(&self, listener: HeardListener) {
        self.heard_listeners.lock().push(listener);
    }

    /// How many ids the registry holds, kept ones included.
    pub fn count(&self) -> usize {
        let lru = self.lru.lock();
        lru.by_id.len() + lru.kept.len()
    }

    /// The songs a song row filed under the album `album` by `artist`, newest first, one per
    /// recording. A row names its album after the song's album, or after its own title when it
    /// has none (SubsonicResponseBuilder.ConvertSongFields), so this matches on the same rule,
    /// plus the title alone, for a song whose album was filled in after its row named one.
    /// For getAlbum when the catalog cannot list the album (#59). `limit` was 50 by default.
    pub fn songs_filed_under(
        &self,
        artist: Option<&str>,
        album: Option<&str>,
        limit: usize,
    ) -> Vec<(String, SharedRouting)> {
        let (Some(artist), Some(album)) = (artist.filter(|a| !a.is_empty()), album.filter(|a| !a.is_empty()))
        else {
            return Vec::new();
        };
        let order: Vec<(String, SharedRouting)> = self.lru.lock().recent_first();

        let mut found = Vec::new();
        let mut recordings = HashSet::new();
        for (id, shared) in order {
            let recording = {
                let routing = shared.lock();
                if routing.kind != RoutingKind::Song {
                    continue;
                }
                if routing.artist.as_deref() != Some(artist) {
                    continue;
                }
                let filed_under = if routing.album.as_deref().is_none_or(|a| a.trim().is_empty()) {
                    routing.title.as_deref()
                } else {
                    routing.album.as_deref()
                };
                if filed_under != Some(album) && routing.title.as_deref() != Some(album) {
                    continue;
                }
                SongIdentity::key(routing.title.as_deref().unwrap_or(""))
            };
            let recording = if recording.is_empty() {
                id.clone()
            } else {
                recording
            };
            if !recordings.insert(recording) {
                continue;
            }
            found.push((id, shared));
            if found.len() >= limit {
                break;
            }
        }
        found
    }

    /// Store a length for a song under `short_id`, by the rules in [`SongLength`]. Looked up by
    /// id at write time rather than handed a routing, because a background lookup can outlive
    /// the routing it started from: a search that ran meanwhile replaced it, and a write to the
    /// old object would be lost.
    pub fn remember_length(&self, short_id: &str, seconds: Option<i32>, source: LengthSource) -> bool {
        if short_id.is_empty() {
            return false;
        }
        let Some(routing) = self.peek(short_id) else {
            return false;
        };
        {
            let mut r = routing.lock();
            if r.kind != RoutingKind::Song {
                return false;
            }
            if !SongLength::remember(&mut r, seconds, source) {
                return false;
            }
        }
        self.mark_changed(short_id);
        true
    }

    /// Pins `video` for playback on the song under `short_id`, written down at the next flush.
    /// `replace` false keeps a different video that is already pinned. Pinning another video
    /// forgets the old one's stream facts. True when the song now plays `video`. False for a
    /// video the song refuses.
    pub fn pin_video(&self, short_id: &str, video: &str, replace: bool) -> bool {
        if short_id.is_empty() || video.is_empty() {
            return false;
        }
        let Some(routing) = self.peek(short_id) else {
            return false;
        };
        // A song whose video a person chose keeps it: only a play's start changes it.
        if self.pin_guarded(&routing.snapshot()) {
            return false;
        }
        let changed = {
            let mut r = routing.lock();
            if r.refuses_video(video) {
                return false;
            }
            if !replace
                && r.you_tube_id
                    .as_deref()
                    .is_some_and(|v| !v.is_empty() && v != video)
            {
                return false;
            }
            r.pin_video(video)
        };
        if changed {
            self.mark_changed(short_id);
        }
        true
    }

    /// Records that the pinned `video` on the song under `short_id` was chosen by ranking
    /// `pick`, but only while the song still plays `video`. True when the record changed.
    pub fn remember_pick(&self, short_id: &str, video: &str, pick: u8) -> bool {
        if short_id.is_empty() || video.is_empty() {
            return false;
        }
        let Some(routing) = self.peek(short_id) else {
            return false;
        };
        // The chosen mark is written only by a play's start; a chosen song's mark stays.
        if pick == SoulseekRouting::VIDEO_CHOSEN || self.pin_guarded(&routing.snapshot()) {
            return false;
        }
        let changed = {
            let mut r = routing.lock();
            if r.you_tube_id.as_deref() != Some(video) || r.video_pick == pick {
                false
            } else {
                r.video_pick = pick;
                true
            }
        };
        if changed {
            self.mark_changed(short_id);
        }
        changed
    }

    /// The ids of the songs whose video an older ranking picked, most recently used first: song
    /// routings with a video, an artist and a title whose `video_pick` is below
    /// [`SoulseekRouting::VIDEO_PICK`]. A video the song was named by is never listed.
    pub fn stale_pins(&self) -> Vec<String> {
        let entries: Vec<(String, SharedRouting)> = self.lru.lock().recent_first();
        entries
            .into_iter()
            .filter(|(_, routing)| {
                let r = routing.lock();
                r.kind == RoutingKind::Song
                    && r.has_you_tube()
                    && r.has_artist_title()
                    && r.video_pick < SoulseekRouting::VIDEO_PICK
            })
            .map(|(id, _)| id)
            .collect()
    }

    /// Up to `limit` videos pinned to songs, the most recently used song first, leaving out
    /// `except`.
    pub fn pinned_videos(&self, limit: usize, except: &std::collections::HashSet<String>) -> Vec<String> {
        let routings: Vec<SharedRouting> = self
            .lru
            .lock()
            .recent_first()
            .into_iter()
            .map(|(_, routing)| routing)
            .collect();
        let mut videos: Vec<String> = Vec::new();
        for routing in routings {
            if videos.len() >= limit {
                break;
            }
            let r = routing.lock();
            if r.kind != RoutingKind::Song {
                continue;
            }
            if let Some(video) = r.you_tube_id.as_deref().filter(|v| !v.is_empty())
                && !except.contains(video)
                && !videos.iter().any(|known| known == video)
            {
                videos.push(video.to_string());
            }
        }
        videos
    }

    /// Marks the video the song under `short_id` plays as picked by the current ranking, but
    /// only while it still plays `video`. True when the record changed.
    pub fn stamp_pick(&self, short_id: &str, video: &str) -> bool {
        self.remember_pick(short_id, video, SoulseekRouting::VIDEO_PICK)
    }

    /// Sets `better` as the video the song under `short_id` switches to at a play's first byte,
    /// but only while the song still plays `current` and does not refuse `better`. True when it
    /// was set.
    pub fn set_pending_video(&self, short_id: &str, current: &str, better: &str) -> bool {
        if short_id.is_empty() || current.is_empty() || better.is_empty() || current == better {
            return false;
        }
        let Some(routing) = self.peek(short_id) else {
            return false;
        };
        if self.pin_guarded(&routing.snapshot()) {
            return false;
        }
        let mut r = routing.lock();
        if r.you_tube_id.as_deref() != Some(current) || r.refuses_video(better) {
            return false;
        }
        r.pending_video = Some(better.to_string());
        true
    }

    /// Switches `routing` to the video waiting in its `pending_video`, taking the old video's
    /// stream facts and a length learned from it with it, and marks the current ranking as its
    /// picker. Returns the video it left when no other routing pins that video any more, so its
    /// copy can go. None when nothing was waiting (or the waiting video is refused or already
    /// playing), or when another routing still pins the old video.
    pub fn apply_pending_video(&self, routing: &SharedRouting) -> Option<String> {
        // A ranking's video never replaces one a person chose: it is dropped.
        if self.pin_guarded(&routing.snapshot()) {
            routing.lock().pending_video = None;
            return None;
        }
        let old = {
            let mut r = routing.lock();
            let better = r.pending_video.take()?;
            let old = r.you_tube_id.clone();
            if r.refuses_video(&better) || old.as_deref() == Some(better.as_str()) {
                return None;
            }
            if let Some(old) = &old {
                r.unpin_video(old);
            }
            r.pin_video(&better);
            r.video_pick = SoulseekRouting::VIDEO_PICK;
            old
        };
        self.mark_changed_routing(routing);
        let old = old?;
        let all: Vec<SharedRouting> = self
            .lru
            .lock()
            .all()
            .into_iter()
            .map(|(_, routing)| routing)
            .collect();
        let pinned_elsewhere = all
            .into_iter()
            .filter(|other| !other.ptr_eq(routing))
            .any(|other| other.lock().you_tube_id.as_deref() == Some(old.as_str()));
        (!pinned_elsewhere).then_some(old)
    }

    /// The key a song's refused videos are filed under: its artist and title with case, accents
    /// and punctuation set aside. None when either is empty, so a song with no name records
    /// nothing.
    pub fn song_key(artist: &str, title: &str) -> Option<String> {
        let artist = SongIdentity::loose_key(artist);
        let title = SongIdentity::loose_key(title);
        if artist.is_empty() || title.is_empty() {
            return None;
        }
        Some(format!("{artist}\n{title}"))
    }

    fn key_of(routing: &SoulseekRouting) -> Option<String> {
        Self::song_key(
            routing.artist.as_deref().unwrap_or(""),
            routing.title.as_deref().unwrap_or(""),
        )
    }

    /// The videos refused for the song `routing` names, oldest first.
    pub fn refusals_for(&self, routing: &SoulseekRouting) -> Vec<String> {
        if routing.kind != RoutingKind::Song {
            return Vec::new();
        }
        Self::key_of(routing)
            .and_then(|key| self.refused.lock().get(&key).cloned())
            .unwrap_or_default()
    }

    /// Adds the videos refused for the song `routing` names to its own refused videos.
    pub fn apply_refusals(&self, routing: &mut SoulseekRouting) {
        let refused = self.refusals_for(routing);
        if !refused.is_empty() {
            routing.keep_refused_videos(&refused);
        }
    }

    /// Records `video` as another recording than `artist - title`, for every routing of the song
    /// now and later. True when it was not recorded yet.
    pub fn refuse_song_video(&self, artist: &str, title: &str, video: &str) -> bool {
        let Some(key) = Self::song_key(artist, title) else {
            return false;
        };
        if video.is_empty() {
            return false;
        }
        let mut refused = self.refused.lock();
        let videos = refused.entry(key).or_default();
        if videos.iter().any(|v| v == video) {
            return false;
        }
        videos.push(video.to_string());
        if videos.len() > Self::MAX_REFUSED_PER_SONG {
            videos.remove(0);
        }
        if refused.len() > Self::MAX_REFUSED_SONGS {
            refused.shift_remove_index(0);
        }
        drop(refused);
        self.refused_dirty.store(true, Ordering::SeqCst);
        self.forget_refused_choice(artist, title, video);
        true
    }

    /// Refuses `video` for the song `short_id` names (a registered id or a decoded one):
    /// unpinned with what was learned from it, and never pinned for it again, by that id or by
    /// any other routing of the same song. What each person heard stays. True when anything
    /// changed.
    pub fn refuse_video(&self, short_id: &str, video: &str) -> bool {
        if short_id.is_empty() || video.is_empty() {
            return false;
        }
        let Some(found) = self.resolve(short_id, false) else {
            return false;
        };
        let routing = found.routing;
        let (refused_here, artist, title, is_song) = {
            let mut r = routing.lock();
            (
                r.refuse_video(video) && found.registered,
                r.artist.clone().unwrap_or_default(),
                r.title.clone().unwrap_or_default(),
                r.kind == RoutingKind::Song,
            )
        };
        let mut changed = refused_here;
        let mut touched: Vec<String> = Vec::new();
        if refused_here {
            touched.push(found.id.clone());
        }
        if is_song && let Some(key) = Self::song_key(&artist, &title) {
            changed |= self.refuse_song_video(&artist, &title, video);
            // Collect the routings with the cache locked, then lock each one after letting the
            // cache go.
            let all = self.lru.lock().all();
            for (id, other) in all.into_iter().filter(|(_, o)| !o.ptr_eq(&routing)) {
                let refused = {
                    let mut o = other.lock();
                    o.kind == RoutingKind::Song
                        && Self::key_of(&o).as_deref() == Some(key.as_str())
                        && o.refuse_video(video)
                };
                if refused {
                    changed = true;
                    touched.push(id);
                }
            }
        }
        if changed {
            self.mark_changed_ids(&touched);
        }
        changed
    }

    /// Pins `video` on the song under `short_id`, replacing any other pin.
    pub fn set_you_tube_id(&self, short_id: &str, video: &str) -> bool {
        self.pin_video(short_id, video, true)
    }

    /// Stores what an answer said of `video`'s stream, but only while it is still the video the
    /// song under `short_id` plays: a re-pin meanwhile makes the facts someone else's.
    pub fn remember_stream(&self, short_id: &str, video: &str, facts: &StreamFacts) -> bool {
        if short_id.is_empty() || video.is_empty() || facts.is_empty() {
            return false;
        }
        let Some(routing) = self.peek(short_id) else {
            return false;
        };
        {
            let mut r = routing.lock();
            if r.kind != RoutingKind::Song || r.you_tube_id.as_deref() != Some(video) {
                return false;
            }
            let before = r.stream_facts();
            r.merge_stream_facts(facts);
            if r.stream_facts() == before {
                return false;
            }
        }
        self.dirty.store(true, Ordering::SeqCst);
        true
    }

    /// What is known of the stream the song under `short_id` plays, when anything is.
    pub fn stream_facts(&self, short_id: &str) -> Option<StreamFacts> {
        if short_id.is_empty() {
            return None;
        }
        let routing = self.peek(short_id)?;
        let r = routing.lock();
        if r.kind != RoutingKind::Song || !r.has_you_tube() {
            return None;
        }
        Some(r.stream_facts()).filter(|facts| !facts.is_empty())
    }

    /// Derive 22 base62 chars from sha256 of routing fields. Same input -> same id.
    /// The Kind prefix is critical: a song "Drake - Hotline Bling" must hash to a
    /// different id than the album "Hotline Bling" or the artist "Drake", or
    /// getCoverArt would return the wrong scope's artwork.
    pub fn make_short_id(r: &SoulseekRouting) -> String {
        /// `ExplicitStatus.CleanWord`: the version a clean edit carries.
        const CLEAN_WORD: &str = "clean";
        let text = |s: &Option<String>| s.clone().unwrap_or_default();
        let seed = match r.kind {
            RoutingKind::Album => format!("k:album|a:{}|al:{}", text(&r.artist), text(&r.album)),
            RoutingKind::Artist => format!("k:artist|a:{}", text(&r.artist)),
            RoutingKind::Song => format!(
                "k:song|yt:{}|a:{}|t:{}|d:{}",
                text(&r.you_tube_id),
                text(&r.artist),
                text(&r.title),
                r.duration.map(|d| d.to_string()).unwrap_or_default()
            ),
        };
        // A clean edit shares its artist, title and often its length with the explicit original,
        // so it gets a mark of its own. Only a clean one: the explicit and the unknown keep the
        // seed they always had, and with it every id already handed out.
        let seed = if r.kind != RoutingKind::Artist && r.version.as_deref() == Some(CLEAN_WORD) {
            format!("{seed}|v:{CLEAN_WORD}")
        } else {
            seed
        };
        let hash = Sha256::digest(seed.as_bytes());
        Self::to_base62(&hash, 22)
    }

    /// Treat the first 16 bytes as a big integer and base62-encode it, least significant digit
    /// first. We don't need strict cryptographic uniqueness: just collision resistance within
    /// ~10k items.
    fn to_base62(bytes: &[u8], length: usize) -> String {
        let mut head = [0u8; 16];
        head.copy_from_slice(&bytes[..16]);
        let mut value = u128::from_be_bytes(head);
        let mut out = String::with_capacity(length);
        while out.len() < length {
            let rem = (value % 62) as usize;
            value /= 62;
            out.push(ALPHABET[rem] as char);
            if value == 0 {
                break;
            }
        }
        while out.len() < length {
            out.push('0');
        }
        out.truncate(length);
        out
    }

    fn load(&self) {
        let Some(path) = &self.path else { return };
        let result = (|| -> anyhow::Result<()> {
            let Some(text) = state_file::read_text(path)? else {
                return Ok(());
            };
            let entries: Option<Vec<Persisted>> = serde_json::from_str(&text)?;
            let Some(entries) = entries else { return Ok(()) };
            let entries: Vec<Persisted> = entries
                .into_iter()
                .filter(|e| e.routing.is_some() && !e.id.is_empty())
                .collect();

            // A file written with stamps keeps every id's recency, kept and listed alike. One
            // written before them is stored most recently used first, so replaying in line order
            // rebuilds the same eviction order rather than an arbitrary one.
            let mut seen = HashSet::new();
            let stamped = entries
                .iter()
                .all(|e| e.recency.is_some_and(|stamp| seen.insert(stamp)));
            let count = entries.len() as u64;
            let mut guard = self.lru.lock();
            let lru = &mut *guard;
            for (at, e) in entries.into_iter().enumerate() {
                let Some(mut routing) = e.routing else { continue };
                self.apply_refusals(&mut routing);
                let routing = SharedRouting::new(routing);
                let line_stamp = if stamped {
                    e.recency.unwrap_or_default()
                } else {
                    count - at as u64
                };
                // A duplicate id: the later routing wins, as `_byId[e.Id] = ...` did, at the
                // earlier (more recent) place, keeping every key either line named.
                let earlier = match lru.kept.remove(&e.id) {
                    Some(kept) => Some((kept.stamp, kept.keys)),
                    None => lru.by_id.remove(&e.id).map(|(_, stamp)| {
                        lru.order.remove(&stamp);
                        (stamp, BTreeSet::new())
                    }),
                };
                let (stamp, mut keys) = earlier.unwrap_or((line_stamp, BTreeSet::new()));
                keys.extend(e.kept.into_iter().filter(|key| !key.is_empty()));
                if keys.is_empty() {
                    lru.order.insert(stamp, e.id.clone());
                    lru.by_id.insert(e.id, (routing, stamp));
                } else {
                    for key in &keys {
                        lru.holders.entry(key.clone()).or_default().insert(e.id.clone());
                        // Unconfirmed until a store names the key again (`finish_start`).
                        lru.unconfirmed.insert(key.clone());
                    }
                    lru.kept.insert(e.id, Kept { routing, keys, stamp });
                }
            }
            let highest = lru
                .order
                .keys()
                .next_back()
                .copied()
                .into_iter()
                .chain(lru.kept.values().map(|kept| kept.stamp))
                .max();
            lru.next = highest.map_or(0, |stamp| stamp + 1);
            lru.trim();
            let (listed, kept) = (lru.by_id.len(), lru.kept.len());
            drop(guard);
            info!("external id registry restored {listed} entries and {kept} kept ones");
            Ok(())
        })();
        if let Err(e) = result {
            // A registry that will not load is a cold start, not a failure to boot.
            warn!("external id registry could not be read: {e}");
        }
    }

    /// The file the refused videos are kept in, beside the registry's own.
    fn refused_path(&self) -> Option<PathBuf> {
        self.path
            .as_ref()
            .map(|path| path.with_file_name("refused-videos.json"))
    }

    fn load_refused(&self) {
        let Some(path) = self.refused_path() else { return };
        let result = (|| -> anyhow::Result<()> {
            let Some(text) = state_file::read_text(&path)? else {
                return Ok(());
            };
            let entries: Option<Vec<PersistedRefusal>> = serde_json::from_str(&text)?;
            let mut refused = self.refused.lock();
            for e in entries.unwrap_or_default() {
                if e.song.is_empty() || e.videos.is_empty() {
                    continue;
                }
                let mut videos = e.videos;
                let excess = videos.len().saturating_sub(Self::MAX_REFUSED_PER_SONG);
                videos.drain(..excess);
                refused.insert(e.song, videos);
            }
            while refused.len() > Self::MAX_REFUSED_SONGS {
                refused.shift_remove_index(0);
            }
            Ok(())
        })();
        if let Err(e) = result {
            warn!("refused videos could not be read: {e}");
        }
    }

    fn flush_refused(&self) {
        let Some(path) = self.refused_path() else { return };
        if !self.refused_dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let entries: Vec<PersistedRefusal> = self
            .refused
            .lock()
            .iter()
            .map(|(song, videos)| PersistedRefusal {
                song: song.clone(),
                videos: videos.clone(),
            })
            .collect();
        if let Err(e) = state_file::save_atomic(&path, &octo_core::json::to_string(&entries)) {
            self.refused_dirty.store(true, Ordering::SeqCst);
            warn!("refused videos could not be written: {e}");
        }
    }

    /// Writes the registry when anything changed since the last write: the LRU first, most
    /// recently used first (an older Octo keeps the first 10,000 lines, so it keeps the live
    /// ids), then the kept ids, every line with its recency stamp. A kept line is the last
    /// flush's copy unless its id changed meanwhile or its stamp or keys moved.
    pub fn flush(&self) {
        let Some(path) = &self.path else { return };
        let _flushing = self.flush_lock.lock();
        self.flush_refused();
        self.choices.flush();
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        type KeptRow = (String, SharedRouting, Vec<String>, u64);
        let (listed, kept, changed) = {
            let mut guard = self.lru.lock();
            let lru = &mut *guard;
            // Taken before rendering, so a change landing meanwhile is written next time.
            let changed = std::mem::take(&mut lru.kept_changed);
            let listed: Vec<(String, SharedRouting, u64)> = lru
                .order
                .iter()
                .rev()
                .filter_map(|(stamp, id)| lru.by_id.get(id).map(|(r, _)| (id.clone(), r.clone(), *stamp)))
                .collect();
            let mut kept: Vec<KeptRow> = lru
                .kept
                .iter()
                .map(|(id, k)| {
                    (
                        id.clone(),
                        k.routing.clone(),
                        k.keys.iter().cloned().collect(),
                        k.stamp,
                    )
                })
                .collect();
            kept.sort_unstable_by_key(|row| std::cmp::Reverse(row.3));
            (listed, kept, changed)
        };
        let mut text = String::from("[");
        for (id, routing, stamp) in listed {
            let line = octo_core::json::to_string(&Persisted {
                id,
                routing: Some(routing.snapshot()),
                kept: Vec::new(),
                recency: Some(stamp),
            });
            push_line(&mut text, &line);
        }
        {
            let mut lines = self.kept_lines.lock();
            let live: HashSet<&str> = kept.iter().map(|row| row.0.as_str()).collect();
            lines.retain(|id, _| live.contains(id.as_str()));
            for (id, routing, keys, stamp) in &kept {
                let fresh = !changed.contains(id)
                    && lines
                        .get(id)
                        .is_some_and(|line| line.stamp == *stamp && line.keys == *keys);
                if !fresh {
                    self.kept_renders.fetch_add(1, Ordering::Relaxed);
                    let rendered = octo_core::json::to_string(&Persisted {
                        id: id.clone(),
                        routing: Some(routing.snapshot()),
                        kept: keys.clone(),
                        recency: Some(*stamp),
                    });
                    lines.insert(
                        id.clone(),
                        KeptLine {
                            stamp: *stamp,
                            keys: keys.clone(),
                            text: rendered,
                        },
                    );
                }
                if let Some(line) = lines.get(id) {
                    push_line(&mut text, &line.text);
                }
            }
        }
        text.push(']');
        match state_file::save_atomic(path, &text) {
            Ok(()) => {
                self.file_writes.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                // Best-effort: losing a flush costs the ids minted since the last one, which
                // is the behavior we already had. It must never take a request down. The kept
                // ids taken for this write are put back, so the next one renders them again.
                self.lru.lock().kept_changed.extend(changed);
                self.dirty.store(true, Ordering::SeqCst);
                warn!("external id registry could not be written: {e}");
            }
        }
    }

    /// The flush timer, as a worker: every `flush_interval` ([`Self::FLUSH_INTERVAL`] unless
    /// [`Self::with_flush_interval`] set another), and once more on shutdown.
    pub async fn run_flusher(
        self: Arc<Self>,
        token: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        let registry = self.clone();
        let flushed =
            state_file::flush_every(self.flush_interval, token, Arc::new(move || registry.flush())).await;
        // The last write before the stop marks the played index as stopped cleanly.
        let stopping = self.clone();
        let _ = tokio::task::spawn_blocking(move || stopping.choices.stop_cleanly()).await;
        flushed
    }
}

impl Drop for ExternalIdRegistry {
    /// The last flush.
    fn drop(&mut self) {
        self.flush();
    }
}

/// Adds one JSON array element to `text`, which starts with the array's `[`.
fn push_line(text: &mut String, line: &str) {
    if text.len() > 1 {
        text.push(',');
    }
    text.push_str(line);
}

#[cfg(test)]
#[path = "external_id_registry_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "external_id_registry_kept_tests.rs"]
mod kept_tests;

#[path = "external_id_registry_choices.rs"]
mod choices;
pub use choices::{ChooseError, LiveGuard, PickState};

#[cfg(test)]
#[path = "external_id_registry_choice_tests.rs"]
mod choice_tests;
