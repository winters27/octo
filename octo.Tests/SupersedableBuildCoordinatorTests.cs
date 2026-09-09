using Octo.Services.Common;

namespace Octo.Tests;

/// <summary>
/// The coordinator's join guard is what stops "cage", "cage t" from cancelling a build a
/// second caller already joined. Get that guard wrong and a client sharing a build with
/// someone else's type-ahead sees its own search cancelled for a query it never typed.
/// </summary>
public class SupersedableBuildCoordinatorTests
{
    private static readonly TimeSpan Generous = TimeSpan.FromSeconds(30);

    [Fact]
    public async Task AJoinedBuildSurvivesSupersession()
    {
        var coordinator = new SupersedableBuildCoordinator<int>();
        var started = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var release = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);

        async Task<int> CageFactory(CancellationToken ct)
        {
            started.TrySetResult();
            await release.Task.WaitAsync(ct);
            return 100;
        }

        var owner = coordinator.RunAsync("cage", CageFactory, Generous, fallback: -1, onFailure: (_, _) => { });
        await started.Task;

        // Joins the same build. SingleFlight increments its Joins counter synchronously
        // before this call can suspend, so the join guard sees it below.
        var joiner = coordinator.RunAsync("cage", CageFactory, Generous, fallback: -1, onFailure: (_, _) => { });

        var extend = coordinator.RunAsync("cage t", _ => Task.FromResult(200), Generous, fallback: -2, onFailure: (_, _) => { });

        release.SetResult();
        var results = await Task.WhenAll(owner, joiner, extend);

        Assert.Equal(100, results[0]);
        Assert.Equal(100, results[1]);
        Assert.Equal(200, results[2]);
    }

    [Fact]
    public async Task ALoneBuildIsSupersededAndFallsBackWithoutThrowing()
    {
        var coordinator = new SupersedableBuildCoordinator<int>();
        var started = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);

        async Task<int> CageFactory(CancellationToken ct)
        {
            started.TrySetResult();
            await Task.Delay(Generous, ct);
            return 1;
        }

        var cage = coordinator.RunAsync("cage", CageFactory, Generous, fallback: -1, onFailure: (_, _) => { });
        await started.Task;

        var cageT = coordinator.RunAsync("cage t", _ => Task.FromResult(2), Generous, fallback: -2, onFailure: (_, _) => { });

        Assert.Equal(-1, await cage);
        Assert.Equal(2, await cageT);
    }

    [Fact]
    public async Task AnUnrelatedQueryDoesNotCancelANonPrefixBuild()
    {
        var coordinator = new SupersedableBuildCoordinator<int>();
        var started = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var release = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);

        async Task<int> CageFactory(CancellationToken ct)
        {
            started.TrySetResult();
            await release.Task.WaitAsync(ct);
            return 1;
        }

        var cage = coordinator.RunAsync("cage", CageFactory, Generous, fallback: -1, onFailure: (_, _) => { });
        await started.Task;

        var beths = await coordinator.RunAsync("beths", _ => Task.FromResult(2), Generous, fallback: -2, onFailure: (_, _) => { });
        Assert.Equal(2, beths);

        release.SetResult();
        Assert.Equal(1, await cage);
    }
}
