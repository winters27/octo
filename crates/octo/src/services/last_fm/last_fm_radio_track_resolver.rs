//! Resolves a Last.fm candidate locally first, then as an external placeholder. Whether a
//! library hit is the recommended recording is
//! `octo_core::last_fm::last_fm_radio_track_resolver::is_same_recording`.

use std::sync::Arc;

use std::collections::HashSet;

use indexmap::IndexMap;
use octo_core::common::dotnet;
use octo_core::json::element::{ElementResult, enumerate_array, get_int32, get_string, try_get_property};
use octo_core::models::domain::Song;
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use serde_json::Value;
use tracing::debug;

pub use octo_core::last_fm::last_fm_radio_track_resolver::is_same_recording;

use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::soulseek::{ExternalIdRegistry, SoulseekMetadataService};
use crate::services::subsonic::SubsonicProxyService;

/// What a scrobbled library song's entry says beside the song itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrobbleDetails {
    pub genres: Vec<String>,
    pub album_id: Option<String>,
    pub album_artist: Option<String>,
    pub cover_art: Option<String>,
}

#[derive(Clone)]
pub struct LastFmRadioTrackResolver {
    proxy: SubsonicProxyService,
    metadata: Arc<dyn IMusicMetadataService>,
    registry: Arc<ExternalIdRegistry>,
}

impl LastFmRadioTrackResolver {
    pub fn new(
        proxy: SubsonicProxyService,
        metadata: Arc<dyn IMusicMetadataService>,
        registry: Arc<ExternalIdRegistry>,
    ) -> Self {
        LastFmRadioTrackResolver {
            proxy,
            metadata,
            registry,
        }
    }

    /// The song a scrobbled id names: an external route as its placeholder, or the library song
    /// Navidrome describes. None when neither knows it.
    pub async fn resolve_scrobble(
        &self,
        id: &str,
        authenticated_parameters: &IndexMap<String, String>,
    ) -> Option<Song> {
        self.resolve_scrobble_with_details(id, authenticated_parameters)
            .await
            .map(|(song, _)| song)
    }

    /// [`Self::resolve_scrobble`], with what the listening log keeps beside the song: a library
    /// song's genres, album id, album artist and cover id. An outside song's cover id is its own.
    pub async fn resolve_scrobble_with_details(
        &self,
        id: &str,
        authenticated_parameters: &IndexMap<String, String>,
    ) -> Option<(Song, ScrobbleDetails)> {
        if let Some(route) = self.registry.lookup(id)
            && route.snapshot().kind == RoutingKind::Song
        {
            let route = route.snapshot();
            let details = ScrobbleDetails {
                cover_art: Some(id.to_string()),
                ..Default::default()
            };
            return Some((
                Song {
                    id: id.to_string(),
                    artist: route.artist.unwrap_or_default(),
                    title: route.title.unwrap_or_default(),
                    album: route.album.unwrap_or_default(),
                    duration: route.duration,
                    is_local: false,
                    external_provider: Some(SoulseekMetadataService::PROVIDER_NAME.to_string()),
                    external_id: Some(id.to_string()),
                    ..Default::default()
                },
                details,
            ));
        }
        let mut parameters = authenticated_parameters.clone();
        parameters.insert("id".into(), id.to_string());
        parameters.insert("f".into(), "json".into());
        let result = self.proxy.relay_safe("rest/getSong", &parameters).await?;
        if result.body.is_empty() {
            return None;
        }
        let read = || -> anyhow::Result<Option<(Song, ScrobbleDetails)>> {
            let document: Value = serde_json::from_slice(&result.body)?;
            let Some(response) = try_get_property(&document, "subsonic-response")? else {
                return Ok(None);
            };
            let Some(song) = try_get_property(response, "song")? else {
                return Ok(None);
            };
            let text = |key: &str| {
                song.get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            };
            let details = ScrobbleDetails {
                genres: crate::services::listening::genres::song_genres(song),
                album_id: text("albumId"),
                album_artist: text("displayAlbumArtist").or_else(|| text("albumArtist")),
                cover_art: text("coverArt"),
            };
            Ok(Some((
                Song {
                    id: id.to_string(),
                    artist: string(song, "artist")?,
                    title: string(song, "title")?,
                    album: string(song, "album")?,
                    genre: nullable_string(song, "genre")?,
                    duration: integer(song, "duration")?,
                    is_local: true,
                    ..Default::default()
                },
                details,
            )))
        };
        match read() {
            Ok(song) => song,
            Err(error) => {
                debug!("scrobble metadata lookup failed for {id}: {error}");
                None
            }
        }
    }

