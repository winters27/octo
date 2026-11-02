//! Apple's catalog standing in for Deezer's: a Deezer API request that Deezer hides from this
//! server (its catalog searches, an artist's albums and top songs, the chart) is answered from
//! Apple's catalog in Deezer's own shape, so every reader of Deezer's answers works unchanged. An
//! Apple id rides in Deezer's id fields above `APPLE_ID_BASE`, and a request naming one is always
//! answered from Apple, whatever Deezer's reach.
//!
//! Albums Apple finds take their Deezer twin's id when Apple's artwork names a barcode Deezer
//! knows and the twin is the same record (title, track count and explicit flag), so they keep
//! Deezer's ISRCs; the title and artist name stay Apple's, as people saw them. Every list the
//! stand-in builds carries the twins already known, and an Apple album id with a twin is answered
//! from the twin.
//!
//! Every Apple song and album the stand-in hands out leaves its cover as an artwork hint, so
//! finding that cover later asks Apple nothing.

use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use octo_core::common::{SongIdentity, dotnet};
use octo_core::metadata::apple_as_deezer::{self, AppleRead, apple_id, code_apple_id, marked};
use octo_core::soulseek::soulseek_metadata_service::RoutingKind;
use reqwest::StatusCode;
use serde_json::{Value, json};
use tracing::debug;

use super::deezer_rate_limit_handler::{AskFor, DeezerRateLimitHandler};
use super::deezer_reach::{DeezerReach, Infos, InfosReader, Outcome, Reach};
use crate::services::apple::{AppleAsked, AppleCatalog, AppleClass, Entity};
use crate::services::cover_art::ArtworkHints;
use crate::services::framework::{HttpAnswer, MemoryCache};
use crate::services::library::album_ownership::AlbumOwnership;

/// Twin lookups one answer may make: its first rows.
const TWINS_PER_ANSWER: usize = 8;
/// How long an answer waits for its twin lookups; one still running then records its verdict
/// when it finishes.
const TWIN_WAIT: Duration = Duration::from_secs(2);
/// How long what the stand-in learned is kept.
const KEPT_FOR: Duration = Duration::from_secs(12 * 60 * 60);
/// Entries each map keeps.
const MAP_ENTRIES: u64 = 4096;
/// Tracks the tracklist cache keeps, counted one per track.
const TRACKLIST_TRACKS: u64 = 40_000;
/// Albums an artist lookup asks for.
const ARTIST_ALBUMS: u32 = 200;
/// Exact-name artists weighed before the closest album count decides.
const ARTIST_CANDIDATES: u32 = 10;

/// What a Deezer request asks, as the stand-in answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Question {
    /// `search` or `search/track`.
    Tracks {
        q: String,
        limit: u32,
        index: u32,
    },
    /// `search/album`.
    Albums {
        q: String,
        limit: u32,
        index: u32,
    },
    /// `search/artist`, answered from Apple only while Deezer is unreachable.
    Artists {
        q: String,
        limit: u32,
    },
    ArtistAlbums {
        artist: ArtistRef,
        limit: u32,
        index: u32,
    },
    ArtistTop {
        artist: ArtistRef,
        limit: u32,
    },
    Artist {
        apple: u64,
    },
    ArtistRelated {
        apple: u64,
        limit: u32,
    },
    Chart {
        limit: u32,
    },
    Album {
        apple: u64,
    },
    AlbumTracks {
        apple: u64,
        limit: u32,
        index: u32,
    },
    Track {
        apple: u64,
    },
    /// A Deezer album with a known Apple twin, while Deezer is unreachable: Apple's album under
    /// the Deezer id asked for.
    TwinAlbum {
        apple: u64,
        deezer: String,
    },
    TwinAlbumTracks {
        apple: u64,
        limit: u32,
        index: u32,
    },
}

/// Whose artist page: Deezer's own artist, or one Apple named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtistRef {
    Deezer(String),
    Apple(u64),
}

/// Where a Deezer request goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Apple answers; Deezer is not asked.
    Apple(Question),
    /// Neither can answer now (Deezer unreachable, a Deezer id): a 503 at once.
    Refuse,
    /// Deezer is asked; a hidden answer is then confirmed and replaced from Apple.
    Check(Question),
    /// Deezer as ever.
    Deezer,
}

/// A twin lookup's verdict, kept by Apple album id.
#[derive(Debug, Clone)]
enum Twin {
    /// The Deezer album that is this record: its own album object.
    Found { deezer_id: String, album: Arc<Value> },
    /// No Deezer album is this record.
    None,
}

/// Twin verdicts by Apple album id, and the Apple album of each Deezer twin.
struct TwinMap {
    by_apple: MemoryCache<Twin>,
    by_deezer: MemoryCache<u64>,
}

impl TwinMap {
    fn new() -> Self {
        Self {
            by_apple: MemoryCache::new(MAP_ENTRIES),
            by_deezer: MemoryCache::new(MAP_ENTRIES),
        }
    }

    fn get(&self, generation: u64, apple: u64) -> Option<Twin> {
        self.by_apple.get(&key(generation, "twin", apple))
    }

    fn apple_of(&self, generation: u64, deezer_id: &str) -> Option<u64> {
        self.by_deezer.get(&key_text(generation, "twin-of", deezer_id))
    }

    fn record(&self, generation: u64, apple: u64, twin: Twin) {
        if let Twin::Found { deezer_id, .. } = &twin {
            self.by_deezer
                .set(key_text(generation, "twin-of", deezer_id), apple, 1, KEPT_FOR);
        }
        self.by_apple
            .set(key(generation, "twin", apple), twin, 1, KEPT_FOR);
    }

    fn clear(&self) {
        self.by_apple.clear();
        self.by_deezer.clear();
    }
}

/// How a Deezer artist reads for the artist map: its name and album count.
#[derive(Debug, Clone)]
struct DeezerArtist {
    name: String,
    albums: i64,
    object: Arc<Value>,
}

/// The stand-in. One per server, held by the Deezer door.
pub struct DeezerStandIn {
    reach: Arc<DeezerReach>,
    apple: Arc<AppleCatalog>,
    door: OnceLock<Weak<DeezerRateLimitHandler>>,
    hints: OnceLock<Arc<ArtworkHints>>,
    seeds: MemoryCache<Arc<Value>>,
    tracklists: MemoryCache<Arc<Vec<Value>>>,
    twins: Arc<TwinMap>,
    artist_ids: MemoryCache<String>,
    deezer_artists: MemoryCache<Option<DeezerArtist>>,
}

