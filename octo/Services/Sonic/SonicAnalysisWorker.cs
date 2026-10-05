using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.Library;
using Octo.Services.Subsonic;

namespace Octo.Services.Sonic;

/// <summary>What the analysis reads of the library: Navidrome's song list, a song that just
/// arrived, and the local file of a song. An interface so the worker can be tested without one.</summary>
public interface ISonicLibrary
{
    Task<(IReadOnlyList<LibrarySongRow> Rows, int Count)?> ListAsync(int start, int count, CancellationToken ct);
    Task<LibrarySongRow?> ArrivedAsync(string id, CancellationToken ct);
    FileInfo? LocalFile(LibrarySongRow row);
}

/// <summary>The library as Navidrome lists it, with each file checked where Octo can read it.</summary>
public sealed class NavidromeSonicLibrary(NavidromePlaylistApi api, NavidromeSongPathResolver resolver) : ISonicLibrary
{
    public Task<(IReadOnlyList<LibrarySongRow> Rows, int Count)?> ListAsync(int start, int count, CancellationToken ct) =>
        api.ListSongsAsync(start, count, ct);

    public async Task<LibrarySongRow?> ArrivedAsync(string id, CancellationToken ct) =>
        await resolver.ResolveAsync(id, ct) is { } file
            ? new LibrarySongRow(id, file.AbsolutePath, null, file.SizeBytes, file.Suffix, 0, file.Title, file.Artist,
                file.DurationSeconds, file.Album)
            : null;

    public FileInfo? LocalFile(LibrarySongRow row) => resolver.LocalFile(row);
}

/// <summary>Where the analysis is, for the dashboard. Skipped: songs this pass leaves out, as
/// Octo cannot find their file, or they are too long or have no length.</summary>
public sealed record SonicStatus(string State, string? Reason, int Analysed, int Total, int Failed,
    int Pass, DateTime? NextPassUtc, bool Paused, int Skipped = 0);

/// <summary>
/// Sounds alike (multi-source radio): asks octo-sonic about every library song once, one at a
/// time with a pause between, only while nothing downloads, then keeps up with new and changed
/// songs on a pass every <see cref="PassInterval"/>. A song that arrived through a tracked
/// download goes first.
/// </summary>
public sealed class SonicAnalysisWorker : BackgroundService
{
    internal static readonly TimeSpan IdleCheck = TimeSpan.FromMinutes(1);
    internal static readonly TimeSpan BusyCheck = TimeSpan.FromSeconds(30);
    internal static readonly TimeSpan DownCheck = TimeSpan.FromMinutes(10);
    internal static readonly TimeSpan PassInterval = TimeSpan.FromHours(6);
    internal static readonly TimeSpan RetryFailedAfter = TimeSpan.FromDays(7);
    internal const int PageSize = 1000;

    private readonly SonicStore _store;
    private readonly SonicClient _sonic;
    private readonly ISonicLibrary _library;
    private readonly IAcquisitionActivity _activity;
    private readonly IOptionsMonitor<RadioSourceSettings> _settings;
    private readonly ILogger<SonicAnalysisWorker> _logger;
    private readonly TimeProvider _time;
    private readonly System.Collections.Concurrent.ConcurrentQueue<string> _first = new();
    /// <summary>Times octo-sonic dropped the connection on a song, by id. Twice means the file
    /// crashes it; it is then marked unreadable instead of being tried forever.</summary>
    private readonly Dictionary<string, int> _dropped = new(StringComparer.Ordinal);
    internal const int DropsBeforeFailed = 2;
    /// <summary>Longer than radio ever plays (RadioFiller), so not worth reading over the network.</summary>
    internal const int LongestSeconds = 45 * 60;
    private List<LibrarySongRow>? _rows;
    /// <summary>Whether this pass's list is the whole library: only then are songs no longer in
    /// it forgotten, so one failed page never costs the songs after it.</summary>
    private bool _complete;
    /// <summary>Songs this pass leaves out, so each tick does not look for their files again.</summary>
    private readonly HashSet<string> _skipped = new(StringComparer.Ordinal);
    private int _generation;
    // Until the first tick, which waits for start-up to finish: not "Off", which would mislead.
    private volatile string _state = "Waiting";
    private volatile string? _reason = "Starts a couple of minutes after Octo does.";

