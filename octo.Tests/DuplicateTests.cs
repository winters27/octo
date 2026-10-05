using System.Net;
using System.Text.Json;
using Microsoft.Extensions.Logging.Abstractions;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services.Fingerprint;
using Octo.Services.Library;
using Octo.Services.Subsonic;

namespace Octo.Tests;

/// <summary>
/// Duplicates (#53): copies of one recording in one version, the copy worth keeping first.
/// Octo only points them out, so the rules that matter are what counts as a copy, which copy is
/// suggested, and that a question a person settled stays settled.
/// </summary>
public class DuplicateScanTests
{
    private static LibraryTrack Track(string id, string title = "Teardrop", string artist = "Massive Attack",
        string recording = "rec-1", string suffix = "flac", int bitRate = 1000, int duration = 330) =>
        new(id, title, artist, "Mezzanine", recording, suffix, bitRate, duration);

    [Fact]
    public void FindGroups_SameRecordingSameVersion_IsAGroupWithTheKeeperFirst()
    {
        var group = Assert.Single(DuplicateScanWorker.FindGroups(
        [
            Track("b", suffix: "mp3", bitRate: 320),
            Track("a", suffix: "flac", bitRate: 1011),
        ]));

        Assert.Equal("dup|a,b", group.Key);
        Assert.Equal(["a", "b"], group.Tracks.Select(track => track.Id));
    }

    // Since 2026-10-05 a copy is what Library health calls one (LibraryHealth.FindDuplicates, the
    // Octo app's rule): tags that name one recording, or the same title, artist and version with
    // lengths within 3 seconds. Before, only a shared recording id in the same version counted.

    [Fact]
    public void FindGroups_AnotherVersionWithoutASharedCode_IsNotAGroup()
        => Assert.Empty(DuplicateScanWorker.FindGroups([Track("a", recording: ""), Track("b", title: "Teardrop (Live)", recording: "")]));

    /// <summary>The tags say it is one recording, so it is, whatever the title says: the app's rule.</summary>
    [Fact]
    public void FindGroups_ASharedRecordingIdAtACloseLength_IsAGroupWhateverTheTitle()
        => Assert.Single(DuplicateScanWorker.FindGroups([Track("a"), Track("b", title: "Tear Drop (Album Version)", duration: 331)]));

    [Fact]
    public void FindGroups_ASharedRecordingIdAtAFarLength_IsATaggingMistake()
        => Assert.Empty(DuplicateScanWorker.FindGroups([Track("a"), Track("b", title: "Angel", duration: 379)]));

    [Fact]
    public void FindGroups_DifferentArtistWithoutASharedCode_IsNotAGroup()
        => Assert.Empty(DuplicateScanWorker.FindGroups([Track("a", recording: ""), Track("b", artist: "Elbow", recording: "")]));

    [Fact]
    public void FindGroups_NoRecordingId_SameTitleArtistAndLength_IsAGroup()
        => Assert.Equal("dup|a,b", Assert.Single(DuplicateScanWorker.FindGroups([Track("a", recording: ""), Track("b", recording: "", duration: 333)])).Key);

    [Fact]
    public void FindGroups_NoRecordingId_LengthsFourSecondsApart_IsNotAGroup()
        => Assert.Empty(DuplicateScanWorker.FindGroups([Track("a", recording: ""), Track("b", recording: "", duration: 334)]));

    [Fact]
    public void FindGroups_ASharedIsrc_IsAGroup()
        => Assert.Single(DuplicateScanWorker.FindGroups(
            [Track("a", recording: "") with { Isrcs = ["GBAAA9800003"] }, Track("b", title: "Tear Drop", recording: "") with { Isrcs = ["GB-AAA-98-00003"] }]));

    [Fact]
    public void FindGroups_ThreeCopiesTwoVersions_GroupsOnlyTheMatchingPair()
    {
        var group = Assert.Single(DuplicateScanWorker.FindGroups(
        [
            Track("a", recording: ""),
            Track("b", title: "Teardrop (Mad Professor mix)", recording: ""),
            Track("c", title: "Teardrop - Remastered 2011", suffix: "mp3", bitRate: 320, recording: ""),
        ]));

        Assert.Equal(["a", "c"], group.Tracks.Select(track => track.Id));
    }

