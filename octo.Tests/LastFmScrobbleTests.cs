using System.Net;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Logging.Abstractions;
using Octo.Controllers;
using Octo.Middleware;
using Octo.Models.Settings;
using Octo.Services.Admin;
using Octo.Services.LastFm;
using Octo.Services.Soulseek;

namespace Octo.Tests;

/// <summary>
/// Outside songs reach Last.fm because Navidrome never hears of them. Library songs must not,
/// because Navidrome scrobbles those itself and a second copy would count every play twice.
/// Everything here talks to <see cref="FakeLastFm"/>; no test reaches the real Last.fm.
/// </summary>
public sealed class LastFmScrobbleServiceTests : IDisposable
{
    private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-lastfm-" + Guid.NewGuid());
    private readonly FakeLastFm _lastFm = new();
    private readonly TestOptionsMonitor<LastFmSettings> _settings;
    private readonly SettingsFileWriter _file;
    private readonly LastFmScrobbleService _service;

    public LastFmScrobbleServiceTests()
    {
        Directory.CreateDirectory(_directory);
        _file = new SettingsFileWriter(Path.Combine(_directory, "settings.json"));
        _file.Merge(JsonNode.Parse("""{ "LastFm": { "UserSessions": { "alice": { "SessionKey": "sk-alice", "LastFmUser": "lfm-alice" } } } }""")!.AsObject());
        _settings = TestOptions.Monitor(new LastFmSettings
        {
            ApiKey = FakeLastFm.ApiKey,
            ApiSecret = FakeLastFm.Secret,
            UserSessions = new(StringComparer.OrdinalIgnoreCase)
            {
                ["alice"] = new LastFmUserSession { SessionKey = "sk-alice", LastFmUser = "lfm-alice" },
            },
        });
        _service = new LastFmScrobbleService(new ReviewFixtures.OneClientFactory(_lastFm), _settings, _file,
            NullLogger<LastFmScrobbleService>.Instance)
        {
            RetryDelay = TimeSpan.FromMilliseconds(20),
            RateLimitPause = TimeSpan.FromMilliseconds(300),
        };
    }

    public void Dispose()
    {
        try { Directory.Delete(_directory, true); } catch { }
    }

    private static readonly LastFmTrack Song = new("Bladee", "Be Nice 2 Me", "Icedancer", 154);

    /// <summary>
    /// The authspec's own worked example, auth.getSession, with the MD5 worked out by hand
    /// outside this code (md5sum of "api_key..." + "method..." + "token..." + secret). format
    /// is sent but never signed.
    /// </summary>
    [Fact]
    public void Signature_MatchesAHandComputedVector()
    {
        var parameters = new Dictionary<string, string>
        {
            ["token"] = "tok123",
            ["format"] = "json",
            ["method"] = "auth.getSession",
            ["api_key"] = FakeLastFm.ApiKey,
        };

        Assert.Equal("5f50f7c80ec9a4fe05f95ce8ee49b1f8", LastFmScrobbleService.Sign(parameters, FakeLastFm.Secret));
    }

    /// <summary>Sorted by name, spaces kept as they are, whatever order they were added in.</summary>
    [Fact]
    public void Signature_SortsParametersByName()
    {
        var parameters = new Dictionary<string, string>
        {
            ["track"] = "Be Nice 2 Me",
            ["sk"] = "sk-alice",
            ["method"] = "track.updateNowPlaying",
            ["artist"] = "Bladee",
            ["api_key"] = FakeLastFm.ApiKey,
            ["album"] = "Icedancer",
        };

        Assert.Equal("33dc1eb9105f636ce7cde482d248d31b", LastFmScrobbleService.Sign(parameters, FakeLastFm.Secret));
    }

    [Fact]
    public async Task CompletedPlay_IsScrobbledWithEverythingLastFmAsksFor()
    {
        var playedAt = DateTimeOffset.UtcNow.AddHours(-1).ToUnixTimeSeconds();
        _service.Scrobble("Alice", Song, DateTimeOffset.FromUnixTimeSeconds(playedAt).UtcDateTime);
        await WhenIdle();

        var call = Assert.Single(_lastFm.CallsTo("track.scrobble"));
        Assert.Equal("sk-alice", call["sk"]);
        Assert.Equal(FakeLastFm.ApiKey, call["api_key"]);
        Assert.Equal("Bladee", call["artist[0]"]);
        Assert.Equal("Be Nice 2 Me", call["track[0]"]);
        Assert.Equal("Icedancer", call["album[0]"]);
        Assert.Equal("154", call["duration[0]"]);
        Assert.Equal(playedAt.ToString(), call["timestamp[0]"]);
        Assert.False(call.ContainsKey("chosenByUser[0]"));
        Assert.Equal(LastFmScrobbleService.Sign(call, FakeLastFm.Secret), call["api_sig"]);
    }

    [Fact]
    public async Task ShortTracksAndUsersWithoutASession_SendNothing()
    {
        _service.Scrobble("alice", Song with { DurationSeconds = 29 }, DateTime.UtcNow);
        _service.Scrobble("bob", Song, DateTime.UtcNow);
        _service.NowPlaying("bob", Song);
        await WhenIdle();

        Assert.Empty(_lastFm.Calls);
    }

    /// <summary>Plays that pile up while one call is out go together, at most 50 a call.</summary>
    [Fact]
    public async Task QueuedPlays_GoInBatchesOfFifty()
    {
        var release = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        _lastFm.Hold = _ => release.Task;
        _service.Scrobble("alice", Song, DateTime.UtcNow);
        await Until(() => _lastFm.Calls.Count == 1);
        for (var i = 0; i < 60; i++)
            _service.Scrobble("alice", Song with { Title = $"Song {i}" }, DateTime.UtcNow.AddMinutes(-i));
        _lastFm.Hold = null;
        release.SetResult();
        await WhenIdle();

        Assert.Equal([1, 50, 10], _lastFm.CallsTo("track.scrobble")
            .Select(call => call.Keys.Count(key => key.StartsWith("artist[", StringComparison.Ordinal))));
    }

    [Fact]
    public async Task UnreachableLastFm_IsRetried()
    {
        _lastFm.Failures.Enqueue(0);
        _lastFm.Failures.Enqueue(16);

        _service.Scrobble("alice", Song, DateTime.UtcNow);
        await WhenIdle();

        var calls = _lastFm.CallsTo("track.scrobble");
        Assert.Equal(3, calls.Count);
        Assert.Single(calls.Select(call => call["timestamp[0]"]).Distinct());
    }

    [Fact]
    public async Task RetriesAreBounded()
    {
        for (var i = 0; i < 10; i++) _lastFm.Failures.Enqueue(0);

        _service.Scrobble("alice", Song, DateTime.UtcNow);
        await WhenIdle();

        Assert.Equal(LastFmScrobbleService.MaxAttempts, _lastFm.CallsTo("track.scrobble").Count);
    }

