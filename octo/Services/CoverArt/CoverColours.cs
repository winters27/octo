using System.Text;

namespace Octo.Services.CoverArt;

// The colour rules of the cover design, the same maths as the Octo apps' covers
// (app.winters.octo.covers) so a list looks the same wherever it is drawn. Colours
// are 0xAARRGGBB ints; rounding is half up, as the apps round.

/// <summary>A colour as OKLCH: lightness 0 to 1, chroma (0 grey, about 0.3 at the most vivid), hue in degrees.</summary>
public readonly record struct Lch(double L, double C, double H);

/// <summary>One colour of a picture and how much of it that colour covers, 0 to 1.</summary>
public readonly record struct Swatch(int Argb, float Share);

public static class CoverColours
{
    public const int White = unchecked((int)0xFFFFFFFF);
    public const int Black = unchecked((int)0xFF000000);

    internal static int Round(double v) => (int)Math.Floor(v + 0.5);
    internal static int Round(float v) => (int)MathF.Floor(v + 0.5f);

    public static int Hex(string hex) => unchecked((int)0xFF000000) | Convert.ToInt32(hex.TrimStart('#'), 16);

    public static int R(int argb) => (argb >> 16) & 0xFF;
    public static int G(int argb) => (argb >> 8) & 0xFF;
    public static int B(int argb) => argb & 0xFF;
    public static int A(int argb) => (argb >>> 24) & 0xFF;

    private static double Linear(int channel)
    {
        var c = channel / 255.0;
        return c <= 0.04045 ? c / 12.92 : Math.Pow((c + 0.055) / 1.055, 2.4);
    }

