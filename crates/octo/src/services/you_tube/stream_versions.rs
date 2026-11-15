//! The YouTube videos an outside song can stream from, for a person choosing which one it plays:
//! one pool per song, listed at once from YouTube Music's songs and videos and the shim's ranked
//! candidates under one time budget, kept a while when every source answered, and the view of it
//! each answer shows. The pool behind every answer served is remembered, so a choice is only ever
//! one of the videos offered.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use indexmap::IndexMap;
use octo_core::common::song_identity::{SongMatchOptions, SongRef};
use octo_core::common::{Clock, SongIdentity};
use octo_core::soulseek::{SongLength, SoulseekRouting};
use parking_lot::Mutex;

use super::you_tube_music_client::{PODCAST_EPISODE, RELEASE, SAME_LENGTH_SECONDS, YtmRow};
use super::you_tube_resolver::{Failure, Lookup, YouTubeHit, YouTubeResolver};
use crate::services::soulseek::ExternalIdRegistry;
use crate::services::soulseek::stream_choices::{StreamChoices, StreamVersionKind};

/// `video_type` of an official music video on YouTube Music.
const MUSIC_VIDEO: &str = "MUSIC_VIDEO_TYPE_OMV";

/// A channel YouTube makes for a release's own audio ends with this.
const TOPIC: &str = " - Topic";

/// How long a listing may take and how much it shows.
#[derive(Debug, Clone)]
pub struct ListCaps {
    /// The whole listing's time; no source waits longer.
    pub budget: Duration,
    /// Each YouTube Music search's own time.
    pub music: Duration,
    /// The shim's search's own time.
    pub youtube: Duration,
    /// Rows each source is asked for.
    pub per_source: i32,
    /// Other versions one answer shows at most.
    pub shown: usize,
    /// How long a pool every source answered is kept.
    pub keep_pool: TimeDelta,
    /// How long the pool behind an answer stays offered.
    pub keep_offered: TimeDelta,
    /// Songs whose pool is kept at once, the least recently read out first.
    pub pooled_songs: usize,
    /// Songs whose offered pool is kept at once, the least recently read out first.
    pub offered_songs: usize,
}

impl Default for ListCaps {
    fn default() -> Self {
        ListCaps {
            budget: Duration::from_secs(10),
            music: Duration::from_secs(8),
            youtube: Duration::from_secs(10),
            per_source: 10,
            shown: 15,
            keep_pool: TimeDelta::minutes(30),
            keep_offered: TimeDelta::minutes(30),
            pooled_songs: 2_000,
            offered_songs: 2_000,
        }
    }
}

/// A source that could not answer a listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionSource {
    YouTubeMusic,
    YouTube,
}

impl VersionSource {
    /// The name in `missed`.
    pub fn wire(self) -> &'static str {
        match self {
            VersionSource::YouTubeMusic => "youtubeMusic",
            VersionSource::YouTube => "youtube",
        }
    }
}

/// One video a song can play from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRow {
    pub video_id: String,
    pub title: String,
    pub channel: Option<String>,
    pub seconds: Option<i32>,
    pub kind: StreamVersionKind,
    /// The same song as the one listed ([`SongIdentity::same`], 5 s apart at most).
    pub same: bool,
}

/// A listing: every video found, in source order, and the sources that could not answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pool {
    pub rows: Vec<VersionRow>,
    pub missed: Vec<VersionSource>,
}

/// What a song is listed by: its artist, its title and its catalog length.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListedSong {
    pub artist: String,
    pub title: String,
    pub seconds: Option<i32>,
}

impl ListedSong {
    pub fn of(routing: &SoulseekRouting) -> Self {
        ListedSong {
            artist: routing.artist.as_deref().unwrap_or_default().trim().to_string(),
            title: routing.title.as_deref().unwrap_or_default().trim().to_string(),
            seconds: StreamChoices::catalog_seconds(routing),
        }
    }
}

