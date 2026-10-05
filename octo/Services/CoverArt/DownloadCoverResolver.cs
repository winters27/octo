using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services.Fingerprint;
using Octo.Services.Soulseek;

namespace Octo.Services.CoverArt;

/// <summary>The cover a download gets, where it came from, and whether it is simply the one the
/// file already carries (so there is nothing to rewrite). <see cref="Headline"/> and
/// <see cref="Note"/> say in words what was kept or replaced and why, for the download's
/// timeline; both are null when the song had no cover of its own to judge.</summary>
public sealed record CoverChoice(byte[] Bytes, string Source, bool KeepsExisting)
{
    public string? Headline { get; init; }
    public string? Note { get; init; }
}

/// <summary>
/// The covers a song has before the chain is asked: the one the file arrived with, whether that
/// file was filed under another album than the one Octo files it under, and, for a replacement
/// of a library song, the cover of the copy it replaces.
/// </summary>
public sealed record CoversOnHand(byte[]? Arrived, bool ArrivedFromOtherAlbum = false, byte[]? LibraryCopy = null);

/// <summary>
/// The download-time cover chain (#51). The download path used to embed one Deezer URL and stop,
/// so anything Deezer did not know was written with no art, while the aggregator that already
/// knew iTunes and Last.fm sat unused beside it.
///
/// Every strict source is asked (Apple's master of the same album, the Cover Art Archive when a
/// fingerprint named the release the album tag describes, the catalog's own cover), within
/// <see cref="GatherTimeout"/>; a cover search by name only when none of them has one at least
/// <see cref="SharpSide"/> wide. The first strict answer of those three, catalog first, says what
/// the release looks like, and only covers that look like it count: Apple's sharper master of
/// another album never replaces a single's own art. The cover the song already has is judged
/// against the same picture (<see cref="CoverKeep"/>) and stays unless a copy of the same art
/// <see cref="CoverKeep.MuchLarger"/> times its side was found. A library copy's own cover of
/// 1000 px or more stays as it is. Whatever happens, the timeline is told what became of the
/// song's own cover and what else was looked at.
/// </summary>
public sealed class DownloadCoverResolver
{
    /// <summary>A slow cover source must cost seconds, never the download: the whole finalize
    /// phase runs under the download lock.</summary>
    private static readonly TimeSpan CatalogTimeout = TimeSpan.FromSeconds(8);

    /// <summary>Big enough that the cover search by name is not asked: the catalog's own covers
    /// are 1000 px.</summary>
    internal const int SharpSide = 1000;

    private readonly CoverArtArchiveLookup _archive;
    private readonly CoverArtAggregator _aggregator;
    private readonly IHttpClientFactory _http;
    private readonly IOptionsMonitor<MetadataSettings> _settings;
    private readonly ILogger<DownloadCoverResolver> _logger;
    private readonly ITunesCoverArtLookup? _itunes;

    public DownloadCoverResolver(CoverArtArchiveLookup archive, CoverArtAggregator aggregator,
        IHttpClientFactory http, IOptionsMonitor<MetadataSettings> settings, ILogger<DownloadCoverResolver> logger,
        ITunesCoverArtLookup? itunes = null)
    {
        _itunes = itunes;
        _archive = archive;
        _aggregator = aggregator;
        _http = http;
        _settings = settings;
        _logger = logger;
    }

    public Task<CoverChoice?> ResolveAsync(Song song, byte[]? embedded, CancellationToken ct) =>
        ChooseAsync(song, new CoversOnHand(embedded), ct);

    /// <summary>One cover the song already has, and whose it is, for the words.</summary>
    private sealed record Own(byte[] Bytes, bool LibraryCopy, bool OtherAlbum)
    {
        public string Kept => LibraryCopy ? "Kept your old copy's cover" : "Kept the cover it came with";
        public string Its => LibraryCopy ? "your old copy's" : "its";
    }

