using System.Globalization;
using System.Text;
using System.Text.RegularExpressions;

namespace Octo.Services.Common;

/// <summary>What a comparison of two songs found.</summary>
public enum SongVerdict
{
    /// <summary>Another song, or another artist's song.</summary>
    Different,

    /// <summary>The same song in another version: live against studio, a remix, a sped-up
    /// upload, a different guest, or a length that is too far off.</summary>
    SameSongDifferentVersion,

    /// <summary>The same recording.</summary>
    Same,
}

/// <summary>A verdict, how sure it is (0 to 1), and why, in words a log or a person can read.</summary>
public sealed record SongMatch(SongVerdict Verdict, double Confidence, string Reason)
{
    public bool IsSame => Verdict == SongVerdict.Same;
}

/// <summary>How one call site wants songs compared. The defaults are the strict reading.</summary>
public sealed record SongMatchOptions
{
    public static readonly SongMatchOptions Default = new();

    /// <summary>How far apart two known lengths may be and still be one recording. Null
    /// compares no lengths.</summary>
    public int? LengthToleranceSeconds { get; init; } = SongIdentity.LengthToleranceSeconds;

    /// <summary>When on, a bracketed subtitle only one title carries ("Blue (Da Ba Dee)"
    /// against "Blue") makes them different songs. Off, it only lowers the confidence.</summary>
    public bool ExtrasMustAgree { get; init; }

    /// <summary>Version markers this caller treats as the same recording, on top of the ones
    /// that always are (remaster, explicit, original mix, album version, mono, stereo).</summary>
    public IReadOnlyCollection<string> AlsoNeutral { get; init; } = [];
}

/// <summary>One side of a comparison. Seconds is the length when it is known.</summary>
public sealed record SongRef(string? Title, string? Artist, double? Seconds = null)
{
    /// <summary>The ISRCs this side is known by, as a source wrote them: a Deezer track, a
    /// file's tags, a MusicBrainz recording. Anything that is not a valid ISRC is ignored.</summary>
    public IReadOnlyList<string?> Isrcs { get; init; } = [];
}

/// <summary>One query to try against a search API, in the order <see cref="SongIdentity.QueryVariants"/>
/// gives them. Artist is empty for the title-only query.</summary>
public sealed record SongQuery(string Title, string Artist)
{
    /// <summary>The artist and the title as one free-text query.</summary>
    public string Text => Artist.Length == 0 ? Title : $"{Artist} {Title}";
}

/// <summary>A title read into its parts.</summary>
/// <param name="Raw">The title as given.</param>
/// <param name="Core">What is left once track numbers, a leading "Artist - ", brackets, features
/// and version tails are taken off: the title a person would say.</param>
/// <param name="Key">The core compared exactly: casefolded, accents and punctuation ignored,
/// with any part number ("Pt. 2") kept.</param>
/// <param name="LooseKey">The key again with stylized characters read as letters ($ as s, 0 as
/// o, 3 as e, @ as a, ! as i). An additional key, never a replacement.</param>
/// <param name="Versions">Every version marker found, canonical names, sorted.</param>
/// <param name="Remixers">Keys of whoever a remix, mix, dub or edit is credited to.</param>
/// <param name="Featured">Artists the title credits, "(feat. X)".</param>
/// <param name="Extras">Keys of bracketed subtitles that are none of the above, "(Da Ba Dee)".</param>
/// <param name="ArtistFromTitle">The artist a title named in "Artist - Title" form, or null.</param>
public sealed record SongTitle(
    string Raw, string Core, string Key, string LooseKey,
    IReadOnlyList<string> Versions, IReadOnlyList<string> Remixers,
    IReadOnlyList<string> Featured, IReadOnlyList<string> Extras, string? ArtistFromTitle)
{
    /// <summary>The core with its part numbers, before keying: where numbers are still apart.</summary>
    internal string Keyed { get; init; } = Core;
}

/// <summary>An artist credit read into its parts.</summary>
/// <param name="Raw">The credit as given.</param>
/// <param name="Display">The credit with channel suffixes, native-script aliases and bracketed
/// guests taken off.</param>
/// <param name="Names">The artists in it, split on every separator but never inside a known
/// name ("Tyler, The Creator") or before "the" ("Bob Marley &amp; The Wailers"). The first is the
/// primary artist.</param>
/// <param name="Featured">Guests credited in brackets, "Drake (feat. Rihanna)".</param>
/// <param name="Pieces">Parts of a name kept whole only by the "the" rule, so "Bob Marley" alone
/// still matches "Bob Marley &amp; The Wailers".</param>
public sealed record SongArtists(
    string Raw, string Display, IReadOnlyList<string> Names, IReadOnlyList<string> Featured,
    IReadOnlyList<string> Pieces)
{
    public string Primary => Names.Count > 0 ? Names[0] : Display;
    public bool IsEmpty => SongIdentity.Key(Display).Length == 0 && Names.Count == 0;
}

/// <summary>How two artist credits relate.</summary>
public enum ArtistAgreement
{
    /// <summary>Either side is empty.</summary>
    Unknown,
    /// <summary>No artist in common.</summary>
    None,
    /// <summary>They share an artist, but each names a guest the other does not.</summary>
    Conflict,
    /// <summary>They share an artist only once stylized characters are read as letters.</summary>
    Loose,
    /// <summary>They share an artist.</summary>
    Agree,
}

/// <summary>
/// One reading of song titles and artist credits for every place Octo decides whether two
/// songs are the same: lyrics, AcoustID and MusicBrainz, Soulseek candidates, the library and
/// radio matchers, search merging, and the Deezer and Last.fm lookups. Each of those used to
/// carry its own normalizer, and they disagreed, so a song one of them recognised another did
/// not.
///
/// The rules, in the order a title is read:
///
/// 1. Fold: Unicode NFKC (fullwidth "＄" and "﹩" become "$", "（" becomes "("), curly quotes and
///    dashes made plain, underscores made spaces, Cyrillic and Greek lookalikes inside a Latin
///    word read as Latin.
/// 2. A track number in front ("01 - ", "01. ", "1-01 ") is dropped, and so is a leading
///    "Artist - " when it names the artist (or when no artist was given at all).
/// 3. Each bracket is read as a guest ("feat. X"), upload noise ("Official Video"), a part
///    number ("Pt. 2", kept in the key), one or more version markers, or a subtitle.
/// 4. A " - " tail that is a version or noise ("- Remastered 2011", "- Live at Wembley") is
///    read the same way, and so is a trailing "feat. X" and a trailing "Remix" or "Sped Up".
/// 5. The key is the rest, casefolded, accents stripped, and only letters, digits and the marks
///    some scripts need kept. A title of only symbols or emoji keeps its symbols instead.
///
/// Artist credits split on , &amp; ; / 、 x × and with feat ft featuring vs, and the whole credit
/// is kept as a candidate beside its parts, so "Simon &amp; Garfunkel" and "Earth, Wind &amp; Fire"
/// match themselves however they are split. A small alias table covers renamed artists ("Ye"
/// for Kanye West), and a bracketed alias in another script ("Ye (侃爷)") is ignored.
///
/// An ISRC on both sides settles it before any of that: one ISRC is one recording, whatever
/// script or language its title is written in. Two different ISRCs settle nothing, because a
/// re-release or a remaster is often given a new code for the same audio.
///
/// The same rules, and a shared list of cases, live in the Octo app; docs/song-identity-cases.json
/// is the contract both run.
/// </summary>
public static class SongIdentity
{
    /// <summary>How far apart two lengths may be and still be one recording.</summary>
    public const int LengthToleranceSeconds = 3;

