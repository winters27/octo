//! Kept ids: the songs found online that playlists keep, which the registry never trims, writes
//! with the keys that keep them, and lets go of with their own recency.

use std::sync::Barrier;

use octo_core::fingerprint::heart_gate::{HeardBy, HeardSource};

use super::*;
use crate::services::playlists::seed_index::SeedIndex;

const MAX: usize = ExternalIdRegistry::MAX_ENTRIES;

fn song(artist: &str, title: &str) -> SoulseekRouting {
    SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some(artist.into()),
        title: Some(title.into()),
        duration: Some(200),
        ..Default::default()
    }
}

/// A song as it looks once it was played: a video, catalog facts and stream facts.
fn played(n: usize) -> SoulseekRouting {
    SoulseekRouting {
        kind: RoutingKind::Song,
        you_tube_id: Some(format!("vid{n:08}")),
        artist: Some(format!("Artist {}", n % 997)),
        title: Some(format!("Song number {n}")),
        album: Some(format!("Album {}", n % 4_999)),
        duration: Some(180 + (n % 120) as i32),
        track: Some(1 + (n % 12) as i32),
        disc_number: Some(1),
        total_tracks: Some(12),
        isrc: Some(format!("USAAA{n:07}")),
        shown_duration: Some(181),
        shown_duration_source: LengthSource::Deezer,
        explicit_content: Some(0),
        stream_size: Some(4_000_000),
        stream_itag: Some("140".into()),
        stream_ext: Some("m4a".into()),
        stream_bit_rate: Some(129),
        video_pick: SoulseekRouting::VIDEO_PICK,
        ..Default::default()
    }
}

fn temp_path() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("external-ids.json");
    (dir, path)
}

/// Registers `n` songs nobody keeps, the newest last.
fn fill(registry: &ExternalIdRegistry, prefix: &str, n: usize) -> Vec<String> {
    (0..n)
        .map(|i| registry.register(song("Filler", &format!("{prefix} {i}"))))
        .collect()
}

/// Keeps `ids` under `key`, each with the routing the registry holds for it now as its seed.
fn keep(registry: &ExternalIdRegistry, key: &str, revision: u64, ids: &[String]) {
    let seeds = ids
        .iter()
        .map(|id| (id.clone(), registry.peek(id).expect("registered").snapshot()))
        .collect();
    registry.set_kept(key, revision, seeds);
}

/// Registers `routing` and keeps it under `hP1`; its id and the kept routing.
fn kept_one(registry: &ExternalIdRegistry, routing: SoulseekRouting) -> (String, SharedRouting) {
    let id = registry.register(routing);
    keep(registry, "hP1", 1, std::slice::from_ref(&id));
    let kept = registry.peek(&id).expect("kept");
    (id, kept)
}

fn written(path: &Path) -> Vec<Persisted> {
    serde_json::from_str(&std::fs::read_to_string(path).expect("reads")).expect("parses")
}

fn line_of(path: &Path, id: &str) -> Persisted {
    written(path)
        .into_iter()
        .find(|line| line.id == id)
        .expect("written")
}

fn heard_video(video: &str, minute: u32) -> HeardRecord {
    use chrono::TimeZone;
    HeardRecord {
        heard_at: chrono::Utc
            .with_ymd_and_hms(2026, 11, 14, 12, minute, 0)
            .single()
            .expect("a time"),
        source: HeardSource::Video {
            id: video.into(),
            size: Some(4_000_000),
            content_type: "audio/webm".into(),
            itag: None,
        },
        sample_id: None,
    }
}

/// A hook that parks inserts made on the thread named "parked" until the test lets them go.
fn park_inserts(registry: &ExternalIdRegistry) -> (Arc<Barrier>, Arc<Barrier>) {
    let (parked, go) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
    let (at_hook, released) = (parked.clone(), go.clone());
    registry.pause_inserts_with(Arc::new(move || {
        if std::thread::current().name() == Some("parked") {
            at_hook.wait();
            released.wait();
        }
    }));
    (parked, go)
}

// ---- Kept, never trimmed --------------------------------------------------------------

#[test]
fn kept_ids_are_never_trimmed_past_the_lru_bound() {
    let registry = ExternalIdRegistry::in_memory();
    let kept = fill(&registry, "Kept", 50);
    keep(&registry, "hP1", 1, &kept);
    fill(&registry, "Newer", MAX);
    assert_eq!(registry.count(), MAX + 50);
    for id in &kept {
        assert!(registry.is_kept(id));
        assert!(registry.lookup(id).is_some());
    }
}

