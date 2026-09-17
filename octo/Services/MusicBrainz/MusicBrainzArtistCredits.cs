using System.Collections.Concurrent;
using System.Net;
using System.Text.Json;

namespace Octo.Services.MusicBrainz;

/// <summary>
/// The other names an artist is known by, looked up on MusicBrainz from a recording MBID.
///
/// Last.fm keys similarity on artist names, and names drift away from library tags when an
/// artist renames. MusicBrainz renamed Kanye West to "Ye", so a library tagged from
/// MusicBrainz says "Ye - Crack Music" while Last.fm only knows "Kanye West - Crack Music":
/// track.getSimilar returns nothing for the tag name, with or without autocorrect, and an
/// instant mix from that track comes back empty. MusicBrainz still records the name each
/// recording was released under (its artist credit) and the names an artist has used
/// (aliases of type "Artist name") - the mapping Last.fm is missing.
///
/// Only consulted after Last.fm has already come back empty, and only for a seed that has a
/// recording MBID, so it never has to guess between different artists who share a name.
/// </summary>
public sealed class MusicBrainzArtistCredits
{
    public const string ClientName = "musicbrainz";

    private const string BaseUrl = "https://musicbrainz.org/ws/2/";
    private const int MaxNames = 4;

    private readonly IHttpClientFactory _httpClientFactory;
    private readonly ILogger<MusicBrainzArtistCredits> _logger;
    private readonly ConcurrentDictionary<string, (DateTime Expiry, IReadOnlyList<string> Names)> _cache = new();
    private readonly SemaphoreSlim _gate = new(1, 1);
    private DateTime _lastRequestUtc = DateTime.MinValue;

    public MusicBrainzArtistCredits(IHttpClientFactory httpClientFactory, ILogger<MusicBrainzArtistCredits> logger)
    {
        _httpClientFactory = httpClientFactory;
        _logger = logger;
    }

    /// <summary>
    /// MusicBrainz asks anonymous clients for at most one request per second, and a lookup
    /// here costs two (the recording, then its first credited artist's aliases).
    /// </summary>
    internal TimeSpan MinRequestInterval { get; init; } = TimeSpan.FromMilliseconds(1100);

    /// <summary>
    /// Names the recording's primary artist is credited or known under, excluding
    /// <paramref name="currentArtist"/>. Empty when the MBID is not a GUID, MusicBrainz does not
    /// know it, or the request fails - the caller's result simply stays empty, as before.
    /// </summary>
    public async Task<IReadOnlyList<string>> GetAlternateArtistNamesAsync(string? recordingMbid,
        string currentArtist, CancellationToken cancellationToken = default)
    {
        if (string.IsNullOrWhiteSpace(recordingMbid) || !Guid.TryParse(recordingMbid, out var mbid))
            return [];

        var key = mbid.ToString();
        if (_cache.TryGetValue(key, out var cached) && cached.Expiry > DateTime.UtcNow)
            return Exclude(cached.Names, currentArtist);

        IReadOnlyList<string> names = [];
        try
        {
            using var recording = await GetJsonAsync($"recording/{key}?inc=artist-credits&fmt=json", cancellationToken);
            if (recording is not null)
            {
                var artistId = FirstCreditedArtistId(recording.RootElement);
                using var artist = artistId is null
                    ? null
                    : await GetJsonAsync($"artist/{artistId}?inc=aliases&fmt=json", cancellationToken);
                names = CollectNames(recording.RootElement, artist?.RootElement);
            }
        }
        catch (Exception ex) when (ex is HttpRequestException or JsonException
                                   || (ex is TaskCanceledException && !cancellationToken.IsCancellationRequested))
        {
            _logger.LogDebug(ex, "MusicBrainz artist credit lookup failed for recording {Mbid}", key);
            return [];
        }

        // A name found is stable for days; "nothing found" is cached briefly so a flaky
        // moment does not suppress the fallback for a whole day.
        var ttl = names.Count > 0 ? TimeSpan.FromDays(7) : TimeSpan.FromHours(1);
        _cache[key] = (DateTime.UtcNow.Add(ttl), names);
        return Exclude(names, currentArtist);
    }

    /// <summary>
    /// The first credit's name as printed on the release, its artist's current name, then that
    /// artist's "Artist name" aliases - in that order, because the printed credit is the name
    /// Last.fm most likely scrobbled the track under. Legal names and search hints are skipped:
    /// nobody scrobbles "Kanye Omari West".
    /// </summary>
    internal static IReadOnlyList<string> CollectNames(JsonElement recording, JsonElement? artist)
    {
        var names = new List<string>();

        if (recording.TryGetProperty("artist-credit", out var credits)
            && credits.ValueKind == JsonValueKind.Array
            && credits.GetArrayLength() > 0)
        {
            var first = credits[0];
            Add(names, first.TryGetProperty("name", out var credited) ? credited.GetString() : null);
            if (first.TryGetProperty("artist", out var creditArtist))
                Add(names, creditArtist.TryGetProperty("name", out var current) ? current.GetString() : null);
        }

        if (artist is { } a && a.TryGetProperty("aliases", out var aliases) && aliases.ValueKind == JsonValueKind.Array)
        {
            foreach (var alias in aliases.EnumerateArray())
            {
                var type = alias.TryGetProperty("type", out var t) ? t.GetString() : null;
                if (string.Equals(type, "Artist name", StringComparison.OrdinalIgnoreCase))
                    Add(names, alias.TryGetProperty("name", out var n) ? n.GetString() : null);
            }
        }

        return names.Take(MaxNames).ToList();
    }

    internal static string? FirstCreditedArtistId(JsonElement recording) =>
        recording.TryGetProperty("artist-credit", out var credits)
        && credits.ValueKind == JsonValueKind.Array
        && credits.GetArrayLength() > 0
        && credits[0].TryGetProperty("artist", out var artist)
        && artist.TryGetProperty("id", out var id)
        && Guid.TryParse(id.GetString(), out var guid)
            ? guid.ToString()
            : null;

    private static void Add(List<string> names, string? name)
    {
        if (string.IsNullOrWhiteSpace(name)) return;
        name = name.Trim();
        if (!names.Contains(name, StringComparer.OrdinalIgnoreCase)) names.Add(name);
    }

    private static IReadOnlyList<string> Exclude(IReadOnlyList<string> names, string currentArtist) =>
        names.Where(n => !string.Equals(n, currentArtist?.Trim(), StringComparison.OrdinalIgnoreCase)).ToList();

    private async Task<JsonDocument?> GetJsonAsync(string path, CancellationToken cancellationToken)
    {
        await _gate.WaitAsync(cancellationToken);
        try
        {
            var wait = _lastRequestUtc + MinRequestInterval - DateTime.UtcNow;
            if (wait > TimeSpan.Zero) await Task.Delay(wait, cancellationToken);
            _lastRequestUtc = DateTime.UtcNow;

            var client = _httpClientFactory.CreateClient(ClientName);
            using var response = await client.GetAsync(BaseUrl + path, cancellationToken);
            if (response.StatusCode == HttpStatusCode.NotFound) return null;
            response.EnsureSuccessStatusCode();
            await using var body = await response.Content.ReadAsStreamAsync(cancellationToken);
            return await JsonDocument.ParseAsync(body, cancellationToken: cancellationToken);
        }
        finally
        {
            _gate.Release();
        }
    }
}
