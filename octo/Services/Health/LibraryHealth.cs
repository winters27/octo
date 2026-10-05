using Octo.Services.Common;

namespace Octo.Services.Health;

// Library health: plain checks over the library's songs that find what is worth fixing in the
// files. Nothing is guessed from outside the library; the same songs always give the same report.
//
// A port of the Octo app's shared core (shared/core/.../health/LibraryHealth.kt), rule for rule,
// so the dashboard and the apps find the same things in one library. docs/library-health-cases.json
// holds the cases both sides are checked against; change a rule here and there together.

/// <summary>A tag the checks look for.</summary>
public enum HealthTag { Genre, Year, AlbumArtist, TrackNumber, Cover }

/// <summary>What the checks read from a song. Each caller reads its own kind of song.</summary>
public interface IHealthFields<T>
{
    string Id(T song);
    string Title(T song);
    string? Artist(T song);
    string? Album(T song);
    string? AlbumId(T song);
    string? AlbumArtist(T song);
    IReadOnlyList<string> Genres(T song);
    int? Year(T song);
    int? Disc(T song);
    int? Track(T song);
    /// <summary>Whole seconds, 0 when unknown.</summary>
    int Seconds(T song);
    string? Format(T song);
    bool Lossless(T song);

    /// <summary>Kilobits per second, bits per sample, samples per second.</summary>
    int? BitRate(T song);
    int? BitDepth(T song);
    int? SampleRate(T song);

    /// <summary>Codes that name the recording: its ISRCs and its MusicBrainz recording id.</summary>
    IReadOnlyList<string> Isrcs(T song);
    string? RecordingId(T song);
    string? Cover(T song);

    /// <summary>Every artist the song credits, and every album artist it names. One credit each
    /// when that is all there is.</summary>
    IReadOnlyList<string> Artists(T song) => Artist(song) is { } one ? [one] : [];
    IReadOnlyList<string> AlbumArtists(T song) => AlbumArtist(song) is { } one ? [one] : [];

    /// <summary>What names the release the song came out on, when it is known: its MusicBrainz
    /// release and release group, its barcode, and its record labels.</summary>
    string? ReleaseId(T song) => null;
    string? ReleaseGroupId(T song) => null;
    string? Barcode(T song) => null;
    IReadOnlyList<string> Labels(T song) => [];

    /// <summary>Where the song is kept. Copies in two places are not duplicates, and an album is
    /// never split across places. The server has one place.</summary>
    string Place(T song) => "";

    /// <summary>The tags this reader can see at all. One it never keeps is never reported missing.</summary>
    IReadOnlySet<HealthTag> Seen => LibraryHealth.AllTags;
}

/// <summary>Why copies were taken for one recording.</summary>
public enum DuplicateBasis
{
    /// <summary>Their tags name the same recording: an ISRC or a MusicBrainz id.</summary>
    Tags,
    /// <summary>The same title and artist, in the same version, and lengths within a few seconds.</summary>
    TitleAndLength,
}

/// <summary>Why the first copy of a set is the one to keep.</summary>
public enum BestReason
{
    /// <summary>It sounds better: lossless, more bits, a higher rate.</summary>
    Sound,
    /// <summary>The copies sound alike, and it has more of its tags.</summary>
    Tags,
    /// <summary>Nothing sets them apart.</summary>
    None,
}

/// <summary>Copies of one recording, the best first.</summary>
public sealed record DuplicateSet<T>(IReadOnlyList<T> Copies, DuplicateBasis Basis, BestReason BestReason)
{
    public T Best => Copies[0];
}

/// <summary>One way the parts of a split album differ, with each part's value.</summary>
public enum AlbumDifference { Title, AlbumArtist, Year, Other }

public sealed record AlbumDifferenceValues(AlbumDifference Kind, IReadOnlyList<string> Values)
{
    public bool Equals(AlbumDifferenceValues? other) =>
        other is not null && Kind == other.Kind && Values.SequenceEqual(other.Values);

    public override int GetHashCode() => HashCode.Combine(Kind, Values.Count);
}

/// <summary>One album entry the server shows, and its songs in album order.</summary>
public sealed record AlbumPart<T>(string AlbumId, IReadOnlyList<T> Songs);

/// <summary>Why parts of one name with different album artists are one album.</summary>
public enum SplitBasis
{
    /// <summary>One part's album artist is credited on the other part too: a song there is by
    /// them, or its album artist names them. A collaboration album filed under each of its artists.</summary>
    SharedArtist,
    SameRelease,
    SameReleaseGroup,
    SameBarcode,
    /// <summary>The parts came out on the same label in the same year.</summary>
    SameLabelAndYear,
}

