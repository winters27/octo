using System.Net;
using System.Text.Json;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Logging.Abstractions;
using Moq;
using Moq.Protected;
using Octo.Models.Settings;
using Octo.Services.Lyrics;
using Octo.Services.Soulseek;

namespace Octo.Tests;

/// <summary>
/// Lyrics (#52): synced before plain, NetEase's credit block removed, a busy service never
/// remembered as "no lyrics", and a file that already has lyrics left alone.
/// </summary>
public sealed class LyricsTests : IDisposable
{
    private readonly string _root = Path.Combine(Path.GetTempPath(), "octo-lyrics-" + Guid.NewGuid().ToString("N"));

    public LyricsTests() => Directory.CreateDirectory(_root);

    public void Dispose()
    {
        try { Directory.Delete(_root, recursive: true); } catch { /* best effort */ }
    }

    // ---- LyricsText ------------------------------------------------------------------------

    [Fact]
    public void ParseLrc_ReadsEveryTagAndFractionAndSorts()
    {
        var lines = LyricsText.ParseLrc("[ar:Someone]\n[00:12.5]second\n[00:01.25][00:30.125]both\n[01:02]third");

        Assert.Equal([1250L, 12500L, 30125L, 62000L], lines.Select(line => line.StartMs));
        Assert.Equal(["both", "second", "both", "third"], lines.Select(line => line.Text));
    }

    /// <summary>The credit block from a real NetEase lyric (Jay Chou), as captured live 2026-09-25.</summary>
    [Fact]
    public void StripNeteaseCredits_DropsTheLeadingCreditBlock()
    {
        var lrc = string.Join('\n',
            "[00:00.000] 作词 : 周杰伦",
            "[00:01.000] 作曲 : 周杰伦",
            "[00:02.000] 编曲 : 林迈可",
            "[00:03.00] 制作人 : 周杰伦",
            "[00:04.00]词版权管理方：杰威尔",
            "[00:20.50]窗外的麻雀 在电线杆上多嘴",
            "[00:24.00]你说这一句 很有夏天的感觉");

        var clean = LyricsText.StripNeteaseCredits(lrc);

        Assert.DoesNotContain("作词", clean);
        Assert.DoesNotContain("编曲", clean);
        Assert.DoesNotContain("版权", clean);
        Assert.Contains("窗外的麻雀", clean);
        Assert.Contains("很有夏天的感觉", clean);
    }

    [Fact]
    public void StripNeteaseCredits_DropsJsonLines()
    {
        var lrc = "{\"t\":0,\"c\":[{\"tx\":\"作词: \"},{\"tx\":\"唐恬\"}]}\n[00:10.00]first line";
        Assert.Equal("[00:10.00]first line", LyricsText.StripNeteaseCredits(lrc));
    }

    /// <summary>An English lyric that happens to have a colon is a lyric, even near the start.</summary>
    [Theory]
    [InlineData("[00:05.00]Stop: don't you go")]
    [InlineData("[00:06.00]Baby: I'm yours")]
    [InlineData("[01:30.00]Hope: it's all we have")]
    public void StripNeteaseCredits_KeepsAnEnglishLyricWithAColon(string line)
        => Assert.Equal(line, LyricsText.StripNeteaseCredits(line));

    [Fact]
    public void StripNeteaseCredits_DropsAMidSongPublisherLine()
    {
        var lrc = "[00:12.00]a line\n[00:13.47]出品：网易飓风 X索尼音乐\n[00:15.00]another line";
        Assert.Equal("[00:12.00]a line\n[00:15.00]another line", LyricsText.StripNeteaseCredits(lrc));
    }

    [Theory]
    [InlineData("Song (feat. Guest)", "Song")]
    [InlineData("Song (Live) [Official Video]", "Song (Live)")]
    [InlineData("Massive Attack - Teardrop", "Teardrop")]
    public void QueryTitle_KeepsTheVersionButNotTheGuestOrTheNoise(string title, string expected)
        => Assert.Equal(expected, LyricsText.QueryTitle(title, "Massive Attack"));

    // ---- LRCLIB ----------------------------------------------------------------------------

