"""Tests for Octo's assist helper: the token rule, which of Mornye's pauses an arm ends, which
browsers it shows, and that the only input a browser ever gets is what was posted, in order."""

from __future__ import annotations

import asyncio
import base64
import hashlib
import json
import math
import os
import time

import httpx
import pytest
import websockets
from websockets.asyncio.server import serve

import assist

TOKEN = "a-long-test-token"
NS = "gateway-v2-abc"
BASE = "https://gateway.invalid"
OTHER_NS = "gateway-v2-xyz"
OTHER_BASE = "https://other-gateway.invalid"


# ----- a Mornye data folder on disk -----


def make_config(tmp_path, token=TOKEN):
    data = tmp_path / "mornye"
    (data / "signed_sessions").mkdir(parents=True)
    (data / "extensions").mkdir()
    proc = tmp_path / "proc"
    proc.mkdir()
    return assist.Config(data=data, cache=data / ".cache", token=token, proc=proc)


def install(config, name, namespace=NS, base=BASE, folder=None, version="1.0"):
    """An installed extension whose manifest has a signedSession block, as Mornye keeps one."""
    path = config.extensions / (folder or name)
    path.mkdir(parents=True, exist_ok=True)
    (path / "manifest.json").write_text(json.dumps({
        "name": name,
        "version": "1.0.0",
        "signedSession": {"namespace": namespace, "baseUrl": base, "appVersion": version, "platform": "extension"},
    }))


def file_for(namespace, base):
    """The pause file's name, worked out here from Mornye's rule rather than the helper's code."""
    gateway = hashlib.sha256(base.rstrip("/").lower().encode()).hexdigest()[:12]
    return f".{namespace}-{gateway}.auth-backoff.json"


def pause(config, failures, length, namespace=NS, base=BASE, stamped=None):
    """A pause file written `length` seconds before its end, as Mornye writes one."""
    path = config.sessions / file_for(namespace, base)
    stamped = time.time() if stamped is None else stamped
    path.write_text(json.dumps({"failures": failures, "not_before": stamped + length}))
    os.utime(path, (stamped, stamped))
    return path


def health(config, rows):
    config.cache.mkdir(parents=True, exist_ok=True)
    (config.cache / "provider_priority.json").write_text(json.dumps(rows))


def browser_file(config, pid, port, launched, alive=True):
    """A port file the launcher left, launched at `launched`, its process alive or gone."""
    config.ports.mkdir(parents=True, exist_ok=True)
    path = config.ports / f"browser-{pid}.port"
    path.write_text(f"{port}\n")
    os.utime(path, (launched, launched))
    if alive:
        (config.proc / str(pid)).mkdir(exist_ok=True)
    return path


def client(helper, **kwargs):
    kwargs.setdefault("headers", {"Cookie": f"mornye_web_token={TOKEN}"})
    return httpx.AsyncClient(
        transport=httpx.ASGITransport(app=assist.create_app(helper)), base_url="http://helper", **kwargs
    )


def run(coroutine):
    return asyncio.run(coroutine)


# ----- the token rule -----


def test_with_no_token_set_every_call_is_refused_and_no_pause_ends(tmp_path):
    config = make_config(tmp_path, token="")
    install(config, "example-ext")
    path = pause(config, failures=1, length=300)
    before = path.read_bytes()
    helper = assist.Helper(config)

    async def scenario():
        async with client(helper) as http:
            answers = [
                await http.get("/assist"),
                await http.post("/assist/arm", json={"extensions": ["example-ext"]}),
                await http.get("/assist/frame", params={"browser": "1", "after": 0}),
                await http.post("/assist/input", json={"browser": "1", "seq": 1, "type": "mouseMoved", "x": 1, "y": 1}),
            ]
        return answers

    for answer in run(scenario()):
        assert answer.status_code == 401
        assert "SPOTIFLAC_WEB_TOKEN" in answer.json()["error"]
    assert path.read_bytes() == before
    assert helper.armed_at is None