    [Fact]
    public void RankForKeeping_LosslessThenBitrateThenDuration()
    {
        var ranked = DuplicateScanWorker.RankForKeeping(
        [
            Track("mp3-high", suffix: "mp3", bitRate: 320),
            Track("aac", suffix: "m4a", bitRate: 256),
            Track("alac", suffix: "m4a", bitRate: 900),
            Track("flac-odd-length", suffix: "flac", bitRate: 1000, duration: 400),
            Track("flac", suffix: "flac", bitRate: 1000, duration: 330),
            Track("mp3-low", suffix: "mp3", bitRate: 128),
        ]);

        Assert.Equal(["flac", "flac-odd-length", "alac", "mp3-high", "aac", "mp3-low"],
            ranked.Select(track => track.Id));
    }

    // ---- fake lossless copies ---------------------------------------------------------------

    private static SpectrumReport Fake(double cutoff) =>
        new(SpectrumVerdict.LikelyLossy, 44100, cutoff, "a cliff", SpectrumAnalyzer.EstimateFor(cutoff));

    private static readonly SpectrumReport Real = new(SpectrumVerdict.Genuine, 44100, null, "audio up to the top of the band");

    [Fact]
    public void RankForKeeping_ATranscodedFlacNeverOutranksAGenuineOne()
    {
        var ranked = DuplicateScanWorker.RankForKeeping(
        [
            Track("fake", bitRate: 1100) with { TranscodedFrom = "about 128 kbps MP3" },
            Track("real", bitRate: 900),
            Track("mp3", suffix: "mp3", bitRate: 320),
        ]);

        Assert.Equal(["real", "fake", "mp3"], ranked.Select(track => track.Id));
    }

    [Fact]
    public async Task CheckTranscodes_ReranksAGroupWithTwoLosslessCopies()
    {
        var group = Assert.Single(DuplicateScanWorker.FindGroups(
            [Track("fake", bitRate: 1100), Track("real", bitRate: 900), Track("mp3", suffix: "mp3", bitRate: 320)]));
        Assert.Equal("fake", group.Tracks[0].Id);
        var asked = new List<string>();

        var checkedGroups = await DuplicateScanWorker.CheckTranscodesAsync([group], track =>
        {
            asked.Add(track.Id);
            return Task.FromResult<SpectrumReport?>(track.Id == "fake" ? Fake(16900) : Real);
        });

        var ranked = Assert.Single(checkedGroups);
        Assert.Equal(group.Key, ranked.Key);
        Assert.Equal(["real", "fake", "mp3"], ranked.Tracks.Select(track => track.Id));
        Assert.Equal("about 128 kbps MP3", ranked.Tracks[1].TranscodedFrom);
        // Only the lossless copies are decoded.
        Assert.Equal(["fake", "real"], asked.Order());
    }

    /// <summary>One lossless copy outranks the lossy ones whatever its spectrum says, so it is
    /// never decoded: the check costs nothing outside groups where it can change the answer.</summary>
    [Fact]
    public async Task CheckTranscodes_LeavesAGroupWithOneLosslessCopyAlone()
    {
        var group = Assert.Single(DuplicateScanWorker.FindGroups([Track("flac"), Track("mp3", suffix: "mp3", bitRate: 320)]));

        var checkedGroups = await DuplicateScanWorker.CheckTranscodesAsync([group],
            _ => throw new InvalidOperationException("nothing should be decoded"));

        Assert.Same(group, Assert.Single(checkedGroups));
    }

    [Fact]
    public async Task CheckTranscodes_ACopyThatCouldNotBeJudgedCountsAsGenuine()
    {
        var group = Assert.Single(DuplicateScanWorker.FindGroups([Track("a", bitRate: 1100), Track("b", bitRate: 900)]));

        var checkedGroups = await DuplicateScanWorker.CheckTranscodesAsync([group],
            track => Task.FromResult<SpectrumReport?>(track.Id == "a" ? null : SpectrumReport.Unknown("no clear cutoff")));

        Assert.Equal(["a", "b"], Assert.Single(checkedGroups).Tracks.Select(track => track.Id));
        Assert.All(Assert.Single(checkedGroups).Tracks, track => Assert.Null(track.TranscodedFrom));
    }

