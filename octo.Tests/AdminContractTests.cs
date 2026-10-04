using System.Net;
using System.Reflection;
using System.Text.Json;
using Microsoft.Extensions.DependencyInjection;
using Octo.Controllers;
using Octo.Middleware;
using Octo.Models.Settings;

namespace Octo.Tests;

/// <summary>
/// The admin API's contract with the dashboard and with the Raw config editor.
///
/// Two of these guard against a drift that has now bitten four times: GET settings pre-fills the
/// forms and GET raw-config is what the Raw editor writes back WHOLESALE, so a setting missing
/// from either is a setting the next save silently blanks or deletes. The admin credentials were
/// the latest instance. Checking every settings property by reflection makes the next one fail
/// here instead of on someone's server.
/// </summary>
public class AdminContractTests
{
    private static readonly (string Section, Type Type)[] Sections =
    [
        ("Subsonic", typeof(SubsonicSettings)),
        ("Soulseek", typeof(SoulseekSettings)),
        ("Lidarr", typeof(LidarrSettings)),
        ("LastFm", typeof(LastFmSettings)),
        ("Genre", typeof(GenreSettings)),
        ("LibraryActions", typeof(LibraryActionSettings)),
        ("Notifications", typeof(NotificationSettings)),
        ("Metadata", typeof(MetadataSettings)),
        ("Server", typeof(ServerSettings)),
        ("Updates", typeof(UpdateSettings)),
        ("ListenBrainz", typeof(ListenBrainzSettings)),
        ("GeneratedPlaylists", typeof(GeneratedPlaylistSettings)),
        ("Imports", typeof(ImportSettings)),
    ];

    /// <summary>Settings deliberately absent from the admin API, each with its reason.</summary>
    private static readonly HashSet<string> Excluded = new(StringComparer.Ordinal)
    {
    };

    private static IEnumerable<string> SettableProperties(Type type) =>
        type.GetProperties(BindingFlags.Public | BindingFlags.Instance)
            .Where(property => property.SetMethod is { IsPublic: true })
            .Select(property => property.Name);

    private static List<string> MissingFrom(JsonElement document) =>
        Sections.SelectMany(section => SettableProperties(section.Type)
                .Where(name => !Excluded.Contains($"{section.Section}.{name}"))
                .Where(name => !document.TryGetProperty(section.Section, out var values)
                               || !values.TryGetProperty(name, out _))
                .Select(name => $"{section.Section}.{name}"))
            .ToList();

    [Fact]
    public async Task GetSettings_ExposesEverySettingsProperty()
    {
        using var factory = new AdminWebFactory();
        using var client = factory.CreateClient();

        var document = JsonDocument.Parse(await client.GetStringAsync("/api/admin/settings")).RootElement;

        Assert.Empty(MissingFrom(document));
    }

    [Fact]
    public async Task GetRawConfig_ExposesEverySettingsProperty()
    {
        using var factory = new AdminWebFactory();
        using var client = factory.CreateClient();

        var document = JsonDocument.Parse(await client.GetStringAsync("/api/admin/raw-config")).RootElement;

        Assert.Empty(MissingFrom(document));
    }

    /// <summary>The factory configures a synthetic admin password. Neither GET may return it; both
    /// return the placeholder the save path swaps back.</summary>
    [Theory]
    [InlineData("/api/admin/settings")]
    [InlineData("/api/admin/raw-config")]
    public async Task AdminPassword_IsNeverReturned(string url)
    {
        using var factory = new AdminWebFactory();
        using var client = factory.CreateClient();

        var body = await client.GetStringAsync(url);

        Assert.DoesNotContain("synthetic-admin-password", body);
        var password = JsonDocument.Parse(body).RootElement.GetProperty("Subsonic").GetProperty("AdminPassword").GetString();
        Assert.Equal(AdminController.SecretPlaceholder, password);
    }

    /// <summary>slskd's web password signs in to slskd as its owner, so it never goes to a browser
    /// either.</summary>
    [Theory]
    [InlineData("/api/admin/settings")]
    [InlineData("/api/admin/raw-config")]
    public async Task SlskdPassword_IsNeverReturned(string url)
    {
        using var factory = new AdminWebFactory();
        using var client = factory.CreateClient();

        var body = await client.GetStringAsync(url);

        Assert.DoesNotContain("synthetic-slskd-password", body);
        var password = JsonDocument.Parse(body).RootElement.GetProperty("Soulseek").GetProperty("Password").GetString();
        Assert.Equal(AdminController.SecretPlaceholder, password);
    }

