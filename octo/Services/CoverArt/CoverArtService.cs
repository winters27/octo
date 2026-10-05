using System.Collections.Concurrent;
using SixLabors.ImageSharp;
using SixLabors.ImageSharp.Formats;
using SixLabors.ImageSharp.PixelFormats;
using SixLabors.ImageSharp.Processing;
using SixLabors.ImageSharp.Formats.Jpeg;

namespace Octo.Services.CoverArt;

/// <summary>
/// Covers Octo draws itself: the small Octo badge on covers of songs found outside the library
/// (for third-party clients), the placeholder when no cover can be had, and the covers of the
/// lists Octo makes (radio stations and mixes), designed like the Octo apps' playlist covers:
/// a painted background picked to match the list's music, darkened only under the words, with
/// its name in white. A picture someone put in the covers folder replaces a list's cover.
/// </summary>
public class CoverArtService
{
    private readonly ILogger<CoverArtService> _logger;
    private Image? _octoLogo;
    private readonly object _logoLock = new();
    private volatile bool _logoLoadAttempted;
    private readonly ConcurrentDictionary<string, byte[]> _namedCovers = new(StringComparer.Ordinal);
    private readonly ConcurrentDictionary<string, (CoverMusic? Music, DateTime Until)> _musicMemo = new(StringComparer.Ordinal);
    private readonly string? _coversDirectory;
    private readonly CoverBook _book;
    private readonly CoverTypesetter _setter = new();

    /// <summary>List covers are drawn at the size asked, within these bounds.</summary>
    internal const int MinCoverSize = 600;
    internal const int MaxCoverSize = 1200;

    private static readonly TimeSpan SeedWait = TimeSpan.FromSeconds(4);
    private static readonly TimeSpan MusicHit = TimeSpan.FromHours(12);
    private static readonly TimeSpan MusicGrey = TimeSpan.FromMinutes(10);
    private static readonly TimeSpan MusicMiss = TimeSpan.FromMinutes(1);
    private static readonly JpegEncoder Jpeg = new() { Quality = 92, ColorType = JpegEncodingColor.YCbCrRatio444 };

