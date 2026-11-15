//! Chosen versions in the registry: the switch at a play's start, the guards, the ids a choice
//! reaches, Octo's pick after an undo, heard records and their samples, plays bound to their
//! videos, and what a re-registration and a seed keep.

use chrono::{TimeDelta, Utc};
use octo_core::fingerprint::heart_gate::HeardSource;
use octo_core::soulseek::PlayBinding;

use super::*;
use crate::services::i_download_service::StreamPlay;
use crate::services::soulseek::stream_choices::{ChoiceCaps, StreamChoice};

fn song(title: &str, catalog: i32) -> SoulseekRouting {
    SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("Drake".into()),
        title: Some(title.into()),
        duration: Some(catalog),
        shown_duration: Some(catalog),
        shown_duration_source: LengthSource::Deezer,
        ..Default::default()
    }
}

/// Registers `routing` pinned to `video`, with that video's confirmed size. A routing a choice
/// already holds stays unpinned (the pin is refused).
fn pinned(registry: &ExternalIdRegistry, routing: SoulseekRouting, video: &str) -> String {
    let id = registry.register(routing);
    registry.set_you_tube_id(&id, video);
    registry.remember_stream(
        &id,
        video,
        &StreamFacts {
            size: Some(4_000_000),
            size_confirmed: true,
            ..Default::default()
        },
    );
    id
}

fn choice(video: &str, seconds: i32) -> StreamChoice {
    StreamChoice {
        video: video.into(),
        seconds: Some(seconds),
        by: "ede".into(),
        at: Utc::now(),
        ..Default::default()
    }
}

fn octo(video: &str) -> StreamChoice {
    StreamChoice {
        video: video.into(),
        seconds: Some(215),
        at: Utc::now(),
        ..Default::default()
    }
}

fn routing_of(registry: &ExternalIdRegistry, id: &str) -> SoulseekRouting {
    registry.peek(id).expect("registered").snapshot()
}

fn start(registry: &ExternalIdRegistry, id: &str) -> Option<String> {
    let routing = registry.peek(id).expect("registered");
    registry.apply_choice_at_start(&routing)
}

fn play(token: Option<&str>, address: &str) -> StreamPlay {
    StreamPlay {
        key: "x_phone".into(),
        token: token.map(str::to_string),
        address: Some(address.into()),
        method: "GET".into(),
        ..Default::default()
    }
}

fn heard(video: &str, at: chrono::DateTime<Utc>) -> HeardRecord {
    HeardRecord {
        heard_at: at,
        source: HeardSource::Video {
            id: video.into(),
            size: Some(1000),
            content_type: "audio/mp4".into(),
            itag: None,
        },
        sample_id: None,
    }
}

fn video_of(binding: Option<PlayBinding>) -> Option<String> {
    binding.map(|binding| binding.video)
}

#[test]
fn a_start_takes_the_chosen_video_with_its_mark_and_the_old_facts_go() {
    let registry = ExternalIdRegistry::in_memory();
    let id = pinned(&registry, song("Started", 215), "old");
    let ids = registry
        .choose_stream_video(&id, choice("new", 220))
        .expect("stored");
    assert_eq!(ids, std::slice::from_ref(&id));
    // Nothing moves before a play starts.
    assert_eq!(routing_of(&registry, &id).you_tube_id.as_deref(), Some("old"));
    assert_eq!(
        registry.song_video(&routing_of(&registry, &id)).as_deref(),
        Some("new")
    );
    assert!(registry.awaits_switch(&id));
    assert_eq!(
        registry.song_stream_facts(&id),
        None,
        "the old video's facts are not the song's"
    );
    assert!(registry.stream_facts(&id).is_some());

    assert_eq!(
        start(&registry, &id).as_deref(),
        Some("old"),
        "the old video's copy may go"
    );
    let r = routing_of(&registry, &id);
    assert_eq!(r.you_tube_id.as_deref(), Some("new"));
    assert_eq!(r.video_pick, SoulseekRouting::VIDEO_CHOSEN);
    assert_eq!(
        (r.shown_duration, r.shown_duration_source),
        (Some(215), LengthSource::Deezer),
        "a video's length never over the catalog's"
    );
    assert_eq!(r.stream_size, None);
    assert!(!registry.awaits_switch(&id));
    assert!(matches!(registry.pick_for(&r), PickState::Person(_)));
    assert_eq!(start(&registry, &id), None, "a second start changes nothing");
}