#[test]
fn a_file_of_recent_ids_plus_old_kept_ids_reloads_every_kept_id() {
    let (_dir, path) = temp_path();
    let kept = {
        let first = ExternalIdRegistry::new(Some(&path));
        let kept = fill(&first, "Kept", 50);
        keep(&first, "hP1", 1, &kept);
        fill(&first, "Newer", MAX);
        first.flush();
        kept
    };
    let reopened = ExternalIdRegistry::new(Some(&path));
    assert_eq!(reopened.count(), MAX + 50);
    // The playlist names its ids again before the start ends.
    keep(&reopened, "hP1", 1, &kept);
    reopened.finish_start();
    assert!(kept.iter().all(|id| reopened.is_kept(id)));
}

#[test]
fn registering_a_kept_id_again_merges_into_the_kept_routing() {
    let registry = ExternalIdRegistry::in_memory();
    let (id, kept) = kept_one(&registry, song("Daft Punk", "Emotion"));
    let (again, handle) = registry.register_handle(SoulseekRouting {
        isrc: Some("FRZ039800212".into()),
        ..song("Daft Punk", "Emotion")
    });
    assert_eq!(again, id);
    assert!(handle.ptr_eq(&kept));
    assert!(registry.lookup(&id).expect("known").ptr_eq(&kept));
    assert_eq!(kept.lock().isrc.as_deref(), Some("FRZ039800212"));
    assert_eq!(registry.count(), 1);
}

#[test]
fn a_set_kept_between_a_registers_read_and_its_insert_makes_no_second_copy() {
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let id = registry.register(song("Daft Punk", "Emotion"));
    let listed = registry.peek(&id).expect("listed");
    let (parked, go) = park_inserts(&registry);
    let writer = {
        let registry = registry.clone();
        std::thread::Builder::new()
            .name("parked".into())
            .spawn(move || {
                registry.register_handle(SoulseekRouting {
                    isrc: Some("FRZ039800212".into()),
                    ..song("Daft Punk", "Emotion")
                })
            })
            .expect("spawns")
    };
    parked.wait();
    keep(&registry, "hP1", 1, std::slice::from_ref(&id));
    go.wait();
    let (again, handle) = writer.join().expect("joins");
    assert_eq!(again, id);
    assert!(handle.ptr_eq(&listed), "the kept routing, not a second copy");
    assert_eq!(registry.count(), 1);
    assert!(registry.is_kept(&id));
    assert_eq!(listed.lock().isrc.as_deref(), Some("FRZ039800212"));
}

#[test]
fn scans_and_by_id_writers_reach_kept_ids() {
    let registry = ExternalIdRegistry::in_memory();
    let (id, kept) = kept_one(
        &registry,
        SoulseekRouting {
            album: Some("Discovery".into()),
            ..song("Daft Punk", "Emotion")
        },
    );
    fill(&registry, "Newer", MAX);

    assert!(registry.pin_video(&id, "vid-a", false));
    assert!(registry.remember_length(&id, Some(222), LengthSource::Deezer));
    assert!(registry.remember_stream(
        &id,
        "vid-a",
        &StreamFacts {
            size: Some(100),
            ..Default::default()
        }
    ));
    assert_eq!(registry.stream_facts(&id).and_then(|f| f.size), Some(100));
    assert!(registry.stale_pins().contains(&id));
    assert!(registry.remember_pick(&id, "vid-a", SoulseekRouting::VIDEO_PICK));
    assert!(!registry.stale_pins().contains(&id));
    assert!(
        registry
            .pinned_videos(5, &HashSet::new())
            .contains(&"vid-a".to_string())
    );
    let filed = registry.songs_filed_under(Some("Daft Punk"), Some("Discovery"), 5);
    assert!(filed.iter().any(|(found, _)| *found == id));

    assert!(registry.set_pending_video(&id, "vid-a", "vid-b"));
    assert_eq!(registry.apply_pending_video(&kept).as_deref(), Some("vid-a"));
    assert_eq!(kept.lock().you_tube_id.as_deref(), Some("vid-b"));
    assert!(registry.set_you_tube_id(&id, "vid-c"));

    // A refusal through another id of the same song reaches the kept routing.
    let other = registry.register(SoulseekRouting {
        duration: Some(201),
        ..song("Daft Punk", "Emotion")
    });
    assert!(registry.refuse_video(&other, "vid-c"));
    assert!(kept.lock().refuses_video("vid-c"));
    assert_eq!(kept.lock().you_tube_id, None);
}

