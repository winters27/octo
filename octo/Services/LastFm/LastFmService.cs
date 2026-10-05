using System.Collections.Concurrent;
using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.Metadata;

namespace Octo.Services.LastFm;

public partial class LastFmService
{
    private readonly HttpClient _httpClient;
    private readonly IOptionsMonitor<LastFmSettings> _settingsOptions;
    private LastFmSettings _settings => _settingsOptions.CurrentValue;
    private readonly ILogger<LastFmService> _logger;
    private const string BaseUrl = "https://ws.audioscrobbler.com/2.0/";
    
    // Concurrent because radio requests arrive on request threads and this is written on
    // every miss. A plain Dictionary written from two threads at once does not merely lose
    // an entry: a resize racing with an insert can corrupt the bucket chain and leave a
    // later read spinning forever inside the lookup.
    // Each entry keeps how many tracks were asked for, so a short answer cached for a station
    // refresh is not handed to a later request that wants more.
    private readonly ConcurrentDictionary<string, (DateTime Expiry, int Asked, List<SimilarTrack> Tracks, SimilarSource Source)> _cache = new();
    private readonly ConcurrentDictionary<string, (DateTime Expiry, object Value)> _radioCache = new();
    private readonly SemaphoreSlim _providerGate = new(4, 4);

    public LastFmService(
        HttpClient httpClient,
        IOptionsMonitor<LastFmSettings> settings,
        IOptions<MetadataSettings> metadataSettings,
        ILogger<LastFmService> logger)
    {
        _httpClient = httpClient;
        AcceptLanguageHeader.Apply(_httpClient, metadataSettings.Value);
        _settingsOptions = settings;
        _logger = logger;
    }

    /// <param name="Listeners">How many people Last.fm counts for the row, where the call says.</param>
    /// <param name="YouTubeId">The video a source already knows plays this song.</param>
    /// <param name="Provider">The <see cref="Octo.Services.Radio.RadioProvider"/> that suggested it.</param>
    public record SimilarTrack(string Artist, string Title, double Match, int? Duration = null, long? Listeners = null,
        string? YouTubeId = null, string? Provider = null);

    /// <summary>How a list of songs like a seed was found.</summary>
    public enum SimilarSource
    {
        /// <summary>Nothing came back.</summary>
        None,
        /// <summary>Last.fm's songs like this very song.</summary>
        Song,
        /// <summary>Songs like the song a nightcore, sped up or remixed seed came from, found only
        /// once that version was taken off the title.</summary>
        Original,
        /// <summary>Top songs of artists like the seed's artist, with nothing for the song itself.</summary>
        SimilarArtists,
    }

    /// <param name="ArtistTrusted">The seed's artist is who Last.fm thinks: it has the song under
    /// them, or they have a real audience there. False for a similar-artists list means the artist
    /// may only be whoever uploaded the song.</param>
    public record SimilarAnswer(List<SimilarTrack> Tracks, SimilarSource Source, bool ArtistTrusted);

    /// <summary>Listeners Last.fm counts for an artist before artists like them are worth a radio
    /// without the song itself: a new song by a known artist, not yet on Last.fm.</summary>
    internal const long KnownArtistListeners = 10_000;

    public async Task<List<SimilarTrack>> GetSimilarTracksAsync(string artist, string title, int limit = 50,
        CancellationToken cancellationToken = default) =>
        (await FindSimilarTracksAsync(artist, title, limit, cancellationToken)).Tracks;

