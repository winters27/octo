//! What becomes of anything that would add a song to a library when a family member asks: a
//! heart on an outside song or album, a play that would keep a copy, an import list's missing
//! songs, a copy picked in Find songs, a library action. The owner and accounts outside the
//! family are never stopped here, and with Family off nothing is.
//!
//! - `Direct` members add at once, into the family pool and linked into their own library,
//!   until their library is full; past that their adds become requests. One whose music goes
//!   to the shared library adds into it, as someone outside the family does, with no storage
//!   limit.
//! - `Request` members ask for a copy (one request per album, one per import list). A heart
//!   still saves the song as a link. An approved copy lands where their music goes.
//! - `SaveOnly` members save songs and albums; anything else that would add one is refused.

use std::sync::Arc;

use octo_core::family::abilities::{AddToLibrary, MusicGoesTo};
use octo_core::family::policy::storage_full;
use octo_core::settings::SettingsStore;

use super::caller::FamilyCaller;
use super::family_store::FamilyStore;
use super::personal_library::PersonalLibrary;
use crate::services::common::AcquisitionTarget;

/// What a member reads when their account only saves songs.
pub const SAVE_ONLY_MESSAGE: &str = "Adding songs to your library is off for this account.";

/// What happens to one library write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteDecision {
    /// Into the shared library, as always: someone outside the family (or Family is off), or a
    /// `Direct` member whose music goes there.
    Shared,
    /// Into the family pool, linked into this member's library.
    Member(String),
    /// Becomes a request. `full` is the message when the member's library is full (a `Direct`
    /// member past their storage limit).
    Request { user: String, full: Option<String> },
    /// Not allowed for this member, with the words they read.
    Refuse { user: String, message: String },
}

impl WriteDecision {
    /// Where an acquisition goes when it may run now.
    pub fn target(&self) -> Option<AcquisitionTarget> {
        match self {
            WriteDecision::Shared => Some(AcquisitionTarget::Shared),
            WriteDecision::Member(user) => Some(AcquisitionTarget::Member(user.clone())),
            _ => None,
        }
    }
}

/// What a heart on an outside song or album does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeartOutcome {
    /// Download it, as hearts always have, into this library.
    Download(AcquisitionTarget),
    /// Save it as a link (it plays from the source). With `request`, a request for a copy is
    /// made too: a `Direct` member whose library is full, with the message saying so.
    Save { user: String, request: Option<String> },
}

/// The gate. See the module comment.
pub struct LibraryWriteGate {
    settings: Arc<SettingsStore>,
    family: Arc<FamilyStore>,
    personal: Arc<PersonalLibrary>,
}

impl LibraryWriteGate {
    pub fn new(
        settings: Arc<SettingsStore>,
        family: Arc<FamilyStore>,
        personal: Arc<PersonalLibrary>,
    ) -> Self {
        LibraryWriteGate {
            settings,
            family,
            personal,
        }
    }

    pub fn personal(&self) -> &Arc<PersonalLibrary> {
        &self.personal
    }

    /// Whether `user` is a family member Octo manages (Family on).
    pub fn is_member(&self, user: Option<&str>) -> bool {
        self.settings.current().family.enabled
            && user
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .is_some_and(|u| self.family.member(u).is_some())
    }

    /// What happens when `user` adds a song to their library.
    pub fn decide(&self, user: Option<&str>) -> WriteDecision {
        if !self.settings.current().family.enabled {
            return WriteDecision::Shared;
        }
        let Some(member) = user
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .and_then(|u| self.family.member(u))
        else {
            return WriteDecision::Shared;
        };
        let abilities = member.abilities();
        match abilities.add_to_library {
            AddToLibrary::Direct if abilities.music_goes_to == MusicGoesTo::Shared => WriteDecision::Shared,
            AddToLibrary::Direct => {
                match storage_full(&abilities, self.personal.storage_used(&member.username)) {
                    Some(full) => WriteDecision::Request {
                        user: member.username,
                        full: Some(full),
                    },
                    None => WriteDecision::Member(member.username),
                }
            }
            AddToLibrary::Request => WriteDecision::Request {
                user: member.username,
                full: None,
            },
            AddToLibrary::SaveOnly => WriteDecision::Refuse {
                user: member.username,
                message: SAVE_ONLY_MESSAGE.to_string(),
            },
        }
    }

