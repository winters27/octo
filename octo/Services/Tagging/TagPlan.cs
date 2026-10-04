using System.Globalization;
using Octo.Models.Domain;
using Octo.Services.Common;

namespace Octo.Services.Tagging;

public enum TagConfidence { None, Low, Ambiguous, Medium, Strong }

/// <summary>One field's value and where it came from, for the report.</summary>
public sealed record FieldDecision(string? Value, string? Source);

public sealed record ScoredCandidate(ReleaseCandidate Candidate, double Distance,
    IReadOnlyList<(string Key, double Penalty, double Weight)> Breakdown)
{
    public double? PenaltyOf(string key) =>
        Breakdown.Where(b => b.Key == key).Select(b => (double?)b.Penalty).FirstOrDefault();
}

/// <summary>
/// What the chooser decided for one download: the release that won, how sure it is, every
/// candidate it weighed, and what each field is set to and from. Applied to the Song before the
/// file is placed; kept as a report in the fetched-songs log.
/// </summary>
public sealed class TagPlan
{
    public TagConfidence Confidence { get; init; }
    public ScoredCandidate? Chosen { get; set; }
    public IReadOnlyList<ScoredCandidate> Ranked { get; init; } = [];
    public TagEvidence? Evidence { get; init; }
    public MatchingSettings Settings { get; init; } = MatchingSettings.Default;
    public ReleaseDetails? Details { get; private set; }

    /// <summary>The catalog's best hit, for the fill-the-blanks rules that run after the plan.</summary>
    public Metadata.DeezerMetadataService.FullTrackMeta? CatalogBest { get; set; }

    public bool Rehearsed { get; set; }
    public bool DetailsPrefetchHit { get; set; }
    public Dictionary<string, FieldDecision> Fields { get; } = new();
    public Dictionary<string, double> StageSeconds { get; } = new();
    public List<string> Notes { get; } = new();
    public double? IntegratedLufs { get; set; }
    public double? TruePeakDbfs { get; set; }

    public static TagPlan Empty(TagEvidence? evidence, MatchingSettings settings) =>
        new() { Confidence = TagConfidence.None, Evidence = evidence, Settings = settings };

    /// <summary>Whether the chosen release may set the album-level fields: a Strong match of any
    /// source, or a Medium one backed by the fingerprint service or the music database.</summary>
    public bool AlbumFromCandidate => Chosen is { } chosen && (Confidence == TagConfidence.Strong
        || (Confidence == TagConfidence.Medium && chosen.Candidate.Source is TagSource.Fingerprint or TagSource.Database));

    /// <summary>Whether the recording is settled, so its ids may be written: the fingerprint named
    /// it, or the match is Strong.</summary>
    public bool RecordingConfirmed => Chosen is { Candidate.RecordingId.Length: > 0 } chosen
        && (Confidence == TagConfidence.Strong || FingerprintBacked(chosen.Candidate));

    public bool FingerprintBacked(ReleaseCandidate candidate) =>
        candidate.RecordingId is { Length: > 0 } id && Evidence?.FingerprintedRecordingIds.Contains(id) == true;

