"""
yt-dlp shim — a tiny Flask service that wraps yt-dlp behind two endpoints.

Why this is a separate container instead of in-process inside Octo:
  Spawning yt-dlp from .NET via System.Diagnostics.Process inside an LXC
  produced wedge conditions on client cancellation (the LXC's namespace
  state would lock up). Isolating yt-dlp here means Octo only ever talks
  HTTP to this service, and any process-management quirks stay sandboxed.

Endpoints:
  GET /search?q=<query>[&duration=<sec>]
      One yt-dlp extraction returning {video_id, title, duration, channel}.
      Without a duration hint it also resolves and caches the stream URL in
      the SAME extraction, so a later /stream is a deterministic cache hit.
      With a hint it picks the closest-length of several flat candidates
      (cheap, no per-video extraction), then resolves the winner's URL.

  GET /facts?id=<videoId>
      The stream /stream would send for a video: format id (itag), exact
      size in bytes, container and bit rate. /search carries the same fields
      for the video it found.

  GET /stream?id=<videoId>
      Resolves the best-audio URL (cached), then proxies bytes from
      YouTube's CDN to the caller. Streaming chunks so this stays
      flat-memory regardless of song length, and so cancellation is
      honored immediately (the upstream connection closes when the
      caller disconnects).
"""
from __future__ import annotations

import json
import logging
import os
import re
import shutil
import sqlite3
import subprocess
import threading
import time
import unicodedata
from typing import Optional

import requests
from requests.adapters import HTTPAdapter
from flask import Flask, Response, abort, jsonify, request, stream_with_context

import ytm

import facts as stream_info
import loudness as loudness_info
from gate import TieredGate
from loudness import LoudnessBook

# The binary baked into the image. Never written to, so a rebuild is always a
# clean floor to fall back to.
YTDLP_DIST = os.environ.get("YTDLP_DIST_PATH", "/usr/local/bin/yt-dlp")
# The binary actually executed. Compose points this at a volume so a self-update
# survives `docker compose up -d` recreating the container; left at the dist path
# the shim still works, it just re-updates after each recreate.
YTDLP = os.environ.get("YTDLP_PATH", YTDLP_DIST)
PORT = int(os.environ.get("PORT", "8080"))

# Root the /download endpoint is allowed to write under (the shared music mount).
# dest arrives over the wire, so downloads are confined here even though the shim
# is internal-only.
_DOWNLOAD_ROOT = os.path.realpath(os.environ.get("DOWNLOAD_ROOT", "/music"))

# Audio format selector shared by /search (single-extraction warm) and
# /stream's _resolve_url (cold path), so both request the exact same format.
# Format 140 is audio-only m4a (AAC); yt-dlp -g / --print returns one
# contiguous googlevideo URL with a Content-Length for it. Format 18 (the old
# muxed 360p mp4) is being phased out by YouTube and 502s on a growing share.
_AUDIO_FORMAT = os.environ.get(
    "YTDLP_AUDIO_FORMAT",
    "140/bestaudio[ext=m4a]/bestaudio[ext=webm]/bestaudio",
)

# The YouTube player client asked for stream URLs. yt-dlp's own clients hand out URLs that
# googlevideo refuses until 4 to 14 s after the resolve (their `available_at`); the visionos
# client's are ready as the resolve ends. Empty uses yt-dlp's own choice. When this client gives
# no URL but yt-dlp's own does, the client is left out for _PLAYER_CLIENT_REST_SEC.
_PLAYER_CLIENT = os.environ.get("YTDLP_PLAYER_CLIENT", "visionos").strip()
_PLAYER_CLIENT_REST_SEC = int(os.environ.get("YTDLP_PLAYER_CLIENT_REST_SEC", "1800"))
# The longest a fetch waits for its URL's `available_at`. A URL further off than this is
# fetched at the limit and left to the 403 path.
_URL_READY_WAIT_MAX_SEC = float(os.environ.get("URL_READY_WAIT_MAX_SEC", "20"))

# User-Agent sent to googlevideo when proxying bytes. Empty (the default) keeps
# requests' own python-requests/x.y, i.e. existing behaviour. Signed URLs from
# some player clients are refused for a mismatched UA, so this exists to test
# that without a code change; it is off by default so it cannot confound the
# 403 diagnostics in stream().
_UPSTREAM_UA = os.environ.get("UPSTREAM_USER_AGENT", "").strip()

# yt-dlp's cache holds the player base.js and solved signatures, and is
# deliberately persistent (see the note in _run). Nothing invalidates it, so a
# rotated player leaves every resolve producing URLs googlevideo refuses.
# Purging is the standard recovery, but doing it on every 403 would throw the
# cache away during a burst and make things worse -- hence the interval.
_CACHE_PURGE_MIN_INTERVAL_SEC = int(os.environ.get("CACHE_PURGE_MIN_INTERVAL_SEC", "900"))
# A 403 that outlives both a re-resolve and a cache purge is usually a yt-dlp too
# old for YouTube's current player. Updating is rate-limited hard: it is a
# network fetch of a binary, and hammering it during an outage helps nobody.
_SELF_UPDATE_ON_403 = os.environ.get("SELF_UPDATE_ON_403", "1").lower() not in ("0", "false", "no")
_SELF_UPDATE_MIN_INTERVAL_SEC = int(os.environ.get("SELF_UPDATE_MIN_INTERVAL_SEC", "21600"))
_SELF_UPDATE_LOCK = threading.Lock()
# None means "never attempted". Not 0.0: time.monotonic() is time since boot on
# Linux, so on a host up for less than the interval, `now - 0.0` is inside the
# window and the very first self-update would be rate-limited away. That is
# precisely the reboot-then-outage case this exists for.
_last_self_update = None
_CACHE_PURGE_LOCK = threading.Lock()
_last_cache_purge = 0.0

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

# Single-flight: collapse concurrent resolves of the same video id onto one
# yt-dlp -g. Second callers wait on the per-id lock and reuse the cached URL.
_INFLIGHT_LOCK = threading.Lock()
_INFLIGHT: "dict[str, threading.Lock]" = {}

# What /health reports: /stream bodies being sent now, and yt-dlp processes running now.
_COUNTS_LOCK = threading.Lock()
_COUNTS = {"active_streams": 0, "ytdlp_running": 0}


def _count(name: str, change: int) -> None:
    with _COUNTS_LOCK:
        _COUNTS[name] += change

# One pooled HTTPS session for all upstream googlevideo byte-proxying. Reuses
# the TCP+TLS connection across an iOS client's `Range: bytes=0-1` probe and
# the real range GET that follows, instead of a fresh handshake per request.
_SESSION = requests.Session()
_SESSION.mount("https://", HTTPAdapter(pool_connections=32, pool_maxsize=64))


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
    with _URL_CACHE_LOCK:
        entry = _URL_CACHE.get(video_id)
        if not entry:
            return None
        ts, url = entry
        if time.time() - ts > _URL_CACHE_TTL:
            del _URL_CACHE[video_id]
            return None
        _URL_CACHE.move_to_end(video_id)
        return url

def _url_cache_put(video_id: str, url: str):
    with _URL_CACHE_LOCK:
        _URL_CACHE[video_id] = (time.time(), url)
        _URL_CACHE.move_to_end(video_id)
        while len(_URL_CACHE) > _URL_CACHE_MAX:
            _URL_CACHE.popitem(last=False)

