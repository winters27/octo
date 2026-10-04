using System.IO.Compression;
using System.Net;
using System.Text;
using Microsoft.Extensions.Logging.Abstractions;
using Octo.Services.Imports;

namespace Octo.Tests;

/// <summary>
/// Where imported songs come from: the Spotify sign-in (PKCE, redirect rules, the pasted address),
/// the Web API as Spotify answers in 2026, a public link's embed page, and exported files.
/// </summary>
public sealed class ImportSourceTests
{
    // ---- Sign-in -----------------------------------------------------------------------------

    [Fact]
    public void TheChallengeIsTheVerifiersSha256_AsRfc7636Says()
    {
        // RFC 7636, appendix B.
        Assert.Equal("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
            SpotifyAuth.Challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"));
        var verifier = SpotifyAuth.Verifier();
        Assert.Equal(64, verifier.Length);
        Assert.Matches("^[A-Za-z0-9_-]+$", verifier);
    }

    [Fact]
    public void ASignInAsksForReadOnlyScopes_AndItsStateAnswersOnce()
    {
        var auth = new SpotifyAuth();
        var (url, state) = auth.Begin("alice", "client-1", "http://127.0.0.1/callback");
        var query = System.Web.HttpUtility.ParseQueryString(new Uri(url).Query);
        Assert.Equal("code", query["response_type"]);
        Assert.Equal("client-1", query["client_id"]);
        Assert.Equal("S256", query["code_challenge_method"]);
        Assert.Equal("http://127.0.0.1/callback", query["redirect_uri"]);
        Assert.Equal(SpotifyAuth.Scopes, query["scope"]);
        Assert.Equal(state, query["state"]);

        var pending = auth.Take(state);
        Assert.NotNull(pending);
        Assert.Equal("alice", pending!.User);
        Assert.Equal(SpotifyAuth.Challenge(pending.Verifier), query["code_challenge"]);
        Assert.Null(auth.Take(state));
    }

    [Fact]
    public void AnOldSignInIsForgotten()
    {
        var now = new DateTime(2026, 10, 4, 12, 0, 0, DateTimeKind.Utc);
        var auth = new SpotifyAuth { Clock = () => now };
        var (_, state) = auth.Begin("alice", "c", "http://127.0.0.1/callback");
        now += SpotifyAuth.PendingFor + TimeSpan.FromSeconds(1);
        Assert.Null(auth.Take(state));
    }

    [Theory]
    [InlineData("http://127.0.0.1/callback", null)]
    [InlineData("http://127.0.0.1:8888/callback", null)]
    [InlineData("http://[::1]:5000/cb", null)]
    [InlineData("https://octo.example.com/imports/spotify/callback", null)]
    [InlineData("http://localhost:8888/callback", "Spotify does not take localhost. Use 127.0.0.1 instead.")]
    [InlineData("http://192.168.50.21:5274/imports/spotify/callback", "Spotify takes only an https address, or http on 127.0.0.1 or [::1].")]
    [InlineData("callback", "The redirect URI is not a full address.")]
    public void OnlyRedirectsSpotifyTakesAreOffered(string uri, string? problem) =>
        Assert.Equal(problem, SpotifyAuth.RedirectProblem(uri));

    [Theory]
    // A loopback registered with no port takes any port on the same path: the apps' own listener.
    [InlineData("http://127.0.0.1/callback", "http://127.0.0.1:53111/callback", "http://127.0.0.1:53111/callback")]
    [InlineData("http://127.0.0.1/callback", null, "http://127.0.0.1/callback")]
    [InlineData("http://127.0.0.1/callback", "http://127.0.0.1:53111/other", null)]
    [InlineData("http://127.0.0.1/callback", "http://evil.example/callback", null)]
    // A registered port is exact.
    [InlineData("http://127.0.0.1:8888/callback", "http://127.0.0.1:53111/callback", null)]
    [InlineData("http://127.0.0.1:8888/callback", "http://127.0.0.1:8888/callback", "http://127.0.0.1:8888/callback")]
    [InlineData("https://octo.example.com/imports/spotify/callback", "http://127.0.0.1:53111/callback", null)]
    public void AnAppMayUseItsOwnPortOnlyOnAPortlessLoopback(string registered, string? asked, string? allowed) =>
        Assert.Equal(allowed, SpotifyAuth.AllowedRedirect(registered, asked));

    [Theory]
    [InlineData("http://127.0.0.1/callback?code=abc%2Fd&state=s1", "abc/d", "s1", null)]
    [InlineData("code=abc&state=s1", "abc", "s1", null)]
    [InlineData("  http://127.0.0.1/callback?error=access_denied&state=s1 ", null, "s1", "access_denied")]
    [InlineData("AQBxyz", "AQBxyz", null, null)]
    [InlineData("", null, null, null)]
    public void ThePastedAddressIsReadHoweverMuchOfItCame(string pasted, string? code, string? state, string? error) =>
        Assert.Equal((code, state, error), SpotifyAuth.ParseReturn(pasted));

    // ---- Web API -------------------------------------------------------------------------------

    private sealed class Spotify : HttpMessageHandler
    {
        public readonly List<(HttpRequestMessage Request, string Body)> Calls = [];
        public Func<HttpRequestMessage, HttpResponseMessage> Answer { get; set; } = _ => Json("{}");

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var body = request.Content is null ? "" : await request.Content.ReadAsStringAsync(ct);
            Calls.Add((request, body));
            return Answer(request);
        }
    }

