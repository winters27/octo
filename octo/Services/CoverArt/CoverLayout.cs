using System.Text;

namespace Octo.Services.CoverArt;

// Where the words of a cover go and how big, as the Octo apps and the design's reference set
// them (app.winters.octo.covers; tools/cover-art/reference.py). The background and the veil
// under the words are CoverBackgrounds' and CoverVeil's.

public enum CoverAlign { Left, Right }

/// <summary>The kinds of writing a cover sets differently.</summary>
public enum CoverScript { Latin, Wide, Tall, Emoji }

/// <summary>How words are set: size in pixels, weight, tracking and line height (shares of the size), most lines.</summary>
public sealed record CoverType(float SizePx, int Weight, float Tracking, float LineHeight, int MaxLines);

/// <summary>What the text engine found: lines, the widest line, the height, and whether words were cut.</summary>
public sealed record Measured(int Lines, float Width, float Height, bool Cut);

/// <summary>The text engine, as the cover's design needs it.</summary>
public interface ICoverTypesetter
{
    /// <summary>The text wrapped in <paramref name="width"/>, at most <c>type.MaxLines</c> lines.</summary>
    Measured Measure(string text, CoverType type, float width);

    /// <summary>The text on one line.</summary>
    float WidthOf(string text, CoverType type);
}

public enum WordsRole { Title, Line, Footer }

/// <summary>Words on a cover: what, how set, the box they are set in, how they sit in it, their colour, and which block they are.</summary>
public sealed record CoverWords(string Text, CoverType Type, float Left, float Top, float Width, CoverAlign Align, int Ink, Measured Measured,
    WordsRole Role = WordsRole.Title)
{
    /// <summary>Where the letters themselves are: left, top, right, bottom.</summary>
    public float[] Inked
    {
        get
        {
            var w = Math.Min(Measured.Width, Width);
            var x = Align == CoverAlign.Left ? Left : Left + Width - w;
            return [x, Top, x + w, Top + Measured.Height];
        }
    }
}

/// <summary>What a cover is made from: whose it is (its id), the name, the light line, the foot line, and its music's colours.</summary>
public sealed record CoverSpec(string Id, string Name, string? Line, string? Footer, CoverMusic? Music);

public sealed record FittedText(string Text, CoverType Type, Measured Measured);

public static class CoverLayout
{
    /// <summary>Where the words go: the name, the light line under it, and the foot line, each white at its opacity.</summary>
    public static List<CoverWords> Words(CoverSpec spec, int side, ICoverTypesetter setter, CoverBook book)
    {
        var layout = book.Layout;
        float s = side;
        var align = IsRightToLeft(spec.Name) ? CoverAlign.Right : CoverAlign.Left;
        var name = spec.Name.Trim();
        var white = CoverColours.White;
        // A tiny cover is its artwork only.
        if (side < layout.TinyBelowPx) return [];
        float margin = CoverColours.Round(s * layout.Margin);
        var width = s - 2 * margin;
        var output = new List<CoverWords>();

        // The foot line first, so the name knows how far down it may go.
        var floor = s - margin;
        var footerText = spec.Footer?.Trim() is { Length: > 0 } f0 && side >= layout.Footer.ShowFromPx ? f0 : null;
        if (footerText is not null)
        {
            var f = layout.Footer;
            var look = new CoverType(0f, f.Weight, f.Tracking, LineHeight(ScriptOf(footerText), f.LineHeight), 1);
            var size = MathF.Max(f.MinPx, s * f.Size);
            var fit = FitText(footerText, width, s * 0.2f, 1, f.MinPx, size, look, setter);
            var top = s - s * f.Bottom - fit.Measured.Height;
            output.Add(new CoverWords(footerText, fit.Type, margin, top, width, align, CoverColours.Alpha(white, f.Opacity), fit.Measured, WordsRole.Footer));
            floor = top - s * 0.04f;
        }
        if (name.Length == 0) return output;

        var t = layout.Title;
        var titleTop = s * t.Top;
        var lineText = spec.Line?.Trim() is { Length: > 0 } l0 && side >= layout.Line.ShowFromPx && !SaysWhatItIs(name) ? l0 : null;
        var lineRoom = lineText is null ? 0f : MathF.Max(s * t.WrapSize, t.WrapMinPx) * layout.Line.ShareOfTitle * layout.Line.LineHeight;
        var room = floor - titleTop - lineRoom;
        var titleLook = new CoverType(0f, t.Weight, t.Tracking, LineHeight(ScriptOf(name), t.LineHeight), 1);
        var minPx = MathF.Max(t.MinPx, s * t.MinSize);
        var oneLineLeast = MathF.Max(minPx, MathF.Max(s * t.OneLineDownTo, t.OneLineMinPx));
        var wrapMost = MathF.Max(minPx, MathF.Max(s * t.WrapSize, MathF.Min(t.WrapMinPx, s * t.Size)));
        var title = (oneLineLeast <= s * t.Size ? FitOrNull(name, width, room, 1, oneLineLeast, s * t.Size, titleLook, setter) : null)
            ?? FitText(name, width, room, t.MaxLines, minPx, wrapMost, titleLook, setter);
        output.Add(new CoverWords(name, title.Type, margin, titleTop, width, align, white, title.Measured));

        if (lineText is not null)
        {
            var l = layout.Line;
            var lineTop = titleTop + title.Measured.Height;
            var most = MathF.Max(l.MinPx, title.Type.SizePx * l.ShareOfTitle);
            var lineLook = new CoverType(0f, l.Weight, l.Tracking, LineHeight(ScriptOf(lineText), l.LineHeight), 1);
            var fit = FitText(lineText, width, s, 1, MathF.Min(l.MinPx, most), most, lineLook, setter);
            if (lineTop + fit.Measured.Height <= floor)
                output.Add(new CoverWords(lineText, fit.Type, margin, lineTop, width, align, white, fit.Measured, WordsRole.Line));
        }
        return output;
    }

