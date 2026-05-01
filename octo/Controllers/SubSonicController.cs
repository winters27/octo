using Microsoft.AspNetCore.Mvc;
using System.Xml.Linq;
using System.Text;
using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Models.Download;
using Octo.Models.Search;
using Octo.Models.Subsonic;
using Octo.Services;
using Octo.Services.Common;
using Octo.Services.Local;
using Octo.Services.Subsonic;
using Octo.Services.LastFm;
using Octo.Services.CoverArt;
using Octo.Services.Soulseek;

namespace Octo.Controllers;

[ApiController]
[Route("")]
public class SubsonicController : ControllerBase
{
    private readonly SubsonicSettings _subsonicSettings;
    private readonly IMusicMetadataService _metadataService;
    private readonly ILocalLibraryService _localLibraryService;
    private readonly IDownloadService _downloadService;
    private readonly SubsonicRequestParser _requestParser;
    private readonly SubsonicResponseBuilder _responseBuilder;
    private readonly SubsonicModelMapper _modelMapper;
    private readonly SubsonicProxyService _proxyService;
    private readonly PlaylistSyncService? _playlistSyncService;
    private readonly LastFmService? _lastFmService;
    private readonly LastFmSettings _lastFmSettings;
    private readonly CoverArtService? _coverArtService;
    private readonly CoverArtAggregator? _coverArtAggregator;
    private readonly ExternalIdRegistry _idRegistry;
    private readonly RadioQueueStore _radioQueueStore;
    private readonly ILogger<SubsonicController> _logger;

    public SubsonicController(
        IOptions<SubsonicSettings> subsonicSettings,
        IMusicMetadataService metadataService,
        ILocalLibraryService localLibraryService,
        IDownloadService downloadService,
        SubsonicRequestParser requestParser,
        SubsonicResponseBuilder responseBuilder,
        SubsonicModelMapper modelMapper,
        SubsonicProxyService proxyService,
        ExternalIdRegistry idRegistry,
        RadioQueueStore radioQueueStore,
        ILogger<SubsonicController> logger,
        IOptions<LastFmSettings> lastFmSettings,
        PlaylistSyncService? playlistSyncService = null,
        LastFmService? lastFmService = null,
        CoverArtService? coverArtService = null,
        CoverArtAggregator? coverArtAggregator = null)
    {
        _subsonicSettings = subsonicSettings.Value;
        _metadataService = metadataService;
        _localLibraryService = localLibraryService;
        _downloadService = downloadService;
        _requestParser = requestParser;
        _responseBuilder = responseBuilder;
        _modelMapper = modelMapper;
        _proxyService = proxyService;
        _idRegistry = idRegistry;
        _radioQueueStore = radioQueueStore;
        _playlistSyncService = playlistSyncService;
        _lastFmService = lastFmService;
        _lastFmSettings = lastFmSettings.Value;
        _coverArtService = coverArtService;
        _coverArtAggregator = coverArtAggregator;
        _logger = logger;

        if (string.IsNullOrWhiteSpace(_subsonicSettings.Url))
        {
            throw new Exception("Error: Environment variable SUBSONIC_URL is not set.");
        }
    }

    // ---------------------------------------------------------------------
    // getRandomSongs — pure shuffle. Pass straight through to Navidrome.
    // The actual "radio from this song" feature is getSimilarSongs2 below.
    // ---------------------------------------------------------------------
    [HttpGet]
    [HttpPost]
    [Route("rest/getRandomSongs")]
    [Route("rest/getRandomSongs.view")]
    public async Task<IActionResult> GetRandomSongs()
    {
        var parametersIn = await ExtractAllParameters();
        var passthrough = await _proxyService.RelayAsync("rest/getRandomSongs", parametersIn);
        return new ContentResult
        {
            Content = System.Text.Encoding.UTF8.GetString(passthrough.Body),
            ContentType = passthrough.ContentType ?? "application/json",
            StatusCode = 200
        };
    }

    // Old getRandomSongs hijack — DISABLED, kept for reference only.
    private async Task<IActionResult> GetRandomSongs_DISABLED_HIJACK()
    {
        var parameters = await ExtractAllParameters();
        var format = parameters.GetValueOrDefault("f", "xml");
        var size = int.TryParse(parameters.GetValueOrDefault("size", "10"), out var n) ? n : 10;

        // 1. Ask Navidrome for ONE random song to use as a Last.fm seed.
        string? seedArtist = null;
        string? seedTitle = null;
        try
        {
            var seedParams = new Dictionary<string, string>(parameters) { ["size"] = "1", ["f"] = "json" };
            var seedResult = await _proxyService.RelayAsync("rest/getRandomSongs", seedParams);
            using var seedDoc = JsonDocument.Parse(System.Text.Encoding.UTF8.GetString(seedResult.Body));
            if (seedDoc.RootElement.TryGetProperty("subsonic-response", out var seedResp) &&
                seedResp.TryGetProperty("randomSongs", out var rs) &&
                rs.TryGetProperty("song", out var seedSongs) &&
                seedSongs.ValueKind == JsonValueKind.Array &&
                seedSongs.GetArrayLength() > 0)
            {
                var seed = seedSongs[0];
                seedArtist = seed.TryGetProperty("artist", out var a) ? a.GetString() : null;
                seedTitle = seed.TryGetProperty("title", out var t) ? t.GetString() : null;
                // Collaboration tracks tagged "ArtistA • ArtistB" / "ArtistA & ArtistB" /
                // "ArtistA feat. ArtistB" don't exist in Last.fm as compound artists.
                // Strip to the primary artist so we get back useful similars.
                seedArtist = NormalizeSeedArtist(seedArtist);
                seedTitle  = NormalizeSeedTitle(seedTitle);
            }
        }
        catch (Exception ex)
        {
            _logger.LogWarning(ex, "getRandomSongs: seed fetch from Navidrome failed; passing through");
        }

        // 2. If we have a seed and Last.fm is wired up, build the radio queue.
        if (!string.IsNullOrEmpty(seedArtist) && !string.IsNullOrEmpty(seedTitle) && _lastFmService != null)
        {
            try
            {
                _logger.LogInformation("getRandomSongs radio seed: {Artist} - {Title}", seedArtist, seedTitle);

                // Cap resolution count: Arpeggio's HTTP client times out around 20-30s. Each
                // YouTube search costs 2-8s through the shim's gate, so we need a tight bound.
                var resolveCap = Math.Min(size, 6);
                var similar = await _lastFmService.GetSimilarTracksAsync(seedArtist!, seedTitle!, resolveCap);
                if (similar.Count > 0)
                {
                    var resolveTasks = similar.Take(resolveCap).Select(async t =>
                    {
                        try
                        {
                            var hits = await _metadataService.SearchSongsByArtistTitleAsync(t.Artist, t.Title, 1, t.Duration);
                            return hits.Count > 0 ? hits[0] : null;
                        }
                        catch { return null; }
                    });
                    var resolved = (await Task.WhenAll(resolveTasks)).Where(s => s != null).Cast<Song>().ToList();

                    if (resolved.Count > 0)
                    {
                        _logger.LogInformation("getRandomSongs radio: resolved {Count}/{Total} similar tracks via YouTube",
                            resolved.Count, similar.Count);
                        return BuildRandomSongsResponse(format, resolved);
                    }
                }
            }
            catch (Exception ex)
            {
                _logger.LogWarning(ex, "getRandomSongs radio path failed, falling back to Navidrome random");
            }
        }

        // 3. Fallback: just proxy the original request to Navidrome.
        var passthrough = await _proxyService.RelayAsync("rest/getRandomSongs", parameters);
        return new ContentResult
        {
            Content = System.Text.Encoding.UTF8.GetString(passthrough.Body),
            ContentType = passthrough.ContentType ?? "application/json",
            StatusCode = 200
        };
    }

