//! `search3` and `search2`: the search hijack, its later song pages, and the sync walk.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use octo_core::common::dotnet;
use octo_core::models::domain::{Album, Artist, Song};
use octo_core::models::search::SearchResult;
use octo_core::models::subsonic::ExternalPlaylist;
use octo_subsonic::subsonic_model_mapper::{Row, SearchRows, SubsonicModelMapper};
use octo_subsonic::subsonic_response_builder::{SUBSONIC_NAMESPACE, SUBSONIC_VERSION};
use octo_subsonic::sync_catalog_response;
use octo_subsonic::xml::XElement;
use octo_subsonic::{Parameters, SubsonicReply};
use serde_json::{Map, Value};
use tracing::{debug, info};

use super::action_helpers::{
    SubsonicCall, extract_local_song_ids, int_param, int_try_parse, is_failed_subsonic_body,
    queue_refresh_if_stale, visible_stations,
};
use super::album_ownership::{judge_albums, scope_of, settle_search_albums};
use super::caller_checks::listener_name_of;
use super::media::draws_its_own_marks_in;
use crate::app::AppState;
use crate::http::error::AppError;
use crate::services::library::Standing;
use crate::services::subsonic::{
    SearchBudget, SearchSongOrder, SearchSongOrderCache, SearchSongPagePlanner, SyncCatalog, SyncCatalogKind,
    SyncCatalogService,
};

/// How long a sync page waits for the user's catalog to finish building before going
/// out without it. Kept short because a page the client gives up on fails its whole
/// sync, where a catalog that misses one sync is simply on the next.
const SYNC_CATALOG_WAIT: Duration = Duration::from_secs(15);