    /// <summary>
    /// The largest whole-pixel size from <paramref name="minPx"/> to <paramref name="maxPx"/> at
    /// which the text fits the box in at most <paramref name="maxLines"/> lines with no word
    /// broken, or null when even the smallest does not.
    /// </summary>
    public static FittedText? FitOrNull(string text, float width, float height, int maxLines, float minPx, float maxPx,
        CoverType look, ICoverTypesetter setter)
    {
        var runs = UnbreakableRuns(text);
        CoverType TypeAt(float size) => look with { SizePx = size, MaxLines = maxLines };
        Measured? Fits(float size)
        {
            var type = TypeAt(size);
            if (runs.Any(run => setter.WidthOf(run, type) > width)) return null;
            var measured = setter.Measure(text, type, width);
            return !measured.Cut && measured.Lines <= maxLines && measured.Height <= height ? measured : null;
        }
        var low = MathF.Max(MathF.Floor(minPx), 1f);
        var high = MathF.Max(MathF.Floor(maxPx), low);
        if (Fits(high) is { } atHigh) return new FittedText(text, TypeAt(high), atHigh);
        if (Fits(low) is not { } best) return null;
        while (high - low > 1f)
        {
            var mid = MathF.Floor((low + high) / 2);
            if (Fits(mid) is { } measured)
            {
                low = mid;
                best = measured;
            }
            else
            {
                high = mid;
            }
        }
        return new FittedText(text, TypeAt(low), best);
    }

    /// <summary>As <see cref="FitOrNull"/>, but when nothing fits: the smallest size, as many lines as the box holds, the rest cut.</summary>
    public static FittedText FitText(string text, float width, float height, int maxLines, float minPx, float maxPx,
        CoverType look, ICoverTypesetter setter)
    {
        if (FitOrNull(text, width, height, maxLines, minPx, maxPx, look, setter) is { } fitted) return fitted;
        var size = MathF.Max(MathF.Floor(minPx), 1f);
        var lines = Math.Clamp((int)(height / (size * look.LineHeight)), 1, maxLines);
        var type = look with { SizePx = size, MaxLines = lines };
        return new FittedText(text, type, setter.Measure(text, type, width));
    }

    // ---------------------------------------------------------------- writing

    private static bool IsEmoji(int cp) =>
        cp is (>= 0x1F000 and <= 0x1FAFF) or (>= 0x2600 and <= 0x27BF) or (>= 0x2B00 and <= 0x2BFF);

