using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Radio;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.Soulseek;
using Octo.Services.Subsonic;

namespace Octo.Services.Library;

/// <summary>
/// "Popular right now": the chart of the moment (the same one the Octo apps show before
/// anything is typed in search) as a read-only playlist for each listener, so every Subsonic
/// app gets it beside the radio stations and mixes. A song the listener's library has is their
/// own copy, as Navidrome describes it to them; the rest are outside songs that play right away
/// and are added with a heart, while search discovery is on and the explicit filter lets them.
///
/// Per listener because the library match is theirs, and made with their own sign-in inside
/// their own request, as the mixes are: Octo keeps no one's Navidrome password. The list holds
/// still for <see cref="RefreshHours"/> so every app shows the same songs, then is made again
/// the next time that listener's app lists playlists. A chart or library that does not answer
/// never empties a list that was made: the last one stays until a new one can be made.
/// Nothing is written to Navidrome, and nothing to disk: after a restart the first list is made
/// again in a second or two.
/// </summary>
public sealed class PopularPlaylistService
{
    public const string Key = "chart:popular";
    public const string Kind = "chart";
    public const string Name = "Popular right now";

    /// <summary>Marks the sync catalog's tracks that came from this list, whose ids are the
    /// list's own rather than found again by search.</summary>
    public const string TrackSource = "popular";

    /// <summary>How long one list holds still. The chart itself changes about once a day; four
    /// times a day keeps the list current, and a song a listener added shows as theirs the same
    /// day, without the list shifting under someone in the middle of it.</summary>
    public const int RefreshHours = 6;

    /// <summary>Songs in the list: the whole chart Octo keeps.</summary>
    public const int MaxSongs = TopSongsService.MaxRows;

    private static readonly TimeSpan FirstListWait = TimeSpan.FromSeconds(1.5);
    private static readonly TimeSpan OpenWait = TimeSpan.FromSeconds(10);
    private static readonly TimeSpan BuildDeadline = TimeSpan.FromSeconds(60);
    private static readonly TimeSpan RetryAfterFailure = TimeSpan.FromMinutes(5);

    /// <summary>A listener's list as made: for which period and settings, whether every library
    /// lookup answered, its songs as Subsonic song JSON, and its outside songs.</summary>
    internal sealed record Made(long Period, DateTime MadeUtc, string Rule, bool Whole,
        IReadOnlyList<string> Entries, IReadOnlyList<Song> Outside);

    private readonly TopSongsService _topSongs;
    private readonly SubsonicResponseBuilder _responses;
    private readonly ExternalIdRegistry _registry;
    private readonly IServiceScopeFactory _scopes;
    private readonly IOptionsMonitor<GeneratedPlaylistSettings> _settings;
    private readonly IOptionsMonitor<SubsonicSettings> _subsonic;
    private readonly ILogger<PopularPlaylistService> _logger;
    private readonly SingleFlight<string, bool> _builds = new();
    private readonly object _lock = new();
    private readonly Dictionary<string, Made> _users = new(StringComparer.Ordinal);
    private readonly Dictionary<string, DateTime> _failedAt = new(StringComparer.Ordinal);

    /// <summary>The clock, for tests.</summary>
    internal Func<DateTime> Clock { get; set; } = () => DateTime.UtcNow;

    public PopularPlaylistService(TopSongsService topSongs, SubsonicResponseBuilder responses,
        ExternalIdRegistry registry, IServiceScopeFactory scopes, IOptionsMonitor<GeneratedPlaylistSettings> settings,
        IOptionsMonitor<SubsonicSettings> subsonic, ILogger<PopularPlaylistService> logger)
    {
        _topSongs = topSongs;
        _responses = responses;
        _registry = registry;
        _scopes = scopes;
        _settings = settings;
        _subsonic = subsonic;
        _logger = logger;
    }

    public bool On => _settings.CurrentValue.PopularNow;

    /// <summary>
    /// The listener's list, made again first when its period ended or the settings it was made
    /// under changed. A listener's very first list waits a moment for it, so it shows on their
    /// first look; later ones are made behind the answer. Null while there is nothing in it.
    /// </summary>
    public async Task<GeneratedPlaylist?> ListAsync(string username, IReadOnlyDictionary<string, string> auth)
    {
        if (!On || string.IsNullOrWhiteSpace(username)) return null;
        var (made, building) = Ensure(username, auth);
        if (made is null && building is not null) await Task.WhenAny(building, Task.Delay(FirstListWait));
        return Find(username, PlaylistId(username));
    }

