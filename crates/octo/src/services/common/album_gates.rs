//! The gates of album hearts: each catalog track of a hearted album and the recording it must
//! prove to be, kept while the album's chains run, and how one file of the album is filed by its
//! own sound against its own track.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use octo_core::common::dotnet;
use octo_core::fingerprint::heart_gate::{
    Acceptance, AcquisitionOrigin, GateVerdict, HeardReading, HeardRecording, HeartGate, Refusal, TagClaim,
    WaitingFor,
};
use octo_core::fingerprint::verification::{AnswerReading, Reading, core_title, length_fits};
use parking_lot::Mutex;

use super::AcquisitionHeld;

/// One catalog track of a hearted album and the recording it must prove to be.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlbumGateTrack {
    /// The album listing's id of the track, which its download runs under.
    pub catalog_track_id: String,
    pub disc: Option<i32>,
    pub position: Option<i32>,
    pub title: String,
    pub artist: String,
    pub duration_seconds: Option<i32>,
    /// The track's own recording: its catalog row's, with the recordings MusicBrainz files under
    /// its code.
    pub r: HeardRecording,
}

/// One person's gate of one hearted album: every catalog track in catalog order, each with its own
/// recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumGate {
    pub user: String,
    pub origin: AcquisitionOrigin,
    pub tracks: Vec<AlbumGateTrack>,
}

impl AlbumGate {
    /// The catalog track the album lists under `track_id`.
    pub fn track(&self, track_id: &str) -> Option<&AlbumGateTrack> {
        self.tracks
            .iter()
            .find(|track| track.catalog_track_id == track_id)
    }

    /// The gate one track's file is judged by: the person, the album's origin and the track's own
    /// recording, with nothing heard.
    pub fn gate_for(&self, track: &AlbumGateTrack) -> HeartGate {
        HeartGate {
            user: self.user.trim().to_string(),
            origin: self.origin,
            r: Some(track.r.clone()),
            heard: None,
            heard_seconds: track.duration_seconds.filter(|seconds| *seconds > 0),
            reading: HeardReading::NotHeard,
            stand_in: None,
        }
    }
}

/// Where a store entry is kept: the provider, the album's id and the person, lowercased.
type GateKey = (String, String, String);

fn gate_key(provider: &str, album_id: &str, user: &str) -> GateKey {
    (
        dotnet::to_lower_invariant(provider.trim()),
        album_id.to_string(),
        dotnet::to_lower_invariant(user.trim()),
    )
}

/// A registered gate, with how many album chains hold it and what its walks held.
struct Registered {
    gate: AlbumGate,
    holders: usize,
    /// Tracks a walk of the album held in the one hold path, each with its files, until the
    /// chain files them.
    held: Vec<(String, AcquisitionHeld)>,
    /// Tracks a walk of the album leaves alone: held already, or filed by an earlier source of
    /// the same chain.
    settled: HashSet<String>,
}

/// The album gates of the album chains that run now, keyed by (provider, album id, person), each
/// with a count of the chains that registered it: two chains of one album (two people with Family
/// off, a resume beside a heart) never remove each other's gate, and the gate goes only when the
/// last of them ends.
#[derive(Default)]
pub struct AlbumGates {
    entries: Mutex<HashMap<GateKey, Registered>>,
}

/// One chain's hold on an album gate: dropping it ends that chain's hold.
pub struct AlbumGateHold {
    gates: Arc<AlbumGates>,
    key: GateKey,
}

impl Drop for AlbumGateHold {
    fn drop(&mut self) {
        let mut entries = self.gates.entries.lock();
        if let Some(entry) = entries.get_mut(&self.key) {
            entry.holders = entry.holders.saturating_sub(1);
            if entry.holders == 0 {
                entries.remove(&self.key);
            }
        }
    }
}

impl AlbumGates {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one chain's gate of an album for its person, answering the chain's hold on it. A
    /// chain registering the same album for the same person while another runs adds one to the
    /// count and keeps the gate already there; `settled` tracks are left alone by its walks.
    pub fn register(
        self: &Arc<Self>,
        provider: &str,
        album_id: &str,
        gate: AlbumGate,
        settled: impl IntoIterator<Item = String>,
    ) -> AlbumGateHold {
        let key = gate_key(provider, album_id, &gate.user);
        let mut entries = self.entries.lock();
        let entry = entries.entry(key.clone()).or_insert_with(|| Registered {
            gate,
            holders: 0,
            held: Vec::new(),
            settled: HashSet::new(),
        });
        entry.holders += 1;
        entry.settled.extend(settled);
        AlbumGateHold {
            gates: Arc::clone(self),
            key,
        }
    }

    /// The gate of an album for one person, while a chain holds it.
    pub fn gate(&self, provider: &str, album_id: &str, user: &str) -> Option<AlbumGate> {
        self.entries
            .lock()
            .get(&gate_key(provider, album_id, user))
            .map(|entry| entry.gate.clone())
    }

