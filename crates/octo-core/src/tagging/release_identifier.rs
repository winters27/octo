//! Works out which release a downloaded file is.
//!
//! This crate cannot name the services that answer, so each call the identifier makes goes
//! through a trait defined here, which the concrete services in the `octo` and `octo-media`
//! crates implement:
//!
//! - [`ReleaseLookup`]: the MusicBrainz client (release lookup, recording search, ISRC lookup);
//! - [`CatalogLookup`]: the Deezer metadata service's ranked track candidates;
//! - [`FileFactsReader`]: the tag reader, for what a file on disk says about itself.
//!
//! Budgets: each call is a future dropped when its budget runs out (`tokio::time::timeout`) or
//! the caller's token is canceled, so the traits take no token.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::common::dotnet::{eq_ignore_case, is_blank};
use crate::common::song_identity::SongIdentity;
use crate::fingerprint::acoust_id_client::AcoustIdLookup;
use crate::fingerprint::recording_identity::{self, Identity, RecordingKind};
use crate::metadata::deezer_metadata_service::{CatalogCandidates, FullTrackMeta};
use crate::models::domain::song::Song;
use crate::settings::SettingsStore;
use crate::tagging::album_tag_context::AlbumTagContext;
use crate::tagging::candidate_sources::CandidateSources;
use crate::tagging::matching_settings::MatchingSettings;
use crate::tagging::net::cmp_ordinal;
use crate::tagging::release_chooser::ReleaseChooser;
use crate::tagging::release_details::ReleaseDetails;
use crate::tagging::tag_evidence::{
    FileFacts, IgnoreCaseSet, ReleaseCandidate, TagEvidence, TagRequest, TagSource,
};
use crate::tagging::tag_plan::{TagConfidence, TagPlan};

/// The music database calls the identifier makes. Implemented by the `octo` crate's MusicBrainz
/// client.
#[async_trait]
pub trait ReleaseLookup: Send + Sync {
    /// The release's details, None when the database has no answer.
    async fn lookup_release(&self, release_id: &str) -> anyhow::Result<Option<ReleaseDetails>>;

    /// The recording search's JSON answer, None when there is none.
    async fn search_recordings(
        &self,
        artist: &str,
        title: &str,
        duration_seconds: i32,
    ) -> anyhow::Result<Option<Value>>;

    /// The recordings that carry the code, each with its releases (the shape of a recording
    /// search's answer, plus `isrc`); None when the database knows no recording with it; an
    /// error when the database could not be asked.
    async fn lookup_isrc(&self, isrc: &str) -> anyhow::Result<Option<Value>>;
}

/// The catalog call the identifier makes. Implemented by the `octo` crate's Deezer metadata
/// service.
#[async_trait]
pub trait CatalogLookup: Send + Sync {
    /// The ranked hits for one song, the best `max` of them with their details.
    async fn enrich_track_candidates(
        &self,
        artist: &str,
        title: &str,
        max: i32,
    ) -> anyhow::Result<CatalogCandidates>;
}

/// What a file on disk says about itself. Implemented by the tag reader in `octo-media`. A
/// file that cannot be read is `FileFacts::unknown`.
pub trait FileFactsReader: Send + Sync {
    fn read_facts(&self, path: &str, tags_are_evidence: bool) -> FileFacts;

    /// What the download writer's `release_fact_tags::tidy` would take off the file for a song
    /// filed under `album`, worked out on the file in memory and never saved. Nothing when the
    /// file cannot be read.
    fn would_take_off(&self, _path: &str, _album: &str) -> Vec<String> {
        Vec::new()
    }
}

/// The caller's token was canceled while the release lookup was being asked, the one
/// failure [`ReleaseIdentifier::identify`] passes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("The operation was canceled.")]
pub struct Cancelled;

/// Why one bounded call gave nothing.
enum Failure {
    /// The caller's token.
    Cancelled,
    /// The call's own budget.
    TimedOut,
    Error(anyhow::Error),
}

impl Failure {
    fn message(&self) -> String {
        match self {
            Failure::Cancelled => Cancelled.to_string(),
            Failure::TimedOut => "The operation has timed out.".to_string(),
            Failure::Error(error) => error.to_string(),
        }
    }
}