    /// <summary>Error 29: nothing more goes until the pause is over, then the play still does.</summary>
    [Fact]
    public async Task RateLimit_PausesThenSends()
    {
        _lastFm.Failures.Enqueue(LastFmScrobbleService.ErrorRateLimited);

        // The pause runs on a clock the test moves, so a slow machine cannot end it early.
        var clock = new ManualClock();
        _service.Time = clock;

        _service.Scrobble("alice", Song, DateTime.UtcNow);
        // Refused, and the queue now waits out the pause.
        await Until(() => clock.Waiting == 1);
        // A play and a Now Playing arriving meanwhile wait too.
        _service.Scrobble("alice", Song with { Title = "Song 2" }, DateTime.UtcNow);
        _service.NowPlaying("alice", Song);
        await Until(() => _service.Outstanding == 2);
        clock.Advance(_service.RateLimitPause - TimeSpan.FromTicks(1));
        Assert.Equal(1, clock.Waiting);
        Assert.Single(_lastFm.Calls);

        clock.Advance(TimeSpan.FromTicks(1));
        await WhenIdle();

        var calls = _lastFm.Calls;
        Assert.Equal(["track.scrobble", "track.scrobble"], calls.Select(call => call["method"]));
        // The refused play went again, with the one that waited behind it.
        Assert.Equal(["Be Nice 2 Me", "Song 2"], [calls[1]["track[0]"], calls[1]["track[1]"]]);
    }

    /// <summary>A clock that moves only when told, with the timers Task.Delay sets on it.</summary>
    private sealed class ManualClock : TimeProvider
    {
        private readonly object _lock = new();
        private readonly List<Timer> _timers = [];
        private DateTimeOffset _now = DateTimeOffset.UtcNow;

        public override DateTimeOffset GetUtcNow() { lock (_lock) return _now; }

        /// <summary>Timers set and not yet fired.</summary>
        public int Waiting { get { lock (_lock) return _timers.Count; } }

        public override ITimer CreateTimer(TimerCallback callback, object? state, TimeSpan dueTime, TimeSpan period)
        {
            var timer = new Timer(this, callback, state);
            timer.Change(dueTime, period);
            return timer;
        }

        public void Advance(TimeSpan by)
        {
            List<Timer> due;
            lock (_lock)
            {
                _now += by;
                due = _timers.Where(timer => timer.Due <= _now).ToList();
                foreach (var timer in due) _timers.Remove(timer);
            }
            foreach (var timer in due) timer.Fire();
        }

        private sealed class Timer(ManualClock clock, TimerCallback callback, object? state) : ITimer
        {
            public DateTimeOffset Due { get; private set; }

            public void Fire() => callback(state);

            public bool Change(TimeSpan dueTime, TimeSpan period)
            {
                lock (clock._lock)
                {
                    clock._timers.Remove(this);
                    if (dueTime == Timeout.InfiniteTimeSpan) return true;
                    Due = clock._now + dueTime;
                    clock._timers.Add(this);
                }
                return true;
            }

            public void Dispose() { lock (clock._lock) clock._timers.Remove(this); }

            public ValueTask DisposeAsync()
            {
                Dispose();
                return ValueTask.CompletedTask;
            }
        }
    }

    /// <summary>Error 9 is Last.fm saying the listener revoked Octo. The session stops at once and
    /// the dashboard says why, but one refusal is not enough to delete what the admin saved, nor
    /// the plays: the refused one and any new ones wait out the grace.</summary>
    [Fact]
    public async Task InvalidSession_PausesTheUser_AndKeepsTheSavedSession()
    {
        _lastFm.Failures.Enqueue(LastFmScrobbleService.ErrorInvalidSession);

        _service.Scrobble("alice", Song, DateTime.UtcNow);
        await Until(() => Assert.Single(_service.Users([]), user => user.User == "alice").Notice is not null);

        Assert.Equal("sk-alice", SavedSession("alice"));
        var alice = Assert.Single(_service.Users([]), user => user.User == "alice");
        Assert.False(alice.Connected);
        Assert.Contains("Connect again", alice.Notice);

        _service.Scrobble("alice", Song with { Title = "Later" }, DateTime.UtcNow);
        _service.NowPlaying("alice", Song);
        await Task.Delay(100);
        Assert.Single(_lastFm.Calls);
        Assert.Equal(2, _service.Outstanding);
    }

    /// <summary>After the grace the session is tried once more. Refused again, it is removed.</summary>
    [Fact]
    public async Task InvalidSession_TwiceAnHourApart_RemovesTheSavedSession()
    {
        _service.RefusalGrace = TimeSpan.FromMilliseconds(150);
        _lastFm.Failures.Enqueue(LastFmScrobbleService.ErrorInvalidSession);
        _lastFm.Failures.Enqueue(LastFmScrobbleService.ErrorInvalidSession);
        var second = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        _lastFm.Hold = _ => _lastFm.Calls.Count > 1 ? second.Task : Task.CompletedTask;
        _service.Scrobble("alice", Song, DateTime.UtcNow);

        // The kept play is what tries the session again, once the grace is over.
        await Until(() => _lastFm.Calls.Count == 2);
        Assert.Equal("sk-alice", SavedSession("alice"));
        second.SetResult();
        await WhenIdle();

        Assert.Equal(2, _lastFm.CallsTo("track.scrobble").Count);
        Assert.True(_lastFm.CallTimes[1] - _lastFm.CallTimes[0] >= _service.RefusalGrace);
        Assert.Null(SavedSession("alice"));
        await Task.Delay(200);
        Assert.False(_service.IsEnabledFor("alice"));
        _service.Scrobble("alice", Song, DateTime.UtcNow);
        Assert.Equal(0, _service.Outstanding);
        Assert.Contains("Connect again", Assert.Single(_service.Users([]), user => user.User == "alice").Notice);
    }

    /// <summary>A session Last.fm takes again after the grace was never revoked: the notice goes.</summary>
    [Fact]
    public async Task InvalidSession_ThenAccepted_IsConnectedAgain()
    {
        _service.RefusalGrace = TimeSpan.FromMilliseconds(150);
        _lastFm.Failures.Enqueue(LastFmScrobbleService.ErrorInvalidSession);
        _service.Scrobble("alice", Song, DateTime.UtcNow);
        await WhenIdle();

        // The play refused with the session was kept, and went once the grace was over.
        var calls = _lastFm.CallsTo("track.scrobble");
        Assert.Equal(2, calls.Count);
        Assert.Equal(calls[0]["timestamp[0]"], calls[1]["timestamp[0]"]);
        Assert.Equal("sk-alice", SavedSession("alice"));
        var alice = Assert.Single(_service.Users([]), user => user.User == "alice");
        Assert.True(alice.Connected);
        Assert.Null(alice.Notice);
    }

