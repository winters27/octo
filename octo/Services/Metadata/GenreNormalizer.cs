using System.Globalization;
using System.Text.RegularExpressions;
using Octo.Models.Settings;

namespace Octo.Services.Metadata;

/// <summary>The normalised genres for a set of raw values, and which rule decided the first one.</summary>
public sealed record GenreNormalizationResult(
    IReadOnlyList<string> Genres, string? Primary, string? MatchedRule);

public enum GenreTagAction { None, Write, Clear }

/// <summary>What should actually be written to a file's genre frame, including the decision
/// to write NOTHING and the decision to write an EMPTY set.</summary>
public sealed record GenreTagPlan(
    GenreTagAction Action, IReadOnlyList<string> Genres, string? Primary, string? MatchedRule);

/// <summary>
/// Turns whatever a downloader, a Soulseek peer or Deezer called a genre into the small set
/// of genres a library can browse.
///
/// Static and synchronous on purpose: the fallback lookup is the caller's async problem, and
/// a backfill over two thousand files has to be able to run this with no network at all.
/// </summary>
public static class GenreNormalizer
{
    /// <summary>
    /// Characters a tagger uses to cram several genres into one string. '&amp;' is NOT here:
    /// splitting on it destroys "R&amp;B" and "Drum &amp; Bass".
    /// </summary>
    private static readonly char[] Separators = [';', ',', '|', '\0', '/'];

    /// <summary>
    /// A "1998" or a "90s" says when, not what.
    ///
    /// Radio's kinship filter and the genre blocklist both need this rule, so it exists once:
    /// two hand-rolled copies is how one of them quietly stops dropping "2020s" when someone
    /// fixes the other.
    /// </summary>
    public static bool IsYearLike(string tag)
    {
        if (tag.Length == 0) return false;
        if (tag.All(char.IsAsciiDigit)) return true;                    // 1998, 2026, 80
        if (!tag.EndsWith("s", StringComparison.Ordinal)) return false;
        var stem = tag[..^1];                                           // 90s, 1990s, 00s
        return stem.Length is > 0 and <= 4 && stem.All(char.IsAsciiDigit);
    }

    /// <summary>
    /// Decide what to write to a file, given the frame it already carries and whatever genre
    /// was resolved for it.
    /// </summary>
    public static GenreTagPlan Plan(IReadOnlyList<string> existingFrame, string? resolved,
        GenreSettings settings, IReadOnlyList<string>? fallbackTags = null)
    {
        // Priority: the value we resolved, which is Deezer's curated album genre, leads. Then
        // the file's own frame in frame order. A stranger's tag is evidence, not authority.
        var candidates = new List<string?>(existingFrame.Count + 1) { resolved };
        candidates.AddRange(existingFrame);

        var result = Normalize(candidates, settings);
        if (result.Genres.Count == 0 && fallbackTags is { Count: > 0 })
            result = Normalize(fallbackTags, settings);

        if (result.Genres.Count > 0)
            return new GenreTagPlan(GenreTagAction.Write, result.Genres, result.Primary, result.MatchedRule);

        // Nothing survived. Whether that CLEARS the frame or leaves it is the difference
        // between fixing the bug and reproducing it: genre was only ever written when
        // non-empty, so a file that arrived tagged "People & Blogs" kept it forever.
        //
        // The one guard: a file that had no frame and resolved to nothing is left completely
        // alone. Writing an empty frame over an absent one is a rewrite with no benefit, and
        // it dirties a file the run should have left byte-identical.
        var hadSomething = existingFrame.Count > 0 || !string.IsNullOrWhiteSpace(resolved);
        if (!hadSomething) return new GenreTagPlan(GenreTagAction.None, [], null, null);

        return settings.OnEmpty switch
        {
            GenreEmptyBehavior.Clear => new GenreTagPlan(GenreTagAction.Clear, [], null, null),
            GenreEmptyBehavior.Unknown => new GenreTagPlan(GenreTagAction.Write,
                [settings.EffectiveUnknownLabel], settings.EffectiveUnknownLabel, null),
            _ => new GenreTagPlan(GenreTagAction.None, [], null, null),
        };
    }

    /// <summary>
    /// The pipeline, and the ORDER is the design:
    /// split, canonicalise, blocklist, map, case, dedupe, cap.
    /// </summary>
    public static GenreNormalizationResult Normalize(IEnumerable<string?> raw, GenreSettings settings)
    {
        var blocked = settings.EffectiveBlocklist();
        var rules = settings.EffectiveMappings();
        var cap = settings.EffectiveMaxGenres;

        var kept = new List<string>();
        var seen = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        string? matchedRule = null;

        foreach (var value in raw)
        {
            if (string.IsNullOrWhiteSpace(value)) continue;
            foreach (var token in Tokenize(value, rules))
            {
                if (blocked.Contains(token.Key) || IsYearLike(token.Key)) continue;

                // An empty target is a delete, documented rather than accidental.
                if (token.Display.Length == 0) continue;
                if (!seen.Add(token.Display)) continue;

                kept.Add(token.Display);
                matchedRule ??= token.Rule is null ? null : $"{token.Rule.Pattern} -> {token.Rule.Genre}";
                if (kept.Count >= cap) return Done(kept, matchedRule);
            }
        }

        return Done(kept, matchedRule);
    }

