using System.Collections;
using System.Reflection;
using System.Text.Json.Nodes;
using System.Text.RegularExpressions;
using Microsoft.AspNetCore.Hosting;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Octo.Controllers;
using Octo.Models.Settings;
using Octo.Services.Imports;

namespace Octo.Tests;

/// <summary>
/// No credential leaves the admin API in clear: not the Discord webhook address, the ntfy
/// token, the Lidarr key or any other. The settings types are walked by reflection, so a
/// credential added later fails here until it is masked too.
/// </summary>
public sealed class AdminSecretTests
{
    /// <summary>Every settings type the admin API serves, by its section.</summary>
    private static readonly Dictionary<Type, string> Sections = new()
    {
        [typeof(SubsonicSettings)] = "Subsonic",
        [typeof(SoulseekSettings)] = "Soulseek",
        [typeof(LidarrSettings)] = "Lidarr",
        [typeof(LastFmSettings)] = "LastFm",
        [typeof(GenreSettings)] = "Genre",
        [typeof(LibraryActionSettings)] = "LibraryActions",
        [typeof(NotificationSettings)] = "Notifications",
        [typeof(MetadataSettings)] = "Metadata",
        [typeof(ServerSettings)] = "Server",
        [typeof(UpdateSettings)] = "Updates",
        [typeof(ListenBrainzSettings)] = "ListenBrainz",
        [typeof(GeneratedPlaylistSettings)] = "GeneratedPlaylists",
        [typeof(ImportSettings)] = "Imports",
    };

    /// <summary>What a credential is called. A webhook address carries its own token.</summary>
    private static readonly Regex Credential = new("(Password|Secret|Token|ApiKey|SessionKey|Webhook)", RegexOptions.IgnoreCase);

    /// <summary>
    /// Every credential among the settings, as a configuration path: "Lidarr:ApiKey", or for one
    /// kept per listener "ListenBrainz:UserTokens:*" and "LastFm:UserSessions:*:SessionKey".
    /// </summary>
    private static List<string> CredentialPaths()
    {
        var paths = new List<string>();
        foreach (var (type, section) in Sections) Walk(type, section, paths, 0);
        return paths;
    }

    private static void Walk(Type type, string prefix, List<string> paths, int depth)
    {
        if (depth > 3) return;
        foreach (var property in type.GetProperties(BindingFlags.Public | BindingFlags.Instance).Where(p => p.SetMethod is { IsPublic: true }))
        {
            var path = $"{prefix}:{property.Name}";
            var kind = property.PropertyType;
            if (kind == typeof(string))
            {
                if (Credential.IsMatch(property.Name)) paths.Add(path);
            }
            else if (kind.IsGenericType && kind.GetGenericTypeDefinition() == typeof(Dictionary<,>))
            {
                var value = kind.GetGenericArguments()[1];
                if (value == typeof(string) && Credential.IsMatch(property.Name)) paths.Add($"{path}:*");
                else if (value.IsClass && value != typeof(string)) Walk(value, $"{path}:*", paths, depth + 1);
            }
            else if (kind.IsClass && !typeof(IEnumerable).IsAssignableFrom(kind) && kind.Namespace?.StartsWith("Octo") == true)
                Walk(kind, path, paths, depth + 1);
        }
    }

    [Fact]
    public void EveryCredentialInTheSettingsIsOneTheAdminApiMasks()
    {
        var masked = AdminController.SecretFields.Select(field => $"{field.Section}:{field.Key}")
            .Concat(AdminController.SecretMaps.Select(map => map.Field is null ? $"{map.Section}:{map.Map}:*" : $"{map.Section}:{map.Map}:*:{map.Field}"))
            .ToHashSet(StringComparer.Ordinal);

        var paths = CredentialPaths();

        Assert.Contains("Notifications:DiscordWebhookUrl", paths);
        Assert.DoesNotContain(paths, path => !masked.Contains(path));
    }

    [Fact]
    public void EverySettingsTypeIsWalked()
    {
        var types = typeof(SubsonicSettings).Assembly.GetTypes()
            .Where(type => type.IsClass && !type.IsAbstract && type.Name.EndsWith("Settings", StringComparison.Ordinal)
                           && type.Namespace?.StartsWith("Octo") == true)
            .Where(type => type.GetProperties().Any(property => Credential.IsMatch(property.Name)))
            .ToList();

        Assert.DoesNotContain(types, type => !Sections.ContainsKey(type));
    }

    [Theory]
    [InlineData("/api/admin/settings")]
    [InlineData("/api/admin/raw-config")]
    [InlineData("/api/admin/config-sources")]
    public async Task NoCredentialIsEverReturned(string url)
    {
        var values = new Dictionary<string, string?>();
        var n = 0;
        foreach (var path in CredentialPaths())
            values[path.Replace("*", "alice")] = path.Contains("DiscordWebhookUrl")
                ? $"https://discord.com/api/webhooks/1/synthetic-credential-{++n}"
                : $"synthetic-credential-{++n}";
        await using var factory = new AdminWebFactory();
        await using var built = factory.WithWebHostBuilder(builder =>
        {
            builder.ConfigureAppConfiguration((_, config) => config.AddInMemoryCollection(values));
            builder.ConfigureServices(services => services.RemoveAll<IHostedService>());
        });
        using var client = built.CreateClient();

        var body = await client.GetStringAsync(url);

        Assert.DoesNotContain("synthetic-credential", body);
    }

