using System.Collections.Concurrent;

namespace Octo.Services.Common;

/// <summary>
/// Wraps a <see cref="SingleFlight{TKey,TValue}"/> keyed build with prefix-based
/// supersession for interactive type-ahead search: a shorter, still-running query is
/// cancelled the moment a longer query that extends it (or vice versa) arrives, since the
/// shorter one's result is about to be replaced in the client's own UI anyway.
///
/// This exists because Amperfy (and most Subsonic clients' type-ahead) fires one search3
/// call per keystroke, uncancelled. Each partial-word build competed for the same
/// rate-limited external lane as the query the user actually meant, and the real query
/// could blow its own timeout waiting behind three stale ones. Cancelling a superseded
/// build frees that lane immediately instead of waiting out its own timeout.
/// </summary>
public sealed class SupersedableBuildCoordinator<TValue>
{
    private readonly ConcurrentDictionary<string, CancellationTokenSource> _activeBuilds = new();
    private readonly SingleFlight<string, TValue> _flight = new();

    /// <summary>
    /// Run <paramref name="factory"/> for <paramref name="query"/>, cancelling any
    /// still-running build for a shorter or longer query that shares a prefix with this
    /// one, then join or start this query's own build.
    /// </summary>
    /// <param name="fallback">Returned if the build fails or times out, instead of throwing.</param>
    /// <param name="onFailure">Invoked with the query and exception on failure, for logging.</param>
    public async Task<TValue> RunAsync(
        string query,
        Func<CancellationToken, Task<TValue>> factory,
        TimeSpan timeout,
        TValue fallback,
        Action<string, Exception> onFailure)
    {
        var key = query.ToLowerInvariant();

        // Cancel any in-flight build whose key is a prefix of this one or vice versa.
        // Never cancel a build another caller has already joined: that caller's request
        // is not superseded just because this one also started, and killing it would fail
        // a search that has nothing to do with type-ahead churn.
        foreach (var (activeKey, cts) in _activeBuilds)
        {
            if (activeKey == key)
            {
                continue;
            }

            var isPrefixOfThis = activeKey.Length < key.Length && key.StartsWith(activeKey, StringComparison.Ordinal);
            var thisIsPrefixOfIt = key.Length < activeKey.Length && activeKey.StartsWith(key, StringComparison.Ordinal);

            if ((isPrefixOfThis || thisIsPrefixOfIt) && _flight.GetJoinCount(activeKey) <= 1)
            {
                try
                {
                    cts.Cancel();
                }
                catch (ObjectDisposedException)
                {
                    // The build already finished and disposed its token between the
                    // snapshot above and this cancel; nothing left to supersede.
                }
            }
        }

        try
        {
            return await _flight.RunAsync(key, token => RunTrackedAsync(key, token, factory), timeout);
        }
        catch (Exception ex)
        {
            onFailure(query, ex);
            return fallback;
        }
    }

    private async Task<TValue> RunTrackedAsync(string key, CancellationToken token, Func<CancellationToken, Task<TValue>> factory)
    {
        using var linked = CancellationTokenSource.CreateLinkedTokenSource(token);
        // Only the caller that actually starts the build reaches here and registers a
        // token to cancel: a joiner returns from SingleFlight.RunAsync's early-join path
        // and never runs this factory at all.
        _activeBuilds[key] = linked;
        try
        {
            return await factory(linked.Token);
        }
        finally
        {
            _activeBuilds.TryRemove(key, out _);
        }
    }
}
