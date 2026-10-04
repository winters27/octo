using System.Net;
using System.Text.Json;
using System.Text.RegularExpressions;

namespace Octo.Services.Imports;

/// <summary>What a public link held: its name, picture and songs, and whether that was all of them.</summary>
public sealed record LinkedList(string Kind, string Id, string Name, string? ImageUrl, IReadOnlyList<ImportTrack> Tracks, bool Capped);

/// <summary>
/// Reads a public Spotify playlist or album from its link, with no sign-in, through the page
/// Spotify serves for embedding it in other sites. That page carries the first 100 songs, with
/// their title, artists and length (no album for a playlist), which is the most a link can give
/// without an account. Larger lists say so, and the account sign-in or an exported file reads them whole.
/// </summary>
public sealed partial class SpotifyLinkReader
{
    public const string ClientName = "spotify-embed";
    /// <summary>What the embed page holds at most; a list this long may be longer.</summary>
    internal const int EmbedCap = 100;

    private readonly IHttpClientFactory _http;
    private readonly ILogger<SpotifyLinkReader> _logger;

    public SpotifyLinkReader(IHttpClientFactory http, ILogger<SpotifyLinkReader> logger)
    {
        _http = http;
        _logger = logger;
    }

    [GeneratedRegex(@"(?:open\.spotify\.com/(?:intl-[a-z-]+/|embed/)?|spotify:)(playlist|album)[/:]([A-Za-z0-9]{10,40})", RegexOptions.IgnoreCase)]
    private static partial Regex LinkPattern();

    [GeneratedRegex(@"<script id=""__NEXT_DATA__"" type=""application/json"">(.*?)</script>", RegexOptions.Singleline)]
    private static partial Regex NextData();

    /// <summary>The kind (playlist or album) and id a link or URI names, or null.</summary>
    public static (string Kind, string Id)? Parse(string? link)
    {
        if (string.IsNullOrWhiteSpace(link)) return null;
        var match = LinkPattern().Match(link.Trim());
        return match.Success ? (match.Groups[1].Value.ToLowerInvariant(), match.Groups[2].Value) : null;
    }

    public async Task<LinkedList> ReadAsync(string link, CancellationToken ct)
    {
        if (Parse(link) is not var (kind, id))
            throw new SpotifyException("That is not a link to a Spotify playlist or album.");
        using var response = await _http.CreateClient(ClientName)
            .GetAsync($"https://open.spotify.com/embed/{kind}/{id}", ct);
        if (response.StatusCode == HttpStatusCode.NotFound)
            throw new SpotifyException($"Spotify has no public {kind} at that link. A private playlist needs the Spotify sign-in.", response.StatusCode);
        if (!response.IsSuccessStatusCode)
            throw new SpotifyException($"Spotify answered {(int)response.StatusCode} for that link.", response.StatusCode);
        var page = await response.Content.ReadAsStringAsync(ct);
        var read = ParsePage(kind, id, page)
            ?? throw new SpotifyException($"Octo could not read the songs on that {kind}'s page. Spotify may have changed it.");
        _logger.LogInformation("Read {Count} songs from Spotify {Kind} {Id} by its link", read.Tracks.Count, kind, id);
        return read;
    }

    /// <summary>The list the embed page describes, or null when it is not the page Octo knows.</summary>
    internal static LinkedList? ParsePage(string kind, string id, string page)
    {
        var match = NextData().Match(page);
        if (!match.Success) return null;
        try
        {
            using var doc = JsonDocument.Parse(match.Groups[1].Value);
            if (!TryPath(doc.RootElement, out var entity, "props", "pageProps", "state", "data", "entity")) return null;
            var name = SpotifyWebApi.Str(entity, "name") ?? SpotifyWebApi.Str(entity, "title") ?? $"Spotify {kind}";
            string? image = null;
            if (entity.TryGetProperty("coverArt", out var cover) && cover.TryGetProperty("sources", out var sources)
                && sources.ValueKind == JsonValueKind.Array)
                image = sources.EnumerateArray().Select(source => SpotifyWebApi.Str(source, "url")).FirstOrDefault(url => url is not null);
            var album = kind == "album" ? name : null;
            var tracks = new List<ImportTrack>();
            if (entity.TryGetProperty("trackList", out var list) && list.ValueKind == JsonValueKind.Array)
                foreach (var row in list.EnumerateArray())
                {
                    var title = SpotifyWebApi.Str(row, "title");
                    if (string.IsNullOrWhiteSpace(title)) continue;
                    if (SpotifyWebApi.Str(row, "entityType") is { } type && type != "track") continue;
                    // The subtitle is the credit, artists joined by commas (with a no-break space).
                    var artist = (SpotifyWebApi.Str(row, "subtitle") ?? "").Replace(' ', ' ').Trim();
                    var seconds = row.TryGetProperty("duration", out var d) && d.TryGetInt64(out var ms) && ms > 0
                        ? (int)Math.Round(ms / 1000.0) : (int?)null;
                    tracks.Add(new ImportTrack
                    {
                        Key = ImportKeys.ForSpotify(SpotifyWebApi.Str(row, "uri")) ?? ImportKeys.ForWords(artist, title),
                        Title = title.Trim(), Artist = artist, Album = album, Seconds = seconds,
                    });
                }
            return new LinkedList(kind, id, name, image, tracks, Capped: tracks.Count >= EmbedCap);
        }
        catch (JsonException)
        {
            return null;
        }
    }

    private static bool TryPath(JsonElement root, out JsonElement found, params string[] path)
    {
        found = root;
        foreach (var step in path)
        {
            if (found.ValueKind != JsonValueKind.Object || !found.TryGetProperty(step, out found)) return false;
        }
        return found.ValueKind == JsonValueKind.Object;
    }
}
