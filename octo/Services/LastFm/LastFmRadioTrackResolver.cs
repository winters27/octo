using System.Text.Json;
using Octo.Models.Domain;
using Octo.Services.Common;
using Octo.Services.Subsonic;
using Octo.Services.Soulseek;

namespace Octo.Services.LastFm;

/// <summary>Resolves a Last.fm candidate locally first, then as an external placeholder.</summary>
public sealed class LastFmRadioTrackResolver
{
    private readonly SubsonicProxyService _proxy;
    private readonly IMusicMetadataService _metadata;
    private readonly ILogger<LastFmRadioTrackResolver> _logger;
    private readonly ExternalIdRegistry _registry;

    public LastFmRadioTrackResolver(SubsonicProxyService proxy, IMusicMetadataService metadata,
        ExternalIdRegistry registry,
        ILogger<LastFmRadioTrackResolver> logger)
    {
        _proxy = proxy;
        _metadata = metadata;
        _registry = registry;
        _logger = logger;
    }

    public async Task<Song?> ResolveScrobbleAsync(string id,
        IReadOnlyDictionary<string, string> authenticatedParameters)
    {
        if (_registry.Lookup(id) is { Kind: RoutingKind.Song } route)
            return new Song { Id = id, Artist = route.Artist ?? "", Title = route.Title ?? "",
                Album = route.Album ?? "", Duration = route.Duration, IsLocal = false,
                ExternalProvider = SoulseekMetadataService.ProviderName, ExternalId = id };
        try
        {
            var parameters = authenticatedParameters.ToDictionary(pair => pair.Key, pair => pair.Value);
            parameters["id"] = id; parameters["f"] = "json";
            var result = await _proxy.RelaySafeAsync("rest/getSong", parameters);
            if (!result.Success || result.Body is not { Length: > 0 }) return null;
            using var document = JsonDocument.Parse(result.Body);
            if (!document.RootElement.TryGetProperty("subsonic-response", out var response)
                || !response.TryGetProperty("song", out var song)) return null;
            return new Song { Id = id, Artist = String(song, "artist"), Title = String(song, "title"),
                Album = String(song, "album"), Genre = NullableString(song, "genre"),
                Duration = Integer(song, "duration"), IsLocal = true };
        }
        catch (Exception ex) { _logger.LogDebug(ex, "scrobble metadata lookup failed for {Id}", id); return null; }
    }

    /// <param name="youTubeId">A video a source already named for this song (YouTube Music).</param>
    public async Task<Song?> ResolveAsync(string artist, string title, int? duration,
        IReadOnlyDictionary<string, string> authenticatedParameters,
        CancellationToken cancellationToken = default, string? youTubeId = null)
    {
        var local = await TryFindLocalMatchAsync(artist, title, authenticatedParameters);
        if (local is not null) return local;
        var hits = await _metadata.SearchSongsByArtistTitleAsync(artist, title, 1, duration);
        // A source that already named the video saves the search later, and plays exactly what
        // it suggested.
        if (hits.Count > 0 && !string.IsNullOrEmpty(youTubeId)
            && _registry.Lookup(hits[0].Id) is { } routing && string.IsNullOrEmpty(routing.YouTubeId))
            routing.YouTubeId = youTubeId;
        return hits.Count > 0 ? hits[0] : null;
    }

    public async Task<Song?> TryFindLocalMatchAsync(string artist, string title,
        IReadOnlyDictionary<string, string> authenticatedParameters)
    {
        try
        {
            var parameters = authenticatedParameters.ToDictionary(kv => kv.Key, kv => kv.Value);
            parameters["query"] = $"{artist} {title}";
            parameters["songCount"] = "3";
            parameters["albumCount"] = "0";
            parameters["artistCount"] = "0";
            parameters["f"] = "json";

            var result = await _proxy.RelaySafeAsync("rest/search3", parameters);
            if (!result.Success || result.Body is not { Length: > 0 }) return null;

            using var document = JsonDocument.Parse(result.Body);
            if (!document.RootElement.TryGetProperty("subsonic-response", out var response)
                || !response.TryGetProperty("searchResult3", out var search)
                || !search.TryGetProperty("song", out var songs)
                || songs.ValueKind != JsonValueKind.Array)
                return null;

            foreach (var song in songs.EnumerateArray())
            {
                var hitArtist = String(song, "artist");
                var hitTitle = String(song, "title");
                var id = String(song, "id");
                if (string.IsNullOrEmpty(id) || !IsSameRecording(artist, title, hitArtist, hitTitle))
                    continue;

                return LibrarySong(song);
            }
        }
        catch (Exception ex)
        {
            _logger.LogDebug(ex, "local radio match lookup failed for {Artist} - {Title}", artist, title);
        }
        return null;
    }

