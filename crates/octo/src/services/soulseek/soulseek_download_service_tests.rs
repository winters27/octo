//! Tests of the Soulseek download service: the deny-list, the resolve retry, the incomplete
//! folder, `resolve_in_job` and the job folders, `weigh_transcode`, and the album walk through
//! the real service against a fake slskd (a wiremock responder that keeps the fake's state).

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use octo_core::fingerprint::heart_gate::{AcquisitionOrigin, HeardReading, HeardRecording, HeartGate};
use octo_core::fingerprint::{AcoustIdLookup, AcoustIdRecording, AcoustIdResult};
use octo_core::models::domain::{Album, Song};
use octo_core::settings::{AppSettings, DownloadSource, SettingsStore, SoulseekSettings, SubsonicSettings};
use octo_core::soulseek::{RoutingKind, SoulseekFileHit, SoulseekRouting};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::common::{DownloadConcurrency, DownloadCore, DownloadServices};
use crate::services::fingerprint::{AcoustIdClient, AcoustIdRateLimitHandler, AcoustIdRateLimiter};
use crate::services::i_download_service::IDownloadService;
use crate::services::local::DownloadHistoryService;
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::notifications::NotificationService;
use crate::services::soulseek::soulseek_client::Timings;
use crate::services::subsonic::NavidromeIdentityService;
use octo_media::audio::AudioFingerprinter;

/// A folder under the temp directory, removed when the test ends.
struct Root(tempfile::TempDir);

impl Root {
    fn new(prefix: &str) -> Self {
        Root(
            tempfile::Builder::new()
                .prefix(prefix)
                .tempdir()
                .expect("a temp folder"),
        )
    }

    fn path(&self) -> String {
        self.0.path().to_string_lossy().into_owned()
    }