/// <summary>
/// One reason, with what it rests on: for a shared artist, Artist is one part's album artist,
/// Other the other part's, and Song a song of the other part by Artist (empty when its album
/// artist names them); for the rest, Value is what the parts share.
/// </summary>
public sealed record SplitReason(SplitBasis Basis, string Artist = "", string Other = "", string Song = "", string Value = "");

/// <summary>
/// An album the server shows as two or more, because its songs' tags do not agree. The first
/// part is the one the others join: the largest, or the one whose album artist names every
/// part's artist. Reasons say why parts with different album artists were taken for one album;
/// parts by one album artist need none.
/// </summary>
public sealed record SplitAlbum<T>(string Title, string Artist, IReadOnlyList<AlbumPart<T>> Parts,
    IReadOnlyList<AlbumDifferenceValues> Differences, IReadOnlyList<SplitReason> Reasons);

/// <summary>What one check is about. The order is the order they are shown in.</summary>
public enum HealthCheck
{
    Duplicates,
    SplitAlbums,
    NoLength,
    NoTrackNumber,
    NoAlbumArtist,
    NoCover,
    NoYear,
    NoGenre,
}

/// <summary>Everything the checks found.</summary>
public sealed record HealthReport<T>(
    int Checked,
    IReadOnlyList<DuplicateSet<T>> Duplicates,
    IReadOnlyList<SplitAlbum<T>> SplitAlbums,
    IReadOnlyList<T> NoLength,
    IReadOnlyDictionary<HealthTag, IReadOnlyList<T>> Missing)
{
    /// <summary>How many things a check found: sets of copies, albums, or songs.</summary>
    public int Count(HealthCheck check) => check switch
    {
        HealthCheck.Duplicates => Duplicates.Count,
        HealthCheck.SplitAlbums => SplitAlbums.Count,
        HealthCheck.NoLength => NoLength.Count,
        _ => Missing.TryGetValue(LibraryHealth.TagOf(check)!.Value, out var songs) ? songs.Count : 0,
    };

    /// <summary>The checks that found something, in order.</summary>
    public IReadOnlyList<HealthCheck> Findings => Enum.GetValues<HealthCheck>().Where(check => Count(check) > 0).ToList();

    public bool Clean => Findings.Count == 0;

    /// <summary>The songs a check is about, for a list: each set of copies together with the best
    /// first, each split album's parts one after the other, and missing tags by artist and album.</summary>
    public IReadOnlyList<T> Songs(HealthCheck check) => check switch
    {
        HealthCheck.Duplicates => Duplicates.SelectMany(set => set.Copies).ToList(),
        HealthCheck.SplitAlbums => SplitAlbums.SelectMany(album => album.Parts.SelectMany(part => part.Songs)).ToList(),
        HealthCheck.NoLength => NoLength,
        _ => Missing.GetValueOrDefault(LibraryHealth.TagOf(check)!.Value) ?? [],
    };

    /// <summary>The report without some songs, as after they were removed from the library: a set
    /// left with one copy is no longer a duplicate.</summary>
    public HealthReport<T> Without(IReadOnlySet<string> ids, IHealthFields<T> fields)
    {
        if (ids.Count == 0) return this;
        bool Keep(T song) => !ids.Contains(fields.Id(song));
        return this with
        {
            Duplicates = Duplicates.Select(set => set with { Copies = set.Copies.Where(Keep).ToList() })
                .Where(set => set.Copies.Count >= 2).ToList(),
            SplitAlbums = SplitAlbums.Select(album => album with
                {
                    Parts = album.Parts.Select(part => part with { Songs = part.Songs.Where(Keep).ToList() })
                        .Where(part => part.Songs.Count > 0).ToList(),
                })
                .Where(album => album.Parts.Count >= 2).ToList(),
            NoLength = NoLength.Where(Keep).ToList(),
            Missing = Missing.Select(pair => (pair.Key, Songs: (IReadOnlyList<T>)pair.Value.Where(Keep).ToList()))
                .Where(pair => pair.Songs.Count > 0).ToDictionary(pair => pair.Key, pair => pair.Songs),
        };
    }

    /// <summary>The report with songs fixed for one check left out of that check only, until the
    /// server's list catches up: a song given its year still lacks its genre.</summary>
    public HealthReport<T> SettledFor(HealthCheck check, IReadOnlySet<string> ids, IHealthFields<T> fields)
    {
        if (ids.Count == 0) return this;
        var only = Without(ids, fields);
        switch (check)
        {
            case HealthCheck.Duplicates: return this with { Duplicates = only.Duplicates };
            case HealthCheck.SplitAlbums: return this with { SplitAlbums = only.SplitAlbums };
            case HealthCheck.NoLength: return this with { NoLength = only.NoLength };
        }
        var tag = LibraryHealth.TagOf(check)!.Value;
        var left = (Missing.GetValueOrDefault(tag) ?? []).Where(song => !ids.Contains(fields.Id(song))).ToList();
        var missing = Missing.Where(pair => pair.Key != tag).ToDictionary(pair => pair.Key, pair => pair.Value);
        if (left.Count > 0) missing[tag] = left;
        // Keep the tags in their order.
        return this with
        {
            Missing = Enum.GetValues<HealthTag>().Where(missing.ContainsKey).ToDictionary(t => t, t => missing[t]),
        };
    }
}