/// Where an answer's song plays from for the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaysFrom {
    /// A YouTube video (also while a copy fetched ahead plays: a pick makes it unserved).
    Stream,
    /// The caller's own or the shared library file.
    File,
    /// Every play adds the song to the library (Download on play, Wait for lossless on play).
    Download,
}

impl PlaysFrom {
    pub fn wire(self) -> &'static str {
        match self {
            PlaysFrom::Stream => "stream",
            PlaysFrom::File => "file",
            PlaysFrom::Download => "download",
        }
    }
}

/// Who picked the video playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Picked {
    Person,
    /// A pick from before whose record is gone; it still stands and can be undone.
    Earlier,
    Octo,
}

impl Picked {
    pub fn wire(self) -> &'static str {
        match self {
            Picked::Person => "person",
            Picked::Earlier => "earlier",
            Picked::Octo => "octo",
        }
    }
}

/// The video a song plays from now, with what is known of it and who picked it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayingVersion {
    pub video_id: String,
    pub title: Option<String>,
    pub channel: Option<String>,
    pub seconds: Option<i32>,
    pub kind: Option<StreamVersionKind>,
    pub picked: Picked,
    pub picked_by: Option<String>,
    /// The caller picked it.
    pub mine: bool,
    pub picked_at: Option<DateTime<Utc>>,
}

/// One answer of getStreamVersions or setStreamVersion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionsAnswer {
    /// The id as the app asked by.
    pub id: String,
    pub plays_from: PlaysFrom,
    /// None while no video is known yet.
    pub playing: Option<PlayingVersion>,
    /// The caller's latest heard video, when it is not the one playing.
    pub heard_video_id: Option<String>,
    /// The caller's own heart, album heart or request of the song runs now.
    pub busy: bool,
    pub versions: Vec<VersionRow>,
    pub missed: Vec<VersionSource>,
    /// Every id the song has.
    pub ids: Vec<String>,
}

/// A kind as the wire names it.
pub fn kind_name(kind: &StreamVersionKind) -> &'static str {
    match kind {
        StreamVersionKind::Release => "release",
        StreamVersionKind::MusicVideo => "musicVideo",
        StreamVersionKind::Video => "video",
    }
}

/// The kind of a video the shim found, by its channel: a Topic channel's is a release's own
/// audio, anything else a video.
pub fn candidate_kind(channel: Option<&str>) -> StreamVersionKind {
    if channel.is_some_and(|channel| channel.trim_end().ends_with(TOPIC)) {
        StreamVersionKind::Release
    } else {
        StreamVersionKind::Video
    }
}

/// The kind of Octo's own pick: YouTube Music's row for the song (which names no channel) is a
/// release; a searched video is judged by its channel.
pub fn octo_pick_kind(hit: &YouTubeHit) -> StreamVersionKind {
    match hit.channel.as_deref() {
        None => StreamVersionKind::Release,
        channel => candidate_kind(channel),
    }
}

/// The versions an answer shows: the pool without the video playing and the videos the song
/// refuses, the same song first (otherwise in pool order), at most `shown`.
pub fn view(
    rows: &[VersionRow],
    playing: Option<&str>,
    refused: impl Fn(&str) -> bool,
    shown: usize,
) -> Vec<VersionRow> {
    let mut left: Vec<VersionRow> = rows
        .iter()
        .filter(|row| Some(row.video_id.as_str()) != playing && !refused(&row.video_id))
        .cloned()
        .collect();
    left.sort_by_key(|row| !row.same);
    left.truncate(shown);
    left
}

/// Whether a copy of the song `external_id` fetched ahead, judged against `judged_video`, is
/// another version than the one a person picked: while a person's pick (or a pick from before)
/// stands for the song, only a copy judged against the video it plays from is that version.
pub fn copy_is_another_version(
    registry: &ExternalIdRegistry,
    external_id: &str,
    judged_video: Option<&str>,
) -> bool {
    !registry.ahead_copy_serves(external_id, judged_video)
}

