using System.Text.Json;
using Microsoft.AspNetCore.Http;
using Microsoft.AspNetCore.Mvc;
using Microsoft.Extensions.Logging;
using Microsoft.Extensions.Options;
using Moq;
using Octo.Models.Domain;
using Octo.Models.Radio;
using Octo.Models.Settings;
using Octo.Services.LastFm;
using Octo.Services.Soulseek;
using Octo.Services.Subsonic;
using Octo.Services;
using Octo.Services.CoverArt;
using Microsoft.Extensions.DependencyInjection;
using SixLabors.ImageSharp;
using SixLabors.ImageSharp.PixelFormats;

namespace Octo.Tests;

public class LastFmRadioCoreTests
{
    [Fact]
    public void RadioStationCovers_KeepLogoAcrossConcurrentFirstRequests()
    {
        var service = new CoverArtService(new Mock<ILogger<CoverArtService>>().Object);
        var covers = Enumerable.Range(0, 16).AsParallel().WithDegreeOfParallelism(8)
            .Select(index => service.GetRadioStationCover($"Station {index}"))
            .ToList();

        Assert.All(covers, bytes =>
        {
            using var image = Image.Load<Rgb24>(bytes);
            var logoPixels = 0;
            for (var y = 78; y < 378; y++)
            for (var x = 150; x < 450; x++)
            {
                var pixel = image[x, y];
                if (pixel.R > 40 || pixel.B > 40) logoPixels++;
            }
            Assert.True(logoPixels > 1_000, $"Expected Octo logo; found {logoPixels} colored pixels");
        });
    }

    [Theory]
    [InlineData("Beyoncé feat. Jay-Z", "Beyoncé")]
    [InlineData("Run the Jewels ft Killer Mike", "Run the Jewels")]
    [InlineData("AC/DC", "AC/DC")]
    public void SeedNormalizer_RemovesFeatureDecorationsWithoutDamagingArtist(string input, string expected) =>
        Assert.Equal(expected, LastFmRadioSeedNormalizer.Artist(input));

    [Theory]
    [InlineData("Song (feat. Guest)", "Song")]
    [InlineData("Song [featuring Guest]", "Song")]
    [InlineData("Song (with Guest)", "Song")]
    public void SeedNormalizer_CleansCommonTitleDecorations(string input, string expected) =>
        Assert.Equal(expected, LastFmRadioSeedNormalizer.Title(input));

    [Fact]
    public async Task RepeatedValueReader_PreservesQueryAndFormBatches()
    {
        var context = new DefaultHttpContext();
        context.Request.QueryString = new QueryString("?id=q1&id=q2");
        var bytes = System.Text.Encoding.UTF8.GetBytes("id=f1&id=f2");
        context.Request.Body = new MemoryStream(bytes);
        context.Request.ContentType = "application/x-www-form-urlencoded";
        context.Request.ContentLength = bytes.Length;
        var values = await new SubsonicRequestParser().ExtractParameterValuesAsync(context.Request, "id");
        Assert.Equal(["q1", "q2", "f1", "f2"], values);
    }

    [Fact]
    public void DiscoverySettings_NormalizeClampAndKeepStableIds()
    {
        var settings = new LastFmSettings
        {
            HistoryRetentionDays = 1, RadioTrackCount = 500, DiscoveryPercent = -4,
            DiscoveryStations = [new() { Id = " Keep-ME! ", Name = " Electronic ",
                Tags = [" IDM ", "idm", "Electronic", "Ambient", "Techno", "House"] }]
        };
        var station = Assert.Single(settings.EffectiveDiscoveryStations());
        Assert.Equal("keepme", station.Id);
        Assert.Equal(["idm", "electronic", "ambient", "techno", "house"], station.Tags);
        Assert.Equal(7, settings.EffectiveHistoryRetentionDays);
        Assert.Equal(100, settings.EffectiveRadioTrackCount);
        Assert.Equal(0, settings.EffectiveDiscoveryPercent);
    }

    [Theory]
    [InlineData(1, 96)]
    [InlineData(120, 128)]
    [InlineData(192, 192)]
    [InlineData(220, 256)]
    [InlineData(999, 320)]
    public void StreamBitrate_UsesSupportedMp3Qualities(int configured, int effective)
    {
        var settings = new LastFmSettings { RadioStreamBitrateKbps = configured };
        Assert.Equal(effective, settings.EffectiveRadioStreamBitrateKbps);
    }

    [Fact]
    public async Task IcyMetadataStream_FramesExistingTrackMetadataAtFixedAudioIntervals()
    {
        await using var output = new MemoryStream();
        var stream = new IcyMetadataStream(output, interval: 4);
        stream.SetTrack(new LastFmRadioTrack { Artist = "Artist One", Title = "Song One" });
        await stream.WriteAsync("ABCDEFGH"u8.ToArray());
        stream.SetTrack(new LastFmRadioTrack { Artist = "Artist Two", Title = "Song Two" });
        await stream.WriteAsync("IJKL"u8.ToArray());

        var bytes = output.ToArray();
        var offset = 0;
        Assert.Equal("ABCD", ReadAudio(bytes, ref offset));
        Assert.Equal("StreamTitle='Artist One - Song One';", ReadMetadata(bytes, ref offset));
        Assert.Equal("EFGH", ReadAudio(bytes, ref offset));
        Assert.Equal("StreamTitle='Artist One - Song One';", ReadMetadata(bytes, ref offset));
        Assert.Equal("IJKL", ReadAudio(bytes, ref offset));
        Assert.Equal("StreamTitle='Artist Two - Song Two';", ReadMetadata(bytes, ref offset));
        Assert.Equal(bytes.Length, offset);
    }

