//! The star queue's turns per person, and whose library a request is for.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, HeartGate};
use octo_core::settings::DownloadSource;
use tokio_util::sync::CancellationToken;

use super::*;

fn star(queue: &TrackAcquisitionQueue, id: &str, user: &str) -> Completion {
    queue.enqueue(
        "deezer",
        id,
        true,
        false,
        true,
        None,
        true,
        Some(user),
        false,
        None,
    )
}

async fn order(queue: &TrackAcquisitionQueue, count: usize) -> Vec<String> {
    let token = CancellationToken::new();
    let mut taken = Vec::new();
    for _ in 0..count {
        let request = queue.dequeue(&token).await.expect("a request");
        taken.push(request.external_id.clone());
    }
    taken
}

/// A queue whose stars take turns per person, as while Family is on.
fn fair_queue() -> TrackAcquisitionQueue {
    TrackAcquisitionQueue::new().with_fair_turns(Arc::new(|| true))
}

#[tokio::test]
async fn one_member_cannot_starve_another() {
    let queue = fair_queue();
    for n in 1..=5 {
        star(&queue, &format!("a{n}"), "alex");
    }
    star(&queue, "s1", "sam");
    star(&queue, "w1", "Winters");
    assert_eq!(order(&queue, 7).await, ["a1", "s1", "w1", "a2", "a3", "a4", "a5"]);
}

#[tokio::test]
async fn single_requester_order_unchanged() {
    let queue = TrackAcquisitionQueue::new();
    for n in 1..=5 {
        star(&queue, &format!("a{n}"), "alex");
    }
    assert_eq!(order(&queue, 5).await, ["a1", "a2", "a3", "a4", "a5"]);

    // Nobody named is one person too.
    let unnamed = TrackAcquisitionQueue::new();
    for n in 1..=3 {
        unnamed.enqueue(
            "deezer",
            &format!("n{n}"),
            true,
            false,
            true,
            None,
            true,
            None,
            false,
            None,
        );
    }
    assert_eq!(order(&unnamed, 3).await, ["n1", "n2", "n3"]);
}

#[tokio::test]
async fn stars_still_before_plays() {
    let queue = TrackAcquisitionQueue::new();
    queue
        .try_enqueue_play("deezer", "played", DownloadSource::YouTube, Some("alex"), None)
        .expect("queued");
    star(&queue, "s1", "sam");
    star(&queue, "a1", "alex");
    assert_eq!(order(&queue, 3).await, ["s1", "a1", "played"]);
}

#[tokio::test]
async fn a_star_that_arrives_while_the_worker_waits_is_taken() {
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let waiting = {
        let queue = Arc::clone(&queue);
        tokio::spawn(async move { queue.dequeue(&CancellationToken::new()).await })
    };
    tokio::task::yield_now().await;
    star(&queue, "late", "alex");
    let taken = waiting.await.expect("joins").expect("a request");
    assert_eq!(taken.external_id, "late");
}

#[test]
fn members_share_one_download_in_flight() {
    let queue = TrackAcquisitionQueue::new();
    let alex = AcquisitionTarget::Member("alex".into());
    let sam = AcquisitionTarget::Member("sam".into());
    let first = queue.enqueue_for(
        &alex,
        false,
        "deezer",
        "1",
        true,
        false,
        true,
        None,
        true,
        Some("alex"),
        false,
        None,
    );
    let second = queue.enqueue_for(
        &sam,
        false,
        "deezer",
        "1",
        true,
        false,
        true,
        None,
        true,
        Some("sam"),
        false,
        None,
    );
    assert!(Arc::ptr_eq(&first.outcome, &second.outcome), "one job");
    let request = queue.running("deezer:1").expect("in flight");
    assert_eq!(
        request.pool_destination(),
        Some(vec!["alex".to_string(), "sam".to_string()])
    );
    assert_eq!(request.first_requester().as_deref(), Some("alex"));
}

