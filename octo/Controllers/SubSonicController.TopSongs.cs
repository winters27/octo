using System.Text.Json;
using Microsoft.AspNetCore.Mvc;
using Octo.Models.Domain;
using Octo.Services.Common;
using Octo.Services.Subsonic;

namespace Octo.Controllers;

public partial class SubsonicController
{
    /// <summary>How many rows a list answers when the caller does not say.</summary>
    private const int TopSongsDefaultCount = 20;

    /// <summary>
    /// octoTopSongs v1: an artist's most played songs, ranked, for the apps' search. Each is
    /// marked in the caller's library or not: a library one plays from the library, an outside
    /// one plays and is added like any search row. <c>artist</c> is the name; <c>id</c> may be
    /// the artist's id from search, which names the catalog artist surely. Always JSON;
    /// credentials checked like getLibraryActions.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/getArtistTopSongs")]
    [Route("rest/getArtistTopSongs.view")]
    public async Task<IActionResult> GetArtistTopSongs()
    {
        var parameters = await ExtractAllParameters();
        if (await RefuseUnlessSignedInAsync(parameters, "json") is { } refused) return refused;
        var name = parameters.GetValueOrDefault("artist", "").Trim();
        if (name.Length == 0) return _responseBuilder.CreateError("json", 10, "Required parameter is missing: artist");
        if (TopSongs() is not { } topSongs) return _responseBuilder.CreateTopSongsResponse(TopSongsService.TopList.Empty, []);

        var list = await topSongs.ForArtistAsync(name, parameters.GetValueOrDefault("id"), HttpContext.RequestAborted);
        return await TopSongsAnswerAsync(list, parameters);
    }

    /// <summary>
    /// octoTopSongs v1: the chart of the moment, ranked and marked like getArtistTopSongs, for
    /// the apps' search before anything is typed.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/getTopChart")]
    [Route("rest/getTopChart.view")]
    public async Task<IActionResult> GetTopChart()
    {
        var parameters = await ExtractAllParameters();
        if (await RefuseUnlessSignedInAsync(parameters, "json") is { } refused) return refused;
        if (TopSongs() is not { } topSongs) return _responseBuilder.CreateTopSongsResponse(TopSongsService.TopList.Empty, []);

        var list = await topSongs.ChartAsync(HttpContext.RequestAborted);
        return await TopSongsAnswerAsync(list, parameters);
    }

    /// <summary>The count the Subsonic API gives getTopSongs when the caller does not say.</summary>
    private const int StandardTopSongsDefaultCount = 50;

    /// <summary>
    /// The standard getTopSongs (<c>artist</c> by name, <c>count</c>), for every Subsonic client,
    /// and by the artist's <c>id</c> instead of the name, which Navidrome offers as the
    /// topSongsByArtistId extension.
    /// Navidrome answers it from the library alone, often with nothing. Octo answers with the
    /// artist's songs ranked as getArtistTopSongs ranks them: a song the caller's library has
    /// is the library's own entry, and the rest are outside songs that play and are added with
    /// a heart, as in search. Outside songs follow search discovery: with it off only the
    /// library's songs are listed, still in Octo's order. When Octo has nothing to give (no
    /// source answered, an artist no source knows, or none of it in the library with discovery
    /// off), Navidrome's own answer is passed on, as before.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/getTopSongs")]
    [Route("rest/getTopSongs.view")]
    public async Task<IActionResult> GetTopSongs()
    {
        var parameters = await ExtractAllParameters();
        var format = parameters.GetValueOrDefault("f", "xml");
        var name = parameters.GetValueOrDefault("artist", "").Trim();
        var artistId = parameters.GetValueOrDefault("id", "").Trim();
        var topSongs = HttpContext.RequestServices.GetService<TopSongsService>();
        // A missing artist is Navidrome's to refuse, in its own words.
        if ((name.Length == 0 && artistId.Length == 0) || topSongs is null) return await NoTopSongsAsync(parameters, format);
        if (await RefuseUnlessSignedInAsync(parameters, format) is { } refused) return refused;
        if (name.Length == 0) name = await ArtistNameAsync(parameters, artistId) ?? "";
        if (name.Length == 0) return await NoTopSongsAsync(parameters, format);

        var count = int.TryParse(parameters.GetValueOrDefault("count"), out var asked) && asked > 0
            ? Math.Min(asked, TopSongsService.MaxRows)
            : StandardTopSongsDefaultCount;
        var list = await topSongs.ForArtistAsync(name, artistId.Length > 0 ? artistId : null, HttpContext.RequestAborted);
        var outside = _subsonicSettings.EnableSearchDiscovery;

        // With outside songs shown, only the rows asked for are looked up in the library. Without
        // them every row is, because the library's rows are all that is left to list.
        var candidates = outside ? list.Songs.Take(count).ToList() : list.Songs.ToList();
        var library = candidates.Count == 0
            ? []
            : await TopSongsService.MatchLibraryAsync(candidates,
                (query, _) => LibrarySongsMatchingAsync(parameters, query), HttpContext.RequestAborted);

        var username = parameters.GetValueOrDefault("u", "");
        var rows = new List<SubsonicResponseBuilder.TopSongRow>();
        for (var i = 0; i < candidates.Count && rows.Count < count; i++)
        {
            if (library[i] is { } owned) rows.Add(new(owned, null));
            else if (outside) rows.Add(new(null, SyncedSong(username, candidates[i].Song)));
        }
        if (rows.Count == 0) return await NoTopSongsAsync(parameters, format);

        _logger.LogInformation("getTopSongs '{Artist}' ({Client}): {Library} library and {Outside} outside songs, ranked by {Source}",
            name, parameters.GetValueOrDefault("c", ""), rows.Count(row => row.Library is not null),
            rows.Count(row => row.Outside is not null), list.Source);
        return _responseBuilder.CreateStandardTopSongsResponse(format, rows);
    }

