using Microsoft.Extensions.Options;
using Octo.Models.Settings;
using Octo.Services.Validation;

namespace Octo.Services.Soulseek;

/// <summary>
/// Validates that slskd is reachable + that the music source is wired correctly.
/// </summary>
public class SoulseekStartupValidator : BaseStartupValidator
{
    private readonly SoulseekSettings _settings;
    private readonly SoulseekClient _client;

    public override string ServiceName => "Soulseek";

    public SoulseekStartupValidator(
        IOptions<SoulseekSettings> settings,
        SoulseekClient client,
        HttpClient httpClient)
        : base(httpClient)
    {
        _settings = settings.Value;
        _client = client;
    }

    public override async Task<ValidationResult> ValidateAsync(CancellationToken cancellationToken)
    {
        Console.WriteLine();

        if (string.IsNullOrWhiteSpace(_settings.BaseUrl))
        {
            WriteStatus("Soulseek (slskd)", "NOT CONFIGURED", ConsoleColor.Red);
            WriteDetail("Set the Soulseek__BaseUrl environment variable (e.g. http://slskd:5030)");
            return ValidationResult.Failure("-1", "Soulseek BaseUrl not set");
        }

        WriteStatus("Soulseek BaseUrl", _settings.BaseUrl, ConsoleColor.Cyan);
        WriteStatus("Search wait", $"{_settings.SearchWaitSeconds}s", ConsoleColor.Cyan);
        WriteStatus("Min file size", $"{_settings.MinFileSizeBytes / (1024 * 1024)} MB", ConsoleColor.Cyan);

        var reachable = await _client.IsReachableAsync(cancellationToken);
        if (!reachable)
        {
            WriteStatus("slskd API", "UNREACHABLE", ConsoleColor.Red);
            WriteDetail("Check that slskd is running and Soulseek__BaseUrl + Username + Password are correct");
            return ValidationResult.Failure("-1", "slskd unreachable");
        }

        WriteStatus("slskd API", "REACHABLE", ConsoleColor.Green);
        return ValidationResult.Success("Soulseek validation passed");
    }
}
