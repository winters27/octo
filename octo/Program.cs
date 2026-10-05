using Microsoft.AspNetCore.HttpOverrides;
using Octo.Models.Settings;
using Octo.Services;
using Octo.Services.Soulseek;
using Octo.Services.YouTube;
using Octo.Services.Local;
using Octo.Services.Validation;
using Octo.Services.Subsonic;
using Octo.Services.Common;
using Octo.Services.LastFm;
using Octo.Services.Lidarr;
using Octo.Middleware;

var builder = WebApplication.CreateBuilder(args);

// Subsonic clients sign in through the query string, and ASP.NET writes every request URL
// to the log. This masks t, s, p, apiKey, token, api_key and client in every line, whatever
// the log level.
builder.Logging.AddCredentialRedaction();

// Editable settings file: anything users change in the admin UI is persisted
// here, and this source is added LAST so it overrides env vars / appsettings.
// reloadOnChange=true means the file watcher picks up writes within a few
// hundred ms — services consuming IOptionsMonitor see new values immediately.
// The /app/config directory is bind-mounted in docker-compose so settings
// survive container recreate.
const string SettingsFilePath = "/app/config/settings.json";
builder.Configuration.AddJsonFile(SettingsFilePath, optional: true, reloadOnChange: true);

builder.Services.AddControllers();
builder.Services.AddHttpClient();
builder.Services.AddEndpointsApiExplorer();
builder.Services.AddSwaggerGen();
builder.Services.AddHttpContextAccessor();
builder.Services.Configure<ForwardedHeadersOptions>(options =>
{
    // Octo normally runs one hop behind a container ingress or tunnel. Only
    // accept the original scheme, which is required when generating absolute
    // Radio stream URLs; client IP and Host continue to come from ASP.NET's
    // direct request data. The proxy address is dynamic in container networks,
    // so it cannot be represented by the loopback-only defaults.
    options.ForwardedHeaders = ForwardedHeaders.XForwardedProto;
    options.ForwardLimit = 1;
    options.KnownNetworks.Clear();
    options.KnownProxies.Clear();
});
builder.Services.AddSingleton<Octo.Services.Admin.SettingsFileWriter>(
    sp => new Octo.Services.Admin.SettingsFileWriter(SettingsFilePath));
// Which restart-only settings have changed since this process started; see RestartTracker.
builder.Services.AddSingleton<Octo.Services.Admin.RestartTracker>();
// Running log of fetched songs, stored next to the settings file (same
// bind-mounted config dir, so it survives restarts).
builder.Services.AddSingleton(sp => new Octo.Services.Local.DownloadHistoryService(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "downloads-history.json"),
    sp.GetRequiredService<ILogger<Octo.Services.Local.DownloadHistoryService>>()));
builder.Services.AddSingleton(sp => new LastFmRadioStateStore(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "lastfm-radio-state.json"),
    sp.GetRequiredService<Microsoft.Extensions.Options.IOptionsMonitor<LastFmSettings>>(),
    sp.GetRequiredService<ExternalIdRegistry>(),
    sp.GetRequiredService<ILogger<LastFmRadioStateStore>>()));

builder.Services.AddExceptionHandler<GlobalExceptionHandler>();
builder.Services.AddProblemDetails();

builder.Services.Configure<GenreSettings>(
    builder.Configuration.GetSection("Genre"));
builder.Services.Configure<LibraryActionSettings>(
    builder.Configuration.GetSection("LibraryActions"));
builder.Services.Configure<GeneratedPlaylistSettings>(
    builder.Configuration.GetSection("GeneratedPlaylists"));
builder.Services.Configure<SubsonicSettings>(
    builder.Configuration.GetSection("Subsonic"));
builder.Services.Configure<SoulseekSettings>(
    builder.Configuration.GetSection("Soulseek"));
builder.Services.Configure<LidarrSettings>(
    builder.Configuration.GetSection("Lidarr"));
builder.Services.Configure<LastFmSettings>(
    builder.Configuration.GetSection("LastFm"));
builder.Services.Configure<NotificationSettings>(
    builder.Configuration.GetSection("Notifications"));
builder.Services.Configure<MetadataSettings>(
    builder.Configuration.GetSection("Metadata"));
builder.Services.Configure<ServerSettings>(
    builder.Configuration.GetSection("Server"));
builder.Services.Configure<ListenBrainzSettings>(
    builder.Configuration.GetSection("ListenBrainz"));
builder.Services.Configure<UpdateSettings>(
    builder.Configuration.GetSection("Updates"));
// Who may use the dashboard and the admin API; see AdminSignInGate.
builder.Services.Configure<AdminSettings>(
    builder.Configuration.GetSection("Admin"));
// Listens are records of plays that already happened; a slow ListenBrainz must not
// hold a scrobble response or a radio stream, so the client is short-fused.
builder.Services.AddHttpClient(Octo.Services.ListenBrainz.ListenBrainzService.ClientName,
    c => c.Timeout = TimeSpan.FromSeconds(10));
builder.Services.AddSingleton<Octo.Services.ListenBrainz.ListenBrainzService>();
// Scrobbles of outside songs go out in the background, never inside a client's request, but a
// hung call would still hold up every play queued behind it.
builder.Services.AddHttpClient(LastFmScrobbleService.ClientName, c => c.Timeout = TimeSpan.FromSeconds(10));
builder.Services.AddSingleton<LastFmScrobbleService>();

builder.Services.AddSingleton<ILocalLibraryService, LocalLibraryService>();

