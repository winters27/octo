//! Every endpoint a family member can reach, in a class. The family
//! gate denies by default for capped and kid members: a `/rest` endpoint this file does not
//! know is refused for them, so a future audio endpoint cannot hand out original files.
//!
//! Covered: Subsonic 1.16.1 and every OpenSubsonic extension endpoint (the list in
//! `docs/family/opensubsonic-endpoints.txt`, checked by a test), Octo's own `/rest`
//! extensions, Navidrome's native API (`/api/*`), its sign-in (`/auth/*`), public shares
//! (`/share/*`) and its Jellyfin API (`/jellyfin`).

use http::Method;

/// What an endpoint does, for the family rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EndpointClass {
    /// Reads, and changes to the caller's own listening state (plays, ratings, stars of
    /// library songs, bookmarks, play queue, own playlists). Allowed for everyone.
    Read,
    /// Hands out audio: streams, downloads, transcodes, share links, radio. Goes through the
    /// audio gate (away, kid, caps, downloads and shares abilities).
    Audio,
    /// Adds to, changes or removes from the library: hearts on outside songs, library
    /// actions, picks, imports, lyrics pins. Goes through the library write gate.
    LibraryWrite,
    /// Changes accounts: users and passwords. Members change theirs in Octo.
    AccountWrite,
    /// Server administration. Members are refused (Navidrome refuses them as well).
    Admin,
    /// Never for a managed member (Jellyfin API, Quick Connect, unknown writes).
    Refused,
}

use EndpointClass::*;