/// <summary>The checks themselves.</summary>
public static class LibraryHealth
{
    public static readonly IReadOnlySet<HealthTag> AllTags = Enum.GetValues<HealthTag>().ToHashSet();

    /// <summary>Lengths of copies of one recording may be this far apart, in seconds.</summary>
    public const int SameLengthSeconds = SongIdentity.LengthToleranceSeconds;

    /// <summary>A shared ISRC or MusicBrainz id counts only when the lengths are this close, so an
    /// album whose every song was tagged with one code by mistake is not taken for one song many times.</summary>
    public const int TaggedLengthSeconds = 10;

    private static readonly SongMatchOptions DuplicateTitles = new()
    {
        LengthToleranceSeconds = SameLengthSeconds,
        ExtrasMustAgree = true,
    };

    public static HealthTag? TagOf(HealthCheck check) => check switch
    {
        HealthCheck.NoTrackNumber => HealthTag.TrackNumber,
        HealthCheck.NoAlbumArtist => HealthTag.AlbumArtist,
        HealthCheck.NoCover => HealthTag.Cover,
        HealthCheck.NoYear => HealthTag.Year,
        HealthCheck.NoGenre => HealthTag.Genre,
        _ => null,
    };

    /// <summary>Runs every check over the songs.</summary>
    public static HealthReport<T> Check<T>(IReadOnlyList<T> songs, IHealthFields<T> fields) => new(
        songs.Count,
        FindDuplicates(songs, fields),
        FindSplitAlbums(songs, fields),
        songs.Where(song => fields.Seconds(song) <= 0).ToList(),
        FindMissingTags(songs, fields));

    // ---- duplicates -----------------------------------------------------------------------------

    /// <summary>
    /// Songs that are the same recording, in sets. A set's copies are all in one place. Songs whose
    /// tags name one recording (an ISRC or a MusicBrainz recording id, with lengths close) go
    /// together first; then songs by the same artist whose titles read the same, version and all,
    /// and whose lengths are within a few seconds.
    /// </summary>
    public static IReadOnlyList<DuplicateSet<T>> FindDuplicates<T>(IReadOnlyList<T> songs, IHealthFields<T> fields)
    {
        var sets = new Sets(songs.Count);
        var byTag = new Dictionary<string, int>(StringComparer.Ordinal);
        var byTitle = new Dictionary<string, List<int>>(StringComparer.Ordinal);
        var keys = MatchKeys(songs, fields);
        var tagged = new bool[songs.Count];

        for (var index = 0; index < songs.Count; index++)
        {
            var song = songs[index];
            var place = fields.Place(song);
            var codes = SongIdentity.Isrcs(fields.Isrcs(song)).Select(isrc => $"i:{isrc}").ToList();
            if (fields.RecordingId(song)?.Trim().ToLowerInvariant() is { Length: > 0 } recording) codes.Add($"m:{recording}");
            foreach (var code in codes)
            {
                var key = $"{place}\0{code}";
                if (!byTag.TryGetValue(key, out var first)) byTag[key] = index;
                else if (LengthsClose(fields.Seconds(songs[first]), fields.Seconds(song), TaggedLengthSeconds)
                         && sets.Join(first, index))
                {
                    tagged[first] = true;
                    tagged[index] = true;
                }
            }
            var match = keys[index];
            // A title with nothing to read in it is never matched on its words.
            var bar = match.IndexOf('|');
            if ((bar < 0 ? match : match[(bar + 1)..]).Length > 0)
            {
                var bucketKey = $"{place}\0{match}";
                if (!byTitle.TryGetValue(bucketKey, out var bucket)) byTitle[bucketKey] = bucket = new List<int>(2);
                bucket.Add(index);
            }
        }

        foreach (var bucket in byTitle.Values)
        {
            if (bucket.Count < 2) continue;
            for (var i = 0; i < bucket.Count; i++)
            {
                var a = songs[bucket[i]];
                var refA = RefOf(a, fields);
                for (var j = i + 1; j < bucket.Count; j++)
                {
                    if (sets.Same(bucket[i], bucket[j])) continue;
                    var b = songs[bucket[j]];
                    if (!LengthsClose(fields.Seconds(a), fields.Seconds(b), SameLengthSeconds)) continue;
                    if (SongIdentity.Same(refA, RefOf(b, fields), DuplicateTitles).IsSame) sets.Join(bucket[i], bucket[j]);
                }
            }
        }

        var groups = new Dictionary<int, List<int>>();
        var rootsInOrder = new List<int>();
        for (var index = 0; index < songs.Count; index++)
        {
            if (sets.Size(index) <= 1) continue;
            var root = sets.Root(index);
            if (!groups.TryGetValue(root, out var members))
            {
                groups[root] = members = new List<int>(2);
                rootsInOrder.Add(root);
            }
            members.Add(index);
        }
        var found = rootsInOrder.Select(root =>
        {
            var members = groups[root];
            var copies = members.Select(i => songs[i])
                .OrderByDescending(song => SoundOf(song, fields))
                .ThenByDescending(song => TagsOf(song, fields))
                .ThenBy(song => fields.Id(song), StringComparer.Ordinal)
                .ToList();
            var basis = members.Any(i => tagged[i]) ? DuplicateBasis.Tags : DuplicateBasis.TitleAndLength;
            return new DuplicateSet<T>(copies, basis, BestReasonOf(copies, fields));
        });
        return found
            .OrderBy(set => SortKey(fields.Artist(set.Best)), StringComparer.Ordinal)
            .ThenBy(set => SortKey(fields.Title(set.Best)), StringComparer.Ordinal)
            .ThenBy(set => fields.Id(set.Best), StringComparer.Ordinal)
            .ToList();
    }

