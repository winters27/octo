using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.Library;
using Octo.Services.Soulseek;

namespace Octo.Services.Imports;

/// <summary>What the trickle is doing, in a word the dashboard and the apps know, and when it starts its next song.</summary>
public sealed record TrickleStatus(string State, DateTime? NextUtc, int PerHour);

/// <summary>The trickle's words for <see cref="TrickleStatus.State"/>. A contract with the dashboard and the apps.</summary>
public static class TrickleStates
{
    /// <summary>Nothing queued.</summary>
    public const string Idle = "idle";
    /// <summary>Working, or waiting for its next turn.</summary>
    public const string Running = "running";
    /// <summary>The asker paused theirs.</summary>
    public const string Paused = "paused";
    /// <summary>Songs per hour is 0.</summary>
    public const string Off = "off";
    /// <summary>Someone's own downloads come first.</summary>
    public const string Yielding = "yielding";
    /// <summary>Soulseek is out and nothing else can fetch.</summary>
    public const string WaitingForSoulseek = "waitingForSoulseek";
}

/// <summary>
/// Works through the missing songs a few an hour, through the same chain a heart takes: the
/// library check first (a song already there is done, never downloaded twice), then the heart
/// sources in their order, Soulseek's outage hold, Lidarr, parallel downloads and all, so an
/// import is fetched exactly the way a heart would be. It never queues ahead of a person: while
/// anyone's own download runs, it waits, and while Soulseek is out with nothing behind it, it
/// waits for Soulseek rather than taking a YouTube copy.
///
/// One song at a time, started no closer together than the songs-per-hour setting allows. A song
/// handed to Lidarr is followed until Lidarr's import lands, without holding the next one back.
/// </summary>
public sealed class TrickleWorker : BackgroundService
{
    internal static readonly TimeSpan TickEvery = TimeSpan.FromSeconds(5);
    /// <summary>A hand-off nobody reports on is given up after this long.</summary>
    internal static readonly TimeSpan GiveUpAfter = TimeSpan.FromHours(12);
    internal const int MaxAttempts = 3;

    private readonly TrickleQueue _queue;
    private readonly IOptionsMonitor<ImportSettings> _settings;
    private readonly ILogger<TrickleWorker> _logger;
    private readonly AcquisitionTracker? _tracker;
    private Task? _running;
    private sealed record Current(string Owner, string Key);
    // The song whose chain is running now, set before it starts and cleared once the chain returns.
    private volatile Current? _current;
    private volatile string _waitingFor = TrickleStates.Idle;

    public TrickleWorker(TrickleQueue queue, IOptionsMonitor<ImportSettings> settings, ILogger<TrickleWorker> logger,
        LibraryOwnership? owned = null, ExternalIdRegistry? ids = null, HeartAcquisitionCoordinator? hearts = null,
        TrackAcquisitionQueue? downloads = null, AcquisitionTracker? tracker = null,
        IOptionsMonitor<SubsonicSettings>? subsonic = null, ISoulseekLink? soulseek = null)
    {
        _queue = queue;
        _settings = settings;
        _logger = logger;
        _tracker = tracker;
        if (tracker is not null) tracker.Ended += OnEnded;

        Owned = async (job, ct) => owned is null ? null
            : (await owned.FindAsync(job.Artist, job.Title, job.Seconds, job.Album, ct)) is { } copy
                ? new OwnedSong(copy.NavidromeId) : null;
        Start = async job =>
        {
            if (ids is null || hearts is null) throw new InvalidOperationException("Downloads are not set up on this server.");
            var externalId = ids.Register(new SoulseekRouting
            {
                Kind = RoutingKind.Song, Artist = job.Artist, Title = job.Title, Album = job.Album,
                Duration = job.Seconds, Isrc = job.Isrc,
            });
            var key = AcquisitionTracker.KeyOf(SoulseekMetadataService.ProviderName, externalId);
            _queue.Update(job.Owner, job.Key, j => j.AcquisitionKey = key);
            // The tracker follows only rows that were opened, so the download gets one, as a heart's does.
            tracker?.Begin(SoulseekMetadataService.ProviderName, externalId, null, job.Owner, job.Artist, job.Title, job.Album);
            await hearts.AcquireTrackAsync(SoulseekMetadataService.ProviderName, externalId, job.Owner);
            return key;
        };
        Row = key => tracker?.All().FirstOrDefault(row => AcquisitionTracker.KeyOf(row.Provider, row.ExternalId) == key);
        PeopleDownloading = () => downloads is { IsIdle: false };
        SoulseekOut = async ct =>
        {
            if (subsonic is null || soulseek is null) return false;
            var steps = subsonic.CurrentValue.EffectiveHeartDownloadSources().Where(step => step.SongEnabled == true)
                .Select(step => step.Source).ToList();
            // With Lidarr in the chain an outage still leaves a source; without it the heart chain would
            // hold or fall back to YouTube, and a trickle that is in no hurry waits instead.
            if (!steps.Contains(HeartDownloadSource.Soulseek) || steps.Contains(HeartDownloadSource.Lidarr)) return false;
            return (await soulseek.ReadAsync(fresh: false, ct))?.Link == SoulseekLinkState.NotLoggedIn;
        };
    }

