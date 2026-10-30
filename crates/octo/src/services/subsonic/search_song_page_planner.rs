use super::search_song_order::SearchSongOrder;

/// Where one later page of a search3 / search2 song list comes from.
///
/// A search with discovery answers page one with the library's best matches and then outside
/// songs. Before this, a client that asked for page two was sent straight to Navidrome at the
/// same offset, so every outside song past the first page was unreachable and the library rows
/// page one had held back were skipped as well. This lays the whole answer out as one list, in
/// the order page one started it, so any songOffset lands on the right rows:
///
///   1. the library rows page one showed (its local prefix)
///   2. the outside rows page one showed
///   3. empty places, when page one came back shorter than it was asked for
///   4. the outside rows page one had no room for
///   5. the rest of the library, from where the prefix stopped
///
/// Places 0 up to page one's song count are page one, whatever it managed to fill, so a client
/// stepping by its page size lands exactly after it. Past that there are no gaps, so a client
/// that stops at the first short page is not stopped early.
pub struct SearchSongPagePlanner;

/// One page, in the order it is rendered: library rows, outside rows page one also showed,
/// outside rows it had no room for, then library rows again. The library rows come from a
/// single Navidrome call at `local_offset` for `leading_locals + trailing_locals` rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchSongPage {
    pub local_offset: i32,
    pub leading_locals: i32,
    pub page_one_external_skip: i32,
    pub page_one_external_take: i32,
    pub later_external_skip: i32,
    pub later_external_take: i32,
    pub trailing_locals: i32,
}

impl SearchSongPagePlanner {
    /// Plan the page at `song_offset` of `song_count` rows, from what page one showed.
    pub fn plan_order(song_offset: i32, song_count: i32, order: &SearchSongOrder) -> SearchSongPage {
        Self::plan(
            song_offset,
            song_count,
            order.prefix_count,
            order.page_one_externals,
            order.page_one_count,
            order.later_externals.len() as i32,
            order.library_continues(),
        )
    }

    /// The same plan from the bare numbers, which is what the tests drive.
    ///
    /// `prefix_count`: library rows page one showed. `page_one_externals`: outside rows page
    /// one took from the build. `page_one_count`: the songCount page one was asked for.
    /// `later_externals`: outside rows left for the pages after it. `library_continues`:
    /// whether the library may have more rows past the prefix, only when page one got a full
    /// prefix (a short one means Navidrome had nothing more to give).
    pub fn plan(
        song_offset: i32,
        song_count: i32,
        prefix_count: i32,
        page_one_externals: i32,
        page_one_count: i32,
        later_externals: i32,
        library_continues: bool,
    ) -> SearchSongPage {
        // i64 throughout, so an absurd offset or count cannot wrap round into a real row.
        let start = i64::from(song_offset.max(0));
        let end = start + i64::from(song_count.max(0));
        let prefix = i64::from(prefix_count.max(0));
        let shown = i64::from(page_one_externals.max(0));
        let page_one = (prefix + shown).max(i64::from(page_one_count));
        let later = i64::from(later_externals.max(0));

        let (leading_from, leading_to) = overlap(start, end, 0, prefix);
        let (shown_from, shown_to) = overlap(start, end, prefix, prefix + shown);
        let (later_from, later_to) = overlap(start, end, page_one, page_one + later);
        let (tail_from, tail_to) = if library_continues {
            overlap(start, end, page_one + later, i64::MAX)
        } else {
            (0, 0)
        };

        // The library side of a page is always one run of Navidrome's own list: rows from the
        // prefix and rows from the rest can only share a page when the page reaches right
        // across the outside rows, and then the rest starts where the prefix stopped.
        let leading = leading_to - leading_from;
        let trailing = tail_to - tail_from;
        let local_offset = if leading > 0 {
            leading_from
        } else if trailing > 0 {
            prefix + (tail_from - page_one - later)
        } else {
            0
        };

        SearchSongPage {
            local_offset: clamp(local_offset),
            leading_locals: clamp(leading),
            page_one_external_skip: clamp(shown_from - prefix),
            page_one_external_take: clamp(shown_to - shown_from),
            later_external_skip: clamp(later_from - page_one),
            later_external_take: clamp(later_to - later_from),
            trailing_locals: clamp(trailing),
        }
    }
}

/// The part of the page that falls inside one stretch of the list.
fn overlap(start: i64, end: i64, from: i64, to: i64) -> (i64, i64) {
    let a = start.max(from);
    let b = end.min(to);
    if a < b { (a, b) } else { (0, 0) }
}

fn clamp(value: i64) -> i32 {
    value.clamp(0, i64::from(i32::MAX)) as i32
}

