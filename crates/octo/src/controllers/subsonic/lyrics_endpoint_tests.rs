//! The lyrics endpoints through the app: getLyricsBySongId, choosing a song's lyrics (octoLyrics
//! v1), and what every other client sees of a choice. The choosing service on its own is tested
//! in `services/lyrics/lyrics_choice_service_tests.rs`.

// `&[source.clone()]` coerces each source to `Arc<dyn ILyricsSource>`, which `from_ref` cannot.
#![allow(clippy::cloned_ref_to_slice_refs)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use octo_core::lyrics::{
    ILyricsSource, LyricsCandidate, LyricsLookup, LyricsPin, LyricsQuery, LyricsResult, LyricsSearch,
};
use octo_core::settings::{AppSettings, MetadataSettings};
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use octo_subsonic::xml::XElement;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

use super::test_app::*;
use crate::app::AppState;
use crate::http::pipeline::App;
use crate::services::lyrics::{LyricsChoiceService, LyricsChoiceStore, LyricsService};

// ---- A source whose search and fetch the tests control ------------------------------------

/// A lyrics source whose search results and fetched lyrics a test sets.
struct ChoosableSource {
    key: String,
    entries: Vec<LyricsCandidate>,
    lyrics: HashMap<String, LyricsResult>,
    answer: Mutex<Option<LyricsResult>>,
    finds: AtomicUsize,
}

impl ChoosableSource {
    fn finds(&self) -> usize {
        self.finds.load(Ordering::SeqCst)
    }

    fn answer_with(&self, synced: &str) {
        *self.answer.lock() = Some(LyricsResult::new("KuGou", Some(synced.into()), None, false));
    }
}

#[async_trait]
impl ILyricsSource for ChoosableSource {
    fn key(&self) -> &str {
        &self.key
    }

    async fn find(&self, _query: &LyricsQuery, _ct: &CancellationToken) -> LyricsLookup {
        self.finds.fetch_add(1, Ordering::SeqCst);
        match self.answer.lock().clone() {
            Some(found) => LyricsLookup::new(Some(found), false),
            None => LyricsLookup::miss(),
        }
    }

    async fn search(&self, _query: &LyricsQuery, _ct: &CancellationToken) -> LyricsSearch {
        LyricsSearch::new(self.entries.clone(), false)
    }

    async fn fetch(&self, id: &str, _ct: &CancellationToken) -> LyricsLookup {
        match self.lyrics.get(id) {
            Some(found) => LyricsLookup::new(Some(found.clone()), false),
            None => LyricsLookup::miss(),
        }
    }
}

/// KuGou with two candidates (the song and a remix), answering an automatic lookup with line
/// lyrics.
fn kugou() -> Arc<ChoosableSource> {
    let source = ChoosableSource {
        key: "kugou".into(),
        entries: vec![
            LyricsCandidate::new(
                "kugou",
                "1.a",
                "Some Song",
                "Some Artist",
                Some("The Album".into()),
                Some(200),
            ),
            LyricsCandidate::new(
                "kugou",
                "2.b",
                "Some Song (Remix)",
                "Some Artist",
                None,
                Some(260),
            ),
        ],
        lyrics: HashMap::from([
            (
                "1.a".to_string(),
                LyricsResult::new(
                    "KuGou",
                    Some(
                        "[00:01.00]<00:01.00>right <00:01.50>words\n[00:03.00]<00:03.00>second<00:04.00>"
                            .into(),
                    ),
                    None,
                    false,
                ),
            ),
            (
                "2.b".to_string(),
                LyricsResult::new("KuGou", Some("[00:01.00]remix words".into()), None, false),
            ),
        ]),
        answer: Mutex::new(None),
        finds: AtomicUsize::new(0),
    };
    source.answer_with("[00:01.00]automatic words");
    Arc::new(source)
}

