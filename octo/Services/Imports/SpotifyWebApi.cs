using System.Net;
using System.Net.Http.Headers;
using System.Text.Json;
using Octo.Services.Common;

namespace Octo.Services.Imports;

/// <summary>A refusal from Spotify, in words a person can act on. Status is the HTTP status, when there was one.</summary>
public sealed class SpotifyException(string message, HttpStatusCode? status = null, bool signInEnded = false) : Exception(message)
{
    public HttpStatusCode? Status { get; } = status;
    /// <summary>The sign-in itself is over (revoked, or six months old): only signing in again helps.</summary>
    public bool SignInEnded { get; } = signInEnded;
}

public sealed record SpotifyTokens(string AccessToken, string? RefreshToken, DateTime ExpiresUtc);

public sealed record SpotifyProfile(string Id, string? DisplayName);

/// <summary>A playlist as the account lists it. Total is what Spotify says it holds.</summary>
public sealed record SpotifyPlaylistInfo(string Id, string Name, string? OwnerId, string? OwnerName, bool Collaborative,
    string? SnapshotId, string? ImageUrl, int Total);

/// <summary>A playlist's songs, or that Spotify would not show them to this app.</summary>
public sealed record SpotifyPlaylistItems(IReadOnlyList<ImportTrack> Tracks, bool Hidden);

/// <summary>
/// The few Spotify Web API calls an import needs, as Spotify answers them in 2026: liked songs
/// (GET /me/tracks), the account's playlists (GET /me/playlists) and a playlist's songs
/// (GET /playlists/{id}/items). A development-mode app gets no ISRCs, and the songs only of
/// playlists the account owns or works on; both are read when they are there. Every page is 50
/// rows, the most Spotify gives, and a 429 waits as long as Spotify asks.
/// </summary>
public sealed class SpotifyWebApi
{
    public const string ClientName = "spotify";
    internal const string ApiBase = "https://api.spotify.com/v1";
    internal const string TokenUrl = "https://accounts.spotify.com/api/token";
    private const int PageSize = 50;
    /// <summary>A guard against a paging loop, far above any real library.</summary>
    internal const int MaxRows = 50_000;
    private const int MaxRetries = 4;
    private static readonly TimeSpan LongestWait = TimeSpan.FromMinutes(2);

    private readonly IHttpClientFactory _http;
    private readonly ILogger<SpotifyWebApi> _logger;

    public SpotifyWebApi(IHttpClientFactory http, ILogger<SpotifyWebApi> logger)
    {
        _http = http;
        _logger = logger;
    }

    /// <summary>How a 429 waits. Tests make it instant.</summary>
    internal Func<TimeSpan, CancellationToken, Task> Wait { get; set; } = Task.Delay;
    internal Func<DateTime> Clock { get; set; } = () => DateTime.UtcNow;

    // ---- Tokens ------------------------------------------------------------------------------

    public Task<SpotifyTokens> ExchangeAsync(string clientId, string code, string verifier, string redirectUri,
        CancellationToken ct) =>
        TokenAsync(new Dictionary<string, string>
        {
            ["grant_type"] = "authorization_code", ["code"] = code, ["redirect_uri"] = redirectUri,
            ["client_id"] = clientId, ["code_verifier"] = verifier,
        }, ct);

    /// <summary>A fresh access token. Spotify may hand back a new refresh token too, which then replaces the old one.</summary>
    public Task<SpotifyTokens> RefreshAsync(string clientId, string refreshToken, CancellationToken ct) =>
        TokenAsync(new Dictionary<string, string>
        {
            ["grant_type"] = "refresh_token", ["refresh_token"] = refreshToken, ["client_id"] = clientId,
        }, ct);

