using System.Text.RegularExpressions;

namespace Octo.Services.Common;

/// <summary>
/// Which versions of a song a peer's file is, read from the places SongIdentity does not look:
/// the album folder a plainly named track sits in ("Too Close (Radio Edit) - Single", "Club Hits
/// (Extended Mixes)", "Mezzanine Remix Tapes '98"), and a file name that runs its version on with
/// no brackets ("09 Next-Too Close-radio edit"). Live takes keep their own, more generous rule in
/// <see cref="LiveVersion"/>.
/// </summary>
internal static class VersionVariant
{
    private static Regex Rx(string pattern) =>
        new(pattern, RegexOptions.IgnoreCase | RegexOptions.CultureInvariant | RegexOptions.Compiled);

    /// <summary>
    /// Words that name a version wherever they stand, bracketed or not, under SongIdentity's own
    /// names so the two readings can be compared. Only words no album, artist or label is likely
    /// to be called: no bare "clean" (Clean Bandit) or "acoustic" (Acoustic Soul); a bracket
    /// still reads those.
    /// </summary>
    private static readonly (Regex Pattern, string Name)[] Words =
    [
        (Rx(@"\bradio\s+(?:edit|version|mix|cut)s?\b"), "radio edit"),
        (Rx(@"\bextended\b(?!\s+(?:edition|play)\b)"), "extended"),
        (Rx(@"\b(?:re-?mix(?:es|ed)?|rmx)\b"), "remix"),
        (Rx(@"\b(?:sped|speed)\s*up\b"), "sped up"),
        (Rx(@"\bslowed\b"), "slowed"),
        (Rx(@"\bnightcore\b"), "nightcore"),
        (Rx(@"\binstrumentals?\b"), "instrumental"),
        (Rx(@"\b(?:a\s*cappellas?|acapellas?)\b"), "acapella"),
        (Rx(@"\bkaraoke\b"), "karaoke"),
        (Rx(@"\bdrumless\b"), "drumless"),
        (Rx(@"\b8d\s+audio\b"), "8d"),
        (Rx(@"\b(?:dj|continuous|non-?stop)\s+mix(?:es)?\b|\bmixed\s+by\b"), "dj mix"),
    ];

    /// <summary>
    /// What a folder's brackets can say that is no version of the files in it: a generic
    /// "(Japanese Version)" or "(Mix)", a label called Reprise, a "[+cover]" scan, a VIP pack.
    /// The live words are LiveVersion's.
    /// </summary>
    private static readonly HashSet<string> NotFromAFolder = new(StringComparer.Ordinal)
    {
        "mix", "edit", "version", "reprise", "cover", "vip", "live", "unplugged", "bootleg",
    };

    private static readonly HashSet<string> LiveNames = new(StringComparer.Ordinal) { "live", "unplugged", "bootleg" };

    /// <summary>An album edition, not a version of its songs.</summary>
    private static readonly Regex Edition = Rx(@"\bextended\s+(?:edition|play)\b");

    /// <summary>
    /// Requested versions a file may carry under another name and still be what was asked for:
    /// a shortened cut is the same recording, and peers often name a single's radio edit plainly.
    /// Every other requested version must show in the file's name or its folder.
    /// </summary>
    private static readonly HashSet<string> Cuts = new(StringComparer.Ordinal) { "radio edit", "edit" };

    private static IEnumerable<string> WordsIn(string? text) =>
        string.IsNullOrWhiteSpace(text) ? [] : Words.Where(word => word.Pattern.IsMatch(text)).Select(word => word.Name);

    /// <summary>The versions a request asks for, by its title and its album.</summary>
    public static IReadOnlySet<string> Requested(string? title, string? album)
    {
        var wanted = new HashSet<string>(StringComparer.Ordinal);
        foreach (var text in new[] { title, album })
        {
            if (string.IsNullOrWhiteSpace(text)) continue;
            wanted.UnionWith(SongIdentity.DistinctVersions(SongIdentity.ParseTitle(text)));
            wanted.UnionWith(WordsIn(text));
        }
        return wanted;
    }

    /// <summary>The text with the artist's own name taken out, so a band called Live, or an
    /// artist folder named after one, is never read as a version.</summary>
    public static string WithoutArtist(string text, string? artist)
    {
        if (string.IsNullOrWhiteSpace(artist) || string.IsNullOrEmpty(text)) return text;
        var names = SongIdentity.ParseArtists(artist).Names.Append(artist.Trim())
            .Where(name => name.Trim().Length > 1)
            .OrderByDescending(name => name.Length);
        foreach (var name in names)
            text = Regex.Replace(text, $@"(?<![\p{{L}}\p{{N}}]){Regex.Escape(name.Trim())}(?![\p{{L}}\p{{N}}])", " ",
                RegexOptions.IgnoreCase | RegexOptions.CultureInvariant);
        return text;
    }

