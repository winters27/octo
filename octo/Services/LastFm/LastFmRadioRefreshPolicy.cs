using Octo.Models.Radio;
using Octo.Models.Settings;

namespace Octo.Services.LastFm;

public static class LastFmRadioRefreshPolicy
{
    private static readonly TimeSpan FailureRetryDelay = TimeSpan.FromMinutes(15);

    public static bool IsStale(LastFmRadioUserState user, LastFmSettings settings,
        DateTime? nowUtc = null) => !user.LastRefreshSuccessUtc.HasValue
        || user.LastRefreshSuccessUtc < (nowUtc ?? DateTime.UtcNow)
            .AddHours(-settings.EffectiveRefreshIntervalHours);

    public static bool ShouldRefreshAfterPlay(LastFmRadioUserState user, LastFmSettings settings,
        DateTime? nowUtc = null)
    {
        var learnedCount = user.Plays.Count(play => play.LearnedSignal);
        return user.Stations.Count == 0 || IsStale(user, settings, nowUtc)
            || user.NewPlaysSinceRefresh >= Math.Max(3, settings.EffectiveMinimumPlays / 2)
            || learnedCount == settings.EffectiveMinimumPlays;
    }

    public static bool ShouldSchedulePeriodicRefresh(LastFmRadioUserState user,
        LastFmSettings settings, DateTime? nowUtc = null)
    {
        var now = nowUtc ?? DateTime.UtcNow;
        if (user.Refreshing || !IsStale(user, settings, now)) return false;
        return user.LastRefreshError is null || !user.LastRefreshAttemptUtc.HasValue
            || user.LastRefreshAttemptUtc <= now.Subtract(FailureRetryDelay);
    }

    public static TimeSpan StartupJitter(string username)
    {
        var hash = System.Security.Cryptography.SHA256.HashData(
            System.Text.Encoding.UTF8.GetBytes(username.Trim().ToLowerInvariant()));
        return TimeSpan.FromMilliseconds(100 + BitConverter.ToUInt16(hash, 0) % 400);
    }
}
