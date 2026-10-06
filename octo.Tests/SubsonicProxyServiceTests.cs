using Microsoft.AspNetCore.Mvc;
using Microsoft.Extensions.Options;
using Microsoft.AspNetCore.Http;
using Moq;
using Moq.Protected;
using Octo.Models.Settings;
using Octo.Services.Subsonic;
using System.Net;

namespace Octo.Tests;

public class SubsonicProxyServiceTests
{
    private readonly SubsonicProxyService _service;
    private readonly Mock<HttpMessageHandler> _mockHttpMessageHandler;
    private readonly Mock<IHttpClientFactory> _mockHttpClientFactory;

    public SubsonicProxyServiceTests()
    {
        _mockHttpMessageHandler = new Mock<HttpMessageHandler>();
        var httpClient = new HttpClient(_mockHttpMessageHandler.Object);

        _mockHttpClientFactory = new Mock<IHttpClientFactory>();
        _mockHttpClientFactory.Setup(x => x.CreateClient(It.IsAny<string>())).Returns(httpClient);

        var settings = TestOptions.Monitor(new SubsonicSettings 
        { 
            Url = "http://localhost:4533" 
        });

        var httpContext = new DefaultHttpContext();
        var httpContextAccessor = new HttpContextAccessor
        {
            HttpContext = httpContext
        };

        _service = new SubsonicProxyService(_mockHttpClientFactory.Object, settings, httpContextAccessor);
    }

    [Fact]
    public async Task RelayAsync_SuccessfulRequest_ReturnsBodyAndContentType()
    {
        // Arrange
        var responseContent = new byte[] { 1, 2, 3, 4, 5 };
        var responseMessage = new HttpResponseMessage(HttpStatusCode.OK)
        {
            Content = new ByteArrayContent(responseContent)
        };
        responseMessage.Content.Headers.ContentType = new System.Net.Http.Headers.MediaTypeHeaderValue("application/json");

        _mockHttpMessageHandler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync",
                ItExpr.IsAny<HttpRequestMessage>(),
                ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync(responseMessage);

        var parameters = new Dictionary<string, string>
        {
            { "u", "admin" },
            { "p", "password" },
            { "v", "1.16.0" }
        };

        // Act
        var (body, contentType) = await _service.RelayAsync("rest/ping", parameters);

        // Assert
        Assert.Equal(responseContent, body);
        Assert.Equal("application/json", contentType);
    }

    [Fact]
    public async Task RelayAsync_BuildsCorrectUrl()
    {
        // Arrange
        HttpRequestMessage? capturedRequest = null;
        var responseMessage = new HttpResponseMessage(HttpStatusCode.OK)
        {
            Content = new ByteArrayContent(Array.Empty<byte>())
        };

        _mockHttpMessageHandler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync",
                ItExpr.IsAny<HttpRequestMessage>(),
                ItExpr.IsAny<CancellationToken>())
            .Callback<HttpRequestMessage, CancellationToken>((req, ct) => capturedRequest = req)
            .ReturnsAsync(responseMessage);

        var parameters = new Dictionary<string, string>
        {
            { "u", "admin" },
            { "p", "secret" }
        };

        // Act
        await _service.RelayAsync("rest/ping", parameters);

