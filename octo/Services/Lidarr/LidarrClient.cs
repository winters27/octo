using System.Net.Http.Json;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;

namespace Octo.Services.Lidarr;

public sealed record LidarrChoice(int Id, string Name);
public sealed record LidarrRootFolder(int Id, string Path);
public sealed record LidarrOptions(
    IReadOnlyList<LidarrRootFolder> RootFolders,
    IReadOnlyList<LidarrChoice> QualityProfiles,
    IReadOnlyList<LidarrChoice> MetadataProfiles);

public sealed record LidarrAlbumCandidate(
    int Id, string ForeignAlbumId, string Title, string Artist, int? Year, JsonObject Resource);

public sealed record LidarrImportedTrack(
    int Id, string Title, int? TrackNumber, int? DurationSeconds, bool HasFile,
    string? Path, long SizeBytes, string? Artist);

public sealed record LidarrAlbumImportState(
    IReadOnlyList<LidarrImportedTrack> Tracks, int TrackCount, int TrackFileCount)
{
    public bool IsComplete => TrackCount > 0 && TrackFileCount >= TrackCount;
}

/// <summary>Small, purpose-built client for the Lidarr v1 endpoints Octo needs.</summary>
public sealed class LidarrClient
{
    private readonly IHttpClientFactory _httpFactory;
    private readonly IOptionsMonitor<LidarrSettings> _settings;

    public LidarrClient(IHttpClientFactory httpFactory, IOptionsMonitor<LidarrSettings> settings)
    {
        _httpFactory = httpFactory;
        _settings = settings;
    }

    public async Task<bool> IsReachableAsync(CancellationToken ct = default)
    {
        try
        {
            using var request = CreateRequest(HttpMethod.Get, "/ping");
            using var response = await SendAsync(request, ct);
            return response.IsSuccessStatusCode;
        }
        catch { return false; }
    }

    public async Task<LidarrOptions> TestConnectionAsync(
        string baseUrl, string apiKey, CancellationToken ct = default)
    {
        if (string.IsNullOrWhiteSpace(baseUrl) || string.IsNullOrWhiteSpace(apiKey))
            throw new InvalidOperationException("Lidarr URL and API key are required.");
        using var request = CreateRequest(HttpMethod.Get, "/api/v1/system/status", baseUrl, apiKey);
        using var response = await SendAsync(request, ct);

        var rootsTask = GetArrayAsync("/api/v1/rootfolder", baseUrl, apiKey, ct);
        var qualityTask = GetArrayAsync("/api/v1/qualityprofile", baseUrl, apiKey, ct);
        var metadataTask = GetArrayAsync("/api/v1/metadataprofile", baseUrl, apiKey, ct);
        await Task.WhenAll(rootsTask, qualityTask, metadataTask);
        return MapOptions(rootsTask.Result, qualityTask.Result, metadataTask.Result);
    }

    public async Task<LidarrOptions> GetOptionsAsync(CancellationToken ct = default)
    {
        var rootsTask = GetArrayAsync("/api/v1/rootfolder", ct);
        var qualityTask = GetArrayAsync("/api/v1/qualityprofile", ct);
        var metadataTask = GetArrayAsync("/api/v1/metadataprofile", ct);
        await Task.WhenAll(rootsTask, qualityTask, metadataTask);

        return MapOptions(rootsTask.Result, qualityTask.Result, metadataTask.Result);
    }

    private static LidarrOptions MapOptions(
        IReadOnlyList<JsonObject> roots,
        IReadOnlyList<JsonObject> quality,
        IReadOnlyList<JsonObject> metadata) =>
        new(
            roots.Select(x => new LidarrRootFolder(Int(x, "id"), Str(x, "path"))).ToList(),
            quality.Select(x => new LidarrChoice(Int(x, "id"), Str(x, "name"))).ToList(),
            metadata.Select(x => new LidarrChoice(Int(x, "id"), Str(x, "name"))).ToList());

    public async Task<LidarrAlbumCandidate> ResolveAlbumAsync(
        string artist, string album, int? year, CancellationToken ct = default)
    {
        var term = Uri.EscapeDataString($"{artist} {album}");
        var rows = await GetArrayAsync($"/api/v1/album/lookup?term={term}", ct);
        var candidates = rows.Select(ParseAlbum).Where(x => !string.IsNullOrEmpty(x.ForeignAlbumId)).ToList();
        return SelectBestAlbum(candidates, artist, album, year)
            ?? throw new InvalidOperationException($"Lidarr could not unambiguously match '{artist} - {album}'.");
    }

    internal static LidarrAlbumCandidate? SelectBestAlbum(
        IReadOnlyList<LidarrAlbumCandidate> candidates, string artist, string album, int? year)
    {
        var wantedArtist = Normalize(artist);
        var wantedAlbum = Normalize(album);
        var exact = candidates
            .Where(c => Normalize(c.Artist) == wantedArtist && Normalize(c.Title) == wantedAlbum)
            .GroupBy(c => c.ForeignAlbumId, StringComparer.OrdinalIgnoreCase)
            .Select(g => g.First())
            .ToList();
        if (exact.Count == 1) return exact[0];
        if (exact.Count == 0) return null;

        if (year is int y)
        {
            var sameYear = exact.Where(c => c.Year == y).ToList();
            if (sameYear.Count == 1) return sameYear[0];
        }

        // Multiple releases with the same display identity are unsafe to choose
        // automatically. A wrong album is much worse than an actionable failure.
        return null;
    }