builder.Services.AddSingleton<SubsonicRequestParser>();
builder.Services.AddSingleton<SubsonicResponseBuilder>();
builder.Services.AddSingleton<SubsonicModelMapper>();
builder.Services.AddScoped<SubsonicProxyService>();
builder.Services.AddScoped<LastFmRadioTrackResolver>();
builder.Services.AddScoped<LastFmRadioRecommendationService>();
builder.Services.AddScoped<LastFmRadioStreamService>();
builder.Services.AddSingleton<IRadioTuneInSelector, RandomRadioTuneInSelector>();
builder.Services.AddSingleton<LastFmRadioRefreshQueue>();
builder.Services.AddSingleton<LastFmRadioStreamSessionStore>();
builder.Services.AddSingleton<LastFmRadioTrackCache>();
builder.Services.AddSingleton<ILastFmRadioAudioTranscoder, FfmpegLastFmRadioAudioTranscoder>();
builder.Services.AddSingleton<LastFmRadioWarmupService>();
builder.Services.AddHostedService<LastFmRadioWarmupService>(provider =>
    provider.GetRequiredService<LastFmRadioWarmupService>());
builder.Services.AddHostedService<LastFmRadioRefreshWorker>();

// Soulseek (FLAC source) + YouTube (instant-preview stream source).
builder.Services.AddSingleton<SoulseekClient>();
// slskd's Soulseek login, read live, for the dashboard and for downloads that wait out an outage.
builder.Services.AddSingleton<SoulseekLink>();
builder.Services.AddSingleton<ISoulseekLink>(sp => sp.GetRequiredService<SoulseekLink>());
// What this server shares back with Soulseek, and whether people can connect to it, for the dashboard.
builder.Services.AddHttpClient(SoulseekPortCheck.ClientName, c => c.Timeout = TimeSpan.FromSeconds(20));
builder.Services.AddSingleton<SoulseekPortCheck>();
// The Share my library switch: keeps slskd.yml's share list to it, and checks every 10 minutes.
builder.Services.AddSingleton<SoulseekShareSwitch>();
builder.Services.AddHostedService(sp => sp.GetRequiredService<SoulseekShareSwitch>());
builder.Services.AddSingleton<SoulseekSharing>();
// How many downloads transfer at once: one until slskd has put a download in its own folder.
builder.Services.AddSingleton<Octo.Services.Common.DownloadConcurrency>();
// Hearts waiting for Soulseek, on disk beside the other state files so a restart keeps them.
builder.Services.AddSingleton(sp => new Octo.Services.Common.SoulseekHoldStore(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "soulseek-holds.json"),
    sp.GetRequiredService<ILogger<Octo.Services.Common.SoulseekHoldStore>>()));
builder.Services.AddSingleton<YouTubeResolver>();

// Two named HTTP clients for the yt-dlp shim:
//   - search: short timeout, used for /search and /health
//   - stream: infinite timeout, because /stream stays open for the whole song
//     and the default 100s HttpClient timeout would kill the read mid-track.
// Using IHttpClientFactory means the handler is pooled and rotated correctly;
// disposing the HttpClient before reading the stream (the prior bug) is no
// longer possible because the factory owns the lifetime.
builder.Services.AddHttpClient(YouTubeResolver.SearchClientName, c =>
{
    // 60s rather than 30s because back-to-back search3 prewarm bursts can fill
    // the shim's yt-dlp gate (MAX_CONCURRENT_YTDLP, which ships as 5) and queue
    // requests behind 5-8s yt-dlp ytsearch1: invocations. 30s was canceling the
    // tail of every prewarm batch.
    c.Timeout = TimeSpan.FromSeconds(60);
});
builder.Services.AddHttpClient(YouTubeResolver.StreamClientName, c =>
{
    c.Timeout = Timeout.InfiniteTimeSpan;
});
builder.Services.AddSingleton(sp => new ExternalIdRegistry(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "external-ids.json"),
    sp.GetRequiredService<ILogger<ExternalIdRegistry>>()));
builder.Services.AddSingleton<RadioQueueStore>();
builder.Services.AddSingleton<Octo.Services.Subsonic.NavidromeIdentityService>();
builder.Services.AddSingleton<Octo.Services.Subsonic.SubsonicDiscoveryService>();
builder.Services.AddSingleton<Octo.Services.Subsonic.SyncCatalogService>();
builder.Services.AddSingleton<Octo.Services.Admin.DirectoryBrowser>();
// Dashboard sign-ins, remembered per browser across restarts; only token hashes are written.
builder.Services.AddSingleton(sp => new Octo.Services.Admin.BrowseSessionStore(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "browse-sessions.json"),
    sp.GetRequiredService<ILogger<Octo.Services.Admin.BrowseSessionStore>>()));
// Ten wrong sign-ins per address per fifteen minutes, then a wait.
builder.Services.AddSingleton<Octo.Services.Admin.SignInThrottle>();
// The one-time way in when Navidrome cannot vouch for anyone.
builder.Services.AddSingleton(sp => new Octo.Services.Admin.AdminRecoveryCode(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "admin-recovery-code"),
    sp.GetRequiredService<ILogger<Octo.Services.Admin.AdminRecoveryCode>>()));
// Ends the sign-ins of someone Navidrome no longer counts as an admin.
builder.Services.AddSingleton<Octo.Services.Admin.AdminRoleCheck>();
builder.Services.AddSingleton<Octo.Services.Metadata.DeezerMetadataService>();

// Deezer's public API allows roughly 50 requests per 5 seconds and signals refusal with
// HTTP 200 plus an error body, so going over budget corrupts metadata rather than merely
// failing. The limiter is the singleton that holds the budget; the handler is transient
// because IHttpClientFactory recycles handler chains. Every Deezer caller must resolve
// the named client or it bypasses this entirely.
builder.Services.AddSingleton<Octo.Services.Metadata.DeezerRateLimiter>();
builder.Services.AddTransient<Octo.Services.Metadata.DeezerRateLimitHandler>();
builder.Services.AddHttpClient(Octo.Services.Metadata.DeezerRateLimiter.ClientName)
    .AddHttpMessageHandler<Octo.Services.Metadata.DeezerRateLimitHandler>();

