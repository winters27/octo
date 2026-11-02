# syntax=docker/dockerfile:1.10
# Octo: the Rust server, published as ghcr.io/winters27/octo for amd64 and arm64 (docker.yml),
# and built from this folder by docker-compose.yml's octo-source. The same contract as the C#
# image it replaces: port 8080 (ASPNETCORE_URLS), /app/config for settings and state, /music for
# the library, /app/downloads created, and the app itself as PID 1 so SIGTERM reaches it and it
# drains requests and workers within its 10 s budget. The C# app stays in octo/ for now, with
# its own Dockerfile.csharp; this image holds none of it.
#
# It reads what the C# app already holds: the dashboard (octo/wwwroot), the logo and social
# images (octo/Assets), the list cover design (octo/Services/CoverArt/Design, built into the
# binary) and appsettings.json (built in). The release number is the VERSION file (else
# octo/octo.csproj, octo-core's build.rs). The place data getPlace reads (GeoNames' postal
# codes) is downloaded and converted here, at build time, never by the running server.
#
#   docker build -t octo:dev .
#   docker build --build-arg OCTO_VERSION=2026.11.01.1 -t octo:dev .
#   docker build --build-arg CARGO_BUILD_JOBS=2 -t octo:dev .   # small machine
#   docker build --build-arg PLACES_REFRESH=2026-11-02 -t octo:dev .   # fresh place data
#   docker build --build-arg PLACES_DOWNLOAD=0 -t octo:dev .   # no place data

ARG RUST_VERSION=1.99
ARG CHEF_VERSION=0.1.78

# ---- Toolchain + cargo-chef, prebuilt ----
# Bookworm, like the runtime stage below: a binary linked against a newer glibc (the plain
# rust-1.99 tags are Debian trixie now) would not start on bookworm-slim.
FROM lukemathwalker/cargo-chef:${CHEF_VERSION}-rust-${RUST_VERSION}-bookworm AS chef
ENV CARGO_TERM_COLOR=never \
    CARGO_INCREMENTAL=0
WORKDIR /src

# ---- Dependency recipe: changes only when a Cargo.toml or Cargo.lock does ----
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo chef prepare --recipe-path recipe.json

# ---- Build ----
FROM chef AS build
# Unset by default, so cargo uses every core the builder has. A machine short of memory for
# the LTO link and the heavier crates (lofty, image, cosmic-text) passes 2.
ARG CARGO_BUILD_JOBS
# Unset by default, so the release profile's thin LTO applies. "off" builds the same program
# without the 3 GB link, for a host short of memory.
ARG CARGO_PROFILE_RELEASE_LTO
COPY --from=planner /src/recipe.json recipe.json
# Every third-party crate, compiled once and cached in this layer.
RUN cargo chef cook --release --locked --package octo --recipe-path recipe.json
# The third-party crates of octo-places, the place data converter, once more without LTO (see
# its build below).
RUN CARGO_PROFILE_RELEASE_LTO=off cargo chef cook --release --locked --package octo-core --recipe-path recipe.json

COPY Cargo.toml Cargo.lock ./
COPY crates crates
# The release (VERSION; octo-core's build.rs reads it unless OCTO_VERSION is set), and what the
# binary compiles in from the C# tree: the list cover design (JSON, Inter fonts, painted
# backgrounds) and appsettings.json. octo.csproj is build.rs's fallback for a tree without
# VERSION.
COPY VERSION ./
COPY octo/Services/CoverArt/Design octo/Services/CoverArt/Design
COPY octo/appsettings.json octo/appsettings.Development.json octo/octo.csproj octo/
ARG OCTO_VERSION
# octo-places, the converter the place data stage below runs (it is not in the image): built
# from octo-core alone and without LTO, so it needs no large link, and its own RUN, so the
# conversion below runs again only when the converter's bytes change.
RUN CARGO_PROFILE_RELEASE_LTO=off cargo build --release --locked --package octo-core --bin octo-places \
    && install -D -m 0755 target/release/octo-places /out/places/octo-places
# Octo's own AcoustID application key, compiled into the server (octo-core's soulseek settings
# read it with option_env!), never written in the source. The published images are given it by
# docker.yml as the build secret acoustid_key (from the repository's secret), mounted into this
# one step as an environment variable: a secret is never a build argument, so no layer, no
# image history and no provenance holds it. OCTO_REQUIRE_ACOUSTID_KEY=1 fails the build when it
# is blank. A build from source given neither has no built-in key: AcoustID then needs a key of
# the user's own. OCTO_ACOUSTID_KEY_SHA256 is the key's hash, never the key: a build with a
# rotated key differs in it, so this step runs again instead of reusing a cached build with the
# old key.
ARG OCTO_REQUIRE_ACOUSTID_KEY
ARG OCTO_ACOUSTID_KEY_SHA256
RUN --mount=type=secret,id=acoustid_key,env=OCTO_ACOUSTID_APPLICATION_KEY \
    echo "Octo's own AcoustID key: ${OCTO_ACOUSTID_KEY_SHA256:-none given} (its SHA-256)" \
    && if [ -n "$OCTO_REQUIRE_ACOUSTID_KEY" ] && [ -z "$(printf '%s' "${OCTO_ACOUSTID_APPLICATION_KEY:-}" | tr -d '[:space:]')" ]; then \
        echo "The acoustid_key build secret is blank, and this build needs Octo's own AcoustID key." >&2; exit 1; \
    fi \
    && cargo build --release --locked --package octo --bin octo \
    && install -D -m 0755 target/release/octo /out/octo

