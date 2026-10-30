//! The Subsonic API Octo answers itself, split by area. Every action is registered at
//! `rest/{name}` and `rest/{name}.view` for GET and POST; any other method, and every path no
//! action claims, reaches the catch-all (`http::catch_all`).
pub mod action_helpers;
pub mod animated_art;
pub mod info;
pub mod internet_radio;
pub mod native;
pub mod ping;
pub mod places;
pub mod playlists;
pub mod search;
pub mod similar_songs;
pub mod stations;
pub mod top_songs;
pub mod transitions;

#[cfg(test)]
pub(crate) mod route_tests;

pub(crate) mod album_ownership;
pub mod browsing;
pub(crate) mod caller_checks;
pub mod downloads;
pub mod extensions;
pub mod family;
pub mod imports;
pub mod lyrics;
pub mod media;
pub mod stars;

pub(crate) mod playlist_names;

#[cfg(test)]
mod test_app;

#[cfg(test)]
mod cover_cache_tests;
#[cfg(test)]
mod extensions_tests;
#[cfg(test)]
mod family_lists_tests;
#[cfg(test)]
mod for_you_tests;
#[cfg(test)]
mod fresh_marks_tests;
#[cfg(test)]
mod import_streaming_tests;
#[cfg(test)]
mod keep_anyway_tests;
#[cfg(test)]
mod library_edit_tests;
#[cfg(test)]
mod list_groups_tests;
#[cfg(test)]
mod lyrics_endpoint_tests;
#[cfg(test)]
mod marked_lists_tests;
#[cfg(test)]
mod media_tests;
#[cfg(test)]
mod merged_browsing_tests;
#[cfg(test)]
pub(crate) mod places_tests;
#[cfg(test)]
mod prefetch_stream_tests;
#[cfg(test)]
mod radio_ban_tests;
#[cfg(test)]
mod rating_tests;
#[cfg(test)]
mod scrobble_tests;
#[cfg(test)]
mod search_album_ownership_tests;
#[cfg(test)]
mod stop_waiting_tests;
#[cfg(test)]
mod stream_length_tests;
#[cfg(test)]
mod transitions_tests;

use axum::routing::get;

use crate::http::routes::RouteSet;

/// Every route of the controller.
pub fn routes() -> RouteSet {
    RouteSet::new()
        .subsonic("ping", get(ping::ping).post(ping::ping))
        .subsonic(
            "getRandomSongs",
            get(ping::get_random_songs).post(ping::get_random_songs),
        )
        .subsonic(
            "getPlaylists",
            get(playlists::get_playlists).post(playlists::get_playlists),
        )
        .subsonic(
            "getPlaylist",
            get(playlists::get_playlist).post(playlists::get_playlist),
        )
        .subsonic(
            "createPlaylist",
            get(playlists::mutate_playlist).post(playlists::mutate_playlist),
        )
        .subsonic(
            "updatePlaylist",
            get(playlists::mutate_playlist).post(playlists::mutate_playlist),
        )
        .subsonic(
            "deletePlaylist",
            get(playlists::mutate_playlist).post(playlists::mutate_playlist),
        )
        .subsonic(
            "getInternetRadioStations",
            get(internet_radio::get_internet_radio_stations)
                .post(internet_radio::get_internet_radio_stations),
        )
        .subsonic(
            "createInternetRadioStation",
            get(playlists::mutate_internet_radio_station).post(playlists::mutate_internet_radio_station),
        )
        .subsonic(
            "updateInternetRadioStation",
            get(playlists::mutate_internet_radio_station).post(playlists::mutate_internet_radio_station),
        )
        .subsonic(
            "deleteInternetRadioStation",
            get(playlists::mutate_internet_radio_station).post(playlists::mutate_internet_radio_station),
        )
        // [HttpGet, HttpHead]: axum answers HEAD with the GET handler, which looks at the
        // method itself.
        .route_with_head(
            "/radio/stream/{token}",
            get(internet_radio::stream_generated_radio),
        )
        .subsonic("search3", get(search::search3).post(search::search3))
        .subsonic("search2", get(search::search3).post(search::search3))
        .subsonic(
            "getSimilarSongs",
            get(similar_songs::get_similar_songs).post(similar_songs::get_similar_songs),
        )
        .subsonic(
            "getSimilarSongs2",
            get(similar_songs::get_similar_songs).post(similar_songs::get_similar_songs),
        )
        .subsonic(
            "getAlbumInfo2",
            get(info::get_album_info2).post(info::get_album_info2),
        )
        .subsonic(
            "getAlbumInfo",
            get(info::get_album_info2).post(info::get_album_info2),
        )
        .subsonic(
            "getArtistInfo2",
            get(info::get_artist_info2).post(info::get_artist_info2),
        )
        .subsonic(
            "getArtistInfo",
            get(info::get_artist_info).post(info::get_artist_info),
        )
        .merge(browsing::routes())
        .merge(media::routes())
        .merge(stars::routes())
        .merge(extensions::routes())
        .merge(downloads::routes())
        .merge(lyrics::routes())
        .merge(top_songs::routes())
        .merge(imports::routes())
        .merge(family::routes())
        .merge(transitions::routes())
        .merge(stations::routes())
        .merge(places::routes())
        .merge(animated_art::routes())
}