# When each resolved URL may first be fetched, from the run that printed it.
_URL_READY_LOCK = threading.Lock()
_URL_READY: "OrderedDict[str, float]" = OrderedDict()


def _url_ready_put(url: str, at: Optional[float]):
    if not url or at is None:
        return
    with _URL_READY_LOCK:
        _URL_READY[url] = at
        _URL_READY.move_to_end(url)
        while len(_URL_READY) > _URL_CACHE_MAX:
            _URL_READY.popitem(last=False)


def _wait_until_ready(url: str, video_id: str):
    """Sleep until the URL's `available_at`, at most _URL_READY_WAIT_MAX_SEC."""
    with _URL_READY_LOCK:
        at = _URL_READY.get(url)
    if at is None:
        return
    wait = min(at - time.time(), _URL_READY_WAIT_MAX_SEC)
    if wait > 0:
        log.info("stream %s: waiting %.1fs for the URL to be ready", video_id, wait)
        time.sleep(wait)


def _keep_url(video_id: str, format_line: Optional[str], url: str):
    """Cache a resolved URL with its format's facts and the time it may first be fetched."""
    _url_cache_put(video_id, url)
    _url_ready_put(url, stream_info.available_at(format_line))
    _facts_put(video_id, stream_info.stream_facts(format_line, url))


# Until when the player client is left out after it gave no URL where yt-dlp's own client did.
_PLAYER_CLIENT_RESTING_UNTIL = 0.0


def _client_args() -> list[str]:
    """The --extractor-args that pick the player client, or none while it rests."""
    if not _PLAYER_CLIENT or time.time() < _PLAYER_CLIENT_RESTING_UNTIL:
        return []
    return ["--extractor-args", f"youtube:player_client={_PLAYER_CLIENT}"]


def _run_with_client(args: list[str], label: str, bg: bool, timeout: int = 15) -> Optional[str]:
    """A format-selecting run with the player client, then with yt-dlp's own client when that
    gives no URL. A video both clients fail on costs two runs and leaves the client in use."""
    global _PLAYER_CLIENT_RESTING_UNTIL
    client = _client_args()
    out = _run([*client, *args], timeout=timeout, label=label, bg=bg)
    if not client or (out and stream_info.split_run(out)[2]):
        return out
    fallback = _run(args, timeout=timeout, label=f"{label}-default", bg=bg)
    if fallback and stream_info.split_run(fallback)[2]:
        log.warning("player client %s gave no URL where yt-dlp's own did; leaving it out for %ds",
                    _PLAYER_CLIENT, _PLAYER_CLIENT_REST_SEC)
        _PLAYER_CLIENT_RESTING_UNTIL = time.time() + _PLAYER_CLIENT_REST_SEC
    return fallback or out

# What each video's stream is (itag, size, container, bit rate), learned in the same run that
# picked its URL. A video's format and size do not change, so these outlive the URL itself.
_FACTS_LOCK = threading.Lock()
_FACTS: "OrderedDict[str, dict]" = OrderedDict()
_FACTS_MAX = int(os.environ.get("FACTS_CACHE_MAX", "4096"))


def _facts_get(video_id: str) -> dict:
    with _FACTS_LOCK:
        known = _FACTS.get(video_id)
        if known is None:
            return {}
        _FACTS.move_to_end(video_id)
        return dict(known)


def _facts_put(video_id: str, learned: dict):
    """Keep what is known; a later answer fills gaps and a measured size replaces a printed one."""
    if not video_id:
        return
    with _FACTS_LOCK:
        known = dict(_FACTS.get(video_id) or {})
        for key, value in learned.items():
            if value is not None:
                known[key] = value
        _FACTS[video_id] = known
        _FACTS.move_to_end(video_id)
        while len(_FACTS) > _FACTS_MAX:
            _FACTS.popitem(last=False)


def _with_facts(payload: dict) -> dict:
    """A /search payload with its video's stream facts, when the run that found it had them."""
    known = _facts_get(payload.get("video_id") or "")
    for key in ("size", "itag", "ext", "abr"):
        if known.get(key) is not None:
            payload[key] = known[key]
    return payload


def _url_cache_evict(video_id: str):
    with _URL_CACHE_LOCK:
        _URL_CACHE.pop(video_id, None)

def _url_cache_evict_if(video_id: str, expected_url: str):
    """Evict only if the cache still holds the URL that just failed.

    Two threads can 403 on the same stale URL (an iOS client sends a
    `Range: bytes=0-1` probe and then the real GET). An unconditional evict lets
    the second thread discard the good URL the first one just resolved, and fork
    another yt-dlp to rediscover it.
    """
    with _URL_CACHE_LOCK:
        entry = _URL_CACHE.get(video_id)
        if entry and entry[1] == expected_url:
            _URL_CACHE.pop(video_id, None)

# Cap concurrent yt-dlp processes globally. Each one is fork+exec heavy;
# letting them stack starves a small container. GATE_RESERVE_INTERACTIVE of the
# slots are unreachable by background work (prewarm, /download), so a user
# pressing play never queues behind a prewarm burst.
_MAX_CONCURRENT = int(os.environ.get("MAX_CONCURRENT_YTDLP", "5"))
_RESERVE_INTERACTIVE = int(os.environ.get("GATE_RESERVE_INTERACTIVE", "2"))
_GATE = TieredGate(_MAX_CONCURRENT, _RESERVE_INTERACTIVE)
# Max time a queued request will wait for a free slot. Long enough that a
# burst of 10 parallel radio-resolution searches all eventually succeed
# rather than dropping requests on the floor.
_GATE_WAIT_SEC = int(os.environ.get("GATE_WAIT_SEC", "45"))
# Background waits far less than that: it is fire-and-forget, it is already
# stale by the time a 45s queue clears, and a parked thread is one of a finite
# gunicorn pool. Shedding it early is better than holding a thread for nothing.
_BG_GATE_WAIT_SEC = int(os.environ.get("BG_GATE_WAIT_SEC", "10"))
# How long an interactive resolve coalesces behind an in-flight one before
# giving up and resolving in parallel. See _resolve_url.
_INFLIGHT_WAIT_SEC = float(os.environ.get("INFLIGHT_WAIT_SEC", "0.25"))

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
log = logging.getLogger("ytdlp-shim")

app = Flask(__name__)

log.info(
    "gate: total=%d reserve=%d bg_capacity=%d",
    _GATE.total, _GATE.reserve, _GATE.total - _GATE.reserve,
)