    private static IHttpClientFactory Http(Func<HttpRequestMessage, HttpResponseMessage> answer, List<string>? calls = null)
    {
        var handler = new Mock<HttpMessageHandler>();
        handler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync", ItExpr.IsAny<HttpRequestMessage>(), ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync((HttpRequestMessage request, CancellationToken _) =>
            {
                calls?.Add(request.RequestUri!.ToString());
                return answer(request);
            });
        var factory = new Mock<IHttpClientFactory>();
        factory.Setup(f => f.CreateClient(It.IsAny<string>())).Returns(() => new HttpClient(handler.Object));
        return factory.Object;
    }

    private static HttpResponseMessage Json(string body, HttpStatusCode status = HttpStatusCode.OK) =>
        new(status) { Content = new StringContent(body) };

    [Fact]
    public async Task Lrclib_Found_ReadsSyncedAndPlain()
    {
        var source = new LrclibLyricsSource(Http(_ => Json(
            """{"trackName":"Teardrop","artistName":"Massive Attack","duration":331.0,"instrumental":false,"syncedLyrics":"[01:02.38] Love, love is a verb","plainLyrics":"Love, love is a verb"}""")),
            NullLogger<LrclibLyricsSource>.Instance);

        var lookup = await source.FindAsync(new LyricsQuery("Massive Attack", "Teardrop", "Mezzanine", 331), CancellationToken.None);

        Assert.False(lookup.Transient);
        Assert.True(lookup.Result!.HasSynced);
        Assert.Equal("LRCLIB", lookup.Result.Source);
    }

    [Fact]
    public async Task Lrclib_404_FallsBackToSearchWithinTwoSeconds()
    {
        var calls = new List<string>();
        var source = new LrclibLyricsSource(Http(request => request.RequestUri!.AbsolutePath.EndsWith("/get")
            ? Json("""{"code":404}""", HttpStatusCode.NotFound)
            : Json("""
              [{"trackName":"Teardrop","artistName":"Massive Attack","duration":400,"plainLyrics":"wrong length"},
               {"trackName":"Teardrop","artistName":"Massive Attack","duration":331.5,"plainLyrics":"right one"}]
              """), calls), NullLogger<LrclibLyricsSource>.Instance);

        var lookup = await source.FindAsync(new LyricsQuery("Massive Attack", "Teardrop", null, 331), CancellationToken.None);

        Assert.Equal("right one", lookup.Result!.Plain);
        Assert.Equal(2, calls.Count);
    }

    /// <summary>A 503 is "not now", never "no lyrics", and LRCLIB asks callers to honour Retry-After.</summary>
    [Fact]
    public async Task Lrclib_Overloaded_CoolsDownWithoutAnotherRequest()
    {
        var calls = new List<string>();
        var source = new LrclibLyricsSource(Http(_ =>
        {
            var busy = Json("""{"message":"The server is busy"}""", HttpStatusCode.ServiceUnavailable);
            busy.Headers.RetryAfter = new System.Net.Http.Headers.RetryConditionHeaderValue(TimeSpan.FromMinutes(5));
            return busy;
        }, calls), NullLogger<LrclibLyricsSource>.Instance);

        var first = await source.FindAsync(new LyricsQuery("A", "T", null, null), CancellationToken.None);
        var second = await source.FindAsync(new LyricsQuery("B", "U", null, null), CancellationToken.None);

        Assert.True(first.Transient);
        Assert.True(second.Transient);
        Assert.Single(calls);
    }

    [Fact]
    public async Task Lrclib_AlbumThatIsOnlyTheTitle_IsNotSent()
    {
        var calls = new List<string>();
        var source = new LrclibLyricsSource(Http(_ => Json("{}", HttpStatusCode.NotFound), calls),
            NullLogger<LrclibLyricsSource>.Instance);

        await source.FindAsync(new LyricsQuery("A", "Single", "Single", 200), CancellationToken.None);

        Assert.DoesNotContain("album_name", calls[0]);
    }

    // ---- LyricsService ---------------------------------------------------------------------

    private sealed class FakeSource(string key, Func<LyricsLookup> answer) : ILyricsSource
    {
        public string Key => key;
        public int Calls { get; private set; }

        public Task<LyricsLookup> FindAsync(LyricsQuery query, CancellationToken ct)
        {
            Calls++;
            return Task.FromResult(answer());
        }
    }

    private static LyricsService Service(string sources, params ILyricsSource[] all) =>
        new(all, TestOptions.Monitor(new MetadataSettings { LyricsSources = sources }), NullLogger<LyricsService>.Instance);

    private static readonly LyricsQuery Query = new("Artist", "Song", null, 200);

    [Fact]
    public async Task Service_SyncedFromALaterSource_BeatsAnEarlierPlainOne()
    {
        var plain = new FakeSource("lrclib", () => new LyricsLookup(new LyricsResult("first", null, "plain words", false), false));
        var synced = new FakeSource("netease", () => new LyricsLookup(new LyricsResult("second", "[00:01.00]timed", null, false), false));

        var lookup = await Service("lrclib,netease", plain, synced).FindAsync(Query, CancellationToken.None);

        Assert.Equal("second", lookup.Result!.Source);
    }

    [Fact]
    public async Task Service_NothingSynced_KeepsThePlainOne()
    {
        var plain = new FakeSource("lyricsovh", () => new LyricsLookup(new LyricsResult("ovh", null, "words", false), false));
        var none = new FakeSource("lrclib", () => LyricsLookup.Miss);

        var lookup = await Service("lrclib,lyricsovh", none, plain).FindAsync(Query, CancellationToken.None);

        Assert.Equal("ovh", lookup.Result!.Source);
    }

    [Fact]
    public async Task Service_SourceNotListed_IsNeverAsked()
    {
        var netease = new FakeSource("netease", () => LyricsLookup.Miss);
        var lrclib = new FakeSource("lrclib", () => LyricsLookup.Miss);

        await Service("lrclib,lyricsovh", lrclib, netease).FindAsync(Query, CancellationToken.None);

        Assert.Equal(0, netease.Calls);
        Assert.Equal(1, lrclib.Calls);
    }

    [Fact]
    public async Task Service_BusySource_IsNotRememberedAsNoLyrics()
    {
        var busy = true;
        var source = new FakeSource("lrclib", () => busy
            ? LyricsLookup.Failed
            : new LyricsLookup(new LyricsResult("lrclib", "[00:01.00]x", null, false), false));
        var service = Service("lrclib", source);

        Assert.True((await service.FindAsync(Query, CancellationToken.None)).Transient);
        busy = false;
        Assert.True((await service.FindAsync(Query, CancellationToken.None)).Result!.HasSynced);
    }

    // ---- LyricsSidecarWriter ---------------------------------------------------------------

    private string Audio(string name = "Artist - Song.mp3")
    {
        var path = Path.Combine(_root, name);
        File.WriteAllBytes(path, AudioFixtures.Mp3());
        return path;
    }

    private static LyricsSidecarWriter Writer(Func<LyricsLookup> answer) =>
        new(Service("lrclib", new FakeSource("lrclib", answer)), NullLogger<LyricsSidecarWriter>.Instance);

    [Fact]
    public async Task Writer_Synced_WritesAnLrcBesideTheFile()
    {
        var audio = Audio();
        var outcome = await Writer(() => new LyricsLookup(new LyricsResult("lrclib", "[00:01.00]line", "line", false), false))
            .WriteAsync(new LyricsJob(audio, "Artist", "Song", null, 200), CancellationToken.None);

        Assert.Equal(LyricsWriteOutcome.Written, outcome);
        Assert.Equal(LyricsSidecarWriter.OctoMark + "\n[00:01.00]line\n", File.ReadAllText(Path.ChangeExtension(audio, ".lrc")));
        Assert.False(File.Exists(Path.ChangeExtension(audio, ".txt")));
    }

    [Fact]
    public async Task Writer_PlainOnly_WritesATxt()
    {
        var audio = Audio();
        await Writer(() => new LyricsLookup(new LyricsResult("ovh", null, "words", false), false))
            .WriteAsync(new LyricsJob(audio, "Artist", "Song", null, 200), CancellationToken.None);

        Assert.Equal("words\n", File.ReadAllText(Path.ChangeExtension(audio, ".txt")));
    }

    [Fact]
    public async Task Writer_Instrumental_WritesNothing()
    {
        var audio = Audio();
        var outcome = await Writer(() => new LyricsLookup(new LyricsResult("lrclib", null, null, true), false))
            .WriteAsync(new LyricsJob(audio, "Artist", "Song", null, 200), CancellationToken.None);

        Assert.Equal(LyricsWriteOutcome.Instrumental, outcome);
        Assert.Empty(Directory.GetFiles(_root, "*.lrc").Concat(Directory.GetFiles(_root, "*.txt")));
    }

    [Fact]
    public async Task Writer_ExistingSidecar_IsLeftAlone()
    {
        var audio = Audio();
        var existing = Path.ChangeExtension(audio, ".lrc");
        File.WriteAllText(existing, "mine");

        var outcome = await Writer(() => new LyricsLookup(new LyricsResult("lrclib", "[00:01.00]theirs", null, false), false))
            .WriteAsync(new LyricsJob(audio, "Artist", "Song", null, 200), CancellationToken.None);

        Assert.Equal(LyricsWriteOutcome.AlreadyThere, outcome);
        Assert.Equal("mine", File.ReadAllText(existing));
    }

    [Fact]
    public async Task Writer_EmbeddedLyrics_AreLeftAlone()
    {
        var audio = Audio();
        using (var file = TagLib.File.Create(audio))
        {
            file.Tag.Lyrics = "already here";
            file.Save();
        }

        var outcome = await Writer(() => new LyricsLookup(new LyricsResult("lrclib", "[00:01.00]x", null, false), false))
            .WriteAsync(new LyricsJob(audio, "Artist", "Song", null, 200), CancellationToken.None);

        Assert.Equal(LyricsWriteOutcome.AlreadyThere, outcome);
    }

    [Fact]
    public async Task Writer_BusyService_RetriesAndThenGivesUp()
    {
        var audio = Audio();
        var writer = Writer(() => LyricsLookup.Failed);

        Assert.Equal(LyricsWriteOutcome.Retrying,
            await writer.WriteAsync(new LyricsJob(audio, "Artist", "Song", null, 200, Attempt: 1), CancellationToken.None));
        Assert.Equal(LyricsWriteOutcome.GaveUp,
            await writer.WriteAsync(new LyricsJob(audio, "Artist", "Song", null, 200, Attempt: 3), CancellationToken.None));
    }

    // ---- getLyricsBySongId -----------------------------------------------------------------

    [Fact]
    public async Task GetLyricsBySongId_ExternalSong_ReturnsStructuredLyrics()
    {
        await using var factory = new LyricsWebFactory(fetch: true,
            new LyricsLookup(new LyricsResult("lrclib", "[00:01.50]first\n[00:03.00]second", null, false), false));
        var client = factory.CreateClient();
        var id = factory.Services.GetRequiredService<ExternalIdRegistry>().Register(new SoulseekRouting
        {
            Kind = RoutingKind.Song, Artist = "Some Artist", Title = "Some Song", Duration = 200,
        });

        var body = await client.GetStringAsync($"/rest/getLyricsBySongId?id={id}&f=json&u=alice&t=t&s=s");

        using var doc = JsonDocument.Parse(body);
        var lyrics = doc.RootElement.GetProperty("subsonic-response").GetProperty("lyricsList")
            .GetProperty("structuredLyrics")[0];
        Assert.True(lyrics.GetProperty("synced").GetBoolean());
        Assert.Equal("Some Artist", lyrics.GetProperty("displayArtist").GetString());
        var lines = lyrics.GetProperty("line").EnumerateArray().ToList();
        Assert.Equal(1500, lines[0].GetProperty("start").GetInt64());
        Assert.Equal("second", lines[1].GetProperty("value").GetString());
    }

    /// <summary>Off is exactly what shipped before: an empty but ok list, never "data not found".</summary>
    [Fact]
    public async Task GetLyricsBySongId_ExternalSong_FetchOff_IsAnEmptyList()
    {
        await using var factory = new LyricsWebFactory(fetch: false,
            new LyricsLookup(new LyricsResult("lrclib", "[00:01.00]x", null, false), false));
        var client = factory.CreateClient();
        var id = factory.Services.GetRequiredService<ExternalIdRegistry>().Register(new SoulseekRouting
        {
            Kind = RoutingKind.Song, Artist = "Some Artist", Title = "Other Song", Duration = 200,
        });

        var body = await client.GetStringAsync($"/rest/getLyricsBySongId?id={id}&f=json&u=alice&t=t&s=s");

        using var doc = JsonDocument.Parse(body);
        var response = doc.RootElement.GetProperty("subsonic-response");
        Assert.Equal("ok", response.GetProperty("status").GetString());
        Assert.Equal(0, response.GetProperty("lyricsList").GetProperty("structuredLyrics").GetArrayLength());
    }

    [Fact]
    public void LyricsListXml_CarriesTimedLines()
    {
        var builder = new Octo.Services.Subsonic.SubsonicResponseBuilder(new ExternalIdRegistry(),
            Microsoft.Extensions.Options.Options.Create(new SubsonicSettings()));

        var result = (Microsoft.AspNetCore.Mvc.ContentResult)builder.CreateLyricsListResponse("xml",
            new LyricsResult("lrclib", "[00:02.00]hello", null, false), "A", "T");

        Assert.Contains("<line start=\"2000\">hello</line>", result.Content);
        Assert.Contains("synced=\"true\"", result.Content);
    }

    private sealed class LyricsWebFactory(bool fetch, LyricsLookup answer) : WebApplicationFactory<Program>
    {
        private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-lyrics-web-" + Guid.NewGuid());

        protected override void ConfigureWebHost(IWebHostBuilder builder)
        {
            Directory.CreateDirectory(_directory);
            builder.ConfigureAppConfiguration((_, configuration) => configuration.AddInMemoryCollection(
                new Dictionary<string, string?>
                {
                    ["Subsonic:Url"] = "http://127.0.0.1:1",
                    ["Subsonic:AutoDetectDownloadPath"] = "false",
                    ["Library:DownloadPath"] = _directory,
                    ["Octo:StateDirectory"] = _directory,
                    ["Metadata:FetchLyrics"] = fetch ? "true" : "false",
                    ["Metadata:LyricsSources"] = "lrclib",
                }));
            builder.ConfigureServices(services =>
            {
                services.RemoveAll<IHostedService>();
                services.RemoveAll<ILyricsSource>();
                services.AddSingleton<ILyricsSource>(new FakeSource("lrclib", () => answer));
            });
        }

        public override async ValueTask DisposeAsync()
        {
            await base.DisposeAsync();
            try { Directory.Delete(_directory, recursive: true); } catch { /* best effort */ }
        }
    }
}