    [Fact]
    public void StreamSessions_AreOpaqueScopedExpiringAndBounded()
    {
        var store = new LastFmRadioStreamSessionStore();
        var now = new DateTime(2026, 8, 26, 1, 0, 0, DateTimeKind.Utc);
        var token = store.Issue("alice", "station-one", new Dictionary<string, string>
        {
            ["u"] = "alice", ["t"] = "secret-token", ["s"] = "salt",
            ["id"] = "must-not-be-copied", ["f"] = "json",
        }, now);
        Assert.DoesNotContain("alice", token);
        Assert.DoesNotContain("secret", token);
        var session = Assert.IsType<LastFmRadioStreamSession>(store.Get(token, now.AddHours(1)));
        Assert.Equal("station-one", session.StationId);
        Assert.Equal("secret-token", session.Authentication["t"]);
        Assert.DoesNotContain("id", session.Authentication.Keys);
        var pool = Enumerable.Range(0, LastFmRadioStreamService.ReadyPoolSize)
            .Select(index => new PreparedRadioTrack($"/tmp/ready-{index}.mp3",
                new LastFmRadioTrack { Artist = "Artist", Title = $"Title {index}" }, index,
                $"key-{index}"))
            .ToList();
        Assert.True(store.AttachReadyPool(token, pool, now.AddHours(1)));
        Assert.Equal(pool, store.Get(token, now.AddHours(1))!.ReadyPool);
        store.ConsumeReadyTrack(token, "key-0");
        Assert.Equal(["key-1", "key-2"], store.Get(token, now.AddHours(1))!.ReadyPool!
            .Select(item => item.CacheKey));
        var replacement = new PreparedRadioTrack("/tmp/ready-3.mp3",
            new LastFmRadioTrack { Artist = "Artist", Title = "Title 3" }, 3, "key-3");
        store.AppendReadyTrack(token, replacement, LastFmRadioStreamService.ReadyPoolSize);
        Assert.Equal(["key-1", "key-2", "key-3"], store.Get(token, now.AddHours(1))!
            .ReadyPool!.Select(item => item.CacheKey));
        Assert.Null(store.Get(token, now.AddHours(13)));

        for (var index = 0; index < LastFmRadioStreamSessionStore.MaximumSessions + 20; index++)
            store.Issue("alice", "station-" + index, new Dictionary<string, string>(),
                now.AddSeconds(index));
        Assert.Equal(LastFmRadioStreamSessionStore.MaximumSessions, store.Count);
    }

    [Fact]
    public async Task FfmpegTranscoder_ProducesConcatenableHeaderlessMp3Segments()
    {
        var transcoder = new FfmpegLastFmRadioAudioTranscoder();
        await using var output = new MemoryStream();
        await using var firstInput = WavSilence();
        await transcoder.TranscodeToMp3Async(firstInput, output, 192, CancellationToken.None);
        var boundary = checked((int)output.Length);
        await using var secondInput = WavSilence();
        await transcoder.TranscodeToMp3Async(secondInput, output, 192, CancellationToken.None);
        var bytes = output.ToArray();
        Assert.True(boundary > 100);
        Assert.True(bytes.Length > boundary + 100);
        Assert.False(bytes.AsSpan(0, 3).SequenceEqual("ID3"u8));
        Assert.False(bytes.AsSpan(boundary, 3).SequenceEqual("ID3"u8));
        Assert.Equal(0xff, bytes[0]);
        Assert.Equal(0xe0, bytes[1] & 0xe0);
        Assert.Equal(0xff, bytes[boundary]);
        Assert.Equal(0xe0, bytes[boundary + 1] & 0xe0);
    }

    [Fact]
    public void PlaylistSerializer_HasJsonXmlSymmetryAndReadOnlyMetadata()
    {
        var builder = new SubsonicResponseBuilder(new ExternalIdRegistry(),
            Options.Create(new SubsonicSettings()));
        var station = Station("alice");
        var song = new Song { Id = "local-1", Artist = "Artist", Title = "Title", Album = "Album",
            Duration = 200, IsLocal = true };
        var json = Assert.IsType<JsonResult>(builder.CreateRadioPlaylistResponse("json", station, [song]));
        using var document = JsonDocument.Parse(JsonSerializer.Serialize(json.Value));
        var playlist = document.RootElement.GetProperty("subsonic-response").GetProperty("playlist");
        Assert.True(playlist.GetProperty("readonly").GetBoolean());
        Assert.Equal("local-1", playlist.GetProperty("entry")[0].GetProperty("id").GetString());
        var xml = Assert.IsType<ContentResult>(builder.CreateRadioPlaylistResponse("xml", station, [song]));
        var element = System.Xml.Linq.XDocument.Parse(xml.Content!).Root!.Elements().Single();
        Assert.Equal("true", element.Attribute("readonly")!.Value);
        Assert.Equal("local-1", element.Elements().Single().Attribute("id")!.Value);
    }

