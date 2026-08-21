using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Moq;
using Octo.Models.Settings;
using Octo.Services;
using Octo.Services.Common;
using Octo.Services.Local;

namespace Octo.Tests;

public sealed class ExternalPlaybackTests
{
    [Fact]
    public async Task ExternalPlaybackUsesYouTubeRegardlessOfLegacyStorageMode()
    {
        var downloads = new Mock<IDownloadService>();
        downloads.Setup(service => service.GetDirectStreamAsync(
                "soulseek", "track-id", null,
                It.IsAny<CancellationToken>()))
            .ReturnsAsync(new DirectStreamInfo
            {
                AudioStream = new MemoryStream([1, 2, 3]),
                ContentType = "audio/mp4",
                ContentLength = 3,
                StatusCode = 200,
            });
        var library = new Mock<ILocalLibraryService>();
        library.Setup(service => service.ParseSongId("external-track"))
            .Returns((true, "soulseek", "track-id"));

        await using var factory = CreateFactory(downloads, library, waitForLossless: false);

        using var client = factory.CreateClient();
        using var response = await client.GetAsync("/rest/stream?id=external-track&f=json");

        response.EnsureSuccessStatusCode();
        Assert.Equal([1, 2, 3], await response.Content.ReadAsByteArrayAsync());
        downloads.Verify(service => service.GetDirectStreamAsync(
            "soulseek", "track-id", null,
            It.IsAny<CancellationToken>()), Times.Once);
        downloads.Verify(service => service.DownloadAndStreamAsync(
            It.IsAny<string>(), It.IsAny<string>(), It.IsAny<CancellationToken>()), Times.Never);
    }

    [Fact]
    public async Task WaitForLosslessStillAcquiresBeforePlaybackWhenEnabled()
    {
        var downloads = new Mock<IDownloadService>();
        var library = new Mock<ILocalLibraryService>();
        library.Setup(service => service.ParseSongId("external-track"))
            .Returns((true, "soulseek", "track-id"));

        await using var factory = CreateFactory(downloads, library, waitForLossless: true);
        using var client = factory.CreateClient();
        var responseTask = client.GetAsync("/rest/stream?id=external-track&f=json");

        var queue = factory.Services.GetRequiredService<TrackAcquisitionQueue>();
        using var dequeueTimeout = new CancellationTokenSource(TimeSpan.FromSeconds(5));
        var request = await queue.DequeueAsync(dequeueTimeout.Token);
        Assert.NotNull(request);
        Assert.False(request.IsStar);
        Assert.False(request.TriggerAlbumDownload);
        Assert.True(request.ForcePermanent);

        var path = Path.Combine(Path.GetTempPath(), $"octo-playback-{Guid.NewGuid():N}.flac");
        try
        {
            await File.WriteAllBytesAsync(path, [4, 5, 6]);
            request.Completion.TrySetResult(path);
            queue.Release(request);

            using var response = await responseTask;
            response.EnsureSuccessStatusCode();
            Assert.Equal([4, 5, 6], await response.Content.ReadAsByteArrayAsync());
            downloads.Verify(service => service.GetDirectStreamAsync(
                It.IsAny<string>(), It.IsAny<string>(), It.IsAny<string?>(),
                It.IsAny<CancellationToken>()), Times.Never);
        }
        finally
        {
            File.Delete(path);
        }
    }

    private static WebApplicationFactory<Program> CreateFactory(
        Mock<IDownloadService> downloads,
        Mock<ILocalLibraryService> library,
        bool waitForLossless)
    {
        return new WebApplicationFactory<Program>()
            .WithWebHostBuilder(builder =>
            {
                builder.ConfigureAppConfiguration((_, config) =>
                    config.AddInMemoryCollection(new Dictionary<string, string?>
                    {
                        ["Subsonic:Url"] = "http://navidrome.invalid",
                        ["Subsonic:StorageMode"] = "Cache",
                        ["Subsonic:DownloadSource"] = "Soulseek",
                        ["Subsonic:WaitForLosslessOnPlay"] = waitForLossless.ToString(),
                        ["Library:DownloadPath"] = Path.GetTempPath(),
                    }));
                builder.ConfigureServices(services =>
                {
                    services.RemoveAll<IHostedService>();
                    services.RemoveAll<IDownloadService>();
                    services.RemoveAll<ILocalLibraryService>();
                    services.AddSingleton(downloads.Object);
                    services.AddSingleton(library.Object);
                });
            });
    }
}
