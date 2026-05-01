using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;

namespace Octo.Services.LastFm;

public class LastFmService
{
    private readonly HttpClient _httpClient;
    private readonly LastFmSettings _settings;
    private readonly ILogger<LastFmService> _logger;
    private const string BaseUrl = "https://ws.audioscrobbler.com/2.0/";
    
    // Simple in-memory cache
    private readonly Dictionary<string, (DateTime Expiry, List<SimilarTrack> Tracks)> _cache = new();

    public LastFmService(
        HttpClient httpClient,
        IOptions<LastFmSettings> settings,
        ILogger<LastFmService> logger)
    {
        _httpClient = httpClient;
        _settings = settings.Value;
        _logger = logger;
    }

    public record SimilarTrack(string Artist, string Title, double Match, int? Duration = null);

    public async Task<List<SimilarTrack>> GetSimilarTracksAsync(string artist, string title, int limit = 50)
    {
        var cacheKey = $"{artist}|{title}".ToLowerInvariant();
        
        // Check cache
        if (_cache.TryGetValue(cacheKey, out var cached) && cached.Expiry > DateTime.UtcNow)
        {
            _logger.LogDebug("Returning {Count} cached similar tracks for {Artist} - {Title}", 
                cached.Tracks.Count, artist, title);
            return cached.Tracks.Take(limit).ToList();
        }

        try
        {
            var url = $"{BaseUrl}?method=track.getsimilar&artist={Uri.EscapeDataString(artist)}&track={Uri.EscapeDataString(title)}&api_key={_settings.ApiKey}&format=json&limit={limit}";
            
            _logger.LogInformation("Fetching similar tracks from Last.fm for {Artist} - {Title}", artist, title);
            
            var response = await _httpClient.GetAsync(url);
            response.EnsureSuccessStatusCode();
            
            var json = await response.Content.ReadAsStringAsync();
            var doc = JsonDocument.Parse(json);
            
            var tracks = new List<SimilarTrack>();
            
            if (doc.RootElement.TryGetProperty("similartracks", out var similarTracks) &&
                similarTracks.TryGetProperty("track", out var trackArray))
            {
                foreach (var track in trackArray.EnumerateArray())
                {
                    var trackName = track.GetProperty("name").GetString() ?? "";
                    var artistName = "";
                    
                    if (track.TryGetProperty("artist", out var artistObj))
                    {
                        artistName = artistObj.TryGetProperty("name", out var name) 
                            ? name.GetString() ?? "" 
                            : "";
                    }
                    
                    var match = 0.0;
                    if (track.TryGetProperty("match", out var matchProp))
                    {
                        // Last.fm returns match as a number, not a string
                        if (matchProp.ValueKind == JsonValueKind.Number)
                        {
                            match = matchProp.GetDouble();
                        }
                        else if (matchProp.ValueKind == JsonValueKind.String)
                        {
                            double.TryParse(matchProp.GetString(), out match);
                        }
                    }
                    
                    // Last.fm returns duration in milliseconds (sometimes a string,
                    // sometimes a number, sometimes "0" when unknown — treat 0 as null
                    // so we fall back to the placeholder default downstream).
                    int? durationSec = null;
                    if (track.TryGetProperty("duration", out var durEl))
                    {
                        long durMs = durEl.ValueKind switch
                        {
                            JsonValueKind.Number => durEl.GetInt64(),
                            JsonValueKind.String => long.TryParse(durEl.GetString(), out var d) ? d : 0,
                            _ => 0
                        };
                        if (durMs > 1000) durationSec = (int)(durMs / 1000);
                    }

                    if (!string.IsNullOrEmpty(trackName) && !string.IsNullOrEmpty(artistName))
                    {
                        tracks.Add(new SimilarTrack(artistName, trackName, match, durationSec));
                    }
                }
            }
            
            _logger.LogInformation("Found {Count} similar tracks from Last.fm", tracks.Count);
            
            // Cache results
            _cache[cacheKey] = (DateTime.UtcNow.AddHours(_settings.RadioCacheDurationHours), tracks);
            
            // If no similar tracks found, try getting top tracks from similar artists
            if (tracks.Count == 0)
            {
                _logger.LogInformation("No similar tracks found, trying similar artists for {Artist}", artist);
                tracks = await GetTopTracksFromSimilarArtistsAsync(artist, limit);
            }
            
            return tracks.Take(limit).ToList();
        }
        catch (Exception ex)
        {
            _logger.LogError(ex, "Error fetching similar tracks from Last.fm for {Artist} - {Title}", artist, title);
            return new List<SimilarTrack>();
        }
    }