impl DeezerStandIn {
    pub fn new(reach: Arc<DeezerReach>, apple: Arc<AppleCatalog>) -> Self {
        Self {
            reach,
            apple,
            door: OnceLock::new(),
            hints: OnceLock::new(),
            seeds: MemoryCache::new(MAP_ENTRIES * 2),
            tracklists: MemoryCache::new(TRACKLIST_TRACKS),
            twins: Arc::new(TwinMap::new()),
            artist_ids: MemoryCache::new(MAP_ENTRIES * 2),
            deezer_artists: MemoryCache::new(MAP_ENTRIES),
        }
    }

    /// The door the stand-in's own Deezer questions go through; set once, when it is wired.
    pub fn set_door(&self, door: Weak<DeezerRateLimitHandler>) {
        let _ = self.door.set(door);
    }

    /// Where the covers of what the stand-in hands out are left; set once, when it is wired.
    pub fn set_hints(&self, hints: Arc<ArtworkHints>) {
        let _ = self.hints.set(hints);
    }

    pub fn reach(&self) -> &Arc<DeezerReach> {
        &self.reach
    }

    /// Frees what was learned, after a catalog switch (keys carry the generation anyway).
    pub fn clear(&self) {
        self.seeds.clear();
        self.tracklists.clear();
        self.twins.clear();
        self.artist_ids.clear();
        self.deezer_artists.clear();
    }

    /// Where a request to Deezer's API goes, from its path and query.
    pub fn route(&self, url: &reqwest::Url) -> Route {
        let status = self.reach.status();
        let (reach, generation) = (status.reach, status.generation);
        let stands_in = reach.stands_in();
        let unreachable = matches!(reach, Reach::Unreachable { .. });
        let query = |name: &str| -> Option<String> {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
        };
        let number = |name: &str, default: u32| -> u32 {
            query(name)
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(default)
        };
        let segments: Vec<&str> = url
            .path_segments()
            .map(|parts| parts.filter(|part| !part.is_empty()).collect())
            .unwrap_or_default();
        let deezer_or_refuse = if unreachable { Route::Refuse } else { Route::Deezer };
        let searched = |question: Question| {
            if stands_in {
                Route::Apple(question)
            } else {
                Route::Check(question)
            }
        };
        match segments.as_slice() {
            ["search"] | ["search", "track"] => match query("q").filter(|q| !dotnet::is_blank(q)) {
                Some(q) => searched(Question::Tracks {
                    q,
                    limit: number("limit", 25),
                    index: number("index", 0),
                }),
                None => Route::Deezer,
            },
            ["search", "album"] => match query("q").filter(|q| !dotnet::is_blank(q)) {
                Some(q) => searched(Question::Albums {
                    q,
                    limit: number("limit", 25),
                    index: number("index", 0),
                }),
                None => Route::Deezer,
            },
            ["search", "artist"] if unreachable => match query("q").filter(|q| !dotnet::is_blank(q)) {
                Some(q) => Route::Apple(Question::Artists {
                    q,
                    limit: number("limit", 25),
                }),
                None => Route::Refuse,
            },
            ["album", id] => match apple_id(id) {
                Some(apple) => Route::Apple(Question::Album { apple }),
                None if unreachable => match self.twins.apple_of(generation, id) {
                    Some(apple) => Route::Apple(Question::TwinAlbum {
                        apple,
                        deezer: (*id).to_string(),
                    }),
                    None => Route::Refuse,
                },
                None => Route::Deezer,
            },
            ["album", id, "tracks"] => {
                let limit = number("limit", 25);
                let index = number("index", 0);
                match apple_id(id) {
                    Some(apple) => Route::Apple(Question::AlbumTracks { apple, limit, index }),
                    None if unreachable => match self.twins.apple_of(generation, id) {
                        Some(apple) => Route::Apple(Question::TwinAlbumTracks { apple, limit, index }),
                        None => Route::Refuse,
                    },
                    None => Route::Deezer,
                }
            }
            ["track", id] => match apple_id(id) {
                Some(apple) => Route::Apple(Question::Track { apple }),
                None => deezer_or_refuse,
            },
            ["artist", id] => match apple_id(id) {
                Some(apple) => Route::Apple(Question::Artist { apple }),
                None => deezer_or_refuse,
            },
            ["artist", id, "related"] => match apple_id(id) {
                Some(apple) => Route::Apple(Question::ArtistRelated {
                    apple,
                    limit: number("limit", 25),
                }),
                None => deezer_or_refuse,
            },
            ["artist", id, "albums"] => {
                let limit = number("limit", 25);
                let index = number("index", 0);
                match apple_id(id) {
                    Some(apple) => Route::Apple(Question::ArtistAlbums {
                        artist: ArtistRef::Apple(apple),
                        limit,
                        index,
                    }),
                    None => searched(Question::ArtistAlbums {
                        artist: ArtistRef::Deezer((*id).to_string()),
                        limit,
                        index,
                    }),
                }
            }
            ["artist", id, "top"] => {
                let limit = number("limit", 25);
                match apple_id(id) {
                    Some(apple) => Route::Apple(Question::ArtistTop {
                        artist: ArtistRef::Apple(apple),
                        limit,
                    }),
                    None => searched(Question::ArtistTop {
                        artist: ArtistRef::Deezer((*id).to_string()),
                        limit,
                    }),
                }
            }
            ["chart", ..] => searched(Question::Chart {
                limit: number("limit", 10),
            }),
            _ => Route::Deezer,
        }
    }

