//! The Mornye source of the download service. Mornye is asked beside Soulseek, in the
//! order its settings give: it finds the song in its catalog, Octo picks the track that is the
//! song (its ISRC, or its title, artist, version and length), Mornye saves it into a job
//! folder under the incoming dot folder, and the file meets the same checks as a Soulseek copy
//! (the length, the version its own title names, AcoustID, the spectrum) before the base places
//! and tags it. A file that fails a check is thrown away and the next source is asked.

use std::time::Duration;

use octo_core::common::version_gap::{Against, other_version};
use octo_core::settings::SpotiflacSettings;
use octo_core::spotiflac::{
    Job, JobState, Pick, Wanted, path_for_spotiflac, pick_audio_file, pick_track, search_queries,
    with_query_isrc,
};

use super::*;
use crate::services::spotiflac::{
    CheckSong, Connection, MornyeWaiting, SPOTIFLAC_FOLDER, SpotiflacClient, make_shared_folder,
    mornye_waited,
};
use octo_core::spotiflac::ExtensionsAnswer;
use octo_core::spotiflac::check::{FailureFacts, failure_words, named_provider};

/// The source's name, as the downloads log and the fetched-songs list show it. Rows written before 2026.10.25 keep "SpotiFLAC".
pub const SPOTIFLAC_SOURCE: &str = "Mornye";

/// How many failed calls in a row end the wait for a job.
const CALLS_THAT_MAY_FAIL: u32 = 5;

impl SoulseekDownloadService {
    /// Mornye's client, when Mornye is on.
    pub(super) fn spotiflac_on(&self) -> Option<Arc<SpotiflacClient>> {
        self.spotiflac.clone().filter(|client| client.is_on())
    }

    /// The lossless sources, Soulseek and Mornye, in the order Mornye's placement gives.
    /// With Mornye off this is Soulseek alone; with Soulseek off, Mornye alone.
    pub(super) async fn download_lossless(
        &self,
        base: &BaseDownloadService,
        routing: &SoulseekRouting,
        download: &mut TrackDownload,
        profile: &SearchProfile,
        ct: &CancellationToken,
    ) -> anyhow::Result<String> {
        let soulseek_on = self.slskd.is_on();
        let Some(client) = self.spotiflac_on() else {
            return self
                .download_via_soulseek(base, routing, download, profile, ct)
                .await;
        };
        let names = [
            routing.artist.as_deref(),
            routing.title.as_deref(),
            routing.album.as_deref(),
        ];
        let track_key = download.song.external_id.clone().unwrap_or_default();
        if !soulseek_on || client.settings().goes_first(&names) {
            let first = self
                .download_via_spotiflac(base, &client, routing, download, ct)
                .await;
            return match first {
                Err(e) if soulseek_on && !ct.is_cancelled() && goes_on(&e) => {
                    info!("Mornye brought nothing ({e}); asking Soulseek");
                    base.log_step(
                        &track_key,
                        acquisition_event_kinds::NOTE,
                        "Mornye brought no copy; asking Soulseek",
                        user_safe(Some(&e.to_string())).as_deref(),
                    );
                    let waited = mornye_waited(&e).cloned();
                    match self
                        .download_via_soulseek(base, routing, download, profile, ct)
                        .await
                    {
                        // Soulseek had none either: the song is still one Mornye's check keeps. A
                        // file that waits in the hold path, or a play nothing proved, keeps its own.
                        Err(other) if !ct.is_cancelled() && goes_on(&other) => match waited {
                            Some(wait) => Err(wait.over(other)),
                            None => Err(other),
                        },
                        result => result,
                    }
                }
                other => other,
            };
        }
        match self
            .download_via_soulseek(base, routing, download, profile, ct)
            .await
        {
            Err(e) if !ct.is_cancelled() && goes_on(&e) => {
                info!("Soulseek brought nothing ({e}); asking Mornye");
                base.log_step(
                    &track_key,
                    acquisition_event_kinds::NOTE,
                    "Soulseek brought no copy; asking Mornye",
                    user_safe(Some(&e.to_string())).as_deref(),
                );
                self.download_via_spotiflac(base, &client, routing, download, ct)
                    .await
            }
            other => other,
        }
    }

