using Microsoft.AspNetCore.Mvc;
using Microsoft.Extensions.Options;
using System.Text.Json;
using System.Text.Json.Nodes;
using Octo.Models.Settings;
using Octo.Services.Admin;
using Octo.Services.LastFm;
using Octo.Services.Soulseek;
using Octo.Services.Subsonic;

namespace Octo.Controllers;

/// <summary>
/// Admin API. Backs the in-app settings UI at /admin/.
///
/// Settings are persisted to the JSON file registered as the highest-priority
/// configuration source in Program.cs — once written, ASP.NET's reloadOnChange
/// watcher refreshes IOptions consumers automatically. Some settings (URLs,
/// HTTP client timeouts, things captured into singletons at startup) require
/// a process restart to fully take effect; the UI marks those clearly and
/// /api/admin/restart triggers a clean exit so docker-compose's restart
/// policy brings the container back up with new values.
/// </summary>
[ApiController]
[Route("api/admin")]
public class AdminController : ControllerBase
{
    private readonly SettingsFileWriter _settings;
    private readonly IOptionsMonitor<SubsonicSettings> _subsonicOpts;
    private readonly IOptionsMonitor<SoulseekSettings> _soulseekOpts;
    private readonly IOptionsMonitor<LastFmSettings> _lastFmOpts;
    private readonly IConfiguration _config;
    private readonly SoulseekClient _slskd;
    private readonly SubsonicProxyService _proxy;
    private readonly IHttpClientFactory _httpFactory;
    private readonly IHostApplicationLifetime _lifetime;
    private readonly ILogger<AdminController> _logger;

    public AdminController(
        SettingsFileWriter settings,
        IOptionsMonitor<SubsonicSettings> subsonicOpts,
        IOptionsMonitor<SoulseekSettings> soulseekOpts,
        IOptionsMonitor<LastFmSettings> lastFmOpts,
        IConfiguration config,
        SoulseekClient slskd,
        SubsonicProxyService proxy,
        IHttpClientFactory httpFactory,
        IHostApplicationLifetime lifetime,
        ILogger<AdminController> logger)
    {
        _settings = settings;
        _subsonicOpts = subsonicOpts;
        _soulseekOpts = soulseekOpts;
        _lastFmOpts = lastFmOpts;
        _config = config;
        _slskd = slskd;
        _proxy = proxy;
        _httpFactory = httpFactory;
        _lifetime = lifetime;
        _logger = logger;
    }

    /// <summary>
    /// Serve the admin SPA's index.html directly when the user hits /admin
    /// or /admin/ without a filename. This bypasses the awkward dance of
    /// UseDefaultFiles + UseStaticFiles in .NET 9 (where MapStaticAssets and
    /// the default-document middleware don't always cooperate); we just hand
    /// back the file by path.
    /// </summary>
    // Single attribute; ASP.NET normalizes the trailing slash so /admin and
    // /admin/ both match. (Adding both attributes triggers
    // AmbiguousMatchException since they end up registering the same endpoint
    // twice.)
    [HttpGet("/admin")]
    public IActionResult AdminRoot()
    {
        var path = Path.Combine(AppContext.BaseDirectory, "wwwroot", "admin", "index.html");
        if (!System.IO.File.Exists(path)) return NotFound(new { error = "admin UI not found in publish output" });
        return PhysicalFile(path, "text/html");
    }