    /// Deezer's answer to a [`Route::Check`] request, or Apple's in its place when Deezer hid it:
    /// a catalog search with no rows while it says there are some, or a list with no rows while
    /// the reach is not known yet, is confirmed with `/infos`. Closed: Apple answers. Open: Deezer's
    /// answer stands. No answer from `/infos`: a 503, never "nothing".
    pub async fn after(
        &self,
        question: &Question,
        origin: &str,
        purpose: AskFor,
        deezer: anyhow::Result<HttpAnswer>,
    ) -> anyhow::Result<HttpAnswer> {
        let Ok(answer) = deezer else {
            return deezer;
        };
        if !answer.is_success() {
            return Ok(answer);
        }
        let Ok(doc) = serde_json::from_slice::<Value>(&answer.body) else {
            return Ok(answer);
        };
        if doc.get("error").is_some() {
            return Ok(answer);
        }
        let rows = doc.get("data").and_then(Value::as_array).map_or(0, Vec::len);
        let total = doc.get("total").and_then(Value::as_i64).unwrap_or(0);
        let confirm = match question {
            Question::Tracks { index, .. } | Question::Albums { index, .. } => {
                if rows > 0 {
                    self.reach.observe_rows();
                    false
                } else {
                    *index == 0 && total > 0
                }
            }
            Question::ArtistAlbums { .. } | Question::ArtistTop { .. } | Question::Chart { .. } => {
                rows == 0 && self.reach.reach() == Reach::Unknown
            }
            _ => false,
        };
        if !confirm {
            return Ok(answer);
        }
        match self.reach.read_infos().await {
            Infos::Closed { .. } => Ok(self.answer(question, origin, purpose).await),
            Infos::Open { .. } => Ok(answer),
            Infos::Silent | Infos::NotSent => Ok(HttpAnswer::empty(StatusCode::SERVICE_UNAVAILABLE)),
        }
    }

    /// Apple's answer to `question`, in Deezer's shape: HTTP 200 with the document, or a 503 when
    /// Apple did not answer.
    pub async fn answer(&self, question: &Question, origin: &str, purpose: AskFor) -> HttpAnswer {
        let generation = self.reach.generation();
        let class = purpose.class;
        let reach_caused = matches!(
            question,
            Question::Tracks { .. }
                | Question::Albums { .. }
                | Question::Artists { .. }
                | Question::Chart { .. }
                | Question::ArtistAlbums {
                    artist: ArtistRef::Deezer(_),
                    ..
                }
                | Question::ArtistTop {
                    artist: ArtistRef::Deezer(_),
                    ..
                }
                | Question::TwinAlbum { .. }
                | Question::TwinAlbumTracks { .. }
        );
        if reach_caused && class == AppleClass::Page && self.reach.stands_in() {
            self.reach.page_answered();
        }
        let answered = match question {
            Question::Tracks { q, limit, index } => self.tracks(q, *limit, *index, class, generation).await,
            Question::Albums { q, limit, index } => {
                self.albums(q, *limit, *index, origin, purpose, generation).await
            }
            Question::Artists { q, limit } => self.artists(q, *limit, class).await,
            Question::ArtistAlbums { artist, limit, index } => {
                self.artist_albums(artist, *limit, *index, origin, purpose, generation)
                    .await
            }
            Question::ArtistTop { artist, limit } => {
                self.artist_top(artist, *limit, origin, purpose, generation).await
            }
            Question::Artist { apple } => self.artist(*apple, origin, purpose, generation).await,
            Question::ArtistRelated { apple, limit } => {
                self.artist_related(*apple, *limit, origin, purpose, generation)
                    .await
            }
            Question::Chart { limit } => self.chart(*limit, class, generation).await,
            Question::Album { apple } => self.album(*apple, origin, purpose, generation).await,
            Question::AlbumTracks { apple, limit, index } => {
                self.album_tracks(*apple, *limit, *index, origin, purpose, generation)
                    .await
            }
            Question::Track { apple } => self.track(*apple, class, generation).await,
            Question::TwinAlbum { apple, deezer } => self
                .album(*apple, origin, purpose, generation)
                .await
                .map(|doc| with_id(doc, deezer)),
            Question::TwinAlbumTracks { apple, limit, index } => {
                self.album_tracks(*apple, *limit, *index, origin, purpose, generation)
                    .await
            }
        };
        match answered {
            Some(doc) => HttpAnswer::from_json(StatusCode::OK, &doc),
            None => HttpAnswer::empty(StatusCode::SERVICE_UNAVAILABLE),
        }
    }

    // ---- the questions ---------------------------------------------------------------------

    async fn tracks(
        &self,
        q: &str,
        limit: u32,
        index: u32,
        class: AppleClass,
        generation: u64,
    ) -> Option<Value> {
        let doc = self.asked(
            self.apple
                .search(q, Entity::Song, (index + limit).min(200), class)
                .await,
            "search",
        )?;
        let Some(doc) = doc else {
            return Some(apple_as_deezer::list_answer(Vec::new(), 0));
        };
        let mut tracks = self.read(apple_as_deezer::tracks(&doc), "search")?;
        for track in &mut tracks {
            self.with_known_twin_album(track, generation);
            self.seed("track", track, generation);
        }
        Some(slice(tracks, index, limit))
    }

    async fn albums(
        &self,
        q: &str,
        limit: u32,
        index: u32,
        origin: &str,
        purpose: AskFor,
        generation: u64,
    ) -> Option<Value> {
        let doc = self.asked(
            self.apple
                .search(q, Entity::Album, (index + limit).min(200), purpose.class)
                .await,
            "search",
        )?;
        let Some(doc) = doc else {
            return Some(apple_as_deezer::list_answer(Vec::new(), 0));
        };
        let albums = self.read(apple_as_deezer::albums(&doc), "search")?;
        let shown: Vec<Value> = albums
            .into_iter()
            .skip(index as usize)
            .take(limit as usize)
            .collect();
        for album in &shown {
            self.seed("album", album, generation);
        }
        let (shown, partial) = self.find_twins(shown, origin, generation).await;
        let total = shown.len() + index as usize;
        let mut answer = apple_as_deezer::list_answer(shown, total);
        if partial && let Value::Object(map) = &mut answer {
            map.insert("octo_partial".into(), true.into());
        }
        Some(answer)
    }

    async fn artists(&self, q: &str, limit: u32, class: AppleClass) -> Option<Value> {
        let doc = self.asked(
            self.apple
                .search(q, Entity::MusicArtist, limit.clamp(1, 25), class)
                .await,
            "search",
        )?;
        let Some(doc) = doc else {
            return Some(apple_as_deezer::list_answer(Vec::new(), 0));
        };
        let artists = self.read(apple_as_deezer::artists(&doc), "search")?;
        let total = artists.len();
        Some(apple_as_deezer::list_answer(artists, total))
    }