#[test]
fn anyone_who_wants_it_shared_makes_it_shared() {
    let queue = TrackAcquisitionQueue::new();
    let alex = AcquisitionTarget::Member("alex".into());
    queue.enqueue_for(
        &alex,
        false,
        "deezer",
        "1",
        true,
        false,
        true,
        None,
        true,
        Some("alex"),
        false,
        None,
    );
    star(&queue, "1", "winters");
    let request = queue.running("deezer:1").expect("in flight");
    assert!(request.wants_shared());
    assert_eq!(request.pool_destination(), None);
    assert_eq!(request.pool_members(), ["alex"]);
}

#[test]
fn every_request_is_shared_unless_a_member_asks() {
    let request = AcquisitionRequest::new("deezer", "1", true, false, true, None, true, None);
    assert_eq!(request.place(), AcquisitionPlace::default());
    let queue = TrackAcquisitionQueue::new();
    star(&queue, "2", "winters");
    let shared = queue.running("deezer:2").expect("in flight");
    assert_eq!(shared.place().pool_members, None);
}

#[test]
fn a_real_flac_ask_is_its_own_job() {
    let queue = TrackAcquisitionQueue::new();
    let alex = AcquisitionTarget::Member("alex".into());
    let any = star(&queue, "1", "alex");
    let flac = queue.enqueue_for(
        &alex,
        true,
        "deezer",
        "1",
        true,
        false,
        true,
        Some(DownloadSource::Soulseek),
        true,
        Some("alex"),
        false,
        None,
    );
    assert!(!Arc::ptr_eq(&any.outcome, &flac.outcome));
    let request = queue.running("deezer:1:flac").expect("in flight");
    assert!(request.place().flac_only);
}

#[tokio::test]
async fn family_off_stars_keep_arrival_order() {
    let queue = TrackAcquisitionQueue::new();
    for n in 1..=3 {
        star(&queue, &format!("a{n}"), "alice");
    }
    star(&queue, "b1", "bob");
    assert_eq!(order(&queue, 4).await, ["a1", "a2", "a3", "b1"]);
}

#[tokio::test]
async fn turning_family_on_or_off_changes_the_order_of_stars_already_waiting() {
    let fair = Arc::new(AtomicBool::new(false));
    let switch = Arc::clone(&fair);
    let queue = TrackAcquisitionQueue::new().with_fair_turns(Arc::new(move || switch.load(Ordering::SeqCst)));
    for n in 1..=3 {
        star(&queue, &format!("a{n}"), "alice");
    }
    star(&queue, "b1", "bob");
    star(&queue, "a4", "alice");
    assert_eq!(order(&queue, 1).await, ["a1"]);
    fair.store(true, Ordering::SeqCst);
    assert_eq!(order(&queue, 2).await, ["a2", "b1"]);
    fair.store(false, Ordering::SeqCst);
    assert_eq!(order(&queue, 2).await, ["a3", "a4"]);
}

#[tokio::test]
async fn stars_lined_up_count_toward_the_bound() {
    let queue = TrackAcquisitionQueue::new();
    let token = CancellationToken::new();
    for n in 0..TrackAcquisitionQueue::STAR_CAPACITY {
        star(&queue, &format!("s{n}"), "alice");
    }
    // One taken: the rest move from the channel into the lines, where they still count.
    let first = queue.dequeue(&token).await.expect("a request");
    queue.release(&first);
    star(&queue, "room-for-one", "bob");
    let over = star(&queue, "one-too-many", "bob");
    assert!(over.is_completed(), "refused at once");
    assert!(over.wait().await.is_err());
    let mut taken = 0;
    while let Ok(Some(request)) =
        tokio::time::timeout(std::time::Duration::from_millis(50), queue.dequeue(&token)).await
    {
        assert_ne!(request.external_id, "one-too-many");
        taken += 1;
    }
    assert_eq!(taken, TrackAcquisitionQueue::STAR_CAPACITY);
}

// ---- Joiners with a gate of their own ---------------------------------------------------------

fn gate(user: &str, origin: AcquisitionOrigin) -> HeartGate {
    HeartGate {
        user: user.into(),
        origin,
        r: None,
        heard: None,
        heard_seconds: None,
        reading: octo_core::fingerprint::heart_gate::HeardReading::NotHeard,
        stand_in: None,
    }
}

