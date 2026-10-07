using System.Globalization;
using System.Text.RegularExpressions;

namespace Octo.Services.Updates;

/// <summary>The host helper, as it describes itself in config/update/helper.</summary>
public sealed record UpdateHelperInfo(string Version, string Mode, string? Dir, DateTime? InstalledUtc);

/// <summary>One update run, as the host helper reports it in config/update/status.</summary>
public sealed record UpdateRunStatus(
    string Id, string? Tag, string? From, string State, string? Step, string? Error,
    DateTime? StartedUtc, DateTime? FinishedUtc);

/// <summary>The states the helper writes, in the order a run passes through them.</summary>
public static class UpdateRunStates
{
    public const string Accepted = "accepted";
    public const string Fetching = "fetching";
    public const string Building = "building";
    public const string Restarting = "restarting";
    public const string Done = "done";
    public const string Failed = "failed";

    public static bool Running(string state) => state is Accepted or Fetching or Building or Restarting;
}

/// <summary>
/// The exchange with the host helper, through files in config/update/. Octo writes a request; a
/// small systemd service on the host (scripts/updater) picks it up, updates and restarts Octo,
/// and writes its progress back. Octo never touches Docker itself.
///
/// Every file is plain key=value lines, so the helper needs no JSON tool, and each is written to
/// a temporary name and renamed, so neither side ever reads half a file.
/// </summary>
public sealed partial class UpdateHost
{
    /// <summary>How long a request may wait for the helper before Octo gives up on it.</summary>
    public static readonly TimeSpan AnswerTimeout = TimeSpan.FromSeconds(90);

    /// <summary>A run that has said nothing for this long has stopped, whatever its last state.</summary>
    public static readonly TimeSpan RunTimeout = TimeSpan.FromMinutes(40);

    private readonly string _dir;
    private readonly Func<DateTime> _clock;
    private readonly ILogger<UpdateHost> _logger;
    private readonly object _lock = new();

    public UpdateHost(string dir, ILogger<UpdateHost> logger, Func<DateTime>? clock = null)
    {
        _dir = dir;
        _logger = logger;
        _clock = clock ?? (() => DateTime.UtcNow);
    }

    private string RequestPath => Path.Combine(_dir, "request");
    private string HelperPath => Path.Combine(_dir, "helper");
    private string StatusPath => Path.Combine(_dir, "status");
    private string LogPath => Path.Combine(_dir, "log");

    /// <summary>The id of the last request the helper never answered, so the page can say so.</summary>
    public string? Unanswered { get; private set; }

    [GeneratedRegex(@"^[A-Za-z0-9._@-]{1,64}$")]
    private static partial Regex SafeWord();

    public UpdateHelperInfo? Helper()
    {
        var values = Read(HelperPath);
        if (values is null) return null;
        var mode = values.GetValueOrDefault("mode") is "image" ? "image" : "build";
        return new UpdateHelperInfo(values.GetValueOrDefault("version") ?? "1", mode,
            values.GetValueOrDefault("dir") is { Length: > 0 } dir ? dir : null, Time(values.GetValueOrDefault("installed")));
    }

    public UpdateRunStatus? Status()
    {
        var values = Read(StatusPath);
        if (values?.GetValueOrDefault("id") is not { Length: > 0 } id) return null;
        return new UpdateRunStatus(id, values.GetValueOrDefault("tag"), values.GetValueOrDefault("from"),
            values.GetValueOrDefault("state") ?? UpdateRunStates.Failed, values.GetValueOrDefault("step"),
            values.GetValueOrDefault("error"), Time(values.GetValueOrDefault("started")), Time(values.GetValueOrDefault("finished")));
    }

    /// <summary>The last lines of the helper's log, for a run that failed.</summary>
    public IReadOnlyList<string> LogTail(int lines = 40)
    {
        try
        {
            return File.Exists(LogPath) ? File.ReadLines(LogPath).TakeLast(lines).ToList() : [];
        }
        catch (IOException)
        {
            return [];
        }
    }

    /// <summary>The id of a request still waiting for the helper, after dropping one it has ignored too long.</summary>
    public string? Pending()
    {
        lock (_lock)
        {
            var values = Read(RequestPath);
            if (values?.GetValueOrDefault("id") is not { Length: > 0 } id) return null;
            if (Status()?.Id == id) return null;
            var at = Time(values.GetValueOrDefault("at")) ?? File.GetLastWriteTimeUtc(RequestPath);
            if (_clock() - at < AnswerTimeout) return id;
            // Nobody picked it up. It is removed so a helper installed later never runs a stale request.
            TryDelete(RequestPath);
            Unanswered = id;
            _logger.LogWarning("The update request {Id} was never picked up by the host helper; dropped it", id);
            return null;
        }
    }

    /// <summary>Whether a run is under way: a request waiting, or the helper still working on one.</summary>
    public bool Busy()
    {
        if (Pending() is not null) return true;
        var status = Status();
        return status is not null && UpdateRunStates.Running(status.State)
               && _clock() - (status.StartedUtc ?? DateTime.MinValue) < RunTimeout;
    }

    /// <summary>Writes a request for the helper and returns its id.</summary>
    public string Request(string tag, string requestedBy)
    {
        if (!ReleaseVersion.TryParse(tag, out _) || tag.Contains('+'))
            throw new ArgumentException($"\"{tag}\" is not a release.", nameof(tag));
        lock (_lock)
        {
            var id = Guid.NewGuid().ToString("D");
            var by = SafeWord().IsMatch(requestedBy) ? requestedBy : "dashboard";
            Write(RequestPath, new Dictionary<string, string>
            {
                ["id"] = id,
                ["tag"] = tag,
                ["by"] = by,
                ["at"] = _clock().ToString("yyyy-MM-ddTHH:mm:ssZ", CultureInfo.InvariantCulture),
            });
            Unanswered = null;
            return id;
        }
    }

    internal static Dictionary<string, string>? Parse(IEnumerable<string> lines)
    {
        var values = new Dictionary<string, string>(StringComparer.Ordinal);
        foreach (var line in lines)
        {
            var split = line.IndexOf('=');
            if (split <= 0) continue;
            values[line[..split].Trim()] = line[(split + 1)..].Trim();
        }
        return values;
    }

    private Dictionary<string, string>? Read(string path)
    {
        try
        {
            return File.Exists(path) ? Parse(File.ReadAllLines(path)) : null;
        }
        catch (IOException ex)
        {
            _logger.LogDebug("Could not read {Path}: {Message}", path, ex.Message);
            return null;
        }
        catch (UnauthorizedAccessException ex)
        {
            _logger.LogDebug("Could not read {Path}: {Message}", path, ex.Message);
            return null;
        }
    }

    private static void Write(string path, IReadOnlyDictionary<string, string> values)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        var temp = path + ".tmp";
        File.WriteAllText(temp, string.Concat(values.Select(pair => $"{pair.Key}={pair.Value}\n")));
        File.Move(temp, path, overwrite: true);
    }

    private static void TryDelete(string path)
    {
        try
        {
            File.Delete(path);
        }
        catch (IOException)
        {
        }
        catch (UnauthorizedAccessException)
        {
        }
    }

    private static DateTime? Time(string? text) =>
        DateTime.TryParse(text, CultureInfo.InvariantCulture,
            DateTimeStyles.AdjustToUniversal | DateTimeStyles.AssumeUniversal, out var time) ? time : null;
}