    async fn artist_albums(
        &self,
        artist: &ArtistRef,
        limit: u32,
        index: u32,
        origin: &str,
        purpose: AskFor,
        generation: u64,
    ) -> Option<Value> {
        let Some(apple) = self.apple_artist(artist, origin, purpose, generation).await? else {
            return Some(apple_as_deezer::list_answer(Vec::new(), 0));
        };
        let mut albums = self.artist_album_list(apple, purpose.class).await?;
        for album in &mut albums {
            self.seed("album", album, generation);
            self.with_known_twin(album, generation);
        }
        Some(slice(albums, index, limit))
    }

    async fn artist_top(
        &self,
        artist: &ArtistRef,
        limit: u32,
        origin: &str,
        purpose: AskFor,
        generation: u64,
    ) -> Option<Value> {
        let Some(apple) = self.apple_artist(artist, origin, purpose, generation).await? else {
            return Some(apple_as_deezer::list_answer(Vec::new(), 0));
        };
        let doc = self.asked(
            self.apple
                .lookup(&[apple], Some(Entity::Song), limit.clamp(1, 200), purpose.class)
                .await,
            "lookup",
        )?;
        let Some(doc) = doc else {
            return Some(apple_as_deezer::list_answer(Vec::new(), 0));
        };
        let mut tracks = self.read(apple_as_deezer::tracks(&doc), "lookup")?;
        for track in &mut tracks {
            self.with_known_twin_album(track, generation);
            self.seed("track", track, generation);
        }
        let total = tracks.len();
        Some(apple_as_deezer::list_answer(tracks, total))
    }

    async fn artist(&self, apple: u64, origin: &str, purpose: AskFor, generation: u64) -> Option<Value> {
        let doc = self.asked(
            self.apple.lookup(&[apple], None, 1, purpose.class).await,
            "lookup",
        )?;
        let Some(doc) = doc else {
            return Some(apple_as_deezer::no_data_answer());
        };
        let mut object = self
            .read(apple_as_deezer::artists(&doc), "lookup")?
            .into_iter()
            .next()?;
        if let Some(deezer) = self
            .deezer_twin_artist(apple, &object, origin, purpose, generation)
            .await
        {
            for field in [
                "picture",
                "picture_small",
                "picture_medium",
                "picture_big",
                "picture_xl",
                "nb_fan",
            ] {
                if let (Value::Object(target), Some(value)) = (&mut object, deezer.object.get(field)) {
                    target.insert(field.into(), value.clone());
                }
            }
        }
        Some(marked(object))
    }

    async fn artist_related(
        &self,
        apple: u64,
        limit: u32,
        origin: &str,
        purpose: AskFor,
        generation: u64,
    ) -> Option<Value> {
        let doc = self.asked(
            self.apple.lookup(&[apple], None, 1, purpose.class).await,
            "lookup",
        )?;
        let object = doc.and_then(|doc| match apple_as_deezer::artists(&doc) {
            AppleRead::Read(found) => found.into_iter().next(),
            AppleRead::Unreadable => None,
        });
        let Some(object) = object else {
            return Some(apple_as_deezer::list_answer(Vec::new(), 0));
        };
        let Some(deezer) = self
            .deezer_twin_artist(apple, &object, origin, purpose, generation)
            .await
        else {
            return Some(apple_as_deezer::list_answer(Vec::new(), 0));
        };
        let deezer_id = deezer.object.get("id").map(Value::to_string).unwrap_or_default();
        let url = format!("{origin}/artist/{deezer_id}/related?limit={limit}");
        let answer = self.deezer(&url, purpose).await?;
        serde_json::from_slice::<Value>(&answer.body).ok()
    }

    async fn chart(&self, limit: u32, class: AppleClass, generation: u64) -> Option<Value> {
        let ids = match self.apple.chart_ids(limit, class).await {
            AppleAsked::Answer(ids) => ids,
            AppleAsked::NoData => return Some(apple_as_deezer::list_answer(Vec::new(), 0)),
            AppleAsked::Busy => return None,
        };
        let doc = self.asked(
            self.apple
                .lookup(&ids, None, u32::try_from(ids.len()).unwrap_or(200), class)
                .await,
            "lookup",
        )?;
        let Some(doc) = doc else {
            return Some(apple_as_deezer::list_answer(Vec::new(), 0));
        };
        let tracks = self.read(apple_as_deezer::tracks(&doc), "lookup")?;
        // In the chart's order.
        let mut ordered: Vec<Value> = ids
            .iter()
            .filter_map(|id| {
                let coded = json!(code_apple_id(*id));
                tracks.iter().find(|track| track["id"] == coded).cloned()
            })
            .collect();
        for track in &mut ordered {
            self.with_known_twin_album(track, generation);
            self.seed("track", track, generation);
        }
        let total = ordered.len();
        Some(apple_as_deezer::list_answer(ordered, total))
    }

    /// An Apple album: from its Deezer twin when it has one (Deezer's tracks, ISRCs and barcode,
    /// under Apple's id, title and artist name), else Apple's own. Partial when the twin could
    /// not be asked or used.
    async fn album(&self, apple: u64, origin: &str, purpose: AskFor, generation: u64) -> Option<Value> {
        let Some(apple_album) = self.apple_album(apple, purpose.class, generation).await? else {
            return Some(apple_as_deezer::no_data_answer());
        };
        let (twin, partial) = self.twin_of(Some(&apple_album), apple, origin, generation).await;
        if let Some(deezer_id) = twin {
            if let Some(mut doc) = self
                .deezer_doc(&format!("{origin}/album/{deezer_id}"), purpose)
                .await
            {
                with_apple_names(&mut doc, &apple_album);
                return Some(with_id(doc, &code_apple_id(apple).to_string()));
            }
            return Some(partial_answer(marked(apple_album)));
        }
        let doc = marked(apple_album);
        Some(if partial { partial_answer(doc) } else { doc })
    }

