//! "Can Last.fm answer at all" and "is the radio feature switched on" are different
//! questions. They used to be one property, so turning radio off also emptied the search
//! bar of discovery results: a setting doing something its name does not say.

use std::sync::Arc;
use std::time::Duration;

use octo_core::settings::{AppSettings, LastFmSettings, MetadataSettings, SettingsStore};
use parking_lot::Mutex;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::services::test_support::received;

fn store(api_key: &str, enable_radio: bool, language: &str) -> Arc<SettingsStore> {
    Arc::new(SettingsStore::for_tests(AppSettings {
        last_fm: LastFmSettings {
            api_key: api_key.to_string(),
            enable_radio,
            radio_cache_duration_hours: 2,
            ..Default::default()
        },
        metadata: MetadataSettings {
            language: language.to_string(),
            ..Default::default()
        },
        ..Default::default()
    }))
}

fn with(api_key: &str, enable_radio: bool) -> LastFmService {
    LastFmService::with_base_url(store(api_key, enable_radio, "en"), "http://127.0.0.1:1/2.0/")
}

/// A Last.fm fake: answers each request from a function of it, with one status.
struct Fixture {
    answer: Box<dyn Fn(&Request) -> String + Send + Sync>,
    status: u16,
    delay: Duration,
}

impl Respond for Fixture {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        ResponseTemplate::new(self.status)
            .set_body_string((self.answer)(request))
            .set_delay(self.delay)
    }
}

fn query(request: &Request, name: &str) -> String {
    request
        .url
        .query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default()
}

async fn serve(fixture: Fixture) -> MockServer {
    let server = MockServer::builder().start().await;
    Mock::given(any()).respond_with(fixture).mount(&server).await;
    server
}

fn answer(f: impl Fn(&Request) -> String + Send + Sync + 'static) -> Fixture {
    Fixture {
        answer: Box::new(f),
        status: 200,
        delay: Duration::ZERO,
    }
}

fn service(server: &MockServer) -> LastFmService {
    LastFmService::with_base_url(store("key", false, "en"), &format!("{}/2.0/", server.uri()))
}

#[test]
fn has_api_key_depends_only_on_the_key() {
    for (key, radio, expected) in [
        ("", true, false),
        ("", false, false),
        ("abc123", true, true),
        ("abc123", false, true),
    ] {
        // Search discovery gates on this, so EnableRadio must not appear in it.
        assert_eq!(with(key, radio).has_api_key(), expected, "{key:?} {radio}");
    }
}

#[test]
fn is_radio_enabled_needs_both_the_key_and_the_switch() {
    for (key, radio, expected) in [
        ("abc123", true, true),
        ("abc123", false, false),
        ("", true, false),
    ] {
        assert_eq!(with(key, radio).is_radio_enabled(), expected, "{key:?} {radio}");
    }
}

#[test]
fn radio_off_still_leaves_search_discovery_available() {
    // The regression this pair exists to prevent.
    let svc = with("abc123", false);

    assert!(svc.has_api_key());
    assert!(!svc.is_radio_enabled());
}

#[tokio::test]
async fn construction_applies_metadata_language_to_the_client() {
    let server = serve(answer(|_| "{}".to_string())).await;
    let svc = LastFmService::with_base_url(store("abc123", true, "en"), &format!("{}/2.0/", server.uri()));

    svc.get_artist_top_tags("A", 10).await.expect("an answer");

    let requests = received(&server).await;
    let request = requests.first().expect("one request");
    assert_eq!(
        request
            .headers
            .get("accept-language")
            .and_then(|v| v.to_str().ok()),
        Some("en")
    );
}