    fn write(&self, relative: &str, size: usize) -> String {
        let path = self.0.path().join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the folder is made");
        std::fs::write(&path, vec![0u8; size]).expect("the file is written");
        path.to_string_lossy().into_owned()
    }
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

// ---- The deny-list -------------------------------------------------------------------------
//
// `candidate_allowed` is the seam between the deny-list and the ranking. The failure it guards
// against is invisible from outside: a filter that denies everything leaves every track
// unfetchable and looks exactly like Soulseek having no copies.

fn hit(user: &str, file: &str) -> SoulseekFileHit {
    SoulseekFileHit {
        username: user.into(),
        filename: file.into(),
        extension: "flac".into(),
        size: 40_000_000,
        ..Default::default()
    }
}

#[test]
fn candidate_allowed_rejected_candidate_is_never_offered_again() {
    let registry = RejectedPeerRegistry::in_memory();
    registry.deny(Some("peer1"), Some("a.flac"), "wrong recording", "A - B");

    assert!(!candidate_allowed(&hit("peer1", "a.flac"), Some(&registry), true));
    assert!(candidate_allowed(&hit("peer1", "b.flac"), Some(&registry), true));
}

/// Turning the setting off is the fastest recovery from a wrong denial, so it has to work
/// without touching the file the denials live in.
#[test]
fn candidate_allowed_verification_off_the_list_is_inert() {
    let registry = RejectedPeerRegistry::in_memory();
    registry.deny(Some("peer1"), Some("a.flac"), "wrong recording", "A - B");

    assert!(candidate_allowed(&hit("peer1", "a.flac"), Some(&registry), false));
}

#[test]
fn candidate_allowed_no_registry_filters_nothing() {
    assert!(candidate_allowed(&hit("peer1", "a.flac"), None, true));
}

// ---- The resolve retry ---------------------------------------------------------------------
//
// slskd marks a transfer Succeeded before moving the file out of its incomplete
// directory, and on bind mounts that move is a copy that can take seconds. The
// one-shot disk check used to miss the mid-move file, fail the attempt, and
// re-download the same track from the next peer. These tests pin the bounded
// re-poll that closes that window.

#[tokio::test]
async fn resolves_immediately_without_waiting() {
    let mut calls = 0;
    let result = retry_resolve(
        || {
            calls += 1;
            Some("/music/song.flac".to_string())
        },
        Duration::from_secs(30),
        Duration::from_millis(10),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(result.as_deref(), Some("/music/song.flac"));
    assert_eq!(calls, 1);
}

#[tokio::test]
async fn resolves_when_file_appears_mid_window() {
    let mut calls = 0;
    let result = retry_resolve(
        || {
            calls += 1;
            (calls >= 3).then(|| "/music/song.flac".to_string())
        },
        Duration::from_secs(30),
        Duration::from_millis(10),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(result.as_deref(), Some("/music/song.flac"));
    assert_eq!(calls, 3);
}

#[tokio::test]
async fn gives_up_after_max_wait() {
    let result = retry_resolve(
        || None,
        Duration::from_millis(100),
        Duration::from_millis(10),
        &CancellationToken::new(),
    )
    .await;

    assert_eq!(result, None);
}

#[tokio::test]
async fn canceled_caller_gets_one_final_check_instead_of_the_window() {
    let cts = CancellationToken::new();
    cts.cancel();

    let mut calls = 0;
    let result = retry_resolve(
        || {
            calls += 1;
            (calls >= 2).then(|| "/music/song.flac".to_string())
        },
        Duration::from_secs(30),
        Duration::from_secs(30),
        &cts,
    )
    .await;

    // First check misses, the delay is canceled, the final check lands.
    assert_eq!(result.as_deref(), Some("/music/song.flac"));
    assert_eq!(calls, 2);
}

// ---- The incomplete folder -----------------------------------------------------------------
//
// slskd marks a transfer Succeeded before moving it out of its incomplete folder, and a
// full-size copy there used to be taken as the download, then deleted under Octo (#69).

const INCOMPLETE_REMOTE: &str = r"Music\Artist\Album\13 - Song.flac";
const INCOMPLETE_LEAF: &str = "13 - Song.flac";
const INCOMPLETE_SIZE: usize = 200_000;

fn resolve_incomplete(root: &Root, remote: &str, excluded: Option<Vec<String>>) -> Option<String> {
    resolve_local_path(
        remote,
        INCOMPLETE_SIZE as i64,
        false,
        &[root.path()],
        &excluded.unwrap_or_else(|| names(&[DEFAULT_INCOMPLETE_FOLDER_NAME])),
        None,
    )
}

#[test]
fn a_full_size_copy_in_the_incomplete_folder_is_never_the_answer() {
    let root = Root::new("octo-incomplete-");
    let partial = root.write(
        &format!("slskd/incomplete/peer/Music/Artist/Album/{INCOMPLETE_LEAF}"),
        INCOMPLETE_SIZE,
    );
    assert_eq!(resolve_incomplete(&root, INCOMPLETE_REMOTE, None), None);
    // The same file with nothing excluded is found, so the None above is the exclusion at work.
    assert_eq!(
        resolve_incomplete(&root, INCOMPLETE_REMOTE, Some(Vec::new())),
        Some(partial)
    );
}

#[test]
fn an_incomplete_folder_named_by_slskd_is_excluded_too() {
    let root = Root::new("octo-incomplete-");
    root.write(
        &format!("slskd/partial/peer/Music/Artist/Album/{INCOMPLETE_LEAF}"),
        INCOMPLETE_SIZE,
    );
    assert_eq!(
        resolve_incomplete(
            &root,
            INCOMPLETE_REMOTE,
            Some(excluded_folder_names(Some(r"D:\slskd\partial\")))
        ),
        None
    );
}

#[test]
fn a_peers_own_folder_called_incomplete_is_still_found() {
    let root = Root::new("octo-incomplete-");
    let final_path = root.write(&format!("slskd/incomplete/{INCOMPLETE_LEAF}"), INCOMPLETE_SIZE);
    assert_eq!(
        resolve_incomplete(&root, r"Music\Artist\incomplete\13 - Song.flac", None),
        Some(final_path)
    );
}

#[test]
fn excluded_names() {
    let cases: [(Option<&str>, &[&str]); 3] = [
        (None, &["incomplete", ".octo-incoming"]),
        (Some("/app/incomplete"), &["incomplete", ".octo-incoming"]),
        (
            Some(r"D:\slskd\Partial\"),
            &["incomplete", "Partial", ".octo-incoming"],
        ),
    ];
    for (configured, expected) in cases {
        assert_eq!(
            excluded_folder_names(configured),
            names(expected),
            "configured {configured:?}"
        );
    }
}

#[test]
fn a_file_in_another_downloads_job_folder_is_never_found_by_name() {
    let root = Root::new("octo-incomplete-");
    // Another download's job folder holds a file with the very same name and size.
    root.write(
        &format!(".octo-incoming/slskd/0123456789abcdef/{INCOMPLETE_LEAF}"),
        INCOMPLETE_SIZE,
    );
    assert_eq!(
        resolve_incomplete(&root, INCOMPLETE_REMOTE, Some(excluded_folder_names(None))),
        None
    );
}

#[tokio::test]
async fn the_wait_outlasts_slskds_move_and_returns_the_final_path() {
    let root = Root::new("octo-incomplete-");
    let partial = root.write(
        &format!("slskd/incomplete/peer/Music/Artist/Album/{INCOMPLETE_LEAF}"),
        INCOMPLETE_SIZE,
    );
    let final_path = format!("{}/slskd/Album/{INCOMPLETE_LEAF}", root.path());
    let (from, to) = (partial.clone(), final_path.clone());
    let mover = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let to_dir = std::path::Path::new(&to)
            .parent()
            .expect("a parent")
            .to_path_buf();
        std::fs::create_dir_all(&to_dir).expect("made");
        std::fs::rename(&from, &to).expect("moved");
        // slskd removes the emptied folder
        std::fs::remove_dir(std::path::Path::new(&from).parent().expect("a parent")).expect("removed");
    });

    let roots = [root.path()];
    let excluded = names(&["incomplete"]);
    let result = retry_resolve(
        || {
            resolve_local_path(
                INCOMPLETE_REMOTE,
                INCOMPLETE_SIZE as i64,
                false,
                &roots,
                &excluded,
                None,
            )
        },
        Duration::from_secs(5),
        Duration::from_millis(10),
        &CancellationToken::new(),
    )
    .await;
    mover.await.expect("the mover finishes");

    assert_eq!(result.as_deref(), Some(final_path.as_str()));
    assert!(std::path::Path::new(&final_path).is_file());
}

#[test]
fn a_copy_slskd_renamed_on_a_clash_is_found() {
    let root = Root::new("octo-incomplete-");
    // slskd names the newcomer <name>_<UTC ticks><ext> when the name is taken.
    root.write(&format!("slskd/Album/{INCOMPLETE_LEAF}"), 1_000); // the older file that took the name
    let renamed = root.write("slskd/Album/13 - Song_638950000000000000.flac", INCOMPLETE_SIZE);
    assert_eq!(resolve_incomplete(&root, INCOMPLETE_REMOTE, None), Some(renamed));
}

#[test]
fn a_name_that_only_looks_renamed_is_not() {
    let root = Root::new("octo-incomplete-");
    root.write("slskd/Album/13 - Song_2.flac", INCOMPLETE_SIZE);
    root.write("slskd/Other/13 - Song_638950000000000000.flac", INCOMPLETE_SIZE);
    assert_eq!(resolve_incomplete(&root, INCOMPLETE_REMOTE, None), None);
}

// ---- Parallel downloads: resolve_in_job and the job folders --------------------------------
//
// Downloads side by side were unsafe while a finished file was found by its name anywhere under
// the music folder. Each Soulseek download now lands in a job folder of its own; these pin down
// finding it there and nowhere else.

const JOB_REMOTE: &str = r"Music\Artist\Album\03 - Song.flac";
const JOB: &str = ".octo-incoming/slskd/aaaa";

#[test]
fn the_file_in_its_own_job_folder_is_found() {
    let root = Root::new("octo-parallel-");
    let mine = root.write(&format!("{JOB}/03 - Song.flac"), 1000);
    assert_eq!(
        resolve_in_job(&[root.path()], JOB, JOB_REMOTE, 1000, false),
        Some(mine)
    );
}

#[test]
fn slskds_renamed_copy_inside_the_job_folder_is_found() {
    let root = Root::new("octo-parallel-");
    let renamed = root.write(&format!("{JOB}/03 - Song_638631234567890123.flac"), 1000);
    assert_eq!(
        resolve_in_job(&[root.path()], JOB, JOB_REMOTE, 1000, false),
        Some(renamed)
    );
}

#[test]
fn a_cleaned_up_name_is_found_by_its_exact_size() {
    let root = Root::new("octo-parallel-");
    let cleaned = root.write(&format!("{JOB}/03 _ Song.flac"), 1000);
    assert_eq!(
        resolve_in_job(&[root.path()], JOB, JOB_REMOTE, 1000, false),
        Some(cleaned)
    );
}

#[test]
fn the_same_file_in_another_job_or_the_music_root_is_never_taken() {
    let root = Root::new("octo-parallel-");
    root.write(".octo-incoming/slskd/bbbb/03 - Song.flac", 1000);
    root.write("Music/Artist/Album/03 - Song.flac", 1000);
    root.write("03 - Song.flac", 1000);
    assert_eq!(resolve_in_job(&[root.path()], JOB, JOB_REMOTE, 1000, false), None);
}

#[test]
fn an_interrupted_transfer_needs_the_exact_size() {
    let root = Root::new("octo-parallel-");
    root.write(&format!("{JOB}/03 - Song.flac"), 1000 - 10);
    assert!(resolve_in_job(&[root.path()], JOB, JOB_REMOTE, 1000, false).is_some());
    assert_eq!(resolve_in_job(&[root.path()], JOB, JOB_REMOTE, 1000, true), None);
}

#[test]
fn job_folders_are_dot_folders_and_unique() {
    let a = new_job_dir();
    let b = new_job_dir();
    assert!(a.starts_with(".octo-incoming/slskd/"), "{a}");
    assert_ne!(a, b);
    assert!(!a.contains(".."), "{a}");
}

// ---- The transcode decision: weigh_transcode -----------------------------------------------

fn held() -> String {
    format!("{}/music/Artist/Song.flac", std::env::temp_dir().display())
}

fn other() -> String {
    format!("{}/music/Peer/Song.flac", std::env::temp_dir().display())
}

#[test]
fn the_first_transcode_is_held_back() {
    assert_eq!(
        weigh_transcode(None, None, &other(), Some(16900.0)),
        ReserveChoice::Hold
    );
}

/// The higher cutoff was made from the higher bitrate, so it replaces the one held.
#[test]
fn a_higher_cutoff_replaces_the_one_held() {
    assert_eq!(
        weigh_transcode(Some(&held()), Some(16900.0), &other(), Some(20350.0)),
        ReserveChoice::Hold
    );
}

#[test]
fn a_no_better_transcode_is_discarded() {
    for cutoff in [16900.0, 15000.0] {
        assert_eq!(
            weigh_transcode(Some(&held()), Some(16900.0), &other(), Some(cutoff)),
            ReserveChoice::DiscardNew,
            "cutoff {cutoff}"
        );
    }
}

/// The resolver can find the file already held back for a later peer offering the
/// same rip. Deleting it as a new, worse copy would lose the only copy there is.
#[test]
fn the_copy_already_held_is_never_discarded_as_a_new_one() {
    assert_eq!(
        weigh_transcode(Some(&held()), Some(16900.0), &held(), Some(16900.0)),
        ReserveChoice::AlreadyHeld
    );
}

// ---- Album folders: the walk, through the real download service against a fake slskd -------

/// A FLAC whose STREAMINFO says it lasts this long, so the length check passes.
fn flac(seconds: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"fLaC");
    bytes.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
    bytes.extend_from_slice(&[0x10, 0x00, 0x10, 0x00]);
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    let (sample_rate, channels_minus_one, bits_minus_one): (u64, u64, u64) = (44100, 1, 15);
    let total_samples = 44100u64 * seconds;
    let packed = (sample_rate << 44) | (channels_minus_one << 41) | (bits_minus_one << 36) | total_samples;
    bytes.extend_from_slice(&packed.to_be_bytes());
    bytes.extend_from_slice(&[0u8; 16]);
    bytes
}

type Answers = Arc<dyn Fn(&str) -> Vec<Value> + Send + Sync>;
type Folders = Arc<dyn Fn(&str, &str) -> Option<Value> + Send + Sync>;

#[derive(Default)]
struct SlskdState {
    searches: Vec<String>,
    batches: Vec<(String, Vec<String>, String)>,
    browses: Vec<(String, String)>,
    search_text: HashMap<String, String>,
    transfers: HashMap<String, Vec<Value>>,
    lengths: HashMap<String, u64>,
    ids: i32,
    /// An slskd without batch downloads: the old one-file enqueue, filed by the peer's folder.
    no_batches: bool,
    /// The files the old enqueue was asked for.
    enqueued: Vec<String>,
    /// What the yt-dlp shim was asked to fetch.
    you_tube_downloads: Vec<String>,
    /// The rows YouTube Music answers a songs search with; none answers 404.
    ytm_tracks: Option<Value>,
    /// Files written with some audio after the STREAMINFO. TagLib# (and the tag reader after it)
    /// gives a FLAC with no audio no length at all, so only these have one to check.
    with_audio: HashSet<String>,
    /// Files written as these exact bytes: real audio, for the checks that listen to it.
    sound: HashMap<String, Vec<u8>>,
    /// The audio the shim's /stream sends whole; none answers the two-byte partial reply.
    preview: Option<Vec<u8>>,
    /// The videos the shim's /search answers with, best first; the first one not skipped wins.
    /// Empty answers the one video "vid123".
    search_videos: Vec<String>,
    /// The audio the shim's /download writes; none writes silent frames.
    download_audio: Option<Vec<u8>>,
    /// What AcoustID answers for the file of each video, by video id.
    lookup_by_video: HashMap<String, Value>,
    /// The video the shim wrote a file for last.
    last_download: String,
    /// Videos whose /download the shim fails with a 502.
    failing_downloads: HashSet<String>,
}

/// slskd as far as one walk needs: logged in, searches answered from a script, and every batch
/// file written straight into its destination folder and reported finished.
#[derive(Clone)]
struct FakeSlskd {
    root: String,
    responses_for: Answers,
    folders: Arc<Mutex<Option<Folders>>>,
    state: Arc<Mutex<SlskdState>>,
}

impl FakeSlskd {
    fn length(&self, remote: &str, seconds: i32) {
        self.state
            .lock()
            .lengths
            .insert(remote.to_string(), seconds as u64);
    }

    /// What a peer lists for one folder of its share, by user and folder; None lists nothing.
    fn set_folders(&self, folders: impl Fn(&str, &str) -> Option<Value> + Send + Sync + 'static) {
        *self.folders.lock() = Some(Arc::new(folders));
    }

    fn searches(&self) -> Vec<String> {
        self.state.lock().searches.clone()
    }

    fn batches(&self) -> Vec<(String, Vec<String>, String)> {
        self.state.lock().batches.clone()
    }

    fn browses(&self) -> Vec<(String, String)> {
        self.state.lock().browses.clone()
    }
}

fn json_answer(body: &Value, status: u16) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_raw(body.to_string().into_bytes(), "application/json")
}

impl Respond for FakeSlskd {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path().to_string();
        let segments: Vec<&str> = path.split('/').collect();
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let method = request.method.as_str();
        if path == "/api/v0/session" {
            return json_answer(&json!({"token": "jwt", "expires": 4102444800i64}), 200);
        }
        if path == "/api/v0/options" {
            return json_answer(
                &json!({"directories": {"downloads": self.root, "incomplete": "/app/incomplete"}}),
                200,
            );
        }
        if path == "/api/v0/searches" && method == "POST" {
            let text = body["searchText"].as_str().unwrap_or_default().to_string();
            let id = body["id"].as_str().unwrap_or_default().to_string();
            let mut state = self.state.lock();
            state.search_text.insert(id, text.clone());
            state.searches.push(text);
            return json_answer(&json!({}), 200);
        }
        if path.starts_with("/api/v0/searches/") && path.ends_with("/responses") {
            let text = self
                .state
                .lock()
                .search_text
                .get(segments[4])
                .cloned()
                .unwrap_or_default();
            return json_answer(&Value::Array((self.responses_for)(&text)), 200);
        }
        if path.starts_with("/api/v0/searches/") {
            return json_answer(
                &json!({"state": "Completed, ResponseLimitReached", "endedAt": "2026-10-03T12:00:00Z", "responseCount": 1}),
                200,
            );
        }
        if path.starts_with("/api/v0/users/") && path.ends_with("/directory") && method == "POST" {
            let user = segments[4].to_string();
            let folder = body["directory"].as_str().unwrap_or_default().to_string();
            self.state.lock().browses.push((user.clone(), folder.clone()));
            let folders = self.folders.lock().clone();
            return match folders.and_then(|f| f(&user, &folder)) {
                Some(listing) => json_answer(&listing, 200),
                None => ResponseTemplate::new(404),
            };
        }
        if path == "/api/v0/transfers/downloads/batches" && self.state.lock().no_batches {
            return ResponseTemplate::new(404);
        }
        if path.starts_with("/api/v0/transfers/downloads/")
            && path != "/api/v0/transfers/downloads/batches"
            && method == "POST"
        {
            // The old way: slskd files the download under the peer's own folder name.
            let user = segments[5].to_string();
            let mut state = self.state.lock();
            for request in body.as_array().cloned().unwrap_or_default() {
                let file = request["filename"].as_str().unwrap_or_default().to_string();
                let normalized = file.replace('\\', "/");
                let parts: Vec<&str> = normalized.split('/').collect();
                let leaf = parts[parts.len() - 1];
                let parent = parts[parts.len() - 2];
                let local = std::path::Path::new(&self.root).join(parent).join(leaf);
                std::fs::create_dir_all(local.parent().expect("a parent")).expect("made");
                let bytes = flac(state.lengths.get(&file).copied().unwrap_or(200));
                std::fs::write(&local, &bytes).expect("written");
                state.enqueued.push(file.clone());
                let transfer = json!({
                    "filename": file, "state": "Completed, Succeeded",
                    "size": bytes.len() as i64, "bytesTransferred": bytes.len() as i64,
                });
                state.transfers.entry(user.clone()).or_default().push(transfer);
            }
            return json_answer(&json!({}), 201);
        }
        if path == "/ytm/search" {
            return match self.state.lock().ytm_tracks.clone() {
                Some(tracks) => json_answer(&json!({"tracks": tracks}), 200),
                None => ResponseTemplate::new(404),
            };
        }
        if path == "/search" {
            let skipped: Vec<String> = request
                .url
                .query_pairs()
                .find(|(k, _)| k == "skip")
                .map(|(_, v)| v.split(',').map(str::to_string).collect())
                .unwrap_or_default();
            let video = {
                let state = self.state.lock();
                if state.search_videos.is_empty() {
                    Some("vid123".to_string())
                } else {
                    state.search_videos.iter().find(|v| !skipped.contains(v)).cloned()
                }
            };
            let Some(video) = video else {
                return json_answer(&json!({"error": "no_hit"}), 404);
            };
            return json_answer(
                &json!({"video_id": video, "title": "Started", "duration": 180,
                        "size": 12000, "itag": "140", "ext": "m4a", "abr": 129.5}),
                200,
            );
        }
        if path == "/v2/lookup" {
            // The answer set for the video last downloaded; else an AcoustID that has never
            // heard any of it.
            let state = self.state.lock();
            let answer = state
                .lookup_by_video
                .get(&state.last_download)
                .cloned()
                .unwrap_or_else(|| json!({"status": "ok", "results": []}));
            return json_answer(&answer, 200);
        }
        if path == "/stream"
            && let Some(preview) = self.state.lock().preview.clone()
        {
            return ResponseTemplate::new(200).set_body_raw(preview, "audio/flac");
        }
        if path == "/stream" {
            return ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 0-1/12345")
                .set_body_raw(vec![1u8, 2], "audio/mp4");
        }
        if path == "/download"
            && request
                .url
                .query_pairs()
                .any(|(k, v)| k == "id" && self.state.lock().failing_downloads.contains(v.as_ref()))
        {
            return json_answer(&json!({"error": "no_audio_url"}), 502);
        }
        if path == "/download" {
            let dest = request
                .url
                .query_pairs()
                .find(|(k, _)| k == "dest")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();
            let file = format!("{dest}.mp3");
            std::fs::create_dir_all(std::path::Path::new(&file).parent().expect("a parent")).expect("made");
            let audio = self
                .state
                .lock()
                .download_audio
                .clone()
                .unwrap_or_else(crate::services::test_support::mp3);
            std::fs::write(&file, audio).expect("written");
            let video = request
                .url
                .query_pairs()
                .find(|(k, _)| k == "id")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();
            let mut state = self.state.lock();
            state.last_download = video;
            state.you_tube_downloads.push(file.clone());
            return json_answer(&json!({"path": file}), 200);
        }
        if path == "/api/v0/transfers/downloads/batches" {
            let user = body["username"].as_str().unwrap_or_default().to_string();
            let destination = body["options"]["destination"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let files: Vec<String> = body["files"]
                .as_array()
                .map(|files| {
                    files
                        .iter()
                        .map(|f| f["filename"].as_str().unwrap_or_default().to_string())
                        .collect()
                })
                .unwrap_or_default();
            let mut state = self.state.lock();
            state
                .batches
                .push((user.clone(), files.clone(), destination.clone()));
            let mut queued = Vec::new();
            for file in &files {
                let leaf = file
                    .replace('\\', "/")
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                let local = std::path::Path::new(&self.root).join(&destination).join(&leaf);
                std::fs::create_dir_all(local.parent().expect("a parent")).expect("made");
                let mut bytes = state
                    .sound
                    .get(file)
                    .cloned()
                    .unwrap_or_else(|| flac(state.lengths.get(file).copied().unwrap_or(200)));
                if state.with_audio.contains(file) {
                    // Enough for a bitrate of at least 1 kbps, within the 64 KB size drift of the advertised size.
                    bytes.extend_from_slice(&[0u8; 48_000]);
                }
                std::fs::write(&local, &bytes).expect("written");
                state.ids += 1;
                let transfer = json!({
                    "id": format!("t-{}", state.ids), "filename": file, "state": "Completed, Succeeded",
                    "size": bytes.len() as i64, "bytesTransferred": bytes.len() as i64, "percentComplete": 100.0,
                });
                state
                    .transfers
                    .entry(user.clone())
                    .or_default()
                    .push(transfer.clone());
                queued.push(transfer);
            }
            return json_answer(
                &json!({"batch": {"username": user, "transfers": queued}, "failures": []}),
                201,
            );
        }
        if path.starts_with("/api/v0/transfers/downloads/") && method == "GET" {
            let user = segments[5].to_string();
            let files = self
                .state
                .lock()
                .transfers
                .get(&user)
                .cloned()
                .unwrap_or_default();
            return json_answer(
                &json!({"username": user, "directories": [{"directory": "x", "files": files}]}),
                200,
            );
        }
        json_answer(&json!({}), 200)
    }
}

/// A Review queue that keeps every verdict a download asked about.
#[derive(Default)]
struct AskedReviews(Mutex<Vec<VerificationResult>>);

impl crate::services::common::ReviewQueue for AskedReviews {
    fn add_review(&self, _: &str, _: &str, _: &Song, verdict: &VerificationResult) -> bool {
        self.0.lock().push(verdict.clone());
        true
    }
}

struct Walk {
    service: Arc<BaseDownloadService>,
    registry: Arc<ExternalIdRegistry>,
    slskd: FakeSlskd,
    songs: Vec<Song>,
    album_id: String,
    rejected: Arc<RejectedPeerRegistry>,
    settings: Arc<SettingsStore>,
    /// What downloads asked about in Review, once Review is switched on in `settings`.
    reviews: Arc<AskedReviews>,
    _server: MockServer,
}

const TITLES: [(&str, i32); 4] = [
    ("Intro", 90),
    ("Hold On", 200),
    ("Hold On, We're Going Home", 228),
    ("Started", 180),
];

async fn build(root: &Root, album_folders: bool, answers: impl FnOnce(Vec<Song>) -> Answers) -> Walk {
    build_with(root, album_folders, DownloadSource::Soulseek, answers).await
}

async fn build_with(
    root: &Root,
    album_folders: bool,
    source: DownloadSource,
    answers: impl FnOnce(Vec<Song>) -> Answers,
) -> Walk {
    build_with_soulseek(root, album_folders, source, true, answers).await
}

async fn build_with_soulseek(
    root: &Root,
    album_folders: bool,
    source: DownloadSource,
    soulseek_on: bool,
    answers: impl FnOnce(Vec<Song>) -> Answers,
) -> Walk {
    build_full(root, album_folders, source, soulseek_on, false, answers).await
}

/// `verified`: downloads are checked, with an AcoustID key, against the fake's AcoustID.
async fn build_full(
    root: &Root,
    album_folders: bool,
    source: DownloadSource,
    soulseek_on: bool,
    verified: bool,
    answers: impl FnOnce(Vec<Song>) -> Answers,
) -> Walk {
    build_full_copies(root, album_folders, source, soulseek_on, verified, None, answers).await
}

/// `build_full` with the service keeping `preview_copies`.
async fn build_full_copies(
    root: &Root,
    album_folders: bool,
    source: DownloadSource,
    soulseek_on: bool,
    verified: bool,
    preview_copies: Option<Arc<PreviewCopies>>,
    answers: impl FnOnce(Vec<Song>) -> Answers,
) -> Walk {
    build_all(
        root,
        album_folders,
        source,
        soulseek_on,
        verified,
        preview_copies,
        None,
        answers,
    )
    .await
}

/// `build_full_copies` with the service fetching through `lidarr_fetcher`.
#[allow(clippy::too_many_arguments)]
async fn build_all(
    root: &Root,
    album_folders: bool,
    source: DownloadSource,
    soulseek_on: bool,
    verified: bool,
    preview_copies: Option<Arc<PreviewCopies>>,
    lidarr_fetcher: Option<Arc<dyn ILidarrTrackFetcher>>,
    answers: impl FnOnce(Vec<Song>) -> Answers,
) -> Walk {
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let songs: Vec<Song> = TITLES
        .iter()
        .enumerate()
        .map(|(i, (title, seconds))| {
            let id = registry.register(SoulseekRouting {
                kind: RoutingKind::Song,
                artist: Some("Drake".into()),
                title: Some(title.to_string()),
                album: Some("Nothing Was the Same".into()),
                duration: Some(*seconds),
                track: Some(i as i32 + 1),
                ..Default::default()
            });
            Song {
                id: format!("ext-soulseek-{id}"),
                external_provider: Some("soulseek".into()),
                external_id: Some(id),
                title: title.to_string(),
                artist: "Drake".into(),
                album: "Nothing Was the Same".into(),
                duration: Some(*seconds),
                track: Some(i as i32 + 1),
                ..Default::default()
            }
        })
        .collect();

    let slskd = FakeSlskd {
        root: root.path(),
        responses_for: answers(songs.clone()),
        folders: Arc::new(Mutex::new(None)),
        state: Arc::new(Mutex::new(SlskdState::default())),
    };
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(slskd.clone())
        .mount(&server)
        .await;

    let soulseek = SoulseekSettings {
        enabled: soulseek_on,
        base_url: Some(server.uri()),
        username: Some("u".into()),
        password: Some("p".into()),
        min_file_size_bytes: 0,
        album_folders,
        parallel_downloads: 3,
        detect_transcodes: false,
        verify_downloads: verified,
        acoust_id_api_key: if verified {
            "key".into()
        } else {
            Default::default()
        },
        download_timeout_seconds: 30,
        ..Default::default()
    };
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        soulseek: soulseek.clone(),
        subsonic: SubsonicSettings {
            auto_detect_download_path: false,
            download_source: source,
            ..Default::default()
        },
        ..Default::default()
    }));
    settings.set_raw("Library:DownloadPath", Some(&root.path()));
    let client = SoulseekClient::with_timings(
        &soulseek,
        Timings {
            search_poll_interval: Duration::from_millis(5),
            poll_interval: Duration::from_millis(5),
            min_search_spacing: Duration::ZERO,
            ..Default::default()
        },
    );