    public async Task<int> EnsureAlbumAndSearchAsync(LidarrAlbumCandidate candidate, CancellationToken ct = default)
    {
        var settings = RequireSettings(requireProfiles: true);
        var existing = await GetArrayAsync(
            $"/api/v1/album?foreignAlbumId={Uri.EscapeDataString(candidate.ForeignAlbumId)}", ct);

        int albumId;
        if (existing.Count > 0)
        {
            var resource = existing[0];
            albumId = Int(resource, "id");
            if (!(resource["monitored"]?.GetValue<bool>() ?? false))
            {
                resource["monitored"] = true;
                await SendJsonAsync(HttpMethod.Put, $"/api/v1/album/{albumId}", resource, ct);
            }
        }
        else
        {
            var resource = (JsonObject)candidate.Resource.DeepClone();
            resource.Remove("id");
            resource["monitored"] = true;
            resource["addOptions"] = new JsonObject { ["searchForNewAlbum"] = false };

            var artist = resource["artist"] as JsonObject
                ?? throw new InvalidOperationException("Lidarr album lookup returned no artist resource.");
            var foreignArtistId = NullableStr(artist, "foreignArtistId") ?? NullableStr(artist, "mbId");
            var existingArtists = await GetArrayAsync("/api/v1/artist", ct);
            var existingArtist = existingArtists.FirstOrDefault(a =>
                !string.IsNullOrEmpty(foreignArtistId)
                && string.Equals(NullableStr(a, "foreignArtistId") ?? NullableStr(a, "mbId"),
                    foreignArtistId, StringComparison.OrdinalIgnoreCase));
            if (existingArtist is not null)
            {
                resource["artistId"] = Int(existingArtist, "id");
                resource["artist"] = existingArtist.DeepClone();
            }
            else
            {
                artist.Remove("id");
                artist["rootFolderPath"] = settings.RootFolderPath;
                artist["qualityProfileId"] = settings.QualityProfileId;
                artist["metadataProfileId"] = settings.MetadataProfileId;
                artist["monitored"] = true;
                artist["monitorNewItems"] = "none";
                artist["tags"] = new JsonArray();
                artist["addOptions"] = new JsonObject
                {
                    ["monitor"] = "none",
                    ["searchForMissingAlbums"] = false,
                };
            }

            var added = await SendJsonAsync(HttpMethod.Post, "/api/v1/album", resource, ct);
            albumId = Int(added, "id");
        }

        await SendJsonAsync(HttpMethod.Post, "/api/v1/command", new JsonObject
        {
            ["name"] = "AlbumSearch",
            ["albumIds"] = new JsonArray(albumId),
        }, ct);
        return albumId;
    }

    public async Task<IReadOnlyList<LidarrImportedTrack>> GetAlbumTracksAsync(int albumId, CancellationToken ct = default)
    {
        // Lidarr deliberately omits the nested trackFile resource from album track
        // list responses, so load the album's files separately and join by id.
        var tracksTask = GetArrayAsync($"/api/v1/track?albumId={albumId}", ct);
        var filesTask = GetArrayAsync($"/api/v1/trackFile?albumId={albumId}", ct);
        await Task.WhenAll(tracksTask, filesTask);
        var files = filesTask.Result.ToDictionary(file => Int(file, "id"));

        return tracksTask.Result.Select(row =>
        {
            var trackFileId = NullableInt(row, "trackFileId") ?? 0;
            files.TryGetValue(trackFileId, out var file);
            var artist = row["artist"] as JsonObject;
            return new LidarrImportedTrack(
                Int(row, "id"), Str(row, "title"), ParseTrackNumber(Str(row, "trackNumber")),
                NullableInt(row, "duration") is int ms ? ms / 1000 : null,
                row["hasFile"]?.GetValue<bool>() ?? false,
                file is null ? null : NullableStr(file, "path"),
                file is null ? 0 : NullableLong(file, "size") ?? 0,
                artist is null ? null : NullableStr(artist, "artistName"));
        }).ToList();
    }

    public async Task<LidarrAlbumImportState> GetAlbumImportStateAsync(
        int albumId, CancellationToken ct = default)
    {
        var albumTask = GetObjectAsync($"/api/v1/album/{albumId}", ct);
        var tracksTask = GetAlbumTracksAsync(albumId, ct);
        await Task.WhenAll(albumTask, tracksTask);
        var statistics = albumTask.Result["statistics"] as JsonObject;
        return new LidarrAlbumImportState(
            tracksTask.Result,
            statistics is null ? tracksTask.Result.Count : Int(statistics, "trackCount"),
            statistics is null ? tracksTask.Result.Count(t => t.HasFile) : Int(statistics, "trackFileCount"));
    }