#[test]
fn changes_to_a_kept_id_set_the_dirty_flag_and_reach_the_next_file() {
    let (_dir, path) = temp_path();
    let registry = ExternalIdRegistry::new(Some(&path));
    let (id, _) = kept_one(&registry, song("Daft Punk", "Emotion"));
    registry.flush();
    assert!(!registry.dirty.load(Ordering::SeqCst));

    assert!(registry.pin_video(&id, "vid-a", false));
    assert!(registry.dirty.load(Ordering::SeqCst));
    registry.flush();
    let line = line_of(&path, &id);
    assert_eq!(line.kept, vec!["hP1".to_string()]);
    assert_eq!(line.routing.and_then(|r| r.you_tube_id).as_deref(), Some("vid-a"));

    assert!(registry.remember_heard(&id, "alice", heard_video("vid-a", 1), None));
    assert!(registry.dirty.load(Ordering::SeqCst));
    registry.flush();
    assert_eq!(line_of(&path, &id).routing.expect("routing").heard.len(), 1);

    assert!(registry.refuse_video(&id, "vid-a"));
    assert!(registry.dirty.load(Ordering::SeqCst));
    registry.flush();
    let routing = line_of(&path, &id).routing.expect("routing");
    assert!(routing.refused_videos.contains(&"vid-a".to_string()));
    assert_eq!(routing.you_tube_id, None);
}

#[test]
fn a_released_id_goes_back_to_the_lru_with_its_own_stamp_and_no_kept_field() {
    let (_dir, path) = temp_path();
    let registry = ExternalIdRegistry::new(Some(&path));
    let (id, _) = kept_one(&registry, song("Daft Punk", "Emotion"));
    registry.flush();
    let stamp = line_of(&path, &id).recency.expect("a stamp");

    registry.set_kept("hP1", 2, Vec::new());
    assert!(!registry.is_kept(&id));
    registry.flush();
    let line = line_of(&path, &id);
    assert!(line.kept.is_empty());
    assert_eq!(line.recency, Some(stamp));
    assert!(
        !std::fs::read_to_string(&path)
            .expect("reads")
            .contains("\"Kept\"")
    );
}

#[test]
fn an_older_revision_is_ignored_and_a_release_then_a_hold_keeps_the_id() {
    let registry = ExternalIdRegistry::in_memory();
    let id = registry.register(song("Daft Punk", "Emotion"));
    keep(&registry, "hP1", 5, std::slice::from_ref(&id));
    registry.set_kept("hP1", 4, Vec::new());
    assert!(registry.is_kept(&id), "an older call changes nothing");
    registry.set_kept("hP1", 6, Vec::new());
    assert!(!registry.is_kept(&id));
    registry.set_kept("hP1", 5, vec![(id.clone(), song("Daft Punk", "Emotion"))]);
    assert!(
        !registry.is_kept(&id),
        "the key remembers its revision after it empties"
    );
    keep(&registry, "hP1", 7, std::slice::from_ref(&id));
    fill(&registry, "Newer", MAX);
    assert!(registry.lookup(&id).is_some());
}

#[test]
fn two_keys_keep_one_id_until_both_let_go() {
    let registry = ExternalIdRegistry::in_memory();
    let id = registry.register(song("Daft Punk", "Emotion"));
    keep(&registry, "hP1", 1, std::slice::from_ref(&id));
    keep(&registry, "fP2", 1, std::slice::from_ref(&id));
    registry.set_kept("hP1", 2, Vec::new());
    assert!(registry.is_kept(&id));
    registry.set_kept("fP2", 2, Vec::new());
    assert!(!registry.is_kept(&id));
    assert_eq!(registry.count(), 1);
}

#[test]
fn finish_start_lets_go_of_keys_nobody_named_again() {
    let (_dir, path) = temp_path();
    let (a, b) = {
        let first = ExternalIdRegistry::new(Some(&path));
        let a = first.register(song("Daft Punk", "Emotion"));
        let b = first.register(song("Justice", "Genesis"));
        keep(&first, "hP1", 1, std::slice::from_ref(&a));
        keep(&first, "hP2", 1, std::slice::from_ref(&b));
        first.flush();
        (a, b)
    };
    let reopened = ExternalIdRegistry::new(Some(&path));
    assert!(reopened.is_kept(&a) && reopened.is_kept(&b));
    keep(&reopened, "hP1", 1, std::slice::from_ref(&a));
    reopened.finish_start();
    assert!(reopened.is_kept(&a));
    assert!(!reopened.is_kept(&b));
    assert!(reopened.peek(&b).is_some());
}