fn heart_as(
    queue: &TrackAcquisitionQueue,
    id: &str,
    user: &str,
    target: AcquisitionTarget,
    gates: Vec<HeartGate>,
) -> Completion {
    queue.enqueue_as(
        Some(AcquisitionOrigin::Heart),
        gates,
        &target,
        false,
        "soulseek",
        id,
        true,
        false,
        true,
        None,
        true,
        Some(user),
        false,
        None,
    )
}

/// The next request the worker would take, or None when none comes within a moment.
async fn next_now(queue: &TrackAcquisitionQueue) -> Option<Arc<AcquisitionRequest>> {
    tokio::time::timeout(
        std::time::Duration::from_millis(50),
        queue.dequeue(&CancellationToken::new()),
    )
    .await
    .ok()
    .flatten()
}

/// A download that carries a sound check belongs to one person: two people's hearts for one song
/// are two requests, each with its own gate and its own library, and the second waits behind the
/// first. Once the first lands, the second runs, carrying the file that landed for its own gate to
/// judge first.
#[tokio::test]
async fn two_peoples_hearts_for_one_song_are_two_requests_the_second_behind_the_first() {
    let queue = TrackAcquisitionQueue::new();
    let alice = heart_as(
        &queue,
        "song",
        "alice",
        AcquisitionTarget::Member("alice".into()),
        vec![gate("alice", AcquisitionOrigin::Heart)],
    );
    let bob = heart_as(
        &queue,
        "song",
        "bob",
        AcquisitionTarget::Member("bob".into()),
        vec![gate("bob", AcquisitionOrigin::Heart)],
    );
    let first = next_now(&queue).await.expect("alice's request");
    let users = |request: &AcquisitionRequest| -> Vec<String> {
        request.gates().into_iter().map(|gate| gate.user).collect()
    };
    assert_eq!(users(&first), ["alice"], "no one else's gate rides on it");
    assert_eq!(first.pool_members(), ["alice"]);
    assert_eq!(first.requested_by(), ["alice"]);
    assert!(next_now(&queue).await.is_none(), "bob's waits behind alice's");
    assert!(queue.is_in_flight("soulseek", "song"));

    queue.release_landed(&first, Some("/pool/song.flac"));
    first.completion.try_set_result("/pool/song.flac");
    assert_eq!(alice.wait().await.expect("landed"), "/pool/song.flac");
    let second = next_now(&queue).await.expect("bob's own request");
    assert_eq!(users(&second), ["bob"]);
    assert_eq!(second.pool_members(), ["bob"]);
    assert_eq!(second.requested_by(), ["bob"]);
    assert_eq!(second.place().landed, ["/pool/song.flac"], "its own gate judges it first");
    assert!(!bob.is_completed());
    queue.release(&second);
    assert!(!queue.is_in_flight("soulseek", "song"));
}

/// A request with no gate (the shipped checks) shares the download in flight as it always did,
/// recorded at once; a gated heart of someone else never rides on it, and waits as its own.
#[tokio::test]
async fn requests_with_no_gate_share_and_a_gated_heart_waits_as_its_own() {
    let queue = TrackAcquisitionQueue::new();
    heart_as(
        &queue,
        "song",
        "alice",
        AcquisitionTarget::Member("alice".into()),
        Vec::new(),
    );
    heart_as(
        &queue,
        "song",
        "bob",
        AcquisitionTarget::Member("bob".into()),
        Vec::new(),
    );
    heart_as(
        &queue,
        "song",
        "carol",
        AcquisitionTarget::Member("carol".into()),
        vec![gate("carol", AcquisitionOrigin::Heart)],
    );
    let request = next_now(&queue).await.expect("a request");
    assert_eq!(request.pool_members(), ["alice", "bob"]);
    assert!(request.gates().is_empty(), "carol's gate is on her own request");
    request.settle_joiners();
    assert_eq!(request.requested_by(), ["alice", "bob"]);
    queue.release(&request);
    let carols = next_now(&queue).await.expect("carol's own request");
    assert_eq!(carols.pool_members(), ["carol"]);
    assert_eq!(carols.place().landed, Vec::<String>::new(), "nothing landed");
}

