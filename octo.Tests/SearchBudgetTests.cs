using Octo.Services.Subsonic;

namespace Octo.Tests;

/// <summary>
/// The local/external split for search3. These pin issue #14: the old local floor was a
/// flat 20, which is also the Subsonic spec default for songCount, so a client that sent
/// the default (or sent nothing) had its whole budget consumed by local results and never
/// received a single discovery row. Search looked like it only ever returned albums.
/// </summary>
public class SearchBudgetTests
{
    /// <summary>
    /// The split exactly as it behaved before the fix. Kept here so the "nothing changes
    /// for large requests" guarantee is checked by CI on every run rather than by a
    /// one-off manual diff against a running server.
    /// </summary>
    private static (int Local, int External) Legacy(int requestedSongs)
    {
        var local = Math.Max(20, requestedSongs / 4);
        var external = Math.Min(150, Math.Max(0, requestedSongs - local));
        return (local, external);
    }

    [Theory]
    // Below the floor: local-only, and we stop asking Navidrome for more than the
    // client wanted. No discovery means no Last.fm fan-out on a type-ahead keystroke.
    [InlineData(0, 0, 0)]
    [InlineData(5, 5, 0)]
    [InlineData(12, 12, 0)]
    // The reported bug. The spec default used to yield zero external rows.
    [InlineData(20, 12, 8)]
    [InlineData(40, 12, 28)]
    [InlineData(60, 15, 45)]
    [InlineData(79, 19, 60)]
    // From here up the quarter rule dominates, so the local side stops changing. The
    // external side is held at the ceiling, which is the number of rows a query actually
    // builds; past it the rows would be unenriched placeholders.
    [InlineData(80, 20, 60)]
    [InlineData(200, 50, 60)]
    [InlineData(1000, 250, 60)]
    public void Compute_SplitsAsSpecified(int requested, int expectedLocal, int expectedExternal)
    {
        var (local, external) = SearchBudget.Compute(requested);

        Assert.Equal(expectedLocal, local);
        Assert.Equal(expectedExternal, external);
    }

    [Fact]
    public void Compute_LeavesTheLocalTargetUnchangedForLargeRequests()
    {
        // At 80 and above, requested/4 is at least 20, so the old flat floor was never the
        // binding term and the new capped floor resolves to the same number. Radio-style
        // clients and anything else sending a large songCount see the same local target
        // they saw before.
        for (var n = 80; n <= 5000; n++)
        {
            Assert.Equal(Legacy(n).Local, SearchBudget.Compute(n).Local);
        }
    }

    [Fact]
    public void Compute_OnlyEverLosesExternalsToTheCeiling()
    {
        // Min(floor, n) can only be smaller than the old flat 20, so the local target never
        // grows and the external target never shrinks on account of the split itself. The
        // one place the count can fall is the ceiling, which was lowered to the number of
        // rows a query actually builds — beyond that the old code was emitting placeholders
        // that no enrichment pass ever reached.
        for (var n = 0; n <= 5000; n++)
        {
            var expected = Math.Min(SearchBudget.ExternalCeiling, Legacy(n).External);
            Assert.True(SearchBudget.Compute(n).External >= expected,
                $"songCount={n} lost external rows beyond the ceiling");
        }
    }

    [Fact]
    public void Compute_KeepsBothTargetsInsideTheRequestedCount()
    {
        // The merge appends externals after locals with no total cap, so a client that
        // renders only the count it asked for would never see an external row if the two
        // targets summed past it.
        for (var n = 0; n <= 5000; n++)
        {
            var (local, external) = SearchBudget.Compute(n);
            Assert.True(local + external <= n, $"songCount={n} produced {local}+{external}");
        }
    }

    [Fact]
    public void Compute_TreatsNegativeCountsAsZero()
    {
        // The old expression sanitised these only by accident, via its flat floor. A
        // negative target would otherwise be relayed to Navidrome verbatim.
        Assert.Equal((0, 0), SearchBudget.Compute(-1));
        Assert.Equal((0, 0), SearchBudget.Compute(int.MinValue));
    }

    [Fact]
    public void Compute_DoesNotOverflowOnAbsurdCounts()
    {
        var (local, external) = SearchBudget.Compute(int.MaxValue);

        Assert.Equal(int.MaxValue / 4, local);
        Assert.Equal(SearchBudget.ExternalCeiling, external);
    }

    [Theory]
    // discoveryEnabled: false sends the whole request to local and drops external to
    // zero regardless of size, so the call site's Last.fm/Deezer fan-out never fires.
    [InlineData(0, 0)]
    [InlineData(5, 5)]
    [InlineData(20, 20)]
    [InlineData(1000, 1000)]
    public void Compute_DiscoveryDisabled_ReturnsAllSlotsLocal(int requested, int expectedLocal)
    {
        var (local, external) = SearchBudget.Compute(requested, discoveryEnabled: false);

        Assert.Equal(expectedLocal, local);
        Assert.Equal(0, external);
    }

    [Fact]
    public void Compute_DiscoveryDefaultsToEnabled()
    {
        // The three-arg and two-arg forms must agree, so a caller upgraded to pass the
        // flag explicitly can't silently change behaviour by getting the default wrong.
        Assert.Equal(SearchBudget.Compute(20), SearchBudget.Compute(20, discoveryEnabled: true));
    }
}
