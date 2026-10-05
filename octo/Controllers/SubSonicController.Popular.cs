using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.AspNetCore.Mvc;
using Octo.Services.Common;
using Octo.Services.CoverArt;
using Octo.Services.Library;
using Octo.Services.Soulseek;

namespace Octo.Controllers;

public partial class SubsonicController
{
    /// <summary>"Popular right now", while it is switched on. Looked up per request so the
    /// controller's constructor stays as it is.</summary>
    private PopularPlaylistService? Popular() =>
        HttpContext?.RequestServices?.GetService<PopularPlaylistService>() is { On: true } popular ? popular : null;

    /// <summary>
    /// "Popular right now" opened by any app: the ping is the sign-in check, as for a mix, so
    /// nothing is made for a caller Navidrome refuses. Outside songs a syncing app already holds
    /// go out as its copy describes them, and the first few are made ready to play, as a
    /// station's are.
    /// </summary>
    private async Task<IActionResult> PopularPlaylistAsync(PopularPlaylistService popular,
        Dictionary<string, string> parameters, string format, string username)
    {
        var authOnly = parameters.ToDictionary(pair => pair.Key, pair => pair.Value);
        authOnly.Remove("id");
        var check = await _proxyService.RelaySafeAsync("rest/ping", authOnly);
        if (!check.Success || check.Body is null || !IsSuccessfulSubsonicResponse(check.Body, format))
            return _responseBuilder.CreateError(format, 40, "Wrong username or password");

        var entries = await popular.SongsAsync(username, parameters, HttpContext.RequestAborted);
        var outside = popular.OutsideSongs(username);
        var outsideIds = outside.Select(song => song.Id).ToHashSet(StringComparer.Ordinal);
        if (_syncCatalog is not null)
            entries = entries.Select(entry =>
                entry["id"]?.GetValue<string>() is { } id && outsideIds.Contains(id)
                && _syncCatalog.TryGetSong(username, id, out var synced)
                    ? JsonSerializer.SerializeToNode(_responseBuilder.ConvertSongToJson(synced))!.AsObject()
                    : entry).ToList();
        _radioQueueStore.Register(outsideIds);
        _ = _metadataService.PrewarmYouTubeIdsAsync(outside, topN: 8);
        return _responseBuilder.CreatePopularPlaylistResponse(format, popular.Playlist(username), entries);
    }

    /// <summary>
    /// The songs whose covers colour "Popular right now"'s cover: its first songs, one per album,
    /// four at most. A library song's cover is read from Navidrome as the listener, an outside
    /// song's is looked up as a station song's is. Only what is already made: drawing the cover
    /// never makes the list.
    /// </summary>
    private IReadOnlyList<CoverSeed> PopularCoverSeeds(PopularPlaylistService popular, string username,
        Dictionary<string, string> parameters)
    {
        if (popular.Songs(username) is not { } songs) return [];
        var outside = popular.OutsideSongs(username).GroupBy(song => song.Id, StringComparer.Ordinal)
            .ToDictionary(group => group.Key, group => group.First(), StringComparer.Ordinal);
        var auth = parameters.Where(pair => pair.Key is not ("id" or "size")).ToDictionary(pair => pair.Key, pair => pair.Value);
        var albums = new HashSet<string>(StringComparer.Ordinal);
        var seeds = new List<CoverSeed>();
        foreach (var entry in songs)
        {
            if (seeds.Count >= 4) break;
            var artist = Text(entry, "artist");
            var title = Text(entry, "title");
            var album = Text(entry, "album") is { Length: > 0 } named ? named : title;
            if (!albums.Add(SongIdentity.Key(artist) + "|" + SongIdentity.Key(album))) continue;

            if (outside.TryGetValue(Text(entry, "id"), out var song))
            {
                if (_coverArtAggregator is not { } covers) continue;
                seeds.Add(new CoverSeed($"song|{SongIdentity.Key(song.Artist)}|{SongIdentity.Key(song.Title)}",
                    ct => covers.GetCoverAsync(new SoulseekRouting
                    {
                        Kind = RoutingKind.Song, Artist = song.Artist, Title = song.Title, Album = song.Album,
                    }, false, ct)));
            }
            else if (Text(entry, "coverArt") is { Length: > 0 } cover)
            {
                seeds.Add(new CoverSeed("navidrome|" + cover, async _ =>
                {
                    var picture = await _proxyService.RelayAsync("rest/getCoverArt",
                        new Dictionary<string, string>(auth) { ["id"] = cover, ["size"] = "128" });
                    return picture.ContentType?.StartsWith("image/", StringComparison.OrdinalIgnoreCase) == true ? picture.Body : null;
                }));
            }
        }
        return seeds;

        static string Text(JsonObject node, string name) =>
            node[name] is JsonValue value && value.TryGetValue<string>(out var text) ? text : "";
    }
}