    private LidarrSettings RequireSettings(bool requireProfiles = false)
    {
        var value = _settings.CurrentValue;
        if (string.IsNullOrWhiteSpace(value.BaseUrl) || string.IsNullOrWhiteSpace(value.ApiKey))
            throw new InvalidOperationException("Lidarr URL and API key are required.");
        if (requireProfiles && (string.IsNullOrWhiteSpace(value.RootFolderPath)
                                || value.QualityProfileId <= 0 || value.MetadataProfileId <= 0))
            throw new InvalidOperationException("Choose a Lidarr root folder, quality profile, and metadata profile.");
        return value;
    }

    private HttpRequestMessage CreateRequest(HttpMethod method, string path)
    {
        var settings = RequireSettings();
        return CreateRequest(method, path, settings.BaseUrl!, settings.ApiKey!);
    }

    private static HttpRequestMessage CreateRequest(
        HttpMethod method, string path, string baseUrl, string apiKey)
    {
        var request = new HttpRequestMessage(method, $"{baseUrl.TrimEnd('/')}{path}");
        request.Headers.TryAddWithoutValidation("X-Api-Key", apiKey);
        return request;
    }

    private async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
    {
        var client = _httpFactory.CreateClient();
        client.Timeout = TimeSpan.FromSeconds(20);
        var response = await client.SendAsync(request, ct);
        if (!response.IsSuccessStatusCode)
        {
            var body = await response.Content.ReadAsStringAsync(ct);
            response.Dispose();
            throw new HttpRequestException(
                $"Lidarr returned HTTP {(int)response.StatusCode}: {Truncate(body, 300)}");
        }
        return response;
    }

    private async Task<List<JsonObject>> GetArrayAsync(string path, CancellationToken ct)
    {
        using var request = CreateRequest(HttpMethod.Get, path);
        return await GetArrayAsync(request, ct);
    }

    private async Task<List<JsonObject>> GetArrayAsync(
        string path, string baseUrl, string apiKey, CancellationToken ct)
    {
        using var request = CreateRequest(HttpMethod.Get, path, baseUrl, apiKey);
        return await GetArrayAsync(request, ct);
    }

    private async Task<List<JsonObject>> GetArrayAsync(HttpRequestMessage request, CancellationToken ct)
    {
        using var response = await SendAsync(request, ct);
        await using var stream = await response.Content.ReadAsStreamAsync(ct);
        var node = await JsonNode.ParseAsync(stream, cancellationToken: ct) as JsonArray
            ?? throw new InvalidOperationException("Lidarr returned an invalid array response.");
        return node.OfType<JsonObject>().ToList();
    }

    private async Task<JsonObject> GetObjectAsync(string path, CancellationToken ct)
    {
        using var request = CreateRequest(HttpMethod.Get, path);
        using var response = await SendAsync(request, ct);
        await using var stream = await response.Content.ReadAsStreamAsync(ct);
        return await JsonNode.ParseAsync(stream, cancellationToken: ct) as JsonObject
            ?? throw new InvalidOperationException("Lidarr returned an invalid object response.");
    }

    private async Task<JsonObject> SendJsonAsync(HttpMethod method, string path, JsonObject body, CancellationToken ct)
    {
        using var request = CreateRequest(method, path);
        request.Content = JsonContent.Create(body);
        using var response = await SendAsync(request, ct);
        await using var stream = await response.Content.ReadAsStreamAsync(ct);
        return await JsonNode.ParseAsync(stream, cancellationToken: ct) as JsonObject
            ?? throw new InvalidOperationException("Lidarr returned an invalid object response.");
    }

    private static LidarrAlbumCandidate ParseAlbum(JsonObject row)
    {
        var artist = row["artist"] as JsonObject;
        var date = NullableStr(row, "releaseDate");
        int? year = date?.Length >= 4 && int.TryParse(date[..4], out var parsed) ? parsed : null;
        return new LidarrAlbumCandidate(
            NullableInt(row, "id") ?? 0,
            NullableStr(row, "foreignAlbumId") ?? "",
            NullableStr(row, "title") ?? "",
            artist is null ? "" : NullableStr(artist, "artistName") ?? "",
            year,
            (JsonObject)row.DeepClone());
    }

    internal static string Normalize(string value) =>
        new(value.Normalize().ToLowerInvariant().Where(char.IsLetterOrDigit).ToArray());

    private static int? ParseTrackNumber(string value)
    {
        var head = value.Split('-', '/', '.')[0];
        return int.TryParse(head, out var parsed) ? parsed : null;
    }

    private static string Truncate(string value, int max) => value.Length <= max ? value : value[..max];
    private static int Int(JsonObject o, string key) => NullableInt(o, key) ?? 0;
    private static string Str(JsonObject o, string key) => NullableStr(o, key) ?? "";
    private static string? NullableStr(JsonObject o, string key) => o[key]?.GetValue<string>();
    private static int? NullableInt(JsonObject o, string key) => o[key]?.GetValue<int>();
    private static long? NullableLong(JsonObject o, string key) => o[key]?.GetValue<long>();
}