    /// <summary>The list by id, for this listener, as last made. Null for anyone else's id,
    /// while it is off, or while it is empty.</summary>
    public GeneratedPlaylist? Find(string username, string id) =>
        IsTheList(username, id) && Playlist(username) is { PoolSize: > 0 } list ? list : null;

    /// <summary>Whether the id is this listener's list while it is on, made yet or not: an app
    /// that kept the id across a restart still opens it.</summary>
    public bool IsTheList(string username, string id) =>
        On && !string.IsNullOrWhiteSpace(username) && !string.IsNullOrEmpty(id) && id == PlaylistId(username);

    /// <summary>The list as a playlist, as last made: its size is 0 while nothing is in it.</summary>
    public GeneratedPlaylist Playlist(string username)
    {
        var made = Current(username);
        var period = made?.Period ?? GeneratedPlaylistService.PeriodIndex(Clock(), RefreshHours);
        var start = DateTime.UnixEpoch.AddHours(period * RefreshHours);
        return new GeneratedPlaylist(PlaylistId(username), Key, Kind, Name, Name, username.Trim(),
            made?.Entries.Count ?? 0, start, start.AddHours(RefreshHours));
    }

    /// <summary>Starts making the listener's list when one is due, without waiting for it.</summary>
    public void Warm(string username, IReadOnlyDictionary<string, string> auth)
    {
        if (On && !string.IsNullOrWhiteSpace(username)) Ensure(username, auth);
    }

    /// <summary>
    /// The list's songs, made again first when it is due (waiting a few seconds for that, then
    /// answering with the last one rather than keep an app waiting).
    /// </summary>
    public async Task<IReadOnlyList<JsonObject>> SongsAsync(string username, IReadOnlyDictionary<string, string> auth,
        CancellationToken ct)
    {
        if (!On || string.IsNullOrWhiteSpace(username)) return [];
        var (made, building) = Ensure(username, auth);
        if (building is not null)
        {
            var wait = made is { Entries.Count: > 0 } ? OpenWait : BuildDeadline;
            await Task.WhenAny(building, Task.Delay(wait, ct));
        }
        KeepPlayable(OutsideSongs(username));
        return Songs(username) ?? [];
    }

    /// <summary>
    /// Outside songs play by their id, which Octo remembers in memory for a while. A list holds
    /// still for hours, so before it is handed out each of its outside songs is made known again
    /// if it was forgotten: the same song gives the same id.
    /// </summary>
    internal void KeepPlayable(IEnumerable<Song> songs)
    {
        foreach (var song in songs)
        {
            if (_registry.Lookup(song.Id) is not null) continue;
            if (song.ArtistId is { } artistId && _registry.Lookup(artistId) is null)
                _registry.Register(new SoulseekRouting { Kind = RoutingKind.Artist, Artist = song.Artist });
            if (song.AlbumId is { } albumId && _registry.Lookup(albumId) is null && !string.IsNullOrWhiteSpace(song.Album))
                _registry.Register(new SoulseekRouting { Kind = RoutingKind.Album, Artist = song.Artist, Album = song.Album });
            var id = _registry.Register(new SoulseekRouting
            {
                Kind = RoutingKind.Song,
                Artist = song.Artist,
                Title = song.Title,
                Album = song.Album,
                Duration = song.Duration,
                ExplicitContent = song.ExplicitContentLyrics,
            });
            _registry.RememberLength(id, song.Duration, LengthSource.Deezer);
        }
    }

    /// <summary>The songs of the list as last made, or null when none was. Never makes one.</summary>
    public IReadOnlyList<JsonObject>? Songs(string username) =>
        Current(username) is { } made ? made.Entries.Select(entry => JsonNode.Parse(entry)!.AsObject()).ToList() : null;

    /// <summary>The list's outside songs as last made, in order. Never makes one.</summary>
    public IReadOnlyList<Song> OutsideSongs(string username) => Current(username)?.Outside ?? [];