/// KuGou answering an automatic lookup with word-timed lyrics.
fn word_timed_kugou() -> Arc<ChoosableSource> {
    let source = kugou();
    source.answer_with("[00:01.00]<00:01.00>kugou <00:01.50>words<00:02.00>");
    source
}

/// A source that takes longer than the interactive budget to answer (`SlowSource`).
struct SlowSource {
    delay: Duration,
    finds: AtomicUsize,
    /// Told once a lookup has its answer, for a test to wait on instead of the clock.
    answered: Notify,
}

impl SlowSource {
    fn new(delay: Duration) -> Arc<Self> {
        Arc::new(SlowSource {
            delay,
            finds: AtomicUsize::new(0),
            answered: Notify::new(),
        })
    }
}

#[async_trait]
impl ILyricsSource for SlowSource {
    fn key(&self) -> &str {
        "kugou"
    }

    async fn find(&self, _query: &LyricsQuery, ct: &CancellationToken) -> LyricsLookup {
        self.finds.fetch_add(1, Ordering::SeqCst);
        tokio::select! {
            _ = tokio::time::sleep(self.delay) => {}
            _ = ct.cancelled() => return LyricsLookup::failed(),
        }
        self.answered.notify_one();
        LyricsLookup::new(
            Some(LyricsResult::new(
                "KuGou",
                Some("[00:01.00]found late".into()),
                None,
                false,
            )),
            false,
        )
    }
}

/// A source answering every lookup the same way.
struct FixedSource {
    key: String,
    answer: LyricsLookup,
}

#[async_trait]
impl ILyricsSource for FixedSource {
    fn key(&self) -> &str {
        &self.key
    }

    async fn find(&self, _query: &LyricsQuery, _ct: &CancellationToken) -> LyricsLookup {
        self.answer.clone()
    }
}

// ---- Navidrome, as far as these calls need it ---------------------------------------------

/// Navidrome's own answer for a library song that has line lyrics of its own.
const LIBRARY_LYRICS_JSON: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","lyricsList":{"structuredLyrics":[{"displayArtist":"Some Artist","displayTitle":"Library Song","lang":"xxx","line":[{"start":0,"value":"navidrome's own"}],"synced":true}]}}}"#;

/// A stand-in Navidrome: accepts the token `good`, knows one library song `lib1`.
#[derive(Clone, Default)]
struct FakeNavidrome {
    pings: Arc<AtomicUsize>,
    /// Whether each lyrics call to Navidrome asked for word cues.
    lyrics_asked_enhanced: Arc<Mutex<Vec<bool>>>,
}

impl Respond for FakeNavidrome {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let query = query_of(request);
        let path = request.url.path();
        if path == "/rest/getLyricsBySongId" {
            self.lyrics_asked_enhanced
                .lock()
                .push(query.get("enhanced").map(String::as_str) == Some("true"));
        }
        let good = query.get("t").map(String::as_str) == Some("good");
        let json = query.get("f").map(String::as_str) == Some("json");
        const REFUSED: &str = r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}"#;
        let body = match path {
            "/rest/ping" => {
                self.pings.fetch_add(1, Ordering::SeqCst);
                Some(if good {
                    r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#
                } else {
                    REFUSED
                })
            }
            "/rest/getSong" => Some(if good {
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1","song":{"id":"lib1","title":"Library Song","artist":"Some Artist","album":"An Album","duration":201}}}"#
            } else {
                REFUSED
            }),
            "/rest/getLyricsBySongId" => Some(if !good {
                REFUSED
            } else if json {
                LIBRARY_LYRICS_JSON
            } else {
                r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"><lyricsList></lyricsList></subsonic-response>"#
            }),
            "/rest/getLyrics" => Some(if !good {
                REFUSED
            } else if json {
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1","lyrics":{"value":""}}}"#
            } else {
                r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"><lyrics></lyrics></subsonic-response>"#
            }),
            "/rest/getOpenSubsonicExtensions" => Some(
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1","openSubsonic":true,"openSubsonicExtensions":[{"name":"songLyrics","versions":[1]}]}}"#,
            ),
            _ => None,
        };
        match body {
            None => ResponseTemplate::new(404),
            Some(body) => {
                let content_type = if body.starts_with('<') {
                    "application/xml"
                } else {
                    "application/json"
                };
                ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), content_type)
            }
        }
    }
}