    /// <summary>Error 8 ("operation failed") comes back the same however often it is asked.</summary>
    [Fact]
    public async Task OperationFailed_IsNotRetried()
    {
        _lastFm.Failures.Enqueue(8);

        _service.Scrobble("alice", Song, DateTime.UtcNow);
        await WhenIdle();

        Assert.Single(_lastFm.CallsTo("track.scrobble"));
    }

    /// <summary>Last.fm ignores plays older than two weeks, so they are not sent to be ignored.</summary>
    [Fact]
    public async Task PlaysOlderThanTwoWeeks_AreNotQueued()
    {
        _service.Scrobble("alice", Song, DateTime.UtcNow.AddDays(-15));
        _service.Scrobble("alice", Song with { Title = "Recent" }, DateTime.UtcNow.AddDays(-13));
        await WhenIdle();

        var call = Assert.Single(_lastFm.CallsTo("track.scrobble"));
        Assert.Equal("Recent", call["track[0]"]);
        Assert.False(call.ContainsKey("track[1]"));
    }

    /// <summary>A radio stream picks the next song itself; Last.fm is told the listener did not.</summary>
    [Fact]
    public async Task APlayTheListenerDidNotPick_IsSentAsNotChosen()
    {
        var release = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        _lastFm.Hold = _ => release.Task;
        _service.Scrobble("alice", Song, DateTime.UtcNow.AddMinutes(-9));
        await Until(() => _lastFm.Calls.Count == 1);
        _service.Scrobble("alice", Song with { Title = "Picked" }, DateTime.UtcNow.AddMinutes(-6));
        _service.Scrobble("alice", Song with { Title = "Radio" }, DateTime.UtcNow.AddMinutes(-3), chosenByUser: false);
        _lastFm.Hold = null;
        release.SetResult();
        await WhenIdle();

        var batch = _lastFm.CallsTo("track.scrobble")[^1];
        Assert.Equal("Picked", batch["track[0]"]);
        Assert.False(batch.ContainsKey("chosenByUser[0]"));
        Assert.Equal("Radio", batch["track[1]"]);
        Assert.Equal("0", batch["chosenByUser[1]"]);
        Assert.Equal(LastFmScrobbleService.Sign(batch, FakeLastFm.Secret), batch["api_sig"]);
    }

    private string? SavedSession(string user) =>
        ((((_file.Load()["LastFm"] as JsonObject)?["UserSessions"] as JsonObject)?[user]) as JsonObject)?["SessionKey"]
            ?.GetValue<string>();

    [Fact]
    public async Task InvalidSession_OnNowPlaying_AlsoDisconnects()
    {
        _lastFm.Failures.Enqueue(LastFmScrobbleService.ErrorInvalidSession);

        _service.NowPlaying("alice", Song);
        await WhenIdle();

        Assert.Equal("track.updateNowPlaying", Assert.Single(_lastFm.Calls)["method"]);
        Assert.False(Assert.Single(_service.Users([]), user => user.User == "alice").Connected);
    }

    /// <summary>A refusal by Now Playing rests the session too: a play finished meanwhile waits
    /// for the grace rather than going with it, and goes after it.</summary>
    [Fact]
    public async Task InvalidSession_OnNowPlaying_PlaysWaitOutTheGrace()
    {
        _service.RefusalGrace = TimeSpan.FromMilliseconds(150);
        _lastFm.Failures.Enqueue(LastFmScrobbleService.ErrorInvalidSession);
        _service.NowPlaying("alice", Song);
        await WhenIdle();

        _service.Scrobble("alice", Song, DateTime.UtcNow);
        await WhenIdle();

        var times = _lastFm.CallTimes;
        Assert.Equal(2, times.Count);
        Assert.Single(_lastFm.CallsTo("track.scrobble"));
        Assert.True(times[1] - times[0] >= _service.RefusalGrace);
    }

    /// <summary>One listener's plays waiting out a refusal do not hold back another's.</summary>
    [Fact]
    public async Task ARestingListener_DoesNotHoldBackAnother()
    {
        _settings.Set(new LastFmSettings
        {
            ApiKey = FakeLastFm.ApiKey, ApiSecret = FakeLastFm.Secret,
            UserSessions = new(StringComparer.OrdinalIgnoreCase)
            {
                ["alice"] = new LastFmUserSession { SessionKey = "sk-alice", LastFmUser = "lfm-alice" },
                ["bob"] = new LastFmUserSession { SessionKey = "sk-bob", LastFmUser = "lfm-bob" },
            },
        });
        _lastFm.Failures.Enqueue(LastFmScrobbleService.ErrorInvalidSession);
        _service.Scrobble("alice", Song, DateTime.UtcNow);
        await Until(() => _lastFm.Calls.Count == 1 && _service.Users([]).Single(user => user.User == "alice").Notice is not null);

        _service.Scrobble("bob", Song, DateTime.UtcNow);
        await Until(() => _service.Outstanding == 1);

        Assert.Equal(["sk-alice", "sk-bob"], _lastFm.CallsTo("track.scrobble").Select(call => call["sk"]));
    }

    [Fact]
    public async Task SwitchedOff_SendsNothing()
    {
        _settings.Set(new LastFmSettings
        {
            ApiKey = FakeLastFm.ApiKey, ApiSecret = FakeLastFm.Secret, ScrobbleExternalPlays = false,
            UserSessions = _settings.CurrentValue.UserSessions,
        });

        _service.Scrobble("alice", Song, DateTime.UtcNow);
        _service.NowPlaying("alice", Song);
        await WhenIdle();

        Assert.Empty(_lastFm.Calls);
    }

    // ---- The dashboard's Connect flow -----------------------------------------------------------
    // The settings monitor here never reloads from the file, which is the moment right after Finish
    // on a real server: settings.json has the session, the running settings do not yet.

    [Fact]
    public async Task Finish_ShowsTheListenerConnectedBeforeTheSettingsReload()
    {
        await _service.BeginConnectAsync("bob", CancellationToken.None);
        _lastFm.Approved = true;
        await _service.FinishConnectAsync("bob", CancellationToken.None);

        var bob = Assert.Single(_service.Users([]), user => user.User == "bob");
        Assert.True(bob.Connected);
        Assert.Equal("lfm-alice", bob.LastFmUser);
        Assert.True(_service.IsEnabledFor("bob"));
    }

