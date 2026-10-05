using System.Net;
using System.Text;
using System.Text.Json.Nodes;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.Logging.Abstractions;
using Octo.Models.Settings;
using Octo.Services.Lidarr;
using Octo.Services.Subsonic;

namespace Octo.Tests;

/// <summary>
/// One song through Lidarr for a replacement. Lidarr fetches whole albums, so Octo borrows the
/// album: it copies out the song it asked for, deletes every file that search brought in, and
/// puts the album's monitoring back. It never touches a file Lidarr had before, a song Lidarr
/// manages is left to Lidarr, and a heart working on the album means not now.
/// </summary>
public sealed class LidarrTrackFetcherTests : IDisposable
{
    internal const string AlbumId = "rg-mezzanine";
    private readonly string _root = Path.Combine(Path.GetTempPath(), "octo-lidarr-fetch-" + Guid.NewGuid().ToString("N"));
    private readonly FakeLidarr _lidarr;
    private readonly LidarrAlbumClaims _claims = new();
    private readonly LidarrSettings _settings = new()
    {
        BaseUrl = "http://lidarr:8686", ApiKey = "k", RootFolderPath = "/data/music",
        QualityProfileId = 1, MetadataProfileId = 1, ImportTimeoutSeconds = 10,
    };

    public LidarrTrackFetcherTests()
    {
        Directory.CreateDirectory(_root);
        _lidarr = new FakeLidarr(_root);
    }

    public void Dispose()
    {
        try { Directory.Delete(_root, true); } catch { /* best effort */ }
    }

    private string Destination => Path.Combine(_root, ".octo-incoming", "lidarr", "job");

    private LidarrTrackFetcher Fetcher()
    {
        var factory = new ReviewFixtures.OneClientFactory(_lidarr);
        var monitor = TestOptions.Monitor(_settings);
        var subsonic = TestOptions.Monitor(new SubsonicSettings { Url = "http://navidrome.test", AutoDetectDownloadPath = false });
        var identity = new NavidromeIdentityService(subsonic, factory, NullLogger<NavidromeIdentityService>.Instance);
        var config = new ConfigurationBuilder()
            .AddInMemoryCollection(new Dictionary<string, string?> { ["Library:DownloadPath"] = _root }).Build();
        return new LidarrTrackFetcher(new LidarrClient(factory, monitor), monitor, identity, config, _claims,
            NullLogger<LidarrTrackFetcher>.Instance) { Poll = TimeSpan.FromMilliseconds(10) };
    }

    private static LidarrTrackRequest Teardrop(bool losslessOnly = true, string? original = null) =>
        new("Massive Attack", "Teardrop", "Mezzanine", 330, losslessOnly, original);

    [Fact]
    public async Task CopiesTheSongThenTakesBackWhatTheSearchBrought()
    {
        _lidarr.OnSearch = () => { _lidarr.Import(1, "Angel", "FLAC"); _lidarr.Import(3, "Teardrop", "FLAC"); };

        var copy = await Fetcher().FetchAsync(Teardrop(), Destination);

        Assert.StartsWith(Destination, copy);
        Assert.Equal(".flac", Path.GetExtension(copy));
        Assert.Equal("Teardrop", File.ReadAllText(copy));
        Assert.Equal(1, _lidarr.Searches);
        // Both files the search brought in are gone, through Lidarr, so its records stay true.
        Assert.Equal(2, _lidarr.Deleted.Count);
        Assert.Empty(Directory.EnumerateFiles(Path.Combine(_root, "Massive Attack"), "*", SearchOption.AllDirectories));
        // Octo added the album, so it stops monitoring it again.
        Assert.Contains(false, _lidarr.MonitorChanges);
    }

