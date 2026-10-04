using Microsoft.Extensions.Options;
using Octo.Models.Domain;
using Octo.Models.Settings;
using Octo.Services.Audio;
using Octo.Services.Common;
using Octo.Services.Fingerprint;

namespace Octo.Services.Tagging;

/// <summary>
/// "Try it on a song": the whole identification a download gets, run on a library file (with
/// its fingerprint and loudness) or on a name alone, outside the download lock and without
/// writing anything. The report is the same one a download leaves in the fetched-songs log.
/// </summary>
public sealed class TagPreview
{
    private readonly IServiceProvider _services;
    private readonly ReleaseIdentifier _identifier;
    private readonly ILogger<TagPreview> _logger;

    public TagPreview(IServiceProvider services, ReleaseIdentifier identifier, ILogger<TagPreview> logger)
    {
        _services = services;
        _identifier = identifier;
        _logger = logger;
    }

    public async Task<TagReport> PreviewAsync(string? path, string? artist, string? title, string? album, CancellationToken ct)
    {
        var metadata = _services.GetService<IOptionsMonitor<MetadataSettings>>()?.CurrentValue ?? new MetadataSettings();
        var song = new Song { Artist = artist?.Trim() ?? "", Title = title?.Trim() ?? "", Album = album?.Trim() ?? "" };
        var notes = new List<string>();
        Task<Loudness?>? loudness = null;

        if (path is not null)
        {
            // The file's own name and artist stand in for a request that gave none; its album is
            // the file's claim, not the request's, so the chooser weighs it rather than keeps it.
            var facts = TagWriterExtras.ReadFacts(path, tagsAreEvidence: true);
            if (song.Artist.Length == 0) song.Artist = facts.Artist ?? "";
            if (song.Title.Length == 0) song.Title = facts.Title ?? "";
            song.Duration = facts.DurationSeconds > 0 ? facts.DurationSeconds : null;

            var verification = _services.GetService<DownloadVerificationService>();
            if (verification is { IsFingerprintingEnabled: true })
                song.Verification = await verification.VerifyAsync(path, song.Artist, song.Title);
            else notes.Add("download verification is off or has no key, so the fingerprint service was not asked");

            if (metadata.ReplayGain && _services.GetService<ILoudnessMeter>() is { } meter)
                loudness = Task.Run(() => meter.MeasureAsync(path, metadata.EffectiveReplayGainTimeoutSeconds, ct), ct);
        }
        else notes.Add("matched by name alone: no file, so no fingerprint and no loudness");

        var request = ReleaseIdentifier.RequestFor(song, song.Artist, song.Title, song.Album, null);
        var plan = await _identifier.IdentifyAsync(song, request, path ?? "", tagsAreEvidence: path is not null, null, ct);
        plan.Notes.InsertRange(0, notes);
        plan.ApplyTo(song);
        BaseDownloadService.FillBlanksFromCatalog(song, plan.CatalogBest);
        if (ExplicitAdvisory.Decide(song, plan, path) is { } advisory) plan.Fields["advisory"] = ExplicitAdvisory.Field(advisory);
        if (path is not null && WouldTakeOff(path, song.Album) is { Count: > 0 } off)
            plan.Notes.Add($"a download would take off the file's own {string.Join(", ", off)}");

        if (loudness is not null)
        {
            try
            {
                var measured = await loudness;
                plan.IntegratedLufs = measured?.IntegratedLufs;
                plan.TruePeakDbfs = measured?.TruePeakDbfs;
                if (ReplayGainTags.ForTrack(measured) is { } tags)
                    plan.Fields["replayGain"] = new FieldDecision($"{tags.GainText}, peak {tags.PeakText}", "Measured");
            }
            catch (Exception ex)
            {
                _logger.LogDebug("preview loudness failed for {Path}: {M}", path, ex.Message);
            }
        }
        return plan.ToReport();
    }

    /// <summary>What the writer would take off the file, worked out on the file in memory and
    /// never saved: the preview writes nothing.</summary>
    private IReadOnlyList<string> WouldTakeOff(string path, string? album)
    {
        try
        {
            using var file = TagLib.File.Create(path);
            return ReleaseFactTags.Tidy(file, ReleaseFactTags.FiledElsewhere(file.Tag.Album, album), null);
        }
        catch (Exception ex)
        {
            _logger.LogDebug("preview could not read {Path}: {M}", path, ex.Message);
            return [];
        }
    }
}