/// The same person's heart joins their own play's download and claims its outcome at once, so a
/// play whose download then fails leaves the row to the heart, whose own chain goes on; someone
/// else's heart never joins it.
#[tokio::test]
async fn a_heart_joining_its_persons_play_claims_its_outcome_at_once() {
    let queue = TrackAcquisitionQueue::new();
    let play = queue
        .try_enqueue_play_for(
            &AcquisitionTarget::Shared,
            "soulseek",
            "song",
            DownloadSource::Soulseek,
            Some("alice"),
            None,
            vec![gate("alice", AcquisitionOrigin::Play)],
        )
        .expect("queued");
    assert!(!play.heart_joined());
    heart_as(
        &queue,
        "song",
        "bob",
        AcquisitionTarget::Shared,
        vec![gate("bob", AcquisitionOrigin::Heart)],
    );
    assert!(!play.heart_joined(), "bob's heart is his own request");
    heart_as(
        &queue,
        "song",
        "alice",
        AcquisitionTarget::Shared,
        vec![gate("alice", AcquisitionOrigin::Heart)],
    );
    assert!(play.heart_joined(), "the heart owns the outcome from its join");
    assert!(play.notifies_on_failure());
    assert_eq!(play.gates().len(), 2, "her play's gate and her heart's");
}

/// The gates a download started with grow with the same person's hearts that join the request
/// while it runs: the place hands the download a shared view of them.
#[tokio::test]
async fn a_download_sees_the_gates_of_hearts_that_join_after_it_started() {
    let queue = TrackAcquisitionQueue::new();
    heart_as(
        &queue,
        "song",
        "alice",
        AcquisitionTarget::Shared,
        vec![gate("alice", AcquisitionOrigin::Import)],
    );
    let request = queue.dequeue(&CancellationToken::new()).await.expect("a request");
    let place = request.place();
    assert_eq!(place.gates.len(), 1);
    heart_as(
        &queue,
        "song",
        "alice",
        AcquisitionTarget::Shared,
        vec![gate("alice", AcquisitionOrigin::Heart)],
    );
    let origins: Vec<AcquisitionOrigin> = place
        .live_gates
        .current()
        .expect("a request's place")
        .into_iter()
        .map(|gate| gate.origin)
        .collect();
    assert_eq!(origins, [AcquisitionOrigin::Import, AcquisitionOrigin::Heart]);
}

/// A person's keep of a held file, as the hold queues it.
fn keep_as(queue: &TrackAcquisitionQueue, user: &str) -> (Completion, bool) {
    queue.enqueue_keeping(
        AcquisitionOrigin::AlbumHeart,
        vec![gate(user, AcquisitionOrigin::AlbumHeart)],
        vec![format!("/held/{user}.flac")],
        &AcquisitionTarget::Shared,
        "soulseek",
        "song",
        Some(DownloadSource::Soulseek),
        Some(user),
    )
}

/// A keep never shares a download with a request that is not one: a keep for a song with another
/// person's heart queued waits behind it as its own, and never turns that heart into a keep.
#[tokio::test]
async fn a_keep_for_a_song_with_anothers_queued_heart_never_turns_that_heart_into_a_keep() {
    let queue = TrackAcquisitionQueue::new();
    heart_as(
        &queue,
        "song",
        "bob",
        AcquisitionTarget::Shared,
        vec![gate("bob", AcquisitionOrigin::Heart)],
    );
    let (_, joined) = keep_as(&queue, "alice");
    assert!(!joined, "the keep is its own request");
    let heart = next_now(&queue).await.expect("bob's heart");
    let place = heart.place();
    assert!(!place.keep_anyway, "bob's heart is no keep");
    assert!(place.keep_for.is_empty());
    assert!(place.held_files.is_empty(), "the keep's files are its own");
    assert_eq!(place.gates.len(), 1);
    queue.release(&heart);
    let keep = next_now(&queue).await.expect("alice's keep");
    let place = keep.place();
    assert!(place.keep_anyway);
    assert_eq!(place.keep_for, ["alice"]);
    assert_eq!(place.held_files, ["/held/alice.flac"]);
}

