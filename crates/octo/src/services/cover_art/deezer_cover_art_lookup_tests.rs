//! Covers for songs outside the library come from the catalog at full size. The lookup used
//! field-qualified searches the catalog no longer answers, so every one came back empty and a
//! smaller source served a soft 600 pixel cover instead.
//!
//! The fake catalog is a mock server: `/cdn/...` stands for the image CDN and answers with its
//! own path as the bytes; anything else is the API, answered by path.

use std::collections::HashMap;

use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::services::metadata::DeezerRateLimiter;

struct FakeCatalog {
    json: HashMap<&'static str, String>,
}

impl Respond for FakeCatalog {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path();
        if path.starts_with("/cdn/") {
            return ResponseTemplate::new(200).set_body_bytes(path.as_bytes().to_vec());
        }
        match self.json.get(path) {
            Some(body) => ResponseTemplate::new(200)
                .insert_header("content-type", "application/json; charset=utf-8")
                .set_body_string(body.clone()),
            None => ResponseTemplate::new(404),
        }
    }
}

/// The server, and how a CDN URL on it is written in the catalog's answers.
async fn catalog(
    json: impl FnOnce(&str) -> HashMap<&'static str, String>,
) -> (MockServer, DeezerCoverArtLookup) {
    let server = MockServer::builder().start().await;
    let cdn = format!("http://localhost:{}/cdn", server.address().port());
    Mock::given(any())
        .respond_with(FakeCatalog { json: json(&cdn) })
        .mount(&server)
        .await;
    let http = Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new())));
    let lookup = DeezerCoverArtLookup::with_base_url(http, &MetadataSettings::default(), &server.uri());
    (server, lookup)
}

async fn asked(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap_or_default()
}

fn q(request: &Request) -> String {
    request
        .url
        .query_pairs()
        .find(|(k, _)| k == "q")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

#[tokio::test]
async fn an_album_with_a_known_id_is_fetched_by_that_id() {
    let (server, lookup) = catalog(|cdn| {
        HashMap::from([(
            "/album/302127",
            format!(
                r#"{{"id":302127,"title":"Discovery","cover_xl":"{cdn}/discovery-1000.jpg","cover_big":"{cdn}/discovery-500.jpg"}}"#
            ),
        )])
    })
    .await;

    let bytes = lookup
        .try_fetch(
            &SoulseekRouting {
                kind: RoutingKind::Album,
                artist: Some("Daft Punk".into()),
                album: Some("Discovery".into()),
                external_album_id: Some("302127".into()),
                ..Default::default()
            },
            false,
        )
        .await
        .expect("a cover");

    assert_eq!(&bytes[..], b"/cdn/discovery-1000.jpg");
    assert!(
        !asked(&server)
            .await
            .iter()
            .any(|r| r.url.path().starts_with("/search"))
    );
}

#[tokio::test]
async fn a_search_is_plain_and_takes_the_asked_for_title() {
    let (server, lookup) = catalog(|cdn| {
        // The same artist's other record first, the asked-for one second.
        HashMap::from([(
            "/search/album",
            format!(
                r#"{{"data":[
                    {{"title":"Homework","artist":{{"name":"Daft Punk"}},"cover_xl":"{cdn}/homework.jpg"}},
                    {{"title":"Random Access Memories (Drumless Edition)","artist":{{"name":"Daft Punk"}},"cover_xl":"{cdn}/ram-drumless.jpg"}}
                  ]}}"#
            ),
        )])
    })
    .await;

    let bytes = lookup
        .try_fetch(
            &SoulseekRouting {
                kind: RoutingKind::Album,
                artist: Some("Daft Punk".into()),
                album: Some("Random Access Memories (Drumless Edition)".into()),
                ..Default::default()
            },
            false,
        )
        .await
        .expect("a cover");

    assert_eq!(&bytes[..], b"/cdn/ram-drumless.jpg");
    let requests = asked(&server).await;
    let searches: Vec<&Request> = requests
        .iter()
        .filter(|r| r.url.path() == "/search/album")
        .collect();
    assert_eq!(searches.len(), 1);
    let q = q(searches[0]);
    assert!(!q.contains("artist:"));
    assert!(!q.contains("album:"));
    assert!(q.contains("Daft Punk"));
}

