using System.Collections.Concurrent;
using System.Text.Json;
using Octo.Models.Settings;

namespace Octo.Services.Library;

public enum LibraryActionState
{
    /// <summary>Written BEFORE the file is touched. A crash leaves this behind, and startup
    /// reconciles it against the filesystem rather than blindly re-running.</summary>
    Pending,
    Applied,
    Failed,

    /// <summary>The id could not be resolved to a file. Never a success, never a delete.</summary>
    Unresolved,
    Skipped,

    /// <summary>
    /// A rehearsal: everything ran except touching the file. Its own state rather than Failed,
    /// because nothing failed, and a log line saying otherwise about a working dry run is the
    /// kind of thing that makes someone turn rehearsal mode off to "fix" it.
    /// </summary>
    Rehearsed,
}

public sealed record LibraryActionEntry(
    string Key, LibraryAction Action, string NavidromeId, string Username,
    string Title, string Artist, string Album,
    string? SourcePath, string? QuarantinePath, PathSource? Resolution,
    LibraryActionState State, string? Detail, bool DryRun, DateTime AtUtc);

/// <summary>
/// Write-ahead ledger for library actions. Two jobs, and the order of operations is the point.
///
/// 1. Idempotency. The playlist and the file are two systems with no transaction across them.
///    Quarantine a file, fail to remove the track from the playlist, and the next sweep sees
///    the same track. Without a record of "already applied", a Wrong version would re-download
///    on every poll forever. So the entry is written Pending BEFORE the file is touched and
///    completed after; a crash in between leaves a Pending entry that startup RECONCILES.
///
/// 2. Audit. Every file this feature moved, who asked, when, from where to where, and whether
///    it was a dry run. That is what makes a delete reversible in practice rather than in
///    principle, and logs scroll away while "which files did this touch" has to be answerable
///    a week later.
/// </summary>
public sealed class LibraryActionJournal : IDisposable
{
    private const int MaxEntries = 2000;
    private static readonly TimeSpan FlushInterval = TimeSpan.FromSeconds(10);

    private readonly string? _path;
    private readonly ILogger<LibraryActionJournal>? _logger;
    private readonly Timer? _flushTimer;
    private readonly object _lock = new();
    private readonly object _reconcileLock = new();
    private readonly object _flushLock = new();
    private readonly ConcurrentDictionary<string, LibraryActionEntry> _byKey = new(StringComparer.Ordinal);
    private readonly List<string> _order = [];
    private int _dirty;

    public LibraryActionJournal(string? path = null, ILogger<LibraryActionJournal>? logger = null)
    {
        _path = string.IsNullOrWhiteSpace(path) ? null : path;
        _logger = logger;
        if (_path is null) return;

        Load();
        _flushTimer = new Timer(_ => Flush(), null, FlushInterval, FlushInterval);
    }

    /// <summary>
    /// Identity of an action.
    ///
    /// Fingerprinted on the file's size and modified time rather than the id alone, because
    /// after a Better quality upgrade the SAME Navidrome id points at a NEW file and the user
    /// is entitled to ask for a better copy of that one too.
    /// </summary>
    public static string MakeKey(LibraryAction action, string navidromeId, string fingerprint) =>
        $"{action}|{navidromeId}|{fingerprint}";

    public static string Fingerprint(long sizeBytes, DateTime lastWriteUtc) =>
        $"{sizeBytes}:{lastWriteUtc.Ticks}";

    /// <summary>
    /// True when this exact action on this exact file content already reached a terminal state.
    ///
    /// A DRY RUN never counts, and that exclusion is load-bearing rather than tidy: a rehearsal
    /// records an entry under the same key, so without this a rehearsal would permanently
    /// suppress the real action for that file. Observed in exactly that form on the first live
    /// run, where the real sweep answered Skipped and quietly consumed the request.
    /// </summary>
    public bool AlreadyApplied(LibraryAction action, string navidromeId, string fingerprint) =>
        _byKey.TryGetValue(MakeKey(action, navidromeId, fingerprint), out var entry)
        && !entry.DryRun
        && entry.State is LibraryActionState.Applied or LibraryActionState.Skipped;

