using System.Net;
using System.Net.Http.Json;
using System.Text.Json;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Logging.Abstractions;
using Octo.Models.Settings;
using Octo.Services.Updates;

namespace Octo.Tests;

/// <summary>
/// The files that hand Update now to the host helper. Octo can only ask, by writing a request
/// naming a release; the helper writes back what it did. A request nobody picks up is dropped, so
/// a helper installed later never runs it by surprise.
/// </summary>
public sealed class UpdateHostTests : IDisposable
{
    private readonly string _dir = Path.Combine(Path.GetTempPath(), "octo-update-host-" + Guid.NewGuid());
    private DateTime _now = new(2026, 10, 3, 12, 0, 0, DateTimeKind.Utc);

    public void Dispose()
    {
        try { Directory.Delete(_dir, true); } catch { }
    }

    private UpdateHost Host() => new(_dir, NullLogger<UpdateHost>.Instance, () => _now);

    private void WriteFile(string name, params string[] lines)
    {
        Directory.CreateDirectory(_dir);
        File.WriteAllLines(Path.Combine(_dir, name), lines);
    }

    [Fact]
    public void ARequestNamesTheReleaseAndWhoAsked()
    {
        var id = Host().Request("2026.10.04", "winters");

        var lines = File.ReadAllLines(Path.Combine(_dir, "request"));
        Assert.Contains($"id={id}", lines);
        Assert.Contains("tag=2026.10.04", lines);
        Assert.Contains("by=winters", lines);
        Assert.Contains("at=2026-10-03T12:00:00Z", lines);
        Assert.False(File.Exists(Path.Combine(_dir, "request.tmp")));
    }

    [Theory]
    [InlineData("2026.10.04; rm -rf /")]
    [InlineData("main")]
    [InlineData("desktop-v1.3.2")]
    [InlineData("2026.10.04+abc")]
    public void OnlyAReleaseCanBeRequested(string tag)
    {
        Assert.Throws<ArgumentException>(() => Host().Request(tag, "winters"));
        Assert.False(File.Exists(Path.Combine(_dir, "request")));
    }

    [Fact]
    public void AnOddUserNameIsNotWrittenIntoTheRequest()
    {
        Host().Request("2026.10.04", "bad\nstate=done");

        var lines = File.ReadAllLines(Path.Combine(_dir, "request"));
        Assert.Contains("by=dashboard", lines);
        Assert.DoesNotContain(lines, line => line.StartsWith("state="));
    }

    [Fact]
    public void ARequestWaitsForTheHelperAndIsDroppedWhenNobodyAnswers()
    {
        var host = Host();
        var id = host.Request("2026.10.04", "winters");

        _now = _now.AddSeconds(89);
        Assert.Equal(id, host.Pending());
        Assert.True(host.Busy());

        _now = _now.AddSeconds(2);
        Assert.Null(host.Pending());
        Assert.Equal(id, host.Unanswered);
        Assert.False(File.Exists(Path.Combine(_dir, "request")));
        Assert.False(host.Busy());
    }

    [Fact]
    public void ARequestTheHelperAnsweredIsNoLongerPending()
    {
        var host = Host();
        var id = host.Request("2026.10.04", "winters");
        WriteFile("status", $"id={id}", "tag=2026.10.04", "state=accepted", "started=2026-10-03T12:00:01Z");

        Assert.Null(host.Pending());
        Assert.True(host.Busy());
        Assert.Null(host.Unanswered);
    }

    [Fact]
    public void ARunThatStoppedReportingIsNotBusyForever()
    {
        WriteFile("status", "id=a", "tag=2026.10.04", "state=building", "started=2026-10-03T11:00:00Z");

        Assert.False(Host().Busy());
    }

    [Fact]
    public void AFinishedRunIsNotBusy()
    {
        WriteFile("status", "id=a", "tag=2026.10.04", "state=done", "started=2026-10-03T11:59:00Z", "finished=2026-10-03T11:59:50Z");

        Assert.False(Host().Busy());
    }

