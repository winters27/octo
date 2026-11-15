//! Merging what Navidrome holds into a held playlist (pure). Navidrome's library list is
//! diffed against the list Octo last saw (`base`) with a Myers diff (no size cap, linear space);
//! equal runs keep their entries, a song moved elsewhere keeps its entry where Navidrome put it,
//! a song replaced by the same take in the same gap keeps its place and fallback, a removed
//! entry with an outside id streams again when its library file is gone, and every new song is
//! placed before the next one that has an entry. The library part of the result always equals
//! Navidrome's list.

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Utc};
use similar::{Algorithm, DiffOp, capture_diff_slices};

use super::held_store::{HeldEntry, SongSnapshot};
use super::same_take::{TAKE_SECONDS, TakeFacts, same_take};
use crate::services::library::navidrome_playlist_api::TrackFacts;

/// One row of Navidrome's list as the admin read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRow {
    pub position: String,
    pub id: String,
    pub title: String,
    pub artist: String,
    pub seconds: Option<i32>,
    pub missing: bool,
}

impl RawRow {
    fn facts(&self) -> TakeFacts {
        TakeFacts {
            title: self.title.clone(),
            artist: self.artist.clone(),
            seconds: self.seconds,
            ..Default::default()
        }
    }
}

impl From<&TrackFacts> for RawRow {
    fn from(track: &TrackFacts) -> Self {
        RawRow {
            position: track.position.clone(),
            id: track.media_file_id.clone(),
            title: track.title.clone(),
            artist: track.artist.clone(),
            seconds: track.seconds,
            missing: track.missing,
        }
    }
}

/// Why a merge changed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeError {
    /// Whether a removed song's file is still there could not be told.
    Unreachable,
}

/// What the diff made of each side. Library entries are numbered in order, the k-th one
/// standing for `base[k]`.
struct Plan {
    /// For each library entry: the raw row it now stands at (equal or the same take in its gap).
    base_to_raw: Vec<Option<usize>>,
    /// Raw rows that stand for an entry in place.
    anchored: Vec<bool>,
    /// For each library entry: the raw row Navidrome moved it to.
    moved_to: Vec<Option<usize>>,
    /// For each raw row: the library entry moved there.
    carried: Vec<Option<usize>>,
}

fn plan(entries: &[HeldEntry], base: &[String], raw: &[RawRow]) -> Plan {
    let library: Vec<&HeldEntry> = entries.iter().filter(|e| e.library_id.is_some()).collect();
    let base_ids: Vec<&str> = base.iter().map(String::as_str).collect();
    let raw_ids: Vec<&str> = raw.iter().map(|row| row.id.as_str()).collect();
    let mut plan = Plan {
        base_to_raw: vec![None; base.len()],
        anchored: vec![false; raw.len()],
        moved_to: vec![None; base.len()],
        carried: vec![None; raw.len()],
    };
    // Each gap between equal runs: the base entries it removed and the raw rows it inserted.
    let mut hunks: Vec<(Vec<usize>, Vec<usize>)> = Vec::new();
    let mut hunk: (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
    for op in capture_diff_slices(Algorithm::Myers, &base_ids, &raw_ids) {
        match op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                if !hunk.0.is_empty() || !hunk.1.is_empty() {
                    hunks.push(std::mem::take(&mut hunk));
                }
                for (b, r) in (old_index..old_index + len).zip(new_index..new_index + len) {
                    plan.base_to_raw[b] = Some(r);
                    plan.anchored[r] = true;
                }
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => hunk.0.extend(old_index..old_index + old_len),
            DiffOp::Insert {
                new_index, new_len, ..
            } => hunk.1.extend(new_index..new_index + new_len),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                hunk.0.extend(old_index..old_index + old_len);
                hunk.1.extend(new_index..new_index + new_len);
            }
        }
    }
    if !hunk.0.is_empty() || !hunk.1.is_empty() {
        hunks.push(hunk);
    }

    // A song moved elsewhere in the list keeps its entry, wherever Navidrome put it.
    let mut removed_by_id: HashMap<&str, VecDeque<usize>> = HashMap::new();
    for (removed, _) in &hunks {
        for &b in removed {
            removed_by_id.entry(base_ids[b]).or_default().push_back(b);
        }
    }
    for (_, inserted) in &hunks {
        for &r in inserted {
            if let Some(b) = removed_by_id.get_mut(raw_ids[r]).and_then(VecDeque::pop_front) {
                plan.moved_to[b] = Some(r);
                plan.carried[r] = Some(b);
            }
        }
    }

    // Within each gap, a removed song pairs with the first later same take Navidrome put there,
    // in order, so the pairs keep the list's order.
    for (removed, inserted) in &hunks {
        let candidates: Vec<(usize, TakeFacts)> = inserted
            .iter()
            .copied()
            .filter(|&r| plan.carried[r].is_none())
            .map(|r| (r, raw[r].facts()))
            .collect();
        let mut from = 0;
        for &b in removed.iter().filter(|&&b| plan.moved_to[b].is_none()) {
            let Some(song) = library.get(b).and_then(|entry| entry.song.as_ref()) else {
                continue;
            };
            let facts = song.take_facts();
            let found = candidates[from..]
                .iter()
                .position(|(_, candidate)| close_lengths(&facts, candidate) && same_take(&facts, candidate));
            if let Some(at) = found {
                let r = candidates[from + at].0;
                plan.base_to_raw[b] = Some(r);
                plan.anchored[r] = true;
                from += at + 1;
            }
        }
    }
    plan
}