    /// <summary>
    /// Songs like one song, and how Last.fm came by them, so a radio can tell a list for the song
    /// itself from a guess. A seed Last.fm has never heard of, a YouTube upload filed under its
    /// uploader for one, gets similar artists to that uploader's name, which can be anyone (#78).
    /// </summary>
    public async Task<SimilarAnswer> FindSimilarTracksAsync(string artist, string title, int limit = 50,
        CancellationToken cancellationToken = default)
    {
        var cacheKey = $"{artist}|{title}".ToLowerInvariant();

        try
        {
            List<SimilarTrack> tracks;
            SimilarSource source;
            // Check cache. A list shorter than what was asked for is everything Last.fm has, so it
            // answers any size.
            if (_cache.TryGetValue(cacheKey, out var cached) && cached.Expiry > DateTime.UtcNow
                && (cached.Asked >= limit || cached.Tracks.Count < cached.Asked))
            {
                _logger.LogDebug("Returning {Count} cached similar tracks for {Artist} - {Title}",
                    cached.Tracks.Count, artist, title);
                (tracks, source) = (cached.Tracks, cached.Source);
            }
            else
            {
                _logger.LogInformation("Fetching similar tracks from Last.fm for {Artist} - {Title}", artist, title);
                (tracks, source) = await FetchSimilarForSpellingsAsync(artist, title, limit, cancellationToken);
                _logger.LogInformation("Found {Count} similar tracks from Last.fm", tracks.Count);
                _cache[cacheKey] = (DateTime.UtcNow.AddHours(_settings.EffectiveRadioCacheDurationHours), limit, tracks, source);
            }

            if (tracks.Count > 0) return new SimilarAnswer(tracks.Take(limit).ToList(), source, ArtistTrusted: true);

            // If no similar tracks found, try getting top tracks from similar artists. For a
            // renamed artist that guess runs under the name Last.fm files them by: asked for
            // artists like "Ye", it answered for someone else, and a mix for "Ye - Crack Music"
            // came back as thirty Jon Anderson songs.
            var guessFrom = SongIdentity.KnownName(artist) ?? artist;
            _logger.LogInformation("No similar tracks found, trying similar artists for {Artist}", guessFrom);
            var guessed = await GetTopTracksFromSimilarArtistsAsync(guessFrom, limit, cancellationToken);
            var known = guessed.Count > 0 && (await KnowsSongAsync(guessFrom, title, cancellationToken)
                || await GetArtistListenersAsync(guessFrom, cancellationToken) >= KnownArtistListeners);
            return new SimilarAnswer(guessed.Take(limit).ToList(),
                guessed.Count > 0 ? SimilarSource.SimilarArtists : SimilarSource.None, known);
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _logger.LogError(ex, "Error fetching similar tracks from Last.fm for {Artist} - {Title}", artist, title);
            return new SimilarAnswer([], SimilarSource.None, ArtistTrusted: false);
        }
    }

    /// <summary>
    /// track.getsimilar under each way the song might be filed, stopping at the first that answers.
    /// </summary>
    private async Task<(List<SimilarTrack> Tracks, SimilarSource Source)> FetchSimilarForSpellingsAsync(
        string artist, string title, int limit, CancellationToken cancellationToken)
    {
        // Last.fm knows a song under one spelling. "$uicideboy$ - $UICIDE" or a title
        // carrying "(feat. X)" or "- Remastered 2011" can come back empty where the same song
        // written plainly does not, so those are asked before falling back to similar artists.
        // A renamed artist is filed under one name only: Last.fm has "Kanye West - Crack
        // Music" and nothing under "Ye", so the name the artist is best known by comes last.
        var knownName = SongIdentity.KnownName(artist);
        var queries = SongIdentity.QueryVariants(title, artist)
            .Where(variant => variant.Artist.Length > 0).Take(3).ToList();
        if (knownName is not null)
            queries.AddRange(SongIdentity.QueryVariants(title, knownName)
                .Where(variant => variant.Artist.Length > 0).Take(2));
        // An upload's artist is often the channel, with the singer in the title:
        // "Nightcore Galaxy" and "Alan Walker - Faded (Nightcore)". The title's artist goes last.
        if (UploadTitle.Match(title) is { Success: true } named
            && SongIdentity.Key(named.Groups["artist"].Value) != SongIdentity.Key(artist))
            queries.AddRange(SongIdentity.QueryVariants(named.Groups["title"].Value, named.Groups["artist"].Value)
                .Where(variant => variant.Artist.Length > 0 && !queries.Contains(variant)).Take(2));

        var styles = StyleVersions(title);
        foreach (var variant in queries)
        {
            var tracks = await FetchSimilarTracksAsync(variant.Artist, variant.Title, limit, cancellationToken);
            if (tracks.Count == 0) continue;
            var kept = StyleVersions(variant.Title);
            return (tracks, styles.All(kept.Contains) ? SimilarSource.Song : SimilarSource.Original);
        }
        return ([], SimilarSource.None);
    }

    /// <summary>"Artist - Title" at the front of an upload's title, split at the first spaced dash.</summary>
    private static readonly System.Text.RegularExpressions.Regex UploadTitle = new(
        @"^\s*(?<artist>[^\-–—]+?)\s+[\-–—]\s+(?<title>.*\S)\s*$",
        System.Text.RegularExpressions.RegexOptions.CultureInvariant);

