namespace Octo.Models.Domain;

/// <summary>
/// Whether a song's words are explicit, the clean edit, or neither, as the catalog says it and
/// as OpenSubsonic clients read it. Kept as the catalog's own number on a <see cref="Song"/>
/// (<see cref="Song.ExplicitContentLyrics"/>), which the explicit filter already reads.
/// </summary>
public static class ExplicitStatus
{
    /// <summary>The catalog's numbers: 0 not explicit, 1 explicit, 3 the clean edit.</summary>
    public const int NotExplicit = 0, Explicit = 1, Clean = 3;

    /// <summary>The words OpenSubsonic's <c>explicitStatus</c> field takes; "" for neither or
    /// not known.</summary>
    public const string ExplicitWord = "explicit", CleanWord = "clean";

    /// <summary>
    /// The catalog's <c>explicit_content_lyrics</c> and <c>explicit_lyrics</c> as one number:
    /// 1 explicit, 3 the clean edit, 0 neither, null when it does not say. An album that is
    /// partly explicit (4) counts as explicit, as a client marks such an album. Its other values
    /// (2 unknown, 5, 6, 7 no advice) say nothing, so only the plain explicit flag can answer.
    /// </summary>
    public static int? FromCatalog(int? content, bool? explicitLyrics) => content switch
    {
        1 or 4 => Explicit,
        3 => Clean,
        0 => NotExplicit,
        _ => explicitLyrics == true ? Explicit : null,
    };

    /// <summary>The word a client gets for the catalog's number.</summary>
    public static string ForClients(int? content) => content switch
    {
        Explicit or 4 => ExplicitWord,
        Clean => CleanWord,
        _ => "",
    };

    /// <summary>The version a clean copy is told apart by in an outside id; null for any other.</summary>
    public static string? VersionOf(int? content) => content == Clean ? CleanWord : null;
}