    private static string? NormalizeSeedArtist(string? artist)
    {
        if (string.IsNullOrWhiteSpace(artist)) return artist;
        // Common collaboration separators. We want only the FIRST artist for
        // Last.fm similar-tracks lookups; Last.fm doesn't index compound names.
        var separators = new[] { " • ", " · ", " & ", " feat. ", " feat ", " ft. ", " ft ", " x ", " X ", " / ", ", ", " with " };
        var s = artist;
        foreach (var sep in separators)
        {
            var idx = s.IndexOf(sep, StringComparison.OrdinalIgnoreCase);
            if (idx > 0) s = s[..idx];
        }
        return s.Trim();
    }

    private static string? NormalizeSeedTitle(string? title)
    {
        if (string.IsNullOrWhiteSpace(title)) return title;
        // Strip "(feat. X)", "[feat X]", "(with Y)" parentheticals from the title
        // so the seed lookup matches the canonical Last.fm track name.
        var s = System.Text.RegularExpressions.Regex.Replace(
            title,
            @"\s*[\(\[](?:feat\.?|featuring|with|ft\.?)[^\)\]]*[\)\]]\s*",
            "",
            System.Text.RegularExpressions.RegexOptions.IgnoreCase);
        return s.Trim();
    }

    private IActionResult BuildRandomSongsResponse(string format, List<Song> songs)
    {
        if (format == "json")
        {
            var jsonSongs = songs.Select(s => _responseBuilder.ConvertSongToJson(s)).ToList();
            return _responseBuilder.CreateJsonResponse(new
            {
                status = "ok",
                version = "1.16.1",
                randomSongs = new { song = jsonSongs }
            });
        }
        // XML fallback (rare; Arpeggio uses JSON)
        return _responseBuilder.CreateResponse(format, "randomSongs", new { song = songs });
    }

    // Extract all parameters (query + body)
    private async Task<Dictionary<string, string>> ExtractAllParameters()
    {
        return await _requestParser.ExtractAllParametersAsync(Request);
    }

    /// <summary>
    /// Search3 hijack. We OWN search results: ~90% Last.fm-driven external songs
    /// (YouTube-resolved on play), ~10% local matches at the bottom for things
    /// that genuinely live in the user's library. This is intentional — the goal
    /// is music DISCOVERY, not library navigation. Library navigation lives in
    /// getAlbumList2, getArtists, etc., which still pass through to Navidrome.
    ///
    /// Empty queries do still pass through so a Subsonic client's "browse all"
    /// fallback isn't broken; with a query, we hijack.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/search3")]
    [Route("rest/search3.view")]
    [Route("rest/search2")]
    [Route("rest/search2.view")]
    public async Task<IActionResult> Search3()
    {
        var parameters = await ExtractAllParameters();
        var query = parameters.GetValueOrDefault("query", "");
        var format = parameters.GetValueOrDefault("f", "xml");

        var cleanQuery = query.Trim().Trim('"');

        if (string.IsNullOrWhiteSpace(cleanQuery))
        {
            try
            {
                var endpoint = (Request.Path.Value ?? "").Contains("search2", StringComparison.OrdinalIgnoreCase)
                    ? "rest/search2" : "rest/search3";
                var result = await _proxyService.RelayAsync(endpoint, parameters);
                var contentType = result.ContentType ?? $"application/{format}";
                return File(result.Body, contentType);
            }
            catch
            {
                return _responseBuilder.CreateResponse(format, "searchResult3", new { });
            }
        }

        var requestedSongs   = int.TryParse(parameters.GetValueOrDefault("songCount",   "20"), out var sc)  ? sc  : 20;
        var requestedAlbums  = int.TryParse(parameters.GetValueOrDefault("albumCount",  "20"), out var ac)  ? ac  : 20;
        var requestedArtists = int.TryParse(parameters.GetValueOrDefault("artistCount", "20"), out var arc) ? arc : 20;

        // Always include local results. The earlier behavior special-cased
        // songCount>=200 (Arpeggi's default) to suppress local songs entirely
        // because at that time clients were using search3 as their radio
        // source and locals would crowd out external recommendations. Now
        // radio goes through getSimilarSongs2 (where we do local-first
        // resolution), so search3 is "search" again — locals belong here.
        // Generous local target so a search for "Drake" surfaces all your
        // owned Drake songs alongside the YouTube discoveries; Navidrome
        // returns at most what it actually has anyway.
        var localSongTarget = Math.Max(20, requestedSongs / 4);

        // Cap external generation. For songCount=2000 we used to fan out to
        // ~1800 Last.fm tracks per call — fast at filling the registry's 10k
        // LRU and ratelimits Last.fm. ~150 is plenty for any reasonable queue
        // and the registry survives across many sessions before recycling.
        const int MaxExternalPerQuery = 150;
        var externalTarget = Math.Min(MaxExternalPerQuery, Math.Max(0, requestedSongs - localSongTarget));

        // External: Last.fm fan-out. We try track.search first (fuzzy matches
        // anywhere in the query), then top up with the canonical artist's top
        // tracks if we're short. Each Last.fm hit becomes an instant placeholder
        // song via SoulseekMetadataService — no yt-dlp call until /rest/stream.
        var externalSongs = await BuildExternalSearchResultsAsync(cleanQuery, externalTarget);

        // Local pass-through. Albums/artists always get the full requested counts;
        // song-side gets the local target.
        var localProxyEndpoint = (Request.Path.Value ?? "").Contains("search2", StringComparison.OrdinalIgnoreCase)
            ? "rest/search2" : "rest/search3";
        var localParams = new Dictionary<string, string>(parameters)
        {
            ["songCount"]   = localSongTarget.ToString(),
            ["albumCount"]  = requestedAlbums.ToString(),
            ["artistCount"] = requestedArtists.ToString(),
        };
        var localResult = await _proxyService.RelaySafeAsync(localProxyEndpoint, localParams);

        var playlistTask = _subsonicSettings.EnableExternalPlaylists
            ? await _metadataService.SearchPlaylistsAsync(cleanQuery, requestedAlbums)
            : new List<ExternalPlaylist>();

        var externalResult = new SearchResult
        {
            Songs = externalSongs,
            Albums = new List<Album>(),
            Artists = new List<Artist>(),
        };

        // Track this response as a "queue" so a later scrobble for any of its
        // songs can drive the sliding-window prewarm of upcoming externals.
        // Order matches the merged response order — local first, external after.
        var localSongIds = ExtractLocalSongIds(localResult.Body, localResult.ContentType);
        _radioQueueStore.Register(localSongIds.Concat(externalSongs.Select(s => s.Id)));

        return MergeSearchResults(localResult, externalResult, playlistTask, format);
    }

    /// <summary>
    /// Pulls just the song-id strings out of a Subsonic search3 response body,
    /// preserving response order. Both JSON and XML shapes are supported because
    /// Navidrome respects the f= parameter the proxy forwards.
    /// </summary>
    private static List<string> ExtractLocalSongIds(byte[]? body, string? contentType)
    {
        if (body == null || body.Length == 0) return new List<string>();
        var ids = new List<string>();
        try
        {
            if (contentType?.Contains("xml") == true)
            {
                var doc = XDocument.Load(new System.IO.MemoryStream(body));
                var ns = doc.Root?.Name.Namespace ?? XNamespace.None;
                var nodes = doc.Descendants(ns + "song");
                foreach (var n in nodes)
                {
                    var id = n.Attribute("id")?.Value;
                    if (!string.IsNullOrEmpty(id)) ids.Add(id);
                }
            }
            else
            {
                using var doc = JsonDocument.Parse(body);
                if (doc.RootElement.TryGetProperty("subsonic-response", out var resp)
                    && (resp.TryGetProperty("searchResult3", out var sr) || resp.TryGetProperty("searchResult2", out sr))
                    && sr.TryGetProperty("song", out var songs)
                    && songs.ValueKind == JsonValueKind.Array)
                {
                    foreach (var s in songs.EnumerateArray())
                    {
                        if (s.TryGetProperty("id", out var idEl) && idEl.ValueKind == JsonValueKind.String)
                        {
                            var id = idEl.GetString();
                            if (!string.IsNullOrEmpty(id)) ids.Add(id);
                        }
                    }
                }
            }
        }
        catch { /* malformed upstream response — return whatever we got */ }
        return ids;
    }

