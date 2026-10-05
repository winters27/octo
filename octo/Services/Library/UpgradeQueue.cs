using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.Soulseek;

namespace Octo.Services.Library;

/// <summary>The words a job's state goes by on the wire. A contract with the Octo apps.</summary>
public static class UpgradeStates
{
    public const string Queued = "queued";
    public const string Waiting = "waiting";
    public const string Working = "working";
    public const string Upgraded = "upgraded";
    public const string NotFound = "notFound";
    public const string Rehearsed = "rehearsed";
    public const string Skipped = "skipped";
    public const string Failed = "failed";

    /// <summary>Still to run or running: not an answer yet.</summary>
    public static bool Open(string state) => state is Queued or Waiting or Working;
}

/// <summary>One song someone asked to find in higher quality.</summary>
public sealed class UpgradeJob
{
    public string NavidromeId { get; set; } = "";
    public string? Title { get; set; }
    public string? Artist { get; set; }
    public string? Album { get; set; }
    public string? Suffix { get; set; }

    /// <summary>The weekly upgrade's key for this file (path and size), when the page knew it, so a
    /// song with no FLAC anywhere is not tried again by the weekly run for four weeks.</summary>
    public string? AttemptKey { get; set; }

    /// <summary>Who it acts as: the person who asked in an app, or the admin signed in on the page.</summary>
    public string RequestedBy { get; set; } = "";
    public string Origin { get; set; } = "app";
    public string State { get; set; } = UpgradeStates.Queued;
    public string? Detail { get; set; }

    /// <summary>provider:externalId of the replacement download, once it is queued.</summary>
    public string? AcquisitionKey { get; set; }
    public DateTime QueuedUtc { get; set; }
    public DateTime UpdatedUtc { get; set; }

    /// <summary>When it started running, for how long it took.</summary>
    public DateTime? StartedUtc { get; set; }

    /// <summary>What a finished job found and did, for a person to read.</summary>
    public UpgradeResult? Result { get; set; }

    /// <summary>The copy someone picked in Find songs, fetched instead of searching; null to search.</summary>
    public PickedCopy? Pick { get; set; }

    public UpgradeJob Copy() => (UpgradeJob)MemberwiseClone();
}

/// <summary>
/// What an upgrade did, in words: the file before and after, the checks the new one passed,
/// where the original is kept, and how long it took. Proof that "Upgraded" really means it.
/// </summary>
public sealed class UpgradeResult
{
    public string? Before { get; set; }
    public long? BeforeBytes { get; set; }
    public string? After { get; set; }
    public long? AfterBytes { get; set; }
    public string? NewFile { get; set; }
    public string? KeptAt { get; set; }
    public List<string> Checks { get; set; } = [];
    public double? Seconds { get; set; }
}

/// <summary>A song to look for, with what the asker already knows about it.</summary>
public sealed record UpgradeAsk(string NavidromeId, string? Title = null, string? Artist = null, string? Album = null,
    string? Suffix = null, string? AttemptKey = null, PickedCopy? Pick = null);

/// <summary>
/// The songs waiting to be found in higher quality, from the apps and the Better quality page, on disk so a
/// restart keeps them. One job per song: asking again for a song already queued or running changes
/// nothing. A job left running by a restart is queued again, and the action journal reconciles any
/// replacement it had started. Finished jobs stay a week so the apps can read how they went.
/// </summary>
public sealed class UpgradeQueue
{
    /// <summary>Jobs still to run at once, across everyone. Past it a request is refused with a reason.</summary>
    internal const int MaxOpenJobs = 5000;
    internal static readonly TimeSpan KeepFinished = TimeSpan.FromDays(7);

    private readonly string? _path;
    private readonly ILogger<UpgradeQueue>? _logger;
    private readonly object _lock = new();
    private readonly Dictionary<string, UpgradeJob> _jobs = new(StringComparer.Ordinal);

    internal Func<DateTime> Clock { get; set; } = () => DateTime.UtcNow;

