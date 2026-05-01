using SixLabors.ImageSharp;
using SixLabors.ImageSharp.PixelFormats;
using SixLabors.ImageSharp.Processing;
using SixLabors.ImageSharp.Formats.Jpeg;

namespace Octo.Services.CoverArt;

/// <summary>
/// Composites the Octo logo onto cover art so radio-sourced tracks are visually
/// distinguishable from local-library tracks in the Subsonic client UI. The
/// previous Tidal-era version drew a procedural diamond; this version loads a
/// real PNG asset shipped in the project's Assets/ directory.
///
/// Logo placement: bottom-right, ~15% of cover dimension, with a soft dark
/// circle behind it so it stays legible on any background. If the asset is
/// missing the badge call returns the original bytes unchanged — never fatal.
/// </summary>
public class CoverArtService
{
    private readonly ILogger<CoverArtService> _logger;
    private Image? _octoLogo;
    private readonly object _logoLock = new();
    private bool _logoLoadAttempted = false;

    public CoverArtService(ILogger<CoverArtService> logger)
    {
        _logger = logger;
    }

    private Image? GetOctoLogo()
    {
        if (_logoLoadAttempted) return _octoLogo;
        lock (_logoLock)
        {
            if (_logoLoadAttempted) return _octoLogo;
            _logoLoadAttempted = true;

            // Asset is copied into the publish output via <CopyToOutputDirectory> in
            // the .csproj. AppContext.BaseDirectory is the directory the running .dll
            // is loaded from, which inside the container is the publish output root.
            var path = System.IO.Path.Combine(AppContext.BaseDirectory, "Assets", "octo_logo.png");
            try
            {
                if (File.Exists(path))
                {
                    _octoLogo = Image.Load<Rgba32>(path);
                    _logger.LogInformation("Octo logo loaded from {Path} ({W}x{H})",
                        path, _octoLogo.Width, _octoLogo.Height);
                }
                else
                {
                    _logger.LogWarning("Octo logo not found at {Path}; radio cover badges disabled", path);
                }
            }
            catch (Exception ex)
            {
                _logger.LogWarning(ex, "Failed to load Octo logo from {Path}", path);
            }

            return _octoLogo;
        }
    }

    /// <summary>
    /// Composites the Octo logo onto the bottom-right of an existing cover art image.
    /// Returns the modified bytes as JPEG, or the original bytes unchanged if the
    /// logo is missing or the source image fails to decode.
    /// </summary>
    public byte[] AddOctoBadge(byte[] originalArt)
    {
        var logo = GetOctoLogo();
        if (logo == null) return originalArt;

        try
        {
            using var image = Image.Load<Rgba32>(originalArt);

            var imageSize = Math.Min(image.Width, image.Height);
            // Logo footprint as a fraction of the cover. 28% reads clearly even
            // at the 100-150px thumbnails most clients use for queue rows.
            var badgeSize = (int)(imageSize * 0.28);
            var padding   = (int)(imageSize * 0.03);

            using var badge = logo.Clone(ctx => ctx.Resize(badgeSize, badgeSize));

            // Top-left placement: most album covers concentrate visual content
            // and text along the center/bottom (artist name, track titles,
            // overlay UI from clients), so top-left is consistently the
            // "quietest" region. Also matches Western reading-order so it's the
            // first thing the eye picks up — exactly what a source indicator
            // wants.
            var badgeX = padding;
            var badgeY = padding;

            image.Mutate(ctx => ctx.DrawImage(badge, new Point(badgeX, badgeY), 1f));

            using var ms = new MemoryStream();
            image.Save(ms, new JpegEncoder { Quality = 90 });
            return ms.ToArray();
        }
        catch (Exception ex)
        {
            _logger.LogError(ex, "Failed to composite Octo badge onto cover art");
            return originalArt;
        }
    }

    /// <summary>
    /// Returns a 600x600 placeholder JPEG with the Octo logo centered on a black
    /// background. Used when iTunes lookup whiffs so we never 404 a cover-art
    /// request — Subsonic clients drop entries whose cover fetch fails.
    /// </summary>
    public byte[] GetPlaceholderCover()
    {
        var logo = GetOctoLogo();
        const int Size = 600;

        try
        {
            using var image = new Image<Rgba32>(Size, Size, new Rgba32(0, 0, 0, 255));

            if (logo != null)
            {
                var logoSize = (int)(Size * 0.55);
                using var sized = logo.Clone(ctx => ctx.Resize(logoSize, logoSize));
                var x = (Size - logoSize) / 2;
                var y = (Size - logoSize) / 2;
                image.Mutate(ctx => ctx.DrawImage(sized, new Point(x, y), 1f));
            }

            using var ms = new MemoryStream();
            image.Save(ms, new JpegEncoder { Quality = 85 });
            return ms.ToArray();
        }
        catch (Exception ex)
        {
            _logger.LogError(ex, "Failed to render Octo placeholder cover");
            // Last-ditch: return a tiny solid-black JPEG so we still respond 200.
            using var fallback = new Image<Rgba32>(64, 64, new Rgba32(0, 0, 0, 255));
            using var ms = new MemoryStream();
            fallback.Save(ms, new JpegEncoder { Quality = 70 });
            return ms.ToArray();
        }
    }
}
