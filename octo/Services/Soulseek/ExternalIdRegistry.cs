using System.Collections.Concurrent;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace Octo.Services.Soulseek;

/// <summary>
/// Server-side registry that maps short opaque IDs (Navidrome-shaped 22-char base62)
/// to Soulseek/YouTube routing info. Subsonic clients are picky about song-id format —
/// some quietly drop entries with long pipe-delimited IDs from their play queues.
/// Translating to a short, alphabetic-looking key avoids that whole class of issue.
///
/// IDs are deterministic (sha256-derived) so the same routing always produces the
/// same id; this keeps caches/de-duplication on the client side stable across calls.
/// The dictionary is LRU-bounded so a single user can't grow it without limit.
/// </summary>
public class ExternalIdRegistry : IDisposable
{
    private const int MaxEntries = 10_000;

    private readonly ConcurrentDictionary<string, SoulseekRouting> _byId = new();
    private readonly LinkedList<string> _lru = new();
    private readonly object _lruLock = new();

    public string Register(SoulseekRouting routing)
    {
        var id = MakeShortId(routing);

        // A song row mints an album routing with no Deezer id (ConvertSongToJson), and it
        // hashes to the same id as the one an album search already resolved. Registering it
        // must not erase that id. Best-effort only: this is an optimization, and losing the
        // race just sends GetAlbumAsync down its cached name-lookup fallback.
        if (routing.ExternalAlbumId is null
            && _byId.TryGetValue(id, out var existing)
            && existing.ExternalAlbumId is not null)
        {
            routing.ExternalAlbumId = existing.ExternalAlbumId;
        }

        // Every search and every station playlist mints its songs again, and each mint is a
        // fresh routing. Without this, the length a lookup found for a song lasted until the
        // next search for it, which is why a row that had a length could lose it again.
        if (_byId.TryGetValue(id, out var previous) && !ReferenceEquals(previous, routing))
        {
            var (seconds, source) = SongLength.Shown(previous);
            if (source > routing.ShownDurationSource)
            {
                routing.ShownDuration = seconds;
                routing.ShownDurationSource = source;
            }
            // The same for the ISRC an album listing found: a search row for the same song
            // names none, and must not forget it.
            routing.Isrc ??= previous.Isrc;
            // And the catalog artist an artist search or page settled on: every album row
            // mints its artist again by name alone, and must not undo that choice.
            routing.ExternalArtistId ??= previous.ExternalArtistId;
            // And what the catalog said of its words: a search row mints the song again before
            // the catalog is asked.
            routing.ExplicitContent ??= previous.ExplicitContent;
        }

        _byId[id] = routing;
        Touch(id);
        Trim();
        Interlocked.Exchange(ref _dirty, 1);
        return id;
    }

    public SoulseekRouting? Lookup(string shortId)
    {
        if (_byId.TryGetValue(shortId, out var r))
        {
            Touch(shortId);
            return r;
        }
        return null;
    }

    /// <summary>
    /// The songs a song row filed under the album <paramref name="album"/> by
    /// <paramref name="artist"/>, newest first, one per recording. A row names its album
    /// after the song's album, or after its own title when it has none
    /// (SubsonicResponseBuilder.ConvertSongFields), so this matches on the same rule, plus
    /// the title alone, for a song whose album was filled in after its row named one.
    /// For getAlbum when the catalog cannot list the album (#59).
    /// </summary>
    public IReadOnlyList<(string Id, SoulseekRouting Routing)> SongsFiledUnder(string? artist, string? album,
        int limit = 50)
    {
        if (string.IsNullOrEmpty(artist) || string.IsNullOrEmpty(album)) return [];
        List<string> order;
        lock (_lruLock) order = _lru.ToList();

        var found = new List<(string Id, SoulseekRouting Routing)>();
        var recordings = new HashSet<string>(StringComparer.Ordinal);
        foreach (var id in order)
        {
            if (!_byId.TryGetValue(id, out var routing) || routing.Kind != RoutingKind.Song) continue;
            if (!string.Equals(routing.Artist, artist, StringComparison.Ordinal)) continue;
            var filedUnder = string.IsNullOrWhiteSpace(routing.Album) ? routing.Title : routing.Album;
            if (!string.Equals(filedUnder, album, StringComparison.Ordinal)
                && !string.Equals(routing.Title, album, StringComparison.Ordinal)) continue;
            var recording = Octo.Services.Common.SongIdentity.Key(routing.Title);
            if (!recordings.Add(recording.Length > 0 ? recording : id)) continue;
            found.Add((id, routing));
            if (found.Count >= limit) break;
        }
        return found;
    }

    /// <summary>
    /// Store a length for a song under <paramref name="shortId"/>, by the rules in
    /// <see cref="SongLength"/>. Looked up by id at write time rather than handed a routing,
    /// because a background lookup can outlive the routing it started from: a search that
    /// ran meanwhile replaced it, and a write to the old object would be lost.
    /// </summary>
    public bool RememberLength(string shortId, int? seconds, LengthSource source)
    {
        if (string.IsNullOrEmpty(shortId)
            || !_byId.TryGetValue(shortId, out var routing)
            || routing.Kind != RoutingKind.Song) return false;
        if (!SongLength.Remember(routing, seconds, source)) return false;
        Interlocked.Exchange(ref _dirty, 1);
        return true;
    }

