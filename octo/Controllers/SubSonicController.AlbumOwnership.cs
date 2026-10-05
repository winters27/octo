using System.Diagnostics;
using System.Text;
using System.Text.Json;
using System.Xml.Linq;
using Microsoft.AspNetCore.Mvc;
using Octo.Models.Domain;
using Octo.Services.Library;

namespace Octo.Controllers;

/// <summary>
/// Outside albums the library already holds. A search, an artist page and an outside album's
/// own page used to judge an outside album by its id or its exact name only, so "HABIBTI",
/// whose eleven songs all sit in the library's "HABIBTI (FOMO)", was listed as not in the
/// library, and its page showed none of them owned. See <see cref="AlbumOwnership"/>.
/// </summary>
public partial class SubsonicController
{
    /// <summary>
    /// How long a request waits for the library and the tracklists it needs. A search started
    /// the wait when its albums came back, alongside its songs, so it rarely adds anything; what
    /// is not ready by then goes out uncounted, as before, and is ready for the next request.
    /// </summary>
    internal static readonly TimeSpan OwnershipWait = TimeSpan.FromMilliseconds(1500);

    /// <summary>At most this many library albums stand in for one outside album held whole.</summary>
    private const int MaxStandIns = 3;

    private AlbumOwnership? Ownership() => HttpContext.RequestServices.GetService<AlbumOwnership>();

    /// <summary>What the library holds of each outside album, once they are found; empty when
    /// it cannot be told.</summary>
    private async Task<IReadOnlyList<AlbumOwnership.Standing>> JudgeAlbumsAsync(Task<IReadOnlyList<Album>> albums)
    {
        if (Ownership() is not { } ownership) return [];
        try
        {
            var found = await albums;
            return found.Count == 0 ? [] : await ownership.JudgeAsync(found, OwnershipWait, HttpContext.RequestAborted);
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            _logger.LogDebug("album ownership failed: {M}", ex.Message);
            return [];
        }
    }

    /// <summary>
    /// A search's outside albums settled against the library: each carries how many of its
    /// songs the library holds; one held whole, or one with a library album's very name and
    /// artist, is left out and the library album that holds it listed instead (fetched as the
    /// caller when the search did not already list it). An outside album whose library album
    /// cannot be listed stays, with its count, so nothing disappears.
    /// </summary>
    private async Task<(List<Album> Outside, List<object> Added)> SettleSearchAlbumsAsync(IReadOnlyList<Album> outside,
        IReadOnlyList<AlbumOwnership.Standing> standings, List<object> localAlbums, string? localContentType,
        Dictionary<string, string> parameters, bool canAdd)
    {
        if (standings.Count == 0) return (outside.ToList(), []);
        var byAlbum = standings.ToDictionary(s => s.Album);
        var listed = localAlbums.Select(AlbumIdOf).OfType<string>().ToHashSet(StringComparer.Ordinal);

        var wanted = new List<string>();
        foreach (var standing in standings)
        {
            if (!canAdd) break;
            foreach (var standIn in standing.StandIns.Take(MaxStandIns))
                if (!listed.Contains(standIn.Id) && !wanted.Contains(standIn.Id)) wanted.Add(standIn.Id);
        }

        var json = localContentType?.Contains("json") == true;
        var fetched = await Task.WhenAll(wanted.Select(id => LibraryAlbumRowAsync(id, parameters, json)));
        var added = new List<object>();
        foreach (var row in fetched)
        {
            if (row is null || AlbumIdOf(row) is not { } id || !listed.Add(id)) continue;
            added.Add(row);
        }

        var kept = new List<Album>();
        foreach (var album in outside)
        {
            if (!byAlbum.TryGetValue(album, out var standing))
            {
                kept.Add(album);
                continue;
            }
            album.OwnedCount = standing.Owned;
            if (standing.StandIns.Take(MaxStandIns).Any(standIn => listed.Contains(standIn.Id))) continue;
            kept.Add(album);
        }
        return (kept, added);
    }

    /// <summary>
    /// An artist page's outside albums settled against the library: each carries its count, and
    /// one held whole by an album the page already lists, or sharing a listed album's name, is
    /// left out. Nothing is added: an artist page lists that artist's albums.
    /// </summary>
    private async Task<List<Album>> SettleArtistAlbumsAsync(IReadOnlyList<Album> outside, IEnumerable<object> localAlbums)
    {
        var standings = await JudgeAlbumsAsync(Task.FromResult(outside));
        if (standings.Count == 0) return outside.ToList();
        var byAlbum = standings.ToDictionary(s => s.Album);
        var listed = localAlbums.Select(AlbumIdOf).OfType<string>().ToHashSet(StringComparer.Ordinal);
        var kept = new List<Album>();
        foreach (var album in outside)
        {
            if (byAlbum.TryGetValue(album, out var standing))
            {
                album.OwnedCount = standing.Owned;
                if (standing.StandIns.Any(standIn => listed.Contains(standIn.Id))) continue;
            }
            kept.Add(album);
        }
        return kept;
    }

