namespace Octo.Services.CoverArt;

/// <summary>What to do with a cover a song already has.</summary>
public enum OwnCoverVerdict
{
    /// <summary>Good enough to keep without asking anyone for another.</summary>
    Keep,

    /// <summary>Worth keeping unless the cover sources find one much larger, or (when nothing said
    /// what the album's cover looks like) one that shows it is another picture.</summary>
    Weigh,

    /// <summary>Not worth keeping when anything else is found.</summary>
    Replace,
}

/// <summary>The verdict on one cover, why, and whether it looked like the album's cover
/// (null when there was nothing to compare it with).</summary>
public sealed record OwnCoverJudgement(OwnCoverVerdict Verdict, string Why, bool? SameArt, int Width, int Height)
{
    public int Side => Math.Min(Width, Height);
}

/// <summary>
/// Whether the cover a song arrived with (or the cover of the library copy a replacement takes
/// the place of) is worth keeping. Asked before the cover chain replaces it: the chain used to
/// put Apple's master over every file, so a peer's clean 1400 px scan of the same art was
/// thrown away for no gain, while a peer's 300 px thumbnail or another album's picture is
/// exactly what the chain is for.
///
/// The rules, in order:
/// <list type="number">
/// <item>Unreadable or under 150 px: replace.</item>
/// <item>Not square, when video covers are being replaced: replace.</item>
/// <item>Art of the album the file was filed away from: replace.</item>
/// <item>A different picture from the album's cover (the look-alike hash more than
/// <see cref="CoverImage.LikenessTolerance"/> bits apart): replace. The library copy's own
/// cover is spared this one: it is what the album shows now, and the cover art page is where
/// that picture gets changed.</item>
/// <item>The same art blown up from a smaller picture (less than <see cref="BlownUpRatio"/> of
/// the album cover's fine detail at the same size): replace.</item>
/// <item>The same art at <see cref="KeepSide"/> px or more: keep.</item>
/// <item>Anything else is weighed: it stays unless the chain finds one at least
/// <see cref="MuchLarger"/> times its side.</item>
/// </list>
/// </summary>
public static class CoverKeep
{
    /// <summary>Big enough to keep outright when it is the same art: the catalog's own covers are
    /// 1000 px, and the chain stops looking at that size too.</summary>
    public const int KeepSide = DownloadCoverResolver.SharpSide;

    /// <summary>How much larger a found cover has to be to replace one that is the same art but
    /// under <see cref="KeepSide"/>: 800 px gives way to the catalog's 1000, 900 px does not.
    /// The cover art page asks a little less (1.2) because there a person picked the album.</summary>
    public const double MuchLarger = 1.25;

    /// <summary>A cover with less than this share of the album cover's fine detail, measured at
    /// the same size, was blown up from a smaller picture.</summary>
    public const double BlownUpRatio = 0.35;

    /// <summary>Below this much detail the album's own cover is too plain (a flat colour, a
    /// single word) for the comparison to say anything.</summary>
    internal const double MinReferenceDetail = 1.0;

    /// <summary>The detail comparison runs at no more than this side, to stay quick.</summary>
    private const int DetailSide = 800;

    /// <summary>
    /// Judges <paramref name="own"/> against <paramref name="reference"/>, the album's cover as
    /// a source has it (the catalog's, or failing that the one the chain found), or null when
    /// there is none to compare with.
    /// </summary>
    public static OwnCoverJudgement Judge(byte[] own, byte[]? reference, bool requireSquare,
        bool otherAlbum = false, bool libraryCopy = false)
    {
        if (CoverImage.Measure(own) is not { } size)
            return new(OwnCoverVerdict.Replace, "it could not be read", null, 0, 0);
        var (width, height) = size;
        if (!CoverImage.IsUsable(own, requireSquare: false))
            return new(OwnCoverVerdict.Replace, "it was too small", null, width, height);
        if (requireSquare && !CoverImage.IsUsable(own, requireSquare: true))
            return new(OwnCoverVerdict.Replace, "it was not square", null, width, height);
        if (otherAlbum)
            return new(OwnCoverVerdict.Replace, "it was the cover of the album the file was filed under before", null, width, height);

        var side = Math.Min(width, height);
        bool? sameArt = null;
        if (reference is { Length: > 0 } && CoverImage.IsUsable(reference, requireSquare: true)
            && CoverImage.LooksHash(own) is { } ownLooks && CoverImage.LooksHash(reference) is { } referenceLooks)
            sameArt = CoverImage.LookAlike(ownLooks, referenceLooks);

        if (sameArt == false && !libraryCopy)
            return new(OwnCoverVerdict.Replace, "it was a different picture from the album's cover", false, width, height);
        if (sameArt == true && BlownUp(own, side, reference!))
            return new(OwnCoverVerdict.Replace, "it was a smaller picture blown up", true, width, height);
        if (side >= KeepSide && (sameArt == true || libraryCopy))
            return new(OwnCoverVerdict.Keep, sameArt == true ? "the same art" : "the cover your library shows now", sameArt, width, height);
        return new(OwnCoverVerdict.Weigh, sameArt == true ? "the same art" : "", sameArt, width, height);
    }

    /// <summary>True when a found cover of <paramref name="foundSide"/> px is enough larger to
    /// replace one of <paramref name="ownSide"/> px.</summary>
    public static bool IsMuchLarger(int foundSide, int ownSide) => foundSide >= ownSide * MuchLarger;

    /// <summary>Whether <paramref name="own"/> holds much less fine detail than the reference at
    /// the size they share. Unknown counts as no.</summary>
    internal static bool BlownUp(byte[] own, int ownSide, byte[] reference)
    {
        if (CoverImage.Measure(reference) is not { } referenceSize) return false;
        var side = Math.Min(DetailSide, Math.Min(ownSide, Math.Min(referenceSize.Width, referenceSize.Height)));
        if (CoverImage.Detail(reference, side) is not { } theirs || theirs < MinReferenceDetail) return false;
        return CoverImage.Detail(own, side) is { } mine && mine < theirs * BlownUpRatio;
    }
}