    public async Task<CoverChoice?> ChooseAsync(Song song, CoversOnHand onHand, CancellationToken ct)
    {
        var settings = _settings.CurrentValue;
        var requireSquare = settings.ReplaceVideoCovers;

        // The library copy's cover is the picture the album shows now, beside its other songs, so
        // a good one stays without asking anyone. The cover art page is where it gets changed.
        if (onHand.LibraryCopy is { Length: > 0 } old)
        {
            var oldJudged = CoverKeep.Judge(old, null, requireSquare, libraryCopy: true);
            if (oldJudged.Verdict == OwnCoverVerdict.Keep)
                return Kept(new Own(old, LibraryCopy: true, OtherAlbum: false), oldJudged, oldJudged.Why, null);
        }

        var owns = new List<Own>();
        if (onHand.LibraryCopy is { Length: > 0 } small) owns.Add(new Own(small, LibraryCopy: true, OtherAlbum: false));
        if (onHand.Arrived is { Length: > 0 } arrived) owns.Add(new Own(arrived, LibraryCopy: false, onHand.ArrivedFromOtherAlbum));

        var found = await GatherAsync(song, settings, requireSquare, ct);

        // What the release looks like: the catalog's cover of the album the song is filed under,
        // else the archive's cover of the release a fingerprint named, else Apple's master of the
        // album, all strict matches. A cover search by name is never the measure.
        var reference = (found.FirstOrDefault(f => f.Bytes is not null && f.Source == CatalogSource)
                ?? found.FirstOrDefault(f => f.Bytes is not null && f.Source == ArchiveSource)
                ?? found.FirstOrDefault(f => f.Bytes is not null && f.Source == AppleSource))?.Bytes;

        var judged = owns
            .Select(own => (Own: own, Judgement: CoverKeep.Judge(own.Bytes, reference, requireSquare, own.OtherAlbum, own.LibraryCopy)))
            .ToList();
        var contender = judged
            .Where(j => j.Judgement.Verdict != OwnCoverVerdict.Replace)
            .OrderByDescending(j => j.Judgement.Side)
            .FirstOrDefault();

        // Only the same art counts: a sharper picture of something else is never taken for the
        // release's. With no strict source to say what the release looks like, the song's own
        // cover says it.
        var measure = reference ?? contender.Own?.Bytes;
        var measureLooks = CoverImage.LooksHash(measure);
        foreach (var candidate in found)
            candidate.SameArt = measureLooks is not { } looks
                || CoverImage.LooksHash(candidate.Bytes) is not { } theirs
                || CoverImage.LookAlike(looks, theirs);
        var best = found.Where(f => f.Bytes is not null && f.SameArt).OrderByDescending(f => f.Side).FirstOrDefault();
        var considered = Considered(found);

        if (contender.Own is { } keep)
        {
            var judgement = contender.Judgement;
            if (best is null || !CoverKeep.IsMuchLarger(best.Side, judgement.Side))
            {
                var why = string.Join("; ", new[]
                {
                    judgement.SameArt == true ? "the same art" : null,
                    best is null ? "nothing else was found"
                        : best.Side > judgement.Side ? "nothing found was much larger" : null,
                }.Where(part => part is not null));
                return Kept(keep, judgement, why, considered);
            }
            return Replaced(keep, judgement with { Why = $"{best.Source} has the same art much larger" }, best, considered);
        }

        if (best is not null)
        {
            if (best.Side < SharpSide)
                _logger.LogInformation("Best cover for {Artist} - {Title} is {Side} px, from {Source}",
                    song.Artist, song.Title, best.Side, best.Source);
            var rejected = judged.FirstOrDefault();
            if (rejected.Own is not null) return Replaced(rejected.Own, rejected.Judgement, best, considered);
            return new CoverChoice(best.Bytes, best.Source, false)
            {
                Headline = $"Cover from {best.Source}, {Size(best.Bytes)}",
                Note = Join("It came with no cover of its own.", considered),
            };
        }

        // Nothing found anywhere: the largest cover the song has stays, whatever it is. A picture
        // of another album or a blown-up one beats no picture at all.
        var fallback = judged
            .Where(j => CoverImage.IsUsable(j.Own.Bytes, requireSquare))
            .OrderByDescending(j => j.Judgement.Side)
            .FirstOrDefault();
        if (fallback.Own is { } last)
            return Kept(last, fallback.Judgement, "nothing else was found", considered);

        if (onHand.Arrived is { Length: > 0 } frame && requireSquare
            && CoverImage.CropToSquare(frame) is { } cropped && CoverImage.IsUsable(cropped, true))
            return new(cropped, "the centre of a video frame", false)
            {
                Headline = "Cut the cover out of its video frame",
                Note = Join("It came with a video frame, not a square cover, and nothing else was found.", considered),
            };
        return null;
    }