def test_a_wrong_or_missing_token_is_refused(tmp_path):
    helper = assist.Helper(make_config(tmp_path))

    async def scenario():
        async with client(helper, headers={}) as http:
            missing = await http.get("/assist")
        async with client(helper, headers={"Cookie": "mornye_web_token=wrong"}) as http:
            wrong = await http.get("/assist")
        return missing, wrong

    for answer in run(scenario()):
        assert answer.status_code == 401
        assert answer.json()["error"] == "The Mornye web token is missing or wrong."


@pytest.mark.parametrize(
    "headers",
    [
        {"Cookie": f"mornye_web_token={TOKEN}"},
        {"Cookie": f"spotiflac_web_token={TOKEN}"},
        {"Cookie": f"mornye_web_token={TOKEN}; spotiflac_web_token={TOKEN}"},
        {"X-Mornye-Token": TOKEN},
    ],
)
def test_the_right_token_is_taken_from_either_cookie_or_the_header(tmp_path, headers):
    helper = assist.Helper(make_config(tmp_path))

    async def scenario():
        async with client(helper, headers=headers) as http:
            return await http.get("/assist")

    answer = run(scenario())
    assert answer.status_code == 200
    assert answer.json() == {"browsers": [], "paused": [], "armed": None}


def test_the_token_is_compared_in_constant_time(tmp_path, monkeypatch):
    seen = []
    real = assist.hmac.compare_digest

    def spy(a, b):
        seen.append((a, b))
        return real(a, b)

    monkeypatch.setattr(assist.hmac, "compare_digest", spy)
    helper = assist.Helper(make_config(tmp_path))

    async def scenario():
        async with client(helper) as http:
            return await http.get("/assist")

    assert run(scenario()).status_code == 200
    assert (TOKEN.encode(), TOKEN.encode()) in seen


# ----- Mornye's files -----


def test_extension_ids_follow_mornyes_rule():
    assert assist.extension_id("ext:example-ext") == "example-ext"
    assert assist.extension_id("example-ext-web") == "example-ext"
    assert assist.extension_id("example-ext-py") == "example-ext"
    assert assist.extension_id("Example-Ext") == "Example-Ext"
    assert assist.extension_id("example-web-py") == "example-web"


def test_a_manifest_names_the_pause_file_as_mornye_does():
    block = {"namespace": NS, "baseUrl": "https://Gateway.Invalid/"}
    assert assist.pause_file_name(block) == file_for(NS, "https://gateway.invalid")
    assert assist.pause_file_name({"namespace": NS}) is None
    assert assist.pause_file_name({"baseUrl": BASE}) is None
    assert assist.pause_file_name("not a block") is None


def test_gateways_read_every_installed_manifest_like_mornye(tmp_path):
    config = make_config(tmp_path)
    install(config, "example-ext")
    install(config, "example-ext-two", version="2.0")
    install(config, "other-ext", namespace=OTHER_NS, base=OTHER_BASE)
    install(config, "hidden-ext", folder=".hidden")
    install(config, "old-ext", folder="old-ext.previous")
    unnamed = config.extensions / "unnamed-folder"
    unnamed.mkdir()
    (unnamed / "manifest.json").write_text(json.dumps({"signedSession": {"namespace": NS, "baseUrl": BASE}}))
    plain = config.extensions / "plain-ext"
    plain.mkdir()
    (plain / "manifest.json").write_text(json.dumps({"name": "plain-ext"}))

    assert assist.gateways(config.extensions) == {
        file_for(NS, BASE): ["example-ext", "example-ext-two", "unnamed-folder"],
        file_for(OTHER_NS, OTHER_BASE): ["other-ext"],
    }


