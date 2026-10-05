using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services.Library;

namespace Octo.Services.Health;

/// <summary>The report on the library as last read, with the songs fixed here left out.</summary>
public sealed record HealthSnapshot(HealthReport<LibrarySongRow> Report, IReadOnlyList<LibrarySongRow> Songs,
    ServerSongFields Fields, DateTime ReadUtc);

/// <summary>One song a lookup found tags for, or nothing for.</summary>
public sealed record HealthLookup(string Id, string Title, string State, string? Detail, string? Origin,
    IReadOnlyList<(TagChange Change, bool Pick)> Changes);

/// <summary>A run of fixes or lookups, and how it is going or went.</summary>
public sealed class HealthRun
{
    public required string Id { get; init; }
    /// <summary>fix, lookup or undo.</summary>
    public required string Kind { get; init; }
    public required string Label { get; init; }
    public HealthCheck? Check { get; init; }
    public required string User { get; init; }
    public required int Total { get; init; }
    public DateTime StartedUtc { get; init; } = DateTime.UtcNow;
    public int Done { get; set; }
    /// <summary>running, done, stopped or failed.</summary>
    public string State { get; set; } = "running";
    public DateTime? FinishedUtc { get; set; }
    public string? Current { get; set; }
    public FixOutcome? Outcome { get; set; }
    /// <summary>Added to while the run goes; lock it to read.</summary>
    public List<HealthLookup> Lookups { get; } = [];

    /// <summary>The lookups so far, copied.</summary>
    public IReadOnlyList<HealthLookup> LookupsNow()
    {
        lock (Lookups) return Lookups.ToList();
    }
    public string? Error { get; set; }
    internal volatile bool StopAsked;
}

/// <summary>
/// Library health on the server, for the dashboard: the same checks as the Octo app over the
/// library snapshot (Navidrome's own song list, read with Octo's admin sign-in and kept a few
/// minutes), and the same fixes, run one song at a time through the same services the app's
/// libraryAction calls, as the signed-in admin. One run at a time, never tied to a request, and
/// one Navidrome scan at its end. Songs fixed here are left out of their finding until a read of
/// the library from after Navidrome's scan shows them as they are now.
/// </summary>
public sealed class LibraryHealthService
{
    /// <summary>How long after a fix a library read is still taken to be from before Navidrome's scan.</summary>
    internal static TimeSpan SettleFor { get; set; } = TimeSpan.FromSeconds(90);

    /// <summary>Most steps one run takes.</summary>
    public const int MaxSteps = 5000;

    /// <summary>How many songs one lookup run takes: each is a fingerprint and a catalog search, a few seconds apiece.</summary>
    public const int LookupBatch = 25;

    private readonly LibrarySnapshot _snapshot;
    private readonly LibraryRescan _rescan;
    private readonly IOptionsMonitor<LibraryActionSettings> _settings;
    private readonly IServiceProvider _services;
    private readonly ILogger<LibraryHealthService> _logger;
    private readonly object _gate = new();

    private (LibraryAlbumIndex Index, HealthReport<LibrarySongRow> Report, ServerSongFields Fields, DateTime ReadUtc)? _checked;
    private Task<HealthSnapshot?>? _checking;
    private DateTime? _checkingSince;

    // Songs fixed here: by check, each with when; and songs removed here, with when.
    private readonly Dictionary<HealthCheck, Dictionary<string, DateTime>> _settled = [];
    private readonly Dictionary<string, DateTime> _removed = new(StringComparer.Ordinal);

    private HealthRun? _run;
    private (string User, IReadOnlyList<FixStep> Steps, string RunId)? _undo;

    public LibraryHealthService(LibrarySnapshot snapshot, LibraryRescan rescan, IOptionsMonitor<LibraryActionSettings> settings,
        IServiceProvider services, ILogger<LibraryHealthService> logger)
    {
        _snapshot = snapshot;
        _rescan = rescan;
        _settings = settings;
        _services = services;
        _logger = logger;
    }

    internal Func<DateTime> Clock { get; set; } = () => DateTime.UtcNow;

    /// <summary>How one step is sent; tests answer here instead of touching files.</summary>
    internal Func<FixStep, string, Task<FixAnswer>>? Send { get; set; }

