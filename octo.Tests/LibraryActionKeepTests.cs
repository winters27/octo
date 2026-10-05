using System.Net;
using System.Text;
using System.Text.Json;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Http;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Logging.Abstractions;
using Octo.Controllers;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services.Fingerprint;
using Octo.Services.Library;
using Octo.Services.Subsonic;

namespace Octo.Tests;

/// <summary>Shared by the #47 tests: a person Octo asked about one track, and a stand-in Navidrome.</summary>
internal static class ReviewFixtures
{
    public static readonly VerificationResult Unknown =
        new() { Reason = InconclusiveReason.NoEntry, Fingerprint = "AQADtEqk", DurationSeconds = 330 };

    public static NoticeQueue AskedAbout(string user, string navidromeId)
    {
        var queue = new NoticeQueue();
        queue.AddReview(user, "/music/teardrop.flac",
            new Song { Artist = "Massive Attack", Title = "Teardrop", Album = "Mezzanine" }, Unknown);
        var key = NoticeQueue.ReviewKey(user, "/music/teardrop.flac");
        queue.SetNavidromeId(key, navidromeId);
        queue.MarkQueued([key]);
        return queue;
    }

    public static HttpResponseMessage Json(string body) =>
        new(HttpStatusCode.OK) { Content = new StringContent(body, Encoding.UTF8, "application/json") };

    public sealed class OneClientFactory(HttpMessageHandler handler) : IHttpClientFactory
    {
        public HttpClient CreateClient(string name) => new(handler, disposeHandler: false);
    }
}

/// <summary>
/// Keep (#47) is an answer, not an operation: it touches no file, so it runs in rehearsal mode
/// too, and on a track Octo never asked about it is just a rating.
/// </summary>
public class LibraryActionKeepTests
{
    private static LibraryActionExecutor Executor(LibraryActionSettings settings, LibraryActionJournal journal,
        NoticeQueue? notices) => new(
        resolver: null!, quarantine: null!, journal: journal, library: null!, ids: null!,
        rejectedPeers: null!, acquisitions: null!,
        settings: TestOptions.Monitor(settings),
        soulseek: TestOptions.Monitor(new SoulseekSettings()),
        subsonicSettings: TestOptions.Monitor(new SubsonicSettings()),
        logger: NullLogger<LibraryActionExecutor>.Instance,
        notices: notices);

    private static LibraryActionSettings Settings() => new()
    {
        Enabled = true, ReviewEnabled = true, DryRun = true, AllowedUsers = ["alice"],
    };

    [Fact]
    public async Task Keep_OnATrackOctoNeverAskedAbout_IsSkippedAndLeavesNoRecord()
    {
        var journal = new LibraryActionJournal();
        var executor = Executor(Settings(), journal, ReviewFixtures.AskedAbout("alice", "nd-asked"));

        var outcome = await executor.ApplyAsync(new LibraryActionRequest(LibraryAction.Keep, "nd-other", "alice"));

        Assert.Equal(LibraryActionState.Skipped, outcome.State);
        Assert.True(outcome.Consumed);
        Assert.Empty(journal.Recent());
    }

    [Fact]
    public async Task Keep_OnATrackOctoAskedAbout_AnswersItEvenInRehearsal()
    {
        var journal = new LibraryActionJournal();
        var notices = ReviewFixtures.AskedAbout("alice", "nd-asked");
        var executor = Executor(Settings(), journal, notices);

        var outcome = await executor.ApplyAsync(new LibraryActionRequest(LibraryAction.Keep, "nd-asked", "alice"));

        Assert.Equal(LibraryActionState.Applied, outcome.State);
        var entry = Assert.Single(journal.Recent());
        Assert.Equal(LibraryAction.Keep, entry.Action);
        Assert.Equal("Teardrop", entry.Title);
        Assert.False(entry.DryRun);
        Assert.Equal(NoticeState.Kept, notices.ForUser("alice", NoticeKind.Review).Single().State);
    }

