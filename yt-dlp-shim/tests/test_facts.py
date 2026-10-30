"""The facts of a resolved stream: yt-dlp's printed format line first, the URL's own
parameters when yt-dlp printed NA, and the bytes of a play over both."""
import os
import subprocess
import sys
import threading

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import facts  # noqa: E402

URL_140 = (
    "https://rr1---sn-x.googlevideo.com/videoplayback?expire=1&itag=140&source=youtube"
    "&mime=audio%2Fmp4&clen=6755585&dur=417.381"
)
URL_251 = "https://rr1---sn-x.googlevideo.com/videoplayback?itag=251&mime=audio%2Fwebm&clen=7012345"


def test_the_printed_line_gives_the_format_and_the_exact_size():
    assert facts.parse_format_line("140|6755585|m4a|129.486") == {
        "itag": "140",
        "size": 6755585,
        "ext": "m4a",
        "abr": 129,
    }


def test_a_field_yt_dlp_did_not_know_is_none():
    assert facts.parse_format_line("251|NA|webm|NA") == {
        "itag": "251",
        "size": None,
        "ext": "webm",
        "abr": None,
    }
    assert facts.parse_format_line("garbage") == {"itag": None, "size": None, "ext": None, "abr": None}


def test_the_url_names_its_itag_size_and_container():
    assert facts.facts_from_url(URL_140) == {"itag": "140", "size": 6755585, "ext": "m4a", "abr": None}
    assert facts.facts_from_url(None)["size"] is None


def test_clen_is_only_the_fallback_when_yt_dlp_printed_no_size():
    # A printed size wins even where the URL says something else.
    assert facts.stream_facts("140|6755000|m4a|129.5", URL_140)["size"] == 6755000
    assert facts.stream_facts("251|NA|webm|NA", URL_251) == {
        "itag": "251",
        "size": 7012345,
        "ext": "webm",
        "abr": None,
    }
    assert facts.stream_facts(None, URL_140)["itag"] == "140"


def test_a_run_splits_into_its_json_its_format_line_and_its_url():
    out = '{"id": "abc", "title": "A | B | C | D"}\n140|6755585|m4a|129.486\n' + URL_140 + "\n"
    rest, format_line, url = facts.split_run(out)
    assert rest == ['{"id": "abc", "title": "A | B | C | D"}']
    assert format_line == "140|6755585|m4a|129.486"
    assert url == URL_140
    # A video with no formats prints NA for the URL.
    rest, format_line, url = facts.split_run('{"id": "x"}\nNA|NA|NA|NA\nNA\n')
    assert url is None and format_line == "NA|NA|NA|NA"


@pytest.mark.parametrize(
    "header,total",
    [("bytes 0-1/12345", 12345), ("bytes */12345", 12345), ("bytes 0-1/*", None), (None, None), ("/", None)],
)
def test_the_total_after_the_slash_of_a_content_range(header, total):
    assert facts.total_from_content_range(header) == total


@pytest.mark.parametrize(
    "header,wanted",
    [
        ("bytes=0-", (0, None)),
        ("bytes=131072-20112002", (131072, 20112002)),
        ("bytes = 5 - 9", (5, 9)),
        ("bytes=-500", (None, 500)),
        ("bytes=0-1,5-9", None),
        ("bytes=9-5", None),
        ("bytes=x-", None),
        ("items=0-1", None),
        ("bytes=-", None),
        (None, None),
    ],
)
def test_a_range_header_reads_as_its_first_and_last_byte(header, wanted):
    assert facts.parse_range(header) == wanted


@pytest.mark.parametrize(
    "header,end",
    [("bytes 0-1/12345", 1), ("bytes 131072-8519679/20112003", 8519679), ("bytes */12345", None), (None, None)],
)
def test_the_last_byte_a_content_range_covers(header, end):
    assert facts.range_end(header) == end


app = pytest.importorskip("app")


