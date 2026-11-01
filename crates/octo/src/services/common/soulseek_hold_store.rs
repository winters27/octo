//! The Soulseek hold store, the store behind `soulseek-holds.json`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use octo_core::common::dotnet;
use octo_core::fingerprint::heart_gate::{
    AcquisitionOrigin, AlbumContext, HeardRecord, HeardRecording, WaitingFor,
};
use octo_core::json::datetime;
use parking_lot::Mutex;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};
use serde_repr::{Deserialize_repr, Serialize_repr};
use tracing::warn;

use crate::services::fingerprint::download_verification_service::LabelSource;
use crate::services::soulseek::soulseek_download_service::INCOMING_FOLDER_NAME;
use crate::services::state_file;

/// Appended, never inserted: the file stores these as numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum HeldKind {
    #[default]
    Track = 0,
    Album = 1,
}

impl HeldKind {
    /// The enum name, as `{Kind}` interpolated it.
    pub fn name(self) -> &'static str {
        match self {
            HeldKind::Track => "Track",
            HeldKind::Album => "Album",
        }
    }
}

/// One person waiting on a held acquisition, with what they heard.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Holder {
    pub user: String,
    /// The source this person heard, when it was first served to them, and its sample.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heard: Option<HeardRecord>,
    /// The album this person listened from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<AlbumContext>,
    /// One print of this person's heard audio over the whole song, taken once when the catalog's
    /// clip lies past the stored sample.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub whole_heard_print: Option<Vec<u32>>,
    /// How this person's heard video has answered the shim's rounds.
    #[serde(skip_serializing_if = "ShimTries::is_clear")]
    pub shim_tries: ShimTries,
}

/// How a heard video has answered the shim's rounds since its last other answer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ShimTries {
    /// Rounds whose probe proved the shim works and in which this video still answered Busy.
    pub busy_rounds: u32,
    /// The video answered gone once after a working-shim proof; a second gone answer after another
    /// proof means it is gone.
    pub gone_once: bool,
}

impl ShimTries {
    /// Whether nothing has been counted.
    pub fn is_clear(&self) -> bool {
        *self == Self::default()
    }
}

/// A held track of an album heart: the album row it is listed under, the catalog track it is, and
/// the recording it must prove to be.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct HeldAlbumTrack {
    /// The album row's acquisition key, which keeping the held tracks is asked with.
    pub album_key: String,
    /// The catalog track's song ids: the album listing's routing short id and its catalog id.
    pub song_ids: Vec<String>,
    pub artist: String,
    pub title: String,
    pub duration_seconds: Option<i32>,
    /// The catalog track's own recording.
    pub r: HeardRecording,
}

/// A held file Lidarr brought, and where its own labels are read from when it is judged again:
/// the download as delivered, or nowhere. Lidarr's own matching is never a label.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct HeldLidarrFile {
    /// Where it waits in the one hold path.
    pub path: String,
    /// The download as delivered, which holds its labels; with none, it has no labels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered: Option<String>,
}

impl HeldLidarrFile {
    /// The file Lidarr brought that waits at `path`, with the labels it was judged by: a judgment
    /// of it later reads the download as delivered when that is where they came from, and no
    /// labels otherwise, never the held file's own tags.
    pub fn of(path: &str, labels: &LabelSource) -> Self {
        HeldLidarrFile {
            path: path.to_string(),
            delivered: match labels {
                LabelSource::Delivered(delivered) => Some(delivered.to_string_lossy().into_owned()),
                LabelSource::TheFile | LabelSource::None => None,
            },
        }
    }

    /// Where its labels are read from when it is judged again.
    pub fn labels(&self) -> LabelSource {
        match &self.delivered {
            Some(delivered) => LabelSource::Delivered(PathBuf::from(delivered)),
            None => LabelSource::None,
        }
    }
}

/// A held acquisition: a heart waiting out a Soulseek outage, or a heart, a play's file or an
/// album track waiting for an answer, a proof or the person. HeldSinceUtc survives a restart, so
/// a Soulseek wait is the setting's length in total, not that long again after every restart.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct HeldAcquisition {
    pub kind: HeldKind,
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub provider: String,
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub external_id: String,
    pub requested_by: Option<String>,
    #[serde(with = "datetime::utc")]
    pub held_since_utc: DateTime<Utc>,
    /// Why the acquisition runs; None on a record written before origins were kept, which is a
    /// heart of its kind.
    pub origin: Option<AcquisitionOrigin>,
    /// What the hold waits for; None for a heart waiting out a Soulseek outage.
    pub waiting_for: Option<WaitingFor>,
    /// Everyone waiting on it, each with what they heard; seeded from `requested_by` when none is
    /// listed.
    pub holders: Vec<Holder>,
    /// The people who asked to keep the heard audio anyway, saved before the keep is carried out.
    pub keep_requested: Vec<String>,
    /// The catalog track's clip print, taken once for the clip check.
    pub clip_print: Option<Vec<u32>>,
    /// The files this record holds in the one hold path.
    pub held_files: Vec<String>,
    /// Those of `held_files` Lidarr brought, each with where its labels are read from.
    pub lidarr_files: Vec<HeldLidarrFile>,
    /// The album row and catalog track of a held album track.
    pub album_track: Option<HeldAlbumTrack>,
    /// The line its downloads row shows while it waits.
    pub line: Option<String>,
}