def test_last_errors_are_read_by_extension_id(tmp_path):
    config = make_config(tmp_path)
    health(config, {
        "provider:ext:example-ext": {"last_error": "HTTP 429 Too Many Requests"},
        "provider:other-ext-py": {"last_error": "EACCES"},
        "provider:quiet-ext": {"last_error": ""},
        "https://endpoint.invalid": {"last_error": "403"},
    })
    assert assist.last_errors(config.cache) == {
        "example-ext": ["HTTP 429 Too Many Requests"],
        "other-ext": ["EACCES"],
    }


# ----- Mornye's own schedule, and pauses the gateway asked for -----


def test_mornyes_own_schedule():
    assert [assist.own_schedule_s(n) for n in (1, 2, 3, 4, 5, 6, 10**9)] == [300, 900, 2700, 8100, 14400, 14400, 14400]
    assert assist.own_schedule_s(0) is None


@pytest.mark.parametrize(
    ("failures", "length", "asked"),
    [
        (1, 300, False),
        (1, 304, False),
        (1, 306, True),
        (2, 900, False),
        (3, 2700, False),
        (5, 14400, False),
        (5, 14406, True),
        (1, 17 * 3600, True),
        (None, 300, True),
    ],
)
def test_a_pause_longer_than_mornyes_schedule_was_asked_for(tmp_path, failures, length, asked):
    stamped = time.time()
    pause_row = assist.Pause("f", failures, stamped + length, stamped)
    assert assist.asked_by_gateway(pause_row, []) is asked


@pytest.mark.parametrize(
    ("text", "refusal"),
    [
        ("HTTP 429 Too Many Requests", True),
        # A refused grant after a failed check: the person can pass it.
        ("Client error '403 Forbidden' for url", False),
        ("retry-after: 60", True),
        ("The gateway said Retry-After 3600", True),
        ("track 14290 not found", False),
        ("EACCES: permission denied", False),
        ("", False),
    ],
)
def test_an_http_refusal_is_named_by_its_error(text, refusal):
    assert assist.names_refusal(text) is refusal


def test_a_pause_on_mornyes_schedule_after_a_refusal_counts_as_asked():
    stamped = time.time()
    own = assist.Pause("f", 1, stamped + 300, stamped)
    assert assist.asked_by_gateway(own, ["EACCES"]) is False
    assert assist.asked_by_gateway(own, ["HTTP 429"]) is True


# ----- arming -----


def test_arm_ends_only_the_named_extensions_gateways(tmp_path):
    config = make_config(tmp_path)
    install(config, "example-ext")
    install(config, "example-ext-two", version="2.0")
    install(config, "other-ext", namespace=OTHER_NS, base=OTHER_BASE)
    armed_gateway = pause(config, failures=2, length=900)
    other_gateway = pause(config, failures=1, length=300, namespace=OTHER_NS, base=OTHER_BASE)
    other_before = (other_gateway.read_bytes(), other_gateway.stat().st_mtime)
    helper = assist.Helper(config)

    before = time.time()
    answer = helper.arm({"example-ext"})

    assert answer == {"ended": [armed_gateway.name], "gatewayAsked": []}
    record = json.loads(armed_gateway.read_text())
    assert record["failures"] == 2
    assert before <= record["not_before"] <= time.time()
    assert (other_gateway.read_bytes(), other_gateway.stat().st_mtime) == other_before
    assert helper.armed_at is not None and helper.armed_at >= armed_gateway.stat().st_mtime


def test_arm_takes_ids_in_mornyes_form(tmp_path):
    config = make_config(tmp_path)
    install(config, "example-ext-web")
    path = pause(config, failures=1, length=300)
    assert assist.Helper(config).arm({"example-ext"})["ended"] == [path.name]


def test_arm_keeps_a_pause_the_gateway_asked_for(tmp_path):
    config = make_config(tmp_path)
    install(config, "example-ext")
    path = pause(config, failures=1, length=3 * 3600)
    before = path.read_bytes()

    answer = assist.Helper(config).arm({"example-ext"})

    until = json.loads(before)["not_before"]
    assert answer == {"ended": [], "gatewayAsked": [{"gateway": "example-ext", "until": assist.utc(until)}]}
    assert path.read_bytes() == before