def test_facts_keep_what_is_known_and_a_played_size_replaces_a_printed_one():
    app._facts_put("vid-keep", {"itag": "140", "size": 100, "ext": "m4a", "abr": 129})
    app._facts_put("vid-keep", {"size": 120, "itag": None})
    assert app._facts_get("vid-keep") == {"itag": "140", "size": 120, "ext": "m4a", "abr": 129}


def test_a_search_payload_carries_its_videos_facts():
    app._facts_put("vid-payload", {"itag": "251", "size": 7012345, "ext": "webm", "abr": 160})
    payload = app._with_facts({"video_id": "vid-payload", "title": "t"})
    assert payload["size"] == 7012345 and payload["itag"] == "251" and payload["abr"] == 160
    assert "size" not in app._with_facts({"video_id": "vid-unknown"})


def test_facts_answers_from_what_is_known_without_running_yt_dlp(monkeypatch):
    app._facts_put("vid-known01", {"itag": "140", "size": 6755585, "ext": "m4a", "abr": 129})
    monkeypatch.setattr(app, "_run", lambda *a, **k: pytest.fail("no yt-dlp run for a known video"))
    answer = app.app.test_client().get("/facts?id=vid-known01")
    assert answer.status_code == 200
    assert answer.get_json() == {"video_id": "vid-known01", "itag": "140", "size": 6755585, "ext": "m4a", "abr": 129}


def test_facts_resolves_an_unknown_video_with_the_format_line(monkeypatch):
    calls = []

    def run(args, **kwargs):
        calls.append(args)
        return "140|6755585|m4a|129.486\n" + URL_140 + "\n"

    monkeypatch.setattr(app, "_run", run)
    answer = app.app.test_client().get("/facts?id=vid-cold001&bg=1")
    assert answer.get_json()["size"] == 6755585
    assert "--print" in calls[0] and facts.FORMAT_PRINT in calls[0]
    assert app._url_cache_get("vid-cold001") == URL_140


def test_facts_is_502_when_nothing_resolves(monkeypatch):
    monkeypatch.setattr(app, "_run", lambda *a, **k: None)
    assert app.app.test_client().get("/facts?id=vid-nothing").status_code == 502


@pytest.mark.parametrize(
    "value,valid",
    [
        ("dQw4w9WgXcQ", True),
        ("a-b_c-d_e-f", True),
        ("dQw4w9WgXc", False),
        ("dQw4w9WgXcQQ", False),
        ("dQw4w9WgXc!", False),
        ("../../etc/x", False),
        ("", False),
        (None, False),
    ],
)
def test_a_video_id_is_eleven_letters_digits_dashes_or_underscores(value, valid):
    assert facts.is_video_id(value) is valid


def test_facts_refuses_an_id_that_is_not_a_video_id_without_running_yt_dlp(monkeypatch):
    monkeypatch.setattr(app, "_run", lambda *a, **k: pytest.fail("no yt-dlp run for a bad id"))
    for bad in ("short", "dQw4w9WgXcQ%20--exec", "ytsearch1:x"):
        answer = app.app.test_client().get(f"/facts?id={bad}")
        assert answer.status_code == 400, bad


@pytest.mark.parametrize(
    "header,start",
    [("bytes 5-9/1234", 5), ("bytes 131072-8519679/20112003", 131072), ("bytes */12345", None), (None, None)],
)
def test_the_first_byte_a_content_range_covers(header, start):
    assert facts.range_start(header) == start


# ---- fresh=1: a resolve of its own, and why one found nothing ----------------------------

