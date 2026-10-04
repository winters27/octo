using System.Collections.Concurrent;
using System.Net;
using System.Text;
using System.Text.Json;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Logging.Abstractions;
using Microsoft.Extensions.Options;
using Moq;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services;
using Octo.Services.Common;
using Octo.Services.Fingerprint;
using Octo.Services.Local;
using Octo.Services.Notifications;
using Octo.Services.Soulseek;
using Octo.Services.Subsonic;
using Octo.Services.YouTube;

namespace Octo.Tests;

/// <summary>
/// Two reports from 2026-10-04. "Songs with (Radio edit) only download radio edit songs": the
/// title's version tag was read as words the file name had to carry, so "Too Close (Radio Edit)"
/// took only files that spelled out "radio edit", and the same rule made "Take Care (feat.
/// Rihanna)" refuse every file not naming Rihanna. And "Take Care downloaded MP3, Find better
/// quality found FLAC": the star's search stopped at 500 files after one second, while Better
/// quality read 2,000. Every file name below is shaped like a real Soulseek share.
/// </summary>
public sealed class DownloadMatchingTests : IDisposable
{
    // ---- The name rule reads the title, not its tags ------------------------------------------

    [Theory]
    [InlineData(@"Music\Drake\Take Care (Deluxe) (2011)\04 - Take Care.flac")]
    [InlineData(@"Music\Drake\Take Care\04 - Drake ft. Rihanna - Take Care.flac")]
    [InlineData(@"VA - Now That's What I Call Music! 81\CD1\17. Drake Feat. Rihanna - Take Care.flac")]
    public void AGuestCreditIsNotWordsTheFileMustCarry(string file)
    {
        Assert.True(SoulseekDownloadService.FilenamePlausiblyMatchesTitle(file, "Take Care (feat. Rihanna)"));
        Assert.True(SoulseekDownloadService.FilenamePlausiblyMatchesTitle(file, "Take Care (feat. Rihanna)", requirePhrase: true));
    }

    [Theory]
    [InlineData("Too Close (Radio Edit)", "Too Close")]
    [InlineData("Too Close - Radio Edit", "Too Close")]
    [InlineData("Teardrop - Remastered 2011", "Teardrop")]
    [InlineData("Take Care (feat. Rihanna)", "Take Care")]
    [InlineData("Blue (Da Ba Dee)", "Blue (Da Ba Dee)")]
    [InlineData("Song (Pt. 2)", "Song (Pt. 2)")]
    [InlineData("(Exchange)", "(Exchange)")]
    public void TheNameTitleDropsOnlyGuestsAndVersionTags(string title, string expected) =>
        Assert.Equal(expected, SoulseekDownloadService.NameTitle(title));

    [Fact]
    public void ARadioEditRequestHearsAPlainlyNamedSingle() =>
        Assert.True(SoulseekDownloadService.FilenamePlausiblyMatchesTitle(
            @"music\Next\Too Close (CDS) (1998)\01 - Too Close.flac", "Too Close (Radio Edit)"));

    // ---- Wanting the song never takes another version of it ------------------------------------

    [Theory]
    [InlineData(@"music\Ministry Of Sound - One R&B [UK] [3CD]\CD3\09 - Next - Too Close [Radio Edit].flac")]
    [InlineData(@"music\Next\Singles\Next - Too Close - Radio Edit.flac")]
    [InlineData(@"music\Next\Singles\09 Next-Too Close-radio edit.flac")]
    [InlineData(@"music\Next\Singles\09_next_-_too_close_(radio_edit).flac")]
    [InlineData(@"music\Next\Rated Next\03 - Too Close (Clean).flac")]
    [InlineData(@"tiktok\Next - Too Close (Sped Up).flac")]
    [InlineData(@"Club\Next - Too Close (Extended Mix).flac")]
    [InlineData(@"music\Next\Singles\Too Close (Single Edit).flac")]
    [InlineData(@"music\Next\Singles\Too Close (Kenny Dope Remix).flac")]
    public void TheSongItselfNeverTakesAnEditOrARemix(string file) =>
        Assert.True(SoulseekDownloadService.AddsVersion(file, "Too Close"));