/// The rows of one listing in source order (YouTube Music's songs, the shim's candidates,
/// YouTube Music's videos), one per video: the first row of a video wins, and a later row fills
/// in a channel or a length it lacks. Podcast episodes, rows without a title and lengths no song
/// has are left out. Each row says whether it is the song listed.
pub fn merge(
    song: &ListedSong,
    songs: &[YtmRow],
    found: &[YouTubeHit],
    videos: &[YtmRow],
) -> Vec<VersionRow> {
    let candidates = songs
        .iter()
        .map(|row| from_music(row, true))
        .chain(found.iter().map(from_hit))
        .chain(videos.iter().map(|row| from_music(row, false)))
        .flatten();
    let mut rows: Vec<VersionRow> = Vec::new();
    for row in candidates {
        match rows.iter_mut().find(|known| known.video_id == row.video_id) {
            Some(known) => {
                if known.channel.is_none() {
                    known.channel = row.channel;
                }
                if known.seconds.is_none() {
                    known.seconds = row.seconds;
                }
            }
            None => rows.push(row),
        }
    }
    rows.retain(|row| row.seconds.is_none() || SongLength::sane_video_length(row.seconds).is_some());
    for row in &mut rows {
        row.same = same_song(song, row);
    }
    rows
}

/// A YouTube Music row as a version; None for a podcast episode or a row without an id.
fn from_music(row: &YtmRow, songs: bool) -> Option<VersionRow> {
    if row.video_id.is_empty() || row.video_type.as_deref() == Some(PODCAST_EPISODE) {
        return None;
    }
    let kind = match row.video_type.as_deref() {
        Some(RELEASE) => StreamVersionKind::Release,
        Some(MUSIC_VIDEO) => StreamVersionKind::MusicVideo,
        Some(_) => StreamVersionKind::Video,
        // A songs search lists release tracks.
        None if songs => StreamVersionKind::Release,
        None => StreamVersionKind::Video,
    };
    Some(VersionRow {
        video_id: row.video_id.clone(),
        title: row.title.clone(),
        channel: Some(row.artists.join(", ")).filter(|channel| !channel.is_empty()),
        seconds: row.duration_seconds,
        kind,
        same: false,
    })
}

/// A video the shim found as a version; None without an id or a title.
fn from_hit(hit: &YouTubeHit) -> Option<VersionRow> {
    let title = hit
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())?;
    if hit.video_id.is_empty() {
        return None;
    }
    Some(VersionRow {
        video_id: hit.video_id.clone(),
        title: title.to_string(),
        channel: hit.channel.clone(),
        seconds: hit.duration,
        kind: candidate_kind(hit.channel.as_deref()),
        same: false,
    })
}

/// Whether a row is the song listed: the same title and artist, 5 s apart at most when both
/// lengths are known. A video's "Artist - Title" names both; any other row is its channel's.
fn same_song(song: &ListedSong, row: &VersionRow) -> bool {
    let options = SongMatchOptions {
        length_tolerance_seconds: Some(SAME_LENGTH_SECONDS),
        ..Default::default()
    };
    let wanted =
        SongRef::new(song.title.clone(), song.artist.clone()).with_seconds(song.seconds.map(f64::from));
    let channel = row.channel.as_deref().unwrap_or_default().trim();
    let channel = channel.strip_suffix(TOPIC).unwrap_or(channel);
    let (artist, title) = match row.title.split_once(" - ") {
        Some((artist, title)) if row.kind != StreamVersionKind::Release => {
            (artist.trim().to_string(), title.trim().to_string())
        }
        _ => (channel.to_string(), row.title.clone()),
    };
    let found = SongRef::new(title, artist).with_seconds(row.seconds.map(f64::from));
    SongIdentity::same(&wanted, &found, Some(&options)).is_same()
}

/// A lookup that took longer than `cap` could not answer.
async fn capped<T>(cap: Duration, lookup: impl Future<Output = Lookup<T>>) -> Lookup<T> {
    tokio::time::timeout(cap, lookup)
        .await
        .unwrap_or(Lookup::Failed(Failure::Other))
}

