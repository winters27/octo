//! One song fetched through Lidarr for a replacement. The request, the track match and the
//! lossless test are `octo_core::lidarr::lidarr_track_fetcher`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use octo_core::common::dotnet;
use octo_core::lidarr::lidarr_track_fetcher::{is_lossless, match_track};
use octo_core::lidarr::{
    LidarrAlbumCandidate, LidarrError, LidarrImportedTrack, LidarrReleasePick, LidarrSearchStarted,
    LidarrTrackRequest,
};
use octo_core::settings::{LidarrSettings, SettingsStore};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::lidarr_heart_acquisition_service::{same_path, translate_imported_path};
use super::{LidarrAlbumClaims, LidarrClient};
use crate::services::library::UpgradeSources;
use crate::services::subsonic::NavidromeIdentityService;

#[async_trait]
pub trait ILidarrTrackFetcher: Send + Sync {
    /// Fetches one song through Lidarr and copies it into `destination_directory`, returning the
    /// copy's path. [`LidarrError::FileNotFound`] when Lidarr has no album for it, or found no
    /// good enough copy in time.
    async fn fetch(
        &self,
        request: &LidarrTrackRequest,
        destination_directory: &str,
        ct: &CancellationToken,
    ) -> Result<String, LidarrError>;
}

/// A task several callers await, which runs whether or not any of them still is.
type SharedTask<T> = Shared<BoxFuture<'static, Result<T, LidarrError>>>;

fn spawn_shared<T: Clone + Send + Sync + 'static>(
    work: impl Future<Output = Result<T, LidarrError>> + Send + 'static,
) -> SharedTask<T> {
    let handle = tokio::spawn(work);
    async move {
        handle
            .await
            .unwrap_or_else(|e| Err(LidarrError::InvalidOperation(e.to_string())))
    }
    .boxed()
    .shared()
}

/// `ManagedText`: why a song Lidarr manages is left to it.
pub const MANAGED_TEXT: &str = "Lidarr manages this file itself, so it was left to Lidarr: a quality profile that wants lossless upgrades it there.";

/// One song through Lidarr, for a replacement. Lidarr only fetches whole albums, so this borrows
/// the album: it searches it, copies out the one song asked for, then deletes every file that
/// search brought in and puts the album's monitoring back the way it was, so nothing else lands
/// in the library and Lidarr does not fetch the album again. The copy then goes through the same
/// checks and the same swap as a Soulseek download.
///
/// Two songs of one album share one search; the clean up waits for the last of them.
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
    poll: Mutex<Duration>,
    /// By foreign album id, ignoring case; its lock also guards joining and leaving a session.
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

struct Session {
    candidate: LidarrAlbumCandidate,
    state: Mutex<SessionState>,
}

#[derive(Default)]
struct SessionState {
    users: usize,
    before: Option<SharedTask<Arc<Vec<LidarrImportedTrack>>>>,
    search: Option<SharedTask<LidarrSearchStarted>>,
    /// The release the album's search grabbed, when it was a pick.
    grabbed: Option<LidarrReleasePick>,
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
                poll: Mutex::new(Duration::from_secs(10)),
                sessions: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// `Poll`: how often Lidarr is asked whether the song is in.
    pub fn set_poll(&self, poll: Duration) {
        *self.inner.poll.lock() = poll;
    }

    /// The album a song is on, found as a fetch finds it: from the original's own release group
    /// tag, then by its album name. Find songs lists that album's releases.
    pub async fn find_album(
        &self,
        request: &LidarrTrackRequest,
        ct: &CancellationToken,
    ) -> Result<Option<LidarrAlbumCandidate>, LidarrError> {
        self.inner.find_album(request, ct).await
    }

    /// How many fetches share the album's search now, for tests that start two.
    #[cfg(test)]
    pub(crate) fn users_of(&self, foreign_album_id: &str) -> usize {
        self.inner
            .sessions
            .lock()
            .get(&dotnet::ordinal_ignore_case_key(foreign_album_id))
            .map_or(0, |s| s.state.lock().users)
    }
}