    private const string AppleSource = "iTunes", ArchiveSource = "Cover Art Archive", CatalogSource = "the catalog",
        SearchSource = "a cover search";

    /// <summary>How long the cover sources may take together. Each has its own timeout too; this
    /// bounds the sum, since finalizing runs under the download lock.</summary>
    internal static TimeSpan GatherTimeout = TimeSpan.FromSeconds(20);

    /// <summary>One source's answer: its cover, or none (Bytes null).</summary>
    private sealed class Found(string source, byte[]? bytes)
    {
        public string Source { get; } = source;
        public byte[]? Bytes { get; } = bytes;
        public int Side { get; } = bytes is { Length: > 0 } && CoverImage.Measure(bytes) is { } size ? Math.Min(size.Width, size.Height) : 0;
        public bool SameArt { get; set; }
    }

    /// <summary>"Also looked at: iTunes had none, Cover Art Archive 1200 px, the catalog 1000 px."</summary>
    private static string? Considered(IReadOnlyList<Found> found)
    {
        if (found.Count == 0) return null;
        var parts = found.Select(f => f.Bytes is null ? $"{f.Source} had none"
            : f.SameArt ? $"{f.Source} {f.Side} px"
            : $"{f.Source} {f.Side} px of a different picture");
        return $"Also looked at: {string.Join(", ", parts)}.";
    }

    private static string? Join(params string?[] sentences) =>
        string.Join(" ", sentences.Where(s => !string.IsNullOrWhiteSpace(s))) is { Length: > 0 } text ? text : null;

    private static CoverChoice Kept(Own own, OwnCoverJudgement judgement, string? why, string? considered) =>
        new(own.Bytes, own.LibraryCopy ? "your old copy" : "the file itself", KeepsExisting: !own.LibraryCopy)
        {
            Headline = own.Kept,
            Note = Join(string.Join(", ", new[] { $"{judgement.Width} x {judgement.Height} px", Kb(own.Bytes), why }
                .Where(part => !string.IsNullOrEmpty(part))) + ".", considered),
        };

    private static CoverChoice Replaced(Own own, OwnCoverJudgement judgement, Found found, string? considered) =>
        new(found.Bytes!, found.Source, false)
        {
            Headline = judgement.Side > 0
                ? $"Replaced {own.Its} {judgement.Side} px cover with a {Size(found.Bytes!)} one from {found.Source}"
                : $"Replaced {own.Its} cover with a {Size(found.Bytes!)} one from {found.Source}",
            Note = Join(Sentence(judgement.Why), considered),
        };

    private static string Size(byte[] bytes) =>
        CoverImage.Measure(bytes) is { } size ? $"{size.Width} x {size.Height} px" : "new";

    private static string Kb(byte[] bytes) => $"{Math.Max(1, (bytes.Length + 512) / 1024)} KB";

