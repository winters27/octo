//! A keep its person asked for while their heard video does not answer stays theirs through the
//! shim's rounds: when the video stays busy for three rounds and they move to wait for a new play,
//! their keep moves with them, into a hold another person already waits in too, and nobody
//! else's keep moves with it.

use super::*;

#[tokio::test]
async fn a_keep_asked_for_goes_with_its_person_when_their_video_stays_busy() {
    let shim = MockServer::start().await;
    facts(&shim, PROBE, fresh_ok()).await;
    facts(&shim, HEARD, fresh_busy()).await;
    let dir = tempfile::tempdir().unwrap();
    let holds = memory_holds();
    let rig = rig_in(
        dir.path(),
        &shim.uri(),
        holds.clone(),
        SCRATCH_FILES,
        SCRATCH_BYTES,
    );
    // ben already waits for a play of the song.
    holds.hold(HeldAcquisition {
        kind: HeldKind::Track,
        provider: "soulseek".into(),
        external_id: "song".into(),
        requested_by: Some("ben".into()),
        held_since_utc: Utc::now(),
        origin: Some(AcquisitionOrigin::Heart),
        waiting_for: Some(WaitingFor::NotYetProven),
        holders: vec![Holder {
            user: "ben".into(),
            ..Default::default()
        }],
        ..Default::default()
    });
    let key = shim_hold(&holds, "song", "anna", HEARD);
    holds
        .update(&key, |record| {
            record.keep_requested = vec!["anna".into(), "carl".into()];
        })
        .expect("held");
    rig.samples.wait_for_shim(&key, "anna", HEARD);

    for _ in 0..STUCK_ROUNDS {
        one_round(&rig.samples).await;
    }

    assert!(holds.get(&key).is_none(), "the fetch attempt ended");
    let moved = holds
        .get("Track:soulseek:song:NotYetProven")
        .expect("waiting for a new play");
    let users: Vec<&str> = moved.holders.iter().map(|holder| holder.user.as_str()).collect();
    assert_eq!(users, ["ben", "anna"]);
    assert_eq!(moved.keep_requested, ["anna"], "hers alone moved with her");
    assert_eq!(moved.line.as_deref(), Some(STUCK_LINE));
}
