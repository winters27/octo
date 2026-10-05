using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.Soulseek;
using Octo.Services.Subsonic;

namespace Octo.Services.Library;

public sealed record LibrarySongRow(string Id, string Path, string? LibraryPath, long Size, string Suffix,
    int BitRate, string Title, string Artist, int? Duration, string? Album = null, string? AlbumId = null,
    string? AlbumArtist = null, IReadOnlyList<string>? Isrcs = null);

public sealed record QualityUpgradeAttempt(DateTime AtUtc, string Outcome, string? Detail);

public sealed class QualityUpgradeState
{
    public DateTime? LastRunUtc { get; set; }
    public string? LastOutcome { get; set; }
    public Dictionary<string, QualityUpgradeAttempt> Attempts { get; set; } = [];
}

public sealed record QualityUpgradeStatus(int PerWeek, string? Off, DateTime? LastRunUtc, string? LastOutcome,
    DateTime? NextDueUtc, int Tried);

/// <summary>
/// What the weekly upgrade has tried, by file. Written straight away on every change: there is
/// about one a day, and a restart must not forget a run and start a second one early.
/// </summary>
public sealed class QualityUpgradeStore
{
    private readonly string? _path;
    private readonly ILogger<QualityUpgradeStore>? _logger;
    private readonly object _lock = new();
    private QualityUpgradeState _state = new();

    public QualityUpgradeStore(string? path = null, ILogger<QualityUpgradeStore>? logger = null)
    {
        _path = string.IsNullOrWhiteSpace(path) ? null : path;
        _logger = logger;
        try
        {
            if (_path is not null && File.Exists(_path))
                _state = JsonSerializer.Deserialize<QualityUpgradeState>(File.ReadAllText(_path)) ?? new();
        }
        catch (Exception ex) { _logger?.LogWarning("quality upgrade state could not be read: {M}", ex.Message); }
    }

    public QualityUpgradeState Snapshot()
    {
        lock (_lock)
            return new() { LastRunUtc = _state.LastRunUtc, LastOutcome = _state.LastOutcome, Attempts = new(_state.Attempts) };
    }

    public void Update(Action<QualityUpgradeState> change)
    {
        string json;
        lock (_lock) { change(_state); json = JsonSerializer.Serialize(_state); }
        if (_path is null) return;
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(_path)!);
            File.WriteAllText(_path + ".tmp", json);
            File.Move(_path + ".tmp", _path, overwrite: true);
        }
        catch (Exception ex) { _logger?.LogWarning("quality upgrade state could not be written: {M}", ex.Message); }
    }
}

/// <summary>
/// Trickles lossy songs through Better quality, a few a week (#70). Every safety the action has
/// applies unchanged, because this only ever calls it: the allowlist, dry run, the quarantine,
/// and putting the original back when the new file is not really better, and the upgraded song
/// keeps its place in Navidrome, with its plays, favorites and playlist entries (W8).
/// </summary>
public sealed class QualityUpgradeWorker : BackgroundService
{
    internal const int PageSize = 1000;
    internal const int MaxPages = 200;
    /// <summary>A song that found nothing is not looked for again for four weeks, so a small
    /// library does not search for the same missing song every few hours.</summary>
    internal static readonly TimeSpan RetryAfter = TimeSpan.FromDays(28);
    private static readonly TimeSpan FirstCheck = TimeSpan.FromMinutes(5);
    private static readonly TimeSpan CheckInterval = TimeSpan.FromMinutes(1);
    private static readonly TimeSpan UnreachableWarningEvery = TimeSpan.FromHours(1);

    private readonly QualityUpgradeStore _store;
    private readonly IOptionsMonitor<LibraryActionSettings> _settings;
    private readonly ILogger<QualityUpgradeWorker> _logger;
    private DateTime? _lastUnreachableWarning;
    private readonly UpgradeQueue? _upgrades;

