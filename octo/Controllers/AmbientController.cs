using Microsoft.AspNetCore.Mvc;
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

    public sealed record AmbientSong(string Artist, string Title, string? Album, string Format, string Source,
        string? CoverArtUrl, long SizeBytes, string DownloadedAt, IReadOnlyList<string>? Colors, string? Accent);

    [HttpGet]
    public async Task<IActionResult> Get(CancellationToken ct)
    {
        var recent = history.GetRecent(SongCount);
        var found = await Task.WhenAll(recent.Select(entry => palettes.PaletteForAsync(entry.CoverArtUrl, ct)));
        var songs = recent.Select((e, i) => new AmbientSong(e.Artist, e.Title, e.Album, e.Format, e.Source,
            e.CoverArtUrl, e.SizeBytes, e.DownloadedAt, found[i]?.Colors, found[i]?.Accent)).ToList();
        return Ok(new { songs });
    }
}
