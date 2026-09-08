namespace Octo.Services.Subsonic;

/// <summary>
/// Splits a client's requested song count between local library results and external
/// discovery results for search3 / search2.
///
/// This exists because the split used to be a flat local floor of 20 that could consume
/// the entire budget. Since 20 is also the Subsonic spec default for songCount (and this
/// server's own fallback when the parameter is absent), the most common search in the
/// wild reserved every slot for local results and generated no discovery at all, which
/// is what made search look like it only ever returned albums.
///
/// The rule that replaced it caps the floor at what the client actually asked for, so
/// the two targets always fit inside the requested count. That matters because the merge
/// concatenates local songs first and appends externals with no total cap: an external
/// past the client's songCount is the same as no external at all for any client that
/// renders only what it requested.
/// </summary>
public static class SearchBudget
{
    /// <summary>
    /// How many local rows to reserve before discovery gets a share. Capped at the
    /// requested count, so a client asking for fewer than this gets local-only results
    /// and no fan-out. That is what keeps per-keystroke type-ahead cheap: a search for
    /// five rows costs one relay, where generating discovery for it would cost a Last.fm
    /// fan-out plus a dozen Deezer enrichment rows and eight yt-dlp lookups.
    /// </summary>
    public const int LocalSongFloor = 12;

    /// <summary>
    /// Ceiling on discovery rows handed to one response. Tied to the number actually built
    /// per query so a caller can never be promised more rows than exist: the build size is
    /// a constant precisely so concurrent callers wanting different amounts can share one
    /// execution.
    /// </summary>
    public const int ExternalCeiling = Common.ExternalSearchService.BuildSize;

    /// <summary>
    /// Split <paramref name="requestedSongs"/> into a local target and an external target.
    /// Both are returned together so the two can never drift apart at a call site.
    /// </summary>
    /// <param name="requestedSongs">
    /// The client's songCount. Negative values are treated as zero; the previous
    /// expression sanitised those only by accident, through its flat floor.
    /// </param>
    /// <param name="discoveryEnabled">
    /// <see cref="Models.Settings.SubsonicSettings.EnableSearchDiscovery"/>. False sends the
    /// whole request to the local target and skips discovery entirely, which is what lets
    /// the call site skip its Last.fm/Deezer fan-out too instead of just discarding it.
    /// </param>
    /// <returns>
    /// Local and external targets. Their sum never exceeds <paramref name="requestedSongs"/>.
    /// </returns>
    public static (int Local, int External) Compute(int requestedSongs, bool discoveryEnabled = true)
    {
        var requested = Math.Max(0, requestedSongs);

        if (!discoveryEnabled)
        {
            return (requested, 0);
        }

        // Locals still scale with big requests through the quarter rule, which is what
        // keeps behaviour identical for the large counts radio-style clients send. The
        // floor only decides small requests, and capping it at the request is the fix.
        var local = Math.Max(Math.Min(LocalSongFloor, requested), requested / 4);

        var external = Math.Min(ExternalCeiling, Math.Max(0, requested - local));

        return (local, external);
    }
}
