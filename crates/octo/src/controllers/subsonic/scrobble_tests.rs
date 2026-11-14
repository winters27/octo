//! `/rest/scrobble` through the app: Last.fm and ListenBrainz scrobbles, a play sent again after
//! a failure, and an API key sign-in, named by Navidrome's tokenInfo.
//!
//! Navidrome and ListenBrainz are wiremock servers and Last.fm is a small local fake that checks
//! every call's signature, as the real one does; the scrobble and ListenBrainz services are
//! pointed at them.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{TimeDelta, Utc};
use futures::future::BoxFuture;
use indexmap::IndexMap;
use octo_core::common::Clock;
use octo_core::last_fm::last_fm_scrobble_service::sign;
use octo_core::settings::{AppSettings, LastFmUserSession};
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use parking_lot::Mutex;
use tokio::sync::watch;
use wiremock::matchers::{any, method};
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

use super::test_app::*;
use crate::app::AppState;
use crate::http::pipeline::App;
use crate::services::common::listening_plays::ListeningPlays;
use crate::services::last_fm::{LastFmScrobbleService, ScrobbleTime, ScrobbleTuning};
use crate::services::listen_brainz::ListenBrainzService;
use crate::services::subsonic::{RecentScrobbles, RequestIdentity};

const API_KEY: &str = "0123456789abcdef0123456789abcdef";
const SECRET: &str = "s3cr3t";

// ---------------------------------------------------------------------------------------
// Last.fm
// ---------------------------------------------------------------------------------------

type Call = IndexMap<String, String>;
type Hold = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// Last.fm's web service as far as scrobbling uses it (`FakeLastFm`).
#[derive(Default)]
struct FakeLastFm {
    calls: Mutex<Vec<Call>>,
    /// What the next calls fail with, in order: a Last.fm error code.
    failures: Mutex<VecDeque<i32>>,
    /// Holds every call open until the returned future completes.
    hold: Mutex<Option<Hold>>,
}

impl FakeLastFm {
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().clone()
    }

    fn calls_to(&self, method: &str) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|call| call.get("method").map(String::as_str) == Some(method))
            .collect()
    }

    /// Starts the fake on a local port; answers at `<base>/2.0/`.
    async fn serve(self: &Arc<Self>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a local port");
        let address = listener.local_addr().expect("an address");
        let app = Router::new().fallback(last_fm).with_state(Arc::clone(self));
        tokio::spawn(async move { axum::serve(listener, app).await });
        format!("http://{address}/2.0/")
    }
}