    /// <summary>How one song is looked up; tests answer here.</summary>
    internal Func<string, Task<SongLookup>>? Look { get; set; }

    /// <summary>The run going now or last, for the page and the activity toasts.</summary>
    public HealthRun? Run
    {
        get { lock (_gate) return _run; }
    }

    /// <summary>When the library check now running began, or null.</summary>
    public DateTime? CheckingSince
    {
        get { lock (_gate) return _checkingSince; }
    }

    /// <summary>Whether the last finished run can be undone, and how many songs that touches.</summary>
    public int UndoCount
    {
        get { lock (_gate) return _undo?.Steps.Count ?? 0; }
    }

    /// <summary>
    /// The report, checking the library first when it changed since: <paramref name="fresh"/>
    /// reads it again from Navidrome (the page's Check again). Null when Navidrome cannot be read.
    /// </summary>
    public async Task<HealthSnapshot?> ReportAsync(bool fresh, CancellationToken ct = default)
    {
        if (fresh)
        {
            lock (_gate) _checkingSince ??= Clock();
            // A read from now on; the check below then finds it as the current one.
            try { await _snapshot.RefreshAsync(ct); }
            finally { lock (_gate) if (_checking is null) _checkingSince = null; }
        }
        Task<HealthSnapshot?> checking;
        lock (_gate) checking = _checking ??= CheckAsync();
        var found = await checking.WaitAsync(ct);
        return found is null ? null : Settled(found);
    }

    private async Task<HealthSnapshot?> CheckAsync()
    {
        await Task.Yield();
        lock (_gate) _checkingSince ??= Clock();
        try
        {
            var index = await _snapshot.CurrentAsync();
            if (index is null) return null;
            var readUtc = _snapshot.ReadAtUtc ?? Clock();
            lock (_gate)
                if (_checked is { } known && ReferenceEquals(known.Index, index))
                    return new HealthSnapshot(known.Report, index.Rows, known.Fields, known.ReadUtc);
            var started = DateTime.UtcNow;
            var (report, fields) = await Task.Run(() =>
            {
                var fields = new ServerSongFields(index.Rows);
                return (LibraryHealth.Check(index.Rows, fields), fields);
            });
            _logger.LogInformation("Library health checked {Songs} songs in {Ms} ms: {Findings}", report.Checked,
                (int)(DateTime.UtcNow - started).TotalMilliseconds,
                string.Join(", ", report.Findings.Select(check => $"{check} {report.Count(check)}")));
            lock (_gate) _checked = (index, report, fields, readUtc);
            return new HealthSnapshot(report, index.Rows, fields, readUtc);
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Library health could not check the library: {M}", ex.Message);
            return null;
        }
        finally
        {
            lock (_gate)
            {
                _checking = null;
                _checkingSince = null;
            }
        }
    }

    /// <summary>The report with what was fixed here left out, while the library read is older than the fix.</summary>
    private HealthSnapshot Settled(HealthSnapshot found)
    {
        lock (_gate)
        {
            bool Stale(DateTime at) => found.ReadUtc < at + SettleFor;
            foreach (var gone in _removed.Where(pair => !Stale(pair.Value)).Select(pair => pair.Key).ToList()) _removed.Remove(gone);
            foreach (var ids in _settled.Values)
            foreach (var gone in ids.Where(pair => !Stale(pair.Value)).Select(pair => pair.Key).ToList())
                ids.Remove(gone);
            var report = found.Report.Without(_removed.Keys.ToHashSet(StringComparer.Ordinal), found.Fields);
            foreach (var (check, ids) in _settled)
                report = report.SettledFor(check, ids.Keys.ToHashSet(StringComparer.Ordinal), found.Fields);
            return found with { Report = report };
        }
    }

    // ---- who may, and what ------------------------------------------------------------------

