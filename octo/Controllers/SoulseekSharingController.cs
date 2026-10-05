using System.Text.Json.Nodes;
using Microsoft.AspNetCore.Mvc;
using Octo.Services.Admin;
using Octo.Services.Soulseek;

namespace Octo.Controllers;

/// <summary>
/// The Sharing card on the dashboard's Soulseek page: what this server gives back to the Soulseek
/// network, read from slskd through Octo's own slskd sign-in, and the switch that turns sharing on
/// and off. Only counts, folder names and states come back; slskd's login stays inside Octo. Behind
/// the same guard as the rest of /api/admin, so the writes need X-Octo-Admin.
/// </summary>
[ApiController]
[Route("api/admin/soulseek/sharing")]
public class SoulseekSharingController(SoulseekSharing sharing, SoulseekShareSwitch shareSwitch,
    SettingsFileWriter settings, ILogger<SoulseekSharingController> logger) : ControllerBase
{
    public sealed record ShareRequest(bool On);

    [HttpGet]
    public async Task<IActionResult> Get(CancellationToken ct) => Ok(await sharing.ReportAsync(testPort: false, ct));

    /// <summary>Ask Soulseek's port test again now. At most every 30 seconds; sooner returns the
    /// last answer.</summary>
    [HttpPost("test-port")]
    public async Task<IActionResult> TestPort(CancellationToken ct) => Ok(await sharing.ReportAsync(testPort: true, ct));

    /// <summary>Have slskd look through its shared folders again.</summary>
    [HttpPost("rescan")]
    public async Task<IActionResult> Rescan(CancellationToken ct)
    {
        var refused = await sharing.RescanAsync(ct);
        return refused is null ? Accepted(new { ok = true }) : Conflict(new { error = refused });
    }

    /// <summary>
    /// The Share my library switch. Saves the choice as Soulseek.ShareLibrary, so the background
    /// check keeps slskd to it, then applies it to slskd straight away and answers with the card as
    /// it then stands. A choice slskd could not take is still saved, and the card says why.
    /// </summary>
    [HttpPost("share")]
    public async Task<IActionResult> Share([FromBody] ShareRequest request, CancellationToken ct)
    {
        try
        {
            settings.Merge(new JsonObject { ["Soulseek"] = new JsonObject { ["ShareLibrary"] = request.On } });
        }
        catch (Exception ex)
        {
            logger.LogError(ex, "Could not save the Soulseek sharing choice");
            return StatusCode(500, new { error = $"Octo could not save the choice: {ex.Message}" });
        }
        await shareSwitch.ApplyAsync(request.On, chosen: true, ct);
        return Ok(await sharing.ReportAsync(testPort: false, ct, shareOn: request.On));
    }
}