FRESH_RUN = '{"channel": "The Beatles - Topic"}\n140|6755585|m4a|129.486\n' + URL_140 + "\n"
CLIENT = ["--extractor-args", "youtube:player_client=visionos"]
# yt-dlp's hint after any YouTube reason that asks to sign in (`_youtube_login_hint`).
LOGIN_HINT = (
    "Use --cookies-from-browser or --cookies for the authentication. See  "
    "https://github.com/yt-dlp/yt-dlp/wiki/FAQ#how-do-i-pass-cookies-to-yt-dlp  for how to manually pass "
    "cookies. Also see  https://github.com/yt-dlp/yt-dlp/wiki/Extractors#exporting-youtube-cookies  for "
    "tips on effectively exporting YouTube cookies"
)
REMOVED = [
    "Video unavailable",
    "Video unavailable. This video has been removed by the uploader",
    "This video has been removed for violating YouTube's Terms of Service",
    "Video unavailable. This video is no longer available because the YouTube account associated with "
    "this video has been terminated.",
    "Private video. Sign in if you've been granted access to this video. " + LOGIN_HINT,
    "This video is not available",
]
BUSY = [
    "Sign in to confirm you’re not a bot. " + LOGIN_HINT,
    "Sign in to confirm you're not a bot. " + LOGIN_HINT,
    "Video unavailable. This content isn't available, try again later. The current session has been "
    "rate-limited by YouTube for up to an hour. It is recommended to use `-t sleep` to add a delay "
    "between video requests to avoid exceeding the rate limit. For more information, refer to  "
    "https://github.com/yt-dlp/yt-dlp/wiki/Extractors#this-content-isnt-available-try-again-later",
    "Unable to download API page: HTTP Error 429: Too Many Requests (caused by <HTTPError 429: Too Many "
    "Requests>)",
    "Video unavailable. YouTube is requiring a captcha challenge before playback",
]
REFUSED = [
    "Sign in to confirm your age. This video may be inappropriate for some users. " + LOGIN_HINT,
    "The uploader has not made this video available in your country\nThis video is available in Germany, "
    "France.\nYou might want to use a VPN or a proxy server (with --proxy) to workaround.",
]
FORMAT_GONE = "Requested format is not available. Use --list-formats for a list of available formats"


def ran(stdout="", stderr="", returncode=0):
    """One finished yt-dlp run, as `_run(..., capture=True)` returns it."""
    return subprocess.CompletedProcess(args=["yt-dlp"], returncode=returncode, stdout=stdout, stderr=stderr)


def failed(video_id, message):
    """A run yt-dlp ended with an error, its stderr as yt-dlp writes it."""
    return ran(stderr=f"ERROR: [youtube] {video_id}: {message}\n", returncode=1)


def runs_answer(monkeypatch, *outcomes):
    """yt-dlp answers each run with the next outcome: a finished run, None (a timeout) or GateFull
    (no slot came free). The player client is in use, so a run that gives no URL is tried again
    with yt-dlp's own client. Returns each run's arguments and keywords."""
    asked = []
    left = list(outcomes)

    def run(args, **kwargs):
        asked.append((list(args), kwargs))
        outcome = left.pop(0)
        if outcome is app.GateFull:
            raise app.GateFull()
        return outcome

    monkeypatch.setattr(app, "_run", run)
    monkeypatch.setattr(app, "_client_args", lambda: list(CLIENT))
    return asked


def fresh_facts(video_id):
    return app.app.test_client().get(f"/facts?id={video_id}&fresh=1&bg=1")


def assert_refused(answer, status, error):
    assert answer.status_code == status
    assert answer.get_json() == {"error": error, "fresh": True}
    assert answer.headers["X-Octo-Fresh"] == "1"