impl Default for HeldAcquisition {
    fn default() -> Self {
        HeldAcquisition {
            kind: HeldKind::Track,
            provider: String::new(),
            external_id: String::new(),
            requested_by: None,
            held_since_utc: datetime::min_value(),
            origin: None,
            waiting_for: None,
            holders: Vec::new(),
            keep_requested: Vec::new(),
            clip_print: None,
            held_files: Vec::new(),
            lidarr_files: Vec::new(),
            album_track: None,
            line: None,
        }
    }
}

impl HeldAcquisition {
    /// The hold's key: the acquisition and, when it waits for something, what it waits for, so two
    /// holds of one song for different reasons never take each other's place.
    pub fn key(&self) -> String {
        match self.waiting_for {
            Some(waiting) => format!("{}:{}", self.base_key(), waiting.name()),
            None => self.base_key(),
        }
    }

    /// The acquisition's own key, `Kind:provider:id`, the same for every hold of it.
    pub fn base_key(&self) -> String {
        format!("{}:{}:{}", self.kind.name(), self.provider, self.external_id)
    }

    /// Why the acquisition runs: the stored origin, else a heart of the record's kind.
    pub fn origin(&self) -> AcquisitionOrigin {
        self.origin.unwrap_or(match self.kind {
            HeldKind::Track => AcquisitionOrigin::Heart,
            HeldKind::Album => AcquisitionOrigin::AlbumHeart,
        })
    }

    /// Whether its downloads row offers to keep the heard audio anyway: a song held while
    /// AcoustID is off, or a held track of an album heart.
    pub fn keeps_anyway(&self) -> bool {
        match self.waiting_for {
            Some(WaitingFor::AcoustIdOff) => true,
            Some(WaitingFor::NotYetProven) => self.album_track.is_some(),
            _ => false,
        }
    }

    /// The id a person's hold keeps a heard copy under: one per person and acquisition, the same
    /// whatever the hold waits for.
    pub fn copy_holder_id(&self, user: &str) -> String {
        let user = dotnet::to_lower_invariant(user.trim());
        format!("heart:{}:{user}", self.base_key())
    }

    /// The folder the record's files wait in: one per acquisition, under [`held_root`].
    pub fn hold_folder(&self, download_path: &Path) -> PathBuf {
        let name: String = self.base_key().chars().map(folder_char).collect();
        held_root(download_path).join(name)
    }

    /// The holders a record that lists none stands for: its one requester, with nothing known of
    /// what they heard.
    pub fn seeded_holders(requested_by: Option<&str>) -> Vec<Holder> {
        requested_by
            .filter(|user| !dotnet::is_blank(user))
            .map(|user| {
                vec![Holder {
                    user: user.to_string(),
                    ..Default::default()
                }]
            })
            .unwrap_or_default()
    }

    /// Fills `holders` from `requested_by` when none is listed.
    pub fn seed_holders(&mut self) {
        if self.holders.is_empty() {
            self.holders = Self::seeded_holders(self.requested_by.as_deref());
        }
    }

    /// Keeps the labels of the Lidarr files it still holds, one per file: a file that left the
    /// record takes its labels with it.
    fn tidy_lidarr_files(&mut self) {
        let held = &self.held_files;
        let mut seen: Vec<String> = Vec::new();
        self.lidarr_files.retain(|file| {
            let keep = held.contains(&file.path) && !seen.contains(&file.path);
            if keep {
                seen.push(file.path.clone());
            }
            keep
        });
    }

    /// Whether `holders` says more than `requested_by` already does, so the file must carry it.
    fn writes_holders(&self) -> bool {
        !self.holders.is_empty() && self.holders != Self::seeded_holders(self.requested_by.as_deref())
    }
}

/// The folder under the download path's incoming folder that every held file waits in.
pub const HELD_FOLDER_NAME: &str = "held";

