using System.Text.Json;

namespace Octo.Services.Metadata;

/// <summary>
/// Which files a run touches. A REQUEST parameter, never a setting, so "the whole library"
/// can never be left switched on and fire again later.
/// </summary>
public enum GenreBackfillScope
{
    /// <summary>Only files Octo downloaded, from .mappings.json. Rewriting these is rewriting
    /// Octo's own output.</summary>
    OctoDownloads,

    /// <summary>Every audio file under the music root, including rips and purchases Octo never
    /// touched. Gated harder for that reason.</summary>
    WholeLibrary,
}

public enum GenreBackfillStatus
{
    Idle,
    Running,
    Completed,
    Cancelled,

    /// <summary>The process stopped mid-run. It does NOT auto-resume: a user may have
    /// restarted specifically to stop it.</summary>
    Interrupted,

    /// <summary>Too many files in a row could not be written, so the run gave up rather than
    /// producing the same error two thousand times.</summary>
    Failed,
}

/// <summary>One file the run would change, or did. The preview is built from these.</summary>
public sealed record GenreBackfillChange(
    string Path, IReadOnlyList<string> Before, IReadOnlyList<string> After,
    string Action, string? Rule);

public sealed class GenreBackfillRun
{
    public string RunId { get; set; } = "";
    public GenreBackfillStatus Status { get; set; } = GenreBackfillStatus.Idle;
    public GenreBackfillScope Scope { get; set; }
    public bool DryRun { get; set; } = true;
    public DateTime? StartedUtc { get; set; }
    public DateTime? FinishedUtc { get; set; }
    public int Total { get; set; }
    public int Processed { get; set; }
    public int Changed { get; set; }
    public int Cleared { get; set; }
    public int Skipped { get; set; }
    public int Failed { get; set; }
    public int Cursor { get; set; }
    public string? LastPath { get; set; }
    public string? Reason { get; set; }
    public List<string> Errors { get; set; } = [];
    public List<GenreBackfillChange> Preview { get; set; } = [];

    /// <summary>Which genre settings this run used (GenreBackfillWorker.HashSettings). Apply
    /// re-plans from the settings in force when it runs, so a preview is only a true description
    /// of an apply while these still match. Null on runs recorded before this existed.</summary>
    public string? SettingsHash { get; set; }

    /// <summary>The queue this run walks, persisted so a resume does not re-enumerate into a
    /// different order and skip files.</summary>
    public List<string> Queue { get; set; } = [];

    public bool CanResume => Status is GenreBackfillStatus.Cancelled or GenreBackfillStatus.Interrupted
        && Cursor < Queue.Count;
}

/// <summary>
/// The run's progress, on disk.
///
/// Persisted with the ExternalIdRegistry idiom (dirty bit, coalesced flush, atomic
/// temp-and-rename) for the same reason: flushing per file would turn a 1,900-file walk into
/// 1,900 extra writes, and a torn write would lose the cursor a resume depends on.
/// </summary>
public sealed class GenreBackfillStore : IDisposable
{
    private static readonly TimeSpan FlushInterval = TimeSpan.FromSeconds(10);

    /// <summary>Enough rows to review before applying, bounded so a whole-library preview
    /// cannot grow the state file without limit.</summary>
    public const int MaxPreviewRows = 500;

    private const int MaxErrors = 20;

    private readonly string? _path;
    private readonly ILogger<GenreBackfillStore>? _logger;
    private readonly Timer? _flushTimer;
    private readonly object _lock = new();
    private int _dirty;

    private GenreBackfillRun _run = new();

    public GenreBackfillStore(string? path = null, ILogger<GenreBackfillStore>? logger = null)
    {
        _path = string.IsNullOrWhiteSpace(path) ? null : path;
        _logger = logger;
        if (_path is null) return;

        Load();
        _flushTimer = new Timer(_ => Flush(), null, FlushInterval, FlushInterval);
    }

    public GenreBackfillRun Current
    {
        get { lock (_lock) return _run; }
    }

    public void Update(Action<GenreBackfillRun> mutate)
    {
        lock (_lock)
        {
            mutate(_run);
            if (_run.Errors.Count > MaxErrors) _run.Errors.RemoveRange(0, _run.Errors.Count - MaxErrors);
            if (_run.Preview.Count > MaxPreviewRows) _run.Preview.RemoveRange(MaxPreviewRows, _run.Preview.Count - MaxPreviewRows);
        }
        Interlocked.Exchange(ref _dirty, 1);
    }

    public void Replace(GenreBackfillRun run)
    {
        lock (_lock) _run = run;
        Interlocked.Exchange(ref _dirty, 1);
        Flush();
    }

    private void Load()
    {
        try
        {
            if (!File.Exists(_path)) return;
            var run = JsonSerializer.Deserialize<GenreBackfillRun>(File.ReadAllText(_path!));
            if (run is null) return;

            // A run that was going when the process stopped is Interrupted, never resumed
            // automatically: the restart may have been how the user stopped it.
            if (run.Status == GenreBackfillStatus.Running)
            {
                run.Status = GenreBackfillStatus.Interrupted;
                run.Reason = "Octo restarted while this run was in progress.";
                _logger?.LogInformation(
                    "genre backfill was interrupted at {Cursor}/{Total}; waiting for an explicit resume",
                    run.Cursor, run.Total);
            }
            _run = run;
        }
        catch (Exception ex)
        {
            // Losing progress is a cold start, not a failure to boot.
            _logger?.LogWarning("genre backfill state could not be read: {M}", ex.Message);
        }
    }

    private void Flush()
    {
        if (_path is null) return;
        if (Interlocked.Exchange(ref _dirty, 0) == 0) return;
        try
        {
            string json;
            lock (_lock) json = JsonSerializer.Serialize(_run);

            Directory.CreateDirectory(Path.GetDirectoryName(_path)!);
            var tmp = _path + ".tmp";
            File.WriteAllText(tmp, json);
            File.Move(tmp, _path, overwrite: true);
        }
        catch (Exception ex)
        {
            Interlocked.Exchange(ref _dirty, 1);
            _logger?.LogWarning("genre backfill state could not be written: {M}", ex.Message);
        }
    }

    public void Dispose()
    {
        _flushTimer?.Dispose();
        Flush();
    }
}