    /// <summary>
    /// What a radio was started from in the library. Subsonic's getSimilarSongs takes a song, an
    /// album or an artist id; an album is read as its first song, an artist as the name alone.
    /// Null when Navidrome knows none of them for this listener.
    /// </summary>
    /// <summary>One library song as this listener's Navidrome describes it; null when they cannot see it.</summary>
    public async Task<Song?> ReadLibrarySongAsync(string id, IReadOnlyDictionary<string, string> authenticatedParameters) =>
        await ReadAsync("rest/getSong", id, authenticatedParameters, "song") is { } song ? LibrarySong(song) : null;

    public async Task<LibrarySeed?> ReadSeedAsync(string id,
        IReadOnlyDictionary<string, string> authenticatedParameters)
    {
        if (await ReadAsync("rest/getSong", id, authenticatedParameters, "song") is { } song)
            return new LibrarySeed(LibrarySong(song), String(song, "artist"));
        if (await ReadAsync("rest/getAlbum", id, authenticatedParameters, "album") is { } album)
        {
            var first = Songs(album).FirstOrDefault();
            return first is null ? null : new LibrarySeed(first, String(album, "artist") is { Length: > 0 } name ? name : first.Artist);
        }
        if (await ReadAsync("rest/getArtist", id, authenticatedParameters, "artist") is { } artist
            && String(artist, "name") is { Length: > 0 } artistName)
            return new LibrarySeed(null, artistName);
        return null;
    }

    /// <summary>
    /// Library songs near a seed by its own tags: the rest of its album, which for a library
    /// downloaded from playlists is the playlist, and songs sharing its genre. They lead a radio
    /// Last.fm cannot place, so the radio keeps the seed's style (#78). Never the seed itself.
    /// </summary>
    public async Task<List<Song>> LibraryNeighboursAsync(Song seed, int count,
        IReadOnlyDictionary<string, string> authenticatedParameters)
    {
        var sameAlbum = new List<Song>();
        if (!string.IsNullOrEmpty(seed.AlbumId)
            && await ReadAsync("rest/getAlbum", seed.AlbumId, authenticatedParameters, "album") is { } album)
        {
            sameAlbum = Songs(album).Where(song => song.Id != seed.Id).ToList();
            Random.Shared.Shuffle(System.Runtime.InteropServices.CollectionsMarshal.AsSpan(sameAlbum));
        }

        var sameGenre = new List<Song>();
        if (!string.IsNullOrWhiteSpace(seed.Genre))
        {
            var parameters = authenticatedParameters.ToDictionary(pair => pair.Key, pair => pair.Value);
            parameters.Remove("id");
            parameters["genre"] = seed.Genre;
            parameters["size"] = Math.Clamp(count, 1, 500).ToString();
            parameters["f"] = "json";
            try
            {
                var result = await _proxy.RelaySafeAsync("rest/getRandomSongs", parameters);
                if (result.Success && result.Body is { Length: > 0 })
                {
                    using var document = JsonDocument.Parse(result.Body);
                    if (document.RootElement.TryGetProperty("subsonic-response", out var response)
                        && response.TryGetProperty("randomSongs", out var random))
                        sameGenre = Songs(random).Where(song => song.Id != seed.Id).ToList();
                }
            }
            catch (Exception ex) { _logger.LogDebug(ex, "radio genre neighbours failed for {Genre}", seed.Genre); }
        }

        return Interleave([sameAlbum, sameGenre]).DistinctBy(song => song.Id).Take(count).ToList();
    }