// Rejected-peer memory for download verification. Next to the settings file for the same
// reason external-ids.json is: it is knowledge earned by a completed transfer, and losing it
// on every container recreate means re-earning it by re-downloading the same wrong files.
builder.Services.AddSingleton(sp => new RejectedPeerRegistry(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "rejected-peers.json"),
    sp.GetRequiredService<ILogger<RejectedPeerRegistry>>(),
    // A Func rather than a captured value, so changing the TTL takes effect without a restart.
    () => sp.GetRequiredService<Microsoft.Extensions.Options.IOptionsMonitor<SoulseekSettings>>()
        .CurrentValue.EffectiveRejectedPeerTtlDays));

// Genre backfill state lives beside the other config-dir files so a run survives a restart
// and can be resumed deliberately rather than silently restarting.
builder.Services.AddSingleton(sp => new Octo.Services.Metadata.GenreBackfillStore(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "genre-backfill.json"),
    sp.GetRequiredService<ILogger<Octo.Services.Metadata.GenreBackfillStore>>()));
builder.Services.AddSingleton(sp => new Octo.Services.Metadata.GenreBackfillJournal(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "genre-backfill-journal.jsonl"),
    sp.GetRequiredService<ILogger<Octo.Services.Metadata.GenreBackfillJournal>>()));
// Singleton AND hosted, the same instance both ways, so the controller can enqueue into the
// worker the host is running rather than a second copy of it.
builder.Services.AddSingleton<Octo.Services.Metadata.GenreBackfillWorker>();
builder.Services.AddHostedService(sp =>
    sp.GetRequiredService<Octo.Services.Metadata.GenreBackfillWorker>());

// The cover upgrade: same shape as the genre backfill. Its journal keeps every replaced
// picture (once per distinct picture, in cover-backups/) so a run can be undone.
builder.Services.AddSingleton(sp => new Octo.Services.CoverArt.CoverUpgradeStore(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "cover-upgrade.json"),
    sp.GetRequiredService<ILogger<Octo.Services.CoverArt.CoverUpgradeStore>>()));
builder.Services.AddSingleton(sp => new Octo.Services.CoverArt.CoverUpgradeJournal(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "cover-upgrade-journal.jsonl"),
    sp.GetRequiredService<ILogger<Octo.Services.CoverArt.CoverUpgradeJournal>>()));
builder.Services.AddSingleton<Octo.Services.CoverArt.IAlbumCoverFinder, Octo.Services.CoverArt.AlbumCoverFinder>();
builder.Services.AddSingleton<Octo.Services.CoverArt.CoverUpgradeWorker>();
builder.Services.AddHostedService(sp =>
    sp.GetRequiredService<Octo.Services.CoverArt.CoverUpgradeWorker>());

// Resolves a Navidrome song id to a verified file on disk. Read-only and non-destructive on
// its own; it exists first because nothing that acts on a library file can be trusted until
// this is proven against a real library.
builder.Services.AddSingleton<Octo.Services.Library.NavidromeSongPathResolver>();

// Recovery before anything that needs recovering from: the quarantine and the journal land
// with the settings, and only then does anything act on a library file.
builder.Services.AddSingleton<Octo.Services.Library.LibraryActionQuarantine>();
// Where Better quality looks: Soulseek, Lidarr, or both in that order.
builder.Services.AddSingleton<Octo.Services.Library.UpgradeSources>();
builder.Services.AddSingleton<Octo.Services.Library.LibraryActionExecutor>();
// Scoped, because SubsonicProxyService is: it depends on IHttpContextAccessor.
builder.Services.AddScoped<Octo.Services.Library.LibraryActionPlaylistProvisioner>();
builder.Services.AddHostedService<Octo.Services.Library.LibraryActionPlaylistWorker>();
// Singleton AND hosted, the same instance both ways, so the controller enqueues into the
// worker the host is running rather than a second copy of it.
builder.Services.AddSingleton<Octo.Services.Library.LibraryActionRatingWorker>();
builder.Services.AddHostedService(sp =>
    sp.GetRequiredService<Octo.Services.Library.LibraryActionRatingWorker>());
builder.Services.AddSingleton(sp => new Octo.Services.Library.LibraryActionJournal(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "library-actions.json"),
    sp.GetRequiredService<ILogger<Octo.Services.Library.LibraryActionJournal>>()));
// The apps' Library health fixes: tags written in place, albums joined, covers added, removed
// songs put back. Every edit is kept beside the settings so it can be undone.
builder.Services.AddSingleton(sp => new Octo.Services.Library.TagEditJournal(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "tag-edits.json"),
    sp.GetRequiredService<ILogger<Octo.Services.Library.TagEditJournal>>()));
builder.Services.AddSingleton<Octo.Services.Library.LibraryRescan>();
// Marking the library explicit or clean: a preview, then the writes it found, journaled in
// tag-edits.json like the apps' edits so a run (or one song) can be undone.
builder.Services.AddSingleton(sp => new Octo.Services.Library.ExplicitBackfillStore(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "explicit-backfill.json"),
    sp.GetRequiredService<ILogger<Octo.Services.Library.ExplicitBackfillStore>>()));
builder.Services.AddSingleton<Octo.Services.Tagging.IExplicitCatalog, Octo.Services.Tagging.DeezerExplicitCatalog>();
builder.Services.AddSingleton<Octo.Services.Library.ExplicitBackfill>();
builder.Services.AddHostedService(sp => sp.GetRequiredService<Octo.Services.Library.ExplicitBackfill>());
builder.Services.AddSingleton<Octo.Services.Library.LibraryEditService>();

// The playlists Octo fills to ask a person something (#47): what was asked and answered lives
// beside the journal, and the admin-side playlist calls are shared with the action sweep so both
// get the same token refresh.
builder.Services.AddSingleton(sp => new Octo.Services.Library.NoticeQueue(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "notice-queue.json"),
    sp.GetRequiredService<ILogger<Octo.Services.Library.NoticeQueue>>()));
