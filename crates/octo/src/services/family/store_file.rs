//! Loading and saving the family's JSON files. A file that cannot be read is never thrown
//! away: it is renamed to `<name>.broken-<unix time>` and the store starts empty, with a
//! warning the Family page can show. An empty file counts as one that cannot be read: Octo
//! never writes one, so it is what a crash in the middle of a write leaves.
//!
//! Every save writes the whole file through a temporary one that reaches the disk before it is
//! renamed into place. Called from async code on the multi-threaded runtime, the write runs as
//! blocking work, so the runtime moves its other tasks off the thread first.

use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::warn;

use crate::services::state_file;

/// The words a family file's messages name its store by.
const FAMILY: &str = "Family";

/// [`load_in`] for the family's files.
pub(crate) fn load<T: DeserializeOwned + Default>(path: Option<&Path>) -> (T, Option<String>) {
    load_in(path, FAMILY)
}

/// The value in `path` (the default when the file is missing or there is no path), and a
/// warning naming `area` (the part of Octo the store belongs to: "Family", "Playlists") when the
/// file was there but unreadable and has been kept aside.
pub(crate) fn load_in<T: DeserializeOwned + Default>(path: Option<&Path>, area: &str) -> (T, Option<String>) {
    let Some(path) = path else {
        return (T::default(), None);
    };
    let problem = match state_file::read_text(path) {
        Ok(None) => return (T::default(), None),
        Ok(Some(text)) if text.trim().is_empty() => "the file is empty".to_string(),
        Ok(Some(text)) => match serde_json::from_str::<T>(&text) {
            Ok(value) => return (value, None),
            Err(e) => e.to_string(),
        },
        Err(e) => e.to_string(),
    };
    let kept = broken_path(path, chrono::Utc::now().timestamp());
    let message = match std::fs::rename(path, &kept) {
        Ok(()) => format!(
            "{} could not be read ({problem}); it was kept as {} and {area} started without it",
            file_name(path),
            file_name(&kept)
        ),
        Err(e) => format!(
            "{} could not be read ({problem}) or kept aside ({e}); {area} started without it",
            file_name(path)
        ),
    };
    warn!("{} {message}", area.to_uppercase());
    (T::default(), Some(message))
}

/// [`save_with_in`] for the family's files.
pub(crate) fn save_with<T: Serialize>(path: Option<&Path>, lock: &Mutex<()>, snapshot: impl FnOnce() -> T) {
    save_with_in(path, lock, FAMILY, snapshot);
}

/// Writes what `snapshot` reads to `path` whole (through a temporary file synced to disk),
/// logging a failure under `area`, holding `lock` so two saves never write at once and the later
/// one writes the newer state. The wait for `lock` is blocking work too ([`blocking`]), so a save
/// waiting behind another's write never holds up the other tasks on its async worker.
pub(crate) fn save_with_in<T: Serialize>(
    path: Option<&Path>,
    lock: &Mutex<()>,
    area: &str,
    snapshot: impl FnOnce() -> T,
) {
    // `try_save_with` logs a failure; a store that saves this way carries on.
    let _ = try_save_with(path, lock, area, snapshot);
}

/// [`save_with_in`] that hands a failed write back, for a store that undoes its change in memory
/// when the file could not be written. The failure is logged under `area` as well.
pub(crate) fn try_save_with<T: Serialize>(
    path: Option<&Path>,
    lock: &Mutex<()>,
    area: &str,
    snapshot: impl FnOnce() -> T,
) -> anyhow::Result<()> {
    let Some(path) = path else { return Ok(()) };
    let written = blocking(|| {
        let _saving = lock.lock();
        let text = octo_core::json::to_string(&snapshot());
        state_file::save_atomic(path, &text)
    });
    written.map_err(|e| {
        warn!(
            "{} {} could not be written: {e}",
            area.to_uppercase(),
            file_name(path)
        );
        anyhow::anyhow!("{} could not be written: {e}", file_name(path))
    })
}

/// Runs `work` (file writes) as blocking work: on a worker of the multi-threaded runtime the
/// runtime first hands the thread's other tasks to the other workers, so a slow disk never
/// stalls them. Anywhere else (a blocking thread, a single-threaded runtime, no runtime) it
/// simply runs.
pub(crate) fn blocking<R>(work: impl FnOnce() -> R) -> R {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
}

/// `<path>.broken-<unix time>`.
pub(crate) fn broken_path(path: &Path, unix_time: i64) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".broken-{unix_time}"));
    PathBuf::from(name)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// A path that is set and not blank.
