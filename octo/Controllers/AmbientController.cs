using Microsoft.AspNetCore.Mvc;
using Octo.Models.Download;
using Octo.Services.Admin;
using Octo.Services.Local;

namespace Octo.Controllers;

/// <summary>
/// The dashboard's light: the newest songs in the fetched-songs log with the colours of their
/// covers. The Status page shows them as Latest fetched, and every page takes its light from the
/// one chosen. Behind the same guard as the rest of /api/admin.
/// </summary>
[ApiController]
[Route("api/admin/ambient")]
public class AmbientController(DownloadHistoryService history, AmbientPaletteService palettes) : ControllerBase
{
    public const int SongCount = 6;
    // How far back the log is read to find that many different covers.
    private const int LookBack = 100;

    public sealed record AmbientSong(string Artist, string Title, string? Album, string Format, string Source,
        string? CoverArtUrl, long SizeBytes, string DownloadedAt, IReadOnlyList<string>? Colors, string? Accent);

    [HttpGet]
    public async Task<IActionResult> Get(CancellationToken ct)
    {
        var recent = NewestPerCover(history.GetRecent(LookBack), SongCount);
        var found = await Task.WhenAll(recent.Select(entry => palettes.PaletteForAsync(entry.CoverArtUrl, ct)));
        var songs = recent.Select((e, i) => new AmbientSong(e.Artist, e.Title, e.Album, e.Format, e.Source,
            e.CoverArtUrl, e.SizeBytes, e.DownloadedAt, found[i]?.Colors, found[i]?.Accent)).ToList();
        return Ok(new { songs });
    }

    /// <summary>
    /// The newest song of each cover, newest first. Four songs from one album are one choice of
    /// light, not four, so an album fetched whole does not fill Recently added with one cover.
    /// A song with no cover counts by its artist and album.
    /// </summary>
    internal static List<DownloadHistoryEntry> NewestPerCover(IEnumerable<DownloadHistoryEntry> newestFirst, int count)
    {
        var seen = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        var picked = new List<DownloadHistoryEntry>();
        foreach (var entry in newestFirst)
        {
            var key = !string.IsNullOrWhiteSpace(entry.CoverArtUrl)
                ? entry.CoverArtUrl!
                : $"{entry.Artist}\n{entry.Album ?? entry.Title}";
            if (!seen.Add(key)) continue;
            picked.Add(entry);
            if (picked.Count == count) break;
        }
        return picked;
    }
}
