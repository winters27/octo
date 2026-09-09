using System.Collections.Concurrent;

namespace Octo.Services.Common;

/// <summary>
/// Collapses concurrent identical work onto one execution. Callers arriving while a key
/// is already running join the running task instead of starting their own.
///
/// This holds no results. Once the work finishes the entry is gone, so there is no
/// staleness policy, no negative-caching rule, and nothing for a poisoned entry to
/// survive in. That is deliberate: the failure mode we are avoiding is a keyed dictionary
/// of tasks that outlives its usefulness, which on arbitrary user input becomes a
/// user-triggerable leak.
/// </summary>
public sealed class SingleFlight<TKey, TValue> where TKey : notnull
{
    private sealed class Entry
    {
        public readonly TaskCompletionSource<TValue> Source =
            new(TaskCreationOptions.RunContinuationsAsynchronously);

        /// <summary>
        /// Callers currently sharing this execution, including the owner. A supersession
        /// policy (see <see cref="SupersedableBuildCoordinator{TValue}"/>) reads this to
        /// decide whether cancelling this key would only hurt its own caller or would also
        /// take a joined caller down with it.
        /// </summary>
        public int Joins = 1;
    }

    private readonly ConcurrentDictionary<TKey, Entry> _inFlight = new();

    /// <summary>Number of executions currently running. Exposed for tests and logging.</summary>
    public int InFlightCount => _inFlight.Count;

    /// <summary>How many callers are currently sharing the execution for <paramref name="key"/>, or 0 if none is running.</summary>
    public int GetJoinCount(TKey key) => _inFlight.TryGetValue(key, out var entry) ? entry.Joins : 0;

    /// <summary>
    /// Run <paramref name="factory"/> for <paramref name="key"/>, or join the execution
    /// already running for it.
    /// </summary>
    /// <param name="timeout">
    /// Deadline for the work itself. The factory never sees a caller's cancellation token:
    /// joined callers share one execution, so letting the first one to disconnect cancel it
    /// would take everyone else down with it. A deadline is what stops a hung dependency
    /// pinning the key instead.
    /// </param>
    public async Task<TValue> RunAsync(
        TKey key,
        Func<CancellationToken, Task<TValue>> factory,
        TimeSpan timeout)
    {
        var mine = new Entry();

        var existing = _inFlight.GetOrAdd(key, mine);
        if (!ReferenceEquals(existing, mine))
        {
            // Someone else owns this key. Join them rather than doing the work twice.
            Interlocked.Increment(ref existing.Joins);
            return await existing.Source.Task;
        }

        try
        {
            using var cts = new CancellationTokenSource(timeout);
            mine.Source.SetResult(await factory(cts.Token));
        }
        catch (Exception ex)
        {
            // Every joiner sees the failure, and the finally below removes the entry, so
            // the next caller retries rather than inheriting a permanently faulted task.
            mine.Source.SetException(ex);
        }
        finally
        {
            // After the result is set, not before: a caller arriving in between joins a
            // completed task and gets the answer, where the reverse order would have it
            // start a redundant execution.
            _inFlight.TryRemove(key, out _);
        }

        return await mine.Source.Task;
    }
}
