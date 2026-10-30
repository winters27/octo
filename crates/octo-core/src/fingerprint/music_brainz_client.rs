//! The pure half of the MusicBrainz client: the query builders and the readers of its answers.
//! The HTTP half (the one-a-second gate and the cache) is
//! `octo::services::fingerprint::music_brainz_client`.

use serde_json::Value;

use crate::common::SongIdentity;
use crate::common::dotnet;
use crate::fingerprint::track_match_comparer::TrackMatchComparer;
use crate::json::element::{
    ElementResult, enumerate_array, get_double, get_int32, get_string, try_get_property,
};

/// The search for a recording by name, with a length window of ten seconds either way when
/// the length is known. Every name goes through [`escape_query`], since a quote, a colon or a
/// slash in a title would otherwise change what the query means and the failure would read as
/// "no candidate".
pub fn build_recording_search_url(artist: &str, title: &str, duration_seconds: i32) -> String {
    let mut parts = Vec::new();
    if !dotnet::is_blank(title) {
        parts.push(format!("recording:\"{}\"", escape_query(title.trim())));
    }
    if !dotnet::is_blank(artist) {
        parts.push(format!("artist:\"{}\"", escape_query(artist.trim())));
    }
    if duration_seconds > 0 {
        let low = (duration_seconds - 10).max(0) * 1000;
        let high = (duration_seconds + 10) * 1000;
        parts.push(format!("dur:[{low} TO {high}]"));
    }
    format!(
        "recording/?query={}&fmt=json&limit=25",
        dotnet::escape_data_string(&parts.join(" AND "))
    )
}

/// Every character the query language reads as an operator, made literal.
pub fn escape_query(value: &str) -> String {
    const SPECIAL: &str = "+-&|!(){}[]^\"~*?:\\/";
    let mut sb = String::with_capacity(value.len() + 8);
    for ch in value.chars() {
        if SPECIAL.contains(ch) {
            sb.push('\\');
        }
        sb.push(ch);
    }
    sb
}

/// The quoting the recording lookup uses: backslashes and quotes only.
pub fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The relative URL the recording lookup asks, or None when the song cannot be asked for.
pub fn find_recording_url(artist: &str, title: &str, duration_seconds: i32) -> Option<String> {
    if dotnet::is_blank(artist) || dotnet::is_blank(title) || duration_seconds <= 0 {
        return None;
    }
    let query = format!(
        "recording:\"{}\" AND artist:\"{}\"",
        escape(title),
        escape(artist)
    );
    Some(format!(
        "recording/?query={}&fmt=json&limit=25",
        dotnet::escape_data_string(&query)
    ))
}

/// The relative URL of one artist by MusicBrainz id, or None for a blank id.
pub fn artist_lookup_url(mbid: &str) -> Option<String> {
    if dotnet::is_blank(mbid) {
        return None;
    }
    Some(format!(
        "artist/{}?fmt=json",
        dotnet::escape_data_string(mbid.trim())
    ))
}

/// The relative URL of the search for an artist by name or alias: the best three, or None for
/// a name with nothing to search by.
pub fn artist_search_url(name: &str) -> Option<String> {
    if SongIdentity::key(name).is_empty() {
        return None;
    }
    let escaped = escape_query(name.trim());
    let query = format!("artist:\"{escaped}\" OR alias:\"{escaped}\"");
    Some(format!(
        "artist/?query={}&limit=3&fmt=json",
        dotnet::escape_data_string(&query)
    ))
}

/// Where one artist is from, as a known two-letter country: `country`, else the first of
/// `area.iso-3166-1-codes`. Empty when MusicBrainz names no country, or names a region that is
/// not one ("XW", "XE").
pub fn artist_country(artist: &Value) -> String {
    let own = artist.get("country").and_then(Value::as_str);
    let area = artist
        .get("area")
        .and_then(|area| area.get("iso-3166-1-codes"))
        .and_then(Value::as_array)
        .and_then(|codes| codes.first())
        .and_then(Value::as_str);
    own.and_then(crate::common::countries::normalize)
        .or_else(|| area.and_then(crate::common::countries::normalize))
        .unwrap_or_default()
        .to_string()
}

/// The country of the artist a name search found: the one result scoring at least 90 whose
/// name, or one of whose aliases, is the same name as `name` ([`SongIdentity::key`]). Empty
/// when no result is that artist, or when two artists of that name both qualify: a near name is
/// another artist, a shared name cannot tell which is meant, and a wrong country is worse than
/// none.
pub fn searched_artist_country(root: &Value, name: &str) -> String {
    let wanted = SongIdentity::key(name);
    let Some(artists) = root.get("artists").and_then(Value::as_array) else {
        return String::new();
    };
    let same = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .is_some_and(|text| SongIdentity::key(text) == wanted)
    };
    let mut found: Option<(&str, &Value)> = None;
    for artist in artists {
        let score = artist.get("score").and_then(Value::as_i64).unwrap_or(0);
        if score < 90 {
            continue;
        }
        let aliased = artist
            .get("aliases")
            .and_then(Value::as_array)
            .is_some_and(|aliases| aliases.iter().any(|alias| same(alias.get("name"))));
        if !(same(artist.get("name")) || aliased) {
            continue;
        }
        let id = artist.get("id").and_then(Value::as_str).unwrap_or("");
        match found {
            None => found = Some((id, artist)),
            Some((known, _)) if !id.is_empty() && known.eq_ignore_ascii_case(id) => {}
            Some(_) => return String::new(),
        }
    }
    found
        .map(|(_, artist)| artist_country(artist))
        .unwrap_or_default()
}

