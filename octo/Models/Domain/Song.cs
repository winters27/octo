namespace Octo.Models.Domain;

/// <summary>
/// Represents a song (local or external)
/// </summary>
public class Song
{
    /// <summary>
    /// Unique ID. For external songs, prefixed with "ext-" + provider + "-" + external id
    /// Example: "ext-deezer-123456" or "local-789"
    /// </summary>
    public string Id { get; set; } = string.Empty;
    
    public string Title { get; set; } = string.Empty;
    public string Artist { get; set; } = string.Empty;
    public string? ArtistId { get; set; }
    public string Album { get; set; } = string.Empty;
    public string? AlbumId { get; set; }
    public int? Duration { get; set; } // In seconds
    public int? Track { get; set; }
    public int? DiscNumber { get; set; }
    public int? TotalTracks { get; set; }
    public int? Year { get; set; }
    public string? Genre { get; set; }

    /// <summary>
    /// The file's real format and bitrate, for a library song Navidrome described. Without them a
    /// library song was declared as FLAC at 1411 kbps whatever it was, and a strict client
    /// prepared the wrong decoder for an MP3.
    /// </summary>
    public string? Suffix { get; set; }
    public int? BitRate { get; set; }

    /// <summary>
    /// The Soulseek peer and remote filename this came from, when it came from Soulseek.
    /// Carried so it can be written down at registration: "Wrong song" needs to know who
    /// delivered the file, and nothing else in Octo records that after the transfer ends.
    /// </summary>
    public string? SourcePeer { get; set; }
    public string? SourceFile { get; set; }

    /// <summary>
    /// For a download kept although its spectrum says it was made from a lossy file, what it
    /// was likely made from ("about 128 kbps MP3"). Written down beside SourcePeer, so a file that
    /// claims to be lossless and is not can be found again. Null for everything else.
    /// </summary>
    public string? TranscodedFrom { get; set; }
    public string? CoverArtUrl { get; set; }
    
    /// <summary>
    /// High-resolution cover art URL (for embedding)
    /// </summary>
    public string? CoverArtUrlLarge { get; set; }
    
    /// <summary>
    /// BPM (beats per minute) if available
    /// </summary>
    public int? Bpm { get; set; }
    
    /// <summary>
    /// ISRC (International Standard Recording Code)
    /// </summary>
    public string? Isrc { get; set; }

    /// <summary>
    /// Every ISRC the library's own entry listed, exactly as Navidrome sent them, for a library
    /// song Octo rebuilt from Navidrome's answer (radio, the Discovery blend). Carried whole so the
    /// song goes back out to the client with the codes it came in with, not with none.
    /// </summary>
    public List<string> Isrcs { get; set; } = new();

    /// <summary>
    /// What a Subsonic response lists under OpenSubsonic's <c>isrc</c>: the library's own list
    /// untouched when there is one, otherwise the song's ISRC when it is a valid one.
    /// </summary>
    public IReadOnlyList<string> IsrcsForClients() =>
        Isrcs.Count > 0 ? Isrcs
        : Octo.Services.Common.SongIdentity.NormalizeIsrc(Isrc) is { } isrc ? [isrc]
        : [];
    
    /// <summary>
    /// Full release date (format: YYYY-MM-DD)
    /// </summary>
    public string? ReleaseDate { get; set; }
    
    /// <summary>
    /// Album artist name (may differ from track artist)
    /// </summary>
    public string? AlbumArtist { get; set; }
    
    /// <summary>
    /// Composer(s)
    /// </summary>
    public string? Composer { get; set; }
    
    /// <summary>
    /// Album label
    /// </summary>
    public string? Label { get; set; }
    
    /// <summary>
    /// Copyright
    /// </summary>
    public string? Copyright { get; set; }
    
    /// <summary>
    /// Contributing artists (features, etc.)
    /// </summary>
    public List<string> Contributors { get; set; } = new();
    
    /// <summary>
    /// Indicates whether the song is available locally or needs to be downloaded
    /// </summary>
    public bool IsLocal { get; set; }
    
    /// <summary>
    /// External provider (deezer, spotify, etc.) - null if local
    /// </summary>
    public string? ExternalProvider { get; set; }
    
    /// <summary>
    /// ID on the external provider (for downloading)
    /// </summary>
    public string? ExternalId { get; set; }
    
    /// <summary>
    /// Local file path (if available)
    /// </summary>
    public string? LocalPath { get; set; }
    
    /// <summary>
    /// Deezer explicit content lyrics value
    /// 0 = Naturally clean, 1 = Explicit, 2 = Not applicable, 3 = Clean/edited version, 6/7 = Unknown
    /// </summary>
    public int? ExplicitContentLyrics { get; set; }

