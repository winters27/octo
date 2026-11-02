use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::http::StatusCode;
use octo_core::settings::SettingsStore;
use octo_subsonic::subsonic_request_parser::escape_data_string;
use serde_json::Value;
use tracing::{debug, warn};

use crate::services::local::ILocalLibraryService;
use crate::services::subsonic::NavidromeIdentityService;

/// How a path was obtained. Logged and surfaced in the dashboard, because the confidence of
/// anything done to the file depends entirely on which of these produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathSource {
    NativeApi,
    SubsonicGetSong,
    LocalMappings,
    None,
}

/// A resolved, VERIFIED file.
///
/// Constructed only by the resolver, and only after the file exists, sits inside the music
/// root, and matches the size Navidrome reported. Anything that acts on a file takes one of
/// these rather than a raw string, so "I have a path" and "I have proof it is the right path"
/// cannot drift apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSongFile {
    pub navidrome_id: String,
    pub absolute_path: String,
    pub size_bytes: i64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub suffix: String,
    pub duration_seconds: Option<i32>,
    pub source: PathSource,
    pub album_artist: Option<String>,
}

/// What a leg reported, before any of it is believed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub raw_path: Option<String>,
    pub library_path: Option<String>,
    pub size: i64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub suffix: String,
    pub duration: Option<i32>,
    pub source: PathSource,
    pub missing: bool,
    pub album_artist: Option<String>,
}

pub struct NavidromeSongPathResolver {
    identity: NavidromeIdentityService,
    library: Arc<dyn ILocalLibraryService>,
    http: reqwest::Client,
    // The Subsonic and library settings, read at the point of use.
    settings: Arc<SettingsStore>,
}

impl NavidromeSongPathResolver {
    pub fn new(
        identity: NavidromeIdentityService,
        library: Arc<dyn ILocalLibraryService>,
        http: reqwest::Client,
        settings: Arc<SettingsStore>,
    ) -> Self {
        NavidromeSongPathResolver {
            identity,
            library,
            http,
            settings,
        }
    }

    fn base_url(&self) -> Option<String> {
        self.settings
            .current()
            .subsonic
            .url
            .clone()
            .filter(|u| !u.trim().is_empty())
    }

    /// Resolve a Navidrome-local song id to a file on Octo's disk, or `None`.
    ///
    /// `None` is a first-class outcome, not an error: an unresolvable id has to make whatever
    /// asked for it a visible no-op. Never return a path that only looks right. Navidrome's
    /// Subsonic `path` is SYNTHESISED FROM TAGS unless the calling player has ReportRealPath set
    /// (it defaults off), so on a library whose filenames are not tag-derived it can name a file
    /// that exists and is a DIFFERENT recording.
    pub async fn resolve(&self, navidrome_id: &str) -> Option<ResolvedSongFile> {
        if navidrome_id.trim().is_empty() {
            return None;
        }

        let root = self.music_root();

        let native = self.try_native(navidrome_id).await;
        if let Some(found) = native.as_ref().and_then(|c| self.verify(c, &root, false)) {
            return Some(found);
        }

        let subsonic = self.try_subsonic(navidrome_id).await;
        if let Some(found) = subsonic.as_ref().and_then(|c| self.verify(c, &root, false)) {
            return Some(found);
        }

        // Tags are the only thing left. Both legs above carry them even when their path is
        // wrong, so prefer whichever actually answered.
        if let Some(tags) = native.as_ref().or(subsonic.as_ref())
            && let Some(found) = self.try_local_mappings(tags, &root).await
        {
            return Some(found);
        }

        warn!(
            "Library: could not resolve Navidrome id {navidrome_id} to a file under {root} (native={}, subsonic={}). No action taken.",
            native.as_ref().and_then(|c| c.raw_path.as_deref()).unwrap_or("-"),
            subsonic
                .as_ref()
                .and_then(|c| c.raw_path.as_deref())
                .unwrap_or("-"),
        );
        None
    }