    /// <summary>Each song's key for "this song in this version". Reading a title is the slow part
    /// of the checks, so each artist and title pair is read once, on every core.</summary>
    private static string[] MatchKeys<T>(IReadOnlyList<T> songs, IHealthFields<T> fields)
    {
        var pairs = new Dictionary<(string?, string), int>();
        var at = new int[songs.Count];
        for (var i = 0; i < songs.Count; i++)
        {
            var pair = (fields.Artist(songs[i]), fields.Title(songs[i]));
            if (!pairs.TryGetValue(pair, out var slot)) pairs[pair] = slot = pairs.Count;
            at[i] = slot;
        }
        var distinct = new (string?, string)[pairs.Count];
        foreach (var (pair, slot) in pairs) distinct[slot] = pair;
        var read = new string[distinct.Length];
        Parallel.For(0, distinct.Length, i => read[i] = SongIdentity.MatchKey(distinct[i].Item1, distinct[i].Item2));
        return at.Select(slot => read[slot]).ToArray();
    }

    private static SongRef RefOf<T>(T song, IHealthFields<T> fields) =>
        new(fields.Title(song), fields.Artist(song), fields.Seconds(song) > 0 ? fields.Seconds(song) : null)
        {
            Isrcs = fields.Isrcs(song).Cast<string?>().ToList(),
        };

    /// <summary>Within the tolerance, or a length unknown on either side.</summary>
    private static bool LengthsClose(int a, int b, int tolerance) => a <= 0 || b <= 0 || Math.Abs(a - b) <= tolerance;

    /// <summary>
    /// How good a copy sounds, as one number that sorts: lossless first, then more bits per
    /// sample, a higher sample rate, and a higher bit rate. The bit rate of two lossless copies of
    /// one depth says only how hard they were packed, so it counts there only when the depth is unknown.
    /// </summary>
    public static long SoundOf<T>(T song, IHealthFields<T> fields)
    {
        var lossless = fields.Lossless(song);
        var depth = Math.Clamp(fields.BitDepth(song) ?? 0, 0, 63);
        var rate = Math.Clamp((fields.SampleRate(song) ?? 0) / 100, 0, 65_535);
        var bits = lossless && depth > 0 ? 0 : Math.Clamp(fields.BitRate(song) ?? 0, 0, 65_535);
        return ((lossless ? 1L : 0L) << 48) | ((long)depth << 40) | ((long)rate << 20) | (long)bits;
    }

    /// <summary>How many of the tags the checks look for a song has.</summary>
    public static int TagsOf<T>(T song, IHealthFields<T> fields)
    {
        var count = 0;
        if (fields.Genres(song).Any(genre => !string.IsNullOrWhiteSpace(genre))) count++;
        if ((fields.Year(song) ?? 0) > 0) count++;
        if (!string.IsNullOrWhiteSpace(fields.AlbumArtist(song))) count++;
        if ((fields.Track(song) ?? 0) > 0) count++;
        if (!string.IsNullOrWhiteSpace(fields.Cover(song))) count++;
        return count;
    }