    /// <summary>
    /// Versions that change how a song sounds enough to be a style of their own: a nightcore or
    /// sped up upload is not the song it came from, and songs like the original are not like it.
    /// </summary>
    private static readonly HashSet<string> StyleMarkers = new(StringComparer.Ordinal)
    {
        "nightcore", "sped up", "slowed", "reverb", "8d", "lofi", "bass boosted",
        "remix", "vip", "bootleg", "rework",
    };

    internal static IReadOnlyList<string> StyleVersions(string? title) =>
        SongIdentity.ParseTitle(title).Versions.Where(StyleMarkers.Contains).ToList();

    /// <summary>
    /// Whether Last.fm has the song under this artist, so the artist is likely the one who made it
    /// and not the channel that uploaded it. A failed check counts as no.
    /// </summary>
    private async Task<bool> KnowsSongAsync(string artist, string title, CancellationToken cancellationToken)
    {
        foreach (var variant in SongIdentity.QueryVariants(title, artist)
                     .Where(variant => variant.Artist.Length > 0).Take(2))
        {
            try
            {
                if (await GetTrackInfoAsync(variant.Artist, variant.Title, cancellationToken) is not null) return true;
            }
            catch (Exception ex) when (ex is not OperationCanceledException)
            {
                _logger.LogDebug(ex, "Last.fm track.getInfo failed for {Artist} - {Title}", variant.Artist, variant.Title);
            }
        }
        return false;
    }

    /// <summary>How many listeners Last.fm counts for an artist; 0 when it does not know them.</summary>
    private Task<long> GetArtistListenersAsync(string artist, CancellationToken cancellationToken) =>
        CachedAsync($"artist-listeners|{artist}", async ct =>
        {
            using var doc = await GetDocumentAsync("artist.getInfo",
                new Dictionary<string, string> { ["artist"] = artist }, ct);
            return doc is not null && doc.RootElement.TryGetProperty("artist", out var found)
                && found.TryGetProperty("stats", out var stats)
                && long.TryParse(stats.TryGetProperty("listeners", out var listeners) ? listeners.ToString() : "", out var count)
                ? count : 0L;
        }, cancellationToken);

    /// <summary>Top songs of the artists Last.fm counts as like one artist: a radio for an artist.</summary>
    public Task<List<SimilarTrack>> GetSimilarArtistTracksAsync(string artist, int limit = 50,
        CancellationToken cancellationToken = default) =>
        GetTopTracksFromSimilarArtistsAsync(SongIdentity.KnownName(artist) ?? artist, limit, cancellationToken);

    /// <summary>One track.getsimilar request, read into tracks.</summary>
    private async Task<List<SimilarTrack>> FetchSimilarTracksAsync(string artist, string title, int limit,
        CancellationToken cancellationToken)
    {
        var url = $"{BaseUrl}?method=track.getsimilar&artist={Uri.EscapeDataString(artist)}&track={Uri.EscapeDataString(title)}&api_key={_settings.ApiKey}&format=json&limit={limit}";

        var response = await _httpClient.GetAsync(url, cancellationToken);
        response.EnsureSuccessStatusCode();

        var json = await response.Content.ReadAsStringAsync();
        using var doc = JsonDocument.Parse(json);

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
                // sometimes a number, sometimes "0" when unknown; treat 0 as null
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
        return tracks;
    }

    private async Task<List<SimilarTrack>> GetTopTracksFromSimilarArtistsAsync(string artist, int limit,
        CancellationToken cancellationToken)
    {
        try
        {
            // Get similar artists
            var artistUrl = $"{BaseUrl}?method=artist.getsimilar&artist={Uri.EscapeDataString(artist)}&api_key={_settings.ApiKey}&format=json&limit=10";
            
            var artistResponse = await _httpClient.GetAsync(artistUrl, cancellationToken);
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
                
                var topResponse = await _httpClient.GetAsync(topTracksUrl, cancellationToken);
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
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _logger.LogError(ex, "Error fetching top tracks from similar artists for {Artist}", artist);
            return new List<SimilarTrack>();
        }
    }

    /// <summary>
    /// Last.fm can answer at all. Search discovery needs only this: an API key.
    /// </summary>
    public bool HasApiKey => !string.IsNullOrEmpty(_settings.ApiKey);

    /// <summary>
    /// Radio specifically is available. EnableRadio is a switch for the radio feature, so
    /// it belongs here and not on <see cref="HasApiKey"/> — the two used to be one property,
    /// which meant turning radio off also silently emptied the search bar of discovery
    /// results, a setting doing something its name does not say.
    /// </summary>
    public bool IsRadioEnabled => HasApiKey && _settings.EnableRadio;