    /// <summary>Version markers that never make a different recording.</summary>
    public static readonly IReadOnlySet<string> NeutralVersions = new HashSet<string>(StringComparer.Ordinal)
    {
        "remaster", "explicit", "original", "album version", "single version", "mono", "stereo",
    };

    // ---- folding ------------------------------------------------------------------------

    /// <summary>
    /// The text with its lookalikes made plain, case and accents untouched: NFKC, curly quotes
    /// and dashes folded, underscores as spaces, whitespace collapsed. What every other reading
    /// starts from, and what a query is sent as.
    /// </summary>
    public static string Fold(string? value)
    {
        if (string.IsNullOrWhiteSpace(value)) return "";
        var text = value.Replace('\u00B4', '\'').Normalize(NormalizationForm.FormKC);
        var sb = new StringBuilder(text.Length);
        foreach (var ch in text)
        {
            sb.Append(ch switch
            {
                '\u2018' or '\u2019' or '\u201A' or '\u201B' or '\u2032' or '`' => '\'',
                '\u201C' or '\u201D' or '\u201E' or '\u201F' or '\u2033' => '"',
                '\u2010' or '\u2011' or '\u2012' or '\u2013' or '\u2014' or '\u2015' or '\u2212' => '-',
                '\u3010' or '\u3016' => '[',
                '\u3011' or '\u3017' => ']',
                '\u3014' => '(',
                '\u3015' => ')',
                '_' => ' ',
                _ when char.IsWhiteSpace(ch) => ' ',
                _ => ch,
            });
        }
        return Whitespace.Replace(FoldMixedScriptWords(sb.ToString()), " ").Trim();
    }

    /// <summary>The exact key: casefolded, accents stripped, "&amp;" read as "and", and only
    /// letters, digits and combining marks kept. Symbols only when there is nothing else.</summary>
    public static string Key(string? value)
    {
        var lower = LowerFold(Fold(value));
        if (lower.Length == 0) return "";
        var sb = new StringBuilder(lower.Length);
        foreach (var rune in lower.EnumerateRunes())
        {
            var category = Rune.GetUnicodeCategory(rune);
            if (Rune.IsLetterOrDigit(rune)
                || category is UnicodeCategory.NonSpacingMark or UnicodeCategory.SpacingCombiningMark)
                sb.Append(rune.ToString());
        }
        if (sb.Length > 0) return sb.ToString();
        // "!!!" or an emoji: nothing is a letter, so the symbols are the name.
        foreach (var rune in lower.EnumerateRunes())
            if (!Rune.IsWhiteSpace(rune)) sb.Append(rune.ToString());
        return sb.ToString();
    }

    /// <summary>The key with stylized characters read as letters, so "$uicideboy$" and
    /// "Suicideboys" agree. Only ever an additional key.</summary>
    public static string LooseKey(string? value) => Key(FoldStylized(Fold(value)));

    /// <summary>
    /// Stylized characters read as the letters they stand for: $ as s, @ as a, 0 as o and 3 as
    /// e when they sit against a letter, ! as i between two letters. "2003", "Blink-182" and
    /// "Help!" are left alone.
    /// </summary>
    public static string FoldStylized(string? value)
    {
        if (string.IsNullOrEmpty(value)) return "";
        var sb = new StringBuilder(value.Length);
        for (var i = 0; i < value.Length; i++)
        {
            var ch = value[i];
            var prev = i > 0 ? value[i - 1] : ' ';
            var next = i + 1 < value.Length ? value[i + 1] : ' ';
            var letterBeside = char.IsLetter(prev) || char.IsLetter(next);
            var digitBeside = char.IsDigit(prev) || char.IsDigit(next);
            var upper = char.IsLetter(next) ? char.IsUpper(next) : char.IsUpper(prev);
            char? read = ch switch
            {
                '$' when letterBeside => 's',
                '@' when letterBeside => 'a',
                '!' when char.IsLetter(prev) && char.IsLetter(next) => 'i',
                '0' when letterBeside && !digitBeside => 'o',
                '3' when letterBeside && !digitBeside => 'e',
                _ => null,
            };
            sb.Append(read is { } letter ? (upper ? char.ToUpperInvariant(letter) : letter) : ch);
        }
        return sb.ToString();
    }

    private static readonly Regex Whitespace = new(@"\s+", RegexOptions.Compiled);

    /// <summary>Cyrillic and Greek letters that look Latin, read as Latin inside a word that
    /// also has Latin letters. A word wholly in Cyrillic or Greek is left as it is.</summary>
    private static readonly Dictionary<char, char> Homoglyphs = new()
    {
        ['А'] = 'A', ['В'] = 'B', ['Е'] = 'E', ['К'] = 'K', ['М'] = 'M', ['Н'] = 'H', ['О'] = 'O',
        ['Р'] = 'P', ['С'] = 'C', ['Т'] = 'T', ['Х'] = 'X', ['І'] = 'I', ['Ј'] = 'J', ['Ѕ'] = 'S',
        ['а'] = 'a', ['е'] = 'e', ['о'] = 'o', ['р'] = 'p', ['с'] = 'c', ['у'] = 'y', ['х'] = 'x',
        ['і'] = 'i', ['ј'] = 'j', ['ѕ'] = 's',
        ['Α'] = 'A', ['Β'] = 'B', ['Ε'] = 'E', ['Ζ'] = 'Z', ['Η'] = 'H', ['Ι'] = 'I', ['Κ'] = 'K',
        ['Μ'] = 'M', ['Ν'] = 'N', ['Ο'] = 'O', ['Ρ'] = 'P', ['Τ'] = 'T', ['Υ'] = 'Y', ['Χ'] = 'X',
        ['ο'] = 'o',
    };

    private static readonly Regex Word = new(@"[\p{L}\p{M}]+", RegexOptions.Compiled);

    private static string FoldMixedScriptWords(string text) =>
        Word.Replace(text, match =>
        {
            var word = match.Value;
            if (!word.Any(ch => ch is >= 'A' and <= 'Z' or >= 'a' and <= 'z')) return word;
            if (!word.Any(Homoglyphs.ContainsKey)) return word;
            return new string(word.Select(ch => Homoglyphs.TryGetValue(ch, out var latin) ? latin : ch).ToArray());
        });

    /// <summary>The folded text lowercased and without accents, spacing and punctuation kept:
    /// for matching words inside something that is not a title, such as a file name.</summary>
    public static string Plain(string? value) => LowerFold(Fold(value));

    /// <summary>Lowercase, the letters that do not decompose spelled out, accents stripped,
    /// "&amp;" and a spaced "+" read as "and".</summary>
    private static string LowerFold(string folded)
    {
        if (folded.Length == 0) return "";
        var lower = folded.ToLowerInvariant()
            .Replace("ß", "ss").Replace("æ", "ae").Replace("œ", "oe").Replace("ø", "o")
            .Replace("đ", "d").Replace("ð", "d").Replace("ł", "l").Replace("þ", "th").Replace("ı", "i")
            .Replace("&", " and ").Replace(" + ", " and ");
        var decomposed = lower.Normalize(NormalizationForm.FormD);
        var sb = new StringBuilder(decomposed.Length);
        foreach (var ch in decomposed)
        {
            // Only the Latin, Greek and Cyrillic accents. A kana's voicing mark is a different
            // letter, and stripping it would make two Japanese titles one.
            if (ch is >= '\u0300' and <= '\u036F' or >= '\u1AB0' and <= '\u1AFF'
                or >= '\u1DC0' and <= '\u1DFF' or >= '\uFE20' and <= '\uFE2F') continue;
            sb.Append(ch);
        }
        return sb.ToString().Normalize(NormalizationForm.FormC);
    }

