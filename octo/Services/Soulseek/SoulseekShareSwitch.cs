using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Text.RegularExpressions;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;

namespace Octo.Services.Soulseek;

/// <summary>Who decides what slskd shares. Octo: the dashboard switch. Outside: slskd.yml or the
/// slskd container's environment sets shares Octo did not write, and Octo leaves them alone.
/// Locked: slskd does not allow remote configuration. Unknown: slskd has not answered yet.</summary>
[JsonConverter(typeof(JsonStringEnumConverter<ShareControl>))]
public enum ShareControl { Unknown, Octo, Outside, Locked }

/// <summary>
/// The Share my library switch. slskd 0.26 cannot change its shares through an options patch (that
/// only covers the listening address), but it does let an administrator replace its settings file,
/// slskd.yml, through its API when remote configuration is on (the bundled compose file turns it
/// on). slskd checks the new file, keeps the old one as slskd.yml.bak and applies it on the spot
/// through its file watch: its share list is read live, no restart. So Octo keeps one marked block
/// in slskd.yml with the share list, waits until slskd reports the new list, then has slskd look
/// through its folders again, which also drops everything no longer shared. Off writes an empty
/// list and cancels uploads still waiting or under way, so off means nothing is shared.
///
/// Octo never touches shares it did not write: a "shares:" section of the person's own in
/// slskd.yml, or shares from the slskd container's environment, make the switch stand aside.
/// It checks every 10 minutes that slskd still matches the switch, so a recreated slskd with a
/// fresh settings file is put right without anyone noticing.
/// </summary>
public sealed class SoulseekShareSwitch : BackgroundService
{
    internal const string BlockStart = "# >>> Octo: sharing. Octo's dashboard writes this part (Soulseek page, Sharing); change it there.";
    internal const string BlockEnd = "# <<< Octo: sharing";
    internal static readonly TimeSpan CheckEvery = TimeSpan.FromMinutes(10);
    internal static readonly TimeSpan RetryEvery = TimeSpan.FromMinutes(1);
    // How many times, half a second apart, to look for slskd to catch up: 15 seconds.
    internal const int Polls = 30;

    private readonly SoulseekClient _client;
    private readonly IOptionsMonitor<SoulseekSettings> _settings;
    private readonly ILogger<SoulseekShareSwitch> _logger;
    private readonly SemaphoreSlim _lock = new(1, 1);

    public SoulseekShareSwitch(SoulseekClient client, IOptionsMonitor<SoulseekSettings> settings,
        ILogger<SoulseekShareSwitch> logger)
    {
        _client = client;
        _settings = settings;
        _logger = logger;
        ReadFile = ct => _client.ReadSettingsFileAsync(ct);
        WriteFile = (yaml, ct) => _client.WriteSettingsFileAsync(yaml, ct);
        ReadOptions = ct => _client.ReadAsync("options", ct);
        ReadUploads = ct => _client.ReadAsync("transfers/uploads", ct);
        ReadApplication = ct => _client.ReadAsync("application", ct);
        ReadShares = ct => _client.ReadAsync("shares", ct);
        Rescan = ct => _client.RescanSharesAsync(ct);
        CancelScan = ct => _client.CancelShareScanAsync(ct);
        CancelUpload = (user, id, ct) => _client.CancelUploadAsync(user, id, ct);
    }

    // Seams, so the switch can be driven against a scripted slskd.
    internal Func<CancellationToken, Task<(int Status, string? Yaml)>> ReadFile { get; set; }
    internal Func<string, CancellationToken, Task<string?>> WriteFile { get; set; }
    internal Func<CancellationToken, Task<string?>> ReadOptions { get; set; }
    internal Func<CancellationToken, Task<string?>> ReadUploads { get; set; }
    internal Func<CancellationToken, Task<string?>> ReadApplication { get; set; }
    internal Func<CancellationToken, Task<string?>> ReadShares { get; set; }
    internal Func<CancellationToken, Task<string?>> Rescan { get; set; }
    internal Func<CancellationToken, Task<bool>> CancelScan { get; set; }
    internal Func<string, string, CancellationToken, Task<bool>> CancelUpload { get; set; }
    internal Func<TimeSpan, CancellationToken, Task> Delay { get; set; } = Task.Delay;
    internal TimeSpan PollEvery { get; set; } = TimeSpan.FromMilliseconds(500);