    private static GenreNormalizationResult Done(List<string> kept, string? rule) =>
        new(kept, kept.FirstOrDefault(), rule);

    private readonly record struct Token(string Key, string Display, GenreMappingSettings? Rule);

    /// <summary>
    /// One raw value becomes one or more candidate tokens.
    ///
    /// The whole string gets its own pass at the rules BEFORE it is split, so a user rule
    /// written for a compound form ("hip-hop/rap") can fire before '/' tears it in half. It is
    /// only emitted when a rule actually matches it, because otherwise an unmapped "Rock, Pop"
    /// would be title-cased whole and kept as a single bogus genre.
    /// </summary>
    private static IEnumerable<Token> Tokenize(string value, IReadOnlyList<GenreMappingSettings> rules)
    {
        var wholeKey = Canonicalize(value);
        if (wholeKey.Length is > 0 and <= 60)
        {
            var wholeRule = FirstMatch(rules, wholeKey);
            if (wholeRule is not null)
            {
                yield return new Token(wholeKey, wholeRule.Genre, wholeRule);
                yield break;
            }
        }

        foreach (var part in value.Split(Separators,
                     StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries))
        {
            var key = Canonicalize(part);
            if (key.Length is 0 or > 60) continue;
            var rule = FirstMatch(rules, key);
            yield return new Token(key, rule is null ? TitleCaseGenre(key, part) : rule.Genre, rule);
        }
    }

    /// <summary>
    /// Lowercase and single-space via the primitive the radio code already uses, with ID3v1
    /// numeric residue stripped first: an ID3v1 tag inside a v2 frame sometimes leaves the
    /// literal text "(17)" or "(17)Rock".
    /// </summary>
    private static string Canonicalize(string value) =>
        DiscoveryStationSettings.NormalizeTag(StripNumericGenre(value));

    /// <summary>"(17)" and "(17)Rock" are ID3v1 numeric genres leaking through as text. Applied
    /// to the display copy as well as the matching key, or preserving the source's own spelling
    /// would preserve the residue with it.</summary>
    private static string StripNumericGenre(string value) =>
        string.Join(' ', Regex.Replace(value, @"^\s*\(\d+\)\s*", "")
            .Trim().Split(' ', StringSplitOptions.RemoveEmptyEntries));

    /// <summary>
    /// First rule wins and mapping STOPS for that token. No re-entry: letting "rap -&gt; Rap"
    /// feed "Rap -&gt; Hip-Hop" means a user's table can loop.
    /// </summary>
    private static GenreMappingSettings? FirstMatch(IReadOnlyList<GenreMappingSettings> rules, string key)
    {
        foreach (var rule in rules)
        {
            if (!rule.Enabled || rule.Pattern.Length == 0) continue;
            var hit = rule.Match == GenreMatchMode.Exact
                ? key.Equals(rule.Pattern, StringComparison.OrdinalIgnoreCase)
                : key.Contains(rule.Pattern, StringComparison.OrdinalIgnoreCase);
            if (hit) return rule;
        }
        return null;
    }

    /// <summary>
    /// Title-case an unmapped genre for display.
    ///
    /// The acronym guard has to read the ORIGINAL text, not the canonical key: the key has
    /// already been lowercased, so asking whether it is all-caps always answers no and EDM,
    /// IDM and UKG all come back as Edm, Idm and Ukg.
    ///
    /// Invariant culture throughout, because under a Turkish locale ToTitleCase turns "indie"
    /// into something that no longer matches "Indie", and a container's locale is not
    /// something a user thinks of as a genre setting.
    /// </summary>
    private static string TitleCaseGenre(string key, string original)
    {
        var trimmed = StripNumericGenre(original);
        if (trimmed.Length == 0) return CultureInfo.InvariantCulture.TextInfo.ToTitleCase(key);
        if (trimmed.Length is > 0 and <= 4
            && trimmed.Any(char.IsLetter)
            && trimmed.All(c => !char.IsLetter(c) || char.IsUpper(c))) return trimmed;

        // Already capitalised by whoever wrote it, so leave their spelling alone. Octo has no
        // business re-capitalising a genre it does not recognise, and doing so counted as a
        // "change": a whole-library run proposed rewriting a file purely to turn
        // "Alternatif et Indé" into "Alternatif Et Indé". Tidying case is only worth a write
        // when the source clearly did not bother, which is when it is entirely lower case.
        if (trimmed.Any(char.IsUpper)) return trimmed;

        return CultureInfo.InvariantCulture.TextInfo.ToTitleCase(key);
    }
}
