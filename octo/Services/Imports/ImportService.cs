using System.Collections.Concurrent;
using System.Security.Cryptography;
using System.Text;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Common;

namespace Octo.Services.Imports;

/// <summary>
/// Importing from Spotify, a public link or a file, for the dashboard and the Octo apps alike:
/// the Spotify sign-in, reading every list, matching each song against the library, keeping a list
/// as a Navidrome playlist, and handing missing songs to the trickle.
///
/// Also the loop that keeps it all current: lists kept as playlists or fetched in full are read
/// again every few hours, a song the trickle fetched is matched and put in its playlists, and a
/// playlist that changed is written. Everything slow runs here, never inside a request.
/// </summary>
public sealed class ImportService : BackgroundService
{
    internal static readonly TimeSpan LoopEvery = TimeSpan.FromSeconds(30);
    /// <summary>Spotify ends every sign-in this long after it was made, however often it is renewed.</summary>
    internal static readonly TimeSpan SignInLasts = TimeSpan.FromDays(182);
    private static readonly TimeSpan BetweenPlaylists = TimeSpan.FromMilliseconds(250);

    private readonly ImportStore _store;
    private readonly TrickleQueue _queue;
    private readonly TrickleWorker _trickle;
    private readonly ImportMatcher _matcher;
    private readonly ImportPlaylists _playlists;
    private readonly SpotifyAuth _auth;
    private readonly SpotifyAccountStore _accounts;
    private readonly SpotifyWebApi _spotify;
    private readonly SpotifyLinkReader _links;
    private readonly IOptionsMonitor<ImportSettings> _settings;
    private readonly AcquisitionTracker? _tracker;
    private readonly ILogger<ImportService> _logger;

    private readonly ConcurrentDictionary<string, ReadView> _reading = new(StringComparer.OrdinalIgnoreCase);
    private readonly ConcurrentDictionary<string, SemaphoreSlim> _gates = new(StringComparer.OrdinalIgnoreCase);
    /// <summary>People whose lists need matching again: the trickle brought them something.</summary>
    private readonly ConcurrentDictionary<string, byte> _rematch = new(StringComparer.OrdinalIgnoreCase);
    /// <summary>Lists whose playlist needs writing, as "owner\nid".</summary>
    private readonly ConcurrentDictionary<string, byte> _dirtyPlaylists = new(StringComparer.Ordinal);
    private CancellationToken _stopping = CancellationToken.None;

    public ImportService(ImportStore store, TrickleQueue queue, TrickleWorker trickle, ImportMatcher matcher,
        ImportPlaylists playlists, SpotifyAuth auth, SpotifyAccountStore accounts, SpotifyWebApi spotify,
        SpotifyLinkReader links, IOptionsMonitor<ImportSettings> settings, ILogger<ImportService> logger,
        AcquisitionTracker? tracker = null)
    {
        _store = store;
        _queue = queue;
        _trickle = trickle;
        _matcher = matcher;
        _playlists = playlists;
        _auth = auth;
        _accounts = accounts;
        _spotify = spotify;
        _links = links;
        _settings = settings;
        _tracker = tracker;
        _logger = logger;
        _trickle.Finished += OnFetched;
    }

    internal Func<DateTime> Clock { get; set; } = () => DateTime.UtcNow;

    // =========================================================================================
    // Reading what there is
    // =========================================================================================

    public ImportOverview Overview(string user)
    {
        var lists = _store.ListsOf(user);
        var jobs = Jobs(user);
        return new ImportOverview(SpotifyFor(user), ReadingFor(user),
            lists.Select(list => Summary(list, jobs)).ToList(), Trickle(user, jobs), _matcher.Problem);
    }

    public ListDetail? Detail(string user, string id)
    {
        if (_store.Get(user, id) is not { } list) return null;
        var jobs = Jobs(user);
        var rows = Rows();
        return new ListDetail(Summary(list, jobs), list.Tracks.Select(track => View(track, jobs, rows)).ToList());
    }

    private Dictionary<string, TrickleJob> Jobs(string user) =>
        _queue.Snapshot(user).GroupBy(job => job.Key).ToDictionary(group => group.Key, group => group.First(), StringComparer.Ordinal);

    private Dictionary<string, AcquisitionSnapshot> Rows() =>
        (_tracker?.All() ?? []).GroupBy(row => AcquisitionTracker.KeyOf(row.Provider, row.ExternalId))
            .ToDictionary(group => group.Key, group => group.First(), StringComparer.OrdinalIgnoreCase);