pub(crate) fn clean_path(path: Option<impl AsRef<Path>>) -> Option<PathBuf> {
    path.map(|p| p.as_ref().to_path_buf())
        .filter(|p| !p.as_os_str().to_string_lossy().trim().is_empty())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_store_file_torn_by_a_crash_is_kept_aside_never_read_as_empty() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("family-pool.json");
        save_with(Some(&path), &Mutex::new(()), || {
            vec!["one".to_string(), "two".to_string()]
        });
        let (read, warning) = load::<Vec<String>>(Some(&path));
        assert_eq!(read, ["one", "two"]);
        assert!(warning.is_none());

        // A crash cut the file short.
        let whole = std::fs::read_to_string(&path).expect("reads");
        std::fs::write(&path, &whole[..whole.len() / 2]).expect("a torn file");
        let (read, warning) = load::<Vec<String>>(Some(&path));
        assert!(read.is_empty());
        assert!(warning.is_some_and(|w| w.contains("could not be read")));
        let kept: Vec<_> = std::fs::read_dir(dir.path())
            .expect("lists")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("family-pool.json.broken-"))
            .collect();
        assert_eq!(kept.len(), 1, "the torn file is kept for the owner");

        // A crash left it empty.
        std::fs::write(&path, "").expect("an empty file");
        let (_, warning) = load::<Vec<String>>(Some(&path));
        assert!(
            warning.is_some_and(|w| w.contains("the file is empty")),
            "an empty store file is broken, not an empty store"
        );
        assert!(!path.exists());
    }

    #[test]
    fn messages_name_the_stores_area() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("playlist-folders.json");
        std::fs::write(&path, "{ torn").expect("a torn file");
        let (read, warning) = load_in::<Vec<String>>(Some(&path), "Playlist folders");
        assert!(read.is_empty());
        let warning = warning.expect("a warning");
        assert!(
            warning.contains("Playlist folders started without it"),
            "{warning}"
        );
        assert!(!warning.contains("Family"), "{warning}");

        std::fs::write(&path, "").expect("an empty file");
        let (_, family) = load::<Vec<String>>(Some(&path));
        assert!(family.is_some_and(|w| w.contains("Family started without it")));

        // A write that fails is handed back, naming the file.
        let blocked = dir.path().join("held.json");
        std::fs::create_dir_all(&blocked).expect("a folder where the file goes");
        let failed = try_save_with(Some(&blocked), &Mutex::new(()), "Playlists", || vec![1]);
        assert!(failed.is_err_and(|e| e.to_string().contains("held.json")));
        let fine = try_save_with(
            Some(&dir.path().join("ok.json")),
            &Mutex::new(()),
            "Playlists",
            || vec![1],
        );
        assert!(fine.is_ok());
    }

    #[test]
    fn blocking_work_runs_anywhere() {
        assert_eq!(blocking(|| 7), 7);
        let single = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime");
        assert_eq!(single.block_on(async { blocking(|| 8) }), 8);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn a_save_waiting_for_another_save_never_stalls_the_other_tasks() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("family-requests.json");
        let lock = Arc::new(Mutex::new(()));
        // Another save holds the lock through a slow write for 300 ms.
        let (held, release) = (Arc::new(std::sync::Barrier::new(2)), Arc::clone(&lock));
        let holder = {
            let held = Arc::clone(&held);
            std::thread::spawn(move || {
                let _saving = release.lock();
                held.wait();
                std::thread::sleep(Duration::from_millis(300));
            })
        };
        held.wait();
        let ticks = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ticks);
        let ticker = tokio::spawn(async move {
            loop {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let before = ticks.load(Ordering::SeqCst);
        save_with(Some(&path), &lock, || vec!["after".to_string()]);
        let during = ticks.load(Ordering::SeqCst) - before;
        ticker.abort();
        holder.join().expect("joins");
        assert!(
            during >= 10,
            "the other task ran {during} times while the save waited for the lock"
        );
        let (read, _) = load::<Vec<String>>(Some(&path));
        assert_eq!(read, ["after"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn blocking_work_never_stalls_the_other_tasks() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ticks);
        let ticker = tokio::spawn(async move {
            loop {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let before = ticks.load(Ordering::SeqCst);
        // The only worker thread is busy with a slow write for 300 ms.
        blocking(|| std::thread::sleep(Duration::from_millis(300)));
        let during = ticks.load(Ordering::SeqCst) - before;
        ticker.abort();
        assert!(
            during >= 10,
            "the other task ran {during} times while the write blocked"
        );
    }
}