    [Fact]
    public void SyncDuplicates_SaysWhichCopyIsTranscoded()
    {
        var queue = new NoticeQueue();
        var group = new DuplicateGroup("dup|a,b", [Track("a", bitRate: 900), Track("b", bitRate: 1100) with { TranscodedFrom = "about 192 kbps" }]);

        queue.SyncDuplicates([group], ["alice"], complete: true);

        var entries = queue.ForUser("alice", NoticeKind.Duplicates).OrderBy(entry => entry.Order).ToList();
        Assert.Equal("FLAC, 1100 kbps, likely transcoded from about 192 kbps, also in the library as FLAC, 900 kbps", entries[1].Reason);
    }

    [Fact]
    public void ParsePage_KeepsEveryLibraryTrack_WithItsCodes()
    {
        var tracks = new List<LibraryTrack>();
        var root = JsonDocument.Parse("""
            {"subsonic-response":{"status":"ok","searchResult3":{"song":[
              {"id":"a","title":"Teardrop","artist":"Massive Attack","album":"Mezzanine","musicBrainzId":"rec-1","suffix":"flac","bitRate":1011,"duration":330,"isrc":["GBAAA9800003"]},
              {"id":"b","title":"Angel","artist":"Massive Attack","suffix":"mp3","bitRate":320,"duration":379},
              {"id":"x","title":"Outside","artist":"Someone","isExternal":true}
            ]}}}
            """).RootElement;

        Assert.Equal(3, DuplicateScanWorker.ParsePage(root, tracks));
        Assert.Equal(["a", "b"], tracks.Select(track => track.Id));
        Assert.Equal(new LibraryTrack("a", "Teardrop", "Massive Attack", "Mezzanine", "rec-1", "flac", 1011, 330) { Isrcs = tracks[0].Isrcs }, tracks[0]);
        Assert.Equal(["GBAAA9800003"], tracks[0].Isrcs);
        Assert.Equal("", tracks[1].RecordingId);
    }

    [Theory]
    [InlineData("""{"subsonic-response":{"status":"failed","error":{"code":40}}}""", null)]
    [InlineData("""{"subsonic-response":{"status":"ok","searchResult3":{}}}""", 0)]
    public void ParsePage_ARefusalIsNotTheEnd(string body, int? expected)
        => Assert.Equal(expected, DuplicateScanWorker.ParsePage(JsonDocument.Parse(body).RootElement, []));

    private sealed class LibraryNavidrome(int tracks, bool failSecondPage = false) : HttpMessageHandler
    {
        public List<string> Queries { get; } = [];

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var path = request.RequestUri!.AbsolutePath;
            if (path == "/auth/login")
                return Task.FromResult(ReviewFixtures.Json(
                    """{"token":"jwt","isAdmin":true,"username":"admin","subsonicToken":"tok","subsonicSalt":"salt"}"""));
            if (path != "/rest/search3") return Task.FromResult(ReviewFixtures.Json("[]"));

