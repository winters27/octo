"""
yt-dlp shim — a tiny Flask service that wraps yt-dlp behind two endpoints.

Why this is a separate container instead of in-process inside Octo:
  Spawning yt-dlp from .NET via System.Diagnostics.Process inside an LXC
  produced wedge conditions on client cancellation (the LXC's namespace
  state would lock up). Isolating yt-dlp here means Octo only ever talks
  HTTP to this service, and any process-management quirks stay sandboxed.

Endpoints:
  GET /search?q=<query>
      Runs `yt-dlp ytsearch1:<query>` and returns {video_id, title, duration}.

  GET /stream?id=<videoId>
      Resolves the best-audio URL via `yt-dlp -g`, then proxies bytes
      from YouTube's CDN to the caller. Streaming chunks so this stays
      flat-memory regardless of song length, and so cancellation is
      honored immediately (the upstream connection closes when the
      caller disconnects).
"""
from __future__ import annotations

import json
import logging
import os
import subprocess
import threading
from typing import Optional

import requests
from flask import Flask, Response, abort, jsonify, request, stream_with_context

YTDLP = os.environ.get("YTDLP_PATH", "/usr/local/bin/yt-dlp")
PORT = int(os.environ.get("PORT", "8080"))

# In-memory LRU cache of search-query -> json result. Cuts repeat searches
# from a 3-8s yt-dlp invocation to a dict lookup. Bounded so we never grow
# without limit. The cache is best-effort; restarts wipe it.
from collections import OrderedDict
_SEARCH_CACHE_LOCK = threading.Lock()
_SEARCH_CACHE: "OrderedDict[str, dict]" = OrderedDict()
_SEARCH_CACHE_MAX = int(os.environ.get("SEARCH_CACHE_MAX", "1024"))

# Per-video stream URL cache. yt-dlp -g returns a signed YouTube CDN URL good
# for several hours; we cache it so /stream calls don't re-invoke yt-dlp for
# tracks the user just searched.
_URL_CACHE_LOCK = threading.Lock()
_URL_CACHE: "OrderedDict[str, tuple[float, str]]" = OrderedDict()
_URL_CACHE_MAX = int(os.environ.get("URL_CACHE_MAX", "512"))
_URL_CACHE_TTL = int(os.environ.get("URL_CACHE_TTL", "3600"))  # 1 hour, well under signed-URL lifetime

def _cache_get(key: str):
    with _SEARCH_CACHE_LOCK:
        v = _SEARCH_CACHE.get(key)
        if v is not None:
            _SEARCH_CACHE.move_to_end(key)
        return v

def _cache_put(key: str, value: dict):
    with _SEARCH_CACHE_LOCK:
        _SEARCH_CACHE[key] = value
        _SEARCH_CACHE.move_to_end(key)
        while len(_SEARCH_CACHE) > _SEARCH_CACHE_MAX:
            _SEARCH_CACHE.popitem(last=False)

def _url_cache_get(video_id: str):
    import time as _t
    with _URL_CACHE_LOCK:
        entry = _URL_CACHE.get(video_id)
        if not entry:
            return None
        ts, url = entry
        if _t.time() - ts > _URL_CACHE_TTL:
            del _URL_CACHE[video_id]
            return None
        _URL_CACHE.move_to_end(video_id)
        return url

def _url_cache_put(video_id: str, url: str):
    import time as _t
    with _URL_CACHE_LOCK:
        _URL_CACHE[video_id] = (_t.time(), url)
        _URL_CACHE.move_to_end(video_id)
        while len(_URL_CACHE) > _URL_CACHE_MAX:
            _URL_CACHE.popitem(last=False)

# Cap concurrent yt-dlp processes globally. Each one is fork+exec heavy;
# letting them stack starves a small container.
_GATE = threading.Semaphore(int(os.environ.get("MAX_CONCURRENT_YTDLP", "5")))
# Max time a queued request will wait for a free slot. Long enough that a
# burst of 10 parallel radio-resolution searches all eventually succeed
# rather than dropping requests on the floor.
_GATE_WAIT_SEC = int(os.environ.get("GATE_WAIT_SEC", "45"))

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
log = logging.getLogger("ytdlp-shim")

app = Flask(__name__)


def _run(args: list[str], timeout: int = 20) -> Optional[str]:
    """Run yt-dlp with a hard timeout. Returns stdout or None on any failure."""
    if not _GATE.acquire(timeout=_GATE_WAIT_SEC):
        log.warning("yt-dlp gate full after %ds, dropping: %s", _GATE_WAIT_SEC, " ".join(args))
        return None
    try:
        full = [YTDLP, "--no-warnings", "--no-cache-dir", "--no-playlist", *args]
        try:
            cp = subprocess.run(
                full,
                capture_output=True,
                text=True,
                timeout=timeout,
                check=False,
            )
        except subprocess.TimeoutExpired:
            log.warning("yt-dlp timed out: %s", " ".join(args))
            return None
        if cp.returncode != 0:
            log.warning("yt-dlp exit %d: %s", cp.returncode, cp.stderr.strip()[:300])
            return None
        return cp.stdout
    finally:
        _GATE.release()


