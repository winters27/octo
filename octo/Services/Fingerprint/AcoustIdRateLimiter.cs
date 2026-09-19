using System.Threading.RateLimiting;

namespace Octo.Services.Fingerprint;

/// <summary>
/// Keeps Octo inside AcoustID's published budget of 3 requests per second.
///
/// The same trap as Deezer, and it bites harder here: over-budget is not reliably a 429,
/// it is an error envelope in a 200 body. That parses as "no match", and this feature reads
/// "no match" as "accept the file" - so exceeding the budget would silently turn
/// verification OFF rather than merely slow it down.
///
/// One lane, unlike Deezer. Every lookup here sits between a finished transfer and a file
/// joining the library, so there is no background work to yield to.
/// </summary>
public sealed class AcoustIdRateLimiter : IDisposable
{
    /// <summary>Named HttpClient that carries the limiting handler. A caller that resolves
    /// any other client bypasses the budget entirely.</summary>
    public const string ClientName = "acoustid";

    private const int PermitsPerSecond = 3;

    private readonly SlidingWindowRateLimiter _limiter = new(new SlidingWindowRateLimiterOptions
    {
        PermitLimit = PermitsPerSecond,
        Window = TimeSpan.FromSeconds(1),
        SegmentsPerWindow = 3,
        // Must be set explicitly: it defaults to 0, which makes AcquireAsync return a
        // NON-acquired lease immediately instead of waiting. A dropped lookup here is an
        // accepted file, so dropping is the one thing this must not do by default.
        // Bounded by the client's 10s timeout: 3/s drains 12 in four seconds.
        QueueLimit = 12,
        QueueProcessingOrder = QueueProcessingOrder.OldestFirst,
        AutoReplenishment = true,
    });

    public ValueTask<RateLimitLease> AcquireAsync(CancellationToken ct) => _limiter.AcquireAsync(1, ct);

    public void Dispose() => _limiter.Dispose();
}