    /// One song through Mornye, into a job folder of its own, checked like a Soulseek copy.
    pub(super) async fn download_via_spotiflac(
        &self,
        base: &BaseDownloadService,
        client: &Arc<SpotiflacClient>,
        routing: &SoulseekRouting,
        download: &mut TrackDownload,
        ct: &CancellationToken,
    ) -> anyhow::Result<String> {
        let download_path = base.download_path();
        if download_path.is_empty() {
            return Err(InvalidOperationException("DownloadPath is not configured".into()).into());
        }
        let measured = self.with_expected_length(base, routing, download).await;
        let routing = measured.as_ref().unwrap_or(routing);
        let (artist, title) = names(routing);
        let settings = client.settings();
        let connection = Connection::of(&settings);
        if let Some(problem) = SpotiflacSettings::token_problem(&connection.token) {
            return Err(file_not_found(format!("Mornye could not be asked: {problem}")));
        }
        let provider = base.provider_name().to_string();
        let track_key = download.song.external_id.clone().unwrap_or_default();
        base.track(|t| {
            t.stage(
                &provider,
                &track_key,
                AcquisitionState::Searching,
                Some(SPOTIFLAC_SOURCE),
                None,
            )
        });
        sweep_spotiflac_folders(&download_path);

        let isrc = download.song.isrc.clone().or(routing.isrc.clone());
        let want = Wanted {
            artist: &artist,
            title: &title,
            album: routing.album.as_deref(),
            isrc: isrc.as_deref(),
            seconds: routing.duration,
        };
        let mut chosen = None;
        let mut why_none = String::from("Mornye's search found nothing");
        for query in search_queries(&want) {
            base.log_step(
                &track_key,
                acquisition_event_kinds::SEARCH,
                &format!("Asking Mornye for \"{query}\""),
                None,
            );
            let found = or_canceled(ct, async {
                client
                    .search(&connection, &query)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            let found = match found {
                Err(e) if e.downcast_ref::<OperationCanceled>().is_none() => {
                    // Mornye 1.1 without its utilities extension cannot search: say that, with
                    // its fix, rather than the search's own failure.
                    if client
                        .check_view(None)
                        .await
                        .is_some_and(|view| view.utilities_missing)
                    {
                        let words = octo_core::spotiflac::check::utilities_words();
                        base.log_step(
                            &track_key,
                            acquisition_event_kinds::NOTE,
                            &words.shown,
                            words.detail.as_deref(),
                        );
                        return Err(file_not_found(words.shown));
                    }
                    Err(e)
                }
                other => other,
            };
            let tracks = with_query_isrc(found?, &query);
            match pick_track(&tracks, &want) {
                Pick::Track { index, why } => {
                    chosen = Some((tracks[index].clone(), why));
                    break;
                }
                Pick::None { why } => why_none = why,
            }
        }
        let Some((track, why)) = chosen else {
            base.log_step(
                &track_key,
                acquisition_event_kinds::SEARCH,
                "Mornye has no copy that fits",
                Some(&why_none),
            );
            return Err(file_not_found(format!(
                "Mornye has no copy of '{artist} - {title}': {why_none}"
            )));
        };
        let Some(link) = track.link() else {
            return Err(file_not_found(format!(
                "Mornye's track for '{artist} - {title}' has no link"
            )));
        };
        base.log_step(
            &track_key,
            acquisition_event_kinds::FOUND,
            "Found it through Mornye",
            Some(&why),
        );
        let check_song = CheckSong {
            link: link.clone(),
            artist: artist.clone(),
            title: title.clone(),
            utc: Utc::now(),
        };

        // Mornye's check keeps every extension the song could use: Octo does not ask, and keeps
        // the song for "Pass Mornye's check". Once before waiting for a turn, and again with the
        // turn, since a song ahead may have just run into the check.
        if let Some(wait) = self
            .mornye_gate(base, client, &settings, &track_key, &check_song)
            .await
        {
            return Err(wait);
        }
        let _slot = or_canceled(ct, async { Ok(client.slot().await) }).await?;
        if let Some(wait) = self
            .mornye_gate(base, client, &settings, &track_key, &check_song)
            .await
        {
            return Err(wait);
        }
        let before = bounded_extensions(client, &connection).await;
        client.count_job();
        let at_start = client.activity();
        let job_dir = combine(&combine(&download_path, SPOTIFLAC_FOLDER), &new_guid());
        make_shared_folder(&job_dir)?;
        let their_dir = path_for_spotiflac(&job_dir, &download_path, settings.effective_folder().as_deref());
        let started_utc = Utc::now();
        download.fetched_from = Some(SPOTIFLAC_SOURCE.to_string());
        if !download.suppress_notify {
            base.notifications().notify(NotificationEvent {
                artist: routing.artist.clone(),
                title: routing.title.clone(),
                album: routing.album.clone(),
                source: Some(SPOTIFLAC_SOURCE.into()),
                format: Some("FLAC".into()),
                duration_seconds: routing.duration,
                ..NotificationEvent::new(NotificationEventType::DownloadStarted)
            });
        }
        base.track(|t| t.transfer(&provider, &track_key, None, None, None, Some(SPOTIFLAC_SOURCE)));
        base.log_step(
            &track_key,
            acquisition_event_kinds::TRY,
            "Asked Mornye to save it",
            Some(&format!("As {}", settings.effective_quality())),
        );
        let job = match client.queue(&connection, &link, &settings, &their_dir).await {
            Ok(job) => job,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&job_dir);
                return Err(anyhow::anyhow!("Mornye did not take the download: {e}"));
            }
        };
        info!(
            "Mornye job {} for '{artist} - {title}' into {their_dir}",
            if job.id.is_empty() { "(no id)" } else { &job.id }
        );

        let waited = self
            .wait_for_spotiflac(client, &connection, &job, &job_dir, client.song_timeout(), ct)
            .await;
        let path = match waited {
            Ok((path, through)) => {
                client.check.store.brought(through.as_deref(), &link);
                path
            }
            Err(e) => {
                if job.is_pollable() && !matches!(e, Waited::Failed { .. }) {
                    client.cancel(&connection, &job.id).await;
                }
                let _ = std::fs::remove_dir_all(&job_dir);
                // The log line, the sentence a row shows, and what the download log adds to it.
                let (words, shown, more) = match e {
                    Waited::Canceled => return Err(OperationCanceled.into()),
                    Waited::TimedOut(timeout) => {
                        let words = format!(
                            "Mornye took longer than {} seconds for '{artist} - {title}', so Octo stopped waiting",
                            timeout.as_secs()
                        );
                        (words.clone(), words, None)
                    }
                    Waited::Failed { reason, provider } => {
                        let explained = self
                            .explain_failure(
                                client,
                                &connection,
                                &reason,
                                provider.as_deref(),
                                &before,
                                at_start,
                                &their_dir,
                            )
                            .await;
                        match explained {
                            Explained::Waiting(wait) => {
                                warn!(
                                    "Mornye's job for '{artist} - {title}' ran into its check: {}",
                                    wait.shown
                                );
                                base.log_step(
                                    &track_key,
                                    acquisition_event_kinds::NOTE,
                                    &wait.shown,
                                    Some(&wait.detail),
                                );
                                client.check.store.remember(check_song);
                                return Err(wait.into_error());
                            }
                            Explained::Words(words) => (
                                format!(
                                    "Mornye could not download '{artist} - {title}': {}{}",
                                    words.shown,
                                    words
                                        .detail
                                        .as_deref()
                                        .map(|d| format!(" ({d})"))
                                        .unwrap_or_default()
                                ),
                                words.shown,
                                words.detail,
                            ),
                        }
                    }
                    Waited::NoFile => {
                        let words = format!(
                            "Mornye said it saved '{artist} - {title}', but nothing arrived in {job_dir}. \
                             Mornye must save into the same folder Octo reads; see the Music folder setting on the Mornye page"
                        );
                        (words.clone(), words, None)
                    }
                    Waited::Unreachable(reason) => {
                        let words = format!("Mornye stopped answering: {reason}");
                        (words.clone(), words, None)
                    }
                };
                warn!("{words}");
                // Each part through user_safe: the first sentence, no server paths.
                let detail = [user_safe(Some(&shown)), user_safe(more.as_deref())]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(". ");
                base.log_step(
                    &track_key,
                    acquisition_event_kinds::NOTE,
                    "Mornye brought no file",
                    (!detail.is_empty()).then_some(detail.as_str()),
                );
                return Err(file_not_found(shown));
            }
        };
        let (quality, _) = AudioSummary::describe(Some(&path));
        base.log_step(
            &track_key,
            acquisition_event_kinds::FOUND,
            "Mornye brought a copy",
            quality.as_deref(),
        );
        match self
            .check_spotiflac_file(base, routing, download, &path, started_utc)
            .await
        {
            Ok(path) => Ok(path),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&job_dir);
                Err(e)
            }
        }
    }

    /// Waits for the job to end and finds its file in the job folder, with the extension it came
    /// through when Mornye says. A job Mornye cannot be asked about again is watched by its
    /// folder: a song file whose size held still for one look is the song.
    async fn wait_for_spotiflac(
        &self,
        client: &SpotiflacClient,
        connection: &Connection,
        job: &Job,
        job_dir: &str,
        timeout: Duration,
        ct: &CancellationToken,
    ) -> Result<(String, Option<String>), Waited> {
        let deadline = Instant::now() + timeout;
        let mut pollable = job.is_pollable();
        let mut failures = 0;
        let mut last_seen: Option<(String, u64)> = None;
        loop {
            if ct.is_cancelled() {
                return Err(Waited::Canceled);
            }
            if Instant::now() >= deadline {
                return Err(Waited::TimedOut(timeout));
            }
            if pollable {
                match client.job(connection, &job.id).await {
                    Ok(Some(now)) => {
                        failures = 0;
                        match now.state() {
                            JobState::Done => {
                                let through = now.provider().map(str::to_string);
                                return self
                                    .landed_file(job_dir, ct)
                                    .await
                                    .map(|path| (path, through))
                                    .ok_or(Waited::NoFile);
                            }
                            JobState::Failed => {
                                return Err(Waited::Failed {
                                    reason: now.failure().unwrap_or_default(),
                                    provider: now.provider().map(str::to_string),
                                });
                            }
                            JobState::Cancelled => {
                                return Err(Waited::Failed {
                                    reason: "the job was cancelled in Mornye".to_string(),
                                    provider: None,
                                });
                            }
                            JobState::Working => {}
                        }
                    }
                    // Mornye forgot the job: the folder says when it is done.
                    Ok(None) => pollable = false,
                    Err(e) => {
                        failures += 1;
                        if failures >= CALLS_THAT_MAY_FAIL {
                            return Err(Waited::Unreachable(e.to_string()));
                        }
                        debug!("Mornye job {}: {e}", job.id);
                    }
                }
            } else {
                let now = audio_in(job_dir);
                if now.is_some() && now == last_seen {
                    return now.map(|(path, _)| (path, None)).ok_or(Waited::NoFile);
                }
                last_seen = now;
            }
            tokio::select! {
                biased;
                _ = ct.cancelled() => return Err(Waited::Canceled),
                _ = tokio::time::sleep(client.poll_interval()) => {}
            }
        }
    }

    /// The song file in the job folder, looked for again for a few seconds: Mornye can say a
    /// job is done a moment before the file is moved into place.
    async fn landed_file(&self, job_dir: &str, ct: &CancellationToken) -> Option<String> {
        retry_resolve(
            || audio_in(job_dir).map(|(path, _)| path),
            Duration::from_secs(5),
            Duration::from_millis(250),
            ct,
        )
        .await
    }

    /// The checks a Soulseek copy meets, on Mornye's file: its length, the version its own
    /// title names, AcoustID and the spectrum. A file that fails one is thrown away.
    async fn check_spotiflac_file(
        &self,
        base: &BaseDownloadService,
        routing: &SoulseekRouting,
        download: &mut TrackDownload,
        path: &str,
        started_utc: DateTime<Utc>,
    ) -> anyhow::Result<String> {
        let (artist, title) = names(routing);
        let provider = base.provider_name().to_string();
        let track_key = download.song.external_id.clone().unwrap_or_default();
        base.track(|t| t.stage(&provider, &track_key, AcquisitionState::Verifying, None, None));

        if download.flac_only && normalize_extension(None, path) != "flac" {
            discard_rejected_download(path, started_utc);
            base.log_step(
                &track_key,
                acquisition_event_kinds::CHECK,
                "Not a FLAC file",
                Some("A FLAC copy was asked for; thrown away"),
            );
            return Err(file_not_found(format!(
                "Mornye brought '{artist} - {title}' as {}, and a FLAC copy was asked for",
                file_name(path)
            )));
        }

        let (matches, actual_secs) = downloaded_duration_matches(path, routing.duration);
        if !matches {
            let expected = display_option(routing.duration);
            warn!(
                "Mornye delivered {actual_secs}s for '{artist} - {title}', expected {expected}s; discarding it"
            );
            discard_rejected_download(path, started_utc);
            base.log_step(
                &track_key,
                acquisition_event_kinds::CHECK,
                &format!(
                    "Wrong length: {} for a {} song",
                    soulseek_candidates::length_text(actual_secs),
                    soulseek_candidates::length_text(routing.duration.unwrap_or(0))
                ),
                Some("Thrown away"),
            );
            return Err(file_not_found(format!(
                "Mornye brought a {actual_secs}s file for the {expected}s song '{artist} - {title}'"
            )));
        }

        // Mornye tags its files from its own catalog. Those tags are never taken as the song's
        // identity, but a title naming another version is a reason to refuse the file.
        let own_title = TagFile::open(path).ok().and_then(|file| file.title());
        if let Some(own_title) = own_title.filter(|own| other_version(&title, own, Against::Title)) {
            warn!("Mornye's file for '{artist} - {title}' is titled '{own_title}'; discarding it");
            discard_rejected_download(path, started_utc);
            base.log_step(
                &track_key,
                acquisition_event_kinds::CHECK,
                "Another version",
                Some(&format!("The file calls itself '{own_title}'; thrown away")),
            );
            return Err(file_not_found(format!(
                "Mornye's file for '{artist} - {title}' is '{own_title}', another version"
            )));
        }

        // A heart's or a play's file is judged by each asker's own gate; every other download keeps
        // the shipped checks (AcoustID, then the song's YouTube preview).
        let length_known = routing.duration.is_some_and(|s| s > 0) && actual_secs > 0;
        let verdict = match self
            .judge_file(base, routing, download, path, None, length_known)
            .await
        {
            FileJudged::Keep(verdict) => verdict,
            FileJudged::Hold(held) => {
                info!("Mornye's file for '{artist} - {title}' waits: {}", held.line);
                return Err(held.into());
            }
            FileJudged::NotProven(why) => {
                info!("Mornye's file for the play of '{artist} - {title}' is not kept: {why}");
                discard_rejected_download(path, started_utc);
                return Err(not_proven());
            }
            FileJudged::Refuse {
                reason,
                shared,
                verdict,
            } => {
                discard_rejected_download(path, started_utc);
                if shared && verdict.verdict == VerificationVerdict::Mismatch {
                    warn!(
                        "Mornye brought {} for '{artist} - {title}' (AcoustID score {}); discarding it",
                        verdict.describe(),
                        percent(verdict.score)
                    );
                    base.log_step_heard(
                        &track_key,
                        acquisition_event_kinds::CHECK,
                        &mismatch_text(&verdict),
                        Some(
                            if verdict.kind.is_some_and(|kind| kind != RecordingKind::Studio) {
                                "Not the take asked for; thrown away"
                            } else {
                                "Not the song asked for; thrown away"
                            },
                        ),
                        verdict.heard(),
                    );
                    return Err(file_not_found(format!(
                        "AcoustID identified Mornye's file as {}",
                        verdict.describe()
                    )));
                }
                return Err(file_not_found(format!(
                    "Mornye's file for '{artist} - {title}' {reason}"
                )));
            }
        };
        let song = &mut download.song;

        let spectrum = self
            .verification
            .check_lossless(path, routing.artist.as_deref(), routing.title.as_deref())
            .await;
        let lossy = spectrum.is_likely_lossy();
        if lossy {
            warn!(
                "Mornye's copy of '{artist} - {title}' is {}; keeping it, marked as a transcode",
                spectrum.describe()
            );
            base.log_step(
                &track_key,
                acquisition_event_kinds::CHECK,
                "Looks made from a lossy file",
                Some(&format!("{}; kept, marked as such", spectrum.describe())),
            );
            song.transcoded_from = spectrum.estimate.clone();
        }
        base.log_step_heard(
            &track_key,
            acquisition_event_kinds::CHECK,
            "Passed the checks",
            Some(&checks_passed_text(
                &verdict,
                !lossy && base.current_soulseek_settings().detect_transcodes,
                false,
            )),
            verdict.heard(),
        );
        verdict.apply_tags_to(song);
        song.verification = Some(Box::new(verdict));
        info!("Mornye download complete: {path}");
        Ok(path.to_string())
    }
}