/// Run one call until it answers, its deadline passes, or the caller's token is canceled.
async fn bounded<T>(
    ct: &CancellationToken,
    deadline: Instant,
    call: impl Future<Output = anyhow::Result<T>>,
) -> Result<T, Failure> {
    tokio::select! {
        biased;
        _ = ct.cancelled() => Err(Failure::Cancelled),
        answer = tokio::time::timeout_at(deadline, call) => match answer {
            Err(_) => Err(Failure::TimedOut),
            Ok(Err(error)) => Err(Failure::Error(error)),
            Ok(Ok(value)) => Ok(value),
        },
    }
}

/// Works out what a downloaded file is, by every source that can say: the fingerprint service's
/// answer (already paid for by verification), the catalog's ranked hits, the file's own tags,
/// and the music database when the fingerprint named nothing. Every candidate is weighed
/// against what was asked for and what landed, and the winner's release is looked up for the
/// facts only the database has. Every external call has its own cap; a timeout costs fields,
/// never the download. Reads only: the plan it returns is applied by the caller.
pub struct ReleaseIdentifier {
    settings: Arc<SettingsStore>,
    facts: Arc<dyn FileFactsReader>,
    music_brainz: Option<Arc<dyn ReleaseLookup>>,
    deezer: Option<Arc<dyn CatalogLookup>>,
    /// How long the code question may take within the database stage.
    code_budget: Duration,
}

impl ReleaseIdentifier {
    pub const CATALOG_BUDGET: Duration = Duration::from_secs(12);
    pub const DATABASE_BUDGET: Duration = Duration::from_secs(15);
    /// The most the code question may take of [`Self::DATABASE_BUDGET`], so a slow answer
    /// still leaves the search by name its time.
    pub const CODE_BUDGET: Duration = Duration::from_secs(8);

    /// How far a recording the code names may be from the file's length and still settle what
    /// the file is without a search by name: the length an owned copy may differ by.
    pub const CODE_LENGTH_TOLERANCE_SECONDS: i32 = 8;
    pub const DETAILS_BUDGET: Duration = Duration::from_secs(10);
    pub const CATALOG_CANDIDATES: i32 = 2;

    /// `music_brainz` and `deezer` are None when no such service is available.
    pub fn new(
        settings: Arc<SettingsStore>,
        facts: Arc<dyn FileFactsReader>,
        music_brainz: Option<Arc<dyn ReleaseLookup>>,
        deezer: Option<Arc<dyn CatalogLookup>>,
    ) -> Self {
        Self {
            settings,
            facts,
            music_brainz,
            deezer,
            code_budget: Self::CODE_BUDGET,
        }
    }

    /// The identifier with another time for the code question, for tests.
    pub fn with_code_budget(mut self, budget: Duration) -> Self {
        self.code_budget = budget;
        self
    }

    /// The request as evidence: what was asked for, before anything corrected it.
    pub fn request_for(
        song: &Song,
        artist: &str,
        title: &str,
        album: Option<&str>,
        track: Option<i32>,
    ) -> TagRequest {
        let parsed = SongIdentity::parse_title(title, None);
        TagRequest {
            artist: artist.to_string(),
            title: title.to_string(),
            album: album.filter(|a| !is_blank(a)).map(str::to_string),
            track,
            disc: song.disc_number,
            duration_seconds: song.duration,
            isrc: song.isrc.as_deref().and_then(SongIdentity::normalize_isrc),
            catalog_album_id: Self::catalog_album_id_of(song.album_id.as_deref()),
            catalog_track_id: song.external_id.clone(),
            version_markers: SongIdentity::distinct_versions(&parsed, None),
            whole_album: false,
            album_group_id: None,
            album_group_title: None,
        }
    }

    /// The request with the group a no-album pick chose, when it asked for no album.
    pub fn with_album_group(mut request: TagRequest, group: Option<&(String, String)>) -> TagRequest {
        if let Some((id, title)) = group.filter(|_| !request.owns_album()) {
            request.album_group_id = Some(id.clone());
            request.album_group_title = Some(title.clone());
        }
        request
    }

    /// "ext-deezer-album-123456" is the catalog's 123456.
    fn catalog_album_id_of(album_id: Option<&str>) -> Option<String> {
        let album_id = album_id.filter(|id| !id.is_empty())?;
        Some(match album_id.rfind('-') {
            Some(at) if at + 1 < album_id.len() => album_id[at + 1..].to_string(),
            _ => album_id.to_string(),
        })
    }