    internal static HttpResponseMessage Json(string json, HttpStatusCode status = HttpStatusCode.OK) =>
        new(status) { Content = new StringContent(json, Encoding.UTF8, "application/json") };

    private sealed class Factory(HttpMessageHandler handler) : IHttpClientFactory
    {
        public HttpClient CreateClient(string name) => new(handler, disposeHandler: false);
    }

    private static (SpotifyWebApi Api, Spotify Http, List<TimeSpan> Waits) Api()
    {
        var http = new Spotify();
        var waits = new List<TimeSpan>();
        var api = new SpotifyWebApi(new Factory(http), NullLogger<SpotifyWebApi>.Instance)
        {
            Wait = (wait, _) => { waits.Add(wait); return Task.CompletedTask; },
            Clock = () => new DateTime(2026, 10, 4, 12, 0, 0, DateTimeKind.Utc),
        };
        return (api, http, waits);
    }

    [Fact]
    public async Task TheCodeIsExchangedWithTheVerifier_AndNoSecret()
    {
        var (api, http, _) = Api();
        http.Answer = _ => Json("""{"access_token":"at","refresh_token":"rt","expires_in":3600,"token_type":"Bearer"}""");
        var tokens = await api.ExchangeAsync("client-1", "the-code", "the-verifier", "http://127.0.0.1/callback", CancellationToken.None);
        Assert.Equal("at", tokens.AccessToken);
        Assert.Equal("rt", tokens.RefreshToken);
        Assert.Equal(new DateTime(2026, 10, 4, 13, 0, 0, DateTimeKind.Utc), tokens.ExpiresUtc);
        var (request, body) = Assert.Single(http.Calls);
        Assert.Equal(SpotifyWebApi.TokenUrl, request.RequestUri!.ToString());
        var form = System.Web.HttpUtility.ParseQueryString(body);
        Assert.Equal("authorization_code", form["grant_type"]);
        Assert.Equal("the-verifier", form["code_verifier"]);
        Assert.Equal("client-1", form["client_id"]);
        Assert.Null(form["client_secret"]);
        Assert.Null(request.Headers.Authorization);
    }

    [Fact]
    public async Task ARevokedOrSixMonthOldSignInSaysToConnectAgain()
    {
        var (api, http, _) = Api();
        http.Answer = _ => Json("""{"error":"invalid_grant","error_description":"Refresh token revoked"}""", HttpStatusCode.BadRequest);
        var ex = await Assert.ThrowsAsync<SpotifyException>(() => api.RefreshAsync("c", "rt", CancellationToken.None));
        Assert.True(ex.SignInEnded);
        Assert.Contains("Connect again", ex.Message);
    }