    let metadata = FakeMetadata::default();
    let album = Album {
        id: "album-1".into(),
        title: "Nothing Was the Same".into(),
        artist: "Drake".into(),
        songs: songs.clone(),
        ..Default::default()
    };
    metadata
        .albums
        .lock()
        .insert(("soulseek".into(), "album-1".into()), album);
    // A song whose id the registry never minted: nothing says what to search for.
    metadata.songs.lock().insert(
        ("soulseek".into(), "unknown-id".into()),
        Song {
            title: "Mystery".into(),
            artist: "Nobody".into(),
            external_provider: Some("soulseek".into()),
            external_id: Some("unknown-id".into()),
            ..Default::default()
        },
    );
    for song in &songs {
        metadata.songs.lock().insert(
            ("soulseek".into(), song.external_id.clone().expect("an id")),
            song.clone(),
        );
    }

    let core = DownloadCore {
        settings: Arc::clone(&settings),
        local_library: Arc::new(FakeLocalLibrary::default()),
        metadata: Arc::new(metadata),
        navidrome_identity: NavidromeIdentityService::new(Arc::clone(&settings), reqwest::Client::new()),
        history: Arc::new(DownloadHistoryService::new(root.0.path().join("history.json"))),
        notifications: Arc::new(NotificationService::new(Vec::new(), Arc::clone(&settings), None)),
    };
    let reviews = Arc::new(AskedReviews::default());
    let services = DownloadServices {
        concurrency: Some(Arc::new(DownloadConcurrency::new(Arc::clone(&settings)))),
        review_queue: Some(reviews.clone()),
        ..Default::default()
    };
    let acoust_id = url::Url::parse(&format!("{}/", server.uri())).expect("parses");
    let verification = Arc::new(DownloadVerificationService::new(
        Arc::new(AudioFingerprinter::new()),
        Arc::new(AcoustIdClient::new(
            Arc::new(AcoustIdRateLimitHandler::with_base_url(
                Arc::new(AcoustIdRateLimiter::new()),
                acoust_id,
            )),
            Arc::clone(&settings),
        )),
        Arc::clone(&settings),
        None,
        None,
    ));
    let rejected = Arc::new(RejectedPeerRegistry::in_memory());
    let parts = SoulseekDownloadParts {
        slskd: client,
        rejected_peers: Arc::clone(&rejected),
        verification,
        youtube: Arc::new(YouTubeResolver::with_base_url(Some(&server.uri()))),
        id_registry: registry.clone(),
        soulseek_link: None,
        lidarr_imports: None,
        lidarr_fetcher,
        spotiflac: None,
        preview_copies,
    };
    let (service, _) = SoulseekDownloadService::build(core, services, parts);
    Walk {
        service,
        registry,
        slskd,
        songs,
        album_id: "album-1".into(),
        rejected,
        settings,
        reviews,
        _server: server,
    }
}

fn response(user: &str, files: &[(String, i32)]) -> Value {
    json!({
        "username": user, "uploadSpeed": 1_000_000, "queueLength": 0, "hasFreeUploadSlot": true,
        "files": files.iter().map(|(file, seconds)| json!({
            "filename": file, "size": flac(*seconds as u64).len() as i64, "length": seconds, "extension": "flac",
        })).collect::<Vec<Value>>(),
    })
}

fn remote(folder: &str, song: &Song) -> String {
    format!(
        "{folder}\\{:02} - {}.flac",
        song.track.expect("a number"),
        song.title
    )
}

fn duration_of(song: &Song) -> i32 {
    song.duration.expect("a length")
}

/// Every FLAC under the root that is not in the staging folder.
fn placed_flacs(root: &Root) -> usize {
    files_under(&root.path())
        .expect("the root lists")
        .iter()
        .filter(|f| f.ends_with(".flac") && !f.contains(".octo-incoming"))
        .count()
}

async fn walk_album(walk: &Walk) -> bool {
    tokio::time::timeout(
        Duration::from_secs(60),
        walk.service.download_album_with_source(
            "soulseek",
            &walk.album_id,
            DownloadSource::Soulseek,
            false,
            &CancellationToken::new(),
            None,
        ),
    )
    .await
    .expect("the walk ends within a minute")
    .expect("the walk runs")
}

#[tokio::test]
async fn a_whole_folder_is_one_search_and_one_batch_and_every_song_lands() {
    let root = Root::new("octo-albumwalk-");
    let walk = build(&root, true, |songs| {
        Arc::new(move |text: &str| {
            if text == "Drake Nothing Was the Same" {
                let files: Vec<(String, i32)> = songs
                    .iter()
                    .map(|s| (remote(r"Music\Drake\NWTS", s), duration_of(s)))
                    .collect();
                vec![response("albumpeer", &files)]
            } else {
                Vec::new()
            }
        })
    })
    .await;
    for song in &walk.songs {
        walk.slskd
            .length(&remote(r"Music\Drake\NWTS", song), duration_of(song));
    }

    assert!(walk_album(&walk).await);

    assert_eq!(walk.slskd.searches(), names(&["Drake Nothing Was the Same"]));
    let batches = walk.slskd.batches();
    assert_eq!(batches.len(), 1);
    let (user, files, destination) = &batches[0];
    assert_eq!(user, "albumpeer");
    assert_eq!(files.len(), 4);
    assert!(destination.starts_with(".octo-incoming/slskd/"), "{destination}");
    assert_eq!(placed_flacs(&root), 4);
}

/// Two hearts of one album at once (two of its songs, or one heart sent twice) walk it once. Two
/// walks side by side each queued their own batch for the same songs, and the second batch took
/// the first's place in the songs' queue, so no song waited on the first batch and nothing cancelled
/// it: its files landed in a job folder nobody looked in.
#[tokio::test]
async fn two_walks_of_one_album_at_once_queue_one_batch() {
    let root = Root::new("octo-albumwalk-");
    let walk = build(&root, true, |songs| {
        Arc::new(move |text: &str| {
            if text == "Drake Nothing Was the Same" {
                let files: Vec<(String, i32)> = songs
                    .iter()
                    .map(|s| (remote(r"Music\Drake\NWTS", s), duration_of(s)))
                    .collect();
                vec![response("albumpeer", &files)]
            } else {
                Vec::new()
            }
        })
    })
    .await;
    for song in &walk.songs {
        walk.slskd
            .length(&remote(r"Music\Drake\NWTS", song), duration_of(song));
    }

    let (first, second) = tokio::join!(walk_album(&walk), walk_album(&walk));

    assert!(first && second);
    assert_eq!(walk.slskd.searches(), names(&["Drake Nothing Was the Same"]));
    assert_eq!(walk.slskd.batches().len(), 1, "{:?}", walk.slskd.batches());
    assert_eq!(placed_flacs(&root), 4);
}

/// The album is let go once its walk ends, so a later heart walks it again.
#[tokio::test]
async fn a_walk_after_the_last_one_ended_runs_again() {
    let root = Root::new("octo-albumwalk-");
    let walk = build(&root, true, |songs| {
        Arc::new(move |text: &str| {
            if text == "Drake Nothing Was the Same" {
                let files: Vec<(String, i32)> = songs
                    .iter()
                    .map(|s| (remote(r"Music\Drake\NWTS", s), duration_of(s)))
                    .collect();
                vec![response("albumpeer", &files)]
            } else {
                Vec::new()
            }
        })
    })
    .await;
    for song in &walk.songs {
        walk.slskd
            .length(&remote(r"Music\Drake\NWTS", song), duration_of(song));
    }

    assert!(walk_album(&walk).await);
    walk_album(&walk).await;

    assert_eq!(
        walk.slskd.searches(),
        names(&["Drake Nothing Was the Same", "Drake Nothing Was the Same"])
    );
}

/// The album search answers the whole folder from one peer; with `slow`, its first answer takes
/// half a second, long enough to catch the walk in the middle.
fn the_album_folder(slow: bool) -> impl FnOnce(Vec<Song>) -> Answers {
    move |songs| {
        let first = Arc::new(Mutex::new(slow));
        Arc::new(move |text: &str| {
            if text != "Drake Nothing Was the Same" {
                return Vec::new();
            }
            if std::mem::take(&mut *first.lock()) {
                std::thread::sleep(Duration::from_millis(500));
            }
            let files: Vec<(String, i32)> = songs
                .iter()
                .map(|s| (remote(r"Music\Drake\NWTS", s), duration_of(s)))
                .collect();
            vec![response("albumpeer", &files)]
        })
    }
}

/// A walk of the album on its own task, for `source`, until `token` is cancelled.
fn spawn_walk(
    walk: &Walk,
    source: DownloadSource,
    token: CancellationToken,
) -> tokio::task::JoinHandle<bool> {
    let (service, album) = (Arc::clone(&walk.service), walk.album_id.clone());
    tokio::spawn(async move {
        service
            .download_album_with_source("soulseek", &album, source, false, &token, None)
            .await
            .expect("the walk runs")
    })
}

/// Waits until the fake slskd has been asked for a search.
async fn until_searched(walk: &Walk) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while walk.slskd.searches().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the walk searches");
}

/// A walk that fails answers false to the request that waited on it too.
#[tokio::test]
async fn a_walk_that_fails_answers_false_to_its_waiter() {
    let root = Root::new("octo-albumwalk-");
    let walk = build(&root, true, |_| Arc::new(|_: &str| Vec::new())).await;

    let (first, second) = tokio::join!(walk_album(&walk), walk_album(&walk));

    assert!(!first && !second);
    // The two asked for at once searched exactly as much as one walk on its own does.
    let together = walk.slskd.searches();
    assert!(!walk_album(&walk).await);
    assert_eq!(walk.slskd.searches().len(), 2 * together.len(), "{together:?}");
    assert_eq!(placed_flacs(&root), 0);
}

/// A walk dropped halfway gives the album up, so the next walk searches for it again.
#[tokio::test]
async fn an_aborted_walk_lets_the_album_go() {
    let root = Root::new("octo-albumwalk-");
    let walk = build(&root, true, the_album_folder(true)).await;
    for song in &walk.songs {
        walk.slskd
            .length(&remote(r"Music\Drake\NWTS", song), duration_of(song));
    }

    let aborted = spawn_walk(&walk, DownloadSource::Soulseek, CancellationToken::new());
    until_searched(&walk).await;
    aborted.abort();
    assert!(aborted.await.is_err_and(|e| e.is_cancelled()));

    assert!(walk_album(&walk).await);
    assert_eq!(
        walk.slskd.searches(),
        names(&["Drake Nothing Was the Same", "Drake Nothing Was the Same"])
    );
    assert_eq!(placed_flacs(&root), 4);
}

/// A request that leaves while it waits answers false at once, and the walk it waited on goes on
/// to the end.
#[tokio::test]
async fn a_cancelled_waiter_answers_false_and_the_walk_goes_on() {
    let root = Root::new("octo-albumwalk-");
    let walk = build(&root, true, the_album_folder(true)).await;
    for song in &walk.songs {
        walk.slskd
            .length(&remote(r"Music\Drake\NWTS", song), duration_of(song));
    }

    let running = spawn_walk(&walk, DownloadSource::Soulseek, CancellationToken::new());
    until_searched(&walk).await;
    let leaving = CancellationToken::new();
    let waiter = spawn_walk(&walk, DownloadSource::Soulseek, leaving.clone());
    tokio::time::sleep(Duration::from_millis(50)).await;
    leaving.cancel();

    let waited = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("the waiter leaves at once")
        .expect("the waiter ends");
    assert!(!waited);
    assert!(!running.is_finished());
    let walked = tokio::time::timeout(Duration::from_secs(60), running)
        .await
        .expect("the walk ends within a minute")
        .expect("the walk ends");
    assert!(walked);
    assert_eq!(walk.slskd.searches(), names(&["Drake Nothing Was the Same"]));
    assert_eq!(placed_flacs(&root), 4);
}

/// A Soulseek walk asked for while a YouTube walk of the same album runs does not take the YouTube
/// walk's answer: it waits for that walk to end and then looks for the lossless copies itself.
#[tokio::test]
async fn a_soulseek_walk_during_a_you_tube_walk_runs_its_own_walk_afterwards() {
    let root = Root::new("octo-albumwalk-");
    let walk = build(&root, true, the_album_folder(false)).await;
    for song in &walk.songs {
        walk.slskd
            .length(&remote(r"Music\Drake\NWTS", song), duration_of(song));
    }
    let token = CancellationToken::new();
    let walk_from = |source| {
        tokio::time::timeout(
            Duration::from_secs(60),
            walk.service
                .download_album_with_source("soulseek", &walk.album_id, source, false, &token, None),
        )
    };

    let (you_tube, soulseek) = tokio::join!(
        walk_from(DownloadSource::YouTube),
        walk_from(DownloadSource::Soulseek)
    );

    assert!(you_tube.expect("in time").expect("the YouTube walk runs"));
    assert!(soulseek.expect("in time").expect("the Soulseek walk runs"));
    assert_eq!(walk.slskd.state.lock().you_tube_downloads.len(), 4);
    assert_eq!(walk.slskd.searches(), names(&["Drake Nothing Was the Same"]));
}

