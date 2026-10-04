using System.Text.Json;

namespace Octo.Services.Imports;

/// <summary>
/// The missing songs the trickle is working through, one job per song per person however many
/// lists hold it, with who has paused theirs, on disk so a restart carries on where it was. A job
/// running when Octo stopped is queued again; the library check it starts with catches one that
/// arrived meanwhile.
///
/// A song that could not be found is tried again after two weeks, since what Soulseek has changes;
/// one someone skipped stays skipped until they ask for it again.
/// </summary>
public sealed class TrickleQueue
{
    internal static readonly TimeSpan RetryNotFoundAfter = TimeSpan.FromDays(14);
    internal static readonly TimeSpan KeepDone = TimeSpan.FromDays(7);

    private sealed class FileShape
    {
        public List<TrickleJob> Jobs { get; set; } = [];
        public List<string> Paused { get; set; } = [];
        public DateTime? LastStartUtc { get; set; }
    }

    private readonly string? _path;
    private readonly ILogger<TrickleQueue>? _logger;
    private readonly object _lock = new();
    private readonly Dictionary<(string Owner, string Key), TrickleJob> _jobs = new();
    private readonly HashSet<string> _paused = new(StringComparer.OrdinalIgnoreCase);
    private DateTime? _lastStart;

    internal Func<DateTime> Clock { get; set; } = () => DateTime.UtcNow;

    public TrickleQueue(string? path = null, ILogger<TrickleQueue>? logger = null)
    {
        _path = string.IsNullOrWhiteSpace(path) ? null : path;
        _logger = logger;
        try
        {
            if (_path is not null && File.Exists(_path) && JsonSerializer.Deserialize<FileShape>(File.ReadAllText(_path)) is { } file)
            {
                foreach (var job in file.Jobs)
                {
                    if (job.State == ImportTrackStates.Downloading)
                    {
                        job.State = ImportTrackStates.Queued;
                        job.Detail = "Octo restarted while this was running; it goes again.";
                    }
                    _jobs[KeyOf(job.Owner, job.Key)] = job;
                }
                _paused.UnionWith(file.Paused);
                _lastStart = file.LastStartUtc;
            }
        }
        catch (Exception ex) { _logger?.LogWarning("the trickle queue could not be read: {M}", ex.Message); }
    }

    private static (string, string) KeyOf(string owner, string key) => (owner.ToLowerInvariant(), key);

    /// <summary>
    /// Queues the songs not already queued, running, fetched or skipped. A song not found two weeks
    /// ago or more is queued again. Answers how many were queued.
    /// </summary>
    public int Add(string owner, IEnumerable<ImportTrack> tracks)
    {
        lock (_lock)
        {
            var now = Clock();
            var added = 0;
            foreach (var track in tracks.DistinctBy(track => track.Key))
            {
                if (string.IsNullOrWhiteSpace(track.Key)) continue;
                if (_jobs.TryGetValue(KeyOf(owner, track.Key), out var existing))
                {
                    var stale = existing.State == ImportTrackStates.NotFound && now - existing.UpdatedUtc >= RetryNotFoundAfter;
                    if (!stale) continue;
                }
                _jobs[KeyOf(owner, track.Key)] = new TrickleJob
                {
                    Owner = owner, Key = track.Key, Title = track.Title, Artist = track.Artist, Album = track.Album,
                    Seconds = track.Seconds, Isrc = track.Isrc, State = ImportTrackStates.Queued,
                    QueuedUtc = now, UpdatedUtc = now,
                };
                added++;
            }
            if (added > 0) Write();
            return added;
        }
    }

    /// <summary>Every job, or one person's, oldest first, as copies.</summary>
    public IReadOnlyList<TrickleJob> Snapshot(string? owner = null)
    {
        lock (_lock)
        {
            Prune();
            return _jobs.Values
                .Where(job => owner is null || string.Equals(job.Owner, owner, StringComparison.OrdinalIgnoreCase))
                .OrderBy(job => job.QueuedUtc)
                .Select(job => job.Copy())
                .ToList();
        }
    }

    public TrickleJob? Get(string owner, string key)
    {
        lock (_lock) return _jobs.TryGetValue(KeyOf(owner, key), out var job) ? job.Copy() : null;
    }

    public bool IsPaused(string owner)
    {
        lock (_lock) return _paused.Contains(owner);
    }

    public void SetPaused(string owner, bool paused)
    {
        lock (_lock)
        {
            if (paused ? _paused.Add(owner) : _paused.Remove(owner)) Write();
        }
    }

    /// <summary>When the trickle last started a song, for its pace.</summary>
    public DateTime? LastStartUtc
    {
        get { lock (_lock) return _lastStart; }
    }

