//! The copy someone picked in Find songs, and the picks waiting for their download to take them.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::{Clock, dotnet};
use octo_core::lidarr::LidarrReleasePick;
use octo_core::models::download::acquisition_event::quality_text;
use octo_core::soulseek::SoulseekFileHit;
use octo_core::soulseek::soulseek_client::normalize_extension;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::services::state_file;

/// Which source a picked copy is from: Soulseek's peer file, or Lidarr's release.
pub const SOULSEEK_SOURCE: &str = "Soulseek";

/// The other source a copy can be picked from.
pub const LIDARR_SOURCE: &str = "Lidarr";

/// One copy someone picked in Find songs: a Soulseek peer's file, or a Lidarr release. Kept as
/// plain values, so an upgrade job waiting for its turn can carry it on disk. What is derived
/// from them (whether it is Soulseek's, the Lidarr release) is never written, so the file holds
/// these fields only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct PickedCopy {
    /// "Soulseek" or "Lidarr".
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub source: String,

    // A Soulseek file, as the search found it.
    pub peer: Option<String>,
    pub file: Option<String>,
    pub size: i64,
    pub format: Option<String>,
    pub bit_rate: Option<i32>,
    pub bit_depth: Option<i32>,
    pub sample_rate: Option<i32>,
    pub length: Option<i32>,

    // A Lidarr release, and its size in Size. Without one, Lidarr chooses by its quality profile.
    pub release_guid: Option<String>,
    pub indexer_id: Option<i32>,
    pub release_title: Option<String>,

    /// Set when the copy replaces a library file whatever its quality, because the person said
    /// so: the song it is held to in place of the library file's own title, album and length.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anyway: Option<AnywaySong>,

    /// Set when the copy replaces a library file the person said is the wrong version, which
    /// Octo could not tell apart by sound: it replaces the file as Wrong version does, whatever
    /// its quality.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub wrong_version: bool,
}

/// The song a copy picked to replace a library file anyway is held to: the library file's own
/// title, album and length, or the plain song when that file is another take of it (a live
/// recording, another version, or a length far from the song's usual one).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct AnywaySong {
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub title: String,
    pub album: Option<String>,
    pub length: Option<i32>,
    /// The library file was another take: once the replacement keeps the song's place, its
    /// title and album become the plain song's.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub retitle: bool,
}

impl Default for PickedCopy {
    fn default() -> Self {
        PickedCopy {
            source: SOULSEEK_SOURCE.to_string(),
            peer: None,
            file: None,
            size: 0,
            format: None,
            bit_rate: None,
            bit_depth: None,
            sample_rate: None,
            length: None,
            release_guid: None,
            indexer_id: None,
            release_title: None,
            anyway: None,
            wrong_version: false,
        }
    }
}

impl PickedCopy {
    /// A Soulseek file rather than a Lidarr release.
    pub fn is_soulseek(&self) -> bool {
        dotnet::eq_ignore_case(&self.source, SOULSEEK_SOURCE)
    }

    /// Whether this is the same copy: the same peer's same file, or the same release.
    pub fn same_as(&self, other: Option<&PickedCopy>) -> bool {
        other.is_some_and(|other| {
            dotnet::eq_ignore_case(&self.source, &other.source)
                && self.peer == other.peer
                && self.file == other.file
                && self.release_guid == other.release_guid
                && self.indexer_id == other.indexer_id
        })
    }

    /// The file as the Soulseek pipeline takes it.
    pub fn to_hit(&self) -> Option<SoulseekFileHit> {
        let peer = self.peer.as_deref().filter(|p| !p.is_empty())?;
        let file = self.file.as_deref().filter(|f| !f.is_empty())?;
        self.is_soulseek().then(|| SoulseekFileHit {
            username: peer.to_string(),
            filename: file.to_string(),
            size: self.size,
            bit_rate: self.bit_rate,
            bit_depth: self.bit_depth,
            sample_rate: self.sample_rate,
            length: self.length,
            extension: normalize_extension(self.format.as_deref(), file),
            ..Default::default()
        })
    }

