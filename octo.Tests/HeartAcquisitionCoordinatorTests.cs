using Microsoft.Extensions.Logging;
using Moq;
using Octo.Models.Settings;
using Octo.Services;
using Octo.Services.Common;
using Octo.Services.Lidarr;

namespace Octo.Tests;

public class HeartAcquisitionCoordinatorTests
{
    [Fact]
    public async Task LidarrSourceRoutesTrackAndAlbumWithoutCallingDirectDownloader()
    {
        var lidarr = new Mock<ILidarrHeartAcquisitionService>();
        var direct = new Mock<IDownloadService>();
        lidarr.Setup(x => x.TryAcquireTrackAsync("soulseek", "track-id", true))
            .ReturnsAsync(true);
        lidarr.Setup(x => x.TryAcquireAlbumAsync("soulseek", "album-id", true))
            .ReturnsAsync(true);
        var queue = new TrackAcquisitionQueue(new Mock<ILogger<TrackAcquisitionQueue>>().Object);
        var coordinator = new HeartAcquisitionCoordinator(
            TestOptions.Monitor(new SubsonicSettings { DownloadSource = DownloadSource.Lidarr }),
            queue, direct.Object, lidarr.Object);

        await coordinator.AcquireTrackAsync("soulseek", "track-id");
        await coordinator.AcquireAlbumAsync("soulseek", "album-id");

        lidarr.Verify(x => x.TryAcquireTrackAsync("soulseek", "track-id", true), Times.Once);
        lidarr.Verify(x => x.TryAcquireAlbumAsync("soulseek", "album-id", true), Times.Once);
        direct.Verify(x => x.DownloadRemainingAlbumTracksInBackground(
            It.IsAny<string>(), It.IsAny<string>(), It.IsAny<string>()), Times.Never);
    }

    [Fact]
    public async Task SoulseekSourceKeepsAlbumOnExistingDirectPath()
    {
        var lidarr = new Mock<ILidarrHeartAcquisitionService>();
        var direct = new Mock<IDownloadService>();
        direct.Setup(x => x.DownloadAlbumWithSourceAsync(
                "soulseek", "album-id", DownloadSource.Soulseek, false, It.IsAny<CancellationToken>()))
            .ReturnsAsync(true);
        var queue = new TrackAcquisitionQueue(new Mock<ILogger<TrackAcquisitionQueue>>().Object);
        var coordinator = new HeartAcquisitionCoordinator(
            TestOptions.Monitor(new SubsonicSettings { DownloadSource = DownloadSource.Soulseek }),
            queue, direct.Object, lidarr.Object);

        await coordinator.AcquireAlbumAsync("soulseek", "album-id");

        direct.Verify(x => x.DownloadAlbumWithSourceAsync(
            "soulseek", "album-id", DownloadSource.Soulseek, false,
            It.IsAny<CancellationToken>()), Times.Once);
        lidarr.VerifyNoOtherCalls();
    }

    [Fact]
    public async Task DownloadSourceChangeTakesEffectWithoutRebuildingCoordinator()
    {
        var lidarr = new Mock<ILidarrHeartAcquisitionService>();
        var direct = new Mock<IDownloadService>();
        direct.Setup(x => x.DownloadAlbumWithSourceAsync(
                "soulseek", "first-album", DownloadSource.Soulseek, false,
                It.IsAny<CancellationToken>()))
            .ReturnsAsync(true);
        lidarr.Setup(x => x.TryAcquireAlbumAsync("soulseek", "second-album", true))
            .ReturnsAsync(true);
        var queue = new TrackAcquisitionQueue(new Mock<ILogger<TrackAcquisitionQueue>>().Object);
        var settings = TestOptions.Monitor(
            new SubsonicSettings { DownloadSource = DownloadSource.Soulseek });
        var coordinator = new HeartAcquisitionCoordinator(settings, queue, direct.Object, lidarr.Object);

        await coordinator.AcquireAlbumAsync("soulseek", "first-album");
        settings.Set(new SubsonicSettings { DownloadSource = DownloadSource.Lidarr });
        await coordinator.AcquireAlbumAsync("soulseek", "second-album");

        direct.Verify(x => x.DownloadAlbumWithSourceAsync(
            "soulseek", "first-album", DownloadSource.Soulseek, false,
            It.IsAny<CancellationToken>()), Times.Once);
        lidarr.Verify(x => x.TryAcquireAlbumAsync("soulseek", "second-album", true), Times.Once);
    }

