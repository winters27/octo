//! Radio from a song Last.fm cannot place (#78), the songs of a library built from YouTube
//! playlists: the artist is the uploader and the album is the playlist. The seed's own album and
//! genre lead, and similar artists to an uploader's name stay out. Also the other sources, what
//! radio learns and one-star bans, through the whole pipeline. (The bare server address test is
//! in `stream_length_tests`.)

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, StatusCode};
use octo_core::last_fm::last_fm_radio_recommendation_service::seeded_randomizer;
use octo_core::models::domain::Song;
use octo_core::settings::{AppSettings, LastFmSettings, RadioSourceSettings, SubsonicSettings};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use tempfile::TempDir;
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use super::{TestMetadata, get, get_string, send};
use crate::app::{AppState, TestServices};
use crate::http::pipeline::{App, build_with};
use crate::services::sonic::SonicSong;

/// Navidrome, Last.fm and the shim's YouTube Music, on one fake host.
#[derive(Clone, Default)]
struct Upstream {
    calls: Arc<Mutex<Vec<String>>>,
}

fn song(id: &str, title: &str, artist: &str, album: &str, album_id: &str, genre: &str) -> Value {
    json!({"id": id, "title": title, "artist": artist, "album": album, "albumId": album_id, "genre": genre, "duration": 180})
}

fn library_song(id: &str) -> Option<Value> {
    Some(match id {
        "spooky" => song("spooky", "SPOOKY", "Missigno", "PHONK", "al-phonk", "Phonk"),
        "teardrop" => song(
            "teardrop",
            "Teardrop",
            "Massive Attack",
            "Mezzanine",
            "al-mezz",
            "Trip-Hop",
        ),
        "roads" => song("roads", "Roads", "Portishead", "Dummy", "al-dummy", "Trip-Hop"),
        "brand-new" => song(
            "brand-new",
            "Brand New",
            "Drake",
            "Brand New",
            "al-brand-new",
            "Hip-Hop",
        ),
        "wide" => song("wide", "Wide", "Wide Artist", "Wides", "al-wides", "Trip-Hop"),
        "hello" => song("hello", "Hello", "Adele", "25", "al-25", "Pop"),
        "renamed" => song(
            "renamed",
            "Stronger",
            "Kanye West",
            "Graduation",
            "al-graduation",
            "Hip-Hop",
        ),
        "quiet" => song("quiet", "Quiet", "Nobody Known", "Quiet", "al-quiet", "Ambient"),
        "hush" => song("hush", "Hush", "Nobody Known", "Hush", "al-hush", "Ambient"),
        "blend" => song(
            "blend",
            "Blend",
            "Blend Artist",
            "Blends",
            "al-blends",
            "Trip-Hop",
        ),
        "faded-nightcore" => song(
            "faded-nightcore",
            "Alan Walker - Faded (Nightcore)",
            "Nightcore Galaxy",
            "Nightcore",
            "al-nightcore",
            "",
        ),
        _ => return None,
    })
}

/// The library's copy of one of Blend's thirty radio songs: every third one ("Band 3 Tune 3",
/// "Band 6 Tune 6", ...) is in the library.
fn blend_library_song(query: &str) -> Option<Value> {
    let i: i32 = query.strip_prefix("Band ")?.split(' ').next()?.parse().ok()?;
    (i % 3 == 0).then(|| {
        song(
            &format!("lib-band-{i}"),
            &format!("Tune {i}"),
            &format!("Band {i}"),
            "Blends",
            "al-blends",
            "Trip-Hop",
        )
    })
}

fn album(id: &str, name: &str, artist: &str, songs: Vec<Value>) -> Value {
    json!({"album": {"id": id, "name": name, "artist": artist, "song": songs}})
}

