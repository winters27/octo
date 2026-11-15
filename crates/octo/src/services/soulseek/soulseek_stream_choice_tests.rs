//! The stream of an outside song with versions chosen: a play keeps the video it started on, a
//! new play takes the chosen one, a look takes it without binding, an older Octo app is never
//! switched, a dead choice gives way to Octo's own pick, and a download takes the chosen video.

use chrono::Utc;
use octo_core::fingerprint::heart_gate::HeardRecord;

use super::*;
use crate::services::i_download_service::StreamPlay;
use crate::services::soulseek::{PickState, StreamChoice, StreamChoices};

const OLD: &str = "oldvideo-11";
const NEW: &str = "newvideo-22";

fn device(key: &str, token: Option<&str>) -> StreamPlay {
    StreamPlay {
        key: key.into(),
        token: token.map(str::to_string),
        address: Some("10.0.0.2".into()),
        method: "GET".into(),
        ..Default::default()
    }
}

fn choose(walk: &Walk, id: &str, video: &str) {
    walk.registry
        .choose_stream_video(
            id,
            StreamChoice {
                video: video.into(),
                by: "ede".into(),
                at: Utc::now(),
                ..Default::default()
            },
        )
        .expect("stored");
}

async fn open(
    walk: &Walk,
    id: &str,
    range: Option<&str>,
    play: Option<&StreamPlay>,
) -> Option<DirectStreamInfo> {
    walk.service
        .get_direct_stream("soulseek", id, range, play, &CancellationToken::new())
        .await
        .expect("no error")
}

fn pin_of(walk: &Walk, id: &str) -> Option<String> {
    walk.registry.lookup(id).expect("known").snapshot().you_tube_id
}

#[tokio::test]
async fn a_listener_mid_song_keeps_its_video_while_another_starts_the_chosen_one() {
    let root = Root::new("octo-choice-");
    let walk = build(&root, false, |_| Arc::new(|_: &str| Vec::new())).await;
    let id = walk.songs[0].external_id.clone().expect("an id");
    walk.registry.set_you_tube_id(&id, OLD);
    let y = device("x_symfonium", None);
    open(&walk, &id, None, Some(&y)).await.expect("a stream");
    assert_eq!(
        walk.registry.bound_play(&id, &y).and_then(|b| b.size),
        Some(12_345),
        "the whole size the answer named"
    );

    choose(&walk, &id, NEW);
    let x = device("x_phone", Some("e1.0"));
    open(&walk, &id, Some("bytes=0-"), Some(&x))
        .await
        .expect("a stream");
    assert_eq!(pin_of(&walk, &id).as_deref(), Some(NEW));

    let later = open(&walk, &id, Some("bytes=500-"), Some(&y))
        .await
        .expect("a stream");
    assert_eq!(
        later.video_id.as_deref(),
        Some(OLD),
        "the running play keeps its video"
    );
    assert_eq!(streamed(&walk).await, [OLD, NEW, OLD]);
    assert_eq!(pin_of(&walk, &id).as_deref(), Some(NEW));
}

#[tokio::test]
async fn a_new_token_from_a_later_byte_takes_what_the_song_has_without_a_switch() {
    let root = Root::new("octo-choice-");
    let walk = build(&root, false, |_| Arc::new(|_: &str| Vec::new())).await;
    let id = walk.songs[0].external_id.clone().expect("an id");
    walk.registry.set_you_tube_id(&id, OLD);
    assert!(walk.registry.set_pending_video(&id, OLD, NEW));
    let resumed = device("x_phone", Some("e1.0"));
    let answer = open(&walk, &id, Some("bytes=4096-"), Some(&resumed))
        .await
        .expect("a stream");
    assert_eq!(answer.video_id.as_deref(), Some(OLD));
    let r = walk.registry.lookup(&id).expect("known").snapshot();
    assert_eq!(
        (r.you_tube_id.as_deref(), r.pending_video.as_deref()),
        (Some(OLD), Some(NEW))
    );
    assert_eq!(
        walk.registry
            .bound_play(&id, &resumed)
            .map(|b| b.video)
            .as_deref(),
        Some(OLD)
    );
}

