using System.Globalization;
using Octo.Services.Common;
using Octo.Services.Library;

namespace Octo.Services.Health;

// Library health's fixes: what each finding comes to when it is put right, worked out from the
// songs alone so it can be shown before anything changes, and then done one song at a time.
// The same plans the Octo app makes (HealthFixes.kt); only the drawing differs.

/// <summary>One tag a fix would write: what the song says now and what it gets.</summary>
public sealed record TagChange(string Tag, string? Now, string Value, string? From = null)
{
    /// <summary>"Year: 1998" or, over a value, "Year: 1998, was 1997".</summary>
    public string Words => Now is null ? $"{HealthFixes.TagName(Tag)}: {Value}" : $"{HealthFixes.TagName(Tag)}: {Value}, was {Now}";
}

/// <summary>One tag two copies disagree on, each copy's value by the copy's id.</summary>
public sealed record TagChoice(string Tag, IReadOnlyList<KeyValuePair<string, string>> Values);

/// <summary>A set of copies put right: one kept, the rest removed, and the tags the kept one lacks
/// taken from the others.</summary>
public sealed record DuplicateFix<T>(
    T Keep,
    IReadOnlyList<T> Remove,
    // Why that one is kept, in plain words.
    string Why,
    // Blank tags on the kept copy another copy can fill. Done by "fix all".
    IReadOnlyList<TagChange> Fills,
    // Tags the copies say differently, for a person to pick from.
    IReadOnlyList<TagChoice> Differs,
    // Something worth knowing first, like a copy that is the only one on its album.
    string? Note);

/// <summary>A split album put right: every song of the other parts takes the album tags of the
/// first part. Where the album artists differ, the moved songs take the first part's.</summary>
public sealed record AlbumJoin<T>(SplitAlbum<T> Album, T Lead, IReadOnlyList<T> Moving)
{
    public string Words
    {
        get
        {
            var count = HealthWords.CountText(Moving.Count, "song", "songs");
            var kept = Album.Parts[0].Songs.Count;
            var named = kept < Album.Parts.Max(part => part.Songs.Count);
            var why = named ? ", since its album artist names every part's artist" : "";
            var head = $"Moves {count} onto {Album.Title.Trim()}, the part with {HealthWords.CountText(kept, "song", "songs")}{why}.";
            var artist = Album.Artist.Trim();
            var differs = Album.Reasons.Count > 0 || Album.Differences.Any(d => d.Kind == AlbumDifference.AlbumArtist);
            return differs && artist.Length > 0 ? $"{head} Their album artist becomes {artist}." : head;
        }
    }
}

/// <summary>The libraryAction names the steps use, the same as the app's.</summary>
public static class FixActions
{
    public const string Remove = "remove";
    public const string Retag = "retag";
    public const string JoinAlbum = "joinAlbum";
    public const string Cover = "cover";
    public const string Restore = "restore";
    public const string Undo = "undo";

    public static readonly string[] All = [Remove, Retag, JoinAlbum, Cover, Restore, Undo];
}

/// <summary>
/// One thing done to one song. <c>With</c> carries the tags for a retag, <c>like</c> (the lead
/// song) for a join, and <c>copy=true</c> for a remove of one copy of a song the library keeps
/// another of, so the song itself is still wanted.
/// </summary>
public sealed record FixStep(string Action, string Id, string Title, IReadOnlyDictionary<string, string>? With = null)
{
    public IReadOnlyDictionary<string, string> Params => With ?? new Dictionary<string, string>();

    /// <summary>The step that puts this one back, once it is done.</summary>
    public FixStep? UndoStep => Action switch
    {
        FixActions.Remove => new FixStep(FixActions.Restore, Id, Title),
        FixActions.Retag or FixActions.JoinAlbum or FixActions.Cover => new FixStep(FixActions.Undo, Id, Title),
        FixActions.Restore => new FixStep(FixActions.Remove, Id, Title),
        _ => null,
    };