#[async_trait]
impl ILidarrTrackFetcher for LidarrTrackFetcher {
    async fn fetch(
        &self,
        request: &LidarrTrackRequest,
        destination_directory: &str,
        ct: &CancellationToken,
    ) -> Result<String, LidarrError> {
        // Run as a task of its own, so the clean up in its finally always runs, even for a
        // caller that stops waiting.
        let inner = self.inner.clone();
        let request = request.clone();
        let destination = destination_directory.to_string();
        let ct = ct.clone();
        tokio::spawn(async move { inner.fetch(&request, &destination, &ct).await })
            .await
            .unwrap_or_else(|e| Err(LidarrError::InvalidOperation(e.to_string())))
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
        self: &Arc<Self>,
        request: &LidarrTrackRequest,
        destination_directory: &str,
        ct: &CancellationToken,
    ) -> Result<String, LidarrError> {
        let current = self.settings.current().lidarr.clone();
        if !UpgradeSources::lidarr_set_up(&current) {
            return Err(LidarrError::InvalidOperation(
                "Lidarr is not set up: it needs an address, a key, a root folder and both profiles.".into(),
            ));
        }

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
        let session_key = dotnet::ordinal_ignore_case_key(&key);
        let session = {
            let mut sessions = self.sessions.lock();
            let session = sessions
                .entry(session_key.clone())
                .or_insert_with(|| {
                    Arc::new(Session {
                        candidate,
                        state: Mutex::new(SessionState::default()),
                    })
                })
                .clone();
            let mut state = session.state.lock();
            state.users += 1;
            if state.before.is_none() {
                let this = self.clone();
                let candidate = session.candidate.clone();
                state.before = Some(spawn_shared(async move { this.snapshot(&candidate).await }));
            }
            drop(state);
            session
        };
        let result = self
            .fetch_from(&session, request, destination_directory, &current, ct)
            .await;

        // finally
        let last = {
            let mut sessions = self.sessions.lock();
            let mut state = session.state.lock();
            state.users -= 1;
            let last = state.users == 0;
            if last
                && sessions
                    .get(&session_key)
                    .is_some_and(|s| Arc::ptr_eq(s, &session))
            {
                sessions.remove(&session_key);
            }
            last
        };
        if last {
            self.clean_up(&session).await;
        }
        self.claims.upgrade_ended(&key);
        result
    }

    async fn fetch_from(
        self: &Arc<Self>,
        session: &Arc<Session>,
        request: &LidarrTrackRequest,
        destination_directory: &str,
        current: &LidarrSettings,
        ct: &CancellationToken,
    ) -> Result<String, LidarrError> {
        let octo_root = self.nav_identity.effective_download_path(
            &self
                .settings
                .raw("Library:DownloadPath")
                .unwrap_or_else(|| "/music".to_string()),
        );
        let before_task = session
            .state
            .lock()
            .before
            .clone()
            .expect("set when the session was joined");
        let before = before_task.await?;
        let before_ids: HashSet<i32> = before
            .iter()
            .filter(|t| t.track_file_id > 0)
            .map(|t| t.track_file_id)
            .collect();

        // What Lidarr had for this song before anything was searched.
        if let Some(had) = match_track(before.iter().filter(|t| t.has_file), request)
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

        let (search, joined, grabbed) = {
            let mut state = session.state.lock();
            let joined = state.search.is_some();
            if !joined {
                state.grabbed = request.release.clone();
                let client = self.client.clone();
                let candidate = session.candidate.clone();
                let grab = request.release.clone();
                // No caller's token: the search is the album's, not this caller's.
                state.search = Some(spawn_shared(async move {
                    client.start_album_search(&candidate, grab.as_ref()).await
                }));
            }
            (
                state.search.clone().expect("just set"),
                joined,
                state.grabbed.clone(),
            )
        };
        if let Some(pick) = &request.release {
            if !joined || grabbed.as_ref().is_some_and(|g| g.guid == pick.guid) {
                if let Some(step) = &request.step {
                    step.tell("Grabbing the release you picked", pick.title.as_deref());
                }
            } else {
                // One album, one search: the song comes from the one already running, not the
                // pick.
                info!(
                    "Lidarr is already fetching '{} - {}', so the release picked for '{}' ({}) is not grabbed",
                    session.candidate.artist,
                    session.candidate.title,
                    request.title,
                    pick.title.as_deref().unwrap_or_default()
                );
                if let Some(step) = &request.step {
                    step.tell(
                        "Lidarr is already fetching this album, so the release you picked was not grabbed",
                        Some("The song comes from the album's search already running"),
                    );
                }
            }
        }
        let started = search.await?;
        info!(
            "Lidarr is searching '{} - {}' for a copy of '{}'",
            session.candidate.artist, session.candidate.title, request.title
        );

        let timeout = Duration::from_secs(current.import_timeout_seconds.max(1) as u64);
        let deadline = Instant::now() + timeout;
        loop {
            if ct.is_cancelled() {
                return Err(LidarrError::operation_canceled());
            }
            let tracks = cancellable(ct, self.client.get_album_tracks(started.album_id)).await?;
            let fresh = tracks.iter().filter(|t| {
                t.has_file
                    && t.track_file_id > 0
                    && !before_ids.contains(&t.track_file_id)
                    && (!request.lossless_only || is_lossless(t))
            });
            if let Some(found) = match_track(fresh, request)
                && let Some(path) = visible(found, current, &octo_root)
            {
                tokio::fs::create_dir_all(destination_directory)
                    .await
                    .map_err(|e| LidarrError::Io(e.to_string()))?;
                let copy = Path::new(destination_directory)
                    .join(format!(
                        "lidarr-{}{}",
                        uuid::Uuid::new_v4().simple(),
                        get_extension(&path)
                    ))
                    .to_string_lossy()
                    .into_owned();
                tokio::fs::copy(&path, &copy)
                    .await
                    .map_err(|e| LidarrError::Io(e.to_string()))?;
                info!(
                    "Lidarr brought '{} - {}' ({}); copied {path} for the replacement",
                    request.artist,
                    request.title,
                    found.quality.as_deref().unwrap_or_else(|| get_extension(&path))
                );
                return Ok(copy);
            }
            if Instant::now() >= deadline {
                break;
            }
            let poll = *self.poll.lock();
            cancellable(ct, async {
                tokio::time::sleep(poll).await;
                Ok(())
            })
            .await?;
        }
        Err(LidarrError::FileNotFound(format!(
            "Lidarr found no {}copy of '{} - {}' within {} minutes.",
            if request.lossless_only { "lossless " } else { "" },
            request.artist,
            request.title,
            (timeout.as_secs() / 60).max(1)
        )))
    }

