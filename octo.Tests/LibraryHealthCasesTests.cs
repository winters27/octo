using System.Text.Json;
using Octo.Services.Health;

namespace Octo.Tests;

/// <summary>
/// Every case in docs/library-health-cases.json: the Octo app's Library health tests written as
/// data, so the server's port and the app are checked against the same songs and the same answers.
/// The songs are Subsonic songs, read here the way the app's SubsonicHealth reads them.
/// </summary>
public class LibraryHealthCasesTests
{
    private static readonly JsonElement Cases = Load();

    private static JsonElement Load()
    {
        var path = Path.Combine(AppContext.BaseDirectory, "library-health-cases.json");
        using var document = JsonDocument.Parse(File.ReadAllText(path));
        return document.RootElement.Clone();
    }

    public static IEnumerable<object[]> All() =>
        Cases.GetProperty("cases").EnumerateArray().Select((item, index) => new object[] { index, item.GetProperty("name").GetString()! });

    [Fact]
    public void EveryAppTestIsACase() => Assert.True(Cases.GetProperty("cases").GetArrayLength() >= 55);

    /// <summary>A song as the app's Song type holds it, from a Subsonic answer.</summary>
    internal sealed record CaseSong(string Id, string Title, string? Artist, string? Album, string? AlbumId,
        string? DisplayAlbumArtist, IReadOnlyList<string> AlbumArtistNames, IReadOnlyList<string> ArtistNames,
        string? Genre, IReadOnlyList<string> GenreNames, int? Year, int? Disc, int? Track, int Duration, string? Suffix,
        int? BitRate, int? BitDepth, int? SamplingRate, IReadOnlyList<string> Isrc, string? MusicBrainzId, string? CoverArt);

    internal static CaseSong Song(JsonElement e)
    {
        static string? Str(JsonElement e, string name) =>
            e.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() : null;
        static int? Int(JsonElement e, string name) =>
            e.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.Number ? v.GetInt32() : null;
        static IReadOnlyList<string> Names(JsonElement e, string name) =>
            e.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.Array
                ? v.EnumerateArray().Select(item => item.ValueKind == JsonValueKind.String ? item.GetString()! : Str(item, "name") ?? "").ToList()
                : [];
        return new CaseSong(Str(e, "id")!, Str(e, "title") ?? "", Str(e, "artist"), Str(e, "album"), Str(e, "albumId"),
            Str(e, "displayAlbumArtist"), Names(e, "albumArtists"), Names(e, "artists"), Str(e, "genre"), Names(e, "genres"),
            Int(e, "year"), Int(e, "discNumber"), Int(e, "track"), Int(e, "duration") ?? 0, Str(e, "suffix"), Int(e, "bitRate"),
            Int(e, "bitDepth"), Int(e, "samplingRate"), Names(e, "isrc"), Str(e, "musicBrainzId"), Str(e, "coverArt"));
    }

    /// <summary>The app's SubsonicHealth and SubsonicAlbumHealth, with a case's facts and places.</summary>
    internal sealed class SubsonicFields(JsonElement @case) : IHealthFields<CaseSong>
    {
        private Dictionary<string, string> Facts(string kind) =>
            @case.TryGetProperty("facts", out var facts) && facts.TryGetProperty(kind, out var map)
                ? map.EnumerateObject().ToDictionary(p => p.Name, p => p.Value.GetString()!)
                : [];

        private JsonElement? AlbumOf(CaseSong song) =>
            @case.TryGetProperty("albums", out var albums)
                ? albums.EnumerateArray().Cast<JsonElement?>().FirstOrDefault(album => album!.Value.GetProperty("id").GetString() == song.AlbumId)
                : null;

        private static bool HasFacts(JsonElement @case) => @case.TryGetProperty("facts", out _);

