//! The largest cover of one album, for the cover upgrade, behind `IAlbumCoverFinder` so the
//! upgrade can be tested without the network.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use octo_core::common::{SongIdentity, SongMatchOptions, dotnet};
use octo_core::metadata::apple_as_deezer;
use octo_core::metadata::deezer_metadata_service::AlbumHit;
use octo_media::cover::cover_image;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::cover_art_archive_lookup::CoverArtArchiveLookup;
use super::itunes_cover_art_lookup::{self, ITunesCoverArtLookup};
use crate::services::framework::HttpAnswer;
use crate::services::metadata::{AskFor, DeezerMetadataService};

/// What is known about an album from its files' tags.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AlbumCoverQuery {
    pub artist: String,
    pub album: Option<String>,
    pub title: Option<String>,
    pub music_brainz_release_id: Option<String>,
    pub music_brainz_release_group_id: Option<String>,
    pub barcode: Option<String>,
}

impl AlbumCoverQuery {
    /// An artist, album and title; no MusicBrainz ids or barcode.
    pub fn new(artist: impl Into<String>, album: Option<String>, title: Option<String>) -> Self {
        Self {
            artist: artist.into(),
            album,
            title,
            ..Self::default()
        }
    }
}

/// A cover found for an album, with its size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundCover {
    pub bytes: Vec<u8>,
    pub source: String,
    pub side: i32,
}

impl FoundCover {
    pub fn new(bytes: Vec<u8>, source: impl Into<String>, side: i32) -> Self {
        Self {
            bytes,
            source: source.into(),
            side,
        }
    }
}

/// Pictures someone turned down for an album: by their SHA-256, and by how they look, so a copy
/// of the same artwork at another size or compression is turned down too.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RejectedCovers {
    pub hashes: HashSet<[u8; 32]>,
    pub looks: Vec<u64>,
}

impl RejectedCovers {
    pub fn sha256(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }

    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty() && self.looks.is_empty()
    }

    /// Whether this picture is one of them, or looks like one.
    pub fn rejects(&self, bytes: &[u8]) -> bool {
        if self.hashes.contains(&Self::sha256(bytes)) {
            return true;
        }
        !self.looks.is_empty()
            && cover_image::looks_hash(Some(bytes)).is_some_and(|looks| {
                self.looks
                    .iter()
                    .any(|rejected| cover_image::look_alike(*rejected, looks))
            })
    }
}

/// What a priming run says it is doing, in words.
pub type PrimeStatus<'a> = &'a (dyn Fn(String) + Send + Sync);

/// Finds the largest cover of one album. A trait so the upgrade can be tested without the
/// network. Canceling is dropping the future.
#[async_trait]
pub trait IAlbumCoverFinder: Send + Sync {
    async fn find(&self, query: &AlbumCoverQuery) -> Option<FoundCover>;

    /// The same choice as [`IAlbumCoverFinder::find`] without downloading the largest
    /// pictures: the found cover's size is right, and its bytes may be a small copy, enough for
    /// a tile and for `cover_image::looks_hash`. For a preview.
    async fn preview(&self, query: &AlbumCoverQuery) -> Option<FoundCover> {
        self.find(query).await
    }

    /// The largest cover that is none of the `rejected` ones. A finder that knows several
    /// answers passes over the rejected ones to the next; this one only drops its answer.
    async fn find_excluding(&self, query: &AlbumCoverQuery, rejected: &RejectedCovers) -> Option<FoundCover> {
        self.find(query)
            .await
            .filter(|found| !rejected.rejects(&found.bytes))
    }