    /// The release Lidarr is to grab, or None to let it choose.
    pub fn release(&self) -> Option<LidarrReleasePick> {
        let guid = self.release_guid.as_deref().filter(|g| !g.is_empty())?;
        let indexer_id = self.indexer_id?;
        (!self.is_soulseek()).then(|| LidarrReleasePick {
            guid: guid.to_string(),
            indexer_id,
            title: self.release_title.clone(),
            size: (self.size > 0).then_some(self.size),
        })
    }

    /// "FLAC 16-bit 44.1 kHz from someone", for the log and the upgrade queue.
    pub fn describe(&self) -> String {
        if !self.is_soulseek() {
            return match self.release_title.as_deref().filter(|t| !t.is_empty()) {
                Some(release) => format!("Lidarr release \"{release}\""),
                None => "Lidarr's choice".to_string(),
            };
        }
        let quality = quality_text(
            self.format.as_deref(),
            self.bit_rate,
            self.bit_depth,
            self.sample_rate,
        );
        format!(
            "{} from {}",
            quality.as_deref().unwrap_or("a file"),
            self.peer.as_deref().unwrap_or("")
        )
    }
}

/// Copies picked in Find songs, waiting for their download to take them. Keyed by the outside id
/// the download runs under; the Soulseek and Lidarr pipelines take the pick instead of searching.
/// A pick nobody takes is forgotten after a while, so a download asked for much later searches
/// as usual.
///
/// A pick is an `Arc`, so [`DownloadPicks::forget`] can tell the very pick it was given from a
/// newer one for the same song: picks are compared by identity, never by value.
pub struct DownloadPicks {
    picks: Mutex<HashMap<String, (Arc<PickedCopy>, DateTime<Utc>)>>,
    clock: Clock,
}

impl Default for DownloadPicks {
    fn default() -> Self {
        Self::new(Clock::system())
    }
}

impl DownloadPicks {
    /// How long a pick waits for its download.
    pub const KEEP: TimeDelta = TimeDelta::hours(2);

    pub fn new(clock: Clock) -> Self {
        DownloadPicks {
            picks: Mutex::new(HashMap::new()),
            clock,
        }
    }

    pub fn pin(&self, external_id: &str, copy: Arc<PickedCopy>) {
        if dotnet::is_blank(external_id) {
            return;
        }
        let now = self.clock.now();
        let mut picks = self.picks.lock();
        picks.retain(|_, (_, at)| now - *at < Self::KEEP);
        picks.insert(external_id.to_string(), (copy, now));
    }

    /// The pick for this song from this source, taken so it is used once. A pick for the other
    /// source stays for that source.
    pub fn take(&self, external_id: &str, source: &str) -> Option<Arc<PickedCopy>> {
        if dotnet::is_blank(external_id) {
            return None;
        }
        let now = self.clock.now();
        let mut picks = self.picks.lock();
        let (copy, at) = picks.get(external_id)?;
        if now - *at >= Self::KEEP {
            picks.remove(external_id);
            return None;
        }
        if !dotnet::eq_ignore_case(&copy.source, source) {
            return None;
        }
        picks.remove(external_id).map(|(copy, _)| copy)
    }

    /// Lets go of this pick once the download it was made for has ended, whether or not it used
    /// it ("already in your library" ends a download without a search). A newer pick for the same
    /// song stays, so a later download never takes a copy from a run long over.
    pub fn forget(&self, external_id: &str, copy: &Arc<PickedCopy>) -> bool {
        if dotnet::is_blank(external_id) {
            return false;
        }
        let mut picks = self.picks.lock();
        match picks.get(external_id) {
            Some((pinned, _)) if Arc::ptr_eq(pinned, copy) => {
                picks.remove(external_id);
                true
            }
            _ => false,
        }
    }

    /// Whether a pick is still waiting for this song.
    pub fn has(&self, external_id: &str) -> bool {
        self.picks.lock().contains_key(external_id)
    }
}
