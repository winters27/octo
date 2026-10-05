"""YouTube Music search and radio for Octo's radio: the row shape Octo reads, the endpoints, the
cache, and what a failure answers."""
import sys
import threading
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
app = pytest.importorskip("app")
ytm = pytest.importorskip("ytm")


@pytest.mark.parametrize("text,expected", [
    ("2:26", 146), ("1:02:03", 3723), ("0:45", 45), ("", None), (None, None), ("live", None), (180, None),
])
def test_seconds(text, expected):
    assert ytm.seconds(text) == expected


def test_a_watch_track_becomes_a_row():
    row = ytm._row({
        "videoId": "v1", "title": "Murder In My Mind", "length": "2:26",
        "artists": [{"name": "Kordhell", "id": "UC1"}, {"name": None}], "album": {"name": "A Million Ways", "id": "MP"},
        "videoType": "MUSIC_VIDEO_TYPE_ATV",
    })
    assert row == {"videoId": "v1", "title": "Murder In My Mind", "artists": ["Kordhell"],
                   "album": "A Million Ways", "durationSeconds": 146, "videoType": "MUSIC_VIDEO_TYPE_ATV"}


def test_a_song_search_row_is_a_release_and_keeps_its_seconds():
    row = ytm._row({"videoId": "v2", "title": "Roads", "resultType": "song", "duration": "5:05",
                    "duration_seconds": 305, "artists": [{"name": "Portishead"}], "album": None})
    assert row["durationSeconds"] == 305
    assert row["videoType"] == "MUSIC_VIDEO_TYPE_ATV"
    assert row["album"] is None


def test_a_row_without_an_id_is_dropped():
    assert ytm._row({"title": "No id"}) is None


def test_each_thread_has_its_own_client(monkeypatch):
    made = []
    monkeypatch.setattr(ytm, "YTMusic", lambda: made.append(object()) or made[-1])
    monkeypatch.setattr(ytm, "_local", threading.local())
    first = ytm.client()
    assert ytm.client() is first
    other = []
    worker = threading.Thread(target=lambda: other.append(ytm.client()))
    worker.start()
    worker.join()
    assert other[0] is not first
    assert len(made) == 2


@pytest.fixture
def client(monkeypatch):
    with app._YTM_LOCK:
        app._YTM_CACHE.clear()
    return app.app.test_client()


def test_search_needs_a_query_and_a_known_filter(client):
    assert client.get("/ytm/search").status_code == 400
    assert client.get("/ytm/search?q=x&filter=albums").status_code == 400


def test_search_answers_rows_and_is_cached(client, monkeypatch):
    calls = []
    monkeypatch.setattr(ytm, "search", lambda q, f, n: calls.append((q, f, n)) or [{"videoId": "v", "title": q}])
    first = client.get("/ytm/search?q=Roads&filter=songs&limit=5").get_json()
    second = client.get("/ytm/search?q=roads&filter=songs&limit=5").get_json()
    assert first == {"tracks": [{"videoId": "v", "title": "Roads"}]}
    assert second == first
    assert calls == [("Roads", "songs", 5)]


def test_a_junk_limit_falls_back_to_the_default(client, monkeypatch):
    seen = []
    monkeypatch.setattr(ytm, "radio", lambda v, n: seen.append(n) or [])
    assert client.get("/ytm/radio?videoId=v&limit=lots").status_code == 200
    assert seen == [50]


def test_radio_answers_rows(client, monkeypatch):
    monkeypatch.setattr(ytm, "radio", lambda v, n: [{"videoId": "next", "title": "Next"}])
    assert client.get("/ytm/radio?videoId=v").get_json() == {"tracks": [{"videoId": "next", "title": "Next"}]}
    assert client.get("/ytm/radio").status_code == 400


def test_a_youtube_music_failure_is_a_502_not_a_crash(client, monkeypatch):
    def broken(*_):
        raise KeyError("contents")
    monkeypatch.setattr(ytm, "radio", broken)
    response = client.get("/ytm/radio?videoId=v")
    assert response.status_code == 502
    assert response.get_json() == {"error": "ytm_failed"}


def test_artist_radio_says_which_artist_it_found(client, monkeypatch):
    monkeypatch.setattr(ytm, "artist_radio", lambda q, n: ("Massive Attack", [{"videoId": "a", "title": "Angel"}]))
    assert client.get("/ytm/artist-radio?q=massive attack").get_json() == {
        "artist": "Massive Attack", "tracks": [{"videoId": "a", "title": "Angel"}]}


def test_artist_radio_uses_the_search_rows_radio_id(monkeypatch):
    class Fake:
        def search(self, name, filter=None, limit=None):
            assert filter == "artists"
            return [{"artist": "Nobody", "radioId": None}, {"artist": "Massive Attack", "radioId": "RDEMx"}]

        def get_watch_playlist(self, playlistId=None, limit=None):
            assert playlistId == "RDEMx"
            return {"tracks": [{"videoId": "a", "title": "Angel", "length": "6:19", "artists": [{"name": "Massive Attack"}]}]}

    monkeypatch.setattr(ytm, "client", lambda: Fake())
    artist, rows = ytm.artist_radio("massive attack", 10)
    assert artist == "Massive Attack"
    assert rows[0]["durationSeconds"] == 379
