using System.Text;
using System.Text.Json;
using Octo.Models.Domain;
using Octo.Models.Search;
using Octo.Models.Subsonic;
using Octo.Services.YouTube;

namespace Octo.Services.Soulseek;

/// <summary>
/// Music metadata service for the YouTube-first / Soulseek-on-star architecture.
///
/// Radio queue creation is YouTube-only and lightweight: one yt-dlp search per
/// Last.fm similar track. We do NOT query Soulseek here — Soulseek is reserved
/// for the explicit "user wants to keep this" action (star / permanent download)
/// in SoulseekDownloadService.
///
/// External IDs are kept short (~30-80 chars) so Subsonic clients accept them.
/// Format:  yt|{videoId}|{artist_b64}|{title_b64}|{durationSec}
/// </summary>
public class SoulseekMetadataService : IMusicMetadataService
{
    public const string ProviderName = "soulseek";

    private readonly YouTubeResolver _youtube;
    private readonly ExternalIdRegistry _idRegistry;
    private readonly ILogger<SoulseekMetadataService> _logger;

    public SoulseekMetadataService(
        YouTubeResolver youtube,
        ExternalIdRegistry idRegistry,
        ILogger<SoulseekMetadataService> logger)
    {
        _youtube = youtube;
        _idRegistry = idRegistry;
        _logger = logger;
    }

    public Task<List<Song>> SearchSongsAsync(string query, int limit = 20)
    {
        if (string.IsNullOrWhiteSpace(query)) return Task.FromResult(new List<Song>());

        var (queryArtist, queryTitle) = ParseQuery(query);
        return SearchSongsByArtistTitleAsync(queryArtist, queryTitle ?? query, 1);
    }

    public Task<List<Song>> SearchSongsByArtistTitleAsync(string artist, string title, int limit = 1, int? durationSeconds = null)
    {
        if (string.IsNullOrWhiteSpace(artist) && string.IsNullOrWhiteSpace(title))
            return Task.FromResult(new List<Song>());

        // INSTANT placeholder. We do NOT call YouTube here — at queue-build time we'd
        // rate-limit ourselves into oblivion (Arpeggio fans out 5-10 search3 calls
        // per radio session). YouTube resolution is deferred to /rest/stream where
        // it happens once per actual playback, sequentially as the user advances.
        var externalId = _idRegistry.Register(new SoulseekRouting
        {
            // YouTubeId intentionally null — resolved lazily on play.
            Artist = artist,
            Title = title,
            Duration = durationSeconds
        });

        _logger.LogDebug("Placeholder song registered for '{Artist} - {Title}' (dur={Dur}) -> id {Id}",
            artist, title, durationSeconds, externalId);

        // 180 is the fallback when we don't know the real duration — most songs
        // are 3-5 min so it's a less-bad guess than 0 (which would prevent
        // clients from rendering a scrub bar at all).
        var effectiveDuration = durationSeconds ?? 180;

        return Task.FromResult(new List<Song>
        {
            new Song
            {
                Id = externalId,
                Title = title,
                Artist = artist,
                Album = "",
                Duration = effectiveDuration,
                IsLocal = false,
                ExternalProvider = ProviderName,
                ExternalId = externalId
            }
        });
    }

    /// <summary>
    /// Fire-and-forget background prewarm: resolve the YouTube videoId (and via
    /// shim's automatic prefetch, the stream URL) for the first <paramref name="topN"/>
    /// placeholder songs from a search. Without this, Arpeggi's ~10s HTTP timeout
    /// fires while the cold yt-dlp ytsearch1: + yt-dlp -g chain is still running,
    /// the client cancels, and external songs never play.
    ///
    /// Only the top hits matter: search clients render in order and users almost
    /// never click past the first screen of results. Resolving 150 placeholders
    /// would saturate the shim's yt-dlp gate and waste work.
    /// </summary>
    public Task PrewarmYouTubeIdsAsync(IEnumerable<Song> songs, int topN, CancellationToken ct = default)
    {
        var ids = songs
            .Where(s => !string.IsNullOrEmpty(s.Id))
            .Select(s => s.Id);
        return PrewarmYouTubeIdsForSongIdsAsync(ids, topN, ct);
    }

    public Task PrewarmYouTubeIdsForSongIdsAsync(IEnumerable<string> songIds, int topN, CancellationToken ct = default)
    {
        // Skip ids whose YouTube resolution is already cached on the routing —
        // those are already warm and don't need a yt-dlp roundtrip. This is the
        // path used by the scrobble-driven sliding window: as the user advances
        // through a queue most upcoming items will still be cold, but if they
        // jump back to one we resolved earlier we don't burn shim cycles re-doing it.
        var targets = songIds
            .Where(id => !string.IsNullOrEmpty(id))
            .Select(id => (id, routing: _idRegistry.Lookup(id)))
            .Where(t => t.routing != null
                        && string.IsNullOrEmpty(t.routing!.YouTubeId)
                        && t.routing.HasArtistTitle)
            .Take(topN)
            .ToList();
        if (targets.Count == 0) return Task.CompletedTask;

        // Concurrency cap below the shim's MAX_CONCURRENT_YTDLP=8 so we don't
        // monopolize it during a search burst (the shim is also serving any
        // in-flight /stream calls).
        var sem = new SemaphoreSlim(4);
        var tasks = targets.Select(async t =>
        {
            await sem.WaitAsync(ct);
            try
            {
                var routing = t.routing!;
                if (!string.IsNullOrEmpty(routing.YouTubeId)) return;
                var hit = await _youtube.SearchAsync($"{routing.Artist} {routing.Title}", ct);
                if (hit is { VideoId: { Length: > 0 } })
                {
                    routing.YouTubeId = hit.VideoId;
                    if (hit.Duration is int d) routing.Duration = d;
                }
            }
            catch { /* best-effort warm; never throw out of fire-and-forget */ }
            finally { sem.Release(); }
        });
        return Task.WhenAll(tasks);
    }