    public SonicAnalysisWorker(SonicStore store, SonicClient sonic, ISonicLibrary library,
        IAcquisitionActivity activity, IOptionsMonitor<RadioSourceSettings> settings,
        ILogger<SonicAnalysisWorker> logger, AcquisitionTracker? tracker = null, TimeProvider? time = null)
    {
        _store = store; _sonic = sonic; _library = library; _activity = activity;
        _settings = settings; _logger = logger; _time = time ?? TimeProvider.System;
        if (tracker is not null)
            tracker.Ended += end => { if (end.Done && end.LibraryId is { Length: > 0 } id) ArrivedFirst(id); };
    }

    /// <summary>A song that just arrived in the library is analysed before the rest of the pass.</summary>
    internal void ArrivedFirst(string libraryId) => _first.Enqueue(libraryId);

    protected override async Task ExecuteAsync(CancellationToken stoppingToken)
    {
        // Never alongside startup, which already works the disk and Navidrome.
        try { await Task.Delay(TimeSpan.FromMinutes(2), _time, stoppingToken); } catch (OperationCanceledException) { return; }
        while (!stoppingToken.IsCancellationRequested)
        {
            TimeSpan wait;
            // Per-tick catch is mandatory: an unhandled exception would stop the host.
            try { wait = await TickAsync(stoppingToken); }
            catch (OperationCanceledException) when (stoppingToken.IsCancellationRequested) { break; }
            catch (Exception ex) { _logger.LogError(ex, "Sounds alike analysis failed"); wait = IdleCheck; }
            try { await Task.Delay(wait, _time, stoppingToken); } catch (OperationCanceledException) { break; }
        }
        _store.Flush();
    }

    public override async Task StopAsync(CancellationToken cancellationToken)
    {
        await base.StopAsync(cancellationToken);
        _store.Flush();
    }

