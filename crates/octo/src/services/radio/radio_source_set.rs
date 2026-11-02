//! Every radio source, asked together. One slow or failing source costs its own answer, never
//! the radio.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use indexmap::IndexMap;
use octo_core::json::datetime;
use octo_core::radio::radio_blend::{RadioWeights, SOUNDS_ALIKE_WHEN_UNKNOWN};
use octo_core::radio::radio_provider::{LAST_FM, LIBRARY, LISTEN_BRAINZ, SOUNDS_ALIKE, YOU_TUBE_MUSIC};
use octo_core::radio::{RadioAnswer, RadioSeed};
use octo_core::settings::{RadioSourceSettings, SettingsStore};
use parking_lot::Mutex;
use tracing::{info, warn};

use super::radio_outcome_store::RadioOutcomeStore;
use super::radio_source::IRadioSource;
use crate::services::last_fm::lookup_trouble;

/// Failures or timeouts in a row before a source is rested, and for how long. A source that
/// hangs would otherwise cost every radio its full timeout, and a station build one timeout per
/// seed.
pub const FAILURES_BEFORE_REST: i32 = 3;
pub const LOWEST_WEIGHT: f64 = 0.3;
pub const HIGHEST_WEIGHT: f64 = 1.5;

pub struct RadioSourceSet {
    sources: Vec<Arc<dyn IRadioSource>>,
    /// The radio source settings.
    settings: Arc<SettingsStore>,
    outcomes: Option<Arc<RadioOutcomeStore>>,
    source_timeout: Mutex<Duration>,
    rest: Mutex<TimeDelta>,
    /// Failures in a row, and until when a source rests.
    health: Mutex<HashMap<String, (i32, DateTime<Utc>)>>,
}

impl RadioSourceSet {
    pub fn new(
        sources: Vec<Arc<dyn IRadioSource>>,
        settings: Arc<SettingsStore>,
        outcomes: Option<Arc<RadioOutcomeStore>>,
    ) -> Self {
        RadioSourceSet {
            sources,
            settings,
            outcomes,
            source_timeout: Mutex::new(Duration::from_secs(8)),
            rest: Mutex::new(TimeDelta::minutes(5)),
            health: Mutex::new(HashMap::new()),
        }
    }

    /// `SourceTimeout = ...`, for tests.
    pub fn set_source_timeout(&self, timeout: Duration) {
        *self.source_timeout.lock() = timeout;
    }

    /// `Rest = ...`, for tests.
    pub fn set_rest(&self, rest: TimeDelta) {
        *self.rest.lock() = rest;
    }

    pub fn available(&self) -> Vec<Arc<dyn IRadioSource>> {
        let now = Utc::now();
        let health = self.health.lock();
        self.sources
            .iter()
            .filter(|source| {
                source.available()
                    && !health
                        .get(source.provider())
                        .is_some_and(|(_, rest_until)| *rest_until > now)
            })
            .cloned()
            .collect()
    }

    /// What each source counts for this radio: its base weight, raised for Sounds alike when no
    /// catalog knows the song, times what this listener's plays have taught (1 with no data),
    /// kept within [`LOWEST_WEIGHT`] and [`HIGHEST_WEIGHT`]. A base of 0 means the admin left the
    /// source out, and stays 0.
    pub fn weights(&self, listener: Option<&str>, library_led: bool) -> RadioWeights {
        let current = self.settings.current().radio_sources.clone();
        let learned = self
            .outcomes
            .as_ref()
            .map(|outcomes| outcomes.multipliers(listener, None))
            .unwrap_or_default();
        let mut sound = RadioSourceSettings::effective_weight(current.sounds_alike_weight);
        if library_led && sound > 0.0 {
            sound = sound.max(SOUNDS_ALIKE_WHEN_UNKNOWN);
        }
        // The bounds never undo the admin: a base set below 0.3 or above 1.5 is itself the bound.
        let weigh = |provider: &str, base: f64| {
            if base <= 0.0 {
                0.0
            } else {
                (base * learned.get(provider).copied().unwrap_or(1.0))
                    .clamp(LOWEST_WEIGHT.min(base), HIGHEST_WEIGHT.max(base))
            }
        };
        IndexMap::from([
            (
                LAST_FM.to_string(),
                weigh(
                    LAST_FM,
                    RadioSourceSettings::effective_weight(current.last_fm_weight),
                ),
            ),
            (
                YOU_TUBE_MUSIC.to_string(),
                weigh(
                    YOU_TUBE_MUSIC,
                    RadioSourceSettings::effective_weight(current.you_tube_music_weight),
                ),
            ),
            (
                LISTEN_BRAINZ.to_string(),
                weigh(
                    LISTEN_BRAINZ,
                    RadioSourceSettings::effective_weight(current.listen_brainz_weight),
                ),
            ),
            (SOUNDS_ALIKE.to_string(), weigh(SOUNDS_ALIKE, sound)),
            (LIBRARY.to_string(), 1.0),
        ])
    }