    /// <summary>
    /// The next song, now running, or null. People take turns: the one whose last song started
    /// longest ago goes next, so one long list cannot keep everyone else waiting. Paused people are passed over.
    /// </summary>
    internal TrickleJob? TakeNext()
    {
        lock (_lock)
        {
            var queued = _jobs.Values.Where(job => job.State == ImportTrackStates.Queued && !_paused.Contains(job.Owner)).ToList();
            if (queued.Count == 0) return null;
            var lastByOwner = _jobs.Values.Where(job => job.StartedUtc is not null)
                .GroupBy(job => job.Owner, StringComparer.OrdinalIgnoreCase)
                .ToDictionary(group => group.Key, group => group.Max(job => job.StartedUtc!.Value), StringComparer.OrdinalIgnoreCase);
            var next = queued
                .OrderBy(job => lastByOwner.TryGetValue(job.Owner, out var last) ? last : DateTime.MinValue)
                .ThenBy(job => job.QueuedUtc)
                .First();
            var now = Clock();
            next.State = ImportTrackStates.Downloading;
            next.Detail = null;
            next.StartedUtc = now;
            next.UpdatedUtc = now;
            next.Attempts++;
            _lastStart = now;
            Write();
            return next.Copy();
        }
    }

    internal void Update(string owner, string key, Action<TrickleJob> change)
    {
        lock (_lock)
        {
            if (!_jobs.TryGetValue(KeyOf(owner, key), out var job)) return;
            change(job);
            job.UpdatedUtc = Clock();
            Write();
        }
    }

    /// <summary>The running job with this download, or null.</summary>
    internal TrickleJob? ByAcquisition(string acquisitionKey)
    {
        lock (_lock)
            return _jobs.Values.FirstOrDefault(job => job.State == ImportTrackStates.Downloading
                && string.Equals(job.AcquisitionKey, acquisitionKey, StringComparison.OrdinalIgnoreCase))?.Copy();
    }

    /// <summary>Songs not found or skipped, queued again. Answers how many.</summary>
    public int Retry(string owner, IEnumerable<string> keys) => Change(owner, keys,
        job => job.State is ImportTrackStates.NotFound or ImportTrackStates.Skipped,
        job => { job.State = ImportTrackStates.Queued; job.Detail = null; job.Attempts = 0; job.QueuedUtc = Clock(); });

    /// <summary>Queued songs set aside. A running one runs to its end. Answers how many.</summary>
    public int Skip(string owner, IEnumerable<string> keys) => Change(owner, keys,
        job => job.State == ImportTrackStates.Queued,
        job => { job.State = ImportTrackStates.Skipped; job.Detail = "Skipped"; });

    private int Change(string owner, IEnumerable<string> keys, Func<TrickleJob, bool> when, Action<TrickleJob> change)
    {
        lock (_lock)
        {
            var changed = 0;
            foreach (var key in keys.Distinct())
            {
                if (!_jobs.TryGetValue(KeyOf(owner, key), out var job) || !when(job)) continue;
                change(job);
                job.UpdatedUtc = Clock();
                changed++;
            }
            if (changed > 0) Write();
            return changed;
        }
    }

    /// <summary>Forgets one person's finished songs: fetched, not found and skipped. Answers how many.</summary>
    public int ClearFinished(string owner)
    {
        lock (_lock)
        {
            var finished = _jobs.Where(pair => string.Equals(pair.Value.Owner, owner, StringComparison.OrdinalIgnoreCase)
                    && !ImportTrackStates.Open(pair.Value.State))
                .Select(pair => pair.Key).ToList();
            foreach (var key in finished) _jobs.Remove(key);
            if (finished.Count > 0) Write();
            return finished.Count;
        }
    }

    /// <summary>Takes back one person's queued songs, leaving what already ran. Answers how many.</summary>
    public int CancelQueued(string owner, IEnumerable<string>? keys = null)
    {
        lock (_lock)
        {
            var only = keys?.ToHashSet(StringComparer.Ordinal);
            var queued = _jobs.Where(pair => string.Equals(pair.Value.Owner, owner, StringComparison.OrdinalIgnoreCase)
                    && pair.Value.State == ImportTrackStates.Queued && (only is null || only.Contains(pair.Value.Key)))
                .Select(pair => pair.Key).ToList();
            foreach (var key in queued) _jobs.Remove(key);
            if (queued.Count > 0) Write();
            return queued.Count;
        }
    }

    // Called with the lock held. Fetched songs go after a week; the list itself says they are there.
    private void Prune()
    {
        var cutoff = Clock() - KeepDone;
        var old = _jobs.Where(pair => pair.Value.State == ImportTrackStates.Done && pair.Value.UpdatedUtc < cutoff)
            .Select(pair => pair.Key).ToList();
        foreach (var key in old) _jobs.Remove(key);
    }

    // Called with the lock held.
    private void Write()
    {
        if (_path is null) return;
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(_path)!);
            var file = new FileShape { Jobs = _jobs.Values.ToList(), Paused = _paused.ToList(), LastStartUtc = _lastStart };
            File.WriteAllText(_path + ".tmp", JsonSerializer.Serialize(file));
            File.Move(_path + ".tmp", _path, overwrite: true);
        }
        catch (Exception ex) { _logger?.LogWarning("the trickle queue could not be written: {M}", ex.Message); }
    }
}
