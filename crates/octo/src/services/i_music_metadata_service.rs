//! The music metadata service's interface.
//!
//! The methods take no cancellation token: a caller that gives up drops the future, which stops
//! the work at its next await.

use std::time::Duration;

use async_trait::async_trait;
use octo_core::models::domain::{Album, Artist, Song};
use octo_core::models::search::SearchResult;
use octo_core::models::subsonic::ExternalPlaylist;
use tokio::task::JoinHandle;

/// Interface for external music metadata search service
/// (Deezer API, Spotify API, MusicBrainz, etc.)
#[async_trait]
pub trait IMusicMetadataService: Send + Sync {
    /// Searches for songs on external providers. `limit` was 20 by default.
    async fn search_songs(&self, query: &str, limit: i32) -> Vec<Song>;

    /// Searches for songs given an already-split artist and title. Preferred when
    /// the caller has structured fields (e.g. Last.fm results) so providers don't
    /// have to reverse-parse a concatenated query: which corrupts multi-word
    /// artists like "The Beatles" into artist="The", title="Beatles ...".
    /// `duration_seconds` if known is stored on the placeholder
    /// so the client's scrub bar shows the right total length on first play.
    /// (`limit` was 1 by default.)
    async fn search_songs_by_artist_title(
        &self,
        artist: &str,
        title: &str,
        limit: i32,
        duration_seconds: Option<i32>,
    ) -> Vec<Song>;

    /// Fills in real duration/album/year on external placeholder songs (which
    /// otherwise go out with no length) from a metadata provider. Bounded, cached,
    /// best-effort; does not block or fail the search if the provider is down.
    async fn enrich_external_songs(&self, _songs: &mut [Song]) {}

    /// The catalog's own track search for free text: each hit's artist and title, in the
    /// catalog's order, at most `limit`. Rows built from them need no lookup of their own. None
    /// when the catalog could not answer.
    async fn search_catalog_tracks(&self, _query: &str, _limit: i32) -> Option<Vec<(String, String)>> {
        None
    }

    /// Whether another catalog answers this catalog's searches now (Apple's, for a server Deezer
    /// does not serve): one song search for a query then fills its rows cheaper than a lookup
    /// per row.
    fn catalog_searches_stand_in(&self) -> bool {
        false
    }

    /// The catalog albums the recordings with these ISRCs are on, each once, with their songs.
    async fn catalog_albums_by_isrc(&self, _isrcs: &[String]) -> Vec<Album> {
        Vec::new()
    }

    /// [`Self::enrich_external_songs`] for a search someone is waiting on: one track search
    /// per row, no year. The rest of each row (the other queries for a row that search missed,
    /// the year) is looked up after the answer, for the next answer that lists the row.
    async fn enrich_search_rows(&self, songs: &mut [Song]) {
        self.enrich_external_songs(songs).await;
    }

    /// Gives external songs outside a search (stations, radio, a song read by its id) the
    /// lengths already known for them, and looks the rest up in the background so the next
    /// response has them. Never waits on the network: whatever is not known yet goes out
    /// with no length for now.
    fn complete_song_lengths(&self, _songs: &mut [Song]) {}

    /// Remembers the length of a file downloaded for the outside song `external_id`, so rows
    /// that still name the song by that id carry the length it really runs.
    fn remember_file_length(&self, _external_id: &str, _seconds: i32) {}

    /// Resolves the real YouTube video (and its duration) for the top of a search
    /// result so the shown length matches the audio that plays. Bounded + cached;
    /// also stores the videoId so playback reuses the same video. When `background`
    /// is true the songs are already sent: only the routing is written, a video a play already
    /// pinned is kept, and the shim's background lane is used.
    async fn resolve_top_durations(&self, _songs: &mut [Song], _background: bool) {}

    /// Starts, without waiting, the video lookups [`Self::resolve_top_durations`] will make for
    /// these songs, so they run while the songs are still being enriched.
    fn prefetch_top_videos(&self, _songs: &[Song]) {}

    /// [`Self::resolve_top_durations`] for a caller that waits at most `budget`: the rows
    /// resolved by then carry their video's length, and the others keep the length they have
    /// and are resolved in the background, as when `background` is true, without `songs` being
    /// touched again. Returns that background work while it still runs, for a caller whose next
    /// step must come after it. The default waits for the whole foreground pass at most
    /// `budget` and leaves nothing running.
    async fn resolve_top_durations_within(
        &self,
        songs: &mut [Song],
        budget: Duration,
    ) -> Option<JoinHandle<()>> {
        let _ = tokio::time::timeout(budget, self.resolve_top_durations(songs, false)).await;
        None
    }

    /// Gives outside rows from an earlier answer what has been found for them since: the
    /// pinned video's length once a lookup pinned one, else the remembered length for a row
    /// that has none, and the album, explicit word and year a row went out without. Reads
    /// what is known only; nothing is looked up.
    fn refresh_outside_songs(&self, _songs: &mut [Song]) {}

    /// Gives one outside song its album year when it has none, for a caller that answers with
    /// this one song (getSong, the native song detail): from what is already known, else one
    /// album lookup on the interactive lane, waited for a moment only. A lookup that takes
    /// longer goes on in the background, and the song is answered without a year.
    async fn complete_song_year(&self, _song: &mut Song) {}

