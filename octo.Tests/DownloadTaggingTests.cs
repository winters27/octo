using System.Net;
using System.Text.Json;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Logging.Abstractions;
using Microsoft.Extensions.Options;
using Moq;
using Moq.Protected;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services;
using Octo.Services.Audio;
using Octo.Services.Common;
using Octo.Services.Fingerprint;
using Octo.Services.Local;
using Octo.Services.Metadata;
using Octo.Services.Notifications;
using Octo.Services.Subsonic;
using Octo.Services.Tagging;

namespace Octo.Tests;

/// <summary>
/// A download end to end, from the landed file to the tags on disk and the report in the
/// fetched-songs log: the catalog, the music database and the loudness meter are fakes, the
/// fingerprint service's answer is a parsed lookup, and the files are real files in a temp
/// folder.
/// </summary>
public sealed class DownloadTaggingTests : IDisposable
{
    private readonly string _root = Path.Combine(Path.GetTempPath(), "octo-tagging-" + Guid.NewGuid().ToString("N"));

    public DownloadTaggingTests() => Directory.CreateDirectory(_root);

    public void Dispose()
    {
        try { if (Directory.Exists(_root)) Directory.Delete(_root, recursive: true); } catch { /* best effort */ }
    }

    // ---- the fakes ------------------------------------------------------------------------

    private sealed class FakeMeter(Loudness? result, int delayMs = 150) : ILoudnessMeter
    {
        public bool Called { get; private set; }
        public bool FileStillThereWhenDone { get; private set; }

        public async Task<Loudness?> MeasureAsync(string path, int timeoutSeconds, CancellationToken ct = default)
        {
            Called = true;
            await Task.Delay(delayMs, ct);
            FileStillThereWhenDone = File.Exists(path);
            return result;
        }
    }

