//! The catalog side of New Releases against a stand-in Deezer, and the whole refresh against a
//! stand-in Navidrome and Deezer together. The picking is tested in
//! `octo_core::library::for_you_lists`; the endpoints in `controllers::subsonic::for_you_tests`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use chrono::{TimeDelta, Utc};
use indexmap::IndexMap;
use octo_core::json::dom::Node;
use octo_core::library::for_you_lists::{self as lists, TopArtist};
use octo_core::library::generated_playlist_service::str_of;
use octo_core::settings::{
    AppSettings, ExplicitFilter, GeneratedPlaylistSettings, SettingsStore, SubsonicSettings,
};
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use octo_subsonic::subsonic_response_builder::SubsonicResponseBuilder;
use parking_lot::Mutex;
use tempfile::TempDir;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::services::library::generated_playlist_service::{
    ForYouServices, GeneratedPlaylist, GeneratedPlaylistService,
};
use crate::services::metadata::{DeezerMetadataService, DeezerRateLimitHandler, DeezerRateLimiter};
use crate::services::soulseek::ExternalIdRegistry;
use crate::services::subsonic::{SubsonicProxyService, SubsonicResponseBuilderExt};

/// A stand-in Deezer (under `/dz`) and Navidrome (under `/nd`) in one: an artist with releases
/// old, new and still to come, and a library whose listener plays that artist most.
pub(crate) struct ForYouUpstream {
    pub calls: Mutex<Vec<String>>,
    pub catalog_down: AtomicBool,
    /// While true, every catalog call waits for it to turn false.
    hold: watch::Sender<bool>,
    /// The artist has more than a page of releases, the newest single on the second.
    pub long_career: AtomicBool,
    /// A library of more than a page whose second page Navidrome fails to send.
    pub walk_breaks: AtomicBool,
    /// Every listener's hearts, newest last: who, the song id, and when it was hearted.
    pub hearts: Mutex<Vec<(String, String, String)>>,
    /// The listener's ratings, by song id.
    pub ratings: Mutex<HashMap<String, i64>>,
    address: Mutex<String>,
}

pub(crate) fn day(days_ago: i64) -> String {
    (Utc::now() - TimeDelta::days(days_ago))
        .format("%Y-%m-%d")
        .to_string()
}

impl ForYouUpstream {
    pub async fn start() -> Arc<ForYouUpstream> {
        let (hold, _) = watch::channel(false);
        let upstream = Arc::new(ForYouUpstream {
            calls: Mutex::new(Vec::new()),
            catalog_down: AtomicBool::new(false),
            hold,
            long_career: AtomicBool::new(false),
            walk_breaks: AtomicBool::new(false),
            hearts: Mutex::new(Vec::new()),
            ratings: Mutex::new(HashMap::new()),
            address: Mutex::new(String::new()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a local port");
        *upstream.address.lock() = format!("http://{}", listener.local_addr().expect("an address"));
        let app = Router::new().fallback(answer).with_state(Arc::clone(&upstream));
        tokio::spawn(async move { axum::serve(listener, app).await });
        upstream
    }

    pub fn navidrome_url(&self) -> String {
        format!("{}/nd", self.address.lock())
    }

    pub fn deezer_url(&self) -> String {
        format!("{}/dz", self.address.lock())
    }

    pub fn hold_catalog(&self, held: bool) {
        self.hold.send_replace(held);
    }

    pub fn calls_ending(&self, suffix: &str) -> usize {
        self.calls
            .lock()
            .iter()
            .filter(|call| call.ends_with(suffix))
            .count()
    }

    /// The catalog this upstream answers as.
    pub fn deezer(&self) -> Arc<DeezerMetadataService> {
        Arc::new(DeezerMetadataService::with_base_url(
            Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new()))),
            Arc::new(SettingsStore::for_tests(AppSettings::default())),
            self.deezer_url(),
        ))
    }
}

fn json_body(body: String) -> Response {
    ([("content-type", "application/json")], body).into_response()
}

