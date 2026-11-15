//! Playlists that hold songs found online, through the Subsonic routes over a stateful wiremock
//! Navidrome (its Subsonic API as each caller, its native API as the admin): conversion,
//! serving, removals, replaces, creation, deletion, the row pass, the caps' words, the sync
//! catalog's view and Keep ready.

use std::sync::Arc;

use axum::body::Body;
use base64::Engine;
use chrono::{DateTime, Duration, TimeZone, Utc};
use octo_core::family::abilities::Preset;
use octo_core::family::device::{Device, NetworkPlace};
use octo_core::models::domain::Song;
use octo_core::settings::{
    AppSettings, FamilySettings, GeneratedPlaylistSettings, LibraryActionSettings, NoticeKind,
    SubsonicSettings,
};
use octo_core::soulseek::{RoutingKind, SoulseekRouting};
use octo_subsonic::subsonic_response_builder::json_shape_to_xml;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use super::{Reply, TestMetadata, get, query_value, until};
use crate::app::{AppState, TestServices};
use crate::controllers::subsonic::action_helpers::SubsonicCall;
use crate::http::pipeline::App;
use crate::services::family::member_credentials::tests::member;
use crate::services::library::LibraryActionPlaylistProvisioner;

pub(super) const ROAD: &str = "1roadtripaaaaaaaaaaaaa";
pub(super) const ALEX_MIX: &str = "2alexmixaaaaaaaaaaaaaa";
pub(super) const SMART: &str = "3smartaaaaaaaaaaaaaaaa";
pub(super) const KID_LIST: &str = "5kidlistaaaaaaaaaaaaaa";
pub(super) const PLAIN: &str = "6plainaaaaaaaaaaaaaaaa";
pub(super) const OUTSIDE: &str = "ext-deezer-song-1";
pub(super) const OUTSIDE_EXPLICIT: &str = "ext-deezer-song-2";
pub(super) const OUTSIDE_THIRD: &str = "ext-deezer-song-3";
pub(super) const OUTSIDE_ALBUM: &str = "ext-deezer-album-5";
const NS: &str = "http://subsonic.org/restapi";

/// One song Navidrome holds.
#[derive(Clone)]
pub(super) struct NdSong {
    pub id: &'static str,
    pub title: &'static str,
    pub artist: &'static str,
    pub seconds: i64,
    /// 1 is shared; 3 is alex's own library.
    pub library: i64,
    pub explicit: &'static str,
    pub missing: bool,
}

const SONGS: &[NdSong] = &[
    NdSong {
        id: "s-a",
        title: "Alpha",
        artist: "Band",
        seconds: 100,
        library: 1,
        explicit: "clean",
        missing: false,
    },
    NdSong {
        id: "s-b",
        title: "Beta",
        artist: "Band",
        seconds: 120,
        library: 1,
        explicit: "clean",
        missing: false,
    },
    NdSong {
        id: "s-glory",
        title: "Glory",
        artist: "Somebody",
        seconds: 201,
        library: 1,
        explicit: "clean",
        missing: false,
    },
    NdSong {
        id: "s-glory2",
        title: "Glory",
        artist: "Somebody",
        seconds: 205,
        library: 1,
        explicit: "explicit",
        missing: false,
    },
    NdSong {
        id: "s-glory-live",
        title: "Glory (Live)",
        artist: "Somebody",
        seconds: 230,
        library: 1,
        explicit: "clean",
        missing: false,
    },
    NdSong {
        id: "s-explicit",
        title: "Rude",
        artist: "Band",
        seconds: 90,
        library: 1,
        explicit: "explicit",
        missing: false,
    },
    NdSong {
        id: "s-alex",
        title: "Alex Only",
        artist: "Band",
        seconds: 80,
        library: 3,
        explicit: "",
        missing: false,
    },
    NdSong {
        id: "s-missing",
        title: "Gone",
        artist: "Band",
        seconds: 60,
        library: 1,
        explicit: "",
        missing: true,
    },
];

/// One Navidrome playlist.
#[derive(Clone)]
pub(super) struct NdPlaylist {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub public: bool,
    pub smart: bool,
    pub comment: String,
    pub tracks: Vec<String>,
    pub created: DateTime<Utc>,
    pub changed: DateTime<Utc>,
}

/// Navidrome's state, changed by the calls it answers.
pub(super) struct Nd {
    pub playlists: Vec<NdPlaylist>,
    /// `<METHOD> <path> <who>` of every call, `who` being `admin` or the caller.
    pub log: Vec<String>,
    /// How many more admin track reads fail with a 500.
    pub fail_track_reads: u32,
    clock: DateTime<Utc>,
    made: u32,
}

impl Nd {
    fn tick(&mut self) -> DateTime<Utc> {
        self.clock += Duration::seconds(1);
        self.clock
    }

    pub fn add(&mut self, id: &str, name: &str, owner: &str, public: bool, smart: bool, tracks: &[&str]) {
        let at = self.tick();
        self.playlists.push(NdPlaylist {
            id: id.into(),
            name: name.into(),
            owner: owner.into(),
            public,
            smart,
            comment: String::new(),
            tracks: tracks.iter().map(|id| id.to_string()).collect(),
            created: at,
            changed: at,
        });
    }

