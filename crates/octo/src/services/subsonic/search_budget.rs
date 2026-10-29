use crate::services::common::external_search_service;

/// Splits a client's requested song count between local library results and external
/// discovery results for search3 / search2.
///
/// This exists because the split used to be a flat local floor of 20 that could consume the
/// entire budget. Since 20 is also the Subsonic spec default for songCount (and this server's
/// own fallback when the parameter is absent), the most common search in the wild reserved
/// every slot for local results and generated no discovery at all, which is what made search
/// look like it only ever returned albums.
///
/// The rule that replaced it caps the floor at what the client actually asked for, so the two
/// targets always fit inside the requested count. That matters because the merge concatenates
/// local songs first and appends externals with no total cap: an external past the client's
/// songCount is the same as no external at all for any client that renders only what it
/// requested.
pub struct SearchBudget;

impl SearchBudget {
    /// How many local rows to reserve before discovery gets a share, unless
    /// `Subsonic:SearchLibraryOnlyUpTo` says otherwise. Capped at the requested count, so a
    /// client asking for fewer than this gets local-only results and no fan-out. That is what
    /// keeps per-keystroke type-ahead cheap: a search for five rows costs one relay, where
    /// generating discovery for it would cost a Last.fm fan-out plus a dozen Deezer enrichment
    /// rows and eight yt-dlp lookups.
    pub const LOCAL_SONG_FLOOR: i32 = 12;

    /// Ceiling on discovery rows handed to one response. Tied to the number actually built
    /// per query so a caller can never be promised more rows than exist: the build size is a
    /// constant precisely so concurrent callers wanting different amounts can share one
    /// execution.
    pub const EXTERNAL_CEILING: i32 = external_search_service::BUILD_SIZE;

    /// How many library rows page one of a search asks Navidrome for when the library comes
    /// first: the whole page. Outside songs then fill only what the library leaves empty
    /// (`SearchSongOrder::page_one_external_count` with no outside share of its own), so a song
    /// the user owns is listed once, as their copy, before any outside song, and outside songs
    /// never push owned ones to a later page. [`Self::compute`] still decides whether the request
    /// is big enough for discovery at all (a type-ahead count is not).
    pub fn library_first_target(requested_songs: i32) -> i32 {
        requested_songs.max(0)
    }

    /// Split `requested_songs` into a local target and an external target. Both are returned
    /// together so the two can never drift apart at a call site.
    ///
    /// `requested_songs` is the client's songCount; negative values are treated as zero (the
    /// previous expression sanitized those only by accident, through its flat floor).
    /// `discovery_enabled` is `SubsonicSettings.EnableSearchDiscovery`: false sends the whole
    /// request to the local target and skips discovery entirely, which is what lets the call
    /// site skip its Last.fm/Deezer fan-out too instead of just discarding it.
    ///
    /// `local_floor` is the effective `Subsonic:SearchLibraryOnlyUpTo` ([`Self::LOCAL_SONG_FLOOR`]
    /// by default): a request for that many songs or fewer gets no discovery.
    ///
    /// Returns (local, external); their sum never exceeds `requested_songs`.
    pub fn compute(requested_songs: i32, discovery_enabled: bool, local_floor: i32) -> (i32, i32) {
        let requested = requested_songs.max(0);

        if !discovery_enabled {
            return (requested, 0);
        }

        // Locals still scale with big requests through the quarter rule, which is what keeps
        // behavior identical for the large counts radio-style clients send. The floor only
        // decides small requests, and capping it at the request is the fix.
        let local = local_floor.max(0).min(requested).max(requested / 4);

        let external = Self::EXTERNAL_CEILING.min((requested - local).max(0));

        (local, external)
    }
}

#[cfg(test)]
mod tests {
    //! These pin issue #14: the old local floor was a flat 20, which is also the Subsonic spec
    //! default for songCount, so a client that sent the default (or sent nothing) had its whole
    //! budget consumed by local results and never received a single discovery row. Search looked
    //! like it only ever returned albums.

    use super::*;

    /// The split exactly as it behaved before the fix. Kept here so the "nothing changes for
    /// large requests" guarantee is checked by CI on every run.
    fn legacy(requested_songs: i32) -> (i32, i32) {
        let local = 20.max(requested_songs / 4);
        let external = 150.min((requested_songs - local).max(0));
        (local, external)
    }

    #[test]
    fn compute_splits_as_specified() {
        for (requested, local, external) in [
            // Below the floor: local-only, and we stop asking Navidrome for more than the
            // client wanted. No discovery means no Last.fm fan-out on a type-ahead keystroke.
            (0, 0, 0),
            (5, 5, 0),
            (12, 12, 0),
            // The reported bug. The spec default used to yield zero external rows.
            (20, 12, 8),
            (40, 12, 28),
            (60, 15, 45),
            (79, 19, 60),
            // From here up the quarter rule dominates, so the local side stops changing. The
            // external side is held at the ceiling, which is the number of rows a query
            // actually builds; past it the rows would be unenriched placeholders.
            (80, 20, 60),
            (200, 50, 60),
            (1000, 250, 60),
        ] {
            assert_eq!(
                SearchBudget::compute(requested, true, SearchBudget::LOCAL_SONG_FLOOR),
                (local, external),
                "songCount={requested}"
            );
        }
    }