    // ---- titles -------------------------------------------------------------------------

    /// <summary>"01 - ", "01. ", "1-01 ", "2) " in front of a title, with a letter after it.
    /// "1-800-273-8255" and "99 Problems" keep their numbers.</summary>
    private static readonly Regex TrackNumber = new(
        @"^(?:\d{1,2}-\d{1,3}\s+|\d{1,3}\s*[.)]\s*|\d{1,3}\s+-\s+)(?=[^\d\s.])", RegexOptions.Compiled);

    private static readonly Regex Bracket = new(@"\s*[\(\[\{]([^\(\)\[\]\{\}]*)[\)\]\}]", RegexOptions.Compiled);

    /// <summary>A bracket a truncated title never closed, "Song (feat. X".</summary>
    private static readonly Regex OpenBracket = new(@"\s+[\(\[]([^\(\)\[\]]*)$", RegexOptions.Compiled);

    private static readonly Regex DashTail = new(@"\s+-\s+", RegexOptions.Compiled);

    private static readonly Regex TrailingFeature = new(
        @"\s+(?:feat\.?|ft\.?|featuring)\s+(.+)$", RegexOptions.IgnoreCase | RegexOptions.Compiled);

    /// <summary>Version words that mean a version even without brackets at the end of a title,
    /// "Mask Off Remix", "Heat Waves Sped Up". Not "live" or "edit": too many titles end in them.</summary>
    private static readonly Regex TrailingVersion = new(
        @"\s+(re-?mix|rmx|sped\s*up|speed\s*up|slowed(?:\s*(?:\+|&|and|n)\s*reverb(?:ed)?)?|slowed\s+down|nightcore|instrumental|acapella|a\s*cappella|karaoke(?:\s+version)?|drumless|8d\s+audio)$",
        RegexOptions.IgnoreCase | RegexOptions.Compiled);

    private static readonly Regex FeatureLead = new(
        @"^(?:feat\.\s*|ft\.\s*|w/\s*|(?:feat|ft|featuring|with)\s+)(.+)$", RegexOptions.IgnoreCase | RegexOptions.Compiled);

    private static readonly Regex PartNumber = new(
        @"^(?:(?:pt|part|vol|volume|chapter|ch|no|book)\.?\s*)?(?:\d{1,3}|[ivx]{1,4})$",
        RegexOptions.IgnoreCase | RegexOptions.Compiled);

    private static readonly Regex Year = new(@"^(?:19|20)\d{2}$", RegexOptions.Compiled);

    /// <summary>A part number, "Pt. 2", "Part II", "Vol. 3", spelled one way so "Part II" and
    /// "Pt. 2" agree.</summary>
    private static readonly Regex PartWord = new(
        @"\b(?:(pt|part)|(vol|volume)|(chapter|ch)|(book))\.?\s*(\d{1,3}|[ivx]{1,4})\b",
        RegexOptions.IgnoreCase | RegexOptions.Compiled);

    private static readonly string[] Romans = ["i", "ii", "iii", "iv", "v", "vi", "vii", "viii", "ix", "x"];

    private static string PartOf(Match match)
    {
        var word = match.Groups[1].Success ? "pt" : match.Groups[2].Success ? "vol" : match.Groups[3].Success ? "ch" : "book";
        var number = match.Groups[5].Value.ToLowerInvariant();
        var roman = Array.IndexOf(Romans, number);
        return $"{word} {(roman >= 0 ? (roman + 1).ToString(CultureInfo.InvariantCulture) : number)}";
    }

    /// <summary>Words that only ever describe how something was uploaded, never which recording
    /// it is. A bracket made only of these is dropped.</summary>
    private static readonly HashSet<string> UploadNoise = new(StringComparer.OrdinalIgnoreCase)
    {
        "official", "music", "video", "audio", "lyric", "lyrics", "visualizer", "visualiser",
        "hd", "hq", "uhd", "4k", "8k", "1080p", "720p", "480p", "mv", "m/v", "clip", "videoclip",
        "with", "full", "song", "only", "new", "premiere", "animated",
    };

    private static readonly Regex NeutralPhrase = new(
        @"^(?:from|taken from|as heard (?:in|on)|as featured in|as seen (?:in|on)|theme from|music from)\b"
        + @"|\b(?:soundtrack|ost|motion picture|original score)\b"
        + @"|^bonus(?:\s+tracks?)?(?:\s+(?:edition|version))?$|^(?:prod|produced)\b"
        // A release edition is the same recordings packaged again; "Drumless Edition" is a marker below.
        + @"|^(?:(?:super\s+)?deluxe|expanded|(?:\d+(?:st|nd|rd|th)\s+)?anniversary|special|collector'?s|limited|tour|platinum)(?:\s+(?:edition|version))?$"
        + @"|^(?:copyright free|free download|out now|audio only|single|ep)$",
        RegexOptions.IgnoreCase | RegexOptions.Compiled);

    private sealed record Marker(Regex Pattern, string Name, bool Credited = false, bool Generic = false);

    /// <summary>Version markers, most specific first. Each match is taken out of the text
    /// before the next is looked for, so "Extended Mix" is extended and not also a mix.</summary>
    private static readonly Marker[] Markers =
    [
        new(Rx(@"\bradio\s+(?:edit|version|mix|cut)\b"), "radio edit"),
        new(Rx(@"\bextended(?:\s+(?:mix|version|edit|cut))?\b"), "extended"),
        new(Rx(@"\boriginal\s+(?:mix|version)\b"), "original"),
        new(Rx(@"\b(?:album|lp)\s+version\b"), "album version"),
        new(Rx(@"\bsingle\s+version\b"), "single version"),
        new(Rx(@"\b(?:\d{4}\s+)?(?:digital(?:ly)?\s+)?re-?master(?:ed)?(?:\s+\d{4})?(?:\s+(?:version|edition))?\b"), "remaster"),
        new(Rx(@"\blive\b"), "live"),
        new(Rx(@"\bunplugged\b"), "unplugged"),
        new(Rx(@"\bacoustic(?:\s+version)?\b"), "acoustic"),
        new(Rx(@"\binstrumental(?:\s+version)?\b"), "instrumental"),
        new(Rx(@"\b(?:a\s*cappella|acapella)\b"), "acapella"),
        new(Rx(@"\bdemo(?:\s+version)?\b"), "demo"),
        new(Rx(@"\b(?:sped\s*up|speed\s*up)\b"), "sped up"),
        new(Rx(@"\bslowed(?:\s+down)?\b"), "slowed"),
        new(Rx(@"\breverb(?:ed)?\b"), "reverb"),
        new(Rx(@"\bnightcore\b"), "nightcore"),
        // Editions that change what is played, unlike a deluxe or anniversary one (NeutralPhrase).
        new(Rx(@"\bdrumless\b"), "drumless"),
        new(Rx(@"\b8d(?:\s+audio)?\b"), "8d"),
        new(Rx(@"\bpiano(?:\s+(?:version|edition|arrangement))?\b"), "piano"),
        new(Rx(@"\b(?:orchestral|symphonic)(?:\s+(?:version|edition|mix))?\b"), "orchestral"),
        new(Rx(@"\blo-?fi(?:\s+(?:version|edit|mix))?\b"), "lofi"),
        new(Rx(@"\bbass\s*boost(?:ed)?\b"), "bass boosted"),
        new(Rx(@"^(.*?)\s*\b(?:re-?mix(?:ed)?|rmx)\b"), "remix", Credited: true),
        new(Rx(@"\bvip(?:\s+mix)?\b"), "vip"),
        new(Rx(@"\bbootleg\b"), "bootleg"),
        new(Rx(@"\brework(?:ed)?\b"), "rework"),
        new(Rx(@"^(.*?)\s*\bdub(?:\s+(?:mix|version))?\b"), "dub", Credited: true),
        new(Rx(@"\b(?:clean|censored)(?:\s+(?:version|edit))?\b"), "clean"),
        new(Rx(@"\b(?:explicit|dirty)(?:\s+version)?\b"), "explicit"),
        new(Rx(@"\bmono(?:\s+(?:version|mix))?\b"), "mono"),
        new(Rx(@"\bstereo(?:\s+(?:version|mix))?\b"), "stereo"),
        new(Rx(@"\bkaraoke(?:\s+version)?\b|\boriginally performed by\b|\bin the style of\b|\bmade (?:popular|famous) by\b|\bbacking (?:version|track)\b"), "karaoke"),
        new(Rx(@"\bcover(?:\s+version)?\b"), "cover"),
        new(Rx(@"\breprise\b"), "reprise"),
        new(Rx(@"\bsessions?\b"), "session"),
        new(Rx(@"^(.*?)\s*\bmix\b"), "mix", Credited: true, Generic: true),
        new(Rx(@"^(.*?)\s*\bedit\b"), "edit", Credited: true, Generic: true),
        new(Rx(@"\bversion\b"), "version", Generic: true),
    ];

