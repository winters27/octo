using Octo.Models.Domain;
using Octo.Services.Common;

namespace Octo.Services.Tagging;

/// <summary>The advisory value for one download and what said so, for the report.</summary>
public sealed record AdvisoryDecision(int Value, string Source);

/// <summary>
/// Whether the version that landed is the explicit one or the clean edit, for the advisory tag
/// the library server reads into each song's explicit status. A clean and an explicit copy of a
/// song carry different codes, so the catalog's flag counts only from the hit whose code is the
/// landed file's, or from a hit the chooser was sure of. The code that was asked for stands in
/// only when the file and the fingerprint name none, and a name that says clean beats it: the
/// file a peer shared may be the other version.
/// </summary>
public static class ExplicitAdvisory
{
    public const int None = 0, Explicit = 1, Clean = 2;

    /// <summary>The advisory for this download, or null when nothing that matched this exact
    /// version said. <paramref name="sourceFile"/> is the peer's file name, when there was a peer.</summary>
    public static AdvisoryDecision? Decide(Song song, TagPlan? plan, string? sourceFile = null)
    {
        // A name that says clean is the strongest evidence there is: it is what was shared.
        if (SaysClean(song.Title) || SaysClean(plan?.Evidence?.File.Title) || SaysClean(LeafName(sourceFile)))
            return new AdvisoryDecision(Clean, "the file name");
        if (plan is null) return null;

        var catalog = plan.Ranked.Select(scored => scored.Candidate).Where(c => c.Source == TagSource.Catalog).ToList();
        if (catalog.Count == 0) return null;

        var landed = LandedCodes(song, plan);
        if (landed.Count > 0)
            return catalog.FirstOrDefault(c => SongIdentity.SharesIsrc(landed, c.Isrcs)) is { } sameCode
                && FromCatalog(sameCode.ExplicitContent, sameCode.Explicit) is { } byCode
                ? new AdvisoryDecision(byCode, TagSource.Catalog.ToString())
                : null;

        // Without a code to compare, only a catalog hit the chooser settled on outright.
        if (!plan.Rehearsed && plan.Confidence == TagConfidence.Strong
            && plan.Chosen?.Candidate is { Source: TagSource.Catalog } chosen
            && FromCatalog(chosen.ExplicitContent, chosen.Explicit) is { } byMatch)
            return new AdvisoryDecision(byMatch, TagSource.Catalog.ToString());
        return null;
    }

    /// <summary>The advisory as the tag report shows it.</summary>
    public static FieldDecision Field(AdvisoryDecision decision) => new(decision.Value switch
    {
        Explicit => "explicit",
        Clean => "clean edit",
        _ => "not explicit",
    }, decision.Source);

    /// <summary>The catalog's numbers as the advisory tag holds them: its 1 is explicit and its 3
    /// the clean edit; 0 is neither. Its other values say it does not know, so only the plain
    /// explicit flag can still answer.</summary>
    public static int? FromCatalog(int? explicitContent, bool? explicitLyrics) => explicitContent switch
    {
        1 => Explicit,
        3 => Clean,
        0 => None,
        _ => explicitLyrics == true ? Explicit : null,
    };

    /// <summary>The codes of what landed: the file's own, then the recording the fingerprint
    /// settled, then the song's (the request's or the match's) when nothing else named one.</summary>
    private static IReadOnlySet<string> LandedCodes(Song song, TagPlan plan)
    {
        var file = plan.Evidence?.File.Isrcs ?? [];
        if (file.Count > 0) return SongIdentity.Isrcs(file);
        if (plan.RecordingConfirmed && plan.Chosen?.Candidate is { Isrcs.Count: > 0 } confirmed)
            return SongIdentity.Isrcs(confirmed.Isrcs);
        return SongIdentity.Isrcs([song.Isrc]);
    }

    private static bool SaysClean(string? title) =>
        !string.IsNullOrWhiteSpace(title) && SongIdentity.ParseTitle(title).Versions.Contains("clean");

    private static string? LeafName(string? path)
    {
        if (string.IsNullOrWhiteSpace(path)) return null;
        var leaf = path.Split('\\', '/').LastOrDefault(part => part.Length > 0);
        return leaf is null ? null : Path.GetFileNameWithoutExtension(leaf);
    }
}
