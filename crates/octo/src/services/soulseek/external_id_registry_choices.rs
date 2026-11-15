//! What the registry does with the versions people chose ([`StreamChoices`]): which video each
//! routing of a chosen song plays, the switch at a play's start, the guards that keep every other
//! writer off a chosen song's video, the plays bound to the video they started on, and the ids
//! the change feed names.
//!
//! Lock order as the registry's: a routing is locked only with the registry's own lock let go,
//! and the store's locks are taken last (copies come out before anything else is locked).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::fingerprint::heart_gate::{HeardRecord, HeardSource};
use octo_core::soulseek::{LengthSource, PlayBinding, RoutingKind, SongLength, SoulseekRouting, StreamFacts};
use parking_lot::Mutex;

use super::{ExternalIdRegistry, SharedRouting};
use crate::services::family::store_file;
use crate::services::i_download_service::StreamPlay;
use crate::services::soulseek::stream_choices::{ChangeFeed, StreamChoice, StreamChoices};

/// Which video choice holds a routing of a song.
#[derive(Debug, Clone, PartialEq)]
pub enum PickState {
    /// No choice applies.
    None,
    /// A person's choice applies: nothing but a play's start changes the routing's video.
    Person(StreamChoice),
    /// Octo's own pick, written when a choice was undone: a routing still marked chosen takes it
    /// at its next play's start.
    Octo(StreamChoice),
    /// A routing still marked chosen with no record that applies (the record was put out, or the
    /// file was lost): its own video stands as a person's choice.
    Earlier,
}

/// Why a choice was not stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChooseError {
    /// The id is not a song the registry holds.
    NotASong,
    /// The song has no artist and title to file a choice under.
    NoKey,
    /// The video is refused for the song.
    Refused,
}

/// A play's body streaming now; dropping it ends that.
pub struct LiveGuard {
    live: Arc<Mutex<HashMap<String, usize>>>,
    key: Option<String>,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        let Some(key) = &self.key else { return };
        let mut live = self.live.lock();
        if let Some(count) = live.get_mut(key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                live.remove(key);
            }
        }
    }
}

/// The key a binding's live bodies are counted under: the registered id and the play's device,
/// with its token, else its address.
fn live_key(id: &str, device: &str, token: Option<&str>, address: Option<&str>) -> String {
    match token {
        Some(token) => format!("{id}\n{device}\nt:{token}"),
        None => format!("{id}\n{device}\na:{}", address.unwrap_or("")),
    }
}

fn live_key_of(id: &str, binding: &PlayBinding) -> String {
    live_key(
        id,
        &binding.key,
        binding.token.as_deref(),
        binding.address.as_deref(),
    )
}

impl ExternalIdRegistry {
    /// The versions people chose.
    pub fn choices(&self) -> &StreamChoices {
        &self.choices
    }

    /// The registered id `id` stands for, with its routing.
    fn registered(&self, id: &str) -> Option<(String, SharedRouting)> {
        self.resolve(id, false)
            .filter(|found| found.registered)
            .map(|found| (found.id, found.routing))
    }

    /// Whether `video` is refused by the routing or by its song.
    fn refused_for(&self, routing: &SoulseekRouting, video: &str) -> bool {
        routing.refuses_video(video) || self.refusals_for(routing).iter().any(|refused| refused == video)
    }

    /// Whether `record` (filed under the routing's key) applies to it: a song, the video refused by
    /// neither the routing nor the song, and the lengths fit.
    fn record_applies(&self, routing: &SoulseekRouting, record: &StreamChoice) -> bool {
        routing.kind == RoutingKind::Song
            && !self.refused_for(routing, &record.video)
            && StreamChoices::fits(record, StreamChoices::catalog_seconds(routing))
    }