    /// Whether what `user` asks for waits for someone to approve it: a member who asks for
    /// copies, unless their requests are approved at once and their library has room.
    pub fn waits_for_approval(&self, user: &str) -> bool {
        if !matches!(self.decide(Some(user)), WriteDecision::Request { .. }) {
            return false;
        }
        let Some(member) = self.family.member(user.trim()) else {
            return false;
        };
        let abilities = member.abilities();
        !(abilities.auto_approve
            && storage_full(&abilities, self.personal.storage_used(&member.username)).is_none())
    }

    /// Where an approved request of `user`'s lands: their own library, or the shared library
    /// when their music goes there.
    pub fn request_target(&self, user: &str) -> AcquisitionTarget {
        let user = user.trim();
        match self.family.member(user) {
            Some(member) if member.abilities().music_goes_to == MusicGoesTo::Own => {
                AcquisitionTarget::Member(member.username)
            }
            Some(_) => AcquisitionTarget::Shared,
            None => AcquisitionTarget::Member(user.to_string()),
        }
    }

    /// Who approves members' requests, as the family knows them: the owner.
    pub fn approver_name(&self) -> String {
        self.family.owner()
    }

    /// [`Self::decide`] for the person behind a request: only a member the family gate signed
    /// in is ever stopped.
    pub fn decide_for(&self, caller: Option<&FamilyCaller>) -> WriteDecision {
        match caller.filter(|c| c.managed) {
            Some(caller) => self.decide(Some(&caller.user)),
            None => WriteDecision::Shared,
        }
    }

    /// What a heart on an outside song or album does for this caller.
    pub fn heart(&self, caller: Option<&FamilyCaller>) -> HeartOutcome {
        match self.decide_for(caller) {
            WriteDecision::Shared => HeartOutcome::Download(AcquisitionTarget::Shared),
            WriteDecision::Member(user) => HeartOutcome::Download(AcquisitionTarget::Member(user)),
            WriteDecision::Request { user, full } => HeartOutcome::Save { user, request: full },
            WriteDecision::Refuse { user, .. } => HeartOutcome::Save { user, request: None },
        }
    }