    [Fact]
    public void SlskdPassword_PlaceholderIsRestoredAndMasked()
    {
        var incoming = JsonNodeObject($$"""{ "Soulseek": { "Password": "{{AdminController.SecretPlaceholder}}" } }""");
        AdminController.RestoreSecretPlaceholders(incoming, JsonNodeObject("""{ "Soulseek": { "Password": "stored" } }"""));
        Assert.Equal("stored", (string?)incoming["Soulseek"]!["Password"]);

        var echoed = AdminController.RedactSecrets(JsonNodeObject("""{ "Soulseek": { "Password": "stored" } }"""));
        Assert.Equal(AdminController.SecretPlaceholder, (string?)echoed["Soulseek"]!["Password"]);
    }

    /// <summary>If the factory's upstream override ever stops applying, the first-run automation
    /// would scan the LAN from a test run. Fail loudly instead.</summary>
    [Fact]
    public async Task TestHost_PointsAtAClosedPortNotARealServer()
    {
        using var factory = new AdminWebFactory();
        using var client = factory.CreateClient();

        var document = JsonDocument.Parse(await client.GetStringAsync("/api/admin/settings")).RootElement;

        Assert.Equal("http://127.0.0.1:1", document.GetProperty("Subsonic").GetProperty("Url").GetString());
    }

    [Fact]
    public async Task Settings_ReportRestartPendingAsAList()
    {
        using var factory = new AdminWebFactory();
        using var client = factory.CreateClient();

        var meta = JsonDocument.Parse(await client.GetStringAsync("/api/admin/settings"))
            .RootElement.GetProperty("_meta");

        Assert.Equal(JsonValueKind.Array, meta.GetProperty("RestartPending").ValueKind);
        Assert.True(meta.GetProperty("ConfigFileValid").ValueKind is JsonValueKind.True or JsonValueKind.False);
    }

    /// <summary>A page on another origin cannot add this header without a preflight Octo refuses,
    /// so a write without it is either a cross-site request or a script that did not opt in.</summary>
    [Fact]
    public async Task AdminWrite_WithoutTheHeader_IsRefused()
    {
        using var factory = new AdminWebFactory();
        using var client = factory.CreateClient();

        using var response = await client.PostAsync("/api/admin/soulseek/rejected-peers/clear", null);

        Assert.Equal(HttpStatusCode.Forbidden, response.StatusCode);
    }

    [Fact]
    public async Task AdminRead_FromAnotherOrigin_CarriesNoCorsHeaders()
    {
        using var factory = new AdminWebFactory();
        using var client = factory.CreateClient();
        using var request = new HttpRequestMessage(HttpMethod.Get, "/api/admin/genre/presets");
        request.Headers.Add("Origin", "http://evil.example");

        using var response = await client.SendAsync(request);

        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        Assert.False(response.Headers.Contains("Access-Control-Allow-Origin"));
    }

    [Fact]
    public async Task AdminPreflight_IsNotApproved()
    {
        using var factory = new AdminWebFactory();
        using var client = factory.CreateClient();
        using var request = new HttpRequestMessage(HttpMethod.Options, "/api/admin/settings");
        request.Headers.Add("Origin", "http://evil.example");
        request.Headers.Add("Access-Control-Request-Method", "POST");
        request.Headers.Add("Access-Control-Request-Headers", AdminRequestGuard.HeaderName);

        using var response = await client.SendAsync(request);

        Assert.False(response.Headers.Contains("Access-Control-Allow-Origin"));
        Assert.False(response.Headers.Contains("Access-Control-Allow-Headers"));
    }

    /// <summary>Subsonic web players on another origin depend on CORS; the guard must not reach
    /// past /api/admin.</summary>
    [Fact]
    public async Task SubsonicRoute_FromAnotherOrigin_KeepsCors()
    {
        using var factory = new AdminWebFactory();
        using var client = factory.CreateClient();
        using var request = new HttpRequestMessage(HttpMethod.Get, "/rest/ping.view?u=a&p=b&v=1.16.1&c=test&f=json");
        request.Headers.Add("Origin", "http://player.example");

        using var response = await client.SendAsync(request);

        Assert.True(response.Headers.Contains("Access-Control-Allow-Origin"));
    }