            Queries.Add(request.RequestUri.Query);
            var query = System.Web.HttpUtility.ParseQueryString(request.RequestUri.Query);
            var offset = int.Parse(query["songOffset"]!);
            if (failSecondPage && offset > 0)
                return Task.FromResult(new HttpResponseMessage(HttpStatusCode.InternalServerError));
            var rows = Enumerable.Range(offset, Math.Max(0, Math.Min(DuplicateScanWorker.PageSize, tracks - offset)))
                .Select(i => $$"""{"id":"t{{i}}","title":"Song {{i}}","artist":"A","musicBrainzId":"rec-{{i}}","suffix":"flac","bitRate":900,"duration":200}""");
            return Task.FromResult(ReviewFixtures.Json(
                """{"subsonic-response":{"status":"ok","searchResult3":{"song":[""" + string.Join(",", rows) + "]}}}"));
        }
    }

    private static DuplicateScanWorker Worker(HttpMessageHandler navidrome)
    {
        var factory = new ReviewFixtures.OneClientFactory(navidrome);
        var subsonic = TestOptions.Monitor(new SubsonicSettings
        {
            Url = "http://navidrome.test", AdminUsername = "admin", AdminPassword = "secret", AutoDetectDownloadPath = false,
        });
        return new DuplicateScanWorker(new NoticeQueue(),
            new NavidromeIdentityService(subsonic, factory, NullLogger<NavidromeIdentityService>.Instance),
            factory, subsonic, TestOptions.Monitor(new LibraryActionSettings()), NullLogger<DuplicateScanWorker>.Instance);
    }

    /// <summary>The same walk Symfonium makes: the empty query, page after page, until a short page.</summary>
    [Fact]
    public async Task Walk_PagesThroughTheLibraryUntilAShortPage()
    {
        var navidrome = new LibraryNavidrome(tracks: DuplicateScanWorker.PageSize + 7);

        var (tracks, complete) = await Worker(navidrome).WalkAsync(CancellationToken.None);

        Assert.True(complete);
        Assert.Equal(DuplicateScanWorker.PageSize + 7, tracks.Count);
        Assert.Equal(2, navidrome.Queries.Count);
        Assert.All(navidrome.Queries, query => Assert.Contains("query=%22%22", query));
        Assert.Contains("u=admin", navidrome.Queries[0]);
    }

    /// <summary>A walk cut short must not read as a library with nothing left in it.</summary>
    [Fact]
    public async Task Walk_APageThatFails_IsIncomplete()
    {
        var navidrome = new LibraryNavidrome(tracks: DuplicateScanWorker.PageSize * 2, failSecondPage: true);

        var (tracks, complete) = await Worker(navidrome).WalkAsync(CancellationToken.None);

        Assert.False(complete);
        Assert.Equal(DuplicateScanWorker.PageSize, tracks.Count);
    }
}

public class DuplicateNoticeTests
{
    private static LibraryTrack Track(string id, string suffix = "flac", int bitRate = 1011) =>
        new(id, "Teardrop", "Massive Attack", "Mezzanine", "rec-1", suffix, bitRate, 330);

    private static DuplicateGroup Pair(string keeper = "a", string other = "b") =>
        new($"dup|{string.Join(",", new[] { keeper, other }.Order(StringComparer.Ordinal))}",
            [Track(keeper), Track(other, "mp3", 320)]);

    private static List<NoticeEntry> Duplicates(NoticeQueue queue, string user) =>
        queue.ForUser(user, NoticeKind.Duplicates).OrderBy(entry => entry.Order).ToList();

    [Fact]
    public void SyncDuplicates_AsksEachAllowedUserAboutEveryCopy_KeeperFirst()
    {
        var queue = new NoticeQueue();

        Assert.Equal(4, queue.SyncDuplicates([Pair()], ["alice", "bob", " "], complete: true));

        var entries = Duplicates(queue, "alice");
        Assert.Equal(["a", "b"], entries.Select(entry => entry.NavidromeId));
        Assert.All(entries, entry =>
        {
            Assert.Equal(NoticeState.Waiting, entry.State);
            Assert.Equal("dup|a,b", entry.GroupKey);
            Assert.Equal("", entry.LocalPath);
        });
        Assert.Equal("The best of 2 copies: FLAC, 1011 kbps", entries[0].Reason);
        Assert.Equal("MP3, 320 kbps, also in the library as FLAC, 1011 kbps", entries[1].Reason);
        Assert.Equal(2, Duplicates(queue, "bob").Count);
    }

    [Fact]
    public void SyncDuplicates_TheSameGroupAgain_AddsNothing()
    {
        var queue = new NoticeQueue();
        queue.SyncDuplicates([Pair()], ["alice"], complete: true);

        Assert.Equal(0, queue.SyncDuplicates([Pair()], ["alice"], complete: true));
    }