    fn song(id: &str) -> Option<&'static NdSong> {
        SONGS.iter().find(|song| song.id == id)
    }

    /// Whether `user` may see a song: alex's own library is alex's alone.
    fn sees(user: &str, song: &NdSong) -> bool {
        song.library != 3 || user == "alex" || user == "admin"
    }

    fn find(&self, id: &str, user: &str) -> Option<usize> {
        self.playlists
            .iter()
            .position(|p| p.id == id && (user == "admin" || p.owner == user || p.public))
    }

    fn song_json(song: &NdSong) -> Value {
        json!({"id": song.id, "parent": "al-1", "isDir": false, "title": song.title,
            "album": "Album", "albumId": "al-1", "artist": song.artist, "artistId": "ar-1",
            "duration": song.seconds, "suffix": "flac", "type": "music", "explicitStatus": song.explicit})
    }

    fn row(&self, playlist: &NdPlaylist, user: &str, entries: bool) -> Value {
        let shown: Vec<&NdSong> = playlist
            .tracks
            .iter()
            .filter_map(|id| Self::song(id))
            .filter(|song| !song.missing && Self::sees(user, song))
            .collect();
        let mut row = json!({"id": playlist.id, "name": playlist.name, "owner": playlist.owner,
            "public": playlist.public, "comment": playlist.comment, "songCount": shown.len(),
            "duration": shown.iter().map(|song| song.seconds).sum::<i64>(),
            "created": stamp(playlist.created), "changed": stamp(playlist.changed)});
        if entries {
            row["entry"] = Value::Array(shown.iter().map(|song| Self::song_json(song)).collect());
        }
        row
    }

    fn native_row(playlist: &NdPlaylist) -> Value {
        json!({"id": playlist.id, "name": playlist.name, "ownerName": playlist.owner,
            "public": playlist.public, "rules": if playlist.smart { json!({"all": [{"is": {"loved": true}}]}) } else { Value::Null },
            "sync": false, "songCount": playlist.tracks.len(), "duration": 0.0,
            "updatedAt": stamp(playlist.changed)})
    }

    fn subsonic(&mut self, endpoint: &str, request: &wiremock::Request) -> Value {
        let user = query_value(request, "u").unwrap_or_default();
        let refused = query_value(request, "t").as_deref() == Some("bad")
            || query_value(request, "p").as_deref() == Some("bad");
        if refused {
            return failed(40, "Wrong username or password");
        }
        let all = |name: &str| -> Vec<String> {
            request
                .url
                .query_pairs()
                .filter(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
                .collect()
        };
        match endpoint {
            "getPlaylists" => {
                let rows: Vec<Value> = self
                    .playlists
                    .iter()
                    .filter(|p| p.owner == user || p.public)
                    .map(|p| self.row(p, &user, false))
                    .collect();
                ok(json!({"playlists": {"playlist": rows}}))
            }
            "getPlaylist" => match query_value(request, "id").and_then(|id| self.find(&id, &user)) {
                Some(at) => ok(json!({"playlist": self.row(&self.playlists[at], &user, true)})),
                None => failed(70, "Playlist not found"),
            },
            "getSong" => match query_value(request, "id").and_then(|id| Self::song(&id)) {
                Some(song) if Self::sees(&user, song) => ok(json!({"song": Self::song_json(song)})),
                _ => failed(70, "Song not found"),
            },
            "updatePlaylist" => {
                let Some(at) = query_value(request, "playlistId").and_then(|id| self.find(&id, &user)) else {
                    return failed(70, "Playlist not found");
                };
                if self.playlists[at].owner != user {
                    return failed(50, "Not the owner");
                }
                let mut removals: Vec<usize> = all("songIndexToRemove")
                    .iter()
                    .filter_map(|i| i.parse().ok())
                    .collect();
                removals.sort_unstable();
                let changed = self.tick();
                let playlist = &mut self.playlists[at];
                for at in removals.into_iter().rev() {
                    if at < playlist.tracks.len() {
                        playlist.tracks.remove(at);
                    }
                }
                for id in all("songIdToAdd") {
                    if Self::song(&id).is_some_and(|song| Self::sees(&user, song)) {
                        playlist.tracks.push(id);
                    }
                }
                if let Some(name) = query_value(request, "name") {
                    playlist.name = name;
                }
                if let Some(comment) = query_value(request, "comment") {
                    playlist.comment = comment;
                }
                if let Some(public) = query_value(request, "public") {
                    playlist.public = public == "true";
                }
                playlist.changed = changed;
                ok(json!({}))
            }
            "createPlaylist" => {
                let tracks: Vec<String> = all("songId")
                    .into_iter()
                    .filter(|id| Self::song(id).is_some_and(|song| Self::sees(&user, song)))
                    .collect();
                let at = match query_value(request, "playlistId") {
                    Some(id) => {
                        let Some(at) = self.find(&id, &user) else {
                            return failed(70, "Playlist not found");
                        };
                        if self.playlists[at].owner != user {
                            return failed(50, "Not the owner");
                        }
                        let changed = self.tick();
                        self.playlists[at].tracks = tracks;
                        self.playlists[at].changed = changed;
                        at
                    }
                    None => {
                        self.made += 1;
                        let id = format!("4new{:018}", self.made);
                        let name = query_value(request, "name").unwrap_or_default();
                        let refs: Vec<&str> = tracks.iter().map(String::as_str).collect();
                        self.add(&id, &name, &user, false, false, &refs);
                        self.playlists.len() - 1
                    }
                };
                ok(json!({"playlist": self.row(&self.playlists[at], &user, true)}))
            }
            "deletePlaylist" => {
                let Some(at) = query_value(request, "id").and_then(|id| self.find(&id, &user)) else {
                    return failed(70, "Playlist not found");
                };
                if self.playlists[at].owner != user {
                    return failed(50, "Not the owner");
                }
                self.playlists.remove(at);
                ok(json!({}))
            }
            _ => ok(json!({})),
        }
    }

    fn native(
        &mut self,
        method: &str,
        path: &str,
        user: &str,
        request: &wiremock::Request,
    ) -> ResponseTemplate {
        if path == "/api/song" {
            let rows: Vec<Value> = if query_value(request, "_start").as_deref() == Some("0") {
                SONGS
                    .iter()
                    .map(|song| {
                        json!({"id": song.id, "albumId": "al-1", "artistId": "ar-1",
                        "albumArtistId": "ar-1", "libraryId": song.library, "libraryPath": "/music",
                        "path": format!("Band/{}.flac", song.id), "explicitStatus": song.explicit,
                        "size": 1000, "title": song.title, "duration": song.seconds as f64 + 0.4,
                        "missing": song.missing})
                    })
                    .collect()
            } else {
                Vec::new()
            };
            return ResponseTemplate::new(200).set_body_json(Value::Array(rows));
        }
        if path == "/api/playlist" {
            let rows: Vec<Value> = self
                .playlists
                .iter()
                .filter(|p| user == "admin" || p.owner == user || p.public)
                .map(Self::native_row)
                .collect();
            let total = rows.len().to_string();
            return ResponseTemplate::new(200)
                .set_body_json(Value::Array(rows))
                .insert_header("X-Total-Count", total.as_str());
        }
        if let Some(id) = path.strip_prefix("/api/song/") {
            return match Self::song(id) {
                Some(song) => ResponseTemplate::new(200).set_body_json(json!({"id": song.id,
                    "title": song.title, "artist": song.artist, "album": "Album",
                    "duration": song.seconds as f64, "libraryId": song.library, "missing": song.missing,
                    "explicitStatus": song.explicit, "tags": {}})),
                None => ResponseTemplate::new(404),
            };
        }
        let Some(rest) = path.strip_prefix("/api/playlist/") else {
            return ResponseTemplate::new(404);
        };
        let parts: Vec<&str> = rest.split('/').collect();
        let Some(at) = self.find(parts[0], user) else {
            return ResponseTemplate::new(404);
        };
        match (method, parts.len()) {
            ("GET", 1) => ResponseTemplate::new(200).set_body_json(Self::native_row(&self.playlists[at])),
            ("DELETE", 1) => {
                self.playlists.remove(at);
                ResponseTemplate::new(200).set_body_json(json!({}))
            }
            ("GET", 2) if user == "admin" && self.fail_track_reads > 0 => {
                self.fail_track_reads -= 1;
                ResponseTemplate::new(500)
            }
            ("GET", 2) => {
                let rows: Vec<Value> = self.playlists[at]
                    .tracks
                    .iter()
                    .enumerate()
                    .filter_map(|(place, id)| Some((place, Self::song(id)?)))
                    .filter(|(_, song)| Self::sees(user, song))
                    .map(|(place, song)| {
                        json!({"id": (place + 1).to_string(), "mediaFileId": song.id,
                        "playlistId": parts[0], "title": song.title, "artist": song.artist,
                        "duration": song.seconds as f64, "missing": song.missing,
                        "explicitStatus": song.explicit})
                    })
                    .collect();
                ResponseTemplate::new(200).set_body_json(Value::Array(rows))
            }
            ("POST", 2) => {
                let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
                let ids: Vec<String> = body["ids"]
                    .as_array()
                    .map(|ids| ids.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
                let added: Vec<String> = ids
                    .into_iter()
                    .filter(|id| Self::song(id).is_some_and(|song| Self::sees(user, song)))
                    .collect();
                let changed = self.tick();
                let count = added.len();
                self.playlists[at].tracks.extend(added);
                self.playlists[at].changed = changed;
                ResponseTemplate::new(200).set_body_json(json!({"added": count}))
            }
            ("DELETE", 2) => {
                let mut places: Vec<usize> = request
                    .url
                    .query_pairs()
                    .filter(|(key, _)| key == "id")
                    .filter_map(|(_, value)| value.parse::<usize>().ok())
                    .collect();
                places.sort_unstable();
                let changed = self.tick();
                for place in places.iter().rev() {
                    if *place >= 1 && *place <= self.playlists[at].tracks.len() {
                        self.playlists[at].tracks.remove(place - 1);
                    }
                }
                self.playlists[at].changed = changed;
                ResponseTemplate::new(200).set_body_json(json!({"ids": places}))
            }
            ("PUT", 3) => {
                let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
                let to = body["insert_before"]
                    .as_str()
                    .and_then(|n| n.parse::<usize>().ok())
                    .unwrap_or(1);
                let from = parts[2].parse::<usize>().unwrap_or(1);
                let changed = self.tick();
                let tracks = &mut self.playlists[at].tracks;
                if from >= 1 && from <= tracks.len() {
                    let moving = tracks.remove(from - 1);
                    let to = to.saturating_sub(1).min(tracks.len());
                    tracks.insert(to, moving);
                }
                self.playlists[at].changed = changed;
                ResponseTemplate::new(200).set_body_json(json!({"id": parts[2]}))
            }
            _ => ResponseTemplate::new(405),
        }
    }
}

/// Navidrome, shared by the fixture and the mock server.
#[derive(Clone)]
pub(super) struct FakeNd(pub Arc<Mutex<Nd>>);

impl FakeNd {
    fn seeded() -> FakeNd {
        let mut nd = Nd {
            playlists: Vec::new(),
            log: Vec::new(),
            fail_track_reads: 0,
            clock: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            made: 0,
        };
        nd.add(ROAD, "Road trip", "winters", false, false, &["s-a", "s-b"]);
        nd.add(ALEX_MIX, "Alex mix", "alex", true, false, &["s-a"]);
        nd.add(SMART, "Loved", "winters", false, true, &["s-a"]);
        nd.add(KID_LIST, "Kid list", "kid", true, false, &["s-a", "s-explicit"]);
        nd.add(PLAIN, "Plain", "winters", false, false, &["s-a"]);
        FakeNd(Arc::new(Mutex::new(nd)))
    }
}

/// The user a native call's JWT names.
fn native_user(request: &wiremock::Request) -> String {
    let header = request
        .headers
        .get("x-nd-authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim_start_matches("Bearer ")
        .to_string();
    if header == "jwt-1" {
        return "admin".into();
    }
    header
        .split('.')
        .nth(1)
        .and_then(|payload| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(payload)
                .ok()
        })
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|claims| claims["username"].as_str().map(str::to_string))
        .unwrap_or_default()
}