    private static Regex Rx(string pattern) =>
        new(pattern, RegexOptions.IgnoreCase | RegexOptions.CultureInvariant | RegexOptions.Compiled);

    /// <summary>Words in front of a mix that name its kind, not who made it.</summary>
    private static readonly HashSet<string> NotACredit = new(StringComparer.Ordinal)
    {
        "", "the", "official", "club", "dance", "house", "main", "short", "long", "full", "new", "a",
    };

    private enum Kind { Noise, Feature, Part, Version, Extra }

    private sealed record Reading(Kind Kind, List<string> Versions, List<string> Credits, List<string> Names);

    /// <summary>What one bracket or " - " tail says.</summary>
    private static Reading Read(string inner)
    {
        var text = inner.Trim().Trim('-', ':', ',', ' ');
        var versions = new List<string>();
        var credits = new List<string>();
        if (text.Length == 0) return new(Kind.Noise, versions, credits, []);

        var words = text.Split([' ', '-'], StringSplitOptions.RemoveEmptyEntries);
        if (words.All(UploadNoise.Contains)) return new(Kind.Noise, versions, credits, []);

        var feature = FeatureLead.Match(text);
        if (feature.Success)
            return new(Kind.Feature, versions, credits, SplitNames(feature.Groups[1].Value).Names);

        if (PartNumber.IsMatch(text)) return new(Kind.Part, versions, credits, []);
        if (NeutralPhrase.IsMatch(text) || Year.IsMatch(text)) return new(Kind.Noise, versions, credits, []);

        var rest = text;
        foreach (var marker in Markers)
        {
            if (marker.Generic && versions.Count > 0) continue;
            var match = marker.Pattern.Match(rest);
            if (!match.Success) continue;
            if (!versions.Contains(marker.Name)) versions.Add(marker.Name);
            if (marker.Credited && match.Groups.Count > 1)
            {
                var credit = Key(match.Groups[1].Value);
                if (!NotACredit.Contains(credit)) credits.Add(credit);
            }
            rest = rest.Remove(match.Index, match.Length).Insert(match.Index, " ");
        }
        return versions.Count > 0
            ? new(Kind.Version, versions, credits, [])
            : new(Kind.Extra, versions, credits, []);
    }

    /// <summary>A title read into its core, its key, its version markers and its guests. The
    /// artist, when given, lets a leading "Artist - " be recognised; an artist given as empty
    /// means the song has none, and then any "X - Y" title is read as artist X and title Y. Null
    /// reads the title alone.</summary>
    public static SongTitle ParseTitle(string? title, string? artist = null)
    {
        var raw = title ?? "";
        var text = Fold(raw);
        var versions = new List<string>();
        var remixers = new List<string>();
        var featured = new List<string>();
        var extras = new List<string>();
        var parts = new List<string>();
        string? artistFromTitle = null;

        void Take(Reading reading, string original)
        {
            switch (reading.Kind)
            {
                case Kind.Feature: featured.AddRange(reading.Names); break;
                case Kind.Part: parts.Add(original.Trim()); break;
                case Kind.Version:
                    foreach (var version in reading.Versions) if (!versions.Contains(version)) versions.Add(version);
                    remixers.AddRange(reading.Credits);
                    break;
                case Kind.Extra: extras.Add(Key(original)); break;
            }
        }

        var numbered = TrackNumber.Match(text);
        if (numbered.Success && text[numbered.Length..].Any(char.IsLetter)) text = text[numbered.Length..];

        // "Artist - Title": the artist named in front, or any name at all when no artist was given.
        var dash = DashTail.Match(text);
        if (dash.Success && dash.Index > 0)
        {
            var left = text[..dash.Index];
            var right = text[(dash.Index + dash.Length)..];
            var tail = Read(right);
            var namesArtist = !string.IsNullOrWhiteSpace(artist) && NamesArtist(left, artist);
            var noArtist = artist is not null && string.IsNullOrWhiteSpace(artist)
                && tail.Kind is Kind.Extra && Key(right).Length > 0;
            if ((namesArtist && Key(right).Length > 0) || noArtist)
            {
                artistFromTitle = left.Trim();
                text = right;
            }
        }

        for (var guard = 0; guard < 12; guard++)
        {
            var match = Bracket.Match(text);
            if (!match.Success) break;
            var inner = match.Groups[1].Value;
            var rest = text.Remove(match.Index, match.Length).Insert(match.Index, " ").Trim();
            // A title that is nothing but a bracket, "(Exchange)", is the words inside it.
            if (Key(rest).Length == 0 && Read(inner).Kind is Kind.Extra or Kind.Noise)
            {
                text = inner;
                break;
            }
            Take(Read(inner), inner);
            text = rest;
        }
        var open = OpenBracket.Match(text);
        if (open.Success)
        {
            Take(Read(open.Groups[1].Value), open.Groups[1].Value);
            text = text[..open.Index];
        }

        // " - Live at Wembley", " - Remastered 2011": only a tail that says what kind of
        // recording this is. "Pt. 2 - The Return" keeps its tail.
        for (var guard = 0; guard < 4; guard++)
        {
            var tails = DashTail.Matches(text);
            if (tails.Count == 0) break;
            var last = tails[^1];
            if (last.Index == 0) break;
            var reading = Read(text[(last.Index + last.Length)..]);
            if (reading.Kind is not (Kind.Version or Kind.Noise or Kind.Feature)) break;
            Take(reading, text[(last.Index + last.Length)..]);
            text = text[..last.Index];
        }

        var trailingFeature = TrailingFeature.Match(text);
        if (trailingFeature.Success && trailingFeature.Index > 0)
        {
            featured.AddRange(SplitNames(trailingFeature.Groups[1].Value).Names);
            text = text[..trailingFeature.Index];
        }

        for (var guard = 0; guard < 3; guard++)
        {
            var trailing = TrailingVersion.Match(text);
            if (!trailing.Success || trailing.Index == 0) break;
            Take(Read(trailing.Groups[1].Value), trailing.Groups[1].Value);
            text = text[..trailing.Index];
        }

        var core = Whitespace.Replace(text, " ").Trim().TrimEnd('-', ':', ',', '/', ' ').Trim();
        if (Key(core).Length == 0)
        {
            // Nothing is left but brackets or symbols: the whole title is the name.
            core = Whitespace.Replace(Fold(raw).Replace("(", " ").Replace(")", " ").Replace("[", " ").Replace("]", " "), " ").Trim();
            extras.Clear();
        }
        var keyed = PartWord.Replace(parts.Count > 0 ? $"{core} {string.Join(' ', parts)}" : core, PartOf);

        versions.Sort(StringComparer.Ordinal);
        return new SongTitle(raw, core, Key(keyed), LooseKey(keyed), versions,
            remixers.Where(remixer => remixer.Length > 0).Distinct().ToList(),
            featured.Where(name => Key(name).Length > 0).Distinct().ToList(),
            extras.Where(extra => extra.Length > 0).Distinct().ToList(),
            artistFromTitle) { Keyed = keyed };
    }

