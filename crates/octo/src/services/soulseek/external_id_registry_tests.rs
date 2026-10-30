//! Tests of the external id registry, the lengths it keeps for a song, and the
//! `external-ids.json` fixture round trip.

use super::*;

fn song(artist: &str, title: &str) -> SoulseekRouting {
    SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some(artist.into()),
        title: Some(title.into()),
        ..Default::default()
    }
}

fn album(artist: &str, album: &str, deezer: Option<&str>) -> SoulseekRouting {
    SoulseekRouting {
        kind: RoutingKind::Album,
        artist: Some(artist.into()),
        album: Some(album.into()),
        external_album_id: deezer.map(String::from),
        ..Default::default()
    }
}

fn temp_path(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join(name);
    (dir, path)
}

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/rust-migration/fixtures/state/external-ids.json"
);

// ---- Registering and looking up --------------------------------------------------------

#[test]
fn register_same_routing_produces_same_id() {
    let registry = ExternalIdRegistry::in_memory();
    let a = SoulseekRouting {
        duration: Some(255),
        ..song("Radiohead", "Nude")
    };
    let b = a.clone();
    assert_eq!(registry.register(a), registry.register(b));
}

#[test]
fn register_different_kinds_same_names_produce_different_ids() {
    // The Kind prefix keeps a song id distinct from its album and artist ids,
    // otherwise getCoverArt would return the wrong scope's artwork.
    let registry = ExternalIdRegistry::in_memory();
    let song_id = registry.register(song("Radiohead", "In Rainbows"));
    let album_id = registry.register(album("Radiohead", "In Rainbows", None));
    let artist_id = registry.register(SoulseekRouting {
        kind: RoutingKind::Artist,
        artist: Some("Radiohead".into()),
        ..Default::default()
    });
    assert_ne!(song_id, album_id);
    assert_ne!(album_id, artist_id);
    assert_ne!(song_id, artist_id);
}

#[test]
fn register_album_without_deezer_id_does_not_clobber_known_external_album_id() {
    let registry = ExternalIdRegistry::in_memory();
    // An album search registers the precise Deezer id.
    let id = registry.register(album("Radiohead", "In Rainbows", Some("14880659")));
    // A song row later mints the same artist+album with no Deezer id. It hashes
    // to the same key, so a naive overwrite would drop the id we already resolved.
    let same_id = registry.register(album("Radiohead", "In Rainbows", None));
    assert_eq!(id, same_id);
    assert_eq!(
        registry
            .lookup(&id)
            .expect("known")
            .lock()
            .external_album_id
            .as_deref(),
        Some("14880659")
    );
}

#[test]
fn register_album_with_deezer_id_overwrites_an_earlier_unknown_id() {
    // The preserve must only fill blanks, never block a real update.
    let registry = ExternalIdRegistry::in_memory();
    let id = registry.register(album("Radiohead", "In Rainbows", None));
    assert!(
        registry
            .lookup(&id)
            .expect("known")
            .lock()
            .external_album_id
            .is_none()
    );

    registry.register(album("Radiohead", "In Rainbows", Some("14880659")));

    assert_eq!(
        registry
            .lookup(&id)
            .expect("known")
            .lock()
            .external_album_id
            .as_deref(),
        Some("14880659")
    );
}

#[test]
fn lookup_unknown_id_returns_null() {
    assert!(ExternalIdRegistry::in_memory().lookup("nonexistent").is_none());
}

// ---- Persistence ----------------------------------------------------------------------
// The registry is the only thing that knows an id is ours, so losing it on restart
// made every id a client still held look local. Those were relayed to Navidrome,
// which answers error 70 "data not found" for media it does not have, and the client
// showed that on every play and every poll.

#[test]
fn register_survives_a_restart() {
    let (_dir, path) = temp_path("octo-ids.json");
    let id = {
        let first = ExternalIdRegistry::new(Some(&path));
        first.register(SoulseekRouting {
            duration: Some(290),
            ..song("Radiohead", "Reckoner")
        })
    }; // dropped: the drop flushes

    let reopened = ExternalIdRegistry::new(Some(&path));
    let routing = reopened.lookup(&id).expect("restored").snapshot();
    assert_eq!(routing.artist.as_deref(), Some("Radiohead"));
    assert_eq!(routing.title.as_deref(), Some("Reckoner"));
    assert_eq!(routing.duration, Some(290));
}

#[test]
fn register_with_no_path_keeps_working_in_memory() {
    // The parameterless form is still a valid registry, just not a durable one.
    let registry = ExternalIdRegistry::new(None::<&str>);
    let id = registry.register(song("A", "B"));
    assert!(registry.lookup(&id).is_some());
    let blank = ExternalIdRegistry::new(Some("   "));
    blank.register(song("A", "B"));
    blank.flush();
}