/// The app over the fake Navidrome, with these sources and a fresh pin store.
struct Factory {
    _server: MockServer,
    navidrome: FakeNavidrome,
    state: AppState,
    app: App,
    store: Arc<LyricsChoiceStore>,
}

impl Factory {
    async fn new(fetch: bool, sources: Vec<Arc<dyn ILyricsSource>>) -> Factory {
        Self::with(fetch, sources, None, true).await
    }

    /// `order`: LYRICS_SOURCES when not just the sources given, in their order.
    async fn with(
        fetch: bool,
        sources: Vec<Arc<dyn ILyricsSource>>,
        order: Option<&str>,
        prefer_words: bool,
    ) -> Factory {
        let server = MockServer::builder().start().await;
        let navidrome = FakeNavidrome::default();
        Mock::given(any())
            .respond_with(navidrome.clone())
            .mount(&server)
            .await;
        let lyrics_sources = order.map(str::to_string).unwrap_or_else(|| {
            sources
                .iter()
                .map(|s| s.key().to_string())
                .collect::<Vec<_>>()
                .join(",")
        });
        let base = settings(&server.uri());
        let configured = AppSettings {
            metadata: MetadataSettings {
                fetch_lyrics: fetch,
                lyrics_sources,
                prefer_word_timed_lyrics: prefer_words,
                ..MetadataSettings::default()
            },
            ..base
        };
        let store = Arc::new(LyricsChoiceStore::new(None));
        let held = Arc::clone(&store);
        let state = state_with(configured, move |inner| {
            let service = Arc::new(LyricsService::new(sources, inner.settings.clone()));
            inner.lyrics_choice_service = Arc::new(LyricsChoiceService::new(service.clone(), held.clone()));
            inner.lyrics_choice_store = held;
            inner.lyrics_service = service;
        });
        let app = app(&state);
        Factory {
            _server: server,
            navidrome,
            state,
            app,
            store,
        }
    }

    /// An outside song in the registry.
    fn external(&self, artist: &str, title: &str) -> String {
        self.state.external_id_registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some(artist.into()),
            title: Some(title.into()),
            album: Some("The Album".into()),
            duration: Some(200),
            ..Default::default()
        })
    }

    fn some_song(&self) -> String {
        self.external("Some Artist", "Some Song")
    }

    async fn get_string(&self, url: &str) -> String {
        let reply = get(&self.app, url).await;
        assert!(reply.status.is_success(), "{url}: {}", reply.status);
        reply.text()
    }

    /// `subsonic-response` of a JSON answer.
    async fn get_json(&self, url: &str) -> Value {
        get(&self.app, url).await.envelope()
    }

    fn choice_for(&self, id: &str) -> String {
        self.state.lyrics_choice_service.choice_for(id)
    }
}

fn sources(list: &[Arc<dyn ILyricsSource>]) -> Vec<Arc<dyn ILyricsSource>> {
    list.to_vec()
}

/// Credentials sent with the Octo app's client name.
fn octo_auth(user: &str, token: &str) -> String {
    auth(user, token, "Octo")
}

fn alice() -> String {
    octo_auth("alice", "good")
}

