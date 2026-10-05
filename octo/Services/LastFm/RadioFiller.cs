using System.Text.RegularExpressions;

namespace Octo.Services.LastFm;

/// <summary>
/// Songs a radio leaves out: shorter than 45 seconds or longer than 45 minutes, an
/// interview or a podcast, or an intro, skit or interlude of four minutes or less. A
/// missing or zero length is not held against a song. The Octo apps apply the same rule.
/// </summary>
public static class RadioFiller
{
    private const int ShortestSeconds = 45;
    private const int LongestSeconds = 45 * 60;
    private const int ShortPieceSeconds = 4 * 60;

    // Whole words with the edges spelled out, matching the apps exactly.
    private static readonly Regex Talk = new(@"(?<![\p{L}\p{N}])(interview|podcast)(?![\p{L}\p{N}])",
        RegexOptions.IgnoreCase | RegexOptions.CultureInvariant);
    private static readonly Regex Filler = new(@"(?<![\p{L}\p{N}])(intro|introduction|skit|interlude)(?![\p{L}\p{N}])",
        RegexOptions.IgnoreCase | RegexOptions.CultureInvariant);

    public static bool IsFiller(string? title, int? durationSeconds)
    {
        var seconds = durationSeconds ?? 0;
        if (seconds > 0 && (seconds < ShortestSeconds || seconds > LongestSeconds)) return true;
        var text = title ?? string.Empty;
        if (Talk.IsMatch(text)) return true;
        return seconds is > 0 and <= ShortPieceSeconds && Filler.IsMatch(text);
    }
}
