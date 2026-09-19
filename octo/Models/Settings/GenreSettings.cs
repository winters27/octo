namespace Octo.Models.Settings;

public enum GenreFallbackSource { None, LastFm, MusicBrainz }

/// <summary>What to write when everything normalises away to nothing.</summary>
public enum GenreEmptyBehavior { Leave, Clear, Unknown }

public enum GenreMatchMode { Contains, Exact }

public sealed class GenreMappingSettings
{
    public string Id { get; set; } = string.Empty;
    public string Pattern { get; set; } = string.Empty;
    public string Genre { get; set; } = string.Empty;
    public GenreMatchMode Match { get; set; } = GenreMatchMode.Contains;
    public bool Enabled { get; set; } = true;
}

/// <summary>
/// Collapses the genres downloads arrive with into a list a library can browse.
///
/// A top-level section rather than Metadata:Genre because the admin JS splits a control's
/// name on "." into exactly two segments, so only one nesting level works.
/// </summary>
public class GenreSettings
{
    /// <summary>
    /// Normalise genres on every download. Off by default: this rewrites a tag the user may
    /// have curated.
    /// Environment variable: GENRE_NORMALIZE
    /// </summary>
    public bool Enabled { get; set; } = false;

    /// <summary>
    /// Pattern to genre, APPLIED IN ORDER, first match wins. Not exposed as an env var:
    /// encoding structured rows in .env is brittle, the same call the pinned radio stations
    /// already make. Edit in the dashboard or settings.json.
    /// </summary>
    public List<GenreMappingSettings> Mappings { get; set; } = [];

    /// <summary>
    /// Values that are not genres at all, dropped before mapping. Added to the built-in list,
    /// never replacing it.
    /// Environment variable: GENRE_BLOCKLIST (comma separated)
    /// </summary>
    public List<string> Blocklist { get; set; } = [];

    /// <summary>
    /// Where to look when a file has no usable genre left.
    /// Environment variable: GENRE_FALLBACK
    /// </summary>
    public GenreFallbackSource Fallback { get; set; } = GenreFallbackSource.None;

    /// <summary>
    /// How many genres a track may keep.
    ///
    /// Defaults to the ceiling, which means "keep what is there". A low default would make
    /// simply switching normalisation on destructive: with no mapping table at all, a track
    /// tagged "Cloud Rap, Emo, Hip Hop, Trap" would silently become "Cloud Rap". Dropping junk,
    /// years and duplicates is what the feature is for; throwing away accurate genres is not,
    /// and nobody asked for it as a default.
    ///
    /// Set it to 1 if you want one broad genre per track, which is what makes browsing by genre
    /// useful on a library whose tags are a mess.
    /// Environment variable: GENRE_MAX
    /// </summary>
    public int MaxGenres { get; set; } = 10;

    /// <summary>
    /// What to write when everything normalises away to nothing.
    ///
    /// Clear is the default when the feature is on, and it is the whole point: genre was only
    /// ever written when non-empty and never cleared, so a file that arrived tagged
    /// "People &amp; Blogs" kept it forever. A normaliser that cannot delete cannot fix that.
    /// Environment variable: GENRE_ON_EMPTY
    /// </summary>
    public GenreEmptyBehavior OnEmpty { get; set; } = GenreEmptyBehavior.Clear;

    /// <summary>
    /// Literal written when OnEmpty is Unknown.
    /// Environment variable: GENRE_UNKNOWN_LABEL
    /// </summary>
    public string UnknownLabel { get; set; } = "Unknown";

    /// <summary>
    /// How many files in a row may fail to be written before a backfill gives up. A read-only
    /// mount should produce one clear failure, not one per file. 0 never gives up.
    /// Environment variable: GENRE_BACKFILL_MAX_FAILURES
    /// </summary>
    public int BackfillMaxConsecutiveFailures { get; set; } = 25;