    /// <summary>
    /// Fans out a free-form query to Last.fm and returns up to <paramref name="target"/>
    /// instant-placeholder external songs. Order:
    ///   1. track.search hits (best fuzzy matches for the query as typed)
    ///   2. canonical artist's top tracks (in case (1) was thin — common for
    ///      single-word artist queries)
    /// We dedupe by artist+title to avoid the same track appearing twice.
    /// </summary>
    private async Task<List<Song>> BuildExternalSearchResultsAsync(string query, int target)
    {
        if (target <= 0 || _lastFmService is null || !_lastFmService.IsConfigured)
            return new List<Song>();

        var seen = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        var collected = new List<(string Artist, string Title)>();

        var tracks = await _lastFmService.SearchTracksAsync(query, Math.Min(50, target * 2));
        foreach (var t in tracks)
        {
            var key = $"{t.Artist}|{t.Title}".ToLowerInvariant();
            if (seen.Add(key)) collected.Add((t.Artist, t.Title));
            if (collected.Count >= target) break;
        }

        if (collected.Count < target)
        {
            // Use the first track-search hit's artist as the canonical anchor
            // for top-tracks padding. Falls back to the raw query string when
            // track.search came back empty.
            var anchor = tracks.Count > 0 ? tracks[0].Artist : query;
            var topTracks = await _lastFmService.GetArtistTopTracksAsync(anchor, target * 2);
            foreach (var t in topTracks)
            {
                var key = $"{t.Artist}|{t.Title}".ToLowerInvariant();
                if (seen.Add(key)) collected.Add((t.Artist, t.Title));
                if (collected.Count >= target) break;
            }
        }

        var songs = new List<Song>(collected.Count);
        foreach (var (artist, title) in collected)
        {
            var hits = await _metadataService.SearchSongsByArtistTitleAsync(artist, title, 1);
            if (hits.Count > 0) songs.Add(hits[0]);
        }
        _logger.LogInformation("External search '{Q}' -> {N} placeholder songs", query, songs.Count);

        // Fire-and-forget: pre-resolve YouTube videoIds for the top hits so the
        // first /rest/stream click doesn't pay the cold yt-dlp double-call cost
        // (ytsearch1: + -g, 6-16s combined). Arpeggi cancels at ~10s and falls
        // back to a local song; without this, external playback is unreachable
        // from that client. 12 ≈ what fits on the first page of search results.
        _ = _metadataService.PrewarmYouTubeIdsAsync(songs, topN: 12);

        return songs;
    }

    /// <summary>
    /// Downloads on-the-fly if needed, or streams directly in Stream mode.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/stream")]
    [Route("rest/stream.view")]
    public async Task<IActionResult> Stream()
    {
        var parameters = await ExtractAllParameters();
        var id = parameters.GetValueOrDefault("id", "");

        if (string.IsNullOrWhiteSpace(id))
        {
            return BadRequest(new { error = "Missing id parameter" });
        }

        var (isExternal, provider, externalId) = _localLibraryService.ParseSongId(id);

        // Verbose entry log: every stream call gets a single line tagged with
        // the client + id + isExternal + Range + UA + key headers. Diagnostics
        // for "client X never plays external songs" — if a tap doesn't even
        // reach this log line, the client is filtering on its side.
        var clientName = parameters.GetValueOrDefault("c", "?");
        var rangeIn = Request.Headers.TryGetValue("Range", out var rngVal) ? rngVal.ToString() : "(none)";
        var uaIn = Request.Headers.TryGetValue("User-Agent", out var uaVal) ? uaVal.ToString() : "(none)";
        _logger.LogInformation(
            "STREAM-IN client={Client} id={Id} isExternal={IsExt} range={Range} ua={Ua}",
            clientName, id, isExternal, rangeIn, uaIn);

        if (!isExternal)
        {
            return await _proxyService.RelayStreamAsync(parameters, HttpContext.RequestAborted);
        }

        // Check for existing local file first (even in Stream mode, use local if available)
        var localPath = await _localLibraryService.GetLocalPathForExternalSongAsync(provider!, externalId!);

        if (localPath != null && System.IO.File.Exists(localPath))
        {
            var stream = System.IO.File.OpenRead(localPath);
            return File(stream, GetContentType(localPath), enableRangeProcessing: true);
        }

        try
        {
            // True streaming mode: proxy directly from CDN without saving to disk.
            if (_subsonicSettings.StorageMode == StorageMode.Stream)
            {
                // Forward the client's Range header up the chain so the shim can
                // ask googlevideo for the requested byte range and we can return
                // a proper 206. iOS Subsonic clients refuse to play non-FLAC
                // audio without working byte-range support — our prior 200/none
                // response was what was making Arpeggi/Narjo silently drop
                // every external song from the queue.
                var rangeHeader = Request.Headers.TryGetValue("Range", out var rh) ? rh.ToString() : null;

                var directStream = await _downloadService.GetDirectStreamAsync(
                    provider!, externalId!, rangeHeader, HttpContext.RequestAborted);
                if (directStream != null)
                {
                    _logger.LogInformation("Direct streaming track {Id} ({Quality}, status={Status})",
                        id, directStream.Quality, directStream.StatusCode);

                    // Manual stream copy: ASP.NET's File(...) requires a seekable
                    // stream for Range support, but our network stream isn't
                    // seekable. Instead we forward the upstream's status code +
                    // Content-Range verbatim and copy bytes to the response body.
                    Response.StatusCode = directStream.StatusCode;
                    Response.Headers["Content-Type"] = directStream.ContentType;
                    Response.Headers["Accept-Ranges"] = "bytes";
                    if (directStream.ContentLength.HasValue)
                    {
                        Response.Headers["Content-Length"] = directStream.ContentLength.Value.ToString();
                    }
                    if (!string.IsNullOrEmpty(directStream.ContentRange))
                    {
                        Response.Headers["Content-Range"] = directStream.ContentRange;
                    }

                    try
                    {
                        await using (directStream.AudioStream)
                        {
                            await directStream.AudioStream.CopyToAsync(Response.Body, HttpContext.RequestAborted);
                        }
                    }
                    catch (OperationCanceledException)
                    {
                        // Client disconnected mid-stream. Normal — don't log as error.
                    }
                    return new EmptyResult();
                }
                // Fallback to download mode if direct stream not available
                _logger.LogWarning("Direct stream not available, falling back to download mode");
            }

            // Cache/Permanent mode: download first, then stream
            var downloadStream = await _downloadService.DownloadAndStreamAsync(provider!, externalId!, HttpContext.RequestAborted);
            return File(downloadStream, "audio/mpeg", enableRangeProcessing: true);
        }
        catch (Exception ex)
        {
            _logger.LogError(ex, "Failed to stream track {Id}", id);
            return StatusCode(500, new { error = $"Failed to stream: {ex.Message}" });
        }
    }

