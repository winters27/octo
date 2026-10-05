using System.Globalization;
using System.Text.Json;
using System.Threading.Channels;
using Octo.Services.Common;
using Octo.Services.Local;
using Octo.Services.Subsonic;
using Octo.Services.Tagging;

namespace Octo.Services.Library;

public enum ExplicitBackfillStatus { Idle, Running, Completed, Cancelled, Interrupted, Failed }

/// <summary>What a run does: look songs up and write nothing, write what a finished preview
/// found, or take out what the last write put in.</summary>
public enum ExplicitBackfillMode { Preview, Apply, Undo }

public sealed record ExplicitBackfillRequest(ExplicitBackfillMode Mode, string Username = "dashboard");

/// <summary>One song to look up: its file and Navidrome's id for it.</summary>
public sealed record ExplicitBackfillSong(string Path, string? NavidromeId);

/// <summary>One song the preview looked up, and what became of it.</summary>
public sealed class ExplicitBackfillRow
{
    public string Path { get; set; } = "";
    public string? NavidromeId { get; set; }
    public string? Artist { get; set; }
    public string? Title { get; set; }

    /// <summary>"explicit", "clean", "notExplicit" or "unsure".</summary>
    public string Outcome { get; set; } = "";
    public string How { get; set; } = "";
    public string? CatalogTrackId { get; set; }

    /// <summary>The file as the preview read it, so Apply leaves alone a file changed since.</summary>
    public long Size { get; set; }
    public long WriteTicks { get; set; }

    public bool Written { get; set; }
}

public sealed class ExplicitBackfillRun
{
    public string RunId { get; set; } = "";
    public ExplicitBackfillStatus Status { get; set; } = ExplicitBackfillStatus.Idle;
    public ExplicitBackfillMode Mode { get; set; }
    public DateTime? StartedUtc { get; set; }
    public DateTime? FinishedUtc { get; set; }
    public int Total { get; set; }
    public int Processed { get; set; }
    public int Explicit { get; set; }
    public int Clean { get; set; }
    public int NotExplicit { get; set; }
    public int Unsure { get; set; }

    /// <summary>Songs that already carry an advisory, which a preview never looks up.</summary>
    public int AlreadyMarked { get; set; }
    public int Failed { get; set; }

    /// <summary>Apply and Undo: how many files the step has to go through, and has.</summary>
    public int StepTotal { get; set; }
    public int StepDone { get; set; }

    /// <summary>Apply: files written. Undo: files put back.</summary>
    public int Written { get; set; }

    /// <summary>Apply and Undo: files changed since, so left as they are.</summary>
    public int LeftAlone { get; set; }

    public string? LastSong { get; set; }
    public string? Reason { get; set; }
    public List<string> Errors { get; set; } = [];

    /// <summary>Every song the preview looked up. Apply writes from these and nothing else.</summary>
    public List<ExplicitBackfillRow> Rows { get; set; } = [];

    /// <summary>What a preview still has to look up, for a resume.</summary>
    public List<ExplicitBackfillSong> Queue { get; set; } = [];
    public int Cursor { get; set; }

    /// <summary>The run whose writes Undo takes out: the last Apply.</summary>
    public string? AppliedRunId { get; set; }

    public bool CanResume => Mode == ExplicitBackfillMode.Preview
        && Status is ExplicitBackfillStatus.Cancelled or ExplicitBackfillStatus.Interrupted
        && Cursor < Queue.Count;

    /// <summary>A finished preview with something worth writing.</summary>
    public bool CanApply => Mode == ExplicitBackfillMode.Preview && Status == ExplicitBackfillStatus.Completed
        && Rows.Any(row => !row.Written && ExplicitBackfill.Writes(row.Outcome));
}

/// <summary>The run's state, beside the settings, so a preview survives a restart.</summary>
public sealed class ExplicitBackfillStore : IDisposable
{
    private static readonly TimeSpan FlushInterval = TimeSpan.FromSeconds(10);
    private const int MaxErrors = 20;
    private const int MaxRows = 50_000;

    private readonly string? _path;
    private readonly ILogger<ExplicitBackfillStore>? _logger;
    private readonly Timer? _flushTimer;
    private readonly object _lock = new();
    private int _dirty;
    private ExplicitBackfillRun _run = new();