    public static Lch ToLch(int argb)
    {
        var r = Linear(R(argb));
        var g = Linear(G(argb));
        var b = Linear(B(argb));
        var l = Math.Cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
        var m = Math.Cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
        var s = Math.Cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);
        var lightness = 0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s;
        var a = 1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s;
        var bb = 0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s;
        var hue = Math.Atan2(bb, a) * 180 / Math.PI;
        if (hue < 0) hue += 360;
        return new Lch(lightness, Math.Sqrt(a * a + bb * bb), hue);
    }

    /// <summary>
    /// The sRGB colour of an OKLCH one, opaque. A colour outside what a screen can show keeps its
    /// lightness and hue and loses chroma until it fits, so it stays the colour it was meant to be,
    /// only quieter.
    /// </summary>
    public static int FromLch(Lch lch)
    {
        var (lo, hi) = (0.0, lch.C);
        var (r, g, b) = LinearRgb(lch.L, hi, lch.H);
        if (!InGamut(r, g, b))
        {
            for (var i = 0; i < 24; i++)
            {
                var mid = (lo + hi) / 2;
                var (mr, mg, mb) = LinearRgb(lch.L, mid, lch.H);
                if (InGamut(mr, mg, mb)) lo = mid; else hi = mid;
            }
            (r, g, b) = LinearRgb(lch.L, lo, lch.H);
        }
        return unchecked((int)0xFF000000) | (Encode(r) << 16) | (Encode(g) << 8) | Encode(b);
    }

    private static (double R, double G, double B) LinearRgb(double lightness, double chroma, double hue)
    {
        var a = chroma * Math.Cos(hue * Math.PI / 180);
        var bb = chroma * Math.Sin(hue * Math.PI / 180);
        var l = Math.Pow(lightness + 0.3963377774 * a + 0.2158037573 * bb, 3);
        var m = Math.Pow(lightness - 0.1055613458 * a - 0.0638541728 * bb, 3);
        var s = Math.Pow(lightness - 0.0894841775 * a - 1.2914855480 * bb, 3);
        return (4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
            -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
            -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s);
    }

    private static bool InGamut(double r, double g, double b) =>
        r is >= -1e-6 and <= 1 + 1e-6 && g is >= -1e-6 and <= 1 + 1e-6 && b is >= -1e-6 and <= 1 + 1e-6;

    private static int Encode(double linear)
    {
        var c = Math.Clamp(linear, 0, 1);
        var srgb = c <= 0.0031308 ? c * 12.92 : 1.055 * Math.Pow(c, 1 / 2.4) - 0.055;
        return Math.Clamp(Round(srgb * 255), 0, 255);
    }

    /// <summary>How far apart two hues are around the circle, 0 to 180 degrees.</summary>
    public static double HueDistance(double a, double b)
    {
        var d = ((a - b) % 360 + 360) % 360;
        return d > 180 ? 360 - d : d;
    }

    /// <summary>How far apart two colours look (0 the same).</summary>
    public static double Distance(Lch x, Lch y)
    {
        var ra = x.H * Math.PI / 180;
        var rb = y.H * Math.PI / 180;
        var da = x.C * Math.Cos(ra) - y.C * Math.Cos(rb);
        var db = x.C * Math.Sin(ra) - y.C * Math.Sin(rb);
        return Math.Sqrt(Math.Pow(x.L - y.L, 2) + da * da + db * db);
    }

    /// <summary>The colour with this opacity.</summary>
    public static int Alpha(int argb, float a) => (Round(Math.Clamp(a, 0f, 1f) * 255) << 24) | (argb & 0xFFFFFF);

    /// <summary>One colour laid over another at the first one's opacity.</summary>
    public static int Over(int top, int under)
    {
        var a = A(top) / 255f;
        if (a >= 1f) return top | unchecked((int)0xFF000000);
        int Channel(int shift)
        {
            var t = (top >> shift) & 0xFF;
            var u = (under >> shift) & 0xFF;
            return Math.Clamp(Round(u + (t - u) * a), 0, 255);
        }
        return unchecked((int)0xFF000000) | (Channel(16) << 16) | (Channel(8) << 8) | Channel(0);
    }

    /// <summary>WCAG relative luminance, with the apps' 0.03928 knee.</summary>
    public static double RelativeLuminance(int argb)
    {
        static double Lin(int channel)
        {
            var c = channel / 255.0;
            return c <= 0.03928 ? c / 12.92 : Math.Pow((c + 0.055) / 1.055, 2.4);
        }
        return 0.2126 * Lin(R(argb)) + 0.7152 * Lin(G(argb)) + 0.0722 * Lin(B(argb));
    }

    /// <summary>How far apart two colours are for reading, from 1 (the same) to 21.</summary>
    public static double ContrastRatio(int a, int b)
    {
        var la = RelativeLuminance(a);
        var lb = RelativeLuminance(b);
        return (Math.Max(la, lb) + 0.05) / (Math.Min(la, lb) + 0.05);
    }

    /// <summary>A number from some text, the same on every device (FNV-1a 64 over its UTF-8), never negative.</summary>
    public static long CoverHash(string text)
    {
        var hash = 0xcbf29ce484222325UL;
        foreach (var b in Encoding.UTF8.GetBytes(text))
        {
            hash ^= b;
            hash *= 0x100000001b3UL;
        }
        return (long)(hash >> 1);
    }

    /// <summary>
    /// The number that picks a list's background and its turn, the same on every device: FNV-1a
    /// 64 over the id's UTF-8, then MurmurHash3's fmix64 so ids that differ only in their last
    /// letter still land far apart, then shifted right once (never negative).
    /// </summary>
    public static long CoverPick(string text)
    {
        var k = 0xcbf29ce484222325UL;
        foreach (var b in Encoding.UTF8.GetBytes(text))
        {
            k ^= b;
            k *= 0x100000001b3UL;
        }
        k ^= k >> 33;
        k *= 0xff51afd7ed558ccdUL;
        k ^= k >> 33;
        k *= 0xc4ceb9fe1a85ec53UL;
        k ^= k >> 33;
        return (long)(k >> 1);
    }

    private sealed class Bucket
    {
        public int Key;
        public int Count;
        public long R, G, B;
        public int Mean => unchecked((int)0xFF000000) | ((int)(R / Count) << 16) | ((int)(G / Count) << 8) | (int)(B / Count);
    }

    /// <summary>
    /// A picture's main colours, most of the picture first, up to <paramref name="most"/>: pixels
    /// counted in coarse buckets (4 bits a channel), every <paramref name="step"/>th one, each
    /// bucket joining the first colour it looks like or starting one of its own.
    /// </summary>
    public static List<Swatch> Swatches(int[] pixels, int step = 3, int most = 6)
    {
        if (pixels.Length == 0) return [];
        var counts = new Dictionary<int, Bucket>();
        for (var i = 0; i < pixels.Length; i += step)
        {
            var p = pixels[i];
            var key = (((p >> 20) & 0xF) << 8) | (((p >> 12) & 0xF) << 4) | ((p >> 4) & 0xF);
            if (!counts.TryGetValue(key, out var bucket)) counts[key] = bucket = new Bucket { Key = key };
            bucket.Count++;
            bucket.R += R(p);
            bucket.G += G(p);
            bucket.B += B(p);
        }
        var found = counts.Values.OrderByDescending(b => b.Count).ThenBy(b => b.Key).ToList();
        var total = (float)found.Sum(b => b.Count);
        var groups = new List<(Lch Seen, Bucket Bucket)>();
        foreach (var bucket in found)
        {
            // Specks of a colour (under 0.2% of the picture) are left out.
            if (bucket.Count < total * 0.002f) break;
            var seen = ToLch(bucket.Mean);
            var near = groups.FirstOrDefault(g => Distance(g.Seen, seen) < 0.09).Bucket;
            if (near is not null)
            {
                near.Count += bucket.Count;
                near.R += bucket.R;
                near.G += bucket.G;
                near.B += bucket.B;
            }
            else if (groups.Count < most * 3)
            {
                groups.Add((seen, new Bucket { Key = bucket.Key, Count = bucket.Count, R = bucket.R, G = bucket.G, B = bucket.B }));
            }
        }
        return groups.Select(g => g.Bucket).OrderByDescending(b => b.Count).Take(most)
            .Select(b => new Swatch(b.Mean, b.Count / total)).ToList();
    }
}