    /// An Apple album's songs: its Deezer twin's when it has one, else Apple's. Partial when the
    /// twin could not be asked or used.
    async fn album_tracks(
        &self,
        apple: u64,
        limit: u32,
        index: u32,
        origin: &str,
        purpose: AskFor,
        generation: u64,
    ) -> Option<Value> {
        let mut tracks = self.tracklists.get(&key(generation, "tracks", apple));
        if tracks.is_none() && self.seeds.get(&key(generation, "album", apple)).is_none() {
            // The tracklist's answer names the album too, and with it its barcodes.
            tracks = Some(self.tracklist(apple, purpose.class, generation).await?);
        }
        let seed = self.seeds.get(&key(generation, "album", apple));
        let (twin, mut partial) = self.twin_of(seed.as_deref(), apple, origin, generation).await;
        if let Some(deezer_id) = twin {
            let url = format!("{origin}/album/{deezer_id}/tracks?limit={limit}&index={index}");
            if let Some(doc) = self.deezer_doc(&url, purpose).await {
                return Some(doc);
            }
            partial = true;
        }
        let tracks = match tracks {
            Some(tracks) => tracks,
            None => self.tracklist(apple, purpose.class, generation).await?,
        };
        if tracks.is_empty() {
            return Some(apple_as_deezer::no_data_answer());
        }
        let answer = slice(tracks.as_ref().clone(), index, limit);
        Some(if partial { partial_answer(answer) } else { answer })
    }

    /// An Apple album's own object: its seed, else looked up. Inner None: Apple has no such album.
    async fn apple_album(&self, apple: u64, class: AppleClass, generation: u64) -> Option<Option<Value>> {
        if let Some(seed) = self.seeds.get(&key(generation, "album", apple)) {
            return Some(Some((*seed).clone()));
        }
        let doc = self.asked(self.apple.lookup(&[apple], None, 1, class).await, "lookup")?;
        let Some(doc) = doc else {
            return Some(None);
        };
        let album = self
            .read(apple_as_deezer::albums(&doc), "lookup")?
            .into_iter()
            .next();
        if let Some(album) = &album {
            self.seed("album", album, generation);
        }
        Some(album)
    }

    async fn track(&self, apple: u64, class: AppleClass, generation: u64) -> Option<Value> {
        if let Some(seed) = self.seeds.get(&key(generation, "track", apple)) {
            return Some(marked((*seed).clone()));
        }
        let doc = self.asked(self.apple.lookup(&[apple], None, 1, class).await, "lookup")?;
        let Some(doc) = doc else {
            return Some(apple_as_deezer::no_data_answer());
        };
        match self
            .read(apple_as_deezer::tracks(&doc), "lookup")?
            .into_iter()
            .next()
        {
            Some(mut track) => {
                self.with_known_twin_album(&mut track, generation);
                self.seed("track", &track, generation);
                Some(marked(track))
            }
            None => Some(apple_as_deezer::no_data_answer()),
        }
    }

    // ---- tracklists ------------------------------------------------------------------------

    /// One album's songs, kept or fetched.
    async fn tracklist(&self, apple: u64, class: AppleClass, generation: u64) -> Option<Arc<Vec<Value>>> {
        let kept = key(generation, "tracks", apple);
        if let Some(tracks) = self.tracklists.get(&kept) {
            return Some(tracks);
        }
        let tracks = Arc::new(self.fetch_tracklist(apple, class, generation).await?);
        let weight = tracks.len().max(1) as u64;
        self.tracklists.set(kept, Arc::clone(&tracks), weight, KEPT_FOR);
        Some(tracks)
    }

    /// One album's songs: from the lookup gathering for Apple's next turn, else, when the
    /// gathered answer lacks it, asked alone (through every store used).
    async fn fetch_tracklist(&self, apple: u64, class: AppleClass, generation: u64) -> Option<Vec<Value>> {
        if let Some(doc) = self.asked(self.apple.tracklists(apple, class).await, "lookup")?
            && let Some(tracks) = self.songs_of(&doc, apple, generation)?
        {
            return Some(tracks);
        }
        let doc = self.asked(
            self.apple.lookup(&[apple], Some(Entity::Song), 300, class).await,
            "lookup",
        )?;
        Some(match doc {
            Some(doc) => self.songs_of(&doc, apple, generation)?.unwrap_or_default(),
            None => Vec::new(),
        })
    }

    /// `apple`'s songs in a tracklist lookup's answer, None inside when the answer lacks the album.
    fn songs_of(&self, doc: &Value, apple: u64, generation: u64) -> Option<Option<Vec<Value>>> {
        let mut lists = self.read(apple_as_deezer::tracklists(doc), "lookup")?;
        for (album, _) in lists.values() {
            if let Some(album) = album {
                self.seed("album", album, generation);
            }
        }
        Some(lists.remove(&apple).map(|(_, mut songs)| {
            for song in &mut songs {
                self.with_known_twin_album(song, generation);
                self.seed("track", song, generation);
            }
            songs
        }))
    }

    // ---- twins -----------------------------------------------------------------------------

    /// The albums with their Deezer twins' ids in place: known twins from the map; the first
    /// [`TWINS_PER_ANSWER`] albums not in the map are looked up first. Also whether the answer is
    /// partial (kept only briefly by the reader): a lookup went unanswered, or Deezer is
    /// unreachable and an album's twin could not be asked.
    async fn find_twins(&self, albums: Vec<Value>, origin: &str, generation: u64) -> (Vec<Value>, bool) {
        let unknown: Vec<(u64, Value)> = albums
            .iter()
            .filter_map(|album| Some((album_apple_id(album)?, album)))
            .filter(|(apple, album)| self.twins.get(generation, *apple).is_none() && has_barcodes(album))
            .map(|(apple, album)| (apple, album.clone()))
            .collect();
        let partial = if matches!(self.reach.reach(), Reach::Unreachable { .. }) {
            !unknown.is_empty()
        } else {
            let lookups = unknown.into_iter().take(TWINS_PER_ANSWER).collect();
            self.look_up_twins(lookups, origin, generation).await
        };
        let mut albums = albums;
        for album in &mut albums {
            self.with_known_twin(album, generation);
        }
        (albums, partial)
    }