#[test]
fn load_unreadable_file_starts_empty_rather_than_throwing() {
    // A registry that will not parse is a cold start, not a failure to boot.
    let (_dir, path) = temp_path("octo-ids.json");
    std::fs::write(&path, "{ this is not the file you are looking for").expect("writes");
    let registry = ExternalIdRegistry::new(Some(&path));
    assert!(registry.lookup("anything").is_none());

    // ...and it must still be usable afterwards.
    let id = registry.register(song("A", "B"));
    assert!(registry.lookup(&id).is_some());
}

#[test]
fn register_artist_by_name_alone_keeps_the_catalog_artist_already_chosen() {
    // Two artists can share a name, and the page settled on one of them. Every album row
    // mints its artist again by name alone, which must not undo that choice.
    let registry = ExternalIdRegistry::in_memory();
    let id = registry.register(SoulseekRouting {
        kind: RoutingKind::Artist,
        artist: Some("Nirvana".into()),
        external_artist_id: Some("415".into()),
        ..Default::default()
    });
    registry.register(SoulseekRouting {
        kind: RoutingKind::Artist,
        artist: Some("Nirvana".into()),
        ..Default::default()
    });
    assert_eq!(
        registry
            .lookup(&id)
            .expect("known")
            .lock()
            .external_artist_id
            .as_deref(),
        Some("415")
    );
}

// ---- Song lengths: the registry keeps them ----------------------------------------------

#[test]
fn registry_re_minting_a_song_keeps_the_length_a_lookup_found() {
    let registry = ExternalIdRegistry::in_memory();
    let id = registry.register(song("Justice", "Genesis"));
    assert!(registry.remember_length(&id, Some(234), LengthSource::Deezer));

    // The next search mints a fresh routing for the same song.
    let again = registry.register(song("Justice", "Genesis"));

    assert_eq!(id, again);
    assert_eq!(
        registry.lookup(&id).expect("known").shown_length(),
        (Some(234), LengthSource::Deezer)
    );
}

#[test]
fn registry_remember_length_leaves_the_download_expectation_alone() {
    // Duration is what a download ranks and checks peer files against. A length found
    // only so a row can show one must not start rejecting files.
    let registry = ExternalIdRegistry::in_memory();
    let id = registry.register(song("Kavinsky", "Prelude"));

    registry.remember_length(&id, Some(95), LengthSource::LastFm);

    let routing = registry.lookup(&id).expect("known").snapshot();
    assert_eq!(routing.duration, None);
    assert_eq!(routing.you_tube_id, None);
    assert_eq!(routing.shown_duration, Some(95));
}

#[test]
fn registry_remember_length_ignores_unknown_ids_and_non_songs() {
    let registry = ExternalIdRegistry::in_memory();
    let album_id = registry.register(album("A", "B", None));

    assert!(!registry.remember_length("nope", Some(200), LengthSource::Deezer));
    assert!(!registry.remember_length(&album_id, Some(200), LengthSource::Deezer));
    assert!(!registry.remember_length("", Some(200), LengthSource::Deezer));
}

#[test]
fn registry_length_outlives_a_restart() {
    let (_dir, path) = temp_path("octo-lengths.json");
    let id = {
        let first = ExternalIdRegistry::new(Some(&path));
        let id = first.register(song("Daft Punk", "Emotion"));
        first.remember_length(&id, Some(417), LengthSource::Video);
        id
    };
    let second = ExternalIdRegistry::new(Some(&path));
    assert_eq!(
        second.lookup(&id).expect("restored").shown_length(),
        (Some(417), LengthSource::Video)
    );
}

// ---- The fixture and the rest of the registry ------------------------------------------

#[test]
fn the_fixture_round_trips_byte_for_byte() {
    let text = std::fs::read_to_string(FIXTURE).expect("the fixture is in the repo");
    let entries: Vec<Persisted> = serde_json::from_str(&text).expect("the fixture reads");
    assert_eq!(entries.len(), 5);
    // 2026.10.05: an explicit song and its clean edit, which has an id of its own.
    assert!(
        entries
            .iter()
            .any(|e| e.routing.as_ref().is_some_and(|r| r.explicit_content == Some(1)))
    );
    assert!(entries.iter().any(|e| {
        e.routing
            .as_ref()
            .is_some_and(|r| r.version.as_deref() == Some("clean"))
    }));
    assert_eq!(octo_core::json::to_string(&entries), text.trim_end_matches('\n'));
}

#[test]
fn the_fixture_ids_are_the_ones_this_registry_mints() {
    // The fixture is a real registry file, so its ids are genuine.
    let text = std::fs::read_to_string(FIXTURE).expect("the fixture is in the repo");
    let entries: Vec<Persisted> = serde_json::from_str(&text).expect("the fixture reads");
    for e in entries {
        let routing = e.routing.expect("a routing");
        assert_eq!(ExternalIdRegistry::make_short_id(&routing), e.id, "{routing:?}");
    }
}