    private static LastFmRadioStation Station(string owner) => new()
    {
        Id = LastFmRadioStateStore.StationId(owner, "mix"), Key = "mix", Name = "Your Mix",
        Owner = owner, Personalized = true, CreatedUtc = DateTime.UtcNow,
        ChangedUtc = DateTime.UtcNow, ValidUntilUtc = DateTime.UtcNow.AddHours(12),
        Tracks = [new() { Artist = "Artist", Title = "Title", Duration = 200 }]
    };

    private static string ReadAudio(byte[] bytes, ref int offset)
    {
        var value = System.Text.Encoding.ASCII.GetString(bytes, offset, 4);
        offset += 4;
        return value;
    }

    private static string ReadMetadata(byte[] bytes, ref int offset)
    {
        var length = bytes[offset++] * 16;
        var value = System.Text.Encoding.UTF8.GetString(bytes, offset, length).TrimEnd('\0');
        offset += length;
        return value;
    }

    private static MemoryStream WavSilence()
    {
        const int sampleRate = 44100;
        const int samples = sampleRate / 4;
        const short channels = 1;
        const short bitsPerSample = 16;
        var dataLength = samples * channels * (bitsPerSample / 8);
        var stream = new MemoryStream(44 + dataLength);
        using (var writer = new BinaryWriter(stream, System.Text.Encoding.ASCII, leaveOpen: true))
        {
            writer.Write("RIFF"u8); writer.Write(36 + dataLength); writer.Write("WAVE"u8);
            writer.Write("fmt "u8); writer.Write(16); writer.Write((short)1); writer.Write(channels);
            writer.Write(sampleRate); writer.Write(sampleRate * channels * bitsPerSample / 8);
            writer.Write((short)(channels * bitsPerSample / 8)); writer.Write(bitsPerSample);
            writer.Write("data"u8); writer.Write(dataLength); writer.Write(new byte[dataLength]);
        }
        stream.Position = 0;
        return stream;
    }
}

public class LastFmRadioTrackResolverTests
{
    [Fact]
    public async Task Resolver_PrefersAuthenticatedLocalMatchBeforeExternalPlaceholder()
    {
        var handler = new DelegateHandler(request =>
        {
            Assert.Contains("u=alice", request.RequestUri!.Query);
            return "{\"subsonic-response\":{\"status\":\"ok\",\"searchResult3\":{\"song\":[{\"id\":\"local-1\",\"artist\":\"The Artist\",\"title\":\"The Song\",\"album\":\"Album\",\"duration\":200}]}}}";
        });
        var metadata = new Mock<IMusicMetadataService>(MockBehavior.Strict);
        var resolver = Resolver(handler, metadata.Object);
        var song = await resolver.ResolveAsync("The Artist", "The Song", 200,
            new Dictionary<string, string> { ["u"] = "alice", ["t"] = "token", ["s"] = "salt" });
        Assert.True(song!.IsLocal);
        Assert.Equal("local-1", song.Id);
        metadata.VerifyNoOtherCalls();
    }

    [Fact]
    public async Task Resolver_UsesExternalPlaceholderOnlyAfterLocalMiss()
    {
        var handler = new DelegateHandler(_ =>
            "{\"subsonic-response\":{\"status\":\"ok\",\"searchResult3\":{\"song\":[]}}}");
        var expected = new Song { Id = "external-1", Artist = "A", Title = "T", IsLocal = false };
        var metadata = new Mock<IMusicMetadataService>();
        metadata.Setup(service => service.SearchSongsByArtistTitleAsync("A", "T", 1, 180))
            .ReturnsAsync([expected]);
        var song = await Resolver(handler, metadata.Object).ResolveAsync("A", "T", 180,
            new Dictionary<string, string> { ["u"] = "alice" });
        Assert.Same(expected, song);
    }

    private static LastFmRadioTrackResolver Resolver(HttpMessageHandler handler,
        IMusicMetadataService metadata)
    {
        var factory = new Mock<IHttpClientFactory>();
        factory.Setup(item => item.CreateClient(It.IsAny<string>())).Returns(new HttpClient(handler));
        var proxy = new SubsonicProxyService(factory.Object,
            TestOptions.Monitor(new SubsonicSettings { Url = "http://navidrome.test" }),
            new HttpContextAccessor { HttpContext = new DefaultHttpContext() });
        return new LastFmRadioTrackResolver(proxy, metadata, new ExternalIdRegistry(),
            new Mock<ILogger<LastFmRadioTrackResolver>>().Object);
    }