    [Fact]
    public async Task ALossyCopyIsNotWorthWaitingFor()
    {
        _lidarr.OnSearch = () => _lidarr.Import(3, "Teardrop", "MP3-320");
        // Short: running out of time is what this is about.
        _settings.ImportTimeoutSeconds = 1;

        var missed = await Assert.ThrowsAsync<FileNotFoundException>(() => Fetcher().FetchAsync(Teardrop(), Destination));

        Assert.Contains("lossless", missed.Message);
        Assert.False(Directory.Exists(Destination) && Directory.EnumerateFiles(Destination).Any());
        Assert.Single(_lidarr.Deleted);
    }

    [Fact]
    public async Task AnyCopyWillDoWhenLosslessIsNotAsked()
    {
        _lidarr.OnSearch = () => _lidarr.Import(3, "Teardrop", "MP3-320");

        var copy = await Fetcher().FetchAsync(Teardrop(losslessOnly: false), Destination);

        Assert.Equal(".mp3", Path.GetExtension(copy));
    }

    [Fact]
    public async Task ALosslessCopyLidarrAlreadyHasInTheLibrary_IsADuplicate()
    {
        _lidarr.AddAlbum(monitored: true);
        _lidarr.Import(3, "Teardrop", "FLAC");

        var refused = await Assert.ThrowsAsync<InvalidOperationException>(() => Fetcher().FetchAsync(Teardrop(), Destination));

        Assert.Contains("duplicate", refused.Message);
        Assert.Equal(0, _lidarr.Searches);
        Assert.Empty(_lidarr.Deleted);
    }

    [Fact]
    public async Task ASongLidarrManages_IsLeftToLidarr()
    {
        _lidarr.AddAlbum(monitored: true);
        var managed = _lidarr.Import(3, "Teardrop", "MP3-320");

        var refused = await Assert.ThrowsAsync<InvalidOperationException>(
            () => Fetcher().FetchAsync(Teardrop(original: managed), Destination));

        Assert.Equal(LidarrTrackFetcher.ManagedText, refused.Message);
        Assert.Equal(0, _lidarr.Searches);
        Assert.True(File.Exists(managed));
    }

    [Fact]
    public async Task AFileLidarrHadBefore_IsNeverDeleted()
    {
        _lidarr.AddAlbum(monitored: true);
        var owners = _lidarr.Import(1, "Angel", "FLAC");
        _lidarr.OnSearch = () => _lidarr.Import(3, "Teardrop", "FLAC");

        await Fetcher().FetchAsync(Teardrop(), Destination);

        Assert.True(File.Exists(owners));
        Assert.Single(_lidarr.Deleted);
        // It was monitored before, so it stays monitored.
        Assert.DoesNotContain(false, _lidarr.MonitorChanges);
    }

    [Fact]
    public async Task AHeartOnTheAlbumMeansNotNow()
    {
        _claims.HeartStarted(AlbumId);

        var refused = await Assert.ThrowsAsync<InvalidOperationException>(() => Fetcher().FetchAsync(Teardrop(), Destination));

        Assert.Contains("heart", refused.Message);
        Assert.Equal(0, _lidarr.Searches);
        Assert.False(_claims.UpgradeBusy(AlbumId));
    }

    [Fact]
    public async Task TwoSongsOfOneAlbumShareOneSearch()
    {
        var release = new TaskCompletionSource();
        _lidarr.OnSearch = () => _ = release.Task.ContinueWith(_ =>
        {
            _lidarr.Import(1, "Angel", "FLAC");
            _lidarr.Import(3, "Teardrop", "FLAC");
        });
        var fetcher = Fetcher();

        var teardrop = fetcher.FetchAsync(Teardrop(), Destination);
        var angel = fetcher.FetchAsync(new LidarrTrackRequest("Massive Attack", "Angel", "Mezzanine", 380, true), Destination);
        await LastFmScrobbleServiceTests.Until(() => _lidarr.Searches == 1);
        release.SetResult();
        await Task.WhenAll(teardrop, angel).WaitAsync(TimeSpan.FromSeconds(10));

        Assert.Equal(1, _lidarr.Searches);
        Assert.Equal("Teardrop", File.ReadAllText(await teardrop));
        Assert.Equal("Angel", File.ReadAllText(await angel));
        Assert.Equal(2, _lidarr.Deleted.Count);
        Assert.False(_claims.UpgradeBusy(AlbumId));
    }

