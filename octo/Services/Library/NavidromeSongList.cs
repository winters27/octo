using System.Text.Json;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Subsonic;

namespace Octo.Services.Library;

/// <summary>One song as Navidrome's native song list has it, by the full path of its file.</summary>
public sealed record NavidromeSongEntry(string FullPath, string? AlbumId, string? Artist, string? Title, string? Album,
    string? Lyrics)
{
    /// <summary>Navidrome's id for the song.</summary>
    public string? Id { get; init; }

    /// <summary>The length in seconds, as Navidrome read it.</summary>
    public double? Duration { get; init; }

    /// <summary>What Navidrome read from the advisory tag: "e" (explicit), "c" (clean) or "".</summary>
    public string? ExplicitStatus { get; init; }

    /// <summary>Whether Navidrome read lyrics from the song's tags (not a file beside it).</summary>
    public bool HasTagLyrics => !string.IsNullOrWhiteSpace(Lyrics) && Lyrics.Trim() is not "[]" and not "null";
}

/// <summary>
/// Navidrome's own song list (`GET /api/song`, 1000 a page, as the admin), which names every
/// song's artist, title, album and tag lyrics in a few requests. A scan that would otherwise open
/// every file over a network mount (a few songs a second) reads this instead.
/// </summary>
public static class NavidromeSongList
{
    /// <summary>Every song, keyed by full path (the same file can be named under the library path
    /// and the music root, so both are keys). Empty when Navidrome cannot be asked.</summary>
    public static async Task<Dictionary<string, NavidromeSongEntry>> ListAsync(IServiceProvider services, string root,
        ILogger logger, CancellationToken ct)
    {
        var songs = new Dictionary<string, NavidromeSongEntry>(StringComparer.Ordinal);
        var identity = services.GetService<NavidromeIdentityService>();
        var baseUrl = services.GetService<IOptionsMonitor<SubsonicSettings>>()?.CurrentValue.Url;
        var http = services.GetService<IHttpClientFactory>();
        if (identity is null || http is null || string.IsNullOrWhiteSpace(baseUrl)) return songs;
        try
        {
            var jwt = await identity.EnsureAdminJwtAsync(ct);
            if (string.IsNullOrEmpty(jwt)) return songs;
            const int page = 1000;
            for (var start = 0; start < 200_000; start += page)
            {
                using var request = new HttpRequestMessage(HttpMethod.Get,
                    $"{baseUrl.TrimEnd('/')}/api/song?_start={start}&_end={start + page}&_sort=id&_order=ASC");
                request.Headers.TryAddWithoutValidation("X-Nd-Authorization", $"Bearer {jwt}");
                using var response = await http.CreateClient().SendAsync(request, ct);
                if (!response.IsSuccessStatusCode) break;
                using var doc = JsonDocument.Parse(await response.Content.ReadAsByteArrayAsync(ct));
                if (doc.RootElement.ValueKind != JsonValueKind.Array) break;
                var count = 0;
                foreach (var song in doc.RootElement.EnumerateArray())
                {
                    count++;
                    var path = Str(song, "path");
                    if (string.IsNullOrEmpty(path)) continue;
                    var relative = Path.Combine(path.Replace('\\', '/').TrimStart('/')
                        .Split('/', StringSplitOptions.RemoveEmptyEntries));
                    var paths = new List<string>();
                    foreach (var baseDir in new[] { Str(song, "libraryPath"), root })
                        if (!string.IsNullOrEmpty(baseDir)) paths.Add(Path.GetFullPath(Path.Combine(baseDir, relative)));
                    // An older Navidrome reports the full path instead.
                    if (Path.IsPathRooted(path)) paths.Add(Path.GetFullPath(path));
                    foreach (var full in paths)
                        songs.TryAdd(full, new NavidromeSongEntry(full, Str(song, "albumId"), Str(song, "artist"),
                            Str(song, "title"), Str(song, "album"), Str(song, "lyrics"))
                        {
                            Id = Str(song, "id"),
                            Duration = song.TryGetProperty("duration", out var length) && length.ValueKind == JsonValueKind.Number
                                ? length.GetDouble() : null,
                            ExplicitStatus = Str(song, "explicitStatus"),
                        });
                }
                if (count < page) break;
            }
        }
        catch (Exception ex) when (!ct.IsCancellationRequested)
        {
            logger.LogInformation("Could not list Navidrome's songs, so each file is read instead: {M}", ex.Message);
        }
        return songs;
    }

    private static string? Str(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String ? value.GetString() : null;
}