    internal async Task<TimeSpan> TickAsync(CancellationToken ct)
    {
        var settings = _settings.CurrentValue;
        if (!settings.SoundsAlike) return Hold("Off", "Sounds alike is off on the Radio page.", IdleCheck);
        if (_store.Read(s => s.Paused)) return Hold("Paused", "Paused from the dashboard.", IdleCheck);
        var health = await _sonic.HealthAsync(ct);
        if (health is null) return Hold("Waiting", "octo-sonic is not answering.", DownCheck);
        if (health.Problem is not null) return Hold("Waiting", CannotSeeMusic, DownCheck);
        if (_activity.IsBusy) return Hold("Waiting", "Waiting for a download to finish.", BusyCheck);

        var now = _time.GetUtcNow().UtcDateTime;
        // Between passes only the songs that just arrived are read, without listing the library
        // again or counting a pass.
        var arrivalsOnly = _rows is null && _store.Read(s => s.NextPassUtc > now);
        if (arrivalsOnly && _first.IsEmpty)
            return Hold("Done", "Every song has been analysed. New or changed ones are looked for later.", IdleCheck);

        var generation = Volatile.Read(ref _generation);
        if (_rows is null && !arrivalsOnly)
        {
            var listed = await ListAsync(ct);
            if (listed is null) return Hold("Waiting", "Navidrome's song list could not be read (Octo needs its own Navidrome sign-in); trying again.", IdleCheck);
            if (generation != Volatile.Read(ref _generation)) return TimeSpan.Zero;
            (_rows, _complete) = listed.Value;
            _skipped.Clear();
        }

        var target = await NextTargetAsync(health.Version, now, arrivalsOnly ? [] : _rows!, ct);
        if (target is null)
        {
            if (arrivalsOnly)
                return Hold("Done", "Every song has been analysed. New or changed ones are looked for later.", IdleCheck);
            if (generation != Volatile.Read(ref _generation)) return TimeSpan.Zero;
            var present = _rows!.Select(row => row.Id).ToHashSet(StringComparer.Ordinal);
            var complete = _complete;
            _store.Write(s =>
            {
                if (complete)
                {
                    foreach (var gone in s.Songs.Keys.Where(id => !present.Contains(id)).ToList()) s.Songs.Remove(gone);
                    foreach (var gone in s.Failed.Keys.Where(id => !present.Contains(id)).ToList()) s.Failed.Remove(gone);
                }
                s.Pass++; s.PassFinishedUtc = now; s.NextPassUtc = now + PassInterval;
            });
            _store.Flush();
            _logger.LogInformation("Sounds alike finished a pass over {Count} song(s), {Skipped} left out", _rows.Count, _skipped.Count);
            _rows = null;
            return Hold("Done", "Every song has been analysed. New or changed ones are looked for later.", IdleCheck);
        }

        var (row, file, stamp) = target.Value;
        SonicAnswer answer;
        try { answer = await _sonic.AnalyseAsync(file, ct); }
        catch (Exception ex) when (ex is HttpRequestException or TaskCanceledException && !ct.IsCancellationRequested)
        {
            // The connection dropped mid-file: octo-sonic restarted, or the file crashed its
            // reader. Once is chance; twice on the same file, it is that file.
            var drops = _dropped[row.Id] = _dropped.GetValueOrDefault(row.Id) + 1;
            if (drops < DropsBeforeFailed) return Hold("Waiting", "octo-sonic stopped answering; trying again.", DownCheck);
            _dropped.Remove(row.Id);
            answer = new SonicAnswer(null, "octo-sonic stopped while reading it", 0);
        }
        // An answer came, so octo-sonic is up: a drop counts only twice in a row.
        if (answer.Status != 0) _dropped.Clear();
        if (generation != Volatile.Read(ref _generation)) return TimeSpan.Zero;
        // Octo sees the file and octo-sonic does not: its music mount is not Octo's. That is no
        // fault of the song, so nothing is recorded and the song is asked again once it is fixed.
        if (answer.Status is 403 or 404) return Hold("Waiting", CannotSeeMusic, DownCheck);
        var (features, error) = (answer.Features, answer.Error);
        _store.Write(s =>
        {
            if (features is not null)
            {
                s.Songs[row.Id] = new SonicSong
                {
                    Stamp = stamp, Title = row.Title, Artist = row.Artist, Album = row.Album,
                    Duration = row.Duration, Version = features.Version, F = features.Values,
                };
                s.Failed.Remove(row.Id);
            }
            else
            {
                s.Failed[row.Id] = new SonicFailure { Stamp = stamp, Error = error ?? "unknown", AtUtc = now };
                _logger.LogInformation("octo-sonic could not read '{Artist} - {Title}': {Error}", row.Artist, row.Title, error);
            }
        });
        return Hold("Running", null, TimeSpan.FromSeconds(settings.EffectiveSonicPauseSeconds));
    }

    internal const string CannotSeeMusic =
        "octo-sonic cannot see the music folder. Check that it mounts the same folder as Octo (DOWNLOAD_PATH) and can read it (SONIC_USER).";

    /// <summary>A song first in line, else the next one with no features, a changed file, older
    /// features, or a failure old enough to try again.</summary>
    private async Task<(LibrarySongRow Row, string File, string Stamp)?> NextTargetAsync(
        int version, DateTime now, List<LibrarySongRow> rows, CancellationToken ct)
    {
        var byId = rows.ToDictionary(row => row.Id, StringComparer.Ordinal);
        while (_first.TryDequeue(out var id))
        {
            // A song that just arrived is not in this pass's list yet: ask Navidrome for it, and
            // keep it in the list so the pass's end does not forget it again.
            var row = byId.GetValueOrDefault(id);
            if (row is null && await _library.ArrivedAsync(id, ct) is { } arrived)
            {
                row = arrived;
                if (_rows is not null && ReferenceEquals(rows, _rows)) { _rows.Add(arrived); byId[id] = arrived; }
            }
            if (row is not null && Candidate(row, version, now) is { } found) return found;
        }
        foreach (var row in rows)
            if (!_skipped.Contains(row.Id) && Candidate(row, version, now) is { } found) return found;
        return null;
    }

