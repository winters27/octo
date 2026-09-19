using System.Threading.Channels;
using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Subsonic;

namespace Octo.Services.Library;

/// <summary>
/// A rating that asked for an action, with the credentials needed to clear it again.
///
/// The triplet is carried rather than looked up because Subsonic ratings are PER USER: clearing
/// with Octo's admin identity would clear the admin's rating and leave the user's in place.
/// Subsonic token auth is md5(password + salt) and is replayable with the same salt, which is
/// the same mechanism NavidromeIdentityService already relies on for startScan.
/// </summary>
public sealed record RatingActionRequest(
    LibraryAction Action, string NavidromeId, string Username,
    string AuthUser, string AuthToken, string AuthSalt);

/// <summary>
/// Applies star-rating actions, off the request thread.
///
/// Separate from the playlist worker because the two triggers are independently configurable:
/// ratings can be on with playlists off, and a single worker with two early-return gates would
/// switch both off together.
/// </summary>
public sealed class LibraryActionRatingWorker : BackgroundService
{
    private readonly Channel<RatingActionRequest> _queue =
        Channel.CreateBounded<RatingActionRequest>(new BoundedChannelOptions(256)
        { FullMode = BoundedChannelFullMode.DropWrite });

    private readonly LibraryActionExecutor _executor;
    private readonly SubsonicProxyService? _proxy;
    private readonly IServiceScopeFactory _scopes;
    private readonly IOptionsMonitor<LibraryActionSettings> _settings;
    private readonly ILogger<LibraryActionRatingWorker> _logger;

    public LibraryActionRatingWorker(LibraryActionExecutor executor, IServiceScopeFactory scopes,
        IOptionsMonitor<LibraryActionSettings> settings, ILogger<LibraryActionRatingWorker> logger)
    {
        _executor = executor;
        _scopes = scopes;
        _settings = settings;
        _logger = logger;
        _proxy = null;
    }

    /// <summary>
    /// Queue a rating for action. The cheap gates run inline so a disabled feature costs one
    /// property read on the request path, and nothing that removes a file runs in the request.
    /// </summary>
    public bool TryEnqueue(RatingActionRequest request)
    {
        var settings = _settings.CurrentValue;
        if (!settings.Enabled || !settings.RatingsEnabled) return false;
        if (!settings.IsAllowed(request.Username)) return false;
        return _queue.Writer.TryWrite(request);
    }

    protected override async Task ExecuteAsync(CancellationToken stoppingToken)
    {
        await foreach (var request in _queue.Reader.ReadAllAsync(stoppingToken))
        {
            // Per-item catch is mandatory: BackgroundServiceExceptionBehavior defaults to
            // StopHost, so one unhandled exception here would take Octo down.
            try
            {
                var outcome = await _executor.ApplyAsync(
                    new LibraryActionRequest(request.Action, request.NavidromeId, request.Username),
                    stoppingToken);

                _logger.LogInformation("Library action {Action} from a rating by {User}: {State} - {Detail}",
                    request.Action, request.Username, outcome.State, outcome.Detail);

                // Only clear a rating the action actually consumed. Leaving it set on a failure
                // means the user can see it did not take, rather than the rating vanishing and
                // nothing having happened.
                if (outcome.Consumed) await ClearRatingAsync(request, stoppingToken);
            }
            catch (OperationCanceledException) { break; }
            catch (Exception ex)
            {
                _logger.LogError(ex, "Library action from a rating failed for {Id}", request.NavidromeId);
            }
        }
    }

    /// <summary>
    /// Put the rating back to 0.
    ///
    /// Three things stop this looping. rating=0 is inert by construction, because the handler
    /// only acts on 1 to 5. This call goes out through RelayAsync, which builds an outbound
    /// request and never re-enters Octo's own routing table. And the journal would make a
    /// re-entry for the same action, id and file content a no-op anyway.
    /// </summary>
    private async Task ClearRatingAsync(RatingActionRequest request, CancellationToken ct)
    {
        try
        {
            using var scope = _scopes.CreateScope();
            var proxy = _proxy ?? scope.ServiceProvider.GetRequiredService<SubsonicProxyService>();

            await proxy.RelayAsync("rest/setRating", new Dictionary<string, string>
            {
                ["id"] = request.NavidromeId,
                ["rating"] = "0",
                ["u"] = request.AuthUser,
                ["t"] = request.AuthToken,
                ["s"] = request.AuthSalt,
                ["v"] = "1.16.1",
                ["c"] = "octo",
                ["f"] = "json",
            });
        }
        catch (Exception ex)
        {
            // A rating left set is cosmetic, and the journal stops it triggering the action a
            // second time.
            _logger.LogInformation("Could not clear the rating on {Id}: {M}", request.NavidromeId, ex.Message);
        }
    }
}