/// Whether a lossless source's failure lets the other one be asked: not when its file waits in the
/// hold path, nor when a play's download was not proven.
fn goes_on(error: &anyhow::Error) -> bool {
    error.downcast_ref::<AcquisitionHeld>().is_none()
        && error
            .downcast_ref::<crate::services::common::PlayOutcome>()
            .is_none()
}

/// How a wait for a Mornye job ended without a file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Waited {
    Canceled,
    TimedOut(Duration),
    /// Mornye's reason, and the extension the job failed in when it says.
    Failed {
        reason: String,
        provider: Option<String>,
    },
    NoFile,
    Unreachable(String),
}

/// Why a Mornye job failed: its check, or words for anything else.
enum Explained {
    Waiting(MornyeWaiting),
    Words(octo_core::spotiflac::check::FailureWords),
}

/// Mornye's extensions and their health, or None when Mornye did not answer in time.
async fn bounded_extensions(client: &SpotiflacClient, connection: &Connection) -> Option<ExtensionsAnswer> {
    let bound = client.check.times.call_bound;
    match tokio::time::timeout(bound, client.extensions(connection)).await {
        Ok(Ok(answer)) => Some(answer),
        _ => None,
    }
}

/// Whether extension `name`'s last error is this job's own: its row moved by exactly one failure
/// and at least one try between the two reads.
fn moved_by_one_failure(
    before: Option<&ExtensionsAnswer>,
    after: Option<&ExtensionsAnswer>,
    name: &str,
) -> bool {
    let (Some(before), Some(after)) = (before.and_then(|b| b.row(name)), after.and_then(|a| a.row(name)))
    else {
        return false;
    };
    after.failures == before.failures + 1 && after.attempts > before.attempts
}