    [Fact]
    public async Task Disconnect_RightAfterFinish_IsNotConnected()
    {
        await _service.BeginConnectAsync("bob", CancellationToken.None);
        _lastFm.Approved = true;
        await _service.FinishConnectAsync("bob", CancellationToken.None);

        _service.Disconnect("bob");

        Assert.DoesNotContain(_service.Users(["bob"]), user => user.Connected && user.User == "bob");
        Assert.False(_service.IsEnabledFor("bob"));
    }

    [Fact]
    public async Task AWaitingConnect_CarriesItsApprovalLink_UntilCancelled()
    {
        var url = await _service.BeginConnectAsync("bob", CancellationToken.None);

        var bob = Assert.Single(_service.Users([]), user => user.User == "bob");
        Assert.True(bob.AwaitingApproval);
        Assert.Equal(url, bob.ApprovalUrl);

        _service.CancelConnect("bob");

        Assert.DoesNotContain(_service.Users([]), user => user.User == "bob");
        await Assert.ThrowsAsync<LastFmScrobbleException>(() => _service.FinishConnectAsync("bob", CancellationToken.None));
    }

    [Fact]
    public async Task LastSent_IsThePlayLastFmTook()
    {
        var playedAt = DateTime.UtcNow.AddMinutes(-3);
        _service.Scrobble("alice", Song, playedAt);
        await WhenIdle();

        var sent = Assert.Single(_service.Users([]), user => user.User == "alice").LastSent;
        Assert.NotNull(sent);
        Assert.Equal(("Bladee", "Be Nice 2 Me"), (sent.Artist, sent.Title));
        Assert.Equal(playedAt, sent.PlayedAtUtc, TimeSpan.FromSeconds(1));
    }

    // ---- Checking a key and secret before they are saved -----------------------------------------

    [Fact]
    public async Task Check_TheSavedPair_IsOk()
    {
        var check = await _service.CheckCredentialsAsync(null, null, CancellationToken.None);

        Assert.Equal(("ok", "ok"), (check.Key, check.Secret));
        var call = Assert.Single(_lastFm.CallsTo("auth.getSession"));
        Assert.False(_lastFm.Approved);
        Assert.Equal(FakeLastFm.ApiKey, call["api_key"]);
    }

    [Fact]
    public async Task Check_ASecretFromAnotherApp_IsInvalid()
    {
        var check = await _service.CheckCredentialsAsync(null, "not-the-secret", CancellationToken.None);

        Assert.Equal(("ok", "invalid"), (check.Key, check.Secret));
    }

    [Fact]
    public async Task Check_TheKeyPastedAsTheSecret_SaysSo()
    {
        var check = await _service.CheckCredentialsAsync(FakeLastFm.ApiKey, FakeLastFm.ApiKey.ToUpperInvariant(), CancellationToken.None);

        Assert.Equal(("ok", "same-as-key"), (check.Key, check.Secret));
    }

    [Fact]
    public async Task Check_AKeyLastFmDoesNotKnow_IsInvalid()
    {
        _lastFm.Failures.Enqueue(10);

        var check = await _service.CheckCredentialsAsync("ffffffffffffffffffffffffffffffff", null, CancellationToken.None);

        Assert.Equal(("invalid", "unchecked"), (check.Key, check.Secret));
    }

    [Fact]
    public async Task Check_LastFmDown_IsUnreachable_NotInvalid()
    {
        _lastFm.Failures.Enqueue(0);

        var check = await _service.CheckCredentialsAsync(null, null, CancellationToken.None);

        Assert.Equal(("unreachable", "unchecked"), (check.Key, check.Secret));
    }

    [Fact]
    public async Task Check_NoKeyAtAll_IsMissing_AndAsksNothing()
    {
        _settings.Set(new LastFmSettings());

        var check = await _service.CheckCredentialsAsync(null, null, CancellationToken.None);

        Assert.Equal(("missing", "missing"), (check.Key, check.Secret));
        Assert.Empty(_lastFm.Calls);
    }

    private Task WhenIdle() => Until(() => _service.Outstanding == 0);

    internal static async Task Until(Func<bool> condition)
    {
        for (var attempt = 0; attempt < 300 && !condition(); attempt++) await Task.Delay(10);
        Assert.True(condition());
    }
}

/// <summary>A played song reaching Octo's /rest/scrobble, all the way to what Last.fm is sent.</summary>
public sealed class LastFmScrobbleEndpointTests
{
    private static string RegisterOutsideSong(RadioWebFactory fixture) =>
        fixture.Services.GetRequiredService<ExternalIdRegistry>().Register(new SoulseekRouting
        {
            Kind = RoutingKind.Song, Artist = "Bladee", Title = "Be Nice 2 Me", Album = "Icedancer", Duration = 154,
        });

    private static async Task WhenIdle(RadioWebFactory fixture)
    {
        var service = fixture.Services.GetRequiredService<LastFmScrobbleService>();
        await LastFmScrobbleServiceTests.Until(() => service.Outstanding == 0);
    }

    [Fact]
    public async Task OutsideSong_CompletedPlay_IsScrobbled()
    {
        await using var fixture = new RadioWebFactory(lastFmScrobbling: true);
        var id = RegisterOutsideSong(fixture);
        using var client = fixture.CreateClient();

        var playedAt = DateTimeOffset.UtcNow.AddMinutes(-5).ToUnixTimeSeconds();
        var body = await client.GetStringAsync(
            $"/rest/scrobble?u=bob&t=token&s=salt&f=json&id={id}&submission=true&time={playedAt}000");
        await WhenIdle(fixture);

        Assert.Contains("\"status\":\"ok\"", body);
        var call = Assert.Single(fixture.Handler.LastFm.CallsTo("track.scrobble"));
        Assert.Equal("sk-bob", call["sk"]);
        Assert.Equal("Bladee", call["artist[0]"]);
        Assert.Equal("Be Nice 2 Me", call["track[0]"]);
        Assert.Equal("Icedancer", call["album[0]"]);
        Assert.Equal("154", call["duration[0]"]);
        Assert.Equal(playedAt.ToString(), call["timestamp[0]"]);
        Assert.Empty(fixture.Handler.LastFm.CallsTo("track.updateNowPlaying"));
    }

    [Fact]
    public async Task OutsideSong_StartOfPlay_IsNowPlaying()
    {
        await using var fixture = new RadioWebFactory(lastFmScrobbling: true);
        var id = RegisterOutsideSong(fixture);
        using var client = fixture.CreateClient();

        await client.GetStringAsync($"/rest/scrobble?u=bob&t=token&s=salt&f=json&id={id}&submission=false");
        await WhenIdle(fixture);

        var call = Assert.Single(fixture.Handler.LastFm.Calls);
        Assert.Equal("track.updateNowPlaying", call["method"]);
        Assert.Equal("Bladee", call["artist"]);
        Assert.Equal("Be Nice 2 Me", call["track"]);
        Assert.Equal("Icedancer", call["album"]);
        Assert.Equal("154", call["duration"]);
    }

