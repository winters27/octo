using System.Text.Json;

namespace Octo.Services.Metadata;

/// <summary>
/// Deezer's popularity lists: an artist's most played songs and the chart of the moment. Both
/// keyless, and each track comes with its album, cover and length, so a row needs no further
/// lookup to be shown.
/// </summary>
public partial class DeezerMetadataService
{
    /// <summary>One track of a popularity list, in the list's own order.</summary>
    public sealed record ChartTrack(
        string DeezerId, string Title, string Artist, string? ArtistId,
        string? Album, string? AlbumId, string? CoverUrl, int? Duration, bool? Explicit);

    /// <summary>The most Deezer lists for an artist, and for its chart.</summary>
    public const int ChartLimit = 100;

    /// <summary>An artist's top songs move slowly; the chart of the moment moves daily.</summary>
    private static readonly TimeSpan ArtistTopTtl = TimeSpan.FromHours(6);
    private static readonly TimeSpan ChartTtl = TimeSpan.FromHours(1);

    /// <summary>
    /// The catalog artist's most played songs, most played first. Null when Deezer did not
    /// answer this time, so a caller can try another source and must cache nothing; empty
    /// when it answered that the artist has none.
    /// </summary>
    public Task<List<ChartTrack>?> GetArtistTopAsync(string deezerArtistId, int limit = ChartLimit,
        CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(deezerArtistId) || limit <= 0) return Task.FromResult<List<ChartTrack>?>([]);
        var take = Math.Min(limit, ChartLimit);
        return ChartAsync($"top|{deezerArtistId}|{take}",
            $"{Base}/artist/{Uri.EscapeDataString(deezerArtistId)}/top?limit={take}", ArtistTopTtl, ct);
    }

    /// <summary>
    /// Deezer's chart of the moment, most played first. Deezer picks the country from where
    /// the request comes from. Null when Deezer did not answer this time.
    /// </summary>
    public Task<List<ChartTrack>?> GetChartAsync(int limit = ChartLimit, CancellationToken ct = default)
    {
        var take = Math.Clamp(limit, 1, ChartLimit);
        return ChartAsync($"chart|{take}", $"{Base}/chart/0/tracks?limit={take}", ChartTtl, ct);
    }

    private async Task<List<ChartTrack>?> ChartAsync(string key, string url, TimeSpan ttl, CancellationToken ct)
    {
        if (TryGetCached<List<ChartTrack>>(key, out var cached)) return cached;
        return await SharedAsync<List<ChartTrack>?>(key, async () =>
        {
            using var r = await GetJsonAsync(url, CancellationToken.None);
            if (r.Transient) return null;
            var tracks = new List<ChartTrack>();
            if (r.Doc is not null
                && r.Doc.RootElement.TryGetProperty("data", out var data)
                && data.ValueKind == JsonValueKind.Array)
            {
                foreach (var t in data.EnumerateArray())
                {
                    if (ReadChartTrack(t) is { } track) tracks.Add(track);
                }
            }
            Put(key, tracks, tracks.Count == 0 ? NegativeTtl : ttl);
            return tracks;
        }, ct);
    }

    /// <summary>A track as the popularity lists give it. Its artist and album ride inside it.</summary>
    internal static ChartTrack? ReadChartTrack(JsonElement t)
    {
        var id = t.TryGetProperty("id", out var tid) && tid.ValueKind == JsonValueKind.Number
            ? tid.GetInt64().ToString() : null;
        var title = Str(t, "title");
        if (id is null || string.IsNullOrWhiteSpace(title)) return null;
        if (t.TryGetProperty("readable", out var readable) && readable.ValueKind == JsonValueKind.False) return null;

        string? artist = null, artistId = null, album = null, albumId = null, cover = null;
        if (t.TryGetProperty("artist", out var a) && a.ValueKind == JsonValueKind.Object)
        {
            artist = Str(a, "name");
            artistId = a.TryGetProperty("id", out var aid) && aid.ValueKind == JsonValueKind.Number
                ? aid.GetInt64().ToString() : null;
        }
        if (string.IsNullOrWhiteSpace(artist)) return null;
        if (t.TryGetProperty("album", out var al) && al.ValueKind == JsonValueKind.Object)
        {
            album = Str(al, "title");
            albumId = al.TryGetProperty("id", out var alid) && alid.ValueKind == JsonValueKind.Number
                ? alid.GetInt64().ToString() : null;
            cover = Str(al, "cover_xl") ?? Str(al, "cover_big") ?? Str(al, "cover_medium");
        }
        bool? explicitLyrics = t.TryGetProperty("explicit_lyrics", out var ex)
            && ex.ValueKind is JsonValueKind.True or JsonValueKind.False ? ex.GetBoolean() : null;
        return new ChartTrack(id, title!, artist!, artistId, album, albumId, cover,
            Int(t, "duration") is > 0 and var seconds ? seconds : null, explicitLyrics);
    }
}
