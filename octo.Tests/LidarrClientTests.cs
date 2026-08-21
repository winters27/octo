using System.Net;
using System.Text.Json.Nodes;
using Microsoft.Extensions.Options;
using Moq;
using Octo.Models.Settings;
using Octo.Services.Lidarr;

namespace Octo.Tests;

public class LidarrClientTests
{
    private sealed class Handler : HttpMessageHandler
    {
        public List<(HttpMethod Method, string Path, string Url, string? ApiKey, string? Body)> Requests { get; } = new();
        public required Func<HttpRequestMessage, string> Respond { get; init; }

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var body = request.Content is null ? null : await request.Content.ReadAsStringAsync(ct);
            Requests.Add((request.Method, request.RequestUri!.PathAndQuery, request.RequestUri.ToString(),
                request.Headers.TryGetValues("X-Api-Key", out var values) ? values.Single() : null, body));
            return new HttpResponseMessage(HttpStatusCode.OK)
            {
                Content = new StringContent(Respond(request)),
            };
        }
    }

    private static LidarrClient Build(Handler handler, LidarrSettings? settings = null)
    {
        settings ??= new LidarrSettings
        {
            BaseUrl = "http://lidarr:8686",
            ApiKey = "secret",
            RootFolderPath = "/data/music",
            QualityProfileId = 3,
            MetadataProfileId = 4,
        };
        var monitor = new Mock<IOptionsMonitor<LidarrSettings>>();
        monitor.SetupGet(x => x.CurrentValue).Returns(settings);
        var factory = new Mock<IHttpClientFactory>();
        factory.Setup(x => x.CreateClient(It.IsAny<string>())).Returns(() => new HttpClient(handler));
        return new LidarrClient(factory.Object, monitor.Object);
    }

    [Fact]
    public async Task OptionsUseApiKeyAndMapServerChoices()
    {
        var handler = new Handler
        {
            Respond = req => req.RequestUri!.AbsolutePath switch
            {
                "/api/v1/rootfolder" => "[{\"id\":1,\"path\":\"/data/music\"}]",
                "/api/v1/qualityprofile" => "[{\"id\":2,\"name\":\"Lossless\"}]",
                "/api/v1/metadataprofile" => "[{\"id\":3,\"name\":\"Standard\"}]",
                _ => "[]",
            },
        };

        var result = await Build(handler).GetOptionsAsync();

        Assert.Equal("/data/music", Assert.Single(result.RootFolders).Path);
        Assert.Equal("Lossless", Assert.Single(result.QualityProfiles).Name);
        Assert.Equal("Standard", Assert.Single(result.MetadataProfiles).Name);
        Assert.All(handler.Requests, r => Assert.Equal("secret", r.ApiKey));
    }

    [Fact]
    public async Task ConnectionTestUsesEnteredUrlAndApiKeyWithoutSaving()
    {
        var handler = new Handler
        {
            Respond = request => request.RequestUri!.AbsolutePath == "/api/v1/system/status" ? "{}" : "[]",
        };

        await Build(handler).TestConnectionAsync("http://new-lidarr:8686/", "entered-key");

        Assert.Contains(handler.Requests, request =>
            request.Path == "/api/v1/system/status"
            && request.Url == "http://new-lidarr:8686/api/v1/system/status");
        Assert.All(handler.Requests, request => Assert.Equal("entered-key", request.ApiKey));
        Assert.Contains(handler.Requests, request => request.Path == "/api/v1/rootfolder");
        Assert.Contains(handler.Requests, request => request.Path == "/api/v1/qualityprofile");
        Assert.Contains(handler.Requests, request => request.Path == "/api/v1/metadataprofile");
    }

    [Fact]
    public void AlbumSelectionRequiresExactIdentityAndUsesYearToDisambiguate()
    {
        var candidates = new[]
        {
            Candidate("a", "In Rainbows", "Radiohead", 2007),
            Candidate("b", "In Rainbows", "Radiohead", 2025),
        };

        Assert.Equal("a", LidarrClient.SelectBestAlbum(candidates, "RADIOHEAD", "In-Rainbows", 2007)!.ForeignAlbumId);
        Assert.Null(LidarrClient.SelectBestAlbum(candidates, "Radiohead", "In Rainbows", null));
        Assert.Null(LidarrClient.SelectBestAlbum(candidates, "Other", "In Rainbows", 2007));
    }

    [Fact]
    public async Task NewAlbumUsesChosenDefaultsAndDoesNotMonitorFutureReleases()
    {
        var handler = new Handler
        {
            Respond = req => (req.Method.Method, req.RequestUri!.AbsolutePath) switch
            {
                ("GET", "/api/v1/album") => "[]",
                ("GET", "/api/v1/artist") => "[]",
                ("POST", "/api/v1/album") => "{\"id\":42}",
                ("POST", "/api/v1/command") => "{\"id\":9}",
                _ => "[]",
            },
        };

        var id = await Build(handler).EnsureAlbumAndSearchAsync(Candidate("mbid", "Album", "Artist", 2020));

        Assert.Equal(42, id);
        var add = JsonNode.Parse(handler.Requests.Single(r => r.Method == HttpMethod.Post && r.Path == "/api/v1/album").Body!)!;
        Assert.Equal("/data/music", add["artist"]!["rootFolderPath"]!.GetValue<string>());
        Assert.Equal(3, add["artist"]!["qualityProfileId"]!.GetValue<int>());
        Assert.Equal(4, add["artist"]!["metadataProfileId"]!.GetValue<int>());
        Assert.Equal("none", add["artist"]!["monitorNewItems"]!.GetValue<string>());
        var command = handler.Requests.Single(r => r.Method == HttpMethod.Post && r.Path == "/api/v1/command").Body!;
        Assert.Contains("AlbumSearch", command);
        Assert.Contains("42", command);
    }

    [Fact]
    public async Task ExistingArtistSettingsArePreservedWhenAddingAlbum()
    {
        var handler = new Handler
        {
            Respond = req => (req.Method.Method, req.RequestUri!.AbsolutePath) switch
            {
                ("GET", "/api/v1/album") => "[]",
                ("GET", "/api/v1/artist") =>
                    "[{\"id\":7,\"foreignArtistId\":\"artist-mbid\",\"path\":\"/existing/Artist\",\"qualityProfileId\":9,\"metadataProfileId\":10}]",
                ("POST", "/api/v1/album") => "{\"id\":42}",
                ("POST", "/api/v1/command") => "{\"id\":9}",
                _ => "[]",
            },
        };

        await Build(handler).EnsureAlbumAndSearchAsync(Candidate("mbid", "Album", "Artist", 2020));

        var add = JsonNode.Parse(handler.Requests.Single(r =>
            r.Method == HttpMethod.Post && r.Path == "/api/v1/album").Body!)!;
        Assert.Equal(7, add["artistId"]!.GetValue<int>());
        Assert.Equal("/existing/Artist", add["artist"]!["path"]!.GetValue<string>());
        Assert.Equal(9, add["artist"]!["qualityProfileId"]!.GetValue<int>());
        Assert.Equal(10, add["artist"]!["metadataProfileId"]!.GetValue<int>());
    }

    [Fact]
    public async Task ExistingAlbumIsMonitoredWithoutChangingItsArtist()
    {
        var handler = new Handler
        {
            Respond = req => (req.Method.Method, req.RequestUri!.AbsolutePath) switch
            {
                ("GET", "/api/v1/album") =>
                    "[{\"id\":12,\"foreignAlbumId\":\"mbid\",\"monitored\":false,\"artist\":{\"id\":7,\"qualityProfileId\":9}}]",
                ("PUT", "/api/v1/album/12") => "{\"id\":12,\"monitored\":true}",
                ("POST", "/api/v1/command") => "{\"id\":9}",
                _ => "[]",
            },
        };

        var id = await Build(handler).EnsureAlbumAndSearchAsync(Candidate("mbid", "Album", "Artist", 2020));

        Assert.Equal(12, id);
        var update = JsonNode.Parse(handler.Requests.Single(r => r.Method == HttpMethod.Put).Body!)!;
        Assert.True(update["monitored"]!.GetValue<bool>());
        Assert.Equal(9, update["artist"]!["qualityProfileId"]!.GetValue<int>());
        Assert.DoesNotContain(handler.Requests, r => r.Path == "/api/v1/artist");
    }

    [Fact]
    public async Task AlbumTracksJoinTrackFilesReturnedBySeparateEndpoint()
    {
        var handler = new Handler
        {
            Respond = req => req.RequestUri!.AbsolutePath switch
            {
                "/api/v1/track" =>
                    "[{\"id\":1,\"title\":\"Song\",\"trackNumber\":\"1\",\"duration\":123000,\"hasFile\":true,\"trackFileId\":8}]",
                "/api/v1/trackFile" =>
                    "[{\"id\":8,\"path\":\"/data/music/Artist/Album/01.flac\",\"size\":456}]",
                _ => "[]",
            },
        };

        var track = Assert.Single(await Build(handler).GetAlbumTracksAsync(42));

        Assert.True(track.HasFile);
        Assert.Equal("/data/music/Artist/Album/01.flac", track.Path);
        Assert.Equal(456, track.SizeBytes);
        Assert.Contains(handler.Requests, r => r.Path == "/api/v1/track?albumId=42");
        Assert.Contains(handler.Requests, r => r.Path == "/api/v1/trackFile?albumId=42");
    }

    [Fact]
    public async Task ImportCompletionUsesAlbumStatisticsNotAlternateReleaseRows()
    {
        var handler = new Handler
        {
            Respond = req => req.RequestUri!.AbsolutePath switch
            {
                "/api/v1/album/42" =>
                    "{\"id\":42,\"statistics\":{\"trackCount\":1,\"trackFileCount\":1}}",
                "/api/v1/track" =>
                    "[{\"id\":1,\"title\":\"Song\",\"hasFile\":true,\"trackFileId\":8},{\"id\":2,\"title\":\"Alternate release bonus\",\"hasFile\":false,\"trackFileId\":0}]",
                "/api/v1/trackFile" =>
                    "[{\"id\":8,\"path\":\"/data/music/Artist/Album/01.flac\",\"size\":456}]",
                _ => "[]",
            },
        };

        var state = await Build(handler).GetAlbumImportStateAsync(42);

        Assert.True(state.IsComplete);
        Assert.Equal(1, state.TrackCount);
        Assert.Equal(2, state.Tracks.Count);
    }

    [Fact]
    public void ImportedPathsTranslateRelativeToSelectedRootAndRejectEscapes()
    {
        var translated = LidarrHeartAcquisitionService.TranslateImportedPath(
            "/data/music/Radiohead/In Rainbows/01.flac", "/data/music", "/music");
        Assert.Equal(Path.GetFullPath("/music/Radiohead/In Rainbows/01.flac"), translated);
        Assert.Throws<InvalidOperationException>(() =>
            LidarrHeartAcquisitionService.TranslateImportedPath("/downloads/other.flac", "/data/music", "/music"));
    }

    private static LidarrAlbumCandidate Candidate(string foreignId, string title, string artist, int? year)
    {
        var resource = new JsonObject
        {
            ["foreignAlbumId"] = foreignId,
            ["title"] = title,
            ["releaseDate"] = year is null ? null : $"{year}-01-01T00:00:00Z",
            ["artist"] = new JsonObject
            {
                ["artistName"] = artist,
                ["foreignArtistId"] = "artist-mbid",
            },
        };
        return new LidarrAlbumCandidate(0, foreignId, title, artist, year, resource);
    }
}