    /// Identify one file. `tags_are_evidence` is false for a file whose tags are an uploader's,
    /// not a peer's. The album context, when the file is one track of a walk, keeps the choice
    /// on the walk's album and steers it onto the walk's release.
    ///
    /// Fails only when `ct` is canceled while the release lookup is being asked; a cancellation
    /// during the catalog or database stage only costs their candidates.
    pub async fn identify<L: Clone>(
        &self,
        song: &Song,
        request: TagRequest,
        file_path: &str,
        tags_are_evidence: bool,
        album: Option<&AlbumTagContext<L>>,
        ct: &CancellationToken,
    ) -> Result<TagPlan, Cancelled> {
        let started = Instant::now();
        let current = self.settings.current();
        let metadata = &current.metadata;
        let settings = MatchingSettings::from_settings(metadata, &current.soulseek);
        let mut notes: Vec<String> = Vec::new();
        let mut stages: IndexMap<&'static str, f64> = IndexMap::new();

        let file = self.facts.read_facts(file_path, tags_are_evidence);
        let lookup = song.verification.as_ref().and_then(|v| v.lookup.as_ref());
        let threshold = settings.fingerprint_threshold;
        let fingerprinted = Self::fingerprinted_ids(lookup, threshold);
        let identity = Identity::of(song.verification.as_deref());
        let mut request = Self::without_contradicted_album(request, identity.as_ref(), &mut notes);
        if let Some(album) = album {
            request.whole_album = request
                .album
                .as_deref()
                .is_some_and(|asked| SongIdentity::key(asked) == SongIdentity::key(album.album_title()));
        }
        let mut candidates = CandidateSources::from_fingerprint(lookup, threshold);
        if let (Some(identity), Some(lookup)) = (&identity, lookup) {
            Self::keep_identified_takes(&mut candidates, identity, lookup, &mut notes);
        }

        let music_brainz = self
            .music_brainz
            .clone()
            .filter(|_| metadata.release_details_lookup);

        // The likeliest release's details are asked for while the catalog is still answering,
        // so the common case pays for neither in series.
        let mut prefetch = None;
        if let Some(client) = &music_brainz
            && let Some(likely) = Self::pre_score(&candidates)
        {
            let (client, id, token) = (Arc::clone(client), likely.clone(), ct.clone());
            let task = tokio::spawn(async move { Self::lookup_details(client.as_ref(), &id, &token).await });
            prefetch = Some((likely, task));
        }

        let mut catalog_best: Option<FullTrackMeta> = None;
        if let Some(deezer) = &self.deezer {
            let clock = Instant::now();
            let call =
                deezer.enrich_track_candidates(&request.artist, &request.title, Self::CATALOG_CANDIDATES);
            match bounded(ct, Instant::now() + Self::CATALOG_BUDGET, call).await {
                Ok(answer) => {
                    if answer.did_not_answer {
                        notes.push("the catalog did not answer".to_string());
                    }
                    for hit in answer.hits {
                        candidates.push(CandidateSources::from_catalog(&hit, Some(&request.title)));
                        if catalog_best.is_none() {
                            catalog_best = Some(hit);
                        }
                    }
                }
                Err(Failure::TimedOut) => notes.push(format!(
                    "the catalog did not answer in {} s",
                    Self::CATALOG_BUDGET.as_secs()
                )),
                Err(failure) => {
                    notes.push("the catalog could not be asked".to_string());
                    debug!(
                        "catalog candidates failed for '{} - {}': {}",
                        request.artist,
                        request.title,
                        failure.message()
                    );
                }
            }
            stages.insert("catalog", clock.elapsed().as_secs_f64());
        }

        if let Some(from_file) = CandidateSources::from_file_tags(&file, Some(&request)) {
            candidates.push(from_file);
        }

        // The music database is asked by name only when the fingerprint named nothing: it is a
        // second a call, and the fingerprint's answer is better evidence than a name search.
        if let Some(client) = &music_brainz
            && !candidates.iter().any(|c| c.source == TagSource::Fingerprint)
        {
            let clock = Instant::now();
            let deadline = Instant::now() + Self::DATABASE_BUDGET;
            let asked: Result<(), Failure> = async {
                if let Some(isrc) = &request.isrc {
                    let code_deadline = (Instant::now() + self.code_budget).min(deadline);
                    match bounded(ct, code_deadline, client.lookup_isrc(isrc)).await {
                        Ok(Some(doc)) => candidates.extend(CandidateSources::from_isrc_lookup(&doc)),
                        Ok(None) => notes
                            .push("the music database's search found no recording with the code".to_string()),
                        Err(Failure::Error(error)) => {
                            notes.push("the music database could not be asked the code lookup".to_string());
                            debug!("code lookup failed for '{}': {}", isrc, error);
                        }
                        Err(Failure::TimedOut) => notes.push(format!(
                            "the music database did not answer the code lookup in {} s",
                            self.code_budget.as_secs()
                        )),
                        Err(failure) => return Err(failure),
                    }
                }
                // The code settles the file only through a release of the same song, of a take as
                // long as the file.
                let asked_key = SongIdentity::parse_title(&request.title, None).key;
                let settled = candidates.iter().any(|c| {
                    c.source == TagSource::Database
                        && c.release_id.is_some()
                        && SongIdentity::parse_title(&c.recording_title, None).key == asked_key
                        && match (c.length_seconds, file.duration_seconds) {
                            (Some(length), file_length) if length > 0 && file_length > 0 => {
                                (length - file_length).abs() <= Self::CODE_LENGTH_TOLERANCE_SECONDS
                            }
                            _ => true,
                        }
                });
                if !settled {
                    let search =
                        client.search_recordings(&request.artist, &request.title, file.duration_seconds);
                    match bounded(ct, deadline, search).await? {
                        Some(doc) => candidates.extend(CandidateSources::from_database_search(&doc)),
                        None => notes.push("the music database did not answer the search".to_string()),
                    }
                }
                Ok(())
            }
            .await;
            match asked {
                Ok(()) => {}
                Err(Failure::TimedOut) => notes.push(format!(
                    "the music database did not answer in {} s",
                    Self::DATABASE_BUDGET.as_secs()
                )),
                Err(failure) => {
                    notes.push("the music database could not be asked".to_string());
                    debug!(
                        "database candidates failed for '{} - {}': {}",
                        request.artist,
                        request.title,
                        failure.message()
                    );
                }
            }
            stages.insert("database", clock.elapsed().as_secs_f64());
        }

        let evidence = TagEvidence {
            request,
            file,
            fingerprint_threshold: threshold,
            fingerprinted_recording_ids: fingerprinted,
            identity,
        };
        let mut plan = ReleaseChooser::choose(&evidence, &candidates, &settings, None);
        plan.catalog_best = catalog_best;
        plan.notes.extend(notes);
        if let Some(album) = album {
            album.keep_on_album(&mut plan);
            album.prefer_settled_release(&mut plan);
        }

        // The release's own facts, for a winner the fingerprint service or the music database named.
        let wanted = plan
            .chosen
            .as_ref()
            .map(|chosen| &chosen.candidate)
            .filter(|c| matches!(c.source, TagSource::Fingerprint | TagSource::Database))
            .and_then(|c| c.release_id.clone())
            .filter(|id| !id.is_empty())
            .filter(|_| {
                matches!(
                    plan.confidence,
                    TagConfidence::Strong | TagConfidence::Medium | TagConfidence::Ambiguous
                )
            });
        if let (Some(client), Some(release_id)) = (&music_brainz, wanted) {
            let clock = Instant::now();
            let answer = match prefetch.take() {
                Some((id, task)) if eq_ignore_case(&id, &release_id) => {
                    let answer = match task.await {
                        Ok(answer) => answer,
                        Err(join) => Err(Failure::Error(anyhow::anyhow!(
                            "the release lookup failed: {join}"
                        ))),
                    };
                    if let Ok(details) = &answer {
                        plan.details_prefetch_hit = details.is_some();
                    }
                    answer
                }
                _ => Self::lookup_details(client.as_ref(), &release_id, ct).await,
            };
            let details = match answer {
                Ok(details) => details,
                Err(Failure::Cancelled) => return Err(Cancelled),
                Err(failure) => {
                    debug!("release details failed for {}: {}", release_id, failure.message());
                    None
                }
            };
            match details {
                Some(details) => plan.with(details),
                None => plan.notes.push(
                    "the music database did not answer the release lookup; label, catalogue number and barcode may be missing"
                        .to_string(),
                ),
            }
            stages.insert("details", clock.elapsed().as_secs_f64());
        }
        // A prefetch nobody needs is left to finish (or time out) on its own.
        drop(prefetch);

        for (stage, seconds) in stages {
            plan.stage_seconds.insert(stage.to_string(), seconds);
        }
        plan.stage_seconds
            .insert("identify".to_string(), started.elapsed().as_secs_f64());
        Ok(plan)
    }

