using System.Text;
using System.Text.RegularExpressions;
using Octo.Services.Common;

namespace Octo.Services.LastFm;

/// <summary>
/// Orders what Last.fm's track.search answers, after <see cref="LastFmSearchCleanup"/> has put
/// the names right. track.search ranks by fuzzy relevance, so "adele" answers with Skyfall
/// (1.4 M listeners) before Rolling in the Deep (2.7 M), and the first rows of a search are
/// whatever matched the text best rather than what people actually listen to. Every row carries
/// its listener count, so the answer is put in that order instead, after two clean-ups that
/// would otherwise let noise reach the top:
///
///   1. Rows in another script than the query ("Adele" answered with Cyrillic scrobbles of a
///      cover) are dropped, only when the query itself is Latin and only when the row's
///      letters are mostly in the other script. A Latin title with one Greek letter in it
///      ("Θ. Macarena") is a Latin title.
///   2. Versions of one song ("Hello", "Hello - Live", "Hello (2011 Remaster)") are one row, the
///      most listened. Remixes and features are other songs and stay.
/// </summary>
public static class LastFmSearchRanking
{
    // Words that make a bracket or a dash tail "the same song, another take".
    private static readonly string VariantWords =
        @"live|remaster(?:ed)?|version|edit|mono|stereo|demo|acoustic|deluxe|bonus|explicit|clean|single|radio|re-?recorded|session|unplugged";

    // Words that make it another song, whatever else it says: "Live (Remix)", "Radio Edit (feat. X)".
    private static readonly Regex Other = new(
        @"\b(?:remix|mix|rmx|feat|ft|featuring|with|vip|dub|rework|bootleg|flip|cover|instrumental|sped|slowed|reverb|nightcore)\b",
        RegexOptions.IgnoreCase | RegexOptions.Compiled);

    private static readonly Regex Variant = new($@"\b(?:{VariantWords})\b", RegexOptions.IgnoreCase | RegexOptions.Compiled);

    // "(...)" or "[...]" at the end of a title, or the tail after the last " - ".
    private static readonly Regex TrailingBracket = new(@"\s*[\(\[]([^\(\)\[\]]*)[\)\]]\s*$", RegexOptions.Compiled);
    private static readonly Regex DashTail = new(@"\s+[-–—]\s+([^-–—]*)$", RegexOptions.Compiled);

    /// <summary>Share of an artist and title's letters that has to be in another script for the
    /// row to be dropped: more than half.</summary>
    private const double NonLatinShare = 0.5;

    /// <summary>
    /// The rows of <paramref name="tracks"/> without foreign-script noise and repeated versions,
    /// most listened first. Rows without a count go last, and rows with the same count keep the
    /// order Last.fm gave them.
    /// </summary>
    public static List<LastFmService.SimilarTrack> Rank(string query, IReadOnlyList<LastFmService.SimilarTrack> tracks)
    {
        var dropForeign = !IsMostlyNonLatin(query);
        var kept = tracks
            .Where(t => !dropForeign || !IsMostlyNonLatin($"{t.Artist} {t.Title}"))
            .ToList();

        // One row per song and version-less title, the most listened (the first on a tie).
        var best = new Dictionary<string, int>(StringComparer.Ordinal);
        for (var i = 0; i < kept.Count; i++)
        {
            var key = VersionKey(kept[i]);
            if (!best.TryGetValue(key, out var at) || (kept[i].Listeners ?? -1) > (kept[at].Listeners ?? -1))
                best[key] = i;
        }
        var survivors = Enumerable.Range(0, kept.Count).Where(i => best[VersionKey(kept[i])] == i);

        // LINQ's OrderBy is stable, so equal counts stay in Last.fm's order.
        return survivors.Select(i => kept[i]).OrderByDescending(t => t.Listeners ?? -1).ToList();
    }

    /// <summary>Whether more than half of the letters of <paramref name="text"/> are in a script
    /// other than Latin (Cyrillic, Arabic, Hebrew, Thai, CJK, Hangul, Greek...).</summary>
    public static bool IsMostlyNonLatin(string? text)
    {
        if (string.IsNullOrEmpty(text)) return false;
        int letters = 0, foreign = 0;
        foreach (var rune in text.EnumerateRunes())
        {
            if (!Rune.IsLetter(rune)) continue;
            letters++;
            if (!IsLatinLetter(rune.Value)) foreign++;
        }
        return letters > 0 && foreign > letters * NonLatinShare;
    }

    // Basic Latin, Latin-1 Supplement, Latin Extended-A/B, IPA and spacing modifiers, Latin
    // Extended Additional, Latin Extended-C/D/E and fullwidth Latin. Letters from anywhere
    // else are another script.
    private static bool IsLatinLetter(int cp) =>
        cp < 0x0250
        || cp is >= 0x1D00 and <= 0x1DBF
        || cp is >= 0x1E00 and <= 0x1EFF
        || cp is >= 0x2C60 and <= 0x2C7F
        || cp is >= 0xA720 and <= 0xA7FF
        || cp is >= 0xAB30 and <= 0xAB6F
        || cp is >= 0xFF21 and <= 0xFF3A
        || cp is >= 0xFF41 and <= 0xFF5A;

    /// <summary>The artist and the title with a version marker taken off the end, as one key.
    /// A remix, a mix or a feature is not a version, so those titles keep their whole text.</summary>
    private static string VersionKey(LastFmService.SimilarTrack track)
    {
        var title = StripVersions(track.Title);
        return $"{SongIdentity.Key(SongIdentity.PrimaryArtist(track.Artist))}|{SongIdentity.Key(title)}";
    }

    private static string StripVersions(string title)
    {
        var text = title.Trim();
        for (var guard = 0; guard < 4; guard++)
        {
            var next = StripOne(text);
            if (next == text) break;
            text = next;
        }
        return text;
    }

    private static string StripOne(string text)
    {
        foreach (var pattern in new[] { TrailingBracket, DashTail })
        {
            var match = pattern.Match(text);
            if (!match.Success || match.Index == 0) continue;
            var inner = match.Groups[1].Value;
            if (Variant.IsMatch(inner) && !Other.IsMatch(inner)) return text[..match.Index].TrimEnd();
        }
        return text;
    }
}