    /// The Deezer twin `apple` is answered from: the verdict known, else, for an album whose
    /// artwork names barcodes, looked up now. Also whether the answer is partial: the lookup went
    /// unanswered, or Deezer is unreachable so a twin cannot be asked or used.
    async fn twin_of(
        &self,
        album: Option<&Value>,
        apple: u64,
        origin: &str,
        generation: u64,
    ) -> (Option<String>, bool) {
        let known = self.twins.get(generation, apple);
        let unasked = known.is_none() && album.is_some_and(has_barcodes);
        if matches!(self.reach.reach(), Reach::Unreachable { .. }) {
            return (None, unasked || matches!(known, Some(Twin::Found { .. })));
        }
        match (known, album) {
            (Some(Twin::Found { deezer_id, .. }), _) => (Some(deezer_id), false),
            (None, Some(album)) if unasked => {
                let partial = self
                    .look_up_twins(vec![(apple, album.clone())], origin, generation)
                    .await;
                match self.twins.get(generation, apple) {
                    Some(Twin::Found { deezer_id, .. }) => (Some(deezer_id), false),
                    _ => (None, partial),
                }
            }
            _ => (None, false),
        }
    }

    /// Looks the twins of `albums` up on Deezer's interactive lane, each in a task of its own that
    /// records its verdict whenever it finishes; waits [`TWIN_WAIT`] at most. True when a lookup
    /// went unanswered or is still running.
    async fn look_up_twins(&self, albums: Vec<(u64, Value)>, origin: &str, generation: u64) -> bool {
        if albums.is_empty() {
            return false;
        }
        let door = self.door.get().cloned();
        let lookups: Vec<_> = albums
            .into_iter()
            .map(|(apple, album)| {
                let (door, origin, twins) = (door.clone(), origin.to_string(), Arc::clone(&self.twins));
                tokio::spawn(async move {
                    match look_up_twin(door, &album, &origin).await {
                        Some(twin) => {
                            twins.record(generation, apple, twin);
                            true
                        }
                        None => false,
                    }
                })
            })
            .collect();
        match tokio::time::timeout(TWIN_WAIT, futures::future::join_all(lookups)).await {
            Ok(done) => done.iter().any(|done| !matches!(done, Ok(true))),
            Err(_) => true,
        }
    }

    /// `album` with its known Deezer twin's id and barcode; its title and artist stay Apple's, as
    /// people saw them.
    fn with_known_twin(&self, album: &mut Value, generation: u64) {
        let Some(apple) = album_apple_id(album) else {
            return;
        };
        if let Some(Twin::Found {
            deezer_id,
            album: twin,
        }) = self.twins.get(generation, apple)
            && let Ok(id) = deezer_id.parse::<i64>()
            && let Value::Object(map) = album
        {
            map.insert("id".into(), id.into());
            if let Some(upc) = twin.get("upc") {
                map.insert("upc".into(), upc.clone());
            }
        }
    }

    /// A track's album id swapped for its Deezer twin's when one is known.
    fn with_known_twin_album(&self, track: &mut Value, generation: u64) {
        let Some(apple) = track.get("album").and_then(album_apple_id) else {
            return;
        };
        if let Some(Twin::Found { deezer_id, .. }) = self.twins.get(generation, apple)
            && let Some(Value::Object(album)) = track.get_mut("album")
            && let Ok(id) = deezer_id.parse::<i64>()
        {
            album.insert("id".into(), id.into());
        }
    }

    // ---- artists ---------------------------------------------------------------------------

    /// The Apple artist of an artist page: Apple's own id, or for a Deezer artist the Apple
    /// artist of the same name (the one whose album count is closest when several share it).
    /// Outer None: a question went unanswered. Inner None: no Apple artist is this one.
    async fn apple_artist(
        &self,
        artist: &ArtistRef,
        origin: &str,
        purpose: AskFor,
        generation: u64,
    ) -> Option<Option<u64>> {
        let deezer_id = match artist {
            ArtistRef::Apple(apple) => return Some(Some(*apple)),
            ArtistRef::Deezer(id) => id,
        };
        let mapped = key_text(generation, "d2a", deezer_id);
        if let Some(apple) = self.artist_ids.get(&mapped) {
            return Some(apple.parse().ok());
        }
        if matches!(self.reach.reach(), Reach::Unreachable { .. }) {
            return None;
        }
        let deezer = self.deezer_artist(deezer_id, origin, purpose, generation).await?;
        let Some(deezer) = deezer else {
            return Some(None);
        };
        let candidates = self.exact_apple_artists(&deezer.name, purpose.class).await?;
        let chosen = match candidates.as_slice() {
            [] => None,
            [only] => Some(*only),
            several => {
                let mut best: Option<(u64, i64)> = None;
                for candidate in several {
                    let count = i64::try_from(self.artist_album_list(*candidate, purpose.class).await?.len())
                        .unwrap_or(i64::MAX);
                    let distance = (count - deezer.albums).abs();
                    if best.is_none_or(|(_, best)| distance < best) {
                        best = Some((*candidate, distance));
                    }
                }
                best.map(|(candidate, _)| candidate)
            }
        };
        if let Some(apple) = chosen {
            self.artist_ids.set(mapped, apple.to_string(), 1, KEPT_FOR);
            self.artist_ids
                .set(key(generation, "a2d", apple), deezer_id.clone(), 1, KEPT_FOR);
        }
        Some(chosen)
    }