    /// <summary>The versions one folder name says its files are, live words left out.</summary>
    internal static IReadOnlySet<string> InFolder(string folder, string? artist = null)
    {
        var text = Edition.Replace(WithoutArtist(folder, artist), " ");
        var found = SongIdentity.DistinctVersions(SongIdentity.ParseTitle(text))
            .Where(version => !NotFromAFolder.Contains(version))
            .ToHashSet(StringComparer.Ordinal);
        found.UnionWith(WordsIn(text));
        return found;
    }

    /// <summary>The album folder and the one above it (a disc folder sits between), never the
    /// share's top level, as LiveVersion reads them.</summary>
    internal static IEnumerable<string> AlbumFolders(string filename)
    {
        var parts = filename.Replace('\\', '/').Split('/', StringSplitOptions.RemoveEmptyEntries);
        return parts.Length > 1 ? parts[..^1].TakeLast(2) : [];
    }

    private static string LeafTitle(string filename)
    {
        var leaf = filename.Replace('\\', '/').Split('/', StringSplitOptions.RemoveEmptyEntries).LastOrDefault() ?? "";
        var dot = leaf.LastIndexOf('.');
        return dot > 0 ? leaf[..dot] : leaf;
    }

    /// <summary>
    /// The versions a peer's file is, by its own name and its album folders, live words included.
    /// Neutral markers (remaster, explicit, album version) are no version and are left out.
    /// </summary>
    public static IReadOnlySet<string> Carried(string filename, string? artist = null)
    {
        var leaf = WithoutArtist(LeafTitle(filename), artist);
        var carried = SongIdentity.DistinctVersions(SongIdentity.ParseTitle(leaf)).ToHashSet(StringComparer.Ordinal);
        carried.UnionWith(WordsIn(leaf));
        foreach (var folder in AlbumFolders(filename))
        {
            carried.UnionWith(InFolder(folder, artist));
            if (LiveVersion.Mentions(WithoutArtist(folder, artist))) carried.Add("live");
        }
        return carried;
    }

    /// <summary>
    /// Versions the file's album folders say it is that the request never asked for: "Too Close"
    /// from "Too Close (Radio Edit) - Single", "Angel" from "Mezzanine Remix Tapes '98". Live
    /// folders are LiveVersion's.
    /// </summary>
    public static IReadOnlySet<string> UnrequestedFromFolder(string filename, string? title, string? album, string? artist = null)
    {
        var wanted = Requested(title, album);
        return AlbumFolders(filename)
            .SelectMany(folder => InFolder(folder, artist))
            .Where(version => !wanted.Contains(version))
            .ToHashSet(StringComparer.Ordinal);
    }

    /// <summary>Versions the file's own name runs on without brackets that the title never
    /// asked for, which SongIdentity cannot read: "Too Close-radio edit".</summary>
    public static IReadOnlySet<string> UnrequestedInName(string filename, string? title, string? artist = null)
    {
        var wanted = Requested(title, null);
        return WordsIn(WithoutArtist(LeafTitle(filename), artist))
            .Where(version => !wanted.Contains(version))
            .ToHashSet(StringComparer.Ordinal);
    }

    /// <summary>
    /// Versions the request's title asks for that the file shows nowhere, its name or its album
    /// folders. Used for ranking: the copy that says it is the asked-for version goes first.
    /// </summary>
    public static IReadOnlySet<string> Missing(string filename, string? title, string? artist = null)
    {
        var wanted = SongIdentity.DistinctVersions(SongIdentity.ParseTitle(title));
        if (wanted.Count == 0) return wanted;
        var carried = Carried(filename, artist);
        return wanted
            .Where(version => !carried.Contains(version) && !(LiveNames.Contains(version) && carried.Contains("live")))
            .ToHashSet(StringComparer.Ordinal);
    }

    /// <summary>
    /// Whether the request asked for a version the file does not show, other than a shortened
    /// cut: "Song (Remix)", "(Acoustic)", "(Sped Up)" or "(Clean)" against a plainly named file
    /// from a plain folder, which is the original. A radio edit may be named plainly; the length
    /// check tells it from the album cut.
    /// </summary>
    public static bool LacksRequested(string filename, string? title, string? artist = null) =>
        Missing(filename, title, artist).Any(version => !Cuts.Contains(version));
}
