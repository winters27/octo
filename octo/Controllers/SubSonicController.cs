using Microsoft.AspNetCore.Mvc;
using System.Xml.Linq;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Models.Download;
using Octo.Models.Search;
using Octo.Models.Radio;
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
    // IOptionsMonitor, not IOptions: the admin UI writes settings.json and the
    // config provider reloads it, but IOptions.Value is resolved once and this is a
    // singleton, so a captured copy would serve startup values until a restart. The
    // admin UI read through IOptionsMonitor and therefore SHOWED the new value while
    // nothing acted on it.
    private readonly IOptionsMonitor<SubsonicSettings> subsonicSettingsOptions;
    private SubsonicSettings _subsonicSettings => subsonicSettingsOptions.CurrentValue;
    private readonly IMusicMetadataService _metadataService;
    private readonly ILocalLibraryService _localLibraryService;
    private readonly IDownloadService _downloadService;
    private readonly SubsonicRequestParser _requestParser;
    private readonly SubsonicResponseBuilder _responseBuilder;
    private readonly SubsonicModelMapper _modelMapper;
    private readonly SubsonicProxyService _proxyService;
    private readonly PlaylistSyncService? _playlistSyncService;
    private readonly LastFmService? _lastFmService;
    private readonly LastFmRadioTrackResolver _radioTrackResolver;
    private readonly IOptionsMonitor<LastFmSettings> _lastFmSettingsOptions;
    private LastFmSettings _lastFmSettings => _lastFmSettingsOptions.CurrentValue;
    private readonly CoverArtService? _coverArtService;
    private readonly CoverArtAggregator? _coverArtAggregator;
    private readonly ExternalIdRegistry _idRegistry;
    private readonly Octo.Services.Common.TrackAcquisitionQueue _acquisitions;
    private readonly HeartAcquisitionCoordinator _heartAcquisitions;
    private readonly Octo.Services.Common.ExternalSearchService _externalSearch;
    private readonly RadioQueueStore _radioQueueStore;
    private readonly NavidromeIdentityService _navIdentity;
    private readonly ILogger<SubsonicController> _logger;
    private readonly LastFmRadioStateStore? _radioStateStore;
    private readonly LastFmRadioRefreshQueue? _radioRefreshQueue;
    private readonly LastFmRadioStreamSessionStore _radioStreamSessions;
    private readonly LastFmRadioStreamService _radioStreams;

    public SubsonicController(
        IOptionsMonitor<SubsonicSettings> subsonicSettings,
        IMusicMetadataService metadataService,
        ILocalLibraryService localLibraryService,
        IDownloadService downloadService,
        SubsonicRequestParser requestParser,
        SubsonicResponseBuilder responseBuilder,
        SubsonicModelMapper modelMapper,
        SubsonicProxyService proxyService,
        ExternalIdRegistry idRegistry,
        Octo.Services.Common.TrackAcquisitionQueue acquisitions,
        HeartAcquisitionCoordinator heartAcquisitions,
        Octo.Services.Common.ExternalSearchService externalSearch,
        RadioQueueStore radioQueueStore,
        NavidromeIdentityService navIdentity,
        LastFmRadioTrackResolver radioTrackResolver,
        ILogger<SubsonicController> logger,
        IOptionsMonitor<LastFmSettings> lastFmSettings,
        LastFmRadioStreamSessionStore radioStreamSessions,
        LastFmRadioStreamService radioStreams,
        PlaylistSyncService? playlistSyncService = null,
        LastFmService? lastFmService = null,
        CoverArtService? coverArtService = null,
        CoverArtAggregator? coverArtAggregator = null,
        LastFmRadioStateStore? radioStateStore = null,
        LastFmRadioRefreshQueue? radioRefreshQueue = null)
    {
        subsonicSettingsOptions = subsonicSettings;
        _metadataService = metadataService;
        _localLibraryService = localLibraryService;
        _downloadService = downloadService;
        _requestParser = requestParser;
        _responseBuilder = responseBuilder;
        _modelMapper = modelMapper;
        _proxyService = proxyService;
        _idRegistry = idRegistry;
        _acquisitions = acquisitions;
        _heartAcquisitions = heartAcquisitions;
        _externalSearch = externalSearch;
        _radioQueueStore = radioQueueStore;
        _navIdentity = navIdentity;
        _radioTrackResolver = radioTrackResolver;
        _playlistSyncService = playlistSyncService;
        _lastFmService = lastFmService;
        _lastFmSettingsOptions = lastFmSettings;
        _coverArtService = coverArtService;
        _coverArtAggregator = coverArtAggregator;
        _logger = logger;
        _radioStateStore = radioStateStore;
        _radioRefreshQueue = radioRefreshQueue;
        _radioStreamSessions = radioStreamSessions;
        _radioStreams = radioStreams;
        // No hard throw on a missing/blank Subsonic URL: that made every request
        // fail opaquely. Misconfiguration is now reported per-request with an
        // actionable message (see Ping and OctoNotConfiguredException), and the
        // admin panel stays reachable so the user can fix it.
    }

    // -------------------------------------------------------------------------
    // ping — the first call every Subsonic client makes. We make it the moment a
    // broken setup explains itself, instead of relaying blindly and returning an
    // opaque error when the Navidrome URL is missing or unreachable.
    // -------------------------------------------------------------------------
    [HttpGet]
    [HttpPost]
    [Route("rest/ping")]
    [Route("rest/ping.view")]
    public async Task<IActionResult> Ping()
    {
        var parameters = await ExtractAllParameters();
        var format = parameters.GetValueOrDefault("f", "xml");

        if (string.IsNullOrWhiteSpace(_subsonicSettings.Url)
            || !Uri.TryCreate(_subsonicSettings.Url, UriKind.Absolute, out _))
        {
            return _responseBuilder.CreateError(format, 0,
                $"Octo isn't configured yet. Open {Request.Scheme}://{Request.Host}/admin and set " +
                "your Navidrome URL (SUBSONIC_URL), then point this client at Octo instead of Navidrome.");
        }

        // Relay to Navidrome so real credentials are validated there. A connection
        // failure means Octo can't reach the configured URL; pass a successful
        // (or auth-failed) Navidrome envelope straight through otherwise.
        var relay = await _proxyService.RelaySafeAsync("rest/ping.view", parameters);
        if (!relay.Success || relay.Body is null)
        {
            return _responseBuilder.CreateError(format, 0,
                $"Octo can't reach Navidrome at {_subsonicSettings.Url}. Check the URL is correct and " +
                "reachable from the Octo container (use a LAN IP or service name, not localhost).");
        }

        return File(relay.Body, relay.ContentType ?? $"application/{format}");
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
                seedArtist = LastFmRadioSeedNormalizer.Artist(seedArtist);
                seedTitle  = LastFmRadioSeedNormalizer.Title(seedTitle);
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

    [HttpGet, HttpPost]
    [Route("rest/getPlaylists")]
    [Route("rest/getPlaylists.view")]
    public async Task<IActionResult> GetPlaylists()
    {
        var parameters = await ExtractAllParameters();
        var format = parameters.GetValueOrDefault("f", "xml");
        var relay = await _proxyService.RelaySafeAsync("rest/getPlaylists", parameters);
        if (!relay.Success || relay.Body is not { Length: > 0 }
            || !IsSuccessfulSubsonicResponse(relay.Body, format))
            return relay.Body is { Length: > 0 }
                ? File(relay.Body, relay.ContentType ?? $"application/{format}")
                : _responseBuilder.CreateError(format, 0, "Unable to authenticate with Navidrome");

        var username = parameters.GetValueOrDefault("u", "");
        await BootstrapRadioProfileAsync(username, parameters);
        var stations = PlaylistStations(username);
        QueueRefreshIfStale(username);
        if (stations.Count == 0) return File(relay.Body, relay.ContentType ?? $"application/{format}");
        try
        {
            if (format.Equals("json", StringComparison.OrdinalIgnoreCase))
            {
                var root = JsonNode.Parse(relay.Body)!.AsObject();
                var response = root["subsonic-response"]!.AsObject();
                var playlists = response["playlists"] as JsonObject ?? new JsonObject();
                response["playlists"] = playlists;
                var rows = playlists["playlist"] as JsonArray ?? new JsonArray();
                playlists["playlist"] = rows;
                foreach (var station in stations)
                    rows.Add(JsonSerializer.SerializeToNode(_responseBuilder.RadioPlaylistFields(station)));
                return File(Encoding.UTF8.GetBytes(root.ToJsonString()), "application/json");
            }
            var document = XDocument.Parse(Encoding.UTF8.GetString(relay.Body));
            var responseElement = document.Root!;
            var ns = responseElement.Name.Namespace;
            var playlistsElement = responseElement.Elements().FirstOrDefault(element => element.Name.LocalName == "playlists");
            if (playlistsElement is null) { playlistsElement = new XElement(ns + "playlists"); responseElement.Add(playlistsElement); }
            foreach (var station in stations)
                playlistsElement.Add(new XElement(ns + "playlist",
                    _responseBuilder.RadioPlaylistFields(station).Select(pair =>
                        new XAttribute(pair.Key, XmlValue(pair.Value)))));
            return File(Encoding.UTF8.GetBytes(document.ToString()), "application/xml");
        }
        catch (Exception ex)
        {
            _logger.LogWarning(ex, "Could not merge Radio stations into getPlaylists");
            return File(relay.Body, relay.ContentType ?? $"application/{format}");
        }
    }

    [HttpGet, HttpPost]
    [Route("rest/getPlaylist")]
    [Route("rest/getPlaylist.view")]
    public async Task<IActionResult> GetPlaylist()
    {
        var parameters = await ExtractAllParameters();
        var format = parameters.GetValueOrDefault("f", "xml");
        var id = parameters.GetValueOrDefault("id", "");
        var username = parameters.GetValueOrDefault("u", "");
        var station = PlaylistStations(username).FirstOrDefault(item => item.Id == id);
        if (station is null)
        {
            var relay = await _proxyService.RelaySafeAsync("rest/getPlaylist", parameters);
            return relay.Success && relay.Body is not null
                ? File(relay.Body, relay.ContentType ?? $"application/{format}")
                : _responseBuilder.CreateError(format, 0, "Playlist not found");
        }
        var auth = parameters.ToDictionary(pair => pair.Key, pair => pair.Value);
        auth.Remove("id");
        var ping = await _proxyService.RelaySafeAsync("rest/ping", auth);
        if (!ping.Success || ping.Body is null || !IsSuccessfulSubsonicResponse(ping.Body, format))
            return _responseBuilder.CreateError(format, 40, "Wrong username or password");

        var songs = await MaterializeStationAsync(station, parameters);
        _radioQueueStore.Register(songs.Select(song => song.Id));
        _ = _metadataService.PrewarmYouTubeIdsAsync(songs, topN: 8);
        QueueRefreshIfStale(username);
        return _responseBuilder.CreateRadioPlaylistResponse(format, station, songs);
    }

    [HttpGet, HttpPost]
    [Route("rest/getInternetRadioStations")]
    [Route("rest/getInternetRadioStations.view")]
    public async Task<IActionResult> GetInternetRadioStations()
    {
        var parameters = await ExtractAllParameters();
        var format = parameters.GetValueOrDefault("f", "xml");
        var relay = await _proxyService.RelaySafeAsync("rest/getInternetRadioStations", parameters);
        if (!relay.Success || relay.Body is not { Length: > 0 }
            || !IsSuccessfulSubsonicResponse(relay.Body, format))
            return relay.Body is { Length: > 0 }
                ? File(relay.Body, relay.ContentType ?? $"application/{format}")
                : _responseBuilder.CreateError(format, 0, "Unable to authenticate with Navidrome");

        var username = parameters.GetValueOrDefault("u", "");
        await BootstrapRadioProfileAsync(username, parameters);
        var stations = StreamStations(username);
        _logger.LogInformation(
            "Continuous Radio discovery requested for {User}: {StationCount} eligible stations",
            username, stations.Count);
        QueueRefreshIfStale(username);
        if (stations.Count == 0) return File(relay.Body, relay.ContentType ?? $"application/{format}");

        var coldSessions = new List<LastFmRadioStreamSession>();

        (LastFmRadioStation Station, string Token)? PublishCachedStation(
            LastFmRadioStation station)
        {
            var token = _radioStreamSessions.Issue(username, station.Id, parameters);
            var session = _radioStreamSessions.Get(token)!;
            var readyPool = _radioStreams.GetReadyPool(session);
            if (readyPool.Count == 0)
            {
                // Station-list requests are latency-sensitive and some clients cancel
                // them after only a few seconds. Warm every cache miss independently,
                // but never make an already-ready station wait for slower siblings.
                coldSessions.Add(session);
                _radioStreamSessions.Remove(token);
                return null;
            }
            if (!_radioStreamSessions.AttachReadyPool(token, readyPool))
            {
                _radioStreamSessions.Remove(token);
                return null;
            }
            if (readyPool.Count < LastFmRadioStreamService.ReadyPoolSize)
                _radioStreams.WarmReadyPool(_radioStreamSessions.Get(token)!);
            return (station, token);
        }

        async Task<(LastFmRadioStation Station, string Token)?> PrepareStarter(
            LastFmRadioStation station)
        {
            var token = _radioStreamSessions.Issue(username, station.Id, parameters);
            var session = _radioStreamSessions.Get(token)!;
            var readyPool = await _radioStreams.PrepareForPublicationAsync(
                session, HttpContext.RequestAborted);
            if (readyPool.Count == 0) { _radioStreamSessions.Remove(token); return null; }
            if (!_radioStreamSessions.AttachReadyPool(token, readyPool))
            {
                _radioStreamSessions.Remove(token);
                return null;
            }
            if (readyPool.Count < LastFmRadioStreamService.ReadyPoolSize)
                _radioStreams.WarmReadyPool(_radioStreamSessions.Get(token)!);
            return (station, token);
        }

        try
        {
            var prepared = stations.Select(PublishCachedStation)
                .Where(item => item is not null).Select(item => item!.Value).ToList();
            // A completely cold install still publishes one usable starter in the
            // same response. The remaining stations are already warming above and
            // will appear on the client's next ordinary refresh.
            if (prepared.Count == 0)
            {
                var starter = await PrepareStarter(stations[0]);
                if (starter is not null) prepared.Add(starter.Value);

                // PrepareStarter owns this station's cold-path production and starts
                // its runway warm only after the publication pool is attached. Do not
                // race it with the cache-miss warmer discovered above.
                coldSessions.RemoveAll(session => session.StationId == stations[0].Id);
            }
            foreach (var coldSession in coldSessions)
                _radioStreams.WarmReadyPool(coldSession);
            _logger.LogInformation(
                "Continuous Radio discovery published {ReadyCount}/{StationCount} ready stations for {User}",
                prepared.Count, stations.Count, username);
            string StreamUrl(string token) =>
                $"{Request.Scheme}://{Request.Host}{Request.PathBase}/radio/stream/{token}";
            if (format.Equals("json", StringComparison.OrdinalIgnoreCase))
            {
                var root = JsonNode.Parse(relay.Body)!.AsObject();
                var response = root["subsonic-response"]!.AsObject();
                var container = response["internetRadioStations"] as JsonObject ?? new JsonObject();
                response["internetRadioStations"] = container;
                var rows = container["internetRadioStation"] as JsonArray ?? new JsonArray();
                container["internetRadioStation"] = rows;
                foreach (var (station, token) in prepared)
                    rows.Add(new JsonObject
                    {
                        ["id"] = station.Id,
                        ["name"] = station.Name,
                        ["streamUrl"] = StreamUrl(token),
                        ["coverArt"] = station.Id,
                    });
                return File(Encoding.UTF8.GetBytes(root.ToJsonString()), "application/json");
            }

            var document = XDocument.Parse(Encoding.UTF8.GetString(relay.Body));
            var responseElement = document.Root!;
            var ns = responseElement.Name.Namespace;
            var containerElement = responseElement.Elements()
                .FirstOrDefault(element => element.Name.LocalName == "internetRadioStations");
            if (containerElement is null)
            {
                containerElement = new XElement(ns + "internetRadioStations");
                responseElement.Add(containerElement);
            }
            foreach (var (station, token) in prepared)
                containerElement.Add(new XElement(ns + "internetRadioStation",
                    new XAttribute("id", station.Id), new XAttribute("name", station.Name),
                    new XAttribute("streamUrl", StreamUrl(token)),
                    new XAttribute("coverArt", station.Id)));
            return File(Encoding.UTF8.GetBytes(document.ToString()), "application/xml");
        }
        catch (Exception ex)
        {
            _logger.LogWarning(ex, "Could not merge Octo stations into getInternetRadioStations");
            return File(relay.Body, relay.ContentType ?? $"application/{format}");
        }
    }

    [HttpGet, HttpHead]
    [Route("radio/stream/{token:length(48)}")]
    public async Task StreamGeneratedRadio(string token)
    {
        var session = _radioStreamSessions.Get(token);
        var station = session is null ? null : _radioStreams.Resolve(session);
        if (session is null || station is null)
        {
            Response.StatusCode = StatusCodes.Status404NotFound;
            return;
        }

        Response.StatusCode = StatusCodes.Status200OK;
        Response.ContentType = "audio/mpeg";
        Response.Headers.CacheControl = "no-store, no-transform";
        Response.Headers["Accept-Ranges"] = "none";
        Response.Headers["icy-name"] = station.Name;
        Response.Headers["icy-br"] = _lastFmSettings.EffectiveRadioStreamBitrateKbps.ToString();
        var includeIcyMetadata = _lastFmSettings.EnableIcyMetadata
            && Request.Headers["Icy-MetaData"].ToString().Trim() == "1";
        if (includeIcyMetadata)
            Response.Headers["icy-metaint"] = IcyMetadataStream.DefaultInterval.ToString();
        if (HttpMethods.IsHead(Request.Method)) return;
        _logger.LogInformation("Continuous Radio stream opened for {Station} by {User}",
            station.Name, session.Username);
        try
        {
            await Response.StartAsync(HttpContext.RequestAborted);
            await _radioStreams.StreamAsync(session, Response.Body, HttpContext.RequestAborted,
                includeIcyMetadata);
        }
        catch (OperationCanceledException) when (HttpContext.RequestAborted.IsCancellationRequested)
        {
            // A radio stream normally ends because the listener stopped playback.
        }
        catch (Exception ex)
        {
            _logger.LogWarning(ex, "Continuous Radio stream failed for station {Station}", station.Id);
            if (!Response.HasStarted) Response.StatusCode = StatusCodes.Status503ServiceUnavailable;
            else HttpContext.Abort();
        }
    }

    [HttpGet, HttpPost]
    [Route("rest/createInternetRadioStation")]
    [Route("rest/createInternetRadioStation.view")]
    [Route("rest/updateInternetRadioStation")]
    [Route("rest/updateInternetRadioStation.view")]
    [Route("rest/deleteInternetRadioStation")]
    [Route("rest/deleteInternetRadioStation.view")]
    public async Task<IActionResult> MutateInternetRadioStation()
    {
        var parameters = await ExtractAllParameters();
        var format = parameters.GetValueOrDefault("f", "xml");
        var id = parameters.GetValueOrDefault("id", "");
        if (id.StartsWith("or", StringComparison.Ordinal) && id.Length == 22)
            return _responseBuilder.CreateError(format, 70, "Octo Radio stations are read-only");
        var endpoint = Request.Path.Value?.Split('/').LastOrDefault()?.Replace(".view", "")
            ?? "updateInternetRadioStation";
        var relay = await _proxyService.RelaySafeAsync("rest/" + endpoint, parameters);
        return relay.Success && relay.Body is not null
            ? File(relay.Body, relay.ContentType ?? $"application/{format}")
            : _responseBuilder.CreateError(format, 0, "Unable to update internet radio station");
    }

    [HttpGet, HttpPost]
    [Route("rest/createPlaylist")]
    [Route("rest/createPlaylist.view")]
    [Route("rest/updatePlaylist")]
    [Route("rest/updatePlaylist.view")]
    [Route("rest/deletePlaylist")]
    [Route("rest/deletePlaylist.view")]
    public async Task<IActionResult> MutatePlaylist()
    {
        var parameters = await ExtractAllParameters();
        var format = parameters.GetValueOrDefault("f", "xml");
        var id = parameters.GetValueOrDefault("playlistId", parameters.GetValueOrDefault("id", ""));
        if (id.StartsWith("or", StringComparison.Ordinal) && id.Length == 22)
            return _responseBuilder.CreateError(format, 70, "Octo Radio stations are read-only");
        var endpoint = Request.Path.Value?.Split('/').LastOrDefault()?.Replace(".view", "") ?? "updatePlaylist";
        var relay = await _proxyService.RelaySafeAsync("rest/" + endpoint, parameters);
        return relay.Success && relay.Body is not null
            ? File(relay.Body, relay.ContentType ?? $"application/{format}")
            : _responseBuilder.CreateError(format, 0, "Unable to update playlist");
    }

    private List<LastFmRadioStation> VisibleStations(string username)
    {
        if (_radioStateStore is null || !_lastFmSettings.EnableRadio || username.Length == 0) return [];
        return _radioStateStore.GetUser(username).Stations.Where(station =>
            station.Personalized ? _lastFmSettings.EnablePersonalizedStations
                : _lastFmSettings.EnableDiscoveryStations).ToList();
    }

    private List<LastFmRadioStation> PlaylistStations(string username) =>
        _lastFmSettings.ExposeRadioAsPlaylists ? VisibleStations(username) : [];

    private List<LastFmRadioStation> StreamStations(string username) =>
        _lastFmSettings.ExposeRadioAsStreams ? VisibleStations(username) : [];

    private void QueueRefreshIfStale(string username)
    {
        if (_radioStateStore is null || _radioRefreshQueue is null || username.Length == 0) return;
        var user = _radioStateStore.GetUser(username);
        if (user.Stations.Count == 0 || LastFmRadioRefreshPolicy.IsStale(user, _lastFmSettings))
            _radioRefreshQueue.Enqueue(username);
    }

    private async Task BootstrapRadioProfileAsync(string username,
        IReadOnlyDictionary<string, string> authenticatedParameters)
    {
        if (_radioStateStore is null || username.Length == 0 || !_lastFmSettings.EnableRadio
            || !_lastFmSettings.EnablePersonalizedStations
            || _radioStateStore.GetUser(username).Plays.Count > 0) return;

        async Task<List<(Song song, bool learned)>> Fetch(string endpoint, string container,
            bool learned)
        {
            var parameters = authenticatedParameters.ToDictionary(pair => pair.Key, pair => pair.Value);
            parameters["f"] = "json";
            if (endpoint == "rest/getRandomSongs") parameters["size"] = "12";
            var result = await _proxyService.RelaySafeAsync(endpoint, parameters);
            if (!result.Success || result.Body is not { Length: > 0 }) return [];
            try
            {
                using var doc = JsonDocument.Parse(result.Body);
                var response = doc.RootElement.GetProperty("subsonic-response");
                if (!response.TryGetProperty(container, out var parent)
                    || !parent.TryGetProperty("song", out var values)
                    || values.ValueKind != JsonValueKind.Array) return [];
                return values.EnumerateArray().Take(20).Select(item => (new Song
                {
                    Id = item.TryGetProperty("id", out var id) ? id.GetString() ?? "" : "",
                    Artist = item.TryGetProperty("artist", out var artist) ? artist.GetString() ?? "" : "",
                    Title = item.TryGetProperty("title", out var title) ? title.GetString() ?? "" : "",
                    Album = item.TryGetProperty("album", out var album) ? album.GetString() ?? "" : "",
                    Genre = item.TryGetProperty("genre", out var genre) ? genre.GetString() : null,
                    Duration = item.TryGetProperty("duration", out var duration) && duration.TryGetInt32(out var seconds) ? seconds : null,
                    IsLocal = true
                }, learned)).ToList();
            }
            catch { return []; }
        }

        var seeds = await Fetch("rest/getStarred2", "starred2", true);
        if (seeds.Count < _lastFmSettings.EffectiveMinimumPlays)
        {
            async Task<List<(Song song, bool learned)>> FetchAlbumSignals(string type)
            {
                var query = authenticatedParameters.ToDictionary(pair => pair.Key, pair => pair.Value);
                query["f"] = "json"; query["type"] = type; query["size"] = "3";
                var result = await _proxyService.RelaySafeAsync("rest/getAlbumList2", query);
                if (!result.Success || result.Body is not { Length: > 0 }) return [];
                try
                {
                    using var document = JsonDocument.Parse(result.Body);
                    var albums = document.RootElement.GetProperty("subsonic-response")
                        .GetProperty("albumList2").GetProperty("album");
                    var output = new List<(Song song, bool learned)>();
                    foreach (var album in albums.EnumerateArray().Take(3))
                    {
                        var albumQuery = authenticatedParameters.ToDictionary(pair => pair.Key, pair => pair.Value);
                        albumQuery["f"] = "json"; albumQuery["id"] = album.GetProperty("id").GetString() ?? "";
                        var detail = await _proxyService.RelaySafeAsync("rest/getAlbum", albumQuery);
                        if (!detail.Success || detail.Body is not { Length: > 0 }) continue;
                        using var detailDoc = JsonDocument.Parse(detail.Body);
                        var songs = detailDoc.RootElement.GetProperty("subsonic-response")
                            .GetProperty("album").GetProperty("song");
                        foreach (var item in songs.EnumerateArray().Take(4))
                            output.Add((new Song
                            {
                                Id = item.GetProperty("id").GetString() ?? "",
                                Artist = item.TryGetProperty("artist", out var artist) ? artist.GetString() ?? "" : "",
                                Title = item.TryGetProperty("title", out var title) ? title.GetString() ?? "" : "",
                                Album = item.TryGetProperty("album", out var albumName) ? albumName.GetString() ?? "" : "",
                                Genre = item.TryGetProperty("genre", out var genre) ? genre.GetString() : null,
                                Duration = item.TryGetProperty("duration", out var duration) && duration.TryGetInt32(out var seconds) ? seconds : null,
                                IsLocal = true
                            }, true));
                    }
                    return output;
                }
                catch { return []; }
            }
            seeds.AddRange(await FetchAlbumSignals("frequent"));
            seeds.AddRange(await FetchAlbumSignals("recent"));
        }
        if (seeds.Count == 0) seeds = await Fetch("rest/getRandomSongs", "randomSongs", false);
        var offset = 0;
        foreach (var (song, learned) in seeds)
            _radioStateStore.RecordPlay(username, new LastFmRadioPlay
            {
                SongId = song.Id, Artist = song.Artist, Title = song.Title, Album = song.Album,
                Genre = song.Genre, Duration = song.Duration, IsLocal = true, Hearted = learned,
                LearnedSignal = learned, Source = learned ? "bootstrap-star" : "bootstrap-random",
                PlayedAtUtc = DateTime.UtcNow.AddMinutes(-(offset++ * 6))
            });
    }

    private async Task<List<Song>> MaterializeStationAsync(LastFmRadioStation station,
        IReadOnlyDictionary<string, string> parameters)
    {
        using var gate = new SemaphoreSlim(4, 4);
        var tasks = station.Tracks.Select(async track =>
        {
            await gate.WaitAsync(HttpContext.RequestAborted);
            try
            {
                return await _radioTrackResolver.ResolveAsync(track.Artist, track.Title, track.Duration,
                    parameters, HttpContext.RequestAborted) ?? new Song
                {
                    Id = track.ResolvedId ?? "", Artist = track.Artist, Title = track.Title,
                    Album = track.Album ?? track.Title, Genre = track.Genre, Duration = track.Duration,
                    Year = track.Year, IsLocal = false, ExternalProvider = track.ExternalProvider,
                    ExternalId = track.ResolvedId
                };
            }
            finally { gate.Release(); }
        });
        var resolved = (await Task.WhenAll(tasks)).Where(song => song.Id.Length > 0)
            .Where(song => _subsonicSettings.ExplicitFilter switch
            {
                ExplicitFilter.CleanOnly => song.ExplicitContentLyrics is not 1,
                ExplicitFilter.ExplicitOnly => song.ExplicitContentLyrics is not 3,
                _ => true
            }).ToList();
        var spaced = new List<Song>(resolved.Count);
        string? previousArtist = null, previousAlbumKey = null;
        foreach (var song in resolved)
        {
            var albumKey = string.IsNullOrWhiteSpace(song.Album) ? null : song.Artist + "|" + song.Album;
            if (string.Equals(previousArtist, song.Artist, StringComparison.OrdinalIgnoreCase)
                || (albumKey is not null
                    && string.Equals(previousAlbumKey, albumKey, StringComparison.OrdinalIgnoreCase))) continue;
            spaced.Add(song); previousArtist = song.Artist; previousAlbumKey = albumKey;
        }
        return spaced;
    }

    private static string XmlValue(object value) => value switch
    {
        bool boolean => boolean ? "true" : "false",
        IFormattable formatted => formatted.ToString(null, System.Globalization.CultureInfo.InvariantCulture),
        _ => value.ToString() ?? ""
    };

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

        // search2 and search3 are the same hijack with different envelopes. Decide once:
        // the relay target, the envelope we answer with, and the empty-query passthrough
        // all have to agree, and they used to be decided in three separate places with the
        // response side hardcoded to searchResult3.
        var isSearch2 = (Request.Path.Value ?? "").Contains("search2", StringComparison.OrdinalIgnoreCase);
        var searchEndpoint = isSearch2 ? "rest/search2" : "rest/search3";
        var envelope = isSearch2 ? "searchResult2" : "searchResult3";

        // Discovery belongs on the first page only. Injected rows are regenerated per
        // request rather than held in a server-side result set, so appending them to page
        // two hands the client the same suggestions it already scrolled past. The native
        // search path refuses later pages for exactly this reason; do the same here and
        // let the library page normally underneath.
        var songOffset = int.TryParse(parameters.GetValueOrDefault("songOffset", "0"), out var so) ? so : 0;

        if (string.IsNullOrWhiteSpace(cleanQuery) || songOffset > 0)
        {
            try
            {
                var result = await _proxyService.RelayAsync(searchEndpoint, parameters);
                var contentType = result.ContentType ?? $"application/{format}";
                return File(result.Body, contentType);
            }
            catch
            {
                return _responseBuilder.CreateResponse(format, envelope, new { });
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
        //
        // The split itself lives in SearchBudget so it can be unit-tested; the
        // local floor used to be a flat 20, which is also the spec default for
        // songCount, so the most common search in the wild left nothing for
        // discovery at all (#14).
        var (localSongTarget, externalTarget) = SearchBudget.Compute(requestedSongs);

        // A client that asked for a handful of songs is searching as the user types. The
        // song side already costs nothing there (the budget leaves no room for discovery),
        // but external album search was still firing a Deezer query per keystroke. Judged
        // from the song count only when the client actually asked for songs, so a genuine
        // album-only search still gets album discovery.
        var isTypeAheadProbe = requestedSongs > 0 && externalTarget == 0;

        // Album discovery runs concurrently with the song fan-out below so it costs no
        // serial latency. It needs no Last.fm key (Deezer's catalog is keyless), so albums
        // still appear for a user who has not set one up.
        var albumTask = requestedAlbums > 0 && !isTypeAheadProbe
            ? _metadataService.SearchAlbumsAsync(cleanQuery, Math.Min(requestedAlbums, 20))
            : Task.FromResult(new List<Album>());

        // One build per query, shared by every caller. Clients routinely fire several
        // search calls for a single typed query, and those calls resolve to the same
        // routing objects, so without this each one would re-run the whole enrichment
        // pipeline over them concurrently. Started here rather than awaited, so it
        // overlaps the local relay below; how many of its rows we actually use depends
        // on what that relay comes back with.
        var externalTask = externalTarget > 0
            ? _externalSearch.GetAsync(cleanQuery)
            : Task.FromResult<IReadOnlyList<Song>>(Array.Empty<Song>());

        // Local pass-through. Albums/artists always get the full requested counts;
        // song-side gets the local target.
        var localParams = new Dictionary<string, string>(parameters)
        {
            ["songCount"]   = localSongTarget.ToString(),
            ["albumCount"]  = requestedAlbums.ToString(),
            ["artistCount"] = requestedArtists.ToString(),
        };
        var localResult = await _proxyService.RelaySafeAsync(searchEndpoint, localParams);

        // Subsonic reports its own errors inside an HTTP 200, so a rejected login and an
        // empty library are the same thing to every check above this line. Left alone,
        // the discovery top-up would read "no local matches", fill the page with
        // suggestions, and present a broken connection as a healthy search.
        if (IsFailedSubsonicBody(localResult.Body, localResult.ContentType))
        {
            _logger.LogDebug("upstream rejected the search for '{Q}'; passing its error through", cleanQuery);
            return File(localResult.Body!, localResult.ContentType ?? $"application/{format}");
        }

        // Parsed here rather than inside the merge so the count that sizes the discovery
        // slice below is taken from the very list the response will render. Deriving it
        // from a second, differently-written parse of the same bytes is how you end up
        // topping up against a local count the client never sees.
        var localParsed = localResult.Success && localResult.Body != null
            ? _modelMapper.ParseSearchResponse(localResult.Body, localResult.ContentType)
            : (Songs: new List<object>(), Albums: new List<object>(), Artists: new List<object>());

        // Hand the slots the library did not fill to discovery. A query the user owns
        // nothing for is the one most worth answering with suggestions, and the local
        // target is a reservation rather than a promise: Navidrome returns what it has.
        // If the relay failed outright the count is zero, and filling the page with
        // discovery is the right answer there too, since the merge will show no locals.
        var built = await externalTask;
        var externalSlice = Math.Min(
            built.Count,
            externalTarget + Math.Max(0, localSongTarget - localParsed.Songs.Count));
        var externalSongs = built.Take(externalSlice).ToList();

        var playlistTask = _subsonicSettings.EnableExternalPlaylists
            ? await _metadataService.SearchPlaylistsAsync(cleanQuery, requestedAlbums)
            : new List<ExternalPlaylist>();

        // Degrade to no albums rather than failing the whole search if Deezer is slow,
        // throttled or unreachable.
        List<Album> externalAlbums;
        try { externalAlbums = await albumTask; }
        catch (Exception ex)
        {
            _logger.LogDebug("external album search failed for '{Q}': {M}", cleanQuery, ex.Message);
            externalAlbums = new List<Album>();
        }

        var externalResult = new SearchResult
        {
            Songs = externalSongs,
            Albums = externalAlbums,
            Artists = new List<Artist>(),
        };

        // Track this response as a "queue" so a later scrobble for any of its
        // songs can drive the sliding-window prewarm of upcoming externals.
        // Order matches the merged response order — local first, external after.
        var localSongIds = ExtractLocalSongIds(localResult.Body, localResult.ContentType);
        _radioQueueStore.Register(localSongIds.Concat(externalSongs.Select(s => s.Id)));

        return MergeSearchResults(localParsed, localResult.ContentType, externalResult, playlistTask, format, envelope);
    }

    /// <summary>
    /// True when a relayed body is a Subsonic error envelope. These arrive as HTTP 200
    /// with <c>status="failed"</c> inside, so the status code alone cannot tell a rejected
    /// request from an empty result set.
    /// </summary>
    internal static bool IsFailedSubsonicBody(byte[]? body, string? contentType)
    {
        if (body == null || body.Length == 0) return false;
        try
        {
            if (contentType?.Contains("json") == true)
            {
                using var doc = JsonDocument.Parse(body);
                return doc.RootElement.TryGetProperty("subsonic-response", out var resp)
                    && resp.TryGetProperty("status", out var st)
                    && st.ValueKind == JsonValueKind.String
                    && string.Equals(st.GetString(), "failed", StringComparison.OrdinalIgnoreCase);
            }

            var xml = XDocument.Load(new System.IO.MemoryStream(body));
            return string.Equals(xml.Root?.Attribute("status")?.Value, "failed",
                StringComparison.OrdinalIgnoreCase);
        }
        catch
        {
            // Unparseable is not the same as failed. Let the normal path handle it.
            return false;
        }
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
    /// Downloads on-the-fly if needed, or streams directly in Stream mode.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/stream")]
    [Route("rest/stream.view")]
    public async Task<IActionResult> Stream()
    {
        var parameters = await ExtractAllParameters();
        var id = parameters.GetValueOrDefault("id", "");
        // Needed so a failure here can answer with a real Subsonic error envelope instead
        // of the bare JSON this method used to emit.
        var format = parameters.GetValueOrDefault("f", "xml");

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

        // A local file may only be served under an external id when this session DECLARES
        // that id as lossless. search3 already told the client a suffix, bitrate and size,
        // and a player picks its decoder from those, so handing back different bytes is
        // what makes tracks silently refuse to start. With the default settings the
        // lossless copy is reached as its own library track after the rescan instead.
        if (_subsonicSettings.WaitForLosslessOnPlay)
        {
            var localPath = await _localLibraryService.GetLocalPathForExternalSongAsync(provider!, externalId!);
            if (localPath != null && System.IO.File.Exists(localPath))
            {
                var stream = System.IO.File.OpenRead(localPath);
                return File(stream, GetContentType(localPath), enableRangeProcessing: true);
            }
        }

        try
        {
            // Lossless-on-play remains an explicit opt-in. Normal playback never starts
            // acquisition: owned ids already went to Navidrome above, and missing ids
            // stream from YouTube below. Hearts are the normal permanent-copy gesture.
            if (_subsonicSettings.WaitForLosslessOnPlay)
            {
                var acquisition = _acquisitions.Enqueue(provider!, externalId!, isStar: false,
                    triggerAlbumDownload: false, forcePermanent: true);
                return await ServeAcquiredAsync(acquisition, provider!, externalId!, id, format,
                    allowPreviewFallback: true);
            }

            var direct = await TryDirectStreamAsync(provider!, externalId!, id);
            if (direct is not null) return direct;

            _logger.LogWarning("Direct stream not available for {Id}", id);
            return _responseBuilder.CreateError(format, 70, "No playable source found for this track");
        }
        catch (OperationCanceledException)
        {
            // The client hung up. Normal, and answering a dead socket would only produce a
            // spurious error log.
            return new EmptyResult();
        }
        catch (Exception ex)
        {
            _logger.LogError(ex, "Failed to stream track {Id}", id);
            return StatusCode(500, new { error = $"Failed to stream: {ex.Message}" });
        }
    }

    /// <summary>
    /// Wait for a queued acquisition and serve the file.
    ///
    /// WaitAsync is what makes this safe: abandoning the WAIT leaves the transfer running,
    /// so a client that gives up costs it nothing.
    /// </summary>
    private async Task<IActionResult> ServeAcquiredAsync(
        Task<string> acquisition, string provider, string externalId, string id, string format,
        bool allowPreviewFallback)
    {
        // Above 0, the wait is bounded and the preview stands in while the fetch keeps
        // running in the background; the next play of this id serves the landed file.
        var timeout = Math.Max(0, _subsonicSettings.LosslessWaitTimeoutSeconds);
        var fallback = allowPreviewFallback && timeout > 0;

        string path;
        try
        {
            path = fallback
                ? await acquisition.WaitAsync(TimeSpan.FromSeconds(timeout), HttpContext.RequestAborted)
                : await acquisition.WaitAsync(HttpContext.RequestAborted);
        }
        catch (OperationCanceledException) when (HttpContext.RequestAborted.IsCancellationRequested)
        {
            return new EmptyResult();
        }
        catch (Exception ex)
        {
            if (fallback)
            {
                // The user opted into a bounded wait, trading the declared-lossless
                // contract for playback that starts. Strict clients may refuse the
                // lossy bytes; timeout 0 keeps the contract exact.
                _logger.LogInformation(
                    "Lossless wait ended early for {Id} ({Reason}); serving the preview while the fetch continues",
                    id, ex is TimeoutException ? $"timeout {timeout}s" : ex.Message);
                var preview = await TryDirectStreamAsync(provider, externalId, id);
                if (preview is not null) return preview;
            }
            else
            {
                // Never fall back to the lossy stream here. This session declared the id
                // lossless, so lossy bytes would be the same contract violation in reverse.
                _logger.LogWarning(ex, "Lossless acquisition failed for {Id}", id);
            }
            return _responseBuilder.CreateError(format, 70, $"Could not fetch a lossless copy: {ex.Message}");
        }

        if (!System.IO.File.Exists(path))
        {
            return _responseBuilder.CreateError(format, 70, "Lossless copy is no longer on disk");
        }
        return File(System.IO.File.OpenRead(path), GetContentType(path), enableRangeProcessing: true);
    }

    /// <summary>
    /// Proxy the lossy preview straight from the CDN. Returns null when no source resolved.
    /// </summary>
    private async Task<IActionResult?> TryDirectStreamAsync(string provider, string externalId, string id)
    {
        // Forward the client's Range header up the chain so the shim can
        // ask googlevideo for the requested byte range and we can return
        // a proper 206. iOS Subsonic clients refuse to play non-FLAC
        // audio without working byte-range support — our prior 200/none
        // response was what was making Arpeggi/Narjo silently drop
        // every external song from the queue.
        var rangeHeader = Request.Headers.TryGetValue("Range", out var rh) ? rh.ToString() : null;

        var directStream = await _downloadService.GetDirectStreamAsync(
            provider, externalId, rangeHeader, HttpContext.RequestAborted);
        if (directStream is null) return null;

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
                // The provider must come from the candidate. A hardcoded "deezer" never
                // matches the metadata service's provider name, so this always returned null.
                deezerAlbum = await _metadataService.GetAlbumAsync(candidate.ExternalProvider!, candidate.ExternalId!);
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
                    deezerAlbum = await _metadataService.GetAlbumAsync(candidate.ExternalProvider!, candidate.ExternalId!);
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

        // Stable, cached Octo-branded station artwork. Deliberately static: playlist
        // requests never perform a live cover mosaic build.
        if (id.Equals("octo-radio", StringComparison.OrdinalIgnoreCase))
            return ServePlaceholder();

        // Generated radio IDs already resolve through the per-user state store,
        // so station artwork follows the current station name without creating
        // a parallel metadata record or exposing that name in the cover ID.
        var radioStation = _radioStateStore?.FindStation(
            parameters.GetValueOrDefault("u", ""), id);
        if (radioStation is not null)
        {
            var bytes = _coverArtService?.GetRadioStationCover(radioStation.Name);
            if (bytes is not null && bytes.Length > 0) return File(bytes, "image/jpeg");
            return ServePlaceholder();
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
        catch (Exception ex)
        {
            // Unbranded on purpose. This is the user's own file; stamping the Octo
            // logo on it makes Octo look like it is claiming a track the user
            // already owned. Reading embedded art off a cloud-backed mount can take
            // seconds cold, so this path is reached by ordinary slowness, not just
            // by missing art — all the more reason not to brand it.
            _logger.LogDebug("cover art relay failed for local id {Id}: {Msg}", id, ex.Message);
            return ServePlaceholder(branded: false);
        }
    }

    /// <summary>
    /// Returns a 200 response with the Octo placeholder JPEG. Used in every code
    /// path that previously returned 404 — Subsonic clients (Arpeggio especially)
    /// drop play-queue entries whose cover-art request fails, so we always serve
    /// something rather than fail.
    /// </summary>
    private IActionResult ServePlaceholder(bool branded = true)
    {
        var bytes = _coverArtService?.GetPlaceholderCover(branded);
        if (bytes == null || bytes.Length == 0) return NotFound();
        return File(bytes, "image/jpeg");
    }

    #region Helper Methods

    private IActionResult MergeSearchResults(
        (List<object> Songs, List<object> Albums, List<object> Artists) local,
        string? localContentType,
        SearchResult externalResult,
        List<ExternalPlaylist> playlistResult,
        string format,
        string envelope)
    {
        var (localSongs, localAlbums, localArtists) = local;

        var isJson = format == "json" || localContentType?.Contains("json") == true;
        var (mergedSongs, mergedAlbums, mergedArtists) = _modelMapper.MergeSearchResults(
            localSongs,
            localAlbums,
            localArtists,
            externalResult,
            playlistResult,
            isJson);

        if (isJson)
        {
            // Dictionary rather than an anonymous type because the envelope name is
            // decided by the request: search2 answered under searchResult3 is a shape the
            // client never asked for, and a strict one drops the whole payload.
            return _responseBuilder.CreateJsonResponse(new Dictionary<string, object>
            {
                ["status"] = "ok",
                ["version"] = "1.16.1",
                [envelope] = new Dictionary<string, object>
                {
                    ["song"] = mergedSongs,
                    ["album"] = mergedAlbums,
                    ["artist"] = mergedArtists,
                },
            });
        }
        else
        {
            var ns = XNamespace.Get("http://subsonic.org/restapi");
            var searchResult = new XElement(ns + envelope);

            foreach (var artist in mergedArtists.Cast<XElement>())
            {
                searchResult.Add(artist);
            }
            foreach (var album in mergedAlbums.Cast<XElement>())
            {
                searchResult.Add(album);
            }
            foreach (var song in mergedSongs.Cast<XElement>())
            {
                searchResult.Add(song);
            }

            var doc = new XDocument(
                new XElement(ns + "subsonic-response",
                    new XAttribute("status", "ok"),
                    new XAttribute("version", "1.16.1"),
                    searchResult
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
        
        // Starring a whole album. Subsonic sends the album under albumId, though some
        // clients reuse id, so accept either.
        //
        // Two lookups with different jobs: the REGISTRY decides whether this is an album,
        // because ParseSongId reports every registry id as a "song" and would otherwise
        // send an entire album down the single-track download path. ParseSongId still
        // supplies the provider string.
        var albumCandidate = !string.IsNullOrEmpty(itemId) ? itemId : parameters.GetValueOrDefault("albumId", "");
        if (!string.IsNullOrEmpty(albumCandidate)
            && _idRegistry.Lookup(albumCandidate)?.Kind == RoutingKind.Album)
        {
            if (!_subsonicSettings.EffectiveHeartDownloadSources()
                    .Any(step => step.AlbumEnabled == true))
            {
                _logger.LogInformation("Starred album {AlbumId} but no album-heart source is enabled; ignoring", albumCandidate);
                return _responseBuilder.CreateResponse(format, "starred", new { });
            }

            // No storage-mode gate here, unlike the song branch. That gate exists because
            // Permanent mode already downloads a song when it is played; an album star is
            // a request for tracks the user has NOT played, so it must work in every mode.
            var albumProviderName = _localLibraryService.ParseSongId(albumCandidate).provider
                                    ?? SoulseekMetadataService.ProviderName;

            _logger.LogInformation("Starring external album {AlbumId}, triggering full album download", albumCandidate);

            _ = Task.Run(async () =>
            {
                try
                {
                    // Log the size up front: downloads are serialized, so a large album is
                    // a multi-hour job and the user should be able to see what they started.
                    var album = await _metadataService.GetAlbumAsync(albumProviderName, albumCandidate);
                    _logger.LogInformation(
                        "Album star: '{Title}' by {Artist} has {Count} track(s); downloads run one at a time",
                        album?.Title, album?.Artist, album?.Songs.Count ?? 0);
                }
                catch (Exception ex)
                {
                    _logger.LogDebug("Could not pre-read album {AlbumId} for logging: {M}", albumCandidate, ex.Message);
                }
            });

            // An empty exclude means "download every track". The engine already skips
            // tracks that are downloaded or in flight and isolates per-track failures.
            _heartAcquisitions.QueueAlbum(albumProviderName, albumCandidate);

            // Navidrome has never seen this id, so relaying the star would just error.
            return _responseBuilder.CreateResponse(format, "starred", new { });
        }

        // Check if this is an external song (enables download-on-star)
        var (isExternal, provider, externalId) = _localLibraryService.ParseSongId(itemId);
        
        if (isExternal && _subsonicSettings.EffectiveHeartDownloadSources()
                .Any(step => step.SongEnabled == true))
        {
            // No storage-mode gate any more. It used to exclude Permanent on the grounds
            // that playing a track there already downloads it, but that was only ever true
            // through the blocking play path — so in Permanent mode a star fell through to
            // the relay and errored on an id Navidrome has never seen.
            //
            // Stars ride the queue's own channel: explicit user intent is never shed under
            // load the way a play is, and the request carries the album-walk flag rather
            // than inheriting whatever a concurrent play happened to ask for.
            _logger.LogInformation("Starring external song {SongId}, queueing permanent download", itemId);

            _heartAcquisitions.QueueTrack(provider!, externalId!);

            // Return success response immediately
            return _responseBuilder.CreateResponse(format, "starred", new { });
        }
        
        // For non-external items or when download-on-star is disabled, relay to real Subsonic server
        try
        {
            var result = await _proxyService.RelayAsync("rest/star", parameters);
            if (_radioStateStore is not null && IsSuccessfulSubsonicResponse(result.Body, format)
                && parameters.GetValueOrDefault("u") is { Length: > 0 } username
                && !string.IsNullOrEmpty(itemId))
            {
                var song = await _radioTrackResolver.ResolveScrobbleAsync(itemId, parameters);
                if (song is not null) _radioStateStore.MarkHeart(username, itemId, song.Artist, song.Title);
            }
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
        count = Math.Clamp(count, 1, _lastFmSettings.EffectiveRadioTrackCount);

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
        if (_lastFmService == null || !_lastFmService.IsRadioEnabled)
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
            return _responseBuilder.CreateResponse(format, responseKey, new { });
        }

        // Strip collab/feature decoration so Last.fm finds the canonical artist.
        var lookupArtist = LastFmRadioSeedNormalizer.Artist(artistName) ?? artistName;
        var lookupTitle  = LastFmRadioSeedNormalizer.Title(trackTitle) ?? trackTitle;
        _logger.LogInformation("Getting similar songs for {Artist} - {Title} (lookup: {LookA} - {LookT})",
            artistName, trackTitle, lookupArtist, lookupTitle);

        var similarTracks = await _lastFmService.GetSimilarTracksAsync(lookupArtist, lookupTitle, count);

        if (similarTracks.Count == 0)
        {
            _logger.LogInformation("No similar tracks found from Last.fm");
            return _responseBuilder.CreateResponse(format, responseKey, new { });
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
                return await _radioTrackResolver.ResolveAsync(
                    track.Artist, track.Title, track.Duration, parameters);
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
        // taps don't pay the full cold yt-dlp resolve. The prewarm method
        // handles its own concurrency limit, shared across every trigger, and
        // marks its shim calls as background so they cannot occupy the slots
        // the shim reserves for interactive plays. Local songs are skipped
        // automatically by the prewarmer (they have no registry entry).
        _ = _metadataService.PrewarmYouTubeIdsAsync(resolvedSongs, topN: 8);

        return BuildSimilarSongsResponse(format, resolvedSongs, responseKey);
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
        var ids = await _requestParser.ExtractParameterValuesAsync(Request, "id", HttpContext.RequestAborted);
        var submissions = await _requestParser.ExtractParameterValuesAsync(Request, "submission", HttpContext.RequestAborted);
        var times = await _requestParser.ExtractParameterValuesAsync(Request, "time", HttpContext.RequestAborted);
        var parameters = await ExtractAllParameters();
        if (ids.Count == 0 && parameters.GetValueOrDefault("id") is { Length: > 0 } singleId) ids = [singleId];
        var id = ids.FirstOrDefault() ?? "";
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
            var relayParameters = parameters
                .Where(pair => pair.Key is not ("id" or "submission" or "time")).ToList();
            relayParameters.AddRange(ids.Select(value => new KeyValuePair<string, string>("id", value)));
            relayParameters.AddRange(submissions.Select(value => new KeyValuePair<string, string>("submission", value)));
            relayParameters.AddRange(times.Select(value => new KeyValuePair<string, string>("time", value)));
            var result = await _proxyService.RelayAsync("rest/scrobble", relayParameters);
            if (IsSuccessfulSubsonicResponse(result.Body, format))
                await LearnFromScrobblesAsync(ids, submissions, times, parameters);
            return File(result.Body, result.ContentType ?? $"application/{format}");
        }
        catch (HttpRequestException)
        {
            // Even if upstream is briefly unhappy, return 200 so the client
            // doesn't think scrobble is broken — the prewarm side already fired.
            return _responseBuilder.CreateResponse(format, "scrobble", new { });
        }
    }

    private async Task LearnFromScrobblesAsync(IReadOnlyList<string> ids,
        IReadOnlyList<string> submissions, IReadOnlyList<string> times,
        IReadOnlyDictionary<string, string> authenticatedParameters)
    {
        if (_radioStateStore is null || !_lastFmSettings.EnableRadio
            || !_lastFmSettings.EnablePersonalizedStations) return;
        var username = authenticatedParameters.GetValueOrDefault("u", "").Trim();
        if (username.Length == 0) return;
        var recorded = false;
        for (var index = 0; index < ids.Count; index++)
        {
            // Explicit start/now-playing scrobbles are not completed plays. Clients that
            // omit submission are accepted because many only send one credible event.
            if (index < submissions.Count && !IsTrue(submissions[index])) continue;
            try
            {
                var song = await _radioTrackResolver.ResolveScrobbleAsync(ids[index], authenticatedParameters);
                if (song is null || song.Artist.Length == 0 || song.Title.Length == 0) continue;
                var playedAt = DateTime.UtcNow;
                if (index < times.Count && long.TryParse(times[index], out var unix))
                {
                    try { playedAt = DateTimeOffset.FromUnixTimeMilliseconds(unix).UtcDateTime; }
                    catch (ArgumentOutOfRangeException) { /* retain now */ }
                }
                recorded |= _radioStateStore.RecordPlay(username, new LastFmRadioPlay
                {
                    SongId = ids[index], Artist = song.Artist, Title = song.Title,
                    Album = song.Album, Genre = song.Genre, Duration = song.Duration,
                    IsLocal = song.IsLocal, PlayedAtUtc = playedAt, Source = "scrobble"
                });
            }
            catch (Exception ex) { _logger.LogDebug(ex, "Radio ignored unreadable scrobble {Id}", ids[index]); }
        }
        if (!recorded || _radioRefreshQueue is null) return;
        var user = _radioStateStore.GetUser(username);
        if (LastFmRadioRefreshPolicy.ShouldRefreshAfterPlay(user, _lastFmSettings))
            _radioRefreshQueue.Enqueue(username);
    }

    private static bool IsTrue(string value) => value.Equals("true", StringComparison.OrdinalIgnoreCase)
        || value == "1";

    private static bool IsSuccessfulSubsonicResponse(byte[] body, string format)
    {
        try
        {
            if (format.Equals("json", StringComparison.OrdinalIgnoreCase))
            {
                using var doc = JsonDocument.Parse(body);
                return doc.RootElement.TryGetProperty("subsonic-response", out var response)
                    && response.TryGetProperty("status", out var status) && status.GetString() == "ok";
            }
            var document = XDocument.Parse(Encoding.UTF8.GetString(body));
            return document.Root?.Attribute("status")?.Value == "ok";
        }
        catch { return false; }
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
    // OpenSubsonic reportPlayback: Feishin pings this on play-start and during
    // playback (176 hits in one session). For external ids Navidrome has no such
    // media and returns an error, so we ack with ok; local ids relay through so
    // Navidrome's now-playing stays accurate.
    [HttpGet, HttpPost]
    [Route("rest/reportPlayback")]
    [Route("rest/reportPlayback.view")]
    public async Task<IActionResult> ReportPlayback()
    {
        var parameters = await ExtractAllParameters();
        var format = parameters.GetValueOrDefault("f", "xml");
        var mediaId = parameters.GetValueOrDefault("mediaId", parameters.GetValueOrDefault("id", ""));
        var (isExternal, _, _) = _localLibraryService.ParseSongId(mediaId);

        if (!isExternal && !string.IsNullOrEmpty(mediaId))
        {
            var relay = await _proxyService.RelaySafeAsync("rest/reportPlayback", parameters);
            if (relay.Success && relay.Body != null)
                return File(relay.Body, relay.ContentType ?? $"application/{format}");
        }
        return _responseBuilder.CreateResponse(format, "reportPlayback", new { });
    }

    // Octo has no jukebox device. Relaying surfaced a misleading "Error
    // connecting to Subsonic server"; return a clean, plain "not supported" so
    // the client just disables jukebox mode instead of logging a scary error.
    [HttpGet, HttpPost]
    [Route("rest/jukeboxControl")]
    [Route("rest/jukeboxControl.view")]
    public async Task<IActionResult> JukeboxControl()
    {
        var parameters = await ExtractAllParameters();
        var format = parameters.GetValueOrDefault("f", "xml");
        return _responseBuilder.CreateError(format, 0, "Jukebox is not supported");
    }

    // OpenSubsonic getLyricsBySongId — Feishin fetches this every time a song
    // plays. External tracks have no lyrics in Navidrome, so it returned code 70
    // "data not found" per play; return an empty-but-ok lyrics list instead.
    // (Real synced lyrics from an open source like lrclib are a future add.)
    [HttpGet, HttpPost]
    [Route("rest/getLyricsBySongId")]
    [Route("rest/getLyricsBySongId.view")]
    public async Task<IActionResult> GetLyricsBySongId()
    {
        var parameters = await ExtractAllParameters();
        var id = parameters.GetValueOrDefault("id", "");
        var format = parameters.GetValueOrDefault("f", "xml");
        var (isExternal, _, _) = _localLibraryService.ParseSongId(id);

        if (isExternal)
        {
            if (format == "json")
                return _responseBuilder.CreateJsonResponse(new Dictionary<string, object>
                {
                    ["status"] = "ok",
                    ["version"] = "1.16.1",
                    ["lyricsList"] = new { structuredLyrics = Array.Empty<object>() },
                });
            return _responseBuilder.CreateResponse(format, "lyricsList", new { });
        }

        var relay = await _proxyService.RelaySafeAsync("rest/getLyricsBySongId", parameters);
        if (relay.Success && relay.Body != null)
            return File(relay.Body, relay.ContentType ?? $"application/{format}");
        return _responseBuilder.CreateResponse(format, "lyricsList", new { });
    }

    // Album/artist "info" panels. For external tracks these used to fall through
    // to Navidrome (which has no such id) and return "data not found" — the error
    // spam a client logs per row. Now they return a valid response with real
    // Deezer art for external ids, and only relay for genuine local ids.
    [HttpGet, HttpPost]
    [Route("rest/getAlbumInfo2")]
    [Route("rest/getAlbumInfo2.view")]
    [Route("rest/getAlbumInfo")]
    [Route("rest/getAlbumInfo.view")]
    public async Task<IActionResult> GetAlbumInfo2()
    {
        var parameters = await ExtractAllParameters();
        var id = parameters.GetValueOrDefault("id", "");
        var format = parameters.GetValueOrDefault("f", "xml");
        var (isExternal, provider, externalId) = _localLibraryService.ParseSongId(id);

        if (isExternal)
        {
            var album = await _metadataService.GetAlbumAsync(provider!, externalId!);
            var url = album?.CoverArtUrl ?? "";
            return _responseBuilder.CreateInfoResponse(format, "albumInfo", new Dictionary<string, string>
            {
                ["notes"] = "",
                ["smallImageUrl"] = url,
                ["mediumImageUrl"] = url,
                ["largeImageUrl"] = url,
            });
        }

        var relay = await _proxyService.RelaySafeAsync("rest/getAlbumInfo2", parameters);
        if (relay.Success && relay.Body != null)
            return File(relay.Body, relay.ContentType ?? $"application/{format}");
        return _responseBuilder.CreateResponse(format, "albumInfo", new { });
    }

    [HttpGet, HttpPost]
    [Route("rest/getArtistInfo2")]
    [Route("rest/getArtistInfo2.view")]
    [Route("rest/getArtistInfo")]
    [Route("rest/getArtistInfo.view")]
    public async Task<IActionResult> GetArtistInfo2()
    {
        var parameters = await ExtractAllParameters();
        var id = parameters.GetValueOrDefault("id", "");
        var format = parameters.GetValueOrDefault("f", "xml");
        var (isExternal, provider, externalId) = _localLibraryService.ParseSongId(id);

        if (isExternal)
        {
            var artist = await _metadataService.GetArtistAsync(provider!, externalId!);
            var url = artist?.ImageUrl ?? "";
            return _responseBuilder.CreateInfoResponse(format, "artistInfo2", new Dictionary<string, string>
            {
                ["biography"] = "",
                ["smallImageUrl"] = url,
                ["mediumImageUrl"] = url,
                ["largeImageUrl"] = url,
            });
        }

        var relay = await _proxyService.RelaySafeAsync("rest/getArtistInfo2", parameters);
        if (relay.Success && relay.Body != null)
            return File(relay.Body, relay.ContentType ?? $"application/{format}");
        return _responseBuilder.CreateResponse(format, "artistInfo2", new { });
    }

    [Route("{**endpoint}")]
    public async Task<IActionResult> GenericEndpoint(string endpoint)
    {
        if (IsOctoOwnedPath(endpoint))
        {
            return NotFound();
        }

        var parameters = await ExtractAllParameters();
        var format = parameters.GetValueOrDefault("f", "xml");

        var nativeRadio = await TryServeNativeRadioAsync(endpoint, parameters);
        if (nativeRadio != null) return nativeRadio;

        // Safety net (client-agnostic): any endpoint we don't explicitly handle,
        // called with one of our external ids, would relay to Navidrome and come
        // back "data not found" — Navidrome has no such id. Degrade to a graceful
        // ok so a client we haven't specifically tested never errors on external
        // tracks. Endpoints that need real external data have their own handlers.
        if (HasExternalId(parameters))
        {
            return _responseBuilder.CreateResponse(format, ElementFor(endpoint), new { });
        }

        // Navidrome-native single-song detail for an external id. Native clients
        // load the now-playing view via GET /api/song/{id}; the id lives in the
        // path, not the query, so HasExternalId above (query-only) misses it and a
        // relay would 500. Serve the synthetic native object instead.
        var nativeSong = await TryServeNativeExternalSongAsync(endpoint);
        if (nativeSong != null) return nativeSong;

        // Navidrome-native discovery. Navidrome-mode clients (e.g. Feishin) search
        // songs via GET /api/song?title=..., which otherwise relays straight through
        // and only ever surfaces the local library. This mirrors the Subsonic
        // search3 hijack onto the native API using the same discovery core, so
        // discovery is a property of the request shape, not the client's mode.
        var nativeSearch = await TryInjectNativeSongSearchAsync(endpoint, parameters);
        if (nativeSearch != null) return nativeSearch;

        // Native album detail for an external id. Same path-vs-query problem as the
        // song case above: HasExternalId can't see an id carried in the path.
        var nativeAlbum = await TryServeNativeExternalAlbumAsync(endpoint);
        if (nativeAlbum != null) return nativeAlbum;

        // Native album search, the twin of the search3 album injection.
        var nativeAlbumSearch = await TryInjectNativeAlbumSearchAsync(endpoint, parameters);
        if (nativeAlbumSearch != null) return nativeAlbumSearch;

        // Native album tracklist. In Navidrome mode a client does NOT get an album's
        // tracks from the album object; it asks for them separately by album_id. Note
        // the parameter is snake_case, so HasExternalId (which checks "albumId") never
        // intercepts it and this handler gets its chance.
        var nativeAlbumSongs = await TryServeNativeAlbumSongsAsync(endpoint, parameters);
        if (nativeAlbumSongs != null) return nativeAlbumSongs;

        try
        {
            // Faithful relay: forward the caller's method + body + status so native
            // Navidrome endpoints (e.g. the POST /auth/login some clients use) work,
            // not just GET-shaped Subsonic calls.
            var raw = await _proxyService.RelayRawAsync(endpoint, parameters);

            // Learn Octo's own Navidrome identity from a client's native sign-in as
            // it passes through, so background work (music-folder detection, an
            // authenticated rescan) has an admin token without any extra config.
            if (raw.Status == 200 && endpoint.Equals("auth/login", StringComparison.OrdinalIgnoreCase))
                _navIdentity.CaptureLogin(raw.Body);

            Response.StatusCode = raw.Status;
            foreach (var h in raw.ResponseHeaders)
                Response.Headers[h.Key] = h.Value;
            Response.ContentType = raw.ContentType ?? $"application/{format}";
            await Response.Body.WriteAsync(raw.Body);
            return new EmptyResult();
        }
        catch (Exception ex)
        {
            return _responseBuilder.CreateError(format, 0, $"Error connecting to Subsonic server: {ex.Message}");
        }
    }

    /// <summary>True if any id-shaped parameter is one of Octo's external ids.</summary>
    private bool HasExternalId(Dictionary<string, string> parameters)
    {
        foreach (var key in new[] { "id", "mediaId", "albumId", "artistId" })
        {
            if (parameters.TryGetValue(key, out var v) && !string.IsNullOrEmpty(v)
                && _localLibraryService.ParseSongId(v).isExternal)
                return true;
        }
        return false;
    }

    private async Task<IActionResult?> TryServeNativeRadioAsync(string endpoint,
        Dictionary<string, string> parameters)
    {
        const string prefix = "api/playlist";
        if (!endpoint.Equals(prefix, StringComparison.OrdinalIgnoreCase)
            && !endpoint.StartsWith(prefix + "/", StringComparison.OrdinalIgnoreCase)) return null;

        var tail = endpoint.Length == prefix.Length ? "" : endpoint[(prefix.Length + 1)..].Trim('/');
        var id = tail.Split('/', StringSplitOptions.RemoveEmptyEntries).FirstOrDefault() ?? "";
        var reserved = id.StartsWith("or", StringComparison.Ordinal) && id.Length == 22;
        if (reserved && !HttpMethods.IsGet(Request.Method))
            return StatusCode(StatusCodes.Status405MethodNotAllowed,
                new { error = "Octo Radio stations are read-only" });
        if (tail.Length > 0 && !reserved) return null;

        // A successful upstream list validates the native bearer token before Octo
        // reveals per-user state. The JWT payload only identifies the profile after
        // Navidrome has accepted its signature and expiry.
        var relayEndpoint = tail.Length == 0 ? endpoint : prefix;
        var relayParameters = new Dictionary<string, string>(parameters);
        if (tail.Length == 0 && HttpMethods.IsGet(Request.Method))
        {
            // Page after merging, so Radio rows cannot disappear merely because the
            // upstream page was already full.
            relayParameters["_start"] = "0";
            relayParameters["_end"] = "1000";
        }
        var raw = await _proxyService.RelayRawAsync(relayEndpoint, relayParameters);
        if (raw.Status is < 200 or >= 300)
        {
            Response.StatusCode = raw.Status;
            return File(raw.Body, raw.ContentType ?? "application/json");
        }
        var username = NativeUsername(parameters);
        var stations = PlaylistStations(username);
        if (tail.Length == 0)
        {
            try
            {
                var node = JsonNode.Parse(raw.Body);
                var rows = node as JsonArray;
                if (rows is null) return File(raw.Body, raw.ContentType ?? "application/json");
                foreach (var station in stations) rows.Add(NativeStation(station));
                var total = rows.Count;
                var start = Math.Max(0, parameters.TryGetValue("_start", out var startText)
                    && int.TryParse(startText, out var parsedStart) ? parsedStart : 0);
                var end = parameters.TryGetValue("_end", out var endText)
                    && int.TryParse(endText, out var parsedEnd) ? parsedEnd : total;
                var page = new JsonArray(rows.Skip(start).Take(Math.Max(0, end - start))
                    .Select(row => row?.DeepClone()).ToArray());
                Response.Headers["X-Total-Count"] = total.ToString();
                QueueRefreshIfStale(username);
                return File(Encoding.UTF8.GetBytes(page.ToJsonString()), "application/json");
            }
            catch { return File(raw.Body, raw.ContentType ?? "application/json"); }
        }

        var stationMatch = stations.FirstOrDefault(station => station.Id == id);
        if (stationMatch is null) return NotFound(new { error = "Radio station not found for this user" });
        if (tail.EndsWith("/tracks", StringComparison.OrdinalIgnoreCase))
        {
            var songs = await MaterializeStationAsync(stationMatch, parameters);
            _radioQueueStore.Register(songs.Select(song => song.Id));
            _ = _metadataService.PrewarmYouTubeIdsAsync(songs, 8);
            var start = Math.Max(0, parameters.TryGetValue("_start", out var startText)
                && int.TryParse(startText, out var parsedStart) ? parsedStart : 0);
            var end = parameters.TryGetValue("_end", out var endText)
                && int.TryParse(endText, out var parsedEnd) ? parsedEnd : songs.Count;
            var page = new JsonArray(songs.Skip(start).Take(Math.Max(0, end - start))
                .Select(song => (JsonNode)BuildNativeSongObject(song)).ToArray());
            Response.Headers["X-Total-Count"] = songs.Count.ToString();
            return File(Encoding.UTF8.GetBytes(page.ToJsonString()), "application/json");
        }
        return File(Encoding.UTF8.GetBytes(NativeStation(stationMatch).ToJsonString()), "application/json");
    }

    private string NativeUsername(IReadOnlyDictionary<string, string> parameters)
    {
        if (parameters.GetValueOrDefault("u") is { Length: > 0 } username) return username;
        var header = Request.Headers["X-Nd-Authorization"].FirstOrDefault()
            ?? Request.Headers.Authorization.FirstOrDefault();
        var token = header?.StartsWith("Bearer ", StringComparison.OrdinalIgnoreCase) == true
            ? header[7..] : header;
        if (_navIdentity.UsernameForNativeToken(token) is { Length: > 0 } captured) return captured;
        try
        {
            var payload = token?.Split('.')[1].Replace('-', '+').Replace('_', '/');
            if (payload is not null)
            {
                payload = payload.PadRight(payload.Length + (4 - payload.Length % 4) % 4, '=');
                using var doc = JsonDocument.Parse(Convert.FromBase64String(payload));
                foreach (var claim in new[] { "username", "preferred_username", "user", "name", "sub" })
                    if (doc.RootElement.TryGetProperty(claim, out var value) && value.GetString() is { Length: > 0 } found)
                        return found;
            }
        }
        catch { /* an accepted but opaque token simply exposes no Radio rows */ }
        return "";
    }

    private static JsonObject NativeStation(LastFmRadioStation station) => new()
    {
        ["id"] = station.Id, ["name"] = station.Name, ["comment"] = "Generated by Octo Radio",
        ["ownerName"] = station.Owner, ["public"] = false, ["songCount"] = station.Tracks.Count,
        ["duration"] = station.Tracks.Sum(track => track.Duration ?? 180),
        ["createdAt"] = station.CreatedUtc, ["updatedAt"] = station.ChangedUtc,
        ["path"] = "", ["smartPlaylist"] = true, ["readonly"] = true,
        ["validUntil"] = station.ValidUntilUtc
    };

    /// <summary>rest/getSomething -> "something"; best-effort element name for an
    /// empty-ok response (JSON ignores it; XML just needs a well-formed element).</summary>
    private static string ElementFor(string endpoint)
    {
        var name = (endpoint.Split('/').LastOrDefault() ?? "response").Replace(".view", "");
        if (name.StartsWith("get", StringComparison.OrdinalIgnoreCase) && name.Length > 3)
            name = char.ToLowerInvariant(name[3]) + name[4..];
        return string.IsNullOrEmpty(name) ? "response" : name;
    }

    /// <summary>
    /// Native single-song fetch for one of Octo's external ids. Navidrome-mode
    /// clients load the now-playing detail via GET /api/song/{id}; relaying an
    /// external id to Navidrome 500s (it has no such song). Rebuild the song from
    /// the id via the same metadata core getSong uses and return it in native shape.
    /// Returns null (fall through to relay) for anything but a leaf external-id fetch.
    /// </summary>
    private async Task<IActionResult?> TryServeNativeExternalSongAsync(string endpoint)
    {
        const string prefix = "api/song/";
        if (!endpoint.StartsWith(prefix, StringComparison.OrdinalIgnoreCase)) return null;
        var id = endpoint[prefix.Length..].Trim('/');
        if (string.IsNullOrEmpty(id) || id.Contains('/')) return null; // leaf id only

        var (isExternal, provider, externalId) = _localLibraryService.ParseSongId(id);
        if (!isExternal) return null;

        var song = await _metadataService.GetSongAsync(provider!, externalId!);
        if (song == null) return null;

        // Enrich (Deezer album/year/art) so the detail matches the search-list row
        // exactly; without this a client that refreshes now-playing from the detail
        // would blank the album. Cached, so this is cheap after the initial search.
        var one = new List<Song> { song };
        await _metadataService.EnrichExternalSongsAsync(one);

        // Lazy-resolve the accurate YouTube duration at play. Navidrome-mode clients
        // re-fetch this endpoint when a track starts, so this is where the scrub bar
        // gets the real length for results past the search's top-N (which are already
        // resolved). Backed by the shim's persistent cache, so it is a disk hit for
        // anything seen before and resolved-once-then-instant otherwise.
        await _metadataService.ResolveTopDurationsAsync(one);

        var bytes = Encoding.UTF8.GetBytes(BuildNativeSongObject(song).ToJsonString());
        Response.StatusCode = 200;
        Response.ContentType = "application/json";
        await Response.Body.WriteAsync(bytes);
        return new EmptyResult();
    }

    /// <summary>
    /// Native-API twin of the Subsonic search3 hijack. When a Navidrome-mode client
    /// searches songs (GET /api/song?title=...), relay the real query, then append
    /// external discovery results serialized in Navidrome's native song shape. Play
    /// and cover art need no special handling: native clients stream via /rest/stream
    /// and fetch art via /rest/getCoverArt using the salt+token from login, and our
    /// existing handlers already resolve Octo's external ids there.
    ///
    /// Returns null to fall through to the normal faithful relay whenever this is not
    /// a first-page native song search we should touch, so library browsing, paging,
    /// and every other native endpoint stay pure passthrough.
    /// </summary>
    private async Task<IActionResult?> TryInjectNativeSongSearchAsync(
        string endpoint, Dictionary<string, string> parameters)
    {
        if (!string.Equals(endpoint, "api/song", StringComparison.OrdinalIgnoreCase))
            return null;

        // Only a text search carries discovery intent. No title filter = library
        // browse; a non-zero _start = a later page. Both stay passthrough so we
        // never duplicate injected rows across pages or disturb navigation.
        var term = parameters.GetValueOrDefault("title", "").Trim();
        if (string.IsNullOrWhiteSpace(term)) return null;
        if (parameters.TryGetValue("_start", out var startStr)
            && int.TryParse(startStr, out var start) && start > 0)
            return null;

        // Relay the real query first; we append to whatever the library returned.
        RawRelayResult raw;
        try { raw = await _proxyService.RelayRawAsync(endpoint, parameters); }
        catch { return null; } // upstream trouble -> let the normal path surface it

        if (raw.Status != 200) return null;

        // Native list endpoints answer with a bare JSON array + X-Total-Count. If the
        // body is any other shape (error object, unexpected version), don't touch it.
        JsonArray? realArr;
        try { realArr = JsonNode.Parse(raw.Body) as JsonArray; }
        catch { return null; }
        if (realArr == null) return null;

        // Stay inside the page window the client asked for so a single page holds
        // everything and the client never pages into a duplicated injection.
        var end = parameters.TryGetValue("_end", out var endStr) && int.TryParse(endStr, out var e)
            ? e : realArr.Count + 60;
        const int MaxExternalNative = 60;
        var target = Math.Min(Math.Max(0, end - realArr.Count), MaxExternalNative);
        if (target <= 0) return null;

        // Same discovery core as Subsonic search3: Last.fm fan-out, Deezer enrich,
        // accurate YouTube durations for the top of the list. Shared with search3, so a
        // client that searches both ways for one query only pays for it once.
        var externalSongs = (await _externalSearch.GetAsync(term)).Take(target).ToList();
        if (externalSongs.Count == 0) return null;

        foreach (var s in externalSongs)
            realArr.Add(BuildNativeSongObject(s));

        // Register for the scrobble-driven prewarm, same as search3.
        _radioQueueStore.Register(externalSongs.Select(s => s.Id));

        var bytes = Encoding.UTF8.GetBytes(realArr.ToJsonString());
        Response.StatusCode = 200;
        foreach (var h in raw.ResponseHeaders)
        {
            if (string.Equals(h.Key, "X-Total-Count", StringComparison.OrdinalIgnoreCase)) continue;
            Response.Headers[h.Key] = h.Value;
        }
        Response.Headers["X-Total-Count"] = realArr.Count.ToString();
        Response.ContentType = raw.ContentType ?? "application/json";
        await Response.Body.WriteAsync(bytes);
        return new EmptyResult();
    }

    /// <summary>
    /// Serializes one external Song into Navidrome's native song JSON shape. Only the
    /// fields a Navidrome-mode client reads to render and play a row are populated.
    /// The id is Octo's external id, which /rest/stream and /rest/getCoverArt resolve.
    /// </summary>
    private JsonObject BuildNativeSongObject(Song s)
    {
        var artistId = string.IsNullOrEmpty(s.ArtistId) ? s.Id + "-ar" : s.ArtistId!;
        var albumId = string.IsNullOrEmpty(s.AlbumId) ? s.Id + "-al" : s.AlbumId!;
        var duration = s.Duration ?? 0;
        // Navidrome-mode clients take their contract from HERE and never from
        // SubsonicResponseBuilder, so this has to follow the same setting or the native
        // path keeps promising m4a while /rest/stream hands back a FLAC. Note the two
        // serializers are not symmetric: this one emits no contentType at all, and
        // defaults an unknown duration to 0 where the Subsonic one uses 180.
        var lossless = _subsonicSettings.WaitForLosslessOnPlay;
        var suffix = lossless ? "flac" : "m4a";
        var bitRate = lossless ? 950 : 128; // format 140 AAC ~128 kbps; FLAC lands ~850-1000
        long size = duration > 0 ? (long)duration * bitRate * 1000L / 8 : 0;

        var o = new JsonObject
        {
            ["id"] = s.Id,
            ["path"] = $"{Sanitize(s.Artist)}/{Sanitize(s.Album)}/{Sanitize(s.Title)}.{suffix}",
            ["title"] = s.Title,
            ["album"] = s.Album ?? "",
            ["artist"] = s.Artist ?? "",
            ["artistId"] = artistId,
            ["albumArtist"] = string.IsNullOrEmpty(s.AlbumArtist) ? s.Artist : s.AlbumArtist,
            ["albumArtistId"] = artistId,
            ["albumId"] = albumId,
            ["hasCoverArt"] = true,
            ["trackNumber"] = s.Track ?? 0,
            ["discNumber"] = s.DiscNumber ?? 1,
            ["size"] = size,
            ["suffix"] = suffix,
            ["duration"] = duration,
            ["bitRate"] = bitRate,
            ["playCount"] = 0,
            // Fixed old timestamp: injected tracks are not "recently added" library
            // items, so they should never crowd a client's recently-added view.
            ["createdAt"] = "2020-01-01T00:00:00Z",
            ["updatedAt"] = "2020-01-01T00:00:00Z",
        };
        if (s.Year is int y && y > 0) o["year"] = y;
        if (!string.IsNullOrEmpty(s.Genre)) o["genre"] = s.Genre;
        return o;
    }

    /// <summary>
    /// Native-API twin of the search3 album injection. Navidrome filters albums with a
    /// full-text "name" parameter. Returns null for anything that is not a first-page
    /// album search, so library browsing and paging stay pure passthrough.
    ///
    /// NOTE: Feishin 1.3.0 does NOT reach this. Its album search goes through
    /// rest/search3.view even in Navidrome mode, and every /api/album call it makes is
    /// browsing (_sort=name|random|max_year|play_count|recently_added, artist_id=) with
    /// no "name" filter. This is kept for clients that DO filter by name, matching the
    /// same client-agnostic reasoning as the external-id interceptor in GenericEndpoint;
    /// album detail (/api/album/{id}) and its tracklist (/api/song?album_id=) are the
    /// two native handlers Feishin actually depends on.
    /// </summary>
    private async Task<IActionResult?> TryInjectNativeAlbumSearchAsync(
        string endpoint, Dictionary<string, string> parameters)
    {
        if (!string.Equals(endpoint, "api/album", StringComparison.OrdinalIgnoreCase))
            return null;

        var term = parameters.GetValueOrDefault("name", "").Trim();
        if (string.IsNullOrWhiteSpace(term)) return null;
        if (parameters.TryGetValue("_start", out var startStr)
            && int.TryParse(startStr, out var start) && start > 0)
            return null;

        RawRelayResult raw;
        try { raw = await _proxyService.RelayRawAsync(endpoint, parameters); }
        catch { return null; }
        if (raw.Status != 200) return null;

        JsonArray? realArr;
        try { realArr = JsonNode.Parse(raw.Body) as JsonArray; }
        catch { return null; }
        if (realArr == null) return null;

        var end = parameters.TryGetValue("_end", out var endStr) && int.TryParse(endStr, out var e)
            ? e : realArr.Count + 20;
        const int MaxExternalAlbums = 20;
        var target = Math.Min(Math.Max(0, end - realArr.Count), MaxExternalAlbums);
        if (target <= 0) return null;

        List<Album> externalAlbums;
        try { externalAlbums = await _metadataService.SearchAlbumsAsync(term, target); }
        catch { return null; }
        if (externalAlbums.Count == 0) return null;

        // Newer Navidrome is multi-library and rows carry a libraryId. Inherit it from a
        // real row rather than hardcoding, so injected albums belong to the same library.
        var libraryId = 1;
        if (realArr.Count > 0 && realArr[0] is JsonObject first
            && first.TryGetPropertyValue("libraryId", out var lib) && lib is not null
            && int.TryParse(lib.ToString(), out var parsedLib))
            libraryId = parsedLib;

        // Don't inject an album the library already returned.
        var localKeys = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        foreach (var node in realArr)
        {
            if (node is not JsonObject o) continue;
            var name = o.TryGetPropertyValue("name", out var n) ? n?.ToString() : null;
            var aa = o.TryGetPropertyValue("albumArtist", out var v) ? v?.ToString() : null;
            if (!string.IsNullOrWhiteSpace(name)) localKeys.Add($"{aa?.Trim()}|{name.Trim()}");
        }

        var added = 0;
        foreach (var album in externalAlbums)
        {
            if (localKeys.Contains($"{album.Artist?.Trim()}|{album.Title?.Trim()}")) continue;
            realArr.Add(BuildNativeAlbumObject(album, libraryId));
            added++;
        }
        if (added == 0) return null;

        var bytes = Encoding.UTF8.GetBytes(realArr.ToJsonString());
        Response.StatusCode = 200;
        foreach (var h in raw.ResponseHeaders)
        {
            if (string.Equals(h.Key, "X-Total-Count", StringComparison.OrdinalIgnoreCase)) continue;
            Response.Headers[h.Key] = h.Value;
        }
        Response.Headers["X-Total-Count"] = realArr.Count.ToString();
        Response.ContentType = raw.ContentType ?? "application/json";
        await Response.Body.WriteAsync(bytes);
        return new EmptyResult();
    }

    /// <summary>
    /// Native single-album detail: GET /api/album/{id} for one of Octo's album ids.
    /// </summary>
    private async Task<IActionResult?> TryServeNativeExternalAlbumAsync(string endpoint)
    {
        const string prefix = "api/album/";
        if (!endpoint.StartsWith(prefix, StringComparison.OrdinalIgnoreCase)) return null;
        var id = endpoint[prefix.Length..].Trim('/');
        if (string.IsNullOrEmpty(id) || id.Contains('/')) return null; // leaf id only

        if (_idRegistry.Lookup(id)?.Kind != RoutingKind.Album) return null;

        var album = await _metadataService.GetAlbumAsync(SoulseekMetadataService.ProviderName, id);
        if (album == null) return null;

        var bytes = Encoding.UTF8.GetBytes(BuildNativeAlbumObject(album, 1).ToJsonString());
        Response.StatusCode = 200;
        Response.ContentType = "application/json";
        await Response.Body.WriteAsync(bytes);
        return new EmptyResult();
    }

    /// <summary>
    /// Native album tracklist: GET /api/song?album_id={externalAlbumId}. A Navidrome-mode
    /// client fetches an album's tracks separately from the album object, so without this
    /// an injected album opens empty.
    /// </summary>
    private async Task<IActionResult?> TryServeNativeAlbumSongsAsync(
        string endpoint, Dictionary<string, string> parameters)
    {
        if (!string.Equals(endpoint, "api/song", StringComparison.OrdinalIgnoreCase))
            return null;

        var albumId = parameters.GetValueOrDefault("album_id", "").Trim();
        if (string.IsNullOrEmpty(albumId)) return null;
        if (_idRegistry.Lookup(albumId)?.Kind != RoutingKind.Album) return null;

        var album = await _metadataService.GetAlbumAsync(SoulseekMetadataService.ProviderName, albumId);
        if (album == null) return null;

        var arr = new JsonArray();
        foreach (var song in album.Songs) arr.Add(BuildNativeSongObject(song));

        var bytes = Encoding.UTF8.GetBytes(arr.ToJsonString());
        Response.StatusCode = 200;
        Response.Headers["X-Total-Count"] = album.Songs.Count.ToString();
        Response.ContentType = "application/json";
        await Response.Body.WriteAsync(bytes);
        return new EmptyResult();
    }

    /// <summary>
    /// Serializes one external Album into Navidrome's native album JSON shape.
    /// Note Navidrome's album model has NO "artist"/"artistId" field — it uses
    /// albumArtist/albumArtistId — and "duration" is seconds as a float.
    /// </summary>
    private static JsonObject BuildNativeAlbumObject(Album a, int libraryId)
    {
        var duration = a.Songs.Sum(s => s.Duration ?? 0);
        var songCount = a.Songs.Count > 0 ? a.Songs.Count : (a.SongCount ?? 0);
        var year = a.Year ?? 0;

        var o = new JsonObject
        {
            ["id"] = a.Id,
            ["libraryId"] = libraryId,
            ["name"] = a.Title,
            ["albumArtist"] = a.Artist ?? "",
            ["albumArtistId"] = string.IsNullOrEmpty(a.ArtistId) ? a.Id + "-ar" : a.ArtistId!,
            ["maxYear"] = year,
            ["minYear"] = year,
            ["compilation"] = false,
            // Explicitly not missing: a client that respects this flag hides rows otherwise.
            ["missing"] = false,
            ["songCount"] = songCount,
            ["duration"] = (double)duration,
            ["size"] = 0,
            ["playCount"] = 0,
            // Fixed old timestamp, same reasoning as BuildNativeSongObject: injected rows
            // must never crowd a client's recently-added view.
            ["createdAt"] = "2020-01-01T00:00:00Z",
            ["updatedAt"] = "2020-01-01T00:00:00Z",
        };
        if (!string.IsNullOrEmpty(a.Genre)) o["genre"] = a.Genre;
        return o;
    }

    private static string Sanitize(string? s) =>
        string.IsNullOrEmpty(s) ? "Unknown" : s.Replace('/', '_').Replace('\\', '_');

    private static bool IsOctoOwnedPath(string endpoint)
    {
        if (string.IsNullOrEmpty(endpoint)) return false;
        var lower = endpoint.ToLowerInvariant();
        // Only Octo's OWN paths. Navidrome's native API also lives under /api/*
        // (api/album, api/song, ...), so we must NOT claim all of /api/ — only
        // api/admin — or Navidrome-mode clients can't reach the native API.
        return lower.StartsWith("admin", StringComparison.Ordinal)
            || lower.StartsWith("api/admin", StringComparison.Ordinal)
            || lower.StartsWith("assets/", StringComparison.Ordinal)
            || lower == "favicon.ico";
    }
}