/// Search3 hijack. We OWN search results: Last.fm-driven external songs (YouTube-resolved on
/// play) after the local matches that genuinely live in the user's library. The goal is music
/// DISCOVERY, not library navigation. Library navigation lives in getAlbumList2, getArtists,
/// etc., which still pass through to Navidrome.
///
/// Empty queries do still pass through so a Subsonic client's "browse all" fallback isn't
/// broken; with a query, we hijack.
pub async fn search3(State(state): State<AppState>, req: Request) -> Result<Response, AppError> {
    let scope = scope_of(crate::services::family::family_caller(req.extensions()));
    let started = Instant::now();
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let format = call.format.as_str();
    let query = call.param_or("query", "");
    let clean_query = query.trim().trim_matches('"').to_string();
    let settings = state.settings.current();
    let subsonic = &settings.subsonic;
    let builder = &state.subsonic_response_builder;

    // search2 and search3 are the same hijack with different envelopes. Decide once:
    // the relay target, the envelope we answer with, and the empty-query passthrough
    // all have to agree.
    let is_search2 = dotnet::to_lower_invariant(&call.path()).contains("search2");
    let search_endpoint = if is_search2 {
        "rest/search2"
    } else {
        "rest/search3"
    };
    let envelope = if is_search2 {
        "searchResult2"
    } else {
        "searchResult3"
    };

    // Page one builds the discovery rows and remembers what it showed. A later page
    // carries on from that instead of going to Navidrome at the same offset, which
    // made every outside song past page one unreachable. See search_later_song_page.
    let song_offset = int_param(&call.parameters, "songOffset", 0);

    // A client that copies the library to the device walks it with an empty query and
    // never searches the server, so the walk is the only place discovery can reach it.
    // Scoped to the whole library: a walk of one music folder is left exactly as it was.
    if dotnet::is_blank(&clean_query)
        && !call.parameters.contains_key("musicFolderId")
        && SyncCatalogService::is_sync_client(subsonic, call.param("c"))
    {
        return sync_walk_page(&state, &call, search_endpoint, envelope).await;
    }

    if !dotnet::is_blank(&clean_query)
        && song_offset > 0
        && let Some(later_page) = search_later_song_page(
            &state,
            &call,
            &clean_query,
            song_offset,
            search_endpoint,
            envelope,
        )
        .await?
    {
        return Ok(later_page);
    }

    if dotnet::is_blank(&clean_query) || song_offset > 0 {
        return Ok(match call.proxy.relay(search_endpoint, &call.parameters).await {
            Ok(result) => call.file(&result.body, result.content_type.as_deref()),
            Err(_) => builder.create_response(format, envelope).into_response(),
        });
    }

    let requested_songs = int_param(&call.parameters, "songCount", 20);
    let requested_albums = int_param(&call.parameters, "albumCount", 20);
    let requested_artists = int_param(&call.parameters, "artistCount", 20);

    // Always include local results. The split itself lives in SearchBudget so it can be
    // unit-tested; the local floor used to be a flat 20, which is also the spec default for
    // songCount, so the most common search in the wild left nothing for discovery at all (#14).
    let (budget_local, external_target) = SearchBudget::compute(
        requested_songs,
        subsonic.enable_search_discovery,
        subsonic.effective_search_library_only_up_to(),
    );
    // The library first for most clients; see page_one_split.
    let (local_song_target, external_share) =
        page_one_split(&call.parameters, requested_songs, budget_local, external_target);

    // A client that asked for a handful of songs is searching as the user types. The
    // song side already costs nothing there (the budget leaves no room for discovery),
    // but external album search was still firing a Deezer query per keystroke. Judged
    // from the song count only when the client actually asked for songs, so a genuine
    // album-only search still gets album discovery.
    let is_type_ahead_probe = requested_songs > 0 && external_target == 0;

    // Outside albums, artists and playlists are a fixed handful that all fit on the
    // first page of their own list. Adding them again to a later album or artist page
    // repeated the same suggestions on every page the client scrolled to.
    let album_offset = int_param(&call.parameters, "albumOffset", 0);
    let artist_offset = int_param(&call.parameters, "artistOffset", 0);

    // Album discovery runs concurrently with the song fan-out below so it costs no
    // serial latency. It needs no Last.fm key (Deezer's catalog is keyless), so albums
    // still appear for a user who has not set one up.
    let album_task = (requested_albums > 0
        && album_offset <= 0
        && !is_type_ahead_probe
        && subsonic.enable_search_discovery)
        .then(|| {
            let external_search = Arc::clone(&state.external_search);
            let query = clean_query.clone();
            // What the library already holds of those albums, from their songs. Its tracklists
            // come from the same Deezer budget as the song rows, so they are read once the song
            // build has looked its rows up; the album search itself goes at once.
            let rows_looked_up = (external_target > 0).then(|| external_search.rows_looked_up(&query));
            let judging = state.clone();
            tokio::spawn(async move {
                let albums = external_search.get_albums(&query, requested_albums.min(20)).await;
                let found = started.elapsed();
                if let Some(rows_looked_up) = rows_looked_up {
                    rows_looked_up.await;
                }
                let standings = judge_albums(&judging, &albums, scope).await;
                (albums, standings, found, started.elapsed())
            })
        });

    // Artists the same way, and for the same reason: the merge has always known how to
    // fold external artists in and dedupe them against local ones. Keyless like albums,
    // so it works without a Last.fm key.
    let artist_task = (requested_artists > 0
        && artist_offset <= 0
        && !is_type_ahead_probe
        && subsonic.enable_search_discovery)
        .then(|| {
            let metadata = Arc::clone(&state.metadata_service);
            let query = clean_query.clone();
            tokio::spawn(async move {
                let artists = metadata.search_artists(&query, requested_artists.min(20)).await;
                (artists, started.elapsed())
            })
        });

    // One build per query, shared by every caller. Started here rather than awaited, so it
    // overlaps the local relay below; how many of its rows we actually use depends on what
    // that relay comes back with.
    let external_task = (external_target > 0).then(|| {
        let external_search = Arc::clone(&state.external_search);
        let query = clean_query.clone();
        tokio::spawn(async move {
            let songs = external_search.get(&query).await;
            (songs, started.elapsed())
        })
    });

    // Local pass-through. Albums/artists always get the full requested counts;
    // song-side gets the local target.
    let mut local_params = call.parameters.clone();
    local_params.insert("songCount".into(), local_song_target.to_string());
    local_params.insert("albumCount".into(), requested_albums.to_string());
    local_params.insert("artistCount".into(), requested_artists.to_string());
    let local_result = call.proxy.relay_safe(search_endpoint, &local_params).await;
    let library_done = started.elapsed();
    let local_body = local_result.as_ref().map(|r| r.body.as_ref());
    let local_type = local_result.as_ref().and_then(|r| r.content_type.as_deref());

    // Subsonic reports its own errors inside an HTTP 200, so a rejected login and an
    // empty library are the same thing to every check above this line. Left alone,
    // the discovery top-up would read "no local matches", fill the page with
    // suggestions, and present a broken connection as a healthy search.
    if is_failed_subsonic_body(local_body, local_type)
        && let Some(local) = &local_result
    {
        debug!("upstream rejected the search for '{clean_query}'; passing its error through");
        return Ok(call.file(&local.body, local.content_type.as_deref()));
    }

    // Parsed here rather than inside the merge so the count that sizes the discovery
    // slice below is taken from the very list the response will render.
    let mapper = model_mapper(&state);
    let mut local_parsed: SearchRows = match &local_result {
        Some(local) => mapper.parse_search_response(&local.body, local.content_type.as_deref()),
        None => (Vec::new(), Vec::new(), Vec::new()),
    };

    // Hand the slots the library did not fill to discovery. A query the user owns
    // nothing for is the one most worth answering with suggestions, and the local
    // target is a reservation rather than a promise: Navidrome returns what it has.
    let (built, songs_done): (Arc<Vec<Song>>, Option<Duration>) = match external_task {
        Some(task) => match task.await {
            Ok((songs, done)) => (songs, Some(done)),
            Err(_) => (Arc::new(Vec::new()), None),
        },
        None => (Arc::new(Vec::new()), None),
    };
    let external_slice = SearchSongOrder::page_one_external_count(
        built.len() as i32,
        local_song_target,
        external_share,
        local_parsed.0.len() as i32,
    );
    let external_songs: Vec<Song> = built
        .iter()
        .take(external_slice.max(0) as usize)
        .cloned()
        .collect();

    // Remember what this page showed so the next page can carry on from it. Only when
    // discovery was part of the answer (a type-ahead page has none to continue) and
    // the library answered, since a failed relay would record an empty library.
    // Filed under who asked; a request Octo cannot name (an API key Navidrome would not
    // vouch for) is not remembered at all, so it can never land in someone else's slot.
    if external_target > 0
        && local_result.is_some()
        && let Some(order_key) = song_order_key(&state, &call, search_endpoint, &clean_query).await?
    {
        state.search_song_order_cache.set(
            &order_key,
            SearchSongOrder::from(
                Arc::clone(&built),
                requested_songs,
                local_song_target,
                external_share,
                &local_parsed.0,
            ),
        );
    }

    let playlists: Vec<ExternalPlaylist> = if subsonic.enable_external_playlists && album_offset <= 0 {
        state
            .metadata_service
            .search_playlists(&clean_query, requested_albums)
            .await
    } else {
        Vec::new()
    };

    // Degrade to no albums rather than failing the whole search if Deezer is slow,
    // throttled or unreachable.
    let mut albums_done = None;
    let (external_albums, standings): (Vec<Album>, Vec<Standing>) = match album_task {
        Some(task) => match task.await {
            Ok((albums, standings, found, judged)) => {
                albums_done = Some((found, judged));
                (albums.as_ref().clone(), standings)
            }
            Err(error) => {
                debug!("external album search failed for '{clean_query}': {error}");
                (Vec::new(), Vec::new())
            }
        },
        None => (Vec::new(), Vec::new()),
    };
    let mut artists_done = None;
    let external_artists: Vec<Artist> = match artist_task {
        Some(task) => match task.await {
            Ok((artists, done)) => {
                artists_done = Some(done);
                artists
            }
            Err(error) => {
                debug!("external artist search failed for '{clean_query}': {error}");
                Vec::new()
            }
        },
        None => Vec::new(),
    };

    // Track this response as a "queue" so a later scrobble for any of its
    // songs can drive the sliding-window prewarm of upcoming externals.
    // Order matches the merged response order: local first, external after.
    let local_song_ids = extract_local_song_ids(local_body, local_type);
    state.radio_queues.register(
        local_song_ids
            .into_iter()
            .chain(external_songs.iter().map(|song| song.id.clone())),
    );

    // An album the library holds whole, or by its very name, is listed as the library's album;
    // one it holds in part says how much. search2 lists folders, not albums, so nothing is added
    // there and its outside albums stay, counted.
    let (settled_albums, added_albums) = settle_search_albums(
        builder,
        &call.proxy,
        &call.parameters,
        external_albums,
        &standings,
        &local_parsed.1,
        local_type,
        !is_search2,
    )
    .await;
    local_parsed.1.extend(added_albums);

    let external_result = SearchResult {
        songs: external_songs,
        albums: settled_albums,
        artists: external_artists,
    };
    let answer = merge_search_results(
        &state,
        &mapper,
        local_parsed,
        local_type,
        &external_result,
        &playlists,
        format,
        envelope,
        None,
    );
    info!(
        "{} '{clean_query}' answered in {} ms; parts done at (ms): library {}, outside songs {}, outside albums {}, artists {}",
        if is_search2 { "search2" } else { "search3" },
        started.elapsed().as_millis(),
        millis(Some(library_done)),
        millis(songs_done),
        albums_done.map_or_else(
            || "-".to_string(),
            |(found, judged)| format!("{} (counted at {})", found.as_millis(), judged.as_millis())
        ),
        millis(artists_done),
    );
    Ok(answer)
}

