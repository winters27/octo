//! The version check, beside the library tag check: whether each library file is the version of
//! its song that its tags say, by its fingerprint, its own ids, the song's recordings in the
//! music database and, for a copy of another length, its fingerprint played back at the speed of
//! the tagged version (`octo_core::tagging::version_audit`); and whether its cover is another
//! artist's release's. What it finds waits in Library health for a person: retag the file as what
//! it is, replace it with the tagged version or its cover with the album's own, or keep it.
//! Nothing is changed by the check itself.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use octo_core::common::SongIdentity;
use octo_core::common::dotnet::{self, is_blank};
use octo_core::fingerprint::TrackMatchComparer;
use octo_core::fingerprint::acoust_id_client::AcoustIdLookup;
use octo_core::fingerprint::recording_identity::RecordingKind;
use octo_core::fingerprint::verification::VerificationResult;
use octo_core::settings::SettingsStore;
use octo_core::tagging::version_audit::{
    self, IsrcEntry, KnownRecording, Resampled, Sureness, VersionFacts, VersionFinding, VersionProblem,
};
use octo_media::audio::{AudioFingerprinter, FingerprintOutcome};
use octo_media::tags::tag_writer_extras;
use serde::{Deserialize, Serialize};
use tracing::debug;

use super::library_tag_edits::song_tag_fields as fields;
use crate::services::fingerprint::acoust_id_rate_limiter::{AcoustIdRateLimiter, LookupClass};
use crate::services::fingerprint::{AcoustIdClient, MusicBrainzClient};
use crate::services::metadata::DeezerMetadataService;

/// What the version check asks of the outside world, behind a seam so tests need no network.
#[async_trait]
pub trait VersionSources: Send + Sync {
    /// The song's recordings in the music database by this artist, every version of it.
    async fn song_recordings(&self, artist: &str, title: &str) -> Option<Vec<KnownRecording>>;
    /// What the catalog says an ISRC is.
    async fn isrc_entry(&self, isrc: &str) -> Option<IsrcEntry>;
    /// The music database's title and length for one recording.
    async fn recording(&self, recording_id: &str) -> Option<KnownRecording>;
    /// AcoustID's ids for the sound of one recording.
    async fn tracks_of(&self, recording_id: &str) -> Option<Vec<String>>;
    /// The file played back at `speed` times its speed, fingerprinted and looked up.
    async fn resampled_lookup(&self, path: &str, speed: f64) -> Option<AcoustIdLookup>;
    /// Every release a loose search for this artist and album offers a cover of, whoever made
    /// it, as (release, its artist, the cover's address).
    async fn cover_candidates(&self, _artist: &str, _album: &str) -> Vec<(String, String, String)> {
        Vec::new()
    }
    /// One picture, by its address.
    async fn image(&self, _url: &str) -> Option<Vec<u8>> {
        None
    }
}

/// The sources over MusicBrainz, the Deezer catalog and AcoustID, each in its background lane.
pub struct OnlineVersionSources {
    pub http: reqwest::Client,
    pub music_brainz: Arc<MusicBrainzClient>,
    pub deezer: Option<Arc<DeezerMetadataService>>,
    pub acoust_id: Arc<AcoustIdClient>,
    pub fingerprinter: Arc<AudioFingerprinter>,
    pub settings: Arc<SettingsStore>,
}

#[async_trait]
impl VersionSources for OnlineVersionSources {
    async fn song_recordings(&self, artist: &str, title: &str) -> Option<Vec<KnownRecording>> {
        let doc = self
            .music_brainz
            .song_recordings_in_background(artist, title)
            .await
            .ok()??;
        Some(recordings_by(&doc, artist))
    }

    async fn isrc_entry(&self, isrc: &str) -> Option<IsrcEntry> {
        let answer = self.deezer.as_ref()?.track_by_isrc(isrc).await;
        let track = answer.tracks.into_iter().next()?;
        Some(IsrcEntry {
            isrc: isrc.to_string(),
            title: track.title,
            seconds: track.duration,
        })
    }

