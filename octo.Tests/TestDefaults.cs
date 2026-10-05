using System.Runtime.CompilerServices;

namespace Octo.Tests;

internal static class TestDefaults
{
    // The web tests written before the dashboard sign-in test what each endpoint does, not who may
    // call it, so they run with it off. AdminSignInTests turns it back on and tests it alone.
#pragma warning disable CA2255
    [ModuleInitializer]
#pragma warning restore CA2255
    internal static void SignInOffForWebTests() => Environment.SetEnvironmentVariable("Admin__SignIn", "off");
}
