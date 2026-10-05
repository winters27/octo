using System.Globalization;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Text.RegularExpressions;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;

namespace Octo.Services.Soulseek;

/// <summary>One folder slskd shares, as slskd reports it. Alias is the name other people see.</summary>
public sealed record SharedFolder(string Alias, string Path, bool Excluded, int? Directories, int? Files);

/// <summary>What people have downloaded from you. Now is what is moving or waiting this minute;
/// the rest covers the last seven days.</summary>
public sealed record UploadActivity(int Sending, int Waiting, int Files, long Bytes, int People, int Failed,
    DateTime? LastUploadAt);

/// <summary>Written as its name, which is what the dashboard reads.</summary>
[JsonConverter(typeof(JsonStringEnumConverter<PortState>))]
public enum PortState { Unknown, Open, Closed, Off }

/// <summary>Soulseek's port test's answer for slskd's listening port. CheckedAt is when it was
/// asked; Error says why there is no answer.</summary>
public sealed record PortCheckResult(PortState State, int? Port, DateTime? CheckedAt, string? Error);

/// <summary>Something about sharing that needs a person. Code is stable for the dashboard; Text
/// is what it says.</summary>
public sealed record SharingWarning(string Code, string Text);

/// <summary>
/// The Sharing card on the Soulseek page: whether this server gives back to the network, built
/// from slskd's own answers. Counts are null when slskd did not say.
/// </summary>
public sealed record SharingReport(
    bool Reachable,
    string Login,
    IReadOnlyList<SharedFolder> Folders,
    int? Directories,
    int? Files,
    bool Scanning,
    double? ScanProgress,
    bool ScanFailed,
    int? NetworkFiles,
    int? NetworkDirectories,
    int? UploadSlots,
    int? UploadSpeedLimitKiB,
    int? RescanMinutes,
    UploadActivity Uploads,
    int? ListenPort,
    PortCheckResult Port,
    ShareSwitchState Switch,
    IReadOnlyList<SharingWarning> Warnings);

/// <summary>The Share my library switch: what it is set to, who decides slskd's shares, and why the
/// switch could not do its job the last time it tried, if it could not.</summary>
public sealed record ShareSwitchState(bool On, ShareControl Control, string? Problem);

/// <summary>
/// Reads how slskd shares with the Soulseek network and says plainly what is wrong with it.
/// Soulseek works because people share: many users refuse to send files to someone who shares
/// nothing, and a closed listening port stops most people reaching you at all. Everything here
/// reads, apart from asking slskd to look through its shared folders again; turning sharing on and
/// off is SoulseekShareSwitch.
/// </summary>
public sealed class SoulseekSharing
{
    private readonly SoulseekClient _client;
    private readonly SoulseekPortCheck _port;
    private readonly IOptionsMonitor<SoulseekSettings> _settings;

    private readonly SoulseekShareSwitch _switch;

    public SoulseekSharing(SoulseekClient client, SoulseekPortCheck port, SoulseekShareSwitch shareSwitch,
        IOptionsMonitor<SoulseekSettings> settings)
    {
        _client = client;
        _port = port;
        _switch = shareSwitch;
        _settings = settings;
        Read = (path, ct) => _client.ReadAsync(path, ct);
    }

    // A seam, so the report can be built from scripted slskd answers.
    internal Func<string, CancellationToken, Task<string?>> Read { get; set; }
    internal Func<DateTime> Clock { get; set; } = () => DateTime.UtcNow;

