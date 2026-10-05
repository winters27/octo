using Microsoft.AspNetCore.Mvc;
using Microsoft.Extensions.Options;
using System.Text.Json;
using System.Text.Json.Nodes;
using Octo.Models.Settings;
using Octo.Services.Admin;
using Octo.Services.LastFm;
using Octo.Services.Library;
using Octo.Services.Metadata;
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
    private readonly RestartTracker _restartTracker;
    private readonly IOptionsMonitor<SubsonicSettings> _subsonicOpts;
    private readonly IOptionsMonitor<SoulseekSettings> _soulseekOpts;
    private readonly IOptionsMonitor<LidarrSettings> _lidarrOpts;
    private readonly IOptionsMonitor<LastFmSettings> _lastFmOpts;
    private readonly IOptionsMonitor<NotificationSettings> _notificationOpts;
    private readonly IOptionsMonitor<MetadataSettings> _metadataOpts;
    private readonly Octo.Services.Soulseek.RejectedPeerRegistry _rejectedPeers;
    private readonly IOptionsMonitor<GenreSettings> _genreOpts;
    private readonly Octo.Services.Metadata.GenreBackfillWorker _genreBackfill;
    private readonly Octo.Services.Metadata.GenreBackfillJournal _genreJournal;
    private readonly Octo.Services.Library.NavidromeSongPathResolver _songPaths;
    private readonly IOptionsMonitor<LibraryActionSettings> _libraryActionOpts;
    private readonly Octo.Services.Library.LibraryActionJournal _libraryActionJournal;
    private readonly IOptionsMonitor<ServerSettings> _serverOpts;
    private readonly IOptionsMonitor<ListenBrainzSettings>? _listenBrainzOpts;
    private readonly Octo.Services.ListenBrainz.ListenBrainzService? _listenBrainz;
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
    private readonly Octo.Services.Library.NoticeQueue? _notices;
    private readonly Octo.Services.Library.DuplicateScanWorker? _duplicates;
    private readonly IOptionsMonitor<GeneratedPlaylistSettings>? _generatedOpts;
    private readonly Octo.Services.Common.AcquisitionTracker? _acquisitions;
    private readonly LastFmScrobbleService? _lastFmScrobbles;
    private readonly Octo.Services.Library.QualityUpgradeWorker? _qualityUpgrade;
    private readonly Octo.Services.Library.LibraryReviewSweepWorker? _reviewSweep;
    private readonly Octo.Services.Soulseek.ISoulseekLink? _soulseekLink;
    private readonly Octo.Services.Library.UpgradeQueue? _upgradeQueue;
    private readonly Octo.Services.Common.DownloadConcurrency? _downloadConcurrency;

    public AdminController(
        SettingsFileWriter settings,
        RestartTracker restartTracker,
        IOptionsMonitor<SubsonicSettings> subsonicOpts,
        IOptionsMonitor<SoulseekSettings> soulseekOpts,
        IOptionsMonitor<LidarrSettings> lidarrOpts,
        IOptionsMonitor<LastFmSettings> lastFmOpts,
        IOptionsMonitor<NotificationSettings> notificationOpts,
        IOptionsMonitor<MetadataSettings> metadataOpts,
        IOptionsMonitor<GenreSettings> genreOpts,
        Octo.Services.Metadata.GenreBackfillWorker genreBackfill,
        Octo.Services.Metadata.GenreBackfillJournal genreJournal,
        Octo.Services.Library.NavidromeSongPathResolver songPaths,
        IOptionsMonitor<LibraryActionSettings> libraryActionOpts,
        Octo.Services.Library.LibraryActionJournal libraryActionJournal,
        IOptionsMonitor<ServerSettings> serverOpts,
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
        Octo.Services.Soulseek.RejectedPeerRegistry rejectedPeers,
        IHttpClientFactory httpFactory,
        IHostApplicationLifetime lifetime,
        ILogger<AdminController> logger,
        LastFmRadioStateStore? radioState = null,
        LastFmRadioRefreshQueue? radioRefresh = null,
        IOptionsMonitor<ListenBrainzSettings>? listenBrainzOpts = null,
        Octo.Services.ListenBrainz.ListenBrainzService? listenBrainz = null,
        Octo.Services.Library.NoticeQueue? notices = null,
        Octo.Services.Library.DuplicateScanWorker? duplicates = null,
        IOptionsMonitor<GeneratedPlaylistSettings>? generatedOpts = null,
        Octo.Services.Common.AcquisitionTracker? acquisitions = null,
        LastFmScrobbleService? lastFmScrobbles = null,
        Octo.Services.Library.QualityUpgradeWorker? qualityUpgrade = null,
        Octo.Services.Library.LibraryReviewSweepWorker? reviewSweep = null,
        Octo.Services.Soulseek.ISoulseekLink? soulseekLink = null,
        Octo.Services.Library.UpgradeQueue? upgradeQueue = null,
        Octo.Services.Common.DownloadConcurrency? downloadConcurrency = null)
    {
        _upgradeQueue = upgradeQueue;
        _downloadConcurrency = downloadConcurrency;
        _soulseekLink = soulseekLink;
        _reviewSweep = reviewSweep;
        _qualityUpgrade = qualityUpgrade;
        _lastFmScrobbles = lastFmScrobbles;
        _acquisitions = acquisitions;
        _generatedOpts = generatedOpts;
        _notices = notices;
        _duplicates = duplicates;
        _listenBrainzOpts = listenBrainzOpts;
        _listenBrainz = listenBrainz;
        _deezer = deezer;
        _coverArt = coverArt;
        _settings = settings;
        _restartTracker = restartTracker;
        _subsonicOpts = subsonicOpts;
        _soulseekOpts = soulseekOpts;
        _lidarrOpts = lidarrOpts;
        _lastFmOpts = lastFmOpts;
        _notificationOpts = notificationOpts;
        _metadataOpts = metadataOpts;
        _rejectedPeers = rejectedPeers;
        _genreOpts = genreOpts;
        _genreBackfill = genreBackfill;
        _genreJournal = genreJournal;
        _songPaths = songPaths;
        _libraryActionOpts = libraryActionOpts;
        _libraryActionJournal = libraryActionJournal;
        _serverOpts = serverOpts;
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

    /// <summary>Checks the ListenBrainz token that applies to a listener (or the
    /// default) against ListenBrainz, so a mistyped token shows up before a play is lost.</summary>
    [HttpGet("listenbrainz/validate")]
    public async Task<IActionResult> ValidateListenBrainz([FromQuery] string? user = null,
        [FromQuery] string? token = null)
    {
        if (_listenBrainz is null || _listenBrainzOpts is null)
            return Ok(new { configured = false, valid = false, detail = "ListenBrainz is not available." });
        var candidate = string.IsNullOrWhiteSpace(token) || token == SecretPlaceholder
            ? _listenBrainzOpts.CurrentValue.TokenFor(user ?? "") ?? ""
            : token;
        if (candidate.Length == 0)
            return Ok(new { configured = false, valid = false, detail = "No token configured." });
        var (valid, userName, detail) = await _listenBrainz.ValidateTokenAsync(candidate, HttpContext.RequestAborted);
        return Ok(new { configured = true, valid, userName, detail });
    }

    public record ListenBrainzValidateRequest(string? User, string? Token);

    /// <summary>The same check as the GET, with the token in the body so a typed token never
    /// lands in a URL, a proxy log or the browser history. The GET stays for compatibility.</summary>
    [HttpPost("listenbrainz/validate")]
    public Task<IActionResult> ValidateListenBrainzPost([FromBody] ListenBrainzValidateRequest request) =>
        ValidateListenBrainz(request.User, request.Token);

    /// <summary>Who can scrobble outside plays to Last.fm: every Navidrome user Octo knows of,
    /// with the Last.fm account each is connected to. Session keys never leave the server.</summary>
    [HttpGet("lastfm/scrobble")]
    public IActionResult GetLastFmScrobbling()
    {
        var settings = _lastFmOpts.CurrentValue;
        var known = (_radioState?.GetSummaries().Select(summary => summary.Username) ?? [])
            .Concat(_listenBrainzOpts?.CurrentValue.UserTokens.Keys ?? Enumerable.Empty<string>());
        return Ok(new
        {
            available = _lastFmScrobbles is not null,
            hasApiKey = !string.IsNullOrWhiteSpace(settings.ApiKey),
            hasApiSecret = !string.IsNullOrWhiteSpace(settings.ApiSecret),
            enabled = settings.ScrobbleExternalPlays,
            libraryPlays = settings.ScrobbleLibraryPlays,
            users = _lastFmScrobbles?.Users(known) ?? [],
        });
    }

    public sealed class LastFmScrobbleUserRequest { public string User { get; set; } = string.Empty; }

    public sealed class LastFmCredentialsRequest
    {
        public string? ApiKey { get; set; }
        public string? ApiSecret { get; set; }
    }

    /// <summary>Checks an API key and shared secret with Last.fm before Save writes them. A blank
    /// or placeholder value means the saved one, so the page can check a secret it never sees.</summary>
    [HttpPost("lastfm/check")]
    public async Task<IActionResult> CheckLastFmCredentials([FromBody] LastFmCredentialsRequest request)
    {
        if (_lastFmScrobbles is null) return NotFound(new { error = "Last.fm scrobbling is not available." });
        static string? Typed(string? value) => value == SecretPlaceholder ? null : value;
        var check = await _lastFmScrobbles.CheckCredentialsAsync(
            Typed(request.ApiKey), Typed(request.ApiSecret), HttpContext.RequestAborted);
        return Ok(new { key = check.Key, secret = check.Secret, message = check.Message });
    }

    /// <summary>Stops waiting on a Connect nobody is going to approve.</summary>
    [HttpPost("lastfm/scrobble/cancel")]
    public IActionResult CancelLastFmConnect([FromBody] LastFmScrobbleUserRequest request)
    {
        if (_lastFmScrobbles is null) return NotFound(new { error = "Last.fm scrobbling is not available." });
        _lastFmScrobbles.CancelConnect(request.User ?? "");
        return Ok(new { ok = true });
    }

    /// <summary>Step one of connecting: the page on last.fm where the admin, signed in as the
    /// listener, approves Octo.</summary>
    [HttpPost("lastfm/scrobble/connect")]
    public async Task<IActionResult> ConnectLastFm([FromBody] LastFmScrobbleUserRequest request)
    {
        if (_lastFmScrobbles is null) return NotFound(new { error = "Last.fm scrobbling is not available." });
        try
        {
            var url = await _lastFmScrobbles.BeginConnectAsync(request.User ?? "", HttpContext.RequestAborted);
            return Ok(new { user = (request.User ?? "").Trim(), url });
        }
        catch (LastFmScrobbleException ex) { return BadRequest(new { error = ex.Message }); }
    }

    /// <summary>Step two, once Octo has been approved on last.fm: saves the session. 409 while
    /// Last.fm has not seen the approval yet, so the dashboard can say "not yet" and let the
    /// admin press Finish again.</summary>
    [HttpPost("lastfm/scrobble/finish")]
    public async Task<IActionResult> FinishLastFm([FromBody] LastFmScrobbleUserRequest request)
    {
        if (_lastFmScrobbles is null) return NotFound(new { error = "Last.fm scrobbling is not available." });
        try
        {
            var session = await _lastFmScrobbles.FinishConnectAsync(request.User ?? "", HttpContext.RequestAborted);
            return Ok(new { ok = true, user = (request.User ?? "").Trim(), lastFmUser = session.LastFmUser });
        }
        catch (LastFmScrobbleException ex) when (ex.Code == LastFmScrobbleService.ErrorTokenNotAuthorized)
        {
            return Conflict(new { error = ex.Message });
        }
        catch (LastFmScrobbleException ex) { return BadRequest(new { error = ex.Message }); }
        catch (SettingsFileCorruptException ex)
        {
            return Conflict(new { error = $"{ex.Message} Fix it in Raw config, or on disk at {ex.Path}, then Connect again." });
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            // Last.fm has already handed over the session; only saving it failed (the file is
            // locked, read-only, or the disk is full). Nothing was saved, so say so plainly.
            _logger.LogWarning(ex, "Could not save the Last.fm session to {Path}", _settings.FilePath);
            return Conflict(new
            {
                error = $"Last.fm approved Octo, but the connection could not be saved to {_settings.FilePath} ({ex.Message})."
                        + " Make sure Octo can write that file, then press Finish again, or Connect again if the link has expired.",
            });
        }
    }

    [HttpPost("lastfm/scrobble/disconnect")]
    public IActionResult DisconnectLastFm([FromBody] LastFmScrobbleUserRequest request)
    {
        if (_lastFmScrobbles is null) return NotFound(new { error = "Last.fm scrobbling is not available." });
        try
        {
            var user = (request.User ?? "").Trim();
            var fromFile = _lastFmScrobbles.Disconnect(user);
            var fromEnvironment = !fromFile && _lastFmOpts.CurrentValue.SessionFor(user) is not null;
            return Ok(new
            {
                ok = true, user,
                message = fromEnvironment
                    ? $"Stopped for now. This session is set in the environment (LASTFM__USERSESSIONS__{user}__SESSIONKEY), so remove it there as well or it comes back after a restart."
                    : "Disconnected. To revoke Octo on Last.fm too, remove it from that account's applications.",
            });
        }
        catch (LastFmScrobbleException ex) { return BadRequest(new { error = ex.Message }); }
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
            Response.Cookies.Append(BrowseCookieName, token, BrowseCookieOptions());
            return Ok(new { ok = true, user = req.Username });
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
        if (BrowseUser(token) is null)
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
    internal const string BrowseCookieName = "octo_browse";

    /// <summary>
    /// The cookie's terms. Secure only over HTTPS, since this is normally reached over plain HTTP
    /// on a LAN and a Secure cookie would simply be dropped there. Its age is set again on every
    /// signed-in request, so the browser keeps it exactly as long as the server keeps the session.
    /// </summary>
    private CookieOptions BrowseCookieOptions() => new()
    {
        HttpOnly = true,
        SameSite = SameSiteMode.Strict,
        Secure = Request.IsHttps,
        Path = "/api/admin",
        MaxAge = BrowseSessionStore.Ttl,
    };

    /// <summary>
    /// The Navidrome admin this request is signed in as, or null. Cookie first (how the admin UI
    /// signs in), header second so the endpoints stay usable from curl. A live cookie is renewed.
    /// </summary>
    private string? BrowseUser(string? headerToken)
    {
        var cookie = Request.Cookies[BrowseCookieName];
        var user = _browseSessions.UserOf(cookie ?? headerToken);
        if (user is not null && cookie is not null) Response.Cookies.Append(BrowseCookieName, cookie, BrowseCookieOptions());
        return user;
    }

    /// <summary>Who this browser is signed in as, for the dashboard's footer. Never prompts.</summary>
    [HttpGet("browse/session")]
    public IActionResult BrowseSession() =>
        BrowseUser(null) is { } user ? Ok(new { signedIn = true, user }) : Ok(new { signedIn = false });

    /// <summary>Sign this browser out: the session is forgotten and the cookie removed.</summary>
    [HttpPost("browse/signout")]
    public IActionResult BrowseSignOut()
    {
        _browseSessions.Revoke(Request.Cookies[BrowseCookieName]);
        Response.Cookies.Delete(BrowseCookieName, new CookieOptions { Path = "/api/admin" });
        return Ok(new { ok = true });
    }

    /// <summary>The running log of songs Octo has fetched, newest first.</summary>
    [HttpGet("downloads")]
    public IActionResult Downloads()
    {
        return Ok(new { downloads = _history.GetRecent(200) });
    }

    /// <summary>A file inside the music folder, or an artist and a title, to try the matching on.</summary>
    public sealed record TagPreviewRequest(string? Path, string? Artist, string? Title, string? Album);

    /// <summary>
    /// How a song would be matched and tagged, without touching anything. Gated on the browse
    /// sign-in like the other endpoints that read files, and a path is only ever a file inside
    /// the music folder, resolved in full, with no link on the way.
    /// </summary>
    [HttpPost("tags/preview")]
    public async Task<IActionResult> PreviewTags([FromBody] TagPreviewRequest request,
        [FromHeader(Name = "X-Octo-Browse-Token")] string? token, CancellationToken ct)
    {
        if (!HasBrowseSession(token))
            return Unauthorized(new { error = "Sign in with your Navidrome admin account first." });
        if (HttpContext.RequestServices.GetService<Octo.Services.Tagging.TagPreview>() is not { } preview)
            return StatusCode(503, new { error = "The matching is not available on this host." });

        string? path = null;
        if (!string.IsNullOrWhiteSpace(request.Path))
        {
            var root = _navIdentity.EffectiveDownloadPath(_config["Library:DownloadPath"] ?? "./downloads");
            path = ResolveUnderRoot(request.Path, root);
            if (path is null) return BadRequest(new { error = "The path must be a file inside the music folder." });
        }
        else if (string.IsNullOrWhiteSpace(request.Artist) || string.IsNullOrWhiteSpace(request.Title))
            return BadRequest(new { error = "Give a path inside the music folder, or an artist and a title." });

        return Ok(await preview.PreviewAsync(path, request.Artist, request.Title, request.Album, ct));
    }

    /// <summary>
    /// The full path of a file that really sits under the root: no ".." out of it, no path of
    /// its own, and no link on the way, so the preview cannot be pointed at any other file.
    /// </summary>
    internal static string? ResolveUnderRoot(string candidate, string root)
    {
        try
        {
            var full = System.IO.Path.GetFullPath(candidate);
            var rootFull = System.IO.Path.GetFullPath(root).TrimEnd(System.IO.Path.DirectorySeparatorChar, System.IO.Path.AltDirectorySeparatorChar);
            var relative = System.IO.Path.GetRelativePath(rootFull, full);
            if (relative == "." || relative.StartsWith("..", StringComparison.Ordinal) || System.IO.Path.IsPathRooted(relative)) return null;

            var info = new FileInfo(full);
            if (!info.Exists || info.LinkTarget is not null) return null;
            for (var dir = info.Directory; dir is not null && dir.FullName.Length > rootFull.Length; dir = dir.Parent)
                if (dir.LinkTarget is not null) return null;
            return full;
        }
        catch
        {
            return null;
        }
    }

    /// <summary>
    /// Every hearted download in flight or ended in the last half hour, everyone's, newest
    /// first. The rows the app reads through getAcquisitions, plus the provider key, and who
    /// asked only while Record who asked is on, the rule the fetched-songs log follows too.
    /// </summary>
    [HttpGet("acquisitions")]
    public IActionResult Acquisitions()
    {
        var showAskers = _subsonicOpts.CurrentValue.RecordRequestedBy;
        var rows = (_acquisitions?.All() ?? []).Select(row =>
        {
            var json = SubsonicResponseBuilder.AcquisitionJson(row);
            json["provider"] = row.Provider;
            json["externalId"] = row.ExternalId;
            if (showAskers && row.RequestedBy.Count > 0) json["requestedBy"] = row.RequestedBy;
            return json;
        }).ToList();
        return Ok(new { acquisitions = rows });
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
        var genre = _genreOpts.CurrentValue;
        var actions = _libraryActionOpts.CurrentValue;
        var mixes = _generatedOpts?.CurrentValue ?? new GeneratedPlaylistSettings();
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
                // Both are fields on the Music server form. Missing here, they never pre-filled,
                // so every save of that form wrote empty strings over them and quietly took away
                // the admin identity library actions and authenticated rescans depend on. The
                // password goes out as a placeholder the save swaps back, so it never reaches a
                // browser.
                ["AdminUsername"] = subsonic.AdminUsername ?? "",
                ["AdminPassword"] = MaskSecret(subsonic.AdminPassword),
                ["EnableSearchDiscovery"] = subsonic.EnableSearchDiscovery,
                ["WaitForSearchDurations"] = subsonic.WaitForSearchDurations,
                ["EnableSyncCatalog"] = subsonic.EnableSyncCatalog,
                ["SyncCatalogClients"] = subsonic.SyncCatalogClients,
                ["SyncCatalogMaxSongs"] = subsonic.SyncCatalogMaxSongs,
                ["StorageMode"] = subsonic.StorageMode.ToString(),
                ["DownloadMode"] = subsonic.DownloadMode.ToString(),
                ["DownloadOnStar"] = subsonic.DownloadOnStar,
                ["DownloadAlbumOnStar"] = subsonic.DownloadAlbumOnStar,
                ["RecordRequestedBy"] = subsonic.RecordRequestedBy,
                ["StarDownloadsForRequester"] = subsonic.StarDownloadsForRequester,
                ["SkipOwnedSongs"] = subsonic.SkipOwnedSongs,
                ["WaitForLosslessOnPlay"] = subsonic.WaitForLosslessOnPlay,
                ["LosslessWaitTimeoutSeconds"] = subsonic.LosslessWaitTimeoutSeconds,
                ["DownloadOnPlay"] = subsonic.DownloadOnPlay,
                ["LidarrAlbumOnPlay"] = subsonic.LidarrAlbumOnPlay,
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
            ["Server"] = new Dictionary<string, object>
            {
                ["PublicUrl"] = _serverOpts.CurrentValue.PublicUrl ?? "",
            },
            ["Updates"] = new Dictionary<string, object>
            {
                ["Check"] = UpdateOptions.Check,
                ["Repo"] = UpdateOptions.Repo ?? "",
            },
            ["Soulseek"] = new Dictionary<string, object>
            {
                ["BaseUrl"] = soulseek.BaseUrl ?? "",
                ["Username"] = soulseek.Username ?? "",
                // slskd's web login opens slskd entirely, so it goes out as the placeholder too.
                ["Password"] = MaskSecret(soulseek.Password),
                ["WebUrl"] = soulseek.WebUrl ?? "",
                ["CheckListenPort"] = soulseek.CheckListenPort,
                ["SearchWaitSeconds"] = soulseek.SearchWaitSeconds,
                ["UpgradeSearchWaitSeconds"] = soulseek.UpgradeSearchWaitSeconds,
                ["MinFileSizeBytes"] = soulseek.MinFileSizeBytes,
                ["PreferredExtension"] = soulseek.PreferredExtension,
                ["DownloadTimeoutSeconds"] = soulseek.DownloadTimeoutSeconds,
                ["VerifyDownloads"] = soulseek.VerifyDownloads,
                ["AcoustIdApiKey"] = MaskSecret(soulseek.AcoustIdApiKey),
                ["MinMatchScore"] = soulseek.MinMatchScore,
                ["TagFromMusicBrainz"] = soulseek.TagFromMusicBrainz,
                ["NameFromMatch"] = soulseek.NameFromMatch,
                ["RejectedPeerTtlDays"] = soulseek.RejectedPeerTtlDays,
                ["FingerprintSeconds"] = soulseek.FingerprintSeconds,
                ["FingerprintTimeoutSeconds"] = soulseek.FingerprintTimeoutSeconds,
                ["AcoustIdTimeoutSeconds"] = soulseek.AcoustIdTimeoutSeconds,
                ["DetectTranscodes"] = soulseek.DetectTranscodes,
                ["TranscodeCheckTimeoutSeconds"] = soulseek.TranscodeCheckTimeoutSeconds,
                ["OutageHoldHours"] = soulseek.OutageHoldHours,
                ["ParallelDownloads"] = soulseek.ParallelDownloads,
                ["AlbumFolders"] = soulseek.AlbumFolders,
                ["SubmitConfirmedFingerprints"] = soulseek.SubmitConfirmedFingerprints,
                ["AcoustIdUserApiKey"] = MaskSecret(soulseek.AcoustIdUserApiKey),
            },
            ["Lidarr"] = new Dictionary<string, object>
            {
                ["BaseUrl"] = lidarr.BaseUrl ?? "",
                ["ApiKey"] = MaskSecret(lidarr.ApiKey),
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
                ["ApiKey"] = MaskSecret(lastfm.ApiKey),
                ["ApiSecret"] = MaskSecret(lastfm.ApiSecret),
                ["ScrobbleExternalPlays"] = lastfm.ScrobbleExternalPlays,
                ["ScrobbleLibraryPlays"] = lastfm.ScrobbleLibraryPlays,
                ["UserSessions"] = MaskSessions(lastfm.UserSessions),
                ["EnableRadio"] = lastfm.EnableRadio,
                ["RadioTrackCount"] = lastfm.RadioTrackCount,
                ["RadioCacheDurationHours"] = lastfm.RadioCacheDurationHours,
                ["EnablePersonalizedStations"] = lastfm.EnablePersonalizedStations,
                ["EnableYourMix"] = lastfm.EnableYourMix,
                ["EnableDiscoveryMix"] = lastfm.EnableDiscoveryMix,
                ["ArtistStationCount"] = lastfm.ArtistStationCount,
                ["GenreStationCount"] = lastfm.GenreStationCount,
                ["EnableDiscoveryStations"] = lastfm.EnableDiscoveryStations,
                ["ExposeRadioAsPlaylists"] = lastfm.ExposeRadioAsPlaylists,
                ["ExposeRadioAsStreams"] = lastfm.ExposeRadioAsStreams,
                ["RadioStreamBitrateKbps"] = lastfm.RadioStreamBitrateKbps,
                ["EnableIcyMetadata"] = lastfm.EnableIcyMetadata,
                ["StarterPublishTimeoutSeconds"] = lastfm.StarterPublishTimeoutSeconds,
                ["RadioLoudnessTargetLufs"] = lastfm.RadioLoudnessTargetLufs,
                ["HistoryRetentionDays"] = lastfm.HistoryRetentionDays,
                ["DiscoveryPercent"] = lastfm.DiscoveryPercent,
                ["RefreshIntervalHours"] = lastfm.RefreshIntervalHours,
                ["MinimumPlays"] = lastfm.MinimumPlays,
                ["DiscoveryStations"] = (_settings.Load()["LastFm"] as JsonObject)?["DiscoveryStations"]?.DeepClone()
                    ?? JsonSerializer.SerializeToNode(lastfm.DiscoveryStations)!,
            },
            ["LibraryActions"] = new Dictionary<string, object>
            {
                ["Enabled"] = actions.Enabled,
                ["PlaylistsEnabled"] = actions.PlaylistsEnabled,
                ["RatingsEnabled"] = actions.RatingsEnabled,
                ["PlaylistPrefix"] = actions.PlaylistPrefix ?? "",
                ["DryRun"] = actions.DryRun,
                ["QuarantineDirectory"] = actions.QuarantineDirectory ?? "",
                ["QuarantineRetentionDays"] = actions.QuarantineRetentionDays,
                ["PollIntervalSeconds"] = actions.PollIntervalSeconds,
                ["MaxActionsPerCycle"] = actions.MaxActionsPerCycle,
                ["KeepReplacedOriginals"] = actions.KeepReplacedOriginals,
                ["NoticePrefix"] = actions.NoticePrefix ?? "",
                ["ReviewEnabled"] = actions.ReviewEnabled,
                ["ReviewPlaylistName"] = actions.ReviewPlaylistName ?? "",
                ["ReviewSweepPerHour"] = actions.ReviewSweepPerHour,
                ["ReviewSweepOctoDownloads"] = actions.ReviewSweepOctoDownloads,
                ["DuplicatesEnabled"] = actions.DuplicatesEnabled,
                ["DuplicatesPlaylistName"] = actions.DuplicatesPlaylistName ?? "",
                ["DuplicatesScanHours"] = actions.DuplicatesScanHours,
                ["UpgradePerWeek"] = actions.UpgradePerWeek,
                ["UpgradeSource"] = actions.UpgradeSource.ToString(),
                ["NoticeMaxTracks"] = actions.NoticeMaxTracks,
                ["RatingsScope"] = actions.RatingsScope.ToString(),
                ["AllowedUsers"] = actions.AllowedUsers ?? [],
                // Effective rather than raw, because the editor needs every action present
                // even when the config names only some of them. Projected so the enum lands as
                // its NAME: serialized directly it becomes a number, and the editor looks the
                // action up by name to label the row.
                ["Actions"] = JsonSerializer.SerializeToNode(actions.EffectiveActions()
                    .Select(action => new
                    {
                        Action = action.Action.ToString(),
                        action.Name,
                        action.Enabled,
                        action.Rating,
                    }))!,
            },
            ["Metadata"] = new Dictionary<string, object>
            {
                ["Language"] = _metadataOpts.CurrentValue.Language ?? "",
                ["AlbumFromTitle"] = _metadataOpts.CurrentValue.AlbumFromTitle,
                ["UseCoverArtArchive"] = _metadataOpts.CurrentValue.UseCoverArtArchive,
                ["ReplaceVideoCovers"] = _metadataOpts.CurrentValue.ReplaceVideoCovers,
                ["WriteCoverFile"] = _metadataOpts.CurrentValue.WriteCoverFile,
                ["EmbedFullSizeCovers"] = _metadataOpts.CurrentValue.EmbedFullSizeCovers,
                ["FetchLyrics"] = _metadataOpts.CurrentValue.FetchLyrics,
                ["LyricsSources"] = _metadataOpts.CurrentValue.LyricsSources ?? "",
                ["PreferWordTimedLyrics"] = _metadataOpts.CurrentValue.PreferWordTimedLyrics,
                ["WriteLyricsBesideAllSongs"] = _metadataOpts.CurrentValue.WriteLyricsBesideAllSongs,
                ["SaveLyricsTo"] = LyricsSaveTo.Normalize(_metadataOpts.CurrentValue.SaveLyricsTo),
                ["PreferOriginalAlbum"] = _metadataOpts.CurrentValue.PreferOriginalAlbum,
                ["YearFromOriginalRelease"] = _metadataOpts.CurrentValue.YearFromOriginalRelease,
                ["PreferredCountries"] = _metadataOpts.CurrentValue.PreferredCountries ?? "",
                ["ReleaseDetailsLookup"] = _metadataOpts.CurrentValue.ReleaseDetailsLookup,
                ["ReplayGain"] = _metadataOpts.CurrentValue.ReplayGain,
                ["ReplayGainTimeoutSeconds"] = _metadataOpts.CurrentValue.ReplayGainTimeoutSeconds,
                ["TagRehearsal"] = _metadataOpts.CurrentValue.TagRehearsal,
            },
            ["GeneratedPlaylists"] = new Dictionary<string, object>
            {
                ["Enabled"] = mixes.Enabled,
                ["Genres"] = mixes.Genres,
                ["Decades"] = mixes.Decades,
                ["TrackCount"] = mixes.TrackCount,
                ["MaxPerArtist"] = mixes.MaxPerArtist,
                ["CreateAt"] = mixes.CreateAt,
                ["RemoveBelow"] = mixes.RemoveBelow,
                ["MaxPlaylists"] = mixes.MaxPlaylists,
                ["RefreshHours"] = mixes.RefreshHours,
                ["NewShare"] = mixes.NewShare,
                ["NewDays"] = mixes.NewDays,
                ["NameFormat"] = mixes.NameFormat ?? "",
            },
            ["Genre"] = new Dictionary<string, object>
            {
                ["Enabled"] = genre.Enabled,
                ["MaxGenres"] = genre.MaxGenres,
                ["OnEmpty"] = genre.OnEmpty.ToString(),
                ["Fallback"] = genre.Fallback.ToString(),
                ["UnknownLabel"] = genre.UnknownLabel ?? "",
                ["Blocklist"] = genre.Blocklist ?? [],
                // Read from the raw file, the same call DiscoveryStations makes above and for
                // the same reason: EffectiveMappings() drops rows, and the editor has to show
                // the user what they typed rather than what survived.
                ["Mappings"] = (_settings.Load()["Genre"] as JsonObject)?["Mappings"]?.DeepClone()
                    ?? JsonSerializer.SerializeToNode(genre.Mappings)!,
                ["BackfillMaxConsecutiveFailures"] = genre.BackfillMaxConsecutiveFailures,
                ["BackfillExtensions"] = genre.BackfillExtensions ?? [],
            },
            ["Notifications"] = new Dictionary<string, object>
            {
                ["NtfyUrl"] = notif.NtfyUrl ?? "",
                ["NtfyToken"] = MaskSecret(notif.NtfyToken),
                // The address carries the webhook's own token, so the whole address is the secret.
                ["DiscordWebhookUrl"] = MaskSecret(notif.DiscordWebhookUrl),
                ["NotifyDownloadStarted"] = notif.NotifyDownloadStarted,
                ["NotifyDownloadCompleted"] = notif.NotifyDownloadCompleted,
                ["NotifyLosslessFallback"] = notif.NotifyLosslessFallback,
                ["NotifyDownloadFailed"] = notif.NotifyDownloadFailed,
                ["NotifyAlbumCompleted"] = notif.NotifyAlbumCompleted,
            },
            ["ListenBrainz"] = new Dictionary<string, object>
            {
                ["Token"] = MaskSecret(_listenBrainzOpts?.CurrentValue.Token),
                ["SubmitExternalPlays"] = _listenBrainzOpts?.CurrentValue.SubmitExternalPlays ?? true,
                ["UserTokens"] = MaskTokens(_listenBrainzOpts?.CurrentValue.UserTokens),
            },
            ["Imports"] = new Dictionary<string, object>
            {
                ["SpotifyClientId"] = ImportOptions.SpotifyClientId ?? "",
                ["SpotifyRedirectUri"] = ImportOptions.EffectiveRedirectUri,
                ["SongsPerHour"] = ImportOptions.SongsPerHour,
                ["RefreshHours"] = ImportOptions.RefreshHours,
            },
            ["_meta"] = new Dictionary<string, object>
            {
                ["ConfigFilePath"] = _settings.FilePath,
                // Drives the "Forget rejected peers" button's label, so an empty list is
                // visibly empty rather than a button that looks like it did nothing.
                ["RejectedPeerCount"] = _rejectedPeers.Count,
                ["ConfigFileExists"] = System.IO.File.Exists(_settings.FilePath),
                // False when the file is there but unparseable, which is when saves are refused.
                ["ConfigFileValid"] = _settings.IsReadable(),
                // Restart-only settings whose saved value has not reached the running services.
                ["RestartPending"] = _restartTracker.Pending(_config),
                // What a saved-but-hidden secret reads as, so the dashboard can recognise it.
                ["SecretPlaceholder"] = SecretPlaceholder,
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

        if (patch["LibraryActions"] is JsonObject actionsPatch)
        {
            var actionsError = ValidateLibraryActions(actionsPatch, _libraryActionOpts.CurrentValue);
            if (actionsError is not null) return BadRequest(new { error = actionsError });
        }

        if (patch["Genre"] is JsonObject genrePatch
            && genrePatch["Mappings"] is JsonArray genreMappings)
        {
            var mappingError = ValidateGenreMappings(genreMappings);
            if (mappingError is not null) return BadRequest(new { error = mappingError });
        }

        if (patch["LastFm"] is JsonObject lastFmPatch
            && lastFmPatch["DiscoveryStations"] is JsonArray discovery)
        {
            var validationError = ValidateDiscoveryStations(discovery);
            if (validationError is not null) return BadRequest(new { error = validationError });
        }

        // The form echoes the placeholder back when the admin password was left alone; that means
        // "keep what is saved", so it must not be written. Anything typed after the placeholder
        // would otherwise be saved as the password.
        // Keys are matched without regard to case, as the configuration system matches them.
        if (Child(patch, "Subsonic") is JsonObject subsonicPatch
            && KeyOf(subsonicPatch, "AdminPassword") is { } passwordKey
            && subsonicPatch[passwordKey] is JsonValue adminPassword
            && adminPassword.TryGetValue<string>(out var adminPasswordText))
        {
            if (adminPasswordText.StartsWith(SecretPlaceholder, StringComparison.Ordinal)
                && adminPasswordText != SecretPlaceholder)
                return BadRequest(new { error = "Retype the whole admin password; it was added to the hidden placeholder." });

            if (adminPasswordText == SecretPlaceholder)
            {
                // A different username with the old password kept is a pairing nobody asked for.
                var newUser = KeyOf(subsonicPatch, "AdminUsername") is { } userKey
                    && subsonicPatch[userKey] is JsonValue userValue
                    && userValue.TryGetValue<string>(out var userText) ? userText : null;
                if (newUser is not null
                    && !string.Equals(newUser.Trim(), (_subsonicOpts.CurrentValue.AdminUsername ?? "").Trim(), StringComparison.Ordinal))
                    return BadRequest(new { error = "Retype the admin password for the new username." });
                subsonicPatch.Remove(passwordKey);
            }
        }

        // The slskd sign-in the same way.
        if (Child(patch, "Soulseek") is JsonObject soulseekPatch
            && KeyOf(soulseekPatch, "Password") is { } slskdPasswordKey
            && soulseekPatch[slskdPasswordKey] is JsonValue slskdPassword
            && slskdPassword.TryGetValue<string>(out var slskdPasswordText)
            && slskdPasswordText.StartsWith(SecretPlaceholder, StringComparison.Ordinal))
        {
            if (slskdPasswordText != SecretPlaceholder)
                return BadRequest(new { error = "Retype the whole slskd password; it was added to the hidden placeholder." });
            var newUser = KeyOf(soulseekPatch, "Username") is { } slskdUserKey
                && soulseekPatch[slskdUserKey] is JsonValue slskdUserValue
                && slskdUserValue.TryGetValue<string>(out var slskdUserText) ? slskdUserText : null;
            if (newUser is not null
                && !string.Equals(newUser.Trim(), (_soulseekOpts.CurrentValue.Username ?? "").Trim(), StringComparison.Ordinal))
                return BadRequest(new { error = "Retype the slskd password for the new username." });
            soulseekPatch.Remove(slskdPasswordKey);
        }

        // Open slskd puts this address in a link, so only a web address is kept.
        if (Child(patch, "Soulseek") is JsonObject webPatch
            && KeyOf(webPatch, "WebUrl") is { } webKey
            && webPatch[webKey] is JsonValue webValue
            && webValue.TryGetValue<string>(out var webText)
            && !string.IsNullOrWhiteSpace(webText)
            && SoulseekSettings.SafeWebUrl(webText) is null)
            return BadRequest(new { error = "slskd's page must be a whole http:// or https:// address, such as http://192.168.1.5:5030." });

        // Sessions are made by Connect and removed by Disconnect. No form sends them, and an
        // echoed placeholder must never overwrite a real key.
        if (Child(patch, "LastFm") is JsonObject lastFmSecrets && KeyOf(lastFmSecrets, "UserSessions") is { } sessionsKey)
            lastFmSecrets.Remove(sessionsKey);
        // Every other credential the same way: the placeholder sent back keeps what is saved.
        if (KeepSavedSecrets(patch, _listenBrainzOpts?.CurrentValue.UserTokens) is { } secretError)
            return BadRequest(new { error = secretError });

        try
        {
            // UserTokens is a dictionary: merged, a user removed in the dashboard would stay.
            var merged = _settings.Merge(patch, ["ListenBrainz.UserTokens"]);
            _logger.LogInformation("Admin settings updated: {Keys}",
                string.Join(",", patch.Select(kv => kv.Key)));
            return new JsonResult(new { ok = true, persisted = RedactSecrets(merged) });
        }
        catch (SettingsFileCorruptException ex)
        {
            _logger.LogWarning("Refused to save settings: {Path} is not valid JSON", ex.Path);
            return Conflict(new { error = $"{ex.Message} Fix it in Raw config, or on disk at {ex.Path}." });
        }
        catch (Exception ex)
        {
            _logger.LogError(ex, "Failed to persist settings to {Path}", _settings.FilePath);
            return StatusCode(500, new { error = ex.Message });
        }
    }

    /// <summary>
    /// The POST path is untyped, so nothing checks a mapping table's shape unless this does.
    /// Mirrors ValidateDiscoveryStations, and is the second of three layers: the editor
    /// validates in the browser and EffectiveMappings() sanitises again at read time.
    /// </summary>
    /// <summary>
    /// The one rule with teeth: the dashboard must not be able to produce a live but
    /// unrestricted configuration. An empty allowlist means nobody, so enabling the feature
    /// without naming anyone is a mistake rather than a permissive choice.
    /// </summary>
    internal static string? ValidateLibraryActions(JsonObject actions, LibraryActionSettings current)
    {
        var enabled = actions["Enabled"]?.GetValue<bool>() ?? current.Enabled;
        var allowed = actions["AllowedUsers"] as JsonArray;
        var allowedCount = allowed is not null
            ? allowed.Count(entry => !string.IsNullOrWhiteSpace(entry?.GetValue<string>()))
            : (current.AllowedUsers ?? []).Count;

        if (enabled && allowedCount == 0)
            return "Library actions need at least one allowed user. An empty list means nobody.";

        if (allowed is not null && allowed.Count > 50)
            return "LibraryActions.AllowedUsers supports at most 50 entries";

        if (actions["Actions"] is JsonArray definitions)
        {
            if (definitions.Count > 5) return "There are only five library actions";
            var ratings = new HashSet<int>();
            foreach (var node in definitions)
            {
                if (node is not JsonObject definition) return "Every library action must be an object";
                var name = definition["Name"]?.GetValue<string>()?.Trim() ?? "";
                if (name.Length > 80) return $"'{name}' is longer than 80 characters";

                var rating = definition["Rating"]?.GetValue<int>() ?? 0;
                if (rating is < 0 or > 5) return "A star rating must be between 0 and 5";
                if (rating > 0 && !ratings.Add(rating))
                    return $"Two actions both use {rating} star(s); a rating can only mean one thing";
            }
        }
        return null;
    }

    private static string? ValidateGenreMappings(JsonArray mappings)
    {
        if (mappings.Count > 200) return "Genre.Mappings supports at most 200 rules";
        var patterns = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        foreach (var node in mappings)
        {
            if (node is not JsonObject rule) return "Every genre rule must be an object";
            var pattern = rule["Pattern"]?.GetValue<string>()?.Trim() ?? "";
            var genre = rule["Genre"]?.GetValue<string>()?.Trim() ?? "";
            var match = rule["Match"]?.GetValue<string>()?.Trim() ?? "Contains";

            if (pattern.Length is 0 or > 60) return "Every genre rule needs a pattern of at most 60 characters";
            if (!patterns.Add(pattern)) return $"Duplicate genre rule pattern '{pattern}': only the first could ever fire";
            if (genre.Length > 60) return $"The genre for '{pattern}' must be at most 60 characters";
            if (!Enum.TryParse<GenreMatchMode>(match, ignoreCase: true, out _))
                return $"The match mode for '{pattern}' must be Contains or Exact";
        }
        return null;
    }

    /// <summary>
    /// Every file library actions have moved, who asked, and where it went.
    ///
    /// Session-gated, because it lists filenames and usernames. Read-only: there is deliberately
    /// no endpoint here that deletes a quarantined file or applies an action on demand, since
    /// /api/admin has no authentication of its own and those would be the wrong things to leave
    /// reachable. The one exception is the Better quality page's upgrade queue, which needs a
    /// Navidrome admin sign-in and acts as that person, who must be on the allowed list.
    /// </summary>
    [HttpGet("library-actions")]
    public IActionResult GetLibraryActions([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!HasBrowseSession(token))
            return Unauthorized(new { error = "Sign in with your Navidrome admin account first." });

        var settings = _libraryActionOpts.CurrentValue;
        return Ok(new
        {
            settings.Enabled,
            settings.DryRun,
            hasAdminIdentity = _navIdentity.HasAdminIdentity,
            allowedUsers = settings.AllowedUsers ?? [],
            quarantine = Path.Combine(_songPaths.MusicRoot(), settings.EffectiveQuarantineDirectory),
            entries = _libraryActionJournal.Recent(200).Select(entry => new
            {
                action = entry.Action.ToString(),
                entry.NavidromeId,
                entry.Username,
                entry.Title,
                entry.Artist,
                entry.Album,
                state = entry.State.ToString(),
                entry.Detail,
                entry.DryRun,
                entry.SourcePath,
                entry.QuarantinePath,
                resolution = entry.Resolution?.ToString(),
                entry.AtUtc,
                entry.Key,
                // A song removed from disk that a download would refuse; Allow downloading again lifts it.
                blocksDownloads = entry.BlocksDownloads,
            }),
        });
    }

    public sealed record AllowDownloadRequest(string? Key);

    /// <summary>
    /// The history's Allow downloading again: a song removed with Delete from disk may be
    /// downloaded once more. Its file stays in the trash. Gated like the history it sits in.
    /// </summary>
    [HttpPost("library-actions/allow-download")]
    public IActionResult AllowDownload([FromBody] AllowDownloadRequest request,
        [FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!HasBrowseSession(token))
            return Unauthorized(new { error = "Sign in with your Navidrome admin account first." });
        if (string.IsNullOrWhiteSpace(request.Key) || _libraryActionJournal.AllowDownloads(request.Key) == 0)
            return BadRequest(new { error = "That song is not kept from downloading." });
        _libraryActionJournal.Flush();
        return Ok(new { ok = true });
    }

    /// <summary>
    /// What Octo has asked people about (#47, #53) and how they answered, newest first. Gated
    /// like the action history: it names files and people.
    /// </summary>
    [HttpGet("notices")]
    public IActionResult GetNotices([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!HasBrowseSession(token))
            return Unauthorized(new { error = "Sign in with your Navidrome admin account first." });

        return Ok(new
        {
            entries = (_notices?.Recent(200) ?? []).Select(entry => new
            {
                kind = entry.Kind.ToString(),
                entry.Username,
                entry.Artist,
                entry.Title,
                entry.Album,
                state = entry.State.ToString(),
                entry.Reason,
                origin = entry.Origin.ToString(),
                entry.Submitted,
                entry.CreatedUtc,
                entry.ResolvedUtc,
            }),
            duplicateScan = _duplicates?.LastResult,
        });
    }

    /// <summary>
    /// The Duplicates card's "Scan now". Read-only, like a radio refresh, so the admin request
    /// guard is enough; the walk runs in the background and the result shows with the questions.
    /// </summary>
    [HttpPost("duplicates/scan")]
    public IActionResult ScanDuplicates()
    {
        var settings = _libraryActionOpts.CurrentValue;
        if (_duplicates is null || !settings.Enabled || !settings.DuplicatesEnabled)
            return BadRequest(new { error = "Turn on library actions and the Duplicates playlist first." });
        if (!_navIdentity.HasAdminIdentity)
            return BadRequest(new { error = "Octo needs a Navidrome admin credential to read the whole library." });
        _duplicates.RequestScan();
        return Accepted(new { ok = true, queued = true });
    }

    /// <summary>The library Review sweep (#72): how far it has got and why it is waiting. Counts only.</summary>
    [HttpGet("review-sweep")]
    public IActionResult GetReviewSweep() => _reviewSweep is null
        ? NotFound(new { error = "The library check is not available." }) : Ok(_reviewSweep.Status());

    [HttpPost("review-sweep/start")]
    public IActionResult StartReviewSweep()
    {
        var settings = _libraryActionOpts.CurrentValue;
        if (_reviewSweep is null || !settings.Enabled || !settings.ReviewEnabled || settings.EffectiveReviewSweepPerHour == 0)
            return BadRequest(new { error = "Turn on library actions and Review, and set how many songs an hour to check, first." });
        _reviewSweep.SetPaused(false);
        return Accepted(new { ok = true });
    }

    [HttpPost("review-sweep/pause")]
    public IActionResult PauseReviewSweep()
    {
        if (_reviewSweep is null) return NotFound(new { error = "The library check is not available." });
        _reviewSweep.SetPaused(true);
        return Accepted(new { ok = true });
    }

    /// <summary>Check every song again from the start. Songs already asked about stay answered.</summary>
    [HttpPost("review-sweep/reset")]
    public IActionResult ResetReviewSweep()
    {
        if (_reviewSweep is null) return NotFound(new { error = "The library check is not available." });
        _reviewSweep.Reset();
        return Accepted(new { ok = true });
    }

    /// <summary>The weekly upgrade's last run and next one. Times and an outcome only, no file or
    /// person, so the admin request guard is enough, like the duplicate scan.</summary>
    [HttpGet("quality-upgrade")]
    public IActionResult GetQualityUpgrade() =>
        _qualityUpgrade is null ? NotFound() : Ok(_qualityUpgrade.Status());

    private const string SignInFirst = "Sign in with your Navidrome admin account first.";

    // The library's lossy songs, listed from Navidrome. Walking a big library takes a while, so the
    // answer is kept a few minutes; "refresh" asks again.
    private static readonly object LossyLock = new();
    private static (DateTime At, IReadOnlyList<Octo.Services.Library.LibrarySongRow> Rows)? _lossyCache;
    private static readonly TimeSpan LossyFor = TimeSpan.FromMinutes(5);

    /// <summary>
    /// The Better quality page's list: every song in the library that is not lossless, with whether
    /// Octo got it from YouTube, when the weekly upgrade last tried it, and any job for it now.
    /// Session-gated, because it lists paths.
    /// </summary>
    [HttpGet("lossy")]
    public async Task<IActionResult> GetLossy([FromHeader(Name = "X-Octo-Browse-Token")] string? token,
        [FromQuery] string? refresh, CancellationToken ct)
    {
        if (!HasBrowseSession(token)) return Unauthorized(new { error = SignInFirst });
        if (_qualityUpgrade is null) return NotFound();
        if (!_navIdentity.HasAdminIdentity)
            return BadRequest(new { error = "Octo needs a Navidrome admin credential to read the whole library." });

        // "1", "true" or "yes": a bool parameter refused "1" with a bare 400 before this ran.
        var fresh = refresh?.Trim().ToLowerInvariant() is "1" or "true" or "yes";
        IReadOnlyList<Octo.Services.Library.LibrarySongRow> rows;
        lock (LossyLock) rows = !fresh && _lossyCache is { } cached && DateTime.UtcNow - cached.At < LossyFor ? cached.Rows : [];
        if (rows.Count == 0)
        {
            var (songs, complete) = await _qualityUpgrade.ListSongs(ct);
            if (!complete && songs.Count == 0) return StatusCode(502, new { error = "Navidrome did not list the library." });
            rows = songs.Where(song => !Octo.Services.Library.DuplicateScanWorker.IsLosslessFile(song.Suffix, song.BitRate)).ToList();
            lock (LossyLock) _lossyCache = (DateTime.UtcNow, rows);
        }

        // Octo's own record of what it fetched from YouTube, matched by file name, then by path.
        var youTube = _history.GetRecent(int.MaxValue)
            .Where(entry => string.Equals(entry.Source, "YouTube", StringComparison.OrdinalIgnoreCase) && entry.Path.Length > 0)
            .Select(entry => entry.Path.Replace('\\', '/'))
            .GroupBy(path => Path.GetFileName(path), StringComparer.OrdinalIgnoreCase)
            .ToDictionary(group => group.Key, group => group.ToList(), StringComparer.OrdinalIgnoreCase);
        var tried = _qualityUpgrade.Tried().Attempts;
        var jobs = (_upgradeQueue?.Snapshot() ?? []).ToDictionary(job => job.NavidromeId, StringComparer.Ordinal);

        return Ok(new
        {
            total = rows.Count,
            songs = rows.Select(row =>
            {
                var key = Octo.Services.Library.QualityUpgradeWorker.KeyOf(row);
                var relative = key[..key.LastIndexOf('|')];
                var fromYouTube = youTube.TryGetValue(Path.GetFileName(relative), out var named)
                    && named.Any(path => path.EndsWith("/" + relative, StringComparison.OrdinalIgnoreCase) || path == relative);
                return new
                {
                    id = row.Id, title = row.Title, artist = row.Artist, album = row.Album, suffix = row.Suffix,
                    bitRate = row.BitRate, size = row.Size, path = relative, fromYouTube, attemptKey = key,
                    lastTried = tried.TryGetValue(key, out var attempt) ? new { atUtc = attempt.AtUtc, outcome = attempt.Outcome } : null,
                    job = jobs.TryGetValue(row.Id, out var job) ? new { state = job.State, detail = job.Detail } : null,
                };
            }),
        });
    }

    /// <summary>The upgrade queue, how many run at once and why, Soulseek's state, and the gate.</summary>
    [HttpGet("upgrades")]
    public async Task<IActionResult> GetUpgrades([FromHeader(Name = "X-Octo-Browse-Token")] string? token, CancellationToken ct)
    {
        var user = BrowseUser(token);
        if (user is null) return Unauthorized(new { error = SignInFirst });
        var live = (_acquisitions?.All() ?? [])
            .GroupBy(row => $"{row.Provider}:{row.ExternalId}")
            .ToDictionary(group => group.Key, group => group.First());
        var reading = _soulseekLink is null ? null : await _soulseekLink.ReadAsync(fresh: false, ct);
        var (up, warning, detail) = Octo.Services.Soulseek.SoulseekLink.Describe(reading, _soulseekOpts.CurrentValue.EffectiveOutageHoldHours);
        var settings = _libraryActionOpts.CurrentValue;
        return Ok(new
        {
            jobs = (_upgradeQueue?.Snapshot() ?? []).Select(job =>
            {
                // The replacement's own download row, while it runs: which stage, and how far.
                var row = job.AcquisitionKey is { } key && job.State == Octo.Services.Library.UpgradeStates.Working
                    ? live.GetValueOrDefault(key) : null;
                return new
                {
                    id = job.NavidromeId, job.Title, job.Artist, job.Album, job.Suffix, job.State, job.Detail, job.RequestedBy,
                    job.Origin, queuedUtc = job.QueuedUtc, updatedUtc = job.UpdatedUtc, startedUtc = job.StartedUtc,
                    progress = row?.Progress,
                    stage = row?.State.ToString(),
                    source = row?.Source,
                    bytesDone = row?.BytesDone,
                    bytesTotal = row?.BytesTotal,
                    note = row?.Note,
                    result = job.Result,
                };
            }),
            parallel = _downloadConcurrency?.Current ?? 1,
            // Where an upgrade looks, and whether that source is set up, so the page never assumes.
            source = UpgradeSourceName,
            sourceReady = UpgradeSourceReady,
            plan = UpgradeSourcesNow?.Plan().Select(Octo.Services.Library.UpgradeSources.Word).ToList()
                ?? (UpgradeSourceReady ? ["Soulseek"] : []),
            why = _downloadConcurrency?.Why ?? "One at a time.",
            soulseek = new { ok = up, warning, detail },
            gate = new
            {
                user,
                enabled = settings.Enabled,
                allowed = settings.IsAllowed(user),
                dryRun = settings.DryRun,
                betterQuality = settings.EffectiveActions().Any(a => a.Action == LibraryAction.BetterQuality && a.Enabled),
            },
        });
    }

    public sealed record UpgradeQueueRequest(List<Octo.Services.Library.UpgradeAsk>? Songs);
    public sealed record UpgradeIdsRequest(List<string>? Ids);

    /// <summary>Most songs one press of the page's button may queue.</summary>
    internal const int MaxUpgradesPerRequest = 2000;

    /// <summary>
    /// Queue songs for a higher quality copy, acting as the Navidrome admin signed in on this page.
    /// Refused unless every gate of the Better quality action is open for that person.
    /// </summary>
    [HttpPost("upgrades")]
    public IActionResult QueueUpgrades([FromHeader(Name = "X-Octo-Browse-Token")] string? token,
        [FromBody] UpgradeQueueRequest request)
    {
        var user = BrowseUser(token);
        if (user is null) return Unauthorized(new { error = SignInFirst });
        if (_upgradeQueue is null) return NotFound();
        var settings = _libraryActionOpts.CurrentValue;
        if (!settings.IsAllowed(user))
            return StatusCode(403, new { error = $"{user} is not on the library actions allowed list, so Octo will not change files for them." });
        var closed = !UpgradeSourceReady
                ? $"Better quality looks for copies on {UpgradeSourceName}, which is not set up here."
            : !settings.Enabled ? "Turn on library actions first."
            : !settings.EffectiveActions().Any(a => a.Action == LibraryAction.BetterQuality && a.Enabled) ? "Turn on the Better quality action first."
            : settings.DryRun ? "Library actions only rehearse while dry run is on; turn it off first."
            : null;
        if (closed is not null) return BadRequest(new { error = closed });
        var songs = request.Songs ?? [];
        if (songs.Count == 0) return BadRequest(new { error = "No songs picked." });
        if (songs.Count > MaxUpgradesPerRequest)
            return BadRequest(new { error = $"At most {MaxUpgradesPerRequest} songs at a time." });
        var (jobs, refused) = _upgradeQueue.Add(songs, user, "page");
        _logger.LogInformation("{User} queued {Count} songs for higher quality from the dashboard", user, jobs.Count);
        return Accepted(new { ok = true, queued = jobs.Count, refused });
    }

    /// <summary>Take back songs that have not started.</summary>
    [HttpPost("upgrades/cancel")]
    public IActionResult CancelUpgrades([FromHeader(Name = "X-Octo-Browse-Token")] string? token,
        [FromBody] UpgradeIdsRequest request)
    {
        if (!HasBrowseSession(token)) return Unauthorized(new { error = SignInFirst });
        if (_upgradeQueue is null) return NotFound();
        return Ok(new { ok = true, cancelled = _upgradeQueue.Cancel(request.Ids ?? []) });
    }

    /// <summary>Forget finished jobs.</summary>
    [HttpPost("upgrades/clear")]
    public IActionResult ClearUpgrades([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!HasBrowseSession(token)) return Unauthorized(new { error = SignInFirst });
        if (_upgradeQueue is null) return NotFound();
        return Ok(new { ok = true, cleared = _upgradeQueue.ClearFinished() });
    }

    /// <summary>
    /// Ask what file a Navidrome song id resolves to, and say which leg answered.
    ///
    /// Read-only, and deliberately shipped before anything that acts on the answer: Navidrome's
    /// Subsonic `path` is synthesised from tags unless a player opts in, so on some libraries it
    /// names a file that exists and is a different recording. This is how that gets checked
    /// against a real library before any of it is load-bearing.
    ///
    /// Session-gated because the answer is a filesystem path.
    /// </summary>
    [HttpGet("library/resolve")]
    public async Task<IActionResult> ResolveLibrarySong([FromQuery] string? id,
        [FromHeader(Name = "X-Octo-Browse-Token")] string? token, CancellationToken ct)
    {
        if (!HasBrowseSession(token))
            return Unauthorized(new { error = "Sign in with your Navidrome admin account first." });
        if (string.IsNullOrWhiteSpace(id))
            return BadRequest(new { error = "Pass the Navidrome song id as ?id=" });

        var resolved = await _songPaths.ResolveAsync(id, ct);
        return Ok(new
        {
            id,
            resolved = resolved is not null,
            musicRoot = _songPaths.MusicRoot(),
            hasAdminIdentity = _navIdentity.HasAdminIdentity,
            path = resolved?.AbsolutePath,
            source = resolved?.Source.ToString() ?? PathSource.None.ToString(),
            sizeBytes = resolved?.SizeBytes,
            artist = resolved?.Artist,
            title = resolved?.Title,
            album = resolved?.Album,
        });
    }

    /// <summary>
    /// One place that answers "is this caller a verified Navidrome admin?".
    ///
    /// Cookie first, the way the dashboard authenticates; header second so the endpoint stays
    /// usable from curl. /api/admin has no authentication of its own, so every endpoint below
    /// that rewrites or deletes a tag is gated on this rather than being the second
    /// unauthenticated destructive surface.
    /// </summary>
    private bool HasBrowseSession(string? headerToken) => BrowseUser(headerToken) is not null;

    public sealed record GenreBackfillStartRequest(string? Scope, bool DryRun, string? Confirm);

    /// <summary>
    /// Start a backfill, or a preview of one.
    ///
    /// DryRun is the default path in the UI: the button says "Preview changes" and only the
    /// preview screen offers to apply. A whole-library APPLY additionally requires the caller
    /// to type the library path back, because that run rewrites files Octo never created and a
    /// drive-by POST must not be able to start it.
    /// </summary>
    [HttpPost("genre/backfill")]
    public IActionResult StartGenreBackfill([FromBody] GenreBackfillStartRequest request,
        [FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!HasBrowseSession(token))
            return Unauthorized(new { error = "Sign in with your Navidrome admin account first." });

        if (!Enum.TryParse<GenreBackfillScope>(request.Scope, ignoreCase: true, out var scope))
            scope = GenreBackfillScope.OctoDownloads;

        if (!_genreOpts.CurrentValue.Enabled)
            return BadRequest(new { error = "Turn genre normalization on first, or a run would change nothing." });

        var root = _config["Library:DownloadPath"] ?? "./downloads";
        if (scope == GenreBackfillScope.WholeLibrary && !request.DryRun
            && !string.Equals(request.Confirm?.Trim(), root, StringComparison.Ordinal))
            return BadRequest(new { error = $"To rewrite the whole library, type the music path exactly: {root}" });

        if (!_genreBackfill.TryEnqueue(new GenreBackfillRequest(scope, request.DryRun)))
            return Conflict(new { error = "A genre backfill is already running." });

        _logger.LogInformation("Genre backfill requested: scope {Scope}, dryRun {DryRun}", scope, request.DryRun);
        return Accepted(new { started = true, scope = scope.ToString(), dryRun = request.DryRun });
    }

    [HttpGet("genre/backfill")]
    public IActionResult GetGenreBackfill([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!HasBrowseSession(token))
            return Unauthorized(new { error = "Sign in with your Navidrome admin account first." });

        var run = _genreBackfill.Current;
        return Ok(new
        {
            run.RunId,
            status = run.Status.ToString(),
            scope = run.Scope.ToString(),
            run.DryRun,
            run.StartedUtc,
            run.FinishedUtc,
            run.Total,
            run.Processed,
            run.Changed,
            run.Cleared,
            run.Skipped,
            run.Failed,
            run.LastPath,
            run.Reason,
            run.Errors,
            run.Preview,
            canResume = run.CanResume,
            canUndo = _genreJournal.Exists,
            // The genre rules changed after this run was planned. Apply re-plans from the current
            // rules, so a preview in this state no longer describes what Apply would write.
            settingsChanged = run.SettingsHash is not null
                && run.SettingsHash != Octo.Services.Metadata.GenreBackfillWorker.HashSettings(_genreOpts.CurrentValue),
            musicPath = _config["Library:DownloadPath"] ?? "./downloads",
        });
    }

    [HttpPost("genre/backfill/cancel")]
    public IActionResult CancelGenreBackfill([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!HasBrowseSession(token))
            return Unauthorized(new { error = "Sign in with your Navidrome admin account first." });

        _genreBackfill.RequestCancel();
        return Accepted(new { cancelling = true });
    }

    [HttpPost("genre/backfill/resume")]
    public IActionResult ResumeGenreBackfill([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!HasBrowseSession(token))
            return Unauthorized(new { error = "Sign in with your Navidrome admin account first." });

        var run = _genreBackfill.Current;
        if (!run.CanResume) return BadRequest(new { error = "There is nothing to resume." });
        // A run's settings are snapshotted when it starts. Resuming under edited rules would
        // finish the library under a different table from the one it started with.
        if (run.SettingsHash is not null
            && run.SettingsHash != Octo.Services.Metadata.GenreBackfillWorker.HashSettings(_genreOpts.CurrentValue))
            return Conflict(new { error = "The genre rules changed since this run started. Preview again instead of resuming." });
        if (!_genreBackfill.TryEnqueue(new GenreBackfillRequest(run.Scope, run.DryRun)))
            return Conflict(new { error = "A genre backfill is already running." });

        return Accepted(new { resumed = true });
    }

    /// <summary>
    /// Put every genre frame the last backfill changed back the way it was.
    ///
    /// The journal is the only real undo, and it covers the genre frame only. TagLib's Save()
    /// rewrites the whole tag block, so anything it does not round-trip was lost on the first
    /// save; entries are keyed by path, so a moved file stays rewritten; and if the journal is
    /// gone there is no undo at all. The dashboard says all three next to the button.
    /// </summary>
    [HttpPost("genre/backfill/undo")]
    public IActionResult UndoGenreBackfill([FromHeader(Name = "X-Octo-Browse-Token")] string? token)
    {
        if (!HasBrowseSession(token))
            return Unauthorized(new { error = "Sign in with your Navidrome admin account first." });

        if (!_genreJournal.Exists) return BadRequest(new { error = "There is no backfill to undo." });
        if (!_genreBackfill.TryEnqueue(new GenreBackfillRequest(GenreBackfillScope.WholeLibrary, DryRun: false, Undo: true)))
            return Conflict(new { error = "A genre backfill is already running." });

        _logger.LogInformation("Genre backfill undo requested");
        return Accepted(new { started = true });
    }

    /// <summary>
    /// The broad-genre preset, served from the one place it is defined. A second copy in
    /// admin.js would be a table the dashboard and the tests could disagree about.
    /// </summary>
    [HttpGet("genre/presets")]
    public IActionResult GetGenrePresets() => Ok(new Dictionary<string, object>
    {
        // Dictionary and an explicit projection, not Ok(new { ... }): the default serializer
        // camelCases property names and writes an enum as a NUMBER, and this payload is posted
        // straight back to the settings API, which reads exact PascalCase and an enum NAME.
        // Emitted any other way the shipped preset cannot be saved, which is how it shipped
        // the first time.
        ["broad"] = GenreSettings.BroadGenrePreset().Select(rule => new Dictionary<string, object>
        {
            ["Id"] = rule.Id,
            ["Pattern"] = rule.Pattern,
            ["Genre"] = rule.Genre,
            ["Match"] = rule.Match.ToString(),
            ["Enabled"] = rule.Enabled,
        }).ToList(),
    });

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
        var genre = _genreOpts.CurrentValue;
        var actions = _libraryActionOpts.CurrentValue;
        var mixes = _generatedOpts?.CurrentValue ?? new GeneratedPlaylistSettings();
        var notif = _notificationOpts.CurrentValue;
        var server = _serverOpts.CurrentValue;

        var effective = new JsonObject
        {
            ["Subsonic"] = new JsonObject
            {
                ["Url"] = subsonic.Url ?? "",
                // Listed for the same reason as every other key here: PUT writes this document
                // wholesale, so a key missing from it is a key a plain Save deletes. The password
                // is the placeholder, which PUT swaps back for the stored value.
                ["AdminUsername"] = subsonic.AdminUsername ?? "",
                ["AdminPassword"] = MaskSecret(subsonic.AdminPassword),
                ["EnableSearchDiscovery"] = subsonic.EnableSearchDiscovery,
                ["WaitForSearchDurations"] = subsonic.WaitForSearchDurations,
                ["EnableSyncCatalog"] = subsonic.EnableSyncCatalog,
                ["SyncCatalogClients"] = subsonic.SyncCatalogClients,
                ["SyncCatalogMaxSongs"] = subsonic.SyncCatalogMaxSongs,
                ["StorageMode"] = subsonic.StorageMode.ToString(),
                ["DownloadMode"] = subsonic.DownloadMode.ToString(),
                ["DownloadOnStar"] = subsonic.DownloadOnStar,
                ["DownloadAlbumOnStar"] = subsonic.DownloadAlbumOnStar,
                ["RecordRequestedBy"] = subsonic.RecordRequestedBy,
                ["StarDownloadsForRequester"] = subsonic.StarDownloadsForRequester,
                ["SkipOwnedSongs"] = subsonic.SkipOwnedSongs,
                ["WaitForLosslessOnPlay"] = subsonic.WaitForLosslessOnPlay,
                ["LosslessWaitTimeoutSeconds"] = subsonic.LosslessWaitTimeoutSeconds,
                ["DownloadOnPlay"] = subsonic.DownloadOnPlay,
                ["LidarrAlbumOnPlay"] = subsonic.LidarrAlbumOnPlay,
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
            // Must be listed here even though nothing reads it back: PUT writes
            // this document wholesale, so a section missing from the GET is a
            // section the next plain Save silently deletes.
            ["Server"] = new JsonObject
            {
                ["PublicUrl"] = server.PublicUrl ?? "",
            },
            ["Updates"] = new JsonObject
            {
                ["Check"] = UpdateOptions.Check,
                ["Repo"] = UpdateOptions.Repo ?? "",
            },
            ["Soulseek"] = new JsonObject
            {
                ["BaseUrl"] = soulseek.BaseUrl ?? "",
                ["Username"] = soulseek.Username ?? "",
                ["Password"] = MaskSecret(soulseek.Password),
                ["WebUrl"] = soulseek.WebUrl ?? "",
                ["CheckListenPort"] = soulseek.CheckListenPort,
                ["SearchWaitSeconds"] = soulseek.SearchWaitSeconds,
                ["UpgradeSearchWaitSeconds"] = soulseek.UpgradeSearchWaitSeconds,
                ["MinFileSizeBytes"] = soulseek.MinFileSizeBytes,
                ["PreferredExtension"] = soulseek.PreferredExtension,
                ["DownloadTimeoutSeconds"] = soulseek.DownloadTimeoutSeconds,
                ["VerifyDownloads"] = soulseek.VerifyDownloads,
                ["AcoustIdApiKey"] = MaskSecret(soulseek.AcoustIdApiKey),
                ["MinMatchScore"] = soulseek.MinMatchScore,
                ["TagFromMusicBrainz"] = soulseek.TagFromMusicBrainz,
                ["NameFromMatch"] = soulseek.NameFromMatch,
                ["RejectedPeerTtlDays"] = soulseek.RejectedPeerTtlDays,
                ["FingerprintSeconds"] = soulseek.FingerprintSeconds,
                ["FingerprintTimeoutSeconds"] = soulseek.FingerprintTimeoutSeconds,
                ["AcoustIdTimeoutSeconds"] = soulseek.AcoustIdTimeoutSeconds,
                ["DetectTranscodes"] = soulseek.DetectTranscodes,
                ["TranscodeCheckTimeoutSeconds"] = soulseek.TranscodeCheckTimeoutSeconds,
                ["OutageHoldHours"] = soulseek.OutageHoldHours,
                ["ParallelDownloads"] = soulseek.ParallelDownloads,
                ["AlbumFolders"] = soulseek.AlbumFolders,
                ["SubmitConfirmedFingerprints"] = soulseek.SubmitConfirmedFingerprints,
                ["AcoustIdUserApiKey"] = MaskSecret(soulseek.AcoustIdUserApiKey),
            },
            ["Lidarr"] = new JsonObject
            {
                ["BaseUrl"] = lidarr.BaseUrl ?? "",
                ["ApiKey"] = MaskSecret(lidarr.ApiKey),
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
                ["ApiKey"] = MaskSecret(lastfm.ApiKey),
                // Placeholders, which PUT swaps back for what is stored.
                ["ApiSecret"] = MaskSecret(lastfm.ApiSecret),
                ["ScrobbleExternalPlays"] = lastfm.ScrobbleExternalPlays,
                ["ScrobbleLibraryPlays"] = lastfm.ScrobbleLibraryPlays,
                ["UserSessions"] = MaskSessions(lastfm.UserSessions),
                ["EnableRadio"] = lastfm.EnableRadio,
                ["RadioTrackCount"] = lastfm.RadioTrackCount,
                ["RadioCacheDurationHours"] = lastfm.RadioCacheDurationHours,
                ["EnablePersonalizedStations"] = lastfm.EnablePersonalizedStations,
                ["EnableYourMix"] = lastfm.EnableYourMix,
                ["EnableDiscoveryMix"] = lastfm.EnableDiscoveryMix,
                ["ArtistStationCount"] = lastfm.ArtistStationCount,
                ["GenreStationCount"] = lastfm.GenreStationCount,
                ["EnableDiscoveryStations"] = lastfm.EnableDiscoveryStations,
                ["ExposeRadioAsPlaylists"] = lastfm.ExposeRadioAsPlaylists,
                ["ExposeRadioAsStreams"] = lastfm.ExposeRadioAsStreams,
                ["RadioStreamBitrateKbps"] = lastfm.RadioStreamBitrateKbps,
                ["EnableIcyMetadata"] = lastfm.EnableIcyMetadata,
                ["StarterPublishTimeoutSeconds"] = lastfm.StarterPublishTimeoutSeconds,
                ["RadioLoudnessTargetLufs"] = lastfm.RadioLoudnessTargetLufs,
                ["HistoryRetentionDays"] = lastfm.HistoryRetentionDays,
                ["DiscoveryPercent"] = lastfm.DiscoveryPercent,
                ["RefreshIntervalHours"] = lastfm.RefreshIntervalHours,
                ["MinimumPlays"] = lastfm.MinimumPlays,
                ["DiscoveryStations"] = JsonSerializer.SerializeToNode(lastfm.DiscoveryStations),
            },
            ["LibraryActions"] = new JsonObject
            {
                ["Enabled"] = actions.Enabled,
                ["PlaylistsEnabled"] = actions.PlaylistsEnabled,
                ["RatingsEnabled"] = actions.RatingsEnabled,
                ["PlaylistPrefix"] = actions.PlaylistPrefix ?? "",
                ["DryRun"] = actions.DryRun,
                ["QuarantineDirectory"] = actions.QuarantineDirectory ?? "",
                ["QuarantineRetentionDays"] = actions.QuarantineRetentionDays,
                ["PollIntervalSeconds"] = actions.PollIntervalSeconds,
                ["MaxActionsPerCycle"] = actions.MaxActionsPerCycle,
                ["KeepReplacedOriginals"] = actions.KeepReplacedOriginals,
                ["NoticePrefix"] = actions.NoticePrefix ?? "",
                ["ReviewEnabled"] = actions.ReviewEnabled,
                ["ReviewPlaylistName"] = actions.ReviewPlaylistName ?? "",
                ["ReviewSweepPerHour"] = actions.ReviewSweepPerHour,
                ["ReviewSweepOctoDownloads"] = actions.ReviewSweepOctoDownloads,
                ["DuplicatesEnabled"] = actions.DuplicatesEnabled,
                ["DuplicatesPlaylistName"] = actions.DuplicatesPlaylistName ?? "",
                ["DuplicatesScanHours"] = actions.DuplicatesScanHours,
                ["UpgradePerWeek"] = actions.UpgradePerWeek,
                ["UpgradeSource"] = actions.UpgradeSource.ToString(),
                ["NoticeMaxTracks"] = actions.NoticeMaxTracks,
                ["RatingsScope"] = actions.RatingsScope.ToString(),
                ["AllowedUsers"] = JsonSerializer.SerializeToNode(actions.AllowedUsers ?? [])!,
                // Effective rather than raw, because the editor needs every action present
                // even when the config names only some of them. Projected so the enum lands as
                // its NAME: serialized directly it becomes a number, and the editor looks the
                // action up by name to label the row.
                ["Actions"] = JsonSerializer.SerializeToNode(actions.EffectiveActions()
                    .Select(action => new
                    {
                        Action = action.Action.ToString(),
                        action.Name,
                        action.Enabled,
                        action.Rating,
                    }))!,
            },
            ["Metadata"] = new JsonObject
            {
                ["Language"] = _metadataOpts.CurrentValue.Language ?? "",
                ["AlbumFromTitle"] = _metadataOpts.CurrentValue.AlbumFromTitle,
                ["UseCoverArtArchive"] = _metadataOpts.CurrentValue.UseCoverArtArchive,
                ["ReplaceVideoCovers"] = _metadataOpts.CurrentValue.ReplaceVideoCovers,
                ["WriteCoverFile"] = _metadataOpts.CurrentValue.WriteCoverFile,
                ["EmbedFullSizeCovers"] = _metadataOpts.CurrentValue.EmbedFullSizeCovers,
                ["FetchLyrics"] = _metadataOpts.CurrentValue.FetchLyrics,
                ["LyricsSources"] = _metadataOpts.CurrentValue.LyricsSources ?? "",
                ["PreferWordTimedLyrics"] = _metadataOpts.CurrentValue.PreferWordTimedLyrics,
                ["WriteLyricsBesideAllSongs"] = _metadataOpts.CurrentValue.WriteLyricsBesideAllSongs,
                ["SaveLyricsTo"] = LyricsSaveTo.Normalize(_metadataOpts.CurrentValue.SaveLyricsTo),
                ["PreferOriginalAlbum"] = _metadataOpts.CurrentValue.PreferOriginalAlbum,
                ["YearFromOriginalRelease"] = _metadataOpts.CurrentValue.YearFromOriginalRelease,
                ["PreferredCountries"] = _metadataOpts.CurrentValue.PreferredCountries ?? "",
                ["ReleaseDetailsLookup"] = _metadataOpts.CurrentValue.ReleaseDetailsLookup,
                ["ReplayGain"] = _metadataOpts.CurrentValue.ReplayGain,
                ["ReplayGainTimeoutSeconds"] = _metadataOpts.CurrentValue.ReplayGainTimeoutSeconds,
                ["TagRehearsal"] = _metadataOpts.CurrentValue.TagRehearsal,
            },
            ["GeneratedPlaylists"] = new JsonObject
            {
                ["Enabled"] = mixes.Enabled,
                ["Genres"] = mixes.Genres,
                ["Decades"] = mixes.Decades,
                ["TrackCount"] = mixes.TrackCount,
                ["MaxPerArtist"] = mixes.MaxPerArtist,
                ["CreateAt"] = mixes.CreateAt,
                ["RemoveBelow"] = mixes.RemoveBelow,
                ["MaxPlaylists"] = mixes.MaxPlaylists,
                ["RefreshHours"] = mixes.RefreshHours,
                ["NewShare"] = mixes.NewShare,
                ["NewDays"] = mixes.NewDays,
                ["NameFormat"] = mixes.NameFormat ?? "",
            },
            ["Genre"] = new JsonObject
            {
                ["Enabled"] = genre.Enabled,
                ["MaxGenres"] = genre.MaxGenres,
                ["OnEmpty"] = genre.OnEmpty.ToString(),
                ["Fallback"] = genre.Fallback.ToString(),
                ["UnknownLabel"] = genre.UnknownLabel ?? "",
                ["Blocklist"] = JsonSerializer.SerializeToNode(genre.Blocklist ?? [])!,
                ["Mappings"] = (_settings.Load()["Genre"] as JsonObject)?["Mappings"]?.DeepClone()
                    ?? JsonSerializer.SerializeToNode(genre.Mappings)!,
                ["BackfillMaxConsecutiveFailures"] = genre.BackfillMaxConsecutiveFailures,
                ["BackfillExtensions"] = JsonSerializer.SerializeToNode(genre.BackfillExtensions ?? [])!,
            },
            ["Notifications"] = new JsonObject
            {
                ["NtfyUrl"] = notif.NtfyUrl ?? "",
                ["NtfyToken"] = MaskSecret(notif.NtfyToken),
                // The address carries the webhook's own token, so the whole address is the secret.
                ["DiscordWebhookUrl"] = MaskSecret(notif.DiscordWebhookUrl),
                ["NotifyDownloadStarted"] = notif.NotifyDownloadStarted,
                ["NotifyDownloadCompleted"] = notif.NotifyDownloadCompleted,
                ["NotifyLosslessFallback"] = notif.NotifyLosslessFallback,
                ["NotifyDownloadFailed"] = notif.NotifyDownloadFailed,
                ["NotifyAlbumCompleted"] = notif.NotifyAlbumCompleted,
            },
            // Present even when unset: a section missing from this document is a
            // section the next Raw Config save silently deletes.
            ["ListenBrainz"] = new JsonObject
            {
                ["Token"] = MaskSecret(_listenBrainzOpts?.CurrentValue.Token),
                ["SubmitExternalPlays"] = _listenBrainzOpts?.CurrentValue.SubmitExternalPlays ?? true,
                ["UserTokens"] = new JsonObject(MaskTokens(_listenBrainzOpts?.CurrentValue.UserTokens)
                    .Select(pair => new KeyValuePair<string, JsonNode?>(pair.Key, pair.Value))),
            },
            ["Imports"] = new JsonObject
            {
                ["SpotifyClientId"] = ImportOptions.SpotifyClientId ?? "",
                ["SpotifyRedirectUri"] = ImportOptions.EffectiveRedirectUri,
                ["SongsPerHour"] = ImportOptions.SongsPerHour,
                ["RefreshHours"] = ImportOptions.RefreshHours,
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
            // Don't merge: this is the "I know exactly what I want" power-user endpoint. It goes
            // through the writer so it shares the lock and atomic write with form saves, and the
            // hidden admin password comes back as a placeholder that must not be saved literally.
            // When the file cannot be read, the running value is the only copy of the password
            // left, and this save is the recovery path the 409 points people to.
            var existing = _settings.IsReadable()
                ? _settings.Load()
                : RunningSecrets();
            RestoreSecretPlaceholders(parsed, existing);
            if (SecretTypedIntoPlaceholder(parsed) is { } typedSecret)
                return BadRequest(new { error = $"Retype the whole {typedSecret}; it was added to the hidden placeholder." });
            if (SessionKeyTypedIntoPlaceholder(parsed) is { } typedInto)
                return BadRequest(new { error = $"Connect {typedInto} to Last.fm again, or paste their whole session key; it was added to the hidden placeholder." });
            var pretty = parsed.ToJsonString(new JsonSerializerOptions { WriteIndented = true });
            _settings.Replace(parsed);
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
            "Subsonic:RecordRequestedBy", "Subsonic:StarDownloadsForRequester",
            "Subsonic:WaitForLosslessOnPlay", "Subsonic:LosslessWaitTimeoutSeconds",
            "Subsonic:DownloadOnPlay", "Subsonic:LidarrAlbumOnPlay",
            "Subsonic:DownloadSource", "Subsonic:AutoDetectDownloadPath", "Subsonic:LibraryPath",
            "Subsonic:FolderStructure",
            "Subsonic:UseLocalStaging", "Subsonic:ExplicitFilter",
            "Subsonic:CacheDurationHours", "Subsonic:EnableExternalPlaylists",
            "Subsonic:WaitForSearchDurations", "Subsonic:SkipOwnedSongs",
            "Subsonic:PlaylistsDirectory",
            "Library:DownloadPath",
            "Server:PublicUrl",
            "Updates:Check", "Updates:Repo",
            "Soulseek:BaseUrl", "Soulseek:Username", "Soulseek:Password", "Soulseek:WebUrl", "Soulseek:CheckListenPort",
            "Soulseek:SearchWaitSeconds", "Soulseek:UpgradeSearchWaitSeconds", "Soulseek:MinFileSizeBytes",
            "Soulseek:PreferredExtension", "Soulseek:DownloadTimeoutSeconds",
            "Soulseek:RejectedPeerTtlDays", "Soulseek:FingerprintSeconds",
            "Soulseek:FingerprintTimeoutSeconds", "Soulseek:AcoustIdTimeoutSeconds",
            "Soulseek:DetectTranscodes", "Soulseek:TranscodeCheckTimeoutSeconds",
            "Soulseek:OutageHoldHours", "Soulseek:ParallelDownloads", "Soulseek:AlbumFolders",
            "Genre:BackfillMaxConsecutiveFailures", "Genre:BackfillExtensions",
            "LibraryActions:Enabled", "LibraryActions:PlaylistsEnabled",
            "LibraryActions:RatingsEnabled", "LibraryActions:PlaylistPrefix",
            "LibraryActions:DryRun", "LibraryActions:QuarantineDirectory",
            "LibraryActions:QuarantineRetentionDays", "LibraryActions:PollIntervalSeconds",
            "LibraryActions:MaxActionsPerCycle", "LibraryActions:KeepReplacedOriginals",
            "LibraryActions:Actions", "LibraryActions:AllowedUsers",
            "LibraryActions:NoticePrefix", "LibraryActions:ReviewEnabled",
            "LibraryActions:ReviewPlaylistName", "LibraryActions:NoticeMaxTracks",
            "LibraryActions:RatingsScope", "LibraryActions:DuplicatesEnabled",
            "LibraryActions:DuplicatesPlaylistName", "LibraryActions:DuplicatesScanHours",
            "LibraryActions:UpgradePerWeek", "LibraryActions:UpgradeSource",
            "LibraryActions:ReviewSweepPerHour", "LibraryActions:ReviewSweepOctoDownloads",
            "Soulseek:SubmitConfirmedFingerprints", "Soulseek:AcoustIdUserApiKey",
            "Genre:Enabled", "Genre:MaxGenres", "Genre:OnEmpty", "Genre:Fallback",
            "Genre:UnknownLabel", "Genre:Mappings", "Genre:Blocklist",
            "Soulseek:VerifyDownloads", "Soulseek:AcoustIdApiKey",
            "Soulseek:MinMatchScore", "Soulseek:TagFromMusicBrainz", "Soulseek:NameFromMatch",
            "Lidarr:BaseUrl", "Lidarr:ApiKey", "Lidarr:RootFolderPath",
            "Lidarr:QualityProfileId", "Lidarr:MetadataProfileId",
            "Lidarr:CompletionMode", "Lidarr:ImportTimeoutSeconds",
            "YouTube:ShimUrl",
            "LastFm:ApiKey", "LastFm:ApiSecret", "LastFm:ScrobbleExternalPlays", "LastFm:ScrobbleLibraryPlays",
            "LastFm:EnableRadio", "LastFm:RadioTrackCount",
            "LastFm:RadioCacheDurationHours", "LastFm:StarterPublishTimeoutSeconds",
            "LastFm:RadioLoudnessTargetLufs",
            "LastFm:EnablePersonalizedStations", "LastFm:EnableYourMix",
            "LastFm:EnableDiscoveryMix", "LastFm:ArtistStationCount",
            "LastFm:GenreStationCount", "LastFm:EnableDiscoveryStations",
            "LastFm:HistoryRetentionDays", "LastFm:DiscoveryPercent",
            "LastFm:RefreshIntervalHours",
            "LastFm:MinimumPlays", "LastFm:DiscoveryStations",
            "Metadata:Language", "Metadata:AlbumFromTitle", "Metadata:UseCoverArtArchive",
            "Metadata:ReplaceVideoCovers", "Metadata:WriteCoverFile", "Metadata:EmbedFullSizeCovers",
            "Metadata:FetchLyrics", "Metadata:LyricsSources", "Metadata:PreferWordTimedLyrics",
            "Metadata:WriteLyricsBesideAllSongs", "Metadata:SaveLyricsTo",
            "Metadata:PreferOriginalAlbum", "Metadata:YearFromOriginalRelease", "Metadata:PreferredCountries",
            "Metadata:ReleaseDetailsLookup", "Metadata:ReplayGain", "Metadata:ReplayGainTimeoutSeconds",
            "Metadata:TagRehearsal",
            "GeneratedPlaylists:Enabled", "GeneratedPlaylists:Genres", "GeneratedPlaylists:Decades",
            "GeneratedPlaylists:TrackCount", "GeneratedPlaylists:MaxPerArtist", "GeneratedPlaylists:CreateAt",
            "GeneratedPlaylists:RemoveBelow", "GeneratedPlaylists:MaxPlaylists", "GeneratedPlaylists:RefreshHours",
            "GeneratedPlaylists:NewShare", "GeneratedPlaylists:NewDays", "GeneratedPlaylists:NameFormat",
            "Notifications:NtfyUrl", "Notifications:NtfyToken",
            "Notifications:DiscordWebhookUrl",
            "Notifications:NotifyDownloadStarted", "Notifications:NotifyDownloadCompleted",
            "Notifications:NotifyLosslessFallback", "Notifications:NotifyDownloadFailed",
            "Notifications:NotifyAlbumCompleted",
            "ListenBrainz:Token", "ListenBrainz:SubmitExternalPlays",
            "Imports:SpotifyClientId", "Imports:SpotifyRedirectUri", "Imports:SongsPerHour", "Imports:RefreshHours",
        };
        var rows = new List<object>();
        foreach (var k in keys)
        {
            var v = _config[k] ?? "";
            // Mask anything that smells like a secret so a screenshot of the
            // page doesn't leak credentials.
            var isSecret = k.EndsWith("Password", StringComparison.OrdinalIgnoreCase)
                        || k.EndsWith("ApiKey", StringComparison.OrdinalIgnoreCase)
                        || k.EndsWith("Secret", StringComparison.OrdinalIgnoreCase)
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
            // The page only ever holds the placeholder for a saved key; testing it means the saved one.
            var key = request.ApiKey == SecretPlaceholder ? _lidarrOpts.CurrentValue.ApiKey : request.ApiKey;
            var options = await _lidarr.TestConnectionAsync(request.BaseUrl ?? "", key ?? "", ct);
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
                return new ServiceProbe(false, "Not set up yet. Enter your server under Music server.");
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
            if (_soulseekLink is null)
            {
                var ok = await _slskd.IsReachableAsync(ct);
                return new ServiceProbe(ok, ok ? "reachable" : "unreachable / auth failed");
            }
            // slskd answering is not slskd able to search: it can be up and out of Soulseek.
            var reading = await _soulseekLink.ReadAsync(fresh: true, ct);
            var (up, warning, detail) = Octo.Services.Soulseek.SoulseekLink.Describe(reading,
                _soulseekOpts.CurrentValue.EffectiveOutageHoldHours);
            return new ServiceProbe(up, detail, Warning: warning);
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
                : new ServiceProbe(true, "Not set up. Optional.", Configured: false);
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
        // Optional: without a key search still shows your library and Deezer albums, and radio
        // falls back to Navidrome. Red here read as a broken install.
        if (string.IsNullOrEmpty(key))
            return new ServiceProbe(true, "No API key, so song discovery and radio are off. Optional.", Configured: false);
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

    /// <summary>
    /// Health of one backing service. Configured is false for an optional service nobody set up,
    /// which the dashboard shows as off rather than as a failure.
    /// </summary>
    private record ServiceProbe(bool Ok, string Detail, bool Warning = false, bool Configured = true);

    /// <summary>
    /// What a saved credential reads as through the admin API: every password, key, token and
    /// webhook address in <see cref="SecretFields"/> and <see cref="SecretMaps"/>. None goes out
    /// in clear, and a save that sends this back keeps what is saved.
    /// </summary>
    internal const string SecretPlaceholder = "(saved, not shown)";

    /// <summary>Every credential among the settings, by section and key, with its name in words
    /// for "Retype the whole ...". AdminSecretTests walks the settings types, so a new one cannot
    /// go out in clear without failing it.</summary>
    internal static readonly (string Section, string Key, string Words)[] SecretFields =
    [
        ("Subsonic", "AdminPassword", "admin password"),
        ("Soulseek", "Password", "slskd password"),
        ("Soulseek", "AcoustIdApiKey", "AcoustID application key"),
        ("Soulseek", "AcoustIdUserApiKey", "AcoustID user key"),
        ("Lidarr", "ApiKey", "Lidarr API key"),
        ("LastFm", "ApiKey", "Last.fm API key"),
        ("LastFm", "ApiSecret", "Last.fm shared secret"),
        ("Notifications", "NtfyToken", "ntfy token"),
        ("Notifications", "DiscordWebhookUrl", "Discord webhook address"),
        ("ListenBrainz", "Token", "ListenBrainz token"),
    ];

    /// <summary>Credentials kept one per listener: section, map, and the field inside each entry,
    /// or null when the entry itself is the credential.</summary>
    internal static readonly (string Section, string Map, string? Field)[] SecretMaps =
    [
        ("LastFm", "UserSessions", "SessionKey"),
        ("ListenBrainz", "UserTokens", null),
    ];

    private static string MaskSecret(string? value) =>
        string.IsNullOrEmpty(value) ? "" : SecretPlaceholder;

    /// <summary>Each listener's ListenBrainz token as the placeholder, names kept.</summary>
    private static Dictionary<string, string> MaskTokens(IReadOnlyDictionary<string, string>? tokens) =>
        (tokens ?? new Dictionary<string, string>())
            .Where(pair => !string.IsNullOrEmpty(pair.Value))
            .ToDictionary(pair => pair.Key, pair => SecretPlaceholder, StringComparer.OrdinalIgnoreCase);

    /// <summary>
    /// A settings patch with every echoed placeholder taken out, so what is saved stays: a
    /// credential, or a listener's ListenBrainz token (kept from <paramref name="storedTokens"/>,
    /// since that map is saved whole). The words to refuse it with when someone typed onto the end
    /// of a placeholder, or null.
    /// </summary>
    internal static string? KeepSavedSecrets(JsonObject patch, IReadOnlyDictionary<string, string>? storedTokens)
    {
        foreach (var (section, name, words) in SecretFields)
        {
            if (Child(patch, section) is not JsonObject part || KeyOf(part, name) is not { } key
                || part[key] is not JsonValue value || !value.TryGetValue<string>(out var text)
                || !text.StartsWith(SecretPlaceholder, StringComparison.Ordinal))
                continue;
            if (text != SecretPlaceholder) return $"Retype the whole {words}; it was added to the hidden placeholder.";
            part.Remove(key);
        }
        if (Child(patch, "ListenBrainz") is JsonObject listenBrainz && Child(listenBrainz, "UserTokens") is JsonObject tokens)
            foreach (var user in tokens.Select(pair => pair.Key).ToList())
            {
                if (tokens[user] is not JsonValue value || !value.TryGetValue<string>(out var text)
                    || !text.StartsWith(SecretPlaceholder, StringComparison.Ordinal))
                    continue;
                if (text != SecretPlaceholder) return $"Retype {user}'s whole ListenBrainz token; it was added to the hidden placeholder.";
                var stored = storedTokens?.FirstOrDefault(pair => string.Equals(pair.Key, user, StringComparison.OrdinalIgnoreCase)).Value;
                if (string.IsNullOrEmpty(stored)) tokens.Remove(user);
                else tokens[user] = stored;
            }
        return null;
    }

    /// <summary>The words for the first credential in a Raw config document that still starts
    /// with the placeholder once the placeholders are restored: someone typed onto its end.</summary>
    internal static string? SecretTypedIntoPlaceholder(JsonObject incoming)
    {
        static bool Typed(JsonNode? node) =>
            node is JsonValue value && value.TryGetValue<string>(out var text) && text.StartsWith(SecretPlaceholder, StringComparison.Ordinal);
        foreach (var (section, name, words) in SecretFields)
            if (Child(incoming, section) is JsonObject part && KeyOf(part, name) is { } key && Typed(part[key]))
                return words;
        if (Child(incoming, "ListenBrainz") is JsonObject listenBrainz && Child(listenBrainz, "UserTokens") is JsonObject tokens
            && tokens.FirstOrDefault(pair => Typed(pair.Value)) is { Key: { } user })
            return $"ListenBrainz token of {user}";
        return null;
    }

    /// <summary>The credentials Octo runs with, shaped like the settings file: what Raw config
    /// keeps when the file itself cannot be read.</summary>
    private JsonObject RunningSecrets()
    {
        var soulseek = _soulseekOpts.CurrentValue;
        var lastFm = _lastFmOpts.CurrentValue;
        var notifications = _notificationOpts.CurrentValue;
        var listenBrainz = _listenBrainzOpts?.CurrentValue;
        return new JsonObject
        {
            ["Subsonic"] = new JsonObject { ["AdminPassword"] = _subsonicOpts.CurrentValue.AdminPassword },
            ["Soulseek"] = new JsonObject
            {
                ["Password"] = soulseek.Password,
                ["AcoustIdApiKey"] = soulseek.AcoustIdApiKey,
                ["AcoustIdUserApiKey"] = soulseek.AcoustIdUserApiKey,
            },
            ["Lidarr"] = new JsonObject { ["ApiKey"] = _lidarrOpts.CurrentValue.ApiKey },
            ["LastFm"] = new JsonObject
            {
                ["ApiKey"] = lastFm.ApiKey,
                ["ApiSecret"] = lastFm.ApiSecret,
                ["UserSessions"] = JsonSerializer.SerializeToNode(lastFm.UserSessions),
            },
            ["Notifications"] = new JsonObject
            {
                ["NtfyToken"] = notifications.NtfyToken,
                ["DiscordWebhookUrl"] = notifications.DiscordWebhookUrl,
            },
            ["ListenBrainz"] = new JsonObject
            {
                ["Token"] = listenBrainz?.Token,
                ["UserTokens"] = JsonSerializer.SerializeToNode(listenBrainz?.UserTokens ?? new Dictionary<string, string>()),
            },
        };
    }

    /// <summary>
    /// Undo the placeholder in a Raw config document before it replaces the file: keep the stored
    /// password when the file has one, otherwise drop the key so an environment value keeps
    /// applying. A real value is left alone.
    /// </summary>
    internal static void RestoreSecretPlaceholders(JsonObject incoming, JsonObject existingFile)
    {
        foreach (var (section, name, _) in SecretFields)
            RestorePlaceholder(Child(incoming, section), Child(existingFile, section), name);

        // Each listener's ListenBrainz token, by name; one only the environment has is dropped.
        if (Child(incoming, "ListenBrainz") is JsonObject listenBrainz && Child(listenBrainz, "UserTokens") is JsonObject tokens)
        {
            var storedTokens = Child(existingFile, "ListenBrainz") is JsonObject storedListenBrainz
                ? Child(storedListenBrainz, "UserTokens") : null;
            foreach (var user in tokens.Select(pair => pair.Key).ToList())
            {
                RestorePlaceholder(tokens, storedTokens, user);
                if (KeyOf(tokens, user) is null) tokens.Remove(user);
            }
        }

        // Each listener's session the same way, matched by username. An entry whose key is only
        // in the environment is dropped whole, so the environment keeps applying.
        if (Child(incoming, "LastFm") is not JsonObject lastFm
            || Child(lastFm, "UserSessions") is not JsonObject sessions)
            return;
        var storedSessions = Child(existingFile, "LastFm") is JsonObject storedLastFm
            ? Child(storedLastFm, "UserSessions") : null;
        foreach (var user in sessions.Select(pair => pair.Key).ToList())
        {
            if (sessions[user] is not JsonObject session) continue;
            var stored = storedSessions is not null && KeyOf(storedSessions, user) is { } storedUser
                ? storedSessions[storedUser] as JsonObject : null;
            RestorePlaceholder(session, stored, "SessionKey");
            if (KeyOf(session, "SessionKey") is null) sessions.Remove(user);
        }
    }

    /// <summary>
    /// The listener whose Last.fm session key, after the placeholders were restored, still starts
    /// with the placeholder: someone typed onto the end of it. Saved, it would be a key Last.fm
    /// refuses, and the listener would be disconnected for it. Null when there is none.
    /// </summary>
    internal static string? SessionKeyTypedIntoPlaceholder(JsonObject incoming)
    {
        if (Child(incoming, "LastFm") is not JsonObject lastFm
            || Child(lastFm, "UserSessions") is not JsonObject sessions)
            return null;
        foreach (var (user, node) in sessions)
            if (node is JsonObject session
                && KeyOf(session, "SessionKey") is { } key
                && session[key] is JsonValue value
                && value.TryGetValue<string>(out var text)
                && text.StartsWith(SecretPlaceholder, StringComparison.Ordinal))
                return user;
        return null;
    }

    /// <summary>Swaps one placeholder back for the stored value, or drops the key when nothing is
    /// stored. A real value is left alone.</summary>
    private static void RestorePlaceholder(JsonObject? incoming, JsonObject? existing, string name)
    {
        if (incoming is null
            || KeyOf(incoming, name) is not { } key
            || incoming[key] is not JsonValue value
            || !value.TryGetValue<string>(out var text)
            || text != SecretPlaceholder)
            return;

        var stored = existing is not null && KeyOf(existing, name) is { } storedKey ? existing[storedKey] : null;
        if (stored is JsonValue storedValue && storedValue.TryGetValue<string>(out var storedText)
            && !string.IsNullOrEmpty(storedText))
            incoming[key] = storedText;
        else
            incoming.Remove(key);
    }

    /// <summary>The key in <paramref name="obj"/> that matches <paramref name="name"/> ignoring
    /// case, as configuration keys do, or null.</summary>
    private static string? KeyOf(JsonObject obj, string name) =>
        obj.Select(pair => pair.Key).FirstOrDefault(key => string.Equals(key, name, StringComparison.OrdinalIgnoreCase));

    private static JsonObject? Child(JsonObject obj, string name) =>
        KeyOf(obj, name) is { } key ? obj[key] as JsonObject : null;

    /// <summary>The merged file echoed after a save, with the admin password masked the same way
    /// the GET masks it.</summary>
    internal static JsonObject RedactSecrets(JsonObject merged)
    {
        var copy = merged.DeepClone().AsObject();
        foreach (var (section, name, _) in SecretFields) MaskIn(Child(copy, section), name);
        foreach (var (section, map, field) in SecretMaps)
        {
            if (Child(copy, section) is not JsonObject part || Child(part, map) is not JsonObject entries) continue;
            foreach (var key in entries.Select(pair => pair.Key).ToList())
                if (field is null) MaskIn(entries, key);
                else MaskIn(entries[key] as JsonObject, field);
        }
        return copy;
    }

    private static void MaskIn(JsonObject? section, string name)
    {
        if (section is null) return;
        foreach (var key in section.Select(pair => pair.Key)
                     .Where(key => string.Equals(key, name, StringComparison.OrdinalIgnoreCase)).ToList())
            if (section[key] is JsonValue value && value.TryGetValue<string>(out var text))
                section[key] = MaskSecret(text);
    }

    /// <summary>Each listener's Last.fm link with the key masked: enough for the Raw editor to
    /// round-trip it, and for nobody to read it back.</summary>
    private static JsonObject MaskSessions(IReadOnlyDictionary<string, LastFmUserSession>? sessions) =>
        new((sessions ?? new Dictionary<string, LastFmUserSession>())
            .Where(pair => !string.IsNullOrWhiteSpace(pair.Value?.SessionKey))
            .Select(pair => new KeyValuePair<string, JsonNode?>(pair.Key, new JsonObject
            {
                ["SessionKey"] = SecretPlaceholder,
                ["LastFmUser"] = pair.Value.LastFmUser ?? "",
            })));

    /// <summary>Where Better quality looks, from the one place that decides it.</summary>
    private Octo.Services.Library.UpgradeSources? UpgradeSourcesNow =>
        HttpContext?.RequestServices.GetService<Octo.Services.Library.UpgradeSources>();

    private bool UpgradeSourceReady =>
        UpgradeSourcesNow?.Ready ?? Octo.Services.Library.UpgradeSources.SoulseekSetUp(_soulseekOpts.CurrentValue);

    private string UpgradeSourceName => UpgradeSourcesNow?.Name ?? "Soulseek";

    /// <summary>The Updates section as configured now; read when asked, so a save shows at once.</summary>
    private UpdateSettings UpdateOptions => _config.GetSection("Updates").Get<UpdateSettings>() ?? new UpdateSettings();

    private ImportSettings ImportOptions => _config.GetSection("Imports").Get<ImportSettings>() ?? new ImportSettings();

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
    /// <summary>
    /// Forget every peer and file that download verification rejected.
    ///
    /// The recovery lever for a wrong denial. Entries lapse on their own after 30 days, but a
    /// user watching a track stop being fetchable should not have to wait a month to find out
    /// whether this list is why.
    /// </summary>
    [HttpPost("soulseek/rejected-peers/clear")]
    public IActionResult ClearRejectedPeers()
    {
        var cleared = _rejectedPeers.Clear();
        _logger.LogInformation("Rejected-peer memory cleared by admin request ({Count} entries)", cleared);
        return Ok(new { cleared });
    }

    [HttpPost("clear-metadata-cache")]
    public IActionResult ClearMetadataCache()
    {
        _deezer.ClearCaches();
        _coverArt.ClearCache();
        _logger.LogInformation("Metadata and cover-art caches cleared by admin request");
        return Ok(new { cleared = true });
    }
}