    private sealed class DelegateHandler(Func<HttpRequestMessage, string> response) : HttpMessageHandler
    {
        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request,
            CancellationToken cancellationToken) => Task.FromResult(new HttpResponseMessage(System.Net.HttpStatusCode.OK)
            { Content = new StringContent(response(request)) });
    }
}

public class LastFmRadioStateStoreTests
{
    [Fact]
    public void MissingAndCorruptFilesRecoverAndPersistAtomically()
    {
        using var fixture = new StateFixture("not json");
        Assert.Empty(fixture.Store.KnownUsers());
        Assert.True(fixture.Store.RecordPlay("alice", Play("A", "One")));
        Assert.True(File.Exists(fixture.Path));
        Assert.False(File.Exists(fixture.Path + ".tmp"));
        Assert.Equal(LastFmRadioStateStore.CurrentVersion,
            JsonDocument.Parse(File.ReadAllText(fixture.Path)).RootElement.GetProperty("Version").GetInt32());
    }

    [Fact]
    public void PlaysAreDeduplicatedAndIsolatedPerUserEvenWhenDisabled()
    {
        using var fixture = new StateFixture(settings: new LastFmSettings { EnableRadio = false });
        var play = Play("Artist", "Title");
        Assert.True(fixture.Store.RecordPlay("alice", play));
        Assert.False(fixture.Store.RecordPlay("alice", Play("Artist", "Title")));
        Assert.True(fixture.Store.RecordPlay("bob", Play("Artist", "Title")));
        Assert.Single(fixture.Store.GetUser("alice").Plays);
        Assert.Single(fixture.Store.GetUser("bob").Plays);
    }

    [Fact]
    public async Task ConcurrentWritesDoNotLoseDistinctPlays()
    {
        using var fixture = new StateFixture();
        await Task.WhenAll(Enumerable.Range(0, 30).Select(index => Task.Run(() =>
            fixture.Store.RecordPlay("alice", Play("Artist", "Title " + index)))));
        Assert.Equal(30, fixture.Store.GetUser("alice").Plays.Count);
    }

    [Fact]
    public void LoadPrunesOldAndOverBoundHistory()
    {
        var plays = Enumerable.Range(0, 2100).Select(index => Play("A", "T" + index,
            DateTime.UtcNow.AddMinutes(-index))).ToList();
        var document = new LastFmRadioStateDocument { Users = new()
        {
            ["alice"] = new LastFmRadioUserState { Username = "alice", Plays = plays }
        }};
        using var fixture = new StateFixture(JsonSerializer.Serialize(document));
        Assert.Equal(2000, fixture.Store.GetUser("alice").Plays.Count);
    }

    [Fact]
    public void UnsupportedVersionStartsCleanAndRetentionDropsExpiredPlays()
    {
        using var unsupported = new StateFixture("{\"Version\":99,\"Users\":{\"alice\":{\"Username\":\"alice\"}}}");
        Assert.Empty(unsupported.Store.KnownUsers());

        var document = new LastFmRadioStateDocument { Users = new()
        {
            ["alice"] = new LastFmRadioUserState { Username = "alice", Plays =
                [Play("A", "old", DateTime.UtcNow.AddDays(-30)), Play("A", "new")] }
        }};
        using var retained = new StateFixture(JsonSerializer.Serialize(document),
            new LastFmSettings { HistoryRetentionDays = 7 });
        Assert.Equal("new", Assert.Single(retained.Store.GetUser("alice").Plays).Title);
    }

    [Fact]
    public void ExternalRoutesAreRehydratedAfterRestart()
    {
        using var fixture = new StateFixture();
        var station = new LastFmRadioStation { Id = LastFmRadioStateStore.StationId("alice", "mix"),
            Name = "Mix", Owner = "alice", Tracks = [new() { Artist = "A", Title = "T" }] };
        fixture.Store.ReplaceStations("alice", [station]);
        var registry = new ExternalIdRegistry();
        var restarted = new LastFmRadioStateStore(fixture.Path, TestOptions.Monitor(new LastFmSettings()),
            registry, new Mock<ILogger<LastFmRadioStateStore>>().Object);
        var track = Assert.Single(Assert.Single(restarted.GetUser("alice").Stations).Tracks);
        Assert.NotNull(track.ResolvedId);
        Assert.NotNull(registry.Lookup(track.ResolvedId!));
    }

    [Fact]
    public void InstallingIdenticalSnapshotPreservesCreatedAndChangedMetadata()
    {
        using var fixture = new StateFixture();
        var station = new LastFmRadioStation { Id = LastFmRadioStateStore.StationId("alice", "mix"),
            Name = "Mix", Owner = "alice", CreatedUtc = DateTime.UtcNow.AddDays(-2),
            ChangedUtc = DateTime.UtcNow.AddDays(-1), Tracks = [new() { Artist = "A", Title = "T" }] };
        fixture.Store.ReplaceStations("alice", [station]);
        station.CreatedUtc = DateTime.UtcNow; station.ChangedUtc = DateTime.UtcNow;
        fixture.Store.ReplaceStations("alice", [station]);
        var installed = Assert.Single(fixture.Store.GetUser("alice").Stations);
        Assert.Equal(station.CreatedUtc.AddDays(-2).Date, installed.CreatedUtc.Date);
        Assert.True(installed.ChangedUtc < DateTime.UtcNow.AddHours(-23));
    }