impl SoulseekDownloadService {
    /// The gate: when Mornye's check keeps every extension the song could use, the wait, logged,
    /// with the song kept for "Pass Mornye's check". None when Mornye may be asked.
    async fn mornye_gate(
        &self,
        base: &BaseDownloadService,
        client: &SpotiflacClient,
        settings: &SpotiflacSettings,
        track_key: &str,
        song: &CheckSong,
    ) -> Option<anyhow::Error> {
        let view = client.check_view(None).await?;
        let now = Utc::now();
        let pause = view.pause(&settings.effective_services(), now)?;
        let wait = MornyeWaiting::new(pause, false, now);
        info!(
            "Not asking Mornye for '{} - {}': {}",
            song.artist, song.title, wait.shown
        );
        base.log_step(
            track_key,
            acquisition_event_kinds::NOTE,
            &wait.shown,
            Some(&wait.detail),
        );
        client.check.store.remember(song.clone());
        Some(wait.into_error())
    }

    /// Why a Mornye job failed, read after the failure was seen: a wait when Mornye's check
    /// keeps its extension (or every extension, or Mornye's own words say it is paused), else
    /// words from the extension's health.
    #[allow(clippy::too_many_arguments)]
    async fn explain_failure(
        &self,
        client: &SpotiflacClient,
        connection: &Connection,
        reason: &str,
        provider: Option<&str>,
        before: &Option<ExtensionsAnswer>,
        at_start: (usize, u64),
        their_dir: &str,
    ) -> Explained {
        let seen = Instant::now();
        let provider = provider.and_then(named_provider);
        let view = client.check_view(Some(seen)).await;
        let now = Utc::now();
        if let Some(view) = &view {
            let pause = provider
                .and_then(|name| view.pause_of(name, now))
                .or_else(|| view.pause(&client.settings().effective_services(), now));
            if let Some(pause) = pause {
                return Explained::Waiting(MornyeWaiting::new(pause, true, now));
            }
        }
        let after = bounded_extensions(client, connection).await;
        let health = provider.and_then(|name| after.as_ref().and_then(|answer| answer.row(name)));
        // Mornye's own "verification paused ... next try in" words, in the job's reason or in
        // the extension's last error (where Mornye 1.1 keeps them) when that error is new since
        // the job began.
        let failed_since = |name: &str| {
            let failures = |answer: &Option<ExtensionsAnswer>| {
                answer.as_ref().and_then(|a| a.row(name)).map(|row| row.failures)
            };
            matches!((failures(before), failures(&after)), (Some(was), Some(is)) if is > was)
        };
        let paused_words = octo_core::spotiflac::check::next_try_from_words(reason, now).or_else(|| {
            health
                .filter(|row| row.last_outcome.eq_ignore_ascii_case("failure"))
                .filter(|_| provider.is_some_and(failed_since))
                .and_then(|row| octo_core::spotiflac::check::next_try_from_words(&row.last_error, now))
        });
        if let Some(until) = paused_words {
            let pause = octo_core::spotiflac::check::CheckPause {
                until_utc: until,
                labels: provider.map(|name| vec![name.to_string()]).unwrap_or_default(),
            };
            return Explained::Waiting(MornyeWaiting::new(pause, true, now));
        }
        // Octo asked Mornye for this song alone, from the health read before it to the one after.
        let own_job = at_start.0 == 1
            && client.activity() == at_start
            && provider.is_some_and(|name| moved_by_one_failure(before.as_ref(), after.as_ref(), name));
        Explained::Words(failure_words(&FailureFacts {
            reason,
            provider,
            health,
            own_job,
            their_dir,
            utilities_missing: view.as_ref().is_some_and(|view| view.utilities_missing),
        }))
    }
}

