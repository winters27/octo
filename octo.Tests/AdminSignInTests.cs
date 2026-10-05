using System.Net;
using System.Text;
using System.Text.Json;
using System.Text.RegularExpressions;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Http;
using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.AspNetCore.Routing;
using Microsoft.Extensions.Configuration;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Logging;
using Microsoft.Extensions.Options;
using Octo.Middleware;
using Octo.Models.Settings;
using Octo.Services.Soulseek;
using Octo.Services.Admin;
using Octo.Services.LastFm;

namespace Octo.Tests;

/// <summary>
/// The dashboard sign-in. Everything here runs with it ON (the rest of the suite turns it off in
/// TestDefaults), against a fake Navidrome that knows four people: admin and listener sign in with
/// "pw"; demoted lost the admin role; gone no longer exists; flaky's lookups fail.
/// </summary>
public class AdminSignInTests
{
    private static readonly string[] OpenEndpoints =
        ["api/admin/auth/recovery", "api/admin/browse/auth", "api/admin/browse/session", "api/admin/browse/signout", "api/admin/status"];

    private static readonly string[] AppEndpoints =
        ["api/admin/downloads", "api/admin/lastfm/radio", "api/admin/lastfm/radio/refresh", "api/admin/library-status", "api/admin/status"];

    // ---- what is open, and everything else is not ---------------------------------------------

    [Fact]
    public async Task OnlyTheMarkedEndpointsAnswerWithoutASignIn()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();
        var endpoints = factory.Services.GetRequiredService<EndpointDataSource>().Endpoints
            .OfType<RouteEndpoint>()
            .Where(endpoint => endpoint.RoutePattern.RawText?.TrimStart('/').StartsWith("api/admin", StringComparison.OrdinalIgnoreCase) == true)
            .ToList();
        Assert.NotEmpty(endpoints);

        string Raw(RouteEndpoint endpoint) => endpoint.RoutePattern.RawText!.TrimStart('/');
        Assert.Equal(OpenEndpoints, endpoints.Where(e => e.Metadata.GetMetadata<AdminOpenAttribute>() is not null).Select(Raw).Distinct().Order());
        Assert.Equal(AppEndpoints, endpoints.Where(e => e.Metadata.GetMetadata<AdminAppCallAttribute>() is not null).Select(Raw).Distinct().Order());

