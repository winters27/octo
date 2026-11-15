//! The version chooser through its handlers, as the apps call them: getStreamVersions,
//! setStreamVersion and getStreamVersionChanges, who may choose, what a pick clears, and who is
//! offered the extensions. The listing on its own is tested in `services/you_tube/stream_versions.rs`,
//! the records and the change sequence in 7.1's registry and store tests.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use chrono::Utc;
use http_body_util::BodyExt;
use octo_core::common::Clock;
use octo_core::family::abilities::{Abilities, Preset};
use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, HeardRecord, HeardSource, WaitingFor};
use octo_core::settings::AppSettings;
use octo_core::soulseek::{LengthSource, RoutingKind, SoulseekRouting};
use octo_subsonic::xml::XElement;
use serde_json::Value;
use wiremock::matchers::{any, path, query_param};
use wiremock::{Mock, MockServer, Request as MockRequest, ResponseTemplate};

use super::extensions::get_open_subsonic_extensions;
use super::stream_versions::{
    BEING_ADDED, CANT_CHOOSE, NO_OCTO_PICK, NOT_OFFERED, PICK_REFUSED, PLAYS_FROM_DOWNLOAD, VIDEO_REFUSED,
    get_stream_version_changes, get_stream_versions, set_stream_version,
};
use super::test_app::*;
use crate::app::AppState;
use crate::services::common::{HeldAcquisition, HeldKind, Holder};
use crate::services::family::FamilyCaller;
use crate::services::local::ILocalLibraryService;
use crate::services::soulseek::external_id_registry::PickState;
use crate::services::you_tube::YouTubeResolver;
use crate::services::you_tube::stream_versions::{ListCaps, StreamVersions};

const EXTENSIONS_JSON: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","openSubsonic":true,"openSubsonicExtensions":[]}}"#;
const EXTENSIONS_XML: &str = r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1" type="navidrome" openSubsonic="true"></subsonic-response>"#;

const SONGS: &str = r#"{"tracks":[{"videoId":"rel","title":"Teardrop","artists":["Massive Attack"],"durationSeconds":330,"videoType":"MUSIC_VIDEO_TYPE_ATV"}]}"#;
const VIDEOS: &str = r#"{"tracks":[
    {"videoId":"omv","title":"Teardrop (Official Video)","artists":["Massive Attack"],"durationSeconds":340,"videoType":"MUSIC_VIDEO_TYPE_OMV"},
    {"videoId":"v0","title":"Teardrop","artists":["Massive Attack"],"durationSeconds":330,"videoType":"MUSIC_VIDEO_TYPE_UGC"}
]}"#;
const CANDIDATES: &str = r#"{"candidates":[
    {"video_id":"live","title":"Massive Attack - Teardrop (Live)","duration":402,"channel":"Fan Uploads"},
    {"video_id":"dead","title":"Massive Attack - Teardrop (Lyrics)","duration":331,"channel":"Lyrics World"},
    {"video_id":"barred","title":"Massive Attack - Teardrop","duration":330,"channel":"Elsewhere"}
],"rank":3}"#;

/// Navidrome: the token `good` signs in; its extension list is empty, in the format asked for.
async fn navidrome() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(|request: &MockRequest| {
            let query = query_of(request);
            if query.get("t").map(String::as_str) != Some("good") {
                return navidrome_json(NAVIDROME_WRONG_PASSWORD);
            }
            match request.url.path() {
                "/rest/getOpenSubsonicExtensions" | "/rest/getOpenSubsonicExtensions.view" => {
                    if query.get("f").map(String::as_str) == Some("json") {
                        navidrome_json(EXTENSIONS_JSON)
                    } else {
                        ResponseTemplate::new(200)
                            .set_body_raw(EXTENSIONS_XML.as_bytes().to_vec(), "application/xml")
                    }
                }
                _ => navidrome_json(NAVIDROME_OK),
            }
        })
        .mount(&server)
        .await;
    server
}