    /// Navidrome's own view: `mf.Path`, which is library-relative, plus `libraryPath`. This is
    /// the real path and never a fakePath. Uses the admin JWT because that is the only standing
    /// native credential Octo has; the endpoint itself is not admin-only today.
    async fn try_native(&self, id: &str) -> Option<Candidate> {
        let base_url = self.base_url()?;
        let jwt = self.identity.ensure_admin_jwt().await.filter(|j| !j.is_empty())?;

        let url = format!(
            "{}/api/song/{}",
            base_url.trim_end_matches('/'),
            escape_data_string(id)
        );
        let result: Result<Option<Candidate>, String> = async {
            let mut response = self.get_native(&url, &jwt).await?;
            if response.status() == StatusCode::UNAUTHORIZED {
                // An expired admin token: log in again once, rather than treating every song as
                // unresolvable until someone happens to open the dashboard.
                self.identity.invalidate_admin_jwt(&jwt);
                let Some(fresh) = self
                    .identity
                    .ensure_admin_jwt()
                    .await
                    .filter(|f| !f.is_empty() && *f != jwt)
                else {
                    return Ok(None);
                };
                response = self.get_native(&url, &fresh).await?;
            }
            if !response.status().is_success() {
                return Ok(None);
            }
            let body = response.bytes().await.map_err(|e| e.to_string())?;
            let document: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
            Self::from_json(&document, id, PathSource::NativeApi, Some("libraryPath"))
        }
        .await;
        match result {
            Ok(candidate) => candidate,
            Err(message) => {
                debug!("native song lookup failed for {id}: {message}");
                None
            }
        }
    }

    async fn get_native(&self, url: &str, jwt: &str) -> Result<reqwest::Response, String> {
        self.http
            .get(url)
            .header("X-Nd-Authorization", format!("Bearer {jwt}"))
            .send()
            .await
            .map_err(|e| e.to_string())
    }

    /// The Navidrome id of a file Octo just placed: search3 for its artist and title, keep the
    /// hits of exactly this file's size, and take the one whose VERIFIED path is this file.
    /// `None` until Navidrome has scanned it. The size filter is what keeps a common title from
    /// costing a native lookup per hit.
    pub async fn find_id_by_path(&self, artist: &str, title: &str, absolute_path: &str) -> Option<String> {
        let base_url = self.base_url()?;
        let metadata = std::fs::metadata(absolute_path).ok().filter(|m| m.is_file())?;
        let (user, token, salt) = self.identity.get_scan_auth()?;

        let size = metadata.len() as i64;
        let target = get_full_path(absolute_path);
        let result: Result<Option<String>, String> = async {
            let url = format!(
                "{}/rest/search3?f=json&c=octo&v=1.16.1&query={}&songCount=20&albumCount=0&artistCount=0&u={}&t={token}&s={salt}",
                base_url.trim_end_matches('/'),
                escape_data_string(format!("{artist} {title}").trim()),
                escape_data_string(&user),
            );
            let response = self.http.get(&url).send().await.map_err(|e| e.to_string())?;
            if !response.status().is_success() {
                return Ok(None);
            }
            let body = response.bytes().await.map_err(|e| e.to_string())?;
            let document: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
            let Some(Value::Array(songs)) = document
                .get("subsonic-response")
                .and_then(|e| e.get("searchResult3"))
                .and_then(|r| r.get("song"))
            else {
                return Ok(None);
            };

            let mut candidates = Vec::new();
            for song in songs {
                let same_size = match song.get("size") {
                    Some(Value::Number(n)) => n.as_i64().ok_or("a size too large for Int64")? == size,
                    _ => true,
                };
                if !same_size {
                    continue;
                }
                match song.get("id") {
                    Some(Value::String(id)) if !id.is_empty() => candidates.push(id.clone()),
                    Some(Value::String(_)) | Some(Value::Null) | None => {}
                    Some(_) => return Err("an id that is not a string".to_string()),
                }
            }

            for id in candidates {
                if let Some(resolved) = self.resolve(&id).await
                    && get_full_path(&resolved.absolute_path) == target
                {
                    return Ok(Some(id));
                }
            }
            Ok(None)
        }
        .await;
        result.unwrap_or_else(|message| {
            debug!("could not find the Navidrome id of {absolute_path}: {message}");
            None
        })
    }

    /// Whether Navidrome has this id as a present song at exactly this file: not missing,
    /// at this path, at this size. The size proves it read the file after it moved in, since a
    /// replacement at the original's own path is still the old row until the scan.
    pub async fn shows_at(&self, navidrome_id: &str, absolute_path: &str) -> bool {
        let song = self.try_native(navidrome_id).await;
        self.shows(song.as_ref(), &self.music_root(), absolute_path)
    }

    pub(crate) fn shows(&self, song: Option<&Candidate>, root: &str, absolute_path: &str) -> bool {
        song.filter(|s| !s.missing)
            .and_then(|s| self.verify(s, root, true))
            .is_some_and(|resolved| get_full_path(&resolved.absolute_path) == get_full_path(absolute_path))
    }

