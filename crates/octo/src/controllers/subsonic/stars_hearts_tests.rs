//! Hearts sent several at once, outside artists, and the outside songs a caller hearted that
//! are still downloading, which their starred lists show until the file lands.

use octo_core::settings::{AppSettings, SoulseekSettings};
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use serde_json::Value;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

use super::ARTIST_NOT_IN_LIBRARY;
use crate::app::AppState;
use crate::controllers::subsonic::test_app::*;
use crate::http::pipeline::App;

/// Navidrome's getStarred2 answer in JSON: one library song.
const STARRED2_JSON: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","starred2":{"song":[{"id":"nd-7","title":"Library song","starred":"2026-01-01T00:00:00Z"}]}}}"#;

/// The same in XML.
const STARRED2_XML: &str = r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1" type="navidrome"><starred2><song id="nd-7" title="Library song" starred="2026-01-01T00:00:00Z"></song></starred2></subsonic-response>"#;

/// getStarred, the older list, in JSON with nothing starred.
const STARRED_EMPTY_JSON: &str =
    r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","starred":{}}}"#;

/// A library id Navidrome does not have: a star naming it is refused.
const MISSING: &str = "nd-missing";

/// Navidrome for these tests: it accepts the token "good", the password "secret" and the API key
/// "bob-key" (whose owner is bob), and answers the starred lists, stars and unstars. A star of
/// [`MISSING`] is refused with error 70.
struct HeartNavidrome;

impl Respond for HeartNavidrome {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let path = request.url.path();
        let query = query_of(request);
        let value = |key: &str| query.get(key).map(String::as_str);
        let signed_in =
            value("t") == Some("good") || value("p") == Some("secret") || value("apiKey") == Some("bob-key");
        if !signed_in {
            return navidrome_json(NAVIDROME_WRONG_PASSWORD);
        }
        if path.ends_with("/rest/tokenInfo") {
            return navidrome_json(
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1","tokenInfo":{"username":"bob"}}}"#,
            );
        }
        if path.ends_with("/rest/getStarred2") || path.ends_with("/rest/getStarred2.view") {
            return if value("f") == Some("json") {
                navidrome_json(STARRED2_JSON)
            } else {
                ResponseTemplate::new(200).set_body_raw(STARRED2_XML.as_bytes().to_vec(), "application/xml")
            };
        }
        if path.ends_with("/rest/getStarred") || path.ends_with("/rest/getStarred.view") {
            return navidrome_json(STARRED_EMPTY_JSON);
        }
        if (path.ends_with("/rest/star") || path.ends_with("/rest/star.view"))
            && request.url.query_pairs().any(|(k, v)| k == "id" && v == MISSING)
        {
            return navidrome_json(
                r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":70,"message":"data not found"}}}"#,
            );
        }
        navidrome_json(NAVIDROME_OK)
    }
}

struct HeartApp {
    state: AppState,
    app: App,
    navidrome: MockServer,
    _directory: tempfile::TempDir,
}

impl HeartApp {
    async fn new() -> HeartApp {
        let navidrome = MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(HeartNavidrome)
            .mount(&navidrome)
            .await;
        let directory = tempfile::tempdir().expect("a temp dir");
        let mut settings = AppSettings {
            soulseek: SoulseekSettings {
                base_url: Some("http://127.0.0.1:1".into()),
                ..Default::default()
            },
            ..settings(&navidrome.uri())
        };
        // A waiting heart is listed only when it becomes a favorite on landing.
        settings.subsonic.star_downloads_for_requester = true;
        let state = state_with(settings, |_| {});
        state
            .settings
            .set_raw("Library:DownloadPath", Some(&directory.path().to_string_lossy()));
        HeartApp {
            app: app(&state),
            state,
            navidrome,
            _directory: directory,
        }
    }

    fn register(&self, kind: RoutingKind, title: &str) -> String {
        self.state.external_id_registry.register(SoulseekRouting {
            kind,
            artist: Some("Massive Attack".into()),
            title: Some(title.into()),
            album: Some("Mezzanine".into()),
            duration: Some(330),
            ..Default::default()
        })
    }