    /// <summary>What the server lets this admin do from the page, the same flags the app reads.</summary>
    public HealthAbilities Abilities(string user)
    {
        var settings = _settings.CurrentValue;
        var enabled = settings.EffectiveActions().Where(a => a.Enabled).Select(a => a.Action).ToHashSet();
        var edits = _services.GetService<LibraryEditService>() is not null;
        var real = settings.Enabled && settings.IsAllowed(user) && !settings.DryRun;
        var sources = _services.GetService<UpgradeSources>();
        var upgradeReady = sources?.Ready ?? false;
        var why = !settings.Enabled ? "Library actions are off, so nothing can be fixed from here. Turn them on under Library actions."
            : !settings.IsAllowed(user) ? $"{user} is not on the library actions allowed list, so Octo will not change files for them."
            : settings.DryRun ? "Library actions are in rehearsal mode, so a fix would only be rehearsed. Turn rehearsal off under Library actions."
            : !edits ? "This server cannot change library files."
            : null;
        return new HealthAbilities(
            Remove: real && enabled.Contains(LibraryAction.Delete) && _services.GetService<LibraryActionExecutor>() is not null,
            Edit: real && edits,
            JoinAlbums: real && edits,
            LookUp: real && edits,
            AddCover: real && edits && _services.GetService<Octo.Services.CoverArt.IAlbumCoverFinder>() is not null,
            Restore: real && edits,
            Upgrade: settings.Enabled && settings.IsAllowed(user) && !settings.DryRun && enabled.Contains(LibraryAction.BetterQuality) && upgradeReady,
            DryRun: settings.DryRun,
            KeepDays: settings.EffectiveQuarantineRetentionDays,
            Why: why);
    }

    /// <summary>Why this caller may not change files, or null when they may: the app path's checks.</summary>
    public string? Refusal(string? user)
    {
        if (_services.GetService<LibraryEditService>() is not { } edits) return "This server cannot change library files.";
        return edits.Refusal(user, admin: true);
    }

    /// <summary>Why these steps are not ones the page may send, or null.</summary>
    public static string? Invalid(IReadOnlyList<FixStep> steps)
    {
        if (steps.Count == 0) return "Nothing to do.";
        if (steps.Count > MaxSteps) return $"At most {MaxSteps:N0} changes at a time.";
        foreach (var step in steps)
        {
            if (string.IsNullOrWhiteSpace(step.Id)) return "A change names no song.";
            if (!FixActions.All.Contains(step.Action)) return $"Unknown change \"{step.Action}\".";
            if (step.Action == FixActions.Retag)
            {
                if (step.Params.Count == 0) return $"No tags to write for {step.Title}.";
                if (step.Params.Keys.FirstOrDefault(key => SongTagFields.Canonical(key) is null) is { } unknown)
                    return $"Unknown tag \"{unknown}\".";
                if (LibraryTagEdits.Invalid(step.Params.ToDictionary(pair => SongTagFields.Canonical(pair.Key)!, pair => (string?)pair.Value)) is { } bad)
                    return bad;
            }
            if (step.Action == FixActions.JoinAlbum && string.IsNullOrWhiteSpace(step.Params.GetValueOrDefault("like")))
                return $"No album to join for {step.Title}.";
        }
        return null;
    }

    // ---- runs ---------------------------------------------------------------------------------

    /// <summary>Starts a run of fix steps as <paramref name="user"/>; the run going now, when one is.</summary>
    public HealthRun? StartFix(string label, HealthCheck? check, IReadOnlyList<FixStep> steps, string user, bool isUndo = false)
    {
        HealthRun run;
        lock (_gate)
        {
            if (_run is { State: "running" } busy) return busy;
            run = new HealthRun
            {
                Id = Guid.NewGuid().ToString("N")[..12], Kind = isUndo ? "undo" : "fix", Label = label, Check = check,
                User = user, Total = steps.Count, StartedUtc = Clock(),
            };
            _run = run;
            if (isUndo) _undo = null;
        }
        _logger.LogInformation("{User} started Library health \"{Label}\" from the dashboard: {Count} change(s)", user, label, steps.Count);
        using (ExecutionContext.SuppressFlow()) _ = Task.Run(() => FixAsync(run, steps, check));
        return null;
    }