    public void Record(LibraryActionEntry entry)
    {
        lock (_lock)
        {
            if (!_byKey.ContainsKey(entry.Key)) _order.Add(entry.Key);
            _byKey[entry.Key] = entry;
            Trim();
        }
        Interlocked.Exchange(ref _dirty, 1);
    }

    public void Complete(string key, LibraryActionState state, string? detail = null,
        string? quarantinePath = null)
    {
        if (!_byKey.TryGetValue(key, out var existing)) return;
        Record(existing with
        {
            State = state,
            Detail = detail ?? existing.Detail,
            QuarantinePath = quarantinePath ?? existing.QuarantinePath,
            AtUtc = DateTime.UtcNow,
        });
    }

    /// <summary>
    /// Has the user said they do not want this track back?
    ///
    /// A Delete means exactly that, so a later re-acquire of the same artist and title is
    /// refused. Read off the journal rather than a second store: it already records artist and
    /// title per entry and is bounded, so this is a scan over a couple of thousand rows in
    /// memory.
    /// </summary>
    public bool IsNeverRequested(string? artist, string? title)
    {
        if (string.IsNullOrWhiteSpace(artist) || string.IsNullOrWhiteSpace(title)) return false;

        return _byKey.Values.Any(entry =>
            entry.Action == LibraryAction.Delete
            && entry.State == LibraryActionState.Applied
            && !entry.DryRun
            && string.Equals(entry.Artist?.Trim(), artist.Trim(), StringComparison.OrdinalIgnoreCase)
            && string.Equals(entry.Title?.Trim(), title.Trim(), StringComparison.OrdinalIgnoreCase));
    }

    public IReadOnlyList<LibraryActionEntry> Pending() =>
        _byKey.Values.Where(entry => entry.State == LibraryActionState.Pending).ToList();

    public IReadOnlyList<LibraryActionEntry> Recent(int limit = 200)
    {
        lock (_lock)
        {
            return _order.AsEnumerable().Reverse().Take(limit)
                .Select(key => _byKey.TryGetValue(key, out var entry) ? entry : null)
                .Where(entry => entry is not null)
                .Select(entry => entry!)
                .ToList();
        }
    }

    /// <summary>
    /// Decide what a Pending entry actually means, by looking at the filesystem rather than
    /// guessing. A half-written action must never re-delete and never re-download.
    ///
    /// The executor records the quarantine path on the Pending entry as soon as the move lands,
    /// before a replacement is fetched, so a crash during that minutes-long fetch is recognisable
    /// here. <paramref name="restoreOriginal"/> puts a quarantined file back; a replacement that
    /// never finished should not leave the user without the track. Runs under a lock because the
    /// playlist worker and the executor can both reconcile at startup, and a second restore of
    /// the same entry would overwrite the first's result with a wrong one.
    /// </summary>
    public int Reconcile(Func<string, bool>? restoreOriginal = null)
    {
        lock (_reconcileLock)
        {
            var reconciled = 0;
            foreach (var entry in Pending())
            {
                var quarantine = entry.QuarantinePath is { Length: > 0 } recorded && File.Exists(recorded)
                    ? recorded : null;
                var sourcePresent = entry.SourcePath is { Length: > 0 } source && File.Exists(source);
                var (state, detail) = Resolve(entry, quarantine, sourcePresent, restoreOriginal);
                Complete(entry.Key, state, detail);
                reconciled++;
            }

            if (reconciled > 0)
            {
                Flush();
                _logger?.LogInformation("Library actions reconciled {Count} interrupted entr(ies)", reconciled);
            }
            return reconciled;
        }
    }

