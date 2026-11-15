//! Binding: a streamed entry takes the library song once the owner's own version of
//! it is in Navidrome, keeping its place, its outside id, its seed and its snapshot.
//!
//! An entry binds to library song `L` only when the owner's own version of its song is on disk
//! (never anyone else's), `L` is that file's Navidrome id, and the owner can read `L`; or (an
//! album heart that brought the same take under another id) when an acquisition row landed the
//! owner's own version of the row's song as `L` and `same_take` says the two are one recording
//! and one take. Binding runs for every ended row (coalesced into one locked operation per
//! playlist), at every locked sync, and in the start sweep.

use std::sync::Arc;

use indexmap::IndexMap;
use octo_core::common::SongIdentity;
use parking_lot::Mutex;
use tokio::sync::Notify;
use tracing::debug;

use super::entries::{take_of_facts, take_of_snapshot};
use super::held_playlists::{HeldLibrary, HeldNavidrome, HeldPlaylists, Readable, streams};
use super::held_store::HeldEntry;
use super::same_take::same_take;
use crate::services::common::acquisition_tracker::AcquisitionEnd;

/// The ended rows waiting to be bound, by playlist, and whether a task is binding them.
#[derive(Default)]
struct BindQueue {
    waiting: IndexMap<String, Vec<AcquisitionEnd>>,
    running: bool,
}

/// Coalesces ended rows into one locked operation per playlist.
#[derive(Default)]
pub struct Binder {
    queue: Mutex<BindQueue>,
    settled: Notify,
}

impl Binder {
    /// An acquisition row ended: a song Navidrome showed queues a bind for every held playlist it
    /// may bind in, and starts the task that binds them when none is running.
    pub(crate) fn ended(&self, held: &Arc<HeldPlaylists>, end: &AcquisitionEnd) {
        if !end.done || end.library_id.as_deref().is_none_or(|id| id.trim().is_empty()) {
            return;
        }
        let playlists = held.playlists_for_end(end);
        if playlists.is_empty() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            debug!(
                "PLAYLISTS no runtime to bind {} on; the next sync binds it",
                end.key
            );
            return;
        };
        let start = {
            let mut queue = self.queue.lock();
            for id in playlists {
                queue.waiting.entry(id).or_default().push(end.clone());
            }
            !std::mem::replace(&mut queue.running, true)
        };
        if start {
            let held = Arc::clone(held);
            runtime.spawn(async move { held.binder().drain(&held).await });
        }
    }

    async fn drain(&self, held: &Arc<HeldPlaylists>) {
        loop {
            let next = {
                let mut queue = self.queue.lock();
                let next = queue.waiting.shift_remove_index(0);
                if next.is_none() {
                    queue.running = false;
                }
                next
            };
            let Some((id, events)) = next else {
                self.settled.notify_waiters();
                return;
            };
            held.bind_now(&id, &events).await;
        }
    }

    /// Resolves once nothing waits and no task is binding.
    pub(crate) async fn settled(&self) {
        loop {
            let notified = self.settled.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.idle() {
                return;
            }
            notified.await;
        }
    }

    fn idle(&self) -> bool {
        let queue = self.queue.lock();
        !queue.running && queue.waiting.is_empty()
    }
}

/// Paths compared as the same file whatever their slashes.
fn same_path(a: &str, b: &str) -> bool {
    a.replace('\\', "/").trim_end_matches('/') == b.replace('\\', "/").trim_end_matches('/')
}

/// The bind check over a playlist's streamed entries, for the owner, with the ended rows that
/// started it (none at a sync or the sweep). True when an entry was bound.
pub(crate) async fn bind_entries(
    library: &dyn HeldLibrary,
    navidrome: &dyn HeldNavidrome,
    owner: &str,
    entries: &mut [HeldEntry],
    events: &[AcquisitionEnd],
) -> bool {
    let mut bound = false;
    for entry in entries.iter_mut().filter(|entry| streams(entry)) {
        let found = match own_version(library, owner, entry, events).await {
            Some(id) => Some(id),
            None => same_take_landed(library, navidrome, owner, entry, events).await,
        };
        if let Some(id) = found {
            entry.library_id = Some(id);
            bound = true;
        }
    }
    bound
}

/// Rule one: the owner's own version of the entry's song is on disk, `L` is that file's id (from
/// an ended row that placed exactly that file, else by its path), and the owner can read `L`.
async fn own_version(
    library: &dyn HeldLibrary,
    owner: &str,
    entry: &HeldEntry,
    events: &[AcquisitionEnd],
) -> Option<String> {
    let (Some(provider), Some(external_id)) = (entry.provider.as_deref(), entry.external_id.as_deref())
    else {
        return None;
    };
    let path = library.own_version_path(owner, provider, external_id).await?;
    let landed = events
        .iter()
        .find(|end| {
            entry.key.as_deref() == Some(end.key.as_str())
                && end.path.as_deref().is_some_and(|placed| same_path(placed, &path))
        })
        .and_then(|end| end.library_id.clone());
    let id = match landed {
        Some(id) => id,
        None => {
            let (artist, title) = entry
                .song
                .as_ref()
                .map_or(("", ""), |song| (song.artist.as_str(), song.title.as_str()));
            library.library_id_at(artist, title, &path).await?
        }
    };
    (library.readable(owner, &id, None) == Readable::Yes).then_some(id)
}

/// Rule two: an ended row landed the owner's own version of the row's song as `L` (an album
/// heart's other id for the same song), its match key is the entry's, `same_take` says one
/// recording and one take, and the owner can read `L`.
async fn same_take_landed(
    library: &dyn HeldLibrary,
    navidrome: &dyn HeldNavidrome,
    owner: &str,
    entry: &HeldEntry,
    events: &[AcquisitionEnd],
) -> Option<String> {
    let snapshot = entry.song.as_ref()?;
    let key = SongIdentity::match_key(&snapshot.artist, &snapshot.title);
    for end in events.iter().filter(|end| end.done) {
        let (Some(id), Some(path), Some(artist), Some(title)) = (
            end.library_id.as_deref(),
            end.path.as_deref(),
            end.artist.as_deref(),
            end.title.as_deref(),
        ) else {
            continue;
        };
        if SongIdentity::match_key(artist, title) != key {
            continue;
        }
        let Some((provider, external_id)) = end.key.split_once(':') else {
            continue;
        };
        let own = library.own_version_path(owner, provider, external_id).await;
        if !own.is_some_and(|own| same_path(&own, path)) {
            continue;
        }
        let Ok(Some(facts)) = navidrome.song_facts(id).await else {
            continue;
        };
        if !same_take(&take_of_snapshot(snapshot), &take_of_facts(&facts)) {
            continue;
        }
        if library.readable(owner, id, facts.library_id) == Readable::Yes {
            return Some(id.to_string());
        }
    }
    None
}