#[test]
fn a_load_and_flush_writes_the_fixture_back_unchanged() {
    let (_dir, path) = temp_path("external-ids.json");
    let text = std::fs::read_to_string(FIXTURE).expect("the fixture is in the repo");
    std::fs::write(&path, &text).expect("writes");
    {
        let registry = ExternalIdRegistry::new(Some(&path));
        assert_eq!(registry.count(), 5);
        // Nothing changed, so nothing is written.
        registry.flush();
        assert_eq!(std::fs::read_to_string(&path).expect("reads"), text);
        // A change makes it write, in the same MRU order (the change touches nothing).
        let first = "V4cZhYE5O7Cf31Zq4RCzV3";
        assert!(!registry.remember_length(first, Some(100), LengthSource::Video));
        registry.dirty.store(true, Ordering::SeqCst);
    }
    assert_eq!(
        std::fs::read_to_string(&path).expect("reads"),
        text.trim_end_matches('\n')
    );
    assert!(!state_file::tmp_path(&path).exists());
}

#[test]
fn the_lru_keeps_the_most_recently_used_and_writes_them_first() {
    let (_dir, path) = temp_path("ids.json");
    let registry = ExternalIdRegistry::new(Some(&path));
    let a = registry.register(song("A", "1"));
    let b = registry.register(song("B", "2"));
    let c = registry.register(song("C", "3"));
    registry.lookup(&a);
    registry.flush();
    let written: Vec<Persisted> =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("reads")).expect("parses");
    let ids: Vec<&str> = written.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, vec![a.as_str(), c.as_str(), b.as_str()]);

    let small = ExternalIdRegistry::in_memory();
    let first = small.register(song("X", "0"));
    for i in 1..=ExternalIdRegistry::MAX_ENTRIES {
        small.register(song("X", &i.to_string()));
    }
    assert_eq!(small.count(), ExternalIdRegistry::MAX_ENTRIES);
    assert!(small.lookup(&first).is_none(), "the oldest is evicted");
}

#[test]
fn a_looked_up_routing_changed_in_place_is_what_the_registry_holds() {
    // ResolveTopDurations pins a video on the routing it looked up; getSong then reads it.
    let registry = ExternalIdRegistry::in_memory();
    let id = registry.register(song("Daft Punk", "Emotion"));
    let routing = registry.lookup(&id).expect("known");
    routing.lock().you_tube_id = Some("playing-now".into());
    assert_eq!(
        registry.lookup(&id).expect("known").lock().you_tube_id.as_deref(),
        Some("playing-now")
    );

    // An artist page settles the catalog artist on the routing it looked up and registers that
    // same object again (ReferenceEquals: nothing to merge).
    let artist_id = registry.register(SoulseekRouting {
        kind: RoutingKind::Artist,
        artist: Some("Nirvana".into()),
        ..Default::default()
    });
    let artist = registry.lookup(&artist_id).expect("known");
    artist.lock().external_artist_id = Some("415".into());
    assert_eq!(registry.register(artist.clone()), artist_id);
    assert!(registry.lookup(&artist_id).expect("known").ptr_eq(&artist));
}

#[test]
fn songs_filed_under_finds_one_row_per_recording_newest_first() {
    let registry = ExternalIdRegistry::in_memory();
    let with_album = |title: &str, album: Option<&str>, duration: i32| SoulseekRouting {
        album: album.map(String::from),
        duration: Some(duration),
        ..song("Justice", title)
    };
    let older = registry.register(with_album("Genesis", Some("Cross"), 1));
    let newer = registry.register(with_album("Genesis", Some("Cross"), 2));
    let phantom = registry.register(with_album("Phantom", Some("Cross"), 3));
    let own_title = registry.register(with_album("Cross", None, 4));
    registry.register(with_album("Genesis", Some("Other"), 5));
    registry.register(album("Justice", "Cross", None));

    let found: Vec<String> = registry
        .songs_filed_under(Some("Justice"), Some("Cross"), 50)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(found, vec![own_title, phantom, newer.clone()]);
    assert!(!found.contains(&older));
    assert_eq!(
        registry
            .songs_filed_under(Some("Justice"), Some("Cross"), 1)
            .len(),
        1
    );
    assert!(registry.songs_filed_under(None, Some("Cross"), 50).is_empty());
    assert!(
        registry
            .songs_filed_under(Some("justice"), Some("Cross"), 50)
            .is_empty()
    );
}

// ---- Outside songs' explicit word: ids ----------------------------------------------------