#[tokio::test]
async fn radio_methods_parse_provider_shapes_and_cache_identical_lookups() {
    let server = serve(answer(|request| {
        match query(request, "method").as_str() {
            "artist.getsimilar" => r#"{"similarartists":{"artist":[{"name":"Muse","match":"0.9"}]}}"#,
            "artist.gettoptags" | "track.gettoptags" => r#"{"toptags":{"tag":[{"name":"alternative rock"}]}}"#,
            "tag.gettoptracks" => {
                r#"{"tracks":{"track":[{"name":"Song","duration":"180000","artist":{"name":"Artist"}}]}}"#
            }
            "track.getInfo" => {
                r#"{"track":{"name":"Song","duration":"180000","artist":{"name":"Artist"},"album":{"title":"Album"},"toptags":{"tag":[{"name":"rock"}]}}}"#
            }
            _ => "{}",
        }
        .to_string()
    }))
    .await;
    let service = service(&server);

    let artists = service
        .get_similar_artists("Radiohead", 20)
        .await
        .expect("artists");
    assert_eq!(artists.len(), 1);
    assert_eq!(artists[0].name, "Muse");
    assert_eq!(
        service.get_artist_top_tags("Radiohead", 10).await.expect("tags"),
        ["alternative rock"]
    );
    assert_eq!(
        service.get_track_top_tags("A", "T", 10).await.expect("tags"),
        ["alternative rock"]
    );
    let top = service.get_tag_top_tracks("rock", 50).await.expect("tracks");
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].duration, Some(180));
    let info = service.get_track_info("Artist", "Song").await.expect("info");
    assert_eq!(info.expect("a track").album.as_deref(), Some("Album"));
    service
        .get_similar_artists("Radiohead", 20)
        .await
        .expect("artists");
    assert_eq!(received(&server).await.len(), 5);
}

#[tokio::test]
async fn radio_methods_tolerate_malformed_empty_and_rate_limited_responses() {
    let malformed = serve(answer(|_| "not json".to_string())).await;
    assert!(
        service(&malformed)
            .get_similar_artists("A", 20)
            .await
            .expect("empty")
            .is_empty()
    );
    let limited = serve(Fixture {
        status: 429,
        ..answer(|_| "{}".to_string())
    })
    .await;
    assert!(
        service(&limited)
            .get_tag_top_tracks("rock", 50)
            .await
            .expect("empty")
            .is_empty()
    );
}

/// The caller's cancellation is dropping the future: the call does not answer an empty list in
/// its place.
#[tokio::test]
async fn radio_methods_propagate_caller_cancellation() {
    let server = serve(Fixture {
        delay: Duration::from_secs(5),
        ..answer(|_| "{}".to_string())
    })
    .await;
    let service = service(&server);

    let outcome = tokio::time::timeout(Duration::from_millis(20), service.get_similar_artists("A", 20)).await;

    assert!(outcome.is_err(), "the call answered instead of being cancelled");
}

/// Last.fm as it files a renamed artist: the catalog under "Kanye West" only.
/// Records every request's method and artist.
fn renamed_artist_last_fm(asked: Arc<Mutex<Vec<(String, String)>>>, knows_song: bool) -> Fixture {
    answer(move |request| {
        let (method, artist) = (query(request, "method"), query(request, "artist"));
        asked.lock().push((method.clone(), artist.clone()));
        match method.as_str() {
            "track.getsimilar" if knows_song && artist == "Kanye West" => {
                r#"{"similartracks":{"track":[{"name":"Gold Digger","match":1,"artist":{"name":"Kanye West"}}]}}"#
                    .to_string()
            }
            "track.getsimilar" => r#"{"similartracks":{"track":[]}}"#.to_string(),
            "artist.getsimilar" if artist == "Kanye West" => {
                r#"{"similarartists":{"artist":[{"name":"Jay-Z"}]}}"#.to_string()
            }
            "artist.getsimilar" => r#"{"similarartists":{"artist":[{"name":"Jon Anderson"}]}}"#.to_string(),
            "artist.gettoptracks" => format!(r#"{{"toptracks":{{"track":[{{"name":"Top of {artist}"}}]}}}}"#),
            _ => "{}".to_string(),
        }
    })
}

fn asked_pair(method: &str, artist: &str) -> (String, String) {
    (method.to_string(), artist.to_string())
}

#[tokio::test]
async fn similar_tracks_a_renamed_artist_is_also_asked_under_the_name_last_fm_knows() {
    // Last.fm has nothing under "Ye" for this song, and the similar tracks under "Kanye West".
    let asked = Arc::new(Mutex::new(Vec::new()));
    let server = serve(renamed_artist_last_fm(Arc::clone(&asked), true)).await;

    let tracks = service(&server).get_similar_tracks("Ye", "Crack Music", 50).await;

    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].title, "Gold Digger");
    let asked = asked.lock();
    assert!(asked.contains(&asked_pair("track.getsimilar", "Ye")), "{asked:?}");
    assert!(
        asked.contains(&asked_pair("track.getsimilar", "Kanye West")),
        "{asked:?}"
    );
    assert!(
        !asked.iter().any(|(method, _)| method == "artist.getsimilar"),
        "{asked:?}"
    );
}