#[test]
fn a_kept_ids_switch_is_written_and_read_back() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("external-ids.json");
    let id = {
        let registry = ExternalIdRegistry::new(Some(&path));
        let seed = song("Started", 215);
        let id = registry.register(seed.clone());
        registry.set_kept("playlist-1", 1, vec![(id.clone(), seed)]);
        assert!(registry.is_kept(&id));
        registry.set_you_tube_id(&id, "old");
        registry.flush();
        registry
            .choose_stream_video(&id, choice("new", 215))
            .expect("stored");
        start(&registry, &id);
        registry.flush();
        id
    };
    let again = ExternalIdRegistry::new(Some(&path));
    let r = routing_of(&again, &id);
    assert_eq!(
        (r.you_tube_id.as_deref(), r.video_pick),
        (Some("new"), SoulseekRouting::VIDEO_CHOSEN)
    );
    assert!(matches!(again.pick_for(&r), PickState::Person(_)));
}

#[test]
fn a_persons_choice_and_an_earlier_one_hold_the_video_and_octos_pick_does_not() {
    let registry = ExternalIdRegistry::in_memory();
    let id = pinned(&registry, song("Started", 215), "old");
    registry
        .choose_stream_video(&id, choice("new", 215))
        .expect("stored");
    start(&registry, &id);
    for video in ["other", "new"] {
        assert!(!registry.pin_video(&id, video, true), "{video}");
    }
    assert!(!registry.remember_pick(&id, "new", SoulseekRouting::VIDEO_PICK));
    assert!(!registry.set_pending_video(&id, "new", "better"));
    let routing = registry.peek(&id).expect("registered");
    routing.lock().pending_video = Some("better".into());
    assert_eq!(registry.apply_pending_video(&routing), None);
    let r = routing_of(&registry, &id);
    assert_eq!(
        (r.you_tube_id.as_deref(), r.pending_video.as_deref()),
        (Some("new"), None)
    );

    // An earlier choice: the mark with no record.
    let earlier = pinned(&registry, song("Hold On", 200), "kept");
    registry.peek(&earlier).expect("registered").lock().video_pick = SoulseekRouting::VIDEO_CHOSEN;
    assert_eq!(
        registry.pick_for(&routing_of(&registry, &earlier)),
        PickState::Earlier
    );
    assert!(!registry.pin_video(&earlier, "other", true));
    assert_eq!(
        registry.song_video(&routing_of(&registry, &earlier)).as_deref(),
        Some("kept")
    );

    // The chosen mark is written only by a play's start.
    let plain = pinned(&registry, song("Intro", 90), "v");
    assert!(!registry.remember_pick(&plain, "v", SoulseekRouting::VIDEO_CHOSEN));

    // Octo's own pick holds nothing.
    registry.undo_stream_choice(&id, octo("octo")).expect("stored");
    assert!(matches!(
        registry.pick_for(&routing_of(&registry, &id)),
        PickState::Octo(_)
    ));
    assert!(registry.pin_video(&id, "elsewhere", true));
}

