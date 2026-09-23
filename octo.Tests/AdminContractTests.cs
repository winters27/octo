using System.Net;
using System.Reflection;
using System.Text.Json;
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
        ("ListenBrainz", typeof(ListenBrainzSettings)),
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

    private static System.Text.Json.Nodes.JsonObject JsonNodeObject(string json) =>
        System.Text.Json.Nodes.JsonNode.Parse(json)!.AsObject();
}
