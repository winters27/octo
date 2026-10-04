using Microsoft.AspNetCore.Mvc;
using Octo.Services.Common;

namespace Octo.Controllers;

/// <summary>
/// octoAcquisitions version 2, the apps' downloads drawer: one download's whole log, clearing
/// finished ones, and Find songs (a search run again by hand, every copy listed, one picked).
/// Always JSON, and every call is checked with a ping to Navidrome as getAcquisitions is. Each
/// person sees and picks only their own.
/// </summary>
public partial class SubsonicController
{
    /// <summary>One of the caller's downloads with its log, by the key getAcquisitions lists.</summary>
    [HttpGet, HttpPost]
    [Route("rest/getAcquisition")]
    [Route("rest/getAcquisition.view")]
    public async Task<IActionResult> GetAcquisition()
    {
        const string format = "json";
        var parameters = await ExtractAllParameters();
        if (await CheckCallerAsync(parameters) is { } refused) return refused;
        var key = parameters.GetValueOrDefault("key", "").Trim();
        if (key.Length == 0) return _responseBuilder.CreateError(format, 10, "Required parameter is missing: key");
        var username = await SignedInUserAsync(parameters);
        var row = string.IsNullOrWhiteSpace(username) ? null : _acquisitionTracker?.Detail(key, username);
        return row is null
            ? _responseBuilder.CreateError(format, 70, "No such download")
            : _responseBuilder.CreateAcquisitionResponse(row);
    }

    /// <summary>Takes the caller's finished downloads off their list, or the one named by key. A
    /// download still running stays.</summary>
    [HttpGet, HttpPost]
    [Route("rest/clearAcquisitions")]
    [Route("rest/clearAcquisitions.view")]
    public async Task<IActionResult> ClearAcquisitions()
    {
        var parameters = await ExtractAllParameters();
        if (await CheckCallerAsync(parameters) is { } refused) return refused;
        var key = parameters.GetValueOrDefault("key")?.Trim();
        var username = await SignedInUserAsync(parameters);
        var cleared = string.IsNullOrWhiteSpace(username) || _acquisitionTracker is null
            ? 0
            : _acquisitionTracker.Clear(username, string.IsNullOrEmpty(key) ? null : key);
        return _responseBuilder.CreateClearedResponse(cleared);
    }

    /// <summary>
    /// Find songs: starts a search for one song (an outside song's id, or a library song's) on the
    /// caller's download sources, and answers at once while it runs. getFoundSongs follows it.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/findSongs")]
    [Route("rest/findSongs.view")]
    public async Task<IActionResult> FindSongs()
    {
        const string format = "json";
        var parameters = await ExtractAllParameters();
        if (await CheckCallerAsync(parameters) is { } refused) return refused;
        var id = parameters.GetValueOrDefault("id", "").Trim();
        if (id.Length == 0) return _responseBuilder.CreateError(format, 10, "Required parameter is missing: id");
        if (HttpContext.RequestServices.GetService<SongFinder>() is not { } finder)
            return _responseBuilder.CreateError(format, 0, "Find songs is not available on this server");
        var username = await SignedInUserAsync(parameters);
        if (string.IsNullOrWhiteSpace(username))
            return _responseBuilder.CreateError(format, 50, "Sign in with a username to find songs");
        var found = await finder.StartAsync(id, username, HttpContext.RequestAborted);
        return found is null
            ? _responseBuilder.CreateError(format, 70, "No song with this id")
            : _responseBuilder.CreateFindSongsResponse(found);
    }

    /// <summary>A Find songs search as it stands, by the id findSongs answered.</summary>
    [HttpGet, HttpPost]
    [Route("rest/getFoundSongs")]
    [Route("rest/getFoundSongs.view")]
    public async Task<IActionResult> GetFoundSongs()
    {
        const string format = "json";
        var parameters = await ExtractAllParameters();
        if (await CheckCallerAsync(parameters) is { } refused) return refused;
        var search = parameters.GetValueOrDefault("search", "").Trim();
        if (search.Length == 0) return _responseBuilder.CreateError(format, 10, "Required parameter is missing: search");
        var username = await SignedInUserAsync(parameters);
        var found = string.IsNullOrWhiteSpace(username) ? null
            : HttpContext.RequestServices.GetService<SongFinder>()?.Get(search, username);
        return found is null
            ? _responseBuilder.CreateError(format, 70, "This search is gone. Search again.")
            : _responseBuilder.CreateFindSongsResponse(found);
    }

    /// <summary>Fetches exactly one copy a Find songs search listed, by its index there.</summary>
    [HttpGet, HttpPost]
    [Route("rest/pickFoundSong")]
    [Route("rest/pickFoundSong.view")]
    public async Task<IActionResult> PickFoundSong()
    {
        const string format = "json";
        var parameters = await ExtractAllParameters();
        if (await CheckCallerAsync(parameters) is { } refused) return refused;
        var search = parameters.GetValueOrDefault("search", "").Trim();
        if (search.Length == 0 || !int.TryParse(parameters.GetValueOrDefault("candidate"), out var index))
            return _responseBuilder.CreateError(format, 10, "Required parameter is missing: search and candidate");
        var username = await SignedInUserAsync(parameters);
        if (string.IsNullOrWhiteSpace(username))
            return _responseBuilder.CreateError(format, 50, "Sign in with a username to pick a song");
        if (HttpContext.RequestServices.GetService<SongFinder>() is not { } finder)
            return _responseBuilder.CreateError(format, 0, "Find songs is not available on this server");
        var outcome = await finder.PickAsync(search, index, username);
        return _responseBuilder.CreatePickResponse(outcome);
    }
}
