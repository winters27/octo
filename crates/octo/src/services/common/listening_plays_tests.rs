use std::time::Duration;

use super::*;

const PHONE: &str = "alice|10.0.0.5|octo";
const DESK: &str = "alice|10.0.0.9|octo";

fn plays() -> (Arc<ListeningPlays>, HandClock) {
    let plays = Arc::new(ListeningPlays::default());
    let clock = plays.hand_clock();
    (plays, clock)
}

/// Lets the waiting tasks run, with no time passing on the hand clock.
async fn settle() {
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
}

#[test]
fn a_player_is_the_listener_the_address_and_the_app() {
    let address: IpAddr = "10.0.0.5".parse().unwrap();
    assert_eq!(
        ListeningPlays::player_key(" Alice ", Some(address), "Octo"),
        PHONE
    );
    assert_eq!(ListeningPlays::player_key("alice", None, "DSub"), "alice||dsub");
}

#[tokio::test]
async fn the_download_starts_once_the_song_has_played_long_enough() {
    let (plays, clock) = plays();
    let ticket = plays.hold(PHONE, "ext-1", Duration::from_secs(30)).unwrap();
    let waited = tokio::spawn(ticket.listened());
    clock.advance(29);
    settle().await;
    assert!(!waited.is_finished(), "29 seconds is not 30");
    clock.advance(1);
    assert!(waited.await.unwrap());
    assert!(plays.is_empty());
}

#[tokio::test]
async fn another_song_on_the_same_player_drops_the_wait_but_not_one_on_another_player() {
    let (plays, clock) = plays();
    let phone = tokio::spawn(
        plays
            .hold(PHONE, "ext-1", Duration::from_secs(30))
            .unwrap()
            .listened(),
    );
    let desk = tokio::spawn(
        plays
            .hold(DESK, "ext-1", Duration::from_secs(30))
            .unwrap()
            .listened(),
    );
    clock.advance(10);
    // The same song again (a seek back to the start, or its now-playing) keeps the wait.
    plays.started(PHONE, "ext-1");
    plays.scrobbled(PHONE, "ext-1", false);
    assert_eq!(plays.waiting_song(PHONE).as_deref(), Some("ext-1"));
    plays.started(PHONE, "nd-42");
    assert!(
        !phone.await.unwrap(),
        "dropped at once, without waiting out the time"
    );
    clock.advance(20);
    assert!(desk.await.unwrap());
}

#[tokio::test]
async fn a_now_playing_scrobble_of_another_song_drops_the_wait() {
    let (plays, _clock) = plays();
    let waited = tokio::spawn(
        plays
            .hold(PHONE, "ext-1", Duration::from_secs(30))
            .unwrap()
            .listened(),
    );
    plays.scrobbled(PHONE, "ext-2", false);
    assert!(!waited.await.unwrap());
}

#[tokio::test]
async fn a_finish_soon_after_the_start_is_a_skip_and_a_later_one_starts_the_download() {
    let (plays, clock) = plays();
    let skipped = tokio::spawn(
        plays
            .hold(PHONE, "ext-1", Duration::from_secs(120))
            .unwrap()
            .listened(),
    );
    clock.advance(12);
    plays.scrobbled(PHONE, "ext-1", true);
    assert!(!skipped.await.unwrap());

    let played = tokio::spawn(
        plays
            .hold(PHONE, "ext-2", Duration::from_secs(120))
            .unwrap()
            .listened(),
    );
    clock.advance(45);
    // A finish of another song changes nothing.
    plays.scrobbled(PHONE, "ext-9", true);
    settle().await;
    assert!(!played.is_finished());
    plays.scrobbled(PHONE, "ext-2", true);
    assert!(
        played.await.unwrap(),
        "finished past the skip window: no more waiting"
    );
}

#[tokio::test]
async fn a_song_started_again_keeps_its_first_plays_time() {
    let (plays, clock) = plays();
    let waited = tokio::spawn(
        plays
            .hold(PHONE, "ext-1", Duration::from_secs(30))
            .unwrap()
            .listened(),
    );
    clock.advance(20);
    assert!(plays.hold(PHONE, "ext-1", Duration::from_secs(30)).is_none());
    clock.advance(10);
    assert!(waited.await.unwrap());
}