/// The seed an id is made from is unchanged for everything but a clean copy, so a star, a
/// playlist place or a pin made before keeps working. Pinned to the ids the old seed made.
#[test]
fn ids_only_a_clean_copy_gets_a_new_one() {
    let registry = ExternalIdRegistry::in_memory();
    let song = |version: Option<&str>| SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("Kendrick Lamar".into()),
        title: Some("luther".into()),
        duration: Some(177),
        version: version.map(String::from),
        ..Default::default()
    };
    let album = |version: Option<&str>| SoulseekRouting {
        kind: RoutingKind::Album,
        artist: Some("Kendrick Lamar".into()),
        album: Some("GNX".into()),
        version: version.map(String::from),
        ..Default::default()
    };

    assert_eq!(registry.register(song(None)), "tkNhkzBNCpsT1buw9JyZd5");
    assert_eq!(
        registry.register(song(Some("explicit"))),
        "tkNhkzBNCpsT1buw9JyZd5"
    );
    assert_eq!(registry.register(album(None)), "7tcNOznFZSzl3pGOYcZfl7");
    assert_ne!(registry.register(song(Some("clean"))), "tkNhkzBNCpsT1buw9JyZd5");
    assert_ne!(registry.register(album(Some("clean"))), "7tcNOznFZSzl3pGOYcZfl7");
}

#[test]
fn registry_a_search_row_minted_again_keeps_what_the_catalog_said() {
    let registry = ExternalIdRegistry::in_memory();
    let id = registry.register(SoulseekRouting {
        explicit_content: Some(1),
        ..song("Tame Impala", "Dracula")
    });
    registry.register(song("Tame Impala", "Dracula"));

    assert_eq!(
        registry
            .lookup(&id)
            .expect("registered")
            .snapshot()
            .explicit_content,
        Some(1)
    );
}

/// A song's id leaves out its album, so radio registering a song Last.fm named without one must
/// not undo the album a search filed it under. A registration with an album of its own still
/// replaces it.
#[test]
fn a_registration_without_an_album_keeps_the_album_already_known() {
    let registry = ExternalIdRegistry::in_memory();
    let searched = registry.register(SoulseekRouting {
        album: Some("Tidal Lines".into()),
        ..song("Zephyr Echo", "Glass Harbor")
    });
    let radio = registry.register(song("Zephyr Echo", "Glass Harbor"));
    assert_eq!(searched, radio);
    let album_of = |id: &str| registry.lookup(id).expect("known").lock().album.clone();
    assert_eq!(album_of(&radio).as_deref(), Some("Tidal Lines"));

    let blank = registry.register(SoulseekRouting {
        album: Some("  ".into()),
        ..song("Zephyr Echo", "Glass Harbor")
    });
    assert_eq!(album_of(&blank).as_deref(), Some("Tidal Lines"));

    let other = registry.register(SoulseekRouting {
        album: Some("Another Album".into()),
        ..song("Zephyr Echo", "Glass Harbor")
    });
    assert_eq!(album_of(&other).as_deref(), Some("Another Album"));

    // An album routing is not a song: its own album is its id.
    let first = registry.register(album("Zephyr Echo", "Tidal Lines", None));
    assert_eq!(album_of(&first).as_deref(), Some("Tidal Lines"));
}

// ---- The stream an outside song plays -------------------------------------------------------

fn sized(size: u64) -> StreamFacts {
    StreamFacts {
        size: Some(size),
        itag: Some("140".into()),
        ext: Some("m4a".into()),
        bit_rate: Some(129),
        ..Default::default()
    }
}

#[test]
fn a_re_registered_song_keeps_its_video_and_size() {
    let registry = ExternalIdRegistry::in_memory();
    let id = registry.register(song("Daft Punk", "Emotion"));
    assert!(registry.set_you_tube_id(&id, "vid-a"));
    assert!(registry.remember_stream(&id, "vid-a", &sized(6_755_585)));

    // A later search mints the song again, with no video of its own.
    assert_eq!(registry.register(song("Daft Punk", "Emotion")), id);
    let r = registry.lookup(&id).expect("known").snapshot();
    assert_eq!(r.you_tube_id.as_deref(), Some("vid-a"));
    assert_eq!(registry.stream_facts(&id), Some(sized(6_755_585)));
}

#[test]
fn a_different_video_clears_the_size() {
    let registry = ExternalIdRegistry::in_memory();
    let id = registry.register(song("Daft Punk", "Emotion"));
    registry.set_you_tube_id(&id, "vid-a");
    registry.remember_stream(&id, "vid-a", &sized(100));

    // A lookup that would keep another pin does not move it, and so keeps the size.
    assert!(!registry.pin_video(&id, "vid-b", false));
    assert_eq!(registry.stream_facts(&id).and_then(|f| f.size), Some(100));

    // A re-pin does, and the size belonged to the old video.
    assert!(registry.set_you_tube_id(&id, "vid-b"));
    assert_eq!(registry.stream_facts(&id), None);
    // Facts for a video the song no longer plays are not taken.
    assert!(!registry.remember_stream(&id, "vid-a", &sized(100)));
    assert!(registry.remember_stream(&id, "vid-b", &sized(200)));
    assert_eq!(registry.stream_facts(&id).and_then(|f| f.size), Some(200));
    assert!(!registry.set_you_tube_id("unknown", "vid-b"));
    assert!(!registry.remember_stream("unknown", "vid-b", &sized(1)));
}

