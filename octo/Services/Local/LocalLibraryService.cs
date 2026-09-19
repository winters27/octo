using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Models.Download;
using Octo.Models.Search;
using Octo.Models.Subsonic;
using Octo.Services;
using Octo.Services.Soulseek;
using Octo.Services.Subsonic;

namespace Octo.Services.Local;

/// <summary>
/// Local library service implementation
/// Uses a simple JSON file to store mappings (can be replaced with a database)
/// </summary>
public class LocalLibraryService : ILocalLibraryService
{
    private readonly string _mappingFilePath;
    private readonly string _downloadDirectory;
    private readonly HttpClient _httpClient;
    // IOptionsMonitor, not IOptions: the admin UI writes settings.json and the
    // config provider reloads it, but IOptions.Value is resolved once and this is a
    // singleton, so a captured copy would serve startup values until a restart. The
    // admin UI read through IOptionsMonitor and therefore SHOWED the new value while
    // nothing acted on it.
    private readonly IOptionsMonitor<SubsonicSettings> subsonicSettingsOptions;
    private SubsonicSettings _subsonicSettings => subsonicSettingsOptions.CurrentValue;
    private readonly ExternalIdRegistry _idRegistry;
    private readonly NavidromeIdentityService _navIdentity;
    private readonly ILogger<LocalLibraryService> _logger;
    private Dictionary<string, LocalSongMapping>? _mappings;
    private readonly SemaphoreSlim _lock = new(1, 1);

    // Debounce to avoid triggering too many scans
    private DateTime _lastScanTrigger = DateTime.MinValue;
    private readonly TimeSpan _scanDebounceInterval = TimeSpan.FromSeconds(30);

    public LocalLibraryService(
        IConfiguration configuration,
        IHttpClientFactory httpClientFactory,
        IOptionsMonitor<SubsonicSettings> subsonicSettings,
        ExternalIdRegistry idRegistry,
        NavidromeIdentityService navIdentity,
        ILogger<LocalLibraryService> logger)
    {
        _downloadDirectory = configuration["Library:DownloadPath"] ?? Path.Combine(Directory.GetCurrentDirectory(), "downloads");
        _mappingFilePath = Path.Combine(_downloadDirectory, ".mappings.json");
        _httpClient = httpClientFactory.CreateClient();
        subsonicSettingsOptions = subsonicSettings;
        _idRegistry = idRegistry;
        _navIdentity = navIdentity;
        _logger = logger;
        
        if (!Directory.Exists(_downloadDirectory))
        {
            Directory.CreateDirectory(_downloadDirectory);
        }
    }

    public async Task<string?> GetLocalPathForExternalSongAsync(string externalProvider, string externalId)
    {
        var mappings = await LoadMappingsAsync();
        var key = $"{externalProvider}:{externalId}";
        
        if (mappings.TryGetValue(key, out var mapping) && File.Exists(mapping.LocalPath))
        {
            return mapping.LocalPath;
        }
        
        return null;
    }

    public async Task RegisterDownloadedSongAsync(Song song, string localPath)
    {
        if (song.ExternalProvider == null || song.ExternalId == null) return;
        
        // Load mappings first (this acquires the lock internally if needed)
        var mappings = await LoadMappingsAsync();
        
        await _lock.WaitAsync();
        try
        {
            var key = $"{song.ExternalProvider}:{song.ExternalId}";
            
            mappings[key] = new LocalSongMapping
            {
                ExternalProvider = song.ExternalProvider,
                ExternalId = song.ExternalId,
                LocalPath = localPath,
                Title = song.Title,
                Artist = song.Artist,
                Album = song.Album,
                DownloadedAt = DateTime.UtcNow,
                SourcePeer = song.SourcePeer,
                SourceFile = song.SourceFile,
            };
            
            await SaveMappingsAsync(mappings);
        }
        finally
        {
            _lock.Release();
        }
    }

    public async Task<string?> GetLocalIdForExternalSongAsync(string externalProvider, string externalId)
    {
        // For now, return null as we don't yet have integration
        // with the Subsonic server to retrieve local ID after scan
        await Task.CompletedTask;
        return null;
    }

    public (bool isExternal, string? provider, string? externalId) ParseSongId(string songId)
    {
        var (isExternal, provider, _, externalId) = ParseExternalId(songId);
        return (isExternal, provider, externalId);
    }