def _run(args: list[str], timeout: int = 20, label: str = "ytdlp",
         bg: bool = False, capture: bool = False):
    """Run yt-dlp with a hard timeout. Returns stdout or None on any failure.

    With capture=True the CompletedProcess is returned instead, so a caller can
    read stderr and tell a 403 apart from an unavailable video. Still None when
    the gate is full or the run times out, since there is no process to report.

    Logs gate-wait and wall time so a real box can show whether interactive
    resolves are queuing behind background prewarm (grep `gate_wait_ms`), and
    `bg` so the two can actually be told apart in the log.
    """
    t_acquire = time.monotonic()
    wait = _BG_GATE_WAIT_SEC if bg else _GATE_WAIT_SEC
    if not _GATE.acquire(bg, wait):
        log.warning("yt-dlp gate full after %ds (bg=%d), dropping: %s",
                    wait, int(bg), " ".join(args))
        return None
    gate_ms = (time.monotonic() - t_acquire) * 1000.0
    try:
        # NOTE: --no-cache-dir intentionally removed. The cache stores the
        # player base.js + solved nsig signatures; disabling it forced a
        # re-download and re-solve on every fork. A persistent cache volume is
        # mounted in compose.
        full = [YTDLP, "--no-warnings", "--no-playlist", *args]
        t_run = time.monotonic()
        try:
            _count("ytdlp_running", 1)
            try:
                cp = subprocess.run(
                    full,
                    capture_output=True,
                    text=True,
                    timeout=timeout,
                    check=False,
                )
            finally:
                _count("ytdlp_running", -1)
        except subprocess.TimeoutExpired:
            log.warning("yt-dlp timed out (%s, gate_wait_ms=%.0f): %s", label, gate_ms, " ".join(args))
            return None
        except OSError as e:
            # Binary missing or not executable. Every caller already handles
            # None, so degrade to "this run failed" rather than letting an
            # exception escape: raised from the startup version probe it would
            # abort module import and take the whole shim down, turning a bad
            # YTDLP_PATH into total loss of playback instead of one log line.
            log.warning("yt-dlp could not be executed at %s (%s): %s", YTDLP, label, e)
            return None
        wall_ms = (time.monotonic() - t_run) * 1000.0
        log.info("ytdlp label=%s bg=%d gate_wait_ms=%.0f wall_ms=%.0f rc=%d",
                 label, int(bg), gate_ms, wall_ms, cp.returncode)
        if cp.returncode != 0:
            log.warning("yt-dlp exit %d (%s): %s", cp.returncode, label, cp.stderr.strip()[:300])
            return cp if capture else None
        return cp if capture else cp.stdout
    finally:
        _GATE.release(bg)


@app.get("/health")
def health():
    """Up, plus how busy: /stream bodies being sent now, yt-dlp processes running now, and how
    many yt-dlp may run at once. Octo's family load view reads these."""
    with _COUNTS_LOCK:
        active, running = _COUNTS["active_streams"], _COUNTS["ytdlp_running"]
    return jsonify(ok=True, active_streams=active, ytdlp_running=running, ytdlp_limit=_GATE.total)


def _is_bg() -> bool:
    """True for fire-and-forget prewarm, which yields gate slots to real plays.

    Absence means interactive, so a caller that forgets the flag fails safe:
    slower background, never a deprioritised play. NEVER fold this into a cache
    key -- the result is identical either way, and keying on it would halve the
    hit rate for no benefit.
    """
    return request.args.get("bg") == "1"


@app.get("/search")
def search():
    q = request.args.get("q", "").strip()
    if not q:
        abort(400, "missing q")

    hint_str = request.args.get("duration", "").strip()
    duration_hint = int(hint_str) if hint_str.isdigit() else None
    bg = _is_bg()

    cache_key = f"{q.lower()}|{duration_hint or ''}"
    cached = _cache_get(cache_key)
    if cached is not None:
        return jsonify(**cached)

    payload = _search_with_hint(q, duration_hint, bg) if duration_hint is not None else _search_single(q, bg)
    if payload is None:
        return jsonify(error="search_failed"), 502
    if not payload.get("video_id"):
        return jsonify(error="no_hit"), 404

    _cache_put(cache_key, payload)
    return jsonify(**payload)


def _payload_from(data: dict) -> dict:
    return {
        "video_id": data.get("id"),
        "title": data.get("title"),
        "duration": data.get("duration"),
        "channel": data.get("channel"),
    }


def _search_single(q: str, bg: bool = False) -> Optional[dict]:
    """No duration hint: fast path. One extraction yields metadata AND the
    stream URL, which we warm into the URL cache so /stream is a cache hit."""
    out = _run_with_client(
        [
            f"ytsearch1:{q}",
            "-f", _AUDIO_FORMAT,
            "--ignore-no-formats-error",
            "--print", "%(.{id,title,duration,channel})j",
            "--print", stream_info.FORMAT_PRINT,
            "--print", "%(urls)s",
        ],
        label="search+resolve",
        bg=bg,
    )
    if out is None:
        return None
    lines, format_line, stream_url = stream_info.split_run(out)
    if not lines:
        return {}
    try:
        data = json.loads(lines[0])
    except json.JSONDecodeError as e:
        log.warning("search: bad json from yt-dlp: %s", e)
        return None
    payload = _payload_from(data)
    if payload.get("video_id") and stream_url:
        # Warm the URL cache from the SAME extraction. This used to be a second
        # `yt-dlp -g` on a detached daemon thread; folding it in removes an
        # entire yt-dlp invocation per track and makes the warm /stream path a
        # deterministic cache hit instead of a gate-race.
        _keep_url(payload["video_id"], format_line, stream_url)
    return _with_facts(payload)


_LIVE = re.compile(r"\b(live|unplugged|in concert|concert|bootleg)\b", re.IGNORECASE)


def _words(text: str) -> set:
    """The words of a title or a query, in lower case, accents and punctuation gone."""
    plain = unicodedata.normalize("NFKD", text or "")
    plain = "".join(c for c in plain if not unicodedata.combining(c))
    return set(re.findall(r"\w+", plain.casefold()))


def _rank_by_length(candidates: list, q: str, duration_hint: int) -> list:
    """The video whose title and channel name every word of the query first, then the closest
    length, and a live take never beats a studio one unless the query asks for live itself.

    Length alone let another artist's song win whenever it ran closer: "KILLY - Killamonjaro"
    at the catalog's 179 s took "SUMMER CEM x BILLA JOE x DARDAN x EZHEL - KILLY MANJARO
    EUROMIX" (171 s) over KILLY's own video (161 s). Words the videos all lack, such as a title
    in another script, say nothing, and the length decides as before. A live video of a song is
    usually within seconds of the studio length, so length alone picked it as often as not. All
    of them live: the closest still plays."""
    wants_live = bool(_LIVE.search(q))
    wanted = _words(q)

    def key(r: dict):
        named = _words(r.get("title") or "") | _words(r.get("channel") or "")
        missing = len(wanted - named)
        live = 0 if wants_live or not _LIVE.search(r.get("title") or "") else 1
        length = abs((r.get("duration") or 0) - duration_hint) if r.get("duration") else float("inf")
        return (missing, live, length)

    return sorted(candidates, key=key)


def _search_with_hint(q: str, duration_hint: int, bg: bool = False) -> Optional[dict]:
    """Duration hint present: pick the closest-length of 5 flat candidates
    (cheap, no per-video extraction), then resolve the winner's URL once.

    Note this path costs TWO gated yt-dlp runs per track (search5 then resolve),
    where the no-hint path gets both from one extraction.
    """
    out = _run(
        [
            f"ytsearch5:{q}",
            "--flat-playlist",
            "--print", "%(.{id,title,duration,channel})j",
        ],
        timeout=20,
        label="search5",
        bg=bg,
    )
    if out is None:
        return None
    candidates = []
    for ln in out.strip().split("\n"):
        ln = ln.strip()
        if not ln:
            continue
        try:
            candidates.append(json.loads(ln))
        except json.JSONDecodeError:
            pass
    if not candidates:
        return {}
    # The video that names the query's words wins, then the closest duration, live
    # takes last; candidates without a duration sort last so a hint never drags us
    # onto an entry we can't length-match.
    candidates = _rank_by_length(candidates, q, duration_hint)
    payload = _payload_from(candidates[0])
    if payload.get("video_id"):
        # Warm the URL cache for the coming /stream (single-flight + cached).
        _resolve_url(payload["video_id"], bg)
    return _with_facts(payload)