    /// <summary>The markers of a title that make it a different recording, for this caller.</summary>
    public static IReadOnlySet<string> DistinctVersions(SongTitle title, SongMatchOptions? options = null) =>
        title.Versions
            .Where(version => !NeutralVersions.Contains(version) && !(options?.AlsoNeutral.Contains(version) ?? false))
            .ToHashSet(StringComparer.Ordinal);

    /// <summary>Version markers the candidate carries that the request does not ("Song (Live)"
    /// for "Song"), empty when it adds none. One-directional, for a request whose title may be
    /// more specific than its match's.</summary>
    public static IReadOnlySet<string> AddedVersions(string? requested, string? candidate, SongMatchOptions? options = null)
    {
        var want = DistinctVersions(ParseTitle(requested), options);
        return DistinctVersions(ParseTitle(candidate), options).Where(version => !want.Contains(version))
            .ToHashSet(StringComparer.Ordinal);
    }

    /// <summary>The title without its guest credits, as a person would write it.</summary>
    public static string StripFeatures(string? title)
    {
        var text = (title ?? "").Trim();
        text = Bracket.Replace(text, match =>
        {
            var inner = match.Groups[1].Value;
            if (Read(Fold(inner)).Kind == Kind.Feature) return "";
            // "(Radio Edit - feat. Pharrell Williams)": the credit goes, the version stays.
            var credit = InnerFeature.Match(inner);
            if (!credit.Success || credit.Index == 0) return match.Value;
            var kept = inner[..credit.Index].Trim().TrimEnd('-', '–', ',', ';', '/', ' ');
            return kept.Length == 0 ? "" : match.Value.Replace(inner, kept);
        });
        var trailing = TrailingFeature.Match(text);
        if (trailing.Success && trailing.Index > 0)
        {
            // "Song feat. X (Radio Edit)": the credit goes, a bracket after it stays.
            var rest = text[trailing.Index..];
            var bracket = rest.IndexOfAny(['(', '[']);
            text = text[..trailing.Index] + (bracket > 0 ? " " + rest[bracket..] : "");
        }
        // "Song - Radio Edit - feat. X" leaves the dash that led to the credit.
        text = text.TrimEnd().TrimEnd('-', '–', ',', ' ');
        return Whitespace.Replace(text, " ").Trim();
    }

    /// <summary>A guest credit run on after a version inside one bracket, "Radio Edit - feat. X".</summary>
    private static readonly Regex InnerFeature = new(
        @"\s*(?:[-–,;/]\s*)?\b(?:feat\.?|ft\.?|featuring)\s+.+$", RegexOptions.IgnoreCase | RegexOptions.Compiled);

    // ---- artists ------------------------------------------------------------------------

    /// <summary>Names that contain a separator and are still one artist. Keys.</summary>
    private static readonly HashSet<string> KnownNames = new(StringComparer.Ordinal)
    {
        "simonandgarfunkel", "earthwindandfire", "tylerthecreator", "acdc", "crosbystillsandnash",
        "crosbystillsnashandyoung", "emersonlakeandpalmer", "bloodsweatandtears", "peterpaulandmary",
        "hallandoates", "darylhallandjohnoates", "mumfordandsons", "ofmonstersandmen", "belleandsebastian",
        "chaseandstatus", "nicoandvinz", "macklemoreandryanlewis", "samanddave", "peachesandherb",
        "brooksanddunn", "bigandrich", "danandshay", "mattandkim", "sheandhim", "ironandwine",
        "angusandjuliastone", "yearsandyears", "coheedandcambria", "chloexhalle", "aura", "axwellingrosso",
        "aboveandbeyond", "alyandfila", "gabrielanddresden", "dimitrivegasandlikemike", "sonnyandcher",
        "captainandtennille", "ikeandtinaturner", "teganandsara", "ashfordandsimpson",
        "shovelsandrope", "florenceandthemachine",
    };

    /// <summary>Other names an artist goes by, as keys, to one canonical key. Small on purpose:
    /// renames and stage names that sources really do disagree on.</summary>
    private static readonly Dictionary<string, string> Aliases = new(StringComparer.Ordinal)
    {
        ["ye"] = "kanyewest", ["kanye"] = "kanyewest",
        ["2pac"] = "2pac", ["tupac"] = "2pac", ["tupacshakur"] = "2pac", ["makaveli"] = "2pac",
        ["diddy"] = "diddy", ["pdiddy"] = "diddy", ["puffdaddy"] = "diddy", ["seancombs"] = "diddy",
        ["snooplion"] = "snoopdogg", ["snoopdoggydogg"] = "snoopdogg",
        ["yasiinbey"] = "mosdef",
        ["biggie"] = "notoriousbig", ["biggiesmalls"] = "notoriousbig", ["thenotoriousbig"] = "notoriousbig",
        ["donaldglover"] = "childishgambino",
        ["princeandthenewpowergeneration"] = "prince", ["theartistformerlyknownasprince"] = "prince",
    };

    /// <summary>The name each artist in <see cref="Aliases"/> is best known by, written the way
    /// the catalogs write it, keyed by the canonical key the aliases lead to.</summary>
    private static readonly Dictionary<string, string> AliasNames = new(StringComparer.Ordinal)
    {
        ["kanyewest"] = "Kanye West",
        ["2pac"] = "2Pac",
        ["diddy"] = "Diddy",
        ["snoopdogg"] = "Snoop Dogg",
        ["mosdef"] = "Mos Def",
        ["notoriousbig"] = "The Notorious B.I.G.",
        ["childishgambino"] = "Childish Gambino",
        ["prince"] = "Prince",
    };

    private static readonly Regex ArtistSeparator = new(
        @"\s*(?:,|;|/|、|×|&|\s\+\s|\s[•·]\s|\sx\s(?!(?:feat|ft|featuring|with|and|x)\b|[&,;/])|\s(?:and|with|feat\.?|ft\.?|featuring|vs\.?|pres\.|presents)\s)\s*",
        RegexOptions.IgnoreCase | RegexOptions.CultureInvariant | RegexOptions.Compiled);

    private static readonly Regex ChannelSuffix = new(@"(?:\s+-\s+topic|(?<=\p{Ll})vevo|\s+vevo)$",
        RegexOptions.IgnoreCase | RegexOptions.Compiled);

    private static readonly Regex HasLatin = new(@"[A-Za-z]", RegexOptions.Compiled);

    private static bool IsJoiner(string separator)
    {
        var s = separator.Trim().ToLowerInvariant();
        return s is "&" or "and" or "+";
    }