#[tokio::test]
async fn a_song_the_folder_lacks_is_searched_on_its_own() {
    let root = Root::new("octo-albumwalk-");
    let walk = build(&root, true, |songs| {
        Arc::new(move |text: &str| {
            if text == "Drake Nothing Was the Same" {
                let files: Vec<(String, i32)> = songs
                    .iter()
                    .take(3)
                    .map(|s| (remote(r"Music\NWTS", s), duration_of(s)))
                    .collect();
                vec![response("albumpeer", &files)]
            } else if text.contains("Started") {
                vec![response(
                    "songpeer",
                    &[(remote("Singles", &songs[3]), duration_of(&songs[3]))],
                )]
            } else {
                Vec::new()
            }
        })
    })
    .await;
    for song in walk.songs.iter().take(3) {
        walk.slskd.length(&remote(r"Music\NWTS", song), duration_of(song));
    }
    walk.slskd
        .length(&remote("Singles", &walk.songs[3]), duration_of(&walk.songs[3]));

    assert!(walk_album(&walk).await);

    let searches = walk.slskd.searches();
    assert_eq!(searches.len(), 2, "{searches:?}");
    assert!(searches.iter().any(|s| s.contains("Started")), "{searches:?}");
    let mut users: Vec<String> = walk.slskd.batches().into_iter().map(|(u, ..)| u).collect();
    users.sort();
    assert_eq!(users, names(&["albumpeer", "songpeer"]));
}

#[tokio::test]
async fn with_album_folders_off_every_song_is_searched_on_its_own() {
    let root = Root::new("octo-albumwalk-");
    let walk = build(&root, false, |songs| {
        Arc::new(move |text: &str| {
            let found = songs.iter().find(|s| {
                let first = s.title.split(',').next().unwrap_or_default();
                text.contains(first) && !(s.title == "Hold On" && text.contains("Home"))
            });
            match found {
                Some(song) => vec![response("peer", &[(remote("x", song), duration_of(song))])],
                None => Vec::new(),
            }
        })
    })
    .await;
    for song in &walk.songs {
        walk.slskd.length(&remote("x", song), duration_of(song));
    }

    walk_album(&walk).await;

    let searches = walk.slskd.searches();
    assert!(
        !searches.iter().any(|s| s == "Drake Nothing Was the Same"),
        "{searches:?}"
    );
    assert!(searches.len() >= 4, "{searches:?}");
    assert!(walk.slskd.batches().iter().all(|(_, files, _)| files.len() == 1));
}

fn lossy_response(user: &str, file: &str, seconds: i32) -> Value {
    json!({
        "username": user, "uploadSpeed": 1_000_000, "queueLength": 0, "hasFreeUploadSlot": true,
        "files": [{"filename": file, "size": 8_000_000i64, "length": seconds, "extension": "mp3", "bitRate": 320}],
    })
}

const NWTS_FOLDER: &str = r"Music\Drake\Nothing Was the Same";

/// #70: the search finds the song only as an MP3, and the FLAC sits beside it in the same
/// folder, unanswered. Octo asks the peer for that folder and takes the FLAC from it.
#[tokio::test]
async fn a_flac_beside_the_only_mp3_the_search_found_is_taken() {
    let root = Root::new("octo-albumwalk-");
    let walk = build(&root, false, |_| {
        Arc::new(|text: &str| {
            if text.contains("Started") {
                vec![lossy_response(
                    "bothpeer",
                    &format!("{NWTS_FOLDER}\\04 - Started.mp3"),
                    180,
                )]
            } else {
                Vec::new()
            }
        })
    })
    .await;
    let started = walk.songs[3].clone();
    let flac_remote = format!("{NWTS_FOLDER}\\04 - Started.flac");
    walk.slskd.length(&flac_remote, 180);
    walk.slskd.set_folders(|user, dir| {
        (user == "bothpeer" && dir == NWTS_FOLDER).then(|| {
            json!({
                "name": NWTS_FOLDER,
                "files": [
                    {"filename": "04 - Started.mp3", "size": 8_000_000i64, "extension": "mp3", "length": 180},
                    {"filename": "04 - Started.flac", "size": flac(180).len() as i64, "extension": "flac", "length": 180},
                    {"filename": "03 - Hold On, We're Going Home.flac", "size": flac(228).len() as i64, "extension": "flac", "length": 228},
                ],
            })
        })
    });

    let path = tokio::time::timeout(
        Duration::from_secs(60),
        walk.service.execute_acquisition(
            "soulseek",
            started.external_id.as_deref().expect("an id"),
            false,
            true,
            Some(DownloadSource::Soulseek),
            &CancellationToken::new(),
            None,
            false,
            None,
        ),
    )
    .await
    .expect("within a minute")
    .expect("the FLAC is fetched");

    assert_eq!(
        walk.slskd.browses(),
        vec![("bothpeer".to_string(), NWTS_FOLDER.to_string())]
    );
    let batches = walk.slskd.batches();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].1, vec![flac_remote]);
    assert!(path.ends_with(".flac"), "{path}");
    assert!(std::path::Path::new(&path).is_file());
}

/// A resumed hold brings back the file it held: the download checks it first and takes it, and
/// no source is asked. A held file that is gone leaves the sources to run as ever.
#[tokio::test]
async fn a_resumed_holds_file_is_checked_first_and_taken_without_a_search() {
    let root = Root::new("octo-held-");
    let walk = build(&root, false, |_| Arc::new(|_: &str| Vec::new())).await;
    let song = walk.songs[1].clone();
    let held = root.write(".octo-incoming/held/Track_soulseek_hold-on/hold-on.flac", 0);
    std::fs::write(&held, flac(200)).expect("the held file");
    let path = tokio::time::timeout(
        Duration::from_secs(60),
        walk.service.execute_acquisition_to(
            "soulseek",
            song.external_id.as_deref().expect("an id"),
            false,
            true,
            Some(DownloadSource::Soulseek),
            &CancellationToken::new(),
            None,
            false,
            None,
            crate::services::common::AcquisitionPlace {
                held_files: vec![held.clone()],
                ..Default::default()
            },
        ),
    )
    .await
    .expect("within a minute")
    .expect("the held file lands");
    assert!(walk.slskd.searches().is_empty(), "{:?}", walk.slskd.searches());
    assert!(std::path::Path::new(&path).is_file());
    assert!(!std::path::Path::new(&held).exists(), "it moved into the library");

    // Gone from the hold path: the search runs as ever.
    let other = walk.songs[3].clone();
    let token = CancellationToken::new();
    let searched = walk.service.execute_acquisition_to(
        "soulseek",
        other.external_id.as_deref().expect("an id"),
        false,
        true,
        Some(DownloadSource::Soulseek),
        &token,
        None,
        false,
        None,
        crate::services::common::AcquisitionPlace {
            held_files: vec![root.0.path().join("gone.flac").to_string_lossy().into_owned()],
            ..Default::default()
        },
    );
    let _ = tokio::time::timeout(Duration::from_secs(5), searched).await;
    token.cancel();
    assert!(!walk.slskd.searches().is_empty());
}

#[tokio::test]
async fn no_flac_beside_the_mp3_means_no_download() {
    let root = Root::new("octo-albumwalk-");
    let walk = build(&root, false, |_| {
        Arc::new(|text: &str| {
            if text.contains("Started") {
                vec![lossy_response(
                    "mp3peer",
                    &format!("{NWTS_FOLDER}\\04 - Started.mp3"),
                    180,
                )]
            } else {
                Vec::new()
            }
        })
    })
    .await;
    walk.slskd.set_folders(|_, _| {
        Some(json!({
            "name": NWTS_FOLDER,
            "files": [{"filename": "04 - Started.mp3", "size": 8_000_000i64, "extension": "mp3", "length": 180}],
        }))
    });

    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        walk.service.execute_acquisition(
            "soulseek",
            walk.songs[3].external_id.as_deref().expect("an id"),
            false,
            true,
            Some(DownloadSource::Soulseek),
            &CancellationToken::new(),
            None,
            false,
            None,
        ),
    )
    .await
    .expect("within a minute");

    let error = outcome.expect_err("nothing to download");
    // "No copy here" is what lets a library action try its next source.
    assert!(
        error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "{error:#}"
    );
    assert_eq!(walk.slskd.browses().len(), 1);
    assert!(walk.slskd.batches().is_empty());
}

// ---- Rejected files, invalid requests and source fallbacks -------------------------------

#[test]
fn a_rejected_file_is_deleted_only_when_this_attempt_made_it() {
    let root = Root::new("octo-discard-");
    let path = root.write("song.flac", 10);
    // Started long after the file was made: not ours.
    discard_rejected_download(&path, Utc::now() + chrono::TimeDelta::hours(1));
    assert!(std::path::Path::new(&path).is_file());
    discard_rejected_download(&path, Utc::now());
    assert!(!std::path::Path::new(&path).exists());
}

#[test]
fn a_missing_artist_or_title_is_an_invalid_operation() {
    let error: anyhow::Error = InvalidOperationException("Cannot download".into()).into();
    assert!(error.downcast_ref::<InvalidOperationException>().is_some());
    assert_eq!(error.to_string(), "Cannot download");
}

async fn acquire(walk: &Walk, external_id: &str, source: Option<DownloadSource>) -> anyhow::Result<String> {
    tokio::time::timeout(
        Duration::from_secs(60),
        walk.service.execute_acquisition(
            "soulseek",
            external_id,
            false,
            true,
            source,
            &CancellationToken::new(),
            None,
            false,
            None,
        ),
    )
    .await
    .expect("within a minute")
}

/// The search for "Started" answers two peers; the first one's file is the wrong length.
fn two_peers(text: &str) -> Vec<Value> {
    if text.contains("Started") {
        vec![
            response("wrongpeer", &[(r"A\Drake\04 - Started.flac".to_string(), 180)]),
            response("rightpeer", &[(r"B\Drake\04 - Started.flac".to_string(), 180)]),
        ]
    } else {
        Vec::new()
    }
}

#[tokio::test]
async fn a_peer_that_delivers_the_wrong_length_is_passed_over_for_the_next() {
    let root = Root::new("octo-attempts-");
    let walk = build(&root, false, |_| Arc::new(two_peers)).await;
    // Advertised as 180 s, and 300 s when it arrives.
    walk.slskd.length(r"A\Drake\04 - Started.flac", 300);
    walk.slskd.length(r"B\Drake\04 - Started.flac", 180);
    for file in [r"A\Drake\04 - Started.flac", r"B\Drake\04 - Started.flac"] {
        walk.slskd.state.lock().with_audio.insert(file.to_string());
    }

    let path = acquire(&walk, walk.songs[3].external_id.as_deref().expect("an id"), None)
        .await
        .expect("the second peer's file is kept");

    let users: Vec<String> = walk.slskd.batches().into_iter().map(|(u, ..)| u).collect();
    assert_eq!(users, names(&["wrongpeer", "rightpeer"]));
    assert!(std::path::Path::new(&path).is_file(), "{path}");
    // The wrong file was this attempt's own, so it is gone; only the kept one is placed.
    assert_eq!(placed_flacs(&root), 1);
    assert_eq!(
        files_under(&root.path())
            .expect("lists")
            .iter()
            .filter(|f| f.ends_with(".flac"))
            .count(),
        1
    );
}

// ---- The preview check ------------------------------------------------------------------

/// 180 s of audio ffmpeg makes from a lavfi source, encoded as `format`; None where ffmpeg or
/// fpcalc is missing.
fn made_sound(source: &str, format: &[&str]) -> Option<Vec<u8>> {
    let on_path = |program: &str| {
        std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                dir.join(format!("{program}{}", std::env::consts::EXE_SUFFIX))
                    .is_file()
            })
        })
    };
    if !on_path("ffmpeg") || !on_path("fpcalc") {
        return None;
    }
    let dir = tempfile::tempdir().expect("a temp dir");
    let out = dir.path().join("sound");
    let made = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", source])
        .args(format)
        .arg(&out)
        .status()
        .is_ok_and(|status| status.success());
    made.then(|| std::fs::read(&out).expect("made"))
}

/// The song "Started", a peer whose file is noise, and a peer whose file is the song; AcoustID
/// has never heard either, and YouTube Music's row for the song is Drake's own.
fn song_and_noise(walk: &Walk) -> Option<Vec<u8>> {
    let melody = "aevalsrc=exprs=0.4*sin(2*PI*t*220*pow(2\\,floor(mod(t*2\\,8))*2/12))\
                  +0.3*sin(2*PI*t*330*pow(2\\,floor(mod(t*3\\,5))*3/12)):s=44100:d=180";
    let song = made_sound(melody, &["-c:a", "flac", "-f", "flac"])?;
    let noise = made_sound(
        "anoisesrc=color=pink:duration=180:amplitude=0.3:seed=7",
        &["-c:a", "flac", "-f", "flac"],
    )?;
    let preview = made_sound(melody, &["-c:a", "aac", "-b:a", "128k", "-f", "mp4"])?;
    let mut state = walk.slskd.state.lock();
    state.sound.insert(r"A\Drake\04 - Started.flac".into(), noise);
    state.sound.insert(r"B\Drake\04 - Started.flac".into(), song);
    state.ytm_tracks = Some(json!([{
        "videoId": "ytm-atv", "title": "Started", "artists": ["Drake"],
        "durationSeconds": 180, "videoType": "MUSIC_VIDEO_TYPE_ATV",
    }]));
    Some(preview)
}

/// The two peers of [`song_and_noise`], each listing its file at its real size, the noise's
/// peer the faster so it is tried first.
fn noise_then_song(sizes: HashMap<String, usize>) -> Answers {
    Arc::new(move |text: &str| {
        if !text.contains("Started") {
            return Vec::new();
        }
        [
            ("wrongpeer", r"A\Drake\04 - Started.flac", 2_000_000),
            ("rightpeer", r"B\Drake\04 - Started.flac", 1_000_000),
        ]
        .iter()
        .map(|(user, file, speed)| {
                json!({
                    "username": user, "uploadSpeed": speed, "queueLength": 0, "hasFreeUploadSlot": true,
                    "files": [{"filename": file, "size": sizes[*file] as i64, "length": 180, "extension": "flac"}],
                })
            })
            .collect()
    })
}

