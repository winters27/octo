using Microsoft.AspNetCore.Mvc;
using Octo.Services.Imports;

namespace Octo.Services.Subsonic;

public partial class SubsonicResponseBuilder
{
    /// <summary>The OpenSubsonic extension a client checks for before it offers Spotify import.</summary>
    public const string ImportsExtension = "octoImports";
    public const int ImportsExtensionVersion = 1;

    /// <summary>getImports: the caller's lists, Spotify sign-in and trickle. Always JSON; the field names are a contract with the Octo apps.</summary>
    public IActionResult CreateImportsResponse(ImportOverview overview) => OctoAnswer("imports", overview);

    /// <summary>getImport: one list and every song in it.</summary>
    public IActionResult CreateImportResponse(ListDetail detail) => OctoAnswer("import", detail);

    /// <summary>importAction: what one action did. Always ok, so the client reads the answer rather than an error.</summary>
    public IActionResult CreateImportActionResponse(ImportActionResult result) => OctoAnswer("importAction", result);

    private IActionResult OctoAnswer(string key, object value) =>
        CreateJsonResponse(new Dictionary<string, object?>
        {
            ["status"] = "ok",
            ["version"] = SubsonicVersion,
            ["type"] = "octo",
            ["openSubsonic"] = true,
            [key] = value,
        });
}