    /// <summary>A credit split into its artists, keeping known names and "X &amp; the Y" whole.</summary>
    private static (List<string> Names, List<string> Pieces) SplitNames(string credit)
    {
        var text = credit.Trim();
        if (text.Length == 0) return ([], []);
        if (KnownNames.Contains(Key(text))) return ([text], []);

        var parts = new List<(int Start, int End, string Before)>();
        var at = 0;
        var before = "";
        foreach (Match separator in ArtistSeparator.Matches(text))
        {
            if (separator.Index > at) parts.Add((at, separator.Index, before));
            before = separator.Value;
            at = separator.Index + separator.Length;
        }
        if (at < text.Length) parts.Add((at, text.Length, before));
        if (parts.Count == 0) return ([text], []);

        var names = new List<string>();
        var starts = new List<int>();
        var pieces = new List<string>();
        for (var i = 0; i < parts.Count; i++)
        {
            // The longest run of parts that is a known name, "Tyler, The Creator".
            var end = i;
            for (var j = parts.Count - 1; j > i; j--)
                if (KnownNames.Contains(Key(text[parts[i].Start..parts[j].End]))) { end = j; break; }
            var name = text[parts[i].Start..parts[end].End].Trim();

            // "Bob Marley & The Wailers": a "the" after "&" or "and" belongs to the name before.
            if (end == i && names.Count > 0 && IsJoiner(parts[i].Before)
                && name.StartsWith("the ", StringComparison.OrdinalIgnoreCase))
            {
                pieces.Add(names[^1]);
                pieces.Add(name);
                names[^1] = text[starts[^1]..parts[i].End].Trim();
                continue;
            }
            names.Add(name);
            starts.Add(parts[i].Start);
            i = end;
        }
        return (names.Where(name => Key(name).Length > 0).ToList(), pieces.Where(piece => Key(piece).Length > 0).ToList());
    }

    /// <summary>An artist credit read into its artists.</summary>
    public static SongArtists ParseArtists(string? artist)
    {
        var raw = artist ?? "";
        var text = ChannelSuffix.Replace(Fold(raw), "").Trim();
        var featured = new List<string>();
        // Every bracket goes: "(feat. X)" is a guest, "Ye (侃爷)" the same artist in another
        // script, and "Nirvana (US)" a disambiguation.
        text = Bracket.Replace(text, match =>
        {
            var feature = FeatureLead.Match(match.Groups[1].Value.Trim());
            if (feature.Success) featured.AddRange(SplitNames(feature.Groups[1].Value).Names);
            return " ";
        });
        text = Whitespace.Replace(text, " ").Trim();
        if (Key(text).Length == 0) text = Whitespace.Replace(Fold(raw), " ").Trim();

        // "Drake feat. Rihanna" names its guest after the separator; the names list keeps it.
        var (names, pieces) = SplitNames(text);
        return new SongArtists(raw, text, names, featured, pieces);
    }

    /// <summary>The artist a credit names first, as written: "Beyoncé" for "Beyoncé feat. Jay-Z",
    /// "Tyler, The Creator" for itself.</summary>
    public static string PrimaryArtist(string? artist) => ParseArtists(artist).Primary;

    /// <summary>Every key a name can be matched by: itself, without a leading "the", and its
    /// alias.</summary>
    private static IEnumerable<string> KeysOf(string name, bool loose)
    {
        var key = loose ? LooseKey(name) : Key(name);
        if (key.Length == 0) yield break;
        yield return key;
        var trimmed = Fold(name);
        if (trimmed.StartsWith("the ", StringComparison.OrdinalIgnoreCase))
        {
            var bare = loose ? LooseKey(trimmed[4..]) : Key(trimmed[4..]);
            if (bare.Length > 0) yield return bare;
        }
        if (Aliases.TryGetValue(key, out var alias)) yield return alias;
    }

    private static HashSet<string> Keys(IEnumerable<string> names, bool loose) =>
        names.SelectMany(name => KeysOf(name, loose)).ToHashSet(StringComparer.Ordinal);

    private sealed record Credit(SongArtists Artists)
    {
        public IEnumerable<string> All =>
            Artists.Names.Concat(Artists.Featured).Concat(Artists.Pieces).Append(Artists.Display);
        public IEnumerable<string> Primary => [Artists.Primary, Artists.Display];
        public IEnumerable<string> Named => Artists.Names.Concat(Artists.Featured);
    }

    /// <summary>How two credits relate. <paramref name="bCredits"/>, when a source lists its
    /// artists one by one, is used instead of splitting <paramref name="b"/>.</summary>
    public static ArtistAgreement CompareArtists(string? a, string? b, IEnumerable<string>? bCredits = null) =>
        CompareArtists(ParseArtists(a), WithCredits(ParseArtists(b), bCredits));

    private static SongArtists WithCredits(SongArtists artists, IEnumerable<string>? credits)
    {
        var listed = credits?.Select(credit => credit.Trim()).Where(credit => Key(credit).Length > 0).ToList();
        if (listed is not { Count: > 0 }) return artists;
        var names = listed.Select(credit => ParseArtists(credit).Display).ToList();
        return artists with
        {
            Display = artists.IsEmpty ? string.Join(" & ", names) : artists.Display,
            Names = names.Concat(artists.Names.Where(name => !names.Any(listedName => Key(listedName) == Key(name)))).ToList(),
        };
    }

    public static ArtistAgreement CompareArtists(SongArtists a, SongArtists b)
    {
        if (a.IsEmpty || b.IsEmpty) return ArtistAgreement.Unknown;
        var left = new Credit(a);
        var right = new Credit(b);

        ArtistAgreement? found = null;
        foreach (var loose in new[] { false, true })
        {
            var allLeft = Keys(left.All, loose);
            var allRight = Keys(right.All, loose);
            if (Keys(left.Primary, loose).Overlaps(allRight) || Keys(right.Primary, loose).Overlaps(allLeft))
            {
                found = loose ? ArtistAgreement.Loose : ArtistAgreement.Agree;
                break;
            }
        }
        if (found is null) return ArtistAgreement.None;

        // "Bizarrap, Duki" against "Bizarrap & Rauw Alejandro": one artist in common, and each
        // names a guest the other does not. A name in another script is not counted, since it
        // may be one of the Latin names written its own way. Nor is a name that holds, or is
        // held in, one on the other side: "feat 2 Chainz B.O.B" never had a separator to split on.
        var leftKeys = left.Named.Where(name => HasLatin.IsMatch(name)).Select(name => Keys([name], true)).ToList();
        var rightKeys = right.Named.Where(name => HasLatin.IsMatch(name)).Select(name => Keys([name], true)).ToList();
        var onlyLeft = leftKeys.Any(keys => !Credited(keys, right));
        var onlyRight = rightKeys.Any(keys => !Credited(keys, left));
        return onlyLeft && onlyRight ? ArtistAgreement.Conflict : found.Value;
    }

    /// <summary>Whether a name is credited on the other side: one of its keys is one there, or
    /// holds or is held in one of the other side's names (never its whole credit, which holds
    /// every name).</summary>
    private static bool Credited(HashSet<string> keys, Credit other)
    {
        if (keys.Overlaps(Keys(other.All, true))) return true;
        var named = Keys(other.Named.Concat(other.Artists.Pieces), true);
        return keys.Any(key => named.Any(name => Math.Min(key.Length, name.Length) >= 3
            && (key.Contains(name, StringComparison.Ordinal) || name.Contains(key, StringComparison.Ordinal))));
    }

    /// <summary>The two credits share an artist and do not disagree about the guests.</summary>
    public static bool ArtistsAgree(string? a, string? b, IEnumerable<string>? bCredits = null) =>
        CompareArtists(a, b, bCredits) is ArtistAgreement.Agree or ArtistAgreement.Loose;