/// When a part of a search was done, in milliseconds from the start of the request; "-" for
/// a part the search did not run.
fn millis(done: Option<Duration>) -> String {
    done.map_or_else(|| "-".to_string(), |done| done.as_millis().to_string())
}

/// Page one's library rows and its own share of outside rows, from the budget's split. Every
/// client but Octo's apps gets the library first: the whole page goes to the library and
/// outside songs get no share of their own, only the places the library leaves empty. Octo's
/// apps show library and outside songs in sections of their own, so they keep the budget's
/// split.
fn page_one_split(
    parameters: &Parameters,
    requested_songs: i32,
    budget_local: i32,
    external_target: i32,
) -> (i32, i32) {
    if draws_its_own_marks_in(parameters) {
        (budget_local, external_target)
    } else {
        (SearchBudget::library_first_target(requested_songs), 0)
    }
}

fn model_mapper(state: &AppState) -> SubsonicModelMapper {
    SubsonicModelMapper::new((*state.subsonic_response_builder).clone())
}

/// Where a search's order is kept: per user, so `None` when the request names nobody Octo
/// can vouch for. An API key sign-in carries no `u`; before this every such user shared the
/// one empty-name slot and could be handed another's order.
async fn song_order_key(
    state: &AppState,
    call: &SubsonicCall,
    search_endpoint: &str,
    clean_query: &str,
) -> Result<Option<String>, AppError> {
    let user = state
        .request_identity
        .username(&call.parameters, &call.proxy)
        .await?;
    Ok(user.map(|user| {
        SearchSongOrderCache::key(
            &user,
            call.param_or("c", ""),
            search_endpoint,
            call.param("musicFolderId"),
            clean_query,
        )
    }))
}