/// <summary>
/// The colour of a list's music that picks its cover's background: hue (whole degrees), chroma
/// and lightness (OKLCH, to 0.001) of the strongest colour of its first covers, rounded as the
/// Octo apps round it so both pick the same background.
/// </summary>
public sealed record CoverMusic(int Hue, double Chroma, double Lightness)
{
    /// <summary>Colours under this chroma read as grey, and give no hue to work from.</summary>
    private const double Colourless = 0.035;

    /// <summary>The colour as short text, for cache keys.</summary>
    public string Key => $"{Hue}.{CoverColours.Round(Chroma * 1000)}.{CoverColours.Round(Lightness * 1000)}";

    public static CoverMusic Of(double hue, double chroma, double lightness) => new(
        (CoverColours.Round(hue) % 360 + 360) % 360,
        CoverColours.Round(Math.Clamp(chroma, 0.0, 0.4) * 1000) / 1000.0,
        CoverColours.Round(Math.Clamp(lightness, 0.0, 1.0) * 1000) / 1000.0);

    /// <summary>
    /// The music's colour from the main colours of some covers (a list of swatches for each):
    /// the strongest by how much of its cover it fills and how vivid it is. Null with no covers
    /// or only grey ones.
    /// </summary>
    public static CoverMusic? FromCovers(IReadOnlyList<IReadOnlyList<Swatch>> covers)
    {
        var withColour = covers.Where(c => c.Count > 0).ToList();
        if (withColour.Count == 0) return null;
        var colourful = withColour.SelectMany(swatches => swatches.Take(4)
                .Select(s => (Lch: CoverColours.ToLch(s.Argb), Weight: (double)s.Share / withColour.Count)))
            .Where(s => s.Lch.C >= Colourless && s.Lch.L is >= 0.15 and <= 0.97)
            .ToList();
        if (colourful.Count == 0) return null;
        var first = colourful.MaxBy(s => Math.Sqrt(s.Weight) * (0.3 + s.Lch.C * 5));
        return Of(first.Lch.H, first.Lch.C, first.Lch.L);
    }
}
