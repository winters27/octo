using Microsoft.AspNetCore.Mvc;
using Octo.Services.Common;

namespace Octo.Services.Subsonic;

/// <summary>The downloads drawer's answers: clearing, Find songs and picking. Field names are a
/// contract with the Octo apps.</summary>
public partial class SubsonicResponseBuilder
{
    public IActionResult CreateClearedResponse(int cleared) => CreateJsonResponse(new Dictionary<string, object?>
    {
        ["status"] = "ok",
        ["version"] = SubsonicVersion,
        ["type"] = "octo",
        ["cleared"] = new Dictionary<string, object?> { ["count"] = cleared },
    });

    /// <summary>
    /// findSongs and getFoundSongs: the song, how each source's look went, and every copy found.
    /// A copy's id names it on this look for good, and pickFoundSong takes it as copy; its index
    /// is its place in the list, which older apps pick by once the search has ended.
    /// </summary>
    public IActionResult CreateFindSongsResponse(FindSnapshot found) => CreateJsonResponse(new Dictionary<string, object?>
    {
        ["status"] = "ok",
        ["version"] = SubsonicVersion,
        ["type"] = "octo",
        ["foundSongs"] = FoundSongsJson(found),
    });

    /// <summary>A Find songs search as the apps and the dashboard read it.</summary>
    public static Dictionary<string, object?> FoundSongsJson(FindSnapshot found) => new()
    {
        ["id"] = found.Id,
        ["state"] = found.State,
        ["error"] = found.Error,
        ["startedAt"] = Utc(found.StartedAt),
        ["song"] = new Dictionary<string, object?>
        {
            ["artist"] = found.Target.Artist,
            ["title"] = found.Target.Title,
            ["album"] = found.Target.Album,
            ["duration"] = found.Target.Duration,
            ["coverArt"] = found.Target.CoverArt,
            ["libraryId"] = found.Target.LibraryId,
            ["format"] = found.Target.OwnedFormat,
            ["quality"] = found.Target.OwnedQuality,
            ["size"] = found.Target.OwnedSize,
        },
        ["source"] = found.Sources.Select(source => new Dictionary<string, object?>
        {
            ["name"] = source.Name,
            ["state"] = source.State,
            ["text"] = source.Text,
            ["query"] = source.Queries,
        }).ToList(),
        ["candidate"] = found.Copies.Select((copy, index) =>
        {
            var json = CandidateJson(copy.Shown);
            json["id"] = copy.Id;
            json["index"] = index;
            return json;
        }).ToList(),
    };

    public IActionResult CreatePickResponse(PickOutcome outcome) => CreateJsonResponse(new Dictionary<string, object?>
    {
        ["status"] = "ok",
        ["version"] = SubsonicVersion,
        ["type"] = "octo",
        ["pick"] = new Dictionary<string, object?>
        {
            ["state"] = outcome.State,
            ["detail"] = outcome.Detail,
            ["key"] = outcome.Key,
        },
    });
}
