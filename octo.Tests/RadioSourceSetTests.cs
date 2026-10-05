using Microsoft.Extensions.Logging.Abstractions;
using Octo.Models.Settings;
using Octo.Services.Radio;

namespace Octo.Tests;

/// <summary>
/// The radio sources asked together: base weights, the seed-aware Sounds alike weight, and a source
/// that keeps failing is rested instead of costing every radio its timeout.
/// </summary>
public sealed class RadioSourceSetTests
{
    private sealed class FakeSource(string provider, Func<CancellationToken, Task<RadioAnswer>> answer) : IRadioSource
    {
        public int Calls;
        public string Provider => provider;
        public bool Available => true;

        public Task<RadioAnswer> SongsLikeAsync(RadioSeed seed, int count,
            IReadOnlyDictionary<string, string> auth, CancellationToken ct)
        {
            Interlocked.Increment(ref Calls);
            return answer(ct);
        }
    }

    private static RadioSourceSet Set(RadioSourceSettings settings, params IRadioSource[] sources) =>
        new(sources, TestOptions.Monitor(settings), NullLogger<RadioSourceSet>.Instance);

    private static readonly RadioSeed Seed = new("Massive Attack", "Teardrop", 330, null);

    [Fact]
    public void BaseWeights_AreTheReasonedStartingPoints()
    {
        var weights = Set(new RadioSourceSettings()).Weights();
        Assert.Equal(1.0, weights[RadioProvider.YouTubeMusic]);
        Assert.Equal(0.9, weights[RadioProvider.LastFm]);
        Assert.Equal(0.7, weights[RadioProvider.ListenBrainz]);
        Assert.Equal(0.4, weights[RadioProvider.SoundsAlike]);
        Assert.Equal(1.0, weights[RadioProvider.Library]);
    }

    [Fact]
    public void WhenNoCatalogKnowsTheSong_SoundsAlikeCountsMore_UnlessItWasLeftOut()
    {
        Assert.Equal(RadioBlend.SoundsAlikeWhenUnknown, Set(new RadioSourceSettings()).Weights(libraryLed: true)[RadioProvider.SoundsAlike]);
        Assert.Equal(1.2, Set(new RadioSourceSettings { SoundsAlikeWeight = 1.2 }).Weights(libraryLed: true)[RadioProvider.SoundsAlike]);
        Assert.Equal(0, Set(new RadioSourceSettings { SoundsAlikeWeight = 0 }).Weights(libraryLed: true)[RadioProvider.SoundsAlike]);
    }

    [Fact]
    public void AnAdminWeightOutsideTheBounds_IsKept()
    {
        var weights = Set(new RadioSourceSettings { LastFmWeight = 0.1, YouTubeMusicWeight = 2.5, ListenBrainzWeight = 9 }).Weights();
        Assert.Equal(0.1, weights[RadioProvider.LastFm], 6);
        Assert.Equal(2.5, weights[RadioProvider.YouTubeMusic], 6);
        Assert.Equal(3, weights[RadioProvider.ListenBrainz], 6);
    }

    [Fact]
    public async Task ASlowSource_IsLeftOutOfOneRadio_TheOthersStillAnswer()
    {
        var saved = RadioSourceSet.SourceTimeout;
        RadioSourceSet.SourceTimeout = TimeSpan.FromMilliseconds(100);
        try
        {
            var slow = new FakeSource(RadioProvider.YouTubeMusic, async ct =>
            {
                await Task.Delay(Timeout.Infinite, ct);
                return RadioAnswer.Nothing(RadioProvider.YouTubeMusic);
            });
            var quick = new FakeSource(RadioProvider.LastFm, _ => Task.FromResult(
                new RadioAnswer(RadioProvider.LastFm, RadioMatch.Song, [new("Portishead", "Roads", 1)], [])));
            var answers = await Set(new RadioSourceSettings(), slow, quick).AskAllAsync(Seed, 10, new Dictionary<string, string>(), CancellationToken.None);
            Assert.Equal(RadioMatch.None, answers.Single(answer => answer.Provider == RadioProvider.YouTubeMusic).Match);
            Assert.Single(answers.Single(answer => answer.Provider == RadioProvider.LastFm).Tracks);
        }
        finally { RadioSourceSet.SourceTimeout = saved; }
    }

    [Fact]
    public async Task ASourceThatKeepsFailing_IsRested_ThenComesBack()
    {
        var savedRest = RadioSourceSet.Rest;
        RadioSourceSet.Rest = TimeSpan.FromMilliseconds(300);
        try
        {
            var failing = new FakeSource(RadioProvider.ListenBrainz, _ => throw new HttpRequestException("down"));
            var set = Set(new RadioSourceSettings(), failing);
            for (var i = 0; i < RadioSourceSet.FailuresBeforeRest; i++)
                await set.AskAllAsync(Seed, 10, new Dictionary<string, string>(), CancellationToken.None);
            Assert.Empty(set.Available);
            await set.AskAllAsync(Seed, 10, new Dictionary<string, string>(), CancellationToken.None);
            Assert.Equal(RadioSourceSet.FailuresBeforeRest, failing.Calls);

            await Task.Delay(400);
            Assert.Single(set.Available);
        }
        finally { RadioSourceSet.Rest = savedRest; }
    }

    [Fact]
    public async Task ASuccess_ClearsTheFailures()
    {
        var fail = true;
        var flaky = new FakeSource(RadioProvider.ListenBrainz, _ => fail
            ? throw new HttpRequestException("down")
            : Task.FromResult(RadioAnswer.Nothing(RadioProvider.ListenBrainz)));
        var set = Set(new RadioSourceSettings(), flaky);
        for (var i = 0; i < RadioSourceSet.FailuresBeforeRest - 1; i++)
            await set.AskAllAsync(Seed, 10, new Dictionary<string, string>(), CancellationToken.None);
        fail = false;
        await set.AskAllAsync(Seed, 10, new Dictionary<string, string>(), CancellationToken.None);
        fail = true;
        await set.AskAllAsync(Seed, 10, new Dictionary<string, string>(), CancellationToken.None);
        Assert.Single(set.Available);
    }
}