    /// <summary>Starts a lookup of up to <see cref="LookupBatch"/> songs; the run going now, when one is.</summary>
    public HealthRun? StartLookup(IReadOnlyList<(string Id, string Title)> songs, HealthCheck? check, string user)
    {
        HealthRun run;
        lock (_gate)
        {
            if (_run is { State: "running" } busy) return busy;
            run = new HealthRun
            {
                Id = Guid.NewGuid().ToString("N")[..12], Kind = "lookup",
                Label = songs.Count == 1 ? HealthWords.LookUpTags : $"Looking up {HealthWords.CountText(songs.Count, "song", "songs")}",
                Check = check, User = user, Total = songs.Count, StartedUtc = Clock(),
            };
            _run = run;
        }
        using (ExecutionContext.SuppressFlow()) _ = Task.Run(() => LookupAsync(run, songs));
        return null;
    }

    /// <summary>Puts back everything the last finished run did. Null when it started; else why not.</summary>
    public string? StartUndo(string user)
    {
        (string User, IReadOnlyList<FixStep> Steps, string RunId) undo;
        lock (_gate)
        {
            if (_run is { State: "running" }) return "A fix is still running. Undo once it is done.";
            if (_undo is not { } last) return "There is nothing to undo.";
            undo = last;
        }
        return StartFix("Undoing the last change", null, undo.Steps, user, isUndo: true) is null ? null : "A fix is still running.";
    }

    /// <summary>Ends the run going now after the song it is on.</summary>
    public bool Stop()
    {
        lock (_gate)
        {
            if (_run is not { State: "running" } run) return false;
            run.StopAsked = true;
            return true;
        }
    }

    private async Task FixAsync(HealthRun run, IReadOnlyList<FixStep> steps, HealthCheck? check)
    {
        var done = new List<FixStep>();
        var unchanged = new List<FixStep>();
        var failed = new List<(FixStep, string)>();
        var rehearsed = false;
        var stopped = false;
        try
        {
            using (_rescan.Hold())
            {
                foreach (var step in steps)
                {
                    if (run.StopAsked)
                    {
                        stopped = true;
                        break;
                    }
                    run.Current = step.Title;
                    FixAnswer answer;
                    try { answer = await (Send ?? SendAsync)(step, run.User); }
                    catch (Exception ex) { answer = new FixAnswer("failed", ex.Message); }
                    switch (answer.State)
                    {
                        case "applied": done.Add(step); break;
                        case "rehearsed": rehearsed = true; break;
                        case "skipped" when HealthFixes.IsNothingToDo(answer.Detail): unchanged.Add(step); break;
                        default: failed.Add((step, answer.Detail is { Length: > 0 } words ? words : $"the server said {answer.State}")); break;
                    }
                    run.Done++;
                }
            }
            var outcome = new FixOutcome(done, unchanged, failed, rehearsed, stopped);
            var now = Clock();
            lock (_gate)
            {
                foreach (var id in outcome.Removed) _removed[id] = now;
                foreach (var id in done.Where(step => step.Action == FixActions.Restore).Select(step => step.Id)) _removed.Remove(id);
                if (check is { } settled)
                {
                    if (!_settled.TryGetValue(settled, out var ids)) _settled[settled] = ids = new(StringComparer.Ordinal);
                    foreach (var step in done.Where(step => step.Action is FixActions.Retag or FixActions.JoinAlbum or FixActions.Cover))
                        ids[step.Id] = now;
                }
                // An undo puts songs back as they were, so they belong in their findings again.
                if (run.Kind == "undo")
                    foreach (var ids in _settled.Values)
                    foreach (var step in done.Where(step => step.Action == FixActions.Undo)) ids.Remove(step.Id);
                if (run.Kind != "undo" && outcome.UndoSteps.Count > 0) _undo = (run.User, outcome.UndoSteps, run.Id);
                run.Outcome = outcome;
                run.State = stopped ? "stopped" : "done";
                run.FinishedUtc = now;
                run.Current = null;
            }
            _logger.LogInformation("{User} ran Library health \"{Label}\": {Summary}", run.User, run.Label, outcome.Summary());
            if (done.Count > 0) ScheduleRead();
        }
        catch (Exception ex)
        {
            _logger.LogError(ex, "Library health run \"{Label}\" failed", run.Label);
            lock (_gate)
            {
                run.Outcome = new FixOutcome(done, unchanged, failed, rehearsed, true);
                if (done.Count > 0 && run.Kind != "undo") _undo = (run.User, run.Outcome.UndoSteps, run.Id);
                run.State = "failed";
                run.Error = ex.Message;
                run.FinishedUtc = Clock();
            }
        }
    }

