//! The external id registry, the store behind `external-ids.json`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
pub struct ExternalIdRegistry {
    path: Option<PathBuf>,
    lru: Mutex<Lru>,
    dirty: AtomicBool,
    flush_lock: Mutex<()>,
    recovery: Mutex<Option<RoutingRecovery>>,
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
/// most-recently-used order without a linear removal from a list on every touch.
#[derive(Default)]
struct Lru {
    by_id: HashMap<String, (SharedRouting, u64)>,
    /// Stamp → id; the highest stamp is the most recently used.
    order: BTreeMap<u64, String>,
    next: u64,
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

    /// Most recently used first.
    fn ids(&self) -> Vec<String> {
        self.order.values().rev().cloned().collect()
    }
}

/// One line of `external-ids.json` (the private record `Persisted(string Id, SoulseekRouting Routing)`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct Persisted {
    #[serde(deserialize_with = "state_file::null_as_default")]
    id: String,
    routing: Option<SoulseekRouting>,
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
        let registry = ExternalIdRegistry {
            path,
            lru: Mutex::new(Lru::default()),
            dirty: AtomicBool::new(false),
            flush_lock: Mutex::new(()),
            recovery: Mutex::new(None),
            refused: Mutex::new(IndexMap::new()),
            refused_dirty: AtomicBool::new(false),
            heard_listeners: Mutex::new(Vec::new()),
        };
        if registry.path.is_some() {
            registry.load_refused();
            registry.load();
        }
        registry
    }

    pub fn register(&self, routing: impl Into<SharedRouting>) -> String {
        let routing = routing.into();
        let id = Self::make_short_id(&routing.lock());
        let existing = self.lru.lock().by_id.get(&id).map(|(r, _)| r.clone());
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

        if let Some(previous) = previous {
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
            let (prev_video, prev_stream, prev_refused, prev_pick, prev_contexts, prev_heard) = {
                let p = previous.lock();
                (
                    p.you_tube_id.clone().filter(|v| !v.is_empty()),
                    p.stream_facts(),
                    p.refused_videos.clone(),
                    p.video_pick,
                    p.contexts.clone(),
                    p.heard.clone(),
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
                    Some(_) => {}
                }
            }
        }

        let mut lru = self.lru.lock();
        let stamp = lru.next;
        lru.next += 1;
        if let Some((_, old)) = lru.by_id.insert(id.clone(), (routing, stamp)) {
            lru.order.remove(&old);
        }
        lru.order.insert(stamp, id.clone());
        lru.trim();
        drop(lru);
        self.dirty.store(true, Ordering::SeqCst);
        id
    }

    pub fn lookup(&self, short_id: &str) -> Option<SharedRouting> {
        {
            let mut lru = self.lru.lock();
            if let Some(routing) = lru.by_id.get(short_id).map(|(r, _)| r.clone()) {
                lru.touch(short_id);
                return Some(routing);
            }
        }
        // Never call a source with the LRU locked. Registering checks only the LRU, so
        // recovery cannot recurse, and unknown Navidrome ids still pass through unchanged.
        let recover = self.recovery.lock().clone()?;
        let routing = recover(short_id)?;
        if Self::make_short_id(&routing) != short_id {
            return None;
        }
        let routing = SharedRouting::new(routing);
        self.register(routing.clone());
        Some(routing)
    }

    pub fn set_recovery(&self, recover: RoutingRecovery) {
        *self.recovery.lock() = Some(recover);
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
            let routing = SharedRouting::new(decoded);
            let id = self.register(routing.clone());
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
        self.dirty.store(true, Ordering::SeqCst);
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
            self.dirty.store(true, Ordering::SeqCst);
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

    /// How many ids the registry holds.
    pub fn count(&self) -> usize {
        self.lru.lock().by_id.len()
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
        let order: Vec<(String, SharedRouting)> = {
            let lru = self.lru.lock();
            lru.ids()
                .into_iter()
                .filter_map(|id| lru.by_id.get(&id).map(|(r, _)| (id, r.clone())))
                .collect()
        };

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
        let Some(routing) = self.lru.lock().by_id.get(short_id).map(|(r, _)| r.clone()) else {
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
        self.dirty.store(true, Ordering::SeqCst);
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
        let Some(routing) = self.lru.lock().by_id.get(short_id).map(|(r, _)| r.clone()) else {
            return false;
        };
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
            self.dirty.store(true, Ordering::SeqCst);
        }
        true
    }

    /// Records that the pinned `video` on the song under `short_id` was chosen by ranking
    /// `pick`, but only while the song still plays `video`. True when the record changed.
    pub fn remember_pick(&self, short_id: &str, video: &str, pick: u8) -> bool {
        if short_id.is_empty() || video.is_empty() {
            return false;
        }
        let Some(routing) = self.lru.lock().by_id.get(short_id).map(|(r, _)| r.clone()) else {
            return false;
        };
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
            self.dirty.store(true, Ordering::SeqCst);
        }
        changed
    }

    /// The ids of the songs whose video an older ranking picked, most recently used first: song
    /// routings with a video, an artist and a title whose `video_pick` is below
    /// [`SoulseekRouting::VIDEO_PICK`]. A video the song was named by is never listed.
    pub fn stale_pins(&self) -> Vec<String> {
        let entries: Vec<(String, SharedRouting)> = {
            let lru = self.lru.lock();
            lru.ids()
                .into_iter()
                .filter_map(|id| lru.by_id.get(&id).map(|(r, _)| (id, r.clone())))
                .collect()
        };
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
        let routings: Vec<SharedRouting> = {
            let lru = self.lru.lock();
            lru.ids()
                .into_iter()
                .filter_map(|id| lru.by_id.get(&id).map(|(r, _)| r.clone()))
                .collect()
        };
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
        let Some(routing) = self.lru.lock().by_id.get(short_id).map(|(r, _)| r.clone()) else {
            return false;
        };
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
        self.dirty.store(true, Ordering::SeqCst);
        let old = old?;
        let all: Vec<SharedRouting> = self.lru.lock().by_id.values().map(|(r, _)| r.clone()).collect();
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
        let (mut changed, artist, title, is_song) = {
            let mut r = routing.lock();
            (
                r.refuse_video(video) && found.registered,
                r.artist.clone().unwrap_or_default(),
                r.title.clone().unwrap_or_default(),
                r.kind == RoutingKind::Song,
            )
        };
        if is_song && let Some(key) = Self::song_key(&artist, &title) {
            changed |= self.refuse_song_video(&artist, &title, video);
            // Collect the routings with the cache locked, then lock each one after letting the
            // cache go.
            let all: Vec<SharedRouting> = self.lru.lock().by_id.values().map(|(r, _)| r.clone()).collect();
            for other in all.into_iter().filter(|o| !o.ptr_eq(&routing)) {
                let mut o = other.lock();
                if o.kind == RoutingKind::Song && Self::key_of(&o).as_deref() == Some(key.as_str()) {
                    changed |= o.refuse_video(video);
                }
            }
        }
        if changed {
            self.dirty.store(true, Ordering::SeqCst);
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
        let Some(routing) = self.lru.lock().by_id.get(short_id).map(|(r, _)| r.clone()) else {
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
        let routing = self.lru.lock().by_id.get(short_id).map(|(r, _)| r.clone())?;
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

            // Stored most-recently-used first, so replaying in order rebuilds the same
            // eviction order rather than an arbitrary one.
            let mut lru = self.lru.lock();
            let mut stamp = entries.len() as u64;
            for e in entries {
                let Some(mut routing) = e.routing else { continue };
                if e.id.is_empty() {
                    continue;
                }
                self.apply_refusals(&mut routing);
                match lru.by_id.get_mut(&e.id) {
                    // A duplicate id: the later routing wins, as `_byId[e.Id] = ...` did, at
                    // the earlier (more recent) place.
                    Some((r, _)) => *r = SharedRouting::new(routing),
                    None => {
                        lru.by_id
                            .insert(e.id.clone(), (SharedRouting::new(routing), stamp));
                        lru.order.insert(stamp, e.id);
                        stamp -= 1;
                    }
                }
            }
            lru.next = lru.order.keys().next_back().map_or(0, |s| s + 1);
            lru.trim();
            let count = lru.by_id.len();
            drop(lru);
            info!("external id registry restored {count} entries");
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

    /// Writes the registry when anything changed since the last write.
    pub fn flush(&self) {
        let Some(path) = &self.path else { return };
        let _flushing = self.flush_lock.lock();
        self.flush_refused();
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let order: Vec<(String, SharedRouting)> = {
            let lru = self.lru.lock();
            lru.ids()
                .into_iter()
                .filter_map(|id| lru.by_id.get(&id).map(|(r, _)| (id, r.clone())))
                .collect()
        };
        let entries: Vec<Persisted> = order
            .into_iter()
            .map(|(id, routing)| Persisted {
                id,
                routing: Some(routing.snapshot()),
            })
            .collect();
        if let Err(e) = state_file::save_atomic(path, &octo_core::json::to_string(&entries)) {
            // Best-effort: losing a flush costs the ids minted since the last one, which
            // is the behavior we already had. It must never take a request down.
            self.dirty.store(true, Ordering::SeqCst);
            warn!("external id registry could not be written: {e}");
        }
    }

    /// The flush timer, as a worker: every [`Self::FLUSH_INTERVAL`], and once more on shutdown.
    pub async fn run_flusher(
        self: Arc<Self>,
        token: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        let registry = self.clone();
        state_file::flush_every(Self::FLUSH_INTERVAL, token, Arc::new(move || registry.flush())).await
    }
}

impl Drop for ExternalIdRegistry {
    /// The last flush.
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
#[path = "external_id_registry_tests.rs"]
mod tests;