async fn verified_walk(root: &Root) -> Option<(Walk, Vec<u8>)> {
    let sizes: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
    let listed = Arc::clone(&sizes);
    let walk = build_full(root, false, DownloadSource::Soulseek, true, true, move |_| {
        Arc::new(move |text: &str| noise_then_song(listed.lock().clone())(text))
    })
    .await;
    let preview = song_and_noise(&walk)?;
    let state = walk.slskd.state.lock();
    *sizes.lock() = state
        .sound
        .iter()
        .map(|(file, bytes)| (file.clone(), bytes.len()))
        .collect();
    drop(state);
    Some((walk, preview))
}

/// #101: a file AcoustID never heard that sounds nothing like the song's own YouTube preview is
/// refused like any failed check, its peer remembered, and the next peer's file is kept.
#[tokio::test]
async fn a_file_that_does_not_sound_like_the_preview_is_passed_over_for_the_next() {
    let root = Root::new("octo-preview-");
    let Some((walk, preview)) = verified_walk(&root).await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    walk.slskd.state.lock().preview = Some(preview);

    let path = acquire(&walk, walk.songs[3].external_id.as_deref().expect("an id"), None)
        .await
        .expect("the second peer's file is kept");

    let users: Vec<String> = walk.slskd.batches().into_iter().map(|(u, ..)| u).collect();
    assert_eq!(users, names(&["wrongpeer", "rightpeer"]));
    assert!(std::path::Path::new(&path).is_file(), "{path}");
    assert_eq!(placed_flacs(&root), 1);
    assert!(
        walk.rejected
            .is_denied(Some("wrongpeer"), Some(r"A\Drake\04 - Started.flac"))
    );
    assert!(
        !walk
            .rejected
            .is_denied(Some("rightpeer"), Some(r"B\Drake\04 - Started.flac"))
    );
}

/// With the shim unable to send the preview, the file is judged as it was before: kept.
#[tokio::test]
async fn without_a_preview_the_file_acoustid_never_heard_is_kept() {
    let root = Root::new("octo-preview-");
    let Some((walk, _)) = verified_walk(&root).await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };

    acquire(&walk, walk.songs[3].external_id.as_deref().expect("an id"), None)
        .await
        .expect("the first peer's file is kept");

    let users: Vec<String> = walk.slskd.batches().into_iter().map(|(u, ..)| u).collect();
    assert_eq!(users, names(&["wrongpeer"]));
    assert_eq!(placed_flacs(&root), 1);
    assert_eq!(walk.rejected.count(), 0);
}

/// What AcoustID answers for every Soulseek file: "Drake - Started", through one submission.
fn lone_lookup() -> Value {
    json!({"status": "ok", "results": [{
        "id": "result", "score": 0.97,
        "recordings": [{
            "id": "rec-started", "title": "Started", "artists": [{"name": "Drake"}],
            "duration": 180, "sources": 1,
        }],
    }]})
}

#[test]
fn a_lone_link_to_the_song_still_says_which_take_a_file_is() {
    let live = VerificationResult {
        reason: InconclusiveReason::LoneLink,
        recording_id: Some("rec".into()),
        kind: Some(RecordingKind::Live),
        ..Default::default()
    };
    assert_eq!(take_of(&live), Some(RecordingKind::Live));
    let confirmed = VerificationResult {
        verdict: VerificationVerdict::Confirmed,
        reason: InconclusiveReason::None,
        ..live.clone()
    };
    assert_eq!(take_of(&confirmed), Some(RecordingKind::Live));
    let elsewhere = VerificationResult {
        recording_id: None,
        ..live.clone()
    };
    assert_eq!(take_of(&elsewhere), None);
    let unsure = VerificationResult {
        reason: InconclusiveReason::BelowThreshold,
        ..live
    };
    assert_eq!(take_of(&unsure), None);
}

/// Review switched on, with alice keeping the library.
fn review_on(walk: &Walk) {
    let mut settings = (*walk.settings.current()).clone();
    settings.library_actions.enabled = true;
    settings.library_actions.review_enabled = true;
    settings.library_actions.allowed_users = vec!["alice".into()];
    walk.settings.set(settings);
}

/// One person's link to the song is not enough alone: the preview a believed row plays refuses
/// the noise (its peer remembered) and confirms the song, which is then asked about nowhere.
#[tokio::test]
async fn a_lone_link_is_confirmed_by_a_preview_that_agrees_and_refused_by_one_that_does_not() {
    let root = Root::new("octo-preview-");
    let Some((walk, preview)) = verified_walk(&root).await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    {
        let mut state = walk.slskd.state.lock();
        state.preview = Some(preview);
        state.lookup_by_video.insert(String::new(), lone_lookup());
    }
    review_on(&walk);

    let path = acquire(&walk, walk.songs[3].external_id.as_deref().expect("an id"), None)
        .await
        .expect("the second peer's file is kept");

    let users: Vec<String> = walk.slskd.batches().into_iter().map(|(u, ..)| u).collect();
    assert_eq!(users, names(&["wrongpeer", "rightpeer"]));
    assert!(std::path::Path::new(&path).is_file(), "{path}");
    assert!(
        walk.rejected
            .is_denied(Some("wrongpeer"), Some(r"A\Drake\04 - Started.flac"))
    );
    assert!(
        !walk
            .rejected
            .is_denied(Some("rightpeer"), Some(r"B\Drake\04 - Started.flac"))
    );
    // Confirmed, so nobody is asked about it.
    assert!(walk.reviews.0.lock().is_empty());
}

/// With no preview to back it, a lone link is kept and asked about, carrying the recording a
/// person's Keep sends AcoustID a second link to.
#[tokio::test]
async fn a_lone_link_without_a_preview_is_kept_and_asked_about() {
    let root = Root::new("octo-preview-");
    let Some((walk, _)) = verified_walk(&root).await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    walk.slskd
        .state
        .lock()
        .lookup_by_video
        .insert(String::new(), lone_lookup());
    review_on(&walk);

    acquire(&walk, walk.songs[3].external_id.as_deref().expect("an id"), None)
        .await
        .expect("the first peer's file is kept");

    assert_eq!(walk.rejected.count(), 0);
    let asked = walk.reviews.0.lock().clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0].reason, InconclusiveReason::LoneLink);
    assert_eq!(asked[0].candidate_recording_id.as_deref(), Some("rec-started"));
}

/// A Lidarr fetcher that hands over whatever audio it was given.
#[derive(Default)]
struct FakeFetcher {
    audio: Mutex<Vec<u8>>,
    fetched: Mutex<Vec<String>>,
}

#[async_trait]
impl ILidarrTrackFetcher for FakeFetcher {
    async fn fetch(
        &self,
        _request: &LidarrTrackRequest,
        destination_directory: &str,
        _ct: &CancellationToken,
    ) -> Result<crate::services::lidarr::LidarrFetched, octo_core::lidarr::LidarrError> {
        std::fs::create_dir_all(destination_directory).expect("made");
        let path = combine(destination_directory, "04 - Started.flac");
        std::fs::write(&path, self.audio.lock().clone()).expect("written");
        self.fetched.lock().push(path.clone());
        Ok(crate::services::lidarr::LidarrFetched {
            path,
            labels: crate::services::fingerprint::download_verification_service::LabelSource::None,
        })
    }
}

/// A walk whose Lidarr fetcher hands over the sound `song_and_noise` made for the peer `wanted`
/// ("A" the noise, "B" the song), with the song's preview playing; None where ffmpeg or fpcalc
/// is missing.
async fn lidarr_walk(root: &Root, wanted: &str) -> Option<(Walk, Arc<FakeFetcher>)> {
    let fetcher = Arc::new(FakeFetcher::default());
    let walk = build_all(
        root,
        false,
        DownloadSource::Soulseek,
        true,
        true,
        None,
        Some(fetcher.clone()),
        |_| Arc::new(|_: &str| Vec::new()),
    )
    .await;
    let preview = song_and_noise(&walk)?;
    let mut state = walk.slskd.state.lock();
    *fetcher.audio.lock() = state.sound[&format!(r"{wanted}\Drake\04 - Started.flac")].clone();
    state.preview = Some(preview);
    drop(state);
    Some((walk, fetcher))
}

/// Lidarr's fetched file gets the checks an imported one does: AcoustID hearing it as another
/// song removes it and refuses the download.
#[tokio::test]
async fn a_lidarr_fetched_file_that_acoust_id_hears_as_another_song_is_removed_and_refused() {
    let root = Root::new("octo-lidarr-");
    let Some((walk, fetcher)) = lidarr_walk(&root, "B").await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    walk.slskd.state.lock().lookup_by_video.insert(
        String::new(),
        json!({"status": "ok", "results": [{
            "id": "result", "score": 0.97,
            "recordings": [{
                "id": "rec-other", "title": "Another Song", "artists": [{"name": "Someone Else"}],
                "duration": 180, "sources": 9,
            }],
        }]}),
    );

    let refused = acquire(
        &walk,
        walk.songs[3].external_id.as_deref().expect("an id"),
        Some(DownloadSource::Lidarr),
    )
    .await
    .expect_err("another song is refused");

    assert!(refused.to_string().contains("AcoustID identified"), "{refused}");
    let fetched = fetcher.fetched.lock().clone();
    assert_eq!(fetched.len(), 1);
    assert!(!std::path::Path::new(&fetched[0]).exists(), "{}", fetched[0]);
    assert_eq!(placed_flacs(&root), 0);
}

/// A fetched file AcoustID has one person's word for, that sounds nothing like the song's
/// YouTube preview, is removed and refused.
#[tokio::test]
async fn a_lidarr_fetched_file_the_preview_calls_another_song_is_removed_and_refused() {
    let root = Root::new("octo-lidarr-");
    let Some((walk, fetcher)) = lidarr_walk(&root, "A").await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    walk.slskd
        .state
        .lock()
        .lookup_by_video
        .insert(String::new(), lone_lookup());
    review_on(&walk);

    let refused = acquire(
        &walk,
        walk.songs[3].external_id.as_deref().expect("an id"),
        Some(DownloadSource::Lidarr),
    )
    .await
    .expect_err("the noise is refused");

    assert!(refused.to_string().contains("Lidarr's file for"), "{refused}");
    let fetched = fetcher.fetched.lock().clone();
    assert_eq!(fetched.len(), 1);
    assert!(!std::path::Path::new(&fetched[0]).exists(), "{}", fetched[0]);
    assert_eq!(placed_flacs(&root), 0);
}

/// Runs the song's acquisition through Lidarr's fetcher for `place`'s origin, gates and deny.
async fn through_lidarr(
    walk: &Walk,
    place: crate::services::common::AcquisitionPlace,
) -> anyhow::Result<String> {
    tokio::time::timeout(
        Duration::from_secs(60),
        walk.service.execute_acquisition_to(
            "soulseek",
            walk.songs[3].external_id.as_deref().expect("an id"),
            false,
            true,
            Some(DownloadSource::Lidarr),
            &CancellationToken::new(),
            None,
            false,
            None,
            place,
        ),
    )
    .await
    .expect("within a minute")
}

/// A Find songs pick Lidarr fetched gets the pick's own Busy line, as a Soulseek one does: with
/// AcoustID not answering, its file is thrown away and the pick ends "AcoustID is busy, try again".
#[tokio::test]
async fn a_find_songs_pick_lidarr_fetched_that_meets_a_busy_acoustid_ends_busy() {
    let root = Root::new("octo-lidarr-pick-");
    let Some((walk, fetcher)) = lidarr_walk(&root, "B").await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    walk.slskd.state.lock().lookup_by_video.insert(
        String::new(),
        json!({"status": "error", "error": {"code": 14, "message": "service unavailable"}}),
    );

    let busy = through_lidarr(
        &walk,
        crate::services::common::AcquisitionPlace {
            origin: Some(AcquisitionOrigin::SongFinderPick),
            ..Default::default()
        },
    )
    .await
    .expect_err("not kept");

    let held = busy
        .downcast_ref::<crate::services::common::AcquisitionHeld>()
        .expect("ended at once");
    assert_eq!(held.line, crate::services::common::song_finder::ACOUST_ID_BUSY);
    assert!(held.files.is_empty(), "nothing waits anywhere");
    let fetched = fetcher.fetched.lock().clone();
    assert!(!std::path::Path::new(&fetched[0]).exists(), "{}", fetched[0]);
    assert_eq!(placed_flacs(&root), 0);
}

/// A Find songs pick Lidarr fetched takes the pick's import rule: a file AcoustID's reading names
/// as another recording is refused, though the shipped check would confirm it by its title.
#[tokio::test]
async fn a_find_songs_pick_lidarr_fetched_another_recording_by_its_reading_is_refused() {
    let root = Root::new("octo-lidarr-pick-");
    let Some((walk, _)) = lidarr_walk(&root, "B").await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    walk.slskd
        .state
        .lock()
        .lookup_by_video
        .insert(String::new(), answer_naming(&[("rec-remix", 12, "USCM51400099")]));

    let refused = through_lidarr(
        &walk,
        crate::services::common::AcquisitionPlace {
            origin: Some(AcquisitionOrigin::SongFinderPick),
            gates: vec![HeartGate {
                origin: AcquisitionOrigin::SongFinderPick,
                ..import_gate(&["rec-started"])
            }],
            ..Default::default()
        },
    )
    .await
    .expect_err("refused");

    assert!(refused.to_string().contains("another recording"), "{refused}");
    assert_eq!(placed_flacs(&root), 0);
}

/// A Wrong song or Wrong version replacement Lidarr fetched is refused when AcoustID hears the
/// original's recording in it again; a copy of another recording lands.
#[tokio::test]
async fn a_replacement_lidarr_fetched_refuses_its_originals_recording_again() {
    let root = Root::new("octo-lidarr-deny-");
    let Some((walk, _)) = lidarr_walk(&root, "B").await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    let replacing = || crate::services::common::AcquisitionPlace {
        origin: Some(AcquisitionOrigin::Replacement),
        deny: Some(crate::services::common::SoundDeny::Recordings(HeardRecording {
            ids: vec!["rec-wrong".into()],
            ..Default::default()
        })),
        ..Default::default()
    };
    walk.slskd
        .state
        .lock()
        .lookup_by_video
        .insert(String::new(), answer_naming(&[("rec-wrong", 20, "USCM51400077")]));
    let refused = through_lidarr(&walk, replacing()).await;
    assert!(refused.is_err(), "the original's recording again: {refused:?}");

    walk.slskd.state.lock().lookup_by_video.insert(
        String::new(),
        answer_naming(&[("rec-started", 20, "USCM51400001")]),
    );
    let landed = through_lidarr(&walk, replacing()).await.expect("lands");
    assert!(std::path::Path::new(&landed).is_file(), "{landed}");
}

