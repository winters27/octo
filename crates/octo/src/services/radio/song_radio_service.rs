//! Radio from one song, album or artist (getSimilarSongs): every source asked at once, their
//! answers blended, resolved local first, and each song marked with the source it came from.

use std::collections::HashSet;
use std::sync::Arc;

use indexmap::IndexMap;
use octo_core::common::skipped_takes;
use octo_core::last_fm::last_fm_radio_placement::{self, Spacing};
use octo_core::last_fm::last_fm_radio_recommendation_service::{
    IgnoreCaseKeys, Randomizer, default_randomizer,
};
use octo_core::last_fm::last_fm_radio_seed_normalizer as seed_normalizer;
use octo_core::last_fm::last_fm_radio_spacing;
use octo_core::models::domain::Song;
use octo_core::radio::radio_blend::{self, RadioPick};
use octo_core::radio::radio_provider::{self, LIBRARY};
use octo_core::radio::{RadioAnswer, RadioMatch, RadioSeed};
use octo_core::settings::{ExplicitFilter, SettingsStore};
use parking_lot::Mutex;
use tokio::sync::Semaphore;
use tracing::{debug, info};

use super::radio_outcome_store::RadioOutcomeStore;
use super::radio_source_set::RadioSourceSet;
use crate::services::family::{FamilyStore, kid_filter};
use crate::services::last_fm::LastFmRadioTrackResolver;
use crate::services::sonic::SonicStore;

/// Picks resolved beyond the count asked for, so the sound check has some to drop.
pub const PICK_HEADROOM: f64 = 1.5;

/// Song radio: the parts shared across requests, with each request's resolver handed to
/// [`SongRadioService::build`].
pub struct SongRadioService {
    sources: Arc<RadioSourceSet>,
    /// The Last.fm, radio source and Subsonic settings.
    settings: Arc<SettingsStore>,
    sonic: Option<Arc<SonicStore>>,
    outcomes: Option<Arc<RadioOutcomeStore>>,
    /// Source of the placement's draw. Tests replace it with a seeded one so a radio repeats
    /// exactly.
    randomizer: Mutex<Randomizer>,
    /// The family, whose kid profiles get clean radio.
    family: Option<Arc<FamilyStore>>,
}

impl SongRadioService {
    pub fn new(
        sources: Arc<RadioSourceSet>,
        settings: Arc<SettingsStore>,
        sonic: Option<Arc<SonicStore>>,
        outcomes: Option<Arc<RadioOutcomeStore>>,
    ) -> Self {
        SongRadioService {
            sources,
            settings,
            sonic,
            outcomes,
            randomizer: Mutex::new(default_randomizer()),
            family: None,
        }
    }

    /// The draw placement uses from now on, as the station service's `set_randomizer`.
    pub fn set_randomizer(&self, randomizer: Randomizer) {
        *self.randomizer.lock() = randomizer;
    }

    /// The service with the family, so a kid profile's radio is clean.
    pub fn with_family(mut self, family: Arc<FamilyStore>) -> Self {
        self.family = Some(family);
        self
    }

    pub fn sources(&self) -> &Arc<RadioSourceSet> {
        &self.sources
    }

    pub fn enabled(&self) -> bool {
        self.settings.current().last_fm.enable_radio && !self.sources.available().is_empty()
    }