    /// <summary>One side went, so the pair Octo asked about is resolved.</summary>
    [Fact]
    public void SyncDuplicates_ResolvedPair_Expires()
    {
        var queue = new NoticeQueue();
        queue.SyncDuplicates([Pair()], ["alice"], complete: true);

        queue.SyncDuplicates([], ["alice"], complete: true);

        Assert.All(Duplicates(queue, "alice"), entry => Assert.Equal(NoticeState.Expired, entry.State));
    }

    [Fact]
    public void SyncDuplicates_AWalkThatDidNotFinish_SettlesNothing()
    {
        var queue = new NoticeQueue();
        queue.SyncDuplicates([Pair()], ["alice"], complete: true);

        queue.SyncDuplicates([], ["alice"], complete: false);

        Assert.All(Duplicates(queue, "alice"), entry => Assert.Equal(NoticeState.Waiting, entry.State));
    }

    [Fact]
    public void SyncDuplicates_DismissedGroup_StaysDismissed()
    {
        var queue = new NoticeQueue();
        queue.SyncDuplicates([Pair()], ["alice", "bob"], complete: true);
        foreach (var entry in Duplicates(queue, "alice")) queue.Resolve(entry.Key, NoticeState.Dismissed);

        queue.SyncDuplicates([Pair()], ["alice", "bob"], complete: true);

        Assert.All(Duplicates(queue, "alice"), entry => Assert.Equal(NoticeState.Dismissed, entry.State));
        Assert.All(Duplicates(queue, "bob"), entry => Assert.Equal(NoticeState.Waiting, entry.State));
    }

    /// <summary>A copy that vanished for a while (a mount that dropped out) and came back is a
    /// pair again, and is asked about again.</summary>
    [Fact]
    public void SyncDuplicates_AnExpiredGroupFoundAgain_IsAskedAgain()
    {
        var queue = new NoticeQueue();
        queue.SyncDuplicates([Pair()], ["alice"], complete: true);
        queue.SyncDuplicates([], ["alice"], complete: true);

        Assert.Equal(2, queue.SyncDuplicates([Pair()], ["alice"], complete: true));
        Assert.All(Duplicates(queue, "alice"), entry => Assert.Equal(NoticeState.Waiting, entry.State));
    }

    /// <summary>A duplicate is a Navidrome row, not a file Octo placed: nothing may expire it by
    /// looking for a local path, and there is no id to look up.</summary>
    [Fact]
    public void DueForLookup_NeverOffersADuplicate()
    {
        var queue = new NoticeQueue();
        queue.SyncDuplicates([Pair()], ["alice"], complete: true);

        Assert.Empty(queue.DueForLookup(DateTime.UtcNow.AddDays(30), 100));
    }

    [Fact]
    public void MarkKept_OnOneCopy_SettlesThatPersonsGroupOnly()
    {
        var queue = new NoticeQueue();
        queue.SyncDuplicates([Pair()], ["alice", "bob"], complete: true);
        foreach (var user in new[] { "alice", "bob" })
            queue.MarkQueued(Duplicates(queue, user).Select(entry => entry.Key));

        Assert.NotNull(queue.MarkKept("alice", "b"));

        Assert.All(Duplicates(queue, "alice"), entry => Assert.Equal(NoticeState.Dismissed, entry.State));
        Assert.All(Duplicates(queue, "bob"), entry => Assert.Equal(NoticeState.Queued, entry.State));
    }

    /// <summary>A star or the Keep playlist cannot say which playlist it came from.</summary>
    [Fact]
    public void MarkKept_ATrackInReviewAndDuplicates_AnswersBoth()
    {
        var queue = new NoticeQueue();
        queue.AddReview("alice", "/music/teardrop.flac",
            new Song { Artist = "Massive Attack", Title = "Teardrop" }, ReviewFixtures.Unknown);
        var review = NoticeQueue.ReviewKey("alice", "/music/teardrop.flac");
        queue.SetNavidromeId(review, "a");
        queue.MarkQueued([review]);
        queue.SyncDuplicates([Pair()], ["alice"], complete: true);

        var kept = queue.MarkKept("alice", "a");

        Assert.Equal(NoticeKind.Review, kept?.Kind);
        Assert.Equal(NoticeState.Kept, queue.ForUser("alice", NoticeKind.Review).Single().State);
        Assert.All(Duplicates(queue, "alice"), entry => Assert.Equal(NoticeState.Dismissed, entry.State));
    }