#[tokio::test]
async fn a_restart_with_a_new_token_gets_the_choice_while_the_old_plays_body_streams() {
    let root = Root::new("octo-choice-");
    let walk = build(&root, false, |_| Arc::new(|_: &str| Vec::new())).await;
    let id = walk.songs[0].external_id.clone().expect("an id");
    walk.registry.set_you_tube_id(&id, OLD);
    let first = device("x_phone", Some("e1.0"));
    let streaming = open(&walk, &id, None, Some(&first)).await.expect("a stream");

    choose(&walk, &id, NEW);
    let restarted = device("x_phone", Some("e1.1"));
    let answer = open(&walk, &id, Some("bytes=0-"), Some(&restarted))
        .await
        .expect("a stream");
    assert_eq!(answer.video_id.as_deref(), Some(NEW));
    assert_eq!(
        walk.registry.bound_play(&id, &first).map(|b| b.video).as_deref(),
        Some(OLD),
        "two token plays of one device keep their own videos"
    );
    // A scout of the restarted play reads the restarted play's video.
    let scout = StreamPlay {
        purpose: Some("scout".into()),
        ..device("x_phone", Some("e1.1"))
    };
    let looked = open(&walk, &id, Some("bytes=0-0"), Some(&scout))
        .await
        .expect("a stream");
    assert_eq!(looked.video_id.as_deref(), Some(NEW));
    drop(streaming);
}

#[tokio::test]
async fn a_look_takes_the_choice_but_never_the_waiting_video_and_binds_nothing() {
    let root = Root::new("octo-choice-");
    let walk = build(&root, false, |_| Arc::new(|_: &str| Vec::new())).await;
    let chosen = walk.songs[0].external_id.clone().expect("an id");
    let waiting = walk.songs[1].external_id.clone().expect("an id");
    walk.registry.set_you_tube_id(&chosen, OLD);
    walk.registry.set_you_tube_id(&waiting, OLD);
    assert!(walk.registry.set_pending_video(&waiting, OLD, NEW));
    choose(&walk, &chosen, NEW);
    let head = StreamPlay {
        method: "HEAD".into(),
        ..device("x_phone", None)
    };
    let looked = open(&walk, &chosen, None, Some(&head)).await.expect("a stream");
    assert_eq!(looked.video_id.as_deref(), Some(NEW));
    assert!(
        walk.registry
            .lookup(&chosen)
            .expect("known")
            .snapshot()
            .plays
            .is_empty()
    );
    open(&walk, &waiting, None, Some(&head)).await.expect("a stream");
    let r = walk.registry.lookup(&waiting).expect("known").snapshot();
    assert_eq!(
        (r.you_tube_id.as_deref(), r.pending_video.as_deref()),
        (Some(OLD), Some(NEW))
    );
}

#[tokio::test]
async fn a_bound_octo_app_without_a_token_is_never_switched() {
    let root = Root::new("octo-choice-");
    let walk = build(&root, false, |_| Arc::new(|_: &str| Vec::new())).await;
    let id = walk.songs[0].external_id.clone().expect("an id");
    walk.registry.set_you_tube_id(&id, OLD);
    let older = StreamPlay {
        octo_device: true,
        ..device("x_octo_1_12", None)
    };
    open(&walk, &id, None, Some(&older)).await.expect("a stream");
    choose(&walk, &id, NEW);
    for range in [None, Some("bytes=0-"), Some("bytes=0-0")] {
        let answer = open(&walk, &id, range, Some(&older)).await.expect("a stream");
        assert_eq!(answer.video_id.as_deref(), Some(OLD), "{range:?}");
    }
    assert_eq!(
        pin_of(&walk, &id).as_deref(),
        Some(OLD),
        "nothing switched the song"
    );
    // Another device, unbound, gets the choice.
    let other = StreamPlay {
        octo_device: true,
        ..device("x_octo_other", None)
    };
    let answer = open(&walk, &id, None, Some(&other)).await.expect("a stream");
    assert_eq!(answer.video_id.as_deref(), Some(NEW));
}