    public UpgradeQueue(string? path = null, ILogger<UpgradeQueue>? logger = null)
    {
        _path = string.IsNullOrWhiteSpace(path) ? null : path;
        _logger = logger;
        try
        {
            if (_path is not null && File.Exists(_path))
                foreach (var job in JsonSerializer.Deserialize<List<UpgradeJob>>(File.ReadAllText(_path)) ?? [])
                {
                    if (job.State == UpgradeStates.Working) job.State = UpgradeStates.Queued;
                    _jobs[job.NavidromeId] = job;
                }
        }
        catch (Exception ex) { _logger?.LogWarning("the upgrade queue could not be read: {M}", ex.Message); }
    }

    /// <summary>
    /// Queues one job per song not already queued or running. Answers each song's job as it now
    /// stands, and a reason for the songs left out because the queue is full.
    /// </summary>
    public (IReadOnlyList<UpgradeJob> Jobs, string? Refused) Add(IEnumerable<UpgradeAsk> asks, string requester, string origin)
    {
        lock (_lock)
        {
            var now = Clock();
            var open = _jobs.Values.Count(job => UpgradeStates.Open(job.State));
            var answer = new List<UpgradeJob>();
            var refused = 0;
            foreach (var ask in asks.DistinctBy(a => a.NavidromeId))
            {
                if (string.IsNullOrWhiteSpace(ask.NavidromeId)) continue;
                if (_jobs.TryGetValue(ask.NavidromeId, out var existing) && UpgradeStates.Open(existing.State))
                {
                    answer.Add(existing.Copy());
                    continue;
                }
                if (open >= MaxOpenJobs) { refused++; continue; }
                var job = new UpgradeJob
                {
                    NavidromeId = ask.NavidromeId, Title = ask.Title, Artist = ask.Artist, Album = ask.Album,
                    Suffix = ask.Suffix, AttemptKey = ask.AttemptKey, RequestedBy = requester, Origin = origin,
                    State = UpgradeStates.Queued, QueuedUtc = now, UpdatedUtc = now, Pick = ask.Pick,
                };
                _jobs[job.NavidromeId] = job;
                open++;
                answer.Add(job.Copy());
            }
            Save();
            return (answer, refused == 0 ? null
                : $"{refused} {(refused == 1 ? "song was" : "songs were")} left out: {MaxOpenJobs} are already waiting.");
        }
    }

    /// <summary>
    /// Puts another picked copy on a job that has not started, for the person who asked for it.
    /// Answers the job as it now stands, or null when it has started or is someone else's: the
    /// copy it fetches is settled then.
    /// </summary>
    public UpgradeJob? ReplacePick(string navidromeId, string requester, PickedCopy pick)
    {
        lock (_lock)
        {
            if (!_jobs.TryGetValue(navidromeId, out var job) || job.State is not (UpgradeStates.Queued or UpgradeStates.Waiting)
                || !string.Equals(job.RequestedBy, requester.Trim(), StringComparison.OrdinalIgnoreCase))
                return null;
            job.Pick = pick;
            job.UpdatedUtc = Clock();
            Save();
            return job.Copy();
        }
    }

    /// <summary>Every job, or only one person's, as copies.</summary>
    public IReadOnlyList<UpgradeJob> Snapshot(string? requester = null)
    {
        lock (_lock)
        {
            Prune();
            return _jobs.Values
                .Where(job => requester is null || string.Equals(job.RequestedBy, requester, StringComparison.OrdinalIgnoreCase))
                .OrderBy(job => job.QueuedUtc)
                .Select(job => job.Copy())
                .ToList();
        }
    }

    public int OpenCount
    {
        get { lock (_lock) return _jobs.Values.Count(job => UpgradeStates.Open(job.State)); }
    }