    /// <summary>Who decides slskd's shares, as of the last look.</summary>
    public ShareControl Control { get; private set; } = ShareControl.Unknown;

    /// <summary>Why the switch could not do its job the last time it tried, or null.</summary>
    public string? Problem { get; private set; }

    protected override async Task ExecuteAsync(CancellationToken stoppingToken)
    {
        while (!stoppingToken.IsCancellationRequested)
        {
            var ok = false;
            try { ok = (await ApplyAsync(_settings.CurrentValue.ShareLibrary, chosen: false, stoppingToken)) is null; }
            catch (Exception ex) when (!stoppingToken.IsCancellationRequested)
            {
                _logger.LogWarning("Soulseek sharing check failed: {Msg}", ex.Message);
            }
            try { await Delay(ok || Control is ShareControl.Outside or ShareControl.Locked ? CheckEvery : RetryEvery, stoppingToken); }
            catch (OperationCanceledException) { return; }
        }
    }

    /// <summary>
    /// Makes slskd share the library, or nothing. Chosen is true when a person just flipped the
    /// switch, and false for the background check, which never takes over shares Octo did not
    /// write and never writes anything to an slskd that already matches. Null when slskd now
    /// matches; otherwise why not, in words for the dashboard.
    /// </summary>
    public async Task<string?> ApplyAsync(bool on, bool chosen, CancellationToken ct)
    {
        await _lock.WaitAsync(ct);
        try
        {
            Problem = await ApplyLockedAsync(on, chosen, ct);
            if (Problem is not null && chosen)
                _logger.LogWarning("Soulseek sharing could not be turned {State}: {Problem}", on ? "on" : "off", Problem);
            return Problem;
        }
        finally
        {
            _lock.Release();
        }
    }

    private async Task<string?> ApplyLockedAsync(bool on, bool chosen, CancellationToken ct)
    {
        var desired = on ? _settings.CurrentValue.SharedFolderList : [];
        if (on && desired.Count == 0)
            return "No folder to share is set (Soulseek.SharedFolders is empty).";

        var (status, yaml) = await ReadFile(ct);
        if (status == 403)
        {
            Control = ShareControl.Locked;
            return "slskd does not let Octo change its shares, because its remote configuration is off. Set SLSKD_REMOTE_CONFIGURATION=true on the slskd container, or set shares in slskd itself.";
        }
        if (yaml is null)
        {
            Control = ShareControl.Unknown;
            return "Octo cannot reach slskd.";
        }

        var (rest, ours) = SplitBlock(yaml);
        var actual = ParseDirectories(await ReadOptions(ct));
        if (HasOwnShares(rest))
        {
            Control = ShareControl.Outside;
            return "slskd.yml has a shares section of its own, so slskd's shares are set there, not here. Remove that section to let this switch decide.";
        }
        if (ours is null && !chosen)
        {
            // Never written by Octo. Nothing to do when slskd already shares nothing and the switch
            // is off; shares from somewhere else are left alone until someone flips the switch.
            if (actual is { Count: 0 } && !on) { Control = ShareControl.Octo; return null; }
            if (actual is { Count: > 0 })
            {
                Control = ShareControl.Outside;
                return null;
            }
        }

        var wanted = WithBlock(rest, desired);
        var changed = !string.Equals(Normalize(wanted), Normalize(yaml), StringComparison.Ordinal);
        if (changed && await WriteFile(wanted, ct) is { } refused) return refused;

        // slskd applies a new settings file through its file watch, a moment after it is written.
        for (var poll = 0; poll < Polls && (actual is null || !actual.SequenceEqual(desired, StringComparer.Ordinal)); poll++)
        {
            await Delay(PollEvery, ct);
            actual = ParseDirectories(await ReadOptions(ct));
        }
        if (actual is null)
        {
            Control = ShareControl.Unknown;
            return "Octo cannot read slskd's settings to check the change.";
        }
        if (!actual.SequenceEqual(desired, StringComparer.Ordinal))
        {
            Control = ShareControl.Outside;
            return actual.Count > desired.Count
                ? $"slskd still shares {string.Join(", ", actual.Skip(desired.Count))}, set outside slskd.yml (for example SLSKD_SHARED_DIR on the slskd container). Remove it there and restart slskd to let this switch decide."
                : "slskd did not take the new share list. Its settings file watch may be off; restart slskd.";
        }

        Control = ShareControl.Octo;
        // A new list needs a look through the folders, and so does one slskd has not looked
        // through yet: a look asked for while slskd was still finishing the last is dropped.
        // A look slskd is running by itself (its daily one) is left to finish.
        var needsLook = changed
            || (ReadScanState(await ReadApplication(ct)) is { Busy: false } && !await LookedThroughAsync(on, ct));
        if (needsLook && await RescanAsync(on, ct) is { } rescanProblem)
            return rescanProblem;
        if (!on) await CancelUploadsAsync(ct);
        if (changed)
            _logger.LogInformation("Soulseek sharing turned {State}: {Folders}", on ? "on" : "off",
                on ? string.Join(", ", desired) : "nothing shared");
        return null;
    }

