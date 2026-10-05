using Microsoft.Extensions.Logging.Abstractions;
using Octo.Models.Settings;
using Octo.Services.Radio;

namespace Octo.Tests;

/// <summary>
/// What radio learns from listening: a started radio song finished within a day is kept, one never
/// finished is skipped, and each listener's counts give each source a multiplier around 1.
/// </summary>
public sealed class RadioOutcomeStoreTests : IDisposable
{
    private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-radio-outcomes-" + Guid.NewGuid());
    private static readonly DateTime T0 = new(2026, 10, 4, 12, 0, 0, DateTimeKind.Utc);
    private string StatePath => Path.Combine(_directory, "radio-outcomes.json");

    public RadioOutcomeStoreTests() => Directory.CreateDirectory(_directory);
    public void Dispose() { try { Directory.Delete(_directory, true); } catch { } }

    private RadioOutcomeStore Store(bool learn = true) =>
        new(StatePath, TestOptions.Monitor(new RadioSourceSettings { LearnFromListening = learn }), NullLogger<RadioOutcomeStore>.Instance);

    private static void Play(RadioOutcomeStore store, string listener, string song, DateTime start, DateTime? finish)
    {
        store.Observe(listener, [song], ["false"], start);
        if (finish is { } end) store.Observe(listener, [song], ["true"], end);
    }

    private static ProviderOutcome Row(RadioOutcomeStore store, string provider, DateTime now) =>
        store.Stats(now).Single(row => row.Provider == provider);

    [Fact]
    public void AStartedSongFinishedWithinADay_IsKept_OneNeverFinished_IsSkipped()
    {
        var store = Store();
        store.Served("alice", [("a", RadioProvider.YouTubeMusic), ("b", RadioProvider.YouTubeMusic)], T0);
        Play(store, "alice", "a", T0.AddMinutes(1), T0.AddMinutes(3));
        Play(store, "alice", "b", T0.AddMinutes(4), null);

        var later = T0.AddHours(25);
        var row = Row(store, RadioProvider.YouTubeMusic, later);
        Assert.Equal(2, row.Plays);
        // The keep has faded for the day the skip took to count.
        Assert.Equal(0.5, row.KeepRate, 2);
        Assert.Equal("YouTube Music", row.Name);
    }

    [Fact]
    public void WhatTeachesNothing_TeachesNothing()
    {
        var store = Store();
        store.Served("alice", [("served", RadioProvider.LastFm)], T0);
        // A finish with no start seen: a client that never sends starts would only ever count keeps.
        store.Observe("alice", ["served"], ["true"], T0.AddMinutes(1));
        // A song never served on radio.
        Play(store, "alice", "elsewhere", T0.AddMinutes(2), T0.AddMinutes(4));
        // A start long after the radio served it.
        Play(store, "alice", "served", T0.AddHours(7), T0.AddHours(7.1));
        // A song the listener played themselves.
        store.Served("alice", [("mine", RadioProvider.History)], T0);
        Play(store, "alice", "mine", T0.AddMinutes(5), T0.AddMinutes(9));

        Assert.Empty(store.Stats(T0.AddDays(2)));
        Assert.Empty(store.Multipliers("alice", T0.AddDays(2)));
    }

    [Fact]
    public void OneSubmissionForSeveralIds_IsForAllOfThem()
    {
        var store = Store();
        store.Served("alice", [("a", RadioProvider.LastFm), ("b", RadioProvider.LastFm)], T0);
        store.Observe("alice", ["a", "b"], ["false"], T0.AddMinutes(1));
        store.Observe("alice", ["a", "b"], ["true"], T0.AddMinutes(2));
        Assert.Equal(1.0, Row(store, RadioProvider.LastFm, T0.AddMinutes(3)).KeepRate);
        Assert.Equal(2, Row(store, RadioProvider.LastFm, T0.AddMinutes(3)).Plays);
    }

    [Fact]
    public void OneStar_IsThreeSkips()
    {
        var store = Store();
        store.Served("alice", [("a", RadioProvider.ListenBrainz)], T0);
        store.Observe("alice", ["a"], ["false"], T0.AddMinutes(1));
        store.Disliked("alice", "a", T0.AddMinutes(2));
        var row = Row(store, RadioProvider.ListenBrainz, T0.AddMinutes(3));
        Assert.Equal(3, row.Plays);
        Assert.Equal(0, row.KeepRate);
    }

    [Fact]
    public void Outcomes_FadeByHalfEverySixtyDays()
    {
        var store = Store();
        for (var i = 0; i < 10; i++)
        {
            store.Served("alice", [($"s{i}", RadioProvider.LastFm)], T0);
            Play(store, "alice", $"s{i}", T0.AddMinutes(i), T0.AddMinutes(i + 1));
        }
        Assert.Equal(10, Row(store, RadioProvider.LastFm, T0.AddHours(1)).Plays);
        Assert.Equal(5, Row(store, RadioProvider.LastFm, T0.AddDays(RadioOutcomeStore.HalfLifeDays)).Plays);
    }