    async fn recording(&self, recording_id: &str) -> Option<KnownRecording> {
        let facts = self
            .music_brainz
            .recording_facts(recording_id, true)
            .await
            .ok()??;
        Some(KnownRecording {
            id: facts.recording_id,
            title: facts.title,
            length_ms: facts.length_ms,
        })
    }

    async fn tracks_of(&self, recording_id: &str) -> Option<Vec<String>> {
        let settings = self.settings.current().soulseek.clone();
        let key = self.acoust_id.key_for(LookupClass::Bulk)?;
        AcoustIdRateLimiter::in_background(AcoustIdRateLimiter::with_class(
            LookupClass::Bulk,
            self.acoust_id.tracks_of_recording(
                &key,
                recording_id,
                settings.effective_acoust_id_timeout_seconds(),
            ),
        ))
        .await
    }

    async fn resampled_lookup(&self, path: &str, speed: f64) -> Option<AcoustIdLookup> {
        let settings = self.settings.current().soulseek.clone();
        let print = self
            .fingerprinter
            .fingerprint_at_speed(
                Path::new(path),
                speed,
                settings.effective_fingerprint_seconds(),
                settings.effective_fingerprint_timeout_seconds(),
            )
            .await;
        let fingerprint = print
            .fingerprint
            .filter(|f| print.outcome == FingerprintOutcome::Ok && !f.is_empty())?;
        let key = self.acoust_id.key_for(LookupClass::Bulk)?;
        AcoustIdRateLimiter::in_background(AcoustIdRateLimiter::with_class(
            LookupClass::Bulk,
            self.acoust_id.lookup(
                &key,
                &fingerprint,
                print.decoded_seconds,
                settings.effective_acoust_id_timeout_seconds(),
            ),
        ))
        .await
        .filter(|lookup| lookup.is_ok)
    }

    async fn cover_candidates(&self, artist: &str, album: &str) -> Vec<(String, String, String)> {
        match &self.deezer {
            Some(deezer) => deezer.release_covers(&format!("{artist} {album}")).await,
            None => Vec::new(),
        }
    }

    async fn image(&self, url: &str) -> Option<Vec<u8>> {
        let answer = self.http.get(url).send().await.ok()?;
        if !answer.status().is_success() {
            return None;
        }
        Some(answer.bytes().await.ok()?.to_vec())
    }
}

