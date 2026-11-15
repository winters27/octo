//! The library action executor: Keep, which outcomes clear a request, refusing a replacement that
//! is not really lossless, carrying a favorite over a replacement, and a replacement keeping the
//! original's identity.

use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use indexmap::IndexMap;
use octo_core::settings::{
    AppSettings, LibraryActionDefinition, LibraryActionSettings, NoticeKind, NotificationSettings,
};
use octo_media::audio::spectrum_analyzer::SpectrumVerdict;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::services::common::test_fakes::until;
use crate::services::common::{AcquisitionTracker, AcquisitionWorker};
use crate::services::i_download_service::{AudioStream, DirectStreamInfo, IDownloadService};
use crate::services::library::PathSource;
use crate::services::library::library_action_test_support::{Extras, asked_about, executor, store};
use crate::services::library::notice_queue::NoticeState;
use crate::services::notifications::NotificationService;
use octo_core::common::Clock;
use octo_core::models::download::DownloadInfo;

// ---- Keep ----------------------------------------------------------------------------------
//
// Keep (#47) is an answer, not an operation: it touches no file, so it runs in rehearsal mode
// too, and on a track Octo never asked about it is just a rating.

fn keep_settings() -> LibraryActionSettings {
    LibraryActionSettings {
        enabled: true,
        review_enabled: true,
        dry_run: true,
        allowed_users: vec!["alice".into()],
        ..Default::default()
    }
}

fn keep_executor(
    settings: LibraryActionSettings,
    journal: &Arc<LibraryActionJournal>,
    notices: Option<Arc<NoticeQueue>>,
) -> Arc<LibraryActionExecutor> {
    executor(
        &store(AppSettings {
            library_actions: settings,
            ..Default::default()
        }),
        Extras {
            journal: Some(journal.clone()),
            notices,
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn keep_on_a_track_octo_never_asked_about_is_skipped_and_leaves_no_record() {
    let journal = Arc::new(LibraryActionJournal::new());
    let executor = keep_executor(keep_settings(), &journal, Some(asked_about("alice", "nd-asked")));

    let outcome = executor
        .apply(LibraryActionRequest::new(
            LibraryAction::Keep,
            "nd-other",
            "alice",
        ))
        .await
        .expect("an outcome");

    assert_eq!(outcome.state, LibraryActionState::Skipped);
    assert!(outcome.consumed());
    assert!(journal.recent(200).is_empty());
}

#[tokio::test]
async fn keep_on_a_track_octo_asked_about_answers_it_even_in_rehearsal() {
    let journal = Arc::new(LibraryActionJournal::new());
    let notices = asked_about("alice", "nd-asked");
    let executor = keep_executor(keep_settings(), &journal, Some(notices.clone()));

    let outcome = executor
        .apply(LibraryActionRequest::new(
            LibraryAction::Keep,
            "nd-asked",
            "alice",
        ))
        .await
        .expect("an outcome");

    assert_eq!(outcome.state, LibraryActionState::Applied);
    let entries = journal.recent(200);
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.action, LibraryAction::Keep);
    assert_eq!(entry.title, "Teardrop");
    assert!(!entry.dry_run);
    let asked = notices.for_user("alice", NoticeKind::Review);
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].state, NoticeState::Kept);
}

#[tokio::test]
async fn keep_switched_off_is_not_applied() {
    let settings = LibraryActionSettings {
        actions: vec![LibraryActionDefinition {
            action: LibraryAction::Keep,
            enabled: false,
            ..Default::default()
        }],
        ..keep_settings()
    };
    let notices = asked_about("alice", "nd-asked");

    let outcome = keep_executor(
        settings,
        &Arc::new(LibraryActionJournal::new()),
        Some(notices.clone()),
    )
    .apply(LibraryActionRequest::new(
        LibraryAction::Keep,
        "nd-asked",
        "alice",
    ))
    .await
    .expect("an outcome");

    assert_eq!(outcome.state, LibraryActionState::Skipped);
    assert_eq!(
        notices.for_user("alice", NoticeKind::Review)[0].state,
        NoticeState::Queued
    );
}

// ---- Which outcomes clear a request --------------------------------------------------------

