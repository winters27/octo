using System.Net;
using System.Text;
using System.Text.Json;
using Microsoft.Extensions.Logging.Abstractions;
using Octo.Middleware;
using Octo.Models.Settings;
using Octo.Services.Soulseek;

namespace Octo.Tests;

/// <summary>
/// The Sharing card: what this server gives back to Soulseek, read from slskd's own answers. The
/// JSON below has the shape slskd 0.26.0 gave on the live box on 2026-10-04.
/// </summary>
public class SoulseekSharingTests
{
    private static readonly DateTime Now = new(2026, 10, 4, 21, 0, 0, DateTimeKind.Utc);
    private static readonly PortCheckResult PortOpen = new(PortState.Open, 50300, Now, null);

    // Logged in, sharing nothing, as LXC 111 was found.
    private const string AppSharingNothing = """
        {
          "server": { "state": "Connected, LoggedIn", "isConnected": true, "isLoggedIn": true },
          "user": { "username": "someone", "statistics": { "averageSpeed": 0, "directoryCount": 0, "fileCount": 0, "uploadCount": 0 } },
          "shares": { "scanPending": false, "scanning": false, "ready": true, "faulted": false, "scanProgress": 1, "directories": 0, "files": 0 }
        }
        """;

    private const string OptionsSharingNothing = """
        {
          "shares": { "directories": [], "filters": [], "cache": { "storageMode": "memory", "workers": 4 } },
          "transfers": { "upload": { "slots": 10, "speedLimit": 2147483647 } },
          "soulseek": { "listenPort": 50300 }
        }
        """;

    private const string AppSharing = """
        {
          "server": { "state": "Connected, LoggedIn", "isConnected": true, "isLoggedIn": true },
          "user": { "username": "someone", "statistics": { "directoryCount": 13, "fileCount": 2438 } },
          "shares": { "scanning": false, "scanPending": false, "faulted": false, "scanProgress": 1, "directories": 13, "files": 2438 }
        }
        """;

    private const string OptionsSharing = """
        {
          "shares": { "directories": ["[Music]/share"], "cache": { "retention": 1440 } },
          "transfers": { "upload": { "slots": 4, "speedLimit": 2048 } },
          "soulseek": { "listenPort": 50300 }
        }
        """;

    private const string SharesSharing = """
        { "local": [ { "id": "a", "alias": "Music", "isExcluded": false, "localPath": "/share", "raw": "[Music]/share", "remotePath": "Music", "directories": 13, "files": 2438 } ] }
        """;

    [Fact]
    public void SharingNothing_SaysSo()
    {
        var report = SoulseekSharing.Build(AppSharingNothing, OptionsSharingNothing, """{ "local": [] }""", "[]",
            PortOpen, defaultLogin: false, Now);

        Assert.True(report.Reachable);
        Assert.Equal("LoggedIn", report.Login);
        Assert.Empty(report.Folders);
        Assert.Equal(0, report.Files);
        Assert.Equal(10, report.UploadSlots);
        Assert.Null(report.UploadSpeedLimitKiB);
        Assert.Equal(50300, report.ListenPort);
        Assert.Equal(["nothingShared"], report.Warnings.Select(w => w.Code));
    }

    [Fact]
    public void ASharedLibrary_IsReportedWithItsLimits_AndNoWarning()
    {
        var report = SoulseekSharing.Build(AppSharing, OptionsSharing, SharesSharing, "[]", PortOpen, false, Now);

        var folder = Assert.Single(report.Folders);
        Assert.Equal(new SharedFolder("Music", "/share", false, 13, 2438), folder);
        Assert.Equal(2438, report.Files);
        Assert.Equal(2438, report.NetworkFiles);
        Assert.Equal(4, report.UploadSlots);
        Assert.Equal(2048, report.UploadSpeedLimitKiB);
        Assert.Equal(1440, report.RescanMinutes);
        Assert.Empty(report.Warnings);
    }

    [Fact]
    public void AClosedPort_AndTheDefaultLogin_AreWarnedAbout()
    {
        var closed = new PortCheckResult(PortState.Closed, 50300, Now, null);

        var report = SoulseekSharing.Build(AppSharing, OptionsSharing, SharesSharing, "[]", closed, defaultLogin: true, Now);

        Assert.Equal(["defaultLogin", "portClosed"], report.Warnings.Select(w => w.Code));
        Assert.Contains("50300", report.Warnings[1].Text);
        Assert.DoesNotContain('—', string.Concat(report.Warnings.Select(w => w.Text)));
    }