def test_arm_keeps_a_pause_after_a_refusal_even_on_mornyes_schedule(tmp_path):
    config = make_config(tmp_path)
    install(config, "example-ext")
    install(config, "example-ext-two", version="2.0")
    path = pause(config, failures=1, length=300)
    health(config, {"provider:ext:example-ext-two": {"last_error": "HTTP 429 Too Many Requests"}})
    before = path.read_bytes()

    answer = assist.Helper(config).arm({"example-ext"})

    assert answer["ended"] == []
    assert answer["gatewayAsked"][0]["gateway"] == "example-ext, example-ext-two"
    assert path.read_bytes() == before


def test_arm_ends_a_pause_after_a_refused_grant(tmp_path):
    # A 403 is the gateway refusing a grant after a failed check: the failure Pass is for.
    config = make_config(tmp_path)
    install(config, "example-ext")
    path = pause(config, failures=1, length=300)
    health(config, {"provider:ext:example-ext": {"last_error": "Client error '403 Forbidden' for url"}})

    answer = assist.Helper(config).arm({"example-ext"})

    assert answer == {"ended": [path.name], "gatewayAsked": []}


def test_an_arm_that_ends_nothing_keeps_the_mark_of_the_run_already_open(tmp_path):
    config = make_config(tmp_path)
    install(config, "example-ext")
    pause(config, failures=1, length=300)
    helper = assist.Helper(config)
    helper.arm({"example-ext"})
    first = helper.armed_at

    again = helper.arm({"example-ext"})

    assert again["ended"] == []
    assert helper.armed_at == first, "a browser the first run's song opens stays shown"


def test_arm_leaves_a_pause_that_already_ended_and_files_it_cannot_read(tmp_path):
    config = make_config(tmp_path)
    install(config, "example-ext")
    install(config, "other-ext", namespace=OTHER_NS, base=OTHER_BASE)
    over = pause(config, failures=1, length=300, stamped=time.time() - 400)
    broken = config.sessions / file_for(OTHER_NS, OTHER_BASE)
    broken.write_text("not json")
    over_before = over.read_bytes()

    answer = assist.Helper(config).arm({"example-ext", "other-ext"})

    assert answer == {"ended": [], "gatewayAsked": []}
    assert over.read_bytes() == over_before
    assert broken.read_text() == "not json"


def test_the_state_lists_pauses_that_hold_and_which_were_stamped_after_the_arm(tmp_path):
    config = make_config(tmp_path)
    install(config, "example-ext")
    install(config, "other-ext", namespace=OTHER_NS, base=OTHER_BASE)
    kept = pause(config, failures=1, length=3 * 3600, namespace=OTHER_NS, base=OTHER_BASE, stamped=time.time() - 60)
    pause(config, failures=1, length=300, stamped=time.time() - 10)
    helper = assist.Helper(config)
    assert helper.arm({"example-ext"})["ended"], "the arm ends example-ext's own pause"
    # Mornye tries again after the arm, fails, and pauses that gateway once more.
    stamped = pause(config, failures=2, length=900, stamped=helper.armed_at + 1)

    rows = helper.pause_rows(time.time())

    by_file = {row["file"]: row for row in rows}
    assert set(by_file) == {kept.name, stamped.name}
    assert by_file[stamped.name]["stampedAfterArm"] is True
    assert by_file[stamped.name]["gatewayAsked"] is False
    assert by_file[stamped.name]["gateway"] == "example-ext"
    assert 0 < by_file[stamped.name]["seconds"] <= 901
    assert by_file[kept.name]["stampedAfterArm"] is False
    assert by_file[kept.name]["gatewayAsked"] is True