    /// <summary>The report as it stands. TestPort asks Soulseek's port test now rather than using
    /// its last answer. ShareOn stands in for the saved switch just after it was flipped, before
    /// the settings file has been read back.</summary>
    public async Task<SharingReport> ReportAsync(bool testPort, CancellationToken ct, bool? shareOn = null)
    {
        var app = Read("application", ct);
        var options = Read("options", ct);
        var shares = Read("shares", ct);
        var uploads = Read("transfers/uploads?includeRemoved=true", ct);
        await Task.WhenAll(app, options, shares, uploads);

        var listenPort = ParseListenPort(options.Result);
        var port = listenPort is { } p
            ? await _port.CheckAsync(p, testPort, ct)
            : new PortCheckResult(_settings.CurrentValue.CheckListenPort ? PortState.Unknown : PortState.Off, null, null, null);

        var settings = _settings.CurrentValue;
        var defaultLogin = string.Equals(settings.Username?.Trim(), "slskd", StringComparison.Ordinal)
                           && string.Equals(settings.Password, "slskd", StringComparison.Ordinal);
        var shareSwitch = new ShareSwitchState(shareOn ?? settings.ShareLibrary, _switch.Control, _switch.Problem);
        return Build(app.Result, options.Result, shares.Result, uploads.Result, port, shareSwitch, defaultLogin, Clock());
    }

    /// <summary>Starts a share rescan in slskd. Null when it started, otherwise why not.</summary>
    public Task<string?> RescanAsync(CancellationToken ct) => _client.RescanSharesAsync(ct);

    /// <summary>
    /// Puts slskd's answers together. Any of them may be null, which reads as "slskd did not
    /// say", never as zero. No application answer at all means slskd is out of reach.
    /// </summary>
    internal static SharingReport Build(string? appJson, string? optionsJson, string? sharesJson,
        string? uploadsJson, PortCheckResult port, ShareSwitchState shareSwitch, bool defaultLogin, DateTime now)
    {
        var none = new UploadActivity(0, 0, 0, 0, 0, 0, null);
        if (appJson is null)
            return new SharingReport(false, "Unknown", [], null, null, false, null, false, null, null, null, null, null,
                none, null, port, shareSwitch, [new SharingWarning("unreachable",
                    "Octo cannot reach slskd, so it cannot tell what you share.")]);

        var login = SoulseekClient.ParseServerReading(appJson).Link;
        int? directories = null, files = null, networkFiles = null, networkDirectories = null;
        bool scanning = false, faulted = false;
        double? progress = null;
        if (TryParse(appJson) is { } app)
        {
            if (Child(app, "shares") is { } summary)
            {
                directories = Int(summary, "directories");
                files = Int(summary, "files");
                scanning = Bool(summary, "scanning") == true || Bool(summary, "scanPending") == true;
                faulted = Bool(summary, "faulted") == true;
                progress = Double(summary, "scanProgress");
            }
            if (Child(app, "user") is { } user && Child(user, "statistics") is { } stats)
            {
                networkFiles = Int(stats, "fileCount");
                networkDirectories = Int(stats, "directoryCount");
            }
        }

        var folders = ParseFolders(sharesJson, optionsJson);
        var (slots, speed, rescan) = ParseUploadOptions(optionsJson);
        var uploads = uploadsJson is null ? none : ParseUploads(uploadsJson, now);
        var listenPort = ParseListenPort(optionsJson);

        var warnings = new List<SharingWarning>();
        if (defaultLogin)
            warnings.Add(new("defaultLogin",
                "slskd still has its default sign-in, slskd and slskd, so anyone on your network can open it. Set SLSKD_USERNAME and SLSKD_PASSWORD in .env, then the same under Connection below."));
        if (login == SoulseekLinkState.NotLoggedIn)
            warnings.Add(new("signedOut",
                "slskd is not signed in to Soulseek right now, so nobody can download from you until it is."));
        var shared = folders.Where(folder => !folder.Excluded).ToList();
        if (shareSwitch.Problem is { } problem)
            warnings.Add(new("switch", problem));
        // Sharing off by choice is a choice: the card says so plainly, and it is not a warning.
        var offByChoice = !shareSwitch.On && shareSwitch.Control is ShareControl.Octo or ShareControl.Unknown;
        if (shared.Count == 0 && optionsJson is not null && !offByChoice && shareSwitch.Problem is null)
            warnings.Add(new("nothingShared",
                "You share nothing. Many people on Soulseek will not send files to someone who shares nothing, so your downloads fail more often."));
        else if (shared.Count > 0 && files == 0 && !scanning && !faulted)
            warnings.Add(new("emptyShares",
                $"slskd shares {string.Join(", ", shared.Select(folder => folder.Path))} but found no files there. Check that the folder is mounted into the slskd container, then rescan."));
        if (faulted)
            warnings.Add(new("scanFailed",
                "slskd's last look through your shared folders failed. Rescan, and read slskd's log if it fails again."));
        if (port.State == PortState.Closed)
            warnings.Add(new("portClosed", shared.Count > 0
                ? $"Other people cannot connect to slskd on port {port.Port}. Forward TCP port {port.Port} on your router to this server. Until then, only people whose own port is open can download from you, and some of your downloads fail too."
                : $"Other people cannot connect to slskd on port {port.Port}, so some of your downloads fail. Forward TCP port {port.Port} on your router to this server."));

        return new SharingReport(true, login.ToString(), folders, directories, files, scanning, progress, faulted,
            networkFiles, networkDirectories, slots, speed, rescan, uploads, listenPort, port, shareSwitch, warnings);
    }

