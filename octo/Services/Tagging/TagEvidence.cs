namespace Octo.Services.Tagging;

/// <summary>Where a candidate came from, from the most to the least trusted.</summary>
public enum TagSource { Fingerprint, Database, Catalog, FileTags, Request }

/// <summary>What a download was asked for, captured before anything corrects it.</summary>
public sealed record TagRequest(
    string Artist, string Title, string? Album, int? Track, int? Disc, int? DurationSeconds,
    string? Isrc, string? CatalogAlbumId, string? CatalogTrackId, IReadOnlySet<string> VersionMarkers)
{
    /// <summary>The request named an album, so the album, track and disc are its to keep.</summary>
    public bool OwnsAlbum => !string.IsNullOrWhiteSpace(Album);
}

/// <summary>What landed on disk: its length and format, and the tags it arrived with.
/// Tag fields are empty for a file whose tags are not evidence (a video site upload).</summary>
public sealed record FileFacts(
    int DurationSeconds, string Extension, int SampleRate,
    string? Title, string? Artist, string? Album, string? AlbumArtist, int? Year,
    int? Track, int? Disc, IReadOnlyList<string> Isrcs, string? Barcode, string? CatalogNumber,
    string? Label, string? RecordingId, string? ReleaseId, bool IsCompilation, bool TagsAreEvidence)
{
    public static FileFacts Unknown(string extension = "") =>
        new(0, extension, 0, null, null, null, null, null, null, null, [], null, null, null, null, null, false, false);
}

/// <summary>One recording on one release, from one source. Null means the source did not say.</summary>
public sealed record ReleaseCandidate(TagSource Source, string RecordingTitle, string ArtistCredit)
{
    public string? RecordingId { get; init; }
    public string? PrimaryArtist { get; init; }
    public IReadOnlyList<string> Artists { get; init; } = [];
    public IReadOnlyList<string> ArtistIds { get; init; } = [];
    public int? LengthSeconds { get; init; }
    public IReadOnlyList<string> Isrcs { get; init; } = [];
    public string? ReleaseId { get; init; }
    public string? ReleaseGroupId { get; init; }
    public string? ReleaseTitle { get; init; }
    public string? GroupTitle { get; init; }
    public string? PrimaryType { get; init; }
    public IReadOnlyList<string> SecondaryTypes { get; init; } = [];
    public string? Status { get; init; }
    public string? Country { get; init; }
    public string? ReleaseDate { get; init; }
    public string? GroupFirstReleaseDate { get; init; }
    public string? Barcode { get; init; }
    public string? Label { get; init; }
    public string? CatalogNumber { get; init; }
    public int? TrackNumber { get; init; }
    public int? TrackCount { get; init; }
    public int? DiscNumber { get; init; }
    public int? DiscCount { get; init; }
    public string? AlbumArtist { get; init; }
    public IReadOnlyList<string> AlbumArtistIds { get; init; } = [];
    public string? ReleaseTrackId { get; init; }
    public bool IsCompilation { get; init; }
    public string? FingerprintId { get; init; }
    public int Sources { get; init; }
    public string? CoverUrl { get; init; }
    public string? Genre { get; init; }
    public bool? Explicit { get; init; }
    public int? ExplicitContent { get; init; }
    public string? CatalogAlbumId { get; init; }
    public string? CatalogTrackId { get; init; }

    /// <summary>The album this candidate files the song under: the release's own title, else its group's.</summary>
    public string? AlbumTitle => !string.IsNullOrWhiteSpace(ReleaseTitle) ? ReleaseTitle : GroupTitle;

    public int? Year => YearOf(ReleaseDate);
    public int? OriginalYear => YearOf(GroupFirstReleaseDate) ?? Year;

    internal static int? YearOf(string? date) =>
        date is { Length: >= 4 } && int.TryParse(date[..4], out var year) && year > 0 ? year : null;

    /// <summary>How the source describes the release's kind, for the report: "album", "album; compilation".</summary>
    public string? KindText => PrimaryType is null && SecondaryTypes.Count == 0 ? null
        : string.Join("; ", new[] { PrimaryType }.Concat(SecondaryTypes).Where(t => !string.IsNullOrWhiteSpace(t))
            .Select(t => t!.ToLowerInvariant()));
}

/// <summary>Everything the chooser weighs a candidate against.</summary>
public sealed record TagEvidence(TagRequest Request, FileFacts File, double FingerprintThreshold,
    IReadOnlySet<string> FingerprintedRecordingIds);