def test_the_arm_answers_over_http_and_refuses_a_body_that_is_not_a_list(tmp_path):
    config = make_config(tmp_path)
    install(config, "example-ext")
    path = pause(config, failures=1, length=300)
    helper = assist.Helper(config)

    async def scenario():
        async with client(helper) as http:
            bad = [
                await http.post("/assist/arm", json={"extensions": "example-ext"}),
                await http.post("/assist/arm", json={"extensions": [1]}),
                await http.post("/assist/arm", content=b"not json"),
            ]
            good = await http.post("/assist/arm", json={"extensions": ["example-ext"]})
            state = await http.get("/assist")
        return bad, good, state

    bad, good, state = run(scenario())
    assert [answer.status_code for answer in bad] == [400, 400, 400]
    assert good.status_code == 200
    assert good.json() == {"ended": [path.name], "gatewayAsked": []}
    assert state.json()["armed"] == assist.utc(helper.armed_at)


def test_the_arm_survives_a_restart_of_the_helper(tmp_path):
    config = make_config(tmp_path)
    first = assist.Helper(config)
    first.mark_armed()
    assert assist.Helper(config).armed_at == first.armed_at


# ----- browsers -----


def test_only_browsers_launched_after_the_latest_arm_are_shown(tmp_path):
    config = make_config(tmp_path)
    helper = assist.Helper(config)
    now = time.time()
    browser_file(config, 101, 9001, launched=now - 30)
    helper.look()
    assert helper.browsers == {}

    helper.mark_armed()
    browser_file(config, 102, 9002, launched=helper.armed_at + 0.01)
    helper.look()
    assert list(helper.browsers) == ["102"]
    assert helper.browsers["102"].port == 9002

    # A new arm lets go of the browsers launched before it.
    time.sleep(0.05)
    helper.mark_armed()
    helper.look()
    assert helper.browsers == {}
    assert (config.ports / "browser-102.port").exists()


def test_a_port_file_is_removed_only_when_its_process_is_gone(tmp_path):
    config = make_config(tmp_path)
    helper = assist.Helper(config)
    helper.mark_armed()
    later = helper.armed_at + 2
    alive = browser_file(config, 201, 9001, launched=later)
    gone = browser_file(config, 202, 9002, launched=later, alive=False)
    old_alive = browser_file(config, 203, 9003, launched=helper.armed_at - 60)
    odd = config.ports / "browser-abc.port"
    odd.write_text("9004")

    # Nothing answers on these DevTools ports: a slow browser keeps its file all the same.
    for _ in range(20):
        helper.look()

    assert alive.exists() and old_alive.exists() and odd.exists()
    assert not gone.exists()
    assert list(helper.browsers) == ["201"]

    (config.proc / "201").rmdir()
    helper.look()
    assert not alive.exists()
    assert helper.browsers == {}


# ----- input -----


@pytest.mark.parametrize(
    "body",
    [
        None,
        [],
        {"browser": "301", "type": "mouseMoved", "x": 1, "y": 1},
        {"browser": "301", "seq": "1", "type": "mouseMoved", "x": 1, "y": 1},
        {"browser": "301", "seq": True, "type": "mouseMoved", "x": 1, "y": 1},
        {"browser": "301", "seq": -1, "type": "mouseMoved", "x": 1, "y": 1},
        {"browser": "../301", "seq": 1, "type": "mouseMoved", "x": 1, "y": 1},
        {"browser": "301", "seq": 1, "type": "keyDown", "x": 1, "y": 1},
        {"browser": "301", "seq": 1, "type": "mouseMoved", "x": "1", "y": 1},
        {"browser": "301", "seq": 1, "type": "mouseMoved", "x": math.nan, "y": 1},
        {"browser": "301", "seq": 1, "type": "mouseMoved", "x": 1, "y": math.inf},
        {"browser": "301", "seq": 1, "type": "mouseMoved", "x": True, "y": 1},
    ],
)
def test_input_that_is_not_one_mouse_event_is_refused(body):
    with pytest.raises(assist.InputError):
        assist.parse_input(body)