/// Where every held file waits: `<download path>/<incoming folder>/held`.
pub fn held_root(download_path: &Path) -> PathBuf {
    download_path.join(INCOMING_FOLDER_NAME).join(HELD_FOLDER_NAME)
}

/// A character as a folder name holds it: letters, digits, '-', '_' and '.' as they are, any other
/// as '_'.
fn folder_char(c: char) -> char {
    if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
        c
    } else {
        '_'
    }
}

/// By hand, because the file also carries the computed `Key` (ignored on read). Why a hold runs,
/// what it waits for, who holds it and what it keeps are written only when set, so a hold with
/// none of them is written exactly as before.
impl Serialize for HeldAcquisition {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let holders = self.writes_holders();
        let optional = [
            self.origin.is_some(),
            self.waiting_for.is_some(),
            holders,
            !self.keep_requested.is_empty(),
            self.clip_print.is_some(),
            !self.held_files.is_empty(),
            !self.lidarr_files.is_empty(),
            self.album_track.is_some(),
            self.line.is_some(),
        ]
        .into_iter()
        .filter(|written| *written)
        .count();
        let mut s = serializer.serialize_struct("HeldAcquisition", 6 + optional)?;
        s.serialize_field("Kind", &self.kind)?;
        s.serialize_field("Provider", &self.provider)?;
        s.serialize_field("ExternalId", &self.external_id)?;
        s.serialize_field("RequestedBy", &self.requested_by)?;
        s.serialize_field("HeldSinceUtc", &datetime::format_utc(&self.held_since_utc))?;
        if let Some(origin) = &self.origin {
            s.serialize_field("Origin", origin)?;
        }
        if let Some(waiting) = &self.waiting_for {
            s.serialize_field("WaitingFor", waiting)?;
        }
        if holders {
            s.serialize_field("Holders", &self.holders)?;
        }
        if !self.keep_requested.is_empty() {
            s.serialize_field("KeepRequested", &self.keep_requested)?;
        }
        if let Some(print) = &self.clip_print {
            s.serialize_field("ClipPrint", print)?;
        }
        if !self.held_files.is_empty() {
            s.serialize_field("HeldFiles", &self.held_files)?;
        }
        if !self.lidarr_files.is_empty() {
            s.serialize_field("LidarrFiles", &self.lidarr_files)?;
        }
        if let Some(track) = &self.album_track {
            s.serialize_field("AlbumTrack", track)?;
        }
        if let Some(line) = &self.line {
            s.serialize_field("Line", line)?;
        }
        s.serialize_field("Key", &self.key())?;
        s.end()
    }
}

/// The held acquisitions, on disk: hearts waiting out a Soulseek outage, and hearts, imports and
/// album tracks waiting for an answer, a proof or the person. Without it a restart during a six
/// hour wait would drop every one of them without a word, where before the wait existed they at
/// least landed as MP3s. Written on every change, inside the lock: there are a handful, and two
/// writes outside it could land in the wrong order.
pub struct SoulseekHoldStore {
    path: Option<PathBuf>,
    held: Mutex<DotnetDictionary<HeldAcquisition>>,
}

impl SoulseekHoldStore {
    /// A store kept at `path` (loaded now), or in memory only when it is absent or blank.
    pub fn new(path: Option<impl AsRef<Path>>) -> Self {
        let path = path
            .map(|p| p.as_ref().to_path_buf())
            .filter(|p| !p.as_os_str().to_string_lossy().trim().is_empty());
        let mut held = DotnetDictionary::default();
        if let Some(path) = &path {
            let loaded = (|| -> anyhow::Result<Vec<HeldAcquisition>> {
                let Some(text) = state_file::read_text(path)? else {
                    return Ok(Vec::new());
                };
                Ok(serde_json::from_str::<Option<Vec<HeldAcquisition>>>(&text)?.unwrap_or_default())
            })();
            match loaded {
                Ok(list) => {
                    for mut h in list {
                        h.seed_holders();
                        h.tidy_lidarr_files();
                        held.set(h.key(), h);
                    }
                }
                Err(e) => warn!("held downloads could not be read: {e}"),
            }
        }
        SoulseekHoldStore {
            path,
            held: Mutex::new(held),
        }
    }

    /// Every hold, oldest first.
    pub fn snapshot(&self) -> Vec<HeldAcquisition> {
        let mut list: Vec<HeldAcquisition> = self.held.lock().values().cloned().collect();
        // A stable sort: holds from the same instant keep their order.
        list.sort_by_key(|h| h.held_since_utc);
        list
    }