    private SpotifyView SpotifyFor(string user)
    {
        var settings = _settings.CurrentValue;
        var redirect = settings.EffectiveRedirectUri;
        var account = _accounts.Get(user);
        return new SpotifyView(
            Configured: !string.IsNullOrWhiteSpace(settings.SpotifyClientId),
            Connected: account is not null && account.Problem is null,
            Account: account?.DisplayName ?? account?.SpotifyId,
            Problem: account?.Problem,
            RedirectUri: redirect,
            RedirectProblem: SpotifyAuth.RedirectProblem(redirect),
            OctoFinishes: OctoFinishes(redirect),
            EndsUtc: account is null ? null : account.ConnectedUtc + SignInLasts);
    }

    /// <summary>Whether the redirect URI is Octo's own callback, so the browser comes back by itself.</summary>
    public static bool OctoFinishes(string redirectUri) =>
        Uri.TryCreate(redirectUri, UriKind.Absolute, out var uri)
        && uri.AbsolutePath.TrimEnd('/').EndsWith(CallbackPath, StringComparison.OrdinalIgnoreCase);

    /// <summary>Where Octo answers Spotify's redirect itself.</summary>
    public const string CallbackPath = "/imports/spotify/callback";

    private ReadView ReadingFor(string user) =>
        _reading.TryGetValue(user, out var read) ? read : new ReadView(false, null, null, null);

    private ListSummary Summary(ImportList list, IReadOnlyDictionary<string, TrickleJob> jobs)
    {
        var states = list.Tracks.Select(track => StateOf(track, jobs)).ToList();
        int Count(string state) => states.Count(s => s == state);
        return new ListSummary(list.Id, list.Name, list.Source, list.By, list.ImageUrl, Math.Max(list.Total, list.Tracks.Count),
            Count(ImportTrackStates.Have), Count(ImportTrackStates.Missing), Count(ImportTrackStates.Queued),
            Count(ImportTrackStates.Downloading), Count(ImportTrackStates.Done), Count(ImportTrackStates.NotFound),
            Count(ImportTrackStates.Skipped), list.KeepPlaylist, list.PlaylistId, list.PlaylistNote, list.GetMissing,
            list.Partial, list.Gone, ImportSources.CanRefresh(list.Source), list.ReadUtc, list.MatchedUtc);
    }

    private static string StateOf(ImportTrack track, IReadOnlyDictionary<string, TrickleJob> jobs) =>
        track.LibraryId is not null ? ImportTrackStates.Have
        : jobs.TryGetValue(track.Key, out var job) ? job.State
        : ImportTrackStates.Missing;

    private static TrackView View(ImportTrack track, IReadOnlyDictionary<string, TrickleJob> jobs,
        IReadOnlyDictionary<string, AcquisitionSnapshot> rows)
    {
        var state = StateOf(track, jobs);
        jobs.TryGetValue(track.Key, out var job);
        var (detail, progress) = state == ImportTrackStates.Have ? (null, null) : Words(job, rows);
        return new TrackView(track.Key, track.Title, track.Artist, track.Album, track.Seconds, state, detail,
            track.LibraryId ?? job?.LibraryId, progress, job?.UpdatedUtc);
    }

    private static TrackView View(TrickleJob job, IReadOnlyDictionary<string, AcquisitionSnapshot> rows)
    {
        var (detail, progress) = Words(job, rows);
        return new TrackView(job.Key, job.Title, job.Artist, job.Album, job.Seconds, job.State, detail, job.LibraryId,
            progress, job.UpdatedUtc);
    }

    /// <summary>What a song is waiting for or what happened to it, from its download's own row while it runs.</summary>
    private static (string? Detail, double? Progress) Words(TrickleJob? job, IReadOnlyDictionary<string, AcquisitionSnapshot> rows)
    {
        if (job is null) return (null, null);
        if (job.State != ImportTrackStates.Downloading || job.AcquisitionKey is null
            || !rows.TryGetValue(job.AcquisitionKey, out var row))
            return (job.Detail ?? (job.State == ImportTrackStates.Downloading ? "Checking your library" : null), null);
        var source = row.Source;
        var words = row.State switch
        {
            AcquisitionState.Queued => "Waiting for a download slot",
            AcquisitionState.Searching => source is null ? "Searching" : $"Searching {source}",
            AcquisitionState.Downloading => source is null ? "Downloading" : $"Downloading from {source}",
            AcquisitionState.Verifying => "Checking it is the right song",
            AcquisitionState.Importing => "Waiting for Navidrome to show it",
            _ => job.Detail,
        };
        if (row.Note is { Length: > 0 } note) words = $"{words}. {note}";
        return (words, row.State == AcquisitionState.Downloading ? row.Progress : null);
    }