    public ExplicitBackfillStore(string? path = null, ILogger<ExplicitBackfillStore>? logger = null)
    {
        _path = string.IsNullOrWhiteSpace(path) ? null : path;
        _logger = logger;
        if (_path is null) return;
        Load();
        _flushTimer = new Timer(_ => Flush(), null, FlushInterval, FlushInterval);
    }

    public ExplicitBackfillRun Current
    {
        get { lock (_lock) return _run; }
    }

    public void Update(Action<ExplicitBackfillRun> mutate)
    {
        lock (_lock)
        {
            mutate(_run);
            if (_run.Errors.Count > MaxErrors) _run.Errors.RemoveRange(0, _run.Errors.Count - MaxErrors);
            if (_run.Rows.Count > MaxRows) _run.Rows.RemoveRange(MaxRows, _run.Rows.Count - MaxRows);
        }
        Interlocked.Exchange(ref _dirty, 1);
    }

    public void Replace(ExplicitBackfillRun run)
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
            var run = JsonSerializer.Deserialize<ExplicitBackfillRun>(File.ReadAllText(_path!));
            if (run is null) return;
            // Never resumed by itself: the restart may have been how it was stopped.
            if (run.Status == ExplicitBackfillStatus.Running)
            {
                run.Status = ExplicitBackfillStatus.Interrupted;
                run.Reason = "Octo restarted while this run was going.";
            }
            _run = run;
        }
        catch (Exception ex)
        {
            _logger?.LogWarning("explicit marking state could not be read: {M}", ex.Message);
        }
    }

    public void Flush()
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
            _logger?.LogWarning("explicit marking state could not be written: {M}", ex.Message);
        }
    }

    public void Dispose()
    {
        _flushTimer?.Dispose();
        Flush();
    }
}

/// <summary>
/// Marks the songs already in the library explicit or clean, the way new downloads are marked
/// (ITUNESADVISORY on MP3 and FLAC, the rtng atom on M4A), so the apps can show it. Navidrome
/// reads the advisory into each song's explicitStatus at its next scan.
///
/// Three steps, like the genre re-tag. A preview looks up every song Navidrome lists with no
/// explicit status and whose file carries no advisory (<see cref="ExplicitLookup"/>, at a polite
/// pace on the catalog's background lane), and writes nothing. Apply writes only what that
/// preview found explicit or clean, in place (Navidrome keeps each song's id, plays and playlist
/// places), skipping a file changed since; "not explicit" is counted but not written, since a
/// missing mark already means that and every write sends the whole file back to a network
/// mount. Each write goes in the tag-edit journal, so Undo (or an app's own undo of that
/// song) takes it out again. Then one normal Navidrome scan.
/// </summary>
public sealed class ExplicitBackfill : BackgroundService
{
    /// <summary>Between two songs' lookups: each is one to three catalog requests, and a search
    /// someone is waiting on must never queue behind this.</summary>
    internal static TimeSpan Pace = TimeSpan.FromMilliseconds(700);

    /// <summary>How long to wait when the catalog does not answer, and how often to try.</summary>
    internal static TimeSpan BusyWait = TimeSpan.FromSeconds(30);
    private const int BusyTries = 3;

    /// <summary>Journal and state are saved every this many writes.</summary>
    private const int SaveEvery = 25;

    private readonly Channel<ExplicitBackfillRequest> _queue =
        Channel.CreateBounded<ExplicitBackfillRequest>(new BoundedChannelOptions(1) { FullMode = BoundedChannelFullMode.DropWrite });

    private readonly ExplicitBackfillStore _store;
    private readonly TagEditJournal _journal;
    private readonly IExplicitCatalog _catalog;
    private readonly IServiceProvider _services;
    private readonly IConfiguration _configuration;
    private readonly ILogger<ExplicitBackfill> _logger;
    private volatile bool _cancelRequested;
    private int _pending;

    /// <summary>The songs to look at, when not Navidrome's list (tests).</summary>
    internal Func<CancellationToken, Task<IReadOnlyList<(ExplicitBackfillSong Song, string? Status)>>>? ListSongs { get; set; }