#[tokio::test]
async fn an_unpinned_song_takes_the_choice_at_any_request() {
    let root = Root::new("octo-choice-");
    let walk = build(&root, false, |_| Arc::new(|_: &str| Vec::new())).await;
    let id = walk.songs[0].external_id.clone().expect("an id");
    choose(&walk, &id, NEW);
    let answer = open(&walk, &id, Some("bytes=500-"), None)
        .await
        .expect("a stream");
    assert_eq!(answer.video_id.as_deref(), Some(NEW));
    assert_eq!(streamed(&walk).await, [NEW], "never searched past the choice");
}

#[tokio::test]
async fn a_choice_gone_from_youtube_is_forgotten_and_octos_pick_plays_in_the_same_request() {
    let root = Root::new("octo-choice-");
    let walk = build(&root, false, |_| Arc::new(|_: &str| Vec::new())).await;
    let id = walk.songs[0].external_id.clone().expect("an id");
    walk.registry.set_you_tube_id(&id, OLD);
    choose(&walk, &id, "deadvideo-1");
    walk.slskd.state.lock().dead_videos.insert("deadvideo-1".into());
    let key = StreamChoices::key_of(&walk.registry.lookup(&id).expect("known").snapshot()).expect("a key");
    let before = walk.registry.choices().changes_since(None, Some(0), 500).seq;

    let answer = open(&walk, &id, None, Some(&device("x_phone", None)))
        .await
        .expect("a stream");
    assert_eq!(answer.video_id.as_deref(), Some("vid123"), "Octo's own pick");
    assert_eq!(walk.registry.choices().record_for_key(&key), None);
    let r = walk.registry.lookup(&id).expect("known").snapshot();
    assert_eq!(walk.registry.pick_for(&r), PickState::None);
    let feed = walk.registry.changes_since(None, Some(before), 500);
    assert!(
        feed.changes.iter().any(|(_, ids)| ids.contains(&id)),
        "a tombstone names the song"
    );

    // An earlier choice gone from YouTube goes the same way.
    let earlier = walk.songs[1].external_id.clone().expect("an id");
    walk.registry.set_you_tube_id(&earlier, "deadvideo-2");
    walk.registry.lookup(&earlier).expect("known").lock().video_pick = SoulseekRouting::VIDEO_CHOSEN;
    walk.slskd.state.lock().dead_videos.insert("deadvideo-2".into());
    let answer = open(&walk, &earlier, None, Some(&device("x_phone", None)))
        .await
        .expect("a stream");
    assert_eq!(answer.video_id.as_deref(), Some("vid123"));
}

#[tokio::test]
async fn a_choice_the_shim_is_too_busy_to_check_is_kept() {
    let root = Root::new("octo-choice-");
    let walk = build(&root, false, |_| Arc::new(|_: &str| Vec::new())).await;
    let id = walk.songs[0].external_id.clone().expect("an id");
    walk.registry.set_you_tube_id(&id, OLD);
    choose(&walk, &id, "busyvideo-1");
    walk.slskd.state.lock().busy_videos.insert("busyvideo-1".into());
    assert!(
        open(&walk, &id, None, Some(&device("x_phone", None)))
            .await
            .is_none()
    );
    let r = walk.registry.lookup(&id).expect("known").snapshot();
    assert!(matches!(walk.registry.pick_for(&r), PickState::Person(_)));
}

#[tokio::test]
async fn a_download_takes_the_chosen_video_and_never_searches() {
    let root = Root::new("octo-choice-");
    let walk = build_with_soulseek(&root, false, DownloadSource::SoulseekThenYouTube, false, |_| {
        Arc::new(|_: &str| Vec::new())
    })
    .await;
    let id = walk.songs[3].external_id.clone().expect("an id");
    choose(&walk, &id, NEW);
    acquire(&walk, &id, None).await.expect("the chosen video lands");
    let downloads = walk.slskd.state.lock().you_tube_downloads.clone();
    // A search would have fetched `vid123`.
    assert_eq!(downloads.len(), 1, "{downloads:?}");
    assert!(downloads[0].contains(NEW), "{downloads:?}");
}