    /// <summary>Takes back jobs that have not started. A running job runs to its end.</summary>
    public int Cancel(IEnumerable<string> ids)
    {
        lock (_lock)
        {
            var removed = ids.Count(id => _jobs.TryGetValue(id, out var job)
                && job.State is UpgradeStates.Queued or UpgradeStates.Waiting && _jobs.Remove(id));
            if (removed > 0) Save();
            return removed;
        }
    }

    /// <summary>Forgets every finished job.</summary>
    public int ClearFinished()
    {
        lock (_lock)
        {
            var finished = _jobs.Values.Where(job => !UpgradeStates.Open(job.State)).Select(job => job.NavidromeId).ToList();
            foreach (var id in finished) _jobs.Remove(id);
            if (finished.Count > 0) Save();
            return finished.Count;
        }
    }

    /// <summary>The oldest queued job, now working, or null.</summary>
    internal UpgradeJob? TakeNext()
    {
        lock (_lock)
        {
            var next = _jobs.Values.Where(job => job.State == UpgradeStates.Queued).OrderBy(job => job.QueuedUtc).FirstOrDefault();
            if (next is null) return null;
            next.State = UpgradeStates.Working;
            next.Detail = null;
            next.StartedUtc = Clock();
            next.Result = null;
            next.UpdatedUtc = Clock();
            Save();
            return next.Copy();
        }
    }

    /// <summary>Waiting jobs back in the queue, once Soulseek is back.</summary>
    internal int Requeue()
    {
        lock (_lock)
        {
            var waiting = _jobs.Values.Where(job => job.State == UpgradeStates.Waiting).ToList();
            foreach (var job in waiting) { job.State = UpgradeStates.Queued; job.UpdatedUtc = Clock(); }
            if (waiting.Count > 0) Save();
            return waiting.Count;
        }
    }

    internal bool AnyWaiting
    {
        get { lock (_lock) return _jobs.Values.Any(job => job.State == UpgradeStates.Waiting); }
    }

    internal void Update(string navidromeId, Action<UpgradeJob> change)
    {
        lock (_lock)
        {
            if (!_jobs.TryGetValue(navidromeId, out var job)) return;
            change(job);
            job.UpdatedUtc = Clock();
            Save();
        }
    }

    // Called with the lock held.
    private void Prune()
    {
        var cutoff = Clock() - KeepFinished;
        foreach (var stale in _jobs.Values.Where(job => !UpgradeStates.Open(job.State) && job.UpdatedUtc < cutoff)
                     .Select(job => job.NavidromeId).ToList())
            _jobs.Remove(stale);
    }

    // Called with the lock held.
    private void Save()
    {
        if (_path is null) return;
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(_path)!);
            File.WriteAllText(_path + ".tmp", JsonSerializer.Serialize(_jobs.Values.ToList()));
            File.Move(_path + ".tmp", _path, overwrite: true);
        }
        catch (Exception ex) { _logger?.LogWarning("the upgrade queue could not be written: {M}", ex.Message); }
    }
}

/// <summary>
/// Runs the upgrade queue: Better quality on each song, up to as many at once as downloads may run,
/// with every safety the action has (the allowlist, dry run, the quarantine, the original back when
/// the new file is not really lossless, and the same Navidrome id after). While Soulseek is out a job
/// waits rather than failing, and goes again once slskd is logged in.
/// </summary>
public sealed class UpgradeWorker : BackgroundService
{
    internal static readonly TimeSpan TickEvery = TimeSpan.FromSeconds(2);

    private readonly UpgradeQueue _queue;
    private readonly ILogger<UpgradeWorker> _logger;
    private readonly AcquisitionTracker? _tracker;
    private readonly QualityUpgradeStore? _attempts;
    private readonly IOptionsMonitor<SoulseekSettings>? _soulseekSettings;
    private readonly DownloadPicks? _picks;
    private readonly List<Task> _running = [];

