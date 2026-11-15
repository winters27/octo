//! Family playlists as each person sees them, and the changes each may make.
//!
//! - The owner and every member see every family playlist. Accounts outside the family do not.
//! - An entry waiting for approval is shown only to whoever added it and to approvers.
//! - An entry is served as the library song when it is shared or in the viewer's own library;
//!   otherwise as its outside version, when it has one.
//! - Anyone with `FamilyPlaylistsEdit` adds and removes songs (kids cannot) and makes
//!   playlists. Renaming needs the playlist's creator or `ManageFamily`; the approval switch
//!   (when making a playlist too) and deleting need `ManageFamily`.
//! - In a playlist that needs approval, an addition by someone who cannot approve waits, and
//!   [`PlaylistAddRequests`] is told so the approvers see it.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::family::abilities::Role;
use parking_lot::RwLock;

use super::caller::FamilyCaller;
use super::family_playlist_store::{
    FamilyAddRefusal, FamilyPlaylist, FamilyPlaylistEntry, FamilyPlaylistStore,
};

/// The scope of an entry whose library song is shared.
pub const SHARED_SCOPE: &str = "Shared";

/// The scope of an entry whose library song is in `user`'s own library.
pub fn personal_scope(user: &str) -> String {
    format!("Personal:{user}")
}

/// The owner every family playlist is listed under.
pub const FAMILY_OWNER: &str = "Family";

/// Told when an addition waits for an approver.
pub trait PlaylistAddRequests: Send + Sync {
    /// `entry` was added at position `index` of `playlist` by `by` and waits for approval.
    fn addition_waits(&self, playlist: &FamilyPlaylist, index: usize, entry: &FamilyPlaylistEntry, by: &str);
}

/// Tells no one.
pub struct NoPlaylistAddRequests;

impl PlaylistAddRequests for NoPlaylistAddRequests {
    fn addition_waits(&self, _: &FamilyPlaylist, _: usize, _: &FamilyPlaylistEntry, _: &str) {}
}

/// Where waiting additions are reported; tells no one until [`PlaylistAddHook::set`].
#[derive(Clone)]
pub struct PlaylistAddHook(Arc<RwLock<Arc<dyn PlaylistAddRequests>>>);

impl Default for PlaylistAddHook {
    fn default() -> Self {
        PlaylistAddHook(Arc::new(RwLock::new(Arc::new(NoPlaylistAddRequests))))
    }
}

impl PlaylistAddHook {
    pub fn set(&self, requests: Arc<dyn PlaylistAddRequests>) {
        *self.0.write() = requests;
    }

    pub fn get(&self) -> Arc<dyn PlaylistAddRequests> {
        Arc::clone(&self.0.read())
    }
}

/// Why a change was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistError {
    NotFound,
    /// The caller may not make this change; the message says why.
    NotAllowed(&'static str),
    /// A name that is blank.
    BlankName,
    /// The playlist changed while its songs were read.
    Changed,
    /// An add past a cap of songs found online, or one whose save failed.
    Refused(FamilyAddRefusal),
}

impl PlaylistError {
    pub fn message(self) -> &'static str {
        match self {
            PlaylistError::NotFound => "Playlist not found",
            PlaylistError::NotAllowed(message) => message,
            PlaylistError::BlankName => "A playlist needs a name.",
            PlaylistError::Changed => {
                "This family playlist changed while Octo read it, so nothing was taken out. Try again."
            }
            PlaylistError::Refused(refused) => refused.message(),
        }
    }
}

const CANNOT_EDIT: &str = "This account cannot change family playlists.";
const CANNOT_RENAME: &str = "Only the person who made this playlist or a family manager can change it.";
const CANNOT_DELETE: &str = "Only a family manager can delete a family playlist.";
const CANNOT_APPROVE: &str = "Only people who manage the family can ask for approval of additions.";

/// Whether the caller sees family playlists: the owner and members.
pub fn may_view(caller: &FamilyCaller) -> bool {
    caller.managed || caller.role == Role::Owner
}

/// Whether the caller adds and removes songs.
pub fn may_edit(caller: &FamilyCaller) -> bool {
    may_view(caller) && caller.abilities.family_playlists_edit
}

/// Whether the caller's additions go in at once in a playlist that needs approval.
pub fn is_approver(caller: &FamilyCaller) -> bool {
    may_view(caller) && caller.abilities.approve_requests
}