/// A request that could not be applied must stay where it is, so fixing whatever blocked it and
/// waiting makes it work rather than requiring the user to ask again. A rehearsal is not a
/// failure, and it is not consumed either: the same set has to replay so the operator reads a
/// stable list.
#[test]
fn consumed_only_terminal_success_clears_the_request() {
    for (state, consumed) in [
        (LibraryActionState::Applied, true),
        (LibraryActionState::Skipped, true),
        (LibraryActionState::Failed, false),
        (LibraryActionState::Unresolved, false),
        (LibraryActionState::Pending, false),
        (LibraryActionState::Rehearsed, false),
    ] {
        assert_eq!(
            LibraryActionOutcome::new(state, None).consumed(),
            consumed,
            "{state:?}"
        );
    }
}

// ---- A Better quality replacement that may be a transcode ----------------------------------

#[test]
fn a_better_quality_replacement_that_is_a_transcode_is_refused() {
    let fake = SpectrumReport::new(
        SpectrumVerdict::LikelyLossy,
        44100,
        Some(16929.0),
        "a cliff",
        Some("about 128 kbps MP3".into()),
    );

    assert_eq!(
        LibraryActionExecutor::not_really_lossless(&fake).as_deref(),
        Some("is likely transcoded from about 128 kbps MP3 (cutoff 16.9 kHz)")
    );
}

#[test]
fn a_better_quality_replacement_the_check_cannot_judge_is_accepted() {
    assert_eq!(
        LibraryActionExecutor::not_really_lossless(&SpectrumReport::unknown("not checked", 0)),
        None
    );
    assert_eq!(
        LibraryActionExecutor::not_really_lossless(&SpectrumReport::new(
            SpectrumVerdict::Genuine,
            44100,
            None,
            "audio up to the top of the band",
            None,
        )),
        None
    );
}

// ---- A replaced favorite stays a favorite --------------------------------------------------
//
// #71 for library actions: a replacement asked for by rating carries the rater's own sign-in,
// so whether they had favorited the song can be read before it is replaced. Nothing else reads.

type Calls = Arc<Mutex<Vec<(String, IndexMap<String, String>)>>>;

/// A `StarOnArrival` whose Navidrome calls are recorded and answered: getSong says the song is
/// (or is not) a favorite and is found under its id, and the replacement shows up as nd-new.
fn stars(calls: &Calls, starred: bool) -> Arc<StarOnArrival> {
    let stars = StarOnArrival::new(
        Arc::new(AcquisitionTracker::new(None, Clock::system())),
        None,
        store(AppSettings::default()),
        Clock::system(),
    );
    let calls = calls.clone();
    stars.configure(|seams| {
        seams.visibility_poll = Duration::from_millis(10);
        seams.library_lookup = Some(Arc::new(|_, _, _| {
            async { Ok(Some("nd-new".to_string())) }.boxed()
        }));
        seams.call = Some(Arc::new(
            move |endpoint: String, parameters: IndexMap<String, String>| {
                calls.lock().push((endpoint.clone(), parameters.clone()));
                let body = if endpoint == "rest/getSong" {
                    format!(
                        r#"{{"subsonic-response":{{"status":"ok","song":{{"id":"{}"{}}}}}}}"#,
                        parameters.get("id").cloned().unwrap_or_default(),
                        if starred {
                            r#","starred":"2026-10-01T10:00:00Z""#
                        } else {
                            ""
                        }
                    )
                } else {
                    r#"{"subsonic-response":{"status":"ok"}}"#.to_string()
                };
                async move { Ok(body.into_bytes()) }.boxed()
            },
        ));
    });
    stars
}

fn credential(extra: &[(&str, &str)]) -> Option<SubsonicCredential> {
    let mut parameters: Vec<(String, String)> = vec![
        ("u".into(), "alice".into()),
        ("t".into(), "token".into()),
        ("s".into(), "salt".into()),
    ];
    parameters.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    SubsonicCredential::from(parameters.iter().map(|(k, v)| (k, v)))
}