    [Fact]
    public async Task Keep_SwitchedOff_IsNotApplied()
    {
        var settings = Settings();
        settings.Actions = [new() { Action = LibraryAction.Keep, Enabled = false }];
        var notices = ReviewFixtures.AskedAbout("alice", "nd-asked");

        var outcome = await Executor(settings, new LibraryActionJournal(), notices)
            .ApplyAsync(new LibraryActionRequest(LibraryAction.Keep, "nd-asked", "alice"));

        Assert.Equal(LibraryActionState.Skipped, outcome.State);
        Assert.Equal(NoticeState.Queued, notices.ForUser("alice", NoticeKind.Review).Single().State);
    }

    /// <summary>
    /// Five stars is also what someone who loves a track gives it, and Keep changed nothing
    /// that needs undoing, so its rating stays. Any other consumed rating is cleared.
    /// </summary>
    [Fact]
    public async Task RatingWorker_NeverClearsAKeep_ButClearsAnotherConsumedRating()
    {
        var settings = Settings();
        settings.RatingsEnabled = true;
        var navidrome = new RatingNavidrome();
        using var services = new ServiceCollection()
            .AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(navidrome))
            .AddSingleton<Microsoft.Extensions.Options.IOptionsMonitor<SubsonicSettings>>(
                TestOptions.Monitor(new SubsonicSettings { Url = "http://navidrome.test" }))
            .AddSingleton<IHttpContextAccessor>(new HttpContextAccessor())
            .AddScoped<SubsonicProxyService>()
            .BuildServiceProvider();
        var worker = new LibraryActionRatingWorker(
            Executor(settings, new LibraryActionJournal(), ReviewFixtures.AskedAbout("alice", "nd-keep")),
            services.GetRequiredService<IServiceScopeFactory>(), TestOptions.Monitor(settings),
            NullLogger<LibraryActionRatingWorker>.Instance);

        await worker.StartAsync(CancellationToken.None);
        try
        {
            Assert.True(worker.TryEnqueue(new RatingActionRequest(LibraryAction.Keep, "nd-keep", "alice", "alice", "t", "s")));
            // Delete is not switched on, so this one is Skipped: consumed, and its rating cleared.
            Assert.True(worker.TryEnqueue(new RatingActionRequest(LibraryAction.Delete, "nd-delete", "alice", "alice", "t", "s")));
            await navidrome.Cleared.Task.WaitAsync(TimeSpan.FromSeconds(10));
        }
        finally
        {
            await worker.StopAsync(CancellationToken.None);
        }

        Assert.Equal(["nd-delete"], navidrome.ClearedIds);
    }

    private sealed class RatingNavidrome : HttpMessageHandler
    {
        public List<string> ClearedIds { get; } = [];
        public TaskCompletionSource Cleared { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var query = System.Web.HttpUtility.ParseQueryString(request.RequestUri!.Query);
            if (request.RequestUri.AbsolutePath.EndsWith("/rest/setRating", StringComparison.Ordinal) && query["rating"] == "0")
            {
                lock (ClearedIds) ClearedIds.Add(query["id"] ?? "");
                Cleared.TrySetResult();
            }
            return Task.FromResult(ReviewFixtures.Json("{\"subsonic-response\":{\"status\":\"ok\"}}"));
        }
    }

    /// <summary>
    /// A notice playlist named like an action playlist would have every track Octo asked about
    /// acted on. Octo's own questions are never commands, whatever they are called.
    /// </summary>
    [Fact]
    public void WantedPlaylists_ANoticePlaylistNamedLikeAnAction_IsNeverACommand()
    {
        var settings = new LibraryActionSettings
        {
            Enabled = true, ReviewEnabled = true,
            PlaylistPrefix = "* ", NoticePrefix = "* ", ReviewPlaylistName = "Delete",
            Actions = [new() { Action = LibraryAction.Delete, Enabled = true, Rating = 1 }],
        };

        var wanted = LibraryActionPlaylistWorker.WantedPlaylists(settings);

        Assert.DoesNotContain("* Delete", wanted.Keys);
        Assert.Equal(LibraryAction.Keep, wanted["* Keep"].Action);
    }

    [Fact]
    public void PlaylistNames_ReadsXmlAsWellAsJson()
    {
        var xml = Encoding.UTF8.GetBytes(
            "<subsonic-response xmlns=\"http://subsonic.org/restapi\" status=\"ok\"><playlists>"
            + "<playlist id=\"1\" name=\"\U0001F6E0 Delete\"/><playlist id=\"2\" name=\"▸ Review\"/>"
            + "</playlists></subsonic-response>");
        var json = Encoding.UTF8.GetBytes(
            "{\"subsonic-response\":{\"status\":\"ok\",\"playlists\":{\"playlist\":[{\"id\":\"1\",\"name\":\"\U0001F6E0 Delete\"}]}}}");

        Assert.Equal(["\U0001F6E0 Delete", "▸ Review"], SubsonicController.PlaylistNames(xml, "xml"));
        Assert.Equal(["\U0001F6E0 Delete"], SubsonicController.PlaylistNames(json, "json"));
        Assert.Empty(SubsonicController.PlaylistNames(Encoding.UTF8.GetBytes("<not xml"), "xml"));
    }
}

