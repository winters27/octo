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
public partial class SubsonicController : ControllerBase
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
    private readonly Octo.Services.ListenBrainz.ListenBrainzService? _listenBrainz;
    private readonly LastFmScrobbleService? _lastFmScrobbles;
    private readonly IOptionsMonitor<LastFmSettings> _lastFmSettingsOptions;
    private LastFmSettings _lastFmSettings => _lastFmSettingsOptions.CurrentValue;
    private readonly IOptionsMonitor<LibraryActionSettings> _libraryActionSettings;

    /// <summary>Optional so the controller still resolves where the library-action workers are
    /// not registered. Null means a rating simply never triggers anything.</summary>
    private readonly Octo.Services.Library.LibraryActionRatingWorker? _ratingActions;
    private readonly Octo.Services.Library.LibraryActionPlaylistProvisioner? _actionPlaylists;
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
    private readonly SyncCatalogService? _syncCatalog;
    private readonly Octo.Services.Lyrics.LyricsService? _lyricsService;
    private readonly IOptionsMonitor<MetadataSettings>? _metadataSettings;
    private readonly Octo.Services.Library.NoticeQueue? _noticeQueue;
    private readonly Octo.Services.Library.GeneratedPlaylistService? _generatedPlaylists;
    private readonly IOptionsMonitor<GeneratedPlaylistSettings>? _generatedSettings;
    private readonly AcquisitionTracker? _acquisitionTracker;
    private readonly Octo.Services.Lyrics.LyricsChoiceService? _lyricsChoices;
    private readonly Octo.Services.Library.LibraryActionExecutor? _libraryActions;
    private readonly Octo.Services.Library.UpgradeQueue? _upgradeQueue;
    private readonly DownloadConcurrency? _downloadConcurrency;
    private readonly IOptionsMonitor<SoulseekSettings>? _soulseekSettings;
    private readonly Octo.Services.Library.UpgradeSources? _upgradeSources;

    /// <summary>Whether a source Better quality searches (Soulseek, Lidarr) is set up here at all.</summary>
    private bool UpgradeReady => _upgradeSources?.Ready
        ?? (_soulseekSettings is null || Octo.Services.Library.UpgradeSources.SoulseekSetUp(_soulseekSettings.CurrentValue));

    /// <summary>Where Better quality looks, in words: "Soulseek", "Lidarr", or "Soulseek or Lidarr".</summary>
    private string UpgradeSourceName => _upgradeSources?.Name ?? "Soulseek";
    private readonly SearchSongOrderCache _searchSongOrders;
    private readonly RequestIdentity _requestIdentity;
    private readonly RecentScrobbles _recentScrobbles;
    private readonly CredentialCheck _credentialCheck;
    private readonly StarOnArrival? _starOnArrival;

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
        IOptionsMonitor<LibraryActionSettings> libraryActionSettings,
        PlaylistSyncService? playlistSyncService = null,
        Octo.Services.Library.LibraryActionRatingWorker? ratingActions = null,
        Octo.Services.Library.LibraryActionPlaylistProvisioner? actionPlaylists = null,
        LastFmService? lastFmService = null,
        CoverArtService? coverArtService = null,
        CoverArtAggregator? coverArtAggregator = null,
        LastFmRadioStateStore? radioStateStore = null,
        LastFmRadioRefreshQueue? radioRefreshQueue = null,
        Octo.Services.ListenBrainz.ListenBrainzService? listenBrainz = null,
        SyncCatalogService? syncCatalog = null,
        Octo.Services.Lyrics.LyricsService? lyricsService = null,
        IOptionsMonitor<MetadataSettings>? metadataSettings = null,
        Octo.Services.Library.NoticeQueue? noticeQueue = null,
        Octo.Services.Library.GeneratedPlaylistService? generatedPlaylists = null,
        IOptionsMonitor<GeneratedPlaylistSettings>? generatedSettings = null,
        AcquisitionTracker? acquisitionTracker = null,
        Octo.Services.Lyrics.LyricsChoiceService? lyricsChoices = null,
        Octo.Services.Library.LibraryActionExecutor? libraryActions = null,
        SearchSongOrderCache? searchSongOrders = null,
        LastFmScrobbleService? lastFmScrobbles = null,
        RequestIdentity? requestIdentity = null,
        RecentScrobbles? recentScrobbles = null, CredentialCheck? credentialCheck = null,
        StarOnArrival? starOnArrival = null,
        Octo.Services.Library.UpgradeQueue? upgradeQueue = null,
        DownloadConcurrency? downloadConcurrency = null,
        IOptionsMonitor<SoulseekSettings>? soulseekSettings = null,
        Octo.Services.Library.UpgradeSources? upgradeSources = null)
    {
        _soulseekSettings = soulseekSettings;
        _upgradeSources = upgradeSources;
        _upgradeQueue = upgradeQueue;
        _downloadConcurrency = downloadConcurrency;
        _starOnArrival = starOnArrival;
        _recentScrobbles = recentScrobbles ?? new RecentScrobbles();
        _lastFmScrobbles = lastFmScrobbles;
        _requestIdentity = requestIdentity
            ?? new RequestIdentity(Microsoft.Extensions.Logging.Abstractions.NullLogger<RequestIdentity>.Instance);
        _credentialCheck = credentialCheck
            ?? new CredentialCheck(Microsoft.Extensions.Logging.Abstractions.NullLogger<CredentialCheck>.Instance);
        _libraryActions = libraryActions;
        _searchSongOrders = searchSongOrders ?? new SearchSongOrderCache();
        _acquisitionTracker = acquisitionTracker;
        _lyricsChoices = lyricsChoices;
        _generatedPlaylists = generatedPlaylists;
        _generatedSettings = generatedSettings;
        _listenBrainz = listenBrainz;
        _syncCatalog = syncCatalog;
        _lyricsService = lyricsService;
        _metadataSettings = metadataSettings;
        _noticeQueue = noticeQueue;
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
        _libraryActionSettings = libraryActionSettings;
        _ratingActions = ratingActions;
        _actionPlaylists = actionPlaylists;
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

        // Navidrome answered ok above, so `u` is authenticated. Ensure this user's action
        // playlists exist, using the body we already have so the common case costs no extra
        // request. Fire-and-forget: creating a playlist must never delay the listing.
        if (_actionPlaylists is not null)
            _ = _actionPlaylists.EnsureAsync(username, PlaylistNames(relay.Body, format), parameters);

        await BootstrapRadioProfileAsync(username, parameters);
        var stations = PlaylistStations(username);
        QueueRefreshIfStale(username);
        var mixSettings = _generatedSettings?.CurrentValue;
        var generated = _generatedPlaylists is not null && mixSettings is { Enabled: true }
            ? await _generatedPlaylists.ListAsync(username, parameters)
            : [];
        if (stations.Count == 0 && generated.Count == 0)
            return File(relay.Body, relay.ContentType ?? $"application/{format}");
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
                foreach (var mix in generated)
                    rows.Add(JsonSerializer.SerializeToNode(_responseBuilder.GeneratedPlaylistFields(mix, mixSettings!)));
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
            foreach (var mix in generated)
                playlistsElement.Add(new XElement(ns + "playlist",
                    _responseBuilder.GeneratedPlaylistFields(mix, mixSettings!).Select(pair =>
                        new XAttribute(pair.Key, XmlValue(pair.Value)))));
            return File(Encoding.UTF8.GetBytes(document.ToString()), "application/xml");
        }
        catch (Exception ex)
        {
            _logger.LogWarning(ex, "Could not merge Radio stations and mixes into getPlaylists");
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

        // A mix is the listener's own library, served as a playlist Navidrome has never heard of.
        // The ping is the auth check, as for a station: nothing about a user is revealed first.
        if (_generatedPlaylists?.Find(username, id) is { } mix && _generatedSettings is not null)
        {
            var authOnly = parameters.ToDictionary(pair => pair.Key, pair => pair.Value);
            authOnly.Remove("id");
            var check = await _proxyService.RelaySafeAsync("rest/ping", authOnly);
            if (!check.Success || check.Body is null || !IsSuccessfulSubsonicResponse(check.Body, format))
                return _responseBuilder.CreateError(format, 40, "Wrong username or password");
            var entries = await _generatedPlaylists.MaterializeAsync(username, mix, parameters, HttpContext.RequestAborted);
            return _responseBuilder.CreateGeneratedPlaylistResponse(format, mix, _generatedSettings.CurrentValue, entries);
        }

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
        if (_generatedPlaylists is not null)
            songs = (await _generatedPlaylists.BlendIntoDiscoveryAsync(username, station, songs, parameters,
                HttpContext.RequestAborted)).ToList();

        // Before the catalog swap below: these rows are this response's own, while a catalog
        // row is shared with every other response that serves it.
        _metadataService.CompleteSongLengths(songs);

        // A song this user's sync catalog also holds goes out as the catalog describes it:
        // same album, and filed under the library's own artist where there is one. A syncing
        // client stores whichever description it read last, so the two must not disagree.
        if (_syncCatalog is not null)
            songs = songs.Select(song =>
                !song.IsLocal && _syncCatalog.TryGetSong(username, song.Id, out var synced) ? synced : song).ToList();
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
            var preparation = _radioStreams.PrepareForPublicationAsync(
                session, HttpContext.RequestAborted);

            // A cold starter is a YouTube fetch plus a transcode, tens of seconds on a
            // small box, and many clients give up on a list request well before that.
            // Answer inside the bound instead. The cache produces the track under its
            // own single-flight regardless of who is still waiting, so the station is
            // simply on the next refresh; the warmer below keeps its runway filling.
            var bound = _lastFmSettings.EffectiveStarterPublishTimeout;
            if (bound is { } limit
                && await Task.WhenAny(preparation, Task.Delay(limit)) != preparation)
            {
                _ = preparation.ContinueWith(
                    task => _ = task.Exception, TaskContinuationOptions.OnlyOnFaulted);
                _radioStreamSessions.Remove(token);
                _radioStreams.WarmReadyPool(session);
                _logger.LogInformation(
                    "Continuous Radio starter for {Station} not ready within {Seconds}s; " +
                    "publishing on the next refresh",
                    station.Name, (int)limit.TotalSeconds);
                return null;
            }

            var readyPool = await preparation;
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
        if (IsOctoPlaylistId(id))
            return _responseBuilder.CreateError(format, 70, "Octo's generated playlists are read-only");
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
        if (IsOctoPlaylistId(id))
            return _responseBuilder.CreateError(format, 70, "Octo's generated playlists are read-only");
        var endpoint = Request.Path.Value?.Split('/').LastOrDefault()?.Replace(".view", "") ?? "updatePlaylist";
        var relay = await _proxyService.RelaySafeAsync("rest/" + endpoint, parameters);
        return relay.Success && relay.Body is not null
            ? File(relay.Body, relay.ContentType ?? $"application/{format}")
            : _responseBuilder.CreateError(format, 0, "Unable to update playlist");
    }

    /// <summary>
    /// The playlist names in a relayed getPlaylists body, so provisioning can tell what is
    /// already there without a second request. Best-effort: an unreadable body simply means
    /// nothing is known to exist, and creating a duplicate is refused by Navidrome anyway.
    /// </summary>
    internal static IReadOnlyCollection<string> PlaylistNames(byte[]? body, string format)
    {
        if (body is not { Length: > 0 }) return [];
        try
        {
            // XML too: Navidrome does not refuse a second playlist with the same name, so a client
            // that asks for XML used to get every action playlist created again on each boot.
            if (!format.Equals("json", StringComparison.OrdinalIgnoreCase))
                return XDocument.Parse(Encoding.UTF8.GetString(body)).Descendants()
                    .Where(element => element.Name.LocalName == "playlist")
                    .Select(element => element.Attribute("name")?.Value)
                    .Where(name => !string.IsNullOrEmpty(name))
                    .Select(name => name!)
                    .ToList();
            var rows = JsonNode.Parse(body)?["subsonic-response"]?["playlists"]?["playlist"];
            if (rows is not JsonArray array) return [];
            return array.Select(row => row?["name"]?.GetValue<string>())
                .Where(name => !string.IsNullOrEmpty(name))
                .Select(name => name!)
                .ToList();
        }
        catch { return []; }
    }

    private List<LastFmRadioStation> VisibleStations(string username)
    {
        if (_radioStateStore is null || !_lastFmSettings.EnableRadio || username.Length == 0) return [];
        return _radioStateStore.GetUser(username).Stations.Where(station =>
            station.Personalized ? PersonalizedStationVisible(station)
                : _lastFmSettings.EnableDiscoveryStations).ToList();
    }

    /// <summary>
    /// Read-time half of the per-type station settings. The build gate decides what gets
    /// made; this decides what a client sees, so switching a type off takes effect on the
    /// next request instead of waiting for a rebuild. Same two-layer arrangement
    /// EnablePersonalizedStations already uses.
    /// </summary>
    private bool PersonalizedStationVisible(LastFmRadioStation station)
    {
        if (!_lastFmSettings.EnablePersonalizedStations) return false;
        return station.Kind switch
        {
            LastFmRadioStationKind.Starter or LastFmRadioStationKind.YourMix
                => _lastFmSettings.EnableYourMix,
            LastFmRadioStationKind.Discovery => _lastFmSettings.EnableDiscoveryMix,
            LastFmRadioStationKind.Artist => _lastFmSettings.EffectiveArtistStationCount > 0,
            LastFmRadioStationKind.Genre => _lastFmSettings.EffectiveGenreStationCount > 0,
            _ => true,
        };
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

        // Page one builds the discovery rows and remembers what it showed. A later page
        // carries on from that instead of going to Navidrome at the same offset, which
        // made every outside song past page one unreachable. See SearchLaterSongPageAsync.
        var songOffset = int.TryParse(parameters.GetValueOrDefault("songOffset", "0"), out var so) ? so : 0;

        // A client that copies the library to the device walks it with an empty query and
        // never searches the server, so the walk is the only place discovery can reach it.
        // Scoped to the whole library: a walk of one music folder is left exactly as it was.
        if (string.IsNullOrWhiteSpace(cleanQuery) && _syncCatalog is not null
            && !parameters.ContainsKey("musicFolderId")
            && SyncCatalogService.IsSyncClient(_subsonicSettings, parameters.GetValueOrDefault("c")))
            return await SyncWalkPageAsync(parameters, searchEndpoint, envelope, format);

        if (!string.IsNullOrWhiteSpace(cleanQuery) && songOffset > 0
            && await SearchLaterSongPageAsync(parameters, cleanQuery, songOffset, searchEndpoint, envelope, format)
                is { } laterPage)
            return laterPage;

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
        var (localSongTarget, externalTarget) =
            SearchBudget.Compute(requestedSongs, _subsonicSettings.EnableSearchDiscovery);

        // A client that asked for a handful of songs is searching as the user types. The
        // song side already costs nothing there (the budget leaves no room for discovery),
        // but external album search was still firing a Deezer query per keystroke. Judged
        // from the song count only when the client actually asked for songs, so a genuine
        // album-only search still gets album discovery.
        var isTypeAheadProbe = requestedSongs > 0 && externalTarget == 0;

        // Outside albums, artists and playlists are a fixed handful that all fit on the
        // first page of their own list. Adding them again to a later album or artist page
        // repeated the same suggestions on every page the client scrolled to.
        var albumOffset = int.TryParse(parameters.GetValueOrDefault("albumOffset", "0"), out var ao) ? ao : 0;
        var artistOffset = int.TryParse(parameters.GetValueOrDefault("artistOffset", "0"), out var aro) ? aro : 0;

        // Album discovery runs concurrently with the song fan-out below so it costs no
        // serial latency. It needs no Last.fm key (Deezer's catalog is keyless), so albums
        // still appear for a user who has not set one up.
        var albumTask = requestedAlbums > 0 && albumOffset <= 0 && !isTypeAheadProbe && _subsonicSettings.EnableSearchDiscovery
            ? _externalSearch.GetAlbumsAsync(cleanQuery, Math.Min(requestedAlbums, 20))
            : Task.FromResult<IReadOnlyList<Album>>(new List<Album>());

        // Artists the same way, and for the same reason: the merge has always known how to
        // fold external artists in and dedupe them against local ones, but nothing ever
        // gave it any, so the artist column of every search showed only what the library
        // already had. Keyless like albums, so it works without a Last.fm key.
        // What the library already holds of those albums, from their songs, started as soon as
        // they are found so it runs alongside the song build rather than after it.
        var ownershipTask = JudgeAlbumsAsync(albumTask);

        var artistTask = requestedArtists > 0 && artistOffset <= 0 && !isTypeAheadProbe && _subsonicSettings.EnableSearchDiscovery
            ? _metadataService.SearchArtistsAsync(cleanQuery, Math.Min(requestedArtists, 20))
            : Task.FromResult(new List<Artist>());

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
        var externalSlice = SearchSongOrder.PageOneExternalCount(
            built.Count, localSongTarget, externalTarget, localParsed.Songs.Count);
        var externalSongs = built.Take(externalSlice).ToList();

        // Remember what this page showed so the next page can carry on from it. Only when
        // discovery was part of the answer (a type-ahead page has none to continue) and
        // the library answered, since a failed relay would record an empty library.
        // Filed under who asked; a request Octo cannot name (an API key Navidrome would not
        // vouch for) is not remembered at all, so it can never land in someone else's slot.
        if (externalTarget > 0 && localResult.Success
            && await SongOrderKeyAsync(parameters, searchEndpoint, cleanQuery) is { } orderKey)
        {
            _searchSongOrders.Set(orderKey,
                SearchSongOrder.From(built, requestedSongs, localSongTarget, externalTarget, localParsed.Songs));
        }

        var playlistTask = _subsonicSettings.EnableExternalPlaylists && albumOffset <= 0
            ? await _metadataService.SearchPlaylistsAsync(cleanQuery, requestedAlbums)
            : new List<ExternalPlaylist>();

        // Degrade to no albums rather than failing the whole search if Deezer is slow,
        // throttled or unreachable.
        IReadOnlyList<Album> externalAlbums;
        try { externalAlbums = await albumTask; }
        catch (Exception ex)
        {
            _logger.LogDebug("external album search failed for '{Q}': {M}", cleanQuery, ex.Message);
            externalAlbums = new List<Album>();
        }

        List<Artist> externalArtists;
        try { externalArtists = await artistTask; }
        catch (Exception ex)
        {
            _logger.LogDebug("external artist search failed for '{Q}': {M}", cleanQuery, ex.Message);
            externalArtists = new List<Artist>();
        }

        // An album the library holds whole, or by its very name, is listed as the library's
        // album; one it holds in part says how much. search2 lists folders, not albums, so
        // nothing is added there and its outside albums stay, counted.
        var (settledAlbums, addedAlbums) = await SettleSearchAlbumsAsync(externalAlbums, await ownershipTask,
            localParsed.Albums, localResult.ContentType, parameters, canAdd: !isSearch2);
        localParsed.Albums.AddRange(addedAlbums);

        var externalResult = new SearchResult
        {
            Songs = externalSongs,
            Albums = settledAlbums,
            Artists = externalArtists,
        };

        // Track this response as a "queue" so a later scrobble for any of its
        // songs can drive the sliding-window prewarm of upcoming externals.
        // Order matches the merged response order — local first, external after.
        var localSongIds = ExtractLocalSongIds(localResult.Body, localResult.ContentType);
        _radioQueueStore.Register(localSongIds.Concat(externalSongs.Select(s => s.Id)));

        return MergeSearchResults(localParsed, localResult.ContentType, externalResult, playlistTask, format, envelope);
    }

    /// <summary>
    /// Where a search's order is kept: per user, so null when the request names nobody Octo can
    /// vouch for. An API key sign-in carries no <c>u</c>; before this every such user shared
    /// the one empty-name slot and could be handed another's order.
    /// </summary>
    private async Task<string?> SongOrderKeyAsync(Dictionary<string, string> parameters, string searchEndpoint,
        string cleanQuery) =>
        await _requestIdentity.UsernameAsync(parameters, _proxyService, HttpContext.RequestAborted) is { } user
            ? SearchSongOrderCache.Key(user, parameters.GetValueOrDefault("c", ""), searchEndpoint,
                parameters.GetValueOrDefault("musicFolderId"), cleanQuery)
            : null;

    /// <summary>
    /// A later page of a search's songs: the next stretch of the order page one started
    /// (its library rows, its outside rows, then the rest of the library), so paging never
    /// repeats or skips a row. See <see cref="SearchSongPagePlanner"/>.
    ///
    /// Null when the page should go to Navidrome unchanged, as every later page used to:
    /// discovery is off, or nothing is remembered for this search and the request is too
    /// small to have earned discovery on its own (a type-ahead count).
    /// </summary>
    private async Task<IActionResult?> SearchLaterSongPageAsync(Dictionary<string, string> parameters,
        string cleanQuery, int songOffset, string searchEndpoint, string envelope, string format)
    {
        if (!_subsonicSettings.EnableSearchDiscovery) return null;

        var requestedSongs = int.TryParse(parameters.GetValueOrDefault("songCount", "20"), out var sc) ? sc : 20;
        // With nobody to file it under, nothing is read or kept: every later page is rebuilt.
        // For an API key sign-in the tokenInfo call that names the user is made with the
        // request's own key, so it is also the credential check this page has not yet had.
        var key = await SongOrderKeyAsync(parameters, searchEndpoint, cleanQuery);
        var order = key is null ? null : _searchSongOrders.Get(key, requestedSongs, songOffset);
        if (order is null)
        {
            // Nothing remembered: expired, or Octo restarted since page one. Build the order
            // again as if page one had asked for this page's count. The build is shared with
            // any page one still running for the query, so a client that asks for two pages
            // at once gets one build.
            var (localTarget, externalTarget) = SearchBudget.Compute(requestedSongs);
            if (externalTarget == 0) return null;

            var builtTask = _externalSearch.GetAsync(cleanQuery);
            var prefix = await _proxyService.RelaySafeAsync(searchEndpoint, new Dictionary<string, string>(parameters)
            {
                ["songOffset"] = "0", ["songCount"] = localTarget.ToString(),
                ["albumCount"] = "0", ["artistCount"] = "0",
            });
            if (IsFailedSubsonicBody(prefix.Body, prefix.ContentType))
                return File(prefix.Body!, prefix.ContentType ?? $"application/{format}");
            if (!prefix.Success || prefix.Body is null) return null;

            var prefixSongs = _modelMapper.ParseSearchResponse(prefix.Body, prefix.ContentType).Songs;
            order = SearchSongOrder.From(await builtTask, requestedSongs, localTarget, externalTarget, prefixSongs);
            if (key is not null) _searchSongOrders.Set(key, order);
            _logger.LogDebug("search '{Q}': page one's order was gone, rebuilt it for offset {Offset}",
                cleanQuery, songOffset);
        }

        var page = SearchSongPagePlanner.Plan(songOffset, requestedSongs, order);

        // One relay for the page's library rows and for the albums and artists, which page
        // exactly as they always have on a later page: Navidrome's, at the client's offsets.
        var localParams = new Dictionary<string, string>(parameters)
        {
            ["songOffset"] = page.LocalOffset.ToString(),
            ["songCount"] = (page.LeadingLocals + page.TrailingLocals).ToString(),
        };
        var localResult = await _proxyService.RelaySafeAsync(searchEndpoint, localParams);
        if (IsFailedSubsonicBody(localResult.Body, localResult.ContentType))
            return File(localResult.Body!, localResult.ContentType ?? $"application/{format}");
        // Navidrome did not answer. The page is not made from the order alone: without its
        // library rows it would be short, and the client, taking it as it came, would never
        // see those rows. It goes to Navidrome as every later page used to.
        if (!localResult.Success || localResult.Body is null) return null;

        var localParsed = _modelMapper.ParseSearchResponse(localResult.Body, localResult.ContentType);
        var leading = localParsed.Songs.Take(page.LeadingLocals).ToList();
        var trailing = localParsed.Songs.Skip(page.LeadingLocals).Take(page.TrailingLocals).ToList();

        // Page one's own outside rows keep their places even where page one left one out
        // because the library had it, so the rows after them do not shift.
        var externalSongs = order.Built
            .Skip(page.PageOneExternalSkip).Take(page.PageOneExternalTake)
            .Where(song => !SubsonicModelMapper.IsListed(song, order.PrefixKeys))
            .Concat(order.LaterExternals.Skip(page.LaterExternalSkip).Take(page.LaterExternalTake))
            .ToList();

        _logger.LogDebug(
            "search '{Q}' page at {Offset}+{Count}: {Leading} library, {External} outside, {Trailing} library",
            cleanQuery, songOffset, requestedSongs, leading.Count, externalSongs.Count, trailing.Count);

        var localSongIds = ExtractLocalSongIds(localResult.Body, localResult.ContentType);
        _radioQueueStore.Register(localSongIds.Take(leading.Count)
            .Concat(externalSongs.Select(s => s.Id))
            .Concat(localSongIds.Skip(leading.Count).Take(trailing.Count)));

        return MergeSearchResults((leading, localParsed.Albums, localParsed.Artists), localResult.ContentType,
            new SearchResult { Songs = externalSongs, Albums = new List<Album>(), Artists = new List<Artist>() },
            new List<ExternalPlaylist>(), format, envelope, trailing);
    }

    /// <summary>
    /// How long a sync page waits for the user's catalog to finish building before going
    /// out without it. Kept short because a page the client gives up on fails its whole
    /// sync, where a catalog that misses one sync is simply on the next.
    /// </summary>
    private static readonly TimeSpan SyncCatalogWait = TimeSpan.FromSeconds(15);

    /// <summary>
    /// One page of a sync walk: Navidrome's page, then, once the library has run out, the
    /// user's catalog rows that fill the rest of it. See <see cref="SyncCatalogService"/>.
    /// Each kind (artist, album, song) is paged on its own, since a client can walk them
    /// separately or together.
    /// </summary>
    private async Task<IActionResult> SyncWalkPageAsync(Dictionary<string, string> parameters,
        string searchEndpoint, string envelope, string format)
    {
        var relay = await _proxyService.RelaySafeAsync(searchEndpoint, parameters);
        if (!relay.Success || relay.Body is not { Length: > 0 })
            return _responseBuilder.CreateResponse(format, envelope, new { });
        IActionResult Unchanged() => File(relay.Body, relay.ContentType ?? $"application/{format}");
        if (IsFailedSubsonicBody(relay.Body, relay.ContentType)) return Unchanged();

        var username = parameters.GetValueOrDefault("u", "");
        var stations = VisibleStations(username);
        var rows = SyncCatalogResponse.CountRows(relay.Body, relay.ContentType, envelope);
        if (username.Length == 0 || rows is null) return Unchanged();

        int Number(string name, int fallback) =>
            int.TryParse(parameters.GetValueOrDefault(name, ""), out var value) ? Math.Max(0, value) : fallback;
        var pages = new[]
        {
            (Kind: SyncCatalogKind.Artist, Name: "artist", Returned: rows.Value.Artists),
            (Kind: SyncCatalogKind.Album, Name: "album", Returned: rows.Value.Albums),
            (Kind: SyncCatalogKind.Song, Name: "song", Returned: rows.Value.Songs),
        }.Select(page => (page.Kind, page.Name, page.Returned,
            Offset: Number(page.Name + "Offset", 0), Count: Number(page.Name + "Count", 20))).ToList();

        // The first page of a walk starts the build, so it has the whole library's worth of
        // pages to finish in before the walk reaches the catalog.
        if (pages.Any(page => page.Offset == 0 && page.Count > 0))
        {
            QueueRefreshIfStale(username);
            _syncCatalog!.Warm(username, stations, parameters);
        }
        if (stations.Count == 0) return Unchanged();

        async Task<bool?> Exists(SyncCatalogKind kind, int index)
        {
            // The client's own empty query, since servers disagree on which spelling of
            // "everything" they accept.
            var probe = new Dictionary<string, string>(parameters)
            {
                ["f"] = "json",
                ["artistCount"] = "0", ["albumCount"] = "0", ["songCount"] = "0",
                ["artistOffset"] = "0", ["albumOffset"] = "0", ["songOffset"] = "0",
            };
            var name = kind.ToString().ToLowerInvariant();
            probe[name + "Count"] = "1"; probe[name + "Offset"] = index.ToString();
            var result = await _proxyService.RelaySafeAsync(searchEndpoint, probe);
            if (!result.Success || result.Body is not { Length: > 0 }
                || IsFailedSubsonicBody(result.Body, result.ContentType)) return null;
            var counted = SyncCatalogResponse.CountRows(result.Body, result.ContentType ?? "application/json", envelope);
            if (counted is null) return null;
            return (kind switch
            {
                SyncCatalogKind.Artist => counted.Value.Artists,
                SyncCatalogKind.Album => counted.Value.Albums,
                _ => counted.Value.Songs,
            }) > 0;
        }

        IReadOnlyList<Artist> artists = [];
        IReadOnlyList<Album> albums = [];
        IReadOnlyList<Song> songs = [];
        SyncCatalog? used = null;
        foreach (var page in pages)
        {
            if (page.Count == 0 || page.Returned >= page.Count) continue;
            var localTotal = SyncCatalogService.LocalTotalFromPage(page.Offset, page.Returned)
                ?? await SyncCatalogService.ResolveLocalTotalAsync(page.Offset,
                    _syncCatalog!.RememberedLocalTotal(username, page.Kind), index => Exists(page.Kind, index));
            if (localTotal is null) continue;

            // A short page is only the end of the library if nothing follows it. A server that
            // caps the page size also answers short, and filling that page would make the
            // client skip the library rows the cap held back.
            if (page.Returned > 0 && await Exists(page.Kind, localTotal.Value) != false) continue;

            var (start, take) = SyncCatalogService.Window(page.Offset, page.Count, page.Returned, localTotal.Value);
            var catalog = start > 0 ? _syncCatalog!.PinnedCatalog(username, page.Kind) : null;
            if (catalog is null)
            {
                try
                {
                    catalog = await _syncCatalog!.GetAsync(username, stations, parameters)
                        .WaitAsync(SyncCatalogWait, HttpContext.RequestAborted);
                }
                catch (TimeoutException)
                {
                    _logger.LogInformation(
                        "Sync catalog for {User} still building; this sync ends at the library and the next one gets it",
                        username);
                    continue;
                }
            }
            _syncCatalog!.Remember(username, page.Kind, localTotal.Value, catalog);
            used = catalog;

            var slice = SyncCatalogService.Slice(catalog, page.Kind, start, take);
            if (slice.Artists.Count > 0) artists = slice.Artists;
            if (slice.Albums.Count > 0) albums = slice.Albums;
            if (slice.Songs.Count > 0) songs = slice.Songs;
        }

        if (used is null || artists.Count + albums.Count + songs.Count == 0) return Unchanged();
        _logger.LogInformation(
            "Sync walk for {User} ({Client}): added {Songs} songs, {Albums} albums, {Artists} artists after the library",
            username, parameters.GetValueOrDefault("c", ""), songs.Count, albums.Count, artists.Count);
        var body = SyncCatalogResponse.Append(relay.Body, relay.ContentType, envelope, _responseBuilder, used,
            artists, albums, songs);
        return File(body, relay.ContentType ?? $"application/{format}");
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

        // Navidrome checks the sign-in on everything relayed to it, but it never sees an outside
        // song, so without this anyone who can reach Octo could play through it with no account.
        if (await RefuseUnlessSignedInAsync(parameters, format) is { } refused) return refused;

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
            // Lossless-on-play remains an explicit opt-in. Normal playback starts no
            // acquisition unless DownloadOnPlay or LidarrAlbumOnPlay are on: owned ids
            // already went to Navidrome above, and missing ids stream from YouTube below.
            // Hearts are the normal permanent-copy gesture.
            // Only a request from the first byte is a play. Clients ask again with a later
            // Range on every seek and while buffering. A transcoded request (format,
            // maxBitRate) is still a play and counts.
            if (IsFirstByteRequest(Request.Method, Request.Headers.Range.ToString()))
            {
                var who = await SignedInUserAsync(parameters);
                _heartAcquisitions.QueuePlay(provider!, externalId!, RequesterFor(who),
                    clientId: id, owner: who);
            }
            if (_subsonicSettings.WaitForLosslessOnPlay)
            {
                var acquisition = _acquisitions.Enqueue(provider!, externalId!, isStar: false,
                    triggerAlbumDownload: false, forcePermanent: true,
                    requestedBy: RequesterFor(await SignedInUserAsync(parameters)));
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

            // Each says how much of it the library holds. None is left out: the page lists no
            // library album to stand in for one.
            albums = await SettleArtistAlbumsAsync(albums, []);
            
            return _responseBuilder.CreateArtistResponse(format, artist, albums);
        }

        // Merged from Navidrome's JSON whatever the client asked for, then answered in the
        // client's format: see CreateMergedResponse.
        var navidromeResult = await _proxyService.RelaySafeAsync("rest/getArtist", AsJson(parameters));
        
        if (!navidromeResult.Success || navidromeResult.Body == null)
        {
            return _responseBuilder.CreateError(format, 70, "Artist not found");
        }

        var navidromeContent = Encoding.UTF8.GetString(navidromeResult.Body);
        string artistName = "";
        string localArtistId = id; // Keep the local artist ID for merged albums
        var localAlbums = new List<object>();
        object? artistData = null;

        if (navidromeResult.ContentType?.Contains("json") == true)
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
            return await RelayAsAskedAsync("rest/getArtist", parameters, format, navidromeResult.Body, navidromeResult.ContentType);
        }

        var localAlbumTitles = localAlbums
            .OfType<Dictionary<string, object>>()
            .Select(dict => dict.TryGetValue("name", out var nameObj) ? nameObj?.ToString() : null)
            .OfType<string>()
            .ToList();

        // The first hit is not reliably this artist: a bigger act whose name contains this one
        // can come first. Of the few asked for, the one with this exact name is.
        var deezerArtists = (await _metadataService.SearchArtistsAsync(artistName, 5))
            .Where(found => SongIdentity.SameArtistName(found.Name, artistName))
            .ToList();
        var deezerAlbums = new List<Album>();
        
        if (deezerArtists.Count > 0)
        {
            var deezerArtist = deezerArtists[0];
            if (SongIdentity.SameArtistName(deezerArtist.Name, artistName))
            {
                // The provider must come from the artist found, as for albums: a hardcoded
                // "deezer" never matches the metadata service's name, so this was always empty.
                // The library's own albums go along, to tell two artists of one name apart.
                deezerAlbums = await _metadataService.GetArtistAlbumsAsync(deezerArtist.ExternalProvider!, deezerArtist.ExternalId!,
                    localAlbumTitles);
                
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

        // An owned album is one the library has by the matcher's key, so "Discovery" in the
        // library hides the catalog's "Discovery" however either is spelled or punctuated.
        var localAlbumNames = localAlbumTitles.Select(SongIdentity.Key).ToHashSet(StringComparer.Ordinal);

        // Each outside album says how much of it the library holds; one the page's own albums
        // hold whole, or by name, is left out.
        deezerAlbums = await SettleArtistAlbumsAsync(deezerAlbums, localAlbums);

        var mergedAlbums = localAlbums.ToList();
        foreach (var deezerAlbum in deezerAlbums)
        {
            if (!localAlbumNames.Contains(SongIdentity.Key(deezerAlbum.Title)))
            {
                mergedAlbums.Add(_responseBuilder.ConvertAlbumToJson(deezerAlbum));
            }
        }

        if (artistData is Dictionary<string, object> artistDict)
        {
            artistDict["album"] = mergedAlbums;
            artistDict["albumCount"] = mergedAlbums.Count;
        }

        return _responseBuilder.CreateMergedResponse(format, "artist", artistData);
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

            // The songs the library already holds go out as the library's own copies.
            if (await OutsideAlbumWithOwnedSongsAsync(album, parameters, format) is { } owned) return owned;

            return _responseBuilder.CreateAlbumResponse(format, album);
        }

        // Merged from Navidrome's JSON whatever the client asked for, then answered in the
        // client's format: see CreateMergedResponse.
        var navidromeResult = await _proxyService.RelaySafeAsync("rest/getAlbum", AsJson(parameters));
        
        if (!navidromeResult.Success || navidromeResult.Body == null)
        {
            return _responseBuilder.CreateError(format, 70, "Album not found");
        }

        var navidromeContent = Encoding.UTF8.GetString(navidromeResult.Body);
        string albumName = "";
        string artistName = "";
        var localSongs = new List<object>();
        object? albumData = null;

        if (navidromeResult.ContentType?.Contains("json") == true)
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
            return await RelayAsAskedAsync("rest/getAlbum", parameters, format, navidromeResult.Body, navidromeResult.ContentType);
        }

        var library = localSongs.Select(AlbumFillIn.FromSubsonic).ToList();

        // The first catalog album by this name that holds the library's songs, known by ISRC
        // or by title and length. A name alone can belong to another record: "Nightcore" by
        // "Nightcore" (octo-player#1).
        var searchQuery = $"{artistName} {albumName}";
        var deezerAlbums = await _metadataService.SearchAlbumsAsync(searchQuery, 5);
        Album? deezerAlbum = null;
        foreach (var candidate in AlbumFillIn.Candidates(deezerAlbums, artistName, albumName, AlbumFillIn.CountSongs(library)))
        {
            // The provider must come from the candidate. A hardcoded "deezer" never
            // matches the metadata service's provider name, so this always returned null.
            var detail = await _metadataService.GetAlbumAsync(candidate.ExternalProvider!, candidate.ExternalId!);
            if (detail is null || detail.Songs.Count == 0) continue;
            if (AlbumFillIn.Holds(library, detail.Songs))
            {
                deezerAlbum = detail;
                break;
            }
            _logger.LogDebug(
                "getAlbum '{Artist} - {Album}': catalog album {Id} shares the name but not the songs; not filled in from it",
                artistName, albumName, candidate.ExternalId);
        }

        if (deezerAlbum != null)
        {
            var mergedSongs = localSongs.ToList();
            foreach (var deezerSong in deezerAlbum.Songs)
            {
                if (!AlbumFillIn.Owned(library, deezerSong))
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

        return _responseBuilder.CreateMergedResponse(format, "album", albumData);
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
            if (_coverArtService is null) return ServePlaceholder(branded: false);
            var seeds = StationCoverSeeds(radioStation);
            var bytes = await _coverArtService.GetListCoverAsync(
                new ListCover(radioStation.Name, StationCoverLabel(radioStation), ListKinds.Radio,
                    _ => Task.FromResult(seeds), radioStation.Tracks.Count),
                RequestedCoverSize(parameters), HttpContext.RequestAborted);
            return File(bytes, "image/jpeg");
        }

        // A mix is the listener's own library, so its cover carries no Octo mark: the logo says
        // where a result came from, and this came from them.
        var coverUser = parameters.GetValueOrDefault("u", "");
        if (_generatedPlaylists?.Find(coverUser, id) is { } mixCover)
        {
            if (_coverArtService is null) return ServePlaceholder(branded: false);
            var bytes = await _coverArtService.GetListCoverAsync(
                new ListCover(mixCover.Name, mixCover.Label, ListKinds.Mix,
                    ct => MixCoverSeedsAsync(coverUser, mixCover, parameters, ct),
                    _generatedPlaylists.Drawn(coverUser, mixCover)?.Count),
                RequestedCoverSize(parameters), HttpContext.RequestAborted);
            return File(bytes, "image/jpeg");
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
        // The Octo app marks songs outside the library itself, so its covers come back
        // plain, and a missing one is a 404 it draws its own empty tile for. Every other
        // client keeps the badge, the only sign it gets that a song came from Octo's search.
        var plain = DrawsItsOwnMarks(parameters);
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
                    return plain ? NotFound() : ServePlaceholder();
                }

                var watermarked = plain ? raw : _coverArtService?.AddOctoBadge(raw) ?? raw;
                return File(watermarked, "image/jpeg");
            }
            catch (Exception ex)
            {
                _logger.LogWarning(ex, "Cover art pipeline failed for registry id {Id}", id);
                return plain ? NotFound() : ServePlaceholder();
            }
        }

        // Legacy "ext-album-{hash}" / "ext-artist-{hash}" ids that pre-date the
        // registry. We can't reverse-resolve them, but returning a 404 makes
        // Arpeggio drop the song, so serve the Octo placeholder instead.
        if (id.StartsWith("ext-album-", StringComparison.OrdinalIgnoreCase)
            || id.StartsWith("ext-artist-", StringComparison.OrdinalIgnoreCase))
        {
            return plain ? NotFound() : ServePlaceholder();
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
                    var watermarked = plain ? imageBytes : _coverArtService?.AddOctoBadge(imageBytes) ?? imageBytes;
                    return File(watermarked, "image/jpeg");
                }
            }
            return plain ? NotFound() : ServePlaceholder();
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

    /// <summary>The size a client asked a cover at, if it asked.</summary>
    private static int? RequestedCoverSize(IReadOnlyDictionary<string, string> parameters) =>
        int.TryParse(parameters.GetValueOrDefault("size", ""), out var size) && size > 0 ? size : null;

    /// <summary>A genre or pinned station's tag, which stands in for its colour when its songs give none.</summary>
    private static string? StationCoverLabel(LastFmRadioStation station) =>
        station.Kind is LastFmRadioStationKind.Genre or LastFmRadioStationKind.Pinned or LastFmRadioStationKind.Discovery
            ? station.Seeds.FirstOrDefault()
            : null;

    /// <summary>
    /// The songs whose covers colour a station's cover: its seed artists' songs first (the
    /// artist of an artist station, the top seeds of Your Mix), then its first songs, one per
    /// album, four at most. Looked up like any song's cover outside the library.
    /// </summary>
    internal IReadOnlyList<CoverSeed> StationCoverSeeds(LastFmRadioStation station)
    {
        if (_coverArtAggregator is not { } covers) return [];
        var seedArtists = station.Kind is LastFmRadioStationKind.Artist or LastFmRadioStationKind.YourMix or LastFmRadioStationKind.Starter
            ? station.Seeds.Take(3).Select(SongIdentity.Key).Where(key => key.Length > 0).ToList()
            : [];
        int Rank(LastFmRadioTrack track)
        {
            var index = seedArtists.IndexOf(SongIdentity.Key(track.Artist));
            return index < 0 ? seedArtists.Count : index;
        }
        return station.Tracks
            .Where(track => !string.IsNullOrWhiteSpace(track.Artist) && !string.IsNullOrWhiteSpace(track.Title))
            .Select((track, order) => (track, order))
            .OrderBy(pair => Rank(pair.track)).ThenBy(pair => pair.order)
            .Select(pair => pair.track)
            .DistinctBy(track => SongIdentity.Key(track.Artist) + "|" + SongIdentity.Key(track.Album ?? track.Title))
            .Take(4)
            .Select(track => new CoverSeed(
                $"song|{SongIdentity.Key(track.Artist)}|{SongIdentity.Key(track.Title)}",
                ct => covers.GetCoverAsync(new SoulseekRouting
                {
                    Kind = RoutingKind.Song, Artist = track.Artist, Title = track.Title, Album = track.Album,
                }, false, ct)))
            .ToList();
    }

    /// <summary>
    /// The songs whose covers colour a mix's cover: the first of this period's draw, one per
    /// album cover, four at most, read from Navidrome as the listener. A period's first draw is
    /// made only for a caller Navidrome accepts, as opening the mix would.
    /// </summary>
    private async Task<IReadOnlyList<CoverSeed>> MixCoverSeedsAsync(string username,
        Octo.Services.Library.GeneratedPlaylist mix, Dictionary<string, string> parameters, CancellationToken ct)
    {
        if (_generatedPlaylists is null) return [];
        var auth = parameters.Where(pair => pair.Key is not ("id" or "size")).ToDictionary(pair => pair.Key, pair => pair.Value);
        var songs = _generatedPlaylists.Drawn(username, mix);
        if (songs is null)
        {
            var ping = await _proxyService.RelaySafeAsync("rest/ping", auth);
            if (!ping.Success || ping.Body is null || !IsSuccessfulSubsonicResponse(ping.Body, auth.GetValueOrDefault("f", "xml")))
                return [];
            // Not cancelled with the cover: a draw that finishes late still serves the next request.
            songs = await _generatedPlaylists.MaterializeAsync(username, mix, parameters, CancellationToken.None).WaitAsync(ct);
        }
        return songs
            .Select(song => song["coverArt"]?.ToString())
            .Where(cover => !string.IsNullOrEmpty(cover))
            .Distinct(StringComparer.Ordinal)
            .Take(4)
            .Select(cover => new CoverSeed("navidrome|" + cover, async _ =>
            {
                var picture = await _proxyService.RelayAsync("rest/getCoverArt",
                    new Dictionary<string, string>(auth) { ["id"] = cover!, ["size"] = "128" });
                return picture.ContentType?.StartsWith("image/", StringComparison.OrdinalIgnoreCase) == true ? picture.Body : null;
            }))
            .ToList();
    }

    /// <summary>Whether the request comes from the Octo app, which shows on its own
    /// artwork that a song is not in the library, so it wants covers without the badge.
    /// Told apart by the Subsonic client name it sends on every call.</summary>
    internal static bool DrawsItsOwnMarks(IReadOnlyDictionary<string, string> parameters) =>
        string.Equals(parameters.GetValueOrDefault("c", "")?.Trim(), "Octo", StringComparison.OrdinalIgnoreCase);

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

    /// <summary>The same request, asking Navidrome for JSON, which is what the merges read.</summary>
    private static Dictionary<string, string> AsJson(Dictionary<string, string> parameters) =>
        new(parameters) { ["f"] = "json" };

    /// <summary>
    /// Navidrome's answer untouched, when there is nothing to merge: the JSON already in hand
    /// for a JSON client, otherwise asked again in the client's own format so an XML client
    /// never gets JSON.
    /// </summary>
    private async Task<IActionResult> RelayAsAskedAsync(
        string endpoint, Dictionary<string, string> parameters, string format, byte[] jsonBody, string? jsonContentType)
    {
        if (format == "json")
            return File(jsonBody, jsonContentType ?? "application/json");
        var asked = await _proxyService.RelaySafeAsync(endpoint, parameters);
        if (!asked.Success || asked.Body == null)
            return _responseBuilder.CreateError(format, 70, "Not found");
        return File(asked.Body, asked.ContentType ?? "application/xml");
    }

    private IActionResult MergeSearchResults(
        (List<object> Songs, List<object> Albums, List<object> Artists) local,
        string? localContentType,
        SearchResult externalResult,
        List<ExternalPlaylist> playlistResult,
        string format,
        string envelope,
        List<object>? trailingLocalSongs = null)
    {
        var (localSongs, localAlbums, localArtists) = local;

        var isJson = format == "json" || localContentType?.Contains("json") == true;
        var (mergedSongs, mergedAlbums, mergedArtists) = _modelMapper.MergeSearchResults(
            localSongs,
            localAlbums,
            localArtists,
            externalResult,
            playlistResult,
            isJson,
            trailingLocalSongs);

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
    /// Stars (favorites) an item. For playlists, triggers download. For external songs and
    /// albums, triggers a download and, outside Octo's own apps, favorites it once it arrives.
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

            // Navidrome never sees this id, so nothing else would check who is asking.
            if (await RefuseUnlessSignedInAsync(parameters, format) is { } refused) return refused;

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
            // Navidrome never sees this id, so nothing else would check who is asking.
            if (await RefuseUnlessSignedInAsync(parameters, format) is { } refused) return refused;

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
            //
            // The progress list is claimed first, so the chain's first step already has a row
            // to move. Its name is the one the request signed in as (for an API key, its owner
            // as Navidrome names it), not RequesterFor: it decides who may see the row, and it
            // is never written anywhere.
            var who = await SignedInUserAsync(parameters);
            // Held before the download is queued, so one that finishes at once still finds it.
            if (FavoriteCredential(parameters) is { } credential)
                _starOnArrival!.HoldAlbum(albumProviderName, albumCandidate, credential, who);
            _acquisitionTracker?.BeginAlbum(albumProviderName, albumCandidate, who);
            _heartAcquisitions.QueueAlbum(albumProviderName, albumCandidate, RequesterFor(who));

            // Navidrome has never seen this id, so relaying the star would just error.
            return _responseBuilder.CreateResponse(format, "starred", new { });
        }

        // Check if this is an external song (enables download-on-star)
        var (isExternal, provider, externalId) = _localLibraryService.ParseSongId(itemId);
        
        if (isExternal && _subsonicSettings.EffectiveHeartDownloadSources()
                .Any(step => step.SongEnabled == true))
        {
            // Navidrome never sees this id, so nothing else would check who is asking.
            if (await RefuseUnlessSignedInAsync(parameters, format) is { } refused) return refused;

            // No storage-mode gate any more. It used to exclude Permanent on the grounds
            // that playing a track there already downloads it, but that was only ever true
            // through the blocking play path — so in Permanent mode a star fell through to
            // the relay and errored on an id Navidrome has never seen.
            //
            // Stars ride the queue's own channel: explicit user intent is never shed under
            // load the way a play is, and the request carries the album-walk flag rather
            // than inheriting whatever a concurrent play happened to ask for.
            _logger.LogInformation("Starring external song {SongId}, queueing permanent download", itemId);

            // Keyed by what the pipeline knows, labelled with the id the client starred so the
            // app can find its row. Named from the routing, which is already in memory.
            var who = await SignedInUserAsync(parameters);
            // Held before the download is queued, so one that finishes at once still finds it.
            if (FavoriteCredential(parameters) is { } credential)
                _starOnArrival!.HoldSong(provider!, externalId!, credential, who);
            var routing = _idRegistry.Lookup(externalId!);
            _acquisitionTracker?.Begin(provider!, externalId!, itemId, who,
                routing?.Artist, routing?.Title, routing?.Album);
            _heartAcquisitions.QueueTrack(provider!, externalId!, RequesterFor(who));

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
    /// The caller's own hearted downloads, while they run and for half an hour after, so the
    /// app can draw progress on the button that started one. Octo answers this itself, so it
    /// works wherever the Subsonic API does, including away from home, unlike the admin API.
    ///
    /// Credentials are checked the way a station or a mix checks them: a ping to Navidrome with
    /// the caller's own. Only rows that user asked for come back, admin or not.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/getAcquisitions")]
    [Route("rest/getAcquisitions.view")]
    public async Task<IActionResult> GetAcquisitions()
    {
        var parameters = await ExtractAllParameters();
        const string format = "json";

        var auth = parameters.ToDictionary(pair => pair.Key, pair => pair.Value);
        auth["f"] = format;
        var check = await _proxyService.RelaySafeAsync("rest/ping", auth);
        if (!check.Success || check.Body is null)
            return _responseBuilder.CreateError(format, 0, "Octo can't reach Navidrome to check who is asking");
        if (!IsSuccessfulSubsonicResponse(check.Body, format))
            return _responseBuilder.CreateError(format, 40, "Wrong username or password");

        var username = await SignedInUserAsync(parameters);
        var rows = _acquisitionTracker is not null && !string.IsNullOrWhiteSpace(username)
            ? _acquisitionTracker.ForUser(username)
            : [];
        return _responseBuilder.CreateAcquisitionsResponse(rows);
    }

    /// <summary>
    /// Navidrome's extension list with octoAcquisitions added, so a client can tell this server
    /// answers getAcquisitions before it asks, octoLibraryActions while library actions are
    /// on, and octoTopSongs while search discovery is. Relayed, then merged; no credentials are needed, as the OpenSubsonic spec has it.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/getOpenSubsonicExtensions")]
    [Route("rest/getOpenSubsonicExtensions.view")]
    public async Task<IActionResult> GetOpenSubsonicExtensions()
    {
        var parameters = await ExtractAllParameters();
        var format = parameters.GetValueOrDefault("f", "xml");
        var relay = await _proxyService.RelaySafeAsync("rest/getOpenSubsonicExtensions", parameters);
        return _responseBuilder.MergeOpenSubsonicExtensions(format,
            relay.Success ? relay.Body : null, relay.ContentType,
            lyricsChoices: _lyricsChoices is not null && _metadataSettings?.CurrentValue.FetchLyrics == true,
            libraryActions: _libraryActions is not null && _libraryActionSettings.CurrentValue.Enabled,
            topSongs: _subsonicSettings.EnableSearchDiscovery);
    }

    /// <summary>
    /// octoLibraryActions v1: what the caller may do to library files from the app, read from the
    /// settings as they are now. Always JSON. Credentials are checked with a ping to Navidrome, as
    /// getAcquisitions checks them.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/getLibraryActions")]
    [Route("rest/getLibraryActions.view")]
    public async Task<IActionResult> GetLibraryActions()
    {
        var parameters = await ExtractAllParameters();
        if (await CheckCallerAsync(parameters) is { } refused) return refused;
        var edits = HttpContext.RequestServices.GetService<Octo.Services.Library.LibraryEditService>();
        return _responseBuilder.CreateLibraryActionsResponse(_libraryActionSettings.CurrentValue,
            parameters.GetValueOrDefault("u"), _downloadConcurrency?.Current ?? 1, UpgradeReady, UpgradeSourceName,
            admin: await IsCallerAdminAsync(parameters), edits: edits is not null,
            covers: HttpContext.RequestServices.GetService<Octo.Services.CoverArt.IAlbumCoverFinder>() is not null);
    }

    /// <summary>
    /// octoLibraryActions v3: the songs the server holds in its trash, newest first, for an app to
    /// offer putting one back. Always JSON; credentials checked like getLibraryActions, and listed
    /// only to someone who could put them back.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/getLibraryTrash")]
    [Route("rest/getLibraryTrash.view")]
    public async Task<IActionResult> GetLibraryTrash()
    {
        var parameters = await ExtractAllParameters();
        if (await CheckCallerAsync(parameters) is { } refused) return refused;
        var edits = HttpContext.RequestServices.GetService<Octo.Services.Library.LibraryEditService>();
        var allowed = edits is not null
            && edits.Refusal(parameters.GetValueOrDefault("u"), await IsCallerAdminAsync(parameters)) is null;
        return _responseBuilder.CreateLibraryTrashResponse(allowed ? edits!.Trash() : [],
            _libraryActionSettings.CurrentValue.EffectiveQuarantineRetentionDays);
    }

    /// <summary>
    /// octoLibraryActions v2: the caller's upgrade jobs and how each is going, read by the apps while
    /// an upgrade they asked for is running. Always JSON; credentials checked like getLibraryActions.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/getUpgrades")]
    [Route("rest/getUpgrades.view")]
    public async Task<IActionResult> GetUpgrades()
    {
        var parameters = await ExtractAllParameters();
        if (await CheckCallerAsync(parameters) is { } refused) return refused;
        var username = parameters.GetValueOrDefault("u");
        var jobs = string.IsNullOrWhiteSpace(username) || _upgradeQueue is null
            ? []
            : _upgradeQueue.Snapshot(username);
        var progress = (_acquisitionTracker?.All() ?? [])
            .GroupBy(row => $"{row.Provider}:{row.ExternalId}")
            .ToDictionary(group => group.Key, group => group.First().Progress);
        return _responseBuilder.CreateUpgradesResponse(jobs.Select(job =>
            (job, job.AcquisitionKey is { } key && job.State == Octo.Services.Library.UpgradeStates.Working
                ? progress.GetValueOrDefault(key) : null)));
    }

    /// <summary>
    /// octoLibraryActions v1: remove one song, exactly as putting it in the Delete action playlist
    /// does. The executor applies every gate: the master switch, the allowlist, Delete being on,
    /// the dry run, and a file it can prove is this song, which goes to quarantine.
    ///
    /// Never through a rating: where rating actions are on, one star can remove a song, and this
    /// is how an app removes one without giving it a rating.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/libraryAction")]
    [Route("rest/libraryAction.view")]
    public async Task<IActionResult> ApplyLibraryAction()
    {
        var parameters = await ExtractAllParameters();
        const string format = "json";
        if (await CheckCallerAsync(parameters) is { } refused) return refused;

        var id = parameters.GetValueOrDefault("id", "").Trim();
        var action = parameters.GetValueOrDefault("action", "").Trim();
        if (id.Length == 0 || action.Length == 0)
            return _responseBuilder.CreateError(format, 10, "Required parameter is missing: id and action");
        if (action.Equals(SubsonicResponseBuilder.UpgradeAction, StringComparison.OrdinalIgnoreCase))
            return QueueUpgrade(id, parameters.GetValueOrDefault("u"));
        if (SubsonicResponseBuilder.EditActions.FirstOrDefault(a => a.Equals(action, StringComparison.OrdinalIgnoreCase)) is { } edit)
            return await ApplyLibraryEdit(id, edit, parameters);
        if (!action.Equals(SubsonicResponseBuilder.RemoveAction, StringComparison.OrdinalIgnoreCase))
            return _responseBuilder.CreateError(format, 0,
                $"Unknown action \"{action}\"; this server knows remove, upgrade, {string.Join(", ", SubsonicResponseBuilder.EditActions)}");

        // The name the ping just checked. An API key alone names nobody here, so it cannot be on
        // the allowlist, and the executor is not asked at all.
        var username = parameters.GetValueOrDefault("u");
        if (string.IsNullOrWhiteSpace(username))
            return _responseBuilder.CreateLibraryActionResponse(id, new Octo.Services.Library.LibraryActionOutcome(
                Octo.Services.Library.LibraryActionState.Skipped,
                "Sign in with a username to remove songs; an API key alone does not say who is asking."));
        if (_libraryActions is null)
            return _responseBuilder.CreateLibraryActionResponse(id, new Octo.Services.Library.LibraryActionOutcome(
                Octo.Services.Library.LibraryActionState.Skipped, "Library actions are off."));

        // Taking a file off the disk is for the server's admins, whatever the allowed list says.
        if (!await IsCallerAdminAsync(parameters))
            return _responseBuilder.CreateLibraryActionResponse(id, new Octo.Services.Library.LibraryActionOutcome(
                Octo.Services.Library.LibraryActionState.Skipped, "Only a server admin can remove songs from the library."));

        // Not tied to the request: a client that hangs up must not stop a move halfway.
        // copy=true: Library health removing a second copy, so the song itself is still wanted.
        var onlyACopy = parameters.GetValueOrDefault("copy", "") is "true" or "1";
        var outcome = await _libraryActions.ApplyAsync(
            new Octo.Services.Library.LibraryActionRequest(LibraryAction.Delete, id, username, OnlyACopy: onlyACopy),
            CancellationToken.None);
        _logger.LogInformation("Library action Delete for {Id} by {User} from the app: {State} - {Detail}",
            id, username, outcome.State, outcome.Detail);
        // Navidrome drops the song on its next scan; one is asked for now rather than whenever.
        if (outcome.State == Octo.Services.Library.LibraryActionState.Applied)
            HttpContext.RequestServices.GetService<Octo.Services.Library.LibraryRescan>()?.Soon();
        return _responseBuilder.CreateLibraryActionResponse(id, outcome);
    }

    /// <summary>
    /// octoLibraryActions v3: change a song's file without removing it, or put a removed one back.
    /// retag writes the tags sent (title, artist, album, albumArtist, year, genre, track, disc,
    /// isrc; an empty one clears it), joinAlbum puts the song on the album of the song named by
    /// <c>like</c>, cover puts a found cover inside a file that has none (<c>preview=true</c> only
    /// says what was found), lookup finds the tags a download would get and writes nothing, undo
    /// puts back the song's last change, and restore takes a removed song out of the trash. All of
    /// them only for an admin on the allowed list; a dry run rehearses.
    /// </summary>
    private async Task<IActionResult> ApplyLibraryEdit(string id, string action, IReadOnlyDictionary<string, string> parameters)
    {
        var username = parameters.GetValueOrDefault("u");
        var edits = HttpContext.RequestServices.GetService<Octo.Services.Library.LibraryEditService>();
        var refusal = edits is null ? "This server cannot change library files." : edits.Refusal(username, await IsCallerAdminAsync(parameters));
        if (refusal is not null)
            return action == SubsonicResponseBuilder.LookupAction
                ? _responseBuilder.CreateLookupResponse(id, new("skipped", refusal, new Dictionary<string, string?>(),
                    new Dictionary<string, string?>(), null, null, null))
                : _responseBuilder.CreateLibraryEditResponse(id, action, new("skipped", refusal));

        // Not tied to the request either: a write that has started finishes.
        var none = CancellationToken.None;
        Octo.Services.Library.LibraryEditOutcome outcome;
        switch (action)
        {
            case SubsonicResponseBuilder.LookupAction:
                return _responseBuilder.CreateLookupResponse(id, await edits!.LookupAsync(id, HttpContext.RequestAborted));
            case SubsonicResponseBuilder.RetagAction:
                var changes = Octo.Services.Library.SongTagFields.All
                    .Where(parameters.ContainsKey)
                    .ToDictionary(field => field, field => (string?)parameters[field]);
                outcome = await edits!.RetagAsync(id, username!, changes, none);
                break;
            case SubsonicResponseBuilder.JoinAlbumAction:
                var like = parameters.GetValueOrDefault("like", "").Trim();
                if (like.Length == 0) return _responseBuilder.CreateError("json", 10, "Required parameter is missing: like");
                outcome = await edits!.JoinAlbumAsync(id, like, username!, none);
                break;
            case SubsonicResponseBuilder.CoverAction:
                var preview = parameters.GetValueOrDefault("preview", "") is "true" or "1";
                outcome = await edits!.AddCoverAsync(id, username!, preview, none);
                break;
            case SubsonicResponseBuilder.UndoAction:
                outcome = await edits!.UndoAsync(id, username!, none);
                break;
            default:
                outcome = await edits!.RestoreAsync(id, username!);
                break;
        }
        _logger.LogInformation("Library {Action} for {Id} by {User} from the app: {State} - {Detail}",
            action, id, username, outcome.State, outcome.Detail);
        return _responseBuilder.CreateLibraryEditResponse(id, action, outcome);
    }

    /// <summary>
    /// Whether the caller is one of Navidrome's admins, asked with their own sign-in. False when
    /// Navidrome cannot say, so a change to the library's files is never allowed by an outage.
    /// Asked every time: a role taken away counts at once, and it is one call to Navidrome.
    /// </summary>
    private async Task<bool> IsCallerAdminAsync(IReadOnlyDictionary<string, string> parameters)
    {
        var username = parameters.GetValueOrDefault("u");
        if (string.IsNullOrWhiteSpace(username)) return false;
        var auth = parameters.ToDictionary(pair => pair.Key, pair => pair.Value);
        auth["f"] = "json";
        auth["username"] = username;
        var relay = await _proxyService.RelaySafeAsync("rest/getUser", auth);
        if (!relay.Success || relay.Body is not { } body) return false;
        var admin = false;
        try
        {
            using var doc = System.Text.Json.JsonDocument.Parse(body);
            admin = doc.RootElement.TryGetProperty("subsonic-response", out var root)
                    && root.TryGetProperty("user", out var user)
                    && user.TryGetProperty("adminRole", out var role)
                    && role.ValueKind == System.Text.Json.JsonValueKind.True;
        }
        catch (System.Text.Json.JsonException) { return false; }
        return admin;
    }

    /// <summary>
    /// octoLibraryActions v2: queue a higher quality copy of one song, and answer at once. The same
    /// gates as the Better quality playlist, read now so the app hears a reason straight away; the
    /// executor checks them all again when the job runs. A job is listed in getUpgrades before this
    /// answers "queued".
    /// </summary>
    private IActionResult QueueUpgrade(string id, string? username)
    {
        const string action = SubsonicResponseBuilder.UpgradeAction;
        var refusal = UpgradeGate.Refusal(_libraryActionSettings.CurrentValue, username,
            _libraryActions is not null && _upgradeQueue is not null, UpgradeReady, UpgradeSourceName);
        if (refusal is not null) return _responseBuilder.CreateLibraryActionResponse(id, "skipped", refusal, action);

        var (jobs, full) = _upgradeQueue!.Add([new Octo.Services.Library.UpgradeAsk(id)], username!, "app");
        if (jobs.Count == 0) return _responseBuilder.CreateLibraryActionResponse(id, "skipped", full, action);
        _logger.LogInformation("Higher quality for {Id} asked by {User} from the app: {State}", id, username, jobs[0].State);
        return _responseBuilder.CreateLibraryActionResponse(id, "queued",
            $"Looking for a higher quality copy on {UpgradeSourceName}.", action);
    }

    /// <summary>
    /// Star ratings.
    ///
    /// Two jobs, in this order and never the other way round:
    ///
    /// 1. ALWAYS relay to Navidrome and return ITS response verbatim. A rating the server did
    ///    not record snaps back in the client on the next refresh, and rating a track is a
    ///    legitimate thing to do for its own sake. Octo reading meaning into it must never cost
    ///    the user the rating itself.
    /// 2. Only then, if library actions are on, the rating maps to an enabled action and the
    ///    user is on the allowlist, queue it. Queue, not execute: nothing that removes a file
    ///    runs inside a request.
    ///
    /// Before this there was no handler at all and setRating fell through to the catch-all,
    /// where an Octo id became a synthetic OK that never reached Navidrome. That behaviour is
    /// preserved exactly.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/setRating")]
    [Route("rest/setRating.view")]
    public async Task<IActionResult> SetRating()
    {
        var parameters = await ExtractAllParameters();
        var format = parameters.GetValueOrDefault("f", "xml");
        var itemId = parameters.GetValueOrDefault("id", "");
        var ratingText = parameters.GetValueOrDefault("rating", "");

        // An external id is not a Navidrome song, and relaying one errors "data not found".
        if (!string.IsNullOrEmpty(itemId) && _localLibraryService.ParseSongId(itemId).isExternal)
            return _responseBuilder.CreateResponse(format, "setRating", new { });

        byte[] body;
        string? contentType;
        try
        {
            var result = await _proxyService.RelayAsync("rest/setRating", parameters);
            (body, contentType) = (result.Body, result.ContentType);
        }
        catch (HttpRequestException ex)
        {
            return _responseBuilder.CreateError(format, 0, $"Error connecting to Subsonic server: {ex.Message}");
        }

        if (_ratingActions is not null
            && IsSuccessfulSubsonicResponse(body, format)
            && int.TryParse(ratingText, out var rating)
            && !string.IsNullOrEmpty(itemId)
            && parameters.GetValueOrDefault("u") is { Length: > 0 } username
            && parameters.GetValueOrDefault("t") is { Length: > 0 } token
            && parameters.GetValueOrDefault("s") is { Length: > 0 } salt
            && _libraryActionSettings.CurrentValue.ActionForRating(rating) is { } definition
            && RatingInScope(username, itemId))
        {
            // Navidrome answered ok to a call carrying this u/t/s, which IS the auth check.
            // Octo does not validate the password itself; it trusts it exactly as far as
            // Navidrome just did, the same idiom getPlaylist and star already use.
            _ratingActions.TryEnqueue(new Octo.Services.Library.RatingActionRequest(
                definition.Action, itemId, username, username, token, salt));
        }

        return File(body, contentType ?? $"application/{format}");
    }

    /// <summary>
    /// Whether a star is a command here. NoticeOnly: only on a track in one of Octo's notice
    /// playlists, where the only reason to rate it is to answer (#47). Global: any track.
    /// </summary>
    private bool RatingInScope(string username, string itemId) =>
        _libraryActionSettings.CurrentValue.EffectiveRatingsScope == LibraryRatingScope.Global
        || (_noticeQueue?.IsQueued(username, itemId) ?? false);

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
        var resolvedSongs = LastFmRadioSpacing.Spread(
            (await Task.WhenAll(resolveTasks)).Where(s => s != null).Cast<Song>().ToList(),
            s => s.Artist, artistName);

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
    /// Library songs are relayed to Navidrome too, because real scrobbling
    /// (last-played stats, the Now Playing panel) is the upstream's job. Outside
    /// songs are not: Navidrome has no such media to record.
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

        // Library songs pass through so Navidrome's last-played/Now Playing stays accurate.
        // An outside id is not Navidrome's: relayed, it logs "data not found" on every play
        // and records nothing (#60). So only library ids go upstream, each keeping its
        // own submission and time when the client sent one per id.
        var library = Enumerable.Range(0, ids.Count)
            .Where(index => !_localLibraryService.ParseSongId(ids[index]).isExternal).ToList();
        // Times that do not pair up with the ids are dropped, not relayed. Navidrome refuses
        // such a scrobble outright, and leaving the outside ids out could make the counts
        // match by accident and pin an outside song's time on a library song.
        if (times.Count > 0 && times.Count != ids.Count) times = [];
        IEnumerable<string> LibraryOnly(IReadOnlyList<string> values) =>
            values.Count == ids.Count ? library.Select(index => values[index]) : values;
        try
        {
            var relayParameters = parameters
                .Where(pair => pair.Key is not ("id" or "submission" or "time")).ToList();
            if (ids.Count > 0 && library.Count == 0)
            {
                // Nothing for Navidrome to record, but its answer was also the credential
                // check that gates learning below. A ping checks the same credentials and
                // answers in the very shape a scrobble does, failures included.
                var check = await _proxyService.RelayAsync("rest/ping", relayParameters);
                if (IsSuccessfulSubsonicResponse(check.Body, format))
                    await LearnFromScrobblesAsync(ids, submissions, times, parameters);
                return File(check.Body, check.ContentType ?? $"application/{format}");
            }
            relayParameters.AddRange(LibraryOnly(ids).Select(value => new KeyValuePair<string, string>("id", value)));
            relayParameters.AddRange(LibraryOnly(submissions).Select(value => new KeyValuePair<string, string>("submission", value)));
            relayParameters.AddRange(LibraryOnly(times).Select(value => new KeyValuePair<string, string>("time", value)));
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

    /// <summary>
    /// What a completed scrobble teaches: the radio profile (when personalised radio is
    /// on), the listener's Last.fm history (outside songs, and library songs unless they are
    /// left to Navidrome) and, for an external track, ListenBrainz. A start-of-play event
    /// becomes Last.fm's Now Playing.
    /// </summary>
    private async Task LearnFromScrobblesAsync(IReadOnlyList<string> ids,
        IReadOnlyList<string> submissions, IReadOnlyList<string> times,
        IReadOnlyDictionary<string, string> authenticatedParameters)
    {
        // Called only once Navidrome has accepted these credentials. An API key sign-in has no
        // u, so its owner is asked of Navidrome; nobody to name means nothing is learned.
        var username = await _requestIdentity.UsernameAsync(authenticatedParameters, _proxyService,
            HttpContext.RequestAborted);
        if (string.IsNullOrEmpty(username)) return;
        var learning = _radioStateStore is not null && _lastFmSettings.EnableRadio
            && _lastFmSettings.EnablePersonalizedStations;
        var submitting = _listenBrainz is not null && _listenBrainz.IsEnabledFor(username);
        var scrobbling = _lastFmScrobbles is not null && _lastFmScrobbles.IsEnabledFor(username);
        if (!learning && !submitting && !scrobbling) return;
        var recorded = false;
        for (var index = 0; index < ids.Count; index++)
        {
            // Explicit start/now-playing scrobbles are not completed plays. Clients that
            // omit submission are accepted because many only send one credible event. One
            // submission for several ids is for all of them, as Navidrome reads it.
            var completed = submissions.Count == 1
                ? IsTrue(submissions[0])
                : index >= submissions.Count || IsTrue(submissions[index]);
            // Last.fm hears about outside songs, and library songs too unless the admin left
            // those to a Navidrome that scrobbles them itself (else they would count twice).
            var outside = _localLibraryService.ParseSongId(ids[index]).isExternal;
            var lastFmTakes = scrobbling && (outside || _lastFmScrobbles!.TakesLibraryPlays);
            if (!completed && !lastFmTakes) continue;
            // A client that sends the same completed play again is not playing it again.
            var time = index < times.Count ? times[index] : null;
            var reportedAt = DateTime.UtcNow;
            if (completed && !_recentScrobbles.FirstReport(username, ids[index], time, reportedAt))
            {
                _logger.LogDebug("Scrobble of {Id} for {User} repeats one already learned from", ids[index], username);
                continue;
            }
            // Until something has learned from the play, it is not taken: a song that could not
            // be looked up this time is withdrawn, so the client's retry counts.
            var taken = false;
            try
            {
                var song = await _radioTrackResolver.ResolveScrobbleAsync(ids[index], authenticatedParameters);
                if (song is null || song.Artist.Length == 0 || song.Title.Length == 0)
                {
                    if (completed) _recentScrobbles.Withdraw(username, ids[index], time, reportedAt);
                    continue;
                }
                var track = new LastFmTrack(song.Artist, song.Title, song.Album, song.Duration);
                if (!completed)
                {
                    if (lastFmTakes) _lastFmScrobbles!.NowPlaying(username, track);
                    continue;
                }
                var playedAt = DateTime.UtcNow;
                if (index < times.Count && long.TryParse(times[index], out var unix))
                {
                    try { playedAt = DateTimeOffset.FromUnixTimeMilliseconds(unix).UtcDateTime; }
                    catch (ArgumentOutOfRangeException) { /* retain now */ }
                }
                taken = true;
                if (learning)
                    recorded |= _radioStateStore!.RecordPlay(username, new LastFmRadioPlay
                    {
                        SongId = ids[index], Artist = song.Artist, Title = song.Title,
                        Album = song.Album, Genre = song.Genre, Duration = song.Duration,
                        IsLocal = song.IsLocal, PlayedAtUtc = playedAt, Source = "scrobble"
                    });
                // Queued, not awaited: the client's answer never waits on Last.fm.
                if (lastFmTakes)
                    _lastFmScrobbles!.Scrobble(username, track, playedAt);
                if (submitting && !song.IsLocal)
                    await _listenBrainz!.SubmitListenAsync(username, song.Artist, song.Title,
                        song.Album, song.Duration, playedAt, HttpContext.RequestAborted);
            }
            catch (Exception ex)
            {
                if (completed && !taken) _recentScrobbles.Withdraw(username, ids[index], time, reportedAt);
                _logger.LogDebug(ex, "Radio ignored unreadable scrobble {Id}", ids[index]);
            }
        }
        if (!learning || !recorded || _radioRefreshQueue is null) return;
        var user = _radioStateStore!.GetUser(username);
        if (LastFmRadioRefreshPolicy.ShouldRefreshAfterPlay(user, _lastFmSettings))
            _radioRefreshQueue.Enqueue(username);
    }

    /// <summary>
    /// Octo's own playlist ids: radio stations start "or", mixes "og". Navidrome's ids are 22
    /// characters of base62 that start with 0 to 7, so neither can ever be one of them.
    /// </summary>
    private static bool IsOctoPlaylistId(string id) => id.Length == 22
        && (id.StartsWith("or", StringComparison.Ordinal) || id.StartsWith("og", StringComparison.Ordinal));

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

    /// <summary>A lyrics lookup made while a client waits: a slow or overloaded source costs
    /// at most this, and the song simply shows no lyrics this time.</summary>
    private static readonly TimeSpan InteractiveLyricsBudget = TimeSpan.FromSeconds(4);

    // OpenSubsonic getLyricsBySongId. Feishin fetches this every time a song plays. An external
    // track has no lyrics in Navidrome, so relaying one returned code 70 "data not found" per
    // play; it now gets real lyrics when LYRICS_FETCH is on (#52), and an empty-but-ok list
    // otherwise. A library song's own lyrics (what Navidrome has, in its tags or beside it) rank
    // among the sources as "song": they are served when they win, and the live lookup's when it
    // does, so a song whose tags hold line-timed lyrics still plays word-timed ones from a source
    // above it, or from any source when word timing is preferred.
    //
    // A song someone pinned lyrics for (setLyricsChoice, or the dashboard) answers with those,
    // for every client, found by the song's artist and title when its id has changed since; one
    // set to "none" answers with none. Word cues go only to a client that asked with
    // enhanced=true; anyone else gets the lines exactly as before.
    [HttpGet, HttpPost]
    [Route("rest/getLyricsBySongId")]
    [Route("rest/getLyricsBySongId.view")]
    public async Task<IActionResult> GetLyricsBySongId()
    {
        var parameters = await ExtractAllParameters();
        var id = parameters.GetValueOrDefault("id", "");
        var format = parameters.GetValueOrDefault("f", "xml");
        var enhanced = IsTrue(parameters.GetValueOrDefault("enhanced", ""));
        var (isExternal, _, _) = _localLibraryService.ParseSongId(id);
        var fetching = _lyricsService is not null && _metadataSettings?.CurrentValue.FetchLyrics == true;
        var pin = string.IsNullOrEmpty(id) ? null : _lyricsChoices?.PinFor(id);

        if (isExternal)
        {
            var routing = _idRegistry.Lookup(id) ?? SoulseekMetadataService.TryDecodeExternalId(id);
            string artist = routing is { HasArtistTitle: true } ? routing.Artist! : "";
            string title = routing is { HasArtistTitle: true } ? routing.Title! : "";
            pin ??= _lyricsChoices?.PinFor(id, artist, title);
            if (pin is not null)
                return _responseBuilder.CreateLyricsListResponse(format, pin.Lyrics, pin.Artist ?? artist, pin.Title ?? title, enhanced);

            Octo.Services.Lyrics.LyricsResult? found = null;
            var stillLooking = false;
            if (fetching && routing is { HasArtistTitle: true })
                (found, stillLooking) = await LiveLyricsAsync(artist, title, routing.Album, routing.Duration);
            if (found is null && stillLooking && DrawsItsOwnMarks(parameters))
                return StillLookingForLyrics(format);
            return _responseBuilder.CreateLyricsListResponse(format, found, artist, title, enhanced);
        }

        // Asked with word cues whenever the answer is JSON, so how the song's own lyrics are timed
        // is known; a client that did not ask gets them without (see WithoutCues).
        var json = format.Equals("json", StringComparison.OrdinalIgnoreCase);
        var asking = json && !enhanced
            ? new Dictionary<string, string>(parameters) { ["enhanced"] = "true" }
            : parameters;
        var relay = await _proxyService.RelaySafeAsync("rest/getLyricsBySongId", asking);
        if (relay.Success && relay.Body != null)
        {
            // Navidrome answering ok is also what says the caller may see this song.
            var allowed = IsSuccessfulSubsonicResponse(relay.Body, format);
            var own = json && allowed ? NavidromeLyricsTiming(relay.Body) : null;
            var song = allowed && ((fetching && own is not null) || (pin is null && _lyricsChoices?.AnyPins == true))
                ? await LibrarySongAsync(parameters, id)
                : null;
            pin ??= song is null ? null : _lyricsChoices?.PinFor(id, song.Artist, song.Title);
            if (pin is not null && allowed)
                return _responseBuilder.CreateLyricsListResponse(format, pin.Lyrics,
                    pin.Artist ?? song?.Artist ?? "", pin.Title ?? song?.Title ?? "", enhanced);

            // Read-only: nothing is written beside a file Octo did not download.
            if (fetching && own is { } timing && song is not null)
            {
                var (found, stillLooking) = await LiveLyricsAsync(song.Artist, song.Title, song.Album, song.Duration, timing);
                // The lookup ranked the song's own among the sources, so anything else it found won;
                // only "instrumental" never outranks lyrics the song has.
                if (found is { IsSongsOwn: false } && (timing == Octo.Services.Lyrics.LyricsTiming.None || !found.Instrumental))
                    return _responseBuilder.CreateLyricsListResponse(format, found, song.Artist, song.Title, enhanced);
                if (timing == Octo.Services.Lyrics.LyricsTiming.None && stillLooking && DrawsItsOwnMarks(parameters))
                    return StillLookingForLyrics(format);
            }
            var body = json && !enhanced ? WithoutCues(relay.Body) : relay.Body;
            return File(body, relay.ContentType ?? $"application/{format}");
        }
        return _responseBuilder.CreateResponse(format, "lyricsList", new { });
    }

    /// <summary>
    /// The legacy lyrics call, by artist and title, which older clients (DSub, Subsonic's own)
    /// use. Navidrome answers for its own songs; when it has nothing, the same live lookup as
    /// getLyricsBySongId fills in, as plain text, and a pin for that artist and title wins.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/getLyrics")]
    [Route("rest/getLyrics.view")]
    public async Task<IActionResult> GetLyrics()
    {
        var parameters = await ExtractAllParameters();
        var format = parameters.GetValueOrDefault("f", "xml");
        var artist = parameters.GetValueOrDefault("artist", "").Trim();
        var title = parameters.GetValueOrDefault("title", "").Trim();

        var relay = await _proxyService.RelaySafeAsync("rest/getLyrics", parameters);
        if (!relay.Success || relay.Body is null)
            return _responseBuilder.CreateError(format, 0, "Octo can't reach Navidrome");
        if (!IsSuccessfulSubsonicResponse(relay.Body, format) || artist.Length == 0 || title.Length == 0)
            return File(relay.Body, relay.ContentType ?? $"application/{format}");

        if (_lyricsChoices?.PinFor(artist, title) is { } pin)
            return _responseBuilder.CreateLyricsResponse(format, pin.Lyrics, artist, title);

        var fetching = _lyricsService is not null && _metadataSettings?.CurrentValue.FetchLyrics == true;
        if (fetching && HasNoLegacyLyrics(relay.Body, format)
            && (await LiveLyricsAsync(artist, title, null, null)).Found is { } found)
            return _responseBuilder.CreateLyricsResponse(format, found, artist, title);
        return File(relay.Body, relay.ContentType ?? $"application/{format}");
    }

    private static bool HasNoLegacyLyrics(byte[] body, string format)
    {
        try
        {
            if (format.Equals("json", StringComparison.OrdinalIgnoreCase))
                return string.IsNullOrWhiteSpace(JsonNode.Parse(body)?["subsonic-response"]?["lyrics"]?["value"]?.GetValue<string>());
            var root = XDocument.Parse(Encoding.UTF8.GetString(body)).Root;
            return string.IsNullOrWhiteSpace(root?.Elements().FirstOrDefault(element => element.Name.LocalName == "lyrics")?.Value);
        }
        catch
        {
            return false;
        }
    }

    /// <summary>How long getLyricsCandidates may take: every source that is on, several
    /// entries each, is a slower thing than playing a song, and a person is choosing.</summary>
    private static readonly TimeSpan CandidatesBudget = TimeSpan.FromSeconds(12);

    /// <summary>
    /// octoLyrics v1: every lyrics entry the sources that are on hold for a song, for choosing
    /// between them, with what the song is set to now. title and artist, when given, search
    /// for those instead of the song's own tags, for a song that is tagged wrong. Always JSON.
    /// Credentials are checked with a ping to Navidrome, as getAcquisitions checks them.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/getLyricsCandidates")]
    [Route("rest/getLyricsCandidates.view")]
    public async Task<IActionResult> GetLyricsCandidates()
    {
        var parameters = await ExtractAllParameters();
        const string format = "json";
        if (await CheckCallerAsync(parameters) is { } refused) return refused;
        if (_lyricsChoices is null || _lyricsService is null || _metadataSettings?.CurrentValue.FetchLyrics != true)
            return _responseBuilder.CreateError(format, 0, "Lyrics lookups are off on this server");

        var id = parameters.GetValueOrDefault("id", "");
        if (string.IsNullOrWhiteSpace(id)) return _responseBuilder.CreateError(format, 10, "Required parameter is missing: id");
        if (await SongForLyricsAsync(parameters, id) is not { } song)
            return _responseBuilder.CreateError(format, 70, "Song not found");

        var artist = parameters.GetValueOrDefault("artist", "").Trim() is { Length: > 0 } a ? a : song.Artist;
        var title = parameters.GetValueOrDefault("title", "").Trim() is { Length: > 0 } t ? t : song.Title;
        using var budget = CancellationTokenSource.CreateLinkedTokenSource(HttpContext.RequestAborted);
        budget.CancelAfter(CandidatesBudget);
        var candidates = await _lyricsChoices.CandidatesAsync(new Octo.Services.Lyrics.LyricsQuery(
            artist, Octo.Services.Lyrics.LyricsText.QueryTitle(title, artist), song.Album, song.Duration), budget.Token);
        return _responseBuilder.CreateLyricsCandidatesResponse(id, _lyricsChoices.ChoiceFor(id, song.Artist, song.Title), candidates);
    }

    /// <summary>
    /// octoLyrics v1: set a song's lyrics to one candidate from getLyricsCandidates, to "none"
    /// to show no lyrics, or to "auto" to go back to finding them. The choice is the server's,
    /// so every client and every user sees it, and getLyricsBySongId and getLyrics honour it.
    /// </summary>
    [HttpGet, HttpPost]
    [Route("rest/setLyricsChoice")]
    [Route("rest/setLyricsChoice.view")]
    public async Task<IActionResult> SetLyricsChoice()
    {
        var parameters = await ExtractAllParameters();
        const string format = "json";
        if (await CheckCallerAsync(parameters) is { } refused) return refused;
        if (_lyricsChoices is null) return _responseBuilder.CreateError(format, 0, "Lyrics are off on this server");

        var id = parameters.GetValueOrDefault("id", "");
        var candidate = parameters.GetValueOrDefault("candidate", "").Trim();
        if (string.IsNullOrWhiteSpace(id) || candidate.Length == 0)
            return _responseBuilder.CreateError(format, 10, "Required parameter is missing: id and candidate");

        var who = NativeUsername(parameters);
        var song = await SongForLyricsAsync(parameters, id);
        if (candidate.Equals(Octo.Services.Lyrics.LyricsPin.Auto, StringComparison.OrdinalIgnoreCase))
        {
            _lyricsChoices.Clear(id, song?.Artist, song?.Title);
            return _responseBuilder.CreateLyricsChoiceResponse(id, Octo.Services.Lyrics.LyricsPin.Auto);
        }

        if (song is null) return _responseBuilder.CreateError(format, 70, "Song not found");
        if (candidate.Equals(Octo.Services.Lyrics.LyricsPin.Hidden, StringComparison.OrdinalIgnoreCase))
        {
            _lyricsChoices.Hide(id, song.Artist, song.Title, who);
            return _responseBuilder.CreateLyricsChoiceResponse(id, Octo.Services.Lyrics.LyricsPin.Hidden);
        }

        if (_metadataSettings?.CurrentValue.FetchLyrics != true)
            return _responseBuilder.CreateError(format, 0, "Lyrics lookups are off on this server");
        using var budget = CancellationTokenSource.CreateLinkedTokenSource(HttpContext.RequestAborted);
        budget.CancelAfter(CandidatesBudget);
        if (!await _lyricsChoices.PinAsync(id, candidate, song.Artist, song.Title, who, budget.Token))
            return _responseBuilder.CreateError(format, 70, "Those lyrics could not be found; ask for the candidates again");
        return _responseBuilder.CreateLyricsChoiceResponse(id, candidate);
    }

    /// <summary>Null when Navidrome accepts the caller's credentials, or the error to send.</summary>
    private async Task<IActionResult?> CheckCallerAsync(IReadOnlyDictionary<string, string> parameters)
    {
        var auth = parameters.ToDictionary(pair => pair.Key, pair => pair.Value);
        auth["f"] = "json";
        var check = await _proxyService.RelaySafeAsync("rest/ping", auth);
        if (!check.Success || check.Body is null)
            return _responseBuilder.CreateError("json", 0, "Octo can't reach Navidrome to check who is asking");
        if (!IsSuccessfulSubsonicResponse(check.Body, "json"))
            return _responseBuilder.CreateError("json", 40, "Wrong username or password");
        return null;
    }

    /// <summary>
    /// Null when Navidrome accepts the request's sign-in, else the error to answer with, in the
    /// format asked for. An outage refuses too: an outside song is Octo fetching from the
    /// internet for whoever asks, and a broken Navidrome must not make that anyone at all.
    /// </summary>
    private async Task<IActionResult?> RefuseUnlessSignedInAsync(
        IReadOnlyDictionary<string, string> parameters, string format)
    {
        var verdict = await _credentialCheck.CheckAsync(SubsonicCredential.From(parameters),
            _proxyService, HttpContext.RequestAborted);
        return verdict switch
        {
            CredentialVerdict.Accepted => null,
            CredentialVerdict.Unreachable =>
                _responseBuilder.CreateError(format, 0, "Octo can't reach Navidrome to check who is asking"),
            _ => _responseBuilder.CreateError(format, 40, "Wrong username or password"),
        };
    }

    /// <summary>Who an accepted request signed in as: u, or its API key's owner as Navidrome
    /// names it, else the native token's name. Ask only after the sign-in is accepted.</summary>
    private async Task<string> SignedInUserAsync(IReadOnlyDictionary<string, string> parameters) =>
        await _requestIdentity.UsernameAsync(parameters, _proxyService, HttpContext.RequestAborted)
        ?? NativeUsername(parameters);

    /// <summary>What lyrics are looked up by, for an outside song from the registry and for a
    /// library song from Navidrome as the caller sees it.</summary>
    private async Task<Song?> SongForLyricsAsync(IReadOnlyDictionary<string, string> parameters, string id)
    {
        var (isExternal, _, _) = _localLibraryService.ParseSongId(id);
        if (!isExternal) return await LibrarySongAsync(parameters, id);
        var routing = _idRegistry.Lookup(id) ?? SoulseekMetadataService.TryDecodeExternalId(id);
        return routing is { HasArtistTitle: true }
            ? new Song { Artist = routing.Artist!, Title = routing.Title!, Album = routing.Album ?? "", Duration = routing.Duration }
            : null;
    }

    /// <summary>
    /// Lyrics for a song as it plays, within the interactive budget. A lookup that runs out of
    /// time keeps going in the background (up to <see cref="BackgroundLyricsLimit"/>), so the
    /// service has the answer cached for the next ask; the caller learns it is still looking.
    /// </summary>
    private async Task<(Octo.Services.Lyrics.LyricsResult? Found, bool StillLooking)> LiveLyricsAsync(
        string artist, string title, string? album, int? duration,
        Octo.Services.Lyrics.LyricsTiming songsOwn = Octo.Services.Lyrics.LyricsTiming.None)
    {
        var query = new Octo.Services.Lyrics.LyricsQuery(
            artist, Octo.Services.Lyrics.LyricsText.QueryTitle(title, artist), album, duration);
        // Not tied to the request: a client that stops waiting must not stop the lookup.
        var limit = new CancellationTokenSource(BackgroundLyricsLimit);
        var lookup = _lyricsService!.FindAsync(query, limit.Token, songsOwn);
        _ = lookup.ContinueWith(_ => limit.Dispose(), TaskScheduler.Default);
        try
        {
            var done = await Task.WhenAny(lookup, Task.Delay(InteractiveLyricsBudget, HttpContext.RequestAborted));
            if (done != lookup) return (null, true);
            var answer = await lookup;
            return (answer.Result, answer.Result is null && answer.Transient);
        }
        catch (OperationCanceledException)
        {
            return (null, true);
        }
    }

    /// <summary>How long a lookup the caller stopped waiting for may keep going.</summary>
    private static readonly TimeSpan BackgroundLyricsLimit = TimeSpan.FromSeconds(30);

    /// <summary>
    /// Told only to the Octo app: the lookup is not finished, so this is "not yet", never "none".
    /// Other clients keep getting the ordinary empty list, which is what the spec gives them.
    /// </summary>
    private IActionResult StillLookingForLyrics(string format) =>
        _responseBuilder.CreateError(format, 0, "Still looking for lyrics; ask again shortly");

    /// <summary>
    /// How the lyrics Navidrome sent are timed, the best entry's: word cues, timed lines, plain,
    /// or None when it has none. Null when the answer is not one Octo can read.
    /// </summary>
    internal static Octo.Services.Lyrics.LyricsTiming? NavidromeLyricsTiming(byte[] body)
    {
        try
        {
            var lyrics = JsonNode.Parse(body)?["subsonic-response"]?["lyricsList"]?["structuredLyrics"];
            if (lyrics is null) return Octo.Services.Lyrics.LyricsTiming.None;
            if (lyrics is not JsonArray entries) return null;
            var best = Octo.Services.Lyrics.LyricsTiming.None;
            foreach (var entry in entries)
            {
                if (entry?["line"] is not JsonArray { Count: > 0 }) continue;
                var timing = entry["cueLine"] is JsonArray { Count: > 0 } ? Octo.Services.Lyrics.LyricsTiming.Word
                    : entry["synced"]?.GetValueKind() == JsonValueKind.True ? Octo.Services.Lyrics.LyricsTiming.Line
                    : Octo.Services.Lyrics.LyricsTiming.Plain;
                if (timing > best) best = timing;
            }
            return best;
        }
        catch
        {
            return null;
        }
    }

    /// <summary>Navidrome's lyrics as a client that did not ask for word cues gets them: without
    /// the cue lines and the kind that came with them. Unchanged when they cannot be read.</summary>
    internal static byte[] WithoutCues(byte[] body)
    {
        try
        {
            var root = JsonNode.Parse(body);
            if (root?["subsonic-response"]?["lyricsList"]?["structuredLyrics"] is not JsonArray entries) return body;
            var changed = false;
            foreach (var entry in entries.OfType<JsonObject>())
                changed |= entry.Remove("cueLine") | entry.Remove("kind");
            return changed ? Encoding.UTF8.GetBytes(root.ToJsonString()) : body;
        }
        catch
        {
            return body;
        }
    }

    /// <summary>Artist, title, album and length of a library song, asked as the calling user.</summary>
    private async Task<Song?> LibrarySongAsync(IReadOnlyDictionary<string, string> parameters, string id)
    {
        var request = parameters.ToDictionary(pair => pair.Key, pair => pair.Value);
        request["id"] = id;
        request["f"] = "json";
        var relay = await _proxyService.RelaySafeAsync("rest/getSong", request);
        if (!relay.Success || relay.Body is null) return null;
        try
        {
            var song = JsonNode.Parse(relay.Body)?["subsonic-response"]?["song"];
            var artist = song?["artist"]?.GetValue<string>();
            var title = song?["title"]?.GetValue<string>();
            if (string.IsNullOrWhiteSpace(artist) || string.IsNullOrWhiteSpace(title)) return null;
            return new Song
            {
                Artist = artist, Title = title,
                Album = song?["album"]?.GetValue<string>() ?? "",
                Duration = song?["duration"]?.GetValue<int>(),
            };
        }
        catch
        {
            return null;
        }
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

        // Native artist page for an outside artist: the artist (id in the path again) and
        // its albums, which the client asks for by artist_id, a key HasExternalId never reads.
        var nativeArtist = await TryServeNativeExternalArtistAsync(endpoint);
        if (nativeArtist != null) return nativeArtist;
        var nativeArtistAlbums = await TryServeNativeArtistAlbumsAsync(endpoint, parameters);
        if (nativeArtistAlbums != null) return nativeArtistAlbums;

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
        var reserved = IsOctoPlaylistId(id);
        if (reserved && !HttpMethods.IsGet(Request.Method))
            return StatusCode(StatusCodes.Status405MethodNotAllowed,
                new { error = "Octo's generated playlists are read-only" });
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
            _metadataService.CompleteSongLengths(songs);
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

    /// <summary>
    /// The user to attribute an acquisition to, or null when attribution is off.
    ///
    /// Gated here rather than at the history write, so with the setting off no username is
    /// captured in the first place and nothing downstream is ever holding one.
    /// </summary>
    private string? RequesterFor(string? username) =>
        _subsonicSettings.RecordRequestedBy && !string.IsNullOrWhiteSpace(username) ? username : null;

    /// <summary>
    /// The sign-in to favorite a hearted outside song or album with, or null. Held for every
    /// heart from another app, not only while downloads are favorited: a song that turns out to
    /// be in the library already is always that person's favorite. Octo's own apps send star for
    /// Add, which asks for a copy and not a favorite.
    /// </summary>
    private SubsonicCredential? FavoriteCredential(IReadOnlyDictionary<string, string> parameters) =>
        _starOnArrival is not null && !StarOnArrival.IsOctoApp(parameters.GetValueOrDefault("c"))
            ? SubsonicCredential.From(parameters) : null;

    /// <summary>No Range, or a Range from byte 0, starts a track. A HEAD plays nothing; the
    /// route does not take HEAD today, and this keeps it that way if it ever does.</summary>
    internal static bool IsFirstByteRequest(string method, string? range) =>
        !HttpMethods.IsHead(method)
        && (string.IsNullOrWhiteSpace(range)
            || range.TrimStart().StartsWith("bytes=0-", StringComparison.OrdinalIgnoreCase));

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

        // Don't inject an album the library already returned. The key is the one search3's
        // merge uses, so a title the catalog spells with a curly apostrophe or an accent the
        // library's tags lack is still the same album.
        var localKeys = new HashSet<string>(StringComparer.Ordinal);
        foreach (var node in realArr)
        {
            if (node is not JsonObject o) continue;
            var name = o.TryGetPropertyValue("name", out var n) ? n?.ToString() : null;
            var aa = o.TryGetPropertyValue("albumArtist", out var v) ? v?.ToString() : null;
            if (SubsonicModelMapper.AlbumKey(aa, name) is string key) localKeys.Add(key);
        }

        var added = 0;
        foreach (var album in externalAlbums)
        {
            if (SubsonicModelMapper.AlbumKey(album.Artist, album.Title) is string key && localKeys.Contains(key)) continue;
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
    /// Native artist detail: GET /api/artist/{id} for one of Octo's artist ids. A
    /// Navidrome-mode client opens an artist page with this. Relayed, Navidrome has no such
    /// artist, the relay fails, and the page waits forever. A library artist falls through.
    /// </summary>
    private async Task<IActionResult?> TryServeNativeExternalArtistAsync(string endpoint)
    {
        const string prefix = "api/artist/";
        if (!endpoint.StartsWith(prefix, StringComparison.OrdinalIgnoreCase)) return null;
        var id = endpoint[prefix.Length..].Trim('/');
        if (string.IsNullOrEmpty(id) || id.Contains('/')) return null; // leaf id only

        if (_idRegistry.Lookup(id)?.Kind != RoutingKind.Artist) return null;

        var artist = await _metadataService.GetArtistAsync(SoulseekMetadataService.ProviderName, id);
        if (artist == null) return null;
        // The counts come from the same list the page shows, so they agree with it. Only the
        // track counts already known: the page asks for that list at the same moment, and
        // that request is the one that asks the catalog for the rest.
        var albums = await OutsideArtistAlbumsAsync(id, artist.Name, knownCountsOnly: true);

        var bytes = Encoding.UTF8.GetBytes(BuildNativeArtistObject(artist, albums).ToJsonString());
        Response.StatusCode = 200;
        Response.ContentType = "application/json";
        await Response.Body.WriteAsync(bytes);
        return new EmptyResult();
    }

    /// <summary>
    /// Native albums by artist: GET /api/album?artist_id={outside artist id}, the list an
    /// artist page shows. The rows are the same albums getArtist lists for that artist.
    /// </summary>
    private async Task<IActionResult?> TryServeNativeArtistAlbumsAsync(
        string endpoint, Dictionary<string, string> parameters)
    {
        if (!string.Equals(endpoint, "api/album", StringComparison.OrdinalIgnoreCase))
            return null;

        var artistId = parameters.GetValueOrDefault("artist_id", "").Trim();
        if (string.IsNullOrEmpty(artistId)) return null;
        var routing = _idRegistry.Lookup(artistId);
        if (routing?.Kind != RoutingKind.Artist) return null;

        var albums = await OutsideArtistAlbumsAsync(artistId, routing.Artist ?? "");

        // Feishin asks for a whole discography with _end=-1, so an end that is not past the
        // start means the rest of the list rather than nothing.
        var start = parameters.TryGetValue("_start", out var startStr)
            && int.TryParse(startStr, out var s) && s > 0 ? Math.Min(s, albums.Count) : 0;
        var end = parameters.TryGetValue("_end", out var endStr)
            && int.TryParse(endStr, out var e) && e > start ? Math.Min(e, albums.Count) : albums.Count;

        var arr = new JsonArray();
        foreach (var album in albums.Skip(start).Take(end - start))
            arr.Add(BuildNativeAlbumObject(album, 1));

        var bytes = Encoding.UTF8.GetBytes(arr.ToJsonString());
        Response.StatusCode = 200;
        Response.Headers["X-Total-Count"] = albums.Count.ToString();
        Response.ContentType = "application/json";
        await Response.Body.WriteAsync(bytes);
        return new EmptyResult();
    }

    /// <summary>
    /// An outside artist's albums, each naming the artist and linking back to the artist's
    /// own id, as getArtist fills them: the catalog's listing carries neither.
    /// </summary>
    private async Task<List<Album>> OutsideArtistAlbumsAsync(string artistId, string artistName,
        bool knownCountsOnly = false)
    {
        var albums = knownCountsOnly
            ? await _metadataService.GetArtistAlbumsKnownCountsAsync(SoulseekMetadataService.ProviderName, artistId)
            : await _metadataService.GetArtistAlbumsAsync(SoulseekMetadataService.ProviderName, artistId);
        foreach (var album in albums)
        {
            if (string.IsNullOrEmpty(album.Artist)) album.Artist = artistName;
            if (string.IsNullOrEmpty(album.ArtistId)) album.ArtistId = artistId;
        }
        return albums;
    }

    /// <summary>
    /// Serializes one outside Artist into Navidrome's native artist JSON shape. Counts go
    /// out both flat (older Navidrome) and under "stats" by role (newer Navidrome), since
    /// clients read one or the other. The image URLs are the three getArtistInfo2 gives; a
    /// client that draws the artist through getCoverArt with the id is served by Octo too.
    /// </summary>
    private static JsonObject BuildNativeArtistObject(Artist artist, IReadOnlyList<Album> albums)
    {
        var songCount = albums.Sum(a => a.SongCount ?? 0);
        JsonObject Stats() => new() { ["albumCount"] = albums.Count, ["songCount"] = songCount, ["size"] = 0 };

        var o = new JsonObject
        {
            ["id"] = artist.Id,
            ["name"] = artist.Name,
            ["albumCount"] = albums.Count,
            ["songCount"] = songCount,
            ["size"] = 0,
            ["stats"] = new JsonObject { ["albumartist"] = Stats(), ["artist"] = Stats() },
            ["playCount"] = 0,
            ["missing"] = false,
            // Fixed old timestamp, same reasoning as BuildNativeSongObject.
            ["createdAt"] = "2020-01-01T00:00:00Z",
            ["updatedAt"] = "2020-01-01T00:00:00Z",
        };
        if (!string.IsNullOrEmpty(artist.ImageUrl))
        {
            o["smallImageUrl"] = artist.ImageUrl;
            o["mediumImageUrl"] = artist.ImageUrl;
            o["largeImageUrl"] = artist.ImageUrl;
        }
        return o;
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
        // Navidrome's own album JSON carries the release types among its tags, in the tag's
        // lowercase words, and a Navidrome-mode client groups an artist's page by them.
        if (a.ReleaseTypes.Count > 0)
        {
            o["tags"] = new JsonObject
            {
                ["releasetype"] = new JsonArray(a.ReleaseTypes
                    .Select(type => (JsonNode?)JsonValue.Create(type.ToLowerInvariant())).ToArray()),
            };
        }
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