/// A Subsonic "ok" carrying the answer's fields.
fn ok(fields: Value) -> String {
    let mut fields: Map<String, Value> = match fields {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    fields.insert("status".into(), "ok".into());
    fields.insert("version".into(), "1.16.1".into());
    json!({"subsonic-response": fields}).to_string()
}

fn ytm(id: &str, title: &str, artist: &str, seconds: i32, kind: &str) -> Value {
    json!({"videoId": id, "title": title, "artists": [artist], "durationSeconds": seconds, "videoType": kind})
}

const ATV: &str = "MUSIC_VIDEO_TYPE_ATV";
const UGC: &str = "MUSIC_VIDEO_TYPE_UGC";

/// The shim's YouTube Music answers: the upload is found among videos, its radio is phonk
/// uploads; Teardrop's radio has Glory Box, an upload and a podcast.
fn you_tube_music(path: &str, query: &HashMap<String, String>) -> Value {
    let q = |name: &str| query.get(name).map(String::as_str).unwrap_or("");
    let rows: Vec<Value> = match path {
        "/ytm/search" => match (q("filter"), q("q")) {
            ("videos", "Missigno SPOOKY") => vec![ytm("spooky-v", "SPOOKY", "Missigno", 180, UGC)],
            ("songs", "Massive Attack Teardrop") => {
                vec![ytm("teardrop-v", "Teardrop", "Massive Attack", 180, ATV)]
            }
            _ => Vec::new(),
        },
        "/ytm/radio" => match q("videoId") {
            "spooky-v" => vec![
                ytm("spooky-v", "SPOOKY", "Missigno", 180, UGC),
                ytm("d1", "DRIFT KING", "Phonk Lord", 150, UGC),
                ytm("d2", "NIGHT RUN", "Phonk Lord II", 160, UGC),
            ],
            "teardrop-v" => vec![
                ytm("g", "Glory Box", "Portishead", 305, ATV),
                ytm("u", "Teardrop cover", "Some Channel", 300, UGC),
                ytm(
                    "p",
                    "Episode 4",
                    "A Podcast",
                    2400,
                    "MUSIC_VIDEO_TYPE_PODCAST_EPISODE",
                ),
                ytm("s", "Sour Times", "Portishead", 250, ATV),
            ],
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    json!({ "tracks": rows })
}

fn last_fm(query: &HashMap<String, String>) -> Value {
    let q = |name: &str| query.get(name).map(String::as_str).unwrap_or("");
    let (artist, track) = (q("artist"), q("track"));
    match q("method") {
        "track.getsimilar" => match (artist, track) {
            // A full answer: Last.fm knows this song well.
            ("Massive Attack", "Teardrop") => {
                let mut tracks = vec![
                    json!({"name": "Glory Box", "artist": {"name": "Portishead"}, "match": 0.9}),
                    json!({"name": "Trigger Hippie", "artist": {"name": "Morcheeba"}, "match": 0.8}),
                ];
                tracks.extend((1..=23).map(|i| {
                    json!({"name": format!("Song {i}"), "artist": {"name": format!("Artist {i}")}, "match": 0.7 - f64::from(i) / 100.0})
                }));
                json!({"similartracks": {"track": tracks}})
            }
            // A thin one: two songs like it.
            ("Portishead", "Roads") => json!({"similartracks": {"track": [
                {"name": "Angel", "artist": {"name": "Massive Attack"}, "match": 0.9},
                {"name": "Unfinished Sympathy", "artist": {"name": "Massive Attack"}, "match": 0.8}
            ]}}),
            // Thirty songs, every third one in the library (see `blend_library_song`).
            ("Blend Artist", "Blend") => json!({"similartracks": {"track": (1..=30).map(|i| {
                json!({"name": format!("Tune {i}"), "artist": {"name": format!("Band {i}")}, "match": 0.95 - f64::from(i) / 100.0})
            }).collect::<Vec<_>>()}}),
            // More songs than any radio asks for, none in the library.
            ("Wide Artist", "Wide") => json!({"similartracks": {"track": (1..=WIDE_SONGS).map(|i| {
                json!({"name": format!("Far {i}"), "artist": {"name": format!("Crowd {i}")}, "match": 0.99 - i as f64 / 200.0})
            }).collect::<Vec<_>>()}}),
            // Takes Skip covers and instrumentals leaves out, beside a song only named after the word.
            ("Adele", "Hello") => json!({"similartracks": {"track": [
                {"name": "Someone Like You", "artist": {"name": "Adele"}, "match": 0.9},
                {"name": "Rolling in the Deep (Instrumental)", "artist": {"name": "Adele"}, "match": 0.85},
                {"name": "Skyfall (Karaoke Version)", "artist": {"name": "Adele"}, "match": 0.8},
                {"name": "Karaoke Queen", "artist": {"name": "Catfish and the Bottlemen"}, "match": 0.75},
                {"name": "Make You Feel My Love (Acoustic Cover)", "artist": {"name": "Adele"}, "match": 0.72},
                {"name": "Set Fire to the Rain", "artist": {"name": "Adele"}, "match": 0.7}
            ]}}),
            // Two songs by an artist the library files under another name, among 23 it lacks.
            ("Kanye West", "Stronger") => {
                let mut tracks = vec![
                    json!({"name": "Through the Wire", "artist": {"name": "Kanye West"}, "match": 0.9}),
                    json!({"name": "Jesus Walks", "artist": {"name": "Kanye West"}, "match": 0.85}),
                ];
                tracks.extend((1..=23).map(|i| {
                    json!({"name": format!("Far {i}"), "artist": {"name": format!("Crowd {i}")}, "match": 0.8 - f64::from(i) / 100.0})
                }));
                json!({"similartracks": {"track": tracks}})
            }
            ("Alan Walker", "Faded") => json!({"similartracks": {"track": [
                {"name": "Wake Me Up", "artist": {"name": "Avicii"}, "match": 0.9}
            ]}}),
            _ => json!({"similartracks": {"track": []}}),
        },
        "artist.getsimilar" => match artist {
            "Massive Attack" => json!({"similarartists": {"artist": [{"name": "Portishead"}]}}),
            "Drake" => json!({"similarartists": {"artist": [{"name": "Future"}]}}),
            // Nobody on Last.fm sounds like them, so the radio finds nothing at all.
            "Nobody Known" => json!({"similarartists": {"artist": []}}),
            _ => json!({"similarartists": {"artist": [{"name": "The Year"}]}}),
        },
        "artist.gettoptracks" => match artist {
            "Portishead" => json!({"toptracks": {"track": [{"name": "Roads"}]}}),
            "Future" => json!({"toptracks": {"track": [{"name": "Mask Off"}]}}),
            _ => json!({"toptracks": {"track": [{"name": "Full Damage"}]}}),
        },
        // An uploader's name can be on Last.fm too, scrobbled by a few people.
        "artist.getInfo" => {
            if artist == "Drake" {
                json!({"artist": {"name": "Drake", "stats": {"listeners": "5200000"}}})
            } else {
                json!({"artist": {"name": "Missigno", "stats": {"listeners": "40"}}})
            }
        }
        "track.getInfo" | "track.getinfo" => json!({"error": 6, "message": "Track not found"}),
        "tag.gettoptracks" => {
            if q("tag") == "Phonk" {
                json!({"tracks": {"track": [{"name": "Murder In My Mind", "artist": {"name": "Kordhell"}}]}})
            } else {
                json!({"tracks": {"track": []}})
            }
        }
        _ => json!({}),
    }
}

impl Respond for Upstream {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let path = request.url.path().to_string();
        self.calls
            .lock()
            .push(format!("{}?{}", path, request.url.query().unwrap_or("")));
        let query: HashMap<String, String> = request.url.query_pairs().into_owned().collect();
        let json = |body: String| ResponseTemplate::new(200).set_body_raw(body, "application/json");
        // Navidrome answers a doubled slash with its web app, as a 200.
        if path.starts_with("//") {
            return ResponseTemplate::new(200).set_body_raw("<!DOCTYPE html>", "text/html");
        }
        if path == "/2.0/" {
            return json(last_fm(&query).to_string());
        }
        if path.starts_with("/ytm/") {
            return json(you_tube_music(&path, &query).to_string());
        }
        // Navidrome refuses mallory's sign-in.
        if query.get("u").map(String::as_str) == Some("mallory")
            && (path.ends_with("/rest/ping") || path.ends_with("/rest/scrobble"))
        {
            return json(
                r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}"#
                    .into(),
            );
        }
        let id = query.get("id").cloned().unwrap_or_default();
        let answer: Option<Value> = if path.ends_with("/rest/getSong") {
            library_song(&id).map(|song| json!({ "song": song }))
        } else if path.ends_with("/rest/getAlbum") {
            match id.as_str() {
                "al-phonk" => Some(album(
                    "al-phonk",
                    "PHONK",
                    "Missigno",
                    vec![
                        library_song("spooky").unwrap(),
                        song("pl-2", "DRIFT", "Missigno", "PHONK", "al-phonk", "Phonk"),
                        song("pl-3", "NIGHT RIDE", "Missigno", "PHONK", "al-phonk", "Phonk"),
                    ],
                )),
                "al-nightcore" => Some(album(
                    "al-nightcore",
                    "Nightcore",
                    "Nightcore Galaxy",
                    vec![
                        library_song("faded-nightcore").unwrap(),
                        song(
                            "pl-9",
                            "Alone (Nightcore)",
                            "Nightcore Galaxy",
                            "Nightcore",
                            "al-nightcore",
                            "",
                        ),
                    ],
                )),
                "album-trip" => Some(album(
                    "album-trip",
                    "Mezzanine",
                    "Massive Attack",
                    vec![library_song("teardrop").unwrap()],
                )),
                "al-dummy" => Some(album(
                    "al-dummy",
                    "Dummy",
                    "Portishead",
                    vec![
                        library_song("roads").unwrap(),
                        song(
                            "dummy-2",
                            "Sour Times",
                            "Portishead",
                            "Dummy",
                            "al-dummy",
                            "Trip-Hop",
                        ),
                    ],
                )),
                _ => None,
            }
        } else if path.ends_with("/rest/getArtist") {
            (id == "artist-massive")
                .then(|| json!({"artist": {"id": "artist-massive", "name": "Massive Attack"}}))
        } else if path.ends_with("/rest/getRandomSongs") {
            Some(if query.get("genre").map(String::as_str) == Some("Phonk") {
                json!({"randomSongs": {"song": [
                    song("genre-1", "GHOST", "Kaito", "Phonk Mix", "al-mix", "Phonk"),
                    library_song("spooky").unwrap()
                ]}})
            } else {
                json!({"randomSongs": {"song": []}})
            })
        } else if path.ends_with("/rest/search3")
            && query.get("query").map(String::as_str) == Some("Through the Wire")
        {
            // Filed under "Ye": found by the title alone, never under "Kanye West".
            Some(json!({"searchResult3": {"song": [
                song("wire-cover", "Through the Wire", "Some Band", "Covers", "al-covers", "Pop"),
                song("ye-wire", "Through the Wire", "Ye", "The College Dropout", "al-dropout", "Hip-Hop")
            ]}}))
        } else if path.ends_with("/rest/search3") {
            Some(match query.get("query").and_then(|q| blend_library_song(q)) {
                Some(found) => json!({"searchResult3": {"song": [found]}}),
                None => json!({"searchResult3": {}}),
            })
        } else if path.ends_with("/rest/getSimilarSongs2") && id == "quiet" {
            // Navidrome's agents know nothing of it either: its "not found" error.
            None
        } else if path.ends_with("/rest/getSimilarSongs2") {
            // Navidrome's own agents, for when Octo's sources have nothing.
            Some(json!({"similarSongs2": {"song": [
                song("nd-1", "Unfinished Sympathy", "Massive Attack", "Blue Lines", "al-blue", "Trip-Hop")
            ]}}))
        } else if path.ends_with("/rest/ping") || path.ends_with("/rest/scrobble") {
            Some(json!({}))
        } else if path.ends_with("/rest/getOpenSubsonicExtensions") {
            Some(json!({"openSubsonic": true, "openSubsonicExtensions": []}))
        } else {
            None
        };
        match answer {
            Some(answer) => json(ok(answer)),
            None => json(
                r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":70,"message":"not found"}}}"#
                    .into(),
            ),
        }
    }
}

struct Fixture {
    _server: MockServer,
    dir: TempDir,
    upstream: Upstream,
    state: AppState,
    app: App,
}

impl Fixture {
    fn calls(&self) -> Vec<String> {
        self.upstream.calls.lock().clone()
    }
}

/// The defaults leave every radio source but Last.fm off, so a test switches on the ones it is
/// about.
async fn fixture(change: impl FnOnce(&mut AppSettings)) -> Fixture {
    let server = MockServer::builder().start().await;
    let upstream = Upstream::default();
    Mock::given(wiremock::matchers::any())
        .respond_with(upstream.clone())
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut settings = AppSettings {
        subsonic: SubsonicSettings {
            url: Some(server.uri()),
            auto_detect_download_path: false,
            ..Default::default()
        },
        last_fm: LastFmSettings {
            api_key: "test-key".into(),
            enable_radio: true,
            ..Default::default()
        },
        radio_sources: RadioSourceSettings {
            you_tube_music: false,
            listen_brainz: false,
            sounds_alike: false,
            learn_from_listening: false,
            ..Default::default()
        },
        ..Default::default()
    };
    change(&mut settings);
    // Every Last.fm pick not in the library comes back as an outside copy named after it.
    let metadata = Arc::new(TestMetadata::answering(|artist, title, _| {
        vec![Song {
            id: format!("ext-{artist}-{title}"),
            artist: artist.into(),
            title: title.into(),
            is_local: false,
            ..Default::default()
        }]
    }));
    let state = AppState::for_tests_with(
        settings,
        TestServices {
            metadata: Some(metadata),
            last_fm_base_url: Some(format!("{}/2.0/", server.uri())),
            config_dir: Some(dir.path().to_path_buf()),
            raw_settings: vec![
                ("YouTube:ShimUrl".into(), server.uri()),
                (
                    "Octo:StateDirectory".into(),
                    dir.path().to_string_lossy().into_owned(),
                ),
            ],
            ..Default::default()
        },
    );
    let app = build_with(
        state.clone(),
        crate::controllers::subsonic::routes().merge(crate::controllers::admin::routes()),
    );
    Fixture {
        _server: server,
        dir,
        upstream,
        state,
        app,
    }
}

async fn radio_ids(f: &Fixture, id: &str) -> Vec<String> {
    let body = get_string(
        &f.app,
        &format!("/rest/getSimilarSongs2?id={id}&u=alice&t=token&s=salt&f=json&count=20"),
    )
    .await;
    let doc: Value = serde_json::from_str(&body).expect("JSON");
    doc["subsonic-response"]["similarSongs2"]["song"]
        .as_array()
        .map(|songs| {
            songs
                .iter()
                .map(|song| song["id"].as_str().expect("an id").to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn songs(body: &str) -> Vec<(String, Option<String>)> {
    let doc: Value = serde_json::from_str(body).expect("JSON");
    doc["subsonic-response"]["similarSongs2"]["song"]
        .as_array()
        .expect("songs")
        .iter()
        .map(|song| {
            (
                song["id"].as_str().expect("an id").to_string(),
                song["octoSuggestedBy"].as_str().map(str::to_string),
            )
        })
        .collect()
}

async fn radio(f: &Fixture, user: &str, id: &str, count: i32) -> Vec<(String, Option<String>)> {
    songs(
        &get_string(
            &f.app,
            &format!("/rest/getSimilarSongs2?id={id}&u={user}&t=token&s=salt&f=json&count={count}"),
        )
        .await,
    )
}

#[tokio::test]
async fn unknown_upload_leads_with_its_playlist_and_genre_and_leaves_out_the_uploaders_similar_artists() {
    let f = fixture(|_| {}).await;
    let songs = radio_ids(&f, "spooky").await;

    assert!(!songs.is_empty());
    assert!(!songs.contains(&"spooky".to_string()));
    // The playlist's own songs and the library's phonk, then Last.fm's phonk.
    for id in ["pl-2", "pl-3", "genre-1"] {
        assert!(songs.contains(&id.to_string()), "{id} in {songs:?}");
    }
    assert!(songs.iter().any(|id| id.starts_with("ext-Kordhell")), "{songs:?}");
    let first_outside = songs
        .iter()
        .position(|id| id.starts_with("ext-"))
        .expect("an outside song");
    assert!(first_outside > 0, "a library song plays first");
    // Last.fm never heard of "Missigno - SPOOKY", so artists like that name are a guess.
    assert!(!songs.iter().any(|id| id.contains("The Year")));
    assert!(
        f.calls()
            .iter()
            .any(|call| call.contains("tag.gettoptracks") && call.contains("tag=Phonk"))
    );
}

#[tokio::test]
async fn skip_covers_and_instrumentals_keeps_outside_takes_off_radio_by_default() {
    let f = fixture(|_| {}).await;
    let ids = |songs: Vec<(String, Option<String>)>| songs.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
    let takes = [
        "ext-Adele-Rolling in the Deep (Instrumental)".to_string(),
        "ext-Adele-Skyfall (Karaoke Version)".to_string(),
        "ext-Adele-Make You Feel My Love (Acoustic Cover)".to_string(),
    ];
    let named = "ext-Catfish and the Bottlemen-Karaoke Queen".to_string();

    let skipped = ids(radio(&f, "alice", "hello", 6).await);
    for take in &takes {
        assert!(!skipped.contains(take), "{take} in {skipped:?}");
    }
    assert!(skipped.contains(&named), "a name is not a take: {skipped:?}");
    assert!(
        skipped.contains(&"ext-Adele-Someone Like You".to_string()),
        "{skipped:?}"
    );

    let mut off = f.state.settings.current().as_ref().clone();
    off.soulseek.skip_covers_and_instrumentals = false;
    f.state.settings.set(off);
    let listed = ids(radio(&f, "alice", "hello", 6).await);
    for take in &takes {
        assert!(listed.contains(take), "{take} in {listed:?}");
    }
}

#[tokio::test]
async fn known_song_is_last_fms_radio_as_before() {
    let f = fixture(|_| {}).await;
    let songs = radio_ids(&f, "teardrop").await;

    // A full answer for the song itself: Last.fm's order, and the library is not asked.
    assert_eq!(
        songs[..2],
        ["ext-Portishead-Glory Box", "ext-Morcheeba-Trigger Hippie"]
    );
    assert_eq!(songs.len(), 20);
    assert!(!f.calls().iter().any(|call| call.contains("rest/getAlbum")));
}

#[tokio::test]
async fn every_radio_song_says_which_source_suggested_it() {
    let f = fixture(|_| {}).await;
    for format in ["json", "xml"] {
        let body = get_string(
            &f.app,
            &format!("/rest/getSimilarSongs2?id=spooky&u=alice&t=token&s=salt&f={format}&count=20"),
        )
        .await;
        // Last.fm's chart for the genre, and the playlist's own songs.
        for by in ["Last.fm", "Your library"] {
            let expected = if format == "json" {
                format!("\"octoSuggestedBy\":\"{by}\"")
            } else {
                format!("octoSuggestedBy=\"{by}\"")
            };
            assert!(body.contains(&expected), "{format}: {expected} in {body}");
        }
    }
}

#[tokio::test]
async fn the_radio_sources_extension_is_listed_while_radio_can_answer() {
    for (radio, listed) in [(true, true), (false, false)] {
        let f = fixture(|s| s.last_fm.enable_radio = radio).await;
        let body = get_string(&f.app, "/rest/getOpenSubsonicExtensions?f=json").await;
        assert_eq!(body.contains("octoRadioSources"), listed, "{radio}: {body}");
    }
}

#[tokio::test]
async fn you_tube_music_joins_the_radio_of_an_upload_last_fm_does_not_know() {
    let f = fixture(|s| s.radio_sources.you_tube_music = true).await;
    let songs = radio(&f, "alice", "spooky", 20).await;

    assert!(
        songs
            .iter()
            .any(|(id, by)| id == "ext-Phonk Lord-DRIFT KING" && by.as_deref() == Some("YouTube Music")),
        "{songs:?}"
    );
    assert!(!songs.iter().any(|(id, _)| id.contains("SPOOKY")));
    // Two songs like it is a thin answer, so the playlist's own songs still lead.
    assert!(songs[0].0.starts_with("pl-"), "{songs:?}");
}

#[tokio::test]
async fn a_song_last_fm_and_you_tube_music_both_suggest_comes_first_and_uploads_stay_off_a_releases_radio() {
    let f = fixture(|s| s.radio_sources.you_tube_music = true).await;
    let songs = radio(&f, "alice", "teardrop", 50).await;

    assert_eq!(songs[0].0, "ext-Portishead-Glory Box");
    // YouTube Music's two songs are a thin answer next to Last.fm's 25: its own pick plays, but
    // after Last.fm's.
    let sour_times = songs
        .iter()
        .position(|(id, by)| id == "ext-Portishead-Sour Times" && by.as_deref() == Some("YouTube Music"))
        .expect("Sour Times plays");
    assert!(sour_times > 20, "Sour Times at {sour_times}");
    assert!(
        !songs
            .iter()
            .any(|(id, _)| id.contains("cover") || id.contains("Episode"))
    );
}

/// 23 numbers with the second one placing the song along a line: songs close on the line sound
/// alike.
fn sound(at: f32, title: &str) -> SonicSong {
    let mut f = vec![0.0; 23];
    f[1] = at;
    SonicSong {
        stamp: "1:1".into(),
        title: title.into(),
        artist: "Artist".into(),
        version: 2,
        f,
        ..Default::default()
    }
}

#[tokio::test]
async fn sounds_alike_adds_library_songs_that_sound_like_it_read_again_with_the_listeners_sign_in() {
    let f = fixture(|s| s.radio_sources.sounds_alike = true).await;
    f.state.sonic_store.write(|s| {
        s.songs.insert("teardrop".into(), sound(0.0, "Teardrop"));
        s.songs.insert("roads".into(), sound(0.1, "Roads"));
        // In the store but not in this listener's Navidrome: never sent.
        s.songs.insert("elsewhere".into(), sound(0.05, "Elsewhere"));
    });

    let songs = radio(&f, "alice", "teardrop", 50).await;

    assert!(
        songs
            .iter()
            .any(|(id, by)| id == "roads" && by.as_deref() == Some("Sounds alike")),
        "{songs:?}"
    );
    assert!(!songs.iter().any(|(id, _)| id == "elsewhere"));
    assert!(
        f.calls().iter().any(|call| call.contains("rest/getSong")
            && call.contains("id=roads")
            && call.contains("u=alice"))
    );
}

#[tokio::test]
async fn the_sound_check_puts_a_library_song_that_sounds_close_above_one_that_sounds_far() {
    let f = fixture(|s| s.radio_sources.sounds_alike = true).await;
    f.state.sonic_store.write(|s| {
        s.songs.insert("spooky".into(), sound(0.0, "SPOOKY"));
        s.songs.insert("pl-3".into(), sound(0.05, "NIGHT RIDE"));
        s.songs.insert("genre-1".into(), sound(0.5, "GHOST"));
        s.songs.insert("pl-2".into(), sound(0.95, "DRIFT"));
        // The rest of the library, for the seed's spread.
        for i in 0..10 {
            s.songs
                .insert(format!("other-{i}"), sound(0.1 * i as f32, &format!("Other {i}")));
        }
    });

    let songs: Vec<String> = radio(&f, "alice", "spooky", 20)
        .await
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    // The playlist's two songs come back in either order from the album; the sound decides.
    let at = |id: &str| songs.iter().position(|s| s == id).expect("listed");
    assert!(at("pl-3") < at("pl-2"), "{songs:?}");
}

fn learning(s: &mut AppSettings) {
    s.radio_sources.you_tube_music = true;
    s.radio_sources.learn_from_listening = true;
}

async fn scrobble(f: &Fixture, user: &str, id: &str, finished: bool) {
    let reply = get(
        &f.app,
        &format!(
            "/rest/scrobble?id={}&submission={finished}&u={user}&t=token&s=salt&f=json",
            octo_core::common::dotnet::escape_data_string(id)
        ),
    )
    .await;
    assert!(reply.status.is_success(), "{}", reply.body);
}

#[tokio::test]
async fn a_radio_song_played_through_counts_for_the_source_that_suggested_it() {
    let f = fixture(learning).await;
    let songs = radio(&f, "alice", "spooky", 20).await;
    let ytm = songs
        .iter()
        .find(|(_, by)| by.as_deref() == Some("YouTube Music"))
        .expect("a YouTube Music song")
        .0
        .clone();
    // The test plays the song through at once; a real one is heard for more than 30 s.
    f.state
        .radio_outcomes
        .set_shortest_keep(chrono::TimeDelta::zero());

    scrobble(&f, "alice", &ytm, false).await;
    scrobble(&f, "alice", &ytm, true).await;

    let stats: Value =
        serde_json::from_str(&get_string(&f.app, "/api/admin/radio-outcomes").await).expect("JSON");
    let row = stats
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["provider"] == "youtube-music")
        .expect("a YouTube Music row");
    assert_eq!(row["plays"], 1);
    assert_eq!(row["keepRate"].as_f64(), Some(1.0));
}

#[tokio::test]
async fn a_scrobble_navidrome_refuses_teaches_nothing() {
    let f = fixture(learning).await;
    let songs = radio(&f, "mallory", "spooky", 20).await;
    let ytm = songs
        .iter()
        .find(|(_, by)| by.as_deref() == Some("YouTube Music"))
        .expect("a YouTube Music song")
        .0
        .clone();

    scrobble(&f, "mallory", &ytm, false).await;
    scrobble(&f, "mallory", &ytm, true).await;

    assert_eq!(get_string(&f.app, "/api/admin/radio-outcomes").await.trim(), "[]");
}

#[tokio::test]
async fn radio_state_files_stay_in_the_hosts_own_folder() {
    let f = fixture(learning).await;
    f.state.radio_outcomes.forget();
    f.state.sonic_store.write(|s| s.pass = 1);
    f.state.sonic_store.flush();
    assert!(f.dir.path().join("radio-outcomes.json").exists());
    assert!(f.dir.path().join("sonic-features.json").exists());
}

#[tokio::test]
async fn forgetting_what_radio_learned_needs_the_dashboards_header() {
    let f = fixture(learning).await;
    let refused = send(
        &f.app,
        Method::POST,
        "/api/admin/radio-outcomes/reset",
        &[],
        Body::empty(),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let allowed = send(
        &f.app,
        Method::POST,
        "/api/admin/radio-outcomes/reset",
        &[("X-Octo-Admin", "1")],
        Body::empty(),
    )
    .await;
    assert!(allowed.status.is_success(), "{}", allowed.body);
}

#[tokio::test]
async fn a_song_the_listener_rated_one_star_stays_off_their_radio() {
    let f = fixture(|_| {}).await;
    f.state
        .last_fm_radio_state
        .set_radio_ban("alice", "glory-box", "Portishead", "Glory Box", true);
    let songs = radio_ids(&f, "teardrop").await;
    assert!(!songs.contains(&"ext-Portishead-Glory Box".to_string()));
    assert!(songs.contains(&"ext-Morcheeba-Trigger Hippie".to_string()));
}

#[tokio::test]
async fn a_thin_exact_answer_is_filled_from_the_library() {
    let f = fixture(|_| {}).await;
    let songs = radio_ids(&f, "roads").await;
    // Last.fm knows the song but has two songs like it: too few to be the radio alone.
    for id in [
        "ext-Massive Attack-Angel",
        "ext-Massive Attack-Unfinished Sympathy",
        "dummy-2",
    ] {
        assert!(songs.contains(&id.to_string()), "{id} in {songs:?}");
    }
    assert!(
        f.calls()
            .iter()
            .any(|call| call.contains("rest/getAlbum") && call.contains("id=al-dummy"))
    );
}

#[tokio::test]
async fn nightcore_upload_tries_the_artist_its_title_names_and_keeps_its_playlist_first() {
    let f = fixture(|_| {}).await;
    let songs = radio_ids(&f, "faded-nightcore").await;
    // Last.fm knows "Alan Walker - Faded", the song this upload came from, and its songs are like
    // the original: they play, but behind the playlist the nightcore upload sits in.
    assert!(
        f.calls()
            .iter()
            .any(|call| call.contains("method=track.getsimilar") && call.contains("artist=Alan%20Walker"))
    );
    assert!(songs.contains(&"ext-Avicii-Wake Me Up".to_string()), "{songs:?}");
    assert!(songs[0].starts_with("pl-"), "{songs:?}");
}

#[tokio::test]
async fn radio_finds_a_copy_filed_under_another_name_and_bounds_its_library_searches() {
    let f = fixture(|_| {}).await;
    let songs = radio_ids(&f, "renamed").await;
    assert!(songs.contains(&"ye-wire".to_string()), "{songs:?}");
    assert!(!songs.contains(&"wire-cover".to_string()), "{songs:?}");

    let searches: Vec<String> = f
        .calls()
        .into_iter()
        .filter(|call| call.contains("/rest/search3"))
        .collect();
    let by_name = searches
        .iter()
        .filter(|call| call.contains("songCount=5"))
        .count();
    let by_title: Vec<&String> = searches
        .iter()
        .filter(|call| call.contains("songCount=20"))
        .collect();
    // At most one search per Last.fm song by artist and title, and the bare title once more only
    // for the two songs by an artist with other names: 25 songs, at most 27 searches.
    assert!(by_name <= 25, "{searches:?}");
    assert_eq!(by_title.len(), 2, "{by_title:?}");
    assert!(
        by_title
            .iter()
            .all(|call| call.contains("query=Through") || call.contains("query=Jesus")),
        "{by_title:?}"
    );
    assert_eq!(searches.len(), by_name + by_title.len(), "{searches:?}");
}

#[tokio::test]
async fn new_song_by_a_known_artist_still_gets_artists_like_them() {
    let f = fixture(|_| {}).await;
    // Not on Last.fm yet, but Drake is, with an audience: artists like him are no guess.
    assert_eq!(radio_ids(&f, "brand-new").await, ["ext-Future-Mask Off"]);
}

#[tokio::test]
async fn album_id_is_a_radio_from_its_first_song() {
    let f = fixture(|_| {}).await;
    let songs = radio_ids(&f, "album-trip").await;
    assert_eq!(
        songs[..2],
        ["ext-Portishead-Glory Box", "ext-Morcheeba-Trigger Hippie"]
    );
}

#[tokio::test]
async fn artist_id_is_the_top_songs_of_artists_like_them() {
    let f = fixture(|_| {}).await;
    assert_eq!(radio_ids(&f, "artist-massive").await, ["ext-Portishead-Roads"]);
}

#[test]
fn interleave_takes_one_from_each_in_turn() {
    use crate::services::last_fm::last_fm_radio_track_resolver::interleave;
    assert_eq!(interleave(&[vec![1, 2, 3], vec![10, 20]]), [1, 10, 2, 20, 3]);
}

/// L for a library song, O for one from outside it.
fn kinds(ids: &[String]) -> String {
    ids.iter()
        .map(|id| if id.starts_with("lib-") { 'L' } else { 'O' })
        .collect()
}

// Radio placement: Octo's own apps (octoMix=client) get the order of fit; every other app gets
// the same songs with the outside ones placed at random at the radio's own mix (here 20 of 30),
// each kind still in its own order. The Discovery balance, set to 90 here, plays no part. The draw is seeded, so the placed
// order repeats exactly.
#[tokio::test]
async fn other_apps_get_outside_songs_placed_and_octos_apps_get_the_order_of_fit() {
    let f = fixture(|settings| settings.last_fm.discovery_percent = 90).await;
    f.state.song_radio.set_randomizer(seeded_randomizer(7));
    async fn blend(f: &Fixture, extra: &str) -> Vec<String> {
        let body = get_string(
            &f.app,
            &format!("/rest/getSimilarSongs2?id=blend&u=alice&t=token&s=salt&f=json&count=30{extra}"),
        )
        .await;
        songs(&body).into_iter().map(|(id, _)| id).collect()
    }
    let fit: Vec<String> = (1..=30)
        .map(|i| {
            if i % 3 == 0 {
                format!("lib-band-{i}")
            } else {
                format!("ext-Band {i}-Tune {i}")
            }
        })
        .collect();

    // Every artist differs, so the order of fit is Last.fm's own order.
    assert_eq!(blend(&f, "&octoMix=client").await, fit);

    let mut sorted_fit = fit.clone();
    sorted_fit.sort();
    let of_kind = |ids: &[String], library: bool| -> Vec<String> {
        ids.iter()
            .filter(|id| id.starts_with("lib-") == library)
            .cloned()
            .collect()
    };
    let placed = blend(&f, "").await;
    let mut sorted = placed.clone();
    sorted.sort();
    assert_eq!(sorted, sorted_fit, "the same songs: {placed:?}");
    assert_eq!(of_kind(&placed, true), of_kind(&fit, true), "{placed:?}");
    assert_eq!(of_kind(&placed, false), of_kind(&fit, false), "{placed:?}");
    // Placed, not the order of fit: the kinds land elsewhere.
    let pattern = kinds(&placed);
    assert_ne!(pattern, kinds(&fit), "placed as the order of fit");
    // At the radio's own mix the library is the rarer kind (10 of 30): never three library songs
    // in a row, not even at the end, and the first ten hold some of both kinds.
    assert!(!pattern.contains("LLL"), "{pattern}");
    assert!(
        pattern[..10].contains('L') && pattern[..10].contains('O'),
        "{pattern}"
    );
    // The same seed, the same radio.
    assert_eq!(blend(&f, "").await, placed);
}

/// A seed with 120 songs like it on Last.fm, none of them in the library.
const WIDE_SONGS: usize = 120;

// "Ask for more songs": Octo's own apps (octoMix=client) may ask for up to 100 songs, whatever
// RadioTrackCount is (50 here, the default). Every other app is still cut to RadioTrackCount.
#[tokio::test]
async fn octos_apps_may_ask_for_100_songs_and_other_apps_get_the_configured_count() {
    let f = fixture(|_| {}).await;
    async fn wide(f: &Fixture, extra: &str) -> usize {
        let body = get_string(
            &f.app,
            &format!("/rest/getSimilarSongs2?id=wide&u=alice&t=token&s=salt&f=json&count=100{extra}"),
        )
        .await;
        songs(&body).len()
    }
    assert_eq!(wide(&f, "&octoMix=client").await, 100);
    assert_eq!(wide(&f, "").await, 50);
}

#[tokio::test]
async fn nothing_from_octos_sources_lets_navidrome_answer() {
    let f = fixture(|s| s.last_fm.api_key = String::new()).await;
    assert_eq!(radio_ids(&f, "teardrop").await, ["nd-1"]);
    let calls = f.calls();
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("/rest/getSimilarSongs2?")),
        "{calls:?}"
    );
}

// Octo's radio is on but finds nothing, and Navidrome's own agents failing too is still the
// empty radio, not an error.
#[tokio::test]
async fn navidromes_own_error_on_the_fallback_is_the_empty_radio() {
    let f = fixture(|_| {}).await;
    let body = get_string(
        &f.app,
        "/rest/getSimilarSongs2?id=quiet&u=alice&t=token&s=salt&f=json&count=20",
    )
    .await;
    let doc: Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(doc["subsonic-response"]["status"], "ok", "{body}");
    assert!(
        f.calls()
            .iter()
            .any(|call| call.starts_with("/rest/getSimilarSongs2?id=quiet")),
        "{:?}",
        f.calls()
    );
}

// Octo's radio is on but finds nothing, and Navidrome's own agents answer: that answer is the
// radio.
#[tokio::test]
async fn navidromes_own_answer_on_the_fallback_reaches_the_client() {
    let f = fixture(|_| {}).await;
    assert_eq!(radio_ids(&f, "hush").await, ["nd-1"]);
    assert!(
        f.calls()
            .iter()
            .any(|call| call.starts_with("/rest/getSimilarSongs2?") && call.contains("id=hush")),
        "{:?}",
        f.calls()
    );
}

// An outside artist (an artist row of a search) is a radio of the artists like them, as a
// library artist is, instead of an empty answer; asked again, Last.fm is not asked again.
#[tokio::test]
async fn an_outside_artist_is_the_top_songs_of_artists_like_them() {
    use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
    let f = fixture(|_| {}).await;
    let id = f.state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Artist,
        artist: Some("Massive Attack".into()),
        ..Default::default()
    });

    assert_eq!(radio_ids(&f, &id).await, ["ext-Portishead-Roads"]);
    assert_eq!(radio_ids(&f, &id).await, ["ext-Portishead-Roads"]);

    let asked = f
        .calls()
        .iter()
        .filter(|call| call.contains("method=artist.getsimilar") && call.contains("Massive"))
        .count();
    assert_eq!(asked, 1, "{:?}", f.calls());
}

// A scrobble starts resolving the next songs of a radio queue only once Navidrome has accepted
// the caller: one it refuses never makes Octo look a song up on YouTube.
#[tokio::test]
async fn a_scrobble_prewarms_the_queue_only_for_a_caller_navidrome_accepts() {
    use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
    let f = fixture(|_| {}).await;
    let outside = |title: &str| {
        f.state.external_id_registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("Queue Band".into()),
            title: Some(title.into()),
            duration: Some(200),
            ..Default::default()
        })
    };
    let playing = outside("Now Playing");
    let next = outside("Up Next");
    f.state.radio_queues.register([playing.clone(), next]);
    let looked_up = |f: &Fixture| {
        f.calls()
            .iter()
            .filter(|call| call.starts_with("/search?") && call.contains("Up"))
            .count()
    };

    scrobble(&f, "mallory", &playing, false).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(looked_up(&f), 0, "{:?}", f.calls());

    scrobble(&f, "alice", &playing, false).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while looked_up(&f) == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(looked_up(&f), 1, "{:?}", f.calls());
}