#[tokio::test]
async fn a_songs_cover_comes_from_a_plain_track_search() {
    let (server, lookup) = catalog(|cdn| {
        HashMap::from([(
            "/search",
            format!(
                r#"{{"data":[
                    {{"title":"Something Else","artist":{{"name":"Air"}},"album":{{"title":"Talkie Walkie","cover_xl":"{cdn}/other.jpg"}}}},
                    {{"title":"Sexy Boy","artist":{{"name":"Air"}},"album":{{"title":"Moon Safari","cover_xl":"{cdn}/moon-safari.jpg"}}}}
                  ]}}"#
            ),
        )])
    })
    .await;

    let bytes = lookup
        .try_fetch(
            &SoulseekRouting {
                kind: RoutingKind::Song,
                artist: Some("Air".into()),
                title: Some("Sexy Boy".into()),
                ..Default::default()
            },
            false,
        )
        .await
        .expect("a cover");

    assert_eq!(&bytes[..], b"/cdn/moon-safari.jpg");
    let requests = asked(&server).await;
    let searches: Vec<&Request> = requests.iter().filter(|r| r.url.path() == "/search").collect();
    assert_eq!(searches.len(), 1);
    assert!(!q(searches[0]).contains("track:"));
}

/// The Accept-Language the lookup was built with goes on every request, and an artist's picture
/// is taken from the best-named hit.
#[tokio::test]
async fn an_artists_picture_is_the_best_named_hit_and_the_language_is_sent() {
    let server = MockServer::builder().start().await;
    let cdn = format!("http://localhost:{}/cdn", server.address().port());
    Mock::given(any())
        .respond_with(FakeCatalog {
            json: HashMap::from([(
                "/search/artist",
                format!(
                    r#"{{"data":[{{"name":"Air Supply","picture_xl":"{cdn}/supply.jpg"}},{{"name":"Air","picture_big":"{cdn}/air.jpg"}}]}}"#
                ),
            )]),
        })
        .mount(&server)
        .await;
    let http = Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new())));
    let metadata = MetadataSettings {
        language: "fr-FR, fr".into(),
        ..Default::default()
    };
    let lookup = DeezerCoverArtLookup::with_base_url(http, &metadata, &server.uri());

    let bytes = lookup
        .try_fetch(
            &SoulseekRouting {
                kind: RoutingKind::Artist,
                artist: Some("Air".into()),
                ..Default::default()
            },
            true,
        )
        .await
        .expect("a picture");

    assert_eq!(&bytes[..], b"/cdn/air.jpg");
    for request in asked(&server).await {
        assert_eq!(
            request
                .headers
                .get("accept-language")
                .and_then(|v| v.to_str().ok()),
            Some("fr-FR, fr")
        );
    }
}

/// Deezer's stand-in for an artist with no picture (an empty hash in its address) is no picture:
/// a real picture of another hit of the same name wins.
#[tokio::test]
async fn deezers_stand_in_is_no_picture() {
    let (_server, lookup) = catalog(|cdn| {
        HashMap::from([(
            "/search/artist",
            format!(
                r#"{{"data":[{{"name":"Linkin Park","picture_xl":"{cdn}/images/artist//1000x1000-000000-80-0-0.jpg"}},{{"name":"Linkin Park","picture_xl":"{cdn}/images/artist/4886905210739af3438990897bad3a98/1000x1000-000000-80-0-0.jpg"}}]}}"#
            ),
        )])
    })
    .await;
    let ask = |artist: &str| SoulseekRouting {
        kind: RoutingKind::Artist,
        artist: Some(artist.into()),
        ..Default::default()
    };

    let bytes = lookup.try_fetch(&ask("Linkin Park"), false).await;
    assert_eq!(
        bytes.as_deref(),
        Some(&b"/cdn/images/artist/4886905210739af3438990897bad3a98/1000x1000-000000-80-0-0.jpg"[..])
    );
}

