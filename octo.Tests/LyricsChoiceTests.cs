using System.Net;
using System.Text;
using System.Text.Json;
using System.Xml.Linq;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Logging.Abstractions;
using Moq;
using Octo.Models.Settings;
using Octo.Services.Local;
using Octo.Services.Lyrics;
using Octo.Services.Soulseek;

namespace Octo.Tests;

/// <summary>
/// Choosing lyrics (octoLyrics v1) and what every other client sees of it: candidates, a pin,
/// "none" and "auto" through the Subsonic API, getLyricsBySongId and getLyrics honouring them,
/// credentials checked against Navidrome, strict clients answered as before, the extensions
/// listed truthfully, and the library job stopping and resuming where it was.
/// </summary>
public sealed class LyricsChoiceTests : IDisposable
{
    private readonly string _root = Path.Combine(Path.GetTempPath(), "octo-lyrics-choice-" + Guid.NewGuid().ToString("N"));

    public LyricsChoiceTests() => Directory.CreateDirectory(_root);

    public void Dispose()
    {
        try { Directory.Delete(_root, recursive: true); } catch { /* best effort */ }
    }

    // ---- A source whose search and fetch the tests control ----------------------------------

    private sealed class ChoosableSource(string key, params LyricsCandidate[] entries) : ILyricsSource
    {
        public string Key => key;
        public int Finds { get; private set; }
        public Dictionary<string, LyricsResult> Lyrics { get; } = new();
        public Func<LyricsLookup>? Answer { get; set; }

        public Task<LyricsLookup> FindAsync(LyricsQuery query, CancellationToken ct)
        {
            Finds++;
            return Task.FromResult(Answer?.Invoke() ?? LyricsLookup.Miss);
        }

        public Task<LyricsSearch> SearchAsync(LyricsQuery query, CancellationToken ct) =>
            Task.FromResult(new LyricsSearch(entries, false));

        public Task<LyricsLookup> FetchAsync(string id, CancellationToken ct) =>
            Task.FromResult(Lyrics.TryGetValue(id, out var found) ? new LyricsLookup(found, false) : LyricsLookup.Miss);
    }

    private static ChoosableSource Kugou()
    {
        var source = new ChoosableSource("kugou",
            new LyricsCandidate("kugou", "1.a", "Some Song", "Some Artist", "The Album", 200),
            new LyricsCandidate("kugou", "2.b", "Some Song (Remix)", "Some Artist", null, 260));
        source.Lyrics["1.a"] = new LyricsResult("KuGou", "[00:01.00]<00:01.00>right <00:01.50>words\n[00:03.00]<00:03.00>second<00:04.00>", null, false);
        source.Lyrics["2.b"] = new LyricsResult("KuGou", "[00:01.00]remix words", null, false);
        source.Answer = () => new LyricsLookup(new LyricsResult("KuGou", "[00:01.00]automatic words", null, false), false);
        return source;
    }

    // ---- Navidrome, as far as these calls need it -------------------------------------------

    private sealed class FakeNavidrome : HttpMessageHandler
    {
        public int Pings;

        /// <summary>Whether each lyrics call to Navidrome asked for word cues.</summary>
        public readonly System.Collections.Concurrent.ConcurrentQueue<bool> LyricsAskedEnhanced = new();

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var uri = request.RequestUri!;
            var query = System.Web.HttpUtility.ParseQueryString(uri.Query);
            if (uri.AbsolutePath == "/rest/getLyricsBySongId") LyricsAskedEnhanced.Enqueue(query["enhanced"] == "true");
            var good = query["t"] == "good";
            var json = query["f"] == "json";
            const string refused = """{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}""";
            string? body = uri.AbsolutePath switch
            {
                "/rest/ping" => good ? """{"subsonic-response":{"status":"ok","version":"1.16.1"}}""" : refused,
                "/rest/getSong" => good
                    ? """{"subsonic-response":{"status":"ok","version":"1.16.1","song":{"id":"lib1","title":"Library Song","artist":"Some Artist","album":"An Album","duration":201}}}"""
                    : refused,
                "/rest/getLyricsBySongId" => !good ? refused : json
                    ? LibraryLyricsJson
                    : """<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"><lyricsList></lyricsList></subsonic-response>""",
                "/rest/getLyrics" => !good ? refused : json
                    ? """{"subsonic-response":{"status":"ok","version":"1.16.1","lyrics":{"value":""}}}"""
                    : """<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"><lyrics></lyrics></subsonic-response>""",
                "/rest/getOpenSubsonicExtensions" => """{"subsonic-response":{"status":"ok","version":"1.16.1","openSubsonic":true,"openSubsonicExtensions":[{"name":"songLyrics","versions":[1]}]}}""",
                _ => null,
            };
            if (uri.AbsolutePath == "/rest/ping") Interlocked.Increment(ref Pings);
            return Task.FromResult(body is null
                ? new HttpResponseMessage(HttpStatusCode.NotFound)
                : new HttpResponseMessage(HttpStatusCode.OK)
                {
                    Content = new StringContent(body, Encoding.UTF8, body.StartsWith('<') ? "application/xml" : "application/json"),
                });
        }

