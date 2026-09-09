using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.Logging;
using Moq;
using Moq.Protected;
using Octo.Services.CoverArt;
using Octo.Services.Metadata;
using Octo.Services.Soulseek;
using Octo.Services.YouTube;
using System.Net;

namespace Octo.Tests;

public class SoulseekMetadataServiceTests
{
    private readonly ExternalIdRegistry _registry = new();

    /// <summary>Builds the service with a Deezer layer answering from a url-substring map.
    /// YouTube is never reached by the album paths under test.</summary>
    private SoulseekMetadataService BuildService(Dictionary<string, string> routes)
    {
        var handler = new Mock<HttpMessageHandler>();
        handler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync",
                ItExpr.IsAny<HttpRequestMessage>(),
                ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync((HttpRequestMessage req, CancellationToken _) =>
            {
                var url = req.RequestUri!.ToString();
                foreach (var (needle, body) in routes)
                {
                    if (url.Contains(needle, StringComparison.OrdinalIgnoreCase))
                        return new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent(body) };
                }
                return new HttpResponseMessage(HttpStatusCode.NotFound);
            });

        var factory = new Mock<IHttpClientFactory>();
        factory.Setup(f => f.CreateClient(It.IsAny<string>()))
            .Returns(() => new HttpClient(handler.Object));

        var config = new ConfigurationBuilder().AddInMemoryCollection(new Dictionary<string, string?>()).Build();
        var youtube = new YouTubeResolver(factory.Object, config, new Mock<ILogger<YouTubeResolver>>().Object);
        var deezer = new DeezerMetadataService(factory.Object,
            TestOptions.Monitor(new Octo.Models.Settings.MetadataSettings()),
            new Mock<ILogger<DeezerMetadataService>>().Object);

        var coverArt = new CoverArtAggregator(
            Array.Empty<ICoverArtSource>(), new Mock<ILogger<CoverArtAggregator>>().Object);

        return new SoulseekMetadataService(
            youtube, _registry, deezer, coverArt, new Mock<ILogger<SoulseekMetadataService>>().Object);
    }

    private const string AlbumSearchJson = @"{""data"":[
        {""id"":1,""title"":""Test Album"",""record_type"":""album"",""nb_tracks"":2,
         ""cover_xl"":""https://cdn/a.jpg"",""artist"":{""name"":""Test Artist""}}]}";

    private const string AlbumDetailJson = @"{""id"":1,""title"":""Test Album"",
        ""cover_xl"":""https://cdn/a.jpg"",""release_date"":""1997-05-21"",
        ""artist"":{""name"":""Test Artist""},""genres"":{""data"":[{""name"":""Rock""}]}}";

    private const string AlbumTracksJson = @"{""total"":2,""data"":[
        {""title"":""Track One"",""duration"":180,""track_position"":1,""disk_number"":1,""artist"":{""name"":""Test Artist""}},
        {""title"":""Track Two"",""duration"":240,""track_position"":2,""disk_number"":1,""artist"":{""name"":""Test Artist""}}]}";

    [Fact]
    public async Task SearchAlbumsAsync_ReturnsAlbumsWithRegistryIds()
    {
        // Arrange
        var svc = BuildService(new() { ["/search/album"] = AlbumSearchJson });

        // Act
        var albums = await svc.SearchAlbumsAsync("test", 10);

        // Assert
        var album = Assert.Single(albums);
        Assert.Equal("Test Album", album.Title);
        Assert.Equal("Test Artist", album.Artist);
        Assert.False(album.IsLocal);
        // The external id must be the REGISTRY id, not the Deezer id: every consumer
        // round-trips through the registry.
        Assert.Equal(album.Id, album.ExternalId);
        var routing = _registry.Lookup(album.Id);
        Assert.NotNull(routing);
        Assert.Equal(RoutingKind.Album, routing!.Kind);
        Assert.Equal("1", routing.ExternalAlbumId);
    }

    [Fact]
    public async Task GetAlbumAsync_PopulatesSongsWithAlbumIdAndRegistryIds()
    {
        // Arrange
        var svc = BuildService(new()
        {
            ["/search/album"] = AlbumSearchJson,
            ["/album/1/tracks"] = AlbumTracksJson,
            ["/album/1"] = AlbumDetailJson,
        });
        var albumId = (await svc.SearchAlbumsAsync("test", 10)).Single().Id;

        // Act
        var album = await svc.GetAlbumAsync(SoulseekMetadataService.ProviderName, albumId);

        // Assert
        Assert.NotNull(album);
        Assert.Equal(2, album!.Songs.Count);
        Assert.Equal(new[] { "Track One", "Track Two" }, album.Songs.Select(s => s.Title));
        Assert.Equal(1997, album.Year);

        foreach (var song in album.Songs)
        {
            // AlbumId is what makes the album clickable in native mode and what
            // DownloadMode.Album reads.
            Assert.Equal(albumId, song.AlbumId);
            Assert.Equal("Test Album", song.Album);
            Assert.Equal(song.Id, song.ExternalId);

            // The routing must carry the album too, or the download path re-derives it
            // from artist+title and can land on a compilation instead.
            var routing = _registry.Lookup(song.Id);
            Assert.NotNull(routing);
            Assert.Equal(RoutingKind.Song, routing!.Kind);
            Assert.Equal("Test Album", routing.Album);
        }

        Assert.Equal(180, album.Songs[0].Duration);
        Assert.Equal(1, album.Songs[0].Track);
    }

    [Fact]
    public async Task GetSongAsync_ReturnsAlbumFromRouting()
    {
        // Guards the tagging fix: the download path rebuilds a song from its id alone.
        var svc = BuildService(new()
        {
            ["/search/album"] = AlbumSearchJson,
            ["/album/1/tracks"] = AlbumTracksJson,
            ["/album/1"] = AlbumDetailJson,
        });
        var albumId = (await svc.SearchAlbumsAsync("test", 10)).Single().Id;
        var trackId = (await svc.GetAlbumAsync(SoulseekMetadataService.ProviderName, albumId))!.Songs[0].Id;

        var song = await svc.GetSongAsync(SoulseekMetadataService.ProviderName, trackId);

        Assert.NotNull(song);
        Assert.Equal("Test Album", song!.Album);
        Assert.Equal("Track One", song.Title);
    }

    [Fact]
    public async Task GetAlbumAsync_TracklistUnresolvable_StillReturnsAlbum()
    {
        // An album with no resolvable tracklist must still render rather than 404.
        var svc = BuildService(new() { ["/search/album"] = AlbumSearchJson });
        var albumId = (await svc.SearchAlbumsAsync("test", 10)).Single().Id;

        var album = await svc.GetAlbumAsync(SoulseekMetadataService.ProviderName, albumId);

        Assert.NotNull(album);
        Assert.Equal("Test Album", album!.Title);
        Assert.Empty(album.Songs);
    }

    [Fact]
    public async Task GetAlbumAsync_UnknownId_ReturnsNull()
    {
        var svc = BuildService(new());

        Assert.Null(await svc.GetAlbumAsync(SoulseekMetadataService.ProviderName, "nope"));
    }

    [Fact]
    public async Task GetAlbumAsync_WrongProvider_ReturnsNull()
    {
        var svc = BuildService(new() { ["/search/album"] = AlbumSearchJson });
        var albumId = (await svc.SearchAlbumsAsync("test", 10)).Single().Id;

        Assert.Null(await svc.GetAlbumAsync("deezer", albumId));
    }
}
