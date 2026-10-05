using System.Globalization;
using System.Text.Json;
using System.Text.Json.Serialization;
using Octo.Models.Download;
using Octo.Services.Common;

namespace Octo.Services.Local;

/// <summary>
/// Persistent, bounded log of songs Octo has fetched. Written to a JSON file next
/// to the settings file so it survives restarts and container recreation (the
/// config dir is a host bind-mount). Newest entries first; capped so it can't grow
/// without limit. Best-effort — a write failure never breaks a download.
/// </summary>
public class DownloadHistoryService
{
    private const int MaxEntries = 500;

    private readonly string _path;
    private readonly ILogger<DownloadHistoryService> _logger;
    private readonly object _lock = new();
    private List<DownloadHistoryEntry>? _cache;

    public DownloadHistoryService(string path, ILogger<DownloadHistoryService> logger)
    {
        _path = path;
        _logger = logger;
    }

    /// <summary>Append a fetched-song entry (newest first) and persist.</summary>
    public void Record(DownloadHistoryEntry entry)
    {
        lock (_lock)
        {
            var list = LoadLocked();
            list.Insert(0, entry);
            while (list.Count > MaxEntries) list.RemoveAt(list.Count - 1);
            SaveLocked(list);
        }
    }

    // ---- A download's log, kept with its entry (the dashboard's Fetched songs) ----------------

    /// <summary>Lines kept per saved log. The first line stays, and the newest ones after it.</summary>
    internal const int MaxLogLines = 80;

    /// <summary>Copies kept on one saved line: the best ones, which are what a search line is read for.</summary>
    internal const int MaxSavedCandidates = 12;

    /// <summary>Longest text, detail or file name kept on a saved line.</summary>
    internal const int MaxSavedText = 300;

    /// <summary>
    /// Room for every saved log together. Past it, the oldest entries let go of their logs (the
    /// entries themselves stay), so the file stays small enough to read whole on every change.
    /// </summary>
    internal const long MaxLogBytes = 2 * 1024 * 1024;

    /// <summary><see cref="MaxLogBytes"/>, smaller in tests.</summary>
    internal long LogRoom { get; set; } = MaxLogBytes;

    // What the file is written with: fields with no value are left out, which keeps saved logs
    // (where most of a copy's figures are often missing) small. Reading needs nothing special.
    private static readonly JsonSerializerOptions FileOptions = new()
    {
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
    };

    /// <summary>
    /// Saves a download's log with the entry it made: the newest entry with this key written
    /// since the run began, so a log never lands on an older fetch of the same song. Called
    /// again when a line arrives after the end (lyrics, an upgrade's verdict), and each time
    /// the whole log is saved anew. False when there is no such entry.
    /// </summary>
    public bool AttachLog(string key, DateTime startedUtc, IReadOnlyList<AcquisitionEvent> events)
    {
        if (string.IsNullOrWhiteSpace(key) || events.Count == 0) return false;
        lock (_lock)
        {
            var list = LoadLocked();
            var entry = list.FirstOrDefault(e => string.Equals(e.Key, key, StringComparison.Ordinal)
                                                 && WrittenAt(e) is { } at && at >= startedUtc.AddSeconds(-1));
            if (entry is null) return false;
            entry.Log = Trimmed(events);
            KeepLogsWithinRoom(list);
            SaveLocked(list);
            return true;
        }
    }

    /// <summary>The entry written at this moment for this key, as listed (DownloadedAt), or null.</summary>
    public DownloadHistoryEntry? Find(string key, string? downloadedAt)
    {
        if (string.IsNullOrWhiteSpace(key)) return null;
        lock (_lock)
        {
            return LoadLocked().FirstOrDefault(e => string.Equals(e.Key, key, StringComparison.Ordinal)
                && (downloadedAt is null || string.Equals(e.DownloadedAt, downloadedAt, StringComparison.Ordinal)));
        }
    }

    internal static DateTime? WrittenAt(DownloadHistoryEntry entry) =>
        DateTime.TryParse(entry.DownloadedAt, CultureInfo.InvariantCulture,
            DateTimeStyles.AdjustToUniversal | DateTimeStyles.AssumeUniversal, out var at) ? at : null;

    /// <summary>
    /// A log as it is saved: at most <see cref="MaxLogLines"/> lines (the first, a line saying how
    /// many were left out, and the newest), each line's words and copies capped.
    /// </summary>
    internal static List<AcquisitionEvent> Trimmed(IReadOnlyList<AcquisitionEvent> events)
    {
        static string? Cap(string? text) =>
            text is null || text.Length <= MaxSavedText ? text : text[..(MaxSavedText - 3)].TrimEnd() + "...";

        static AcquisitionEvent Line(AcquisitionEvent line) => line with
        {
            Text = Cap(line.Text) ?? "",
            Detail = Cap(line.Detail),
            Candidates = line.Candidates is { Count: > 0 } copies
                ? copies.Take(MaxSavedCandidates).Select(copy => copy with
                {
                    File = Cap(copy.File), Folder = Cap(copy.Folder), Note = Cap(copy.Note),
                }).ToList()
                : null,
        };

        if (events.Count <= MaxLogLines) return events.Select(Line).ToList();
        var left = events.Count - (MaxLogLines - 1);
        var kept = new List<AcquisitionEvent> { Line(events[0]) };
        var tail = events.Skip(events.Count - (MaxLogLines - 2)).ToList();
        kept.Add(new AcquisitionEvent(tail[0].At, AcquisitionEventKinds.Note,
            $"{left} {(left == 1 ? "line" : "lines")} left out to keep the saved log short"));
        kept.AddRange(tail.Select(Line));
        return kept;
    }

    /// <summary>The newest entries keep their logs; once they fill the room, older ones let go.
    /// Caller holds the lock.</summary>
    private void KeepLogsWithinRoom(List<DownloadHistoryEntry> list)
    {
        long used = 0;
        var full = false;
        foreach (var entry in list)
        {
            if (entry.Log is null) continue;
            if (!full)
            {
                var size = JsonSerializer.SerializeToUtf8Bytes(entry.Log, FileOptions).Length;
                if (used + size <= LogRoom)
                {
                    used += size;
                    continue;
                }
                full = true;
            }
            entry.Log = null;
        }
    }

    /// <summary>The most recent entries, newest first.</summary>
    public IReadOnlyList<DownloadHistoryEntry> GetRecent(int limit = 200)
    {
        lock (_lock)
        {
            return LoadLocked().Take(Math.Max(0, limit)).ToList();
        }
    }

    private List<DownloadHistoryEntry> LoadLocked()
    {
        if (_cache != null) return _cache;
        try
        {
            if (File.Exists(_path))
            {
                var json = File.ReadAllText(_path);
                _cache = string.IsNullOrWhiteSpace(json)
                    ? new List<DownloadHistoryEntry>()
                    : JsonSerializer.Deserialize<List<DownloadHistoryEntry>>(json) ?? new List<DownloadHistoryEntry>();
            }
            else
            {
                _cache = new List<DownloadHistoryEntry>();
            }
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Download history load failed ({Msg}); starting fresh", ex.Message);
            _cache = new List<DownloadHistoryEntry>();
        }
        return _cache;
    }

    private void SaveLocked(List<DownloadHistoryEntry> list)
    {
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(_path) ?? ".");
            var tmp = _path + ".tmp";
            File.WriteAllText(tmp, JsonSerializer.Serialize(list, FileOptions));
            File.Move(tmp, _path, overwrite: true);
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Download history save failed: {Msg}", ex.Message);
        }
    }
}