    [Fact]
    public void TheHelperDescribesItself()
    {
        Assert.Null(Host().Helper());

        WriteFile("helper", "version=1", "mode=image", "dir=/opt/octo", "installed=2026-10-03T10:00:00Z");
        var helper = Host().Helper()!;

        Assert.Equal("image", helper.Mode);
        Assert.Equal("/opt/octo", helper.Dir);
        Assert.Equal(new DateTime(2026, 10, 3, 10, 0, 0, DateTimeKind.Utc), helper.InstalledUtc);
    }

    [Fact]
    public void AnUnknownModeIsTheBuiltFromSourceInstall()
    {
        WriteFile("helper", "version=1", "mode=something");

        Assert.Equal("build", Host().Helper()!.Mode);
    }

    [Fact]
    public void AVersionTwoHelperInAGitCloneIsCurrent()
    {
        WriteFile("helper", "version=2", "mode=git", "dir=/opt/octo");
        var helper = Host().Helper()!;

        Assert.Equal("git", helper.Mode);
        Assert.False(helper.Outdated);
    }

    [Theory]
    [InlineData("version=1")]
    [InlineData("version=")]
    [InlineData("version=two")]
    [InlineData("mode=build")]
    public void AHelperFromBeforeVersionTwoIsOutdated(string line)
    {
        // Version 1 picked image mode once Octo was pulled, pulled the release its old compose
        // file named, and reported the update done. A file without a version is version 1's.
        WriteFile("helper", line, "dir=/opt/octo");

        Assert.True(Host().Helper()!.Outdated);
    }

    [Fact]
    public void TheStatusReadsBackWithItsTimes()
    {
        WriteFile("status", "id=a", "tag=2026.10.04", "from=2026.10.01", "state=failed", "step=Building Octo 2026.10.04",
            "error=The build failed, so nothing was restarted.", "started=2026-10-03T11:59:00Z", "finished=2026-10-03T12:01:00Z");

        var status = Host().Status()!;

        Assert.Equal("failed", status.State);
        Assert.Equal("2026.10.01", status.From);
        Assert.Equal("The build failed, so nothing was restarted.", status.Error);
        Assert.Equal(new DateTime(2026, 10, 3, 12, 1, 0, DateTimeKind.Utc), status.FinishedUtc);
    }

    [Fact]
    public void LinesWithoutAKeyAreIgnoredAndValuesMayHoldEquals()
    {
        var values = UpdateHost.Parse(["junk", "=nokey", "error=a=b", " tag = 2026.10.04 "])!;

        Assert.Equal("a=b", values["error"]);
        Assert.Equal("2026.10.04", values["tag"]);
        Assert.Equal(2, values.Count);
    }
}

/// <summary>The update card's API, with a release check that knows a newer release.</summary>
public sealed class UpdateEndpointTests : IDisposable
{
    private readonly string _dir = Path.Combine(Path.GetTempPath(), "octo-update-api-" + Guid.NewGuid());
    private readonly ReleaseCheckTests.GitHub _github = new();

    public void Dispose()
    {
        try { Directory.Delete(_dir, true); } catch { }
    }

    private string UpdateDir => Path.Combine(_dir, "update");

    private async Task<(WebApplicationFactory<Program> Factory, HttpClient Client)> StartAsync(string running = "2026.10.01")
    {
        Directory.CreateDirectory(UpdateDir);
        _github.Answer(HttpStatusCode.OK, ReleaseCheckTests.Releases);
        var check = new ReleaseCheck(Path.Combine(UpdateDir, "release.json"), new ReviewFixtures.OneClientFactory(_github),
            TestOptions.Monitor(new UpdateSettings()), NullLogger<ReleaseCheck>.Instance, running: running);
        await check.CheckAsync(manual: false, CancellationToken.None);
        var host = new UpdateHost(UpdateDir, NullLogger<UpdateHost>.Instance);

        var factory = new WebApplicationFactory<Program>().WithWebHostBuilder(builder =>
        {
            builder.UseEnvironment("Development");
            builder.ConfigureAppConfiguration(config => config.AddInMemoryCollection(new Dictionary<string, string?>
            {
                ["Library:DownloadPath"] = _dir,
                ["Subsonic:Url"] = "http://127.0.0.1:1",
                ["Soulseek:BaseUrl"] = "http://127.0.0.1:1",
                ["YouTube:ShimUrl"] = "http://127.0.0.1:1",
            }));
            builder.ConfigureServices(services =>
            {
                services.RemoveAll<ReleaseCheck>();
                services.AddSingleton(check);
                services.RemoveAll<UpdateHost>();
                services.AddSingleton(host);
                services.RemoveAll<Octo.Services.Admin.BrowseSessionStore>();
                services.AddSingleton(new Octo.Services.Admin.BrowseSessionStore());
            });
        });
        var client = factory.CreateClient();
        client.DefaultRequestHeaders.Add("X-Octo-Admin", "1");
        return (factory, client);
    }