    internal static string MakeShortId(SoulseekRouting r)
    {
        // Derive 22 base62 chars from sha256 of routing fields. Same input -> same id.
        // The Kind prefix is critical: a song "Drake - Hotline Bling" must hash to a
        // different id than the album "Hotline Bling" or the artist "Drake", or
        // getCoverArt would return the wrong scope's artwork.
        var seed = r.Kind switch
        {
            RoutingKind.Album  => $"k:album|a:{r.Artist}|al:{r.Album}",
            RoutingKind.Artist => $"k:artist|a:{r.Artist}",
            _                  => $"k:song|yt:{r.YouTubeId}|a:{r.Artist}|t:{r.Title}|d:{r.Duration}",
        };
        // A clean edit shares its artist, title and often its length with the explicit original,
        // so it gets a mark of its own. Only a clean one: the explicit and the unknown keep the
        // seed they always had, and with it every id already handed out.
        if (r.Kind != RoutingKind.Artist && string.Equals(r.Version, Octo.Models.Domain.ExplicitStatus.CleanWord, StringComparison.Ordinal))
            seed += $"|v:{Octo.Models.Domain.ExplicitStatus.CleanWord}";
        Span<byte> hash = stackalloc byte[32];
        SHA256.HashData(Encoding.UTF8.GetBytes(seed), hash);
        return ToBase62(hash, 22);
    }

    private const string Alphabet = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

    private static string ToBase62(ReadOnlySpan<byte> bytes, int length)
    {
        // Treat the first 16 bytes as a big integer and base62-encode it. We don't need
        // strict cryptographic uniqueness — just collision resistance within ~10k items.
        var value = new System.Numerics.BigInteger(bytes[..16], isUnsigned: true, isBigEndian: true);
        var sb = new StringBuilder(length);
        var b = new System.Numerics.BigInteger(62);
        while (sb.Length < length)
        {
            value = System.Numerics.BigInteger.DivRem(value, b, out var rem);
            sb.Append(Alphabet[(int)rem]);
            if (value.IsZero) break;
        }
        while (sb.Length < length) sb.Append('0');
        return sb.ToString()[..length];
    }

    private void Touch(string id)
    {
        lock (_lruLock)
        {
            _lru.Remove(id);
            _lru.AddFirst(id);
        }
    }

    private void Trim()
    {
        if (_byId.Count <= MaxEntries) return;
        lock (_lruLock)
        {
            while (_byId.Count > MaxEntries && _lru.Last is not null)
            {
                var oldest = _lru.Last.Value;
                _lru.RemoveLast();
                _byId.TryRemove(oldest, out _);
            }
        }
    }
    // ---- Persistence ---------------------------------------------------------
    //
    // The registry is the ONLY thing that knows an id is ours: ParseExternalId decides
    // "external" by looking it up here. So when this was memory-only, every restart made
    // every id a client still held look local, and those ids were relayed to Navidrome,
    // which has no such media and answers error 70 "data not found". Clients surface that
    // per play and per poll, which reads as a stream of errors from a working server.
    //
    // Ids are deterministic, so re-running the search that minted one brings it back. That
    // is a recovery, not a design: a queue built before a restart has no reason to search
    // again. Writing the map down is what makes an id outlive the process.

    private readonly string? _path;
    private readonly ILogger<ExternalIdRegistry>? _logger;
    private readonly Timer? _flushTimer;
    private int _dirty;

    /// <summary>A search registers well over a hundred routings, so flushing per write
    /// would turn one search into a hundred file writes. Coalesce instead: the window is
    /// short next to the restarts this exists to survive.</summary>
    private static readonly TimeSpan FlushInterval = TimeSpan.FromSeconds(10);

    private sealed record Persisted(string Id, SoulseekRouting Routing);

    public ExternalIdRegistry(string? path = null, ILogger<ExternalIdRegistry>? logger = null)
    {
        _path = string.IsNullOrWhiteSpace(path) ? null : path;
        _logger = logger;
        if (_path is null) return;

        Load();
        _flushTimer = new Timer(_ => Flush(), null, FlushInterval, FlushInterval);
    }

    private void Load()
    {
        try
        {
            if (!File.Exists(_path)) return;
            var entries = JsonSerializer.Deserialize<List<Persisted>>(File.ReadAllText(_path!));
            if (entries is null) return;

            // Stored most-recently-used first, so replaying in order rebuilds the same
            // eviction order rather than an arbitrary one.
            foreach (var e in entries)
            {
                if (string.IsNullOrEmpty(e.Id) || e.Routing is null) continue;
                _byId[e.Id] = e.Routing;
                lock (_lruLock) _lru.AddLast(e.Id);
            }
            Trim();
            _logger?.LogInformation("external id registry restored {Count} entries", _byId.Count);
        }
        catch (Exception ex)
        {
            // A registry that will not load is a cold start, not a failure to boot.
            _logger?.LogWarning("external id registry could not be read: {M}", ex.Message);
        }
    }

    private void Flush()
    {
        if (_path is null) return;
        if (Interlocked.Exchange(ref _dirty, 0) == 0) return;
        try
        {
            List<string> order;
            lock (_lruLock) order = _lru.ToList();

            var entries = new List<Persisted>(order.Count);
            foreach (var id in order)
                if (_byId.TryGetValue(id, out var routing)) entries.Add(new Persisted(id, routing));

            Directory.CreateDirectory(Path.GetDirectoryName(_path)!);
            var tmp = _path + ".tmp";
            File.WriteAllText(tmp, JsonSerializer.Serialize(entries));
            File.Move(tmp, _path, overwrite: true);
        }
        catch (Exception ex)
        {
            // Best-effort: losing a flush costs the ids minted since the last one, which
            // is the behaviour we already had. It must never take a request down.
            Interlocked.Exchange(ref _dirty, 1);
            _logger?.LogWarning("external id registry could not be written: {M}", ex.Message);
        }
    }

    public void Dispose()
    {
        _flushTimer?.Dispose();
        Flush();
    }
}