    /// <summary>
    /// Returns external song info if needed.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/getSong")]
    [Route("rest/getSong.view")]
    public async Task<IActionResult> GetSong()
    {
        var parameters = await ExtractAllParameters();
        var id = parameters.GetValueOrDefault("id", "");
        var format = parameters.GetValueOrDefault("f", "xml");

        if (string.IsNullOrWhiteSpace(id))
        {
            return _responseBuilder.CreateError(format, 10, "Missing id parameter");
        }

        var (isExternal, provider, externalId) = _localLibraryService.ParseSongId(id);

        if (!isExternal)
        {
            var result = await _proxyService.RelayAsync("rest/getSong", parameters);
            var contentType = result.ContentType ?? $"application/{format}";
            return File(result.Body, contentType);
        }

        var song = await _metadataService.GetSongAsync(provider!, externalId!);

        if (song == null)
        {
            return _responseBuilder.CreateError(format, 70, "Song not found");
        }

        return _responseBuilder.CreateSongResponse(format, song);
    }

    /// <summary>
    /// Merges local and Deezer albums.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/getArtist")]
    [Route("rest/getArtist.view")]
    public async Task<IActionResult> GetArtist()
    {
        var parameters = await ExtractAllParameters();
        var id = parameters.GetValueOrDefault("id", "");
        var format = parameters.GetValueOrDefault("f", "xml");

        if (string.IsNullOrWhiteSpace(id))
        {
            return _responseBuilder.CreateError(format, 10, "Missing id parameter");
        }

        var (isExternal, provider, externalId) = _localLibraryService.ParseSongId(id);

        if (isExternal)
        {
            var artist = await _metadataService.GetArtistAsync(provider!, externalId!);
            if (artist == null)
            {
                return _responseBuilder.CreateError(format, 70, "Artist not found");
            }

            var albums = await _metadataService.GetArtistAlbumsAsync(provider!, externalId!);
            
            // Fill artist info for each album (Deezer API doesn't include it in artist/albums endpoint)
            foreach (var album in albums)
            {
                if (string.IsNullOrEmpty(album.Artist))
                {
                    album.Artist = artist.Name;
                }
                if (string.IsNullOrEmpty(album.ArtistId))
                {
                    album.ArtistId = artist.Id;
                }
            }
            
            return _responseBuilder.CreateArtistResponse(format, artist, albums);
        }

        var navidromeResult = await _proxyService.RelaySafeAsync("rest/getArtist", parameters);
        
        if (!navidromeResult.Success || navidromeResult.Body == null)
        {
            return _responseBuilder.CreateError(format, 70, "Artist not found");
        }

        var navidromeContent = Encoding.UTF8.GetString(navidromeResult.Body);
        string artistName = "";
        string localArtistId = id; // Keep the local artist ID for merged albums
        var localAlbums = new List<object>();
        object? artistData = null;

        if (format == "json" || navidromeResult.ContentType?.Contains("json") == true)
        {
            var jsonDoc = JsonDocument.Parse(navidromeContent);
            if (jsonDoc.RootElement.TryGetProperty("subsonic-response", out var response) &&
                response.TryGetProperty("artist", out var artistElement))
            {
                artistName = artistElement.TryGetProperty("name", out var name) ? name.GetString() ?? "" : "";
                artistData = _responseBuilder.ConvertSubsonicJsonElement(artistElement, true);
                
                if (artistElement.TryGetProperty("album", out var albums))
                {
                    foreach (var album in albums.EnumerateArray())
                    {
                        localAlbums.Add(_responseBuilder.ConvertSubsonicJsonElement(album, true));
                    }
                }
            }
        }

        if (string.IsNullOrEmpty(artistName) || artistData == null)
        {
            return File(navidromeResult.Body, navidromeResult.ContentType ?? "application/json");
        }

        var deezerArtists = await _metadataService.SearchArtistsAsync(artistName, 1);
        var deezerAlbums = new List<Album>();
        
        if (deezerArtists.Count > 0)
        {
            var deezerArtist = deezerArtists[0];
            if (deezerArtist.Name.Equals(artistName, StringComparison.OrdinalIgnoreCase))
            {
                deezerAlbums = await _metadataService.GetArtistAlbumsAsync("deezer", deezerArtist.ExternalId!);
                
                // Fill artist info for each album (Deezer API doesn't include it in artist/albums endpoint)
                // Use local artist ID and name so albums link back to the local artist
                foreach (var album in deezerAlbums)
                {
                    if (string.IsNullOrEmpty(album.Artist))
                    {
                        album.Artist = artistName;
                    }
                    if (string.IsNullOrEmpty(album.ArtistId))
                    {
                        album.ArtistId = localArtistId;
                    }
                }
            }
        }

        var localAlbumNames = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        foreach (var album in localAlbums)
        {
            if (album is Dictionary<string, object> dict && dict.TryGetValue("name", out var nameObj))
            {
                localAlbumNames.Add(nameObj?.ToString() ?? "");
            }
        }

        var mergedAlbums = localAlbums.ToList();
        foreach (var deezerAlbum in deezerAlbums)
        {
            if (!localAlbumNames.Contains(deezerAlbum.Title))
            {
                mergedAlbums.Add(_responseBuilder.ConvertAlbumToJson(deezerAlbum));
            }
        }

        if (artistData is Dictionary<string, object> artistDict)
        {
            artistDict["album"] = mergedAlbums;
            artistDict["albumCount"] = mergedAlbums.Count;
        }

        return _responseBuilder.CreateJsonResponse(new
        {
            status = "ok",
            version = "1.16.1",
            artist = artistData
        });
    }

