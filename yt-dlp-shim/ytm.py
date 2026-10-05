"""YouTube Music search and radio for Octo's radio, through ytmusicapi (no sign-in)."""
import threading

from ytmusicapi import YTMusic

# One client per thread: ytmusicapi promises no thread safety, and gunicorn serves the shim
# from 32 threads.
_local = threading.local()


def client():
    ytm = getattr(_local, "ytm", None)
    if ytm is None:
        ytm = _local.ytm = YTMusic()
    return ytm


def seconds(length):
    """'2:26' or '1:02:03' as seconds; None when it is not a length."""
    if not length or not isinstance(length, str):
        return None
    total = 0
    for part in length.split(":"):
        if not part.isdigit():
            return None
        total = total * 60 + int(part)
    return total


def _row(item):
    video_id = item.get("videoId")
    title = item.get("title")
    if not video_id or not title:
        return None
    album = item.get("album")
    return {
        "videoId": video_id,
        "title": title,
        "artists": [a.get("name") for a in (item.get("artists") or []) if a and a.get("name")],
        "album": album.get("name") if isinstance(album, dict) else None,
        "durationSeconds": item.get("duration_seconds") or seconds(item.get("length") or item.get("duration")),
        "videoType": item.get("videoType") or ("MUSIC_VIDEO_TYPE_ATV" if item.get("resultType") == "song" else None),
    }


def search(query, filter_name="songs", limit=10):
    rows = client().search(query, filter=filter_name, limit=limit)
    return [r for r in (_row(item) for item in rows) if r]


def radio(video_id, limit=50):
    playlist = client().get_watch_playlist(videoId=video_id, radio=True, limit=limit)
    return [r for r in (_row(item) for item in playlist.get("tracks") or []) if r]


def artist_radio(name, limit=50):
    """The artist's radio, found through search: its rows carry the radio's playlist id
    (get_artist's radioId can be None on 1.12.3, ytmusicapi #1019). Returns (artist, rows)."""
    for hit in client().search(name, filter="artists", limit=5):
        radio_id = hit.get("radioId")
        if radio_id:
            playlist = client().get_watch_playlist(playlistId=radio_id, limit=limit)
            return hit.get("artist"), [r for r in (_row(item) for item in playlist.get("tracks") or []) if r]
    return None, []
