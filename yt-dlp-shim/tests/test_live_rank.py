"""A live video of a song is usually within seconds of the studio length, so the closest
length alone picked live takes as often as not. Live takes now sort last unless asked for."""
import os
import sys

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

app_module = pytest.importorskip("app")


def ids(ranked):
    return [r["id"] for r in ranked]


def test_a_live_take_never_beats_the_studio_song():
    candidates = [
        {"id": "live", "title": "Silverstein - Smile in Your Sleep (Live at the El Mocambo)", "duration": 209},
        {"id": "studio", "title": "Silverstein - Smile in Your Sleep", "duration": 203},
    ]
    assert ids(app_module._rank_by_length(candidates, "Silverstein - Smile in Your Sleep", 209))[0] == "studio"


def test_a_query_that_asks_for_live_ranks_by_length_alone():
    candidates = [
        {"id": "studio", "title": "Nirvana - About a Girl", "duration": 168},
        {"id": "unplugged", "title": "Nirvana - About a Girl (MTV Unplugged)", "duration": 217},
    ]
    assert ids(app_module._rank_by_length(candidates, "Nirvana - About a Girl (MTV Unplugged)", 217))[0] == "unplugged"


def test_when_every_candidate_is_live_the_closest_still_plays():
    candidates = [
        {"id": "far", "title": "Song (Live)", "duration": 400},
        {"id": "near", "title": "Song (Live in Berlin)", "duration": 201},
    ]
    assert ids(app_module._rank_by_length(candidates, "Artist - Song", 200))[0] == "near"


def test_a_candidate_without_a_length_sorts_last():
    candidates = [
        {"id": "unknown", "title": "Song", "duration": None},
        {"id": "known", "title": "Song", "duration": 250},
    ]
    assert ids(app_module._rank_by_length(candidates, "Artist - Song", 200)) == ["known", "unknown"]


# The real five YouTube answered for "KILLY - Killamonjaro" (2026-10-22). KILLY's song runs 179 s in
# the catalog and 161 s as its video, so by length alone another artist's song won.
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


@pytest.mark.parametrize("hint", [179, 171, 161, 153, 147])
def test_another_artists_song_of_a_like_title_never_wins_on_length(hint):
    assert ids(app_module._rank_by_length(KILLAMONJARO, "KILLY - Killamonjaro", hint))[0] in ("killy", "genius")


def test_among_videos_naming_the_song_the_closest_length_still_wins():
    assert ids(app_module._rank_by_length(KILLAMONJARO, "KILLY Killamonjaro", 179))[0] == "killy"
    assert ids(app_module._rank_by_length(KILLAMONJARO, "KILLY Killamonjaro", 240))[0] == "genius"


def test_the_channel_counts_as_naming_the_artist():
    candidates = [
        {"id": "other", "title": "Teardrop (Cover)", "duration": 330, "channel": "Some Singer"},
        {"id": "own", "title": "Teardrop", "duration": 320, "channel": "Massive Attack - Topic"},
    ]
    assert ids(app_module._rank_by_length(candidates, "Massive Attack Teardrop", 330))[0] == "own"


def test_accents_and_case_do_not_hide_a_word():
    candidates = [
        {"id": "far", "title": "Bjork - Joga", "duration": 400, "channel": "x"},
        {"id": "other", "title": "Someone - Joga", "duration": 305, "channel": "y"},
    ]
    assert ids(app_module._rank_by_length(candidates, "Björk Jóga", 305))[0] == "far"


def test_words_no_video_has_leave_the_length_to_decide():
    candidates = [
        {"id": "far", "title": "残酷な天使のテーゼ", "duration": 300, "channel": "アニメ"},
        {"id": "near", "title": "残酷な天使のテーゼ (TV size)", "duration": 241, "channel": "歌"},
    ]
    assert ids(app_module._rank_by_length(candidates, "Yoko Takahashi A Cruel Angel's Thesis", 240))[0] == "near"