fn first_line(response: &Value) -> String {
    response["lyricsList"]["structuredLyrics"][0]["line"][0]["value"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

// ---- getLyricsBySongId --------------------------------------------------------------------

/// The app with Navidrome unreachable and one LRCLIB stand-in.
async fn lyrics_web_factory(fetch: bool, answer: LyricsLookup) -> Factory {
    Factory::new(
        fetch,
        vec![Arc::new(FixedSource {
            key: "lrclib".into(),
            answer,
        })],
    )
    .await
}

#[tokio::test]
async fn get_lyrics_by_song_id_external_song_returns_structured_lyrics() {
    let factory = lyrics_web_factory(
        true,
        LyricsLookup::new(
            Some(LyricsResult::new(
                "lrclib",
                Some("[00:01.50]first\n[00:03.00]second".into()),
                None,
                false,
            )),
            false,
        ),
    )
    .await;
    let id = factory.state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("Some Artist".into()),
        title: Some("Some Song".into()),
        duration: Some(200),
        ..Default::default()
    });

    let body = factory
        .get_json(&format!("/rest/getLyricsBySongId?id={id}&f=json&u=alice&t=t&s=s"))
        .await;

    let lyrics = &body["lyricsList"]["structuredLyrics"][0];
    assert_eq!(lyrics["synced"], true);
    assert_eq!(lyrics["displayArtist"], "Some Artist");
    let lines = lyrics["line"].as_array().expect("lines");
    assert_eq!(lines[0]["start"].as_i64(), Some(1500));
    assert_eq!(lines[1]["value"], "second");
}

/// Off is exactly what shipped before: an empty but ok list, never "data not found".
#[tokio::test]
async fn get_lyrics_by_song_id_external_song_fetch_off_is_an_empty_list() {
    let factory = lyrics_web_factory(
        false,
        LyricsLookup::new(
            Some(LyricsResult::new(
                "lrclib",
                Some("[00:01.00]x".into()),
                None,
                false,
            )),
            false,
        ),
    )
    .await;
    let id = factory.state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("Some Artist".into()),
        title: Some("Other Song".into()),
        duration: Some(200),
        ..Default::default()
    });

    let response = factory
        .get_json(&format!("/rest/getLyricsBySongId?id={id}&f=json&u=alice&t=t&s=s"))
        .await;

    assert_eq!(response["status"], "ok");
    assert_eq!(
        response["lyricsList"]["structuredLyrics"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );
}

// ---- getLyricsCandidates / setLyricsChoice ------------------------------------------------

#[tokio::test]
async fn candidates_list_every_entry_the_same_song_first_with_a_preview() {
    let factory = Factory::new(true, sources(&[kugou()])).await;
    let id = factory.some_song();

    let response = factory
        .get_json(&format!("/rest/getLyricsCandidates?id={id}&{}", alice()))
        .await;

    assert_eq!(response["status"], "ok");
    let list = &response["lyricsCandidates"];
    assert_eq!(list["id"], id.as_str());
    assert_eq!(list["choice"], "auto");
    let candidates = list["candidate"].as_array().expect("candidates");
    let ids: Vec<&str> = candidates.iter().filter_map(|c| c["id"].as_str()).collect();
    assert_eq!(ids, ["kugou:1.a", "kugou:2.b"]);
    let first = &candidates[0];
    assert_eq!(first["source"], "kugou");
    assert_eq!(first["kind"], "word");
    assert_eq!(first["album"], "The Album");
    assert_eq!(first["duration"].as_i64(), Some(200));
    assert_eq!(first["sameSong"], true);
    assert_eq!(first["chosen"], false);
    let preview: Vec<&str> = first["preview"]
        .as_array()
        .expect("preview")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(preview, ["right words", "second"]);
    assert_eq!(candidates[1]["sameSong"], false);
    assert_eq!(candidates[1]["kind"], "line");
}

#[tokio::test]
async fn candidates_manual_search_judges_by_the_title_and_artist_given() {
    let factory = Factory::new(true, sources(&[kugou()])).await;
    let id = factory.external("Wrong Tag", "Track 01");

    let response = factory
        .get_json(&format!(
            "/rest/getLyricsCandidates?id={id}&title=Some%20Song&artist=Some%20Artist&{}",
            alice()
        ))
        .await;

    assert_eq!(response["lyricsCandidates"]["candidate"][0]["sameSong"], true);
}