    private async Task<SpotifyTokens> TokenAsync(Dictionary<string, string> form, CancellationToken ct)
    {
        using var response = await _http.CreateClient(ClientName).PostAsync(TokenUrl, new FormUrlEncodedContent(form), ct);
        var body = await response.Content.ReadAsStringAsync(ct);
        if (!response.IsSuccessStatusCode)
        {
            var (error, description) = ReadError(body);
            // invalid_grant: the code was used or is old, or the refresh token was revoked or has
            // reached Spotify's six months. Either way only a new sign-in helps.
            if (error == "invalid_grant")
                throw new SpotifyException("Spotify ended this sign-in. Connect again.", response.StatusCode, signInEnded: true);
            if (error == "invalid_client")
                throw new SpotifyException("Spotify does not know this Client ID. Check it on the Spotify developer dashboard.", response.StatusCode);
            throw new SpotifyException($"Spotify refused the sign-in: {description ?? error ?? $"HTTP {(int)response.StatusCode}"}", response.StatusCode);
        }
        using var doc = JsonDocument.Parse(body);
        var root = doc.RootElement;
        var access = Str(root, "access_token") ?? throw new SpotifyException("Spotify sent no access token.");
        var seconds = root.TryGetProperty("expires_in", out var e) && e.TryGetInt32(out var s) ? s : 3600;
        return new SpotifyTokens(access, Str(root, "refresh_token"), Clock().AddSeconds(seconds));
    }

    // ---- Reads -------------------------------------------------------------------------------

    public async Task<SpotifyProfile> ProfileAsync(string token, CancellationToken ct)
    {
        using var doc = await GetAsync(token, $"{ApiBase}/me", ct);
        var root = doc.RootElement;
        return new SpotifyProfile(Str(root, "id") ?? "", Str(root, "display_name"));
    }

    /// <summary>Every liked song, newest first, as Spotify orders them.</summary>
    public async Task<IReadOnlyList<ImportTrack>> LikedSongsAsync(string token, Action<int, int>? progress, CancellationToken ct)
    {
        var tracks = new List<ImportTrack>();
        await PageAsync(token, $"{ApiBase}/me/tracks", item =>
        {
            if (TrackOf(item) is { } track) tracks.Add(track);
        }, progress, ct);
        return tracks;
    }

    public async Task<IReadOnlyList<SpotifyPlaylistInfo>> PlaylistsAsync(string token, CancellationToken ct)
    {
        var playlists = new List<SpotifyPlaylistInfo>();
        await PageAsync(token, $"{ApiBase}/me/playlists", item =>
        {
            if (Str(item, "id") is not { Length: > 0 } id) return;
            var owner = item.TryGetProperty("owner", out var o) && o.ValueKind == JsonValueKind.Object ? o : default;
            // items.total since February 2026; tracks.total before, and still sent for now.
            var total = Total(item, "items") ?? Total(item, "tracks") ?? 0;
            playlists.Add(new SpotifyPlaylistInfo(id, Str(item, "name") ?? "Untitled playlist",
                owner.ValueKind == JsonValueKind.Object ? Str(owner, "id") : null,
                owner.ValueKind == JsonValueKind.Object ? Str(owner, "display_name") ?? Str(owner, "id") : null,
                item.TryGetProperty("collaborative", out var c) && c.ValueKind == JsonValueKind.True,
                Str(item, "snapshot_id"), FirstImage(item), total));
        }, null, ct);
        return playlists;
    }

    /// <summary>
    /// A playlist's songs in order. A development-mode app is shown the songs only of playlists the
    /// account owns or works on: for any other, Spotify answers 403, which reads as Hidden.
    /// </summary>
    public async Task<SpotifyPlaylistItems> PlaylistItemsAsync(string token, string playlistId,
        Action<int, int>? progress, CancellationToken ct)
    {
        var tracks = new List<ImportTrack>();
        try
        {
            await PageAsync(token, $"{ApiBase}/playlists/{Uri.EscapeDataString(playlistId)}/items?additional_types=track",
                item => { if (TrackOf(item) is { } track) tracks.Add(track); }, progress, ct);
        }
        catch (SpotifyException ex) when (ex.Status is HttpStatusCode.Forbidden or HttpStatusCode.NotFound)
        {
            return new SpotifyPlaylistItems([], Hidden: true);
        }
        return new SpotifyPlaylistItems(tracks, Hidden: false);
    }