    /// The library's copy when it has the recording, else the first external placeholder.
    ///
    /// `you_tube_id`: a video a source already named for this song (YouTube Music).
    pub async fn resolve(
        &self,
        artist: &str,
        title: &str,
        duration: Option<i32>,
        authenticated_parameters: &IndexMap<String, String>,
        you_tube_id: Option<&str>,
    ) -> Option<Song> {
        if let Some(local) = self
            .try_find_local_match(artist, title, authenticated_parameters)
            .await
        {
            return Some(local);
        }
        let hit = self
            .metadata
            .search_songs_by_artist_title(artist, title, 1, duration)
            .await
            .into_iter()
            .next()?;
        // A source that already named the video saves the search later, and plays exactly what
        // it suggested.
        if let Some(video) = you_tube_id.filter(|video| !video.is_empty()) {
            self.registry.pin_video(&hit.id, video, false);
            self.registry
                .remember_pick(&hit.id, video, SoulseekRouting::VIDEO_NAMED);
        }
        Some(hit)
    }

    /// Navidrome's search for the artist and title, as the listener; the first hit that is the
    /// same recording. A library that files the artist under another name ("Ye" for "Kanye
    /// West") finds nothing under the name asked, so when that search has no match the title
    /// alone is searched once more, and the same rule judges its hits.
    pub async fn try_find_local_match(
        &self,
        artist: &str,
        title: &str,
        authenticated_parameters: &IndexMap<String, String>,
    ) -> Option<Song> {
        // Navidrome wants every word it is given, so a joined credit ("Kordhell, Scarlxrd") or a
        // video title ("... (Official Video)") would find nothing: the main artist and the core
        // title find the candidates, and is_same_recording still decides the version.
        let first = library_query(artist, title);
        if let Some(song) = self
            .local_match_for_query(&first, artist, title, authenticated_parameters)
            .await
        {
            return Some(song);
        }
        let by_title = library_title_query(artist, title)?;
        self.local_match_for_query(&by_title, artist, title, authenticated_parameters)
            .await
    }

    async fn local_match_for_query(
        &self,
        query: &str,
        artist: &str,
        title: &str,
        authenticated_parameters: &IndexMap<String, String>,
    ) -> Option<Song> {
        let mut parameters = authenticated_parameters.clone();
        parameters.insert("query".into(), query.to_string());
        parameters.insert("songCount".into(), "5".into());
        parameters.insert("albumCount".into(), "0".into());
        parameters.insert("artistCount".into(), "0".into());
        parameters.insert("f".into(), "json".into());

        let result = self.proxy.relay_safe("rest/search3", &parameters).await?;
        if result.body.is_empty() {
            return None;
        }
        let read = || -> anyhow::Result<Option<Song>> {
            let document: Value = serde_json::from_slice(&result.body)?;
            let Some(response) = try_get_property(&document, "subsonic-response")? else {
                return Ok(None);
            };
            let Some(search) = try_get_property(response, "searchResult3")? else {
                return Ok(None);
            };
            let Some(songs @ Value::Array(_)) = try_get_property(search, "song")? else {
                return Ok(None);
            };
            for song in enumerate_array(songs)? {
                let hit_artist = string(song, "artist")?;
                let hit_title = string(song, "title")?;
                let id = string(song, "id")?;
                if id.is_empty() || !is_same_recording(artist, title, &hit_artist, &hit_title) {
                    continue;
                }

                return Ok(Some(library_song(song)?));
            }
            Ok(None)
        };
        match read() {
            Ok(song) => song,
            Err(error) => {
                debug!("local radio match lookup failed for {artist} - {title}: {error}");
                None
            }
        }
    }
}

