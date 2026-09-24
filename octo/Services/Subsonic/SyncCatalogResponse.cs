using System.Globalization;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Xml.Linq;
using Octo.Models.Domain;

namespace Octo.Services.Subsonic;

/// <summary>
/// Reads and extends a relayed search2/search3 page for a sync walk. Navidrome's page goes
/// out byte-for-byte apart from the catalog rows added after its own, so nothing about the
/// library rows a syncing client already has can change because this ran.
/// </summary>
public static class SyncCatalogResponse
{
    private const string DateFormat = "yyyy-MM-ddTHH:mm:ss.fffZ";

    /// <summary>How many artists, albums and songs a page holds. Null when it cannot be
    /// read, which leaves the page alone.</summary>
    public static (int Artists, int Albums, int Songs)? CountRows(byte[] body, string? contentType, string envelope)
    {
        try
        {
            if (IsJson(contentType))
            {
                using var document = JsonDocument.Parse(body);
                if (!document.RootElement.TryGetProperty("subsonic-response", out var response)) return null;
                if (!response.TryGetProperty(envelope, out var result)) return (0, 0, 0);
                int Count(string name) =>
                    result.TryGetProperty(name, out var rows) && rows.ValueKind == JsonValueKind.Array
                        ? rows.GetArrayLength() : 0;
                return (Count("artist"), Count("album"), Count("song"));
            }

            var xml = XDocument.Parse(Encoding.UTF8.GetString(body));
            var element = xml.Root?.Elements().FirstOrDefault(child => child.Name.LocalName == envelope);
            if (xml.Root is null) return null;
            if (element is null) return (0, 0, 0);
            int XmlCount(string name) => element.Elements().Count(child => child.Name.LocalName == name);
            return (XmlCount("artist"), XmlCount("album"), XmlCount("song"));
        }
        catch (Exception)
        {
            return null;
        }
    }

    /// <summary>
    /// The page with catalog rows appended after the library's, each kind after its own. The
    /// builder renders the rows, so they are the same shape every other injected row has;
    /// <c>created</c> is then replaced with when the row joined the catalog.
    /// </summary>
    public static byte[] Append(byte[] body, string? contentType, string envelope,
        SubsonicResponseBuilder builder, SyncCatalog catalog,
        IReadOnlyList<Artist> artists, IReadOnlyList<Album> albums, IReadOnlyList<Song> songs)
    {
        string? Created(string id) =>
            catalog.Added.TryGetValue(id, out var added) ? added.ToString(DateFormat, CultureInfo.InvariantCulture) : null;

        if (IsJson(contentType))
        {
            var root = JsonNode.Parse(body)!.AsObject();
            var response = root["subsonic-response"]!.AsObject();
            var result = response[envelope] as JsonObject ?? new JsonObject();
            response[envelope] = result;

            JsonArray Rows(string name)
            {
                var rows = result[name] as JsonArray ?? new JsonArray();
                result[name] = rows;
                return rows;
            }

            void Add(JsonArray rows, object fields, string id)
            {
                var node = JsonSerializer.SerializeToNode(fields)!.AsObject();
                if (Created(id) is { } created && node.ContainsKey("created")) node["created"] = created;
                rows.Add(node);
            }

            if (artists.Count > 0) { var rows = Rows("artist"); foreach (var artist in artists) Add(rows, builder.ConvertArtistToJson(artist), artist.Id); }
            if (albums.Count > 0) { var rows = Rows("album"); foreach (var album in albums) Add(rows, builder.ConvertAlbumToJson(album), album.Id); }
            if (songs.Count > 0) { var rows = Rows("song"); foreach (var song in songs) Add(rows, builder.ConvertSongToJson(song), song.Id); }
            return Encoding.UTF8.GetBytes(root.ToJsonString());
        }

        var document = XDocument.Parse(Encoding.UTF8.GetString(body));
        var responseElement = document.Root!;
        var ns = responseElement.Name.Namespace;
        var container = responseElement.Elements().FirstOrDefault(child => child.Name.LocalName == envelope);
        if (container is null) { container = new XElement(ns + envelope); responseElement.Add(container); }

        XElement Stamp(XElement element, string id)
        {
            if (Created(id) is { } created && element.Attribute("created") is not null)
                element.SetAttributeValue("created", created);
            return element;
        }

        // Kept in schema order (artists, then albums, then songs) for clients that validate it.
        void Insert(string kind, IEnumerable<XElement> elements, params string[] after)
        {
            var list = elements.ToList();
            if (list.Count == 0) return;
            var anchor = container.Elements()
                .LastOrDefault(child => child.Name.LocalName == kind || after.Contains(child.Name.LocalName));
            if (anchor is null) container.AddFirst(list);
            else anchor.AddAfterSelf(list);
        }

        Insert("artist", artists.Select(artist => Stamp(builder.ConvertArtistToXml(artist, ns), artist.Id)));
        Insert("album", albums.Select(album => Stamp(builder.ConvertAlbumToXml(album, ns), album.Id)), "artist");
        Insert("song", songs.Select(song => Stamp(builder.ConvertSongToXml(song, ns), song.Id)), "artist", "album");
        return Encoding.UTF8.GetBytes(document.ToString());
    }

    private static bool IsJson(string? contentType) => contentType?.Contains("json", StringComparison.OrdinalIgnoreCase) == true;
}