    /// <summary>
    /// Enriches local albums with Deezer songs.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/getAlbum")]
    [Route("rest/getAlbum.view")]
    public async Task<IActionResult> GetAlbum()
    {
        var parameters = await ExtractAllParameters();
        var id = parameters.GetValueOrDefault("id", "");
        var format = parameters.GetValueOrDefault("f", "xml");

        if (string.IsNullOrWhiteSpace(id))
        {
            return _responseBuilder.CreateError(format, 10, "Missing id parameter");
        }
        
        // Check if this is an external playlist
        if (PlaylistIdHelper.IsExternalPlaylist(id))
        {
            try
            {
                var (provider, externalId) = PlaylistIdHelper.ParsePlaylistId(id);
                
                // Get playlist metadata
                var playlist = await _metadataService.GetPlaylistAsync(provider, externalId);
                if (playlist == null)
                {
                    return _responseBuilder.CreateError(format, 70, "Playlist not found");
                }
                
                // Get playlist tracks
                var tracks = await _metadataService.GetPlaylistTracksAsync(provider, externalId);
                
                // Add all tracks to playlist cache so when they're played, we know they belong to this playlist
                if (_playlistSyncService != null)
                {
                    foreach (var track in tracks)
                    {
                        if (!string.IsNullOrEmpty(track.ExternalId))
                        {
                            var trackId = $"ext-{provider}-{track.ExternalId}";
                            _playlistSyncService.AddTrackToPlaylistCache(trackId, id);
                        }
                    }
                    
                    _logger.LogDebug("Added {TrackCount} tracks to playlist cache for {PlaylistId}", tracks.Count, id);
                }
                
                // Convert to album response (playlist as album)
                return _responseBuilder.CreatePlaylistAsAlbumResponse(format, playlist, tracks);
            }
            catch (Exception ex)
            {
                _logger.LogError(ex, "Error getting playlist {Id}", id);
                return _responseBuilder.CreateError(format, 70, "Playlist not found");
            }
        }

        var (isExternal, albumProvider, albumExternalId) = _localLibraryService.ParseSongId(id);

        if (isExternal)
        {
            var album = await _metadataService.GetAlbumAsync(albumProvider!, albumExternalId!);

            if (album == null)
            {
                return _responseBuilder.CreateError(format, 70, "Album not found");
            }

            return _responseBuilder.CreateAlbumResponse(format, album);
        }

        var navidromeResult = await _proxyService.RelaySafeAsync("rest/getAlbum", parameters);
        
        if (!navidromeResult.Success || navidromeResult.Body == null)
        {
            return _responseBuilder.CreateError(format, 70, "Album not found");
        }

        var navidromeContent = Encoding.UTF8.GetString(navidromeResult.Body);
        string albumName = "";
        string artistName = "";
        var localSongs = new List<object>();
        object? albumData = null;

        if (format == "json" || navidromeResult.ContentType?.Contains("json") == true)
        {
            var jsonDoc = JsonDocument.Parse(navidromeContent);
            if (jsonDoc.RootElement.TryGetProperty("subsonic-response", out var response) &&
                response.TryGetProperty("album", out var albumElement))
            {
                albumName = albumElement.TryGetProperty("name", out var name) ? name.GetString() ?? "" : "";
                artistName = albumElement.TryGetProperty("artist", out var artist) ? artist.GetString() ?? "" : "";
                albumData = _responseBuilder.ConvertSubsonicJsonElement(albumElement, true);
                
                if (albumElement.TryGetProperty("song", out var songs))
                {
                    foreach (var song in songs.EnumerateArray())
                    {
                        localSongs.Add(_responseBuilder.ConvertSubsonicJsonElement(song, true));
                    }
                }
            }
        }

        if (string.IsNullOrEmpty(albumName) || string.IsNullOrEmpty(artistName) || albumData == null)
        {
            return File(navidromeResult.Body, navidromeResult.ContentType ?? "application/json");
        }

        var searchQuery = $"{artistName} {albumName}";
        var deezerAlbums = await _metadataService.SearchAlbumsAsync(searchQuery, 5);
        Album? deezerAlbum = null;
        
        // Find matching album on Deezer (exact match first)
        foreach (var candidate in deezerAlbums)
        {
            if (candidate.Artist != null && 
                candidate.Artist.Equals(artistName, StringComparison.OrdinalIgnoreCase) &&
                candidate.Title.Equals(albumName, StringComparison.OrdinalIgnoreCase))
            {
                deezerAlbum = await _metadataService.GetAlbumAsync("deezer", candidate.ExternalId!);
                break;
            }
        }

        // Fallback to fuzzy match
        if (deezerAlbum == null)
        {
            foreach (var candidate in deezerAlbums)
            {
                if (candidate.Artist != null && 
                    candidate.Artist.Contains(artistName, StringComparison.OrdinalIgnoreCase) &&
                    (candidate.Title.Contains(albumName, StringComparison.OrdinalIgnoreCase) ||
                     albumName.Contains(candidate.Title, StringComparison.OrdinalIgnoreCase)))
                {
                    deezerAlbum = await _metadataService.GetAlbumAsync("deezer", candidate.ExternalId!);
                    break;
                }
            }
        }

        if (deezerAlbum != null && deezerAlbum.Songs.Count > 0)
        {
            var localSongTitles = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
            foreach (var song in localSongs)
            {
                if (song is Dictionary<string, object> dict && dict.TryGetValue("title", out var titleObj))
                {
                    localSongTitles.Add(titleObj?.ToString() ?? "");
                }
            }

            var mergedSongs = localSongs.ToList();
            foreach (var deezerSong in deezerAlbum.Songs)
            {
                if (!localSongTitles.Contains(deezerSong.Title))
                {
                    mergedSongs.Add(_responseBuilder.ConvertSongToJson(deezerSong));
                }
            }

            mergedSongs = mergedSongs
                .OrderBy(s => s is Dictionary<string, object> dict && dict.TryGetValue("track", out var track) 
                    ? Convert.ToInt32(track) 
                    : 0)
                .ToList();

            if (albumData is Dictionary<string, object> albumDict)
            {
                albumDict["song"] = mergedSongs;
                albumDict["songCount"] = mergedSongs.Count;
                
                var totalDuration = 0;
                foreach (var song in mergedSongs)
                {
                    if (song is Dictionary<string, object> dict && dict.TryGetValue("duration", out var dur))
                    {
                        totalDuration += Convert.ToInt32(dur);
                    }
                }
                albumDict["duration"] = totalDuration;
            }
        }

        return _responseBuilder.CreateJsonResponse(new
        {
            status = "ok",
            version = "1.16.1",
            album = albumData
        });
    }

    /// <summary>
    /// Proxies external covers. Uses type from ID to determine which API to call.
    /// Format: ext-{provider}-{type}-{id} (e.g., ext-deezer-artist-259, ext-deezer-album-96126)
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/getCoverArt")]
    [Route("rest/getCoverArt.view")]
    public async Task<IActionResult> GetCoverArt()
    {
        var parameters = await ExtractAllParameters();
        var id = parameters.GetValueOrDefault("id", "");

        if (string.IsNullOrWhiteSpace(id))
        {
            return NotFound();
        }

        // Playlist covers haven't changed — keep the existing path.
        if (PlaylistIdHelper.IsExternalPlaylist(id))
        {
            try
            {
                var (provider, externalId) = PlaylistIdHelper.ParsePlaylistId(id);
                var playlist = await _metadataService.GetPlaylistAsync(provider, externalId);
                if (playlist == null || string.IsNullOrEmpty(playlist.CoverUrl))
                    return ServePlaceholder();

                using var http = new HttpClient();
                var imageResponse = await http.GetAsync(playlist.CoverUrl);
                if (!imageResponse.IsSuccessStatusCode) return ServePlaceholder();
                var imageBytes = await imageResponse.Content.ReadAsByteArrayAsync();
                var contentType = imageResponse.Content.Headers.ContentType?.ToString() ?? "image/jpeg";
                return File(imageBytes, contentType);
            }
            catch (Exception ex)
            {
                _logger.LogError(ex, "Error getting playlist cover art for {Id}", id);
                return ServePlaceholder();
            }
        }

        // Registry-backed id (song / album / artist). Resolve to artist+title via the
        // registry and look the cover up on iTunes. Watermark with the Octo logo so
        // radio-sourced art is visually distinct from local-library art.
        var routing = _idRegistry.Lookup(id);
        if (routing != null)
        {
            try
            {
                var raw = _coverArtAggregator != null ? await _coverArtAggregator.GetCoverAsync(routing) : null;
                if (raw == null)
                {
                    _logger.LogDebug("cover art all-source miss for {Kind} '{A} - {T}/{Al}', serving placeholder",
                        routing.Kind, routing.Artist, routing.Title, routing.Album);
                    return ServePlaceholder();
                }

                var watermarked = _coverArtService?.AddOctoBadge(raw) ?? raw;
                return File(watermarked, "image/jpeg");
            }
            catch (Exception ex)
            {
                _logger.LogWarning(ex, "Cover art pipeline failed for registry id {Id}", id);
                return ServePlaceholder();
            }
        }

        // Legacy "ext-album-{hash}" / "ext-artist-{hash}" ids that pre-date the
        // registry. We can't reverse-resolve them, but returning a 404 makes
        // Arpeggio drop the song, so serve the Octo placeholder instead.
        if (id.StartsWith("ext-album-", StringComparison.OrdinalIgnoreCase)
            || id.StartsWith("ext-artist-", StringComparison.OrdinalIgnoreCase))
        {
            return ServePlaceholder();
        }

        // Existing ext-{provider}-{type}-{id} path (Deezer/Tidal-era, kept for
        // compatibility with any in-flight clients).
        var (isExternal, coverProvider, type, coverExternalId) = _localLibraryService.ParseExternalId(id);
        if (isExternal)
        {
            string? coverUrl = type switch
            {
                "artist" => (await _metadataService.GetArtistAsync(coverProvider!, coverExternalId!))?.ImageUrl,
                "album"  => (await _metadataService.GetAlbumAsync(coverProvider!, coverExternalId!))?.CoverArtUrl,
                _        => (await _metadataService.GetSongAsync(coverProvider!, coverExternalId!))?.CoverArtUrl
                            ?? (await _metadataService.GetAlbumAsync(coverProvider!, coverExternalId!))?.CoverArtUrl,
            };

            if (coverUrl != null)
            {
                using var httpClient = new HttpClient();
                var response = await httpClient.GetAsync(coverUrl);
                if (response.IsSuccessStatusCode)
                {
                    var imageBytes = await response.Content.ReadAsByteArrayAsync();
                    var watermarked = _coverArtService?.AddOctoBadge(imageBytes) ?? imageBytes;
                    return File(watermarked, "image/jpeg");
                }
            }
            return ServePlaceholder();
        }

        // Local library — proxy to Navidrome unchanged.
        try
        {
            var result = await _proxyService.RelayAsync("rest/getCoverArt", parameters);
            var contentType = result.ContentType ?? "image/jpeg";
            return File(result.Body, contentType);
        }
        catch
        {
            return ServePlaceholder();
        }
    }