    public QualityUpgradeWorker(QualityUpgradeStore store, LibraryActionExecutor executor,
        IAcquisitionActivity activity, NavidromePlaylistApi navidrome, NavidromeIdentityService identity,
        IOptionsMonitor<LibraryActionSettings> settings, ILogger<QualityUpgradeWorker> logger,
        ISoulseekLink? soulseek = null, UpgradeQueue? upgrades = null, UpgradeSources? sources = null)
    {
        _upgrades = upgrades;
        SourceReady = () => sources?.Ready ?? true;
        _store = store;
        _settings = settings;
        _logger = logger;
        AcquisitionsIdle = () => !activity.IsBusy;
        HasAdminIdentity = () => identity.HasAdminIdentity;
        Apply = (request, ct) => executor.ApplyAsync(request, ct);
        ListSongs = ct => WalkAsync(navidrome, ct);
        // Out only when every source is: a Soulseek outage leaves Lidarr when it is set up.
        SoulseekOffline = async ct => sources is not null ? await sources.WaitingForSoulseekAsync(ct)
            : soulseek is not null && (await soulseek.ReadAsync(fresh: false, ct))?.Link == SoulseekLinkState.NotLoggedIn;
    }

    // Seams, the same way SoulseekClient exposes Clock and PollInterval.
    internal Func<DateTime> Clock { get; set; } = () => DateTime.UtcNow;
    internal Func<bool> AcquisitionsIdle { get; set; }
    internal Func<bool> HasAdminIdentity { get; set; }
    internal Func<LibraryActionRequest, CancellationToken, Task<LibraryActionOutcome>> Apply { get; set; }
    internal Func<CancellationToken, Task<(IReadOnlyList<LibrarySongRow> Songs, bool Complete)>> ListSongs { get; set; }
    internal Func<CancellationToken, Task<bool>> SoulseekOffline { get; set; }
    internal Func<bool> SourceReady { get; set; }

    internal enum Tick { Off, NotDue, Busy, Offline, Unreachable, NothingToDo, Ran }

    public static TimeSpan? Interval(int perWeek) =>
        perWeek <= 0 ? null : TimeSpan.FromTicks(TimeSpan.FromDays(7).Ticks / Math.Clamp(perWeek, 1, 500));

    internal static string? ActingUser(LibraryActionSettings s) =>
        (s.AllowedUsers ?? []).Select(u => u?.Trim()).FirstOrDefault(u => !string.IsNullOrEmpty(u));

    internal static string? WhyOff(LibraryActionSettings s, bool hasAdmin, bool sourceReady = true) =>
        s.EffectiveUpgradePerWeek <= 0 ? "Off."
        // Every try would fail and be stamped as tried for four weeks.
        : !sourceReady ? "Better quality has no source set up: it needs slskd or Lidarr."
        : !s.Enabled ? "Library actions are off."
        : !s.EffectiveActions().Any(a => a.Action == LibraryAction.BetterQuality && a.Enabled) ? "Better quality is not switched on."
        : ActingUser(s) is null ? "Nobody is on the allowed list."
        : !hasAdmin ? "Octo needs a Navidrome admin credential to read the library."
        : null;

    /// <summary>
    /// The library path and size, never the Navidrome id: ids change when a file moves, and
    /// Navidrome 0.64 changed all of them. A failed upgrade puts the original back unchanged, so
    /// its key holds. A successful one turns it into a FLAC, which is never picked.
    /// </summary>
    internal static string KeyOf(LibrarySongRow song)
    {
        var path = song.Path.Replace('\\', '/');
        var root = (song.LibraryPath ?? "").Replace('\\', '/').TrimEnd('/');
        if (root.Length > 0 && path.StartsWith(root + "/", StringComparison.Ordinal)) path = path[(root.Length + 1)..];
        return $"{path.TrimStart('/')}|{song.Size}";
    }