/// With only Deezer's stand-in to offer, an artist gets no picture at all.
#[tokio::test]
async fn only_the_stand_in_finds_nothing() {
    let (_server, lookup) = catalog(|cdn| {
        HashMap::from([(
            "/search/artist",
            format!(
                r#"{{"data":[{{"name":"LINKIN PARK x STEVE AOKI","picture_xl":"{cdn}/images/artist//1000x1000-000000-80-0-0.jpg","picture":"{cdn}/artist/5181383/image"}}]}}"#
            ),
        )])
    })
    .await;

    let bytes = lookup
        .try_fetch(
            &SoulseekRouting {
                kind: RoutingKind::Artist,
                artist: Some("LINKIN PARK x STEVE AOKI".into()),
                ..Default::default()
            },
            false,
        )
        .await;
    assert!(bytes.is_none());
}

/// An artist's picture goes to the hit that names the artist, in any script, never to a longer
/// name that only starts with the same letters.
#[tokio::test]
async fn an_artists_picture_is_never_a_name_that_only_starts_the_same() {
    let (_server, lookup) = catalog(|cdn| {
        HashMap::from([(
            "/search/artist",
            format!(
                r#"{{"data":[{{"name":"Kinoman","picture_xl":"{cdn}/kinoman.jpg"}},{{"name":"Кино","picture_xl":"{cdn}/kino.jpg"}}]}}"#
            ),
        )])
    })
    .await;

    let bytes = lookup
        .try_fetch(
            &SoulseekRouting {
                kind: RoutingKind::Artist,
                artist: Some("Kino".into()),
                ..Default::default()
            },
            false,
        )
        .await
        .expect("a picture");

    assert_eq!(&bytes[..], b"/cdn/kino.jpg");
}

/// A song search takes the hit credited to the artist written another way over one by an artist
/// whose name merely holds the same letters.
#[tokio::test]
async fn a_songs_cover_follows_the_artist_written_another_way() {
    let (_server, lookup) = catalog(|cdn| {
        HashMap::from([(
            "/search",
            format!(
                r#"{{"data":[
                    {{"title":"Despacito","artist":{{"name":"Luis Fonsiman"}},"album":{{"title":"Covers","cover_xl":"{cdn}/other.jpg"}}}},
                    {{"title":"Despacito","artist":{{"name":"Fonsi, Luis"}},"album":{{"title":"Vida","cover_xl":"{cdn}/vida.jpg"}}}}
                  ]}}"#
            ),
        )])
    })
    .await;

    let bytes = lookup
        .try_fetch(
            &SoulseekRouting {
                kind: RoutingKind::Song,
                artist: Some("Luis Fonsi".into()),
                title: Some("Despacito".into()),
                ..Default::default()
            },
            false,
        )
        .await
        .expect("a cover");

    assert_eq!(&bytes[..], b"/cdn/vida.jpg");
}

/// "Still Got It" by Tyga feat. Drake, on the "I'm On One 3" mixtape with album artist Drake:
/// the catalog has no such album, and its track search for the album's name offers DJ Khaled's
/// "I'm On One" on "We The Best Forever". That is another artist's release of another name, so
/// no cover is taken from it.
#[tokio::test]
async fn another_artists_release_of_a_near_name_is_never_the_albums_cover() {
    let (_server, lookup) = catalog(|cdn| {
        HashMap::from([
            ("/search/album", r#"{"data":[]}"#.to_string()),
            (
                "/search",
                format!(
                    r#"{{"data":[
                        {{"title":"I'm On One (Explicit Version)","artist":{{"name":"DJ Khaled"}},
                          "album":{{"title":"We The Best Forever","cover_xl":"{cdn}/we-the-best-forever.jpg"}}}}
                      ]}}"#
                ),
            ),
        ])
    })
    .await;

    let bytes = lookup
        .try_fetch(
            &SoulseekRouting {
                kind: RoutingKind::Album,
                artist: Some("Drake".into()),
                title: Some("Still Got It".into()),
                album: Some("I'm On One 3".into()),
                ..Default::default()
            },
            false,
        )
        .await;

    assert_eq!(bytes, None);
}