def test_a_mouse_event_is_parsed():
    assert assist.parse_input({"browser": 301, "seq": 4, "type": "mousePressed", "x": 10, "y": 20.5}) == (
        "301", 4, "mousePressed", 10.0, 20.5,
    )


class FakeBrowser:
    """A stand-in for a browser's DevTools on one port: /json/list over HTTP, and one page's
    websocket. It records every call it gets and, while screencasting, sends a screen every 50 ms."""

    def __init__(self):
        self.calls: list[tuple[str, dict]] = []
        self.connections = 0
        self.open = 0
        self.casting = False
        self.server = None
        self.port = 0

    async def start(self):
        self.server = await serve(self.handle, "127.0.0.1", 0, process_request=self.http)
        self.port = self.server.sockets[0].getsockname()[1]
        return self

    async def stop(self):
        self.server.close()
        await self.server.wait_closed()

    def http(self, connection, request):
        if request.path == "/json/list":
            page = {"type": "page", "url": "about:blank", "webSocketDebuggerUrl": f"ws://127.0.0.1:{self.port}/devtools/page/1"}
            return connection.respond(200, json.dumps([page]))
        return None

    async def handle(self, ws):
        self.connections += 1
        self.open += 1
        sender = asyncio.create_task(self.screens(ws))
        try:
            async for raw in ws:
                message = json.loads(raw)
                self.calls.append((message["method"], message.get("params", {})))
                if message["method"] == "Page.startScreencast":
                    self.casting = True
                elif message["method"] == "Page.stopScreencast":
                    self.casting = False
                await ws.send(json.dumps({"id": message["id"], "result": {}}))
        except websockets.ConnectionClosed:
            pass
        finally:
            sender.cancel()
            self.casting = False
            self.open -= 1

    async def screens(self, ws):
        count = 0
        while True:
            await asyncio.sleep(0.05)
            if self.casting:
                count += 1
                try:
                    await ws.send(json.dumps({
                        "method": "Page.screencastFrame",
                        "params": {
                            "data": base64.b64encode(b"jpeg-%d" % count).decode(),
                            "sessionId": count,
                            "metadata": {"deviceWidth": 1279, "deviceHeight": 756},
                        },
                    }))
                except websockets.ConnectionClosed:
                    return

    def methods(self):
        return {method for method, _ in self.calls}


async def until(condition, seconds=5.0):
    """Waits for a condition, a few seconds at most."""
    deadline = time.monotonic() + seconds
    while not condition():
        if time.monotonic() > deadline:
            return False
        await asyncio.sleep(0.02)
    return True


async def first_frame(http, browser):
    for _ in range(250):
        answer = await http.get("/assist/frame", params={"browser": browser, "after": 0})
        if answer.status_code == 200:
            return answer
        await asyncio.sleep(0.02)
    raise AssertionError("no frame arrived")