/// The recordings of a MusicBrainz recording search credited to `artist`.
pub fn recordings_by(doc: &serde_json::Value, artist: &str) -> Vec<KnownRecording> {
    let Some(found) = doc.get("recordings").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    found
        .iter()
        .filter_map(|recording| {
            let id = recording.get("id")?.as_str()?;
            let title = recording.get("title")?.as_str()?;
            let credits: Vec<String> = recording
                .get("artist-credit")
                .and_then(serde_json::Value::as_array)
                .map(|credits| {
                    credits
                        .iter()
                        .filter_map(|c| c.get("name").and_then(serde_json::Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let joined = credits.join(", ");
            if !TrackMatchComparer::artist_matches(artist, &joined, &credits) {
                return None;
            }
            let length_ms = recording
                .get("length")
                .and_then(serde_json::Value::as_i64)
                .filter(|ms| *ms > 0);
            Some(KnownRecording::new(id, title, length_ms))
        })
        .collect()
}

/// How many recordings of the tagged version AcoustID is asked about for their sound.
const TAGGED_TRACK_ASKS: usize = 3;

/// What the check needs of one file beyond the tag check's reading.
#[derive(Debug, Clone, Default)]
pub struct VersionInput {
    pub path: String,
    pub artist: String,
    pub album_artist: Option<String>,
    /// The picture the file carries as its front cover.
    pub cover: Option<Vec<u8>>,
    pub title: String,
    pub album: String,
    pub recording_id: Option<String>,
    pub isrcs: Vec<String>,
    pub file_ms: Option<i64>,
}

impl VersionInput {
    /// The ISRCs and the exact length, read from the file.
    pub fn read(
        path: &str,
        artist: &str,
        title: &str,
        album: &str,
        recording_id: Option<String>,
    ) -> VersionInput {
        let facts = tag_writer_extras::read_facts(path, true);
        let file = octo_media::tags::TagFile::open(path).ok();
        let file_ms = file
            .as_ref()
            .map(|file| file.duration().as_millis() as i64)
            .filter(|ms| *ms > 0);
        let cover = file
            .as_ref()
            .and_then(octo_media::tags::front_cover)
            .map(|picture| picture.data);
        VersionInput {
            path: path.to_string(),
            artist: artist.to_string(),
            album_artist: facts.album_artist.clone(),
            cover,
            title: title.to_string(),
            album: album.to_string(),
            recording_id,
            isrcs: facts.isrcs,
            file_ms,
        }
    }
}

/// The recordings of a lookup's best answer at or above the threshold: the ones of this song by
/// this artist, every AcoustID id of such answers, and whether anything was known at all.
fn heard_in(
    lookup: Option<&AcoustIdLookup>,
    threshold: f64,
    artist: &str,
    title: &str,
) -> (Vec<KnownRecording>, Vec<String>, bool) {
    let Some(lookup) = lookup else {
        return (Vec::new(), Vec::new(), false);
    };
    let mut results: Vec<_> = lookup.results.iter().filter(|r| r.score >= threshold).collect();
    results.sort_by(|a, b| b.score.total_cmp(&a.score));
    let tracks: Vec<String> = results.iter().filter_map(|r| r.id.clone()).collect();
    let mut heard: Vec<KnownRecording> = Vec::new();
    for recording in results.iter().flat_map(|r| &r.recordings) {
        let same_song = SongIdentity::same_title(title, &recording.title, None).verdict
            != octo_core::common::SongVerdict::Different;
        if same_song
            && TrackMatchComparer::artist_matches(artist, &recording.artist_credit(), &recording.artists)
            && !heard
                .iter()
                .any(|h| dotnet::eq_ignore_case(&h.id, &recording.recording_id))
        {
            heard.push(KnownRecording::new(
                &recording.recording_id,
                &recording.title,
                recording
                    .duration_ms
                    .or_else(|| recording.duration_seconds.map(|s| i64::from(s) * 1000)),
            ));
        }
    }
    (heard, tracks, !results.is_empty())
}

/// Everything the check gathers about one file, from its fingerprint's answer and the sources.
pub async fn gather(
    sources: &dyn VersionSources,
    input: &VersionInput,
    verdict: &VerificationResult,
    threshold: f64,
) -> VersionFacts {
    let artist = SongIdentity::primary_artist(&input.artist);
    let (heard, heard_tracks, heard_anything) =
        heard_in(verdict.lookup.as_ref(), threshold, &artist, &input.title);
    let mut facts = VersionFacts {
        title: input.title.clone(),
        artist: artist.clone(),
        album: input.album.clone(),
        file_ms: input.file_ms,
        heard,
        heard_anything,
        heard_tracks,
        ..VersionFacts::default()
    };
    if let Some(id) = input.recording_id.as_deref().filter(|id| !is_blank(id)) {
        facts.tagged_recording = match facts.heard.iter().find(|h| dotnet::eq_ignore_case(&h.id, id)) {
            Some(heard) => Some(heard.clone()),
            None => sources.recording(id).await,
        };
    }
    if let Some(isrc) = input.isrcs.first() {
        facts.isrc = sources.isrc_entry(isrc).await;
    }
    let core = SongIdentity::parse_title(&input.title, Some(&artist)).core;
    if !is_blank(&core) {
        facts.catalog = sources.song_recordings(&artist, &core).await.unwrap_or_default();
    }
    let heard_tagged = facts
        .heard
        .iter()
        .any(|r| version_audit::is_tagged_version(&facts.title, r));
    if !heard_tagged && heard_anything {
        let tagged: Vec<String> = facts
            .catalog
            .iter()
            .filter(|r| version_audit::is_tagged_version(&facts.title, r))
            .take(TAGGED_TRACK_ASKS)
            .map(|r| r.id.clone())
            .collect();
        if !tagged.is_empty() {
            let mut tracks: Vec<String> = Vec::new();
            let mut answered = false;
            for id in tagged {
                if let Some(found) = sources.tracks_of(&id).await {
                    answered = true;
                    tracks.extend(found);
                }
            }
            facts.tagged_tracks = answered.then_some(tracks);
        }
    }
    if facts.heard.is_empty()
        && let Some(ratio) = version_audit::speed_ratio_to_try(&facts)
        && let Some(lookup) = sources.resampled_lookup(&input.path, ratio).await
    {
        let (again, _, _) = heard_in(Some(&lookup), threshold, &artist, &input.title);
        if let Some(recording) = again.into_iter().next() {
            facts.resampled = Some(Resampled { ratio, recording });
        } else {
            debug!(
                "the version check's resampled fingerprint of {} named nothing of the song",
                input.path
            );
        }
    }
    facts
}

/// How many covers of other artists' releases one check downloads to hold against a file's.
const COVER_LOOKS: usize = 6;

/// The file's cover, when it is another artist's release's: a loose search for the album offers
/// that release, and its cover looks like the file's. None when the file has no cover, when the
/// cover is the album's own (a release of the album by its artist looks like it), or when nothing
/// tells.
pub async fn check_cover(sources: &dyn VersionSources, input: &VersionInput) -> Option<KeptCover> {
    use octo_core::common::cover_names::release_fits;
    use octo_media::cover::cover_image::{look_alike, looks_hash};
    let shown = looks_hash(input.cover.as_deref())?;
    let album = if is_blank(&input.album) {
        &input.title
    } else {
        &input.album
    };
    let mut wanted: Vec<String> = Vec::new();
    for name in [input.album_artist.as_deref(), Some(input.artist.as_str())]
        .into_iter()
        .flatten()
    {
        let primary = SongIdentity::primary_artist(name);
        for candidate in [name.to_string(), primary] {
            if !is_blank(&candidate) && !wanted.contains(&candidate) {
                wanted.push(candidate);
            }
        }
    }
    let want: Vec<&str> = wanted.iter().map(String::as_str).collect();
    let mut offered: Vec<(String, String, String)> = Vec::new();
    for artist in &want {
        for found in sources.cover_candidates(artist, album).await {
            if !offered.iter().any(|seen| seen.2 == found.2) {
                offered.push(found);
            }
        }
    }
    let (own, others): (Vec<_>, Vec<_>) = offered
        .into_iter()
        .partition(|(title, artist, _)| release_fits(album, &want, title, artist));
    for (_, _, url) in own.iter().take(2) {
        if let Some(bytes) = sources.image(url).await
            && looks_hash(Some(&bytes)).is_some_and(|hash| look_alike(hash, shown))
        {
            return None;
        }
    }
    for (title, artist, url) in others.into_iter().take(COVER_LOOKS) {
        let Some(bytes) = sources.image(&url).await else {
            continue;
        };
        if looks_hash(Some(&bytes)).is_some_and(|hash| look_alike(hash, shown)) {
            let by = input.album_artist.as_deref().unwrap_or(&input.artist);
            return Some(KeptCover {
                words: format!(
                    "The cover is the one of '{title}' by {artist}, another artist's release, not of '{album}' by {by}."
                ),
                release_title: title,
                release_artist: artist,
                kept: false,
            });
        }
    }
    None
}

/// A cover finding, as identity-sweep.json keeps it beside the song.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct KeptCover {
    pub words: String,
    /// The other artist's release the cover is from, and its artist.
    pub release_title: String,
    pub release_artist: String,
    /// A person kept the cover as it is; asked again only once the file changes.
    pub kept: bool,
}

/// One version finding, as identity-sweep.json keeps it beside the song.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct KeptVersion {
    /// "otherVersion", "speed" or "lengthOff".
    pub problem: String,
    /// The audio itself says so; otherwise the lengths and names do.
    pub heard: bool,
    pub words: String,
    /// The recording the file really is, when known.
    pub is_id: Option<String>,
    pub is_title: Option<String>,
    /// The take a sped-up or slowed copy is, by its title word.
    pub kind: Option<String>,
    /// The tagged version's length, which a replacement is held to.
    pub expected_seconds: Option<i32>,
    /// The tags "Retag as what it is" writes; empty when it cannot say what the file is.
    pub retag: IndexMap<String, Option<String>>,
    /// A person kept the file as it is; asked again only once the file changes.
    pub kept: bool,
}

impl KeptVersion {
    pub fn problem(&self) -> Option<VersionProblem> {
        VersionProblem::parse(&self.problem)
    }
}

/// The finding as kept, with the tags that retag the file as what it is: the recording's own
/// title and id, and no ISRC of the tagged version; a sped-up or slowed copy says so in its title
/// and carries no recording's ids.
pub fn kept_of(finding: &VersionFinding, input: &VersionInput) -> KeptVersion {
    let mut retag: IndexMap<String, Option<String>> = IndexMap::new();
    match (finding.problem, &finding.is, finding.kind) {
        (VersionProblem::OtherVersion, Some(is), _) => {
            retag.insert(fields::TITLE.to_string(), Some(is.title.clone()));
            retag.insert(fields::RECORDING_ID.to_string(), Some(is.id.clone()));
            if finding.isrc_belongs_elsewhere && !input.isrcs.is_empty() {
                retag.insert(fields::ISRC.to_string(), None);
            }
        }
        (VersionProblem::Speed, _, Some(kind)) => {
            if let Some(word) = kind.title_word()
                && !RecordingKind::marked_in(&input.title).contains(&kind)
            {
                retag.insert(
                    fields::TITLE.to_string(),
                    Some(format!("{} ({word})", input.title.trim())),
                );
            }
            if input.recording_id.as_deref().is_some_and(|id| !is_blank(id)) {
                retag.insert(fields::RECORDING_ID.to_string(), None);
            }
            if !input.isrcs.is_empty() {
                retag.insert(fields::ISRC.to_string(), None);
            }
        }
        _ => {}
    }
    KeptVersion {
        problem: finding.problem.name().to_string(),
        heard: finding.sureness == Sureness::Heard,
        words: finding.words.clone(),
        is_id: finding.is.as_ref().map(|is| is.id.clone()),
        is_title: finding.is.as_ref().map(|is| is.title.clone()),
        kind: finding
            .kind
            .and_then(RecordingKind::title_word)
            .map(str::to_string),
        expected_seconds: finding.expected_seconds,
        retag,
        kept: false,
    }
}

/// Checks one song's version now, for the Check version action: what was found, in words, or
/// why it could not be checked.
pub trait VersionChecker: Send + Sync {
    fn check<'a>(&'a self, navidrome_id: &'a str) -> futures::future::BoxFuture<'a, Result<String, String>>;
}

/// The Check version playlist's description: what adding a song does, and what the last few
/// songs added were found to be.
pub fn check_playlist_comment(recent: &[String]) -> String {
    let mut text = "Add a song here to check that its file is the version its tags say: not a remix, a sped-up or slowed copy or another take. Octo checks it, takes it out of here and lists what it finds in Library health on its dashboard, where it can be retagged, replaced or kept. To replace one from any app, add it to Wrong version.".to_string();
    if !recent.is_empty() {
        text.push_str(" Last checked: ");
        text.push_str(&recent.join(" | "));
    }
    text
}

#[cfg(test)]
#[path = "version_check_tests.rs"]
mod tests;