/// The same search does give the album's own cover when the catalog has it, by its own artist.
/// While Apple answers Deezer's hidden searches, a cover by name is left to the iTunes source next
/// in the chain (asking here too would ask Apple twice); a cover by a known Deezer id is still
/// fetched from Deezer.
#[tokio::test]
async fn while_apple_stands_in_names_are_left_to_itunes_and_ids_still_fetch() {
    use crate::services::apple::{AppleCatalog, ApplePace, PaceConfig};
    use crate::services::metadata::{DeezerReach, DeezerStandIn, DoorInfos};

    let server = MockServer::builder().start().await;
    let cdn = format!("http://localhost:{}/cdn", server.address().port());
    Mock::given(any())
        .respond_with(FakeCatalog {
            json: HashMap::from([
                (
                    "/infos",
                    r#"{"country_iso":"IN","country":"India","open":false}"#.to_string(),
                ),
                (
                    "/album/302127",
                    format!(r#"{{"id":302127,"title":"Discovery","cover_xl":"{cdn}/discovery-1000.jpg"}}"#),
                ),
            ]),
        })
        .mount(&server)
        .await;
    let pace = Arc::new(ApplePace::new(PaceConfig::unpaced()));
    let reach = Arc::new(DeezerReach::new(Arc::clone(&pace)));
    let apple = Arc::new(AppleCatalog::with_bases(
        pace,
        "http://127.0.0.1:0",
        "http://127.0.0.1:0",
        None,
    ));
    let stand_in = Arc::new(DeezerStandIn::new(Arc::clone(&reach), apple));
    let http = Arc::new(DeezerRateLimitHandler::with_api_host(
        Arc::new(DeezerRateLimiter::new()),
        "127.0.0.1",
    ));
    stand_in.set_door(Arc::downgrade(&http));
    http.set_stand_in(Arc::clone(&stand_in));
    reach.set_reader(Arc::new(DoorInfos::new(Arc::downgrade(&http), &server.uri())));
    reach.read_infos().await;
    assert!(reach.stands_in());
    let lookup = DeezerCoverArtLookup::with_base_url(http, &MetadataSettings::default(), &server.uri());

    for routing in [
        SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("Daft Punk".into()),
            title: Some("One More Time".into()),
            ..Default::default()
        },
        SoulseekRouting {
            kind: RoutingKind::Album,
            artist: Some("Daft Punk".into()),
            album: Some("Homework".into()),
            ..Default::default()
        },
    ] {
        assert_eq!(lookup.try_fetch(&routing, false).await, None);
    }
    assert!(
        !asked(&server)
            .await
            .iter()
            .any(|r| r.url.path().starts_with("/search")),
        "no name search was sent"
    );
    let by_id = lookup
        .try_fetch(
            &SoulseekRouting {
                kind: RoutingKind::Album,
                artist: Some("Daft Punk".into()),
                album: Some("Discovery".into()),
                external_album_id: Some("302127".into()),
                ..Default::default()
            },
            false,
        )
        .await
        .expect("a cover by its Deezer id");
    assert_eq!(&by_id[..], b"/cdn/discovery-1000.jpg");
}

#[tokio::test]
async fn the_albums_own_release_by_its_artist_is_taken() {
    let (_server, lookup) = catalog(|cdn| {
        HashMap::from([(
            "/search/album",
            format!(
                r#"{{"data":[
                    {{"title":"I'm On One","artist":{{"name":"DJ Khaled"}},"cover_xl":"{cdn}/khaled.jpg"}},
                    {{"title":"I'm On One 3","artist":{{"name":"Drake"}},"cover_xl":"{cdn}/mixtape.jpg"}}
                  ]}}"#
            ),
        )])
    })
    .await;

    let bytes = lookup
        .try_fetch(
            &SoulseekRouting {
                kind: RoutingKind::Album,
                artist: Some("Drake".into()),
                album: Some("I'm On One 3".into()),
                ..Default::default()
            },
            false,
        )
        .await
        .expect("a cover");

    assert_eq!(&bytes[..], b"/cdn/mixtape.jpg");
}