    /// Records a hold, its holders seeded from its requester when it lists none. A hold already
    /// there under the same key keeps its first start time and takes in the new record's holders:
    /// a person not listed yet joins it, and a listed person's entry is replaced only by one heard
    /// later. Answers the hold as kept.
    pub fn hold(&self, mut held: HeldAcquisition) -> HeldAcquisition {
        held.seed_holders();
        held.tidy_lidarr_files();
        let mut map = self.held.lock();
        let key = held.key();
        if let Some(existing) = map.get(&key) {
            let mut kept = existing.clone();
            if !merge_holders(&mut kept.holders, held.holders) {
                return kept;
            }
            map.set(key, kept.clone());
            self.save(&map);
            return kept;
        }
        map.set(key, held.clone());
        self.save(&map);
        held
    }

    pub fn release(&self, key: &str) {
        let mut map = self.held.lock();
        if map.remove(key) {
            self.save(&map);
        }
    }

    /// The hold under `key`, when there is one.
    pub fn get(&self, key: &str) -> Option<HeldAcquisition> {
        self.held.lock().get(key).cloned()
    }

    /// Takes `user` off the hold `key`, with any keep they asked for. The hold ends when nobody is
    /// left: None then, and None when there is no such hold. A hold that lists nobody, or not
    /// this person, is answered as it is. When the person who asked first leaves, the next holder
    /// is the one it is requested by.
    pub fn release_holder(&self, key: &str, user: &str) -> Option<HeldAcquisition> {
        let user = user.trim();
        let mut map = self.held.lock();
        let mut kept = map.get(key)?.clone();
        let before = kept.holders.len();
        kept.holders
            .retain(|holder| !dotnet::eq_ignore_case(holder.user.trim(), user));
        if kept.holders.len() == before {
            return Some(kept);
        }
        kept.keep_requested
            .retain(|asked| !dotnet::eq_ignore_case(asked.trim(), user));
        if kept.holders.is_empty() {
            map.remove(key);
            self.save(&map);
            return None;
        }
        if kept
            .requested_by
            .as_deref()
            .is_some_and(|first| dotnet::eq_ignore_case(first.trim(), user))
        {
            kept.requested_by = Some(kept.holders[0].user.clone());
        }
        map.set(key.to_string(), kept.clone());
        self.save(&map);
        Some(kept)
    }

    /// Every hold of one acquisition (`Kind:provider:id`), whatever it waits for, oldest first.
    pub fn records_for(&self, base_key: &str) -> Vec<HeldAcquisition> {
        let mut list: Vec<HeldAcquisition> = self
            .held
            .lock()
            .values()
            .filter(|held| held.base_key() == base_key)
            .cloned()
            .collect();
        list.sort_by_key(|held| held.held_since_utc);
        list
    }

    /// Files the hold `key` under another wait, keeping everything else; into a hold already
    /// waiting for that, its holders join. Answers the hold as kept, or None when there is no
    /// such hold.
    pub fn move_to(&self, key: &str, waiting_for: Option<WaitingFor>) -> Option<HeldAcquisition> {
        let mut map = self.held.lock();
        let mut moved = map.get(key)?.clone();
        if moved.waiting_for == waiting_for {
            return Some(moved);
        }
        map.remove(key);
        moved.waiting_for = waiting_for;
        let new_key = moved.key();
        let kept = match map.get(&new_key) {
            Some(existing) => {
                let mut kept = existing.clone();
                merge_holders(&mut kept.holders, moved.holders);
                kept
            }
            None => moved,
        };
        map.set(new_key, kept.clone());
        self.save(&map);
        Some(kept)
    }

    /// Changes the hold `key` in place (who asked to keep it, its prints, the shim's tries, its
    /// held files) and saves it; a held file it no longer holds takes its Lidarr labels with it.
    /// A change of what it waits for files it under its new key. Answers the hold as kept, or None
    /// when there is no such hold.
    pub fn update(&self, key: &str, change: impl FnOnce(&mut HeldAcquisition)) -> Option<HeldAcquisition> {
        let mut map = self.held.lock();
        let mut kept = map.get(key)?.clone();
        change(&mut kept);
        kept.tidy_lidarr_files();
        let new_key = kept.key();
        if new_key != key {
            map.remove(key);
        }
        map.set(new_key, kept.clone());
        self.save(&map);
        Some(kept)
    }

    // Called with the lock held.
    fn save(&self, map: &DotnetDictionary<HeldAcquisition>) {
        let Some(path) = &self.path else { return };
        let list: Vec<&HeldAcquisition> = map.values().collect();
        if let Err(e) = state_file::save_atomic(path, &octo_core::json::to_string(&list)) {
            warn!("held downloads could not be written: {e}");
        }
    }
}