    /// The request without an album that names a take the audio is not: a replacement held to
    /// "The Black Parade (instrumentals)" whose audio is the studio take is not tagged onto the
    /// promo of instrumentals. The album is a hint about the file, never a wish for a take.
    pub fn without_contradicted_album(
        mut request: TagRequest,
        identity: Option<&Identity>,
        notes: &mut Vec<String>,
    ) -> TagRequest {
        let (Some(identity), Some(album)) = (identity, request.album.clone()) else {
            return request;
        };
        let asked = RecordingKind::requested_by(&request.title);
        if let Some(named) = identity.contradicted_by(&album, &asked) {
            notes.push(format!(
                "the album '{album}' names {}, which this file is not, so it was not used",
                named.described()
            ));
            request.album = None;
            request.track = None;
            request.disc = None;
        }
        request
    }

    /// Leaves out the fingerprint's releases of recordings the audio is not: every other
    /// recording when the exact length chose this one, otherwise those of another kind of take.
    /// The album then always comes from the recording the file is.
    pub fn keep_identified_takes(
        candidates: &mut Vec<ReleaseCandidate>,
        identity: &Identity,
        lookup: &AcoustIdLookup,
        notes: &mut Vec<String>,
    ) {
        let kinds: std::collections::HashMap<String, RecordingKind> = lookup
            .results
            .iter()
            .flat_map(|result| &result.recordings)
            .map(|recording| {
                (
                    crate::common::dotnet::to_lower_invariant(&recording.recording_id),
                    recording_identity::kind_of(recording, None),
                )
            })
            .collect();
        let before = candidates.len();
        candidates.retain(|candidate| {
            candidate.source != TagSource::Fingerprint
                || candidate.recording_id.as_deref().is_none_or(|id| {
                    let kind = kinds
                        .get(&crate::common::dotnet::to_lower_invariant(id))
                        .copied()
                        .unwrap_or(RecordingKind::Studio);
                    identity.admits(id, kind)
                })
        });
        let dropped = before - candidates.len();
        if dropped > 0 {
            notes.push(format!(
                "the audio is {} ({}); {dropped} releases of other takes were left out",
                identity.kind.described(),
                identity.recording_id
            ));
        }
    }