    private TrickleView Trickle(string user, IReadOnlyDictionary<string, TrickleJob> jobs)
    {
        var status = _trickle.StatusFor(user);
        var rows = Rows();
        var all = jobs.Values.ToList();
        int Count(string state) => all.Count(job => job.State == state);
        var current = all.Where(job => job.State == ImportTrackStates.Downloading).MaxBy(job => job.StartedUtc);
        var recent = all.Where(job => !ImportTrackStates.Open(job.State)).OrderByDescending(job => job.UpdatedUtc).Take(20)
            .Select(job => View(job, rows)).ToList();
        var next = all.Where(job => job.State == ImportTrackStates.Queued).OrderBy(job => job.QueuedUtc).Take(5)
            .Select(job => View(job, rows)).ToList();
        return new TrickleView(status.State, status.NextUtc, status.PerHour, Count(ImportTrackStates.Queued),
            Count(ImportTrackStates.Downloading), Count(ImportTrackStates.Done), Count(ImportTrackStates.NotFound),
            Count(ImportTrackStates.Skipped), current is null ? null : View(current, rows), recent, next);
    }

    // =========================================================================================
    // The Spotify sign-in
    // =========================================================================================

    /// <summary>
    /// Starts a sign-in: the address to open. An app may ask for its own loopback redirect, which
    /// must be the registered one, or the same loopback address on another port.
    /// </summary>
    public ImportActionResult BeginSpotify(string user, string? appRedirect = null)
    {
        var settings = _settings.CurrentValue;
        if (string.IsNullOrWhiteSpace(settings.SpotifyClientId))
            return new(false, "Add your Spotify app's Client ID first, on the dashboard's Spotify import page.");
        var registered = settings.EffectiveRedirectUri;
        if (SpotifyAuth.RedirectProblem(registered) is { } problem) return new(false, problem);
        if (SpotifyAuth.AllowedRedirect(registered, appRedirect) is not { } redirect)
            return new(false, $"This app needs the redirect URI registered as {registered}, a loopback address with no port. Change it on the dashboard and on the Spotify app.");
        var (url, _) = _auth.Begin(user, settings.SpotifyClientId.Trim(), redirect);
        return new(true, "Opened Spotify. Allow Octo there.", Url: url);
    }

    /// <summary>
    /// Finishes a sign-in from what Spotify sent back: the whole address, or code and state. The
    /// state says whose it is; <paramref name="user"/>, when given, must be the same person.
    /// </summary>
    public async Task<ImportActionResult> FinishSpotifyAsync(string? pasted, string? code, string? state, string? user,
        CancellationToken ct)
    {
        if (pasted is not null)
        {
            var parsed = SpotifyAuth.ParseReturn(pasted);
            if (parsed.Error is { } refused)
                return new(false, refused == "access_denied" ? "Spotify says access was not allowed." : $"Spotify sent back an error: {refused}");
            (code, state) = (parsed.Code, parsed.State ?? state);
        }
        if (string.IsNullOrWhiteSpace(code)) return new(false, "That address holds no code from Spotify. Copy the whole address the browser ended on.");
        if (_auth.Take(state) is not { } pending)
            return new(false, "That sign-in is too old or was already used. Start again with Connect.");
        if (user is not null && !string.Equals(user, pending.User, StringComparison.OrdinalIgnoreCase))
            return new(false, "That sign-in was started by someone else.");
        var clientId = _settings.CurrentValue.SpotifyClientId.Trim();
        try
        {
            var tokens = await _spotify.ExchangeAsync(clientId, code!, pending.Verifier, pending.RedirectUri, ct);
            var profile = await _spotify.ProfileAsync(tokens.AccessToken, ct);
            _accounts.Set(pending.User, new SpotifyAccount
            {
                AccessToken = tokens.AccessToken, RefreshToken = tokens.RefreshToken ?? "", ExpiresUtc = tokens.ExpiresUtc,
                ConnectedUtc = Clock(), SpotifyId = profile.Id, DisplayName = profile.DisplayName,
            });
            _logger.LogInformation("{User} connected Spotify account {Account}", pending.User, profile.DisplayName ?? profile.Id);
            StartReading(pending.User);
            return new(true, $"Connected to Spotify as {profile.DisplayName ?? profile.Id}. Octo is reading your lists.");
        }
        catch (SpotifyException ex)
        {
            _logger.LogWarning("Spotify sign-in for {User} failed: {Message}", pending.User, ex.Message);
            return new(false, ex.Message);
        }
    }