/// Takes `joining` into `holders`, one entry per person (ignoring case): a person not listed is
/// added; a listed person's entry is replaced by one heard later, and otherwise takes the album
/// context it lacked. True when anything changed.
fn merge_holders(holders: &mut Vec<Holder>, joining: Vec<Holder>) -> bool {
    let mut changed = false;
    for holder in joining {
        if holder.user.trim().is_empty() {
            continue;
        }
        let Some(known) = holders
            .iter_mut()
            .find(|known| dotnet::eq_ignore_case(known.user.trim(), holder.user.trim()))
        else {
            holders.push(holder);
            changed = true;
            continue;
        };
        let heard_later = match (&known.heard, &holder.heard) {
            (_, None) => false,
            (None, Some(_)) => true,
            (Some(was), Some(now)) => now.heard_at > was.heard_at,
        };
        if heard_later {
            *known = holder;
            changed = true;
        } else if known.context.is_none() && holder.context.is_some() {
            known.context = holder.context;
            changed = true;
        }
    }
    changed
}

/// A `Dictionary<string, T>`'s enumeration order: insertion order, except that an entry added
/// after a removal takes the most recently freed slot. The file is written in this order.
#[derive(Debug)]
struct DotnetDictionary<T> {
    slots: Vec<Option<(String, T)>>,
    index: HashMap<String, usize>,
    /// Freed slots, the most recently freed last.
    free: Vec<usize>,
}

impl<T> Default for DotnetDictionary<T> {
    fn default() -> Self {
        DotnetDictionary {
            slots: Vec::new(),
            index: HashMap::new(),
            free: Vec::new(),
        }
    }
}

impl<T> DotnetDictionary<T> {
    fn get(&self, key: &str) -> Option<&T> {
        self.index
            .get(key)
            .and_then(|&i| self.slots[i].as_ref().map(|(_, v)| v))
    }

    /// `dict[key] = value`: replaces in place, or adds.
    fn set(&mut self, key: String, value: T) {
        if let Some(&i) = self.index.get(&key) {
            self.slots[i] = Some((key, value));
            return;
        }
        let i = match self.free.pop() {
            Some(i) => i,
            None => {
                self.slots.push(None);
                self.slots.len() - 1
            }
        };
        self.index.insert(key.clone(), i);
        self.slots[i] = Some((key, value));
    }

    fn remove(&mut self, key: &str) -> bool {
        let Some(i) = self.index.remove(key) else {
            return false;
        };
        self.slots[i] = None;
        self.free.push(i);
        true
    }