    private static BestReason BestReasonOf<T>(IReadOnlyList<T> copies, IHealthFields<T> fields)
    {
        var best = copies[0];
        var next = copies[1];
        if (SoundOf(best, fields) > SoundOf(next, fields)) return BestReason.Sound;
        if (TagsOf(best, fields) > TagsOf(next, fields)) return BestReason.Tags;
        return BestReason.None;
    }

    // ---- split albums ---------------------------------------------------------------------------

    /// <summary>
    /// Albums the server shows as more than one: one title, in one place, under two or more album
    /// ids, with years that do not tell them apart (the same year, or none on one side). Parts by
    /// the same album artist belong together. Parts by different album artists belong together
    /// only when something shows they are one release: the same MusicBrainz release or release
    /// group, the same barcode, the same label in the same year, or one part's album artist
    /// credited on the other part, as a collaboration album filed under each of its artists is. A
    /// shared credit alone does not join a title many artists use ("Greatest Hits", "Live") or an
    /// artist's own name. Two albums of one name from different years stay apart, and so do albums
    /// of one name by artists with nothing in common.
    /// </summary>
    public static IReadOnlyList<SplitAlbum<T>> FindSplitAlbums<T>(IReadOnlyList<T> songs, IHealthFields<T> fields)
    {
        var byAlbum = new Dictionary<string, List<T>>(StringComparer.Ordinal);
        var albumOrder = new List<string>();
        foreach (var song in songs)
        {
            if (fields.AlbumId(song) is not { Length: > 0 } id) continue;
            if (!byAlbum.TryGetValue(id, out var members))
            {
                byAlbum[id] = members = [];
                albumOrder.Add(id);
            }
            members.Add(song);
        }
        var buckets = new Dictionary<string, List<PartFacts>>(StringComparer.Ordinal);
        var bucketOrder = new List<string>();
        foreach (var id in albumOrder)
        {
            var members = byAlbum[id];
            var first = members[0];
            var title = SongIdentity.Key(fields.Album(first));
            if (title.Length == 0) continue;
            if (PartFactsOf(id, members, fields) is not { } facts) continue;
            var bucketKey = $"{fields.Place(first)}\0{title}";
            if (!buckets.TryGetValue(bucketKey, out var parts))
            {
                buckets[bucketKey] = parts = new List<PartFacts>(1);
                bucketOrder.Add(bucketKey);
            }
            parts.Add(facts);
        }

        var found = new List<SplitAlbum<T>>();
        foreach (var bucketKey in bucketOrder)
        {
            var parts = buckets[bucketKey];
            if (parts.Count < 2) continue;
            var title = bucketKey[(bucketKey.IndexOf('\0') + 1)..];
            var sets = new Sets(parts.Count);
            var reasons = new List<(int Part, SplitReason Reason)>();
            for (var i = 0; i < parts.Count; i++)
            for (var j = i + 1; j < parts.Count; j++)
            {
                var a = parts[i];
                var b = parts[j];
                if (!YearsAgree(a.Years, b.Years)) continue;
                if (a.ArtistKey == b.ArtistKey)
                {
                    sets.Join(i, j);
                    continue;
                }
                if ((SameRelease(a, b) ?? SharedArtist(a, b, title)) is not { } reason) continue;
                sets.Join(i, j);
                reasons.Add((i, reason));
            }

            var groupsInOrder = new List<List<int>>();
            var byRoot = new Dictionary<int, List<int>>();
            for (var i = 0; i < parts.Count; i++)
            {
                var root = sets.Root(i);
                if (!byRoot.TryGetValue(root, out var group))
                {
                    byRoot[root] = group = [];
                    groupsInOrder.Add(group);
                }
                group.Add(i);
            }
            foreach (var together in groupsInOrder.Where(group => group.Count > 1))
            {
                var bySize = together.OrderByDescending(i => parts[i].Size)
                    .ThenBy(i => parts[i].Id, StringComparer.Ordinal).ToList();
                // A part whose album artist already names every part's artist leads; otherwise
                // the largest does.
                var artists = together.Select(i => parts[i].ArtistKey).ToHashSet(StringComparer.Ordinal);
                int? named = artists.Count > 1
                    ? bySize.Cast<int?>().FirstOrDefault(i => artists.All(parts[i!.Value].AlbumArtists.ContainsKey))
                    : null;
                var lead = named ?? bySize[0];
                var ordered = new[] { lead }.Concat(bySize.Where(i => i != lead)).Select(i =>
                {
                    var id = parts[i].Id;
                    return new AlbumPart<T>(id, InAlbumOrder(byAlbum[id], fields));
                }).ToList();
                var leadSongs = ordered[0].Songs;
                var root = sets.Root(together[0]);
                found.Add(new SplitAlbum<T>(
                    fields.Album(leadSongs[0]) ?? "",
                    AlbumArtistOf(leadSongs, fields),
                    ordered,
                    DifferencesOf(ordered, fields),
                    reasons.Where(pair => sets.Root(pair.Part) == root).Select(pair => pair.Reason).Distinct().ToList()));
            }
        }
        return found
            .OrderBy(album => SortKey(album.Artist), StringComparer.Ordinal)
            .ThenBy(album => SortKey(album.Title), StringComparer.Ordinal)
            .ThenBy(album => album.Parts[0].AlbumId, StringComparer.Ordinal)
            .ToList();
    }