builder.Services.AddSingleton<Octo.Services.Library.NavidromePlaylistApi>();
builder.Services.AddHostedService<Octo.Services.Library.NoticePlaylistWorker>();
// Singleton AND hosted, like the rating worker, so the dashboard's "Scan now" reaches the
// instance the host is running.
builder.Services.AddSingleton<Octo.Services.Library.DuplicateScanWorker>();
// Genre and decade mixes (#54): served by Octo like radio stations, never written to Navidrome.
builder.Services.AddSingleton(sp => new Octo.Services.Library.GeneratedPlaylistService(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "generated-playlists.json"),
    sp.GetRequiredService<IServiceScopeFactory>(),
    sp.GetRequiredService<Microsoft.Extensions.Options.IOptionsMonitor<GeneratedPlaylistSettings>>(),
    sp.GetRequiredService<Microsoft.Extensions.Options.IOptionsMonitor<GenreSettings>>(),
    sp.GetRequiredService<ILogger<Octo.Services.Library.GeneratedPlaylistService>>()));
builder.Services.AddHostedService(sp =>
    sp.GetRequiredService<Octo.Services.Library.DuplicateScanWorker>());
// The weekly quality upgrade (#70). What it tried is kept by file, beside the other state files.
builder.Services.AddSingleton(sp => new Octo.Services.Library.QualityUpgradeStore(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "quality-upgrade.json"),
    sp.GetRequiredService<ILogger<Octo.Services.Library.QualityUpgradeStore>>()));
builder.Services.AddSingleton<Octo.Services.Library.QualityUpgradeWorker>();
// Songs asked to be found in higher quality, from the apps and the Better quality page.
builder.Services.AddSingleton(sp => new Octo.Services.Library.UpgradeQueue(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "upgrades.json"),
    sp.GetRequiredService<ILogger<Octo.Services.Library.UpgradeQueue>>()));
builder.Services.AddSingleton<Octo.Services.Library.UpgradeWorker>();
// Whether a newer Octo release is out, and the files that hand Update now to the host helper.
builder.Services.AddHttpClient(Octo.Services.Updates.ReleaseCheck.ClientName, c => c.Timeout = TimeSpan.FromSeconds(15));
// The dashboard's light: the colours of the fetched songs' covers.
builder.Services.AddHttpClient(Octo.Services.Admin.AmbientPaletteService.ClientName, c => c.Timeout = TimeSpan.FromSeconds(6));
builder.Services.AddSingleton<Octo.Services.Admin.AmbientPaletteService>();
builder.Services.AddSingleton(sp => new Octo.Services.Updates.ReleaseCheck(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "update", "release.json"),
    sp.GetRequiredService<IHttpClientFactory>(),
    sp.GetRequiredService<Microsoft.Extensions.Options.IOptionsMonitor<UpdateSettings>>(),
    sp.GetRequiredService<ILogger<Octo.Services.Updates.ReleaseCheck>>()));
builder.Services.AddHostedService(sp => sp.GetRequiredService<Octo.Services.Updates.ReleaseCheck>());
builder.Services.AddSingleton(sp => new Octo.Services.Updates.UpdateHost(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "update"),
    sp.GetRequiredService<ILogger<Octo.Services.Updates.UpdateHost>>()));
// Whether a song is already in the library, so nothing downloads a second copy.
builder.Services.AddSingleton<Octo.Services.Library.LibraryOwnership>();
// Asked before a heart goes anywhere: a song already in the library is favorited, not fetched.
builder.Services.AddSingleton<Octo.Services.Library.HeartOwnership>();
builder.Services.AddHostedService(sp => sp.GetRequiredService<Octo.Services.Library.UpgradeWorker>());
builder.Services.AddHostedService(sp => sp.GetRequiredService<Octo.Services.Library.QualityUpgradeWorker>());
// Importing from Spotify, a public link or a file: the lists, the sign-ins, and the trickle that
// fetches what the library is missing a few songs an hour, through the heart chain.
builder.Services.Configure<ImportSettings>(builder.Configuration.GetSection("Imports"));
builder.Services.AddHttpClient(Octo.Services.Imports.SpotifyWebApi.ClientName, c => c.Timeout = TimeSpan.FromSeconds(20));
builder.Services.AddHttpClient(Octo.Services.Imports.SpotifyLinkReader.ClientName, c =>
{
    c.Timeout = TimeSpan.FromSeconds(20);
    c.DefaultRequestHeaders.UserAgent.ParseAdd("Mozilla/5.0 (compatible; Octo)");
});
builder.Services.AddSingleton(sp => new Octo.Services.Imports.ImportStore(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "imports.json"),
    sp.GetRequiredService<ILogger<Octo.Services.Imports.ImportStore>>()));
builder.Services.AddSingleton(sp => new Octo.Services.Imports.TrickleQueue(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "imports-trickle.json"),
    sp.GetRequiredService<ILogger<Octo.Services.Imports.TrickleQueue>>()));
builder.Services.AddSingleton(sp => new Octo.Services.Imports.SpotifyAccountStore(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "spotify-accounts.json"),
    sp.GetRequiredService<ILogger<Octo.Services.Imports.SpotifyAccountStore>>()));
builder.Services.AddSingleton<Octo.Services.Imports.SpotifyAuth>();
builder.Services.AddSingleton<Octo.Services.Imports.SpotifyWebApi>();
builder.Services.AddSingleton<Octo.Services.Imports.SpotifyLinkReader>();
builder.Services.AddSingleton<Octo.Services.Imports.ImportMatcher>();
builder.Services.AddSingleton<Octo.Services.Library.LibrarySnapshot>();
builder.Services.AddSingleton<Octo.Services.Library.AlbumOwnership>();
builder.Services.AddSingleton<Octo.Services.Imports.ImportPlaylists>();
builder.Services.AddSingleton<Octo.Services.Imports.TrickleWorker>();
builder.Services.AddSingleton<Octo.Services.Imports.ImportService>();
builder.Services.AddHostedService(sp => sp.GetRequiredService<Octo.Services.Imports.TrickleWorker>());
builder.Services.AddHostedService(sp => sp.GetRequiredService<Octo.Services.Imports.ImportService>());
// The library Review sweep (#72): asks about music that was already there, a few songs an hour,
// only while nothing downloads. Off until LibraryActions:ReviewSweepPerHour is set.
builder.Services.AddSingleton(sp => new Octo.Services.Library.ReviewSweepStore(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "review-sweep.json"),
    sp.GetRequiredService<ILogger<Octo.Services.Library.ReviewSweepStore>>()));
