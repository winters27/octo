//! getTransitionProfile and the octoTransitions extension, as the apps see them: a profile
//! octo-sonic made, a song without one, one from an older analysis, a song from outside the
//! library by the id the app plays it by, and a caller who is not signed in.

use std::sync::Arc;

use octo_core::settings::{AppSettings, RadioSourceSettings};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request as MockRequest};

use super::test_app::*;
use crate::app::AppState;
use crate::http::pipeline::App;
use crate::services::local::ILocalLibraryService;
use crate::services::sonic::SonicFile;

const ALICE: &str = "u=alice&t=good&s=salt&v=1.16.1&c=test";
const WRONG: &str = "u=alice&t=bad&s=salt&v=1.16.1&c=test";

/// The profile octo-sonic writes, cut down: the fields the apps read are the profile's own.
fn profile(version: i32) -> Value {
    json!({
        "version": version,
        "durationMs": 241_330,
        "bodyDb": -11.52,
        "head": { "startMs": 0, "hopMs": 100, "levels": "AAEC", "bodyDb": -12.5, "gateDb": -57.5, "boundariesMs": [] },
        "tail": { "startMs": 181_330, "hopMs": 100, "levels": "AAEC", "bodyDb": -11.52, "gateDb": -56.52, "outroStartMs": 230_000, "boundariesMs": [200_000] }
    })
}

/// Navidrome accepts the token "good" and lists no extensions of its own.
async fn navidrome() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(|request: &MockRequest| {
            if query_of(request).get("t").map(String::as_str) != Some("good") {
                return navidrome_json(NAVIDROME_WRONG_PASSWORD);
            }
            match request.url.path() {
                "/rest/getOpenSubsonicExtensions" | "/rest/getOpenSubsonicExtensions.view" => navidrome_json(
                    r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","openSubsonic":true,"openSubsonicExtensions":[]}}"#,
                ),
                _ => navidrome_json(NAVIDROME_OK),
            }
        })
        .mount(&server)
        .await;
    server
}

struct Harness {
    app: App,
    state: AppState,
    library: Arc<FakeLibrary>,
    files: tempfile::TempDir,
    _navidrome: MockServer,
}

async fn harness(sounds_alike: bool) -> Harness {
    let navidrome = navidrome().await;
    let mut settings: AppSettings = settings(&navidrome.uri());
    settings.radio_sources = RadioSourceSettings {
        sounds_alike,
        ..Default::default()
    };
    let library = Arc::new(FakeLibrary::with_external("Yt9aZ", "youtube", "dQw4w9WgXcQ"));
    let held = Arc::clone(&library);
    let state = state_with(settings, move |inner| {
        inner.local_library = held as Arc<dyn ILocalLibraryService>;
    });
    Harness {
        app: app(&state),
        state,
        library,
        files: tempfile::tempdir().expect("a folder"),
        _navidrome: navidrome,
    }
}

impl Harness {
    /// The outside song's file here, which its plays are served, and the stamp a reading of it
    /// keeps.
    fn kept_file(&self, name: &str) -> (String, String) {
        let path = self.files.path().join(name);
        std::fs::write(&path, b"audio").expect("written");
        let path = path.to_string_lossy().into_owned();
        self.library
            .local_paths
            .lock()
            .insert(("youtube".into(), "dQw4w9WgXcQ".into()), path.clone());
        let file = SonicFile::stat(&path).expect("a file");
        let stamp = format!("{}:{}", file.length, file.last_write_ticks);
        (path, stamp)
    }
}

async fn ask(h: &Harness, query: &str) -> Value {
    get(&h.app, &format!("/rest/{query}")).await.json()["subsonic-response"].clone()
}

