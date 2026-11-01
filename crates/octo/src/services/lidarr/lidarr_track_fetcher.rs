//! One song fetched through Lidarr for a replacement. Lidarr fetches whole albums, so a
//! replacement borrows one under the rules a heart's pick follows: Lidarr only for an artist with
//! no music in the user's Lidarr, only an album that is unmonitored with no queue item, monitoring
//! never turned on, and the search pinned to a release that carries the recordings the request
//! names (for a Better quality, the original's own, when that is known). Everything after the search is the heart service's own follow
//! (`lidarr_heart_acquisition_service`): Octo's grabs by their search windows, each imported file
//! Octo's only by its own row, a file of the user's making Octo step aside. The song's own file of
//! Octo's import is handed to the replacement; every other file of Octo's is held out of the
//! library and deleted, and the album is put back once Lidarr is done with Octo's downloads, the
//! caller's wait or not. The request, the track match and the lossless test are
//! `octo_core::lidarr::lidarr_track_fetcher`.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use futures::future::BoxFuture;
use octo_core::common::dotnet;
use octo_core::lidarr::lidarr_track_fetcher::{is_lossless, match_track};
use octo_core::lidarr::{LidarrAlbumCandidate, LidarrError, LidarrImportedTrack, LidarrTrackRequest};
use octo_core::settings::{LidarrSettings, SettingsStore};
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::lidarr_heart_acquisition_service::{same_path, translate_imported_path};
use super::{LidarrAlbumClaims, LidarrClient};
use crate::services::fingerprint::download_verification_service::LabelSource;
use crate::services::library::UpgradeSources;
use crate::services::subsonic::NavidromeIdentityService;

/// A song Lidarr brought for a replacement: the copy, and where its own labels are read from
/// (Lidarr's own matching is never one).
#[derive(Debug, Clone, PartialEq)]
pub struct LidarrFetched {
    pub path: String,
    pub labels: LabelSource,
}

#[async_trait]
pub trait ILidarrTrackFetcher: Send + Sync {
    /// Fetches one song through Lidarr and moves it into `destination_directory`, answering the
    /// copy. [`LidarrError::FileNotFound`] when Lidarr has no album for it or brought no good
    /// enough copy in time; [`LidarrError::InvalidOperation`] when Lidarr is not used for it.
    async fn fetch(
        &self,
        request: &LidarrTrackRequest,
        destination_directory: &str,
        ct: &CancellationToken,
    ) -> Result<LidarrFetched, LidarrError>;

    /// The recordings the library file a request replaces is: what its own sound reads clear as,
    /// else its recording tag; none without one. Only a Better quality asks: a Wrong song or a
    /// Wrong version replaces a file that is not the recording wanted.
    async fn original_recordings(&self, _request: &LidarrTrackRequest) -> Vec<String> {
        Vec::new()
    }
}

/// The heart service's side of a replacement's fetch: the pick under Lidarr's rules and its
/// follow.
#[async_trait]
pub trait LidarrReplacements: Send + Sync {
    /// Fetches the song from `album` as a pick does, its search followed until every download of
    /// Octo's is accounted for, and answers the song's own file of Octo's import, moved into
    /// `destination`, once it is in. The answer waits up to Lidarr's import timeout; the follow
    /// goes on after.
    async fn fetch_from(
        &self,
        album: LidarrAlbumCandidate,
        request: &LidarrTrackRequest,
        destination: &str,
        ct: &CancellationToken,
    ) -> Result<LidarrFetched, LidarrError>;

    /// The walk's first pick for the request's recordings: the release group the walk asks Lidarr
    /// for first.
    async fn first_pick(&self, request: &LidarrTrackRequest) -> Option<String>;
}

/// The recordings a library file's own sound reads clear as (its path and length), or none.
pub type OriginalSound = Arc<dyn Fn(String, Option<i32>) -> BoxFuture<'static, Vec<String>> + Send + Sync>;

/// `ManagedText`: why a song Lidarr manages is left to it.
pub const MANAGED_TEXT: &str = "Lidarr manages this file itself, so it was left to Lidarr: a quality profile that wants lossless upgrades it there.";

/// One song through Lidarr, for a replacement.
#[derive(Clone)]
pub struct LidarrTrackFetcher {
    inner: Arc<Inner>,
}

struct Inner {
    client: Arc<LidarrClient>,
    /// The Lidarr settings and `Library:DownloadPath`, read at use.
    settings: Arc<SettingsStore>,
    nav_identity: NavidromeIdentityService,
    claims: Arc<LidarrAlbumClaims>,
    /// The pick and its follow, set once the heart service is built.
    replacements: OnceLock<Arc<dyn LidarrReplacements>>,
    /// What an original's own sound reads as, set once what reads it is built.
    original_sound: OnceLock<OriginalSound>,
}