        public string Id(CaseSong song) => song.Id;
        public string Title(CaseSong song) => song.Title;
        public string? Artist(CaseSong song) => song.Artist;
        public string? Album(CaseSong song) => song.Album;
        public string? AlbumId(CaseSong song) => song.AlbumId;
        public string? AlbumArtist(CaseSong song) =>
            !string.IsNullOrWhiteSpace(song.DisplayAlbumArtist) ? song.DisplayAlbumArtist : song.AlbumArtistNames.FirstOrDefault();
        public IReadOnlyList<string> Genres(CaseSong song) =>
            song.GenreNames.Count > 0 ? song.GenreNames : !string.IsNullOrWhiteSpace(song.Genre) ? [song.Genre] : [];
        public int? Year(CaseSong song) => song.Year;
        public int? Disc(CaseSong song) => song.Disc;
        public int? Track(CaseSong song) => song.Track;
        public int Seconds(CaseSong song) => song.Duration;
        public string? Format(CaseSong song) => string.IsNullOrWhiteSpace(song.Suffix) ? null : song.Suffix.ToLowerInvariant();
        public bool Lossless(CaseSong song) => ServerSongFields.IsLossless(song.Suffix, song.BitDepth);
        public int? BitRate(CaseSong song) => song.BitRate;
        public int? BitDepth(CaseSong song) => song.BitDepth;
        public int? SampleRate(CaseSong song) => song.SamplingRate;
        public IReadOnlyList<string> Isrcs(CaseSong song) => song.Isrc;
        public string? RecordingId(CaseSong song) => song.MusicBrainzId;
        public string? Cover(CaseSong song) => song.CoverArt;

        public IReadOnlyList<string> Artists(CaseSong song)
        {
            var named = song.ArtistNames.Where(name => !string.IsNullOrWhiteSpace(name)).ToList();
            return named.Count > 0 ? named : Artist(song) is { } one ? [one] : [];
        }

        public IReadOnlyList<string> AlbumArtists(CaseSong song)
        {
            var named = song.AlbumArtistNames.Where(name => !string.IsNullOrWhiteSpace(name)).ToList();
            return named.Count > 0 ? named : AlbumArtist(song) is { } one ? [one] : [];
        }

        public string? ReleaseId(CaseSong song) => HasFacts(@case)
            ? Facts("releases").GetValueOrDefault(song.AlbumId ?? "")
            : AlbumOf(song) is { } album && album.TryGetProperty("musicBrainzId", out var id) && id.GetString() is { Length: > 0 } text ? text : null;

        public string? ReleaseGroupId(CaseSong song) => Facts("releaseGroups").GetValueOrDefault(song.AlbumId ?? "");
        public string? Barcode(CaseSong song) => Facts("barcodes").GetValueOrDefault(song.AlbumId ?? "");

        public IReadOnlyList<string> Labels(CaseSong song)
        {
            if (HasFacts(@case)) return Facts("labels").TryGetValue(song.AlbumId ?? "", out var label) ? [label] : [];
            return AlbumOf(song) is { } album && album.TryGetProperty("recordLabels", out var labels)
                ? labels.EnumerateArray().Select(item => item.GetProperty("name").GetString()!).ToList()
                : [];
        }

        public string Place(CaseSong song) =>
            @case.TryGetProperty("places", out var places) && places.TryGetProperty(song.Id, out var place) ? place.GetString()! : "";

        public IReadOnlySet<HealthTag> Seen => @case.TryGetProperty("seen", out var seen)
            ? seen.EnumerateArray().Select(tag => Enum.Parse<HealthTag>(tag.GetString()!)).ToHashSet()
            : LibraryHealth.AllTags;
    }

    private static List<string> Strings(JsonElement array) => array.EnumerateArray().Select(item => item.GetString()!).ToList();