    internal static bool IsWide(int cp) =>
        cp is (>= 0x1100 and <= 0x11FF) or (>= 0x2E80 and <= 0x2FDF) or (>= 0x3040 and <= 0x30FF) or (>= 0x3100 and <= 0x312F)
            or (>= 0x3130 and <= 0x318F) or (>= 0x31A0 and <= 0x31BF) or (>= 0x31F0 and <= 0x31FF) or (>= 0x3400 and <= 0x4DBF)
            or (>= 0x4E00 and <= 0x9FFF) or (>= 0xA960 and <= 0xA97F) or (>= 0xAC00 and <= 0xD7FF) or (>= 0xF900 and <= 0xFAFF)
            or (>= 0xFF66 and <= 0xFF9F) or (>= 0x20000 and <= 0x3FFFF) or 0x3005 or 0x3006 or 0x3007;

    private static bool IsSpaced(int cp) =>
        cp is (>= 0x0041 and <= 0x024F) or (>= 0x1E00 and <= 0x1EFF) or (>= 0x0370 and <= 0x03FF) or (>= 0x1F00 and <= 0x1FFF)
            or (>= 0x0400 and <= 0x052F) or (>= 0x0530 and <= 0x058F) or (>= 0x10A0 and <= 0x10FF) or (>= 0x2C00 and <= 0x2C7F)
            or (>= 0xA720 and <= 0xA7FF) or (>= 0xFF21 and <= 0xFF5A);

    private static bool IsRtlLetter(int cp) =>
        cp is (>= 0x0590 and <= 0x05FF) or (>= 0x0600 and <= 0x06FF) or (>= 0x0700 and <= 0x074F) or (>= 0x0750 and <= 0x077F)
            or (>= 0x0780 and <= 0x07BF) or (>= 0x07C0 and <= 0x07FF) or (>= 0x0860 and <= 0x08FF) or (>= 0xFB1D and <= 0xFDFF)
            or (>= 0xFE70 and <= 0xFEFF) or (>= 0x1EE00 and <= 0x1EEFF);

    private static IEnumerable<Rune> Runes(string text) => text.EnumerateRunes();

    /// <summary>The writing most of the text is in; digits, spaces and marks do not count.</summary>
    public static CoverScript ScriptOf(string text)
    {
        int latin = 0, wide = 0, tall = 0, emoji = 0;
        foreach (var rune in Runes(text))
        {
            var cp = rune.Value;
            if (IsEmoji(cp)) emoji++;
            else if (!Rune.IsLetter(rune)) { }
            else if (IsWide(cp)) wide++;
            else if (IsSpaced(cp)) latin++;
            else tall++;
        }
        var most = Math.Max(Math.Max(latin, wide), Math.Max(tall, emoji));
        if (most == 0 || most == latin) return CoverScript.Latin;
        if (most == wide) return CoverScript.Wide;
        return most == tall ? CoverScript.Tall : CoverScript.Emoji;
    }

    /// <summary>Whether the text reads right to left: its first letter decides.</summary>
    public static bool IsRightToLeft(string text)
    {
        foreach (var rune in Runes(text))
            if (Rune.IsLetter(rune)) return IsRtlLetter(rune.Value);
        return false;
    }

    /// <summary>The space between lines for this writing: room for marks in the scripts that have them.</summary>
    public static float LineHeight(CoverScript script, float wanted) => script switch
    {
        CoverScript.Latin => wanted,
        CoverScript.Wide or CoverScript.Emoji => MathF.Max(wanted, 1.15f),
        _ => MathF.Max(wanted, 1.4f),
    };

    private static readonly HashSet<string> KindWords = new(StringComparer.OrdinalIgnoreCase)
    {
        "Mix", "Mixes", "Radio", "Radios", "Station", "Stations", "Playlist", "Playlists", "Chart", "Charts",
    };

    /// <summary>Whether the name's last word already says what kind of list it is, so no light line repeats it.</summary>
    public static bool SaysWhatItIs(string name)
    {
        var words = System.Text.RegularExpressions.Regex.Split(name.Trim(), @"[^\w]+").Where(w => w.Length > 0).ToList();
        return words.Count > 0 && KindWords.Contains(words[^1]);
    }

    /// <summary>The pieces that cannot be broken across lines: words between spaces; wide characters each stand alone.</summary>
    public static List<string> UnbreakableRuns(string text)
    {
        var runs = new List<string>();
        var word = new StringBuilder();
        void End()
        {
            if (word.Length > 0) runs.Add(word.ToString());
            word.Clear();
        }
        foreach (var rune in Runes(text))
        {
            if (Rune.IsWhiteSpace(rune)) End();
            else if (IsWide(rune.Value))
            {
                End();
                runs.Add(rune.ToString());
            }
            else word.Append(rune.ToString());
        }
        End();
        return runs;
    }
}
