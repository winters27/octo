using System.Threading.Channels;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Local;

namespace Octo.Services.Metadata;

public sealed record GenreBackfillRequest(GenreBackfillScope Scope, bool DryRun, bool Undo = false);

/// <summary>
/// Re-tags genres across files that are already in the library.
///
/// A BackgroundService rather than a request, because a 1,900-file walk does not fit in one
/// and the shutdown budget is ten seconds. One run at a time: a second request gets 409 rather
/// than a parallel walk over the same files.
/// </summary>
public sealed class GenreBackfillWorker : BackgroundService
{
    /// <summary>
    /// The early-failure stop condition. The consecutive-failure ceiling is a setting, because
    /// how much a user tolerates before giving up is a preference; this ratio is a sanity check
    /// on the first sample and is not worth a knob.
    /// </summary>
    private const int EarlySampleSize = 200;
    private const double EarlyFailureRatio = 0.10;

    private readonly Channel<GenreBackfillRequest> _queue =
        Channel.CreateBounded<GenreBackfillRequest>(new BoundedChannelOptions(1)
        { FullMode = BoundedChannelFullMode.DropWrite });

    private readonly GenreBackfillStore _store;
    private readonly GenreBackfillJournal _journal;
    private readonly IOptionsMonitor<GenreSettings> _settings;
    private readonly IConfiguration _configuration;
    private readonly IServiceScopeFactory _scopes;
    private readonly ILogger<GenreBackfillWorker> _logger;

    private volatile bool _cancelRequested;

    public GenreBackfillWorker(GenreBackfillStore store, GenreBackfillJournal journal,
        IOptionsMonitor<GenreSettings> settings, IConfiguration configuration,
        IServiceScopeFactory scopes, ILogger<GenreBackfillWorker> logger)
    {
        _store = store;
        _journal = journal;
        _settings = settings;
        _configuration = configuration;
        _scopes = scopes;
        _logger = logger;
    }

    public bool IsRunning => _store.Current.Status == GenreBackfillStatus.Running;

    /// <summary>The run the dashboard polls. Exposed here so the controller has one thing to
    /// talk to rather than needing the store as well.</summary>
    public GenreBackfillRun Current => _store.Current;

    /// <summary>False when a run is already going, which the controller turns into a 409.</summary>
    public bool TryEnqueue(GenreBackfillRequest request)
    {
        if (IsRunning) return false;
        return _queue.Writer.TryWrite(request);
    }

    public void RequestCancel() => _cancelRequested = true;

    protected override async Task ExecuteAsync(CancellationToken stoppingToken)
    {
        await foreach (var request in _queue.Reader.ReadAllAsync(stoppingToken))
        {
            // Per-item catch is mandatory: BackgroundServiceExceptionBehavior defaults to
            // StopHost, so a single unhandled exception here would take Octo down.
            try
            {
                if (request.Undo) await RunUndoAsync(stoppingToken);
                else await RunAsync(request, stoppingToken);
            }
            catch (OperationCanceledException) { break; }
            catch (Exception ex)
            {
                _logger.LogError(ex, "Genre backfill failed");
                _store.Update(run =>
                {
                    run.Status = GenreBackfillStatus.Failed;
                    run.Reason = ex.Message;
                    run.FinishedUtc = DateTime.UtcNow;
                });
            }
        }
    }

