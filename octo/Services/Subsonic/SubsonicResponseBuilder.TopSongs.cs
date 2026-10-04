using System.Text.Json;
using Microsoft.AspNetCore.Mvc;
using Octo.Services.Common;

namespace Octo.Services.Subsonic;

public partial class SubsonicResponseBuilder
{
    /// <summary>The OpenSubsonic extension a client checks for before it asks for
    /// getArtistTopSongs or getTopChart.</summary>
    public const string TopSongsExtension = "octoTopSongs";
    public const int TopSongsExtensionVersion = 1;

    /// <summary>
    /// getArtistTopSongs and getTopChart: a ranked list, each entry a song with its rank, the
    /// Last.fm counts when there are any, and whether the caller's library has it. A library
    /// song is sent as Navidrome described it to the caller, so it plays from the library and
    /// keeps its heart and rating; an outside one in the shape every outside song has. Always
    /// JSON. The field names are a contract with the Octo apps.
    /// </summary>
    public IActionResult CreateTopSongsResponse(TopSongsService.TopList list, IReadOnlyList<JsonElement?> library) =>
        CreateJsonResponse(new Dictionary<string, object?>
        {
            ["status"] = "ok",
            ["version"] = SubsonicVersion,
            ["type"] = "octo",
            ["openSubsonic"] = true,
            ["topSongs"] = new Dictionary<string, object?>
            {
                ["artist"] = list.Artist,
                ["source"] = list.Source,
                ["entry"] = list.Songs.Select((top, index) =>
                {
                    var owned = index < library.Count ? library[index] : null;
                    return new Dictionary<string, object?>
                    {
                        ["rank"] = top.Rank,
                        ["plays"] = top.Plays,
                        ["listeners"] = top.Listeners,
                        ["inLibrary"] = owned is not null,
                        ["song"] = owned is { } element ? ConvertSubsonicJsonElement(element, isLocal: true) : ConvertSongToJson(top.Song),
                    };
                }).ToList(),
            },
        });
}
