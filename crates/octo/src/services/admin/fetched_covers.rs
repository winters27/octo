//! `FetchedCovers`: the covers the dashboard draws, always served from Octo's own address.
//!
//! A page never loads a cover from Deezer or Apple itself: a browser that cannot reach those
//! hosts would show no covers at all, and a song the log saved no cover address for would show
//! none either. A fetched song's cover is read from its own file first (the picture the library
//! and the apps show), then fetched by Octo from the cover address saved with it. Every answer is
//! a small JPEG, kept in memory, so a page asks as often as it likes.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use futures::StreamExt;
use octo_core::models::download::DownloadHistoryEntry;
use octo_media::cover::cover_image;
use octo_media::tags::{TagFile, front_cover};
use parking_lot::Mutex;
use tokio::sync::Semaphore;
use tracing::debug;

/// The side of a small cover (a row, the strip of covers): 56 px drawn at up to three times.
pub const SMALL_SIDE: u32 = 168;
/// The side of a large cover (Latest fetched's art, the Listening page's top song).
pub const LARGE_SIDE: u32 = 504;

/// The side for the `size` a page asks for: large above 200 px, small otherwise.
pub fn side_for(size: Option<&str>) -> u32 {
    match size.and_then(|size| size.trim().parse::<u32>().ok()) {
        Some(px) if px > 200 => LARGE_SIDE,
        _ => SMALL_SIDE,
    }
}

/// The address a fetched song's cover is served at, by the moment its entry was written.
pub fn entry_cover_path(downloaded_at: &str) -> String {
    let at: String = url::form_urlencoded::byte_serialize(downloaded_at.as_bytes()).collect();
    format!("/api/admin/downloads/cover?at={at}")
}

/// The picture of a song file: its own front cover, else the cover file in its folder
/// (`cover.*`, then `folder.*`, then `front.*`, the order Navidrome ranks them, any case). None
/// when the file is gone: a folder's cover then says nothing about which song this was.
pub fn song_picture(path: &str) -> Option<Vec<u8>> {
    let file = Path::new(path);
    if path.trim().is_empty() || !file.is_file() {
        return None;
    }
    if let Some(embedded) = TagFile::open(path)
        .ok()
        .and_then(|tags| front_cover(&tags))
        .map(|picture| picture.data)
        .filter(|bytes| cover_image::measure(bytes).is_some())
    {
        return Some(embedded);
    }
    folder_cover(file.parent()?)
}

fn folder_cover(folder: &Path) -> Option<Vec<u8>> {
    let mut names: Vec<(String, std::path::PathBuf)> = std::fs::read_dir(folder)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| (entry.file_name().to_string_lossy().to_lowercase(), entry.path()))
        .collect();
    names.sort();
    for stem in ["cover.", "folder.", "front."] {
        for (_, path) in names.iter().filter(|(name, _)| name.starts_with(stem)) {
            if let Ok(bytes) = std::fs::read(path)
                && cover_image::measure(&bytes).is_some()
            {
                return Some(bytes);
            }
        }
    }
    None
}

/// A picture as a JPEG no larger than `side`.
fn small_jpeg(bytes: &[u8], side: u32) -> Vec<u8> {
    cover_image::to_jpeg(&cover_image::fit_within(bytes, side))
}

/// When a file and its folder last changed, so a new cover in either is read again.
fn changed_at(path: &str) -> String {
    let at = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_nanos())
    };
    let file = Path::new(path);
    format!("{}:{}", at(file), file.parent().map_or(0, at))
}

type Picture = Option<Arc<Vec<u8>>>;
type ReadSong = Arc<dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync>;

/// What an outside address gave: a picture, a clear no (not a success, not a picture, too
/// large), or no answer at all (unreachable, timed out), which is asked again next time.
enum Fetched {
    Picture(Vec<u8>),
    Nothing,
    Failed,
}

#[derive(Default)]
struct Kept {
    covers: HashMap<String, Picture>,
    bytes: usize,
}

impl Kept {
    fn get(&self, key: &str) -> Option<Picture> {
        self.covers.get(key).cloned()
    }

    fn put(&mut self, key: String, picture: Picture) {
        let size = picture.as_ref().map_or(0, |p| p.len());
        if self.bytes + size > FetchedCovers::MAX_KEPT_BYTES || self.covers.len() >= FetchedCovers::MAX_KEPT {
            self.covers.clear();
            self.bytes = 0;
        }
        if let Some(old) = self.covers.insert(key, picture) {
            self.bytes -= old.map_or(0, |p| p.len());
        }
        self.bytes += size;
    }
}

pub struct FetchedCovers {
    http: reqwest::Client,
    reads: Arc<Semaphore>,
    file_budget: Duration,
    read_song: ReadSong,
    kept: Arc<Mutex<Kept>>,
}

impl FetchedCovers {
    /// The outside client's timeout.
    pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(6);
    /// How long a fetched song's file may take before the saved address answers in its place
    /// (a library on a network mount reads slowly the first time).
    pub const FILE_BUDGET: Duration = Duration::from_secs(3);

    // A cover is a few hundred kilobytes; anything far past that is not one.
    const MAX_BYTES: usize = 8 * 1024 * 1024;
    const MAX_KEPT_BYTES: usize = 8 * 1024 * 1024;
    const MAX_KEPT: usize = 1024;
    // Files and addresses read at once, so a long page does not swamp the disk.
    const READS_AT_ONCE: usize = 4;