fn star_executor(calls: &Calls, starred: bool) -> Arc<LibraryActionExecutor> {
    executor(
        &store(AppSettings::default()),
        Extras {
            stars: Some(stars(calls, starred)),
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn replacement_of_a_favorite_is_marked_to_carry() {
    let calls = Calls::default();
    let executor = star_executor(&calls, true);

    let carry = executor
        .was_starred_by_requester(
            &LibraryActionRequest::new(LibraryAction::BetterQuality, "nd-1", "alice")
                .with_credential(credential(&[])),
        )
        .await;

    assert!(carry);
    let calls = calls.lock();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "rest/getSong");
    assert_eq!(calls[0].1["id"], "nd-1");
    assert_eq!(calls[0].1["u"], "alice");
}

#[tokio::test]
async fn delete_never_reads() {
    let calls = Calls::default();
    let executor = star_executor(&calls, true);

    let carry = executor
        .was_starred_by_requester(
            &LibraryActionRequest::new(LibraryAction::Delete, "nd-1", "alice")
                .with_credential(credential(&[])),
        )
        .await;

    assert!(!carry);
    assert!(calls.lock().is_empty());
}

#[tokio::test]
async fn playlist_action_without_sign_in_does_not_carry() {
    let calls = Calls::default();
    let executor = star_executor(&calls, true);

    let carry = executor
        .was_starred_by_requester(&LibraryActionRequest::new(
            LibraryAction::BetterQuality,
            "nd-1",
            "alice",
        ))
        .await;

    assert!(!carry);
    assert!(calls.lock().is_empty());
}

// `rating_carries_the_raters_sign_in` is in `library_action_rating_worker.rs`.

// ---- A replacement keeps the original's identity -------------------------------------------
//
// A replacement takes the original's folder and name, Octo checks that Navidrome kept the
// ORIGINAL id on the new file, records the answer on the action, and only when it did not,
// favorites the new song again for the person who rated it.

const KEY: &str = "key-1";

fn original(dir: &Path) -> ResolvedSongFile {
    ResolvedSongFile {
        navidrome_id: "nd-1".into(),
        absolute_path: dir
            .join("Massive Attack - Teardrop.mp3")
            .to_string_lossy()
            .into_owned(),
        size_bytes: 1000,
        title: "Teardrop".into(),
        artist: "Massive Attack".into(),
        album: "Mezzanine".into(),
        suffix: "mp3".into(),
        duration_seconds: Some(330),
        source: PathSource::NativeApi,
        album_artist: None,
    }
}

fn identity_journal() -> Arc<LibraryActionJournal> {
    let journal = LibraryActionJournal::new();
    journal.record(LibraryActionEntry {
        key: KEY.into(),
        action: LibraryAction::BetterQuality,
        navidrome_id: "nd-1".into(),
        username: "alice".into(),
        title: "Teardrop".into(),
        artist: "Massive Attack".into(),
        album: "Mezzanine".into(),
        source_path: Some("/music/a.mp3".into()),
        quarantine_path: None,
        resolution: Some(PathSource::NativeApi),
        state: LibraryActionState::Applied,
        detail: Some("Replaced with a.flac.".into()),
        dry_run: false,
        at_utc: Utc::now(),
        history_kept: None,
        revealed_path: None,
        copy: false,
        download_allowed: false,
        denied_source: None,
    });
    Arc::new(journal)
}

fn identity_entry(journal: &LibraryActionJournal) -> LibraryActionEntry {
    journal
        .recent(200)
        .into_iter()
        .find(|entry| entry.key == KEY)
        .expect("the entry")
}

fn identity() -> KeptIdentity {
    KeptIdentity {
        title: "Teardrop".into(),
        album: Some("Mezzanine".into()),
        album_artist: vec!["Massive Attack".into()],
        album_artists: Vec::new(),
        album_version: None,
        release_date: None,
        album_id: None,
        release_track_id: None,
        track: 3,
        track_count: 11,
        disc: 1,
        disc_count: 1,
        compilation: false,
    }
}

fn identity_executor(
    journal: &Arc<LibraryActionJournal>,
    stars: Option<Arc<StarOnArrival>>,
) -> Arc<LibraryActionExecutor> {
    let executor = executor(
        &store(AppSettings::default()),
        Extras {
            journal: Some(journal.clone()),
            stars,
            ..Default::default()
        },
    );
    executor.configure(|seams| {
        seams.history_poll = Duration::from_millis(1);
        seams.history_attempts = 3;
    });
    executor
}

fn no_admit() -> BeforeReveal {
    Arc::new(|_| async { None }.boxed())
}

#[test]
fn the_replacement_target_is_the_originals_folder_and_stem() {
    let dir = tempfile::tempdir().expect("temp dir");
    let original = original(dir.path());
    std::fs::write(&original.absolute_path, [1, 2, 3]).expect("written");
    let handoff = LibraryActionExecutor::handoff_for(&original, identity(), no_admit(), None);

    let flac = dir
        .path()
        .join("Massive Attack - Teardrop.flac")
        .to_string_lossy()
        .into_owned();
    assert_eq!(handoff.target_for(".flac"), Some(flac.clone()));
    assert_eq!(handoff.target_for(".mp3"), None);
    std::fs::remove_file(&original.absolute_path).expect("removed");
    assert_eq!(handoff.target_for(".mp3"), Some(original.absolute_path.clone()));
    std::fs::write(&flac, [1, 2, 3]).expect("written");
    assert_eq!(handoff.target_for(".flac"), None);
}

#[tokio::test]
async fn the_original_id_showing_the_new_file_is_history_kept() {
    let dir = tempfile::tempdir().expect("temp dir");
    let journal = identity_journal();
    let executor = identity_executor(&journal, None);
    let asked: Arc<Mutex<Vec<String>>> = Arc::default();
    let scans = Arc::new(AtomicUsize::new(0));
    let (asked_seen, scans_seen) = (asked.clone(), scans.clone());
    executor.configure(move |seams| {
        seams.shows_at = Some(Arc::new(move |id, _| {
            let mut asked = asked_seen.lock();
            asked.push(id);
            let shown = asked.len() >= 2;
            async move { shown }.boxed()
        }));
        seams.force_scan = Some(Arc::new(move || {
            scans_seen.fetch_add(1, AtomicOrdering::SeqCst);
            async { true }.boxed()
        }));
    });

    let kept = executor
        .confirm_history_kept(
            &LibraryActionRequest::new(LibraryAction::BetterQuality, "nd-1", "alice"),
            &original(dir.path()),
            &dir.path()
                .join("Massive Attack - Teardrop.flac")
                .to_string_lossy(),
            KEY,
            false,
        )
        .await;

    assert!(kept);
    assert_eq!(asked.lock().len(), 2);
    assert!(asked.lock().iter().all(|id| id == "nd-1"));
    assert_eq!(scans.load(AtomicOrdering::SeqCst), 1);
    let entry = identity_entry(&journal);
    assert_eq!(entry.history_kept, Some(true));
    assert!(
        entry
            .detail
            .unwrap_or_default()
            .contains(LibraryActionExecutor::HISTORY_KEPT_TEXT)
    );
}

fn never_shown(executor: &LibraryActionExecutor) {
    executor.configure(|seams| {
        seams.shows_at = Some(Arc::new(|_, _| async { false }.boxed()));
        seams.force_scan = Some(Arc::new(|| async { true }.boxed()));
    });
}

#[tokio::test]
async fn navidrome_taking_it_for_a_new_song_falls_back_to_the_raters_favorite() {
    let dir = tempfile::tempdir().expect("temp dir");
    let calls = Calls::default();
    let journal = identity_journal();
    let executor = identity_executor(&journal, Some(stars(&calls, false)));
    never_shown(&executor);

    let kept = executor
        .confirm_history_kept(
            &LibraryActionRequest::new(LibraryAction::BetterQuality, "nd-1", "alice")
                .with_credential(credential(&[("c", "Symfonium")])),
            &original(dir.path()),
            &dir.path()
                .join("Massive Attack - Teardrop.flac")
                .to_string_lossy(),
            KEY,
            true,
        )
        .await;

    assert!(!kept);
    let entry = identity_entry(&journal);
    assert_eq!(entry.history_kept, Some(false));
    assert!(
        entry
            .detail
            .unwrap_or_default()
            .contains(LibraryActionExecutor::HISTORY_LOST_TEXT)
    );
    let starred = || {
        calls
            .lock()
            .iter()
            .filter(|(endpoint, _)| endpoint == "rest/star")
            .map(|(_, parameters)| parameters.clone())
            .collect::<Vec<_>>()
    };
    until(|| starred().len() == 1).await;
    let star = &starred()[0];
    assert_eq!(star["id"], "nd-new");
    assert_eq!(star["u"], "alice");
}

#[tokio::test]
async fn not_kept_without_a_favorite_stars_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let calls = Calls::default();
    let journal = identity_journal();
    let executor = identity_executor(&journal, Some(stars(&calls, false)));
    never_shown(&executor);

    let kept = executor
        .confirm_history_kept(
            &LibraryActionRequest::new(LibraryAction::BetterQuality, "nd-1", "alice")
                .with_credential(credential(&[("c", "Symfonium")])),
            &original(dir.path()),
            &dir.path()
                .join("Massive Attack - Teardrop.flac")
                .to_string_lossy(),
            KEY,
            false,
        )
        .await;

    assert!(!kept);
    assert_eq!(identity_entry(&journal).history_kept, Some(false));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(calls.lock().is_empty());
}

/// A stand-in `IDownloadService` set up for exactly one acquisition: the worker must hand the
/// download this handoff, the Soulseek source, the slow search and the permanent copy.
struct ExpectingDownloads {
    handoff: Arc<ReplacementHandoff>,
}

#[async_trait::async_trait]
impl IDownloadService for ExpectingDownloads {
    async fn download_song(&self, _: &str, _: &str, _: &CancellationToken) -> anyhow::Result<String> {
        anyhow::bail!("not set up")
    }

    async fn download_and_stream(
        &self,
        _: &str,
        _: &str,
        _: &CancellationToken,
    ) -> anyhow::Result<AudioStream> {
        anyhow::bail!("not set up")
    }

    fn download_remaining_album_tracks_in_background(&self, _: &str, _: &str, _: &str) {}

    async fn execute_acquisition(
        &self,
        provider: &str,
        external_id: &str,
        trigger_album_download: bool,
        force_permanent: bool,
        source_override: Option<DownloadSource>,
        _: &CancellationToken,
        _: Option<Vec<String>>,
        upgrade_search: bool,
        replacement: Option<Arc<ReplacementHandoff>>,
    ) -> anyhow::Result<String> {
        let expected = (
            provider,
            external_id,
            trigger_album_download,
            force_permanent,
            source_override,
            upgrade_search,
        );
        if expected
            != (
                "soulseek",
                "id-1",
                false,
                true,
                Some(DownloadSource::Soulseek),
                true,
            )
            || !replacement.is_some_and(|h| Arc::ptr_eq(&h, &self.handoff))
        {
            anyhow::bail!("not the acquisition that was set up");
        }
        Ok("/music/a.flac".into())
    }

    async fn download_album_with_source(
        &self,
        _: &str,
        _: &str,
        _: DownloadSource,
        _: bool,
        _: &CancellationToken,
        _: Option<Vec<String>>,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }

    fn get_download_status(&self, _: &str) -> Option<DownloadInfo> {
        None
    }

    async fn get_local_path_if_exists(&self, _: &str, _: &str) -> Option<String> {
        None
    }

    async fn is_available(&self) -> bool {
        true
    }

    async fn get_direct_stream(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: Option<&crate::services::i_download_service::StreamPlay>,
        _: &CancellationToken,
    ) -> anyhow::Result<Option<DirectStreamInfo>> {
        Ok(None)
    }
}

#[tokio::test]
async fn the_worker_hands_the_replacement_handoff_to_the_download() {
    let dir = tempfile::tempdir().expect("temp dir");
    let handoff = Arc::new(LibraryActionExecutor::handoff_for(
        &original(dir.path()),
        identity(),
        no_admit(),
        None,
    ));
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let settings = store(AppSettings {
        notifications: NotificationSettings::default(),
        ..Default::default()
    });
    let worker = Arc::new(AcquisitionWorker::new(
        queue.clone(),
        Arc::new(ExpectingDownloads {
            handoff: handoff.clone(),
        }),
        Arc::new(ExternalIdRegistry::new(None::<&Path>)),
        Arc::new(NotificationService::new(Vec::new(), settings, None)),
        None,
    ));
    let stopping = CancellationToken::new();
    let running = tokio::spawn(worker.run(stopping.clone()));

    let path = queue
        .enqueue(
            "soulseek",
            "id-1",
            true,
            false,
            true,
            Some(DownloadSource::Soulseek),
            false,
            None,
            true,
            Some(handoff),
        )
        .wait()
        .await;
    stopping.cancel();
    let _ = running.await;

    assert_eq!(path.expect("the download's path"), "/music/a.flac");
}
