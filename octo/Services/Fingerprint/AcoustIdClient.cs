using System.Text.Json;

namespace Octo.Services.Fingerprint;

/// <summary>
/// One recording AcoustID matched, with the MusicBrainz fields that come back in the same
/// lookup. There is no separate MusicBrainz client on purpose: AcoustID's metadata IS
/// MusicBrainz data, and asking for it via meta= costs nothing extra on a call already
/// being made and already inside a rate budget.
/// </summary>
public sealed record AcoustIdRecording(
    string RecordingId, string Title, IReadOnlyList<string> Artists, string? AlbumTitle, int? Year)
{
    public string ArtistCredit => string.Join(", ", Artists);
}

public sealed record AcoustIdResult(double Score, IReadOnlyList<AcoustIdRecording> Recordings);

public sealed record AcoustIdLookup(bool IsOk, string? Error, IReadOnlyList<AcoustIdResult> Results);

public sealed class AcoustIdClient
{
    /// <summary>
    /// A recording on a long-lived catalogue can carry hundreds of release groups, and one
    /// fingerprint can match several ids. Both are bounded so a pathological response cannot
    /// turn a download into a CPU burn.
    /// </summary>
    /// <summary>
    /// What to ask AcoustID to return: recordings gives title and artists, releasegroups the
    /// canonical album title, releases the year, and compress asks for a gzipped body (which is
    /// why the named client sets AutomaticDecompression).
    ///
    /// SPACE separated, and that is not cosmetic. FormUrlEncodedContent encodes a literal '+'
    /// as %2B, so writing these joined by '+' sends AcoustID one unknown token rather than four
    /// fields. It answers 200 with a perfectly good score and NO metadata at all, every result
    /// then has zero recordings, and the verdict is permanently Inconclusive: the feature
    /// accepts every file forever while looking like it is working. Verified against the live
    /// API on 2026-09-18, one track, both spellings.
    /// </summary>
    internal const string MetaFields = "recordings releasegroups releases compress";

    private const int MaxResults = 10;
    private const int MaxRecordings = 25;
    private const int MaxReleaseGroups = 50;

    private readonly IHttpClientFactory _httpClientFactory;
    private readonly ILogger<AcoustIdClient> _logger;

    public AcoustIdClient(IHttpClientFactory httpClientFactory, ILogger<AcoustIdClient> logger)
    {
        _httpClientFactory = httpClientFactory;
        _logger = logger;
    }