    public ImportActionResult DisconnectSpotify(string user)
    {
        var removed = _accounts.Remove(user);
        return new(true, removed
            ? "Disconnected. Your lists stay, and are no longer read again. Remove Octo under Apps on your Spotify account page to end it there too."
            : "No Spotify account was connected.");
    }

    /// <summary>A working access token for this person, renewed when it is about to end.</summary>
    private async Task<string> TokenAsync(string user, CancellationToken ct)
    {
        var account = _accounts.Get(user) ?? throw new SpotifyException("Connect a Spotify account first.");
        if (account.Problem is { } problem) throw new SpotifyException(problem, signInEnded: true);
        if (account.ExpiresUtc - Clock() > TimeSpan.FromMinutes(2)) return account.AccessToken;
        try
        {
            var tokens = await _spotify.RefreshAsync(_settings.CurrentValue.SpotifyClientId.Trim(), account.RefreshToken, ct);
            _accounts.Update(user, a =>
            {
                a.AccessToken = tokens.AccessToken;
                a.ExpiresUtc = tokens.ExpiresUtc;
                if (!string.IsNullOrEmpty(tokens.RefreshToken)) a.RefreshToken = tokens.RefreshToken;
            });
            return tokens.AccessToken;
        }
        catch (SpotifyException ex) when (ex.SignInEnded)
        {
            _accounts.Update(user, a => a.Problem = ex.Message);
            throw;
        }
    }

    // =========================================================================================
    // Reading lists
    // =========================================================================================

    /// <summary>Reads everything again from Spotify in the background. False when it is already reading.</summary>
    public bool StartReading(string user)
    {
        if (_reading.TryGetValue(user, out var now) && now.Busy) return false;
        _reading[user] = new ReadView(true, "Starting", null, null);
        _ = Task.Run(() => ReadSpotifyAsync(user, _stopping), CancellationToken.None);
        return true;
    }

    internal async Task ReadSpotifyAsync(string user, CancellationToken ct)
    {
        var gate = Gate(user);
        await gate.WaitAsync(ct);
        try
        {
            void Step(string words) => _reading[user] = new ReadView(true, words, null, null);
            var token = await TokenAsync(user, ct);
            var seen = new HashSet<string>(StringComparer.Ordinal);

            Step("Reading your liked songs");
            var liked = await _spotify.LikedSongsAsync(token, (done, total) => Step($"Reading your liked songs ({done} of {total})"), ct);
            seen.Add(LikedId);
            _store.Save(new ImportList
            {
                Id = LikedId, Owner = user, Name = "Liked Songs", Source = ImportSources.SpotifyLiked,
                Total = liked.Count, Tracks = liked.ToList(), ReadUtc = Clock(),
            });

            Step("Reading your playlists");
            token = await TokenAsync(user, ct);
            var account = _accounts.Get(user);
            var playlists = await _spotify.PlaylistsAsync(token, ct);
            var index = 0;
            foreach (var playlist in playlists)
            {
                index++;
                var id = $"spotify-{playlist.Id}";
                if (!seen.Add(id)) continue;
                Step($"Reading playlist {index} of {playlists.Count}: {playlist.Name}");
                var existing = _store.Get(user, id);
                // Unchanged since the last read: Spotify's snapshot says so, and nothing is fetched.
                if (existing is { Gone: false, Partial: null } && existing.SnapshotId == playlist.SnapshotId && playlist.SnapshotId is not null)
                {
                    _store.Update(user, id, list => { list.Name = playlist.Name; list.ImageUrl = playlist.ImageUrl; });
                    continue;
                }
                token = await TokenAsync(user, ct);
                var mine = playlist.Collaborative || string.Equals(playlist.OwnerId, account?.SpotifyId, StringComparison.Ordinal);
                var read = await ReadPlaylistAsync(token, playlist, mine, ct);
                _store.Save(new ImportList
                {
                    Id = id, Owner = user, Name = playlist.Name, Source = ImportSources.SpotifyPlaylist, SourceRef = playlist.Id,
                    By = mine ? null : playlist.OwnerName, ImageUrl = playlist.ImageUrl, SnapshotId = read.Partial is null ? playlist.SnapshotId : null,
                    Total = playlist.Total, Tracks = read.Tracks.ToList(), Partial = read.Partial, ReadUtc = Clock(),
                });
                await Task.Delay(BetweenPlaylists, ct);
            }

            // A playlist no longer on the account: gone, unless it is kept or fetching, which keeps it.
            foreach (var list in _store.ListsOf(user).Where(list => list.Source == ImportSources.SpotifyPlaylist && !seen.Contains(list.Id)))
            {
                if (list.KeepPlaylist || list.GetMissing) _store.Update(user, list.Id, l => l.Gone = true);
                else _store.Remove(user, list.Id);
            }
            _store.UpdateAll(user, list => { if (seen.Contains(list.Id)) list.Gone = false; });

            Step("Matching against your library");
            await MatchAndFollowAsync(user, ct);
            _reading[user] = new ReadView(false, null, null, Clock());
            _logger.LogInformation("Read {Liked} liked songs and {Playlists} playlists from Spotify for {User}",
                liked.Count, playlists.Count, user);
        }
        catch (OperationCanceledException) when (ct.IsCancellationRequested)
        {
            _reading[user] = new ReadView(false, null, "Stopped: Octo is shutting down.", Clock());
        }
        catch (Exception ex)
        {
            _logger.LogWarning("Reading Spotify for {User} failed: {Message}", user, ex.Message);
            _reading[user] = new ReadView(false, null, ex is SpotifyException ? ex.Message
                : $"Reading Spotify failed: {AcquisitionTracker.UserSafe(ex.Message) ?? "something went wrong"}.", Clock());
        }
        finally { gate.Release(); }
    }

