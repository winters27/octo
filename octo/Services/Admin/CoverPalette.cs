using Octo.Services.CoverArt;

namespace Octo.Services.Admin;

/// <summary>
/// The colours the dashboard's Afterglow look takes from a cover: three lights behind the glass
/// and an accent for the controls.
///
/// The colour logic is the station covers' own. A cover's main colours come from
/// <see cref="CoverColours.Swatches"/>, grey ones are left out by the same rule, and the first
/// light and the accent are the colour <see cref="CoverMusic.FromCovers"/> would call the
/// music's: so the dashboard, a station's cover and the apps agree on what colour an album is.
/// Only the lightness and chroma are the dashboard's, chosen for dark glass and dark words.
/// </summary>
public static class CoverPalette
{
    /// <summary>Three lights, strongest first, and the accent, all as #rrggbb.</summary>
    public sealed record Palette(IReadOnlyList<string> Colors, string Accent);

    // CoverMusic's line between a colour and a grey, and the lightness it trusts.
    private const double Colourless = 0.035;
    // Two lights at least this far apart on the hue circle, so the three are three colours.
    private const double MinHueGap = 35;
    // A cover of one colour still lights the page from three sides, from its neighbours.
    private const double NeighbourTurn = 40;

    // Lights: mid and vivid, so they read as colour through dark glass.
    private const double LightL = 0.62;
    private const double LightCMin = 0.10, LightCMax = 0.20;
    // Accent: light, so dark words sit on it and it reads as words on the dark page.
    private const double AccentL = 0.80;
    private const double AccentCMin = 0.08, AccentCMax = 0.16;

    public static Palette? FromSwatches(IReadOnlyList<Swatch>? swatches)
    {
        if (swatches is not { Count: > 0 }) return null;
        var ranked = swatches
            .Select(s => (Lch: CoverColours.ToLch(s.Argb), s.Share))
            .Where(s => s.Lch.C >= Colourless && s.Lch.L is >= 0.15 and <= 0.97)
            // CoverMusic's measure of the strongest colour: how much of the cover, how vivid.
            .OrderByDescending(s => Math.Sqrt(s.Share) * (0.3 + s.Lch.C * 5))
            .Select(s => s.Lch)
            .ToList();
        if (ranked.Count == 0) return null;

        var picks = new List<Lch>();
        foreach (var lch in ranked)
        {
            if (picks.Any(p => CoverColours.HueDistance(p.H, lch.H) < MinHueGap)) continue;
            picks.Add(lch);
            if (picks.Count == 3) break;
        }
        var first = picks[0];
        while (picks.Count < 3)
            picks.Add(first with { H = (first.H + (picks.Count == 1 ? NeighbourTurn : -NeighbourTurn) + 360) % 360 });

        var colors = picks
            .Select(p => Hex(new Lch(LightL, Math.Clamp(p.C, LightCMin, LightCMax), p.H)))
            .ToList();
        var accent = Hex(new Lch(AccentL, Math.Clamp(first.C, AccentCMin, AccentCMax), first.H));
        return new Palette(colors, accent);
    }

    private static string Hex(Lch lch)
    {
        var argb = CoverColours.FromLch(lch);
        return $"#{CoverColours.R(argb):x2}{CoverColours.G(argb):x2}{CoverColours.B(argb):x2}";
    }
}
