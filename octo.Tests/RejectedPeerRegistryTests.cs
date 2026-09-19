using Octo.Services.Soulseek;

namespace Octo.Tests;

/// <summary>
/// The deny-list is what makes a rejection worth anything. Without it a rejected download
/// deleted the bytes and threw the fact away, so the next star re-ran the same search,
/// ranked the same peer first for the same reasons, and paid for the same wrong file again.
/// </summary>
public class RejectedPeerRegistryTests
{
    private const string File1 = @"MyMusic\Mark Morrison\Return of the Mack\05 Return of the Mack.flac";
    private const string File2 = @"MyMusic\Mark Morrison\Return of the Mack\06 Horny.flac";

    [Fact]
    public void IsDenied_AfterDeny_BlocksThatExactPeerAndFile()
    {
        var registry = new RejectedPeerRegistry();
        registry.Deny("peer1", File1, "is a karaoke version", "Mark Morrison - Return of the Mack");

        Assert.True(registry.IsDenied("peer1", File1));
        Assert.Equal(1, registry.Count);
    }

    /// <summary>
    /// Denying a peer wholesale would blacklist an entire well-stocked library over one bad
    /// rip, which costs far more than the one file it saves.
    /// </summary>
    [Fact]
    public void IsDenied_SamePeerDifferentFile_IsStillAllowed()
    {
        var registry = new RejectedPeerRegistry();
        registry.Deny("peer1", File1, "wrong recording", "A - B");

        Assert.False(registry.IsDenied("peer1", File2));
        Assert.False(registry.IsDenied("peer2", File1));
    }

    /// <summary>
    /// slskd echoes the peer's own path, and the casing differs between a search response and
    /// a transfer record. Two distinct files on one peer never differ by case alone.
    /// </summary>
    [Fact]
    public void IsDenied_CasingVariesBetweenSearchAndTransfer_StillMatches()
    {
        var registry = new RejectedPeerRegistry();
        registry.Deny("Peer1", File1, "wrong recording", "A - B");

        Assert.True(registry.IsDenied("peer1", File1.ToUpperInvariant()));
    }

    /// <summary>
    /// A deny-list with no expiry turns one false positive into a track that can never be
    /// fetched again, with nothing in the UI saying why.
    /// </summary>
    [Theory]
    [InlineData(31, 30, true)]
    [InlineData(30, 30, true)]
    [InlineData(29, 30, false)]
    [InlineData(1, 30, false)]
    [InlineData(8, 7, true)]
    [InlineData(6, 7, false)]
    public void IsExpired_LapsesAtTheConfiguredAge(int ageDays, int ttlDays, bool expired)
    {
        var now = new DateTime(2026, 9, 15, 12, 0, 0, DateTimeKind.Utc);
        Assert.Equal(expired, RejectedPeerRegistry.IsExpired(now.AddDays(-ageDays), now, ttlDays));
    }

    /// <summary>
    /// 0 is a real choice, not a disabled feature: it suits anyone who would rather clear the
    /// list by hand than have denials lapse on their own.
    /// </summary>
    [Fact]
    public void IsExpired_ZeroDays_NeverLapses()
    {
        var now = new DateTime(2026, 9, 15, 12, 0, 0, DateTimeKind.Utc);
        Assert.False(RejectedPeerRegistry.IsExpired(now.AddYears(-5), now, ttlDays: 0));
    }

    [Fact]
    public void Deny_SurvivesARestart()
    {
        var path = Path.Combine(Path.GetTempPath(), "octo-denylist-" + Guid.NewGuid() + ".json");
        try
        {
            using (var first = new RejectedPeerRegistry(path))
            {
                first.Deny("peer1", File1, "is a karaoke version", "A - B");
            }

            using var second = new RejectedPeerRegistry(path);
            Assert.True(second.IsDenied("peer1", File1));
        }
        finally { try { System.IO.File.Delete(path); } catch { } }
    }

    /// <summary>
    /// Clearing flushes synchronously. A user who clears the list and immediately restarts the
    /// container must not get every entry back, which is what "cleared" would otherwise mean
    /// for the ten seconds until the coalesced flush fires.
    /// </summary>
    [Fact]
    public void Clear_TakesEffectBeforeTheNextFlushTick()
    {
        var path = Path.Combine(Path.GetTempPath(), "octo-denylist-" + Guid.NewGuid() + ".json");
        try
        {
            using (var first = new RejectedPeerRegistry(path))
            {
                first.Deny("peer1", File1, "wrong recording", "A - B");
                first.Deny("peer2", File2, "wrong recording", "A - B");
                Assert.Equal(2, first.Clear());
            }

            using var second = new RejectedPeerRegistry(path);
            Assert.Equal(0, second.Count);
        }
        finally { try { System.IO.File.Delete(path); } catch { } }
    }

    /// <summary>A registry that will not load is a cold start, not a failure to boot.</summary>
    [Fact]
    public void Load_UnreadableFile_StartsEmptyRatherThanThrowing()
    {
        var path = Path.Combine(Path.GetTempPath(), "octo-denylist-" + Guid.NewGuid() + ".json");
        System.IO.File.WriteAllText(path, "{ this is not the shape we wrote }");
        try
        {
            using var registry = new RejectedPeerRegistry(path);
            Assert.Equal(0, registry.Count);
            Assert.False(registry.IsDenied("peer1", File1));
        }
        finally { try { System.IO.File.Delete(path); } catch { } }
    }

    [Fact]
    public void Deny_BlankPeerOrFile_IsIgnored()
    {
        var registry = new RejectedPeerRegistry();
        registry.Deny("", File1, "r", "t");
        registry.Deny("peer1", "", "r", "t");

        Assert.Equal(0, registry.Count);
        Assert.False(registry.IsDenied("", File1));
    }
}