#[test]
fn restore_as_files_a_seed_under_its_id_and_drops_a_refused_pin() {
    let registry = ExternalIdRegistry::in_memory();
    registry.refuse_song_video("KILLY", "Killamonjaro", "summer-cem");
    let seed = SoulseekRouting {
        you_tube_id: Some("summer-cem".into()),
        ..song("KILLY", "Killamonjaro")
    };
    let id = "AAAAAAAAAAAAAAAAAAAAAA";
    assert_ne!(ExternalIdRegistry::make_short_id(&seed), id);
    let restored = registry.restore_as(id, seed);
    assert!(registry.lookup(id).expect("filed as given").ptr_eq(&restored));
    assert_eq!(restored.lock().you_tube_id, None);
    assert!(restored.lock().refuses_video("summer-cem"));

    // The seed's own refusal goes the same way.
    let own = registry.restore_as(
        "BBBBBBBBBBBBBBBBBBBBBB",
        SoulseekRouting {
            you_tube_id: Some("v2".into()),
            refused_videos: vec!["v2".into()],
            ..song("Someone", "Else")
        },
    );
    assert_eq!(own.lock().you_tube_id, None);
}

#[test]
fn a_sources_answer_is_filed_as_given_but_the_imports_hook_is_still_hash_checked() {
    let registry = ExternalIdRegistry::in_memory();
    let seed = SoulseekRouting {
        you_tube_id: Some("vid-a".into()),
        ..song("Daft Punk", "Emotion")
    };
    let id = "CCCCCCCCCCCCCCCCCCCCCC".to_string();
    let hook_calls = Arc::new(AtomicUsize::new(0));
    {
        let (calls, answer) = (hook_calls.clone(), seed.clone());
        registry.set_recovery(Arc::new(move |_: &str| {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(answer.clone())
        }));
    }
    assert!(
        registry.lookup(&id).is_none(),
        "the hook's answer hashes to another id"
    );
    assert_eq!(hook_calls.load(Ordering::SeqCst), 1);

    {
        let (wanted, answer) = (id.clone(), seed.clone());
        registry.set_recovery_sources(vec![Arc::new(move |asked: &str| {
            (asked == wanted).then(|| answer.clone())
        })]);
    }
    let found = registry.lookup(&id).expect("from the source");
    assert_eq!(found.lock().you_tube_id.as_deref(), Some("vid-a"));
    assert_eq!(hook_calls.load(Ordering::SeqCst), 1, "the source is asked first");
}

#[test]
fn a_kept_id_without_a_pin_takes_its_seeds_video_unless_the_song_refuses_it() {
    let registry = ExternalIdRegistry::in_memory();
    let id = registry.register(song("Daft Punk", "Emotion"));
    registry.set_kept(
        "hP1",
        1,
        vec![(
            id.clone(),
            SoulseekRouting {
                you_tube_id: Some("vid-a".into()),
                video_pick: SoulseekRouting::VIDEO_PICK,
                ..song("Daft Punk", "Emotion")
            },
        )],
    );
    let routing = registry.peek(&id).expect("kept").snapshot();
    assert_eq!(routing.you_tube_id.as_deref(), Some("vid-a"));
    assert_eq!(routing.video_pick, SoulseekRouting::VIDEO_PICK);

    let refused = registry.register(song("KILLY", "Killamonjaro"));
    assert!(registry.refuse_video(&refused, "summer-cem"));
    registry.set_kept(
        "hP2",
        1,
        vec![(
            refused.clone(),
            SoulseekRouting {
                you_tube_id: Some("summer-cem".into()),
                ..song("KILLY", "Killamonjaro")
            },
        )],
    );
    assert_eq!(registry.peek(&refused).expect("kept").lock().you_tube_id, None);
}

// ---- What a kept routing takes from a newcomer ------------------------------------------