builder.Services.AddSingleton(sp => new Octo.Services.Library.LibraryReviewSweepWorker(
    sp.GetRequiredService<Octo.Services.Library.ReviewSweepStore>(),
    sp.GetRequiredService<Octo.Services.Library.NoticeQueue>(),
    new Octo.Services.Library.FingerprintSweepVerifier(sp),
    sp.GetRequiredService<Octo.Services.Common.IAcquisitionActivity>(),
    sp.GetRequiredService<ILocalLibraryService>(),
    sp.GetRequiredService<Microsoft.Extensions.Options.IOptionsMonitor<LibraryActionSettings>>(),
    sp.GetRequiredService<Microsoft.Extensions.Options.IOptionsMonitor<SubsonicSettings>>(),
    // The resolver's root, the same one review actions resolve inside.
    () => sp.GetRequiredService<Octo.Services.Library.NavidromeSongPathResolver>().MusicRoot(),
    sp.GetRequiredService<ILogger<Octo.Services.Library.LibraryReviewSweepWorker>>()));
builder.Services.AddHostedService(sp => sp.GetRequiredService<Octo.Services.Library.LibraryReviewSweepWorker>());
builder.Services.AddHttpClient(Octo.Services.Fingerprint.MusicBrainzClient.ClientName, c =>
{
    c.BaseAddress = new Uri("https://musicbrainz.org/ws/2/");
    c.Timeout = TimeSpan.FromSeconds(10);
    c.DefaultRequestHeaders.UserAgent.ParseAdd(Octo.Services.Common.OctoUserAgent.Value);
});
builder.Services.AddSingleton<Octo.Services.Fingerprint.MusicBrainzClient>();

builder.Services.AddSingleton<Octo.Services.Audio.ILoudnessMeter, Octo.Services.Audio.LoudnessMeter>();
builder.Services.AddSingleton<Octo.Services.Tagging.ReleaseIdentifier>();
builder.Services.AddSingleton<Octo.Services.Tagging.TagPreview>();
builder.Services.AddSingleton<Octo.Services.Fingerprint.AudioFingerprinter>();
builder.Services.AddSingleton<Octo.Services.Fingerprint.SpectrumAnalyzer>();

// AcoustID allows 3 requests/second and, like Deezer, signals refusal with an error envelope
// rather than reliably a 429. Here that parses as "no match", which this feature reads as
// "accept the file", so going over budget would silently switch verification off.
builder.Services.AddSingleton<Octo.Services.Fingerprint.AcoustIdRateLimiter>();
builder.Services.AddTransient<Octo.Services.Fingerprint.AcoustIdRateLimitHandler>();
builder.Services.AddHttpClient(Octo.Services.Fingerprint.AcoustIdRateLimiter.ClientName, c =>
    {
        c.BaseAddress = new Uri("https://api.acoustid.org/");
        // Verification sits between a finished transfer and the file joining the library.
        // A slow AcoustID must cost seconds, never the download.
        c.Timeout = TimeSpan.FromSeconds(10);
    })
    .ConfigurePrimaryHttpMessageHandler(() => new SocketsHttpHandler
    {
        // meta=...+compress asks AcoustID to gzip the body. Without this it arrives
        // compressed, fails to parse, and reads as "no match" - silently accepting
        // everything, which is the worst outcome this feature can have.
        AutomaticDecompression = System.Net.DecompressionMethods.GZip | System.Net.DecompressionMethods.Deflate,
    })
    .AddHttpMessageHandler<Octo.Services.Fingerprint.AcoustIdRateLimitHandler>();

builder.Services.AddSingleton<Octo.Services.Fingerprint.AcoustIdClient>();
builder.Services.AddSingleton<Octo.Services.Fingerprint.DownloadVerificationService>();

builder.Services.AddSingleton<IMusicMetadataService, SoulseekMetadataService>();
builder.Services.AddSingleton<IDownloadService, SoulseekDownloadService>();
builder.Services.AddSingleton<LidarrClient>();
builder.Services.AddSingleton<ILidarrHeartAcquisitionService, LidarrHeartAcquisitionService>();
// Lidarr as a source for one song at a time (Better quality, wrong song), and which albums a
// heart or an upgrade is working on, so the two never share one.
builder.Services.AddSingleton<Octo.Services.Lidarr.LidarrAlbumClaims>();
builder.Services.AddSingleton<Octo.Services.Lidarr.LidarrImportHandoff>();
builder.Services.AddSingleton<Octo.Services.Lidarr.ILidarrTrackFetcher, Octo.Services.Lidarr.LidarrTrackFetcher>();
builder.Services.AddSingleton<HeartAcquisitionCoordinator>();
builder.Services.AddHostedService<Octo.Services.Common.SoulseekHoldResumer>();

// Discovery results are built once per query and shared. Clients fire several search
// calls for one typed query, and they all resolve to the same routing objects, so without
// this each call re-runs the enrichment pipeline over them concurrently.
builder.Services.AddSingleton<Octo.Services.Common.ExternalSearchService>();

// An artist's most played songs and the chart of the moment, for the apps' search. Built once
// per artist and kept, since every listener searching that artist asks for the same list.
builder.Services.AddSingleton<Octo.Services.Common.TopSongsService>();

