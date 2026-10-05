using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Local;
using Octo.Services.Subsonic;

namespace Octo.Services.Library;

/// <summary>
/// How a path was obtained. Logged and surfaced in the dashboard, because the confidence of
/// anything done to the file depends entirely on which of these produced it.
/// </summary>
public enum PathSource { NativeApi, SubsonicGetSong, LocalMappings, None }

/// <summary>
/// A resolved, VERIFIED file.
///
/// Constructed only by the resolver, and only after the file exists, sits inside the music
/// root, and matches the size Navidrome reported. Anything that acts on a file takes one of
/// these rather than a raw string, so "I have a path" and "I have proof it is the right path"
/// cannot drift apart.
/// </summary>
public sealed record ResolvedSongFile(
    string NavidromeId, string AbsolutePath, long SizeBytes,
    string Title, string Artist, string Album, string Suffix, int? DurationSeconds,
    PathSource Source, string? AlbumArtist = null);

public sealed class NavidromeSongPathResolver
{
    /// <summary>What a leg reported, before any of it is believed.</summary>
    internal sealed record Candidate(
        string Id, string? RawPath, string? LibraryPath, long Size,
        string Title, string Artist, string Album, string Suffix, int? Duration, PathSource Source,
        bool Missing = false, string? AlbumArtist = null);

    private readonly NavidromeIdentityService _identity;
    private readonly ILocalLibraryService _library;
    private readonly IHttpClientFactory _http;
    private readonly IOptionsMonitor<SubsonicSettings> _subsonic;
    private readonly IConfiguration _config;
    private readonly ILogger<NavidromeSongPathResolver> _logger;

    public NavidromeSongPathResolver(NavidromeIdentityService identity, ILocalLibraryService library,
        IHttpClientFactory http, IOptionsMonitor<SubsonicSettings> subsonic,
        IConfiguration config, ILogger<NavidromeSongPathResolver> logger)
    {
        _identity = identity;
        _library = library;
        _http = http;
        _subsonic = subsonic;
        _config = config;
        _logger = logger;
    }

    /// <summary>
    /// Resolve a Navidrome-local song id to a file on Octo's disk, or null.
    ///
    /// Null is a first-class outcome, not an error: an unresolvable id has to make whatever
    /// asked for it a visible no-op. Never return a path that only looks right. Navidrome's
    /// Subsonic `path` is SYNTHESISED FROM TAGS unless the calling player has ReportRealPath
    /// set (it defaults off), so on a library whose filenames are not tag-derived it can name
    /// a file that exists and is a DIFFERENT recording.
    /// </summary>
    public async Task<ResolvedSongFile?> ResolveAsync(string navidromeId, CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(navidromeId)) return null;

        var root = MusicRoot();

        var native = await TryNativeAsync(navidromeId, ct);
        if (native is not null && Verify(native, root) is { } fromNative) return fromNative;

        var subsonic = await TrySubsonicAsync(navidromeId, ct);
        if (subsonic is not null && Verify(subsonic, root) is { } fromSubsonic) return fromSubsonic;

        // Tags are the only thing left. Both legs above carry them even when their path is
        // wrong, so prefer whichever actually answered.
        var tags = native ?? subsonic;
        if (tags is not null && await TryLocalMappingsAsync(tags, root) is { } fromMappings) return fromMappings;