#[test]
fn a_choice_reaches_every_id_of_the_song_whose_length_fits() {
    let registry = ExternalIdRegistry::in_memory();
    let id = pinned(&registry, song("Started", 215), "old");
    let other = pinned(&registry, song("Started", 219), "old2");
    let longer = pinned(&registry, song("Started", 245), "live");
    let clean = pinned(
        &registry,
        SoulseekRouting {
            version: Some("clean".into()),
            ..song("Started", 215)
        },
        "clean",
    );
    let ids = registry
        .choose_stream_video(&id, choice("new", 300))
        .expect("stored");
    assert!(ids.contains(&id) && ids.contains(&other), "{ids:?}");
    assert!(!ids.contains(&longer) && !ids.contains(&clean), "{ids:?}");
    assert_eq!(registry.song_ids(&other).first(), Some(&other));
    // The other id switches at its own start; the longer one never does.
    start(&registry, &other);
    assert_eq!(routing_of(&registry, &other).you_tube_id.as_deref(), Some("new"));
    start(&registry, &longer);
    assert_eq!(
        routing_of(&registry, &longer).you_tube_id.as_deref(),
        Some("live")
    );
    // A length a video gave is not the song's: such a routing is reached.
    let measured = pinned(
        &registry,
        SoulseekRouting {
            duration: Some(300),
            shown_duration: Some(300),
            shown_duration_source: LengthSource::Video,
            ..song("Started", 0)
        },
        "v",
    );
    assert!(matches!(
        registry.pick_for(&routing_of(&registry, &measured)),
        PickState::Person(_)
    ));
}

#[test]
fn with_no_catalog_length_a_choice_fits_by_the_chosen_videos_length() {
    let registry = ExternalIdRegistry::in_memory();
    let bare = SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("Drake".into()),
        title: Some("Started".into()),
        duration: Some(100),
        ..Default::default()
    };
    let id = pinned(&registry, bare, "old");
    registry
        .choose_stream_video(&id, choice("new", 240))
        .expect("stored");
    let key = StreamChoices::key_of(&routing_of(&registry, &id)).expect("a key");
    assert_eq!(
        registry
            .choices()
            .record_for_key(&key)
            .and_then(|r| r.song_seconds),
        None
    );
    let near = pinned(&registry, song("Started", 236), "a");
    let far = pinned(&registry, song("Started", 215), "b");
    assert!(matches!(
        registry.pick_for(&routing_of(&registry, &near)),
        PickState::Person(_)
    ));
    assert_eq!(registry.pick_for(&routing_of(&registry, &far)), PickState::None);
}

#[test]
fn a_refused_video_is_never_chosen_and_refusing_a_chosen_one_forgets_it() {
    let registry = ExternalIdRegistry::in_memory();
    let id = pinned(&registry, song("Started", 215), "old");
    registry.refuse_song_video("Drake", "Started", "bad");
    assert_eq!(
        registry.choose_stream_video(&id, choice("bad", 215)),
        Err(ChooseError::Refused)
    );
    registry
        .choose_stream_video(&id, choice("new", 215))
        .expect("stored");
    let before = registry.choices().changes_since(None, Some(0), 500).seq;
    assert!(registry.refuse_song_video("Drake", "Started", "new"));
    assert_eq!(registry.pick_for(&routing_of(&registry, &id)), PickState::None);
    let feed = registry.changes_since(None, Some(before), 500);
    assert_eq!(feed.changes.len(), 1);
    assert!(
        feed.changes[0].1.contains(&id),
        "the tombstone names the song's ids"
    );
}

#[test]
fn an_undo_switches_routings_still_marked_chosen_to_octos_pick() {
    let registry = ExternalIdRegistry::in_memory();
    let id = pinned(&registry, song("Started", 215), "old");
    registry
        .choose_stream_video(&id, choice("new", 215))
        .expect("stored");
    start(&registry, &id);
    registry.undo_stream_choice(&id, octo("octo")).expect("stored");
    let r = routing_of(&registry, &id);
    assert!(matches!(registry.pick_for(&r), PickState::Octo(_)));
    assert_eq!(registry.song_video(&r).as_deref(), Some("octo"));
    assert!(!registry.pin_guarded(&r));
    assert_eq!(start(&registry, &id).as_deref(), Some("new"));
    let r = routing_of(&registry, &id);
    assert_eq!(
        (r.you_tube_id.as_deref(), r.video_pick),
        (Some("octo"), SoulseekRouting::VIDEO_PICK)
    );
    assert_eq!(registry.song_video(&r).as_deref(), Some("octo"));
}