/// <summary>
/// What goes back to AcoustID when a person keeps a track (#47): only with consent and both
/// keys, never in rehearsal, and only for what AcoustID could not place.
/// </summary>
public class AcoustIdSubmissionTests
{
    [Theory]
    [InlineData(true, false, "app", "user", true)]
    [InlineData(false, false, "app", "user", false)]
    [InlineData(true, true, "app", "user", false)]
    [InlineData(true, false, "", "user", false)]
    [InlineData(true, false, "app", "", false)]
    public void MaySubmit_NeedsConsentBothKeysAndARealRun(bool consent, bool dryRun, string appKey, string userKey, bool expected)
        => Assert.Equal(expected, NoticePlaylistWorker.MaySubmit(
            new LibraryActionSettings { DryRun = dryRun },
            new SoulseekSettings { SubmitConfirmedFingerprints = consent, AcoustIdApiKey = appKey, AcoustIdUserApiKey = userKey }));

    /// <summary>
    /// A track AcoustID confidently named as something else was kept for some reason other than
    /// "AcoustID is missing this", and a shortened fingerprint must never land beside the
    /// standard ones.
    /// </summary>
    [Theory]
    [InlineData(InconclusiveReason.NoEntry, 120, true)]
    [InlineData(InconclusiveReason.BelowThreshold, 120, true)]
    [InlineData(InconclusiveReason.SourceDisagreed, 120, false)]
    [InlineData(InconclusiveReason.NoEntry, 60, false)]
    public void Submittable_OnlyWhatAcoustIdCouldNotPlace_AtTheStandardLength(InconclusiveReason cause, int seconds, bool expected)
        => Assert.Equal(expected, NoticePlaylistWorker.Submittable(
            new NoticeEntry { Cause = cause, Fingerprint = "AQADtEqk", DurationSeconds = 330 },
            new SoulseekSettings { FingerprintSeconds = seconds }));

    [Fact]
    public void Submittable_WithoutALengthOrAFingerprint_IsNot()
    {
        var soulseek = new SoulseekSettings { FingerprintSeconds = 120 };

        Assert.False(NoticePlaylistWorker.Submittable(
            new NoticeEntry { Cause = InconclusiveReason.NoEntry, Fingerprint = "AQADtEqk" }, soulseek));
        Assert.False(NoticePlaylistWorker.Submittable(
            new NoticeEntry { Cause = InconclusiveReason.NoEntry, DurationSeconds = 330 }, soulseek));
    }