    /// Which choice holds `routing`.
    pub fn pick_for(&self, routing: &SoulseekRouting) -> PickState {
        if let Some(key) = StreamChoices::key_of(routing)
            && let Some(record) = self.choices.record_for_key(&key)
            && self.record_applies(routing, &record)
        {
            return if record.auto {
                PickState::Octo(record)
            } else {
                PickState::Person(record)
            };
        }
        let still_chosen = routing.video_pick == SoulseekRouting::VIDEO_CHOSEN
            && routing
                .you_tube_id
                .as_deref()
                .is_some_and(|video| !video.is_empty() && !self.refused_for(routing, video));
        if still_chosen {
            PickState::Earlier
        } else {
            PickState::None
        }
    }

    /// Whether a person's choice (or an earlier one) holds the routing's video.
    pub fn pin_guarded(&self, routing: &SoulseekRouting) -> bool {
        matches!(self.pick_for(routing), PickState::Person(_) | PickState::Earlier)
    }

    /// The video the song plays for new work (a heart's stand-in, a download, Keep ready, radio,
    /// a copy fetched ahead): a person's choice when one applies, Octo's pick when one applies to a
    /// routing still marked chosen, else the routing's own video.
    pub fn song_video(&self, routing: &SoulseekRouting) -> Option<String> {
        match self.pick_for(routing) {
            PickState::Person(record) => Some(record.video),
            PickState::Octo(record) if routing.video_pick == SoulseekRouting::VIDEO_CHOSEN => {
                Some(record.video)
            }
            _ => routing.you_tube_id.clone().filter(|video| !video.is_empty()),
        }
    }

    /// What is known of the stream the song under `id` plays, only while its video is the song's
    /// video: a routing waiting for its switch has no facts to tell.
    pub fn song_stream_facts(&self, id: &str) -> Option<StreamFacts> {
        let facts = self.stream_facts(id)?;
        let routing = self.peek(id)?.snapshot();
        let pinned = routing.you_tube_id.clone().filter(|video| !video.is_empty());
        (self.song_video(&routing) == pinned).then_some(facts)
    }

    /// Whether the song under `id` waits for its next play's start to take another video.
    pub fn awaits_switch(&self, id: &str) -> bool {
        let Some(routing) = self.peek(id) else {
            return false;
        };
        let routing = routing.snapshot();
        self.song_video(&routing)
            .is_some_and(|video| routing.you_tube_id.as_deref() != Some(video.as_str()))
    }

    /// Every id of the song `id` names that a choice of it reaches: `id` first, then the routings
    /// of its key the record applies to, then the ids it played under whose lengths fit.
    pub fn song_ids(&self, id: &str) -> Vec<String> {
        let Some((registered, routing)) = self.registered(id) else {
            return Vec::new();
        };
        let key = StreamChoices::key_of(&routing.lock());
        let Some(key) = key else {
            return vec![registered];
        };
        let record = self.choices.record_for_key(&key);
        let mut ids = vec![registered];
        let found = self
            .ids_for_keys(&[(key.clone(), record)])
            .remove(&key)
            .unwrap_or_default();
        for other in found {
            if !ids.contains(&other) {
                ids.push(other);
            }
        }
        ids
    }