/// Whether the caller may rename the playlist or turn approval on or off.
pub fn may_rename(caller: &FamilyCaller, playlist: &FamilyPlaylist) -> bool {
    may_view(caller)
        && (caller.abilities.manage_family
            || (caller.abilities.family_playlists_edit && eq_ignore_case(&playlist.created_by, &caller.user)))
}

/// Whether the caller may delete the playlist.
pub fn may_delete(caller: &FamilyCaller) -> bool {
    may_view(caller) && caller.abilities.manage_family
}

/// Whether the caller may turn approval of additions on or off.
pub fn may_set_approval(caller: &FamilyCaller) -> bool {
    may_view(caller) && caller.abilities.manage_family
}

/// How an entry is served to one viewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Serve {
    /// The library song, by its Navidrome id. The outside id, when there is one, is the
    /// fallback if the library song cannot be read.
    Library { id: String, outside: Option<String> },
    /// The outside version, by its outside id.
    Outside(String),
}

/// One entry of the list a viewer is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownEntry {
    /// The entry's position in the stored playlist.
    pub stored: usize,
    pub serve: Serve,
}

/// Whether an entry's library song is one `viewer` can play.
fn library_visible(entry: &FamilyPlaylistEntry, viewer: &str) -> bool {
    let scope = entry.scope.trim();
    scope.is_empty()
        || eq_ignore_case(scope, SHARED_SCOPE)
        || scope
            .split_once(':')
            .is_some_and(|(kind, user)| eq_ignore_case(kind, "Personal") && eq_ignore_case(user, viewer))
}

/// The list `viewer` is shown, in order: waiting entries only for their adder and approvers,
/// and only entries with something this viewer can play.
pub fn shown_entries(playlist: &FamilyPlaylist, viewer: &FamilyCaller) -> Vec<ShownEntry> {
    let approver = is_approver(viewer);
    playlist
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| !entry.pending || approver || eq_ignore_case(&entry.added_by, &viewer.user))
        .filter_map(|(stored, entry)| {
            let outside = entry.outside_id.clone().filter(|id| !id.trim().is_empty());
            let serve = match entry.library_id.as_deref().filter(|id| !id.trim().is_empty()) {
                Some(id) if library_visible(entry, &viewer.user) => Serve::Library {
                    id: id.to_string(),
                    outside,
                },
                _ => Serve::Outside(outside?),
            };
            Some(ShownEntry { stored, serve })
        })
        .collect()
}

/// How many songs `viewer` is shown.
pub fn visible_count(playlist: &FamilyPlaylist, viewer: &FamilyCaller) -> usize {
    shown_entries(playlist, viewer).len()
}

/// The family playlists `viewer` sees (none for an account outside the family).
pub fn visible_playlists(store: &FamilyPlaylistStore, viewer: &FamilyCaller) -> Vec<FamilyPlaylist> {
    if may_view(viewer) {
        store.list()
    } else {
        Vec::new()
    }
}

/// One family playlist, when `viewer` sees it.
pub fn find(store: &FamilyPlaylistStore, viewer: &FamilyCaller, id: &str) -> Option<FamilyPlaylist> {
    may_view(viewer).then(|| store.get(id)).flatten()
}

/// Makes a new family playlist.
pub fn create(
    store: &FamilyPlaylistStore,
    caller: &FamilyCaller,
    name: &str,
    approve_additions: bool,
    now: DateTime<Utc>,
) -> Result<FamilyPlaylist, PlaylistError> {
    if !may_edit(caller) {
        return Err(PlaylistError::NotAllowed(CANNOT_EDIT));
    }
    if approve_additions && !may_set_approval(caller) {
        return Err(PlaylistError::NotAllowed(CANNOT_APPROVE));
    }
    if name.trim().is_empty() {
        return Err(PlaylistError::BlankName);
    }
    let made = store.create(name, &caller.user, now);
    if approve_additions {
        store.set_approve(&made.id, true);
    }
    store.get(&made.id).ok_or(PlaylistError::NotFound)
}

/// Renames a family playlist.
pub fn rename(
    store: &FamilyPlaylistStore,
    caller: &FamilyCaller,
    id: &str,
    name: &str,
) -> Result<FamilyPlaylist, PlaylistError> {
    let playlist = find(store, caller, id).ok_or(PlaylistError::NotFound)?;
    if !may_rename(caller, &playlist) {
        return Err(PlaylistError::NotAllowed(CANNOT_RENAME));
    }
    if name.trim().is_empty() {
        return Err(PlaylistError::BlankName);
    }
    store.rename(id, name);
    store.get(id).ok_or(PlaylistError::NotFound)
}