#[cfg(test)]
mod tests {
    //! Where each later page of a search's songs comes from. Page one shows the library's best
    //! matches and then outside songs; these pin that every page after it carries on from there,
    //! so a client scrolling the list sees each row once and none go missing.

    use super::*;
    use crate::services::subsonic::search_budget::SearchBudget;

    /// One search, described the way page one left it: a library of `library` matches, a build
    /// of `built` outside songs, and page one asked for `page_one_count` rows, with the
    /// local/outside split the search uses: the whole page for the library when `library_first`
    /// (every client but Octo's apps), else the budget's split.
    struct Search {
        page_one_count: i32,
        library: i32,
        built: i32,
        library_first: bool,
    }

    impl Search {
        fn budget(&self) -> (i32, i32) {
            if self.library_first {
                (SearchBudget::library_first_target(self.page_one_count), 0)
            } else {
                SearchBudget::compute(self.page_one_count, true, SearchBudget::LOCAL_SONG_FLOOR)
            }
        }
        fn prefix_count(&self) -> i32 {
            self.budget().0.min(self.library)
        }
        fn shown(&self) -> i32 {
            let (local, external) = self.budget();
            SearchSongOrder::page_one_external_count(self.built, local, external, self.prefix_count())
        }
        fn later(&self) -> i32 {
            self.built - self.shown()
        }
        fn library_continues(&self) -> bool {
            self.prefix_count() >= self.budget().0 && self.budget().0 > 0
        }
        fn library_rows(&self) -> Vec<String> {
            (0..self.library).map(|i| format!("l{i}")).collect()
        }
        fn built_rows(&self) -> Vec<String> {
            (0..self.built).map(|i| format!("e{i}")).collect()
        }

        /// Every row of the search once, in order, as if it had no pages.
        fn whole(&self) -> Vec<String> {
            let library = self.library_rows();
            let prefix = self.prefix_count() as usize;
            let mut rows: Vec<String> = library[..prefix].to_vec();
            rows.extend(self.built_rows());
            if self.library_continues() {
                rows.extend(library[prefix..].iter().cloned());
            }
            rows
        }

        fn plan(&self, offset: i32, count: i32) -> SearchSongPage {
            SearchSongPagePlanner::plan(
                offset,
                count,
                self.prefix_count(),
                self.shown(),
                self.page_one_count,
                self.later(),
                self.library_continues(),
            )
        }

        /// The rows a page renders, with Navidrome answering the library part.
        fn render(&self, page: SearchSongPage) -> Vec<String> {
            let take = |rows: Vec<String>, skip: i32, take: i32| -> Vec<String> {
                rows.into_iter().skip(skip as usize).take(take as usize).collect()
            };
            let locals = take(
                self.library_rows(),
                page.local_offset,
                page.leading_locals + page.trailing_locals,
            );
            let built = self.built_rows();
            let shown = self.shown() as usize;
            let mut rows = take(locals.clone(), 0, page.leading_locals);
            rows.extend(take(
                built[..shown].to_vec(),
                page.page_one_external_skip,
                page.page_one_external_take,
            ));
            rows.extend(take(
                built[shown..].to_vec(),
                page.later_external_skip,
                page.later_external_take,
            ));
            rows.extend(locals.into_iter().skip(page.leading_locals as usize));
            rows
        }

        /// Page one exactly as the search builds it today: the library's prefix, then the
        /// outside rows its budget allows.
        fn page_one(&self) -> Vec<String> {
            let mut rows: Vec<String> = self.library_rows()[..self.prefix_count() as usize].to_vec();
            rows.extend(self.built_rows()[..self.shown() as usize].iter().cloned());
            rows
        }

        /// The last place anything can be, so a walk knows it has passed the end.
        fn end(&self) -> i32 {
            self.page_one_count
                + self.later()
                + if self.library_continues() {
                    self.library - self.prefix_count()
                } else {
                    0
                }
        }
    }

    fn searches() -> Vec<Search> {
        // Page sizes around the budget's edges: the smallest request that earns discovery, the
        // spec default, the last one below the outside ceiling, and big radio-style ones.
        let mut all = Vec::new();
        for library_first in [false, true] {
            for page_one_count in [13, 20, 40, 79, 80, 81, 200] {
                for library in [0, 5, 12, 13, 30, 100, 400] {
                    for built in [0, 3, 8, 25, 60] {
                        all.push(Search {
                            page_one_count,
                            library,
                            built,
                            library_first,
                        });
                    }
                }
            }
        }
        all
    }

    fn name(s: &Search) -> String {
        format!(
            "pageOne={} library={} built={} libraryFirst={}",
            s.page_one_count, s.library, s.built, s.library_first
        )
    }