#[tokio::test]
async fn endpoints_wrong_password_is_error40() {
    for (endpoint, extra) in [
        ("getLyricsCandidates", ""),
        ("setLyricsChoice", "&candidate=none"),
    ] {
        let factory = Factory::new(true, sources(&[kugou()])).await;
        let id = factory.some_song();

        let response = factory
            .get_json(&format!(
                "/rest/{endpoint}?id={id}{extra}&{}",
                octo_auth("alice", "bad")
            ))
            .await;

        assert_eq!(response["status"], "failed", "{endpoint}");
        assert_eq!(response["error"]["code"].as_i64(), Some(40), "{endpoint}");
        assert_eq!(factory.choice_for(&id), "auto", "{endpoint}");
    }
}

#[tokio::test]
async fn pin_every_client_gets_the_pinned_lyrics_and_auto_goes_back() {
    let factory = Factory::new(true, sources(&[kugou()])).await;
    let id = factory.some_song();

    factory
        .get_json(&format!("/rest/getLyricsCandidates?id={id}&{}", alice()))
        .await;
    let set = factory
        .get_json(&format!(
            "/rest/setLyricsChoice?id={id}&candidate=kugou:1.a&{}",
            alice()
        ))
        .await;
    assert_eq!(set["lyricsChoice"]["choice"], "kugou:1.a");

    // Another user, another client, no enhanced: the pinned words, as plain lines.
    let other = factory
        .get_json(&format!(
            "/rest/getLyricsBySongId?id={id}&f=json&{}",
            octo_auth("bob", "good")
        ))
        .await;
    assert_eq!(first_line(&other), "right words");

    // The candidates now say which is chosen.
    let again = factory
        .get_json(&format!(
            "/rest/getLyricsCandidates?id={id}&{}",
            octo_auth("bob", "good")
        ))
        .await;
    assert_eq!(again["lyricsCandidates"]["choice"], "kugou:1.a");
    assert_eq!(again["lyricsCandidates"]["candidate"][0]["chosen"], true);

    factory
        .get_json(&format!(
            "/rest/setLyricsChoice?id={id}&candidate=auto&{}",
            alice()
        ))
        .await;
    let automatic = factory
        .get_json(&format!("/rest/getLyricsBySongId?id={id}&f=json&{}", alice()))
        .await;
    assert_eq!(first_line(&automatic), "automatic words");
}

#[tokio::test]
async fn pin_without_a_list_first_fetches_the_candidate_from_its_source() {
    let factory = Factory::new(true, sources(&[kugou()])).await;
    let id = factory.some_song();

    let set = factory
        .get_json(&format!(
            "/rest/setLyricsChoice?id={id}&candidate=kugou:1.a&{}",
            alice()
        ))
        .await;
    let gone = factory
        .get_json(&format!(
            "/rest/setLyricsChoice?id={id}&candidate=kugou:9.z&{}",
            alice()
        ))
        .await;

    assert_eq!(set["status"], "ok");
    assert_eq!(gone["error"]["code"].as_i64(), Some(70));
    assert_eq!(factory.choice_for(&id), "kugou:1.a");
}

#[tokio::test]
async fn hide_every_client_gets_no_lyrics_even_navidromes_own() {
    let kugou = kugou();
    let factory = Factory::new(true, sources(&[kugou.clone()])).await;

    factory
        .get_json(&format!(
            "/rest/setLyricsChoice?id=lib1&candidate=none&{}",
            alice()
        ))
        .await;
    let hidden = factory
        .get_json(&format!(
            "/rest/getLyricsBySongId?id=lib1&f=json&{}",
            octo_auth("bob", "good")
        ))
        .await;
    let legacy = factory
        .get_json(&format!(
            "/rest/getLyrics?artist=Some%20Artist&title=Library%20Song&f=json&{}",
            octo_auth("bob", "good")
        ))
        .await;

    assert_eq!(hidden["status"], "ok");
    assert_eq!(
        hidden["lyricsList"]["structuredLyrics"].as_array().map(Vec::len),
        Some(0)
    );
    assert_eq!(legacy["lyrics"]["value"], "");
    assert_eq!(kugou.finds(), 0);
}