    /// <summary>Add what the release lookup said to the chosen candidate.</summary>
    public void With(ReleaseDetails details)
    {
        if (Chosen is not { } chosen) return;
        Details = details;
        var c = chosen.Candidate;
        var track = details.TrackFor(c.RecordingId);
        Chosen = chosen with
        {
            Candidate = c with
            {
                ReleaseTitle = c.ReleaseTitle ?? details.Title,
                GroupTitle = c.GroupTitle ?? details.GroupTitle,
                ReleaseGroupId = c.ReleaseGroupId ?? details.GroupId,
                Status = details.Status ?? c.Status,
                Country = details.Country ?? c.Country,
                ReleaseDate = details.Date ?? c.ReleaseDate,
                GroupFirstReleaseDate = details.GroupFirstReleaseDate ?? c.GroupFirstReleaseDate,
                Barcode = details.Barcode ?? c.Barcode,
                Label = details.Label ?? c.Label,
                CatalogNumber = details.CatalogNumber ?? c.CatalogNumber,
                PrimaryType = c.PrimaryType ?? details.PrimaryType,
                SecondaryTypes = c.SecondaryTypes.Count > 0 ? c.SecondaryTypes : details.SecondaryTypes,
                AlbumArtist = c.AlbumArtist ?? details.AlbumArtist,
                AlbumArtistIds = c.AlbumArtistIds.Count > 0 ? c.AlbumArtistIds : details.AlbumArtistIds,
                DiscCount = c.DiscCount ?? (details.DiscCount > 0 ? details.DiscCount : null),
                IsCompilation = c.IsCompilation || details.IsCompilation,
                ReleaseTrackId = c.ReleaseTrackId ?? track?.ReleaseTrackId,
                TrackNumber = c.TrackNumber ?? track?.Position,
                DiscNumber = c.DiscNumber ?? (track is null ? null : track.DiscNumber),
                TrackCount = c.TrackCount ?? (track is { TrackCount: > 0 } t ? t.TrackCount : null),
                Isrcs = c.Isrcs.Count > 0 ? c.Isrcs : track?.Isrcs ?? [],
            },
        };
    }

    /// <summary>
    /// Set the Song from the decision. Album-level fields come from the chosen release only when
    /// <see cref="AlbumFromCandidate"/>, and never the album, track or disc a request owns; ids
    /// when <see cref="RecordingConfirmed"/>; the code from the request, else the match, else the
    /// file. Anything weaker is left for the old fill-the-blanks rules that run after this.
    /// </summary>
    public void ApplyTo(Song song)
    {
        var request = Evidence?.Request;
        var file = Evidence?.File;

        if (Chosen is not { } chosen)
        {
            ApplyCode(song, request, file, null);
            return;
        }
        var c = chosen.Candidate;
        var from = c.Source.ToString();

        if (RecordingConfirmed)
        {
            if (!string.IsNullOrEmpty(c.RecordingId)) Set(song, "recordingId", song.MusicBrainzRecordingId = c.RecordingId, from);
            if (c.ArtistIds.Count > 0) song.MusicBrainzArtistIds = c.ArtistIds.ToList();
            if (c.Artists.Count > 1) song.Artists = c.Artists.ToList();
            if (c.PrimaryArtist is { Length: > 0 } primary) song.PrimaryArtist = primary;
            if (c.FingerprintId is { Length: > 0 } acoustId) Set(song, "acoustId", song.AcoustId = acoustId, from);
        }

        if (Confidence == TagConfidence.Strong && Settings.TagFromMatch)
        {
            if (!string.IsNullOrWhiteSpace(c.RecordingTitle)) Set(song, "title", song.Title = c.RecordingTitle, from);
            if (!string.IsNullOrWhiteSpace(c.ArtistCredit)) Set(song, "artist", song.Artist = c.ArtistCredit, from);
        }

        if (AlbumFromCandidate)
        {
            // A request that named its album keeps it; the release only confirms it, and lends its
            // facts when it is the same album.
            var requestOwns = request?.OwnsAlbum == true;
            var sameAlbum = !requestOwns || chosen.PenaltyOf("album") is < 1;
            if (!requestOwns)
            {
                if (c.AlbumTitle is { Length: > 0 } album) Set(song, "album", song.Album = album, from);
                if (c.AlbumArtist is { Length: > 0 } albumArtist) Set(song, "albumArtist", song.AlbumArtist = albumArtist, from);
                song.IsCompilation = c.IsCompilation;
                if (c.TrackNumber is > 0) Set(song, "track", (song.Track = c.TrackNumber).ToString(), from);
                if (c.TrackCount is > 0) song.TotalTracks = c.TrackCount;
                if (c.DiscNumber is > 0) song.DiscNumber = c.DiscNumber;
            }
            if (sameAlbum)
            {
                ApplyDates(song, c, from);
                if (c.Label is { Length: > 0 }) Set(song, "label", song.Label = c.Label, from);
                if (c.CatalogNumber is { Length: > 0 }) Set(song, "catalogNumber", song.CatalogNumber = c.CatalogNumber, from);
                if (c.Barcode is { Length: > 0 }) Set(song, "barcode", song.Barcode = c.Barcode, from);
                if (c.KindText is { Length: > 0 }) Set(song, "releaseType", song.ReleaseType = c.KindText, from);
                if (c.Status is { Length: > 0 }) Set(song, "releaseStatus", song.ReleaseStatus = c.Status.ToLowerInvariant(), from);
                if (c.Country is { Length: > 0 }) Set(song, "releaseCountry", song.ReleaseCountry = c.Country, from);
                if (c.DiscCount is > 0) Set(song, "discCount", (song.TotalDiscs = c.DiscCount).ToString(), from);
                if (c.Source is TagSource.Fingerprint or TagSource.Database)
                {
                    song.MusicBrainzReleaseId = c.ReleaseId;
                    song.MusicBrainzReleaseGroupId = c.ReleaseGroupId;
                    song.MusicBrainzAlbumTitle = c.AlbumTitle;
                    if (c.ReleaseTrackId is { Length: > 0 }) Set(song, "releaseTrackId", song.MusicBrainzReleaseTrackId = c.ReleaseTrackId, from);
                    if (c.AlbumArtistIds.Count > 0) song.MusicBrainzAlbumArtistIds = c.AlbumArtistIds.ToList();
                }
                if (c.CoverUrl is { Length: > 0 } cover && string.IsNullOrEmpty(song.CoverArtUrlLarge)) song.CoverArtUrlLarge = cover;
                if (c.Genre is { Length: > 0 } genre && string.IsNullOrEmpty(song.Genre)) song.Genre = genre;
            }
        }
        else if (Confidence != TagConfidence.None)
            Notes.Add($"the album-level tags were not taken from {Describe(c)}: {Confidence} from {from}");

        ApplyCode(song, request, file, Confidence is TagConfidence.Strong or TagConfidence.Medium ? c : null);
    }

