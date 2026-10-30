//! The MusicBrainz client: its cache, escaping and lanes, the heart lane's retries, the re-asker
//! and a heart's questions. Reading release details and candidate sources is tested with the
//! tagging code; the query builders and the pick are tested in `octo_core`.

use std::path::{Path, PathBuf};

use octo_core::fingerprint::heart_release::{RecordedAnswer, RecordedAnswers, VARIOUS_ARTISTS_ID};
use tokio::sync::mpsc;
use wiremock::matchers::{any, path, query_param};
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use super::*;

const NIGHT_AT_THE_OPERA: &str = r#"
{
  "id": "r-nato", "title": "A Night at the Opera", "status": "Official", "date": "1975-11-21", "country": "GB",
  "barcode": "077778949224", "quality": "normal",
  "label-info": [{"catalog-number": "EMTC 103", "label": {"id": "l-emi", "name": "EMI"}}],
  "release-group": {"id": "g-nato", "title": "A Night at the Opera", "primary-type": "Album",
    "secondary-types": [], "first-release-date": "1975-11-21"},
  "artist-credit": [{"name": "Queen", "joinphrase": "", "artist": {"id": "a-queen", "name": "Queen"}}],
  "media": [{"position": 1, "format": "12\" Vinyl", "track-count": 12,
    "tracks": [
      {"id": "t-1", "position": 1, "number": "A1", "title": "Death on Two Legs", "length": 223000,
       "recording": {"id": "rec-dotl", "title": "Death on Two Legs (Dedicated to...)", "length": 223000, "isrcs": ["GBUM71029604"]}},
      {"id": "t-11", "position": 11, "number": "B5", "title": "Bohemian Rhapsody", "length": 355000,
       "recording": {"id": "rec-br", "title": "Bohemian Rhapsody", "length": 355000, "isrcs": ["GBUM71029604", "GBUM71029605"]}}
    ]}],
  "genres": [{"name": "rock", "count": 12}, {"name": "progressive rock", "count": 5}, {"name": "glam rock", "count": 1}]
}
"#;

async fn client(answer: ResponseTemplate) -> (MusicBrainzClient, MockServer) {
    let server = MockServer::builder().start().await;
    Mock::given(any()).respond_with(answer).mount(&server).await;
    let base = Url::parse(&format!("{}/ws/2/", server.uri())).expect("parses");
    (MusicBrainzClient::with_base_url(base), server)
}

async fn calls(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.url.to_string())
        .collect()
}

#[tokio::test]
async fn lookup_release_asks_once_then_answers_from_memory() {
    let (client, server) = client(ResponseTemplate::new(200).set_body_string(NIGHT_AT_THE_OPERA)).await;

    let first = client
        .lookup_release("r-nato")
        .await
        .expect("no timeout")
        .expect("found");
    let second = client
        .lookup_release("r-nato")
        .await
        .expect("no timeout")
        .expect("found");

    assert!(Arc::ptr_eq(&first, &second));
    let calls = calls(&server).await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains(
        "release/r-nato?inc=labels+release-groups+artist-credits+recordings+isrcs+genres&fmt=json"
    ));
    assert!(calls[0].contains("/ws/2/release/"));
    // MusicBrainz wants a User-Agent naming the application.
    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        requests[0]
            .headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok()),
        Some(octo_user_agent::value())
    );
}

#[tokio::test]
async fn lookup_release_server_error_is_null_and_not_remembered() {
    let (client, server) = client(ResponseTemplate::new(503)).await;

    assert!(
        client
            .lookup_release("r-nato")
            .await
            .expect("no timeout")
            .is_none()
    );
    assert!(
        client
            .lookup_release("r-nato")
            .await
            .expect("no timeout")
            .is_none()
    );
    assert_eq!(calls(&server).await.len(), 2);
}