    /// <summary>
    /// The shared folders from /shares, which carries each one's counts; from the options when
    /// slskd gave no shares answer. A raw entry is "[Alias]/path", or "!/path" for a folder kept
    /// out of a share.
    /// </summary>
    internal static IReadOnlyList<SharedFolder> ParseFolders(string? sharesJson, string? optionsJson)
    {
        var folders = new List<SharedFolder>();
        if (TryParse(sharesJson) is { ValueKind: JsonValueKind.Object } hosts)
        {
            foreach (var host in hosts.EnumerateObject())
            {
                if (host.Value.ValueKind != JsonValueKind.Array) continue;
                foreach (var share in host.Value.EnumerateArray())
                    folders.Add(new SharedFolder(
                        Text(share, "alias") ?? "",
                        Text(share, "localPath") ?? Text(share, "raw") ?? "",
                        Bool(share, "isExcluded") == true,
                        Int(share, "directories"),
                        Int(share, "files")));
            }
            return folders;
        }

        if (TryParse(optionsJson) is { } options && Child(options, "shares") is { } sharesOptions
            && Child(sharesOptions, "directories") is { ValueKind: JsonValueKind.Array } raw)
            foreach (var entry in raw.EnumerateArray())
                if (entry.ValueKind == JsonValueKind.String && FolderFromRaw(entry.GetString()!) is { } folder)
                    folders.Add(folder);
        return folders;
    }

    private static readonly Regex AliasPrefix = new(@"^\[(?<alias>[^\]]+)\](?<path>.+)$", RegexOptions.Compiled);

    internal static SharedFolder? FolderFromRaw(string raw)
    {
        raw = raw.Trim();
        if (raw.Length == 0) return null;
        var excluded = raw[0] is '!' or '-';
        if (excluded) raw = raw[1..];
        var match = AliasPrefix.Match(raw);
        var path = match.Success ? match.Groups["path"].Value : raw;
        var alias = match.Success
            ? match.Groups["alias"].Value
            : path.TrimEnd('/', '\\').Split('/', '\\').LastOrDefault() ?? path;
        return new SharedFolder(alias, path, excluded, null, null);
    }

    /// <summary>Upload slots, the total upload speed limit in KiB/s, and the rescan interval in
    /// minutes. slskd's "no limit" (int.MaxValue) reads as null.</summary>
    internal static (int? Slots, int? SpeedLimitKiB, int? RescanMinutes) ParseUploadOptions(string? optionsJson)
    {
        if (TryParse(optionsJson) is not { } options) return (null, null, null);
        int? slots = null, speed = null, rescan = null;
        if (Child(options, "transfers") is { } transfers && Child(transfers, "upload") is { } upload)
        {
            slots = Int(upload, "slots");
            speed = Int(upload, "speedLimit") is { } limit && limit < int.MaxValue ? limit : null;
        }
        if (Child(options, "shares") is { } shares && Child(shares, "cache") is { } cache)
            rescan = Int(cache, "retention");
        return (slots, speed, rescan);
    }

