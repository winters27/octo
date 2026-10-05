using System.Globalization;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.Extensions.Caching.Memory;
using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Radio;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.LastFm;
using Octo.Services.Metadata;
using Octo.Services.Subsonic;

namespace Octo.Services.Library;

/// <summary>One mix or Made for you list, as one listener sees it for the current draw.</summary>
/// <param name="Kind">"genre", "decade", or a Made for you kind ("newReleases", "rediscover", "deepCuts").</param>
/// <param name="Key">"genre:Rock", "decade:1990" or "for:newReleases".</param>
public sealed record GeneratedPlaylist(string Id, string Key, string Kind, string Label, string Name,
    string Owner, int PoolSize, DateTime PeriodStartUtc, DateTime PeriodEndUtc);

/// <summary>
/// Genre and decade mixes built from the listener's own library and served like radio stations
/// (#54): per listener, read-only, under ids that start "og" so they can never be mistaken for
/// a Navidrome playlist. Nothing is written to Navidrome.
///
/// Which mixes exist is decided from counts refreshed every RefreshHours, with hysteresis so a
/// genre sitting at the threshold does not appear and vanish on alternate days. What is in a mix
/// is a seeded draw per listener and period, so it holds still for the period and every client
/// sees the same tracks, then changes.
///
/// The Made for you lists (New Releases, Rediscover, Deep Cuts) live here too, on by default and
/// switched apart from the mixes: built in the same refresh, from one walk of the listener's
/// library as they see it, and kept on disk, so opening one never waits on a build.
/// </summary>
public sealed class GeneratedPlaylistService
{
    internal const int PoolPage = 500;
    internal const int FirstDecade = 1950;
    private const int MaxGenrePages = 4;
    private const int BlendCandidates = 200;
    private static readonly TimeSpan FirstListWait = TimeSpan.FromSeconds(1.5);
    private static readonly TimeSpan RefreshDeadline = TimeSpan.FromMinutes(2);
    // New Releases asks the catalog for up to NewReleaseArtists artists on its background lane.
    private static readonly TimeSpan ForYouRefreshDeadline = TimeSpan.FromMinutes(6);
    // A library bigger than this is walked this far: 40,000 songs.
    internal const int MaxWalkPages = 80;
    private const string ForYouPrefix = "for:";
    private static readonly TimeSpan RetryAfterFailure = TimeSpan.FromMinutes(5);
    private static readonly JsonSerializerOptions Json = new() { WriteIndented = false };

    private readonly string? _statePath;
    private readonly IServiceScopeFactory _scopes;
    private readonly IOptionsMonitor<GeneratedPlaylistSettings> _settings;
    private readonly IOptionsMonitor<GenreSettings> _genre;
    private readonly ILogger<GeneratedPlaylistService> _logger;
    private readonly SingleFlight<string, bool> _refreshes = new();
    private readonly MemoryCache _drawn = new(new MemoryCacheOptions { SizeLimit = 256 });
    private readonly object _lock = new();
    private readonly Dictionary<string, UserMixes> _users = new(StringComparer.Ordinal);
    private readonly Dictionary<string, DateTime> _lastAttempt = new(StringComparer.Ordinal);

    public GeneratedPlaylistService(string? statePath, IServiceScopeFactory scopes,
        IOptionsMonitor<GeneratedPlaylistSettings> settings, IOptionsMonitor<GenreSettings> genre,
        ILogger<GeneratedPlaylistService> logger)
    {
        _statePath = string.IsNullOrWhiteSpace(statePath) ? null : statePath;
        _scopes = scopes;
        _settings = settings;
        _genre = genre;
        _logger = logger;
        Load();
    }

    /// <summary>What decides which mixes a listener has, persisted so a restart keeps them.</summary>
    internal sealed class UserMixes
    {
        public List<string> Active { get; set; } = [];
        public Dictionary<string, int> Counts { get; set; } = new(StringComparer.Ordinal);
        public DateTime CountsUtc { get; set; }
        public string Kinds { get; set; } = "";

        /// <summary>Each Made for you list's songs as Subsonic song JSON, by kind.</summary>
        public Dictionary<string, List<string>> ForYou { get; set; } = new(StringComparer.Ordinal);

        /// <summary>Album covers New Releases' cover is coloured from: outside albums Navidrome
        /// does not know, so they are fetched from the catalog's own image host.</summary>
        public List<string> ForYouCovers { get; set; } = [];
    }