    [Fact]
    public async Task LikedSongsArePagedFifty_AndReadUnderItemOrTrack()
    {
        var (api, http, _) = Api();
        http.Answer = request =>
        {
            var offset = System.Web.HttpUtility.ParseQueryString(request.RequestUri!.Query)["offset"];
            Assert.Equal("50", System.Web.HttpUtility.ParseQueryString(request.RequestUri!.Query)["limit"]);
            return offset == "0"
                ? Json("""
                    {"total":3,"next":"https://api.spotify.com/v1/me/tracks?offset=2","items":[
                      {"track":{"type":"track","id":"4uLU6hMCjMI75M1A2tKUQC","name":"Never Gonna Give You Up","duration_ms":213573,
                        "artists":[{"name":"Rick Astley"}],"album":{"name":"Whenever You Need Somebody"},"external_ids":{"isrc":"gbarl9300135"}}},
                      {"track":{"type":"episode","id":"ep","name":"A podcast"}}]}
                    """)
                : Json("""
                    {"total":3,"next":null,"items":[
                      {"item":{"type":"track","id":"7ouMYWpwJ422jRcDASZB7P","name":"Teardrop","duration_ms":330000,
                        "artists":[{"name":"Massive Attack"},{"name":"Elizabeth Fraser"}],"album":{"name":"Mezzanine"}}}]}
                    """);
        };
        var tracks = await api.LikedSongsAsync("at", null, CancellationToken.None);
        Assert.Equal(2, tracks.Count);
        Assert.Equal("spotify:4uLU6hMCjMI75M1A2tKUQC", tracks[0].Key);
        Assert.Equal("GBARL9300135", tracks[0].Isrc);
        Assert.Equal(214, tracks[0].Seconds);
        Assert.Equal("Massive Attack, Elizabeth Fraser", tracks[1].Artist);
        Assert.Null(tracks[1].Isrc);
        Assert.All(http.Calls, call => Assert.Equal("Bearer at", call.Request.Headers.Authorization!.ToString()));
    }

    [Fact]
    public async Task PlaylistsReadTheirTotalFromItemsOrTracks()
    {
        var (api, http, _) = Api();
        http.Answer = _ => Json("""
            {"next":null,"items":[
              {"id":"p1","name":"Mine","owner":{"id":"me","display_name":"Me"},"collaborative":false,"snapshot_id":"s1","items":{"total":12},
               "images":[{"url":"https://i.scdn.co/image/a"}]},
              {"id":"p2","name":"Theirs","owner":{"id":"them"},"collaborative":true,"tracks":{"total":7}}]}
            """);
        var lists = await api.PlaylistsAsync("at", CancellationToken.None);
        Assert.Equal(12, lists[0].Total);
        Assert.Equal("https://i.scdn.co/image/a", lists[0].ImageUrl);
        Assert.Equal(7, lists[1].Total);
        Assert.True(lists[1].Collaborative);
        Assert.Equal("them", lists[1].OwnerName);
    }

    [Fact]
    public async Task SomeoneElsesPlaylistIsHidden_NotAnError()
    {
        var (api, http, _) = Api();
        http.Answer = _ => Json("""{"error":{"status":403,"message":"Forbidden"}}""", HttpStatusCode.Forbidden);
        var items = await api.PlaylistItemsAsync("at", "p2", null, CancellationToken.None);
        Assert.True(items.Hidden);
        Assert.Empty(items.Tracks);
        Assert.Contains("/playlists/p2/items", http.Calls[0].Request.RequestUri!.ToString());
    }

    [Fact]
    public async Task ATooManyRequestsWaitsAsLongAsSpotifyAsks()
    {
        var (api, http, waits) = Api();
        var first = true;
        http.Answer = _ =>
        {
            if (!first) return Json("""{"total":0,"next":null,"items":[]}""");
            first = false;
            var slow = Json("{}", HttpStatusCode.TooManyRequests);
            slow.Headers.RetryAfter = new System.Net.Http.Headers.RetryConditionHeaderValue(TimeSpan.FromSeconds(7));
            return slow;
        };
        await api.LikedSongsAsync("at", null, CancellationToken.None);
        Assert.Equal([TimeSpan.FromSeconds(7)], waits);
    }

    [Fact]
    public async Task AUsedUpQuotaStopsAtOnce()
    {
        var (api, http, waits) = Api();
        http.Answer = _ => Json("""{"error":{"status":429,"message":"Too many","reason":"QUOTA_EXCEEDED"}}""", HttpStatusCode.TooManyRequests);
        var ex = await Assert.ThrowsAsync<SpotifyException>(() => api.LikedSongsAsync("at", null, CancellationToken.None));
        Assert.Contains("used up", ex.Message);
        Assert.Empty(waits);
    }

    [Fact]
    public async Task AnAccountNotOnTheAppsListIsToldWhereToAddIt()
    {
        var (api, http, _) = Api();
        http.Answer = _ => Json("""{"error":{"status":403,"message":"User not registered in the Developer Dashboard"}}""", HttpStatusCode.Forbidden);
        var ex = await Assert.ThrowsAsync<SpotifyException>(() => api.ProfileAsync("at", CancellationToken.None));
        Assert.Contains("User Management", ex.Message);
    }