    /// <summary>
    /// The list's outside songs as a station, for the sync catalog of a client that copies the
    /// library to the device: it then holds these songs under the ids the playlist lists, so the
    /// playlist finds them there. Null while the list is off or has no outside songs.
    /// </summary>
    public LastFmRadioStation? AsSyncStation(string username)
    {
        if (!On || Current(username) is not { Outside.Count: > 0 } made) return null;
        return new LastFmRadioStation
        {
            Id = PlaylistId(username),
            Key = Key,
            Name = Name,
            Owner = username.Trim(),
            CreatedUtc = made.MadeUtc,
            ChangedUtc = made.MadeUtc,
            ValidUntilUtc = DateTime.UnixEpoch.AddHours((made.Period + 1) * RefreshHours),
            Tracks = made.Outside.Select(song => new LastFmRadioTrack
            {
                Artist = song.Artist ?? "",
                Title = song.Title ?? "",
                Album = song.Album,
                Duration = song.Duration,
                Source = TrackSource,
                ResolvedId = song.Id,
                ExternalProvider = song.ExternalProvider,
            }).ToList(),
        };
    }

    private Made? Current(string username)
    {
        lock (_lock) return _users.TryGetValue(UserKey(username), out var made) ? made : null;
    }

    /// <summary>What decides whether a made list still holds: outside songs shown or not, and
    /// which of them the explicit filter lets through.</summary>
    private string Rule()
    {
        var subsonic = _subsonic.CurrentValue;
        return $"{subsonic.EnableSearchDiscovery}|{subsonic.ExplicitFilter}";
    }

    /// <summary>The listener's list as it is, and the making of a new one when one is due.</summary>
    private (Made? Made, Task<bool>? Building) Ensure(string username, IReadOnlyDictionary<string, string> auth)
    {
        var user = UserKey(username);
        var now = Clock();
        var period = GeneratedPlaylistService.PeriodIndex(now, RefreshHours);
        var rule = Rule();
        Made? made;
        lock (_lock)
        {
            _users.TryGetValue(user, out made);
            var due = made is null || made.Period != period || made.Rule != rule || !made.Whole;
            // A list that could not be made is tried again after a while, not on every request.
            if (!due || (_failedAt.TryGetValue(user, out var failed) && now - failed < RetryAfterFailure))
                return (made, null);
        }

        var copy = auth.ToDictionary(pair => pair.Key, pair => pair.Value);
        var building = _builds.RunAsync(user, ct => MakeAsync(username, copy, period, rule, ct), BuildDeadline);
        _ = building.ContinueWith(task =>
            {
                lock (_lock) _failedAt[user] = Clock();
                _logger.LogDebug(task.Exception, "Popular right now could not be made for {User}", username);
            },
            CancellationToken.None, TaskContinuationOptions.OnlyOnFaulted, TaskScheduler.Default);
        return (made, building);
    }

    /// <summary>
    /// Make the listener's list: the chart, each song matched against their library by the rule
    /// radio uses to play a library copy, the rest kept as outside songs when they may be shown.
    /// </summary>
    private async Task<bool> MakeAsync(string username, IReadOnlyDictionary<string, string> auth, long period,
        string rule, CancellationToken ct)
    {
        var user = UserKey(username);
        var chart = await _topSongs.ChartAsync(ct);
        var rows = chart.Songs.Take(MaxSongs).ToList();
        if (rows.Count == 0)
        {
            lock (_lock) _failedAt[user] = Clock();
            _logger.LogInformation("Popular right now for {User}: the chart did not answer, the last list stays", username);
            return false;
        }

        using var scope = _scopes.CreateScope();
        var proxy = scope.ServiceProvider.GetRequiredService<SubsonicProxyService>();
        var unanswered = 0;
        var library = await TopSongsService.MatchLibraryAsync(rows, async (query, _) =>
        {
            var found = await SearchLibraryAsync(proxy, auth, query);
            if (found is null) Interlocked.Increment(ref unanswered);
            return found;
        }, ct);

        lock (_lock)
        {
            // A library that answered nothing at all must not turn a list of the listener's own
            // songs into outside ones.
            if (unanswered == rows.Count && _users.TryGetValue(user, out var before) && before.Entries.Count > 0)
            {
                _failedAt[user] = Clock();
                return false;
            }
        }

        var subsonic = _subsonic.CurrentValue;
        var (entries, outside) = Assemble(rows, library, subsonic.EnableSearchDiscovery, subsonic.ExplicitFilter,
            song => JsonSerializer.SerializeToNode(_responses.ConvertSongToJson(song))!.AsObject());
        lock (_lock)
        {
            _users[user] = new Made(period, Clock(), rule, unanswered == 0,
                entries.Select(entry => entry.ToJsonString()).ToList(), outside);
            // A list made while some library lookups did not answer is made again after a while.
            if (unanswered == 0) _failedAt.Remove(user);
            else _failedAt[user] = Clock();
        }
        _logger.LogInformation("Popular right now for {User}: {Library} library and {Outside} outside songs from the {Source} chart",
            username, entries.Count - outside.Count, outside.Count, chart.Source);
        return true;
    }

