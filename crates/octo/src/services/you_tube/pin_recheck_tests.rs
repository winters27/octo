use octo_core::soulseek::{LengthSource, RoutingKind, StreamFacts};
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

fn request(token: Option<&str>, method: &str, purpose: Option<&str>) -> StreamPlay {
    StreamPlay {
        key: "x_phone".into(),
        token: token.map(str::to_string),
        address: Some("10.0.0.2".into()),
        method: method.into(),
        purpose: purpose.map(str::to_string),
        octo_device: false,
        user_agent: "Symfonium/9".into(),
    }
}

fn bound(token: Option<&str>) -> PlayBinding {
    PlayBinding {
        key: "x_phone".into(),
        token: token.map(str::to_string),
        video: "old".into(),
        ..Default::default()
    }
}

#[test]
fn a_token_goes_on_with_its_own_binding_and_a_new_one_starts_a_play() {
    let same = request(Some("e1.0"), "GET", None);
    assert_eq!(
        play_start(&same, Some("bytes=500-"), Some(&bound(Some("e1.0")))),
        PlayStart::Continue
    );
    assert_eq!(
        play_start(&same, None, Some(&bound(Some("e1.0")))),
        PlayStart::Continue
    );
    let restarted = request(Some("e1.1"), "GET", None);
    assert_eq!(
        play_start(&restarted, Some("bytes=0-"), Some(&bound(Some("e1.0")))),
        PlayStart::Start { switch: true }
    );
    assert_eq!(
        play_start(&restarted, Some("bytes=0-0"), None),
        PlayStart::Start { switch: true }
    );
    // A new token picking up past the first byte binds what the song has, without a switch.
    assert_eq!(
        play_start(&restarted, Some("bytes=4096-"), None),
        PlayStart::Start { switch: false }
    );
}

#[test]
fn without_a_token_only_an_open_read_or_avplayers_probe_restarts_a_bound_play() {
    let app = request(None, "GET", None);
    assert_eq!(
        play_start(&app, Some("bytes=0-1"), None),
        PlayStart::Start { switch: true }
    );
    assert_eq!(
        play_start(&app, Some("bytes=900-"), None),
        PlayStart::Start { switch: false }
    );
    let b = bound(None);
    assert_eq!(
        play_start(&app, None, Some(&b)),
        PlayStart::Start { switch: true }
    );
    assert_eq!(
        play_start(&app, Some(""), Some(&b)),
        PlayStart::Start { switch: true }
    );
    assert_eq!(
        play_start(&app, Some("bytes=0-"), Some(&b)),
        PlayStart::Start { switch: true }
    );
    assert_eq!(play_start(&app, Some("bytes=0-0"), Some(&b)), PlayStart::Continue);
    assert_eq!(play_start(&app, Some("bytes=0-1"), Some(&b)), PlayStart::Continue);
    assert_eq!(
        play_start(&app, Some("bytes=0-65535"), Some(&b)),
        PlayStart::Continue
    );
    assert_eq!(
        play_start(&app, Some("bytes=900-"), Some(&b)),
        PlayStart::Continue
    );
    let avplayer = StreamPlay {
        user_agent: "AppleCoreMedia/1.0.0.21E236 (iPhone; U; CPU OS 17_4 like Mac OS X)".into(),
        ..request(None, "GET", None)
    };
    assert_eq!(
        play_start(&avplayer, Some("bytes=0-1"), Some(&b)),
        PlayStart::Start { switch: true }
    );
    assert_eq!(
        play_start(&avplayer, Some("bytes=0-4000000"), Some(&b)),
        PlayStart::Continue
    );
}

#[test]
fn an_older_octo_app_bound_to_a_video_is_never_switched() {
    let older = StreamPlay {
        octo_device: true,
        ..request(None, "GET", None)
    };
    let b = bound(None);
    for range in [None, Some("bytes=0-"), Some("bytes=0-0"), Some("bytes=300-")] {
        assert_eq!(
            play_start(&older, range, Some(&b)),
            PlayStart::Continue,
            "{range:?}"
        );
    }
    assert_eq!(play_start(&older, None, None), PlayStart::Start { switch: true });
}