    public ExplicitBackfill(ExplicitBackfillStore store, TagEditJournal journal, IExplicitCatalog catalog,
        IServiceProvider services, IConfiguration configuration, ILogger<ExplicitBackfill> logger)
    {
        _store = store;
        _journal = journal;
        _catalog = catalog;
        _services = services;
        _configuration = configuration;
        _logger = logger;
    }

    public ExplicitBackfillRun Current => _store.Current;
    public bool IsBusy => _store.Current.Status == ExplicitBackfillStatus.Running || Volatile.Read(ref _pending) != 0;

    /// <summary>Whether the last Apply left anything for Undo to take out.</summary>
    public bool CanUndo => _store.Current.AppliedRunId is { } run && _journal.OfRun(run).Count > 0;

    /// <summary>The outcomes Apply writes.</summary>
    public static bool Writes(string outcome) => outcome is "explicit" or "clean";

    /// <summary>False when a run is going or waiting to start.</summary>
    public bool TryEnqueue(ExplicitBackfillRequest request)
    {
        if (_store.Current.Status == ExplicitBackfillStatus.Running || Interlocked.CompareExchange(ref _pending, 1, 0) != 0) return false;
        if (_queue.Writer.TryWrite(request)) return true;
        Interlocked.Exchange(ref _pending, 0);
        return false;
    }

    public void RequestCancel() => _cancelRequested = true;

    /// <summary>The music folder the run walks when Navidrome cannot list it.</summary>
    public string MusicPath()
    {
        var fallback = _configuration["Library:DownloadPath"] ?? "./downloads";
        return _services.GetService<NavidromeIdentityService>()?.EffectiveDownloadPath(fallback) ?? fallback;
    }

    protected override async Task ExecuteAsync(CancellationToken stoppingToken)
    {
        await foreach (var request in _queue.Reader.ReadAllAsync(stoppingToken))
        {
            try
            {
                _cancelRequested = false;
                switch (request.Mode)
                {
                    case ExplicitBackfillMode.Preview: await PreviewAsync(stoppingToken); break;
                    case ExplicitBackfillMode.Apply: await ApplyAsync(request.Username, stoppingToken); break;
                    case ExplicitBackfillMode.Undo: await UndoAsync(stoppingToken); break;
                }
            }
            catch (OperationCanceledException) when (stoppingToken.IsCancellationRequested)
            {
                _store.Update(run => run.Status = ExplicitBackfillStatus.Interrupted);
                break;
            }
            catch (Exception ex)
            {
                _logger.LogError(ex, "Explicit marking failed");
                _store.Update(run =>
                {
                    run.Status = ExplicitBackfillStatus.Failed;
                    run.Reason = ex.Message;
                    run.FinishedUtc = DateTime.UtcNow;
                });
            }
            finally
            {
                _journal.Flush();
                _store.Flush();
                Interlocked.Exchange(ref _pending, 0);
            }
        }
    }

    /// <summary>Runs one request to the end, for tests.</summary>
    internal async Task RunAsync(ExplicitBackfillRequest request, CancellationToken ct = default)
    {
        Interlocked.Exchange(ref _pending, 1);
        try
        {
            _cancelRequested = false;
            switch (request.Mode)
            {
                case ExplicitBackfillMode.Preview: await PreviewAsync(ct); break;
                case ExplicitBackfillMode.Apply: await ApplyAsync(request.Username, ct); break;
                case ExplicitBackfillMode.Undo: await UndoAsync(ct); break;
            }
        }
        finally
        {
            _journal.Flush();
            Interlocked.Exchange(ref _pending, 0);
        }
    }

    // ---- Preview ---------------------------------------------------------------------------