#[test]
fn a_re_pin_and_a_size_are_written_down_and_outlive_a_restart() {
    let (_dir, path) = temp_path("octo-ids.json");
    let first = ExternalIdRegistry::new(Some(&path));
    let id = first.register(song("Daft Punk", "Emotion"));
    first.flush();
    first.set_you_tube_id(&id, "vid-a");
    first.remember_stream(&id, "vid-a", &sized(6_755_585));
    first.flush();
    drop(first);

    let reopened = ExternalIdRegistry::new(Some(&path));
    assert_eq!(
        reopened
            .lookup(&id)
            .expect("restored")
            .lock()
            .you_tube_id
            .as_deref(),
        Some("vid-a")
    );
    assert_eq!(reopened.stream_facts(&id), Some(sized(6_755_585)));

    // A re-pin alone marks the registry changed, so it is written too.
    reopened.flush();
    reopened.set_you_tube_id(&id, "vid-b");
    reopened.flush();
    drop(reopened);
    let again = ExternalIdRegistry::new(Some(&path));
    assert_eq!(
        again.lookup(&id).expect("restored").lock().you_tube_id.as_deref(),
        Some("vid-b")
    );
    assert_eq!(again.stream_facts(&id), None);
}

#[test]
fn refusing_a_video_unpins_it_and_keeps_it_out() {
    let registry = ExternalIdRegistry::in_memory();
    let id = registry.register(song("KILLY", "Killamonjaro"));
    assert!(registry.set_you_tube_id(&id, "summer-cem"));

    assert!(registry.refuse_video(&id, "summer-cem"));
    let snapshot = registry.lookup(&id).expect("known").snapshot();
    assert_eq!(snapshot.you_tube_id, None);
    assert!(snapshot.refuses_video("summer-cem"));

    assert!(!registry.set_you_tube_id(&id, "summer-cem"));
    assert!(!registry.pin_video(&id, "summer-cem", false));
    assert!(!registry.refuse_video("no-such-id", "x"));
}

#[test]
fn registering_the_same_song_again_keeps_its_refused_videos() {
    let registry = ExternalIdRegistry::in_memory();
    let id = registry.register(song("KILLY", "Killamonjaro"));
    assert!(registry.refuse_video(&id, "summer-cem"));

    let again = registry.register(song("KILLY", "Killamonjaro"));
    assert_eq!(again, id);
    assert!(
        registry
            .lookup(&id)
            .expect("known")
            .snapshot()
            .refuses_video("summer-cem")
    );
    assert!(!registry.set_you_tube_id(&id, "summer-cem"));
}

#[test]
fn a_refused_video_outlives_a_restart() {
    let (_dir, path) = temp_path("octo-ids.json");
    let first = ExternalIdRegistry::new(Some(&path));
    let id = first.register(song("KILLY", "Killamonjaro"));
    first.flush();
    assert!(first.refuse_video(&id, "summer-cem"));
    first.flush();
    drop(first);

    let reopened = ExternalIdRegistry::new(Some(&path));
    let routing = reopened.lookup(&id).expect("restored");
    assert!(routing.lock().refuses_video("summer-cem"));
    assert!(!reopened.set_you_tube_id(&id, "summer-cem"));
}

fn song_of_length(artist: &str, title: &str, seconds: i32) -> SoulseekRouting {
    SoulseekRouting {
        duration: Some(seconds),
        ..song(artist, title)
    }
}

#[test]
fn a_refusal_holds_for_every_id_of_the_song_cached_or_registered_later() {
    let registry = ExternalIdRegistry::in_memory();
    let a = registry.register(song_of_length("KILLY", "Killamonjaro", 153));
    let b = registry.register(song_of_length("killy", "Killamonjaro!", 179));
    assert_ne!(a, b);
    assert!(registry.set_you_tube_id(&b, "summer-cem"));

    assert!(registry.refuse_video(&a, "summer-cem"));

    let other = registry.lookup(&b).expect("known").snapshot();
    assert!(other.refuses_video("summer-cem"));
    assert_eq!(other.you_tube_id, None);

    let later = registry.register(song_of_length("KILLY", "Killamonjaro", 200));
    let later = registry.lookup(&later).expect("known").snapshot();
    assert!(later.refuses_video("summer-cem"));

    let mut decoded = song("KILLY", "Killamonjaro");
    registry.apply_refusals(&mut decoded);
    assert_eq!(decoded.refused_videos, ["summer-cem"]);
    assert_eq!(registry.refusals_for(&decoded), ["summer-cem"]);

    let mut stranger = song("Someone", "Else");
    registry.apply_refusals(&mut stranger);
    assert!(stranger.refused_videos.is_empty());
}