impl LidarrTrackFetcher {
    pub fn new(
        client: Arc<LidarrClient>,
        settings: Arc<SettingsStore>,
        nav_identity: NavidromeIdentityService,
        claims: Arc<LidarrAlbumClaims>,
    ) -> Self {
        LidarrTrackFetcher {
            inner: Arc::new(Inner {
                client,
                settings,
                nav_identity,
                claims,
                replacements: OnceLock::new(),
                original_sound: OnceLock::new(),
            }),
        }
    }

    /// Who runs a fetch's pick and its follow: the heart service.
    pub fn use_replacements(&self, replacements: Arc<dyn LidarrReplacements>) {
        let _ = self.inner.replacements.set(replacements);
    }

    /// Who reads what a library file's own sound is.
    pub fn use_original_sound(&self, sound: OriginalSound) {
        let _ = self.inner.original_sound.set(sound);
    }

    /// The album a song is on, found as a fetch finds it: from the original's own release group
    /// tag, then by its album name, then the walk's first pick for the recordings the request
    /// names. Find songs lists that album's releases.
    pub async fn find_album(
        &self,
        request: &LidarrTrackRequest,
        ct: &CancellationToken,
    ) -> Result<Option<LidarrAlbumCandidate>, LidarrError> {
        self.inner.find_album(request, ct).await
    }
}

#[async_trait]
impl ILidarrTrackFetcher for LidarrTrackFetcher {
    async fn fetch(
        &self,
        request: &LidarrTrackRequest,
        destination_directory: &str,
        ct: &CancellationToken,
    ) -> Result<LidarrFetched, LidarrError> {
        // A task of its own, so the album's claim is always let go, even for a caller that stops
        // waiting.
        let inner = self.inner.clone();
        let request = request.clone();
        let destination = destination_directory.to_string();
        let ct = ct.clone();
        tokio::spawn(async move { inner.fetch(&request, &destination, &ct).await })
            .await
            .unwrap_or_else(|e| Err(LidarrError::InvalidOperation(e.to_string())))
    }

    async fn original_recordings(&self, request: &LidarrTrackRequest) -> Vec<String> {
        self.inner.original_recordings(request).await
    }
}

/// `await` on a call made with the caller's token: the token wins when it fires first.
async fn cancellable<T>(
    ct: &CancellationToken,
    work: impl Future<Output = Result<T, LidarrError>>,
) -> Result<T, LidarrError> {
    tokio::select! {
        biased;
        _ = ct.cancelled() => Err(LidarrError::Canceled("A task was canceled.".into())),
        result = work => result,
    }
}

impl Inner {
    async fn fetch(
        &self,
        request: &LidarrTrackRequest,
        destination_directory: &str,
        ct: &CancellationToken,
    ) -> Result<LidarrFetched, LidarrError> {
        let current = self.settings.current().lidarr.clone();
        if !UpgradeSources::lidarr_set_up(&current) {
            return Err(LidarrError::InvalidOperation(
                "Lidarr is not set up: it needs an address, a key, a root folder and both profiles.".into(),
            ));
        }
        let Some(replacements) = self.replacements.get().cloned() else {
            return Err(LidarrError::InvalidOperation(
                "Lidarr is not available on this server.".into(),
            ));
        };
        // The walk and the pin aim only at the recordings the request names: what the original
        // is counts only for a Better quality, whose caller names it.
        let candidate = self.find_album(request, ct).await?.ok_or_else(|| {
            LidarrError::FileNotFound(format!(
                "Lidarr knows no album with '{} - {}' on it.",
                request.artist, request.title
            ))
        })?;
        let key = candidate.foreign_album_id.clone();
        if self.claims.heart_busy(&key) {
            return Err(LidarrError::InvalidOperation(
                "Lidarr is fetching this album for a heart right now; try again once it lands.".into(),
            ));
        }

        self.claims.upgrade_started(&key);
        let result = match self.already_there(&candidate, request, &current, ct).await {
            Ok(()) => {
                replacements
                    .fetch_from(candidate, request, destination_directory, ct)
                    .await
            }
            Err(e) => Err(e),
        };
        self.claims.upgrade_ended(&key);
        result
    }

