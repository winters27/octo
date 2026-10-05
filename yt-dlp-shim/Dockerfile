FROM python:3.12-slim

ARG YTDLP_VERSION=latest
ARG DENO_VERSION=latest

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl ffmpeg unzip \
    && rm -rf /var/lib/apt/lists/*

# Pull yt-dlp directly from GitHub releases. Pinning to "latest" by default so
# rebuilds keep up with YouTube's frequent format-extractor breakage; pin to a
# specific tag with --build-arg YTDLP_VERSION=2025.10.22 if you need stability.
RUN if [ "$YTDLP_VERSION" = "latest" ]; then \
        curl -fsSL -o /usr/local/bin/yt-dlp https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp; \
    else \
        curl -fsSL -o /usr/local/bin/yt-dlp https://github.com/yt-dlp/yt-dlp/releases/download/${YTDLP_VERSION}/yt-dlp; \
    fi \
    && chmod +x /usr/local/bin/yt-dlp \
    && yt-dlp --version

# yt-dlp solves YouTube's JavaScript challenges (signatures, nsig) with an
# external runtime. Without one it warns that "YouTube extraction without a JS
# runtime has been deprecated, and some formats may be missing", and upstream
# says availability will keep getting worse. deno is the only runtime yt-dlp
# enables by default, so having it on PATH is the whole integration -- no flag,
# unlike node/quickjs/bun which need --js-runtimes. Minimum supported is 2.3.0.
# https://github.com/yt-dlp/yt-dlp/wiki/EJS
RUN set -eux; \
    case "$(dpkg --print-architecture)" in \
        amd64) target="x86_64-unknown-linux-gnu" ;; \
        arm64) target="aarch64-unknown-linux-gnu" ;; \
        *) echo "unsupported arch for deno: $(dpkg --print-architecture)" >&2; exit 1 ;; \
    esac; \
    if [ "$DENO_VERSION" = "latest" ]; then \
        url="https://github.com/denoland/deno/releases/latest/download/deno-${target}.zip"; \
    else \
        url="https://github.com/denoland/deno/releases/download/${DENO_VERSION}/deno-${target}.zip"; \
    fi; \
    curl -fsSL -o /tmp/deno.zip "$url"; \
    unzip -q /tmp/deno.zip -d /usr/local/bin; \
    rm /tmp/deno.zip; \
    chmod +x /usr/local/bin/deno; \
    deno --version

RUN pip install --no-cache-dir flask gunicorn requests "ytmusicapi>=1.12,<2"

# gate.py and ytm.py too, not just app.py -- app.py imports them at module scope, so omitting
# it here is a ModuleNotFoundError at startup and total loss of playback. The
# shim image is never built in CI, so nothing else would catch that.
COPY app.py gate.py ytm.py /app/

EXPOSE 8080

# Single worker, threaded — yt-dlp is the bottleneck not Python; threading is fine.
# Threads are sized well above MAX_CONCURRENT_YTDLP deliberately: /stream holds
# its thread for the whole song, so at 8 threads a few simultaneous playbacks
# could consume the worker and leave gate slots idle with nobody able to reach
# them. A thread parked on a socket read costs a stack and nothing else.
CMD ["gunicorn", "-b", "0.0.0.0:8080", "-w", "1", "--threads", "32", "--timeout", "120", "app:app"]
