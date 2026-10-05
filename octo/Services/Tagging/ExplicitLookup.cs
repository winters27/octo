using Octo.Models.Domain;
using Octo.Services.Common;
using Octo.Services.Metadata;

namespace Octo.Services.Tagging;

/// <summary>What a library song's own file says about it, for the explicit lookup.</summary>
public sealed record LibrarySongFacts(string? Artist, string? Title, string? Album, double? Seconds,
    IReadOnlyList<string> Isrcs, string? FileName = null);

/// <summary>What the lookup found about one library song's words.</summary>
public enum ExplicitOutcome
{
    Explicit,
    Clean,
    NotExplicit,

    /// <summary>No match confident enough to write anything.</summary>
    Unsure,
}

/// <summary>The finding, in words for the dashboard, and the catalog track that said so.</summary>
public sealed record ExplicitFinding(ExplicitOutcome Outcome, string How, string? CatalogTrackId = null)
{
    /// <summary>The catalog did not answer this time; ask again later rather than record it.</summary>
    public bool CatalogBusy { get; init; }

    /// <summary>The advisory tag's number: 1 explicit, 2 clean, 0 neither; null when unsure.</summary>
    public int? Advisory => Outcome switch
    {
        ExplicitOutcome.Explicit => ExplicitAdvisory.Explicit,
        ExplicitOutcome.Clean => ExplicitAdvisory.Clean,
        ExplicitOutcome.NotExplicit => ExplicitAdvisory.None,
        _ => null,
    };

    public static ExplicitFinding Busy => new(ExplicitOutcome.Unsure, "the catalog did not answer") { CatalogBusy = true };
}

/// <summary>Where the lookup asks. Deezer in the app; a stand-in in tests.</summary>
public interface IExplicitCatalog
{
    Task<CatalogTrackAnswer> ByIsrcAsync(string isrc, CancellationToken ct);
    Task<CatalogTrackAnswer> SearchAsync(string? artist, string? title, CancellationToken ct);
}

public sealed class DeezerExplicitCatalog(DeezerMetadataService deezer) : IExplicitCatalog
{
    public Task<CatalogTrackAnswer> ByIsrcAsync(string isrc, CancellationToken ct) => deezer.TrackByIsrcAsync(isrc, ct);
    public Task<CatalogTrackAnswer> SearchAsync(string? artist, string? title, CancellationToken ct) =>
        deezer.SearchTrackFactsAsync(artist, title, ct);
}

/// <summary>
/// Whether a song already in the library is explicit, the clean edit or neither, for the
/// library-wide marking. Only a confident match counts:
/// <list type="number">
/// <item>A title or file name that says clean is the clean edit, as for a download.</item>
/// <item>The song's ISRC names one recording, and the catalog's track with that code says.</item>
/// <item>Otherwise every catalog track that is the same song by artist, title (version
/// included) and length, on the same album when any is: when they all say the same thing,
/// that is the answer. A clean and an explicit copy that both fit, or nothing that fits, is
/// "unsure" and nothing is written.</item>
/// </list>
/// </summary>
public static class ExplicitLookup
{
    public static async Task<ExplicitFinding> FindAsync(LibrarySongFacts song, IExplicitCatalog catalog, CancellationToken ct)
    {
        if (SaysClean(song.Title) || SaysClean(song.FileName))
            return new(ExplicitOutcome.Clean, "its name says it is the clean edit");

        foreach (var isrc in song.Isrcs.Select(SongIdentity.NormalizeIsrc).OfType<string>().Distinct())
        {
            var answer = await catalog.ByIsrcAsync(isrc, ct);
            if (answer.DidNotAnswer) return ExplicitFinding.Busy;
            if (answer.Tracks.FirstOrDefault(track => track.ExplicitContent is not null) is { } coded)
                return new(Outcome(coded.ExplicitContent), $"same ISRC ({isrc})", coded.TrackId);
        }

        if (string.IsNullOrWhiteSpace(song.Artist) || string.IsNullOrWhiteSpace(song.Title))
            return new(ExplicitOutcome.Unsure, "the file names no artist or title");
        var found = await catalog.SearchAsync(song.Artist, song.Title, ct);
        if (found.DidNotAnswer) return ExplicitFinding.Busy;
        return Decide(song, found.Tracks);
    }

    /// <summary>The name rule over the tracks a search found.</summary>
    public static ExplicitFinding Decide(LibrarySongFacts song, IReadOnlyList<CatalogTrackFacts> tracks)
    {
        var mine = new SongRef(song.Title, song.Artist, song.Seconds) { Isrcs = song.Isrcs.ToList<string?>() };
        var same = tracks
            .Where(track => SongIdentity.Same(mine, new SongRef(track.Title, track.Artist, track.Duration) { Isrcs = [track.Isrc] }).IsSame)
            .ToList();
        if (same.Count == 0) return new(ExplicitOutcome.Unsure, "no catalog song matched closely enough");

        var onAlbum = same.Where(track => AlbumsAgree(song.Album, track.Album)).ToList();
        var pool = onAlbum.Count > 0 ? onAlbum : same;
        var said = pool.Where(track => track.ExplicitContent is not null).ToList();
        if (said.Count == 0) return new(ExplicitOutcome.Unsure, "the catalog does not say");

        var answers = said.Select(track => Outcome(track.ExplicitContent)).Distinct().ToList();
        if (answers.Count > 1)
            return new(ExplicitOutcome.Unsure, answers.Contains(ExplicitOutcome.Clean) && answers.Contains(ExplicitOutcome.Explicit)
                ? "an explicit and a clean copy both fit" : "the copies that fit disagree");

        var where = onAlbum.Count > 0 ? "artist, title, length and album" : "artist, title and length";
        var copies = said.Count == 1 ? "" : $", {said.Count} copies agree";
        return new(answers[0], where + copies, said[0].TrackId);
    }

    private static ExplicitOutcome Outcome(int? content) => content switch
    {
        ExplicitStatus.Explicit => ExplicitOutcome.Explicit,
        ExplicitStatus.Clean => ExplicitOutcome.Clean,
        _ => ExplicitOutcome.NotExplicit,
    };

    private static bool AlbumsAgree(string? a, string? b)
    {
        var x = SongIdentity.Key(a);
        var y = SongIdentity.Key(b);
        return x.Length > 0 && y.Length > 0 && (x == y || x.Contains(y) || y.Contains(x));
    }

    private static bool SaysClean(string? title) =>
        !string.IsNullOrWhiteSpace(title) && SongIdentity.ParseTitle(title).Versions.Contains("clean");
}