#[test]
fn an_earlier_choice_plays_on_and_can_be_undone() {
    let registry = ExternalIdRegistry::in_memory();
    let id = pinned(&registry, song("Started", 215), "kept");
    registry.peek(&id).expect("registered").lock().video_pick = SoulseekRouting::VIDEO_CHOSEN;
    assert_eq!(registry.pick_for(&routing_of(&registry, &id)), PickState::Earlier);
    assert_eq!(start(&registry, &id), None, "never reverted on its own");
    registry.undo_stream_choice(&id, octo("octo")).expect("stored");
    start(&registry, &id);
    assert_eq!(routing_of(&registry, &id).you_tube_id.as_deref(), Some("octo"));
}

#[test]
fn octos_pick_put_out_of_a_full_store_switches_its_routings_at_once() {
    let registry = ExternalIdRegistry::in_memory();
    registry.choices().set_caps(ChoiceCaps {
        records: 1,
        ..ChoiceCaps::default()
    });
    let id = pinned(&registry, song("Started", 215), "old");
    registry
        .choose_stream_video(&id, choice("new", 215))
        .expect("stored");
    start(&registry, &id);
    registry.undo_stream_choice(&id, octo("octo")).expect("stored");
    let other = pinned(&registry, song("Hold On", 200), "x");
    registry
        .choose_stream_video(&other, choice("y", 200))
        .expect("stored");
    let r = routing_of(&registry, &id);
    assert_eq!(
        (r.you_tube_id.as_deref(), r.video_pick),
        (Some("octo"), SoulseekRouting::VIDEO_PICK)
    );
}

#[test]
fn a_sample_finished_after_the_record_was_dropped_brings_nothing_back() {
    let registry = ExternalIdRegistry::in_memory();
    let id = pinned(&registry, song("Started", 215), "old");
    let at = Utc::now();
    let record = heard("old", at);
    assert!(registry.remember_heard(&id, "ede", record.clone(), None));
    assert!(registry.remember_heard(&id, "anna", heard("old", at), None));
    let dropped = registry.forget_heard_of_song(std::slice::from_ref(&id), "Ede");
    assert_eq!(dropped.len(), 1);
    assert!(
        registry.heard_by(&id, "anna").is_some(),
        "only the chooser's record goes"
    );
    assert!(!registry.attach_sample(&id, "ede", at, &record.source, "s1"));
    assert_eq!(registry.heard_by(&id, "ede"), None);
    assert!(registry.attach_sample(&id, "anna", at, &record.source, "s1"));
    assert_eq!(
        registry
            .heard_by(&id, "anna")
            .and_then(|r| r.sample_id)
            .as_deref(),
        Some("s1")
    );
    assert!(
        !registry.attach_sample(&id, "anna", at + TimeDelta::seconds(1), &record.source, "s2"),
        "another play's sample"
    );
}

#[test]
fn a_token_binding_is_found_by_its_token_and_a_device_keeps_two() {
    let registry = ExternalIdRegistry::in_memory();
    let id = pinned(&registry, song("Started", 215), "old");
    registry.bind_play(&id, &play(Some("e1.0"), "10.0.0.2"), "old");
    registry.bind_play(&id, &play(Some("e2.0"), "10.0.0.2"), "new");
    // The same token from another address is the same play.
    assert_eq!(
        video_of(registry.bound_play(&id, &play(Some("e1.0"), "192.168.1.5"))).as_deref(),
        Some("old")
    );
    assert_eq!(
        video_of(registry.bound_play(&id, &play(Some("e2.0"), "10.0.0.2"))).as_deref(),
        Some("new")
    );
    assert_eq!(registry.bound_play(&id, &play(Some("e3.0"), "10.0.0.2")), None);
    // A third token: the oldest with no body streaming goes.
    let _streaming = registry.live_play(&id, &play(Some("e1.0"), "10.0.0.2"));
    registry.bind_play(&id, &play(Some("e3.0"), "10.0.0.2"), "new");
    assert!(
        registry
            .bound_play(&id, &play(Some("e1.0"), "10.0.0.2"))
            .is_some(),
        "its body streams"
    );
    assert_eq!(registry.bound_play(&id, &play(Some("e2.0"), "10.0.0.2")), None);
}