    private sealed class StateDocument
    {
        public Dictionary<string, UserMixes> Users { get; set; } = new(StringComparer.Ordinal);
    }

    /// <summary>
    /// The listener's mixes, refreshing the counts behind them when they are stale. The first list
    /// ever waits a moment for them, so mixes show on a listener's first look rather than their
    /// second; later refreshes run behind the answer.
    /// </summary>
    public async Task<IReadOnlyList<GeneratedPlaylist>> ListAsync(string username, IReadOnlyDictionary<string, string> auth)
    {
        var settings = _settings.CurrentValue;
        if (string.IsNullOrWhiteSpace(username) || !(MixesOn(settings) || settings.AnyForYou)) return [];

        var user = UserKey(username);
        var now = DateTime.UtcNow;
        UserMixes? state;
        bool due;
        lock (_lock)
        {
            _users.TryGetValue(user, out state);
            due = (state is null || IsStale(state, settings, now))
                  && (!_lastAttempt.TryGetValue(user, out var attempted) || now - attempted >= RetryAfterFailure);
            if (due) _lastAttempt[user] = now;
        }

        if (due)
        {
            var copy = auth.ToDictionary(pair => pair.Key, pair => pair.Value);
            var refresh = _refreshes.RunAsync(user, ct => RefreshAsync(username, copy, ct),
                settings.NewReleases ? ForYouRefreshDeadline : RefreshDeadline);
            _ = refresh.ContinueWith(task => _logger.LogDebug(task.Exception, "Mix refresh failed for {User}", username),
                CancellationToken.None, TaskContinuationOptions.OnlyOnFaulted, TaskScheduler.Default);
            if (state is null) await Task.WhenAny(refresh, Task.Delay(FirstListWait));
        }
        return Current(username, settings, DateTime.UtcNow);
    }

    /// <summary>A mix by id, for this listener, as of now. Null for anyone else's id.</summary>
    public GeneratedPlaylist? Find(string username, string id)
    {
        var settings = _settings.CurrentValue;
        if (string.IsNullOrEmpty(id) || !id.StartsWith("og", StringComparison.Ordinal)) return null;
        return Current(username, settings, DateTime.UtcNow).FirstOrDefault(mix => mix.Id == id);
    }

    private IReadOnlyList<GeneratedPlaylist> Current(string username, GeneratedPlaylistSettings settings, DateTime nowUtc)
    {
        var user = UserKey(username);
        var hours = settings.EffectiveRefreshHours;
        var period = PeriodIndex(nowUtc, hours);
        var start = DateTime.UnixEpoch.AddHours(period * hours);
        var end = start.AddHours(hours);

        lock (_lock)
        {
            if (!_users.TryGetValue(user, out var state)) return [];
            // Made for you first, newest to oldest, and only a list with something in it.
            var forYou = ForYouLists.Kinds
                .Where(kind => ForYouOn(settings, kind) && state.ForYou.TryGetValue(kind, out var songs) && songs.Count > 0)
                .Select(kind => new GeneratedPlaylist(PlaylistId(username, ForYouPrefix + kind), ForYouPrefix + kind, kind,
                    ForYouLists.Name(kind), ForYouLists.Name(kind), username.Trim(), state.ForYou[kind].Count, start, end));
            IEnumerable<GeneratedPlaylist> mixes = !MixesOn(settings) ? [] : state.Active
                .Where(key => (settings.Genres && key.StartsWith("genre:", StringComparison.Ordinal))
                    || (settings.Decades && key.StartsWith("decade:", StringComparison.Ordinal)))
                .Select(key =>
                {
                    var (kind, label) = Describe(key);
                    return new GeneratedPlaylist(PlaylistId(username, key), key, kind, label, settings.Name(label),
                        username.Trim(), state.Counts.GetValueOrDefault(key), start, end);
                });
            return forYou.Concat(mixes).ToList();
        }
    }

    private static bool MixesOn(GeneratedPlaylistSettings settings) =>
        settings.Enabled && (settings.Genres || settings.Decades);

    private static bool ForYouOn(GeneratedPlaylistSettings settings, string kind) => kind switch
    {
        ForYouLists.NewReleasesKind => settings.NewReleases,
        ForYouLists.RediscoverKind => settings.Rediscover,
        ForYouLists.DeepCutsKind => settings.DeepCuts,
        _ => false,
    };