    [Theory]
    [InlineData(@"music\Next\Rated Next (1997)\03 - Too Close.flac")]
    [InlineData(@"music\Next\Rated Next (1997)\03 - Too Close (Album Version).flac")]
    [InlineData(@"music\Next\Rated Next (1997)\03 - Too Close (Explicit).flac")]
    [InlineData(@"music\Next\Rated Next (Remastered 2017)\03 - Too Close (Remastered).flac")]
    public void TheSameRecordingUnderAnotherLabelIsFine(string file)
    {
        Assert.False(SoulseekDownloadService.AddsVersion(file, "Too Close"));
        Assert.False(SoulseekDownloadService.FromVersionFolder(file, "Too Close", "Rated Next", "Next"));
    }

    [Theory]
    [InlineData(@"music\Next\Too Close (Radio Edit) - Single\01 - Too Close.flac")]
    [InlineData(@"Share\VA - Club Hits 1998 (Extended Mixes)\CD2\04 - Next - Too Close.flac")]
    [InlineData(@"Share\Next - Too Close (The Remixes)\01 - Too Close.flac")]
    [InlineData(@"Share\Next - Rated Next [Clean]\03 - Too Close.flac")]
    [InlineData(@"Share\Next - Rated Next (Instrumentals)\03 - Too Close.flac")]
    [InlineData(@"Share\Ministry of Sound - The Annual 1999 (DJ Mix)\CD1\05 - Next - Too Close.flac")]
    public void AFolderThatNamesAVersionMakesItsPlainFilesThatVersion(string file) =>
        Assert.True(SoulseekDownloadService.FromVersionFolder(file, "Too Close", "Rated Next", "Next"));

    [Fact]
    public void ARemixAlbumsFolderIsReadLikeALiveOne() =>
        // The real folder that pulled four Mezzanine tracks off a Mad Professor remix album.
        Assert.True(SoulseekDownloadService.FromVersionFolder(
            @"Music\Massive Attack\Massive Attack V Mad Professor Part II (Mezzanine Remix Tapes '98)\03 - Angel.flac",
            "Angel", "Mezzanine", "Massive Attack"));

    [Theory]
    [InlineData(@"Music\Drake\Take Care (Deluxe Edition) [2011] [FLAC 16-44]\04 - Take Care.flac")]
    [InlineData(@"Music\Drake\Take Care (Extended Edition)\04 - Take Care.flac")]
    [InlineData(@"Music\Clean Bandit\New Eyes (2014)\04 - Take Care.flac")]
    [InlineData(@"Music\Reprise Records\Take Care\04 - Take Care.flac")]
    [InlineData(@"Music\Drake\Take Care (Japanese Version)\04 - Take Care.flac")]
    public void AnEditionOrANameIsNotAVersion(string file) =>
        Assert.False(SoulseekDownloadService.FromVersionFolder(file, "Take Care", "Take Care", "Drake"));

    [Fact]
    public void ABandCalledLiveIsNotALiveTake()
    {
        const string file = @"Music\Live\Throwing Copper (1994)\05 - Lightning Crashes.flac";
        Assert.False(SoulseekDownloadService.FromVersionFolder(file, "Lightning Crashes", "Throwing Copper", "Live"));
        // Their concert record still reads as one by its other words; AcoustID's live check backs it up.
        Assert.True(SoulseekDownloadService.FromLiveFolder(@"Music\Live\Live - Paradiso (In Concert 1995)\05 - Lightning Crashes.flac",
            "Lightning Crashes", "Throwing Copper", "Live"));
    }