    /// <summary>
    /// Returns the *effective* configuration the app sees right now, so the UI
    /// can show users the same values code is using regardless of whether they
    /// came from env var, appsettings.json, or the editable settings file.
    /// Sensitive keys are returned in clear because this admin endpoint is
    /// intended for trusted LAN-only access (matches Navidrome's admin pages).
    /// </summary>
    [HttpGet("settings")]
    public IActionResult GetSettings()
    {
        var subsonic = _subsonicOpts.CurrentValue;
        var soulseek = _soulseekOpts.CurrentValue;
        var lastfm = _lastFmOpts.CurrentValue;

        // Use Dictionary<string, object> so System.Text.Json doesn't camelCase
        // the keys. The admin UI's form fields are named "Subsonic.FolderStructure"
        // etc and look up settings by exact PascalCase key — a casing mismatch
        // here meant the form fields silently failed to pre-fill.
        var resp = new Dictionary<string, object>
        {
            ["Subsonic"] = new Dictionary<string, object>
            {
                ["Url"] = subsonic.Url ?? "",
                ["StorageMode"] = subsonic.StorageMode.ToString(),
                ["DownloadMode"] = subsonic.DownloadMode.ToString(),
                ["DownloadOnStar"] = subsonic.DownloadOnStar,
                ["FolderStructure"] = subsonic.FolderStructure.ToString(),
                ["UseLocalStaging"] = subsonic.UseLocalStaging,
                ["ExplicitFilter"] = subsonic.ExplicitFilter.ToString(),
                ["CacheDurationHours"] = subsonic.CacheDurationHours,
                ["EnableExternalPlaylists"] = subsonic.EnableExternalPlaylists,
                ["PlaylistsDirectory"] = subsonic.PlaylistsDirectory,
            },
            ["Library"] = new Dictionary<string, object>
            {
                ["DownloadPath"] = _config["Library:DownloadPath"] ?? "/music",
            },
            ["Soulseek"] = new Dictionary<string, object>
            {
                ["BaseUrl"] = soulseek.BaseUrl ?? "",
                ["Username"] = soulseek.Username ?? "",
                ["Password"] = soulseek.Password ?? "",
                ["SearchWaitSeconds"] = soulseek.SearchWaitSeconds,
                ["MinFileSizeBytes"] = soulseek.MinFileSizeBytes,
                ["PreferredExtension"] = soulseek.PreferredExtension,
                ["DownloadTimeoutSeconds"] = soulseek.DownloadTimeoutSeconds,
            },
            ["YouTube"] = new Dictionary<string, object>
            {
                ["ShimUrl"] = _config["YouTube:ShimUrl"] ?? "",
            },
            ["LastFm"] = new Dictionary<string, object>
            {
                ["ApiKey"] = lastfm.ApiKey ?? "",
                ["EnableRadio"] = lastfm.EnableRadio,
                ["RadioTrackCount"] = lastfm.RadioTrackCount,
                ["RadioCacheDurationHours"] = lastfm.RadioCacheDurationHours,
            },
            ["_meta"] = new Dictionary<string, object>
            {
                ["ConfigFilePath"] = _settings.FilePath,
                ["ConfigFileExists"] = System.IO.File.Exists(_settings.FilePath),
            }
        };
        return new JsonResult(resp);
    }

    /// <summary>
    /// Writes a partial settings patch to the JSON file. Body shape matches
    /// the GET response; any subset of keys may be supplied. reloadOnChange
    /// picks up the file write within ~500ms, so IOptionsMonitor.CurrentValue
    /// reflects the new config on the next caller — but consumers that captured
    /// IOptions.Value at startup keep their old values until a process restart.
    /// </summary>
    [HttpPost("settings")]
    public async Task<IActionResult> SaveSettings()
    {
        using var reader = new StreamReader(Request.Body);
        var body = await reader.ReadToEndAsync();
        if (string.IsNullOrWhiteSpace(body))
            return BadRequest(new { error = "empty body" });

        JsonObject patch;
        try
        {
            patch = JsonNode.Parse(body) as JsonObject
                ?? throw new InvalidOperationException("expected object");
        }
        catch (Exception ex)
        {
            return BadRequest(new { error = $"invalid JSON: {ex.Message}" });
        }

        // Strip any meta-only keys the UI might echo back so they don't end
        // up persisted to disk.
        patch.Remove("_meta");

        try
        {
            var merged = _settings.Merge(patch);
            _logger.LogInformation("Admin settings updated: {Keys}",
                string.Join(",", patch.Select(kv => kv.Key)));
            return new JsonResult(new { ok = true, persisted = merged });
        }
        catch (Exception ex)
        {
            _logger.LogError(ex, "Failed to persist settings to {Path}", _settings.FilePath);
            return StatusCode(500, new { error = ex.Message });
        }
    }

    /// <summary>
    /// Returns the raw settings.json file contents (or "{}" if it doesn't yet
    /// exist). Used by the Raw Config tab so power users can edit the file
    /// directly without going through the form-by-form UI. Read-write
    /// counterpart is PUT /api/admin/raw-config.
    /// </summary>
    [HttpGet("raw-config")]
    public IActionResult GetRawConfig()
    {
        var json = _settings.Load().ToJsonString(new JsonSerializerOptions { WriteIndented = true });
        return Content(json, "application/json");
    }

