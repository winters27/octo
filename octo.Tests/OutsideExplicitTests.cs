using System.Collections.Concurrent;
using System.Net;
using System.Text.Json;
using System.Xml.Linq;
using Microsoft.AspNetCore.Mvc;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.Logging;
using Microsoft.Extensions.Options;
using Moq;
using Moq.Protected;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services.CoverArt;
using Octo.Services.Metadata;
using Octo.Services.Soulseek;
using Octo.Services.Subsonic;
using Octo.Services.YouTube;

namespace Octo.Tests;

/// <summary>
/// Outside songs and albums carry the catalog's explicit word (OpenSubsonic's explicitStatus),
/// so the apps can mark the explicit version before it is added, and a clean copy gets an id of
/// its own so it can be listed beside the explicit original, while every id handed out before
/// stays the same.
/// </summary>
public class OutsideExplicitTests
{
    private readonly ExternalIdRegistry _registry = new();
    private readonly ConcurrentQueue<string> _calls = new();

    private SoulseekMetadataService Service(Dictionary<string, string> routes)
    {
        var handler = new Mock<HttpMessageHandler>();
        handler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync", ItExpr.IsAny<HttpRequestMessage>(), ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync((HttpRequestMessage request, CancellationToken _) =>
            {
                var url = request.RequestUri!.ToString();
                _calls.Enqueue(url);
                foreach (var (needle, body) in routes)
                    if (url.Contains(needle, StringComparison.OrdinalIgnoreCase))
                        return new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent(body) };
                return new HttpResponseMessage(HttpStatusCode.NotFound);
            });
        var factory = new Mock<IHttpClientFactory>();
        factory.Setup(f => f.CreateClient(It.IsAny<string>())).Returns(() => new HttpClient(handler.Object));
        var config = new ConfigurationBuilder().AddInMemoryCollection(new Dictionary<string, string?>()).Build();
        return new SoulseekMetadataService(
            new YouTubeResolver(factory.Object, config, new Mock<ILogger<YouTubeResolver>>().Object), _registry,
            new DeezerMetadataService(factory.Object, TestOptions.Monitor(new MetadataSettings()), new Mock<ILogger<DeezerMetadataService>>().Object),
            new CoverArtAggregator([], new Mock<ILogger<CoverArtAggregator>>().Object),
            new Mock<ILogger<SoulseekMetadataService>>().Object);
    }

    // ---- The words -------------------------------------------------------------------------

    [Theory]
    [InlineData(1, null, 1)]
    [InlineData(3, null, 3)]
    [InlineData(0, true, 0)]
    [InlineData(4, null, 1)]
    [InlineData(2, true, 1)]
    [InlineData(6, false, null)]
    [InlineData(null, null, null)]
    public void FromCatalog_ReadsTheCatalogsNumbersAndFlag(int? content, bool? flag, int? expected) =>
        Assert.Equal(expected, ExplicitStatus.FromCatalog(content, flag));

    [Theory]
    [InlineData(1, "explicit")]
    [InlineData(3, "clean")]
    [InlineData(0, "")]
    [InlineData(null, "")]
    public void ForClients_IsOpenSubsonicsWord(int? content, string word) =>
        Assert.Equal(word, ExplicitStatus.ForClients(content));

    // ---- Ids -------------------------------------------------------------------------------

    /// <summary>The seed an id is made from is unchanged for everything but a clean copy, so a
    /// star, a playlist place or a pin made before keeps working. Pinned to the ids the old seed made.</summary>
    [Fact]
    public void Ids_OnlyACleanCopyGetsANewOne()
    {
        SoulseekRouting Song(string? version) => new()
        {
            Kind = RoutingKind.Song, Artist = "Kendrick Lamar", Title = "luther", Duration = 177, Version = version,
        };
        SoulseekRouting Album(string? version) => new()
        {
            Kind = RoutingKind.Album, Artist = "Kendrick Lamar", Album = "GNX", Version = version,
        };

        Assert.Equal("tkNhkzBNCpsT1buw9JyZd5", _registry.Register(Song(null)));
        Assert.Equal("tkNhkzBNCpsT1buw9JyZd5", _registry.Register(Song("explicit")));
        Assert.Equal("7tcNOznFZSzl3pGOYcZfl7", _registry.Register(Album(null)));
        Assert.NotEqual("tkNhkzBNCpsT1buw9JyZd5", _registry.Register(Song("clean")));
        Assert.NotEqual("7tcNOznFZSzl3pGOYcZfl7", _registry.Register(Album("clean")));
    }

    [Fact]
    public void Registry_ASearchRowMintedAgain_KeepsWhatTheCatalogSaid()
    {
        var id = _registry.Register(new SoulseekRouting { Artist = "Tame Impala", Title = "Dracula", ExplicitContent = 1 });
        _registry.Register(new SoulseekRouting { Artist = "Tame Impala", Title = "Dracula" });

        Assert.Equal(1, _registry.Lookup(id)!.ExplicitContent);
    }

    // ---- Search songs ----------------------------------------------------------------------

    /// <summary>The catalog lists the clean edit first; the row is the song, and the song is explicit.</summary>
    private const string DraculaSearch = @"{""data"":[
        {""id"":11,""title"":""Dracula"",""duration"":210,""explicit_lyrics"":false,""explicit_content_lyrics"":3,
         ""artist"":{""name"":""Tame Impala""},""album"":{""id"":5,""title"":""Dracula""}},
        {""id"":12,""title"":""Dracula"",""duration"":210,""explicit_lyrics"":true,""explicit_content_lyrics"":1,
         ""artist"":{""name"":""Tame Impala""},""album"":{""id"":6,""title"":""Dracula""}}]}";

    [Fact]
    public async Task SearchRows_CarryTheSongsExplicitWord_AndGetSongKeepsIt()
    {
        var svc = Service(new() { ["/search?q="] = DraculaSearch });
        var rows = await svc.SearchSongsByArtistTitleAsync("Tame Impala", "Dracula");
        var idBefore = rows[0].Id;

        await svc.EnrichExternalSongsAsync(rows);
        var again = await svc.SearchSongsByArtistTitleAsync("Tame Impala", "Dracula");
        var song = await svc.GetSongAsync(SoulseekMetadataService.ProviderName, idBefore);

        Assert.Equal(ExplicitStatus.Explicit, rows[0].ExplicitContentLyrics);
        Assert.Equal(idBefore, again[0].Id);
        Assert.Equal(ExplicitStatus.Explicit, again[0].ExplicitContentLyrics);
        Assert.Equal(ExplicitStatus.Explicit, song!.ExplicitContentLyrics);
    }

    [Fact]
    public async Task SearchRows_ASongOnlyTheCleanEditOf_SaysClean()
    {
        var svc = Service(new() { ["/search?q="] = @"{""data"":[
            {""id"":11,""title"":""Song"",""duration"":200,""explicit_lyrics"":false,""explicit_content_lyrics"":3,
             ""artist"":{""name"":""A""},""album"":{""id"":5,""title"":""Song""}}]}" });
        var rows = await svc.SearchSongsByArtistTitleAsync("A", "Song");

        await svc.EnrichExternalSongsAsync(rows);

        Assert.Equal(ExplicitStatus.Clean, rows[0].ExplicitContentLyrics);
    }

    // ---- Albums ----------------------------------------------------------------------------

    private const string GnxSearch = @"{""data"":[
        {""id"":100,""title"":""GNX"",""record_type"":""album"",""nb_tracks"":12,""explicit_lyrics"":true,""artist"":{""name"":""Kendrick Lamar""}},
        {""id"":200,""title"":""GNX"",""record_type"":""album"",""nb_tracks"":12,""explicit_lyrics"":false,""artist"":{""name"":""Kendrick Lamar""}},
        {""id"":300,""title"":""Mr. Morale"",""record_type"":""album"",""nb_tracks"":18,""explicit_lyrics"":false,""artist"":{""name"":""Kendrick Lamar""}}]}";

    [Fact]
    public async Task AlbumSearch_ACleanCopyBesideItsExplicitOriginal_IsItsOwnRow()
    {
        var svc = Service(new() { ["/search/album"] = GnxSearch });

        var albums = await svc.SearchAlbumsAsync("gnx", 10);

        var (explicitCopy, cleanCopy, other) = (albums[0], albums[1], albums[2]);
        Assert.NotEqual(explicitCopy.Id, cleanCopy.Id);
        Assert.Equal(ExplicitStatus.Explicit, explicitCopy.ExplicitContentLyrics);
        Assert.Equal(ExplicitStatus.Clean, cleanCopy.ExplicitContentLyrics);
        // Not explicit and with no explicit twin: the listing cannot say it is a clean edit.
        Assert.Null(other.ExplicitContentLyrics);
        Assert.Equal("100", _registry.Lookup(explicitCopy.Id)!.ExternalAlbumId);
        Assert.Equal("200", _registry.Lookup(cleanCopy.Id)!.ExternalAlbumId);
        Assert.Equal("7tcNOznFZSzl3pGOYcZfl7", explicitCopy.Id);
    }

    [Fact]
    public async Task GetAlbum_TheCleanCopysSongsAreTheirOwn_AndSayClean()
    {
        var svc = Service(new()
        {
            ["/search/album"] = GnxSearch,
            ["/album/100/tracks"] = @"{""total"":1,""data"":[{""title"":""squabble up"",""duration"":157,""track_position"":2,""disk_number"":1,
                ""explicit_lyrics"":true,""explicit_content_lyrics"":1,""artist"":{""name"":""Kendrick Lamar""}}]}",
            ["/album/200/tracks"] = @"{""total"":1,""data"":[{""title"":""squabble up"",""duration"":157,""track_position"":2,""disk_number"":1,
                ""explicit_lyrics"":false,""explicit_content_lyrics"":3,""artist"":{""name"":""Kendrick Lamar""}}]}",
            ["/album/100"] = @"{""id"":100,""title"":""GNX"",""explicit_lyrics"":true,""explicit_content_lyrics"":1,""nb_tracks"":1,""artist"":{""name"":""Kendrick Lamar""}}",
            ["/album/200"] = @"{""id"":200,""title"":""GNX"",""explicit_lyrics"":false,""explicit_content_lyrics"":3,""nb_tracks"":1,""artist"":{""name"":""Kendrick Lamar""}}",
        });
        var albums = await svc.SearchAlbumsAsync("gnx", 10);

        var original = await svc.GetAlbumAsync(SoulseekMetadataService.ProviderName, albums[0].Id);
        var clean = await svc.GetAlbumAsync(SoulseekMetadataService.ProviderName, albums[1].Id);

        Assert.Equal(ExplicitStatus.Explicit, original!.ExplicitContentLyrics);
        Assert.Equal(ExplicitStatus.Clean, clean!.ExplicitContentLyrics);
        Assert.Equal(ExplicitStatus.Explicit, original.Songs[0].ExplicitContentLyrics);
        Assert.Equal(ExplicitStatus.Clean, clean.Songs[0].ExplicitContentLyrics);
        Assert.NotEqual(original.Songs[0].Id, clean.Songs[0].Id);
        Assert.Equal(ExplicitStatus.Clean, (await svc.GetSongAsync(SoulseekMetadataService.ProviderName, clean.Songs[0].Id))!.ExplicitContentLyrics);
    }

    [Fact]
    public async Task ArtistPage_OfAReleaseInTwoCopies_ListsTheExplicitOne()
    {
        var deezer = new DeezerMetadataService(HttpAnswering(@"{""total"":2,""data"":[
            {""id"":200,""title"":""GNX"",""record_type"":""album"",""explicit_lyrics"":false,""release_date"":""2024-11-22""},
            {""id"":100,""title"":""GNX"",""record_type"":""album"",""explicit_lyrics"":true,""release_date"":""2024-11-22""}]}"),
            TestOptions.Monitor(new MetadataSettings()), new Mock<ILogger<DeezerMetadataService>>().Object);

        var hit = Assert.Single(await deezer.GetArtistAlbumsAsync("525046", "Kendrick Lamar"));

        Assert.Equal("100", hit.DeezerId);
        Assert.True(hit.ExplicitLyrics);
    }

    private static IHttpClientFactory HttpAnswering(string body)
    {
        var handler = new Mock<HttpMessageHandler>();
        handler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync", ItExpr.IsAny<HttpRequestMessage>(), ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync(() => new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent(body) });
        var factory = new Mock<IHttpClientFactory>();
        factory.Setup(f => f.CreateClient(It.IsAny<string>())).Returns(() => new HttpClient(handler.Object));
        return factory.Object;
    }

    // ---- What a client reads ---------------------------------------------------------------

    private static SubsonicResponseBuilder Builder() => new(new ExternalIdRegistry(), Options.Create(new SubsonicSettings()));

    [Fact]
    public void Responses_SayExplicitOrCleanForOutsideSongsAndAlbums_InJsonAndXml()
    {
        var builder = Builder();
        var song = new Song { Id = "s1", Artist = "Tame Impala", Title = "Dracula", ExplicitContentLyrics = 1 };
        var plain = new Song { Id = "s2", Artist = "A", Title = "T" };
        var album = new Album { Id = "a1", Artist = "Kendrick Lamar", Title = "GNX", ExplicitContentLyrics = 3 };

        var songJson = JsonSerializer.SerializeToElement(builder.ConvertSongToJson(song));
        var albumJson = JsonSerializer.SerializeToElement(builder.ConvertAlbumToJson(album));
        XNamespace ns = "http://subsonic.org/restapi";

        Assert.Equal("explicit", songJson.GetProperty("explicitStatus").GetString());
        Assert.Equal("", JsonSerializer.SerializeToElement(builder.ConvertSongToJson(plain)).GetProperty("explicitStatus").GetString());
        Assert.Equal("clean", albumJson.GetProperty("explicitStatus").GetString());
        Assert.Equal("explicit", builder.ConvertSongToXml(song, ns).Attribute("explicitStatus")?.Value);
    }
}
