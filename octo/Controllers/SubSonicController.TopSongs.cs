using System.Text.Json;
using Microsoft.AspNetCore.Mvc;
using Octo.Services.Common;

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