/// The cheap half of [`same_take`], checked first.
fn close_lengths(a: &TakeFacts, b: &TakeFacts) -> bool {
    matches!((a.seconds, b.seconds), (Some(x), Some(y)) if (x - y).abs() <= TAKE_SECONDS)
}

/// The library ids whose presence the merge needs: removed entries with an outside id that
/// neither moved nor paired. Ask Navidrome whether each file is still there before merging.
pub fn ids_needing_presence(entries: &[HeldEntry], base: &[String], raw: &[RawRow]) -> Vec<String> {
    let plan = plan(entries, base, raw);
    let mut ids: Vec<String> = Vec::new();
    for (k, entry) in entries.iter().filter(|e| e.library_id.is_some()).enumerate() {
        let removed = plan.base_to_raw.get(k).copied().flatten().is_none()
            && plan.moved_to.get(k).copied().flatten().is_none();
        if removed
            && entry.outside_id.is_some()
            && let Some(id) = &entry.library_id
            && !ids.contains(id)
        {
            ids.push(id.clone());
        }
    }
    ids
}

/// `entries` with Navidrome's list `raw` merged in, where `base` is the library list the entries
/// were last in step with (their library part). `present` answers, for each id
/// [`ids_needing_presence`] named, whether its library file is still there; a missing or `None`
/// answer for an id the merge needs is [`MergeError::Unreachable`] and nothing changes.
pub fn merge(
    entries: &[HeldEntry],
    base: &[String],
    raw: &[RawRow],
    present: &HashMap<String, Option<bool>>,
) -> Result<Vec<HeldEntry>, MergeError> {
    let plan = plan(entries, base, raw);
    let library: Vec<&HeldEntry> = entries.iter().filter(|e| e.library_id.is_some()).collect();
    let now = Utc::now();
    let mut out: Vec<HeldEntry> = Vec::with_capacity(entries.len() + raw.len());
    let mut next = 0usize;
    let mut k = 0usize;
    for entry in entries {
        let Some(library_id) = entry.library_id.as_deref() else {
            out.push(entry.clone());
            continue;
        };
        let at = k;
        k += 1;
        if let Some(r) = plan.base_to_raw.get(at).copied().flatten() {
            emit_unanchored(&plan, &library, raw, &mut next, r, now, &mut out);
            next = next.max(r + 1);
            let mut kept = entry.clone();
            kept.library_id = Some(raw[r].id.clone());
            out.push(kept);
        } else if plan.moved_to.get(at).copied().flatten().is_some() {
            // Written where Navidrome moved it.
        } else if entry.outside_id.is_some() {
            match present.get(library_id).copied().flatten() {
                None => return Err(MergeError::Unreachable),
                Some(false) => {
                    let mut streamed = entry.clone();
                    streamed.library_id = None;
                    out.push(streamed);
                }
                Some(true) => {}
            }
        }
    }
    emit_unanchored(&plan, &library, raw, &mut next, raw.len(), now, &mut out);
    Ok(out)
}

/// Writes the raw rows from `next` up to `until` that stand for no entry in place: a moved
/// entry, or a new library entry.
fn emit_unanchored(
    plan: &Plan,
    library: &[&HeldEntry],
    raw: &[RawRow],
    next: &mut usize,
    until: usize,
    now: DateTime<Utc>,
    out: &mut Vec<HeldEntry>,
) {
    for (r, row) in raw.iter().enumerate().take(until).skip(*next) {
        if plan.anchored[r] {
            continue;
        }
        let entry = match plan.carried[r].and_then(|b| library.get(b)) {
            Some(moved) => {
                let mut moved = (*moved).clone();
                moved.library_id = Some(row.id.clone());
                moved
            }
            None => new_entry(row, now),
        };
        out.push(entry);
    }
    *next = (*next).max(until);
}

