"""Octo's assist helper for Mornye's check.

Runs beside Mornye in Octo's Mornye image, as the user Mornye runs as. Mornye opens the browser for
its check through Octo's launcher (CHROME_PATH), which notes the browser's DevTools port in
$MORNYE_DATA_DIR/octo-assist/browser-<pid>.port. When a person presses Pass on Octo's dashboard,
Octo arms this helper: it ends Mornye's own pause for the gateways of the extensions Octo names.
From then on it shows the browsers Mornye opens, and passes that person's mouse events into them.
It never moves, clicks or types on its own.

Every call needs Mornye's web token (Mornye's cookie, or the X-Mornye-Token header). With no token
set, every call is refused.

Routes:
  GET  /assist        the browsers opened after the latest arm, and Mornye's pauses that still hold
  POST /assist/arm    {"extensions": [ids]}: ends the pauses of those extensions' gateways that
                      keep Mornye's own schedule; a pause the gateway asked for stays
  GET  /assist/frame  ?browser=<id>&after=<seq>: the browser's latest screen as a JPEG, or 204
  POST /assist/input  {"browser", "seq", "type", "x", "y"}: one mouse event, in the screen's CSS
                      pixels; a seq not above the last one taken for that browser is dropped
"""

from __future__ import annotations

import asyncio
import base64
import contextlib
import hashlib
import hmac
import json
import logging
import math
import os
import re
import time
import urllib.request
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

import websockets
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse, Response
from starlette.routing import Route

log = logging.getLogger("octo-assist")

TOKEN_COOKIES = ("mornye_web_token", "spotiflac_web_token")
TOKEN_HEADER = "x-mornye-token"
MOUSE_TYPES = ("mouseMoved", "mousePressed", "mouseReleased")

# Mornye's own pause schedule after a failed check: 300 s, three times longer after each failure
# in a row, at most 4 h. A pause that lasts longer than this (plus slack for the write) was asked
# for by the gateway.
SCHEDULE_BASE_S = 300
SCHEDULE_FACTOR = 3
SCHEDULE_MAX_S = 4 * 3600
SCHEDULE_SLACK_S = 5

# An extension's last error that names a request to wait: a 429 or a Retry-After. A 403 is the
# gateway refusing a grant, which is the failed check a person can pass, not a wait.
REFUSAL = re.compile(r"(?<![0-9])429(?![0-9])|retry-after", re.IGNORECASE)

PAUSE_SUFFIX = ".auth-backoff.json"

# How long a browser's screen and connection are kept after the last frame or event asked.
IDLE_SECONDS = 10.0
# How often the port files are read, and a session looks whether it is still wanted.
LOOK_SECONDS = 0.25
# How long one DevTools call may take.
CALL_SECONDS = 5.0


# ----- Mornye's files, read the way Mornye reads them -----


def extension_id(name: str) -> str:
    """An extension's id by Mornye's own rule, as Octo applies it: without a leading `ext:`, then
    without a trailing `-web`, then without a trailing `-py`. Case is kept."""
    name = str(name).strip()
    name = name.removeprefix("ext:")
    name = name.removesuffix("-web")
    return name.removesuffix("-py")


def read_json(path: Path) -> dict:
    """A JSON object from a file, or an empty one when the file is missing or not an object."""
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    return data if isinstance(data, dict) else {}


def pause_file_name(block: object) -> str | None:
    """The pause file Mornye writes for a manifest's signedSession block, named as Mornye names it:
    .<namespace>-<first 12 hex of sha256(the base URL without its trailing slash, lowercased)>
    .auth-backoff.json. None when the block names no namespace or base URL."""
    if not isinstance(block, dict):
        return None
    namespace = str(block.get("namespace", ""))
    base_url = str(block.get("baseUrl", "")).rstrip("/")
    if not namespace or not base_url:
        return None
    gateway = hashlib.sha256(base_url.lower().encode()).hexdigest()[:12]
    return f".{namespace}-{gateway}{PAUSE_SUFFIX}"


