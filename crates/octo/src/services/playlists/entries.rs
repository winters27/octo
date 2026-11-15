//! Building held entries from the song ids a client sends, outside the playlist's
//! lock, one lookup per distinct id, at most eight at once.
//!
//! - An outside song (a registry or `yt|` id whose routing is a song, or an
//!   `ext-<provider>-song-<id>`) keeps the id exactly as it was sent. A registry or `yt|` id also
//!   keeps the id the registry resolved it under (`SeedId`, never a hash of a routing that may
//!   carry a pinned video since) and a copy of its routing. Albums, artists and any other kind are
//!   refused.
//! - A library song must be one Navidrome knows (asked as the admin) and one the playlist's owner
//!   can read. Only a definite "no" skips an id; "not now" or "could not tell" ends the whole
//!   operation with the busy error, so a replace never drops songs while an index warms up.
//! - Ids answering to an entry the playlist already holds, or one its `Replaced` record keeps, are
//!   taken over under the lock and not checked again.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use octo_core::models::domain::Song;
use octo_core::soulseek::{RoutingKind, SoulseekRouting};

use super::held_playlists::{HeldError, HeldLibrary, HeldNavidrome, Readable};
use super::held_store::{HeldEntry, SongSnapshot};
use super::same_take::TakeFacts;
use crate::services::common::acquisition_tracker::key_of;
use crate::services::family::audio_gate::ExplicitMark;
use crate::services::library::navidrome_playlist_api::{SongFacts, TrackFacts};
use crate::services::soulseek::{ExternalIdRegistry, SoulseekMetadataService};

/// How many ids are looked up at once.
const AT_ONCE: usize = 8;

/// One song id as an operation will use it.
#[derive(Debug, Clone)]
pub enum Built {
    /// A new entry for the id.
    Entry { id: String, entry: HeldEntry },
    /// An id that answers to an entry the playlist holds or its `Replaced` record keeps: taken
    /// over under the lock.
    Takeover(String),
    /// A definite "no": Navidrome has no such song, the owner can't read it, or the catalog knows
    /// no such outside song.
    Skipped(String),
}

impl Built {
    pub fn id(&self) -> &str {
        match self {
            Built::Entry { id, .. } | Built::Takeover(id) | Built::Skipped(id) => id,
        }
    }

    /// The ids skipped, for `octoSkipped`.
    pub fn skipped(built: &[Built]) -> Vec<String> {
        built
            .iter()
            .filter_map(|built| match built {
                Built::Skipped(id) => Some(id.clone()),
                _ => None,
            })
            .collect()
    }
}

/// What building reads from.
pub(crate) struct BuildContext<'a> {
    pub registry: &'a ExternalIdRegistry,
    pub library: &'a dyn HeldLibrary,
    pub navidrome: &'a dyn HeldNavidrome,
    pub now: DateTime<Utc>,
}

/// What one id's lookup found.
enum Looked {
    Entry(HeldEntry),
    Skipped,
    NotSong,
    Busy,
}

/// The entries for `ids` in order (blank ids left out): [`HeldError::NotSong`] when any id is not
/// a song, [`HeldError::Busy`] when any library id could not be told.
pub(crate) async fn build(
    context: &BuildContext<'_>,
    owner: &str,
    ids: &[String],
    takeovers: &HashSet<String>,
) -> Result<Vec<Built>, HeldError> {
    let mut seen = HashSet::new();
    let distinct: Vec<String> = ids
        .iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty() && !takeovers.contains(*id) && seen.insert(id.to_string()))
        .map(str::to_string)
        .collect();
    let looked: HashMap<String, Looked> = futures::stream::iter(distinct)
        .map(|id| async move {
            let looked = look_up(context, owner, &id).await;
            (id, looked)
        })
        .buffer_unordered(AT_ONCE)
        .collect()
        .await;
    if looked.values().any(|looked| matches!(looked, Looked::NotSong)) {
        return Err(HeldError::NotSong);
    }
    if looked.values().any(|looked| matches!(looked, Looked::Busy)) {
        return Err(HeldError::Busy);
    }
    Ok(ids
        .iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .map(|id| {
            if takeovers.contains(id) {
                return Built::Takeover(id.to_string());
            }
            match looked.get(id) {
                Some(Looked::Entry(entry)) => Built::Entry {
                    id: id.to_string(),
                    entry: entry.clone(),
                },
                _ => Built::Skipped(id.to_string()),
            }
        })
        .collect())
}