impl Respond for FakeNd {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let mut nd = self.0.lock();
        let path = request.url.path().trim_end_matches(".view").to_string();
        if let Some(endpoint) = path.strip_prefix("/rest/") {
            let user = query_value(request, "u").unwrap_or_default();
            nd.log.push(format!("{} {path} {user}", request.method));
            let answer = nd.subsonic(endpoint, request);
            if query_value(request, "f").as_deref() == Some("json") {
                let body = json!({ "subsonic-response": answer }).to_string();
                return ResponseTemplate::new(200).set_body_raw(body, "application/json");
            }
            let mut root = json_shape_to_xml(Some(NS), "subsonic-response", &answer);
            root.set_attr("xmlns", NS);
            let body = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{}",
                root.to_xml_string()
            );
            return ResponseTemplate::new(200).set_body_raw(body, "application/xml");
        }
        let user = native_user(request);
        nd.log.push(format!("{} {path} {user}", request.method));
        let method = request.method.to_string();
        nd.native(&method, &path, &user, request)
    }
}

fn ok(mut fields: Value) -> Value {
    fields["status"] = json!("ok");
    fields["version"] = json!("1.16.1");
    fields
}

fn failed(code: i32, message: &str) -> Value {
    json!({"status": "failed", "version": "1.16.1", "error": {"code": code, "message": message}})
}

fn stamp(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// A native caller's JWT: its payload names the user.
pub(super) fn native_token(user: &str) -> String {
    let payload =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json!({"username": user}).to_string());
    format!("Bearer x.{payload}.y")
}

pub(super) fn auth(user: &str) -> String {
    match user {
        "winters" => "u=winters&t=good&s=salt&v=1.16.1&c=test".to_string(),
        _ => format!("u={user}&p={user}-secret&v=1.16.1&c=test"),
    }
}

pub(super) struct Fixture {
    pub _server: MockServer,
    pub _dir: TempDir,
    pub nd: FakeNd,
    pub state: AppState,
    pub app: App,
}

impl Fixture {
    pub async fn rest(&self, user: &str, call: &str) -> Reply {
        let joiner = if call.contains('?') { '&' } else { '?' };
        get(&self.app, &format!("/rest/{call}{joiner}{}", auth(user))).await
    }

    pub async fn rest_as(&self, query_auth: &str, call: &str) -> Reply {
        let joiner = if call.contains('?') { '&' } else { '?' };
        get(&self.app, &format!("/rest/{call}{joiner}{query_auth}")).await
    }

    pub async fn json(&self, user: &str, call: &str) -> Value {
        let joiner = if call.contains('?') { '&' } else { '?' };
        self.rest(user, &format!("{call}{joiner}f=json")).await.json()["subsonic-response"].clone()
    }

    pub fn tracks(&self, id: &str) -> Vec<String> {
        let nd = self.nd.0.lock();
        nd.playlists
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.tracks.clone())
            .unwrap_or_default()
    }

    /// The held entries' ids (library id, else outside id), or None when the playlist is not held.
    pub fn held(&self, id: &str) -> Option<Vec<String>> {
        self.state.held_playlists.view_of(id).map(|view| {
            view.entries
                .iter()
                .map(|entry| {
                    entry
                        .library_id
                        .clone()
                        .or(entry.outside_id.clone())
                        .unwrap_or_default()
                })
                .collect()
        })
    }

    /// Whether no playlist is held.
    pub fn none_held(&self) -> bool {
        self.state.held_playlists.is_empty()
    }

    /// Admin calls to playlists and songs (the library index's own reads left out).
    pub fn admin_calls(&self) -> usize {
        self.nd
            .0
            .lock()
            .log
            .iter()
            .filter(|line| {
                line.ends_with(" admin") && (line.contains("/api/playlist") || line.contains("/api/song/"))
            })
            .count()
    }
}