    /// <summary>Navidrome scrobbles only a listener linked in its own settings, so Octo sends
    /// library plays too, and still relays them so Navidrome's play counts stay right.</summary>
    [Fact]
    public async Task LibrarySong_ReachesLastFm_AndIsStillRelayed()
    {
        await using var fixture = new RadioWebFactory(lastFmScrobbling: true);
        using var client = fixture.CreateClient();

        await client.GetStringAsync("/rest/scrobble?u=bob&t=token&s=salt&f=json&id=one&submission=false");
        await WhenIdle(fixture);
        var playing = Assert.Single(fixture.Handler.LastFm.CallsTo("track.updateNowPlaying"));
        Assert.Equal("Artist one", playing["artist"]);
        Assert.Equal("Title one", playing["track"]);

        var playedAt = DateTimeOffset.UtcNow.AddMinutes(-5).ToUnixTimeSeconds();
        await client.GetStringAsync($"/rest/scrobble?u=bob&t=token&s=salt&f=json&id=one&submission=true&time={playedAt}000");
        await WhenIdle(fixture);

        Assert.Equal(["one"], fixture.Handler.RelayedScrobbleIds);
        var call = Assert.Single(fixture.Handler.LastFm.CallsTo("track.scrobble"));
        Assert.Equal("Artist one", call["artist[0]"]);
        Assert.Equal("Title one", call["track[0]"]);
        Assert.Equal("Album one", call["album[0]"]);
        Assert.Equal(playedAt.ToString(), call["timestamp[0]"]);
    }

    /// <summary>Left to a Navidrome linked to Last.fm itself, a library play is not sent twice.</summary>
    [Fact]
    public async Task LibrarySong_LeftToNavidrome_NeverReachesLastFm()
    {
        await using var fixture = new RadioWebFactory(lastFmScrobbling: true, lastFmLibraryPlays: false);
        using var client = fixture.CreateClient();

        await client.GetStringAsync("/rest/scrobble?u=bob&t=token&s=salt&f=json&id=one&submission=false");
        await client.GetStringAsync("/rest/scrobble?u=bob&t=token&s=salt&f=json&id=one&submission=true");
        await WhenIdle(fixture);

        Assert.Equal(["one"], fixture.Handler.RelayedScrobbleIds);
        Assert.Empty(fixture.Handler.LastFm.Calls);
    }

    /// <summary>With no relay, a ping is the credential check. Failing it sends nothing, or anyone
    /// could scrobble to a listener's Last.fm by naming them.</summary>
    [Fact]
    public async Task WrongCredentials_SendNothing()
    {
        await using var fixture = new RadioWebFactory(lastFmScrobbling: true);
        var id = RegisterOutsideSong(fixture);
        using var client = fixture.CreateClient();

        // "bad" fails the fixture's ping; it is given a session so only the check stops it.
        var body = await client.GetStringAsync($"/rest/scrobble?u=bad&f=json&id={id}&submission=true");
        await WhenIdle(fixture);

        Assert.Contains("failed", body);
        Assert.Empty(fixture.Handler.LastFm.Calls);
    }

    [Fact]
    public async Task UserWithoutASession_SendsNothing()
    {
        await using var fixture = new RadioWebFactory(lastFmScrobbling: true);
        var id = RegisterOutsideSong(fixture);
        using var client = fixture.CreateClient();

        await client.GetStringAsync($"/rest/scrobble?u=alice&t=token&s=salt&f=json&id={id}&submission=false");
        await client.GetStringAsync($"/rest/scrobble?u=alice&t=token&s=salt&f=json&id={id}&submission=true");
        await WhenIdle(fixture);

        Assert.Empty(fixture.Handler.LastFm.Calls);
        // ListenBrainz still has alice's default token, so the play was not lost there.
        Assert.Single(fixture.Handler.ListenBrainzSubmissions);
    }

    [Fact]
    public async Task InvalidSession_DisconnectsAndStopsSending()
    {
        await using var fixture = new RadioWebFactory(lastFmScrobbling: true);
        var id = RegisterOutsideSong(fixture);
        using var client = fixture.CreateClient();
        fixture.Handler.LastFm.Failures.Enqueue(LastFmScrobbleService.ErrorInvalidSession);

        var service = fixture.Services.GetRequiredService<LastFmScrobbleService>();
        bool Refused() => service.Users([]).Single(user => user.User == "bob").Notice is not null;

        await client.GetStringAsync($"/rest/scrobble?u=bob&t=token&s=salt&f=json&id={id}&submission=true");
        await LastFmScrobbleServiceTests.Until(Refused);
        await client.GetStringAsync($"/rest/scrobble?u=bob&t=token&s=salt&f=json&id={id}&submission=true");
        await Task.Delay(100);

        Assert.Single(fixture.Handler.LastFm.Calls);
        Assert.False(service.Users([]).Single(user => user.User == "bob").Connected);
    }

    /// <summary>One submission for several ids is for all of them, as Navidrome reads it: two songs
    /// starting are two Now Playings, not a Now Playing and a finished play.</summary>
    [Fact]
    public async Task OneSubmissionFlag_AppliesToEveryId()
    {
        await using var fixture = new RadioWebFactory(lastFmScrobbling: true);
        var first = RegisterOutsideSong(fixture);
        var second = fixture.Services.GetRequiredService<ExternalIdRegistry>().Register(new SoulseekRouting
        {
            Kind = RoutingKind.Song, Artist = "Bladee", Title = "Hahaha", Album = "Icedancer", Duration = 160,
        });
        using var client = fixture.CreateClient();

        await client.GetStringAsync($"/rest/scrobble?u=bob&t=token&s=salt&f=json&id={first}&id={second}&submission=false");
        await WhenIdle(fixture);

        Assert.Equal(2, fixture.Handler.LastFm.CallsTo("track.updateNowPlaying").Count);
        Assert.Empty(fixture.Handler.LastFm.CallsTo("track.scrobble"));
        Assert.Empty(fixture.Handler.ListenBrainzSubmissions);
        Assert.Empty(fixture.State.GetUser("bob").Plays);
    }