    /// <summary>slskd's Soulseek listening port, the one other people connect to.</summary>
    internal static int? ParseListenPort(string? optionsJson) =>
        TryParse(optionsJson) is { } options && Child(options, "soulseek") is { } soulseek
            ? Int(soulseek, "listenPort")
            : null;

    /// <summary>
    /// What people downloaded from you, from slskd's uploads list (user, then folder, then file).
    /// A state is slskd's flags in words, such as "InProgress", "Queued, Locally" or
    /// "Completed, Succeeded". Finished uploads count when they ended in the last seven days.
    /// </summary>
    internal static UploadActivity ParseUploads(string uploadsJson, DateTime now)
    {
        if (TryParse(uploadsJson) is not { ValueKind: JsonValueKind.Array } users)
            return new UploadActivity(0, 0, 0, 0, 0, 0, null);

        var since = now.AddDays(-7);
        int sending = 0, waiting = 0, files = 0, failed = 0;
        long bytes = 0;
        var people = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        DateTime? last = null;
        foreach (var user in users.EnumerateArray())
        {
            var name = Text(user, "username") ?? "";
            if (Child(user, "directories") is not { ValueKind: JsonValueKind.Array } dirs) continue;
            foreach (var dir in dirs.EnumerateArray())
            {
                if (Child(dir, "files") is not { ValueKind: JsonValueKind.Array } list) continue;
                foreach (var file in list.EnumerateArray())
                {
                    var state = Text(file, "state") ?? "";
                    if (Has(state, "InProgress") || Has(state, "Initializing")) { sending++; continue; }
                    if (Has(state, "Queued") || Has(state, "Requested")) { waiting++; continue; }
                    if (!Has(state, "Completed")) continue;
                    var ended = Date(file, "endedAt");
                    if (ended is null || ended < since) continue;
                    if (Has(state, "Succeeded"))
                    {
                        files++;
                        bytes += Long(file, "size") ?? 0;
                        people.Add(name);
                        if (last is null || ended > last) last = ended;
                    }
                    else if (Has(state, "Errored") || Has(state, "TimedOut") || Has(state, "Rejected")
                             || Has(state, "Aborted"))
                        failed++;
                }
            }
        }
        return new UploadActivity(sending, waiting, files, bytes, people.Count, failed, last);
    }

    private static bool Has(string state, string flag) => state.Contains(flag, StringComparison.OrdinalIgnoreCase);

    private static JsonElement? TryParse(string? json)
    {
        if (string.IsNullOrWhiteSpace(json)) return null;
        try
        {
            using var doc = JsonDocument.Parse(json);
            return doc.RootElement.Clone();
        }
        catch (JsonException) { return null; }
    }

    private static JsonElement? Child(JsonElement element, string name)
    {
        if (element.ValueKind != JsonValueKind.Object) return null;
        foreach (var property in element.EnumerateObject())
            if (string.Equals(property.Name, name, StringComparison.OrdinalIgnoreCase))
                return property.Value;
        return null;
    }

    private static string? Text(JsonElement element, string name) =>
        Child(element, name) is { ValueKind: JsonValueKind.String } value ? value.GetString() : null;

    private static int? Int(JsonElement element, string name) =>
        Child(element, name) is { ValueKind: JsonValueKind.Number } value && value.TryGetInt32(out var n) ? n : null;

    private static long? Long(JsonElement element, string name) =>
        Child(element, name) is { ValueKind: JsonValueKind.Number } value && value.TryGetInt64(out var n) ? n : null;

    private static double? Double(JsonElement element, string name) =>
        Child(element, name) is { ValueKind: JsonValueKind.Number } value && value.TryGetDouble(out var n) ? n : null;

