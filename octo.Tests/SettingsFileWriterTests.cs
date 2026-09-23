using System.Text.Json.Nodes;
using Octo.Services.Admin;

namespace Octo.Tests;

/// <summary>
/// Every dashboard save goes through this writer, so the ways it can lose a user's settings are
/// pinned here: merging must keep what the patch does not name, an unreadable file must be
/// refused rather than replaced, and a dictionary setting must be replaceable as a whole.
/// </summary>
public class SettingsFileWriterTests : IDisposable
{
    private readonly string _dir = Path.Combine(Path.GetTempPath(), "octo-settings-" + Guid.NewGuid());
    private string SettingsPath => Path.Combine(_dir, "settings.json");

    public SettingsFileWriterTests() => Directory.CreateDirectory(_dir);

    public void Dispose()
    {
        try { Directory.Delete(_dir, true); } catch { }
    }

    [Fact]
    public void Merge_KeepsKeysThePatchDoesNotName()
    {
        File.WriteAllText(SettingsPath, """{ "Soulseek": { "Password": "kept" }, "LastFm": { "ApiKey": "old" } }""");
        var writer = new SettingsFileWriter(SettingsPath);

        writer.Merge(new JsonObject { ["LastFm"] = new JsonObject { ["ApiKey"] = "new" } });

        var saved = JsonNode.Parse(File.ReadAllText(SettingsPath))!;
        Assert.Equal("kept", (string?)saved["Soulseek"]!["Password"]);
        Assert.Equal("new", (string?)saved["LastFm"]!["ApiKey"]);
    }

    /// <summary>
    /// Before this, an unparseable file read as empty and the next save wrote only that one
    /// form's fields, silently discarding everything else the user had saved.
    /// </summary>
    [Fact]
    public void Merge_RefusesACorruptFileAndLeavesItUntouched()
    {
        const string broken = """{ "Soulseek": { "Password": "kept" """;
        File.WriteAllText(SettingsPath, broken);
        var writer = new SettingsFileWriter(SettingsPath);

        Assert.Throws<SettingsFileCorruptException>(() =>
            writer.Merge(new JsonObject { ["LastFm"] = new JsonObject { ["ApiKey"] = "new" } }));
        Assert.Equal(broken, File.ReadAllText(SettingsPath));
        Assert.False(writer.IsReadable());
    }

    /// <summary>The configuration provider accepts comments and trailing commas, so a file the
    /// user annotated by hand is valid for Octo and must stay saveable.</summary>
    [Fact]
    public void Merge_AcceptsCommentsAndTrailingCommas()
    {
        File.WriteAllText(SettingsPath, """
            {
              // set by hand
              "Soulseek": { "Password": "kept", },
            }
            """);
        var writer = new SettingsFileWriter(SettingsPath);

        Assert.True(writer.IsReadable());
        writer.Merge(new JsonObject { ["LastFm"] = new JsonObject { ["ApiKey"] = "new" } });

        var saved = JsonNode.Parse(File.ReadAllText(SettingsPath))!;
        Assert.Equal("kept", (string?)saved["Soulseek"]!["Password"]);
    }

    /// <summary>A merge can only add dictionary keys. Removing a ListenBrainz per-user token in the
    /// dashboard has to replace the whole dictionary or the removed user keeps scrobbling.</summary>
    [Fact]
    public void Merge_ReplacesAListedObjectInsteadOfMergingIt()
    {
        File.WriteAllText(SettingsPath,
            """{ "ListenBrainz": { "Token": "t", "UserTokens": { "alice": "a", "bob": "b" } } }""");
        var writer = new SettingsFileWriter(SettingsPath);

        writer.Merge(new JsonObject
        {
            ["ListenBrainz"] = new JsonObject { ["UserTokens"] = new JsonObject { ["alice"] = "a" } },
        }, ["ListenBrainz.UserTokens"]);

        var saved = JsonNode.Parse(File.ReadAllText(SettingsPath))!;
        var tokens = saved["ListenBrainz"]!["UserTokens"]!.AsObject();
        Assert.True(tokens.ContainsKey("alice"));
        Assert.False(tokens.ContainsKey("bob"));
        Assert.Equal("t", (string?)saved["ListenBrainz"]!["Token"]);
    }

    [Fact]
    public void IsReadable_FalseOnlyForUnparseableContent()
    {
        var writer = new SettingsFileWriter(SettingsPath);
        Assert.True(writer.IsReadable());          // missing

        File.WriteAllText(SettingsPath, "   ");
        Assert.True(writer.IsReadable());          // empty

        File.WriteAllText(SettingsPath, "[1, 2]");
        Assert.False(writer.IsReadable());         // valid JSON, but not an object

        File.WriteAllText(SettingsPath, "{}");
        Assert.True(writer.IsReadable());
    }

    [Fact]
    public void Replace_WritesExactlyTheGivenDocument()
    {
        File.WriteAllText(SettingsPath, """{ "Old": { "Key": 1 } }""");
        var writer = new SettingsFileWriter(SettingsPath);

        writer.Replace(new JsonObject { ["New"] = new JsonObject { ["Key"] = 2 } });

        var saved = JsonNode.Parse(File.ReadAllText(SettingsPath))!.AsObject();
        Assert.False(saved.ContainsKey("Old"));
        Assert.Equal(2, (int)saved["New"]!["Key"]!);
    }
}