    /// <summary>
    /// The list in chart order: the library's song where the row has one, else the outside song
    /// when outside songs may be shown and the explicit filter lets it through. The filter is
    /// the one stations follow: a list Octo picks follows it, a search does not.
    /// </summary>
    internal static (List<JsonObject> Entries, List<Song> Outside) Assemble(IReadOnlyList<TopSongsService.TopSong> rows,
        IReadOnlyList<JsonElement?> library, bool outsideShown, ExplicitFilter filter, Func<Song, JsonObject> asJson)
    {
        var entries = new List<JsonObject>();
        var outside = new List<Song>();
        for (var i = 0; i < rows.Count; i++)
        {
            if (i < library.Count && library[i] is { } owned)
            {
                entries.Add(JsonNode.Parse(owned.GetRawText())!.AsObject());
            }
            else if (outsideShown && Passes(rows[i].Song, filter))
            {
                entries.Add(asJson(rows[i].Song));
                outside.Add(rows[i].Song);
            }
        }
        return (entries, outside);
    }

    internal static bool Passes(Song song, ExplicitFilter filter)
    {
        var status = ExplicitStatus.ForClients(song.ExplicitContentLyrics);
        return filter switch
        {
            ExplicitFilter.CleanOnly => status != ExplicitStatus.ExplicitWord,
            ExplicitFilter.ExplicitOnly => status != ExplicitStatus.CleanWord,
            _ => true,
        };
    }

    /// <summary>Navidrome's songs for a query, searched as the listener, or null when it did not
    /// answer. Library only: this goes to Navidrome, not through Octo's search.</summary>
    private async Task<JsonElement?> SearchLibraryAsync(SubsonicProxyService proxy,
        IReadOnlyDictionary<string, string> auth, string query)
    {
        // Only who is asking and how: the request it came from (a playlist list, a sync walk
        // with its offsets) says nothing this search should carry.
        var search = auth.Where(pair => SignInKeys.Contains(pair.Key))
            .ToDictionary(pair => pair.Key, pair => pair.Value, StringComparer.Ordinal);
        search["query"] = query;
        search["songCount"] = "5";
        search["albumCount"] = "0";
        search["artistCount"] = "0";
        search["f"] = "json";
        try
        {
            var result = await proxy.RelaySafeAsync("rest/search3", search);
            if (!result.Success || result.Body is not { Length: > 0 }) return null;
            using var document = JsonDocument.Parse(result.Body);
            if (!document.RootElement.TryGetProperty("subsonic-response", out var response)
                || !response.TryGetProperty("status", out var status) || status.GetString() != "ok")
                return null;
            return response.TryGetProperty("searchResult3", out var found) && found.TryGetProperty("song", out var songs)
                ? songs.Clone()
                : JsonDocument.Parse("[]").RootElement.Clone();
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _logger.LogDebug("Popular right now: library search '{Q}' failed: {M}", query, ex.Message);
            return null;
        }
    }

    /// <summary>The Subsonic parameters that say who is asking: a password, a token and its salt,
    /// or an API key, with the protocol version and the app's name.</summary>
    private static readonly HashSet<string> SignInKeys = new(StringComparer.Ordinal) { "u", "p", "t", "s", "apiKey", "v", "c" };

    /// <summary>The list's id for this listener: Octo's generated playlist shape ("og"), so
    /// every guard that keeps Octo's playlists read-only covers it.</summary>
    public static string PlaylistId(string username) => GeneratedPlaylistService.PlaylistId(username, Key);

    private static string UserKey(string username) => username.Trim().ToLowerInvariant();
}