    /// <summary>
    /// A look through the folders, which is also what makes slskd drop what it no longer shares.
    /// slskd 0.26 answers a rescan request with OK even when it drops it because a look is still
    /// finishing (it saves and reloads its index for some seconds after the files are counted), so
    /// Octo waits for slskd to be idle first, stopping a look that read the old list, and then
    /// checks the look really started or already did its work.
    /// </summary>
    private async Task<string?> RescanAsync(bool on, CancellationToken ct)
    {
        for (var attempt = 0; attempt < 3; attempt++)
        {
            if (!await WaitIdleAsync(ct))
            {
                await CancelScan(ct);
                if (!await WaitIdleAsync(ct)) continue;
            }
            var refused = await Rescan(ct);
            if (refused is not null && !refused.Contains("already", StringComparison.OrdinalIgnoreCase)) return refused;
            for (var poll = 0; poll < Polls; poll++)
            {
                if (ReadScanState(await ReadApplication(ct)) is { Busy: true }) return null;
                if (await LookedThroughAsync(on, ct)) return null;
                await Delay(PollEvery, ct);
            }
        }
        return "slskd did not start looking through the folders; press Rescan shared folders.";
    }

    /// <summary>Waits until slskd is not looking through its folders. False when it still is.</summary>
    private async Task<bool> WaitIdleAsync(CancellationToken ct)
    {
        for (var poll = 0; poll < Polls; poll++)
        {
            if (ReadScanState(await ReadApplication(ct)) is { Busy: false }) return true;
            await Delay(PollEvery, ct);
        }
        return false;
    }

    /// <summary>
    /// Whether slskd's index matches the switch. On: every shared folder has counts, which slskd
    /// only gives a folder once it has looked through it. Off: slskd counts no file at all.
    /// </summary>
    private async Task<bool> LookedThroughAsync(bool on, CancellationToken ct)
    {
        var state = ReadScanState(await ReadApplication(ct));
        if (state is null || state.Busy) return false;
        if (!on) return state.Files == 0;
        var folders = SoulseekSharing.ParseFolders(await ReadShares(ct), null).Where(folder => !folder.Excluded).ToList();
        return folders.Count > 0 && folders.All(folder => folder.Files is not null);
    }

    internal sealed record ScanState(bool Busy, int? Files);

    /// <summary>slskd's share summary: busy while a look runs or is finishing (ready is false from
    /// the start of a look until its index is saved and reloaded).</summary>
    internal static ScanState? ReadScanState(string? applicationJson)
    {
        if (string.IsNullOrWhiteSpace(applicationJson)) return null;
        try
        {
            using var doc = JsonDocument.Parse(applicationJson);
            if (!TryGet(doc.RootElement, "shares", out var shares)) return null;
            bool Flag(string name) => TryGet(shares, name, out var v) && v.ValueKind == JsonValueKind.True;
            var ready = !TryGet(shares, "ready", out var r) || r.ValueKind != JsonValueKind.False;
            int? files = TryGet(shares, "files", out var f) && f.ValueKind == JsonValueKind.Number ? f.GetInt32() : null;
            return new ScanState(Flag("scanning") || !ready, files);
        }
        catch (JsonException) { return null; }
    }

    /// <summary>Off means nothing goes out: uploads still waiting or under way are cancelled.</summary>
    private async Task CancelUploadsAsync(CancellationToken ct)
    {
        foreach (var (user, id) in OpenUploads(await ReadUploads(ct)))
            await CancelUpload(user, id, ct);
    }

