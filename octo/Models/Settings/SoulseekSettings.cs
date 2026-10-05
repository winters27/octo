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
    /// The address the dashboard's Open slskd button opens in your browser. Blank works it out:
    /// BaseUrl when that is an address a browser can reach, otherwise this server's own name on
    /// BaseUrl's port (5030). Only http and https addresses are used. The browser gets the address
    /// and nothing else: slskd asks for its own sign-in, and Octo's slskd login never leaves Octo.
    /// Environment variable: SLSKD_WEB_URL
    /// </summary>
    public string WebUrl { get; set; } = string.Empty;

    /// <summary>
    /// Ask Soulseek's own port test (tools.slsknet.org) whether other people can connect to slskd's
    /// listening port, for the Sharing card. It tests the address the request comes from, so it
    /// only means something when Octo and slskd reach the internet the same way. Sends the port
    /// number and nothing else, at most every 6 hours unless you press Test. Off never asks.
    /// Environment variable: SLSKD_CHECK_PORT
    /// </summary>
    public bool CheckListenPort { get; set; } = true;

    /// <summary>
    /// Share the music library with the Soulseek network, through slskd. On, Octo writes
    /// SharedFolders into slskd's own settings file and has slskd look through them; off, it writes
    /// an empty list, has slskd forget what it shared and cancels uploads still waiting or under
    /// way. Applied within seconds, without a restart. Off here so an existing install never starts
    /// sharing on an update by itself; the installer and .env.example turn it on for new ones.
    /// Environment variable: SLSKD_SHARE_LIBRARY
    /// </summary>
    public bool ShareLibrary { get; set; } = false;

    /// <summary>
    /// What slskd shares while ShareLibrary is on, as slskd sees the folders, separated by ";".
    /// "[Music]/share" is the library's read-only mount in the bundled compose file, shown to other
    /// people as a folder named Music. A "!" in front keeps a folder out.
    /// Environment variable: SLSKD_SHARED_DIR
    /// </summary>
    public string SharedFolders { get; set; } = "[Music]/share";

    /// <summary>SharedFolders as a list, blanks dropped.</summary>
    public IReadOnlyList<string> SharedFolderList =>
        (SharedFolders ?? "").Split(';', StringSplitOptions.TrimEntries | StringSplitOptions.RemoveEmptyEntries);

    /// <summary>WebUrl when it is an absolute http or https address, otherwise null.</summary>
    public string? EffectiveWebUrl => SafeWebUrl(WebUrl);

    /// <summary>An absolute http or https address, trimmed, or null. Anything else (a script: or
    /// file: address, a bare host) is never handed to the browser as a link.</summary>
    public static string? SafeWebUrl(string? value) =>
        Uri.TryCreate(value?.Trim(), UriKind.Absolute, out var uri)
        && (uri.Scheme == Uri.UriSchemeHttp || uri.Scheme == Uri.UriSchemeHttps)
        && !string.IsNullOrEmpty(uri.Host)
            ? uri.ToString()
            : null;

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
    /// This is a CEILING, not a duration. slskd ends most searches itself, after 15 s
    /// with no new answer or at the response or file limit, and Octo reads the answers
    /// then. A search still running at the ceiling is cancelled, which makes slskd hand
    /// over everything it gathered, so a long search costs time but never its results.
    ///
    /// Star-triggered downloads are fire-and-forget, so the wait costs the user
    /// nothing; it only delays the file landing.
    /// </summary>
    public int SearchWaitSeconds { get; set; } = 30;

    /// <summary>
    /// The search ceiling for Better quality and the weekly upgrade. Longer than SearchWaitSeconds
    /// because these searches exist for songs the quick search did not find.
    /// Environment variable: SLSKD_UPGRADE_SEARCH_WAIT_SECONDS
    /// </summary>
    public int UpgradeSearchWaitSeconds { get; set; } = 90;

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
    /// How long (seconds) a download may go without a new byte before Octo gives up on
    /// that peer, cancels the transfer in slskd and tries the next one. A peer that
    /// keeps sending is waited for however slow it is, up to an hour. Per attempt, not
    /// per track. A peer that rejects outright is detected in seconds.
    /// </summary>
    public int DownloadTimeoutSeconds { get; set; } = 180;

    /// <summary>
    /// Fingerprint every finished download with Chromaprint and ask AcoustID what it actually
    /// is before accepting it. A Soulseek file identified as a different recording is discarded
    /// and the next peer tried; a YouTube file has no second candidate, so it is kept and, with
    /// the Review playlist on, asked about. Off by default: it needs a free AcoustID key and
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
    /// which are MusicBrainz's, onto the file instead of trusting the source's tags. Applies to
    /// Soulseek and YouTube downloads alike. Does nothing unless VerifyDownloads is on and a key
    /// is set.
    /// Environment variable: SLSKD_TAG_FROM_MUSICBRAINZ
    /// </summary>
    public bool TagFromMusicBrainz { get; set; } = false;

    /// <summary>
    /// On a confident AcoustID match, name the file from the matched recording as well as
    /// tagging it: artist folder, title, album and track number all come from MusicBrainz, so
    /// the path and the tags are one decision (#48). Implies TagFromMusicBrainz for that file,
    /// because a path from MusicBrainz beside tags from the source is the split this removes.
    /// Off by default: a canonical name is not always the one a user wants on disk (a legal
    /// name, or a composer where the library files the performer). Only files Octo downloads
    /// and confirms are affected; nothing already in the library is renamed.
    /// Environment variable: NAME_FROM_MATCH
    /// </summary>
    public bool NameFromMatch { get; set; } = false;

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

    /// <summary>
    /// Check a download that claims to be lossless (FLAC, WAV, AIFF and the like) for a lossy
    /// file converted to it, by the cutoff in its spectrum. A likely transcode is passed over
    /// for another lossless copy when there is one, and kept, marked as transcoded, when there
    /// is not: it is still the right song. Needs no API key and never fails a download; a
    /// missing ffmpeg or a file it cannot read is no opinion. On by default, since it costs a
    /// second or so of decoding per lossless download and only ever changes which copy is kept.
    /// Also used by the better-quality library action and the duplicate scan.
    /// </summary>
    public bool DetectTranscodes { get; set; } = true;

    /// <summary>
    /// How long the transcode check may decode for, every window included. A FLAC takes well
    /// under a second, so twenty is a hang rather than a slow disk.
    /// </summary>
    public int TranscodeCheckTimeoutSeconds { get; set; } = 20;

    /// <summary>
    /// How long a Soulseek-first download waits while slskd is not logged in to the Soulseek
    /// network, before it goes to the next source. slskd answering is not slskd being able to
    /// search: during Soulseek's maintenance on 2026-10-03 it answered for three hours while
    /// every search failed, and a hearted album landed as YouTube MP3s. Six hours covers a
    /// normal maintenance window. 0 turns the wait off. Read live, no restart needed.
    /// Environment variable: SLSKD_OUTAGE_HOLD_HOURS
    /// </summary>
    public int OutageHoldHours { get; set; } = 6;

    /// <summary>
    /// How many downloads may transfer at once. Each Soulseek download lands in its own folder,
    /// which is what makes this safe; until slskd has put one there, Octo keeps to one at a time
    /// whatever this says. Placing files into the library stays one at a time either way. 1 is the
    /// old behaviour exactly. Read live.
    /// Environment variable: SLSKD_PARALLEL_DOWNLOADS
    /// </summary>
    public int ParallelDownloads { get; set; } = 3;

    /// <summary>
    /// An album heart searches the album once and takes one peer's folder of it in one batch, then
    /// searches song by song only for what that folder lacks. Off is the old song by song walk.
    /// Environment variable: SLSKD_ALBUM_FOLDERS
    /// </summary>
    public bool AlbumFolders { get; set; } = true;

    /// <summary>
    /// Send AcoustID the fingerprints a person confirmed with Keep, so the next person who
    /// downloads that recording gets Confirmed instead of Inconclusive (#47). Only a fingerprint
    /// whose MusicBrainz recording is unambiguous, only after a human kept it, and never one
    /// AcoustID confidently called something else. Needs AcoustIdUserApiKey. Off by default: it
    /// writes to a public database.
    /// Environment variable: ACOUSTID_SUBMIT
    /// </summary>
    public bool SubmitConfirmedFingerprints { get; set; } = false;

    /// <summary>
    /// Your personal AcoustID key, shown at acoustid.org after signing in. Separate from the
    /// application key, because AcoustID credits a submission to a person, not an app. Named
    /// ...ApiKey so the Config-sources tab masks it.
    /// Environment variable: ACOUSTID_USER_KEY
    /// </summary>
    public string AcoustIdUserApiKey { get; set; } = string.Empty;

    /// <summary>0 means never forget, so it is not clamped upward.</summary>
    public int EffectiveRejectedPeerTtlDays =>
        RejectedPeerTtlDays <= 0 ? 0 : Math.Clamp(RejectedPeerTtlDays, 1, 3650);

    public int EffectiveFingerprintSeconds => Math.Clamp(FingerprintSeconds, 15, 600);
    public int EffectiveFingerprintTimeoutSeconds => Math.Clamp(FingerprintTimeoutSeconds, 5, 300);
    public int EffectiveAcoustIdTimeoutSeconds => Math.Clamp(AcoustIdTimeoutSeconds, 2, 120);
    public int EffectiveTranscodeCheckTimeoutSeconds => Math.Clamp(TranscodeCheckTimeoutSeconds, 5, 300);
    public int EffectiveUpgradeSearchWaitSeconds => Math.Clamp(UpgradeSearchWaitSeconds, 30, 300);

    /// <summary>Two days at most: past that the next source is the better answer.</summary>
    public int EffectiveOutageHoldHours => Math.Clamp(OutageHoldHours, 0, 48);

    /// <summary>Six at most: more peers at once gains little and spends the Soulseek network's patience.</summary>
    public int EffectiveParallelDownloads => Math.Clamp(ParallelDownloads, 1, 6);

    /// <summary>
    /// Below 50 an AcoustID score is noise and acting on it manufactures false rejections;
    /// 100 is a score no real fingerprint reaches, which would silently disable the feature.
    /// </summary>
    public int EffectiveMinMatchScore => Math.Clamp(MinMatchScore, 50, 99);

    /// <summary>The same threshold as AcoustID reports it, in 0..1.</summary>
    public double EffectiveMinScoreFraction => EffectiveMinMatchScore / 100.0;
}