    private void Helper(string version = "2", string mode = "git") =>
        File.WriteAllLines(Path.Combine(UpdateDir, "helper"), [$"version={version}", $"mode={mode}", "dir=/opt/octo"]);

    private static async Task<JsonElement> Json(HttpResponseMessage response) =>
        JsonDocument.Parse(await response.Content.ReadAsStringAsync()).RootElement;

    [Fact]
    public async Task TheCardSaysWhatIsNewAndWhatToRun()
    {
        var (factory, client) = await StartAsync();
        using var _ = factory;

        var view = await Json(await client.GetAsync("/api/admin/update"));

        Assert.Equal("2026.10.01", view.GetProperty("running").GetString());
        Assert.True(view.GetProperty("updateAvailable").GetBoolean());
        Assert.Equal("behind", view.GetProperty("standing").GetString());
        Assert.Equal("2026.10.02.1", view.GetProperty("latest").GetProperty("tag").GetString());
        Assert.Equal(2, view.GetProperty("newer").GetArrayLength());
        Assert.False(view.GetProperty("helper").GetProperty("installed").GetBoolean());
        Assert.Equal("git fetch --tags && git checkout --detach 2026.10.02.1"
            + " && (docker compose pull octo yt-dlp-shim octo-sonic"
            + " || docker compose --profile source build octo-source yt-dlp-shim-source octo-sonic-source)"
            + " && docker compose up -d",
            view.GetProperty("command").GetString());
        Assert.Equal("docker compose pull octo yt-dlp-shim octo-sonic && docker compose up -d octo yt-dlp-shim octo-sonic",
            view.GetProperty("imageCommand").GetString());
    }

    [Fact]
    public async Task WithoutTheHelperNothingIsRequested()
    {
        var (factory, client) = await StartAsync();
        using var _ = factory;

        var response = await client.PostAsJsonAsync("/api/admin/update", new { tag = "2026.10.02.1" });

        Assert.Equal(HttpStatusCode.Conflict, response.StatusCode);
        Assert.Contains("helper", (await Json(response)).GetProperty("error").GetString());
        Assert.False(File.Exists(Path.Combine(UpdateDir, "request")));
    }

    [Fact]
    public async Task OnlyTheNewestReleaseCanBeAskedFor()
    {
        var (factory, client) = await StartAsync();
        using var _ = factory;
        Helper();

        var response = await client.PostAsJsonAsync("/api/admin/update", new { tag = "2026.10.02" });

        Assert.Equal(HttpStatusCode.Conflict, response.StatusCode);
        Assert.False(File.Exists(Path.Combine(UpdateDir, "request")));
    }

    [Fact]
    public async Task UpdateNowHandsTheReleaseToTheHelperOnce()
    {
        var (factory, client) = await StartAsync();
        using var _ = factory;
        Helper();

        var response = await client.PostAsJsonAsync("/api/admin/update", new { tag = "2026.10.02.1" });

        Assert.Equal(HttpStatusCode.Accepted, response.StatusCode);
        var id = (await Json(response)).GetProperty("id").GetString();
        var lines = File.ReadAllLines(Path.Combine(UpdateDir, "request"));
        Assert.Contains($"id={id}", lines);
        Assert.Contains("tag=2026.10.02.1", lines);
        Assert.Contains("by=dashboard", lines);

        var view = await Json(await client.GetAsync("/api/admin/update"));
        Assert.True(view.GetProperty("pending").GetBoolean());

        var again = await client.PostAsJsonAsync("/api/admin/update", new { tag = "2026.10.02.1" });
        Assert.Equal(HttpStatusCode.Conflict, again.StatusCode);
    }

