using Microsoft.AspNetCore.Mvc;
using Octo.Services.Sonic;

namespace Octo.Controllers;

/// <summary>Sounds alike on the dashboard's Radio page: where the analysis is, and its controls.</summary>
[ApiController]
[Route("api/admin/sonic")]
public sealed class SonicAdminController(SonicAnalysisWorker worker) : ControllerBase
{
    [HttpGet]
    public IActionResult Status() => Ok(worker.Status());

    [HttpPost("start")]
    public IActionResult Start() { worker.SetPaused(false); return Ok(worker.Status()); }

    [HttpPost("pause")]
    public IActionResult Pause() { worker.SetPaused(true); return Ok(worker.Status()); }

    [HttpPost("reset")]
    public IActionResult Reset() { worker.Reset(); return Ok(worker.Status()); }
}
