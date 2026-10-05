using System.Globalization;

namespace Octo.Services.Health;

/// <summary>
/// The words for library health, the same as the Octo app's (HealthWords.kt and FixWords.kt), so a
/// finding reads the same on the phone, the desktop and the dashboard: what each check is called,
/// what it means, and what to do about it.
/// </summary>
public static class HealthWords
{
    /// <summary>A count with its noun, "1 song" or "2,256 songs".</summary>
    public static string CountText(int count, string one, string many) =>
        count == 1 ? $"1 {one}" : $"{count.ToString("N0", CultureInfo.InvariantCulture)} {many}";

    public static string Title(HealthCheck check) => check switch
    {
        HealthCheck.Duplicates => "Songs you have twice",
        HealthCheck.SplitAlbums => "Albums split apart",
        HealthCheck.NoLength => "Songs with no length",
        HealthCheck.NoTrackNumber => "No track number",
        HealthCheck.NoAlbumArtist => "No album artist",
        HealthCheck.NoCover => "No cover art",
        HealthCheck.NoYear => "No year",
        _ => "No genre",
    };

    /// <summary>How much a check found, in its own terms.</summary>
    public static string CountLabel(HealthCheck check, int count) => check switch
    {
        HealthCheck.Duplicates => CountText(count, "song with more than one copy", "songs with more than one copy"),
        HealthCheck.SplitAlbums => CountText(count, "album", "albums"),
        _ => CountText(count, "song", "songs"),
    };

    /// <summary>What a finding means for the listener.</summary>
    public static string Meaning(HealthCheck check) => check switch
    {
        HealthCheck.Duplicates =>
            "The same recording is in your library more than once, so it shows up twice in your lists and takes up space.",
        HealthCheck.SplitAlbums =>
            "Each of these albums shows up as more than one album, because its songs' tags do not agree.",
        HealthCheck.NoLength =>
            "The server could not tell how long these songs are, which usually means the file is damaged or cut short.",
        HealthCheck.NoTrackNumber =>
            "These songs are on albums but have no track number, so their albums play out of order.",
        HealthCheck.NoAlbumArtist =>
            "Without an album artist, an album can be filed under each song's own artist and fall apart.",
        HealthCheck.NoCover => "These songs have no picture, so they show a blank square.",
        HealthCheck.NoYear => "These songs have no year, so year filters leave them out and they sort last by year.",
        _ => "These songs have no genre, so genre pages and genre filters leave them out.",
    };

    /// <summary>What to do about it by hand.</summary>
    public static string Advice(HealthCheck check) => check switch
    {
        HealthCheck.Duplicates =>
            "The first copy of each is the one to keep: it sounds best, or has the fullest tags. " +
            "Remove the others, unless a copy belongs to an album you want to keep whole.",
        HealthCheck.SplitAlbums =>
            "Give every song of the album the same album title, album artist and year with a tag editor, then let the server rescan.",
        HealthCheck.NoLength => "Play one to check. If it will not play, replace the file.",
        HealthCheck.NoCover =>
            "Add a picture to the files, or put a cover image in the album's folder, then let the server rescan.",
        _ => "Add the missing tag with a tag editor, then let the server rescan.",
    };

    /// <summary>Why a set's copies were taken for one recording.</summary>
    public static string Words(DuplicateBasis basis) => basis switch
    {
        DuplicateBasis.Tags => "Tagged as the same recording",
        _ => "Same title, artist and length",
    };

    /// <summary>Which copy to keep and why, for a set whose best copy sounds like <paramref name="best"/>.</summary>
    public static string Words(BestReason reason, string best) => reason switch
    {
        BestReason.Sound => $"Keep the first, {best}",
        BestReason.Tags => "They sound alike; the first has fuller tags",
        _ => "They sound alike",
    };

    /// <summary>One line for a set of copies: "3 copies. Same title, artist and length. Keep the first, FLAC, 24-bit, 96 kHz."</summary>
    public static string Summary<T>(DuplicateSet<T> set, IHealthFields<T> fields) =>
        $"{set.Copies.Count} copies. {Words(set.Basis)}. {Words(set.BestReason, QualityText(set.Best, fields))}.";

    /// <summary>A heading for a set of copies: the song and its artist.</summary>
    public static string Heading<T>(DuplicateSet<T> set, IHealthFields<T> fields)
    {
        var artist = fields.Artist(set.Best);
        return string.IsNullOrWhiteSpace(artist) ? fields.Title(set.Best) : $"{fields.Title(set.Best)} by {artist}";
    }

    /// <summary>A heading for a split album.</summary>
    public static string Heading<T>(SplitAlbum<T> album)
    {
        var by = string.IsNullOrWhiteSpace(album.Artist) ? album.Title.Trim() : $"{album.Title.Trim()} by {album.Artist.Trim()}";
        return $"{by}, shown as {album.Parts.Count} albums";
    }

    /// <summary>Why the parts are one album, when their album artists differ, then what they disagree on.</summary>
    public static string Summary<T>(SplitAlbum<T> album) =>
        string.Join(" ", album.Reasons.Select(Words).Concat(album.Differences.Select(Words)));

