using System.Text.Json;
using Octo.Services.Common;
using Octo.Services.Fingerprint;
using Octo.Services.Metadata;

namespace Octo.Services.Tagging;

/// <summary>Turns each source's answer into candidates the chooser can weigh.</summary>
public static class CandidateSources
{
    /// <summary>One candidate per recording and release the fingerprint service named at or above
    /// the threshold. A recording with no release still counts, so the recording id can be written.</summary>
    public static IReadOnlyList<ReleaseCandidate> FromFingerprint(AcoustIdLookup? lookup, double threshold)
    {
        if (lookup is not { IsOk: true }) return [];
        var candidates = new List<ReleaseCandidate>();
        foreach (var result in lookup.Results.Where(r => r.Score >= threshold))
            foreach (var recording in result.Recordings)
            {
                if (recording.Releases.Count == 0)
                {
                    candidates.Add(Base(recording, result));
                    continue;
                }
                foreach (var release in recording.Releases)
                    candidates.Add(Base(recording, result) with
                    {
                        ReleaseId = release.ReleaseId,
                        ReleaseGroupId = release.ReleaseGroupId,
                        ReleaseTitle = release.Title,
                        GroupTitle = release.GroupTitle ?? release.Title,
                        PrimaryType = release.PrimaryType,
                        SecondaryTypes = release.SecondaryTypes,
                        Country = release.Country,
                        ReleaseDate = release.Date ?? release.Year?.ToString(System.Globalization.CultureInfo.InvariantCulture),
                        TrackNumber = release.TrackNumber,
                        TrackCount = release.TrackCount,
                        DiscNumber = release.DiscNumber,
                        DiscCount = release.DiscCount,
                        AlbumArtist = release.AlbumArtist,
                        AlbumArtistIds = release.AlbumArtistIds,
                        ReleaseTrackId = release.ReleaseTrackId,
                        IsCompilation = release.IsCompilation,
                    });
            }
        return WithGroupFirstDates(candidates);
    }

    private static ReleaseCandidate Base(AcoustIdRecording recording, AcoustIdResult result) =>
        new(TagSource.Fingerprint, recording.Title, recording.ArtistCredit)
        {
            RecordingId = recording.RecordingId,
            PrimaryArtist = recording.PrimaryArtist,
            Artists = recording.Artists,
            ArtistIds = recording.Credits.Select(c => c.ArtistId).OfType<string>().ToList(),
            LengthSeconds = recording.DurationSeconds,
            Isrcs = recording.Isrcs,
            FingerprintId = result.Id,
            Sources = recording.Sources,
        };

    /// <summary>
    /// The fingerprint service gives each release its own date but not the group's first; the
    /// earliest date among a group's releases stands in for it. Only within what was returned,
    /// which is bounded, so a reissue-only answer reads as its own first.
    /// </summary>
    internal static IReadOnlyList<ReleaseCandidate> WithGroupFirstDates(List<ReleaseCandidate> candidates)
    {
        var first = candidates.Where(c => c.ReleaseGroupId is { Length: > 0 } && c.ReleaseDate is { Length: > 0 })
            .GroupBy(c => c.ReleaseGroupId!, StringComparer.OrdinalIgnoreCase)
            .ToDictionary(g => g.Key, g => g.Select(c => c.ReleaseDate!).Order(StringComparer.Ordinal).First(), StringComparer.OrdinalIgnoreCase);
        return candidates.Select(c => c.GroupFirstReleaseDate is null && c.ReleaseGroupId is { Length: > 0 } id
                && first.TryGetValue(id, out var date) ? c with { GroupFirstReleaseDate = date } : c).ToList();
    }