    private async Task LookupAsync(HealthRun run, IReadOnlyList<(string Id, string Title)> songs)
    {
        try
        {
            var look = Look ?? (id => _services.GetRequiredService<LibraryEditService>().LookupAsync(id));
            foreach (var (id, title) in songs)
            {
                if (run.StopAsked) break;
                run.Current = title;
                SongLookup lookup;
                try { lookup = await look(id); }
                catch (Exception ex) when (ex is not OperationCanceledException)
                {
                    lookup = new SongLookup("failed", ex.Message, new Dictionary<string, string?>(), new Dictionary<string, string?>(), null, null, null);
                }
                var found = lookup.State == "found";
                lock (run.Lookups)
                    run.Lookups.Add(new HealthLookup(id, title, lookup.State,
                        found ? null : lookup.Detail is { Length: > 0 } detail ? detail : $"Nothing was found for {title}.",
                        found ? HealthFixes.Origin(lookup) : null,
                        found ? HealthFixes.Changes(lookup) : []));
                run.Done++;
            }
            lock (_gate)
            {
                run.State = run.StopAsked && run.Done < run.Total ? "stopped" : "done";
                run.FinishedUtc = Clock();
                run.Current = null;
            }
        }
        catch (Exception ex)
        {
            _logger.LogError(ex, "Library health lookup failed");
            lock (_gate)
            {
                run.State = "failed";
                run.Error = ex.Message;
                run.FinishedUtc = Clock();
            }
        }
    }

    /// <summary>One step, through the same service the app's libraryAction uses.</summary>
    internal async Task<FixAnswer> SendAsync(FixStep step, string user)
    {
        var none = CancellationToken.None;
        if (step.Action == FixActions.Remove)
        {
            if (_services.GetService<LibraryActionExecutor>() is not { } executor) return new("skipped", "Library actions are off.");
            var copy = step.Params.GetValueOrDefault("copy") is "true" or "1";
            var outcome = await executor.ApplyAsync(new LibraryActionRequest(LibraryAction.Delete, step.Id, user, OnlyACopy: copy), none);
            // The executor never asks for a scan itself; the run's one scan covers it.
            if (outcome.State == LibraryActionState.Applied) _rescan.Soon();
            return new(outcome.State switch
            {
                LibraryActionState.Applied => "applied",
                LibraryActionState.Rehearsed => "rehearsed",
                LibraryActionState.Skipped => "skipped",
                LibraryActionState.Unresolved => "unresolved",
                _ => "failed",
            }, outcome.Detail);
        }
        if (_services.GetService<LibraryEditService>() is not { } edits) return new("skipped", "This server cannot change library files.");
        var result = step.Action switch
        {
            FixActions.Retag => await edits.RetagAsync(step.Id, user,
                step.Params.ToDictionary(pair => SongTagFields.Canonical(pair.Key) ?? pair.Key, pair => (string?)pair.Value), none),
            FixActions.JoinAlbum => await edits.JoinAlbumAsync(step.Id, step.Params.GetValueOrDefault("like") ?? "", user, none),
            FixActions.Cover => await edits.AddCoverAsync(step.Id, user, preview: false, none),
            FixActions.Undo => await edits.UndoAsync(step.Id, user, none),
            FixActions.Restore => await edits.RestoreAsync(step.Id, user),
            _ => new LibraryEditOutcome("failed", $"Unknown change \"{step.Action}\"."),
        };
        return new(result.State, result.Detail);
    }

    /// <summary>A read of the library a little after Navidrome's scan, so the page catches up by itself.</summary>
    private void ScheduleRead()
    {
        using (ExecutionContext.SuppressFlow())
            _ = Task.Run(async () =>
            {
                try
                {
                    await Task.Delay(SettleFor);
                    await ReportAsync(fresh: true);
                }
                catch (Exception ex) { _logger.LogDebug("Library health re-read failed: {M}", ex.Message); }
            });
    }
}

/// <summary>What the page may offer: the same flags the app's library actions answer gives.</summary>
public sealed record HealthAbilities(bool Remove, bool Edit, bool JoinAlbums, bool LookUp, bool AddCover, bool Restore,
    bool Upgrade, bool DryRun, int KeepDays, string? Why);