def gateways(extensions_dir: Path) -> dict[str, list[str]]:
    """Each pause file Mornye's installed extensions can write, with the names of the extensions
    on that gateway. Read from the manifests as Mornye's session listing reads them: folders
    starting with a dot or ending in .previous are skipped, and a manifest's name falls back to
    its folder's."""
    found: dict[str, list[str]] = {}
    try:
        entries = sorted(extensions_dir.iterdir())
    except OSError:
        return found
    for entry in entries:
        if entry.name.startswith(".") or entry.name.endswith(".previous"):
            continue
        manifest_path = entry / "manifest.json"
        if not manifest_path.is_file():
            continue
        manifest = read_json(manifest_path)
        name = str(manifest.get("name") or entry.name)
        file = pause_file_name(manifest.get("signedSession"))
        if file:
            found.setdefault(file, []).append(name)
    return found


def last_errors(cache_dir: Path) -> dict[str, list[str]]:
    """Each extension id's last errors in Mornye's provider health file (its provider:<name> keys;
    one extension can be named there more than one way)."""
    errors: dict[str, list[str]] = {}
    for key, entry in read_json(cache_dir / "provider_priority.json").items():
        kind, _, name = str(key).partition(":")
        if kind != "provider" or not name or not isinstance(entry, dict):
            continue
        text = str(entry.get("last_error") or "")
        if text:
            errors.setdefault(extension_id(name), []).append(text)
    return errors


def names_refusal(text: str) -> bool:
    """Whether an error names a request to wait: a 429 or a Retry-After."""
    return bool(REFUSAL.search(text or ""))


def own_schedule_s(failures: int) -> int | None:
    """How long Mornye's own schedule pauses after this many failures in a row, or None for a
    count Mornye never writes."""
    if failures < 1:
        return None
    return min(SCHEDULE_BASE_S * SCHEDULE_FACTOR ** min(failures - 1, 10), SCHEDULE_MAX_S)


@dataclass
class Pause:
    """One of Mornye's pause files: its count of failures, its end, and when it was written."""

    file: str
    failures: int | None
    not_before: float
    stamped: float


def read_pause(path: Path) -> Pause | None:
    """A pause file, or None when it is missing or unreadable."""
    try:
        stamped = path.stat().st_mtime
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    if not isinstance(data, dict):
        return None
    try:
        not_before = float(data.get("not_before") or 0)
    except (TypeError, ValueError):
        return None
    failures = data.get("failures")
    if isinstance(failures, bool) or not isinstance(failures, (int, float)) or not math.isfinite(failures):
        failures = None
    return Pause(path.name, None if failures is None else int(failures), not_before, stamped)


def asked_by_gateway(pause: Pause, errors: list[str]) -> bool:
    """Whether the gateway itself set this pause, so it is not Mornye's to end: it lasts longer
    than Mornye's own schedule for its failures (or names no failures), or an extension on the
    gateway last failed on an HTTP refusal, whose wait can be shorter than Mornye's own."""
    schedule = None if pause.failures is None else own_schedule_s(pause.failures)
    if schedule is None or pause.not_before - pause.stamped > schedule + SCHEDULE_SLACK_S:
        return True
    return any(names_refusal(text) for text in errors)


def end_pause(path: Path, now: float) -> bool:
    """Ends one of Mornye's pauses: its end moves to now and its count of failures stays, so a
    check that fails again pauses as long as Mornye's schedule says. Mornye reads the file before
    each try. Written aside and renamed into place."""
    data = read_json(path)
    if not data:
        return False
    data["not_before"] = now
    aside = path.with_name(path.name + ".octo-tmp")
    try:
        aside.write_text(json.dumps(data), encoding="utf-8")
        os.replace(aside, path)
    except OSError:
        with contextlib.suppress(OSError):
            aside.unlink()
        return False
    return True


def gateway_words(file: str, names: list[str]) -> str:
    """A gateway as people know it: the extensions on it, else the namespace in its file's name."""
    if names:
        return ", ".join(sorted(names))
    stem = file.removeprefix(".").removesuffix(PAUSE_SUFFIX)
    return stem.rsplit("-", 1)[0] if "-" in stem else stem


