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
/// First the cover the song already has is judged (<see cref="CoverKeep"/>): the same art as the
/// catalog's cover at 1000 px or more stays as it is, and nothing else is asked. Otherwise the
/// chain is asked in order: Apple's master of the same album (often 3000 px, and only on a
/// strict match), the Cover Art Archive when a fingerprint named the release the album tag
/// describes, the catalog's own cover, and the aggregator by name. The first one at least
/// <see cref="SharpSide"/> wide wins at once; otherwise the largest one seen does. A smaller
/// cover of the same art the song already had stays unless the chain's is
/// <see cref="CoverKeep.MuchLarger"/> times its side. A cover that is not square counts as
/// missing, and a letterboxed video frame gives up its centre only when nothing else was found.
/// </summary>
public sealed class DownloadCoverResolver
{
    /// <summary>A slow cover source must cost seconds, never the download: the whole finalize
    /// phase runs under the download lock.</summary>
    private static readonly TimeSpan CatalogTimeout = TimeSpan.FromSeconds(8);

    /// <summary>Big enough to stop looking: the catalog's own covers are 1000 px. Apple's master,
    /// asked first, is usually far larger, and is what a match there gets.</summary>
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

        // The library copy first: it is the picture the album shows now.
        var owns = new List<Own>();
        if (onHand.LibraryCopy is { Length: > 0 } old) owns.Add(new Own(old, LibraryCopy: true, OtherAlbum: false));
        if (onHand.Arrived is { Length: > 0 } arrived) owns.Add(new Own(arrived, LibraryCopy: false, onHand.ArrivedFromOtherAlbum));

        // The catalog's cover is what the album looks like, so it is fetched first when there is
        // a cover to compare with it; otherwise the chain fetches it in its turn.
        byte[]? catalog = null;
        var catalogFetched = false;
        var catalogUrl = song.CoverArtUrlLarge ?? song.CoverArtUrl;
        if (owns.Count > 0 && catalogUrl is { Length: > 0 })
        {
            catalog = await DownloadAsync(catalogUrl, ct);
            catalogFetched = true;
        }
        var reference = CoverImage.IsUsable(catalog, requireSquare: true) ? catalog : null;

        var judged = new List<(Own Own, OwnCoverJudgement Judgement)>();
        foreach (var own in owns)
        {
            var judgement = CoverKeep.Judge(own.Bytes, reference, requireSquare, own.OtherAlbum, own.LibraryCopy);
            if (judgement.Verdict == OwnCoverVerdict.Keep) return Kept(own, judgement, judgement.Why);
            judged.Add((own, judgement));
        }

        var (best, bestSide) = await RunChainAsync(song, settings, requireSquare, catalogFetched, catalog, ct);

        foreach (var (own, first) in judged.Where(j => j.Judgement.Verdict == OwnCoverVerdict.Weigh))
        {
            if (best is null)
                return Kept(own, first, first.SameArt == true ? "the same art; nothing else was found" : "nothing else was found");
            var judgement = first;
            // With nothing to say what the album looks like, the chain's own find says it.
            if (judgement.SameArt is null)
            {
                judgement = CoverKeep.Judge(own.Bytes, best.Bytes, requireSquare, own.OtherAlbum, own.LibraryCopy);
                if (judgement.Verdict == OwnCoverVerdict.Keep) return Kept(own, judgement, judgement.Why);
                if (judgement.Verdict == OwnCoverVerdict.Replace) return Replaced(own, judgement, best);
            }
            if (!CoverKeep.IsMuchLarger(bestSide, judgement.Side))
                return Kept(own, judgement, (judgement.SameArt == true ? "the same art; " : "")
                    + $"nothing found was much larger ({best.Source}, {Size(best.Bytes)})");
            return Replaced(own, judgement with { Why = "it was much smaller" }, best);
        }

        if (best is not null)
        {
            if (bestSide < SharpSide)
                _logger.LogInformation("Best cover for {Artist} - {Title} is {Side} px, from {Source}",
                    song.Artist, song.Title, bestSide, best.Source);
            var replaced = judged.FirstOrDefault();
            return replaced.Own is null ? best : Replaced(replaced.Own, replaced.Judgement, best);
        }

        // Nothing found anywhere: the largest cover the song has stays, whatever it is. A picture
        // of another album or a blown-up one beats no picture at all.
        var fallback = judged
            .Where(j => CoverImage.IsUsable(j.Own.Bytes, requireSquare))
            .OrderByDescending(j => j.Judgement.Side)
            .Select(j => j.Own)
            .FirstOrDefault();
        if (fallback is not null)
            return new CoverChoice(fallback.Bytes, fallback.LibraryCopy ? "your old copy" : "the file itself", !fallback.LibraryCopy)
            {
                Headline = fallback.Kept,
                Note = $"{Size(fallback.Bytes)}, nothing else was found",
            };