    [Fact]
    public void RestoreSecretPlaceholders_KeepsTheStoredPassword()
    {
        var incoming = JsonNodeObject($$"""{ "Subsonic": { "AdminPassword": "{{AdminController.SecretPlaceholder}}" } }""");
        var existing = JsonNodeObject("""{ "Subsonic": { "AdminPassword": "stored" } }""");

        AdminController.RestoreSecretPlaceholders(incoming, existing);

        Assert.Equal("stored", (string?)incoming["Subsonic"]!["AdminPassword"]);
    }

    /// <summary>With nothing stored in the file the password came from the environment, so the
    /// key is dropped and the environment keeps applying.</summary>
    [Fact]
    public void RestoreSecretPlaceholders_DropsTheKeyWhenTheFileHasNone()
    {
        var incoming = JsonNodeObject($$"""{ "Subsonic": { "AdminPassword": "{{AdminController.SecretPlaceholder}}" } }""");

        AdminController.RestoreSecretPlaceholders(incoming, JsonNodeObject("{}"));

        Assert.False(incoming["Subsonic"]!.AsObject().ContainsKey("AdminPassword"));
    }

    [Fact]
    public void RestoreSecretPlaceholders_LeavesARealValueAlone()
    {
        var incoming = JsonNodeObject("""{ "Subsonic": { "AdminPassword": "typed" } }""");

        AdminController.RestoreSecretPlaceholders(incoming, JsonNodeObject("""{ "Subsonic": { "AdminPassword": "stored" } }"""));

        Assert.Equal("typed", (string?)incoming["Subsonic"]!["AdminPassword"]);
    }

    /// <summary>Configuration keys ignore case, so a hand-edited or scripted lowercase key must be
    /// handled exactly like the canonical one.</summary>
    [Fact]
    public void Placeholders_AreFoundWhateverTheKeyCase()
    {
        var incoming = JsonNodeObject($$"""{ "subsonic": { "adminPassword": "{{AdminController.SecretPlaceholder}}" } }""");
        AdminController.RestoreSecretPlaceholders(incoming, JsonNodeObject("""{ "Subsonic": { "AdminPassword": "stored" } }"""));
        Assert.Equal("stored", (string?)incoming["subsonic"]!["adminPassword"]);

        var echoed = AdminController.RedactSecrets(JsonNodeObject("""{ "subsonic": { "adminpassword": "stored" } }"""));
        Assert.Equal(AdminController.SecretPlaceholder, (string?)echoed["subsonic"]!["adminpassword"]);
    }

    [Fact]
    public void RedactSecrets_MasksTheAdminPasswordInTheSaveEcho()
    {
        var merged = JsonNodeObject("""{ "Subsonic": { "AdminPassword": "stored", "Url": "http://x" } }""");

        var echoed = AdminController.RedactSecrets(merged);

        Assert.Equal(AdminController.SecretPlaceholder, (string?)echoed["Subsonic"]!["AdminPassword"]);
        Assert.Equal("stored", (string?)merged["Subsonic"]!["AdminPassword"]);
    }

    /// <summary>Keep made five actions. A sixth is still a config that does not mean anything.</summary>
    [Fact]
    public void ValidateLibraryActions_AcceptsFive_RefusesSix()
    {
        static System.Text.Json.Nodes.JsonObject WithActions(int count)
        {
            var actions = new System.Text.Json.Nodes.JsonArray();
            for (var i = 0; i < count; i++)
                actions.Add(new System.Text.Json.Nodes.JsonObject { ["Name"] = $"a{i}", ["Rating"] = 0 });
            return new System.Text.Json.Nodes.JsonObject { ["Actions"] = actions };
        }

        Assert.Null(AdminController.ValidateLibraryActions(WithActions(5), new LibraryActionSettings()));
        Assert.Equal("There are only five library actions",
            AdminController.ValidateLibraryActions(WithActions(6), new LibraryActionSettings()));
    }

    private static System.Text.Json.Nodes.JsonObject JsonNodeObject(string json) =>
        System.Text.Json.Nodes.JsonNode.Parse(json)!.AsObject();