async fn look_up(context: &BuildContext<'_>, owner: &str, id: &str) -> Looked {
    if let Some(routing) = context.registry.resolve_song(id, false) {
        let kind = routing.lock().kind;
        if kind != RoutingKind::Song {
            return Looked::NotSong;
        }
        return registry_song(context, id).await;
    }
    let (external, provider, kind, external_id) = context.library.parse_external_id(id);
    if external {
        if !kind
            .as_deref()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("song"))
        {
            return Looked::NotSong;
        }
        let (Some(provider), Some(external_id)) = (provider, external_id) else {
            return Looked::Skipped;
        };
        return catalog_song(context, id, &provider, &external_id).await;
    }
    library_song(context, owner, id).await
}

/// A registry or `yt|` id: `SeedId` is the id the registry resolves it under (the id itself when
/// registered, else the decoded routing's id, which is what `resolve_song` registers it as).
async fn registry_song(context: &BuildContext<'_>, id: &str) -> Looked {
    let seed_id = if context.registry.lookup(id).is_some() {
        id.to_string()
    } else {
        match SoulseekMetadataService::try_decode_external_id(Some(id)) {
            Some(decoded) => ExternalIdRegistry::make_short_id(&decoded),
            None => return Looked::Skipped,
        }
    };
    let Some(routing) = context.registry.resolve_song(id, true) else {
        return Looked::Skipped;
    };
    let seed = routing.snapshot();
    if seed.kind != RoutingKind::Song {
        return Looked::NotSong;
    }
    let song = context.library.catalog_song("soulseek", &seed_id).await;
    let snapshot = song
        .as_ref()
        .map_or_else(|| snapshot_of_routing(&seed), snapshot_of_song);
    Looked::Entry(HeldEntry {
        library_id: None,
        outside_id: Some(id.to_string()),
        provider: Some("soulseek".to_string()),
        external_id: Some(seed_id.clone()),
        key: Some(key_of("soulseek", &seed_id)),
        seed_id: Some(seed_id),
        seed: Some(Arc::new(seed)),
        song: Some(snapshot),
        added: context.now,
    })
}

/// An `ext-<provider>-song-<id>`: the catalog's song is the snapshot; none, and the id is skipped.
async fn catalog_song(context: &BuildContext<'_>, id: &str, provider: &str, external_id: &str) -> Looked {
    let Some(song) = context.library.catalog_song(provider, external_id).await else {
        return Looked::Skipped;
    };
    Looked::Entry(HeldEntry {
        library_id: None,
        outside_id: Some(id.to_string()),
        provider: Some(provider.to_string()),
        external_id: Some(external_id.to_string()),
        key: Some(key_of(provider, external_id)),
        seed_id: None,
        seed: None,
        song: Some(snapshot_of_song(&song)),
        added: context.now,
    })
}

/// A library id: Navidrome must know it and the owner must be able to read it; the outside song
/// it was downloaded from, when Octo downloaded it, is its fallback (with the registry's routing
/// as its seed, read with `peek`).
async fn library_song(context: &BuildContext<'_>, owner: &str, id: &str) -> Looked {
    let facts = match context.navidrome.song_facts(id).await {
        Err(_) => return Looked::Busy,
        Ok(None) => return Looked::Skipped,
        Ok(Some(facts)) => facts,
    };
    match context.library.readable(owner, id, facts.library_id) {
        Readable::No => return Looked::Skipped,
        Readable::NotNow => return Looked::Busy,
        Readable::Yes => {}
    }
    let mut entry = HeldEntry {
        library_id: Some(id.to_string()),
        outside_id: None,
        provider: None,
        external_id: None,
        key: None,
        seed_id: None,
        seed: None,
        song: Some(snapshot_of_facts(&facts)),
        added: context.now,
    };
    if let Some((provider, external_id)) = context.library.downloaded_from(id).await {
        let peeked = provider
            .eq_ignore_ascii_case("soulseek")
            .then(|| context.registry.peek(&external_id))
            .flatten();
        entry.outside_id = Some(if peeked.is_some() {
            external_id.clone()
        } else {
            format!("ext-{provider}-song-{external_id}")
        });
        if let Some(routing) = peeked {
            entry.seed_id = Some(external_id.clone());
            entry.seed = Some(Arc::new(routing.snapshot()));
        }
        entry.key = Some(key_of(&provider, &external_id));
        entry.provider = Some(provider);
        entry.external_id = Some(external_id);
    }
    Looked::Entry(entry)
}

