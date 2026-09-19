using System.Net;

namespace Octo.Services.Fingerprint;

/// <summary>
/// Spends an <see cref="AcoustIdRateLimiter"/> permit before each AcoustID call.
/// Transient by design: IHttpClientFactory recycles handler chains, so the limiter itself
/// is the singleton and this is just the seam that consults it.
/// </summary>
public sealed class AcoustIdRateLimitHandler : DelegatingHandler
{
    private const string ApiHost = "api.acoustid.org";

    private readonly AcoustIdRateLimiter _limiter;
    private readonly ILogger<AcoustIdRateLimitHandler> _logger;

    public AcoustIdRateLimitHandler(AcoustIdRateLimiter limiter, ILogger<AcoustIdRateLimitHandler> logger)
    {
        _limiter = limiter;
        _logger = logger;
    }

    protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
    {
        if (!string.Equals(request.RequestUri?.Host, ApiHost, StringComparison.OrdinalIgnoreCase))
            return await base.SendAsync(request, ct);

        using var lease = await _limiter.AcquireAsync(ct);
        if (!lease.IsAcquired)
        {
            // Answering 429 rather than throwing means the caller takes its ordinary
            // "AcoustID had no verdict" path, which keeps the file and remembers nothing.
            // Back-pressure can make verification less effective; it must never make it
            // reject a good download.
            _logger.LogWarning("acoustid rate limiter rejected a request to {Url}", request.RequestUri);
            return new HttpResponseMessage(HttpStatusCode.TooManyRequests) { RequestMessage = request };
        }

        return await base.SendAsync(request, ct);
    }
}