    /// Looks up the length of one outside song that has none known yet, right away and on the
    /// interactive lane, for a caller that answers with this one song (getSong). The video
    /// found is pinned for playback, so the length shown is the length that plays. A song
    /// whose video is already pinned is left as it is.
    async fn resolve_song_length(&self, _song: &mut Song) {}

    /// Best-effort pre-resolve of upstream identifiers (e.g. YouTube videoIds) for
    /// the first N songs of a freshly-built search result. Called fire-and-forget
    /// so search3 still returns instantly. Provider-specific (a Deezer/Qobuz
    /// implementation would no-op); default no-op preserves source compatibility
    /// for any provider that doesn't need it.
    async fn prewarm_you_tube_ids(&self, _songs: &[Song], _top_n: usize) {}

    /// Same as [`IMusicMetadataService::prewarm_you_tube_ids`] but accepts raw song ids: the
    /// implementation looks each id up in its own routing registry to find the
    /// artist/title to resolve. Used by the scrobble-driven sliding-window
    /// prewarm where the controller only has the scrobbled song id and the
    /// upcoming-songs list it stored at search time.
    async fn prewarm_you_tube_ids_for_song_ids(&self, _song_ids: &[String], _top_n: usize) {}

    /// Readies the first `count` outside songs of `song_ids`, in order, to start playing at
    /// once: a song with no video gets one, and a video whose stream was never resolved has
    /// it resolved, so the shim holds its stream URL when the play arrives. For the songs that
    /// play next in a queue. Returns the ids of the songs it took on, so a wider prewarm can
    /// leave them out.
    async fn prewarm_next_streams(&self, _song_ids: &[String], _count: usize) -> Vec<String> {
        Vec::new()
    }

    /// Best-effort pre-fetch of cover art for the first N songs of a freshly-built
    /// search result, so a client that renders them a moment later finds the image
    /// already cached instead of triggering the fetch itself. Fire-and-forget, on
    /// each source's background rate-limit lane so it never queues behind a live
    /// search or cover request. Default no-op preserves source compatibility for
    /// any provider that doesn't need it.
    async fn prewarm_cover_art(&self, _songs: &[Song], _top_n: usize) {}

    /// Artists the catalog relates to an outside artist, as outside artists, at most `limit`.
    async fn get_related_artists(
        &self,
        _external_provider: &str,
        _external_id: &str,
        _limit: i32,
    ) -> Vec<Artist> {
        Vec::new()
    }

    /// Searches for albums on external providers. `limit` was 20 by default.
    async fn search_albums(&self, query: &str, limit: i32) -> Vec<Album>;

    /// The album rows a search lists: [`Self::search_albums`] with repeat editions of one record
    /// (deluxe, remastered, explicit and clean copies) as one row. Other album lookups, such as
    /// finding the catalog album that holds a library album's songs, see every edition.
    async fn search_album_rows(&self, query: &str, limit: i32) -> Vec<Album> {
        self.search_albums(query, limit).await
    }

    /// Searches for artists on external providers. `limit` was 20 by default.
    async fn search_artists(&self, query: &str, limit: i32) -> Vec<Artist>;

    /// Combined search (songs, albums, artists). The limits were 20 each by default.
    async fn search_all(
        &self,
        query: &str,
        song_limit: i32,
        album_limit: i32,
        artist_limit: i32,
    ) -> SearchResult;

    /// Gets details of an external song
    async fn get_song(&self, external_provider: &str, external_id: &str) -> Option<Song>;

    /// Gets details of an external album with its songs
    async fn get_album(&self, external_provider: &str, external_id: &str) -> Option<Album>;

    /// Gets details of an external artist
    async fn get_artist(&self, external_provider: &str, external_id: &str) -> Option<Artist>;

    /// Gets an artist's albums
    async fn get_artist_albums(&self, external_provider: &str, external_id: &str) -> Vec<Album>;

    /// Gets an artist's albums for a library artist's page. The library's album titles say
    /// which of two artists of one name is meant; a provider that cannot use them ignores them.
    async fn get_artist_albums_for_library(
        &self,
        external_provider: &str,
        external_id: &str,
        _library_album_titles: Option<&[String]>,
    ) -> Vec<Album> {
        self.get_artist_albums(external_provider, external_id).await
    }

    /// Gets an artist's albums with only the track counts already known, asking the catalog
    /// for none: for counts shown beside an artist's page, whose own album list asks for the
    /// missing ones at the same moment. A provider with no such lookup answers the plain list.
    async fn get_artist_albums_known_counts(&self, external_provider: &str, external_id: &str) -> Vec<Album> {
        self.get_artist_albums(external_provider, external_id).await
    }

    /// Searches for playlists on external providers. `limit` was 20 by default.
    async fn search_playlists(&self, query: &str, limit: i32) -> Vec<ExternalPlaylist>;

    /// Gets details of an external playlist (metadata only, not tracks).
    /// `external_provider` is the provider name (e.g., "deezer", "qobuz"), `external_id` the
    /// playlist ID from the provider. None if not found.
    async fn get_playlist(&self, external_provider: &str, external_id: &str) -> Option<ExternalPlaylist>;

    /// Gets all tracks from an external playlist.
    async fn get_playlist_tracks(&self, external_provider: &str, external_id: &str) -> Vec<Song>;
}