    /// <summary>
    /// Returns a 200 response with the Octo placeholder JPEG. Used in every code
    /// path that previously returned 404 — Subsonic clients (Arpeggio especially)
    /// drop play-queue entries whose cover-art request fails, so we always serve
    /// something rather than fail.
    /// </summary>
    private IActionResult ServePlaceholder()
    {
        var bytes = _coverArtService?.GetPlaceholderCover();
        if (bytes == null || bytes.Length == 0) return NotFound();
        return File(bytes, "image/jpeg");
    }

    #region Helper Methods

    private IActionResult MergeSearchResults(
        (byte[]? Body, string? ContentType, bool Success) subsonicResult,
        SearchResult externalResult,
        List<ExternalPlaylist> playlistResult,
        string format)
    {
        var (localSongs, localAlbums, localArtists) = subsonicResult.Success && subsonicResult.Body != null
            ? _modelMapper.ParseSearchResponse(subsonicResult.Body, subsonicResult.ContentType)
            : (new List<object>(), new List<object>(), new List<object>());

        var isJson = format == "json" || subsonicResult.ContentType?.Contains("json") == true;
        var (mergedSongs, mergedAlbums, mergedArtists) = _modelMapper.MergeSearchResults(
            localSongs, 
            localAlbums, 
            localArtists, 
            externalResult,
            playlistResult,
            isJson);

        if (isJson)
        {
            return _responseBuilder.CreateJsonResponse(new
            {
                status = "ok",
                version = "1.16.1",
                searchResult3 = new
                {
                    song = mergedSongs,
                    album = mergedAlbums,
                    artist = mergedArtists
                }
            });
        }
        else
        {
            var ns = XNamespace.Get("http://subsonic.org/restapi");
            var searchResult3 = new XElement(ns + "searchResult3");
            
            foreach (var artist in mergedArtists.Cast<XElement>())
            {
                searchResult3.Add(artist);
            }
            foreach (var album in mergedAlbums.Cast<XElement>())
            {
                searchResult3.Add(album);
            }
            foreach (var song in mergedSongs.Cast<XElement>())
            {
                searchResult3.Add(song);
            }

            var doc = new XDocument(
                new XElement(ns + "subsonic-response",
                    new XAttribute("status", "ok"),
                    new XAttribute("version", "1.16.1"),
                    searchResult3
                )
            );

            return Content(doc.ToString(), "application/xml");
        }
    }

    private string GetContentType(string filePath)
    {
        var extension = Path.GetExtension(filePath).ToLowerInvariant();
        return extension switch
        {
            ".mp3" => "audio/mpeg",
            ".flac" => "audio/flac",
            ".ogg" => "audio/ogg",
            ".m4a" => "audio/mp4",
            ".wav" => "audio/wav",
            ".aac" => "audio/aac",
            _ => "audio/mpeg"
        };
    }

    #endregion

    /// <summary>
    /// Stars (favorites) an item. For playlists and external songs, triggers download.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/star")]
    [Route("rest/star.view")]
    public async Task<IActionResult> Star()
    {
        var parameters = await ExtractAllParameters();
        var format = parameters.GetValueOrDefault("f", "xml");
        
        var itemId = parameters.GetValueOrDefault("id", "");
        
        // Check if this is a playlist
        if (!string.IsNullOrEmpty(itemId) && PlaylistIdHelper.IsExternalPlaylist(itemId))
        {
            if (_playlistSyncService == null)
            {
                return _responseBuilder.CreateError(format, 0, "Playlist functionality is not enabled");
            }
            
            _logger.LogInformation("Starring external playlist {PlaylistId}, triggering download", itemId);
            
            // Trigger playlist download in background
            _ = Task.Run(async () =>
            {
                try
                {
                    await _playlistSyncService.DownloadFullPlaylistAsync(itemId);
                }
                catch (Exception ex)
                {
                    _logger.LogError(ex, "Failed to download playlist {PlaylistId}", itemId);
                }
            });
            
            // Return success response immediately
            return _responseBuilder.CreateResponse(format, "starred", new { });
        }
        
        // Check if this is an external song (enables download-on-star)
        var (isExternal, provider, externalId) = _localLibraryService.ParseSongId(itemId);
        
        if (isExternal && _subsonicSettings.DownloadOnStar && 
            (_subsonicSettings.StorageMode == StorageMode.Stream || _subsonicSettings.StorageMode == StorageMode.Cache))
        {
            _logger.LogInformation("Starring external song {SongId}, triggering permanent download", itemId);
            
            // Trigger background download (permanent, ignoring current storage mode)
            _ = Task.Run(async () =>
            {
                try
                {
                    // Force permanent download by calling DownloadSongAsync directly
                    var localPath = await _downloadService.DownloadSongAsync(provider!, externalId!, CancellationToken.None);
                    _logger.LogInformation("Download-on-star completed: {Path}", localPath);
                    
                    // Trigger library scan to register the new file
                    await _localLibraryService.TriggerLibraryScanAsync();
                }
                catch (Exception ex)
                {
                    _logger.LogError(ex, "Failed to download starred song {SongId}", itemId);
                }
            });
            
            // Return success response immediately
            return _responseBuilder.CreateResponse(format, "starred", new { });
        }
        
        // For non-external items or when download-on-star is disabled, relay to real Subsonic server
        try
        {
            var result = await _proxyService.RelayAsync("rest/star", parameters);
            var contentType = result.ContentType ?? $"application/{format}";
            return File(result.Body, contentType);
        }
        catch (HttpRequestException ex)
        {
            return _responseBuilder.CreateError(format, 0, $"Error connecting to Subsonic server: {ex.Message}");
        }
    }