    /// <summary>
    /// The answer when Octo has none: Navidrome's own, as every request got before, except for an
    /// outside artist's id, which Navidrome has never heard of. That gets an empty list, as any
    /// call it does not know with an outside id does.
    /// </summary>
    private async Task<IActionResult> NoTopSongsAsync(Dictionary<string, string> parameters, string format)
    {
        if (HasExternalId(parameters)) return _responseBuilder.CreateStandardTopSongsResponse(format, []);
        var relay = await _proxyService.RelaySafeAsync("rest/getTopSongs", parameters);
        return relay.Success && relay.Body is { Length: > 0 }
            ? File(relay.Body, relay.ContentType ?? $"application/{format}")
            : _responseBuilder.CreateError(format, 0, "Unable to reach Navidrome");
    }

    /// <summary>The name of the artist an id names: an outside artist's from what Octo remembers
    /// of it, a library artist's from Navidrome, asked as the caller. Null when neither knows.</summary>
    private async Task<string?> ArtistNameAsync(Dictionary<string, string> parameters, string artistId)
    {
        if (_idRegistry.Lookup(artistId) is { Kind: Octo.Services.Soulseek.RoutingKind.Artist, Artist: { Length: > 0 } outside })
            return outside;
        if (HasExternalId(parameters)) return null;
        var lookup = new Dictionary<string, string>(parameters) { ["f"] = "json" };
        lookup.Remove("count");
        var relay = await _proxyService.RelaySafeAsync("rest/getArtist", lookup);
        if (!relay.Success || relay.Body is not { Length: > 0 }) return null;
        try
        {
            using var document = JsonDocument.Parse(relay.Body);
            return document.RootElement.TryGetProperty("subsonic-response", out var response)
                && response.TryGetProperty("artist", out var artist)
                && artist.TryGetProperty("name", out var named) && named.ValueKind == JsonValueKind.String
                    ? named.GetString()?.Trim()
                    : null;
        }
        catch (JsonException)
        {
            return null;
        }
    }

    /// <summary>An outside song as this listener's sync catalog describes it, when it holds the
    /// song: a client that copies the library keeps whichever description it read last, so the
    /// two must agree, as on a station playlist.</summary>
    private Song SyncedSong(string username, Song song) =>
        _syncCatalog is not null && !song.IsLocal && _syncCatalog.TryGetSong(username, song.Id, out var synced) ? synced : song;

    /// <summary>The service, while search discovery is on. Looked up per request so the
    /// controller's constructor stays as it is.</summary>
    private TopSongsService? TopSongs() =>
        _subsonicSettings.EnableSearchDiscovery ? HttpContext.RequestServices.GetService<TopSongsService>() : null;

    /// <summary>The first <c>count</c> rows (20 unless asked, at most 50), each matched against
    /// the caller's own library.</summary>
    private async Task<IActionResult> TopSongsAnswerAsync(TopSongsService.TopList list,
        IReadOnlyDictionary<string, string> parameters)
    {
        var count = int.TryParse(parameters.GetValueOrDefault("count"), out var asked) && asked > 0
            ? Math.Min(asked, TopSongsService.MaxRows)
            : TopSongsDefaultCount;
        var shown = list with { Songs = list.Songs.Take(count).ToList() };
        var library = await TopSongsService.MatchLibraryAsync(shown.Songs,
            (query, ct) => LibrarySongsMatchingAsync(parameters, query), HttpContext.RequestAborted);
        return _responseBuilder.CreateTopSongsResponse(shown, library);
    }

    /// <summary>Navidrome's songs for a query, searched as the caller, or null when it did not
    /// answer. Nothing outside the library: this goes to Navidrome, not through Octo's search.</summary>
    private async Task<JsonElement?> LibrarySongsMatchingAsync(IReadOnlyDictionary<string, string> parameters, string query)
    {
        var search = parameters.ToDictionary(pair => pair.Key, pair => pair.Value);
        search["query"] = query;
        search["songCount"] = "5";
        search["albumCount"] = "0";
        search["artistCount"] = "0";
        search["f"] = "json";
        search.Remove("count");
        search.Remove("id");
        search.Remove("artist");
        try
        {
            var result = await _proxyService.RelaySafeAsync("rest/search3", search);
            if (!result.Success || result.Body is not { Length: > 0 }) return null;
            using var document = JsonDocument.Parse(result.Body);
            return document.RootElement.TryGetProperty("subsonic-response", out var response)
                && response.TryGetProperty("searchResult3", out var found)
                && found.TryGetProperty("song", out var songs)
                ? songs.Clone()
                : null;
        }
        catch (Exception ex)
        {
            _logger.LogDebug("top songs: library search '{Q}' failed: {M}", query, ex.Message);
            return null;
        }
    }
}