    /// <summary>Why parts by different album artists were taken for one album.</summary>
    public static string Words(SplitReason reason) => reason.Basis switch
    {
        SplitBasis.SharedArtist => !string.IsNullOrWhiteSpace(reason.Song)
            ? $"One album filed under two artists: the part by {reason.Other.Trim()} has {reason.Song.Trim()} by {reason.Artist.Trim()}."
            : $"One album filed under two artists: the part by {reason.Other.Trim()} names {reason.Artist.Trim()} too.",
        SplitBasis.SameRelease => "The parts are tagged as the same MusicBrainz release.",
        SplitBasis.SameReleaseGroup => "The parts are tagged as the same MusicBrainz release group.",
        SplitBasis.SameBarcode => $"The parts carry the same barcode, {reason.Value}.",
        _ => $"The parts came out on the same label in the same year: {reason.Value}.",
    };

    public static string Words(AlbumDifferenceValues difference)
    {
        var listed = string.Join(", ", difference.Values.Select(value => value.Trim() is { Length: > 0 } text ? text : "none"));
        // A space before or after the words is there but cannot be seen.
        var spaceOnly = difference.Values.Select(value => value.Trim()).Distinct(StringComparer.Ordinal).Count() == 1;
        if (spaceOnly && difference.Kind == AlbumDifference.Title) return "The album title has a stray space on some songs.";
        if (spaceOnly && difference.Kind == AlbumDifference.AlbumArtist) return "The album artist has a stray space on some songs.";
        return difference.Kind switch
        {
            AlbumDifference.Title => $"The album title is written differently: {listed}.",
            AlbumDifference.AlbumArtist => $"The album artist differs: {listed}.",
            AlbumDifference.Year => $"The year differs: {listed}.",
            _ => "Something else in the tags differs, often a release date or an album id on only some songs.",
        };
    }

    /// <summary>How a copy sounds, in a few words: "FLAC, 24-bit, 96 kHz" or "MP3, 320 kbps". Only what the server says.</summary>
    public static string QualityText<T>(T song, IHealthFields<T> fields) =>
        QualityText(fields.Format(song), fields.Lossless(song), fields.BitDepth(song), fields.SampleRate(song), fields.BitRate(song));

    public static string QualityText(string? format, bool lossless, int? bitDepth, int? sampleRate, int? bitRate)
    {
        var parts = new List<string>();
        if (!string.IsNullOrWhiteSpace(format)) parts.Add(format.ToUpperInvariant());
        if (lossless)
        {
            if (bitDepth is > 0) parts.Add($"{bitDepth}-bit");
            if (sampleRate is > 0) parts.Add(RateText(sampleRate.Value));
        }
        else if (bitRate is > 0) parts.Add($"{bitRate} kbps");
        return parts.Count == 0 ? "unknown quality" : string.Join(", ", parts);
    }

    /// <summary>44100 as "44.1 kHz", 96000 as "96 kHz".</summary>
    private static string RateText(int hz)
    {
        var tenths = (hz + 50) / 100;
        return tenths % 10 == 0 ? $"{tenths / 10} kHz" : $"{tenths / 10}.{tenths % 10} kHz";
    }

    /// <summary>The line under the page's title.</summary>
    public static string Overview<T>(HealthReport<T> report)
    {
        var checkedText = $"Checked {CountText(report.Checked, "song", "songs")}.";
        if (report.Clean) return checkedText;
        var kinds = report.Findings.Count;
        return $"{checkedText} {(kinds == 1 ? "One thing" : $"{kinds} things")} could be better.";
    }

    /// <summary>What the page says when there is nothing to fix.</summary>
    public const string AllClear = "No second copies, no split albums and no missing tags. There is nothing to fix.";

    // ---- fixes (FixWords.kt) ---------------------------------------------------------------------

    public const string RecentlyRemoved = "Recently removed";
    public const string PutBack = "Put back";
    public const string LookUpTags = "Look up tags";

    /// <summary>What a fix button says for a check, for <paramref name="count"/> items.</summary>
    public static string FixAllLabel(HealthCheck check, int count) => check switch
    {
        HealthCheck.Duplicates => $"Fix {CountText(count, "song", "songs")}",
        HealthCheck.SplitAlbums => $"Join {CountText(count, "album", "albums")}",
        HealthCheck.NoCover => $"Find {CountText(count, "cover", "covers")}",
        HealthCheck.NoLength => $"Find higher quality for {CountText(count, "song", "songs")}",
        HealthCheck.NoTrackNumber => $"Look up {CountText(count, "song", "songs")}",
        _ => $"Fill in {CountText(count, "song", "songs")} from their albums",
    };

    /// <summary>How a fix for a check works, in a sentence, for the preview.</summary>
    public static string FixMeaning(HealthCheck check) => check switch
    {
        HealthCheck.Duplicates =>
            "Keeps the best copy of each song, fills in the tags it lacks from the others, and moves the others to the server's trash, where they can be put back.",
        HealthCheck.SplitAlbums =>
            "Gives the songs of the other parts the album tags of the largest part, album artist and all, so each album shows as one. " +
            "When one part's album artist already names every part's artist, that part leads instead.",
        HealthCheck.NoCover =>
            "Looks for each album's cover and puts it inside songs that have no picture. A picture already there is never replaced.",
        HealthCheck.NoLength =>
            "Asks the server for a higher quality copy of each song, which takes the damaged file's place once it passes the server's checks.",
        HealthCheck.NoTrackNumber =>
            "Looks each song up the way a download is tagged, and shows what it found before anything is written.",
        _ =>
            "Fills in the tag where the rest of the album agrees on it. The others can be looked up the way a download is tagged, and every change is shown before it is written.",
    };
}