    private const string LikedId = "spotify-liked";

    private async Task<(IReadOnlyList<ImportTrack> Tracks, string? Partial)> ReadPlaylistAsync(string token,
        SpotifyPlaylistInfo playlist, bool mine, CancellationToken ct)
    {
        var items = await _spotify.PlaylistItemsAsync(token, playlist.Id, null, ct);
        if (!items.Hidden) return (items.Tracks, null);
        // Someone else's playlist: Spotify shows an app in development mode only the songs of
        // playlists the account made or works on. Its public page still lists the first 100.
        try
        {
            var page = await _links.ReadAsync($"https://open.spotify.com/playlist/{playlist.Id}", ct);
            var whole = !page.Capped && page.Tracks.Count >= playlist.Total;
            return (page.Tracks, whole ? null
                : $"Spotify shows Octo only the songs of playlists you made, so Octo read this one from its public page, which lists the first {SpotifyLinkReader.EmbedCap}. Import it from a file to get the rest.");
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _logger.LogDebug("Public page of playlist {Id} could not be read: {Message}", playlist.Id, ex.Message);
            return ([], mine
                ? "Spotify would not show this playlist's songs."
                : "Spotify shows Octo only the songs of playlists you made, and this one has no public page. Import it from a file instead.");
        }
    }

    /// <summary>A public playlist or album, by its link. No sign-in needed.</summary>
    public async Task<ImportActionResult> AddLinkAsync(string user, string link, CancellationToken ct)
    {
        if (SpotifyLinkReader.Parse(link) is not var (kind, id))
            return new(false, "That is not a link to a Spotify playlist or album.");
        var gate = Gate(user);
        await gate.WaitAsync(ct);
        try
        {
            var read = await _links.ReadAsync(link, ct);
            var listId = $"link-{kind}-{id}";
            if (RoomFor(user, [(listId, read.Tracks.Count)]) is { } full) return new(false, full);
            _store.Save(new ImportList
            {
                Id = listId, Owner = user, Name = read.Name, Source = ImportSources.Link,
                SourceRef = $"https://open.spotify.com/{kind}/{id}", ImageUrl = read.ImageUrl, Total = read.Tracks.Count,
                Tracks = read.Tracks.ToList(), ReadUtc = Clock(),
                Partial = read.Capped
                    ? $"A link shows only the first {SpotifyLinkReader.EmbedCap} songs. Connect Spotify or import a file for the whole list."
                    : null,
            });
            await MatchAndFollowAsync(user, ct);
            return new(true, $"Added {read.Name}, {read.Tracks.Count} songs.", ListId: listId, Count: read.Tracks.Count);
        }
        catch (SpotifyException ex) { return new(false, ex.Message); }
        finally { gate.Release(); }
    }