def test_a_fresh_facts_resolves_anew_and_never_touches_the_caches(monkeypatch):
    vid = "freshfact01"
    app._facts_put(vid, {"itag": "140", "size": 111, "ext": "m4a", "abr": 129})
    app._url_cache_put(vid, "https://googlevideo.example/cached")
    # Another resolve of this video holds its single-flight lock; a fresh one never waits on it.
    monkeypatch.setattr(app, "_GATE_WAIT_SEC", 0.1)
    leader = threading.Lock()
    leader.acquire()
    app._INFLIGHT[vid] = leader
    try:
        asked = runs_answer(monkeypatch, ran(stdout=FRESH_RUN))
        answer = fresh_facts(vid)
        assert answer.status_code == 200
        assert answer.get_json() == {
            "video_id": vid,
            "itag": "140",
            "size": 6755585,
            "ext": "m4a",
            "abr": 129,
            "channel": "The Beatles - Topic",
            "fresh": True,
        }
        assert answer.headers["X-Octo-Fresh"] == "1"
        args, kwargs = asked[0]
        assert args[:2] == CLIENT
        assert args[args.index("-f") + 1] == app._AUDIO_FORMAT
        assert "%(.{channel})j" in args
        assert kwargs["capture"] is True and kwargs["bg"] is True
        assert app._facts_get(vid) == {"itag": "140", "size": 111, "ext": "m4a", "abr": 129}
        assert app._url_cache_get(vid) == "https://googlevideo.example/cached"
        assert app._INFLIGHT.get(vid) is leader and leader.locked()
    finally:
        app._INFLIGHT.pop(vid, None)
        leader.release()


class FreshPiece:
    """googlevideo's answer to the first piece of a 4-byte Opus stream."""

    status_code = 206
    headers = {"Content-Type": "audio/webm", "Content-Range": "bytes 0-3/4", "Content-Length": "4"}

    def iter_content(self, chunk_size):
        yield b"opus"

    def close(self):
        pass


class LoudnessBook:
    """A loudness book that records every measurement begun."""

    def __init__(self):
        self.begun = []

    def get(self, video_id):
        return None

    def begin(self, video_id):
        self.begun.append(video_id)
        return False


def test_a_fresh_stream_resolves_the_asked_format_and_keeps_nothing(monkeypatch):
    vid = "freshstrm01"
    app._url_cache_put(vid, "https://googlevideo.example/cached")
    asked = runs_answer(monkeypatch, ran(stdout="251|4|webm|160\n" + URL_251 + "\n"))
    opened = []

    def open_upstream(url, headers, video_id):
        opened.append(url)
        return FreshPiece()

    monkeypatch.setattr(app, "_open_upstream", open_upstream)
    book = LoudnessBook()
    monkeypatch.setattr(app, "_LOUDNESS", book)
    answer = app.app.test_client().get(f"/stream?id={vid}&fresh=1&bg=1&itag=251", headers={"Range": "bytes=0-"})
    assert answer.status_code == 206
    assert answer.headers["X-Octo-Fresh"] == "1"
    assert answer.data == b"opus"
    args, kwargs = asked[0]
    assert args[args.index("-f") + 1] == "251"
    assert kwargs["bg"] is True
    assert opened == [URL_251]
    # The cached URL is the plays' still, the stream's size is not the plays' to learn, and nobody
    # listens to a fresh stream.
    assert app._url_cache_get(vid) == "https://googlevideo.example/cached"
    assert app._facts_get(vid) == {}
    assert book.begun == []


def test_a_plain_answer_is_never_marked_fresh(monkeypatch):
    app._facts_put("freshplain1", {"itag": "140", "size": 6755585, "ext": "m4a", "abr": 129})
    monkeypatch.setattr(app, "_run", lambda *a, **k: pytest.fail("no yt-dlp run for a known video"))
    answer = app.app.test_client().get("/facts?id=freshplain1")
    assert "fresh" not in answer.get_json()
    assert "X-Octo-Fresh" not in answer.headers


@pytest.mark.parametrize("message", REMOVED)
def test_a_removed_video_is_410(monkeypatch, message):
    vid = "freshgone01"
    asked = runs_answer(monkeypatch, failed(vid, message), failed(vid, message))
    assert_refused(fresh_facts(vid), 410, "video_unavailable")
    # The player client's run, then yt-dlp's own client's.
    assert [args[:2] == CLIENT for args, _ in asked] == [True, False]


@pytest.mark.parametrize("message", BUSY)
def test_a_limit_is_busy_even_when_it_reads_video_unavailable(monkeypatch, message):
    vid = "freshbusy01"
    runs_answer(monkeypatch, failed(vid, message), failed(vid, message))
    assert_refused(fresh_facts(vid), 503, "busy")


