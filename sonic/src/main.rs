//! octo-sonic: reads one song and answers with bliss's numbers for how it sounds.
//! Octo keeps the numbers and finds songs that sound alike; this only reads files,
//! and only under MUSIC_ROOT. Each file is read by a child process (`octo-sonic read
//! <path>`): FFmpeg can crash on a damaged file, and a crash there must cost that file,
//! not the server.

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
}

#[derive(Deserialize)]
struct Analyse {
    path: String,
}

fn version() -> u16 {
    u16::from(FeaturesVersion::LATEST)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 3 && args[1] == "read" {
        std::process::exit(read_one(Path::new(&args[2])));
    }
    tracing_subscriber::fmt::init();
    let root = std::env::var("MUSIC_ROOT").unwrap_or_else(|_| "/music".into());
    let root = std::fs::canonicalize(&root).unwrap_or_else(|_| PathBuf::from(root));
    let slots = std::env::var("ANALYSE_CONCURRENCY").ok().and_then(|v| v.parse().ok()).unwrap_or(1usize).max(1);
    let program = std::env::current_exe().expect("own path");
    let app = router(App { root, slots: Arc::new(Semaphore::new(slots)), program });
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

/// The child: print the features as JSON on stdout, or the error on stderr. Exit 0 or 2.
fn read_one(path: &Path) -> i32 {
    match FFmpegDecoder::song_from_path(path) {
        Ok(song) => {
            println!("{}", json!({ "features": song.analysis.as_vec(), "version": version() }));
            0
        }
        Err(e) => {
            eprintln!("{e}");
            2
        }
    }
}

fn router(app: App) -> Router {
    Router::new().route("/health", get(health)).route("/analyse", post(analyse)).with_state(app)
}

/// Healthy only when the music folder is there and has something in it: a mount that failed
/// leaves an empty folder, and every song would then look missing.
async fn health(State(app): State<App>) -> impl IntoResponse {
    let root = app.root.clone();
    let sees = tokio::task::spawn_blocking(move || {
        std::fs::read_dir(root).map(|mut entries| entries.next().is_some()).unwrap_or(false)
    })
    .await
    .unwrap_or(false);
    if sees {
        (StatusCode::OK, Json(json!({ "ok": true, "featuresVersion": version() })))
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, Json(json!({
            "ok": false, "featuresVersion": version(),
            "error": "cannot see the music folder",
        })))
    }
}

async fn analyse(State(app): State<App>, Json(body): Json<Analyse>) -> impl IntoResponse {
    let path = match inside(&app.root, Path::new(&body.path)) {
        Ok(path) => path,
        Err(Refused::Missing) => return (StatusCode::NOT_FOUND, Json(json!({ "error": "no such file" }))),
        Err(Refused::Outside) => return (StatusCode::FORBIDDEN, Json(json!({ "error": "outside the music folder" }))),
    };
    if !path.is_file() {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "not a file" })));
    }
    let _slot = app.slots.acquire().await.expect("semaphore");
    let child = tokio::process::Command::new(&app.program)
        .arg("read").arg(&path)
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
        router(App {
            root: std::fs::canonicalize(root).unwrap(),
            slots: Arc::new(Semaphore::new(1)),
            program: PathBuf::from("/nonexistent/octo-sonic"),
        })
    }

    async fn post(app: Router, path: &Path) -> StatusCode {
        let body = json!({ "path": path.to_string_lossy() }).to_string();
        app.oneshot(Request::post("/analyse").header("content-type", "application/json")
            .body(Body::from(body)).unwrap()).await.unwrap().status()
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
        assert_eq!(read_one(&junk), 2);
    }
}