    /// <summary>
    /// Extensions a whole-library backfill walks. Editable because which containers a library
    /// holds is the user's business, not Octo's.
    /// </summary>
    public List<string> BackfillExtensions { get; set; } = [];

    /// <summary>0 means never give up, so it is not clamped upward.</summary>
    public int EffectiveBackfillMaxConsecutiveFailures =>
        BackfillMaxConsecutiveFailures <= 0 ? 0 : Math.Clamp(BackfillMaxConsecutiveFailures, 1, 10_000);

    private static readonly string[] DefaultBackfillExtensions =
        [".flac", ".mp3", ".m4a", ".ogg", ".opus", ".wav", ".aiff", ".aif", ".wma"];

    /// <summary>Normalised to a leading dot and lowercase, so a user typing "FLAC" works.</summary>
    public IReadOnlySet<string> EffectiveBackfillExtensions()
    {
        var configured = (BackfillExtensions ?? [])
            .Select(entry => (entry ?? "").Trim().ToLowerInvariant())
            .Where(entry => entry.Length is > 0 and <= 10)
            .Select(entry => entry.StartsWith('.') ? entry : "." + entry)
            .ToList();

        return new HashSet<string>(
            configured.Count > 0 ? configured : DefaultBackfillExtensions,
            StringComparer.OrdinalIgnoreCase);
    }

    // Bounds are computed at READ time, hand-written, no data annotations: the house pattern.
    // A value that arrived from a hand-edited settings.json is sanitised where it is used.
    public int EffectiveMaxGenres => Math.Clamp(MaxGenres, 1, 10);

    public string EffectiveUnknownLabel =>
        (UnknownLabel ?? "").Trim() is { Length: > 0 and <= 60 } label ? label : "Unknown";

    /// <summary>
    /// Values that are never a genre regardless of settings: YouTube's category list, the
    /// container words a downloader invents, and the format tags peers add. A user blocklist
    /// ADDS to this and cannot remove from it, because "Music" is not a genre in any
    /// configuration.
    /// </summary>
    internal static readonly string[] BuiltInBlocklist =
    [
        "music", "people & blogs", "people and blogs", "gaming", "entertainment", "education",
        "news & politics", "science & technology", "howto & style", "film & animation",
        "autos & vehicles", "pets & animals", "sports", "travel & events", "comedy",
        "nonprofits & activism", "shows", "trailers",
        "unknown", "other", "misc", "miscellaneous", "genre", "audio", "soundtrack music",
        "youtube", "soulseek", "lossless", "flac", "mp3", "320kbps", "cd", "vinyl", "album",
    ];

    public IReadOnlySet<string> EffectiveBlocklist()
    {
        var set = new HashSet<string>(BuiltInBlocklist, StringComparer.OrdinalIgnoreCase);
        foreach (var entry in (Blocklist ?? []).Take(500))
        {
            var value = DiscoveryStationSettings.NormalizeTag(entry);
            if (value.Length is > 0 and <= 60) set.Add(value);
        }
        return set;
    }

    /// <summary>
    /// Rows in the order the user put them, sanitised. Order is meaning here, so a duplicate
    /// pattern keeps the FIRST occurrence: the later one could never fire anyway.
    /// </summary>
    public IReadOnlyList<GenreMappingSettings> EffectiveMappings()
    {
        var seenPatterns = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        var seenIds = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        var result = new List<GenreMappingSettings>();

        foreach (var source in (Mappings ?? []).Take(200))
        {
            var pattern = DiscoveryStationSettings.NormalizeTag(source.Pattern);
            if (pattern.Length is 0 or > 60 || !seenPatterns.Add(pattern)) continue;

            // Genre is kept VERBATIM apart from trimming: the user typed "Hip-Hop" and that
            // exact spelling is what lands in the file. Normalising it here would write
            // "hip-hop" into a genre browser.
            var genre = (source.Genre ?? "").Trim();
            if (genre.Length > 60) continue;

            var id = DiscoveryStationSettings.NormalizeId(source.Id);
            if (string.IsNullOrEmpty(id)) id = DiscoveryStationSettings.DeterministicId(pattern, [genre]);
            if (!seenIds.Add(id)) continue;

            result.Add(new GenreMappingSettings
            {
                Id = id,
                Pattern = pattern,
                Genre = genre,
                Match = source.Match,
                Enabled = source.Enabled,
            });
        }
        return result;
    }