#[test]
fn without_a_token_a_start_replaces_its_address_and_finished_plays_elsewhere_go() {
    let registry = ExternalIdRegistry::in_memory();
    let id = pinned(&registry, song("Started", 215), "old");
    let office = play(None, "10.0.0.2");
    let home = play(None, "192.168.1.5");
    registry.bind_play(&id, &office, "old");
    registry.finish_play(&id, &office);
    registry.bind_play(&id, &home, "new");
    assert_eq!(
        routing_of(&registry, &id).plays.len(),
        1,
        "the finished office play went"
    );
    // A later request from the office goes on with the play left.
    assert_eq!(
        video_of(registry.bound_play(&id, &office)).as_deref(),
        Some("new")
    );
    registry.bind_play(&id, &home, "newer");
    assert_eq!(routing_of(&registry, &id).plays.len(), 1);
    registry.confirm_play_size(&id, &home, 5_000);
    assert_eq!(registry.bound_play(&id, &home).and_then(|b| b.size), Some(5_000));
}

#[test]
fn a_start_while_the_same_devices_body_streams_shares_that_play() {
    let registry = ExternalIdRegistry::in_memory();
    let id = pinned(&registry, song("Started", 215), "old");
    let first = play(None, "10.0.0.2");
    registry.bind_play(&id, &first, "old");
    assert_eq!(registry.live_binding(&id, &first), None);
    let streaming = registry.live_play(&id, &first);
    assert_eq!(
        video_of(registry.live_binding(&id, &first)).as_deref(),
        Some("old")
    );
    drop(streaming);
    assert_eq!(registry.live_binding(&id, &first), None);
    // A look at the song never counts as streaming.
    let look = StreamPlay {
        method: "HEAD".into(),
        ..play(None, "10.0.0.2")
    };
    let _look = registry.live_play(&id, &look);
    assert_eq!(registry.live_binding(&id, &first), None);
}

#[test]
fn bindings_outlive_a_restart() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("external-ids.json");
    let p = play(Some("e1.0"), "10.0.0.2");
    let id = {
        let registry = ExternalIdRegistry::new(Some(&path));
        let id = pinned(&registry, song("Started", 215), "old");
        registry.bind_play(&id, &p, "old");
        registry.confirm_play_size(&id, &p, 4_000_000);
        registry.flush();
        id
    };
    let again = ExternalIdRegistry::new(Some(&path));
    let bound = again.bound_play(&id, &p).expect("still bound");
    assert_eq!((bound.video.as_str(), bound.size), ("old", Some(4_000_000)));
}