    /// <summary>
    /// An outside album's page with the songs the library holds given as the library's own
    /// copies, in the album's order and numbered as the album numbers them, so they play from
    /// the library and show as owned; the rest stay outside songs. Null when the library holds
    /// none of them or cannot be read in time, and the page goes out as before.
    /// </summary>
    private async Task<IActionResult?> OutsideAlbumWithOwnedSongsAsync(Album album, Dictionary<string, string> parameters,
        string format)
    {
        if (album.Songs.Count == 0 || Ownership() is not { } ownership) return null;
        var clock = Stopwatch.StartNew();
        if (await ownership.LibraryAsync(OwnershipWait, HttpContext.RequestAborted) is not { } index) return null;

        var rows = album.Songs
            .Select(song => AlbumOwnership.Find(index, album,
                new AlbumOwnership.CatalogTrack(song.Title, song.Artist, song.Duration, song.Isrc)))
            .ToList();
        album.OwnedCount = rows.Count(row => row is not null);
        if (album.OwnedCount == 0) return null;

        // The library's songs as the caller's Navidrome describes them, one call per library album.
        var copies = new Dictionary<string, Dictionary<string, object>>(StringComparer.Ordinal);
        var holders = rows.OfType<LibrarySongRow>().Select(row => row.AlbumId).OfType<string>().Distinct().ToList();
        foreach (var songs in await Task.WhenAll(holders.Select(id => LibraryAlbumSongsAsync(id, parameters))))
            foreach (var (id, song) in songs) copies.TryAdd(id, song);

        var merged = new List<object>();
        var owned = 0;
        for (var i = 0; i < album.Songs.Count; i++)
        {
            var song = album.Songs[i];
            if (rows[i] is { } row && copies.TryGetValue(row.Id, out var copy))
            {
                var placed = new Dictionary<string, object>(copy);
                if (song.Track is int track) placed["track"] = track;
                if (song.DiscNumber is int disc) placed["discNumber"] = disc;
                merged.Add(placed);
                owned++;
            }
            else merged.Add(_responseBuilder.ConvertSongToJson(song));
        }
        album.OwnedCount = owned;
        if (owned == 0) return null;

        var fields = _responseBuilder.AlbumDetailFields(album);
        fields["song"] = merged;
        _logger.LogDebug("getAlbum '{Artist} - {Album}': {Owned} of {Total} songs given as the library's copies in {Ms} ms",
            album.Artist, album.Title, owned, album.Songs.Count, clock.ElapsedMilliseconds);
        return _responseBuilder.CreateMergedResponse(format, "album", fields);
    }

    /// <summary>A library album as the caller's search would list it, without its songs, in
    /// the search's own format; null when Navidrome does not answer.</summary>
    private async Task<object?> LibraryAlbumRowAsync(string id, Dictionary<string, string> parameters, bool json)
    {
        var asked = LibraryAlbumParameters(id, parameters);
        asked["f"] = json ? "json" : "xml";
        try
        {
            var result = await _proxyService.RelaySafeAsync("rest/getAlbum", asked);
            if (!result.Success || result.Body is not { Length: > 0 } body || IsFailedSubsonicBody(body, result.ContentType)) return null;
            if (result.ContentType?.Contains("json") == true)
            {
                using var doc = JsonDocument.Parse(body);
                if (!doc.RootElement.TryGetProperty("subsonic-response", out var response)
                    || !response.TryGetProperty("album", out var albumElement)) return null;
                if (_responseBuilder.ConvertSubsonicJsonElement(albumElement, true) is not Dictionary<string, object> row) return null;
                row.Remove("song");
                return row;
            }
            var xml = XDocument.Parse(Encoding.UTF8.GetString(body));
            var ns = xml.Root?.GetDefaultNamespace() ?? XNamespace.None;
            if (xml.Root?.Element(ns + "album") is not { } element) return null;
            var album = _responseBuilder.ConvertSubsonicXmlElement(element, "album");
            album.Elements(ns + "song").Remove();
            return album;
        }
        catch (Exception ex)
        {
            _logger.LogDebug("library album {Id} could not be listed: {M}", id, ex.Message);
            return null;
        }
    }

    /// <summary>A library album's songs as the caller's Navidrome describes them, by id.</summary>
    private async Task<List<(string Id, Dictionary<string, object> Song)>> LibraryAlbumSongsAsync(string id,
        Dictionary<string, string> parameters)
    {
        var asked = LibraryAlbumParameters(id, parameters);
        asked["f"] = "json";
        var found = new List<(string, Dictionary<string, object>)>();
        try
        {
            var result = await _proxyService.RelaySafeAsync("rest/getAlbum", asked);
            if (!result.Success || result.Body is not { Length: > 0 } body) return found;
            using var doc = JsonDocument.Parse(body);
            if (!doc.RootElement.TryGetProperty("subsonic-response", out var response)
                || !response.TryGetProperty("album", out var albumElement)
                || !albumElement.TryGetProperty("song", out var songs) || songs.ValueKind != JsonValueKind.Array) return found;
            foreach (var song in songs.EnumerateArray())
            {
                if (_responseBuilder.ConvertSubsonicJsonElement(song, true) is Dictionary<string, object> dict
                    && dict.TryGetValue("id", out var songId) && songId?.ToString() is { Length: > 0 } key)
                    found.Add((key, dict));
            }
        }
        catch (Exception ex)
        {
            _logger.LogDebug("library album {Id} songs could not be read: {M}", id, ex.Message);
        }
        return found;
    }

    /// <summary>The caller's own credentials and client, asking for one album.</summary>
    private static Dictionary<string, string> LibraryAlbumParameters(string id, Dictionary<string, string> parameters)
    {
        var asked = new Dictionary<string, string>(StringComparer.Ordinal);
        foreach (var name in new[] { "u", "p", "t", "s", "apiKey", "v", "c" })
            if (parameters.TryGetValue(name, out var value)) asked[name] = value;
        asked["id"] = id;
        return asked;
    }

    private static string? AlbumIdOf(object album) => album switch
    {
        Dictionary<string, object> dict => dict.TryGetValue("id", out var id) ? id?.ToString() : null,
        XElement element => element.Attribute("id")?.Value,
        _ => null,
    };
}