    /// <summary>
    /// Free-form track search. Used by Search3 hijack so the search bar
    /// returns Last.fm-driven discovery results instead of just local hits.
    /// Last.fm's track.search is a fuzzy match: "drake" returns Drake tracks,
    /// "drake hotline" returns "Hotline Bling" first, etc.
    /// </summary>
    public async Task<List<SimilarTrack>> SearchTracksAsync(string query, int limit = 30,
        CancellationToken cancellationToken = default)
    {
        if (string.IsNullOrWhiteSpace(query)) return new List<SimilarTrack>();
        try
        {
            var url = $"{BaseUrl}?method=track.search&track={Uri.EscapeDataString(query)}&api_key={_settings.ApiKey}&format=json&limit={limit}";
            var response = await _httpClient.GetAsync(url, cancellationToken);
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
                    long? listeners = t.TryGetProperty("listeners", out var l) && long.TryParse(
                        l.ValueKind == JsonValueKind.String ? l.GetString() : l.GetRawText(), out var count) ? count : null;
                    if (!string.IsNullOrEmpty(name) && !string.IsNullOrEmpty(artist))
                        tracks.Add(new SimilarTrack(artist!, name!, 1.0, Listeners: listeners));
                }
            }
            _logger.LogInformation("Last.fm track.search '{Q}' -> {N} tracks", query, tracks.Count);
            return tracks;
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
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
    public async Task<List<SimilarTrack>> GetArtistTopTracksAsync(string artist, int limit = 30,
        CancellationToken cancellationToken = default)
    {
        if (string.IsNullOrWhiteSpace(artist)) return new List<SimilarTrack>();
        try
        {
            var url = $"{BaseUrl}?method=artist.gettoptracks&artist={Uri.EscapeDataString(artist)}&api_key={_settings.ApiKey}&format=json&limit={limit}";
            var response = await _httpClient.GetAsync(url, cancellationToken);
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
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _logger.LogWarning(ex, "Last.fm artist.gettoptracks failed for '{A}'", artist);
            return new List<SimilarTrack>();
        }
    }

    public sealed record SimilarArtist(string Name, double Match);
    public sealed record TrackInfo(string Artist, string Title, string? Album, int? Duration,
        IReadOnlyList<string> Tags);

    public Task<List<SimilarArtist>> GetSimilarArtistsAsync(string artist, int limit = 20,
        CancellationToken cancellationToken = default) => CachedAsync(
        $"artist-similar|{artist}|{limit}", async ct =>
        {
            using var doc = await GetDocumentAsync("artist.getsimilar",
                new Dictionary<string, string> { ["artist"] = artist, ["limit"] = limit.ToString() }, ct);
            if (doc is null || !TryArray(doc.RootElement, "similarartists", "artist", out var values)) return [];
            return values.EnumerateArray().Select(item => new SimilarArtist(
                    Text(item, "name"), Number(item, "match")))
                .Where(item => item.Name.Length > 0).Take(limit).ToList();
        }, cancellationToken);

    public Task<List<string>> GetArtistTopTagsAsync(string artist, int limit = 10,
        CancellationToken cancellationToken = default) => GetTagsAsync(
        "artist.gettoptags", new() { ["artist"] = artist }, $"artist-tags|{artist}|{limit}", limit,
        cancellationToken);

    public Task<List<string>> GetTrackTopTagsAsync(string artist, string title, int limit = 10,
        CancellationToken cancellationToken = default) => GetTagsAsync(
        "track.gettoptags", new() { ["artist"] = artist, ["track"] = title },
        $"track-tags|{artist}|{title}|{limit}", limit, cancellationToken);

    public Task<List<SimilarTrack>> GetTagTopTracksAsync(string tag, int limit = 50,
        CancellationToken cancellationToken = default) => CachedAsync(
        $"tag-tracks|{tag}|{limit}", async ct =>
        {
            using var doc = await GetDocumentAsync("tag.gettoptracks",
                new Dictionary<string, string> { ["tag"] = tag, ["limit"] = limit.ToString() }, ct);
            if (doc is null || !TryArray(doc.RootElement, "tracks", "track", out var values)) return [];
            return values.EnumerateArray().Select(item => new SimilarTrack(
                    Text(item, "artist", "name"), Text(item, "name"), 1,
                    DurationSeconds(item)))
                .Where(item => item.Artist.Length > 0 && item.Title.Length > 0).Take(limit).ToList();
        }, cancellationToken);