    /// <summary>A map that keeps the order its keys were first put in, as the app's LinkedHashMap does.</summary>
    private sealed class Ordered
    {
        private readonly Dictionary<string, string> _values = new(StringComparer.Ordinal);
        public List<string> Keys { get; } = [];

        public void PutIfAbsent(string key, string value)
        {
            if (_values.TryAdd(key, value)) Keys.Add(key);
        }

        public bool ContainsKey(string key) => _values.ContainsKey(key);
        public string this[string key] => _values[key];
        public string? Get(string key) => _values.GetValueOrDefault(key);
    }

    /// <summary>What the split check reads of one album part.</summary>
    private sealed record PartFacts(
        string Id,
        int Size,
        string Artist,
        string ArtistKey,
        // Every artist the album artist names, by key, as written.
        Ordered AlbumArtists,
        // Every artist the songs credit, by key, with one song of theirs.
        Ordered Credits,
        IReadOnlySet<int> Years,
        IReadOnlySet<string> Releases,
        IReadOnlySet<string> Groups,
        IReadOnlySet<string> Barcodes,
        // Labels by key, as written.
        Ordered Labels);

    private static PartFacts? PartFactsOf<T>(string id, List<T> members, IHealthFields<T> fields)
    {
        var artist = AlbumArtistOf(members, fields);
        var artistKey = SongIdentity.Key(SongIdentity.PrimaryArtist(artist));
        if (artistKey.Length == 0) return null;
        var albumArtists = new Ordered();
        var named = members.SelectMany(fields.AlbumArtists).Where(name => !string.IsNullOrWhiteSpace(name)).Distinct().ToList();
        if (named.Count == 0) named = [artist];
        foreach (var credit in named)
        foreach (var name in NamesIn(credit))
            albumArtists.PutIfAbsent(SongIdentity.Key(name), name);
        var credits = new Ordered();
        foreach (var song in members)
        foreach (var credit in fields.Artists(song))
        foreach (var name in NamesIn(credit))
            credits.PutIfAbsent(SongIdentity.Key(name), fields.Title(song));
        HashSet<string> Codes(Func<T, string?> of, Func<string, string> clean) =>
            members.Select(of).OfType<string>().Select(clean).Where(code => code.Length > 0).ToHashSet(StringComparer.Ordinal);
        var labels = new Ordered();
        foreach (var song in members)
        foreach (var label in fields.Labels(song))
            if (SongIdentity.Key(label) is { Length: > 0 } key) labels.PutIfAbsent(key, label.Trim());
        return new PartFacts(
            id, members.Count, artist, artistKey, albumArtists, credits,
            members.Select(fields.Year).Where(year => year is > 0).Select(year => year!.Value).ToHashSet(),
            Codes(fields.ReleaseId, code => code.Trim().ToLowerInvariant()),
            Codes(fields.ReleaseGroupId, code => code.Trim().ToLowerInvariant()),
            Codes(fields.Barcode, code => new string(code.Where(char.IsDigit).ToArray()).TrimStart('0')),
            labels);
    }

    /// <summary>The artists a credit names, a guest in brackets left out: both of "PARTYNEXTDOOR &amp; Drake".</summary>
    private static IEnumerable<string> NamesIn(string credit) =>
        SongIdentity.ParseArtists(credit).Names.Where(name => SongIdentity.Key(name).Length > 0);

    /// <summary>Years that do not tell two parts apart: the same year, or none on one side.</summary>
    private static bool YearsAgree(IReadOnlySet<int> a, IReadOnlySet<int> b) => a.Count == 0 || b.Count == 0 || a.Any(b.Contains);

    /// <summary>The same release by its codes, or by its label and year.</summary>
    private static SplitReason? SameRelease(PartFacts a, PartFacts b)
    {
        if (a.Releases.Any(b.Releases.Contains)) return new SplitReason(SplitBasis.SameRelease);
        if (a.Groups.Any(b.Groups.Contains)) return new SplitReason(SplitBasis.SameReleaseGroup);
        if (a.Barcodes.FirstOrDefault(b.Barcodes.Contains) is { } barcode) return new SplitReason(SplitBasis.SameBarcode, Value: barcode);
        if (a.Labels.Keys.FirstOrDefault(b.Labels.ContainsKey) is not { } label) return null;
        var common = a.Years.Where(b.Years.Contains).ToList();
        if (common.Count == 0) return null;
        return new SplitReason(SplitBasis.SameLabelAndYear, Value: $"{a.Labels[label]}, {common.Min()}");
    }