    /// <summary>Whether a name is the artist, in any of the forms a credit is matched by.</summary>
    private static bool NamesArtist(string name, string artist)
    {
        var candidate = ParseArtists(name);
        var credit = new Credit(ParseArtists(artist));
        return Keys([candidate.Display], false).Overlaps(Keys(credit.All, false))
            || Keys([candidate.Display], true).Overlaps(Keys(credit.All, true));
    }

    // ---- ISRCs --------------------------------------------------------------------------

    /// <summary>Two letters of country, three of registrant, two digits of year, five of designation.</summary>
    private static readonly Regex IsrcShape = new(@"^[A-Z]{2}[A-Z0-9]{3}[0-9]{7}$", RegexOptions.Compiled);

    /// <summary>
    /// An ISRC in its one canonical spelling, or null when the value is not one. Sources write
    /// them "USRC17607839", "US-RC1-76-07839", "us rc1 76 07839" and with dots; all of those are
    /// one code. Anything that is still not twelve characters of the right shape once the
    /// separators are gone is treated as absent, never as a code that matches nothing.
    /// </summary>
    public static string? NormalizeIsrc(string? value)
    {
        if (string.IsNullOrWhiteSpace(value)) return null;
        var text = value.Normalize(NormalizationForm.FormKC);
        var sb = new StringBuilder(12);
        foreach (var ch in text)
        {
            if (ch is '-' or '.' || char.IsWhiteSpace(ch)) continue;
            sb.Append(char.ToUpperInvariant(ch));
        }
        var isrc = sb.ToString();
        return IsrcShape.IsMatch(isrc) ? isrc : null;
    }

    /// <summary>Every valid ISRC among the values, normalised.</summary>
    public static IReadOnlySet<string> Isrcs(IEnumerable<string?>? values) =>
        (values ?? []).Select(NormalizeIsrc).OfType<string>().ToHashSet(StringComparer.Ordinal);

    /// <summary>Both sides carry a valid ISRC and at least one is on both.</summary>
    public static bool SharesIsrc(IEnumerable<string?>? a, IEnumerable<string?>? b)
    {
        var left = Isrcs(a);
        return left.Count > 0 && Isrcs(b).Overlaps(left);
    }

    // ---- comparing ----------------------------------------------------------------------

    private static readonly Regex Digits = new(@"\d+", RegexOptions.Compiled);

    private enum TitleAgreement { None, Exact, Loose, Numbers }

    private static TitleAgreement CompareKeys(SongTitle a, SongTitle b)
    {
        if (a.Key.Length == 0 || b.Key.Length == 0) return TitleAgreement.None;
        if (a.Key == b.Key) return TitleAgreement.Exact;
        if (a.LooseKey == b.LooseKey) return TitleAgreement.Loose;

        // Numbers compare as sets: "Vol. 53" is "Vol. 53/66", but "Shotta Flow" is never
        // "Shotta Flow 4".
        var lettersA = new string(a.Key.Where(char.IsLetter).ToArray());
        var lettersB = new string(b.Key.Where(char.IsLetter).ToArray());
        if (lettersA.Length == 0 || lettersA != lettersB) return TitleAgreement.None;
        var numbersA = Digits.Matches(a.Keyed).Select(match => match.Value).ToHashSet();
        var numbersB = Digits.Matches(b.Keyed).Select(match => match.Value).ToHashSet();
        if (numbersA.Count == 0 || numbersB.Count == 0) return TitleAgreement.None;
        return numbersA.IsSubsetOf(numbersB) || numbersB.IsSubsetOf(numbersA) ? TitleAgreement.Numbers : TitleAgreement.None;
    }

    private static readonly HashSet<string> CreditedVersions = new(StringComparer.Ordinal) { "remix", "mix", "dub", "edit" };

    /// <summary>
    /// For telling duplicates apart (#53) and choosing the one MusicBrainz recording a kept
    /// fingerprint belongs to (#47): a subtitle only one title has ("Song (Interlude)") is
    /// another title, because a false yes there deletes a file or files it under the wrong
    /// recording.
    /// </summary>
    public static readonly SongMatchOptions StrictTitles = new() { ExtrasMustAgree = true, LengthToleranceSeconds = null };

    /// <summary>Whether two titles are one song, and one version of it. Artists are not looked at.</summary>
    public static SongMatch SameTitle(string? a, string? b, SongMatchOptions? options = null) =>
        CompareTitles(ParseTitle(a), ParseTitle(b), options ?? SongMatchOptions.Default);

    private static SongMatch CompareTitles(SongTitle a, SongTitle b, SongMatchOptions options)
    {
        var agreement = CompareKeys(a, b);
        if (agreement == TitleAgreement.None)
            return new(SongVerdict.Different, 0.95, a.Key.Length == 0 || b.Key.Length == 0
                ? "a title is empty" : $"different titles ('{a.Core}' and '{b.Core}')");

        var confidence = 1.0;
        var reasons = new List<string>();
        if (agreement == TitleAgreement.Loose)
        {
            confidence -= 0.15;
            reasons.Add("the same title once stylized characters are read as letters");
        }
        if (agreement == TitleAgreement.Numbers)
        {
            confidence -= 0.1;
            reasons.Add("the same title with numbers that overlap");
        }

        var versionsA = DistinctVersions(a, options);
        var versionsB = DistinctVersions(b, options);
        if (!versionsA.SetEquals(versionsB))
            return new(SongVerdict.SameSongDifferentVersion, 0.9,
                $"a different version ({Describe(versionsA)} against {Describe(versionsB)})");

        if (versionsA.Overlaps(CreditedVersions))
        {
            if (a.Remixers.Count > 0 && b.Remixers.Count > 0 && !a.Remixers.Intersect(b.Remixers).Any())
                return new(SongVerdict.SameSongDifferentVersion, 0.85, "remixes by different people");
            if (a.Remixers.Count > 0 != b.Remixers.Count > 0)
            {
                confidence -= 0.05;
                reasons.Add("only one says who made the remix");
            }
        }

        if (a.Extras.Count > 0 && b.Extras.Count > 0 && !a.Extras.Intersect(b.Extras).Any())
            return new(SongVerdict.Different, 0.7, "different subtitles");
        if (a.Extras.Count > 0 != b.Extras.Count > 0)
        {
            if (options.ExtrasMustAgree) return new(SongVerdict.Different, 0.7, "only one title has a subtitle");
            confidence -= 0.1;
            reasons.Add("only one title has a subtitle");
        }

        return new(SongVerdict.Same, Math.Round(confidence, 2), reasons.Count == 0 ? "the same title" : string.Join("; ", reasons));
    }

    private static string Describe(IReadOnlySet<string> versions) =>
        versions.Count == 0 ? "the original" : string.Join(" + ", versions.Order(StringComparer.Ordinal));