    private async Task PreviewAsync(CancellationToken ct)
    {
        var current = _store.Current;
        if (current.CanResume)
        {
            _store.Update(run => { run.Status = ExplicitBackfillStatus.Running; run.Reason = null; });
            _logger.LogInformation("Explicit marking preview resuming at {Cursor}/{Total}", current.Cursor, current.Queue.Count);
        }
        else
        {
            var listed = await SongsAsync(ct);
            var queue = listed.Where(song => string.IsNullOrEmpty(song.Status)).Select(song => song.Song).ToList();
            _store.Replace(new ExplicitBackfillRun
            {
                RunId = Guid.NewGuid().ToString("N")[..12],
                Status = ExplicitBackfillStatus.Running,
                Mode = ExplicitBackfillMode.Preview,
                StartedUtc = DateTime.UtcNow,
                Total = queue.Count,
                AlreadyMarked = listed.Count - queue.Count,
                Queue = queue,
                AppliedRunId = current.AppliedRunId,
            });
            _logger.LogInformation("Explicit marking preview started: {Count} song(s) to look up, {Marked} already marked",
                queue.Count, listed.Count - queue.Count);
        }

        var songs = _store.Current.Queue;
        for (var index = _store.Current.Cursor; index < songs.Count; index++)
        {
            if (_cancelRequested)
            {
                _store.Update(run =>
                {
                    run.Status = ExplicitBackfillStatus.Cancelled;
                    run.FinishedUtc = DateTime.UtcNow;
                    run.Reason = "Stopped from the dashboard.";
                });
                return;
            }
            var song = songs[index];
            var row = await LookUpAsync(song, ct);
            _store.Update(run =>
            {
                run.Cursor = index + 1;
                run.Processed++;
                run.LastSong = row?.Title is { } title ? $"{title} by {row.Artist}" : song.Path;
                if (row is null) { run.AlreadyMarked++; return; }
                switch (row.Outcome)
                {
                    case "explicit": run.Explicit++; break;
                    case "clean": run.Clean++; break;
                    case "notExplicit": run.NotExplicit++; break;
                    case "failed":
                        run.Failed++;
                        run.Errors.Add($"{song.Path}: {row.How}");
                        return;
                    default: run.Unsure++; break;
                }
                run.Rows.Add(row);
            });
            if (Pace > TimeSpan.Zero && index + 1 < songs.Count) await Task.Delay(Pace, ct);
        }

        _store.Update(run =>
        {
            run.Status = ExplicitBackfillStatus.Completed;
            run.FinishedUtc = DateTime.UtcNow;
            run.Queue = [];
        });
        var done = _store.Current;
        _logger.LogInformation(
            "Explicit marking preview finished: {Explicit} explicit, {Clean} clean, {Not} not explicit, {Unsure} unsure, {Marked} already marked, {Failed} unreadable",
            done.Explicit, done.Clean, done.NotExplicit, done.Unsure, done.AlreadyMarked, done.Failed);
    }

    /// <summary>One song's row, or null when its file already carries an advisory.</summary>
    private async Task<ExplicitBackfillRow?> LookUpAsync(ExplicitBackfillSong song, CancellationToken ct)
    {
        LibrarySongFacts facts;
        var row = new ExplicitBackfillRow { Path = song.Path, NavidromeId = song.NavidromeId };
        try
        {
            var info = new FileInfo(song.Path);
            using var file = TagLib.File.Create(song.Path);
            if (TagWriterExtras.ReadAdvisory(file) is not null) return null;
            var isrc = TagWriterExtras.ReadText(file, TagFields.Isrc) ?? file.Tag.ISRC;
            facts = new LibrarySongFacts(
                file.Tag.JoinedPerformers is { Length: > 0 } artist ? artist : null,
                file.Tag.Title, file.Tag.Album,
                file.Properties?.Duration.TotalSeconds is > 0 and var seconds ? seconds : null,
                string.IsNullOrWhiteSpace(isrc) ? [] : isrc.Split([';', '/', ','], StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries),
                Path.GetFileNameWithoutExtension(song.Path));
            (row.Artist, row.Title, row.Size, row.WriteTicks) = (facts.Artist, facts.Title, info.Length, info.LastWriteTimeUtc.Ticks);
        }
        catch (Exception ex)
        {
            row.Outcome = "failed";
            row.How = ex is TagLib.UnsupportedFormatException or TagLib.CorruptFileException
                ? "the file could not be read as music" : ex.Message;
            return row;
        }

        ExplicitFinding finding = ExplicitFinding.Busy;
        for (var attempt = 0; attempt < BusyTries; attempt++)
        {
            finding = await ExplicitLookup.FindAsync(facts, _catalog, ct);
            if (!finding.CatalogBusy) break;
            if (attempt + 1 < BusyTries) await Task.Delay(BusyWait, ct);
        }
        row.Outcome = finding.Outcome switch
        {
            ExplicitOutcome.Explicit => "explicit",
            ExplicitOutcome.Clean => "clean",
            ExplicitOutcome.NotExplicit => "notExplicit",
            _ => "unsure",
        };
        row.How = finding.How;
        row.CatalogTrackId = finding.CatalogTrackId;
        return row;
    }

