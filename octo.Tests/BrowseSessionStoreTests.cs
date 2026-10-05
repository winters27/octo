using Octo.Services.Admin;

namespace Octo.Tests;

/// <summary>
/// This token is the only thing standing between the browse endpoint and anyone who
/// can reach Octo's port, so the interesting cases are the ones where a naive store
/// says yes: an empty token, an unknown token, or one that should have lapsed.
/// </summary>
public class BrowseSessionStoreTests
{
    [Fact]
    public void AMintedTokenValidates()
    {
        var store = new BrowseSessionStore();

        Assert.True(store.Validate(store.Create("winters")));
    }

    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("   ")]
    [InlineData("not-a-real-token")]
    public void AnythingNotMintedByUsIsRejected(string? token)
    {
        var store = new BrowseSessionStore();
        store.Create("winters"); // a live session must not make other tokens valid

        Assert.False(store.Validate(token));
    }

    [Fact]
    public void TokensAreUnpredictableAndNotSharedBetweenSessions()
    {
        var store = new BrowseSessionStore();

        var first = store.Create("winters");
        var second = store.Create("winters");

        Assert.NotEqual(first, second);
        // 32 bytes hex. Guessing is not meant to be on the table.
        Assert.Equal(64, first.Length);
    }

    private static string TempFile() =>
        Path.Combine(Path.GetTempPath(), "octo-browse-" + Guid.NewGuid().ToString("N"), "browse-sessions.json");

    [Fact]
    public void ASignInSurvivesARestart()
    {
        var path = TempFile();
        try
        {
            var token = new BrowseSessionStore(path).Create("winters");
            var afterRestart = new BrowseSessionStore(path);
            Assert.Equal("winters", afterRestart.UserOf(token));
        }
        finally { Directory.Delete(Path.GetDirectoryName(path)!, recursive: true); }
    }

    [Fact]
    public void TheFileHoldsNoTokenABrowserCouldPresent()
    {
        var path = TempFile();
        try
        {
            var token = new BrowseSessionStore(path).Create("winters");
            var written = File.ReadAllText(path);
            Assert.DoesNotContain(token, written, StringComparison.OrdinalIgnoreCase);
            Assert.Contains("winters", written);
        }
        finally { Directory.Delete(Path.GetDirectoryName(path)!, recursive: true); }
    }

    [Fact]
    public void ABrowserInUseStaysSignedIn_AnUnusedOneLapsesAfterNinetyDays()
    {
        var now = new DateTime(2026, 10, 3, 12, 0, 0, DateTimeKind.Utc);
        var store = new BrowseSessionStore { Clock = () => now };
        var token = store.Create("winters");

        for (var day = 0; day < 200; day += 30)
        {
            now = now.AddDays(30);
            Assert.True(store.Validate(token), $"lapsed while in use, day {day + 30}");
        }

        now = now.AddDays(91);
        Assert.False(store.Validate(token));
        Assert.Null(store.UserOf(token));
    }

    [Fact]
    public void SignOutForgetsTheBrowser_AndStaysForgottenAfterARestart()
    {
        var path = TempFile();
        try
        {
            var store = new BrowseSessionStore(path);
            var token = store.Create("winters");
            var other = store.Create("winters");
            store.Revoke(token);
            Assert.False(store.Validate(token));
            Assert.True(store.Validate(other));
            Assert.False(new BrowseSessionStore(path).Validate(token));
        }
        finally { Directory.Delete(Path.GetDirectoryName(path)!, recursive: true); }
    }

    [Fact]
    public void AnUnreadableFileMeansSigningInAgainNotACrash()
    {
        var path = TempFile();
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        try
        {
            File.WriteAllText(path, "{ not json");
            var store = new BrowseSessionStore(path);
            Assert.False(store.Validate("anything"));
            Assert.True(store.Validate(store.Create("winters")));
        }
        finally { Directory.Delete(Path.GetDirectoryName(path)!, recursive: true); }
    }

    [Fact]
    public void ARecoverySignInLapsesAfterAnHour_ANavidromeOneDoesNot()
    {
        var now = DateTime.UtcNow;
        var store = new BrowseSessionStore { Clock = () => now };
        var recovery = store.CreateRecovery();
        var navidrome = store.Create("winters");

        now = now.Add(BrowseSessionStore.RecoveryTtl).AddMinutes(1);

        Assert.Null(store.UserOf(recovery));
        Assert.Equal("winters", store.UserOf(navidrome));
    }

    [Fact]
    public void ARecoverySignInNeverCountsAsANavidromeAdmin()
    {
        var store = new BrowseSessionStore();
        var recovery = store.CreateRecovery();

        Assert.Equal(BrowseSessionStore.RecoveryUser, store.UserOf(recovery));
        Assert.Null(store.NavidromeUserOf(recovery));
        Assert.Equal("winters", store.NavidromeUserOf(store.Create("winters")));
    }

    [Fact]
    public void RevokeUserEndsEverySignInOfThatPersonAndNoOneElse()
    {
        var store = new BrowseSessionStore();
        var phone = store.Create("winters");
        var laptop = store.Create("Winters");
        var someoneElse = store.Create("guest");

        Assert.Equal(2, store.RevokeUser("WINTERS"));

        Assert.Null(store.UserOf(phone));
        Assert.Null(store.UserOf(laptop));
        Assert.Equal("guest", store.UserOf(someoneElse));
    }
}