#[tokio::test]
async fn without_batches_the_file_is_found_by_name_the_old_way() {
    let root = Root::new("octo-attempts-");
    let walk = build(&root, false, |songs| {
        Arc::new(move |text: &str| {
            if text.contains("Started") {
                vec![response(
                    "oldpeer",
                    &[(remote(r"Music\Drake\NWTS", &songs[3]), duration_of(&songs[3]))],
                )]
            } else {
                Vec::new()
            }
        })
    })
    .await;
    walk.slskd.state.lock().no_batches = true;
    walk.slskd.length(
        &remote(r"Music\Drake\NWTS", &walk.songs[3]),
        duration_of(&walk.songs[3]),
    );

    let path = acquire(&walk, walk.songs[3].external_id.as_deref().expect("an id"), None)
        .await
        .expect("found by its name");

    assert_eq!(
        walk.slskd.state.lock().enqueued,
        vec![remote(r"Music\Drake\NWTS", &walk.songs[3])]
    );
    assert!(walk.slskd.batches().is_empty());
    assert!(
        path.ends_with(".flac") && std::path::Path::new(&path).is_file(),
        "{path}"
    );
}

#[tokio::test]
async fn soulseek_then_you_tube_falls_back_to_the_mp3_when_soulseek_has_nothing() {
    let root = Root::new("octo-attempts-");
    let walk = build_with(&root, false, DownloadSource::SoulseekThenYouTube, |_| {
        Arc::new(|_: &str| Vec::new())
    })
    .await;

    let path = acquire(&walk, walk.songs[3].external_id.as_deref().expect("an id"), None)
        .await
        .expect("the YouTube copy lands");

    assert_eq!(walk.slskd.state.lock().you_tube_downloads.len(), 1);
    assert!(
        path.ends_with(".mp3") && std::path::Path::new(&path).is_file(),
        "{path}"
    );
}

#[tokio::test]
async fn a_song_without_a_routing_is_an_invalid_operation() {
    let root = Root::new("octo-attempts-");
    let walk = build(&root, false, |_| Arc::new(|_: &str| Vec::new())).await;

    let error = acquire(&walk, "unknown-id", Some(DownloadSource::Soulseek))
        .await
        .expect_err("nothing to search for");

    assert!(
        error.downcast_ref::<InvalidOperationException>().is_some(),
        "{error:#}"
    );
    assert_eq!(
        error.to_string(),
        "Cannot download 'Nobody - Mystery': missing artist/title in external id"
    );
}

// What the row says while a peer has not started.
#[test]
fn a_transfer_that_has_not_started_names_whom_it_waits_on() {
    assert_eq!(
        waiting_words("tokidoki", "Queued, Remotely"),
        "Waiting in tokidoki's queue"
    );
    assert_eq!(
        waiting_words("tokidoki", "Initializing"),
        "Waiting for tokidoki to start sending"
    );
    assert_eq!(
        waiting_words("tokidoki", "Requested"),
        "Waiting for tokidoki to start sending"
    );
}

// ---- The stream an outside song plays -------------------------------------------------------

#[test]
fn the_played_size_is_the_whole_body_or_the_total_after_the_slash() {
    assert_eq!(played_size(200, Some(6_755_585), None), Some(6_755_585));
    assert_eq!(played_size(206, Some(2), Some("bytes 0-1/12345")), Some(12_345));
    assert_eq!(played_size(416, None, Some("bytes */12345")), Some(12_345));
    assert_eq!(played_size(206, Some(2), Some("bytes 0-1/*")), None);
    assert_eq!(played_size(206, Some(2), None), None);
    assert_eq!(played_size(502, Some(10), None), None);
}

#[tokio::test]
async fn a_play_pins_the_found_video_and_learns_its_true_size_from_the_stream() {
    let root = Root::new("octo-stream-");
    let walk = build(&root, false, |_| Arc::new(|_: &str| Vec::new())).await;
    let id = walk.songs[0].external_id.clone().expect("an id");

    let stream = walk
        .service
        .get_direct_stream("soulseek", &id, Some("bytes=0-1"), &CancellationToken::new())
        .await
        .expect("no error")
        .expect("a stream");
    assert_eq!(stream.status_code, 206);

    let routing = walk.registry.lookup(&id).expect("known").snapshot();
    assert_eq!(routing.you_tube_id.as_deref(), Some("vid123"));
    // The search said 12000; the bytes on their way say 12345, and they win.
    let facts = walk.registry.stream_facts(&id).expect("facts");
    assert_eq!(facts.size, Some(12_345));
    assert_eq!(facts.itag.as_deref(), Some("140"));
    assert_eq!(facts.bit_rate, Some(130));
}

#[tokio::test]
async fn a_play_takes_youtube_musics_video_for_the_song_before_the_yt_dlp_search() {
    let root = Root::new("octo-stream-");
    let walk = build(&root, false, |_| Arc::new(|_: &str| Vec::new())).await;
    let song = walk.songs[0].clone();
    let id = song.external_id.clone().expect("an id");
    let length = walk.registry.lookup(&id).expect("known").snapshot().duration;
    walk.slskd.state.lock().ytm_tracks = Some(json!([{
        "videoId": "ytm-atv", "title": song.title, "artists": [song.artist],
        "durationSeconds": length.unwrap_or(200), "videoType": "MUSIC_VIDEO_TYPE_ATV",
    }]));

    walk.service
        .get_direct_stream("soulseek", &id, Some("bytes=0-1"), &CancellationToken::new())
        .await
        .expect("no error")
        .expect("a stream");

    let routing = walk.registry.lookup(&id).expect("known").snapshot();
    assert_eq!(routing.you_tube_id.as_deref(), Some("ytm-atv"));
    assert!(routing.shown_duration.is_some());
    let asked: Vec<String> = walk
        ._server
        .received_requests()
        .await
        .expect("recording")
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    assert!(asked.iter().any(|p| p == "/ytm/search"), "{asked:?}");
    assert!(!asked.iter().any(|p| p == "/search"), "{asked:?}");
    // The stream opened is the picked video.
    assert!(
        walk._server
            .received_requests()
            .await
            .expect("recording")
            .iter()
            .any(|r| r.url.path() == "/stream" && r.url.query().unwrap_or("").contains("id=ytm-atv"))
    );
}

// ---- A video the current ranking prefers is taken only by a play from the first byte --------

const OLD_VIDEO: &str = "oldvideo-01";
const NEW_VIDEO: &str = "newvideo-02";

/// A preview source that always has the same 1000 bytes.
struct SteadyPreview;

#[async_trait::async_trait]
impl crate::services::prefetch::PreviewSource for SteadyPreview {
    async fn open_whole(&self, _video_id: &str) -> Option<crate::services::prefetch::WholePreview> {
        let body: Vec<std::io::Result<bytes::Bytes>> = vec![Ok(bytes::Bytes::from(vec![7u8; 1000]))];
        Some(crate::services::prefetch::WholePreview {
            content_type: "audio/mp4".to_string(),
            total: 1000,
            body: Box::pin(futures::stream::iter(body)),
        })
    }
}