_META_CACHE_LOCK = threading.Lock()
_META_CACHE: "OrderedDict[str, dict]" = OrderedDict()

# Persistent duration cache. YouTube is the single source of truth for a track's
# length, and a resolved "artist - title" -> {video_id, duration} mapping is
# stable forever, so it is worth surviving process restarts. This is what turns
# the speed-vs-accuracy tradeoff into "both": once a track is resolved (by a real
# search or a background warm) its accurate duration is a disk lookup for every
# future search and play, in BOTH Subsonic and Navidrome modes. The in-memory
# _META_CACHE stays as a hot front; this is the durable backing store.
# Mount META_DB_PATH's directory as a volume to also survive container recreation.
_META_DB_PATH = os.environ.get("META_DB_PATH", "/var/lib/ytshim/meta.db")
_META_DB_LOCK = threading.Lock()


def _norm_q(q: str) -> str:
    """Normalize a query so trivially-different spellings share a cache row:
    lowercase and collapse runs of whitespace. The duration hint is deliberately
    NOT part of the key — a resolved track's YouTube length is what we trust, and
    a slightly different Deezer hint next time must still hit the same row."""
    return " ".join(q.lower().split())


def _meta_db_init() -> None:
    try:
        os.makedirs(os.path.dirname(_META_DB_PATH) or ".", exist_ok=True)
        with sqlite3.connect(_META_DB_PATH, timeout=5) as c:
            c.execute("PRAGMA journal_mode=WAL")
            c.execute(
                "CREATE TABLE IF NOT EXISTS meta ("
                "key TEXT PRIMARY KEY, video_id TEXT, title TEXT, "
                "duration INTEGER, updated REAL)"
            )
    except Exception as e:  # a broken db must never take the shim down
        log.warning("meta db init failed (%s); persistence disabled", e)


def _meta_db_get(key: str) -> Optional[dict]:
    try:
        with sqlite3.connect(_META_DB_PATH, timeout=5) as c:
            c.execute("PRAGMA busy_timeout=5000")
            row = c.execute(
                "SELECT video_id, title, duration FROM meta WHERE key=?", (key,)
            ).fetchone()
        if row and row[0]:
            return {"video_id": row[0], "title": row[1], "duration": row[2]}
    except Exception as e:
        log.warning("meta db get failed: %s", e)
    return None


def _meta_db_put(key: str, payload: dict) -> None:
    if not payload.get("video_id"):
        return
    try:
        with _META_DB_LOCK, sqlite3.connect(_META_DB_PATH, timeout=5) as c:
            c.execute("PRAGMA busy_timeout=5000")
            c.execute(
                "INSERT OR REPLACE INTO meta(key, video_id, title, duration, updated) "
                "VALUES(?,?,?,?,?)",
                (key, payload.get("video_id"), payload.get("title"),
                 payload.get("duration"), time.time()),
            )
    except Exception as e:
        log.warning("meta db put failed: %s", e)


_meta_db_init()


@app.get("/meta")
def meta():
    """Fast metadata-only lookup (flat search, NO url solve) for an accurate
    duration on search rows. With a duration hint it picks the closest-length of
    5 candidates (avoids long-form/compilation uploads); otherwise the top result.
    Cached, so repeat searches are a dict lookup."""
    q = request.args.get("q", "").strip()
    if not q:
        abort(400, "missing q")
    hint_str = request.args.get("duration", "").strip()
    duration_hint = int(hint_str) if hint_str.isdigit() else None
    bg = _is_bg()

    key = _norm_q(q)
    # Hot front: exact repeat within this process.
    with _META_CACHE_LOCK:
        c = _META_CACHE.get(key)
        if c is not None:
            _META_CACHE.move_to_end(key)
            return jsonify(**c)
    # Durable backing: resolved in a past search/warm, possibly a past process.
    persisted = _meta_db_get(key)
    if persisted is not None:
        with _META_CACHE_LOCK:
            _META_CACHE[key] = persisted
            _META_CACHE.move_to_end(key)
        return jsonify(**persisted)

    if duration_hint is not None:
        out = _run([f"ytsearch5:{q}", "--flat-playlist", "--print", "%(.{id,title,duration})j"],
                   timeout=20, label="meta5", bg=bg)
        cands = []
        for ln in (out or "").strip().split("\n"):
            ln = ln.strip()
            if ln:
                try:
                    cands.append(json.loads(ln))
                except json.JSONDecodeError:
                    pass
        if not cands:
            return jsonify(error="no_hit"), 404
        data = _rank_by_length(cands, q, duration_hint)[0]
    else:
        out = _run([f"ytsearch1:{q}", "--flat-playlist", "--print", "%(.{id,title,duration})j"],
                   timeout=15, label="meta", bg=bg)
        if out is None:
            return jsonify(error="search_failed"), 502
        line = next((ln.strip() for ln in out.strip().split("\n") if ln.strip()), "")
        if not line:
            return jsonify(error="no_hit"), 404
        try:
            data = json.loads(line)
        except json.JSONDecodeError:
            return jsonify(error="bad_yt_response"), 502

    payload = {"video_id": data.get("id"), "title": data.get("title"), "duration": data.get("duration")}
    if payload["video_id"]:
        with _META_CACHE_LOCK:
            _META_CACHE[key] = payload
            _META_CACHE.move_to_end(key)
            while len(_META_CACHE) > _SEARCH_CACHE_MAX:
                _META_CACHE.popitem(last=False)
        _meta_db_put(key, payload)  # persist so it survives restarts, for both modes
    return jsonify(**payload)


def _resolve_url(video_id: str, bg: bool = False) -> Optional[str]:
    cached = _url_cache_get(video_id)
    if cached:
        return cached

    # Single-flight per video id: if a prefetch/other /stream is already
    # resolving this id, wait for it and reuse its result instead of forking a
    # second identical `yt-dlp -g`.
    with _INFLIGHT_LOCK:
        lock = _INFLIGHT.setdefault(video_id, threading.Lock())

    # An interactive caller must not inherit a background leader's queue
    # position. The leader may be parked in the gate for seconds, and that wait
    # is invisible to the reserve because a follower never reaches a gate at
    # all -- it is asleep on this lock. If the leader does not hand over
    # promptly, resolve in parallel: one extra yt-dlp fork is far cheaper than a
    # user-visible stall.
    coalesced = lock.acquire(timeout=_GATE_WAIT_SEC if bg else _INFLIGHT_WAIT_SEC)
    try:
        # Re-check on BOTH branches. The leader may have finished while we
        # waited, and without this every interactive caller for a contended id
        # forks its own resolve -- burning the very slots the reserve protects.
        cached = _url_cache_get(video_id)
        if cached:
            return cached
        # The same URL `-g` printed, plus the picked format's facts from the same run.
        out = _run_with_client(
            [
                "-f", _AUDIO_FORMAT,
                "--print", stream_info.FORMAT_PRINT,
                "--print", "%(urls)s",
                f"https://www.youtube.com/watch?v={video_id}",
            ],
            label="resolve",
            bg=bg,
        )
        if not out:
            return None
        _, format_line, url = stream_info.split_run(out)
        if url:
            _keep_url(video_id, format_line, url)
        return url or None
    finally:
        # Only the thread that actually holds the lock may pop and release it.
        if coalesced:
            with _INFLIGHT_LOCK:
                _INFLIGHT.pop(video_id, None)
            lock.release()


