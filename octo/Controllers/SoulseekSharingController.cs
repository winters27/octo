using Microsoft.AspNetCore.Mvc;
using Octo.Services.Soulseek;

namespace Octo.Controllers;

/// <summary>
/// The Sharing card on the dashboard's Soulseek page: what this server gives back to the Soulseek
/// network, read from slskd through Octo's own slskd sign-in. Only counts, folder names and states
/// come back; slskd's login stays inside Octo. Behind the same guard as the rest of /api/admin, so
/// the two writes need X-Octo-Admin.
/// </summary>
[ApiController]
[Route("api/admin/soulseek/sharing")]
public class SoulseekSharingController(SoulseekSharing sharing) : ControllerBase
{
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
}