    [Fact]
    public void ASourceSkippedMoreThanTheListenersAverage_CountsLess_OneKeptMore_CountsMore()
    {
        var store = Store();
        var at = T0;
        for (var i = 0; i < 40; i++)
        {
            store.Served("alice", [($"y{i}", RadioProvider.YouTubeMusic), ($"l{i}", RadioProvider.LastFm)], at);
            Play(store, "alice", $"y{i}", at.AddSeconds(1), at.AddSeconds(2));
            Play(store, "alice", $"l{i}", at.AddSeconds(3), null);
            at = at.AddMinutes(10);
        }
        var multipliers = store.Multipliers("alice", at.AddDays(2));
        Assert.True(multipliers[RadioProvider.YouTubeMusic] > 1.2, $"{multipliers[RadioProvider.YouTubeMusic]}");
        Assert.True(multipliers[RadioProvider.LastFm] < 0.8, $"{multipliers[RadioProvider.LastFm]}");
    }

    [Fact]
    public void AFewPlays_BarelyMoveASource()
    {
        var counts = new Dictionary<string, (double Kept, double Skipped)>
        {
            [RadioProvider.YouTubeMusic] = (3, 0),
            [RadioProvider.LastFm] = (30, 30),
        };
        Assert.InRange(RadioOutcomeStore.MultipliersOf(counts)[RadioProvider.YouTubeMusic], 1.0, 1.1);
    }

    [Fact]
    public void Multipliers_StayWithinTheirBounds()
    {
        var counts = new Dictionary<string, (double Kept, double Skipped)>
        {
            [RadioProvider.YouTubeMusic] = (0, 1000),
            [RadioProvider.LastFm] = (1000, 0),
        };
        var multipliers = RadioOutcomeStore.MultipliersOf(counts);
        Assert.Equal(RadioOutcomeStore.LowestMultiplier, multipliers[RadioProvider.YouTubeMusic]);
        Assert.Equal(RadioOutcomeStore.HighestMultiplier, multipliers[RadioProvider.LastFm]);
    }

    [Fact]
    public void WithLearningOff_NothingIsRecorded()
    {
        var store = Store(learn: false);
        store.Served("alice", [("a", RadioProvider.LastFm)], T0);
        Play(store, "alice", "a", T0.AddMinutes(1), T0.AddMinutes(2));
        Assert.Empty(store.Stats(T0.AddHours(1)));
        Assert.Empty(store.Multipliers("alice", T0.AddHours(1)));
    }

    [Fact]
    public void Listeners_AreOnePersonWhateverTheCase()
    {
        var store = Store();
        store.Served("Alice", [("a", RadioProvider.LastFm)], T0);
        Play(store, "alice ", "a", T0.AddMinutes(1), T0.AddMinutes(2));
        Assert.Single(store.Multipliers("ALICE", T0.AddMinutes(3)));
    }

    [Fact]
    public void AStationRefetch_DoesNotRenewTheWindow()
    {
        var store = Store();
        store.Served("alice", [("a", RadioProvider.LastFm)], T0, refresh: false);
        store.Served("alice", [("a", RadioProvider.LastFm)], T0.AddHours(5), refresh: false);
        Play(store, "alice", "a", T0.AddHours(7), T0.AddHours(7.1));
        Assert.Empty(store.Stats(T0.AddHours(8)));

        // A radio the listener asked for again does renew it.
        store.Served("alice", [("b", RadioProvider.LastFm)], T0);
        store.Served("alice", [("b", RadioProvider.LastFm)], T0.AddHours(5));
        Play(store, "alice", "b", T0.AddHours(7), T0.AddHours(7.1));
        Assert.Single(store.Stats(T0.AddHours(8)));
    }

    [Fact]
    public void TheQuietestListener_IsForgottenPastTheCap()
    {
        var store = Store();
        for (var i = 0; i <= RadioOutcomeStore.MaxListeners; i++)
        {
            var at = T0.AddMinutes(i * 10);
            store.Served($"user{i}", [("a", RadioProvider.LastFm)], at);
            Play(store, $"user{i}", "a", at.AddMinutes(1), at.AddMinutes(2));
        }
        Assert.Empty(store.Multipliers("user0", T0.AddDays(1)));
        Assert.NotEmpty(store.Multipliers($"user{RadioOutcomeStore.MaxListeners}", T0.AddDays(1)));
    }

    [Fact]
    public void WhatWasLearned_SurvivesARestart_AndForgetClearsIt()
    {
        var store = Store();
        store.Served("alice", [("a", RadioProvider.LastFm)], T0);
        Play(store, "alice", "a", T0.AddMinutes(1), T0.AddMinutes(2));
        store.Flush();

        var again = Store();
        Assert.Equal(1, Row(again, RadioProvider.LastFm, T0.AddMinutes(3)).Plays);
        again.Forget();
        Assert.Empty(Store().Stats(T0.AddMinutes(3)));
    }
}
