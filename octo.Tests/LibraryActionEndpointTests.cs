using System.Net;
using System.Text;
using System.Text.Json;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Octo.Models.Settings;
using Octo.Services.Library;

namespace Octo.Tests;

/// <summary>
/// octoLibraryActions as the Octo app sees it: getLibraryActions says what the caller may do, and
/// libraryAction removes a song through the same executor the Delete playlist uses, checked
/// against Navidrome with the caller's own credentials. Nothing here goes through a rating.
/// </summary>
public sealed class LibraryActionEndpointTests
{
    private const string SongId = "nd-teardrop";
    private const string SongPath = "Massive Attack/Mezzanine/03 - Teardrop.flac";
    private static readonly byte[] SongBytes = Encoding.UTF8.GetBytes("fLaC not really, but the right size");

    /// <summary>
    /// Navidrome as far as these calls need it: a ping that accepts the token "good" or the API
    /// key "goodkey", an admin login for Octo's own identity, one song the native API knows, and
    /// a short extension list in either format.
    /// </summary>
    private sealed class FakeNavidrome(string libraryPath) : HttpMessageHandler
    {
        public int Pings;
        public int SongLookups;
        public int UserLookups;

        /// <summary>Whether getUser says the caller is an admin. Removing needs it.</summary>
        public bool Admin = true;

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var uri = request.RequestUri!;
            var query = System.Web.HttpUtility.ParseQueryString(uri.Query);
            if (uri.AbsolutePath.EndsWith("/rest/ping", StringComparison.Ordinal))
            {
                Interlocked.Increment(ref Pings);
                return Task.FromResult(Json(query["t"] == "good" || query["apiKey"] == "goodkey"
                    ? """{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome"}}"""
                    : """{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}"""));
            }
            if (uri.AbsolutePath.EndsWith("/rest/getUser", StringComparison.Ordinal))
            {
                Interlocked.Increment(ref UserLookups);
                return Task.FromResult(Json(JsonSerializer.Serialize(new Dictionary<string, object>
                {
                    ["subsonic-response"] = new { status = "ok", version = "1.16.1", user = new { username = query["username"], adminRole = Admin } },
                })));
            }
            if (uri.AbsolutePath == "/auth/login")
                return Task.FromResult(Json(
                    """{"token":"admin-jwt","isAdmin":true,"username":"admin","subsonicToken":"st","subsonicSalt":"ss"}"""));
            if (uri.AbsolutePath.StartsWith("/api/song/", StringComparison.Ordinal)
                || uri.AbsolutePath.EndsWith("/rest/getSong", StringComparison.Ordinal))
            {
                Interlocked.Increment(ref SongLookups);
                if (uri.AbsolutePath == $"/api/song/{SongId}")
                    return Task.FromResult(Json(JsonSerializer.Serialize(new
                    {
                        id = SongId, path = SongPath, libraryPath, size = SongBytes.Length, suffix = "flac",
                        title = "Teardrop", artist = "Massive Attack", album = "Mezzanine", duration = 330,
                    })));
                return Task.FromResult(new HttpResponseMessage(HttpStatusCode.NotFound));
            }
            if (uri.AbsolutePath.EndsWith("/rest/getOpenSubsonicExtensions", StringComparison.Ordinal))
            {
                return Task.FromResult(query["f"] == "json"
                    ? Json("""{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.58.0","openSubsonic":true,"openSubsonicExtensions":[{"name":"formPost","versions":[1]}]}}""")
                    : new HttpResponseMessage(HttpStatusCode.OK)
                    {
                        Content = new StringContent(
                            """<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1" type="navidrome"><openSubsonicExtensions name="formPost"><versions>1</versions></openSubsonicExtensions></subsonic-response>""",
                            Encoding.UTF8, "application/xml"),
                    });
            }
            return Task.FromResult(new HttpResponseMessage(HttpStatusCode.NotFound));
        }

