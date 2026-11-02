//! getArtistTopSongs and getTopChart (octoTopSongs, for the Octo apps), the standard getTopSongs
//! (every Subsonic app), getCharts (octoTopSongs v2: the charts a country has), and
//! setChartCountry and pinChartCountry (v3: a listener's own chart country and pinned ones).

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use octo_core::charts::chart_settings::{self, ChartKind, MAX_PINNED, OVERALL, list_id};
use octo_core::charts::top_songs::MAX_ROWS;
use octo_core::common::skipped_takes;
use octo_core::models::domain::Song;
use octo_core::soulseek::soulseek_metadata_service::RoutingKind;
use octo_subsonic::subsonic_request_parser::Parameters;
use octo_subsonic::subsonic_response_builder::{
    ChartChoiceRow, ChartsHeader, RankedRow, TopSongRow, TopSongsHeader,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::{debug, info};

use super::caller_checks::{SubsonicCall, listener_name, refuse_unless_signed_in, signed_in_user};
use super::native::has_external_id;
use crate::app::AppState;
use crate::http::error::{AppError, AppResult};
use crate::http::routes::RouteSet;
use crate::services::charts::apple_music_pages::LaneCell;
use crate::services::common::top_songs_service::OtherWork;
use crate::services::common::{TopList, TopSongsService};
use crate::services::subsonic::SubsonicProxyService;

/// How many rows a list answers when the caller does not say.
const TOP_SONGS_DEFAULT_COUNT: usize = 20;
/// The count the Subsonic API gives getTopSongs when the caller does not say.
const STANDARD_TOP_SONGS_DEFAULT_COUNT: usize = 50;

pub fn routes() -> RouteSet {
    RouteSet::new()
        .subsonic(
            "getArtistTopSongs",
            get(get_artist_top_songs).post(get_artist_top_songs),
        )
        .subsonic("getTopChart", get(get_top_chart).post(get_top_chart))
        .subsonic("getCharts", get(get_charts).post(get_charts))
        .subsonic("setChartCountry", get(set_chart_country).post(set_chart_country))
        .subsonic("pinChartCountry", get(pin_chart_country).post(pin_chart_country))
        .subsonic("getTopSongs", get(get_top_songs).post(get_top_songs))
}

/// The answer to a call Octo refuses, always JSON.
fn refuse(state: &AppState, code: i32, message: &str) -> Response {
    state
        .subsonic_response_builder
        .create_error("json", code, message)
        .into_response()
}

const STILL_LOADING_COUNTRIES: &str = "Still loading your other countries. Try again in a moment.";

/// Who a country load counts for: the lowercased user, or when the call names nobody, a
/// fingerprint of its credential (the API key, else the token and salt).
fn counted_who(username: &str, call: &SubsonicCall) -> String {
    who_of(
        username,
        call.param_opt("apiKey"),
        call.param("t"),
        call.param("s"),
    )
}

/// [`counted_who`] from the credential's parts.
fn who_of(username: &str, api_key: Option<&str>, token: &str, salt: &str) -> String {
    let name = username.trim().to_lowercase();
    if !name.is_empty() {
        return name;
    }
    let mut credential = Sha256::new();
    match api_key.filter(|key| !key.is_empty()) {
        Some(key) => credential.update(key.as_bytes()),
        None => {
            credential.update(token.as_bytes());
            credential.update(salt.as_bytes());
        }
    }
    credential.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The country a chart call asks for: `country` when sent (Apple must have it, and the listener
/// must not have two other countries loading already), else the caller's own, as for `auto`.
/// With it, what loading it counts as: kept by the work that call starts. `for_menu` says the
/// call loads the country's genre menu rather than one chart. Err holds the answer for one that
/// is refused.
#[allow(clippy::result_large_err)] // the error is the answer itself, sent at once
async fn asked_country(
    state: &AppState,
    call: &SubsonicCall,
    service: &TopSongsService,
    username: &str,
    chart: &str,
    for_menu: bool,
) -> Result<(Option<String>, OtherWork), Response> {
    let Some(sent) = call
        .param_opt("country")
        .map(str::trim)
        .filter(|c| !c.is_empty() && !c.eq_ignore_ascii_case("auto"))
    else {
        return Ok((service.country_of(username).await, OtherWork::Free));
    };
    let code = sent.to_ascii_lowercase();
    if !service.has_country(&code) {
        return Err(refuse(state, 10, "Unknown country."));
    }
    let key = if for_menu {
        TopSongsService::menu_work(&code)
    } else {
        TopSongsService::chart_work(&code, chart)
    };
    let Some(work) = service.begin_other_work(&counted_who(username, call), &code, &key) else {
        return Err(refuse(state, 0, STILL_LOADING_COUNTRIES));
    };
    Ok((Some(code), work))
}

/// The signed-in listener a save is for, or the answer when there is none.
async fn listener(state: &AppState, call: &SubsonicCall) -> Result<Result<String, Response>, AppError> {
    let username = signed_in_user(state, call).await?;
    if username.trim().is_empty() {
        return Ok(Err(refuse(
            state,
            10,
            "Sign in as a listener to choose chart countries.",
        )));
    }
    Ok(Ok(username))
}

/// The `country` a save names, lowercase, or the answer when it is missing.
#[allow(clippy::result_large_err)] // the error is the answer itself, sent at once
fn sent_country(state: &AppState, call: &SubsonicCall) -> Result<String, Response> {
    call.param_opt("country")
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| refuse(state, 10, "Required parameter is missing: country"))
}

/// The service, while search discovery is on: its lists are outside songs.
fn top_songs(state: &AppState) -> Option<&TopSongsService> {
    state
        .settings
        .current()
        .subsonic
        .enable_search_discovery
        .then_some(&state.top_songs)
}

/// octoTopSongs v1: an artist's most played songs, ranked, for the apps' search. Each is marked
/// in the caller's library or not. `artist` is the name; `id` may be the artist's id from search,
/// which names the catalog artist surely. Always JSON.
pub async fn get_artist_top_songs(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    if let Some(refused) = refuse_unless_signed_in(&state, &call, "json").await? {
        return Ok(refused);
    }
    let name = call.param_opt("artist").unwrap_or("").trim().to_string();
    if name.is_empty() {
        return Ok(state
            .subsonic_response_builder
            .create_error("json", 10, "Required parameter is missing: artist")
            .into_response());
    }
    let Some(service) = top_songs(&state) else {
        return Ok(empty_answer(&state));
    };
    let list = service.for_artist(&name, call.param_opt("id")).await;
    let count = count(&call, TOP_SONGS_DEFAULT_COUNT, MAX_ROWS);
    Ok(answer(&state, &call, &list, count).await)
}

/// octoTopSongs: a chart, ranked and marked like getArtistTopSongs. `chart` (v2) is `34` (the
/// default, Popular right now), `new`, `trending` or an Apple genre id; `country` (v3) any Apple
/// Music country, the caller's own when not sent. Always JSON.
pub async fn get_top_chart(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    if let Some(refused) = refuse_unless_signed_in(&state, &call, "json").await? {
        return Ok(refused);
    }
    let chart = call
        .param_opt("chart")
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .unwrap_or(OVERALL)
        .to_ascii_lowercase();
    if chart_settings::kind(&chart).is_none() {
        return Ok(state
            .subsonic_response_builder
            .create_error("json", 10, "Unknown chart.")
            .into_response());
    }
    let Some(service) = top_songs(&state) else {
        return Ok(empty_answer(&state));
    };
    let username = signed_in_user(&state, &call).await?;
    let (country, work) = match asked_country(&state, &call, service, &username, &chart, false).await {
        Ok(asked) => asked,
        Err(refused) => return Ok(refused),
    };
    let list = service
        .chart_in_counted(country, &chart, &LaneCell::now(), work)
        .await;
    let count = count(&call, TOP_SONGS_DEFAULT_COUNT, chart_settings::max_songs(&chart));
    Ok(answer(&state, &call, &list, count).await)
}

/// octoTopSongs v2: the charts a country has, Popular right now first, then Best New Songs and
/// Trending Songs, then the genres in Apple's order, each with whether it is a playlist for
/// everyone and, while it is one, this listener's playlist id. `country` (v3) is any Apple Music
/// country, the caller's own when not sent. Always JSON.
pub async fn get_charts(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    if let Some(refused) = refuse_unless_signed_in(&state, &call, "json").await? {
        return Ok(refused);
    }
    let Some(service) = top_songs(&state) else {
        return Ok(no_charts(&state));
    };
    let username = signed_in_user(&state, &call).await?;
    let (country, work) = match asked_country(&state, &call, service, &username, OVERALL, true).await {
        Ok(asked) => asked,
        Err(refused) => return Ok(refused),
    };
    Ok(charts_answer(&state, service, &username, country, work).await)
}

/// getCharts while search discovery is off: no charts.
fn no_charts(state: &AppState) -> Response {
    state
        .subsonic_response_builder
        .create_charts_response(
            &ChartsHeader {
                setting: "auto",
                ..Default::default()
            },
            &[],
        )
        .into_response()
}

/// octoTopSongs v3: saves the caller's own chart country (`country`, any Apple Music country, or
/// `auto` to follow the server's) and answers like getCharts. A pinned country that is now their
/// own is no longer pinned.
pub async fn set_chart_country(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    if let Some(refused) = refuse_unless_signed_in(&state, &call, "json").await? {
        return Ok(refused);
    }
    let username = match listener(&state, &call).await? {
        Ok(username) => username,
        Err(refused) => return Ok(refused),
    };
    let code = match sent_country(&state, &call) {
        Ok(code) => code,
        Err(refused) => return Ok(refused),
    };
    let service = &state.top_songs;
    let mut work = OtherWork::Free;
    let home = if code == "auto" {
        service.country().await
    } else {
        if !service.has_country(&code) {
            return Ok(refuse(&state, 10, "Unknown country."));
        }
        match service.begin_other_work(
            &counted_who(&username, &call),
            &code,
            &TopSongsService::menu_work(&code),
        ) {
            Some(started) => work = started,
            None => return Ok(refuse(&state, 0, STILL_LOADING_COUNTRIES)),
        }
        Some(code.clone())
    };
    let _ = service.listeners().update(&username, |mine| {
        mine.country = if code == "auto" {
            String::new()
        } else {
            code.clone()
        };
        mine.pinned.retain(|pin| Some(pin) != home.as_ref());
        Ok::<(), ()>(())
    });
    info!("Chart country for {username}: {code}");
    Ok(match top_songs(&state) {
        Some(service) => charts_answer(&state, service, &username, None, work).await,
        None => no_charts(&state),
    })
}

/// octoTopSongs v3: pins one more country for the caller (`country`, `pinned=true`), whose charts
/// they then get as playlists too, or unpins it (`pinned=false`), on the list as it is now, so
/// two apps pinning at once never undo each other. Answers like getCharts.
pub async fn pin_chart_country(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    if let Some(refused) = refuse_unless_signed_in(&state, &call, "json").await? {
        return Ok(refused);
    }
    let username = match listener(&state, &call).await? {
        Ok(username) => username,
        Err(refused) => return Ok(refused),
    };
    let code = match sent_country(&state, &call) {
        Ok(code) => code,
        Err(refused) => return Ok(refused),
    };
    let pin = match call.param_opt("pinned").map(|p| p.trim().to_ascii_lowercase()) {
        Some(p) if p == "true" || p == "1" => true,
        Some(p) if p == "false" || p == "0" => false,
        Some(_) => return Ok(refuse(&state, 10, "pinned is true or false.")),
        None => return Ok(refuse(&state, 10, "Required parameter is missing: pinned")),
    };
    let service = &state.top_songs;
    let mut work = OtherWork::Free;
    if pin {
        if !service.has_country(&code) {
            return Ok(refuse(&state, 10, "Unknown country."));
        }
        let home = service.country_of(&username).await;
        if home.as_deref() == Some(code.as_str()) {
            return Ok(refuse(&state, 10, "That is already your chart country."));
        }
        let mine = service.listeners().get(&username);
        if !mine.pinned.contains(&code) {
            if mine.pinned.len() >= MAX_PINNED {
                return Ok(refuse(
                    &state,
                    10,
                    &format!("You can pin at most {MAX_PINNED} countries."),
                ));
            }
            match service.begin_other_work(
                &counted_who(&username, &call),
                &code,
                &TopSongsService::menu_work(&code),
            ) {
                Some(started) => work = started,
                None => return Ok(refuse(&state, 0, STILL_LOADING_COUNTRIES)),
            }
        }
    }
    let saved = service.listeners().update(&username, |mine| {
        if !pin {
            mine.pinned.retain(|p| *p != code);
        } else if !mine.pinned.contains(&code) {
            if mine.pinned.len() >= MAX_PINNED {
                return Err(());
            }
            mine.pinned.push(code.clone());
        }
        Ok(())
    });
    let Ok(saved) = saved else {
        return Ok(refuse(
            &state,
            10,
            &format!("You can pin at most {MAX_PINNED} countries."),
        ));
    };
    info!(
        "Chart countries pinned for {username}: {}",
        if saved.pinned.is_empty() {
            "none".to_string()
        } else {
            saved.pinned.join(", ")
        }
    );
    // A country just pinned starts loading its genre menu, counted until that ends.
    if pin {
        service.menu_in_counted(Some(code.clone()), work).await;
    }
    Ok(match top_songs(&state) {
        Some(service) => charts_answer(&state, service, &username, None, OtherWork::Free).await,
        None => no_charts(&state),
    })
}

/// getCharts' answer for a country (None: the listener's own). A chart that is on is this
/// listener's playlist when the country is their own or one they pinned. `work` is what loading
/// the country's menu counts as.
async fn charts_answer(
    state: &AppState,
    service: &TopSongsService,
    username: &str,
    asked: Option<String>,
    work: OtherWork,
) -> Response {
    let own = service.country_of(username).await;
    let mine = service.listeners().get(username);
    let asked = asked.or_else(|| own.clone());
    // Whose lists these charts are: the listener's own, a pinned country's, or none.
    let pinned = asked
        .as_deref()
        .filter(|c| asked != own && mine.pinned.iter().any(|p| p == c))
        .map(str::to_string);
    let theirs = asked == own || pinned.is_some();
    let (country, menu) = service.menu_in_counted(asked, work).await;
    let lists = state.settings.current().generated_playlists.clone();
    let on: Vec<String> = lists.chart_ids();
    let mut ids: Vec<String> = vec![
        OVERALL.into(),
        chart_settings::NEW_SONGS.into(),
        chart_settings::TRENDING.into(),
    ];
    ids.extend(menu.iter().map(|g| g.id.clone()).filter(|id| id != OVERALL));
    let rows: Vec<ChartChoiceRow> = ids
        .iter()
        .filter_map(|id| {
            let (name, label) = TopSongsService::names(&menu, id)?;
            let kind = match chart_settings::kind(id)? {
                ChartKind::Overall => "overall",
                ChartKind::NewSongs => "new",
                ChartKind::Trending => "trending",
                ChartKind::Genre => "genre",
            };
            let is_on = if id == OVERALL {
                lists.popular_now
            } else {
                on.contains(id)
            };
            Some(ChartChoiceRow {
                id: id.clone(),
                name,
                label,
                kind,
                playlist: (is_on && theirs)
                    .then(|| {
                        state
                            .chart_playlists
                            .listed_id(username, &list_id(id, pinned.as_deref()))
                    })
                    .flatten(),
                on: is_on,
            })
        })
        .collect();
    let setting = if mine.country.is_empty() {
        "auto"
    } else {
        mine.country.as_str()
    };
    let server = service.country().await;
    let countries = service.apple().countries();
    let header = ChartsHeader {
        country: country.as_deref(),
        setting,
        pinned: &mine.pinned,
        server: server.as_deref(),
        countries: &countries,
        max_pinned: MAX_PINNED,
    };
    state
        .subsonic_response_builder
        .create_charts_response(&header, &rows)
        .into_response()
}

/// The standard getTopSongs (`artist` by name, `count`), for every Subsonic client, and by the
/// artist's `id` instead of the name. Navidrome answers it from the library alone, often with
/// nothing. Octo answers with the artist's songs ranked as getArtistTopSongs ranks them: a song
/// the caller's library has is the library's own entry, and the rest are outside songs that play
/// and are added with a heart. Outside songs follow search discovery. When Octo has nothing to
/// give, Navidrome's own answer is passed on, as before.
pub async fn get_top_songs(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let format = call.param_opt("f").unwrap_or("xml").to_string();
    let mut name = call.param_opt("artist").unwrap_or("").trim().to_string();
    let artist_id = call.param_opt("id").unwrap_or("").trim().to_string();
    // A missing artist is Navidrome's to refuse, in its own words.
    if name.is_empty() && artist_id.is_empty() {
        return Ok(no_top_songs(&state, &call, &format).await);
    }
    if let Some(refused) = refuse_unless_signed_in(&state, &call, &format).await? {
        return Ok(refused);
    }
    if name.is_empty() {
        name = artist_name(&state, &call, &artist_id).await.unwrap_or_default();
    }
    if name.is_empty() {
        return Ok(no_top_songs(&state, &call, &format).await);
    }

    let count = count(&call, STANDARD_TOP_SONGS_DEFAULT_COUNT, MAX_ROWS);
    let list = state
        .top_songs
        .for_artist(&name, Some(artist_id.as_str()).filter(|id| !id.is_empty()))
        .await;
    let settings = state.settings.current();
    let outside = settings.subsonic.enable_search_discovery;
    let skip_covers_and_instrumentals = settings.soulseek.skip_covers_and_instrumentals;

    // With outside songs shown, only the rows asked for are looked up in the library, and the
    // rows Skip covers and instrumentals would leave out beside them. Without outside songs every row is,
    // because the library's rows are all that is left to list.
    let candidates: Vec<_> = if outside {
        TopSongsService::rows_to_match(&list.songs, count, skip_covers_and_instrumentals)
    } else {
        list.songs.clone()
    };
    let username = listener_name(&state, &call).await;
    let library = if candidates.is_empty() {
        Vec::new()
    } else {
        let session = state
            .library_matches
            .session(&call.proxy, &call.parameters, &username)
            .await;
        TopSongsService::match_library_with(
            &candidates,
            |query| library_songs_matching(&call.proxy, &call.parameters, query),
            &session,
        )
        .await
    };

    // A Clean only listener never gets an outside song whose words are not known yet (its lookup
    // got no answer): the kid filter reads the word clients see, and such a song has none.
    let clean_only = crate::services::family::kid_filter::explicit_filter_for(
        &settings,
        Some(&state.family_store),
        Some(&username),
    ) == octo_core::settings::ExplicitFilter::CleanOnly;
    let mut rows: Vec<TopSongRow> = Vec::new();
    for (i, top) in candidates.iter().enumerate() {
        if rows.len() >= count {
            break;
        }
        match library.get(i).cloned().flatten() {
            Some(owned) => rows.push(TopSongRow::Library(owned)),
            None if outside && !skipped_takes::hides_song(&top.song, skip_covers_and_instrumentals) => {
                let song = synced_song(&state, &username, &top.song);
                if clean_only
                    && song.explicit_content_lyrics
                        == Some(octo_core::models::domain::explicit_status::UNANSWERED)
                {
                    continue;
                }
                rows.push(TopSongRow::Outside(song));
            }
            None => {}
        }
    }
    if rows.is_empty() {
        return Ok(no_top_songs(&state, &call, &format).await);
    }
    let in_library = rows
        .iter()
        .filter(|r| matches!(r, TopSongRow::Library(_)))
        .count();
    info!(
        "getTopSongs '{name}' ({}): {in_library} library and {} outside songs, ranked by {}",
        call.param_opt("c").unwrap_or(""),
        rows.len() - in_library,
        list.source
    );
    Ok(state
        .subsonic_response_builder
        .create_standard_top_songs_response(&format, &rows)
        .into_response())
}

/// The answer when Octo has none: Navidrome's own, except for an outside artist's id, which
/// Navidrome has never heard of. That gets an empty list.
async fn no_top_songs(state: &AppState, call: &SubsonicCall, format: &str) -> Response {
    let builder = &state.subsonic_response_builder;
    if has_external_id(state, &call.parameters) {
        return builder
            .create_standard_top_songs_response(format, &[])
            .into_response();
    }
    match call.proxy.relay_safe("rest/getTopSongs", &call.parameters).await {
        Some(relay) if !relay.body.is_empty() => {
            let content_type = relay
                .content_type
                .unwrap_or_else(|| format!("application/{format}"));
            ([(axum::http::header::CONTENT_TYPE, content_type)], relay.body).into_response()
        }
        _ => builder
            .create_error(format, 0, "Unable to reach Navidrome")
            .into_response(),
    }
}

/// The name of the artist an id names: an outside artist's from what Octo remembers of it, a
/// library artist's from Navidrome, asked as the caller. None when neither knows.
async fn artist_name(state: &AppState, call: &SubsonicCall, artist_id: &str) -> Option<String> {
    if let Some(routing) = state.external_id_registry.lookup(artist_id) {
        let routing = routing.snapshot();
        if routing.kind == RoutingKind::Artist
            && let Some(name) = routing.artist.filter(|a| !a.is_empty())
        {
            return Some(name);
        }
    }
    if has_external_id(state, &call.parameters) {
        return None;
    }
    let mut lookup = call.parameters.clone();
    lookup.insert("f".into(), "json".into());
    lookup.shift_remove("count");
    let relay = call.proxy.relay_safe("rest/getArtist", &lookup).await?;
    let doc: Value = serde_json::from_slice(&relay.body).ok()?;
    doc.pointer("/subsonic-response/artist/name")
        .and_then(Value::as_str)
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
}

/// An outside song as this listener's sync catalog describes it, when it holds the song: a
/// client that copies the library keeps whichever description it read last, so the two must
/// agree, as on a station playlist.
fn synced_song(state: &AppState, username: &str, song: &Song) -> Song {
    if song.is_local {
        return song.clone();
    }
    state
        .sync_catalog
        .try_get_song(username, &song.id)
        .unwrap_or_else(|| song.clone())
}

/// The first `count` rows, each matched against the caller's own library. While Skip covers,
/// karaoke and instrumentals is on, an outside cover, karaoke or instrumental take is left out and
/// the next row takes its place.
async fn answer(state: &AppState, call: &SubsonicCall, list: &TopList, count: usize) -> Response {
    let skip_covers_and_instrumentals = state.settings.current().soulseek.skip_covers_and_instrumentals;
    let candidates = TopSongsService::rows_to_match(&list.songs, count, skip_covers_and_instrumentals);
    let username = listener_name(state, call).await;
    let session = state
        .library_matches
        .session(&call.proxy, &call.parameters, &username)
        .await;
    let library = TopSongsService::match_library_with(
        &candidates,
        |query| library_songs_matching(&call.proxy, &call.parameters, query),
        &session,
    )
    .await;
    let (shown, library) =
        TopSongsService::shown_rows(candidates, library, count, skip_covers_and_instrumentals);
    let rows: Vec<RankedRow<'_>> = shown
        .iter()
        .enumerate()
        .map(|(i, top)| RankedRow {
            rank: top.rank,
            song: &top.song,
            plays: top.plays,
            listeners: top.listeners,
            library: library.get(i).and_then(Option::as_ref),
        })
        .collect();
    let header = TopSongsHeader {
        artist: list.artist.as_deref(),
        source: &list.source,
        chart: list.chart.as_deref(),
        name: list.name.as_deref(),
        country: list.country.as_deref(),
    };
    state
        .subsonic_response_builder
        .create_top_songs_response(&header, &rows)
        .into_response()
}

fn empty_answer(state: &AppState) -> Response {
    state
        .subsonic_response_builder
        .create_top_songs_response(&TopSongsHeader::default(), &[])
        .into_response()
}

/// `count`, when given and above 0, at most `max`; else the default.
fn count(call: &SubsonicCall, default: usize, max: usize) -> usize {
    call.param_opt("count")
        .and_then(|c| c.trim().parse::<usize>().ok())
        .filter(|c| *c > 0)
        .map_or(default, |c| c.min(max))
}

/// Navidrome's songs for a query, searched as the caller: Some (maybe empty) when it answered,
/// None when it did not. Nothing outside the library: this goes to Navidrome, not through Octo's
/// search.
pub(crate) async fn library_songs_matching(
    proxy: &SubsonicProxyService,
    parameters: &Parameters,
    query: String,
) -> Option<Vec<Value>> {
    let mut search = parameters.clone();
    search.insert("query".into(), query.clone());
    search.insert("songCount".into(), "5".into());
    search.insert("albumCount".into(), "0".into());
    search.insert("artistCount".into(), "0".into());
    search.insert("f".into(), "json".into());
    search.shift_remove("count");
    search.shift_remove("id");
    search.shift_remove("artist");
    search.shift_remove("chart");
    let Some(relay) = proxy.relay_safe("rest/search3", &search).await else {
        debug!("top songs: library search '{query}' had no answer");
        return None;
    };
    let doc: Value = serde_json::from_slice(&relay.body).ok()?;
    let response = doc.get("subsonic-response")?;
    if response.get("status").and_then(Value::as_str) != Some("ok") {
        return None;
    }
    Some(
        response
            .pointer("/searchResult3/song")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    )
}

#[cfg(test)]
#[path = "top_songs_tests.rs"]
mod tests;