    /// <summary>
    /// Replaces the settings.json file wholesale with the request body.
    /// Validates that the body parses as a JSON object before writing —
    /// otherwise we'd let the user save a broken file that crashes the next
    /// container restart.
    /// </summary>
    [HttpPut("raw-config")]
    public async Task<IActionResult> PutRawConfig()
    {
        using var reader = new StreamReader(Request.Body);
        var body = await reader.ReadToEndAsync();
        if (string.IsNullOrWhiteSpace(body))
            return BadRequest(new { error = "empty body" });

        JsonObject parsed;
        try
        {
            parsed = JsonNode.Parse(body) as JsonObject
                ?? throw new InvalidOperationException("top level must be an object");
        }
        catch (Exception ex)
        {
            return BadRequest(new { error = $"invalid JSON: {ex.Message}" });
        }

        try
        {
            // Atomic write via tmp + rename. Don't merge — this is the "I
            // know exactly what I want" power-user endpoint.
            var path = _settings.FilePath;
            Directory.CreateDirectory(Path.GetDirectoryName(path) ?? ".");
            var tmp = path + ".tmp";
            var pretty = parsed.ToJsonString(new JsonSerializerOptions { WriteIndented = true });
            System.IO.File.WriteAllText(tmp, pretty);
            System.IO.File.Move(tmp, path, overwrite: true);
            _logger.LogInformation("Admin raw-config saved ({Bytes} bytes)", pretty.Length);
            return new JsonResult(new { ok = true, bytes = pretty.Length });
        }
        catch (Exception ex)
        {
            _logger.LogError(ex, "Failed to write raw config to {Path}", _settings.FilePath);
            return StatusCode(500, new { error = ex.Message });
        }
    }

    /// <summary>
    /// Snapshot of every config key the app effectively sees, with the source
    /// of each value (env var vs settings.json vs appsettings.json default).
    /// Helps users debug why a value isn't what they expect — most often,
    /// because env wins over the file or vice versa.
    /// </summary>
    [HttpGet("config-sources")]
    public IActionResult GetConfigSources()
    {
        // Walk the IConfigurationRoot's providers in reverse order (highest
        // priority first) so the user can see which provider supplied each
        // effective value. .NET's IConfigurationRoot.GetDebugView would do
        // this for us but emits unstructured text; this gives the UI structured
        // data it can render as a table.
        var keys = new[]
        {
            "Subsonic:Url", "Subsonic:StorageMode", "Subsonic:DownloadMode",
            "Subsonic:DownloadOnStar", "Subsonic:FolderStructure",
            "Subsonic:UseLocalStaging", "Subsonic:ExplicitFilter",
            "Subsonic:CacheDurationHours", "Subsonic:EnableExternalPlaylists",
            "Subsonic:PlaylistsDirectory",
            "Library:DownloadPath",
            "Soulseek:BaseUrl", "Soulseek:Username", "Soulseek:Password",
            "Soulseek:SearchWaitSeconds", "Soulseek:MinFileSizeBytes",
            "Soulseek:PreferredExtension", "Soulseek:DownloadTimeoutSeconds",
            "YouTube:ShimUrl",
            "LastFm:ApiKey", "LastFm:EnableRadio", "LastFm:RadioTrackCount",
            "LastFm:RadioCacheDurationHours",
        };
        var rows = new List<object>();
        foreach (var k in keys)
        {
            var v = _config[k] ?? "";
            // Mask anything that smells like a secret so a screenshot of the
            // page doesn't leak credentials.
            var isSecret = k.EndsWith("Password", StringComparison.OrdinalIgnoreCase)
                        || k.EndsWith("ApiKey", StringComparison.OrdinalIgnoreCase);
            var display = isSecret && !string.IsNullOrEmpty(v)
                ? new string('•', Math.Min(v.Length, 16))
                : v;
            rows.Add(new Dictionary<string, object>
            {
                ["Key"] = k,
                ["Value"] = display,
                ["IsSecret"] = isSecret,
            });
        }
        return new JsonResult(new { keys = rows, configFile = _settings.FilePath });
    }