    public sealed record OwnedSong(string? LibraryId);

    // Seams, so the pace and the outcomes can be tested without downloading anything.
    internal Func<TrickleJob, CancellationToken, Task<OwnedSong?>> Owned { get; set; }
    internal Func<TrickleJob, Task<string>> Start { get; set; }
    internal Func<string, AcquisitionSnapshot?> Row { get; set; }
    internal Func<bool> PeopleDownloading { get; set; }
    internal Func<CancellationToken, Task<bool>> SoulseekOut { get; set; }
    internal Func<DateTime> Clock { get; set; } = () => DateTime.UtcNow;

    /// <summary>A song finished: fetched (with its library id when Navidrome showed it) or not.</summary>
    public event Action<TrickleJob>? Finished;

    /// <summary>What the trickle is doing for this person.</summary>
    public TrickleStatus StatusFor(string owner)
    {
        var perHour = _settings.CurrentValue.EffectiveSongsPerHour;
        var jobs = _queue.Snapshot(owner);
        var open = jobs.Any(job => ImportTrackStates.Open(job.State));
        if (!open) return new(TrickleStates.Idle, null, perHour);
        if (_queue.IsPaused(owner)) return new(TrickleStates.Paused, null, perHour);
        if (perHour == 0) return new(TrickleStates.Off, null, perHour);
        if (jobs.Any(job => job.State == ImportTrackStates.Queued))
        {
            var waiting = _waitingFor;
            if (waiting is TrickleStates.Yielding or TrickleStates.WaitingForSoulseek) return new(waiting, null, perHour);
        }
        return new(TrickleStates.Running, NextStart(perHour), perHour);
    }

    private DateTime? NextStart(int perHour)
    {
        if (perHour <= 0) return null;
        var last = _queue.LastStartUtc;
        var next = last is null ? Clock() : last.Value + TimeSpan.FromSeconds(3600.0 / perHour);
        return next < Clock() ? Clock() : next;
    }

    /// <summary>One pass: settle hand-offs, then start the next song when its turn has come. True when one started.</summary>
    internal async Task<bool> TickAsync(CancellationToken ct)
    {
        SettleHandOffs();
        if (_running is { IsCompleted: false }) return false;
        var perHour = _settings.CurrentValue.EffectiveSongsPerHour;
        if (perHour == 0) return false;
        if (NextStart(perHour) is { } next && next > Clock()) return false;
        if (PeopleDownloading()) { _waitingFor = TrickleStates.Yielding; return false; }
        if (await SoulseekOut(ct)) { _waitingFor = TrickleStates.WaitingForSoulseek; return false; }
        _waitingFor = TrickleStates.Running;
        if (_queue.TakeNext() is not { } job) { _waitingFor = TrickleStates.Idle; return false; }
        _current = new Current(job.Owner, job.Key);
        _running = Task.Run(() => RunAsync(job, ct), CancellationToken.None);
        return true;
    }

    /// <summary>Waits for the song started last. Only tests need it.</summary>
    internal Task DrainAsync() => _running ?? Task.CompletedTask;