    /// <summary>The album covers New Releases' own cover is coloured from, up to four.</summary>
    public IReadOnlyList<string> ForYouCovers(string username)
    {
        lock (_lock)
            return _users.TryGetValue(UserKey(username), out var state) ? state.ForYouCovers.Take(4).ToList() : [];
    }

    private static (string Kind, string Label) Describe(string key) =>
        key.StartsWith("decade:", StringComparison.Ordinal)
            ? ("decade", key["decade:".Length..] + "s")
            : ("genre", key["genre:".Length..]);

    private static string KindsOf(GeneratedPlaylistSettings settings) =>
        (MixesOn(settings) && settings.Genres ? "genre" : "") + "," + (MixesOn(settings) && settings.Decades ? "decade" : "")
        + "," + string.Join("+", ForYouLists.Kinds.Where(kind => ForYouOn(settings, kind)))
        + (settings.NewReleases ? $",w{settings.EffectiveNewReleaseWeeks}a{settings.EffectiveNewReleaseArtists}" : "")
        + (settings.Rediscover ? $",m{settings.EffectiveRediscoverMonths}" : "");

    private static bool IsStale(UserMixes state, GeneratedPlaylistSettings settings, DateTime nowUtc) =>
        nowUtc - state.CountsUtc >= TimeSpan.FromHours(settings.EffectiveRefreshHours)
        || state.Kinds != KindsOf(settings);

    /// <summary>
    /// Count what the listener's library holds per genre and decade, and decide which mixes they
    /// have. Only a complete count replaces the last one: a Navidrome that stops answering half
    /// way must not make every mix vanish.
    /// </summary>
    private async Task<bool> RefreshAsync(string username, IReadOnlyDictionary<string, string> auth, CancellationToken ct)
    {
        var settings = _settings.CurrentValue;
        using var scope = _scopes.CreateScope();
        var proxy = scope.ServiceProvider.GetRequiredService<SubsonicProxyService>();
        var counts = new Dictionary<string, int>(StringComparer.Ordinal);

        if (MixesOn(settings) && settings.Genres)
        {
            if (await CountGenresAsync(proxy, auth) is not { } genres) return false;
            foreach (var (key, count) in genres) counts[key] = count;
        }
        if (MixesOn(settings) && settings.Decades)
        {
            for (var decade = FirstDecade; decade <= DateTime.UtcNow.Year / 10 * 10; decade += 10)
            {
                ct.ThrowIfCancellationRequested();
                if (await FetchSongsAsync(proxy, auth, "rest/getRandomSongs", "randomSongs", new()
                    {
                        ["size"] = PoolPage.ToString(CultureInfo.InvariantCulture),
                        ["fromYear"] = decade.ToString(CultureInfo.InvariantCulture),
                        ["toYear"] = (decade + 9).ToString(CultureInfo.InvariantCulture),
                    }) is not { } songs)
                    return false;
                counts[$"decade:{decade}"] = songs.Count;
            }
        }

        UserMixes? before;
        lock (_lock) _users.TryGetValue(UserKey(username), out before);
        var (forYou, covers) = settings.AnyForYou
            ? await BuildForYouAsync(scope.ServiceProvider, proxy, username, auth, settings, before, ct)
            : (new Dictionary<string, List<string>>(StringComparer.Ordinal), new List<string>());

        lock (_lock)
        {
            var previous = _users.TryGetValue(UserKey(username), out var old) ? old.Active : [];
            var active = ApplyHysteresis(counts, previous, settings.EffectiveCreateAt, settings.EffectiveRemoveBelow,
                settings.EffectiveMaxPlaylists);
            _users[UserKey(username)] = new UserMixes
            {
                Active = active.ToList(), Counts = counts, CountsUtc = DateTime.UtcNow, Kinds = KindsOf(settings),
                ForYou = forYou, ForYouCovers = covers,
            };
            SaveLocked();
        }
        _logger.LogInformation("Mixes for {User}: {Count} from {Candidates} genres and decades", username,
            counts.Count(pair => pair.Value > 0), counts.Count);
        return true;
    }