    [Fact]
    public void MarkActed_OnOneCopy_ExpiresTheRestOfTheGroupForEveryone()
    {
        var queue = new NoticeQueue();
        queue.SyncDuplicates([Pair()], ["alice", "bob"], complete: true);

        queue.MarkActed("b");

        foreach (var user in new[] { "alice", "bob" })
        {
            var entries = Duplicates(queue, user);
            Assert.Equal(NoticeState.Expired, entries[0].State);
            Assert.Equal(NoticeState.Acted, entries[1].State);
        }
    }

    private static NoticeEntry Entry(string id, NoticeState state, string group, int order = 0) => new()
    {
        Key = $"k-{id}-{state}", Username = "alice", Kind = NoticeKind.Duplicates, NavidromeId = id,
        State = state, GroupKey = group, Order = order,
    };

    /// <summary>A duplicate group is one question: taking either copy out answers it.</summary>
    [Fact]
    public void Plan_TakingOneCopyOut_DismissesTheGroupAndTakesTheRestOff()
    {
        var plan = NoticeReconcile.Plan(
            [Entry("a", NoticeState.Queued, "g"), Entry("b", NoticeState.Queued, "g", 1)],
            new HashSet<string>(["b"], StringComparer.Ordinal), 10);

        Assert.Equal(["k-a-Queued", "k-b-Queued"], plan.Dismiss);
        Assert.Equal(["b"], plan.Remove);
        Assert.Empty(plan.Add);
    }

    /// <summary>A group found again after an older one expired must keep its own tracks.</summary>
    [Fact]
    public void Plan_AStaleEntry_NeverRemovesATrackAnOpenQuestionStillNeeds()
    {
        var plan = NoticeReconcile.Plan(
            [Entry("a", NoticeState.Expired, "old"), Entry("a", NoticeState.Queued, "new"), Entry("c", NoticeState.Queued, "new", 1)],
            new HashSet<string>(["a", "c"], StringComparer.Ordinal), 10);

        Assert.Empty(plan.Remove);
        Assert.Empty(plan.Dismiss);
    }
}

public class DuplicateSettingsTests
{
    [Fact]
    public void Duplicates_AloneTurnsOnNoticesKeepAndTheNoticeOnlyScope()
    {
        var settings = new LibraryActionSettings { DuplicatesEnabled = true };

        Assert.True(settings.NoticesEnabled);
        Assert.Equal([NoticeKind.Duplicates], settings.EnabledNoticeKinds());
        Assert.Equal(LibraryRatingScope.NoticeOnly, settings.EffectiveRatingsScope);
        Assert.True(settings.EffectiveActions().Single(action => action.Action == LibraryAction.Keep).Enabled);
        Assert.Equal("▸ Duplicates", settings.NoticeTitle(NoticeKind.Duplicates));
        Assert.Equal("▸ Duplicates", new LibraryActionSettings { DuplicatesPlaylistName = " " }.NoticeTitle(NoticeKind.Duplicates));
    }

    [Theory]
    [InlineData(0, 1)]
    [InlineData(24, 24)]
    [InlineData(10000, 168)]
    public void EffectiveDuplicatesScanInterval_IsClamped(int hours, int expected)
        => Assert.Equal(expected,
            (int)new LibraryActionSettings { DuplicatesScanHours = hours }.EffectiveDuplicatesScanInterval.TotalHours);

    [Fact]
    public async Task ScanNow_WithDuplicatesOff_SaysWhatToTurnOn()
    {
        using var factory = new AdminWebFactory();
        using var client = factory.CreateClient();
        using var request = new HttpRequestMessage(HttpMethod.Post, "/api/admin/duplicates/scan");
        request.Headers.Add("X-Octo-Admin", "1");

        using var response = await client.SendAsync(request);

        Assert.Equal(HttpStatusCode.BadRequest, response.StatusCode);
        Assert.Contains("Duplicates", await response.Content.ReadAsStringAsync());
    }
}
