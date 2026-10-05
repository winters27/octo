using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Common;
using Octo.Services.Fingerprint;
using Octo.Services.Subsonic;

namespace Octo.Services.Library;

/// <summary>One library row, as much of it as telling copies apart needs.</summary>
public sealed record LibraryTrack(string Id, string Title, string Artist, string Album, string RecordingId,
    string Suffix, int BitRate, int Duration)
{
    /// <summary>For a lossless file whose spectrum says it was made from a lossy one, what it was
    /// likely made from ("about 128 kbps MP3"). Only ever set on a copy inside a duplicate group.</summary>
    public string? TranscodedFrom { get; init; }

    /// <summary>The ISRCs its tags carry, which name the recording as a MusicBrainz id does.</summary>
    public IReadOnlyList<string> Isrcs { get; init; } = [];
}

/// <summary>What the shared duplicate finder reads of a track: who, what, how long, and its codes.</summary>
internal sealed class LibraryTrackFields : Octo.Services.Health.IHealthFields<LibraryTrack>
{
    public static readonly LibraryTrackFields Instance = new();

    public string Id(LibraryTrack song) => song.Id;
    public string Title(LibraryTrack song) => song.Title;
    public string? Artist(LibraryTrack song) => song.Artist.Length == 0 ? null : song.Artist;
    public string? Album(LibraryTrack song) => song.Album;
    public string? AlbumId(LibraryTrack song) => null;
    public string? AlbumArtist(LibraryTrack song) => null;
    public IReadOnlyList<string> Genres(LibraryTrack song) => [];
    public int? Year(LibraryTrack song) => null;
    public int? Disc(LibraryTrack song) => null;
    public int? Track(LibraryTrack song) => null;
    public int Seconds(LibraryTrack song) => song.Duration;
    public string? Format(LibraryTrack song) => song.Suffix;
    public bool Lossless(LibraryTrack song) => DuplicateScanWorker.IsLossless(song);
    public int? BitRate(LibraryTrack song) => song.BitRate;
    public int? BitDepth(LibraryTrack song) => null;
    public int? SampleRate(LibraryTrack song) => null;
    public IReadOnlyList<string> Isrcs(LibraryTrack song) => song.Isrcs;
    public string? RecordingId(LibraryTrack song) => song.RecordingId;
    public string? Cover(LibraryTrack song) => null;
}

/// <summary>Copies of one recording, the one worth keeping first.</summary>
public sealed record DuplicateGroup(string Key, IReadOnlyList<LibraryTrack> Tracks);

/// <summary>What the last walk found, for the dashboard.</summary>
public sealed record DuplicateScanResult(DateTime AtUtc, int Tracks, int Groups, int Added, bool Complete);

/// <summary>
/// Walks the library for recordings it holds more than once (#53) and hands them to the
/// Duplicates playlists. It only points them out: nothing here touches a file.
///
/// A duplicate is what Library health calls one, in the Octo app and on the dashboard alike
/// (<see cref="Octo.Services.Health.LibraryHealth.FindDuplicates{T}"/>): files whose tags name one
/// recording (an ISRC or a MusicBrainz recording id, lengths within 10 seconds), or files by the
/// same artist whose titles read the same, version and all (a live take, a remix or a radio edit
/// is never a copy), with lengths within 3 seconds. Until 2026-10-05 only files sharing a recording
/// id were grouped here, which found almost nothing in a library where few files carry one, and
/// disagreed with what Library health showed for the same library.
/// </summary>
public sealed class DuplicateScanWorker : BackgroundService
{
    internal const int PageSize = 500;

    /// <summary>100,000 tracks. A library larger than that is scanned in part, which can add
    /// questions but never settle one.</summary>
    internal const int MaxPages = 200;

    private static readonly TimeSpan FirstCheck = TimeSpan.FromMinutes(2);
    private static readonly TimeSpan CheckInterval = TimeSpan.FromMinutes(1);

    private static readonly HashSet<string> LosslessSuffixes = new(StringComparer.OrdinalIgnoreCase)
    {
        "flac", "alac", "wav", "aiff", "aif", "ape", "wv", "dsf",
    };