def _purge_ytdlp_cache(video_id: str) -> None:
    """Drop yt-dlp's player/signature cache after a 403 survives a re-resolve.

    The cache is persistent by design (see _run), which means a rotated YouTube
    player leaves every resolve producing URLs googlevideo refuses, forever.
    Purging is the standard recovery. It is rate-limited because doing it on
    every 403 would throw away base.js during a burst and make things worse, and
    it deliberately does not retry the current play -- the next one benefits.
    """
    global _last_cache_purge
    with _CACHE_PURGE_LOCK:
        now = time.monotonic()
        if now - _last_cache_purge < _CACHE_PURGE_MIN_INTERVAL_SEC:
            return
        _last_cache_purge = now
    log.warning(
        "stream %s: 403 survived a re-resolve, purging yt-dlp cache "
        "(rotated player is the usual cause)", video_id,
    )
    _purge_cache_contents()


def _purge_cache_contents() -> None:
    """Empty yt-dlp's cache without removing the directory itself.

    yt-dlp --rm-cache-dir rmtree's the cache root, and compose mounts a named
    volume at exactly that path. Removing a mount point from inside the
    container is EBUSY, so the flag deletes the contents and then dies on the
    final rmdir with exit 1. The purge looked like it failed and the log filled
    with a traceback, when the part that mattered had already happened. Clearing
    the contents directly does the same job and reports honestly.
    """
    root = os.environ.get("XDG_CACHE_HOME") or os.path.expanduser("~/.cache")
    cache_dir = os.path.join(root, "yt-dlp")
    if not os.path.isdir(cache_dir):
        return
    removed = 0
    for name in os.listdir(cache_dir):
        target = os.path.join(cache_dir, name)
        try:
            if os.path.isdir(target) and not os.path.islink(target):
                shutil.rmtree(target)
            else:
                os.remove(target)
            removed += 1
        except OSError as e:
            log.warning("cache purge could not remove %s: %s", target, e)
    log.info("yt-dlp cache purged (%d entries)", removed)


def _seed_writable_ytdlp() -> None:
    """Copy the image's yt-dlp into the writable location when it is the newer one.

    Runs only when YTDLP_PATH points somewhere other than the image copy. Seeds
    on first boot, and re-seeds after an image rebuild so a freshly built version
    wins over an older self-updated one on the volume. Any failure is logged and
    ignored: falling back to whatever is already there beats refusing to start.
    """
    if YTDLP == YTDLP_DIST:
        return
    try:
        if not os.path.isfile(YTDLP_DIST):
            log.warning("no dist yt-dlp at %s to seed from", YTDLP_DIST)
            return
        os.makedirs(os.path.dirname(YTDLP) or ".", exist_ok=True)
        if os.path.isfile(YTDLP) and os.path.getmtime(YTDLP) >= os.path.getmtime(YTDLP_DIST):
            return
        shutil.copy2(YTDLP_DIST, YTDLP)
        os.chmod(YTDLP, 0o755)
        log.info("seeded %s from %s", YTDLP, YTDLP_DIST)
    except OSError as e:
        log.warning("could not seed %s from %s: %s", YTDLP, YTDLP_DIST, e)


def _ytdlp_version() -> str:
    out = _run(["--version"], timeout=30, label="version", bg=False)
    return (out or "").strip()


def _maybe_self_update(reason: str) -> bool:
    """Update yt-dlp in place. True only when the version actually changed.

    This is the last rung of the 403 ladder. YouTube rotates its player on its
    own schedule and the Dockerfile's YTDLP_VERSION=latest is resolved at build
    time, so an image that is not rebuilt ships one frozen version forever. That
    is exactly how the 2026-08-21 outage happened: previews and downloads were
    refused for days on 2026.07.04 while the fix sat published in 2026.08.19.
    """
    if not _SELF_UPDATE_ON_403:
        return False
    global _last_self_update
    with _SELF_UPDATE_LOCK:
        now = time.monotonic()
        if (_last_self_update is not None
                and now - _last_self_update < _SELF_UPDATE_MIN_INTERVAL_SEC):
            return False
        _last_self_update = now

    before = _ytdlp_version()
    log.warning("%s: attempting yt-dlp self-update (current %s)", reason, before or "unknown")
    _run(["-U"], timeout=180, label="self-update", bg=True)
    after = _ytdlp_version()
    if after and after != before:
        # The old player cache belongs to the old binary; keep them together.
        log.warning("yt-dlp self-updated %s -> %s, purging cache and retrying", before or "unknown", after)
        _purge_cache_contents()
        return True
    log.warning(
        "yt-dlp self-update left version at %s, so the refusal is not a stale binary",
        after or before or "unknown",
    )
    return False


# ReplayGain for played videos. Measuring reads the whole audio once more, so it runs only on a
# play, LOUDNESS_PER_MINUTE at most a minute, and one at a time under its own limit (never a
# yt-dlp gate slot, so it never makes a play or a resolve wait).
_LOUDNESS = LoudnessBook(per_minute=int(os.environ.get("LOUDNESS_PER_MINUTE", "2")))
_LOUDNESS_TIMEOUT_SEC = int(os.environ.get("LOUDNESS_TIMEOUT_SEC", "120"))


def _measure_loudness(url: str) -> Optional[dict]:
    try:
        cp = subprocess.run(
            loudness_info.ffmpeg_args(url, _UPSTREAM_UA),
            capture_output=True,
            text=True,
            timeout=_LOUDNESS_TIMEOUT_SEC,
            check=False,
        )
    except (subprocess.TimeoutExpired, OSError) as e:
        log.warning("loudness measurement failed: %s", e)
        return None
    measured = loudness_info.parse_ebur128(cp.stderr)
    if measured is None:
        return None
    return loudness_info.replay_gain(*measured)


def _maybe_measure_loudness(video_id: str, url: str) -> bool:
    """Starts measuring a played video's loudness in the background. False when it is known,
    a measurement is running, or the minute's cap is reached."""
    if _LOUDNESS.get(video_id) is not None:
        return False
    if not _LOUDNESS.begin(video_id):
        return False

    def run():
        measured = None
        try:
            measured = _measure_loudness(url)
            log.debug("loudness %s: %s", video_id, measured)
        finally:
            _LOUDNESS.finish(video_id, measured)

    threading.Thread(target=run, name=f"loudness-{video_id}", daemon=True).start()
    return True


def _open_upstream(url: str, headers: dict, video_id: str):
    _wait_until_ready(url, video_id)
    try:
        return _SESSION.get(url, stream=True, timeout=(8, 30), headers=headers)
    except Exception as e:
        log.warning("stream upstream failed for %s: %s", video_id, e)
        return None