    public static FixStep RemoveCopy(string id, string title) =>
        new(FixActions.Remove, id, title, new Dictionary<string, string> { ["copy"] = "true" });

    public static FixStep RetagWith(string id, string title, IEnumerable<TagChange> changes) =>
        new(FixActions.Retag, id, title, changes.GroupBy(change => change.Tag).ToDictionary(group => group.Key, group => group.Last().Value));

    public static FixStep Join(string id, string title, string like) =>
        new(FixActions.JoinAlbum, id, title, new Dictionary<string, string> { ["like"] = like });

    public bool Equals(FixStep? other) =>
        other is not null && Action == other.Action && Id == other.Id && Title == other.Title
        && Params.Count == other.Params.Count && Params.All(pair => other.Params.TryGetValue(pair.Key, out var value) && value == pair.Value);

    public override int GetHashCode() => HashCode.Combine(Action, Id, Title);
}

/// <summary>How one step went: applied, rehearsed, skipped, failed, unresolved.</summary>
public sealed record FixAnswer(string State, string? Detail);

/// <summary>How a run of steps went.</summary>
public sealed record FixOutcome(
    IReadOnlyList<FixStep> Done,
    // Steps that changed nothing because nothing needed changing.
    IReadOnlyList<FixStep> Unchanged,
    // Steps that could not be done, with the server's words.
    IReadOnlyList<(FixStep Step, string Why)> Failed,
    // Only rehearsed: library actions are in dry run.
    bool Rehearsed,
    // Stopped before the end.
    bool Stopped)
{
    public IReadOnlySet<string> Removed => Done.Where(step => step.Action == FixActions.Remove).Select(step => step.Id).ToHashSet();

    /// <summary>What puts every done step back, the last first.</summary>
    public IReadOnlyList<FixStep> UndoSteps => Done.Reverse().Select(step => step.UndoStep).OfType<FixStep>().ToList();

    /// <summary>One line: what was done, and what was not.</summary>
    public string Summary()
    {
        if (Rehearsed) return "The server only rehearsed this, because its library actions are in dry run. Nothing changed.";
        var parts = new List<string>();
        int Count(string action) => Done.Count(step => step.Action == action);
        var removes = Count(FixActions.Remove);
        var retags = Count(FixActions.Retag);
        var joins = Count(FixActions.JoinAlbum);
        var covers = Count(FixActions.Cover);
        var restores = Count(FixActions.Restore);
        var undos = Count(FixActions.Undo);
        if (removes > 0) parts.Add($"removed {HealthWords.CountText(removes, "song", "songs")}");
        if (retags > 0) parts.Add($"changed the tags of {HealthWords.CountText(retags, "song", "songs")}");
        if (joins > 0) parts.Add($"moved {HealthWords.CountText(joins, "song", "songs")} onto their album");
        if (covers > 0) parts.Add($"added {HealthWords.CountText(covers, "cover", "covers")}");
        if (restores > 0) parts.Add($"put back {HealthWords.CountText(restores, "song", "songs")}");
        if (undos > 0) parts.Add($"undid the changes to {HealthWords.CountText(undos, "song", "songs")}");
        string head;
        if (parts.Count == 0) head = Unchanged.Count > 0 ? "Nothing needed changing." : "Nothing was done.";
        else
        {
            var joined = string.Join(", ", parts);
            head = char.ToUpperInvariant(joined[0]) + joined[1..] + ".";
        }
        var stop = Stopped ? " Stopped before the end." : "";
        var miss = Failed.Count switch
        {
            0 => "",
            1 => $" {Failed[0].Step.Title}: {Failed[0].Why}",
            _ => $" {HealthWords.CountText(Failed.Count, "song", "songs")} could not be done, like {Failed[0].Step.Title}: {Failed[0].Why}",
        };
        return head + stop + miss;
    }
}

public static class HealthFixes
{
    /// <summary>The tags filled first, in this order.</summary>
    private static readonly string[] FillOrder =
    [
        SongTagFields.Album, SongTagFields.AlbumArtist, SongTagFields.Track, SongTagFields.Disc,
        SongTagFields.Year, SongTagFields.Genre, SongTagFields.Isrc,
    ];