    [Fact]
    public void RejectTrack_RemovesItFromEveryStationAndPersistsCooldown()
    {
        using var fixture = new StateFixture();
        var bad = new LastFmRadioTrack { Artist = "Bad Artist", Title = "Bad Song" };
        fixture.Store.ReplaceStations("alice",
        [
            new LastFmRadioStation { Id = "one", Tracks = [bad, new() { Artist = "A", Title = "T" }] },
            new LastFmRadioStation { Id = "two", Tracks = [bad, new() { Artist = "B", Title = "U" }] },
        ]);

        Assert.Equal(2, fixture.Store.RejectTrack("alice", bad));
        var user = fixture.Store.GetUser("alice");
        Assert.All(user.Stations, station => Assert.DoesNotContain(station.Tracks,
            track => track.Title == "Bad Song"));
        var unavailable = Assert.Single(user.UnavailableTracks);
        Assert.Equal(LastFmRadioSeedNormalizer.TrackKey("Bad Artist", "Bad Song"), unavailable.Key);
        Assert.True(unavailable.RetryAfterUtc > unavailable.FailedAtUtc);
    }

    private static LastFmRadioPlay Play(string artist, string title, DateTime? at = null) =>
        new() { Artist = artist, Title = title, PlayedAtUtc = at ?? DateTime.UtcNow };

    private sealed class StateFixture : IDisposable
    {
        private readonly string _dir = System.IO.Path.Combine(System.IO.Path.GetTempPath(), "octo-radio-" + Guid.NewGuid());
        public string Path => System.IO.Path.Combine(_dir, "state.json");
        public LastFmRadioStateStore Store { get; }
        public StateFixture(string? json = null, LastFmSettings? settings = null)
        {
            Directory.CreateDirectory(_dir);
            if (json is not null) File.WriteAllText(Path, json);
            Store = new LastFmRadioStateStore(Path, TestOptions.Monitor(settings ?? new LastFmSettings()),
                new ExternalIdRegistry(), new Mock<ILogger<LastFmRadioStateStore>>().Object);
        }
        public void Dispose() { try { Directory.Delete(_dir, true); } catch { } }
    }
}

public class LastFmRadioRefreshQueueTests
{
    [Fact]
    public async Task Queue_CollapsesIdenticalJobsButKeepsIndependentPinnedRefreshes()
    {
        var queue = new LastFmRadioRefreshQueue();
        Assert.True(queue.Enqueue("alice"));
        Assert.False(queue.Enqueue("ALICE"));
        Assert.True(queue.Enqueue("alice", "rock"));
        Assert.True(queue.Enqueue("alice", "jazz"));
        Assert.Null((await queue.DequeueAsync(default)).StationDefinitionId);
        Assert.Equal("rock", (await queue.DequeueAsync(default)).StationDefinitionId);
        Assert.Equal("jazz", (await queue.DequeueAsync(default)).StationDefinitionId);
    }

    [Fact]
    public void Policy_CoversThresholdNewPlayStalenessAndDeterministicStartupJitter()
    {
        var settings = new LastFmSettings { MinimumPlays = 10, RefreshIntervalHours = 12 };
        var now = DateTime.UtcNow;
        var user = new LastFmRadioUserState
        {
            Plays = Enumerable.Range(0, 9).Select(index => new LastFmRadioPlay
                { Artist = "A", Title = "T" + index, LearnedSignal = true }).ToList(),
            Stations = [new LastFmRadioStation()], LastRefreshSuccessUtc = now,
            NewPlaysSinceRefresh = 4
        };
        Assert.False(LastFmRadioRefreshPolicy.ShouldRefreshAfterPlay(user, settings, now));
        user.NewPlaysSinceRefresh = 5;
        Assert.True(LastFmRadioRefreshPolicy.ShouldRefreshAfterPlay(user, settings, now));
        user.NewPlaysSinceRefresh = 0;
        user.Plays.Add(new LastFmRadioPlay { Artist = "A", Title = "threshold", LearnedSignal = true });
        Assert.True(LastFmRadioRefreshPolicy.ShouldRefreshAfterPlay(user, settings, now));
        user.Plays.RemoveAt(user.Plays.Count - 1);
        user.LastRefreshSuccessUtc = now.AddHours(-13);
        Assert.True(LastFmRadioRefreshPolicy.IsStale(user, settings, now));
        Assert.True(LastFmRadioRefreshPolicy.ShouldSchedulePeriodicRefresh(user, settings, now));
        user.Refreshing = true;
        Assert.False(LastFmRadioRefreshPolicy.ShouldSchedulePeriodicRefresh(user, settings, now));
        user.Refreshing = false;
        user.LastRefreshError = "provider unavailable";
        user.LastRefreshAttemptUtc = now.AddMinutes(-5);
        Assert.False(LastFmRadioRefreshPolicy.ShouldSchedulePeriodicRefresh(user, settings, now));
        user.LastRefreshAttemptUtc = now.AddMinutes(-16);
        Assert.True(LastFmRadioRefreshPolicy.ShouldSchedulePeriodicRefresh(user, settings, now));
        var jitter = LastFmRadioRefreshPolicy.StartupJitter("Alice");
        Assert.Equal(jitter, LastFmRadioRefreshPolicy.StartupJitter("alice"));
        Assert.InRange(jitter.TotalMilliseconds, 100, 499);
    }