    public Task<TrackInfo?> GetTrackInfoAsync(string artist, string title,
        CancellationToken cancellationToken = default) => CachedAsync<TrackInfo?>(
        $"track-info|{artist}|{title}", async ct =>
        {
            using var doc = await GetDocumentAsync("track.getInfo",
                new Dictionary<string, string> { ["artist"] = artist, ["track"] = title }, ct);
            if (doc is null || !doc.RootElement.TryGetProperty("track", out var track)) return null;
            var tags = new List<string>();
            if (TryArray(track, "toptags", "tag", out var values))
                tags.AddRange(values.EnumerateArray().Select(item => Text(item, "name"))
                    .Where(value => value.Length > 0));
            return new TrackInfo(Text(track, "artist", "name"), Text(track, "name"),
                Text(track, "album", "title") is { Length: > 0 } album ? album : null,
                DurationSeconds(track), tags);
        }, cancellationToken);

    private Task<List<string>> GetTagsAsync(string method, Dictionary<string, string> parameters,
        string cacheKey, int limit, CancellationToken cancellationToken) => CachedAsync(cacheKey, async ct =>
    {
        using var doc = await GetDocumentAsync(method, parameters, ct);
        if (doc is null || !TryArray(doc.RootElement, "toptags", "tag", out var values)) return [];
        return values.EnumerateArray().Select(item => Text(item, "name"))
            .Where(value => value.Length > 0).Take(limit).ToList();
    }, cancellationToken);

    private async Task<T> CachedAsync<T>(string key, Func<CancellationToken, Task<T>> load,
        CancellationToken cancellationToken)
    {
        key = key.ToLowerInvariant();
        if (_radioCache.TryGetValue(key, out var cached) && cached.Expiry > DateTime.UtcNow
            && cached.Value is T typed) return typed;
        var value = await load(cancellationToken);
        _radioCache[key] = (DateTime.UtcNow.AddHours(_settings.EffectiveRadioCacheDurationHours), value!);
        return value;
    }

    private async Task<JsonDocument?> GetDocumentAsync(string method,
        IReadOnlyDictionary<string, string> parameters, CancellationToken cancellationToken)
    {
        if (!HasApiKey) return null;
        await _providerGate.WaitAsync(cancellationToken);
        try
        {
            var query = new Dictionary<string, string>(parameters)
            {
                ["method"] = method, ["api_key"] = _settings.ApiKey, ["format"] = "json"
            };
            var url = BaseUrl + "?" + string.Join("&", query.Select(pair =>
                $"{Uri.EscapeDataString(pair.Key)}={Uri.EscapeDataString(pair.Value)}"));
            using var response = await _httpClient.GetAsync(url, cancellationToken);
            if ((int)response.StatusCode == 429)
            {
                _logger.LogWarning("Last.fm rate limited {Method}", method);
                return null;
            }
            if (!response.IsSuccessStatusCode) return null;
            await using var stream = await response.Content.ReadAsStreamAsync(cancellationToken);
            return await JsonDocument.ParseAsync(stream, cancellationToken: cancellationToken);
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _logger.LogWarning(ex, "Last.fm {Method} failed", method);
            return null;
        }
        finally { _providerGate.Release(); }
    }

    private static bool TryArray(JsonElement root, string container, string array,
        out JsonElement values)
    {
        values = default;
        return root.TryGetProperty(container, out var parent)
            && parent.TryGetProperty(array, out values)
            && values.ValueKind == JsonValueKind.Array;
    }

    private static string Text(JsonElement element, string property) =>
        element.TryGetProperty(property, out var value) ? value.ToString() : "";

    private static string Text(JsonElement element, string parent, string property) =>
        element.TryGetProperty(parent, out var value) && value.ValueKind == JsonValueKind.Object
            ? Text(value, property) : "";

    private static double Number(JsonElement element, string property) =>
        element.TryGetProperty(property, out var value)
            && double.TryParse(value.ToString(), System.Globalization.NumberStyles.Float,
                System.Globalization.CultureInfo.InvariantCulture, out var number) ? number : 0;

    private static int? DurationSeconds(JsonElement element)
    {
        if (!element.TryGetProperty("duration", out var value)
            || !long.TryParse(value.ToString(), out var milliseconds) || milliseconds <= 0) return null;
        return milliseconds > 1000 ? (int)(milliseconds / 1000) : (int)milliseconds;
    }
}