# googlevideo sends a range that spans at most 10 MiB at full speed and a wider one at about
# 33 KB/s, so a wider range is asked for in pieces of this size.
_UPSTREAM_PIECE = int(os.environ.get("UPSTREAM_PIECE_BYTES", str(8 * 1024 * 1024)))
# A piece that breaks, or cannot be opened, is asked for again from where the answer stopped,
# this many times in a row before the answer is cut off.
_PIECE_RETRIES = 2
# The wait before asking again for a piece that could not be opened.
_PIECE_RETRY_WAIT = float(os.environ.get("UPSTREAM_PIECE_RETRY_WAIT", "0.5"))


class _OtherFile(Exception):
    """A piece answered from another file than the one the answer began with."""


def _is_pieced(wanted) -> bool:
    """Whether a range is wider than one piece: an open one, or one spanning more than a piece.
    A suffix range (no first byte) and an unreadable one go upstream as they are."""
    if wanted is None or wanted[0] is None:
        return False
    start, end = wanted
    return end is None or end - start + 1 > _UPSTREAM_PIECE


def _piece_end(start: int, end: Optional[int]) -> int:
    """The last byte of the piece that starts at `start`, within the range's own last byte."""
    last = start + _UPSTREAM_PIECE - 1
    return last if end is None else min(last, end)


def _pieced_answer(video_id, url, upstream_headers, first, wanted, total, synthesized, t_enter):
    """The answer to a range wider than one piece: the first piece already open, then each next
    piece asked for as the one before it ends, all sent as one body under the range's own
    status, length and Content-Range. A caller that sent no Range gets a plain 200."""
    start = wanted[0]
    last = total - 1 if wanted[1] is None else min(wanted[1], total - 1)
    status = 200 if synthesized else 206
    _facts_put(video_id, {"size": total})
    current = {"url": url}

    def open_piece(pos):
        headers = dict(upstream_headers, Range=f"bytes={pos}-{_piece_end(pos, last)}")
        piece = _open_upstream(current["url"], headers, video_id)
        if piece is not None and piece.status_code in (403, 410):
            # The signed URL ran out partway through a long song: one fresh URL, then on.
            piece.close()
            _url_cache_evict_if(video_id, current["url"])
            current["url"] = _resolve_url(video_id)
            piece = _open_upstream(current["url"], headers, video_id) if current["url"] else None
        if piece is None or piece.status_code != 206:
            if piece is not None:
                log.warning("stream %s: the piece from byte %d answered %d", video_id, pos, piece.status_code)
                piece.close()
            return None
        # A fresh URL can name another format of the video: a piece must go on with this file.
        answered = piece.headers.get("Content-Range")
        if (
            stream_info.range_start(answered) != pos
            or stream_info.total_from_content_range(answered) != total
        ):
            piece.close()
            raise _OtherFile(f"stream {video_id}: the piece from byte {pos} is {answered!r}, not of {total} bytes")
        return piece

    @stream_with_context
    def generator():
        pos = start
        piece = first
        logged = False
        failures = 0
        _count("active_streams", 1)
        try:
            while pos <= last and failures <= _PIECE_RETRIES:
                if piece is None:
                    piece = open_piece(pos)
                    if piece is None:
                        failures += 1
                        time.sleep(_PIECE_RETRY_WAIT)
                        continue
                before = pos
                try:
                    for chunk in piece.iter_content(chunk_size=64 * 1024):
                        if not chunk:
                            continue
                        if not logged:
                            log.info("stream %s ttfb_ms=%.0f status=%d pieced",
                                     video_id, (time.monotonic() - t_enter) * 1000.0, status)
                            logged = True
                        chunk = chunk[: last - pos + 1]
                        pos += len(chunk)
                        yield chunk
                        if pos > last:
                            break
                except requests.RequestException as e:
                    log.warning("stream %s: the piece broke at byte %d: %s", video_id, pos, e)
                finally:
                    piece.close()
                    piece = None
                failures = 0 if pos > before else failures + 1
            if pos <= last:
                # The headers promised every byte: the connection is ended, so the player knows
                # at once instead of waiting on bytes that will not come.
                raise ConnectionAbortedError(f"stream {video_id}: ended at byte {pos} of {last + 1}")
        finally:
            if piece is not None:
                piece.close()
            _count("active_streams", -1)

    headers = {
        "Content-Type": first.headers.get("Content-Type", "audio/mp4"),
        "Accept-Ranges": "bytes",
        "Cache-Control": "no-store",
        "Content-Length": str(last - start + 1),
    }
    if not synthesized:
        headers["Content-Range"] = f"bytes {start}-{last}/{total}"
    if request.method == "HEAD":
        first.close()
        return Response(iter(()), headers=headers, status=status)
    return Response(generator(), headers=headers, status=status)


# --- YouTube Music (Octo's radio) -------------------------------------------------
# Search and radio through ytmusicapi, no sign-in. These are plain HTTP calls to YouTube
# Music, not yt-dlp processes, so they have their own small gate instead of TieredGate.
_YTM_GATE = threading.BoundedSemaphore(int(os.environ.get("YTM_MAX_CONCURRENT", "3")))
# Octo gives a radio source 8 s; a request still queued past this has no one waiting for it, and
# would only hold a gunicorn thread that playback needs.
_YTM_GATE_WAIT = float(os.environ.get("YTM_GATE_WAIT_SEC", "5"))
# One cache per kind, so a burst of radios never pushes the searches out.
_YTM_CACHE: "dict[str, OrderedDict]" = {"search": OrderedDict(), "radio": OrderedDict(), "artist": OrderedDict()}
_YTM_LIMIT = {"search": 2000, "radio": 500, "artist": 500}
_YTM_LOCK = threading.Lock()
_YTM_TTL = {"search": 24 * 3600, "radio": 3600, "artist": 3600}


def _arg_int(name, default, low, high):
    try:
        return min(max(int(request.args.get(name, default)), low), high)
    except (TypeError, ValueError):
        return default


def _ytm_cached(kind, key, load):
    """A cached answer, else load() under the gate; None when the gate stays full."""
    cache = _YTM_CACHE[kind]
    with _YTM_LOCK:
        hit = cache.get(key)
        if hit and time.time() - hit[0] < _YTM_TTL[kind]:
            cache.move_to_end(key)
            return hit[1]
    if not _YTM_GATE.acquire(timeout=_YTM_GATE_WAIT):
        return None
    try:
        value = load()
    finally:
        _YTM_GATE.release()
    with _YTM_LOCK:
        cache[key] = (time.time(), value)
        while len(cache) > _YTM_LIMIT[kind]:
            cache.popitem(last=False)
    return value


@app.get("/ytm/search")
def ytm_search():
    q = request.args.get("q", "").strip()
    if not q:
        abort(400, "missing q")
    filter_name = request.args.get("filter", "songs")
    if filter_name not in ("songs", "videos"):
        abort(400, "filter must be songs or videos")
    limit = _arg_int("limit", 10, 1, 20)
    try:
        rows = _ytm_cached("search", (q.lower(), filter_name, limit), lambda: ytm.search(q, filter_name, limit))
    except Exception as e:  # ytmusicapi raises plain exceptions when YouTube Music changes its pages
        log.warning("ytm search failed for %r: %s", q, e)
        return jsonify(error="ytm_failed"), 502
    if rows is None:
        return jsonify(error="busy"), 503
    return jsonify(tracks=rows)


