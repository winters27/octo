//! When a listener's stations are due a rebuild.

use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use sha2::{Digest, Sha256};

use crate::common::dotnet;
use crate::models::radio::LastFmRadioUserState;
use crate::settings::LastFmSettings;

/// How long after a failed refresh, or one that kept stations as they were, the next try waits.
pub const FAILURE_RETRY_DELAY: TimeDelta = TimeDelta::minutes(15);

/// A refresh still marked as running this long after it started has died (a refresh gives up
/// well within a minute), so it no longer holds the next one back.
pub const REFRESH_PRESUMED_DEAD: TimeDelta = TimeDelta::minutes(5);

/// No successful refresh yet, or the last one is older than the refresh interval.
pub fn is_stale(user: &LastFmRadioUserState, settings: &LastFmSettings, now_utc: DateTime<Utc>) -> bool {
    match user.last_refresh_success_utc {
        None => true,
        Some(success) => {
            success < now_utc - TimeDelta::hours(i64::from(settings.effective_refresh_interval_hours()))
        }
    }
}

/// After a recorded play: no stations yet, stale, or the play that makes the profile learned.
/// Plays alone never rebuild stations that are still fresh: a rebuild draws every station again,
/// and an app that holds a station as a playlist would have it change under the listener.
pub fn should_refresh_after_play(
    user: &LastFmRadioUserState,
    settings: &LastFmSettings,
    now_utc: DateTime<Utc>,
) -> bool {
    let learned_count = user.plays.iter().filter(|play| play.learned_signal).count() as i32;
    user.stations.is_empty()
        || is_stale(user, settings, now_utc)
        || learned_count == settings.effective_minimum_plays()
}

/// Whether a refresh is running now: marked as running, and started recently enough to still be.
pub fn is_refreshing(user: &LastFmRadioUserState, now_utc: DateTime<Utc>) -> bool {
    user.refreshing
        && user
            .last_refresh_attempt_utc
            .is_some_and(|started| started > now_utc - REFRESH_PRESUMED_DEAD)
}

/// The minute scan: stale and not refreshing, and a failed refresh is retried only after a
/// quarter of an hour.
pub fn should_schedule_periodic_refresh(
    user: &LastFmRadioUserState,
    settings: &LastFmSettings,
    now_utc: DateTime<Utc>,
) -> bool {
    if is_refreshing(user, now_utc) || !is_stale(user, settings, now_utc) {
        return false;
    }
    user.last_refresh_error.is_none()
        || user
            .last_refresh_attempt_utc
            .is_none_or(|attempt| attempt <= now_utc - FAILURE_RETRY_DELAY)
}

/// The minute scan, for stations the last refresh kept as they were because their lookups did
/// not answer: some are kept, radio and at least one kind of station are on, nothing is
/// refreshing, and the last try was at least a quarter of an hour ago.
pub fn should_retry_carried(
    user: &LastFmRadioUserState,
    settings: &LastFmSettings,
    now_utc: DateTime<Utc>,
) -> bool {
    !user.carried_station_keys.is_empty()
        && settings.enable_radio
        && (settings.enable_personalized_stations || settings.enable_discovery_stations)
        && !is_refreshing(user, now_utc)
        && user
            .last_refresh_attempt_utc
            .is_none_or(|attempt| attempt <= now_utc - FAILURE_RETRY_DELAY)
}