impl LastFmRadioTrackResolver {
    /// What a radio was started from in the library. Subsonic's getSimilarSongs takes a song, an
    /// album or an artist id; an album is read as its first song, an artist as the name alone.
    /// None when Navidrome knows none of them for this listener.
    pub async fn read_seed(
        &self,
        id: &str,
        authenticated_parameters: &IndexMap<String, String>,
    ) -> Option<LibrarySeed> {
        let read = |element: &Value| -> ElementResult<Option<LibrarySeed>> {
            Ok(Some(LibrarySeed {
                artist: string(element, "artist")?,
                song: Some(library_song(element)?),
            }))
        };
        if let Some(song) = self
            .read("rest/getSong", id, authenticated_parameters, "song")
            .await
        {
            return read(&song).unwrap_or_else(|error| {
                debug!("radio seed {id} could not be read: {error}");
                None
            });
        }
        if let Some(album) = self
            .read("rest/getAlbum", id, authenticated_parameters, "album")
            .await
        {
            let seed = || -> ElementResult<Option<LibrarySeed>> {
                let Some(first) = songs(&album)?.into_iter().next() else {
                    return Ok(None);
                };
                let name = string(&album, "artist")?;
                Ok(Some(LibrarySeed {
                    artist: if name.is_empty() {
                        first.artist.clone()
                    } else {
                        name
                    },
                    song: Some(first),
                }))
            };
            return seed().unwrap_or_else(|error| {
                debug!("radio seed album {id} could not be read: {error}");
                None
            });
        }
        if let Some(artist) = self
            .read("rest/getArtist", id, authenticated_parameters, "artist")
            .await
            && let Ok(name) = string(&artist, "name")
            && !name.is_empty()
        {
            return Some(LibrarySeed {
                song: None,
                artist: name,
            });
        }
        None
    }

    /// One library song as this listener's Navidrome describes it; None when they cannot see it.
    pub async fn read_library_song(
        &self,
        id: &str,
        authenticated_parameters: &IndexMap<String, String>,
    ) -> Option<Song> {
        let song = self
            .read("rest/getSong", id, authenticated_parameters, "song")
            .await?;
        library_song(&song).ok()
    }

    /// Library songs near a seed by its own tags: the rest of its album, which for a library
    /// downloaded from playlists is the playlist, and songs sharing its genre. They lead a radio
    /// Last.fm cannot place, so the radio keeps the seed's style (#78). Never the seed itself.
    pub async fn library_neighbours(
        &self,
        seed: &Song,
        count: i32,
        authenticated_parameters: &IndexMap<String, String>,
    ) -> Vec<Song> {
        let mut same_album = Vec::new();
        if let Some(album_id) = seed.album_id.as_deref().filter(|id| !id.is_empty())
            && let Some(album) = self
                .read("rest/getAlbum", album_id, authenticated_parameters, "album")
                .await
        {
            same_album = songs(&album)
                .unwrap_or_default()
                .into_iter()
                .filter(|song| song.id != seed.id)
                .collect();
            shuffle(&mut same_album);
        }

        let mut same_genre = Vec::new();
        if let Some(genre) = seed.genre.as_deref().filter(|genre| !dotnet::is_blank(genre)) {
            let mut parameters = authenticated_parameters.clone();
            parameters.shift_remove("id");
            parameters.insert("genre".into(), genre.to_string());
            parameters.insert("size".into(), count.clamp(1, 500).to_string());
            parameters.insert("f".into(), "json".into());
            if let Some(result) = self.proxy.relay_safe("rest/getRandomSongs", &parameters).await
                && !result.body.is_empty()
            {
                let read = || -> anyhow::Result<Vec<Song>> {
                    let document: Value = serde_json::from_slice(&result.body)?;
                    let Some(response) = try_get_property(&document, "subsonic-response")? else {
                        return Ok(Vec::new());
                    };
                    let Some(random) = try_get_property(response, "randomSongs")? else {
                        return Ok(Vec::new());
                    };
                    Ok(songs(random)?)
                };
                match read() {
                    Ok(found) => same_genre = found.into_iter().filter(|song| song.id != seed.id).collect(),
                    Err(error) => debug!("radio genre neighbours failed for {genre}: {error}"),
                }
            }
        }

        let mut seen = HashSet::new();
        interleave(&[same_album, same_genre])
            .into_iter()
            .filter(|song| seen.insert(song.id.clone()))
            .take(count.max(0) as usize)
            .collect()
    }

