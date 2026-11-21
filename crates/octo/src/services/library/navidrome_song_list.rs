//! Navidrome's own song list (`GET /api/song`, 1000 a page, as the admin), which names every
//! song's artist, title, album and tag lyrics in a few requests. A scan that would otherwise open
//! every file over a network mount (a few songs a second) reads this instead.

use std::collections::HashMap;
use std::path::{Component, PathBuf};

use octo_core::common::dotnet::is_blank;
use tracing::info;

use crate::services::framework::HttpAnswer;
use crate::services::framework::http::parse_json;
use crate::services::subsonic::NavidromeIdentityService;

/// One song as Navidrome's native song list has it, by the full path of its file.
#[derive(Debug, Clone, PartialEq)]
pub struct NavidromeSongEntry {
    pub full_path: String,
    pub album_id: Option<String>,
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
    pub lyrics: Option<String>,
    /// Navidrome's id for the song.
    pub id: Option<String>,
    /// The length in seconds, as Navidrome read it.
    pub duration: Option<f64>,
    /// What Navidrome read from the advisory tag: "e" (explicit), "c" (clean) or "".
    pub explicit_status: Option<String>,
    /// Navidrome no longer finds the file: it was moved or deleted and the scan could not
    /// match it to a file somewhere else.
    pub missing: bool,
}

impl NavidromeSongEntry {
    /// Whether Navidrome read lyrics from the song's tags (not a file beside it).
    pub fn has_tag_lyrics(&self) -> bool {
        self.lyrics
            .as_deref()
            .is_some_and(|lyrics| !is_blank(lyrics) && !matches!(lyrics.trim(), "[]" | "null"))
    }
}

/// Every song, keyed by full path (the same file can be named under the library path and the
/// music root, so both are keys). Empty when Navidrome cannot be asked.
pub async fn list(
    identity: Option<&NavidromeIdentityService>,
    http: Option<&reqwest::Client>,
    base_url: Option<&str>,
    root: &str,
) -> HashMap<String, NavidromeSongEntry> {
    let mut songs: HashMap<String, NavidromeSongEntry> = HashMap::new();
    let (Some(identity), Some(http), Some(base_url)) =
        (identity, http, base_url.filter(|url| !is_blank(url)))
    else {
        return songs;
    };
    let walked = each_song(identity, http, base_url, |song| {
        let text = |name: &str| {
            song.get(name)
                .and_then(|value| value.as_str())
                .map(str::to_string)
        };
        let Some(path) = text("path").filter(|path| !path.is_empty()) else {
            return;
        };
        let relative = path
            .replace('\\', "/")
            .trim_start_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>()
            .join("/");
        let mut paths = Vec::new();
        for base in [text("libraryPath"), Some(root.to_string())]
            .into_iter()
            .flatten()
        {
            if !base.is_empty() {
                paths.push(full_path(&combine(&base, &relative)));
            }
        }
        // An older Navidrome reports the full path instead.
        if path.starts_with('/') {
            paths.push(full_path(&path));
        }
        for full in paths {
            songs.entry(full.clone()).or_insert_with(|| NavidromeSongEntry {
                full_path: full,
                album_id: text("albumId"),
                artist: text("artist"),
                title: text("title"),
                album: text("album"),
                lyrics: text("lyrics"),
                id: text("id"),
                duration: song.get("duration").and_then(serde_json::Value::as_f64),
                explicit_status: text("explicitStatus"),
                missing: song
                    .get("missing")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            });
        }
    })
    .await;
    if let Err(failure) = walked {
        info!("Could not list Navidrome's songs, so each file is read instead: {failure}");
    }
    songs
}

/// Walks Navidrome's native song list (`GET /api/song`, 1000 a page, as the admin), handing
/// each song's JSON to `visit`. Answers false when there is no admin sign-in to ask with, and
/// stops quietly at a page Navidrome refuses or that is not a list.
pub(crate) async fn each_song(
    identity: &NavidromeIdentityService,
    http: &reqwest::Client,
    base_url: &str,
    mut visit: impl FnMut(&serde_json::Value),
) -> anyhow::Result<bool> {
    let Some(jwt) = identity.ensure_admin_jwt().await.filter(|jwt| !jwt.is_empty()) else {
        return Ok(false);
    };
    const PAGE: usize = 1000;
    let mut start = 0;
    while start < 200_000 {
        let url = format!(
            "{}/api/song?_start={start}&_end={}&_sort=media_file.id&_order=ASC",
            base_url.trim_end_matches('/'),
            start + PAGE
        );
        let answer = HttpAnswer::read(
            http.get(&url)
                .header("X-Nd-Authorization", format!("Bearer {jwt}"))
                .send()
                .await?,
        )
        .await?;
        if !answer.is_success() {
            break;
        }
        let doc = parse_json(&answer.body)?;
        let Some(page) = doc.as_array() else {
            break;
        };
        for song in page {
            visit(song);
        }
        if page.len() < PAGE {
            break;
        }
        start += PAGE;
    }
    Ok(true)
}

/// `relative` joined onto `base` with one separator; `base` alone when `relative` is empty.
pub(crate) fn combine(base: &str, relative: &str) -> String {
    if relative.is_empty() {
        base.to_string()
    } else if base.ends_with('/') {
        format!("{base}{relative}")
    } else {
        format!("{base}/{relative}")
    }
}

/// The full path: made absolute against the working folder, with `.`, `..` and doubled
/// separators taken out.
pub(crate) fn full_path(path: &str) -> String {
    let absolute = if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut parts: Vec<String> = Vec::new();
    for component in absolute.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    let mut full = format!("/{}", parts.join("/"));
    if path.ends_with('/') && full.len() > 1 {
        full.push('/');
    }
    full
}