async fn last_fm(State(fake): State<Arc<FakeLastFm>>, body: String) -> Response {
    let call: Call = url::form_urlencoded::parse(body.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    // Only a Last.fm call counts. The fake's port may have been another test's address that
    // nothing listened on (`refused_address`), and that test's background calls can still
    // arrive here: they carry no Last.fm method.
    if !call.contains_key("method") {
        return last_fm_error(3, "Not a Last.fm call");
    }
    fake.calls.lock().push(call.clone());
    let hold = fake.hold.lock().clone();
    if let Some(hold) = hold {
        hold().await;
    }

    let signature = sign(call.iter().map(|(k, v)| (k.as_str(), v.as_str())), SECRET);
    if call.get("api_sig") != Some(&signature) {
        return last_fm_error(13, "Invalid method signature supplied");
    }
    let failure = fake.failures.lock().pop_front();
    if let Some(failure) = failure {
        return last_fm_error(failure, "Refused by the fixture");
    }
    let json = match call.get("method").map(String::as_str) {
        Some("track.scrobble") => {
            let accepted = call.keys().filter(|key| key.starts_with("artist[")).count();
            format!(r#"{{"scrobbles":{{"@attr":{{"accepted":{accepted},"ignored":0}},"scrobble":[]}}}}"#)
        }
        Some("track.updateNowPlaying") => {
            r##"{"nowplaying":{"ignoredMessage":{"code":"0","#text":""}}}"##.into()
        }
        _ => return last_fm_error(3, "Invalid Method - No method with that name in this package"),
    };
    (StatusCode::OK, [("content-type", "application/json")], json).into_response()
}

fn last_fm_error(code: i32, message: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        [("content-type", "application/json")],
        serde_json::json!({ "error": code, "message": message }).to_string(),
    )
        .into_response()
}

// ---------------------------------------------------------------------------------------
// Navidrome (`RadioUpstreamHandler`) and ListenBrainz
// ---------------------------------------------------------------------------------------

#[derive(Default)]
struct UpstreamState {
    relayed_scrobble_ids: Mutex<Vec<String>>,
    relayed_scrobble_times: Mutex<Vec<String>>,
    /// How many getSong calls fail with a 503 before they answer again.
    get_song_failures: AtomicUsize,
    token_info_calls: AtomicUsize,
    /// When set, tokenInfo fails though the key itself is accepted.
    token_info_fails: AtomicBool,
    /// How long tokenInfo takes to answer.
    token_info_delay: Mutex<Duration>,
}

/// Navidrome as the radio fixture answered it: `u=bad` fails every call, the API key `bob-key`
/// belongs to bob (any other key fails, and so does `u` beside a key), getSong describes any
/// id, and every other call is ok.
#[derive(Clone, Default)]
struct Upstream(Arc<UpstreamState>);

fn ok_json(fields: &str) -> String {
    let fields = if fields.is_empty() {
        String::new()
    } else {
        format!(",{fields}")
    };
    format!(r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1"{fields}}}}}"#)
}

fn failed(format: &str) -> ResponseTemplate {
    if format == "xml" {
        ResponseTemplate::new(200).set_body_raw(
            br#"<subsonic-response xmlns="http://subsonic.org/restapi" status="failed" version="1.16.1"><error code="40" message="Wrong username or password"/></subsonic-response>"#.to_vec(),
            "application/xml",
        )
    } else {
        navidrome_json(NAVIDROME_WRONG_PASSWORD)
    }
}

impl Respond for Upstream {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let state = &self.0;
        let path = request.url.path().trim_matches('/').to_string();
        let pairs: Vec<(String, String)> = request.url.query_pairs().into_owned().collect();
        let first = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        let all = |key: &str| -> Vec<String> {
            pairs
                .iter()
                .filter(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .collect()
        };
        let format = first("f").unwrap_or_else(|| "json".into());
        let username = first("u").unwrap_or_default();
        if username == "bad" {
            return failed(&format);
        }
        let api_key = first("apiKey");
        if let Some(key) = &api_key
            && (key != "bob-key" || !username.is_empty())
        {
            return failed(&format);
        }
        if path.eq_ignore_ascii_case("rest/tokenInfo") {
            state.token_info_calls.fetch_add(1, Ordering::SeqCst);
            let answer = if state.token_info_fails.load(Ordering::SeqCst) || api_key.is_none() {
                navidrome_json(NAVIDROME_WRONG_PASSWORD)
            } else {
                navidrome_json(&ok_json(r#""tokenInfo":{"username":"bob"}"#))
            };
            return answer.set_delay(*state.token_info_delay.lock());
        }
        if path.eq_ignore_ascii_case("rest/scrobble") {
            *state.relayed_scrobble_ids.lock() = all("id");
            *state.relayed_scrobble_times.lock() = all("time");
        }
        if path.eq_ignore_ascii_case("rest/getSong") {
            if state
                .get_song_failures
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                return ResponseTemplate::new(503);
            }
            let id = first("id").unwrap_or_else(|| "song".into());
            return navidrome_json(&ok_json(&format!(
                r#""song":{{"id":"{id}","artist":"Artist {id}","title":"Title {id}","album":"Album {id}","genre":"Rock","duration":180}}"#
            )));
        }
        if path.eq_ignore_ascii_case("rest/ping") || path.eq_ignore_ascii_case("rest/scrobble") {
            return if format == "xml" {
                ResponseTemplate::new(200).set_body_raw(
                    br#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"></subsonic-response>"#.to_vec(),
                    "application/xml",
                )
            } else {
                navidrome_json(&ok_json(r#""scrobble":{}"#))
            };
        }
        navidrome_json(&ok_json(""))
    }
}

/// The app over the stand-in upstreams, as far as the scrobble tests use it.
struct Fixture {
    upstream: Upstream,
    _navidrome: MockServer,
    listen_brainz: MockServer,
    last_fm: Arc<FakeLastFm>,
    state: AppState,
    app: App,
}

impl Fixture {
    /// The fixture with Last.fm scrobbling and Last.fm library plays switched as given.
    async fn new(last_fm_scrobbling: bool, last_fm_library_plays: bool) -> Fixture {
        Self::with(last_fm_scrobbling, last_fm_library_plays, None).await
    }

    async fn with(
        last_fm_scrobbling: bool,
        last_fm_library_plays: bool,
        identity: Option<RequestIdentity>,
    ) -> Fixture {
        let upstream = Upstream::default();
        // Servers of their own, never wiremock's shared pool: a pooled server can still be
        // answering requests another test sent it before it was handed over here.
        let navidrome = MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(upstream.clone())
            .mount(&navidrome)
            .await;
        let listen_brainz = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .respond_with(navidrome_json(r#"{"status":"ok"}"#))
            .mount(&listen_brainz)
            .await;
        let last_fm = Arc::new(FakeLastFm::default());
        let last_fm_url = last_fm.serve().await;

        let mut settings: AppSettings = settings(&navidrome.uri());
        settings.listen_brainz.token = "lb-default-token".into();
        settings
            .listen_brainz
            .user_tokens
            .insert("bob".into(), "lb-bob-token".into());
        settings.last_fm.enable_radio = true;
        settings.last_fm.enable_personalized_stations = true;
        settings.last_fm.enable_discovery_stations = true;
        if last_fm_scrobbling {
            // Only when asked for: with a key, radio would start calling Last.fm too.
            settings.last_fm.api_key = API_KEY.into();
            settings.last_fm.api_secret = SECRET.into();
            settings.last_fm.scrobble_library_plays = last_fm_library_plays;
            settings.last_fm.user_sessions.insert(
                "bob".into(),
                LastFmUserSession {
                    session_key: "sk-bob".into(),
                    last_fm_user: "lfm-bob".into(),
                },
            );
            // Fails the fixture's credential check, so a session for it must still send nothing.
            settings.last_fm.user_sessions.insert(
                "bad".into(),
                LastFmUserSession {
                    session_key: "sk-bad".into(),
                    last_fm_user: String::new(),
                },
            );
        }

        let lb_url = listen_brainz.uri();
        let state = state_with(settings, |inner| {
            inner.last_fm_scrobbles = Arc::new(LastFmScrobbleService::with_parts(
                reqwest::Client::new(),
                &last_fm_url,
                inner.settings.clone(),
                inner.settings_writer.clone(),
                ScrobbleTuning {
                    retry_delay: TimeDelta::milliseconds(50),
                    ..Default::default()
                },
                ScrobbleTime::system(),
            ));
            inner.listen_brainz = Arc::new(ListenBrainzService::with_parts(
                reqwest::Client::new(),
                &lb_url,
                inner.settings.clone(),
            ));
            // One minute for the whole test, so plays sent without a time never straddle two.
            inner.recent_scrobbles = Arc::new(RecentScrobbles::with_clock(Clock::fixed(Utc::now())));
            if let Some(identity) = identity {
                inner.request_identity = Arc::new(identity);
            }
        });
        let app = app(&state);
        Fixture {
            upstream,
            _navidrome: navidrome,
            listen_brainz,
            last_fm,
            state,
            app,
        }
    }

    fn register_outside_song(&self, title: &str, duration: i32) -> String {
        self.state.external_id_registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("Bladee".into()),
            title: Some(title.into()),
            album: Some("Icedancer".into()),
            duration: Some(duration),
            ..Default::default()
        })
    }

    /// `RegisterOutsideSong`: Bladee's "Be Nice 2 Me".
    fn outside_song(&self) -> String {
        self.register_outside_song("Be Nice 2 Me", 154)
    }

    async fn get(&self, uri: &str) -> String {
        let reply = get(&self.app, uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}: {}", reply.text());
        reply.text()
    }

    async fn when_idle(&self) {
        let service = Arc::clone(&self.state.last_fm_scrobbles);
        until(move || service.outstanding() == 0).await;
    }

    /// The listens ListenBrainz took once Last.fm is idle and ListenBrainz has `expected` of
    /// them, or after two seconds, whichever comes first: every sink a test asserts is waited
    /// for before it is read.
    async fn listens(&self, expected: usize) -> Vec<(String, String)> {
        self.when_idle().await;
        for _ in 0..200 {
            let listens = self.listen_brainz_submissions().await;
            if listens.len() == expected {
                return listens;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.listen_brainz_submissions().await
    }

    /// The listens ListenBrainz took: `(authorization, body)` of each submission. Only real
    /// submissions count; a stray request from another test that reaches this port does not.
    async fn listen_brainz_submissions(&self) -> Vec<(String, String)> {
        self.listen_brainz
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/1/submit-listens")
            .map(|r| {
                (
                    r.headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string(),
                    String::from_utf8_lossy(&r.body).into_owned(),
                )
            })
            .collect()
    }

    fn relayed_ids(&self) -> Vec<String> {
        self.upstream.0.relayed_scrobble_ids.lock().clone()
    }

    fn relayed_times(&self) -> Vec<String> {
        self.upstream.0.relayed_scrobble_times.lock().clone()
    }

    fn token_info_calls(&self) -> usize {
        self.upstream.0.token_info_calls.load(Ordering::SeqCst)
    }

    fn plays(&self, user: &str) -> usize {
        self.state.last_fm_radio_state.get_user(user).plays.len()
    }
}

/// Waits until `condition` holds.
async fn until(condition: impl Fn() -> bool) {
    for _ in 0..1000 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the condition never held");
}

fn minutes_ago_ms(minutes: i64) -> i64 {
    (Utc::now() - TimeDelta::minutes(minutes)).timestamp_millis()
}

// ---------------------------------------------------------------------------------------
// Scrobbles
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn outside_song_completed_play_is_scrobbled() {
    let fixture = Fixture::new(true, true).await;
    let id = fixture.outside_song();

    let played_at = (Utc::now() - TimeDelta::minutes(5)).timestamp();
    let body = fixture
        .get(&format!(
            "/rest/scrobble?u=bob&t=token&s=salt&f=json&id={id}&submission=true&time={played_at}000"
        ))
        .await;
    fixture.when_idle().await;

    assert!(body.contains(r#""status":"ok""#), "{body}");
    let calls = fixture.last_fm.calls_to("track.scrobble");
    assert_eq!(calls.len(), 1, "{calls:?}");
    let call = &calls[0];
    assert_eq!(call["sk"], "sk-bob");
    assert_eq!(call["artist[0]"], "Bladee");
    assert_eq!(call["track[0]"], "Be Nice 2 Me");
    assert_eq!(call["album[0]"], "Icedancer");
    assert_eq!(call["duration[0]"], "154");
    assert_eq!(call["timestamp[0]"], played_at.to_string());
    assert!(fixture.last_fm.calls_to("track.updateNowPlaying").is_empty());
}

#[tokio::test]
async fn a_scrobble_tells_listen_first_what_the_player_did() {
    let fixture = Fixture::new(false, false).await;
    let first = fixture.register_outside_song("Be Nice 2 Me", 154);
    let second = fixture.register_outside_song("Obedient", 160);
    let listening = fixture.state.heart_acquisition_coordinator.listening();
    let player = ListeningPlays::player_key("bob", None, "Octo");
    let scrobble = |c: &str, id: &str, submission: Option<bool>| {
        let submission = submission.map(|s| format!("&submission={s}")).unwrap_or_default();
        format!("/rest/scrobble?u=bob&t=token&s=salt&f=json&c={c}&id={id}{submission}")
    };

    let _wait = listening.hold(&player, &first, Duration::from_secs(600)).unwrap();
    // Its own now-playing, another app's, and a scrobble that sends no submission change nothing.
    fixture.get(&scrobble("Octo", &first, Some(false))).await;
    fixture.get(&scrobble("DSub", &second, Some(false))).await;
    fixture.get(&scrobble("Octo", &first, None)).await;
    assert_eq!(listening.waiting_song(&player), Some(first.clone()));

    // A finish at once is a skip.
    fixture.get(&scrobble("Octo", &first, Some(true))).await;
    assert!(listening.waiting_song(&player).is_none());

    // Another song's now-playing on the same player drops the wait too.
    let _wait = listening.hold(&player, &first, Duration::from_secs(600)).unwrap();
    fixture.get(&scrobble("Octo", &second, Some(false))).await;
    assert!(listening.waiting_song(&player).is_none());
}

#[tokio::test]
async fn outside_song_start_of_play_is_now_playing() {
    let fixture = Fixture::new(true, true).await;
    let id = fixture.outside_song();

    fixture
        .get(&format!(
            "/rest/scrobble?u=bob&t=token&s=salt&f=json&id={id}&submission=false"
        ))
        .await;
    fixture.when_idle().await;

    let calls = fixture.last_fm.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let call = &calls[0];
    assert_eq!(call["method"], "track.updateNowPlaying");
    assert_eq!(call["artist"], "Bladee");
    assert_eq!(call["track"], "Be Nice 2 Me");
    assert_eq!(call["album"], "Icedancer");
    assert_eq!(call["duration"], "154");
}

/// Navidrome scrobbles only a listener linked in its own settings, so Octo sends library plays
/// too, and still relays them so Navidrome's play counts stay right.
#[tokio::test]
async fn library_song_reaches_last_fm_and_is_still_relayed() {
    let fixture = Fixture::new(true, true).await;

    fixture
        .get("/rest/scrobble?u=bob&t=token&s=salt&f=json&id=one&submission=false")
        .await;
    fixture.when_idle().await;
    let playing = fixture.last_fm.calls_to("track.updateNowPlaying");
    assert_eq!(playing.len(), 1, "{playing:?}");
    assert_eq!(playing[0]["artist"], "Artist one");
    assert_eq!(playing[0]["track"], "Title one");

    let played_at = (Utc::now() - TimeDelta::minutes(5)).timestamp();
    fixture
        .get(&format!(
            "/rest/scrobble?u=bob&t=token&s=salt&f=json&id=one&submission=true&time={played_at}000"
        ))
        .await;
    fixture.when_idle().await;

    assert_eq!(fixture.relayed_ids(), ["one"]);
    let calls = fixture.last_fm.calls_to("track.scrobble");
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["artist[0]"], "Artist one");
    assert_eq!(calls[0]["track[0]"], "Title one");
    assert_eq!(calls[0]["album[0]"], "Album one");
    assert_eq!(calls[0]["timestamp[0]"], played_at.to_string());
}

/// Left to a Navidrome linked to Last.fm itself, a library play is not sent twice.
#[tokio::test]
async fn library_song_left_to_navidrome_never_reaches_last_fm() {
    let fixture = Fixture::new(true, false).await;

    fixture
        .get("/rest/scrobble?u=bob&t=token&s=salt&f=json&id=one&submission=false")
        .await;
    fixture
        .get("/rest/scrobble?u=bob&t=token&s=salt&f=json&id=one&submission=true")
        .await;
    fixture.when_idle().await;

    assert_eq!(fixture.relayed_ids(), ["one"]);
    assert!(fixture.last_fm.calls().is_empty());
}

/// With no relay, a ping is the credential check. Failing it sends nothing, or anyone could
/// scrobble to a listener's Last.fm by naming them.
#[tokio::test]
async fn wrong_credentials_send_nothing() {
    let fixture = Fixture::new(true, true).await;
    let id = fixture.outside_song();

    // "bad" fails the fixture's ping; it is given a session so only the check stops it.
    let body = fixture
        .get(&format!("/rest/scrobble?u=bad&f=json&id={id}&submission=true"))
        .await;
    fixture.when_idle().await;

    assert!(body.contains("failed"), "{body}");
    assert!(fixture.last_fm.calls().is_empty());
}

#[tokio::test]
async fn user_without_a_session_sends_nothing() {
    let fixture = Fixture::new(true, true).await;
    let id = fixture.outside_song();

    fixture
        .get(&format!(
            "/rest/scrobble?u=alice&t=token&s=salt&f=json&id={id}&submission=false"
        ))
        .await;
    fixture
        .get(&format!(
            "/rest/scrobble?u=alice&t=token&s=salt&f=json&id={id}&submission=true"
        ))
        .await;
    fixture.when_idle().await;

    assert!(fixture.last_fm.calls().is_empty());
    // ListenBrainz still has alice's default token, so the play was not lost there.
    assert_eq!(fixture.listens(1).await.len(), 1);
}

#[tokio::test]
async fn invalid_session_disconnects_and_stops_sending() {
    let fixture = Fixture::new(true, true).await;
    let id = fixture.outside_song();
    fixture
        .last_fm
        .failures
        .lock()
        .push_back(LastFmScrobbleService::ERROR_INVALID_SESSION);

    let service = Arc::clone(&fixture.state.last_fm_scrobbles);
    let bob = move || {
        service
            .users(Vec::<String>::new())
            .into_iter()
            .find(|user| user.user == "bob")
            .expect("bob is listed")
    };

    let url = format!("/rest/scrobble?u=bob&t=token&s=salt&f=json&id={id}&submission=true");
    fixture.get(&url).await;
    let refused = bob.clone();
    until(move || refused().notice.is_some()).await;
    fixture.get(&url).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(fixture.last_fm.calls().len(), 1);
    assert!(!bob().connected);
}

/// One submission for several ids is for all of them, as Navidrome reads it: two songs
/// starting are two Now Playings, not a Now Playing and a finished play.
#[tokio::test]
async fn one_submission_flag_applies_to_every_id() {
    let fixture = Fixture::new(true, true).await;
    let first = fixture.outside_song();
    let second = fixture.register_outside_song("Hahaha", 160);

    fixture
        .get(&format!(
            "/rest/scrobble?u=bob&t=token&s=salt&f=json&id={first}&id={second}&submission=false"
        ))
        .await;
    fixture.when_idle().await;

    assert_eq!(fixture.last_fm.calls_to("track.updateNowPlaying").len(), 2);
    assert!(fixture.last_fm.calls_to("track.scrobble").is_empty());
    assert!(fixture.listens(0).await.is_empty());
    assert_eq!(fixture.plays("bob"), 0);
}

/// One time for two ids, one of them outside. Navidrome refuses times that do not pair with
/// ids; relaying the lone time with the library id alone would have paired them wrongly.
#[tokio::test]
async fn times_that_do_not_pair_with_ids_are_not_relayed() {
    // Library plays left to Navidrome, so the one play sent to Last.fm is the outside one.
    let fixture = Fixture::new(true, false).await;
    let outside = fixture.outside_song();
    let stale = (Utc::now() - TimeDelta::days(3)).timestamp_millis();

    fixture
        .get(&format!(
            "/rest/scrobble?u=bob&t=token&s=salt&f=json&id={outside}&id=one&submission=true&time={stale}"
        ))
        .await;
    fixture.when_idle().await;

    assert_eq!(fixture.relayed_ids(), ["one"]);
    assert!(fixture.relayed_times().is_empty());
    // Nor is it pinned on the outside song: the play is dated when it arrived.
    let calls = fixture.last_fm.calls_to("track.scrobble");
    assert_eq!(calls.len(), 1, "{calls:?}");
    let sent: i64 = calls[0]["timestamp[0]"].parse().expect("a timestamp");
    assert!(sent > (Utc::now() - TimeDelta::minutes(5)).timestamp());
}

/// A client that posts the same finished play twice played it once.
#[tokio::test]
async fn a_repeated_finished_play_is_learned_once() {
    let fixture = Fixture::new(true, true).await;
    let id = fixture.outside_song();
    let at = minutes_ago_ms(4);

    for _ in 0..2 {
        fixture
            .get(&format!(
                "/rest/scrobble?u=bob&t=token&s=salt&f=json&id={id}&submission=true"
            ))
            .await;
    }
    for _ in 0..2 {
        fixture
            .get(&format!(
                "/rest/scrobble?u=bob&t=token&s=salt&f=json&id={id}&submission=true&time={at}"
            ))
            .await;
    }
    fixture.when_idle().await;

    // Once without a time, once with one: two plays, not four.
    let plays: usize = fixture
        .last_fm
        .calls_to("track.scrobble")
        .iter()
        .map(|call| call.keys().filter(|key| key.starts_with("artist[")).count())
        .sum();
    assert_eq!(plays, 2);
    assert_eq!(fixture.listens(2).await.len(), 2);
}

/// The client's answer does not wait on Last.fm: a stalled call still gets an ok.
#[tokio::test]
async fn scrobble_answer_does_not_wait_for_last_fm() {
    let fixture = Fixture::new(true, true).await;
    let id = fixture.outside_song();
    let release = Arc::new(watch::channel(false).0);
    let gate = Arc::clone(&release);
    *fixture.last_fm.hold.lock() = Some(Arc::new(move || {
        let mut receiver = gate.subscribe();
        Box::pin(async move {
            let _ = receiver.wait_for(|open| *open).await;
        })
    }));

    let body = tokio::time::timeout(
        Duration::from_secs(5),
        fixture.get(&format!(
            "/rest/scrobble?u=bob&t=token&s=salt&f=json&id={id}&submission=true"
        )),
    )
    .await
    .expect("the answer does not wait for Last.fm");

    assert!(body.contains(r#""status":"ok""#), "{body}");
    release.send_replace(true);
    fixture.when_idle().await;
    assert_eq!(fixture.last_fm.calls_to("track.scrobble").len(), 1);
}

// ---------------------------------------------------------------------------------------
// A play sent again
// ---------------------------------------------------------------------------------------

/// A finished play sent twice is learned from once, but only once something did learn from
/// it. When the song could not be looked up the first time, the client's retry is the play.
#[tokio::test]
async fn a_play_not_taken_the_first_time_counts_when_the_client_sends_it_again() {
    let fixture = Fixture::new(false, true).await;
    // Navidrome cannot say what the library song is for a moment.
    fixture.upstream.0.get_song_failures.store(1, Ordering::SeqCst);
    let at = minutes_ago_ms(4);
    let url = format!("/rest/scrobble?u=bob&t=token&s=salt&f=json&id=local-song&submission=true&time={at}");

    fixture.get(&url).await;
    assert_eq!(fixture.plays("bob"), 0);

    fixture.get(&url).await;
    fixture.get(&url).await;

    assert_eq!(fixture.plays("bob"), 1);
}

// ---------------------------------------------------------------------------------------
// Who an API key sign-in is
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn api_key_scrobble_reaches_the_key_owners_last_fm() {
    let fixture = Fixture::new(true, true).await;
    let id = fixture.outside_song();

    let body = fixture
        .get(&format!(
            "/rest/scrobble?apiKey=bob-key&v=1.16.1&c=x&f=json&id={id}&submission=true"
        ))
        .await;
    fixture
        .get(&format!(
            "/rest/scrobble?apiKey=bob-key&v=1.16.1&c=x&f=json&id={id}&submission=false"
        ))
        .await;
    fixture.when_idle().await;

    assert!(body.contains(r#""status":"ok""#), "{body}");
    let scrobbles = fixture.last_fm.calls_to("track.scrobble");
    assert_eq!(scrobbles.len(), 1, "{scrobbles:?}");
    assert_eq!(scrobbles[0]["sk"], "sk-bob");
    assert_eq!(fixture.last_fm.calls_to("track.updateNowPlaying").len(), 1);
    let submissions = fixture.listens(1).await;
    assert_eq!(submissions.len(), 1, "{submissions:?}");
    assert_eq!(submissions[0].0, "Token lb-bob-token");
    assert_eq!(fixture.plays("bob"), 1);
    // Asked once, then remembered.
    assert_eq!(fixture.token_info_calls(), 1);
}

#[tokio::test]
async fn api_key_scrobble_when_navidrome_will_not_say_whose_learns_nothing() {
    let fixture = Fixture::new(true, true).await;
    fixture.upstream.0.token_info_fails.store(true, Ordering::SeqCst);
    let id = fixture.outside_song();

    let body = fixture
        .get(&format!(
            "/rest/scrobble?apiKey=bob-key&v=1.16.1&c=x&f=json&id={id}&submission=true"
        ))
        .await;
    fixture.when_idle().await;

    assert!(body.contains(r#""status":"ok""#), "{body}");
    assert_eq!(fixture.token_info_calls(), 1);
    assert!(fixture.last_fm.calls().is_empty());
    assert!(fixture.listens(0).await.is_empty());
    assert_eq!(fixture.plays("bob"), 0);
}

/// A key Navidrome refuses is never looked up: the credential check comes first.
#[tokio::test]
async fn unknown_api_key_is_never_looked_up() {
    let fixture = Fixture::new(true, true).await;
    let id = fixture.outside_song();

    let body = fixture
        .get(&format!(
            "/rest/scrobble?apiKey=stranger&v=1.16.1&c=x&f=json&id={id}&submission=true"
        ))
        .await;
    fixture.when_idle().await;

    assert!(body.contains("failed"), "{body}");
    assert_eq!(fixture.token_info_calls(), 0);
    assert!(fixture.last_fm.calls().is_empty());
}

/// A Navidrome whose tokenInfo will not say is not asked again on every request.
#[tokio::test]
async fn api_key_navidrome_would_not_name_is_not_asked_again_straight_away() {
    let fixture = Fixture::new(true, true).await;
    fixture.upstream.0.token_info_fails.store(true, Ordering::SeqCst);
    let id = fixture.outside_song();

    for _ in 0..3 {
        fixture
            .get(&format!(
                "/rest/scrobble?apiKey=bob-key&v=1.16.1&c=x&f=json&id={id}&submission=false"
            ))
            .await;
    }

    assert_eq!(fixture.token_info_calls(), 1);
    assert!(fixture.last_fm.calls().is_empty());
}

/// Only briefly, though: once the wait is over the key is asked about again.
#[tokio::test]
async fn api_key_navidrome_would_not_name_is_asked_again_after_a_while() {
    let fixture = Fixture::with(
        true,
        true,
        Some(RequestIdentity::with_unnamed_lifetime(Duration::from_millis(1))),
    )
    .await;
    fixture.upstream.0.token_info_fails.store(true, Ordering::SeqCst);
    let id = fixture.outside_song();
    let url = format!("/rest/scrobble?apiKey=bob-key&v=1.16.1&c=x&f=json&id={id}&submission=false");

    fixture.get(&url).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    fixture.upstream.0.token_info_fails.store(false, Ordering::SeqCst);
    fixture.get(&url).await;
    fixture.when_idle().await;

    assert_eq!(fixture.token_info_calls(), 2);
    assert_eq!(fixture.last_fm.calls_to("track.updateNowPlaying").len(), 1);
}

/// Requests with one key arriving together make one tokenInfo call between them.
#[tokio::test]
async fn api_key_requests_arriving_together_ask_once() {
    let fixture = Fixture::new(true, true).await;
    *fixture.upstream.0.token_info_delay.lock() = Duration::from_millis(300);
    let id = fixture.outside_song();
    let url = format!("/rest/scrobble?apiKey=bob-key&v=1.16.1&c=x&f=json&id={id}&submission=false");

    futures::future::join_all((0..4).map(|_| fixture.get(&url))).await;
    fixture.when_idle().await;

    assert_eq!(fixture.token_info_calls(), 1);
    assert_eq!(fixture.last_fm.calls_to("track.updateNowPlaying").len(), 4);
}

// ---------------------------------------------------------------------------------------
// Now playing moves the songs fetched ahead
// ---------------------------------------------------------------------------------------

/// A now-playing scrobble queues the next songs of the listener's saved queue to be fetched
/// ahead, in the background; a completed play does not.
#[tokio::test]
async fn a_now_playing_scrobble_queues_the_next_songs_ahead() {
    let navidrome = MockServer::start().await;
    Mock::given(any())
        .respond_with(navidrome_json(NAVIDROME_OK))
        .mount(&navidrome)
        .await;
    let folder = tempfile::tempdir().expect("a temp dir");
    let mut app_settings = settings(&navidrome.uri());
    app_settings.prefetch.enabled = true;
    app_settings.prefetch.scope = octo_core::settings::PrefetchScope::All;
    app_settings.prefetch.folder = folder.path().to_string_lossy().into_owned();
    app_settings.soulseek.base_url = Some(format!(
        "http://{}",
        crate::services::test_support::refused_address()
    ));
    let state = AppState::for_tests(app_settings);
    let ids: Vec<String> = ["One", "Two", "Three", "Four", "Five"]
        .iter()
        .map(|title| {
            state.external_id_registry.register(SoulseekRouting {
                kind: RoutingKind::Song,
                artist: Some("Artist".into()),
                title: Some(title.to_string()),
                ..Default::default()
            })
        })
        .collect();
    state
        .prefetch
        .saved_queue("alice", ids.clone(), Some(ids[0].clone()), Some(0));
    let app = app(&state);

    let done = get(
        &app,
        &format!(
            "/rest/scrobble?id={}&submission=true&f=json&{}",
            ids[0],
            auth("alice", "good", "Octo")
        ),
    )
    .await;
    assert_eq!(done.status, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        state.prefetch.status().await.next.is_empty(),
        "a completed play moves nothing"
    );

    let playing = get(
        &app,
        &format!(
            "/rest/scrobble?id={}&submission=false&f=json&{}",
            ids[0],
            auth("alice", "good", "Octo")
        ),
    )
    .await;
    assert_eq!(playing.status, StatusCode::OK);
    let mut next = Vec::new();
    for _ in 0..200 {
        next = state.prefetch.status().await.next;
        if !next.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let titles: Vec<Option<String>> = next.into_iter().map(|song| song.title).collect();
    // From the song after the next: the app buffers the next one itself.
    assert_eq!(
        titles,
        [
            Some("Three".to_string()),
            Some("Four".to_string()),
            Some("Five".to_string())
        ]
    );
}

// ---------------------------------------------------------------------------------------
// Listening
// ---------------------------------------------------------------------------------------

/// The fixture with the radio and ListenBrainz off, so only the listening log learns.
async fn listening_only() -> Fixture {
    let fixture = Fixture::new(false, false).await;
    let mut settings = (*fixture.state.settings.current()).clone();
    settings.last_fm.enable_radio = false;
    settings.last_fm.enable_personalized_stations = false;
    settings.listen_brainz.token = String::new();
    settings.listen_brainz.user_tokens.clear();
    fixture.state.settings.set(settings);
    fixture
}

async fn as_octo_app(fixture: &Fixture, query: &str) {
    let reply = send(
        &fixture.app,
        axum::http::Method::GET,
        &format!("/rest/scrobble?u=bob&t=token&s=salt&c=Octo&f=json&{query}"),
        &[("User-Agent", "Octo/1.9.0 (Android)")],
        "",
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
}

#[tokio::test]
async fn a_completed_play_is_written_down_with_its_app_whatever_the_radio_does() {
    let fixture = listening_only().await;
    // Reported once, three minutes after it started; the next song ends it.
    let started = minutes_ago_ms(3);
    as_octo_app(&fixture, &format!("id=lib1&submission=true&time={started}")).await;
    assert!(fixture.state.listening.plays_of("bob").is_empty());
    as_octo_app(&fixture, "id=lib9&submission=false").await;
    let log = Arc::clone(&fixture.state.listening);
    until(move || !log.plays_of("bob").is_empty()).await;
    let plays = fixture.state.listening.plays_of("bob");
    assert_eq!(plays.len(), 1);
    let play = &plays[0];
    assert_eq!(play.song_id, "lib1");
    assert_eq!(play.title, "Title lib1");
    assert_eq!(play.genre.as_deref(), Some("Rock"));
    assert_eq!(play.app, "Octo 1.9.0 (Android)");
    assert_eq!(play.platform, "android");
    assert_eq!(play.source, "scrobble");
    assert!(play.is_local);
    assert_eq!(play.played_at.timestamp_millis(), started);
    // Timed to when the next song started, never past the song's 180 seconds.
    assert!((170..=180).contains(&play.seconds), "{}", play.seconds);
    // The radio learned nothing: it is off.
    assert_eq!(fixture.plays("bob"), 0);
}

#[tokio::test]
async fn a_heart_while_the_song_plays_marks_the_play() {
    let fixture = listening_only().await;
    as_octo_app(&fixture, "id=lib2&submission=false").await;
    let star = fixture
        .get("/rest/star?u=bob&t=token&s=salt&c=Octo&f=json&id=lib2")
        .await;
    assert!(star.contains(r#""status":"ok""#), "{star}");
    as_octo_app(&fixture, "id=lib2&submission=true").await;
    as_octo_app(&fixture, "id=lib10&submission=false").await;
    let log = Arc::clone(&fixture.state.listening);
    until(move || !log.plays_of("bob").is_empty()).await;
    assert!(fixture.state.listening.plays_of("bob")[0].hearted);
}

#[tokio::test]
async fn with_recording_off_no_play_is_written() {
    let fixture = listening_only().await;
    let mut settings = (*fixture.state.settings.current()).clone();
    settings.listening.enabled = false;
    fixture.state.settings.set(settings);
    as_octo_app(&fixture, "id=lib3&submission=false").await;
    as_octo_app(&fixture, "id=lib3&submission=true").await;
    as_octo_app(&fixture, "id=lib11&submission=false").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(fixture.state.listening.plays_of("bob").is_empty());
    assert!(fixture.state.listening.people().is_empty());
}