    /// <summary>Tags that belong to an album: taken only from a copy on the same album, or with the
    /// album itself when the kept copy has none.</summary>
    private static readonly HashSet<string> AlbumTags =
        [SongTagFields.Album, SongTagFields.AlbumArtist, SongTagFields.Track, SongTagFields.Disc, SongTagFields.Year];

    /// <summary>A song's tags as the server names them, for the ones the reader can see. Blank is null.</summary>
    public static IReadOnlyDictionary<string, string?> TagValues<T>(T song, IHealthFields<T> fields)
    {
        static string? Text(string? value) => string.IsNullOrWhiteSpace(value) ? null : value;
        static string? Number(int? value) => value is > 0 ? value.Value.ToString(CultureInfo.InvariantCulture) : null;
        return new Dictionary<string, string?>
        {
            [SongTagFields.Title] = Text(fields.Title(song)),
            [SongTagFields.Artist] = Text(fields.Artist(song)),
            [SongTagFields.Album] = Text(fields.Album(song)),
            [SongTagFields.AlbumArtist] = fields.Seen.Contains(HealthTag.AlbumArtist) ? Text(fields.AlbumArtist(song)) : null,
            [SongTagFields.Year] = Number(fields.Year(song)),
            [SongTagFields.Genre] = Text(string.Join("; ", fields.Genres(song).Where(genre => !string.IsNullOrWhiteSpace(genre)))),
            [SongTagFields.Track] = Number(fields.Track(song)),
            [SongTagFields.Disc] = Number(fields.Disc(song)),
            [SongTagFields.Isrc] = Text(fields.Isrcs(song).FirstOrDefault()),
        };
    }

    /// <summary>The plan for one set of copies, keeping <paramref name="keep"/> (the best copy unless
    /// a person picked another).</summary>
    public static DuplicateFix<T> DuplicateFix<T>(DuplicateSet<T> set, IHealthFields<T> fields, T? keep = default)
    {
        keep ??= set.Best;
        var keepId = fields.Id(keep);
        var others = set.Copies.Where(copy => fields.Id(copy) != keepId).ToList();
        var mine = TagValues(keep, fields);
        // The fullest tagged donors first.
        var donors = others.OrderByDescending(copy => LibraryHealth.TagsOf(copy, fields)).ToList();
        var myAlbum = SongIdentity.Key(mine[SongTagFields.Album]);
        var fills = new List<TagChange>();
        // With no album, the album comes from one donor whole: its title, album artist, track and disc together.
        var albumDonor = myAlbum.Length == 0
            ? donors.FirstOrDefault(donor => TagValues(donor, fields)[SongTagFields.Album] is not null)
            : default;
        var hasAlbumDonor = myAlbum.Length == 0 && donors.Any(donor => TagValues(donor, fields)[SongTagFields.Album] is not null);
        foreach (var tag in FillOrder)
        {
            if (mine[tag] is not null) continue;
            var found = false;
            T? from = default;
            foreach (var donor in donors)
            {
                var theirs = TagValues(donor, fields);
                if (theirs[tag] is null) continue;
                if (!AlbumTags.Contains(tag)
                    || (hasAlbumDonor && fields.Id(donor) == fields.Id(albumDonor!))
                    || (myAlbum.Length > 0 && SongIdentity.Key(theirs[SongTagFields.Album]) == myAlbum))
                {
                    from = donor;
                    found = true;
                    break;
                }
            }
            if (!found) continue;
            fills.Add(new TagChange(tag, null, TagValues(from!, fields)[tag]!, fields.Id(from!)));
        }
        var differs = new List<TagChoice>();
        foreach (var tag in new[] { SongTagFields.Title, SongTagFields.Artist, SongTagFields.Album, SongTagFields.AlbumArtist, SongTagFields.Year, SongTagFields.Genre })
        {
            var values = new List<KeyValuePair<string, string>>();
            if (mine[tag] is { } value) values.Add(new(keepId, value));
            foreach (var other in others)
                if (TagValues(other, fields)[tag] is { } theirs) values.Add(new(fields.Id(other), theirs));
            if (mine[tag] is not null && values.Select(pair => pair.Value).Distinct(StringComparer.Ordinal).Count() > 1)
                differs.Add(new TagChoice(tag, values));
        }
        // A copy that is the only song of its album here takes the album with it.
        var lonely = others.FirstOrDefault(other =>
        {
            var album = SongIdentity.Key(fields.Album(other));
            return album.Length > 0 && album != myAlbum;
        });
        var note = lonely is null
            ? null
            : $"{CopyName(lonely, fields)} is on {fields.Album(lonely)!.Trim()}, so that album will no longer have this song.";
        return new DuplicateFix<T>(keep, others, KeepWhy(set, keep, fields), fills, differs, note);
    }