    [Fact]
    public async Task AlbumPriorityStopsAtFirstSuccessfulSource()
    {
        var lidarr = new Mock<ILidarrHeartAcquisitionService>();
        var direct = new Mock<IDownloadService>();
        direct.Setup(x => x.DownloadAlbumWithSourceAsync(
                "soulseek", "album-id", DownloadSource.Soulseek, true,
                It.IsAny<CancellationToken>()))
            .ReturnsAsync(true);
        var settings = TestOptions.Monitor(new SubsonicSettings
        {
            HeartDownloadSources =
            [
                new() { Source = HeartDownloadSource.YouTube, Enabled = false },
                new() { Source = HeartDownloadSource.Soulseek, Enabled = true },
                new() { Source = HeartDownloadSource.Lidarr, Enabled = true },
            ],
        });
        var coordinator = new HeartAcquisitionCoordinator(settings,
            new TrackAcquisitionQueue(new Mock<ILogger<TrackAcquisitionQueue>>().Object),
            direct.Object, lidarr.Object);

        await coordinator.AcquireAlbumAsync("soulseek", "album-id");

        lidarr.VerifyNoOtherCalls();
    }

    [Fact]
    public async Task AlbumPriorityFallsThroughToLidarrAfterDirectFailure()
    {
        var lidarr = new Mock<ILidarrHeartAcquisitionService>();
        var direct = new Mock<IDownloadService>();
        direct.Setup(x => x.DownloadAlbumWithSourceAsync(
                "soulseek", "album-id", DownloadSource.Soulseek, true,
                It.IsAny<CancellationToken>()))
            .ReturnsAsync(false);
        lidarr.Setup(x => x.TryAcquireAlbumAsync("soulseek", "album-id", true))
            .ReturnsAsync(true);
        var settings = TestOptions.Monitor(new SubsonicSettings
        {
            HeartDownloadSources =
            [
                new() { Source = HeartDownloadSource.Soulseek, Enabled = true },
                new() { Source = HeartDownloadSource.YouTube, Enabled = false },
                new() { Source = HeartDownloadSource.Lidarr, Enabled = true },
            ],
        });
        var coordinator = new HeartAcquisitionCoordinator(settings,
            new TrackAcquisitionQueue(new Mock<ILogger<TrackAcquisitionQueue>>().Object),
            direct.Object, lidarr.Object);

        await coordinator.AcquireAlbumAsync("soulseek", "album-id");

        lidarr.Verify(x => x.TryAcquireAlbumAsync("soulseek", "album-id", true), Times.Once);
        direct.Verify(x => x.DownloadAlbumWithSourceAsync(
            It.IsAny<string>(), It.IsAny<string>(), DownloadSource.YouTube,
            It.IsAny<bool>(), It.IsAny<CancellationToken>()), Times.Never);
    }

    [Fact]
    public async Task AlbumPriorityTriesDirectSourcesInOrderAndStopsOnSuccess()
    {
        var attempts = new List<DownloadSource>();
        var direct = new Mock<IDownloadService>();
        direct.Setup(x => x.DownloadAlbumWithSourceAsync(
                "soulseek", "album-id", It.IsAny<DownloadSource>(),
                It.IsAny<bool>(), It.IsAny<CancellationToken>()))
            .ReturnsAsync((string _, string _, DownloadSource source, bool _, CancellationToken _) =>
            {
                attempts.Add(source);
                return source == DownloadSource.YouTube;
            });
        var lidarr = new Mock<ILidarrHeartAcquisitionService>();
        var settings = TestOptions.Monitor(new SubsonicSettings
        {
            HeartDownloadSources =
            [
                new() { Source = HeartDownloadSource.Soulseek, Enabled = true },
                new() { Source = HeartDownloadSource.YouTube, Enabled = true },
                new() { Source = HeartDownloadSource.Lidarr, Enabled = true },
            ],
        });
        var coordinator = new HeartAcquisitionCoordinator(settings,
            new TrackAcquisitionQueue(new Mock<ILogger<TrackAcquisitionQueue>>().Object),
            direct.Object, lidarr.Object);

        await coordinator.AcquireAlbumAsync("soulseek", "album-id");

        Assert.Equal([DownloadSource.Soulseek, DownloadSource.YouTube], attempts);
        lidarr.VerifyNoOtherCalls();
    }