    /// Refuses a song whose copy Lidarr already has in the library: one Lidarr manages itself
    /// (the very file being replaced), or a lossless one.
    async fn already_there(
        &self,
        candidate: &LidarrAlbumCandidate,
        request: &LidarrTrackRequest,
        current: &LidarrSettings,
        ct: &CancellationToken,
    ) -> Result<(), LidarrError> {
        let Some(existing) = cancellable(ct, self.client.find_album(&candidate.foreign_album_id)).await?
        else {
            return Ok(());
        };
        let tracks = cancellable(ct, self.client.get_album_tracks(existing.id)).await?;
        let octo_root = self.nav_identity.effective_download_path(
            &self
                .settings
                .raw("Library:DownloadPath")
                .unwrap_or_else(|| "/music".to_string()),
        );
        if let Some(had) = match_track(tracks.iter().filter(|t| t.has_file), request)
            && let Some(had_path) = visible(had, current, &octo_root)
        {
            if let Some(original) = &request.original_path
                && same_path(&had_path, original)
            {
                return Err(LidarrError::InvalidOperation(MANAGED_TEXT.into()));
            }
            if is_lossless(had) {
                return Err(LidarrError::InvalidOperation(format!(
                    "Lidarr already has a lossless copy of this song in the library, at {had_path}; the lossy one is a duplicate of it."
                )));
            }
        }
        Ok(())
    }

    /// The recordings the original is: what its own sound reads clear as, else its recording tag.
    async fn original_recordings(&self, request: &LidarrTrackRequest) -> Vec<String> {
        let Some(original) = request
            .original_path
            .clone()
            .filter(|path| !path.is_empty() && Path::new(path).is_file())
        else {
            return Vec::new();
        };
        if let Some(sound) = self.original_sound.get() {
            let heard = sound(original.clone(), request.duration_seconds).await;
            if !heard.is_empty() {
                return heard;
            }
        }
        tag_of(Some(&original), lofty::tag::ItemKey::MusicBrainzRecordingId)
            .await
            .into_iter()
            .collect()
    }

    /// The album: from the original's own release group tag, then by its album name, then the
    /// walk's first pick for the recordings the request names.
    async fn find_album(
        &self,
        request: &LidarrTrackRequest,
        ct: &CancellationToken,
    ) -> Result<Option<LidarrAlbumCandidate>, LidarrError> {
        if let Some(tagged) = tag_of(
            request.original_path.as_deref(),
            lofty::tag::ItemKey::MusicBrainzReleaseGroupId,
        )
        .await
            && let Some(by_tag) = cancellable(ct, self.client.resolve_album_by_foreign_id(&tagged)).await?
        {
            return Ok(Some(by_tag));
        }
        if let Some(album) = request.album.as_deref().filter(|a| !dotnet::is_blank(a)) {
            match cancellable(ct, self.client.resolve_album(&request.artist, album, None)).await {
                Ok(found) => return Ok(Some(found)),
                // No single match by name.
                Err(LidarrError::InvalidOperation(_)) => {}
                Err(e) => return Err(e),
            }
        }
        if let Some(replacements) = self.replacements.get()
            && let Some(pick) = replacements.first_pick(request).await
        {
            info!(
                "The walk for the recording of '{} - {}' picks release group {pick}",
                request.artist, request.title
            );
            return cancellable(ct, self.client.resolve_album_by_foreign_id(&pick)).await;
        }
        Ok(None)
    }
}

/// One MusicBrainz id in the file's tags (the first tag that has one), or None when there is no
/// file, no such tag, or it does not read.
async fn tag_of(path: Option<&str>, key: lofty::tag::ItemKey) -> Option<String> {
    let path = path.filter(|p| !p.is_empty())?.to_string();
    if !Path::new(&path).is_file() {
        return None;
    }
    tokio::task::spawn_blocking(move || {
        use lofty::file::TaggedFileExt;
        let file = lofty::read_from_path(&path).ok()?;
        file.tags()
            .iter()
            .find_map(|tag| tag.get_string(key).filter(|id| !id.is_empty()))
            .map(str::to_string)
    })
    .await
    .ok()
    .flatten()
}

/// Where Octo sees the track's file, or None when it cannot: Lidarr's path under its root folder
/// maps onto Octo's library, and a path Octo shares as is also counts.
fn visible(track: &LidarrImportedTrack, current: &LidarrSettings, octo_root: &str) -> Option<String> {
    let path = track.path.as_deref().filter(|p| !dotnet::is_blank(p))?;
    // Under another root folder (an artist Lidarr already had keeps its own) the translation
    // throws, and the path as Lidarr gave it is tried.
    if let Ok(translated) = translate_imported_path(path, current.root_folder_path.as_deref(), octo_root)
        && Path::new(&translated).is_file()
    {
        return Some(translated);
    }
    Path::new(path).is_file().then(|| path.to_string())
}