@app.get("/ytm/radio")
def ytm_radio():
    video_id = request.args.get("videoId", "").strip()
    if not video_id:
        abort(400, "missing videoId")
    limit = _arg_int("limit", 50, 1, 100)
    try:
        rows = _ytm_cached("radio", (video_id, limit), lambda: ytm.radio(video_id, limit))
    except Exception as e:
        log.warning("ytm radio failed for %s: %s", video_id, e)
        return jsonify(error="ytm_failed"), 502
    if rows is None:
        return jsonify(error="busy"), 503
    return jsonify(tracks=rows)


@app.get("/ytm/artist-radio")
def ytm_artist_radio():
    q = request.args.get("q", "").strip()
    if not q:
        abort(400, "missing q")
    limit = _arg_int("limit", 50, 1, 100)
    try:
        found = _ytm_cached("artist", (q.lower(), limit), lambda: ytm.artist_radio(q, limit))
    except Exception as e:
        log.warning("ytm artist radio failed for %r: %s", q, e)
        return jsonify(error="ytm_failed"), 502
    if found is None:
        return jsonify(error="busy"), 503
    artist, rows = found
    return jsonify(artist=artist, tracks=rows)


@app.get("/facts")
def stream_facts():
    """What /stream would send for a video: {video_id, itag, size, ext, abr}, plus
    {track_gain, track_peak} once a play measured its loudness.

    Resolves the URL the way /stream does (same format, same cache, same gate) when this video
    has not been resolved yet, so a later play of it is a cache hit.
    """
    video_id = request.args.get("id", "").strip()
    if not video_id:
        abort(400, "missing id")
    if not stream_info.is_video_id(video_id):
        abort(400, "not a video id")
    known = _facts_get(video_id)
    if known.get("size") is None:
        if not _resolve_url(video_id, _is_bg()):
            return jsonify(error="no_audio_url"), 502
        known = _facts_get(video_id)
    return jsonify(
        video_id=video_id,
        itag=known.get("itag"),
        size=known.get("size"),
        ext=known.get("ext"),
        abr=known.get("abr"),
        **(_LOUDNESS.get(video_id) or {}),
    )


@app.get("/stream")
def stream():
    video_id = request.args.get("id", "").strip()
    if not video_id:
        abort(400, "missing id")
    t_enter = time.monotonic()

    url = _resolve_url(video_id)
    if not url:
        return jsonify(error="no_audio_url"), 502

    # Forward the caller's Range header to googlevideo, which supports byte
    # ranges natively. iOS Subsonic clients (Arpeggi, Narjo) probe with
    # `Range: bytes=0-1` first and refuse to play if the server can't satisfy
    # range requests on an audio/mp4 container — without this passthrough they
    # silently drop the song from the queue. Browsers and Feishin don't care
    # about Range for short clips, hence why those clients worked anyway.
    #
    # A caller that sends NO Range at all still needs one synthesized upstream:
    # googlevideo serves a plain unranged GET at ~30KB/s (a whole song's worth
    # of trickle) but the exact same bytes at multiple MB/s the instant the
    # request carries any Range header, even a fully open-ended "bytes=0-".
    # Measured 2026-09-08: unranged GET of a 3.6MB track topped out at 32KB/s
    # and never finished in 60s; "Range: bytes=0-" pulled the whole file in
    # 1.1s. Since the caller never asked for a partial response here, we
    # translate upstream's 206 back to a plain 200 below so this stays
    # invisible to clients that did not request a range themselves.
    #
    # The full speed holds only for a range that spans at most 10 MiB. A wider
    # one (the rest of any song over about ten minutes) trickles at about 33 KB/s
    # after a 2 s wait (measured 2026-10-21), so it is asked for in pieces and the
    # pieces go back as one answer: see `_pieced_answer`.
    upstream_headers = {}
    incoming_range = request.headers.get("Range")
    synthesized_range = not incoming_range
    wanted = (0, None) if synthesized_range else stream_info.parse_range(incoming_range)
    pieced = _is_pieced(wanted)
    if pieced:
        upstream_headers["Range"] = f"bytes={wanted[0]}-{_piece_end(wanted[0], wanted[1])}"
    else:
        upstream_headers["Range"] = incoming_range if incoming_range else "bytes=0-"
    if _UPSTREAM_UA:
        upstream_headers["User-Agent"] = _UPSTREAM_UA

    upstream = _open_upstream(url, upstream_headers, video_id)
    # A signed googlevideo URL can expire between resolve and play (long or
    # paused song, or a stale cache entry). Expiry shows up as 403/410. Evict
    # the bad entry, re-resolve once, and retry before surfacing a failure, so
    # one stale URL does not fail every play of this id for the cache TTL.
    if upstream is not None and upstream.status_code in (403, 410):
        # Google states the reason in the body and headers. Closing without
        # reading them threw away the only direct evidence of why a play failed,
        # which cost a long debugging session on 2026-08-14. Bounded read: a
        # refusal body is small, and we are about to discard the response.
        try:
            detail = upstream.raw.read(512, decode_content=True) or b""
        except Exception:
            detail = b""
        log.warning(
            "stream %s: upstream %d headers=%s body=%r",
            video_id, upstream.status_code, dict(upstream.headers), detail[:200],
        )
        upstream.close()
        _url_cache_evict_if(video_id, url)
        url = _resolve_url(video_id)
        upstream = _open_upstream(url, upstream_headers, video_id) if url else None
        # A freshly resolved URL that is refused again is not an expiry; the
        # most common remaining cause is a stale cached player.
        if upstream is not None and upstream.status_code in (403, 410):
            _purge_ytdlp_cache(video_id)
            # Last rung: a refusal that outlives a fresh URL and a purged cache
            # points at the binary, not at this play. Only retry when the update
            # actually moved the version, so a healthy shim never pays for it.
            if _maybe_self_update(f"stream {video_id}: 403 survived a cache purge"):
                upstream.close()
                _url_cache_evict_if(video_id, url)
                url = _resolve_url(video_id)
                upstream = _open_upstream(url, upstream_headers, video_id) if url else None

    # A range past the end is the caller's mistake, not an upstream failure: pass the 416 and
    # its `bytes */<size>` back, which is how a player probing the end learns the length.
    if upstream is not None and upstream.status_code == 416:
        total = stream_info.total_from_content_range(upstream.headers.get("Content-Range"))
        upstream.close()
        if total is not None:
            _facts_put(video_id, {"size": total})
        headers = {"Accept-Ranges": "bytes", "Cache-Control": "no-store", "Content-Length": "0"}
        if total is not None:
            headers["Content-Range"] = f"bytes */{total}"
        return Response(b"", status=416, headers=headers)

    if upstream is None or upstream.status_code not in (200, 206):
        code = upstream.status_code if upstream is not None else "n/a"
        if upstream is not None:
            upstream.close()
        log.warning("stream upstream %s for %s", code, video_id)
        return jsonify(error="upstream_failed"), 502

    # A play from the first byte (not a seek) is when a video's loudness is worth measuring.
    if not incoming_range or incoming_range.replace(" ", "").lower().startswith("bytes=0-"):
        _maybe_measure_loudness(video_id, url)

    content_range = upstream.headers.get("Content-Range")
    total = stream_info.total_from_content_range(content_range)
    if (
        pieced
        and upstream.status_code == 206
        and total is not None
        and stream_info.range_end(content_range) is not None
    ):
        return _pieced_answer(
            video_id, url, upstream_headers, upstream, wanted, total, synthesized_range, t_enter
        )
    if pieced:
        # The piece came back with no Content-Range to go on from: the range goes up as it came.
        upstream.close()
        upstream_headers["Range"] = incoming_range if incoming_range else "bytes=0-"
        upstream = _open_upstream(url, upstream_headers, video_id)
        if upstream is None or upstream.status_code not in (200, 206):
            code = upstream.status_code if upstream is not None else "n/a"
            if upstream is not None:
                upstream.close()
            log.warning("stream upstream %s for %s", code, video_id)
            return jsonify(error="upstream_failed"), 502

    @stream_with_context
    def generator():
        first = True
        _count("active_streams", 1)
        try:
            for chunk in upstream.iter_content(chunk_size=64 * 1024):
                if chunk:
                    if first:
                        log.info("stream %s ttfb_ms=%.0f status=%d",
                                 video_id, (time.monotonic() - t_enter) * 1000.0,
                                 upstream.status_code)
                        first = False
                    yield chunk
        finally:
            _count("active_streams", -1)
            try:
                upstream.close()
            except Exception:
                pass

    # Reflect upstream's status (200 for full body, 206 for partial). Forward
    # the metadata that AVPlayer / Subsonic clients need to seek correctly.
    #
    # A synthesized Range (see above) makes upstream answer 206 to a request
    # the caller never made as partial. Report 200 to the caller instead and
    # drop Content-Range: upstream's Content-Length already equals the full
    # size here since the synthesized range always starts at byte 0.
    # The bytes themselves are the last word on the size: the total of a partial answer, or
    # the whole body of one that starts at the first byte.
    played_size = stream_info.total_from_content_range(upstream.headers.get("Content-Range"))
    if played_size is None and upstream.status_code == 200:
        played_size = stream_info.total_from_content_range(f"/{upstream.headers.get('Content-Length', '')}")
    if played_size is not None:
        _facts_put(video_id, {"size": played_size})

    response_status = upstream.status_code
    headers = {
        "Content-Type": upstream.headers.get("Content-Type", "audio/mp4"),
        "Accept-Ranges": "bytes",
        "Cache-Control": "no-store",
    }
    skip_headers = {"Content-Range"} if synthesized_range and upstream.status_code == 206 else set()
    if synthesized_range and upstream.status_code == 206:
        response_status = 200
    for h in ("Content-Length", "Content-Range"):
        if h in skip_headers:
            continue
        v = upstream.headers.get(h)
        if v is not None:
            headers[h] = v
    if request.method == "HEAD":
        upstream.close()
        return Response(iter(()), headers=headers, status=response_status)
    return Response(generator(), headers=headers, status=response_status)