/// Turns approval of additions on or off.
pub fn set_approve(
    store: &FamilyPlaylistStore,
    caller: &FamilyCaller,
    id: &str,
    approve_additions: bool,
) -> Result<FamilyPlaylist, PlaylistError> {
    find(store, caller, id).ok_or(PlaylistError::NotFound)?;
    if !may_set_approval(caller) {
        return Err(PlaylistError::NotAllowed(CANNOT_APPROVE));
    }
    store.set_approve(id, approve_additions);
    store.get(id).ok_or(PlaylistError::NotFound)
}

/// Deletes a family playlist.
pub fn delete(store: &FamilyPlaylistStore, caller: &FamilyCaller, id: &str) -> Result<(), PlaylistError> {
    find(store, caller, id).ok_or(PlaylistError::NotFound)?;
    if !may_delete(caller) {
        return Err(PlaylistError::NotAllowed(CANNOT_DELETE));
    }
    if store.delete(id) {
        Ok(())
    } else {
        Err(PlaylistError::NotFound)
    }
}

/// A song to add: its library song and scope, and its outside id, as far as they are known,
/// with the outside version's length and explicit mark when the catalog gave them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewEntry {
    pub library_id: Option<String>,
    pub scope: String,
    pub outside_id: Option<String>,
    pub duration: Option<i64>,
    pub explicit_status: Option<String>,
}

/// Adds songs at the end. In a playlist that needs approval, a caller who cannot approve adds
/// them waiting, and `requests` is told of each. Answers how many were added.
pub fn add(
    store: &FamilyPlaylistStore,
    requests: &dyn PlaylistAddRequests,
    caller: &FamilyCaller,
    id: &str,
    songs: Vec<NewEntry>,
    now: DateTime<Utc>,
) -> Result<usize, PlaylistError> {
    let playlist = find(store, caller, id).ok_or(PlaylistError::NotFound)?;
    if !may_edit(caller) {
        return Err(PlaylistError::NotAllowed(CANNOT_EDIT));
    }
    let pending = playlist.approve_additions && !is_approver(caller);
    let entries: Vec<FamilyPlaylistEntry> = songs
        .into_iter()
        .filter(|song| song.library_id.is_some() || song.outside_id.is_some())
        .map(|song| FamilyPlaylistEntry {
            library_id: song.library_id,
            scope: song.scope,
            outside_id: song.outside_id,
            added_by: caller.user.clone(),
            added: now,
            pending,
            duration: song.duration,
            explicit_status: song.explicit_status,
            seed_id: None,
            seed: None,
        })
        .collect();
    let added = entries.len();
    if added == 0 {
        return Ok(0);
    }
    let first = playlist.entries.len();
    if !store
        .try_add_entries(id, entries.clone())
        .map_err(PlaylistError::Refused)?
    {
        return Err(PlaylistError::NotFound);
    }
    if pending && let Some(after) = store.get(id) {
        for (offset, entry) in entries.iter().enumerate() {
            requests.addition_waits(&after, first + offset, entry, &caller.user);
        }
    }
    Ok(added)
}

/// Removes the songs at these positions of the list the caller was shown.
pub fn remove_shown(
    store: &FamilyPlaylistStore,
    caller: &FamilyCaller,
    id: &str,
    shown_indexes: &[usize],
) -> Result<usize, PlaylistError> {
    let playlist = find(store, caller, id).ok_or(PlaylistError::NotFound)?;
    if !may_edit(caller) {
        return Err(PlaylistError::NotAllowed(CANNOT_EDIT));
    }
    let shown = shown_entries(&playlist, caller);
    let stored: Vec<usize> = shown_indexes
        .iter()
        .filter_map(|at| shown.get(*at).map(|entry| entry.stored))
        .collect();
    if !stored.is_empty() {
        store.remove_indexes(id, &stored);
    }
    Ok(stored.len())
}