    [Fact]
    public async Task Worker_RetainsStaleSnapshotAndRecordsFailedReplacement()
    {
        using var fixture = new WorkerFixture(new LastFmSettings
            { EnableRadio = true, EnablePersonalizedStations = false, EnableDiscoveryStations = true });
        var prior = new LastFmRadioStation { Id = LastFmRadioStateStore.StationId("alice", "old"),
            Key = "old", Name = "Last Good", Owner = "alice",
            Tracks = [new() { Artist = "A", Title = "T" }] };
        fixture.State.ReplaceStations("alice", [prior]);
        await Assert.ThrowsAsync<InvalidOperationException>(() =>
            fixture.Worker.ProcessAsync(new LastFmRadioRefreshJob("alice")));
        var user = fixture.State.GetUser("alice");
        Assert.Equal("Last Good", Assert.Single(user.Stations).Name);
        Assert.NotNull(user.LastRefreshError);
        Assert.False(user.Refreshing);
    }

    [Fact]
    public async Task DefinitionChange_QueuesOnlyTheChangedPinnedDefinition()
    {
        using var fixture = new WorkerFixture(new LastFmSettings
        {
            DiscoveryStations = [new() { Id = "rock", Name = "Rock", Tags = ["rock"] }]
        });
        fixture.State.RecordPlay("alice", new LastFmRadioPlay { Artist = "A", Title = "T" });
        fixture.Settings.Set(new LastFmSettings
        {
            DiscoveryStations =
            [
                new() { Id = "rock", Name = "Rock", Tags = ["rock"] },
                new() { Id = "jazz", Name = "Jazz", Tags = ["jazz"] }
            ]
        });
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(1));
        var job = await fixture.Queue.DequeueAsync(timeout.Token);
        Assert.Equal("alice", job.Username);
        Assert.Equal("jazz", job.StationDefinitionId);
    }

    [Fact]
    public async Task Worker_CollapsesConcurrentRefreshesForTheSameUser()
    {
        using var fixture = new WorkerFixture(new LastFmSettings
            { ApiKey = "key", MinimumPlays = 3 }, new SlowEmptyHandler());
        for (var index = 0; index < 3; index++) fixture.State.RecordPlay("alice",
            new LastFmRadioPlay { Artist = "A" + index, Title = "T" + index });
        var first = fixture.Worker.ProcessAsync(new LastFmRadioRefreshJob("alice"));
        await Task.Delay(20);
        var second = fixture.Worker.ProcessAsync(new LastFmRadioRefreshJob("alice"));
        await Task.Delay(20);
        Assert.Equal(1, fixture.Worker.InFlightCount);
        Assert.True(await first);
        Assert.True(await second);
        Assert.Equal(0, fixture.Worker.InFlightCount);
    }

    private sealed class WorkerFixture : IDisposable
    {
        private readonly string _directory = System.IO.Path.Combine(System.IO.Path.GetTempPath(),
            "octo-radio-worker-" + Guid.NewGuid());
        private readonly ServiceProvider _provider;
        public TestOptionsMonitor<LastFmSettings> Settings { get; }
        public LastFmRadioStateStore State { get; }
        public LastFmRadioRefreshQueue Queue { get; } = new();
        public LastFmRadioRefreshWorker Worker { get; }

        public WorkerFixture(LastFmSettings settings, HttpMessageHandler? handler = null)
        {
            Directory.CreateDirectory(_directory);
            Settings = TestOptions.Monitor(settings);
            State = new LastFmRadioStateStore(System.IO.Path.Combine(_directory, "state.json"), Settings,
                new ExternalIdRegistry(), new Mock<ILogger<LastFmRadioStateStore>>().Object);
            var lastFm = new LastFmService(handler is null ? new HttpClient() : new HttpClient(handler), Settings,
                Options.Create(new MetadataSettings()), new Mock<ILogger<LastFmService>>().Object);
            var recommendation = new LastFmRadioRecommendationService(lastFm, State, Settings,
                new Mock<ILogger<LastFmRadioRecommendationService>>().Object);
            _provider = new ServiceCollection().AddSingleton(recommendation).BuildServiceProvider();
            Worker = new LastFmRadioRefreshWorker(Queue,
                _provider.GetRequiredService<IServiceScopeFactory>(), State, Settings,
                new Mock<ILogger<LastFmRadioRefreshWorker>>().Object);
        }
        public void Dispose()
        {
            _provider.Dispose();
            try { Directory.Delete(_directory, true); } catch { }
        }
    }

    private sealed class SlowEmptyHandler : HttpMessageHandler
    {
        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request,
            CancellationToken cancellationToken)
        {
            await Task.Delay(80, cancellationToken);
            return new HttpResponseMessage(System.Net.HttpStatusCode.OK)
                { Content = new StringContent("{}") };
        }
    }
}