    [Fact]
    public void SignedOut_FailedScan_AndAShareWithNoFiles_AreEachWarnedAbout()
    {
        var signedOut = AppSharing.Replace("\"isLoggedIn\": true", "\"isLoggedIn\": false");
        Assert.Contains("signedOut", SoulseekSharing.Build(signedOut, OptionsSharing, SharesSharing, "[]", PortOpen, false, Now)
            .Warnings.Select(w => w.Code));

        var failed = AppSharing.Replace("\"faulted\": false", "\"faulted\": true");
        var failedReport = SoulseekSharing.Build(failed, OptionsSharing, SharesSharing, "[]", PortOpen, false, Now);
        Assert.True(failedReport.ScanFailed);
        Assert.Equal(["scanFailed"], failedReport.Warnings.Select(w => w.Code));

        var empty = AppSharing.Replace("\"files\": 2438", "\"files\": 0");
        Assert.Equal(["emptyShares"], SoulseekSharing.Build(empty, OptionsSharing, SharesSharing, "[]", PortOpen, false, Now)
            .Warnings.Select(w => w.Code));

        // An empty share being scanned is not empty yet.
        var scanning = empty.Replace("\"scanning\": false", "\"scanning\": true");
        Assert.Empty(SoulseekSharing.Build(scanning, OptionsSharing, SharesSharing, "[]", PortOpen, false, Now).Warnings);
    }

    [Fact]
    public void Unreachable_IsOneWarning_AndNoZeroCounts()
    {
        var report = SoulseekSharing.Build(null, null, null, null, PortOpen, false, Now);

        Assert.False(report.Reachable);
        Assert.Null(report.Files);
        Assert.Equal(["unreachable"], report.Warnings.Select(w => w.Code));
    }

    /// <summary>Without a shares answer the folders still come from the options, alias and
    /// exclusions included.</summary>
    [Fact]
    public void Folders_FallBackToTheOptions()
    {
        const string options = """{ "shares": { "directories": ["[Music]/share", "!/share/Private", "/data/More Music/"] } }""";

        var folders = SoulseekSharing.ParseFolders(null, options);

        Assert.Equal(
        [
            new SharedFolder("Music", "/share", false, null, null),
            new SharedFolder("Private", "/share/Private", true, null, null),
            new SharedFolder("More Music", "/data/More Music/", false, null, null),
        ], folders);
    }

    [Fact]
    public void Uploads_CountNowAndTheLastSevenDays()
    {
        var uploads = $$"""
            [
              { "username": "alice", "directories": [ { "directory": "Music", "files": [
                { "state": "InProgress", "size": 30000000 },
                { "state": "Queued, Locally", "size": 1 },
                { "state": "Completed, Succeeded", "size": 25000000, "endedAt": "{{Now.AddDays(-1):O}}" },
                { "state": "Completed, Succeeded", "size": 99, "endedAt": "{{Now.AddDays(-9):O}}" }
              ] } ] },
              { "username": "bob", "directories": [ { "directory": "Music", "files": [
                { "state": "Completed, Succeeded", "size": 5000000, "endedAt": "{{Now.AddHours(-2):O}}" },
                { "state": "Completed, Errored", "size": 1, "endedAt": "{{Now.AddHours(-3):O}}" },
                { "state": "Completed, Cancelled", "size": 1, "endedAt": "{{Now.AddHours(-3):O}}" }
              ] } ] }
            ]
            """;

        var activity = SoulseekSharing.ParseUploads(uploads, Now);

        Assert.Equal(1, activity.Sending);
        Assert.Equal(1, activity.Waiting);
        Assert.Equal(2, activity.Files);
        Assert.Equal(30_000_000, activity.Bytes);
        Assert.Equal(2, activity.People);
        Assert.Equal(1, activity.Failed);
        Assert.Equal(Now.AddHours(-2), activity.LastUploadAt);
    }

    [Theory]
    [InlineData("IP: 203.0.113.9 Port: 50300/tcp OPEN. Congratulations", PortState.Open)]
    [InlineData("IP: 203.0.113.9 Port: 50300/tcp CLOSED. Your router", PortState.Closed)]
    [InlineData("IP: 203.0.113.9 Port: 2234/tcp OPEN.", PortState.Unknown)]
    [InlineData("<html>maintenance</html>", PortState.Unknown)]
    public void PortTestAnswer_IsRead(string page, PortState expected) =>
        Assert.Equal(expected, SoulseekPortCheck.ParseAnswer(page, 50300));

    [Fact]
    public async Task PortTest_IsKeptSixHours_AndTestAsksAgainAtMostEvery30Seconds()
    {
        var handler = new CountingHandler("Port: 50300/tcp CLOSED.");
        var clock = Now;
        var check = new SoulseekPortCheck(new ReviewFixtures.OneClientFactory(handler),
            TestOptions.Monitor(new SoulseekSettings { CheckListenPort = true }), NullLogger<SoulseekPortCheck>.Instance)
        { Clock = () => clock };

        Assert.Equal(PortState.Closed, (await check.CheckAsync(50300, force: false, default)).State);
        Assert.Equal("http://tools.slsknet.org/porttest.php?port=50300", handler.LastUrl);
        await check.CheckAsync(50300, force: false, default);
        await check.CheckAsync(50300, force: true, default);
        Assert.Equal(1, handler.Calls);

        clock = Now.AddSeconds(31);
        handler.Answer = "Port: 50300/tcp OPEN.";
        Assert.Equal(PortState.Open, (await check.CheckAsync(50300, force: true, default)).State);
        Assert.Equal(2, handler.Calls);

        clock = Now.AddHours(7);
        await check.CheckAsync(50300, force: false, default);
        Assert.Equal(3, handler.Calls);
    }