    /// `only`: the sources to ask; all available ones when None. An artist station that has
    /// already asked Last.fm itself leaves Last.fm out.
    pub async fn ask_all(
        &self,
        seed: &RadioSeed,
        count: i32,
        auth: &IndexMap<String, String>,
        only: Option<&(dyn Fn(&dyn IRadioSource) -> bool + Sync)>,
    ) -> Vec<RadioAnswer> {
        let asked: Vec<Arc<dyn IRadioSource>> = self
            .available()
            .into_iter()
            .filter(|source| only.is_none_or(|only| only(source.as_ref())))
            .collect();
        let resting = self.resting(only);
        let answers = futures::future::join_all(
            asked
                .iter()
                .map(|source| self.ask(source.as_ref(), source.songs_like(seed, count, auth))),
        )
        .await;
        note_unanswered(&answers, resting);
        answers.into_iter().map(|(answer, _)| answer).collect()
    }

    pub async fn tag_all(&self, tag: &str, count: i32) -> Vec<RadioAnswer> {
        let asked = self.available();
        let resting = self.resting(None);
        let answers = futures::future::join_all(
            asked
                .iter()
                .map(|source| self.ask(source.as_ref(), source.tag(tag, count))),
        )
        .await;
        note_unanswered(&answers, resting);
        answers.into_iter().map(|(answer, _)| answer).collect()
    }

    /// Whether a source that would otherwise be asked is resting now.
    fn resting(&self, only: Option<&(dyn Fn(&dyn IRadioSource) -> bool + Sync)>) -> bool {
        let now = Utc::now();
        let health = self.health.lock();
        self.sources.iter().any(|source| {
            source.available()
                && only.is_none_or(|only| only(source.as_ref()))
                && health
                    .get(source.provider())
                    .is_some_and(|(_, rest_until)| *rest_until > now)
        })
    }

    /// One source's answer, and whether it answered cleanly: in time, without an error, and with
    /// every Last.fm lookup inside it answered. Last.fm trouble inside a source still counts for
    /// the station build that asked.
    async fn ask(
        &self,
        source: &dyn IRadioSource,
        ask: impl Future<Output = anyhow::Result<RadioAnswer>>,
    ) -> (RadioAnswer, bool) {
        let timeout = *self.source_timeout.lock();
        let (outcome, inner) = lookup_trouble::watch(tokio::time::timeout(timeout, ask)).await;
        if inner.core {
            lookup_trouble::note_core();
        }
        match outcome {
            Ok(Ok(answer)) => {
                // A success clears the count of failures in a row, but never cuts a rest short:
                // an answer to a call made before the rest began says nothing about the source
                // now.
                let mut health = self.health.lock();
                let entry = health
                    .entry(source.provider().to_string())
                    .or_insert((0, datetime::min_value()));
                entry.0 = 0;
                (answer, !inner.any())
            }
            Err(_) => {
                info!(
                    "Radio source {} took longer than {} s; left out",
                    source.provider(),
                    timeout.as_secs_f64()
                );
                self.failed(source.provider());
                (RadioAnswer::nothing(source.provider()), false)
            }
            Ok(Err(error)) => {
                warn!("Radio source {} failed; left out: {error}", source.provider());
                self.failed(source.provider());
                (RadioAnswer::nothing(source.provider()), false)
            }
        }
    }

    fn failed(&self, provider: &str) {
        let mut health = self.health.lock();
        let entry = health
            .entry(provider.to_string())
            .or_insert((0, datetime::min_value()));
        entry.0 += 1;
        if entry.0 < FAILURES_BEFORE_REST {
            return;
        }
        let rest = *self.rest.lock();
        *entry = (0, Utc::now() + rest);
        warn!(
            "Radio source {provider} failed {FAILURES_BEFORE_REST} times in a row; resting it for {} minutes",
            rest.num_seconds() as f64 / 60.0
        );
    }
}