    [Fact]
    public void BuildSubmitForm_NumbersEachItemAndLeavesOutAnUnknownFormat()
    {
        var form = AcoustIdClient.BuildSubmitForm("app", "user",
        [
            new AcoustIdSubmission("AQAB1", 330, "f200a9a9-6f0a-4a8b-9f5e-000000000001", "flac"),
            new AcoustIdSubmission("AQAB2", 200, "b39f9fe4-6f0a-4a8b-9f5e-000000000002", null),
        ]);
        var fields = form.ToDictionary(pair => pair.Key, pair => pair.Value);

        Assert.Equal("app", fields["client"]);
        Assert.Equal("user", fields["user"]);
        Assert.Equal("json", fields["format"]);
        Assert.StartsWith("octo-", fields["clientversion"]);
        Assert.Equal("330", fields["duration.0"]);
        Assert.Equal("AQAB1", fields["fingerprint.0"]);
        Assert.Equal("f200a9a9-6f0a-4a8b-9f5e-000000000001", fields["mbid.0"]);
        Assert.Equal("flac", fields["fileformat.0"]);
        Assert.Equal("200", fields["duration.1"]);
        Assert.False(fields.ContainsKey("fileformat.1"));
    }
}

/// <summary>
/// The MusicBrainz recording a kept fingerprint is submitted with. MusicBrainz holds
/// near-duplicates, so anything but exactly one fit is no answer: a guess is never sent with
/// someone's name on it.
/// </summary>
public class MusicBrainzRecordingPickTests
{
    private static JsonElement Recordings(params string[] recordings) =>
        JsonDocument.Parse($"{{\"recordings\":[{string.Join(",", recordings)}]}}").RootElement;

    private static string Recording(string id, string title, int lengthMs, string artist = "Massive Attack",
        int score = 100, string disambiguation = "", bool video = false) =>
        JsonSerializer.Serialize(new Dictionary<string, object>
        {
            ["id"] = id, ["score"] = score, ["title"] = title, ["length"] = lengthMs, ["video"] = video,
            ["disambiguation"] = disambiguation, ["artist-credit"] = new[] { new { name = artist } },
        });

    /// <summary>The real pair: two "Teardrop" recordings by Massive Attack, 27 ms apart.</summary>
    [Fact]
    public void Pick_TwoRecordingsThatBothFit_IsNoAnswer()
        => Assert.Null(MusicBrainzClient.Pick(Recordings(
            Recording("f200a9a9", "Teardrop", 314813),
            Recording("b39f9fe4", "Teardrop", 314786)), "Massive Attack", "Teardrop", 315));

    [Fact]
    public void Pick_OtherVersionsDoNotCount_SoOneFitIsTheAnswer()
        => Assert.Equal("f200a9a9", MusicBrainzClient.Pick(Recordings(
            Recording("f200a9a9", "Teardrop", 314813),
            Recording("live", "Teardrop", 316000, disambiguation: "live, 1998-12-05: Brixton Academy"),
            Recording("remix", "Teardrop (Mad Professor mix)", 314000)), "Massive Attack", "Teardrop", 315));

    [Fact]
    public void Pick_IgnoresWeakScoresVideosOtherArtistsAndOtherLengths()
        => Assert.Equal("keep", MusicBrainzClient.Pick(Recordings(
            Recording("keep", "Teardrop", 314813),
            Recording("weak", "Teardrop", 314813, score: 80),
            Recording("video", "Teardrop", 314813, video: true),
            Recording("cover", "Teardrop", 314813, artist: "Elbow"),
            Recording("long", "Teardrop", 330000)), "Massive Attack", "Teardrop", 315));

    [Fact]
    public void Pick_NoRecordings_IsNoAnswer()
        => Assert.Null(MusicBrainzClient.Pick(JsonDocument.Parse("{}").RootElement, "Massive Attack", "Teardrop", 315));
}