    /// <summary>
    /// Ask AcoustID what this fingerprint is. Returns null on any transport or parse
    /// failure, which the caller reads as "no verdict" and keeps the file.
    /// </summary>
    public async Task<AcoustIdLookup?> LookupAsync(string apiKey, string fingerprint,
        int durationSeconds, int timeoutSeconds)
    {
        // POST rather than GET: a Chromaprint fingerprint is kilobytes of base64 and would
        // blow past URL length limits.
        var form = new Dictionary<string, string>
        {
            ["client"] = apiKey,
            ["format"] = "json",
            ["duration"] = durationSeconds.ToString(),
            ["fingerprint"] = fingerprint,
            ["meta"] = MetaFields,
        };

        try
        {
            var client = _httpClientFactory.CreateClient(AcoustIdRateLimiter.ClientName);
            // Per-call rather than the client's own Timeout, so Soulseek:AcoustIdTimeoutSeconds
            // takes effect without a restart. The client keeps a generous ceiling behind this.
            using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(timeoutSeconds));
            using var response = await client.PostAsync("v2/lookup", new FormUrlEncodedContent(form), cts.Token);
            if (!response.IsSuccessStatusCode)
            {
                _logger.LogWarning("acoustid lookup answered {Status}", (int)response.StatusCode);
                return null;
            }

            using var doc = JsonDocument.Parse(await response.Content.ReadAsByteArrayAsync());
            return ParseLookup(doc.RootElement);
        }
        catch (Exception ex)
        {
            // Warning, not Debug. A parse failure here reads downstream as "accept every
            // file", so a silent degradation to a no-op is the worst outcome this feature
            // can have and must be visible in the log.
            _logger.LogWarning("acoustid lookup failed: {M}", ex.Message);
            return null;
        }
    }

    /// <summary>
    /// AcoustID answers some refusals with HTTP 200 and an error envelope, the exact shape
    /// that made over-budget Deezer calls silently destructive. IsOk is therefore read from
    /// the body, never from the status code.
    /// </summary>
    internal static AcoustIdLookup ParseLookup(JsonElement root)
    {
        var status = root.TryGetProperty("status", out var s) ? s.GetString() : null;
        if (!string.Equals(status, "ok", StringComparison.OrdinalIgnoreCase))
        {
            var message = root.TryGetProperty("error", out var err) && err.ValueKind == JsonValueKind.Object
                && err.TryGetProperty("message", out var m) ? m.GetString() : status;
            return new AcoustIdLookup(false, message ?? "unknown error", []);
        }

        var results = new List<AcoustIdResult>();
        if (root.TryGetProperty("results", out var rs) && rs.ValueKind == JsonValueKind.Array)
        {
            foreach (var result in rs.EnumerateArray().Take(MaxResults))
            {
                var score = result.TryGetProperty("score", out var sc) && sc.ValueKind == JsonValueKind.Number
                    ? sc.GetDouble() : 0d;
                results.Add(new AcoustIdResult(score, ParseRecordings(result)));
            }
        }

        return new AcoustIdLookup(true, null, results);
    }

    private static IReadOnlyList<AcoustIdRecording> ParseRecordings(JsonElement result)
    {
        if (!result.TryGetProperty("recordings", out var recs) || recs.ValueKind != JsonValueKind.Array)
            return [];

        var recordings = new List<AcoustIdRecording>();
        foreach (var rec in recs.EnumerateArray().Take(MaxRecordings))
        {
            var id = rec.TryGetProperty("id", out var i) ? i.GetString() ?? "" : "";
            var title = rec.TryGetProperty("title", out var t) ? t.GetString() ?? "" : "";

            var artists = new List<string>();
            if (rec.TryGetProperty("artists", out var arts) && arts.ValueKind == JsonValueKind.Array)
                foreach (var artist in arts.EnumerateArray())
                    if (artist.TryGetProperty("name", out var n) && n.GetString() is { Length: > 0 } name)
                        artists.Add(name);

            var (album, year) = PickRelease(rec);
            recordings.Add(new AcoustIdRecording(id, title, artists, album, year));
        }
        return recordings;
    }

    /// <summary>
    /// Prefer a plain studio album: a release group with no secondarytypes. Otherwise a
    /// compilation or a live album supplies the album name and year for a studio track.
    /// The year is the EARLIEST release in the chosen group, because a 2011 reissue is not
    /// the track's year.
    /// </summary>
    private static (string? Album, int? Year) PickRelease(JsonElement recording)
    {
        if (!recording.TryGetProperty("releasegroups", out var groups)
            || groups.ValueKind != JsonValueKind.Array) return (null, null);

        JsonElement? chosen = null;
        foreach (var group in groups.EnumerateArray().Take(MaxReleaseGroups))
        {
            chosen ??= group;
            var isAlbum = group.TryGetProperty("type", out var ty)
                && string.Equals(ty.GetString(), "Album", StringComparison.OrdinalIgnoreCase);
            var hasSecondary = group.TryGetProperty("secondarytypes", out var sec)
                && sec.ValueKind == JsonValueKind.Array && sec.GetArrayLength() > 0;
            if (isAlbum && !hasSecondary) { chosen = group; break; }
        }
        if (chosen is not { } pick) return (null, null);

        var album = pick.TryGetProperty("title", out var title) ? title.GetString() : null;

        int? year = null;
        if (pick.TryGetProperty("releases", out var releases) && releases.ValueKind == JsonValueKind.Array)
        {
            foreach (var release in releases.EnumerateArray())
            {
                if (!release.TryGetProperty("date", out var date) || date.ValueKind != JsonValueKind.Object) continue;
                if (!date.TryGetProperty("year", out var y) || y.ValueKind != JsonValueKind.Number) continue;
                var candidate = y.GetInt32();
                if (candidate > 0 && (year is null || candidate < year)) year = candidate;
            }
        }

        return (string.IsNullOrWhiteSpace(album) ? null : album, year);
    }
}
