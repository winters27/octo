//! What a download is and what is written to it: identification, the old catalog fill, the
//! album folder's facts, the loudness, the genre and the cover, the tags themselves, and the
//! album gain at the end of a walk. The tag body itself is `octo_media::tags::write_song`.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use octo_core::common::{SongIdentity, dotnet};
use octo_core::fingerprint::heart_release::HeartSong;
use octo_core::fingerprint::recording_identity::Identity;
use octo_core::last_fm::last_fm_radio_recommendation_service::canonical_tag;
use octo_core::metadata::deezer_metadata_service::FullTrackMeta;
use octo_core::metadata::genre_normalizer::{GenreNormalizer, GenreTagAction};
use octo_core::models::domain::Song;
use octo_core::settings::{FolderStructure, GenreFallbackSource, GenreSettings, MetadataSettings};
use octo_core::tagging::tag_guard;
use octo_core::tagging::{
    AlbumGrouping, AlbumTagContext, CatalogBlankFiller, FileFacts, MeasuredLoudness, PreviewLoudnessMeter,
    ReleaseIdentifier, ReplayGainText, explicit_advisory,
};
use octo_media::audio::{ILoudnessMeter, Loudness, ReplayGainTags};
use octo_media::cover::cover_image;
use octo_media::tags::{
    GenreWrite, KeptIdentityTags, TagFile, embed_cover, front_cover, release_fact_tags, tag_writer_extras,
    write_album_facts, write_album_gain, write_song,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::placement::{get_directory_name, get_file_name, is_audio_file};
use super::{BaseDownloadService, RequestedIdentity, get_extension};
use crate::services::common::acquisition_tracker::acquisition_event_kinds;
use crate::services::cover_art::CoversOnHand;
use crate::services::fingerprint::music_brainz_client::{HeartQuestions, Lane};
use crate::services::library::navidrome_song_path_resolver::get_full_path;
use crate::services::soulseek::soulseek_download_service::INCOMING_FOLDER_NAME;

impl BaseDownloadService {
    /// Whether a landed file came from the video site's staging folder, whose tags are an
    /// uploader's and not evidence of anything.
    pub fn is_staged_upload(path: &str) -> bool {
        path.split(['/', '\\'])
            .any(|segment| dotnet::eq_ignore_case(segment, INCOMING_FOLDER_NAME))
    }

    /// A download asked for with no album and with an ISRC starts the no-album pick on the
    /// Background lane, which fills `requested.album_pick` once it has an answer. Identifying
    /// the file takes it only if it has finished by then, and never waits for it.
    pub(super) fn start_album_pick(&self, song: &Song, requested: &RequestedIdentity) {
        let Some(isrc) = song.isrc.as_deref().and_then(SongIdentity::normalize_isrc) else {
            return;
        };
        let (Some(music_brainz), Some(deezer)) = (&self.services.music_brainz, &self.services.deezer) else {
            return;
        };
        if !requested.album.trim().is_empty() {
            return;
        }
        let questions = HeartQuestions::new(Arc::clone(music_brainz), deezer.clone(), Lane::Background);
        let heart = HeartSong {
            artist: song.artist.clone(),
            title: song.title.clone(),
            isrc,
            seconds: song
                .duration
                .and_then(|seconds| u32::try_from(seconds).ok())
                .unwrap_or(0),
            ..Default::default()
        };
        let pick = Arc::clone(&requested.album_pick);
        tokio::spawn(async move {
            if let Some(group) = questions.no_album_pick(&heart).await {
                let _ = pick.set(group);
            }
        });
    }

    /// Works out what the file is and sets the Song from it. Every candidate release the
    /// fingerprint service, the music database and the catalog offer is weighed against what was
    /// asked for and what landed; a sure match sets the album-level tags, a doubtful one only
    /// fills blanks, the way the catalog always did. Reads only: the tags are written by
    /// `write_metadata` once the file has been placed. Best-effort; a miss never breaks the
    /// download.
    pub async fn identify(
        &self,
        song: &mut Song,
        requested: &RequestedIdentity,
        file_path: &str,
        album: Option<&AlbumTagContext<Loudness>>,
    ) {
        // Last.fm/YouTube titles often carry a redundant "Artist - " prefix (e.g. "Radiohead - No
        // Surprises") which mislabels the file, so it goes from the written title. The lookup
        // gets the title whole: the catalog tries it without "(Official Video)" and guests on its
        // own, and a "(Live)" left in is what stops a live download being tagged with the studio
        // album's cover, track number and year.
        song.title = strip_artist_prefix(&song.artist, &song.title);
        let query_title = song.title.clone();
        let settings = self.settings.current().metadata.clone();

        let request = ReleaseIdentifier::with_album_group(
            ReleaseIdentifier::request_for(
                song,
                &song.artist,
                &query_title,
                Some(&requested.album),
                requested.track,
            ),
            requested.album_pick.get(),
        );
        let identified = self
            .identifier()
            .identify(
                song,
                request,
                file_path,
                !Self::is_staged_upload(file_path),
                album,
                &CancellationToken::new(),
            )
            .await;
        let mut planned = false;
        match identified {
            Ok(mut plan) => {
                planned = true;
                if settings.tag_rehearsal {
                    plan.apply_rehearsal_to(song);
                } else {
                    plan.apply_to(song);
                    if plan.album_from_candidate()
                        && let Some(chosen) = plan.fields.get("album").and_then(|f| f.value.as_deref())
                        && let Some(file_album) = plan
                            .evidence
                            .as_ref()
                            .and_then(|e| e.file.album.as_deref())
                            .filter(|a| !a.is_empty())
                        && SongIdentity::key(file_album) != SongIdentity::key(chosen)
                    {
                        info!(
                            "'{chosen}' replaces the file's own '{file_album}' ({})",
                            plan.confidence
                        );
                    }
                }
                let catalog_best = plan.catalog_best.clone();
                song.tag_plan = Some(Box::new(plan));
                Self::fill_blanks_from_catalog(song, catalog_best.as_ref());
            }
            Err(e) => {
                warn!(
                    "Identification failed for '{} - {}'; filling blanks the old way: {e}",
                    song.artist, song.title
                );
                self.fill_blanks_from_catalog_async(song, &query_title).await;
            }
        }

        fill_album_from_file(song, file_path);
        Self::apply_single_fallback(song, settings.album_from_title);

        // Explicit or clean, from what landed; the library server shows it on every song.
        let advisory = explicit_advisory::decide(song, song.tag_plan.as_deref(), song.source_file.as_deref());
        song.advisory = advisory.as_ref().map(|advisory| advisory.value);
        if let (Some(plan), Some(advisory)) = (song.tag_plan.as_deref_mut(), advisory.as_ref()) {
            plan.fields
                .insert("advisory".to_string(), explicit_advisory::field(advisory));
        }
        if let Some(advisory) = &advisory {
            let text = match advisory.value {
                explicit_advisory::EXPLICIT => "Marked explicit",
                explicit_advisory::CLEAN => "Marked as the clean edit",
                _ => "Marked not explicit",
            };
            let why = if advisory.source == explicit_advisory::FILE_NAME_SOURCE {
                "The file's name says it is the clean edit"
            } else {
                "The catalog's match for this exact version says so"
            };
            self.log_song_step(song, acquisition_event_kinds::TAGS, text, Some(why));
        }

        let sibling = if self.settings.current().subsonic.folder_structure == FolderStructure::Organized {
            self.album_sibling(song, requested, file_path)
        } else {
            None
        };
        if let Some(sibling) = &sibling {
            join_album(song, sibling);
        }

        if planned && !settings.tag_rehearsal {
            match album {
                Some(album) => {
                    album.pin(song);
                    if let Some(plan) = song.tag_plan.as_deref() {
                        album.capture(plan, song);
                    }
                }
                None => {
                    if let Some(sibling) = &sibling {
                        pin_to_sibling(song, sibling);
                    }
                }
            }
        }
    }

    /// The old catalog enrichment, asked for on its own when identification itself failed.
    async fn fill_blanks_from_catalog_async(&self, song: &mut Song, query_title: &str) {
        if let Some(deezer) = &self.services.deezer {
            let meta = deezer.enrich_track_full(&song.artist, query_title).await;
            Self::fill_blanks_from_catalog(song, meta.as_ref());
        }
    }

    /// Fills any missing metadata on `song` from the catalog's best hit. Existing values win (a
    /// well-tagged Soulseek FLAC is enriched, not overwritten); the catalog fills the gaps and
    /// supplies the cover. The album's own facts (its cover, its compilation flag) are taken only
    /// when the hit is the album the song is filed under, since the chooser may have put the song
    /// on another release than the catalog's first hit.
    pub fn fill_blanks_from_catalog(song: &mut Song, m: Option<&FullTrackMeta>) {
        let Some(m) = m else {
            return;
        };
        let empty = |value: &Option<String>| value.as_deref().is_none_or(str::is_empty);
        let same_album = song.album.is_empty()
            || empty(&m.album_title)
            || SongIdentity::key(&song.album) == SongIdentity::key(m.album_title.as_deref().unwrap_or(""));

        // The catalog's main artist names the folder when the request carried a list of credits
        // (#49); its contributors give every credited artist a value of their own, so Navidrome
        // files a collaboration under each of them.
        if empty(&song.primary_artist) && !empty(&m.artist_name) {
            song.primary_artist = m.artist_name.clone();
        }
        if song.artists.is_empty()
            && let Some(contributors) = m.contributors.as_ref().filter(|c| c.len() > 1)
        {
            song.artists = contributors.clone();
        }
        if song.album.is_empty()
            && let Some(title) = m.album_title.as_deref().filter(|t| !t.is_empty())
        {
            song.album = title.to_string();
        }
        if same_album {
            // The album's own artist, not the track's: the two differ on every feature and every
            // compilation.
            if empty(&song.album_artist)
                && let Some(album_artist) = m
                    .album_artist_name
                    .as_deref()
                    .or(m.artist_name.as_deref())
                    .filter(|a| !a.is_empty())
            {
                song.album_artist = Some(album_artist.to_string());
            }
            if Self::is_various_artists(m.album_artist_name.as_deref())
                || m.record_type
                    .as_deref()
                    .is_some_and(|t| dotnet::eq_ignore_case(t, "compile"))
            {
                song.is_compilation = true;
            }
            // The catalog album's cover only when it is that album by the song's own artist.
            let by = m
                .album_artist_name
                .as_deref()
                .or(m.artist_name.as_deref())
                .unwrap_or("");
            let wanted: Vec<&str> = [
                song.album_artist.as_deref(),
                song.primary_artist.as_deref(),
                Some(song.artist.as_str()),
            ]
            .into_iter()
            .flatten()
            .collect();
            if empty(&song.cover_art_url_large)
                && octo_core::common::cover_names::release_artist_fits(&wanted, by)
            {
                song.cover_art_url_large = m.album_cover_url.clone();
            }
            if song.year.is_none() {
                song.year = m.year;
            }
            if song.track.is_none() {
                song.track = m.track_number;
            }
            if song.disc_number.is_none() {
                song.disc_number = m.disc_number;
            }
            if song.total_tracks.is_none() {
                song.total_tracks = m.total_tracks;
            }
            if empty(&song.label) {
                song.label = m.label.clone();
            }
            if empty(&song.barcode) {
                song.barcode = m.barcode.clone();
            }
            if empty(&song.release_date) {
                song.release_date = m.release_date.clone();
            }
        }
        if song.duration.is_none() {
            song.duration = m.duration;
        }
        if empty(&song.genre) {
            song.genre = m.genre.clone();
        }
        if empty(&song.isrc) {
            song.isrc = m.isrc.clone();
        }
    }

    /// One file of the album folder this song will join, when the folder is already there and
    /// the file agrees on the album and its artist; its facts as read, with its path.
    fn album_sibling(
        &self,
        song: &Song,
        requested: &RequestedIdentity,
        current_path: &str,
    ) -> Option<AlbumSiblingFile> {
        if dotnet::is_blank(&song.album) || self.download_path().is_empty() {
            return None;
        }
        let target = self.organized_target(song, requested, get_extension(current_path));
        let dir = get_directory_name(&target).filter(|d| !d.is_empty() && Path::new(d).is_dir())?;
        let current = get_full_path(current_path);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                debug!("could not read the album folder's sibling for {current_path}: {e}");
                return None;
            }
        };
        let sibling = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| e.path().to_string_lossy().into_owned())
            .find(|file| is_audio_file(file) && !dotnet::eq_ignore_case(&get_full_path(file), &current))?;

        let facts = tag_writer_extras::read_facts(&sibling, true);
        if SongIdentity::key(facts.album.as_deref().unwrap_or("")) != SongIdentity::key(&song.album) {
            return None;
        }
        let album_artist = song
            .album_artist
            .clone()
            .or_else(|| song.primary_artist.clone())
            .unwrap_or_else(|| song.artist.clone());
        if let Some(theirs) = facts.album_artist.as_deref().filter(|a| !a.is_empty())
            && !album_artist.is_empty()
            && !SongIdentity::same_artist_name(theirs, &album_artist)
        {
            return None;
        }
        Some(AlbumSiblingFile { path: sibling, facts })
    }

    /// Renames a landed file whose extension names another format than its bytes (a FLAC named
    /// .mp3, an M4A named .flac) and logs it. None when the name was right or the file could not
    /// be renamed; the file is then tagged as it is, as before.
    pub(super) fn fix_landed_extension(
        &self,
        path: &str,
        provider: &str,
        external_id: &str,
    ) -> Option<octo_media::tags::audio_container::ExtensionFix> {
        match octo_media::tags::audio_container::fix_extension(path) {
            Ok(Some(fix)) => {
                info!(
                    "{path} is {}; renamed from {} to {} before tagging",
                    fix.format, fix.from, fix.to
                );
                self.track(|t| {
                    t.log(
                        provider,
                        external_id,
                        acquisition_event_kinds::CHECK,
                        &format!("Renamed to {}: the file is {}", fix.to, fix.format),
                        Some(&format!("It arrived named {}", fix.from)),
                    )
                });
                Some(fix)
            }
            Ok(None) => None,
            Err(e) => {
                warn!("Could not check or fix the extension of {path}: {e}");
                None
            }
        }
    }

    /// Start measuring a landed file, beside identification, when ReplayGain is on.
    pub(super) fn start_loudness(&self, path: &str) -> Option<JoinHandle<Option<Loudness>>> {
        let settings = self.settings.current().metadata.clone();
        if !settings.replay_gain {
            return None;
        }
        let meter = Arc::clone(self.services.loudness_meter.as_ref()?);
        let (path, timeout) = (path.to_string(), settings.effective_replay_gain_timeout_seconds());
        Some(tokio::spawn(async move {
            meter
                .measure(Path::new(&path), timeout, &CancellationToken::new())
                .await
        }))
    }

    /// Wait for the measurement, which has its own cap, and set the song's ReplayGain.
    pub(super) async fn apply_loudness(
        &self,
        song: &mut Song,
        measurement: Option<JoinHandle<Option<Loudness>>>,
        path: &str,
    ) {
        let Some(measurement) = measurement else {
            return;
        };
        let clock = std::time::Instant::now();
        let loudness = match measurement.await {
            Ok(loudness) => loudness,
            Err(e) => {
                debug!("loudness measurement failed for {path}: {e}");
                None
            }
        };
        if let Some(plan) = song.tag_plan.as_deref_mut() {
            plan.stage_seconds
                .insert("loudness".to_string(), clock.elapsed().as_secs_f64());
            plan.integrated_lufs = loudness.map(|l| l.integrated_lufs);
            plan.true_peak_dbfs = loudness.map(|l| l.true_peak_dbfs);
        }
        let Some(tags) = ReplayGainTags::for_track(loudness.as_ref()) else {
            if let Some(plan) = song.tag_plan.as_deref_mut() {
                plan.notes
                    .push("the loudness could not be measured, so the file has no ReplayGain".to_string());
            }
            return;
        };
        song.replay_gain_track_gain_db = Some(tags.gain_db);
        song.replay_gain_track_peak = Some(tags.peak);
    }

    /// A track that still has no album is filed as a single under its own title (#50). That is a
    /// real, correct release, and it gives Navidrome something to group; the alternative is one
    /// "[Unknown Album]" collecting every unrelated album-less track. Skipped for a compilation,
    /// where a hundred one-track albums would be worse than the one untidy bucket.
    pub fn apply_single_fallback(song: &mut Song, enabled: bool) {
        if !enabled || !dotnet::is_blank(&song.album) || dotnet::is_blank(&song.title) {
            return;
        }
        if song.is_compilation || Self::is_various_artists(song.album_artist.as_deref()) {
            return;
        }
        song.album = song.title.trim().to_string();
        if song.album_artist.as_deref().is_none_or(str::is_empty) {
            song.album_artist = Some(song.primary_artist.clone().unwrap_or_else(|| song.artist.clone()));
        }
    }

    pub fn is_various_artists(name: Option<&str>) -> bool {
        name.map(str::trim).is_some_and(|value| {
            !value.is_empty()
                && ["Various Artists", "Various", "VA"]
                    .iter()
                    .any(|various| dotnet::eq_ignore_case(value, various))
        })
    }

    /// Last resort for a file with no usable genre: the listener-supplied tags Last.fm already
    /// caches for radio.
    ///
    /// Gated on `has_api_key`, NOT on radio being on. A user who turned radio off still
    /// configured a key, and tagging is not radio. Last.fm tags are also the rawest input the
    /// normalizer ever sees, so they go through radio's own vocabulary first: that list filters
    /// Last.fm data, which is exactly what it was written for, without the genre settings having
    /// to own it.
    async fn resolve_genre_fallback(&self, song: &Song, settings: &GenreSettings) -> Vec<String> {
        if settings.fallback == GenreFallbackSource::MusicBrainz {
            // The release lookup already carries the genres people voted on for the chosen
            // release. Two votes or more count, so one person's tag cannot name a genre. With no
            // votes it behaves as Last.fm and says so, because a setting that appears to work and
            // silently does nothing is worse than one that is missing.
            let voted = song
                .tag_plan
                .as_deref()
                .and_then(|plan| plan.details())
                .map(|details| details.top_genres(2, 3))
                .unwrap_or_default();
            if !voted.is_empty() {
                return voted;
            }
            info!(
                "MusicBrainz lists no voted genres for {} - {}; using Last.fm top tags",
                song.artist, song.title
            );
        }

        let Some(last_fm) = self.services.last_fm.as_ref().filter(|l| l.has_api_key()) else {
            return Vec::new();
        };
        let tags = async {
            let mut tags = last_fm.get_track_top_tags(&song.artist, &song.title, 8).await?;
            if tags.is_empty() && !song.artist.is_empty() {
                tags = last_fm.get_artist_top_tags(&song.artist, 8).await?;
            }
            anyhow::Ok(tags)
        }
        .await;
        match tags {
            Ok(tags) => tags
                .iter()
                .map(|tag| canonical_tag(tag))
                .filter(|tag| !tag.is_empty())
                .collect(),
            Err(e) => {
                debug!(
                    "genre fallback lookup failed for {} - {}: {e}",
                    song.artist, song.title
                );
                Vec::new()
            }
        }
    }

    /// The last check before the tags are written: each one agrees with the recording the audio
    /// was found to be (see `tag_guard`). What it changed or found goes in the song's plan
    /// notes, its download log and the server log.
    fn guard_tags(&self, song: &mut Song) {
        // No tag claims a version the sound did not prove: a file kept as heard, one a shipped
        // check confirmed while its sound could not settle the version, and one the person kept
        // anyway carry no recording id, no ISRC and no release of their own.
        if let Some(proof) = song
            .verification
            .as_deref()
            .map(|verification| verification.proof.clone())
            .filter(|proof| !proof.allows_recording_tags())
        {
            let claimed = song.music_brainz_recording_id.is_some() || song.isrc.is_some();
            song.music_brainz_recording_id = None;
            song.isrc = None;
            song.music_brainz_release_id = None;
            song.music_brainz_release_group_id = None;
            if claimed {
                let mark = proof.row_mark().unwrap_or("version not proven");
                if let Some(plan) = song.tag_plan.as_deref_mut() {
                    plan.notes
                        .push(format!("{mark}: no recording id or ISRC is written"));
                }
                self.log_song_step(
                    song,
                    acquisition_event_kinds::TAGS,
                    "No recording id or ISRC written",
                    Some(&format!("{mark}: its sound did not prove which recording it is")),
                );
            }
        }
        let identity = Identity::of(song.verification.as_deref());
        let chosen = song
            .tag_plan
            .as_deref()
            .and_then(|plan| plan.chosen.as_ref())
            .map(|chosen| chosen.candidate.clone());
        let findings = tag_guard::guard(song, identity.as_ref(), chosen.as_ref());
        for finding in findings {
            if finding.fixed {
                info!(
                    "Tag check ({}) for '{} - {}': {}",
                    finding.rule, song.artist, song.title, finding.detail
                );
            } else {
                warn!(
                    "Tag check ({}) for '{} - {}': {}",
                    finding.rule, song.artist, song.title, finding.detail
                );
            }
            if let Some(plan) = song.tag_plan.as_deref_mut() {
                plan.notes.push(finding.detail.clone());
            }
            self.log_song_step(
                song,
                acquisition_event_kinds::TAGS,
                if finding.fixed {
                    "Fixed a tag that disagreed with the audio"
                } else {
                    "A tag may disagree with the audio"
                },
                Some(&finding.detail),
            );
        }
    }

    /// Write the Song's tags and the chosen cover onto the file. Returns the cover it settled
    /// on, embedded or already there, so the same picture can go beside the file as cover.jpg;
    /// `None` when there was none or the write failed.
    ///
    /// What the genre and cover lookups need is read first, and the file opened again to write,
    /// so no file is held across a wait.
    pub async fn write_metadata(&self, file_path: &str, song: &mut Song) -> Option<Vec<u8>> {
        self.write_metadata_replacing(file_path, song, None).await
    }

    /// [`Self::write_metadata`] for a download that replaces the library file at
    /// `replacing_path`: that copy's cover is weighed first, since it is the picture the album
    /// shows now.
    pub async fn write_metadata_replacing(
        &self,
        file_path: &str,
        song: &mut Song,
        replacing_path: Option<&str>,
    ) -> Option<Vec<u8>> {
        info!("Writing metadata to: {file_path}");
        self.guard_tags(song);
        // A peer tags a file for the release it ripped. What it wrote about that release goes
        // before Octo writes its own, so the file never names two albums at once (a barcode, a
        // release id or a track number of the album it was filed away from). Worked out on this
        // first read, and done again on the file that is written.
        let (existing, embedded, filed_elsewhere) = match TagFile::open(file_path) {
            Ok(mut file) => {
                let arrived_album = file.album();
                let filed_elsewhere =
                    release_fact_tags::filed_elsewhere(arrived_album.as_deref(), Some(&song.album));
                // The file's own cover is judged before the chain replaces it; whether it came
                // from the album the file was filed away from goes with it.
                let embedded = front_cover(&file).map(|picture| picture.data);
                let existing = file.genres();
                let dropped = release_fact_tags::tidy(&mut file, filed_elsewhere, song.joins_album.as_ref());
                if !dropped.is_empty() {
                    let why = if filed_elsewhere {
                        format!(
                            "it arrived filed under '{}'",
                            arrived_album.as_deref().unwrap_or("")
                        )
                    } else {
                        "a new download keeps no album grouping values".to_string()
                    };
                    let names = dropped.join(", ");
                    if let Some(plan) = song.tag_plan.as_deref_mut() {
                        plan.notes.push(format!("took off the file's own {names}: {why}"));
                    }
                    info!("Took off {names} from {file_path}: {why}");
                    let detail = if filed_elsewhere {
                        format!(
                            "The peer had it filed under \"{}\": {names}",
                            arrived_album.as_deref().unwrap_or("")
                        )
                    } else {
                        format!("A new download starts its own album: {names}")
                    };
                    self.log_song_step(
                        song,
                        acquisition_event_kinds::TAGS,
                        "Took off the peer's own album tags",
                        Some(&detail),
                    );
                }
                if let Some(theirs) = stray_recording_id(&mut file, song) {
                    let detail = format!(
                        "The peer's file named the recording {theirs}, which the audio was not found to be"
                    );
                    if let Some(plan) = song.tag_plan.as_deref_mut() {
                        plan.notes
                            .push(format!("took off the file's own recording id {theirs}"));
                    }
                    info!("Took the peer's recording id {theirs} off {file_path}");
                    self.log_song_step(
                        song,
                        acquisition_event_kinds::TAGS,
                        "Took off the peer's recording id",
                        Some(&detail),
                    );
                }
                (existing, embedded, filed_elsewhere)
            }
            Err(e) => {
                error!("Failed to write metadata to: {file_path}: {e}");
                return None;
            }
        };

        // Genre is the one tag that must be able to write NOTHING.
        //
        // Before this, genre was written ONLY when the song's genre was non-empty, and was never
        // cleared. So when the catalog missed and the source had no genre, whatever multi-value
        // frame the Soulseek peer's file or the yt-dlp output already carried survived untouched:
        // "People & Blogs" and seven-genres-at-once reached the library through the ABSENCE of a
        // write, not a bad one. The fix has to read the existing frame, normalize it, and write
        // the result back.
        let genre_settings = self.settings.current().genre.clone();
        let genres = if genre_settings.enabled {
            let mut plan = GenreNormalizer::plan(&existing, song.genre.as_deref(), &genre_settings, None);

            // Only pay for the lookup when nothing usable survived. Last.fm's top tags are weaker
            // evidence than a real genre frame and earn a turn only when there is no frame left.
            if plan.action != GenreTagAction::Write && genre_settings.fallback != GenreFallbackSource::None {
                let fallback = self.resolve_genre_fallback(song, &genre_settings).await;
                if !fallback.is_empty() {
                    plan = GenreNormalizer::plan(
                        &existing,
                        song.genre.as_deref(),
                        &genre_settings,
                        Some(&fallback),
                    );
                }
            }

            let write = match plan.action {
                GenreTagAction::Write => {
                    song.genre = plan.primary.clone();
                    GenreWrite::Write(plan.genres.clone())
                }
                GenreTagAction::Clear => {
                    // Destructive and not undoable per file, so it is logged below at
                    // Information: a user who regrets their mapping table can at least see what
                    // left.
                    song.genre = None;
                    GenreWrite::Clear
                }
                GenreTagAction::None => GenreWrite::Keep,
            };
            if plan.action != GenreTagAction::None {
                info!(
                    "Genre normalised for {file_path}: [{}] -> [{}]{}",
                    existing.join(", "),
                    plan.genres.join(", "),
                    plan.matched_rule
                        .as_deref()
                        .map(|rule| format!(" via {rule}"))
                        .unwrap_or_default()
                );
            }
            write
        } else if let Some(genre) = song.genre.as_deref().filter(|g| !g.is_empty()) {
            // Feature off: byte-for-byte the behavior that shipped before this.
            GenreWrite::Write(vec![genre.to_string()])
        } else {
            GenreWrite::Keep
        };

        // One chain (#51) instead of one Deezer URL: Apple's master of the album, the Cover Art
        // Archive when a fingerprint named the release, the catalog's own cover, and a cover
        // search by name when none of those is sharp. The file's own cover is judged before the
        // chain replaces it: the same art at a good size stays. The art of the album the file
        // was filed away from is not this album's, so it never outranks the chain; it stays on
        // the file only when the chain finds nothing. A replacement of a library song also weighs
        // the old copy's cover, which is the picture the album shows now. The file gets the
        // chosen cover at 1500 px unless full size is asked for; cover.jpg gets it whole.
        let mut chosen_cover = None;
        let mut embed = None;
        let library_copy = self.library_copy_cover(file_path, replacing_path);
        if let Some(resolver) = &self.services.cover_resolver
            && let Some(cover) = resolver
                .choose(
                    song,
                    &CoversOnHand {
                        arrived: embedded.as_deref(),
                        arrived_from_other_album: filed_elsewhere,
                        library_copy: library_copy.as_deref(),
                    },
                    &CancellationToken::new(),
                )
                .await
        {
            if cover.keeps_existing {
                if let Some(kept) = cover.headline.as_deref() {
                    info!(
                        "Cover kept for {file_path}: {}",
                        cover.note.as_deref().unwrap_or_default()
                    );
                    self.log_cover_step(song, kept, cover.note.as_deref());
                }
            } else {
                let bytes = if self.settings.current().metadata.embed_full_size_covers {
                    cover.bytes.clone()
                } else {
                    cover_image::fit_within(&cover.bytes, MetadataSettings::EMBEDDED_COVER_SIDE as u32)
                };
                info!("Cover art embedded from {}: {} bytes", cover.source, bytes.len());
                let embedded_side = cover_image::measure(&bytes);
                let place = match embedded_side {
                    Some(side) if cover_image::measure(&cover.bytes) == Some(side) => {
                        "embedded in the song".to_string()
                    }
                    Some((width, height)) => format!("embedded in the song at {width} x {height} px"),
                    None => "embedded in the song".to_string(),
                };
                match cover.headline.as_deref() {
                    Some(headline) => {
                        let detail = match cover.note.as_deref() {
                            Some(note) => format!("{note} Now {place}."),
                            None => format!("Now {place}."),
                        };
                        self.log_cover_step(song, headline, Some(&detail));
                    }
                    None => {
                        let detail = match embedded_side {
                            Some((width, height)) => format!("{width} x {height} px, embedded in the song"),
                            None => "Embedded in the song".to_string(),
                        };
                        self.log_cover_step(song, &format!("Cover from {}", cover.source), Some(&detail));
                    }
                }
                embed = Some(bytes);
            }
            chosen_cover = Some(cover.bytes);
        }

        let written = (|| {
            let mut file = TagFile::open(file_path)?;
            release_fact_tags::tidy(&mut file, filed_elsewhere, song.joins_album.as_ref());
            if stray_recording_id(&mut file, song).is_some() {
                tag_writer_extras::clear_recording_id(&mut file);
            }
            // A file whose sound proved no recording keeps no ISRC its source tagged it with.
            if song
                .verification
                .as_deref()
                .is_some_and(|verification| !verification.proof.allows_recording_tags())
            {
                tag_writer_extras::set_or_clear_text(&mut file, octo_media::tags::TagFields::ISRC, None);
            }
            write_song(&mut file, song, &genres, filed_elsewhere);
            if let Some(bytes) = embed {
                let mime = cover_image::mime_type(&bytes);
                embed_cover(&mut file, bytes, mime);
            }
            file.save()
        })();
        match written {
            Ok(()) => {
                info!("Metadata written successfully to: {file_path}");
                self.log_song_step(
                    song,
                    acquisition_event_kinds::TAGS,
                    "Tags written",
                    Some(file_path.rsplit('/').next().unwrap_or(file_path)),
                );
                chosen_cover
            }
            Err(e) => {
                error!("Failed to write metadata to: {file_path}: {e}");
                None
            }
        }
    }

    /// For a replacement of a library song (Better quality, wrong song, a picked copy): the
    /// cover of the copy being replaced, still in place while the new one is tagged. None for an
    /// ordinary download, or when that file cannot be read.
    fn library_copy_cover(&self, new_path: &str, replacing_path: Option<&str>) -> Option<Vec<u8>> {
        let original = replacing_path.filter(|path| !path.is_empty())?;
        if !Path::new(original).is_file() || get_full_path(original) == get_full_path(new_path) {
            return None;
        }
        match TagFile::open(original) {
            Ok(file) => front_cover(&file).map(|picture| picture.data),
            Err(e) => {
                debug!("Could not read the cover of {original}: {e}");
                None
            }
        }
    }

    /// One line in the download's log about its cover (`LogStep` with the cover kind). Tests
    /// also read the lines back from the service.
    fn log_cover_step(&self, song: &Song, text: &str, detail: Option<&str>) {
        self.log_song_step(song, acquisition_event_kinds::COVER, text, detail);
        #[cfg(test)]
        self.cover_steps
            .lock()
            .push((text.to_string(), detail.map(str::to_string)));
    }

    /// Once the walk ends, every track it placed gets the album-level tags the walk settled on,
    /// rewritten in place: a track tagged before the walk matched the album's own release was
    /// written without its facts, and two tracks that differ on one of them are two albums to
    /// the library server. Answers how many files were rewritten.
    pub fn settle_album_tags(&self, context: &AlbumTagContext<Loudness>, album_title: &str) -> usize {
        if self.settings.current().metadata.tag_rehearsal {
            return 0;
        }
        let mut rewritten = 0;
        for (path, song) in context.unsettled() {
            if !super::file_exists(&path) {
                continue;
            }
            match write_album_facts(Path::new(&path), &song) {
                Ok(()) => {
                    rewritten += 1;
                    debug!("{path} rewritten to the album tags of '{album_title}'");
                }
                Err(e) => warn!("Could not write the album's tags to {path}: {e}"),
            }
        }
        if rewritten > 0 {
            info!("Album '{album_title}': {rewritten} track(s) rewritten to the album's own tags");
        }
        rewritten
    }

    /// The album gain and peak, written into every file the walk measured, once the walk ends.
    /// Only when every track was measured: an album gain for half an album is worse than none.
    /// A rewrite in place, so the library server keeps each file's id.
    pub fn write_album_gain(&self, context: &AlbumTagContext<Loudness>, album_title: &str) {
        let measured = context.loudness();
        if measured.is_empty() {
            return;
        }
        let values: Vec<Option<Loudness>> = measured.iter().map(|(_, loudness)| *loudness).collect();
        let Some(album) = ReplayGainTags::for_album(&values) else {
            info!("No album gain for '{album_title}': not every track could be measured");
            return;
        };
        for (path, _) in &measured {
            if !super::file_exists(path) {
                continue;
            }
            if let Err(e) = write_album_gain(Path::new(path), album.gain_db, album.peak) {
                warn!("Could not write the album gain to {path}: {e}");
            }
        }
        info!(
            "Album gain {} (peak {}) written to {} tracks of '{album_title}'",
            album.gain_text(),
            album.peak_text(),
            measured.len()
        );
    }
}

