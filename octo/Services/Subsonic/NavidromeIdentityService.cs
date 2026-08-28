using System.Text;
using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;

namespace Octo.Services.Subsonic;

/// <summary>One library as Navidrome reports it. <paramref name="Folder"/> is the
/// path Octo should use: remotePath when Navidrome supplies one (how other services
/// see the same library), otherwise its own path.</summary>
public record NavidromeLibrary(string? Id, string? Name, string Folder);

/// <summary>
/// Octo's own standing identity toward the upstream Navidrome. Octo is a proxy, so
/// most requests carry the client's credentials. But background work has no client
/// in flight: detecting where Navidrome keeps its music, and triggering a rescan
/// after a download. This service supplies that identity two ways, in priority
/// order:
///   1. Configured admin creds (Subsonic:AdminUsername / AdminPassword), if set.
///   2. Captured from traffic: when a client signs in through the relayed native
///      POST /auth/login, we cache the returned admin JWT + subsonic salt/token.
///
/// From that identity it auto-detects the music folder from Navidrome's native
/// GET /api/library, so downloads land where Navidrome actually scans no matter
/// how the two are configured, instead of relying on a hand-kept DownloadPath that
/// can silently drift from the server's real music directory.
/// </summary>
public class NavidromeIdentityService
{
    // IOptionsMonitor, not IOptions: the admin UI writes settings.json and the
    // config provider reloads it, but IOptions.Value is resolved once and this is a
    // singleton, so a captured copy would serve startup values until a restart. The
    // admin UI read through IOptionsMonitor and therefore SHOWED the new value while
    // nothing acted on it.
    private readonly IOptionsMonitor<SubsonicSettings> settingsOptions;
    private SubsonicSettings _settings => settingsOptions.CurrentValue;
    private readonly IHttpClientFactory _httpFactory;
    private readonly ILogger<NavidromeIdentityService> _logger;

    private readonly object _lock = new();
    private string? _jwt;
    private string? _subsonicToken;
    private string? _subsonicSalt;
    private string? _username;
    private readonly Dictionary<string, string> _nativeUsers = new(StringComparer.Ordinal);

    private string? _detectedFolder;
    private List<NavidromeLibrary> _libraries = new();
    private DateTime _detectedAt;
    private static readonly TimeSpan DetectTtl = TimeSpan.FromMinutes(30);
    private readonly SemaphoreSlim _detectGate = new(1, 1);

    public NavidromeIdentityService(
        IOptionsMonitor<SubsonicSettings> settings,
        IHttpClientFactory httpFactory,
        ILogger<NavidromeIdentityService> logger)
    {
        settingsOptions = settings;
        _httpFactory = httpFactory;
        _logger = logger;
    }

    /// <summary>The Navidrome music folder detected from /api/library, or null.</summary>
    public string? DetectedMusicFolder { get { lock (_lock) return _detectedFolder; } }

    /// <summary>Every library Navidrome reported on the last successful detection.
    /// Empty until one has run. Used by the admin UI to offer a choice instead of
    /// silently adopting whichever one Navidrome listed first.</summary>
    public IReadOnlyList<NavidromeLibrary> KnownLibraries { get { lock (_lock) return _libraries; } }

    /// <summary>
    /// Effective download path: an explicit override wins, else the Navidrome-detected
    /// music folder, else the caller's configured fallback. Auto-detect can be turned
    /// off, in which case the configured value is always used.
    /// </summary>
    public string EffectiveDownloadPath(string configuredFallback)
    {
        if (!_settings.AutoDetectDownloadPath) return configuredFallback;
        var d = DetectedMusicFolder;
        return string.IsNullOrEmpty(d) ? configuredFallback : d;
    }

    /// <summary>Subsonic auth triplet (u/t/s) for authenticated Subsonic calls such
    /// as startScan, or null if no admin identity has been captured/configured yet.</summary>
    public (string user, string token, string salt)? GetScanAuth()
    {
        lock (_lock)
        {
            if (!string.IsNullOrEmpty(_username) && !string.IsNullOrEmpty(_subsonicToken)
                && !string.IsNullOrEmpty(_subsonicSalt))
                return (_username!, _subsonicToken!, _subsonicSalt!);
            return null;
        }
    }

