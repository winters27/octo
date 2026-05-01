FROM python:3.12-slim

ARG YTDLP_VERSION=latest

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl ffmpeg \
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

RUN pip install --no-cache-dir flask gunicorn requests

COPY app.py /app/app.py

EXPOSE 8080

# Single worker, threaded — yt-dlp is the bottleneck not Python; threading is fine.
CMD ["gunicorn", "-b", "0.0.0.0:8080", "-w", "1", "--threads", "8", "--timeout", "120", "app:app"]