    [Fact]
    public async Task AnAlbumLidarrDoesNotKnow_IsNotFound()
    {
        _lidarr.KnowsAlbum = false;

        await Assert.ThrowsAsync<FileNotFoundException>(() => Fetcher().FetchAsync(Teardrop(), Destination));
        Assert.Equal(0, _lidarr.Searches);
    }

    // ---- Picked releases ----------------------------------------------------------------------

    private const string Release = "Massive Attack - Mezzanine (1998) [FLAC]";

    [Fact]
    public async Task APickLidarrHasForgottenIsFoundAgainByItsTitleIndexerAndSize()
    {
        _lidarr.Offered.Add(("fresh-guid", 3, Release, 400_000_000));
        _lidarr.Offered.Add(("other-guid", 3, "Massive Attack - Mezzanine (1998) [MP3]", 90_000_000));
        _lidarr.OnSearch = () => _lidarr.Import(3, "Teardrop", "FLAC");
        var steps = new List<string>();

        var copy = await Fetcher().FetchAsync(Teardrop() with
        {
            Release = new LidarrReleasePick("stale-guid", 3, Release, 400_000_000), Step = (text, _) => steps.Add(text),
        }, Destination);

        Assert.Equal("Teardrop", File.ReadAllText(copy));
        Assert.Equal(["fresh-guid"], _lidarr.Grabs);
        Assert.Equal(["Grabbing the release you picked"], steps);
    }

    [Fact]
    public async Task APickNoLongerOfferedIsRefusedInPlainWords()
    {
        _lidarr.Offered.Add(("other-guid", 3, "Massive Attack - Mezzanine (1998) [MP3]", 90_000_000));

        var refused = await Assert.ThrowsAsync<InvalidOperationException>(() => Fetcher().FetchAsync(Teardrop() with
        {
            Release = new LidarrReleasePick("stale-guid", 3, Release, 400_000_000),
        }, Destination));

        Assert.Equal(LidarrClient.PickGoneText, refused.Message);
        Assert.Empty(_lidarr.Grabs);
    }

    [Fact]
    public async Task APickForAnAlbumAlreadyBeingFetchedSaysItWasNotGrabbed()
    {
        var release = new TaskCompletionSource();
        _lidarr.Offered.Add(("guid", 3, Release, 400_000_000));
        _lidarr.OnSearch = () => _ = release.Task.ContinueWith(_ =>
        {
            _lidarr.Import(1, "Angel", "FLAC");
            _lidarr.Import(3, "Teardrop", "FLAC");
        });
        var fetcher = Fetcher();
        var steps = new List<string>();

        var teardrop = fetcher.FetchAsync(Teardrop(), Destination);
        await LastFmScrobbleServiceTests.Until(() => _lidarr.Searches == 1);
        var angel = fetcher.FetchAsync(new LidarrTrackRequest("Massive Attack", "Angel", "Mezzanine", 380, true,
            Release: new LidarrReleasePick("guid", 3, Release, 400_000_000), Step: (text, _) => steps.Add(text)), Destination);
        await LastFmScrobbleServiceTests.Until(() => steps.Count == 1);
        release.SetResult();
        await Task.WhenAll(teardrop, angel).WaitAsync(TimeSpan.FromSeconds(10));

        Assert.Equal(["Lidarr is already fetching this album, so the release you picked was not grabbed"], steps);
        Assert.Empty(_lidarr.Grabs);
    }