/// <summary>
/// Navidrome's admin token lapses after SessionTimeout (48 hours by default). Before this, every
/// native call answered 401 from then on, read as "nothing there", and library actions stalled
/// without a word.
/// </summary>
public class NavidromePlaylistApiTests
{
    private sealed class TokenNavidrome(Func<string, HttpStatusCode> playlists) : HttpMessageHandler
    {
        private int _logins;
        public int Logins => _logins;
        public List<string> Bearers { get; } = [];

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            if (request.RequestUri!.AbsolutePath == "/auth/login")
            {
                var login = Interlocked.Increment(ref _logins);
                return Task.FromResult(ReviewFixtures.Json(
                    $"{{\"token\":\"jwt-{login}\",\"isAdmin\":true,\"username\":\"admin\"}}"));
            }
            // A login also sets off a music-folder lookup; only the playlist calls are under test.
            if (!request.RequestUri.AbsolutePath.StartsWith("/api/playlist", StringComparison.Ordinal))
                return Task.FromResult(ReviewFixtures.Json("[]"));
            var bearer = request.Headers.TryGetValues("X-Nd-Authorization", out var values) ? values.First() : "";
            lock (Bearers) Bearers.Add(bearer);
            var status = playlists(bearer);
            return Task.FromResult(status == HttpStatusCode.OK
                ? ReviewFixtures.Json("[{\"id\":\"p1\",\"name\":\"Review\",\"ownerName\":\"alice\"}]")
                : new HttpResponseMessage(status));
        }
    }

    private static NavidromePlaylistApi Api(HttpMessageHandler navidrome)
    {
        var factory = new ReviewFixtures.OneClientFactory(navidrome);
        var subsonic = TestOptions.Monitor(new SubsonicSettings
        {
            Url = "http://navidrome.test", AdminUsername = "admin", AdminPassword = "secret",
            AutoDetectDownloadPath = false,
        });
        var identity = new NavidromeIdentityService(subsonic, factory, NullLogger<NavidromeIdentityService>.Instance);
        return new NavidromePlaylistApi(factory, identity, subsonic, NullLogger<NavidromePlaylistApi>.Instance);
    }

    [Fact]
    public async Task ListPlaylists_ALapsedToken_LogsInAgainAndRetriesOnce()
    {
        var navidrome = new TokenNavidrome(bearer => bearer == "Bearer jwt-1" ? HttpStatusCode.Unauthorized : HttpStatusCode.OK);

        var playlists = await Api(navidrome).ListPlaylistsAsync(CancellationToken.None);

        Assert.Equal("Review", Assert.Single(playlists).Name);
        Assert.Equal(2, navidrome.Logins);
        Assert.Equal(["Bearer jwt-1", "Bearer jwt-2"], navidrome.Bearers);
    }

    [Fact]
    public async Task ListPlaylists_AFreshTokenRefusedToo_GivesUpAfterOneRetry()
    {
        var navidrome = new TokenNavidrome(_ => HttpStatusCode.Unauthorized);

        Assert.Empty(await Api(navidrome).ListPlaylistsAsync(CancellationToken.None));
        Assert.Equal(2, navidrome.Logins);
        Assert.Equal(2, navidrome.Bearers.Count);
    }

    [Fact]
    public async Task AddTracks_SendsTheIdsNavidromeExpects()
    {
        string? body = null;
        var navidrome = new CapturingNavidrome(request => body = request);

        Assert.True(await Api(navidrome).AddTracksAsync("p1", ["a", "b"], CancellationToken.None));
        Assert.Equal("{\"ids\":[\"a\",\"b\"]}", body);
    }

    [Fact]
    public async Task ListSongs_ANavidromeThatTimesOut_IsNoAnswer()
    {
        // HttpClient reports its own timeout as a cancellation the caller never asked for.
        var navidrome = new SlowNavidrome();
        Assert.Null(await Api(navidrome).ListSongsAsync(0, 1000, CancellationToken.None));
    }

    private sealed class SlowNavidrome : HttpMessageHandler
    {
        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            if (request.RequestUri!.AbsolutePath == "/auth/login")
                return Task.FromResult(ReviewFixtures.Json("{\"token\":\"jwt\",\"isAdmin\":true,\"username\":\"admin\"}"));
            if (!request.RequestUri.AbsolutePath.StartsWith("/api/song", StringComparison.Ordinal))
                return Task.FromResult(ReviewFixtures.Json("[]"));
            throw new TaskCanceledException("The request was canceled due to the configured HttpClient.Timeout",
                new TimeoutException());
        }
    }

    private sealed class CapturingNavidrome(Action<string> capture) : HttpMessageHandler
    {
        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            if (request.RequestUri!.AbsolutePath == "/auth/login")
                return ReviewFixtures.Json("{\"token\":\"jwt\",\"isAdmin\":true,\"username\":\"admin\"}");
            if (request.Content is not null) capture(await request.Content.ReadAsStringAsync(ct));
            return ReviewFixtures.Json("{\"added\":2}");
        }
    }
}

