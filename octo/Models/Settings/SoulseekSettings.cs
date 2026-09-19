namespace Octo.Models.Settings;

/// <summary>
/// Configuration for the Soulseek (slskd) integration.
/// Octo talks to a self-hosted slskd instance which fronts the Soulseek P2P network.
/// </summary>
public class SoulseekSettings
{
    /// <summary>
    /// Base URL of the slskd REST API (e.g. http://slskd:5030 when running in the same docker network).
    /// </summary>
    public string? BaseUrl { get; set; }

    /// <summary>
    /// slskd web UI / API admin username (Basic Auth).
    /// </summary>
    public string? Username { get; set; }

    /// <summary>
    /// slskd web UI / API admin password (Basic Auth).
    /// </summary>
    public string? Password { get; set; }

    /// <summary>
    /// How long to wait (seconds) for a Soulseek search to gather peer responses
    /// before returning results. Soulseek searches stream in over time, so this is
    /// the difference between finding a lossless file and silently settling for a
    /// transcode.
    ///
    /// This was 6, which measurement showed is simply too short: polling slskd's
    /// /responses for the same query returned nothing at 6s, 10s or 15s, then 14
    /// responses including 5 FLACs at 20s — reproducibly, across three runs. The
    /// effect was that every star fell back to YouTube MP3 while lossless copies
    /// were sitting there unseen. Note that the search status object reports a
    /// responseCount well before /responses will hand the files over, so a status
    /// poll makes short waits look adequate when they are not.
    ///
    /// This is a CEILING, not a duration: the search returns as soon as it has
    /// enough usable candidates to choose from, or as soon as slskd says the search
    /// has finished. A short value is therefore still a hard cap on finding
    /// anything, while a generous one costs nothing when results arrive early or
    /// when the search comes back empty.
    ///
    /// Star-triggered downloads are fire-and-forget, so the wait costs the user
    /// nothing; it only delays the file landing.
    /// </summary>
    public int SearchWaitSeconds { get; set; } = 30;

    /// <summary>
    /// Minimum file size in bytes to consider a search hit a real lossless file.
    /// Default 5 MB filters out 30s teaser clips and mislabelled tiny files.
    /// </summary>
    public long MinFileSizeBytes { get; set; } = 5 * 1024 * 1024;

    /// <summary>
    /// Preferred file extension. Hits with this extension are sorted first.
    /// </summary>
    public string PreferredExtension { get; set; } = "flac";

    /// <summary>
    /// Max time to wait (seconds) for a download to complete before giving up on that
    /// peer and trying the next one. Per attempt, not per track: a track that has to
    /// walk all five candidates can spend this five times over. A peer that rejects
    /// outright is detected in seconds and does not wait this out.
    /// </summary>
    public int DownloadTimeoutSeconds { get; set; } = 180;

    /// <summary>
    /// Fingerprint every finished Soulseek download with Chromaprint and ask AcoustID what
    /// it actually is before accepting it. Off by default: it needs a free AcoustID key and
    /// the fpcalc binary, and without both it can only ever be a no-op.
    ///
    /// This also switches on the rejected-peer memory. A file discarded for being the wrong
    /// recording, by this check OR by the pre-existing duration check, is remembered by peer
    /// and filename and never offered as a candidate again.
    /// Environment variable: SLSKD_VERIFY_DOWNLOADS
    /// </summary>
    public bool VerifyDownloads { get; set; } = false;

    /// <summary>
    /// AcoustID application key, free from https://acoustid.org/new-application. Blank
    /// disables the lookup half of VerifyDownloads; the duration check and its rejection
    /// memory keep working without it. Named ...ApiKey so the Config-sources tab masks it.
    /// Environment variable: ACOUSTID_API_KEY
    /// </summary>
    public string AcoustIdApiKey { get; set; } = string.Empty;

    /// <summary>
    /// On a confident AcoustID match, write that recording's title, artist, album and year,
    /// which are MusicBrainz's, onto the file instead of trusting the peer's tags. Does
    /// nothing unless VerifyDownloads is on and a key is set.
    /// Environment variable: SLSKD_TAG_FROM_MUSICBRAINZ
    /// </summary>
    public bool TagFromMusicBrainz { get; set; } = false;

    /// <summary>
    /// Minimum AcoustID fingerprint score, as a percentage, before a lookup result is
    /// allowed to decide anything at all.
    ///
    /// Counter-intuitively this is a permissiveness dial, not a strictness dial: results
    /// below it are ignored entirely rather than treated as rejections, so a HIGHER value
    /// rejects FEWER files. The admin help text says so out loud because the intuition runs
    /// the other way.
    /// Environment variable: SLSKD_MIN_MATCH_SCORE
    /// </summary>
    public int MinMatchScore { get; set; } = 85;

    /// <summary>
    /// How long a rejected peer and filename is remembered. Long enough that a wrong file is
    /// not re-fetched across a listening season, short enough that a verdict that was simply
    /// wrong lapses without the user ever learning the file existed. 0 never forgets, which
    /// suits anyone who would rather clear the list by hand.
    /// Environment variable: SLSKD_REJECTED_PEER_DAYS
    /// </summary>
    public int RejectedPeerTtlDays { get; set; } = 30;

    /// <summary>
    /// How many seconds of audio to fingerprint. AcoustID's own tools submit 120, and more
    /// costs decode time without identifying anything extra.
    /// Environment variable: SLSKD_FINGERPRINT_SECONDS
    /// </summary>
    public int FingerprintSeconds { get; set; } = 120;

    /// <summary>
    /// How long fpcalc may take before it is treated as hung. A FLAC fingerprints in about a
    /// second, so thirty is a hang rather than a slow disk; raise it for very slow storage.
    /// Environment variable: SLSKD_FINGERPRINT_TIMEOUT_SECONDS
    /// </summary>
    public int FingerprintTimeoutSeconds { get; set; } = 30;

    /// <summary>
    /// How long an AcoustID lookup may take. Verification sits between a finished transfer and
    /// the file joining the library, so a slow AcoustID must cost seconds, never the download.
    /// Environment variable: SLSKD_ACOUSTID_TIMEOUT_SECONDS
    /// </summary>
    public int AcoustIdTimeoutSeconds { get; set; } = 10;

    /// <summary>0 means never forget, so it is not clamped upward.</summary>
    public int EffectiveRejectedPeerTtlDays =>
        RejectedPeerTtlDays <= 0 ? 0 : Math.Clamp(RejectedPeerTtlDays, 1, 3650);

    public int EffectiveFingerprintSeconds => Math.Clamp(FingerprintSeconds, 15, 600);
    public int EffectiveFingerprintTimeoutSeconds => Math.Clamp(FingerprintTimeoutSeconds, 5, 300);
    public int EffectiveAcoustIdTimeoutSeconds => Math.Clamp(AcoustIdTimeoutSeconds, 2, 120);

    /// <summary>
    /// Below 50 an AcoustID score is noise and acting on it manufactures false rejections;
    /// 100 is a score no real fingerprint reaches, which would silently disable the feature.
    /// </summary>
    public int EffectiveMinMatchScore => Math.Clamp(MinMatchScore, 50, 99);

    /// <summary>The same threshold as AcoustID reports it, in 0..1.</summary>
    public double EffectiveMinScoreFraction => EffectiveMinMatchScore / 100.0;
}