        if (onHand.Arrived is { Length: > 0 } frame && requireSquare
            && CoverImage.CropToSquare(frame) is { } cropped && CoverImage.IsUsable(cropped, true))
            return new(cropped, "the centre of a video frame", false);
        return null;
    }

    private static CoverChoice Kept(Own own, OwnCoverJudgement judgement, string? why) =>
        new(own.Bytes, own.LibraryCopy ? "your old copy" : "the file itself", KeepsExisting: !own.LibraryCopy)
        {
            Headline = own.Kept,
            Note = string.Join(", ", new[] { $"{judgement.Width} x {judgement.Height} px", Kb(own.Bytes), why }
                .Where(part => !string.IsNullOrEmpty(part))),
        };

    private static CoverChoice Replaced(Own own, OwnCoverJudgement judgement, CoverChoice found) =>
        found with
        {
            Headline = judgement.Side > 0
                ? $"Replaced {own.Its} {judgement.Side} px cover with a {Size(found.Bytes)} one from {found.Source}"
                : $"Replaced {own.Its} cover with a {Size(found.Bytes)} one from {found.Source}",
            Note = Sentence(judgement.Why),
        };

    private static string Size(byte[] bytes) =>
        CoverImage.Measure(bytes) is { } size ? $"{size.Width} x {size.Height} px" : "new";

    private static string Kb(byte[] bytes) => $"{Math.Max(1, (bytes.Length + 512) / 1024)} KB";

    private static string? Sentence(string? words) =>
        string.IsNullOrWhiteSpace(words) ? null : char.ToUpperInvariant(words[0]) + words[1..] + ".";

    /// <summary>The chain, without the song's own cover: the first one at least
    /// <see cref="SharpSide"/> wide, else the largest seen, else null.</summary>
    private async Task<(CoverChoice? Best, int Side)> RunChainAsync(Song song, MetadataSettings settings,
        bool requireSquare, bool catalogFetched, byte[]? catalog, CancellationToken ct)
    {
        CoverChoice? best = null;
        var bestSide = 0;

        // True when this one is sharp enough to stop; otherwise it is kept if it is the biggest.
        bool Offer(byte[]? bytes, string source)
        {
            if (!CoverImage.IsUsable(bytes, requireSquare) || CoverImage.Measure(bytes!) is not { } size) return false;
            var side = Math.Min(size.Width, size.Height);
            if (side > bestSide)
            {
                best = new CoverChoice(bytes!, source, false);
                bestSide = side;
            }
            return side >= SharpSide;
        }

        // A compilation's album artist is nobody Apple would list it under.
        if (_itunes is not null && !song.IsCompilation)
        {
            // A barcode the chooser found names one release outright, so Apple is asked by it
            // first and the master lookup below answers from that match without a search.
            if (song.Barcode is { Length: > 0 } barcode && !string.IsNullOrWhiteSpace(song.Album))
                await _itunes.PrimeByBarcodeAsync([(song.PrimaryArtist ?? song.Artist, song.Album, barcode)], null, ct);
            var master = await _itunes.TryFetchAlbumMasterAsync(song.PrimaryArtist ?? song.Artist, song.Album, song.Title, ct);
            if (Offer(master, "iTunes")) return (best, bestSide);
        }

        // Only when the album tag IS the release the fingerprint matched: a download tagged with
        // a compilation's name must not get the original album's cover.
        if (settings.UseCoverArtArchive
            && (song.MusicBrainzReleaseId is { Length: > 0 } || song.MusicBrainzReleaseGroupId is { Length: > 0 })
            && VerificationResult.AlbumIsFromRelease(song))
        {
            var archived = await _archive.TryFetchAsync(song.MusicBrainzReleaseId, song.MusicBrainzReleaseGroupId, ct);
            if (Offer(archived, "Cover Art Archive")) return (best, bestSide);
        }

        if (!catalogFetched && (song.CoverArtUrlLarge ?? song.CoverArtUrl) is { Length: > 0 } url)
            catalog = await DownloadAsync(url, ct);
        if (Offer(catalog, "the catalog")) return (best, bestSide);

        try
        {
            var routing = new SoulseekRouting
            {
                Kind = string.IsNullOrWhiteSpace(song.Album) ? RoutingKind.Song : RoutingKind.Album,
                Artist = song.PrimaryArtist ?? song.Artist,
                Title = song.Title,
                Album = song.Album,
            };
            var aggregated = await _aggregator.GetCoverAsync(routing, background: true, ct);
            Offer(aggregated, "a cover search");
        }
        catch (Exception ex)
        {
            _logger.LogDebug("cover search failed for {Artist} - {Title}: {M}", song.Artist, song.Title, ex.Message);
        }
        return (best, bestSide);
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