    /// The ids of each song key, in one pass over the registry: the routings of the key its record
    /// (when it has one) applies to, then the played ids whose lengths fit it.
    fn ids_for_keys(&self, keys: &[(String, Option<StreamChoice>)]) -> HashMap<String, Vec<String>> {
        let mut found: HashMap<String, Vec<String>> = HashMap::new();
        if keys.is_empty() {
            return found;
        }
        let wanted: HashMap<&str, Option<&StreamChoice>> = keys
            .iter()
            .map(|(key, record)| (key.as_str(), record.as_ref()))
            .collect();
        let all = self.lru.lock().all();
        for (id, routing) in all {
            let r = routing.lock();
            let Some(key) = StreamChoices::key_of(&r) else {
                continue;
            };
            let Some(record) = wanted.get(key.as_str()) else {
                continue;
            };
            if let Some(record) = *record
                && !self.record_applies(&r, record)
            {
                continue;
            }
            drop(r);
            found.entry(key).or_default().push(id);
        }
        for (key, record) in keys {
            let ids = found.entry(key.clone()).or_default();
            for (id, seconds) in self.choices.played_ids(key) {
                let fits = record
                    .as_ref()
                    .is_none_or(|record| StreamChoices::fits(record, seconds));
                if fits && !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        found
    }

    /// Stores `choice` as a person's choice of the song `id` names, for every id of the song.
    /// Answers the ids it reaches. The song's catalog length is kept on it: this routing's, else
    /// the one every routing of the song agrees on.
    pub fn choose_stream_video(&self, id: &str, choice: StreamChoice) -> Result<Vec<String>, ChooseError> {
        self.store_choice(
            id,
            StreamChoice {
                auto: false,
                ..choice
            },
        )
    }

    /// Undoes a person's choice of the song `id` names: `octo` (Octo's own pick, found now) becomes
    /// its record. Routings still marked chosen take it at their next play's start.
    pub fn undo_stream_choice(&self, id: &str, octo: StreamChoice) -> Result<Vec<String>, ChooseError> {
        self.store_choice(
            id,
            StreamChoice {
                auto: true,
                by: String::new(),
                ..octo
            },
        )
    }

    fn store_choice(&self, id: &str, choice: StreamChoice) -> Result<Vec<String>, ChooseError> {
        let (_, routing) = self.registered(id).ok_or(ChooseError::NotASong)?;
        let snapshot = routing.snapshot();
        if snapshot.kind != RoutingKind::Song {
            return Err(ChooseError::NotASong);
        }
        let key = StreamChoices::key_of(&snapshot).ok_or(ChooseError::NoKey)?;
        if choice.video.trim().is_empty() || self.refused_for(&snapshot, &choice.video) {
            return Err(ChooseError::Refused);
        }
        let song_seconds = StreamChoices::catalog_seconds(&snapshot).or_else(|| self.agreed_seconds(&key));
        let choice = StreamChoice {
            song_seconds,
            ..choice
        };
        if let Some((evicted_key, evicted)) = self.choices.put(&key, choice) {
            self.settle_evicted(&evicted_key, &evicted);
        }
        Ok(self.song_ids(id))
    }

    /// The catalog length every routing of the song under `key` agrees on.
    fn agreed_seconds(&self, key: &str) -> Option<i32> {
        let all = self.lru.lock().all();
        let lengths: Vec<Option<i32>> = all
            .into_iter()
            .filter_map(|(_, routing)| {
                let r = routing.lock();
                (StreamChoices::key_of(&r).as_deref() == Some(key))
                    .then(|| StreamChoices::catalog_seconds(&r))
            })
            .collect();
        StreamChoices::agreed_seconds(lengths)
    }

    /// A record put out to make room. Octo's own pick: every routing of its song still marked
    /// chosen takes that video at once, so none plays an undone choice as an earlier one (plays
    /// bound to their videos keep them). A person's choice: its routings play on as earlier ones.
    fn settle_evicted(&self, key: &str, record: &StreamChoice) {
        if !record.auto {
            return;
        }
        let all = self.lru.lock().all();
        let mut touched = Vec::new();
        for (id, routing) in all {
            let switched = {
                let mut r = routing.lock();
                let fits = StreamChoices::key_of(&r).as_deref() == Some(key)
                    && r.video_pick == SoulseekRouting::VIDEO_CHOSEN
                    && !r.refuses_video(&record.video)
                    && StreamChoices::fits(record, StreamChoices::catalog_seconds(&r));
                if fits {
                    if let Some(old) = r.you_tube_id.clone().filter(|video| *video != record.video) {
                        r.unpin_video(&old);
                    }
                    r.pin_video(&record.video);
                    r.video_pick = SoulseekRouting::VIDEO_PICK;
                    r.pending_video = None;
                }
                fits
            };
            if switched {
                touched.push(id);
            }
        }
        if !touched.is_empty() {
            self.mark_changed_ids(&touched);
            self.flush_now();
        }
    }

    /// Drops what `user` heard of every song in `ids`, so the next play of the song is what they
    /// hear of it. Answers each record dropped, by id, for the holds that held its copy.
    pub fn forget_heard_of_song(&self, ids: &[String], user: &str) -> Vec<(String, HeardRecord)> {
        let user = user.trim();
        let mut dropped = Vec::new();
        for id in ids {
            let Some(routing) = self.peek(id) else {
                continue;
            };
            let record = {
                let mut r = routing.lock();
                let at = r.heard.iter().position(|known| eq_ignore_case(&known.user, user));
                at.map(|at| r.heard.remove(at).record)
            };
            if let Some(record) = record {
                dropped.push((id.clone(), record));
            }
        }
        if !dropped.is_empty() {
            let changed: Vec<String> = dropped.iter().map(|(id, _)| id.clone()).collect();
            self.mark_changed_ids(&changed);
        }
        dropped
    }

    /// At a play's start (or a look at the song), and at any request of a routing with no video:
    /// a person's choice that applies is pinned with the chosen mark (the old video's stream facts
    /// and a length learned from it go; the chosen video's length is kept as a video's, never
    /// over a catalog's), and a routing still marked chosen takes Octo's pick that applies. The
    /// ranking's waiting video is dropped. Answers the video left when no other routing pins it any
    /// more, so its copy can go. A changed video is written down at once.
    pub fn apply_choice_at_start(&self, routing: &SharedRouting) -> Option<String> {
        let snapshot = routing.snapshot();
        let (target, mark) = match self.pick_for(&snapshot) {
            PickState::Person(record) => (record, SoulseekRouting::VIDEO_CHOSEN),
            PickState::Octo(record) if snapshot.video_pick == SoulseekRouting::VIDEO_CHOSEN => {
                (record, SoulseekRouting::VIDEO_PICK)
            }
            _ => return None,
        };
        let (changed, moved, left) = {
            let mut r = routing.lock();
            let current = r.you_tube_id.clone().filter(|video| !video.is_empty());
            if current.as_deref() == Some(target.video.as_str()) {
                let changed = r.video_pick != mark || r.pending_video.is_some();
                r.video_pick = mark;
                r.pending_video = None;
                (changed, false, None)
            } else {
                if let Some(old) = &current {
                    r.unpin_video(old);
                }
                if r.pin_video(&target.video) {
                    r.video_pick = mark;
                    SongLength::remember(&mut r, target.seconds, LengthSource::Video);
                }
                r.pending_video = None;
                (true, true, current)
            }
        };
        if !changed {
            return None;
        }
        self.mark_changed_routing(routing);
        if moved {
            self.flush_now();
        }
        let left = left?;
        let pinned_elsewhere = self
            .lru
            .lock()
            .all()
            .into_iter()
            .filter(|(_, other)| !other.ptr_eq(routing))
            .any(|(_, other)| other.lock().you_tube_id.as_deref() == Some(left.as_str()));
        (!pinned_elsewhere).then_some(left)
    }

    /// Notes a play's start of the song `id` names in the played index.
    pub fn note_play(&self, id: &str) {
        let Some((registered, routing)) = self.registered(id) else {
            return;
        };
        let (key, seconds) = {
            let r = routing.lock();
            (StreamChoices::key_of(&r), StreamChoices::catalog_seconds(&r))
        };
        if let Some(key) = key {
            self.choices.note_played(&key, &registered, seconds);
        }
    }

    /// The chosen `video` of the song `id` names cannot be played any more (YouTube says it is
    /// gone or refused): a person's record of it is removed with a tombstone naming the song's
    /// ids, and every routing of the song marked chosen for it lets it go. Written down at once.
    /// True when anything changed.
    pub fn forget_pick_video(&self, id: &str, video: &str) -> bool {
        let Some((_, routing)) = self.registered(id) else {
            return false;
        };
        let key = StreamChoices::key_of(&routing.lock());
        let Some(key) = key else {
            return false;
        };
        let record = self
            .choices
            .record_for_key(&key)
            .filter(|record| !record.auto && record.video == video);
        let ids = self
            .ids_for_keys(&[(key.clone(), record.clone())])
            .remove(&key)
            .unwrap_or_default();
        let removed = record.is_some() && self.choices.remove(&key, Some(video), ids.clone()).is_some();
        let unpinned = self.unpin_chosen(&key, video);
        if unpinned && !removed {
            self.choices.note_removed(&key, ids);
        }
        if removed || unpinned {
            self.flush_now();
        }
        removed || unpinned
    }

    /// Unpins `video` from every routing of the song under `key` marked chosen for it.
    fn unpin_chosen(&self, key: &str, video: &str) -> bool {
        let all = self.lru.lock().all();
        let mut touched = Vec::new();
        for (id, routing) in all {
            let unpinned = {
                let mut r = routing.lock();
                StreamChoices::key_of(&r).as_deref() == Some(key)
                    && r.video_pick == SoulseekRouting::VIDEO_CHOSEN
                    && r.unpin_video(video)
            };
            if unpinned {
                touched.push(id);
            }
        }
        if !touched.is_empty() {
            self.mark_changed_ids(&touched);
        }
        !touched.is_empty()
    }

    /// A video refused for the song `artist - title`: a record that chose it is removed with a
    /// tombstone. Called with no lock held.
    pub(super) fn forget_refused_choice(&self, artist: &str, title: &str, video: &str) {
        for key in StreamChoices::keys_for_names(artist, title) {
            let chose_it = self
                .choices
                .record_for_key(&key)
                .is_some_and(|record| record.video == video);
            if !chose_it {
                continue;
            }
            let ids = self
                .ids_for_keys(&[(key.clone(), None)])
                .remove(&key)
                .unwrap_or_default();
            self.choices.remove(&key, Some(video), ids);
        }
    }

    /// Names the sample of what `user` heard of the song `id` names, only while the record kept
    /// is the one the sample was taken of (the same time and source). Never adds a record, so a
    /// sample finished after the record was dropped brings nothing back. Every heard listener is
    /// told of a changed record. True when it changed.
    pub fn attach_sample(
        &self,
        id: &str,
        user: &str,
        heard_at: DateTime<Utc>,
        source: &HeardSource,
        sample_id: &str,
    ) -> bool {
        let Some((registered, routing)) = self.registered(id) else {
            return false;
        };
        let user = user.trim();
        let record = {
            let mut r = routing.lock();
            if r.kind != RoutingKind::Song {
                return false;
            }
            let Some(known) = r.heard.iter_mut().find(|known| eq_ignore_case(&known.user, user)) else {
                return false;
            };
            if known.record.heard_at != heard_at
                || known.record.source != *source
                || known.record.sample_id.as_deref() == Some(sample_id)
            {
                return false;
            }
            known.record.sample_id = Some(sample_id.to_string());
            known.record.clone()
        };
        self.mark_changed(&registered);
        let listeners = self.heard_listeners.lock().clone();
        for listener in listeners {
            listener(&registered, user, &record);
        }
        true
    }

    /// The binding `play` goes on with on the song `id` names: with a token, the device's binding
    /// of that token; without, the device's binding from the same address, else its most recently
    /// bound one. Its `finished` as handed back means the play is over: a body reached the file's
    /// last byte and none of the play's bodies streams now.
    pub fn bound_play(&self, id: &str, play: &StreamPlay) -> Option<PlayBinding> {
        let (registered, routing) = self.registered(id)?;
        let mut binding = {
            let r = routing.lock();
            Self::binding_at(&r.plays, play).map(|at| r.plays[at].clone())?
        };
        if binding.finished
            && self
                .live_plays
                .lock()
                .get(&live_key_of(&registered, &binding))
                .is_some_and(|count| *count > 0)
        {
            binding.finished = false;
        }
        Some(binding)
    }

    /// Whether a copy of the song `external_id` fetched ahead, judged against `judged_video` when
    /// it landed, may stand for the song: always, unless a person's choice (or an earlier one)
    /// holds the song, and then only a copy judged against the very video the song plays. The one
    /// rule for serving, keeping and promoting copies fetched ahead.
    pub fn ahead_copy_serves(&self, external_id: &str, judged_video: Option<&str>) -> bool {
        let Some(routing) = self.resolve_song(external_id, false) else {
            return true;
        };
        let routing = routing.snapshot();
        if !self.pin_guarded(&routing) {
            return true;
        }
        let wanted = self.song_video(&routing);
        wanted.is_some() && wanted.as_deref() == judged_video
    }

    fn binding_at(plays: &[PlayBinding], play: &StreamPlay) -> Option<usize> {
        let mine = |binding: &PlayBinding| binding.key == play.key;
        match play.token.as_deref() {
            Some(token) => plays
                .iter()
                .rposition(|b| mine(b) && b.token.as_deref() == Some(token)),
            None => plays
                .iter()
                .rposition(|b| mine(b) && b.token.is_none() && b.address == play.address)
                .or_else(|| plays.iter().rposition(|b| mine(b) && b.token.is_none())),
        }
    }

    /// For a request without a token that starts a play: the same device's binding with a body
    /// streaming now. Two devices the server cannot tell apart are then served one play, and the
    /// second starts nothing.
    pub fn live_binding(&self, id: &str, play: &StreamPlay) -> Option<PlayBinding> {
        if play.token.is_some() {
            return None;
        }
        let (registered, routing) = self.registered(id)?;
        let plays = routing.lock().plays.clone();
        let live = self.live_plays.lock();
        plays.into_iter().rev().find(|binding| {
            binding.key == play.key
                && binding.token.is_none()
                && live
                    .get(&live_key_of(&registered, binding))
                    .is_some_and(|count| *count > 0)
        })
    }

    /// Binds `play` to `video` on the song `id` names, after its stream opened. A new token makes
    /// its own binding (a device keeps two token bindings of a song, the oldest without a body
    /// streaming going first). Without a token, the device's finished bindings with no body
    /// streaming go, and the one from the same address is replaced. At most
    /// [`SoulseekRouting::MAX_PLAYS`] a song, the least recently bound first out.
    pub fn bind_play(&self, id: &str, play: &StreamPlay, video: &str) {
        if play.peeks() || video.is_empty() {
            return;
        }
        let Some((registered, routing)) = self.registered(id) else {
            return;
        };
        let live: HashSet<String> = self
            .live_plays
            .lock()
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(key, _)| key.clone())
            .collect();
        {
            let mut r = routing.lock();
            let plays = &mut r.plays;
            let streaming = |binding: &PlayBinding| live.contains(&live_key_of(&registered, binding));
            let fresh = PlayBinding {
                key: play.key.clone(),
                token: play.token.clone(),
                address: play.address.clone(),
                video: video.to_string(),
                size: None,
                finished: false,
            };
            if play.token.is_some() {
                plays.retain(|b| !(b.key == fresh.key && b.token == fresh.token));
                plays.push(fresh);
                let tokened = |b: &PlayBinding| b.key == play.key && b.token.is_some();
                while plays.iter().filter(|&b| tokened(b)).count() > 2 {
                    let at = plays
                        .iter()
                        .position(|b| tokened(b) && !streaming(b))
                        .or_else(|| plays.iter().position(&tokened));
                    match at {
                        Some(at) => {
                            plays.remove(at);
                        }
                        None => break,
                    }
                }
            } else {
                plays.retain(|b| {
                    let replaced = b.address == fresh.address || (b.finished && !streaming(b));
                    !(b.key == fresh.key && b.token.is_none() && replaced)
                });
                plays.push(fresh);
            }
            let excess = plays.len().saturating_sub(SoulseekRouting::MAX_PLAYS);
            plays.drain(..excess);
        }
        self.mark_changed(&registered);
    }

    /// Keeps `size` as the whole size of the video `play`'s binding plays.
    pub fn confirm_play_size(&self, id: &str, play: &StreamPlay, size: u64) {
        self.change_binding(id, play, |binding| {
            let changed = binding.size != Some(size);
            binding.size = Some(size);
            changed
        });
    }

    /// Marks `play`'s binding finished: a body of it reached the file's last byte.
    pub fn finish_play(&self, id: &str, play: &StreamPlay) {
        self.change_binding(id, play, |binding| {
            let changed = !binding.finished;
            binding.finished = true;
            changed
        });
    }

    fn change_binding(&self, id: &str, play: &StreamPlay, change: impl FnOnce(&mut PlayBinding) -> bool) {
        if play.peeks() {
            return;
        }
        let Some((registered, routing)) = self.registered(id) else {
            return;
        };
        let changed = {
            let mut r = routing.lock();
            match Self::binding_at(&r.plays, play) {
                Some(at) => change(&mut r.plays[at]),
                None => false,
            }
        };
        if changed {
            self.mark_changed(&registered);
        }
    }

    /// Counts a body of `play` on the song `id` names as streaming until the guard is dropped. A
    /// look at the song (a HEAD, a scout read) never counts.
    pub fn live_play(&self, id: &str, play: &StreamPlay) -> LiveGuard {
        let key = (!play.peeks()).then(|| {
            let registered = self
                .registered(id)
                .map(|(registered, _)| registered)
                .unwrap_or_else(|| id.to_string());
            live_key(
                &registered,
                &play.key,
                play.token.as_deref(),
                play.address.as_deref(),
            )
        });
        if let Some(key) = &key {
            *self.live_plays.lock().entry(key.clone()).or_default() += 1;
        }
        LiveGuard {
            live: Arc::clone(&self.live_plays),
            key,
        }
    }

    /// Writes the registry now, off the async workers' tasks: a chosen video changed.
    pub fn flush_now(&self) {
        store_file::blocking(|| self.flush());
    }

    /// The changes after `since` with each changed song's ids: its routings and played ids, and
    /// the ids its removed records named. Incomplete also when the played index lost ids and a
    /// changed song is not in it.
    pub fn changes_since(&self, epoch: Option<&str>, since: Option<u64>, limit: usize) -> ChangeFeed {
        let page = self.choices.changes_since(epoch, since, limit);
        let keys: Vec<(String, Option<StreamChoice>)> = page
            .changes
            .iter()
            .map(|(_, key)| (key.clone(), self.choices.record_for_key(key)))
            .collect();
        let ids = self.ids_for_keys(&keys);
        let lossy = self.choices.lossy();
        let mut complete = page.complete;
        let mut changes = Vec::with_capacity(page.changes.len());
        for (seq, key) in page.changes {
            let mut found = ids.get(&key).cloned().unwrap_or_default();
            for id in self.choices.removed_ids(&key) {
                if !found.contains(&id) {
                    found.push(id);
                }
            }
            if lossy && !self.choices.knows_played(&key) {
                complete = false;
            }
            changes.push((seq, found));
        }
        ChangeFeed {
            epoch: page.epoch,
            seq: page.seq,
            complete,
            changes,
        }
    }

    /// A seed about to be restored: its bindings go (a seed is a copy from any time), a chosen
    /// mark no record backs becomes Octo's pick, and under a person's choice the chooser's heard
    /// records older than the choice go.
    pub(super) fn settle_restored(&self, routing: &mut SoulseekRouting) {
        routing.plays.clear();
        match self.pick_for(routing) {
            PickState::Person(record) => {
                let chooser = record.by.trim().to_string();
                routing.heard.retain(|known| {
                    !(eq_ignore_case(&known.user, &chooser) && known.record.heard_at < record.at)
                });
            }
            PickState::Earlier => routing.video_pick = SoulseekRouting::VIDEO_PICK,
            PickState::Octo(_) | PickState::None => {}
        }
    }
}