    /// The Deezer artist of an Apple artist, for its picture and related artists (only while
    /// Deezer serves its artist search here, so not while it is unreachable).
    async fn deezer_twin_artist(
        &self,
        apple: u64,
        apple_object: &Value,
        origin: &str,
        purpose: AskFor,
        generation: u64,
    ) -> Option<DeezerArtist> {
        if matches!(self.reach.reach(), Reach::Unreachable { .. }) {
            return None;
        }
        if let Some(deezer_id) = self.artist_ids.get(&key(generation, "a2d", apple)) {
            return self
                .deezer_artist(&deezer_id, origin, purpose, generation)
                .await
                .flatten();
        }
        let name = apple_object.get("name").and_then(Value::as_str)?;
        let url = format!(
            "{origin}/search/artist?q={}&limit={ARTIST_CANDIDATES}",
            dotnet::escape_data_string(name)
        );
        let answer = self.deezer(&url, purpose).await?;
        let doc = serde_json::from_slice::<Value>(&answer.body).ok()?;
        let named: Vec<&Value> = doc
            .get("data")
            .and_then(Value::as_array)?
            .iter()
            .filter(|hit| {
                hit.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|hit| SongIdentity::same_artist_name(hit, name))
            })
            .collect();
        let chosen = match named.as_slice() {
            [] => return None,
            [only] => (*only).clone(),
            several => {
                let apple_albums = i64::try_from(self.artist_album_list(apple, purpose.class).await?.len())
                    .unwrap_or(i64::MAX);
                (*several
                    .iter()
                    .min_by_key(|hit| {
                        (hit.get("nb_album").and_then(Value::as_i64).unwrap_or(0) - apple_albums).abs()
                    })
                    .expect("several is not empty"))
                .clone()
            }
        };
        let deezer_id = chosen.get("id").map(Value::to_string).unwrap_or_default();
        self.artist_ids
            .set(key(generation, "a2d", apple), deezer_id.clone(), 1, KEPT_FOR);
        let artist = DeezerArtist {
            name: chosen
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(name)
                .to_string(),
            albums: chosen.get("nb_album").and_then(Value::as_i64).unwrap_or(0),
            object: Arc::new(chosen),
        };
        // The search's hit is the artist: the next page of this artist asks Deezer nothing more.
        self.deezer_artists.set(
            key_text(generation, "deezer-artist", &deezer_id),
            Some(artist.clone()),
            1,
            KEPT_FOR,
        );
        Some(artist)
    }

    /// A Deezer artist by id (Deezer answers this from every country it serves or hides).
    async fn deezer_artist(
        &self,
        deezer_id: &str,
        origin: &str,
        purpose: AskFor,
        generation: u64,
    ) -> Option<Option<DeezerArtist>> {
        let kept = key_text(generation, "deezer-artist", deezer_id);
        if let Some(artist) = self.deezer_artists.get(&kept) {
            return Some(artist);
        }
        let answer = self
            .deezer(&format!("{origin}/artist/{deezer_id}"), purpose)
            .await?;
        let doc = serde_json::from_slice::<Value>(&answer.body).ok()?;
        let artist = match doc.get("name").and_then(Value::as_str) {
            Some(name) if doc.get("error").is_none() => Some(DeezerArtist {
                name: name.to_string(),
                albums: doc.get("nb_album").and_then(Value::as_i64).unwrap_or(0),
                object: Arc::new(doc.clone()),
            }),
            _ => None,
        };
        self.deezer_artists.set(kept, artist.clone(), 1, KEPT_FOR);
        Some(artist)
    }

    /// Apple's artists of exactly this name, in Apple's order.
    async fn exact_apple_artists(&self, name: &str, class: AppleClass) -> Option<Vec<u64>> {
        let doc = self.asked(
            self.apple
                .search(name, Entity::MusicArtist, ARTIST_CANDIDATES, class)
                .await,
            "search",
        )?;
        let Some(doc) = doc else {
            return Some(Vec::new());
        };
        let artists = self.read(apple_as_deezer::artists(&doc), "search")?;
        Some(
            artists
                .iter()
                .filter(|artist| {
                    artist
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|candidate| SongIdentity::same_artist_name(candidate, name))
                })
                .filter_map(album_apple_id)
                .collect(),
        )
    }

    /// An Apple artist's albums, in Apple's order.
    async fn artist_album_list(&self, apple: u64, class: AppleClass) -> Option<Vec<Value>> {
        let doc = self.asked(
            self.apple
                .lookup(&[apple], Some(Entity::Album), ARTIST_ALBUMS, class)
                .await,
            "lookup",
        )?;
        match doc {
            Some(doc) => self.read(apple_as_deezer::albums(&doc), "lookup"),
            None => Some(Vec::new()),
        }
    }

    // ---- plumbing --------------------------------------------------------------------------

    /// Apple's answer: Some(Some(doc)), Some(None) for "nothing", None when Apple did not answer.
    fn asked(&self, asked: AppleAsked<Arc<Value>>, _kind: &'static str) -> Option<Option<Arc<Value>>> {
        match asked {
            AppleAsked::Answer(doc) => Some(Some(doc)),
            AppleAsked::NoData => Some(None),
            AppleAsked::Busy => None,
        }
    }

    /// A translation, or None (warned once) when Apple's answer could not be read.
    fn read<T>(&self, read: AppleRead<T>, kind: &'static str) -> Option<T> {
        match read {
            AppleRead::Read(items) => Some(items),
            AppleRead::Unreadable => {
                self.apple.answer_unreadable(kind);
                None
            }
        }
    }

    /// Keeps an object handed out, by its Apple id, and leaves its cover as a hint.
    fn seed(&self, kind: &str, object: &Value, generation: u64) {
        self.hint(object);
        if let Some(apple) = album_apple_id(object) {
            self.seeds.set(
                key(generation, kind, apple),
                Arc::new(object.clone()),
                1,
                KEPT_FOR,
            );
        }
    }

    /// Leaves the Apple cover of a song (and its album) or an album handed out as an artwork hint
    /// by artist and title, so finding that cover asks Apple nothing.
    fn hint(&self, object: &Value) {
        let Some(hints) = self.hints.get() else {
            return;
        };
        let text =
            |value: &Value, field: &str| value.get(field).and_then(Value::as_str).unwrap_or("").to_string();
        let artist = object
            .get("artist")
            .map(|artist| text(artist, "name"))
            .unwrap_or_default();
        let title = text(object, "title");
        match object.get("type").and_then(Value::as_str) {
            Some("track") => {
                let Some(album) = object.get("album") else {
                    return;
                };
                let cover = text(album, "cover_xl");
                hints.remember(RoutingKind::Song, &artist, &title, &cover);
                hints.remember(RoutingKind::Album, &artist, &text(album, "title"), &cover);
            }
            Some("album") => hints.remember(RoutingKind::Album, &artist, &title, &text(object, "cover_xl")),
            _ => {}
        }
    }

    /// One of the stand-in's own Deezer questions, through the door; None when Deezer did not
    /// answer with a 200.
    async fn deezer(&self, url: &str, purpose: AskFor) -> Option<HttpAnswer> {
        deezer_get(self.door.get().cloned(), url.to_string(), purpose).await
    }

    /// A Deezer document that is not Deezer's error, or None.
    async fn deezer_doc(&self, url: &str, purpose: AskFor) -> Option<Value> {
        let answer = self.deezer(url, purpose).await?;
        serde_json::from_slice::<Value>(&answer.body)
            .ok()
            .filter(|doc| doc.get("error").is_none())
    }
}