        _logger.LogWarning(
            "Library: could not resolve Navidrome id {Id} to a file under {Root} "
            + "(native={Native}, subsonic={Subsonic}). No action taken.",
            navidromeId, root, native?.RawPath ?? "-", subsonic?.RawPath ?? "-");
        return null;
    }

    /// <summary>
    /// Navidrome's own view: `mf.Path`, which is library-relative, plus `libraryPath`. This is
    /// the real path and never a fakePath. Uses the admin JWT because that is the only standing
    /// native credential Octo has; the endpoint itself is not admin-only today.
    /// </summary>
    private async Task<Candidate?> TryNativeAsync(string id, CancellationToken ct)
    {
        var baseUrl = _subsonic.CurrentValue.Url;
        if (string.IsNullOrWhiteSpace(baseUrl)) return null;

        var jwt = await _identity.EnsureAdminJwtAsync(ct);
        if (string.IsNullOrEmpty(jwt)) return null;

        try
        {
            var url = $"{baseUrl.TrimEnd('/')}/api/song/{Uri.EscapeDataString(id)}";
            var response = await GetNativeAsync(url, jwt, ct);
            if (response.StatusCode == System.Net.HttpStatusCode.Unauthorized)
            {
                // An expired admin token: log in again once, rather than treating every song as
                // unresolvable until someone happens to open the dashboard.
                response.Dispose();
                _identity.InvalidateAdminJwt(jwt);
                var fresh = await _identity.EnsureAdminJwtAsync(ct);
                if (string.IsNullOrEmpty(fresh) || fresh == jwt) return null;
                response = await GetNativeAsync(url, fresh, ct);
            }
            using (response)
            {
                if (!response.IsSuccessStatusCode) return null;
                using var doc = JsonDocument.Parse(await response.Content.ReadAsByteArrayAsync(ct));
                return FromJson(doc.RootElement, id, PathSource.NativeApi, libraryPathProperty: "libraryPath");
            }
        }
        catch (Exception ex)
        {
            _logger.LogDebug("native song lookup failed for {Id}: {M}", id, ex.Message);
            return null;
        }
    }

    private async Task<HttpResponseMessage> GetNativeAsync(string url, string jwt, CancellationToken ct)
    {
        using var request = new HttpRequestMessage(HttpMethod.Get, url);
        request.Headers.TryAddWithoutValidation("X-Nd-Authorization", $"Bearer {jwt}");
        return await _http.CreateClient().SendAsync(request, ct);
    }

    /// <summary>
    /// The Navidrome id of a file Octo just placed: search3 for its artist and title, keep the
    /// hits of exactly this file's size, and take the one whose VERIFIED path is this file.
    /// Null until Navidrome has scanned it. The size filter is what keeps a common title from
    /// costing a native lookup per hit.
    /// </summary>
    public async Task<string?> FindIdByPathAsync(string artist, string title, string absolutePath,
        CancellationToken ct = default)
    {
        var baseUrl = _subsonic.CurrentValue.Url;
        if (string.IsNullOrWhiteSpace(baseUrl) || !File.Exists(absolutePath)) return null;
        if (_identity.GetScanAuth() is not { } auth) return null;

        var size = new FileInfo(absolutePath).Length;
        var target = Path.GetFullPath(absolutePath);
        try
        {
            var url = $"{baseUrl.TrimEnd('/')}/rest/search3?f=json&c=octo&v=1.16.1"
                + $"&query={Uri.EscapeDataString($"{artist} {title}".Trim())}&songCount=20&albumCount=0&artistCount=0"
                + $"&u={Uri.EscapeDataString(auth.user)}&t={auth.token}&s={auth.salt}";
            using var response = await _http.CreateClient().GetAsync(url, ct);
            if (!response.IsSuccessStatusCode) return null;

            using var doc = JsonDocument.Parse(await response.Content.ReadAsByteArrayAsync(ct));
            if (!doc.RootElement.TryGetProperty("subsonic-response", out var envelope)
                || !envelope.TryGetProperty("searchResult3", out var result)
                || !result.TryGetProperty("song", out var songs) || songs.ValueKind != JsonValueKind.Array)
                return null;

            var candidates = songs.EnumerateArray()
                .Where(song => !song.TryGetProperty("size", out var s) || s.ValueKind != JsonValueKind.Number || s.GetInt64() == size)
                .Select(song => song.TryGetProperty("id", out var i) ? i.GetString() : null)
                .Where(id => !string.IsNullOrEmpty(id))
                .Select(id => id!)
                .ToList();

            foreach (var id in candidates)
                if (await ResolveAsync(id, ct) is { } resolved
                    && string.Equals(Path.GetFullPath(resolved.AbsolutePath), target, StringComparison.Ordinal))
                    return id;
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _logger.LogDebug("could not find the Navidrome id of {Path}: {M}", absolutePath, ex.Message);
        }
        return null;
    }

    /// <summary>
    /// Whether Navidrome has this id as a present song at exactly this file (W8): not missing,
    /// at this path, at this size. The size proves it read the file after it moved in, since a
    /// replacement at the original's own path is still the old row until the scan.
    /// </summary>
    public async Task<bool> ShowsAtAsync(string navidromeId, string absolutePath, CancellationToken ct = default) =>
        Shows(await TryNativeAsync(navidromeId, ct), MusicRoot(), absolutePath);

    internal bool Shows(Candidate? song, string root, string absolutePath) =>
        song is { Missing: false } && Verify(song, root, quiet: true) is { } resolved
        && string.Equals(Path.GetFullPath(resolved.AbsolutePath), Path.GetFullPath(absolutePath),
            OperatingSystem.IsWindows() ? StringComparison.OrdinalIgnoreCase : StringComparison.Ordinal);

    /// <summary>
    /// Subsonic getSong with Octo's admin triplet.
    ///
    /// The `path` here is `fakePath(mf)`, synthesised from TAGS as Artist/Album/NN - Title.ext,
    /// unless the calling player has ReportRealPath set, which defaults off. It is therefore a
    /// HINT, and it is only ever accepted after Verify() matches the byte size.
    /// </summary>
    private async Task<Candidate?> TrySubsonicAsync(string id, CancellationToken ct)
    {
        var baseUrl = _subsonic.CurrentValue.Url;
        if (string.IsNullOrWhiteSpace(baseUrl)) return null;
        if (_identity.GetScanAuth() is not { } auth) return null;

        try
        {
            var url = $"{baseUrl.TrimEnd('/')}/rest/getSong?f=json&c=octo&v=1.16.1"
                + $"&id={Uri.EscapeDataString(id)}&u={Uri.EscapeDataString(auth.user)}"
                + $"&t={auth.token}&s={auth.salt}";

            using var response = await _http.CreateClient().GetAsync(url, ct);
            if (!response.IsSuccessStatusCode) return null;

            using var doc = JsonDocument.Parse(await response.Content.ReadAsByteArrayAsync(ct));
            if (!doc.RootElement.TryGetProperty("subsonic-response", out var envelope)) return null;
            if (!envelope.TryGetProperty("song", out var song)) return null;

            return FromJson(song, id, PathSource.SubsonicGetSong, libraryPathProperty: null);
        }
        catch (Exception ex)
        {
            _logger.LogDebug("subsonic song lookup failed for {Id}: {M}", id, ex.Message);
            return null;
        }
    }

    /// <summary>
    /// Last resort: Octo's own record of where it put a file, matched on artist, title and
    /// album. Only covers downloads Octo made, which is the common case for the mistakes this
    /// exists to fix.
    /// </summary>
    private async Task<ResolvedSongFile?> TryLocalMappingsAsync(Candidate tags, string root)
    {
        var mapping = await _library.FindMappingByTagsAsync(tags.Artist, tags.Title, tags.Album);
        if (mapping?.LocalPath is not { Length: > 0 } path) return null;

        var full = Path.GetFullPath(path);
        if (!IsInside(full, root) || !File.Exists(full)) return null;

        var info = new FileInfo(full);
        return new ResolvedSongFile(tags.Id, full, info.Length, tags.Title, tags.Artist,
            tags.Album, Path.GetExtension(full).TrimStart('.'), tags.Duration, PathSource.LocalMappings);
    }

    /// <summary>
    /// Turn a candidate into a ResolvedSongFile, or null. Four checks, all required:
    ///
    ///   1. The path resolves INSIDE the music root, which blocks "../" and an absolute path
    ///      from a differently-mounted Navidrome pointing at Octo's own config directory.
    ///   2. The file exists.
    ///   3. Its byte size equals the size Navidrome reported.
    ///   4. Its extension equals the suffix Navidrome reported.
    ///
    /// (3) is the one that matters. Navidrome's fakePath is built from tags, so on a library
    /// whose filenames do NOT come from its tags it can name a real, DIFFERENT file. Two
    /// distinct audio files agreeing byte-for-byte on length is not a thing that happens by
    /// accident.
    /// </summary>
    internal ResolvedSongFile? Verify(Candidate candidate, string root, bool quiet = false)
    {
        foreach (var attempt in CandidatePaths(candidate, root))
        {
            string full;
            try { full = Path.GetFullPath(attempt); }
            catch { continue; }

            if (!IsInside(full, root)) continue;

            var info = new FileInfo(full);
            if (!info.Exists) continue;

            if (candidate.Size > 0 && info.Length != candidate.Size)
            {
                if (!quiet)
                    _logger.LogWarning(
                        "Library: {Path} exists but is {Actual} bytes and Navidrome reports {Expected}. "
                        + "Refusing it, because a path built from tags can name a different file.",
                        full, info.Length, candidate.Size);
                continue;
            }

            if (!string.IsNullOrEmpty(candidate.Suffix)
                && !Path.GetExtension(full).TrimStart('.')
                    .Equals(candidate.Suffix, StringComparison.OrdinalIgnoreCase)) continue;

            return new ResolvedSongFile(candidate.Id, full, info.Length, candidate.Title,
                candidate.Artist, candidate.Album, candidate.Suffix, candidate.Duration, candidate.Source, candidate.AlbumArtist);
        }
        return null;
    }

    /// <summary>
    /// Every way the reported path can be joined onto a real directory, in confidence order.
    /// libraryPath first because it is Navidrome's own root and is correct when the two
    /// containers share a mount; then Octo's effective root, which is what the shipped compose
    /// file produces since both mount the same directory at /music; then the rooted-path case.
    /// </summary>
    internal static IEnumerable<string> CandidatePaths(Candidate candidate, string root)
    {
        if (string.IsNullOrWhiteSpace(candidate.RawPath)) yield break;

        // Navidrome reports '/' separators regardless of host. Convert to the platform's own
        // before combining, so a yielded path is not a mix of both and a log line is readable.
        var segments = candidate.RawPath.Replace('\\', '/').TrimStart('/')
            .Split('/', StringSplitOptions.RemoveEmptyEntries);
        if (segments.Length == 0) yield break;

        var relative = Path.Combine(segments);

        if (!string.IsNullOrEmpty(candidate.LibraryPath) && Directory.Exists(candidate.LibraryPath))
            yield return Path.Combine(candidate.LibraryPath, relative);

        yield return Path.Combine(root, relative);

        if (Path.IsPathRooted(candidate.RawPath))
        {
            yield return candidate.RawPath;                       // same mount point
            // Different mount: keep the tail that still looks like Artist/Album/File.
            yield return Path.Combine(root, Path.Combine(segments.TakeLast(3).ToArray()));
        }
    }

    internal static bool IsInside(string full, string root)
    {
        var normalised = Path.GetFullPath(root).TrimEnd(Path.DirectorySeparatorChar)
            + Path.DirectorySeparatorChar;
        return full.StartsWith(normalised, OperatingSystem.IsWindows()
            ? StringComparison.OrdinalIgnoreCase
            : StringComparison.Ordinal);
    }

    /// <summary>
    /// The music root, read through the SAME accessor the downloader uses.
    /// Subsonic:AutoDetectDownloadPath defaults true, so Library:DownloadPath is only a
    /// fallback, and reading it directly would let this resolver and the downloader disagree
    /// about where the library is. That disagreement is exactly how you act on the wrong
    /// directory.
    /// </summary>
    public string MusicRoot() =>
        _identity.EffectiveDownloadPath(_config["Library:DownloadPath"] ?? "./downloads");

    internal static Candidate? FromJson(JsonElement element, string id, PathSource source,
        string? libraryPathProperty)
    {
        if (element.ValueKind != JsonValueKind.Object) return null;

        return new Candidate(
            Id: id,
            RawPath: Str(element, "path"),
            LibraryPath: libraryPathProperty is null ? null : Str(element, libraryPathProperty),
            Size: element.TryGetProperty("size", out var size) && size.TryGetInt64(out var bytes) ? bytes : 0,
            Title: Str(element, "title") ?? "",
            Artist: Str(element, "artist") ?? "",
            Album: Str(element, "album") ?? "",
            Suffix: Str(element, "suffix") ?? "",
            // Navidrome's native API sends seconds with a fraction (301.37); Subsonic's getSong whole ones.
            Duration: element.TryGetProperty("duration", out var d) && d.ValueKind == JsonValueKind.Number
                      && d.TryGetDouble(out var secs) && secs > 0
                ? (int)Math.Round(secs, MidpointRounding.AwayFromZero) : null,
            Source: source,
            Missing: element.TryGetProperty("missing", out var m) && m.ValueKind == JsonValueKind.True,
            AlbumArtist: Str(element, "albumArtist"));
    }

    private static string? Str(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String
            ? value.GetString()
            : null;
}