    /// `listener`: who the radio is for, named the way scrobbles name them (RequestIdentity), so
    /// what they play teaches their own weights; None for nobody. `client_order`: the caller
    /// mixes the radio itself (Octo's apps send `octoMix=client`) and wants the order of fit, so
    /// outside songs are not placed.
    pub async fn build(
        &self,
        seed: &RadioSeed,
        count: i32,
        listener: Option<&str>,
        auth: &IndexMap<String, String>,
        bans: &IgnoreCaseKeys,
        resolver: &LastFmRadioTrackResolver,
        client_order: bool,
    ) -> Vec<Song> {
        // Strip collab/feature decoration so the catalogs find the canonical artist.
        let lookup = RadioSeed {
            artist: seed_normalizer::artist(&seed.artist),
            title: seed_normalizer::title(&seed.title),
            ..seed.clone()
        };
        // A few more than asked for: filler and the listener's one-star songs are left out.
        let mut answers = self
            .sources
            .ask_all(&lookup, (count + 10).min(100), auth, None)
            .await;

        // When no catalog has songs like this very song, the seed's own tags know its style
        // better than a guess from its artist's name, which for a YouTube upload is only the
        // channel's (#78).
        let library_led =
            seed.library_song.is_some() && !seed.is_artist() && radio_blend::library_led(&answers, count);
        if library_led && let Some(library_song) = &seed.library_song {
            answers.push(RadioAnswer::new(
                LIBRARY,
                RadioMatch::Song,
                Vec::new(),
                resolver.library_neighbours(library_song, count, auth).await,
            ));
            if let Some(genre) = radio_blend::primary_genre(library_song.genre.as_deref()) {
                answers.extend(self.sources.tag_all(&genre, count).await);
            }
        }

        let answers: Vec<RadioAnswer> = answers
            .into_iter()
            .map(|answer| radio_blend::without_filler_or_bans(answer, bans))
            .collect();
        let weights = self.sources.weights(listener, library_led);
        let picks = radio_blend::blend(
            &answers,
            &weights,
            (f64::from(count) * PICK_HEADROOM).ceil() as usize,
            &seed_normalizer::track_key(&seed.artist, &seed.title),
            seed.library_song.as_ref().map(|song| song.id.as_str()),
            count,
        );
        info!(
            "Radio for '{} - {}': {}; weights {}; {} picked{}",
            seed.artist,
            seed.title,
            answers
                .iter()
                .map(|answer| format!("{} {} {}", answer.provider, answer.r#match, answer.count()))
                .collect::<Vec<_>>()
                .join(", "),
            weights
                .iter()
                .map(|(provider, weight)| format!("{provider} {weight:.2}"))
                .collect::<Vec<_>>()
                .join(", "),
            picks.len(),
            if library_led { ", led by the library" } else { "" }
        );

        // For each pick, prefer the local copy if we own it. Tracks the user already has play at
        // full quality from Navidrome and skip the YouTube round trip. Lookups run in parallel.
        let gate = Semaphore::new(10);
        let resolved: Vec<Option<Song>> = futures::future::join_all(picks.iter().map(|pick| {
            let gate = &gate;
            async move {
                let _permit = gate.acquire().await.expect("the gate is never closed");
                resolve(pick, auth, resolver).await
            }
        }))
        .await;

        // One per song, and never the seed itself.
        let seed_id = seed.library_song.as_ref().map(|song| song.id.as_str());
        let mut seen = HashSet::new();
        let mut scored: Vec<(Song, f64)> = picks
            .iter()
            .zip(resolved)
            .filter_map(|(pick, song)| song.map(|song| (song, pick.score)))
            .filter(|(song, _)| Some(song.id.as_str()) != seed_id)
            .filter(|(song, _)| seen.insert(song.id.clone()))
            .collect();
        // Sound check: a library pick that sounds far from the seed slips down, one that sounds
        // close moves up. It catches a catalog's wrong-mood pick that the other sources missed.
        // Only songs Octo has analyzed have a sound, so outside songs keep their score.
        if let Some(sonic) = &self.sonic
            && self.settings.current().radio_sources.sounds_alike
            && let Some(seed_song) = seed.library_song.as_ref().filter(|song| !song.id.is_empty())
        {
            let fit = sonic.fit_factors(
                &seed_song.id,
                scored
                    .iter()
                    .filter(|(song, _)| song.is_local)
                    .map(|(song, _)| song.id.as_str()),
            );
            if !fit.is_empty() {
                for (song, score) in &mut scored {
                    *score *= fit.get(&song.id).copied().unwrap_or(1.0);
                }
                // A stable sort, highest score first.
                scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            }
        }
        // The admin's explicit filter, as stations apply it: with outside songs from every
        // source, radio is where most unknown lyrics now come from. A kid profile's is clean.
        // Skip covers and instrumentals leaves outside cover, karaoke and instrumental takes out the same way.
        let settings = self.settings.current();
        let filter = kid_filter::explicit_filter_for(&settings, self.family.as_deref(), listener);
        let skip_covers_and_instrumentals = settings.soulseek.skip_covers_and_instrumentals;
        let songs: Vec<Song> = scored
            .into_iter()
            .map(|(song, _)| song)
            .filter(|song| match filter {
                ExplicitFilter::CleanOnly => octo_core::models::domain::explicit_status::clean_only_allows(
                    song.explicit_content_lyrics,
                ),
                ExplicitFilter::ExplicitOnly => song.explicit_content_lyrics != Some(3),
                ExplicitFilter::All => true,
            })
            .filter(|song| !skipped_takes::hides_song(song, skip_covers_and_instrumentals))
            .take(count.max(0) as usize)
            .collect();
        if let Some(outcomes) = &self.outcomes {
            outcomes.served(
                listener,
                songs.iter().map(|song| {
                    (
                        song.id.as_str(),
                        radio_provider::from_display_name(song.suggested_by.as_deref()),
                    )
                }),
                None,
                true,
            );
        }

        // A library-led radio keeps its order: its neighbors often share the seed's artist (one
        // uploader for a whole playlist), and spacing by artist would push every one of them
        // back.
        if library_led {
            info!("Radio for '{} - {}': led by the library", seed.artist, seed.title);
            songs
        } else if client_order {
            // Octo's own apps mix the radio themselves and ask for the order of fit
            // (octoMix=client).
            info!("Radio for '{} - {}': client order", seed.artist, seed.title);
            last_fm_radio_spacing::spread(
                &songs,
                |song: &Song| Some(song.artist.clone()),
                Some(&seed.artist),
            )
        } else {
            // For every other app this order is the radio, so outside songs are placed at random
            // at the radio's own mix: the share of its songs from outside the library, not the
            // Discovery balance, which is Your Radio's (last_fm_radio_placement). Artists are
            // spaced as spread spaces them, with no title gap.
            let outside = songs.iter().filter(|song| !song.is_local).count();
            let share = if songs.is_empty() {
                0.0
            } else {
                outside as f64 / songs.len() as f64
            };
            info!(
                "Radio for '{} - {}': placed at share {share:.2}",
                seed.artist, seed.title
            );
            let randomizer = self.randomizer.lock().clone();
            let mut random = randomizer();
            last_fm_radio_placement::place(
                &songs,
                |song: &Song| !song.is_local,
                |song: &Song| Some(song.artist.clone()),
                Some(&seed.artist),
                Spacing::artists(last_fm_radio_spacing::GAP),
                share,
                &mut *random,
            )
        }
    }
}

/// One pick as a song the listener can play, marked with its source.
async fn resolve(
    pick: &RadioPick,
    auth: &IndexMap<String, String>,
    resolver: &LastFmRadioTrackResolver,
) -> Option<Song> {
    let song = match (&pick.library_song, &pick.track) {
        (Some(library), _) if pick.needs_reading => resolver.read_library_song(&library.id, auth).await,
        (Some(library), _) => Some(library.clone()),
        (None, Some(track)) => {
            resolver
                .resolve(
                    &track.artist,
                    &track.title,
                    track.duration,
                    auth,
                    track.you_tube_id.as_deref(),
                )
                .await
        }
        (None, None) => {
            debug!("radio resolve failed for a {} pick", pick.provider);
            None
        }
    };
    song.map(|mut song| {
        song.suggested_by = radio_provider::display_name(Some(&pick.provider)).map(str::to_string);
        song
    })
}

#[cfg(test)]
#[path = "song_radio_service_tests.rs"]
mod tests;