def test_only_the_posted_mouse_events_reach_the_browser_in_order(tmp_path):
    config = make_config(tmp_path)

    async def scenario():
        fake = await FakeBrowser().start()
        helper = assist.Helper(config)
        try:
            helper.mark_armed()
            browser_file(config, 301, fake.port, launched=helper.armed_at + 2)
            async with client(helper) as http:
                listed = (await http.get("/assist")).json()["browsers"]
                # Listed is not watched: nothing connects before a frame is asked.
                await asyncio.sleep(0.3)
                connections_before_ask = fake.connections

                frame = await first_frame(http, "301")
                posted = [
                    (1, "mouseMoved", 100, 200),
                    (2, "mouseMoved", 110, 205.5),
                    (3, "mousePressed", 110, 205.5),
                    (5, "mouseReleased", 110, 205.5),
                ]
                answers = []
                for seq, kind, x, y in posted:
                    body = {"browser": "301", "seq": seq, "type": kind, "x": x, "y": y}
                    answers.append((await http.post("/assist/input", json=body)).status_code)
                late = await http.post(
                    "/assist/input", json={"browser": "301", "seq": 4, "type": "mousePressed", "x": 1, "y": 1}
                )
                again = await http.post(
                    "/assist/input", json={"browser": "301", "seq": 5, "type": "mouseReleased", "x": 1, "y": 1}
                )
                outside = await http.post(
                    "/assist/input", json={"browser": "301", "seq": 6, "type": "mouseMoved", "x": 5000, "y": 1}
                )
                closed = await http.post(
                    "/assist/input", json={"browser": "999", "seq": 7, "type": "mouseMoved", "x": 1, "y": 1}
                )
                await asyncio.sleep(0.2)
            return listed, connections_before_ask, frame, posted, answers, late, again, outside, closed, fake
        finally:
            for task in list(helper.tasks):
                task.cancel()
            await fake.stop()

    listed, connections_before_ask, frame, posted, answers, late, again, outside, closed, fake = run(scenario())
    assert [row["id"] for row in listed] == ["301"]
    assert connections_before_ask == 0
    assert frame.headers["content-type"] == "image/jpeg"
    assert frame.headers["x-frame-width"] == "1279" and frame.headers["x-frame-height"] == "756"
    assert frame.content.startswith(b"jpeg-")
    assert answers == [200, 200, 200, 200]
    assert late.status_code == 409 and again.status_code == 409
    assert outside.status_code == 400
    assert closed.status_code == 404

    sent = [params for method, params in fake.calls if method.startswith("Input.")]
    assert {method for method, _ in fake.calls if method.startswith("Input.")} == {"Input.dispatchMouseEvent"}
    assert [(p["type"], p["x"], p["y"]) for p in sent] == [(kind, x, y) for _, kind, x, y in posted]
    assert [(p["button"], p["buttons"], p["clickCount"]) for p in sent] == [
        ("none", 0, 0), ("none", 0, 0), ("left", 1, 1), ("left", 0, 1),
    ]
    assert fake.methods() <= {
        "Page.enable", "Page.startScreencast", "Page.screencastFrameAck", "Page.stopScreencast",
        "Input.dispatchMouseEvent",
    }


def test_the_screen_is_cast_only_while_frames_are_asked(tmp_path):
    config = make_config(tmp_path)

    async def scenario():
        fake = await FakeBrowser().start()
        helper = assist.Helper(config, idle_seconds=0.5)
        try:
            helper.mark_armed()
            browser_file(config, 401, fake.port, launched=helper.armed_at + 2)
            async with client(helper) as http:
                await http.get("/assist")
                await first_frame(http, "401")
                casting_while_asked = fake.casting
                # Nobody asks: the cast stops and the connection closes.
                stopped = await until(lambda: "Page.stopScreencast" in fake.methods() and fake.open == 0)
                frames_after_stop = sum(1 for method, _ in fake.calls if method == "Page.screencastFrameAck")
                await asyncio.sleep(0.3)
                quiet = sum(1 for method, _ in fake.calls if method == "Page.screencastFrameAck") == frames_after_stop
                # Asking again starts it again.
                again = await http.get("/assist/frame", params={"browser": "401", "after": 10**6})
                restarted = await until(lambda: fake.casting and fake.connections == 2)
            return casting_while_asked, stopped, quiet, again.status_code, restarted
        finally:
            for task in list(helper.tasks):
                task.cancel()
            await fake.stop()

    casting_while_asked, stopped, quiet, again, restarted = run(scenario())
    assert casting_while_asked
    assert stopped
    assert quiet
    assert again == 204
    assert restarted