    /// <summary>What a rehearsal still writes: the facts that do not depend on the match. The
    /// code from the request, else the file, and the fingerprint service's id when verification
    /// already confirmed the recording.</summary>
    public void ApplyRehearsalTo(Song song)
    {
        Rehearsed = true;
        ApplyCode(song, Evidence?.Request, Evidence?.File, null);
        if (song.Verification is { Verdict: Fingerprint.VerificationVerdict.Confirmed, AcoustId: { Length: > 0 } id })
            Set(song, "acoustId", song.AcoustId = id, TagSource.Fingerprint.ToString());
    }

    private void ApplyDates(Song song, ReleaseCandidate c, string from)
    {
        var year = Settings.YearFromOriginalRelease ? c.OriginalYear ?? c.Year : c.Year ?? c.OriginalYear;
        if (year is > 0) Set(song, "year", (song.Year = year).ToString(), from);
        var original = c.GroupFirstReleaseDate ?? (c.OriginalYear?.ToString(CultureInfo.InvariantCulture));
        if (original is { Length: > 0 }) Set(song, "originalDate", song.OriginalDate = original, from);
        if (c.ReleaseDate is { Length: > 0 }) Set(song, "releaseDate", song.ReleaseDate = c.ReleaseDate, from);
    }

    /// <summary>The request's code wins, then the match's, then the file's own.</summary>
    private void ApplyCode(Song song, TagRequest? request, FileFacts? file, ReleaseCandidate? candidate)
    {
        if (SongIdentity.NormalizeIsrc(request?.Isrc) is { } requested)
            Set(song, "isrc", song.Isrc = requested, TagSource.Request.ToString());
        else if (candidate is { Isrcs.Count: > 0 } c)
            Set(song, "isrc", song.Isrc = c.Isrcs[0], c.Source.ToString());
        else if (file is { Isrcs.Count: > 0 } f)
            Set(song, "isrc", song.Isrc = f.Isrcs[0], TagSource.FileTags.ToString());
        else if (SongIdentity.NormalizeIsrc(song.Isrc) is { } own)
            song.Isrc = own;
    }