    /// <summary>
    /// Gets similar songs for radio feature using Last.fm recommendations.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/getSimilarSongs")]
    [Route("rest/getSimilarSongs.view")]
    [Route("rest/getSimilarSongs2")]
    [Route("rest/getSimilarSongs2.view")]
    public async Task<IActionResult> GetSimilarSongs()
    {
        var parameters = await ExtractAllParameters();
        var id = parameters.GetValueOrDefault("id", "");
        var format = parameters.GetValueOrDefault("f", "xml");
        var count = int.TryParse(parameters.GetValueOrDefault("count", "50"), out var c) ? c : 50;

        // Subsonic spec: getSimilarSongs.view → key "similarSongs"; getSimilarSongs2.view → "similarSongs2".
        // Clients (Arpeggi) parse the v2 key strictly and ignore v1-shaped responses
        // when they called v2 — that's why the radio queue showed up empty.
        var isV2Request = (Request.Path.Value ?? "").Contains("getSimilarSongs2", StringComparison.OrdinalIgnoreCase);
        var responseKey = isV2Request ? "similarSongs2" : "similarSongs";

        if (string.IsNullOrWhiteSpace(id))
        {
            return _responseBuilder.CreateError(format, 10, "Missing id parameter");
        }

        // Check if Last.fm radio is configured and enabled
        if (_lastFmService == null || !_lastFmService.IsConfigured)
        {
            _logger.LogDebug("Last.fm radio not configured, relaying to upstream server");
            try
            {
                var result = await _proxyService.RelayAsync(Request.Path.Value ?? "rest/getSimilarSongs", parameters);
                return File(result.Body, result.ContentType ?? $"application/{format}");
            }
            catch
            {
                return _responseBuilder.CreateResponse(format, responseKey, new { });
            }
        }

        // Get the seed song metadata
        string artistName = "";
        string trackTitle = "";

        var (isExternal, provider, externalId) = _localLibraryService.ParseSongId(id);

        if (isExternal)
        {
            // External song - get metadata from our service
            var song = await _metadataService.GetSongAsync(provider!, externalId!);
            if (song != null)
            {
                artistName = song.Artist ?? "";
                trackTitle = song.Title;
            }
        }
        else
        {
            // Local song - get metadata from Navidrome
            try
            {
                // Build parameters with auth from original request
                var getSongParams = new Dictionary<string, string>(parameters)
                {
                    ["id"] = id,
                    ["f"] = "json"
                };
                var result = await _proxyService.RelayAsync("rest/getSong", getSongParams);

                var json = System.Text.Encoding.UTF8.GetString(result.Body);
                var doc = JsonDocument.Parse(json);

                if (doc.RootElement.TryGetProperty("subsonic-response", out var response) &&
                    response.TryGetProperty("song", out var songElement))
                {
                    artistName = songElement.TryGetProperty("artist", out var artist) ? artist.GetString() ?? "" : "";
                    trackTitle = songElement.TryGetProperty("title", out var title) ? title.GetString() ?? "" : "";
                }
            }
            catch (Exception ex)
            {
                _logger.LogError(ex, "Failed to get song metadata for {Id}", id);
                return _responseBuilder.CreateResponse(format, responseKey, new { });
            }
        }

        if (string.IsNullOrEmpty(artistName) || string.IsNullOrEmpty(trackTitle))
        {
            _logger.LogWarning("Could not get artist/title for song {Id}", id);
            return _responseBuilder.CreateResponse(format, "similarSongs", new { });
        }

        // Strip collab/feature decoration so Last.fm finds the canonical artist.
        var lookupArtist = NormalizeSeedArtist(artistName) ?? artistName;
        var lookupTitle  = NormalizeSeedTitle(trackTitle) ?? trackTitle;
        _logger.LogInformation("Getting similar songs for {Artist} - {Title} (lookup: {LookA} - {LookT})",
            artistName, trackTitle, lookupArtist, lookupTitle);

        var similarTracks = await _lastFmService.GetSimilarTracksAsync(lookupArtist, lookupTitle, count);

        if (similarTracks.Count == 0)
        {
            _logger.LogInformation("No similar tracks found from Last.fm");
            return _responseBuilder.CreateResponse(format, "similarSongs", new { });
        }

        _logger.LogInformation("Found {Count} similar tracks from Last.fm; building radio queue",
            similarTracks.Count);

        // For each Last.fm recommendation, prefer the local copy if we own it.
        // Tracks the user already has play at full FLAC quality from Navidrome
        // and avoid the yt-dlp roundtrip entirely. Lookups go in parallel
        // against Navidrome — at 50ms each that's ~150ms total under a
        // semaphore=10 cap, which fits comfortably inside Arpeggi's HTTP
        // budget.
        var sem = new SemaphoreSlim(10);
        var resolveTasks = similarTracks.Take(count).Select(async track =>
        {
            await sem.WaitAsync();
            try
            {
                var local = await TryFindLocalMatchAsync(track.Artist, track.Title, parameters);
                if (local != null) return local;
                // Forward Last.fm-provided duration so the client's scrub bar
                // shows the real song length on first play. Without this every
                // external song defaulted to 180s and ran past total length on
                // anything longer than 3 minutes.
                var hits = await _metadataService.SearchSongsByArtistTitleAsync(
                    track.Artist, track.Title, 1, track.Duration);
                return hits.Count > 0 ? hits[0] : null;
            }
            catch (Exception ex)
            {
                _logger.LogDebug(ex, "radio resolve failed for {Artist} - {Title}", track.Artist, track.Title);
                return null;
            }
            finally { sem.Release(); }
        }).ToList();
        var resolvedSongs = (await Task.WhenAll(resolveTasks))
            .Where(s => s != null).Cast<Song>().ToList();

        var localCount = resolvedSongs.Count(s => s.IsLocal);
        var externalCount = resolvedSongs.Count - localCount;
        _logger.LogInformation("Radio for '{SeedArtist} - {SeedTitle}' -> {N} songs ({L} local, {E} external)",
            artistName, trackTitle, resolvedSongs.Count, localCount, externalCount);

        // Track this radio queue so scrobble events can drive the sliding-window
        // prewarm of upcoming externals.
        _radioQueueStore.Register(resolvedSongs.Select(s => s.Id));

        // Fire-and-forget prewarm for the top of the queue so the first few
        // taps don't pay the full cold yt-dlp resolve. Cap below the shim's
        // MAX_CONCURRENT_YTDLP=8 (the prewarm method handles its own
        // concurrency limit internally). Local songs are skipped automatically
        // by the prewarmer (they have no registry entry).
        _ = _metadataService.PrewarmYouTubeIdsAsync(resolvedSongs, topN: 8);

        return BuildSimilarSongsResponse(format, resolvedSongs, responseKey);
    }

    /// <summary>
    /// Ask Navidrome whether we already have a song matching this artist+title.
    /// Returns a Song built from the local match (with <c>IsLocal=true</c> so
    /// the merger and stream path treat it as a real library track), or null
    /// when no good match exists.
    ///
    /// "Good match" = top hit's artist contains the expected artist (case
    /// insensitive) AND top hit's title contains the expected title. Navidrome's
    /// search is fuzzy — without that filter we'd match almost anything.
    /// </summary>
    private async Task<Song?> TryFindLocalMatchAsync(string artist, string title, Dictionary<string, string> baseParams)
    {
        try
        {
            var query = $"{artist} {title}";
            var navParams = new Dictionary<string, string>(baseParams)
            {
                ["query"] = query,
                ["songCount"] = "3",
                ["albumCount"] = "0",
                ["artistCount"] = "0",
                ["f"] = "json",
            };
            var result = await _proxyService.RelaySafeAsync("rest/search3", navParams);
            if (!result.Success || result.Body == null || result.Body.Length == 0) return null;
            using var doc = JsonDocument.Parse(result.Body);
            if (!doc.RootElement.TryGetProperty("subsonic-response", out var resp)
                || !resp.TryGetProperty("searchResult3", out var sr)
                || !sr.TryGetProperty("song", out var songs)
                || songs.ValueKind != JsonValueKind.Array
                || songs.GetArrayLength() == 0) return null;

            foreach (var s in songs.EnumerateArray())
            {
                var hitArtist = s.TryGetProperty("artist", out var a) ? a.GetString() ?? "" : "";
                var hitTitle = s.TryGetProperty("title", out var t) ? t.GetString() ?? "" : "";
                var hitId = s.TryGetProperty("id", out var idEl) ? idEl.GetString() ?? "" : "";
                if (string.IsNullOrEmpty(hitId)) continue;
                if (!ArtistOrTitleContains(hitArtist, artist)) continue;
                if (!ArtistOrTitleContains(hitTitle, title)) continue;
                return new Song
                {
                    Id = hitId,
                    Title = hitTitle,
                    Artist = hitArtist,
                    ArtistId = s.TryGetProperty("artistId", out var aid) ? aid.GetString() : null,
                    Album = s.TryGetProperty("album", out var al) ? al.GetString() ?? "" : "",
                    AlbumId = s.TryGetProperty("albumId", out var alid) ? alid.GetString() : null,
                    Duration = s.TryGetProperty("duration", out var d) && d.ValueKind == JsonValueKind.Number ? d.GetInt32() : null,
                    Year = s.TryGetProperty("year", out var yr) && yr.ValueKind == JsonValueKind.Number ? yr.GetInt32() : null,
                    Track = s.TryGetProperty("track", out var tr) && tr.ValueKind == JsonValueKind.Number ? tr.GetInt32() : null,
                    Genre = s.TryGetProperty("genre", out var g) ? g.GetString() : null,
                    IsLocal = true,
                };
            }
        }
        catch (Exception ex)
        {
            _logger.LogDebug(ex, "local match lookup failed for {A} - {T}", artist, title);
        }
        return null;
    }