@app.get("/health")
def health():
    return jsonify(ok=True)


@app.get("/search")
def search():
    q = request.args.get("q", "").strip()
    if not q:
        abort(400, "missing q")

    cache_key = q.lower()
    cached = _cache_get(cache_key)
    if cached is not None:
        return jsonify(**cached)

    out = _run(
        [
            f"ytsearch1:{q}",
            "--print",
            "%(.{id,title,duration,channel})j",
        ],
        timeout=15,
    )
    if not out:
        return jsonify(error="search_failed"), 502

    line = out.strip().split("\n", 1)[0].strip()
    if not line:
        return jsonify(error="no_hit"), 404
    try:
        data = json.loads(line)
    except json.JSONDecodeError as e:
        log.warning("search: bad json from yt-dlp: %s", e)
        return jsonify(error="bad_yt_response"), 502

    payload = {
        "video_id": data.get("id"),
        "title": data.get("title"),
        "duration": data.get("duration"),
        "channel": data.get("channel"),
    }
    if payload["video_id"]:
        _cache_put(cache_key, payload)
        # Pre-resolve the stream URL in the background so /stream is instant
        # when the user actually plays this track. Best-effort; failures are
        # silent and just mean /stream pays the lookup cost on first hit.
        threading.Thread(
            target=_prefetch_stream_url,
            args=(payload["video_id"],),
            daemon=True,
        ).start()
    return jsonify(**payload)


def _prefetch_stream_url(video_id: str) -> None:
    if _url_cache_get(video_id) is not None:
        return
    url = _resolve_url(video_id)
    if url:
        _url_cache_put(video_id, url)
        log.info("prefetched stream url for %s", video_id)


def _resolve_url(video_id: str) -> Optional[str]:
    cached = _url_cache_get(video_id)
    if cached:
        return cached
    # Format 140 is audio-only m4a (AAC). Despite often being labeled "DASH" in
    # yt-dlp's table, `yt-dlp -g` returns a single contiguous googlevideo.com
    # URL with a Content-Length — proxying the body through requests.get works.
    # Format 18 (the muxed 360p mp4 we used to use) is being phased out by
    # YouTube and is missing on a growing share of videos; relying on it gave
    # us many silent /stream 502s. Audio-only is what the Subsonic client
    # actually wants anyway.
    out = _run(
        [
            "-g",
            "-f",
            "140/bestaudio[ext=m4a]/bestaudio[ext=webm]/bestaudio",
            f"https://www.youtube.com/watch?v={video_id}",
        ],
        timeout=15,
    )
    if not out:
        return None
    url = out.strip().split("\n", 1)[0].strip()
    if url:
        _url_cache_put(video_id, url)
    return url or None


@app.get("/stream")
def stream():
    video_id = request.args.get("id", "").strip()
    if not video_id:
        abort(400, "missing id")

    url = _resolve_url(video_id)
    if not url:
        return jsonify(error="no_audio_url"), 502

    # Forward the caller's Range header to googlevideo, which supports byte
    # ranges natively. iOS Subsonic clients (Arpeggi, Narjo) probe with
    # `Range: bytes=0-1` first and refuse to play if the server can't satisfy
    # range requests on an audio/mp4 container — without this passthrough they
    # silently drop the song from the queue. Browsers and Feishin don't care
    # about Range for short clips, hence why those clients worked anyway.
    upstream_headers = {}
    incoming_range = request.headers.get("Range")
    if incoming_range:
        upstream_headers["Range"] = incoming_range

    try:
        upstream = requests.get(url, stream=True, timeout=(8, 30), headers=upstream_headers)
        # Accept both 200 (full body) and 206 (partial). Anything else is a real
        # failure we need to surface.
        if upstream.status_code not in (200, 206):
            log.warning("stream upstream %d for %s", upstream.status_code, video_id)
            return jsonify(error="upstream_failed", status=upstream.status_code), 502
    except Exception as e:
        log.warning("stream upstream failed for %s: %s", video_id, e)
        return jsonify(error="upstream_failed"), 502

    @stream_with_context
    def generator():
        try:
            for chunk in upstream.iter_content(chunk_size=64 * 1024):
                if chunk:
                    yield chunk
        finally:
            try:
                upstream.close()
            except Exception:
                pass

    # Reflect upstream's status (200 for full body, 206 for partial). Forward
    # the metadata that AVPlayer / Subsonic clients need to seek correctly.
    headers = {
        "Content-Type": upstream.headers.get("Content-Type", "audio/mp4"),
        "Accept-Ranges": "bytes",
        "Cache-Control": "no-store",
    }
    for h in ("Content-Length", "Content-Range"):
        v = upstream.headers.get(h)
        if v is not None:
            headers[h] = v
    return Response(generator(), headers=headers, status=upstream.status_code)


if __name__ == "__main__":
    log.info("yt-dlp-shim listening on :%d using %s", PORT, YTDLP)
    app.run(host="0.0.0.0", port=PORT, threaded=True)
