//! octo-sonic: reads one song and answers with bliss's numbers for how it sounds, and with
//! its transition profile (where it can blend into the next song). Octo keeps the numbers
//! and finds songs that sound alike, and hands the profiles to the apps; this only reads
//! files, and only under MUSIC_ROOT. Each file is read by a child process (`octo-sonic read
//! <path> [parts] [bpm]`): FFmpeg can crash on a damaged file, and a crash there must cost that
//! file, not the server.

mod automix;
mod transition;

use std::{path::{Component, Path, PathBuf}, sync::Arc, time::Duration};

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::{get, post}, Json, Router};
use bliss_audio::decoder::Decoder as _;
use bliss_audio::decoder::ffmpeg::FFmpegDecoder;
use bliss_audio::FeaturesVersion;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Semaphore;

/// Longest a single file may take; a 45-minute FLAC over a network mount is the worst case.
const READ_LIMIT: Duration = Duration::from_secs(600);

#[derive(Clone)]
struct App {
    root: PathBuf,
    slots: Arc<Semaphore>,
    program: PathBuf,
    /// False when Sounds alike is turned off (SONIC_ENABLED, from RADIO_SOUNDS_ALIKE): songs
    /// are still read for their transition profiles, never for how they sound.
    sounds_alike: bool,
}

#[derive(Deserialize)]
struct Analyse {
    path: String,
    /// What to read: "sound" (bliss's numbers) and "transition" (the profile). Only the
    /// sound when left out, as an Octo that knows nothing of profiles asks.
    #[serde(default)]
    parts: Option<Vec<String>>,
    /// The song's published tempo (a catalog's BPM or its BPM tag), when it has one. It only
    /// picks the octave of the tempo the profile finds; see `transition`.
    #[serde(default)]
    bpm: Option<f64>,
}

/// A published tempo worth handing on: a real number of beats per minute. 0 is how catalogs
/// and tags say they have none.
fn published_bpm(bpm: Option<f64>) -> Option<f64> {
    bpm.filter(|bpm| bpm.is_finite() && *bpm > 0.0 && *bpm < 1_000.0)
}

/// What a reading covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Parts {
    sound: bool,
    transition: bool,
}

impl Parts {
    /// From the names asked for; nothing known is the sound alone.
    fn of<'a>(names: impl IntoIterator<Item = &'a str>) -> Parts {
        let mut parts = Parts { sound: false, transition: false };
        for name in names {
            match name.trim() {
                "sound" => parts.sound = true,
                "transition" => parts.transition = true,
                _ => {}
            }
        }
        if !parts.sound && !parts.transition {
            parts.sound = true;
        }
        parts
    }

    /// What is left when how the song sounds is not read: the profile alone, if it was asked.
    fn without_sound(self) -> Parts {
        Parts { sound: false, ..self }
    }

    fn is_empty(self) -> bool {
        !self.sound && !self.transition
    }

    /// As the child takes them on its command line.
    fn arg(self) -> String {
        let mut names = Vec::new();
        if self.sound {
            names.push("sound");
        }
        if self.transition {
            names.push("transition");
        }
        names.join(",")
    }
}

fn version() -> u16 {
    u16::from(FeaturesVersion::LATEST)
}

/// SONIC_ENABLED as compose passes RADIO_SOUNDS_ALIKE: on unless it says false, 0, off or no.
fn sounds_alike(value: Option<&str>) -> bool {
    !matches!(value.map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("false" | "0" | "off" | "no"))
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if (3..=5).contains(&args.len()) && args[1] == "read" {
        let parts = Parts::of(args.get(3).map_or("sound", String::as_str).split(','));
        let bpm = published_bpm(args.get(4).and_then(|bpm| bpm.parse().ok()));
        std::process::exit(read_one(Path::new(&args[2]), parts, bpm));
    }
    tracing_subscriber::fmt::init();
    let root = std::env::var("MUSIC_ROOT").unwrap_or_else(|_| "/music".into());
    let root = std::fs::canonicalize(&root).unwrap_or_else(|_| PathBuf::from(root));
    let slots = std::env::var("ANALYSE_CONCURRENCY").ok().and_then(|v| v.parse().ok()).unwrap_or(1usize).max(1);
    let program = std::env::current_exe().expect("own path");
    let sounds_alike = sounds_alike(std::env::var("SONIC_ENABLED").ok().as_deref());
    if !sounds_alike {
        tracing::info!("Sounds alike is off (RADIO_SOUNDS_ALIKE=false): making transition profiles only");
    }
    let app = router(App { root, slots: Arc::new(Semaphore::new(slots)), program, sounds_alike });
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.expect("port 8080");
    axum::serve(listener, app)
        .with_graceful_shutdown(stopped())
        .await
        .expect("server");
}

