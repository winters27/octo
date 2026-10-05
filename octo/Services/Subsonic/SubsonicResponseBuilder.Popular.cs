using System.Text.Json;
using System.Text.Json.Nodes;
using System.Xml.Linq;
using Microsoft.AspNetCore.Mvc;
using Octo.Models.Settings;
using Octo.Services.Library;

namespace Octo.Services.Subsonic;

public partial class SubsonicResponseBuilder
{
    /// <summary>What "Popular right now" says it is, in apps that show a playlist's comment.</summary>
    public const string PopularComment = "The most played songs right now, made by Octo. Songs you have play from your library.";

    /// <summary>"Popular right now" as a playlist row: the keys a mix has, read-only, with its own
    /// comment and its real song count.</summary>
    public Dictionary<string, object> PopularPlaylistFields(GeneratedPlaylist list)
    {
        var fields = GeneratedPlaylistFields(list, new GeneratedPlaylistSettings { TrackCount = 500 });
        fields["songCount"] = list.PoolSize;
        fields["comment"] = PopularComment;
        return fields;
    }

    /// <summary>
    /// "Popular right now" with its songs: library songs as Navidrome described them to the
    /// listener, outside songs in the shape every outside song has. In XML each song is an
    /// <c>entry</c> with its fields as attributes and its lists as child elements.
    /// </summary>
    public IActionResult CreatePopularPlaylistResponse(string format, GeneratedPlaylist list, IReadOnlyList<JsonObject> entries)
    {
        var fields = PopularPlaylistFields(list);
        fields["songCount"] = entries.Count;
        fields["duration"] = entries.Sum(entry =>
            entry["duration"] is JsonValue value && value.TryGetValue<int>(out var seconds) ? seconds : 0);
        if (format.Equals("json", StringComparison.OrdinalIgnoreCase))
        {
            var playlist = new Dictionary<string, object>(fields)
            {
                ["entry"] = new JsonArray(entries.Select(entry => (JsonNode)entry.DeepClone()).ToArray()),
            };
            return CreateJsonResponse(new Dictionary<string, object>
            {
                ["status"] = "ok", ["version"] = SubsonicVersion, ["playlist"] = playlist,
            });
        }
        var ns = XNamespace.Get(SubsonicNamespace);
        var document = new XDocument(new XElement(ns + "subsonic-response",
            new XAttribute("status", "ok"), new XAttribute("version", SubsonicVersion),
            new XElement(ns + "playlist", Attributes(fields),
                entries.Select(entry =>
                {
                    using var parsed = JsonDocument.Parse(entry.ToJsonString());
                    return JsonAsXml(ns + "entry", parsed.RootElement);
                }))));
        return new ContentResult { Content = document.ToString(), ContentType = "application/xml" };
    }
}