    private (LibrarySongRow, string, string)? Candidate(LibrarySongRow row, int version, DateTime now)
    {
        // No length is a file Navidrome could not read, and a long one is more than radio plays.
        if (row.Duration is null or <= 0 or > LongestSeconds) { _skipped.Add(row.Id); return null; }
        var known = _store.Read(s => (s.Songs.GetValueOrDefault(row.Id), s.Failed.GetValueOrDefault(row.Id)));
        // Cheap checks first, without touching the disk (a stat over a network mount is slow):
        // a song already analysed at this size and version, or one that failed at this size lately.
        if (known.Item1 is { } done && done.Version == version && done.Stamp.StartsWith(row.Size + ":", StringComparison.Ordinal))
            return null;
        if (known.Item2 is { } lately && lately.Stamp.StartsWith(row.Size + ":", StringComparison.Ordinal)
            && now - lately.AtUtc < RetryFailedAfter)
            return null;
        if (_library.LocalFile(row) is not { } file) { _skipped.Add(row.Id); return null; }
        var stamp = $"{file.Length}:{file.LastWriteTimeUtc.Ticks}";
        if (known.Item1 is { } same && same.Version == version && same.Stamp == stamp) return null;
        if (known.Item2 is { } failed && failed.Stamp == stamp && now - failed.AtUtc < RetryFailedAfter) return null;
        return (row, file.FullName, stamp);
    }

    /// <summary>The library, one song per id, and whether the list reached its end. Null when any
    /// page could not be read: a list with a hole would look like songs gone.</summary>
    private async Task<(List<LibrarySongRow> Rows, bool Complete)?> ListAsync(CancellationToken ct)
    {
        var rows = new List<LibrarySongRow>();
        for (var page = 0; page < 200; page++)
        {
            var answer = await _library.ListAsync(page * PageSize, PageSize, ct);
            if (answer is null) return null;
            rows.AddRange(answer.Value.Rows);
            // Count is the page's rows before missing and pathless songs were left out: a page
            // shorter than asked is the end, a page with a missing song is not.
            if (answer.Value.Count < PageSize)
                return (rows.DistinctBy(row => row.Id, StringComparer.Ordinal).ToList(), true);
        }
        return (rows.DistinctBy(row => row.Id, StringComparer.Ordinal).ToList(), false);
    }

    public void SetPaused(bool paused)
    {
        _store.Write(s => s.Paused = paused);
        _store.Flush();
    }

    /// <summary>Forget every song's features and start a new pass.</summary>
    public void Reset()
    {
        Interlocked.Increment(ref _generation);
        _rows = null;
        _skipped.Clear();
        _store.Write(s => { s.Songs.Clear(); s.Failed.Clear(); s.Pass = 0; s.NextPassUtc = null; s.PassFinishedUtc = null; });
        _store.Flush();
    }

    public SonicStatus Status()
    {
        var rows = _rows;
        // This pass's, or the last one's once it is done.
        var skipped = _skipped.Count;
        return _store.Read(s => new SonicStatus(_state, _reason, s.Songs.Count,
            rows?.Count ?? s.Songs.Count + s.Failed.Count, s.Failed.Count, s.Pass, s.NextPassUtc, s.Paused, skipped));
    }

    private TimeSpan Hold(string state, string? reason, TimeSpan wait)
    {
        _state = state; _reason = reason;
        return wait;
    }
}