    /// <summary>
    /// Genres by song count. Spellings that differ only in case are one genre, under the spelling
    /// with the most songs; years and the genre blocklist are never a mix of their own.
    /// </summary>
    private async Task<Dictionary<string, int>?> CountGenresAsync(SubsonicProxyService proxy, IReadOnlyDictionary<string, string> auth)
    {
        var result = await proxy.RelaySafeAsync("rest/getGenres", Parameters(auth, []));
        if (!result.Success || result.Body is not { Length: > 0 }) return null;
        try
        {
            var response = JsonNode.Parse(result.Body)?["subsonic-response"];
            if (response?["status"]?.GetValue<string>() != "ok") return null;
            var blocked = _genre.CurrentValue.EffectiveBlocklist();
            return ParseGenres(response["genres"]?["genre"] as JsonArray, blocked);
        }
        catch (Exception ex) when (ex is JsonException or InvalidOperationException or FormatException)
        {
            return null;
        }
    }

    internal static Dictionary<string, int> ParseGenres(JsonArray? rows, IReadOnlySet<string> blocked)
    {
        var counts = new Dictionary<string, int>(StringComparer.Ordinal);
        var spellings = (rows ?? [])
            .OfType<JsonObject>()
            .Select(row => (Value: (Str(row, "value") ?? "").Trim(), Count: Int(row, "songCount")))
            .Where(row => row.Value.Length > 0 && row.Count > 0
                && !GenreNormalizer.IsYearLike(row.Value) && !blocked.Contains(row.Value)
                && !blocked.Contains(DiscoveryStationSettings.NormalizeTag(row.Value)));
        foreach (var genre in spellings.GroupBy(row => row.Value, StringComparer.OrdinalIgnoreCase))
        {
            var largest = genre.OrderByDescending(row => row.Count).ThenBy(row => row.Value, StringComparer.Ordinal).First();
            counts[$"genre:{largest.Value}"] = largest.Count;
        }
        return counts;
    }

    /// <summary>
    /// Keep a mix while it has at least removeBelow tracks, add one once it reaches createAt,
    /// and show the largest first.
    /// </summary>
    internal static IReadOnlyList<string> ApplyHysteresis(IReadOnlyDictionary<string, int> counts,
        IReadOnlyCollection<string> previouslyActive, int createAt, int removeBelow, int max)
    {
        var previous = previouslyActive.ToHashSet(StringComparer.Ordinal);
        return counts
            .Where(pair => previous.Contains(pair.Key) ? pair.Value >= removeBelow : pair.Value >= createAt)
            .OrderByDescending(pair => pair.Value)
            .ThenBy(pair => pair.Key, StringComparer.Ordinal)
            .Take(max)
            .Select(pair => pair.Key)
            .ToList();
    }

    /// <summary>
    /// Made for you, from one walk of the listener's library as they see it. A list that could not
    /// be made this time (Navidrome or the catalog not answering) keeps what it had, so a bad day
    /// never empties it.
    /// </summary>
    private async Task<(Dictionary<string, List<string>> Lists, List<string> Covers)> BuildForYouAsync(
        IServiceProvider services, SubsonicProxyService proxy, string username, IReadOnlyDictionary<string, string> auth,
        GeneratedPlaylistSettings settings, UserMixes? before, CancellationToken ct)
    {
        var lists = new Dictionary<string, List<string>>(StringComparer.Ordinal);
        var covers = before?.ForYouCovers.ToList() ?? [];
        void KeepOld(string kind)
        {
            if (before?.ForYou.TryGetValue(kind, out var old) == true) lists[kind] = old;
        }

        var library = await WalkLibraryAsync(proxy, auth, ct);
        if (library is null)
        {
            foreach (var kind in ForYouLists.Kinds.Where(kind => ForYouOn(settings, kind))) KeepOld(kind);
            return (lists, covers);
        }

        var now = DateTime.UtcNow;
        var period = PeriodIndex(now, settings.EffectiveRefreshHours);
        var rediscover = ForYouLists.Rediscover(library, now, settings.EffectiveRediscoverMonths,
            Seed(username, ForYouPrefix + ForYouLists.RediscoverKind, period));
        if (settings.Rediscover) lists[ForYouLists.RediscoverKind] = rediscover.Select(song => song.ToJsonString()).ToList();
        if (settings.DeepCuts)
        {
            var artists = ForYouLists.TopArtists(library, ForYouLists.DeepCutArtists).Select(artist => artist.Key).ToList();
            var excluded = rediscover.Select(song => ForYouLists.Str(song, "id")).OfType<string>().ToHashSet(StringComparer.Ordinal);
            lists[ForYouLists.DeepCutsKind] = ForYouLists.DeepCuts(library, artists, excluded,
                Seed(username, ForYouPrefix + ForYouLists.DeepCutsKind, period)).Select(song => song.ToJsonString()).ToList();
        }
        if (settings.NewReleases)
        {
            var builder = services.GetService<NewReleasesBuilder>();
            var responses = services.GetService<SubsonicResponseBuilder>();
            if (builder is null || responses is null)
            {
                KeepOld(ForYouLists.NewReleasesKind);
            }
            else
            {
                var built = await builder.BuildAsync(ForYouLists.TopArtists(library, settings.EffectiveNewReleaseArtists),
                    library, DateOnly.FromDateTime(now), settings.EffectiveNewReleaseWeeks, ct);
                if (built.Entries.Count == 0 && !built.Whole)
                {
                    KeepOld(ForYouLists.NewReleasesKind);
                }
                else
                {
                    lists[ForYouLists.NewReleasesKind] = built.Entries
                        .Select(entry => (entry.Library ?? System.Text.Json.JsonSerializer.SerializeToNode(
                            responses.ConvertSongToJson(entry.Outside!))!.AsObject()).ToJsonString())
                        .ToList();
                    covers = built.Entries.Select(entry => entry.CoverUrl).OfType<string>()
                        .Distinct(StringComparer.Ordinal).Take(4).ToList();
                }
            }
        }
        _logger.LogInformation("Made for you for {User}: {Lists} from {Songs} library songs", username,
            string.Join(", ", lists.Select(pair => $"{ForYouLists.Name(pair.Key)} {pair.Value.Count}")), library.Count);
        return (lists, covers);
    }