public class LastFmRadioRecommendationTests
{
    [Fact]
    public async Task SparseHistory_ProducesDeterministicStarterAndLocalPinnedFallback()
    {
        var directory = System.IO.Path.Combine(System.IO.Path.GetTempPath(), "octo-radio-rec-" + Guid.NewGuid());
        Directory.CreateDirectory(directory);
        try
        {
            var settings = TestOptions.Monitor(new LastFmSettings
            {
                ApiKey = "", MinimumPlays = 10, RadioTrackCount = 10,
                DiscoveryStations = [new() { Id = "rock", Name = "Rock Discovery", Tags = ["rock"] }]
            });
            var state = new LastFmRadioStateStore(System.IO.Path.Combine(directory, "state.json"), settings,
                new ExternalIdRegistry(), new Mock<ILogger<LastFmRadioStateStore>>().Object);
            foreach (var index in Enumerable.Range(0, 6)) state.RecordPlay("alice", new LastFmRadioPlay
            {
                Artist = "Artist " + index, Title = "Track " + index, Genre = index < 5 ? "Rock" : "Jazz",
                PlayedAtUtc = DateTime.UtcNow.AddHours(-index)
            });
            var lastFm = new LastFmService(new HttpClient(), settings,
                Options.Create(new MetadataSettings { Language = "en" }),
                new Mock<ILogger<LastFmService>>().Object);
            var service = new LastFmRadioRecommendationService(lastFm, state, settings,
                new Mock<ILogger<LastFmRadioRecommendationService>>().Object);
            var first = await service.BuildAsync("alice");
            var second = await service.BuildAsync("alice");
            Assert.Contains(first, station => station.Kind == LastFmRadioStationKind.Starter);
            Assert.Contains(first, station => station.Kind == LastFmRadioStationKind.Pinned);
            Assert.DoesNotContain(first, station => station.Kind == LastFmRadioStationKind.YourMix);
            Assert.Equal(first.Select(station => station.Id), second.Select(station => station.Id));
            Assert.Equal(first.SelectMany(station => station.Tracks).Select(track => track.Artist + track.Title),
                second.SelectMany(station => station.Tracks).Select(track => track.Artist + track.Title));
        }
        finally { try { Directory.Delete(directory, true); } catch { } }
    }

    [Fact]
    public async Task LearnedProfile_AppliesDecayAliasesDenylistDiscoveryMergeAndSpacing()
    {
        var directory = System.IO.Path.Combine(System.IO.Path.GetTempPath(), "octo-radio-rec-" + Guid.NewGuid());
        Directory.CreateDirectory(directory);
        try
        {
            var settings = TestOptions.Monitor(new LastFmSettings
            {
                ApiKey = "key", MinimumPlays = 3, RadioTrackCount = 10, DiscoveryPercent = 50,
                DiscoveryStations = [new() { Id = "fusion", Name = "Fusion",
                    Tags = ["rock", "idm"] }]
            });
            var state = new LastFmRadioStateStore(System.IO.Path.Combine(directory, "state.json"), settings,
                new ExternalIdRegistry(), new Mock<ILogger<LastFmRadioStateStore>>().Object);
            for (var index = 0; index < 3; index++) state.RecordPlay("alice", new LastFmRadioPlay
            {
                Artist = "Fresh", Title = "Fresh Seed " + index, Genre = "Electronica",
                Hearted = index == 0, PlayedAtUtc = DateTime.UtcNow.AddHours(-index)
            });
            for (var index = 0; index < 9; index++) state.RecordPlay("alice", new LastFmRadioPlay
            {
                Artist = "Repeated Old", Title = "Old Seed " + index, Genre = "Rock",
                PlayedAtUtc = DateTime.UtcNow.AddDays(-80).AddHours(index)
            });
            state.RejectTrack("alice", new LastFmRadioTrack
                { Artist = "rock Artist 0", Title = "rock-0" });

            var service = RecommendationService(settings, state, new RecommendationHandler());
            var stations = await service.BuildAsync("alice");
            var mix = Assert.Single(stations, station => station.Kind == LastFmRadioStationKind.YourMix);
            var familiarKeys = state.GetUser("alice").Plays.Select(play =>
                LastFmRadioSeedNormalizer.TrackKey(play.Artist, play.Title)).ToHashSet();
            Assert.InRange(mix.Tracks.Count(track => familiarKeys.Contains(
                LastFmRadioSeedNormalizer.TrackKey(track.Artist, track.Title))), 1, 5);

            var freshNeighborhood = Assert.Single(stations, station =>
                station.Kind == LastFmRadioStationKind.Artist && station.Name == "Fresh Radio");
            Assert.True(freshNeighborhood.Tracks.Count >= 5);
            Assert.Contains(stations, station => station.Kind == LastFmRadioStationKind.Genre
                && station.Seeds.Contains("electronic"));
            Assert.DoesNotContain(stations.SelectMany(station => station.Seeds), seed => seed == "seen live");

            var pinned = Assert.Single(stations, station => station.Kind == LastFmRadioStationKind.Pinned);
            Assert.Contains(pinned.Tracks, track => track.Title.StartsWith("rock-"));
            Assert.Contains(pinned.Tracks, track => track.Title.StartsWith("idm-"));
            Assert.DoesNotContain(pinned.Tracks, track => track.Title == "rock-0");
            Assert.Equal(settings.CurrentValue.EffectiveRadioTrackCount, pinned.Tracks.Count);
            foreach (var station in stations)
            {
                Assert.Equal(station.Tracks.Count, station.Tracks.Select(track =>
                    LastFmRadioSeedNormalizer.TrackKey(track.Artist, track.Title)).Distinct().Count());
                Assert.DoesNotContain(station.Tracks.Zip(station.Tracks.Skip(1)), pair =>
                    pair.First.Artist.Equals(pair.Second.Artist, StringComparison.OrdinalIgnoreCase));
            }
        }
        finally { try { Directory.Delete(directory, true); } catch { } }
    }