#[test]
fn an_older_octo_apps_play_that_is_over_starts_again_and_its_offline_reads_take_the_plain_rule() {
    let older = StreamPlay {
        octo_device: true,
        ..request(None, "GET", None)
    };
    let over = PlayBinding {
        finished: true,
        ..bound(None)
    };
    assert_eq!(
        play_start(&older, Some("bytes=0-"), Some(&over)),
        PlayStart::Start { switch: true }
    );
    assert_eq!(
        play_start(&older, None, Some(&over)),
        PlayStart::Start { switch: true }
    );
    // Its probes and later ranges still go on with the play.
    assert_eq!(
        play_start(&older, Some("bytes=0-0"), Some(&over)),
        PlayStart::Continue
    );
    assert_eq!(
        play_start(&older, Some("bytes=300-"), Some(&over)),
        PlayStart::Continue
    );
    // An offline read (save ahead, the prefetcher) takes the rule other apps take.
    let offline = StreamPlay {
        purpose: Some("offline".into()),
        ..older.clone()
    };
    assert_eq!(
        play_start(&offline, Some("bytes=0-"), Some(&bound(None))),
        PlayStart::Start { switch: true }
    );
    assert_eq!(
        play_start(&offline, Some("bytes=0-0"), Some(&bound(None))),
        PlayStart::Continue
    );
    assert_eq!(
        play_start(&offline, Some("bytes=900-"), Some(&bound(None))),
        PlayStart::Continue
    );
}

#[test]
fn a_head_or_a_scout_read_only_looks() {
    for play in [
        request(None, "HEAD", None),
        request(Some("e1.0"), "GET", Some("scout")),
        request(None, "GET", Some("scout")),
    ] {
        assert_eq!(play_start(&play, None, None), PlayStart::Peek);
        assert_eq!(
            play_start(&play, Some("bytes=0-"), Some(&bound(None))),
            PlayStart::Peek
        );
    }
    // An offline copy is a play like any other (its own key tells it apart).
    assert_eq!(
        play_start(&request(None, "GET", Some("offline")), None, None),
        PlayStart::Start { switch: true }
    );
}

fn song(artist: &str, title: &str) -> SoulseekRouting {
    SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some(artist.into()),
        title: Some(title.into()),
        ..Default::default()
    }
}

fn facts() -> StreamFacts {
    StreamFacts {
        size: Some(6_755_585),
        itag: Some("140".into()),
        ext: Some("m4a".into()),
        bit_rate: Some(129),
        ..Default::default()
    }
}

/// A song pinned to `video` by an older ranking, with that video's stream facts.
fn stale(registry: &ExternalIdRegistry, artist: &str, title: &str, video: &str) -> String {
    let id = registry.register(song(artist, title));
    registry.set_you_tube_id(&id, video);
    registry.remember_stream(&id, video, &facts());
    id
}

fn music_row(video: &str, artist: &str, title: &str) -> String {
    format!(
        r#"{{"tracks":[{{"videoId":"{video}","title":"{title}","artists":["{artist}"],"durationSeconds":200,"videoType":"MUSIC_VIDEO_TYPE_ATV"}}]}}"#
    )
}