    /// <summary>Never tried first, then the longest ago. A rehearsal counts as a try only while
    /// dry run is on, so turning dry run off starts again from the top.</summary>
    internal static LibrarySongRow? Pick(IEnumerable<LibrarySongRow> songs, QualityUpgradeState state, DateTime now, bool dryRun) =>
        songs.Where(song => !DuplicateScanWorker.IsLosslessFile(song.Suffix, song.BitRate))
            .Select(song => (song, tried: state.Attempts.TryGetValue(KeyOf(song), out var a)
                && (dryRun || a.Outcome != nameof(LibraryActionState.Rehearsed)) ? a : null))
            .Where(x => x.tried is null || now - x.tried.AtUtc >= RetryAfter)
            .OrderBy(x => x.tried is not null)
            .ThenBy(x => x.tried?.AtUtc ?? DateTime.MinValue)
            .ThenBy(x => KeyOf(x.song), StringComparer.Ordinal)
            .Select(x => x.song)
            .FirstOrDefault();

    internal async Task<Tick> TickAsync(CancellationToken ct)
    {
        var settings = _settings.CurrentValue;
        if (WhyOff(settings, HasAdminIdentity(), SourceReady()) is not null) return Tick.Off;
        var now = Clock();
        var state = _store.Snapshot();
        if (state.LastRunUtc is { } last && now - last < Interval(settings.EffectiveUpgradePerWeek)!.Value) return Tick.NotDue;
        // Never queue ahead of a person: a heart, star or play in flight means try again next minute.
        if (!AcquisitionsIdle()) return Tick.Busy;
        // Songs someone asked to upgrade go first, and the weekly run never competes with them.
        if (_upgrades?.OpenCount > 0) return Tick.Busy;
        // An upgrade during a Soulseek outage finds nothing and would not look at that song again
        // for four weeks. Not stamped, so the run happens once slskd is back.
        if (await SoulseekOffline(ct)) return Tick.Offline;

        var (songs, complete) = await ListSongs(ct);
        if (!complete && songs.Count == 0)
        {
            // Navidrome did not answer. Stamping the run would spend the week's slot on a
            // library nobody could read, so it is tried again next minute instead.
            if (_lastUnreachableWarning is not { } warned || now - warned >= UnreachableWarningEvery)
            {
                _lastUnreachableWarning = now;
                _logger.LogWarning("Quality upgrade: Navidrome did not list the library; trying again every minute");
            }
            else _logger.LogDebug("Quality upgrade: Navidrome still did not list the library");
            return Tick.Unreachable;
        }
        var pick = Pick(songs, state, now, settings.DryRun);
        _store.Update(s =>
        {
            // Stamped before the attempt, so a crash or restart mid-download cannot cause a burst.
            s.LastRunUtc = now;
            if (complete)
            {
                var present = songs.Select(KeyOf).ToHashSet(StringComparer.Ordinal);
                foreach (var gone in s.Attempts.Keys.Where(k => !present.Contains(k)).ToList()) s.Attempts.Remove(gone);
            }
            if (pick is null) s.LastOutcome = "Nothing";
        });
        if (pick is null) return Tick.NothingToDo;

        _logger.LogInformation("Quality upgrade: trying '{Artist} - {Title}' ({Suffix})", pick.Artist, pick.Title, pick.Suffix);
        LibraryActionOutcome outcome;
        try { outcome = await Apply(new LibraryActionRequest(LibraryAction.BetterQuality, pick.Id, ActingUser(settings)!), ct); }
        catch (Exception ex) when (ex is not OperationCanceledException) { outcome = new(LibraryActionState.Failed, ex.Message); }

        var key = KeyOf(pick);
        _store.Update(s =>
        {
            s.Attempts[key] = new(Clock(), outcome.State.ToString(), outcome.Detail);
            s.LastOutcome = outcome.State.ToString();
        });
        return Tick.Ran;
    }

    /// <summary>What the weekly run has tried, by file, for the Better quality page.</summary>
    internal QualityUpgradeState Tried() => _store.Snapshot();