// What page one of each search showed, so a later page carries on from it rather than
// building its discovery rows again and repeating or skipping some.
builder.Services.AddSingleton<Octo.Services.Subsonic.SearchSongOrderCache>();

// Who a request is from when it signs in with an API key and so carries no username.
builder.Services.AddSingleton<Octo.Services.Subsonic.RequestIdentity>();

// Checks a sign-in with Navidrome before Octo fetches or plays an outside song for it.
builder.Services.AddSingleton<Octo.Services.Subsonic.CredentialCheck>();

// Completed plays each listener reported lately, so one sent twice is learned from once.
builder.Services.AddSingleton<Octo.Services.Subsonic.RecentScrobbles>();

// Permanent-copy fetches run here, never inside the request that asked for one. A client
// giving up on a slow play must not cancel a transfer slskd is going to finish anyway.
builder.Services.AddSingleton<Octo.Services.Common.TrackAcquisitionQueue>();
// Whether anything is downloading, for the background library jobs that wait until nothing is.
builder.Services.AddSingleton<Octo.Services.Common.IAcquisitionActivity>(sp =>
    new Octo.Services.Common.AcquisitionActivity(sp));
builder.Services.AddHostedService<Octo.Services.Common.AcquisitionWorker>();

// Where each hearted download has got to, for the app's progress ring (getAcquisitions) and
// the dashboard. In memory only; it watches the pipeline and never steers it.
builder.Services.AddSingleton(sp => new Octo.Services.Common.AcquisitionTracker(
    sp.GetRequiredService<ILogger<Octo.Services.Common.AcquisitionTracker>>(), sp));
// Favorites a starred outside song for whoever starred it once Navidrome shows it (#71).
builder.Services.AddSingleton<Octo.Services.Common.StarOnArrival>();
// Find songs in the apps' downloads drawer: a search run again by hand, and the copy picked from
// it, which the Soulseek and Lidarr pipelines take instead of searching.
builder.Services.AddSingleton<Octo.Services.Common.DownloadPicks>();
builder.Services.AddSingleton(sp => new Octo.Services.Common.SongFinder(
    sp, sp.GetRequiredService<ILogger<Octo.Services.Common.SongFinder>>()));

// Long enough for an already-downloaded file to finish being tagged and registered, and
// no longer: sizing this for the transfer itself would tax every restart for a benefit
// that only lands when a download happens to be seconds from done.
builder.Services.Configure<HostOptions>(o => o.ShutdownTimeout = TimeSpan.FromSeconds(10));

builder.Services.AddHttpClient<LastFmService>();
builder.Services.AddSingleton<LastFmService>();

// Push notifications (ntfy / Discord webhook). The orchestrator takes
// IEnumerable<INotificationSink>, so adding a transport is one registration line.
// Short timeout on purpose: a slow notification server must never be felt
// anywhere near the download path.
builder.Services.AddHttpClient(Octo.Services.Notifications.NotificationService.ClientName,
    c => c.Timeout = TimeSpan.FromSeconds(10));
builder.Services.AddSingleton<Octo.Services.Notifications.INotificationSink,
    Octo.Services.Notifications.NtfySink>();
builder.Services.AddSingleton<Octo.Services.Notifications.INotificationSink,
    Octo.Services.Notifications.DiscordSink>();
builder.Services.AddSingleton<Octo.Services.Notifications.NotificationService>();

builder.Services.AddSingleton<IStartupValidator, SubsonicStartupValidator>();
builder.Services.AddSingleton<IStartupValidator, SoulseekStartupValidator>();
builder.Services.AddHostedService<StartupValidationOrchestrator>();

builder.Services.AddHostedService<CacheCleanupService>();

// Pictures in /app/config/covers replace the generated cover of the playlist they are named after.
builder.Services.AddSingleton(sp => new Octo.Services.CoverArt.CoverArtService(
    sp.GetRequiredService<ILogger<Octo.Services.CoverArt.CoverArtService>>(),
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "covers")));
// Cover-art sources, registered in fallback order. The aggregator pulls them
// all out via IEnumerable<ICoverArtSource> and queries them sequentially —
// adding/removing a source is a one-line registration change here.
builder.Services.AddSingleton<Octo.Services.CoverArt.ICoverArtSource, Octo.Services.CoverArt.DeezerCoverArtLookup>();
// Registered as itself too: downloads and the cover upgrade ask it for an album's master.
builder.Services.AddSingleton(sp => new Octo.Services.CoverArt.ITunesCoverArtLookup(
    sp.GetRequiredService<IHttpClientFactory>(), sp.GetRequiredService<ILogger<Octo.Services.CoverArt.ITunesCoverArtLookup>>(),
    // Matches survive a restart, so a library is not sent back to Apple album by album.
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "itunes-masters.json")));
builder.Services.AddSingleton<Octo.Services.CoverArt.ICoverArtSource>(sp =>
    sp.GetRequiredService<Octo.Services.CoverArt.ITunesCoverArtLookup>());
builder.Services.AddSingleton<Octo.Services.CoverArt.ICoverArtSource, Octo.Services.CoverArt.LastFmCoverArtLookup>();
builder.Services.AddSingleton<Octo.Services.CoverArt.CoverArtAggregator>();

// The download-time cover chain (#51). The Cover Art Archive answers a known MusicBrainz release
// directly; it redirects to archive.org, which the default handler follows. Short timeout because
// the whole finalize phase runs under the download lock.
builder.Services.AddHttpClient(Octo.Services.CoverArt.CoverArtArchiveLookup.ClientName, c =>
{
    c.BaseAddress = new Uri("https://coverartarchive.org/");
    c.Timeout = TimeSpan.FromSeconds(8);
    c.DefaultRequestHeaders.UserAgent.ParseAdd(Octo.Services.Common.OctoUserAgent.Value);
});
builder.Services.AddSingleton<Octo.Services.CoverArt.CoverArtArchiveLookup>();
builder.Services.AddSingleton<Octo.Services.CoverArt.DownloadCoverResolver>();