    /// <summary>One HTTP layer for the catalog and the music database, answering by url substring.</summary>
    private static IHttpClientFactory Http(Dictionary<string, string> routes, Func<string, HttpResponseMessage?>? special = null)
    {
        var handler = new Mock<HttpMessageHandler>();
        handler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync", ItExpr.IsAny<HttpRequestMessage>(), ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync((HttpRequestMessage req, CancellationToken _) =>
            {
                var url = req.RequestUri!.ToString();
                if (special?.Invoke(url) is { } answer) return answer;
                foreach (var (needle, body) in routes)
                    if (url.Contains(needle, StringComparison.OrdinalIgnoreCase))
                        return new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent(body) };
                return new HttpResponseMessage(HttpStatusCode.NotFound);
            });
        var factory = new Mock<IHttpClientFactory>();
        factory.Setup(f => f.CreateClient(It.IsAny<string>())).Returns((string name) =>
            new HttpClient(handler.Object)
            {
                BaseAddress = name == MusicBrainzClient.ClientName ? new Uri("https://musicbrainz.test/ws/2/") : null,
            });
        return factory.Object;
    }

    private sealed class TaggingService : BaseDownloadService
    {
        public Func<Song, string> Landing { get; set; } = _ => throw new InvalidOperationException("no landing set");

        public TaggingService(string root, FolderStructure layout, IServiceProvider provider,
            IMusicMetadataService metadata, ILocalLibraryService library, DownloadHistoryService history)
            : base(
                new ConfigurationBuilder().AddInMemoryCollection(new Dictionary<string, string?> { ["Library:DownloadPath"] = root }).Build(),
                library, metadata,
                TestOptions.Monitor(new SubsonicSettings
                {
                    FolderStructure = layout, AutoDetectDownloadPath = false,
                    DownloadMode = DownloadMode.Track, StorageMode = StorageMode.Permanent,
                }),
                TestOptions.Monitor(new GenreSettings()),
                new NavidromeIdentityService(TestOptions.Monitor(new SubsonicSettings { AutoDetectDownloadPath = false }),
                    Mock.Of<IHttpClientFactory>(), NullLogger<NavidromeIdentityService>.Instance),
                history,
                new NotificationService([], TestOptions.Monitor(new NotificationSettings()), NullLogger<NotificationService>.Instance),
                provider, NullLogger.Instance)
        {
        }

        protected override string ProviderName => "test";
        public override Task<bool> IsAvailableAsync() => Task.FromResult(true);
        protected override string? ExtractExternalIdFromAlbumId(string albumId) => null;

        protected override Task<string> DownloadTrackAsync(string trackId, Song song, bool suppressNotify,
            DownloadSource? sourceOverride, bool upgradeSearch, CancellationToken cancellationToken) => Task.FromResult(Landing(song));

        public Task<string> Download(string id) => DownloadSongAsync("test", id);

        public Task<string> DownloadInWalk(string id, AlbumTagContext context) =>
            DownloadSongInternalAsync("test", id, triggerAlbumDownload: false, CancellationToken.None,
                forcePermanent: true, suppressNotify: true, albumContext: context);
    }

    private sealed class Harness
    {
        public required TaggingService Service { get; init; }
        public required DownloadHistoryService History { get; init; }
        public required FakeMeter Meter { get; init; }
        public required IServiceProvider Provider { get; init; }
    }

    private Harness Build(Dictionary<string, Song> songs, Dictionary<string, string> routes,
        MetadataSettings? metadata = null, SoulseekSettings? soulseek = null, Loudness? loudness = null,
        FolderStructure layout = FolderStructure.Flat, Func<string, HttpResponseMessage?>? special = null,
        AcquisitionTracker? tracker = null)
    {
        var http = Http(routes, special);
        var meter = new FakeMeter(loudness ?? new Loudness(-11.5, 6.3, -0.3));
        var metadataSettings = TestOptions.Monitor(metadata ?? new MetadataSettings());
        var services = new ServiceCollection()
            .AddSingleton<IOptionsMonitor<SoulseekSettings>>(TestOptions.Monitor(soulseek ?? new SoulseekSettings()))
            .AddSingleton<IOptionsMonitor<MetadataSettings>>(metadataSettings)
            .AddSingleton(new DeezerMetadataService(http, metadataSettings, NullLogger<DeezerMetadataService>.Instance))
            .AddSingleton(new MusicBrainzClient(http, NullLogger<MusicBrainzClient>.Instance))
            .AddSingleton<ILoudnessMeter>(meter)
            .AddSingleton(sp => new ReleaseIdentifier(sp, NullLogger<ReleaseIdentifier>.Instance));
        if (tracker is not null) services.AddSingleton(tracker);
        var provider = services.BuildServiceProvider();

        var catalog = new Mock<IMusicMetadataService>();
        catalog.Setup(m => m.GetSongAsync("test", It.IsAny<string>()))
            .ReturnsAsync((string _, string id) => songs.TryGetValue(id, out var song) ? song : null);
        var library = new Mock<ILocalLibraryService>();
        library.Setup(l => l.GetMappingsAsync()).ReturnsAsync([]);
        library.Setup(l => l.GetLocalPathForExternalSongAsync(It.IsAny<string>(), It.IsAny<string>())).ReturnsAsync((string?)null);
        library.Setup(l => l.RegisterDownloadedSongAsync(It.IsAny<Song>(), It.IsAny<string>())).Returns(Task.CompletedTask);
        library.Setup(l => l.TriggerLibraryScanAsync(It.IsAny<bool>())).ReturnsAsync(true);
        var history = new DownloadHistoryService(Path.Combine(_root, "history.json"), NullLogger<DownloadHistoryService>.Instance);

        return new Harness
        {
            Service = new TaggingService(_root, layout, provider, catalog.Object, library.Object, history),
            History = history,
            Meter = meter,
            Provider = provider,
        };
    }

    // ---- the fixtures ---------------------------------------------------------------------

    private const string TeardropLookup = """
    {"status": "ok", "results": [{"id": "acoustid-1", "score": 0.97, "recordings": [{
      "id": "rec-teardrop", "title": "Teardrop", "duration": 330.2, "sources": 40, "isrcs": ["GBAAA9800001"],
      "artists": [{"id": "a-ma", "name": "Massive Attack", "joinphrase": " feat. "}, {"id": "a-ef", "name": "Elizabeth Fraser"}],
      "releasegroups": [
        {"id": "g-collected", "title": "Collected", "type": "Album", "secondarytypes": ["Compilation"],
         "artists": [{"id": "a-ma", "name": "Massive Attack"}],
         "releases": [{"id": "r-col", "date": {"year": 2006, "month": 3, "day": 27}, "country": "GB",
           "mediums": [{"position": 1, "track_count": 14, "tracks": [{"id": "t-col", "position": 4}]}]}]},
        {"id": "g-mezzanine", "title": "Mezzanine", "type": "Album",
         "artists": [{"id": "a-ma", "name": "Massive Attack"}],
         "releases": [
           {"id": "r-mezz-2019", "date": {"year": 2019, "month": 8, "day": 23}, "country": "XE",
            "mediums": [{"position": 1, "track_count": 11, "tracks": [{"id": "t-m19", "position": 3}]}]},
           {"id": "r-mezz", "date": {"year": 1998, "month": 4, "day": 20}, "country": "GB",
            "mediums": [{"position": 1, "track_count": 11, "tracks": [{"id": "t-m", "position": 3}]}]}]}
      ]}]}]}
    """;

    private const string MezzanineDetails = """
    {"id": "r-mezz", "title": "Mezzanine", "status": "Official", "date": "1998-04-20", "country": "GB", "barcode": "724384559922",
     "label-info": [{"catalog-number": "CDV 2851", "label": {"id": "l-virgin", "name": "Virgin"}}],
     "release-group": {"id": "g-mezzanine", "title": "Mezzanine", "primary-type": "Album", "secondary-types": [], "first-release-date": "1998-04-20"},
     "artist-credit": [{"name": "Massive Attack", "artist": {"id": "a-ma", "name": "Massive Attack"}}],
     "media": [{"position": 1, "track-count": 11, "tracks": [
       {"id": "t-m", "position": 3, "number": "3", "title": "Teardrop", "recording": {"id": "rec-teardrop", "title": "Teardrop", "isrcs": ["GBAAA9800001"]}}]}],
     "genres": [{"name": "trip hop", "count": 9}, {"name": "electronic", "count": 3}, {"name": "downtempo", "count": 1}]}
    """;

    private static Dictionary<string, string> MezzanineRoutes(string catalogAlbum = "Mezzanine") => new()
    {
        ["/album/1"] = """{"id":1,"record_type":"album","upc":"724384559922","label":"Virgin","release_date":"1998-04-20","nb_tracks":11,"artist":{"name":"Massive Attack"},"genres":{"data":[{"name":"Electro"}]}}""",
        ["/track/11"] = """{"id":11,"track_position":3,"disk_number":1,"isrc":"GBAAA9800001","gain":-9.8,"contributors":[{"name":"Massive Attack","role":"Main"},{"name":"Elizabeth Fraser","role":"Featured"}]}""",
        ["/search?q="] = """{"data":[{"id":11,"title":"Teardrop","duration":330,"isrc":"GBAAA9800001","album":{"id":1,"title":"CATALOG_ALBUM","cover_xl":"https://cdn.example/mezz.jpg"},"artist":{"name":"Massive Attack"}}]}"""
            .Replace("CATALOG_ALBUM", catalogAlbum),
        ["release/r-mezz?"] = MezzanineDetails,
    };

    private static VerificationResult ConfirmedTeardrop()
    {
        using var doc = JsonDocument.Parse(TeardropLookup);
        var lookup = AcoustIdClient.ParseLookup(doc.RootElement);
        var recording = lookup.Results[0].Recordings[0];
        return new VerificationResult
        {
            Verdict = VerificationVerdict.Confirmed, Score = 0.97, Match = recording, RecordingId = recording.RecordingId,
            MatchedTitle = recording.Title, MatchedArtist = recording.ArtistCredit, MatchedAlbum = recording.AlbumTitle,
            MatchedYear = recording.Year, Lookup = lookup, AcoustId = "acoustid-1",
        };
    }

    private string LandedFlac(string folder, Action<TagLib.File>? tag = null)
    {
        var dir = Path.Combine(_root, folder);
        Directory.CreateDirectory(dir);
        var path = Path.Combine(dir, Guid.NewGuid().ToString("N") + ".flac");
        File.WriteAllBytes(path, AudioFixtures.Flac());
        using var file = TagLib.File.Create(path);
        file.Tag.Title = "Teardrop";
        file.Tag.Performers = ["Massive Attack"];
        tag?.Invoke(file);
        file.Save();
        return path;
    }

    private static string? Vorbis(string path, string field)
    {
        using var file = TagLib.File.Create(path);
        return (file.GetTag(TagLib.TagTypes.Xiph, false) as TagLib.Ogg.XiphComment)?.GetFirstField(field);
    }

    private static string[] VorbisAll(string path, string field)
    {
        using var file = TagLib.File.Create(path);
        return (file.GetTag(TagLib.TagTypes.Xiph, false) as TagLib.Ogg.XiphComment)?.GetField(field) ?? [];
    }

    // ---- A: the FLAC ends with the full tag set -------------------------------------------

    [Fact]
    public async Task LoneStarFlac_EndsWithTheFullReleaseSet_ReplayGain_AndAReport()
    {
        var harness = Build(new() { ["1"] = new Song { Artist = "Massive Attack", Title = "Teardrop" } }, MezzanineRoutes());
        harness.Service.Landing = song =>
        {
            var verdict = ConfirmedTeardrop();
            verdict.ApplyTagsTo(song);
            song.Verification = verdict;
            return LandedFlac("peer share", file => file.Tag.Comment = "a peer's comment");
        };

        var path = await harness.Service.Download("1");

        Assert.Equal("Mezzanine", Vorbis(path, "ALBUM"));
        Assert.Equal("1998", Vorbis(path, "DATE"));
        Assert.Equal("1998-04-20", Vorbis(path, "ORIGINALDATE"));
        Assert.Equal("Virgin", Vorbis(path, "LABEL"));
        Assert.Equal("CDV 2851", Vorbis(path, "CATALOGNUMBER"));
        Assert.Equal("724384559922", Vorbis(path, "BARCODE"));
        Assert.Equal("GBAAA9800001", Vorbis(path, "ISRC"));
        Assert.Equal("album", Vorbis(path, "RELEASETYPE"));
        Assert.Equal("official", Vorbis(path, "RELEASESTATUS"));
        Assert.Equal("GB", Vorbis(path, "RELEASECOUNTRY"));
        Assert.Equal("rec-teardrop", Vorbis(path, "MUSICBRAINZ_TRACKID"));
        Assert.Equal("t-m", Vorbis(path, "MUSICBRAINZ_RELEASETRACKID"));
        Assert.Equal("g-mezzanine", Vorbis(path, "MUSICBRAINZ_RELEASEGROUPID"));
        Assert.Equal(["a-ma", "a-ef"], VorbisAll(path, "MUSICBRAINZ_ARTISTID"));
        Assert.Equal("a-ma", Vorbis(path, "MUSICBRAINZ_ALBUMARTISTID"));
        Assert.Equal("acoustid-1", Vorbis(path, "ACOUSTID_ID"));
        Assert.Equal("-6.50 dB", Vorbis(path, "REPLAYGAIN_TRACK_GAIN"));
        Assert.Equal("0.966051", Vorbis(path, "REPLAYGAIN_TRACK_PEAK"));
        Assert.Equal(3, int.Parse(Vorbis(path, "TRACKNUMBER")!));
        // The code has its own field now; the peer's comment is left exactly as it was.
        Assert.Equal("a peer's comment", Vorbis(path, "COMMENT"));

        var entry = harness.History.GetRecent(1).Single();
        var report = entry.Tagging!;
        Assert.Equal("Strong", report.Confidence);
        Assert.Equal("r-mezz", report.ReleaseId);
        Assert.True(report.DetailsPrefetchHit);
        Assert.Equal("Fingerprint", report.Fields["album"].Source);
        Assert.Equal(-11.5, report.IntegratedLufs);
        Assert.True(report.StageSeconds.ContainsKey("loudness"));
        Assert.True(report.StageSeconds.ContainsKey("total"));
        // The measurement finished before the file moved: nothing reads a file while it moves.
        Assert.True(harness.Meter.Called);
        Assert.True(harness.Meter.FileStillThereWhenDone);
    }

    // ---- an upload's tags are not evidence ----------------------------------------------

    [Fact]
    public async Task StagedUpload_IgnoresItsOwnTagsAsEvidence()
    {
        var routes = new Dictionary<string, string>
        {
            ["/album/2"] = """{"id":2,"record_type":"single","release_date":"2021-05-01","nb_tracks":1,"artist":{"name":"Artist"}}""",
            ["/track/22"] = """{"id":22,"track_position":1,"disk_number":1}""",
            ["/search?q="] = """{"data":[{"id":22,"title":"Song","duration":200,"album":{"id":2,"title":"Song"},"artist":{"name":"Artist"}}]}""",
        };
        var harness = Build(new() { ["1"] = new Song { Artist = "Artist", Title = "Song" } }, routes);
        harness.Service.Landing = _ =>
        {
            var dir = Path.Combine(_root, Octo.Services.Soulseek.SoulseekDownloadService.IncomingFolderName);
            Directory.CreateDirectory(dir);
            var path = Path.Combine(dir, "upload.mp3");
            File.WriteAllBytes(path, AudioFixtures.Mp3());
            using var file = TagLib.File.Create(path);
            file.Tag.Title = "Some Channel - Song";
            file.Tag.Album = "Some Channel";
            file.Tag.Performers = ["Some Channel"];
            file.Save();
            return path;
        };

        var path = await harness.Service.Download("1");

        using var tagged = TagLib.File.Create(path);
        Assert.Equal("Song", tagged.Tag.Album);
        var report = harness.History.GetRecent(1).Single().Tagging!;
        Assert.DoesNotContain(report.Candidates, c => c.Source == "FileTags");
        Assert.Equal("Medium", report.Confidence);
    }

    // ---- C: the request's album is never overwritten ------------------------------------

    [Fact]
    public async Task AlbumWalkTrack_KeepsTheRequestedAlbumAndTrack()
    {
        var harness = Build(new()
        {
            ["1"] = new Song { Artist = "Massive Attack", Title = "Teardrop", Album = "Collected", Track = 4, TotalTracks = 14 },
        }, MezzanineRoutes());
        harness.Service.Landing = song =>
        {
            var verdict = ConfirmedTeardrop();
            verdict.ApplyTagsTo(song);
            song.Verification = verdict;
            return LandedFlac("peer share");
        };

        var path = await harness.Service.Download("1");

        Assert.Equal("Collected", Vorbis(path, "ALBUM"));
        Assert.Equal(4, int.Parse(Vorbis(path, "TRACKNUMBER")!));
        Assert.Equal("rec-teardrop", Vorbis(path, "MUSICBRAINZ_TRACKID"));
        // The chooser confirmed the compilation, which is the release it was asked about.
        Assert.Equal("r-col", harness.History.GetRecent(1).Single().Tagging!.ReleaseId);
    }

    // ---- G: a catalog-only Medium fills blanks only ---------------------------------------

    [Fact]
    public async Task CatalogOnlyMedium_FillsBlanksOnly_NoReleaseFactsWritten()
    {
        var routes = new Dictionary<string, string>
        {
            ["/album/2"] = """{"id":2,"record_type":"single","release_date":"2021-05-01","nb_tracks":1,"label":"A Label","upc":"111111111111","artist":{"name":"Artist"}}""",
            ["/track/22"] = """{"id":22,"track_position":1,"disk_number":1,"isrc":"USAAA2100001"}""",
            ["/search?q="] = """{"data":[{"id":22,"title":"Song","duration":200,"album":{"id":2,"title":"Song"},"artist":{"name":"Artist"}}]}""",
        };
        var harness = Build(new() { ["1"] = new Song { Artist = "Artist", Title = "Song" } }, routes);
        harness.Service.Landing = _ => LandedFlac("peer share", file =>
        {
            file.Tag.Title = "Song";
            file.Tag.Performers = ["Artist"];
            file.Tag.Album = "Peer Album";
        });

        var path = await harness.Service.Download("1");

        var report = harness.History.GetRecent(1).Single().Tagging!;
        Assert.Equal("Medium", report.Confidence);
        Assert.Equal("Catalog", report.Source);
        Assert.Equal("Song", Vorbis(path, "ALBUM"));
        Assert.Equal("2021", Vorbis(path, "DATE"));
        Assert.Equal("USAAA2100001", Vorbis(path, "ISRC"));
        // Only the chooser writes a release's kind and status, and it did not get to.
        Assert.Null(Vorbis(path, "RELEASETYPE"));
        Assert.Null(Vorbis(path, "RELEASESTATUS"));
        Assert.Null(Vorbis(path, "CATALOGNUMBER"));
        Assert.False(report.Fields.ContainsKey("album"));
    }

    // ---- H: the database down and the catalog throttled -----------------------------------

    [Fact]
    public async Task DatabaseDownAndCatalogThrottled_KeepsTheFingerprintsRelease_AndSaysSo()
    {
        var harness = Build(new() { ["1"] = new Song { Artist = "Massive Attack", Title = "Teardrop" } }, new(),
            special: url => url.Contains("musicbrainz.test") ? new HttpResponseMessage(HttpStatusCode.ServiceUnavailable)
                : url.Contains("api.deezer.com") ? new HttpResponseMessage(HttpStatusCode.OK)
                    { Content = new StringContent("""{"error":{"type":"Exception","message":"Quota limit exceeded","code":4}}""") }
                : null);
        harness.Service.Landing = song =>
        {
            var verdict = ConfirmedTeardrop();
            verdict.ApplyTagsTo(song);
            song.Verification = verdict;
            return LandedFlac("peer share");
        };

        var path = await harness.Service.Download("1");

        Assert.Equal("Mezzanine", Vorbis(path, "ALBUM"));
        Assert.Equal("1998", Vorbis(path, "DATE"));
        Assert.Equal("rec-teardrop", Vorbis(path, "MUSICBRAINZ_TRACKID"));
        Assert.Null(Vorbis(path, "LABEL"));
        Assert.Null(Vorbis(path, "CATALOGNUMBER"));
        var report = harness.History.GetRecent(1).Single().Tagging!;
        Assert.Equal("Strong", report.Confidence);
        Assert.Contains(report.Notes, n => n.Contains("catalog did not answer"));
        Assert.Contains(report.Notes, n => n.Contains("release lookup"));
        Assert.DoesNotContain(report.Candidates, c => c.Source == "Catalog");
    }

    // ---- I: a rip from a compilation ------------------------------------------------------

    private const string NowLookup = """
    {"status": "ok", "results": [{"id": "acoustid-1", "score": 0.95, "recordings": [{
      "id": "rec-teardrop", "title": "Teardrop", "duration": 330, "sources": 40,
      "artists": [{"id": "a-ma", "name": "Massive Attack"}],
      "releasegroups": [
        {"id": "g-now42", "title": "Now That's What I Call Music! 42", "type": "Album", "secondarytypes": ["Compilation"],
         "artists": [{"id": "va", "name": "Various Artists"}],
         "releases": [{"id": "r-now", "date": {"year": 1999, "month": 4, "day": 12}, "country": "GB",
           "mediums": [{"position": 1, "track_count": 21, "tracks": [{"id": "t-now", "position": 9}]}]}]},
        {"id": "g-mezzanine", "title": "Mezzanine", "type": "Album",
         "artists": [{"id": "a-ma", "name": "Massive Attack"}],
         "releases": [{"id": "r-mezz", "date": {"year": 1998, "month": 4, "day": 20}, "country": "GB",
           "mediums": [{"position": 1, "track_count": 11, "tracks": [{"id": "t-m", "position": 3}]}]}]}
      ]}]}]}
    """;

    private Harness CompilationRip(bool preferOriginalAlbum, Action<TagLib.Ogg.XiphComment>? peer = null)
    {
        var harness = Build(new() { ["1"] = new Song { Artist = "Massive Attack", Title = "Teardrop" } },
            new() { ["release/r-mezz?"] = MezzanineDetails, ["/search?q="] = """{"data":[]}""" },
            metadata: new MetadataSettings { PreferOriginalAlbum = preferOriginalAlbum });
        harness.Service.Landing = song =>
        {
            using var doc = JsonDocument.Parse(NowLookup);
            var lookup = AcoustIdClient.ParseLookup(doc.RootElement);
            var recording = lookup.Results[0].Recordings[0];
            var verdict = new VerificationResult
            {
                Verdict = VerificationVerdict.Confirmed, Score = 0.95, Match = recording, RecordingId = recording.RecordingId,
                MatchedAlbum = recording.AlbumTitle, Lookup = lookup, AcoustId = "acoustid-1",
            };
            verdict.ApplyTagsTo(song);
            song.Verification = verdict;
            return LandedFlac("peer share", file =>
            {
                file.Tag.Album = "Now That's What I Call Music! 42";
                file.Tag.AlbumArtists = ["Various Artists"];
                TagWriterExtras.SetCompilation(file, true);
                peer?.Invoke((TagLib.Ogg.XiphComment)file.GetTag(TagLib.TagTypes.Xiph, true));
            });
        };
        return harness;
    }

    /// <summary>What a peer who tagged the compilation wrote about it: every one of these names
    /// the compilation, not the studio album the song is filed under.</summary>
    private static void TaggedForTheCompilation(TagLib.Ogg.XiphComment xiph)
    {
        xiph.SetField("MUSICBRAINZ_ALBUMID", "0b1c2d3e-4f50-4617-8899-aabbccddeeff");
        xiph.SetField("MUSICBRAINZ_RELEASETRACKID", "t-now");
        xiph.SetField("MUSICBRAINZ_RELEASEGROUPID", "g-now42");
        xiph.SetField("RELEASEDATE", "1999-04-12");
        xiph.SetField("UPC", "0724352168227");
        xiph.SetField("ORGANIZATION", "EMI");
        xiph.SetField("TRACKNUMBER", "9");
        xiph.SetField("TRACKTOTAL", "21");
        xiph.SetField("DISCNUMBER", "2");
        xiph.SetField("TOTALDISCS", "2");
        xiph.SetField("REPLAYGAIN_ALBUM_GAIN", "-9.40 dB");
    }

    [Fact]
    public async Task CompilationRip_FiledUnderTheStudioAlbum_KeepsNoneOfTheCompilationsFacts()
    {
        var harness = CompilationRip(preferOriginalAlbum: true, TaggedForTheCompilation);

        var path = await harness.Service.Download("1");

        Assert.Equal("Mezzanine", Vorbis(path, "ALBUM"));
        Assert.Equal((3, 11, 1, 1), (int.Parse(Vorbis(path, "TRACKNUMBER")!), int.Parse(Vorbis(path, "TRACKTOTAL")!),
            int.Parse(Vorbis(path, "DISCNUMBER")!), int.Parse(Vorbis(path, "DISCTOTAL")!)));
        Assert.Equal("724384559922", Vorbis(path, "BARCODE"));
        Assert.Equal(["Virgin"], VorbisAll(path, "LABEL"));
        foreach (var field in new[] { "MUSICBRAINZ_ALBUMID", "RELEASEDATE", "UPC", "ORGANIZATION", "TOTALDISCS", "REPLAYGAIN_ALBUM_GAIN", "COMPILATION" })
            Assert.True(Vorbis(path, field) is null, $"{field} should have gone");
        Assert.NotEqual("t-now", Vorbis(path, "MUSICBRAINZ_RELEASETRACKID"));
        Assert.NotEqual("g-now42", Vorbis(path, "MUSICBRAINZ_RELEASEGROUPID"));
        var report = harness.History.GetRecent(1).Single().Tagging!;
        Assert.Contains(report.Notes, n => n.Contains("Now That's What I Call Music! 42") && n.Contains("UPC"));
    }

    [Fact]
    public async Task CompilationKept_LosesOnlyTheGroupingValues()
    {
        var harness = CompilationRip(preferOriginalAlbum: false, TaggedForTheCompilation);

        var path = await harness.Service.Download("1");

        Assert.Equal("Now That's What I Call Music! 42", Vorbis(path, "ALBUM"));
        Assert.Null(Vorbis(path, "MUSICBRAINZ_ALBUMID"));
        Assert.Null(Vorbis(path, "RELEASEDATE"));
        // Still the compilation, so what the peer wrote about it stays.
        Assert.Equal("EMI", Vorbis(path, "ORGANIZATION"));
        Assert.Equal("1", Vorbis(path, "COMPILATION"));
        Assert.Equal("-9.40 dB", Vorbis(path, "REPLAYGAIN_ALBUM_GAIN"));
    }

    [Fact]
    public async Task SameAlbumFromAPeer_EndsWithOneLabelOneBarcodeAndNoGroupingValues()
    {
        var harness = Build(new() { ["1"] = new Song { Artist = "Massive Attack", Title = "Teardrop" } }, MezzanineRoutes());
        harness.Service.Landing = song =>
        {
            var verdict = ConfirmedTeardrop();
            verdict.ApplyTagsTo(song);
            song.Verification = verdict;
            return LandedFlac("peer share", file =>
            {
                file.Tag.Album = "Mezzanine";
                var xiph = (TagLib.Ogg.XiphComment)file.GetTag(TagLib.TagTypes.Xiph, true);
                xiph.SetField("MUSICBRAINZ_ALBUMID", "0b1c2d3e-4f50-4617-8899-aabbccddeeff");
                xiph.SetField("RELEASEDATE", "2019-08-23");
                xiph.SetField("ORGANIZATION", "Circa");
                xiph.SetField("UPC", "724384559922");
                xiph.SetField("MUSICBRAINZ_ALBUMTYPE", "album");
            });
        };

        var path = await harness.Service.Download("1");

        using var file = TagLib.File.Create(path);
        var view = Octo.Services.Library.KeptIdentityTags.NavidromeView(file);
        Assert.Equal(["Virgin"], view["label"]);
        Assert.False(view.ContainsKey("organization"));
        Assert.Equal(["724384559922"], view["barcode"]);
        Assert.False(view.ContainsKey("upc"));
        Assert.False(view.ContainsKey("musicbrainz_albumtype"));
        var identity = Octo.Services.Library.KeptIdentityTags.Read(path)!;
        Assert.Null(identity.AlbumId);
        Assert.Null(identity.ReleaseDate);
    }

    [Fact]
    public async Task JoiningAnAlbumFolder_CarriesTheAlbumsOwnGroupingValues()
    {
        const string theirs = "99999999-8888-4777-8666-555555555555";
        var harness = Build(new() { ["1"] = new Song { Artist = "Massive Attack", Title = "Teardrop" } }, MezzanineRoutes(catalogAlbum: "Collected"),
            layout: FolderStructure.Organized, metadata: new MetadataSettings { TagRehearsal = true });
        var folder = Path.GetDirectoryName(PathHelper.BuildLayoutPath(FolderStructure.Organized, _root,
            "Massive Attack", "Collected", "Teardrop", 4, ".flac"))!;
        Directory.CreateDirectory(folder);
        var sibling = Path.Combine(folder, "01 - Unfinished Sympathy.flac");
        File.WriteAllBytes(sibling, AudioFixtures.Flac());
        using (var file = TagLib.File.Create(sibling))
        {
            file.Tag.Title = "Unfinished Sympathy";
            file.Tag.Album = "Collected";
            file.Tag.AlbumArtists = ["Massive Attack"];
            var xiph = (TagLib.Ogg.XiphComment)file.GetTag(TagLib.TagTypes.Xiph, true);
            xiph.SetField("MUSICBRAINZ_ALBUMID", theirs);
            xiph.SetField("RELEASEDATE", "2006-03-27");
            file.Save();
        }
        harness.Service.Landing = song =>
        {
            var verdict = ConfirmedTeardrop();
            verdict.ApplyTagsTo(song);
            song.Verification = verdict;
            // A peer's own release id for another pressing of the same compilation.
            return LandedFlac("peer share", file =>
                ((TagLib.Ogg.XiphComment)file.GetTag(TagLib.TagTypes.Xiph, true)).SetField("MUSICBRAINZ_ALBUMID", "0b1c2d3e-4f50-4617-8899-aabbccddeeff"));
        };

        // A rehearsal files it under the catalog's album, the one the folder holds.
        var path = await harness.Service.Download("1");

        Assert.Equal(folder, Path.GetDirectoryName(path));
        var identity = Octo.Services.Library.KeptIdentityTags.Read(path)!;
        Assert.Equal((theirs, "2006-03-27"), (identity.AlbumId, identity.ReleaseDate));
    }

    [Fact]
    public async Task TheExplicitCopyThatLanded_IsTaggedExplicit_AndTheCleanOneClean()
    {
        var routes = MoneyTreesRoutes();
        async Task<(string Path, TagReport Report)> Landing(string isrc)
        {
            var harness = Build(new() { ["1"] = new Song { Artist = "Kendrick Lamar", Title = "Money Trees" } }, routes);
            harness.Service.Landing = _ => LandedFlac("peer share", file =>
            {
                file.Tag.Title = "Money Trees";
                file.Tag.Performers = ["Kendrick Lamar"];
                TagWriterExtras.SetText(file, TagFields.Isrc, isrc);
            });
            var path = await harness.Service.Download("1");
            return (path, harness.History.GetRecent(1).Single().Tagging!);
        }

        var explicitCopy = await Landing("USUM71210782");
        Assert.Equal("1", Vorbis(explicitCopy.Path, "ITUNESADVISORY"));
        Assert.Equal(new FieldDecision("explicit", "Catalog"), explicitCopy.Report.Fields["advisory"]);

        // The catalog's first hit is the explicit one; the file's own code says it is the edit.
        File.Delete(explicitCopy.Path);
        var cleanCopy = await Landing("USUM71210787");
        Assert.Equal("2", Vorbis(cleanCopy.Path, "ITUNESADVISORY"));
    }

    [Fact]
    public async Task TheDownloadsLogSaysWhatCameOffThePeersTagsAndWhetherTheSongIsExplicit()
    {
        var tracker = new AcquisitionTracker(NullLogger<AcquisitionTracker>.Instance);
        var harness = Build(new() { ["1"] = new Song { Artist = "Kendrick Lamar", Title = "Money Trees", ExternalProvider = "test", ExternalId = "1" } },
            MoneyTreesRoutes(), tracker: tracker);
        harness.Service.Landing = _ => LandedFlac("peer share", file =>
        {
            file.Tag.Title = "Money Trees";
            file.Tag.Performers = ["Kendrick Lamar"];
            TagWriterExtras.SetText(file, TagFields.Isrc, "USUM71210782");
            ((TagLib.Ogg.XiphComment)file.GetTag(TagLib.TagTypes.Xiph, true)).SetField("MUSICBRAINZ_ALBUMID", "0b1c2d3e-4f50-4617-8899-aabbccddeeff");
        });
        tracker.Begin("test", "1", null, "alice");

        await harness.Service.Download("1");

        var log = tracker.Detail("test:1", "alice")!.Events!;
        Assert.Contains(log, e => e.Kind == AcquisitionEventKinds.Tags && e.Text == "Marked explicit"
            && e.Detail == "The catalog's match for this exact version says so");
        Assert.Contains(log, e => e.Kind == AcquisitionEventKinds.Tags && e.Text == "Took off the peer's own album tags"
            && e.Detail!.Contains("MUSICBRAINZ_ALBUMID", StringComparison.OrdinalIgnoreCase));
    }

    [Fact]
    public async Task TryItOnASong_ShowsTheAdvisoryAndWhatADownloadWouldTakeOff_AndWritesNothing()
    {
        var harness = Build(new(), MoneyTreesRoutes());
        var path = LandedFlac("library", file =>
        {
            file.Tag.Title = "Money Trees";
            file.Tag.Performers = ["Kendrick Lamar"];
            file.Tag.Album = "Kendrick Lamar Mixtape Vol. 2";
            var xiph = (TagLib.Ogg.XiphComment)file.GetTag(TagLib.TagTypes.Xiph, true);
            xiph.SetField("ISRC", "USUM71210787");
            xiph.SetField("MUSICBRAINZ_ALBUMID", "0b1c2d3e-4f50-4617-8899-aabbccddeeff");
            xiph.SetField("CATALOGNUMBER", "MIX-2");
        });
        var before = File.ReadAllBytes(path);
        var preview = new TagPreview(harness.Provider, harness.Provider.GetRequiredService<ReleaseIdentifier>(), NullLogger<TagPreview>.Instance);

        var report = await preview.PreviewAsync(path, null, null, null, CancellationToken.None);

        Assert.Equal(new FieldDecision("clean edit", "Catalog"), report.Fields["advisory"]);
        Assert.Contains(report.Notes, n => n.StartsWith("a download would take off") && n.Contains("MUSICBRAINZ_ALBUMID") && n.Contains("CATALOGNUMBER"));
        Assert.Equal(before, File.ReadAllBytes(path));
    }

    /// <summary>"Money Trees" twice in the catalog: the explicit copy first, then the clean edit,
    /// each with its own code.</summary>
    private static Dictionary<string, string> MoneyTreesRoutes() => new()
    {
        ["/album/7"] = """{"id":7,"record_type":"album","release_date":"2012-10-22","nb_tracks":12,"artist":{"name":"Kendrick Lamar"}}""",
        ["/album/8"] = """{"id":8,"record_type":"album","release_date":"2012-10-22","nb_tracks":12,"artist":{"name":"Kendrick Lamar"}}""",
        ["/track/71"] = """{"id":71,"track_position":5,"disk_number":1,"isrc":"USUM71210782","explicit_lyrics":true,"explicit_content_lyrics":1}""",
        ["/track/81"] = """{"id":81,"track_position":5,"disk_number":1,"isrc":"USUM71210787","explicit_lyrics":false,"explicit_content_lyrics":3}""",
        ["/search?q="] = """
            {"data":[
              {"id":71,"title":"Money Trees","duration":386,"explicit_lyrics":true,"album":{"id":7,"title":"good kid, m.A.A.d city"},"artist":{"name":"Kendrick Lamar"}},
              {"id":81,"title":"Money Trees","duration":386,"explicit_lyrics":false,"album":{"id":8,"title":"good kid, m.A.A.d city"},"artist":{"name":"Kendrick Lamar"}}]}
            """,
    };

    [Fact]
    public async Task CompilationRip_PreferOriginalAlbumOn_IsFiledUnderTheStudioAlbum()
    {
        var harness = CompilationRip(preferOriginalAlbum: true);

        var path = await harness.Service.Download("1");

        Assert.Equal("Mezzanine", Vorbis(path, "ALBUM"));
        Assert.Equal("CDV 2851", Vorbis(path, "CATALOGNUMBER"));
        Assert.Null(Vorbis(path, "COMPILATION"));
        Assert.Equal("Medium", harness.History.GetRecent(1).Single().Tagging!.Confidence);
    }

    [Fact]
    public async Task CompilationRip_PreferOriginalAlbumOff_KeepsTheCompilation()
    {
        var harness = CompilationRip(preferOriginalAlbum: false);

        var path = await harness.Service.Download("1");

        Assert.Equal("Now That's What I Call Music! 42", Vorbis(path, "ALBUM"));
        Assert.Equal("1", Vorbis(path, "COMPILATION"));
        Assert.Equal("r-now", harness.History.GetRecent(1).Single().Tagging!.ReleaseId);
    }

    // ---- rehearsal ------------------------------------------------------------------------

    [Fact]
    public async Task Rehearsal_WritesTodaysSetPlusTheAdditiveFacts_AndReportsThePlan()
    {
        var harness = Build(new() { ["1"] = new Song { Artist = "Massive Attack", Title = "Teardrop" } },
            MezzanineRoutes(catalogAlbum: "Collected"), metadata: new MetadataSettings { TagRehearsal = true });
        harness.Service.Landing = song =>
        {
            var verdict = ConfirmedTeardrop();
            verdict.ApplyTagsTo(song);
            song.Verification = verdict;
            return LandedFlac("peer share");
        };

        var path = await harness.Service.Download("1");

        // What the old rules wrote: the catalog's own album for a song that arrived without one.
        Assert.Equal("Collected", Vorbis(path, "ALBUM"));
        Assert.Null(Vorbis(path, "CATALOGNUMBER"));
        Assert.Null(Vorbis(path, "MUSICBRAINZ_RELEASETRACKID"));
        Assert.Null(Vorbis(path, "RELEASESTATUS"));
        // The additive facts that do not depend on the match.
        Assert.Equal("-6.50 dB", Vorbis(path, "REPLAYGAIN_TRACK_GAIN"));
        Assert.Equal("GBAAA9800001", Vorbis(path, "ISRC"));
        Assert.Equal("acoustid-1", Vorbis(path, "ACOUSTID_ID"));
        var report = harness.History.GetRecent(1).Single().Tagging!;
        Assert.True(report.Rehearsed);
        Assert.Equal("Strong", report.Confidence);
        Assert.Equal("Mezzanine", report.ReleaseTitle);
    }

    // ---- loudness that could not be measured ----------------------------------------------

    [Fact]
    public async Task MeasurementThatGivesNothing_LeavesNoReplayGain_AndTheReportSaysSo()
    {
        var harness = Build(new() { ["1"] = new Song { Artist = "Massive Attack", Title = "Teardrop" } }, MezzanineRoutes(),
            loudness: new Loudness(double.NegativeInfinity, 0, double.NegativeInfinity));
        harness.Service.Landing = song =>
        {
            var verdict = ConfirmedTeardrop();
            verdict.ApplyTagsTo(song);
            song.Verification = verdict;
            return LandedFlac("peer share");
        };

        var path = await harness.Service.Download("1");

        Assert.Null(Vorbis(path, "REPLAYGAIN_TRACK_GAIN"));
        Assert.Contains(harness.History.GetRecent(1).Single().Tagging!.Notes, n => n.Contains("loudness could not be measured"));
    }

    // ---- two tracks of one walk share the album-level fields --------------------------------

    [Fact]
    public async Task TwoTracksOfOneWalk_ShareTheReleaseFacts()
    {
        var harness = Build(new()
        {
            ["1"] = new Song { Artist = "Massive Attack", Title = "Teardrop", Album = "Mezzanine", Track = 3 },
            ["2"] = new Song { Artist = "Massive Attack", Title = "Angel", Album = "Mezzanine", Track = 1 },
        }, MezzanineRoutes(), layout: FolderStructure.Organized);
        var context = new AlbumTagContext("1", "Mezzanine", "Massive Attack");
        var second = false;
        harness.Service.Landing = song =>
        {
            var verdict = ConfirmedTeardrop();
            if (second)
            {
                // The second track's fingerprint names only the 2019 pressing of the same album.
                using var doc = JsonDocument.Parse(TeardropLookup.Replace("\"r-mezz\"", "\"r-gone\"").Replace("1998", "2019"));
                var lookup = AcoustIdClient.ParseLookup(doc.RootElement);
                verdict = verdict with { Lookup = lookup, Match = lookup.Results[0].Recordings[0] with { RecordingId = "rec-angel" }, RecordingId = "rec-angel" };
            }
            verdict.ApplyTagsTo(song);
            song.Verification = verdict;
            var path = LandedFlac("peer share", file => file.Tag.Title = song.Title);
            second = true;
            return path;
        };

        var first = await harness.Service.DownloadInWalk("1", context);
        var next = await harness.Service.DownloadInWalk("2", context);

        Assert.True(context.Captured);
        Assert.Equal("CDV 2851", Vorbis(first, "CATALOGNUMBER"));
        Assert.Equal("CDV 2851", Vorbis(next, "CATALOGNUMBER"));
        Assert.Equal(Vorbis(first, "DATE"), Vorbis(next, "DATE"));
        Assert.Equal(Vorbis(first, "LABEL"), Vorbis(next, "LABEL"));
        Assert.Equal(2, context.Loudness.Count);
        Assert.Equal(Path.GetDirectoryName(first), Path.GetDirectoryName(next));
    }

    [Fact]
    public void WriteAlbumGain_WritesTheAlbumValuesIntoEveryMeasuredFile()
    {
        var harness = Build(new(), new());
        var context = new AlbumTagContext("1", "Mezzanine", "Massive Attack");
        var a = LandedFlac("album");
        var b = LandedFlac("album");
        context.Loudness[a] = new Loudness(-10, 0, -1);
        context.Loudness[b] = new Loudness(-14, 0, -0.5);

        harness.Service.WriteAlbumGain(context, "Mezzanine");

        var expected = ReplayGainTags.ForAlbum([context.Loudness[a], context.Loudness[b]])!;
        Assert.Equal(expected.GainText, Vorbis(a, "REPLAYGAIN_ALBUM_GAIN"));
        Assert.Equal(expected.GainText, Vorbis(b, "REPLAYGAIN_ALBUM_GAIN"));
        Assert.Equal(expected.PeakText, Vorbis(b, "REPLAYGAIN_ALBUM_PEAK"));
    }

    // ---- #69: a download with nothing on disk fails ---------------------------------------

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public async Task ALandedPathWithNoAudioFailsAndIsNeverRecorded(bool emptyFile)
    {
        var harness = Build(new() { ["1"] = new Song { Artist = "Massive Attack", Title = "Teardrop" } }, new());
        var path = Path.Combine(_root, "slskd", "incomplete", "peer", "Teardrop.flac");
        if (emptyFile)
        {
            Directory.CreateDirectory(Path.GetDirectoryName(path)!);
            File.WriteAllBytes(path, []);
        }
        harness.Service.Landing = _ => path;

        await Assert.ThrowsAsync<FileNotFoundException>(() => harness.Service.Download("1"));

        Assert.Empty(harness.History.GetRecent());
        Assert.False(harness.Meter.Called);
    }
}