    /// <param name="coversDirectory">Pictures that replace a generated cover, named after the
    /// playlist (/app/config/covers).</param>
    /// <param name="book">The cover design; the one shipped in the app by default.</param>
    public CoverArtService(ILogger<CoverArtService> logger, string? coversDirectory = null, CoverBook? book = null)
    {
        _logger = logger;
        _coversDirectory = string.IsNullOrWhiteSpace(coversDirectory) ? null : coversDirectory;
        _book = book ?? CoverBook.Default;
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

    /// <summary>Draws one cover and forgets it, so the fonts are loaded before anyone asks.</summary>
    public void Warm()
    {
        try
        {
            Render(Spec("Warm Radio", ListKinds.Radio, 1, null), MinCoverSize);
            _ = CoverFonts.Fallbacks.Count;
        }
        catch (Exception ex)
        {
            _logger.LogWarning(ex, "The cover fonts could not be loaded; list covers will be plain placeholders");
        }
    }

    /// <summary>A radio station's cover from its name alone. It's a playlist like any other, with no Octo mark.</summary>
    public byte[] GetRadioStationCover(string stationName, int? size = null)
    {
        var name = string.IsNullOrWhiteSpace(stationName) ? "Octo Radio" : stationName.Trim();
        return GetNamedCover(name, null, size, ListKinds.Radio);
    }

    /// <summary>A list's cover from its name alone: its genre's or decade's colour, or its design's own.</summary>
    public byte[] GetNamedCover(string name, string? label = null, int? size = null, string kind = ListKinds.Mix) =>
        GetListCoverAsync(new ListCover(name, label, kind), size).GetAwaiter().GetResult();

    /// <summary>
    /// A list's cover. In order: a picture in the covers folder named after the list or its genre
    /// or decade; a painted background matched to the colours of its seed songs' covers, else to
    /// its genre's or decade's colour, else picked by its name; and last a plain placeholder,
    /// never the logo. A replaced picture or a new seed cover shows without a restart.
    /// </summary>
    public async Task<byte[]> GetListCoverAsync(ListCover list, int? requestedSize = null, CancellationToken ct = default)
    {
        var size = CoverSize(requestedSize);
        var display = string.IsNullOrWhiteSpace(list.Name) ? "Octo" : list.Name.Trim();
        var lookup = string.IsNullOrWhiteSpace(list.Label) ? display : list.Label.Trim();
        try
        {
            if (FindOverride(display, lookup) is { } custom)
            {
                var overrideKey = $"override\n{custom}\n{File.GetLastWriteTimeUtc(custom).Ticks}\n{size}";
                if (_namedCovers.TryGetValue(overrideKey, out var cached)) return cached;
                if (LoadOverride(custom, size) is { } picture) return Remember(overrideKey, picture);
            }

            var music = await SeedMusicAsync(list, display, ct) ?? FallbackMusic(display, lookup);
            var spec = Spec(display, list.Kind, list.SongCount, music);
            var background = CoverBackgrounds.Choose(_book, music, spec.Id);
            var key = $"{_book.Version}\n{spec.Id}\n{spec.Name}\n{spec.Line}\n{spec.Footer}\n{background}\n{size}";
            if (_namedCovers.TryGetValue(key, out var drawn)) return drawn;
            return Remember(key, Render(spec, size));
        }
        catch (OperationCanceledException) when (ct.IsCancellationRequested)
        {
            throw;
        }
        catch (Exception ex)
        {
            _logger.LogWarning(ex, "Could not draw a cover for {Name}", display);
        }
        // Nothing could be drawn: a plain placeholder, never the logo. A playlist's cover is its
        // own, whether Octo made the list or the listener did.
        return GetPlaceholderCover(branded: false);
    }

    private byte[] Remember(string key, byte[] bytes)
    {
        if (_namedCovers.Count >= 256) _namedCovers.Clear();
        return _namedCovers.GetOrAdd(key, bytes);
    }

    internal static int CoverSize(int? requested) =>
        Math.Clamp(requested is > 0 ? requested.Value : MinCoverSize, MinCoverSize, MaxCoverSize);

    /// <summary>The lightness a genre's or decade's stand-in colour is given: the middle of the library's.</summary>
    internal const double GenreLightness = 0.62;

    /// <summary>A stand-in for the music when the songs give none: the genre's or decade's hue, else nothing.</summary>
    internal CoverMusic? FallbackMusic(string display, string lookup) =>
        (_book.ListHue(lookup) ?? _book.ListHue(display)) is { } hue ? CoverMusic.Of(hue.Hue, hue.Chroma, GenreLightness) : null;

    /// <summary>
    /// What goes on a list's cover. The name is the list's own, less a trailing "Radio" on a
    /// station, "Chart" on a chart or "Mix" on a mix, since the light line under it says which
    /// it is; "Your Mix" stays whole. A name that still ends in a word saying what it is ("Your
    /// Mix", "Discovery Mix") has no light line, as the name already says it. The foot line is
    /// its song count when known. The design is picked by the list's full name, the same on every request.
    /// </summary>
    internal static CoverSpec Spec(string display, string? kind, int? songCount, CoverMusic? music)
    {
        var (line, suffix) = kind switch
        {
            ListKinds.Radio => ("Station", " Radio"),
            ListKinds.Chart => ("Chart", " Chart"),
            _ => ("Mix", " Mix"),
        };
        var title = display;
        if (display.EndsWith(suffix, StringComparison.OrdinalIgnoreCase))
        {
            var head = display[..^suffix.Length].Trim();
            if (head.Length > 1 && !Possessives.Contains(head)) title = head;
        }
        return new CoverSpec(display, title, CoverLayout.SaysWhatItIs(title) ? null : line, Footer(songCount), music);
    }

    private static readonly HashSet<string> Possessives = new(StringComparer.OrdinalIgnoreCase) { "Your", "My", "Our" };

    internal static string? Footer(int? songs) => songs switch
    {
        1 => "1 song",
        > 1 => songs.Value.ToString("N0", System.Globalization.CultureInfo.InvariantCulture) + " songs",
        _ => null,
    };

    internal byte[] Render(CoverSpec spec, int size, bool drawWords = true)
    {
        using var image = Paint(spec, size, drawWords);
        using var ms = new MemoryStream();
        image.Save(ms, Jpeg);
        return ms.ToArray();
    }

    internal Image<Rgb24> Paint(CoverSpec spec, int size, bool drawWords = true) =>
        CoverPainter.Paint(_book, Compose(spec, size), _setter, drawWords);

    /// <summary>The cover's background, how it is turned, and where its words go.</summary>
    internal CoverArt Compose(CoverSpec spec, int size) =>
        new(size, CoverBackgrounds.Choose(_book, spec.Music, spec.Id), CoverBackgrounds.Orientation(_book, spec.Id),
            CoverLayout.Words(spec, size, _setter, _book));

    /// <summary>
    /// Colours from the list's seed covers: fetched until two pictures are in hand, all within a
    /// few seconds. Remembered by which seeds they were, so a new seed can change the cover and an
    /// unchanged one costs nothing. Null when there are none to read.
    /// </summary>
    private async Task<CoverMusic?> SeedMusicAsync(ListCover list, string display, CancellationToken ct)
    {
        if (list.Seeds is null) return null;
        using var budget = CancellationTokenSource.CreateLinkedTokenSource(ct);
        budget.CancelAfter(SeedWait);
        try
        {
            var seeds = await list.Seeds(budget.Token);
            if (seeds.Count == 0) return null;
            var memoKey = display + "\n" + string.Join("\n", seeds.Select(seed => seed.Identity));
            if (_musicMemo.TryGetValue(memoKey, out var known) && known.Until > DateTime.UtcNow) return known.Music;

            var covers = new List<IReadOnlyList<Swatch>>();
            foreach (var seed in seeds)
            {
                if (covers.Count >= 2) break;
                try
                {
                    if (await seed.Fetch(budget.Token) is { Length: > 0 } bytes && SwatchesOf(bytes) is { } swatches)
                        covers.Add(swatches);
                }
                catch (Exception ex) when (ex is not OperationCanceledException)
                {
                    _logger.LogDebug(ex, "A seed cover for {Name} could not be fetched", display);
                }
            }
            var music = covers.Count == 0 ? null : CoverMusic.FromCovers(covers);
            var ttl = music is not null ? MusicHit : covers.Count > 0 ? MusicGrey : MusicMiss;
            if (_musicMemo.Count >= 1024) _musicMemo.Clear();
            _musicMemo[memoKey] = (music, DateTime.UtcNow + ttl);
            return music;
        }
        catch (OperationCanceledException) when (!ct.IsCancellationRequested)
        {
            // Out of time: the fallback colours now, the seeds on a later request.
            return null;
        }
    }

    /// <summary>A picture's main colours, read from a small copy of it.</summary>
    internal static IReadOnlyList<Swatch>? SwatchesOf(byte[] picture)
    {
        try
        {
            using var image = Image.Load<Rgba32>(new DecoderOptions { TargetSize = new Size(64, 64) }, picture);
            if (image.Width > 64 || image.Height > 64) image.Mutate(ctx => ctx.Resize(64, 64));
            var pixels = new int[image.Width * image.Height];
            image.ProcessPixelRows(access =>
            {
                for (var y = 0; y < access.Height; y++)
                {
                    var row = access.GetRowSpan(y);
                    for (var x = 0; x < row.Length; x++)
                        pixels[y * image.Width + x] = unchecked((int)0xFF000000) | (row[x].R << 16) | (row[x].G << 8) | row[x].B;
                }
            });
            return CoverColours.Swatches(pixels);
        }
        catch (Exception)
        {
            return null;
        }
    }

    /// <summary>
    /// A picture in the covers folder named after the playlist or its genre or decade. Only ever
    /// inside that folder, whatever the name contains.
    /// </summary>
    private string? FindOverride(string display, string lookup)
    {
        if (_coversDirectory is null || !Directory.Exists(_coversDirectory)) return null;
        var root = System.IO.Path.GetFullPath(_coversDirectory).TrimEnd(System.IO.Path.DirectorySeparatorChar)
            + System.IO.Path.DirectorySeparatorChar;
        foreach (var stem in new[] { SafeName(display), SafeName(lookup) }.Distinct())
        foreach (var extension in new[] { ".jpg", ".jpeg", ".png", ".webp" })
        {
            var path = System.IO.Path.GetFullPath(System.IO.Path.Combine(_coversDirectory, stem + extension));
            if (path.StartsWith(root, StringComparison.Ordinal) && File.Exists(path)) return path;
        }
        return null;
    }

    private static string SafeName(string name)
    {
        var invalid = System.IO.Path.GetInvalidFileNameChars().Concat(['/', '\\']).ToHashSet();
        return new string(name.Select(ch => invalid.Contains(ch) ? '_' : ch).ToArray()).Trim();
    }

    /// <summary>Someone's own picture, cropped to its centre square and sized like every cover.</summary>
    private byte[]? LoadOverride(string path, int size)
    {
        try
        {
            using var image = Image.Load<Rgba32>(path);
            var side = Math.Min(image.Width, image.Height);
            image.Mutate(ctx => ctx
                .Crop(new Rectangle((image.Width - side) / 2, (image.Height - side) / 2, side, side))
                .Resize(size, size));
            using var ms = new MemoryStream();
            image.Save(ms, Jpeg);
            return ms.ToArray();
        }
        catch (Exception ex)
        {
            _logger.LogWarning("The cover {Path} could not be read: {M}", path, ex.Message);
            return null;
        }
    }
}

/// <summary>The kinds of list Octo makes.</summary>
public static class ListKinds
{
    public const string Radio = "radio";
    public const string Mix = "mix";
    public const string Chart = "chart";
}

/// <summary>A list whose cover Octo draws: its name, its genre or decade, its kind, its song
/// count if known, and the songs whose covers give it its colours.</summary>
public sealed record ListCover(string Name, string? Label = null, string Kind = ListKinds.Mix,
    Func<CancellationToken, Task<IReadOnlyList<CoverSeed>>>? Seeds = null, int? SongCount = null);

/// <summary>One seed cover: what it is (so its colours can be remembered by it) and how to get it.</summary>
public sealed record CoverSeed(string Identity, Func<CancellationToken, Task<byte[]?>> Fetch);