    // ---- the tag preview cannot be pointed at any file ----------------------------------

    /// <summary>With a session, a path that walks out of the music folder is refused before
    /// anything is read; the tool cannot be used to read arbitrary files.</summary>
    [Theory]
    [InlineData("../../etc/passwd")]
    [InlineData("C:\\Windows\\win.ini")]
    [InlineData("/etc/passwd")]
    public async Task TagPreview_PathOutsideTheMusicFolder_IsRefused(string path)
    {
        using var factory = new AdminWebFactory();
        using var client = factory.CreateClient();
        var token = factory.Services.GetRequiredService<Octo.Services.Admin.BrowseSessionStore>().Create("admin");
        using var request = new HttpRequestMessage(HttpMethod.Post, "/api/admin/tags/preview");
        request.Headers.Add(AdminRequestGuard.HeaderName, "1");
        request.Headers.Add("X-Octo-Browse-Token", token);
        request.Content = System.Net.Http.Json.JsonContent.Create(new { path });

        using var response = await client.SendAsync(request);

        Assert.Equal(HttpStatusCode.BadRequest, response.StatusCode);
        Assert.Contains("inside the music folder", await response.Content.ReadAsStringAsync());
    }

    [Fact]
    public async Task TagPreview_NeitherAPathNorAName_IsRefused()
    {
        using var factory = new AdminWebFactory();
        using var client = factory.CreateClient();
        var token = factory.Services.GetRequiredService<Octo.Services.Admin.BrowseSessionStore>().Create("admin");
        using var request = new HttpRequestMessage(HttpMethod.Post, "/api/admin/tags/preview");
        request.Headers.Add(AdminRequestGuard.HeaderName, "1");
        request.Headers.Add("X-Octo-Browse-Token", token);
        request.Content = System.Net.Http.Json.JsonContent.Create(new { artist = "Only an artist" });

        using var response = await client.SendAsync(request);

        Assert.Equal(HttpStatusCode.BadRequest, response.StatusCode);
    }

    [Fact]
    public void ResolveUnderRoot_RefusesDotDotRootedAndMissing_AcceptsAFileInside()
    {
        var root = Path.Combine(Path.GetTempPath(), "octo-root-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(Path.Combine(root, "Artist"));
        var inside = Path.Combine(root, "Artist", "song.flac");
        File.WriteAllBytes(inside, [1]);
        var outside = Path.Combine(Path.GetTempPath(), "octo-outside-" + Guid.NewGuid().ToString("N") + ".flac");
        File.WriteAllBytes(outside, [1]);
        try
        {
            Assert.Equal(Path.GetFullPath(inside), AdminController.ResolveUnderRoot(inside, root));
            Assert.Equal(Path.GetFullPath(inside), AdminController.ResolveUnderRoot(Path.Combine(root, "Artist", "..", "Artist", "song.flac"), root));
            Assert.Null(AdminController.ResolveUnderRoot(Path.Combine(root, "..", Path.GetFileName(outside)), root));
            Assert.Null(AdminController.ResolveUnderRoot(outside, root));
            Assert.Null(AdminController.ResolveUnderRoot(Path.Combine(root, "Artist", "missing.flac"), root));
            Assert.Null(AdminController.ResolveUnderRoot(root, root));
        }
        finally
        {
            try { Directory.Delete(root, true); File.Delete(outside); } catch { /* best effort */ }
        }
    }

    /// <summary>A link inside the folder that points outside is refused. Skipped where the OS
    /// will not let the test create one (Windows without developer mode).</summary>
    [Fact]
    public void ResolveUnderRoot_RefusesASymlink()
    {
        var root = Path.Combine(Path.GetTempPath(), "octo-root-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(root);
        var outside = Path.Combine(Path.GetTempPath(), "octo-outside-" + Guid.NewGuid().ToString("N") + ".flac");
        File.WriteAllBytes(outside, [1]);
        var link = Path.Combine(root, "link.flac");
        try
        {
            try { File.CreateSymbolicLink(link, outside); }
            catch (Exception) { return; }
            Assert.Null(AdminController.ResolveUnderRoot(link, root));
        }
        finally
        {
            try { Directory.Delete(root, true); File.Delete(outside); } catch { /* best effort */ }
        }
    }
}