/// One file of the album folder a song will join, and its facts as read.
struct AlbumSiblingFile {
    path: String,
    facts: FileFacts,
}

/// A single joining an album folder that is already there takes that album's release facts from
/// one of its files, so the album the library server shows keeps one label, one catalog number
/// and one year.
/// The recording id a peer's file arrived with when it is not the one the song is written as:
/// every tag a download writes belongs to one identity, so a title from the request never sits
/// beside a recording id from someone else's tagger.
fn stray_recording_id(file: &mut TagFile, song: &Song) -> Option<String> {
    let theirs = tag_writer_extras::read_recording_id(file)?;
    let ours = song
        .music_brainz_recording_id
        .as_deref()
        .filter(|id| !dotnet::is_blank(id));
    (!ours.is_some_and(|ours| dotnet::eq_ignore_case(ours, &theirs))).then_some(theirs)
}

fn pin_to_sibling(song: &mut Song, sibling: &AlbumSiblingFile) {
    let facts = &sibling.facts;
    if let Some(year) = facts.year.filter(|y| *y > 0) {
        song.year = Some(year);
    }
    if let Some(label) = facts.label.clone().filter(|l| !l.is_empty()) {
        song.label = Some(label);
    }
    if let Some(number) = facts.catalog_number.clone().filter(|n| !n.is_empty()) {
        song.catalog_number = Some(number);
    }
    if let Some(barcode) = facts.barcode.clone().filter(|b| !b.is_empty()) {
        song.barcode = Some(barcode);
    }
    if let Some(plan) = song.tag_plan.as_deref_mut() {
        plan.notes.push(format!(
            "album facts taken from the album folder's own '{}'",
            get_file_name(&sibling.path)
        ));
    }
}

