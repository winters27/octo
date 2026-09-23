using Microsoft.Extensions.Configuration;
using Octo.Services.Admin;

namespace Octo.Tests;

/// <summary>
/// The dashboard's "restart to apply" banner is computed from this, so it has to name a changed
/// startup-only setting and nothing else.
/// </summary>
public class RestartTrackerTests
{
    private static IConfigurationRoot Config(params (string Key, string? Value)[] values) =>
        new ConfigurationBuilder()
            .AddInMemoryCollection(values.ToDictionary(v => v.Key, v => v.Value))
            .Build();

    [Fact]
    public void Pending_IsEmptyAtStartup()
    {
        var config = Config(("Soulseek:Password", "secret"), ("Library:DownloadPath", "/music"));
        Assert.Empty(new RestartTracker(config).Pending(config));
    }

    [Fact]
    public void Pending_NamesAChangedKey()
    {
        var config = Config(("Soulseek:Password", "secret"));
        var tracker = new RestartTracker(config);

        config["Soulseek:Password"] = "changed";

        Assert.Equal(["Soulseek:Password"], tracker.Pending(config));
    }

    /// <summary>A hot-reload setting is not a restart setting, however it changes.</summary>
    [Fact]
    public void Pending_IgnoresSettingsThatApplyLive()
    {
        var config = Config(("Genre:MaxGenres", "10"));
        var tracker = new RestartTracker(config);

        config["Genre:MaxGenres"] = "1";

        Assert.Empty(tracker.Pending(config));
    }

    [Fact]
    public void Pending_IgnoresBooleanCasing()
    {
        var config = Config(("LibraryActions:Enabled", "True"));
        var tracker = new RestartTracker(config);

        config["LibraryActions:Enabled"] = "true";

        Assert.Empty(tracker.Pending(config));
    }

    /// <summary>Only booleans compare without case. A password or a Linux path that changes only
    /// in case has changed, and still needs the restart.</summary>
    [Fact]
    public void Pending_NamesACaseOnlyChangeToAString()
    {
        var config = Config(("Soulseek:Password", "secret"));
        var tracker = new RestartTracker(config);

        config["Soulseek:Password"] = "Secret";

        Assert.Equal(["Soulseek:Password"], tracker.Pending(config));
    }

    [Fact]
    public void Pending_TreatsEmptyAndMissingAlike()
    {
        var config = Config();
        var tracker = new RestartTracker(config);

        config["YouTube:ShimUrl"] = "  ";

        Assert.Empty(tracker.Pending(config));
    }
}