    public QualityUpgradeStatus Status()
    {
        var settings = _settings.CurrentValue;
        var state = _store.Snapshot();
        var off = WhyOff(settings, HasAdminIdentity(), SourceReady());
        DateTime? next = off is null && Interval(settings.EffectiveUpgradePerWeek) is { } interval
            ? (state.LastRunUtc is { } last ? last + interval : Clock())
            : null;
        return new(settings.EffectiveUpgradePerWeek, off, state.LastRunUtc, state.LastOutcome, next, state.Attempts.Count);
    }

    protected override async Task ExecuteAsync(CancellationToken stoppingToken)
    {
        var wait = FirstCheck;
        while (!stoppingToken.IsCancellationRequested)
        {
            try { await Task.Delay(wait, stoppingToken); } catch (OperationCanceledException) { break; }
            wait = CheckInterval;
            // Per-tick catch: BackgroundServiceExceptionBehavior defaults to StopHost.
            try { await TickAsync(stoppingToken); }
            catch (OperationCanceledException) when (stoppingToken.IsCancellationRequested) { break; }
            catch (Exception ex) { _logger.LogError(ex, "Quality upgrade tick failed"); }
        }
    }

    private static async Task<(IReadOnlyList<LibrarySongRow>, bool)> WalkAsync(NavidromePlaylistApi api, CancellationToken ct)
    {
        var all = new List<LibrarySongRow>();
        for (var page = 0; page < MaxPages; page++)
        {
            if (await api.ListSongsAsync(page * PageSize, PageSize, ct) is not { } result) return (all, false);
            all.AddRange(result.Rows);
            if (result.Count < PageSize) return (all, true);
        }
        return (all, false);
    }

    internal static (IReadOnlyList<LibrarySongRow> Rows, int Count)? ParseSongs(JsonElement root)
    {
        if (root.ValueKind != JsonValueKind.Array) return null;
        var rows = new List<LibrarySongRow>();
        var count = 0;
        foreach (var song in root.EnumerateArray())
        {
            count++;
            if (song.TryGetProperty("missing", out var m) && m.ValueKind == JsonValueKind.True) continue;
            var id = Str(song, "id");
            var path = Str(song, "path");
            if (string.IsNullOrEmpty(id) || string.IsNullOrEmpty(path)) continue;
            rows.Add(new LibrarySongRow(id, path, Str(song, "libraryPath"),
                song.TryGetProperty("size", out var sz) && sz.TryGetInt64(out var bytes) ? bytes : 0,
                Str(song, "suffix") ?? "",
                song.TryGetProperty("bitRate", out var br) && br.TryGetInt32(out var rate) ? rate : 0,
                Str(song, "title") ?? "", Str(song, "artist") ?? "",
                // A float in Navidrome's native API, unlike Subsonic's whole seconds.
                song.TryGetProperty("duration", out var d) && d.ValueKind == JsonValueKind.Number
                    ? (int)Math.Round(d.GetDouble()) : null,
                Str(song, "album"), Str(song, "albumId"), Str(song, "albumArtist"), IsrcsOf(song)));
        }
        return (rows, count);
    }

    private static string? Str(JsonElement e, string name) =>
        e.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() : null;

    /// <summary>A song's ISRCs: Navidrome files them among its tags, as a list; one written as
    /// a single string is read as a list of one.</summary>
    private static IReadOnlyList<string> IsrcsOf(JsonElement song)
    {
        var found = new List<string>();
        void Read(JsonElement value)
        {
            if (value.ValueKind == JsonValueKind.String && value.GetString() is { Length: > 0 } one) found.Add(one);
            else if (value.ValueKind == JsonValueKind.Array)
                foreach (var item in value.EnumerateArray()) Read(item);
        }
        if (song.TryGetProperty("tags", out var tags) && tags.ValueKind == JsonValueKind.Object
            && tags.TryGetProperty("isrc", out var tagged)) Read(tagged);
        if (song.TryGetProperty("isrc", out var direct)) Read(direct);
        return found;
    }
}