/// A heart of another person for a song a keep is running for waits as its own: it never rides on
/// the keep, whose file counts as kept only for its keeper. Another keep of the same person shares
/// it.
#[tokio::test]
async fn a_heart_of_another_person_never_rides_on_a_running_keep() {
    let queue = TrackAcquisitionQueue::new();
    let (keep, _) = keep_as(&queue, "alice");
    let running = next_now(&queue).await.expect("alice's keep");
    let bob = heart_as(
        &queue,
        "song",
        "bob",
        AcquisitionTarget::Shared,
        vec![gate("bob", AcquisitionOrigin::Heart)],
    );
    let users: Vec<String> = running.gates().into_iter().map(|gate| gate.user).collect();
    assert_eq!(users, ["alice"], "bob's gate is not on the keep");
    assert_eq!(running.place().keep_for, ["alice"]);
    let (_, joined) = keep_as(&queue, "ALICE");
    assert!(joined, "her own second keep shares it");
    assert_eq!(running.place().keep_for, ["alice"]);

    queue.release_landed(&running, Some("/music/kept.flac"));
    running.completion.try_set_result("/music/kept.flac");
    assert!(keep.wait().await.is_ok());
    assert!(!bob.is_completed(), "bob's heart was never answered by the keep");
    let theirs = next_now(&queue).await.expect("bob's own request");
    assert!(!theirs.place().keep_anyway);
    assert_eq!(theirs.place().landed, ["/music/kept.flac"]);
}

/// An AcoustID answer whose recordings are these (id, title, artist, submissions), every one at
/// the match score.
fn answer_of(recordings: &[(&str, &str, &str, i64)]) -> AcoustIdAnswer {
    let doc = serde_json::json!({"status": "ok", "results": [{
        "id": "fp", "score": 0.97,
        "recordings": recordings.iter().map(|(id, title, artist, sources)| serde_json::json!({
            "id": id, "title": title, "artists": [{"name": artist}], "sources": sources,
        })).collect::<Vec<_>>(),
    }]});
    octo_core::fingerprint::acoust_id_client::parse_answer(&doc)
        .expect("parses")
        .expect("an answer")
}

/// A Wrong song or Wrong version replacement refuses a file whose dominant recording is its
/// original's clear recording or one of that recording's same-note duplicates, and for a Wrong song
/// whose original read ambiguous, any recording of the original's song; any other file stands,
/// whatever else its answer names.
#[test]
fn a_replacement_refuses_its_originals_recording_or_song_again_by_the_dominant_recording() {
    let recordings = SoundDeny::Recordings(HeardRecording {
        ids: vec!["help-2015".into(), "help-2015-again".into()],
        duplicates: vec!["help-2015".into(), "help-2015-again".into()],
        ..Default::default()
    });
    let min = 0.8;
    for dominant in ["help-2015", "HELP-2015-AGAIN"] {
        let file = answer_of(&[
            (dominant, "Help!", "The Beatles", 60),
            ("help-2009", "Help!", "The Beatles", 5),
        ]);
        assert_eq!(
            recordings.refuses(&file, min),
            Some(SAME_RECORDING_AGAIN),
            "{dominant}"
        );
    }
    let other_mix = answer_of(&[
        ("help-2009", "Help!", "The Beatles", 60),
        ("help-2015", "Help!", "The Beatles", 5),
    ]);
    assert_eq!(recordings.refuses(&other_mix, min), None, "another mix stands");
    assert_eq!(
        recordings.refuses(&answer_of(&[]), min),
        None,
        "an unknown file stands"
    );

    let song = SoundDeny::Song(octo_core::fingerprint::verification::SongName {
        title: "Yellow Submarine".into(),
        artists: vec!["The Beatles".into()],
        artist_ids: Vec::new(),
    });
    let again = answer_of(&[(
        "ys-remaster",
        "Yellow Submarine (Remastered 2009)",
        "The Beatles",
        40,
    )]);
    assert_eq!(song.refuses(&again, min), Some(SAME_SONG_AGAIN));
    let wanted = answer_of(&[("help-2009", "Help!", "The Beatles", 60)]);
    assert_eq!(song.refuses(&wanted, min), None, "the song asked for stands");
}