/// Whether a release date ("2009", "2009-09" or "2009-09-08") is earlier than a known one.
/// Only the part both dates have is compared, so "2009" is not earlier than "2009-09-08";
/// when that part is the same, the more precise date wins.
pub(crate) fn is_earlier_date(date: &str, known: &str) -> bool {
    let (date, known) = (date.as_bytes(), known.as_bytes());
    let shared = date.len().min(known.len());
    match date[..shared].cmp(&known[..shared]) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => date.len() > known.len(),
    }
}

/// The "isrcs" list of a recording lookup, each one normalized; invalid ones dropped.
pub fn parse_isrcs(root: &Value) -> ElementResult<Vec<String>> {
    let mut codes: Vec<String> = Vec::new();
    if let Some(isrcs @ Value::Array(_)) = try_get_property(root, "isrcs")? {
        for isrc in enumerate_array(isrcs)? {
            if let Value::String(text) = isrc
                && let Some(code) = SongIdentity::normalize_isrc(text)
                && !codes.contains(&code)
            {
                codes.push(code);
            }
        }
    }
    Ok(codes)
}

/// The one recording that is this song, this version, by this artist, at this length.
pub fn pick(root: &Value, artist: &str, title: &str, duration_seconds: i32) -> ElementResult<Option<String>> {
    let Some(recordings @ Value::Array(_)) = try_get_property(root, "recordings")? else {
        return Ok(None);
    };

    // A HashSet with OrdinalIgnoreCase: the first spelling seen is kept.
    let mut ids: Vec<String> = Vec::new();
    for recording in enumerate_array(recordings)? {
        if let Some(score @ Value::Number(_)) = try_get_property(recording, "score")?
            && get_int32(score)? < 90
        {
            continue;
        }
        if let Some(Value::Bool(true)) = try_get_property(recording, "video")? {
            continue;
        }
        let Some(length @ Value::Number(_)) = try_get_property(recording, "length")? else {
            continue;
        };
        if (get_double(length)? / 1000.0 - f64::from(duration_seconds)).abs() > 3.0 {
            continue;
        }

        let name = match try_get_property(recording, "title")? {
            Some(t) => get_string(t)?.unwrap_or(""),
            None => "",
        };
        let disambiguation = match try_get_property(recording, "disambiguation")? {
            Some(d) => get_string(d)?.unwrap_or(""),
            None => "",
        };
        // A live take or a remix says so in its disambiguation, not always in its title.
        let described = if disambiguation.is_empty() {
            name.to_string()
        } else {
            format!("{name} ({disambiguation})")
        };
        if !SongIdentity::same_title(title, &described, Some(&SongIdentity::strict_titles())).is_same() {
            continue;
        }

        let mut credits: Vec<String> = Vec::new();
        if let Some(credit @ Value::Array(_)) = try_get_property(recording, "artist-credit")? {
            for entry in enumerate_array(credit)? {
                let name = match try_get_property(entry, "name")? {
                    Some(n) => get_string(n)?.unwrap_or(""),
                    None => "",
                };
                if !name.is_empty() {
                    credits.push(name.to_string());
                }
            }
        }
        if !TrackMatchComparer::artist_matches(artist, &credits.join(" & "), &credits) {
            continue;
        }

        if let Some(id) = try_get_property(recording, "id")?
            && let Some(recording_id) = get_string(id)?.filter(|id| !id.is_empty())
            && !ids
                .iter()
                .any(|known| dotnet::eq_ignore_case(known, recording_id))
        {
            ids.push(recording_id.to_string());
        }
    }
    Ok(if ids.len() == 1 { ids.pop() } else { None })
}

// ---- artists by name, and the credits of recordings -----------------------------------------

/// One MusicBrainz artist: its id and the name MusicBrainz spells it with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MusicBrainzArtist {
    pub id: String,
    pub name: String,
}

/// One artist of a recording's credit, and the words that join it to the next ("" for the
/// last, which MusicBrainz leaves out).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CreditPart {
    pub name: String,
    #[serde(default)]
    pub join_phrase: String,
}

/// The search for artists by a name, under their name or any alias.
pub fn artists_named_url(name: &str) -> String {
    let wanted = escape_query(name.trim());
    let query = format!("artist:\"{wanted}\" OR alias:\"{wanted}\"");
    format!(
        "artist/?query={}&fmt=json&limit=25",
        dotnet::escape_data_string(&query)
    )
}