    private async Task<List<SimilarTrack>> GetTopTracksFromSimilarArtistsAsync(string artist, int limit)
    {
        try
        {
            // Get similar artists
            var artistUrl = $"{BaseUrl}?method=artist.getsimilar&artist={Uri.EscapeDataString(artist)}&api_key={_settings.ApiKey}&format=json&limit=10";
            
            var artistResponse = await _httpClient.GetAsync(artistUrl);
            artistResponse.EnsureSuccessStatusCode();
            
            var artistJson = await artistResponse.Content.ReadAsStringAsync();
            var artistDoc = JsonDocument.Parse(artistJson);
            
            var similarArtists = new List<string>();
            
            if (artistDoc.RootElement.TryGetProperty("similarartists", out var similarArtistsObj) &&
                similarArtistsObj.TryGetProperty("artist", out var artistArray))
            {
                foreach (var a in artistArray.EnumerateArray().Take(5))
                {
                    var name = a.GetProperty("name").GetString();
                    if (!string.IsNullOrEmpty(name))
                    {
                        similarArtists.Add(name);
                    }
                }
            }
            
            _logger.LogInformation("Found {Count} similar artists for {Artist}", similarArtists.Count, artist);
            
            // Get top tracks from each similar artist
            var tracks = new List<SimilarTrack>();
            
            foreach (var similarArtist in similarArtists)
            {
                var topTracksUrl = $"{BaseUrl}?method=artist.gettoptracks&artist={Uri.EscapeDataString(similarArtist)}&api_key={_settings.ApiKey}&format=json&limit=10";
                
                var topResponse = await _httpClient.GetAsync(topTracksUrl);
                if (!topResponse.IsSuccessStatusCode) continue;
                
                var topJson = await topResponse.Content.ReadAsStringAsync();
                var topDoc = JsonDocument.Parse(topJson);
                
                if (topDoc.RootElement.TryGetProperty("toptracks", out var topTracks) &&
                    topTracks.TryGetProperty("track", out var trackArray))
                {
                    foreach (var track in trackArray.EnumerateArray().Take(10))
                    {
                        var trackName = track.GetProperty("name").GetString() ?? "";
                        
                        if (!string.IsNullOrEmpty(trackName))
                        {
                            tracks.Add(new SimilarTrack(similarArtist, trackName, 0.5));
                        }
                    }
                }
            }
            
            _logger.LogInformation("Found {Count} top tracks from similar artists", tracks.Count);
            
            return tracks.Take(limit).ToList();
        }
        catch (Exception ex)
        {
            _logger.LogError(ex, "Error fetching top tracks from similar artists for {Artist}", artist);
            return new List<SimilarTrack>();
        }
    }

    public bool IsConfigured => !string.IsNullOrEmpty(_settings.ApiKey) && _settings.EnableRadio;

    /// <summary>
    /// Free-form track search. Used by Search3 hijack so the search bar
    /// returns Last.fm-driven discovery results instead of just local hits.
    /// Last.fm's track.search is a fuzzy match: "drake" returns Drake tracks,
    /// "drake hotline" returns "Hotline Bling" first, etc.
    /// </summary>
    public async Task<List<SimilarTrack>> SearchTracksAsync(string query, int limit = 30)
    {
        if (string.IsNullOrWhiteSpace(query)) return new List<SimilarTrack>();
        try
        {
            var url = $"{BaseUrl}?method=track.search&track={Uri.EscapeDataString(query)}&api_key={_settings.ApiKey}&format=json&limit={limit}";
            var response = await _httpClient.GetAsync(url);
            response.EnsureSuccessStatusCode();
            var json = await response.Content.ReadAsStringAsync();
            using var doc = JsonDocument.Parse(json);

            var tracks = new List<SimilarTrack>();
            if (doc.RootElement.TryGetProperty("results", out var results) &&
                results.TryGetProperty("trackmatches", out var matches) &&
                matches.TryGetProperty("track", out var trackArray) &&
                trackArray.ValueKind == JsonValueKind.Array)
            {
                foreach (var t in trackArray.EnumerateArray())
                {
                    var name = t.TryGetProperty("name", out var n) ? n.GetString() : null;
                    var artist = t.TryGetProperty("artist", out var a) ? a.GetString() : null;
                    if (!string.IsNullOrEmpty(name) && !string.IsNullOrEmpty(artist))
                        tracks.Add(new SimilarTrack(artist!, name!, 1.0));
                }
            }
            _logger.LogInformation("Last.fm track.search '{Q}' -> {N} tracks", query, tracks.Count);
            return tracks;
        }
        catch (Exception ex)
        {
            _logger.LogWarning(ex, "Last.fm track.search failed for '{Q}'", query);
            return new List<SimilarTrack>();
        }
    }

    /// <summary>
    /// Top tracks for a known artist. Used to pad a search when track.search
    /// returns thin results (e.g. one-word artist queries) and as the primary
    /// data source for "play this artist" radio behaviors.
    /// </summary>
    public async Task<List<SimilarTrack>> GetArtistTopTracksAsync(string artist, int limit = 30)
    {
        if (string.IsNullOrWhiteSpace(artist)) return new List<SimilarTrack>();
        try
        {
            var url = $"{BaseUrl}?method=artist.gettoptracks&artist={Uri.EscapeDataString(artist)}&api_key={_settings.ApiKey}&format=json&limit={limit}";
            var response = await _httpClient.GetAsync(url);
            response.EnsureSuccessStatusCode();
            var json = await response.Content.ReadAsStringAsync();
            using var doc = JsonDocument.Parse(json);

            var tracks = new List<SimilarTrack>();
            if (doc.RootElement.TryGetProperty("toptracks", out var top) &&
                top.TryGetProperty("track", out var trackArray) &&
                trackArray.ValueKind == JsonValueKind.Array)
            {
                foreach (var t in trackArray.EnumerateArray())
                {
                    var name = t.TryGetProperty("name", out var n) ? n.GetString() : null;
                    if (!string.IsNullOrEmpty(name))
                        tracks.Add(new SimilarTrack(artist, name!, 1.0));
                }
            }
            return tracks;
        }
        catch (Exception ex)
        {
            _logger.LogWarning(ex, "Last.fm artist.gettoptracks failed for '{A}'", artist);
            return new List<SimilarTrack>();
        }
    }
}