/// The values the library server groups the folder's album by, read the way it reads them, so
/// the new track carries exactly the same ones and lands in that album rather than one of its
/// own. Not a matching decision, so a rehearsal keeps it too.
fn join_album(song: &mut Song, sibling: &AlbumSiblingFile) {
    let Some(kept) = KeptIdentityTags::read(Path::new(&sibling.path), None) else {
        return;
    };
    if kept.album_id.is_none() && kept.release_date.is_none() && kept.album_version.is_none() {
        return;
    }
    song.joins_album = Some(AlbumGrouping::new(
        kept.album_id,
        kept.release_date,
        kept.album_version,
    ));
    if let Some(plan) = song.tag_plan.as_deref_mut() {
        plan.notes.push(format!(
            "album grouping copied from the album folder's own '{}'",
            get_file_name(&sibling.path)
        ));
    }
    debug!("{} joins the album of {}", song.title, sibling.path);
}

/// A source's own album tag beats nothing, and beats filing the track under its title. Its
/// compilation flag counts only for that album: a song the chooser filed elsewhere is not a
/// compilation because the file it came from was ripped from one.
fn fill_album_from_file(song: &mut Song, file_path: &str) {
    let (album, album_artist, compilation) = tag_writer_extras::read_album(Path::new(file_path));
    let album_blank = album.as_deref().is_none_or(dotnet::is_blank);
    let same_album = dotnet::is_blank(&song.album)
        || album_blank
        || SongIdentity::key(&song.album) == SongIdentity::key(album.as_deref().unwrap_or(""));
    if same_album && (compilation || BaseDownloadService::is_various_artists(album_artist.as_deref())) {
        song.is_compilation = true;
    }
    if !dotnet::is_blank(&song.album) || album_blank {
        return;
    }
    song.album = album.as_deref().unwrap_or("").trim().to_string();
    if song.album_artist.as_deref().is_none_or(str::is_empty)
        && let Some(album_artist) = album_artist.as_deref().filter(|a| !dotnet::is_blank(a))
    {
        song.album_artist = Some(album_artist.trim().to_string());
    }
}