    private async Task<IReadOnlyList<(ExplicitBackfillSong Song, string? Status)>> SongsAsync(CancellationToken ct)
    {
        if (ListSongs is not null) return await ListSongs(ct);
        var root = MusicPath();
        using var scope = _services.CreateScope();
        var listed = await NavidromeSongList.ListAsync(scope.ServiceProvider, root, _logger, ct);
        if (listed.Count > 0)
            return listed.Values
                .GroupBy(entry => entry.Id ?? entry.FullPath, StringComparer.Ordinal)
                .Select(group => group.FirstOrDefault(entry => File.Exists(entry.FullPath)))
                .OfType<NavidromeSongEntry>()
                .OrderBy(entry => entry.FullPath, StringComparer.Ordinal)
                .Select(entry => (new ExplicitBackfillSong(entry.FullPath, entry.Id), entry.ExplicitStatus))
                .ToList();

        // Navidrome could not be asked: every music file under the folder, each opened to see.
        _logger.LogInformation("Explicit marking could not list Navidrome's songs; walking {Root} instead", root);
        if (!Directory.Exists(root)) return [];
        string[] kinds = [".flac", ".mp3", ".m4a", ".ogg", ".opus"];
        return Directory.EnumerateFiles(root, "*", SearchOption.AllDirectories)
            .Where(path => kinds.Contains(Path.GetExtension(path), StringComparer.OrdinalIgnoreCase))
            .Where(path => !path.Contains($"{Path.DirectorySeparatorChar}.octo-", StringComparison.Ordinal))
            .OrderBy(path => path, StringComparer.Ordinal)
            .Select(path => (new ExplicitBackfillSong(path, null), (string?)null))
            .ToList();
    }

    // ---- Apply -----------------------------------------------------------------------------

    private async Task ApplyAsync(string username, CancellationToken ct)
    {
        var preview = _store.Current;
        if (!preview.CanApply)
        {
            _store.Update(run => run.Reason = "Nothing to write: preview first.");
            return;
        }
        var applyId = Guid.NewGuid().ToString("N")[..12];
        var rows = preview.Rows.Where(row => !row.Written && Writes(row.Outcome)).ToList();
        _store.Update(run =>
        {
            run.Status = ExplicitBackfillStatus.Running;
            run.Mode = ExplicitBackfillMode.Apply;
            run.StartedUtc = DateTime.UtcNow;
            run.FinishedUtc = null;
            run.StepTotal = rows.Count;
            run.StepDone = run.Written = run.LeftAlone = 0;
            run.Reason = null;
            run.AppliedRunId = applyId;
        });
        _logger.LogInformation("Explicit marking: writing {Count} file(s)", rows.Count);

        var since = 0;
        foreach (var row in rows)
        {
            if (_cancelRequested || ct.IsCancellationRequested) break;
            var outcome = WriteOne(row, username, applyId);
            _store.Update(run =>
            {
                run.StepDone++;
                run.LastSong = $"{row.Title} by {row.Artist}";
                switch (outcome)
                {
                    case null: run.Written++; row.Written = true; break;
                    case "": run.LeftAlone++; break;
                    default: run.Failed++; run.Errors.Add($"{row.Path}: {outcome}"); break;
                }
            });
            if (++since >= SaveEvery)
            {
                since = 0;
                _journal.Flush();
                _store.Flush();
                await Task.Yield();
            }
        }
        _journal.Flush();

        var stopped = _cancelRequested || ct.IsCancellationRequested;
        _store.Update(run =>
        {
            // Back to a preview, so what is left can still be written and the rows stay.
            run.Mode = ExplicitBackfillMode.Preview;
            run.Status = ExplicitBackfillStatus.Completed;
            run.FinishedUtc = DateTime.UtcNow;
            run.Reason = (stopped ? "Stopped. " : "") + $"Marked {run.Written} song(s)"
                + (run.LeftAlone > 0 ? $"; {run.LeftAlone} changed since the preview and were left alone" : "")
                + (run.Failed > 0 ? $"; {run.Failed} could not be written" : "") + ".";
        });
        if (_store.Current.Written > 0) await RescanAsync();
    }