    /// <summary>Why <paramref name="keep"/> is the one kept, in words.</summary>
    public static string KeepWhy<T>(DuplicateSet<T> set, T keep, IHealthFields<T> fields)
    {
        var quality = HealthWords.QualityText(keep, fields);
        if (fields.Id(keep) != fields.Id(set.Best)) return $"You picked this copy ({quality}) to keep.";
        return set.BestReason switch
        {
            BestReason.Sound => $"Keeps the {quality} copy, because it sounds best.",
            BestReason.Tags => $"The copies sound alike, so this keeps the one with the fullest tags ({quality}).",
            _ => $"The copies sound alike and have the same tags, so this keeps the first ({quality}).",
        };
    }

    /// <summary>"Holocene (MP3, 320 kbps)".</summary>
    public static string CopyName<T>(T song, IHealthFields<T> fields) => $"{fields.Title(song)} ({HealthWords.QualityText(song, fields)})";

    /// <summary>A tag's name for people.</summary>
    public static string TagName(string tag) => tag switch
    {
        SongTagFields.Title => "Title",
        SongTagFields.Artist => "Artist",
        SongTagFields.Album => "Album",
        SongTagFields.AlbumArtist => "Album artist",
        SongTagFields.Year => "Year",
        SongTagFields.Genre => "Genre",
        SongTagFields.Track => "Track number",
        SongTagFields.Disc => "Disc number",
        SongTagFields.Isrc => "ISRC",
        _ => tag.Length == 0 ? tag : char.ToUpperInvariant(tag[0]) + tag[1..],
    };

    /// <summary>The steps that fix a set of copies: fill the kept one's blank tags, then remove the others.</summary>
    public static IReadOnlyList<FixStep> Steps<T>(DuplicateFix<T> fix, IHealthFields<T> fields, IReadOnlyList<TagChange>? fills = null)
    {
        fills ??= fix.Fills;
        var steps = new List<FixStep>();
        if (fills.Count > 0) steps.Add(FixStep.RetagWith(fields.Id(fix.Keep), fields.Title(fix.Keep), fills));
        steps.AddRange(fix.Remove.Select(copy => FixStep.RemoveCopy(fields.Id(copy), fields.Title(copy))));
        return steps;
    }

    public static AlbumJoin<T> AlbumJoin<T>(SplitAlbum<T> album) =>
        new(album, album.Parts[0].Songs[0], album.Parts.Skip(1).SelectMany(part => part.Songs).ToList());

    public static IReadOnlyList<FixStep> Steps<T>(AlbumJoin<T> join, IHealthFields<T> fields) =>
        join.Moving.Select(song => FixStep.Join(fields.Id(song), fields.Title(song), fields.Id(join.Lead))).ToList();

