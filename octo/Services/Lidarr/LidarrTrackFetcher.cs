using System.Collections.Concurrent;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.Fingerprint;
using Octo.Services.Library;
using Octo.Services.Subsonic;

namespace Octo.Services.Lidarr;

/// <param name="LosslessOnly">Wait for a lossless file; any file will do otherwise.</param>
/// <param name="OriginalPath">The library file this replaces, when there is one: its tags can name
/// the album exactly, and Lidarr may already be the one managing it.</param>
/// <param name="Release">A release picked in Find songs, grabbed instead of Lidarr's own search.</param>
public sealed record LidarrTrackRequest(string Artist, string Title, string? Album, int? DurationSeconds,
    bool LosslessOnly, string? OriginalPath = null, LidarrReleasePick? Release = null);

public interface ILidarrTrackFetcher
{
    /// <summary>
    /// Fetches one song through Lidarr and copies it into <paramref name="destinationDirectory"/>,
    /// returning the copy's path. Throws <see cref="FileNotFoundException"/> when Lidarr has no
    /// album for it, or found no good enough copy in time.
    /// </summary>
    Task<string> FetchAsync(LidarrTrackRequest request, string destinationDirectory, CancellationToken ct = default);
}

/// <summary>
/// One song through Lidarr, for a replacement. Lidarr only fetches whole albums, so this borrows
/// the album: it searches it, copies out the one song asked for, then deletes every file that
/// search brought in and puts the album's monitoring back the way it was, so nothing else lands
/// in the library and Lidarr does not fetch the album again. The copy then goes through the same
/// checks and the same swap as a Soulseek download.
///
/// Two songs of one album share one search; the clean up waits for the last of them.
/// </summary>
public sealed class LidarrTrackFetcher(
    LidarrClient client,
    IOptionsMonitor<LidarrSettings> settings,
    NavidromeIdentityService navIdentity,
    IConfiguration configuration,
    LidarrAlbumClaims claims,
    ILogger<LidarrTrackFetcher> logger,
    MusicBrainzClient? musicBrainz = null) : ILidarrTrackFetcher
{
    internal TimeSpan Poll { get; set; } = TimeSpan.FromSeconds(10);

    internal const string ManagedText =
        "Lidarr manages this file itself, so it was left to Lidarr: a quality profile that wants lossless upgrades it there.";

    private static readonly HashSet<string> LosslessSuffixes = new(StringComparer.OrdinalIgnoreCase)
        { ".flac", ".wav", ".aiff", ".aif", ".ape", ".wv" };

    private readonly ConcurrentDictionary<string, Session> _sessions = new(StringComparer.OrdinalIgnoreCase);
    private readonly object _gate = new();

    private sealed class Session(LidarrAlbumCandidate candidate)
    {
        public LidarrAlbumCandidate Candidate { get; } = candidate;
        public int Users;
        public Task<IReadOnlyList<LidarrImportedTrack>>? Before;
        public Task<LidarrSearchStarted>? Search;
    }

    public async Task<string> FetchAsync(LidarrTrackRequest request, string destinationDirectory, CancellationToken ct = default)
    {
        var current = settings.CurrentValue;
        if (!UpgradeSources.LidarrSetUp(current))
            throw new InvalidOperationException("Lidarr is not set up: it needs an address, a key, a root folder and both profiles.");

        var candidate = await FindAlbumAsync(request, ct)
            ?? throw new FileNotFoundException($"Lidarr knows no album with '{request.Artist} - {request.Title}' on it.");
        var key = candidate.ForeignAlbumId;
        if (claims.HeartBusy(key))
            throw new InvalidOperationException("Lidarr is fetching this album for a heart right now; try again once it lands.");

        claims.UpgradeStarted(key);
        Session session;
        lock (_gate)
        {
            session = _sessions.GetOrAdd(key, _ => new Session(candidate));
            session.Users++;
            session.Before ??= SnapshotAsync(candidate);
        }
        try
        {
            return await FetchFromAsync(session, request, destinationDirectory, current, ct);
        }
        finally
        {
            bool last;
            lock (_gate)
            {
                last = --session.Users == 0;
                if (last) _sessions.TryRemove(new KeyValuePair<string, Session>(key, session));
            }
            if (last) await CleanUpAsync(session);
            claims.UpgradeEnded(key);
        }
    }

    private async Task<string> FetchFromAsync(Session session, LidarrTrackRequest request, string destinationDirectory,
        LidarrSettings current, CancellationToken ct)
    {
        var octoRoot = navIdentity.EffectiveDownloadPath(configuration["Library:DownloadPath"] ?? "/music");
        var before = await session.Before!;
        var beforeIds = before.Where(t => t.TrackFileId > 0).Select(t => t.TrackFileId).ToHashSet();

        // What Lidarr had for this song before anything was searched.
        if (Match(before.Where(t => t.HasFile), request) is { } had && Visible(had, current, octoRoot) is { } hadPath)
        {
            if (request.OriginalPath is { } original && SamePath(hadPath, original))
                throw new InvalidOperationException(ManagedText);
            if (IsLossless(had))
                throw new InvalidOperationException(
                    $"Lidarr already has a lossless copy of this song in the library, at {hadPath}; the lossy one is a duplicate of it.");
        }

        Task<LidarrSearchStarted> search;
        lock (_gate) search = session.Search ??= client.StartAlbumSearchAsync(session.Candidate, CancellationToken.None, request.Release);
        var started = await search;
        logger.LogInformation("Lidarr is searching '{Artist} - {Album}' for a copy of '{Title}'",
            session.Candidate.Artist, session.Candidate.Title, request.Title);

        var timeout = TimeSpan.FromSeconds(Math.Max(1, current.ImportTimeoutSeconds));
        var deadline = DateTime.UtcNow + timeout;
        while (true)
        {
            ct.ThrowIfCancellationRequested();
            var tracks = await client.GetAlbumTracksAsync(started.AlbumId, ct);
            var fresh = tracks.Where(t => t.HasFile && t.TrackFileId > 0 && !beforeIds.Contains(t.TrackFileId)
                                          && (!request.LosslessOnly || IsLossless(t)));
            if (Match(fresh, request) is { } found && Visible(found, current, octoRoot) is { } path)
            {
                Directory.CreateDirectory(destinationDirectory);
                var copy = Path.Combine(destinationDirectory, $"lidarr-{Guid.NewGuid():N}{Path.GetExtension(path)}");
                File.Copy(path, copy);
                logger.LogInformation("Lidarr brought '{Artist} - {Title}' ({Quality}); copied {Path} for the replacement",
                    request.Artist, request.Title, found.Quality ?? Path.GetExtension(path), path);
                return copy;
            }
            if (DateTime.UtcNow >= deadline) break;
            await Task.Delay(Poll, ct);
        }
        throw new FileNotFoundException(
            $"Lidarr found no {(request.LosslessOnly ? "lossless " : "")}copy of '{request.Artist} - {request.Title}' within {Math.Max(1, (int)timeout.TotalMinutes)} minutes.");
    }

    /// <summary>
    /// Takes back what the borrowed search brought in: every file that is new since it started
    /// (the copied songs are in the library under Octo's name by now), and the monitoring Octo
    /// switched on. Best effort; whatever fails is logged.
    /// </summary>
    private async Task CleanUpAsync(Session session)
    {
        if (session.Search is null || session.Before is null) return;
        try
        {
            var started = await session.Search;
            var before = (await session.Before).Where(t => t.TrackFileId > 0).Select(t => t.TrackFileId).ToHashSet();
            var now = await client.GetAlbumTracksAsync(started.AlbumId);
            var added = now.Where(t => t.TrackFileId > 0 && !before.Contains(t.TrackFileId))
                .Select(t => t.TrackFileId).Distinct().ToList();
            foreach (var id in added)
            {
                try { await client.DeleteTrackFileAsync(id); }
                catch (Exception ex) { logger.LogWarning("Could not delete Lidarr track file {Id}: {Message}", id, ex.Message); }
            }
            if (!started.WasMonitored) await client.SetAlbumsMonitoredAsync([started.AlbumId], false);
            logger.LogInformation("Lidarr album '{Artist} - {Album}': removed {Count} files the search brought in{Monitor}",
                session.Candidate.Artist, session.Candidate.Title, added.Count,
                started.WasMonitored ? "" : ", and stopped monitoring it again");
        }
        catch (Exception ex)
        {
            logger.LogWarning("Could not tidy Lidarr album '{Artist} - {Album}' after an upgrade: {Message}",
                session.Candidate.Artist, session.Candidate.Title, ex.Message);
        }
    }

    private async Task<IReadOnlyList<LidarrImportedTrack>> SnapshotAsync(LidarrAlbumCandidate candidate)
    {
        var existing = await client.FindAlbumAsync(candidate.ForeignAlbumId);
        return existing is null ? [] : await client.GetAlbumTracksAsync(existing.Id);
    }

    /// <summary>The album: from the original's own release group tag, then by its album name, then
    /// the studio album MusicBrainz files the song under.</summary>
    internal async Task<LidarrAlbumCandidate?> FindAlbumAsync(LidarrTrackRequest request, CancellationToken ct)
    {
        if (ReleaseGroupOf(request.OriginalPath) is { } tagged
            && await client.ResolveAlbumByForeignIdAsync(tagged, ct) is { } byTag)
            return byTag;
        if (!string.IsNullOrWhiteSpace(request.Album))
        {
            try { return await client.ResolveAlbumAsync(request.Artist, request.Album, null, ct); }
            catch (InvalidOperationException) { /* no single match by name; MusicBrainz next */ }
        }
        if (musicBrainz is not null
            && await musicBrainz.FindStudioAlbumAsync(request.Artist, request.Title, ct) is { } studio)
            return await client.ResolveAlbumByForeignIdAsync(studio, ct);
        return null;
    }

    private static string? ReleaseGroupOf(string? path)
    {
        if (string.IsNullOrEmpty(path) || !File.Exists(path)) return null;
        try
        {
            using var file = TagLib.File.Create(path);
            return file.Tag.MusicBrainzReleaseGroupId is { Length: > 0 } id ? id : null;
        }
        catch
        {
            return null;
        }
    }

    /// <summary>The album's track for this song: by title, never by number, since the number came
    /// from whatever release the request was made from. Several by title: the nearest length.</summary>
    internal static LidarrImportedTrack? Match(IEnumerable<LidarrImportedTrack> tracks, LidarrTrackRequest request)
    {
        var byTitle = tracks.Where(t => SongIdentity.SameTitle(t.Title, request.Title, SongIdentity.StrictTitles).IsSame).ToList();
        if (byTitle.Count <= 1 || request.DurationSeconds is not > 0) return byTitle.FirstOrDefault();
        return byTitle.OrderBy(t => t.DurationSeconds is int d ? Math.Abs(d - request.DurationSeconds.Value) : int.MaxValue).First();
    }

    internal static bool IsLossless(LidarrImportedTrack track) =>
        track.Quality is { } quality
            ? quality.StartsWith("FLAC", StringComparison.OrdinalIgnoreCase) || quality.StartsWith("ALAC", StringComparison.OrdinalIgnoreCase)
              || quality.StartsWith("WAV", StringComparison.OrdinalIgnoreCase) || quality.StartsWith("APE", StringComparison.OrdinalIgnoreCase)
            : LosslessSuffixes.Contains(Path.GetExtension(track.Path ?? ""));

    /// <summary>Where Octo sees the track's file, or null when it cannot: Lidarr's path under its
    /// root folder maps onto Octo's library, and a path Octo shares as is also counts.</summary>
    private static string? Visible(LidarrImportedTrack track, LidarrSettings current, string octoRoot)
    {
        if (string.IsNullOrWhiteSpace(track.Path)) return null;
        try
        {
            var translated = LidarrHeartAcquisitionService.TranslateImportedPath(track.Path, current.RootFolderPath, octoRoot);
            if (File.Exists(translated)) return translated;
        }
        catch (InvalidOperationException)
        {
            // Under another root folder: an artist Lidarr already had keeps its own.
        }
        return File.Exists(track.Path) ? track.Path : null;
    }

    private static bool SamePath(string a, string b) =>
        string.Equals(Path.GetFullPath(a), Path.GetFullPath(b),
            OperatingSystem.IsWindows() ? StringComparison.OrdinalIgnoreCase : StringComparison.Ordinal);
}