fn extensions(answer: &Value) -> Vec<String> {
    answer["openSubsonicExtensions"]
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|e| e["name"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn a_ready_profile_comes_whole_with_the_id_asked_for() {
    let h = harness(true).await;
    let store = h.state.sonic_worker.transitions();
    store.set_current_version(1);
    store.put("nd-1", "10:20", None, &profile(1));

    let answer = ask(&h, &format!("getTransitionProfile?{ALICE}&id=nd-1")).await;

    assert_eq!(answer["status"], "ok");
    assert_eq!(answer["type"], "octo");
    let got = &answer["transitionProfile"];
    assert_eq!(got["id"], "nd-1");
    assert_eq!(got["ready"], true);
    let mut expected = profile(1);
    expected["id"] = json!("nd-1");
    expected["ready"] = json!(true);
    assert_eq!(*got, expected);
}

#[tokio::test]
async fn a_song_without_a_profile_is_not_ready_and_says_the_version_made_now() {
    let h = harness(true).await;
    h.state.sonic_worker.transitions().set_current_version(1);

    let answer = ask(&h, &format!("getTransitionProfile.view?{ALICE}&id=nd-2")).await;

    assert_eq!(answer["status"], "ok");
    assert_eq!(
        answer["transitionProfile"],
        json!({ "id": "nd-2", "ready": false, "version": 1 })
    );
}

#[tokio::test]
async fn a_profile_from_an_older_analysis_is_not_handed_out() {
    let h = harness(true).await;
    let store = h.state.sonic_worker.transitions();
    store.set_current_version(1);
    store.put("nd-1", "10:20", None, &profile(1));
    store.set_current_version(2);

    let answer = ask(&h, &format!("getTransitionProfile?{ALICE}&id=nd-1")).await;

    assert_eq!(
        answer["transitionProfile"],
        json!({ "id": "nd-1", "ready": false, "version": 2 })
    );
}

#[tokio::test]
async fn an_outside_song_is_found_by_the_id_the_app_plays_it_by() {
    let h = harness(true).await;
    let store = h.state.sonic_worker.transitions();
    store.set_current_version(1);
    let (path, stamp) = h.kept_file("a.mp3");
    store.put("youtube:dQw4w9WgXcQ", &stamp, Some(&path), &profile(1));

    let answer = ask(&h, &format!("getTransitionProfile?{ALICE}&id=Yt9aZ")).await;

    assert_eq!(answer["transitionProfile"]["id"], "Yt9aZ");
    assert_eq!(answer["transitionProfile"]["ready"], true);
    assert_eq!(answer["transitionProfile"]["tail"]["outroStartMs"], 230_000);
}

/// An outside song's profile is answered only while it was read from the file its play gets
/// now: not while its play streams, not when its play gets another file, and not when that file
/// changed since it was read.
#[tokio::test]
async fn an_outside_song_profiled_from_another_file_than_its_play_gets_is_not_ready() {
    let h = harness(true).await;
    let store = h.state.sonic_worker.transitions();
    store.set_current_version(1);
    let not_ready = json!({ "id": "Yt9aZ", "ready": false, "version": 1 });
    let query = format!("getTransitionProfile?{ALICE}&id=Yt9aZ");

    store.put(
        "youtube:dQw4w9WgXcQ",
        "5:6",
        Some("/music/elsewhere.mp3"),
        &profile(1),
    );
    assert_eq!(
        ask(&h, &query).await["transitionProfile"],
        not_ready,
        "its play streams"
    );

    let (path, stamp) = h.kept_file("a.mp3");
    assert_eq!(
        ask(&h, &query).await["transitionProfile"],
        not_ready,
        "another file"
    );

    store.put("youtube:dQw4w9WgXcQ", "1:1", Some(&path), &profile(1));
    assert_eq!(
        ask(&h, &query).await["transitionProfile"],
        not_ready,
        "changed since"
    );

    store.put("youtube:dQw4w9WgXcQ", &stamp, Some(&path), &profile(1));
    assert_eq!(ask(&h, &query).await["transitionProfile"]["ready"], true);
}

#[tokio::test]
async fn only_a_signed_in_caller_gets_an_answer() {
    let h = harness(true).await;
    let store = h.state.sonic_worker.transitions();
    store.set_current_version(1);
    store.put("nd-1", "10:20", None, &profile(1));

    let refused = ask(&h, &format!("getTransitionProfile?{WRONG}&id=nd-1")).await;
    assert_eq!(refused["status"], "failed");
    assert_eq!(refused["error"]["code"], 40);
    assert!(refused.get("transitionProfile").is_none());

    let missing = ask(&h, &format!("getTransitionProfile?{ALICE}")).await;
    assert_eq!(missing["status"], "failed");
    assert_eq!(missing["error"]["code"], 10);
}

#[tokio::test]
async fn the_extension_is_listed_once_profiles_are_made() {
    let h = harness(true).await;
    let query = format!("getOpenSubsonicExtensions?{ALICE}&f=json");
    // octo-sonic has not said it makes profiles yet.
    assert!(!extensions(&ask(&h, &query).await).contains(&"octoTransitions".to_string()));
    h.state.sonic_worker.transitions().set_current_version(1);
    let listed = ask(&h, &query).await;
    let entry = listed["openSubsonicExtensions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "octoTransitions")
        .cloned();
    assert_eq!(entry, Some(json!({ "name": "octoTransitions", "versions": [1] })));
}

#[tokio::test]
async fn with_sounds_alike_off_the_extension_is_listed_and_profiles_are_served() {
    let off = harness(false).await;
    let query = format!("getOpenSubsonicExtensions?{ALICE}&f=json");
    assert!(!extensions(&ask(&off, &query).await).contains(&"octoTransitions".to_string()));
    off.state.sonic_worker.transitions().set_current_version(1);
    off.state
        .sonic_worker
        .transitions()
        .put("nd-1", "1:1", None, &profile(1));
    assert!(extensions(&ask(&off, &query).await).contains(&"octoTransitions".to_string()));
    let answer = ask(&off, &format!("getTransitionProfile?{ALICE}&id=nd-1")).await;
    assert_eq!(answer["transitionProfile"]["ready"], true);
}