    public UpgradeWorker(UpgradeQueue queue, LibraryActionExecutor executor, NavidromeSongPathResolver resolver,
        ILogger<UpgradeWorker> logger, DownloadConcurrency? concurrency = null, ISoulseekLink? soulseek = null,
        AcquisitionTracker? tracker = null, QualityUpgradeStore? attempts = null,
        IOptionsMonitor<SoulseekSettings>? soulseekSettings = null, UpgradeSources? sources = null,
        DownloadPicks? picks = null)
    {
        _picks = picks;
        _soulseekSettings = soulseekSettings;
        SourceName = () => sources?.Name ?? "Soulseek";
        _queue = queue;
        _logger = logger;
        _tracker = tracker;
        _attempts = attempts;
        Apply = (request, ct) => executor.ApplyAsync(request, ct);
        Describe = async (id, ct) => await resolver.ResolveAsync(id, ct);
        Width = () => Math.Max(1, concurrency?.Current ?? 1);
        // Waiting only when every source is out: with Lidarr set up, a Soulseek outage leaves Lidarr.
        SoulseekOffline = async ct => sources is not null ? await sources.WaitingForSoulseekAsync(ct)
            : soulseek is not null && (await soulseek.ReadAsync(fresh: false, ct))?.Link == SoulseekLinkState.NotLoggedIn;
    }

    // Seams, the same way the weekly upgrade exposes them.
    internal Func<LibraryActionRequest, CancellationToken, Task<LibraryActionOutcome>> Apply { get; set; }
    internal Func<string, CancellationToken, Task<ResolvedSongFile?>> Describe { get; set; }
    internal Func<int> Width { get; set; }
    internal Func<CancellationToken, Task<bool>> SoulseekOffline { get; set; }
    internal Func<string> SourceName { get; set; }

    /// <summary>Jobs this worker is running now.</summary>
    internal int Running
    {
        get { lock (_running) return _running.Count(task => !task.IsCompleted); }
    }

    /// <summary>One pass: waiting jobs back in the queue when Soulseek is back, then queued jobs
    /// started while there is room. Answers how many it started.</summary>
    internal async Task<int> TickAsync(CancellationToken ct)
    {
        if (_queue.AnyWaiting && !await SoulseekOffline(ct)) _queue.Requeue();
        var started = 0;
        lock (_running) _running.RemoveAll(task => task.IsCompleted);
        while (Running < Width() && _queue.TakeNext() is { } job)
        {
            var run = Task.Run(() => RunAsync(job, ct), CancellationToken.None);
            lock (_running) _running.Add(run);
            started++;
        }
        return started;
    }

    /// <summary>Waits for every job started so far. Only tests need it.</summary>
    internal Task DrainAsync()
    {
        Task[] running;
        lock (_running) running = _running.ToArray();
        return Task.WhenAll(running);
    }