/// A walk keeping preview copies, whose first song plays `OLD_VIDEO` while `NEW_VIDEO` waits,
/// with a copy of the old video made.
async fn waiting_walk(root: &Root) -> (Walk, String, Arc<PreviewCopies>) {
    let copies = PreviewCopies::new(
        root.0.path().join("previews"),
        Arc::new(SteadyPreview),
        octo_core::common::Clock::system(),
        || 0,
    );
    let walk = build_full_copies(
        root,
        false,
        DownloadSource::Soulseek,
        true,
        false,
        Some(copies.clone()),
        |_| Arc::new(|_: &str| Vec::new()),
    )
    .await;
    let id = walk.songs[0].external_id.clone().expect("an id");
    walk.registry.set_you_tube_id(&id, OLD_VIDEO);
    assert!(walk.registry.set_pending_video(&id, OLD_VIDEO, NEW_VIDEO));
    copies.start(OLD_VIDEO, None);
    for _ in 0..500 {
        if copies.is_ready(OLD_VIDEO) {
            return (walk, id, copies);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the copy of the old video was never made");
}

/// The videos the shim was asked to stream, in order.
async fn streamed(walk: &Walk) -> Vec<String> {
    walk._server
        .received_requests()
        .await
        .expect("recording")
        .iter()
        .filter(|r| r.url.path() == "/stream")
        .filter_map(|r| {
            r.url
                .query_pairs()
                .find(|(k, _)| k == "id")
                .map(|(_, v)| v.into_owned())
        })
        .collect()
}

#[tokio::test]
async fn a_play_picking_up_past_the_first_byte_streams_the_old_video_and_leaves_the_new_one_waiting() {
    let root = Root::new("octo-stream-");
    let (walk, id, copies) = waiting_walk(&root).await;
    // Past the first byte the old copy answers on its own; take it away so the shim is asked.
    copies.drop_copy(OLD_VIDEO);

    walk.service
        .get_direct_stream("soulseek", &id, Some("bytes=1000-"), &CancellationToken::new())
        .await
        .expect("no error")
        .expect("a stream");

    let routing = walk.registry.lookup(&id).expect("known").snapshot();
    assert_eq!(routing.you_tube_id.as_deref(), Some(OLD_VIDEO));
    assert_eq!(routing.pending_video.as_deref(), Some(NEW_VIDEO));
    assert_eq!(streamed(&walk).await, [OLD_VIDEO]);
}

#[tokio::test]
async fn a_play_from_the_first_byte_streams_the_new_video_and_drops_the_old_copy() {
    let root = Root::new("octo-stream-");
    let (walk, id, copies) = waiting_walk(&root).await;

    walk.service
        .get_direct_stream("soulseek", &id, None, &CancellationToken::new())
        .await
        .expect("no error")
        .expect("a stream");

    let routing = walk.registry.lookup(&id).expect("known").snapshot();
    assert_eq!(routing.you_tube_id.as_deref(), Some(NEW_VIDEO));
    assert_eq!(routing.video_pick, SoulseekRouting::VIDEO_PICK);
    assert_eq!(routing.pending_video, None);
    assert_eq!(streamed(&walk).await, [NEW_VIDEO]);
    assert!(!copies.is_ready(OLD_VIDEO), "the old video's copy went");
}

#[tokio::test]
async fn the_old_copy_stays_while_another_song_still_pins_the_old_video() {
    let root = Root::new("octo-stream-");
    let (walk, id, copies) = waiting_walk(&root).await;
    let other = walk.songs[1].external_id.clone().expect("an id");
    walk.registry.set_you_tube_id(&other, OLD_VIDEO);

    walk.service
        .get_direct_stream("soulseek", &id, Some("bytes=0-"), &CancellationToken::new())
        .await
        .expect("no error")
        .expect("a stream");

    let routing = walk.registry.lookup(&id).expect("known").snapshot();
    assert_eq!(routing.you_tube_id.as_deref(), Some(NEW_VIDEO));
    assert!(copies.is_ready(OLD_VIDEO), "another song still plays it");
}

#[tokio::test]
async fn a_play_falls_back_to_the_yt_dlp_search_when_youtube_music_has_no_such_song() {
    let root = Root::new("octo-stream-");
    let walk = build(&root, false, |_| Arc::new(|_: &str| Vec::new())).await;
    let id = walk.songs[0].external_id.clone().expect("an id");
    walk.slskd.state.lock().ytm_tracks = Some(json!([{
        "videoId": "other", "title": "Another Song", "artists": ["Someone Else"],
        "durationSeconds": 200, "videoType": "MUSIC_VIDEO_TYPE_ATV",
    }]));

    walk.service
        .get_direct_stream("soulseek", &id, Some("bytes=0-1"), &CancellationToken::new())
        .await
        .expect("no error")
        .expect("a stream");

    let routing = walk.registry.lookup(&id).expect("known").snapshot();
    assert_eq!(routing.you_tube_id.as_deref(), Some("vid123"));
}

#[tokio::test]
async fn with_soulseek_off_soulseek_then_you_tube_goes_straight_to_the_mp3() {
    let root = Root::new("octo-attempts-");
    let walk = build_with_soulseek(&root, false, DownloadSource::SoulseekThenYouTube, false, |_| {
        Arc::new(|_: &str| Vec::new())
    })
    .await;

    let path = acquire(&walk, walk.songs[3].external_id.as_deref().expect("an id"), None)
        .await
        .expect("the YouTube copy lands");

    assert!(path.ends_with(".mp3"), "{path}");
    assert_eq!(walk.slskd.state.lock().you_tube_downloads.len(), 1);
    let asked: Vec<String> = walk
        ._server
        .received_requests()
        .await
        .expect("recording")
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    assert!(!asked.iter().any(|p| p.starts_with("/api/")), "{asked:?}");
}

#[tokio::test]
async fn with_soulseek_off_a_soulseek_download_says_so_without_asking_slskd() {
    let root = Root::new("octo-attempts-");
    let walk = build_with_soulseek(&root, false, DownloadSource::Soulseek, false, |_| {
        Arc::new(|_: &str| Vec::new())
    })
    .await;

    let error = acquire(&walk, walk.songs[3].external_id.as_deref().expect("an id"), None)
        .await
        .expect_err("no source to try");

    assert!(error.to_string().starts_with("Soulseek is off"), "{error:#}");
    assert!(walk.slskd.searches().is_empty());
    let asked = walk._server.received_requests().await.expect("recording");
    assert!(!asked.iter().any(|r| r.url.path().starts_with("/api/")));
}

// ---- What a YouTube download does when AcoustID names another song --------------------------

/// A verdict that heard `artist - title` as recording `rec`, which AcoustID lists at
/// `recording_seconds` against a file of 153 s.
fn heard_at(
    verdict: VerificationVerdict,
    artist: &str,
    title: &str,
    recording_seconds: Option<i32>,
) -> VerificationResult {
    let recording = AcoustIdRecording {
        duration_seconds: recording_seconds,
        sources: 10,
        ..AcoustIdRecording::new("rec", title, [artist], None, None)
    };
    VerificationResult {
        verdict,
        score: 0.97,
        matched_artist: Some(artist.into()),
        matched_title: Some(title.into()),
        recording_id: Some("rec".into()),
        duration_seconds: 153,
        lookup: Some(AcoustIdLookup::new(
            true,
            None,
            vec![AcoustIdResult::new(0.97, vec![recording])],
        )),
        ..Default::default()
    }
}

fn heard(verdict: VerificationVerdict, artist: &str, title: &str) -> VerificationResult {
    heard_at(verdict, artist, title, Some(153))
}

/// Whether `verdict` names another recording than `KILLY - Killamonjaro`, when the catalog says
/// the song runs `catalog` seconds and any take is wanted.
fn another_recording(verdict: &VerificationResult, catalog: i32) -> bool {
    names_another_recording(verdict, "KILLY", "Killamonjaro", Some(catalog), &Takes::Any)
}

#[test]
fn another_artists_song_of_another_title_is_another_recording() {
    // A video of another artist's song of a like title.
    let summer_cem = heard(
        VerificationVerdict::Mismatch,
        "Summer Cem & BILLA JOE",
        "KILLY MANJARO",
    );
    assert!(another_recording(&summer_cem, 153));
}

#[test]
fn the_artists_other_song_is_another_recording_only_at_another_length() {
    let other_song = heard(VerificationVerdict::Mismatch, "KILLY", "Distance");
    assert!(another_recording(&other_song, 179));
    assert!(!another_recording(&other_song, 153));
}

#[test]
fn a_cover_by_another_artist_is_another_recording_only_at_another_length() {
    let cover = heard(VerificationVerdict::Mismatch, "Some Band", "Killamonjaro");
    assert!(another_recording(&cover, 173));
    // At the song's own length it may be an alias of the artist: asked about, not thrown away.
    assert!(!another_recording(&cover, 153));
}

#[test]
fn a_title_variant_at_the_songs_length_is_kept_and_asked_about() {
    let variant = heard(VerificationVerdict::Mismatch, "KILLY", "Killamonjaro Part 2");
    assert!(!names_another_recording(
        &variant,
        "KILLY",
        "Killamonjaro Pt. 2",
        Some(153),
        &Takes::Any
    ));
}

#[test]
fn a_certain_take_nobody_asked_for_is_another_recording() {
    let mut live = heard(VerificationVerdict::Mismatch, "KILLY", "Killamonjaro");
    live.kind = Some(RecordingKind::Live);
    live.kind_certain = true;
    assert!(another_recording(&live, 153));
    // The same take, asked for by the title, is the song.
    assert!(!names_another_recording(
        &live,
        "KILLY",
        "Killamonjaro (Live)",
        Some(153),
        &Takes::Any
    ));
    // A take the verdict is not sure of is a question.
    live.kind_certain = false;
    assert!(!another_recording(&live, 153));
}

#[test]
fn another_version_of_the_title_is_another_recording() {
    let remix = heard(VerificationVerdict::Mismatch, "KILLY", "Killamonjaro (Remix)");
    assert!(another_recording(&remix, 153));
}

#[test]
fn a_verdict_that_names_nothing_or_confirms_is_not() {
    let nothing_named = VerificationResult {
        verdict: VerificationVerdict::Mismatch,
        ..Default::default()
    };
    let confirmed = heard(VerificationVerdict::Confirmed, "Summer Cem", "KILLY MANJARO");
    for verdict in [nothing_named, confirmed] {
        assert!(!another_recording(&verdict, 153), "{verdict:?}");
    }
}

#[test]
fn a_name_in_another_script_is_kept_and_asked_about() {
    let japanese = heard(VerificationVerdict::Mismatch, "高橋洋子", "残酷な天使のテーゼ");
    assert!(!names_another_recording(
        &japanese,
        "Yoko Takahashi",
        "A Cruel Angel's Thesis",
        Some(153),
        &Takes::Any
    ));
    let russian = heard(VerificationVerdict::Mismatch, "Кино", "Звезда по имени Солнце");
    assert!(!names_another_recording(
        &russian,
        "Viktor Tsoi",
        "Star Called Sun",
        Some(153),
        &Takes::Any
    ));
}

#[test]
fn an_unbelieved_link_is_kept_and_asked_about() {
    let too_long = heard_at(
        VerificationVerdict::Mismatch,
        "Summer Cem & BILLA JOE",
        "KILLY MANJARO",
        Some(210),
    );
    let no_length = heard_at(
        VerificationVerdict::Mismatch,
        "Summer Cem & BILLA JOE",
        "KILLY MANJARO",
        None,
    );
    let mut dwarfed = heard(
        VerificationVerdict::Mismatch,
        "Summer Cem & BILLA JOE",
        "KILLY MANJARO",
    );
    let other = AcoustIdRecording {
        duration_seconds: Some(153),
        sources: 250,
        ..AcoustIdRecording::new("other", "Something Else", ["Other"], None, None)
    };
    dwarfed.lookup.as_mut().expect("a lookup").results[0]
        .recordings
        .push(other);
    let mut no_lookup = heard(
        VerificationVerdict::Mismatch,
        "Summer Cem & BILLA JOE",
        "KILLY MANJARO",
    );
    no_lookup.lookup = None;
    for verdict in [too_long, no_length, dwarfed, no_lookup] {
        assert!(!another_recording(&verdict, 153), "{verdict:?}");
    }
}

#[tokio::test]
async fn a_refused_video_is_not_taken_again() {
    let root = Root::new("octo-stream-");
    let walk = build(&root, false, |_| Arc::new(|_: &str| Vec::new())).await;
    let song = walk.songs[0].clone();
    let id = song.external_id.clone().expect("an id");
    let length = walk.registry.lookup(&id).expect("known").snapshot().duration;
    // YouTube Music would hand back the refused video.
    walk.slskd.state.lock().ytm_tracks = Some(json!([{
        "videoId": "ytm-atv", "title": song.title, "artists": [song.artist],
        "durationSeconds": length.unwrap_or(200), "videoType": "MUSIC_VIDEO_TYPE_ATV",
    }]));
    walk.registry.refuse_video(&id, "ytm-atv");

    walk.service
        .get_direct_stream("soulseek", &id, Some("bytes=0-1"), &CancellationToken::new())
        .await
        .expect("no error")
        .expect("a stream");

    let routing = walk.registry.lookup(&id).expect("known").snapshot();
    assert_eq!(routing.you_tube_id.as_deref(), Some("vid123"));
    let asked = walk._server.received_requests().await.expect("recording");
    assert!(
        asked
            .iter()
            .any(|r| r.url.path() == "/search" && r.url.query().unwrap_or("").contains("skip=ytm-atv")),
        "{asked:?}"
    );
}

// ---- A YouTube file that is another recording: the next video is tried ----------------------

/// What AcoustID answers for a 180 s file it hears as `artist - title`.
fn heard_lookup(artist: &str, title: &str) -> Value {
    json!({"status": "ok", "results": [{
        "id": "result", "score": 0.97,
        "recordings": [{
            "id": "rec", "title": title, "artists": [{"name": artist}],
            "duration": 180, "sources": 10,
        }],
    }]})
}

/// A walk whose YouTube downloads are real 180 s audio; None where ffmpeg or fpcalc is missing.
async fn recording_walk(root: &Root, videos: &[&str]) -> Option<Walk> {
    let melody = "aevalsrc=exprs=0.4*sin(2*PI*t*220*pow(2\\,floor(mod(t*2\\,8))*2/12))\
                  +0.3*sin(2*PI*t*330*pow(2\\,floor(mod(t*3\\,5))*3/12)):s=44100:d=180";
    let audio = made_sound(melody, &["-c:a", "libmp3lame", "-f", "mp3"])?;
    let walk = build_full(root, false, DownloadSource::YouTube, true, true, |_| {
        Arc::new(|_: &str| Vec::new())
    })
    .await;
    {
        let mut state = walk.slskd.state.lock();
        state.download_audio = Some(audio);
        state.search_videos = videos.iter().map(|v| v.to_string()).collect();
    }
    Some(walk)
}

fn searches_of(requests: &[Request]) -> Vec<String> {
    requests
        .iter()
        .filter(|r| r.url.path() == "/search")
        .map(|r| r.url.query().unwrap_or("").to_string())
        .collect()
}

#[tokio::test]
async fn a_video_that_is_another_recording_is_refused_and_the_next_one_is_kept() {
    let root = Root::new("octo-attempts-");
    let Some(walk) = recording_walk(&root, &["bad", "good"]).await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    {
        let mut state = walk.slskd.state.lock();
        state
            .lookup_by_video
            .insert("bad".into(), heard_lookup("Other Artist", "Other Song"));
        state
            .lookup_by_video
            .insert("good".into(), heard_lookup("Drake", "Started"));
    }
    let id = walk.songs[3].external_id.clone().expect("an id");

    acquire(&walk, &id, None).await.expect("the second video lands");

    let routing = walk.registry.lookup(&id).expect("known").snapshot();
    assert_eq!(routing.you_tube_id.as_deref(), Some("good"));
    assert_eq!(routing.refused_videos, ["bad"]);
    assert_eq!(walk.registry.refusals_for(&routing), ["bad"]);
    assert_eq!(walk.slskd.state.lock().you_tube_downloads.len(), 2);
    let asked = walk._server.received_requests().await.expect("recording");
    let searches = searches_of(&asked);
    assert!(searches.len() >= 2, "{searches:?}");
    assert!(!searches[0].contains("skip="), "{searches:?}");
    assert!(searches[1].contains("skip=bad"), "{searches:?}");
}

/// A YouTube Music row that names the song, artist and length is a second word beside one
/// person's AcoustID link: the MP3 is confirmed and nobody is asked about it.
#[tokio::test]
async fn a_lone_link_through_a_row_that_names_the_song_is_confirmed() {
    let root = Root::new("octo-attempts-");
    let Some(walk) = recording_walk(&root, &["vid"]).await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    {
        let mut state = walk.slskd.state.lock();
        state.ytm_tracks = Some(json!([{
            "videoId": "ytm-row", "title": "Started", "artists": ["Drake"],
            "durationSeconds": 180, "videoType": "MUSIC_VIDEO_TYPE_ATV",
        }]));
        state.lookup_by_video.insert("ytm-row".into(), lone_lookup());
    }
    review_on(&walk);
    let id = walk.songs[3].external_id.clone().expect("an id");

    acquire(&walk, &id, None).await.expect("the row's MP3 lands");

    let routing = walk.registry.lookup(&id).expect("known").snapshot();
    assert_eq!(routing.you_tube_id.as_deref(), Some("ytm-row"));
    assert!(walk.reviews.0.lock().is_empty());
}

/// A video whose own facts do not name the song backs nothing: the lone link stays a question.
#[tokio::test]
async fn a_lone_link_through_a_video_nothing_names_is_asked_about() {
    let root = Root::new("octo-attempts-");
    let Some(walk) = recording_walk(&root, &["vid"]).await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    walk.slskd
        .state
        .lock()
        .lookup_by_video
        .insert("vid".into(), lone_lookup());
    review_on(&walk);
    let id = walk.songs[3].external_id.clone().expect("an id");

    acquire(&walk, &id, None).await.expect("the video's MP3 lands");

    let asked = walk.reviews.0.lock().clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0].reason, InconclusiveReason::LoneLink);
}

#[tokio::test]
async fn three_videos_that_are_other_recordings_fail_the_download() {
    let root = Root::new("octo-attempts-");
    let Some(walk) = recording_walk(&root, &["a", "b", "c", "d"]).await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    for video in ["a", "b", "c", "d"] {
        walk.slskd
            .state
            .lock()
            .lookup_by_video
            .insert(video.into(), heard_lookup("Other Artist", "Other Song"));
    }
    let id = walk.songs[3].external_id.clone().expect("an id");

    let error = acquire(&walk, &id, None).await.expect_err("no video is right");

    assert!(error.to_string().contains("AcoustID identified"), "{error:#}");
    let routing = walk.registry.lookup(&id).expect("known").snapshot();
    assert_eq!(routing.refused_videos, ["a", "b", "c"]);
    assert_eq!(walk.slskd.state.lock().you_tube_downloads.len(), 3);
}

#[tokio::test]
async fn a_pinned_video_that_is_refused_is_never_downloaded_twice() {
    let root = Root::new("octo-attempts-");
    let Some(walk) = recording_walk(&root, &["bad", "good"]).await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    {
        let mut state = walk.slskd.state.lock();
        state
            .lookup_by_video
            .insert("bad".into(), heard_lookup("Other Artist", "Other Song"));
        state
            .lookup_by_video
            .insert("good".into(), heard_lookup("Drake", "Started"));
    }
    let id = walk.songs[3].external_id.clone().expect("an id");
    assert!(walk.registry.set_you_tube_id(&id, "bad"));

    acquire(&walk, &id, None).await.expect("the other video lands");

    let downloads = walk.slskd.state.lock().you_tube_downloads.clone();
    assert_eq!(downloads.len(), 2, "{downloads:?}");
    assert!(downloads[0].contains("bad-"), "{downloads:?}");
    assert!(downloads[1].contains("good-"), "{downloads:?}");
}

#[tokio::test]
async fn a_refused_video_is_not_downloaded_again() {
    let root = Root::new("octo-attempts-");
    let walk = build_with(&root, false, DownloadSource::YouTube, |_| {
        Arc::new(|_: &str| Vec::new())
    })
    .await;
    let song = walk.songs[0].clone();
    let id = song.external_id.clone().expect("an id");
    let length = walk.registry.lookup(&id).expect("known").snapshot().duration;
    // YouTube Music would hand back the refused video.
    walk.slskd.state.lock().ytm_tracks = Some(json!([{
        "videoId": "ytm-atv", "title": song.title, "artists": [song.artist],
        "durationSeconds": length.unwrap_or(200), "videoType": "MUSIC_VIDEO_TYPE_ATV",
    }]));
    walk.registry.refuse_video(&id, "ytm-atv");

    acquire(&walk, &id, None).await.expect("the YouTube copy lands");

    let routing = walk.registry.lookup(&id).expect("known").snapshot();
    assert_eq!(routing.you_tube_id.as_deref(), Some("vid123"));
    let asked = walk._server.received_requests().await.expect("recording");
    assert!(
        asked
            .iter()
            .any(|r| r.url.path() == "/search" && r.url.query().unwrap_or("").contains("skip=ytm-atv")),
        "{asked:?}"
    );
}

// ---- The heart gate at the sources ------------------------------------------------------------

/// A video the shim could not save is no reason to give up: the next video is tried, for every
/// origin, and its copy is kept.
#[tokio::test]
async fn a_video_whose_download_fails_gives_way_to_the_next() {
    let root = Root::new("octo-youtube-");
    let walk = build_with(&root, false, DownloadSource::YouTube, |_| {
        Arc::new(|_: &str| Vec::new())
    })
    .await;
    {
        let mut state = walk.slskd.state.lock();
        state.search_videos = vec!["broken".into(), "working".into()];
        state.failing_downloads.insert("broken".into());
    }
    let path = acquire(&walk, walk.songs[3].external_id.as_deref().expect("an id"), None)
        .await
        .expect("the second video's copy lands");
    assert!(std::path::Path::new(&path).is_file(), "{path}");
    let state = walk.slskd.state.lock();
    assert_eq!(state.last_download, "working");
    assert_eq!(state.you_tube_downloads.len(), 1);
}

/// A file held in the one hold path for a day and more survives Mornye's folder sweep and the
/// incoming sweep, and a Soulseek download's job-folder sweep: only its hold's end removes it.
#[tokio::test]
async fn a_held_file_a_day_old_survives_every_sweep() {
    let root = Root::new("octo-held-");
    let walk = build(&root, false, |_| Arc::new(|_: &str| Vec::new())).await;
    let held = root.write(".octo-incoming/held/Track_soulseek_started/started.flac", 0);
    std::fs::write(&held, flac(180)).expect("the held file");
    let old = std::time::SystemTime::now() - Duration::from_secs(25 * 3600);
    std::fs::File::options()
        .write(true)
        .open(&held)
        .and_then(|file| file.set_modified(old))
        .expect("aged");
    spotiflac_download::sweep_spotiflac_folders(&root.path());
    sweep_incoming(&combine(&root.path(), INCOMING_FOLDER_NAME));
    // A Soulseek download sweeps its job folders first; nothing is found to fetch after it.
    let _ = acquire(&walk, walk.songs[3].external_id.as_deref().expect("an id"), None).await;
    assert!(std::path::Path::new(&held).is_file(), "{held}");
}