    /// <summary>One candidate from a catalog track, its album's kind read from the catalog's word for it.</summary>
    public static ReleaseCandidate FromCatalog(DeezerMetadataService.FullTrackMeta meta, string? requestedTitle = null)
    {
        var kind = meta.RecordType?.Trim().ToLowerInvariant();
        var compilation = string.Equals(kind, "compile", StringComparison.Ordinal)
            || BaseDownloadService.IsVariousArtists(meta.AlbumArtistName);
        return new ReleaseCandidate(TagSource.Catalog, meta.Title ?? requestedTitle ?? "", meta.ArtistName ?? "")
        {
            PrimaryArtist = meta.ArtistName,
            Artists = meta.Contributors?.ToList() ?? [],
            LengthSeconds = meta.Duration,
            Isrcs = SongIdentity.NormalizeIsrc(meta.Isrc) is { } isrc ? [isrc] : [],
            ReleaseTitle = meta.AlbumTitle,
            GroupTitle = meta.AlbumTitle,
            PrimaryType = kind switch { "album" or "compile" => "Album", "single" => "Single", "ep" => "EP", null or "" => null, _ => null },
            SecondaryTypes = compilation ? ["Compilation"] : [],
            ReleaseDate = meta.ReleaseDate ?? meta.Year?.ToString(System.Globalization.CultureInfo.InvariantCulture),
            GroupFirstReleaseDate = meta.ReleaseDate ?? meta.Year?.ToString(System.Globalization.CultureInfo.InvariantCulture),
            Barcode = meta.Barcode,
            Label = meta.Label,
            TrackNumber = meta.TrackNumber,
            TrackCount = meta.TotalTracks,
            DiscNumber = meta.DiscNumber,
            AlbumArtist = meta.AlbumArtistName,
            IsCompilation = compilation,
            CoverUrl = meta.AlbumCoverUrl,
            Genre = meta.Genre,
            Explicit = meta.ExplicitLyrics,
            ExplicitContent = meta.ExplicitContent,
            CatalogAlbumId = meta.AlbumId,
            CatalogTrackId = meta.TrackId,
        };
    }

    /// <summary>One candidate from what a peer tagged the file with. The title and artist fall
    /// back to the request's when the file names none, since the file's claim is its album.</summary>
    public static ReleaseCandidate? FromFileTags(FileFacts file, TagRequest? request = null)
    {
        if (!file.TagsAreEvidence || string.IsNullOrWhiteSpace(file.Album)) return null;
        return new ReleaseCandidate(TagSource.FileTags,
            string.IsNullOrWhiteSpace(file.Title) ? request?.Title ?? "" : file.Title,
            string.IsNullOrWhiteSpace(file.Artist) ? request?.Artist ?? "" : file.Artist)
        {
            RecordingId = file.RecordingId,
            LengthSeconds = file.DurationSeconds > 0 ? file.DurationSeconds : null,
            Isrcs = file.Isrcs,
            ReleaseId = file.ReleaseId,
            ReleaseTitle = file.Album,
            GroupTitle = file.Album,
            SecondaryTypes = file.IsCompilation ? ["Compilation"] : [],
            ReleaseDate = file.Year?.ToString(System.Globalization.CultureInfo.InvariantCulture),
            Barcode = file.Barcode,
            Label = file.Label,
            CatalogNumber = file.CatalogNumber,
            TrackNumber = file.Track,
            DiscNumber = file.Disc,
            AlbumArtist = file.AlbumArtist,
            IsCompilation = file.IsCompilation,
        };
    }

    /// <summary>One candidate per recording and release in a music database recording search.</summary>
    public static IReadOnlyList<ReleaseCandidate> FromDatabaseSearch(JsonElement root)
    {
        if (root.ValueKind != JsonValueKind.Object || !root.TryGetProperty("recordings", out var recordings)
            || recordings.ValueKind != JsonValueKind.Array) return [];
        var candidates = new List<ReleaseCandidate>();
        foreach (var recording in recordings.EnumerateArray())
            candidates.AddRange(FromDatabaseRecording(recording, null));
        return candidates;
    }

    /// <summary>One candidate per recording and release the music database lists for a code.</summary>
    public static IReadOnlyList<ReleaseCandidate> FromIsrcLookup(JsonElement root)
    {
        if (root.ValueKind != JsonValueKind.Object || !root.TryGetProperty("recordings", out var recordings)
            || recordings.ValueKind != JsonValueKind.Array) return [];
        var isrc = SongIdentity.NormalizeIsrc(ReleaseDetails.Str(root, "isrc"));
        var candidates = new List<ReleaseCandidate>();
        foreach (var recording in recordings.EnumerateArray())
            candidates.AddRange(FromDatabaseRecording(recording, isrc));
        return candidates;
    }