#[tokio::test]
async fn pin_on_a_library_song_still_needs_the_callers_credentials() {
    let factory = Factory::new(true, sources(&[kugou()])).await;
    factory
        .get_json(&format!(
            "/rest/setLyricsChoice?id=lib1&candidate=kugou:1.a&{}",
            alice()
        ))
        .await;

    let refused = factory
        .get_json(&format!(
            "/rest/getLyricsBySongId?id=lib1&f=json&{}",
            octo_auth("alice", "bad")
        ))
        .await;

    assert_eq!(refused["status"], "failed");
}

#[tokio::test]
async fn pin_enhanced_client_gets_the_word_cues() {
    let factory = Factory::new(true, sources(&[kugou()])).await;
    let id = factory.some_song();
    factory
        .get_json(&format!(
            "/rest/setLyricsChoice?id={id}&candidate=kugou:1.a&{}",
            alice()
        ))
        .await;

    let rich = factory
        .get_json(&format!(
            "/rest/getLyricsBySongId?id={id}&f=json&enhanced=true&{}",
            alice()
        ))
        .await;

    let lyrics = &rich["lyricsList"]["structuredLyrics"][0];
    assert_eq!(lyrics["kind"], "main");
    let cues: Vec<&str> = lyrics["cueLine"][0]["cue"]
        .as_array()
        .expect("cues")
        .iter()
        .filter_map(|cue| cue["value"].as_str())
        .collect();
    assert_eq!(cues, ["right ", "words"]);
}

// ---- Strict clients ------------------------------------------------------------------------

/// A library song Navidrome has lyrics for, asked without enhanced and with nothing pinned:
/// Navidrome's answer, byte for byte, exactly as before.
#[tokio::test]
async fn strict_client_library_song_gets_navidromes_answer_untouched() {
    let factory = Factory::new(true, sources(&[kugou()])).await;

    let body = factory
        .get_string(&format!("/rest/getLyricsBySongId?id=lib1&f=json&{}", alice()))
        .await;

    assert_eq!(body, LIBRARY_LYRICS_JSON);
}

// ---- The song's own lyrics, ranked ---------------------------------------------------------

/// Navidrome has line-timed lyrics for the song (in its tags), KuGou word-timed ones, and word
/// timing is preferred: the app gets KuGou's, words and all.
#[tokio::test]
async fn library_song_line_timed_own_lyrics_lose_to_word_timed_ones_when_words_are_preferred() {
    let factory = Factory::new(true, sources(&[word_timed_kugou()])).await;

    let response = factory
        .get_json(&format!(
            "/rest/getLyricsBySongId?id=lib1&f=json&enhanced=true&{}",
            alice()
        ))
        .await;

    let lyrics = &response["lyricsList"]["structuredLyrics"][0];
    assert_eq!(lyrics["line"][0]["value"], "kugou words");
    let cues: Vec<&str> = lyrics["cueLine"][0]["cue"]
        .as_array()
        .expect("cues")
        .iter()
        .filter_map(|cue| cue["value"].as_str())
        .collect();
    assert_eq!(cues, ["kugou ", "words"]);
}

/// Word timing not preferred and the song first: its own line-timed lyrics stand, and no
/// source is asked.
#[tokio::test]
async fn library_song_own_lyrics_first_stand_without_asking_a_source() {
    let kugou = word_timed_kugou();
    let factory = Factory::with(true, sources(&[kugou.clone()]), Some("song,kugou"), false).await;

    let body = factory
        .get_string(&format!(
            "/rest/getLyricsBySongId?id=lib1&f=json&enhanced=true&{}",
            alice()
        ))
        .await;

    assert!(body.contains("navidrome's own"), "{body}");
    assert_eq!(kugou.finds(), 0);
}