/// Every `/rest` endpoint Octo knows, by name, with its class. Subsonic and OpenSubsonic
/// first (in the index's order), then Octo's own extensions.
pub const REST_ENDPOINTS: &[(&str, EndpointClass)] = &[
    // Subsonic 1.16.1 and OpenSubsonic (opensubsonic.netlify.app/docs/endpoints, 2026-10-08)
    ("addChatMessage", Read),
    ("changePassword", AccountWrite),
    ("createBookmark", Read),
    ("createInternetRadioStation", Admin),
    ("createPlaylist", Read),
    ("createPodcastChannel", Admin),
    ("createShare", Audio),
    ("createUser", AccountWrite),
    ("deleteBookmark", Read),
    ("deleteInternetRadioStation", Admin),
    ("deletePlaylist", Read),
    ("deletePodcastChannel", Admin),
    ("deletePodcastEpisode", Admin),
    ("deleteShare", Read),
    ("deleteUser", AccountWrite),
    ("download", Audio),
    ("downloadPodcastEpisode", Admin),
    ("findSonicPath", Read),
    ("getAlbum", Read),
    ("getAlbumInfo", Read),
    ("getAlbumInfo2", Read),
    ("getAlbumList", Read),
    ("getAlbumList2", Read),
    ("getArtist", Read),
    ("getArtistInfo", Read),
    ("getArtistInfo2", Read),
    ("getArtists", Read),
    ("getAvatar", Read),
    ("getBookmarks", Read),
    ("getCaptions", Read),
    ("getChatMessages", Read),
    ("getCoverArt", Read),
    ("getGenres", Read),
    ("getIndexes", Read),
    ("getInternetRadioStations", Read),
    ("getLicense", Read),
    ("getLyrics", Read),
    ("getLyricsBySongId", Read),
    ("getMusicDirectory", Read),
    ("getMusicFolders", Read),
    ("getNewestPodcasts", Read),
    ("getNowPlaying", Read),
    ("getOpenSubsonicExtensions", Read),
    ("getPlaylist", Read),
    ("getPlaylists", Read),
    ("getPlayQueue", Read),
    ("getPlayQueueByIndex", Read),
    ("getPodcastEpisode", Read),
    ("getPodcasts", Read),
    ("getRandomSongs", Read),
    ("getScanStatus", Read),
    ("getShares", Read),
    ("getSimilarSongs", Read),
    ("getSimilarSongs2", Read),
    ("getSong", Read),
    ("getSongsByGenre", Read),
    ("getSonicSimilarTracks", Read),
    ("getStarred", Read),
    ("getStarred2", Read),
    ("getTopSongs", Read),
    ("getTranscodeDecision", Audio),
    ("getTranscodeStream", Audio),
    ("getUser", Read),
    ("getUsers", Admin),
    ("getVideoInfo", Read),
    ("getVideos", Read),
    ("hls", Audio),
    ("jukeboxControl", Admin),
    ("ping", Read),
    ("refreshPodcasts", Admin),
    ("reportPlayback", Read),
    ("savePlayQueue", Read),
    ("savePlayQueueByIndex", Read),
    ("scrobble", Read),
    ("search", Read),
    ("search2", Read),
    ("search3", Read),
    ("setRating", Read),
    ("star", LibraryWrite),
    ("startScan", Admin),
    ("stream", Audio),
    ("tokenInfo", Read),
    ("unstar", Read),
    ("updateInternetRadioStation", Admin),
    ("updatePlaylist", Read),
    ("updateShare", Audio),
    ("updateUser", AccountWrite),
    // Octo's own extensions
    ("clearAcquisitions", Admin),
    ("findSongs", LibraryWrite),
    ("getAcquisition", Read),
    ("getAcquisitions", Read),
    ("getArtistTopSongs", Read),
    ("getCharts", Read),
    ("getFoundSongs", Read),
    ("getImport", Read),
    ("getImportServices", Read),
    ("getImports", Read),
    ("getLibraryActions", Read),
    ("getLibraryTrash", Read),
    ("getLyricsCandidates", Read),
    ("getStationSongs", Read),
    ("getPlace", Read),
    ("getAnimatedArt", Read),
    ("getTopChart", Read),
    ("getTransitionProfile", Read),
    ("getUpgrades", Read),
    ("importAction", LibraryWrite),
    ("importFile", LibraryWrite),
    ("importText", LibraryWrite),
    ("keepAcquisitionAnyway", LibraryWrite),
    ("libraryAction", LibraryWrite),
    ("pickFoundSong", LibraryWrite),
    // A listener's own chart countries.
    ("pinChartCountry", Read),
    ("setChartCountry", Read),
    ("setLyricsChoice", LibraryWrite),
    ("testPing", Read),
    // Octo Family (`octoFamily`): each checks the caller's own rights.
    ("getFamily", Read),
    ("setFamilyQuality", Read),
    ("setFamilyDeviceQuality", Read),
    ("getFamilyRequests", Read),
    ("requestCopy", Read),
    ("cancelFamilyRequest", Read),
    ("decideFamilyRequest", Read),
    ("getFamilyDevices", Read),
    ("forgetFamilyDevice", Read),
    ("getFamilyLogin", Read),
    ("changeFamilyPassword", Read),
    ("removeFromMyLibrary", Read),
    ("createFamilyPlaylist", Read),
    ("startFamilySignIn", Read),
    ("redeemFamilySignIn", Read),
    ("getFamilySignInPending", Read),
    ("decideFamilySignIn", Read),
    ("putFamilySignInBox", Read),
    ("getFamilySignInRedeem", Read),
];

/// The `/rest` endpoint a path names (`/rest/getAlbum.view` is `getAlbum`), or None for any
/// other path.
pub fn rest_endpoint_name(path: &str) -> Option<&str> {
    let rest = strip_prefix_ignore_case(path, "/rest/")?;
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    Some(match rest.len().checked_sub(5) {
        Some(at) if rest[at..].eq_ignore_ascii_case(".view") => &rest[..at],
        _ => rest,
    })
}

/// The class of a `/rest` endpoint by name (any case, with or without `.view`), or None when
/// Octo does not know it.
pub fn rest_class(name: &str) -> Option<EndpointClass> {
    let name = name.strip_suffix(".view").unwrap_or(name);
    REST_ENDPOINTS
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(name))
        .map(|(_, class)| *class)
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &text[prefix.len()..])
}