/// The heart gate for "Started": its catalog recording, as a heart that heard nothing has it.
fn started_gate() -> HeartGate {
    HeartGate {
        user: "brandon".into(),
        origin: AcquisitionOrigin::Heart,
        r: Some(HeardRecording {
            ids: vec!["rec-started".into()],
            duplicates: vec!["rec-started".into()],
            title: "Started".into(),
            artist: "Drake".into(),
            ..Default::default()
        }),
        heard: None,
        heard_seconds: Some(180),
        reading: HeardReading::NotHeard,
        stand_in: None,
    }
}

async fn acquire_for_heart(walk: &Walk, gate: HeartGate) -> anyhow::Result<String> {
    tokio::time::timeout(
        Duration::from_secs(60),
        walk.service.execute_acquisition_to(
            "soulseek",
            walk.songs[3].external_id.as_deref().expect("an id"),
            false,
            true,
            Some(DownloadSource::Soulseek),
            &CancellationToken::new(),
            None,
            false,
            None,
            crate::services::common::AcquisitionPlace {
                origin: Some(AcquisitionOrigin::Heart),
                gates: vec![gate],
                ..Default::default()
            },
        ),
    )
    .await
    .expect("within a minute")
}

/// A heart's gate refuses files AcoustID has no entry for: each refusal is the heart's own, kept
/// for its song and R and never on the shared list, so an import of the same song still takes the
/// peer's file; and the heart's file is never judged by the song's YouTube preview.
#[tokio::test]
async fn a_hearts_refusal_never_denies_the_peer_for_an_import() {
    let root = Root::new("octo-gate-");
    let Some((walk, preview)) = verified_walk(&root).await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    walk.slskd.state.lock().preview = Some(preview);
    let song = walk.songs[3].external_id.clone().expect("an id");
    let gate = started_gate();

    let refused = acquire_for_heart(&walk, gate.clone()).await;
    assert!(refused.is_err(), "neither file is proven: {refused:?}");
    let asked = walk._server.received_requests().await.expect("recording");
    assert!(
        !asked.iter().any(|r| r.url.path() == "/stream"),
        "a heart's file is never judged by the preview"
    );
    for (peer, file) in [
        ("wrongpeer", r"A\Drake\04 - Started.flac"),
        ("rightpeer", r"B\Drake\04 - Started.flac"),
    ] {
        assert!(!walk.rejected.is_denied(Some(peer), Some(file)), "{peer}");
        assert!(
            walk.rejected
                .is_denied_for_heart(&song, gate.r.as_ref().expect("R"), Some(peer), Some(file)),
            "{peer}"
        );
    }

    // An import of the song runs the shipped checks, and the right peer's file still lands.
    let path = acquire(&walk, &song, None)
        .await
        .expect("the import's file lands");
    assert!(std::path::Path::new(&path).is_file(), "{path}");
}

/// A heart's file whose only agreeing link is one person's submission is not accepted on that,
/// even with the song's YouTube preview sounding like it: its reading is ambiguous and nothing
/// heard can prove it. The same file for an import is kept as the shipped checks keep it.
#[tokio::test]
async fn a_lone_submission_is_not_accepted_for_a_heart_and_lands_for_an_import() {
    let root = Root::new("octo-gate-");
    let Some((walk, preview)) = verified_walk(&root).await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    {
        let mut state = walk.slskd.state.lock();
        state.preview = Some(preview);
        state.last_download = "lone".into();
        state.lookup_by_video.insert("lone".into(), lone_lookup());
    }

    let refused = acquire_for_heart(&walk, started_gate()).await;
    assert!(
        refused.is_err(),
        "a lone link proves nothing for a heart: {refused:?}"
    );

    let landed = acquire(&walk, walk.songs[3].external_id.as_deref().expect("an id"), None)
        .await
        .expect("the import keeps it");
    assert!(std::path::Path::new(&landed).is_file(), "{landed}");
}

/// What AcoustID answers for every Soulseek file: "Drake - Started" as `recordings`, each id with
/// its submissions and its own ISRC.
fn answer_naming(recordings: &[(&str, i64, &str)]) -> Value {
    json!({"status": "ok", "results": [{
        "id": "result", "score": 0.97,
        "recordings": recordings.iter().map(|(id, sources, isrc)| json!({
            "id": id, "title": "Started", "artists": [{"name": "Drake"}],
            "duration": 180, "sources": sources, "isrcs": [isrc],
        })).collect::<Vec<Value>>(),
    }]})
}

/// An import of "Started" whose R is known: its catalog recording and ISRC.
fn import_gate(ids: &[&str]) -> HeartGate {
    HeartGate {
        user: "brandon".into(),
        origin: AcquisitionOrigin::Import,
        r: Some(HeardRecording {
            ids: ids.iter().map(|id| id.to_string()).collect(),
            duplicates: ids.iter().map(|id| id.to_string()).collect(),
            isrcs: vec!["USCM51400001".into()],
            title: "Started".into(),
            artist: "Drake".into(),
            ..Default::default()
        }),
        heard: None,
        heard_seconds: None,
        reading: HeardReading::NotHeard,
        stand_in: None,
    }
}

async fn import_with(walk: &Walk, gate: HeartGate) -> anyhow::Result<String> {
    tokio::time::timeout(
        Duration::from_secs(60),
        walk.service.execute_acquisition_to(
            "soulseek",
            walk.songs[3].external_id.as_deref().expect("an id"),
            false,
            true,
            Some(DownloadSource::Soulseek),
            &CancellationToken::new(),
            None,
            false,
            None,
            crate::services::common::AcquisitionPlace {
                origin: Some(AcquisitionOrigin::Import),
                gates: vec![gate],
                ..Default::default()
            },
        ),
    )
    .await
    .expect("within a minute")
}

/// An import with an ISRC refuses a file AcoustID's reading names as another recording, though the
/// shipped check confirms it by title, and tries the next; with no ISRC (no R known) the shipped
/// check alone decides, as for a CSV import, and the file lands.
#[tokio::test]
async fn an_import_with_an_isrc_refuses_another_recording_and_one_without_still_lands() {
    let root = Root::new("octo-import-");
    let Some((walk, _)) = verified_walk(&root).await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    {
        let mut state = walk.slskd.state.lock();
        state.last_download = "x".into();
        state
            .lookup_by_video
            .insert("x".into(), answer_naming(&[("rec-remix", 12, "USCM51400099")]));
    }
    let refused = import_with(&walk, import_gate(&["rec-started"])).await;
    assert!(
        refused.is_err(),
        "both peers' files are another recording: {refused:?}"
    );
    let users: Vec<String> = walk.slskd.batches().into_iter().map(|(u, ..)| u).collect();
    assert_eq!(users, names(&["wrongpeer", "rightpeer"]), "the next was tried");
    assert!(
        !walk
            .rejected
            .is_denied(Some("rightpeer"), Some(r"B\Drake\04 - Started.flac")),
        "an import's refusal is its own"
    );

    let landed = import_with(&walk, import_gate(&[])).await.expect("lands");
    assert!(std::path::Path::new(&landed).is_file(), "{landed}");
}

/// `place`'s acquisition of "Started", within a minute.
async fn started_with(
    walk: &Walk,
    place: crate::services::common::AcquisitionPlace,
) -> anyhow::Result<String> {
    tokio::time::timeout(
        Duration::from_secs(60),
        walk.service.execute_acquisition_to(
            "soulseek",
            walk.songs[3].external_id.as_deref().expect("an id"),
            false,
            true,
            Some(DownloadSource::Soulseek),
            &CancellationToken::new(),
            None,
            false,
            None,
            place,
        ),
    )
    .await
    .expect("within a minute")
}

/// A Find songs pick takes the import rule: a file AcoustID's reading names as another recording is
/// refused, though the shipped check confirms it by title, with that reason.
#[tokio::test]
async fn a_find_songs_pick_another_recording_by_its_reading_is_refused_with_the_reason() {
    let root = Root::new("octo-pick-");
    let Some((walk, _)) = verified_walk(&root).await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    {
        let mut state = walk.slskd.state.lock();
        state.last_download = "x".into();
        state
            .lookup_by_video
            .insert("x".into(), answer_naming(&[("rec-remix", 12, "USCM51400099")]));
    }
    let refused = started_with(
        &walk,
        crate::services::common::AcquisitionPlace {
            origin: Some(AcquisitionOrigin::SongFinderPick),
            gates: vec![HeartGate {
                origin: AcquisitionOrigin::SongFinderPick,
                ..import_gate(&["rec-started"])
            }],
            ..Default::default()
        },
    )
    .await
    .expect_err("refused");
    assert!(refused.to_string().contains("another recording"), "{refused}");
    assert_eq!(placed_flacs(&root), 0);
}

/// A Find songs pick is never held: a file whose lookup AcoustID does not answer is thrown away at
/// once, and the pick ends with "AcoustID is busy, try again", trying no other peer.
#[tokio::test]
async fn a_find_songs_picks_file_that_meets_a_busy_acoustid_is_thrown_away_and_ends_busy() {
    let root = Root::new("octo-pick-");
    let Some((walk, _)) = verified_walk(&root).await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    {
        let mut state = walk.slskd.state.lock();
        state.last_download = "x".into();
        state.lookup_by_video.insert(
            "x".into(),
            json!({"status": "error", "error": {"code": 14, "message": "service unavailable"}}),
        );
    }
    let busy = started_with(
        &walk,
        crate::services::common::AcquisitionPlace {
            origin: Some(AcquisitionOrigin::SongFinderPick),
            ..Default::default()
        },
    )
    .await
    .expect_err("not kept");
    let held = busy
        .downcast_ref::<crate::services::common::AcquisitionHeld>()
        .expect("ended at once");
    assert_eq!(held.line, crate::services::common::song_finder::ACOUST_ID_BUSY);
    assert!(held.files.is_empty(), "nothing waits anywhere");
    let users: Vec<String> = walk.slskd.batches().into_iter().map(|(u, ..)| u).collect();
    assert_eq!(users, names(&["wrongpeer"]), "no other peer is tried");
    let left: Vec<String> = files_under(&root.path())
        .expect("the root lists")
        .into_iter()
        .filter(|f| f.ends_with(".flac"))
        .collect();
    assert!(left.is_empty(), "no file left: {left:?}");
}

/// A Wrong song or Wrong version replacement refuses a file AcoustID names as its original's
/// recording again, and tries the next peer; a file of another recording lands.
#[tokio::test]
async fn a_replacement_refuses_its_originals_recording_again_and_tries_the_next_peer() {
    let root = Root::new("octo-deny-");
    let Some((walk, _)) = verified_walk(&root).await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    let replacing = || crate::services::common::AcquisitionPlace {
        origin: Some(AcquisitionOrigin::Replacement),
        deny: Some(crate::services::common::SoundDeny::Recordings(HeardRecording {
            ids: vec!["rec-wrong".into()],
            ..Default::default()
        })),
        ..Default::default()
    };
    {
        let mut state = walk.slskd.state.lock();
        state.last_download = "x".into();
        state
            .lookup_by_video
            .insert("x".into(), answer_naming(&[("rec-wrong", 20, "USCM51400077")]));
    }
    let refused = started_with(&walk, replacing()).await;
    assert!(refused.is_err(), "the original's recording again: {refused:?}");
    let users: Vec<String> = walk.slskd.batches().into_iter().map(|(u, ..)| u).collect();
    assert_eq!(users, names(&["wrongpeer", "rightpeer"]), "the next was tried");

    walk.slskd
        .state
        .lock()
        .lookup_by_video
        .insert("x".into(), answer_naming(&[("rec-started", 20, "USCM51400001")]));
    let landed = started_with(&walk, replacing()).await.expect("lands");
    assert!(std::path::Path::new(&landed).is_file(), "{landed}");
}

/// A clustered import the shipped check confirms lands as it does today, marked "version not
/// proven": no Review entry, and no recording id written.
#[tokio::test]
async fn a_clustered_import_lands_as_version_not_proven_with_no_review_entry() {
    let root = Root::new("octo-import-");
    let Some((walk, _)) = verified_walk(&root).await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    {
        let mut state = walk.slskd.state.lock();
        state.last_download = "x".into();
        state.lookup_by_video.insert(
            "x".into(),
            answer_naming(&[
                ("rec-started", 20, "USCM51400001"),
                ("rec-started-remaster", 18, "USCM51400002"),
            ]),
        );
    }
    let landed = import_with(&walk, import_gate(&["rec-started"]))
        .await
        .expect("the confirmed file lands");
    assert!(std::path::Path::new(&landed).is_file(), "{landed}");
    assert!(walk.reviews.0.lock().is_empty(), "nothing goes to Review");
    let (recording, _) = octo_media::tags::tag_writer_extras::read_identity(std::path::Path::new(&landed));
    assert_eq!(recording, None, "no tag claims a version the sound did not prove");
}

/// An import's file is held to the song's YouTube preview in the shim's background lane, so its
/// preview check never runs ahead of a heart's or a play's fetch.
#[tokio::test]
async fn an_imports_preview_check_goes_in_the_shims_background_lane() {
    let root = Root::new("octo-import-");
    let Some((walk, preview)) = verified_walk(&root).await else {
        eprintln!("skipped: ffmpeg or fpcalc is not on the PATH");
        return;
    };
    walk.slskd.state.lock().preview = Some(preview);
    let landed = import_with(&walk, import_gate(&[])).await.expect("lands");
    assert!(std::path::Path::new(&landed).is_file(), "{landed}");
    let asked: Vec<bool> = walk
        ._server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == "/stream")
        .map(|r| r.url.query().unwrap_or("").contains("bg=1"))
        .collect();
    assert!(!asked.is_empty() && asked.iter().all(|bg| *bg), "{asked:?}");
}

/// The remuxed stream keeps the very audio that played: its audio packets are the copy's, byte for
/// byte. Fails, rather than skips, without ffmpeg.
#[tokio::test]
async fn the_remuxed_heard_stream_is_byte_identical_to_the_copy() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let copy = dir.path().join("vid.mp4");
    let made = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=3",
        ])
        .args(["-c:a", "aac", "-b:a", "128k", "-f", "mp4"])
        .arg(&copy)
        .status()
        .expect("ffmpeg runs: the gate needs it");
    assert!(made.success());
    let folder = dir.path().join("out").to_string_lossy().into_owned();
    let remuxed = soulseek_gate::remux_audio(&copy, "audio/mp4", &folder, "vid")
        .await
        .expect("remuxed");
    assert!(remuxed.ends_with(".m4a"), "{remuxed}");
    let packets = |file: &std::path::Path| {
        let out = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(file)
            .args(["-map", "0:a:0", "-c", "copy", "-f", "data", "-"])
            .output()
            .expect("ffmpeg runs");
        assert!(out.status.success());
        out.stdout
    };
    let original = packets(&copy);
    assert!(!original.is_empty());
    assert_eq!(packets(std::path::Path::new(&remuxed)), original);
}

#[path = "soulseek_gate_tests.rs"]
mod gate;