/// An empty Subsonic answer, in the format asked for, holding the element named (if any).
fn plain(query: &HashMap<String, String>, element: &str) -> Response {
    if query.get("f").map(String::as_str) == Some("json") {
        let inner = if element.is_empty() {
            String::new()
        } else {
            format!(r#","{element}":{{}}"#)
        };
        return json_body(format!(
            r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1"{inner}}}}}"#
        ));
    }
    let inner = if element.is_empty() {
        String::new()
    } else {
        format!("<{element}/>")
    };
    (
        [("content-type", "application/xml")],
        format!(r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1">{inner}</subsonic-response>"#),
    )
        .into_response()
}

fn album(id: i32, title: &str, kind: &str, date: &str, tracks: i32, cover: Option<&str>) -> String {
    let cover = cover
        .map(|cover| format!(r#","cover_xl":"{cover}""#))
        .unwrap_or_default();
    format!(
        r#"{{"id":{id},"title":"{title}","record_type":"{kind}","release_date":"{date}","nb_tracks":{tracks}{cover}}}"#
    )
}

fn album_track(title: &str, duration: i32, position: i32, rank: i32, artist: &str, explicit: i32) -> String {
    format!(
        r#"{{"title":"{title}","duration":{duration},"track_position":{position},"disk_number":1,"rank":{rank},"explicit_content_lyrics":{explicit},"artist":{{"name":"{artist}"}}}}"#
    )
}

fn navidrome_songs(upstream: &ForYouUpstream, query: &HashMap<String, String>) -> String {
    if query.get("songOffset").map(String::as_str) != Some("0") {
        return r#"{"subsonic-response":{"status":"ok","version":"1.16.1","searchResult3":{"song":[]}}}"#
            .to_string();
    }
    let old = (Utc::now() - TimeDelta::days(365)).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let mut songs: Vec<String> = Vec::new();
    // A full first page, so the walk asks for a second.
    if upstream.walk_breaks.load(Ordering::SeqCst) {
        for i in 0..500 {
            songs.push(format!(
                r#"{{"id":"filler{i}","title":"Filler {i}","artist":"Filler","artistId":"arFiller","duration":200,"playCount":9}}"#
            ));
        }
    }
    for i in 0..12 {
        songs.push(format!(
            r#"{{"id":"big{i}","title":"Big {i}","artist":"Big Artist","artistId":"arBig","duration":200,"playCount":{},"played":"{old}"}}"#,
            if i < 6 { 8 } else { 0 }
        ));
    }
    songs.push(r#"{"id":"owned","title":"Owned Song","artist":"Big Artist","artistId":"arBig","duration":210,"playCount":0}"#.into());
    songs.push(r#"{"id":"small","title":"Small","artist":"Small Artist","artistId":"arSmall","duration":200,"playCount":1}"#.into());
    songs.push(r#"{"id":"never","title":"Never","artist":"Never Played","artistId":"arNever","duration":200,"playCount":0}"#.into());
    format!(
        r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","searchResult3":{{"song":[{}]}}}}}}"#,
        songs.join(",")
    )
}

/// getStarred2 as Navidrome answers it: the hearted songs, with a rating only when it is not 0.
fn starred_songs(upstream: &ForYouUpstream, user: &str) -> String {
    let ratings = upstream.ratings.lock();
    let songs: Vec<String> = upstream
        .hearts
        .lock()
        .iter()
        .filter(|(who, _, _)| who == user)
        .map(|(_, id, at)| {
            let rating = ratings
                .get(id)
                .filter(|r| **r > 0)
                .map(|r| format!(r#","userRating":{r}"#))
                .unwrap_or_default();
            format!(r#"{{"id":"{id}","title":"Song {id}","artist":"Big Artist","duration":200,"starred":"{at}"{rating}}}"#)
        })
        .collect();
    format!(
        r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","starred2":{{"song":[{}]}}}}}}"#,
        songs.join(",")
    )
}

/// Who a call signs in as: `u`, else the owner of its API key (`<name>-key`).
fn caller(query: &HashMap<String, String>) -> String {
    match query.get("u").filter(|u| !u.is_empty()) {
        Some(user) => user.clone(),
        None => query
            .get("apiKey")
            .and_then(|key| key.strip_suffix("-key"))
            .unwrap_or_default()
            .to_string(),
    }
}

async fn answer(State(upstream): State<Arc<ForYouUpstream>>, uri: Uri, RawQuery(raw): RawQuery) -> Response {
    let full = uri.path().trim_matches('/').to_string();
    let (host, path) = full.split_once('/').unwrap_or((full.as_str(), ""));
    let (host, path) = (host.to_string(), path.to_string());
    let raw = raw.unwrap_or_default();
    let query: HashMap<String, String> = url::form_urlencoded::parse(raw.as_bytes()).into_owned().collect();
    let host_name = if host == "nd" { "navidrome" } else { "deezer" };
    upstream.calls.lock().push(format!("{host_name}/{path}"));
    let catalog = host != "nd";
    if catalog {
        let mut held = upstream.hold.subscribe();
        let _ = held.wait_for(|held| !*held).await;
        if upstream.catalog_down.load(Ordering::SeqCst) {
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
    }
    if !catalog
        && upstream.walk_breaks.load(Ordering::SeqCst)
        && path == "rest/search3"
        && query.get("songOffset").map(String::as_str) != Some("0")
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if !catalog {
        return match path.as_str() {
            "rest/search3" => json_body(navidrome_songs(&upstream, &query)),
            "rest/ping" | "rest/ping.view" => plain(&query, ""),
            "rest/getPlaylists" | "rest/getPlaylists.view" => plain(&query, "playlists"),
            "rest/star" | "rest/star.view" => {
                let user = caller(&query);
                let id = query.get("id").cloned().unwrap_or_default();
                let mut hearts = upstream.hearts.lock();
                if !hearts.iter().any(|(who, heart, _)| *who == user && *heart == id) {
                    let at = Utc::now() + TimeDelta::seconds(hearts.len() as i64);
                    hearts.push((user, id, at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)));
                }
                plain(&query, "")
            }
            // Navidrome refuses a bad password with HTTP 200 and a failed envelope.
            "rest/unstar" | "rest/unstar.view" if query.get("t").map(String::as_str) == Some("bad") => json_body(
                r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}"#.into(),
            ),
            "rest/tokenInfo" | "rest/tokenInfo.view" => json_body(format!(
                r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","tokenInfo":{{"username":"{}"}}}}}}"#,
                caller(&query)
            )),
            "rest/unstar" | "rest/unstar.view" => {
                let user = caller(&query);
                let id = query.get("id").cloned().unwrap_or_default();
                upstream
                    .hearts
                    .lock()
                    .retain(|(who, heart, _)| !(*who == user && *heart == id));
                plain(&query, "")
            }
            "rest/setRating" | "rest/setRating.view" => {
                let id = query.get("id").cloned().unwrap_or_default();
                let rating = query.get("rating").and_then(|r| r.parse().ok()).unwrap_or(0);
                upstream.ratings.lock().insert(id, rating);
                plain(&query, "")
            }
            "rest/getStarred2" | "rest/getStarred2.view" => {
                json_body(starred_songs(&upstream, &caller(&query)))
            }
            _ => json_body(r#"{"data":[]}"#.into()),
        };
    }
    let long_career = upstream.long_career.load(Ordering::SeqCst);
    let body = match path.as_str() {
        "search/artist" => {
            r#"{"data":[{"id":7,"name":"Big Artist","nb_fan":900},{"id":8,"name":"Big Artist Tribute","nb_fan":5000}]}"#
                .to_string()
        }
        "artist/7/albums" if long_career && raw.contains("index=100") => format!(
            r#"{{"data":[{}],"total":105}}"#,
            album(104, "Page Two Single", "single", &day(1), 1, Some("https://cdn.test/104.jpg"))
        ),
        "artist/7/albums" => format!(
            r#"{{"data":[{},{},{},{}],"total":{}}}"#,
            album(100, "New Album", "album", &day(10), 4, Some("https://cdn.test/100.jpg")),
            album(101, "New Single", "single", &day(3), 1, Some("https://cdn.test/101.jpg")),
            album(102, "Old Album", "album", &day(400), 9, None),
            album(103, "Coming Soon", "album", &day(-30), 9, None),
            if long_career { 105 } else { 4 }
        ),
        "album/104" => r#"{"id":104,"title":"Page Two Single","record_type":"single","nb_tracks":1,"artist":{"name":"Big Artist"}}"#.into(),
        "album/104/tracks" => format!(
            r#"{{"data":[{}],"total":1}}"#,
            album_track("Page Two Song", 180, 1, 300, "Big Artist", 0)
        ),
        "album/100" => r#"{"id":100,"title":"New Album","record_type":"album","nb_tracks":4,"artist":{"name":"Big Artist"}}"#.into(),
        "album/100/tracks" => format!(
            r#"{{"data":[{},{},{},{}],"total":4}}"#,
            album_track("Opener", 180, 1, 100, "Big Artist", 0),
            album_track("Hit", 200, 2, 900, "Big Artist", 1),
            album_track("Owned Song", 210, 3, 800, "Big Artist", 1),
            album_track("Closer", 240, 4, 700, "Big Artist", 0)
        ),
        "album/101" => r#"{"id":101,"title":"New Single","record_type":"single","nb_tracks":1,"artist":{"name":"Big Artist"}}"#.into(),
        // The single is credited to a guest, as the catalog does with shared songs.
        "album/101/tracks" => format!(
            r#"{{"data":[{}],"total":1}}"#,
            album_track("Single Song", 190, 1, 500, "Guest Star", 0)
        ),
        _ => r#"{"data":[]}"#.into(),
    };
    json_body(body)
}

fn node(text: &str) -> Node {
    Node::parse(text).expect("JSON")
}

fn title_of(entry: &Entry) -> String {
    match (&entry.outside, &entry.library) {
        (Some(song), _) => song.title.clone(),
        (None, Some(library)) => str_of(library, "title").unwrap_or_default().to_string(),
        _ => String::new(),
    }
}

fn titles(result: &BuildResult) -> Vec<String> {
    result.entries.iter().map(title_of).collect()
}

fn big_artist() -> TopArtist {
    TopArtist::new("arBig", "Big Artist", 50, 0, 13)
}

fn today() -> chrono::NaiveDate {
    Utc::now().date_naive()
}

// ---- New releases -------------------------------------------------------------------------

#[tokio::test]
async fn new_releases_are_the_artists_latest_newest_first_with_the_listeners_own_copies() {
    let upstream = ForYouUpstream::start().await;
    let registry = Arc::new(ExternalIdRegistry::new(None::<&str>));
    let builder = NewReleasesBuilder::new(upstream.deezer(), Arc::clone(&registry));
    let library = [
        node(r#"{"id":"owned","title":"Owned Song","artist":"Big Artist","playCount":0}"#),
        // Credited to a guest in the catalog, filed under the listener's own artist here.
        node(r#"{"id":"single","title":"Single Song","artist":"Big Artist","playCount":0}"#),
    ];

    let result = builder
        .build(
            &[big_artist()],
            &library,
            today(),
            8,
            &CancellationToken::new(),
            ExplicitFilter::All,
            false,
        )
        .await
        .expect("built");

    assert!(result.whole);
    // The single (3 days old) first, then the album's best three by rank, in album order.
    assert_eq!(titles(&result), ["Single Song", "Hit", "Owned Song", "Closer"]);
    assert_eq!(
        result.entries[2]
            .library
            .as_ref()
            .and_then(|song| str_of(song, "id")),
        Some("owned")
    );
    assert_eq!(
        result.entries[0]
            .library
            .as_ref()
            .and_then(|song| str_of(song, "id")),
        Some("single")
    );
    for entry in result.entries.iter().filter(|entry| entry.outside.is_some()) {
        let outside = entry.outside.as_ref().expect("an outside song");
        assert!(!outside.is_local);
        assert!(registry.lookup(&outside.id).is_some());
        assert!(["New Album", "New Single"].contains(&outside.album.as_str()));
    }
    // The tribute act has more fans, but only the artist of that very name is theirs.
    let calls = upstream.calls.lock().clone();
    assert!(!calls.iter().any(|call| call.contains("artist/8/")));
    assert!(!calls.iter().any(|call| call.contains("album/102")));
    assert!(!calls.iter().any(|call| call.contains("album/103")));
    // The catalog's artist rides on a song credited to them; a guest's song names no catalog artist.
    let hit = result
        .entries
        .iter()
        .find_map(|entry| entry.outside.as_ref().filter(|song| song.title == "Hit"))
        .expect("Hit");
    let artist = registry
        .lookup(hit.artist_id.as_deref().expect("an artist id"))
        .expect("registered");
    assert_eq!(artist.lock().external_artist_id.as_deref(), Some("7"));
    assert_eq!(hit.explicit_content_lyrics, Some(1));
}

fn make(upstream: &ForYouUpstream) -> Arc<NewReleasesBuilder> {
    Arc::new(NewReleasesBuilder::new(
        upstream.deezer(),
        Arc::new(ExternalIdRegistry::new(None::<&str>)),
    ))
}

#[tokio::test]
async fn a_clean_only_server_leaves_out_explicit_outside_songs_but_never_the_listeners_own() {
    let upstream = ForYouUpstream::start().await;
    let builder = make(&upstream);
    let library = [node(
        r#"{"id":"owned","title":"Owned Song","artist":"Big Artist","playCount":0}"#,
    )];

    let clean = builder
        .build(
            &[big_artist()],
            &library,
            today(),
            8,
            &CancellationToken::new(),
            ExplicitFilter::CleanOnly,
            false,
        )
        .await
        .expect("built");

    assert_eq!(titles(&clean), ["Single Song", "Owned Song", "Closer"]);
}

#[tokio::test]
async fn a_long_career_is_read_past_its_first_page_so_the_newest_single_is_found() {
    let upstream = ForYouUpstream::start().await;
    upstream.long_career.store(true, Ordering::SeqCst);
    let builder = make(&upstream);

    let result = builder
        .build(
            &[big_artist()],
            &[],
            today(),
            8,
            &CancellationToken::new(),
            ExplicitFilter::All,
            false,
        )
        .await
        .expect("built");

    assert_eq!(titles(&result)[0], "Page Two Song");
    assert!(upstream.calls_ending("artist/7/albums") > 0);
}

#[tokio::test]
async fn listeners_building_together_take_the_catalog_in_turn() {
    let upstream = ForYouUpstream::start().await;
    upstream.hold_catalog(true);
    let builder = make(&upstream);

    let first = {
        let builder = Arc::clone(&builder);
        tokio::spawn(async move {
            builder
                .build(
                    &[big_artist()],
                    &[],
                    today(),
                    8,
                    &CancellationToken::new(),
                    ExplicitFilter::All,
                    false,
                )
                .await
        })
    };
    // Another listener, another artist: a search of its own, were it allowed to ask.
    let second = {
        let builder = Arc::clone(&builder);
        tokio::spawn(async move {
            builder
                .build(
                    &[TopArtist::new("arOther", "Other Artist", 9, 0, 3)],
                    &[],
                    today(),
                    8,
                    &CancellationToken::new(),
                    ExplicitFilter::All,
                    false,
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    // Only the first listener's walk is asking the catalog; the second waits its turn.
    assert_eq!(upstream.calls_ending("search/artist"), 1);
    upstream.hold_catalog(false);

    assert!(!first.await.expect("joined").expect("built").entries.is_empty());
    assert!(second.await.expect("joined").expect("built").entries.is_empty());
    assert_eq!(upstream.calls_ending("search/artist"), 2);
}

// ---- The Made for you lists ---------------------------------------------------------------

struct Built {
    service: Arc<GeneratedPlaylistService>,
    registry: Arc<ExternalIdRegistry>,
}

fn service_with(
    dir: &TempDir,
    upstream: &ForYouUpstream,
    settings: GeneratedPlaylistSettings,
    explicit_filter: ExplicitFilter,
) -> Built {
    let store = Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(upstream.navidrome_url()),
            explicit_filter,
            ..Default::default()
        },
        generated_playlists: settings,
        ..Default::default()
    }));
    let registry = Arc::new(ExternalIdRegistry::new(None::<&str>));
    let service = GeneratedPlaylistService::new(
        Some(dir.path().join("generated-playlists.json")),
        SubsonicProxyService::new(store.clone()),
        store,
    )
    .with_for_you(ForYouServices {
        builder: Arc::new(NewReleasesBuilder::new(upstream.deezer(), Arc::clone(&registry))),
        responses: Arc::new(SubsonicResponseBuilder::new(
            Arc::clone(&registry) as Arc<dyn octo_subsonic::subsonic_response_builder::IdRegistry>,
            &SubsonicSettings::default(),
        )),
    });
    Built {
        service: Arc::new(service),
        registry,
    }
}

fn service(
    dir: &TempDir,
    upstream: &ForYouUpstream,
    settings: GeneratedPlaylistSettings,
) -> Arc<GeneratedPlaylistService> {
    service_with(dir, upstream, settings, ExplicitFilter::All).service
}

fn auth() -> IndexMap<String, String> {
    [
        ("u", "alice"),
        ("t", "token"),
        ("s", "salt"),
        ("v", "1.16.1"),
        ("c", "test"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// The first list waits only a moment for a build; ask until the build has landed.
async fn listed(service: &Arc<GeneratedPlaylistService>) -> Vec<GeneratedPlaylist> {
    for _ in 0..100 {
        let lists = service.list("alice", &auth()).await;
        if !lists.is_empty() {
            return lists;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    service.list("alice", &auth()).await
}

async fn materialize(service: &GeneratedPlaylistService, list: &GeneratedPlaylist) -> Vec<Node> {
    service
        .materialize("alice", list, &auth(), &CancellationToken::new())
        .await
        .expect("materialized")
}

fn ids(songs: &[Node]) -> Vec<String> {
    songs
        .iter()
        .map(|song| str_of(song, "id").unwrap_or_default().to_string())
        .collect()
}

fn is_external(song: &Node) -> Option<bool> {
    match song.get("isExternal") {
        Some(Node::Bool(flag)) => Some(*flag),
        _ => None,
    }
}

#[tokio::test]
async fn the_three_lists_are_made_from_the_listeners_plays_newest_first_without_the_mixes_switch() {
    let dir = TempDir::new().expect("a temp dir");
    let upstream = ForYouUpstream::start().await;
    let service = service(&dir, &upstream, GeneratedPlaylistSettings::default());

    let lists = listed(&service).await;

    assert_eq!(
        lists.iter().map(|list| list.name.as_str()).collect::<Vec<_>>(),
        ["New Releases", "Rediscover", "Deep Cuts"]
    );
    assert_eq!(
        lists.iter().map(|list| list.kind.as_str()).collect::<Vec<_>>(),
        ["newReleases", "rediscover", "deepCuts"]
    );
    assert!(lists.iter().all(|list| list.id.starts_with("og")));
    let rediscover = materialize(&service, &lists[1]).await;
    assert_eq!(rediscover.len(), 3); // the six played songs, three per artist at most
    let deep_cuts = materialize(&service, &lists[2]).await;
    assert!(
        deep_cuts
            .iter()
            .all(|song| octo_core::library::generated_playlist_service::int_of(song, "playCount") <= 1)
    );
    assert!(!ids(&deep_cuts).contains(&"never".to_string()));
    let new_releases = materialize(&service, &lists[0]).await;
    assert!(ids(&new_releases).contains(&"owned".to_string()));
    assert!(
        new_releases
            .iter()
            .any(|song| str_of(song, "title") == Some("Single Song") && is_external(song) != Some(false))
    );
    assert_eq!(service.find("alice", &lists[0].id), Some(lists[0].clone()));
    assert_eq!(service.find("bob", &lists[0].id), None);
    assert_eq!(
        service.for_you_covers("alice"),
        ["https://cdn.test/101.jpg", "https://cdn.test/100.jpg"]
    );
}

#[tokio::test]
async fn the_lists_survive_a_restart_without_being_made_again() {
    let dir = TempDir::new().expect("a temp dir");
    let upstream = ForYouUpstream::start().await;
    listed(&service(&dir, &upstream, GeneratedPlaylistSettings::default())).await;
    let walked = upstream.calls_ending("rest/search3");

    let restarted = service(&dir, &upstream, GeneratedPlaylistSettings::default());
    let lists = restarted.list("alice", &auth()).await;

    assert_eq!(lists.len(), 3);
    assert_eq!(upstream.calls_ending("rest/search3"), walked);
    assert!(!materialize(&restarted, &lists[1]).await.is_empty());
}

#[tokio::test]
async fn a_list_switched_off_is_gone_at_once() {
    let dir = TempDir::new().expect("a temp dir");
    let upstream = ForYouUpstream::start().await;
    listed(&service(&dir, &upstream, GeneratedPlaylistSettings::default())).await;

    let lists = service(
        &dir,
        &upstream,
        GeneratedPlaylistSettings {
            rediscover: false,
            ..Default::default()
        },
    )
    .list("alice", &auth())
    .await;

    assert!(!lists.iter().any(|list| list.kind == lists::REDISCOVER_KIND));
}

#[tokio::test]
async fn a_catalog_that_does_not_answer_leaves_yesterdays_new_releases() {
    let dir = TempDir::new().expect("a temp dir");
    let upstream = ForYouUpstream::start().await;
    let settings = GeneratedPlaylistSettings {
        refresh_hours: 1,
        ..Default::default()
    };
    let first = listed(&service(&dir, &upstream, settings.clone())).await;
    let before = materialize(&service(&dir, &upstream, settings.clone()), &first[0])
        .await
        .len();

    upstream.catalog_down.store(true, Ordering::SeqCst);
    // Made stale by a new switch value, so the next list builds again with the catalog down.
    let again = service(
        &dir,
        &upstream,
        GeneratedPlaylistSettings {
            refresh_hours: 1,
            new_release_weeks: 9,
            ..Default::default()
        },
    );
    again.list("alice", &auth()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let lists = again.list("alice", &auth()).await;

    let new_releases = lists
        .iter()
        .find(|list| list.kind == lists::NEW_RELEASES_KIND)
        .expect("New Releases");
    assert_eq!(materialize(&again, new_releases).await.len(), before);
}

#[tokio::test]
async fn an_outside_song_pushed_out_of_octos_memory_is_made_again_with_the_same_id() {
    let dir = TempDir::new().expect("a temp dir");
    let upstream = ForYouUpstream::start().await;
    let built = service_with(
        &dir,
        &upstream,
        GeneratedPlaylistSettings::default(),
        ExplicitFilter::All,
    );
    let lists = listed(&built.service).await;
    let outside: Vec<String> = materialize(&built.service, &lists[0])
        .await
        .iter()
        .filter(|song| is_external(song) == Some(true))
        .map(|song| str_of(song, "id").unwrap_or_default().to_string())
        .collect();
    assert!(!outside.is_empty());

    // A busy day of searches: more outside songs than Octo remembers.
    for i in 0..ExternalIdRegistry::MAX_ENTRIES + 50 {
        built.registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("Flood".into()),
            title: Some(format!("Song {i}")),
            ..Default::default()
        });
    }
    assert!(outside.iter().all(|id| built.registry.lookup(id).is_none()));

    let served = materialize(&built.service, &lists[0]).await;

    assert!(outside.iter().all(|id| built.registry.lookup(id).is_some()));
    assert!(served.iter().all(|song| song.get("octoCatalog").is_none()));
}

#[tokio::test]
async fn a_walk_that_breaks_part_way_leaves_the_lists_as_they_were() {
    let dir = TempDir::new().expect("a temp dir");
    let upstream = ForYouUpstream::start().await;
    let settings = GeneratedPlaylistSettings {
        refresh_hours: 1,
        ..Default::default()
    };
    let first = listed(&service(&dir, &upstream, settings.clone())).await;
    let deep_cuts_before = ids(&materialize(&service(&dir, &upstream, settings), &first[2]).await);

    upstream.walk_breaks.store(true, Ordering::SeqCst);
    let again = service(
        &dir,
        &upstream,
        GeneratedPlaylistSettings {
            refresh_hours: 1,
            new_release_weeks: 9,
            ..Default::default()
        },
    );
    again.list("alice", &auth()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let lists = again.list("alice", &auth()).await;

    let deep_cuts = lists
        .iter()
        .find(|list| list.kind == lists::DEEP_CUTS_KIND)
        .expect("Deep Cuts");
    assert_eq!(ids(&materialize(&again, deep_cuts).await), deep_cuts_before);
}

#[tokio::test]
async fn rediscover_and_deep_cuts_show_while_new_releases_waits_on_the_catalog() {
    let dir = TempDir::new().expect("a temp dir");
    let upstream = ForYouUpstream::start().await;
    upstream.hold_catalog(true);
    let service = service(&dir, &upstream, GeneratedPlaylistSettings::default());

    let early = listed(&service).await;
    assert_eq!(
        early.iter().map(|list| list.kind.as_str()).collect::<Vec<_>>(),
        ["rediscover", "deepCuts"]
    );

    upstream.hold_catalog(false);
    let mut lists = early;
    for _ in 0..100 {
        if lists.len() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        lists = service.list("alice", &auth()).await;
    }
    assert_eq!(
        lists.iter().map(|list| list.kind.as_str()).collect::<Vec<_>>(),
        ["newReleases", "rediscover", "deepCuts"]
    );
}

#[tokio::test]
async fn the_explicit_filter_is_obeyed_and_changing_it_makes_new_releases_again() {
    let dir = TempDir::new().expect("a temp dir");
    let upstream = ForYouUpstream::start().await;
    let all = listed(&service(&dir, &upstream, GeneratedPlaylistSettings::default())).await;
    let all_service = service(&dir, &upstream, GeneratedPlaylistSettings::default());
    assert!(
        materialize(&all_service, &all[0])
            .await
            .iter()
            .any(|song| str_of(song, "title") == Some("Hit"))
    );

    let clean = service_with(
        &dir,
        &upstream,
        GeneratedPlaylistSettings::default(),
        ExplicitFilter::CleanOnly,
    )
    .service;
    clean.list("alice", &auth()).await;
    let mut songs: Vec<Node> = Vec::new();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let lists = clean.list("alice", &auth()).await;
        songs = materialize(&clean, &lists[0]).await;
        if songs.iter().all(|song| str_of(song, "title") != Some("Hit")) {
            break;
        }
    }

    assert!(!songs.iter().any(|song| str_of(song, "title") == Some("Hit")));
    assert!(
        songs
            .iter()
            .any(|song| str_of(song, "title") == Some("Owned Song"))
    );
}

#[tokio::test]
async fn all_three_off_and_mixes_off_is_nothing() {
    let dir = TempDir::new().expect("a temp dir");
    let upstream = ForYouUpstream::start().await;
    let lists = service(
        &dir,
        &upstream,
        GeneratedPlaylistSettings {
            new_releases: false,
            rediscover: false,
            deep_cuts: false,
            ..Default::default()
        },
    )
    .list("alice", &auth())
    .await;
    assert!(lists.is_empty());
}

// ---- What a client is told about the lists ------------------------------------------------

fn builder() -> SubsonicResponseBuilder {
    SubsonicResponseBuilder::new(
        Arc::new(ExternalIdRegistry::new(None::<&str>))
            as Arc<dyn octo_subsonic::subsonic_response_builder::IdRegistry>,
        &SubsonicSettings::default(),
    )
}

fn list(kind: &str, songs: i32) -> GeneratedPlaylist {
    GeneratedPlaylist {
        id: format!("og{}", "a".repeat(20)),
        key: format!("for:{kind}"),
        kind: kind.into(),
        label: kind.into(),
        name: kind.into(),
        owner: "alice".into(),
        pool_size: songs,
        period_start_utc: Utc::now(),
        period_end_utc: Utc::now() + TimeDelta::days(1),
    }
}

#[test]
fn a_made_for_you_list_counts_what_it_holds_not_the_mixes_track_count() {
    let settings = GeneratedPlaylistSettings {
        track_count: 10,
        ..Default::default()
    };
    assert_eq!(
        builder().generated_playlist_fields(&list(lists::DEEP_CUTS_KIND, 24), &settings)["songCount"],
        24
    );
    assert_eq!(
        builder().generated_playlist_fields(&list("genre", 24), &settings)["songCount"],
        10
    );
}

#[test]
fn new_releases_covers_are_fetched_only_from_the_catalogs_image_host() {
    for (url, fetched) in [
        (
            "https://cdn-images.dzcdn.net/images/cover/abc/1000x1000-000000-80-0-0.jpg",
            true,
        ),
        (
            "https://e-cdns-images.dzcdn.net/images/cover/abc/500x500.jpg",
            true,
        ),
        ("http://cdn-images.dzcdn.net/images/cover/abc.jpg", false),
        ("https://dzcdn.net.example.com/cover.jpg", false),
        // Apple's catalog, standing in where Deezer does not serve the server.
        (
            "https://is1-ssl.mzstatic.com/image/thumb/Music/v4/a/b/c/794043201943.jpg/1000x1000bb.jpg",
            true,
        ),
        ("http://is1-ssl.mzstatic.com/image/thumb/a.jpg", false),
        ("https://mzstatic.com.example.com/a.jpg", false),
        ("https://192.168.1.10/cover.jpg", false),
        ("not a url", false),
    ] {
        assert_eq!(
            crate::controllers::subsonic::media::is_catalog_image(url),
            fetched,
            "{url}"
        );
    }
}

#[test]
fn a_made_for_you_list_says_which_one_and_a_mix_does_not() {
    let builder = builder();

    let new_releases = builder.generated_playlist_fields(
        &list(lists::NEW_RELEASES_KIND, 10),
        &GeneratedPlaylistSettings::default(),
    );
    let genre = builder.generated_playlist_fields(&list("genre", 10), &GeneratedPlaylistSettings::default());

    assert_eq!(new_releases["octoList"], "newReleases");
    assert!(
        new_releases["comment"]
            .as_str()
            .expect("a comment")
            .contains("new releases")
    );
    assert!(!genre.contains_key("octoList"));
}

#[test]
fn the_lists_are_advertised() {
    assert!(
        octo_subsonic::subsonic_response_builder::OWN_EXTENSIONS
            .iter()
            .any(|(name, versions)| *name == "octoLists" && versions.contains(&1))
    );
}
