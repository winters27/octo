using System.Net;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.Extensions.Logging.Abstractions;
using Octo.Controllers;
using Octo.Models.Settings;
using Octo.Services.Soulseek;

namespace Octo.Tests;

/// <summary>
/// The Share my library switch, against a scripted slskd that behaves like 0.26: slskd.yml is read
/// and replaced through the API, and the share list slskd reports follows the file once its file
/// watch has caught up.
/// </summary>
public class SoulseekShareSwitchTests
{
    // The start of the commented-out slskd.yml slskd writes on first run, with
    // the Linux line ends slskd writes whatever this file was checked out with.
    private static readonly string StockFile = """
        # debug: false
        # remote_configuration: false
        # shares:
        #   directories:
        #     - 'D:\Music'
        """.ReplaceLineEndings("\n");

    private sealed class FakeSlskd
    {
        public string File = StockFile;
        public int Status = 200;
        public int Writes, Rescans;
        // The folders slskd last looked through; null before any look.
        public List<string>? Scanned;
        // Rescan requests slskd answers OK and then drops, as 0.26 does while a look is finishing.
        public int Drop;
        public bool Busy;
        public List<string> Cancelled = [];
        // Shares from somewhere other than slskd.yml, such as SLSKD_SHARED_DIR on the container.
        public List<string> FromEnvironment = [];
        public string Uploads = "[]";
        // How many option reads it takes slskd to notice a new file.
        public int Lag = 1;
        private int _readsSinceWrite;

        public List<string> Directories()
        {
            // Every uncommented list entry in the file is a share; the stock file's are all comments.
            var fromFile = System.Text.RegularExpressions.Regex
                .Matches(File, @"^\s+- '(.*)'$", System.Text.RegularExpressions.RegexOptions.Multiline)
                .Select(m => m.Groups[1].Value.Replace("''", "'"))
                .ToList();
            // A config array merges by position: the file's entries replace the environment's.
            var merged = new List<string>(fromFile);
            merged.AddRange(FromEnvironment.Skip(fromFile.Count));
            return merged;
        }

        private List<string> _seen = [];

        public SoulseekShareSwitch Switch(bool on, string folders = "[Music]/share")
        {
            _seen = Directories();
            var settings = TestOptions.Monitor(new SoulseekSettings { ShareLibrary = on, SharedFolders = folders });
            return new SoulseekShareSwitch(null!, settings, NullLogger<SoulseekShareSwitch>.Instance)
            {
                ReadFile = _ => Task.FromResult<(int, string?)>((Status, Status == 200 ? File : null)),
                WriteFile = (yaml, _) => { File = yaml; Writes++; _readsSinceWrite = 0; return Task.FromResult<string?>(null); },
                ReadOptions = _ =>
                {
                    if (++_readsSinceWrite > Lag) _seen = Directories();
                    return Task.FromResult<string?>(JsonSerializer.Serialize(new { shares = new { directories = _seen } }));
                },
                ReadUploads = _ => Task.FromResult<string?>(Uploads),
                Rescan = _ =>
                {
                    Rescans++;
                    if (Drop > 0) Drop--;
                    else Scanned = [.. _seen];
                    return Task.FromResult<string?>(null);
                },
                ReadApplication = _ => Task.FromResult<string?>(JsonSerializer.Serialize(new
                {
                    shares = new { scanning = Busy, ready = !Busy, files = (Scanned?.Count ?? 0) * 2 },
                })),
                ReadShares = _ => Task.FromResult<string?>(JsonSerializer.Serialize(new
                {
                    local = _seen.Select(dir => new
                    {
                        alias = "Music", localPath = dir, isExcluded = false,
                        files = Scanned?.Contains(dir) == true ? 2 : (int?)null,
                    }),
                })),
                CancelScan = _ => Task.FromResult(true),
                CancelUpload = (user, id, _) => { Cancelled.Add($"{user}/{id}"); return Task.FromResult(true); },
                Delay = (_, _) => Task.CompletedTask,
            };
        }
    }