    private readonly NoticeQueue _queue;
    private readonly NavidromeIdentityService _identity;
    private readonly IHttpClientFactory _http;
    private readonly IOptionsMonitor<SubsonicSettings> _subsonic;
    private readonly IOptionsMonitor<LibraryActionSettings> _settings;
    private readonly ILogger<DuplicateScanWorker> _logger;
    private readonly NavidromeSongPathResolver? _resolver;
    private readonly SpectrumAnalyzer? _spectrum;
    private readonly IOptionsMonitor<SoulseekSettings>? _soulseek;
    private readonly SemaphoreSlim _requested = new(0, 1);
    private DateTime _lastScanUtc = DateTime.MinValue;

    /// <summary>
    /// Spectrum verdicts by track id, size and modification time, so a file is decoded once and
    /// not again on every scan. A replaced or re-tagged file has a new key and is looked at again.
    /// </summary>
    private readonly System.Collections.Concurrent.ConcurrentDictionary<string, SpectrumReport> _spectra = new();

    /// <summary>How many files one scan may decode. A second or so each, and only copies inside a
    /// group with two or more lossless files are ever looked at; the rest wait for the next scan.</summary>
    internal const int MaxSpectrumChecksPerScan = 200;

    public DuplicateScanWorker(NoticeQueue queue, NavidromeIdentityService identity, IHttpClientFactory http,
        IOptionsMonitor<SubsonicSettings> subsonic, IOptionsMonitor<LibraryActionSettings> settings,
        ILogger<DuplicateScanWorker> logger, NavidromeSongPathResolver? resolver = null,
        SpectrumAnalyzer? spectrum = null, IOptionsMonitor<SoulseekSettings>? soulseek = null)
    {
        _queue = queue;
        _identity = identity;
        _http = http;
        _subsonic = subsonic;
        _settings = settings;
        _logger = logger;
        _resolver = resolver;
        _spectrum = spectrum;
        _soulseek = soulseek;
    }

    public DuplicateScanResult? LastResult { get; private set; }

    /// <summary>A scan walking the library right now, and when it began.</summary>
    public bool IsScanning => ScanStartedUtc is not null;
    public DateTime? ScanStartedUtc { get; private set; }

    /// <summary>"Scan now" pressed and not yet picked up.</summary>
    public bool IsRequested => _requested.CurrentCount > 0;

    /// <summary>The dashboard's "Scan now". A scan already waiting to run absorbs a second request.</summary>
    public void RequestScan()
    {
        try { _requested.Release(); }
        catch (SemaphoreFullException) { /* already requested */ }
    }

    protected override async Task ExecuteAsync(CancellationToken stoppingToken)
    {
        var wait = FirstCheck;
        while (!stoppingToken.IsCancellationRequested)
        {
            bool requested;
            try { requested = await _requested.WaitAsync(wait, stoppingToken); }
            catch (OperationCanceledException) { break; }
            wait = CheckInterval;

            // Read afresh every tick, so switching Duplicates on needs no restart and the first
            // scan comes a minute later rather than a day later.
            var settings = _settings.CurrentValue;
            if (!settings.Enabled || !settings.DuplicatesEnabled || !_identity.HasAdminIdentity) continue;
            if (!requested && DateTime.UtcNow - _lastScanUtc < settings.EffectiveDuplicatesScanInterval) continue;

            // Per-scan catch is mandatory: BackgroundServiceExceptionBehavior defaults to
            // StopHost, so one unhandled exception here would take Octo down.
            try
            {
                ScanStartedUtc = DateTime.UtcNow;
                await ScanAsync(settings, stoppingToken);
            }
            catch (OperationCanceledException) when (stoppingToken.IsCancellationRequested) { break; }
            catch (Exception ex) { _logger.LogError(ex, "Duplicate scan failed"); }
            finally
            {
                _lastScanUtc = DateTime.UtcNow;
                ScanStartedUtc = null;
            }
        }
    }

    private async Task ScanAsync(LibraryActionSettings settings, CancellationToken ct)
    {
        var (tracks, complete) = await WalkAsync(ct);
        var groups = FindGroups(tracks);
        if (_resolver is not null && _spectrum is not null && _soulseek?.CurrentValue.DetectTranscodes == true)
        {
            var decoded = 0;
            groups = await CheckTranscodesAsync(groups, track =>
                decoded >= MaxSpectrumChecksPerScan
                    ? Task.FromResult<SpectrumReport?>(null)
                    : SpectrumOfAsync(track, () => decoded++, ct));
        }
        var users = (settings.AllowedUsers ?? []).Where(user => !string.IsNullOrWhiteSpace(user)).ToList();
        var added = _queue.SyncDuplicates(groups, users, complete);
        _queue.Flush();

        LastResult = new DuplicateScanResult(DateTime.UtcNow, tracks.Count, groups.Count, added, complete);
        _logger.LogInformation("Duplicate scan: {Tracks} tracks, {Groups} groups, {Added} new question(s){Partial}",
            tracks.Count, groups.Count, added, complete ? "" : " (the walk did not finish, so nothing was settled)");
    }