def test_a_timeout_is_busy(monkeypatch):
    runs_answer(monkeypatch, None, None)
    assert_refused(fresh_facts("freshslow01"), 503, "busy")


def test_a_full_gate_is_busy_and_runs_nothing_more(monkeypatch):
    asked = runs_answer(monkeypatch, app.GateFull)
    assert_refused(fresh_facts("freshfull01"), 503, "busy")
    assert len(asked) == 1


def test_a_full_background_gate_answers_a_fresh_facts_busy(monkeypatch):
    monkeypatch.setattr(app, "_BG_GATE_WAIT_SEC", 0.05)
    held = 0
    while app._GATE.acquire(True, 0.05):
        held += 1
    try:
        assert_refused(fresh_facts("freshfull02"), 503, "busy")
    finally:
        for _ in range(held):
            app._GATE.release(True)


@pytest.mark.parametrize("message", REFUSED)
def test_an_age_gate_or_a_geo_block_is_403(monkeypatch, message):
    vid = "freshrefd01"
    runs_answer(monkeypatch, failed(vid, message), failed(vid, message))
    assert_refused(fresh_facts(vid), 403, "video_refused")


def test_a_refused_run_beside_a_removed_one_is_403(monkeypatch):
    vid = "freshrefd02"
    runs_answer(monkeypatch, failed(vid, REMOVED[1]), failed(vid, REFUSED[0]))
    assert_refused(fresh_facts(vid), 403, "video_refused")


def test_two_runs_with_one_busy_are_busy(monkeypatch):
    vid = "freshtwo001"
    runs_answer(monkeypatch, failed(vid, REMOVED[1]), None)
    assert_refused(fresh_facts(vid), 503, "busy")
    runs_answer(monkeypatch, failed(vid, BUSY[0]), failed(vid, REMOVED[1]))
    assert_refused(fresh_facts(vid), 503, "busy")


def test_yt_dlps_own_client_may_find_what_the_player_client_did_not(monkeypatch):
    vid = "freshtwo002"
    resting = app._PLAYER_CLIENT_RESTING_UNTIL
    runs_answer(monkeypatch, failed(vid, REMOVED[1]), ran(stdout=FRESH_RUN))
    answer = fresh_facts(vid)
    assert answer.status_code == 200 and answer.get_json()["fresh"] is True
    assert app._PLAYER_CLIENT_RESTING_UNTIL == resting


def test_a_format_no_longer_served_is_404_on_the_itag_path_only(monkeypatch):
    vid = "freshitag01"
    runs_answer(monkeypatch, failed(vid, FORMAT_GONE), failed(vid, FORMAT_GONE))
    assert_refused(app.app.test_client().get(f"/stream?id={vid}&fresh=1&bg=1&itag=251"), 404, "itag_unavailable")
    runs_answer(monkeypatch, failed(vid, FORMAT_GONE), failed(vid, FORMAT_GONE))
    assert_refused(fresh_facts(vid), 502, "no_audio_url")


def test_any_other_failure_is_502(monkeypatch):
    vid = "freshfail01"
    runs_answer(monkeypatch, failed(vid, "Unable to extract nsig function code"), ran(stdout="NA|NA|NA|NA\nNA\n"))
    assert_refused(fresh_facts(vid), 502, "no_audio_url")


def test_a_fresh_stream_refuses_a_bad_id_or_format_without_running_yt_dlp(monkeypatch):
    monkeypatch.setattr(app, "_run", lambda *a, **k: pytest.fail("no yt-dlp run for a bad request"))
    for query in ("id=short&fresh=1", "id=freshbad001&fresh=1&itag=251abc", "id=freshbad001&fresh=1&itag=-f"):
        answer = app.app.test_client().get(f"/stream?{query}")
        assert answer.status_code == 400, query
        assert answer.headers["X-Octo-Fresh"] == "1", query