    /// The object `element` of Navidrome's answer to `endpoint` for `id`, read with the caller's
    /// credentials; None when it failed or holds none.
    async fn read(
        &self,
        endpoint: &str,
        id: &str,
        authenticated_parameters: &IndexMap<String, String>,
        element: &str,
    ) -> Option<Value> {
        let mut parameters = authenticated_parameters.clone();
        parameters.insert("id".into(), id.to_string());
        parameters.insert("f".into(), "json".into());
        let result = self.proxy.relay_safe(endpoint, &parameters).await?;
        if result.body.is_empty() {
            return None;
        }
        let read = || -> anyhow::Result<Option<Value>> {
            let document: Value = serde_json::from_slice(&result.body)?;
            let Some(response) = try_get_property(&document, "subsonic-response")? else {
                return Ok(None);
            };
            Ok(try_get_property(response, element)?
                .filter(|found| found.is_object())
                .cloned())
        };
        read().unwrap_or_else(|error| {
            debug!("{endpoint} failed for radio seed {id}: {error}");
            None
        })
    }
}

/// A radio's starting point in the library.
///
/// `song`: the song, or an album's first song; None for an artist. `artist`: whose radio it is,
/// as the library credits them.
#[derive(Debug, Clone)]
pub struct LibrarySeed {
    pub song: Option<Song>,
    pub artist: String,
}

/// One from each list in turn, keeping each list's order, until all are spent.
pub fn interleave<T: Clone>(lists: &[Vec<T>]) -> Vec<T> {
    let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
    let mut out = Vec::new();
    for index in 0..longest {
        for list in lists {
            if let Some(item) = list.get(index) {
                out.push(item.clone());
            }
        }
    }
    out
}

/// An unseeded Fisher-Yates shuffle.
fn shuffle<T>(items: &mut [T]) {
    for i in (1..items.len()).rev() {
        let j = rand::random_range(0..=i);
        items.swap(i, j);
    }
}

/// What the library is searched with for a song: the one the charts search with too.
pub use octo_core::charts::top_songs::{library_query, library_title_query};

/// The songs of an album or a random-songs answer that have an id.
fn songs(parent: &Value) -> ElementResult<Vec<Song>> {
    let Some(Value::Array(items)) = try_get_property(parent, "song")? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for song in items {
        if !string(song, "id")?.is_empty() {
            out.push(library_song(song)?);
        }
    }
    Ok(out)
}

/// A song as Navidrome describes it.
fn library_song(song: &Value) -> ElementResult<Song> {
    Ok(Song {
        id: string(song, "id")?,
        title: string(song, "title")?,
        artist: string(song, "artist")?,
        artist_id: nullable_string(song, "artistId")?,
        album: string(song, "album")?,
        album_id: nullable_string(song, "albumId")?,
        duration: integer(song, "duration")?,
        year: integer(song, "year")?,
        track: integer(song, "track")?,
        genre: nullable_string(song, "genre")?,
        suffix: nullable_string(song, "suffix")?,
        bit_rate: integer(song, "bitRate")?,
        isrcs: texts(song, "isrc")?,
        music_brainz_recording_id: nullable_string(song, "musicBrainzId")?,
        is_local: true,
        ..Default::default()
    })
}

/// `TryGetProperty(name) ? GetString() ?? "" : ""`.
fn string(element: &Value, name: &str) -> ElementResult<String> {
    Ok(nullable_string(element, name)?.unwrap_or_default())
}

fn nullable_string(element: &Value, name: &str) -> ElementResult<Option<String>> {
    match try_get_property(element, name)? {
        Some(value) => Ok(get_string(value)?.map(str::to_string)),
        None => Ok(None),
    }
}

/// A number's `GetInt32()`; anything else is null.
fn integer(element: &Value, name: &str) -> ElementResult<Option<i32>> {
    match try_get_property(element, name)? {
        Some(value @ Value::Number(_)) => Ok(Some(get_int32(value)?)),
        _ => Ok(None),
    }
}

