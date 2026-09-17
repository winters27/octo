using System.Globalization;
using System.Text;

namespace Octo.Services.Common;

/// <summary>
/// Compares titles and names the way a listener would, so that "What It's Like" and
/// "What It’s Like" are one song rather than two.
///
/// Every merge in Octo asks the same question - do I already own this? - by comparing
/// a string from Navidrome (whatever the file's tags say) against a string from a
/// metadata provider (whatever its house style is). Those two disagree about
/// typography constantly: curly versus straight apostrophes, en dashes versus hyphens,
/// accents present or stripped. An ordinal comparison reads each disagreement as a
/// different track, and the consequences are visible:
///
///   - getAlbum appends a preview copy of a track already in the album, so the album
///     lists the same song twice (observed on "Grandson, Vol. 1": three of thirteen
///     titles carry U+2019 and all three were duplicated).
///   - A radio station streams a YouTube preview of a track whose FLAC is right there
///     in the library.
///
/// The key is deliberately conservative. It folds case, Unicode punctuation and
/// diacritics, drops apostrophes entirely ("ain't" = "aints" is not a risk, but
/// "aint" = "ain't" is a real tagging difference), and collapses whitespace. It does
/// NOT drop other punctuation or parenthetical suffixes: "Crazy Story" and
/// "Crazy Story, Pt. 3" are different songs and must stay different.
/// </summary>
public static class TitleMatch
{
    /// <summary>
    /// A comparison key. Equal keys mean "treat these as the same title"; an empty key
    /// means there was nothing to compare and callers should not match on it.
    /// </summary>
    public static string Key(string? value)
    {
        if (string.IsNullOrWhiteSpace(value)) return string.Empty;

        var builder = new StringBuilder(value.Length);
        var pendingSpace = false;

        // FormD separates a letter from its accents, so the accents can be dropped by
        // category below and "Beyoncé" meets "Beyonce".
        foreach (var rune in value.Normalize(NormalizationForm.FormD))
        {
            var ch = Fold(rune);
            if (ch == '\0') continue;

            if (char.IsWhiteSpace(ch))
            {
                pendingSpace = builder.Length > 0;
                continue;
            }

            if (pendingSpace)
            {
                builder.Append(' ');
                pendingSpace = false;
            }
            builder.Append(char.ToLowerInvariant(ch));
        }

        return builder.ToString();
    }

    /// <summary>Same title, allowing for typography.</summary>
    public static bool Same(string? left, string? right)
    {
        var key = Key(left);
        return key.Length > 0 && key == Key(right);
    }

    /// <summary>
    /// Either title contains the other, compared on keys. This is how a candidate from
    /// a recommendation provider is matched against a library hit, where one side often
    /// carries a suffix the other does not ("Jimmy" vs "Jimmy (feat. Lil Durk)").
    /// </summary>
    public static bool ContainsEither(string? left, string? right)
    {
        var leftKey = Key(left);
        var rightKey = Key(right);
        if (leftKey.Length == 0 || rightKey.Length == 0) return false;
        return leftKey.Contains(rightKey, StringComparison.Ordinal)
            || rightKey.Contains(leftKey, StringComparison.Ordinal);
    }

    /// <summary>A comparison key for "the same release by the same artist".</summary>
    public static string? AlbumKey(string? artist, string? album)
    {
        var albumKey = Key(album);
        return albumKey.Length == 0 ? null : $"{Key(artist)}|{albumKey}";
    }

    /// <summary>
    /// One character's fate: '\0' to drop it, itself or an ASCII stand-in to keep it.
    /// </summary>
    private static char Fold(char value) => value switch
    {
        // Apostrophes and primes, in every form tags use them. Dropped rather than
        // normalised: sources disagree about whether the character is there at all.
        '\'' or '‘' or '’' or '‚' or '‛' or 'ʼ' or 'ʹ'
            or '′' or '`' or '´' => '\0',
        // Quotation marks, the same argument.
        '"' or '“' or '”' or '„' or '‟' or '″' => '\0',
        // Dashes: a hyphen in one catalogue is an en dash in another.
        '‐' or '‑' or '‒' or '–' or '—' or '―'
            or '−' => '-',
        // Combining accents left over from FormD.
        _ when CharUnicodeInfo.GetUnicodeCategory(value) == UnicodeCategory.NonSpacingMark => '\0',
        // Zero-width and bidi marks, which arrive invisibly inside tags.
        '​' or '‌' or '‍' or '‎' or '‏' or '﻿' => '\0',
        _ => value,
    };
}