    [Fact]
    public async Task On_WritesOctosBlock_WaitsForSlskd_ThenRescans()
    {
        var slskd = new FakeSlskd();
        var share = slskd.Switch(on: true);

        Assert.Null(await share.ApplyAsync(true, chosen: true, default));

        Assert.Equal(ShareControl.Octo, share.Control);
        Assert.Equal(["[Music]/share"], slskd.Directories());
        Assert.StartsWith(StockFile.TrimEnd(), slskd.File);
        Assert.Contains(SoulseekShareSwitch.BlockStart, slskd.File);
        Assert.Equal(1, slskd.Rescans);
        Assert.Empty(slskd.Cancelled);

        // Already as wanted: nothing is written and slskd is not made to look again.
        Assert.Null(await share.ApplyAsync(true, chosen: false, default));
        Assert.Equal(1, slskd.Writes);
        Assert.Equal(1, slskd.Rescans);
    }

    /// <summary>slskd 0.26 says OK to a rescan it then drops while the last look is finishing. The
    /// switch checks the look happened and asks again; a later background check puts right a list
    /// slskd never looked through, without writing anything.</summary>
    [Fact]
    public async Task ADroppedRescan_IsAskedAgain_AndTheBackgroundCheckHeals()
    {
        var slskd = new FakeSlskd { Drop = 1 };
        Assert.Null(await slskd.Switch(on: true).ApplyAsync(true, chosen: true, default));
        Assert.Equal(2, slskd.Rescans);
        Assert.Equal(["[Music]/share"], slskd.Scanned);

        slskd.Scanned = null;
        Assert.Null(await slskd.Switch(on: true).ApplyAsync(true, chosen: false, default));
        Assert.Equal(1, slskd.Writes);
        Assert.Equal(["[Music]/share"], slskd.Scanned);

        // A look slskd is running by itself is never stopped by the background check.
        slskd.Scanned = null;
        slskd.Busy = true;
        Assert.Null(await slskd.Switch(on: true).ApplyAsync(true, chosen: false, default));
        Assert.Equal(3, slskd.Rescans);
    }

    [Fact]
    public async Task Off_SharesNothing_RescansSoSlskdForgets_AndCancelsOpenUploads()
    {
        var slskd = new FakeSlskd();
        await slskd.Switch(on: true).ApplyAsync(true, chosen: true, default);
        slskd.Uploads = """
            [ { "username": "alice", "directories": [ { "directory": "Music", "files": [
                { "id": "u1", "state": "InProgress" },
                { "id": "u2", "state": "Queued, Locally" },
                { "id": "u3", "state": "Completed, Succeeded" } ] } ] } ]
            """;

        var share = slskd.Switch(on: false);
        Assert.Null(await share.ApplyAsync(false, chosen: true, default));

        Assert.Empty(slskd.Directories());
        Assert.Contains("  directories: []", slskd.File);
        Assert.Single(slskd.File.Split(SoulseekShareSwitch.BlockStart)[1..]);
        Assert.Equal(2, slskd.Rescans);
        Assert.Equal(["alice/u1", "alice/u2"], slskd.Cancelled);
    }

    /// <summary>The background check on an install that never shared, with the switch off: nothing
    /// to do, so slskd.yml is not touched at all.</summary>
    [Fact]
    public async Task Off_OnAnInstallThatNeverShared_WritesNothing()
    {
        var slskd = new FakeSlskd();

        Assert.Null(await slskd.Switch(on: false).ApplyAsync(false, chosen: false, default));

        Assert.Equal(0, slskd.Writes);
        Assert.Equal(StockFile, slskd.File);
    }

    /// <summary>Shares someone set on the slskd container are theirs: the background check leaves
    /// them, and a person flipping the switch is told where they come from.</summary>
    [Fact]
    public async Task SharesFromTheEnvironment_AreLeftAlone_AndNamed()
    {
        var slskd = new FakeSlskd { FromEnvironment = ["/data/music"] };

        var background = slskd.Switch(on: false);
        Assert.Null(await background.ApplyAsync(false, chosen: false, default));
        Assert.Equal(ShareControl.Outside, background.Control);
        Assert.Equal(0, slskd.Writes);

        var chosen = slskd.Switch(on: false);
        var problem = await chosen.ApplyAsync(false, chosen: true, default);
        Assert.Contains("/data/music", problem);
        Assert.Contains("SLSKD_SHARED_DIR", problem);
        Assert.Equal(0, slskd.Rescans);
    }