/// A list of text as Navidrome sent it, OpenSubsonic's `isrc`; empty when absent.
fn texts(element: &Value, name: &str) -> ElementResult<Vec<String>> {
    Ok(match try_get_property(element, name)? {
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use octo_core::settings::{AppSettings, SettingsStore, SubsonicSettings};
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::services::common::test_fakes::FakeMetadata;
    use crate::services::test_support::received;

    async fn resolver(body: &str, metadata: Arc<FakeMetadata>) -> (LastFmRadioTrackResolver, MockServer) {
        let server = MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            subsonic: SubsonicSettings {
                url: Some(server.uri()),
                ..Default::default()
            },
            ..Default::default()
        }));
        let resolver = LastFmRadioTrackResolver::new(
            SubsonicProxyService::new(settings),
            metadata,
            Arc::new(ExternalIdRegistry::in_memory()),
        );
        (resolver, server)
    }

    fn auth(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn resolver_prefers_authenticated_local_match_before_external_placeholder() {
        let metadata = Arc::new(FakeMetadata::default());
        let (resolver, server) = resolver(
            r#"{"subsonic-response":{"status":"ok","searchResult3":{"song":[{"id":"local-1","artist":"The Artist","title":"The Song","album":"Album","duration":200}]}}}"#,
            metadata.clone(),
        )
        .await;
        let song = resolver
            .resolve(
                "The Artist",
                "The Song",
                Some(200),
                &auth(&[("u", "alice"), ("t", "token"), ("s", "salt")]),
                None,
            )
            .await
            .expect("a song");
        assert!(song.is_local);
        assert_eq!(song.id, "local-1");
        let requests = received(&server).await;
        assert!(requests[0].url.query().unwrap().contains("u=alice"));
        // Strict: the metadata service was never asked (its hits table is empty, and nothing
        // came from it).
        assert_eq!(song.album, "Album");
    }

    #[tokio::test]
    async fn resolver_uses_external_placeholder_only_after_local_miss() {
        let metadata = Arc::new(FakeMetadata::default());
        let expected = Song {
            id: "external-1".into(),
            artist: "A".into(),
            title: "T".into(),
            is_local: false,
            ..Default::default()
        };
        metadata
            .hits
            .lock()
            .insert(("A".into(), "T".into()), expected.clone());
        let (resolver, _server) = resolver(
            r#"{"subsonic-response":{"status":"ok","searchResult3":{"song":[]}}}"#,
            metadata,
        )
        .await;
        let song = resolver
            .resolve("A", "T", Some(180), &auth(&[("u", "alice")]), None)
            .await;
        assert_eq!(song.map(|song| song.id), Some(expected.id));
    }

    #[tokio::test]
    async fn resolver_keeps_the_video_a_source_named_without_replacing_one_already_found() {
        let metadata = Arc::new(FakeMetadata::default());
        let (resolver, _server) = resolver(
            r#"{"subsonic-response":{"status":"ok","searchResult3":{"song":[]}}}"#,
            metadata.clone(),
        )
        .await;
        let id =
            resolver
                .registry
                .register(octo_core::soulseek::soulseek_metadata_service::SoulseekRouting {
                    artist: Some("A".into()),
                    title: Some("T".into()),
                    duration: Some(180),
                    ..Default::default()
                });
        metadata.hits.lock().insert(
            ("A".into(), "T".into()),
            Song {
                id: id.clone(),
                artist: "A".into(),
                title: "T".into(),
                is_local: false,
                ..Default::default()
            },
        );
        let video = |resolver: &LastFmRadioTrackResolver| {
            resolver
                .registry
                .lookup(&id)
                .expect("known")
                .lock()
                .you_tube_id
                .clone()
        };

        resolver
            .resolve("A", "T", Some(180), &IndexMap::new(), Some("video-1"))
            .await;
        assert_eq!(video(&resolver).as_deref(), Some("video-1"));

        resolver
            .resolve("A", "T", Some(180), &IndexMap::new(), Some("video-2"))
            .await;
        assert_eq!(video(&resolver).as_deref(), Some("video-1"));
    }

    /// The library is searched with the main artist and the core title, a seed is a song, an
    /// album's first song or an artist's name, and the neighbors are the album's other songs and
    /// the genre's, never the seed.
    #[tokio::test]
    async fn the_library_is_searched_plainly_and_seeds_are_songs_albums_or_artists() {
        assert_eq!(
            library_query("Kordhell, Scarlxrd", "Murder In My Mind (Official Video)"),
            "Kordhell Murder In My Mind"
        );
        assert_eq!(library_query("A feat. B", ""), "A");

        let server = MockServer::start().await;
        let answer = |path: &str, id: &str| -> String {
            let body = match (path, id) {
                ("/rest/getAlbum", "al-1") => {
                    r#""album":{"id":"al-1","artist":"Album Artist","song":[{"id":"s1","title":"One","artist":"X","albumId":"al-1","genre":"Phonk"},{"id":"s2","title":"Two","artist":"X","albumId":"al-1","duration":200.5}]}"#
                }
                ("/rest/getAlbum", "al-2") => {
                    r#""album":{"id":"al-2","song":[{"id":"s1","title":"One","artist":"X","albumId":"al-2","genre":"Phonk"},{"id":"s3","title":"Three","artist":"X"}]}"#
                }
                ("/rest/getArtist", "ar-1") => r#""artist":{"id":"ar-1","name":"Massive Attack"}"#,
                ("/rest/getRandomSongs", _) => {
                    r#""randomSongs":{"song":[{"id":"g1","title":"G","artist":"Y"},{"id":"s1","title":"One","artist":"X"}]}"#
                }
                _ => r#""error":{"code":70}"#,
            };
            format!(r#"{{"subsonic-response":{{"status":"ok",{body}}}}}"#)
        };
        Mock::given(any())
            .respond_with(move |request: &wiremock::Request| {
                let id = request
                    .url
                    .query_pairs()
                    .find(|(key, _)| key == "id")
                    .map(|(_, value)| value.into_owned())
                    .unwrap_or_default();
                ResponseTemplate::new(200).set_body_string(answer(request.url.path(), &id))
            })
            .mount(&server)
            .await;
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            subsonic: SubsonicSettings {
                url: Some(server.uri()),
                ..Default::default()
            },
            ..Default::default()
        }));
        let resolver = LastFmRadioTrackResolver::new(
            SubsonicProxyService::new(settings),
            Arc::new(FakeMetadata::default()),
            Arc::new(ExternalIdRegistry::in_memory()),
        );
        let no_auth = IndexMap::new();
        // A fractional length fails the song's read: the album reads as no seed.
        assert!(resolver.read_seed("al-1", &no_auth).await.is_none());
        let album = resolver.read_seed("al-2", &no_auth).await.expect("an album seed");
        assert_eq!(album.artist, "X");
        let first = album.song.expect("its first song");
        assert_eq!(first.id, "s1");
        let artist = resolver
            .read_seed("ar-1", &no_auth)
            .await
            .expect("an artist seed");
        assert!(artist.song.is_none());
        assert_eq!(artist.artist, "Massive Attack");
        assert!(resolver.read_seed("nothing", &no_auth).await.is_none());

        let neighbours = resolver.library_neighbours(&first, 10, &no_auth).await;
        let ids: Vec<&str> = neighbours.iter().map(|song| song.id.as_str()).collect();
        assert_eq!(ids, ["s3", "g1"]);
    }

    /// A hit that is another recording is passed over, an answer that is not the shape read gives
    /// no match, and a scrobbled external id is its route.
    #[tokio::test]
    async fn other_recordings_and_odd_answers_are_no_match() {
        let (matching, _server) = resolver(
            r#"{"subsonic-response":{"status":"ok","searchResult3":{"song":[{"id":"x","artist":"Airbourne","title":"Sexy Boy"},{"id":"y","artist":"Air","title":"Sexy Boy","duration":"long"}]}}}"#,
            Arc::new(FakeMetadata::default()),
        )
        .await;
        assert!(
            matching
                .try_find_local_match("Air", "Sexy Boy", &IndexMap::new())
                .await
                .is_some()
        );
        let (odd, _server) = resolver(
            r#"{"subsonic-response":{"status":"ok","searchResult3":{"song":[{"id":"y","artist":"Air","title":"Sexy Boy","duration":200.5}]}}}"#,
            Arc::new(FakeMetadata::default()),
        )
        .await;
        assert!(
            odd.try_find_local_match("Air", "Sexy Boy", &IndexMap::new())
                .await
                .is_none()
        );

        let id =
            matching
                .registry
                .register(octo_core::soulseek::soulseek_metadata_service::SoulseekRouting {
                    artist: Some("A".into()),
                    title: Some("T".into()),
                    ..Default::default()
                });
        let song = matching.resolve_scrobble(&id, &IndexMap::new()).await.unwrap();
        assert!(!song.is_local);
        assert_eq!(song.external_provider.as_deref(), Some("soulseek"));
        let song = matching.resolve_scrobble("navidrome-id", &IndexMap::new()).await;
        assert!(song.is_none());
    }
}