    [Theory]
    [MemberData(nameof(All))]
    public void Case(int index, string name)
    {
        var @case = Cases.GetProperty("cases")[index];
        Assert.Equal(name, @case.GetProperty("name").GetString());
        var songs = @case.GetProperty("songs").EnumerateArray().Select(Song).ToList();
        var fields = new SubsonicFields(@case);
        var report = LibraryHealth.Check(songs, fields);
        var expect = @case.GetProperty("expect");

        if (expect.TryGetProperty("duplicates", out var duplicates))
        {
            Assert.Equal(duplicates.GetArrayLength(), report.Duplicates.Count);
            for (var i = 0; i < duplicates.GetArrayLength(); i++) CheckSet(duplicates[i], report.Duplicates[i], fields);
        }
        if (expect.TryGetProperty("splitAlbums", out var splits))
        {
            Assert.Equal(splits.GetArrayLength(), report.SplitAlbums.Count);
            for (var i = 0; i < splits.GetArrayLength(); i++) CheckSplit(splits[i], report.SplitAlbums[i], fields);
        }
        if (expect.TryGetProperty("missing", out var missing))
            foreach (var tag in missing.EnumerateObject())
                Assert.Equal(Strings(tag.Value), report.Missing[Enum.Parse<HealthTag>(tag.Name)].Select(song => song.Title));
        if (expect.TryGetProperty("missingAbsent", out var absent))
            foreach (var tag in Strings(absent)) Assert.False(report.Missing.ContainsKey(Enum.Parse<HealthTag>(tag)));
        if (expect.TryGetProperty("missingKeys", out var keys))
            Assert.Equal(Strings(keys), report.Missing.Keys.Select(tag => tag.ToString()));
        if (expect.TryGetProperty("songsOf", out var songsOf))
            foreach (var check in songsOf.EnumerateObject())
                Assert.Equal(Strings(check.Value), report.Songs(Enum.Parse<HealthCheck>(check.Name)).Select(song => song.Title));
        if (expect.TryGetProperty("noLength", out var noLength))
            Assert.Equal(Strings(noLength), report.NoLength.Select(song => song.Title));
        if (expect.TryGetProperty("findings", out var findings))
            Assert.Equal(Strings(findings), report.Findings.Select(check => check.ToString()));
        if (expect.TryGetProperty("counts", out var counts))
            foreach (var count in counts.EnumerateObject())
                Assert.Equal(count.Value.GetInt32(), report.Count(Enum.Parse<HealthCheck>(count.Name)));
        if (expect.TryGetProperty("overview", out var overview))
            Assert.Equal(overview.GetString(), HealthWords.Overview(report));
        if (expect.TryGetProperty("without", out var without))
        {
            var after = report.Without(Strings(without.GetProperty("ids")).ToHashSet(), fields);
            Assert.Equal(without.GetProperty("duplicates").GetInt32(), after.Duplicates.Count);
            foreach (var check in without.GetProperty("songsOf").EnumerateObject())
                Assert.Equal(Strings(check.Value), after.Songs(Enum.Parse<HealthCheck>(check.Name)).Select(song => song.Id));
        }
        if (expect.TryGetProperty("fix", out var fix)) CheckFix(fix, Assert.Single(report.Duplicates), fields);
        if (expect.TryGetProperty("fillsFromAlbum", out var fills))
        {
            var tag = Enum.Parse<HealthTag>(fills.GetProperty("tag").GetString()!);
            var wanting = fills.TryGetProperty("missing", out var ids)
                ? songs.Where(song => Strings(ids).Contains(song.Id)).ToList()
                : report.Missing[tag];
            var found = HealthFixes.FillsFromAlbum(wanting, songs, tag, fields);
            Assert.Equal(
                fills.GetProperty("fills").EnumerateArray().Select(fill => (fill[0].GetString(), fill[1].GetString(), fill[2].GetString())),
                found.Select(fill => ((string?)fill.Song.Id, (string?)fill.Change.Tag, (string?)fill.Change.Value)));
        }
    }

    private static void CheckSet(JsonElement expect, DuplicateSet<CaseSong> set, IHealthFields<CaseSong> fields)
    {
        if (expect.TryGetProperty("copies", out var copies)) Assert.Equal(Strings(copies), set.Copies.Select(song => song.Id));
        if (expect.TryGetProperty("size", out var size)) Assert.Equal(size.GetInt32(), set.Copies.Count);
        if (expect.TryGetProperty("basis", out var basis)) Assert.Equal(basis.GetString(), set.Basis.ToString());
        if (expect.TryGetProperty("best", out var best)) Assert.Equal(best.GetString(), set.BestReason.ToString());
        if (expect.TryGetProperty("heading", out var heading)) Assert.Equal(heading.GetString(), HealthWords.Heading(set, fields));
        if (expect.TryGetProperty("summary", out var summary)) Assert.Equal(summary.GetString(), HealthWords.Summary(set, fields));
    }