    [Fact]
    public void ARequestOnTheVersionsAlbumTakesItsFolder() =>
        Assert.False(SoulseekDownloadService.FromVersionFolder(@"Share\Next - Too Close (The Remixes)\01 - Too Close.flac",
            "Too Close", "Too Close (The Remixes)", "Next"));

    // ---- Wanting a version gets that version ----------------------------------------------------

    private static SoulseekFileHit Hit(string user, string file, int seconds, string ext = "flac", int queue = 0) => new()
    {
        Username = user, Filename = file, Size = 30_000_000, Length = seconds, Extension = ext, QueueLength = queue,
        UploadSpeed = 1_000_000,
    };

    private static List<SoulseekFileHit> Rank(IEnumerable<SoulseekFileHit> hits, string title, int? seconds,
        string? album = null, string? artist = null) =>
        SoulseekDownloadService.Rank(hits, new SoulseekDownloadService.CandidateWant(title, seconds, Album: album, Artist: artist),
            "flac", 0, _ => true);

    [Fact]
    public void ARadioEditRequestTakesTheNamedEditFirst_AndAPlainCopyOfItsLength()
    {
        var ranked = Rank(
        [
            // The plain one first in every other way: no queue.
            Hit("plain", @"music\Next\Too Close (CDS)\01 - Too Close.flac", 226),
            Hit("named", @"music\VA - One R&B\CD3\09 - Next - Too Close [Radio Edit].flac", 227, queue: 5),
            Hit("album", @"music\Next\Rated Next (1997)\03 - Too Close.flac", 261),
            Hit("clean", @"music\Next\Singles\Too Close (Radio Edit) (Clean).flac", 226),
        ], "Too Close (Radio Edit)", 226, artist: "Next");

        Assert.Equal(["named", "plain"], ranked.Select(h => h.Username));
    }

    [Fact]
    public void TheSongItselfNeverRanksAnEdit()
    {
        var ranked = Rank(
        [
            Hit("edit", @"music\VA - One R&B\CD3\09 - Next - Too Close [Radio Edit].flac", 262),
            Hit("single", @"music\Next\Too Close (Radio Edit) - Single\01 - Too Close.flac", 262),
            Hit("album", @"music\Next\Rated Next (1997)\03 - Too Close.flac", 261, queue: 9),
        ], "Too Close", 261, "Rated Next", "Next");

        Assert.Equal("album", Assert.Single(ranked).Username);
    }

    [Theory]
    [InlineData("Too Close (Kenny Dope Remix)", @"music\Next\Rated Next (1997)\03 - Too Close.flac")]
    [InlineData("Heat Waves (Sped Up)", @"music\Glass Animals\Dreamland\08 - Heat Waves.flac")]
    [InlineData("Creep (Acoustic)", @"music\Radiohead\Pablo Honey\02 - Creep.flac")]
    [InlineData("Love Story (Taylor's Version)", @"music\Taylor Swift\Fearless (2008)\03 - Love Story.flac")]
    [InlineData("Smile in Your Sleep (Live)", @"Music\Silverstein\Discovering the Waterfront (2005)\05 - Smile in Your Sleep.flac")]
    public void AVersionThatIsNotACutIsNeverAnsweredByThePlainSong(string title, string file) =>
        Assert.True(VersionVariant.LacksRequested(file, title));

    [Theory]
    [InlineData("Too Close (Kenny Dope Remix)", @"Share\Next - Too Close (The Remixes)\02 - Too Close (Kenny Dope Remix).flac")]
    [InlineData("Heat Waves (Sped Up)", @"tiktok\Glass Animals - Heat Waves (Sped Up).flac")]
    [InlineData("Love Story (Taylor's Version)", @"music\Taylor Swift\Fearless (Taylor's Version)\03 - Love Story (Taylor's Version).flac")]
    [InlineData("Smile in Your Sleep (Live)", @"Music\Silverstein\Decade (live at the El Mocambo) (2010)\17 - Smile in Your Sleep.flac")]
    [InlineData("Too Close (Radio Edit)", @"music\Next\Too Close (CDS)\01 - Too Close.flac")]
    public void TheVersionAskedForPassesWhereverItIsNamed(string title, string file)
    {
        Assert.False(VersionVariant.LacksRequested(file, title));
        Assert.False(SoulseekDownloadService.AddsVersion(file, title));
    }