/// Drop a redundant leading "Artist - " from a track title.
fn strip_artist_prefix(artist: &str, title: &str) -> String {
    let t = title.trim();
    let a = artist.trim();
    let prefix = format!("{a} - ");
    if !a.is_empty() && dotnet::starts_with_ignore_case(t, &prefix) {
        // OrdinalIgnoreCase matches one character for one.
        return t
            .chars()
            .skip(prefix.chars().count())
            .collect::<String>()
            .trim()
            .to_string();
    }
    t.to_string()
}

/// `BaseDownloadService.FillBlanksFromCatalog`, for the tag preview.
#[derive(Debug, Clone, Copy, Default)]
pub struct CatalogBlanks;

impl CatalogBlankFiller for CatalogBlanks {
    fn fill_blanks_from_catalog(&self, song: &mut Song, meta: Option<&FullTrackMeta>) {
        BaseDownloadService::fill_blanks_from_catalog(song, meta);
    }
}

/// The loudness meter as the tag preview asks it: the measurement read through
/// `ReplayGainTags.ForTrack`.
pub struct MeterPreview(pub Arc<dyn ILoudnessMeter>);

#[async_trait]
impl PreviewLoudnessMeter for MeterPreview {
    async fn measure(
        &self,
        path: &str,
        timeout_seconds: i32,
        ct: &CancellationToken,
    ) -> anyhow::Result<Option<MeasuredLoudness>> {
        let measured = self.0.measure(Path::new(path), timeout_seconds, ct).await;
        Ok(measured.map(|loudness| MeasuredLoudness {
            integrated_lufs: loudness.integrated_lufs,
            true_peak_dbfs: loudness.true_peak_dbfs,
            replay_gain: ReplayGainTags::for_track(Some(&loudness)).map(|tags| ReplayGainText {
                gain_text: tags.gain_text(),
                peak_text: tags.peak_text(),
            }),
        }))
    }
}