    /// <summary>Whether two songs are the same recording. Both artists must be known: a title
    /// alone is not an identity, unless both sides share an ISRC, which is.</summary>
    public static SongMatch Same(SongRef a, SongRef b, SongMatchOptions? options = null)
    {
        // First, and above the text: a romanised title and the same title in its own script
        // share no letter, and one ISRC still says they are one recording. Different ISRCs fall
        // through to the text, since a re-release can carry a new code for the same audio.
        if (SharesIsrc(a.Isrcs, b.Isrcs)) return new(SongVerdict.Same, 1.0, "same ISRC");

        options ??= SongMatchOptions.Default;
        var titleA = ParseTitle(a.Title, a.Artist ?? "");
        var titleB = ParseTitle(b.Title, b.Artist ?? "");
        var title = CompareTitles(titleA, titleB, options);
        if (title.Verdict == SongVerdict.Different) return title;

        var artistA = CreditOf(a.Artist, titleA);
        var artistB = CreditOf(b.Artist, titleB);
        var artist = CompareArtists(artistA, artistB);
        switch (artist)
        {
            case ArtistAgreement.Unknown:
                return new(SongVerdict.Different, 0.6, "no artist to compare");
            case ArtistAgreement.None:
                return new(SongVerdict.Different, 0.9, $"different artists ('{artistA.Display}' and '{artistB.Display}')");
        }
        if (title.Verdict == SongVerdict.SameSongDifferentVersion) return title;
        if (artist == ArtistAgreement.Conflict)
            return new(SongVerdict.SameSongDifferentVersion, 0.75, "different guest artists");

        var confidence = title.Confidence;
        var reasons = new List<string>();
        if (title.Reason != "the same title") reasons.Add(title.Reason);
        if (artist == ArtistAgreement.Loose)
        {
            confidence -= 0.1;
            reasons.Add("the same artist once stylized characters are read as letters");
        }

        if (options.LengthToleranceSeconds is { } tolerance)
        {
            if (a.Seconds is > 0 && b.Seconds is > 0)
            {
                var apart = Math.Abs(a.Seconds.Value - b.Seconds.Value);
                if (apart > tolerance)
                    return new(SongVerdict.SameSongDifferentVersion, 0.8, $"lengths differ by {apart:0.#} s");
            }
            else
            {
                confidence -= 0.05;
                reasons.Add("a length is unknown");
            }
        }

        return new(SongVerdict.Same, Math.Round(Math.Clamp(confidence, 0, 1), 2),
            reasons.Count == 0 ? "the same song" : string.Join("; ", reasons));
    }

    public static SongMatch Same(string? aTitle, string? aArtist, string? bTitle, string? bArtist,
        SongMatchOptions? options = null) =>
        Same(new SongRef(aTitle, aArtist), new SongRef(bTitle, bArtist), options);

    /// <summary>The artist credit of one side, with the guests its title names, and the artist
    /// an "Artist - Title" title gave when the credit itself is empty.</summary>
    private static SongArtists CreditOf(string? artist, SongTitle title)
    {
        var credit = ParseArtists(string.IsNullOrWhiteSpace(artist) ? title.ArtistFromTitle : artist);
        if (title.Featured.Count == 0) return credit;
        return credit with { Featured = credit.Featured.Concat(title.Featured).ToList() };
    }

    /// <summary>Within the tolerance, or unknown on either side.</summary>
    public static bool LengthFits(int? want, double? got, int tolerance = LengthToleranceSeconds) =>
        want is not > 0 || got is not > 0 || Math.Abs(got.Value - want.Value) <= tolerance;

    /// <summary>
    /// A key for "this song in this version", for deduplicating and for remembering songs across
    /// sources: the primary artist and the title keys, and the version markers that make a
    /// different recording. "Drake feat. Rihanna - Too Good" and "Drake - Too Good (feat.
    /// Rihanna)" share one; "Song (Live)" has its own.
    /// </summary>
    public static string MatchKey(string? artist, string? title)
    {
        var parsed = ParseTitle(title, artist ?? "");
        var credit = ParseArtists(string.IsNullOrWhiteSpace(artist) ? parsed.ArtistFromTitle : artist);
        var primary = Key(credit.Primary);
        if (Aliases.TryGetValue(primary, out var alias)) primary = alias;
        return $"{primary}|{VersionedKey(parsed)}";
    }

    /// <summary>The title part of <see cref="MatchKey"/>, for songs already known to share an
    /// artist, such as the tracks of one album.</summary>
    public static string TitleKey(string? title) => VersionedKey(ParseTitle(title));

    private static string VersionedKey(SongTitle parsed)
    {
        var versions = DistinctVersions(parsed).Order(StringComparer.Ordinal).ToList();
        return versions.Count == 0 ? parsed.Key : $"{parsed.Key}|{string.Join('+', versions)}";
    }

    /// <summary>
    /// Whether two names are one artist, whole: case, accents, a leading "The", stylized
    /// characters and the alias table ignored, but never split, so "Bob Marley" is not "Bob
    /// Marley &amp; The Wailers". For an artist page, where a credit's guests do not belong.
    /// </summary>
    public static bool SameArtistName(string? a, string? b)
    {
        var left = ParseArtists(a).Display;
        var right = ParseArtists(b).Display;
        if (Key(left).Length == 0 || Key(right).Length == 0) return false;
        return Keys([left], false).Overlaps(Keys([right], false)) || Keys([left], true).Overlaps(Keys([right], true));
    }

    /// <summary>
    /// The name an artist is best known by, when <paramref name="artist"/> credits them under
    /// another name in the alias table: "Kanye West" for "Ye". Null when the name is not an
    /// alias or already is that name. A source that files a renamed artist under one name only
    /// finds nothing under the other, so this is the second name to ask.
    /// </summary>
    public static string? KnownName(string? artist)
    {
        var key = Key(ParseArtists(artist).Primary);
        if (!Aliases.TryGetValue(key, out var canonical)
            || !AliasNames.TryGetValue(canonical, out var name)) return null;
        return Key(name) == key ? null : name;
    }

    // ---- searching ----------------------------------------------------------------------

    /// <summary>
    /// The queries to try against a search API, in order, stopping at the first that finds the
    /// song:
    ///
    /// 1. the title and artist as given;
    /// 2. the title without brackets, guests or version tails, and the credit without aliases;
    /// 3. the same with stylized characters read as letters ("suicideboys suicide");
    /// 4. the primary artist only;
    /// 5. the title alone, as a last resort.
    ///
    /// A variant that reads the same as an earlier one is left out, so a plain "Drake - Landed"
    /// has two. Whatever a variant finds must still pass <see cref="Same(SongRef, SongRef, SongMatchOptions?)"/>
    /// against the song as asked: a looser query never means a looser match.
    /// </summary>
    public static IReadOnlyList<SongQuery> QueryVariants(string? title, string? artist)
    {
        var variants = new List<SongQuery>();
        void Add(string t, string a)
        {
            t = Whitespace.Replace(t, " ").Trim();
            a = Whitespace.Replace(a, " ").Trim();
            if (Key(t).Length == 0) return;
            if (variants.Any(v => string.Equals(Fold(v.Title), Fold(t), StringComparison.OrdinalIgnoreCase)
                    && string.Equals(Fold(v.Artist), Fold(a), StringComparison.OrdinalIgnoreCase))) return;
            variants.Add(new SongQuery(t, a));
        }

        var parsed = ParseTitle(title, artist ?? "");
        var credit = ParseArtists(string.IsNullOrWhiteSpace(artist) ? parsed.ArtistFromTitle : artist);
        var cleanTitle = parsed.Core;
        var parts = Bracket.Matches(Fold(title)).Select(match => match.Groups[1].Value.Trim())
            .Where(inner => PartNumber.IsMatch(inner)).ToList();
        if (parts.Count > 0) cleanTitle = $"{cleanTitle} {string.Join(' ', parts)}";

        Add((title ?? "").Trim(), (artist ?? "").Trim());
        Add(cleanTitle, credit.Display);
        Add(FoldStylized(cleanTitle), FoldStylized(credit.Display));
        Add(cleanTitle, credit.Primary);
        Add(cleanTitle, "");
        return variants;
    }
}