    /// The gates of an album for these people, each with the gate of the track the album lists
    /// under `track_id`; a person with no gate of the album is left out.
    pub fn track_gates(
        &self,
        provider: &str,
        album_id: &str,
        users: &[String],
        track_id: &str,
    ) -> Vec<HeartGate> {
        let entries = self.entries.lock();
        users
            .iter()
            .filter_map(|user| entries.get(&gate_key(provider, album_id, user)))
            .filter_map(|entry| entry.gate.track(track_id).map(|track| entry.gate.gate_for(track)))
            .collect()
    }

    /// The album gate of one person that lists `track_id`, with the album's id and that track:
    /// a per-song chain of an album finds its track's gate here.
    pub fn for_track(
        &self,
        provider: &str,
        track_id: &str,
        user: &str,
    ) -> Option<(String, AlbumGate, AlbumGateTrack)> {
        let provider = dotnet::to_lower_invariant(provider.trim());
        let user = dotnet::to_lower_invariant(user.trim());
        self.entries
            .lock()
            .iter()
            .filter(|((p, _, u), _)| *p == provider && *u == user)
            .find_map(|((_, album, _), entry)| {
                entry
                    .gate
                    .track(track_id)
                    .map(|track| (album.clone(), entry.gate.clone(), track.clone()))
            })
    }

    /// A walk of the album held one track in the one hold path, for each of these people with a
    /// gate of it; their chain files it. The track is left alone by the album's later walks.
    pub fn held(
        &self,
        provider: &str,
        album_id: &str,
        users: &[String],
        track_id: &str,
        held: AcquisitionHeld,
    ) {
        let mut entries = self.entries.lock();
        for user in users {
            if let Some(entry) = entries.get_mut(&gate_key(provider, album_id, user)) {
                entry.held.push((track_id.to_string(), held.clone()));
                entry.settled.insert(track_id.to_string());
            }
        }
    }

    /// The tracks a walk held for one person, taken for their chain to file.
    pub fn take_held(&self, provider: &str, album_id: &str, user: &str) -> Vec<(String, AcquisitionHeld)> {
        self.entries
            .lock()
            .get_mut(&gate_key(provider, album_id, user))
            .map(|entry| std::mem::take(&mut entry.held))
            .unwrap_or_default()
    }

    /// Whether every one of these people's gates of the album leaves `track_id` alone.
    pub fn settled(&self, provider: &str, album_id: &str, users: &[String], track_id: &str) -> bool {
        let entries = self.entries.lock();
        let gated: Vec<&Registered> = users
            .iter()
            .filter_map(|user| entries.get(&gate_key(provider, album_id, user)))
            .collect();
        !gated.is_empty() && gated.iter().all(|entry| entry.settled.contains(track_id))
    }
}

/// The line an album heart's downloads row shows while it holds `held` tracks: one track's own
/// words for one, the plural for more.
pub fn tracks_wait_line(held: usize) -> String {
    if held == 1 {
        "1 track waits: play it once, or keep it anyway".to_string()
    } else {
        format!("{held} tracks wait: play them once, or keep them anyway")
    }
}

/// Whether a file's own title tag and length fit a catalog track: the same core title, and a
/// length within max(7 s, 4%) of the track's (the title alone when the catalog gives no length).
/// A file with no title tag or no length fits nothing.
pub fn fits(track: &AlbumGateTrack, title: Option<&str>, seconds: Option<i32>) -> bool {
    let (Some(title), Some(seconds)) = (title, seconds.filter(|seconds| *seconds > 0)) else {
        return false;
    };
    let core = core_title(title);
    if core.is_empty() || core != core_title(&track.title) {
        return false;
    }
    track
        .duration_seconds
        .filter(|want| *want > 0)
        .is_none_or(|want| length_fits(f64::from(seconds), f64::from(want)))
}

/// One file's own title tag and length, as [`fits`] reads them.
pub fn title_and_length(path: &str) -> (Option<String>, Option<i32>) {
    match octo_media::tags::TagFile::open(path) {
        Ok(file) => (
            file.title().filter(|title| !title.trim().is_empty()),
            Some(file.duration_seconds()).filter(|seconds| *seconds > 0),
        ),
        Err(_) => (None, None),
    }
}

/// One track's gate's verdict on a file, with what the album's rule reads beside it.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackCheck {
    /// The track's place in the album gate's list.
    pub track: usize,
    /// The track gate's own verdict.
    pub verdict: GateVerdict,
    /// The file's reading against the track's recording, when AcoustID answered.
    pub reading: Option<AnswerReading>,
    /// The track's recording names no recording to prove.
    pub no_recording: bool,
    /// The file's own title tag and length fit the track.
    pub fits: bool,
}