    /// Subsonic getSong with Octo's admin triplet.
    ///
    /// The `path` here is `fakePath(mf)`, synthesised from TAGS as Artist/Album/NN - Title.ext,
    /// unless the calling player has ReportRealPath set, which defaults off. It is therefore a
    /// HINT, and it is only ever accepted after `verify` matches the byte size.
    async fn try_subsonic(&self, id: &str) -> Option<Candidate> {
        let base_url = self.base_url()?;
        let (user, token, salt) = self.identity.get_scan_auth()?;

        let result: Result<Option<Candidate>, String> = async {
            let url = format!(
                "{}/rest/getSong?f=json&c=octo&v=1.16.1&id={}&u={}&t={token}&s={salt}",
                base_url.trim_end_matches('/'),
                escape_data_string(id),
                escape_data_string(&user),
            );
            let response = self.http.get(&url).send().await.map_err(|e| e.to_string())?;
            if !response.status().is_success() {
                return Ok(None);
            }
            let body = response.bytes().await.map_err(|e| e.to_string())?;
            let document: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
            let Some(song) = document.get("subsonic-response").and_then(|e| e.get("song")) else {
                return Ok(None);
            };
            Self::from_json(song, id, PathSource::SubsonicGetSong, None)
        }
        .await;
        result.unwrap_or_else(|message| {
            debug!("subsonic song lookup failed for {id}: {message}");
            None
        })
    }

    /// Last resort: Octo's own record of where it put a file, matched on artist, title and
    /// album. Only covers downloads Octo made, which is the common case for the mistakes this
    /// exists to fix.
    async fn try_local_mappings(&self, tags: &Candidate, root: &str) -> Option<ResolvedSongFile> {
        let mapping = self
            .library
            .find_mapping_by_tags(Some(&tags.artist), Some(&tags.title), Some(&tags.album))
            .await?;
        if mapping.local_path.is_empty() {
            return None;
        }

        let full = get_full_path(&mapping.local_path);
        if !is_inside(&full, root) {
            return None;
        }
        let metadata = std::fs::metadata(&full).ok().filter(|m| m.is_file())?;
        Some(ResolvedSongFile {
            navidrome_id: tags.id.clone(),
            suffix: extension(&full),
            absolute_path: full,
            size_bytes: metadata.len() as i64,
            title: tags.title.clone(),
            artist: tags.artist.clone(),
            album: tags.album.clone(),
            duration_seconds: tags.duration,
            source: PathSource::LocalMappings,
            album_artist: None,
        })
    }

    /// Turn a candidate into a `ResolvedSongFile`, or `None`. Four checks, all required:
    ///
    ///   1. The path resolves INSIDE the music root, which blocks "../" and an absolute path
    ///      from a differently-mounted Navidrome pointing at Octo's own config directory.
    ///   2. The file exists.
    ///   3. Its byte size equals the size Navidrome reported.
    ///   4. Its extension equals the suffix Navidrome reported.
    ///
    /// (3) is the one that matters. Navidrome's fakePath is built from tags, so on a library
    /// whose filenames do NOT come from its tags it can name a real, DIFFERENT file. Two
    /// distinct audio files agreeing byte-for-byte on length is not a thing that happens by
    /// accident.
    pub(crate) fn verify(&self, candidate: &Candidate, root: &str, quiet: bool) -> Option<ResolvedSongFile> {
        for attempt in Self::candidate_paths(candidate, root) {
            let full = get_full_path(&attempt);

            if !is_inside(&full, root) {
                continue;
            }

            let Some(metadata) = std::fs::metadata(&full).ok().filter(|m| m.is_file()) else {
                continue;
            };
            let length = metadata.len() as i64;

            if candidate.size > 0 && length != candidate.size {
                if !quiet {
                    warn!(
                        "Library: {full} exists but is {length} bytes and Navidrome reports {}. Refusing it, because a path built from tags can name a different file.",
                        candidate.size
                    );
                }
                continue;
            }

            if !candidate.suffix.is_empty() && !extension(&full).eq_ignore_ascii_case(&candidate.suffix) {
                continue;
            }

            return Some(ResolvedSongFile {
                navidrome_id: candidate.id.clone(),
                absolute_path: full,
                size_bytes: length,
                title: candidate.title.clone(),
                artist: candidate.artist.clone(),
                album: candidate.album.clone(),
                suffix: candidate.suffix.clone(),
                duration_seconds: candidate.duration,
                source: candidate.source,
                album_artist: candidate.album_artist.clone(),
            });
        }
        None
    }