/// <summary>
/// Where a star counts as a command (#47). With Review on, a star is a command only on a track
/// Octo asked the person about; everywhere else it is just a rating, relayed and left alone.
/// </summary>
public sealed class SetRatingScopeTests
{
    private sealed class OkNavidrome : HttpMessageHandler
    {
        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
            => Task.FromResult(ReviewFixtures.Json("{\"subsonic-response\":{\"status\":\"ok\",\"version\":\"1.16.1\"}}"));
    }

    private sealed class RatingWebFactory(string scope) : WebApplicationFactory<Program>
    {
        private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-rating-web-" + Guid.NewGuid());
        private readonly NoticeQueue _notices = ReviewFixtures.AskedAbout("alice", "nd-asked");

        public LibraryActionRatingWorker Ratings => Services.GetRequiredService<LibraryActionRatingWorker>();

        protected override void ConfigureWebHost(IWebHostBuilder builder)
        {
            Directory.CreateDirectory(_directory);
            builder.ConfigureAppConfiguration((_, configuration) => configuration.AddInMemoryCollection(
                new Dictionary<string, string?>
                {
                    ["Subsonic:Url"] = "http://navidrome.test",
                    ["Subsonic:AutoDetectDownloadPath"] = "false",
                    ["Library:DownloadPath"] = _directory,
                    ["Octo:StateDirectory"] = _directory,
                    ["LibraryActions:Enabled"] = "true",
                    ["LibraryActions:RatingsEnabled"] = "true",
                    ["LibraryActions:ReviewEnabled"] = "true",
                    ["LibraryActions:RatingsScope"] = scope,
                    ["LibraryActions:AllowedUsers:0"] = "alice",
                    ["LibraryActions:Actions:0:Action"] = "Delete",
                    ["LibraryActions:Actions:0:Enabled"] = "true",
                    ["LibraryActions:Actions:0:Rating"] = "1",
                }));
            builder.ConfigureServices(services =>
            {
                services.RemoveAll<IHostedService>();
                services.RemoveAll<IHttpClientFactory>();
                services.AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(new OkNavidrome()));
                services.RemoveAll<NoticeQueue>();
                services.AddSingleton(_notices);
            });
        }

        public override async ValueTask DisposeAsync()
        {
            await base.DisposeAsync();
            try { Directory.Delete(_directory, recursive: true); } catch { /* best effort */ }
        }
    }

    private static string Rate(string id) =>
        $"/rest/setRating.view?u=alice&t=token&s=salt&v=1.16.1&c=test&f=json&id={id}&rating=1";

    [Theory]
    [InlineData("NoticeOnly")]
    [InlineData("Auto")]
    public async Task SetRating_WithReviewOn_IgnoresATrackOctoDidNotAskAbout(string scope)
    {
        await using var factory = new RatingWebFactory(scope);
        using var client = factory.CreateClient();

        (await client.GetAsync(Rate("nd-elsewhere"))).EnsureSuccessStatusCode();
        Assert.Equal(0, factory.Ratings.Pending);

        (await client.GetAsync(Rate("nd-asked"))).EnsureSuccessStatusCode();
        Assert.Equal(1, factory.Ratings.Pending);
    }

    [Fact]
    public async Task SetRating_Global_CountsOnAnyTrack()
    {
        await using var factory = new RatingWebFactory("Global");
        using var client = factory.CreateClient();

        (await client.GetAsync(Rate("nd-elsewhere"))).EnsureSuccessStatusCode();

        Assert.Equal(1, factory.Ratings.Pending);
    }
}