    [Fact]
    public void AnAlbumFolderNeverGivesAVersionTrackThePlainFile()
    {
        var track = new AlbumTrack("t1", "Too Close (Kenny Dope Remix)", 330, 2);
        var files = new[] { Hit("p", @"Share\Next - Rated Next\02 - Too Close.flac", 330) };
        Assert.Empty(AlbumFolderPicker.Match([track], files));
    }

    // ---- AcoustID says which recording arrived ---------------------------------------------------

    private static AcoustIdLookup Lookup(string title) =>
        new(true, null, [new AcoustIdResult(0.98, [new AcoustIdRecording("rec", title, ["Next"], "Rated Next", 1997)])]);

    [Fact]
    public void TheSongItselfRefusesARecordingMusicBrainzCallsTheRadioEdit() =>
        Assert.Equal(VerificationVerdict.Mismatch, DownloadVerificationService.Decide(Lookup("Too Close (radio edit)"),
            "Next", "Too Close", 0.85, tagsAuthoritative: true).Verdict);

    [Fact]
    public void ARadioEditRequestKeepsTheRecordingMusicBrainzListsPlainly() =>
        Assert.Equal(VerificationVerdict.Confirmed, DownloadVerificationService.Decide(Lookup("Too Close"),
            "Next", "Too Close (Radio Edit)", 0.85, tagsAuthoritative: true).Verdict);

    // ---- One search width for a star and Better quality -----------------------------------------

    [Fact]
    public void AStarSearchesAsWideAsBetterQuality()
    {
        var s = new SoulseekSettings();
        var star = SearchProfile.Interactive(s);
        var upgrade = SearchProfile.Upgrade(s);
        Assert.Equal((upgrade.ResponseLimit, upgrade.FileLimit), (star.ResponseLimit, star.FileLimit));
        Assert.Equal((1_000, 8_000), (star.Wider().ResponseLimit, star.Wider().FileLimit));
        Assert.Equal(star.CeilingSeconds, star.Wider().CeilingSeconds);
    }

    [Theory]
    [InlineData("Completed, FileLimitReached", true)]
    [InlineData("Completed, ResponseLimitReached", true)]
    [InlineData("Completed, TimedOut", false)]
    [InlineData("Completed, Cancelled", false)]
    [InlineData(null, false)]
    public void ASearchStoppedAtItsLimitIsTold(string? state, bool limited) =>
        Assert.Equal(limited, SoulseekClient.HitLimit(state));

    [Fact]
    public void TheTakeCareSearchFindsThePlainFlacAmongItsMp3s()
    {
        // 'Drake Take Care' answers as the canary saw them: a wall of MP3s, the FLACs named plainly.
        var hits = Enumerable.Range(1, 40)
            .Select(i => Hit($"mp3peer{i}", $@"Music\Drake - Take Care\04 - Take Care (feat. Rihanna).mp3", 277, "mp3"))
            .Append(Hit("flacpeer", @"Music\Drake\Take Care (Deluxe) (2011)\04 - Take Care.flac", 277, queue: 3))
            .ToList();

        var ranked = Rank(hits, "Take Care (feat. Rihanna)", 277, "Take Care", "Drake");

        Assert.Equal("flacpeer", Assert.Single(ranked).Username);
    }

    private readonly string _root = Path.Combine(Path.GetTempPath(), "octo-dl-matching-" + Guid.NewGuid().ToString("N"));

    public DownloadMatchingTests() => Directory.CreateDirectory(_root);

    public void Dispose()
    {
        try { Directory.Delete(_root, recursive: true); } catch { /* best effort */ }
    }