        var checkedCount = 0;
        foreach (var endpoint in endpoints.Where(e => e.Metadata.GetMetadata<AdminOpenAttribute>() is null))
        {
            var path = "/" + Regex.Replace(Raw(endpoint), @"\{[^}]+\}", "x");
            foreach (var method in endpoint.Metadata.GetMetadata<HttpMethodMetadata>()?.HttpMethods ?? ["GET"])
            {
                using var request = new HttpRequestMessage(new HttpMethod(method), path);
                request.Headers.Add("X-Octo-Admin", "1");
                if (method != "GET") request.Content = new StringContent("{}", Encoding.UTF8, "application/json");
                using var response = await client.SendAsync(request);
                Assert.True(response.StatusCode == HttpStatusCode.Unauthorized, $"{method} {path} answered {(int)response.StatusCode} without a sign-in");
                Assert.True((await Json(response)).GetProperty("signIn").GetBoolean(), $"{method} {path} did not ask for a sign-in");
                checkedCount++;
            }
        }
        Assert.True(checkedCount > 50, $"only {checkedCount} endpoints were walked");
    }

    [Fact]
    public async Task AnUnknownAdminPathAndOddCasingStayClosed()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();

        Assert.Equal(HttpStatusCode.Unauthorized, (await client.GetAsync("/API/Admin/Settings")).StatusCode);
        Assert.Equal(HttpStatusCode.Unauthorized, (await client.GetAsync("/api/admin/no-such-thing")).StatusCode);
    }

    // ---- status ---------------------------------------------------------------------------------

    [Fact]
    public async Task StatusTellsAnyoneOnlyWhetherEachServiceIsUp()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();

        using var response = await client.GetAsync("/api/admin/status");
        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        var text = await response.Content.ReadAsStringAsync();
        // The shape install.sh greps for.
        Assert.Contains("\"navidrome\":{\"ok\":", text);
        using var doc = JsonDocument.Parse(text);
        Assert.True(doc.RootElement.TryGetProperty("octo", out _));
        foreach (var service in doc.RootElement.GetProperty("services").EnumerateObject())
            Assert.Equal(["configured", "ok"], service.Value.EnumerateObject().Select(p => p.Name).Order());
    }

    [Fact]
    public async Task StatusGivesASignedInAdminTheDetails()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();
        client.DefaultRequestHeaders.Add("X-Octo-Browse-Token", factory.Sessions.Create("admin"));

        var services = (await Json(await client.GetAsync("/api/admin/status"))).GetProperty("services");
        Assert.True(services.GetProperty("navidrome").TryGetProperty("detail", out _));
    }

    // ---- signing in with Navidrome ----------------------------------------------------------------

    [Fact]
    public async Task ANavidromeAdminSignsInAndTheCookieOpensTheDashboard()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();

        using var signIn = await SignInAsync(client, "admin", "pw");
        Assert.Equal(HttpStatusCode.OK, signIn.StatusCode);
        var cookie = string.Join(";", signIn.Headers.GetValues("Set-Cookie")).ToLowerInvariant();
        Assert.Contains("octo_browse=", cookie);
        Assert.Contains("path=/api/admin", cookie);
        Assert.Contains("samesite=strict", cookie);
        Assert.Contains("httponly", cookie);

        Assert.Equal(HttpStatusCode.OK, (await client.GetAsync("/api/admin/settings")).StatusCode);
        var session = await Json(await client.GetAsync("/api/admin/browse/session"));
        Assert.True(session.GetProperty("signedIn").GetBoolean());
        Assert.Equal("admin", session.GetProperty("user").GetString());
        Assert.False(session.GetProperty("recovery").GetBoolean());
        Assert.False(session.GetProperty("signInOff").GetBoolean());
    }

    [Fact]
    public async Task AListenerCannotSignInToTheDashboard()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();

        using var response = await SignInAsync(client, "listener", "pw");
        Assert.Equal(HttpStatusCode.Unauthorized, response.StatusCode);
        Assert.Contains("not a Navidrome admin", (await Json(response)).GetProperty("error").GetString());
    }

    [Fact]
    public async Task TenWrongPasswordsMakeTheAddressWait()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();

        for (var i = 0; i < SignInThrottle.MaxFailures; i++)
            Assert.Equal(HttpStatusCode.Unauthorized, (await SignInAsync(client, "admin", "wrong")).StatusCode);
        using var blocked = await SignInAsync(client, "admin", "pw");
        Assert.Equal((HttpStatusCode)429, blocked.StatusCode);
        Assert.True(blocked.Headers.Contains("Retry-After"));
    }

    // ---- the recovery code ------------------------------------------------------------------------

    [Fact]
    public async Task TheRecoveryCodeWorksOnceAndOpensSettingsButNotFiles()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();
        var code = factory.Recovery.Current()!;

        using var first = await PostJsonAsync(client, "/api/admin/auth/recovery", new { code = code.ToLowerInvariant().Replace("-", " ") });
        Assert.Equal(HttpStatusCode.OK, first.StatusCode);
        Assert.NotEqual(code, factory.Recovery.Current());

        Assert.Equal(HttpStatusCode.OK, (await client.GetAsync("/api/admin/settings")).StatusCode);
        Assert.Equal(HttpStatusCode.Unauthorized, (await client.GetAsync("/api/admin/lyrics/library")).StatusCode);
        Assert.True((await Json(await client.GetAsync("/api/admin/browse/session"))).GetProperty("recovery").GetBoolean());

        using var other = factory.CreateClient();
        Assert.Equal(HttpStatusCode.Unauthorized, (await PostJsonAsync(other, "/api/admin/auth/recovery", new { code })).StatusCode);
    }

    [Fact]
    public async Task ARecoverySignInIsNeverWrittenToDisk()
    {
        var path = Path.Combine(Path.GetTempPath(), "octo-sessions-" + Guid.NewGuid() + ".json");
        try
        {
            var store = new BrowseSessionStore(path);
            store.Create("admin");
            var recovery = store.CreateRecovery();

            Assert.Equal(BrowseSessionStore.RecoveryUser, store.UserOf(recovery));
            Assert.Null(store.NavidromeUserOf(recovery));
            Assert.Single(JsonDocument.Parse(File.ReadAllText(path)).RootElement.EnumerateArray());
        }
        finally { File.Delete(path); }
    }

    // ---- the Octo app's own sign-in ---------------------------------------------------------------

    [Fact]
    public async Task TheAppSignsItsAdminCallsWithItsSubsonicSignIn()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();

        Assert.Equal(HttpStatusCode.OK, (await client.GetAsync("/api/admin/downloads?u=admin&p=pw&v=1.16.1&c=test")).StatusCode);
        Assert.Equal(HttpStatusCode.Forbidden, (await client.GetAsync("/api/admin/downloads?u=listener&p=pw&v=1.16.1&c=test")).StatusCode);
        Assert.Equal(HttpStatusCode.Unauthorized, (await client.GetAsync("/api/admin/downloads?u=listener&p=bad&v=1.16.1&c=test")).StatusCode);
        // Not an app call, so a Subsonic sign-in opens nothing here.
        Assert.Equal(HttpStatusCode.Unauthorized, (await client.GetAsync("/api/admin/settings?u=admin&p=pw&v=1.16.1&c=test")).StatusCode);
    }

    [Fact]
    public async Task AListenerSeesAndRebuildsOnlyTheirOwnStations()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();

        var radio = await Json(await client.GetAsync("/api/admin/lastfm/radio?user=admin&u=listener&p=pw&v=1.16.1&c=test"));
        Assert.Equal("listener", radio.GetProperty("selectedUser").GetString());
        Assert.All(radio.GetProperty("users").EnumerateArray(), u => Assert.Equal("listener", u.GetProperty("username").GetString()));

        using var request = new HttpRequestMessage(HttpMethod.Post, "/api/admin/lastfm/radio/refresh?u=listener&p=pw&v=1.16.1&c=test")
        {
            Content = new StringContent("{\"user\":\"admin\"}", Encoding.UTF8, "application/json"),
        };
        request.Headers.Add("X-Octo-Admin", "1");
        Assert.Equal(HttpStatusCode.Accepted, (await client.SendAsync(request)).StatusCode);
        using var wait = new CancellationTokenSource(TimeSpan.FromSeconds(5));
        var job = await factory.Services.GetRequiredService<LastFmRadioRefreshQueue>().DequeueAsync(wait.Token);
        Assert.Equal("listener", job.Username);
    }

    [Fact]
    public async Task WrongAppPasswordsCountTowardTheWait()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();

        for (var i = 0; i < SignInThrottle.MaxFailures; i++)
            await client.GetAsync($"/api/admin/downloads?u=admin&p=bad{i}&v=1.16.1&c=test");
        Assert.Equal(HttpStatusCode.Unauthorized, (await client.GetAsync("/api/admin/downloads?u=admin&p=pw&v=1.16.1&c=test")).StatusCode);
    }

    // ---- sessions end when Navidrome says so --------------------------------------------------------

    [Fact]
    public async Task SignInsEndWhenNavidromeTakesTheAdminRoleAway()
    {
        await using var factory = new SignInWebFactory();
        using var admin = factory.CreateClient();
        // Signing in gives Octo its own admin identity, which the role check asks with.
        Assert.Equal(HttpStatusCode.OK, (await SignInAsync(admin, "admin", "pw")).StatusCode);

        var demoted = factory.Sessions.Create("demoted");
        var gone = factory.Sessions.Create("gone");

        Assert.Equal(HttpStatusCode.Unauthorized, (await GetWithToken(factory, "/api/admin/settings", demoted)).StatusCode);
        Assert.Equal(HttpStatusCode.Unauthorized, (await GetWithToken(factory, "/api/admin/settings", gone)).StatusCode);
        Assert.Null(factory.Sessions.UserOf(demoted));
        Assert.Null(factory.Sessions.UserOf(gone));

        // Asked once an hour, not on every call.
        var asked = factory.Navidrome.UserListCalls;
        Assert.Equal(HttpStatusCode.OK, (await admin.GetAsync("/api/admin/settings")).StatusCode);
        Assert.Equal(HttpStatusCode.OK, (await admin.GetAsync("/api/admin/settings")).StatusCode);
        Assert.Equal(asked + 1, factory.Navidrome.UserListCalls);
        var check = factory.Services.GetRequiredService<AdminRoleCheck>();
        var later = DateTime.UtcNow.AddMinutes(61);
        check.Clock = () => later;
        Assert.Equal(HttpStatusCode.OK, (await admin.GetAsync("/api/admin/settings")).StatusCode);
        Assert.Equal(asked + 2, factory.Navidrome.UserListCalls);
    }

    [Fact]
    public async Task ADemotedAdminIsSignedOutEvenWhenOctosTokenIsTheirOwn()
    {
        // Found on a real starter stack: signing in made admin2's own login Octo's admin token, so
        // after the demotion Octo checked admin2 with admin2's token, saw a non-admin's view, and
        // let the session stand. That view is answer enough about admin2 themselves.
        await using var factory = new SignInWebFactory();
        using var admin2 = factory.CreateClient();
        Assert.Equal(HttpStatusCode.OK, (await SignInAsync(admin2, "admin2", "pw")).StatusCode);

        factory.Navidrome.Admin2Demoted = true;

        Assert.Equal(HttpStatusCode.Unauthorized, (await admin2.GetAsync("/api/admin/settings")).StatusCode);
    }

    [Fact]
    public async Task AStaleTokenIsDroppedAndTheSettingsLoginAsksAgain()
    {
        // Octo's token came from admin2, since demoted; checking someone else needs a real admin's
        // view, so Octo logs in again with the admin login in its settings, as a starter stack has.
        await using var factory = new SignInWebFactory(adminLoginInSettings: true);
        using var admin2 = factory.CreateClient();
        Assert.Equal(HttpStatusCode.OK, (await SignInAsync(admin2, "admin2", "pw")).StatusCode);
        var demoted = factory.Sessions.Create("demoted");

        factory.Navidrome.Admin2Demoted = true;

        Assert.Equal(HttpStatusCode.Unauthorized, (await GetWithToken(factory, "/api/admin/settings", demoted)).StatusCode);
        Assert.Null(factory.Sessions.UserOf(demoted));
    }

    [Fact]
    public async Task NobodyIsLockedOutWhenNavidromeCannotSay()
    {
        await using var factory = new SignInWebFactory();
        using var admin = factory.CreateClient();
        Assert.Equal(HttpStatusCode.OK, (await SignInAsync(admin, "admin", "pw")).StatusCode);

        // The list fails outright.
        factory.Navidrome.UserListFails = true;
        var first = factory.Sessions.Create("stranger");
        Assert.Equal(HttpStatusCode.OK, (await GetWithToken(factory, "/api/admin/settings", first)).StatusCode);

        // The list comes back as a non-admin's view (just one listener): it cannot say anyone is missing.
        factory.Navidrome.UserListFails = false;
        factory.Navidrome.UserListAsListener = true;
        var second = factory.Sessions.Create("someone-else");
        Assert.Equal(HttpStatusCode.OK, (await GetWithToken(factory, "/api/admin/settings", second)).StatusCode);
        Assert.Equal("someone-else", factory.Sessions.UserOf(second));
    }

    [Fact]
    public async Task ABrowserThatNeverSignedInIsToldSoWithoutAPrompt()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();

        using var response = await client.GetAsync("/api/admin/browse/session");
        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        var session = await Json(response);
        Assert.False(session.GetProperty("signedIn").GetBoolean());
        Assert.False(session.GetProperty("signInOff").GetBoolean());
    }

    [Fact]
    public async Task SignOutEverywhereEndsEveryBrowserOfThatPerson()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();
        Assert.Equal(HttpStatusCode.OK, (await SignInAsync(client, "admin", "pw")).StatusCode);
        var otherBrowser = factory.Sessions.Create("admin");
        var someoneElse = factory.Sessions.Create("other-admin");

        var answer = await Json(await PostJsonAsync(client, "/api/admin/browse/signout?everywhere=true", new { }));
        Assert.Equal(2, answer.GetProperty("ended").GetInt32());
        Assert.Null(factory.Sessions.UserOf(otherBrowser));
        Assert.Equal("other-admin", factory.Sessions.UserOf(someoneElse));
        Assert.Equal(HttpStatusCode.Unauthorized, (await client.GetAsync("/api/admin/settings")).StatusCode);
    }

    // ---- headers --------------------------------------------------------------------------------

    [Fact]
    public async Task TheDashboardCannotBeFramedAndAdminAnswersAreNeverCached()
    {
        await using var factory = new SignInWebFactory();
        using var client = factory.CreateClient();

        using var page = await client.GetAsync("/admin/index.html");
        Assert.Equal("SAMEORIGIN", page.Headers.GetValues("X-Frame-Options").Single());
        Assert.Equal("frame-ancestors 'self'", page.Headers.GetValues("Content-Security-Policy").Single());

        client.DefaultRequestHeaders.Add("X-Octo-Browse-Token", factory.Sessions.Create("admin"));
        using var settings = await client.GetAsync("/api/admin/settings");
        Assert.Contains("no-store", settings.Headers.CacheControl?.ToString() ?? "");
        Assert.Equal("nosniff", settings.Headers.GetValues("X-Content-Type-Options").Single());
    }

    // ---- switched off, or switched to something unknown ----------------------------------------------

    [Fact]
    public async Task WithTheSignInOffTheDashboardIsOpenButFilesStillAskForNavidrome()
    {
        await using var factory = new SignInWebFactory("off");
        using var client = factory.CreateClient();

        Assert.Equal(HttpStatusCode.OK, (await client.GetAsync("/api/admin/settings")).StatusCode);
        Assert.True((await Json(await client.GetAsync("/api/admin/browse/session"))).GetProperty("signInOff").GetBoolean());
        Assert.Equal(HttpStatusCode.Unauthorized, (await client.GetAsync("/api/admin/lyrics/library")).StatusCode);
    }

    [Fact]
    public async Task AnUnknownSignInValueKeepsTheSignInOn()
    {
        await using var factory = new SignInWebFactory("of");
        using var client = factory.CreateClient();

        Assert.Equal(HttpStatusCode.Unauthorized, (await client.GetAsync("/api/admin/settings")).StatusCode);
    }

    // ---- the admin port ---------------------------------------------------------------------------

    [Theory]
    [InlineData(8080, "/admin/index.html", 404)]
    [InlineData(8080, "/api/admin/status", 404)]
    [InlineData(8080, "/rest/ping", 0)]
    [InlineData(5275, "/rest/ping", 404)]
    [InlineData(5275, "/", 302)]
    [InlineData(5275, "/api/admin/status", 0)]
    [InlineData(5275, "/admin/index.html", 0)]
    [InlineData(5275, "/Assets/octo_logo.png", 0)]
    public void TheAdminPortServesOnlyTheDashboard(int localPort, string path, int answered)
    {
        var context = new DefaultHttpContext();
        context.Connection.LocalPort = localPort;
        context.Request.Path = path;

        var refused = AdminPortSplit.Refuse(context, adminPort: 5275);

        Assert.Equal(answered != 0, refused);
        if (answered != 0) Assert.Equal(answered, context.Response.StatusCode);
    }

    [Theory]
    [InlineData("http://+:8080", null, 5275, "http://+:8080;http://+:5275")]
    [InlineData(null, "8080", 5275, "http://+:8080;http://+:5275")]
    [InlineData(null, null, 5275, "http://localhost:5000;http://+:5275")]
    [InlineData("https://localhost:7248;http://localhost:5274", null, 5275, "https://localhost:7248;http://localhost:5274;http://+:5275")]
    [InlineData("http://+:8080", null, 8080, null)]
    [InlineData("http://+:8080", null, 0, null)]
    public void TheAdminPortIsAddedToOctosOwnAddresses(string? urls, string? httpPorts, int adminPort, string? expected)
    {
        Assert.Equal(expected, AdminPortSplit.ListenUrls(urls, httpPorts, adminPort));
    }

    // ---- helpers --------------------------------------------------------------------------------

    private static Task<HttpResponseMessage> SignInAsync(HttpClient client, string username, string password) =>
        PostJsonAsync(client, "/api/admin/browse/auth", new { username, password });

    private static Task<HttpResponseMessage> PostJsonAsync(HttpClient client, string path, object body)
    {
        var request = new HttpRequestMessage(HttpMethod.Post, path)
        {
            Content = new StringContent(JsonSerializer.Serialize(body), Encoding.UTF8, "application/json"),
        };
        request.Headers.Add("X-Octo-Admin", "1");
        return client.SendAsync(request);
    }

    private static async Task<HttpResponseMessage> GetWithToken(SignInWebFactory factory, string path, string token)
    {
        using var client = factory.CreateClient(new WebApplicationFactoryClientOptions { HandleCookies = false });
        client.DefaultRequestHeaders.Add("X-Octo-Browse-Token", token);
        return await client.GetAsync(path);
    }

    private static async Task<JsonElement> Json(HttpResponseMessage response) =>
        JsonDocument.Parse(await response.Content.ReadAsStringAsync()).RootElement.Clone();

    /// <summary>Octo with the sign-in on (or the value given), a fake Navidrome, and its state in a temp folder.</summary>
    private sealed class SignInWebFactory(string signIn = "required", bool adminLoginInSettings = false) : WebApplicationFactory<Program>
    {
        private readonly string _directory = Path.Combine(Path.GetTempPath(), "octo-signin-" + Guid.NewGuid());

        public FakeNavidrome Navidrome { get; } = new();
        public BrowseSessionStore Sessions { get; } = new();
        public AdminRecoveryCode Recovery => Services.GetRequiredService<AdminRecoveryCode>();

        protected override void ConfigureWebHost(IWebHostBuilder builder)
        {
            Directory.CreateDirectory(_directory);
            builder.UseEnvironment("Development");
            // App configuration, not host configuration: it must win over the environment variable
            // TestDefaults sets for every other test.
            builder.ConfigureAppConfiguration((_, configuration) => configuration.AddInMemoryCollection(new Dictionary<string, string?>
            {
                ["Admin:SignIn"] = signIn,
                // The admin login a starter stack writes, which Octo can always log in again with.
                ["Subsonic:AdminUsername"] = adminLoginInSettings ? "admin" : null,
                ["Subsonic:AdminPassword"] = adminLoginInSettings ? "pw" : null,
                ["Subsonic:Url"] = "http://navidrome.test",
                ["Subsonic:AutoDetectDownloadPath"] = "false",
                ["Library:DownloadPath"] = _directory,
                ["Soulseek:BaseUrl"] = "http://127.0.0.1:1",
                ["YouTube:ShimUrl"] = "http://127.0.0.1:1",
            }));
            builder.ConfigureServices(services =>
            {
                services.RemoveAll<IHostedService>();
                services.RemoveAll<IHttpClientFactory>();
                services.AddSingleton<IHttpClientFactory>(new OneHandlerFactory(Navidrome));
                services.RemoveAll<BrowseSessionStore>();
                services.AddSingleton(Sessions);
                services.RemoveAll<AdminRecoveryCode>();
                services.AddSingleton(provider => new AdminRecoveryCode(Path.Combine(_directory, "admin-recovery-code"),
                    provider.GetRequiredService<ILogger<AdminRecoveryCode>>()));
                services.RemoveAll<LastFmRadioStateStore>();
                services.AddSingleton(provider => new LastFmRadioStateStore(
                    Path.Combine(_directory, "radio-state.json"),
                    provider.GetRequiredService<IOptionsMonitor<LastFmSettings>>(),
                    provider.GetRequiredService<ExternalIdRegistry>(),
                    provider.GetRequiredService<ILogger<LastFmRadioStateStore>>()));
            });
        }

        public override async ValueTask DisposeAsync()
        {
            await base.DisposeAsync();
            try { Directory.Delete(_directory, true); } catch { }
        }
    }

    private sealed class OneHandlerFactory(HttpMessageHandler handler) : IHttpClientFactory
    {
        public HttpClient CreateClient(string name) => new(handler, disposeHandler: false);
    }

    /// <summary>The few Navidrome answers the sign-in needs; anything else is a 404.</summary>
    internal sealed class FakeNavidrome : HttpMessageHandler
    {
        private int _userListCalls;
        public int UserListCalls => Volatile.Read(ref _userListCalls);
        public bool UserListFails { get; set; }
        public bool UserListAsListener { get; set; }
        public bool Admin2Demoted { get; set; }

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
        {
            var path = request.RequestUri!.AbsolutePath;
            var query = Microsoft.AspNetCore.WebUtilities.QueryHelpers.ParseQuery(request.RequestUri.Query);
            string Q(string key) => query.TryGetValue(key, out var value) ? value.ToString() : "";

            if (path == "/auth/login")
            {
                using var body = JsonDocument.Parse(await request.Content!.ReadAsStringAsync(cancellationToken));
                var user = body.RootElement.GetProperty("username").GetString();
                var password = body.RootElement.GetProperty("password").GetString();
                if (password != "pw" || user is not ("admin" or "admin2" or "listener")) return new HttpResponseMessage(HttpStatusCode.Unauthorized);
                return Answer(JsonSerializer.Serialize(new
                {
                    id = user, name = user, username = user, isAdmin = user == "admin" || (user == "admin2" && !Admin2Demoted),
                    token = "jwt-" + user, subsonicSalt = "salt", subsonicToken = "token-" + user,
                }));
            }

            if (path.StartsWith("/rest/ping", StringComparison.Ordinal))
                return Q("p") == "pw" && Q("u") is "admin" or "listener" ? Subsonic("\"status\":\"ok\"") : Failed(40);

            // As real Navidrome does: getUser answers only for the caller's own name, and anyone else,
            // even asked by an admin, is error 50. (server/subsonic/users.go)
            if (path.StartsWith("/rest/getUser", StringComparison.Ordinal))
            {
                if (!string.Equals(Q("username"), Q("u"), StringComparison.OrdinalIgnoreCase)) return Failed(50);
                return Q("u") switch
                {
                    "admin" => Subsonic("\"status\":\"ok\",\"user\":{\"adminRole\":true}"),
                    "listener" => Subsonic("\"status\":\"ok\",\"user\":{\"adminRole\":false}"),
                    _ => Failed(40),
                };
            }

            // The native user list, an admin's token sees everyone. (server/nativeapi, model/user.go)
            if (path == "/api/user")
            {
                Interlocked.Increment(ref _userListCalls);
                if (UserListFails) return new HttpResponseMessage(HttpStatusCode.InternalServerError);
                var auth = request.Headers.TryGetValues("X-Nd-Authorization", out var values) ? values.Single() : "";
                // admin2's own token after Navidrome demoted admin2: a non-admin sees only themselves.
                if (auth == "Bearer jwt-admin2" && Admin2Demoted)
                    return Answer(JsonSerializer.Serialize(new object[] { new { userName = "admin2", isAdmin = false } }));
                if (auth is not ("Bearer jwt-admin" or "Bearer jwt-admin2")) return new HttpResponseMessage(HttpStatusCode.Unauthorized);
                object[] users = UserListAsListener
                    ? [new { userName = "listener", isAdmin = false }]
                    :
                    [
                        new { userName = "admin", isAdmin = true },
                        new { userName = "other-admin", isAdmin = true },
                        new { userName = "listener", isAdmin = false },
                        new { userName = "demoted", isAdmin = false },
                        new { userName = "admin2", isAdmin = !Admin2Demoted },
                    ];
                return Answer(JsonSerializer.Serialize(users));
            }

            return new HttpResponseMessage(HttpStatusCode.NotFound);
        }

        private static HttpResponseMessage Answer(string json) =>
            new(HttpStatusCode.OK) { Content = new StringContent(json, Encoding.UTF8, "application/json") };

        private static HttpResponseMessage Subsonic(string inner) =>
            Answer("{\"subsonic-response\":{" + inner + ",\"version\":\"1.16.1\"}}");

        private static HttpResponseMessage Failed(int code) =>
            Subsonic($"\"status\":\"failed\",\"error\":{{\"code\":{code},\"message\":\"no\"}}");
    }
}