    public (bool isExternal, string? provider, string? type, string? externalId) ParseExternalId(string id)
    {
        // First check the registry — IDs we generated for YouTube/Soulseek
        // entries are pure base62 (no prefix) so they look identical to local
        // Navidrome IDs to clients but we still know they're ours.
        if (_idRegistry.Lookup(id) != null)
        {
            return (true, "soulseek", "song", id);
        }

        if (!id.StartsWith("ext-"))
        {
            return (false, null, null, null);
        }
        
        var parts = id.Split('-');
        
        // Known types for the new format
        var knownTypes = new HashSet<string> { "song", "album", "artist" };
        
        // New format: ext-{provider}-{type}-{id} (e.g., ext-deezer-artist-259)
        // Only use new format if parts[2] is a known type
        if (parts.Length >= 4 && knownTypes.Contains(parts[2]))
        {
            var provider = parts[1];
            var type = parts[2];
            var externalId = string.Join("-", parts.Skip(3)); // Handle IDs with dashes
            return (true, provider, type, externalId);
        }
        
        // Legacy format: ext-{provider}-{id} (assumes "song" type for backward compatibility)
        // This handles both 3-part IDs and 4+ part IDs where parts[2] is NOT a known type
        if (parts.Length >= 3)
        {
            var provider = parts[1];
            var externalId = string.Join("-", parts.Skip(2)); // Everything after provider is the ID
            return (true, provider, "song", externalId);
        }
        
        return (false, null, null, null);
    }

    private async Task<Dictionary<string, LocalSongMapping>> LoadMappingsAsync()
    {
        // Fast path: return cached mappings if available
        if (_mappings != null) return _mappings;
        
        // Slow path: acquire lock to load from file (prevents race condition)
        await _lock.WaitAsync();
        try
        {
            // Double-check after acquiring lock
            if (_mappings != null) return _mappings;
            
            if (File.Exists(_mappingFilePath))
            {
                var json = await File.ReadAllTextAsync(_mappingFilePath);
                _mappings = System.Text.Json.JsonSerializer.Deserialize<Dictionary<string, LocalSongMapping>>(json) 
                            ?? new Dictionary<string, LocalSongMapping>();
            }
            else
            {
                _mappings = new Dictionary<string, LocalSongMapping>();
            }
            
            return _mappings;
        }
        finally
        {
            _lock.Release();
        }
    }

    private async Task SaveMappingsAsync(Dictionary<string, LocalSongMapping> mappings)
    {
        _mappings = mappings;
        var json = System.Text.Json.JsonSerializer.Serialize(mappings, new System.Text.Json.JsonSerializerOptions 
        { 
            WriteIndented = true 
        });
        await File.WriteAllTextAsync(_mappingFilePath, json);
    }

    public string GetDownloadDirectory() => _downloadDirectory;

    /// <summary>
    /// Drop the mapping for a path Octo no longer owns.
    ///
    /// Without this, DownloadSongInternalAsync's existing-file short-circuit keeps pointing a
    /// re-acquire at the file that was just quarantined, and the replacement never happens.
    /// </summary>
    public async Task<bool> ForgetMappingAsync(string localPath)
    {
        if (string.IsNullOrWhiteSpace(localPath)) return false;

        var mappings = await LoadMappingsAsync();
        await _lock.WaitAsync();
        try
        {
            var stale = mappings
                .Where(pair => string.Equals(pair.Value.LocalPath, localPath, StringComparison.OrdinalIgnoreCase))
                .Select(pair => pair.Key).ToList();
            if (stale.Count == 0) return false;

            foreach (var key in stale) mappings.Remove(key);
            await SaveMappingsAsync(mappings);
            return true;
        }
        finally { _lock.Release(); }
    }

    public async Task<LocalSongMapping?> FindMappingByTagsAsync(string? artist, string? title, string? album)
    {
        if (string.IsNullOrWhiteSpace(artist) || string.IsNullOrWhiteSpace(title)) return null;

        var mappings = await LoadMappingsAsync();
        var matches = mappings.Values
            .Where(mapping =>
                string.Equals(mapping.Artist?.Trim(), artist.Trim(), StringComparison.OrdinalIgnoreCase)
                && string.Equals(mapping.Title?.Trim(), title.Trim(), StringComparison.OrdinalIgnoreCase)
                // Album only narrows when both sides have one; a mapping written before album
                // enrichment should not be excluded for lacking it.
                && (string.IsNullOrWhiteSpace(album) || string.IsNullOrWhiteSpace(mapping.Album)
                    || string.Equals(mapping.Album.Trim(), album.Trim(), StringComparison.OrdinalIgnoreCase))
                && !string.IsNullOrEmpty(mapping.LocalPath)
                && File.Exists(mapping.LocalPath))
            .Take(2)
            .ToList();

        return matches.Count == 1 ? matches[0] : null;
    }