// Lyrics (#52). Sources in the order LYRICS_SOURCES names them; the writer is singleton AND hosted,
// the same instance both ways, so downloads enqueue into the worker the host is running.
// No cookies: NetEase answers a search that carries the cookie its first answer set with
// unrelated popular songs, so with the default handler every search after the first missed.
builder.Services.AddHttpClient(Octo.Services.Lyrics.LrclibLyricsSource.ClientName, c =>
{
    c.Timeout = TimeSpan.FromSeconds(8);
    c.DefaultRequestHeaders.UserAgent.ParseAdd(Octo.Services.Common.OctoUserAgent.Value);
}).ConfigurePrimaryHttpMessageHandler(() => new SocketsHttpHandler { UseCookies = false });
// KuGou's API is unofficial: a short timeout of its own, so a slow or dead KuGou costs a lookup
// a few seconds at most.
builder.Services.AddHttpClient(Octo.Services.Lyrics.KugouLyricsSource.ClientName, c =>
{
    c.Timeout = TimeSpan.FromSeconds(6);
    c.DefaultRequestHeaders.UserAgent.ParseAdd(Octo.Services.Common.OctoUserAgent.Value);
}).ConfigurePrimaryHttpMessageHandler(() => new SocketsHttpHandler { UseCookies = false });
builder.Services.AddSingleton<Octo.Services.Lyrics.ILyricsSource, Octo.Services.Lyrics.KugouLyricsSource>();
builder.Services.AddSingleton<Octo.Services.Lyrics.ILyricsSource, Octo.Services.Lyrics.LrclibLyricsSource>();
builder.Services.AddSingleton<Octo.Services.Lyrics.ILyricsSource, Octo.Services.Lyrics.NeteaseLyricsSource>();
builder.Services.AddSingleton<Octo.Services.Lyrics.ILyricsSource, Octo.Services.Lyrics.LyricsOvhLyricsSource>();
builder.Services.AddSingleton<Octo.Services.Lyrics.LyricsService>();
// Lyrics someone chose by hand, for every client. Beside the settings, like the other state
// that is a person's decision rather than a cache.
builder.Services.AddSingleton(sp => new Octo.Services.Lyrics.LyricsChoiceStore(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "lyrics-choices.json"),
    sp.GetRequiredService<ILogger<Octo.Services.Lyrics.LyricsChoiceStore>>()));
builder.Services.AddSingleton<Octo.Services.Lyrics.LyricsChoiceService>();
builder.Services.AddSingleton<Octo.Services.Lyrics.LyricsSidecarWriter>();
builder.Services.AddHostedService(sp => sp.GetRequiredService<Octo.Services.Lyrics.LyricsSidecarWriter>());
// "Find lyrics for the library": its progress beside the settings, so a stop or a restart can
// be resumed from where it was.
builder.Services.AddSingleton(sp => new Octo.Services.Lyrics.LyricsLibraryStore(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "lyrics-library.json"),
    sp.GetRequiredService<ILogger<Octo.Services.Lyrics.LyricsLibraryStore>>()));
// What the lyrics page's Save wrote over, so Undo can put it back.
builder.Services.AddSingleton(new Octo.Services.Lyrics.LyricsUndoJournal(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "lyrics-undo.jsonl")));
builder.Services.AddSingleton<Octo.Services.Lyrics.LyricsLibraryWorker>();
builder.Services.AddHostedService(sp => sp.GetRequiredService<Octo.Services.Lyrics.LyricsLibraryWorker>());

builder.Services.AddCors(options =>
{
    options.AddDefaultPolicy(policy =>
    {
        policy.AllowAnyOrigin()
            .AllowAnyMethod()
            .AllowAnyHeader()
            .WithExposedHeaders("X-Content-Duration", "X-Total-Count", "X-Nd-Authorization");
    });
});

// The dashboard on a port of its own, when asked for. Kestrel takes one list of addresses, so the
// admin port is added to the ones Octo already listens on; Listen() in code would REPLACE them.
var adminPort = builder.Configuration.GetValue<int>("Admin:Port");
if (AdminPortSplit.ListenUrls(builder.Configuration["urls"], builder.Configuration["http_ports"], adminPort) is { } listen)
    builder.WebHost.UseUrls(listen);
else if (adminPort > 0)
{
    Console.Error.WriteLine($"Admin:Port {adminPort} is already one of Octo's own ports, so the dashboard keeps sharing it.");
    adminPort = 0;
}

var app = builder.Build();

// Resolved here so it snapshots the values this process actually started with, before the
// dashboard or first-run automation can change anything.
app.Services.GetRequiredService<Octo.Services.Admin.RestartTracker>();

// Make the recovery code exist, and say where it is. While no Navidrome is set there is no other
// way in, so the code itself goes to the log too.
{
    var adminOpts = app.Services.GetRequiredService<Microsoft.Extensions.Options.IOptionsMonitor<AdminSettings>>().CurrentValue;
    var startLog = app.Services.GetRequiredService<ILogger<Program>>();
    if (adminOpts.SignIn?.Trim().ToLowerInvariant() is not ("required" or "off"))
        startLog.LogWarning("Admin:SignIn is \"{Value}\", which Octo does not know, so the dashboard sign-in stays on.", adminOpts.SignIn);
    if (adminOpts.SignInOff)
        startLog.LogWarning("The dashboard sign-in is OFF (ADMIN_SIGN_IN=off). Anyone who can reach Octo can change it.");
    else
    {
        var recovery = app.Services.GetRequiredService<Octo.Services.Admin.AdminRecoveryCode>();
        var code = recovery.Current();
        if (string.IsNullOrWhiteSpace(app.Configuration["Subsonic:Url"]) && code is not null)
            startLog.LogWarning("No Navidrome is set yet, so sign in to the dashboard with the recovery code {Code} (also in {Path}).", code, recovery.FilePath);
        else
            startLog.LogInformation("Locked out of the dashboard? The recovery code is in {Path}.", recovery.FilePath);
    }
    if (adminPort > 0)
        startLog.LogInformation("The dashboard answers only on port {Port} now; Octo's own port refuses it.", adminPort);
}