    fn values(&self) -> impl Iterator<Item = &T> {
        self.slots.iter().filter_map(|s| s.as_ref().map(|(_, v)| v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/rust-migration/fixtures/state/soulseek-holds.json"
    );

    fn held(kind: HeldKind, id: &str, minute: u32) -> HeldAcquisition {
        HeldAcquisition {
            kind,
            provider: "soulseek".into(),
            external_id: id.into(),
            requested_by: Some("brandon".into()),
            held_since_utc: Utc.with_ymd_and_hms(2026, 10, 3, 12, minute, 0).unwrap(),
            ..Default::default()
        }
    }

    #[test]
    fn the_fixture_round_trips_byte_for_byte() {
        let text = std::fs::read_to_string(FIXTURE).expect("the fixture is in the repo");
        let list: Vec<HeldAcquisition> = serde_json::from_str(&text).expect("the fixture reads");
        assert_eq!(list[1].key(), "Album:soulseek:Ab9Cd8Ef7Gh6Ij5Kl4Mn3o");
        assert_eq!(octo_core::json::to_string(&list), text.trim_end_matches('\n'));
    }

    #[test]
    fn holds_survive_a_restart_and_a_second_hold_keeps_the_first_start() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("config").join("soulseek-holds.json");
        let store = SoulseekHoldStore::new(Some(&path));
        let first = store.hold(held(HeldKind::Track, "a", 5));
        let again = store.hold(held(HeldKind::Track, "a", 9));
        assert_eq!(again, first);
        store.hold(held(HeldKind::Album, "b", 1));

        let reopened = SoulseekHoldStore::new(Some(&path));
        let keys: Vec<String> = reopened.snapshot().iter().map(HeldAcquisition::key).collect();
        assert_eq!(keys, vec!["Album:soulseek:b", "Track:soulseek:a"], "oldest first");

        reopened.release("Album:soulseek:b");
        reopened.release("nothing");
        assert_eq!(SoulseekHoldStore::new(Some(&path)).snapshot().len(), 1);
    }

    #[test]
    fn the_file_keeps_dictionary_order_with_freed_slots_reused() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("soulseek-holds.json");
        let store = SoulseekHoldStore::new(Some(&path));
        store.hold(held(HeldKind::Track, "a", 1));
        store.hold(held(HeldKind::Track, "b", 2));
        store.hold(held(HeldKind::Track, "c", 3));
        store.release("Track:soulseek:a");
        store.hold(held(HeldKind::Track, "d", 4));
        let written: Vec<HeldAcquisition> =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("reads")).expect("parses");
        let ids: Vec<&str> = written.iter().map(|h| h.external_id.as_str()).collect();
        assert_eq!(ids, vec!["d", "b", "c"]);
    }

    #[test]
    fn an_unreadable_file_or_no_path_means_no_holds() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("soulseek-holds.json");
        std::fs::write(&path, "[{").expect("writes");
        assert!(SoulseekHoldStore::new(Some(&path)).snapshot().is_empty());
        let memory = SoulseekHoldStore::new(None::<PathBuf>);
        memory.hold(held(HeldKind::Track, "a", 1));
        assert_eq!(memory.snapshot().len(), 1);
    }

    #[test]
    fn a_hold_with_holders_and_a_wait_round_trips_under_its_own_key() {
        let mut record = held(HeldKind::Track, "a", 1);
        record.waiting_for = Some(WaitingFor::AcoustId);
        record.origin = Some(AcquisitionOrigin::Heart);
        record.holders = vec![
            Holder {
                user: "brandon".into(),
                whole_heard_print: Some(vec![1, 2, 3]),
                ..Default::default()
            },
            Holder {
                user: "anna".into(),
                shim_tries: ShimTries {
                    busy_rounds: 2,
                    gone_once: false,
                },
                ..Default::default()
            },
        ];
        record.keep_requested = vec!["anna".into()];
        record.clip_print = Some(vec![7, 8]);
        record.held_files = vec!["/downloads/.octo-incoming/held/x/a.flac".into()];
        let text = octo_core::json::to_string(&record);
        let back: HeldAcquisition = serde_json::from_str(&text).expect("reads");
        assert_eq!(back, record);
        assert_eq!(record.key(), "Track:soulseek:a:AcoustId");
        assert_eq!(record.base_key(), "Track:soulseek:a");
        assert!(text.contains(r#""Key":"Track:soulseek:a:AcoustId""#), "{text}");
    }

    #[test]
    fn a_hold_whose_only_holder_is_its_requester_is_written_as_before() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("soulseek-holds.json");
        let store = SoulseekHoldStore::new(Some(&path));
        let kept = store.hold(held(HeldKind::Track, "a", 1));
        assert_eq!(kept.holders, HeldAcquisition::seeded_holders(Some("brandon")));
        let written = std::fs::read_to_string(&path).expect("written");
        assert!(!written.contains("Holders"), "{written}");
        let reopened = SoulseekHoldStore::new(Some(&path));
        assert_eq!(reopened.snapshot()[0].holders[0].user, "brandon");
    }

    #[test]
    fn copy_holder_ids_are_per_person_whatever_the_hold_waits_for() {
        let mut waiting = held(HeldKind::Track, "a", 1);
        waiting.waiting_for = Some(WaitingFor::Shim);
        let plain = held(HeldKind::Track, "a", 1);
        let id = waiting.copy_holder_id(" Brandon ");
        assert_eq!(id, "heart:Track:soulseek:a:brandon");
        assert_eq!(plain.copy_holder_id("BRANDON"), id);
    }

    #[test]
    fn only_a_song_held_while_acoustid_is_off_or_a_held_album_track_keeps_anyway() {
        let mut record = held(HeldKind::Track, "a", 1);
        assert!(!record.keeps_anyway());
        record.waiting_for = Some(WaitingFor::AcoustIdOff);
        assert!(record.keeps_anyway());
        record.waiting_for = Some(WaitingFor::NotYetProven);
        assert!(!record.keeps_anyway());
        record.album_track = Some(HeldAlbumTrack::default());
        assert!(record.keeps_anyway());
    }

    #[test]
    fn a_record_with_no_origin_is_a_heart_of_its_kind() {
        let track = held(HeldKind::Track, "a", 1);
        assert_eq!(track.origin(), AcquisitionOrigin::Heart);
        let album = held(HeldKind::Album, "b", 1);
        assert_eq!(album.origin(), AcquisitionOrigin::AlbumHeart);
        let mut play = held(HeldKind::Track, "c", 1);
        play.origin = Some(AcquisitionOrigin::Play);
        assert_eq!(play.origin(), AcquisitionOrigin::Play);
    }

    fn heard(video: &str, minute: u32) -> HeardRecord {
        HeardRecord {
            heard_at: Utc.with_ymd_and_hms(2026, 10, 29, 12, minute, 0).unwrap(),
            source: octo_core::fingerprint::heart_gate::HeardSource::Video {
                id: video.into(),
                size: Some(1000),
                content_type: "audio/webm".into(),
                itag: None,
            },
            sample_id: None,
        }
    }

    fn waiting(user: &str, video: &str, minute: u32) -> HeldAcquisition {
        let mut record = held(HeldKind::Track, "help", 1);
        record.requested_by = Some(user.into());
        record.origin = Some(AcquisitionOrigin::Heart);
        record.waiting_for = Some(WaitingFor::MusicBrainz);
        record.holders = vec![Holder {
            user: user.into(),
            heard: Some(heard(video, minute)),
            ..Default::default()
        }];
        record
    }

    fn videos(record: &HeldAcquisition) -> Vec<(String, Option<String>)> {
        record
            .holders
            .iter()
            .map(|h| {
                let video = h
                    .heard
                    .as_ref()
                    .and_then(|r| r.source.video_id())
                    .map(str::to_string);
                (h.user.clone(), video)
            })
            .collect()
    }

    #[test]
    fn two_people_heart_one_held_song_and_one_unheart_leaves_the_others_hold() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("soulseek-holds.json");
        let store = SoulseekHoldStore::new(Some(&path));
        let first = store.hold(waiting("brandon", "v2009", 1));
        let joined = store.hold(waiting("anna", "v2015", 2));
        assert_eq!(joined.key(), first.key(), "one record");
        assert_eq!(
            videos(&joined),
            [
                ("brandon".to_string(), Some("v2009".to_string())),
                ("anna".to_string(), Some("v2015".to_string()))
            ]
        );
        // An older heard source for a listed person changes nothing; a later one replaces it.
        store.hold(waiting("Brandon", "older", 0));
        let later = store.hold(waiting("BRANDON", "v2009-again", 5));
        assert_eq!(
            later.holders[0].heard.as_ref().unwrap().source.video_id(),
            Some("v2009-again")
        );

        let left = store
            .release_holder(&first.key(), " Brandon ")
            .expect("anna still holds it");
        assert_eq!(videos(&left), [("anna".to_string(), Some("v2015".to_string()))]);
        assert_eq!(left.requested_by.as_deref(), Some("anna"));
        let reopened = SoulseekHoldStore::new(Some(&path));
        assert_eq!(videos(&reopened.snapshot()[0]), videos(&left), "saved");

        assert!(
            reopened.release_holder(&first.key(), "nobody").is_some(),
            "not a holder"
        );
        assert!(
            reopened.release_holder(&first.key(), "anna").is_none(),
            "nobody left"
        );
        assert!(SoulseekHoldStore::new(Some(&path)).snapshot().is_empty());
    }

    #[test]
    fn records_of_one_song_are_found_whatever_they_wait_for_and_move_between_waits() {
        let store = SoulseekHoldStore::new(None::<PathBuf>);
        let outage = store.hold(held(HeldKind::Track, "help", 1));
        let mb = store.hold(waiting("brandon", "v", 2));
        store.hold(held(HeldKind::Track, "other", 3));
        let keys: Vec<String> = store
            .records_for("Track:soulseek:help")
            .iter()
            .map(HeldAcquisition::key)
            .collect();
        assert_eq!(keys, vec![outage.key(), mb.key()]);

        let mut acoustid = waiting("anna", "w", 4);
        acoustid.waiting_for = Some(WaitingFor::AcoustId);
        store.hold(acoustid);
        let moved = store
            .move_to(&mb.key(), Some(WaitingFor::AcoustId))
            .expect("moved");
        assert_eq!(moved.key(), "Track:soulseek:help:AcoustId");
        let users: Vec<&str> = moved.holders.iter().map(|h| h.user.as_str()).collect();
        assert_eq!(users, ["anna", "brandon"], "the two holds became one");
        assert!(store.get(&mb.key()).is_none());

        let updated = store
            .update(&moved.key(), |record| {
                record.keep_requested.push("anna".into());
                record.clip_print = Some(vec![1, 2]);
            })
            .expect("updated");
        assert_eq!(store.get(&moved.key()), Some(updated));
        assert!(store.update("missing", |_| {}).is_none());
        assert!(store.move_to("missing", None).is_none());
    }

    #[test]
    fn holds_of_every_origin_survive_a_restart() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("soulseek-holds.json");
        let store = SoulseekHoldStore::new(Some(&path));
        store.hold(waiting("brandon", "v", 1));
        let mut album = held(HeldKind::Album, "help-album", 2);
        album.waiting_for = Some(WaitingFor::LidarrIndexers);
        album.origin = Some(AcquisitionOrigin::AlbumHeart);
        store.hold(album);
        let mut import = held(HeldKind::Track, "imported", 3);
        import.origin = Some(AcquisitionOrigin::Import);
        import.waiting_for = Some(WaitingFor::AcoustId);
        import.held_files = vec!["/downloads/.octo-incoming/held/x/a.flac".into()];
        store.hold(import);

        let reopened = SoulseekHoldStore::new(Some(&path)).snapshot();
        let read: Vec<(String, AcquisitionOrigin, usize)> = reopened
            .iter()
            .map(|h| (h.key(), h.origin(), h.holders.len()))
            .collect();
        assert_eq!(
            read,
            [
                (
                    "Track:soulseek:help:MusicBrainz".to_string(),
                    AcquisitionOrigin::Heart,
                    1
                ),
                (
                    "Album:soulseek:help-album:LidarrIndexers".to_string(),
                    AcquisitionOrigin::AlbumHeart,
                    1
                ),
                (
                    "Track:soulseek:imported:AcoustId".to_string(),
                    AcquisitionOrigin::Import,
                    1
                ),
            ]
        );
        assert_eq!(
            reopened[0].holders[0].heard.as_ref().unwrap().source.video_id(),
            Some("v")
        );
        assert_eq!(reopened[2].held_files.len(), 1);
    }

    #[test]
    fn held_files_wait_in_one_folder_per_acquisition() {
        let record = held(HeldKind::Track, "a|b c", 1);
        let folder = record.hold_folder(Path::new("/downloads"));
        let expected = Path::new("/downloads")
            .join(INCOMING_FOLDER_NAME)
            .join(HELD_FOLDER_NAME)
            .join("Track_soulseek_a_b_c");
        assert_eq!(folder, expected);
    }

    /// A held file Lidarr brought keeps where its labels are read from on its hold, across a
    /// restart; the labels end with the file whenever it leaves the hold (kept, refused or thrown
    /// away), and with the hold itself when it ends.
    #[test]
    fn a_held_lidarr_files_labels_live_and_end_with_its_place_on_the_hold() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("soulseek-holds.json");
        let store = SoulseekHoldStore::new(Some(&path));
        let (lidarr, other) = (
            "/downloads/.octo-incoming/held/Track_soulseek_x/a.flac",
            "/downloads/.octo-incoming/held/Track_soulseek_x/b.flac",
        );
        let brought = HeldLidarrFile::of(
            lidarr,
            &LabelSource::Delivered(PathBuf::from("/downloads/complete/a.flac")),
        );
        let mut record = held(HeldKind::Track, "x", 1);
        record.waiting_for = Some(WaitingFor::AcoustId);
        record.held_files = vec![lidarr.into(), other.into()];
        // One file the hold no longer holds, and one Lidarr label named twice.
        record.lidarr_files = vec![
            brought.clone(),
            brought.clone(),
            HeldLidarrFile::of("/gone.flac", &LabelSource::None),
        ];
        let key = store.hold(record).key();

        assert_eq!(store.get(&key).expect("held").lidarr_files, [brought.clone()]);
        let reopened = SoulseekHoldStore::new(Some(&path));
        let read = reopened.get(&key).expect("held after a restart");
        assert_eq!(read.lidarr_files, [brought.clone()]);
        assert_eq!(
            read.lidarr_files[0].labels(),
            LabelSource::Delivered(PathBuf::from("/downloads/complete/a.flac"))
        );
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("written")).expect("JSON");
        assert_eq!(written[0]["LidarrFiles"][0]["Path"], lidarr);

        let left = reopened
            .update(&key, |kept| kept.held_files.retain(|file| file != lidarr))
            .expect("updated");
        assert!(left.lidarr_files.is_empty(), "{left:?}");
        assert_eq!(left.held_files, [other]);

        reopened.update(&key, |kept| {
            kept.held_files.push(lidarr.into());
            kept.lidarr_files.push(brought.clone());
        });
        reopened.release(&key);
        assert!(reopened.snapshot().is_empty());
        assert!(SoulseekHoldStore::new(Some(&path)).snapshot().is_empty());
    }

    /// A file Lidarr brought with Lidarr writing its tags, or whose delivered copy was not read,
    /// has no labels when it is judged again: never the held file's own tags.
    #[test]
    fn a_held_lidarr_file_without_a_delivered_copy_reads_as_no_labels() {
        for labels in [LabelSource::TheFile, LabelSource::None] {
            let brought = HeldLidarrFile::of("/held/a.flac", &labels);
            assert_eq!(brought.delivered, None, "{labels:?}");
            assert_eq!(brought.labels(), LabelSource::None, "{labels:?}");
        }
    }
}