/// A deterministic 100–499 ms per listener, so a restart does not refresh every profile at once.
pub fn startup_jitter(username: &str) -> Duration {
    let hash = Sha256::digest(dotnet::to_lower_invariant(username.trim()).as_bytes());
    let value = u16::from_le_bytes([hash[0], hash[1]]);
    Duration::from_millis(100 + u64::from(value % 400))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::radio::{LastFmRadioPlay, LastFmRadioStation};

    #[test]
    fn policy_covers_threshold_new_play_staleness_and_deterministic_startup_jitter() {
        let settings = LastFmSettings {
            minimum_plays: 10,
            refresh_interval_hours: 12,
            ..Default::default()
        };
        let now = Utc::now();
        let mut user = LastFmRadioUserState {
            plays: (0..9)
                .map(|index| LastFmRadioPlay {
                    artist: "A".into(),
                    title: format!("T{index}"),
                    learned_signal: true,
                    ..Default::default()
                })
                .collect(),
            stations: vec![LastFmRadioStation::default()],
            last_refresh_success_utc: Some(now),
            new_plays_since_refresh: 4,
            ..Default::default()
        };
        assert!(!should_refresh_after_play(&user, &settings, now));
        user.new_plays_since_refresh = 50;
        assert!(
            !should_refresh_after_play(&user, &settings, now),
            "plays alone never redraw fresh stations"
        );
        user.new_plays_since_refresh = 0;
        user.plays.push(LastFmRadioPlay {
            artist: "A".into(),
            title: "threshold".into(),
            learned_signal: true,
            ..Default::default()
        });
        assert!(should_refresh_after_play(&user, &settings, now));
        user.plays.pop();
        user.last_refresh_success_utc = Some(now - TimeDelta::hours(13));
        assert!(is_stale(&user, &settings, now));
        assert!(should_schedule_periodic_refresh(&user, &settings, now));
        user.refreshing = true;
        user.last_refresh_attempt_utc = Some(now - TimeDelta::seconds(30));
        assert!(!should_schedule_periodic_refresh(&user, &settings, now));
        user.last_refresh_attempt_utc = None;
        assert!(should_schedule_periodic_refresh(&user, &settings, now));
        user.refreshing = false;
        user.last_refresh_error = Some("provider unavailable".into());
        user.last_refresh_attempt_utc = Some(now - TimeDelta::minutes(5));
        assert!(!should_schedule_periodic_refresh(&user, &settings, now));
        user.last_refresh_attempt_utc = Some(now - TimeDelta::minutes(16));
        assert!(should_schedule_periodic_refresh(&user, &settings, now));
        let jitter = startup_jitter("Alice");
        assert_eq!(jitter, startup_jitter("alice"));
        assert!((100..=499).contains(&jitter.as_millis()));
    }

    /// Kept stations are tried again a quarter of an hour after the last try, and only while
    /// radio and a kind of station are on.
    #[test]
    fn carried_stations_are_tried_again_a_quarter_of_an_hour_after_the_last_try() {
        let settings = LastFmSettings::default();
        let now = Utc::now();
        let mut user = LastFmRadioUserState {
            carried_station_keys: vec!["discovery".into()],
            last_refresh_attempt_utc: Some(now - TimeDelta::minutes(14)),
            ..Default::default()
        };
        assert!(!should_retry_carried(&user, &settings, now));
        user.last_refresh_attempt_utc = Some(now - TimeDelta::minutes(15));
        assert!(should_retry_carried(&user, &settings, now));
        for off in [
            LastFmSettings {
                enable_radio: false,
                ..settings.clone()
            },
            LastFmSettings {
                enable_personalized_stations: false,
                enable_discovery_stations: false,
                ..settings.clone()
            },
        ] {
            assert!(!should_retry_carried(&user, &off, now));
        }
        let pinned_only = LastFmSettings {
            enable_personalized_stations: false,
            ..settings.clone()
        };
        assert!(should_retry_carried(&user, &pinned_only, now));
        user.carried_station_keys.clear();
        assert!(!should_retry_carried(&user, &settings, now));
    }

    /// Through a day of listening a station is drawn again on the refresh interval and on
    /// nothing else; through a day of silence it is drawn again on the same schedule.
    #[test]
    fn stations_are_drawn_again_on_the_interval_listening_or_not() {
        let settings = LastFmSettings {
            minimum_plays: 10,
            refresh_interval_hours: 12,
            ..Default::default()
        };
        let start = Utc::now();
        let user = |last: DateTime<Utc>, plays: i32| LastFmRadioUserState {
            stations: vec![LastFmRadioStation::default()],
            last_refresh_success_utc: Some(last),
            new_plays_since_refresh: plays,
            ..Default::default()
        };
        // A play every three and a half minutes for eleven hours redraws nothing.
        for minute in (0..11 * 60).step_by(7) {
            let now = start + TimeDelta::minutes(minute) + TimeDelta::seconds(30);
            let listener = user(start, (minute / 7) as i32 + 1);
            assert!(
                !should_refresh_after_play(&listener, &settings, now),
                "minute {minute}"
            );
            assert!(
                !should_schedule_periodic_refresh(&listener, &settings, now),
                "minute {minute}"
            );
        }
        // Twelve hours on, the minute scan redraws it whether or not anyone listened.
        for plays in [0, 40] {
            let at = start + TimeDelta::hours(12) + TimeDelta::minutes(1);
            assert!(should_schedule_periodic_refresh(
                &user(start, plays),
                &settings,
                at
            ));
            assert!(!should_schedule_periodic_refresh(
                &user(start, plays),
                &settings,
                start + TimeDelta::hours(11) + TimeDelta::minutes(59)
            ));
        }
        // A refresh left marked as running by a process that stopped holds nothing back.
        let mut stuck = user(start, 0);
        stuck.refreshing = true;
        stuck.last_refresh_attempt_utc = Some(start + TimeDelta::hours(3));
        assert!(should_schedule_periodic_refresh(
            &stuck,
            &settings,
            start + TimeDelta::hours(12) + TimeDelta::minutes(1)
        ));
        assert!(!is_refreshing(&stuck, start + TimeDelta::hours(12)));
        assert!(is_refreshing(
            &stuck,
            start + TimeDelta::hours(3) + TimeDelta::minutes(1)
        ));
    }

    /// The reference values for these names.
    #[test]
    fn startup_jitter_matches_the_reference_values() {
        assert_eq!(startup_jitter("Alice"), Duration::from_millis(239));
        assert_eq!(startup_jitter(" bob "), Duration::from_millis(421));
    }
}