    /// <summary>Which radio source suggested this song, as the apps show it ("YouTube Music").
    /// Only radio answers set it; never stored.</summary>
    [System.Text.Json.Serialization.JsonIgnore]
    public string? SuggestedBy { get; set; }

    /// <summary>A shallow copy, for a response that must change a song another response shares.</summary>
    public Song Copy() => (Song)MemberwiseClone();

    /// <summary>
    /// MusicBrainz recording id of a fingerprint-confirmed download. Written as
    /// MUSICBRAINZ_TRACKID (UFID on ID3), which is where Picard and Navidrome both keep the
    /// RECORDING id, despite the name.
    /// </summary>
    public string? MusicBrainzRecordingId { get; set; }

    /// <summary>
    /// The release and release group the match came from, and that group's title. Used to find
    /// the right cover; the release id is never written, because Navidrome groups albums by
    /// MUSICBRAINZ_ALBUMID before the album name.
    /// </summary>
    public string? MusicBrainzReleaseId { get; set; }
    public string? MusicBrainzReleaseGroupId { get; set; }
    public string? MusicBrainzAlbumTitle { get; set; }
    public List<string> MusicBrainzArtistIds { get; set; } = new();

    /// <summary>
    /// Every credited artist, one per entry, for the multi-value ARTISTS tag. Empty when the
    /// credit is a single name. Navidrome reads it, so a collaboration is filed under each artist
    /// instead of under a new artist named after all of them (#49).
    /// </summary>
    public List<string> Artists { get; set; } = new();

    /// <summary>The first credited artist, when a structured source said which one that is.
    /// Names the artist folder; see BaseDownloadService.PrimaryCredit.</summary>
    public string? PrimaryArtist { get; set; }

    public bool IsCompilation { get; set; }

    /// <summary>The catalogue number the label gave the release.</summary>
    public string? CatalogNumber { get; set; }

    /// <summary>The release's barcode (UPC or EAN).</summary>
    public string? Barcode { get; set; }

    /// <summary>The kind of release, lowercase, as the music database says it: "album", "single", "album; compilation".</summary>
    public string? ReleaseType { get; set; }

    /// <summary>The release's status, lowercase: "official", "promotion", "bootleg".</summary>
    public string? ReleaseStatus { get; set; }

    /// <summary>The two-letter country the release came out in ("XW" for worldwide).</summary>
    public string? ReleaseCountry { get; set; }

    /// <summary>When the recording first came out, as a full date when known (YYYY-MM-DD), else a year.</summary>
    public string? OriginalDate { get; set; }

    /// <summary>The id of this track on the chosen release, which taggers and the library server both read.</summary>
    public string? MusicBrainzReleaseTrackId { get; set; }

    /// <summary>The ids of the release's album artists, one per credit.</summary>
    public List<string> MusicBrainzAlbumArtistIds { get; set; } = new();

    /// <summary>How many discs the release has, when the chosen release said.</summary>
    public int? TotalDiscs { get; set; }

    /// <summary>What the downloaded version's words are, as the advisory tag holds it: 1 explicit,
    /// 2 the clean edit, 0 neither. Null when nothing that matched this exact version said.</summary>
    public int? Advisory { get; set; }

    /// <summary>
    /// The album grouping values the album this track joins already carries in the library (its
    /// release id, release date and version), copied exactly so the library server keeps the two
    /// as one album. Null when there is no such album to join. Never serialised.
    /// </summary>
    [System.Text.Json.Serialization.JsonIgnore]
    public Octo.Services.Tagging.AlbumGrouping? JoinsAlbum { get; set; }

    /// <summary>The fingerprint service's id for the audio, once it confirmed the recording.</summary>
    public string? AcoustId { get; set; }

    /// <summary>ReplayGain from the measured loudness: the gain in dB that brings the track to the
    /// reference level, and its peak as a fraction of full scale. Album values come from an album walk.</summary>
    public double? ReplayGainTrackGainDb { get; set; }
    public double? ReplayGainTrackPeak { get; set; }
    public double? ReplayGainAlbumGainDb { get; set; }
    public double? ReplayGainAlbumPeak { get; set; }

    /// <summary>
    /// How the download was identified and what the tags came from, carried through the download
    /// so the fetched-songs log can show it. Never serialised.
    /// </summary>
    [System.Text.Json.Serialization.JsonIgnore]
    public Octo.Services.Tagging.TagPlan? TagPlan { get; set; }

    /// <summary>
    /// What AcoustID said about the downloaded file. Carried on the Song because
    /// DownloadSongInternalAsync threads ONE instance through download, tagging and placement.
    /// Never serialised.
    /// </summary>
    [System.Text.Json.Serialization.JsonIgnore]
    public Octo.Services.Fingerprint.VerificationResult? Verification { get; set; }
}