    /// Takes back what the borrowed search brought in: every file that is new since it started
    /// (the copied songs are in the library under Octo's name by now), and the monitoring Octo
    /// switched on. Best effort; whatever fails is logged.
    async fn clean_up(&self, session: &Session) {
        let (search, before) = {
            let state = session.state.lock();
            (state.search.clone(), state.before.clone())
        };
        let (Some(search), Some(before)) = (search, before) else {
            return;
        };
        let tidied = async {
            let started = search.await?;
            let before: HashSet<i32> = before
                .await?
                .iter()
                .filter(|t| t.track_file_id > 0)
                .map(|t| t.track_file_id)
                .collect();
            let now = self.client.get_album_tracks(started.album_id).await?;
            let mut added: Vec<i32> = Vec::new();
            for t in &now {
                if t.track_file_id > 0
                    && !before.contains(&t.track_file_id)
                    && !added.contains(&t.track_file_id)
                {
                    added.push(t.track_file_id);
                }
            }
            for id in &added {
                if let Err(e) = self.client.delete_track_file(*id).await {
                    warn!("Could not delete Lidarr track file {id}: {e}");
                }
            }
            if !started.was_monitored {
                self.client
                    .set_albums_monitored(&[started.album_id], false)
                    .await?;
            }
            info!(
                "Lidarr album '{} - {}': removed {} files the search brought in{}",
                session.candidate.artist,
                session.candidate.title,
                added.len(),
                if started.was_monitored {
                    ""
                } else {
                    ", and stopped monitoring it again"
                }
            );
            Ok::<_, LidarrError>(())
        }
        .await;
        if let Err(e) = tidied {
            warn!(
                "Could not tidy Lidarr album '{} - {}' after an upgrade: {e}",
                session.candidate.artist, session.candidate.title
            );
        }
    }

    async fn snapshot(
        &self,
        candidate: &LidarrAlbumCandidate,
    ) -> Result<Arc<Vec<LidarrImportedTrack>>, LidarrError> {
        Ok(Arc::new(
            match self.client.find_album(&candidate.foreign_album_id).await? {
                None => Vec::new(),
                Some(existing) => self.client.get_album_tracks(existing.id).await?,
            },
        ))
    }

    /// The album: from the original's own release group tag, then by its album name.
    async fn find_album(
        &self,
        request: &LidarrTrackRequest,
        ct: &CancellationToken,
    ) -> Result<Option<LidarrAlbumCandidate>, LidarrError> {
        if let Some(tagged) = release_group_of(request.original_path.as_deref()).await
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
        Ok(None)
    }
}

/// The MusicBrainz release group id in the file's tags (the first tag that has one), or None
/// when there is no file, no such tag, or it does not read.
async fn release_group_of(path: Option<&str>) -> Option<String> {
    let path = path.filter(|p| !p.is_empty())?.to_string();
    if !Path::new(&path).is_file() {
        return None;
    }
    tokio::task::spawn_blocking(move || {
        use lofty::file::TaggedFileExt;
        use lofty::tag::ItemKey;
        let file = lofty::read_from_path(&path).ok()?;
        file.tags()
            .iter()
            .find_map(|tag| {
                tag.get_string(ItemKey::MusicBrainzReleaseGroupId)
                    .filter(|id| !id.is_empty())
            })
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

/// The extension of the file name: its last dot and what follows, or empty when the name has
/// no dot or ends with one.
fn get_extension(path: &str) -> &str {
    let name = &path[path.rfind('/').map_or(0, |i| i + 1)..];
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => &name[dot..],
        _ => "",
    }
}

#[cfg(test)]
#[path = "lidarr_track_fetcher_tests.rs"]
mod tests;