#[test]
fn a_kept_id_keeps_its_stronger_length_and_takes_blank_facts_from_a_newcomer() {
    let registry = ExternalIdRegistry::in_memory();
    let (id, kept) = kept_one(&registry, song("Justice", "Genesis"));
    assert!(registry.remember_length(&id, Some(234), LengthSource::File));
    registry.register(SoulseekRouting {
        shown_duration: Some(230),
        shown_duration_source: LengthSource::LastFm,
        track: Some(1),
        disc_number: Some(1),
        total_tracks: Some(10),
        isrc: Some("FR6V80700001".into()),
        album: Some("Cross".into()),
        explicit_content: Some(0),
        external_artist_id: Some("1".into()),
        replaces_library_file: true,
        ..song("Justice", "Genesis")
    });
    let k = kept.snapshot();
    assert_eq!(kept.shown_length(), (Some(234), LengthSource::File));
    assert_eq!(
        (k.track, k.disc_number, k.total_tracks),
        (Some(1), Some(1), Some(10))
    );
    assert_eq!(k.isrc.as_deref(), Some("FR6V80700001"));
    assert_eq!(k.album.as_deref(), Some("Cross"));
    assert_eq!(k.explicit_content, Some(0));
    assert_eq!(k.external_artist_id.as_deref(), Some("1"));
    assert!(k.replaces_library_file);

    // A stronger length is taken.
    let (other, stronger) = kept_one(&registry, song("Justice", "D.A.N.C.E."));
    assert!(registry.remember_length(&other, Some(200), LengthSource::Video));
    registry.register(SoulseekRouting {
        shown_duration: Some(242),
        shown_duration_source: LengthSource::Deezer,
        ..song("Justice", "D.A.N.C.E.")
    });
    assert_eq!(stronger.shown_length(), (Some(242), LengthSource::Deezer));
}

#[test]
fn a_kept_id_unions_refusals_and_merges_heard_records() {
    let registry = ExternalIdRegistry::in_memory();
    let (id, kept) = kept_one(&registry, song("KILLY", "Killamonjaro"));
    assert!(registry.pin_video(&id, "summer-cem", false));
    assert!(registry.remember_heard(&id, "alice", heard_video("vid-x", 1), None));
    registry.register(SoulseekRouting {
        refused_videos: vec!["summer-cem".into()],
        heard: vec![HeardBy {
            user: "bob".into(),
            record: heard_video("vid-y", 2),
        }],
        ..song("KILLY", "Killamonjaro")
    });
    let k = kept.snapshot();
    assert!(k.refuses_video("summer-cem"));
    assert_eq!(k.you_tube_id, None);
    assert!(k.heard_by("alice").is_some());
    assert!(k.heard_by("bob").is_some());
}

#[test]
fn a_kept_id_without_a_video_takes_the_newcomers_pin_with_its_stream_and_pick() {
    let registry = ExternalIdRegistry::in_memory();
    let named = SoulseekRouting {
        you_tube_id: Some("vid-a".into()),
        ..song("Daft Punk", "Emotion")
    };
    let (id, kept) = kept_one(&registry, named.clone());
    // A caller unpins it in place (not refused), so the kept routing has no video.
    kept.lock().unpin_video("vid-a");
    registry.mark_changed(&id);
    registry.register(SoulseekRouting {
        stream_size: Some(100),
        stream_itag: Some("140".into()),
        ..named
    });
    let k = kept.snapshot();
    assert_eq!(k.you_tube_id.as_deref(), Some("vid-a"));
    assert_eq!(k.video_pick, SoulseekRouting::VIDEO_NAMED);
    assert_eq!(k.stream_size, Some(100));
}

#[test]
fn registering_the_kept_handle_itself_skips_the_merge_but_marks_it_changed() {
    let (_dir, path) = temp_path();
    let registry = ExternalIdRegistry::new(Some(&path));
    let (id, kept) = kept_one(&registry, song("Daft Punk", "Emotion"));
    registry.flush();
    // A caller edits in place and registers the same routing again.
    kept.lock().album = Some("Discovery".into());
    let (again, handle) = registry.register_handle(kept.clone());
    assert_eq!(again, id);
    assert!(handle.ptr_eq(&kept));
    assert!(registry.dirty.load(Ordering::SeqCst));
    registry.flush();
    assert_eq!(
        line_of(&path, &id).routing.and_then(|r| r.album).as_deref(),
        Some("Discovery")
    );
}

#[test]
fn the_same_video_fills_the_kept_routings_missing_stream_facts() {
    let registry = ExternalIdRegistry::in_memory();
    let named = SoulseekRouting {
        you_tube_id: Some("vid-a".into()),
        ..song("Daft Punk", "Emotion")
    };
    let (id, _) = kept_one(&registry, named.clone());
    assert_eq!(registry.stream_facts(&id), None);
    registry.register(SoulseekRouting {
        stream_size: Some(500),
        ..named
    });
    assert_eq!(registry.stream_facts(&id).and_then(|f| f.size), Some(500));
}