#[tokio::test]
async fn similar_tracks_the_similar_artist_guess_runs_under_the_name_last_fm_knows() {
    // Asked for artists like "Ye", Last.fm answers for someone else: a mix of Jon Anderson.
    let asked = Arc::new(Mutex::new(Vec::new()));
    let server = serve(renamed_artist_last_fm(Arc::clone(&asked), false)).await;

    let tracks = service(&server).get_similar_tracks("Ye", "Crack Music", 50).await;

    let mut artists: Vec<&str> = tracks.iter().map(|track| track.artist.as_str()).collect();
    artists.dedup();
    assert_eq!(artists, ["Jay-Z"]);
    let asked = asked.lock();
    assert!(
        asked.contains(&asked_pair("artist.getsimilar", "Kanye West")),
        "{asked:?}"
    );
    assert!(
        !asked.contains(&asked_pair("artist.getsimilar", "Ye")),
        "{asked:?}"
    );
}

#[tokio::test]
async fn similar_tracks_an_artist_with_one_name_is_asked_only_under_it() {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let server = serve(renamed_artist_last_fm(Arc::clone(&asked), false)).await;

    service(&server)
        .get_similar_tracks("Radiohead", "Creep", 50)
        .await;

    let asked = asked.lock();
    for (method, artist) in asked.iter().filter(|(method, _)| method != "artist.gettoptracks") {
        assert_eq!(artist, "Radiohead", "{method}");
    }
    assert!(
        asked.contains(&asked_pair("artist.getsimilar", "Radiohead")),
        "{asked:?}"
    );
}

#[tokio::test]
async fn last_fm_asks_for_the_song_written_plainly_before_giving_up_on_it() {
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = Arc::clone(&calls);
    let server = serve(answer(move |request| {
        let url = dotnet::unescape_data_string(request.url.as_str());
        let body = if url.contains("artist=Drake&track=Too Good&") {
            r#"{"similartracks":{"track":[{"name":"Controlla","artist":{"name":"Drake"},"match":0.9}]}}"#
        } else {
            r#"{"similartracks":{"track":[]}}"#
        };
        seen.lock().push(url);
        body.to_string()
    }))
    .await;
    let service = LastFmService::with_base_url(store("key", false, ""), &format!("{}/2.0/", server.uri()));

    let similar = service
        .get_similar_tracks("Drake feat. Rihanna", "Too Good (feat. Rihanna)", 50)
        .await;

    assert_eq!(similar.len(), 1);
    assert_eq!(similar[0].title, "Controlla");
    assert!(
        !calls
            .lock()
            .iter()
            .any(|call| call.contains("getsimilarartists") || call.contains("artist.getsimilar"))
    );
}

