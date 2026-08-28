# Build stage
FROM mcr.microsoft.com/dotnet/sdk:9.0 AS build
WORKDIR /src

COPY octo.sln .
COPY octo/octo.csproj octo/
COPY octo.Tests/octo.Tests.csproj octo.Tests/

RUN dotnet restore

COPY octo/ octo/
COPY octo.Tests/ octo.Tests/

RUN dotnet publish octo/octo.csproj -c Release -o /app/publish

# Runtime stage
FROM mcr.microsoft.com/dotnet/aspnet:9.0
WORKDIR /app

# Continuous Subsonic Radio normalizes mixed FLAC/M4A sources into one stable
# MP3 response inside the core Octo process. This is a runtime dependency, not a
# Radio sidecar or service boundary.
RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg fonts-dejavu-core \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /app/downloads

COPY --from=build /app/publish .

EXPOSE 8080
ENV ASPNETCORE_URLS=http://+:8080

ENTRYPOINT ["dotnet", "octo.dll"]
