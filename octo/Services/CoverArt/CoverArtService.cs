using System.Collections.Concurrent;
using SixLabors.Fonts;
using SixLabors.ImageSharp;
using SixLabors.ImageSharp.Drawing.Processing;
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
    private volatile bool _logoLoadAttempted;
    private readonly ConcurrentDictionary<string, byte[]> _radioStationCovers =
        new(StringComparer.Ordinal);

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
            // The logo can land in either Assets/ or wwwroot/Assets/ in the
            // publish output depending on how the csproj globs play out
            // (sometimes the wwwroot/<Content Link=> entry overrides the
            // Assets/<None> entry and only one copy actually ships). Try both
            // so adding the logo isn't tied to which MSBuild quirk wins this
            // build. AppContext.BaseDirectory is the publish root at runtime.
            string[] candidates = new[]
            {
                System.IO.Path.Combine(AppContext.BaseDirectory, "Assets", "octo_logo.png"),
                System.IO.Path.Combine(AppContext.BaseDirectory, "wwwroot", "Assets", "octo_logo.png"),
            };
            string? loadedFrom = null;
            foreach (var path in candidates)
            {
                try
                {
                    if (File.Exists(path))
                    {
                        _octoLogo = Image.Load<Rgba32>(path);
                        loadedFrom = path;
                        break;
                    }
                }
                catch (Exception ex)
                {
                    _logger.LogWarning(ex, "Failed to load Octo logo from {Path}", path);
                }
            }
            if (loadedFrom != null && _octoLogo != null)
            {
                _logger.LogInformation("Octo logo loaded from {Path} ({W}x{H})",
                    loadedFrom, _octoLogo.Width, _octoLogo.Height);
            }
            else
            {
                _logger.LogWarning("Octo logo not found at any of: {Paths}; radio cover badges disabled",
                    string.Join(", ", candidates));
            }

            // Publish completion only after the image reference is ready. The
            // volatile write prevents concurrent first requests from observing
            // "attempted" while _octoLogo is still temporarily null.
            _logoLoadAttempted = true;
            return _octoLogo;
        }
    }

    private Image? CloneOctoLogo(int width, int height)
    {
        var logo = GetOctoLogo();
        if (logo is null) return null;
        // ImageSharp does not promise concurrent processing operations on the
        // same Image instance. Keep only the clone/resize inside this lock;
        // each caller renders its independent clone concurrently afterward.
        lock (_logoLock)
            return logo.Clone(ctx => ctx.Resize(width, height));
    }

    /// <summary>
    /// Composites the Octo logo onto the bottom-right of an existing cover art image.
    /// Returns the modified bytes as JPEG, or the original bytes unchanged if the
    /// logo is missing or the source image fails to decode.
    /// </summary>
    public byte[] AddOctoBadge(byte[] originalArt)
    {
        try
        {
            using var image = Image.Load<Rgba32>(originalArt);

            var imageSize = Math.Min(image.Width, image.Height);
            // Logo footprint as a fraction of the cover. 28% reads clearly even
            // at the 100-150px thumbnails most clients use for queue rows.
            var badgeSize = (int)(imageSize * 0.28);
            var padding   = (int)(imageSize * 0.03);

            using var badge = CloneOctoLogo(badgeSize, badgeSize);
            if (badge is null) return originalArt;

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
    /// <param name="branded">
    /// Whether to stamp the Octo logo. True for external tracks, where the badge
    /// says where the track came from. **False for anything in the user's own
    /// library**: a local file that simply has no embedded art, or whose art could
    /// not be read in time, is not Octo's, and branding it reads as Octo claiming
    /// a song the user already owned.
    /// </param>
    public byte[] GetPlaceholderCover(bool branded = true)
    {
        const int Size = 600;

        try
        {
            using var image = new Image<Rgba32>(Size, Size, new Rgba32(0, 0, 0, 255));

            if (branded)
            {
                var logoSize = (int)(Size * 0.55);
                using var sized = CloneOctoLogo(logoSize, logoSize);
                if (sized is not null)
                {
                    var x = (Size - logoSize) / 2;
                    var y = (Size - logoSize) / 2;
                    image.Mutate(ctx => ctx.DrawImage(sized, new Point(x, y), 1f));
                }
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

    /// <summary>
    /// Uses the established Octo placeholder artwork for an Internet Radio
    /// station, adding only the station's current display name. The bounded
    /// cache keeps ordinary Subsonic cover refreshes from repeatedly rendering
    /// the same image while allowing renamed stations to receive new artwork.
    /// </summary>
    public byte[] GetRadioStationCover(string stationName)
    {
        var name = string.IsNullOrWhiteSpace(stationName) ? "Octo Radio" : stationName.Trim();
        if (_radioStationCovers.Count >= 128) _radioStationCovers.Clear();
        return _radioStationCovers.GetOrAdd(name, RenderRadioStationCover);
    }

    private byte[] RenderRadioStationCover(string stationName)
    {
        const int size = 600;

        try
        {
            using var image = new Image<Rgba32>(size, size, new Rgba32(0, 0, 0, 255));
            const int logoSize = 300;
            using (var sized = CloneOctoLogo(logoSize, logoSize))
            {
                if (sized is not null)
                    image.Mutate(ctx => ctx.DrawImage(sized, new Point(150, 78), 1f));
            }

            var families = SystemFonts.Families.ToList();
            if (families.Count > 0)
            {
                var family = families.FirstOrDefault(item =>
                    item.Name.Equals("DejaVu Sans", StringComparison.OrdinalIgnoreCase));
                if (string.IsNullOrEmpty(family.Name)) family = families[0];
                var fontSize = 52f;
                Font font;
                FontRectangle measured;
                do
                {
                    font = family.CreateFont(fontSize, FontStyle.Bold);
                    measured = TextMeasurer.MeasureSize(stationName, new TextOptions(font));
                    fontSize -= 2f;
                } while (measured.Width > 520f && fontSize >= 22f);

                // This is the dominant purple in octo_logo.png. Keeping the
                // label here makes the station name read as part of the existing
                // brand rather than as client-provided album metadata.
                var purple = new Color(new Rgba32(147, 118, 255, 255));
                var origin = new PointF((size - measured.Width) / 2f, 432f);
                image.Mutate(ctx => ctx.DrawText(stationName, font, purple, origin));
            }

            using var ms = new MemoryStream();
            image.Save(ms, new JpegEncoder { Quality = 88 });
            return ms.ToArray();
        }
        catch (Exception ex)
        {
            _logger.LogError(ex, "Failed to render radio station cover for {Station}", stationName);
            return GetPlaceholderCover();
        }
    }
}