/// The shapes track.getsimilar answers in: a string match with a thousands separator, durations
/// as strings and numbers, "0" as unknown, rows missing a name or artist skipped; and a `track`
/// that is an object instead of an array failing the whole read, which answers no tracks (and,
/// being an error, is not cached).
#[tokio::test]
async fn similar_tracks_read_every_shape_last_fm_sends() {
    let server = serve(answer(|request| {
        match query(request, "artist").as_str() {
            "Good" => {
                r#"{"similartracks":{"track":[
                {"name":"One","artist":{"name":"A"},"match":"1,000.5","duration":"180000"},
                {"name":"Two","artist":{"name":"B"},"match":0.25,"duration":2500},
                {"name":"Three","artist":{"name":"C"},"duration":"0"},
                {"name":"","artist":{"name":"D"}},
                {"name":"Five","artist":{}}
            ]}}"#
            }
            "Bad" => r#"{"similartracks":{"track":{"name":"Solo","artist":{"name":"A"}}}}"#,
            _ => "{}",
        }
        .to_string()
    }))
    .await;
    let service = service(&server);

    let tracks = service.get_similar_tracks("Good", "Song", 50).await;
    let shapes: Vec<(&str, f64, Option<i32>)> = tracks
        .iter()
        .map(|t| (t.title.as_str(), t.r#match, t.duration))
        .collect();
    assert_eq!(
        shapes,
        [
            ("One", 1000.5, Some(180)),
            ("Two", 0.25, Some(2)),
            ("Three", 0.0, None)
        ]
    );
    assert_eq!(service.get_similar_tracks("Good", "Song", 2).await.len(), 2);

    assert!(service.get_similar_tracks("Bad", "Song", 50).await.is_empty());
}

/// Track.search's listener counts, and what fails the radio readers outright.
#[tokio::test]
async fn search_reads_listeners_and_radio_readers_fail_on_the_wrong_shape() {
    let server = serve(answer(|request| {
        match query(request, "method").as_str() {
            "track.search" => {
                r#"{"results":{"trackmatches":{"track":[
                {"name":"Nightcall","artist":"Kavinsky","listeners":"1257699"},
                {"name":"Odd","artist":"X","listeners":12},
                {"name":"Float","artist":"Y","listeners":1.5},
                {"name":"NoArtist"}
            ]}}}"#
            }
            "artist.getsimilar" => r#"{"similarartists":{"artist":["not an object"]}}"#,
            _ => "{}",
        }
        .to_string()
    }))
    .await;
    let service = service(&server);

    let found: Vec<(String, Option<i64>)> = service
        .search_tracks("kavinsky", 30)
        .await
        .into_iter()
        .map(|t| (t.title, t.listeners))
        .collect();
    assert_eq!(
        found,
        [
            ("Nightcall".to_string(), Some(1_257_699)),
            ("Odd".to_string(), Some(12)),
            ("Float".to_string(), None)
        ]
    );
    assert!(service.search_tracks("  ", 30).await.is_empty());
    assert!(service.get_similar_artists("A", 20).await.is_err());
    // No key: nothing is asked, and the empty answer is cached.
    let keyless = LastFmService::with_base_url(store("", false, "en"), &format!("{}/2.0/", server.uri()));
    let before = received(&server).await.len();
    assert!(
        keyless
            .get_artist_top_tags("A", 10)
            .await
            .expect("empty")
            .is_empty()
    );
    assert_eq!(received(&server).await.len(), before);
}