/// Marks the station build's lookup as not answered by the radio sources when none of them
/// answered cleanly: each one asked failed, timed out or met trouble, or the ones left are all
/// resting. No source to ask at all is a setting, not trouble.
fn note_unanswered(answers: &[(RadioAnswer, bool)], resting: bool) {
    if (!answers.is_empty() || resting) && !answers.iter().any(|(_, clean)| *clean) {
        lookup_trouble::note_sources();
    }
}

#[cfg(test)]
mod tests {
    //! The radio sources asked together: base weights, the seed-aware Sounds alike weight, and a
    //! source that keeps failing is rested instead of costing every radio its timeout.

    use std::sync::atomic::{AtomicBool, AtomicUsize};

    use async_trait::async_trait;
    use octo_core::radio::RadioMatch;
    use octo_core::radio::radio_blend::SOUNDS_ALIKE_WHEN_UNKNOWN;
    use octo_core::settings::AppSettings;
    use tokio::sync::Notify;

    use super::*;

    #[derive(Clone, Copy)]
    enum Behavior {
        Nothing,
        Hang,
        Fail,
        Roads,
    }

    struct FakeSource {
        provider: &'static str,
        behavior: Mutex<Behavior>,
        calls: AtomicUsize,
        /// The first call waits for this, when set.
        first_waits: Option<Arc<Notify>>,
        first_done: AtomicBool,
    }

    impl FakeSource {
        fn new(provider: &'static str, behavior: Behavior) -> Arc<FakeSource> {
            Arc::new(FakeSource {
                provider,
                behavior: Mutex::new(behavior),
                calls: AtomicUsize::new(0),
                first_waits: None,
                first_done: AtomicBool::new(false),
            })
        }
    }

    #[async_trait]
    impl IRadioSource for FakeSource {
        fn provider(&self) -> &'static str {
            self.provider
        }

        fn available(&self) -> bool {
            true
        }

