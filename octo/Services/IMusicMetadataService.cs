using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Models.Download;
using Octo.Models.Search;
using Octo.Models.Subsonic;

namespace Octo.Services;

/// <summary>
/// Interface for external music metadata search service
/// (Deezer API, Spotify API, MusicBrainz, etc.)
/// </summary>
public interface IMusicMetadataService
{
    /// <summary>
    /// Searches for songs on external providers
    /// </summary>
    /// <param name="query">Search term</param>
    /// <param name="limit">Maximum number of results</param>
    /// <returns>List of found songs</returns>
    Task<List<Song>> SearchSongsAsync(string query, int limit = 20);

    /// <summary>
    /// Searches for songs given an already-split artist and title. Preferred when
    /// the caller has structured fields (e.g. Last.fm results) so providers don't
    /// have to reverse-parse a concatenated query — which corrupts multi-word
    /// artists like "The Beatles" into artist="The", title="Beatles ...".
    /// <paramref name="durationSeconds"/> if known is stored on the placeholder
    /// so the client's scrub bar shows the right total length on first play.
    /// </summary>
    Task<List<Song>> SearchSongsByArtistTitleAsync(string artist, string title, int limit = 1, int? durationSeconds = null);

    /// <summary>
    /// Best-effort pre-resolve of upstream identifiers (e.g. YouTube videoIds) for
    /// the first N songs of a freshly-built search result. Called fire-and-forget
    /// so search3 still returns instantly. Provider-specific (a Deezer/Qobuz
    /// implementation would no-op); default no-op preserves source compatibility
    /// for any provider that doesn't need it.
    /// </summary>
    Task PrewarmYouTubeIdsAsync(IEnumerable<Song> songs, int topN, CancellationToken ct = default)
        => Task.CompletedTask;

    /// <summary>
    /// Same as <see cref="PrewarmYouTubeIdsAsync"/> but accepts raw song ids — the
    /// implementation looks each id up in its own routing registry to find the
    /// artist/title to resolve. Used by the scrobble-driven sliding-window
    /// prewarm where the controller only has the scrobbled song id and the
    /// upcoming-songs list it stored at search time.
    /// </summary>
    Task PrewarmYouTubeIdsForSongIdsAsync(IEnumerable<string> songIds, int topN, CancellationToken ct = default)
        => Task.CompletedTask;
    
    /// <summary>
    /// Searches for albums on external providers
    /// </summary>
    Task<List<Album>> SearchAlbumsAsync(string query, int limit = 20);
    
    /// <summary>
    /// Searches for artists on external providers
    /// </summary>
    Task<List<Artist>> SearchArtistsAsync(string query, int limit = 20);
    
    /// <summary>
    /// Combined search (songs, albums, artists)
    /// </summary>
    Task<SearchResult> SearchAllAsync(string query, int songLimit = 20, int albumLimit = 20, int artistLimit = 20);
    
    /// <summary>
    /// Gets details of an external song
    /// </summary>
    Task<Song?> GetSongAsync(string externalProvider, string externalId);
    
    /// <summary>
    /// Gets details of an external album with its songs
    /// </summary>
    Task<Album?> GetAlbumAsync(string externalProvider, string externalId);
    
    /// <summary>
    /// Gets details of an external artist
    /// </summary>
    Task<Artist?> GetArtistAsync(string externalProvider, string externalId);
    
    /// <summary>
    /// Gets an artist's albums
    /// </summary>
    Task<List<Album>> GetArtistAlbumsAsync(string externalProvider, string externalId);
    
    /// <summary>
    /// Searches for playlists on external providers
    /// </summary>
    /// <param name="query">Search term</param>
    /// <param name="limit">Maximum number of results</param>
    /// <returns>List of found playlists</returns>
    Task<List<ExternalPlaylist>> SearchPlaylistsAsync(string query, int limit = 20);
    
    /// <summary>
    /// Gets details of an external playlist (metadata only, not tracks)
    /// </summary>
    /// <param name="externalProvider">Provider name (e.g., "deezer", "qobuz")</param>
    /// <param name="externalId">Playlist ID from the provider</param>
    /// <returns>Playlist details or null if not found</returns>
    Task<ExternalPlaylist?> GetPlaylistAsync(string externalProvider, string externalId);
    
    /// <summary>
    /// Gets all tracks from an external playlist
    /// </summary>
    /// <param name="externalProvider">Provider name (e.g., "deezer", "qobuz")</param>
    /// <param name="externalId">Playlist ID from the provider</param>
    /// <returns>List of songs in the playlist</returns>
    Task<List<Song>> GetPlaylistTracksAsync(string externalProvider, string externalId);
}