    private void Set(Song song, string field, string? value, string source)
    {
        _ = song;
        Fields[field] = new FieldDecision(value, source);
    }

    public static string Describe(ReleaseCandidate c) =>
        $"'{c.AlbumTitle ?? "?"}'{(c.KindText is null ? "" : $" ({c.KindText})")}{(c.Year is null ? "" : $" {c.Year}")}";

    /// <summary>One line for the log.</summary>
    public string Describe(Song song)
    {
        var gain = song.ReplayGainTrackGainDb is { } g ? g.ToString("+0.00;-0.00", CultureInfo.InvariantCulture) + " dB" : "none";
        var seconds = StageSeconds.TryGetValue("total", out var total) ? total : StageSeconds.Values.Sum();
        var label = string.Join(" ", new[] { song.Label, song.CatalogNumber }.Where(v => !string.IsNullOrEmpty(v)));
        var from = Chosen?.Candidate.Source.ToString() ?? "nothing";
        return string.Format(CultureInfo.InvariantCulture,
            "Tagged '{0} - {1}' as '{2}' ({3}{4}) from {5}, {6} ({7:0.000}), {8} candidates, gain {9}, {10:0.0}s",
            song.Artist, song.Title, song.Album, song.Year?.ToString(CultureInfo.InvariantCulture) ?? "no year",
            label.Length > 0 ? ", " + label : "", from, Confidence, Chosen?.Distance ?? 1, Ranked.Count, gain, seconds);
    }

    /// <summary>What goes into the fetched-songs log and the dashboard: the top candidates and
    /// their biggest penalties, the field table, the notes and the timings.</summary>
    public TagReport ToReport()
    {
        var candidates = Ranked.Take(5).Select(scored => new TagReportCandidate(
            scored.Candidate.Source.ToString(), scored.Candidate.RecordingTitle, scored.Candidate.AlbumTitle ?? "",
            scored.Candidate.KindText, scored.Candidate.ReleaseDate ?? scored.Candidate.GroupFirstReleaseDate,
            Math.Round(scored.Distance, 3),
            scored.Breakdown.Where(b => b.Penalty > 0).OrderByDescending(b => b.Penalty * b.Weight).Take(3)
                .Select(b => string.Format(CultureInfo.InvariantCulture, "{0} {1:0.00}", b.Key, b.Penalty)).ToList())).ToList();
        return new TagReport(Confidence.ToString(), Chosen is null ? null : Math.Round(Chosen.Distance, 3),
            Chosen?.Candidate.AlbumTitle, Chosen?.Candidate.ReleaseId, Chosen?.Candidate.Source.ToString(),
            Chosen?.Candidate.ReleaseDate, candidates, new Dictionary<string, FieldDecision>(Fields), Notes.ToList(),
            StageSeconds.ToDictionary(s => s.Key, s => Math.Round(s.Value, 2)), Rehearsed, DetailsPrefetchHit,
            IntegratedLufs, TruePeakDbfs);
    }
}

/// <summary>What goes into the fetched-songs log and the dashboard.</summary>
public sealed record TagReport(string Confidence, double? Distance, string? ReleaseTitle, string? ReleaseId,
    string? Source, string? ReleaseDate, IReadOnlyList<TagReportCandidate> Candidates,
    IReadOnlyDictionary<string, FieldDecision> Fields, IReadOnlyList<string> Notes,
    IReadOnlyDictionary<string, double> StageSeconds, bool Rehearsed, bool DetailsPrefetchHit,
    double? IntegratedLufs, double? TruePeakDbfs);

public sealed record TagReportCandidate(string Source, string Title, string Album, string? Type, string? Date,
    double Distance, IReadOnlyList<string> BiggestPenalties);