fn new_entry(row: &RawRow, now: DateTime<Utc>) -> HeldEntry {
    HeldEntry {
        library_id: Some(row.id.clone()),
        outside_id: None,
        provider: None,
        external_id: None,
        key: None,
        seed_id: None,
        seed: None,
        song: Some(SongSnapshot {
            title: row.title.clone(),
            artist: row.artist.clone(),
            seconds: row.seconds,
            ..Default::default()
        }),
        added: now,
    }
}

/// The key an entry is matched by occurrence: its outside id when it has one (bound and streamed
/// entries), else its library id.
fn occurrence_key(entry: &HeldEntry) -> Option<String> {
    entry
        .outside_id
        .as_ref()
        .map(|id| format!("o:{id}"))
        .or_else(|| entry.library_id.as_ref().map(|id| format!("l:{id}")))
}

/// A kept-aside copy of a playlist moved back over the copy held now: `present` stays as it is,
/// and each kept-aside streamed entry that is not present (matched by key and occurrence, the
/// nth kept-aside occurrence to the nth present one) goes right after its nearest earlier
/// kept-aside entry that is present (one moved back counts), or at the start when it has none.
/// Library and bound entries that are not present are not added.
pub fn move_back_entries(present: &[HeldEntry], kept_aside: &[HeldEntry]) -> Vec<HeldEntry> {
    let mut places: HashMap<String, Vec<usize>> = HashMap::new();
    for (at, entry) in present.iter().enumerate() {
        if let Some(key) = occurrence_key(entry) {
            places.entry(key).or_default().push(at);
        }
    }
    // Group 0 is the start; group i + 1 follows present[i].
    let mut groups: Vec<Vec<HeldEntry>> = vec![Vec::new(); present.len() + 1];
    let mut seen: HashMap<String, usize> = HashMap::new();
    let (mut group, mut cursor) = (0usize, 0usize);
    for entry in kept_aside {
        let Some(key) = occurrence_key(entry) else {
            continue;
        };
        let occurrence = {
            let count = seen.entry(key.clone()).or_insert(0);
            let n = *count;
            *count += 1;
            n
        };
        if let Some(&at) = places.get(&key).and_then(|list| list.get(occurrence)) {
            group = at + 1;
            cursor = 0;
            continue;
        }
        if entry.streams() {
            groups[group].insert(cursor, entry.clone());
            cursor += 1;
        }
    }
    let mut out: Vec<HeldEntry> = Vec::with_capacity(present.len() + kept_aside.len());
    let mut groups = groups.into_iter();
    out.extend(groups.next().unwrap_or_default());
    for (entry, after) in present.iter().zip(groups) {
        out.push(entry.clone());
        out.extend(after);
    }
    out
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn added() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 14, 9, 0, 0)
            .single()
            .expect("a time")
    }

    fn snap(title: &str, seconds: i32) -> SongSnapshot {
        SongSnapshot {
            title: title.into(),
            artist: "Artist".into(),
            seconds: Some(seconds),
            ..Default::default()
        }
    }

    fn lib(id: &str, title: &str) -> HeldEntry {
        HeldEntry {
            library_id: Some(id.into()),
            outside_id: None,
            provider: None,
            external_id: None,
            key: None,
            seed_id: None,
            seed: None,
            song: Some(snap(title, 200)),
            added: added(),
        }
    }

    fn bound(id: &str, outside: &str, title: &str) -> HeldEntry {
        HeldEntry {
            outside_id: Some(outside.into()),
            provider: Some("deezer".into()),
            ..lib(id, title)
        }
    }

    fn streamed(outside: &str, title: &str) -> HeldEntry {
        HeldEntry {
            library_id: None,
            ..bound("", outside, title)
        }
    }

    fn row(id: &str, title: &str, seconds: i32) -> RawRow {
        RawRow {
            position: String::new(),
            id: id.into(),
            title: title.into(),
            artist: "Artist".into(),
            seconds: Some(seconds),
            missing: false,
        }
    }

    /// Each entry by its library id, else its outside id.
    fn ids(entries: &[HeldEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| {
                e.library_id
                    .clone()
                    .or_else(|| e.outside_id.clone())
                    .unwrap_or_default()
            })
            .collect()
    }

    fn library_part(entries: &[HeldEntry]) -> Vec<String> {
        entries.iter().filter_map(|e| e.library_id.clone()).collect()
    }

    fn raw_ids(raw: &[RawRow]) -> Vec<String> {
        raw.iter().map(|r| r.id.clone()).collect()
    }

    fn merged(
        entries: &[HeldEntry],
        raw: &[RawRow],
        present: &[(&str, Option<bool>)],
    ) -> Result<Vec<HeldEntry>, MergeError> {
        let present: HashMap<String, Option<bool>> = present
            .iter()
            .map(|(id, answer)| (id.to_string(), *answer))
            .collect();
        let out = merge(entries, &library_part(entries), raw, &present)?;
        assert_eq!(
            library_part(&out),
            raw_ids(raw),
            "the library part is Navidrome's list"
        );
        Ok(out)
    }

    #[test]
    fn a_removal_drops_the_entry_and_streamed_songs_keep_their_places() {
        let entries = vec![
            lib("a", "Alpha"),
            streamed("ext-deezer-song-1", "One"),
            lib("b", "Beta"),
            lib("c", "Gamma"),
        ];
        let out = merged(&entries, &[row("a", "Alpha", 200), row("c", "Gamma", 200)], &[]).expect("merged");
        assert_eq!(ids(&out), ["a", "ext-deezer-song-1", "c"]);
    }

    #[test]
    fn an_append_lands_after_trailing_streamed_songs_and_an_insert_before_the_next_entry() {
        let entries = vec![lib("a", "Alpha"), streamed("ext-deezer-song-1", "One")];
        let out = merged(&entries, &[row("a", "Alpha", 200), row("n", "New", 190)], &[]).expect("merged");
        assert_eq!(ids(&out), ["a", "ext-deezer-song-1", "n"]);
        assert_eq!(out[2].song.as_ref().map(|s| s.title.as_str()), Some("New"));

        let entries = vec![
            lib("a", "Alpha"),
            streamed("ext-deezer-song-1", "One"),
            lib("b", "Beta"),
        ];
        let raw = [
            row("a", "Alpha", 200),
            row("n", "New", 190),
            row("b", "Beta", 200),
        ];
        let out = merged(&entries, &raw, &[]).expect("merged");
        assert_eq!(ids(&out), ["a", "ext-deezer-song-1", "n", "b"]);
    }

    #[test]
    fn a_reorder_moves_entries_with_their_fallbacks() {
        let entries = vec![
            lib("a", "Alpha"),
            streamed("ext-deezer-song-1", "One"),
            lib("b", "Beta"),
            streamed("ext-deezer-song-2", "Two"),
            bound("c", "ext-deezer-song-3", "Gamma"),
        ];
        let raw = [
            row("c", "Gamma", 200),
            row("a", "Alpha", 200),
            row("b", "Beta", 200),
        ];
        let out = merged(&entries, &raw, &[]).expect("merged, no presence asked for a move");
        assert_eq!(
            ids(&out),
            ["c", "a", "ext-deezer-song-1", "b", "ext-deezer-song-2"]
        );
        assert_eq!(out[0].outside_id.as_deref(), Some("ext-deezer-song-3"));
    }

    #[test]
    fn a_1500_by_1500_middle_keeps_every_fallback() {
        let mut entries = vec![lib("h", "Head")];
        let mut raw = vec![row("h", "Head", 200)];
        for i in 0..1_500 {
            entries.push(bound(
                &format!("old{i}"),
                &format!("ext-deezer-song-{i}"),
                &format!("Morning {i} light"),
            ));
            raw.push(row(&format!("new{i}"), &format!("Morning {i} light"), 201));
        }
        entries.push(lib("t", "Tail"));
        raw.push(row("t", "Tail", 200));
        let out = merged(&entries, &raw, &[]).expect("merged");
        assert_eq!(out.len(), 1_502);
        for (i, entry) in out[1..1_501].iter().enumerate() {
            assert_eq!(entry.library_id.as_deref(), Some(format!("new{i}").as_str()));
            assert_eq!(
                entry.outside_id.as_deref(),
                Some(format!("ext-deezer-song-{i}").as_str())
            );
        }
    }

    #[test]
    fn a_gone_file_streams_again_and_an_unknown_answer_changes_nothing() {
        let entries = vec![lib("a", "Alpha"), bound("b", "ext-deezer-song-1", "Beta")];
        let raw = [row("a", "Alpha", 200)];
        assert_eq!(
            ids_needing_presence(&entries, &library_part(&entries), &raw),
            ["b"]
        );
        let out = merged(&entries, &raw, &[("b", Some(false))]).expect("merged");
        assert_eq!(ids(&out), ["a", "ext-deezer-song-1"]);
        assert!(out[1].streams());
        let out = merged(&entries, &raw, &[("b", Some(true))]).expect("merged");
        assert_eq!(ids(&out), ["a"]);
        assert_eq!(
            merged(&entries, &raw, &[("b", None)]),
            Err(MergeError::Unreachable)
        );
        assert_eq!(merged(&entries, &raw, &[]), Err(MergeError::Unreachable));
    }

    #[test]
    fn the_same_take_in_the_gap_keeps_the_place_and_fallback() {
        let entries = vec![
            lib("x", "Start"),
            bound("old", "ext-deezer-song-1", "Song"),
            lib("z", "End"),
        ];
        let raw = [
            row("x", "Start", 200),
            row("new", "Song", 201),
            row("z", "End", 200),
        ];
        assert!(ids_needing_presence(&entries, &library_part(&entries), &raw).is_empty());
        let out = merged(&entries, &raw, &[]).expect("merged");
        assert_eq!(ids(&out), ["x", "new", "z"]);
        assert_eq!(out[1].outside_id.as_deref(), Some("ext-deezer-song-1"));
    }

    #[test]
    fn another_take_is_not_paired() {
        let entries = vec![
            lib("x", "Start"),
            bound("old", "ext-deezer-song-1", "Song"),
            lib("z", "End"),
        ];
        let raw = [
            row("x", "Start", 200),
            row("live", "Song (Live)", 200),
            row("z", "End", 200),
        ];
        let out = merged(&entries, &raw, &[("old", Some(true))]).expect("merged");
        assert_eq!(ids(&out), ["x", "live", "z"]);
        assert_eq!(out[1].outside_id, None);
        let out = merged(&entries, &raw, &[("old", Some(false))]).expect("merged");
        assert_eq!(ids(&out), ["x", "ext-deezer-song-1", "live", "z"]);
    }

    #[test]
    fn two_inserts_in_one_gap_keep_their_order_around_a_pair() {
        let entries = vec![
            lib("x", "Start"),
            bound("old", "ext-deezer-song-1", "Song"),
            lib("z", "End"),
        ];
        let raw = [
            row("x", "Start", 200),
            row("n1", "Other", 180),
            row("n2", "Song", 200),
            row("z", "End", 200),
        ];
        let out = merged(&entries, &raw, &[]).expect("merged");
        assert_eq!(ids(&out), ["x", "n1", "n2", "z"]);
        assert_eq!(out[1].outside_id, None);
        assert_eq!(out[2].outside_id.as_deref(), Some("ext-deezer-song-1"));
    }

    #[test]
    fn a_moved_back_copy_adds_only_its_streamed_entries_by_their_neighbors() {
        let s1 = streamed("ext-deezer-song-1", "One");
        let s2 = streamed("ext-deezer-song-2", "Two");
        let kept_aside = vec![lib("a", "A"), s1.clone(), lib("b", "B"), s2.clone()];

        let out = move_back_entries(&[lib("a", "A"), lib("b", "B")], &kept_aside);
        assert_eq!(ids(&out), ["a", "ext-deezer-song-1", "b", "ext-deezer-song-2"]);

        let out = move_back_entries(&[lib("b", "B"), lib("a", "A")], &kept_aside);
        assert_eq!(ids(&out), ["b", "ext-deezer-song-2", "a", "ext-deezer-song-1"]);

        // A streamed entry with no present entry before it goes first; one already there stays once.
        let out = move_back_entries(
            &[lib("b", "B"), s2.clone()],
            &[s1.clone(), lib("b", "B"), s2.clone()],
        );
        assert_eq!(ids(&out), ["ext-deezer-song-1", "b", "ext-deezer-song-2"]);

        // Occurrences: the second kept-aside copy of a song goes after the first.
        let out = move_back_entries(std::slice::from_ref(&s1), &[s1.clone(), s1.clone()]);
        assert_eq!(ids(&out), ["ext-deezer-song-1", "ext-deezer-song-1"]);

        // A library entry that is not present is not added back.
        let out = move_back_entries(&[], &[lib("gone", "Gone"), s1]);
        assert_eq!(ids(&out), ["ext-deezer-song-1"]);
    }
}
