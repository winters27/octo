using Octo.Models.Settings;
using Octo.Services;
using Octo.Services.Soulseek;
using Octo.Services.YouTube;
using Octo.Services.Local;
using Octo.Services.Validation;
using Octo.Services.Subsonic;
using Octo.Services.Common;
using Octo.Services.LastFm;
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

builder.Services.AddExceptionHandler<GlobalExceptionHandler>();
builder.Services.AddProblemDetails();

builder.Services.Configure<SubsonicSettings>(
    builder.Configuration.GetSection("Subsonic"));
builder.Services.Configure<SoulseekSettings>(
    builder.Configuration.GetSection("Soulseek"));
builder.Services.Configure<LastFmSettings>(
    builder.Configuration.GetSection("LastFm"));

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
    // the shim's MAX_CONCURRENT_YTDLP=8 gate and queue requests behind 5-8s
    // yt-dlp ytsearch1: invocations. 30s was canceling the tail of every
    // prewarm batch.
    c.Timeout = TimeSpan.FromSeconds(60);
});
builder.Services.AddHttpClient(YouTubeResolver.StreamClientName, c =>
{
    c.Timeout = Timeout.InfiniteTimeSpan;
});
builder.Services.AddSingleton<ExternalIdRegistry>();
builder.Services.AddSingleton<RadioQueueStore>();
builder.Services.AddSingleton<IMusicMetadataService, SoulseekMetadataService>();
builder.Services.AddSingleton<IDownloadService, SoulseekDownloadService>();

builder.Services.AddHttpClient<LastFmService>();
builder.Services.AddSingleton<LastFmService>();

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

app.UseExceptionHandler(_ => { });

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