// ---- Seeds for ids that are not kept ---------------------------------------------------

#[test]
fn a_bound_entrys_id_restores_from_the_seed_index_after_10000_newer() {
    let registry = ExternalIdRegistry::in_memory();
    let index = Arc::new(SeedIndex::new());
    registry.set_recovery_sources(vec![index.recovery()]);
    let hook_calls = Arc::new(AtomicUsize::new(0));
    {
        let calls = hook_calls.clone();
        registry.set_recovery(Arc::new(move |_: &str| -> Option<SoulseekRouting> {
            calls.fetch_add(1, Ordering::SeqCst);
            None
        }));
    }
    let id = registry.register(song("Daft Punk", "Emotion"));
    // Played: its video was pinned after the id was minted, so it no longer hashes to it.
    assert!(registry.pin_video(&id, "vid-b", true));
    let seed = registry.peek(&id).expect("registered").snapshot();
    index.set("hP1", 1, vec![(id.clone(), Arc::new(seed))]);
    fill(&registry, "Newer", MAX);
    assert!(registry.peek(&id).is_none(), "evicted");

    let found = registry.resolve_song(&id, false).expect("restored from its seed");
    assert_eq!(found.lock().you_tube_id.as_deref(), Some("vid-b"));
    assert_eq!(
        hook_calls.load(Ordering::SeqCst),
        0,
        "the seed index is asked first"
    );
    // It went in at the newest end, so one more registration at the bound leaves it.
    registry.register(song("Filler", "One more"));
    assert!(registry.peek(&id).is_some());
}

#[test]
fn a_seed_two_playlists_share_survives_one_removing_it_and_older_revisions_are_ignored() {
    let registry = ExternalIdRegistry::in_memory();
    let index = Arc::new(SeedIndex::new());
    registry.set_recovery_sources(vec![index.recovery()]);
    let id = registry.register(song("Daft Punk", "Emotion"));
    let seed = Arc::new(registry.peek(&id).expect("registered").snapshot());
    index.set("hP1", 1, vec![(id.clone(), seed.clone())]);
    index.set("fP2", 1, vec![(id.clone(), seed)]);
    index.set("hP1", 2, Vec::new());
    index.set("fP2", 0, Vec::new());
    fill(&registry, "Newer", MAX);
    assert!(registry.lookup(&id).is_some());
}

#[test]
fn a_set_kept_racing_a_restore_leaves_one_copy() {
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let index = Arc::new(SeedIndex::new());
    registry.set_recovery_sources(vec![index.recovery()]);
    let seed = SoulseekRouting {
        you_tube_id: Some("vid-b".into()),
        ..song("Daft Punk", "Emotion")
    };
    let id = "DDDDDDDDDDDDDDDDDDDDDD".to_string();
    index.set("hP1", 1, vec![(id.clone(), Arc::new(seed.clone()))]);
    let (parked, go) = park_inserts(&registry);
    let reader = {
        let (registry, id) = (registry.clone(), id.clone());
        std::thread::Builder::new()
            .name("parked".into())
            .spawn(move || registry.lookup(&id))
            .expect("spawns")
    };
    parked.wait();
    registry.set_kept("hP1", 1, vec![(id.clone(), seed)]);
    go.wait();
    let found = reader.join().expect("joins").expect("restored");
    assert_eq!(registry.count(), 1);
    assert!(registry.is_kept(&id));
    assert!(found.ptr_eq(&registry.peek(&id).expect("one copy")));
}

// ---- Restarts and a lost file ----------------------------------------------------------

#[test]
fn a_kept_id_survives_a_restart_and_10000_newer_with_its_video_pinned_before_or_after() {
    for pin_first in [true, false] {
        let (_dir, path) = temp_path();
        let id = {
            let first = ExternalIdRegistry::new(Some(&path));
            let id = first.register(song("Daft Punk", "Emotion"));
            if pin_first {
                assert!(first.pin_video(&id, "vid-a", true));
            }
            keep(&first, "hP1", 1, std::slice::from_ref(&id));
            if !pin_first {
                assert!(first.pin_video(&id, "vid-a", true));
            }
            id
        }; // dropped: the drop flushes
        let reopened = ExternalIdRegistry::new(Some(&path));
        keep(&reopened, "hP1", 1, std::slice::from_ref(&id));
        reopened.finish_start();
        fill(&reopened, "Newer", MAX);
        let found = reopened.resolve_song(&id, false).expect("kept");
        assert_eq!(
            found.lock().you_tube_id.as_deref(),
            Some("vid-a"),
            "pin_first={pin_first}"
        );
    }
}