    /// Every way the reported path can be joined onto a real directory, in confidence order.
    /// libraryPath first because it is Navidrome's own root and is correct when the two
    /// containers share a mount; then Octo's effective root, which is what the shipped compose
    /// file produces since both mount the same directory at /music; then the rooted-path case.
    pub(crate) fn candidate_paths(candidate: &Candidate, root: &str) -> Vec<String> {
        let mut paths = Vec::new();
        let Some(raw_path) = candidate.raw_path.as_deref().filter(|p| !p.trim().is_empty()) else {
            return paths;
        };

        // Navidrome reports '/' separators regardless of host. Convert to the platform's own
        // before combining, so a yielded path is not a mix of both and a log line is readable.
        let normalized = raw_path.replace('\\', "/");
        let segments: Vec<&str> = normalized
            .trim_start_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        if segments.is_empty() {
            return paths;
        }

        let relative = segments.join("/");

        if let Some(library_path) = candidate.library_path.as_deref().filter(|l| !l.is_empty())
            && Path::new(library_path).is_dir()
        {
            paths.push(combine(library_path, &relative));
        }

        paths.push(combine(root, &relative));

        if raw_path.starts_with('/') {
            paths.push(raw_path.to_string()); // same mount point
            // Different mount: every tail of the path under the music root, longest first, so a
            // file at any depth (a flat `Artist - Title`, an `Artist/Album/Disc 2/` one) is found
            // whatever the other mount's own folders were.
            for start in 1..segments.len() {
                let tail = combine(root, &segments[start..].join("/"));
                if !paths.contains(&tail) {
                    paths.push(tail);
                }
            }
        }
        paths
    }

    /// The music root, read through the SAME accessor the downloader uses.
    /// Subsonic:AutoDetectDownloadPath defaults true, so Library:DownloadPath is only a
    /// fallback, and reading it directly would let this resolver and the downloader disagree
    /// about where the library is. That disagreement is exactly how you act on the wrong
    /// directory.
    pub fn music_root(&self) -> String {
        let configured = self
            .settings
            .raw("Library:DownloadPath")
            .unwrap_or_else(|| "./downloads".to_string());
        self.identity.effective_download_path(&configured)
    }

    /// A candidate from a song object. `Err` for a size or duration that is not a number, which
    /// its callers take as a failed lookup.
    pub(crate) fn from_json(
        element: &Value,
        id: &str,
        source: PathSource,
        library_path_property: Option<&str>,
    ) -> Result<Option<Candidate>, String> {
        if !element.is_object() {
            return Ok(None);
        }
        let number = |name: &str| -> Result<Option<&serde_json::Number>, String> {
            match element.get(name) {
                None => Ok(None),
                Some(Value::Number(n)) => Ok(Some(n)),
                Some(_) => Err(format!(
                    "The requested operation requires an element of type 'Number' ({name})."
                )),
            }
        };
        Ok(Some(Candidate {
            id: id.to_string(),
            raw_path: text(element, "path"),
            library_path: library_path_property.and_then(|p| text(element, p)),
            size: number("size")?.and_then(serde_json::Number::as_i64).unwrap_or(0),
            title: text(element, "title").unwrap_or_default(),
            artist: text(element, "artist").unwrap_or_default(),
            album: text(element, "album").unwrap_or_default(),
            suffix: text(element, "suffix").unwrap_or_default(),
            // Navidrome's native API sends seconds with a fraction (301.37); Subsonic's getSong
            // whole ones.
            duration: number("duration")?
                .and_then(serde_json::Number::as_f64)
                .filter(|secs| *secs > 0.0)
                .and_then(|secs| i32::try_from(secs.round() as i64).ok()),
            source,
            missing: element.get("missing") == Some(&Value::Bool(true)),
            album_artist: text(element, "albumArtist"),
        }))
    }
}

fn text(element: &Value, name: &str) -> Option<String> {
    match element.get(name)? {
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// A relative `b` joined onto `a` with one separator.
fn combine(a: &str, b: &str) -> String {
    if a.is_empty() {
        b.to_string()
    } else if a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// The full path: rooted against the working directory, `.` and `..` resolved lexically
/// (no symbolic links followed), repeated separators collapsed. A trailing separator is kept.
pub(crate) fn get_full_path(path: &str) -> String {
    let joined = if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut parts: Vec<String> = Vec::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    let mut full = format!("/{}", parts.join("/"));
    if path.ends_with('/') && full != "/" {
        full.push('/');
    }
    full
}

/// Whether the full path is strictly under the root (the root itself is not inside it).
pub(crate) fn is_inside(full: &str, root: &str) -> bool {
    let normalized = format!("{}/", get_full_path(root).trim_end_matches('/'));
    full.starts_with(&normalized)
}

/// The extension without its dot: the text after the file name's last dot, empty when there is
/// none or the name ends with it.
fn extension(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => name[dot + 1..].trim_start_matches('.').to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
#[path = "navidrome_song_path_resolver_tests.rs"]
mod tests;
