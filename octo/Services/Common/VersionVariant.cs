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
        (Rx(@"\bmash[\s-]?ups?\b"), "mashup"),
        (Rx(@"\bmedley\b"), "medley"),
        (Rx(@"\bmega[\s-]?mix\b"), "megamix"),
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

    // ---- A shortened cut named plainly, and songs joined into one ----------------------------

    /// <summary>
    /// Whether the request asks for a shortened cut ("(Radio Edit)") the file does not name, and
    /// the file's name is more than the song's title. A plainly named file is taken for the cut
    /// only when it is named as just the song: "01. Get Lucky", "Daft Punk - Get Lucky (feat.
    /// Pharrell Williams)". "Get Lucky x Alakazam!" of the same length is another record.
    /// </summary>
    public static bool CutNotPlain(string filename, string? title, string? artist = null) =>
        Missing(filename, title, artist).Any(Cuts.Contains) && !JustTheTitle(filename, title, artist);

    /// <summary>A format or quality a peer puts beside a song's name: "[FLAC]", "(16-44)", "320".</summary>
    private static readonly Regex FormatTag = Rx(@"^(?:flac|mp3|wav|alac|aiff?|ape|lossless|hires|web|cd|vinyl|bit|khz|kbps|\d+)+$");

    /// <summary>
    /// Whether the file's own name is the song's title and nothing else: a track number, the
    /// artist (with a guest credit), a guest credit, a year or a format tag may stand beside it,
    /// split by the usual " - ".
    /// </summary>
    internal static bool JustTheTitle(string filename, string? title, string? artist)
    {
        var wanted = SongIdentity.Key(SongIdentity.ParseTitle(title).Core);
        if (wanted.Length == 0) return true;
        var artists = string.IsNullOrWhiteSpace(artist) ? new HashSet<string>()
            : SongIdentity.ParseArtists(artist).Names.Append(artist).Select(SongIdentity.Key).Where(key => key.Length > 0)
                .ToHashSet(StringComparer.Ordinal);
        var parts = Regex.Split(SongIdentity.Fold(LeafTitle(filename)), @"\s+[-\u2013]\s+")
            .Where(part => SongIdentity.Key(part).Length > 0).ToList();
        var titled = false;
        foreach (var part in parts)
        {
            var read = SongIdentity.ParseTitle(part);
            if (!titled && SongIdentity.Key(read.Core) == wanted && read.Extras.All(extra => FormatTag.IsMatch(extra)))
            {
                titled = true;
                continue;
            }
            if (Regex.IsMatch(part.Trim(), @"^\d{1,3}\.?$")) continue;
            if (artists.Contains(SongIdentity.Key(SongIdentity.StripFeatures(part)))) continue;
            return false;
        }
        return titled;
    }

    /// <summary>The versions that put two songs or more in one file.</summary>
    private static readonly HashSet<string> Joined = new(StringComparer.Ordinal) { "mashup", "medley", "megamix", "blend" };

    /// <summary>A word that joins another song onto this one's title: "Get Lucky x Alakazam!",
    /// "Get Lucky vs Billie Jean".</summary>
    private static readonly Regex JoinWord = Rx(@"^\s*(?:x|vs\.?|versus)\s+(\S.*)$");

    /// <summary>A song run straight on after the title: "Get Lucky-Billie Jean".</summary>
    private static readonly Regex RunOn = Rx(@"^-(?=\S)(.+)$");

    /// <summary>
    /// Whether the file is a mash-up, medley, megamix or blend of the song with others, which the
    /// request did not ask for. Read from the words after the title, never before it, where " x "
    /// and "vs" are as likely an artist's credit ("Skrillex x Diplo - ..."), and never when the
    /// title itself has the word.
    /// </summary>
    public static bool MashUp(string filename, string? title, string? artist = null)
    {
        if (string.IsNullOrWhiteSpace(title)) return false;
        var wanted = Requested(title, null);
        if (Carried(filename, artist).Any(version => Joined.Contains(version) && !wanted.Contains(version))) return true;

        var name = SongIdentity.Plain(Octo.Services.Soulseek.SoulseekDownloadService.NameTitle(title)).Trim();
        if (name.Length == 0 || Regex.IsMatch(name, @"(?<![\p{L}\p{N}])(?:x|vs\.?|versus)(?![\p{L}\p{N}])")) return false;
        var leaf = SongIdentity.Plain(WithoutArtist(LeafTitle(filename), artist));
        var found = Regex.Match(leaf, $@"(?<![\p{{L}}\p{{N}}]){Regex.Escape(name)}(?![\p{{L}}\p{{N}}])");
        if (!found.Success) return false;
        var tail = leaf[(found.Index + found.Length)..];
        var bracket = tail.IndexOfAny(['(', '[', '{']);
        if (bracket >= 0) tail = tail[..bracket];
        tail = tail.TrimEnd();

        // Whoever the title credits may follow it: "Get Lucky x Pharrell Williams" is not two songs.
        var credited = SongIdentity.ParseTitle(title).Featured
            .Concat(string.IsNullOrWhiteSpace(artist) ? [] : SongIdentity.ParseArtists(artist).Names)
            .Select(SongIdentity.Key).ToHashSet(StringComparer.Ordinal);
        bool AnotherSong(string words)
        {
            var read = SongIdentity.ParseTitle($"x ({words.Trim()})");
            return read.Versions.Count == 0 && read.Featured.Count == 0 && read.Extras.Count > 0
                   && !credited.Contains(SongIdentity.Key(words));
        }
        var joined = JoinWord.Match(tail);
        if (joined.Success) return AnotherSong(joined.Groups[1].Value);
        var runOn = RunOn.Match(tail);
        return runOn.Success && AnotherSong(runOn.Groups[1].Value);
    }
}