    internal static IEnumerable<(string User, string Id)> OpenUploads(string? uploadsJson)
    {
        if (string.IsNullOrWhiteSpace(uploadsJson)) yield break;
        JsonDocument doc;
        try { doc = JsonDocument.Parse(uploadsJson); }
        catch (JsonException) { yield break; }
        using (doc)
        {
            if (doc.RootElement.ValueKind != JsonValueKind.Array) yield break;
            foreach (var user in doc.RootElement.EnumerateArray())
            {
                var name = user.TryGetProperty("username", out var u) ? u.GetString() : null;
                if (name is null || !user.TryGetProperty("directories", out var dirs) || dirs.ValueKind != JsonValueKind.Array) continue;
                foreach (var dir in dirs.EnumerateArray())
                {
                    if (!dir.TryGetProperty("files", out var files) || files.ValueKind != JsonValueKind.Array) continue;
                    foreach (var file in files.EnumerateArray())
                    {
                        var state = file.TryGetProperty("state", out var s) ? s.GetString() ?? "" : "";
                        var id = file.TryGetProperty("id", out var i) ? i.GetString() : null;
                        if (id is not null && !state.Contains("Completed", StringComparison.OrdinalIgnoreCase))
                            yield return (name, id);
                    }
                }
            }
        }
    }

    /// <summary>slskd's share list from its options, or null when it did not say.</summary>
    internal static IReadOnlyList<string>? ParseDirectories(string? optionsJson)
    {
        if (string.IsNullOrWhiteSpace(optionsJson)) return null;
        try
        {
            using var doc = JsonDocument.Parse(optionsJson);
            if (!TryGet(doc.RootElement, "shares", out var shares)) return null;
            if (!TryGet(shares, "directories", out var dirs)) return [];
            return dirs.ValueKind == JsonValueKind.Array
                ? dirs.EnumerateArray().Where(d => d.ValueKind == JsonValueKind.String).Select(d => d.GetString()!).ToList()
                : [];
        }
        catch (JsonException) { return null; }
    }

    private static bool TryGet(JsonElement element, string name, out JsonElement value)
    {
        value = default;
        if (element.ValueKind != JsonValueKind.Object) return false;
        foreach (var property in element.EnumerateObject())
            if (string.Equals(property.Name, name, StringComparison.OrdinalIgnoreCase))
            {
                value = property.Value;
                return true;
            }
        return false;
    }

    /// <summary>The file without Octo's block, and the block, or null when there is none.</summary>
    internal static (string Others, string? Block) SplitBlock(string yaml)
    {
        var text = yaml.Replace("\r\n", "\n");
        var start = text.IndexOf(BlockStart, StringComparison.Ordinal);
        if (start < 0) return (text, null);
        var end = text.IndexOf(BlockEnd, start, StringComparison.Ordinal);
        if (end < 0) return (text, null);
        end += BlockEnd.Length;
        if (end < text.Length && text[end] == '\n') end++;
        return (text[..start] + text[end..], text[start..end]);
    }

    private static readonly Regex OwnShares = new(@"^shares\s*:", RegexOptions.Multiline | RegexOptions.Compiled);

    /// <summary>An uncommented top-level "shares:" outside Octo's block: the person's own.</summary>
    internal static bool HasOwnShares(string rest) => OwnShares.IsMatch(rest);

    /// <summary>The file with Octo's block at the end, sharing these folders, or nothing.</summary>
    internal static string WithBlock(string rest, IReadOnlyList<string> folders)
    {
        var block = new StringBuilder();
        block.Append(BlockStart).Append('\n').Append("shares:\n");
        if (folders.Count == 0) block.Append("  directories: []\n");
        else
        {
            block.Append("  directories:\n");
            foreach (var folder in folders)
                block.Append("    - '").Append(folder.Replace("'", "''")).Append("'\n");
        }
        block.Append(BlockEnd).Append('\n');
        var head = rest.TrimEnd('\n', ' ');
        return head.Length == 0 ? block.ToString() : head + "\n\n" + block;
    }

    private static string Normalize(string yaml) => yaml.Replace("\r\n", "\n").TrimEnd('\n', ' ');
}