        async fn songs_like(
            &self,
            _seed: &RadioSeed,
            _count: i32,
            _auth: &IndexMap<String, String>,
        ) -> anyhow::Result<RadioAnswer> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(gate) = &self.first_waits
                && !self.first_done.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                gate.notified().await;
                return Ok(RadioAnswer::nothing(self.provider));
            }
            let behavior = *self.behavior.lock();
            match behavior {
                Behavior::Nothing => Ok(RadioAnswer::nothing(self.provider)),
                Behavior::Hang => {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
                Behavior::Fail => anyhow::bail!("down"),
                Behavior::Roads => Ok(RadioAnswer::new(
                    self.provider,
                    RadioMatch::Song,
                    vec![octo_core::last_fm::SimilarTrack::new("Portishead", "Roads", 1.0)],
                    Vec::new(),
                )),
            }
        }
    }

    fn set(settings: RadioSourceSettings, sources: Vec<Arc<dyn IRadioSource>>) -> RadioSourceSet {
        RadioSourceSet::new(
            sources,
            Arc::new(SettingsStore::for_tests(AppSettings {
                radio_sources: settings,
                ..Default::default()
            })),
            None,
        )
    }

    fn seed() -> RadioSeed {
        RadioSeed::new("Massive Attack", "Teardrop", Some(330), None)
    }

    async fn ask(set: &RadioSourceSet) -> Vec<RadioAnswer> {
        set.ask_all(&seed(), 10, &IndexMap::new(), None).await
    }

    #[test]
    fn base_weights_are_the_reasoned_starting_points() {
        let weights = set(RadioSourceSettings::default(), Vec::new()).weights(None, false);
        assert_eq!(weights[YOU_TUBE_MUSIC], 1.0);
        assert_eq!(weights[LAST_FM], 0.9);
        assert_eq!(weights[LISTEN_BRAINZ], 0.7);
        assert_eq!(weights[SOUNDS_ALIKE], 0.4);
        assert_eq!(weights[LIBRARY], 1.0);
    }

    #[test]
    fn when_no_catalog_knows_the_song_sounds_alike_counts_more_unless_it_was_left_out() {
        let with = |weight: f64| {
            set(
                RadioSourceSettings {
                    sounds_alike_weight: weight,
                    ..Default::default()
                },
                Vec::new(),
            )
            .weights(None, true)[SOUNDS_ALIKE]
        };
        assert_eq!(with(0.4), SOUNDS_ALIKE_WHEN_UNKNOWN);
        assert_eq!(with(1.2), 1.2);
        assert_eq!(with(0.0), 0.0);
    }

    #[test]
    fn an_admin_weight_outside_the_bounds_is_kept() {
        let weights = set(
            RadioSourceSettings {
                last_fm_weight: 0.1,
                you_tube_music_weight: 2.5,
                listen_brainz_weight: 9.0,
                ..Default::default()
            },
            Vec::new(),
        )
        .weights(None, false);
        assert!((weights[LAST_FM] - 0.1).abs() < 1e-6);
        assert!((weights[YOU_TUBE_MUSIC] - 2.5).abs() < 1e-6);
        assert!((weights[LISTEN_BRAINZ] - 3.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn a_slow_source_is_left_out_of_one_radio_the_others_still_answer() {
        let slow = FakeSource::new(YOU_TUBE_MUSIC, Behavior::Hang);
        let quick = FakeSource::new(LAST_FM, Behavior::Roads);
        let set = set(RadioSourceSettings::default(), vec![slow, quick]);
        set.set_source_timeout(Duration::from_millis(100));
        let answers = ask(&set).await;
        let of = |provider: &str| answers.iter().find(|a| a.provider == provider).expect("answered");
        assert_eq!(of(YOU_TUBE_MUSIC).r#match, RadioMatch::None);
        assert_eq!(of(LAST_FM).tracks.len(), 1);
    }

    #[tokio::test]
    async fn a_source_that_keeps_failing_is_rested_then_comes_back() {
        let failing = FakeSource::new(LISTEN_BRAINZ, Behavior::Fail);
        let set = set(RadioSourceSettings::default(), vec![failing.clone()]);
        set.set_rest(TimeDelta::milliseconds(300));
        for _ in 0..FAILURES_BEFORE_REST {
            ask(&set).await;
        }
        assert!(set.available().is_empty());
        ask(&set).await;
        assert_eq!(
            failing.calls.load(std::sync::atomic::Ordering::SeqCst),
            FAILURES_BEFORE_REST as usize
        );

        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(set.available().len(), 1);
    }

    #[tokio::test]
    async fn a_success_clears_the_failures() {
        let flaky = FakeSource::new(LISTEN_BRAINZ, Behavior::Fail);
        let set = set(RadioSourceSettings::default(), vec![flaky.clone()]);
        for _ in 0..FAILURES_BEFORE_REST - 1 {
            ask(&set).await;
        }
        *flaky.behavior.lock() = Behavior::Nothing;
        ask(&set).await;
        *flaky.behavior.lock() = Behavior::Fail;
        ask(&set).await;
        assert_eq!(set.available().len(), 1);
    }

    #[tokio::test]
    async fn an_answer_to_a_call_from_before_the_rest_does_not_end_the_rest() {
        let gate = Arc::new(Notify::new());
        let source = Arc::new(FakeSource {
            provider: LISTEN_BRAINZ,
            behavior: Mutex::new(Behavior::Fail),
            calls: AtomicUsize::new(0),
            first_waits: Some(gate.clone()),
            first_done: AtomicBool::new(false),
        });
        let set = Arc::new(set(RadioSourceSettings::default(), vec![source]));
        set.set_source_timeout(Duration::from_secs(30));
        let early = {
            let set = set.clone();
            tokio::spawn(async move { ask(&set).await })
        };
        // Let the early call start and wait at its gate.
        tokio::time::sleep(Duration::from_millis(50)).await;
        for _ in 0..FAILURES_BEFORE_REST {
            ask(&set).await;
        }
        assert!(set.available().is_empty());

        gate.notify_one();
        early.await.expect("the early call ends");
        assert!(set.available().is_empty());
    }

    #[tokio::test]
    async fn only_the_sources_asked_for_are_asked() {
        let last_fm = FakeSource::new(LAST_FM, Behavior::Nothing);
        let ytm = FakeSource::new(YOU_TUBE_MUSIC, Behavior::Nothing);
        let set = set(
            RadioSourceSettings {
                you_tube_music: true,
                ..Default::default()
            },
            vec![last_fm.clone(), ytm],
        );
        let not_last_fm = |source: &dyn IRadioSource| source.provider() != LAST_FM;
        let answers = set
            .ask_all(&seed(), 10, &IndexMap::new(), Some(&not_last_fm))
            .await;
        let providers: Vec<&str> = answers.iter().map(|a| a.provider.as_str()).collect();
        assert_eq!(providers, [YOU_TUBE_MUSIC]);
        assert_eq!(last_fm.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