/// KuGou ranked above the song: its lyrics win even at the same timing.
#[tokio::test]
async fn library_song_source_ranked_above_the_song_wins() {
    let factory = Factory::with(true, sources(&[kugou()]), Some("kugou,song"), false).await;

    let body = factory
        .get_string(&format!("/rest/getLyricsBySongId?id=lib1&f=json&{}", alice()))
        .await;

    assert!(body.contains("automatic words"), "{body}");
    assert!(!body.contains("navidrome's own"), "{body}");
}

/// Navidrome is always asked for word cues, so the song's own timing is known, and a client
/// that did not ask for them still gets Navidrome's answer without.
#[tokio::test]
async fn library_song_navidrome_is_asked_for_cues_but_a_strict_client_gets_none() {
    let factory = Factory::with(true, sources(&[kugou()]), Some("song,kugou"), false).await;

    let body = factory
        .get_string(&format!("/rest/getLyricsBySongId?id=lib1&f=json&{}", alice()))
        .await;

    assert_eq!(*factory.navidrome.lyrics_asked_enhanced.lock(), [true]);
    assert!(!body.contains("cueLine"), "{body}");
}

/// A pin made when the song had another id (its file was replaced by a better copy) still
/// answers for it, found by artist and title, and Automatic clears it for good.
#[tokio::test]
async fn pin_follows_the_song_to_its_new_id_and_automatic_clears_it() {
    let factory = Factory::new(true, sources(&[kugou()])).await;
    factory.store.set(LyricsPin::new(
        "old-id",
        "kugou:1.a",
        Some("KuGou".into()),
        Some("[00:01.00]<00:01.00>pinned <00:01.50>words".into()),
        None,
        Some("Some Artist".into()),
        Some("Library Song".into()),
        Some("alice".into()),
        Utc::now(),
    ));

    let pinned = factory
        .get_string(&format!("/rest/getLyricsBySongId?id=lib1&f=json&{}", alice()))
        .await;
    let listed = factory
        .get_json(&format!("/rest/getLyricsCandidates?id=lib1&{}", alice()))
        .await;
    factory
        .get_json(&format!(
            "/rest/setLyricsChoice?id=lib1&candidate=auto&{}",
            alice()
        ))
        .await;

    assert!(pinned.contains("pinned words"), "{pinned}");
    assert_eq!(listed["lyricsCandidates"]["choice"], "kugou:1.a");
    assert!(factory.store.all().is_empty());
}

// ---- Slow lookups --------------------------------------------------------------------------