def test_an_event_waits_for_the_connection_and_never_starts_the_cast(tmp_path):
    config = make_config(tmp_path)

    async def scenario():
        fake = await FakeBrowser().start()
        helper = assist.Helper(config, idle_seconds=0.5)
        try:
            helper.mark_armed()
            browser_file(config, 501, fake.port, launched=helper.armed_at + 2)
            helper.look()
            helper.browsers["501"].width, helper.browsers["501"].height = 100, 100
            async with client(helper) as http:
                answer = await http.post(
                    "/assist/input", json={"browser": "501", "seq": 1, "type": "mouseMoved", "x": 5, "y": 5}
                )
            await until(lambda: fake.open == 0)
            return answer.status_code, fake
        finally:
            for task in list(helper.tasks):
                task.cancel()
            await fake.stop()

    status, fake = run(scenario())
    assert status == 200
    assert [method for method, _ in fake.calls] == ["Page.enable", "Input.dispatchMouseEvent"]


def test_a_frame_for_a_browser_not_shown_is_204(tmp_path):
    helper = assist.Helper(make_config(tmp_path))

    async def scenario():
        async with client(helper) as http:
            return await http.get("/assist/frame", params={"browser": "12", "after": "x"})

    assert run(scenario()).status_code == 204


def test_a_failed_background_task_is_logged(tmp_path, caplog):
    helper = assist.Helper(make_config(tmp_path))

    async def boom():
        raise RuntimeError("boom")

    async def scenario():
        task = helper.spawn(boom(), "a task")
        await asyncio.sleep(0)
        await asyncio.sleep(0)
        return task

    with caplog.at_level("WARNING", logger="octo-assist"):
        task = run(scenario())
    assert task.done()
    assert "a task stopped: RuntimeError('boom')" in caplog.text
    assert helper.tasks == set()


# ----- against Mornye's own code, where it is installed (the image) -----


def test_the_file_name_is_the_one_mornye_writes_and_reads(tmp_path):
    mobile = pytest.importorskip("mornye.core.signed_session_mobile")
    status = pytest.importorskip("mornye.core.signed_session_status")
    config = make_config(tmp_path)
    install(config, "example-ext")
    client_ = mobile.SignedSessionClient(base_url="https://Gateway.Invalid/", namespace=NS, data_dir=str(config.sessions))

    assert mobile._auth_backoff_path(client_).name == assist.pause_file_name(
        {"namespace": NS, "baseUrl": "https://Gateway.Invalid/"}
    )
    mobile._record_auth_failure(client_)
    info = {"base_url": "https://Gateway.Invalid", "namespace": NS}
    assert status._auth_paused_s(config.sessions, info, time.time()) > 0
    assert list(assist.gateways(config.extensions)) == [mobile._auth_backoff_path(client_).name]


def test_mornyes_own_pauses_end_on_arm_and_its_schedule_goes_on(tmp_path):
    mobile = pytest.importorskip("mornye.core.signed_session_mobile")
    config = make_config(tmp_path)
    install(config, "example-ext")
    client_ = mobile.SignedSessionClient(base_url=BASE, namespace=NS, data_dir=str(config.sessions))
    helper = assist.Helper(config)

    for _ in range(3):
        mobile._record_auth_failure(client_)
    assert mobile.auth_backoff_remaining(client_) > 2000
    assert helper.arm({"example-ext"})["ended"] == [mobile._auth_backoff_path(client_).name]
    assert mobile.auth_backoff_remaining(client_) == 0

    # Mornye's next failure pauses as its schedule says after four in a row.
    assert mobile._record_auth_failure(client_) == 8100
    row = helper.pause_rows(time.time())[0]
    assert row["stampedAfterArm"] is True and row["gatewayAsked"] is False

    # A gateway that asks for 17 hours is kept.
    mobile._record_auth_failure(client_, retry_after=17 * 3600)
    assert helper.arm({"example-ext"})["ended"] == []
    assert mobile.auth_backoff_remaining(client_) > 16 * 3600