    [Fact]
    public async Task AnOldHelperIsNotAskedAndTheCardSaysToReinstallIt()
    {
        var (factory, client) = await StartAsync();
        using var _ = factory;
        // The helper of 2026.10.05 and before, left in place when reinstalling it failed.
        Helper(version: "1", mode: "build");

        var response = await client.PostAsJsonAsync("/api/admin/update", new { tag = "2026.10.02.1" });

        Assert.Equal(HttpStatusCode.Conflict, response.StatusCode);
        Assert.Contains("scripts/updater/install-updater.sh", (await Json(response)).GetProperty("error").GetString());
        Assert.False(File.Exists(Path.Combine(UpdateDir, "request")));
        var view = await Json(await client.GetAsync("/api/admin/update"));
        Assert.True(view.GetProperty("helper").GetProperty("outdated").GetBoolean());
        Assert.Equal("scripts/updater/install-updater.sh", view.GetProperty("reinstallCommand").GetString());
    }

    [Fact]
    public async Task TheCurrentHelperIsNotCalledOutdated()
    {
        var (factory, client) = await StartAsync();
        using var _ = factory;
        Helper();

        var helper = (await Json(await client.GetAsync("/api/admin/update"))).GetProperty("helper");

        Assert.False(helper.GetProperty("outdated").GetBoolean());
        Assert.Equal("git", helper.GetProperty("mode").GetString());
    }

    [Fact]
    public async Task NothingIsNewerSoNothingIsRequested()
    {
        var (factory, client) = await StartAsync(running: "2026.10.02.1");
        using var _ = factory;
        Helper();

        var response = await client.PostAsJsonAsync("/api/admin/update", new { tag = "2026.10.02.1" });

        Assert.Equal(HttpStatusCode.Conflict, response.StatusCode);
    }

    [Fact]
    public async Task APageOnAnotherSiteCannotStartAnUpdate()
    {
        var (factory, client) = await StartAsync();
        using var _ = factory;
        Helper();
        client.DefaultRequestHeaders.Remove("X-Octo-Admin");

        var response = await client.PostAsJsonAsync("/api/admin/update", new { tag = "2026.10.02.1" });

        Assert.Equal(HttpStatusCode.Forbidden, response.StatusCode);
        Assert.False(File.Exists(Path.Combine(UpdateDir, "request")));
    }

    [Fact]
    public async Task OctoBackOnTheReleaseMeansTheRunFinished()
    {
        var (factory, client) = await StartAsync(running: "2026.10.02.1");
        using var _ = factory;
        File.WriteAllLines(Path.Combine(UpdateDir, "status"),
            ["id=a", "tag=2026.10.02.1", "from=2026.10.01", "state=restarting", "step=Restarting Octo", $"started={DateTime.UtcNow:yyyy-MM-ddTHH:mm:ssZ}"]);

        var run = (await Json(await client.GetAsync("/api/admin/update"))).GetProperty("run");

        Assert.Equal("done", run.GetProperty("state").GetString());
    }

    [Fact]
    public async Task AFailedRunCarriesTheEndOfTheLog()
    {
        var (factory, client) = await StartAsync();
        using var _ = factory;
        File.WriteAllLines(Path.Combine(UpdateDir, "status"),
            ["id=a", "tag=2026.10.02.1", "state=failed", "error=The build failed, so nothing was restarted.", $"started={DateTime.UtcNow:yyyy-MM-ddTHH:mm:ssZ}"]);
        File.WriteAllLines(Path.Combine(UpdateDir, "log"), Enumerable.Range(1, 60).Select(i => $"line {i}"));

        var run = (await Json(await client.GetAsync("/api/admin/update"))).GetProperty("run");

        Assert.Equal("failed", run.GetProperty("state").GetString());
        var log = run.GetProperty("log").EnumerateArray().Select(line => line.GetString()).ToList();
        Assert.Equal(40, log.Count);
        Assert.Equal("line 60", log[^1]);
    }
}