    /// <summary>
    /// The broad-genre collapse, ordered MOST SPECIFIC FIRST because matching stops at the
    /// first hit: "trap latino" has to be seen before "trap", or Latin music becomes Hip-Hop.
    ///
    /// Lives here and only here. The dashboard fetches it from /api/admin/genre/presets rather
    /// than keeping a second copy in admin.js that would drift.
    /// </summary>
    public static IReadOnlyList<GenreMappingSettings> BroadGenrePreset() => Build(
        // Latin before rap, or "trap latino" and "latin trap" become Hip-Hop.
        ("trap latino", "Latin"), ("latin trap", "Latin"), ("reggaeton", "Latin"),
        ("musica mexicana", "Latin"), ("regional mexican", "Latin"), ("salsa", "Latin"),
        ("bachata", "Latin"), ("cumbia", "Latin"), ("latin", "Latin"),

        // Compound electronic names before their single-word parts.
        ("drum and bass", "Electronic"), ("drum & bass", "Electronic"),
        ("dance-pop", "Pop"), ("dance pop", "Pop"),

        ("k-pop", "Pop"), ("j-pop", "Pop"), ("synthpop", "Pop"), ("indie pop", "Pop"),

        ("pop rap", "Hip-Hop"), ("hip hop", "Hip-Hop"), ("hip-hop", "Hip-Hop"),
        ("trap", "Hip-Hop"), ("drill", "Hip-Hop"), ("grime", "Hip-Hop"),
        ("phonk", "Hip-Hop"), ("rap", "Hip-Hop"),

        ("r&b", "R&B"), ("rnb", "R&B"), ("rhythm and blues", "R&B"), ("neo-soul", "R&B"),
        ("soul", "R&B"), ("funk", "R&B"), ("motown", "R&B"),

        ("house", "Dance"), ("techno", "Dance"), ("trance", "Dance"), ("edm", "Dance"),
        ("dubstep", "Dance"), ("garage", "Dance"), ("disco", "Dance"),

        ("electronica", "Electronic"), ("electro", "Electronic"), ("ambient", "Electronic"),
        ("idm", "Electronic"), ("downtempo", "Electronic"), ("synthwave", "Electronic"),
        ("electronic", "Electronic"),

        ("metal", "Metal"), ("hardcore", "Metal"), ("grindcore", "Metal"), ("doom", "Metal"),

        ("punk", "Rock"), ("grunge", "Rock"), ("emo", "Rock"), ("shoegaze", "Rock"),
        ("indie rock", "Rock"), ("alternative", "Rock"), ("rock", "Rock"),

        ("country", "Country"), ("americana", "Country"), ("bluegrass", "Country"),
        ("blues", "Blues"),
        ("jazz", "Jazz"), ("bebop", "Jazz"), ("swing", "Jazz"),
        ("classical", "Classical"), ("baroque", "Classical"), ("orchestral", "Classical"),
        ("opera", "Classical"),
        ("folk", "Folk"), ("singer-songwriter", "Folk"), ("acoustic", "Folk"),
        ("reggae", "Reggae"), ("dancehall", "Reggae"), ("ska", "Reggae"), ("dub", "Reggae"),
        ("pop", "Pop"));

    private static IReadOnlyList<GenreMappingSettings> Build(params (string Pattern, string Genre)[] rows) =>
        rows.Select(row => new GenreMappingSettings
        {
            Id = DiscoveryStationSettings.DeterministicId(row.Pattern, [row.Genre]),
            Pattern = row.Pattern,
            Genre = row.Genre,
            Match = GenreMatchMode.Contains,
            Enabled = true,
        }).ToList();
}
