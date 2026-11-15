"""/candidates lists several ranked videos for a person choosing a version: ranked as /search
ranks them, skipped videos left out, at most `limit`, answers kept in the search cache, 404 when
there is none, 502 when the search failed, 503 when the gate is full."""
import json
import os
import sys

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

app_module = pytest.importorskip("app")

client = app_module.app.test_client()

KILLAMONJARO = [
    {"id": "killy", "title": "KILLY - Killamonjaro (Official Music Video)", "duration": 161, "channel": "KILLY"},
    {"id": "summer-cem", "title": "SUMMER CEM x BILLA JOE - KILLY MANJARO (Official Video)",
     "duration": 153, "channel": "Summer Cem"},
    {"id": "genius", "title": "KILLY \"Killamonjaro\" Official Lyrics & Meaning | Verified",
     "duration": 238, "channel": "Genius"},
    {"id": "euromix", "title": "SUMMER CEM x BILLA JOE x DARDAN x EZHEL - KILLY MANJARO EUROMIX (Official Video)",
     "duration": 171, "channel": "Summer Cem and 2 more"},
    {"id": "lyrics", "title": "SUMMER CEM - KILLY MANJARO (Lyrics) ft. BILLA JOE", "duration": 147,
     "channel": "Luna Liebling"},
]

QUERY = "KILLY Killamonjaro"


def _clear_caches():
    with app_module._SEARCH_CACHE_LOCK:
        app_module._SEARCH_CACHE.clear()


@pytest.fixture
def runs(monkeypatch):
    """Answers every yt-dlp search with `rows` (or fails it when `fail` is set) and records the
    arguments it was run with. A listing never resolves a stream."""
    _clear_caches()
    state = {"rows": list(KILLAMONJARO), "args": [], "fail": False}

    def fake_run(args, *a, **k):
        state["args"].append(list(args))
        if state["fail"]:
            return None
        return "\n".join(json.dumps(r) for r in state["rows"])

    def no_resolve(*a, **k):
        raise AssertionError("a listing resolves no stream")

    monkeypatch.setattr(app_module, "_run", fake_run)
    monkeypatch.setattr(app_module, "_resolve_url", no_resolve)
    yield state
    _clear_caches()


def ids(answer):
    return [c["video_id"] for c in answer.get_json()["candidates"]]


def test_candidates_are_ranked_as_search_ranks_them(runs):
    answer = client.get(f"/candidates?q={QUERY}&duration=161")
    assert answer.status_code == 200
    expected = [r["id"] for r in app_module._rank_by_length(list(KILLAMONJARO), QUERY, 161)]
    assert ids(answer) == expected
    assert ids(answer)[0] == "killy"
    assert answer.get_json()["candidates"][0] == {
        "video_id": "killy",
        "title": "KILLY - Killamonjaro (Official Music Video)",
        "duration": 161,
        "channel": "KILLY",
    }
    assert answer.get_json()["rank"] == app_module._RANK
    assert runs["args"][-1][0] == f"ytsearch10:{QUERY}"
    assert "--flat-playlist" in runs["args"][-1]


def test_skipped_videos_are_left_out_and_more_rows_are_asked_for(runs):
    answer = client.get(f"/candidates?q={QUERY}&skip=killy,genius&limit=3")
    assert answer.status_code == 200
    assert "killy" not in ids(answer)
    assert "genius" not in ids(answer)
    assert len(ids(answer)) == 3
    assert runs["args"][-1][0] == f"ytsearch5:{QUERY}"


def test_limit_is_kept_between_1_and_20(runs):
    assert len(ids(client.get(f"/candidates?q={QUERY}&limit=2"))) == 2
    assert runs["args"][-1][0] == f"ytsearch2:{QUERY}"
    client.get(f"/candidates?q={QUERY}&limit=99")
    assert runs["args"][-1][0] == f"ytsearch20:{QUERY}"
    client.get(f"/candidates?q={QUERY}&limit=0")
    assert runs["args"][-1][0] == f"ytsearch1:{QUERY}"


def test_an_answer_is_kept_in_the_search_cache(runs):
    first = client.get(f"/candidates?q={QUERY}&duration=161")
    again = client.get(f"/candidates?q={QUERY.lower()}&duration=161")
    assert len(runs["args"]) == 1
    assert again.get_json() == first.get_json()


def test_no_candidate_is_a_404_no_hit(runs):
    runs["rows"] = []
    answer = client.get("/candidates?q=nothing at all")
    assert answer.status_code == 404
    assert answer.get_json() == {"error": "no_hit"}
    runs["rows"] = [KILLAMONJARO[0]]
    skipped = client.get(f"/candidates?q={QUERY}&skip=killy")
    assert skipped.status_code == 404
    assert skipped.get_json() == {"error": "no_hit"}


def test_a_failed_search_is_a_502_and_is_not_kept(runs):
    runs["fail"] = True
    answer = client.get(f"/candidates?q={QUERY}")
    assert answer.status_code == 502
    assert answer.get_json() == {"error": "search_failed"}
    runs["fail"] = False
    assert client.get(f"/candidates?q={QUERY}").status_code == 200


def test_a_missing_query_is_a_400(runs):
    assert client.get("/candidates").status_code == 400


@pytest.fixture
def full_background_gate(monkeypatch):
    """Every background slot held, and the wait for one cut to 0.05 s."""
    _clear_caches()
    monkeypatch.setattr(app_module, "_BG_GATE_WAIT_SEC", 0.05)
    held = 0
    while app_module._GATE.acquire(True, 0.05):
        held += 1
    try:
        yield held
    finally:
        for _ in range(held):
            app_module._GATE.release(True)
        _clear_caches()


def test_a_full_gate_answers_busy(full_background_gate):
    assert full_background_gate > 0
    answer = client.get("/candidates?q=busy+gate+candidates&bg=1")
    assert answer.status_code == 503
    assert answer.get_json() == {"error": "busy"}
