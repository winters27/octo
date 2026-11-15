use super::*;

/// The shim, as a table: what /facts and /search answer, and what was asked.
#[derive(Default)]
struct FakeShim {
    facts: HashMap<String, StreamFacts>,
    hits: HashMap<String, YouTubeHit>,
    asked: Mutex<Vec<String>>,
}

#[async_trait]
impl StreamFactsSource for FakeShim {
    async fn facts(&self, video_id: &str) -> Option<StreamFacts> {
        self.asked.lock().push(format!("facts {video_id}"));
        self.facts.get(video_id).cloned()
    }

    async fn search(&self, query: &str, duration_hint: Option<i32>) -> Option<YouTubeHit> {
        self.asked
            .lock()
            .push(format!("search {query} {duration_hint:?}"));
        self.hits.get(query).cloned()
    }
}

fn song(artist: &str, title: &str) -> SoulseekRouting {
    SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some(artist.into()),
        title: Some(title.into()),
        duration: Some(417),
        ..Default::default()
    }
}

fn sized(size: u64) -> StreamFacts {
    StreamFacts {
        size: Some(size),
        itag: Some("140".into()),
        ext: Some("m4a".into()),
        bit_rate: Some(129),
        ..Default::default()
    }
}

fn warmer_over(shim: FakeShim) -> (Arc<ExternalIdRegistry>, Arc<FakeShim>, StreamSizeWarmer) {
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let shim = Arc::new(shim);
    let warmer = StreamSizeWarmer::new(registry.clone(), shim.clone());
    (registry, shim, warmer)
}

#[test]
fn the_rate_window_lets_thirty_a_minute_start() {
    let t0 = Instant::now();
    let mut window = RateWindow::new(30);
    for i in 0..30 {
        assert_eq!(
            window.wait_before_next(t0 + Duration::from_millis(i as u64)),
            None,
            "start {i}"
        );
    }
    // The 31st in the same minute waits until the first start is a minute old.
    let at = t0 + Duration::from_secs(10);
    assert_eq!(window.wait_before_next(at), Some(Duration::from_secs(50)));
    assert_eq!(window.wait_before_next(t0 + Duration::from_secs(60)), None);
}

#[tokio::test]
async fn a_pinned_song_is_sized_from_its_videos_facts() {
    let mut shim = FakeShim::default();
    shim.facts.insert("vid-a".into(), sized(6_755_585));
    let (registry, shim, warmer) = warmer_over(shim);
    let id = registry.register(song("Daft Punk", "Emotion"));
    registry.set_you_tube_id(&id, "vid-a");

    assert!(warmer.want(&id, true));
    warmer.warm_one(&id).await;

    assert_eq!(registry.stream_facts(&id), Some(sized(6_755_585)));
    assert_eq!(*shim.asked.lock(), vec!["facts vid-a".to_string()]);
}

#[tokio::test]
async fn an_unpinned_song_gets_the_video_a_play_would_pick_and_its_size() {
    let mut shim = FakeShim::default();
    shim.hits.insert(
        "Daft Punk Emotion".into(),
        YouTubeHit {
            video_id: "vid-b".into(),
            duration: Some(416),
            stream: sized(7_000_000),
            ..Default::default()
        },
    );
    let (registry, shim, warmer) = warmer_over(shim);
    let id = registry.register(song("Daft Punk", "Emotion"));

    warmer.warm_one(&id).await;

    let r = registry.lookup(&id).expect("known").snapshot();
    assert_eq!(r.you_tube_id.as_deref(), Some("vid-b"));
    assert_eq!(r.duration, Some(416));
    assert_eq!(registry.stream_facts(&id).and_then(|f| f.size), Some(7_000_000));
    // The same query and length hint the play path searches with.
    assert_eq!(
        *shim.asked.lock(),
        vec!["search Daft Punk Emotion Some(417)".to_string()]
    );
}