    [Fact]
    public async Task ASearchFilledWithMp3sIsAskedAgainWider_AndTheFlacItThenHearsIsTaken()
    {
        const string flac = @"Music\Drake\Take Care (Deluxe) (2011)\04 - Take Care.flac";
        var (service, slskd, song) = Build((text, fileLimit) =>
            text != "Drake Take Care" ? ([], "Completed, TimedOut")
            : fileLimit <= SearchProfile.SongFileLimit ? (Mp3Wall(), "Completed, FileLimitReached")
            : (Mp3Wall().Append(Response("flacpeer", [(flac, 277)])).ToArray(), "Completed, FileLimitReached"));
        slskd.Length(flac, 277);

        var path = await service.ExecuteAcquisitionAsync("soulseek", song.ExternalId!, false, true,
            DownloadSource.Soulseek, CancellationToken.None).WaitAsync(TimeSpan.FromSeconds(60));

        Assert.Equal([("Drake Take Care", 2_000), ("Drake Take Care", 8_000)], slskd.Searches);
        Assert.Equal("flacpeer", Assert.Single(slskd.Batches).User);
        Assert.EndsWith(".flac", path);
    }

    [Fact]
    public async Task ASearchThatRanOutOfAnswersIsNotAskedAgain()
    {
        var (service, slskd, song) = Build((text, _) => (Mp3Wall(), "Completed, TimedOut"));

        await Assert.ThrowsAnyAsync<Exception>(() => service.ExecuteAcquisitionAsync("soulseek", song.ExternalId!, false, true,
            DownloadSource.Soulseek, CancellationToken.None).WaitAsync(TimeSpan.FromSeconds(60)));

        Assert.Equal(["Drake Take Care", "Take Care"], slskd.Searches.Select(s => s.Text));
        Assert.All(slskd.Searches, s => Assert.Equal(2_000, s.FileLimit));
    }

    private static object[] Mp3Wall() => Enumerable.Range(1, 30).Select(i => (object)new
    {
        username = $"mp3peer{i}", uploadSpeed = 5_000_000, queueLength = 0, hasFreeUploadSlot = true,
        files = new[] { new { filename = $@"Music\Drake - Take Care\04 - Take Care (feat. Rihanna).mp3", size = 11_000_000L, length = 277, extension = "mp3", bitRate = 320 } },
    }).ToArray();

    private static object Response(string user, IEnumerable<(string File, int Seconds)> files) => new
    {
        username = user, uploadSpeed = 1_000_000, queueLength = 2, hasFreeUploadSlot = false,
        files = files.Select(f => new { filename = f.File, size = (long)Flac(f.Seconds).Length, length = f.Seconds, extension = "flac" }).ToArray(),
    };