    /// <summary>One from each list in turn, keeping each list's order, until all are spent.</summary>
    public static IEnumerable<T> Interleave<T>(IReadOnlyList<IReadOnlyList<T>> lists)
    {
        for (var index = 0; lists.Any(list => index < list.Count); index++)
            foreach (var list in lists)
                if (index < list.Count) yield return list[index];
    }

    private async Task<JsonElement?> ReadAsync(string endpoint, string id,
        IReadOnlyDictionary<string, string> authenticatedParameters, string element)
    {
        try
        {
            var parameters = authenticatedParameters.ToDictionary(pair => pair.Key, pair => pair.Value);
            parameters["id"] = id; parameters["f"] = "json";
            var result = await _proxy.RelaySafeAsync(endpoint, parameters);
            if (!result.Success || result.Body is not { Length: > 0 }) return null;
            using var document = JsonDocument.Parse(result.Body);
            return document.RootElement.TryGetProperty("subsonic-response", out var response)
                && response.TryGetProperty(element, out var found) && found.ValueKind == JsonValueKind.Object
                ? found.Clone() : null;
        }
        catch (Exception ex) { _logger.LogDebug(ex, "{Endpoint} failed for radio seed {Id}", endpoint, id); return null; }
    }

    private static List<Song> Songs(JsonElement parent) =>
        parent.TryGetProperty("song", out var songs) && songs.ValueKind == JsonValueKind.Array
            ? songs.EnumerateArray().Where(song => String(song, "id").Length > 0).Select(LibrarySong).ToList()
            : [];

    private static Song LibrarySong(JsonElement song) => new()
    {
        Id = String(song, "id"),
        Title = String(song, "title"),
        Artist = String(song, "artist"),
        ArtistId = NullableString(song, "artistId"),
        Album = String(song, "album"),
        AlbumId = NullableString(song, "albumId"),
        Duration = Integer(song, "duration"),
        Year = Integer(song, "year"),
        Track = Integer(song, "track"),
        Genre = NullableString(song, "genre"),
        Suffix = NullableString(song, "suffix"),
        BitRate = Integer(song, "bitRate"),
        Isrcs = Texts(song, "isrc"),
        MusicBrainzRecordingId = NullableString(song, "musicBrainzId"),
        IsLocal = true,
    };

    /// <summary>
    /// Whether a library hit is the recording Last.fm recommended.
    ///
    /// Biased to NO, the opposite of the download verifier: a false no plays the external copy
    /// instead, a false yes silently plays a different song you own. Substring matching in
    /// either direction was doing exactly that ("Air" matched "Airbourne", "Intro" matched every
    /// intro). So the whole comparison is <see cref="SongIdentity"/>'s: the same title, the same
    /// version, a shared artist, both artists known. A radio edit counts as the song here, since
    /// playing your radio edit for the album cut is what anyone wants from a station.
    /// </summary>
    internal static bool IsSameRecording(string wantArtist, string wantTitle, string hitArtist, string hitTitle) =>
        !string.IsNullOrWhiteSpace(wantArtist) && !string.IsNullOrWhiteSpace(hitArtist)
        && SongIdentity.Same(wantTitle, wantArtist, hitTitle, hitArtist, RadioMatch).IsSame;

    private static readonly SongMatchOptions RadioMatch = new()
    {
        LengthToleranceSeconds = null,
        AlsoNeutral = ["radio edit"],
    };

    private static string String(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) ? value.GetString() ?? "" : "";

    private static string? NullableString(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) ? value.GetString() : null;

    private static int? Integer(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.Number
            ? value.GetInt32()
            : null;

    /// <summary>A list of text as Navidrome sent it, OpenSubsonic's <c>isrc</c>; empty when absent.</summary>
    private static List<string> Texts(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.Array
            ? value.EnumerateArray().Where(item => item.ValueKind == JsonValueKind.String)
                .Select(item => item.GetString()!).ToList()
            : [];
}

/// <summary>A radio's starting point in the library.</summary>
/// <param name="Song">The song, or an album's first song; null for an artist.</param>
/// <param name="Artist">Whose radio it is, as the library credits them.</param>
public sealed record LibrarySeed(Song? Song, string Artist);