    /// <summary>
    /// Cache the identity from a native login response body. Only admin logins are
    /// kept: /api/library and startScan need admin, and a later non-admin sign-in
    /// must not overwrite a good admin identity.
    /// </summary>
    public void CaptureLogin(byte[] body)
    {
        try
        {
            using var doc = JsonDocument.Parse(body);
            var root = doc.RootElement;
            if (root.ValueKind != JsonValueKind.Object) return;
            var token = root.TryGetProperty("token", out var t) ? t.GetString() : null;
            var isAdmin = root.TryGetProperty("isAdmin", out var a) && a.ValueKind == JsonValueKind.True;
            var loginUsername = root.TryGetProperty("username", out var loginUser) ? loginUser.GetString() : null;
            if (string.IsNullOrEmpty(token)) return;

            if (!string.IsNullOrEmpty(loginUsername))
            {
                lock (_lock)
                {
                    _nativeUsers[token] = loginUsername;
                    while (_nativeUsers.Count > 100) _nativeUsers.Remove(_nativeUsers.Keys.First());
                }
            }
            if (!isAdmin) return;

            lock (_lock)
            {
                _jwt = token;
                _subsonicToken = root.TryGetProperty("subsonicToken", out var st) ? st.GetString() : _subsonicToken;
                _subsonicSalt = root.TryGetProperty("subsonicSalt", out var ss) ? ss.GetString() : _subsonicSalt;
                _username = root.TryGetProperty("username", out var u) ? u.GetString() : _username;
            }
            _logger.LogInformation("Captured Navidrome admin identity from login (user={User})", _username);
            // Refresh the detected music folder in the background off the new token.
            _ = Task.Run(() => DetectMusicFolderAsync(force: true));
        }
        catch { /* not a login response we understand; ignore */ }
    }

    /// <summary>Returns the user associated with a native login token captured while
    /// proxying /auth/login. Tokens and mappings are memory-only.</summary>
    public string? UsernameForNativeToken(string? token)
    {
        if (string.IsNullOrWhiteSpace(token)) return null;
        lock (_lock) return _nativeUsers.GetValueOrDefault(token);
    }