    #[test]
    fn offset_zero_is_exactly_page_one() {
        for search in searches() {
            assert_eq!(
                search.render(search.plan(0, search.page_one_count)),
                search.page_one(),
                "{}",
                name(&search)
            );
        }
    }

    #[test]
    fn stepping_by_the_page_size_shows_every_row_once() {
        for search in searches() {
            let mut seen = search.page_one();
            let mut offset = search.page_one_count;
            while offset <= search.end() {
                seen.extend(search.render(search.plan(offset, search.page_one_count)));
                offset += search.page_one_count;
            }
            assert_eq!(seen, search.whole(), "{}", name(&search));
        }
    }

    #[test]
    fn changing_the_page_size_midway_still_shows_every_row_once() {
        let sizes = [7, 1, 33, 20, 12, 50, 3, 100];
        for search in searches() {
            let mut seen = search.page_one();
            let mut offset = search.page_one_count;
            let mut step = 0;
            while offset <= search.end() {
                let size = sizes[step % sizes.len()];
                seen.extend(search.render(search.plan(offset, size)));
                offset += size;
                step += 1;
            }
            assert_eq!(seen, search.whole(), "{}", name(&search));
        }
    }

    #[test]
    fn worked_example() {
        // The worked example: a 20-row page, 30 library matches, 25 outside songs. Page one is
        // l0-l11 then e0-e7. The whole list is l0-l11, e0-e24, l12-l29.
        #[rustfmt::skip]
        let cases = [
            // offset count  localOffset leading shownSkip shownTake laterSkip laterTake trailing
            (20, 20, [12, 0, 0, 0, 0, 17, 3]),   // e8-e24, then l12-l14
            (40, 20, [15, 0, 0, 0, 0, 0, 20]),   // l15 onward; Navidrome has only 15 left to give
            (60, 20, [35, 0, 0, 0, 0, 0, 20]),   // past the end: Navidrome answers nothing
            (30, 5, [0, 0, 0, 0, 10, 5, 0]),     // a smaller page inside the outside rows
            (35, 5, [12, 0, 0, 0, 15, 2, 3]),    // across the join into the library
            (10, 20, [10, 2, 0, 8, 0, 10, 0]),   // an offset inside page one: its own rows again
            (20, 0, [0, 0, 0, 0, 0, 0, 0]),      // a page of nothing asks Navidrome for nothing
        ];
        for (offset, count, [lo, le, ss, st, ls, lt, tr]) in cases {
            let page = SearchSongPagePlanner::plan(offset, count, 12, 8, 20, 17, true);
            let expected = SearchSongPage {
                local_offset: lo,
                leading_locals: le,
                page_one_external_skip: ss,
                page_one_external_take: st,
                later_external_skip: ls,
                later_external_take: lt,
                trailing_locals: tr,
            };
            assert_eq!(page, expected, "offset={offset} count={count}");
        }
    }

    fn page(values: [i32; 7]) -> SearchSongPage {
        let [lo, le, ss, st, ls, lt, tr] = values;
        SearchSongPage {
            local_offset: lo,
            leading_locals: le,
            page_one_external_skip: ss,
            page_one_external_take: st,
            later_external_skip: ls,
            later_external_take: lt,
            trailing_locals: tr,
        }
    }

    #[test]
    fn a_short_page_one_leaves_its_empty_places_behind_so_the_next_page_starts_on_the_library() {
        // 20 asked for, 12 from the library and only 3 outside songs built: page one showed 15
        // rows. A client stepping by 20 must get l12 next, not l17.
        let planned = SearchSongPagePlanner::plan(20, 20, 12, 3, 20, 0, true);

        assert_eq!(planned, page([12, 0, 0, 0, 0, 0, 20]));
    }

    #[test]
    fn a_library_that_ran_out_on_page_one_is_not_asked_again() {
        let planned = SearchSongPagePlanner::plan(20, 20, 5, 15, 20, 45, false);

        assert_eq!(planned, page([0, 0, 0, 0, 0, 20, 0]));
        assert_eq!(
            SearchSongPagePlanner::plan(80, 20, 5, 15, 20, 45, false),
            page([0; 7])
        );
    }

    #[test]
    fn nonsense_numbers_neither_throw_nor_wrap_round() {
        for (offset, count) in [(-5, 20), (20, -1), (i32::MAX, i32::MAX)] {
            let page = SearchSongPagePlanner::plan(offset, count, 12, 8, 20, 17, true);

            assert!(page.local_offset >= 0 && page.leading_locals >= 0 && page.trailing_locals >= 0);
            assert!(page.page_one_external_skip >= 0 && page.later_external_skip >= 0);
        }
    }
}