// Built now rather than on the first Subsonic request, so it is already listening when the
// first download finishes.
app.Services.GetRequiredService<Octo.Services.Common.StarOnArrival>();

// The first list cover loads the fonts and finds the system's fallbacks, which takes a second
// or two; done here in the background so no client waits for it.
_ = Task.Run(() => app.Services.GetRequiredService<Octo.Services.CoverArt.CoverArtService>().Warm());

// First-run automation (best-effort, background). Octo is an accessory to an
// existing Navidrome, so it self-configures what it can: if no upstream URL is
// set, scan the LAN and adopt the server when exactly one is found; then detect
// the music folder from it. Anything ambiguous (several servers, none found) is
// left for the dashboard so we never silently point at the wrong server.
_ = Task.Run(async () =>
{
    var sp = app.Services;
    var log = sp.GetRequiredService<ILogger<Program>>();
    try
    {
        var subOpts = sp.GetRequiredService<Microsoft.Extensions.Options.IOptionsMonitor<Octo.Models.Settings.SubsonicSettings>>();
        if (string.IsNullOrWhiteSpace(subOpts.CurrentValue.Url))
        {
            var servers = await sp.GetRequiredService<Octo.Services.Subsonic.SubsonicDiscoveryService>().ScanAsync();
            if (servers.Count == 1)
            {
                sp.GetRequiredService<Octo.Services.Admin.SettingsFileWriter>().Merge(
                    new System.Text.Json.Nodes.JsonObject
                    {
                        ["Subsonic"] = new System.Text.Json.Nodes.JsonObject { ["Url"] = servers[0].Url }
                    });
                // The URL is a restart-required setting (services bind it via IOptions
                // at startup), so restart cleanly to apply it. A supervised deploy
                // (compose restart policy / systemd) brings Octo straight back, now
                // with the URL loaded; on next boot the URL is set so this is skipped.
                log.LogInformation("First-run: auto-configured Navidrome URL -> {Url} ({Type} {Ver}). Restarting to apply.",
                    servers[0].Url, servers[0].Type, servers[0].ServerVersion);
                sp.GetRequiredService<IHostApplicationLifetime>().StopApplication();
                return;
            }
            else if (servers.Count > 1)
                log.LogInformation("First-run: {N} servers found; pick one in the dashboard.", servers.Count);
            else
                log.LogInformation("First-run: no Navidrome auto-detected; set the URL in the dashboard.");
        }
    }
    catch (Exception ex) { log.LogWarning("First-run server auto-detect failed: {Msg}", ex.Message); }

    // Detect the music folder from whatever URL we now have (configured or adopted).
    await sp.GetRequiredService<Octo.Services.Subsonic.NavidromeIdentityService>()
        .DetectMusicFolderAsync(force: true);
});

// This must run before any middleware or controller reads Request.Scheme.
app.UseForwardedHeaders();
// Before anything else answers, so the wrong port never even serves a static file.
app.UseAdminPortSplit(adminPort);
app.UseAdminSecurityHeaders();
app.UseExceptionHandler(_ => { });
// Ahead of UseCors on purpose: it strips the CORS headers from /api/admin answers and refuses
// admin writes that lack X-Octo-Admin. See AdminRequestGuard for what it does and does not stop.
app.UseAdminRequestGuard();
// Then the sign-in itself: nothing under /api/admin answers a caller who is not signed in.
app.UseAdminSignInGate();

// Capture the raw request body for body-carrying methods so the proxy can
// faithfully forward it after parameter extraction has consumed/closed the
// stream (needed for relayed native endpoints like POST /auth/login).
app.Use(async (ctx, next) =>
{
    var m = ctx.Request.Method;
    if (HttpMethods.IsPost(m) || HttpMethods.IsPut(m) || HttpMethods.IsPatch(m))
    {
        ctx.Request.EnableBuffering();
        using var ms = new MemoryStream();
        await ctx.Request.Body.CopyToAsync(ms);
        ctx.Items["Octo.RawBody"] = ms.ToArray();
        ctx.Request.Body.Position = 0; // rewind so form/model reading still works
    }
    await next(ctx);
});

if (app.Environment.IsDevelopment())
{
    app.UseSwagger();
    app.UseSwaggerUI();
}

// HTTPS redirection intentionally removed: Octo terminates HTTP-only inside
// the docker network and behind whatever reverse proxy / Cloudflare tunnel
// the user fronts it with. Forcing HTTPS here just turned every /admin/ asset
// into a redirect to a port we don't bind, which got swallowed by the
// catch-all SubsonicController and returned as Navidrome HTML.

// Serve the admin UI from wwwroot/admin/ as static files. The MVC controller
// at /admin (no slash) redirects to /admin/ so both paths work. We register
// both MapStaticAssets() (.NET 9's manifest-based endpoint approach) and
// UseStaticFiles (the classic file-system middleware) so either path can
// claim the request before the SubsonicController catch-all sees it.
app.MapStaticAssets();
app.UseDefaultFiles();
app.UseStaticFiles();
// The Octo logo lives in /app/Assets (copied into the publish output via the
// csproj). Expose it under /Assets/* so the admin UI can use it without us
// duplicating the file under wwwroot.
var assetsDir = Path.Combine(AppContext.BaseDirectory, "Assets");
if (Directory.Exists(assetsDir))
{
    app.UseStaticFiles(new StaticFileOptions
    {
        FileProvider = new Microsoft.Extensions.FileProviders.PhysicalFileProvider(assetsDir),
        RequestPath = "/Assets",
    });
}
app.UseAuthorization();
app.UseCors();
app.MapControllers();

app.Run();