    // ---- Links ---------------------------------------------------------------------------------

    [Theory]
    [InlineData("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M?si=abc", "playlist", "37i9dQZF1DXcBWIGoYBM5M")]
    [InlineData("https://open.spotify.com/intl-de/album/4LH4d3cOWNNsVw41Gqt2kv", "album", "4LH4d3cOWNNsVw41Gqt2kv")]
    [InlineData("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M", "playlist", "37i9dQZF1DXcBWIGoYBM5M")]
    [InlineData("https://open.spotify.com/embed/playlist/37i9dQZF1DXcBWIGoYBM5M", "playlist", "37i9dQZF1DXcBWIGoYBM5M")]
    public void LinksAndUrisAreRead(string link, string kind, string id) =>
        Assert.Equal((kind, id), SpotifyLinkReader.Parse(link));

    [Theory]
    [InlineData("https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC")]
    [InlineData("https://example.com/playlist/37i9dQZF1DXcBWIGoYBM5M")]
    [InlineData("")]
    public void OtherLinksAreNotTaken(string link) => Assert.Null(SpotifyLinkReader.Parse(link));

    private static string EmbedPage(int count, string name = "Road trip")
    {
        var rows = string.Join(",", Enumerable.Range(1, count).Select(i =>
            $$"""{"uri":"spotify:track:{{i:D22}}","title":"Song {{i}}","subtitle":"Artist {{i}}, Guest","duration":200000,"entityType":"track"}"""));
        var json = "{\"props\":{\"pageProps\":{\"state\":{\"data\":{\"entity\":{\"type\":\"playlist\",\"name\":\"" + name
            + "\",\"coverArt\":{\"sources\":[{\"url\":\"https://i.scdn.co/image/x\"}]},\"trackList\":[" + rows + "]}}}}}}";
        return $"<html><body><script id=\"__NEXT_DATA__\" type=\"application/json\">{json}</script></body></html>";
    }

    [Fact]
    public void AnEmbedPageGivesTitlesArtistsAndLengths()
    {
        var list = SpotifyLinkReader.ParsePage("playlist", "p", EmbedPage(3))!;
        Assert.Equal("Road trip", list.Name);
        Assert.Equal("https://i.scdn.co/image/x", list.ImageUrl);
        Assert.Equal(3, list.Tracks.Count);
        Assert.Equal("Artist 1, Guest", list.Tracks[0].Artist);
        Assert.Equal(200, list.Tracks[0].Seconds);
        Assert.Equal($"spotify:{1:D22}", list.Tracks[0].Key);
        Assert.False(list.Capped);
        Assert.True(SpotifyLinkReader.ParsePage("playlist", "p", EmbedPage(100))!.Capped);
        Assert.Null(SpotifyLinkReader.ParsePage("playlist", "p", "<html>changed</html>"));
    }

    [Fact]
    public async Task APrivatePlaylistsLinkSaysItNeedsTheSignIn()
    {
        var http = new Spotify { Answer = _ => new HttpResponseMessage(HttpStatusCode.NotFound) };
        var reader = new SpotifyLinkReader(new Factory(http), NullLogger<SpotifyLinkReader>.Instance);
        var ex = await Assert.ThrowsAsync<SpotifyException>(() =>
            reader.ReadAsync("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M", CancellationToken.None));
        Assert.Contains("sign-in", ex.Message);
        Assert.Equal("https://open.spotify.com/embed/playlist/37i9dQZF1DXcBWIGoYBM5M", http.Calls[0].Request.RequestUri!.ToString());
    }

    internal static string EmbedFor(int count, string name = "Road trip") => EmbedPage(count, name);

    // ---- Files ---------------------------------------------------------------------------------

    [Fact]
    public void AnExportifyCsvIsReadWithQuotesCommasAndLineBreaks()
    {
        const string csv = "﻿Track URI,Track Name,Artist Name(s),Album Name,Track Duration (ms),ISRC\r\n"
            + "spotify:track:4uLU6hMCjMI75M1A2tKUQC,Never Gonna Give You Up,Rick Astley,Whenever You Need Somebody,213573,GBARL9300135\r\n"
            + "spotify:track:7ouMYWpwJ422jRcDASZB7P,\"Teardrop, \"\"Live\"\"\",\"Massive Attack, Elizabeth Fraser\",\"Mezzanine\nDeluxe\",330000,\r\n";
        var lists = ImportFileReader.Read("Road trip.csv", new MemoryStream(Encoding.UTF8.GetBytes(csv)));
        var list = Assert.Single(lists);
        Assert.Equal("Road trip", list.Name);
        Assert.Equal(2, list.Tracks.Count);
        Assert.Equal("spotify:4uLU6hMCjMI75M1A2tKUQC", list.Tracks[0].Key);
        Assert.Equal(214, list.Tracks[0].Seconds);
        Assert.Equal("GBARL9300135", list.Tracks[0].Isrc);
        Assert.Equal("Teardrop, \"Live\"", list.Tracks[1].Title);
        Assert.Equal("Mezzanine\nDeluxe", list.Tracks[1].Album);
    }

