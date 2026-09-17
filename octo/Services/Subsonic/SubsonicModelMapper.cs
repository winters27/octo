using System.Text;
using System.Text.Json;
using System.Xml.Linq;
using Octo.Models.Search;
using Octo.Models.Subsonic;
using Octo.Services.Common;

namespace Octo.Services.Subsonic;

/// <summary>
/// Handles parsing Subsonic API responses and merging local with external search results.
/// </summary>
public class SubsonicModelMapper
{
    private readonly SubsonicResponseBuilder _responseBuilder;
    private readonly ILogger<SubsonicModelMapper> _logger;

    public SubsonicModelMapper(
        SubsonicResponseBuilder responseBuilder,
        ILogger<SubsonicModelMapper> logger)
    {
        _responseBuilder = responseBuilder;
        _logger = logger;
    }

    /// <summary>
    /// Parses a Subsonic search response and extracts songs, albums, and artists.
    /// </summary>
    public (List<object> Songs, List<object> Albums, List<object> Artists) ParseSearchResponse(
        byte[] responseBody,
        string? contentType)
    {
        var songs = new List<object>();
        var albums = new List<object>();
        var artists = new List<object>();

        try
        {
            var content = Encoding.UTF8.GetString(responseBody);
            
            if (contentType?.Contains("json") == true)
            {
                var jsonDoc = JsonDocument.Parse(content);
                // Both envelopes: search2 and search3 are the same hijack, and a search2
                // relay answers under searchResult2. Reading only searchResult3 silently
                // dropped every local row for search2 clients.
                if (jsonDoc.RootElement.TryGetProperty("subsonic-response", out var response) &&
                    (response.TryGetProperty("searchResult3", out var searchResult) ||
                     response.TryGetProperty("searchResult2", out searchResult)))
                {
                    if (searchResult.TryGetProperty("song", out var songElements))
                    {
                        foreach (var song in songElements.EnumerateArray())
                        {
                            songs.Add(_responseBuilder.ConvertSubsonicJsonElement(song, true));
                        }
                    }
                    if (searchResult.TryGetProperty("album", out var albumElements))
                    {
                        foreach (var album in albumElements.EnumerateArray())
                        {
                            albums.Add(_responseBuilder.ConvertSubsonicJsonElement(album, true));
                        }
                    }
                    if (searchResult.TryGetProperty("artist", out var artistElements))
                    {
                        foreach (var artist in artistElements.EnumerateArray())
                        {
                            artists.Add(_responseBuilder.ConvertSubsonicJsonElement(artist, true));
                        }
                    }
                }
            }
            else
            {
                var xmlDoc = XDocument.Parse(content);
                var ns = xmlDoc.Root?.GetDefaultNamespace() ?? XNamespace.None;
                var searchResult = xmlDoc.Descendants(ns + "searchResult3").FirstOrDefault()
                                   ?? xmlDoc.Descendants(ns + "searchResult2").FirstOrDefault();
                
                if (searchResult != null)
                {
                    foreach (var song in searchResult.Elements(ns + "song"))
                    {
                        songs.Add(_responseBuilder.ConvertSubsonicXmlElement(song, "song"));
                    }
                    foreach (var album in searchResult.Elements(ns + "album"))
                    {
                        albums.Add(_responseBuilder.ConvertSubsonicXmlElement(album, "album"));
                    }
                    foreach (var artist in searchResult.Elements(ns + "artist"))
                    {
                        artists.Add(_responseBuilder.ConvertSubsonicXmlElement(artist, "artist"));
                    }
                }
            }
        }
        catch (Exception ex)
        {
            _logger.LogWarning(ex, "Error parsing Subsonic search response");
        }

        return (songs, albums, artists);
    }

    /// <summary>
    /// Merges local and external search results (songs, albums, artists, playlists).
    /// </summary>
    public (List<object> MergedSongs, List<object> MergedAlbums, List<object> MergedArtists) MergeSearchResults(
        List<object> localSongs,
        List<object> localAlbums,
        List<object> localArtists,
        SearchResult externalResult,
        List<ExternalPlaylist> externalPlaylists,
        bool isJson)
    {
        if (isJson)
        {
            return MergeSearchResultsJson(localSongs, localAlbums, localArtists, externalResult, externalPlaylists);
        }
        else
        {
            return MergeSearchResultsXml(localSongs, localAlbums, localArtists, externalResult, externalPlaylists);
        }
    }