fn under(path: &str, prefix: &str) -> bool {
    match strip_prefix_ignore_case(path, prefix) {
        Some(rest) => rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}

fn is_read(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// The class of a request when Octo knows the endpoint, or None when it does not (an unknown
/// `/rest` name, or a write to a Navidrome native path Octo has not classified).
pub fn classify_known(path: &str, method: &Method) -> Option<EndpointClass> {
    if let Some(name) = rest_endpoint_name(path) {
        return rest_class(name);
    }
    if under(path, "/rest") {
        // `/rest/a/b`: no Subsonic endpoint looks like that.
        return None;
    }
    if under(path, "/jellyfin") {
        return Some(Refused);
    }
    if under(path, "/radio/stream") {
        return Some(Audio);
    }
    if under(path, "/share") {
        return Some(Audio);
    }
    if under(path, "/auth") {
        if under(path, "/auth/login") {
            return Some(Read);
        }
        if under(path, "/auth/createAdmin") {
            return Some(Admin);
        }
        return is_read(method).then_some(Read);
    }
    // Octo's own pages and APIs check their callers themselves.
    if under(path, "/api/admin") || under(path, "/api/family") {
        return Some(Read);
    }
    if under(path, "/api") {
        return native_class(path, method);
    }
    Some(Read)
}

/// Navidrome's native API (`server/nativeapi`, 0.64.2).
fn native_class(path: &str, method: &Method) -> Option<EndpointClass> {
    // Approving a Jellyfin Quick Connect device hands out a Jellyfin token, which streams
    // original files past every cap.
    if under(path, "/api/quickconnect/authorize") {
        return Some(Refused);
    }
    if is_read(method) {
        return Some(Read);
    }
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let area = segments.get(1).copied().unwrap_or("").to_ascii_lowercase();
    match area.as_str() {
        "user" if segments.get(3).is_some_and(|s| s.eq_ignore_ascii_case("library")) => Some(Admin),
        "user" => Some(AccountWrite),
        "share" => Some(Audio),
        "playlist" | "queue" | "keepalive" => Some(Read),
        "player" | "transcoding" | "library" | "radio" | "missing" | "plugin" | "metadata" | "config"
        | "inspect" | "artist" => Some(Admin),
        _ => None,
    }
}

/// The class the family gate uses: an endpoint Octo does not know is `Refused` when it is a
/// `/rest` call or a native write, and `Read` when it is a native read.
pub fn classify(path: &str, method: &Method) -> EndpointClass {
    classify_known(path, method).unwrap_or(Refused)
}

/// The class for one caller. `strict` is true for capped and kid members, who are refused an
/// unknown `/rest` endpoint; for everyone else an unknown `/rest` endpoint is a `Read` (the
/// relay passes it on, as today). Unknown native writes are refused either way.
pub fn classify_for(path: &str, method: &Method, strict: bool) -> EndpointClass {
    match classify_known(path, method) {
        Some(class) => class,
        None if !strict && rest_endpoint_name(path).is_some() => Read,
        None => Refused,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = include_str!("../../../../docs/family/opensubsonic-endpoints.txt");

    fn names() -> Vec<&'static str> {
        LIST.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect()
    }

    #[test]
    fn every_opensubsonic_endpoint_is_classified() {
        let names = names();
        assert!(names.len() >= 85, "the list has {} names", names.len());
        let missing: Vec<&str> = names
            .iter()
            .copied()
            .filter(|name| rest_class(name).is_none())
            .collect();
        assert!(missing.is_empty(), "unclassified: {missing:?}");
        for name in names {
            assert_eq!(
                classify(&format!("/rest/{name}.view"), &Method::GET),
                rest_class(name).expect("classified"),
                "{name}"
            );
        }
    }

    #[test]
    fn the_class_table_names_each_endpoint_once() {
        let mut seen = std::collections::HashSet::new();
        for (name, _) in REST_ENDPOINTS {
            assert!(seen.insert(name.to_ascii_lowercase()), "{name} twice");
        }
    }

    #[test]
    fn every_audio_path_is_audio() {
        for name in [
            "stream",
            "download",
            "getTranscodeDecision",
            "getTranscodeStream",
            "hls",
            "createShare",
        ] {
            assert_eq!(classify(&format!("/rest/{name}"), &Method::GET), Audio, "{name}");
        }
        assert_eq!(classify("/radio/stream/abc123", &Method::GET), Audio);
        assert_eq!(classify("/share/abc/stream", &Method::GET), Audio);
        assert_eq!(classify("/api/share", &Method::POST), Audio);
    }

    #[test]
    fn names_and_paths_ignore_case_and_view() {
        assert_eq!(rest_endpoint_name("/rest/getAlbum.view"), Some("getAlbum"));
        assert_eq!(rest_endpoint_name("/REST/Stream.VIEW"), Some("Stream"));
        assert_eq!(rest_endpoint_name("/rest/ping"), Some("ping"));
        assert_eq!(rest_endpoint_name("/rest/"), None);
        assert_eq!(rest_endpoint_name("/api/song"), None);
        assert_eq!(classify("/REST/Stream.VIEW", &Method::POST), Audio);
    }

    #[test]
    fn unknown_rest_endpoint_is_refused_only_for_strict_callers() {
        assert_eq!(classify_known("/rest/getSomethingNew", &Method::GET), None);
        assert_eq!(classify("/rest/getSomethingNew", &Method::GET), Refused);
        assert_eq!(classify_for("/rest/getSomethingNew", &Method::GET, true), Refused);
        assert_eq!(classify_for("/rest/getSomethingNew", &Method::GET, false), Read);
        assert_eq!(classify_for("/rest/getAlbum", &Method::GET, true), Read);
    }

    #[test]
    fn native_reads_pass_and_unknown_native_writes_are_refused() {
        assert_eq!(classify("/api/song", &Method::GET), Read);
        assert_eq!(classify("/api/somethingnew/1", &Method::GET), Read);
        assert_eq!(classify("/api/song/1", &Method::PUT), Refused);
        assert_eq!(classify_for("/api/somethingnew", &Method::POST, false), Refused);
        assert_eq!(classify("/api/user/abc", &Method::PUT), AccountWrite);
        assert_eq!(classify("/api/user", &Method::POST), AccountWrite);
        assert_eq!(classify("/api/user/abc/library", &Method::PUT), Admin);
        assert_eq!(classify("/api/player/p1", &Method::PUT), Admin);
        assert_eq!(classify("/api/transcoding", &Method::POST), Admin);
        assert_eq!(classify("/api/library", &Method::POST), Admin);
        assert_eq!(classify("/api/playlist/p1/tracks", &Method::POST), Read);
        assert_eq!(classify("/api/queue", &Method::POST), Read);
    }

    #[test]
    fn jellyfin_and_quick_connect_are_refused() {
        assert_eq!(classify("/jellyfin/Items", &Method::GET), Refused);
        assert_eq!(classify("/jellyfin", &Method::GET), Refused);
        assert_eq!(classify("/api/quickconnect/authorize", &Method::POST), Refused);
        assert_eq!(classify("/api/quickconnect", &Method::GET), Read);
    }

    #[test]
    fn sign_in_and_octo_pages() {
        assert_eq!(classify("/auth/login", &Method::POST), Read);
        assert_eq!(classify("/auth/createAdmin", &Method::POST), Admin);
        assert_eq!(classify("/auth/other", &Method::POST), Refused);
        assert_eq!(classify("/api/admin/settings", &Method::POST), Read);
        assert_eq!(classify("/api/family/me", &Method::GET), Read);
        assert_eq!(classify("/app/", &Method::GET), Read);
        assert_eq!(classify("/apiary", &Method::POST), Read);
    }

    #[test]
    fn library_writes_are_library_writes() {
        for name in [
            "star",
            "libraryAction",
            "pickFoundSong",
            "importAction",
            "importFile",
            "importText",
            "setLyricsChoice",
            "findSongs",
            "keepAcquisitionAnyway",
        ] {
            assert_eq!(rest_class(name), Some(LibraryWrite), "{name}");
        }
        assert_eq!(rest_class("getImportServices"), Some(Read));
        for name in ["createUser", "updateUser", "deleteUser", "changePassword"] {
            assert_eq!(rest_class(name), Some(AccountWrite), "{name}");
        }
    }
}