    private async Task RunAsync(GenreBackfillRequest request, CancellationToken stoppingToken)
    {
        _cancelRequested = false;

        var resuming = request.Scope == _store.Current.Scope
            && request.DryRun == _store.Current.DryRun
            && _store.Current.CanResume;

        List<string> queue;
        if (resuming)
        {
            queue = _store.Current.Queue;
            _store.Update(run =>
            {
                run.Status = GenreBackfillStatus.Running;
                run.Reason = null;
            });
            _logger.LogInformation("Genre backfill resuming at {Cursor}/{Total}",
                _store.Current.Cursor, queue.Count);
        }
        else
        {
            queue = (await EnumerateAsync(request.Scope)).ToList();
            _store.Replace(new GenreBackfillRun
            {
                RunId = Guid.NewGuid().ToString("N")[..12],
                Status = GenreBackfillStatus.Running,
                Scope = request.Scope,
                DryRun = request.DryRun,
                StartedUtc = DateTime.UtcNow,
                Total = queue.Count,
                Queue = queue,
            });
            _logger.LogInformation("Genre backfill started: {Count} file(s), scope {Scope}, dryRun {DryRun}",
                queue.Count, request.Scope, request.DryRun);
        }

        // Snapshot the settings at run start rather than reading per file, so a run's results
        // are explainable: a mid-run settings edit would otherwise produce a file where half
        // the library followed one table and half followed another.
        var settings = _settings.CurrentValue;
        var runId = _store.Current.RunId;
        var dryRun = _store.Current.DryRun;
        var consecutiveFailures = 0;

        for (var index = _store.Current.Cursor; index < queue.Count; index++)
        {
            // Checked between files, never mid-file: a half-written tag block is a corrupt file.
            if (stoppingToken.IsCancellationRequested)
            {
                _store.Update(run => run.Status = GenreBackfillStatus.Interrupted);
                return;
            }
            if (_cancelRequested)
            {
                _store.Update(run =>
                {
                    run.Status = GenreBackfillStatus.Cancelled;
                    run.FinishedUtc = DateTime.UtcNow;
                    run.Reason = "Cancelled from the dashboard.";
                });
                _logger.LogInformation("Genre backfill cancelled at {Cursor}/{Total}", index, queue.Count);
                return;
            }

            var path = queue[index];
            var outcome = ProcessFile(path, settings, dryRun, runId);

            _store.Update(run =>
            {
                run.Cursor = index + 1;
                run.Processed++;
                run.LastPath = path;
                switch (outcome.Kind)
                {
                    case FileOutcome.Changed:
                        run.Changed++;
                        if (outcome.Change!.Action == "Clear") run.Cleared++;
                        if (run.Preview.Count < GenreBackfillStore.MaxPreviewRows) run.Preview.Add(outcome.Change);
                        break;
                    case FileOutcome.Skipped: run.Skipped++; break;
                    case FileOutcome.Failed:
                        run.Failed++;
                        run.Errors.Add($"{path}: {outcome.Error}");
                        break;
                }
            });

            consecutiveFailures = outcome.Kind == FileOutcome.Failed ? consecutiveFailures + 1 : 0;

            var current = _store.Current;
            var earlyRatioBreached = current.Processed >= EarlySampleSize
                && current.Failed >= current.Processed * EarlyFailureRatio
                && current.Processed <= EarlySampleSize * 2;

            var failureCeiling = settings.EffectiveBackfillMaxConsecutiveFailures;
            if ((failureCeiling > 0 && consecutiveFailures >= failureCeiling) || earlyRatioBreached)
            {
                var reason = failureCeiling > 0 && consecutiveFailures >= failureCeiling
                    ? $"{consecutiveFailures} files in a row could not be written. Is the music directory read-only?"
                    : $"{current.Failed} of the first {current.Processed} files could not be written.";
                _store.Update(run =>
                {
                    run.Status = GenreBackfillStatus.Failed;
                    run.FinishedUtc = DateTime.UtcNow;
                    run.Reason = reason;
                });
                _logger.LogError("Genre backfill stopped: {Reason}", reason);
                return;
            }

            // Yield so a long walk does not monopolise the thread pool.
            if (index % 50 == 49) await Task.Yield();
        }

        _store.Update(run =>
        {
            run.Status = GenreBackfillStatus.Completed;
            run.FinishedUtc = DateTime.UtcNow;
        });

        var final = _store.Current;
        _logger.LogInformation(
            "Genre backfill {Mode}: {Changed} changed ({Cleared} cleared), {Skipped} skipped, {Failed} failed, of {Total}",
            dryRun ? "preview finished" : "finished", final.Changed, final.Cleared,
            final.Skipped, final.Failed, final.Total);

        if (!dryRun && final.Changed > 0) await RescanAsync();
    }

    private async Task RunUndoAsync(CancellationToken stoppingToken)
    {
        _cancelRequested = false;

        var entries = _journal.ReadAll();
        _store.Replace(new GenreBackfillRun
        {
            RunId = Guid.NewGuid().ToString("N")[..12],
            Status = GenreBackfillStatus.Running,
            Scope = GenreBackfillScope.WholeLibrary,
            DryRun = false,
            StartedUtc = DateTime.UtcNow,
            Total = entries.Count,
            Reason = "Undoing the last genre backfill.",
        });
        _logger.LogInformation("Genre backfill undo started: {Count} entries", entries.Count);

        // Newest first. A file changed twice is restored to the frame the OLDEST entry saw,
        // because that one is applied last.
        var restored = 0;
        var seen = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        foreach (var entry in entries)
        {
            if (stoppingToken.IsCancellationRequested || _cancelRequested) break;

            try
            {
                if (!File.Exists(entry.Path))
                {
                    // Keyed by path, so a file moved since the run cannot be found and stays
                    // rewritten. Counted as skipped rather than failed: nothing went wrong here.
                    _store.Update(run => run.Skipped++);
                    continue;
                }

                using var tagFile = TagLib.File.Create(entry.Path);
                tagFile.Tag.Genres = entry.Before.ToArray();
                tagFile.Save();
                restored++;
                if (seen.Add(entry.Path)) _store.Update(run => run.Changed++);
            }
            catch (Exception ex)
            {
                _store.Update(run =>
                {
                    run.Failed++;
                    run.Errors.Add($"{entry.Path}: {ex.Message}");
                });
            }
            finally
            {
                _store.Update(run => { run.Processed++; run.LastPath = entry.Path; });
            }
        }

        _journal.Clear();
        _store.Update(run =>
        {
            run.Status = GenreBackfillStatus.Completed;
            run.FinishedUtc = DateTime.UtcNow;
            run.Reason = $"Restored the genre frame on {restored} file(s).";
        });
        _logger.LogInformation("Genre backfill undo finished: {Count} restored", restored);

        if (restored > 0) await RescanAsync();
    }