    private (List<object> MergedSongs, List<object> MergedAlbums, List<object> MergedArtists) MergeSearchResultsJson(
        List<object> localSongs,
        List<object> localAlbums,
        List<object> localArtists,
        SearchResult externalResult,
        List<ExternalPlaylist> externalPlaylists)
    {
        // Local songs first, external (YouTube placeholder) after. The earlier
        // version flipped this to put externals first because Arpeggi's "play
        // artist radio" feature reused search3 with songCount=2000 — locals
        // first would crowd externals out of its top-N. Now Arpeggi/Narjo
        // radio goes through getSimilarSongs2, so search3 is plain search and
        // users expect their owned tracks to top the results, with discovery
        // suggestions following.
        var mergedSongs = localSongs
            .Concat(externalResult.Songs.Select(s => _responseBuilder.ConvertSongToJson(s)))
            .ToList();
        
        // Albums, deduplicated by artist+name so an album you own is not listed twice.
        // Playlists follow, appearing as albums with genre "Playlist".
        var localAlbumKeys = new HashSet<string>(StringComparer.Ordinal);
        foreach (var album in localAlbums)
        {
            if (album is not Dictionary<string, object> dict) continue;
            dict.TryGetValue("artist", out var artistObj);
            dict.TryGetValue("name", out var nameObj);
            var key = AlbumKey(artistObj?.ToString(), nameObj?.ToString());
            if (key is not null) localAlbumKeys.Add(key);
        }

        var mergedAlbums = localAlbums
            .Concat(externalResult.Albums
                .Where(a => AlbumKey(a.Artist, a.Title) is not string k || !localAlbumKeys.Contains(k))
                .Select(a => _responseBuilder.ConvertAlbumToJson(a)))
            .Concat(externalPlaylists.Select(p => ConvertPlaylistToAlbumJson(p)))
            .ToList();
        
        // Deduplicate artists by name - prefer local artists over external ones
        var localArtistNames = new HashSet<string>(StringComparer.Ordinal);
        foreach (var artist in localArtists)
        {
            if (artist is Dictionary<string, object> dict && dict.TryGetValue("name", out var nameObj))
            {
                localArtistNames.Add(TitleMatch.Key(nameObj?.ToString()));
            }
        }
        
        var mergedArtists = localArtists.ToList();
        foreach (var externalArtist in externalResult.Artists)
        {
            // Only add external artist if no local artist with same name exists
            if (!localArtistNames.Contains(TitleMatch.Key(externalArtist.Name)))
            {
                mergedArtists.Add(_responseBuilder.ConvertArtistToJson(externalArtist));
            }
        }

        return (mergedSongs, mergedAlbums, mergedArtists);
    }

    private (List<object> MergedSongs, List<object> MergedAlbums, List<object> MergedArtists) MergeSearchResultsXml(
        List<object> localSongs,
        List<object> localAlbums,
        List<object> localArtists,
        SearchResult externalResult,
        List<ExternalPlaylist> externalPlaylists)
    {
        var ns = XNamespace.Get("http://subsonic.org/restapi");
        
        // Deduplicate artists by name - prefer local artists over external ones
        var localArtistNamesXml = new HashSet<string>(StringComparer.Ordinal);
        var mergedArtists = new List<object>();
        
        foreach (var artist in localArtists.Cast<XElement>())
        {
            var name = artist.Attribute("name")?.Value;
            if (!string.IsNullOrEmpty(name))
            {
                localArtistNamesXml.Add(TitleMatch.Key(name));
            }
            artist.Name = ns + "artist";
            mergedArtists.Add(artist);
        }
        
        foreach (var artist in externalResult.Artists)
        {
            // Only add external artist if no local artist with same name exists
            if (!localArtistNamesXml.Contains(TitleMatch.Key(artist.Name)))
            {
                mergedArtists.Add(_responseBuilder.ConvertArtistToXml(artist, ns));
            }
        }
        
        // Albums, deduplicated by artist+name so an album you own is not listed twice.
        var localAlbumKeysXml = new HashSet<string>(StringComparer.Ordinal);
        var mergedAlbums = new List<object>();
        foreach (var album in localAlbums.Cast<XElement>())
        {
            var key = AlbumKey(album.Attribute("artist")?.Value, album.Attribute("name")?.Value);
            if (key is not null) localAlbumKeysXml.Add(key);
            album.Name = ns + "album";
            mergedAlbums.Add(album);
        }
        foreach (var album in externalResult.Albums)
        {
            var key = AlbumKey(album.Artist, album.Title);
            if (key is not null && localAlbumKeysXml.Contains(key)) continue;
            mergedAlbums.Add(_responseBuilder.ConvertAlbumToXml(album, ns));
        }
        // Add playlists as albums
        foreach (var playlist in externalPlaylists)
        {
            mergedAlbums.Add(ConvertPlaylistToAlbumXml(playlist, ns));
        }
        
        // Songs
        var mergedSongs = new List<object>();
        foreach (var song in localSongs.Cast<XElement>())
        {
            song.Name = ns + "song";
            mergedSongs.Add(song);
        }
        foreach (var song in externalResult.Songs)
        {
            mergedSongs.Add(_responseBuilder.ConvertSongToXml(song, ns));
        }

        return (mergedSongs, mergedAlbums, mergedArtists);
    }
    