    /// <summary>The lists in an exported file: CSV, Spotify's data export (JSON), or its zip.</summary>
    public async Task<ImportActionResult> AddFileAsync(string user, string fileName, Stream content, CancellationToken ct)
    {
        IReadOnlyList<FileList> lists;
        try { lists = ImportFileReader.Read(fileName, content); }
        catch (Exception ex) when (ex is FormatException or System.Text.Json.JsonException or InvalidDataException)
        {
            // Octo's own words for a file it refuses; a parser's are cut to their first plain sentence.
            return new(false, $"Octo could not read that file: {(ex is FormatException ? ex.Message : (AcquisitionTracker.UserSafe(ex.Message) ?? "it is not a file Octo knows") + ".")}");
        }
        if (lists.Count == 0) return new(false, "That file held no songs Octo could read. It needs a title and an artist for each song.");
        var gate = Gate(user);
        await gate.WaitAsync(ct);
        try
        {
            if (RoomFor(user, lists.Select(list => ($"file-{Hash(list.Name)}", list.Tracks.Count)).ToList()) is { } full)
                return new(false, full);
            string? first = null;
            foreach (var list in lists)
            {
                // By name, so a newer export of the same playlist updates it rather than adding a second.
                var id = $"file-{Hash(list.Name)}";
                first ??= id;
                _store.Save(new ImportList
                {
                    Id = id, Owner = user, Name = list.Name, Source = ImportSources.File, SourceRef = Path.GetFileName(fileName),
                    Total = list.Tracks.Count, Tracks = list.Tracks.ToList(), ReadUtc = Clock(),
                });
            }
            await MatchAndFollowAsync(user, ct);
            var songs = lists.Sum(list => list.Tracks.Count);
            return new(true, lists.Count == 1 ? $"Added {lists[0].Name}, {songs} songs." : $"Added {lists.Count} lists, {songs} songs.",
                ListId: first, Count: songs);
        }
        finally { gate.Release(); }
    }

    /// <summary>Lists one person may keep, from files and links.</summary>
    internal const int MaxLists = 500;

    /// <summary>Songs across one person's lists.</summary>
    internal const int MaxSongs = 250_000;

    /// <summary>Why these lists would not fit beside the person's others, or null. A list that
    /// takes the place of one with the same id counts once.</summary>
    private string? RoomFor(string user, IReadOnlyList<(string Id, int Songs)> incoming)
    {
        var ids = incoming.Select(list => list.Id).ToHashSet(StringComparer.Ordinal);
        var kept = _store.ListsOf(user).Where(list => !ids.Contains(list.Id)).ToList();
        if (kept.Count + ids.Count > MaxLists)
            return $"That would make more than {MaxLists} lists, the most Octo keeps for one person. Remove some first.";
        if (kept.Sum(list => list.Tracks.Count) + incoming.Sum(list => list.Songs) > MaxSongs)
            return $"That would make more than 250,000 songs across your lists, the most Octo keeps for one person. Remove some first.";
        return null;
    }