#[test]
fn a_fresh_mint_keeps_the_chosen_video_and_the_bound_plays() {
    let registry = ExternalIdRegistry::in_memory();
    // Minted with its video, then another chosen: the id stays the mint's.
    let minted = SoulseekRouting {
        you_tube_id: Some("mint".into()),
        ..song("Started", 215)
    };
    let id = registry.register(minted.clone());
    registry
        .choose_stream_video(&id, choice("new", 215))
        .expect("stored");
    start(&registry, &id);
    registry.bind_play(&id, &play(Some("e1.0"), "10.0.0.2"), "mint");
    assert_eq!(registry.register(minted), id, "a search mints the song again");
    let r = routing_of(&registry, &id);
    assert_eq!(
        (r.you_tube_id.as_deref(), r.video_pick),
        (Some("new"), SoulseekRouting::VIDEO_CHOSEN)
    );
    assert_eq!(r.plays.len(), 1);

    // Minted with no video: the chosen pin and mark come along too.
    let bare = song("Hold On", 200);
    let other = registry.register(bare.clone());
    registry
        .choose_stream_video(&other, choice("pick", 200))
        .expect("stored");
    start(&registry, &other);
    registry.bind_play(&other, &play(None, "10.0.0.2"), "pick");
    assert_eq!(registry.register(bare), other);
    let r = routing_of(&registry, &other);
    assert_eq!(
        (r.you_tube_id.as_deref(), r.video_pick, r.plays.len()),
        (Some("pick"), SoulseekRouting::VIDEO_CHOSEN, 1)
    );

    // An earlier choice is kept the same way.
    let lone = SoulseekRouting {
        you_tube_id: Some("lone".into()),
        ..song("Intro", 90)
    };
    let third = registry.register(lone.clone());
    registry.set_you_tube_id(&third, "earlier");
    registry.peek(&third).expect("registered").lock().video_pick = SoulseekRouting::VIDEO_CHOSEN;
    registry.register(lone);
    assert_eq!(
        routing_of(&registry, &third).you_tube_id.as_deref(),
        Some("earlier")
    );
}

#[test]
fn a_kept_routing_under_a_choice_takes_no_video_from_a_seed() {
    let registry = ExternalIdRegistry::in_memory();
    let seed = song("Started", 215);
    let id = registry.register(seed.clone());
    registry.set_kept("p", 1, vec![(id.clone(), seed.clone())]);
    registry
        .choose_stream_video(&id, choice("new", 215))
        .expect("stored");
    registry.restore_as(
        &id,
        SoulseekRouting {
            you_tube_id: Some("seeded".into()),
            ..seed
        },
    );
    let r = routing_of(&registry, &id);
    assert_eq!(r.you_tube_id, None);
    assert_eq!(registry.song_video(&r).as_deref(), Some("new"));
}

#[test]
fn a_restored_seed_brings_no_old_plays_and_no_heard_record_older_than_the_choice() {
    let registry = ExternalIdRegistry::in_memory();
    let id = pinned(&registry, song("Started", 215), "old");
    let before = Utc::now() - TimeDelta::minutes(5);
    registry
        .choose_stream_video(&id, choice("new", 215))
        .expect("stored");
    let mut seed = song("Started", 215);
    seed.you_tube_id = Some("new".into());
    seed.video_pick = SoulseekRouting::VIDEO_CHOSEN;
    seed.plays = vec![PlayBinding {
        key: "x".into(),
        video: "old".into(),
        ..Default::default()
    }];
    seed.remember_heard("ede", heard("old", before));
    seed.remember_heard("anna", heard("old", before));
    let restored = registry.restore_as("restoredid0000000000000", seed).snapshot();
    assert!(restored.plays.is_empty());
    assert!(restored.heard_by("ede").is_none());
    assert!(restored.heard_by("anna").is_some());
    // A chosen mark no record backs comes back as Octo's pick.
    let mut orphan = song("Hold On", 200);
    orphan.you_tube_id = Some("x".into());
    orphan.video_pick = SoulseekRouting::VIDEO_CHOSEN;
    assert_eq!(
        registry
            .restore_as("orphanid000000000000000", orphan)
            .snapshot()
            .video_pick,
        SoulseekRouting::VIDEO_PICK
    );
}

#[test]
fn a_re_registration_between_a_lookup_and_its_bind_keeps_the_binding() {
    let registry = ExternalIdRegistry::in_memory();
    let bare = song("Started", 215);
    let id = registry.register(bare.clone());
    let p = play(Some("e1.0"), "10.0.0.2");
    assert!(registry.bound_play(&id, &p).is_none());
    // A search mints the song again between the play's lookup and its bind.
    registry.register(bare.clone());
    registry.bind_play(&id, &p, "v");
    assert_eq!(video_of(registry.bound_play(&id, &p)).as_deref(), Some("v"));
    registry.register(bare);
    assert_eq!(
        video_of(registry.bound_play(&id, &p)).as_deref(),
        Some("v"),
        "carried over"
    );
}
