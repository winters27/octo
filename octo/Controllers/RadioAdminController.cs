using Microsoft.AspNetCore.Mvc;
using Octo.Services.Radio;

namespace Octo.Controllers;

/// <summary>What radio has learned from listening, on the dashboard's Radio page.</summary>
[ApiController]
[Route("api/admin/radio-outcomes")]
public sealed class RadioAdminController(RadioOutcomeStore outcomes) : ControllerBase
{
    [HttpGet]
    public IActionResult Stats() => Ok(outcomes.Stats());

    [HttpPost("reset")]
    public IActionResult Reset() { outcomes.Forget(); return Ok(outcomes.Stats()); }
}