#[tokio::test]
async fn slow_lookup_tells_the_octo_app_not_yet_then_serves_what_it_found_in_the_background() {
    let slow = SlowSource::new(Duration::from_millis(5500));
    let factory = Factory::new(true, sources(&[slow.clone()])).await;
    let id = factory.some_song();

    // Past the budget: the Octo app is told the lookup is still running, not "none".
    let first = factory
        .get_json(&format!("/rest/getLyricsBySongId?id={id}&f=json&{}", alice()))
        .await;
    assert_eq!(first["status"], "failed");
    assert!(
        first["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Still looking"),
        "{first}"
    );

    // The lookup kept going and was kept, so the next ask gets the lyrics at once. Waited for
    // by the lookup's own answer, not a fixed sleep; the short grace after it is for the
    // answer to be stored.
    tokio::time::timeout(Duration::from_secs(30), slow.answered.notified())
        .await
        .expect("the lookup answered");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let second = factory
        .get_json(&format!("/rest/getLyricsBySongId?id={id}&f=json&{}", alice()))
        .await;
    assert_eq!(second["status"], "ok");
    assert_eq!(first_line(&second), "found late");
    assert_eq!(slow.finds.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn slow_lookup_other_clients_still_get_the_ordinary_empty_list() {
    let factory = Factory::new(true, sources(&[SlowSource::new(Duration::from_secs(10))])).await;
    let id = factory.some_song();

    let other = factory
        .get_json(&format!(
            "/rest/getLyricsBySongId?id={id}&f=json&{}",
            auth("alice", "good", "Symfonium")
        ))
        .await;

    assert_eq!(other["status"], "ok");
    assert!(
        other["lyricsList"]["structuredLyrics"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{other}"
    );
}

#[tokio::test]
async fn strict_client_external_song_gets_lines_without_cues_or_kind() {
    let kugou = kugou();
    kugou.answer_with("[00:01.00]<00:01.00>timed <00:01.50>words<00:02.00>");
    let factory = Factory::new(true, sources(&[kugou])).await;
    let id = factory.some_song();

    let body = factory
        .get_string(&format!("/rest/getLyricsBySongId?id={id}&f=json&{}", alice()))
        .await;

    assert_eq!(
        body,
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1","lyricsList":{"structuredLyrics":[{"lang":"xxx","synced":true,"displayArtist":"Some Artist","displayTitle":"Some Song","offset":0,"line":[{"start":1000,"value":"timed words"}]}]}}}"#
    );
}

// ---- The legacy getLyrics ------------------------------------------------------------------

#[tokio::test]
async fn legacy_get_lyrics_navidrome_has_none_gets_the_plain_words() {
    let kugou = kugou();
    kugou.answer_with("[00:01.00]<00:01.00>timed <00:01.50>words\n[00:03.00]next");
    let factory = Factory::new(true, sources(&[kugou])).await;

    let json = factory
        .get_json(&format!(
            "/rest/getLyrics?artist=Some%20Artist&title=Some%20Song&f=json&{}",
            alice()
        ))
        .await;
    let xml = factory
        .get_string(&format!(
            "/rest/getLyrics?artist=Some%20Artist&title=Some%20Song&{}",
            alice()
        ))
        .await;

    assert_eq!(json["lyrics"]["value"], "timed words\nnext");
    let root = XElement::parse(&xml).expect("XML");
    let elements: Vec<&XElement> = root.elements().collect();
    assert_eq!(elements.len(), 1, "{xml}");
    assert_eq!(elements[0].value(), "timed words\nnext");
}

#[tokio::test]
async fn legacy_get_lyrics_fetch_off_is_navidromes_answer() {
    let kugou = kugou();
    let factory = Factory::new(false, sources(&[kugou.clone()])).await;

    let json = factory
        .get_json(&format!(
            "/rest/getLyrics?artist=Some%20Artist&title=Some%20Song&f=json&{}",
            alice()
        ))
        .await;

    assert_eq!(json["lyrics"]["value"], "");
    assert_eq!(kugou.finds(), 0);
}

// ---- Extensions ----------------------------------------------------------------------------

#[tokio::test]
async fn extensions_song_lyrics_two_always_octo_lyrics_only_while_lookups_run() {
    for fetch in [true, false] {
        let factory = Factory::new(fetch, sources(&[kugou()])).await;

        let response = factory.get_json("/rest/getOpenSubsonicExtensions?f=json").await;

        let extensions: HashMap<String, Vec<i64>> = response["openSubsonicExtensions"]
            .as_array()
            .expect("extensions")
            .iter()
            .map(|e| {
                (
                    e["name"].as_str().unwrap_or_default().to_string(),
                    e["versions"]
                        .as_array()
                        .map(|v| v.iter().filter_map(Value::as_i64).collect())
                        .unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(extensions["songLyrics"], [1, 2], "fetch={fetch}");
        assert_eq!(extensions.contains_key("octoLyrics"), fetch, "fetch={fetch}");
        if fetch {
            assert_eq!(extensions["octoLyrics"], [1]);
        }
        assert_eq!(extensions["octoAcquisitions"], [1, 2, 3, 4], "fetch={fetch}");
    }
}