/// A GET through the door, None when Deezer did not answer with a 200. Boxed: the door's own
/// answer can come back here.
fn deezer_get(
    door: Option<Weak<DeezerRateLimitHandler>>,
    url: String,
    purpose: AskFor,
) -> BoxFuture<'static, Option<HttpAnswer>> {
    async move {
        let door = door?.upgrade()?;
        let answer = door.get_for(&url, purpose, None).await.ok()?;
        answer.is_success().then_some(answer)
    }
    .boxed()
}

/// The Deezer twin of one Apple album, by the barcodes its artwork names, asked on Deezer's
/// interactive lane: Some(verdict) when Deezer answered for every form asked, None when one went
/// unanswered.
async fn look_up_twin(
    door: Option<Weak<DeezerRateLimitHandler>>,
    album: &Value,
    origin: &str,
) -> Option<Twin> {
    let art = album.get("cover_xl").and_then(Value::as_str).unwrap_or("");
    for form in apple_as_deezer::artwork_barcodes(art) {
        let answer = deezer_get(door.clone(), format!("{origin}/album/upc:{form}"), AskFor::row()).await?;
        let doc = serde_json::from_slice::<Value>(&answer.body).ok()?;
        if doc.get("error").is_some() {
            // Deezer's "no such album": try the next form.
            continue;
        }
        if !is_same_record(album, &doc) {
            return Some(Twin::None);
        }
        let deezer_id = doc.get("id").map(Value::to_string).unwrap_or_default();
        // A list carries the album, not its tracklist.
        let mut doc = doc;
        if let Value::Object(map) = &mut doc {
            map.remove("tracks");
        }
        let apple_id = album.get("id").map(Value::to_string).unwrap_or_default();
        debug!("Apple album {apple_id} is Deezer album {deezer_id}");
        return Some(Twin::Found {
            deezer_id,
            album: Arc::new(doc),
        });
    }
    Some(Twin::None)
}

/// Whether an Apple album's artwork names barcodes to look its twin up by.
fn has_barcodes(album: &Value) -> bool {
    album
        .get("cover_xl")
        .and_then(Value::as_str)
        .is_some_and(|art| !apple_as_deezer::artwork_barcodes(art).is_empty())
}

/// A document answered under another id (Deezer's error left as it is).
fn with_id(mut doc: Value, id: &str) -> Value {
    if let Value::Object(map) = &mut doc
        && map.get("error").is_none()
        && let Ok(id) = id.parse::<i64>()
    {
        map.insert("id".into(), id.into());
    }
    doc
}

/// A Deezer album with the Apple album's title and artist name, as people saw them.
fn with_apple_names(doc: &mut Value, apple: &Value) {
    if let Some(title) = apple.get("title").filter(|title| title.is_string())
        && let Value::Object(map) = &mut *doc
    {
        map.insert("title".into(), title.clone());
    }
    if let Some(name) = apple.pointer("/artist/name").filter(|name| name.is_string())
        && let Some(Value::Object(artist)) = doc.get_mut("artist")
    {
        artist.insert("name".into(), name.clone());
    }
}

/// An answer the reader keeps only briefly: something in it may change soon.
fn partial_answer(mut doc: Value) -> Value {
    if let Value::Object(map) = &mut doc {
        map.insert("octo_partial".into(), true.into());
    }
    doc
}

/// Reads Deezer's `/infos` through the door, on the lane the reach asks for.
pub struct DoorInfos {
    door: Weak<DeezerRateLimitHandler>,
    origin: String,
}

impl DoorInfos {
    pub fn new(door: Weak<DeezerRateLimitHandler>, origin: &str) -> Self {
        Self {
            door,
            origin: origin.trim_end_matches('/').to_string(),
        }
    }
}

#[async_trait::async_trait]
impl InfosReader for DoorInfos {
    async fn read(&self, background: bool) -> Infos {
        let Some(door) = self.door.upgrade() else {
            return Infos::Silent;
        };
        let url = format!("{}/infos", self.origin);
        match door.get_with_outcome(&url, AskFor::lane(background)).await {
            (_, Outcome::NotSent) => Infos::NotSent,
            (Ok(answer), _) if answer.is_success() => serde_json::from_slice::<Value>(&answer.body)
                .map(|doc| Infos::read(&doc))
                .unwrap_or(Infos::Silent),
            _ => Infos::Silent,
        }
    }
}

/// Whether a Deezer album is the Apple album's record: the same base title, track count and
/// explicit flag.
fn is_same_record(apple: &Value, deezer: &Value) -> bool {
    let title = |album: &Value| AlbumOwnership::base_key(album.get("title").and_then(Value::as_str));
    let count = |album: &Value| album.get("nb_tracks").and_then(Value::as_i64);
    let explicit = |album: &Value| {
        album
            .get("explicit_lyrics")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    let apple_title = title(apple);
    !apple_title.is_empty()
        && apple_title == title(deezer)
        && count(apple).is_some()
        && count(apple) == count(deezer)
        && explicit(apple) == explicit(deezer)
}

/// The Apple id an object's `id` carries, None for a Deezer id.
fn album_apple_id(object: &Value) -> Option<u64> {
    let id = object.get("id")?;
    let text = match id {
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        _ => return None,
    };
    apple_id(&text)
}

/// `items[index..index + limit]` as a Deezer list answer, with how many there are in all.
fn slice(items: Vec<Value>, index: u32, limit: u32) -> Value {
    let total = items.len();
    let shown = items
        .into_iter()
        .skip(index as usize)
        .take(limit.max(1) as usize)
        .collect();
    apple_as_deezer::list_answer(shown, total)
}

fn key(generation: u64, kind: &str, id: u64) -> String {
    format!("g{generation}|{kind}|{id}")
}

fn key_text(generation: u64, kind: &str, id: &str) -> String {
    format!("g{generation}|{kind}|{id}")
}

#[cfg(test)]
#[path = "deezer_stand_in_tests.rs"]
mod tests;
