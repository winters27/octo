using Microsoft.AspNetCore.Mvc;
using Microsoft.Extensions.Options;
using System.Text.Json;
using System.Text.Json.Nodes;
using Octo.Models.Settings;
using Octo.Services.Admin;
using Octo.Services.LastFm;
using Octo.Services.Lidarr;
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
    private readonly IOptionsMonitor<LidarrSettings> _lidarrOpts;
    private readonly IOptionsMonitor<LastFmSettings> _lastFmOpts;
    private readonly IOptionsMonitor<NotificationSettings> _notificationOpts;
    private readonly IOptionsMonitor<MetadataSettings> _metadataOpts;
    private readonly Octo.Services.Notifications.NotificationService _notifications;
    private readonly IConfiguration _config;
    private readonly SoulseekClient _slskd;
    private readonly LidarrClient _lidarr;
    private readonly SubsonicProxyService _proxy;
    private readonly SubsonicDiscoveryService _discovery;
    private readonly NavidromeIdentityService _navIdentity;
    private readonly DirectoryBrowser _browser;
    private readonly BrowseSessionStore _browseSessions;
    private readonly Octo.Services.Local.DownloadHistoryService _history;
    private readonly Octo.Services.Metadata.DeezerMetadataService _deezer;
    private readonly Octo.Services.CoverArt.CoverArtAggregator _coverArt;
    private readonly IHttpClientFactory _httpFactory;
    private readonly IHostApplicationLifetime _lifetime;
    private readonly ILogger<AdminController> _logger;
    private readonly LastFmRadioStateStore? _radioState;
    private readonly LastFmRadioRefreshQueue? _radioRefresh;

    public AdminController(
        SettingsFileWriter settings,
        IOptionsMonitor<SubsonicSettings> subsonicOpts,
        IOptionsMonitor<SoulseekSettings> soulseekOpts,
        IOptionsMonitor<LidarrSettings> lidarrOpts,
        IOptionsMonitor<LastFmSettings> lastFmOpts,
        IOptionsMonitor<NotificationSettings> notificationOpts,
        IOptionsMonitor<MetadataSettings> metadataOpts,
        Octo.Services.Notifications.NotificationService notifications,
        IConfiguration config,
        SoulseekClient slskd,
        LidarrClient lidarr,
        SubsonicProxyService proxy,
        SubsonicDiscoveryService discovery,
        NavidromeIdentityService navIdentity,
        DirectoryBrowser browser,
        BrowseSessionStore browseSessions,
        Octo.Services.Local.DownloadHistoryService history,
        Octo.Services.Metadata.DeezerMetadataService deezer,
        Octo.Services.CoverArt.CoverArtAggregator coverArt,
        IHttpClientFactory httpFactory,
        IHostApplicationLifetime lifetime,
        ILogger<AdminController> logger,
        LastFmRadioStateStore? radioState = null,
        LastFmRadioRefreshQueue? radioRefresh = null)
    {
        _deezer = deezer;
        _coverArt = coverArt;
        _settings = settings;
        _subsonicOpts = subsonicOpts;
        _soulseekOpts = soulseekOpts;
        _lidarrOpts = lidarrOpts;
        _lastFmOpts = lastFmOpts;
        _notificationOpts = notificationOpts;
        _metadataOpts = metadataOpts;
        _notifications = notifications;
        _config = config;
        _slskd = slskd;
        _lidarr = lidarr;
        _proxy = proxy;
        _discovery = discovery;
        _navIdentity = navIdentity;
        _browser = browser;
        _browseSessions = browseSessions;
        _history = history;
        _httpFactory = httpFactory;
        _lifetime = lifetime;
        _logger = logger;
        _radioState = radioState;
        _radioRefresh = radioRefresh;
    }

    [HttpGet("lastfm/radio")]
    public IActionResult GetLastFmRadio([FromQuery] string? user = null)
    {
        if (_radioState is null) return Ok(new { users = Array.Empty<object>(), stations = Array.Empty<object>() });
        var summaries = _radioState.GetSummaries();
        var selected = string.IsNullOrWhiteSpace(user) ? summaries.FirstOrDefault()?.Username : user.Trim();
        var state = selected is null ? null : _radioState.GetUser(selected);
        var settings = _lastFmOpts.CurrentValue;
        return Ok(new
        {
            enabled = settings.EnableRadio,
            hasApiKey = !string.IsNullOrWhiteSpace(settings.ApiKey),
            personalizedEnabled = settings.EnablePersonalizedStations,
            discoveryEnabled = settings.EnableDiscoveryStations,
            playlistsEnabled = settings.ExposeRadioAsPlaylists,
            streamsEnabled = settings.ExposeRadioAsStreams,
            streamBitrateKbps = settings.EffectiveRadioStreamBitrateKbps,
            icyMetadataEnabled = settings.EnableIcyMetadata,
            minimumPlays = settings.EffectiveMinimumPlays,
            selectedUser = selected,
            users = summaries,
            learning = state is null ? null : new
            {
                plays = state.Plays.Count(play => play.LearnedSignal),
                needed = Math.Max(0, settings.EffectiveMinimumPlays - state.Plays.Count(play => play.LearnedSignal)),
                source = state.Plays.Any(play => play.LearnedSignal) ? "completed scrobbles and accessible stars" :
                    state.Plays.Count > 0 ? "accessible random Starter seeds" : "waiting for completed scrobbles",
                state.Refreshing, state.LastRefreshAttemptUtc, state.LastRefreshSuccessUtc,
                state.LastRefreshError
            },
            stations = state?.Stations.Select(station => new
            {
                station.Id, station.Name, kind = station.Kind.ToString(), station.Personalized,
                trackCount = station.Tracks.Count, station.Seeds, station.CreatedUtc,
                station.ChangedUtc, station.ValidUntilUtc,
                preview = station.Tracks.Take(5).Select(track => new { track.Artist, track.Title })
            }) ?? []
        });
    }

    [HttpPost("lastfm/radio/refresh")]
    public IActionResult RefreshLastFmRadio([FromBody] RadioUserRequest request)
    {
        if (_radioRefresh is null || string.IsNullOrWhiteSpace(request.User))
            return BadRequest(new { error = "A known Navidrome user is required" });
        var queued = _radioRefresh.Enqueue(request.User, request.StationId);
        return Accepted(new { ok = true, queued });
    }

    [HttpDelete("lastfm/radio/history")]
    public IActionResult ResetLastFmRadio([FromQuery] string user)
    {
        if (_radioState is null || string.IsNullOrWhiteSpace(user))
            return BadRequest(new { error = "A known Navidrome user is required" });
        var before = _radioState.GetUser(user);
        var removed = _radioState.Reset(user);
        return Ok(new
        {
            ok = removed, user, removedPlays = removed ? before.Plays.Count : 0,
            removedStations = removed ? before.Stations.Count : 0,
            message = "Radio history and generated snapshots were removed. Downloaded music was untouched."
        });
    }

    public sealed class RadioUserRequest { public string User { get; set; } = string.Empty; public string? StationId { get; set; } }

    /// <summary>
    /// Scans the local network for Subsonic/Navidrome servers so the setup UI can
    /// offer a detected upstream URL instead of requiring it typed by hand. Returns
    /// the found servers with their type/version. Empty when Octo can't see the LAN
    /// (e.g. a Docker bridge network).
    /// </summary>
    [HttpGet("discover-servers")]
    public async Task<IActionResult> DiscoverServers(CancellationToken ct)
    {
        var servers = await _discovery.ScanAsync(ct);
        return Ok(new { servers });
    }

    /// <summary>
    /// Where downloads will actually land, and why. Octo fronts Navidrome, so the
    /// library Navidrome scans is the source of truth and the default; this states
    /// the whole chain (what Navidrome reports, whether Octo can see it, what is
    /// therefore in effect) so "my downloads went nowhere" is answerable from the
    /// UI instead of the container logs.
    /// </summary>
    [HttpGet("library-status")]
    public async Task<IActionResult> LibraryStatus(CancellationToken ct)
    {
        var subsonic = _subsonicOpts.CurrentValue;
        var configured = _config["Library:DownloadPath"] ?? "";

        // Cheap when already detected: this is TTL-cached inside the service.
        await _navIdentity.DetectMusicFolderAsync(ct: ct);

        var reported = _navIdentity.DetectedMusicFolder;
        var effective = _navIdentity.EffectiveDownloadPath(configured);
        var libraries = _navIdentity.KnownLibraries
            .Select(l => new { l.Id, l.Name, l.Folder, visible = Directory.Exists(l.Folder) })
            .ToList();

        return Ok(new
        {
            autoDetect = subsonic.AutoDetectDownloadPath,
            pinnedLibraryPath = subsonic.LibraryPath ?? "",
            navidromeReports = reported,
            // Navidrome describes paths as IT sees them. Whether Octo can see the
            // same path is the difference between downloads being scanned and
            // vanishing, so it is stated rather than implied.
            visibleToOcto = !string.IsNullOrEmpty(reported) && Directory.Exists(reported),
            configuredFallback = configured,
            effectiveDownloadPath = effective,
            writable = !string.IsNullOrEmpty(effective) && Directory.Exists(effective)
                       && DirectoryBrowser.IsWritable(effective),
            rescanAuthenticated = _navIdentity.GetScanAuth() != null,
            libraries,
        });
    }

    /// <summary>
    /// Exchange Navidrome admin credentials for a short-lived browse token.
    ///
    /// Credentials arrive in the body, never the query string, so they cannot end up
    /// in access logs or a referrer. Verification is delegated to the Navidrome Octo
    /// already fronts: no new credential store, and admin rights are Navidrome's call
    /// rather than something Octo asserts for itself.
    /// </summary>
    [HttpPost("browse/auth")]
    public async Task<IActionResult> BrowseAuth([FromBody] BrowseAuthRequest req, CancellationToken ct)
    {
        var url = _subsonicOpts.CurrentValue.Url?.TrimEnd('/');
        if (string.IsNullOrEmpty(url))
            return StatusCode(503, new { error = "Navidrome URL is not configured yet." });
        if (req is null || string.IsNullOrWhiteSpace(req.Username) || string.IsNullOrWhiteSpace(req.Password))
            return Unauthorized(new { error = "Username and password are required." });

        try
        {
            var http = _httpFactory.CreateClient();
            var payload = JsonSerializer.Serialize(new { username = req.Username, password = req.Password });
            using var content = new StringContent(payload, System.Text.Encoding.UTF8, "application/json");
            using var resp = await http.PostAsync($"{url}/auth/login", content, ct);
            if (!resp.IsSuccessStatusCode)
                return Unauthorized(new { error = "Navidrome rejected those credentials." });

            using var doc = JsonDocument.Parse(await resp.Content.ReadAsStringAsync(ct));
            var isAdmin = doc.RootElement.TryGetProperty("isAdmin", out var adminEl)
                          && adminEl.ValueKind == JsonValueKind.True;
            if (!isAdmin)
                return Unauthorized(new { error = "That account is not a Navidrome admin." });

            _logger.LogInformation("Browse session opened for Navidrome admin {User}", req.Username);
            var token = _browseSessions.Create(req.Username);

            // Hand the session back as an HttpOnly cookie rather than something the
            // page has to hold. It survives a reload, so the user is not asked to
            // sign in again every time they come back to the settings, and script
            // on the page cannot read it even if something managed to inject some.
            // Secure only over HTTPS, since this is normally reached over plain HTTP
            // on a LAN and a Secure cookie would simply be dropped there.
            Response.Cookies.Append(BrowseCookieName, token, new CookieOptions
            {
                HttpOnly = true,
                SameSite = SameSiteMode.Strict,
                Secure = Request.IsHttps,
                Path = "/api/admin",
                MaxAge = BrowseSessionStore.Ttl,
            });
            return Ok(new { ok = true });
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Browse auth against Navidrome failed: {Msg}", ex.Message);
            return StatusCode(502, new { error = "Could not reach Navidrome to verify credentials." });
        }
    }

    /// <summary>
    /// List directories so the download folder can be picked rather than typed.
    /// Requires a token from browse/auth; a 401 discloses nothing about the
    /// filesystem, not even whether a path exists.
    /// </summary>
    [HttpGet("browse")]
    public IActionResult Browse([FromQuery] string? path, [FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        // Cookie first (how the admin UI authenticates), header second so the
        // endpoint stays usable from curl or a script without one.
        var session = Request.Cookies[BrowseCookieName] ?? token;
        if (!_browseSessions.Validate(session))
            return Unauthorized(new { error = "Browse session required." });

        var result = _browser.Browse(path);
        return Ok(new
        {
            result.Path,
            result.Parent,
            result.Separator,
            result.Writable,
            result.Exists,
            result.Entries,
            result.Truncated,
            result.AudioFiles,
            // Under Docker this is the container's mount namespace, not the host's
            // drives. Saying so in the payload keeps the UI honest about why a
            // user's D: drive is nowhere to be seen.
            containerised = !OperatingSystem.IsWindows() && Directory.Exists("/.dockerenv"),
        });
    }

    /// <summary>
    /// Sends a test notification through every configured sink so URLs and tokens can
    /// be verified without waiting for a real download. Reports per-sink outcome,
    /// including the transport's real error text on failure.
    /// </summary>
    [HttpPost("test-notification")]
    public async Task<IActionResult> TestNotification(CancellationToken ct)
    {
        var results = await _notifications.SendTestAsync(ct);
        return Ok(new { results });
    }

    /// <summary>Credentials for <see cref="BrowseAuth"/>. Body-only by design.</summary>
    public record BrowseAuthRequest(string? Username, string? Password);

    /// <summary>Cookie carrying the browse session. Scoped to /api/admin so it is
    /// never sent with the Subsonic traffic Octo proxies.</summary>
    private const string BrowseCookieName = "octo_browse";

    /// <summary>The running log of songs Octo has fetched, newest first.</summary>
    [HttpGet("downloads")]
    public IActionResult Downloads()
    {
        return Ok(new { downloads = _history.GetRecent(200) });
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
        return Redirect("/admin/index.html");
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
        var lidarr = _lidarrOpts.CurrentValue;
        var lastfm = _lastFmOpts.CurrentValue;
        var notif = _notificationOpts.CurrentValue;

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
                ["DownloadAlbumOnStar"] = subsonic.DownloadAlbumOnStar,
                ["WaitForLosslessOnPlay"] = subsonic.WaitForLosslessOnPlay,
                ["LosslessWaitTimeoutSeconds"] = subsonic.LosslessWaitTimeoutSeconds,
                // These two are rendered by the dashboard but were missing here, so their
                // fields never pre-filled with the saved value.
                ["DownloadSource"] = subsonic.DownloadSource.ToString(),
                ["HeartDownloadSources"] = subsonic.EffectiveHeartDownloadSources()
                    .Select(step => new Dictionary<string, object>
                    {
                        ["Source"] = step.Source.ToString(),
                        ["SongEnabled"] = step.SongEnabled == true,
                        ["AlbumEnabled"] = step.AlbumEnabled == true,
                    }).ToList(),
                ["AutoDetectDownloadPath"] = subsonic.AutoDetectDownloadPath,
                ["LibraryPath"] = subsonic.LibraryPath,
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
            ["Lidarr"] = new Dictionary<string, object>
            {
                ["BaseUrl"] = lidarr.BaseUrl ?? "",
                ["ApiKey"] = lidarr.ApiKey ?? "",
                ["RootFolderPath"] = lidarr.RootFolderPath ?? "",
                ["QualityProfileId"] = lidarr.QualityProfileId,
                ["MetadataProfileId"] = lidarr.MetadataProfileId,
                ["CompletionMode"] = lidarr.CompletionMode.ToString(),
                ["ImportTimeoutSeconds"] = lidarr.ImportTimeoutSeconds,
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
                ["EnablePersonalizedStations"] = lastfm.EnablePersonalizedStations,
                ["EnableDiscoveryStations"] = lastfm.EnableDiscoveryStations,
                ["ExposeRadioAsPlaylists"] = lastfm.ExposeRadioAsPlaylists,
                ["ExposeRadioAsStreams"] = lastfm.ExposeRadioAsStreams,
                ["RadioStreamBitrateKbps"] = lastfm.RadioStreamBitrateKbps,
                ["EnableIcyMetadata"] = lastfm.EnableIcyMetadata,
                ["HistoryRetentionDays"] = lastfm.HistoryRetentionDays,
                ["DiscoveryPercent"] = lastfm.DiscoveryPercent,
                ["RefreshIntervalHours"] = lastfm.RefreshIntervalHours,
                ["MinimumPlays"] = lastfm.MinimumPlays,
                ["DiscoveryStations"] = (_settings.Load()["LastFm"] as JsonObject)?["DiscoveryStations"]?.DeepClone()
                    ?? JsonSerializer.SerializeToNode(lastfm.DiscoveryStations)!,
            },
            ["Metadata"] = new Dictionary<string, object>
            {
                ["Language"] = _metadataOpts.CurrentValue.Language ?? "",
            },
            ["Notifications"] = new Dictionary<string, object>
            {
                ["NtfyUrl"] = notif.NtfyUrl ?? "",
                ["NtfyToken"] = notif.NtfyToken ?? "",
                ["DiscordWebhookUrl"] = notif.DiscordWebhookUrl ?? "",
                ["NotifyDownloadStarted"] = notif.NotifyDownloadStarted,
                ["NotifyDownloadCompleted"] = notif.NotifyDownloadCompleted,
                ["NotifyLosslessFallback"] = notif.NotifyLosslessFallback,
                ["NotifyDownloadFailed"] = notif.NotifyDownloadFailed,
                ["NotifyAlbumCompleted"] = notif.NotifyAlbumCompleted,
            },
            ["_meta"] = new Dictionary<string, object>
            {
                ["ConfigFilePath"] = _settings.FilePath,
                ["ConfigFileExists"] = System.IO.File.Exists(_settings.FilePath),
                // So a bug report can name a build. Comes from <InformationalVersion>
                // in octo.csproj, which is bumped when a release is tagged.
                ["Version"] = OctoVersion,
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

        if (patch["LastFm"] is JsonObject lastFmPatch
            && lastFmPatch["DiscoveryStations"] is JsonArray discovery)
        {
            var validationError = ValidateDiscoveryStations(discovery);
            if (validationError is not null) return BadRequest(new { error = validationError });
        }

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

    private static string? ValidateDiscoveryStations(JsonArray stations)
    {
        if (stations.Count > 12) return "LastFm.DiscoveryStations supports at most 12 entries";
        var ids = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        var names = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        foreach (var node in stations)
        {
            if (node is not JsonObject station) return "Every discovery station must be an object";
            var id = station["Id"]?.GetValue<string>()?.Trim() ?? "";
            var name = station["Name"]?.GetValue<string>()?.Trim() ?? "";
            var tags = station["Tags"] as JsonArray;
            if (id.Length == 0 || !ids.Add(id)) return "Discovery station IDs must be present and unique";
            if (name.Length is 0 or > 100 || !names.Add(name)) return "Discovery station names must be present, unique, and at most 100 characters";
            if (tags is null || tags.Count is 0 or > 5
                || tags.Any(tag => string.IsNullOrWhiteSpace(tag?.GetValue<string>())))
                return $"{name} must contain between one and five non-empty tags";
        }
        return null;
    }

    /// <summary>
    /// Returns the *effective* configuration as a JSON document, with values
    /// pulled from the live IOptionsMonitor (so what the app actually sees).
    /// Anything explicitly persisted to settings.json sits on top of env vars
    /// and appsettings.json defaults; that merged result is what we return so
    /// the Raw Config editor shows the full picture rather than only the
    /// sparse overrides file.
    ///
    /// On PUT, the entire body is written wholesale to settings.json, which
    /// is a no-op if the user just hits Save without editing (values match
    /// env) and a real override otherwise.
    /// </summary>
    [HttpGet("raw-config")]
    public IActionResult GetRawConfig()
    {
        var subsonic = _subsonicOpts.CurrentValue;
        var soulseek = _soulseekOpts.CurrentValue;
        var lidarr = _lidarrOpts.CurrentValue;
        var lastfm = _lastFmOpts.CurrentValue;
        var notif = _notificationOpts.CurrentValue;

        var effective = new JsonObject
        {
            ["Subsonic"] = new JsonObject
            {
                ["Url"] = subsonic.Url ?? "",
                ["StorageMode"] = subsonic.StorageMode.ToString(),
                ["DownloadMode"] = subsonic.DownloadMode.ToString(),
                ["DownloadOnStar"] = subsonic.DownloadOnStar,
                ["DownloadAlbumOnStar"] = subsonic.DownloadAlbumOnStar,
                ["WaitForLosslessOnPlay"] = subsonic.WaitForLosslessOnPlay,
                ["LosslessWaitTimeoutSeconds"] = subsonic.LosslessWaitTimeoutSeconds,
                ["DownloadSource"] = subsonic.DownloadSource.ToString(),
                ["HeartDownloadSources"] = new JsonArray(
                    subsonic.EffectiveHeartDownloadSources()
                        .Select(step => (JsonNode)new JsonObject
                        {
                            ["Source"] = step.Source.ToString(),
                            ["SongEnabled"] = step.SongEnabled == true,
                            ["AlbumEnabled"] = step.AlbumEnabled == true,
                        }).ToArray()),
                ["AutoDetectDownloadPath"] = subsonic.AutoDetectDownloadPath,
                ["LibraryPath"] = subsonic.LibraryPath,
                ["FolderStructure"] = subsonic.FolderStructure.ToString(),
                ["UseLocalStaging"] = subsonic.UseLocalStaging,
                ["ExplicitFilter"] = subsonic.ExplicitFilter.ToString(),
                ["CacheDurationHours"] = subsonic.CacheDurationHours,
                ["EnableExternalPlaylists"] = subsonic.EnableExternalPlaylists,
                ["PlaylistsDirectory"] = subsonic.PlaylistsDirectory,
            },
            ["Library"] = new JsonObject
            {
                ["DownloadPath"] = _config["Library:DownloadPath"] ?? "/music",
            },
            ["Soulseek"] = new JsonObject
            {
                ["BaseUrl"] = soulseek.BaseUrl ?? "",
                ["Username"] = soulseek.Username ?? "",
                ["Password"] = soulseek.Password ?? "",
                ["SearchWaitSeconds"] = soulseek.SearchWaitSeconds,
                ["MinFileSizeBytes"] = soulseek.MinFileSizeBytes,
                ["PreferredExtension"] = soulseek.PreferredExtension,
                ["DownloadTimeoutSeconds"] = soulseek.DownloadTimeoutSeconds,
            },
            ["Lidarr"] = new JsonObject
            {
                ["BaseUrl"] = lidarr.BaseUrl ?? "",
                ["ApiKey"] = lidarr.ApiKey ?? "",
                ["RootFolderPath"] = lidarr.RootFolderPath ?? "",
                ["QualityProfileId"] = lidarr.QualityProfileId,
                ["MetadataProfileId"] = lidarr.MetadataProfileId,
                ["CompletionMode"] = lidarr.CompletionMode.ToString(),
                ["ImportTimeoutSeconds"] = lidarr.ImportTimeoutSeconds,
            },
            ["YouTube"] = new JsonObject
            {
                ["ShimUrl"] = _config["YouTube:ShimUrl"] ?? "",
            },
            ["LastFm"] = new JsonObject
            {
                ["ApiKey"] = lastfm.ApiKey ?? "",
                ["EnableRadio"] = lastfm.EnableRadio,
                ["RadioTrackCount"] = lastfm.RadioTrackCount,
                ["RadioCacheDurationHours"] = lastfm.RadioCacheDurationHours,
                ["EnablePersonalizedStations"] = lastfm.EnablePersonalizedStations,
                ["EnableDiscoveryStations"] = lastfm.EnableDiscoveryStations,
                ["ExposeRadioAsPlaylists"] = lastfm.ExposeRadioAsPlaylists,
                ["ExposeRadioAsStreams"] = lastfm.ExposeRadioAsStreams,
                ["RadioStreamBitrateKbps"] = lastfm.RadioStreamBitrateKbps,
                ["EnableIcyMetadata"] = lastfm.EnableIcyMetadata,
                ["HistoryRetentionDays"] = lastfm.HistoryRetentionDays,
                ["DiscoveryPercent"] = lastfm.DiscoveryPercent,
                ["RefreshIntervalHours"] = lastfm.RefreshIntervalHours,
                ["MinimumPlays"] = lastfm.MinimumPlays,
                ["DiscoveryStations"] = JsonSerializer.SerializeToNode(lastfm.DiscoveryStations),
            },
            ["Metadata"] = new JsonObject
            {
                ["Language"] = _metadataOpts.CurrentValue.Language ?? "",
            },
            ["Notifications"] = new JsonObject
            {
                ["NtfyUrl"] = notif.NtfyUrl ?? "",
                ["NtfyToken"] = notif.NtfyToken ?? "",
                ["DiscordWebhookUrl"] = notif.DiscordWebhookUrl ?? "",
                ["NotifyDownloadStarted"] = notif.NotifyDownloadStarted,
                ["NotifyDownloadCompleted"] = notif.NotifyDownloadCompleted,
                ["NotifyLosslessFallback"] = notif.NotifyLosslessFallback,
                ["NotifyDownloadFailed"] = notif.NotifyDownloadFailed,
                ["NotifyAlbumCompleted"] = notif.NotifyAlbumCompleted,
            },
        };
        var json = effective.ToJsonString(new JsonSerializerOptions { WriteIndented = true });
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
            "Subsonic:DownloadOnStar", "Subsonic:DownloadAlbumOnStar",
            "Subsonic:WaitForLosslessOnPlay", "Subsonic:LosslessWaitTimeoutSeconds",
            "Subsonic:DownloadSource", "Subsonic:AutoDetectDownloadPath", "Subsonic:LibraryPath",
            "Subsonic:FolderStructure",
            "Subsonic:UseLocalStaging", "Subsonic:ExplicitFilter",
            "Subsonic:CacheDurationHours", "Subsonic:EnableExternalPlaylists",
            "Subsonic:PlaylistsDirectory",
            "Library:DownloadPath",
            "Soulseek:BaseUrl", "Soulseek:Username", "Soulseek:Password",
            "Soulseek:SearchWaitSeconds", "Soulseek:MinFileSizeBytes",
            "Soulseek:PreferredExtension", "Soulseek:DownloadTimeoutSeconds",
            "Lidarr:BaseUrl", "Lidarr:ApiKey", "Lidarr:RootFolderPath",
            "Lidarr:QualityProfileId", "Lidarr:MetadataProfileId",
            "Lidarr:CompletionMode", "Lidarr:ImportTimeoutSeconds",
            "YouTube:ShimUrl",
            "LastFm:ApiKey", "LastFm:EnableRadio", "LastFm:RadioTrackCount",
            "LastFm:RadioCacheDurationHours",
            "LastFm:EnablePersonalizedStations", "LastFm:EnableDiscoveryStations",
            "LastFm:HistoryRetentionDays", "LastFm:DiscoveryPercent",
            "LastFm:RefreshIntervalHours",
            "LastFm:MinimumPlays", "LastFm:DiscoveryStations",
            "Metadata:Language",
            "Notifications:NtfyUrl", "Notifications:NtfyToken",
            "Notifications:DiscordWebhookUrl",
            "Notifications:NotifyDownloadStarted", "Notifications:NotifyDownloadCompleted",
            "Notifications:NotifyLosslessFallback", "Notifications:NotifyDownloadFailed",
            "Notifications:NotifyAlbumCompleted",
        };
        var rows = new List<object>();
        foreach (var k in keys)
        {
            var v = _config[k] ?? "";
            // Mask anything that smells like a secret so a screenshot of the
            // page doesn't leak credentials.
            var isSecret = k.EndsWith("Password", StringComparison.OrdinalIgnoreCase)
                        || k.EndsWith("ApiKey", StringComparison.OrdinalIgnoreCase)
                        // A Discord webhook URL embeds its token, so the whole URL is
                        // the secret; ntfy tokens are credentials outright.
                        || k.EndsWith("Token", StringComparison.OrdinalIgnoreCase)
                        || k.EndsWith("WebhookUrl", StringComparison.OrdinalIgnoreCase);
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
    /// Navidrome, slskd, Lidarr, the yt-dlp shim, and Last.fm.
    /// </summary>
    [HttpGet("status")]
    public async Task<IActionResult> GetStatus(CancellationToken ct)
    {
        // Probe each in parallel — total time is the slowest probe, not sum.
        var probeTasks = new Dictionary<string, Task<ServiceProbe>>
        {
            ["navidrome"] = ProbeNavidromeAsync(ct),
            ["slskd"] = ProbeSlskdAsync(ct),
            ["lidarr"] = ProbeLidarrAsync(ct),
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

    /// <summary>Choices owned by the connected Lidarr instance for add-album defaults.</summary>
    [HttpGet("lidarr/options")]
    public async Task<IActionResult> GetLidarrOptions(CancellationToken ct)
    {
        try { return new JsonResult(await _lidarr.GetOptionsAsync(ct)); }
        catch (Exception ex) { return BadRequest(new { error = ex.Message }); }
    }

    public record LidarrConnectionTestRequest(string? BaseUrl, string? ApiKey);

    /// <summary>Tests entered credentials without persisting them.</summary>
    [HttpPost("lidarr/test")]
    public async Task<IActionResult> TestLidarrConnection(
        [FromBody] LidarrConnectionTestRequest request, CancellationToken ct)
    {
        try
        {
            var options = await _lidarr.TestConnectionAsync(
                request.BaseUrl ?? "", request.ApiKey ?? "", ct);
            return Ok(new { ok = true, message = "Connected to Lidarr. Choices loaded.", options });
        }
        catch (Exception ex) { return BadRequest(new { ok = false, error = ex.Message }); }
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

    private async Task<ServiceProbe> ProbeLidarrAsync(CancellationToken ct)
    {
        var settings = _lidarrOpts.CurrentValue;
        var lidarrEnabled = _subsonicOpts.CurrentValue.EffectiveHeartDownloadSources()
            .Any(step => step.Source == HeartDownloadSource.Lidarr
                         && (step.SongEnabled == true || step.AlbumEnabled == true));
        // An absent optional service is a calm state, not a warning: most installs
        // never configure Lidarr and their dashboard should not carry a permanent
        // yellow dot for it. Yellow means "you enabled it but haven't finished
        // setting it up" — incomplete config, as opposed to an outage.
        if (string.IsNullOrWhiteSpace(settings.BaseUrl) || string.IsNullOrWhiteSpace(settings.ApiKey))
            return lidarrEnabled
                ? new ServiceProbe(true, "selected but not configured", Warning: true)
                : new ServiceProbe(true, "not configured (optional)");
        if (lidarrEnabled
            && (string.IsNullOrWhiteSpace(settings.RootFolderPath)
                || settings.QualityProfileId <= 0 || settings.MetadataProfileId <= 0))
            return new ServiceProbe(true, "select a root folder and profiles", Warning: true);
        try
        {
            var ok = await _lidarr.IsReachableAsync(ct);
            return new ServiceProbe(ok, ok ? "reachable" : "unreachable / API key invalid");
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

    private record ServiceProbe(bool Ok, string Detail, bool Warning = false);

    /// <summary>The release this build came from, e.g. "2026.07.29". Falls back to the
    /// assembly version if the informational version was not stamped.</summary>
    private static string OctoVersion =>
        typeof(AdminController).Assembly
            .GetCustomAttributes(typeof(System.Reflection.AssemblyInformationalVersionAttribute), false)
            .OfType<System.Reflection.AssemblyInformationalVersionAttribute>()
            .FirstOrDefault()?.InformationalVersion
            // .NET appends "+<commit sha>" to the informational version; trim it.
            ?.Split('+')[0]
        ?? typeof(AdminController).Assembly.GetName().Version?.ToString()
        ?? "unknown";

    /// <summary>
    /// Drop every cached metadata answer and cover image.
    ///
    /// Cached entries now expire on their own, so this is a recovery lever rather than
    /// routine maintenance: it turns "wait for the TTL" into "fixed now" when a run of
    /// throttled upstream calls has left albums or covers looking wrong. Clearing every
    /// instance matters, because a poisoned entry surviving in one of them would outlive
    /// the very button meant to remove it.
    /// </summary>
    [HttpPost("clear-metadata-cache")]
    public IActionResult ClearMetadataCache()
    {
        _deezer.ClearCaches();
        _coverArt.ClearCache();
        _logger.LogInformation("Metadata and cover-art caches cleared by admin request");
        return Ok(new { cleared = true });
    }
}