# ---- Place data: the download ----
# GeoNames' postal codes (https://download.geonames.org/export/zip/, CC BY 4.0) and its readme,
# which carries the license. The download is cached like any layer, keyed by the release
# (VERSION), PLACES_REFRESH (docker-compose.yml passes it through, and the update helper sets
# it) and the files in places-data/ when the build context has them: docker.yml downloads them
# once per run, for both platforms, so both have the same data. Those are used as they are.
# Without place data (GeoNames cannot be reached, or PLACES_DOWNLOAD=0) the image is built
# anyway and the build log says so in a warning; getPlace then answers error 70 for every place
# and octoPlaces is not listed. PLACES_REQUIRED=1 (docker.yml) fails the build instead.
FROM debian:bookworm-slim AS geonames
ARG PLACES_URL=https://download.geonames.org/export/zip
ARG PLACES_REQUIRED=0
ARG PLACES_DOWNLOAD=1
ARG PLACES_REFRESH
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /geonames
# The folder's name as a pattern, so a context without places-data/ copies VERSION alone.
COPY VERSION places-dat[a] /geonames/
RUN release="$(tr -d ' \r\n' < VERSION)" && rm VERSION \
    && echo "Place data for Octo $release${PLACES_REFRESH:+ ($PLACES_REFRESH)}" \
    && if [ -f allCountries.zip ] && [ -f readme.txt ]; then \
         echo "Place data from the build context's places-data/"; \
       elif [ "$PLACES_DOWNLOAD" = 0 ]; then \
         echo "PLACES_DOWNLOAD=0: no place data is downloaded"; \
       elif curl -fsSL --retry 3 -o allCountries.zip "$PLACES_URL/allCountries.zip" \
            && curl -fsSL --retry 3 -o readme.txt "$PLACES_URL/readme.txt"; then \
         echo "Place data downloaded from $PLACES_URL"; \
       else \
         echo "GeoNames could not be reached at $PLACES_URL" >&2; \
       fi \
    && if [ ! -f allCountries.zip ] || [ ! -f readme.txt ]; then \
         rm -f allCountries.zip readme.txt; \
         if [ "$PLACES_REQUIRED" = 1 ]; then \
           echo "PLACES_REQUIRED=1 and there is no place data" >&2; exit 1; \
         fi; \
         printf '%s\n' "" \
           "WARNING: THIS IMAGE HAS NO PLACE DATA." \
           "getPlace will answer error 70 for every place, and octoPlaces will not be listed." \
           "Build again with --build-arg PLACES_REFRESH=<anything new> to try the download again." \
           "" >&2; \
       fi \
    && ls -l

# ---- Place data: the conversion ----
# Into /out/data/places.bin, deflated blocks of sorted lines that getPlace reads a block at a
# time (octo_core::places), with the readme beside the other licenses. Runs again only when the
# download above or the converter's bytes change.
FROM debian:bookworm-slim AS places
RUN apt-get update && apt-get install -y --no-install-recommends unzip \
    && rm -rf /var/lib/apt/lists/*
COPY --from=geonames /geonames /geonames
COPY --from=build /out/places/octo-places /usr/local/bin/octo-places
RUN mkdir -p /out/data /out/licenses \
    && if [ -f /geonames/allCountries.zip ]; then \
         unzip -q -d /tmp /geonames/allCountries.zip allCountries.txt \
         && octo-places /tmp/allCountries.txt /out/data/places.bin \
         && cp /geonames/readme.txt /out/licenses/GeoNames-postal-codes.txt \
         && rm /tmp/allCountries.txt; \
       else \
         printf '%s\n' "" "WARNING: THIS IMAGE HAS NO PLACE DATA (see the download step)." "" >&2; \
       fi

# ---- Runtime ----
FROM debian:bookworm-slim
WORKDIR /app

# ffmpeg: Continuous Subsonic Radio normalizes mixed FLAC/M4A sources into one stable MP3
# response inside the Octo process, and the loudness and spectrum checks read through it.
# fpcalc (libchromaprint-tools) is the other half of download verification: it turns a
# finished download into the Chromaprint fingerprint AcoustID is asked about. Absent, the
# feature degrades to a no-op and logs once; it never fails a download.
# Generated list covers set names in Inter (inside the app); Noto CJK and Symbola draw the
# Chinese, Japanese, Korean and emoji names Inter has no letters for, DejaVu Arabic and Hebrew.
# ca-certificates: the .NET base image carried a root store; slim Debian does not, and every
# upstream (Deezer, MusicBrainz, Last.fm, GitHub, ...) is HTTPS.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates ffmpeg fonts-dejavu-core fonts-noto-cjk fonts-symbola libchromaprint-tools \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /app/downloads /app/config

# The binary looks for wwwroot/ and Assets/ beside itself (StaticRoots::from_env, and the
# cover service's logo lookup), as the C# app looked in its publish root.
COPY --from=build /out/octo /app/octo
COPY octo/wwwroot /app/wwwroot
COPY octo/Assets /app/Assets
# Inter is compiled into the binary; its license ships beside it.
COPY octo/Services/CoverArt/Design/Fonts/OFL.txt /app/licenses/Inter-OFL.txt
# The place data getPlace reads (data/places.bin beside the binary) and its license.
COPY --from=places /out/ /app/

# No version label here: the published image gets org.opencontainers.image.version (and the
# rest) from docker.yml's metadata step, and the release itself is in the binary.
LABEL org.opencontainers.image.title="octo" \
      org.opencontainers.image.licenses="GPL-3.0-only"

EXPOSE 8080
ENV ASPNETCORE_URLS=http://+:8080

# Exec form: octo is PID 1 and receives SIGTERM from `docker stop` directly.
ENTRYPOINT ["/app/octo"]