fn ranked(video: &str) -> String {
    format!(r#"{{"video_id":"{video}","title":"T","duration":200,"rank":3}}"#)
}

async fn music_answers(shim: &MockServer, body: &str) {
    Mock::given(path("/ytm/search"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(shim)
        .await;
}

async fn search_answers(shim: &MockServer, status: u16, body: &str) {
    Mock::given(path("/search"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(shim)
        .await;
}

fn worker(registry: &Arc<ExternalIdRegistry>, shim: &MockServer) -> PinRecheck {
    PinRecheck::new(
        registry.clone(),
        Arc::new(YouTubeResolver::with_base_url(Some(&shim.uri()))),
    )
    .with_pause(Duration::ZERO)
}

async fn asked(shim: &MockServer) -> Vec<(String, String)> {
    shim.received_requests()
        .await
        .expect("recorded")
        .iter()
        .map(|r| (r.url.path().to_string(), r.url.query().unwrap_or("").to_string()))
        .collect()
}

/// What a play of the song does with a waiting video: only a request from the first byte takes
/// it, and the old video's copy is the one to drop.
fn play(registry: &ExternalIdRegistry, id: &str, range: Option<&str>) -> Option<String> {
    let routing = registry.lookup(id).expect("known");
    if starts_at_first_byte(range) {
        registry.apply_pending_video(&routing)
    } else {
        None
    }
}

#[test]
fn only_a_request_from_the_first_byte_to_the_end_starts_at_the_first_byte() {
    assert!(starts_at_first_byte(None));
    assert!(starts_at_first_byte(Some("")));
    assert!(starts_at_first_byte(Some("bytes=0-")));
    assert!(!starts_at_first_byte(Some("bytes=0-1")));
    assert!(!starts_at_first_byte(Some("bytes=1000-")));
    assert!(!starts_at_first_byte(Some("bytes=-500")));
}

#[tokio::test]
async fn a_stale_pin_the_new_pick_agrees_with_is_stamped_and_keeps_its_stream_facts() {
    let shim = MockServer::start().await;
    music_answers(&shim, &music_row("vid-a", "Daft Punk", "Emotion")).await;
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let id = stale(&registry, "Daft Punk", "Emotion", "vid-a");
    assert_eq!(registry.stale_pins(), std::slice::from_ref(&id));

    let report = worker(&registry, &shim).pass(&CancellationToken::new()).await;

    assert_eq!(
        (report.checked, report.kept, report.better, report.failed),
        (1, 1, 0, 0)
    );
    let routing = registry.lookup(&id).expect("known").snapshot();
    assert_eq!(routing.video_pick, SoulseekRouting::VIDEO_PICK);
    assert_eq!(routing.pending_video, None);
    assert_eq!(routing.stream_facts(), facts());
    assert!(registry.stale_pins().is_empty());
}

#[tokio::test]
async fn a_different_pick_waits_for_a_play_from_the_first_byte() {
    let shim = MockServer::start().await;
    music_answers(&shim, &music_row("vid-b", "Daft Punk", "Emotion")).await;
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let id = stale(&registry, "Daft Punk", "Emotion", "vid-a");
    registry.remember_length(&id, Some(250), LengthSource::Video);

    let report = worker(&registry, &shim).pass(&CancellationToken::new()).await;
    assert_eq!((report.checked, report.kept, report.better), (1, 0, 1));
    let waiting = registry.lookup(&id).expect("known").snapshot();
    assert_eq!(waiting.pending_video.as_deref(), Some("vid-b"));
    assert_eq!(waiting.you_tube_id.as_deref(), Some("vid-a"));

    // A play that picks up past the first byte still opens the old video.
    assert_eq!(play(&registry, &id, Some("bytes=1000-")), None);
    let mid = registry.lookup(&id).expect("known").snapshot();
    assert_eq!(mid.you_tube_id.as_deref(), Some("vid-a"));
    assert_eq!(mid.pending_video.as_deref(), Some("vid-b"));
    assert_eq!(mid.stream_facts(), facts());

    // A play from byte 0 opens the new one, with the old video's facts and length gone.
    assert_eq!(play(&registry, &id, Some("bytes=0-")).as_deref(), Some("vid-a"));
    let now = registry.lookup(&id).expect("known").snapshot();
    assert_eq!(now.you_tube_id.as_deref(), Some("vid-b"));
    assert_eq!(now.pending_video, None);
    assert_eq!(now.video_pick, SoulseekRouting::VIDEO_PICK);
    assert!(now.stream_facts().is_empty());
    assert_eq!(now.shown_duration_source, LengthSource::None);
    assert_eq!(now.shown_duration, None);
}

#[tokio::test]
async fn the_old_video_is_not_left_to_drop_while_another_song_pins_it() {
    let registry = ExternalIdRegistry::in_memory();
    let id = stale(&registry, "Daft Punk", "Emotion", "vid-a");
    let _other = stale(&registry, "Daft Punk", "Emotion (Radio Edit)", "vid-a");
    assert!(registry.set_pending_video(&id, "vid-a", "vid-b"));

    assert_eq!(play(&registry, &id, None), None);
    let now = registry.lookup(&id).expect("known").snapshot();
    assert_eq!(now.you_tube_id.as_deref(), Some("vid-b"));
}

#[tokio::test]
async fn named_videos_are_never_looked_at() {
    let shim = MockServer::start().await;
    music_answers(&shim, &music_row("vid-b", "Daft Punk", "Emotion")).await;
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let id = registry.register(SoulseekRouting {
        you_tube_id: Some("vid-a".into()),
        ..song("Daft Punk", "Emotion")
    });
    assert_eq!(
        registry.lookup(&id).expect("known").snapshot().video_pick,
        SoulseekRouting::VIDEO_NAMED
    );
    assert!(registry.stale_pins().is_empty());

    let report = worker(&registry, &shim).pass(&CancellationToken::new()).await;

    assert_eq!(report, PassReport::default());
    assert!(asked(&shim).await.is_empty());
}

#[tokio::test]
async fn a_busy_youtube_music_leaves_the_pin_unstamped_and_ends_the_pass() {
    let shim = MockServer::start().await;
    Mock::given(path("/ytm/search"))
        .respond_with(ResponseTemplate::new(503).set_body_string(r#"{"error":"busy"}"#))
        .mount(&shim)
        .await;
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let first = stale(&registry, "Daft Punk", "Emotion", "vid-a");
    let second = stale(&registry, "Adele", "Hello", "vid-c");

    let report = worker(&registry, &shim).pass(&CancellationToken::new()).await;

    assert_eq!(report.failed, 1);
    assert_eq!(asked(&shim).await.len(), 1, "the pass ended at the first");
    for id in [first, second] {
        assert_eq!(registry.lookup(&id).expect("known").snapshot().video_pick, 0);
    }
    assert_eq!(registry.stale_pins().len(), 2);
}

#[tokio::test]
async fn a_youtube_music_that_times_out_never_falls_back_to_the_search() {
    let shim = MockServer::start().await;
    Mock::given(path("/ytm/search"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(music_row("vid-b", "Daft Punk", "Emotion"))
                .set_delay(YouTubeResolver::MUSIC_TIMEOUT + Duration::from_millis(500)),
        )
        .mount(&shim)
        .await;
    search_answers(&shim, 200, &ranked("vid-b")).await;
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let id = stale(&registry, "Daft Punk", "Emotion", "vid-a");

    let report = worker(&registry, &shim).pass(&CancellationToken::new()).await;

    assert_eq!(report.failed, 1);
    assert!(asked(&shim).await.iter().all(|(p, _)| p == "/ytm/search"));
    let routing = registry.lookup(&id).expect("known").snapshot();
    assert_eq!((routing.video_pick, routing.pending_video), (0, None));
}

#[tokio::test]
async fn a_refused_video_is_never_the_pending_pick() {
    let shim = MockServer::start().await;
    music_answers(&shim, &music_row("vid-b", "Daft Punk", "Emotion")).await;
    search_answers(&shim, 404, r#"{"error":"no_hit"}"#).await;
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let id = stale(&registry, "Daft Punk", "Emotion", "vid-a");
    registry.refuse_video(&id, "vid-b");
    assert!(!registry.set_pending_video(&id, "vid-a", "vid-b"));

    let report = worker(&registry, &shim).pass(&CancellationToken::new()).await;

    assert_eq!((report.kept, report.better), (1, 0));
    let after = registry.lookup(&id).expect("known").snapshot();
    assert_eq!(after.pending_video, None);
    assert_eq!(after.you_tube_id.as_deref(), Some("vid-a"));
    // The search that decided was told to leave the refused video out.
    let searches: Vec<String> = asked(&shim)
        .await
        .into_iter()
        .filter(|(p, _)| p == "/search")
        .map(|(_, q)| q)
        .collect();
    assert_eq!(searches.len(), 1);
    assert!(searches[0].contains("skip=vid-b"), "{searches:?}");
}

#[tokio::test]
async fn nothing_better_anywhere_stamps_the_pin() {
    let shim = MockServer::start().await;
    music_answers(&shim, r#"{"tracks":[]}"#).await;
    search_answers(&shim, 404, r#"{"error":"no_hit"}"#).await;
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let id = stale(&registry, "Daft Punk", "Emotion", "vid-a");

    let report = worker(&registry, &shim).pass(&CancellationToken::new()).await;

    assert_eq!((report.kept, report.failed), (1, 0));
    let routing = registry.lookup(&id).expect("known").snapshot();
    assert_eq!(routing.video_pick, SoulseekRouting::VIDEO_PICK);
    assert_eq!(routing.you_tube_id.as_deref(), Some("vid-a"));
}

#[tokio::test]
async fn a_failed_ranked_search_leaves_that_pin_and_the_pass_goes_on() {
    let shim = MockServer::start().await;
    music_answers(&shim, r#"{"tracks":[]}"#).await;
    search_answers(&shim, 500, "{}").await;
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let first = stale(&registry, "Daft Punk", "Emotion", "vid-a");
    let second = stale(&registry, "Adele", "Hello", "vid-c");

    let report = worker(&registry, &shim).pass(&CancellationToken::new()).await;

    assert_eq!((report.checked, report.failed), (2, 2));
    for id in [first, second] {
        assert_eq!(registry.lookup(&id).expect("known").snapshot().video_pick, 0);
    }
}

#[tokio::test]
async fn a_busy_search_leaves_that_pin_unstamped_and_ends_the_pass() {
    let shim = MockServer::start().await;
    music_answers(&shim, r#"{"tracks":[]}"#).await;
    search_answers(&shim, 503, r#"{"error":"busy"}"#).await;
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let first = stale(&registry, "Daft Punk", "Emotion", "vid-a");
    let second = stale(&registry, "Adele", "Hello", "vid-c");

    let report = worker(&registry, &shim).pass(&CancellationToken::new()).await;

    assert_eq!((report.checked, report.failed), (1, 1));
    let requests = asked(&shim).await;
    assert_eq!(
        requests.iter().filter(|(p, _)| p == "/search").count(),
        1,
        "{requests:?}"
    );
    assert_eq!(requests.len(), 2, "the second pin was never looked at");
    for id in [first, second] {
        assert_eq!(registry.lookup(&id).expect("known").snapshot().video_pick, 0);
    }
}

#[tokio::test]
async fn an_unreachable_shim_ends_the_pass_after_the_first_pin() {
    // Nothing listens on port 1, so every connection is refused.
    let uri = "http://127.0.0.1:1".to_string();
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let first = stale(&registry, "Daft Punk", "Emotion", "vid-a");
    let second = stale(&registry, "Adele", "Hello", "vid-c");
    let worker = PinRecheck::new(
        registry.clone(),
        Arc::new(YouTubeResolver::with_base_url(Some(&uri))),
    )
    .with_pause(Duration::ZERO);

    let report = worker.pass(&CancellationToken::new()).await;

    assert_eq!((report.checked, report.failed), (1, 1));
    for id in [first, second] {
        assert_eq!(registry.lookup(&id).expect("known").snapshot().video_pick, 0);
    }
}

#[tokio::test]
async fn a_non_json_answer_from_youtube_music_leaves_that_pin_and_the_pass_goes_on() {
    let shim = MockServer::start().await;
    music_answers(&shim, "<html>oops</html>").await;
    search_answers(&shim, 200, &ranked("vid-a")).await;
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let first = stale(&registry, "Daft Punk", "Emotion", "vid-a");
    let second = stale(&registry, "Adele", "Hello", "vid-c");

    let report = worker(&registry, &shim).pass(&CancellationToken::new()).await;

    assert_eq!((report.checked, report.failed), (2, 2));
    let requests = asked(&shim).await;
    assert_eq!(requests.len(), 2, "both pins were asked, neither fell back");
    assert!(requests.iter().all(|(p, _)| p == "/ytm/search"));
    for id in [first, second] {
        assert_eq!(registry.lookup(&id).expect("known").snapshot().video_pick, 0);
    }
}

#[tokio::test]
async fn the_background_requests_say_so() {
    let shim = MockServer::start().await;
    music_answers(&shim, r#"{"tracks":[]}"#).await;
    search_answers(&shim, 200, &ranked("vid-a")).await;
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    stale(&registry, "Daft Punk", "Emotion", "vid-a");

    worker(&registry, &shim).pass(&CancellationToken::new()).await;

    let requests = asked(&shim).await;
    let music = requests
        .iter()
        .find(|(p, _)| p == "/ytm/search")
        .expect("music asked");
    assert!(music.1.split('&').any(|q| q == "bg=1"), "{music:?}");
    let search = requests
        .iter()
        .find(|(p, _)| p == "/search")
        .expect("search asked");
    assert!(search.1.split('&').any(|q| q == "bg=1"), "{search:?}");
    assert!(search.1.split('&').any(|q| q == "resolve=0"), "{search:?}");
}