/// The ids of a getPlaylist answer's entries.
pub(super) fn served(playlist: &Value) -> Vec<String> {
    playlist["entry"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| e["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub(super) async fn fixture() -> Fixture {
    fixture_with(false, |_| {}).await
}

pub(super) async fn fixture_with(family: bool, change: impl FnOnce(&mut AppSettings)) -> Fixture {
    let server = crate::services::family::navidrome_admin::tests::navidrome().await;
    let nd = FakeNd::seeded();
    Mock::given(any())
        .respond_with(nd.clone())
        .with_priority(10)
        .mount(&server)
        .await;
    let dir = TempDir::new().expect("a temp dir");
    let metadata = TestMetadata::default();
    for (id, title, seconds, explicit) in [
        ("1", "Glory", 200, 0),
        ("2", "Rude Online", 180, 1),
        ("3", "Third", 150, 0),
    ] {
        metadata.songs.lock().insert(
            id.to_string(),
            Song {
                id: format!("ext-deezer-song-{id}"),
                title: title.into(),
                artist: "Somebody".into(),
                album: "Elsewhere".into(),
                duration: Some(seconds),
                isrc: (id == "1").then(|| "X1".to_string()),
                is_local: false,
                external_provider: Some("deezer".into()),
                external_id: Some(id.into()),
                explicit_content_lyrics: Some(explicit),
                ..Default::default()
            },
        );
    }
    let mut settings = AppSettings {
        subsonic: SubsonicSettings {
            url: Some(server.uri()),
            admin_username: Some("admin".into()),
            admin_password: Some("pw".into()),
            auto_detect_download_path: false,
            enable_search_discovery: false,
            ..Default::default()
        },
        generated_playlists: GeneratedPlaylistSettings {
            enabled: false,
            new_releases: false,
            rediscover: false,
            deep_cuts: false,
            popular_now: false,
            charts: String::new(),
            ..Default::default()
        },
        family: FamilySettings {
            enabled: family,
            ..Default::default()
        },
        ..Default::default()
    };
    change(&mut settings);
    let state = AppState::for_tests_with(
        settings,
        TestServices {
            config_dir: Some(dir.path().to_path_buf()),
            metadata: Some(Arc::new(metadata)),
            ..Default::default()
        },
    );
    if family {
        assert!(state.family_vault.create_key_if_missing().expect("a key"));
        let store = &state.family_store;
        store.set_owner("winters");
        for (name, preset, library) in [("alex", Preset::Member, 3), ("kid", Preset::Kid, 5)] {
            let mut person = member(name, state.family_vault.seal("hidden").expect("sealed"));
            person.preset = preset;
            person.personal_library_id = Some(library);
            person.account_password =
                crate::services::family::passwords::seal(&state.family_vault, &format!("{name}-secret"))
                    .expect("sealed");
            store.upsert_member(person);
            store.add_device(Device {
                id: format!("d_{name}"),
                username: name.into(),
                name: String::new(),
                created: Utc::now(),
                last_seen: None,
                app: String::new(),
                place: NetworkPlace::Home,
                seen_apps: Vec::new(),
                navidrome_jwt: None,
                quality: Default::default(),
            });
        }
    }
    state.library_index.refresh().await.expect("the library reads");
    let app = super::app(state.clone());
    Fixture {
        _server: server,
        _dir: dir,
        nd,
        state,
        app,
    }
}

/// ROAD holding OUTSIDE after s-a and s-b.
async fn held_road(f: &Fixture) {
    let added = f
        .json(
            "winters",
            &format!("updatePlaylist?playlistId={ROAD}&songIdToAdd={OUTSIDE}"),
        )
        .await;
    assert_eq!(added["status"], "ok", "{added}");
    assert_eq!(f.held(ROAD).unwrap(), ["s-a", "s-b", OUTSIDE]);
}

#[tokio::test]
async fn t1_an_outside_song_added_to_a_plain_playlist_is_kept_and_served_in_place() {
    let f = fixture().await;
    held_road(&f).await;
    // Navidrome keeps the library songs, in order, under the same id.
    assert_eq!(f.tracks(ROAD), ["s-a", "s-b"]);
    let playlist = f.json("winters", &format!("getPlaylist?id={ROAD}")).await["playlist"].clone();
    assert_eq!(served(&playlist), ["s-a", "s-b", OUTSIDE]);
    assert_eq!(playlist["id"], ROAD);
    assert_eq!(playlist["entry"][2]["isExternal"], true);
    assert_eq!(playlist["entry"][2]["title"], "Glory");
    assert_eq!(playlist["songCount"], 3);
    let xml = f.rest("winters", &format!("getPlaylist?id={ROAD}")).await.body;
    assert_eq!(xml.matches("<entry ").count(), 3, "{xml}");
    assert!(xml.contains(&format!(r#"id="{OUTSIDE}""#)), "{xml}");
}

#[tokio::test]
async fn t2_library_only_changes_stay_plain_relays() {
    let f = fixture().await;
    let added = f
        .json(
            "winters",
            &format!("updatePlaylist?playlistId={ROAD}&songIdToAdd=s-glory&songIndexToRemove=0"),
        )
        .await;
    assert_eq!(added["status"], "ok", "{added}");
    assert_eq!(f.tracks(ROAD), ["s-b", "s-glory"]);
    assert!(f.held(ROAD).is_none());
    assert!(f.none_held());
    let replaced = f
        .json(
            "winters",
            &format!("createPlaylist?playlistId={ROAD}&songId=s-a&songId=s-b"),
        )
        .await;
    assert_eq!(replaced["status"], "ok", "{replaced}");
    assert_eq!(f.tracks(ROAD), ["s-a", "s-b"]);
    assert!(f.none_held());
}

#[tokio::test]
async fn t3_a_replace_with_an_outside_song_converts_and_keeps_the_id_and_every_library_song() {
    let f = fixture().await;
    let replaced = f
        .json(
            "winters",
            &format!("createPlaylist?playlistId={ROAD}&songId=s-a&songId={OUTSIDE}&songId=s-b"),
        )
        .await;
    assert_eq!(replaced["status"], "ok", "{replaced}");
    assert_eq!(served(&replaced["playlist"]), ["s-a", OUTSIDE, "s-b"]);
    assert_eq!(replaced["playlist"]["id"], ROAD);
    assert_eq!(f.tracks(ROAD), ["s-a", "s-b"]);
    assert_eq!(f.held(ROAD).unwrap(), ["s-a", OUTSIDE, "s-b"]);
}

#[tokio::test]
async fn t15_a_reorder_keeps_the_streamed_entry_and_its_snapshot() {
    let f = fixture().await;
    held_road(&f).await;
    let replaced = f
        .json(
            "winters",
            &format!("createPlaylist?playlistId={ROAD}&songId={OUTSIDE}&songId=s-a&songId=s-b"),
        )
        .await;
    assert_eq!(replaced["status"], "ok", "{replaced}");
    assert_eq!(f.held(ROAD).unwrap(), [OUTSIDE, "s-a", "s-b"]);
    assert_eq!(f.tracks(ROAD), ["s-a", "s-b"]);
    assert_eq!(replaced["playlist"]["entry"][0]["title"], "Glory");
}

#[tokio::test]
async fn t15_t47_an_old_octo_phones_title_match_keeps_the_streamed_entry_and_the_same_take_binds() {
    let f = fixture().await;
    held_road(&f).await;
    let old_phone = "u=winters&t=good&s=salt&v=1.16.1&c=Octo";
    // Another take with the same title: the phone's own match, ignored.
    let pushed = f
        .rest_as(
            old_phone,
            &format!("createPlaylist?playlistId={ROAD}&songId=s-a&songId=s-b&songId=s-glory2&f=json"),
        )
        .await
        .json();
    assert_eq!(pushed["subsonic-response"]["status"], "ok", "{pushed}");
    assert_eq!(f.held(ROAD).unwrap(), ["s-a", "s-b", OUTSIDE]);
    assert_eq!(f.tracks(ROAD), ["s-a", "s-b"]);
    // The same take: bound, and with nothing left streaming the playlist is Navidrome's alone.
    let pushed = f
        .rest_as(
            old_phone,
            &format!("createPlaylist?playlistId={ROAD}&songId=s-a&songId=s-b&songId=s-glory&f=json"),
        )
        .await
        .json();
    assert_eq!(pushed["subsonic-response"]["status"], "ok", "{pushed}");
    assert!(f.held(ROAD).is_none());
    assert_eq!(f.tracks(ROAD), ["s-a", "s-b", "s-glory"]);
}

#[tokio::test]
async fn t15_t47_a_genuine_swap_from_any_other_client_is_kept() {
    let f = fixture().await;
    held_road(&f).await;
    // A third-party client takes the song out and puts "Glory (Live)" in: both changes stand.
    let third_party = "u=winters&t=good&s=salt&v=1.16.1&c=Symfonium";
    let pushed = f
        .rest_as(
            third_party,
            &format!("createPlaylist?playlistId={ROAD}&songId=s-a&songId=s-b&songId=s-glory-live&f=json"),
        )
        .await
        .json();
    assert_eq!(pushed["subsonic-response"]["status"], "ok", "{pushed}");
    assert!(f.held(ROAD).is_none());
    assert_eq!(f.tracks(ROAD), ["s-a", "s-b", "s-glory-live"]);

    // From an old Octo phone an unrelated song in the gap is a real add, and the streamed one a
    // real removal. The playlist goes back to its two library songs first.
    let reset = f
        .rest_as(
            third_party,
            &format!("createPlaylist?playlistId={ROAD}&songId=s-a&songId=s-b&f=json"),
        )
        .await
        .json();
    assert_eq!(reset["subsonic-response"]["status"], "ok", "{reset}");
    assert_eq!(f.tracks(ROAD), ["s-a", "s-b"]);
    held_road(&f).await;
    let pushed = f
        .rest_as(
            "u=winters&t=good&s=salt&v=1.16.1&c=Octo",
            &format!("createPlaylist?playlistId={ROAD}&songId=s-a&songId=s-b&songId=s-explicit&f=json"),
        )
        .await
        .json();
    assert_eq!(pushed["subsonic-response"]["status"], "ok", "{pushed}");
    assert!(f.held(ROAD).is_none());
    assert_eq!(f.tracks(ROAD), ["s-a", "s-b", "s-explicit"]);
}

#[tokio::test]
async fn t50_a_replace_with_own_ids_is_never_paired_and_keeps_no_record() {
    let f = fixture().await;
    held_road(&f).await;
    let pushed = f
        .rest_as(
            "u=winters&t=good&s=salt&v=1.16.1&c=Octo&octoOwnIds=1",
            &format!("createPlaylist?playlistId={ROAD}&songId=s-a&songId=s-b&songId=s-glory&f=json"),
        )
        .await
        .json();
    assert_eq!(pushed["subsonic-response"]["status"], "ok", "{pushed}");
    assert!(f.held(ROAD).is_none());
    assert_eq!(f.tracks(ROAD), ["s-a", "s-b", "s-glory"]);
    assert!(f.state.held_playlists.record_ids(ROAD).is_empty());
}

#[tokio::test]
async fn t16_a_split_replace_takes_its_removed_entries_back_after_an_eviction() {
    let f = fixture().await;
    let kept = f.state.external_id_registry.register(SoulseekRouting {
        kind: RoutingKind::Song,
        artist: Some("Somebody".into()),
        title: Some("Kept".into()),
        duration: Some(190),
        ..Default::default()
    });
    held_road(&f).await;
    let added = f
        .json(
            "winters",
            &format!("updatePlaylist?playlistId={ROAD}&songIdToAdd={kept}"),
        )
        .await;
    assert_eq!(added["status"], "ok", "{added}");
    assert_eq!(f.held(ROAD).unwrap(), ["s-a", "s-b", OUTSIDE, kept.as_str()]);

    // Batch 1 holds the first song only: the playlist releases at once, and the record keeps the
    // outside entries it took out.
    let first = f
        .json("winters", &format!("createPlaylist?playlistId={ROAD}&songId=s-a"))
        .await;
    assert_eq!(first["status"], "ok", "{first}");
    assert!(f.held(ROAD).is_none());
    assert_eq!(f.tracks(ROAD), ["s-a"]);

    // Ten thousand newer songs, and another device's read, between the batches.
    for n in 0..10_001 {
        f.state.external_id_registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("Filler".into()),
            title: Some(format!("Filler {n}")),
            ..Default::default()
        });
    }
    let other_device = f
        .rest_as(
            "u=winters&t=good&s=salt&v=1.16.1&c=Other",
            &format!("getPlaylist?id={ROAD}&f=json"),
        )
        .await
        .json();
    assert_eq!(served(&other_device["subsonic-response"]["playlist"]), ["s-a"]);

    // Batch 2 adds the rest back: the removed entries come back with their snapshots.
    let rest = f
        .json(
            "winters",
            &format!(
                "updatePlaylist?playlistId={ROAD}&songIdToAdd=s-b&songIdToAdd={OUTSIDE}&songIdToAdd={kept}"
            ),
        )
        .await;
    assert_eq!(rest["status"], "ok", "{rest}");
    assert!(rest.get("octoSkipped").is_none(), "{rest}");
    assert_eq!(f.held(ROAD).unwrap(), ["s-a", "s-b", OUTSIDE, kept.as_str()]);
    let playlist = f.json("winters", &format!("getPlaylist?id={ROAD}")).await["playlist"].clone();
    assert_eq!(playlist["entry"][3]["title"], "Kept");
}

#[tokio::test]
async fn t16_a_library_add_from_another_app_clears_a_released_playlists_record() {
    let f = fixture().await;
    held_road(&f).await;
    // A replace without octoOwnIds takes the song found online out: the playlist is released and
    // its record keeps the entry.
    let first = f
        .json("winters", &format!("createPlaylist?playlistId={ROAD}&songId=s-a"))
        .await;
    assert_eq!(first["status"], "ok", "{first}");
    assert!(f.held(ROAD).is_none());
    assert!(f.state.held_playlists.record_ids(ROAD).contains(OUTSIDE));
    // Another app adds a library song: Octo has a part in it (the record), so the change runs under
    // the lock, is relayed, and clears the record.
    let other = f
        .rest_as(
            "u=winters&t=good&s=salt&v=1.16.1&c=Other",
            &format!("updatePlaylist?playlistId={ROAD}&songIdToAdd=s-b&f=json"),
        )
        .await
        .json();
    assert_eq!(other["subsonic-response"]["status"], "ok", "{other}");
    assert_eq!(f.tracks(ROAD), ["s-a", "s-b"]);
    assert!(f.held(ROAD).is_none());
    assert!(f.state.held_playlists.record_ids(ROAD).is_empty());
}

#[tokio::test]
async fn t17_t46_a_stale_stamp_answers_the_conflict_and_changes_nothing_and_a_fresh_one_goes_through() {
    let f = fixture().await;
    held_road(&f).await;
    let stale = f
        .json(
            "winters",
            &format!("createPlaylist?playlistId={ROAD}&songId=s-b&octoIfStamp=2000-01-01T00:00:00.000Z"),
        )
        .await;
    assert_eq!(stale["status"], "failed");
    assert_eq!(stale["error"]["code"], 0);
    assert_eq!(stale["error"]["message"], "The playlist changed. Read it again.");
    assert_eq!(stale["error"]["octoConflict"], true);
    assert_eq!(f.held(ROAD).unwrap(), ["s-a", "s-b", OUTSIDE]);
    assert_eq!(f.tracks(ROAD), ["s-a", "s-b"]);

    // The served stamp, read now: the replace's own sync (seed refresh included) leaves it alone.
    let changed = f.json("winters", &format!("getPlaylist?id={ROAD}")).await["playlist"]["changed"]
        .as_str()
        .unwrap()
        .to_string();
    let fresh = f
        .json(
            "winters",
            &format!("createPlaylist?playlistId={ROAD}&songId={OUTSIDE}&songId=s-a&songId=s-b&octoIfStamp={changed}"),
        )
        .await;
    assert_eq!(fresh["status"], "ok", "{fresh}");
    assert_eq!(f.held(ROAD).unwrap(), [OUTSIDE, "s-a", "s-b"]);

    // A plain playlist checks the stamp too.
    let plain = f
        .json(
            "winters",
            &format!("createPlaylist?playlistId={PLAIN}&songId=s-b&octoIfStamp=2000-01-01T00:00:00.000Z"),
        )
        .await;
    assert_eq!(plain["error"]["octoConflict"], true);
    assert_eq!(f.tracks(PLAIN), ["s-a"]);
    let changed = f.json("winters", &format!("getPlaylist?id={PLAIN}")).await["playlist"]["changed"]
        .as_str()
        .unwrap()
        .to_string();
    let plain = f
        .json(
            "winters",
            &format!("createPlaylist?playlistId={PLAIN}&songId=s-b&octoIfStamp={changed}"),
        )
        .await;
    assert_eq!(plain["status"], "ok", "{plain}");
    assert_eq!(f.tracks(PLAIN), ["s-b"]);
}

#[tokio::test]
async fn t18_a_new_playlist_with_an_outside_song_is_held_from_the_start() {
    let f = fixture().await;
    let made = f
        .json(
            "winters",
            &format!("createPlaylist?name=Fresh&songId=s-a&songId={OUTSIDE}"),
        )
        .await;
    assert_eq!(made["status"], "ok", "{made}");
    let id = made["playlist"]["id"].as_str().unwrap().to_string();
    assert_eq!(served(&made["playlist"]), ["s-a", OUTSIDE]);
    assert_eq!(f.tracks(&id), ["s-a"]);
    assert_eq!(f.held(&id).unwrap(), ["s-a", OUTSIDE]);
    assert_eq!(made["playlist"]["name"], "Fresh");
}

#[tokio::test]
async fn t18_creations_cut_short_attach_at_the_next_listing_each_to_its_own_playlist() {
    let f = fixture().await;
    // The holds of the next two creations can't read Navidrome's tracks, so each intent stays.
    f.nd.0.lock().fail_track_reads = 2;
    for _ in 0..2 {
        let cut = f
            .json(
                "winters",
                &format!("createPlaylist?name=Twin&songId=s-a&songId={OUTSIDE}"),
            )
            .await;
        assert_eq!(cut["status"], "failed", "{cut}");
    }
    let twins: Vec<String> =
        f.nd.0
            .lock()
            .playlists
            .iter()
            .filter(|p| p.name == "Twin")
            .map(|p| p.id.clone())
            .collect();
    assert_eq!(twins.len(), 2);
    assert!(twins.iter().all(|id| f.held(id).is_none()));
    assert_eq!(f.json("winters", "getPlaylists").await["status"], "ok");
    until("both twins attach", || {
        twins.iter().all(|id| f.held(id).is_some())
    })
    .await;
    assert_eq!(f.held(&twins[0]).unwrap(), ["s-a", OUTSIDE]);
    assert_eq!(f.held(&twins[1]).unwrap(), ["s-a", OUTSIDE]);
}

#[tokio::test]
async fn t19_only_the_owner_changes_a_playlist_and_a_wrong_sign_in_makes_no_admin_call() {
    let f = fixture().await;
    let refused = f
        .json(
            "winters",
            &format!("updatePlaylist?playlistId={ALEX_MIX}&songIdToAdd={OUTSIDE}"),
        )
        .await;
    assert_eq!(refused["error"]["code"], 50);
    assert_eq!(
        refused["error"]["message"],
        "Only the playlist's owner can change it."
    );
    assert!(f.held(ALEX_MIX).is_none());

    let before = f.admin_calls();
    let wrong = f
        .rest_as(
            "u=winters&t=bad&s=salt&v=1.16.1&c=test",
            &format!("updatePlaylist?playlistId={ROAD}&songIdToAdd={OUTSIDE}&f=json"),
        )
        .await
        .json();
    assert_eq!(wrong["subsonic-response"]["error"]["code"], 40);
    assert_eq!(f.admin_calls(), before);
    assert!(f.held(ROAD).is_none());
}

#[tokio::test]
async fn t19_t44_smart_and_upkeep_playlists_refuse_outside_songs_before_any_conversion() {
    let f = fixture_with(false, |settings| {
        settings.library_actions = LibraryActionSettings {
            enabled: true,
            playlists_enabled: true,
            review_enabled: true,
            allowed_users: vec!["winters".into()],
            ..Default::default()
        };
    })
    .await;
    let smart = f
        .json(
            "winters",
            &format!("updatePlaylist?playlistId={SMART}&songIdToAdd={OUTSIDE}"),
        )
        .await;
    assert_eq!(smart["error"]["code"], 50);
    assert_eq!(
        smart["error"]["message"],
        "This playlist is made by Navidrome's rules, so songs can't be added to it."
    );

    let action =
        LibraryActionPlaylistProvisioner::new(f.state.subsonic_proxy.clone(), f.state.settings.clone())
            .upkeep_titles("winters")
            .actions
            .first()
            .cloned()
            .expect("an action playlist");
    f.nd.0.lock().add(
        "aupkeepaaaaaaaaaaaaaaa",
        &action,
        "winters",
        false,
        false,
        &["s-a"],
    );
    for call in [
        "updatePlaylist?playlistId=aupkeepaaaaaaaaaaaaaaa&songIdToAdd=ext-deezer-song-1".to_string(),
        "createPlaylist?playlistId=aupkeepaaaaaaaaaaaaaaa&songId=s-a&songId=ext-deezer-song-1".to_string(),
    ] {
        let refused = f.json("winters", &call).await;
        assert_eq!(refused["error"]["code"], 0, "{refused}");
        assert_eq!(
            refused["error"]["message"],
            "Songs found online can't go in Octo's library upkeep playlists."
        );
    }
    assert!(f.none_held());
    assert_eq!(f.tracks("aupkeepaaaaaaaaaaaaaaa"), ["s-a"]);

    // Octo's own ids stay refused as before.
    let station = f
        .json(
            "winters",
            "updatePlaylist?playlistId=orstationaaaaaaaaaaaaa&songIdToAdd=s-a",
        )
        .await;
    assert_eq!(station["error"]["code"], 70);
}

#[tokio::test]
async fn t20_album_ids_are_refused_and_unknown_ids_skipped_and_listed() {
    let f = fixture().await;
    let album = f
        .json(
            "winters",
            &format!("updatePlaylist?playlistId={ROAD}&songIdToAdd={OUTSIDE_ALBUM}"),
        )
        .await;
    assert_eq!(album["error"]["code"], 10);
    assert_eq!(album["error"]["message"], "Only songs can go in a playlist.");
    assert!(f.held(ROAD).is_none());

    let skipped = f
        .json(
            "winters",
            &format!("updatePlaylist?playlistId={ROAD}&songIdToAdd=s-nobody&songIdToAdd={OUTSIDE}"),
        )
        .await;
    assert_eq!(skipped["status"], "ok", "{skipped}");
    assert_eq!(skipped["octoSkipped"], json!(["s-nobody"]));
    assert_eq!(f.held(ROAD).unwrap(), ["s-a", "s-b", OUTSIDE]);
}

#[tokio::test]
async fn t21_removing_the_last_streamed_song_releases_and_emptying_releases() {
    let f = fixture().await;
    held_road(&f).await;
    let removed = f
        .json(
            "winters",
            &format!("updatePlaylist?playlistId={ROAD}&octoSongToRemove={OUTSIDE}@1"),
        )
        .await;
    assert_eq!(removed["status"], "ok", "{removed}");
    assert!(f.held(ROAD).is_none());
    assert_eq!(f.tracks(ROAD), ["s-a", "s-b"]);

    held_road(&f).await;
    let emptied = f
        .json(
            "winters",
            &format!(
                "updatePlaylist?playlistId={ROAD}&songIndexToRemove=0&songIndexToRemove=1&songIndexToRemove=2"
            ),
        )
        .await;
    assert_eq!(emptied["status"], "ok", "{emptied}");
    assert!(f.held(ROAD).is_none());
    assert!(f.tracks(ROAD).is_empty());
}

#[tokio::test]
async fn t22_delete_drops_the_held_file_and_the_placement() {
    let f = fixture().await;
    held_road(&f).await;
    let folder = "pfAAAAAAAAAAAAAAAAAAA1";
    assert_eq!(
        f.json("winters", &format!("savePlaylistFolder?id={folder}&name=Trips"))
            .await["status"],
        "ok"
    );
    let placed = f
        .json(
            "winters",
            &format!("setPlaylistFolder?playlistId={ROAD}&folderId={folder}"),
        )
        .await;
    assert_eq!(placed["playlistFolders"]["place"][0]["playlist"], ROAD);
    let deleted = f.json("winters", &format!("deletePlaylist?id={ROAD}")).await;
    assert_eq!(deleted["status"], "ok", "{deleted}");
    assert!(f.held(ROAD).is_none());
    assert!(f.nd.0.lock().playlists.iter().all(|p| p.id != ROAD));
    let folders = f.json("winters", "getPlaylistFolders").await;
    assert!(
        folders["playlistFolders"]["place"]
            .as_array()
            .is_none_or(|places| places.is_empty()),
        "{folders}"
    );
}

#[tokio::test]
async fn t23_name_comment_and_public_are_relayed_as_the_caller() {
    let f = fixture().await;
    held_road(&f).await;
    let changed = f
        .json(
            "winters",
            &format!("updatePlaylist?playlistId={ROAD}&name=Coast&comment=Summer&public=true&songIdToAdd={OUTSIDE_THIRD}"),
        )
        .await;
    assert_eq!(changed["status"], "ok", "{changed}");
    {
        let nd = f.nd.0.lock();
        let road = nd.playlists.iter().find(|p| p.id == ROAD).unwrap();
        assert_eq!(
            (road.name.as_str(), road.comment.as_str(), road.public),
            ("Coast", "Summer", true)
        );
        assert!(
            nd.log
                .iter()
                .any(|line| line == "GET /rest/updatePlaylist winters")
        );
    }
    assert_eq!(f.held(ROAD).unwrap(), ["s-a", "s-b", OUTSIDE, OUTSIDE_THIRD]);
}

#[tokio::test]
async fn t25_the_row_pass_counts_held_songs_and_marks_upkeep_and_notice_rows_in_json_and_xml() {
    let f = fixture_with(false, |settings| {
        settings.library_actions = LibraryActionSettings {
            enabled: true,
            playlists_enabled: true,
            review_enabled: true,
            allowed_users: vec!["winters".into()],
            ..Default::default()
        };
    })
    .await;
    held_road(&f).await;
    let review = f
        .state
        .settings
        .current()
        .library_actions
        .notice_title(NoticeKind::Review);
    let action =
        LibraryActionPlaylistProvisioner::new(f.state.subsonic_proxy.clone(), f.state.settings.clone())
            .upkeep_titles("winters")
            .actions
            .first()
            .cloned()
            .expect("an action title");
    {
        let mut nd = f.nd.0.lock();
        nd.add(
            "brevieweaaaaaaaaaaaaaa",
            &review,
            "winters",
            false,
            false,
            &["s-a"],
        );
        nd.add("cactionaaaaaaaaaaaaaaa", &action, "winters", false, false, &[]);
    }
    let listing = f.json("winters", "getPlaylists").await;
    let rows = listing["playlists"]["playlist"].as_array().unwrap().clone();
    let row = |id: &str| rows.iter().find(|row| row["id"] == id).cloned().unwrap();
    assert_eq!(row(ROAD)["songCount"], 3);
    assert_eq!(row(ROAD)["duration"], 100 + 120 + 200);
    assert!(row(ROAD).get("octoUpkeep").is_none());
    assert_eq!(row("brevieweaaaaaaaaaaaaaa")["octoNotice"], "review");
    assert_eq!(row("brevieweaaaaaaaaaaaaaa")["octoUpkeep"], true);
    assert_eq!(row("brevieweaaaaaaaaaaaaaa")["coverArt"], "oc-notice-review");
    assert_eq!(row("cactionaaaaaaaaaaaaaaa")["octoUpkeep"], true);
    assert_eq!(row("brevieweaaaaaaaaaaaaaa")["octoUpkeepKind"], "notice");
    assert_eq!(row("cactionaaaaaaaaaaaaaaa")["octoUpkeepKind"], "action");
    assert!(row(ROAD).get("octoUpkeepKind").is_none());

    let xml = f.rest("winters", "getPlaylists").await.body;
    let road = xml
        .split("<playlist ")
        .find(|row| row.contains(ROAD))
        .unwrap()
        .to_string();
    assert!(road.contains(r#"songCount="3""#), "{road}");
    let upkeep = xml
        .split("<playlist ")
        .find(|row| row.contains("cactionaaaaaaaaaaaaaaa"))
        .unwrap()
        .to_string();
    assert!(upkeep.contains(r#"octoUpkeep="true""#), "{upkeep}");
    assert!(upkeep.contains(r#"octoUpkeepKind="action""#), "{upkeep}");

    // getPlaylist rows carry the same marks.
    let one = f.json("winters", "getPlaylist?id=cactionaaaaaaaaaaaaaaa").await;
    assert_eq!(one["playlist"]["octoUpkeep"], true);
    assert_eq!(one["playlist"]["octoUpkeepKind"], "action");
}

#[tokio::test]
async fn t43_a_song_removal_on_a_plain_playlist_counts_present_rows_past_a_missing_file() {
    let f = fixture().await;
    {
        let mut nd = f.nd.0.lock();
        let road = nd.playlists.iter_mut().find(|p| p.id == ROAD).unwrap();
        road.tracks = vec!["s-missing".into(), "s-a".into(), "s-b".into(), "s-a".into()];
    }
    let removed = f
        .json(
            "winters",
            &format!("updatePlaylist?playlistId={ROAD}&octoSongToRemove=s-a@2"),
        )
        .await;
    assert_eq!(removed["status"], "ok", "{removed}");
    assert_eq!(f.tracks(ROAD), ["s-missing", "s-a", "s-b"]);
    assert!(f.held(ROAD).is_none());
}

#[tokio::test]
async fn t38_the_sync_catalog_lists_held_songs_and_keep_ready_offers_held_playlists() {
    let f = fixture().await;
    held_road(&f).await;
    let request = axum::extract::Request::builder()
        .uri(format!("/rest/search3?{}&f=json&query=", auth("winters")))
        .body(Body::empty())
        .expect("a request");
    let Ok(call) = SubsonicCall::read(&f.state, request).await else {
        panic!("the request reads");
    };
    let catalog = crate::controllers::subsonic::held::sync_catalog_songs(&f.state, &call, "winters").await;
    // "Glory" is in the library in another take; the catalog still lists the playlist's song.
    assert_eq!(
        catalog
            .songs
            .iter()
            .map(|song| song.id.as_str())
            .collect::<Vec<_>>(),
        [OUTSIDE]
    );
    assert!(!catalog.fingerprint.is_empty());

    let view = f.state.keep_ready.view("winters").await;
    let road = view.lists.iter().find(|list| list.id == ROAD).expect("offered");
    assert_eq!(road.kind, "playlist");
    assert_eq!(road.outside, 1);
}

#[tokio::test]
async fn t49_the_caps_refuse_an_add_past_either_bound_with_their_own_words() {
    let f = fixture().await;
    // The caps count songs the registry keeps; catalog songs (ext- ids) cost it nothing.
    let registered = |title: &str| {
        f.state.external_id_registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("Somebody".into()),
            title: Some(title.into()),
            duration: Some(190),
            ..Default::default()
        })
    };
    let (first, second) = (registered("First"), registered("Second"));
    f.state.held_playlists.set_caps_for_tests(1, 30_000);
    let added = f
        .json(
            "winters",
            &format!("updatePlaylist?playlistId={ROAD}&songIdToAdd={first}"),
        )
        .await;
    assert_eq!(added["status"], "ok", "{added}");
    let refused = f
        .json(
            "winters",
            &format!("updatePlaylist?playlistId={ROAD}&songIdToAdd={second}"),
        )
        .await;
    assert_eq!(refused["error"]["code"], 0, "{refused}");
    assert_eq!(
        refused["error"]["message"],
        "Your playlists hold as many songs found online as Octo keeps."
    );
    assert_eq!(f.held(ROAD).unwrap(), ["s-a", "s-b", first.as_str()]);
    // A catalog song is never counted, so it goes in at the cap.
    let catalog = f
        .json(
            "winters",
            &format!("updatePlaylist?playlistId={ROAD}&songIdToAdd={OUTSIDE}"),
        )
        .await;
    assert_eq!(catalog["status"], "ok", "{catalog}");
    assert_eq!(f.held(ROAD).unwrap(), ["s-a", "s-b", first.as_str(), OUTSIDE]);
    // Downloaded songs never count: a library add goes through.
    let library = f
        .json(
            "winters",
            &format!("updatePlaylist?playlistId={ROAD}&songIdToAdd=s-glory"),
        )
        .await;
    assert_eq!(library["status"], "ok", "{library}");

    f.state.held_playlists.set_caps_for_tests(10, 1);
    let refused = f
        .json(
            "winters",
            &format!("updatePlaylist?playlistId={PLAIN}&songIdToAdd={second}"),
        )
        .await;
    assert_eq!(
        refused["error"]["message"],
        "This server holds as many songs found online in playlists as Octo keeps."
    );
    assert!(f.held(PLAIN).is_none());

    // Over the cap, a replace that takes songs out goes through.
    f.state.held_playlists.set_caps_for_tests(0, 0);
    let lowered = f
        .json(
            "winters",
            &format!("createPlaylist?playlistId={ROAD}&songId=s-a&octoOwnIds=1"),
        )
        .await;
    assert_eq!(lowered["status"], "ok", "{lowered}");
    assert!(f.held(ROAD).is_none());
}

#[tokio::test]
async fn t14_t25_a_kid_removes_by_the_places_it_was_shown_and_counts_only_clean_songs() {
    // The owner's listing shows members' playlists only with OwnerSeesMemberPlaylists on.
    let f = fixture_with(true, |settings| {
        settings.family.owner_sees_member_playlists = true
    })
    .await;
    let added = f
        .json(
            "kid",
            &format!(
                "updatePlaylist?playlistId={KID_LIST}&songIdToAdd={OUTSIDE}&songIdToAdd={OUTSIDE_EXPLICIT}"
            ),
        )
        .await;
    assert_eq!(added["status"], "ok", "{added}");
    assert_eq!(
        f.held(KID_LIST).unwrap(),
        ["s-a", "s-explicit", OUTSIDE, OUTSIDE_EXPLICIT]
    );

    // The kid is shown s-a and OUTSIDE; the owner sees all four.
    let kid_row = f.json("kid", "getPlaylists").await["playlists"]["playlist"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == KID_LIST)
        .cloned()
        .unwrap();
    assert_eq!(kid_row["songCount"], 2 + 1);
    let owner_row = f.json("winters", "getPlaylists").await["playlists"]["playlist"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == KID_LIST)
        .cloned()
        .unwrap();
    assert_eq!(owner_row["songCount"], 2 + 2);

    // Place 1 is OUTSIDE for the kid, never the explicit library song.
    let removed = f
        .json(
            "kid",
            &format!("updatePlaylist?playlistId={KID_LIST}&songIndexToRemove=1"),
        )
        .await;
    assert_eq!(removed["status"], "ok", "{removed}");
    assert_eq!(f.held(KID_LIST).unwrap(), ["s-a", "s-explicit", OUTSIDE_EXPLICIT]);

    let both = f
        .json(
            "kid",
            &format!("updatePlaylist?playlistId={KID_LIST}&songIndexToRemove=0&octoSongToRemove=s-a@1"),
        )
        .await;
    assert_eq!(both["error"]["code"], 10);
    assert_eq!(f.held(KID_LIST).unwrap(), ["s-a", "s-explicit", OUTSIDE_EXPLICIT]);
}

#[tokio::test]
async fn t42_another_members_library_song_is_skipped_on_add() {
    let f = fixture_with(true, |_| {}).await;
    let added = f
        .json(
            "winters",
            &format!("updatePlaylist?playlistId={ROAD}&songIdToAdd=s-alex&songIdToAdd={OUTSIDE}"),
        )
        .await;
    assert_eq!(added["status"], "ok", "{added}");
    assert_eq!(added["octoSkipped"], json!(["s-alex"]));
    assert_eq!(f.held(ROAD).unwrap(), ["s-a", "s-b", OUTSIDE]);
    assert_eq!(f.tracks(ROAD), ["s-a", "s-b"]);
}

#[tokio::test]
async fn t19_a_members_request_a_copy_playlist_still_turns_adds_into_requests() {
    let f = fixture_with(true, |_| {}).await;
    let request_playlist = crate::services::library::library_action_playlist_provisioner::REQUEST_PLAYLIST;
    f.nd.0.lock().add(
        "drequestaaaaaaaaaaaaaa",
        request_playlist,
        "alex",
        false,
        false,
        &[],
    );
    let asked = f
        .json(
            "alex",
            &format!("updatePlaylist?playlistId=drequestaaaaaaaaaaaaaa&songIdToAdd={OUTSIDE}"),
        )
        .await;
    assert_eq!(asked["status"], "ok", "{asked}");
    assert!(f.held("drequestaaaaaaaaaaaaaa").is_none());
    assert!(f.tracks("drequestaaaaaaaaaaaaaa").is_empty());
}