    /// <summary>Null when written; "" when the file changed since the preview (left alone);
    /// otherwise why it could not be written.</summary>
    private string? WriteOne(ExplicitBackfillRow row, string username, string runId)
    {
        var info = new FileInfo(row.Path);
        if (!info.Exists || info.Length != row.Size || info.LastWriteTimeUtc.Ticks != row.WriteTicks) return "";
        var advisory = row.Outcome == "clean" ? ExplicitAdvisory.Clean : ExplicitAdvisory.Explicit;
        var result = LibraryTagEdits.SetAdvisory(row.Path, advisory);
        if (result.Error is { } error) return error;
        if (!result.Changed) return "";
        var after = new FileInfo(row.Path);
        _journal.Record(new TagEditEntry(Guid.NewGuid().ToString("N"), row.NavidromeId ?? "", row.Path, TagEditKinds.Advisory,
            username, new Dictionary<string, string?>(result.Before), new Dictionary<string, string?>(result.After),
            after.Length, after.LastWriteTimeUtc.Ticks, DateTime.UtcNow) { RunId = runId }, save: false);
        return null;
    }

    // ---- Undo ------------------------------------------------------------------------------

    private async Task UndoAsync(CancellationToken ct)
    {
        if (_store.Current.AppliedRunId is not { } applied) return;
        var entries = _journal.OfRun(applied);
        _store.Update(run =>
        {
            run.Status = ExplicitBackfillStatus.Running;
            run.Mode = ExplicitBackfillMode.Undo;
            run.StartedUtc = DateTime.UtcNow;
            run.FinishedUtc = null;
            run.StepTotal = entries.Count;
            run.StepDone = run.Written = run.LeftAlone = 0;
            run.Reason = null;
        });

        var since = 0;
        foreach (var entry in entries)
        {
            if (_cancelRequested || ct.IsCancellationRequested) break;
            var info = new FileInfo(entry.Path);
            string? problem = null;
            var leftAlone = !info.Exists || info.Length != entry.SizeAfter || info.LastWriteTimeUtc.Ticks != entry.WriteTicksAfter;
            if (!leftAlone)
            {
                var before = int.TryParse(entry.Before.GetValueOrDefault(TagEditKinds.Advisory), NumberStyles.Integer,
                    CultureInfo.InvariantCulture, out var was) ? was : (int?)null;
                var result = LibraryTagEdits.SetAdvisory(entry.Path, before);
                problem = result.Error;
                if (problem is null) _journal.MarkUndone(entry.Id, save: false);
            }
            _store.Update(run =>
            {
                run.StepDone++;
                run.LastSong = entry.Path;
                if (leftAlone) run.LeftAlone++;
                else if (problem is null) run.Written++;
                else { run.Failed++; run.Errors.Add($"{entry.Path}: {problem}"); }
                var row = run.Rows.FirstOrDefault(r => r.Path == entry.Path);
                if (row is not null && !leftAlone && problem is null)
                {
                    row.Written = false;
                    var now = new FileInfo(entry.Path);
                    (row.Size, row.WriteTicks) = (now.Length, now.LastWriteTimeUtc.Ticks);
                }
            });
            if (++since >= SaveEvery)
            {
                since = 0;
                _journal.Flush();
                await Task.Yield();
            }
        }
        _journal.Flush();

        _store.Update(run =>
        {
            run.Mode = ExplicitBackfillMode.Preview;
            run.Status = ExplicitBackfillStatus.Completed;
            run.FinishedUtc = DateTime.UtcNow;
            run.Reason = $"Took the mark off {run.Written} song(s)"
                + (run.LeftAlone > 0 ? $"; {run.LeftAlone} changed since or missing, so left as they are (they stay undoable)" : "") + ".";
        });
        if (_store.Current.Written > 0) await RescanAsync();
    }

    /// <summary>One normal scan (not a full one), forced past the debounce so it is not lost.</summary>
    private async Task RescanAsync()
    {
        try
        {
            using var scope = _services.CreateScope();
            if (scope.ServiceProvider.GetService<ILocalLibraryService>() is { } library) await library.TriggerLibraryScanAsync(force: true);
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Explicit marking could not ask Navidrome to scan: {M}", ex.Message);
        }
    }
}