    [Fact]
    public void TheSongIsMatchedByTitleNeverByNumber()
    {
        LidarrImportedTrack Track(int number, string title, int seconds) =>
            new(number, title, number, seconds, true, $"/data/music/{title}.flac", 1, "Massive Attack", number);
        var tracks = new[] { Track(1, "Angel", 380), Track(3, "Teardrop", 330) };

        Assert.Equal("Teardrop", LidarrTrackFetcher.Match(tracks, Teardrop())!.Title);
        Assert.Null(LidarrTrackFetcher.Match(tracks, new LidarrTrackRequest("Massive Attack", "Unknown", null, 380, true)));
        // Two takes of one title: the nearer length.
        var takes = new[] { Track(3, "Teardrop", 200), Track(4, "Teardrop", 331) };
        Assert.Equal(4, LidarrTrackFetcher.Match(takes, Teardrop())!.Id);
    }

    /// <summary>
    /// Enough of Lidarr's v1 API for one album: lookup, add, monitor, search, tracks and files.
    /// A search runs OnSearch, which imports files the way Lidarr would, onto the disk Octo sees.
    /// </summary>
    internal sealed class FakeLidarr(string octoRoot) : HttpMessageHandler
    {
        private readonly object _gate = new();
        private bool _added;
        private bool _monitored;
        private int _nextFile = 100;
        private readonly Dictionary<int, (int TrackId, string Path, string Quality)> _files = new();
        private readonly Dictionary<int, (string Title, int Number)> _tracks = new()
        {
            [1] = ("Angel", 1), [3] = ("Teardrop", 3),
        };

        public bool KnowsAlbum { get; set; } = true;
        public Action? OnSearch { get; set; }
        public int Searches;
        public List<int> Deleted { get; } = [];
        public List<bool> MonitorChanges { get; } = [];

        public void AddAlbum(bool monitored) { _added = true; _monitored = monitored; }

        /// <summary>The releases an interactive search lists now. A grab by any other guid is
        /// refused the way Lidarr refuses a release it has forgotten.</summary>
        public List<(string Guid, int IndexerId, string Title, long Size)> Offered { get; } = [];
        public List<string> Grabs { get; } = [];

        /// <summary>A file for a track, at Lidarr's path, written where Octo sees it. Returns Octo's path.</summary>
        public string Import(int trackId, string title, string quality)
        {
            var extension = quality.StartsWith("FLAC", StringComparison.Ordinal) ? ".flac" : ".mp3";
            var relative = Path.Combine("Massive Attack", "Mezzanine", $"{trackId:00} - {title}{extension}");
            var local = Path.Combine(octoRoot, relative);
            Directory.CreateDirectory(Path.GetDirectoryName(local)!);
            File.WriteAllText(local, title);
            lock (_gate) _files[_nextFile++] = (trackId, "/data/music/" + relative.Replace('\\', '/'), quality);
            return local;
        }

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var path = request.RequestUri!.AbsolutePath;
            var query = request.RequestUri.Query;
            var body = request.Content is null ? null : await request.Content.ReadAsStringAsync(ct);
            lock (_gate)
            {
                return (request.Method.Method, path) switch
                {
                    ("GET", "/api/v1/album/lookup") => Json(KnowsAlbum ? $"[{Album(0)}]" : "[]"),
                    ("GET", "/api/v1/album") => Json(_added ? $"[{Album(7)}]" : "[]"),
                    ("GET", "/api/v1/album/7") => Json(Statistics()),
                    ("GET", "/api/v1/artist") => Json("[]"),
                    ("POST", "/api/v1/album") => Added(),
                    ("PUT", "/api/v1/album/7") => Monitor(true),
                    ("PUT", "/api/v1/album/monitor") => Monitor(JsonNode.Parse(body!)!["monitored"]!.GetValue<bool>()),
                    ("POST", "/api/v1/command") => Search(),
                    ("GET", "/api/v1/release") => Json(new JsonArray(Offered.Select(r => (JsonNode)new JsonObject
                    {
                        ["guid"] = r.Guid, ["indexerId"] = r.IndexerId, ["title"] = r.Title, ["size"] = r.Size,
                    }).ToArray()).ToJsonString()),
                    ("POST", "/api/v1/release") => Grab(JsonNode.Parse(body!)!["guid"]!.GetValue<string>()),
                    ("GET", "/api/v1/track") => Json(Tracks()),
                    ("GET", "/api/v1/trackFile") => Json(Files()),
                    ("DELETE", _) when path.StartsWith("/api/v1/trackfile/", StringComparison.Ordinal) => Delete(int.Parse(path[18..])),
                    _ => new HttpResponseMessage(HttpStatusCode.NotFound) { Content = new StringContent($"no {request.Method} {path}{query}") },
                };
            }
        }