    /// Where a play that keeps a copy (download on play, the lossless wait) goes for `user`;
    /// None skips it (anyone who would have to ask first, or whose library is full).
    pub fn play_target(&self, user: Option<&str>) -> Option<AcquisitionTarget> {
        self.decide(user).target()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::family::test_support::{World, member};
    use octo_core::family::abilities::Preset;
    use serde_json::json;

    fn gate(world: &World) -> LibraryWriteGate {
        let personal = Arc::new(PersonalLibrary::new(
            world.settings.clone(),
            world.family.clone(),
            world.index.clone(),
            world.pool.clone(),
        ));
        LibraryWriteGate::new(world.settings.clone(), world.family.clone(), personal)
    }

    #[test]
    fn each_preset_writes_its_own_way() {
        let world = World::new(None);
        world.family.upsert_member(member("kim", Preset::Kid, 5));
        let mut save_only = member("pat", Preset::Listener, 6);
        save_only.edits.insert("AddToLibrary".into(), json!("SaveOnly"));
        world.family.upsert_member(save_only);
        let gate = gate(&world);
        assert_eq!(gate.decide(Some("Alex")), WriteDecision::Member("alex".into()));
        assert_eq!(
            gate.decide(Some("sam")),
            WriteDecision::Request {
                user: "sam".into(),
                full: None
            }
        );
        assert!(matches!(gate.decide(Some("kim")), WriteDecision::Request { .. }));
        assert_eq!(
            gate.decide(Some("pat")),
            WriteDecision::Refuse {
                user: "pat".into(),
                message: SAVE_ONLY_MESSAGE.into()
            }
        );
        assert_eq!(gate.decide(Some("winters")), WriteDecision::Shared);
        assert_eq!(gate.decide(None), WriteDecision::Shared);
        assert_eq!(
            gate.play_target(Some("alex")),
            Some(AcquisitionTarget::Member("alex".into()))
        );
        assert_eq!(
            gate.play_target(Some("sam")),
            None,
            "a listener's play keeps no copy"
        );
        assert_eq!(gate.play_target(Some("winters")), Some(AcquisitionTarget::Shared));
    }

    #[tokio::test]
    async fn over_limit_direct_add_becomes_request() {
        let world = World::new(None);
        let mut small = member("alex", Preset::Member, 3);
        small.edits.insert("StorageLimitGb".into(), json!(1));
        world.family.upsert_member(small);
        let gate = gate(&world);
        assert_eq!(gate.decide(Some("alex")), WriteDecision::Member("alex".into()));
        let pooled = world.pool_file("A/B/big.flac", 10);
        world.pool.link(&pooled, "alex").await.expect("linked");
        world.index.note_placed(
            3,
            &format!("{}/alex/A/B/big.flac", world.members()),
            1_200_000_000,
        );
        match gate.decide(Some("alex")) {
            WriteDecision::Request { user, full } => {
                assert_eq!(user, "alex");
                assert_eq!(full.as_deref(), Some("Your library is full (1.2 of 1 GB)"));
            }
            other => panic!("expected a request, got {other:?}"),
        }
        assert_eq!(gate.play_target(Some("alex")), None);
        let caller = FamilyCaller::member(
            "alex",
            Preset::Member,
            octo_core::family::abilities::Abilities::preset(Preset::Member),
        );
        assert_eq!(
            gate.heart(Some(&caller)),
            HeartOutcome::Save {
                user: "alex".into(),
                request: Some("Your library is full (1.2 of 1 GB)".into())
            }
        );
    }

    #[tokio::test]
    async fn music_that_goes_to_the_shared_library_lands_there_whatever_their_own_holds() {
        let world = World::new(None);
        let shared = |name: &str, preset: Preset, library: i64, add: Option<&str>| {
            let mut m = member(name, preset, library);
            m.edits.insert("MusicGoesTo".into(), json!("Shared"));
            if let Some(add) = add {
                m.edits.insert("AddToLibrary".into(), json!(add));
            }
            m.edits.insert("StorageLimitGb".into(), json!(1));
            m
        };
        world
            .family
            .upsert_member(shared("alex", Preset::Member, 3, None));
        world
            .family
            .upsert_member(shared("sam", Preset::Listener, 4, None));
        world
            .family
            .upsert_member(shared("pat", Preset::Listener, 6, Some("SaveOnly")));
        world.family.upsert_member(member("kim", Preset::Kid, 5));
        let gate = gate(&world);
        // Alex's own library is past its limit, which shared music never counts against.
        world.index.note_placed(
            3,
            &format!("{}/alex/A/B/big.flac", world.members()),
            2_000_000_000,
        );

        assert_eq!(gate.decide(Some("alex")), WriteDecision::Shared);
        assert_eq!(gate.play_target(Some("alex")), Some(AcquisitionTarget::Shared));
        let caller = FamilyCaller::member(
            "alex",
            Preset::Member,
            octo_core::family::abilities::Abilities::preset(Preset::Member),
        );
        assert_eq!(
            gate.heart(Some(&caller)),
            HeartOutcome::Download(AcquisitionTarget::Shared)
        );
        assert_eq!(gate.request_target("Alex"), AcquisitionTarget::Shared);
        // A listener still asks first; what is approved lands in the shared library.
        assert_eq!(
            gate.decide(Some("sam")),
            WriteDecision::Request {
                user: "sam".into(),
                full: None
            }
        );
        assert!(gate.waits_for_approval("sam"));
        assert_eq!(gate.request_target("sam"), AcquisitionTarget::Shared);
        // Save only stays save only.
        assert_eq!(
            gate.decide(Some("pat")),
            WriteDecision::Refuse {
                user: "pat".into(),
                message: SAVE_ONLY_MESSAGE.into()
            }
        );
        // Everyone else's approved songs land in their own library, as always.
        assert_eq!(
            gate.request_target("kim"),
            AcquisitionTarget::Member("kim".into())
        );
        assert_eq!(
            gate.request_target("gone"),
            AcquisitionTarget::Member("gone".into())
        );
    }

    #[test]
    fn family_off_and_the_owner_write_as_always() {
        let world = World::with_family(None, false);
        let gate = gate(&world);
        assert_eq!(gate.decide(Some("sam")), WriteDecision::Shared);
        let owner = FamilyCaller::unmanaged("winters", true);
        let on = World::new(None);
        let gate_on = super::LibraryWriteGate::new(
            on.settings.clone(),
            on.family.clone(),
            Arc::new(PersonalLibrary::new(
                on.settings.clone(),
                on.family.clone(),
                on.index.clone(),
                on.pool.clone(),
            )),
        );
        assert_eq!(
            gate_on.heart(Some(&owner)),
            HeartOutcome::Download(AcquisitionTarget::Shared)
        );
        assert_eq!(
            gate_on.heart(None),
            HeartOutcome::Download(AcquisitionTarget::Shared)
        );
        assert!(!gate.is_member(Some("alex")));
        assert!(gate_on.is_member(Some("alex")));
    }
}