    private static void CheckSplit(JsonElement expect, SplitAlbum<CaseSong> album, IHealthFields<CaseSong> fields)
    {
        if (expect.TryGetProperty("parts", out var parts)) Assert.Equal(Strings(parts), album.Parts.Select(part => part.AlbumId));
        if (expect.TryGetProperty("sizes", out var sizes))
            Assert.Equal(sizes.EnumerateArray().Select(size => size.GetInt32()), album.Parts.Select(part => part.Songs.Count));
        if (expect.TryGetProperty("artist", out var artist)) Assert.Equal(artist.GetString(), album.Artist);
        if (expect.TryGetProperty("heading", out var heading)) Assert.Equal(heading.GetString(), HealthWords.Heading(album));
        if (expect.TryGetProperty("summary", out var summary)) Assert.Equal(summary.GetString(), HealthWords.Summary(album));
        if (expect.TryGetProperty("reasons", out var reasons))
        {
            Assert.Equal(reasons.GetArrayLength(), album.Reasons.Count);
            for (var i = 0; i < reasons.GetArrayLength(); i++)
            {
                var want = reasons[i];
                var got = album.Reasons[i];
                Assert.Equal(want.GetProperty("basis").GetString(), got.Basis.ToString());
                if (want.TryGetProperty("artist", out var a)) Assert.Equal(a.GetString(), got.Artist);
                if (want.TryGetProperty("other", out var o)) Assert.Equal(o.GetString(), got.Other);
                if (want.TryGetProperty("song", out var s)) Assert.Equal(s.GetString(), got.Song);
            }
        }
        if (expect.TryGetProperty("reasonWords", out var words)) Assert.Equal(Strings(words), album.Reasons.Select(HealthWords.Words));
        if (expect.TryGetProperty("differences", out var differences))
        {
            Assert.Equal(differences.GetArrayLength(), album.Differences.Count);
            for (var i = 0; i < differences.GetArrayLength(); i++)
            {
                Assert.Equal(differences[i].GetProperty("kind").GetString(), album.Differences[i].Kind.ToString());
                Assert.Equal(Strings(differences[i].GetProperty("values")), album.Differences[i].Values);
            }
        }
        var join = HealthFixes.AlbumJoin(album);
        if (expect.TryGetProperty("joinMoving", out var moving))
            Assert.Equal(Strings(moving).Order(StringComparer.Ordinal), join.Moving.Select(song => song.Id).Order(StringComparer.Ordinal));
        if (expect.TryGetProperty("joinLeadAlbum", out var lead))
        {
            Assert.Equal(lead.GetString(), join.Lead.AlbumId);
            Assert.All(HealthFixes.Steps(join, fields), step =>
            {
                Assert.Equal(FixActions.JoinAlbum, step.Action);
                Assert.Equal(join.Lead.Id, step.Params["like"]);
            });
        }
        if (expect.TryGetProperty("joinWords", out var joinWords)) Assert.Equal(joinWords.GetString(), join.Words);
    }

    private static void CheckFix(JsonElement expect, DuplicateSet<CaseSong> set, IHealthFields<CaseSong> fields)
    {
        var pick = expect.TryGetProperty("pick", out var picked) ? set.Copies.Single(song => song.Id == picked.GetString()) : null;
        var fix = HealthFixes.DuplicateFix(set, fields, pick);
        if (expect.TryGetProperty("keep", out var keep)) Assert.Equal(keep.GetString(), fix.Keep.Id);
        if (expect.TryGetProperty("remove", out var remove)) Assert.Equal(Strings(remove), fix.Remove.Select(song => song.Id));
        if (expect.TryGetProperty("why", out var why)) Assert.Equal(why.GetString(), fix.Why);
        if (expect.TryGetProperty("fills", out var fills))
            Assert.Equal(fills.EnumerateObject().ToDictionary(p => p.Name, p => p.Value.GetString()),
                fix.Fills.ToDictionary(change => change.Tag, change => (string?)change.Value));
        if (expect.TryGetProperty("fillsFrom", out var from))
            Assert.All(fix.Fills, change =>
            {
                Assert.Equal(from.GetString(), change.From);
                Assert.Null(change.Now);
            });
        if (expect.TryGetProperty("note", out var note)) Assert.Equal(note.ValueKind == JsonValueKind.Null ? null : note.GetString(), fix.Note);
        if (expect.TryGetProperty("differs", out var differs))
        {
            Assert.Equal(differs.GetArrayLength(), fix.Differs.Count);
            for (var i = 0; i < differs.GetArrayLength(); i++)
            {
                Assert.Equal(differs[i].GetProperty("tag").GetString(), fix.Differs[i].Tag);
                Assert.Equal(differs[i].GetProperty("values").EnumerateArray().Select(pair => (pair[0].GetString(), pair[1].GetString())),
                    fix.Differs[i].Values.Select(pair => ((string?)pair.Key, (string?)pair.Value)));
            }
        }
    }
}