type Kept = IndexMap<String, (DateTime<Utc>, Vec<VersionRow>)>;

/// The rows kept for `key` while younger than `keep`, moved to the end as the most recently read;
/// an older entry goes.
fn read_kept(map: &mut Kept, key: &str, now: DateTime<Utc>, keep: TimeDelta) -> Option<Vec<VersionRow>> {
    let (at, rows) = map.shift_remove(key)?;
    if now - at >= keep {
        return None;
    }
    map.insert(key.to_string(), (at, rows.clone()));
    Some(rows)
}

/// Keeps `rows` for `key` as of `at`, the least recently read out once more than `most` are kept.
fn keep(map: &mut Kept, key: &str, at: DateTime<Utc>, rows: Vec<VersionRow>, most: usize) {
    map.shift_remove(key);
    map.insert(key.to_string(), (at, rows));
    while map.len() > most.max(1) {
        map.shift_remove_index(0);
    }
}

/// Lists the versions of outside songs and remembers what was offered.
pub struct StreamVersions {
    resolver: Arc<YouTubeResolver>,
    clock: Clock,
    caps: ListCaps,
    /// Pools every source answered, by song key, with when each was listed.
    pools: Mutex<Kept>,
    /// The pool behind each answer served, by song key, with when it was served.
    offered: Mutex<Kept>,
}

impl StreamVersions {
    pub fn new(resolver: Arc<YouTubeResolver>, clock: Clock, caps: ListCaps) -> Self {
        StreamVersions {
            resolver,
            clock,
            caps,
            pools: Mutex::new(IndexMap::new()),
            offered: Mutex::new(IndexMap::new()),
        }
    }

    /// The shim client the listings use, which also checks a video and finds Octo's own pick.
    pub fn resolver(&self) -> &Arc<YouTubeResolver> {
        &self.resolver
    }

    pub fn caps(&self) -> &ListCaps {
        &self.caps
    }

    /// The pool of the song `key`: the one kept, unless `fresh` asks for a new listing or none is
    /// kept. A listing every source answered is kept; one with a missed source is not. Either way
    /// it is what the answer offers.
    pub async fn pool(&self, key: &str, song: &ListedSong, fresh: bool) -> Pool {
        if !fresh {
            let now = self.clock.now();
            let kept = read_kept(&mut self.pools.lock(), key, now, self.caps.keep_pool);
            if let Some(rows) = kept {
                self.offer(key, &rows, now);
                return Pool {
                    rows,
                    missed: Vec::new(),
                };
            }
        }
        let pool = self.list(song).await;
        let now = self.clock.now();
        if pool.missed.is_empty() {
            keep(
                &mut self.pools.lock(),
                key,
                now,
                pool.rows.clone(),
                self.caps.pooled_songs,
            );
        }
        self.offer(key, &pool.rows, now);
        pool
    }

    /// The pool the last answer for the song `key` offered, while it is offered.
    pub fn offered(&self, key: &str) -> Option<Vec<VersionRow>> {
        read_kept(
            &mut self.offered.lock(),
            key,
            self.clock.now(),
            self.caps.keep_offered,
        )
    }

    /// The row of `video` in the pool offered for the song `key`.
    pub fn offered_row(&self, key: &str, video: &str) -> Option<VersionRow> {
        self.offered(key)?.into_iter().find(|row| row.video_id == video)
    }

    fn offer(&self, key: &str, rows: &[VersionRow], at: DateTime<Utc>) {
        keep(
            &mut self.offered.lock(),
            key,
            at,
            rows.to_vec(),
            self.caps.offered_songs,
        );
    }