    /// <summary>
    /// Every song of the listener's library as they see it, 500 at a time, up to
    /// <see cref="MaxWalkPages"/> pages; null when Navidrome did not answer, so nothing known is lost.
    /// </summary>
    private async Task<List<JsonObject>?> WalkLibraryAsync(SubsonicProxyService proxy, IReadOnlyDictionary<string, string> auth,
        CancellationToken ct)
    {
        var all = new List<JsonObject>();
        for (var page = 0; page < MaxWalkPages; page++)
        {
            ct.ThrowIfCancellationRequested();
            var songs = await FetchSongsAsync(proxy, auth, "rest/search3", "searchResult3", new()
            {
                // Navidrome's whole library: an empty phrase, as the duplicate scan walks it.
                ["query"] = "\"\"",
                ["songCount"] = PoolPage.ToString(CultureInfo.InvariantCulture),
                ["songOffset"] = (page * PoolPage).ToString(CultureInfo.InvariantCulture),
                ["albumCount"] = "0",
                ["artistCount"] = "0",
            });
            if (songs is null) return page == 0 ? null : all;
            all.AddRange(songs);
            if (songs.Count < PoolPage) return all;
        }
        _logger.LogInformation("Made for you read the first {Count} songs of a bigger library", all.Count);
        return all;
    }

    /// <summary>The songs of one mix for this period: the same for every request in the period.</summary>
    public async Task<IReadOnlyList<JsonObject>> MaterializeAsync(string username, GeneratedPlaylist playlist,
        IReadOnlyDictionary<string, string> auth, CancellationToken ct)
    {
        if (ForYouLists.IsForYou(playlist.Kind)) return StoredForYou(username, playlist.Kind);

        // Cached as text: a JsonObject is not safe to share between requests, and each request
        // needs objects of its own to hand to the response anyway.
        var cacheKey = $"{UserKey(username)}|{playlist.Id}|{playlist.PeriodStartUtc.Ticks}";
        if (_drawn.TryGetValue(cacheKey, out string[]? cached) && cached is not null) return Parse(cached);

        var settings = _settings.CurrentValue;
        using var scope = _scopes.CreateScope();
        var proxy = scope.ServiceProvider.GetRequiredService<SubsonicProxyService>();
        var pool = new List<JsonObject>();
        if (playlist.Kind == "decade")
        {
            var decade = int.Parse(playlist.Key["decade:".Length..], CultureInfo.InvariantCulture);
            pool.AddRange(await FetchSongsAsync(proxy, auth, "rest/getRandomSongs", "randomSongs", new()
            {
                ["size"] = PoolPage.ToString(CultureInfo.InvariantCulture),
                ["fromYear"] = decade.ToString(CultureInfo.InvariantCulture),
                ["toYear"] = (decade + 9).ToString(CultureInfo.InvariantCulture),
            }) ?? []);
        }
        else
        {
            for (var page = 0; page < MaxGenrePages; page++)
            {
                ct.ThrowIfCancellationRequested();
                var songs = await FetchSongsAsync(proxy, auth, "rest/getSongsByGenre", "songsByGenre", new()
                {
                    ["genre"] = playlist.Label,
                    ["count"] = PoolPage.ToString(CultureInfo.InvariantCulture),
                    ["offset"] = (page * PoolPage).ToString(CultureInfo.InvariantCulture),
                });
                if (songs is null) break;
                pool.AddRange(songs);
                if (songs.Count < PoolPage) break;
            }
        }

        var period = PeriodIndex(playlist.PeriodStartUtc, settings.EffectiveRefreshHours);
        var drawn = Select(pool, settings.EffectiveTrackCount, settings.EffectiveMaxPerArtist,
            settings.EffectiveNewShare, settings.EffectiveNewDays, DateTime.UtcNow, Seed(username, playlist.Key, period))
            .Select(song => song.ToJsonString())
            .ToArray();
        if (pool.Count > 0)
            _drawn.Set(cacheKey, drawn, new MemoryCacheEntryOptions { Size = 1, AbsoluteExpiration = playlist.PeriodEndUtc });
        return Parse(drawn);
    }