    /// <summary>Dedup key for an album. Null when there is not enough to compare on,
    /// which means "never treat this as a duplicate".</summary>
    // Typography-insensitive: a provider writing "Hoes Ain’t Shit" must not add a
    // second copy of the album the library already returned as "Hoes Ain't Shit".
    private static string? AlbumKey(string? artist, string? name)
        => TitleMatch.AlbumKey(artist, name);

    /// <summary>
    /// Converts an ExternalPlaylist to a JSON object representing an album.
    /// Playlists are represented as albums with genre "Playlist" and artist "🎵 {Provider} {Curator}".
    /// </summary>
    private Dictionary<string, object> ConvertPlaylistToAlbumJson(ExternalPlaylist playlist)
    {
        var artistName = $"🎵 {char.ToUpper(playlist.Provider[0])}{playlist.Provider.Substring(1)}";
        if (!string.IsNullOrEmpty(playlist.CuratorName))
        {
            artistName += $" {playlist.CuratorName}";
        }
        
        var artistId = $"curator-{playlist.Provider}-{playlist.CuratorName?.ToLowerInvariant().Replace(" ", "-") ?? "unknown"}";
        
        var album = new Dictionary<string, object>
        {
            ["id"] = playlist.Id,
            ["name"] = playlist.Name,
            ["artist"] = artistName,
            ["artistId"] = artistId,
            ["genre"] = "Playlist",
            ["songCount"] = playlist.TrackCount,
            ["duration"] = playlist.Duration
        };
        
        if (playlist.CreatedDate.HasValue)
        {
            album["year"] = playlist.CreatedDate.Value.Year;
            album["created"] = playlist.CreatedDate.Value.ToString("yyyy-MM-ddTHH:mm:ss");
        }
        
        if (!string.IsNullOrEmpty(playlist.CoverUrl))
        {
            album["coverArt"] = playlist.Id;
        }
        
        return album;
    }
    
    /// <summary>
    /// Converts an ExternalPlaylist to an XML element representing an album.
    /// Playlists are represented as albums with genre "Playlist" and artist "🎵 {Provider} {Curator}".
    /// </summary>
    private XElement ConvertPlaylistToAlbumXml(ExternalPlaylist playlist, XNamespace ns)
    {
        var artistName = $"🎵 {char.ToUpper(playlist.Provider[0])}{playlist.Provider.Substring(1)}";
        if (!string.IsNullOrEmpty(playlist.CuratorName))
        {
            artistName += $" {playlist.CuratorName}";
        }
        
        var artistId = $"curator-{playlist.Provider}-{playlist.CuratorName?.ToLowerInvariant().Replace(" ", "-") ?? "unknown"}";
        
        var album = new XElement(ns + "album",
            new XAttribute("id", playlist.Id),
            new XAttribute("name", playlist.Name),
            new XAttribute("artist", artistName),
            new XAttribute("artistId", artistId),
            new XAttribute("genre", "Playlist"),
            new XAttribute("songCount", playlist.TrackCount),
            new XAttribute("duration", playlist.Duration)
        );
        
        if (playlist.CreatedDate.HasValue)
        {
            album.Add(new XAttribute("year", playlist.CreatedDate.Value.Year));
            album.Add(new XAttribute("created", playlist.CreatedDate.Value.ToString("yyyy-MM-ddTHH:mm:ss")));
        }
        
        if (!string.IsNullOrEmpty(playlist.CoverUrl))
        {
            album.Add(new XAttribute("coverArt", playlist.Id));
        }
        
        return album;
    }
}