        /// <summary>Navidrome's own answer for a library song that has line lyrics of its own.</summary>
        public const string LibraryLyricsJson =
            """{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","lyricsList":{"structuredLyrics":[{"displayArtist":"Some Artist","displayTitle":"Library Song","lang":"xxx","line":[{"start":0,"value":"navidrome's own"}],"synced":true}]}}}""";
    }

    private sealed class Factory(bool fetch, params ILyricsSource[] sources) : WebApplicationFactory<Program>
    {
        private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-lyrics-choice-web-" + Guid.NewGuid());
        public FakeNavidrome Navidrome { get; } = new();

        /// <summary>LYRICS_SOURCES, when not just the sources given, in their order.</summary>
        public string? Order { get; init; }

        public bool PreferWords { get; init; } = true;

        protected override void ConfigureWebHost(IWebHostBuilder builder)
        {
            Directory.CreateDirectory(_directory);
            builder.ConfigureAppConfiguration((_, configuration) => configuration.AddInMemoryCollection(
                new Dictionary<string, string?>
                {
                    ["Subsonic:Url"] = "http://navidrome.test",
                    ["Subsonic:AutoDetectDownloadPath"] = "false",
                    ["Soulseek:BaseUrl"] = "http://127.0.0.1:1",
                    ["YouTube:ShimUrl"] = "http://127.0.0.1:1",
                    ["Library:DownloadPath"] = _directory,
                    ["Metadata:FetchLyrics"] = fetch ? "true" : "false",
                    ["Metadata:LyricsSources"] = Order ?? string.Join(',', sources.Select(source => source.Key)),
                    ["Metadata:PreferWordTimedLyrics"] = PreferWords ? "true" : "false",
                }));
            builder.ConfigureServices(services =>
            {
                services.RemoveAll<IHostedService>();
                services.RemoveAll<IHttpClientFactory>();
                services.AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(Navidrome));
                services.RemoveAll<ILyricsSource>();
                foreach (var source in sources) services.AddSingleton<ILyricsSource>(source);
                services.RemoveAll<LyricsChoiceStore>();
                services.AddSingleton(new LyricsChoiceStore());
                services.RemoveAll<LyricsLibraryStore>();
                services.AddSingleton(new LyricsLibraryStore());
            });
        }

        public string External(string artist = "Some Artist", string title = "Some Song") =>
            Services.GetRequiredService<ExternalIdRegistry>().Register(new SoulseekRouting
            {
                Kind = RoutingKind.Song, Artist = artist, Title = title, Album = "The Album", Duration = 200,
            });

        public override async ValueTask DisposeAsync()
        {
            await base.DisposeAsync();
            try { Directory.Delete(_directory, recursive: true); } catch { /* best effort */ }
        }
    }

    private static string Auth(string user = "alice", string token = "good") => $"u={user}&t={token}&s=salt&v=1.16.1&c=Octo";

    private static async Task<JsonElement> GetJson(HttpClient client, string url)
    {
        var body = await client.GetStringAsync(url);
        return JsonDocument.Parse(body).RootElement.GetProperty("subsonic-response").Clone();
    }

    // ---- getLyricsCandidates / setLyricsChoice ----------------------------------------------

    [Fact]
    public async Task Candidates_ListEveryEntryTheSameSongFirstWithAPreview()
    {
        await using var factory = new Factory(fetch: true, Kugou());
        using var client = factory.CreateClient();
        var id = factory.External();

        var response = await GetJson(client, $"/rest/getLyricsCandidates?id={id}&{Auth()}");

        Assert.Equal("ok", response.GetProperty("status").GetString());
        var list = response.GetProperty("lyricsCandidates");
        Assert.Equal(id, list.GetProperty("id").GetString());
        Assert.Equal("auto", list.GetProperty("choice").GetString());
        var candidates = list.GetProperty("candidate").EnumerateArray().ToList();
        Assert.Equal(["kugou:1.a", "kugou:2.b"], candidates.Select(c => c.GetProperty("id").GetString()));
        var first = candidates[0];
        Assert.Equal("kugou", first.GetProperty("source").GetString());
        Assert.Equal("word", first.GetProperty("kind").GetString());
        Assert.Equal("The Album", first.GetProperty("album").GetString());
        Assert.Equal(200, first.GetProperty("duration").GetInt32());
        Assert.True(first.GetProperty("sameSong").GetBoolean());
        Assert.False(first.GetProperty("chosen").GetBoolean());
        Assert.Equal(["right words", "second"], first.GetProperty("preview").EnumerateArray().Select(p => p.GetString()));
        Assert.False(candidates[1].GetProperty("sameSong").GetBoolean());
        Assert.Equal("line", candidates[1].GetProperty("kind").GetString());
    }

    [Fact]
    public async Task Candidates_ManualSearch_JudgesByTheTitleAndArtistGiven()
    {
        await using var factory = new Factory(fetch: true, Kugou());
        using var client = factory.CreateClient();
        var id = factory.External("Wrong Tag", "Track 01");

        var response = await GetJson(client, $"/rest/getLyricsCandidates?id={id}&title=Some%20Song&artist=Some%20Artist&{Auth()}");

        var first = response.GetProperty("lyricsCandidates").GetProperty("candidate")[0];
        Assert.True(first.GetProperty("sameSong").GetBoolean());
    }

    [Theory]
    [InlineData("getLyricsCandidates", "")]
    [InlineData("setLyricsChoice", "&candidate=none")]
    public async Task Endpoints_WrongPassword_IsError40(string endpoint, string extra)
    {
        await using var factory = new Factory(fetch: true, Kugou());
        using var client = factory.CreateClient();
        var id = factory.External();

        var response = await GetJson(client, $"/rest/{endpoint}?id={id}{extra}&{Auth(token: "bad")}");

        Assert.Equal("failed", response.GetProperty("status").GetString());
        Assert.Equal(40, response.GetProperty("error").GetProperty("code").GetInt32());
        Assert.Equal("auto", factory.Services.GetRequiredService<LyricsChoiceService>().ChoiceFor(id));
    }

    [Fact]
    public async Task Pin_EveryClientGetsThePinnedLyrics_AndAutoGoesBack()
    {
        await using var factory = new Factory(fetch: true, Kugou());
        using var client = factory.CreateClient();
        var id = factory.External();

        await GetJson(client, $"/rest/getLyricsCandidates?id={id}&{Auth()}");
        var set = await GetJson(client, $"/rest/setLyricsChoice?id={id}&candidate=kugou:1.a&{Auth()}");
        Assert.Equal("kugou:1.a", set.GetProperty("lyricsChoice").GetProperty("choice").GetString());

        // Another user, another client, no enhanced: the pinned words, as plain lines.
        var other = await GetJson(client, $"/rest/getLyricsBySongId?id={id}&f=json&{Auth("bob")}");
        var lines = other.GetProperty("lyricsList").GetProperty("structuredLyrics")[0].GetProperty("line");
        Assert.Equal("right words", lines[0].GetProperty("value").GetString());

        // The candidates now say which is chosen.
        var again = await GetJson(client, $"/rest/getLyricsCandidates?id={id}&{Auth("bob")}");
        Assert.Equal("kugou:1.a", again.GetProperty("lyricsCandidates").GetProperty("choice").GetString());
        Assert.True(again.GetProperty("lyricsCandidates").GetProperty("candidate")[0].GetProperty("chosen").GetBoolean());

        await GetJson(client, $"/rest/setLyricsChoice?id={id}&candidate=auto&{Auth()}");
        var automatic = await GetJson(client, $"/rest/getLyricsBySongId?id={id}&f=json&{Auth()}");
        Assert.Equal("automatic words",
            automatic.GetProperty("lyricsList").GetProperty("structuredLyrics")[0].GetProperty("line")[0].GetProperty("value").GetString());
    }

    [Fact]
    public async Task Pin_WithoutAListFirst_FetchesTheCandidateFromItsSource()
    {
        await using var factory = new Factory(fetch: true, Kugou());
        using var client = factory.CreateClient();
        var id = factory.External();

        var set = await GetJson(client, $"/rest/setLyricsChoice?id={id}&candidate=kugou:1.a&{Auth()}");
        var gone = await GetJson(client, $"/rest/setLyricsChoice?id={id}&candidate=kugou:9.z&{Auth()}");

        Assert.Equal("ok", set.GetProperty("status").GetString());
        Assert.Equal(70, gone.GetProperty("error").GetProperty("code").GetInt32());
        Assert.Equal("kugou:1.a", factory.Services.GetRequiredService<LyricsChoiceService>().ChoiceFor(id));
    }

    [Fact]
    public async Task Hide_EveryClientGetsNoLyrics_EvenNavidromesOwn()
    {
        var kugou = Kugou();
        await using var factory = new Factory(fetch: true, kugou);
        using var client = factory.CreateClient();

        await GetJson(client, $"/rest/setLyricsChoice?id=lib1&candidate=none&{Auth()}");
        var hidden = await GetJson(client, $"/rest/getLyricsBySongId?id=lib1&f=json&{Auth("bob")}");
        var legacy = await GetJson(client, $"/rest/getLyrics?artist=Some%20Artist&title=Library%20Song&f=json&{Auth("bob")}");

        Assert.Equal("ok", hidden.GetProperty("status").GetString());
        Assert.Equal(0, hidden.GetProperty("lyricsList").GetProperty("structuredLyrics").GetArrayLength());
        Assert.Equal("", legacy.GetProperty("lyrics").GetProperty("value").GetString());
        Assert.Equal(0, kugou.Finds);
    }

    [Fact]
    public async Task Pin_OnALibrarySong_StillNeedsTheCallersCredentials()
    {
        await using var factory = new Factory(fetch: true, Kugou());
        using var client = factory.CreateClient();
        await GetJson(client, $"/rest/setLyricsChoice?id=lib1&candidate=kugou:1.a&{Auth()}");

        var refused = await GetJson(client, $"/rest/getLyricsBySongId?id=lib1&f=json&{Auth(token: "bad")}");

        Assert.Equal("failed", refused.GetProperty("status").GetString());
    }

    [Fact]
    public async Task Pin_EnhancedClient_GetsTheWordCues()
    {
        await using var factory = new Factory(fetch: true, Kugou());
        using var client = factory.CreateClient();
        var id = factory.External();
        await GetJson(client, $"/rest/setLyricsChoice?id={id}&candidate=kugou:1.a&{Auth()}");

        var rich = await GetJson(client, $"/rest/getLyricsBySongId?id={id}&f=json&enhanced=true&{Auth()}");

        var lyrics = rich.GetProperty("lyricsList").GetProperty("structuredLyrics")[0];
        Assert.Equal("main", lyrics.GetProperty("kind").GetString());
        var cues = lyrics.GetProperty("cueLine")[0].GetProperty("cue").EnumerateArray().ToList();
        Assert.Equal(["right ", "words"], cues.Select(cue => cue.GetProperty("value").GetString()));
    }

    // ---- Strict clients --------------------------------------------------------------------

    /// <summary>A library song Navidrome has lyrics for, asked without enhanced and with nothing
    /// pinned: Navidrome's answer, byte for byte, exactly as before.</summary>
    [Fact]
    public async Task StrictClient_LibrarySong_GetsNavidromesAnswerUntouched()
    {
        await using var factory = new Factory(fetch: true, Kugou());
        using var client = factory.CreateClient();

        var body = await client.GetStringAsync($"/rest/getLyricsBySongId?id=lib1&f=json&{Auth()}");

        Assert.Equal(FakeNavidrome.LibraryLyricsJson, body);
    }

    // ---- The song's own lyrics, ranked (the lyrics finder had to be used every time) --------

    private static ChoosableSource WordTimedKugou()
    {
        var source = Kugou();
        source.Answer = () => new LyricsLookup(new LyricsResult("KuGou", "[00:01.00]<00:01.00>kugou <00:01.50>words<00:02.00>", null, false), false);
        return source;
    }

    /// <summary>Navidrome has line-timed lyrics for the song (in its tags), KuGou word-timed ones,
    /// and word timing is preferred: the app gets KuGou's, words and all.</summary>
    [Fact]
    public async Task LibrarySong_LineTimedOwnLyrics_LoseToWordTimedOnesWhenWordsArePreferred()
    {
        await using var factory = new Factory(fetch: true, WordTimedKugou());
        using var client = factory.CreateClient();

        var response = await GetJson(client, $"/rest/getLyricsBySongId?id=lib1&f=json&enhanced=true&{Auth()}");

        var lyrics = response.GetProperty("lyricsList").GetProperty("structuredLyrics")[0];
        Assert.Equal("kugou words", lyrics.GetProperty("line")[0].GetProperty("value").GetString());
        Assert.Equal(["kugou ", "words"], lyrics.GetProperty("cueLine")[0].GetProperty("cue").EnumerateArray()
            .Select(cue => cue.GetProperty("value").GetString()));
    }

    /// <summary>Word timing not preferred and the song first: its own line-timed lyrics stand,
    /// and no source is asked.</summary>
    [Fact]
    public async Task LibrarySong_OwnLyricsFirst_StandWithoutAskingASource()
    {
        var kugou = WordTimedKugou();
        await using var factory = new Factory(fetch: true, kugou) { Order = "song,kugou", PreferWords = false };
        using var client = factory.CreateClient();

        var body = await client.GetStringAsync($"/rest/getLyricsBySongId?id=lib1&f=json&enhanced=true&{Auth()}");

        Assert.Contains("navidrome's own", body);
        Assert.Equal(0, kugou.Finds);
    }

    /// <summary>KuGou ranked above the song: its lyrics win even at the same timing.</summary>
    [Fact]
    public async Task LibrarySong_SourceRankedAboveTheSong_Wins()
    {
        await using var factory = new Factory(fetch: true, Kugou()) { Order = "kugou,song", PreferWords = false };
        using var client = factory.CreateClient();

        var body = await client.GetStringAsync($"/rest/getLyricsBySongId?id=lib1&f=json&{Auth()}");

        Assert.Contains("automatic words", body);
        Assert.DoesNotContain("navidrome's own", body);
    }

    /// <summary>Navidrome is always asked for word cues, so the song's own timing is known, and a
    /// client that did not ask for them still gets Navidrome's answer without.</summary>
    [Fact]
    public async Task LibrarySong_NavidromeIsAskedForCues_ButAStrictClientGetsNone()
    {
        await using var factory = new Factory(fetch: true, Kugou()) { Order = "song,kugou", PreferWords = false };
        using var client = factory.CreateClient();

        var body = await client.GetStringAsync($"/rest/getLyricsBySongId?id=lib1&f=json&{Auth()}");

        Assert.Equal([true], factory.Navidrome.LyricsAskedEnhanced);
        Assert.DoesNotContain("cueLine", body);
    }

    /// <summary>A pin made when the song had another id (its file was replaced by a better copy)
    /// still answers for it, found by artist and title, and Automatic clears it for good.</summary>
    [Fact]
    public async Task Pin_FollowsTheSongToItsNewId_AndAutomaticClearsIt()
    {
        await using var factory = new Factory(fetch: true, Kugou());
        using var client = factory.CreateClient();
        var store = factory.Services.GetRequiredService<LyricsChoiceStore>();
        store.Set(new LyricsPin("old-id", "kugou:1.a", "KuGou", "[00:01.00]<00:01.00>pinned <00:01.50>words", null,
            "Some Artist", "Library Song", "alice", DateTime.UtcNow));

        var pinned = await client.GetStringAsync($"/rest/getLyricsBySongId?id=lib1&f=json&{Auth()}");
        var listed = await GetJson(client, $"/rest/getLyricsCandidates?id=lib1&{Auth()}");
        await GetJson(client, $"/rest/setLyricsChoice?id=lib1&candidate=auto&{Auth()}");

        Assert.Contains("pinned words", pinned);
        Assert.Equal("kugou:1.a", listed.GetProperty("lyricsCandidates").GetProperty("choice").GetString());
        Assert.Empty(store.All());
    }

    /// <summary>A source that takes longer than the interactive budget to answer.</summary>
    private sealed class SlowSource(TimeSpan delay) : ILyricsSource
    {
        public string Key => "kugou";
        public int Finds;

        /// <summary>Done once a lookup has its answer, for a test to wait on instead of the clock.</summary>
        public TaskCompletionSource Answered { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);

        public async Task<LyricsLookup> FindAsync(LyricsQuery query, CancellationToken ct)
        {
            Interlocked.Increment(ref Finds);
            await Task.Delay(delay, ct);
            Answered.TrySetResult();
            return new LyricsLookup(new LyricsResult("KuGou", "[00:01.00]found late", null, false), false);
        }
    }

    [Fact]
    public async Task SlowLookup_TellsTheOctoAppNotYet_ThenServesWhatItFoundInTheBackground()
    {
        var slow = new SlowSource(TimeSpan.FromSeconds(5.5));
        await using var factory = new Factory(fetch: true, slow);
        using var client = factory.CreateClient();
        var id = factory.External();

        // Past the budget: the Octo app is told the lookup is still running, not "none".
        var first = await GetJson(client, $"/rest/getLyricsBySongId?id={id}&f=json&{Auth()}");
        Assert.Equal("failed", first.GetProperty("status").GetString());
        Assert.Contains("Still looking", first.GetProperty("error").GetProperty("message").GetString());

        // The lookup kept going and was kept, so the next ask gets the lyrics at once. Waited for
        // by the lookup's own answer, not a fixed sleep, which a loaded machine overran; the short
        // grace after it is for the answer to be stored.
        await slow.Answered.Task.WaitAsync(TimeSpan.FromSeconds(30));
        await Task.Delay(TimeSpan.FromMilliseconds(300));
        var second = await GetJson(client, $"/rest/getLyricsBySongId?id={id}&f=json&{Auth()}");
        Assert.Equal("ok", second.GetProperty("status").GetString());
        var line = second.GetProperty("lyricsList").GetProperty("structuredLyrics")[0].GetProperty("line")[0];
        Assert.Equal("found late", line.GetProperty("value").GetString());
        Assert.Equal(1, slow.Finds);
    }

    [Fact]
    public async Task SlowLookup_OtherClientsStillGetTheOrdinaryEmptyList()
    {
        await using var factory = new Factory(fetch: true, new SlowSource(TimeSpan.FromSeconds(10)));
        using var client = factory.CreateClient();
        var id = factory.External();

        var other = await GetJson(client, $"/rest/getLyricsBySongId?id={id}&f=json&{Auth().Replace("c=Octo", "c=Symfonium")}");

        Assert.Equal("ok", other.GetProperty("status").GetString());
        Assert.False(other.GetProperty("lyricsList").TryGetProperty("structuredLyrics", out var lyrics)
            && lyrics.GetArrayLength() > 0);
    }

    [Fact]
    public async Task StrictClient_ExternalSong_GetsLinesWithoutCuesOrKind()
    {
        var kugou = Kugou();
        kugou.Answer = () => new LyricsLookup(new LyricsResult("KuGou", "[00:01.00]<00:01.00>timed <00:01.50>words<00:02.00>", null, false), false);
        await using var factory = new Factory(fetch: true, kugou);
        using var client = factory.CreateClient();
        var id = factory.External();

        var body = await client.GetStringAsync($"/rest/getLyricsBySongId?id={id}&f=json&{Auth()}");

        Assert.Equal(
            """{"subsonic-response":{"status":"ok","version":"1.16.1","lyricsList":{"structuredLyrics":[{"lang":"xxx","synced":true,"displayArtist":"Some Artist","displayTitle":"Some Song","offset":0,"line":[{"start":1000,"value":"timed words"}]}]}}}""",
            body);
    }

    [Fact]
    public async Task LegacyGetLyrics_NavidromeHasNone_GetsThePlainWords()
    {
        var kugou = Kugou();
        kugou.Answer = () => new LyricsLookup(new LyricsResult("KuGou", "[00:01.00]<00:01.00>timed <00:01.50>words\n[00:03.00]next", null, false), false);
        await using var factory = new Factory(fetch: true, kugou);
        using var client = factory.CreateClient();

        var json = await GetJson(client, $"/rest/getLyrics?artist=Some%20Artist&title=Some%20Song&f=json&{Auth()}");
        var xml = XDocument.Parse(await client.GetStringAsync($"/rest/getLyrics?artist=Some%20Artist&title=Some%20Song&{Auth()}"));

        Assert.Equal("timed words\nnext", json.GetProperty("lyrics").GetProperty("value").GetString());
        Assert.Equal("timed words\nnext", xml.Root!.Elements().Single().Value);
    }

    [Fact]
    public async Task LegacyGetLyrics_FetchOff_IsNavidromesAnswer()
    {
        var kugou = Kugou();
        await using var factory = new Factory(fetch: false, kugou);
        using var client = factory.CreateClient();

        var json = await GetJson(client, $"/rest/getLyrics?artist=Some%20Artist&title=Some%20Song&f=json&{Auth()}");

        Assert.Equal("", json.GetProperty("lyrics").GetProperty("value").GetString());
        Assert.Equal(0, kugou.Finds);
    }

    // ---- Extensions ------------------------------------------------------------------------

    [Theory]
    [InlineData(true)]
    [InlineData(false)]
    public async Task Extensions_SongLyricsTwoAlways_OctoLyricsOnlyWhileLookupsRun(bool fetch)
    {
        await using var factory = new Factory(fetch, Kugou());
        using var client = factory.CreateClient();

        var response = await GetJson(client, "/rest/getOpenSubsonicExtensions?f=json");

        var extensions = response.GetProperty("openSubsonicExtensions").EnumerateArray()
            .ToDictionary(e => e.GetProperty("name").GetString()!, e => e.GetProperty("versions").EnumerateArray().Select(v => v.GetInt32()).ToList());
        Assert.Equal([1, 2], extensions["songLyrics"]);
        Assert.Equal(fetch, extensions.ContainsKey("octoLyrics"));
        if (fetch) Assert.Equal([1], extensions["octoLyrics"]);
        Assert.Equal([1, 2], extensions["octoAcquisitions"]);
    }

    // ---- The pins on disk ------------------------------------------------------------------

    [Fact]
    public void Store_PinsSurviveARestart()
    {
        var path = Path.Combine(_root, "lyrics-choices.json");
        var store = new LyricsChoiceStore(path);
        store.Set(new LyricsPin("s1", "kugou:1.a", "KuGou", "[00:01.00]x", null, "A", "T", "alice", DateTime.UtcNow));
        store.Set(new LyricsPin("s2", LyricsPin.Hidden, null, null, null, "B", "U", "bob", DateTime.UtcNow));

        var again = new LyricsChoiceStore(path);

        Assert.Equal("kugou:1.a", again.Get("s1")!.Choice);
        Assert.True(again.Get("s2")!.IsHidden);
        Assert.Equal("s1", again.FindByName("a", "t")!.SongId);
        Assert.True(again.Remove("s1"));
        Assert.Null(new LyricsChoiceStore(path).Get("s1"));
    }

    // ---- The library job -------------------------------------------------------------------

    private string Song(string name, string artist, string title)
    {
        var path = Path.Combine(_root, "music", name);
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        File.WriteAllBytes(path, AudioFixtures.Mp3());
        using var file = TagLib.File.Create(path);
        file.Tag.Performers = [artist];
        file.Tag.Title = title;
        file.Save();
        return path;
    }

    private sealed class CountingSource(Func<LyricsQuery, LyricsLookup> answer) : ILyricsSource
    {
        public string Key => "kugou";
        public List<string> Asked { get; } = [];
        public Action? After { get; set; }

        public Task<LyricsLookup> FindAsync(LyricsQuery query, CancellationToken ct)
        {
            Asked.Add(query.Title);
            var lookup = answer(query);
            After?.Invoke();
            return Task.FromResult(lookup);
        }
    }

    private (LyricsLibraryWorker Worker, LyricsLibraryStore Store) Job(ILyricsSource source, string statePath, bool besideAll = true)
    {
        var settings = TestOptions.Monitor(new MetadataSettings
        {
            FetchLyrics = true, LyricsSources = source.Key, WriteLyricsBesideAllSongs = besideAll,
        });
        var lyrics = new LyricsService([source], settings, NullLogger<LyricsService>.Instance);
        var writer = new LyricsSidecarWriter(lyrics, NullLogger<LyricsSidecarWriter>.Instance);
        var store = new LyricsLibraryStore(statePath);
        var scopes = new Mock<IServiceScopeFactory>();
        return (new LyricsLibraryWorker(store, writer, settings, scopes.Object, () => Path.Combine(_root, "music"),
            NullLogger<LyricsLibraryWorker>.Instance), store);
    }

    [Fact]
    public async Task Job_StoppedPartWay_ResumesWhereItWasAndWritesTheRest()
    {
        LyricsLibraryWorker.Gap = TimeSpan.Zero;
        var songs = Enumerable.Range(1, 5).Select(n => Song($"{n:00} Song {n}.mp3", "Artist", $"Song {n}")).ToList();
        LyricsLibraryWorker? worker = null;
        var source = new CountingSource(query => new LyricsLookup(new LyricsResult("KuGou", $"[00:01.00]<00:01.00>{query.Title}<00:02.00>", null, false), false));
        var state = Path.Combine(_root, "lyrics-library.json");
        (worker, var store) = Job(source, state);
        source.After = () => { if (source.Asked.Count == 2) worker!.RequestCancel(); };

        await worker.RunAsync(new LyricsLibraryRequest(Upgrade: false), CancellationToken.None);

        Assert.Equal(LyricsLibraryStatus.Cancelled, store.Current.Status);
        Assert.Equal(2, store.Current.Cursor);
        Assert.True(store.Current.CanResume);
        store.Dispose();

        // A restart in between: the run is read back from disk.
        var (resumed, reread) = Job(source, state);
        source.After = null;
        await resumed.RunAsync(new LyricsLibraryRequest(Upgrade: false, Resume: true), CancellationToken.None);

        Assert.Equal(LyricsLibraryStatus.Completed, reread.Current.Status);
        Assert.Equal(["Song 1", "Song 2", "Song 3", "Song 4", "Song 5"], source.Asked);
        Assert.Equal(5, reread.Current.Written);
        Assert.Equal(5, reread.Current.WordTimed);
        Assert.All(songs, song => Assert.StartsWith(LyricsSidecarWriter.OctoMark, File.ReadAllText(Path.ChangeExtension(song, ".lrc"))));
    }

    [Fact]
    public void Job_ARunGoingWhenOctoStopped_ComesBackInterruptedNotRunning()
    {
        var state = Path.Combine(_root, "lyrics-library.json");
        File.WriteAllText(state, JsonSerializer.Serialize(new LyricsLibraryRun
        {
            Status = LyricsLibraryStatus.Running, Queue = ["a", "b"], Cursor = 1, Total = 2,
        }));

        var store = new LyricsLibraryStore(state);

        Assert.Equal(LyricsLibraryStatus.Interrupted, store.Current.Status);
        Assert.True(store.Current.CanResume);
        store.Dispose();
    }

    [Fact]
    public async Task Job_ServicesStopAnswering_PausesSoAResumeAsksAgain()
    {
        LyricsLibraryWorker.Gap = TimeSpan.Zero;
        for (var n = 1; n <= LyricsLibraryWorker.BusyInARowLimit + 2; n++) Song($"{n:00}.mp3", "Artist", $"Song {n}");
        var source = new CountingSource(_ => LyricsLookup.Failed);
        var (worker, store) = Job(source, Path.Combine(_root, "state.json"));

        await worker.RunAsync(new LyricsLibraryRequest(false), CancellationToken.None);

        Assert.Equal(LyricsLibraryStatus.Interrupted, store.Current.Status);
        Assert.Equal(0, store.Current.Cursor);
        Assert.Equal(0, store.Current.Busy);
        Assert.Equal(0, store.Current.Processed);
        store.Dispose();
    }

    [Fact]
    public async Task Job_UncertainMatch_GoesOnTheReviewList()
    {
        LyricsLibraryWorker.Gap = TimeSpan.Zero;
        var path = Song("01.mp3", "Artist", "Song");
        var source = new CountingSource(_ => new LyricsLookup(
            new LyricsResult("KuGou", "[00:01.00]x", null, false) { CandidateId = "kugou:1.a", Doubt = "lengths differ by 2 s" }, false));
        var (worker, store) = Job(source, Path.Combine(_root, "state.json"));

        await worker.RunAsync(new LyricsLibraryRequest(false), CancellationToken.None);

        var entry = Assert.Single(store.Current.Review);
        Assert.Equal(path, entry.Path);
        Assert.Equal("kugou:1.a", entry.CandidateId);
        Assert.Equal("lengths differ by 2 s", entry.Reason);
        worker.DismissReview(path);
        Assert.Empty(store.Current.Review);
        store.Dispose();
    }

    [Fact]
    public async Task Job_ByDefault_OnlyWalksOctosDownloads()
    {
        LyricsLibraryWorker.Gap = TimeSpan.Zero;
        var mine = Song("01 Mine.mp3", "Artist", "Mine");
        Song("02 Theirs.mp3", "Artist", "Theirs");
        var source = new CountingSource(_ => new LyricsLookup(new LyricsResult("KuGou", "[00:01.00]x", null, false), false));
        var settings = TestOptions.Monitor(new MetadataSettings { FetchLyrics = true, LyricsSources = "kugou" });
        var lyrics = new LyricsService([source], settings, NullLogger<LyricsService>.Instance);
        var writer = new LyricsSidecarWriter(lyrics, NullLogger<LyricsSidecarWriter>.Instance);
        var library = new Mock<ILocalLibraryService>();
        library.Setup(l => l.GetMappingsAsync()).ReturnsAsync([new LocalSongMapping { LocalPath = mine }]);
        var provider = new ServiceCollection().AddSingleton(library.Object).BuildServiceProvider();
        var store = new LyricsLibraryStore();
        var worker = new LyricsLibraryWorker(store, writer, settings, provider.GetRequiredService<IServiceScopeFactory>(),
            () => Path.Combine(_root, "music"), NullLogger<LyricsLibraryWorker>.Instance);

        await worker.RunAsync(new LyricsLibraryRequest(false), CancellationToken.None);

        Assert.Equal("OctoDownloads", store.Current.Scope);
        Assert.Equal(["Mine"], source.Asked);
        Assert.False(File.Exists(Path.Combine(_root, "music", "02 Theirs.lrc")));
    }

    // ---- The sidecar -----------------------------------------------------------------------

    [Fact]
    public async Task Sidecar_UpgradeReplacesOctosLineTimedFileButNeverTheOwners()
    {
        var ours = Song("01 Ours.mp3", "Artist", "Ours");
        var theirs = Song("02 Theirs.mp3", "Artist", "Theirs");
        File.WriteAllText(Path.ChangeExtension(ours, ".lrc"), LyricsSidecarWriter.OctoMark + "\n[00:01.00]line only\n");
        File.WriteAllText(Path.ChangeExtension(theirs, ".lrc"), "[00:01.00]the owner's line\n");
        var words = new CountingSource(_ => new LyricsLookup(new LyricsResult("KuGou", "[00:01.00]<00:01.00>word<00:02.00>", null, false), false));
        var settings = TestOptions.Monitor(new MetadataSettings { LyricsSources = "kugou" });
        var writer = new LyricsSidecarWriter(new LyricsService([words], settings, NullLogger<LyricsService>.Instance),
            NullLogger<LyricsSidecarWriter>.Instance);

        var upgraded = await writer.WriteAsync(new LyricsJob(ours, "Artist", "Ours", null, 1), upgrade: true, CancellationToken.None);
        var kept = await writer.WriteAsync(new LyricsJob(theirs, "Artist", "Theirs", null, 1), upgrade: true, CancellationToken.None);

        Assert.Equal(LyricsWriteOutcome.Upgraded, upgraded.Outcome);
        Assert.Equal(LyricsSidecarWriter.OctoMark + "\n[00:01.00]<00:01.00>word<00:02.00>\n", File.ReadAllText(Path.ChangeExtension(ours, ".lrc")));
        Assert.Equal(LyricsWriteOutcome.AlreadyThere, kept.Outcome);
        Assert.Equal("[00:01.00]the owner's line\n", File.ReadAllText(Path.ChangeExtension(theirs, ".lrc")));
    }

    [Fact]
    public void Sidecar_OctosMark_IsSkippedByTheLrcReader()
    {
        var lines = LyricsText.ParseLrc(LyricsSidecarWriter.OctoMark + "\n[00:01.00]<00:01.00>word");
        Assert.Equal(["word"], lines.Select(line => line.Text));
    }
}
