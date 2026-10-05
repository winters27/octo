using Microsoft.Extensions.Logging;
using Moq;
using Moq.Protected;
using Octo.Models.Settings;
using Octo.Services.Metadata;
using System.Net;

namespace Octo.Tests;

public class DeezerMetadataServiceTests
{
    /// <summary>Builds a service whose HTTP layer answers from a url-substring to body map.
    /// Any url with no match returns 404, which exercises the best-effort paths.</summary>
    private static DeezerMetadataService BuildService(Dictionary<string, string> routes,
        string language = "en", List<HttpRequestMessage>? capture = null,
        ILogger<DeezerMetadataService>? logger = null)
    {
        var handler = new Mock<HttpMessageHandler>();
        handler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync",
                ItExpr.IsAny<HttpRequestMessage>(),
                ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync((HttpRequestMessage req, CancellationToken _) =>
            {
                capture?.Add(req);
                var url = req.RequestUri!.ToString();
                foreach (var (needle, body) in routes)
                {
                    if (url.Contains(needle, StringComparison.OrdinalIgnoreCase))
                        return new HttpResponseMessage(HttpStatusCode.OK)
                        {
                            Content = new StringContent(body),
                        };
                }
                return new HttpResponseMessage(HttpStatusCode.NotFound);
            });

        var factory = new Mock<IHttpClientFactory>();
        factory.Setup(f => f.CreateClient(It.IsAny<string>()))
            .Returns(() => new HttpClient(handler.Object));

        return new DeezerMetadataService(factory.Object,
            TestOptions.Monitor(new MetadataSettings { Language = language }),
            logger ?? new Mock<ILogger<DeezerMetadataService>>().Object);
    }

    /// <summary>Deezer reports throttling as HTTP 200 with this body, which is the whole
    /// reason a parsed document cannot be treated as a successful call.</summary>
    private const string QuotaEnvelope =
        @"{""error"":{""type"":""Exception"",""message"":""Quota limit exceeded"",""code"":4}}";

    /// <summary>Deezer's only "this really does not exist" answer, also HTTP 200.</summary>
    private const string NoDataEnvelope =
        @"{""error"":{""type"":""DataException"",""message"":""no data"",""code"":800}}";

    /// <summary>Like <see cref="BuildService"/>, but each route serves its bodies in order
    /// and repeats the last one once exhausted, so a test can make a call fail and then
    /// succeed. Routes are matched by url SUBSTRING, so "/album/1/tracks" must be
    /// registered before "/album/1" or the shorter needle swallows both.</summary>
    private static DeezerMetadataService BuildSequencedService(
        List<(string Needle, string[] Bodies)> routes, out Func<string, int> callCount)
    {
        var counts = new Dictionary<string, int>();
        var gate = new object();

        var handler = new Mock<HttpMessageHandler>();
        handler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync",
                ItExpr.IsAny<HttpRequestMessage>(),
                ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync((HttpRequestMessage req, CancellationToken _) =>
            {
                var url = req.RequestUri!.ToString();
                lock (gate)
                {
                    foreach (var (needle, bodies) in routes)
                    {
                        if (!url.Contains(needle, StringComparison.OrdinalIgnoreCase)) continue;
                        counts.TryGetValue(needle, out var n);
                        counts[needle] = n + 1;
                        return new HttpResponseMessage(HttpStatusCode.OK)
                        {
                            Content = new StringContent(bodies[Math.Min(n, bodies.Length - 1)]),
                        };
                    }
                }
                return new HttpResponseMessage(HttpStatusCode.NotFound);
            });

        var factory = new Mock<IHttpClientFactory>();
        factory.Setup(f => f.CreateClient(It.IsAny<string>()))
            .Returns(() => new HttpClient(handler.Object));

        callCount = needle => { lock (gate) { return counts.TryGetValue(needle, out var n) ? n : 0; } };
        return new DeezerMetadataService(factory.Object,
            TestOptions.Monitor(new MetadataSettings()),
            new Mock<ILogger<DeezerMetadataService>>().Object);
    }