    /// <summary>One part's album artist credited on the other part: a song there by them, though
    /// that part is filed under someone else, or else both album artists naming them ("Drake &amp;
    /// Future" and "Future"). Never for a title many artists use, or one that is an artist's own name.</summary>
    private static SplitReason? SharedArtist(PartFacts a, PartFacts b, string title)
    {
        if (IsCommonTitle(title) || a.AlbumArtists.ContainsKey(title) || b.AlbumArtists.ContainsKey(title)) return null;
        SplitReason? named = null;
        foreach (var (mine, theirs) in new[] { (a, b), (b, a) })
        {
            foreach (var key in mine.AlbumArtists.Keys)
            {
                var name = mine.AlbumArtists[key];
                if (theirs.AlbumArtists.ContainsKey(key))
                {
                    // Said of the credit that names more than its own artist.
                    named ??= new SplitReason(SplitBasis.SharedArtist, Artist: name,
                        Other: mine.ArtistKey == key ? theirs.Artist : mine.Artist);
                    continue;
                }
                if (theirs.Credits.Get(key) is { } song)
                    return new SplitReason(SplitBasis.SharedArtist, Artist: name, Other: theirs.Artist, Song: song);
            }
        }
        return named;
    }

    /// <summary>Album titles many artists use, by key: sharing one says nothing about being one album.</summary>
    private static readonly HashSet<string> CommonTitles = new(StringComparer.Ordinal)
    {
        "album", "thealbum", "untitled", "unknown", "unknownalbum", "single", "singles", "thesingles", "ep", "theep",
        "live", "unplugged", "acoustic", "demo", "demos", "remixes", "theremixes", "bsides", "rarities", "hits", "thehits",
        "collection", "thecollection", "essentials", "anthology", "mixtape", "soundtrack", "originalsoundtrack", "ost",
        "christmas", "christmasalbum", "achristmasalbum", "lovesongs", "covers", "instrumentals", "deluxe", "deluxeedition",
        "remastered", "nonalbumsingle", "nonalbumsingles", "greatest", "thebest",
    };

    private static readonly string[] CommonTitleStarts =
    [
        "greatesthits", "thegreatesthits", "bestof", "thebestof", "theverybestof", "theessential", "essential",
        "liveat", "livein", "livefrom", "mtvunplugged", "unplugged",
    ];

    private static bool IsCommonTitle(string title) =>
        CommonTitles.Contains(title) || CommonTitleStarts.Any(start => title.StartsWith(start, StringComparison.Ordinal));

    /// <summary>The album artist most of an album's songs name, or their artist.</summary>
    internal static string AlbumArtistOf<T>(IReadOnlyList<T> songs, IHealthFields<T> fields)
    {
        var named = songs.Select(fields.AlbumArtist).Where(name => !string.IsNullOrWhiteSpace(name)).Select(name => name!).ToList();
        var pool = named.Count > 0
            ? named
            : songs.Select(fields.Artist).Where(name => !string.IsNullOrWhiteSpace(name)).Select(name => name!).ToList();
        // The most named; between equals, the first in plain character order.
        return pool.GroupBy(name => name, StringComparer.Ordinal)
            .OrderByDescending(group => group.Count())
            .ThenBy(group => group.Key, StringComparer.Ordinal)
            .Select(group => group.Key)
            .FirstOrDefault() ?? "";
    }

    /// <summary>What the parts disagree on, as each part says it.</summary>
    private static IReadOnlyList<AlbumDifferenceValues> DifferencesOf<T>(IReadOnlyList<AlbumPart<T>> parts, IHealthFields<T> fields)
    {
        var found = new List<AlbumDifferenceValues>();
        void Check(AlbumDifference kind, Func<IReadOnlyList<T>, string> of)
        {
            var values = parts.Select(part => of(part.Songs)).ToList();
            if (values.Distinct(StringComparer.Ordinal).Count() > 1) found.Add(new AlbumDifferenceValues(kind, values));
        }
        Check(AlbumDifference.Title, songs => fields.Album(songs[0]) ?? "");
        Check(AlbumDifference.AlbumArtist, songs => string.Join(", ", songs.Select(fields.AlbumArtist)
            .Where(name => !string.IsNullOrWhiteSpace(name)).Select(name => name!)
            .Distinct(StringComparer.Ordinal).Order(StringComparer.Ordinal)));
        Check(AlbumDifference.Year, songs => string.Join(", ", songs.Select(fields.Year).Where(year => year is > 0)
            .Select(year => year!.Value).Distinct().Order()));
        if (found.Count == 0) found.Add(new AlbumDifferenceValues(AlbumDifference.Other, []));
        return found;
    }