    private async Task RunAsync(UpgradeJob job, CancellationToken ct)
    {
        try
        {
            if (job.Title is null && await Describe(job.NavidromeId, ct) is { } song)
            {
                _queue.Update(job.NavidromeId, j =>
                {
                    j.Title = song.Title; j.Artist = song.Artist; j.Album = song.Album; j.Suffix = song.Suffix;
                });
                (job.Title, job.Artist, job.Album) = (song.Title, song.Artist, song.Album);
            }

            string? acquired = null;
            LibraryActionOutcome outcome;
            try
            {
                outcome = await Apply(new LibraryActionRequest(LibraryAction.BetterQuality, job.NavidromeId, job.RequestedBy,
                    OnReplacementQueued: (provider, externalId) =>
                    {
                        acquired = externalId;
                        _queue.Update(job.NavidromeId, j => j.AcquisitionKey = $"{provider}:{externalId}");
                        // A copy picked in Find songs is fetched as it is, with no search.
                        if (job.Pick is { } pick) _picks?.Pin(externalId, pick);
                        // The tracker follows only rows that were opened, so the replacement gets one. It
                        // carries the library song's id, so the apps draw that song's cover on it.
                        _tracker?.Begin(provider, externalId, job.NavidromeId, job.RequestedBy, job.Artist, job.Title, job.Album,
                            AcquisitionKinds.Upgrade);
                        _tracker?.Log(provider, externalId, AcquisitionEventKinds.Note,
                            job.Pick is { } chosen ? $"Getting {chosen.Describe()}" : $"Your copy is {job.Suffix?.ToUpperInvariant() ?? "lossy"}; looking for a lossless one",
                            "Your copy stays until the new one passes every check");
                    },
                    OnlySource: job.Pick is { } only ? (only.IsSoulseek ? DownloadSource.Soulseek : DownloadSource.Lidarr) : null), ct);
            }
            finally
            {
                // A pick the replacement never used (it joined another download, or found the
                // song needed nothing) is let go with the job, not left for a later download.
                if (acquired is not null && job.Pick is { } unused) _picks?.Forget(acquired, unused);
            }

            var state = StateFor(outcome);
            var result = state == UpgradeStates.Upgraded ? Report(outcome, job) : null;
            _queue.Update(job.NavidromeId, j =>
            {
                j.State = state;
                j.Result = result;
                j.Detail = state switch
                {
                    UpgradeStates.Waiting => "Waiting for Soulseek",
                    UpgradeStates.Upgraded when result?.After is { } after =>
                        $"Now {after}{Size(result.AfterBytes)}, was {result.Before ?? job.Suffix?.ToUpperInvariant()}{Size(result.BeforeBytes)}.",
                    UpgradeStates.NotFound => $"No lossless copy of this song on {SourceName()} right now. Your copy is unchanged.",
                    _ => outcome.Detail,
                };
            });
            // The verdict comes after the download's own row has ended, so it is a line of its own.
            if (acquired is not null)
                _tracker?.Log(SoulseekMetadataService.ProviderName, acquired,
                    state == UpgradeStates.Upgraded ? AcquisitionEventKinds.Done : AcquisitionEventKinds.Note,
                    state switch
                    {
                        UpgradeStates.Upgraded => "Took the place of your old copy",
                        UpgradeStates.NotFound => "No lossless copy found; your copy is unchanged",
                        UpgradeStates.Waiting => "Waiting for Soulseek",
                        UpgradeStates.Rehearsed => "Only rehearsed; nothing changed",
                        _ => "Your copy is unchanged",
                    },
                    state == UpgradeStates.Upgraded && result?.After is { } now
                        ? $"Now {now}{Size(result.AfterBytes)}, was {result.Before ?? job.Suffix?.ToUpperInvariant()}{Size(result.BeforeBytes)}"
                        : AcquisitionTracker.UserSafe(outcome.Detail));
            if (state == UpgradeStates.NotFound && job.AttemptKey is { } key)
                _attempts?.Update(s => s.Attempts[key] = new QualityUpgradeAttempt(DateTime.UtcNow, outcome.State.ToString(), outcome.Detail));
            _logger.LogInformation("Higher quality for {Id} ('{Artist} - {Title}') by {User}: {State} - {Detail}",
                job.NavidromeId, job.Artist, job.Title, job.RequestedBy, state, outcome.Detail);
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _logger.LogWarning("Higher quality for {Id} failed: {Message}", job.NavidromeId, ex.Message);
            _queue.Update(job.NavidromeId, j => { j.State = UpgradeStates.Failed; j.Detail = ex.Message; });
        }
    }