    private IReadOnlyList<JsonObject> StoredForYou(string username, string kind)
    {
        lock (_lock)
            return _users.TryGetValue(UserKey(username), out var state) && state.ForYou.TryGetValue(kind, out var songs)
                ? Parse(songs)
                : [];
    }

    /// <summary>The songs of one mix for this period if they have been drawn already, else null. Never fetches.</summary>
    public IReadOnlyList<JsonObject>? Drawn(string username, GeneratedPlaylist playlist) =>
        ForYouLists.IsForYou(playlist.Kind) ? StoredForYou(username, playlist.Kind) :
        _drawn.TryGetValue($"{UserKey(username)}|{playlist.Id}|{playlist.PeriodStartUtc.Ticks}", out string[]? cached) && cached is not null
            ? Parse(cached)
            : null;

    private static IReadOnlyList<JsonObject> Parse(IEnumerable<string> songs) =>
        songs.Select(song => JsonNode.Parse(song)!.AsObject()).ToList();

    /// <summary>
    /// A shuffled draw of up to <paramref name="count"/> songs, never more than
    /// <paramref name="maxPerArtist"/> by one artist, with <paramref name="newShare"/> percent kept
    /// for songs new to the listener when there are enough. The cap is never relaxed: a mix that
    /// cannot be filled without breaking it is shorter.
    /// </summary>
    internal static IReadOnlyList<JsonObject> Select(IReadOnlyList<JsonObject> pool, int count, int maxPerArtist,
        int newShare, int newDays, DateTime nowUtc, int seed)
    {
        var shuffled = pool.ToList();
        var random = new Random(seed);
        for (var i = shuffled.Count - 1; i > 0; i--)
        {
            var j = random.Next(i + 1);
            (shuffled[i], shuffled[j]) = (shuffled[j], shuffled[i]);
        }

        var taken = new SortedSet<int>();
        var perArtist = new Dictionary<string, int>(StringComparer.Ordinal);
        bool TryTake(int index)
        {
            var artist = ArtistKey(shuffled[index]);
            var already = perArtist.GetValueOrDefault(artist);
            if (already >= maxPerArtist) return false;
            perArtist[artist] = already + 1;
            taken.Add(index);
            return true;
        }

        var quota = (int)Math.Round(count * newShare / 100.0, MidpointRounding.AwayFromZero);
        for (var i = 0; i < shuffled.Count && taken.Count < Math.Min(quota, count); i++)
            if (IsNew(shuffled[i], nowUtc, newDays)) TryTake(i);
        for (var i = 0; i < shuffled.Count && taken.Count < count; i++)
            if (!taken.Contains(i)) TryTake(i);

        return taken.Select(index => shuffled[index]).ToList();
    }

    /// <summary>Never played by this listener, or added to the library in the last newDays.</summary>
    internal static bool IsNew(JsonObject song, DateTime nowUtc, int newDays)
    {
        if (Int(song, "playCount") == 0) return true;
        return Str(song, "created") is { } created
            && DateTime.TryParse(created, CultureInfo.InvariantCulture, DateTimeStyles.RoundtripKind, out var at)
            && nowUtc - at.ToUniversalTime() <= TimeSpan.FromDays(newDays);
    }