    [Fact]
    public async Task SharesInSlskdYml_AreTheirs_AndNeverOverwritten()
    {
        var own = StockFile + "\nshares:\n  directories:\n    - '/data/music'\n";
        var slskd = new FakeSlskd { File = own };

        var share = slskd.Switch(on: true);
        var problem = await share.ApplyAsync(true, chosen: true, default);

        Assert.Contains("shares section of its own", problem);
        Assert.Equal(ShareControl.Outside, share.Control);
        Assert.Equal(own, slskd.File);
    }

    [Fact]
    public async Task RemoteConfigurationOff_IsSaidPlainly()
    {
        var slskd = new FakeSlskd { Status = 403 };

        var share = slskd.Switch(on: true);
        var problem = await share.ApplyAsync(true, chosen: true, default);

        Assert.Equal(ShareControl.Locked, share.Control);
        Assert.Contains("SLSKD_REMOTE_CONFIGURATION", problem);
        Assert.Equal(0, slskd.Writes);
    }

    [Fact]
    public void TheBlock_IsReplacedInPlace_AndQuotesAreEscaped()
    {
        var once = SoulseekShareSwitch.WithBlock(StockFile, ["[Music]/share", "!/share/Bob's Memos"]);
        var (rest, block) = SoulseekShareSwitch.SplitBlock(once);

        Assert.Equal(StockFile.TrimEnd(), rest.TrimEnd());
        Assert.Contains("    - '!/share/Bob''s Memos'", block);
        Assert.False(SoulseekShareSwitch.HasOwnShares(rest));

        var twice = SoulseekShareSwitch.WithBlock(rest, []);
        Assert.Single(twice.Split(SoulseekShareSwitch.BlockStart)[1..]);
        Assert.Contains("  directories: []", twice);
    }

    /// <summary>Through the real app: the switch is saved to settings.json, and with slskd out of
    /// reach the answer says so instead of pretending.</summary>
    [Fact]
    public async Task ShareEndpoint_SavesTheChoice_AndReportsWhatSlskdDid()
    {
        await using var factory = new ScrobbleAdminFactory();
        using var client = factory.AdminClient();

        using var on = await client.PostAsync("/api/admin/soulseek/sharing/share",
            new StringContent("""{ "on": true }""", Encoding.UTF8, "application/json"));
        on.EnsureSuccessStatusCode();
        var report = JsonDocument.Parse(await on.Content.ReadAsStringAsync()).RootElement;
        Assert.True(report.GetProperty("switch").GetProperty("on").GetBoolean());
        Assert.Contains("cannot reach slskd", report.GetProperty("switch").GetProperty("problem").GetString());
        Assert.True((bool)JsonNode.Parse(File.ReadAllText(factory.SettingsPath))!["Soulseek"]!["ShareLibrary"]!);

        using var off = await client.PostAsync("/api/admin/soulseek/sharing/share",
            new StringContent("""{ "on": false }""", Encoding.UTF8, "application/json"));
        off.EnsureSuccessStatusCode();
        Assert.False(JsonDocument.Parse(await off.Content.ReadAsStringAsync()).RootElement
            .GetProperty("switch").GetProperty("on").GetBoolean());
        Assert.False((bool)JsonNode.Parse(File.ReadAllText(factory.SettingsPath))!["Soulseek"]!["ShareLibrary"]!);

        using var unguarded = factory.CreateClient();
        using var refused = await unguarded.PostAsync("/api/admin/soulseek/sharing/share",
            new StringContent("""{ "on": true }""", Encoding.UTF8, "application/json"));
        Assert.Equal(HttpStatusCode.Forbidden, refused.StatusCode);
    }
}