#[test]
fn a_song_refusal_outlives_a_restart_and_old_installs_load_without_the_file() {
    let (dir, path) = temp_path("octo-ids.json");
    let first = ExternalIdRegistry::new(Some(&path));
    first.refuse_song_video("KILLY", "Killamonjaro", "summer-cem");
    first.flush();
    drop(first);
    assert!(dir.path().join("refused-videos.json").exists());

    let reopened = ExternalIdRegistry::new(Some(&path));
    assert_eq!(
        reopened.refusals_for(&song("KILLY", "Killamonjaro")),
        ["summer-cem"]
    );

    let (_fresh, fresh_path) = temp_path("octo-ids.json");
    let fresh = ExternalIdRegistry::new(Some(&fresh_path));
    assert!(fresh.refusals_for(&song("KILLY", "Killamonjaro")).is_empty());
}

#[test]
fn a_song_with_no_artist_or_title_records_no_refusal() {
    let registry = ExternalIdRegistry::in_memory();
    assert!(!registry.refuse_song_video("", "Title", "v"));
    assert!(!registry.refuse_song_video("Artist", " ", "v"));
    assert!(ExternalIdRegistry::song_key("", "Title").is_none());
    assert!(registry.refusals_for(&song("", "Title")).is_empty());
}

#[test]
fn song_refusals_are_capped_per_song_and_overall() {
    let registry = ExternalIdRegistry::in_memory();
    for n in 0..7 {
        registry.refuse_song_video("Artist", "Title", &format!("v{n}"));
    }
    assert_eq!(
        registry.refusals_for(&song("Artist", "Title")),
        ["v2", "v3", "v4", "v5", "v6"]
    );

    let registry = ExternalIdRegistry::in_memory();
    for n in 0..=ExternalIdRegistry::MAX_REFUSED_SONGS {
        registry.refuse_song_video(&format!("Artist {n}"), "Title", "v");
    }
    assert!(registry.refusals_for(&song("Artist 0", "Title")).is_empty());
    assert_eq!(registry.refusals_for(&song("Artist 1", "Title")), ["v"]);
    assert_eq!(
        registry.refusals_for(&song(
            &format!("Artist {}", ExternalIdRegistry::MAX_REFUSED_SONGS),
            "Title"
        )),
        ["v"]
    );
}

#[test]
fn a_pick_is_stamped_only_while_the_song_plays_that_video() {
    let registry = ExternalIdRegistry::in_memory();
    let id = registry.register(song("Daft Punk", "Emotion"));
    assert!(!registry.remember_pick(&id, "vid-a", 3));
    assert!(registry.set_you_tube_id(&id, "vid-a"));
    assert!(!registry.remember_pick(&id, "vid-b", 3));
    assert!(registry.remember_pick(&id, "vid-a", 3));
    assert_eq!(registry.lookup(&id).expect("known").snapshot().video_pick, 3);

    assert!(registry.set_you_tube_id(&id, "vid-b"));
    assert_eq!(registry.lookup(&id).expect("known").snapshot().video_pick, 0);
}

#[test]
fn a_mint_with_a_video_is_named_and_a_search_remint_keeps_the_stamp() {
    let registry = ExternalIdRegistry::in_memory();
    let named = SoulseekRouting {
        you_tube_id: Some("vid-a".into()),
        ..song("Daft Punk", "Emotion")
    };
    let id = registry.register(named);
    assert_eq!(
        registry.lookup(&id).expect("known").snapshot().video_pick,
        SoulseekRouting::VIDEO_NAMED
    );

    // A pinned, stamped song minted again with no video keeps both.
    let other = registry.register(song("Daft Punk", "Around"));
    registry.set_you_tube_id(&other, "vid-b");
    registry.remember_pick(&other, "vid-b", 3);
    let again = registry.register(song("Daft Punk", "Around"));
    assert_eq!(again, other);
    let kept = registry.lookup(&other).expect("known").snapshot();
    assert_eq!((kept.you_tube_id.as_deref(), kept.video_pick), (Some("vid-b"), 3));

    // A fresh mint that names the same video keeps the stamp too.
    assert!(registry.remember_pick(&id, "vid-a", 3));
    let same = registry.register(SoulseekRouting {
        you_tube_id: Some("vid-a".into()),
        ..song("Daft Punk", "Emotion")
    });
    assert_eq!(same, id);
    assert_eq!(registry.lookup(&id).expect("known").snapshot().video_pick, 3);
}

// ---- One resolver per song id; contexts and heard records -------------------------------

fn heard_video(video: &str, minute: u32) -> HeardRecord {
    use chrono::TimeZone;
    HeardRecord {
        heard_at: chrono::Utc.with_ymd_and_hms(2026, 10, 29, 12, minute, 0).unwrap(),
        source: octo_core::fingerprint::heart_gate::HeardSource::Video {
            id: video.into(),
            size: Some(4_000_000),
            content_type: "audio/webm".into(),
            itag: None,
        },
        sample_id: None,
    }
}