    /// <summary>A FLAC whose STREAMINFO says it lasts this long, so the length check passes.</summary>
    private static byte[] Flac(int seconds)
    {
        using var stream = new MemoryStream();
        stream.Write("fLaC"u8);
        stream.Write([0x80, 0x00, 0x00, 0x22]);
        stream.Write([0x10, 0x00, 0x10, 0x00]);
        stream.Write([0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        const ulong sampleRate = 44100, channelsMinusOne = 1, bitsMinusOne = 15;
        var totalSamples = 44100UL * (ulong)seconds;
        var packed = (sampleRate << 44) | (channelsMinusOne << 41) | (bitsMinusOne << 36) | totalSamples;
        for (var shift = 56; shift >= 0; shift -= 8) stream.WriteByte((byte)(packed >> shift));
        stream.Write(new byte[16]);
        return stream.ToArray();
    }

    /// <summary>
    /// slskd as far as one star needs: logged in, each search answered by its words and its file
    /// limit with the state slskd ended it in, and a queued file written straight into its folder.
    /// </summary>
    private sealed class FakeSlskd(string root, Func<string, int, (object[] Responses, string State)> answer) : HttpMessageHandler
    {
        public readonly ConcurrentQueue<(string Text, int FileLimit)> Searches = new();
        public readonly ConcurrentQueue<(string User, string[] Files)> Batches = new();
        private readonly ConcurrentDictionary<string, (string Text, int FileLimit)> _search = new();
        private readonly ConcurrentDictionary<string, List<object>> _transfers = new();
        private readonly ConcurrentDictionary<string, int> _lengths = new();
        private int _ids;

        public void Length(string remote, int seconds) => _lengths[remote] = seconds;

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {
            var path = request.RequestUri!.AbsolutePath;
            var body = request.Content is null ? "" : await request.Content.ReadAsStringAsync(ct);
            if (path == "/api/v0/session") return Json("""{"token":"jwt","expires":4102444800}""");
            if (path == "/api/v0/options")
                return Json(JsonSerializer.Serialize(new { directories = new { downloads = root, incomplete = "/app/incomplete" } }));
            if (path == "/api/v0/searches" && request.Method == HttpMethod.Post)
            {
                using var doc = JsonDocument.Parse(body);
                var search = (doc.RootElement.GetProperty("searchText").GetString()!, doc.RootElement.GetProperty("fileLimit").GetInt32());
                _search[doc.RootElement.GetProperty("id").GetString()!] = search;
                Searches.Enqueue(search);
                return Json("{}");
            }
            if (path.StartsWith("/api/v0/searches/") && request.Method == HttpMethod.Get)
            {
                var search = _search.GetValueOrDefault(path.Split('/')[4]);
                var (responses, state) = answer(search.Text ?? "", search.FileLimit);
                return path.EndsWith("/responses")
                    ? Json(JsonSerializer.Serialize(responses))
                    : Json(JsonSerializer.Serialize(new { state, endedAt = "2026-10-04T12:00:00Z", responseCount = responses.Length }));
            }
            if (path.StartsWith("/api/v0/users/") && path.EndsWith("/directory")) return new HttpResponseMessage(HttpStatusCode.NotFound);
            if (path == "/api/v0/transfers/downloads/batches")
            {
                using var doc = JsonDocument.Parse(body);
                var user = doc.RootElement.GetProperty("username").GetString()!;
                var destination = doc.RootElement.GetProperty("options").GetProperty("destination").GetString()!;
                var files = doc.RootElement.GetProperty("files").EnumerateArray()
                    .Select(f => f.GetProperty("filename").GetString()!).ToArray();
                Batches.Enqueue((user, files));
                var queued = new List<object>();
                foreach (var file in files)
                {
                    var local = Path.Combine(root, destination, file.Replace('\\', '/').Split('/')[^1]);
                    Directory.CreateDirectory(Path.GetDirectoryName(local)!);
                    var bytes = Flac(_lengths.GetValueOrDefault(file, 200));
                    File.WriteAllBytes(local, bytes);
                    var transfer = new
                    {
                        id = $"t-{Interlocked.Increment(ref _ids)}", filename = file, state = "Completed, Succeeded",
                        size = (long)bytes.Length, bytesTransferred = (long)bytes.Length, percentComplete = 100.0,
                    };
                    _transfers.AddOrUpdate(user, _ => [transfer], (_, list) => { lock (list) list.Add(transfer); return list; });
                    queued.Add(transfer);
                }
                return Json(JsonSerializer.Serialize(new { batch = new { username = user, transfers = queued }, failures = Array.Empty<object>() }),
                    HttpStatusCode.Created);
            }
            if (path.StartsWith("/api/v0/transfers/downloads/") && request.Method == HttpMethod.Get)
            {
                var user = Uri.UnescapeDataString(path.Split('/')[5]);
                var files = _transfers.TryGetValue(user, out var list) ? list.ToArray() : [];
                return Json(JsonSerializer.Serialize(new { username = user, directories = new[] { new { directory = "x", files } } }));
            }
            return Json("{}");
        }

        private static HttpResponseMessage Json(string body, HttpStatusCode code = HttpStatusCode.OK) =>
            new(code) { Content = new StringContent(body, Encoding.UTF8, "application/json") };
    }

    /// <summary>One song, "Drake - Take Care (feat. Rihanna)" from the album Take Care, ready to star.</summary>
    private (SoulseekDownloadService Service, FakeSlskd Slskd, Song Song) Build(
        Func<string, int, (object[] Responses, string State)> answer)
    {
        var registry = new ExternalIdRegistry();
        var id = registry.Register(new SoulseekRouting
        {
            Kind = RoutingKind.Song, Artist = "Drake", Title = "Take Care (feat. Rihanna)", Album = "Take Care", Duration = 277,
        });
        var song = new Song
        {
            Id = $"ext-soulseek-{id}", ExternalProvider = "soulseek", ExternalId = id, Title = "Take Care (feat. Rihanna)",
            Artist = "Drake", Album = "Take Care", Duration = 277,
        };

        var slskd = new FakeSlskd(_root, answer);
        var factory = new Mock<IHttpClientFactory>();
        factory.Setup(f => f.CreateClient(It.IsAny<string>())).Returns(() => new HttpClient(slskd, disposeHandler: false));
        var soulseekSettings = new SoulseekSettings
        {
            BaseUrl = "http://slskd.test", Username = "u", Password = "p", MinFileSizeBytes = 0,
            AlbumFolders = false, ParallelDownloads = 1, DetectTranscodes = false, VerifyDownloads = false,
            DownloadTimeoutSeconds = 30,
        };
        var client = new SoulseekClient(factory.Object, Options.Create(soulseekSettings), NullLogger<SoulseekClient>.Instance)
        {
            SearchPollInterval = TimeSpan.FromMilliseconds(5),
            PollInterval = TimeSpan.FromMilliseconds(5),
            MinSearchSpacing = TimeSpan.Zero,
        };

        var metadata = new Mock<IMusicMetadataService>();
        metadata.Setup(m => m.GetSongAsync("soulseek", id)).ReturnsAsync(song);

        var monitor = TestOptions.Monitor(soulseekSettings);
        var services = new ServiceCollection()
            .AddSingleton<IOptionsMonitor<SoulseekSettings>>(monitor)
            .AddSingleton(new DownloadConcurrency(monitor))
            .BuildServiceProvider();
        var config = new ConfigurationBuilder()
            .AddInMemoryCollection(new Dictionary<string, string?> { ["Library:DownloadPath"] = _root }).Build();
        var subsonic = TestOptions.Monitor(new SubsonicSettings { AutoDetectDownloadPath = false, DownloadSource = DownloadSource.Soulseek });
        var service = new SoulseekDownloadService(config, Mock.Of<ILocalLibraryService>(), metadata.Object, subsonic,
            TestOptions.Monitor(new GenreSettings()), Options.Create(soulseekSettings), client,
            new YouTubeResolver(factory.Object, config, NullLogger<YouTubeResolver>.Instance), registry, factory.Object,
            new NavidromeIdentityService(subsonic, factory.Object, NullLogger<NavidromeIdentityService>.Instance),
            new DownloadHistoryService(Path.Combine(_root, "history.json"), NullLogger<DownloadHistoryService>.Instance),
            new NotificationService([], TestOptions.Monitor(new NotificationSettings()), NullLogger<NotificationService>.Instance),
            new RejectedPeerRegistry(),
            new DownloadVerificationService(new AudioFingerprinter(NullLogger<AudioFingerprinter>.Instance),
                new AcoustIdClient(factory.Object, NullLogger<AcoustIdClient>.Instance), monitor,
                NullLogger<DownloadVerificationService>.Instance),
            services, NullLogger<SoulseekDownloadService>.Instance);
        return (service, slskd, song);
    }
}
