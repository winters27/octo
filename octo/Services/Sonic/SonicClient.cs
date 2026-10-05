using System.Net.Http.Json;
using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;

namespace Octo.Services.Sonic;

/// <summary>How one song sounds: bliss's numbers, and the version of them.</summary>
public sealed record SonicFeatures(float[] Values, int Version);

/// <summary>octo-sonic is up and writes this feature version; <paramref name="Problem"/> when it
/// cannot see the music folder, so nothing it is sent could be read; <paramref name="Off"/> when
/// Sounds alike is turned off in .env (RADIO_SOUNDS_ALIKE=false), so it reads nothing on purpose.</summary>
public sealed record SonicHealth(int Version, string? Problem, bool Off = false);

/// <summary>What octo-sonic said of one song: its features, or why not, and the HTTP status
/// (403 and 404 are octo-sonic's view of the music folder, not the song).</summary>
public sealed record SonicAnswer(SonicFeatures? Features, string? Error, int Status);

/// <summary>The octo-sonic sidecar, which reads a song and says how it sounds.</summary>
public sealed class SonicClient(IHttpClientFactory http, IOptionsMonitor<RadioSourceSettings> settings,
    ILogger<SonicClient> logger)
{
    public const string ClientName = "octo-sonic";
    private string BaseUrl => settings.CurrentValue.SonicUrl.TrimEnd('/');

    /// <summary>The feature version octo-sonic writes and whether it sees the music, or null when it does not answer.</summary>
    public async Task<SonicHealth?> HealthAsync(CancellationToken ct)
    {
        try
        {
            using var timeout = CancellationTokenSource.CreateLinkedTokenSource(ct);
            timeout.CancelAfter(TimeSpan.FromSeconds(5));
            using var response = await http.CreateClient(ClientName).GetAsync(BaseUrl + "/health", timeout.Token);
            var body = await response.Content.ReadAsStringAsync(ct);
            using var doc = JsonDocument.Parse(body.Length > 0 ? body : "{}");
            if (!doc.RootElement.TryGetProperty("featuresVersion", out var v) || !v.TryGetInt32(out var version)) return null;
            if (response.IsSuccessStatusCode) return new SonicHealth(version, null);
            if (doc.RootElement.TryGetProperty("off", out var off) && off.ValueKind == JsonValueKind.True)
                return new SonicHealth(version, null, Off: true);
            return (int)response.StatusCode == 503
                ? new SonicHealth(version, doc.RootElement.TryGetProperty("error", out var e) && e.ValueKind == JsonValueKind.String
                    ? e.GetString() : "cannot see the music folder")
                : null;
        }
        catch (Exception ex) when (ex is not OperationCanceledException || !ct.IsCancellationRequested)
        {
            logger.LogDebug(ex, "octo-sonic health check failed");
            return null;
        }
    }

    /// <summary>The song's features, or an error to remember: octo-sonic could not read it.
    /// A dropped connection throws; an answer Octo cannot read is that song's error.</summary>
    public async Task<SonicAnswer> AnalyseAsync(string path, CancellationToken ct)
    {
        using var response = await http.CreateClient(ClientName).PostAsJsonAsync(BaseUrl + "/analyse", new { path }, ct);
        var status = (int)response.StatusCode;
        var body = await response.Content.ReadAsStringAsync(ct);
        try
        {
            using var doc = JsonDocument.Parse(body.Length > 0 ? body : "{}");
            if (!response.IsSuccessStatusCode)
                return new SonicAnswer(null, doc.RootElement.TryGetProperty("error", out var e) && e.ValueKind == JsonValueKind.String
                    ? e.GetString() : $"HTTP {status}", status);
            var values = doc.RootElement.GetProperty("features").EnumerateArray().Select(item => item.GetSingle()).ToArray();
            return new SonicAnswer(new SonicFeatures(values, doc.RootElement.GetProperty("version").GetInt32()), null, status);
        }
        catch (Exception ex) when (ex is JsonException or KeyNotFoundException or InvalidOperationException or FormatException)
        {
            logger.LogDebug(ex, "octo-sonic's answer for {Path} could not be read", path);
            return new SonicAnswer(null, $"octo-sonic answered something Octo could not read (HTTP {status})", status);
        }
    }
}