/// A later page of a search's songs: the next stretch of the order page one started (its
/// library rows, its outside rows, then the rest of the library), so paging never repeats or
/// skips a row. See `SearchSongPagePlanner`.
///
/// `None` when the page should go to Navidrome unchanged, as every later page used to:
/// discovery is off, or nothing is remembered for this search and the request is too small to
/// have earned discovery on its own (a type-ahead count).
async fn search_later_song_page(
    state: &AppState,
    call: &SubsonicCall,
    clean_query: &str,
    song_offset: i32,
    search_endpoint: &str,
    envelope: &str,
) -> Result<Option<Response>, AppError> {
    if !state.settings.current().subsonic.enable_search_discovery {
        return Ok(None);
    }

    let requested_songs = int_param(&call.parameters, "songCount", 20);
    // With nobody to file it under, nothing is read or kept: every later page is rebuilt.
    // For an API key sign-in the tokenInfo call that names the user is made with the
    // request's own key, so it is also the credential check this page has not yet had.
    let key = song_order_key(state, call, search_endpoint, clean_query).await?;
    let mapper = model_mapper(state);
    let mut order = key.as_deref().and_then(|key| {
        state
            .search_song_order_cache
            .get(key, requested_songs, song_offset)
    });
    if order.is_none() {
        // Nothing remembered: expired, or Octo restarted since page one. Build the order
        // again as if page one had asked for this page's count. The build is shared with
        // any page one still running for the query, so a client that asks for two pages
        // at once gets one build.
        let (budget_local, external_target) = SearchBudget::compute(
            requested_songs,
            true,
            state
                .settings
                .current()
                .subsonic
                .effective_search_library_only_up_to(),
        );
        if external_target == 0 {
            return Ok(None);
        }
        let (local_target, external_share) =
            page_one_split(&call.parameters, requested_songs, budget_local, external_target);

        let built_task = {
            let external_search = Arc::clone(&state.external_search);
            let query = clean_query.to_string();
            tokio::spawn(async move { external_search.get(&query).await })
        };
        let mut prefix_params = call.parameters.clone();
        prefix_params.insert("songOffset".into(), "0".into());
        prefix_params.insert("songCount".into(), local_target.to_string());
        prefix_params.insert("albumCount".into(), "0".into());
        prefix_params.insert("artistCount".into(), "0".into());
        let prefix = call.proxy.relay_safe(search_endpoint, &prefix_params).await;
        if let Some(prefix) = &prefix
            && is_failed_subsonic_body(Some(&prefix.body), prefix.content_type.as_deref())
        {
            return Ok(Some(call.file(&prefix.body, prefix.content_type.as_deref())));
        }
        let Some(prefix) = prefix else {
            return Ok(None);
        };

        let prefix_songs = mapper
            .parse_search_response(&prefix.body, prefix.content_type.as_deref())
            .0;
        let built = built_task.await.unwrap_or_default();
        let rebuilt = SearchSongOrder::from(
            built,
            requested_songs,
            local_target,
            external_share,
            &prefix_songs,
        );
        if let Some(key) = &key {
            state.search_song_order_cache.set(key, rebuilt.clone());
        }
        debug!("search '{clean_query}': page one's order was gone, rebuilt it for offset {song_offset}");
        order = Some(rebuilt);
    }
    let order = order.expect("set above");

    let page = SearchSongPagePlanner::plan_order(song_offset, requested_songs, &order);

    // One relay for the page's library rows and for the albums and artists, which page
    // exactly as they always have on a later page: Navidrome's, at the client's offsets.
    let mut local_params = call.parameters.clone();
    local_params.insert("songOffset".into(), page.local_offset.to_string());
    local_params.insert(
        "songCount".into(),
        (page.leading_locals + page.trailing_locals).to_string(),
    );
    let local_result = call.proxy.relay_safe(search_endpoint, &local_params).await;
    if let Some(local) = &local_result
        && is_failed_subsonic_body(Some(&local.body), local.content_type.as_deref())
    {
        return Ok(Some(call.file(&local.body, local.content_type.as_deref())));
    }
    // Navidrome did not answer. The page is not made from the order alone: without its
    // library rows it would be short, and the client, taking it as it came, would never
    // see those rows. It goes to Navidrome as every later page used to.
    let Some(local) = local_result else {
        return Ok(None);
    };

    let (songs, albums, artists) = mapper.parse_search_response(&local.body, local.content_type.as_deref());
    let leading_count = count(page.leading_locals);
    let leading: Vec<Row> = songs.iter().take(leading_count).cloned().collect();
    let trailing: Vec<Row> = songs
        .iter()
        .skip(leading_count)
        .take(count(page.trailing_locals))
        .cloned()
        .collect();

    // Page one's own outside rows keep their places even where page one left one out
    // because the library had it, so the rows after them do not shift.
    let external_songs: Vec<Song> = order
        .built
        .iter()
        .skip(count(page.page_one_external_skip))
        .take(count(page.page_one_external_take))
        .filter(|song| !SubsonicModelMapper::is_listed(song, &order.prefix_keys))
        .chain(
            order
                .later_externals
                .iter()
                .skip(count(page.later_external_skip))
                .take(count(page.later_external_take)),
        )
        .cloned()
        .collect();

    debug!(
        "search '{clean_query}' page at {song_offset}+{requested_songs}: {} library, {} outside, {} library",
        leading.len(),
        external_songs.len(),
        trailing.len()
    );

    let local_song_ids = extract_local_song_ids(Some(&local.body), local.content_type.as_deref());
    let (leading_len, trailing_len) = (leading.len(), trailing.len());
    state.radio_queues.register(
        local_song_ids
            .iter()
            .take(leading_len)
            .cloned()
            .chain(external_songs.iter().map(|song| song.id.clone()))
            .chain(
                local_song_ids
                    .iter()
                    .skip(leading_len)
                    .take(trailing_len)
                    .cloned(),
            )
            .collect::<Vec<_>>(),
    );

    let external_result = SearchResult {
        songs: external_songs,
        albums: Vec::new(),
        artists: Vec::new(),
    };
    Ok(Some(merge_search_results(
        state,
        &mapper,
        (leading, albums, artists),
        local.content_type.as_deref(),
        &external_result,
        &[],
        &call.format,
        envelope,
        Some(trailing),
    )))
}

