using System.Collections.Concurrent;
using Octo.Services.CoverArt;

namespace Octo.Services.Admin;

/// <summary>
/// The palettes of the covers in the fetched-songs log, for the dashboard's light. A cover is read
/// once and its palette kept for as long as Octo runs, so the page asks as often as it likes.
///
/// The browser cannot read these colours itself: the covers come from Deezer's servers, and a
/// page may only read the pixels of an image its own server sent. Octo fetches the cover, keeps
/// only the few colours, and never passes the image on.
/// </summary>
public sealed class AmbientPaletteService(IHttpClientFactory httpFactory, ILogger<AmbientPaletteService> logger)
{
    public const string ClientName = "ambient-palette";

    // A cover is a few hundred kilobytes; anything far past that is not one.
    private const long MaxBytes = 8 * 1024 * 1024;
    private const int MaxRemembered = 512;

    // Null is remembered too: a cover that failed or has no colour is not fetched again.
    private readonly ConcurrentDictionary<string, CoverPalette.Palette?> _palettes = new(StringComparer.Ordinal);

    public async Task<CoverPalette.Palette?> PaletteForAsync(string? coverUrl, CancellationToken ct)
    {
        if (string.IsNullOrWhiteSpace(coverUrl)) return null;
        if (_palettes.TryGetValue(coverUrl, out var known)) return known;
        if (!Uri.TryCreate(coverUrl, UriKind.Absolute, out var uri) || (uri.Scheme != Uri.UriSchemeHttps && uri.Scheme != Uri.UriSchemeHttp))
            return null;

        CoverPalette.Palette? palette = null;
        try
        {
            using var response = await httpFactory.CreateClient(ClientName).GetAsync(uri, HttpCompletionOption.ResponseHeadersRead, ct);
            if (response.IsSuccessStatusCode && (response.Content.Headers.ContentLength ?? 0) <= MaxBytes)
            {
                await using var stream = await response.Content.ReadAsStreamAsync(ct);
                using var limited = new MemoryStream();
                var buffer = new byte[81920];
                int read;
                while ((read = await stream.ReadAsync(buffer, ct)) > 0)
                {
                    if (limited.Length + read > MaxBytes) { limited.SetLength(0); break; }
                    limited.Write(buffer, 0, read);
                }
                // The station covers' reading of a picture: its main colours from a small copy.
                if (limited.Length > 0)
                    palette = CoverPalette.FromSwatches(CoverArtService.SwatchesOf(limited.ToArray()));
            }
        }
        catch (OperationCanceledException) when (ct.IsCancellationRequested)
        {
            throw;
        }
        catch (Exception ex)
        {
            // A cover that cannot be read only costs the page its colour; the dashboard keeps its own.
            logger.LogDebug(ex, "Could not read the colours of a fetched song's cover");
        }

        if (_palettes.Count >= MaxRemembered) _palettes.Clear();
        _palettes[coverUrl] = palette;
        return palette;
    }
}