#[tokio::test]
async fn similar_tracks_a_small_cached_answer_does_not_short_change_a_bigger_ask() {
    // A station refresh asks for a few; a player's radio then asks for many.
    let server = serve(answer(|request| {
        let limit: usize = query(request, "limit").parse().unwrap_or(50);
        if query(request, "method") != "track.getsimilar" {
            return "{}".into();
        }
        let tracks: Vec<String> = (0..limit)
            .map(|index| {
                format!(r#"{{"name":"song-{index}","match":1,"artist":{{"name":"Artist {index}"}}}}"#)
            })
            .collect();
        format!(r#"{{"similartracks":{{"track":[{}]}}}}"#, tracks.join(","))
    }))
    .await;
    let service = service(&server);

    assert_eq!(service.get_similar_tracks("Radiohead", "Creep", 5).await.len(), 5);
    assert_eq!(
        service.get_similar_tracks("Radiohead", "Creep", 20).await.len(),
        20
    );
    assert_eq!(received(&server).await.len(), 2);

    // A smaller ask after that is served from the bigger answer.
    assert_eq!(
        service.get_similar_tracks("Radiohead", "Creep", 10).await.len(),
        10
    );
    assert_eq!(received(&server).await.len(), 2);
}

/// How a list of songs like a seed was found. A nightcore title found only as its original is the
/// original's, an upload's own "Artist - Title" is asked under the artist it names, and similar
/// artists to a name Last.fm barely knows are a guess.
#[tokio::test]
async fn find_similar_tracks_says_how_the_list_was_found() {
    let server = serve(answer(|request| {
        let (method, artist, track) = (query(request, "method"), query(request, "artist"), query(request, "track"));
        match (method.as_str(), artist.as_str(), track.as_str()) {
            ("track.getsimilar", "Alan Walker", "Faded") => {
                r#"{"similartracks":{"track":[{"name":"Wake Me Up","artist":{"name":"Avicii"},"match":0.9}]}}"#.into()
            }
            ("track.getsimilar", "Massive Attack", "Teardrop") => {
                r#"{"similartracks":{"track":[{"name":"Roads","artist":{"name":"Portishead"},"match":0.9}]}}"#.into()
            }
            ("track.getsimilar", _, _) => r#"{"similartracks":{"track":[]}}"#.into(),
            ("artist.getsimilar", _, _) => r#"{"similarartists":{"artist":[{"name":"The Year"}]}}"#.into(),
            ("artist.gettoptracks", _, _) => r#"{"toptracks":{"track":[{"name":"Full Damage"}]}}"#.into(),
            ("artist.getInfo", "Drake", _) => r#"{"artist":{"stats":{"listeners":"5200000"}}}"#.into(),
            ("artist.getInfo", _, _) => r#"{"artist":{"stats":{"listeners":40}}}"#.into(),
            _ => r#"{"error":6,"message":"Track not found"}"#.into(),
        }
    }))
    .await;
    let service = service(&server);

    let song = service
        .find_similar_tracks("Massive Attack", "Teardrop", 10)
        .await;
    assert_eq!((song.source, song.artist_trusted), (SimilarSource::Song, true));

    let nightcore = service
        .find_similar_tracks("Nightcore Galaxy", "Alan Walker - Faded (Nightcore)", 10)
        .await;
    assert_eq!(nightcore.source, SimilarSource::Original);
    assert_eq!(nightcore.tracks[0].title, "Wake Me Up");

    let guess = service.find_similar_tracks("Missigno", "SPOOKY", 10).await;
    assert_eq!(
        (guess.source, guess.artist_trusted, guess.tracks.len()),
        (SimilarSource::SimilarArtists, false, 1)
    );
    let known = service.find_similar_tracks("Drake", "Brand New", 10).await;
    assert_eq!(
        (known.source, known.artist_trusted),
        (SimilarSource::SimilarArtists, true)
    );

    assert_eq!(style_versions(Some("Faded (Nightcore)")), ["nightcore"]);
    assert!(style_versions(Some("Faded (Remastered)")).is_empty());
}

/// Fails the first call, then answers every call with `body`; counts the calls.
struct FailsOnce {
    calls: Arc<Mutex<u32>>,
    body: &'static str,
}

impl Respond for FailsOnce {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let mut calls = self.calls.lock();
        *calls += 1;
        if *calls == 1 {
            ResponseTemplate::new(500)
        } else {
            ResponseTemplate::new(200).set_body_string(self.body)
        }
    }
}

#[tokio::test]
async fn a_failed_call_is_not_kept_as_an_empty_answer() {
    let calls = Arc::new(Mutex::new(0));
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(FailsOnce {
            calls: Arc::clone(&calls),
            body: r#"{"similarartists":{"artist":[{"name":"Justice","match":"1"}]}}"#,
        })
        .mount(&server)
        .await;
    let last_fm = service(&server);

    let failed = last_fm.get_similar_artists("Daft Punk", 5).await.expect("reads");
    let answered = last_fm.get_similar_artists("Daft Punk", 5).await.expect("reads");
    let kept = last_fm.get_similar_artists("Daft Punk", 5).await.expect("reads");

    assert!(failed.is_empty());
    assert_eq!(answered.len(), 1);
    assert_eq!(kept.len(), 1);
    assert_eq!(
        *calls.lock(),
        2,
        "asked again after the failure, then answered from the cache"
    );
}

#[tokio::test]
async fn keeping_an_answer_sweeps_out_the_expired_ones() {
    let server = serve(answer(|_| {
        r#"{"toptags":{"tag":[{"name":"house"}]}}"#.to_string()
    }))
    .await;
    let last_fm = service(&server);
    last_fm.radio_cache.lock().insert(
        "stale".into(),
        (Utc::now() - TimeDelta::minutes(1), RadioValue::Count(1)),
    );

    last_fm.get_artist_top_tags("Daft Punk", 3).await.expect("reads");

    let cache = last_fm.radio_cache.lock();
    assert!(!cache.contains_key("stale"));
    assert_eq!(cache.len(), 1);
}

#[tokio::test]
async fn an_artists_top_tracks_are_kept_and_not_asked_for_twice() {
    let server = serve(answer(|_| {
        r#"{"toptracks":{"track":[{"name":"One More Time"}]}}"#.to_string()
    }))
    .await;
    let last_fm = service(&server);

    let first = last_fm.get_artist_top_tracks("Daft Punk", 10).await;
    let second = last_fm.get_artist_top_tracks("Daft Punk", 10).await;

    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(received(&server).await.len(), 1);
}

/// Notes when each call arrived, and answers it slowly.
struct SlowArrivals {
    arrived: Arc<Mutex<Vec<std::time::Instant>>>,
}

impl Respond for SlowArrivals {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        self.arrived.lock().push(std::time::Instant::now());
        ResponseTemplate::new(200)
            .set_body_string(r#"{"toptracks":{"track":[]}}"#)
            .set_delay(Duration::from_millis(400))
    }
}

#[tokio::test]
async fn top_track_calls_take_turns_at_the_provider_gate() {
    let arrived = Arc::new(Mutex::new(Vec::new()));
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(SlowArrivals {
            arrived: Arc::clone(&arrived),
        })
        .mount(&server)
        .await;
    let last_fm = service(&server);
    let start = std::time::Instant::now();

    futures::future::join_all((0..8).map(|i| {
        let last_fm = &last_fm;
        async move { last_fm.get_artist_top_tracks(&format!("Artist {i}"), 5).await }
    }))
    .await;

    let early = arrived
        .lock()
        .iter()
        .filter(|at| at.duration_since(start) < Duration::from_millis(200))
        .count();
    assert_eq!(early, 4, "four at once, the rest after");
}

/// Two artists like the seed, the second one's top songs rate limited while `limited` is set.
struct OneArtistLimited {
    limited: Arc<Mutex<bool>>,
}

impl Respond for OneArtistLimited {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        match query(request, "method").as_str() {
            "artist.getsimilar" => ResponseTemplate::new(200)
                .set_body_string(r#"{"similarartists":{"artist":[{"name":"One"},{"name":"Two"}]}}"#),
            "artist.gettoptracks" if query(request, "artist") == "Two" && *self.limited.lock() => {
                ResponseTemplate::new(429)
            }
            "artist.gettoptracks" => ResponseTemplate::new(200).set_body_string(format!(
                r#"{{"toptracks":{{"track":[{{"name":"{} song"}}]}}}}"#,
                query(request, "artist")
            )),
            _ => ResponseTemplate::new(200).set_body_string("{}"),
        }
    }
}

/// Radio for an artist plays the songs of the similar artists Last.fm answered for, and keeps
/// that list only once every one of them answered.
#[tokio::test]
async fn similar_artist_songs_skip_an_unanswered_artist_and_are_kept_only_when_whole() {
    let limited = Arc::new(Mutex::new(true));
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(OneArtistLimited {
            limited: Arc::clone(&limited),
        })
        .mount(&server)
        .await;
    let last_fm = service(&server);
    let titles =
        |tracks: Vec<SimilarTrack>| -> Vec<String> { tracks.into_iter().map(|track| track.title).collect() };

    assert_eq!(
        titles(last_fm.get_similar_artist_tracks("Seed", 10).await),
        ["One song"]
    );
    *limited.lock() = false;
    assert_eq!(
        titles(last_fm.get_similar_artist_tracks("Seed", 10).await),
        ["One song", "Two song"],
        "the list missing Two was not kept"
    );
}