    [Fact]
    public void LegacyFallbackMapsToOrderedSourcesWithLidarrLast()
    {
        var settings = new SubsonicSettings { DownloadSource = DownloadSource.SoulseekThenYouTube };

        var steps = settings.EffectiveHeartDownloadSources();

        Assert.Collection(steps,
            step => { Assert.Equal(HeartDownloadSource.Soulseek, step.Source); Assert.True(step.SongEnabled); Assert.True(step.AlbumEnabled); },
            step => { Assert.Equal(HeartDownloadSource.YouTube, step.Source); Assert.True(step.SongEnabled); Assert.True(step.AlbumEnabled); },
            step => { Assert.Equal(HeartDownloadSource.Lidarr, step.Source); Assert.False(step.SongEnabled); Assert.False(step.AlbumEnabled); });
    }

    [Fact]
    public async Task TrackPriorityFallsThroughFromSoulseekToLidarr()
    {
        var lidarr = new Mock<ILidarrHeartAcquisitionService>();
        lidarr.Setup(x => x.TryAcquireTrackAsync("soulseek", "track-id", true))
            .ReturnsAsync(true);
        var queue = new TrackAcquisitionQueue(new Mock<ILogger<TrackAcquisitionQueue>>().Object);
        var settings = TestOptions.Monitor(new SubsonicSettings
        {
            HeartDownloadSources =
            [
                new() { Source = HeartDownloadSource.Soulseek, Enabled = true },
                new() { Source = HeartDownloadSource.YouTube, Enabled = false },
                new() { Source = HeartDownloadSource.Lidarr, Enabled = true },
            ],
        });
        var coordinator = new HeartAcquisitionCoordinator(
            settings, queue, new Mock<IDownloadService>().Object, lidarr.Object);

        var acquisition = coordinator.AcquireTrackAsync("soulseek", "track-id");
        var request = await queue.DequeueAsync(CancellationToken.None);
        Assert.NotNull(request);
        Assert.Equal(DownloadSource.Soulseek, request.SourceOverride);
        Assert.False(request.NotifyOnFailure);
        queue.Release(request);
        request.Completion.TrySetException(new InvalidOperationException("no peer"));
        await acquisition;

        lidarr.Verify(x => x.TryAcquireTrackAsync("soulseek", "track-id", true), Times.Once);
    }

    [Fact]
    public void ConfiguredOrderIsPreservedAndMissingSourcesAreAppendedDisabled()
    {
        var settings = new SubsonicSettings
        {
            HeartDownloadSources =
            [
                new() { Source = HeartDownloadSource.Lidarr, Enabled = true },
                new() { Source = HeartDownloadSource.YouTube, Enabled = true },
            ],
        };

        var steps = settings.EffectiveHeartDownloadSources();

        Assert.Collection(steps,
            step => Assert.Equal(HeartDownloadSource.Lidarr, step.Source),
            step => Assert.Equal(HeartDownloadSource.YouTube, step.Source),
            step => { Assert.Equal(HeartDownloadSource.Soulseek, step.Source); Assert.False(step.SongEnabled); Assert.False(step.AlbumEnabled); });
    }

    [Fact]
    public async Task SongAndAlbumHeartsUseTheirOwnPerSourceSwitches()
    {
        var lidarr = new Mock<ILidarrHeartAcquisitionService>();
        lidarr.Setup(x => x.TryAcquireAlbumAsync("soulseek", "album-id", true))
            .ReturnsAsync(true);
        var direct = new Mock<IDownloadService>();
        var queue = new TrackAcquisitionQueue(new Mock<ILogger<TrackAcquisitionQueue>>().Object);
        var settings = TestOptions.Monitor(new SubsonicSettings
        {
            HeartDownloadSources =
            [
                new() { Source = HeartDownloadSource.Soulseek, SongEnabled = true, AlbumEnabled = false },
                new() { Source = HeartDownloadSource.Lidarr, SongEnabled = false, AlbumEnabled = true },
                new() { Source = HeartDownloadSource.YouTube, SongEnabled = false, AlbumEnabled = false },
            ],
        });
        var coordinator = new HeartAcquisitionCoordinator(settings, queue, direct.Object, lidarr.Object);

        var trackAcquisition = coordinator.AcquireTrackAsync("soulseek", "track-id");
        var request = await queue.DequeueAsync(CancellationToken.None);
        Assert.NotNull(request);
        Assert.Equal(DownloadSource.Soulseek, request.SourceOverride);
        request.Completion.TrySetResult("/music/track.flac");
        queue.Release(request);
        await trackAcquisition;
        await coordinator.AcquireAlbumAsync("soulseek", "album-id");

        lidarr.Verify(x => x.TryAcquireTrackAsync(
            It.IsAny<string>(), It.IsAny<string>(), It.IsAny<bool>()), Times.Never);
        lidarr.Verify(x => x.TryAcquireAlbumAsync("soulseek", "album-id", true), Times.Once);
    }
}