    private static bool? Bool(JsonElement element, string name) =>
        Child(element, name) is { ValueKind: JsonValueKind.True or JsonValueKind.False } value ? value.GetBoolean() : null;

    private static DateTime? Date(JsonElement element, string name) =>
        Text(element, name) is { } text && DateTime.TryParse(text, CultureInfo.InvariantCulture,
            DateTimeStyles.AdjustToUniversal | DateTimeStyles.AssumeUniversal, out var at)
            ? at
            : null;
}

/// <summary>
/// Asks Soulseek's own port test whether other people can connect to slskd. The test connects
/// back to the address the request came from, so it answers for this server's internet address;
/// that is slskd's too unless slskd goes out another way, such as through a VPN. It sends the port
/// number and nothing else. One answer is kept for 6 hours, and Test asks again at most every 30
/// seconds.
/// </summary>
public sealed class SoulseekPortCheck
{
    public const string ClientName = "soulseek-port-check";
    internal const string TestUrl = "http://tools.slsknet.org/porttest.php?port={0}";
    internal static readonly TimeSpan KeepFor = TimeSpan.FromHours(6);
    internal static readonly TimeSpan TestAtMostEvery = TimeSpan.FromSeconds(30);

    private readonly IHttpClientFactory _http;
    private readonly IOptionsMonitor<SoulseekSettings> _settings;
    private readonly ILogger<SoulseekPortCheck> _logger;
    private readonly SemaphoreSlim _lock = new(1, 1);
    private PortCheckResult? _last;
    private DateTime _lastAskedAt = DateTime.MinValue;

    public SoulseekPortCheck(IHttpClientFactory http, IOptionsMonitor<SoulseekSettings> settings,
        ILogger<SoulseekPortCheck> logger)
    {
        _http = http;
        _settings = settings;
        _logger = logger;
    }

    internal Func<DateTime> Clock { get; set; } = () => DateTime.UtcNow;

    public async Task<PortCheckResult> CheckAsync(int port, bool force, CancellationToken ct)
    {
        if (!_settings.CurrentValue.CheckListenPort) return new PortCheckResult(PortState.Off, port, null, null);

        await _lock.WaitAsync(ct);
        try
        {
            var age = Clock() - _lastAskedAt;
            if (_last is { } last && last.Port == port
                && (force ? age < TestAtMostEvery : age < KeepFor))
                return last;

            _lastAskedAt = Clock();
            try
            {
                using var client = _http.CreateClient(ClientName);
                var answer = await client.GetStringAsync(string.Format(CultureInfo.InvariantCulture, TestUrl, port), ct);
                var state = ParseAnswer(answer, port);
                _last = new PortCheckResult(state, port, Clock(),
                    state == PortState.Unknown ? "Soulseek's port test gave an answer Octo does not understand." : null);
            }
            catch (Exception ex) when (!ct.IsCancellationRequested)
            {
                _logger.LogInformation("Soulseek port test did not answer: {Msg}", ex.Message);
                _last = new PortCheckResult(PortState.Unknown, port, Clock(), "Soulseek's port test did not answer.");
            }
            return _last;
        }
        finally
        {
            _lock.Release();
        }
    }

    private static readonly Regex Answer = new(@"Port:\s*(?<port>\d+)/tcp\s+(?<state>OPEN|CLOSED)",
        RegexOptions.IgnoreCase | RegexOptions.Compiled);

    /// <summary>The test's page says "Port: 50300/tcp OPEN" or "... CLOSED". Anything else, or an
    /// answer about another port, is Unknown.</summary>
    internal static PortState ParseAnswer(string page, int port)
    {
        var match = Answer.Match(page ?? "");
        if (!match.Success || match.Groups["port"].Value != port.ToString(CultureInfo.InvariantCulture))
            return PortState.Unknown;
        return string.Equals(match.Groups["state"].Value, "OPEN", StringComparison.OrdinalIgnoreCase)
            ? PortState.Open
            : PortState.Closed;
    }
}