    private async Task PageAsync(string token, string url, Action<JsonElement> each, Action<int, int>? progress,
        CancellationToken ct)
    {
        var offset = 0;
        while (offset < MaxRows)
        {
            var join = url.Contains('?') ? '&' : '?';
            using var doc = await GetAsync(token, $"{url}{join}limit={PageSize}&offset={offset}", ct);
            var root = doc.RootElement;
            var count = 0;
            if (root.TryGetProperty("items", out var items) && items.ValueKind == JsonValueKind.Array)
                foreach (var item in items.EnumerateArray())
                {
                    count++;
                    each(item);
                }
            offset += count;
            var total = root.TryGetProperty("total", out var t) && t.TryGetInt32(out var n) ? n : offset;
            progress?.Invoke(offset, total);
            var more = root.TryGetProperty("next", out var next) && next.ValueKind == JsonValueKind.String;
            if (count == 0 || !more) return;
        }
    }

    private async Task<JsonDocument> GetAsync(string token, string url, CancellationToken ct)
    {
        for (var attempt = 0; ; attempt++)
        {
            using var request = new HttpRequestMessage(HttpMethod.Get, url);
            request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", token);
            using var response = await _http.CreateClient(ClientName).SendAsync(request, ct);
            var body = await response.Content.ReadAsStringAsync(ct);
            if (response.IsSuccessStatusCode) return JsonDocument.Parse(body.Length == 0 ? "{}" : body);

            if (response.StatusCode == HttpStatusCode.TooManyRequests)
            {
                // Since July 2026 a 429 that is the app's quota says so; waiting a few seconds will not help.
                if (body.Contains("QUOTA_EXCEEDED", StringComparison.Ordinal))
                    throw new SpotifyException("This Spotify app has used up its requests for now. Octo tries again later.", response.StatusCode);
                var wait = response.Headers.RetryAfter?.Delta ?? TimeSpan.FromSeconds(5 * (attempt + 1));
                if (attempt >= MaxRetries || wait > LongestWait)
                    throw new SpotifyException("Spotify asked Octo to slow down. Octo tries again later.", response.StatusCode);
                _logger.LogInformation("Spotify asked to wait {Seconds} s before {Url}", wait.TotalSeconds, url);
                await Wait(wait, ct);
                continue;
            }
            if (response.StatusCode >= HttpStatusCode.InternalServerError && attempt < 2)
            {
                await Wait(TimeSpan.FromSeconds(2 * (attempt + 1)), ct);
                continue;
            }
            var (error, description) = ReadError(body);
            var reason = description ?? error;
            throw response.StatusCode switch
            {
                HttpStatusCode.Unauthorized => new SpotifyException("Spotify did not take the sign-in.", response.StatusCode),
                HttpStatusCode.Forbidden when reason?.Contains("user", StringComparison.OrdinalIgnoreCase) == true =>
                    new SpotifyException("Spotify refused: this account is not on the app's list of users. Add it under User Management on the Spotify developer dashboard.", response.StatusCode),
                _ => new SpotifyException($"Spotify answered {(int)response.StatusCode}{(reason is null ? "" : $": {reason}")}", response.StatusCode),
            };
        }
    }

    // ---- Shapes ------------------------------------------------------------------------------