    /// Every id value of every request Navidrome received at `endpoint`, one list per request.
    async fn ids_sent_to(&self, endpoint: &str) -> Vec<Vec<String>> {
        self.navidrome
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| {
                let path = r.url.path();
                path.ends_with(&format!("/rest/{endpoint}"))
                    || path.ends_with(&format!("/rest/{endpoint}.view"))
            })
            .map(|r| {
                r.url
                    .query_pairs()
                    .filter(|(k, _)| matches!(k.as_ref(), "id" | "albumId" | "artistId"))
                    .map(|(_, v)| v.into_owned())
                    .collect()
            })
            .collect()
    }

    async fn starred2_song_ids(&self, sign_in: &str) -> Vec<String> {
        let reply = get(&self.app, &format!("/rest/getStarred2.view?{sign_in}&f=json")).await;
        reply.envelope()["starred2"]["song"]
            .as_array()
            .expect("a song list")
            .iter()
            .map(|song| song["id"].as_str().expect("an id").to_string())
            .collect()
    }
}

fn alice() -> String {
    auth("alice", "good", "Symfonium")
}

/// Library ids sent together reach Navidrome together, outside songs among them are each queued
/// on their own, and a comma list counts as separate ids.
#[tokio::test]
async fn a_batch_star_queues_its_outside_songs_and_relays_the_library_ones_together() {
    let t = HeartApp::new().await;
    let teardrop = t.register(RoutingKind::Song, "Teardrop");
    let angel = t.register(RoutingKind::Song, "Angel");

    let reply = get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id=nd-1&id={teardrop}&id=nd-2", alice()),
    )
    .await;
    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    let comma = get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id=nd-3,{angel}", alice()),
    )
    .await;
    assert_eq!(comma.envelope()["status"], "ok", "{}", comma.text());

    assert_eq!(t.ids_sent_to("star").await, [vec!["nd-1", "nd-2"], vec!["nd-3"]]);
    let downloads: Vec<String> = t
        .state
        .acquisition_tracker
        .for_user("alice")
        .iter()
        .map(|row| row.id.clone())
        .collect();
    assert_eq!(downloads.len(), 2, "{downloads:?}");
    assert!(downloads.contains(&teardrop) && downloads.contains(&angel));
    assert_eq!(t.state.star_on_arrival.held(), 2);
}

/// A batch of library ids alone goes to Navidrome exactly as it was sent.
#[tokio::test]
async fn a_batch_of_library_ids_is_relayed_as_sent() {
    let t = HeartApp::new().await;

    let reply = get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id=nd-1&id=nd-2&albumId=al-1", alice()),
    )
    .await;

    assert_eq!(reply.envelope()["status"], "ok");
    assert_eq!(t.ids_sent_to("star").await, [vec!["nd-1", "nd-2", "al-1"]]);
    assert!(t.state.track_acquisition_queue.is_idle());
}

/// An outside artist cannot be a favorite until it is in the library: error 70, and nothing is
/// downloaded or sent on, whether it comes as id or artistId.
#[tokio::test]
async fn an_outside_artist_is_refused_with_error_70_and_downloads_nothing() {
    let t = HeartApp::new().await;
    let artist = t.register(RoutingKind::Artist, "Massive Attack");

    for query in [
        format!("id={artist}"),
        format!("artistId={artist}"),
        "artistId=ext-deezer-artist-259".to_string(),
        format!("id=nd-1&id={artist}"),
    ] {
        let reply = get(&t.app, &format!("/rest/star.view?{}&f=json&{query}", alice())).await;
        let envelope = reply.envelope();
        assert_eq!(envelope["status"], "failed", "{query}");
        assert_eq!(envelope["error"]["code"], 70, "{query}");
        assert_eq!(envelope["error"]["message"], ARTIST_NOT_IN_LIBRARY, "{query}");
    }

    assert!(t.state.acquisition_tracker.all().is_empty());
    assert!(t.state.track_acquisition_queue.is_idle());
    assert_eq!(t.state.star_on_arrival.held(), 0);
    assert!(t.ids_sent_to("star").await.is_empty());
}

/// A wrong sign-in is refused before an outside artist is even looked at.
#[tokio::test]
async fn an_outside_artist_from_a_wrong_sign_in_is_refused_as_a_wrong_sign_in() {
    let t = HeartApp::new().await;
    let artist = t.register(RoutingKind::Artist, "Massive Attack");

    let reply = get(
        &t.app,
        &format!(
            "/rest/star.view?{}&f=json&id={artist}",
            auth("alice", "bad", "Symfonium")
        ),
    )
    .await;

    assert_eq!(reply.envelope()["error"]["code"], 40);
}