/// Ctrl-C, or the SIGTERM `docker stop` sends: without it the container waits out the
/// stop timeout and is killed, a file half read.
async fn stopped() {
    let interrupt = async { let _ = tokio::signal::ctrl_c().await; };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => tokio::select! { _ = interrupt => {}, _ = term.recv() => {} },
            Err(_) => interrupt.await,
        }
    }
    #[cfg(not(unix))]
    interrupt.await;
}

/// The child: print what was asked for as JSON on stdout, or the error on stderr. Exit 0
/// or 2. The song is decoded once (mono, 22050 Hz) for both parts.
fn read_one(path: &Path, parts: Parts, bpm: Option<f64>) -> i32 {
    match read_parts(path, parts, bpm) {
        Ok(answer) => {
            println!("{answer}");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            2
        }
    }
}

fn read_parts(path: &Path, parts: Parts, bpm: Option<f64>) -> Result<serde_json::Value, bliss_audio::BlissError> {
    let decoded = FFmpegDecoder::decode(path)?;
    let mut answer = serde_json::Map::new();
    if parts.transition {
        let profile = transition::analyze(&decoded.sample_array, DECODED_RATE, bpm);
        answer.insert("transition".into(), transition::to_json(&profile));
    }
    if parts.sound {
        let song = bliss_audio::Song::try_from(decoded)?;
        answer.insert("features".into(), json!(song.analysis.as_vec()));
        answer.insert("version".into(), json!(version()));
    }
    Ok(serde_json::Value::Object(answer))
}

/// The rate bliss's decoder hands the sound over at, mono.
const DECODED_RATE: u32 = 22_050;

fn router(app: App) -> Router {
    Router::new().route("/health", get(health)).route("/analyse", post(analyse)).with_state(app)
}

/// Healthy only when the music folder is there and has something in it: a mount that failed
/// leaves an empty folder, and every song would then look missing. `soundsAlike` says whether
/// it reads how songs sound; transition profiles are made either way. `transitionRevision` is
/// the pass of the profile analysis: Octo reads songs with a lower one again.
async fn health(State(app): State<App>) -> impl IntoResponse {
    let root = app.root.clone();
    let sees = tokio::task::spawn_blocking(move || {
        std::fs::read_dir(root).map(|mut entries| entries.next().is_some()).unwrap_or(false)
    })
    .await
    .unwrap_or(false);
    if sees {
        (StatusCode::OK, Json(json!({
            "ok": true, "featuresVersion": version(), "transitionVersion": transition::version(),
            "transitionRevision": transition::revision(), "soundsAlike": app.sounds_alike,
        })))
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, Json(json!({
            "ok": false, "featuresVersion": version(), "transitionVersion": transition::version(),
            "transitionRevision": transition::revision(), "soundsAlike": app.sounds_alike,
            "error": "cannot see the music folder",
        })))
    }
}

async fn analyse(State(app): State<App>, Json(body): Json<Analyse>) -> impl IntoResponse {
    // With Sounds alike off, how a song sounds is never read: only its profile, if asked.
    let parts = Parts::of(body.parts.iter().flatten().map(String::as_str));
    let parts = if app.sounds_alike { parts } else { parts.without_sound() };
    if parts.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "Sounds alike is off", "off": true })));
    }
    let path = match inside(&app.root, Path::new(&body.path)) {
        Ok(path) => path,
        Err(Refused::Missing) => return (StatusCode::NOT_FOUND, Json(json!({ "error": "no such file" }))),
        Err(Refused::Outside) => return (StatusCode::FORBIDDEN, Json(json!({ "error": "outside the music folder" }))),
    };
    if !path.is_file() {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "not a file" })));
    }
    let _slot = app.slots.acquire().await.expect("semaphore");
    let mut command = tokio::process::Command::new(&app.program);
    command.arg("read").arg(&path).arg(parts.arg());
    if let Some(bpm) = published_bpm(body.bpm).filter(|_| parts.transition) {
        command.arg(bpm.to_string());
    }
    let child = command
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    };
    match tokio::time::timeout(READ_LIMIT, child.wait_with_output()).await {
        Err(_) => (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "error": "took longer than 10 minutes" }))),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
        Ok(Ok(out)) if out.status.success() => match serde_json::from_slice::<serde_json::Value>(&out.stdout) {
            Ok(value) => (StatusCode::OK, Json(value)),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
        },
        Ok(Ok(out)) => {
            let reason = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let reason = if out.status.code().is_none() { "crashed reading the file".to_string() } else { reason };
            (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "error": reason })))
        }
    }
}