#[test]
fn a_lost_registry_file_restores_a_kept_id_from_its_seed() {
    let (_dir, path) = temp_path();
    let id = ExternalIdRegistry::make_short_id(&song("Daft Punk", "Emotion"));
    let registry = ExternalIdRegistry::new(Some(&path));
    registry.set_kept(
        "hP1",
        1,
        vec![(
            id.clone(),
            SoulseekRouting {
                you_tube_id: Some("vid-b".into()),
                ..song("Daft Punk", "Emotion")
            },
        )],
    );
    assert!(registry.is_kept(&id));
    fill(&registry, "Newer", MAX);
    assert_eq!(
        registry
            .lookup(&id)
            .expect("restored")
            .lock()
            .you_tube_id
            .as_deref(),
        Some("vid-b")
    );
}

// ---- Writing at size --------------------------------------------------------------------

#[test]
fn at_the_worst_kept_size_a_flush_is_one_write_under_the_bound_and_lru_flushes_reuse_kept_lines() {
    const KEPT: usize = 60_000;
    const PART: usize = 5_000;
    let (_dir, path) = temp_path();
    let registry = ExternalIdRegistry::new(Some(&path));
    let mut kept = Vec::with_capacity(KEPT);
    for part in 0..KEPT / PART {
        let ids: Vec<String> = (part * PART..(part + 1) * PART)
            .map(|n| registry.register(played(n)))
            .collect();
        keep(&registry, &format!("hPlaylist{part}"), 1, &ids);
        kept.extend(ids);
    }
    let live: Vec<String> = (KEPT..KEPT + MAX).map(|n| registry.register(played(n))).collect();
    assert_eq!(registry.count(), KEPT + MAX);

    let writes = registry.file_writes.load(Ordering::SeqCst);
    registry.flush();
    assert_eq!(
        registry.file_writes.load(Ordering::SeqCst),
        writes + 1,
        "one file write"
    );
    let bytes = std::fs::metadata(&path).expect("written").len();
    assert!(bytes < 40 * 1024 * 1024, "{bytes} bytes");
    assert_eq!(registry.kept_renders.load(Ordering::SeqCst), KEPT);

    // The LRU first, most recent first, then the kept lines.
    let lines = written(&path);
    assert_eq!(lines.len(), KEPT + MAX);
    assert_eq!(lines[0].id, live[MAX - 1]);
    assert!(lines[..MAX].iter().all(|line| line.kept.is_empty()));
    assert!(lines[..MAX].windows(2).all(|w| w[0].recency > w[1].recency));
    assert!(lines[MAX..].iter().all(|line| !line.kept.is_empty()));

    // A search afterwards writes the LRU again and reuses every kept line.
    registry.register(played(KEPT + MAX));
    registry.flush();
    assert_eq!(registry.kept_renders.load(Ordering::SeqCst), KEPT);

    // Letting go of 15,000 long-unplayed ids keeps the 10,000 most recent live ones.
    for part in 0..3 {
        registry.set_kept(&format!("hPlaylist{part}"), 2, Vec::new());
    }
    assert!(live[1..].iter().all(|id| registry.peek(id).is_some()));
    assert!(kept[..15_000].iter().all(|id| registry.peek(id).is_none()));
    assert_eq!(registry.count(), KEPT - 15_000 + MAX);
}

#[test]
fn an_id_played_a_moment_ago_and_then_released_stays_live_also_across_a_restart() {
    // In one process.
    let registry = ExternalIdRegistry::in_memory();
    let (id, _) = kept_one(&registry, song("Daft Punk", "Emotion"));
    fill(&registry, "Newer", MAX);
    registry.lookup(&id);
    registry.set_kept("hP1", 2, Vec::new());
    assert!(registry.peek(&id).is_some());
    assert_eq!(registry.count(), MAX);

    // Across a restart: the kept line was cached, then a play moved its stamp.
    let (_dir, path) = temp_path();
    let id = {
        let first = ExternalIdRegistry::new(Some(&path));
        let (id, _) = kept_one(&first, song("Daft Punk", "Emotion"));
        first.flush();
        fill(&first, "Newer", MAX);
        first.flush();
        first.lookup(&id);
        id
    }; // dropped: the drop flushes
    let reopened = ExternalIdRegistry::new(Some(&path));
    reopened.finish_start();
    assert!(!reopened.is_kept(&id));
    assert!(reopened.peek(&id).is_some(), "played a moment ago, so it stays");
    assert_eq!(reopened.count(), MAX);
}

