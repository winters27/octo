using System.Text.Json;

namespace Octo.Services.LastFm;

/// <summary>
/// Last.fm's play counts: an artist's most played songs and the site's chart. Uncached here;
/// the caller keeps what it builds from them, and must know a call that did not answer apart
/// from an empty one, which the radio cache cannot tell it.
/// </summary>
public partial class LastFmService
{
    /// <summary>One song of a Last.fm list, with how often it was played and by how many.</summary>
    public sealed record ChartTrack(string Artist, string Title, long? Plays, long? Listeners);

    /// <summary>
    /// The artist's most played songs, most played first. Last.fm's own spelling of the name is
    /// used (autocorrect), so "daft punk" finds Daft Punk. Null when there is no key or Last.fm
    /// did not answer.
    /// </summary>
    public async Task<List<ChartTrack>?> GetArtistChartAsync(string artist, int limit = 50,
        CancellationToken cancellationToken = default)
    {
        if (string.IsNullOrWhiteSpace(artist)) return [];
        using var doc = await GetDocumentAsync("artist.gettoptracks", new Dictionary<string, string>
        {
            ["artist"] = artist, ["limit"] = limit.ToString(), ["autocorrect"] = "1",
        }, cancellationToken);
        if (doc is null || doc.RootElement.TryGetProperty("error", out _)) return null;
        return TryArray(doc.RootElement, "toptracks", "track", out var values) ? ReadChart(values, artist) : [];
    }

    /// <summary>Last.fm's chart of the moment, most played first. Null when there is no key or
    /// Last.fm did not answer.</summary>
    public async Task<List<ChartTrack>?> GetChartAsync(int limit = 50, CancellationToken cancellationToken = default)
    {
        using var doc = await GetDocumentAsync("chart.gettoptracks",
            new Dictionary<string, string> { ["limit"] = limit.ToString() }, cancellationToken);
        if (doc is null || doc.RootElement.TryGetProperty("error", out _)) return null;
        return TryArray(doc.RootElement, "tracks", "track", out var values) ? ReadChart(values, null) : [];
    }

    private static List<ChartTrack> ReadChart(JsonElement values, string? artist) =>
        values.EnumerateArray()
            .Select(item => new ChartTrack(
                Text(item, "artist", "name") is { Length: > 0 } name ? name : artist ?? "",
                Text(item, "name"),
                Count(item, "playcount"),
                Count(item, "listeners")))
            .Where(track => track.Artist.Length > 0 && track.Title.Length > 0)
            .ToList();

    private static long? Count(JsonElement element, string property) =>
        element.TryGetProperty(property, out var value) && long.TryParse(value.ToString(), out var count) && count >= 0
            ? count : null;
}