    [Fact]
    public async Task PersonalizedAndPinnedTogglesAreIndependentAndDoNotDeleteState()
    {
        var directory = System.IO.Path.Combine(System.IO.Path.GetTempPath(), "octo-radio-toggle-" + Guid.NewGuid());
        Directory.CreateDirectory(directory);
        try
        {
            var settings = TestOptions.Monitor(new LastFmSettings
            {
                ApiKey = "", EnablePersonalizedStations = false, EnableDiscoveryStations = true,
                DiscoveryStations = [new() { Id = "rock", Name = "Rock", Tags = ["rock"] }]
            });
            var state = new LastFmRadioStateStore(System.IO.Path.Combine(directory, "state.json"), settings,
                new ExternalIdRegistry(), new Mock<ILogger<LastFmRadioStateStore>>().Object);
            for (var index = 0; index < 3; index++) state.RecordPlay("alice", new LastFmRadioPlay
                { Artist = "A" + index, Title = "T" + index, Genre = "rock" });
            var service = RecommendationService(settings, state, new RecommendationHandler());
            var pinnedOnly = await service.BuildAsync("alice");
            Assert.All(pinnedOnly, station => Assert.Equal(LastFmRadioStationKind.Pinned, station.Kind));

            settings.Set(new LastFmSettings
            {
                ApiKey = "", EnablePersonalizedStations = true, EnableDiscoveryStations = false,
                DiscoveryStations = settings.CurrentValue.DiscoveryStations
            });
            var personalizedOnly = await service.BuildAsync("alice");
            Assert.NotEmpty(personalizedOnly);
            Assert.DoesNotContain(personalizedOnly, station => station.Kind == LastFmRadioStationKind.Pinned);
            Assert.Equal(3, state.GetUser("alice").Plays.Count);
        }
        finally { try { Directory.Delete(directory, true); } catch { } }
    }

    private static LastFmRadioRecommendationService RecommendationService(
        TestOptionsMonitor<LastFmSettings> settings, LastFmRadioStateStore state,
        HttpMessageHandler handler)
    {
        var lastFm = new LastFmService(new HttpClient(handler), settings,
            Options.Create(new MetadataSettings { Language = "en" }),
            new Mock<ILogger<LastFmService>>().Object);
        return new LastFmRadioRecommendationService(lastFm, state, settings,
            new Mock<ILogger<LastFmRadioRecommendationService>>().Object);
    }

    private sealed class RecommendationHandler : HttpMessageHandler
    {
        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request,
            CancellationToken cancellationToken)
        {
            var query = System.Web.HttpUtility.ParseQueryString(request.RequestUri!.Query);
            var method = query["method"];
            object body = method switch
            {
                "artist.gettoptags" => new { toptags = new { tag = new[]
                {
                    new { name = "electronica" }, new { name = "seen live" }, new { name = "rock" }
                }}},
                "track.getsimilar" => new { similartracks = new { track = Enumerable.Range(0, 15)
                    .Select(index => new { name = $"similar-{index}", match = 1d - index / 100d,
                        duration = 180000, artist = new { name = $"Similar Artist {index}" } }).ToArray() }},
                "artist.getsimilar" => new { similarartists = new { artist = Enumerable.Range(0, 6)
                    .Select(index => new { name = $"Neighbor {index}", match = .9 - index / 10d }).ToArray() }},
                "artist.gettoptracks" => new { toptracks = new { track = Enumerable.Range(0, 12)
                    .Select(index => new { name = $"{query["artist"]}-top-{index}" }).ToArray() }},
                "tag.gettoptracks" => new { tracks = new { track = Enumerable.Range(0, 20)
                    .Select(index => new { name = $"{query["tag"]}-{index}", duration = 180000,
                        artist = new { name = $"{query["tag"]} Artist {index}" } }).ToArray() }},
                _ => new { }
            };
            return Task.FromResult(new HttpResponseMessage(System.Net.HttpStatusCode.OK)
            { Content = new StringContent(JsonSerializer.Serialize(body)) });
        }
    }
}
