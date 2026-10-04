using System.Runtime.CompilerServices;

namespace Octo.Tests;

internal static class TestEnvironment
{
    /// <summary>
    /// Every web test boots the real Program, whose release check would ask GitHub for Octo's
    /// releases and keep the answer beside the real settings file. No test may do either, so
    /// checks are off for the whole run; the update tests build their own check with them on.
    /// </summary>
    [ModuleInitializer]
    internal static void TurnOffReleaseChecks() => Environment.SetEnvironmentVariable("Updates__Check", "false");

    /// <summary>The same for Soulseek's port test: no test run may ask tools.slsknet.org about
    /// this machine. The sharing tests give the check a scripted answer instead.</summary>
    [ModuleInitializer]
    internal static void TurnOffPortChecks() => Environment.SetEnvironmentVariable("Soulseek__CheckListenPort", "false");
}