    [Fact]
    public async Task PortTest_TurnedOff_NeverAsks()
    {
        var handler = new CountingHandler("Port: 50300/tcp OPEN.");
        var check = new SoulseekPortCheck(new ReviewFixtures.OneClientFactory(handler),
            TestOptions.Monitor(new SoulseekSettings { CheckListenPort = false }), NullLogger<SoulseekPortCheck>.Instance);

        Assert.Equal(PortState.Off, (await check.CheckAsync(50300, force: true, default)).State);
        Assert.Equal(0, handler.Calls);
    }

    [Fact]
    public async Task PortTest_ThatFails_IsUnknownNotClosed()
    {
        var handler = new CountingHandler("") { Fail = true };
        var check = new SoulseekPortCheck(new ReviewFixtures.OneClientFactory(handler),
            TestOptions.Monitor(new SoulseekSettings { CheckListenPort = true }), NullLogger<SoulseekPortCheck>.Instance);

        var result = await check.CheckAsync(50300, force: false, default);

        Assert.Equal(PortState.Unknown, result.State);
        Assert.NotNull(result.Error);
    }

    [Theory]
    [InlineData("http://192.168.1.5:5030", "http://192.168.1.5:5030/")]
    [InlineData(" https://slskd.example.com/ ", "https://slskd.example.com/")]
    [InlineData("javascript:alert(1)", null)]
    [InlineData("file:///etc/passwd", null)]
    [InlineData("slskd:5030", null)]
    [InlineData("", null)]
    public void WebUrl_OnlyHttpAddressesAreUsed(string value, string? expected) =>
        Assert.Equal(expected, SoulseekSettings.SafeWebUrl(value));

    [Fact]
    public async Task SavingAWebUrl_KeepsOnlyWebAddresses()
    {
        await using var factory = new ScrobbleAdminFactory();
        using var client = factory.AdminClient();

        using var bad = await client.PostAsync("/api/admin/settings", new StringContent(
            """{ "Soulseek": { "WebUrl": "javascript:alert(1)" } }""", Encoding.UTF8, "application/json"));
        Assert.Equal(HttpStatusCode.BadRequest, bad.StatusCode);

        using var good = await client.PostAsync("/api/admin/settings", new StringContent(
            """{ "Soulseek": { "WebUrl": "http://192.168.1.5:5030" } }""", Encoding.UTF8, "application/json"));
        good.EnsureSuccessStatusCode();

        using var blank = await client.PostAsync("/api/admin/settings", new StringContent(
            """{ "Soulseek": { "WebUrl": "" } }""", Encoding.UTF8, "application/json"));
        blank.EnsureSuccessStatusCode();
    }

    /// <summary>Through the real app, with slskd at a closed port: the card says slskd is out of
    /// reach and asks nobody about the port, and the rescan is a guarded write.</summary>
    [Fact]
    public async Task Endpoints_WithSlskdOutOfReach()
    {
        using var factory = new AdminWebFactory();
        using var client = factory.CreateClient();

        var body = await client.GetStringAsync("/api/admin/soulseek/sharing");
        Assert.DoesNotContain("synthetic-slskd-password", body);
        var report = JsonDocument.Parse(body).RootElement;
        Assert.False(report.GetProperty("reachable").GetBoolean());
        Assert.Equal("unreachable", report.GetProperty("warnings")[0].GetProperty("code").GetString());
        // The dashboard reads the port's state by name.
        Assert.Equal("Off", report.GetProperty("port").GetProperty("state").GetString());

        using var unguarded = await client.PostAsync("/api/admin/soulseek/sharing/rescan", null);
        Assert.Equal(HttpStatusCode.Forbidden, unguarded.StatusCode);

        client.DefaultRequestHeaders.Add(AdminRequestGuard.HeaderName, "1");
        using var rescan = await client.PostAsync("/api/admin/soulseek/sharing/rescan", null);
        Assert.Equal(HttpStatusCode.Conflict, rescan.StatusCode);
        Assert.Contains("cannot reach slskd", await rescan.Content.ReadAsStringAsync());
    }

    private sealed class CountingHandler(string answer) : HttpMessageHandler
    {
        public string Answer { get; set; } = answer;
        public bool Fail { get; init; }
        public int Calls { get; private set; }
        public string? LastUrl { get; private set; }

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            Calls++;
            LastUrl = request.RequestUri?.ToString();
            if (Fail) throw new HttpRequestException("no route");
            return Task.FromResult(new HttpResponseMessage(HttpStatusCode.OK)
            {
                Content = new StringContent(Answer, Encoding.UTF8, "text/html"),
            });
        }
    }
}
