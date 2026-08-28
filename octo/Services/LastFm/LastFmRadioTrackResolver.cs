using System.Text.Json;
using Octo.Models.Domain;
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

    public async Task<Song?> ResolveAsync(string artist, string title, int? duration,
        IReadOnlyDictionary<string, string> authenticatedParameters,
        CancellationToken cancellationToken = default)
    {
        var local = await TryFindLocalMatchAsync(artist, title, authenticatedParameters);
        if (local is not null) return local;
        var hits = await _metadata.SearchSongsByArtistTitleAsync(artist, title, 1, duration);
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
                if (string.IsNullOrEmpty(id)
                    || !ContainsEither(hitArtist, artist)
                    || !ContainsEither(hitTitle, title))
                    continue;

                return new Song
                {
                    Id = id,
                    Title = hitTitle,
                    Artist = hitArtist,
                    ArtistId = NullableString(song, "artistId"),
                    Album = String(song, "album"),
                    AlbumId = NullableString(song, "albumId"),
                    Duration = Integer(song, "duration"),
                    Year = Integer(song, "year"),
                    Track = Integer(song, "track"),
                    Genre = NullableString(song, "genre"),
                    IsLocal = true,
                };
            }
        }
        catch (Exception ex)
        {
            _logger.LogDebug(ex, "local radio match lookup failed for {Artist} - {Title}", artist, title);
        }
        return null;
    }

    private static bool ContainsEither(string left, string right) =>
        !string.IsNullOrEmpty(left) && !string.IsNullOrEmpty(right)
        && (left.Contains(right, StringComparison.OrdinalIgnoreCase)
            || right.Contains(left, StringComparison.OrdinalIgnoreCase));

    private static string String(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) ? value.GetString() ?? "" : "";

    private static string? NullableString(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) ? value.GetString() : null;

    private static int? Integer(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.Number
            ? value.GetInt32()
            : null;
}