/// With nothing waiting, the starred lists are Navidrome's answers as they came.
#[tokio::test]
async fn with_no_waiting_hearts_the_starred_lists_are_navidromes() {
    let t = HeartApp::new().await;

    let json = get(&t.app, &format!("/rest/getStarred2.view?{}&f=json", alice())).await;
    assert_eq!(json.text(), STARRED2_JSON);
    let xml = get(&t.app, &format!("/rest/getStarred2.view?{}", alice())).await;
    assert_eq!(xml.text(), STARRED2_XML);
    let older = get(&t.app, &format!("/rest/getStarred.view?{}&f=json", alice())).await;
    assert_eq!(older.text(), STARRED_EMPTY_JSON);
}

/// A hearted outside song still downloading shows in the hearter's starred lists, JSON and XML,
/// as an outside song with the time it was hearted; nobody else's lists change.
#[tokio::test]
async fn a_waiting_heart_shows_in_the_hearters_starred_lists_only() {
    let t = HeartApp::new().await;
    let teardrop = t.register(RoutingKind::Song, "Teardrop");
    let star = get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id={teardrop}", alice()),
    )
    .await;
    assert_eq!(star.envelope()["status"], "ok");

    assert_eq!(t.starred2_song_ids(&alice()).await, ["nd-7", teardrop.as_str()]);
    let reply = get(&t.app, &format!("/rest/getStarred2.view?{}&f=json", alice())).await;
    let songs = reply.envelope()["starred2"]["song"].clone();
    let song = &songs[1];
    assert_eq!(song["title"], "Teardrop");
    assert_eq!(song["artist"], "Massive Attack");
    assert!(
        song["starred"].as_str().is_some_and(|s| s.ends_with('Z')),
        "{song}"
    );

    let xml = get(&t.app, &format!("/rest/getStarred2.view?{}", alice())).await;
    let text = xml.text();
    assert!(text.contains(&format!(r#"id="{teardrop}""#)), "{text}");
    assert!(text.contains(r#"title="Teardrop""#), "{text}");

    let older = get(&t.app, &format!("/rest/getStarred.view?{}&f=json", alice())).await;
    let older: Value = older.envelope()["starred"]["song"].clone();
    assert_eq!(older[0]["id"], teardrop.as_str());

    // Someone else's lists are Navidrome's as they came.
    let carol = get(
        &t.app,
        &format!(
            "/rest/getStarred2.view?{}&f=json",
            auth("carol", "good", "Symfonium")
        ),
    )
    .await;
    assert_eq!(carol.text(), STARRED2_JSON);
}

/// An API key sign-in's waiting hearts are filed under the key's owner and shown to that key.
#[tokio::test]
async fn an_api_keys_waiting_hearts_show_for_that_key() {
    let t = HeartApp::new().await;
    let teardrop = t.register(RoutingKind::Song, "Teardrop");
    let bob = "apiKey=bob-key&v=1.16.1&c=Symfonium";
    let star = get(&t.app, &format!("/rest/star.view?{bob}&f=json&id={teardrop}")).await;
    assert_eq!(star.envelope()["status"], "ok", "{}", star.text());

    assert_eq!(t.starred2_song_ids(bob).await, ["nd-7", teardrop.as_str()]);
    assert_eq!(t.starred2_song_ids(&alice()).await, ["nd-7"]);
}

/// Taking a heart back before the song lands takes it off the starred lists; a library id sent
/// with it still reaches Navidrome.
#[tokio::test]
async fn an_unstar_before_the_song_lands_takes_it_off_the_lists() {
    let t = HeartApp::new().await;
    let teardrop = t.register(RoutingKind::Song, "Teardrop");
    get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id={teardrop}", alice()),
    )
    .await;
    assert_eq!(t.state.star_on_arrival.held(), 1);

    let reply = get(
        &t.app,
        &format!("/rest/unstar.view?{}&f=json&id=nd-1&id={teardrop}", alice()),
    )
    .await;

    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    assert_eq!(t.state.star_on_arrival.held(), 0);
    assert_eq!(t.ids_sent_to("unstar").await, [vec!["nd-1"]]);
    assert_eq!(t.starred2_song_ids(&alice()).await, ["nd-7"]);
}

/// A held heart two people of Octo's own apps hearted: one taking theirs back leaves the other's
/// hold, and the last one taking theirs back ends it.
#[tokio::test]
async fn an_unstar_by_one_of_two_octo_app_users_keeps_the_others_hold() {
    use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, WaitingFor};

    use crate::services::common::{HeldAcquisition, Holder, StarOnArrival};

    let t = HeartApp::new().await;
    let teardrop = t.register(RoutingKind::Song, "Teardrop");
    let (_, provider, external_id) = t.state.local_library.parse_song_id(&teardrop);
    let holder = |user: &str| Holder {
        user: user.into(),
        ..Default::default()
    };
    let record = t.state.soulseek_holds.hold(HeldAcquisition {
        provider: provider.expect("an outside id"),
        external_id: external_id.expect("an outside id"),
        requested_by: Some("alice".into()),
        origin: Some(AcquisitionOrigin::Heart),
        waiting_for: Some(WaitingFor::MusicBrainz),
        holders: vec![holder("alice"), holder("carol")],
        ..Default::default()
    });
    let octo_app = StarOnArrival::OCTO_APP_CLIENT;

    let reply = get(
        &t.app,
        &format!(
            "/rest/unstar.view?{}&f=json&id={teardrop}",
            auth("carol", "good", octo_app)
        ),
    )
    .await;
    assert_eq!(reply.envelope()["status"], "ok", "{}", reply.text());
    let kept = t
        .state
        .soulseek_holds
        .get(&record.key())
        .expect("alice still holds it");
    let users: Vec<&str> = kept.holders.iter().map(|h| h.user.as_str()).collect();
    assert_eq!(users, ["alice"]);

    // A wrong sign-in takes nobody off.
    get(
        &t.app,
        &format!(
            "/rest/unstar.view?{}&f=json&id={teardrop}",
            auth("alice", "bad", octo_app)
        ),
    )
    .await;
    assert!(t.state.soulseek_holds.get(&record.key()).is_some());

    get(
        &t.app,
        &format!(
            "/rest/unstar.view?{}&f=json&id={teardrop}",
            auth("alice", "good", octo_app)
        ),
    )
    .await;
    assert!(t.state.soulseek_holds.snapshot().is_empty(), "nobody is left");
}

/// Someone else cannot take back a heart by naming its owner without their sign-in.
#[tokio::test]
async fn an_unstar_with_a_wrong_sign_in_keeps_the_heart() {
    let t = HeartApp::new().await;
    let teardrop = t.register(RoutingKind::Song, "Teardrop");
    get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id={teardrop}", alice()),
    )
    .await;

    let reply = get(
        &t.app,
        &format!(
            "/rest/unstar.view?{}&f=json&id={teardrop}",
            auth("alice", "bad", "Symfonium")
        ),
    )
    .await;

    assert_eq!(reply.envelope()["error"]["code"], 40);
    assert_eq!(t.state.star_on_arrival.held(), 1);
}

/// Another listener, signed in properly, cannot take back someone else's waiting heart: theirs
/// is the only one an unstar can drop.
#[tokio::test]
async fn another_signed_in_listener_cannot_unstar_someone_elses_heart() {
    let t = HeartApp::new().await;
    let teardrop = t.register(RoutingKind::Song, "Teardrop");
    let star = get(
        &t.app,
        &format!("/rest/star.view?{}&f=json&id={teardrop}", alice()),
    )
    .await;
    assert_eq!(star.envelope()["status"], "ok", "{}", star.text());

    for other in [
        auth("carol", "good", "Symfonium"),
        "apiKey=bob-key&v=1.16.1&c=Symfonium".to_string(),
    ] {
        let reply = get(&t.app, &format!("/rest/unstar.view?{other}&f=json&id={teardrop}")).await;
        assert_eq!(reply.envelope()["status"], "ok", "{other}: {}", reply.text());
        assert_eq!(t.state.star_on_arrival.held(), 1, "{other}");
    }
    assert_eq!(t.starred2_song_ids(&alice()).await, ["nd-7", teardrop.as_str()]);
}

/// A star Navidrome refuses queues none of the outside songs or albums it carried and holds no
/// heart for them: the error is the whole answer.
#[tokio::test]
async fn a_star_navidrome_refuses_queues_nothing_from_outside() {
    let t = HeartApp::new().await;
    let teardrop = t.register(RoutingKind::Song, "Teardrop");
    let mezzanine = t.register(RoutingKind::Album, "Mezzanine");

    let reply = get(
        &t.app,
        &format!(
            "/rest/star.view?{}&f=json&id={MISSING}&id={teardrop}&albumId={mezzanine}",
            alice()
        ),
    )
    .await;

    let envelope = reply.envelope();
    assert_eq!(envelope["status"], "failed", "{}", reply.text());
    assert_eq!(envelope["error"]["code"], 70);
    assert_eq!(t.ids_sent_to("star").await, [vec![MISSING]]);
    assert!(t.state.acquisition_tracker.all().is_empty());
    assert!(t.state.track_acquisition_queue.is_idle());
    assert_eq!(t.state.star_on_arrival.held(), 0);
    assert_eq!(t.starred2_song_ids(&alice()).await, ["nd-7"]);
}