    private static bool ArtistOrTitleContains(string haystack, string needle)
    {
        if (string.IsNullOrEmpty(haystack) || string.IsNullOrEmpty(needle)) return false;
        return haystack.Contains(needle, StringComparison.OrdinalIgnoreCase)
            || needle.Contains(haystack, StringComparison.OrdinalIgnoreCase);
    }

    private IActionResult BuildSimilarSongsResponse(string format, List<Song> songs, string responseKey)
    {
        if (format == "json")
        {
            var jsonSongs = songs.Select(s => _responseBuilder.ConvertSongToJson(s)).ToList();
            return _responseBuilder.CreateJsonResponse(new Dictionary<string, object>
            {
                ["status"] = "ok",
                ["version"] = "1.16.1",
                [responseKey] = new Dictionary<string, object> { ["song"] = jsonSongs }
            });
        }
        else
        {
            var ns = XNamespace.Get("http://subsonic.org/restapi");
            var similarSongsElement = new XElement(ns + responseKey);

            foreach (var song in songs)
            {
                similarSongsElement.Add(_responseBuilder.ConvertSongToXml(song, ns));
            }

            var doc = new XDocument(
                new XElement(ns + "subsonic-response",
                    new XAttribute("status", "ok"),
                    new XAttribute("version", "1.16.1"),
                    similarSongsElement
                )
            );

            return Content(doc.ToString(), "application/xml");
        }
    }

    /// <summary>
    /// Scrobble hijack: every Subsonic client posts here when a track starts
    /// playing (and again at end-of-play). We use the start-of-play signal to
    /// drive the sliding-window prewarm — if the scrobbled song is in a queue
    /// we registered, fire-and-forget yt-dlp resolution for the next 8
    /// unresolved external songs so a fast-skip user always has 8 ready ahead.
    ///
    /// We always relay to Navidrome too, because real scrobbling (last-played
    /// stats, the Now Playing panel) is the upstream's job.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/scrobble")]
    [Route("rest/scrobble.view")]
    public async Task<IActionResult> Scrobble()
    {
        var parameters = await ExtractAllParameters();
        var id = parameters.GetValueOrDefault("id", "");
        var format = parameters.GetValueOrDefault("f", "xml");

        if (!string.IsNullOrEmpty(id))
        {
            var upcoming = _radioQueueStore.GetUpcomingFrom(id, count: 16);
            if (upcoming.Count > 0)
            {
                _logger.LogDebug("scrobble {Id}: prewarming next {N} from queue", id, upcoming.Count);
                _ = _metadataService.PrewarmYouTubeIdsForSongIdsAsync(upcoming, topN: 8);
            }
        }

        // Always pass through so Navidrome's last-played/Now Playing stays accurate.
        try
        {
            var result = await _proxyService.RelayAsync("rest/scrobble", parameters);
            return File(result.Body, result.ContentType ?? $"application/{format}");
        }
        catch (HttpRequestException)
        {
            // Even if upstream is briefly unhappy, return 200 so the client
            // doesn't think scrobble is broken — the prewarm side already fired.
            return _responseBuilder.CreateResponse(format, "scrobble", new { });
        }
    }

    // OpenSubsonic transcoding extension. Feishin posts here before /rest/stream
    // to ask the server "should I transcode this or play it directly?" Navidrome
    // implements this for local songs. For external (Octo placeholder) songs the
    // upstream relay returns nothing useful and Feishin gets stuck — won't even
    // issue the /rest/stream call. So we hijack: external IDs always direct-play,
    // local IDs pass through to Navidrome's real implementation.
    [HttpGet, HttpPost]
    [Route("rest/getTranscodeDecision")]
    [Route("rest/getTranscodeDecision.view")]
    public async Task<IActionResult> GetTranscodeDecision()
    {
        var parameters = await ExtractAllParameters();
        var mediaId = parameters.GetValueOrDefault("mediaId", "");
        var (isExternal, _, _) = _localLibraryService.ParseSongId(mediaId);

        if (isExternal)
        {
            _logger.LogDebug("getTranscodeDecision: direct-play for external id {Id}", mediaId);
            return DirectPlayResponse();
        }

        try
        {
            var result = await _proxyService.RelayAsync("rest/getTranscodeDecision.view", parameters);
            return File(result.Body, result.ContentType ?? "application/json");
        }
        catch (HttpRequestException ex)
        {
            // Navidrome may be stock-Subsonic without the OpenSubsonic transcoding
            // extension. Returning a non-200 also makes Feishin fall back to the
            // direct stream URL, but a positive direct-play decision is cleaner.
            _logger.LogDebug("getTranscodeDecision local relay failed ({Msg}); returning direct-play", ex.Message);
            return DirectPlayResponse();
        }
    }

    // canDirectPlay:true is the only field Feishin's controller checks on the
    // happy path — see Feishin's subsonic-controller.ts: requiresTranscoding =
    // !td?.canDirectPlay. Returning the minimal envelope lets it advance to
    // /rest/stream which is where our own controller takes over for externals.
    private IActionResult DirectPlayResponse() => new JsonResult(new Dictionary<string, object>
    {
        ["subsonic-response"] = new Dictionary<string, object>
        {
            ["status"] = "ok",
            ["version"] = "1.16.1",
            ["transcodeDecision"] = new Dictionary<string, object>
            {
                ["canDirectPlay"] = true,
                ["canTranscode"] = false
            }
        }
    });

    // Generic endpoint that proxies any unmatched Subsonic API call to
    // Navidrome unchanged. We exclude paths that are owned by Octo's own
    // admin UI / static assets so that even if the static-files middleware
    // doesn't claim them first (turns out routing can win the race in some
    // .NET 9 + Static Web Assets configurations), we don't accidentally turn
    // /admin/admin.css into a Navidrome HTML response.
    [HttpGet, HttpPost]
    [Route("{**endpoint}")]
    public async Task<IActionResult> GenericEndpoint(string endpoint)
    {
        if (IsOctoOwnedPath(endpoint))
        {
            return NotFound();
        }

        var parameters = await ExtractAllParameters();
        var format = parameters.GetValueOrDefault("f", "xml");

        try
        {
            var result = await _proxyService.RelayAsync(endpoint, parameters);
            var contentType = result.ContentType ?? $"application/{format}";
            return File(result.Body, contentType);
        }
        catch (HttpRequestException ex)
        {
            // Return Subsonic-compatible error response
            return _responseBuilder.CreateError(format, 0, $"Error connecting to Subsonic server: {ex.Message}");
        }
    }

    private static bool IsOctoOwnedPath(string endpoint)
    {
        if (string.IsNullOrEmpty(endpoint)) return false;
        var lower = endpoint.ToLowerInvariant();
        return lower.StartsWith("admin", StringComparison.Ordinal)
            || lower.StartsWith("api/", StringComparison.Ordinal)
            || lower.StartsWith("assets/", StringComparison.Ordinal)
            || lower == "favicon.ico";
    }
}