/// The shim: YouTube Music's songs and videos, the ranked candidates, every video playable but
/// `dead` (removed), and the play path's search finding nothing.
async fn shim() -> MockServer {
    let shim = MockServer::start().await;
    Mock::given(path("/ytm/search"))
        .and(query_param("filter", "songs"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SONGS))
        .mount(&shim)
        .await;
    Mock::given(path("/ytm/search"))
        .and(query_param("filter", "videos"))
        .respond_with(ResponseTemplate::new(200).set_body_string(VIDEOS))
        .mount(&shim)
        .await;
    Mock::given(path("/candidates"))
        .respond_with(ResponseTemplate::new(200).set_body_string(CANDIDATES))
        .mount(&shim)
        .await;
    Mock::given(path("/facts"))
        .and(query_param("id", "dead"))
        .respond_with(
            ResponseTemplate::new(410)
                .insert_header("x-octo-fresh", "1")
                .set_body_string(r#"{"error":"video_unavailable","fresh":true}"#),
        )
        .mount(&shim)
        .await;
    Mock::given(path("/facts"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"fresh":true,"size":1000,"itag":"140","ext":"m4a","abr":129.5}"#),
        )
        .mount(&shim)
        .await;
    Mock::given(path("/search"))
        .respond_with(ResponseTemplate::new(404).set_body_string(r#"{"error":"no_hit"}"#))
        .mount(&shim)
        .await;
    shim
}

struct Fixture {
    state: AppState,
    library: Arc<FakeLibrary>,
    shim: MockServer,
    files: tempfile::TempDir,
    _navidrome: MockServer,
}

fn request(endpoint: &str, query: &str, user: &str, caller: Option<FamilyCaller>) -> Request {
    let token = if user == "nobody" { "bad" } else { "good" };
    let mut request = Request::builder()
        .uri(format!(
            "/rest/{endpoint}.view?{query}&u={user}&t={token}&s=salt&v=1.16.1&c=Octo&f=json"
        ))
        .body(Body::empty())
        .expect("a request");
    if let Some(caller) = caller {
        request.extensions_mut().insert(caller);
    }
    request
}

async fn body_of(response: Response) -> String {
    let body = response.into_body().collect().await.expect("read").to_bytes();
    String::from_utf8_lossy(&body).into_owned()
}

async fn envelope(response: Response) -> Value {
    let value: Value = serde_json::from_str(&body_of(response).await).expect("JSON");
    value["subsonic-response"].clone()
}

fn video_heard(video: &str) -> HeardRecord {
    HeardRecord {
        heard_at: Utc::now(),
        source: HeardSource::Video {
            id: video.into(),
            size: None,
            content_type: "audio/mp4".into(),
            itag: None,
        },
        sample_id: None,
    }
}

fn holder(user: &str, video: &str) -> Holder {
    Holder {
        user: user.into(),
        heard: Some(video_heard(video)),
        ..Default::default()
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn versions_of(answer: &Value) -> Vec<String> {
    answer["streamVersions"]["version"]
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|row| row["videoId"].as_str().map(str::to_string))
        .collect()
}

fn assert_refused(answer: &Value, code: i64, words: &str) {
    assert_eq!(answer["status"], "failed", "{answer}");
    assert_eq!(answer["error"]["code"], code, "{answer}");
    assert_eq!(answer["error"]["message"], words, "{answer}");
}

fn member(user: &str, preset: Preset) -> FamilyCaller {
    FamilyCaller::member(user, preset, Abilities::preset(preset))
}

impl Fixture {
    async fn new(change: impl FnOnce(&mut AppSettings)) -> Fixture {
        let navidrome = navidrome().await;
        let shim = shim().await;
        let mut configured = settings(&navidrome.uri());
        change(&mut configured);
        let library = Arc::new(FakeLibrary::default());
        let held = Arc::clone(&library);
        let shim_url = shim.uri();
        let state = state_with(configured, move |inner| {
            inner.local_library = held as Arc<dyn ILocalLibraryService>;
            inner.stream_versions = Arc::new(StreamVersions::new(
                Arc::new(YouTubeResolver::with_base_url(Some(&shim_url))),
                Clock::system(),
                ListCaps {
                    budget: Duration::from_secs(3),
                    music: Duration::from_secs(3),
                    youtube: Duration::from_secs(3),
                    ..ListCaps::default()
                },
            ));
        });
        Fixture {
            state,
            library,
            shim,
            files: tempfile::tempdir().expect("a folder"),
            _navidrome: navidrome,
        }
    }

    /// Teardrop by Massive Attack, an outside song playing from `v0`, listed from `album` (each
    /// album another id of the same song).
    fn song(&self, album: &str) -> String {
        self.song_with(album, |_| {})
    }

    fn song_with(&self, album: &str, change: impl FnOnce(&mut SoulseekRouting)) -> String {
        let mut routing = SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("Massive Attack".into()),
            title: Some("Teardrop".into()),
            album: Some(album.into()),
            you_tube_id: Some("v0".into()),
            shown_duration: Some(330),
            shown_duration_source: LengthSource::Deezer,
            ..Default::default()
        };
        change(&mut routing);
        let id = self.state.external_id_registry.register(routing);
        self.library.add_external(&id, "soulseek", &id);
        id
    }

    async fn versions(&self, id: &str, user: &str) -> Value {
        envelope(
            get_stream_versions(
                State(self.state.clone()),
                request("getStreamVersions", &format!("id={id}"), user, None),
            )
            .await,
        )
        .await
    }

    async fn choose(&self, id: &str, video: &str, user: &str) -> Value {
        self.choose_as(id, video, user, None).await
    }

    async fn choose_as(&self, id: &str, video: &str, user: &str, caller: Option<FamilyCaller>) -> Value {
        envelope(
            set_stream_version(
                State(self.state.clone()),
                request(
                    "setStreamVersion",
                    &format!("id={id}&video={video}"),
                    user,
                    caller,
                ),
            )
            .await,
        )
        .await
    }

    async fn changes(&self, query: &str) -> Value {
        envelope(
            get_stream_version_changes(
                State(self.state.clone()),
                request("getStreamVersionChanges", query, "alice", None),
            )
            .await,
        )
        .await
    }

    fn heard(&self, id: &str, user: &str, video: &str) {
        self.state
            .external_id_registry
            .remember_heard(id, user, video_heard(video), None);
    }

    fn heard_video(&self, id: &str, user: &str) -> Option<String> {
        match self.state.external_id_registry.heard_by(id, user)?.source {
            HeardSource::Video { id, .. } => Some(id),
            _ => None,
        }
    }

    fn pick(&self, id: &str) -> PickState {
        let routing = self
            .state
            .external_id_registry
            .resolve_song(id, false)
            .expect("known")
            .snapshot();
        self.state.external_id_registry.pick_for(&routing)
    }

    fn picked_video(&self, id: &str) -> Option<String> {
        match self.pick(id) {
            PickState::Person(choice) => Some(choice.video),
            _ => None,
        }
    }

    /// A play of the song starts from its first byte: the song switches to what was chosen.
    fn start(&self, id: &str) {
        let registry = &self.state.external_id_registry;
        let routing = registry.resolve_song(id, false).expect("known");
        registry.apply_choice_at_start(&routing);
    }

    async fn asked(&self, at: &str) -> usize {
        self.shim
            .received_requests()
            .await
            .expect("recorded")
            .iter()
            .filter(|r| r.url.path() == at)
            .count()
    }
}

// ---- getStreamVersions --------------------------------------------------------------------

#[tokio::test]
async fn the_listing_shows_the_video_playing_and_the_others_same_song_first() {
    let f = Fixture::new(|_| {}).await;
    let id = f.song("Mezzanine");
    let answer = f.versions(&id, "alice").await;
    assert_eq!(answer["status"], "ok", "{answer}");
    assert_eq!(answer["type"], "octo");
    let versions = &answer["streamVersions"];
    assert_eq!(versions["id"], id.as_str());
    assert_eq!(versions["playsFrom"], "stream");
    assert_eq!(versions["playing"]["videoId"], "v0");
    assert_eq!(versions["playing"]["picked"], "octo");
    assert_eq!(versions["playing"]["title"], "Teardrop");
    assert_eq!(versions["playing"]["duration"], 330);
    assert_eq!(versions["playing"]["mine"], false);
    assert_eq!(versions["busy"], false);
    assert_eq!(versions["missed"], serde_json::json!([]));
    assert!(versions.get("heardVideoId").is_none());
    let shown = versions_of(&answer);
    assert_eq!(shown[0], "rel", "the same song first");
    for video in ["rel", "live", "dead", "barred", "omv"] {
        assert!(shown.iter().any(|v| v == video), "{video} in {shown:?}");
    }
    assert!(
        !shown.iter().any(|v| v == "v0"),
        "the playing video is not among the others"
    );
    assert!(strings(&versions["ids"]).contains(&id));
    let live = versions["version"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["videoId"] == "live")
        .expect("live")
        .clone();
    assert_eq!(
        live,
        serde_json::json!({ "videoId": "live", "title": "Massive Attack - Teardrop (Live)", "channel": "Fan Uploads", "duration": 402, "kind": "video" })
    );
}

#[tokio::test]
async fn the_answer_names_the_callers_heard_video_when_another_plays() {
    let f = Fixture::new(|_| {}).await;
    let id = f.song("Mezzanine");
    f.heard(&id, "alice", "live");
    f.heard(&id, "bob", "v0");
    assert_eq!(
        f.versions(&id, "alice").await["streamVersions"]["heardVideoId"],
        "live"
    );
    assert!(
        f.versions(&id, "bob").await["streamVersions"]
            .get("heardVideoId")
            .is_none()
    );
}

#[tokio::test]
async fn a_song_played_from_a_file_or_downloaded_on_play_lists_no_versions() {
    let f = Fixture::new(|_| {}).await;
    let id = f.song("Mezzanine");
    let file = f.files.path().join("teardrop.flac");
    std::fs::write(&file, b"audio").expect("written");
    f.library.local_paths.lock().insert(
        ("soulseek".into(), id.clone()),
        file.to_string_lossy().into_owned(),
    );
    let answer = f.versions(&id, "alice").await;
    assert_eq!(answer["streamVersions"]["playsFrom"], "file");
    assert!(versions_of(&answer).is_empty());

    let downloads = Fixture::new(|s| s.subsonic.download_on_play = true).await;
    let id = downloads.song("Mezzanine");
    let answer = downloads.versions(&id, "alice").await;
    assert_eq!(answer["streamVersions"]["playsFrom"], "download");
    assert!(versions_of(&answer).is_empty());
    assert_eq!(downloads.asked("/candidates").await, 0, "nothing is listed");
}

#[tokio::test]
async fn busy_says_the_callers_own_heart_album_heart_or_request_of_any_id_runs() {
    let f = Fixture::new(|_| {}).await;
    let x = f.song("Mezzanine");
    let y = f.song("Collected");
    let coordinator = &f.state.heart_acquisition_coordinator;
    for origin in [
        AcquisitionOrigin::Heart,
        AcquisitionOrigin::AlbumHeart,
        AcquisitionOrigin::Request,
    ] {
        let running = coordinator.note_chain(&y, Some("alice"), &[], origin);
        assert_eq!(
            f.versions(&x, "alice").await["streamVersions"]["busy"],
            true,
            "{origin:?}"
        );
        assert_eq!(
            f.versions(&x, "bob").await["streamVersions"]["busy"],
            false,
            "{origin:?}"
        );
        drop(running);
    }
    assert_eq!(f.versions(&x, "alice").await["streamVersions"]["busy"], false);
    let _play = coordinator.note_chain(&x, Some("alice"), &[], AcquisitionOrigin::Play);
    assert_eq!(
        f.versions(&x, "alice").await["streamVersions"]["busy"],
        false,
        "a play's download is not the caller's own"
    );
}

#[tokio::test]
async fn a_pick_from_before_is_shown_as_one() {
    let f = Fixture::new(|_| {}).await;
    let id = f.song_with("Mezzanine", |r| r.video_pick = SoulseekRouting::VIDEO_CHOSEN);
    let answer = f.versions(&id, "alice").await;
    assert_eq!(answer["streamVersions"]["playing"]["videoId"], "v0");
    assert_eq!(answer["streamVersions"]["playing"]["picked"], "earlier");
}

#[tokio::test]
async fn the_listing_refuses_what_cannot_be_given_another_version() {
    let f = Fixture::new(|_| {}).await;
    let id = f.song("Mezzanine");
    assert_eq!(f.versions(&id, "nobody").await["error"]["code"], 40);
    let missing = envelope(
        get_stream_versions(
            State(f.state.clone()),
            request("getStreamVersions", "x=1", "alice", None),
        )
        .await,
    )
    .await;
    assert_eq!(missing["error"]["code"], 10);
    assert_refused(&f.versions("lib1", "alice").await, 70, CANT_CHOOSE);
    f.library.add_external("ext-deezer-song-1", "deezer", "1");
    assert_refused(&f.versions("ext-deezer-song-1", "alice").await, 70, CANT_CHOOSE);
    let untitled = f.song_with("Mezzanine", |r| r.title = None);
    assert_refused(&f.versions(&untitled, "alice").await, 70, CANT_CHOOSE);
}

// ---- setStreamVersion ---------------------------------------------------------------------

#[tokio::test]
async fn a_pick_of_an_offered_video_is_the_songs_on_every_id() {
    let f = Fixture::new(|_| {}).await;
    let x = f.song("Mezzanine");
    let y = f.song("Collected");
    f.versions(&x, "alice").await;
    let answer = f.choose(&x, "live", "alice").await;
    assert_eq!(answer["status"], "ok", "{answer}");
    let playing = &answer["streamVersions"]["playing"];
    assert_eq!(playing["videoId"], "live");
    assert_eq!(playing["picked"], "person");
    assert_eq!(playing["pickedBy"], "alice");
    assert_eq!(playing["mine"], true);
    assert_eq!(playing["kind"], "video");
    assert_eq!(playing["title"], "Massive Attack - Teardrop (Live)");
    let ids = strings(&answer["streamVersions"]["ids"]);
    assert!(ids.contains(&x) && ids.contains(&y), "{ids:?}");
    let shown = versions_of(&answer);
    assert!(!shown.iter().any(|v| v == "live"));
    assert!(
        shown.iter().any(|v| v == "v0"),
        "the video played before is another version now"
    );
    assert_eq!(f.picked_video(&x).as_deref(), Some("live"));
    assert_eq!(f.picked_video(&y).as_deref(), Some("live"));
    let seen_by_bob = f.versions(&y, "bob").await;
    assert_eq!(seen_by_bob["streamVersions"]["playing"]["pickedBy"], "alice");
    assert_eq!(seen_by_bob["streamVersions"]["playing"]["mine"], false);
}

#[tokio::test]
async fn a_video_not_offered_is_listed_once_more_then_refused() {
    let f = Fixture::new(|_| {}).await;
    let x = f.song("Mezzanine");
    // Nothing was offered yet: the listing runs once and finds it.
    assert_eq!(f.choose(&x, "rel", "alice").await["status"], "ok");
    assert_eq!(f.asked("/candidates").await, 1);
    let answer = f.choose(&x, "nope", "alice").await;
    assert_refused(&answer, 70, NOT_OFFERED);
    assert_eq!(f.asked("/candidates").await, 2, "listed once more");
    assert_eq!(f.picked_video(&x).as_deref(), Some("rel"));
}

#[tokio::test]
async fn a_refused_or_removed_video_is_refused_and_nothing_is_stored() {
    let f = Fixture::new(|_| {}).await;
    let x = f.song_with("Mezzanine", |r| r.refused_videos = vec!["barred".into()]);
    f.versions(&x, "alice").await;
    assert_refused(&f.choose(&x, "barred", "alice").await, 70, VIDEO_REFUSED);
    assert_refused(&f.choose(&x, "dead", "alice").await, 0, VIDEO_REFUSED);
    assert!(f.picked_video(&x).is_none());
}

#[tokio::test]
async fn a_pick_waits_while_the_callers_own_download_of_the_song_runs() {
    let f = Fixture::new(|_| {}).await;
    let x = f.song("Mezzanine");
    let y = f.song("Collected");
    f.versions(&x, "alice").await;
    let facts_before = f.asked("/facts").await;
    let running =
        f.state
            .heart_acquisition_coordinator
            .note_chain(&y, Some("alice"), &[], AcquisitionOrigin::Heart);
    assert_refused(&f.choose(&x, "live", "alice").await, 0, BEING_ADDED);
    assert!(
        f.asked("/facts").await > facts_before,
        "the video was checked first"
    );
    assert!(f.picked_video(&x).is_none());
    drop(running);
    assert_eq!(f.choose(&x, "live", "alice").await["status"], "ok");
}

#[tokio::test]
async fn download_modes_listeners_and_kids_cannot_choose() {
    let downloads = Fixture::new(|s| s.subsonic.wait_for_lossless_on_play = true).await;
    let id = downloads.song("Mezzanine");
    assert_refused(
        &downloads.choose(&id, "live", "alice").await,
        0,
        PLAYS_FROM_DOWNLOAD,
    );

    let f = Fixture::new(|_| {}).await;
    let id = f.song("Mezzanine");
    f.versions(&id, "alice").await;
    for preset in [Preset::Listener, Preset::Kid] {
        let answer = f.choose_as(&id, "live", "sam", Some(member("sam", preset))).await;
        assert_refused(&answer, 50, PICK_REFUSED);
        let undo = f.choose_as(&id, "auto", "sam", Some(member("sam", preset))).await;
        assert_refused(&undo, 50, PICK_REFUSED);
    }
    assert!(f.picked_video(&id).is_none());
    let answer = f
        .choose_as(&id, "live", "ede", Some(member("ede", Preset::Member)))
        .await;
    assert_eq!(answer["status"], "ok", "{answer}");
}

#[tokio::test]
async fn choosing_the_video_playing_changes_nothing() {
    let f = Fixture::new(|_| {}).await;
    let x = f.song("Mezzanine");
    f.heard(&x, "alice", "v0");
    let answer = f.choose(&x, "v0", "alice").await;
    assert_eq!(answer["status"], "ok");
    assert_eq!(answer["streamVersions"]["playing"]["videoId"], "v0");
    assert_eq!(answer["streamVersions"]["playing"]["picked"], "octo");
    assert!(matches!(f.pick(&x), PickState::None));
    assert_eq!(f.heard_video(&x, "alice").as_deref(), Some("v0"));
    assert_eq!(f.asked("/facts").await, 0);
}

#[tokio::test]
async fn a_pick_clears_the_choosers_heard_records_and_holds_on_every_id_and_keeps_others() {
    let f = Fixture::new(|_| {}).await;
    let x = f.song("Mezzanine");
    let y = f.song("Collected");
    f.heard(&x, "alice", "v0");
    f.heard(&y, "alice", "v0");
    f.heard(&x, "bob", "v0");
    f.state.soulseek_holds.hold(HeldAcquisition {
        kind: HeldKind::Track,
        provider: "soulseek".into(),
        external_id: y.clone(),
        waiting_for: Some(WaitingFor::NotYetProven),
        holders: vec![holder("alice", "v0"), holder("bob", "v0")],
        ..Default::default()
    });
    f.versions(&x, "alice").await;
    assert_eq!(f.choose(&x, "live", "alice").await["status"], "ok");
    assert!(f.heard_video(&x, "alice").is_none());
    assert!(f.heard_video(&y, "alice").is_none());
    assert_eq!(f.heard_video(&x, "bob").as_deref(), Some("v0"));
    let hold = f
        .state
        .soulseek_holds
        .snapshot()
        .into_iter()
        .find(|held| held.external_id == y)
        .expect("the hold stays");
    let heard_of = |user: &str| {
        hold.holders
            .iter()
            .find(|h| h.user == user)
            .expect("a holder")
            .heard
            .is_some()
    };
    assert!(!heard_of("alice"));
    assert!(heard_of("bob"));
}

#[tokio::test]
async fn going_back_stores_octos_pick_and_an_unreachable_youtube_keeps_the_pick() {
    let f = Fixture::new(|_| {}).await;
    let x = f.song("Mezzanine");
    let other = f.song_with("Mezzanine", |r| r.title = Some("Angel".into()));
    // No pick stands: nothing changes.
    let unchanged = f.choose(&other, "auto", "alice").await;
    assert_eq!(unchanged["status"], "ok");
    assert!(matches!(f.pick(&other), PickState::None));

    f.versions(&x, "alice").await;
    f.choose(&x, "live", "alice").await;
    f.start(&x);
    let back = f.choose(&x, "auto", "alice").await;
    assert_eq!(back["status"], "ok", "{back}");
    assert_eq!(back["streamVersions"]["playing"]["videoId"], "rel");
    assert_eq!(back["streamVersions"]["playing"]["picked"], "octo");
    assert!(matches!(f.pick(&x), PickState::Octo(choice) if choice.video == "rel" && choice.auto));

    assert_eq!(f.choose(&x, "live", "alice").await["status"], "ok");
    f.shim.reset().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(503).set_body_string(r#"{"error":"busy"}"#))
        .mount(&f.shim)
        .await;
    assert_refused(&f.choose(&x, "auto", "alice").await, 0, NO_OCTO_PICK);
    assert_eq!(f.picked_video(&x).as_deref(), Some("live"), "the pick stays");
}

// ---- getStreamVersionChanges --------------------------------------------------------------

#[tokio::test]
async fn the_change_feed_lists_picks_in_order_with_their_ids_and_a_cursor() {
    let f = Fixture::new(|_| {}).await;
    let teardrop = f.song("Mezzanine");
    let angel = f.song_with("Mezzanine", |r| r.title = Some("Angel".into()));
    let start = f.changes("").await;
    assert_eq!(start["status"], "ok", "{start}");
    let epoch = start["streamVersionChanges"]["epoch"]
        .as_str()
        .expect("an epoch")
        .to_string();
    let seq = start["streamVersionChanges"]["seq"].as_u64().expect("a seq");

    for id in [&teardrop, &angel] {
        f.versions(id, "alice").await;
        assert_eq!(f.choose(id, "live", "alice").await["status"], "ok");
    }
    let since = f.changes(&format!("epoch={epoch}&since={seq}")).await;
    let feed = &since["streamVersionChanges"];
    assert_eq!(feed["complete"], true);
    assert_eq!(feed["epoch"], epoch.as_str());
    let change = feed["change"].as_array().expect("changes");
    assert_eq!(change.len(), 2, "{feed}");
    let seqs: Vec<u64> = change.iter().map(|c| c["seq"].as_u64().expect("seq")).collect();
    assert!(seqs[0] < seqs[1] && seqs[0] > seq, "{seqs:?}");
    assert!(strings(&change[0]["ids"]).contains(&teardrop));
    assert!(strings(&change[1]["ids"]).contains(&angel));
    let next = feed["seq"].as_u64().expect("seq");
    assert_eq!(next, seqs[1]);

    let nothing = f.changes(&format!("epoch={epoch}&since={next}")).await;
    assert_eq!(nothing["streamVersionChanges"]["complete"], true);
    assert_eq!(nothing["streamVersionChanges"]["change"], serde_json::json!([]));

    let other = f.changes(&format!("epoch=another&since={next}")).await;
    assert_eq!(other["streamVersionChanges"]["complete"], false);

    let everything = f.changes("").await;
    assert_eq!(everything["streamVersionChanges"]["complete"], true);
    assert!(
        everything["streamVersionChanges"]["change"]
            .as_array()
            .expect("changes")
            .len()
            >= 2
    );

    let refused = envelope(
        get_stream_version_changes(
            State(f.state.clone()),
            request("getStreamVersionChanges", "", "nobody", None),
        )
        .await,
    )
    .await;
    assert_eq!(refused["error"]["code"], 40);
}

// ---- getOpenSubsonicExtensions --------------------------------------------------------------

/// The extension names a getOpenSubsonicExtensions answer lists, in JSON or XML.
async fn listed(f: &Fixture, json: bool, caller: Option<FamilyCaller>) -> Vec<String> {
    let format = if json { "&f=json" } else { "" };
    let mut request = Request::builder()
        .uri(format!(
            "/rest/getOpenSubsonicExtensions.view?u=alice&t=good&s=salt&v=1.16.1&c=Octo{format}"
        ))
        .body(Body::empty())
        .expect("a request");
    if let Some(caller) = caller {
        request.extensions_mut().insert(caller);
    }
    let text = body_of(get_open_subsonic_extensions(State(f.state.clone()), request).await).await;
    if json {
        let value: Value = serde_json::from_str(&text).expect("JSON");
        return value["subsonic-response"]["openSubsonicExtensions"]
            .as_array()
            .expect("a list")
            .iter()
            .filter_map(|e| e["name"].as_str().map(str::to_string))
            .collect();
    }
    XElement::parse(&text)
        .expect("XML")
        .elements()
        .filter(|e| e.name == "openSubsonicExtensions")
        .filter_map(|e| e.attribute("name").map(str::to_string))
        .collect()
}

#[tokio::test]
async fn choosing_is_offered_to_callers_who_may_and_the_changes_to_everyone() {
    let f = Fixture::new(|_| {}).await;
    let callers: [(Option<FamilyCaller>, bool); 5] = [
        (None, true),
        (Some(FamilyCaller::unmanaged("winters", true)), true),
        (Some(member("ede", Preset::Member)), true),
        (Some(member("sam", Preset::Listener)), false),
        (Some(member("kid", Preset::Kid)), false),
    ];
    for (caller, may_pick) in callers {
        for json in [true, false] {
            let who = caller.as_ref().map(|c| c.user.clone());
            let names = listed(&f, json, caller.clone()).await;
            assert_eq!(
                names.iter().any(|n| n == "octoStreamVersions"),
                may_pick,
                "{who:?} json={json}: {names:?}"
            );
            assert!(
                names.iter().any(|n| n == "octoStreamVersionChanges"),
                "{who:?} json={json}: {names:?}"
            );
        }
    }
}