/// The song file in a job folder, with its size, or None while there is none.
fn audio_in(job_dir: &str) -> Option<(String, u64)> {
    let files: Vec<(String, u64)> = files_under(job_dir)
        .ok()?
        .into_iter()
        .filter_map(|path| std::fs::metadata(&path).ok().map(|meta| (path, meta.len())))
        .collect();
    let picked = pick_audio_file(&files)?;
    files.iter().find(|(path, _)| path == picked).cloned()
}

/// Removes Mornye job folders a placed or discarded download left empty, after half an hour,
/// and any a day old. Only inside Octo's own Mornye folder.
pub(super) fn sweep_spotiflac_folders(download_path: &str) {
    let jobs = combine(download_path, SPOTIFLAC_FOLDER);
    let sweep = || -> std::io::Result<()> {
        if !Path::new(&jobs).is_dir() {
            return Ok(());
        }
        for entry in std::fs::read_dir(&jobs)? {
            let dir = entry?.path();
            // Mornye's own downloads may share this folder: only Octo's job and check folders go.
            let named_by_octo = dir
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(octo_folder_name);
            if !dir.is_dir() || !named_by_octo {
                continue;
            }
            let empty = audio_in(&dir.to_string_lossy()).is_none();
            let age = age_of(&std::fs::metadata(&dir)?.modified()?);
            let young = if empty {
                age < Duration::from_secs(30 * 60)
            } else {
                age < Duration::from_secs(24 * 3600)
            };
            if !young {
                std::fs::remove_dir_all(&dir)?;
                debug!("Removed an old Mornye folder {}", dir.display());
            }
        }
        Ok(())
    };
    if let Err(e) = sweep() {
        debug!("Could not sweep {jobs}: {e}");
    }
}

/// A folder name Octo gives: a job id (32 hex characters), or a folder check's `octo-check-<id>`.
pub(crate) fn octo_folder_name(name: &str) -> bool {
    let id = name.strip_prefix("octo-check-").unwrap_or(name);
    id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
#[path = "spotiflac_download_tests.rs"]
mod tests;