enum Refused {
    /// Under the music folder by its name, but not there.
    Missing,
    /// Outside it, by "..", a symlink or another folder.
    Outside,
}

/// The file, resolved, when it really is under the music folder (no "..", no symlink out). A
/// path that names a place under it but is not there is missing, not refused, so Octo can tell
/// a song it should look for again from a request it should never have made.
fn inside(root: &Path, path: &Path) -> Result<PathBuf, Refused> {
    match std::fs::canonicalize(path) {
        Ok(full) if full.starts_with(root) => Ok(full),
        Ok(_) => Err(Refused::Outside),
        Err(_) if path.is_absolute() && path.starts_with(root)
            && !path.components().any(|part| matches!(part, Component::ParentDir)) => Err(Refused::Missing),
        Err(_) => Err(Refused::Outside),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// The test binary is not octo-sonic, so the child reader is exercised by the Docker smoke
    /// test (Step 8.4); these cover the server's own checks.
    fn app(root: &Path) -> Router {
        app_with(root, true)
    }

    fn app_with(root: &Path, sounds_alike: bool) -> Router {
        app_running(root, sounds_alike, PathBuf::from("/nonexistent/octo-sonic"))
    }

    fn app_running(root: &Path, sounds_alike: bool, program: PathBuf) -> Router {
        router(App {
            root: std::fs::canonicalize(root).unwrap(),
            slots: Arc::new(Semaphore::new(1)),
            program,
            sounds_alike,
        })
    }

    async fn post(app: Router, path: &Path) -> StatusCode {
        post_parts(app, path, None).await.0
    }

    async fn post_parts(app: Router, path: &Path, parts: Option<&[&str]>) -> (StatusCode, serde_json::Value) {
        let body = match parts {
            Some(parts) => json!({ "path": path.to_string_lossy(), "parts": parts }),
            None => json!({ "path": path.to_string_lossy() }),
        };
        post_body(app, body).await
    }

    async fn post_body(app: Router, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        let res = app.oneshot(Request::post("/analyse").header("content-type", "application/json")
            .body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = res.status();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null))
    }

    /// A stand-in reader that answers with the parts it was started with, so a test sees what
    /// the child was asked to read.
    #[cfg(unix)]
    fn echoing_reader(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let program = dir.join("echo-reader.sh");
        std::fs::write(&program, "#!/bin/sh\nprintf '{\"parts\":\"%s\",\"bpm\":\"%s\"}' \"$3\" \"$4\"\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        program
    }

    async fn health(app: Router) -> (StatusCode, serde_json::Value) {
        let res = app.oneshot(Request::get("/health").body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn health_says_the_feature_version() {
        let root = std::env::temp_dir().join("octo-sonic-health");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("song.flac"), b"x").unwrap();
        let (status, value) = health(app(&root)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["featuresVersion"], json!(2));
        // Profiles of the second pass: the version the apps read stays 1.
        assert_eq!(value["transitionVersion"], json!(1));
        assert_eq!(value["transitionRevision"], json!(2));
        let blind = std::env::temp_dir().join("octo-sonic-health-blind");
        std::fs::create_dir_all(&blind).unwrap();
        let (status, value) = health(app(&blind)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!((&value["transitionVersion"], &value["transitionRevision"]), (&json!(1), &json!(2)));
    }

    #[test]
    fn a_reading_asks_for_the_sound_unless_told_otherwise() {
        assert_eq!(Parts::of([]), Parts { sound: true, transition: false });
        assert_eq!(Parts::of(["nonsense"]), Parts { sound: true, transition: false });
        assert_eq!(Parts::of(["transition"]), Parts { sound: false, transition: true });
        assert_eq!(Parts::of(["sound", " transition"]).arg(), "sound,transition");
        assert_eq!(Parts::of("transition".split(',')).arg(), "transition");
    }

    #[test]
    fn only_a_clear_no_turns_sounds_alike_off() {
        for off in ["false", "FALSE", " 0 ", "off", "no"] {
            assert!(!sounds_alike(Some(off)), "{off}");
        }
        for on in ["true", "1", "", "yes"] {
            assert!(sounds_alike(Some(on)), "{on}");
        }
        assert!(sounds_alike(None));
    }

    #[test]
    fn without_sounds_alike_only_the_profile_is_left() {
        assert_eq!(Parts::of(["sound", "transition"]).without_sound(), Parts { sound: false, transition: true });
        assert!(Parts::of(["sound"]).without_sound().is_empty());
    }

    #[tokio::test]
    async fn with_sounds_alike_off_it_is_healthy_and_says_so() {
        let root = std::env::temp_dir().join("octo-sonic-off");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("song.flac"), b"x").unwrap();
        let (status, value) = health(app_with(&root, false)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["ok"], json!(true));
        assert_eq!(value["soundsAlike"], json!(false));
        assert_eq!(value["transitionVersion"], json!(1));
        assert_eq!(health(app(&root)).await.1["soundsAlike"], json!(true));
    }

    #[tokio::test]
    async fn with_sounds_alike_off_the_sound_alone_is_refused() {
        let root = std::env::temp_dir().join("octo-sonic-off-sound");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("song.flac"), b"x").unwrap();
        let song = root.join("song.flac");
        assert_eq!(post(app_with(&root, false), &song).await, StatusCode::SERVICE_UNAVAILABLE);
        let (status, value) = post_parts(app_with(&root, false), &song, Some(&["sound"])).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(value["off"], json!(true));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn with_sounds_alike_off_only_the_profile_is_read() {
        let root = std::env::temp_dir().join("octo-sonic-off-profile");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("song.flac"), b"x").unwrap();
        let reader = echoing_reader(&std::env::temp_dir().join("octo-sonic-off-profile-reader"));
        let song = root.join("song.flac");
        let (status, value) =
            post_parts(app_running(&root, false, reader.clone()), &song, Some(&["sound", "transition"])).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["parts"], json!("transition"));
        let (_, value) =
            post_parts(app_running(&root, true, reader.clone()), &song, Some(&["sound", "transition"])).await;
        assert_eq!(value["parts"], json!("sound,transition"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_published_tempo_goes_to_the_reader_only_with_a_profile() {
        let root = std::env::temp_dir().join("octo-sonic-bpm");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("song.flac"), b"x").unwrap();
        let reader = echoing_reader(&std::env::temp_dir().join("octo-sonic-bpm-reader"));
        let song = root.join("song.flac").to_string_lossy().to_string();
        let ask = |parts: &[&str], bpm: serde_json::Value| {
            let app = app_running(&root, true, reader.clone());
            let body = json!({ "path": song, "parts": parts, "bpm": bpm });
            async move { post_body(app, body).await.1["bpm"].clone() }
        };
        assert_eq!(ask(&["transition"], json!(128.5)).await, json!("128.5"));
        assert_eq!(ask(&["sound"], json!(128.5)).await, json!(""));
        assert_eq!(ask(&["transition"], json!(0)).await, json!(""));
        assert_eq!(ask(&["transition"], json!(-3)).await, json!(""));
        assert_eq!(ask(&["transition"], serde_json::Value::Null).await, json!(""));
    }

    #[test]
    fn only_a_real_tempo_is_handed_on() {
        assert_eq!(published_bpm(Some(128.0)), Some(128.0));
        for missing in [None, Some(0.0), Some(-1.0), Some(f64::NAN), Some(f64::INFINITY), Some(5_000.0)] {
            assert_eq!(published_bpm(missing), None);
        }
    }

    #[tokio::test]
    async fn an_empty_music_folder_is_not_healthy() {
        let root = std::env::temp_dir().join("octo-sonic-empty");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let (status, value) = health(app(&root)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(value["ok"], json!(false));
    }

    #[tokio::test]
    async fn a_path_outside_the_music_folder_is_refused() {
        let root = std::env::temp_dir().join("octo-sonic-root");
        std::fs::create_dir_all(&root).unwrap();
        let outside = std::env::temp_dir().join("octo-sonic-outside.txt");
        std::fs::write(&outside, b"x").unwrap();
        assert_eq!(post(app(&root), &outside).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_missing_file_is_missing_not_refused() {
        let root = std::fs::canonicalize({
            let root = std::env::temp_dir().join("octo-sonic-root3");
            std::fs::create_dir_all(&root).unwrap();
            root
        }).unwrap();
        assert_eq!(post(app(&root), &root.join("gone.flac")).await, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_missing_path_that_climbs_out_is_refused() {
        let root = std::fs::canonicalize({
            let root = std::env::temp_dir().join("octo-sonic-root4");
            std::fs::create_dir_all(&root).unwrap();
            root
        }).unwrap();
        assert_eq!(post(app(&root), &root.join("../nowhere/gone.flac")).await, StatusCode::FORBIDDEN);
    }

    #[test]
    fn a_file_that_is_not_music_is_an_error_not_a_crash() {
        let junk = std::env::temp_dir().join("octo-sonic-junk.flac");
        std::fs::write(&junk, b"not audio").unwrap();
        assert_eq!(read_one(&junk, Parts::of(["sound", "transition"]), Some(120.0)), 2);
    }
}