/// The stored places of the songs these (id, occurrence) pairs name in the list `viewer` is
/// shown: the `n`th (1-based) shown entry that answers to `id`, by its library id or its
/// outside id. A pair that names nothing is left out (the song is already gone).
pub fn stored_places_of(
    playlist: &FamilyPlaylist,
    viewer: &FamilyCaller,
    songs: &[(String, usize)],
) -> Vec<usize> {
    let shown = shown_entries(playlist, viewer);
    let mut places: Vec<usize> = songs
        .iter()
        .filter_map(|(id, n)| {
            shown
                .iter()
                .filter(|entry| match &entry.serve {
                    Serve::Library { id: library, outside } => {
                        library == id || outside.as_deref() == Some(id)
                    }
                    Serve::Outside(outside) => outside == id,
                })
                .nth(n.checked_sub(1)?)
                .map(|entry| entry.stored)
        })
        .collect();
    places.sort_unstable();
    places.dedup();
    places
}

/// Removes the entries at these places of the stored playlist (places past its end are left
/// alone), only while the playlist's entries are still `expected` (the ones the places were
/// worked out from). Answers how many went.
pub fn remove_stored(
    store: &FamilyPlaylistStore,
    caller: &FamilyCaller,
    id: &str,
    expected: &[FamilyPlaylistEntry],
    stored: &[usize],
) -> Result<usize, PlaylistError> {
    let playlist = find(store, caller, id).ok_or(PlaylistError::NotFound)?;
    if !may_edit(caller) {
        return Err(PlaylistError::NotAllowed(CANNOT_EDIT));
    }
    let mut places: Vec<usize> = stored
        .iter()
        .copied()
        .filter(|at| *at < playlist.entries.len())
        .collect();
    places.sort_unstable();
    places.dedup();
    if !places.is_empty() && !store.remove_indexes_if(id, expected, &places) {
        return Err(PlaylistError::Changed);
    }
    Ok(places.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use octo_core::family::abilities::{Abilities, Preset};
    use parking_lot::Mutex;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 20, 18, 0, 0).unwrap()
    }

    fn caller(user: &str, preset: Preset) -> FamilyCaller {
        FamilyCaller::member(user, preset, Abilities::preset(preset))
    }

    fn owner() -> FamilyCaller {
        FamilyCaller::unmanaged("winters", true)
    }

    fn library(id: &str, scope: &str, outside: Option<&str>) -> NewEntry {
        NewEntry {
            library_id: Some(id.into()),
            scope: scope.into(),
            outside_id: outside.map(str::to_string),
            ..NewEntry::default()
        }
    }

    fn outside(id: &str) -> NewEntry {
        NewEntry {
            outside_id: Some(id.into()),
            ..NewEntry::default()
        }
    }

    #[derive(Default)]
    struct Told(Mutex<Vec<(usize, String)>>);

    impl PlaylistAddRequests for Told {
        fn addition_waits(&self, _: &FamilyPlaylist, index: usize, _: &FamilyPlaylistEntry, by: &str) {
            self.0.lock().push((index, by.to_string()));
        }
    }

    #[test]
    fn personal_entries_are_served_outside_to_everyone_else() {
        let store = FamilyPlaylistStore::in_memory();
        let list = create(&store, &owner(), "Road trip", false, now()).expect("made");
        let alex = caller("alex", Preset::Member);
        add(
            &store,
            &NoPlaylistAddRequests,
            &alex,
            &list.id,
            vec![
                library("s-shared", SHARED_SCOPE, None),
                library("s-alex", &personal_scope("alex"), Some("ext-deezer-song-1")),
                library("s-alex-only", &personal_scope("alex"), None),
            ],
            now(),
        )
        .expect("added");
        let list = store.get(&list.id).expect("there");
        let for_alex = shown_entries(&list, &alex);
        assert_eq!(for_alex.len(), 3);
        let sam = caller("sam", Preset::Listener);
        let for_sam = shown_entries(&list, &sam);
        assert_eq!(
            for_sam,
            vec![
                ShownEntry {
                    stored: 0,
                    serve: Serve::Library {
                        id: "s-shared".into(),
                        outside: None
                    }
                },
                ShownEntry {
                    stored: 1,
                    serve: Serve::Outside("ext-deezer-song-1".into())
                },
            ]
        );
        assert_eq!(visible_count(&list, &sam), 2);
    }

    #[test]
    fn waiting_entries_show_only_to_their_adder_and_approvers() {
        let store = FamilyPlaylistStore::in_memory();
        let list = create(&store, &owner(), "Road trip", true, now()).expect("made");
        let told = Told::default();
        let sam = caller("sam", Preset::Listener);
        let alex = caller("alex", Preset::Member);
        let co = caller("co", Preset::CoAdmin);
        assert_eq!(
            add(
                &store,
                &told,
                &sam,
                &list.id,
                vec![outside("ext-deezer-song-9")],
                now()
            ),
            Ok(1)
        );
        assert_eq!(
            add(
                &store,
                &told,
                &co,
                &list.id,
                vec![outside("ext-deezer-song-8")],
                now()
            ),
            Ok(1)
        );
        assert_eq!(*told.0.lock(), vec![(0, "sam".to_string())]);
        let list = store.get(&list.id).expect("there");
        assert!(list.entries[0].pending);
        assert!(!list.entries[1].pending, "an approver's addition goes in at once");
        assert_eq!(visible_count(&list, &sam), 2);
        assert_eq!(visible_count(&list, &alex), 1);
        assert_eq!(visible_count(&list, &co), 2);
        assert_eq!(visible_count(&list, &owner()), 2);
    }

    #[test]
    fn kids_and_outsiders_cannot_change_anything() {
        let store = FamilyPlaylistStore::in_memory();
        let list = create(&store, &owner(), "Road trip", false, now()).expect("made");
        let kid = caller("kid", Preset::Kid);
        assert_eq!(
            add(
                &store,
                &NoPlaylistAddRequests,
                &kid,
                &list.id,
                vec![outside("x")],
                now()
            ),
            Err(PlaylistError::NotAllowed(CANNOT_EDIT))
        );
        assert_eq!(
            create(&store, &kid, "Mine", false, now()),
            Err(PlaylistError::NotAllowed(CANNOT_EDIT))
        );
        assert!(visible_count(&list, &kid) == 0);
        let guest = FamilyCaller::unmanaged("guest", false);
        assert!(visible_playlists(&store, &guest).is_empty());
        assert_eq!(
            rename(&store, &guest, &list.id, "x"),
            Err(PlaylistError::NotFound)
        );
        assert_eq!(visible_playlists(&store, &kid).len(), 1, "kids still listen");
    }

    #[test]
    fn rename_needs_the_creator_and_delete_needs_a_manager() {
        let store = FamilyPlaylistStore::in_memory();
        let alex = caller("alex", Preset::Member);
        let sam = caller("sam", Preset::Listener);
        let list = create(&store, &alex, "Road trip", false, now()).expect("made");
        assert_eq!(list.created_by, "alex");
        assert_eq!(
            rename(&store, &sam, &list.id, "Mine"),
            Err(PlaylistError::NotAllowed(CANNOT_RENAME))
        );
        assert_eq!(
            rename(&store, &alex, &list.id, " ").map(|p| p.name),
            Err(PlaylistError::BlankName)
        );
        assert_eq!(
            rename(&store, &alex, &list.id, "Summer").map(|p| p.name),
            Ok("Summer".into())
        );
        assert_eq!(
            set_approve(&store, &owner(), &list.id, true).map(|p| p.approve_additions),
            Ok(true)
        );
        assert_eq!(
            delete(&store, &alex, &list.id),
            Err(PlaylistError::NotAllowed(CANNOT_DELETE))
        );
        assert_eq!(delete(&store, &owner(), &list.id), Ok(()));
        assert_eq!(delete(&store, &owner(), &list.id), Err(PlaylistError::NotFound));
    }

    #[test]
    fn removal_maps_from_the_list_the_viewer_saw() {
        let store = FamilyPlaylistStore::in_memory();
        let list = create(&store, &owner(), "Road trip", false, now()).expect("made");
        let alex = caller("alex", Preset::Member);
        let sam = caller("sam", Preset::Listener);
        add(
            &store,
            &NoPlaylistAddRequests,
            &alex,
            &list.id,
            vec![
                library("a1", &personal_scope("alex"), None),
                outside("o1"),
                outside("o2"),
            ],
            now(),
        )
        .expect("added");
        // Sam sees o1 and o2 at 0 and 1: removing 1 removes o2, stored at 2.
        assert_eq!(remove_shown(&store, &sam, &list.id, &[1, 7]), Ok(1));
        let left: Vec<Option<String>> = store
            .get(&list.id)
            .expect("there")
            .entries
            .into_iter()
            .map(|e| e.library_id.or(e.outside_id))
            .collect();
        assert_eq!(left, vec![Some("a1".into()), Some("o1".into())]);
    }

    #[test]
    fn removal_by_stored_place_ignores_repeats_and_places_past_the_end() {
        let store = FamilyPlaylistStore::in_memory();
        let list = create(&store, &owner(), "Road trip", false, now()).expect("made");
        let alex = caller("alex", Preset::Member);
        add(
            &store,
            &NoPlaylistAddRequests,
            &alex,
            &list.id,
            vec![outside("o0"), outside("o1"), outside("o2")],
            now(),
        )
        .expect("added");
        let kid = caller("kid", Preset::Kid);
        let current = store.get(&list.id).expect("there").entries;
        assert_eq!(
            remove_stored(&store, &kid, &list.id, &current, &[0]),
            Err(PlaylistError::NotAllowed(CANNOT_EDIT))
        );
        assert_eq!(
            remove_stored(&store, &alex, &list.id, &current, &[2, 2, 9]),
            Ok(1)
        );
        let left: Vec<Option<String>> = store
            .get(&list.id)
            .expect("there")
            .entries
            .into_iter()
            .map(|e| e.outside_id)
            .collect();
        assert_eq!(left, vec![Some("o0".into()), Some("o1".into())]);
    }

    #[test]
    fn removal_by_stored_place_refuses_a_changed_playlist() {
        let store = FamilyPlaylistStore::in_memory();
        let list = create(&store, &owner(), "Road trip", false, now()).expect("made");
        let alex = caller("alex", Preset::Member);
        add(
            &store,
            &NoPlaylistAddRequests,
            &alex,
            &list.id,
            vec![outside("o0"), outside("o1")],
            now(),
        )
        .expect("added");
        let before = store.get(&list.id).expect("there").entries;
        add(
            &store,
            &NoPlaylistAddRequests,
            &alex,
            &list.id,
            vec![outside("o2")],
            now(),
        )
        .expect("added");
        assert_eq!(
            remove_stored(&store, &alex, &list.id, &before, &[0]),
            Err(PlaylistError::Changed)
        );
        assert_eq!(store.get(&list.id).expect("there").entries.len(), 3);
    }

    #[test]
    fn songs_named_by_id_and_occurrence_find_their_stored_places() {
        let store = FamilyPlaylistStore::in_memory();
        let list = create(&store, &owner(), "Road trip", true, now()).expect("made");
        let alex = caller("alex", Preset::Member);
        let sam = caller("sam", Preset::Listener);
        add(
            &store,
            &NoPlaylistAddRequests,
            &alex,
            &list.id,
            vec![
                library("a", SHARED_SCOPE, None),
                library("b", SHARED_SCOPE, Some("ext-b")),
                library("a", SHARED_SCOPE, None),
                outside("o1"),
            ],
            now(),
        )
        .expect("added");
        // Sam's addition waits, so alex does not see it.
        add(
            &store,
            &NoPlaylistAddRequests,
            &sam,
            &list.id,
            vec![library("a", SHARED_SCOPE, None)],
            now(),
        )
        .expect("added");
        let list = store.get(&list.id).expect("there");
        let pair = |id: &str, n: usize| (id.to_string(), n);
        assert_eq!(stored_places_of(&list, &alex, &[pair("a", 2)]), vec![2]);
        assert_eq!(
            stored_places_of(&list, &alex, &[pair("a", 1), pair("a", 2)]),
            vec![0, 2]
        );
        assert_eq!(
            stored_places_of(&list, &alex, &[pair("ext-b", 1), pair("o1", 1)]),
            vec![1, 3],
            "an outside id answers too"
        );
        assert_eq!(
            stored_places_of(&list, &alex, &[pair("zzz", 1), pair("a", 9)]),
            Vec::<usize>::new(),
            "a song that is not there is left out"
        );
        assert_eq!(
            stored_places_of(&list, &alex, &[pair("a", 3)]),
            Vec::<usize>::new(),
            "a waiting entry alex cannot see is not counted"
        );
        assert_eq!(stored_places_of(&list, &owner(), &[pair("a", 3)]), vec![4]);
    }

    #[test]
    fn the_hook_tells_no_one_until_set() {
        let hook = PlaylistAddHook::default();
        let told = Arc::new(Told::default());
        let playlist = FamilyPlaylistStore::in_memory().create("x", "winters", now());
        let entry = FamilyPlaylistEntry {
            library_id: None,
            scope: String::new(),
            outside_id: Some("o".into()),
            added_by: "sam".into(),
            added: now(),
            pending: true,
            duration: None,
            explicit_status: None,
            seed_id: None,
            seed: None,
        };
        hook.get().addition_waits(&playlist, 0, &entry, "sam");
        hook.set(told.clone());
        hook.get().addition_waits(&playlist, 0, &entry, "sam");
        assert_eq!(told.0.lock().len(), 1);
    }
}