/// The artists of a search answer named `name`, by their name or one of their aliases, read the
/// way Octo reads names (case, accents, "&" and "and" aside). The search ranks near matches
/// alongside, and a resemblance is somebody else, so those are dropped.
pub fn parse_artists_named(root: &Value, name: &str) -> Vec<MusicBrainzArtist> {
    let wanted = SongIdentity::key(name);
    let mut found: Vec<MusicBrainzArtist> = Vec::new();
    if wanted.is_empty() {
        return found;
    }
    let Some(artists) = root.get("artists").and_then(Value::as_array) else {
        return found;
    };
    let text =
        |value: &Value, field: &str| value.get(field).and_then(Value::as_str).unwrap_or("").to_string();
    for artist in artists {
        let id = text(artist, "id");
        let artist_name = text(artist, "name");
        if id.is_empty() || found.iter().any(|known| known.id.eq_ignore_ascii_case(&id)) {
            continue;
        }
        let by_name = SongIdentity::key(&artist_name) == wanted;
        let by_alias = artist
            .get("aliases")
            .and_then(Value::as_array)
            .is_some_and(|aliases| {
                aliases
                    .iter()
                    .any(|alias| SongIdentity::key(&text(alias, "name")) == wanted)
            });
        if by_name || by_alias {
            found.push(MusicBrainzArtist {
                id,
                name: artist_name,
            });
        }
    }
    found
}

/// Whether `id` looks like a MusicBrainz id, so it can go into a query as it is.
pub fn is_mbid(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(at, b)| match at {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// One search for the credits of up to 100 recordings by id. Ids that are not MusicBrainz ids
/// are left out.
pub fn recording_credits_url(ids: &[String]) -> Option<String> {
    let ids: Vec<String> = ids
        .iter()
        .filter(|id| is_mbid(id))
        .map(|id| id.to_lowercase())
        .collect();
    if ids.is_empty() {
        return None;
    }
    let query = format!("rid:({})", ids.join(" OR "));
    Some(format!(
        "recording/?query={}&fmt=json&limit=100",
        dotnet::escape_data_string(&query)
    ))
}

/// Each recording of a search answer with its artist credit, by lowercase id.
pub fn parse_recording_credits(root: &Value) -> std::collections::HashMap<String, Vec<CreditPart>> {
    let mut credits = std::collections::HashMap::new();
    let Some(recordings) = root.get("recordings").and_then(Value::as_array) else {
        return credits;
    };
    for recording in recordings {
        let Some(id) = recording
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let parts: Vec<CreditPart> = recording
            .get("artist-credit")
            .and_then(Value::as_array)
            .map(|credit| {
                credit
                    .iter()
                    .filter_map(|entry| {
                        let name = entry.get("name").and_then(Value::as_str).or_else(|| {
                            entry
                                .get("artist")
                                .and_then(|artist| artist.get("name"))
                                .and_then(Value::as_str)
                        })?;
                        Some(CreditPart {
                            name: name.to_string(),
                            join_phrase: entry
                                .get("joinphrase")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        credits.insert(id.to_lowercase(), parts);
    }
    credits
}

#[cfg(test)]
#[path = "music_brainz_client_tests.rs"]
mod tests;

/// The lookup of one recording with every release it is on and their groups: what the music
/// database says the recording is, beyond what AcoustID sends.
pub fn recording_facts_url(recording_id: &str) -> Option<String> {
    is_mbid(recording_id).then(|| {
        format!(
            "recording/{}?inc=releases+release-groups&fmt=json",
            dotnet::escape_data_string(recording_id)
        )
    })
}

/// A recording lookup's title, disambiguation, length and releases (each with its status and
/// its group's title and secondary types).
pub fn parse_recording_facts(
    root: &Value,
) -> ElementResult<crate::fingerprint::recording_identity::RecordingFacts> {
    use crate::fingerprint::recording_identity::{FactRelease, RecordingFacts};
    use crate::json::element::{str_prop, string_prop};

    let mut releases = Vec::new();
    if let Some(list @ Value::Array(_)) = try_get_property(root, "releases")? {
        for release in enumerate_array(list)? {
            let group = try_get_property(release, "release-group")?;
            let mut secondary = Vec::new();
            if let Some(group) = group
                && let Some(types @ Value::Array(_)) = try_get_property(group, "secondary-types")?
            {
                for kind in enumerate_array(types)? {
                    if let Some(kind) = get_string(kind)? {
                        secondary.push(kind.to_string());
                    }
                }
            }
            releases.push(FactRelease {
                title: str_prop(release, "title")?.unwrap_or("").to_string(),
                status: string_prop(release, "status")?,
                group_title: match group {
                    Some(group) => string_prop(group, "title")?,
                    None => None,
                },
                group_secondary_types: secondary,
            });
        }
    }
    let length_ms = match try_get_property(root, "length")? {
        Some(length @ Value::Number(_)) => Some(get_double(length)? as i64).filter(|ms| *ms > 0),
        _ => None,
    };
    Ok(RecordingFacts {
        recording_id: str_prop(root, "id")?.unwrap_or("").to_string(),
        title: str_prop(root, "title")?.unwrap_or("").to_string(),
        disambiguation: string_prop(root, "disambiguation")?.filter(|text| !dotnet::is_blank(text)),
        length_ms,
        releases,
    })
}