    private static string? Sentence(string? words) =>
        string.IsNullOrWhiteSpace(words) ? null
            : (char.IsLower(words[0]) && words.Length > 1 && char.IsUpper(words[1]) ? words : char.ToUpperInvariant(words[0]) + words[1..]) + ".";

    /// <summary>
    /// Every strict source's cover, in the order asked: Apple's master of the album, the archive's
    /// cover of the release a fingerprint named, the catalog's own cover. A sharp one no longer
    /// stops the others, since a larger copy of the same art may come after it. The cover search
    /// by name is asked only when none of those has one at least <see cref="SharpSide"/> wide.
    /// Unusable answers (too small, or not square while video covers are replaced) count as none.
    /// </summary>
    private async Task<List<Found>> GatherAsync(Song song, MetadataSettings settings, bool requireSquare, CancellationToken ct)
    {
        var found = new List<Found>();
        using var budget = CancellationTokenSource.CreateLinkedTokenSource(ct);
        budget.CancelAfter(GatherTimeout);

        async Task Ask(string source, Func<CancellationToken, Task<byte[]?>> fetch)
        {
            byte[]? bytes = null;
            try { bytes = await fetch(budget.Token); }
            catch (Exception ex) when (!ct.IsCancellationRequested)
            {
                _logger.LogDebug("{Source} cover lookup failed for {Artist} - {Title}: {M}", source, song.Artist, song.Title, ex.Message);
            }
            found.Add(new Found(source, CoverImage.IsUsable(bytes, requireSquare) ? bytes : null));
        }

        // A compilation's album artist is nobody Apple would list it under.
        if (_itunes is not null && !song.IsCompilation)
            await Ask(AppleSource, async token =>
            {
                // A barcode the chooser found names one release outright, so Apple is asked by it
                // first and the master lookup below answers from that match without a search.
                if (song.Barcode is { Length: > 0 } barcode && !string.IsNullOrWhiteSpace(song.Album))
                    await _itunes.PrimeByBarcodeAsync([(song.PrimaryArtist ?? song.Artist, song.Album, barcode)], null, token);
                return await _itunes.TryFetchAlbumMasterAsync(song.PrimaryArtist ?? song.Artist, song.Album, song.Title, token);
            });

        // Only when the album tag IS the release the fingerprint matched: a download tagged with
        // a compilation's name must not get the original album's cover.
        if (settings.UseCoverArtArchive
            && (song.MusicBrainzReleaseId is { Length: > 0 } || song.MusicBrainzReleaseGroupId is { Length: > 0 })
            && VerificationResult.AlbumIsFromRelease(song))
            await Ask(ArchiveSource, token => _archive.TryFetchAsync(song.MusicBrainzReleaseId, song.MusicBrainzReleaseGroupId, token));

        if ((song.CoverArtUrlLarge ?? song.CoverArtUrl) is { Length: > 0 } url)
            await Ask(CatalogSource, token => DownloadAsync(url, token));

        if (!found.Any(f => f.Side >= SharpSide))
            await Ask(SearchSource, token => _aggregator.GetCoverAsync(new SoulseekRouting
            {
                Kind = string.IsNullOrWhiteSpace(song.Album) ? RoutingKind.Song : RoutingKind.Album,
                Artist = song.PrimaryArtist ?? song.Artist,
                Title = song.Title,
                Album = song.Album,
            }, background: true, token));
        return found;
    }

    private async Task<byte[]?> DownloadAsync(string url, CancellationToken ct)
    {
        try
        {
            using var timeout = CancellationTokenSource.CreateLinkedTokenSource(ct);
            timeout.CancelAfter(CatalogTimeout);
            using var response = await _http.CreateClient().GetAsync(url, timeout.Token);
            return response.IsSuccessStatusCode ? await response.Content.ReadAsByteArrayAsync(timeout.Token) : null;
        }
        catch (Exception ex)
        {
            _logger.LogDebug("cover download {Url} failed: {M}", url, ex.Message);
            return null;
        }
    }
}