    public async Task<IReadOnlyList<LocalSongMapping>> GetMappingsAsync()
    {
        var mappings = await LoadMappingsAsync();
        return mappings.Values.ToList();
    }

    public async Task<bool> TriggerLibraryScanAsync(bool force = false)
    {
        // Debounce: avoid triggering too many successive scans. A forced call skips it —
        // otherwise the last track of a batch can have its scan swallowed and stay
        // invisible until some unrelated trigger happens along.
        var now = DateTime.UtcNow;
        if (!force && now - _lastScanTrigger < _scanDebounceInterval)
        {
            _logger.LogDebug("Scan debounced - last scan was {Elapsed}s ago", 
                (now - _lastScanTrigger).TotalSeconds);
            return true;
        }
        
        _lastScanTrigger = now;
        
        try
        {
            // Navidrome's startScan requires an admin identity. Octo, as a proxy,
            // gets one from the NavidromeIdentityService (captured from a client's
            // relayed login or configured admin creds). When available we send the
            // Subsonic u/t/s triplet; otherwise we fall back to the bare call, which
            // only works on servers that allow unauthenticated localhost scans.
            var auth = _navIdentity.GetScanAuth();
            var url = auth is { } a
                ? $"{_subsonicSettings.Url}/rest/startScan?f=json&c=octo&v=1.16.1" +
                  $"&u={Uri.EscapeDataString(a.user)}&t={a.token}&s={a.salt}"
                : $"{_subsonicSettings.Url}/rest/startScan?f=json";

            _logger.LogInformation("Triggering Subsonic library scan ({Auth})...",
                auth is null ? "unauthenticated" : "authenticated");

            var response = await _httpClient.GetAsync(url);
            
            if (response.IsSuccessStatusCode)
            {
                var content = await response.Content.ReadAsStringAsync();
                _logger.LogInformation("Subsonic scan triggered successfully: {Response}", content);
                return true;
            }
            else
            {
                _logger.LogWarning("Failed to trigger Subsonic scan: {StatusCode} - Server may require authentication", response.StatusCode);
                return false;
            }
        }
        catch (Exception ex)
        {
            _logger.LogError(ex, "Error triggering Subsonic library scan");
            return false;
        }
    }

    public async Task<ScanStatus?> GetScanStatusAsync()
    {
        try
        {
            // Note: This endpoint works without authentication on most Subsonic/Navidrome servers
            // when called from localhost.
            var url = $"{_subsonicSettings.Url}/rest/getScanStatus?f=json";
            
            var response = await _httpClient.GetAsync(url);
            
            if (response.IsSuccessStatusCode)
            {
                var content = await response.Content.ReadAsStringAsync();
                var doc = JsonDocument.Parse(content);
                
                if (doc.RootElement.TryGetProperty("subsonic-response", out var subsonicResponse) &&
                    subsonicResponse.TryGetProperty("scanStatus", out var scanStatus))
                {
                    return new ScanStatus
                    {
                        Scanning = scanStatus.TryGetProperty("scanning", out var scanning) && scanning.GetBoolean(),
                        Count = scanStatus.TryGetProperty("count", out var count) ? count.GetInt32() : null
                    };
                }
            }
        }
        catch (Exception ex)
        {
            _logger.LogError(ex, "Error getting Subsonic scan status");
        }
        
        return null;
    }
}

/// <summary>
/// Represents the mapping between an external song and its local file
/// </summary>
public class LocalSongMapping
{
    public string ExternalProvider { get; set; } = string.Empty;
    public string ExternalId { get; set; } = string.Empty;
    public string LocalPath { get; set; } = string.Empty;
    public string? LocalSubsonicId { get; set; }
    public string Title { get; set; } = string.Empty;
    public string Artist { get; set; } = string.Empty;
    public string Album { get; set; } = string.Empty;
    public DateTime DownloadedAt { get; set; }

    /// <summary>Who delivered this file, when it came from Soulseek. Optional, so mappings
    /// written before this existed still load.</summary>
    public string? SourcePeer { get; set; }
    public string? SourceFile { get; set; }
}
