using System.Text.Json;
using System.Xml.Linq;
using Microsoft.AspNetCore.Mvc;
using Octo.Models.Domain;
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

    /// <summary>One row of the standard getTopSongs: the caller's own library song as Navidrome
    /// described it to them, or a song from outside the library.</summary>
    public sealed record TopSongRow(JsonElement? Library, Song? Outside);

    /// <summary>
    /// The standard Subsonic getTopSongs, answered by Octo: <c>topSongs</c> holding one
    /// <c>song</c> per row, in rank order, in the format asked for. A library song goes out as
    /// Navidrome sent it (in XML its fields as attributes and its lists as child elements, the
    /// shape Navidrome's own XML has), an outside song in the shape every outside song has.
    /// </summary>
    public IActionResult CreateStandardTopSongsResponse(string format, IReadOnlyList<TopSongRow> rows)
    {
        if (format == "json")
        {
            return CreateJsonResponse(new Dictionary<string, object>
            {
                ["status"] = "ok",
                ["version"] = SubsonicVersion,
                ["openSubsonic"] = true,
                ["topSongs"] = new Dictionary<string, object>
                {
                    ["song"] = rows.Select(row => row.Library is { } library
                        ? ConvertSubsonicJsonElement(library, isLocal: true)
                        : ConvertSongToJson(row.Outside!)).ToList(),
                },
            });
        }

        var ns = XNamespace.Get(SubsonicNamespace);
        var document = new XDocument(new XElement(ns + "subsonic-response",
            new XAttribute("status", "ok"),
            new XAttribute("version", SubsonicVersion),
            new XAttribute("openSubsonic", "true"),
            new XElement(ns + "topSongs", rows.Select(row => row.Library is { } library
                ? LibrarySongXml(library, ns)
                : ConvertSongToXml(row.Outside!, ns)))));
        return new ContentResult { Content = document.ToString(), ContentType = "application/xml" };
    }

    /// <summary>A library song Navidrome described in JSON, as the <c>song</c> element its XML
    /// would have held, marked as the library's like every library row Octo passes on.</summary>
    internal static XElement LibrarySongXml(JsonElement song, XNamespace ns)
    {
        var element = JsonAsXml(ns + "song", song);
        element.SetAttributeValue("isExternal", "false");
        return element;
    }

    /// <summary>
    /// A Subsonic JSON object as the XML element the API writes for it: plain values as
    /// attributes, an object as a child element, a list of objects as one child element each
    /// (<c>genres</c>, <c>artists</c>), and a list of plain values as one child element each
    /// holding the value as text (<c>isrc</c>).
    /// </summary>
    internal static XElement JsonAsXml(XName name, JsonElement value)
    {
        var element = new XElement(name);
        if (value.ValueKind != JsonValueKind.Object) return element;
        foreach (var property in value.EnumerateObject())
        {
            var child = name.Namespace + property.Name;
            switch (property.Value.ValueKind)
            {
                case JsonValueKind.Object:
                    element.Add(JsonAsXml(child, property.Value));
                    break;
                case JsonValueKind.Array:
                    foreach (var item in property.Value.EnumerateArray())
                    {
                        if (item.ValueKind == JsonValueKind.Object) element.Add(JsonAsXml(child, item));
                        else if (JsonScalar(item) is { } text) element.Add(new XElement(child, text));
                    }
                    break;
                default:
                    if (JsonScalar(property.Value) is { } scalar) element.SetAttributeValue(property.Name, scalar);
                    break;
            }
        }
        return element;
    }

    private static string? JsonScalar(JsonElement value) => value.ValueKind switch
    {
        JsonValueKind.String => value.GetString(),
        JsonValueKind.Number => value.GetRawText(),
        JsonValueKind.True => "true",
        JsonValueKind.False => "false",
        _ => null,
    };
}
