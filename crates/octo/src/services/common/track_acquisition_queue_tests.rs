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
    let request = queue
        .in_flight
        .lock()
        .get("deezer:1")
        .cloned()
        .expect("in flight");
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
    let request = queue
        .in_flight
        .lock()
        .get("deezer:1")
        .cloned()
        .expect("in flight");
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
    let shared = queue
        .in_flight
        .lock()
        .get("deezer:2")
        .cloned()
        .expect("in flight");
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
    let request = queue
        .in_flight
        .lock()
        .get("deezer:1:flac")
        .cloned()
        .expect("in flight");
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

/// With Family on, a heart that joins a request in flight with its own gate gets its member's
/// link only when its own gate passed the landed file: a refused joiner gets no link and no
/// attribution, and one that passed gets both.
#[tokio::test]
async fn a_joiner_its_own_gate_refused_gets_no_link_and_no_attribution() {
    let queue = TrackAcquisitionQueue::new();
    let first = heart_as(
        &queue,
        "song",
        "alice",
        AcquisitionTarget::Member("alice".into()),
        vec![gate("alice", AcquisitionOrigin::Heart)],
    );
    heart_as(
        &queue,
        "song",
        "bob",
        AcquisitionTarget::Member("bob".into()),
        vec![gate("bob", AcquisitionOrigin::Heart)],
    );
    heart_as(
        &queue,
        "song",
        "carol",
        AcquisitionTarget::Member("carol".into()),
        vec![gate("carol", AcquisitionOrigin::Heart)],
    );
    let request = queue.dequeue(&CancellationToken::new()).await.expect("a request");
    assert_eq!(request.pool_members(), ["alice"], "joiners wait for their gates");
    assert_eq!(request.requested_by(), ["alice"]);
    assert_eq!(request.gates().len(), 3, "each joiner is judged on its own");
    // The download's check point writes each gate's verdict.
    let ledger = first.ledger();
    ledger.record("alice", AcquisitionOrigin::Heart, GateOutcome::Accepted);
    ledger.record(
        "bob",
        AcquisitionOrigin::Heart,
        GateOutcome::NotAccepted("another mix".into()),
    );
    ledger.record("carol", AcquisitionOrigin::Heart, GateOutcome::Accepted);
    request.settle_joiners();
    assert_eq!(request.pool_members(), ["alice", "carol"]);
    assert_eq!(request.requested_by(), ["alice", "carol"]);
}

/// A joiner without a gate (the shipped checks) is recorded at once, as it always was; a joiner
/// with a gate its own check never judged the landed file by (it joined a download that ran the
/// shipped checks) is never credited with it: no link and no attribution.
#[tokio::test]
async fn a_gated_joiner_no_check_judged_is_never_credited() {
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
    let request = queue.dequeue(&CancellationToken::new()).await.expect("a request");
    assert_eq!(request.pool_members(), ["alice", "bob"]);
    request.settle_joiners();
    assert_eq!(request.pool_members(), ["alice", "bob"]);
    assert_eq!(request.requested_by(), ["alice", "bob"]);
}

/// A heart that joins a play's download claims its outcome at once, so a play whose download
/// then fails leaves the row to the heart, whose own chain goes on.
#[tokio::test]
async fn a_heart_joining_a_play_claims_its_outcome_at_once() {
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
    assert!(play.heart_joined(), "the heart owns the outcome from its join");
    assert!(play.notifies_on_failure());
    assert_eq!(play.requested_by(), ["alice"], "its name waits for its own gate");
}

/// The gates a download started with grow with everyone who joins the request while it runs: the
/// place hands the download a shared view of them.
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
        "bob",
        AcquisitionTarget::Shared,
        vec![gate("bob", AcquisitionOrigin::Heart)],
    );
    let users: Vec<String> = place
        .live_gates
        .current()
        .expect("a request's place")
        .into_iter()
        .map(|gate| gate.user)
        .collect();
    assert_eq!(users, ["alice", "bob"]);
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