#[test]
fn a_playlist_synced_many_times_then_released_leaves_the_live_lru_intact() {
    let registry = ExternalIdRegistry::in_memory();
    let kept = fill(&registry, "Kept", 100);
    keep(&registry, "hP1", 1, &kept);
    let live = fill(&registry, "Live", MAX);
    for _ in 0..20 {
        for id in &kept {
            assert!(registry.peek(id).is_some());
        }
    }
    registry.set_kept("hP1", 2, Vec::new());
    assert!(live.iter().all(|id| registry.peek(id).is_some()));
    assert!(kept.iter().all(|id| registry.peek(id).is_none()));
}

#[test]
fn a_kept_bump_renders_one_line() {
    let (_dir, path) = temp_path();
    let registry = ExternalIdRegistry::new(Some(&path));
    let kept = fill(&registry, "Kept", 100);
    keep(&registry, "hP1", 1, &kept);
    registry.flush();
    assert_eq!(registry.kept_renders.load(Ordering::SeqCst), 100);
    registry.lookup(&kept[7]);
    registry.flush();
    assert_eq!(registry.kept_renders.load(Ordering::SeqCst), 101);
}

#[test]
fn a_broadcast_refusal_and_a_pending_switch_reach_a_cached_kept_line() {
    let (_dir, path) = temp_path();
    let registry = ExternalIdRegistry::new(Some(&path));
    let (id, kept) = kept_one(&registry, song("KILLY", "Killamonjaro"));
    assert!(registry.set_you_tube_id(&id, "summer-cem"));
    registry.flush();

    let other = registry.register(SoulseekRouting {
        duration: Some(201),
        ..song("KILLY", "Killamonjaro")
    });
    assert!(registry.refuse_video(&other, "summer-cem"));
    registry.flush();
    let routing = line_of(&path, &id).routing.expect("routing");
    assert!(routing.refused_videos.contains(&"summer-cem".to_string()));
    assert_eq!(routing.you_tube_id, None);

    assert!(registry.set_you_tube_id(&id, "vid-a"));
    registry.flush();
    assert!(registry.set_pending_video(&id, "vid-a", "vid-b"));
    registry.apply_pending_video(&kept);
    registry.flush();
    assert_eq!(
        line_of(&path, &id).routing.and_then(|r| r.you_tube_id).as_deref(),
        Some("vid-b")
    );
}

#[test]
fn a_failed_flush_keeps_its_ids_for_the_next_one() {
    let (_dir, path) = temp_path();
    let registry = ExternalIdRegistry::new(Some(&path));
    let (id, _) = kept_one(&registry, song("Daft Punk", "Emotion"));
    registry.flush();
    assert!(registry.pin_video(&id, "vid-a", false));
    std::fs::remove_file(&path).expect("removes");
    std::fs::create_dir(&path).expect("a folder in the file's place");
    registry.flush();
    assert!(registry.dirty.load(Ordering::SeqCst), "still to write");
    assert!(registry.lru.lock().kept_changed.contains(&id));
    std::fs::remove_dir(&path).expect("clears");
    registry.flush();
    assert_eq!(
        line_of(&path, &id).routing.and_then(|r| r.you_tube_id).as_deref(),
        Some("vid-a")
    );
    assert!(registry.lru.lock().kept_changed.is_empty());
}

#[tokio::test]
async fn the_flusher_writes_at_the_interval_it_was_built_with() {
    let (_dir, path) = temp_path();
    let registry =
        Arc::new(ExternalIdRegistry::new(Some(&path)).with_flush_interval(Duration::from_millis(20)));
    registry.register(song("Daft Punk", "Emotion"));
    let token = tokio_util::sync::CancellationToken::new();
    let flusher = tokio::spawn(registry.clone().run_flusher(token.clone()));
    // Waits for the file, not for a time: the default interval would not write for 10 s.
    let appeared = tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(appeared.is_ok(), "written at the interval it was built with");
    token.cancel();
    flusher.await.expect("joins").expect("stops");
}