        private static HttpResponseMessage Json(string body) =>
            new(HttpStatusCode.OK) { Content = new StringContent(body, Encoding.UTF8, "application/json") };
    }

    /// <summary>
    /// A real executor, resolver and quarantine over a temporary music folder. Library actions
    /// are on, real (no dry run), Delete is on and alice is allowed, unless a test says otherwise.
    /// </summary>
    private sealed class LibraryActionWebFactory : WebApplicationFactory<Program>
    {
        private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-action-web-" + Guid.NewGuid());
        private readonly Dictionary<string, string?> _overrides;

        public LibraryActionWebFactory(Dictionary<string, string?>? overrides = null)
        {
            _overrides = overrides ?? [];
            Navidrome = new FakeNavidrome(_directory);
            SongFile = Path.Combine(_directory, "Massive Attack", "Mezzanine", "03 - Teardrop.flac");
            Directory.CreateDirectory(Path.GetDirectoryName(SongFile)!);
            File.WriteAllBytes(SongFile, SongBytes);
        }

        public FakeNavidrome Navidrome { get; }
        public LibraryActionJournal Journal { get; } = new();
        public UpgradeQueue Upgrades { get; } = new();
        public string SongFile { get; }
        public string Quarantine => Path.Combine(_directory, ".octo-trash");

        protected override void ConfigureWebHost(IWebHostBuilder builder)
        {
            var settings = new Dictionary<string, string?>
            {
                ["Subsonic:Url"] = "http://navidrome.test",
                ["Subsonic:AutoDetectDownloadPath"] = "false",
                ["Subsonic:AdminUsername"] = "admin",
                ["Subsonic:AdminPassword"] = "admin-password",
                ["Soulseek:BaseUrl"] = "http://127.0.0.1:1",
                ["YouTube:ShimUrl"] = "http://127.0.0.1:1",
                ["Library:DownloadPath"] = _directory,
                ["LibraryActions:Enabled"] = "true",
                ["LibraryActions:DryRun"] = "false",
                ["LibraryActions:AllowedUsers:0"] = "alice",
                ["LibraryActions:Actions:0:Action"] = "Delete",
                ["LibraryActions:Actions:0:Enabled"] = "true",
                ["LibraryActions:QuarantineRetentionDays"] = "14",
            };
            foreach (var (key, value) in _overrides) settings[key] = value;

            builder.ConfigureAppConfiguration((_, configuration) => configuration.AddInMemoryCollection(settings));
            builder.ConfigureServices(services =>
            {
                services.RemoveAll<IHostedService>();
                services.RemoveAll<IHttpClientFactory>();
                services.AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(Navidrome));
                // In memory, so a test never writes a journal beside a real settings file.
                services.RemoveAll<LibraryActionJournal>();
                services.AddSingleton(Journal);
                services.RemoveAll<UpgradeQueue>();
                services.AddSingleton(Upgrades);
            });
        }

        public override async ValueTask DisposeAsync()
        {
            await base.DisposeAsync();
            try { Directory.Delete(_directory, recursive: true); } catch { /* best effort */ }
        }
    }

    /// <summary>Version 3's edits, listed after remove and upgrade whenever library actions are on.</summary>
    private static readonly string[] Edits = ["retag", "joinAlbum", "lookup", "undo", "restore", "cover"];

    private static string Auth(string user, string token = "good") =>
        $"u={user}&t={token}&s=salt&v=1.16.1&c=octo-android";

    private static JsonElement Envelope(JsonDocument doc) => doc.RootElement.GetProperty("subsonic-response");

    private static async Task<JsonDocument> GetJson(HttpClient client, string url)
    {
        var response = await client.GetAsync(url);
        Assert.Equal("application/json", response.Content.Headers.ContentType?.MediaType);
        return JsonDocument.Parse(await response.Content.ReadAsStringAsync());
    }

    private static JsonElement Action(JsonDocument doc)
    {
        var envelope = Envelope(doc);
        Assert.Equal("ok", envelope.GetProperty("status").GetString());
        Assert.Equal("octo", envelope.GetProperty("type").GetString());
        Assert.True(envelope.GetProperty("openSubsonic").GetBoolean());
        var action = envelope.GetProperty("libraryAction");
        Assert.Equal(["action", "detail", "id", "state"],
            action.EnumerateObject().Select(p => p.Name).OrderBy(n => n, StringComparer.Ordinal));
        return action;
    }

    private static void AssertFailed(JsonDocument doc, int code)
    {
        var envelope = Envelope(doc);
        Assert.Equal("failed", envelope.GetProperty("status").GetString());
        Assert.Equal(code, envelope.GetProperty("error").GetProperty("code").GetInt32());
        Assert.False(envelope.TryGetProperty("libraryAction", out _));
        Assert.False(envelope.TryGetProperty("libraryActions", out _));
    }

    /// <summary>Nothing moved, nothing looked up, nothing written to the journal.</summary>
    private static void AssertUntouched(LibraryActionWebFactory factory)
    {
        Assert.True(File.Exists(factory.SongFile));
        Assert.False(Directory.Exists(factory.Quarantine));
        Assert.Equal(0, factory.Navidrome.SongLookups);
        Assert.Empty(factory.Journal.Recent());
    }

    // getOpenSubsonicExtensions

    [Theory]
    [InlineData("true", true)]
    [InlineData("false", false)]
    public async Task Extensions_ListOctoLibraryActionsOnlyWhileTheyAreOn(string enabled, bool listed)
    {
        await using var factory = new LibraryActionWebFactory(new() { ["LibraryActions:Enabled"] = enabled });
        using var client = factory.CreateClient();

        using var doc = JsonDocument.Parse(await client.GetStringAsync("/rest/getOpenSubsonicExtensions.view?f=json&v=1.16.1&c=octo-android"));

        var extensions = Envelope(doc).GetProperty("openSubsonicExtensions").EnumerateArray()
            .ToDictionary(e => e.GetProperty("name").GetString()!,
                e => e.GetProperty("versions").EnumerateArray().Select(v => v.GetInt32()).ToList());
        Assert.Contains("formPost", extensions.Keys);
        Assert.Contains("octoAcquisitions", extensions.Keys);
        if (listed) Assert.Equal([1, 2, 3], extensions["octoLibraryActions"]);
        else Assert.DoesNotContain("octoLibraryActions", extensions.Keys);
    }

    [Theory]
    [InlineData("true", true)]
    [InlineData("false", false)]
    public async Task Extensions_ListOctoLibraryActionsOnlyWhileTheyAreOn_InXmlToo(string enabled, bool listed)
    {
        await using var factory = new LibraryActionWebFactory(new() { ["LibraryActions:Enabled"] = enabled });
        using var client = factory.CreateClient();

        var xml = System.Xml.Linq.XDocument.Parse(await client.GetStringAsync("/rest/getOpenSubsonicExtensions?v=1.16.1&c=test"));

        System.Xml.Linq.XNamespace ns = "http://subsonic.org/restapi";
        var ours = xml.Root!.Elements(ns + "openSubsonicExtensions")
            .Where(e => (string?)e.Attribute("name") == "octoLibraryActions").ToList();
        Assert.Contains(xml.Root.Elements(ns + "openSubsonicExtensions"), e => (string?)e.Attribute("name") == "formPost");
        if (listed) Assert.Equal("1", Assert.Single(ours).Element(ns + "versions")?.Value);
        else Assert.Empty(ours);
    }

    // getLibraryActions

    [Fact]
    public async Task GetLibraryActions_DescribesWhatTheCallerMayDo_InTheAppsShape()
    {
        await using var factory = new LibraryActionWebFactory();
        using var client = factory.CreateClient();

        // No f=json: JSON regardless.
        using var doc = await GetJson(client, $"/rest/getLibraryActions.view?{Auth("alice")}");

        var envelope = Envelope(doc);
        Assert.Equal("ok", envelope.GetProperty("status").GetString());
        Assert.Equal("1.16.1", envelope.GetProperty("version").GetString());
        Assert.Equal("octo", envelope.GetProperty("type").GetString());
        Assert.True(envelope.GetProperty("openSubsonic").GetBoolean());
        var actions = envelope.GetProperty("libraryActions");
        Assert.Equal(["actions", "admin", "allowed", "dryRun", "enabled", "keepDays", "parallel", "upgradeSource"],
            actions.EnumerateObject().Select(p => p.Name).OrderBy(n => n, StringComparer.Ordinal));
        Assert.True(actions.GetProperty("enabled").GetBoolean());
        Assert.True(actions.GetProperty("allowed").GetBoolean());
        Assert.False(actions.GetProperty("dryRun").GetBoolean());
        Assert.Equal(["remove", .. Edits], actions.GetProperty("actions").EnumerateArray().Select(a => a.GetString()));
        Assert.True(actions.GetProperty("admin").GetBoolean());
        Assert.Equal(14, actions.GetProperty("keepDays").GetInt32());
    }

    [Fact]
    public async Task GetLibraryActions_SomeoneNotOnTheAllowlist_IsNotAllowed()
    {
        await using var factory = new LibraryActionWebFactory();
        using var client = factory.CreateClient();

        using var doc = await GetJson(client, $"/rest/getLibraryActions?{Auth("bob")}&f=json");

        var actions = Envelope(doc).GetProperty("libraryActions");
        Assert.True(actions.GetProperty("enabled").GetBoolean());
        Assert.False(actions.GetProperty("allowed").GetBoolean());
    }

    [Fact]
    public async Task GetLibraryActions_DryRunOn_DeleteOff_KeptForever()
    {
        await using var factory = new LibraryActionWebFactory(new()
        {
            ["LibraryActions:DryRun"] = "true",
            ["LibraryActions:Actions:0:Enabled"] = "false",
            ["LibraryActions:QuarantineRetentionDays"] = "0",
        });
        using var client = factory.CreateClient();

        using var doc = await GetJson(client, $"/rest/getLibraryActions?{Auth("alice")}");

        var actions = Envelope(doc).GetProperty("libraryActions");
        Assert.True(actions.GetProperty("dryRun").GetBoolean());
        // Remove is off; the edits are listed, and the app reads dryRun before offering them.
        Assert.Equal(Edits, actions.GetProperty("actions").EnumerateArray().Select(a => a.GetString()));
        Assert.Equal(0, actions.GetProperty("keepDays").GetInt32());
    }

    [Fact]
    public async Task GetLibraryActions_Off_SaysSo()
    {
        await using var factory = new LibraryActionWebFactory(new() { ["LibraryActions:Enabled"] = "false" });
        using var client = factory.CreateClient();

        using var doc = await GetJson(client, $"/rest/getLibraryActions?{Auth("alice")}");

        Assert.False(Envelope(doc).GetProperty("libraryActions").GetProperty("enabled").GetBoolean());
    }

    [Fact]
    public async Task GetLibraryActions_WrongPassword_IsError40()
    {
        await using var factory = new LibraryActionWebFactory();
        using var client = factory.CreateClient();

        using var doc = await GetJson(client, $"/rest/getLibraryActions.view?{Auth("alice", "bad")}");

        AssertFailed(doc, 40);
        Assert.Equal(1, factory.Navidrome.Pings);
    }

    // libraryAction

    [Fact]
    public async Task Remove_MovesTheFileToQuarantine_AsDeleteForTheCaller()
    {
        await using var factory = new LibraryActionWebFactory();
        using var client = factory.CreateClient();

        // A form post, as formPost clients send it.
        var response = await client.PostAsync("/rest/libraryAction.view", new FormUrlEncodedContent(
            new Dictionary<string, string>
            {
                ["u"] = "alice", ["t"] = "good", ["s"] = "salt", ["v"] = "1.16.1", ["c"] = "octo-android",
                ["id"] = SongId, ["action"] = "remove",
            }));
        Assert.Equal("application/json", response.Content.Headers.ContentType?.MediaType);
        using var doc = JsonDocument.Parse(await response.Content.ReadAsStringAsync());

        var action = Action(doc);
        Assert.Equal(SongId, action.GetProperty("id").GetString());
        Assert.Equal("remove", action.GetProperty("action").GetString());
        Assert.Equal("applied", action.GetProperty("state").GetString());
        Assert.Equal("Removed. It will not be downloaded again.", action.GetProperty("detail").GetString());

        Assert.False(File.Exists(factory.SongFile));
        var moved = Assert.Single(Directory.GetFiles(factory.Quarantine, "*.flac", SearchOption.AllDirectories));
        Assert.Equal(SongBytes, File.ReadAllBytes(moved));

        var entry = Assert.Single(factory.Journal.Recent());
        Assert.Equal(LibraryAction.Delete, entry.Action);
        Assert.Equal(SongId, entry.NavidromeId);
        Assert.Equal("alice", entry.Username);
        Assert.Equal(LibraryActionState.Applied, entry.State);
        Assert.False(entry.DryRun);
    }

    [Fact]
    public async Task Remove_InADryRun_IsRehearsedAndMovesNothing()
    {
        await using var factory = new LibraryActionWebFactory(new() { ["LibraryActions:DryRun"] = "true" });
        using var client = factory.CreateClient();

        using var doc = await GetJson(client, $"/rest/libraryAction?{Auth("alice")}&id={SongId}&action=remove");

        var action = Action(doc);
        Assert.Equal("rehearsed", action.GetProperty("state").GetString());
        Assert.StartsWith("Dry run: would remove ", action.GetProperty("detail").GetString());
        Assert.True(File.Exists(factory.SongFile));
        Assert.False(Directory.Exists(factory.Quarantine));
        Assert.Equal(LibraryActionState.Rehearsed, Assert.Single(factory.Journal.Recent()).State);
    }

    [Fact]
    public async Task Remove_BySomeoneNotOnTheAllowlist_IsSkippedAndMovesNothing()
    {
        await using var factory = new LibraryActionWebFactory();
        using var client = factory.CreateClient();

        using var doc = await GetJson(client, $"/rest/libraryAction?{Auth("bob")}&id={SongId}&action=remove");

        var action = Action(doc);
        Assert.Equal("skipped", action.GetProperty("state").GetString());
        Assert.Equal("bob is not on the allowlist.", action.GetProperty("detail").GetString());
        AssertUntouched(factory);
    }

    [Fact]
    public async Task Remove_WithLibraryActionsOff_IsSkippedAndMovesNothing()
    {
        await using var factory = new LibraryActionWebFactory(new() { ["LibraryActions:Enabled"] = "false" });
        using var client = factory.CreateClient();

        using var doc = await GetJson(client, $"/rest/libraryAction?{Auth("alice")}&id={SongId}&action=remove");

        Assert.Equal("skipped", Action(doc).GetProperty("state").GetString());
        AssertUntouched(factory);
    }

    [Fact]
    public async Task Remove_WithDeleteOff_IsSkippedAndMovesNothing()
    {
        await using var factory = new LibraryActionWebFactory(new() { ["LibraryActions:Actions:0:Enabled"] = "false" });
        using var client = factory.CreateClient();

        using var doc = await GetJson(client, $"/rest/libraryAction?{Auth("alice")}&id={SongId}&action=remove");

        Assert.Equal("skipped", Action(doc).GetProperty("state").GetString());
        AssertUntouched(factory);
    }

    [Fact]
    public async Task Remove_OfASongWithNoFileItCanProve_IsUnresolved()
    {
        await using var factory = new LibraryActionWebFactory();
        using var client = factory.CreateClient();

        using var doc = await GetJson(client, $"/rest/libraryAction?{Auth("alice")}&id=nd-unknown&action=remove");

        var action = Action(doc);
        Assert.Equal("nd-unknown", action.GetProperty("id").GetString());
        Assert.Equal("unresolved", action.GetProperty("state").GetString());
        Assert.True(File.Exists(factory.SongFile));
        Assert.False(Directory.Exists(factory.Quarantine));
    }

    [Theory]
    [InlineData("action=remove")]
    [InlineData("id=" + SongId)]
    [InlineData("id=&action=remove")]
    public async Task Remove_WithoutIdOrAction_IsError10(string query)
    {
        await using var factory = new LibraryActionWebFactory();
        using var client = factory.CreateClient();

        using var doc = await GetJson(client, $"/rest/libraryAction.view?{Auth("alice")}&{query}");

        AssertFailed(doc, 10);
        AssertUntouched(factory);
    }

    [Theory]
    [InlineData("delete")]
    [InlineData("rate")]
    [InlineData("wrongSong")]
    public async Task Remove_AnyOtherAction_IsAnErrorAndMovesNothing(string verb)
    {
        await using var factory = new LibraryActionWebFactory();
        using var client = factory.CreateClient();

        using var doc = await GetJson(client, $"/rest/libraryAction?{Auth("alice")}&id={SongId}&action={verb}");

        AssertFailed(doc, 0);
        Assert.Contains(verb, Envelope(doc).GetProperty("error").GetProperty("message").GetString());
        AssertUntouched(factory);
    }

    [Fact]
    public async Task Remove_WithAWrongPassword_IsError40AndNeverReachesTheExecutor()
    {
        await using var factory = new LibraryActionWebFactory();
        using var client = factory.CreateClient();

        using var doc = await GetJson(client, $"/rest/libraryAction?{Auth("alice", "bad")}&id={SongId}&action=remove");

        AssertFailed(doc, 40);
        Assert.Equal(1, factory.Navidrome.Pings);
        AssertUntouched(factory);
    }

    [Fact]
    public async Task Remove_WithOnlyAnApiKey_IsSkippedBecauseItNamesNobody()
    {
        await using var factory = new LibraryActionWebFactory();
        using var client = factory.CreateClient();

        using var doc = await GetJson(client,
            $"/rest/libraryAction?apiKey=goodkey&v=1.16.1&c=octo-android&id={SongId}&action=remove");

        var action = Action(doc);
        Assert.Equal("skipped", action.GetProperty("state").GetString());
        Assert.Contains("API key", action.GetProperty("detail").GetString());
        Assert.Equal(1, factory.Navidrome.Pings);
        AssertUntouched(factory);
    }

    // octoLibraryActions v2: upgrade and getUpgrades

    private static Dictionary<string, string?> BetterQualityOn(bool dryRun = false, bool slskd = true) => new()
    {
        // Better quality searches Soulseek, so it is offered only where slskd is set up.
        ["Soulseek:Username"] = slskd ? "slskd-user" : "",
        ["Soulseek:Password"] = slskd ? "slskd-pass" : "",
        ["LibraryActions:Actions:1:Action"] = "BetterQuality",
        ["LibraryActions:Actions:1:Enabled"] = "true",
        ["LibraryActions:DryRun"] = dryRun ? "true" : "false",
    };

    [Fact]
    public async Task GetLibraryActions_ListsUpgradeOnlyWhileBetterQualityIsOn()
    {
        await using (var off = new LibraryActionWebFactory())
        {
            using var client = off.CreateClient();
            using var doc = await GetJson(client, $"/rest/getLibraryActions.view?{Auth("alice")}");
            var actions = Envelope(doc).GetProperty("libraryActions");
            Assert.Equal(["remove", .. Edits], actions.GetProperty("actions").EnumerateArray().Select(a => a.GetString()));
            Assert.Equal(1, actions.GetProperty("parallel").GetInt32());
        }
        await using (var on = new LibraryActionWebFactory(BetterQualityOn()))
        {
            using var client = on.CreateClient();
            using var doc = await GetJson(client, $"/rest/getLibraryActions.view?{Auth("alice")}");
            Assert.Equal(["remove", "upgrade", .. Edits],
                Envelope(doc).GetProperty("libraryActions").GetProperty("actions").EnumerateArray().Select(a => a.GetString()));
        }
    }

    [Fact]
    public async Task Upgrade_IsQueuedAtOnce_AndListedForTheCallerFirst()
    {
        await using var factory = new LibraryActionWebFactory(BetterQualityOn());
        using var client = factory.CreateClient();

        var watch = System.Diagnostics.Stopwatch.StartNew();
        using var doc = await GetJson(client, $"/rest/libraryAction.view?id=nd-1&action=upgrade&{Auth("alice")}");
        watch.Stop();

        var action = Action(doc);
        Assert.Equal("nd-1", action.GetProperty("id").GetString());
        Assert.Equal("upgrade", action.GetProperty("action").GetString());
        Assert.Equal("queued", action.GetProperty("state").GetString());
        Assert.True(watch.Elapsed < TimeSpan.FromSeconds(5), $"answered after {watch.Elapsed}");
        var job = Assert.Single(factory.Upgrades.Snapshot());
        Assert.Equal("alice", job.RequestedBy);
        Assert.Equal("app", job.Origin);
        AssertUntouched(factory);

        using var listed = await GetJson(client, $"/rest/getUpgrades.view?{Auth("alice")}");
        var row = Assert.Single(Envelope(listed).GetProperty("upgrades").EnumerateArray());
        Assert.Equal("nd-1", row.GetProperty("id").GetString());
        Assert.Equal("queued", row.GetProperty("state").GetString());
        Assert.Equal(["acquisition", "album", "artist", "detail", "id", "picked", "progress", "state", "title", "updatedAt"],
            row.EnumerateObject().Select(p => p.Name).OrderBy(n => n, StringComparer.Ordinal));

        using var someoneElse = await GetJson(client, $"/rest/getUpgrades.view?{Auth("bob")}");
        Assert.Empty(Envelope(someoneElse).GetProperty("upgrades").EnumerateArray());
    }

    [Theory]
    [InlineData("bob", false, "allowed list")]
    [InlineData("alice", true, "dry run")]
    public async Task Upgrade_ThatCouldNotChangeAnything_IsSkippedWithTheReason(string user, bool dryRun, string reason)
    {
        await using var factory = new LibraryActionWebFactory(BetterQualityOn(dryRun));
        using var client = factory.CreateClient();
        using var doc = await GetJson(client, $"/rest/libraryAction.view?id=nd-1&action=upgrade&{Auth(user)}");
        var action = Action(doc);
        Assert.Equal("skipped", action.GetProperty("state").GetString());
        Assert.Contains(reason, action.GetProperty("detail").GetString());
        Assert.Empty(factory.Upgrades.Snapshot());
    }

    [Fact]
    public async Task Upgrade_WithBetterQualityOff_IsSkipped()
    {
        await using var factory = new LibraryActionWebFactory();
        using var client = factory.CreateClient();
        using var doc = await GetJson(client, $"/rest/libraryAction.view?id=nd-1&action=upgrade&{Auth("alice")}");
        Assert.Contains("Better quality", Action(doc).GetProperty("detail").GetString());
        Assert.Empty(factory.Upgrades.Snapshot());
    }

    [Fact]
    public async Task WithoutSlskdSetUp_UpgradeIsNotOffered_AndAskingIsSkipped()
    {
        await using var factory = new LibraryActionWebFactory(BetterQualityOn(slskd: false));
        using var client = factory.CreateClient();
        using var actions = await GetJson(client, $"/rest/getLibraryActions.view?{Auth("alice")}");
        var described = Envelope(actions).GetProperty("libraryActions");
        Assert.DoesNotContain("upgrade", described.GetProperty("actions").EnumerateArray().Select(a => a.GetString()));
        Assert.Equal(System.Text.Json.JsonValueKind.Null, described.GetProperty("upgradeSource").ValueKind);

        using var doc = await GetJson(client, $"/rest/libraryAction.view?id=nd-1&action=upgrade&{Auth("alice")}");
        var action = Action(doc);
        Assert.Equal("skipped", action.GetProperty("state").GetString());
        Assert.Contains("not set up", action.GetProperty("detail").GetString());
        Assert.Empty(factory.Upgrades.Snapshot());
    }

    [Fact]
    public async Task GetLibraryActions_NamesWhereAnUpgradeLooks()
    {
        await using var factory = new LibraryActionWebFactory(BetterQualityOn());
        using var client = factory.CreateClient();
        using var doc = await GetJson(client, $"/rest/getLibraryActions.view?{Auth("alice")}");
        Assert.Equal("Soulseek", Envelope(doc).GetProperty("libraryActions").GetProperty("upgradeSource").GetString());
    }

    [Fact]
    public async Task GetUpgrades_WrongPassword_IsError40()
    {
        await using var factory = new LibraryActionWebFactory(BetterQualityOn());
        using var client = factory.CreateClient();
        using var doc = await GetJson(client, $"/rest/getUpgrades.view?{Auth("alice", "wrong")}");
        var envelope = Envelope(doc);
        Assert.Equal("failed", envelope.GetProperty("status").GetString());
        Assert.Equal(40, envelope.GetProperty("error").GetProperty("code").GetInt32());
    }
}