    private static IEnumerable<ReleaseCandidate> FromDatabaseRecording(JsonElement recording, string? knownIsrc)
    {
        if (recording.TryGetProperty("video", out var video) && video.ValueKind == JsonValueKind.True) yield break;
        var id = ReleaseDetails.Str(recording, "id");
        var title = ReleaseDetails.Str(recording, "title") ?? "";
        // A live take or a remix says so in its disambiguation, not always in its title.
        var disambiguation = ReleaseDetails.Str(recording, "disambiguation");
        var described = string.IsNullOrWhiteSpace(disambiguation) ? title : $"{title} ({disambiguation})";
        var (credit, artistIds) = ReleaseDetails.Credits(recording);
        var artists = ArtistNames(recording);
        int? length = ReleaseDetails.Int(recording, "length") is { } ms ? (int)Math.Round(ms / 1000.0) : null;
        var isrcs = ReleaseDetails.Isrcs(recording);
        if (knownIsrc is not null && !isrcs.Contains(knownIsrc)) isrcs = isrcs.Append(knownIsrc).ToList();
        var firstRelease = ReleaseDetails.Str(recording, "first-release-date");

        var basis = new ReleaseCandidate(TagSource.Database, described, credit ?? "")
        {
            RecordingId = id,
            PrimaryArtist = artists.FirstOrDefault(),
            Artists = artists,
            ArtistIds = artistIds,
            LengthSeconds = length,
            Isrcs = isrcs,
        };

        if (!recording.TryGetProperty("releases", out var releases) || releases.ValueKind != JsonValueKind.Array
            || releases.GetArrayLength() == 0)
        {
            yield return basis with { GroupFirstReleaseDate = firstRelease };
            yield break;
        }

        foreach (var release in releases.EnumerateArray().Take(25))
        {
            string? groupId = null, groupTitle = null, groupFirst = null, primaryType = null;
            var secondary = new List<string>();
            if (release.TryGetProperty("release-group", out var group) && group.ValueKind == JsonValueKind.Object)
            {
                groupId = ReleaseDetails.Str(group, "id");
                groupTitle = ReleaseDetails.Str(group, "title");
                groupFirst = ReleaseDetails.Str(group, "first-release-date");
                primaryType = ReleaseDetails.Str(group, "primary-type");
                if (group.TryGetProperty("secondary-types", out var types) && types.ValueKind == JsonValueKind.Array)
                    secondary.AddRange(types.EnumerateArray().Select(t => t.GetString()).OfType<string>());
            }
            var (albumArtist, albumArtistIds) = ReleaseDetails.Credits(release);

            int? trackNumber = null, trackCount = null, discNumber = null;
            string? trackId = null;
            if (release.TryGetProperty("media", out var media) && media.ValueKind == JsonValueKind.Array)
                foreach (var medium in media.EnumerateArray())
                {
                    if (!medium.TryGetProperty("track", out var tracks) || tracks.ValueKind != JsonValueKind.Array
                        || tracks.GetArrayLength() == 0) continue;
                    var track = tracks[0];
                    trackId = ReleaseDetails.Str(track, "id");
                    trackNumber = ReleaseDetails.Int(track, "position")
                        ?? (int.TryParse(ReleaseDetails.Str(track, "number"), out var number) ? number : null);
                    if (trackNumber is null && ReleaseDetails.Int(medium, "track-offset") is { } offset) trackNumber = offset + 1;
                    trackCount = ReleaseDetails.Int(medium, "track-count");
                    discNumber = ReleaseDetails.Int(medium, "position");
                    break;
                }

            var isCompilation = secondary.Any(t => string.Equals(t, "Compilation", StringComparison.OrdinalIgnoreCase))
                || string.Equals(albumArtist, "Various Artists", StringComparison.OrdinalIgnoreCase);
            yield return basis with
            {
                ReleaseId = ReleaseDetails.Str(release, "id"),
                ReleaseGroupId = groupId,
                ReleaseTitle = ReleaseDetails.Str(release, "title"),
                GroupTitle = groupTitle ?? ReleaseDetails.Str(release, "title"),
                PrimaryType = primaryType,
                SecondaryTypes = secondary,
                Status = ReleaseDetails.Str(release, "status"),
                Country = ReleaseDetails.Str(release, "country"),
                ReleaseDate = Blank(ReleaseDetails.Str(release, "date")),
                GroupFirstReleaseDate = Blank(groupFirst) ?? Blank(firstRelease),
                Barcode = Blank(ReleaseDetails.Str(release, "barcode")),
                TrackNumber = trackNumber,
                TrackCount = trackCount,
                DiscNumber = discNumber,
                DiscCount = release.TryGetProperty("media", out var all) && all.ValueKind == JsonValueKind.Array ? all.GetArrayLength() : null,
                AlbumArtist = albumArtist,
                AlbumArtistIds = albumArtistIds,
                ReleaseTrackId = trackId,
                IsCompilation = isCompilation,
            };
        }
    }

    private static IReadOnlyList<string> ArtistNames(JsonElement recording)
    {
        if (!recording.TryGetProperty("artist-credit", out var credit) || credit.ValueKind != JsonValueKind.Array) return [];
        return credit.EnumerateArray()
            .Select(entry => ReleaseDetails.Str(entry, "name")
                ?? (entry.TryGetProperty("artist", out var artist) ? ReleaseDetails.Str(artist, "name") : null))
            .OfType<string>().Where(name => name.Length > 0).ToList();
    }

    private static string? Blank(string? value) => string.IsNullOrWhiteSpace(value) ? null : value;
}