/// Where one file of an album heart is filed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackFiling {
    /// Its sound proves it is this track: it lands as the track, tagged as `tags` says.
    Land {
        track: usize,
        by: Acceptance,
        tags: TagClaim,
    },
    /// It waits in the one hold path, tied to this track, for `waiting_for`.
    Hold { track: usize, waiting_for: WaitingFor },
    /// It is none of the tracks it was read against.
    Refuse(Refusal),
}

/// Whether a check reads the file as the track's song in a version AcoustID cannot tell apart:
/// ambiguous, or clear against a recording built on a reading that was not clear.
fn ambiguous(check: &TrackCheck) -> bool {
    matches!(
        check.reading.as_ref().map(|reading| reading.reading),
        Some(Reading::Ambiguous | Reading::Clear)
    ) && !matches!(check.verdict, GateVerdict::Accept { .. })
}

/// Whether a check knows nothing of the file's recording: AcoustID has no entry for it, cannot
/// link it, is off, or the track names no recording to prove.
fn unknown(check: &TrackCheck) -> bool {
    check.no_recording
        || match check.reading.as_ref().map(|reading| reading.reading) {
            Some(Reading::Empty | Reading::Unknown) => true,
            Some(_) => false,
            None => matches!(check.verdict, GateVerdict::Undecided(_)),
        }
}

/// Files one file of an album by its own sound among tracks, `checks` in catalog order, each its
/// track gate's check of the file. Positions never decide, and a label is judged only against the
/// track the file is filed against:
///
/// - the first track whose gate accepts it (a clear reading against its recording, or the master a
///   person played) has it, whatever position the file came at;
/// - a track whose gate holds for an answer (AcoustID or MusicBrainz busy) holds it;
/// - a file read as a track's song in a version AcoustID cannot tell apart, or one AcoustID knows
///   nothing of whose own title tag and length fit a track, waits "not yet proven" tied to the
///   first such track its labels do not refuse; refused for its label only when its labels refuse
///   it for every such track;
/// - anything else is refused: another recording than every track, one AcoustID cannot read, one
///   the heard audio's print says is not what played, or an unknown file that fits no track.
pub fn file_among(checks: &[TrackCheck]) -> TrackFiling {
    // A track with no recording to prove is never proven by a reading against nothing; the very
    // master a person played still proves it.
    if let Some((check, by, tags)) = checks.iter().find_map(|check| match &check.verdict {
        GateVerdict::Accept { by, tags } if !(check.no_recording && *by == Acceptance::Sound) => {
            Some((check, *by, tags.clone()))
        }
        _ => None,
    }) {
        return TrackFiling::Land {
            track: check.track,
            by,
            tags,
        };
    }
    if let Some((check, waiting_for)) = checks.iter().find_map(|check| match check.verdict {
        GateVerdict::Hold(waiting_for) => Some((check, waiting_for)),
        _ => None,
    }) {
        return TrackFiling::Hold {
            track: check.track,
            waiting_for,
        };
    }
    let fitting: Vec<&TrackCheck> = checks
        .iter()
        .filter(|check| {
            !matches!(
                check.verdict,
                GateVerdict::Refuse(Refusal::DiffersFromHeard | Refusal::Unreadable)
            )
        })
        .filter(|check| (ambiguous(check) && !check.no_recording) || (unknown(check) && check.fits))
        .filter(|check| {
            check.no_recording
                || check
                    .reading
                    .as_ref()
                    .is_none_or(|reading| reading.reading != Reading::Another)
        })
        .collect();
    if let Some(check) = fitting
        .iter()
        .find(|check| check.verdict != GateVerdict::Refuse(Refusal::Label))
    {
        return TrackFiling::Hold {
            track: check.track,
            waiting_for: WaitingFor::NotYetProven,
        };
    }
    if !fitting.is_empty() {
        return TrackFiling::Refuse(Refusal::Label);
    }
    let first = checks.first();
    TrackFiling::Refuse(match first.map(|check| &check.verdict) {
        Some(GateVerdict::Refuse(refusal)) => *refusal,
        _ => match first
            .and_then(|check| check.reading.as_ref())
            .map(|reading| reading.reading)
        {
            Some(Reading::Empty) => Refusal::NoEntry,
            Some(Reading::Another) => Refusal::AnotherRecording,
            _ => Refusal::Unknown,
        },
    })
}

/// One track gate's verdict on a file of an album heart under the album's rule, as a gate's
/// verdict: it lands when the gate accepts it, waits "not yet proven" when its version cannot be
/// told apart or AcoustID knows nothing of it and it fits the track, and is refused otherwise.
pub fn album_verdict(check: TrackCheck) -> GateVerdict {
    match file_among(std::slice::from_ref(&check)) {
        TrackFiling::Land { by, tags, .. } => GateVerdict::Accept { by, tags },
        TrackFiling::Hold { waiting_for, .. } => GateVerdict::Hold(waiting_for),
        TrackFiling::Refuse(refusal) => GateVerdict::Refuse(refusal),
    }
}

#[cfg(test)]
#[path = "album_gates_tests.rs"]
mod tests;