    #[test]
    fn compute_leaves_the_local_target_unchanged_for_large_requests() {
        // At 80 and above, requested/4 is at least 20, so the old flat floor was never the
        // binding term and the new capped floor resolves to the same number.
        for n in 80..=5000 {
            assert_eq!(
                legacy(n).0,
                SearchBudget::compute(n, true, SearchBudget::LOCAL_SONG_FLOOR).0,
                "songCount={n}"
            );
        }
    }

    #[test]
    fn compute_only_ever_loses_externals_to_the_ceiling() {
        // Min(floor, n) can only be smaller than the old flat 20, so the local target never
        // grows and the external target never shrinks on account of the split itself. The one
        // place the count can fall is the ceiling.
        for n in 0..=5000 {
            let expected = SearchBudget::EXTERNAL_CEILING.min(legacy(n).1);
            assert!(
                SearchBudget::compute(n, true, SearchBudget::LOCAL_SONG_FLOOR).1 >= expected,
                "songCount={n} lost external rows beyond the ceiling"
            );
        }
    }

    #[test]
    fn compute_keeps_both_targets_inside_the_requested_count() {
        // The merge appends externals after locals with no total cap, so a client that renders
        // only the count it asked for would never see an external row if the two targets
        // summed past it.
        for n in 0..=5000 {
            let (local, external) = SearchBudget::compute(n, true, SearchBudget::LOCAL_SONG_FLOOR);
            assert!(local + external <= n, "songCount={n} produced {local}+{external}");
        }
    }

    #[test]
    fn compute_treats_negative_counts_as_zero() {
        assert_eq!(
            SearchBudget::compute(-1, true, SearchBudget::LOCAL_SONG_FLOOR),
            (0, 0)
        );
        assert_eq!(
            SearchBudget::compute(i32::MIN, true, SearchBudget::LOCAL_SONG_FLOOR),
            (0, 0)
        );
    }

    #[test]
    fn compute_does_not_overflow_on_absurd_counts() {
        let (local, external) = SearchBudget::compute(i32::MAX, true, SearchBudget::LOCAL_SONG_FLOOR);

        assert_eq!(local, i32::MAX / 4);
        assert_eq!(external, SearchBudget::EXTERNAL_CEILING);
    }

    #[test]
    fn compute_discovery_disabled_returns_all_slots_local() {
        // discoveryEnabled: false sends the whole request to local and drops external to zero
        // regardless of size, so the call site's Last.fm/Deezer fan-out never fires.
        for (requested, local) in [(0, 0), (5, 5), (20, 20), (1000, 1000)] {
            assert_eq!(
                SearchBudget::compute(requested, false, SearchBudget::LOCAL_SONG_FLOOR),
                (local, 0),
                "songCount={requested}"
            );
        }
    }

    #[test]
    fn library_first_target_is_the_whole_page() {
        for (requested, local) in [(0, 0), (5, 5), (20, 20), (1000, 1000), (-1, 0), (i32::MIN, 0)] {
            assert_eq!(
                SearchBudget::library_first_target(requested),
                local,
                "songCount={requested}"
            );
        }
    }

    #[test]
    fn compute_with_a_lower_floor_gives_small_requests_discovery() {
        // The Music Assistant app asks for 10 songs; with the default floor that is all library.
        assert_eq!(
            SearchBudget::compute(10, true, SearchBudget::LOCAL_SONG_FLOOR),
            (10, 0)
        );
        assert_eq!(SearchBudget::compute(10, true, 9), (9, 1));
        assert_eq!(SearchBudget::compute(10, true, 0), (2, 8));
        assert_eq!(SearchBudget::compute(5, true, 0), (1, 4));
        // A request at or below the floor stays library only, as before.
        assert_eq!(SearchBudget::compute(5, true, 5), (5, 0));
        // Large requests are decided by the quarter rule, whatever the floor.
        assert_eq!(SearchBudget::compute(200, true, 0), (50, 60));
        // A negative floor counts as 0.
        assert_eq!(SearchBudget::compute(10, true, -3), (2, 8));
    }

    #[test]
    fn compute_with_a_lower_floor_keeps_both_targets_inside_the_requested_count() {
        for floor in [0, 1, 5, 12, 100] {
            for n in 0..=500 {
                let (local, external) = SearchBudget::compute(n, true, floor);
                assert!(
                    local + external <= n,
                    "songCount={n} floor={floor} produced {local}+{external}"
                );
            }
        }
    }

    /// Callers always pass the discovery flag; the enabled split is the one pinned above.
    #[test]
    fn compute_discovery_defaults_to_enabled() {
        assert_eq!(
            SearchBudget::compute(20, true, SearchBudget::LOCAL_SONG_FLOOR),
            (12, 8)
        );
    }
}
