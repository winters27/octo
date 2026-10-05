using System.Net.Http.Json;
using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;

namespace Octo.Services.Sonic;

/// <summary>How one song sounds: bliss's numbers, and the version of them.</summary>
public sealed record SonicFeatures(float[] Values, int Version);

/// <summary>The octo-sonic sidecar, which reads a song and says how it sounds.</summary>
public sealed class SonicClient(IHttpClientFactory http, IOptionsMonitor<RadioSourceSettings> settings,
    ILogger<SonicClient> logger)
{
    public const string ClientName = "octo-sonic";
    private string BaseUrl => settings.CurrentValue.SonicUrl.TrimEnd('/');

    /// <summary>The feature version octo-sonic writes, or null when it does not answer.</summary>
    public async Task<int?> HealthAsync(CancellationToken ct)
    {
        try
        {
            using var timeout = CancellationTokenSource.CreateLinkedTokenSource(ct);
            timeout.CancelAfter(TimeSpan.FromSeconds(5));
            using var response = await http.CreateClient(ClientName).GetAsync(BaseUrl + "/health", timeout.Token);
            if (!response.IsSuccessStatusCode) return null;
            using var doc = JsonDocument.Parse(await response.Content.ReadAsStringAsync(ct));
            return doc.RootElement.TryGetProperty("featuresVersion", out var v) && v.TryGetInt32(out var version) ? version : null;
        }
        catch (Exception ex) when (ex is not OperationCanceledException || !ct.IsCancellationRequested)
        {
            logger.LogDebug(ex, "octo-sonic health check failed");
            return null;
        }
    }

    /// <summary>The song's features, or an error to remember: octo-sonic could not read it.</summary>
    public async Task<(SonicFeatures? Features, string? Error)> AnalyseAsync(string path, CancellationToken ct)
    {
        using var response = await http.CreateClient(ClientName).PostAsJsonAsync(BaseUrl + "/analyse", new { path }, ct);
        var body = await response.Content.ReadAsStringAsync(ct);
        using var doc = JsonDocument.Parse(body.Length > 0 ? body : "{}");
        if (!response.IsSuccessStatusCode)
            return (null, doc.RootElement.TryGetProperty("error", out var e) ? e.GetString() : $"HTTP {(int)response.StatusCode}");
        var values = doc.RootElement.GetProperty("features").EnumerateArray().Select(item => item.GetSingle()).ToArray();
        return (new SonicFeatures(values, doc.RootElement.GetProperty("version").GetInt32()), null);
    }
}