    /// One listing: every source at once, each within its own time and the budget.
    async fn list(&self, song: &ListedSong) -> Pool {
        let query = format!("{} {}", song.artist, song.title);
        let rows = self.caps.per_source;
        let music_cap = self.caps.music.min(self.caps.budget);
        let youtube_cap = self.caps.youtube.min(self.caps.budget);
        let (songs, found, videos) = tokio::join!(
            capped(music_cap, self.resolver.music_list(&query, "songs", rows)),
            capped(youtube_cap, self.resolver.search_list(&query, song.seconds, rows)),
            capped(music_cap, self.resolver.music_list(&query, "videos", rows)),
        );
        let mut missed = Vec::new();
        if matches!(songs, Lookup::Failed(_)) || matches!(videos, Lookup::Failed(_)) {
            missed.push(VersionSource::YouTubeMusic);
        }
        if matches!(found, Lookup::Failed(_)) {
            missed.push(VersionSource::YouTube);
        }
        let rows = merge(
            song,
            &songs.found().unwrap_or_default(),
            &found.found().unwrap_or_default(),
            &videos.found().unwrap_or_default(),
        );
        Pool { rows, missed }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use octo_core::soulseek::RoutingKind;
    use wiremock::matchers::{path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::services::soulseek::stream_choices::StreamChoice;

    const SONGS: &str = r#"{"tracks":[
        {"videoId":"rel","title":"Teardrop","artists":["Massive Attack"],"durationSeconds":330,"videoType":"MUSIC_VIDEO_TYPE_ATV"},
        {"videoId":"pod","title":"Teardrop talk","artists":["A Show"],"durationSeconds":900,"videoType":"MUSIC_VIDEO_TYPE_PODCAST_EPISODE"}
    ]}"#;
    const VIDEOS: &str = r#"{"tracks":[
        {"videoId":"omv","title":"Teardrop (Official Video)","artists":["Massive Attack"],"durationSeconds":331,"videoType":"MUSIC_VIDEO_TYPE_OMV"},
        {"videoId":"rel","title":"Teardrop","artists":["Massive Attack"],"durationSeconds":330,"videoType":"MUSIC_VIDEO_TYPE_ATV"},
        {"videoId":"long","title":"Teardrop 10 hours","artists":["Loops"],"durationSeconds":36000,"videoType":"MUSIC_VIDEO_TYPE_UGC"}
    ]}"#;
    const CANDIDATES: &str = r#"{"candidates":[
        {"video_id":"live","title":"Massive Attack - Teardrop (Live)","duration":402,"channel":"Fan Uploads"},
        {"video_id":"omv","title":"Massive Attack - Teardrop (Official Video)","duration":null,"channel":"MassiveAttackVEVO"},
        {"video_id":"topic","title":"Teardrop","duration":329,"channel":"Massive Attack - Topic"}
    ],"rank":3}"#;

    fn teardrop() -> ListedSong {
        ListedSong {
            artist: "Massive Attack".into(),
            title: "Teardrop".into(),
            seconds: Some(330),
        }
    }

    /// A clock the test moves, and the hand that moves it.
    fn clock() -> (Clock, Arc<Mutex<DateTime<Utc>>>) {
        let now = Arc::new(Mutex::new(Utc.with_ymd_and_hms(2026, 11, 14, 10, 0, 0).unwrap()));
        let read = Arc::clone(&now);
        (Clock::new(move || *read.lock()), now)
    }

    fn caps() -> ListCaps {
        ListCaps {
            budget: Duration::from_secs(3),
            music: Duration::from_secs(3),
            youtube: Duration::from_secs(3),
            ..ListCaps::default()
        }
    }

    /// The shim answering every source; `candidates` is the shim's answer to /candidates.
    async fn shim(candidates: ResponseTemplate) -> MockServer {
        let shim = MockServer::start().await;
        Mock::given(path("/ytm/search"))
            .and(query_param("filter", "songs"))
            .respond_with(ResponseTemplate::new(200).set_body_string(SONGS))
            .mount(&shim)
            .await;
        Mock::given(path("/ytm/search"))
            .and(query_param("filter", "videos"))
            .respond_with(ResponseTemplate::new(200).set_body_string(VIDEOS))
            .mount(&shim)
            .await;
        Mock::given(path("/candidates"))
            .respond_with(candidates)
            .mount(&shim)
            .await;
        shim
    }

    fn versions(shim: &MockServer, clock: Clock, caps: ListCaps) -> StreamVersions {
        StreamVersions::new(
            Arc::new(YouTubeResolver::with_base_url(Some(&shim.uri()))),
            clock,
            caps,
        )
    }

    async fn asked(shim: &MockServer) -> usize {
        shim.received_requests().await.expect("recorded").len()
    }

    /// One row per video in source order, a later row filling the length an earlier one lacked,
    /// kinds by YouTube Music's type or the channel, podcasts and lengths no song has left out.
    #[tokio::test]
    async fn a_pool_merges_the_sources_in_order_and_fills_what_a_row_lacks() {
        let shim = shim(ResponseTemplate::new(200).set_body_string(CANDIDATES)).await;
        let (clock, _) = clock();
        let pool = versions(&shim, clock, caps()).pool("k", &teardrop(), false).await;
        assert!(pool.missed.is_empty());
        let ids: Vec<&str> = pool.rows.iter().map(|r| r.video_id.as_str()).collect();
        assert_eq!(ids, ["rel", "live", "omv", "topic"]);
        let row = |id: &str| {
            pool.rows
                .iter()
                .find(|r| r.video_id == id)
                .expect("a row")
                .clone()
        };
        assert_eq!(row("rel").kind, StreamVersionKind::Release);
        assert_eq!(row("rel").channel.as_deref(), Some("Massive Attack"));
        assert!(row("rel").same);
        assert_eq!(row("live").kind, StreamVersionKind::Video);
        assert!(!row("live").same);
        assert_eq!(
            row("omv").seconds,
            Some(331),
            "filled from YouTube Music's videos"
        );
        assert_eq!(row("omv").channel.as_deref(), Some("MassiveAttackVEVO"));
        assert_eq!(row("topic").kind, StreamVersionKind::Release);
        assert!(row("topic").same);
        let asked_shim: Vec<String> = shim
            .received_requests()
            .await
            .expect("recorded")
            .iter()
            .filter(|r| r.url.path() == "/candidates")
            .map(|r| r.url.query().unwrap_or("").to_string())
            .collect();
        assert!(asked_shim[0].contains("duration=330") && asked_shim[0].contains("limit=10"));
    }

    /// The view leaves out the playing and refused videos, puts the same song first and shows at
    /// most the cap; it is read from the pool each time.
    #[tokio::test]
    async fn the_view_is_the_pool_without_the_playing_and_refused_videos_same_first() {
        let shim = shim(ResponseTemplate::new(200).set_body_string(CANDIDATES)).await;
        let (clock, _) = clock();
        let pool = versions(&shim, clock, caps()).pool("k", &teardrop(), false).await;
        let shown = view(&pool.rows, Some("rel"), |video| video == "omv", 15);
        let ids: Vec<&str> = shown.iter().map(|r| r.video_id.as_str()).collect();
        assert_eq!(ids, ["topic", "live"]);
        assert_eq!(view(&pool.rows, None, |_| false, 2).len(), 2);
        assert_eq!(view(&pool.rows, None, |_| false, 2)[0].video_id, "rel");
        let after_a_pick = view(&pool.rows, Some("live"), |_| false, 15);
        assert!(after_a_pick.iter().all(|r| r.video_id != "live"));
        assert!(after_a_pick.iter().any(|r| r.video_id == "rel"));
    }

    /// A pool every source answered is kept for half an hour; a listing asked fresh asks again.
    #[tokio::test]
    async fn a_whole_pool_is_kept_for_half_an_hour() {
        let shim = shim(ResponseTemplate::new(200).set_body_string(CANDIDATES)).await;
        let (clock, now) = clock();
        let versions = versions(&shim, clock, caps());
        versions.pool("k", &teardrop(), false).await;
        assert_eq!(asked(&shim).await, 3);
        versions.pool("k", &teardrop(), false).await;
        assert_eq!(asked(&shim).await, 3, "kept");
        versions.pool("k", &teardrop(), true).await;
        assert_eq!(asked(&shim).await, 6, "fresh asks again");
        *now.lock() += TimeDelta::minutes(31);
        versions.pool("k", &teardrop(), false).await;
        assert_eq!(asked(&shim).await, 9, "too old");
    }

    /// A failed search is missed and its pool is not kept.
    #[tokio::test]
    async fn a_missed_source_is_named_and_its_pool_not_kept() {
        let shim = shim(ResponseTemplate::new(502).set_body_string(r#"{"error":"search_failed"}"#)).await;
        let (clock, _) = clock();
        let versions = versions(&shim, clock, caps());
        let pool = versions.pool("k", &teardrop(), false).await;
        assert_eq!(pool.missed, [VersionSource::YouTube]);
        assert_eq!(pool.rows.len(), 2, "YouTube Music's rows still listed");
        versions.pool("k", &teardrop(), false).await;
        assert_eq!(asked(&shim).await, 6, "asked again");
    }

    /// A source slower than its own cap is missed, and the listing does not wait for it.
    #[tokio::test]
    async fn a_source_over_its_cap_is_missed() {
        let shim = MockServer::start().await;
        Mock::given(path("/ytm/search"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(SONGS)
                    .set_delay(Duration::from_secs(2)),
            )
            .mount(&shim)
            .await;
        Mock::given(path("/candidates"))
            .respond_with(ResponseTemplate::new(200).set_body_string(CANDIDATES))
            .mount(&shim)
            .await;
        let (clock, _) = clock();
        let quick = ListCaps {
            music: Duration::from_millis(200),
            ..caps()
        };
        let started = std::time::Instant::now();
        let pool = versions(&shim, clock, quick).pool("k", &teardrop(), false).await;
        assert!(started.elapsed() < Duration::from_millis(1_500));
        assert_eq!(pool.missed, [VersionSource::YouTubeMusic]);
        assert_eq!(pool.rows.len(), 3);
    }

    /// What was offered is kept for a bounded number of songs and for half an hour.
    #[tokio::test]
    async fn what_was_offered_is_bounded_and_ages_out() {
        let shim = shim(ResponseTemplate::new(200).set_body_string(CANDIDATES)).await;
        let (clock, now) = clock();
        let versions = versions(
            &shim,
            clock,
            ListCaps {
                offered_songs: 2,
                ..caps()
            },
        );
        for key in ["a", "b", "c"] {
            versions.pool(key, &teardrop(), false).await;
        }
        assert!(versions.offered("a").is_none());
        assert!(versions.offered_row("c", "live").is_some());
        assert!(versions.offered_row("c", "nope").is_none());
        *now.lock() += TimeDelta::minutes(31);
        assert!(versions.offered("c").is_none());
    }

    /// A copy fetched ahead is another version only while a person's pick stands and it was
    /// judged against another video than the pick.
    #[test]
    fn a_copy_is_another_version_only_against_a_persons_pick() {
        let registry = ExternalIdRegistry::in_memory();
        let id = registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("Massive Attack".into()),
            title: Some("Teardrop".into()),
            you_tube_id: Some("v0".into()),
            ..Default::default()
        });
        assert!(!copy_is_another_version(&registry, &id, Some("other")));
        registry
            .choose_stream_video(
                &id,
                StreamChoice {
                    video: "v1".into(),
                    title: Some("Teardrop".into()),
                    channel: None,
                    seconds: Some(330),
                    kind: StreamVersionKind::Release,
                    song_seconds: None,
                    by: "alice".into(),
                    at: Utc::now(),
                    auto: false,
                    seq: 0,
                },
            )
            .expect("chosen");
        assert!(!copy_is_another_version(&registry, &id, Some("v1")));
        assert!(copy_is_another_version(&registry, &id, Some("v0")));
        assert!(copy_is_another_version(&registry, &id, None));
        assert!(!copy_is_another_version(&registry, "unknown", Some("v0")));
    }
}