/// A count that may be negative, as a length (a negative count is 0).
fn count(value: i32) -> usize {
    usize::try_from(value).unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
fn merge_search_results(
    state: &AppState,
    mapper: &SubsonicModelMapper,
    (local_songs, local_albums, local_artists): SearchRows,
    local_content_type: Option<&str>,
    external_result: &SearchResult,
    playlist_result: &[ExternalPlaylist],
    format: &str,
    envelope: &str,
    trailing_local_songs: Option<Vec<Row>>,
) -> Response {
    let is_json = format == "json" || local_content_type.is_some_and(|c| c.contains("json"));
    let (merged_songs, merged_albums, merged_artists) = mapper.merge_search_results(
        local_songs,
        local_albums,
        local_artists,
        external_result,
        playlist_result,
        is_json,
        trailing_local_songs,
    );

    if is_json {
        // Keyed by the request's envelope: search2 answered under searchResult3 is a shape
        // the client never asked for, and a strict one drops the whole payload.
        let rows = |rows: Vec<Row>| Value::Array(rows.into_iter().filter_map(Row::into_json).collect());
        let mut result = Map::new();
        result.insert("song".into(), rows(merged_songs));
        result.insert("album".into(), rows(merged_albums));
        result.insert("artist".into(), rows(merged_artists));
        let mut body = Map::new();
        body.insert("status".into(), "ok".into());
        body.insert("version".into(), SUBSONIC_VERSION.into());
        body.insert(envelope.into(), Value::Object(result));
        return state
            .subsonic_response_builder
            .create_json_response(Value::Object(body))
            .into_response();
    }

    let mut search_result = XElement::ns(SUBSONIC_NAMESPACE, envelope);
    for row in merged_artists
        .into_iter()
        .chain(merged_albums)
        .chain(merged_songs)
    {
        if let Some(element) = row.into_xml() {
            search_result.push(element);
        }
    }
    let document = XElement::ns(SUBSONIC_NAMESPACE, "subsonic-response")
        .attr("status", "ok")
        .attr("version", SUBSONIC_VERSION)
        .child(search_result);
    SubsonicReply::xml(&document).into_response()
}

/// One page of a sync walk: Navidrome's page, then, once the library has run out, the
/// user's catalog rows that fill the rest of it. See `SyncCatalogService`. Each kind
/// (artist, album, song) is paged on its own, since a client can walk them separately or
/// together.
async fn sync_walk_page(
    state: &AppState,
    call: &SubsonicCall,
    search_endpoint: &str,
    envelope: &str,
) -> Result<Response, AppError> {
    let format = call.format.as_str();
    let relay = call.proxy.relay_safe(search_endpoint, &call.parameters).await;
    let Some(relay) = relay.filter(|relay| !relay.body.is_empty()) else {
        return Ok(state
            .subsonic_response_builder
            .create_response(format, envelope)
            .into_response());
    };
    let unchanged = || call.file(&relay.body, relay.content_type.as_deref());
    if is_failed_subsonic_body(Some(&relay.body), relay.content_type.as_deref()) {
        return Ok(unchanged());
    }

    let username = listener_name_of(state, &call.parameters, &call.proxy, &call.parts.headers).await;
    let mut stations = visible_stations(state, &username);
    // The chart lists ride along as stations, so a syncing client holds their outside songs
    // under the ids the playlists list.
    if !username.is_empty() {
        state.chart_playlists.warm(&username, &call.parameters).await;
        stations.extend(state.chart_playlists.as_sync_stations(&username));
    }
    let rows = sync_catalog_response::count_rows(&relay.body, relay.content_type.as_deref(), envelope);
    let Some((artists_returned, albums_returned, songs_returned)) = rows.filter(|_| !username.is_empty())
    else {
        return Ok(unchanged());
    };

    let number = |name: &str, fallback: i32| match int_try_parse(call.param_or(name, "")) {
        Some(value) => value.max(0),
        None => fallback,
    };
    let pages: Vec<(SyncCatalogKind, i32, i32, i32)> = [
        (SyncCatalogKind::Artist, "artist", artists_returned),
        (SyncCatalogKind::Album, "album", albums_returned),
        (SyncCatalogKind::Song, "song", songs_returned),
    ]
    .into_iter()
    .map(|(kind, name, returned)| {
        (
            kind,
            returned as i32,
            number(&format!("{name}Offset"), 0),
            number(&format!("{name}Count"), 20),
        )
    })
    .collect();

    // The first page of a walk starts the build, so it has the whole library's worth of
    // pages to finish in before the walk reaches the catalog.
    if pages
        .iter()
        .any(|(_, _, offset, count)| *offset == 0 && *count > 0)
    {
        queue_refresh_if_stale(state, &username);
        state.sync_catalog.warm(&username, &stations, &call.parameters);
    }
    if stations.is_empty() {
        return Ok(unchanged());
    }

    let mut artists: Vec<Artist> = Vec::new();
    let mut albums: Vec<Album> = Vec::new();
    let mut songs: Vec<Song> = Vec::new();
    let mut used: Option<Arc<SyncCatalog>> = None;
    for (kind, returned, offset, count) in pages {
        if count == 0 || returned >= count {
            continue;
        }
        let local_total = match SyncCatalogService::local_total_from_page(offset, returned) {
            Some(total) => Some(total),
            None => {
                SyncCatalogService::resolve_local_total(
                    offset,
                    state.sync_catalog.remembered_local_total(&username, kind),
                    |index| exists(call, search_endpoint, envelope, kind, index),
                )
                .await
            }
        };
        let Some(local_total) = local_total else {
            continue;
        };

        // A short page is only the end of the library if nothing follows it. A server that
        // caps the page size also answers short, and filling that page would make the
        // client skip the library rows the cap held back.
        if returned > 0 && exists(call, search_endpoint, envelope, kind, local_total).await != Some(false) {
            continue;
        }

        let (start, take) = SyncCatalogService::window(offset, count, returned, local_total);
        let pinned = if start > 0 {
            state.sync_catalog.pinned_catalog(&username, kind)
        } else {
            None
        };
        let catalog = match pinned {
            Some(catalog) => catalog,
            None => {
                let building = state.sync_catalog.get(&username, &stations, &call.parameters);
                match tokio::time::timeout(SYNC_CATALOG_WAIT, building).await {
                    Ok(catalog) => catalog,
                    Err(_) => {
                        info!(
                            "Sync catalog for {username} still building; this sync ends at the library and the next one gets it"
                        );
                        continue;
                    }
                }
            }
        };
        state
            .sync_catalog
            .remember(&username, kind, local_total, Arc::clone(&catalog));

        let (slice_songs, slice_albums, slice_artists) =
            SyncCatalogService::slice(&catalog, kind, start, take);
        if !slice_artists.is_empty() {
            artists = slice_artists;
        }
        if !slice_albums.is_empty() {
            albums = slice_albums;
        }
        if !slice_songs.is_empty() {
            songs = slice_songs;
        }
        used = Some(catalog);
    }

    let Some(used) = used.filter(|_| artists.len() + albums.len() + songs.len() > 0) else {
        return Ok(unchanged());
    };
    info!(
        "Sync walk for {username} ({}): added {} songs, {} albums, {} artists after the library",
        call.param_or("c", ""),
        songs.len(),
        albums.len(),
        artists.len()
    );
    let body = sync_catalog_response::append(
        &relay.body,
        relay.content_type.as_deref(),
        envelope,
        &state.subsonic_response_builder,
        used.added(),
        &artists,
        &albums,
        &songs,
    )?;
    Ok(call.file(&body.into(), relay.content_type.as_deref()))
}

/// Whether the library has a row of `kind` at `index`: one row asked for there, in JSON.
/// `None` when the probe fails, so nothing is appended on a guess.
async fn exists(
    call: &SubsonicCall,
    search_endpoint: &str,
    envelope: &str,
    kind: SyncCatalogKind,
    index: i32,
) -> Option<bool> {
    // The client's own empty query, since servers disagree on which spelling of
    // "everything" they accept.
    let mut probe: Parameters = call.parameters.clone();
    probe.insert("f".into(), "json".into());
    for name in [
        "artistCount",
        "albumCount",
        "songCount",
        "artistOffset",
        "albumOffset",
        "songOffset",
    ] {
        probe.insert(name.into(), "0".into());
    }
    let name = match kind {
        SyncCatalogKind::Artist => "artist",
        SyncCatalogKind::Album => "album",
        SyncCatalogKind::Song => "song",
    };
    probe.insert(format!("{name}Count"), "1".into());
    probe.insert(format!("{name}Offset"), index.to_string());
    let result = call.proxy.relay_safe(search_endpoint, &probe).await?;
    if result.body.is_empty() || is_failed_subsonic_body(Some(&result.body), result.content_type.as_deref()) {
        return None;
    }
    let (artists, albums, songs) = sync_catalog_response::count_rows(
        &result.body,
        Some(result.content_type.as_deref().unwrap_or("application/json")),
        envelope,
    )?;
    Some(
        match kind {
            SyncCatalogKind::Artist => artists,
            SyncCatalogKind::Album => albums,
            SyncCatalogKind::Song => songs,
        } > 0,
    )
}