    /// <summary>
    /// Quick health snapshot for each backing service. The UI shows a status
    /// dot per service; the user can tell at a glance whether Octo can reach
    /// Navidrome, slskd, the yt-dlp shim, and Last.fm.
    /// </summary>
    [HttpGet("status")]
    public async Task<IActionResult> GetStatus(CancellationToken ct)
    {
        // Probe each in parallel — total time is the slowest probe, not sum.
        var probeTasks = new Dictionary<string, Task<ServiceProbe>>
        {
            ["navidrome"] = ProbeNavidromeAsync(ct),
            ["slskd"] = ProbeSlskdAsync(ct),
            ["ytDlpShim"] = ProbeYouTubeShimAsync(ct),
            ["lastfm"] = ProbeLastFmAsync(ct),
        };
        await Task.WhenAll(probeTasks.Values);

        var results = probeTasks.ToDictionary(
            kv => kv.Key,
            kv => kv.Value.Result);

        return new JsonResult(new
        {
            octo = new ServiceProbe(true, "Octo is responding"),
            services = results,
            time = DateTimeOffset.UtcNow.ToString("O"),
        });
    }

    /// <summary>
    /// Exit the process with code 1 so docker compose's restart policy brings
    /// the container back up with refreshed config. The caller gets an empty
    /// 202 before the shutdown actually fires.
    /// </summary>
    [HttpPost("restart")]
    public IActionResult Restart()
    {
        _logger.LogWarning("Admin requested restart; container will exit in 1s");
        // Fire-and-forget so the response can be returned first.
        _ = Task.Run(async () =>
        {
            await Task.Delay(1000);
            _lifetime.StopApplication();
            await Task.Delay(2000);
            // Belt and braces: if graceful stop hasn't completed in 2s, hard exit
            // so docker-compose treats it as a crash and restarts.
            Environment.Exit(1);
        });
        return Accepted(new { ok = true, message = "restarting" });
    }

    private async Task<ServiceProbe> ProbeNavidromeAsync(CancellationToken ct)
    {
        try
        {
            var url = _subsonicOpts.CurrentValue.Url;
            if (string.IsNullOrWhiteSpace(url))
                return new ServiceProbe(false, "Subsonic URL not configured");
            // Navidrome's /rest/ping requires auth, but it returns 200 with an
            // error body even on bad credentials — which proves connectivity.
            var http = _httpFactory.CreateClient();
            http.Timeout = TimeSpan.FromSeconds(5);
            using var resp = await http.GetAsync($"{url.TrimEnd('/')}/rest/ping?u=probe&p=probe&v=1.16.1&c=octo&f=json", ct);
            return new ServiceProbe(resp.IsSuccessStatusCode, $"HTTP {(int)resp.StatusCode} from {url}");
        }
        catch (Exception ex) { return new ServiceProbe(false, ex.Message); }
    }

    private async Task<ServiceProbe> ProbeSlskdAsync(CancellationToken ct)
    {
        try
        {
            var ok = await _slskd.IsReachableAsync(ct);
            return new ServiceProbe(ok, ok ? "reachable" : "unreachable / auth failed");
        }
        catch (Exception ex) { return new ServiceProbe(false, ex.Message); }
    }

    private async Task<ServiceProbe> ProbeYouTubeShimAsync(CancellationToken ct)
    {
        try
        {
            var shimUrl = (_config["YouTube:ShimUrl"] ?? "http://yt-dlp-shim:8080").TrimEnd('/');
            var http = _httpFactory.CreateClient();
            http.Timeout = TimeSpan.FromSeconds(5);
            using var resp = await http.GetAsync($"{shimUrl}/health", ct);
            return new ServiceProbe(resp.IsSuccessStatusCode, $"HTTP {(int)resp.StatusCode}");
        }
        catch (Exception ex) { return new ServiceProbe(false, ex.Message); }
    }

    private async Task<ServiceProbe> ProbeLastFmAsync(CancellationToken ct)
    {
        var key = _lastFmOpts.CurrentValue.ApiKey;
        if (string.IsNullOrEmpty(key))
            return new ServiceProbe(false, "API key not set");
        try
        {
            var http = _httpFactory.CreateClient();
            http.Timeout = TimeSpan.FromSeconds(5);
            // auth.getSession with a bad token returns code 4 — proves the key
            // dispatch works without consuming a real auth slot.
            using var resp = await http.GetAsync($"https://ws.audioscrobbler.com/2.0/?method=track.getInfo&artist=cher&track=believe&api_key={key}&format=json", ct);
            return new ServiceProbe(resp.IsSuccessStatusCode, $"HTTP {(int)resp.StatusCode}");
        }
        catch (Exception ex) { return new ServiceProbe(false, ex.Message); }
    }

    private record ServiceProbe(bool Ok, string Detail);
}