    /// <summary>
    /// Every track, by paging search3 with the empty query, the same walk
    /// Symfonium makes to copy a library. Complete only when a short page ends it.
    /// </summary>
    internal async Task<(IReadOnlyList<LibraryTrack> Tracks, bool Complete)> WalkAsync(CancellationToken ct)
    {
        var baseUrl = (_subsonic.CurrentValue.Url ?? "").TrimEnd('/');
        // The scan credential comes from the admin login, so make sure there has been one.
        await _identity.EnsureAdminJwtAsync(ct);
        if (baseUrl.Length == 0 || _identity.GetScanAuth() is not { } auth) return ([], false);

        var tracks = new List<LibraryTrack>();
        var client = _http.CreateClient();
        for (var page = 0; page < MaxPages; page++)
        {
            var url = $"{baseUrl}/rest/search3?f=json&c=octo&v=1.16.1&query=%22%22"
                + $"&songCount={PageSize}&songOffset={page * PageSize}&albumCount=0&artistCount=0"
                + $"&u={Uri.EscapeDataString(auth.user)}&t={Uri.EscapeDataString(auth.token)}&s={Uri.EscapeDataString(auth.salt)}";
            using var response = await client.GetAsync(url, ct);
            if (!response.IsSuccessStatusCode) return (tracks, false);
            using var doc = JsonDocument.Parse(await response.Content.ReadAsByteArrayAsync(ct));
            if (ParsePage(doc.RootElement, tracks) is not { } rows) return (tracks, false);
            if (rows < PageSize) return (tracks, true);
        }

        _logger.LogWarning("The duplicate scan stopped at {Tracks} tracks. Pairs past that point are not found, "
            + "and nothing is settled from a partial walk.", MaxPages * PageSize);
        return (tracks, false);
    }

    /// <summary>
    /// Adds the page's tracks, and returns how many rows the page held, or null when it is not a
    /// successful answer at all (a refusal must not read as the end).
    /// </summary>
    internal static int? ParsePage(JsonElement root, List<LibraryTrack> into)
    {
        if (!root.TryGetProperty("subsonic-response", out var envelope)
            || !envelope.TryGetProperty("status", out var status) || status.GetString() != "ok")
            return null;
        if (!envelope.TryGetProperty("searchResult3", out var result)
            || !result.TryGetProperty("song", out var songs) || songs.ValueKind != JsonValueKind.Array)
            return 0;

        var rows = 0;
        foreach (var song in songs.EnumerateArray())
        {
            rows++;
            var id = Str(song, "id");
            if (string.IsNullOrEmpty(id)) continue;
            // An outside song a server lists beside the library's is not a file to keep or remove.
            if (song.TryGetProperty("isExternal", out var external) && external.ValueKind == JsonValueKind.True) continue;
            into.Add(new LibraryTrack(id, Str(song, "title") ?? "", Str(song, "artist") ?? "", Str(song, "album") ?? "",
                Str(song, "musicBrainzId") ?? "", Str(song, "suffix") ?? "", Int(song, "bitRate"), Int(song, "duration"))
            {
                Isrcs = IsrcsOf(song),
            });
        }
        return rows;
    }

    /// <summary>OpenSubsonic's isrc list; one written as a single string is a list of one.</summary>
    private static IReadOnlyList<string> IsrcsOf(JsonElement song)
    {
        if (!song.TryGetProperty("isrc", out var isrc)) return [];
        if (isrc.ValueKind == JsonValueKind.String) return isrc.GetString() is { Length: > 0 } one ? [one] : [];
        if (isrc.ValueKind != JsonValueKind.Array) return [];
        return isrc.EnumerateArray().Where(value => value.ValueKind == JsonValueKind.String)
            .Select(value => value.GetString()!).Where(value => value.Length > 0).ToList();
    }