    [Fact]
    public void ASaveThatSendsThePlaceholdersBackKeepsEveryCredential()
    {
        var placeholder = AdminController.SecretPlaceholder;
        var patch = JsonNode.Parse($$"""
            {
              "Lidarr": { "ApiKey": "{{placeholder}}", "BaseUrl": "http://lidarr:8686" },
              "Notifications": { "NtfyToken": "{{placeholder}}", "DiscordWebhookUrl": "{{placeholder}}" },
              "ListenBrainz": { "Token": "{{placeholder}}", "UserTokens": { "alice": "{{placeholder}}", "bob": "new-token", "carol": "{{placeholder}}" } }
            }
            """)!.AsObject();

        var refused = AdminController.KeepSavedSecrets(patch, new Dictionary<string, string> { ["alice"] = "alices-token" });

        Assert.Null(refused);
        Assert.Equal("""{"BaseUrl":"http://lidarr:8686"}""", patch["Lidarr"]!.ToJsonString());
        Assert.Equal("{}", patch["Notifications"]!.ToJsonString());
        Assert.Equal("""{"alice":"alices-token","bob":"new-token"}""", patch["ListenBrainz"]!["UserTokens"]!.ToJsonString());

        var typed = JsonNode.Parse($$"""{ "Notifications": { "NtfyToken": "{{placeholder}}tk_extra" } }""")!.AsObject();
        Assert.Equal("Retype the whole ntfy token; it was added to the hidden placeholder.", AdminController.KeepSavedSecrets(typed, null));
    }

    [Fact]
    public void ASavedCredentialNeverFollowsANewAddressItWasNotTypedFor()
    {
        var placeholder = AdminController.SecretPlaceholder;
        var running = new Dictionary<string, string>
        {
            ["Lidarr:BaseUrl"] = "http://lidarr:8686", ["Lidarr:ApiKey"] = "stored-key",
            ["Notifications:NtfyUrl"] = "https://ntfy.sh/octo", ["Notifications:NtfyToken"] = "tk_stored",
        };
        string? Current(string section, string name) => running.GetValueOrDefault($"{section}:{name}");
        JsonObject Patch(string json) => JsonNode.Parse(json)!.AsObject();

        Assert.Equal("Retype the Lidarr API key for the new address.", AdminController.SecretSentElsewhere(
            Patch($$"""{ "Lidarr": { "BaseUrl": "http://elsewhere:8686", "ApiKey": "{{placeholder}}" } }"""), Current));
        Assert.Equal("Retype the ntfy token for the new address.", AdminController.SecretSentElsewhere(
            Patch("""{ "Notifications": { "NtfyUrl": "https://elsewhere.example/octo" } }"""), Current));
        // The same address, a retyped key, or nothing saved yet: nothing to keep from anyone.
        Assert.Null(AdminController.SecretSentElsewhere(
            Patch($$"""{ "Lidarr": { "BaseUrl": "http://lidarr:8686/", "ApiKey": "{{placeholder}}" } }"""), Current));
        Assert.Null(AdminController.SecretSentElsewhere(
            Patch("""{ "Lidarr": { "BaseUrl": "http://elsewhere:8686", "ApiKey": "typed-key" } }"""), Current));
        Assert.Null(AdminController.SecretSentElsewhere(
            Patch($$"""{ "Soulseek": { "BaseUrl": "http://slskd:5030", "Password": "{{placeholder}}" } }"""), Current));
    }

    [Fact]
    public void RawConfigPutsBackEveryCredentialItHidAndEchoesNone()
    {
        var placeholder = AdminController.SecretPlaceholder;
        var incoming = JsonNode.Parse($$"""
            {
              "Lidarr": { "ApiKey": "{{placeholder}}" },
              "Notifications": { "DiscordWebhookUrl": "{{placeholder}}" },
              "ListenBrainz": { "UserTokens": { "alice": "{{placeholder}}" } }
            }
            """)!.AsObject();
        var stored = JsonNode.Parse("""
            {
              "Lidarr": { "ApiKey": "stored-key" },
              "Notifications": { "DiscordWebhookUrl": "https://discord.com/api/webhooks/1/stored" },
              "ListenBrainz": { "UserTokens": { "alice": "stored-token" } }
            }
            """)!.AsObject();

        AdminController.RestoreSecretPlaceholders(incoming, stored);

        Assert.Equal(stored.ToJsonString(), incoming.ToJsonString());
        Assert.DoesNotContain("stored", AdminController.RedactSecrets(stored).ToJsonString());
    }
}