    /// <summary>
    /// One saved or playlist row as a song, or null for a podcast episode or an empty slot. The
    /// song sits under "item" since February 2026 and under "track" before; both are read.
    /// </summary>
    internal static ImportTrack? TrackOf(JsonElement row)
    {
        var track = row.TryGetProperty("item", out var i) && i.ValueKind == JsonValueKind.Object ? i
            : row.TryGetProperty("track", out var t) && t.ValueKind == JsonValueKind.Object ? t
            : default;
        if (track.ValueKind != JsonValueKind.Object) return null;
        if (Str(track, "type") is { } type && type != "track") return null;
        var title = Str(track, "name");
        if (string.IsNullOrWhiteSpace(title)) return null;
        var artists = track.TryGetProperty("artists", out var a) && a.ValueKind == JsonValueKind.Array
            ? a.EnumerateArray().Select(artist => Str(artist, "name")).Where(name => !string.IsNullOrWhiteSpace(name)).ToList()
            : [];
        var album = track.TryGetProperty("album", out var al) && al.ValueKind == JsonValueKind.Object ? Str(al, "name") : null;
        var seconds = track.TryGetProperty("duration_ms", out var d) && d.TryGetInt64(out var ms) && ms > 0
            ? (int)Math.Round(ms / 1000.0) : (int?)null;
        var isrc = track.TryGetProperty("external_ids", out var ids) && ids.ValueKind == JsonValueKind.Object
            ? SongIdentity.NormalizeIsrc(Str(ids, "isrc")) : null;
        var artist = string.Join(", ", artists!);
        var id = Str(track, "id");
        return new ImportTrack
        {
            // A local file on someone's Spotify has no id; its words are all there is.
            Key = string.IsNullOrEmpty(id) ? ImportKeys.ForWords(artist, title) : $"spotify:{id}",
            Title = title!.Trim(), Artist = artist, Album = album, Seconds = seconds, Isrc = isrc,
        };
    }

    private static int? Total(JsonElement item, string name) =>
        item.TryGetProperty(name, out var holder) && holder.ValueKind == JsonValueKind.Object
        && holder.TryGetProperty("total", out var total) && total.TryGetInt32(out var n) ? n : null;

    private static string? FirstImage(JsonElement item) =>
        item.TryGetProperty("images", out var images) && images.ValueKind == JsonValueKind.Array
            ? images.EnumerateArray().Select(image => Str(image, "url")).FirstOrDefault(url => url is not null)
            : null;

    private static (string? Error, string? Description) ReadError(string body)
    {
        try
        {
            using var doc = JsonDocument.Parse(body);
            var root = doc.RootElement;
            if (root.TryGetProperty("error", out var error))
            {
                if (error.ValueKind == JsonValueKind.String) return (error.GetString(), Str(root, "error_description"));
                if (error.ValueKind == JsonValueKind.Object) return (Str(error, "reason"), Str(error, "message"));
            }
        }
        catch (JsonException) { }
        return (null, null);
    }

    internal static string? Str(JsonElement e, string name) =>
        e.ValueKind == JsonValueKind.Object && e.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() : null;
}

/// <summary>Keys for songs that have no Spotify id.</summary>
public static class ImportKeys
{
    /// <summary>The song by its words, the same for "Drake feat. Rihanna" and "Drake, Rihanna".</summary>
    public static string ForWords(string? artist, string? title) => $"song:{SongIdentity.MatchKey(artist, title)}";

    /// <summary>Spotify's key for a track id or a spotify:track: URI, or null.</summary>
    public static string? ForSpotify(string? idOrUri)
    {
        if (string.IsNullOrWhiteSpace(idOrUri)) return null;
        var value = idOrUri.Trim();
        const string prefix = "spotify:track:";
        if (value.StartsWith(prefix, StringComparison.OrdinalIgnoreCase)) value = value[prefix.Length..];
        else if (value.Contains("open.spotify.com/track/", StringComparison.OrdinalIgnoreCase))
            value = value[(value.IndexOf("/track/", StringComparison.OrdinalIgnoreCase) + 7)..].Split('?', '/')[0];
        return value.Length is > 10 and < 40 && value.All(char.IsLetterOrDigit) ? $"spotify:{value}" : null;
    }
}