    private enum FileOutcome { Unchanged, Changed, Skipped, Failed }

    private readonly record struct ProcessResult(FileOutcome Kind, GenreBackfillChange? Change, string? Error);

    private ProcessResult ProcessFile(string path, GenreSettings settings, bool dryRun, string runId)
    {
        try
        {
            using var tagFile = TagLib.File.Create(path);
            var before = tagFile.Tag.Genres ?? [];

            // No resolved genre and no fallback: a backfill is local-only by design, so a run
            // over two thousand files makes no network calls at all.
            var plan = GenreNormalizer.Plan(before, null, settings);
            if (plan.Action == GenreTagAction.None) return new(FileOutcome.Unchanged, null, null);

            var after = plan.Action == GenreTagAction.Clear ? [] : plan.Genres;
            if (before.SequenceEqual(after, StringComparer.Ordinal))
                return new(FileOutcome.Unchanged, null, null);

            var change = new GenreBackfillChange(path, before, after,
                plan.Action == GenreTagAction.Clear ? "Clear" : "Write", plan.MatchedRule);

            if (dryRun) return new(FileOutcome.Changed, change, null);

            // Journal BEFORE the write. A crash between the two costs an undo entry for a file
            // that was not changed, which is harmless; the other order loses the only record of
            // a file that WAS.
            _journal.Append(new GenreJournalEntry(path, before, after, DateTime.UtcNow, runId));

            tagFile.Tag.Genres = after.ToArray();
            tagFile.Save();
            return new(FileOutcome.Changed, change, null);
        }
        catch (Exception ex) when (ex is TagLib.CorruptFileException or TagLib.UnsupportedFormatException)
        {
            // A cue sheet, a weird container, a stream TagLib does not model. Not an error.
            _logger.LogDebug("Genre backfill skipped {Path}: {M}", path, ex.Message);
            return new(FileOutcome.Skipped, null, null);
        }
        catch (Exception ex) when (ex is UnauthorizedAccessException or IOException)
        {
            return new(FileOutcome.Failed, null, ex.Message);
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Genre backfill could not process {Path}: {M}", path, ex.Message);
            return new(FileOutcome.Failed, null, ex.Message);
        }
    }

    private async Task<IReadOnlyList<string>> EnumerateAsync(GenreBackfillScope scope)
    {
        using var scopeHandle = _scopes.CreateScope();
        var library = scopeHandle.ServiceProvider.GetRequiredService<ILocalLibraryService>();

        if (scope == GenreBackfillScope.OctoDownloads)
        {
            var mappings = await library.GetMappingsAsync();
            return mappings
                .Select(mapping => mapping.LocalPath)
                .Where(path => !string.IsNullOrEmpty(path) && File.Exists(path))
                .Distinct(StringComparer.OrdinalIgnoreCase)
                .OrderBy(path => path, StringComparer.Ordinal)
                .ToList();
        }

        var extensions = _settings.CurrentValue.EffectiveBackfillExtensions();
        var root = _configuration["Library:DownloadPath"] ?? "./downloads";
        if (!Directory.Exists(root))
        {
            _logger.LogWarning("Genre backfill found no music directory at {Root}", root);
            return [];
        }

        try
        {
            return Directory
                .EnumerateFiles(root, "*", SearchOption.AllDirectories)
                .Where(path => extensions.Contains(Path.GetExtension(path)))
                .OrderBy(path => path, StringComparer.Ordinal)
                .ToList();
        }
        catch (Exception ex)
        {
            _logger.LogError(ex, "Genre backfill could not enumerate {Root}", root);
            return [];
        }
    }

    /// <summary>
    /// force: true is not optional. The scan is debounced, and a run that finishes inside the
    /// debounce window would leave every rewrite invisible to Navidrome, which reads as the
    /// button having done nothing.
    /// </summary>
    private async Task RescanAsync()
    {
        try
        {
            using var scope = _scopes.CreateScope();
            var library = scope.ServiceProvider.GetRequiredService<ILocalLibraryService>();
            await library.TriggerLibraryScanAsync(force: true);
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Genre backfill could not trigger a library scan: {M}", ex.Message);
        }
    }
}