    /// <summary>
    /// Copies of one recording, by Library health's own rule, so the Duplicates playlist asks about
    /// exactly the sets Library health shows. Each set is ranked here for keeping, which can also
    /// weigh a fake lossless copy (<see cref="CheckTranscodesAsync"/>).
    /// </summary>
    internal static IReadOnlyList<DuplicateGroup> FindGroups(IEnumerable<LibraryTrack> tracks) =>
        Octo.Services.Health.LibraryHealth.FindDuplicates(tracks.ToList(), LibraryTrackFields.Instance)
            .Select(set => new DuplicateGroup(
                "dup|" + string.Join(",", set.Copies.Select(track => track.Id).Order(StringComparer.Ordinal)),
                RankForKeeping(set.Copies)))
            .ToList();

    /// <summary>
    /// The groups again, with every lossless copy in a group that has two or more of them
    /// checked for being a transcode, and each group ranked again. Only those copies: a lone
    /// lossless file outranks the lossy ones either way, and the rest of the library is never
    /// decoded. A copy the check could not judge counts as genuine.
    /// </summary>
    internal static async Task<IReadOnlyList<DuplicateGroup>> CheckTranscodesAsync(
        IReadOnlyList<DuplicateGroup> groups, Func<LibraryTrack, Task<SpectrumReport?>> check)
    {
        var result = new List<DuplicateGroup>(groups.Count);
        foreach (var group in groups)
        {
            if (group.Tracks.Count(IsLossless) < 2)
            {
                result.Add(group);
                continue;
            }
            var tracks = new List<LibraryTrack>(group.Tracks.Count);
            foreach (var track in group.Tracks)
            {
                var report = IsLossless(track) ? await check(track) : null;
                tracks.Add(report is { IsLikelyLossy: true } ? track with { TranscodedFrom = report.Estimate } : track);
            }
            result.Add(group with { Tracks = RankForKeeping(tracks) });
        }
        return result;
    }

    private async Task<SpectrumReport?> SpectrumOfAsync(LibraryTrack track, Action decoded, CancellationToken ct)
    {
        try
        {
            if (await _resolver!.ResolveAsync(track.Id, ct) is not { } file) return null;
            var key = $"{track.Id}|{file.SizeBytes}|{File.GetLastWriteTimeUtc(file.AbsolutePath).Ticks}";
            if (_spectra.TryGetValue(key, out var known)) return known;

            decoded();
            var report = await _spectrum!.AnalyzeAsync(file.AbsolutePath, _soulseek!.CurrentValue.EffectiveTranscodeCheckTimeoutSeconds);
            // Unknown from a timeout or a missing ffmpeg is not remembered, so it is asked again.
            if (report.Verdict != SpectrumVerdict.Unknown) _spectra[key] = report;
            if (report.IsLikelyLossy)
                _logger.LogInformation("Duplicate scan: {Path} is {Spectrum}", file.AbsolutePath, report.Describe());
            return report;
        }
        catch (OperationCanceledException) { throw; }
        catch (Exception ex)
        {
            _logger.LogDebug("Duplicate scan: could not check {Id} for transcoding: {M}", track.Id, ex.Message);
            return null;
        }
    }

    /// <summary>
    /// Genuine lossless first, then a lossless file made from a lossy one, then the higher
    /// bitrate, then the length closest to the group's median (a copy much longer or shorter
    /// than the rest is the likelier to be cut or padded), then id.
    /// </summary>
    internal static IReadOnlyList<LibraryTrack> RankForKeeping(IReadOnlyList<LibraryTrack> group)
    {
        var lengths = group.Select(track => track.Duration).Order().ToList();
        var median = lengths[lengths.Count / 2];
        return group
            .OrderByDescending(track => IsLossless(track) && track.TranscodedFrom is null)
            .ThenByDescending(IsLossless)
            .ThenByDescending(track => track.BitRate)
            .ThenBy(track => Math.Abs(track.Duration - median))
            .ThenBy(track => track.Id, StringComparer.Ordinal)
            .ToList();
    }

    internal static bool IsLossless(LibraryTrack track) => IsLosslessFile(track.Suffix, track.BitRate);

    /// <summary>ALAC arrives as m4a, told apart from AAC only by its bitrate.</summary>
    internal static bool IsLosslessFile(string suffix, int bitRate) =>
        LosslessSuffixes.Contains(suffix)
        || (suffix.Equals("m4a", StringComparison.OrdinalIgnoreCase) && bitRate > 500);

    private static string? Str(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String ? value.GetString() : null;

    private static int Int(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.Number
        && value.TryGetInt32(out var number) ? number : 0;
}