#[tokio::test]
async fn the_warmer_leaves_a_refused_video_alone() {
    let mut shim = FakeShim::default();
    shim.hits.insert(
        "Daft Punk Emotion".into(),
        YouTubeHit {
            video_id: "vid-b".into(),
            duration: Some(416),
            stream: sized(7_000_000),
            ..Default::default()
        },
    );
    let (registry, _shim, warmer) = warmer_over(shim);
    let id = registry.register(song("Daft Punk", "Emotion"));
    assert!(registry.refuse_video(&id, "vid-b"));

    assert_eq!(warmer.warm_one(&id).await, Some(false));

    let r = registry.lookup(&id).expect("known").snapshot();
    assert_eq!(r.you_tube_id, None);
    assert_eq!(r.duration, Some(417));
    assert_eq!(r.shown_duration, None);
    assert_eq!(registry.stream_facts(&id), None);
}

#[tokio::test]
async fn sized_unknown_and_recently_tried_songs_are_not_queued_twice() {
    let (registry, _shim, warmer) = warmer_over(FakeShim::default());
    let id = registry.register(song("Daft Punk", "Emotion"));
    assert!(warmer.want(&id, true));
    // Already waiting.
    assert!(!warmer.want(&id, true));
    // Tried and given no answer: left alone for an hour.
    warmer.done(&id, None);
    assert!(!warmer.want(&id, true));

    let sized_id = registry.register(song("Justice", "Genesis"));
    registry.set_you_tube_id(&sized_id, "vid-c");
    registry.remember_stream(&sized_id, "vid-c", &sized(1));
    assert!(!warmer.want(&sized_id, true));
    assert!(!warmer.want("unknown", true));
    let artist = registry.register(SoulseekRouting {
        kind: RoutingKind::Artist,
        artist: Some("Justice".into()),
        ..Default::default()
    });
    assert!(!warmer.want(&artist, true));
}

#[test]
fn a_full_queue_drops_new_rows() {
    let (registry, _shim, warmer) = warmer_over(FakeShim::default());
    for i in 0..QUEUE_CAPACITY {
        let id = registry.register(song("Artist", &format!("Song {i}")));
        assert!(warmer.want(&id, true), "row {i}");
    }
    let late = registry.register(song("Artist", "One too many"));
    assert!(!warmer.want(&late, true));
    // Dropped, not remembered: it can be queued again once there is room.
    assert!(!warmer.queued.lock().contains(&late));
}