fn context(user: &str, client: &str, album: &str) -> AlbumContext {
    AlbumContext {
        user: user.into(),
        client: Some(client.into()),
        catalog_album_id: album.into(),
        album: Some("Help!".into()),
        disc: Some(1),
        track: Some(13),
        total: Some(14),
        isrc: Some("GBAYE0601477".into()),
        listed_at: chrono::Utc::now(),
        ..Default::default()
    }
}

/// A song as a client holds it before the registry has seen it: its `yt|` id.
fn raw_id(artist: &str, title: &str, video: &str) -> String {
    SoulseekMetadataService::encode_external_id(&SoulseekRouting {
        you_tube_id: Some(video.into()),
        duration: Some(125),
        ..song(artist, title)
    })
}

#[test]
fn unknown_decoded_song_ids_read_by_get_song_register_nothing() {
    let registry = ExternalIdRegistry::in_memory();
    for n in 0..20_000 {
        let id = raw_id("Somebody", &format!("Song {n}"), &format!("v{n}"));
        let found = registry.resolve_song(&id, false).expect("decodes");
        assert_eq!(found.lock().title.as_deref(), Some(format!("Song {n}").as_str()));
    }
    assert_eq!(registry.count(), 0);
    assert!(registry.resolve_song("not an id", false).is_none());
}

#[test]
fn a_stream_of_a_decoded_id_and_a_heart_of_the_raw_id_meet_on_one_routing() {
    let registry = ExternalIdRegistry::in_memory();
    let raw = raw_id("The Beatles", "Yesterday", "vid-y");
    // The stream registers it and records what played.
    assert!(registry.remember_heard(&raw, "brandon", heard_video("vid-y", 1), None));
    assert_eq!(registry.count(), 1);
    // The heart holds the raw id and finds the record; so does the short id it hashes to.
    let heard = registry.heard_by(&raw, "Brandon").expect("found by the raw id");
    assert_eq!(heard.source.video_id(), Some("vid-y"));
    let decoded = SoulseekMetadataService::try_decode_external_id(Some(&raw)).expect("decodes");
    let short = ExternalIdRegistry::make_short_id(&decoded);
    let by_short = registry.resolve_song(&short, true).expect("registered");
    assert!(by_short.ptr_eq(&registry.resolve_song(&raw, true).expect("the same one")));
    assert_eq!(registry.count(), 1, "the heart's resolve registered nothing more");
}

#[test]
fn a_heard_record_written_by_id_lands_on_the_routing_a_search_registered_meanwhile() {
    let registry = ExternalIdRegistry::in_memory();
    let id = registry.register(song("The Beatles", "Yesterday"));
    // The stream holds this routing while the print is taken...
    let held = registry.lookup(&id).expect("known");
    // ...and a search mints the song again meanwhile, swapping in a new routing.
    let again = registry.register(song("The Beatles", "Yesterday"));
    assert_eq!(again, id);
    let registered = registry.lookup(&id).expect("known");
    assert!(!registered.ptr_eq(&held));

    let mut record = heard_video("vid-y", 2);
    record.sample_id = Some("sample-1".into());
    assert!(registry.remember_heard(&id, "brandon", record, None));
    let sample = registered
        .lock()
        .heard_by("brandon")
        .and_then(|r| r.sample_id.clone());
    assert_eq!(sample.as_deref(), Some("sample-1"));
    assert!(
        held.lock().heard.is_empty(),
        "nothing written to the routing the stream held"
    );
}

#[test]
fn a_re_register_carrying_an_older_heard_record_keeps_the_newer_one() {
    let registry = ExternalIdRegistry::in_memory();
    let id = registry.register(song("The Beatles", "Help!"));
    registry.remember_heard(&id, "brandon", heard_video("new", 9), None);
    registry.remember_context(&id, context("brandon", "OctoPlayer", "302127"));

    // A copy read from elsewhere (another process's decode, an old file) with an older record.
    let mut stale = song("The Beatles", "Help!");
    stale.remember_heard("brandon", heard_video("old", 3));
    stale.remember_heard("anna", heard_video("hers", 4));
    assert_eq!(registry.register(stale), id);

    let kept = registry.lookup(&id).expect("known").snapshot();
    assert_eq!(
        kept.heard_by("brandon").and_then(|r| r.source.video_id()),
        Some("new")
    );
    assert_eq!(
        kept.heard_by("anna").and_then(|r| r.source.video_id()),
        Some("hers")
    );
    assert_eq!(kept.contexts.len(), 1, "the context came across too");
}

