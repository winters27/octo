using Octo.Services.Soulseek;

namespace Octo.Tests;

/// <summary>
/// CandidateAllowed is the seam between the deny-list and the ranking. The failure it guards
/// against is invisible from outside: a filter that denies everything leaves every track
/// unfetchable and looks exactly like Soulseek having no copies.
/// </summary>
public class SoulseekDenyListTests
{
    private static SoulseekFileHit Hit(string user, string file) =>
        new() { Username = user, Filename = file, Extension = "flac", Size = 40_000_000 };

    [Fact]
    public void CandidateAllowed_RejectedCandidate_IsNeverOfferedAgain()
    {
        var registry = new RejectedPeerRegistry();
        registry.Deny("peer1", "a.flac", "wrong recording", "A - B");

        Assert.False(SoulseekDownloadService.CandidateAllowed(Hit("peer1", "a.flac"), registry, enabled: true));
        Assert.True(SoulseekDownloadService.CandidateAllowed(Hit("peer1", "b.flac"), registry, enabled: true));
    }

    /// <summary>
    /// Turning the setting off is the fastest recovery from a wrong denial, so it has to work
    /// without touching the file the denials live in.
    /// </summary>
    [Fact]
    public void CandidateAllowed_VerificationOff_TheListIsInert()
    {
        var registry = new RejectedPeerRegistry();
        registry.Deny("peer1", "a.flac", "wrong recording", "A - B");

        Assert.True(SoulseekDownloadService.CandidateAllowed(Hit("peer1", "a.flac"), registry, enabled: false));
    }

    [Fact]
    public void CandidateAllowed_NoRegistry_FiltersNothing()
        => Assert.True(SoulseekDownloadService.CandidateAllowed(Hit("peer1", "a.flac"), null, enabled: true));
}