        private string Album(int id) => new JsonObject
        {
            ["id"] = id, ["foreignAlbumId"] = AlbumId, ["title"] = "Mezzanine", ["monitored"] = _monitored,
            ["releaseDate"] = "1998-04-20",
            ["artist"] = new JsonObject { ["id"] = 5, ["artistName"] = "Massive Attack", ["foreignArtistId"] = "ma", ["monitored"] = true },
        }.ToJsonString();

        private string Statistics()
        {
            var album = JsonNode.Parse(Album(7))!.AsObject();
            album["statistics"] = new JsonObject { ["trackCount"] = _tracks.Count, ["trackFileCount"] = _files.Count };
            return album.ToJsonString();
        }

        private HttpResponseMessage Added()
        {
            _added = true;
            _monitored = true;
            return Json(Album(7));
        }

        private HttpResponseMessage Monitor(bool monitored)
        {
            _monitored = monitored;
            MonitorChanges.Add(monitored);
            return Json(Album(7));
        }

        private HttpResponseMessage Search()
        {
            Searches++;
            // Before the answer, the way a quick Lidarr would have it, so no test races the clock.
            // The lock is the same thread's, and C#'s lock lets it in again.
            OnSearch?.Invoke();
            return Json("""{"id":1,"name":"AlbumSearch"}""");
        }

        private HttpResponseMessage Grab(string guid)
        {
            if (Offered.All(r => r.Guid != guid))
                return new HttpResponseMessage(HttpStatusCode.NotFound)
                {
                    Content = new StringContent("""{"message":"Couldn't find requested release in cache, cache timeout probably expired."}"""),
                };
            Grabs.Add(guid);
            return Search();
        }

        private string Tracks() => new JsonArray(_tracks.Select(t =>
        {
            var file = _files.FirstOrDefault(f => f.Value.TrackId == t.Key);
            var has = file.Value.Path is not null;
            return (JsonNode)new JsonObject
            {
                ["id"] = t.Key, ["title"] = t.Value.Title, ["trackNumber"] = t.Value.Number.ToString(),
                ["duration"] = t.Key == 1 ? 380000 : 330000, ["hasFile"] = has, ["trackFileId"] = has ? file.Key : 0,
                ["artist"] = new JsonObject { ["artistName"] = "Massive Attack" },
            };
        }).ToArray()).ToJsonString();

        private string Files() => new JsonArray(_files.Select(f => (JsonNode)new JsonObject
        {
            ["id"] = f.Key, ["path"] = f.Value.Path, ["size"] = 1000,
            ["quality"] = new JsonObject { ["quality"] = new JsonObject { ["name"] = f.Value.Quality } },
        }).ToArray()).ToJsonString();

        private HttpResponseMessage Delete(int id)
        {
            if (_files.Remove(id, out var file))
            {
                Deleted.Add(id);
                var local = Path.Combine(octoRoot, file.Path["/data/music/".Length..]);
                if (File.Exists(local)) File.Delete(local);
            }
            return new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent("") };
        }

        private static HttpResponseMessage Json(string body) =>
            new(HttpStatusCode.OK) { Content = new StringContent(body, Encoding.UTF8, "application/json") };
    }
}