    private const string AlbumDetailJson =
        @"{""id"":711108,""title"":""Canciones Prohibidas"",""nb_tracks"":10,""label"":""WM Spain"",
           ""release_date"":""1998-04-30"",""cover_xl"":""https://cdn/xl.jpg"",
           ""artist"":{""name"":""Extremoduro""},""genres"":{""data"":[{""name"":""Pop""}]}}";

    private static string TracksJson(int count) =>
        @"{""total"":" + count + @",""data"":[" + string.Join(",",
            Enumerable.Range(1, count).Select(i =>
                $@"{{""title"":""Track {i}"",""duration"":200,""track_position"":{i},
                     ""disk_number"":1,""artist"":{{""name"":""Extremoduro""}}}}")) + "]}";

    [Fact]
    public async Task SearchAlbumsAsync_MapsFieldsAndDropsSingles()
    {
        // Arrange: one real album, one EP, and a one-track "single" that must be dropped.
        var json = @"{""data"":[
            {""id"":14880659,""title"":""In Rainbows"",""record_type"":""album"",""nb_tracks"":10,
             ""cover_xl"":""https://cdn/xl.jpg"",""artist"":{""name"":""Radiohead""}},
            {""id"":14880561,""title"":""In Rainbows (Disk 2)"",""record_type"":""ep"",""nb_tracks"":8,
             ""cover_xl"":""https://cdn/ep.jpg"",""artist"":{""name"":""Radiohead""}},
            {""id"":999,""title"":""Nude"",""record_type"":""single"",""nb_tracks"":1,
             ""cover_xl"":""https://cdn/s.jpg"",""artist"":{""name"":""Radiohead""}}
        ]}";
        var svc = BuildService(new() { ["/search/album"] = json });

        // Act
        var hits = await svc.SearchAlbumsAsync("In Rainbows", 10);

        // Assert
        Assert.Equal(2, hits.Count);
        Assert.Equal("14880659", hits[0].DeezerId);
        Assert.Equal("In Rainbows", hits[0].Title);
        Assert.Equal("Radiohead", hits[0].Artist);
        Assert.Equal("https://cdn/xl.jpg", hits[0].CoverUrl);
        Assert.Equal(10, hits[0].TrackCount);
        Assert.DoesNotContain(hits, h => h.Title == "Nude");
    }

    [Fact]
    public async Task SearchAlbumsAsync_KeepsMultiTrackSingle()
    {
        // Only a 1-2 track "single" is noise; a longer one is a real release.
        var json = @"{""data"":[{""id"":5,""title"":""Long Single"",""record_type"":""single"",
            ""nb_tracks"":6,""artist"":{""name"":""X""}}]}";
        var svc = BuildService(new() { ["/search/album"] = json });

        var hits = await svc.SearchAlbumsAsync("q", 10);

        Assert.Single(hits);
    }

    [Fact]
    public async Task GetAlbumDetailAsync_OrdersByDiscThenTrackPosition()
    {
        // Arrange: deliberately out of order, spanning two discs.
        var album = @"{""id"":1,""title"":""Test Album"",""cover_xl"":""https://cdn/a.jpg"",
            ""release_date"":""1997-05-21"",""label"":""Label X"",
            ""artist"":{""name"":""Test Artist""},""genres"":{""data"":[{""name"":""Rock""}]}}";
        var tracks = @"{""total"":4,""data"":[
            {""title"":""D2T1"",""duration"":100,""track_position"":1,""disk_number"":2,""artist"":{""name"":""Test Artist""}},
            {""title"":""D1T2"",""duration"":200,""track_position"":2,""disk_number"":1,""isrc"":""ABC"",""artist"":{""name"":""Test Artist""}},
            {""title"":""D1T1"",""duration"":300,""track_position"":1,""disk_number"":1,""artist"":{""name"":""Test Artist""}},
            {""title"":""D2T2"",""duration"":150,""track_position"":2,""disk_number"":2,""artist"":{""name"":""Test Artist""}}
        ]}";
        var svc = BuildService(new()
        {
            ["/album/1/tracks"] = tracks,
            ["/album/1"] = album,
        });

        // Act
        var detail = await svc.GetAlbumDetailAsync("1");

        // Assert
        Assert.NotNull(detail);
        Assert.Equal("Test Album", detail!.Title);
        Assert.Equal("Test Artist", detail.Artist);
        Assert.Equal(1997, detail.Year);
        Assert.Equal("Rock", detail.Genre);
        Assert.Equal("Label X", detail.Label);
        Assert.Equal(new[] { "D1T1", "D1T2", "D2T1", "D2T2" }, detail.Tracks.Select(t => t.Title));
        Assert.Equal("ABC", detail.Tracks[1].Isrc);
        Assert.Equal(300, detail.Tracks[0].Duration);
    }

    [Fact]
    public async Task GetAlbumDetailAsync_MalformedPayload_ReturnsNullWithoutThrowing()
    {
        var svc = BuildService(new() { ["/album/"] = "{ this is not json" });

        var detail = await svc.GetAlbumDetailAsync("1");

        Assert.Null(detail);
    }

    [Fact]
    public async Task GetAlbumDetailAsync_UnreachableApi_ReturnsNull()
    {
        // No routes registered, so every request 404s.
        var svc = BuildService(new());

        Assert.Null(await svc.GetAlbumDetailAsync("1"));
    }

    [Fact]
    public async Task FindAlbumIdAsync_ReturnsFirstMatch()
    {
        var json = @"{""data"":[{""id"":14880659,""title"":""In Rainbows""}]}";
        var svc = BuildService(new() { ["/search/album"] = json });

        var id = await svc.FindAlbumIdAsync("Radiohead", "In Rainbows");

        Assert.Equal("14880659", id);
    }

    [Fact]
    public async Task FindAlbumIdAsync_NoAlbumName_ReturnsNullWithoutCallingApi()
    {
        var svc = BuildService(new());

        Assert.Null(await svc.FindAlbumIdAsync("Radiohead", ""));
    }

    [Fact]
    public async Task SearchAlbumsAsync_EmptyQueryOrZeroLimit_ReturnsEmpty()
    {
        var svc = BuildService(new());

        Assert.Empty(await svc.SearchAlbumsAsync("", 10));
        Assert.Empty(await svc.SearchAlbumsAsync("q", 0));
    }

    // ---- Quota-poisoning regression tests (issue #8) ------------------------
    // Deezer answers HTTP 200 even when refusing a call, so a parsed document is not
    // a successful call. Caching one of those refusals is what made an album report
    // songCount 0 permanently.

    /// <summary>
    /// The exact reported failure: the album call succeeds and the TRACKLIST call is
    /// throttled, which used to build a valid AlbumDetail carrying real title/year/genre
    /// with an empty tracklist and cache it forever.
    /// </summary>
    [Fact]
    public async Task GetAlbumDetailAsync_TracklistThrottled_NotCachedAndRecoversOnRetry()
    {
        var svc = BuildSequencedService(new()
        {
            // Longest needle first: "/album/711108" is a prefix of the tracks url.
            ("/album/711108/tracks", new[] { QuotaEnvelope, TracksJson(10) }),
            ("/album/711108", new[] { AlbumDetailJson }),
        }, out var calls);

        var first = await svc.GetAlbumDetailAsync("711108");
        Assert.Null(first);

        var second = await svc.GetAlbumDetailAsync("711108");
        Assert.NotNull(second);
        Assert.Equal(10, second!.Tracks.Count);
        Assert.Equal("Canciones Prohibidas", second.Title);
        Assert.Equal(1998, second.Year);

        // Proves it retried rather than serving a cached failure.
        Assert.Equal(2, calls("/album/711108/tracks"));
    }

    /// <summary>The other poisoning branch: the album call itself is throttled.</summary>
    [Fact]
    public async Task GetAlbumDetailAsync_AlbumCallThrottled_NotCachedAndRecoversOnRetry()
    {
        var svc = BuildSequencedService(new()
        {
            ("/album/711108/tracks", new[] { TracksJson(10) }),
            ("/album/711108", new[] { QuotaEnvelope, AlbumDetailJson }),
        }, out _);

        Assert.Null(await svc.GetAlbumDetailAsync("711108"));

        var second = await svc.GetAlbumDetailAsync("711108");
        Assert.NotNull(second);
        Assert.Equal(10, second!.Tracks.Count);
    }

    /// <summary>
    /// Int() returns int?, and a lifted "nbTracks > 0" is FALSE when nb_tracks is absent.
    /// Testing that alone would let an empty tracklist through and cache it, leaving the
    /// reported bug fixed only for albums that happen to report a track count.
    /// </summary>
    [Fact]
    public async Task GetAlbumDetailAsync_EmptyTracklistAndNoTrackCount_NotCached()
    {
        const string noNbTracks =
            @"{""id"":711108,""title"":""Canciones Prohibidas"",""artist"":{""name"":""Extremoduro""}}";

        var svc = BuildSequencedService(new()
        {
            ("/album/711108/tracks", new[] { @"{""data"":[]}", TracksJson(10) }),
            ("/album/711108", new[] { noNbTracks, AlbumDetailJson }),
        }, out _);

        Assert.Null(await svc.GetAlbumDetailAsync("711108"));

        var second = await svc.GetAlbumDetailAsync("711108");
        Assert.NotNull(second);
        Assert.Equal(10, second!.Tracks.Count);
    }

    /// <summary>
    /// The case the guard must NOT break: Deezer says the album has zero tracks and
    /// returns zero tracks. That is an answer, so it is cached rather than refetched
    /// on every getAlbum forever.
    /// </summary>
    [Fact]
    public async Task GetAlbumDetailAsync_GenuinelyEmptyAlbum_IsAnsweredAndCached()
    {
        const string zeroTracks =
            @"{""id"":42,""title"":""Empty"",""nb_tracks"":0,""artist"":{""name"":""Nobody""}}";

        var svc = BuildSequencedService(new()
        {
            ("/album/42/tracks", new[] { @"{""total"":0,""data"":[]}" }),
            ("/album/42", new[] { zeroTracks }),
        }, out var calls);

        var first = await svc.GetAlbumDetailAsync("42");
        Assert.NotNull(first);
        Assert.Empty(first!.Tracks);

        await svc.GetAlbumDetailAsync("42");
        Assert.Equal(1, calls("/album/42/tracks"));
    }

    /// <summary>A definitive "no data" is cacheable, unlike a throttle.</summary>
    [Fact]
    public async Task GetAlbumDetailAsync_DefinitiveNoData_IsCached()
    {
        var svc = BuildSequencedService(new()
        {
            ("/album/999/tracks", new[] { TracksJson(3) }),
            ("/album/999", new[] { NoDataEnvelope, AlbumDetailJson }),
        }, out var calls);

        Assert.Null(await svc.GetAlbumDetailAsync("999"));
        Assert.Null(await svc.GetAlbumDetailAsync("999"));

        // One call only: a definitive answer is allowed to stick.
        Assert.Equal(1, calls("/album/999"));
    }

    /// <summary>getAlbum lists the songs filed under an album when Deezer answered that it has
    /// no such album or no tracks for it, never when it failed to answer. So the lookup says
    /// which, and a cached "no such album" still says so.</summary>
    [Fact]
    public async Task LookUpAlbumDetailAsync_SaysWhyThereIsNoDetail()
    {
        var svc = BuildSequencedService(new()
        {
            ("/album/711108/tracks", new[] { QuotaEnvelope, @"{""data"":[]}", TracksJson(10) }),
            ("/album/711108", new[] { AlbumDetailJson }),
            // The tracklist is asked beside the album, and dropped when there is no album.
            ("/album/999/tracks", new[] { @"{""data"":[]}" }),
            ("/album/999", new[] { NoDataEnvelope }),
        }, out var calls);

        Assert.Equal(DeezerMetadataService.AlbumAnswer.Unavailable, (await svc.LookUpAlbumDetailAsync("711108")).Answer);
        Assert.Equal(DeezerMetadataService.AlbumAnswer.NoTracks, (await svc.LookUpAlbumDetailAsync("711108")).Answer);
        var found = await svc.LookUpAlbumDetailAsync("711108");
        Assert.Equal(DeezerMetadataService.AlbumAnswer.Found, found.Answer);
        Assert.Equal(10, found.Detail!.Tracks.Count);

        Assert.Equal(DeezerMetadataService.AlbumAnswer.NoSuchAlbum, (await svc.LookUpAlbumDetailAsync("999")).Answer);
        Assert.Equal(DeezerMetadataService.AlbumAnswer.NoSuchAlbum, (await svc.LookUpAlbumDetailAsync("999")).Answer);
        Assert.Equal(1, calls("/album/999"));
        Assert.Equal(1, calls("/album/999/tracks"));
    }

    /// <summary>
    /// A throttled album search used to cache an empty list, so external albums silently
    /// stopped appearing in search3 for the life of the process.
    /// </summary>
    [Fact]
    public async Task SearchAlbumsAsync_Throttled_NotCachedAndRecoversOnRetry()
    {
        const string results =
            @"{""data"":[{""id"":711108,""title"":""Canciones Prohibidas"",""record_type"":""album"",
               ""nb_tracks"":10,""cover_xl"":""https://cdn/xl.jpg"",""artist"":{""name"":""Extremoduro""}}]}";

        var svc = BuildSequencedService(new()
        {
            ("/search/album", new[] { QuotaEnvelope, results }),
        }, out _);

        Assert.Empty(await svc.SearchAlbumsAsync("extremoduro golfa", 10));

        var second = await svc.SearchAlbumsAsync("extremoduro golfa", 10);
        Assert.Single(second);
        Assert.Equal("711108", second[0].DeezerId);
    }

    /// <summary>
    /// Entries now expire on their own, but a definitive negative still sticks for its TTL.
    /// ClearCaches is the lever that turns "wait for the TTL" into "fixed now", so it has
    /// to actually drop cached answers.
    /// </summary>
    [Fact]
    public async Task ClearCaches_ForcesARefetch()
    {
        var svc = BuildSequencedService(new()
        {
            ("/album/999/tracks", new[] { TracksJson(3) }),
            ("/album/999", new[] { NoDataEnvelope, AlbumDetailJson }),
        }, out var calls);

        Assert.Null(await svc.GetAlbumDetailAsync("999"));
        Assert.Null(await svc.GetAlbumDetailAsync("999"));
        Assert.Equal(1, calls("/album/999"));

        svc.ClearCaches();

        Assert.NotNull(await svc.GetAlbumDetailAsync("999"));
        Assert.Equal(2, calls("/album/999"));
    }

    // ---- Metadata language (issue #24) ---------------------------------------
    // Deezer localizes genre names to the caller's IP country, so the request
    // must pin the language or a non-English host writes localized genre tags.

    [Fact]
    public async Task Requests_CarryConfiguredAcceptLanguage()
    {
        var seen = new List<HttpRequestMessage>();
        var svc = BuildService(new(), language: "en", capture: seen);

        await svc.GetAlbumDetailAsync("1");

        Assert.NotEmpty(seen);
        Assert.All(seen, r => Assert.Contains(r.Headers.AcceptLanguage, v => v.Value == "en"));
    }

    [Fact]
    public async Task Requests_OmitAcceptLanguage_WhenLanguageEmpty()
    {
        var seen = new List<HttpRequestMessage>();
        var svc = BuildService(new(), language: "", capture: seen);

        await svc.GetAlbumDetailAsync("1");

        Assert.NotEmpty(seen);
        Assert.All(seen, r => Assert.Empty(r.Headers.AcceptLanguage));
    }

    /// <summary>A throttled track search must not be cached as "this track has no metadata".</summary>
    [Fact]
    public async Task EnrichTrackAsync_Throttled_NotCachedAndRecoversOnRetry()
    {
        const string track =
            @"{""data"":[{""title"":""Golfa"",""duration"":359,
               ""album"":{""id"":711108,""title"":""Canciones Prohibidas"",""cover_xl"":""https://cdn/xl.jpg""},
               ""artist"":{""name"":""Extremoduro""}}]}";

        var svc = BuildSequencedService(new()
        {
            ("/search?q=", new[] { QuotaEnvelope, track }),
            ("/album/711108", new[] { AlbumDetailJson }),
        }, out _);

        Assert.Null(await svc.EnrichTrackAsync("Extremoduro", "Golfa"));

        var second = await svc.EnrichTrackAsync("Extremoduro", "Golfa");
        Assert.NotNull(second);
        Assert.Equal("Canciones Prohibidas", second!.AlbumTitle);
    }

    // ---- Plain-query regression (Deezer dropped field-qualified search) ------
    // Octo used to ask for artist:"X" track:"Y". Deezer now reads that as free text, so
    // the literal words "artist" and "track" had to appear in the record and nothing ever
    // matched: every external song lost its album, year and duration and fell back to a
    // flat 180s, and every download was written with bare tags. These tests assert the
    // query SHAPE, because the stub routes on path alone and stayed green throughout.

    [Fact]
    public async Task EnrichTrackAsync_SendsPlainTerms_WithoutFieldQualifiers()
    {
        var json = @"{""data"":[{""title"":""Reckoner"",""duration"":290,
            ""album"":{""id"":1,""title"":""In Rainbows""},""artist"":{""name"":""Radiohead""}}]}";
        var sent = new List<HttpRequestMessage>();
        var svc = BuildService(new() { ["/search"] = json }, capture: sent);

        await svc.EnrichTrackAsync("Radiohead", "Reckoner", includeYear: false);

        var query = Uri.UnescapeDataString(sent[0].RequestUri!.Query);
        Assert.DoesNotContain("artist:", query);
        Assert.DoesNotContain("track:", query);
        Assert.Contains("Radiohead Reckoner", query);
    }

    [Fact]
    public async Task EnrichTrackFullAsync_SendsPlainTerms_WithoutFieldQualifiers()
    {
        var json = @"{""data"":[{""title"":""Teardrop"",""duration"":330,""isrc"":""X"",
            ""album"":{""id"":1,""title"":""Mezzanine""},""artist"":{""name"":""Massive Attack""}}]}";
        var sent = new List<HttpRequestMessage>();
        var svc = BuildService(new() { ["/album/1"] = @"{""id"":1}", ["/search"] = json }, capture: sent);

        await svc.EnrichTrackFullAsync("Massive Attack", "Teardrop");

        var query = Uri.UnescapeDataString(sent[0].RequestUri!.Query);
        Assert.DoesNotContain("artist:", query);
        Assert.DoesNotContain("track:", query);
    }

    /// <summary>
    /// The search hit names only the main artist and carries no track position, so a
    /// collaboration was tagged as one artist (#49) and the track number was never written (#48).
    /// Both come from the track's own record. Shape taken from a live answer, 2026-09-25.
    /// </summary>
    [Fact]
    public async Task EnrichTrackFullAsync_ReadsContributorsAndPositionFromTheTrack()
    {
        var search = @"{""data"":[{""id"":2334934765,""title"":""Rauw Alejandro: Bzrp Music Sessions, Vol. 56/66"",
            ""duration"":210,""album"":{""id"":1,""title"":""Session 56""},""artist"":{""name"":""Bizarrap""}}]}";
        var track = @"{""id"":2334934765,""track_position"":1,""disk_number"":1,""contributors"":[
            {""name"":""Bizarrap"",""role"":""Main""},{""name"":""Rauw Alejandro"",""role"":""Main""}]}";
        var svc = BuildService(new()
        {
            ["/track/2334934765"] = track,
            ["/album/1"] = @"{""id"":1,""nb_tracks"":1}",
            ["/search"] = search,
        });

        var meta = await svc.EnrichTrackFullAsync("Bizarrap, Rauw Alejandro", "Rauw Alejandro: Bzrp Music Sessions, Vol. 56/66");

        Assert.NotNull(meta);
        Assert.Equal("Bizarrap", meta.ArtistName);
        Assert.Equal(["Bizarrap", "Rauw Alejandro"], meta.Contributors);
        Assert.Equal(1, meta.TrackNumber);
        Assert.Equal(1, meta.DiscNumber);
    }

    /// <summary>
    /// Deezer reports most compilations as record_type "album" (checked live on three), so the
    /// album artist is the signal a compilation is recognised by, and it is the album artist
    /// rather than the track's that belongs in the album-artist tag.
    /// </summary>
    [Fact]
    public async Task EnrichTrackFullAsync_ReadsTheAlbumArtistAndRecordType()
    {
        var search = @"{""data"":[{""title"":""Song"",""duration"":200,
            ""album"":{""id"":5,""title"":""Summer Hits""},""artist"":{""name"":""Artist""}}]}";
        var album = @"{""id"":5,""record_type"":""album"",""artist"":{""id"":5080,""name"":""Various Artists""}}";
        var svc = BuildService(new() { ["/album/5"] = album, ["/search"] = search });

        var meta = await svc.EnrichTrackFullAsync("Artist", "Song");

        Assert.NotNull(meta);
        Assert.Equal("Various Artists", meta.AlbumArtistName);
        Assert.Equal("album", meta.RecordType);
        Assert.Equal("Artist", meta.ArtistName);
    }

    [Fact]
    public async Task FindAlbumIdAsync_SendsPlainTerms_WithoutFieldQualifiers()
    {
        var json = @"{""data"":[{""id"":7,""title"":""Discovery"",""artist"":{""name"":""Daft Punk""}}]}";
        var sent = new List<HttpRequestMessage>();
        var svc = BuildService(new() { ["/search/album"] = json }, capture: sent);

        await svc.FindAlbumIdAsync("Daft Punk", "Discovery");

        var query = Uri.UnescapeDataString(sent[0].RequestUri!.Query);
        Assert.DoesNotContain("artist:", query);
        Assert.DoesNotContain("album:", query);
    }

    // ---- The guard that makes a plain query safe --------------------------------

    [Fact]
    public async Task EnrichTrackAsync_SkipsAHitByAnotherArtist()
    {
        // A plain query is fuzzy enough to put a cover at position 0. Taking it would
        // attach the wrong album and length to the song as fact.
        var json = @"{""data"":[
            {""title"":""Creep"",""duration"":120,""album"":{""id"":9,""title"":""Karaoke Hits""},
             ""artist"":{""name"":""Karaoke All Stars""}},
            {""title"":""Creep"",""duration"":238,""album"":{""id"":1,""title"":""Pablo Honey""},
             ""artist"":{""name"":""Radiohead""}}]}";
        var svc = BuildService(new() { ["/search"] = json });

        var meta = await svc.EnrichTrackAsync("Radiohead", "Creep", includeYear: false);

        Assert.Equal("Pablo Honey", meta!.AlbumTitle);
        Assert.Equal(238, meta.Duration);
    }

    [Fact]
    public async Task EnrichTrackAsync_AcceptsADecoratedTitle()
    {
        // Deezer decorates titles; an exact compare would reject the right recording.
        var json = @"{""data"":[{""title"":""Reckoner (Remastered 2016)"",""duration"":290,
            ""album"":{""id"":1,""title"":""In Rainbows""},""artist"":{""name"":""Radiohead""}}]}";
        var svc = BuildService(new() { ["/search"] = json });

        var meta = await svc.EnrichTrackAsync("Radiohead", "Reckoner", includeYear: false);

        Assert.Equal(290, meta!.Duration);
    }

    [Fact]
    public async Task EnrichTrackAsync_RejectsAHitThatMatchesNothing()
    {
        // A hit stating neither the artist nor the title is not a match by default.
        var json = @"{""data"":[{""duration"":111,""album"":{""id"":9,""title"":""Something Else""}}]}";
        var svc = BuildService(new() { ["/search"] = json });

        Assert.Null(await svc.EnrichTrackAsync("Radiohead", "Reckoner", includeYear: false));
    }

    // ---- A throttled year must not cost the whole track -------------------------

    [Fact]
    public async Task EnrichTrackAsync_ThrottledYear_KeepsAlbumAndDuration()
    {
        // The album detail carries only the year. Returning null when it was throttled
        // threw away an album title and duration already in hand, which is what left a
        // song with no length at all.
        var track = @"{""data"":[{""title"":""Creep"",""duration"":238,
            ""album"":{""id"":1,""title"":""Pablo Honey""},""artist"":{""name"":""Radiohead""}}]}";
        var svc = BuildService(new() { ["/album/1"] = QuotaEnvelope, ["/search"] = track });

        var meta = await svc.EnrichTrackAsync("Radiohead", "Creep", includeYear: true);

        Assert.NotNull(meta);
        Assert.Equal("Pablo Honey", meta!.AlbumTitle);
        Assert.Equal(238, meta.Duration);
        Assert.Null(meta.Year);
    }

    [Fact]
    public async Task EnrichTrackAsync_ThrottledYear_IsNotCached()
    {
        // ...and because the year is still unknown, the answer must not be remembered,
        // or one throttle blip becomes "this track has no year" for the life of the entry.
        var svc = BuildSequencedService(new()
        {
            ("/album/1", new[] { QuotaEnvelope, @"{""id"":1,""release_date"":""1993-02-22""}" }),
            ("/search", new[] { @"{""data"":[{""title"":""Creep"",""duration"":238,
                ""album"":{""id"":1,""title"":""Pablo Honey""},""artist"":{""name"":""Radiohead""}}]}" }),
        }, out _);

        Assert.Null((await svc.EnrichTrackAsync("Radiohead", "Creep"))!.Year);
        Assert.Equal(1993, (await svc.EnrichTrackAsync("Radiohead", "Creep"))!.Year);
    }
    // ---- External artist search -------------------------------------------------
    // search3's merge has always known how to fold external artists in, but nothing
    // populated the list, so the artist column only ever showed the local library.

    [Fact]
    public async Task SearchArtistsAsync_MapsNameImageAndAlbumCount()
    {
        var json = @"{""data"":[
            {""id"":399,""name"":""Radiohead"",""picture_xl"":""https://cdn/r.jpg"",""nb_album"":24,""nb_fan"":6100000},
            {""id"":27,""name"":""Daft Punk"",""picture_medium"":""https://cdn/d.jpg"",""nb_album"":11}]}";
        var svc = BuildService(new() { ["/search/artist"] = json });

        var hits = await svc.SearchArtistsAsync("radiohead", 10);

        Assert.Equal(2, hits.Count);
        Assert.Equal("399", hits[0].DeezerId);
        Assert.Equal("Radiohead", hits[0].Name);
        Assert.Equal("https://cdn/r.jpg", hits[0].PictureUrl);
        Assert.Equal(24, hits[0].AlbumCount);
        // What tells two artists of one name apart when nothing better is known.
        Assert.Equal(6100000, hits[0].Fans);
        Assert.Equal(0, hits[1].Fans);
        // Falls back to the medium picture when there is no xl.
        Assert.Equal("https://cdn/d.jpg", hits[1].PictureUrl);
    }

    [Fact]
    public async Task SearchArtistsAsync_Throttled_ReturnsEmptyWithoutCaching()
    {
        // Caching an empty list on a refusal is what would make external artists vanish
        // from search3 for the rest of the process (issue #8's shape).
        var svc = BuildSequencedService(new()
        {
            ("/search/artist", new[] { QuotaEnvelope, @"{""data"":[{""id"":399,""name"":""Radiohead""}]}" }),
        }, out _);

        Assert.Empty(await svc.SearchArtistsAsync("radiohead", 10));
        Assert.Single(await svc.SearchArtistsAsync("radiohead", 10));
    }

    [Fact]
    public async Task SearchArtistsAsync_SendsPlainQuery()
    {
        var sent = new List<HttpRequestMessage>();
        var svc = BuildService(new() { ["/search/artist"] = @"{""data"":[]}" }, capture: sent);

        await svc.SearchArtistsAsync("radiohead", 10);

        Assert.DoesNotContain("artist:", Uri.UnescapeDataString(sent[0].RequestUri!.Query));
    }

    [Fact]
    public async Task GetArtistAlbumsAsync_GroupsAlbumsThenEpsThenSinglesThenCompilations()
    {
        // The catalog's own shape for an artist's releases: no artist and no track counts,
        // a clean and an explicit copy of one album, a single, and a compilation of theirs.
        var json = @"{""data"":[
            {""id"":1,""title"":""First"",""record_type"":""album"",""release_date"":""1997-01-20"",""cover_xl"":""https://cdn/1.jpg""},
            {""id"":2,""title"":""Second"",""record_type"":""album"",""release_date"":""2001-03-12""},
            {""id"":3,""title"":""Second"",""record_type"":""album"",""release_date"":""2001-03-12""},
            {""id"":4,""title"":""A Single"",""record_type"":""single"",""release_date"":""2005-01-01""},
            {""id"":5,""title"":""The Best Of"",""record_type"":""compile"",""release_date"":""2010-06-01""},
            {""id"":6,""title"":""Live Set"",""record_type"":""ep"",""release_date"":""2003-09-09""}
        ]}";
        var svc = BuildService(new() { ["/artist/42/albums"] = json });

        var hits = await svc.GetArtistAlbumsAsync("42", "Test Artist");

        // The way the apps group a discography, newest first within each group.
        Assert.Equal(new[] { "Second", "First", "Live Set", "A Single", "The Best Of" }, hits.Select(h => h.Title));
        Assert.Equal(new[] { "album", "album", "ep", "single", "compile" }, hits.Select(h => h.RecordType));
        Assert.All(hits, h => Assert.Equal("Test Artist", h.Artist));
        Assert.Equal("2", hits.Single(h => h.Title == "Second").DeezerId);
        Assert.Equal(1997, hits.Single(h => h.Title == "First").Year);
        Assert.Equal("https://cdn/1.jpg", hits.Single(h => h.Title == "First").CoverUrl);
    }

    [Fact]
    public async Task GetArtistAlbumsAsync_SinglesAreNewestFirstToo()
    {
        var json = @"{""data"":[
            {""id"":1,""title"":""Song A"",""record_type"":""single"",""release_date"":""2020-01-01""},
            {""id"":2,""title"":""Song B"",""record_type"":""single"",""release_date"":""2022-01-01""}
        ]}";
        var svc = BuildService(new() { ["/artist/9/albums"] = json });

        var hits = await svc.GetArtistAlbumsAsync("9", "New Artist");

        Assert.Equal(new[] { "Song B", "Song A" }, hits.Select(h => h.Title));
    }

    [Fact]
    public async Task GetArtistAlbumsAsync_ACareerOfSinglesShowsThemAfterItsAlbum()
    {
        // Singles used to be hidden whenever there was one album, which hid most of a career
        // like this one.
        var json = @"{""data"":[
            {""id"":1,""title"":""Hit Three"",""record_type"":""single"",""release_date"":""2024-01-01""},
            {""id"":2,""title"":""Hit Two"",""record_type"":""single"",""release_date"":""2023-01-01""},
            {""id"":3,""title"":""The Album"",""record_type"":""album"",""release_date"":""2021-01-01""},
            {""id"":4,""title"":""Hit One"",""record_type"":""single"",""release_date"":""2020-01-01""}
        ]}";
        var svc = BuildService(new() { ["/artist/9/albums"] = json });

        var hits = await svc.GetArtistAlbumsAsync("9", "Singles Artist");

        Assert.Equal(new[] { "The Album", "Hit Three", "Hit Two", "Hit One" }, hits.Select(h => h.Title));
    }

    [Theory]
    [InlineData("album", "album")]
    [InlineData("ep", "ep")]
    [InlineData("single", "single")]
    [InlineData("compile", "album,compilation")]
    [InlineData("compilation", "album,compilation")]
    [InlineData("ALBUM", "album")]
    [InlineData("mixtape", "")]
    [InlineData(null, "")]
    public void ReleaseTypes_AreNavidromesLowercaseWords(string? recordType, string expected)
        => Assert.Equal(expected, string.Join(",", DeezerMetadataService.ReleaseTypes(recordType)));

    [Theory]
    [InlineData("album", "ep", "single")]
    [InlineData("single", "ep", "album")]
    [InlineData("ep", "album", "single")]
    public async Task GetArtistAlbumsAsync_OneTitleIsOneRow_TheAlbumKept(string first, string second, string third)
    {
        // An album and its own EP or single can share a title. An outside album's id is made
        // from the artist and title, so both rows would open one release: whichever came last.
        var json = $@"{{""data"":[
            {{""id"":1,""title"":""Same Name"",""record_type"":""{first}"",""release_date"":""2020-01-01""}},
            {{""id"":2,""title"":""Same Name"",""record_type"":""{second}"",""release_date"":""2020-01-01""}},
            {{""id"":3,""title"":""same name!"",""record_type"":""{third}"",""release_date"":""2019-01-01""}}
        ]}}";
        var svc = BuildService(new() { ["/artist/9/albums"] = json });

        var hit = Assert.Single(await svc.GetArtistAlbumsAsync("9", "Some Artist"));

        Assert.Equal("album", hit.RecordType);
        Assert.Equal(new[] { first, second, third }.ToList().IndexOf("album") + 1, int.Parse(hit.DeezerId));
    }

    [Fact]
    public async Task GetArtistAlbumsAsync_SaysSoWhenTheCatalogHasMoreThanTheListing()
    {
        var logger = new Mock<ILogger<DeezerMetadataService>>();
        var json = @"{""total"":250,""data"":[
            {""id"":1,""title"":""Newest"",""record_type"":""album"",""release_date"":""2024-01-01""}]}";
        var svc = BuildService(new() { ["/artist/9/albums"] = json }, logger: logger.Object);

        Assert.Single(await svc.GetArtistAlbumsAsync("9", "Long Career"));

        logger.Verify(l => l.Log(LogLevel.Warning, It.IsAny<EventId>(),
            It.Is<It.IsAnyType>((state, _) => state.ToString()!.Contains("has 250 releases; the page lists the first 1")),
            It.IsAny<Exception?>(), It.IsAny<Func<It.IsAnyType, Exception?, string>>()), Times.Once);
    }

    [Fact]
    public async Task GetArtistAlbumsAsync_AWholeListingWarnsOfNothing()
    {
        var logger = new Mock<ILogger<DeezerMetadataService>>();
        var json = @"{""total"":1,""data"":[
            {""id"":1,""title"":""Only"",""record_type"":""album"",""release_date"":""2024-01-01""}]}";
        var svc = BuildService(new() { ["/artist/9/albums"] = json }, logger: logger.Object);

        await svc.GetArtistAlbumsAsync("9", "Short Career");

        logger.Verify(l => l.Log(LogLevel.Warning, It.IsAny<EventId>(), It.IsAny<It.IsAnyType>(),
            It.IsAny<Exception?>(), It.IsAny<Func<It.IsAnyType, Exception?, string>>()), Times.Never);
    }

    // ---- Ranked candidates for the release chooser ----------------------------------------

    private const string TwoHitSearch = @"{""data"":[
        {""id"":11,""title"":""Teardrop"",""duration"":330,""isrc"":""GBAAA9800001"",""explicit_lyrics"":false,
         ""album"":{""id"":1,""title"":""Mezzanine"",""cover_xl"":""https://cdn/mezz.jpg""},""artist"":{""name"":""Massive Attack""}},
        {""id"":22,""title"":""Teardrop"",""duration"":330,""isrc"":""GBAAA9800001"",
         ""album"":{""id"":2,""title"":""Collected"",""cover_xl"":""https://cdn/col.jpg""},""artist"":{""name"":""Massive Attack""}},
        {""id"":33,""title"":""Teardrop (Live)"",""duration"":340,
         ""album"":{""id"":3,""title"":""Live at Wembley""},""artist"":{""name"":""Massive Attack""}}
    ]}";

    [Fact]
    public async Task EnrichTrackCandidatesAsync_DetailFetchesTheBestTwo_AndReadsTheNewFields()
    {
        var sent = new List<HttpRequestMessage>();
        var svc = BuildService(new()
        {
            ["/album/1"] = @"{""id"":1,""record_type"":""album"",""upc"":""724384559922"",""label"":""Virgin"",""release_date"":""1998-04-20"",""nb_tracks"":11,""artist"":{""name"":""Massive Attack""}}",
            ["/album/2"] = @"{""id"":2,""record_type"":""compile"",""upc"":""094636482323"",""release_date"":""2006-03-27"",""artist"":{""name"":""Massive Attack""}}",
            ["/album/3"] = @"{""id"":3,""record_type"":""album""}",
            ["/track/11"] = @"{""id"":11,""track_position"":3,""disk_number"":1,""gain"":-9.8,""explicit_lyrics"":false,""contributors"":[{""name"":""Massive Attack"",""role"":""Main""},{""name"":""Elizabeth Fraser"",""role"":""Featured""}]}",
            ["/track/22"] = @"{""id"":22,""track_position"":7,""disk_number"":1}",
            ["/search"] = TwoHitSearch,
        }, capture: sent);

        var answer = await svc.EnrichTrackCandidatesAsync("Massive Attack", "Teardrop", max: 2);

        Assert.False(answer.DidNotAnswer);
        Assert.Equal(2, answer.Hits.Count);
        var mezzanine = answer.Hits[0];
        Assert.Equal("Teardrop", mezzanine.Title);
        Assert.Equal("11", mezzanine.TrackId);
        Assert.Equal("1", mezzanine.AlbumId);
        Assert.Equal("724384559922", mezzanine.Barcode);
        Assert.Equal(false, mezzanine.ExplicitLyrics);
        Assert.Equal(-9.8, mezzanine.CatalogGain);
        Assert.Equal("album", mezzanine.RecordType);
        Assert.Equal(["Massive Attack", "Elizabeth Fraser"], mezzanine.Contributors);
        Assert.Equal("compile", answer.Hits[1].RecordType);
        // The live take contradicts the title, so it is never a hit, and the best two cost two album details.
        Assert.Equal(2, sent.Count(r => r.RequestUri!.AbsolutePath.StartsWith("/album/")));
        Assert.DoesNotContain(sent, r => r.RequestUri!.AbsolutePath == "/album/3");
    }

    [Fact]
    public async Task EnrichTrackCandidatesAsync_OneHit_CostsOneAlbumDetail_AndIsRemembered()
    {
        var sent = new List<HttpRequestMessage>();
        var one = @"{""data"":[{""id"":11,""title"":""Teardrop"",""duration"":330,""album"":{""id"":1,""title"":""Mezzanine""},""artist"":{""name"":""Massive Attack""}}]}";
        var svc = BuildService(new() { ["/album/1"] = @"{""id"":1,""record_type"":""album""}", ["/track/11"] = @"{""id"":11}", ["/search"] = one }, capture: sent);

        var first = await svc.EnrichTrackCandidatesAsync("Massive Attack", "Teardrop", max: 2);
        var second = await svc.EnrichTrackCandidatesAsync("Massive Attack", "Teardrop", max: 2);

        Assert.Single(first.Hits);
        Assert.Same(first, second);
        Assert.Equal(1, sent.Count(r => r.RequestUri!.AbsolutePath.StartsWith("/album/")));
    }

    /// <summary>A throttled catalog gives no candidate, says so, and leaves nothing in the cache
    /// to repeat the throttle for the next twelve hours.</summary>
    [Fact]
    public async Task EnrichTrackCandidatesAsync_Throttled_AnswersNothingAndRemembersNothing()
    {
        var svc = BuildSequencedService(new()
        {
            ("/album/1", [@"{""id"":1,""record_type"":""album""}"]),
            ("/track/11", [@"{""id"":11}"]),
            ("/search", [QuotaEnvelope, @"{""data"":[{""id"":11,""title"":""Teardrop"",""duration"":330,""album"":{""id"":1,""title"":""Mezzanine""},""artist"":{""name"":""Massive Attack""}}]}"]),
        }, out var calls);

        var throttled = await svc.EnrichTrackCandidatesAsync("Massive Attack", "Teardrop");
        var later = await svc.EnrichTrackCandidatesAsync("Massive Attack", "Teardrop");

        Assert.True(throttled.DidNotAnswer);
        Assert.Empty(throttled.Hits);
        Assert.False(later.DidNotAnswer);
        Assert.Single(later.Hits);
        Assert.Equal(2, calls("/search"));
    }
}