def utc(moment: float) -> str | None:
    """A moment as an ISO 8601 UTC time, or None when it is not one."""
    if not math.isfinite(moment):
        return None
    try:
        return datetime.fromtimestamp(moment, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    except (OverflowError, OSError, ValueError):
        return None


def seconds_left(not_before: float, now: float) -> int:
    """Whole seconds until a pause ends, rounded up."""
    left = not_before - now
    if not math.isfinite(left):
        return 10**9
    return max(0, min(int(math.ceil(left)), 10**9))


class InputError(ValueError):
    """A mouse event the helper does not take, with the words why."""


def parse_input(body: object) -> tuple[str, int, str, float, float]:
    """A posted mouse event as (browser, seq, type, x, y), or InputError."""
    if not isinstance(body, dict):
        raise InputError("Send browser, seq, type, x and y.")
    browser, seq, kind, x, y = (body.get(key) for key in ("browser", "seq", "type", "x", "y"))
    if isinstance(browser, int) and not isinstance(browser, bool):
        browser = str(browser)
    if not isinstance(browser, str) or not browser.isdigit():
        raise InputError("Send browser, seq, type, x and y.")
    if isinstance(seq, bool) or not isinstance(seq, int) or seq < 0:
        raise InputError("Send browser, seq, type, x and y.")
    if kind not in MOUSE_TYPES:
        raise InputError("Unknown mouse event.")
    for value in (x, y):
        if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
            raise InputError("Send browser, seq, type, x and y.")
    return browser, seq, kind, float(x), float(y)


# ----- The helper -----


@dataclass
class Config:
    """Where Mornye keeps its files, and the token every call needs."""

    data: Path
    cache: Path
    token: str
    proc: Path = Path("/proc")
    listen_port: int = 8011

    @classmethod
    def from_env(cls) -> Config:
        data = Path(os.environ.get("MORNYE_DATA_DIR") or "/home/mornye/.mornye").expanduser()
        cache = os.environ.get("MORNYE_CACHE_DIR")
        return cls(
            data=data,
            cache=Path(cache).expanduser() if cache else data / ".cache",
            token=(os.environ.get("MORNYE_WEB_TOKEN") or "").strip(),
            listen_port=int(os.environ.get("OCTO_ASSIST_PORT") or "8011"),
        )

    @property
    def ports(self) -> Path:
        return self.data / "octo-assist"

    @property
    def sessions(self) -> Path:
        return self.data / "signed_sessions"

    @property
    def extensions(self) -> Path:
        return self.data / "extensions"


def page_target(port: int) -> dict | None:
    """The browser's first page, or None when its DevTools port does not answer."""
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list", timeout=2) as answer:
            targets = json.load(answer)
    except (OSError, ValueError):
        return None
    if not isinstance(targets, list):
        return None
    return next(
        (t for t in targets if isinstance(t, dict) and t.get("type") == "page" and t.get("webSocketDebuggerUrl")),
        None,
    )


@dataclass(eq=False)
class Browser:
    """One browser Mornye opened for its check: its page, its latest screen, and its mouse."""

    id: str
    port: int
    launched: float
    url: str = ""
    frame: bytes | None = None
    seq: int = 0
    width: int = 0
    height: int = 0
    last_input_seq: int = -1
    frames_asked: float = -math.inf
    input_asked: float = -math.inf
    session: asyncio.Task | None = None
    ws: object | None = None
    connected: asyncio.Event = field(default_factory=asyncio.Event)
    input_lock: asyncio.Lock = field(default_factory=asyncio.Lock)
    next_call: int = 0
    waiting: dict[int, asyncio.Future] = field(default_factory=dict)

    def view(self) -> dict:
        return {
            "id": self.id,
            "url": self.url,
            "since": utc(self.launched),
            "seq": self.seq,
            "width": self.width,
            "height": self.height,
        }

    def _new_id(self) -> int:
        self.next_call += 1
        return self.next_call

    async def call(self, method: str, params: dict | None = None) -> dict:
        ws = self.ws
        if ws is None:
            raise ConnectionError("the browser is gone")
        call_id = self._new_id()
        answer = asyncio.get_running_loop().create_future()
        self.waiting[call_id] = answer
        try:
            await ws.send(json.dumps({"id": call_id, "method": method, "params": params or {}}))
            return await asyncio.wait_for(answer, CALL_SECONDS)
        finally:
            self.waiting.pop(call_id, None)

    async def read(self, ws) -> None:
        """Takes the browser's messages until it closes: screens, its address, and answers."""
        with contextlib.suppress(websockets.ConnectionClosed):
            async for raw in ws:
                await self.take(ws, json.loads(raw))

    async def take(self, ws, message: dict) -> None:
        method = message.get("method")
        if method == "Page.screencastFrame":
            params = message.get("params", {})
            meta = params.get("metadata", {})
            self.frame = base64.b64decode(params.get("data", ""))
            self.width = int(round(meta.get("deviceWidth") or 0))
            self.height = int(round(meta.get("deviceHeight") or 0))
            self.seq += 1
            await ws.send(json.dumps({
                "id": self._new_id(),
                "method": "Page.screencastFrameAck",
                "params": {"sessionId": params.get("sessionId")},
            }))
        elif method == "Page.frameNavigated":
            frame = message.get("params", {}).get("frame", {})
            if not frame.get("parentId"):
                self.url = frame.get("url", self.url)
        elif "id" in message:
            waiting = self.waiting.pop(message["id"], None)
            if waiting is not None and not waiting.done():
                waiting.set_result(message)

    async def mouse(self, kind: str, x: float, y: float) -> None:
        pressing = kind in ("mousePressed", "mouseReleased")
        await self.call(
            "Input.dispatchMouseEvent",
            {
                "type": kind,
                "x": x,
                "y": y,
                "button": "left" if pressing else "none",
                "buttons": 1 if kind == "mousePressed" else 0,
                "clickCount": 1 if pressing else 0,
            },
        )


class Helper:
    """The browsers, the latest arm, and the background work, for one Mornye data folder."""

    def __init__(self, config: Config, idle_seconds: float = IDLE_SECONDS, clock=time.monotonic) -> None:
        self.config = config
        self.idle_seconds = idle_seconds
        self.clock = clock
        self.browsers: dict[str, Browser] = {}
        self.tasks: set[asyncio.Task] = set()
        self.armed_at: float | None = self._armed_on_disk()

    # The latest arm is kept on disk too, so a restarted helper still knows it.
    def _armed_file(self) -> Path:
        return self.config.ports / "armed"

    def _armed_on_disk(self) -> float | None:
        with contextlib.suppress(OSError):
            return self._armed_file().stat().st_mtime
        return None

    def _mark_armed(self) -> float:
        now = time.time()
        with contextlib.suppress(OSError):
            self.config.ports.mkdir(parents=True, exist_ok=True)
            self._armed_file().write_text(utc(now) or "", encoding="utf-8")
            return self._armed_file().stat().st_mtime
        return now

    def mark_armed(self) -> None:
        """Marks an arm now, kept on disk across a restart: browsers launched from now on are
        shown, and the ones before are let go."""
        self.armed_at = self._mark_armed()

    # ----- background work, whose errors are always logged -----

    def spawn(self, coroutine, name: str) -> asyncio.Task:
        task = asyncio.get_running_loop().create_task(coroutine, name=name)
        self.tasks.add(task)
        task.add_done_callback(self._finished)
        return task

    def _finished(self, task: asyncio.Task) -> None:
        self.tasks.discard(task)
        if task.cancelled():
            return
        error = task.exception()
        if error is not None:
            log.warning("%s stopped: %r", task.get_name(), error, exc_info=error)

    async def watch(self) -> None:
        while True:
            try:
                self.look()
            except Exception:
                log.exception("reading the browsers failed")
            await asyncio.sleep(LOOK_SECONDS)

    def look(self) -> None:
        """Takes in each browser the launcher noted after the latest arm. A port file is removed
        only once its browser's process is gone; a browser from before the latest arm is let go."""
        seen: set[str] = set()
        ports = self.config.ports
        paths = sorted(ports.glob("browser-*.port")) if ports.is_dir() else []
        for path in paths:
            pid = path.name.removeprefix("browser-").removesuffix(".port")
            if not pid.isdigit():
                continue
            if not (self.config.proc / pid).exists():
                with contextlib.suppress(OSError):
                    path.unlink()
                continue
            try:
                launched = path.stat().st_mtime
            except OSError:
                continue
            if self.armed_at is None or launched <= self.armed_at:
                continue
            seen.add(pid)
            if pid in self.browsers:
                continue
            try:
                port = int(path.read_text(encoding="utf-8").strip())
            except (OSError, ValueError):
                continue
            self.browsers[pid] = Browser(pid, port, launched)
        for pid in [pid for pid in self.browsers if pid not in seen]:
            self.drop(pid)

    def drop(self, pid: str) -> None:
        browser = self.browsers.pop(pid, None)
        if browser is not None and browser.session is not None:
            browser.session.cancel()

    # ----- a browser's session: connected only while a frame or an event asked lately -----

    def frames_wanted(self, browser: Browser) -> bool:
        return self.clock() - browser.frames_asked < self.idle_seconds

    def wanted(self, browser: Browser) -> bool:
        return self.clock() - max(browser.frames_asked, browser.input_asked) < self.idle_seconds

    def want(self, browser: Browser, frames: bool) -> None:
        if frames:
            browser.frames_asked = self.clock()
        else:
            browser.input_asked = self.clock()
        if browser.session is None or browser.session.done():
            browser.session = self.spawn(self.run_session(browser), f"browser {browser.id}")

    async def run_session(self, browser: Browser) -> None:
        page = await asyncio.to_thread(page_target, browser.port)
        if page is None:
            return
        browser.url = page.get("url", browser.url)
        try:
            async with websockets.connect(
                page["webSocketDebuggerUrl"], max_size=None, ping_interval=None, open_timeout=CALL_SECONDS
            ) as ws:
                browser.ws = ws
                reader = self.spawn(browser.read(ws), f"browser {browser.id} reader")
                try:
                    await browser.call("Page.enable")
                    browser.connected.set()
                    casting = False
                    while self.wanted(browser) and not reader.done():
                        frames = self.frames_wanted(browser)
                        if frames and not casting:
                            await browser.call(
                                "Page.startScreencast",
                                {"format": "jpeg", "quality": 70, "maxWidth": 1280, "maxHeight": 900},
                            )
                            casting = True
                        elif casting and not frames:
                            await browser.call("Page.stopScreencast")
                            casting = False
                        await asyncio.wait([reader], timeout=LOOK_SECONDS)
                    if casting and not reader.done():
                        with contextlib.suppress(ConnectionError, asyncio.TimeoutError, websockets.ConnectionClosed):
                            await browser.call("Page.stopScreencast")
                finally:
                    browser.connected.clear()
                    browser.ws = None
                    reader.cancel()
        except (OSError, asyncio.TimeoutError, websockets.ConnectionClosed, websockets.InvalidHandshake) as error:
            log.info("browser %s closed: %r", browser.id, error)
        finally:
            browser.connected.clear()
            browser.ws = None

    # ----- pauses and arming -----

    def pause_rows(self, now: float) -> list[dict]:
        names = gateways(self.config.extensions)
        errors = last_errors(self.config.cache)
        rows = []
        sessions = self.config.sessions
        for path in sorted(sessions.glob(f".*{PAUSE_SUFFIX}")) if sessions.is_dir() else []:
            pause = read_pause(path)
            if pause is None or not pause.not_before > now:
                continue
            on = names.get(pause.file, [])
            rows.append({
                "file": pause.file,
                "gateway": gateway_words(pause.file, on),
                "seconds": seconds_left(pause.not_before, now),
                "gatewayAsked": asked_by_gateway(pause, errors_of(on, errors)),
                "stampedAfterArm": self.armed_at is not None and pause.stamped > self.armed_at,
            })
        return rows

    def arm(self, ids: set[str]) -> dict:
        """Ends the pauses of the named extensions' gateways that keep Mornye's own schedule, and,
        when it ended one, marks the arm: browsers launched from now on are shown. An arm that
        ends nothing leaves the mark as it was, so a run already open keeps its browser."""
        now = time.time()
        names = gateways(self.config.extensions)
        errors = last_errors(self.config.cache)
        ended: list[str] = []
        asked: list[dict] = []
        for file, on in sorted(names.items()):
            if not any(extension_id(name) in ids for name in on):
                continue
            path = self.config.sessions / file
            pause = read_pause(path)
            if pause is None or not pause.not_before > now:
                continue
            if asked_by_gateway(pause, errors_of(on, errors)):
                asked.append({"gateway": gateway_words(file, on), "until": utc(pause.not_before)})
                continue
            if end_pause(path, now):
                ended.append(file)
        if ended:
            self.mark_armed()
        log.info("armed: ended %s, kept %s", ended, [row["gateway"] for row in asked])
        return {"ended": ended, "gatewayAsked": asked}

    # ----- routes -----

    def allowed(self, request: Request) -> bool:
        """Only a caller with Mornye's web token; with no token set, nobody."""
        token = self.config.token
        if not token:
            return False
        given = [request.cookies.get(name, "") for name in TOKEN_COOKIES]
        given.append(request.headers.get(TOKEN_HEADER, ""))
        return any(hmac.compare_digest(token.encode(), value.encode()) for value in given if value)

    def refused(self) -> JSONResponse:
        if not self.config.token:
            return JSONResponse(
                {"error": "Mornye has no web token, so its check cannot be passed from Octo. Set SPOTIFLAC_WEB_TOKEN."},
                status_code=401,
            )
        return JSONResponse({"error": "The Mornye web token is missing or wrong."}, status_code=401)

    async def route_state(self, request: Request) -> Response:
        if not self.allowed(request):
            return self.refused()
        self.look()
        browsers = sorted(self.browsers.values(), key=lambda browser: browser.launched)
        return JSONResponse({
            "browsers": [browser.view() for browser in browsers],
            "paused": self.pause_rows(time.time()),
            "armed": None if self.armed_at is None else utc(self.armed_at),
        })

    async def route_arm(self, request: Request) -> Response:
        if not self.allowed(request):
            return self.refused()
        try:
            body = await request.json()
        except ValueError:
            body = None
        ids = body.get("extensions") if isinstance(body, dict) else None
        if not isinstance(ids, list) or not all(isinstance(value, str) for value in ids):
            return JSONResponse({"error": "Send the extensions to arm, as a list of ids."}, status_code=400)
        # Octo sends ids already in Mornye's form; applying the rule again could change one.
        answer = self.arm({value.strip() for value in ids if value.strip()})
        self.look()
        return JSONResponse(answer)

    async def route_frame(self, request: Request) -> Response:
        if not self.allowed(request):
            return self.refused()
        browser = self.browsers.get(request.query_params.get("browser", ""))
        try:
            after = int(request.query_params.get("after") or 0)
        except ValueError:
            after = 0
        if browser is None:
            return Response(status_code=204)
        self.want(browser, frames=True)
        if browser.frame is None or browser.seq <= after:
            return Response(status_code=204)
        return Response(
            browser.frame,
            media_type="image/jpeg",
            headers={
                "X-Frame-Seq": str(browser.seq),
                "X-Frame-Width": str(browser.width),
                "X-Frame-Height": str(browser.height),
                "Cache-Control": "no-store",
            },
        )

    async def route_input(self, request: Request) -> Response:
        if not self.allowed(request):
            return self.refused()
        try:
            body = await request.json()
        except ValueError:
            body = None
        try:
            browser_id, seq, kind, x, y = parse_input(body)
        except InputError as error:
            return JSONResponse({"error": str(error)}, status_code=400)
        browser = self.browsers.get(browser_id)
        if browser is None:
            return JSONResponse({"error": "That browser has closed."}, status_code=404)
        if not (0 <= x <= max(browser.width, 1) and 0 <= y <= max(browser.height, 1)):
            return JSONResponse({"error": "That point is outside the screen."}, status_code=400)
        if seq <= browser.last_input_seq:
            return JSONResponse(
                {"error": "That pointer event came after a newer one, so it was dropped."}, status_code=409
            )
        browser.last_input_seq = seq
        self.want(browser, frames=False)
        async with browser.input_lock:
            try:
                await asyncio.wait_for(browser.connected.wait(), CALL_SECONDS)
                await browser.mouse(kind, x, y)
            except (ConnectionError, asyncio.TimeoutError, websockets.ConnectionClosed):
                return JSONResponse({"error": "That browser has closed."}, status_code=404)
        return JSONResponse({"ok": True})


def errors_of(names: list[str], errors: dict[str, list[str]]) -> list[str]:
    """The last errors of the extensions with these names."""
    return [text for name in names for text in errors.get(extension_id(name), [])]


def create_app(helper: Helper) -> Starlette:
    @contextlib.asynccontextmanager
    async def lifespan(app):
        watcher = helper.spawn(helper.watch(), "watch")
        try:
            yield
        finally:
            watcher.cancel()
            for browser in list(helper.browsers.values()):
                if browser.session is not None:
                    browser.session.cancel()

    return Starlette(
        routes=[
            Route("/assist", helper.route_state, methods=["GET"]),
            Route("/assist/arm", helper.route_arm, methods=["POST"]),
            Route("/assist/frame", helper.route_frame, methods=["GET"]),
            Route("/assist/input", helper.route_input, methods=["POST"]),
        ],
        lifespan=lifespan,
    )


if __name__ == "__main__":
    import uvicorn

    logging.basicConfig(level=logging.INFO, format="octo-assist: %(message)s")
    config = Config.from_env()
    uvicorn.run(create_app(Helper(config)), host="0.0.0.0", port=config.listen_port, log_level="warning")
