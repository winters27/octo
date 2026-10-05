using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Logging.Abstractions;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.Library;

namespace Octo.Tests;

/// <summary>
/// Find songs picks as they reach the queues: a second pick of a song waiting in the upgrade
/// queue, picks no download used, and how many looks one person may run at once.
/// </summary>
public sealed class FindSongsPickTests
{
    private static readonly FindTarget Owned =
        new("Air", "Sexy Boy", "Moon Safari", null, "nd-1", LibraryId: "nd-1", OwnedFormat: "mp3");

    private sealed class Server
    {
        public required SongFinder Finder { get; init; }
        public required UpgradeQueue Queue { get; init; }
        public required ServiceProvider Services { get; init; }
    }

    private static Server Build(FindTarget target, LibraryActionSettings? actions = null)
    {
        actions ??= new LibraryActionSettings
        {
            Enabled = true, DryRun = false, AllowedUsers = ["alice", "bob"],
            Actions = [new LibraryActionDefinition { Action = LibraryAction.BetterQuality, Enabled = true }],
        };
        var actionsMonitor = TestOptions.Monitor(actions);
        var soulseek = TestOptions.Monitor(new SoulseekSettings { BaseUrl = "http://slskd.test", Username = "u", Password = "p" });
        var queue = new UpgradeQueue();
        var services = new ServiceCollection()
            .AddSingleton<IOptionsMonitor<LibraryActionSettings>>(actionsMonitor)
            .AddSingleton<IOptionsMonitor<SoulseekSettings>>(soulseek)
            .AddSingleton(queue)
            .AddSingleton(new DownloadPicks())
            .AddSingleton(new UpgradeSources(actionsMonitor, soulseek, TestOptions.Monitor(new LidarrSettings())))
            .AddSingleton(new LibraryActionExecutor(
                resolver: null!, quarantine: null!, journal: null!, library: null!, ids: null!,
                rejectedPeers: null!, acquisitions: null!,
                settings: actionsMonitor, soulseek: soulseek,
                subsonicSettings: TestOptions.Monitor(new SubsonicSettings()),
                logger: NullLogger<LibraryActionExecutor>.Instance))
            .BuildServiceProvider();
        var finder = new SongFinder(services, NullLogger<SongFinder>.Instance)
        {
            Resolve = (_, _) => Task.FromResult<FindTarget?>(target),
            SourcesFor = _ => new Dictionary<string, string?> { [SongFinder.SoulseekSource] = null },
            SearchSoulseek = (_, _) => Task.FromResult<(IReadOnlyList<FoundCopy>, IReadOnlyList<string>, string?)>((
                [Flac("peer1", 1), Flac("peer2", 2), Flac("peer3", 3)], ["Air Sexy Boy"], "3 files from 3 peers; 3 fit the song")),
        };
        return new Server { Finder = finder, Queue = queue, Services = services };
    }

    private static FoundCopy Flac(string peer, int rank)
    {
        var file = $@"Music\Air\Moon Safari (1998)\02 - Sexy Boy.flac";
        return new FoundCopy(new AcquisitionCandidate(SongFinder.SoulseekSource, peer, "02 - Sexy Boy.flac", "Moon Safari (1998)", "flac",
                Size: 30_000_000, Length: 298, Rank: rank),
            new PickedCopy { Source = SongFinder.SoulseekSource, Peer = peer, File = file, Size = 30_000_000, Format = "flac", Length = 298 });
    }

    private static async Task<FindSnapshot> LookAsync(SongFinder finder, string user = "alice") =>
        (await finder.WaitAsync((await finder.StartAsync("nd-1", user))!.Id, TimeSpan.FromSeconds(10)))!;

    // ---- A second pick ----------------------------------------------------------------------

    [Fact]
    public async Task ASecondPickBeforeTheUpgradeStartsTakesThePlaceOfTheFirst()
    {
        var server = Build(Owned);
        var look = await LookAsync(server.Finder);

        Assert.Equal(LibraryActionStates.Queued, (await server.Finder.PickAsync(look.Id, look.Copies[0].Id, "alice")).State);
        Assert.Equal("peer1", Assert.Single(server.Queue.Snapshot()).Pick!.Peer);

        var second = await server.Finder.PickAsync(look.Id, look.Copies[1].Id, "alice");

        Assert.Equal(LibraryActionStates.Queued, second.State);
        Assert.StartsWith("Changed to the copy you picked", second.Detail);
        Assert.Equal("peer2", Assert.Single(server.Queue.Snapshot()).Pick!.Peer);
    }

    [Fact]
    public async Task APickForAnUpgradeAlreadyRunningOrSomeoneElsesSaysAnotherCopyIsOnItsWay()
    {
        var server = Build(Owned);
        var look = await LookAsync(server.Finder);
        await server.Finder.PickAsync(look.Id, look.Copies[0].Id, "alice");

        var bobs = await LookAsync(server.Finder, "bob");
        var bob = await server.Finder.PickAsync(bobs.Id, bobs.Copies[2].Id, "bob");
        Assert.Equal(LibraryActionStates.Skipped, bob.State);
        Assert.Equal("Another copy of this song is already on its way. Pick again once it ends.", bob.Detail);

        Assert.NotNull(server.Queue.TakeNext());
        var late = await server.Finder.PickAsync(look.Id, look.Copies[1].Id, "alice");
        Assert.Equal(LibraryActionStates.Skipped, late.State);
        Assert.Equal("peer1", Assert.Single(server.Queue.Snapshot()).Pick!.Peer);

        // The same copy again is the pick already waiting, not another one.
        Assert.Equal(LibraryActionStates.Queued, (await server.Finder.PickAsync(look.Id, look.Copies[0].Id, "alice")).State);
    }
}