    private static (LibraryActionState State, string Detail) Resolve(LibraryActionEntry entry,
        string? quarantine, bool sourcePresent, Func<string, bool>? restoreOriginal)
    {
        if (quarantine is null)
        {
            return sourcePresent
                // The file is still where it was, so nothing happened. Failed rather than left
                // Pending, so it is not treated as in flight forever.
                ? (LibraryActionState.Failed, "Octo stopped before this action was applied; nothing was changed.")
                : (LibraryActionState.Failed,
                    $"Octo stopped partway through. The file is no longer at {entry.SourcePath}; check the quarantine folder.");
        }

        if (entry.Action == LibraryAction.Delete)
        {
            return sourcePresent
                ? (LibraryActionState.Failed,
                    $"Octo stopped partway. The file is back at its path and a copy is in quarantine at {quarantine}.")
                // The move landed and only the bookkeeping after it was lost: the removal is done.
                : (LibraryActionState.Applied, "Reconciled after a restart.");
        }

        // A replacement was being fetched when Octo stopped.
        if (sourcePresent)
        {
            // Something is at the original path again (a replacement may have landed). Putting
            // the original back would overwrite it, so leave both for the user to compare.
            return (LibraryActionState.Failed,
                "Octo stopped while replacing this track. A file is at the original path and the original "
                + $"is also in quarantine at {quarantine}; compare them before removing either.");
        }

        return restoreOriginal?.Invoke(quarantine) == true
            ? (LibraryActionState.Failed, "Octo stopped while replacing this track. The original was put back.")
            : (LibraryActionState.Failed,
                $"Octo stopped while replacing this track. The original is in quarantine at {quarantine}.");
    }

    private void Trim()
    {
        while (_order.Count > MaxEntries)
        {
            var oldest = _order[0];
            _order.RemoveAt(0);
            _byKey.TryRemove(oldest, out _);
        }
    }

    private void Load()
    {
        try
        {
            if (!File.Exists(_path)) return;
            var entries = JsonSerializer.Deserialize<List<LibraryActionEntry>>(File.ReadAllText(_path!));
            if (entries is null) return;

            lock (_lock)
            {
                foreach (var entry in entries)
                {
                    if (string.IsNullOrEmpty(entry.Key)) continue;
                    if (!_byKey.ContainsKey(entry.Key)) _order.Add(entry.Key);
                    _byKey[entry.Key] = entry;
                }
                Trim();
            }
            if (_byKey.Count > 0)
                _logger?.LogInformation("library action journal restored {Count} entries", _byKey.Count);
        }
        catch (Exception ex)
        {
            _logger?.LogWarning("library action journal could not be read: {M}", ex.Message);
        }
    }

    /// <summary>
    /// Write the journal to disk now rather than on the next timer tick. The executor calls this
    /// around moving a file, because an entry that only exists in memory when Octo stops cannot
    /// be reconciled. Serialised, since the timer can flush at the same moment.
    /// </summary>
    /// <returns>False only when there was something to write and it could not be written.</returns>
    public bool Flush()
    {
        if (_path is null) return true;
        lock (_flushLock)
            return FlushLocked(_path);
    }

    private bool FlushLocked(string path)
    {
        if (Interlocked.Exchange(ref _dirty, 0) == 0) return true;
        try
        {
            List<LibraryActionEntry> entries;
            lock (_lock)
                entries = _order.Select(key => _byKey.TryGetValue(key, out var entry) ? entry : null)
                    .Where(entry => entry is not null).Select(entry => entry!).ToList();

            Directory.CreateDirectory(Path.GetDirectoryName(path)!);
            var tmp = path + ".tmp";
            File.WriteAllText(tmp, JsonSerializer.Serialize(entries));
            File.Move(tmp, path, overwrite: true);
            return true;
        }
        catch (Exception ex)
        {
            Interlocked.Exchange(ref _dirty, 1);
            _logger?.LogWarning("library action journal could not be written: {M}", ex.Message);
            return false;
        }
    }

    public void Dispose()
    {
        _flushTimer?.Dispose();
        Flush();
    }
}