        // Assert
        Assert.NotNull(capturedRequest);
        Assert.Contains("http://localhost:4533/rest/ping", capturedRequest!.RequestUri!.ToString());
        Assert.Contains("u=admin", capturedRequest.RequestUri.ToString());
        Assert.Contains("p=secret", capturedRequest.RequestUri.ToString());
    }

    [Fact]
    public async Task RelayAsync_PicksUpASettingsChangeWithoutBeingRebuilt()
    {
        // Regression for the admin UI applying nothing until a restart. These
        // services are singletons, so capturing IOptions.Value in the constructor
        // froze settings at startup while the admin UI, reading through
        // IOptionsMonitor, happily showed the new value as if it had taken effect.
        HttpRequestMessage? capturedRequest = null;
        var responseMessage = new HttpResponseMessage(HttpStatusCode.OK)
        {
            Content = new ByteArrayContent(Array.Empty<byte>())
        };

        _mockHttpMessageHandler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync",
                ItExpr.IsAny<HttpRequestMessage>(),
                ItExpr.IsAny<CancellationToken>())
            .Callback<HttpRequestMessage, CancellationToken>((req, ct) => capturedRequest = req)
            .ReturnsAsync(responseMessage);

        var settings = TestOptions.Monitor(new SubsonicSettings { Url = "http://localhost:4533" });
        var service = new SubsonicProxyService(
            _mockHttpClientFactory.Object,
            settings,
            new HttpContextAccessor { HttpContext = new DefaultHttpContext() });

        // Stand in for settings.json being rewritten by the admin UI and reloaded.
        settings.Set(new SubsonicSettings { Url = "http://navidrome-moved:9999" });

        await service.RelayAsync("rest/ping", new Dictionary<string, string> { { "u", "admin" } });

        // The host is the point: the relay followed the new value with no rebuild.
        Assert.Contains("http://navidrome-moved:9999/rest/ping", capturedRequest!.RequestUri!.ToString());
        Assert.DoesNotContain("localhost:4533", capturedRequest.RequestUri.ToString());
    }

    [Fact]
    public async Task RelayAsync_EncodesSpecialCharacters()
    {
        // Arrange
        HttpRequestMessage? capturedRequest = null;
        var responseMessage = new HttpResponseMessage(HttpStatusCode.OK)
        {
            Content = new ByteArrayContent(Array.Empty<byte>())
        };

        _mockHttpMessageHandler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync",
                ItExpr.IsAny<HttpRequestMessage>(),
                ItExpr.IsAny<CancellationToken>())
            .Callback<HttpRequestMessage, CancellationToken>((req, ct) => capturedRequest = req)
            .ReturnsAsync(responseMessage);

        var parameters = new Dictionary<string, string>
        {
            { "query", "rock & roll" },
            { "artist", "AC/DC" }
        };

        // Act
        await _service.RelayAsync("rest/search3", parameters);

        // Assert
        Assert.NotNull(capturedRequest);
        var url = capturedRequest!.RequestUri!.ToString();
        // HttpClient automatically applies URL encoding when building the URI
        // Space can be encoded as + or %20, & as %26, / as %2F
        Assert.Contains("query=", url);
        Assert.Contains("artist=", url);
        Assert.Contains("AC%2FDC", url); // / should be encoded as %2F
    }

    [Fact]
    public async Task RelayAsync_HttpError_ThrowsException()
    {
        // Arrange
        var responseMessage = new HttpResponseMessage(HttpStatusCode.NotFound);

        _mockHttpMessageHandler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync",
                ItExpr.IsAny<HttpRequestMessage>(),
                ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync(responseMessage);

        var parameters = new Dictionary<string, string> { { "u", "admin" } };

        // Act & Assert
        await Assert.ThrowsAsync<HttpRequestException>(() => 
            _service.RelayAsync("rest/ping", parameters));
    }

    [Fact]
    public async Task RelaySafeAsync_SuccessfulRequest_ReturnsSuccessTrue()
    {
        // Arrange
        var responseContent = new byte[] { 1, 2, 3 };
        var responseMessage = new HttpResponseMessage(HttpStatusCode.OK)
        {
            Content = new ByteArrayContent(responseContent)
        };
        responseMessage.Content.Headers.ContentType = new System.Net.Http.Headers.MediaTypeHeaderValue("application/xml");

        _mockHttpMessageHandler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync",
                ItExpr.IsAny<HttpRequestMessage>(),
                ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync(responseMessage);

        var parameters = new Dictionary<string, string> { { "u", "admin" } };

        // Act
        var (body, contentType, success) = await _service.RelaySafeAsync("rest/ping", parameters);

        // Assert
        Assert.True(success);
        Assert.Equal(responseContent, body);
        Assert.Equal("application/xml", contentType);
    }

    [Fact]
    public async Task RelaySafeAsync_HttpError_ReturnsSuccessFalse()
    {
        // Arrange
        var responseMessage = new HttpResponseMessage(HttpStatusCode.InternalServerError);

        _mockHttpMessageHandler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync",
                ItExpr.IsAny<HttpRequestMessage>(),
                ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync(responseMessage);

        var parameters = new Dictionary<string, string> { { "u", "admin" } };

        // Act
        var (body, contentType, success) = await _service.RelaySafeAsync("rest/ping", parameters);

        // Assert
        Assert.False(success);
        Assert.Null(body);
        Assert.Null(contentType);
    }

    [Fact]
    public async Task RelaySafeAsync_NetworkException_ReturnsSuccessFalse()
    {
        // Arrange
        _mockHttpMessageHandler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync",
                ItExpr.IsAny<HttpRequestMessage>(),
                ItExpr.IsAny<CancellationToken>())
            .ThrowsAsync(new HttpRequestException("Network error"));

        var parameters = new Dictionary<string, string> { { "u", "admin" } };

        // Act
        var (body, contentType, success) = await _service.RelaySafeAsync("rest/ping", parameters);

        // Assert
        Assert.False(success);
        Assert.Null(body);
        Assert.Null(contentType);
    }

    [Fact]
    public async Task RelayStreamAsync_SuccessfulRequest_ReturnsFileStreamResult()
    {
        // Arrange
        var streamContent = new byte[] { 1, 2, 3, 4, 5 };
        var responseMessage = new HttpResponseMessage(HttpStatusCode.OK)
        {
            Content = new ByteArrayContent(streamContent)
        };
        responseMessage.Content.Headers.ContentType = new System.Net.Http.Headers.MediaTypeHeaderValue("audio/mpeg");

        _mockHttpMessageHandler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync",
                ItExpr.IsAny<HttpRequestMessage>(),
                ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync(responseMessage);

        var parameters = new Dictionary<string, string> 
        { 
            { "id", "song123" },
            { "u", "admin" }
        };

        // Act
        var result = await _service.RelayStreamAsync(parameters, CancellationToken.None);

        // Assert
        var fileResult = Assert.IsType<FileStreamResult>(result);
        Assert.Equal("audio/mpeg", fileResult.ContentType);
        Assert.True(fileResult.EnableRangeProcessing);
    }

    [Fact]
    public async Task RelayStreamAsync_HttpError_ReturnsStatusCodeResult()
    {
        // Arrange
        var responseMessage = new HttpResponseMessage(HttpStatusCode.NotFound);

        _mockHttpMessageHandler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync",
                ItExpr.IsAny<HttpRequestMessage>(),
                ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync(responseMessage);

        var parameters = new Dictionary<string, string> { { "id", "song123" } };

        // Act
        var result = await _service.RelayStreamAsync(parameters, CancellationToken.None);

        // Assert
        var statusResult = Assert.IsType<StatusCodeResult>(result);
        Assert.Equal(404, statusResult.StatusCode);
    }

    [Fact]
    public async Task RelayStreamAsync_Exception_ReturnsObjectResultWith500()
    {
        // Arrange
        _mockHttpMessageHandler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync",
                ItExpr.IsAny<HttpRequestMessage>(),
                ItExpr.IsAny<CancellationToken>())
            .ThrowsAsync(new HttpRequestException("Connection failed"));

        var parameters = new Dictionary<string, string> { { "id", "song123" } };

        // Act
        var result = await _service.RelayStreamAsync(parameters, CancellationToken.None);

        // Assert
        var objectResult = Assert.IsType<ObjectResult>(result);
        Assert.Equal(500, objectResult.StatusCode);
    }

    [Fact]
    public async Task RelayStreamAsync_DefaultContentType_UsesAudioMpeg()
    {
        // Arrange
        var streamContent = new byte[] { 1, 2, 3 };
        var responseMessage = new HttpResponseMessage(HttpStatusCode.OK)
        {
            Content = new ByteArrayContent(streamContent)
            // No ContentType set
        };

        _mockHttpMessageHandler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync",
                ItExpr.IsAny<HttpRequestMessage>(),
                ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync(responseMessage);

        var parameters = new Dictionary<string, string> { { "id", "song123" } };

        // Act
        var result = await _service.RelayStreamAsync(parameters, CancellationToken.None);

        // Assert
        var fileResult = Assert.IsType<FileStreamResult>(result);
        Assert.Equal("audio/mpeg", fileResult.ContentType);
    }

    [Fact]
    public async Task RelayStreamAsync_WithRangeHeader_ForwardsRangeToUpstream()
    {
        // Arrange
        HttpRequestMessage? capturedRequest = null;
        var streamContent = new byte[] { 1, 2, 3, 4, 5 };
        var responseMessage = new HttpResponseMessage(HttpStatusCode.PartialContent)
        {
            Content = new ByteArrayContent(streamContent)
        };
        responseMessage.Content.Headers.ContentType = new System.Net.Http.Headers.MediaTypeHeaderValue("audio/mpeg");

        _mockHttpMessageHandler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync",
                ItExpr.IsAny<HttpRequestMessage>(),
                ItExpr.IsAny<CancellationToken>())
            .Callback<HttpRequestMessage, CancellationToken>((req, ct) => capturedRequest = req)
            .ReturnsAsync(responseMessage);

        var httpContext = new DefaultHttpContext();
        httpContext.Request.Headers["Range"] = "bytes=0-1023";
        var httpContextAccessor = new HttpContextAccessor { HttpContext = httpContext };
        var service = new SubsonicProxyService(_mockHttpClientFactory.Object, 
            TestOptions.Monitor(new SubsonicSettings { Url = "http://localhost:4533" }), 
            httpContextAccessor);

        var parameters = new Dictionary<string, string> { { "id", "song123" } };

        // Act
        await service.RelayStreamAsync(parameters, CancellationToken.None);

        // Assert
        Assert.NotNull(capturedRequest);
        Assert.True(capturedRequest!.Headers.Contains("Range"));
        Assert.Equal("bytes=0-1023", capturedRequest.Headers.GetValues("Range").First());
    }

    [Fact]
    public async Task RelayStreamAsync_WithIfRangeHeader_ForwardsIfRangeToUpstream()
    {
        // Arrange
        HttpRequestMessage? capturedRequest = null;
        var streamContent = new byte[] { 1, 2, 3 };
        var responseMessage = new HttpResponseMessage(HttpStatusCode.OK)
        {
            Content = new ByteArrayContent(streamContent)
        };

        _mockHttpMessageHandler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync",
                ItExpr.IsAny<HttpRequestMessage>(),
                ItExpr.IsAny<CancellationToken>())
            .Callback<HttpRequestMessage, CancellationToken>((req, ct) => capturedRequest = req)
            .ReturnsAsync(responseMessage);

        var httpContext = new DefaultHttpContext();
        httpContext.Request.Headers["If-Range"] = "\"etag123\"";
        var httpContextAccessor = new HttpContextAccessor { HttpContext = httpContext };
        var service = new SubsonicProxyService(_mockHttpClientFactory.Object,
            TestOptions.Monitor(new SubsonicSettings { Url = "http://localhost:4533" }),
            httpContextAccessor);

        var parameters = new Dictionary<string, string> { { "id", "song123" } };

        // Act
        await service.RelayStreamAsync(parameters, CancellationToken.None);

        // Assert
        Assert.NotNull(capturedRequest);
        Assert.True(capturedRequest!.Headers.Contains("If-Range"));
    }

    [Fact]
    public async Task RelayStreamAsync_NullHttpContext_ReturnsError()
    {
        // Arrange
        var httpContextAccessor = new HttpContextAccessor { HttpContext = null };
        var service = new SubsonicProxyService(_mockHttpClientFactory.Object,
            TestOptions.Monitor(new SubsonicSettings { Url = "http://localhost:4533" }),
            httpContextAccessor);

        var parameters = new Dictionary<string, string> { { "id", "song123" } };

        // Act
        var result = await service.RelayStreamAsync(parameters, CancellationToken.None);

        // Assert
        var objectResult = Assert.IsType<ObjectResult>(result);
        Assert.Equal(500, objectResult.StatusCode);
    }

    /// <summary>A body that sends <paramref name="sent"/> bytes and then fails the way
    /// HttpClient does when a response ends before its announced Content-Length.</summary>
    private sealed class EndsEarlyStream(byte[] sent) : MemoryStream(sent)
    {
        public override int Read(byte[] buffer, int offset, int count)
        {
            var read = base.Read(buffer, offset, count);
            return read > 0 ? read : throw Ended();
        }

        public override async ValueTask<int> ReadAsync(Memory<byte> buffer, CancellationToken cancellationToken = default)
        {
            var read = await base.ReadAsync(buffer, cancellationToken);
            return read > 0 ? read : throw Ended();
        }

        public override Task<int> ReadAsync(byte[] buffer, int offset, int count, CancellationToken cancellationToken) =>
            ReadAsync(buffer.AsMemory(offset, count), cancellationToken).AsTask();

        private static HttpIOException Ended() => new(HttpRequestError.ResponseEnded,
            "The response ended prematurely, with at least 3 additional bytes expected. (ResponseEnded)");
    }

    private static HttpResponseMessage Upstream(HttpStatusCode status, byte[] sent, long announced, string? acceptRanges)
    {
        var response = new HttpResponseMessage(status) { Content = new StreamContent(new EndsEarlyStream(sent)) };
        response.Content.Headers.ContentLength = announced;
        response.Content.Headers.ContentType = new System.Net.Http.Headers.MediaTypeHeaderValue("audio/mpeg");
        if (acceptRanges is not null) response.Headers.AcceptRanges.Add(acceptRanges);
        return response;
    }

    private (SubsonicProxyService Service, DefaultHttpContext Context) ServiceAnswering(HttpResponseMessage upstream)
    {
        _mockHttpMessageHandler.Protected()
            .Setup<Task<HttpResponseMessage>>("SendAsync",
                ItExpr.IsAny<HttpRequestMessage>(),
                ItExpr.IsAny<CancellationToken>())
            .ReturnsAsync(upstream);
        var context = new DefaultHttpContext();
        context.Request.Method = "GET";
        var service = new SubsonicProxyService(_mockHttpClientFactory.Object,
            TestOptions.Monitor(new SubsonicSettings { Url = "http://localhost:4533" }),
            new HttpContextAccessor { HttpContext = context });
        return (service, context);
    }

    private static readonly Dictionary<string, string> EstimatedTranscode = new()
    {
        { "id", "song123" }, { "format", "mp3" }, { "maxBitRate", "192" }, { "estimateContentLength", "true" },
    };

    [Fact]
    public async Task RelayStreamAsync_EstimatedTranscode_LeavesTheLengthOut_AndEndsAtTheRealEnd()
    {
        var sent = Enumerable.Range(0, 97).Select(i => (byte)i).ToArray();
        var (service, context) = ServiceAnswering(Upstream(HttpStatusCode.OK, sent, 100, "none"));

        var result = await service.RelayStreamAsync(EstimatedTranscode, CancellationToken.None);

        var file = Assert.IsType<FileStreamResult>(result);
        Assert.False(context.Response.Headers.ContainsKey("Content-Length"));
        Assert.Equal("none", context.Response.Headers.AcceptRanges.ToString());
        using var received = new MemoryStream();
        await file.FileStream.CopyToAsync(received);
        Assert.Equal(sent, received.ToArray());
        // Synchronous readers end cleanly too, and a read after the end stays at the end.
        Assert.Equal(0, file.FileStream.Read(new byte[8], 0, 8));
    }

    [Fact]
    public async Task RelayStreamAsync_CachedTranscode_KeepsItsExactLength()
    {
        // Navidrome serves a transcode it has finished from its cache, with ranges and the
        // real size, even when the client asked for an estimate.
        var sent = new byte[] { 1, 2, 3 };
        var (service, context) = ServiceAnswering(Upstream(HttpStatusCode.OK, sent, 3, "bytes"));

        var result = await service.RelayStreamAsync(EstimatedTranscode, CancellationToken.None);

        Assert.IsType<FileStreamResult>(result);
        Assert.Equal("3", context.Response.Headers.ContentLength?.ToString());
    }

    [Fact]
    public async Task RelayStreamAsync_RawStreamThatEndsEarly_IsNotPassedOffAsWhole()
    {
        // An exact length that is not met is a real failure, and must not read as a clean end.
        var (service, context) = ServiceAnswering(Upstream(HttpStatusCode.OK, [1, 2, 3], 6, "bytes"));

        var result = await service.RelayStreamAsync(new Dictionary<string, string> { { "id", "song123" } },
            CancellationToken.None);

        var file = Assert.IsType<FileStreamResult>(result);
        Assert.Equal("6", context.Response.Headers.ContentLength?.ToString());
        await Assert.ThrowsAsync<HttpIOException>(() => file.FileStream.CopyToAsync(Stream.Null));
    }

    [Fact]
    public async Task RelayStreamAsync_RangeNotSatisfiable_PassesTheRealSizeOn()
    {
        var upstream = new HttpResponseMessage(HttpStatusCode.RequestedRangeNotSatisfiable)
        {
            Content = new ByteArrayContent([]),
        };
        upstream.Content.Headers.ContentRange = new System.Net.Http.Headers.ContentRangeHeaderValue(3974093);
        var (service, context) = ServiceAnswering(upstream);

        var result = await service.RelayStreamAsync(EstimatedTranscode, CancellationToken.None);

        Assert.Equal(416, Assert.IsType<StatusCodeResult>(result).StatusCode);
        Assert.Equal("bytes */3974093", context.Response.Headers["Content-Range"].ToString());
    }

    [Fact]
    public async Task RelayRawAsync_DownloadOfAnEstimatedTranscode_KeepsEveryByte()
    {
        var sent = Enumerable.Range(0, 97).Select(i => (byte)i).ToArray();
        var (service, _) = ServiceAnswering(Upstream(HttpStatusCode.OK, sent, 100, "none"));

        var result = await service.RelayRawAsync("rest/download", EstimatedTranscode);

        Assert.Equal(200, result.Status);
        Assert.Equal(sent, result.Body);
    }

    [Theory]
    // Navidrome's own marks win: a live transcode says none, a file or cached transcode bytes.
    [InlineData(200, "none", "", true)]
    [InlineData(200, "bytes", "format=mp3&maxBitRate=192&estimateContentLength=true", false)]
    [InlineData(200, "none", "format=mp3&maxBitRate=192&estimateContentLength=true", true)]
    // A 206 carries an exact Content-Range.
    [InlineData(206, null, "format=mp3&estimateContentLength=true", false)]
    // With no mark, the request decides.
    [InlineData(200, null, "estimateContentLength=true", true)]
    [InlineData(200, null, "format=mp3", true)]
    [InlineData(200, null, "maxBitRate=128", true)]
    [InlineData(200, null, "format=raw", false)]
    [InlineData(200, null, "maxBitRate=0", false)]
    [InlineData(200, null, "estimateContentLength=false", false)]
    [InlineData(200, null, "", false)]
    public void LengthMayBeEstimate_FollowsNavidromeThenTheRequest(int status, string? acceptRanges, string query,
        bool expected)
    {
        using var response = new HttpResponseMessage((HttpStatusCode)status);
        if (acceptRanges is not null) response.Headers.AcceptRanges.Add(acceptRanges);
        var parameters = query.Split('&', StringSplitOptions.RemoveEmptyEntries)
            .Select(pair => pair.Split('='))
            .ToDictionary(pair => pair[0], pair => pair[1]);

        Assert.Equal(expected, SubsonicProxyService.LengthMayBeEstimate(response, parameters));
    }
}