    private static string ArtistKey(JsonObject song) =>
        Str(song, "artistId") is { Length: > 0 } id ? id : SongIdentity.Key(Str(song, "artist"));

    /// <summary>
    /// Keep MIX_NEW_SHARE percent of Discovery Mix for library songs new to the listener. Discovery
    /// is otherwise all outside the library, so what someone added and never played was the one
    /// thing it could not surface.
    /// </summary>
    public async Task<IReadOnlyList<Song>> BlendIntoDiscoveryAsync(string username, LastFmRadioStation station,
        IReadOnlyList<Song> songs, IReadOnlyDictionary<string, string> auth, CancellationToken ct)
    {
        var settings = _settings.CurrentValue;
        var share = settings.EffectiveNewShare;
        if (share == 0 || station.Kind != LastFmRadioStationKind.Discovery || songs.Count == 0) return songs;
        var wanted = (int)Math.Round(songs.Count * share / 100.0, MidpointRounding.AwayFromZero);
        if (wanted == 0) return songs;

        // Only the candidates are cached, and applied to the station as it is now: a track that
        // became a library song since must not be put back as it was.
        var cacheKey = $"blend|{UserKey(username)}|{station.Id}|{station.ChangedUtc.Ticks}";
        if (!_drawn.TryGetValue(cacheKey, out string[]? candidates) || candidates is null)
        {
            using var scope = _scopes.CreateScope();
            var proxy = scope.ServiceProvider.GetRequiredService<SubsonicProxyService>();
            var fetched = await FetchSongsAsync(proxy, auth, "rest/getRandomSongs", "randomSongs", new()
            {
                ["size"] = BlendCandidates.ToString(CultureInfo.InvariantCulture),
            });
            if (fetched is null) return songs;
            candidates = fetched.Select(song => song.ToJsonString()).ToArray();
            _drawn.Set(cacheKey, candidates, new MemoryCacheEntryOptions
            {
                Size = 1,
                AbsoluteExpiration = station.ValidUntilUtc > DateTime.UtcNow ? station.ValidUntilUtc : DateTime.UtcNow.AddHours(1),
            });
        }
        return Blend(songs, Parse(candidates), wanted, settings.EffectiveNewDays, DateTime.UtcNow);
    }

    /// <summary>
    /// Put up to <paramref name="wanted"/> new library songs into the station, spread from the end
    /// so the familiar opening is untouched, never repeating a song already in it.
    /// </summary>
    internal static IReadOnlyList<Song> Blend(IReadOnlyList<Song> songs, IReadOnlyList<JsonObject> candidates,
        int wanted, int newDays, DateTime nowUtc)
    {
        var present = songs.Select(song => SongKey(song.Artist, song.Title)).ToHashSet(StringComparer.Ordinal);
        var picks = new List<Song>();
        foreach (var candidate in candidates)
        {
            if (picks.Count >= wanted) break;
            if (!IsNew(candidate, nowUtc, newDays) || Str(candidate, "id") is not { Length: > 0 } id) continue;
            if (!present.Add(SongKey(Str(candidate, "artist"), Str(candidate, "title")))) continue;
            picks.Add(new Song
            {
                Id = id,
                Title = Str(candidate, "title") ?? "",
                Artist = Str(candidate, "artist") ?? "",
                ArtistId = Str(candidate, "artistId"),
                Album = Str(candidate, "album") ?? "",
                AlbumId = Str(candidate, "albumId"),
                Duration = NullableInt(candidate, "duration"),
                Year = NullableInt(candidate, "year"),
                Track = NullableInt(candidate, "track"),
                Genre = Str(candidate, "genre"),
                Suffix = Str(candidate, "suffix"),
                BitRate = NullableInt(candidate, "bitRate"),
                Isrcs = Texts(candidate, "isrc"),
                IsLocal = true,
            });
        }
        if (picks.Count == 0) return songs;

        var result = songs.ToList();
        var step = Math.Max(1, songs.Count / picks.Count);
        for (var i = 0; i < picks.Count; i++)
            result[songs.Count - 1 - i * step] = picks[i];
        return result;
    }

    private static string SongKey(string? artist, string? title) =>
        SongIdentity.MatchKey(artist, title);