    public Task<List<Album>> SearchAlbumsAsync(string query, int limit = 20)
        => Task.FromResult(new List<Album>());

    public Task<List<Artist>> SearchArtistsAsync(string query, int limit = 20)
        => Task.FromResult(new List<Artist>());

    public async Task<SearchResult> SearchAllAsync(string query, int songLimit = 20, int albumLimit = 20, int artistLimit = 20)
    {
        var songs = await SearchSongsAsync(query, songLimit);
        return new SearchResult { Songs = songs, Albums = new List<Album>(), Artists = new List<Artist>() };
    }

    public Task<Song?> GetSongAsync(string externalProvider, string externalId)
    {
        if (!string.Equals(externalProvider, ProviderName, StringComparison.OrdinalIgnoreCase))
            return Task.FromResult<Song?>(null);

        var routing = _idRegistry.Lookup(externalId) ?? TryDecodeExternalId(externalId);
        if (routing is null) return Task.FromResult<Song?>(null);

        return Task.FromResult<Song?>(new Song
        {
            Id = externalId,
            Title = routing.Title ?? "",
            Artist = routing.Artist ?? "",
            Album = "",
            Duration = routing.Duration,
            IsLocal = false,
            ExternalProvider = ProviderName,
            ExternalId = externalId
        });
    }

    public Task<Album?> GetAlbumAsync(string externalProvider, string externalId)
        => Task.FromResult<Album?>(null);

    public Task<Artist?> GetArtistAsync(string externalProvider, string externalId)
        => Task.FromResult<Artist?>(null);

    public Task<List<Album>> GetArtistAlbumsAsync(string externalProvider, string externalId)
        => Task.FromResult(new List<Album>());

    public Task<List<ExternalPlaylist>> SearchPlaylistsAsync(string query, int limit = 20)
        => Task.FromResult(new List<ExternalPlaylist>());

    public Task<ExternalPlaylist?> GetPlaylistAsync(string externalProvider, string externalId)
        => Task.FromResult<ExternalPlaylist?>(null);

    public Task<List<Song>> GetPlaylistTracksAsync(string externalProvider, string externalId)
        => Task.FromResult(new List<Song>());

    // ====== Short opaque ID format ======
    // Pipe-delimited fields, base64url where needed.
    //   yt|{videoId}|{artistB64}|{titleB64}|{durationSec}
    // Total length ~30-80 chars depending on artist/title length.

    public static string EncodeExternalId(SoulseekRouting r)
    {
        var artist = r.Artist ?? "";
        var title = r.Title ?? "";
        var dur = r.Duration?.ToString() ?? "";
        return $"yt|{r.YouTubeId ?? ""}|{B64UrlEncode(artist)}|{B64UrlEncode(title)}|{dur}";
    }

    public static SoulseekRouting? TryDecodeExternalId(string? externalId)
    {
        if (string.IsNullOrWhiteSpace(externalId)) return null;
        var parts = externalId.Split('|');
        if (parts.Length < 4 || parts[0] != "yt") return null;
        try
        {
            int? duration = null;
            if (parts.Length >= 5 && int.TryParse(parts[4], out var d)) duration = d;
            return new SoulseekRouting
            {
                YouTubeId = parts[1],
                Artist = B64UrlDecode(parts[2]),
                Title = B64UrlDecode(parts[3]),
                Duration = duration
            };
        }
        catch
        {
            return null;
        }
    }

    private static string B64UrlEncode(string s)
        => Convert.ToBase64String(Encoding.UTF8.GetBytes(s))
            .TrimEnd('=').Replace('+', '-').Replace('/', '_');

    private static string B64UrlDecode(string s)
    {
        s = s.Replace('-', '+').Replace('_', '/');
        switch (s.Length % 4)
        {
            case 2: s += "=="; break;
            case 3: s += "="; break;
        }
        return Encoding.UTF8.GetString(Convert.FromBase64String(s));
    }

    private static (string artist, string? title) ParseQuery(string query)
    {
        var trimmed = query.Trim();
        var idx = trimmed.IndexOf(' ');
        if (idx > 0) return (trimmed[..idx], trimmed[(idx + 1)..].Trim());
        return (trimmed, null);
    }
}

public enum RoutingKind
{
    Song = 0,
    Album = 1,
    Artist = 2,
}

public class SoulseekRouting
{
    public RoutingKind Kind { get; set; } = RoutingKind.Song;
    public string? YouTubeId { get; set; }
    public string? Artist { get; set; }
    public string? Title { get; set; }
    public string? Album { get; set; }
    public int? Duration { get; set; }

    public bool HasYouTube => !string.IsNullOrEmpty(YouTubeId);
    public bool HasArtistTitle => !string.IsNullOrEmpty(Artist) && !string.IsNullOrEmpty(Title);
}