    private async Task RunAsync(TrickleJob job, CancellationToken ct)
    {
        try
        {
            if (await Owned(job, ct) is { } owned)
            {
                Finish(job, ImportTrackStates.Done, "Already in your library", owned.LibraryId);
                return;
            }
            var key = await Start(job);
            // The chain is over: a song in the library, a failure, or a hand-off to Lidarr still
            // running. The tracker's row says which; a hand-off is settled when it ends.
            _current = null;
            Settle(job.Owner, job.Key, Row(key));
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _logger.LogWarning("Trickle for '{Artist} - {Title}' ({Owner}) failed: {Message}", job.Artist, job.Title, job.Owner, ex.Message);
            Finish(job, ImportTrackStates.NotFound, AcquisitionTracker.UserSafe(ex.Message) ?? "Could not get it.", null);
        }
        finally { _current = null; }
    }

    private bool IsCurrent(TrickleJob job) =>
        _current is { } current && current.Key == job.Key && string.Equals(current.Owner, job.Owner, StringComparison.OrdinalIgnoreCase);

    private void OnEnded(AcquisitionEnd end)
    {
        try
        {
            // The song whose chain is still running is settled when the chain returns: a source
            // in the middle of the chain failing is not the end of it.
            if (_queue.ByAcquisition(end.Key) is not { } job || IsCurrent(job)) return;
            Settle(job.Owner, job.Key, Row(end.Key));
        }
        catch (Exception ex) { _logger.LogDebug("Trickle could not read a download's end: {Message}", ex.Message); }
    }

    /// <summary>Songs whose chain returned with a download still running elsewhere (Lidarr), and ones nothing reports on.</summary>
    private void SettleHandOffs()
    {
        foreach (var job in _queue.Snapshot().Where(job => job.State == ImportTrackStates.Downloading && !IsCurrent(job)))
            Settle(job.Owner, job.Key, job.AcquisitionKey is { } key ? Row(key) : null);
    }

    private void Settle(string owner, string trackKey, AcquisitionSnapshot? row)
    {
        if (_queue.Get(owner, trackKey) is not { State: ImportTrackStates.Downloading } job) return;
        var age = job.StartedUtc is { } started ? Clock() - started : TimeSpan.Zero;
        switch (row?.State)
        {
            case AcquisitionState.Done:
                Finish(job, ImportTrackStates.Done, row.Source is { } source ? $"Fetched from {source}" : "Fetched", row.LibraryId);
                return;
            case AcquisitionState.Failed:
                Finish(job, ImportTrackStates.NotFound, row.Error ?? "No download source could get this song.", null);
                return;
            case null:
                // No row: it aged out of the tracker, or never opened. The library check at the
                // start of its next turn says whether it arrived; three times without one is enough.
                if (job.Attempts >= MaxAttempts)
                    Finish(job, ImportTrackStates.NotFound, "Octo lost track of this download three times.", null);
                else
                    _queue.Update(owner, trackKey, j => { j.State = ImportTrackStates.Queued; j.Detail = "Checking again"; });
                return;
            default:
                if (age > GiveUpAfter) Finish(job, ImportTrackStates.NotFound, "Still not in the library after 12 hours.", null);
                return;
        }
    }

    private void Finish(TrickleJob job, string state, string? detail, string? libraryId)
    {
        _queue.Update(job.Owner, job.Key, j => { j.State = state; j.Detail = detail; j.LibraryId = libraryId; });
        _logger.LogInformation("Trickle: '{Artist} - {Title}' for {Owner}: {State} ({Detail})",
            job.Artist, job.Title, job.Owner, state, detail);
        if (_queue.Get(job.Owner, job.Key) is { } finished)
            try { Finished?.Invoke(finished); }
            catch (Exception ex) { _logger.LogWarning("A trickle listener failed: {Message}", ex.Message); }
    }

    protected override async Task ExecuteAsync(CancellationToken stoppingToken)
    {
        while (!stoppingToken.IsCancellationRequested)
        {
            // Per-tick catch: BackgroundServiceExceptionBehavior defaults to StopHost.
            try { await TickAsync(stoppingToken); }
            catch (OperationCanceledException) when (stoppingToken.IsCancellationRequested) { break; }
            catch (Exception ex) { _logger.LogError(ex, "Trickle tick failed"); }
            try { await Task.Delay(TickEvery, stoppingToken); } catch (OperationCanceledException) { break; }
        }
    }
}
