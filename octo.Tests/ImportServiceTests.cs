using System.Net;
using System.Text;
using Microsoft.Extensions.Logging.Abstractions;
using Octo.Models.Settings;
using Octo.Services.Imports;
using Octo.Services.Library;

namespace Octo.Tests;

/// <summary>
/// Importing end to end, with Spotify, its embed pages and Navidrome all stood in for: the sign-in
/// and its pasted address, reading liked songs and playlists (someone else's through its public
/// page), have and missing, the trickle queue, the playlist, and a fetched song finding its place.
/// </summary>
public sealed class ImportServiceTests
{
    private const string Theirs = "37i9dQZF1DXcBWIGoYBM5M";

    private sealed class Web : HttpMessageHandler
    {
        public readonly List<string> Calls = [];
        public string TokenError = "";
        public int Refreshes;

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var url = request.RequestUri!.ToString();
            Calls.Add($"{request.Method} {url}");
            if (url == SpotifyWebApi.TokenUrl)
            {
                var form = await request.Content!.ReadAsStringAsync(ct);
                if (form.Contains("grant_type=refresh_token")) Refreshes++;
                if (TokenError.Length > 0) return ImportSourceTests.Json($$"""{"error":"{{TokenError}}"}""", HttpStatusCode.BadRequest);
                return ImportSourceTests.Json("""{"access_token":"at","refresh_token":"rt2","expires_in":3600}""");
            }
            var path = request.RequestUri.AbsolutePath;
            return path switch
            {
                "/v1/me" => ImportSourceTests.Json("""{"id":"me","display_name":"Alice on Spotify"}"""),
                "/v1/me/tracks" => ImportSourceTests.Json("""
                    {"total":2,"next":null,"items":[
                      {"track":{"type":"track","id":"liked00000000000000001","name":"Teardrop","duration_ms":330000,"artists":[{"name":"Massive Attack"}],"album":{"name":"Mezzanine"}}},
                      {"track":{"type":"track","id":"liked00000000000000002","name":"Angel","duration_ms":379000,"artists":[{"name":"Massive Attack"}],"album":{"name":"Mezzanine"}}}]}
                    """),
                "/v1/me/playlists" => ImportSourceTests.Json($$$"""
                    {"next":null,"items":[
                      {"id":"mine0000000000000000001","name":"Road trip","owner":{"id":"me"},"snapshot_id":"s1","items":{"total":2}},
                      {"id":"{{{Theirs}}}","name":"Today's hits","owner":{"id":"spotify","display_name":"Spotify"},"snapshot_id":"s9","items":{"total":3}}]}
                    """),
                "/v1/playlists/mine0000000000000000001/items" => ImportSourceTests.Json("""
                    {"total":2,"next":null,"items":[
                      {"item":{"type":"track","id":"road00000000000000001","name":"Halo","duration_ms":261000,"artists":[{"name":"Beyoncé"}]}},
                      {"item":{"type":"track","id":"liked00000000000000002","name":"Angel","duration_ms":379000,"artists":[{"name":"Massive Attack"}]}}]}
                    """),
                $"/v1/playlists/{Theirs}/items" => ImportSourceTests.Json("""{"error":{"status":403,"message":"Forbidden"}}""", HttpStatusCode.Forbidden),
                $"/embed/playlist/{Theirs}" => new HttpResponseMessage(HttpStatusCode.OK)
                {
                    Content = new StringContent(ImportSourceTests.EmbedFor(3, "Today's hits"), Encoding.UTF8, "text/html"),
                },
                _ => new HttpResponseMessage(HttpStatusCode.NotFound),
            };
        }
    }

    private sealed class Factory(HttpMessageHandler handler) : IHttpClientFactory
    {
        public HttpClient CreateClient(string name) => new(handler, disposeHandler: false);
    }

    private sealed class Rig
    {
        public DateTime Now = new(2026, 10, 4, 12, 0, 0, DateTimeKind.Utc);
        public readonly Web Web = new();
        public readonly TestOptionsMonitor<ImportSettings> Settings = TestOptions.Monitor(new ImportSettings { SpotifyClientId = "client-1" });
        public readonly ImportStore Store = new();
        public readonly TrickleQueue Queue = new();
        public readonly SpotifyAccountStore Accounts = new();
        public readonly SpotifyAuth Auth = new();
        public readonly TrickleWorker Trickle;
        public readonly ImportService Service;
        public readonly List<LibrarySongRow> Library =
        [
            new("nd-teardrop", "/m/t.flac", null, 1, "flac", 900, "Teardrop", "Massive Attack", 331, "Mezzanine"),
            new("nd-halo", "/m/h.mp3", null, 1, "mp3", 320, "Halo", "Beyonce", 261, null),
        ];
        public readonly Dictionary<string, List<string>> Playlists = new();

        public Rig()
        {
            Trickle = new TrickleWorker(Queue, Settings, NullLogger<TrickleWorker>.Instance);
            var http = new Factory(Web);
            var matcher = new ImportMatcher(null!, NullLogger<ImportMatcher>.Instance)
            {
                ReadPage = (start, count, _) =>
                {
                    var page = Library.Skip(start).Take(count).ToList();
                    return Task.FromResult<(IReadOnlyList<LibrarySongRow>, int)?>((page, page.Count));
                },
            };
            var playlists = new ImportPlaylists(null!, null!, NullLogger<ImportPlaylists>.Instance)
            {
                AdminName = () => "alice",
                Create = (_, _, _) => { Playlists["pl1"] = []; return Task.FromResult<string?>("pl1"); },
                Exists = (id, _) => Task.FromResult<bool?>(Playlists.ContainsKey(id)),
                ReadTracks = (id, _) => Task.FromResult<IReadOnlyList<(string, string)>?>(
                    Playlists[id].Select((song, i) => ((i + 1).ToString(), song)).ToList()),
                Remove = (id, positions, _) =>
                {
                    foreach (var p in positions.Select(int.Parse).OrderDescending()) Playlists[id].RemoveAt(p - 1);
                    return Task.FromResult(true);
                },
                Add = (id, songs, _) => { Playlists[id].AddRange(songs); return Task.FromResult(true); },
            };
            var api = new SpotifyWebApi(http, NullLogger<SpotifyWebApi>.Instance)
            {
                Wait = (_, _) => Task.CompletedTask, Clock = () => Now,
            };
            Service = new ImportService(Store, Queue, Trickle, matcher, playlists, Auth, Accounts, api,
                new SpotifyLinkReader(http, NullLogger<SpotifyLinkReader>.Instance), Settings,
                NullLogger<ImportService>.Instance) { Clock = () => Now };
        }

        public async Task<ImportOverview> ConnectAndRead()
        {
            var begin = Service.BeginSpotify("alice");
            Assert.True(begin.Ok, begin.Message);
            var state = System.Web.HttpUtility.ParseQueryString(new Uri(begin.Url!).Query)["state"];
            var finish = await Service.FinishSpotifyAsync($"http://127.0.0.1/callback?code=the-code&state={state}", null, null, "alice", CancellationToken.None);
            Assert.True(finish.Ok, finish.Message);
            var deadline = DateTime.UtcNow + TimeSpan.FromSeconds(20);
            while (Service.Overview("alice").Reading.Busy && DateTime.UtcNow < deadline) await Task.Delay(20);
            return Service.Overview("alice");
        }
    }

    [Fact]
    public void WithoutAClientIdTheSignInSaysWhatToAdd()
    {
        var rig = new Rig();
        rig.Settings.Set(new ImportSettings());
        var begin = rig.Service.BeginSpotify("alice");
        Assert.False(begin.Ok);
        Assert.Contains("Client ID", begin.Message);
        Assert.False(rig.Service.Overview("alice").Spotify.Configured);
    }

    [Fact]
    public void AnAppSignsInOnItsOwnLoopbackPort()
    {
        var rig = new Rig();
        var begin = rig.Service.BeginSpotify("alice", "http://127.0.0.1:51234/callback");
        Assert.True(begin.Ok);
        Assert.Contains(Uri.EscapeDataString("http://127.0.0.1:51234/callback"), begin.Url);
        Assert.False(rig.Service.BeginSpotify("alice", "http://192.168.1.2:51234/callback").Ok);
    }

    [Fact]
    public async Task ConnectingReadsLikedSongsAndPlaylists_AndSaysWhatTheLibraryHas()
    {
        var rig = new Rig();
        var overview = await rig.ConnectAndRead();
        Assert.Null(overview.Reading.Error);
        Assert.True(overview.Spotify.Connected);
        Assert.Equal("Alice on Spotify", overview.Spotify.Account);
        Assert.Equal(rig.Now + ImportService.SignInLasts, overview.Spotify.EndsUtc);

        var liked = overview.Lists.Single(list => list.Source == ImportSources.SpotifyLiked);
        Assert.Equal((2, 1, 1), (liked.Total, liked.Have, liked.Missing));
        var road = overview.Lists.Single(list => list.Name == "Road trip");
        Assert.Equal((1, 1), (road.Have, road.Missing));
        Assert.Null(road.Partial);

        // Someone else's playlist: Spotify hides its songs, its public page lists them.
        var theirs = overview.Lists.Single(list => list.Name == "Today's hits");
        Assert.Equal(3, theirs.Missing);
        Assert.Equal("Spotify", theirs.By);
        Assert.Null(theirs.Partial);
        Assert.Contains(rig.Web.Calls, call => call.EndsWith($"/embed/playlist/{Theirs}"));

        var detail = rig.Service.Detail("alice", liked.Id)!;
        Assert.Equal("nd-teardrop", detail.Tracks.Single(t => t.Title == "Teardrop").LibraryId);
        Assert.Equal(ImportTrackStates.Missing, detail.Tracks.Single(t => t.Title == "Angel").State);
    }

    [Fact]
    public async Task AnUnchangedPlaylistIsNotReadAgain()
    {
        var rig = new Rig();
        await rig.ConnectAndRead();
        rig.Web.Calls.Clear();
        await rig.Service.ReadSpotifyAsync("alice", CancellationToken.None);
        Assert.DoesNotContain(rig.Web.Calls, call => call.Contains("/playlists/mine0000000000000000001/items"));
        Assert.Contains(rig.Web.Calls, call => call.Contains("/me/tracks"));
    }

    [Fact]
    public async Task ASignInIsUsedOnceAndOnlyByWhoeverStartedIt()
    {
        var rig = new Rig();
        var begin = rig.Service.BeginSpotify("alice");
        var state = System.Web.HttpUtility.ParseQueryString(new Uri(begin.Url!).Query)["state"];
        Assert.False((await rig.Service.FinishSpotifyAsync(null, "c", state, "bob", CancellationToken.None)).Ok);
        // Refused for bob, and spent: a state answers once.
        Assert.False((await rig.Service.FinishSpotifyAsync(null, "c", state, "alice", CancellationToken.None)).Ok);
        var denied = await rig.Service.FinishSpotifyAsync("http://127.0.0.1/callback?error=access_denied", null, null, "alice", CancellationToken.None);
        Assert.Contains("not allowed", denied.Message);
    }

    [Fact]
    public async Task AnEndedSignInIsShown_AndNothingIsReadWithIt()
    {
        var rig = new Rig();
        await rig.ConnectAndRead();
        rig.Now += TimeSpan.FromHours(2);
        rig.Web.TokenError = "invalid_grant";
        await rig.Service.ReadSpotifyAsync("alice", CancellationToken.None);
        var overview = rig.Service.Overview("alice");
        Assert.False(overview.Spotify.Connected);
        Assert.Contains("Connect again", overview.Spotify.Problem);
        Assert.Contains("Connect again", overview.Reading.Error);
    }

    [Fact]
    public async Task AnAccessTokenIsRenewedWhenItEnds_KeepingTheNewRefreshToken()
    {
        var rig = new Rig();
        await rig.ConnectAndRead();
        rig.Now += TimeSpan.FromHours(2);
        await rig.Service.ReadSpotifyAsync("alice", CancellationToken.None);
        Assert.Equal(1, rig.Web.Refreshes);
        Assert.Equal("rt2", rig.Accounts.Get("alice")!.RefreshToken);
    }

    [Fact]
    public async Task FetchingAListQueuesItsMissingSongs_AndStoppingTakesThemBack()
    {
        var rig = new Rig();
        var overview = await rig.ConnectAndRead();
        var liked = overview.Lists.Single(list => list.Source == ImportSources.SpotifyLiked).Id;
        var road = overview.Lists.Single(list => list.Name == "Road trip").Id;

        var on = rig.Service.SetGetMissing("alice", liked, true);
        Assert.Equal(1, on.Count);
        // Angel is the only song Road trip misses, and Liked Songs already queued it.
        Assert.Equal(0, rig.Service.SetGetMissing("alice", road, true).Count);
        Assert.Single(rig.Queue.Snapshot("alice"));
        Assert.Equal(ImportTrackStates.Queued, rig.Service.Detail("alice", road)!.Tracks.Single(t => t.Title == "Angel").State);

        // Angel is still wanted by Road trip, so stopping Liked Songs leaves it queued.
        Assert.Equal(0, rig.Service.SetGetMissing("alice", liked, false).Count);
        Assert.Equal(1, rig.Service.SetGetMissing("alice", road, false).Count);
        Assert.Empty(rig.Queue.Snapshot("alice"));
    }

    [Fact]
    public async Task APlaylistIsKept_AndAFetchedSongTakesItsPlace()
    {
        var rig = new Rig();
        var overview = await rig.ConnectAndRead();
        var road = overview.Lists.Single(list => list.Name == "Road trip").Id;
        var made = await rig.Service.SetPlaylistAsync("alice", road, true, CancellationToken.None);
        Assert.True(made.Ok, made.Message);
        Assert.Equal(["nd-halo"], rig.Playlists["pl1"]);
        Assert.Equal("pl1", rig.Service.Overview("alice").Lists.Single(list => list.Id == road).PlaylistId);

        // The trickle brings Angel; the next pass matches it and puts it in its place.
        rig.Service.SetGetMissing("alice", road, true);
        rig.Library.Add(new("nd-angel", "/m/a.flac", null, 1, "flac", 900, "Angel", "Massive Attack", 379, "Mezzanine"));
        rig.Trickle.Owned = (_, _) => Task.FromResult<TrickleWorker.OwnedSong?>(new("nd-angel"));
        rig.Trickle.PeopleDownloading = () => false;
        rig.Trickle.SoulseekOut = _ => Task.FromResult(false);
        await rig.Trickle.TickAsync(CancellationToken.None);
        await rig.Trickle.DrainAsync();
        await rig.Service.LoopOnceAsync(CancellationToken.None);

        Assert.Equal(["nd-halo", "nd-angel"], rig.Playlists["pl1"]);
        var list = rig.Service.Overview("alice").Lists.Single(l => l.Id == road);
        Assert.Equal((2, 0), (list.Have, list.Missing));
    }

    [Fact]
    public async Task APlaylistGoneFromTheAccountStaysWhileItIsKept()
    {
        var rig = new Rig();
        var overview = await rig.ConnectAndRead();
        var road = overview.Lists.Single(list => list.Name == "Road trip").Id;
        var hits = overview.Lists.Single(list => list.Name == "Today's hits").Id;
        rig.Service.SetGetMissing("alice", road, true);
        // Both disappear from the account.
        rig.Store.Update("alice", road, l => l.Id = "spotify-renamed-away");
        rig.Store.Update("alice", hits, l => l.Id = "spotify-also-away");
        await rig.Service.ReadSpotifyAsync("alice", CancellationToken.None);
        var lists = rig.Service.Overview("alice").Lists;
        Assert.True(lists.Single(l => l.Id == "spotify-renamed-away").Gone);
        Assert.DoesNotContain(lists, l => l.Id == "spotify-also-away");
    }

    [Fact]
    public async Task AListComesFromALinkOrAFile_WithNoSignIn()
    {
        var rig = new Rig();
        rig.Settings.Set(new ImportSettings());
        var link = await rig.Service.AddLinkAsync("bob", $"https://open.spotify.com/playlist/{Theirs}?si=x", CancellationToken.None);
        Assert.True(link.Ok, link.Message);
        Assert.Equal(3, link.Count);

        var csv = "Track Name,Artist Name(s),Track Duration (ms)\nTeardrop,Massive Attack,330000\nNope,Nobody,1000\n";
        var file = await rig.Service.AddFileAsync("bob", "Gym.csv", new MemoryStream(Encoding.UTF8.GetBytes(csv)), CancellationToken.None);
        Assert.True(file.Ok, file.Message);
        var gym = rig.Service.Overview("bob").Lists.Single(list => list.Name == "Gym");
        Assert.Equal((1, 1), (gym.Have, gym.Missing));
        Assert.False(gym.CanRefresh);
        Assert.Empty(rig.Service.Overview("alice").Lists);

        var bad = await rig.Service.AddFileAsync("bob", "x.csv", new MemoryStream(Encoding.UTF8.GetBytes("a,b\n1,2\n")), CancellationToken.None);
        Assert.False(bad.Ok);
    }

    [Fact]
    public async Task OnePersonKeepsAtMost500Lists_AndAFileOfTheSameNameStillReplacesItsList()
    {
        var rig = new Rig();
        rig.Settings.Set(new ImportSettings());
        for (var i = 0; i < ImportService.MaxLists; i++)
            rig.Store.Save(new ImportList { Id = $"file-{i}", Owner = "bob", Name = $"List {i}", Source = ImportSources.File });
        var csv = "Track Name,Artist Name(s)\nTeardrop,Massive Attack\n";

        var refused = await rig.Service.AddFileAsync("bob", "Gym.csv", new MemoryStream(Encoding.UTF8.GetBytes(csv)), CancellationToken.None);

        Assert.False(refused.Ok);
        Assert.Equal("That would make more than 500 lists, the most Octo keeps for one person. Remove some first.", refused.Message);
        Assert.True((await rig.Service.AddFileAsync("alice", "Gym.csv", new MemoryStream(Encoding.UTF8.GetBytes(csv)), CancellationToken.None)).Ok);
    }

    [Fact]
    public void TheRedirectSaysWhetherOctoFinishesItself()
    {
        Assert.True(ImportService.OctoFinishes("https://octo.example.com/imports/spotify/callback"));
        Assert.False(ImportService.OctoFinishes("http://127.0.0.1/callback"));
    }
}