    [Fact]
    public void ACsvWithAPlaylistColumnBecomesOneListEach()
    {
        const string csv = "Track name;Artist name;Album;Playlist name;Duration\n"
            + "Song A;Artist;Album;First;3:20\nSong B;Artist;Album;Second;200\nSong C;Artist;;First;\n";
        var lists = ImportFileReader.Read("export.csv", new MemoryStream(Encoding.UTF8.GetBytes(csv)));
        Assert.Equal(["First", "Second"], lists.Select(list => list.Name));
        Assert.Equal(200, lists[0].Tracks[0].Seconds);
        Assert.Equal(200, lists[1].Tracks[0].Seconds);
        Assert.Equal(2, lists[0].Tracks.Count);
        Assert.StartsWith("song:", lists[0].Tracks[0].Key);
    }

    [Fact]
    public void ACsvWithoutTitleOrArtistSaysWhatItNeeds()
    {
        var ex = Assert.Throws<FormatException>(() =>
            ImportFileReader.Read("x.csv", new MemoryStream(Encoding.UTF8.GetBytes("Foo,Bar\n1,2\n"))));
        Assert.Contains("title column and an artist column", ex.Message);
    }

    private const string YourLibrary = """
        {"tracks":[{"artist":"Rick Astley","album":"Whenever You Need Somebody","track":"Never Gonna Give You Up","uri":"spotify:track:4uLU6hMCjMI75M1A2tKUQC"}],
         "albums":[],"shows":[],"episodes":[],"bannedTracks":[],"artists":[],"bannedArtists":[],"other":[]}
        """;

    private const string Playlist1 = """
        {"playlists":[{"name":"Gym","lastModifiedDate":"2026-01-02","description":null,"numberOfFollowers":0,
          "items":[{"track":{"trackName":"Teardrop","artistName":"Massive Attack","albumName":"Mezzanine","trackUri":"spotify:track:7ouMYWpwJ422jRcDASZB7P"},
                    "episode":null,"localTrack":null,"addedDate":"2026-01-01"},
                   {"track":null,"episode":{"episodeName":"A podcast"},"localTrack":null}]}]}
        """;

    [Fact]
    public void SpotifysOwnDataExportIsRead_AsJsonOrItsZip()
    {
        Assert.Equal("Liked Songs", Assert.Single(ImportFileReader.Read("YourLibrary.json", new MemoryStream(Encoding.UTF8.GetBytes(YourLibrary)))).Name);

        var zip = new MemoryStream();
        using (var archive = new ZipArchive(zip, ZipArchiveMode.Create, leaveOpen: true))
        {
            foreach (var (name, text) in new[] { ("Spotify Account Data/YourLibrary.json", YourLibrary), ("Spotify Account Data/Playlist1.json", Playlist1),
                         ("Spotify Account Data/StreamingHistory_music_0.json", "[]") })
            {
                using var writer = new StreamWriter(archive.CreateEntry(name).Open());
                writer.Write(text);
            }
        }
        zip.Position = 0;
        var lists = ImportFileReader.Read("my_spotify_data.zip", zip);
        Assert.Equal(["Gym", "Liked Songs"], lists.Select(list => list.Name).Order());
        var gym = lists.Single(list => list.Name == "Gym");
        Assert.Equal("spotify:7ouMYWpwJ422jRcDASZB7P", Assert.Single(gym.Tracks).Key);
    }

    [Theory]
    [InlineData("213573", true, 214)]
    [InlineData("3:20", false, 200)]
    [InlineData("1:02:03", false, 3723)]
    [InlineData("245", false, 245)]
    [InlineData("245000", false, 245)]
    [InlineData("", false, null)]
    [InlineData("soon", false, null)]
    public void LengthsAreReadInEveryUsualForm(string value, bool ms, int? seconds) =>
        Assert.Equal(seconds, ImportFileReader.Seconds(value, ms));
}
