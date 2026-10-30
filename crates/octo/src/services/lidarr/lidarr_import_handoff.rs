//! The handoff of files a Lidarr heart imported to the download pipeline.

use std::collections::HashMap;

use parking_lot::Mutex;

use crate::services::fingerprint::download_verification_service::LabelSource;

/// One file a Lidarr heart brought in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LidarrImport {
    /// Where Lidarr put the file, as Octo sees it, or where Octo holds it for its album.
    pub path: String,
    /// No notice for this one: the album's other songs, or an album heart, which gets one notice
    /// for the whole album instead.
    pub quiet: bool,
    /// Where the file's own labels are read from: Lidarr's import is never one, so only the
    /// download as delivered, or the file itself when Lidarr writes no tags.
    pub labels: LabelSource,
    /// Where a file the checks refuse goes back to instead of being deleted: the album's hold
    /// folder, since a hearted song's refused candidate is still a track of the album.
    pub refused_to: Option<String>,
}

/// The files a Lidarr heart brought in, waiting for the download pipeline to take them by the
/// song's external id. Lidarr is then only how the file was found: the pipeline checks it with
/// AcoustID and the spectrum, identifies, tags, places and records it exactly as it does a
/// Soulseek download.
#[derive(Default)]
pub struct LidarrImportHandoff {
    offers: Mutex<HashMap<String, LidarrImport>>,
}

impl LidarrImportHandoff {
    pub fn new() -> Self {
        Self::default()
    }

    /// Offers a file whose labels are not read, deleted when the checks refuse it.
    pub fn offer(&self, external_id: &str, path: &str, quiet: bool) {
        self.offer_import(
            external_id,
            LidarrImport {
                path: path.to_string(),
                quiet,
                labels: LabelSource::None,
                refused_to: None,
            },
        );
    }

    pub fn offer_import(&self, external_id: &str, import: LidarrImport) {
        self.offers.lock().insert(external_id.to_string(), import);
    }

    /// The file for this song, once; None when none is waiting.
    pub fn take(&self, external_id: &str) -> Option<LidarrImport> {
        self.offers.lock().remove(external_id)
    }

    /// Takes an offer back. True when it was still waiting, so the pipeline never used it.
    pub fn withdraw(&self, external_id: &str) -> bool {
        self.offers.lock().remove(external_id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_offer_is_taken_once_and_by_exact_id() {
        let handoff = LidarrImportHandoff::new();
        handoff.offer("abc", "/music/a.flac", true);
        assert!(handoff.take("ABC").is_none());
        assert_eq!(
            handoff.take("abc"),
            Some(LidarrImport {
                path: "/music/a.flac".into(),
                quiet: true,
                labels: LabelSource::None,
                refused_to: None,
            })
        );
        assert!(handoff.take("abc").is_none());
        handoff.offer("abc", "/music/a.flac", false);
        assert!(handoff.withdraw("abc"));
        assert!(!handoff.withdraw("abc"));
    }
}