    private static string Hash(string text) =>
        Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(text.ToLowerInvariant())))[..12].ToLowerInvariant();

    /// <summary>Reads one list again from where it came from: the account for a Spotify list, the page for a link.</summary>
    public async Task<ImportActionResult> RefreshAsync(string user, string id, CancellationToken ct)
    {
        if (_store.Get(user, id) is not { } list) return new(false, "There is no such list.");
        switch (list.Source)
        {
            case ImportSources.SpotifyLiked:
            case ImportSources.SpotifyPlaylist:
                return StartReading(user)
                    ? new(true, "Reading your Spotify lists again.")
                    : new(true, "Octo is already reading your Spotify lists.");
            case ImportSources.Link when list.SourceRef is { } link:
                return await AddLinkAsync(user, link, ct);
            default:
                var gate = Gate(user);
                await gate.WaitAsync(ct);
                try { await MatchAndFollowAsync(user, ct); }
                finally { gate.Release(); }
                return new(true, "Matched against your library again.");
        }
    }

    // =========================================================================================
    // What to do with a list
    // =========================================================================================

    /// <summary>Keep the list as a Navidrome playlist, or stop. Stopping leaves the playlist where it is.</summary>
    public async Task<ImportActionResult> SetPlaylistAsync(string user, string id, bool on, CancellationToken ct)
    {
        if (!_store.Update(user, id, list => list.KeepPlaylist = on)) return new(false, "There is no such list.");
        if (!on) return new(true, "Octo no longer changes this playlist. It stays in Navidrome as it is.");
        var result = await WritePlaylistAsync(user, id, ct);
        return result ?? new(false, "Navidrome could not be reached to make the playlist. Octo tries again shortly.");
    }

    /// <summary>Fetch the list's missing songs, and every song added to it later, or stop.</summary>
    public ImportActionResult SetGetMissing(string user, string id, bool on)
    {
        if (_store.Get(user, id) is not { } list) return new(false, "There is no such list.");
        // Never on a guess: a list nobody could match against the library would fetch songs already there.
        if (on && list.MatchedUtc is null) return new(false, _matcher.Problem ?? "Octo has not checked this list against your library yet. Try again in a moment.");
        _store.Update(user, id, l => l.GetMissing = on);
        if (on)
        {
            var queued = _queue.Add(user, list.Tracks.Where(track => track.LibraryId is null));
            return new(true, queued == 0 ? "Nothing new to fetch: every song is in your library or already queued."
                : $"Queued {queued} {(queued == 1 ? "song" : "songs")}. Octo fetches up to {_settings.CurrentValue.EffectiveSongsPerHour} an hour.", Count: queued);
        }
        // Songs still waiting that no other fetching list wants go back out of the queue.
        var wanted = _store.ListsOf(user).Where(l => l.GetMissing).SelectMany(l => l.Tracks).Select(t => t.Key).ToHashSet(StringComparer.Ordinal);
        var cancelled = _queue.CancelQueued(user, list.Tracks.Select(t => t.Key).Where(key => !wanted.Contains(key)));
        return new(true, cancelled == 0 ? "Stopped fetching this list." : $"Stopped fetching this list; {cancelled} waiting {(cancelled == 1 ? "song was" : "songs were")} taken back.", Count: cancelled);
    }

    /// <summary>Queue chosen songs of a list, whether or not the list fetches the rest.</summary>
    public ImportActionResult GetSongs(string user, string id, IReadOnlyCollection<string> keys)
    {
        if (_store.Get(user, id) is not { } list) return new(false, "There is no such list.");
        var wanted = keys.ToHashSet(StringComparer.Ordinal);
        var songs = list.Tracks.Where(track => track.LibraryId is null && wanted.Contains(track.Key)).ToList();
        _queue.Retry(user, songs.Select(song => song.Key));
        var queued = _queue.Add(user, songs);
        return new(true, $"Queued {songs.Count} {(songs.Count == 1 ? "song" : "songs")}.", Count: queued);
    }

    public ImportActionResult Remove(string user, string id)
    {
        if (_store.Get(user, id) is not { } list) return new(false, "There is no such list.");
        if (list.GetMissing) SetGetMissing(user, id, false);
        _store.Remove(user, id);
        return new(true, list.PlaylistId is null ? $"Removed {list.Name}."
            : $"Removed {list.Name}. Its playlist stays in Navidrome; delete it there if you no longer want it.");
    }

    // ---- The trickle -------------------------------------------------------------------------

    public ImportActionResult Pause(string user, bool paused)
    {
        _queue.SetPaused(user, paused);
        return new(true, paused ? "Paused. A song already running finishes." : "Going again.");
    }

    public ImportActionResult Retry(string user, IReadOnlyCollection<string>? keys)
    {
        var chosen = keys is { Count: > 0 } ? keys
            : _queue.Snapshot(user).Where(job => job.State == ImportTrackStates.NotFound).Select(job => job.Key).ToList();
        var count = _queue.Retry(user, chosen);
        return new(true, count == 0 ? "Nothing to try again." : $"Trying {count} {(count == 1 ? "song" : "songs")} again.", Count: count);
    }

    public ImportActionResult Skip(string user, IReadOnlyCollection<string> keys)
    {
        var count = _queue.Skip(user, keys);
        return new(true, count == 0 ? "Nothing was waiting to skip." : $"Skipped {count} {(count == 1 ? "song" : "songs")}.", Count: count);
    }

    public ImportActionResult ClearFinished(string user)
    {
        var count = _queue.ClearFinished(user);
        return new(true, $"Cleared {count} finished {(count == 1 ? "song" : "songs")}.", Count: count);
    }

    // =========================================================================================
    // Matching and following
    // =========================================================================================

    /// <summary>Matches every list of this person, queues new missing songs of fetching lists, and writes their playlists.</summary>
    internal async Task MatchAndFollowAsync(string user, CancellationToken ct)
    {
        var lists = _store.ListsOf(user);
        var tracks = lists.SelectMany(list => list.Tracks).ToList();
        if (!await _matcher.MatchAsync(tracks, ct)) return;
        var found = tracks.GroupBy(track => track.Key, StringComparer.Ordinal)
            .ToDictionary(group => group.Key, group => group.First().LibraryId, StringComparer.Ordinal);
        var now = Clock();
        _store.UpdateAll(user, list =>
        {
            foreach (var track in list.Tracks)
                if (found.TryGetValue(track.Key, out var id)) track.LibraryId = id;
            list.MatchedUtc = now;
        });
        foreach (var list in _store.ListsOf(user))
        {
            if (list.GetMissing) _queue.Add(user, list.Tracks.Where(track => track.LibraryId is null));
            if (list.KeepPlaylist) _dirtyPlaylists[$"{user}\n{list.Id}"] = 0;
        }
        await WriteDirtyPlaylistsAsync(ct);
    }

    private async Task<ImportActionResult?> WritePlaylistAsync(string user, string id, CancellationToken ct)
    {
        if (_store.Get(user, id) is not { KeepPlaylist: true } list) return null;
        var result = await _playlists.SyncAsync(list, ct);
        if (!result.Ok)
        {
            _dirtyPlaylists[$"{user}\n{id}"] = 0;
            return null;
        }
        _store.Update(user, id, l =>
        {
            l.PlaylistId = result.PlaylistId;
            l.PlaylistNote = result.Note;
            if (result.Deleted) l.KeepPlaylist = false;
        });
        return result.Deleted
            ? new(true, "The playlist was deleted in Navidrome, so Octo stopped keeping it. Turn it on again to make a new one.")
            : new(true, $"The playlist holds the {result.Songs} {(result.Songs == 1 ? "song" : "songs")} in your library. Octo adds the rest as they arrive.");
    }

    private async Task WriteDirtyPlaylistsAsync(CancellationToken ct)
    {
        foreach (var entry in _dirtyPlaylists.Keys.ToList())
        {
            if (!_dirtyPlaylists.TryRemove(entry, out _)) continue;
            var split = entry.IndexOf('\n');
            try { await WritePlaylistAsync(entry[..split], entry[(split + 1)..], ct); }
            catch (Exception ex) when (ex is not OperationCanceledException)
            {
                _logger.LogWarning("Could not write the playlist for {Entry}: {Message}", entry.Replace('\n', '/'), ex.Message);
                _dirtyPlaylists[entry] = 0;
            }
        }
    }

    /// <summary>A song the trickle finished: when it was fetched, the library is read again on the next pass.</summary>
    private void OnFetched(TrickleJob job)
    {
        if (job.State != ImportTrackStates.Done) return;
        _matcher.Forget();
        _rematch[job.Owner] = 0;
    }

    private SemaphoreSlim Gate(string user) => _gates.GetOrAdd(user, _ => new SemaphoreSlim(1, 1));

    /// <summary>One pass of the loop: matches people the trickle brought songs, writes playlists, and reads stale lists again.</summary>
    internal async Task LoopOnceAsync(CancellationToken ct)
    {
        foreach (var user in _rematch.Keys.ToList())
        {
            if (!_rematch.TryRemove(user, out _)) continue;
            var gate = Gate(user);
            if (!await gate.WaitAsync(0, ct)) { _rematch[user] = 0; continue; }
            try { await MatchAndFollowAsync(user, ct); }
            finally { gate.Release(); }
        }
        await WriteDirtyPlaylistsAsync(ct);

        var hours = _settings.CurrentValue.RefreshHours;
        if (hours <= 0) return;
        var stale = Clock() - TimeSpan.FromHours(hours);
        var followed = _store.All().Where(list => (list.KeepPlaylist || list.GetMissing) && !list.Gone && list.ReadUtc < stale).ToList();
        foreach (var user in followed.Where(list => list.Source is ImportSources.SpotifyLiked or ImportSources.SpotifyPlaylist)
                     .Select(list => list.Owner).Distinct(StringComparer.OrdinalIgnoreCase))
            if (_accounts.Get(user) is { Problem: null }) StartReading(user);
        foreach (var list in followed.Where(list => list.Source == ImportSources.Link && list.SourceRef is not null))
        {
            var read = await AddLinkAsync(list.Owner, list.SourceRef!, ct);
            // A link that no longer reads is tried again next time round, not every pass.
            if (!read.Ok)
            {
                _logger.LogDebug("Could not read {Link} again: {Message}", list.SourceRef, read.Message);
                _store.Update(list.Owner, list.Id, l => l.ReadUtc = Clock());
            }
        }
    }

    protected override async Task ExecuteAsync(CancellationToken stoppingToken)
    {
        _stopping = stoppingToken;
        while (!stoppingToken.IsCancellationRequested)
        {
            // Per-pass catch: BackgroundServiceExceptionBehavior defaults to StopHost.
            try { await LoopOnceAsync(stoppingToken); }
            catch (OperationCanceledException) when (stoppingToken.IsCancellationRequested) { break; }
            catch (Exception ex) { _logger.LogError(ex, "Import loop failed"); }
            try { await Task.Delay(LoopEvery, stoppingToken); } catch (OperationCanceledException) { break; }
        }
    }
}
