using System.Collections.Concurrent;
using Octo.Models.Domain;
using Octo.Services.Audio;
using Octo.Services.Common;

namespace Octo.Services.Tagging;

/// <summary>
/// What one album walk has settled, so every track of the walk shares the album-level fields:
/// the release the first track matched, its facts, and each track's measured loudness for the
/// album gain at the end. Two tracks of one album matched to two pressings would otherwise
/// carry two labels and two catalogue numbers, and the library server shows one album's facts
/// from whichever track it reads first.
/// </summary>
public sealed class AlbumTagContext
{
    public AlbumTagContext(string? catalogAlbumId, string albumTitle, string? albumArtist)
    {
        CatalogAlbumId = catalogAlbumId;
        AlbumTitle = albumTitle;
        AlbumArtist = albumArtist;
    }

    public string? CatalogAlbumId { get; }
    public string AlbumTitle { get; }
    public string? AlbumArtist { get; }

    /// <summary>The release the walk settled on, from its first identified track.</summary>
    public string? ReleaseId { get; private set; }
    public string? ReleaseGroupId { get; private set; }
    public string? MusicBrainzAlbumTitle { get; private set; }
    public string? Label { get; private set; }
    public string? CatalogNumber { get; private set; }
    public string? Barcode { get; private set; }
    public string? ReleaseType { get; private set; }
    public string? ReleaseStatus { get; private set; }
    public string? ReleaseCountry { get; private set; }
    public int? Year { get; private set; }
    public string? OriginalDate { get; private set; }
    public string? ReleaseDate { get; private set; }
    public int? TotalTracks { get; private set; }
    public int? TotalDiscs { get; private set; }
    public IReadOnlyList<string> AlbumArtistIds { get; private set; } = [];
    public bool Captured { get; private set; }

    /// <summary>Each placed track's path and loudness, for the album gain once the walk ends.</summary>
    public ConcurrentDictionary<string, Loudness?> Loudness { get; } = new(StringComparer.OrdinalIgnoreCase);

    /// <summary>Remember the first track's release facts, once a plan set them from a release.</summary>
    public void Capture(TagPlan plan, Song song)
    {
        if (Captured || !plan.AlbumFromCandidate || plan.Chosen is not { } chosen) return;
        if (SongIdentity.Key(song.Album) != SongIdentity.Key(AlbumTitle)) return;
        Captured = true;
        ReleaseId = song.MusicBrainzReleaseId ?? chosen.Candidate.ReleaseId;
        ReleaseGroupId = song.MusicBrainzReleaseGroupId ?? chosen.Candidate.ReleaseGroupId;
        MusicBrainzAlbumTitle = song.MusicBrainzAlbumTitle;
        Label = song.Label;
        CatalogNumber = song.CatalogNumber;
        Barcode = song.Barcode;
        ReleaseType = song.ReleaseType;
        ReleaseStatus = song.ReleaseStatus;
        ReleaseCountry = song.ReleaseCountry;
        Year = song.Year;
        OriginalDate = song.OriginalDate;
        ReleaseDate = song.ReleaseDate;
        TotalTracks = song.TotalTracks;
        TotalDiscs = song.TotalDiscs;
        AlbumArtistIds = song.MusicBrainzAlbumArtistIds.ToList();
    }

    /// <summary>
    /// Steer a later track onto the walk's release: when its best candidate is another pressing
    /// of the same group, the candidate on the settled release takes its place.
    /// </summary>
    public bool PreferSettledRelease(TagPlan plan)
    {
        if (!Captured || ReleaseId is null || plan.Chosen is not { } chosen) return false;
        if (string.Equals(chosen.Candidate.ReleaseId, ReleaseId, StringComparison.OrdinalIgnoreCase)) return false;
        if (!string.Equals(chosen.Candidate.ReleaseGroupId, ReleaseGroupId, StringComparison.OrdinalIgnoreCase)) return false;
        var settled = plan.Ranked.FirstOrDefault(s => string.Equals(s.Candidate.ReleaseId, ReleaseId, StringComparison.OrdinalIgnoreCase));
        if (settled is null) return false;
        plan.Chosen = settled;
        plan.Notes.Add($"the walk's release was kept over another pressing of the same album ({chosen.Candidate.ReleaseDate ?? "?"})");
        return true;
    }

    /// <summary>Copy the settled album-level facts onto a track of the same album.</summary>
    public void Pin(Song song)
    {
        if (!Captured || SongIdentity.Key(song.Album) != SongIdentity.Key(AlbumTitle)) return;
        song.MusicBrainzReleaseId = ReleaseId;
        song.MusicBrainzReleaseGroupId = ReleaseGroupId;
        song.MusicBrainzAlbumTitle = MusicBrainzAlbumTitle;
        song.Label = Label;
        song.CatalogNumber = CatalogNumber;
        song.Barcode = Barcode;
        song.ReleaseType = ReleaseType;
        song.ReleaseStatus = ReleaseStatus;
        song.ReleaseCountry = ReleaseCountry;
        if (Year is not null) song.Year = Year;
        song.OriginalDate = OriginalDate;
        song.ReleaseDate = ReleaseDate;
        if (TotalTracks is not null) song.TotalTracks = TotalTracks;
        if (TotalDiscs is not null) song.TotalDiscs = TotalDiscs;
        song.MusicBrainzAlbumArtistIds = AlbumArtistIds.ToList();
    }
}