    /// <summary>One time for two ids, one of them outside. Navidrome refuses times that do not pair
    /// with ids; relaying the lone time with the library id alone would have paired them wrongly.</summary>
    [Fact]
    public async Task TimesThatDoNotPairWithIds_AreNotRelayed()
    {
        // Library plays left to Navidrome, so the one play sent to Last.fm is the outside one.
        await using var fixture = new RadioWebFactory(lastFmScrobbling: true, lastFmLibraryPlays: false);
        var outside = RegisterOutsideSong(fixture);
        using var client = fixture.CreateClient();
        var stale = DateTimeOffset.UtcNow.AddDays(-3).ToUnixTimeMilliseconds();

        await client.GetStringAsync(
            $"/rest/scrobble?u=bob&t=token&s=salt&f=json&id={outside}&id=one&submission=true&time={stale}");
        await WhenIdle(fixture);

        Assert.Equal(["one"], fixture.Handler.RelayedScrobbleIds);
        Assert.Empty(fixture.Handler.RelayedScrobbleTimes);
        // Nor is it pinned on the outside song: the play is dated when it arrived.
        var sent = long.Parse(Assert.Single(fixture.Handler.LastFm.CallsTo("track.scrobble"))["timestamp[0]"]);
        Assert.True(sent > DateTimeOffset.UtcNow.AddMinutes(-5).ToUnixTimeSeconds());
    }

    /// <summary>A client that posts the same finished play twice played it once.</summary>
    [Fact]
    public async Task ARepeatedFinishedPlay_IsLearnedOnce()
    {
        await using var fixture = new RadioWebFactory(lastFmScrobbling: true);
        var id = RegisterOutsideSong(fixture);
        using var client = fixture.CreateClient();
        var at = DateTimeOffset.UtcNow.AddMinutes(-4).ToUnixTimeMilliseconds();

        for (var i = 0; i < 2; i++)
            await client.GetStringAsync($"/rest/scrobble?u=bob&t=token&s=salt&f=json&id={id}&submission=true");
        for (var i = 0; i < 2; i++)
            await client.GetStringAsync($"/rest/scrobble?u=bob&t=token&s=salt&f=json&id={id}&submission=true&time={at}");
        await WhenIdle(fixture);

        // Once without a time, once with one: two plays, not four.
        var plays = fixture.Handler.LastFm.CallsTo("track.scrobble")
            .Sum(call => call.Keys.Count(key => key.StartsWith("artist[", StringComparison.Ordinal)));
        Assert.Equal(2, plays);
        Assert.Equal(2, fixture.Handler.ListenBrainzSubmissions.Count);
    }

    /// <summary>The client's answer does not wait on Last.fm: a stalled call still gets an ok.</summary>
    [Fact]
    public async Task ScrobbleAnswer_DoesNotWaitForLastFm()
    {
        await using var fixture = new RadioWebFactory(lastFmScrobbling: true);
        var id = RegisterOutsideSong(fixture);
        using var client = fixture.CreateClient();
        var release = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        fixture.Handler.LastFm.Hold = _ => release.Task;

        var body = await client.GetStringAsync($"/rest/scrobble?u=bob&t=token&s=salt&f=json&id={id}&submission=true")
            .WaitAsync(TimeSpan.FromSeconds(5));

        Assert.Contains("\"status\":\"ok\"", body);
        release.SetResult();
        await WhenIdle(fixture);
        Assert.Single(fixture.Handler.LastFm.CallsTo("track.scrobble"));
    }
}

/// <summary>The dashboard's Connect, Finish and Disconnect, and what the admin API shows of them.</summary>
public sealed class LastFmScrobbleAdminTests
{
    [Theory]
    [InlineData("/api/admin/lastfm/scrobble/connect")]
    [InlineData("/api/admin/lastfm/scrobble/finish")]
    [InlineData("/api/admin/lastfm/scrobble/disconnect")]
    [InlineData("/api/admin/lastfm/scrobble/cancel")]
    [InlineData("/api/admin/lastfm/check")]
    public async Task Writes_WithoutTheAdminHeader_AreRefused(string url)
    {
        await using var factory = new ScrobbleAdminFactory();
        using var client = factory.CreateClient();

        using var response = await client.PostAsync(url, Json(new { user = "alice" }));

        Assert.Equal(HttpStatusCode.Forbidden, response.StatusCode);
        Assert.Empty(factory.LastFm.Calls);
    }

    [Fact]
    public async Task ConnectFinishDisconnect_LinksAndUnlinksAUser()
    {
        await using var factory = new ScrobbleAdminFactory();
        using var client = factory.AdminClient();

        // Connect: a signed auth.getToken, and the page to approve Octo on.
        using var connect = await client.PostAsync("/api/admin/lastfm/scrobble/connect", Json(new { user = "alice" }));
        connect.EnsureSuccessStatusCode();
        var url = JsonDocument.Parse(await connect.Content.ReadAsStringAsync()).RootElement.GetProperty("url").GetString();
        Assert.Equal($"https://www.last.fm/api/auth/?api_key={FakeLastFm.ApiKey}&token=tok-1", url);
        Assert.Single(factory.LastFm.CallsTo("auth.getToken"));

        // Finish before approving: Last.fm says not yet, and nothing is saved.
        using var early = await client.PostAsync("/api/admin/lastfm/scrobble/finish", Json(new { user = "alice" }));
        Assert.Equal(HttpStatusCode.Conflict, early.StatusCode);
        Assert.DoesNotContain("sk-alice", File.ReadAllText(factory.SettingsPath));

        factory.LastFm.Approved = true;
        using var finish = await client.PostAsync("/api/admin/lastfm/scrobble/finish", Json(new { user = "alice" }));
        finish.EnsureSuccessStatusCode();
        Assert.Equal("lfm-alice", JsonDocument.Parse(await finish.Content.ReadAsStringAsync())
            .RootElement.GetProperty("lastFmUser").GetString());
        Assert.Equal("tok-1", factory.LastFm.CallsTo("auth.getSession")[^1]["token"]);
        Assert.Contains("sk-alice", File.ReadAllText(factory.SettingsPath));

        // The dashboard shows who alice is on Last.fm; no admin read shows the key.
        var status = "";
        for (var attempt = 0; attempt < 300 && !status.Contains("\"connected\":true"); attempt++)
        {
            status = await Status(client);
            await Task.Delay(10);
        }
        Assert.Contains("\"connected\":true", status);
        Assert.Contains("lfm-alice", status);
        Assert.Contains("\"libraryPlays\":true", status);
        foreach (var read in new[] { "/api/admin/lastfm/scrobble", "/api/admin/settings", "/api/admin/raw-config" })
            Assert.DoesNotContain("sk-alice", await client.GetStringAsync(read));

        using var disconnect = await client.PostAsync("/api/admin/lastfm/scrobble/disconnect", Json(new { user = "alice" }));
        disconnect.EnsureSuccessStatusCode();
        Assert.DoesNotContain("sk-alice", File.ReadAllText(factory.SettingsPath));
        Assert.False(factory.Services.GetRequiredService<LastFmScrobbleService>().IsEnabledFor("alice"));
    }