    /// <summary>
    /// The songs of one Subsonic call, or null when it did not answer: an empty list is a real
    /// answer and null is not, and only the first may replace what is known.
    /// </summary>
    private async Task<List<JsonObject>?> FetchSongsAsync(SubsonicProxyService proxy, IReadOnlyDictionary<string, string> auth,
        string endpoint, string container, Dictionary<string, string> extra)
    {
        try
        {
            var result = await proxy.RelaySafeAsync(endpoint, Parameters(auth, extra));
            if (!result.Success || result.Body is not { Length: > 0 }) return null;
            var response = JsonNode.Parse(result.Body)?["subsonic-response"];
            if (response?["status"]?.GetValue<string>() != "ok") return null;
            return (response[container]?["song"] as JsonArray ?? [])
                .OfType<JsonObject>()
                .Select(song => song.DeepClone().AsObject())
                .ToList();
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _logger.LogDebug("Mix lookup {Endpoint} failed: {M}", endpoint, ex.Message);
            return null;
        }
    }

    /// <summary>
    /// The caller's own parameters, whatever auth they carry (a token, a password or an API key),
    /// as the sync catalog does, minus anything that named the playlist being read.
    /// </summary>
    private static Dictionary<string, string> Parameters(IReadOnlyDictionary<string, string> auth,
        Dictionary<string, string> extra)
    {
        var parameters = auth.ToDictionary(pair => pair.Key, pair => pair.Value, StringComparer.Ordinal);
        parameters.Remove("id");
        parameters.Remove("playlistId");
        foreach (var (key, value) in extra) parameters[key] = value;
        parameters["f"] = "json";
        return parameters;
    }

    internal static string PlaylistId(string username, string key) =>
        "og" + LastFmRadioStateStore.ToBase62(SHA256.HashData(Encoding.UTF8.GetBytes($"{UserKey(username)}|{key}")), 20);

    internal static long PeriodIndex(DateTime nowUtc, int hours) =>
        (long)Math.Floor((nowUtc - DateTime.UnixEpoch).TotalHours / hours);

    internal static int Seed(string username, string key, long period) =>
        BitConverter.ToInt32(SHA256.HashData(Encoding.UTF8.GetBytes($"{UserKey(username)}|{key}|{period}")), 0);

    private static string UserKey(string username) => username.Trim().ToLowerInvariant();

    private static string? Str(JsonObject node, string name) =>
        node[name] is JsonValue value && value.TryGetValue<string>(out var text) ? text : null;

    private static int Int(JsonObject node, string name) => NullableInt(node, name) ?? 0;

    /// <summary>A list of text as Navidrome sent it, OpenSubsonic's <c>isrc</c>; empty when absent.</summary>
    private static List<string> Texts(JsonObject node, string name) =>
        node[name] is JsonArray values
            ? values.OfType<JsonValue>().Select(value => value.TryGetValue<string>(out var text) ? text : null)
                .OfType<string>().ToList()
            : [];

    private static int? NullableInt(JsonObject node, string name) =>
        node[name] is JsonValue value
            ? value.TryGetValue<int>(out var number) ? number
            : value.TryGetValue<long>(out var wide) ? (int)Math.Clamp(wide, int.MinValue, int.MaxValue)
            : value.TryGetValue<double>(out var real) ? (int)real
            : null
            : null;

    private void Load()
    {
        if (_statePath is null || !File.Exists(_statePath)) return;
        try
        {
            var document = JsonSerializer.Deserialize<StateDocument>(File.ReadAllText(_statePath), Json);
            if (document?.Users is null) return;
            lock (_lock)
                foreach (var (user, state) in document.Users) _users[user] = state;
        }
        catch (Exception ex)
        {
            // Only counts: the next list rebuilds them, so a bad file is set aside, not fatal.
            _logger.LogWarning("Mix state could not be read ({M}); it will be rebuilt", ex.Message);
            try { File.Move(_statePath, $"{_statePath}.corrupt-{DateTime.UtcNow.Ticks}"); } catch { /* best effort */ }
        }
    }

    private void SaveLocked()
    {
        if (_statePath is null) return;
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(_statePath)!);
            var tmp = _statePath + ".tmp";
            File.WriteAllText(tmp, JsonSerializer.Serialize(new StateDocument { Users = _users }, Json));
            File.Move(tmp, _statePath, overwrite: true);
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Mix state could not be written: {M}", ex.Message);
        }
    }
}