    /// <summary>
    /// The proof for a replacement that went in: both files described from their own headers, the
    /// checks the new one passed to get there, and where the original waits. Never throws: a file
    /// that cannot be read is simply not described.
    /// </summary>
    internal UpgradeResult Report(LibraryActionOutcome outcome, UpgradeJob job)
    {
        var settings = _soulseekSettings?.CurrentValue ?? new SoulseekSettings();
        var result = new UpgradeResult
        {
            NewFile = outcome.NewPath is { } path ? Path.GetFileName(path) : null,
            KeptAt = outcome.QuarantinePath is { } kept ? KeptFolder(kept) : null,
            Seconds = job.StartedUtc is { } started ? Math.Round((DateTime.UtcNow - started).TotalSeconds) : null,
        };
        (result.After, result.AfterBytes) = AudioSummary.Describe(outcome.NewPath);
        (result.Before, result.BeforeBytes) = AudioSummary.Describe(outcome.QuarantinePath);
        // Every replacement is held to these before it may take the original's place.
        result.Checks.Add("the same length");
        if (settings.VerifyDownloads && !string.IsNullOrWhiteSpace(settings.AcoustIdApiKey))
            result.Checks.Add("AcoustID: the same recording");
        if (settings.DetectTranscodes) result.Checks.Add("the spectrum: really lossless, not a converted MP3");
        return result;
    }

    // ".octo-trash/2026-10-03", the part of the quarantine path a person can find again.
    private static string KeptFolder(string path)
    {
        var parts = path.Replace('\\', '/').Split('/', StringSplitOptions.RemoveEmptyEntries);
        var trash = Array.FindIndex(parts, p => p.StartsWith('.') && p.Contains("trash", StringComparison.OrdinalIgnoreCase));
        return trash >= 0 ? string.Join('/', parts[trash..^1]) : Path.GetDirectoryName(path) ?? path;
    }

    private static string Size(long? bytes) => bytes is > 0 ? $", {bytes.Value / 1048576.0:0.0} MB" : "";

    /// <summary>How an action's outcome reads as a job state. On the reason code, never the words.</summary>
    internal static string StateFor(LibraryActionOutcome outcome) => outcome switch
    {
        { State: LibraryActionState.Applied } => UpgradeStates.Upgraded,
        { State: LibraryActionState.Rehearsed } => UpgradeStates.Rehearsed,
        { Code: LibraryActionCodes.SoulseekOffline } => UpgradeStates.Waiting,
        { Code: LibraryActionCodes.NoReplacement } => UpgradeStates.NotFound,
        { State: LibraryActionState.Skipped } => UpgradeStates.Skipped,
        _ => UpgradeStates.Failed,
    };

    protected override async Task ExecuteAsync(CancellationToken stoppingToken)
    {
        while (!stoppingToken.IsCancellationRequested)
        {
            // Per-tick catch: BackgroundServiceExceptionBehavior defaults to StopHost.
            try { await TickAsync(stoppingToken); }
            catch (OperationCanceledException) when (stoppingToken.IsCancellationRequested) { break; }
            catch (Exception ex) { _logger.LogError(ex, "Upgrade queue tick failed"); }
            try { await Task.Delay(TickEvery, stoppingToken); } catch (OperationCanceledException) { break; }
        }
    }
}

/// <summary>A file described from its own header: "FLAC 16-bit 44.1 kHz" or "MP3 320 kbps".</summary>
public static class AudioSummary
{
    private static readonly HashSet<string> Lossless = new(StringComparer.OrdinalIgnoreCase)
        { "flac", "wav", "aiff", "aif", "alac", "ape", "wv" };

    public static (string? Text, long? Bytes) Describe(string? path)
    {
        if (string.IsNullOrEmpty(path) || !File.Exists(path)) return (null, null);
        var bytes = new FileInfo(path).Length;
        var format = Path.GetExtension(path).TrimStart('.').ToUpperInvariant();
        try
        {
            using var file = TagLib.File.Create(path);
            var p = file.Properties;
            if (Lossless.Contains(format) && p.BitsPerSample > 0 && p.AudioSampleRate > 0)
                return ($"{format} {p.BitsPerSample}-bit {p.AudioSampleRate / 1000.0:0.#} kHz", bytes);
            if (p.AudioBitrate > 0) return ($"{format} {p.AudioBitrate} kbps", bytes);
        }
        catch
        {
            // Unreadable: the format from the name is still worth saying.
        }
        return (format, bytes);
    }
}