#[tokio::test]
async fn a_download_whose_chosen_video_fails_says_so() {
    let root = Root::new("octo-choice-");
    let walk = build_with_soulseek(&root, false, DownloadSource::SoulseekThenYouTube, false, |_| {
        Arc::new(|_: &str| Vec::new())
    })
    .await;
    let id = walk.songs[3].external_id.clone().expect("an id");
    choose(&walk, &id, NEW);
    walk.slskd.state.lock().failing_downloads.insert(NEW.into());
    let error = acquire(&walk, &id, None).await.expect_err("nothing to keep");
    assert!(
        format!("{error:#}").contains("The version chosen for this song could not be fetched from YouTube"),
        "{error:#}"
    );
}

#[tokio::test]
async fn a_hearts_download_takes_the_video_its_gate_heard_before_the_chosen_one() {
    let root = Root::new("octo-choice-");
    let walk = build_with(&root, false, DownloadSource::YouTube, |_| {
        Arc::new(|_: &str| Vec::new())
    })
    .await;
    let id = walk.songs[3].external_id.clone().expect("an id");
    choose(&walk, &id, NEW);
    {
        let mut state = walk.slskd.state.lock();
        state.failing_downloads.insert("heardvideo-1".into());
        state.failing_downloads.insert(NEW.into());
    }
    let gate = HeartGate {
        heard: Some(HeardRecord {
            heard_at: Utc::now(),
            source: HeardSource::Video {
                id: "heardvideo-1".into(),
                size: Some(12_345),
                content_type: "audio/mp4".into(),
                itag: None,
            },
            sample_id: None,
        }),
        ..started_gate()
    };
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        walk.service.execute_acquisition_to(
            "soulseek",
            &id,
            false,
            true,
            Some(DownloadSource::YouTube),
            &CancellationToken::new(),
            None,
            false,
            None,
            crate::services::common::AcquisitionPlace {
                origin: Some(AcquisitionOrigin::Heart),
                gates: vec![gate],
                ..Default::default()
            },
        ),
    )
    .await
    .expect("within a minute");
    assert!(result.is_err(), "neither video could be saved");
    let fetched: Vec<String> = walk
        ._server
        .received_requests()
        .await
        .expect("recording")
        .iter()
        .filter(|r| r.url.path() == "/download")
        .filter_map(|r| {
            r.url
                .query_pairs()
                .find(|(k, _)| k == "id")
                .map(|(_, v)| v.into_owned())
        })
        .collect();
    assert_eq!(
        fetched,
        ["heardvideo-1", NEW],
        "the heard video first, then the choice, never a search"
    );
}

#[tokio::test]
async fn an_older_octo_apps_play_that_is_over_takes_the_choice_and_one_still_streaming_does_not() {
    let root = Root::new("octo-choice-");
    let walk = build(&root, false, |_| Arc::new(|_: &str| Vec::new())).await;
    let id = walk.songs[0].external_id.clone().expect("an id");
    walk.registry.set_you_tube_id(&id, OLD);
    let older = StreamPlay {
        octo_device: true,
        ..device("x_octo_1_12", None)
    };
    open(&walk, &id, None, Some(&older)).await.expect("a stream");
    choose(&walk, &id, NEW);
    walk.registry.finish_play(&id, &older);

    // A body of the play still streams: the play is not over.
    let streaming = open(&walk, &id, Some("bytes=600-"), Some(&older))
        .await
        .expect("a stream");
    let again = open(&walk, &id, Some("bytes=0-"), Some(&older))
        .await
        .expect("a stream");
    assert_eq!(again.video_id.as_deref(), Some(OLD));
    drop(again);
    drop(streaming);

    // Over, with nothing streaming: a read from the first byte starts the song again.
    let fresh = open(&walk, &id, Some("bytes=0-"), Some(&older))
        .await
        .expect("a stream");
    assert_eq!(fresh.video_id.as_deref(), Some(NEW));
    assert_eq!(pin_of(&walk, &id).as_deref(), Some(NEW));
}