    /// Gets ready for many albums at once, before [`IAlbumCoverFinder::find`] is asked about
    /// each: what can be matched in bulk is matched here, and what it is doing is reported in
    /// words. Optional; an album it did not reach is found the slow way.
    async fn prime(
        &self,
        _albums: &[AlbumCoverQuery],
        _status: Option<PrimeStatus<'_>>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

/// The largest cover of one album, for the cover upgrade. Every source must name the same
/// release, because what this finds is written into the owner's files: Apple's master on a
/// strict artist and album match, the Cover Art Archive by the MusicBrainz release the files
/// already name, and the catalog's album by the same strict match. All three are asked at once
/// and the largest wins.
///
/// Apple answers about 20 searches a minute, which made a thousand albums an hour. So a run over
/// many albums is primed: each album's barcode (from its own tags when they carry one, else from
/// Deezer, which answers far more), and every 20 barcodes found go to Apple as one lookup. It is
/// a pipeline: barcodes, Apple's answers and each album's covers all move at once, and an album
/// waits only for its own batch, never for the whole run.
pub struct AlbumCoverFinder {
    itunes: Arc<ITunesCoverArtLookup>,
    archive: Arc<CoverArtArchiveLookup>,
    deezer: Arc<DeezerMetadataService>,
    /// The default client, for the catalog's cover.
    http: reqwest::Client,
    /// See [`Self::APPLE_BATCH_IDLE`]; the tests shorten it.
    apple_batch_idle: Duration,

    /// The catalog's album found while priming, so its cover needs no second search.
    catalog: Mutex<HashMap<String, Option<AlbumHit>>>,

    /// Albums being primed, released once Apple has answered for their batch (or they had no
    /// barcode): `find` waits for this and no longer. A canceled token is a released album.
    ready: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

/// Releases every album still waiting, however a priming run ended, including when its future
/// is dropped.
struct ReleaseAll(Arc<Mutex<HashMap<String, CancellationToken>>>);

impl Drop for ReleaseAll {
    fn drop(&mut self) {
        // However it ended, nobody waits on an album this will never answer for.
        for done in self.0.lock().values() {
            done.cancel();
        }
    }
}

impl AlbumCoverFinder {
    const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15);
    const PRIME_PARALLELISM: usize = 4;

    /// Barcodes sent to Apple as soon as this many are waiting (more go along when they piled
    /// up during Apple's pacing, up to [`itunes_cover_art_lookup::UPC_BATCH`]).
    pub const APPLE_BATCH_MIN: usize = 20;

    /// A batch smaller than [`Self::APPLE_BATCH_MIN`] goes anyway after this long without a
    /// new barcode, so the last albums are not kept waiting.
    pub const APPLE_BATCH_IDLE: Duration = Duration::from_secs(2);

    /// The longest an album waits for its batch before it is searched on its own.
    const READY_TIMEOUT: Duration = Duration::from_secs(15 * 60);

    pub fn new(
        itunes: Arc<ITunesCoverArtLookup>,
        archive: Arc<CoverArtArchiveLookup>,
        deezer: Arc<DeezerMetadataService>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            itunes,
            archive,
            deezer,
            http,
            apple_batch_idle: Self::APPLE_BATCH_IDLE,
            catalog: Mutex::new(HashMap::new()),
            ready: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Another wait before a short batch goes (tests shorten it).
    pub fn with_apple_batch_idle(mut self, idle: Duration) -> Self {
        self.apple_batch_idle = idle;
        self
    }

    fn key_of(artist: &str, album: &str) -> String {
        SongIdentity::match_key(artist, album)
    }

    fn release(&self, artist: &str, album: &str) {
        if let Some(done) = self.ready.lock().get(&Self::key_of(artist, album)) {
            done.cancel();
        }
    }

    async fn choose(
        &self,
        query: &AlbumCoverQuery,
        probe_only: bool,
        rejected: Option<&RejectedCovers>,
    ) -> Option<FoundCover> {
        let album = query.album.as_deref().filter(|album| !dotnet::is_blank(album));
        // An album being primed waits for its own batch at Apple, so its master is matched in
        // bulk rather than searched; the other sources start at once.
        let ready = album.and_then(|album| {
            self.ready
                .lock()
                .get(&Self::key_of(&query.artist, album))
                .cloned()
        });
        // A preview learns the master's size from its first bytes and takes Apple's small copy;
        // only a replace downloads the master itself.
        let itunes = async {
            if let Some(ready) = ready {
                let _ = tokio::time::timeout(Self::READY_TIMEOUT, ready.cancelled()).await;
            }
            if !probe_only {
                let master = self
                    .itunes
                    .try_fetch_album_master(
                        Some(&query.artist),
                        query.album.as_deref(),
                        query.title.as_deref(),
                    )
                    .await;
                return (master.map(|bytes| bytes.to_vec()), None);
            }
            match self
                .itunes
                .try_probe_album_master(
                    Some(&query.artist),
                    query.album.as_deref(),
                    query.title.as_deref(),
                )
                .await
            {
                Some((side, thumb)) => (Some(thumb.to_vec()), Some(side)),
                None => (None, None),
            }
        };
        let archive = async {
            if query.music_brainz_release_id.as_deref().is_none_or(str::is_empty)
                && query
                    .music_brainz_release_group_id
                    .as_deref()
                    .is_none_or(str::is_empty)
            {
                return None;
            }
            self.archive
                .try_fetch(
                    query.music_brainz_release_id.as_deref(),
                    query.music_brainz_release_group_id.as_deref(),
                )
                .await
                .map(|bytes| bytes.to_vec())
        };
        let catalog = async {
            match album {
                Some(album) => self.catalog_cover(&query.artist, album).await,
                None => None,
            }
        };
        let ((itunes, probed_side), archive, catalog) = tokio::join!(itunes, archive, catalog);
        let (catalog, catalog_source) = match catalog {
            Some((bytes, source)) => (Some(bytes), source),
            None => (None, "Deezer"),
        };

        let mut best: Option<FoundCover> = None;
        for (bytes, source) in [
            (itunes, "iTunes"),
            (archive, "Cover Art Archive"),
            (catalog, catalog_source),
        ] {
            let Some(bytes) = bytes else {
                continue;
            };
            if !cover_image::is_usable(Some(&bytes), true)
                || rejected.is_some_and(|rejected| rejected.rejects(&bytes))
            {
                continue;
            }
            let Some((width, height)) = cover_image::measure(&bytes) else {
                continue;
            };
            let side = match probed_side {
                Some(probed) if source == "iTunes" => probed,
                _ => width.min(height),
            };
            let side = i32::try_from(side).unwrap_or(i32::MAX);
            if best.as_ref().is_none_or(|best| side > best.side) {
                best = Some(FoundCover::new(bytes, source, side));
            }
        }
        best
    }

    async fn catalog_hit(&self, artist: &str, album: &str) -> Option<AlbumHit> {
        let key = Self::key_of(artist, album);
        if let Some(known) = self.catalog.lock().get(&key) {
            return known.clone();
        }
        let options = SongMatchOptions {
            length_tolerance_seconds: None,
            ..SongMatchOptions::default()
        };
        // A cover's question: Apple, standing in, paces it as a cover rather than a page.
        let hits = self
            .deezer
            .search_albums_for(&format!("{artist} {album}"), 10, true, AskFor::cover(false))
            .await;
        // The album by its own artist, never another artist's record of a near name.
        let hit = hits.into_iter().find(|hit| {
            SongIdentity::same_text(album, artist, &hit.title, &hit.artist, Some(&options)).is_same()
                && octo_core::common::cover_names::release_fits(album, &[artist], &hit.title, &hit.artist)
        });
        let mut catalog = self.catalog.lock();
        if catalog.len() > 5000 {
            catalog.clear();
        }
        catalog.insert(key, hit.clone());
        hit
    }

    /// The catalog's cover of an album, with the name of the catalog that answered: an id above
    /// Deezer's range is Apple's.
    async fn catalog_cover(&self, artist: &str, album: &str) -> Option<(Vec<u8>, &'static str)> {
        let hit = self.catalog_hit(artist, album).await?;
        let source = apple_as_deezer::catalog_name(&hit.deezer_id);
        let url = hit.cover_url.filter(|url| !url.is_empty())?;
        let attempt = async {
            let response = self.http.get(&url).timeout(Self::DOWNLOAD_TIMEOUT).send().await?;
            anyhow::Ok(HttpAnswer::read(response).await?)
        };
        match attempt.await {
            Ok(answer) if answer.is_success() => Some((answer.body.to_vec(), source)),
            Ok(_) => None,
            Err(failure) => {
                debug!("catalog cover failed for {artist} - {album}: {failure}");
                None
            }
        }
    }
}

#[async_trait]
impl IAlbumCoverFinder for AlbumCoverFinder {
    async fn find(&self, query: &AlbumCoverQuery) -> Option<FoundCover> {
        self.choose(query, false, None).await
    }

    async fn preview(&self, query: &AlbumCoverQuery) -> Option<FoundCover> {
        self.choose(query, true, None).await
    }

    async fn find_excluding(&self, query: &AlbumCoverQuery, rejected: &RejectedCovers) -> Option<FoundCover> {
        self.choose(query, false, Some(rejected)).await
    }

    async fn prime(&self, albums: &[AlbumCoverQuery], status: Option<PrimeStatus<'_>>) -> anyhow::Result<()> {
        let mut keys = HashSet::new();
        let named: Vec<(&str, &str, Option<&str>)> = albums
            .iter()
            .filter_map(|query| {
                let album = query.album.as_deref().filter(|album| !dotnet::is_blank(album))?;
                (!dotnet::is_blank(&query.artist)).then_some((
                    query.artist.as_str(),
                    album,
                    query.barcode.as_deref(),
                ))
            })
            .filter(|(artist, album, _)| keys.insert(Self::key_of(artist, album)))
            .collect();
        {
            let mut ready = self.ready.lock();
            ready.clear();
            for (artist, album, _) in &named {
                ready.insert(Self::key_of(artist, album), CancellationToken::new());
            }
        }
        if named.is_empty() {
            return Ok(());
        }
        let _release = ReleaseAll(self.ready.clone());

        let (looked, coded, from_tags, matched) = (
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
        );
        let report = || {
            if let Some(status) = status {
                status(format!(
                    "barcodes {} of {} · Apple matched {}",
                    n0(looked.load(Ordering::SeqCst)),
                    n0(named.len()),
                    n0(matched.load(Ordering::SeqCst))
                ));
            }
        };

        // Barcodes: the album's own tag when it has one, else the catalog's.
        let (sender, mut barcodes) = mpsc::unbounded_channel::<(String, String, String)>();
        let producer = {
            let sender = sender;
            let (named, looked, coded, from_tags, report) = (&named, &looked, &coded, &from_tags, &report);
            async move {
                futures::stream::iter(named.iter())
                    .for_each_concurrent(Self::PRIME_PARALLELISM, |&(artist, album, barcode)| {
                        let sender = sender.clone();
                        async move {
                            let mut upc = barcode.filter(|code| !dotnet::is_blank(code)).map(str::to_string);
                            if upc.is_some() {
                                from_tags.fetch_add(1, Ordering::SeqCst);
                            } else if let Some(hit) = self.catalog_hit(artist, album).await {
                                upc = self.deezer.get_album_upc(&hit.deezer_id).await;
                            }
                            match upc.filter(|code| !dotnet::is_blank(code)) {
                                Some(upc) => {
                                    coded.fetch_add(1, Ordering::SeqCst);
                                    let _ = sender.send((
                                        artist.to_string(),
                                        album.to_string(),
                                        upc.trim().to_string(),
                                    ));
                                }
                                None => self.release(artist, album),
                            }
                            if (looked.fetch_add(1, Ordering::SeqCst) + 1) % 10 == 0 {
                                report();
                            }
                        }
                    })
                    .await;
                // The writer completes here, so Apple's side sees the end.
                drop(sender);
            }
        };

        // Apple: a batch as soon as enough are waiting, or the producer has gone quiet.
        let consumer = async {
            let mut pending: Vec<(String, String, String)> = Vec::new();
            let mut finished = false;
            loop {
                while pending.len() < itunes_cover_art_lookup::UPC_BATCH {
                    match barcodes.try_recv() {
                        Ok(item) => pending.push(item),
                        Err(TryRecvError::Disconnected) => {
                            finished = true;
                            break;
                        }
                        Err(TryRecvError::Empty) => break,
                    }
                }
                let mut idle = false;
                if pending.len() < Self::APPLE_BATCH_MIN && !finished {
                    match tokio::time::timeout(self.apple_batch_idle, barcodes.recv()).await {
                        Ok(Some(item)) => {
                            pending.push(item);
                            continue;
                        }
                        Ok(None) => {
                            finished = true;
                            continue;
                        }
                        Err(_) => idle = true,
                    }
                }
                if pending.is_empty() {
                    if finished {
                        break;
                    }
                    continue;
                }
                if pending.len() >= Self::APPLE_BATCH_MIN || finished || idle {
                    let send = std::mem::take(&mut pending);
                    matched.fetch_add(self.itunes.prime_by_barcode(&send, None).await, Ordering::SeqCst);
                    for (artist, album, _) in &send {
                        self.release(artist, album);
                    }
                    report();
                }
            }
        };
        tokio::join!(producer, consumer);
        info!(
            "Cover upgrade: {} of {} album(s) had a barcode ({} from their own tags), Apple matched {} in bulk",
            coded.load(Ordering::SeqCst),
            named.len(),
            from_tags.load(Ordering::SeqCst),
            matched.load(Ordering::SeqCst)
        );
        Ok(())
    }
}

/// `{n:N0}` in the invariant culture: thousands separated by commas.
fn n0(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
#[path = "album_cover_finder_tests.rs"]
mod tests;