    /// <summary>The page never sees a saved secret, only the placeholder; checking with it checks the
    /// stored one. A typed secret is checked as typed.</summary>
    [Fact]
    public async Task Check_WithThePlaceholder_ChecksTheStoredSecret()
    {
        await using var factory = new ScrobbleAdminFactory();
        using var client = factory.AdminClient();

        using var saved = await client.PostAsync("/api/admin/lastfm/check",
            Json(new { apiKey = FakeLastFm.ApiKey, apiSecret = AdminController.SecretPlaceholder }));
        using var typed = await client.PostAsync("/api/admin/lastfm/check",
            Json(new { apiKey = FakeLastFm.ApiKey, apiSecret = "typed-wrong" }));

        Assert.Equal("ok", JsonDocument.Parse(await saved.Content.ReadAsStringAsync()).RootElement.GetProperty("secret").GetString());
        Assert.Equal("invalid", JsonDocument.Parse(await typed.Content.ReadAsStringAsync()).RootElement.GetProperty("secret").GetString());
    }

    /// <summary>The Raw editor writes back what it was shown. The masked session and secret must come
    /// back as what is stored, not as the placeholder.</summary>
    [Fact]
    public async Task RawConfig_RoundTrip_KeepsTheSecretAndSessions()
    {
        await using var factory = new ScrobbleAdminFactory(
            """{ "LastFm": { "ApiSecret": "stored-secret", "UserSessions": { "alice": { "SessionKey": "sk-alice", "LastFmUser": "lfm-alice" } } } }""");
        using var client = factory.AdminClient();

        var shown = await client.GetStringAsync("/api/admin/raw-config");
        Assert.DoesNotContain("stored-secret", shown);
        Assert.DoesNotContain("sk-alice", shown);
        using var put = await client.PutAsync("/api/admin/raw-config", new StringContent(shown, Encoding.UTF8, "application/json"));
        put.EnsureSuccessStatusCode();

        var saved = JsonNode.Parse(File.ReadAllText(factory.SettingsPath))!["LastFm"]!;
        Assert.Equal("stored-secret", (string?)saved["ApiSecret"]);
        Assert.Equal("sk-alice", (string?)saved["UserSessions"]!["alice"]!["SessionKey"]);
    }

    /// <summary>A form save echoes the placeholder for a secret nobody touched. That keeps what is
    /// stored; it is never saved as the secret.</summary>
    [Fact]
    public async Task FormSave_WithThePlaceholder_KeepsTheStoredSecret()
    {
        await using var factory = new ScrobbleAdminFactory("""{ "LastFm": { "ApiSecret": "stored-secret" } }""");
        using var client = factory.AdminClient();

        using var save = await client.PostAsync("/api/admin/settings",
            Json(new { LastFm = new { ApiKey = FakeLastFm.ApiKey, ApiSecret = AdminController.SecretPlaceholder } }));
        save.EnsureSuccessStatusCode();

        Assert.DoesNotContain("stored-secret", await save.Content.ReadAsStringAsync());
        Assert.Equal("stored-secret", (string?)JsonNode.Parse(File.ReadAllText(factory.SettingsPath))!["LastFm"]!["ApiSecret"]);
    }

    /// <summary>The Soulseek page's Connection form sends the slskd sign-in back as it was shown.
    /// The placeholder keeps the stored password; with a new username it is refused, because the
    /// old password under a new name is a pairing nobody typed.</summary>
    [Fact]
    public async Task FormSave_WithTheSlskdPlaceholder_KeepsTheStoredPassword()
    {
        const string stored = """{ "Soulseek": { "Username": "octo", "Password": "slskd-secret" } }""";
        await using var factory = new ScrobbleAdminFactory(stored);
        using var client = factory.AdminClient();

        using var save = await client.PostAsync("/api/admin/settings",
            Json(new { Soulseek = new { Username = "octo", Password = AdminController.SecretPlaceholder } }));
        save.EnsureSuccessStatusCode();
        Assert.DoesNotContain("slskd-secret", await save.Content.ReadAsStringAsync());
        Assert.Equal("slskd-secret", (string?)JsonNode.Parse(File.ReadAllText(factory.SettingsPath))!["Soulseek"]!["Password"]);

        using var renamed = await client.PostAsync("/api/admin/settings",
            Json(new { Soulseek = new { Username = "someone", Password = AdminController.SecretPlaceholder } }));
        Assert.Equal(HttpStatusCode.BadRequest, renamed.StatusCode);

        using var appended = await client.PostAsync("/api/admin/settings",
            Json(new { Soulseek = new { Password = AdminController.SecretPlaceholder + "x" } }));
        Assert.Equal(HttpStatusCode.BadRequest, appended.StatusCode);
        Assert.Equal("slskd-secret", (string?)JsonNode.Parse(File.ReadAllText(factory.SettingsPath))!["Soulseek"]!["Password"]);
    }

    /// <summary>A session key typed onto the end of the placeholder is refused, like the secret is:
    /// saved, it would be a key Last.fm rejects, and the listener would be cut off for it.</summary>
    [Fact]
    public async Task RawConfig_WithASessionKeyTypedOntoThePlaceholder_IsRefused()
    {
        const string stored = """{ "LastFm": { "UserSessions": { "alice": { "SessionKey": "sk-alice", "LastFmUser": "lfm-alice" } } } }""";
        await using var factory = new ScrobbleAdminFactory(stored);
        using var client = factory.AdminClient();

        using var put = await client.PutAsync("/api/admin/raw-config", new StringContent(
            $$"""{ "LastFm": { "UserSessions": { "alice": { "SessionKey": "{{AdminController.SecretPlaceholder}}x" } } } }""",
            Encoding.UTF8, "application/json"));

        Assert.Equal(HttpStatusCode.BadRequest, put.StatusCode);
        Assert.Contains("alice", await put.Content.ReadAsStringAsync());
        Assert.Equal(stored, File.ReadAllText(factory.SettingsPath));
    }

    /// <summary>Last.fm handed over the session but settings.json could not be written. The admin
    /// hears that plainly, as a conflict, rather than as a server error.</summary>
    [Fact]
    public async Task Finish_WhenTheSettingsFileCannotBeWritten_SaysSo()
    {
        await using var factory = new ScrobbleAdminFactory();
        using var client = factory.AdminClient();
        using var connect = await client.PostAsync("/api/admin/lastfm/scrobble/connect", Json(new { user = "alice" }));
        connect.EnsureSuccessStatusCode();
        factory.LastFm.Approved = true;

        HttpResponseMessage finish;
        using (new FileStream(factory.SettingsPath, FileMode.Open, FileAccess.ReadWrite, FileShare.None))
            finish = await client.PostAsync("/api/admin/lastfm/scrobble/finish", Json(new { user = "alice" }));

        using (finish)
        {
            Assert.Equal(HttpStatusCode.Conflict, finish.StatusCode);
            Assert.Contains("could not be saved", await finish.Content.ReadAsStringAsync());
        }
        Assert.DoesNotContain("sk-alice", File.ReadAllText(factory.SettingsPath));
    }