    /// The production service, on its own 6 s client.
    pub fn new() -> Self {
        // The client does not decompress responses.
        let http = reqwest::Client::builder()
            .timeout(Self::CLIENT_TIMEOUT)
            .no_gzip()
            .no_deflate()
            .build()
            .expect("a plain HTTP client builds");
        Self::with_client(http, Self::FILE_BUDGET)
    }

    pub fn with_client(http: reqwest::Client, file_budget: Duration) -> Self {
        FetchedCovers {
            http,
            reads: Arc::new(Semaphore::new(Self::READS_AT_ONCE)),
            file_budget,
            read_song: Arc::new(song_picture),
            kept: Arc::new(Mutex::new(Kept::default())),
        }
    }

    /// Tests only: song files are read by `read_song`.
    #[cfg(test)]
    pub fn reading_songs_with(
        mut self,
        read_song: impl Fn(&str) -> Option<Vec<u8>> + Send + Sync + 'static,
    ) -> Self {
        self.read_song = Arc::new(read_song);
        self
    }

    /// A fetched song's cover at `side`: from its file, else from the cover address saved with
    /// it. A file slower than the budget gives way to the saved address this time, when there is
    /// one, and its picture is kept for the next ask.
    pub async fn entry_cover(&self, entry: &DownloadHistoryEntry, side: u32) -> Picture {
        let url = entry
            .cover_art_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty());
        let key = format!(
            "entry\n{}\n{}\n{}\n{side}\n{}",
            entry.downloaded_at,
            entry.path,
            url.unwrap_or(""),
            changed_at(&entry.path)
        );
        if let Some(kept) = self.kept.lock().get(&key) {
            return kept;
        }
        let mut file = self.read_file(entry.path.clone(), side);
        let from_file = match url {
            None => file.await.ok().flatten(),
            Some(url) => match tokio::time::timeout(self.file_budget, &mut file).await {
                Ok(done) => done.ok().flatten(),
                Err(_) => {
                    let kept = Arc::clone(&self.kept);
                    tokio::spawn(async move {
                        if let Ok(Some(picture)) = file.await {
                            kept.lock().put(key, Some(picture));
                        }
                    });
                    return self.outside_cover(url, side).await;
                }
            },
        };
        let picture = match (from_file, url) {
            (Some(picture), _) => Some(picture),
            (None, Some(url)) => self.outside_cover(url, side).await,
            (None, None) => None,
        };
        self.kept.lock().put(key, picture.clone());
        picture
    }

    /// A library song's cover at `side`, read from its file or its folder.
    pub async fn file_cover(&self, path: &str, side: u32) -> Picture {
        let key = format!("file\n{path}\n{side}\n{}", changed_at(path));
        if let Some(kept) = self.kept.lock().get(&key) {
            return kept;
        }
        let picture = self.read_file(path.to_string(), side).await.ok().flatten();
        self.kept.lock().put(key, picture.clone());
        picture
    }

    /// The picture at an outside address at `side`, fetched by Octo. Only http and https.
    pub async fn outside_cover(&self, url: &str, side: u32) -> Picture {
        let key = format!("outside\n{url}\n{side}");
        if let Some(kept) = self.kept.lock().get(&key) {
            return kept;
        }
        let picture = match self.fetch(url).await {
            Fetched::Picture(bytes) => tokio::task::spawn_blocking(move || small_jpeg(&bytes, side))
                .await
                .ok()
                .map(Arc::new),
            Fetched::Nothing => None,
            Fetched::Failed => return None,
        };
        self.kept.lock().put(key, picture.clone());
        picture
    }

    fn read_file(&self, path: String, side: u32) -> tokio::task::JoinHandle<Picture> {
        let reads = Arc::clone(&self.reads);
        let read_song = Arc::clone(&self.read_song);
        tokio::spawn(async move {
            let _turn = reads.acquire_owned().await.ok()?;
            tokio::task::spawn_blocking(move || {
                read_song(&path).map(|bytes| Arc::new(small_jpeg(&bytes, side)))
            })
            .await
            .ok()
            .flatten()
        })
    }

    async fn fetch(&self, url: &str) -> Fetched {
        let Some(uri) = url::Url::parse(url.trim())
            .ok()
            .filter(|uri| matches!(uri.scheme(), "http" | "https"))
        else {
            return Fetched::Nothing;
        };
        let Ok(_turn) = self.reads.acquire().await else {
            return Fetched::Failed;
        };
        match self.read(uri).await {
            Ok(bytes) if cover_image::measure(&bytes).is_some() => Fetched::Picture(bytes),
            Ok(_) => Fetched::Nothing,
            Err(e) => {
                debug!("Could not fetch a cover for the dashboard: {e}");
                Fetched::Failed
            }
        }
    }

    /// The picture's bytes, or none when the answer was not a success, said it was too large,
    /// or ran past the limit while it was read.
    async fn read(&self, uri: url::Url) -> Result<Vec<u8>, reqwest::Error> {
        let response = self.http.get(uri).send().await?;
        if !response.status().is_success()
            || response.content_length().unwrap_or(0) as usize > Self::MAX_BYTES
        {
            return Ok(Vec::new());
        }
        let mut limited = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if limited.len() + chunk.len() > Self::MAX_BYTES {
                limited.clear();
                break;
            }
            limited.extend_from_slice(&chunk);
        }
        Ok(limited)
    }
}

impl Default for FetchedCovers {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "fetched_covers_tests.rs"]
mod tests;