#[tokio::test]
async fn the_worker_sizes_what_was_queued_and_stops_on_shutdown() {
    let mut shim = FakeShim::default();
    shim.facts.insert("vid-a".into(), sized(42));
    let (registry, _shim, warmer) = warmer_over(shim);
    let warmer = Arc::new(warmer);
    let id = registry.register(song("Daft Punk", "Emotion"));
    registry.set_you_tube_id(&id, "vid-a");
    warmer.want(&id, true);

    let token = CancellationToken::new();
    let worker = tokio::spawn(warmer.clone().run(token.clone()));
    tokio::time::timeout(Duration::from_secs(10), async {
        while registry.stream_facts(&id).is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("sized");
    token.cancel();
    worker.await.expect("joins").expect("ends cleanly");
}

#[tokio::test]
async fn a_played_song_asks_for_its_measured_loudness_even_with_its_size_known() {
    let mut shim = FakeShim::default();
    shim.facts.insert(
        "vid-a".into(),
        StreamFacts {
            track_gain: Some(-10.0),
            track_peak: Some(1.135011),
            ..sized(6_755_585)
        },
    );
    let (registry, shim, warmer) = warmer_over(shim);
    let id = registry.register(song("Daft Punk", "Emotion"));
    registry.set_you_tube_id(&id, "vid-a");
    registry.remember_stream(&id, "vid-a", &sized(6_755_585));

    // Sized: a listing alone asks nothing.
    assert!(!warmer.want(&id, true));
    assert!(warmer.want_loudness(&id));
    warmer.warm_one(&id).await;

    let facts = registry.stream_facts(&id).expect("facts");
    assert_eq!(
        (facts.track_gain, facts.track_peak),
        (Some(-10.0), Some(1.135011))
    );
    assert_eq!(*shim.asked.lock(), vec!["facts vid-a".to_string()]);
    // Known now: a later play asks nothing more.
    warmer.done(&id, None);
    assert!(!warmer.want_loudness(&id));
}

#[test]
fn a_song_with_no_pinned_video_has_no_loudness_to_ask_for() {
    let (registry, _shim, warmer) = warmer_over(FakeShim::default());
    let id = registry.register(song("Daft Punk", "Emotion"));
    assert!(!warmer.want_loudness(&id));
    assert!(!warmer.want_loudness("unknown"));
}

fn hit_of(duration: i32) -> YouTubeHit {
    YouTubeHit {
        video_id: "vid-c".into(),
        duration: Some(duration),
        stream: sized(48_000),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_stream_far_from_the_catalog_length_never_replaces_it() {
    let mut shim = FakeShim::default();
    shim.hits.insert("Velvet Static Velvet".into(), hit_of(3));
    let (registry, _shim, warmer) = warmer_over(shim);
    let id = registry.register(SoulseekRouting {
        duration: Some(201),
        ..song("Velvet Static", "Velvet")
    });

    warmer.warm_one(&id).await;

    let r = registry.lookup(&id).expect("known").snapshot();
    assert_eq!(r.you_tube_id.as_deref(), Some("vid-c"));
    assert_eq!(r.duration, Some(201));
}

#[tokio::test]
async fn a_stream_length_fills_an_unknown_length() {
    let mut shim = FakeShim::default();
    shim.hits.insert("Velvet Static Velvet".into(), hit_of(200));
    let (registry, _shim, warmer) = warmer_over(shim);
    let id = registry.register(SoulseekRouting {
        duration: None,
        ..song("Velvet Static", "Velvet")
    });

    warmer.warm_one(&id).await;

    assert_eq!(
        registry.lookup(&id).expect("known").snapshot().duration,
        Some(200)
    );
}

#[tokio::test]
async fn a_clip_length_never_fills_an_unknown_length() {
    let mut shim = FakeShim::default();
    shim.hits.insert("Velvet Static Velvet".into(), hit_of(3));
    let (registry, _shim, warmer) = warmer_over(shim);
    let id = registry.register(SoulseekRouting {
        duration: None,
        ..song("Velvet Static", "Velvet")
    });

    warmer.warm_one(&id).await;

    assert_eq!(registry.lookup(&id).expect("known").snapshot().duration, None);
}

fn pinned(registry: &ExternalIdRegistry, title: &str) -> String {
    let id = registry.register(song("Artist", title));
    registry.set_you_tube_id(&id, &format!("vid-{title}"));
    id
}

/// Lists `count` outside rows in one answer, every other one pinned to a video, none sized.
async fn list_rows(
    warmer: &Arc<StreamSizeWarmer>,
    registry: &ExternalIdRegistry,
    count: usize,
) -> Vec<String> {
    let ids: Vec<String> = (0..count)
        .map(|i| {
            if i % 2 == 0 {
                pinned(registry, &format!("Pinned {i}"))
            } else {
                registry.register(song("Artist", &format!("Unpinned {i}")))
            }
        })
        .collect();
    warmer
        .listing(async {
            for id in &ids {
                warmer.outside_listed(id);
                warmer.size_wanted(id);
            }
        })
        .await;
    ids
}

#[tokio::test]
async fn a_short_answer_may_search_for_its_unpinned_songs() {
    let (registry, _shim, warmer) = warmer_over(FakeShim::default());
    let warmer = Arc::new(warmer);
    let ids = list_rows(&warmer, &registry, SEARCH_LISTING_MAX).await;
    let queued = warmer.queued.lock().clone();
    assert!(ids.iter().all(|id| queued.contains(id)), "every row queued");
}

#[tokio::test]
async fn a_long_answer_starts_no_searches_but_still_sizes_its_pinned_songs() {
    let (registry, _shim, warmer) = warmer_over(FakeShim::default());
    let warmer = Arc::new(warmer);
    let ids = list_rows(&warmer, &registry, SEARCH_LISTING_MAX + 1).await;
    let queued = warmer.queued.lock().clone();
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(queued.contains(id), i % 2 == 0, "row {i}");
    }
}

#[test]
fn a_row_listed_outside_any_answer_never_starts_a_search() {
    let (registry, _shim, warmer) = warmer_over(FakeShim::default());
    let unpinned = registry.register(song("Daft Punk", "Emotion"));
    let pinned = pinned(&registry, "Genesis");
    warmer.size_wanted(&unpinned);
    warmer.size_wanted(&pinned);
    let queued = warmer.queued.lock().clone();
    assert!(!queued.contains(&unpinned));
    assert!(queued.contains(&pinned));
}

#[test]
fn zero_a_minute_turns_the_warmer_off() {
    let (registry, _shim, warmer) = warmer_over(FakeShim::default());
    let warmer = warmer.with_per_minute(Arc::new(|| 0));
    let id = pinned(&registry, "Genesis");
    assert!(!warmer.want(&id, true));
    assert!(!warmer.want_loudness(&id));
}

#[test]
fn five_a_minute_by_default() {
    let (_registry, _shim, warmer) = warmer_over(FakeShim::default());
    assert_eq!(warmer.per_minute(), 5);
}

#[test]
fn five_failed_or_empty_lookups_in_a_row_pause_the_warmer_for_thirty_minutes() {
    let t0 = Instant::now();
    let mut breaker = Breaker::default();
    for _ in 0..BREAKER_MISSES - 1 {
        breaker.record(false, t0);
    }
    assert_eq!(breaker.paused_for(t0), None);
    breaker.record(false, t0);
    assert_eq!(BREAKER_MISSES, 5);
    assert_eq!(BREAKER_PAUSE, Duration::from_secs(30 * 60));
    assert_eq!(breaker.paused_for(t0), Some(BREAKER_PAUSE));
    assert_eq!(breaker.paused_for(t0 + BREAKER_PAUSE), None);
}

#[test]
fn a_lookup_that_answers_starts_the_count_of_misses_again() {
    let t0 = Instant::now();
    let mut breaker = Breaker::default();
    for _ in 0..BREAKER_MISSES - 1 {
        breaker.record(false, t0);
    }
    breaker.record(true, t0);
    for _ in 0..BREAKER_MISSES - 1 {
        breaker.record(false, t0);
    }
    assert_eq!(breaker.paused_for(t0), None);
}

#[tokio::test]
async fn lookups_that_come_back_empty_trip_the_breaker() {
    let (registry, _shim, warmer) = warmer_over(FakeShim::default());
    for i in 0..BREAKER_MISSES {
        let id = pinned(&registry, &format!("Song {i}"));
        let answered = warmer.warm_one(&id).await;
        warmer.done(&id, answered);
    }
    assert!(warmer.paused_for(Instant::now()).is_some());
}

#[tokio::test]
async fn a_play_during_a_lookup_still_gets_its_loudness_asked_for() {
    let mut shim = FakeShim::default();
    shim.facts.insert("vid-Genesis".into(), sized(42));
    let (registry, _shim, warmer) = warmer_over(shim);
    let id = pinned(&registry, "Genesis");
    assert!(warmer.want(&id, true));

    let answered = warmer.warm_one(&id).await;
    // Played while that lookup ran: the waiting entry is the one running.
    assert!(warmer.want_loudness(&id));
    warmer.done(&id, answered);

    assert!(
        warmer.queued.lock().contains(&id),
        "queued again for its loudness"
    );
}

#[test]
fn a_song_waiting_for_its_chosen_video_is_neither_warmed_nor_listed_with_the_old_size() {
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let id = registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("Drake".into()),
        title: Some("Started".into()),
        ..Default::default()
    });
    registry.set_you_tube_id(&id, "old");
    registry.remember_stream(
        &id,
        "old",
        &StreamFacts {
            itag: Some("140".into()),
            ..Default::default()
        },
    );
    registry
        .choose_stream_video(
            &id,
            crate::services::soulseek::StreamChoice {
                video: "new".into(),
                by: "ede".into(),
                at: chrono::Utc::now(),
                ..Default::default()
            },
        )
        .expect("stored");
    let warmer = StreamSizeWarmer::new(Arc::clone(&registry), Arc::new(FakeShim::default()));
    assert!(!warmer.want(&id, true));
    assert_eq!(IdRegistry::stream_facts(&warmer, &id), None);
    assert!(
        registry.stream_facts(&id).is_some(),
        "the old video's facts are still on the routing"
    );
}