    private static Task<string> Status(HttpClient client) => client.GetStringAsync("/api/admin/lastfm/scrobble");

    private static StringContent Json(object value) =>
        new(JsonSerializer.Serialize(value), Encoding.UTF8, "application/json");
}

/// <summary>Octo with its settings file in a temporary folder, read back as configuration the way
/// /app/config/settings.json is, and Last.fm faked.</summary>
internal sealed class ScrobbleAdminFactory : WebApplicationFactory<Program>
{
    private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-lastfm-admin-" + Guid.NewGuid());
    public FakeLastFm LastFm { get; } = new();
    public string SettingsPath => Path.Combine(_directory, "settings.json");

    public ScrobbleAdminFactory(string settings = "{}")
    {
        Directory.CreateDirectory(_directory);
        File.WriteAllText(SettingsPath, settings);
    }

    public HttpClient AdminClient()
    {
        var client = CreateClient();
        client.DefaultRequestHeaders.Add(AdminRequestGuard.HeaderName, "1");
        return client;
    }

    protected override void ConfigureWebHost(IWebHostBuilder builder)
    {
        builder.ConfigureAppConfiguration((_, configuration) =>
        {
            configuration.AddInMemoryCollection(new Dictionary<string, string?>
            {
                ["Subsonic:Url"] = "http://127.0.0.1:1",
                ["Subsonic:AutoDetectDownloadPath"] = "false",
                ["Soulseek:BaseUrl"] = "http://127.0.0.1:1",
                ["YouTube:ShimUrl"] = "http://127.0.0.1:1",
                ["Library:DownloadPath"] = _directory,
                ["Octo:StateDirectory"] = _directory,
                ["LastFm:ApiKey"] = FakeLastFm.ApiKey,
                ["LastFm:ApiSecret"] = FakeLastFm.Secret,
            });
            configuration.AddJsonFile(SettingsPath, optional: true, reloadOnChange: true);
        });
        builder.ConfigureServices(services =>
        {
            services.RemoveAll<IHostedService>();
            services.RemoveAll<IHttpClientFactory>();
            services.AddSingleton<IHttpClientFactory>(new ReviewFixtures.OneClientFactory(LastFm));
            services.RemoveAll<SettingsFileWriter>();
            services.AddSingleton(new SettingsFileWriter(SettingsPath));
        });
    }

    public override async ValueTask DisposeAsync()
    {
        await base.DisposeAsync();
        try { Directory.Delete(_directory, true); } catch { }
    }
}

/// <summary>
/// Last.fm's web service as far as Octo uses it. Refuses a wrong signature with error 13 as the
/// real one does, so every call a test sees was signed correctly.
/// </summary>
internal sealed class FakeLastFm : HttpMessageHandler
{
    public const string ApiKey = "0123456789abcdef0123456789abcdef";
    public const string Secret = "s3cr3t";

    private readonly object _lock = new();
    private readonly List<Dictionary<string, string>> _calls = [];
    private readonly List<DateTime> _times = [];

    /// <summary>What the next calls fail with, in order: a Last.fm error code, or 0 for HTTP 503.</summary>
    public System.Collections.Concurrent.ConcurrentQueue<int> Failures { get; } = new();

    /// <summary>Whether the admin has approved Octo on last.fm yet.</summary>
    public bool Approved { get; set; }

    /// <summary>Holds a call open until the returned task completes.</summary>
    public Func<Dictionary<string, string>, Task>? Hold { get; set; }

    public IReadOnlyList<Dictionary<string, string>> Calls { get { lock (_lock) return _calls.ToList(); } }

    /// <summary>When each call arrived, on the real clock.</summary>
    public IReadOnlyList<DateTime> CallTimes { get { lock (_lock) return _times.ToList(); } }

    public IReadOnlyList<Dictionary<string, string>> CallsTo(string method) =>
        Calls.Where(call => call.GetValueOrDefault("method") == method).ToList();

    protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken) =>
        RespondAsync(request, cancellationToken);

    public async Task<HttpResponseMessage> RespondAsync(HttpRequestMessage request, CancellationToken cancellationToken)
    {
        if (!request.RequestUri!.Host.Equals("ws.audioscrobbler.com", StringComparison.OrdinalIgnoreCase))
            return new HttpResponseMessage(HttpStatusCode.NotFound);
        var body = request.Content is null ? "" : await request.Content.ReadAsStringAsync(cancellationToken);
        var form = System.Web.HttpUtility.ParseQueryString(body);
        var call = form.AllKeys.Where(key => key is not null).ToDictionary(key => key!, key => form[key] ?? "");
        lock (_lock)
        {
            _calls.Add(call);
            _times.Add(DateTime.UtcNow);
        }
        if (Hold is { } hold) await hold(call);

        if (call.GetValueOrDefault("api_sig") != LastFmScrobbleService.Sign(call, Secret))
            return Error(13, "Invalid method signature supplied");
        if (Failures.TryDequeue(out var failure))
            return failure == 0 ? new HttpResponseMessage(HttpStatusCode.ServiceUnavailable) : Error(failure, "Refused by the fixture");

        return call.GetValueOrDefault("method") switch
        {
            "auth.getToken" => Ok("""{"token":"tok-1"}"""),
            "auth.getSession" => Approved
                ? Ok("""{"session":{"name":"lfm-alice","key":"sk-alice","subscriber":0}}""")
                : Error(14, "Unauthorized Token - This token has not been authorized"),
            "track.scrobble" => Ok(JsonSerializer.Serialize(new Dictionary<string, object>
            {
                ["scrobbles"] = new Dictionary<string, object>
                {
                    ["@attr"] = new { accepted = call.Keys.Count(key => key.StartsWith("artist[", StringComparison.Ordinal)), ignored = 0 },
                    ["scrobble"] = Array.Empty<object>(),
                },
            })),
            "track.updateNowPlaying" => Ok("""{"nowplaying":{"ignoredMessage":{"code":"0","#text":""}}}"""),
            _ => Error(3, "Invalid Method - No method with that name in this package"),
        };
    }

    private static HttpResponseMessage Ok(string json) =>
        new(HttpStatusCode.OK) { Content = new StringContent(json, Encoding.UTF8, "application/json") };

    // Last.fm answers a refusal with a 4xx and the error in the body.
    private static HttpResponseMessage Error(int code, string message) =>
        new(HttpStatusCode.Forbidden)
        {
            Content = new StringContent(JsonSerializer.Serialize(new { error = code, message }), Encoding.UTF8, "application/json"),
        };
}