    /// <summary>
    /// A tag a song lacks that the rest of its album agrees on, so it can be filled in without
    /// looking anything up: the year or genre every other song of the album has, or an album artist
    /// from the album or, when every song is by one artist, that artist.
    /// </summary>
    public static IReadOnlyList<(T Song, TagChange Change)> FillsFromAlbum<T>(IReadOnlyList<T> missing, IReadOnlyList<T> all,
        HealthTag tag, IHealthFields<T> fields)
    {
        var name = tag switch
        {
            HealthTag.Year => SongTagFields.Year,
            HealthTag.Genre => SongTagFields.Genre,
            HealthTag.AlbumArtist => SongTagFields.AlbumArtist,
            _ => null,
        };
        if (name is null) return [];
        var byAlbum = all.GroupBy(song => fields.AlbumId(song) ?? "", StringComparer.Ordinal)
            .ToDictionary(group => group.Key, group => group.ToList(), StringComparer.Ordinal);
        var found = new List<(T, TagChange)>();
        foreach (var song in missing)
        {
            if (fields.AlbumId(song) is not { Length: > 0 } albumId) continue;
            var id = fields.Id(song);
            var siblings = (byAlbum.GetValueOrDefault(albumId) ?? []).Where(other => fields.Id(other) != id).ToList();
            var values = siblings.Select(other => TagValues(other, fields)[name]).OfType<string>().Distinct(StringComparer.Ordinal).ToList();
            var value = values.Count == 1 ? values[0] : null;
            if (value is null && tag == HealthTag.AlbumArtist && values.Count == 0)
            {
                var artists = siblings.Append(song).Select(fields.Artist).Where(artist => !string.IsNullOrWhiteSpace(artist))
                    .Distinct(StringComparer.Ordinal).ToList();
                value = artists.Count == 1 ? artists[0] : null;
            }
            if (value is not null) found.Add((song, new TagChange(name, null, value)));
        }
        return found;
    }

    /// <summary>
    /// What a lookup would change: each tag found that the file says differently. A blank one is
    /// picked to be filled; one the file has is picked only when the match is sure and the tag is
    /// not the song's name.
    /// </summary>
    public static IReadOnlyList<(TagChange Change, bool Pick)> Changes(SongLookup lookup)
    {
        var sure = string.Equals(lookup.Confidence, "Strong", StringComparison.OrdinalIgnoreCase)
                   || string.Equals(lookup.Confidence, "Medium", StringComparison.OrdinalIgnoreCase);
        var found = new List<(TagChange, bool)>();
        foreach (var tag in SongTagFields.All)
        {
            if (lookup.Suggested.GetValueOrDefault(tag) is not { } value || string.IsNullOrWhiteSpace(value)) continue;
            var now = lookup.Current.GetValueOrDefault(tag) is { } current && !string.IsNullOrWhiteSpace(current) ? current : null;
            if (now is not null && now.Trim() == value.Trim()) continue;
            var named = tag is SongTagFields.Title or SongTagFields.Artist or SongTagFields.Album;
            found.Add((new TagChange(tag, now, value, lookup.Source), now is null || (sure && !named)));
        }
        return found;
    }

    /// <summary>Where a lookup came from: "A sure match from the song's fingerprint, on 'Mezzanine' 1998."</summary>
    public static string Origin(SongLookup lookup)
    {
        var how = lookup.Confidence?.ToLowerInvariant() switch
        {
            "strong" => "A sure match",
            "medium" => "A likely match",
            null => "Found",
            _ => "A doubtful match",
        };
        var from = lookup.Source is { } source ? $" from {SourceName(source)}" : "";
        var on = !string.IsNullOrWhiteSpace(lookup.Release) ? $", on {lookup.Release}" : "";
        return $"{how}{from}{on}.";
    }

    private static string SourceName(string source) => source.ToLowerInvariant() switch
    {
        "fingerprint" => "the song's fingerprint",
        "database" => "MusicBrainz",
        "catalog" => "the catalog",
        "filetags" => "the file's own tags",
        _ => source,
    };

    private static readonly string[] NothingToDo = ["nothing to change", "already", "picture of its own", "has a picture"];

    /// <summary>Skipped with nothing to change is not a failure: the song already says it, or the cover is there.</summary>
    public static bool IsNothingToDo(string? detail) =>
        detail is not null && NothingToDo.Any(words => detail.Contains(words, StringComparison.OrdinalIgnoreCase));
}