#[tokio::test]
async fn search_recordings_is_remembered_for_the_same_song() {
    let (client, server) = client(ResponseTemplate::new(200).set_body_string(r#"{"recordings": []}"#)).await;

    let first = client
        .search_recordings("Portishead", "Glory Box (Live)", 300)
        .await
        .expect("no timeout");
    let second = client
        .search_recordings("Portishead", "Glory Box (Live)", 300)
        .await
        .expect("no timeout");

    assert!(first.is_some());
    assert!(second.is_some());
    assert_eq!(calls(&server).await.len(), 1);
}

/// The second request waits out the one-a-second gap.
#[tokio::test]
async fn requests_are_spaced_a_second_apart() {
    let (client, _server) =
        client(ResponseTemplate::new(200).set_body_string(r#"{"isrcs": ["GBUM71029604"]}"#)).await;

    let start = Instant::now();
    assert_eq!(
        client.fetch_isrcs("rec-1").await.expect("no timeout"),
        Some(vec!["GBUM71029604".to_string()])
    );
    client.fetch_isrcs("rec-2").await.expect("no timeout");
    assert!(Instant::now() - start >= Duration::from_millis(1100));
}

// ---- Where an artist is from, on the background lane ----------------------------------------

const ROSALIA: &str =
    r#"{"id":"a-rosalia","name":"Rosalía","country":"ES","area":{"name":"Spain","iso-3166-1-codes":["ES"]}}"#;

#[tokio::test]
async fn an_artist_lookup_reads_the_country_and_remembers_it() {
    let (client, server) = client(ResponseTemplate::new(200).set_body_string(ROSALIA)).await;

    let first = client.artist_country("A-Rosalia").await.expect("no timeout");
    let second = client.artist_country("a-rosalia").await.expect("no timeout");

    assert_eq!(first.as_deref(), Some("ES"));
    assert_eq!(second.as_deref(), Some("ES"));
    let calls = calls(&server).await;
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].ends_with("/ws/2/artist/A-Rosalia?fmt=json"),
        "{}",
        calls[0]
    );
}

#[tokio::test]
async fn an_artist_search_that_could_not_be_asked_is_none_and_not_remembered() {
    let (client, server) = client(ResponseTemplate::new(503)).await;

    assert_eq!(
        client.search_artist_country("Rosalía").await.expect("no timeout"),
        None
    );
    assert_eq!(
        client.search_artist_country("Rosalía").await.expect("no timeout"),
        None
    );

    let calls = calls(&server).await;
    assert_eq!(calls.len(), 2);
    assert!(
        calls[0].contains("/ws/2/artist/?query=artist%3A%22Rosal"),
        "{}",
        calls[0]
    );
}

#[tokio::test]
async fn background_lookups_step_aside_so_a_download_check_waits_one_gap_at_most() {
    let (client, _server) = client(ResponseTemplate::new(200).set_body_string(ROSALIA)).await;
    let client = Arc::new(client);
    let background = {
        let client = Arc::clone(&client);
        tokio::spawn(async move {
            for i in 0..20 {
                let _ = client.artist_country(&format!("artist-{i}")).await;
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;

    // A download being verified asks three questions in a row.
    let started = std::time::Instant::now();
    client.fetch_isrcs("rec-1").await.expect("no timeout");
    let first = started.elapsed();
    client.fetch_isrcs("rec-2").await.expect("no timeout");
    client.fetch_isrcs("rec-3").await.expect("no timeout");
    let all = started.elapsed();
    background.abort();

    let gap = MusicBrainzClient::MINIMUM_GAP;
    assert!(first < gap + Duration::from_millis(400), "first {first:?}");
    // One gap behind the background's last call, then one gap each: never behind its queue.
    assert!(all < gap * 3 + Duration::from_millis(900), "all {all:?}");
}

// ---- artists by name, and recording credits ------------------------------------------------

#[tokio::test]
async fn find_artists_named_asks_once_and_remembers_the_answer() {
    let body = r#"{"artists":[{"id":"a1","name":"Earth, Wind & Fire"},{"id":"a2","name":"Earth, Wind & Fire Experience"}]}"#;
    let (client, server) = client(ResponseTemplate::new(200).set_body_string(body)).await;

    assert!(client.known_artists_named("Earth, Wind & Fire").is_none());
    let first = client
        .find_artists_named("Earth, Wind & Fire")
        .await
        .expect("no timeout")
        .expect("asked");
    let second = client
        .find_artists_named("EARTH, WIND & FIRE")
        .await
        .expect("no timeout")
        .expect("asked");

    assert_eq!(first.len(), 1);
    assert_eq!(second, first);
    assert_eq!(calls(&server).await.len(), 1);
    assert!(client.known_artists_named("earth, wind & fire").is_some());
}

/// A failed request is "could not ask", never "no such artist", and is not remembered.
#[tokio::test]
async fn find_artists_named_failure_is_none_and_not_remembered() {
    let (client, _server) = client(ResponseTemplate::new(503)).await;

    assert!(
        client
            .find_artists_named("Bizarrap")
            .await
            .expect("no timeout")
            .is_none()
    );
    assert!(client.known_artists_named("Bizarrap").is_none());
}

#[tokio::test]
async fn recording_credits_answer_every_id_in_one_search() {
    let body = r#"{"recordings":[{"id":"11111111-2222-3333-4444-555555555555","artist-credit":[{"name":"Bizarrap","joinphrase":", "},{"name":"Rauw Alejandro"}]}]}"#;
    let (client, server) = client(ResponseTemplate::new(200).set_body_string(body)).await;
    let ids = vec![
        "11111111-2222-3333-4444-555555555555".to_string(),
        "99999999-2222-3333-4444-555555555555".to_string(),
    ];

    let credits = client
        .recording_credits(&ids)
        .await
        .expect("no timeout")
        .expect("asked");
    let again = client
        .recording_credits(&ids)
        .await
        .expect("no timeout")
        .expect("asked");

    assert_eq!(credits[&ids[0]].len(), 2);
    assert_eq!(credits[&ids[0]][1].join_phrase, "");
    assert!(credits[&ids[1]].is_empty());
    assert_eq!(again, credits);
    assert_eq!(calls(&server).await.len(), 1);
}

#[test]
fn the_kept_answers_are_dropped_before_they_outgrow_their_bound() {
    let mut map: HashMap<String, u32> = (0..3).map(|i| (i.to_string(), i)).collect();
    make_room(&mut map, 1, 4);
    assert_eq!(map.len(), 3, "room for one more");
    make_room(&mut map, 2, 4);
    assert!(map.is_empty(), "no room for two: the old answers go");
    assert_eq!(KNOWN_MAX, 10_000);
}

// ---- codes ---------------------------------------------------------------------------------

fn recorded(name: &str) -> String {
    std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/musicbrainz/").to_owned() + name,
    )
    .expect("a recorded MusicBrainz answer")
}

fn json_answer(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "application/json")
}

/// A server answering the search by code and the code lookup for one code.
async fn code_server(
    code: &str,
    search: ResponseTemplate,
    lookup: ResponseTemplate,
) -> (MusicBrainzClient, MockServer) {
    let server = MockServer::builder().start().await;
    Mock::given(path("/ws/2/recording"))
        .and(query_param("query", format!("isrc:{code}")))
        .respond_with(search)
        .mount(&server)
        .await;
    Mock::given(path(format!("/ws/2/isrc/{code}")))
        .respond_with(lookup)
        .mount(&server)
        .await;
    let base = Url::parse(&format!("{}/ws/2/", server.uri())).expect("parses");
    (MusicBrainzClient::with_base_url(base), server)
}

fn ids(doc: &serde_json::Value) -> Vec<String> {
    doc["recordings"]
        .as_array()
        .expect("recordings")
        .iter()
        .map(|r| r["id"].as_str().expect("an id").to_string())
        .collect()
}

#[tokio::test]
async fn lookup_isrc_asks_a_search_by_code_and_reads_releases_and_their_groups() {
    let (client, server) = code_server(
        "GBA077700130",
        json_answer(recorded("isrc-search-GBA077700130.json")),
        ResponseTemplate::new(500),
    )
    .await;

    let IsrcLookup::Found(doc) = client.lookup_isrc("gb-a07-77-00130").await.expect("no timeout") else {
        panic!("found");
    };
    assert_eq!(doc["isrc"], "GBA077700130");
    assert_eq!(doc["recordings"].as_array().map(Vec::len), Some(3));
    let candidates = octo_core::tagging::candidate_sources::CandidateSources::from_isrc_lookup(&doc);
    assert!(
        candidates
            .iter()
            .any(|c| c.release_group_id.is_some() && c.primary_type.is_some() && c.track_number.is_some()),
        "{candidates:?}"
    );
    // Remembered: a second ask goes nowhere.
    client.lookup_isrc("GBA077700130").await.expect("no timeout");
    let asked = calls(&server).await;
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(
        asked[0].ends_with("/ws/2/recording?query=isrc:GBA077700130&limit=100&fmt=json"),
        "{asked:?}"
    );
    assert!(!asked[0].contains("release-groups"));
}

#[tokio::test]
async fn lookup_isrc_leaves_out_recordings_the_search_lists_without_the_code() {
    let answer = serde_json::json!({"recordings": [
        {"id": "with", "title": "Song", "isrcs": ["GBA077700130"]},
        {"id": "without", "title": "Song", "isrcs": ["USXXX0000001"]},
        {"id": "none", "title": "Song"}
    ]});
    let (client, _server) = code_server(
        "GBA077700130",
        json_answer(answer.to_string()),
        ResponseTemplate::new(500),
    )
    .await;
    let IsrcLookup::Found(doc) = client.lookup_isrc("GBA077700130").await.expect("no timeout") else {
        panic!("found");
    };
    assert_eq!(ids(&doc), ["with"]);
}

#[tokio::test]
async fn the_tagger_asks_a_code_once_and_a_code_nobody_knows_is_not_known_and_not_remembered() {
    let (client, server) = code_server(
        "ZZZZZ0000001",
        json_answer(recorded("search-empty.json")),
        ResponseTemplate::new(404).set_body_raw(recorded("isrc-not-found.json"), "application/json"),
    )
    .await;
    assert_eq!(
        client.lookup_isrc("ZZZZZ0000001").await.expect("no timeout"),
        IsrcLookup::NotKnown
    );
    assert_eq!(calls(&server).await.len(), 1, "the tagger asks the search only");
    assert_eq!(
        client.lookup_isrc("ZZZZZ0000001").await.expect("no timeout"),
        IsrcLookup::NotKnown
    );
    assert_eq!(calls(&server).await.len(), 2, "not known is not remembered");
}

#[tokio::test]
async fn a_code_answer_keeps_at_most_25_releases_per_recording_and_the_same_candidates() {
    let raw: serde_json::Value =
        serde_json::from_str(&recorded("isrc-search-USSM16401419.json")).expect("JSON");
    assert!(
        raw["recordings"][0]["releases"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0)
            > 25
    );
    let (client, _server) = code_server(
        "USSM16401419",
        json_answer(recorded("isrc-search-USSM16401419.json")),
        ResponseTemplate::new(500),
    )
    .await;
    let IsrcLookup::Found(doc) = client.lookup_isrc("USSM16401419").await.expect("no timeout") else {
        panic!("found");
    };
    for recording in doc["recordings"].as_array().expect("recordings") {
        assert!(recording["releases"].as_array().map(Vec::len).unwrap_or(0) <= 25);
    }
    let mut whole = raw.clone();
    whole
        .as_object_mut()
        .expect("an object")
        .insert("isrc".into(), serde_json::Value::String("USSM16401419".into()));
    use octo_core::tagging::candidate_sources::CandidateSources;
    assert_eq!(
        CandidateSources::from_isrc_lookup(&doc),
        CandidateSources::from_isrc_lookup(&whole)
    );
}

#[tokio::test]
async fn a_dj_mix_segment_the_code_lists_is_left_out() {
    let (client, server) = code_server(
        "USSM19803320",
        json_answer(recorded("search-empty.json")),
        json_answer(recorded("isrc-lookup-USSM19803320.json")),
    )
    .await;
    let IsrcLookup::Found(doc) = client
        .isrc_recordings_in_background("USSM19803320")
        .await
        .expect("no timeout")
    else {
        panic!("found");
    };
    assert_eq!(ids(&doc), ["384f304d-5539-4cbf-920b-cfa8e69f795e"]);
    // The code lookup alone: the search is not asked for the matcher.
    let asked = calls(&server).await;
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(
        asked[0].ends_with("/ws/2/isrc/USSM19803320?inc=artist-credits&fmt=json"),
        "{asked:?}"
    );
}

#[test]
fn a_dj_mix_segment_is_read_however_its_hyphen_is_written() {
    let said = |text: &str| serde_json::json!({ "disambiguation": text });
    for segment in [
        "part of \u{201c}NOW 80s\u{201d} DJ\u{2010}mix",
        "part of X DJ-mix",
        "part of X DJ mix",
        "dj mix",
    ] {
        assert!(is_dj_mix_segment(&said(segment)), "{segment}");
    }
    for take in [
        "2015 stereo mix",
        "remix",
        "original acoustic version, stereo mix",
        "",
    ] {
        assert!(!is_dj_mix_segment(&said(take)), "{take}");
    }
}

#[tokio::test]
async fn the_matcher_gets_every_recording_the_code_lookup_lists() {
    let lookup = serde_json::json!({"isrc": "GBA077700130", "recordings": [
        {"id": "a", "title": "Song"},
        {"id": "b", "title": "Song"}
    ]});
    let (client, server) = code_server(
        "GBA077700130",
        ResponseTemplate::new(500),
        json_answer(lookup.to_string()),
    )
    .await;
    let IsrcLookup::Found(doc) = client
        .isrc_recordings_in_background("GBA077700130")
        .await
        .expect("no timeout")
    else {
        panic!("found");
    };
    assert_eq!(ids(&doc), ["a", "b"]);
    // Remembered: asked once.
    client
        .isrc_recordings_in_background("GBA077700130")
        .await
        .expect("no timeout");
    let asked = calls(&server).await;
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(
        asked[0].ends_with("/ws/2/isrc/GBA077700130?inc=artist-credits&fmt=json"),
        "{asked:?}"
    );
}

#[tokio::test]
async fn the_matchers_code_lookup_tells_each_kind_of_answer() {
    let (busy, _a) = code_server(
        "GBA077700130",
        ResponseTemplate::new(500),
        ResponseTemplate::new(503),
    )
    .await;
    assert_eq!(
        busy.isrc_recordings_in_background("GBA077700130")
            .await
            .expect("no timeout"),
        IsrcLookup::Unanswered
    );
    let (refused, _b) = code_server(
        "GBA077700130",
        ResponseTemplate::new(500),
        ResponseTemplate::new(400).set_body_raw(recorded("isrc-refused.json"), "application/json"),
    )
    .await;
    assert_eq!(
        refused
            .isrc_recordings_in_background("GBA077700130")
            .await
            .expect("no timeout"),
        IsrcLookup::Refused
    );
    let (unknown, _c) = code_server(
        "ZZZZZ0000001",
        ResponseTemplate::new(500),
        ResponseTemplate::new(404).set_body_raw(recorded("isrc-not-found.json"), "application/json"),
    )
    .await;
    assert_eq!(
        unknown
            .isrc_recordings_in_background("ZZZZZ0000001")
            .await
            .expect("no timeout"),
        IsrcLookup::NotKnown
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_question_musicbrainz_refuses_is_refused_and_logged_as_a_warning() {
    let (logs, _guard) = crate::services::test_support::LogCapture::start();
    let (client, server) = code_server(
        "GBA077700130",
        ResponseTemplate::new(400).set_body_raw(recorded("isrc-refused.json"), "application/json"),
        ResponseTemplate::new(500),
    )
    .await;
    assert_eq!(
        client.lookup_isrc("GBA077700130").await.expect("no timeout"),
        IsrcLookup::Refused
    );
    assert_eq!(calls(&server).await.len(), 1);
    let warnings = logs.at(tracing::Level::WARN);
    assert!(
        warnings
            .iter()
            .any(|line| line.contains("refused the code search question")
                && line.contains("not a valid inc parameter")),
        "{warnings:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn an_id_musicbrainz_calls_invalid_is_nothing_and_never_warned() {
    let (logs, _guard) = crate::services::test_support::LogCapture::start();
    let (client, _server) = client(
        ResponseTemplate::new(400).set_body_raw(recorded("release-invalid-mbid.json"), "application/json"),
    )
    .await;
    assert!(
        client
            .lookup_release("not-an-mbid")
            .await
            .expect("no timeout")
            .is_none()
    );
    assert!(
        logs.at(tracing::Level::WARN).is_empty(),
        "{:?}",
        logs.at(tracing::Level::WARN)
    );
}

#[tokio::test]
async fn a_busy_musicbrainz_is_unanswered_and_asked_again_next_time() {
    let server = MockServer::builder().start().await;
    Mock::given(path("/ws/2/recording"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/ws/2/recording"))
        .respond_with(json_answer(recorded("isrc-search-GBA077700130.json")))
        .mount(&server)
        .await;
    let base = Url::parse(&format!("{}/ws/2/", server.uri())).expect("parses");
    let client = MusicBrainzClient::with_base_url(base);
    assert_eq!(
        client.lookup_isrc("GBA077700130").await.expect("no timeout"),
        IsrcLookup::Unanswered
    );
    assert!(matches!(
        client.lookup_isrc("GBA077700130").await.expect("no timeout"),
        IsrcLookup::Found(_)
    ));
}

#[tokio::test]
async fn the_matchers_code_questions_step_aside_so_a_download_check_waits_one_gap_at_most() {
    let (client, _server) = client(json_answer(recorded("search-empty.json"))).await;
    let client = Arc::new(client);
    // A download check has just asked: the gate's last call is a foreground one.
    client
        .fetch_isrcs("11111111-1111-4111-8111-111111111111")
        .await
        .expect("no timeout");
    let background = {
        let client = Arc::clone(&client);
        tokio::spawn(async move {
            for i in 0..20 {
                let _ = client
                    .isrc_recordings_in_background(&format!("ZZZZZ00000{i:02}"))
                    .await;
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    let started = std::time::Instant::now();
    client
        .fetch_isrcs("22222222-2222-4222-8222-222222222222")
        .await
        .expect("no timeout");
    let waited = started.elapsed();
    background.abort();

    let gap = MusicBrainzClient::MINIMUM_GAP;
    assert!(waited < gap + Duration::from_millis(400), "waited {waited:?}");
}

#[tokio::test]
async fn the_tagger_hears_each_kind_of_code_answer() {
    use octo_core::tagging::ReleaseLookup;

    let (busy, _a) = code_server(
        "GBA077700130",
        ResponseTemplate::new(503),
        ResponseTemplate::new(500),
    )
    .await;
    assert!(ReleaseLookup::lookup_isrc(&busy, "GBA077700130").await.is_err());

    let (refused, _b) = code_server(
        "GBA077700130",
        ResponseTemplate::new(400).set_body_raw(recorded("isrc-refused.json"), "application/json"),
        ResponseTemplate::new(500),
    )
    .await;
    assert!(
        ReleaseLookup::lookup_isrc(&refused, "GBA077700130")
            .await
            .is_err()
    );

    let (unknown, server) = code_server(
        "ZZZZZ0000001",
        json_answer(recorded("search-empty.json")),
        ResponseTemplate::new(500),
    )
    .await;
    assert_eq!(
        ReleaseLookup::lookup_isrc(&unknown, "ZZZZZ0000001")
            .await
            .expect("an answer"),
        None
    );
    assert_eq!(calls(&server).await.len(), 1);

    let (known, _c) = code_server(
        "GBA077700130",
        json_answer(recorded("isrc-search-GBA077700130.json")),
        ResponseTemplate::new(500),
    )
    .await;
    assert!(
        ReleaseLookup::lookup_isrc(&known, "GBA077700130")
            .await
            .expect("an answer")
            .is_some()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn each_kind_of_answer_is_read_and_only_a_refused_question_is_warned() {
    let (logs, _guard) = crate::services::test_support::LogCapture::start();
    let cases: [(ResponseTemplate, &str); 7] = [
        (
            ResponseTemplate::new(404).set_body_raw(recorded("isrc-not-found.json"), "application/json"),
            "not known",
        ),
        (
            ResponseTemplate::new(400)
                .set_body_raw(recorded("release-invalid-mbid.json"), "application/json"),
            "not known",
        ),
        (ResponseTemplate::new(429), "busy"),
        (
            ResponseTemplate::new(400).set_body_raw(recorded("isrc-refused.json"), "application/json"),
            "refused",
        ),
        (ResponseTemplate::new(500), "busy"),
        (
            ResponseTemplate::new(200).set_body_raw("<html>not json</html>", "text/html"),
            "busy",
        ),
        (json_answer(recorded("search-empty.json")), "answer"),
    ];
    for (answer, expected) in cases {
        let (client, _server) = client(answer).await;
        let asked = client
            .ask(
                "recording?query=isrc:GBA077700130&limit=100&fmt=json",
                "code search",
            )
            .await
            .expect("no timeout");
        let kind = match asked {
            Asked::Answer(_) => "answer",
            Asked::NotKnown => "not known",
            Asked::Refused => "refused",
            Asked::Busy { .. } => "busy",
        };
        assert_eq!(kind, expected);
    }
    assert_eq!(
        logs.at(tracing::Level::WARN).len(),
        1,
        "{:?}",
        logs.at(tracing::Level::WARN)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_lookup_by_an_id_that_is_not_a_musicbrainz_id_is_nothing_whatever_musicbrainz_says() {
    let (logs, _guard) = crate::services::test_support::LogCapture::start();
    let (client, _server) = client(
        ResponseTemplate::new(400)
            .set_body_raw(r#"{"error":"Something else entirely."}"#, "application/json"),
    )
    .await;
    assert!(
        client
            .lookup_release("not-an-mbid")
            .await
            .expect("no timeout")
            .is_none()
    );
    assert!(asks_by_a_non_mbid("release/not-an-mbid?inc=labels&fmt=json"));
    assert!(!asks_by_a_non_mbid(
        "release/5e5c5a1c-b4fc-4ba2-9d1e-5f0e1bd1b9a2?inc=labels&fmt=json"
    ));
    assert!(!asks_by_a_non_mbid(
        "recording?query=isrc:GBA077700130&limit=100&fmt=json"
    ));
    assert!(!asks_by_a_non_mbid(
        "isrc/GBA077700130?inc=artist-credits&fmt=json"
    ));
    assert!(
        logs.at(tracing::Level::WARN).is_empty(),
        "{:?}",
        logs.at(tracing::Level::WARN)
    );
}

#[tokio::test]
async fn a_big_code_answer_keeps_25_releases_a_recording_and_drops_its_dj_mix_segment() {
    let raw: serde_json::Value =
        serde_json::from_str(&recorded("isrc-search-USSM19803320.json")).expect("JSON");
    let (client, _server) = code_server(
        "USSM19803320",
        json_answer(recorded("isrc-search-USSM19803320.json")),
        ResponseTemplate::new(500),
    )
    .await;
    let IsrcLookup::Found(doc) = client.lookup_isrc("USSM19803320").await.expect("no timeout") else {
        panic!("found");
    };
    assert_eq!(ids(&doc), ["384f304d-5539-4cbf-920b-cfa8e69f795e"]);
    assert_eq!(
        doc["recordings"][0]["releases"].as_array().map(Vec::len),
        Some(25)
    );

    // The candidates are those of the whole answer without the segment: the reader takes 25.
    let mut whole = raw.clone();
    let recordings = whole["recordings"].as_array_mut().expect("recordings");
    recordings.retain(|r| r["id"] != "4f6bae05-b451-4117-8703-f4fffa07322c");
    whole
        .as_object_mut()
        .expect("an object")
        .insert("isrc".into(), serde_json::Value::String("USSM19803320".into()));
    use octo_core::tagging::candidate_sources::CandidateSources;
    assert_eq!(
        CandidateSources::from_isrc_lookup(&doc),
        CandidateSources::from_isrc_lookup(&whole)
    );
}

// ---- the heart lane, the re-asker and a heart's questions -----------------------------------

/// The gap the lane tests keep between calls, and their background step-back.
const QUICK: Duration = Duration::from_millis(50);

const YESTERDAY_CODE: &str = "GBAYE0601477";
const YESTERDAY_CODE_LOOKUP: &str = "isrc/GBAYE0601477?inc=artist-credits&fmt=json";
const YESTERDAY_1987: &str = "c05194a3-f6f0-4f52-b78a-13fb5580bc0f";
const HELP_GROUP: &str = "0d44e1cb-c6e0-3453-8b68-4d2082f05421";
const A_DOWNLOAD_CHECK: &str = "recording/11111111-1111-4111-8111-111111111111?inc=isrcs&fmt=json";

/// A client on `server` with a short gap and step-back, so the lanes' turns take milliseconds.
fn quick(server: &MockServer, timeout: Duration) -> MusicBrainzClient {
    let base = Url::parse(&format!("{}/ws/2/", server.uri())).expect("parses");
    MusicBrainzClient::with_base_url(base).with_pace(QUICK, timeout, QUICK)
}

fn heart_album(folder: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../octo-core/tests/fixtures/heart_album")
        .join(folder)
}

fn recorded_folder(folder: &str) -> Arc<RecordedAnswers> {
    Arc::new(RecordedAnswers::open(&heart_album(folder)).expect("the folder's urls.tsv reads"))
}

/// Yesterday's code lookup, as MusicBrainz answered it.
fn yesterday_code() -> String {
    std::fs::read_to_string(heart_album("rows/beatles-yesterday_a0/isrc-lookup.json"))
        .expect("the recorded code lookup")
}

/// The question a request asked: its address after the base, with its query.
fn question_of(request: &wiremock::Request) -> String {
    let path = request.url.path();
    let path = path.strip_prefix("/ws/2/").unwrap_or(path);
    match request.url.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_string(),
    }
}

async fn asked(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(question_of)
        .collect()
}

/// Waits until the server has received `count` requests.
async fn until_received(server: &MockServer, count: usize) {
    for _ in 0..1000 {
        if calls(server).await.len() >= count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the server never received {count} requests");
}

/// Serves recorded answers by their exact question: a 404 for one recorded as answered 404, a
/// 500 for any question the folder never recorded.
struct Recorded(Arc<RecordedAnswers>);

impl Respond for Recorded {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        match self.0.get(&question_of(request)) {
            Some(RecordedAnswer::Found(doc)) => ResponseTemplate::new(200).set_body_json(doc),
            Some(RecordedAnswer::NotFound) => ResponseTemplate::new(404),
            None => ResponseTemplate::new(500),
        }
    }
}

/// Deezer's side of a heart's questions, answered from the recorded Deezer answers: a question
/// they do not hold fails the test.
struct RecordedCatalog {
    answers: RecordedAnswers,
    asked: Mutex<usize>,
}

impl RecordedCatalog {
    fn open() -> Arc<RecordedCatalog> {
        Arc::new(RecordedCatalog {
            answers: RecordedAnswers::open(&heart_album("deezer")).expect("the Deezer urls.tsv reads"),
            asked: Mutex::new(0),
        })
    }

    fn answer(&self, question: &str) -> Option<Value> {
        match self.answers.answer(question) {
            RecordedAnswer::Found(doc) => Some(doc),
            RecordedAnswer::NotFound => None,
        }
    }

    /// How many questions a heart asked it.
    fn asked(&self) -> usize {
        *self.asked.lock()
    }
}

#[async_trait::async_trait]
impl CatalogQuestions for RecordedCatalog {
    async fn album_search(
        &self,
        artist: &str,
        album: &str,
        _lane: Lane,
    ) -> DeezerAsked<Vec<CatalogAlbumHit>> {
        *self.asked.lock() += 1;
        match release::deezer_album_search_url(artist, album).and_then(|question| self.answer(&question)) {
            Some(doc) => DeezerAsked::Answer(release::read_album_search(&doc)),
            None => DeezerAsked::NoData,
        }
    }

    async fn album(&self, album_id: &str, _lane: Lane) -> DeezerAsked<(CatalogAlbum, Vec<CatalogTrack>)> {
        *self.asked.lock() += 1;
        let album = self
            .answer(&release::deezer_album_url(album_id))
            .and_then(|doc| release::read_catalog_album(&doc));
        let tracks = self
            .answer(&release::deezer_album_tracks_url(album_id))
            .map(|doc| release::read_catalog_tracks(&doc))
            .unwrap_or_default();
        match album {
            Some(album) => DeezerAsked::Answer((album, tracks)),
            None => DeezerAsked::NoData,
        }
    }
}

/// A wake that sends how a held question ended.
fn wake_into(sender: &mpsc::UnboundedSender<ReAsked>) -> Box<dyn FnOnce(ReAsked) + Send> {
    let sender = sender.clone();
    Box::new(move |how: ReAsked| {
        let _ = sender.send(how);
    })
}

/// How a held question ended, within a few seconds.
async fn ending(ended: &mut mpsc::UnboundedReceiver<ReAsked>) -> ReAsked {
    tokio::time::timeout(Duration::from_secs(8), ended.recv())
        .await
        .expect("the held question ended in time")
        .expect("an ending")
}

#[tokio::test]
async fn yesterday_from_help_through_the_whole_sequence_gives_help() {
    let answers = recorded_folder("rows/beatles-yesterday_a0");
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(Recorded(Arc::clone(&answers)))
        .mount(&server)
        .await;
    let catalog = RecordedCatalog::open();
    let listing = catalog
        .answer(&release::deezer_album_tracks_url("12047944"))
        .map(|doc| release::read_catalog_tracks(&doc))
        .unwrap_or_default();
    let questions = HeartQuestions::new(
        Arc::new(quick(&server, Duration::from_secs(5))),
        catalog.clone(),
        Lane::Heart,
    );
    let song = HeartSong {
        artist: "The Beatles".into(),
        title: "Yesterday (Remastered 2009)".into(),
        album: "Help! (Remastered 2009)".into(),
        disc: Some(1),
        position: Some(13),
        total_tracks: Some(14),
        isrc: YESTERDAY_CODE.into(),
        seconds: 123,
        explicit: listing
            .iter()
            .find(|track| track.id == "116348366")
            .and_then(|track| track.explicit),
    };

    let identity = questions
        .identity(&song, Some("12047944"))
        .await
        .expect("answered");
    let walk = questions.walk(&song, &identity).await.expect("answered");

    assert_eq!(identity.listened_group.as_deref(), Some(HELP_GROUP));
    assert_eq!(walk.first().map(String::as_str), Some(HELP_GROUP));
    // Every recorded question was asked once, and nothing else.
    let unasked: Vec<&str> = answers
        .unasked()
        .iter()
        .map(|line| line.question.as_str())
        .collect();
    assert!(unasked.is_empty(), "{unasked:?}");
    let sent = asked(&server).await;
    assert_eq!(sent.len(), answers.questions().len(), "{sent:?}");

    // Asked again, every answer comes from a heart's rows and the code's own entry.
    let deezer_asked = catalog.asked();
    assert_eq!(
        questions.identity(&song, Some("12047944")).await,
        Ok(identity.clone())
    );
    assert_eq!(questions.walk(&song, &identity).await, Ok(walk));
    assert_eq!(asked(&server).await.len(), sent.len());
    assert_eq!(catalog.asked(), deezer_asked);
}

#[tokio::test]
async fn a_song_with_no_album_takes_the_walks_first_pick_once_its_carriers_are_kept() {
    for (folder, artist, title, code, seconds, group, album) in [
        (
            "rows/beatles-yesterday_1",
            "The Beatles",
            "Yesterday (Remastered 2009)",
            YESTERDAY_CODE,
            123,
            HELP_GROUP,
            "Help!",
        ),
        (
            "rows/celine-my-heart-will-go-on_0",
            "Céline Dion",
            "My Heart Will Go On (Love Theme from \"Titanic\")",
            "CAC229700120",
            280,
            "d90d5240-d510-3d82-95ba-13c6624ea28c",
            "Let’s Talk About Love",
        ),
    ] {
        let server = MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(Recorded(recorded_folder(folder)))
            .mount(&server)
            .await;
        let client = Arc::new(quick(&server, Duration::from_secs(5)));
        let background = HeartQuestions::new(Arc::clone(&client), RecordedCatalog::open(), Lane::Background);
        let song = HeartSong {
            artist: artist.into(),
            title: title.into(),
            isrc: code.into(),
            seconds,
            ..Default::default()
        };

        // Nothing kept: no pick, and only the code lookup was asked.
        assert_eq!(background.no_album_pick(&song).await, None, "{folder}");
        let sent = asked(&server).await;
        assert!(
            sent.iter().all(|question| question.starts_with("isrc/")),
            "{sent:?}"
        );

        // A heart's walk keeps the carriers and the artist's country.
        let heart = HeartQuestions::new(Arc::clone(&client), RecordedCatalog::open(), Lane::Heart);
        let codes = heart.code_recordings(code).await.expect("answered");
        let identity = release::identify(&song, &codes, None);
        let walk = heart.walk(&song, &identity).await.expect("answered");
        let before = asked(&server).await.len();

        let pick = background.no_album_pick(&song).await;

        assert_eq!(pick, Some((group.to_string(), album.to_string())), "{folder}");
        assert_eq!(walk.first().map(String::as_str), Some(group), "{folder}");
        let after = asked(&server).await;
        assert!(
            after[before..]
                .iter()
                .all(|question| question.starts_with("isrc/")),
            "{after:?}"
        );
    }
}

#[tokio::test]
async fn a_heart_question_answered_503_is_asked_again_and_answered() {
    let server = MockServer::builder().start().await;
    Mock::given(path("/ws/2/isrc/GBAYE0601477"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/ws/2/isrc/GBAYE0601477"))
        .respond_with(json_answer(yesterday_code()))
        .mount(&server)
        .await;
    let client = quick(&server, Duration::from_secs(5));

    let IsrcLookup::Found(doc) = client
        .isrc_recordings_for_heart(YESTERDAY_CODE)
        .await
        .expect("no timeout")
    else {
        panic!("found");
    };
    assert_eq!(ids(&doc), [YESTERDAY_1987]);
    assert_eq!(calls(&server).await.len(), 2);
}

#[tokio::test]
async fn four_503s_are_busy_name_the_question_and_leave_nothing_kept() {
    let server = MockServer::builder().start().await;
    Mock::given(path("/ws/2/isrc/GBAYE0601477"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(4)
        .mount(&server)
        .await;
    Mock::given(path("/ws/2/isrc/GBAYE0601477"))
        .respond_with(json_answer(yesterday_code()))
        .mount(&server)
        .await;
    let client = Arc::new(quick(&server, Duration::from_secs(5)));
    let questions = HeartQuestions::new(Arc::clone(&client), RecordedCatalog::open(), Lane::Heart);

    assert_eq!(
        questions.code_recordings(YESTERDAY_CODE).await,
        Err(HeartBusy::MusicBrainz(YESTERDAY_CODE_LOOKUP.to_string()))
    );
    assert_eq!(calls(&server).await.len(), 4, "the question and three retries");
    // Nothing was kept: the next ask goes to MusicBrainz, which answers now.
    let IsrcLookup::Found(doc) = client
        .isrc_recordings_for_heart(YESTERDAY_CODE)
        .await
        .expect("no timeout")
    else {
        panic!("found");
    };
    assert_eq!(ids(&doc), [YESTERDAY_1987]);
    assert_eq!(calls(&server).await.len(), 5);
}

#[tokio::test]
async fn a_retry_after_of_30_seconds_is_busy_at_once_and_the_re_asker_waits_for_it() {
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "30"))
        .mount(&server)
        .await;
    let client = Arc::new(quick(&server, Duration::from_secs(5)));

    let started = std::time::Instant::now();
    assert_eq!(
        client
            .isrc_recordings_for_heart(YESTERDAY_CODE)
            .await
            .expect("no timeout"),
        IsrcLookup::Unanswered
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(calls(&server).await.len(), 1, "never asked again");
    // While it runs, a heart's question is busy without being sent, and a held one waits.
    assert_eq!(
        client.current_recording(YESTERDAY_1987).await,
        MusicBrainzFact::Busy
    );
    let (woke, mut ended) = mpsc::unbounded_channel();
    client.re_ask(YESTERDAY_CODE_LOOKUP, wake_into(&woke));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(calls(&server).await.len(), 1);
    assert!(ended.try_recv().is_err());
}

#[tokio::test]
async fn the_re_asker_waits_out_a_short_retry_after_and_hands_its_answer_on() {
    let probe = recorded_folder("c4-probe");
    let server = MockServer::builder().start().await;
    Mock::given(path("/ws/2/isrc/GBAYE0601477"))
        .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/ws/2/isrc/GBAYE0601477"))
        .respond_with(json_answer(yesterday_code()))
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(Recorded(Arc::clone(&probe)))
        .mount(&server)
        .await;
    let client = Arc::new(quick(&server, Duration::from_secs(5)));

    let (woke, mut ended) = mpsc::unbounded_channel();
    let started = std::time::Instant::now();
    client.re_ask(YESTERDAY_CODE_LOOKUP, wake_into(&woke));
    assert_eq!(ending(&mut ended).await, ReAsked::Answered);
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    // The held question's answer is the next ask's: nothing more is sent.
    let before = calls(&server).await.len();
    let IsrcLookup::Found(doc) = client
        .isrc_recordings_for_heart(YESTERDAY_CODE)
        .await
        .expect("no timeout")
    else {
        panic!("found");
    };
    assert_eq!(ids(&doc), [YESTERDAY_1987]);
    assert_eq!(calls(&server).await.len(), before);
}

#[tokio::test]
async fn a_code_musicbrainz_does_not_know_gives_no_identity_and_no_retry() {
    let server = MockServer::builder().start().await;
    Mock::given(path("/ws/2/isrc/ZZZZZ0000001"))
        .respond_with(
            ResponseTemplate::new(404).set_body_raw(recorded("isrc-not-found.json"), "application/json"),
        )
        .mount(&server)
        .await;
    let questions = HeartQuestions::new(
        Arc::new(quick(&server, Duration::from_secs(5))),
        RecordedCatalog::open(),
        Lane::Heart,
    );
    let song = HeartSong {
        artist: "The Beatles".into(),
        title: "Yesterday".into(),
        isrc: "ZZZZZ0000001".into(),
        ..HeartSong::default()
    };

    let identity = questions
        .identity(&song, None)
        .await
        .expect("answered, never busy");
    assert!(identity.recordings.is_empty(), "{identity:?}");
    assert_eq!(identity.listened_group, None);
    assert_eq!(calls(&server).await.len(), 1, "a 404 is an answer");
}

#[tokio::test]
async fn a_refused_code_question_is_refused_once_and_never_held() {
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(400).set_body_raw(recorded("isrc-refused.json"), "application/json"),
        )
        .mount(&server)
        .await;
    let client = Arc::new(quick(&server, Duration::from_secs(5)));

    assert_eq!(
        client
            .isrc_recordings_for_heart("GBA077700130")
            .await
            .expect("no timeout"),
        IsrcLookup::Refused
    );
    assert_eq!(
        calls(&server).await.len(),
        1,
        "a refused question is not asked again"
    );
    let questions = HeartQuestions::new(Arc::clone(&client), RecordedCatalog::open(), Lane::Heart);
    assert_eq!(questions.code_recordings("GBA077700130").await, Ok(Vec::new()));
    assert_eq!(
        client.isrc_songs_for_heart("GBA077700130").await,
        MusicBrainzFact::NotKnown
    );
    assert_eq!(calls(&server).await.len(), 3);
}

#[tokio::test]
async fn a_heart_and_an_import_asking_one_code_share_one_question() {
    let server = MockServer::builder().start().await;
    Mock::given(path("/ws/2/isrc/GBAYE0601477"))
        .respond_with(json_answer(yesterday_code()).set_delay(Duration::from_millis(300)))
        .mount(&server)
        .await;
    let client = Arc::new(quick(&server, Duration::from_secs(5)));
    let import = crate::services::imports::import_matcher::isrc_recordings_from(Arc::clone(&client));

    let heart = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.isrc_recordings_for_heart(YESTERDAY_CODE).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    let imported = import(YESTERDAY_CODE.to_string()).await;
    let hearted = heart.await.expect("the heart's task ends").expect("no timeout");

    let IsrcLookup::Found(doc) = hearted else {
        panic!("found");
    };
    assert_eq!(ids(&doc), [YESTERDAY_1987]);
    let imported: Vec<String> = imported
        .expect("answered")
        .into_iter()
        .map(|recording| recording.id)
        .collect();
    assert_eq!(imported, [YESTERDAY_1987]);
    assert_eq!(calls(&server).await.len(), 1);
}

#[tokio::test]
async fn the_tagger_and_the_matcher_keep_their_lanes_and_only_a_heart_asks_again() {
    for (asker, sent) in [("tagger", 1), ("matcher", 1), ("heart", 4)] {
        let server = MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let client = quick(&server, Duration::from_secs(5));
        let lookup = match asker {
            "tagger" => client.lookup_isrc("GBA077700130").await,
            "matcher" => client.isrc_recordings_in_background("GBA077700130").await,
            _ => client.isrc_recordings_for_heart("GBA077700130").await,
        };
        assert_eq!(lookup.expect("no timeout"), IsrcLookup::Unanswered, "{asker}");
        assert_eq!(calls(&server).await.len(), sent, "{asker}");
    }
}

#[tokio::test]
async fn a_carriers_page_that_timed_out_is_asked_again_in_pages_of_25() {
    let pages = recorded_folder("c4-timeout-retry");
    let server = MockServer::builder().start().await;
    Mock::given(query_param("limit", "100"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("{}")
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(Recorded(Arc::clone(&pages)))
        .mount(&server)
        .await;
    let questions = HeartQuestions::new(
        Arc::new(quick(&server, Duration::from_millis(300))),
        RecordedCatalog::open(),
        Lane::Heart,
    );

    let carriers = questions.carriers(YESTERDAY_1987).await.expect("answered");

    let whole_page = release::carriers_url(YESTERDAY_1987, 0, release::CARRIERS_PAGE).expect("an id");
    let mut expected = vec![whole_page.clone()];
    expected.extend(pages.questions().iter().map(|line| line.question.clone()));
    assert_eq!(asked(&server).await, expected);
    // The three pages of 25 hold the 66 releases the one page of 100 lists.
    let row = RecordedAnswers::open(&heart_album("rows/beatles-yesterday_a0")).expect("the row reads");
    let RecordedAnswer::Found(doc) = row.answer(&whole_page) else {
        panic!("the page of 100 is recorded");
    };
    let mut wanted: Vec<String> = release::read_carrier_page(&doc)
        .releases
        .into_iter()
        .map(|carrier| carrier.id)
        .collect();
    let mut got: Vec<String> = carriers
        .releases
        .iter()
        .map(|carrier| carrier.id.clone())
        .collect();
    wanted.sort();
    got.sort();
    assert_eq!(got.len(), 66);
    assert_eq!(got, wanted);
}

#[tokio::test]
async fn an_always_failing_page_and_code_lookup_on_an_idle_client_end_after_three_failures() {
    let probe = recorded_folder("c4-probe");
    let server = MockServer::builder().start().await;
    Mock::given(path("/ws/2/artist/89ad4ac3-39f7-470e-963a-56509c546377"))
        .respond_with(Recorded(Arc::clone(&probe)))
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let client = Arc::new(quick(&server, Duration::from_secs(5)));
    let page = release::carriers_url(YESTERDAY_1987, 0, release::CARRIERS_PAGE).expect("an id");

    let (page_woke, mut page_ended) = mpsc::unbounded_channel();
    let (code_woke, mut code_ended) = mpsc::unbounded_channel();
    client.re_ask(&page, wake_into(&page_woke));
    client.re_ask(YESTERDAY_CODE_LOOKUP, wake_into(&code_woke));
    assert_eq!(ending(&mut page_ended).await, ReAsked::GaveUp);
    assert_eq!(ending(&mut code_ended).await, ReAsked::GaveUp);

    let sent = asked(&server).await;
    assert_eq!(sent.iter().filter(|question| **question == page).count(), 3);
    assert_eq!(
        sent.iter()
            .filter(|question| *question == YESTERDAY_CODE_LOOKUP)
            .count(),
        3
    );
    assert!(sent.iter().any(|question| question == PROBE), "{sent:?}");
    // The walk then goes on without the page, and the identity without the code: asked once
    // more, neither is sent.
    let questions = HeartQuestions::new(Arc::clone(&client), RecordedCatalog::open(), Lane::Heart);
    let carriers = questions.carriers(YESTERDAY_1987).await.expect("not busy");
    assert!(carriers.releases.is_empty());
    assert_eq!(questions.code_recordings(YESTERDAY_CODE).await, Ok(Vec::new()));
    assert_eq!(asked(&server).await.len(), sent.len());
}

#[tokio::test]
async fn the_probe_asks_musicbrainz_past_its_kept_rows_and_a_failed_probe_keeps_the_hold() {
    let probe = recorded_folder("c4-probe");
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(Recorded(Arc::clone(&probe)))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let client = Arc::new(quick(&server, Duration::from_secs(5)));
    // The probe's artist is kept both ways: the day-long artist entry and a heart's rows.
    assert_eq!(
        client
            .artist_country(VARIOUS_ARTISTS_ID)
            .await
            .expect("no timeout"),
        Some(String::new())
    );
    let questions = HeartQuestions::new(Arc::clone(&client), RecordedCatalog::open(), Lane::Heart);
    assert_eq!(
        questions.home_country(VARIOUS_ARTISTS_ID).await,
        Ok(String::new())
    );
    assert_eq!(calls(&server).await.len(), 2);

    // MusicBrainz now fails every question.
    let (woke, mut ended) = mpsc::unbounded_channel();
    client.re_ask(YESTERDAY_CODE_LOOKUP, wake_into(&woke));
    tokio::time::sleep(Duration::from_millis(600)).await;
    let sent = asked(&server).await;
    assert_eq!(
        sent[2..],
        [YESTERDAY_CODE_LOOKUP.to_string(), PROBE.to_string()],
        "the probe went to MusicBrainz"
    );
    assert!(
        ended.try_recv().is_err(),
        "a failed probe keeps the question held"
    );

    // Once MusicBrainz answers another question, the held one is asked again and answered.
    server.reset().await;
    Mock::given(path("/ws/2/isrc/GBAYE0601477"))
        .respond_with(json_answer(yesterday_code()))
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(json_answer(r#"{"isrcs": []}"#.to_string()))
        .mount(&server)
        .await;
    client
        .fetch_isrcs("11111111-1111-4111-8111-111111111111")
        .await
        .expect("no timeout");
    assert_eq!(ending(&mut ended).await, ReAsked::Answered);
}

#[test]
fn the_probe_is_the_recorded_various_artists_question() {
    assert_eq!(
        pure::artist_lookup_url(VARIOUS_ARTISTS_ID).as_deref(),
        Some(PROBE)
    );
    let recorded = recorded_folder("c4-probe");
    let questions: Vec<&str> = recorded
        .questions()
        .iter()
        .map(|line| line.question.as_str())
        .collect();
    assert_eq!(questions, [PROBE]);
}

#[tokio::test]
async fn the_gate_is_free_while_a_heart_question_is_out() {
    let server = MockServer::builder().start().await;
    Mock::given(path("/ws/2/isrc/GBAYE0601477"))
        .respond_with(json_answer(yesterday_code()).set_delay(Duration::from_millis(1500)))
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(json_answer(r#"{"isrcs": []}"#.to_string()))
        .mount(&server)
        .await;
    let client = Arc::new(quick(&server, Duration::from_secs(5)));

    let heart = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.isrc_recordings_for_heart(YESTERDAY_CODE).await })
    };
    until_received(&server, 1).await;
    let started = std::time::Instant::now();
    client
        .fetch_isrcs("11111111-1111-4111-8111-111111111111")
        .await
        .expect("no timeout");
    let waited = started.elapsed();

    assert!(waited < Duration::from_millis(1000), "waited {waited:?}");
    assert!(!heart.is_finished(), "the heart's question is still out");
    let hearted = heart.await.expect("the heart's task ends").expect("no timeout");
    assert!(matches!(hearted, IsrcLookup::Found(_)), "{hearted:?}");
}

#[tokio::test]
async fn a_download_check_queued_during_a_hearts_retries_goes_first() {
    let server = MockServer::builder().start().await;
    Mock::given(path("/ws/2/isrc/GBAYE0601477"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/ws/2/isrc/GBAYE0601477"))
        .respond_with(json_answer(yesterday_code()))
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(json_answer(r#"{"isrcs": []}"#.to_string()))
        .mount(&server)
        .await;
    let base = Url::parse(&format!("{}/ws/2/", server.uri())).expect("parses");
    let gap = Duration::from_millis(400);
    let client = Arc::new(MusicBrainzClient::with_base_url(base).with_pace(gap, Duration::from_secs(5), gap));

    let heart = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.isrc_recordings_for_heart(YESTERDAY_CODE).await })
    };
    until_received(&server, 1).await;
    client
        .fetch_isrcs("11111111-1111-4111-8111-111111111111")
        .await
        .expect("no timeout");
    let hearted = heart.await.expect("the heart's task ends").expect("no timeout");

    assert!(matches!(hearted, IsrcLookup::Found(_)), "{hearted:?}");
    assert_eq!(
        asked(&server).await,
        [YESTERDAY_CODE_LOOKUP, A_DOWNLOAD_CHECK, YESTERDAY_CODE_LOOKUP]
    );
}

#[tokio::test]
async fn the_background_lane_steps_aside_while_a_heart_question_waits() {
    let server = MockServer::builder().start().await;
    Mock::given(path("/ws/2/isrc/GBAYE0601477"))
        .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/ws/2/isrc/GBAYE0601477"))
        .respond_with(json_answer(yesterday_code()))
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string(ROSALIA))
        .mount(&server)
        .await;
    let client = Arc::new(quick(&server, Duration::from_secs(5)));

    let heart = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.isrc_recordings_for_heart(YESTERDAY_CODE).await })
    };
    until_received(&server, 1).await;
    // The heart waits out MusicBrainz's second with the gate free; the background lane still
    // waits for it.
    let country = client.artist_country("a-rosalia").await.expect("no timeout");
    let hearted = heart.await.expect("the heart's task ends").expect("no timeout");

    assert_eq!(country.as_deref(), Some("ES"));
    assert!(matches!(hearted, IsrcLookup::Found(_)), "{hearted:?}");
    assert_eq!(
        asked(&server).await,
        [
            YESTERDAY_CODE_LOOKUP,
            YESTERDAY_CODE_LOOKUP,
            "artist/a-rosalia?fmt=json"
        ]
    );
}

#[tokio::test]
async fn a_merged_recording_answers_with_the_one_it_became_and_is_kept() {
    const MERGED: &str = "6a3e7d5c-0000-4000-8000-000000000001";
    const GONE: &str = "6a3e7d5c-0000-4000-8000-000000000002";
    const BUSY: &str = "6a3e7d5c-0000-4000-8000-000000000003";
    let server = MockServer::builder().start().await;
    Mock::given(path(format!("/ws/2/recording/{MERGED}")))
        .respond_with(json_answer(format!(
            r#"{{"id": "{YESTERDAY_1987}", "title": "Yesterday"}}"#
        )))
        .mount(&server)
        .await;
    Mock::given(path(format!("/ws/2/recording/{GONE}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let client = quick(&server, Duration::from_secs(5));

    assert_eq!(
        client.current_recording(&MERGED.to_uppercase()).await,
        MusicBrainzFact::Known(YESTERDAY_1987.to_string())
    );
    assert_eq!(
        client.current_recording(MERGED).await,
        MusicBrainzFact::Known(YESTERDAY_1987.to_string())
    );
    assert_eq!(client.current_recording(GONE).await, MusicBrainzFact::NotKnown);
    assert_eq!(
        client.current_recording("not-an-id").await,
        MusicBrainzFact::NotKnown
    );
    assert_eq!(
        asked(&server).await,
        [
            format!("recording/{MERGED}?fmt=json"),
            format!("recording/{GONE}?fmt=json")
        ]
    );
    assert_eq!(client.current_recording(BUSY).await, MusicBrainzFact::Busy);
    assert_eq!(
        calls(&server).await.len(),
        6,
        "a busy one is asked again three times"
    );
}

#[tokio::test]
async fn a_codes_recordings_read_as_answer_recordings() {
    let server = MockServer::builder().start().await;
    Mock::given(path("/ws/2/isrc/GBAYE0601477"))
        .respond_with(json_answer(yesterday_code()))
        .mount(&server)
        .await;
    let client = quick(&server, Duration::from_secs(5));

    assert_eq!(
        client.isrc_songs_for_heart("gb-aye-06-01477").await,
        MusicBrainzFact::Known(vec![AnswerRecording {
            id: YESTERDAY_1987.into(),
            title: "Yesterday".into(),
            artists: vec![AnswerArtist {
                id: Some("b10bbbfc-cf9e-42e0-be17-e2c3e1d2600d".into()),
                name: "The Beatles".into(),
            }],
            duration_seconds: Some(125.666),
            isrcs: vec![YESTERDAY_CODE.into()],
            sources: 0,
        }])
    );
}

#[test]
fn a_hearts_rows_keep_at_most_20000_rows_and_drop_the_least_recently_used() {
    let mut rows = HeartRows::default();
    rows.put("carriers|a".into(), HeartRow::Country("A".into()), 10_000);
    rows.put("carriers|b".into(), HeartRow::Country("B".into()), 10_000);
    assert!(rows.get("carriers|a").is_some());

    rows.put("carriers|c".into(), HeartRow::Country("C".into()), 1);

    assert!(
        rows.get("carriers|b").is_none(),
        "the least recently used goes first"
    );
    assert!(rows.get("carriers|a").is_some());
    assert!(rows.get("carriers|c").is_some());
    rows.put("carriers|d".into(), HeartRow::Country("D".into()), 20_001);
    assert!(
        rows.get("carriers|d").is_none(),
        "an answer bigger than the cache is not kept"
    );
    assert_eq!(rows.rows, 10_001);
    assert_eq!(HEART_ROWS_MAX, 20_000);
}

// ---- the warm-up ------------------------------------------------------------------------------

/// Deezer's side of a warm-up: it knows no album, and keeps which album each question named and
/// on which lane. A question for the album `held` waits until the test lets `hold` go.
struct WatchedCatalog {
    asked: Mutex<Vec<(String, Lane)>>,
    held: String,
    hold: tokio::sync::Mutex<()>,
}

impl WatchedCatalog {
    fn holding(album_id: &str) -> Arc<WatchedCatalog> {
        Arc::new(WatchedCatalog {
            asked: Mutex::new(Vec::new()),
            held: album_id.to_string(),
            hold: tokio::sync::Mutex::new(()),
        })
    }

    fn albums(&self) -> Vec<String> {
        self.asked.lock().iter().map(|(album, _)| album.clone()).collect()
    }

    fn lanes(&self) -> Vec<Lane> {
        self.asked.lock().iter().map(|(_, lane)| *lane).collect()
    }
}

#[async_trait::async_trait]
impl CatalogQuestions for WatchedCatalog {
    async fn album_search(
        &self,
        _artist: &str,
        _album: &str,
        lane: Lane,
    ) -> DeezerAsked<Vec<CatalogAlbumHit>> {
        self.asked.lock().push(("search".to_string(), lane));
        DeezerAsked::NoData
    }

    async fn album(&self, album_id: &str, lane: Lane) -> DeezerAsked<(CatalogAlbum, Vec<CatalogTrack>)> {
        self.asked.lock().push((album_id.to_string(), lane));
        if album_id == self.held {
            let _released = self.hold.lock().await;
        }
        DeezerAsked::NoData
    }
}

/// A song with nothing MusicBrainz is asked about: no ISRC, so no recordings and no artist.
fn plain_song(title: &str) -> HeartSong {
    HeartSong {
        artist: "Artist".into(),
        title: title.into(),
        ..HeartSong::default()
    }
}

/// Waits until `done` says so.
async fn until(what: &str, done: impl Fn() -> bool) {
    for _ in 0..1000 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("{what} never happened");
}

#[tokio::test]
async fn the_warm_up_keeps_the_newest_8_songs_and_asks_the_newest_first() {
    let server = MockServer::builder().start().await;
    let client = Arc::new(quick(&server, Duration::from_secs(5)));
    let catalog = WatchedCatalog::holding("a0");
    let held = catalog.hold.lock().await;

    client.warm_up("song-0", plain_song("0"), Some("a0".into()), catalog.clone());
    until("the first song's question", || catalog.albums().len() == 1).await;
    for n in 1..=10 {
        client.warm_up(
            &format!("song-{n}"),
            plain_song(&n.to_string()),
            Some(format!("a{n}")),
            catalog.clone(),
        );
    }
    drop(held);
    client.warmed_up().await;

    // The ninth and tenth songs put out the two that had waited longest.
    assert_eq!(
        catalog.albums(),
        ["a0", "a10", "a9", "a8", "a7", "a6", "a5", "a4", "a3"]
    );
    assert!(
        catalog.lanes().iter().all(|lane| *lane == Lane::Background),
        "{:?}",
        catalog.lanes()
    );
    assert!(calls(&server).await.is_empty());
}

#[tokio::test]
async fn a_song_waiting_or_being_warmed_is_queued_once() {
    let server = MockServer::builder().start().await;
    let client = Arc::new(quick(&server, Duration::from_secs(5)));
    let catalog = WatchedCatalog::holding("x");
    let held = catalog.hold.lock().await;

    client.warm_up("x", plain_song("x"), Some("x".into()), catalog.clone());
    until("the first song's question", || catalog.albums().len() == 1).await;
    // Played again while its questions are out: it is not queued.
    client.warm_up("x", plain_song("x"), Some("x".into()), catalog.clone());
    client.warm_up("y", plain_song("y"), Some("y".into()), catalog.clone());
    client.warm_up("z", plain_song("z"), Some("z".into()), catalog.clone());
    // Played again while it waits: it moves to the front.
    client.warm_up("y", plain_song("y"), Some("y".into()), catalog.clone());
    drop(held);
    client.warmed_up().await;
    assert_eq!(catalog.albums(), ["x", "y", "z"]);

    // Once its warm-up is over, a song played again is queued again.
    client.warm_up("x", plain_song("x"), Some("x".into()), catalog.clone());
    client.warmed_up().await;
    assert_eq!(catalog.albums(), ["x", "y", "z", "x"]);
}

#[tokio::test]
async fn a_warmed_up_song_leaves_a_heart_only_its_carriers_to_ask() {
    let answers = recorded_folder("rows/beatles-yesterday_a0");
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(Recorded(Arc::clone(&answers)))
        .mount(&server)
        .await;
    let catalog = RecordedCatalog::open();
    let client = Arc::new(quick(&server, Duration::from_secs(5)));
    let song = HeartSong {
        artist: "The Beatles".into(),
        title: "Yesterday (Remastered 2009)".into(),
        album: "Help! (Remastered 2009)".into(),
        disc: Some(1),
        position: Some(13),
        total_tracks: Some(14),
        isrc: YESTERDAY_CODE.into(),
        seconds: 123,
        explicit: catalog
            .answer(&release::deezer_album_tracks_url("12047944"))
            .map(|doc| release::read_catalog_tracks(&doc))
            .unwrap_or_default()
            .iter()
            .find(|track| track.id == "116348366")
            .and_then(|track| track.explicit),
    };

    client.warm_up(
        "yesterday",
        song.clone(),
        Some("12047944".into()),
        catalog.clone(),
    );
    client.warmed_up().await;

    // Every recorded question but the carriers page, each once.
    let is_carriers = |question: &str| question.starts_with("release?recording=");
    let carriers: Vec<String> = answers
        .questions()
        .iter()
        .map(|line| line.question.clone())
        .filter(|question| is_carriers(question))
        .collect();
    assert_eq!(carriers.len(), 1);
    let warmed = asked(&server).await;
    assert!(!warmed.iter().any(|question| is_carriers(question)), "{warmed:?}");
    let mut once = warmed.clone();
    once.sort();
    once.dedup();
    assert_eq!(once.len(), warmed.len(), "{warmed:?}");
    assert_eq!(warmed.len(), answers.questions().len() - 1, "{warmed:?}");

    // Hearted now, its identity and its artist come from what the warm-up kept.
    let deezer_asked = catalog.asked();
    let questions = HeartQuestions::new(Arc::clone(&client), catalog.clone(), Lane::Heart);
    let identity = questions
        .identity(&song, Some("12047944"))
        .await
        .expect("answered");
    assert_eq!(identity.listened_group.as_deref(), Some(HELP_GROUP));
    assert_eq!(asked(&server).await, warmed);
    assert_eq!(catalog.asked(), deezer_asked);
    let walk = questions.walk(&song, &identity).await.expect("answered");
    assert_eq!(walk.first().map(String::as_str), Some(HELP_GROUP));
    assert_eq!(asked(&server).await[warmed.len()..], carriers);
}

#[tokio::test]
async fn a_warm_up_asks_on_the_background_lane_and_a_busy_answer_ends_its_turn() {
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let client = Arc::new(quick(&server, Duration::from_secs(5)));
    let catalog = WatchedCatalog::holding("");
    let song = HeartSong {
        isrc: YESTERDAY_CODE.into(),
        ..plain_song("Yesterday")
    };

    client.warm_up("yesterday", song, None, catalog.clone());
    client.warmed_up().await;

    // Asked once: the background lane never asks again, where a heart's question would.
    assert_eq!(asked(&server).await, [YESTERDAY_CODE_LOOKUP]);
    let last = *client.gate.try_lock().expect("the gate is free");
    assert_eq!(last.map(|call| call.lane), Some(Lane::Background));
    // With no album id and no album title, Deezer has nothing to be asked.
    assert!(catalog.albums().is_empty());
}

#[test]
fn a_routing_reads_as_the_song_a_heart_knows() {
    let routing = SoulseekRouting {
        artist: Some(" The Beatles ".into()),
        title: Some("Yesterday (Remastered 2009)".into()),
        album: Some("Help! (Remastered 2009)".into()),
        duration: Some(123),
        track: Some(13),
        disc_number: Some(1),
        total_tracks: Some(14),
        isrc: Some("GBAYE0601477".into()),
        explicit_content: Some(4),
        ..SoulseekRouting::default()
    };
    assert_eq!(
        heart_song(&routing),
        HeartSong {
            artist: "The Beatles".into(),
            title: "Yesterday (Remastered 2009)".into(),
            album: "Help! (Remastered 2009)".into(),
            disc: Some(1),
            position: Some(13),
            total_tracks: Some(14),
            isrc: "GBAYE0601477".into(),
            seconds: 123,
            explicit: Some(1),
        }
    );
    // A count or a length that is not above zero is unknown.
    let bare = SoulseekRouting {
        track: Some(0),
        disc_number: Some(-1),
        duration: Some(0),
        ..SoulseekRouting::default()
    };
    assert_eq!(heart_song(&bare), HeartSong::default());
}