/// The explicit word Octo keeps for a catalog's explicit code.
fn explicit_word(code: Option<i32>) -> Option<String> {
    match ExplicitMark::from_catalog(code) {
        ExplicitMark::Explicit => Some("explicit".to_string()),
        ExplicitMark::Clean => Some("clean".to_string()),
        ExplicitMark::Unrated => None,
    }
}

/// A catalog song's snapshot.
pub fn snapshot_of_song(song: &Song) -> SongSnapshot {
    let mut isrcs = song.isrcs.clone();
    if let Some(isrc) = song.isrc.clone().filter(|isrc| !isrc.trim().is_empty())
        && !isrcs.contains(&isrc)
    {
        isrcs.insert(0, isrc);
    }
    SongSnapshot {
        title: song.title.clone(),
        artist: song.artist.clone(),
        artist_id: song.artist_id.clone(),
        album: song.album.clone(),
        album_id: song.album_id.clone(),
        seconds: song.duration,
        track: song.track,
        disc: song.disc_number,
        year: song.year,
        genre: song.genre.clone(),
        isrcs,
        explicit_status: explicit_word(song.explicit_content_lyrics),
        cover_art: song.album_id.clone(),
    }
}

/// A routing's snapshot, for a song the catalog answered nothing for.
pub fn snapshot_of_routing(routing: &SoulseekRouting) -> SongSnapshot {
    SongSnapshot {
        title: routing.title.clone().unwrap_or_default(),
        artist: routing.artist.clone().unwrap_or_default(),
        artist_id: routing.external_artist_id.clone(),
        album: routing.album.clone().unwrap_or_default(),
        album_id: None,
        seconds: routing.shown_duration.or(routing.duration),
        track: routing.track,
        disc: routing.disc_number,
        year: None,
        genre: None,
        isrcs: routing.isrc.clone().into_iter().collect(),
        explicit_status: explicit_word(routing.explicit_content),
        cover_art: None,
    }
}

/// A library song's snapshot, from Navidrome's facts (its explicit mark as Octo's word).
pub fn snapshot_of_facts(facts: &SongFacts) -> SongSnapshot {
    SongSnapshot::from(facts)
}

/// A library entry for a row of Navidrome's list (a conversion, a move-back onto a plain list).
pub fn library_entry(row: &TrackFacts, now: DateTime<Utc>) -> HeldEntry {
    HeldEntry {
        library_id: Some(row.media_file_id.clone()),
        outside_id: None,
        provider: None,
        external_id: None,
        key: None,
        seed_id: None,
        seed: None,
        song: Some(SongSnapshot {
            title: row.title.clone(),
            artist: row.artist.clone(),
            artist_id: None,
            album: String::new(),
            album_id: None,
            seconds: row.seconds,
            track: None,
            disc: None,
            year: None,
            genre: None,
            isrcs: Vec::new(),
            explicit_status: None,
            cover_art: None,
        }),
        added: now,
    }
}

/// What `same_take` and the phones' match compare, from a snapshot.
pub fn take_of_snapshot(snapshot: &SongSnapshot) -> TakeFacts {
    snapshot.take_facts()
}

/// What `same_take` compares, from Navidrome's facts.
pub fn take_of_facts(facts: &SongFacts) -> TakeFacts {
    TakeFacts::from(facts)
}