@app.get("/download")
def download():
    """Download a YouTube video as a tagged MP3 to <dest>.mp3.

    GET /download?id=<videoId>&dest=<path_no_ext>[&artist=<a>&title=<t>]
    Returns {"path": "<dest>.mp3"} on success.
    """
    video_id = request.args.get("id", "").strip()
    dest = request.args.get("dest", "").strip()
    artist = request.args.get("artist", "").strip()
    title = request.args.get("title", "").strip()
    if not video_id or not dest:
        abort(400, "missing id or dest")

    # Confine writes to the shared music root.
    full_dest = os.path.realpath(dest)
    if not (full_dest == _DOWNLOAD_ROOT or full_dest.startswith(_DOWNLOAD_ROOT + os.sep)):
        abort(400, "dest outside download root")
    os.makedirs(os.path.dirname(full_dest) or ".", exist_ok=True)

    ytdlp_args = [
        "-x",
        "-f", "141/140/bestaudio[ext=m4a]/bestaudio",
        "--audio-format", "mp3",
        "--audio-quality", "0",
        "--embed-metadata",
        "--embed-thumbnail",
        "--convert-thumbnails", "jpg",
        "-o", f"{full_dest}.%(ext)s",
        f"https://www.youtube.com/watch?v={video_id}",
    ]
    path = f"{full_dest}.mp3"

    def _sweep_partials() -> None:
        for ext in (".jpg", ".webp", ".png", ".mp3"):
            leftover = full_dest + ext
            if os.path.exists(leftover):
                try:
                    os.remove(leftover)
                except OSError:
                    pass

    def _attempt():
        # Background on purpose, regardless of what triggered it: this holds its
        # slot for up to five minutes, and a single star-triggered download must
        # never be able to sit in a slot a play is waiting for.
        return _run(ytdlp_args, timeout=300, label="download", bg=True, capture=True)

    cp = _attempt()
    if cp is None or cp.returncode != 0 or not os.path.exists(path):
        # Downloads can be the only thing exercising YouTube on an instance with
        # DownloadSource=YouTube, so this path needs its own rung of the ladder
        # rather than waiting for a play to notice the binary went stale. Gated
        # on a 403 in stderr so an unavailable or private video never triggers a
        # binary fetch.
        stderr = (cp.stderr if cp is not None else "") or ""
        if "403" in stderr and _maybe_self_update("download: yt-dlp refused with 403"):
            _sweep_partials()
            cp = _attempt()

    if cp is None or cp.returncode != 0 or not os.path.exists(path):
        _sweep_partials()
        return jsonify(error="download_failed", expected=path), 502

    # Overwrite yt-dlp's video-derived title/artist with the clean values Octo
    # passed, so Navidrome groups the track correctly. Codec-copy keeps the audio
    # and the embedded cover; list args are quoting-safe for spaces. Best-effort:
    # on failure we keep the original mp3.
    if artist or title:
        tagged = f"{full_dest}.tagged.mp3"
        meta = []
        if artist:
            meta += ["-metadata", f"artist={artist}", "-metadata", f"album_artist={artist}"]
        if title:
            meta += ["-metadata", f"title={title}"]
        try:
            rc = subprocess.run(
                ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
                 "-i", path, "-map", "0", "-map_metadata", "0", "-codec", "copy",
                 *meta, tagged],
                timeout=60, check=False,
            ).returncode
            if rc == 0 and os.path.exists(tagged):
                os.replace(tagged, path)
            elif os.path.exists(tagged):
                os.remove(tagged)
        except Exception as e:
            log.warning("retag failed for %s: %s", video_id, e)

    log.info("downloaded %s -> %s", video_id, path)
    return jsonify(path=path)


# Runs under gunicorn too, where __main__ never fires. Seeding must happen
# before anything executes the binary. The version goes in the log because a
# stale yt-dlp still answers /health with ok:true and still resolves metadata,
# so "which version is this" needs to be answerable without docker exec.
_seed_writable_ytdlp()
log.info("yt-dlp %s at %s", _ytdlp_version() or "unknown", YTDLP)


if __name__ == "__main__":
    log.info("yt-dlp-shim listening on :%d using %s", PORT, YTDLP)
    app.run(host="0.0.0.0", port=PORT, threaded=True)
