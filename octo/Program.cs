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
builder.Services.AddSingleton<Octo.Services.Admin.SettingsFileWriter>(
    sp => new Octo.Services.Admin.SettingsFileWriter(SettingsFilePath));
// Running log of fetched songs, stored next to the settings file (same
// bind-mounted config dir, so it survives restarts).
builder.Services.AddSingleton(sp => new Octo.Services.Local.DownloadHistoryService(
    System.IO.Path.Combine(System.IO.Path.GetDirectoryName(SettingsFilePath)!, "downloads-history.json"),
    sp.GetRequiredService<ILogger<Octo.Services.Local.DownloadHistoryService>>()));

builder.Services.AddExceptionHandler<GlobalExceptionHandler>();
builder.Services.AddProblemDetails();

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

builder.Services.AddSingleton<ILocalLibraryService, LocalLibraryService>();

builder.Services.AddSingleton<SubsonicRequestParser>();
builder.Services.AddSingleton<SubsonicResponseBuilder>();
builder.Services.AddSingleton<SubsonicModelMapper>();
builder.Services.AddScoped<SubsonicProxyService>();

// Soulseek (FLAC source) + YouTube (instant-preview stream source).
builder.Services.AddSingleton<SoulseekClient>();
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
builder.Services.AddSingleton<ExternalIdRegistry>();
builder.Services.AddSingleton<RadioQueueStore>();
builder.Services.AddSingleton<Octo.Services.Subsonic.NavidromeIdentityService>();
builder.Services.AddSingleton<Octo.Services.Subsonic.SubsonicDiscoveryService>();
builder.Services.AddSingleton<Octo.Services.Admin.DirectoryBrowser>();
// Singleton so browse tokens survive between requests; they are in-memory only,
// so a restart ends every browse session, which is the right trade for a token
// that grants filesystem visibility.
builder.Services.AddSingleton<Octo.Services.Admin.BrowseSessionStore>();
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

builder.Services.AddSingleton<IMusicMetadataService, SoulseekMetadataService>();
builder.Services.AddSingleton<IDownloadService, SoulseekDownloadService>();
builder.Services.AddSingleton<LidarrClient>();
builder.Services.AddSingleton<ILidarrHeartAcquisitionService, LidarrHeartAcquisitionService>();
builder.Services.AddSingleton<HeartAcquisitionCoordinator>();

// Discovery results are built once per query and shared. Clients fire several search
// calls for one typed query, and they all resolve to the same routing objects, so without
// this each call re-runs the enrichment pipeline over them concurrently.
builder.Services.AddSingleton<Octo.Services.Common.ExternalSearchService>();

// Permanent-copy fetches run here, never inside the request that asked for one. A client
// giving up on a slow play must not cancel a transfer slskd is going to finish anyway.
builder.Services.AddSingleton<Octo.Services.Common.TrackAcquisitionQueue>();
builder.Services.AddHostedService<Octo.Services.Common.AcquisitionWorker>();

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

builder.Services.AddSingleton<Octo.Services.CoverArt.CoverArtService>();
// Cover-art sources, registered in fallback order. The aggregator pulls them
// all out via IEnumerable<ICoverArtSource> and queries them sequentially —
// adding/removing a source is a one-line registration change here.
builder.Services.AddSingleton<Octo.Services.CoverArt.ICoverArtSource, Octo.Services.CoverArt.DeezerCoverArtLookup>();
builder.Services.AddSingleton<Octo.Services.CoverArt.ICoverArtSource, Octo.Services.CoverArt.ITunesCoverArtLookup>();
builder.Services.AddSingleton<Octo.Services.CoverArt.ICoverArtSource, Octo.Services.CoverArt.LastFmCoverArtLookup>();
builder.Services.AddSingleton<Octo.Services.CoverArt.CoverArtAggregator>();

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

var app = builder.Build();

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

app.UseExceptionHandler(_ => { });

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