    // ---- missing tags ---------------------------------------------------------------------------

    /// <summary>Songs missing each tag the reader can see. A track number and an album artist
    /// matter only on an album of two or more songs; a single needs neither.</summary>
    public static IReadOnlyDictionary<HealthTag, IReadOnlyList<T>> FindMissingTags<T>(IReadOnlyList<T> songs, IHealthFields<T> fields)
    {
        var albumSizes = new Dictionary<string, int>(StringComparer.Ordinal);
        foreach (var song in songs)
            if (fields.AlbumId(song) is { } id) albumSizes[id] = albumSizes.GetValueOrDefault(id) + 1;
        bool OnAlbum(T song) => (fields.AlbumId(song) is { } id ? albumSizes.GetValueOrDefault(id) : 0) > 1;
        var found = new Dictionary<HealthTag, IReadOnlyList<T>>();
        foreach (var tag in Enum.GetValues<HealthTag>())
        {
            if (!fields.Seen.Contains(tag)) continue;
            var missing = songs.Where(song => tag switch
            {
                HealthTag.Genre => !fields.Genres(song).Any(genre => !string.IsNullOrWhiteSpace(genre)),
                HealthTag.Year => (fields.Year(song) ?? 0) <= 0,
                HealthTag.AlbumArtist => OnAlbum(song) && string.IsNullOrWhiteSpace(fields.AlbumArtist(song)),
                HealthTag.TrackNumber => OnAlbum(song) && (fields.Track(song) ?? 0) <= 0,
                _ => string.IsNullOrWhiteSpace(fields.Cover(song)),
            }).ToList();
            if (missing.Count > 0) found[tag] = AcrossAlbums(missing, fields);
        }
        return found;
    }

    /// <summary>Songs of one album in its order: disc, track, then title.</summary>
    internal static IReadOnlyList<T> InAlbumOrder<T>(IEnumerable<T> songs, IHealthFields<T> fields) =>
        songs.OrderBy(song => fields.Disc(song) ?? 0)
            .ThenBy(song => TrackOrLast(fields.Track(song)))
            .ThenBy(song => SortKey(fields.Title(song)), StringComparer.Ordinal)
            .ThenBy(song => fields.Id(song), StringComparer.Ordinal)
            .ToList();

    /// <summary>Songs from many albums: by artist, then album, then album order.</summary>
    private static IReadOnlyList<T> AcrossAlbums<T>(IEnumerable<T> songs, IHealthFields<T> fields) =>
        songs.OrderBy(song => SortKey(fields.AlbumArtist(song) ?? fields.Artist(song)), StringComparer.Ordinal)
            .ThenBy(song => SortKey(fields.Album(song)), StringComparer.Ordinal)
            .ThenBy(song => fields.AlbumId(song) ?? "", StringComparer.Ordinal)
            .ThenBy(song => fields.Disc(song) ?? 0)
            .ThenBy(song => TrackOrLast(fields.Track(song)))
            .ThenBy(song => SortKey(fields.Title(song)), StringComparer.Ordinal)
            .ThenBy(song => fields.Id(song), StringComparer.Ordinal)
            .ToList();

    /// <summary>A song with no track number goes after the numbered ones.</summary>
    private static int TrackOrLast(int? track) => track is > 0 ? track.Value : int.MaxValue;

    internal static string SortKey(string? text) => (text ?? "").Trim().ToLowerInvariant();

    /// <summary>Sets of songs joined as they are found to belong together, by index.</summary>
    private sealed class Sets(int count)
    {
        private readonly int[] _parent = Enumerable.Range(0, count).ToArray();
        private readonly int[] _sizes = Enumerable.Repeat(1, count).ToArray();

        public int Root(int i)
        {
            var at = i;
            while (_parent[at] != at)
            {
                _parent[at] = _parent[_parent[at]];
                at = _parent[at];
            }
            return at;
        }

        public bool Same(int a, int b) => Root(a) == Root(b);

        public int Size(int i) => _sizes[Root(i)];

        /// <summary>Joins the two sets; false when they were one already.</summary>
        public bool Join(int a, int b)
        {
            var x = Root(a);
            var y = Root(b);
            if (x == y) return false;
            if (_sizes[x] < _sizes[y])
            {
                _parent[x] = y;
                _sizes[y] += _sizes[x];
            }
            else
            {
                _parent[y] = x;
                _sizes[x] += _sizes[y];
            }
            return true;
        }
    }
}
