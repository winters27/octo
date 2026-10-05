using System.Text.Json;
using Octo.Services.Common;

namespace Octo.Services.Metadata;

/// <summary>One catalog track as the explicit lookup weighs it: who, what, how long, its code,
/// and what the catalog says of its words (see <see cref="Octo.Models.Domain.ExplicitStatus"/>).</summary>
public sealed record CatalogTrackFacts(string TrackId, string Title, string Artist, string? Album,
    int? Duration, string? Isrc, int? ExplicitContent);

/// <summary>A catalog answer: the tracks, or that the catalog did not answer this time.</summary>
public sealed record CatalogTrackAnswer(IReadOnlyList<CatalogTrackFacts> Tracks, bool DidNotAnswer)
{
    public static readonly CatalogTrackAnswer Busy = new([], true);
}

public partial class DeezerMetadataService
{
    /// <summary>How many hits a search for the explicit lookup reads: enough for the explicit
    /// original and its clean edit to both be seen, which five often is not.</summary>
    private const int ExplicitSearchHits = 15;

    /// <summary>
    /// The catalog's track for this ISRC, or none when it has none. In the background lane,
    /// for a library-wide job that must never crowd out a search someone is waiting on.
    /// </summary>
    public async Task<CatalogTrackAnswer> TrackByIsrcAsync(string isrc, CancellationToken ct = default)
    {
        if (SongIdentity.NormalizeIsrc(isrc) is not { } code) return new([], false);
        using var r = await GetJsonAsync($"{Base}/track/isrc:{Uri.EscapeDataString(code)}", ct, background: true);
        if (r.Transient) return CatalogTrackAnswer.Busy;
        if (r.Doc is null || r.Doc.RootElement.ValueKind != JsonValueKind.Object) return new([], false);
        return Facts(r.Doc.RootElement) is { } track ? new([track], false) : new([], false);
    }

    /// <summary>
    /// Every track of a search that names this song (the guard the tagger's lookups use: a
    /// positive match on artist or title and a contradiction on neither), from the first way of
    /// writing it that finds any. The caller judges them strictly. In the background lane.
    /// </summary>
    public async Task<CatalogTrackAnswer> SearchTrackFactsAsync(string? artist, string? title, CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(artist) && string.IsNullOrWhiteSpace(title)) return new([], false);
        foreach (var variant in SongIdentity.QueryVariants(title, artist).Take(TrackSearches))
        {
            using var r = await GetJsonAsync(
                $"{Base}/search?q={Uri.EscapeDataString(variant.Text)}&limit={ExplicitSearchHits}", ct, background: true);
            if (r.Transient) return CatalogTrackAnswer.Busy;
            var hits = AllMatches(r.Doc, artist, title).Select(Facts).OfType<CatalogTrackFacts>().ToList();
            if (hits.Count > 0) return new(hits, false);
        }
        return new([], false);
    }

    private static CatalogTrackFacts? Facts(JsonElement t)
    {
        var id = t.TryGetProperty("id", out var tid) && tid.ValueKind == JsonValueKind.Number
            ? tid.GetInt64().ToString(System.Globalization.CultureInfo.InvariantCulture) : null;
        var title = Str(t, "title");
        if (id is null || string.IsNullOrWhiteSpace(title)) return null;
        var artist = t.TryGetProperty("artist", out var a) && a.ValueKind == JsonValueKind.Object ? Str(a, "name") : null;
        var album = t.TryGetProperty("album", out var al) && al.ValueKind == JsonValueKind.Object ? Str(al, "title") : null;
        return new CatalogTrackFacts(id, title, artist ?? "", album, Int(t, "duration"), Str(t, "isrc"),
            Octo.Models.Domain.ExplicitStatus.FromCatalog(Int(t, "explicit_content_lyrics"), Bool(t, "explicit_lyrics")));
    }
}