    /// The release lookup within its own budget: None when the budget runs out.
    async fn lookup_details(
        client: &dyn ReleaseLookup,
        release_id: &str,
        ct: &CancellationToken,
    ) -> Result<Option<ReleaseDetails>, Failure> {
        match bounded(
            ct,
            Instant::now() + Self::DETAILS_BUDGET,
            client.lookup_release(release_id),
        )
        .await
        {
            Err(Failure::TimedOut) => Ok(None),
            other => other,
        }
    }

    /// The recordings the fingerprint service named at or above the threshold.
    pub fn fingerprinted_ids(lookup: Option<&AcoustIdLookup>, threshold: f64) -> IgnoreCaseSet {
        let Some(lookup) = lookup.filter(|l| l.is_ok) else {
            return IgnoreCaseSet::new();
        };
        lookup
            .results
            .iter()
            .filter(|r| r.score >= threshold)
            .flat_map(|r| &r.recordings)
            .map(|r| r.recording_id.as_str())
            .filter(|id| !id.is_empty())
            .collect()
    }

    /// A cheap guess at the release the chooser will pick, for the details prefetch: a plain
    /// album over anything else, then the earliest first release, then the earliest pressing.
    pub fn pre_score(fingerprinted: &[ReleaseCandidate]) -> Option<String> {
        let rank = |c: &ReleaseCandidate| {
            let primary = c.primary_type.as_deref().unwrap_or("");
            if eq_ignore_case(primary, "Album") && c.secondary_types.is_empty() {
                0
            } else if eq_ignore_case(primary, "Single") || eq_ignore_case(primary, "EP") {
                1
            } else {
                2
            }
        };
        let mut pool: Vec<&ReleaseCandidate> = fingerprinted
            .iter()
            .filter(|c| {
                c.source == TagSource::Fingerprint && c.release_id.as_deref().is_some_and(|id| !id.is_empty())
            })
            .collect();
        pool.sort_by(|a, b| {
            rank(a)
                .cmp(&rank(b))
                .then_with(|| {
                    cmp_ordinal(
                        a.group_first_release_date.as_deref().unwrap_or("9999"),
                        b.group_first_release_date.as_deref().unwrap_or("9999"),
                    )
                })
                .then_with(|| {
                    cmp_ordinal(
                        a.release_date.as_deref().unwrap_or("9999"),
                        b.release_date.as_deref().unwrap_or("9999"),
                    )
                })
        });
        pool.first().and_then(|c| c.release_id.clone())
    }
}

#[cfg(test)]
#[path = "release_identifier_tests.rs"]
mod tests;