    /// <summary>
    /// Detects Navidrome's music folder via GET /api/library. Prefers remotePath
    /// (the path as other services see the same library) then falls back to path.
    /// Cached with a TTL; pass force to bypass the cache.
    /// </summary>
    public async Task<string?> DetectMusicFolderAsync(bool force = false, CancellationToken ct = default)
    {
        lock (_lock)
        {
            if (!force && !string.IsNullOrEmpty(_detectedFolder) && DateTime.UtcNow - _detectedAt < DetectTtl)
                return _detectedFolder;
        }
        if (string.IsNullOrEmpty(_settings.Url) || !_settings.AutoDetectDownloadPath) return null;

        await _detectGate.WaitAsync(ct);
        try
        {
            lock (_lock)
            {
                if (!force && !string.IsNullOrEmpty(_detectedFolder) && DateTime.UtcNow - _detectedAt < DetectTtl)
                    return _detectedFolder;
            }

            var jwt = await EnsureJwtAsync(ct);
            if (string.IsNullOrEmpty(jwt)) return null;

            var http = _httpFactory.CreateClient();
            using var req = new HttpRequestMessage(HttpMethod.Get, $"{_settings.Url!.TrimEnd('/')}/api/library");
            req.Headers.TryAddWithoutValidation("X-Nd-Authorization", $"Bearer {jwt}");
            using var resp = await http.SendAsync(req, ct);
            if (resp.StatusCode == System.Net.HttpStatusCode.Unauthorized)
            {
                lock (_lock) _jwt = null; // captured token expired; force a fresh login next time
                return null;
            }
            if (!resp.IsSuccessStatusCode) return null;

            var json = await resp.Content.ReadAsStringAsync(ct);
            using var doc = JsonDocument.Parse(json);
            if (doc.RootElement.ValueKind != JsonValueKind.Array || doc.RootElement.GetArrayLength() == 0)
                return null;

            // Navidrome can serve several libraries. Read them all: taking [0] and
            // calling it "the" music folder meant a multi-library server had its
            // download target chosen by whatever order Navidrome happened to return,
            // with no way to say otherwise.
            var libraries = new List<NavidromeLibrary>();
            foreach (var entry in doc.RootElement.EnumerateArray())
            {
                // remotePath is the path as OTHER services see this library, which is
                // what Octo needs; path is how Navidrome itself sees it.
                var remote = entry.TryGetProperty("remotePath", out var rp) ? rp.GetString() : null;
                var path = entry.TryGetProperty("path", out var p) ? p.GetString() : null;
                var f = !string.IsNullOrEmpty(remote) ? remote : path;
                if (string.IsNullOrEmpty(f)) continue;
                libraries.Add(new NavidromeLibrary(
                    entry.TryGetProperty("id", out var idEl) ? idEl.ToString() : null,
                    entry.TryGetProperty("name", out var nameEl) ? nameEl.GetString() : null,
                    f));
            }
            if (libraries.Count == 0) return null;
            lock (_lock) _libraries = libraries;

            // An explicit pin wins. Absent one, behave exactly as before and take the
            // first entry, so an existing install's download target cannot move on
            // update. A pin that no longer matches anything Navidrome reports falls
            // back the same way rather than stranding downloads.
            var pinned = _settings.LibraryPath?.Trim();
            var chosen = libraries[0];
            if (!string.IsNullOrEmpty(pinned))
            {
                var match = libraries.FirstOrDefault(l =>
                    string.Equals(l.Folder, pinned, StringComparison.Ordinal));
                if (match != null) chosen = match;
                else
                    _logger.LogWarning(
                        "Pinned library path '{Pinned}' is not among the {Count} libraries " +
                        "Navidrome reports; using '{Fallback}' instead.",
                        pinned, libraries.Count, chosen.Folder);
            }
            var folder = chosen.Folder;

            // Safety gate: only ADOPT the detected path if it actually exists inside
            // Octo's own container. Navidrome reports the path as IT sees it, which is
            // only useful to Octo when the same directory is mounted at the same path
            // here. If it isn't (different mount, or Octo can't see it), keep the
            // configured DownloadPath instead of silently redirecting downloads to a
            // folder Navidrome can't read. This is what makes auto-detect safe to have
            // on by default, including for existing installs on update.
            if (!Directory.Exists(folder))
            {
                _logger.LogWarning(
                    "Navidrome reports its music folder as '{Folder}', but that path is not " +
                    "mounted in Octo's container — keeping the configured download path. " +
                    "Mount '{Folder}' into Octo (or set Navidrome's remotePath) to enable auto-detect.",
                    folder, folder);
                return null;
            }

            lock (_lock) { _detectedFolder = folder; _detectedAt = DateTime.UtcNow; }
            _logger.LogInformation("Detected Navidrome music folder: {Folder}", folder);
            return folder;
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Navidrome music-folder detection failed: {Msg}", ex.Message);
            return null;
        }
        finally
        {
            _detectGate.Release();
        }
    }

    /// <summary>Ensure a usable admin JWT: a captured one, else a fresh login with
    /// configured admin creds. Returns the token or null when neither is available.</summary>
    private async Task<string?> EnsureJwtAsync(CancellationToken ct)
    {
        lock (_lock) { if (!string.IsNullOrEmpty(_jwt)) return _jwt; }

        if (string.IsNullOrEmpty(_settings.AdminUsername) || string.IsNullOrEmpty(_settings.AdminPassword)
            || string.IsNullOrEmpty(_settings.Url))
            return null;

        try
        {
            var http = _httpFactory.CreateClient();
            var payload = JsonSerializer.Serialize(new { username = _settings.AdminUsername, password = _settings.AdminPassword });
            using var content = new StringContent(payload, Encoding.UTF8, "application/json");
            using var resp = await http.PostAsync($"{_settings.Url!.TrimEnd('/')}/auth/login", content, ct);
            if (!resp.IsSuccessStatusCode)
            {
                _logger.LogWarning("Navidrome admin login failed: HTTP {Code}", (int)resp.StatusCode);
                return null;
            }
            CaptureLogin(await resp.Content.ReadAsByteArrayAsync(ct));
            lock (_lock) return _jwt;
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Navidrome admin login error: {Msg}", ex.Message);
            return null;
        }
    }
}