#[test]
fn a_heard_video_swapped_away_and_refused_for_the_song_is_still_what_was_heard() {
    let registry = ExternalIdRegistry::in_memory();
    let id = registry.register(SoulseekRouting {
        you_tube_id: Some("heard".into()),
        ..song("The Beatles", "Yesterday")
    });
    registry.remember_heard(&id, "brandon", heard_video("heard", 1), None);
    // The re-check swaps the video away at the next play's first byte...
    assert!(registry.set_pending_video(&id, "heard", "better"));
    let routing = registry.lookup(&id).expect("known");
    registry.apply_pending_video(&routing);
    // ...and a YouTube download then refuses it for the song as another take.
    registry.refuse_video(&id, "heard");
    let kept = routing.snapshot();
    assert_eq!(kept.you_tube_id.as_deref(), Some("better"));
    assert!(kept.refuses_video("heard"));
    let heard = registry.heard_by(&id, "brandon").expect("kept");
    assert_eq!(heard.source.video_id(), Some("heard"));
}

#[test]
fn refusing_by_a_raw_id_reaches_the_registered_routing() {
    let registry = ExternalIdRegistry::in_memory();
    let raw = raw_id("The Beatles", "Yesterday", "vid-y");
    let routing = registry.resolve_song(&raw, true).expect("registered");
    assert!(registry.refuse_video(&raw, "vid-y"));
    assert!(routing.lock().refuses_video("vid-y"));
    assert_eq!(routing.lock().you_tube_id, None);
}

#[test]
fn contexts_survive_a_restart_and_a_song_evicted_meanwhile_is_found_again_by_its_raw_id() {
    let (_dir, path) = temp_path("external-ids.json");
    let raw = raw_id("The Beatles", "Yesterday", "vid-y");
    let id = {
        let registry = ExternalIdRegistry::new(Some(&path));
        let id = registry.resolve(&raw, true).expect("registered").id;
        assert!(registry.remember_context(&id, context("brandon", "OctoPlayer", "302127")));
        assert!(registry.remember_context(&id, context("anna", "Feishin", "1781")));
        registry.remember_heard(&raw, "brandon", heard_video("vid-y", 1), None);
        registry.flush();
        id
    };
    let restarted = ExternalIdRegistry::new(Some(&path));
    let contexts = restarted.contexts(&raw);
    let users: Vec<(&str, Option<&str>, &str)> = contexts
        .iter()
        .map(|c| (c.user.as_str(), c.client.as_deref(), c.catalog_album_id.as_str()))
        .collect();
    assert_eq!(
        users,
        [
            ("brandon", Some("OctoPlayer"), "302127"),
            ("anna", Some("Feishin"), "1781")
        ]
    );
    assert!(restarted.heard_by(&id, "brandon").is_some());

    // Evicted from the LRU by 10,000 newer songs: a write by the raw id lands on a routing
    // registered again, and the heart reads it there.
    for n in 0..ExternalIdRegistry::MAX_ENTRIES {
        restarted.register(song("Filler", &format!("{n}")));
    }
    assert!(restarted.lookup(&id).is_none(), "evicted");
    assert!(restarted.remember_heard(&raw, "brandon", heard_video("vid-y", 5), None));
    assert!(restarted.heard_by(&raw, "brandon").is_some());
}

#[test]
fn a_heard_record_is_told_to_every_listener_once_stored() {
    let registry = ExternalIdRegistry::in_memory();
    let told: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let seen = told.clone();
    registry.on_heard(Arc::new(move |id, user, _| {
        seen.lock().push((id.to_string(), user.to_string()))
    }));
    let raw = raw_id("The Beatles", "Yesterday", "vid-y");
    assert!(registry.remember_heard(&raw, " brandon ", heard_video("vid-y", 2), None));
    // An older record changes nothing and tells nobody.
    assert!(!registry.remember_heard(&raw, "brandon", heard_video("vid-y", 1), None));
    let short = registry.resolve(&raw, false).expect("registered").id;
    assert_eq!(*told.lock(), [(short, "brandon".to_string())]);
}

#[test]
fn a_play_marks_the_users_context_from_the_same_client_played() {
    let registry = ExternalIdRegistry::in_memory();
    let id = registry.register(song("The Beatles", "Yesterday"));
    registry.remember_context(&id, context("brandon", "OctoPlayer", "302127"));
    registry.remember_context(&id, context("brandon", "Feishin", "1781"));
    assert!(registry.remember_heard(
        &id,
        "brandon",
        heard_video("v", 3),
        Some(PlayedMark {
            client: Some("OctoPlayer".into()),
            catalog_album_id: None,
        })
    ));
    let contexts = registry.contexts(&id);
    assert!(contexts[0].played_at.is_some(), "{contexts:?}");
    assert!(contexts[1].played_at.is_none());
}

#[test]
fn a_song_routing_never_takes_the_catalogs_album_id_from_an_older_copy() {
    let registry = ExternalIdRegistry::in_memory();
    // A routing written before songs stopped carrying it.
    let id = registry.register(SoulseekRouting {
        external_album_id: Some("302127".into()),
        ..song("The Beatles", "Yesterday")
    });
    registry.register(song("The Beatles", "Yesterday"));
    assert_eq!(
        registry.lookup(&id).expect("known").lock().external_album_id,
        None
    );
}
