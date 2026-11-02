//! The "Tidy artists" plan: which folders split, which spellings change, which guests reach the
//! ARTISTS tag, and, as much as anything, what is left alone and why.

use octo_core::settings::FolderStructure;

use super::*;

const ROOT: &str = "/music";

fn at(relative: &str) -> String {
    format!("{ROOT}/{relative}")
}

/// A readable, in-scope file tagged with `artist` (and the album artist, when given).
fn song(relative: &str, artist: &str) -> TidyFile {
    TidyFile {
        path: at(relative),
        tags: TidyTags {
            artist: vec![artist.to_string()],
            album_artist: Vec::new(),
            artists: Vec::new(),
        },
        title: Some("Song".to_string()),
        in_scope: true,
        readable: true,
        ..TidyFile::default()
    }
}

trait Build {
    fn album_artist(self, value: &str) -> Self;
    fn artists(self, values: &[&str]) -> Self;
    fn ids(self, values: &[&str]) -> Self;
    fn recording(self, id: &str) -> Self;
    fn title(self, title: &str) -> Self;
    fn out_of_scope(self) -> Self;
}

impl Build for TidyFile {
    fn album_artist(mut self, value: &str) -> Self {
        self.tags.album_artist = vec![value.to_string()];
        self
    }
    fn artists(mut self, values: &[&str]) -> Self {
        self.tags.artists = values.iter().map(|v| v.to_string()).collect();
        self
    }
    fn ids(mut self, values: &[&str]) -> Self {
        self.artist_ids = values.iter().map(|v| v.to_string()).collect();
        self
    }
    fn recording(mut self, id: &str) -> Self {
        self.recording_id = Some(id.to_string());
        self
    }
    fn title(mut self, title: &str) -> Self {
        self.title = Some(title.to_string());
        self
    }
    fn out_of_scope(mut self) -> Self {
        self.in_scope = false;
        self
    }
}

fn plan_of(files: &[TidyFile]) -> TidyPlan {
    plan(ROOT, files, &TidyAnswers::default(), &TidyOptions::default())
}

fn plan_with(files: &[TidyFile], answers: &TidyAnswers) -> TidyPlan {
    plan(ROOT, files, answers, &TidyOptions::default())
}

fn knows(entries: &[(&str, &[&str])]) -> TidyAnswers {
    TidyAnswers {
        artists: entries
            .iter()
            .map(|(name, names)| {
                (
                    artist_lookup_key(name),
                    names.iter().map(|n| n.to_string()).collect(),
                )
            })
            .collect(),
        credits: HashMap::new(),
    }
}

fn skip_saying<'a>(plan: &'a TidyPlan, words: &str) -> Option<&'a TidyRow> {
    plan.rows
        .iter()
        .find(|row| row.kind == TidyKind::Skip && row.evidence.contains(words))
}

fn row_of(plan: &TidyPlan, kind: TidyKind) -> Option<&TidyRow> {
    plan.rows.iter().find(|row| row.kind == kind)
}

const SPLIT: &str = "Bizarrap, Rauw Alejandro";

/// Both parts as artists of their own elsewhere in the library.
fn both_parts_in_the_library() -> Vec<TidyFile> {
    vec![
        song("Bizarrap/Other/01 - X.flac", "Bizarrap"),
        song("Rauw Alejandro/Other/01 - Y.flac", "Rauw Alejandro"),
    ]
}

// ---- splitting ------------------------------------------------------------------------------

#[test]
fn known_whole_names_never_split_and_ask_nothing() {
    for name in [
        "Tyler, The Creator",
        "Earth, Wind & Fire",
        "Simon & Garfunkel",
        "Bob Marley & The Wailers",
    ] {
        let plan = plan_of(&[song(&format!("{name}/Album/01 - Song.flac"), name).album_artist(name)]);
        assert!(plan.moves.is_empty(), "{name}");
        assert!(plan.wanted_artists.is_empty(), "{name}");
        assert!(row_of(&plan, TidyKind::Split).is_none(), "{name}");
    }
}

#[test]
fn tags_naming_the_whole_credit_keep_it_whole() {
    let plan =
        plan_of(
            &[song("Brothers & Sisters/A/01 - Song.flac", "Brothers & Sisters")
                .artists(&["Brothers & Sisters"])],
        );
    assert!(plan.moves.is_empty());
    assert!(skip_saying(&plan, "as one artist").is_some());
}

#[test]
fn artists_tag_naming_the_first_credit_splits_where_a_new_download_would_go() {
    let plan = plan_of(&[song(&format!("{SPLIT}/An Album/02 - Session.flac"), SPLIT)
        .album_artist(SPLIT)
        .artists(&["Bizarrap", "Rauw Alejandro"])
        .title("Session")]);

    let movement = plan.moves.first().expect("a move");
    assert_eq!(plan.moves.len(), 1);
    let fresh = PathHelper::build_layout_path(
        FolderStructure::Organized,
        ROOT,
        "Bizarrap",
        "An Album",
        "Session",
        Some(2),
        ".flac",
    );
    assert_eq!(format!("{}/02 - Session.flac", movement.to), fresh);
    assert_eq!(movement.from, at(&format!("{SPLIT}/An Album")));
    let row = row_of(&plan, TidyKind::Split).expect("a split row");
    assert_eq!(row.after, "Bizarrap");
    assert!(
        row.evidence.contains("name \"Bizarrap\" first"),
        "{}",
        row.evidence
    );
    assert!(plan.wanted_artists.is_empty());
}

#[test]
fn artists_tag_naming_a_later_credit_does_not_split() {
    let plan =
        plan_of(&[song(&format!("{SPLIT}/A/01 - S.flac"), SPLIT).artists(&["Rauw Alejandro", "Bizarrap"])]);
    assert!(plan.moves.is_empty());
    assert!(
        plan.rows
            .iter()
            .any(|row| row.kind == TidyKind::Skip && row.path == SPLIT)
    );
}

#[test]
fn files_that_disagree_do_not_split() {
    let plan = plan_of(&[
        song("A, B/X/01 - S.flac", "A, B").artists(&["A", "B"]),
        song("A, B/X/02 - T.flac", "A, B").artists(&["A, B"]),
    ]);
    assert!(plan.moves.is_empty());
    assert!(skip_saying(&plan, "disagree").is_some());
}

#[test]
fn a_recording_credit_naming_the_first_artist_splits() {
    let id = "11111111-2222-3333-4444-555555555555";
    let files = [song(&format!("{SPLIT}/A/01 - S.flac"), SPLIT).recording(id)];

    let unasked = plan_of(&files);
    assert!(unasked.moves.is_empty());
    assert_eq!(unasked.wanted_recordings, [id]);
    assert!(skip_saying(&unasked, "not asked").is_some());

    let answers = TidyAnswers {
        credits: [(
            id.to_string(),
            vec![
                CreditPart {
                    name: "Bizarrap".into(),
                    join_phrase: ", ".into(),
                },
                CreditPart {
                    name: "Rauw Alejandro".into(),
                    join_phrase: String::new(),
                },
            ],
        )]
        .into(),
        ..TidyAnswers::default()
    };
    let asked = plan_with(&files, &answers);
    assert_eq!(asked.moves.len(), 1);
    assert_eq!(asked.moves[0].to, at("Bizarrap/A"));
    assert_eq!(asked.retags[0].after.artists, ["Bizarrap", "Rauw Alejandro"]);
}

/// Without credits in the songs, a part that is not an artist in this library is no evidence,
/// and MusicBrainz is not even asked.
#[test]
fn a_split_is_refused_when_one_part_is_not_a_library_artist() {
    let mut files = vec![song(&format!("{SPLIT}/A/01 - S.flac"), SPLIT)];
    files.push(song("Bizarrap/Other/01 - X.flac", "Bizarrap"));

    let plan = plan_of(&files);

    assert!(plan.moves.is_empty());
    assert!(plan.wanted_artists.is_empty());
    assert!(skip_saying(&plan, "\"Rauw Alejandro\" is not an artist in this library").is_some());
}

/// MusicBrainz alone never splits: knowing every part and not the whole is not enough when the
/// parts are not artists here.
#[test]
fn musicbrainz_alone_never_splits() {
    let answers = knows(&[
        (SPLIT, &[]),
        ("Bizarrap", &["Bizarrap"]),
        ("Rauw Alejandro", &["Rauw Alejandro"]),
    ]);
    let plan = plan_with(&[song(&format!("{SPLIT}/A/01 - S.flac"), SPLIT)], &answers);
    assert!(plan.moves.is_empty());
    assert!(plan.retags.is_empty());
}

#[test]
fn library_parts_ask_musicbrainz_about_the_whole_and_skip_until_answered() {
    let mut files = both_parts_in_the_library();
    files.push(song(&format!("{SPLIT}/A/01 - S.flac"), SPLIT));

    let plan = plan_of(&files);

    assert!(plan.moves.is_empty());
    assert_eq!(plan.wanted_artists, [SPLIT]);
    assert!(skip_saying(&plan, "not asked").is_some());
}

#[test]
fn library_parts_and_musicbrainz_not_knowing_the_whole_split_and_fill_artists() {
    let mut files = both_parts_in_the_library();
    files.push(song(&format!("{SPLIT}/A/01 - S.flac"), SPLIT));

    let plan = plan_with(&files, &knows(&[(SPLIT, &[])]));

    assert_eq!(plan.moves.len(), 1);
    assert_eq!(plan.moves[0].to, at("Bizarrap/A"));
    let retag = plan
        .retags
        .iter()
        .find(|retag| retag.path == at(&format!("{SPLIT}/A/01 - S.flac")))
        .expect("the split song is re-tagged");
    assert_eq!(retag.after.artists, ["Bizarrap", "Rauw Alejandro"]);
    // The artist tag keeps the full credit, as a new download's does.
    assert_eq!(retag.after.artist, [SPLIT]);
    let row = row_of(&plan, TidyKind::Split).expect("a split row");
    assert!(
        row.evidence.contains("already artists in this library"),
        "{}",
        row.evidence
    );
}

#[test]
fn musicbrainz_knowing_the_whole_name_keeps_it_whole() {
    let mut files = both_parts_in_the_library();
    files.push(song(&format!("{SPLIT}/A/01 - S.flac"), SPLIT));

    let plan = plan_with(&files, &knows(&[(SPLIT, &[SPLIT])]));

    assert!(plan.moves.is_empty());
    assert!(skip_saying(&plan, "MusicBrainz knows").is_some());
}

#[test]
fn a_folder_no_file_is_tagged_for_is_left_alone() {
    let plan = plan_of(&[song("Rock, Pop/A/01 - S.flac", "Somebody")]);
    assert!(plan.moves.is_empty());
    assert!(skip_saying(&plan, "folder's name").is_some());
}

// ---- spelling -------------------------------------------------------------------------------

#[test]
fn a_spelling_navidrome_already_treats_as_one_artist_is_retagged_freely() {
    let plan = plan_of(&[
        song("Guns N' Roses/A/01 - S.flac", "Guns N' Roses"),
        song("Guns N' Roses/A/02 - T.flac", "Guns N' Roses"),
        song("Guns N\u{2019} Roses/B/01 - U.flac", "Guns N\u{2019} Roses"),
    ]);

    let row = row_of(&plan, TidyKind::Respell).expect("a respell row");
    assert!(!row.opt_in);
    assert_eq!(plan.retags.len(), 1);
    assert_eq!(plan.retags[0].after.artist, ["Guns N' Roses"]);
    assert_eq!(plan.moves.len(), 1);
    assert_eq!(plan.moves[0].to, at("Guns N' Roses/B"));
}

#[test]
fn star_loss_rows_are_unticked_by_default() {
    let files = [
        song("Beyonc\u{e9}/A/01 - S.flac", "Beyonc\u{e9}").album_artist("Beyonc\u{e9}"),
        song("Beyonc\u{e9}/A/02 - T.flac", "Beyonc\u{e9}").album_artist("Beyonc\u{e9}"),
        song("Beyonce/B/01 - U.flac", "Beyonce").album_artist("Beyonce"),
    ];

    let unticked = plan_of(&files);
    let row = row_of(&unticked, TidyKind::Rename).expect("a rename row");
    assert!(row.opt_in && !row.ticked);
    assert!(
        row.evidence.contains("stars and ratings on \"Beyonce\""),
        "{}",
        row.evidence
    );
    assert!(row.evidence.contains("lost for every user"), "{}", row.evidence);
    assert_eq!(row.files, 1);
    assert!(unticked.retags.is_empty());
    assert!(unticked.moves.is_empty());

    let ticked = plan(
        ROOT,
        &files,
        &TidyAnswers::default(),
        &TidyOptions {
            ticked: [row.id.clone()].into(),
            ..TidyOptions::default()
        },
    );
    assert_eq!(ticked.retags.len(), 1);
    assert_eq!(ticked.retags[0].after.artist, ["Beyonc\u{e9}"]);
    assert_eq!(ticked.retags[0].after.album_artist, ["Beyonc\u{e9}"]);
    assert_eq!(ticked.moves[0].to, at("Beyonc\u{e9}/B"));
    assert_eq!(ticked.moves[0].row, row.id);
}

#[test]
fn case_only_spellings_need_no_retag() {
    let plan = plan_of(&[
        song("M\u{d8}/A/01 - S.flac", "M\u{d8}"),
        song("M\u{d8}/A/02 - T.flac", "M\u{d8}"),
        song("M\u{f8}/B/01 - U.flac", "M\u{f8}"),
    ]);
    assert!(plan.retags.is_empty());
    assert!(plan.moves.is_empty());
    assert!(skip_saying(&plan, "Only the capitals differ").is_some());
}

#[test]
fn files_tagged_from_musicbrainz_spell_it_and_are_never_retagged() {
    let plan = plan(
        ROOT,
        &[
            song("Beyonc\u{e9}/A/01 - S.flac", "Beyonc\u{e9}").ids(&["a4"]),
            song("Beyonce/B/01 - T.flac", "Beyonce"),
            song("Beyonce/B/02 - U.flac", "Beyonce"),
            song("Beyonce/C/01 - V.flac", "Beyonc\u{e9}")
                .album_artist("Beyonce")
                .ids(&["a4"]),
        ],
        &TidyAnswers::default(),
        &TidyOptions {
            tick_all: true,
            ..TidyOptions::default()
        },
    );
    let row = row_of(&plan, TidyKind::Rename).expect("a rename row");
    assert_eq!(row.after, "Beyonc\u{e9}");
    assert_eq!(plan.retags.len(), 2);
    assert!(plan.retags.iter().all(|retag| !retag.path.ends_with("V.flac")));
}

#[test]
fn spellings_whose_files_carry_different_musicbrainz_artists_are_never_merged() {
    let plan = plan(
        ROOT,
        &[
            song("Nirvana/A/01 - S.flac", "Nirvana").ids(&["us"]),
            song("NIRVANA/B/01 - T.flac", "NIRVANA").ids(&["uk"]),
            song("Nirvana/A/02 - U.flac", "Nirvana"),
        ],
        &TidyAnswers::default(),
        &TidyOptions {
            tick_all: true,
            ..TidyOptions::default()
        },
    );
    assert!(plan.retags.is_empty());
    assert!(plan.moves.is_empty());
    assert!(skip_saying(&plan, "different artists").is_some());
}

#[test]
fn an_unsettled_tie_asks_musicbrainz_then_follows_it() {
    let files = [
        song("Beyonc\u{e9}/A/01 - S.flac", "Beyonc\u{e9}"),
        song("Beyonce/B/01 - T.flac", "Beyonce"),
    ];

    let unasked = plan_of(&files);
    assert!(unasked.retags.is_empty());
    assert_eq!(unasked.wanted_artists.len(), 2);
    assert!(skip_saying(&unasked, "tie").is_some());

    let answers = knows(&[
        ("Beyonce", &["Beyonc\u{e9}"]),
        ("Beyonc\u{e9}", &["Beyonc\u{e9}"]),
    ]);
    let asked = plan(
        ROOT,
        &files,
        &answers,
        &TidyOptions {
            tick_all: true,
            ..TidyOptions::default()
        },
    );
    assert_eq!(asked.retags.len(), 1);
    assert_eq!(asked.retags[0].after.artist, ["Beyonc\u{e9}"]);
}

#[test]
fn a_tie_musicbrainz_cannot_settle_is_skipped() {
    let plan = plan_with(
        &[
            song("Foo/A/01 - S.flac", "Foo"),
            song("Fo\u{f3}/B/01 - T.flac", "Fo\u{f3}"),
        ],
        &knows(&[("Foo", &[]), ("Fo\u{f3}", &[])]),
    );
    assert!(plan.retags.is_empty());
    assert!(skip_saying(&plan, "does not settle").is_some());
}

#[test]
fn spellings_that_only_match_without_the_are_reported_not_merged() {
    let plan = plan_of(&[
        song("The Weeknd/A/01 - S.flac", "The Weeknd"),
        song("The Weeknd/A/02 - T.flac", "The Weeknd"),
        song("Weeknd/B/01 - U.flac", "Weeknd"),
    ]);
    assert!(plan.retags.is_empty());
    assert!(plan.moves.is_empty());
    assert!(skip_saying(&plan, "\"The\"").is_some());
}

#[test]
fn out_of_scope_files_vote_but_are_never_changed() {
    let plan = plan(
        ROOT,
        &[
            song("Guns N' Roses/A/01 - S.flac", "Guns N' Roses").out_of_scope(),
            song("Guns N' Roses/A/02 - T.flac", "Guns N' Roses").out_of_scope(),
            song("Guns N\u{2019} Roses/B/01 - U.flac", "Guns N\u{2019} Roses"),
        ],
        &TidyAnswers::default(),
        &TidyOptions::default(),
    );
    assert_eq!(plan.retags.len(), 1);
    assert_eq!(plan.retags[0].path, at("Guns N\u{2019} Roses/B/01 - U.flac"));
    assert!(
        plan.moves
            .iter()
            .all(|m| m.from.starts_with(&at("Guns N\u{2019} Roses")))
    );
}

#[test]
fn a_folder_named_from_the_request_follows_the_tags() {
    let plan = plan_of(&[song("Beyonce/A/01 - S.flac", "Beyonc\u{e9}")]);
    assert!(plan.retags.is_empty());
    assert_eq!(plan.moves.len(), 1);
    assert_eq!(plan.moves[0].to, at("Beyonc\u{e9}/A"));
}

#[test]
fn a_folder_never_follows_its_tags_to_a_hidden_name() {
    let plan = plan_of(&[song("38 Special/A/01 - S.flac", ".38 Special")]);
    assert!(plan.moves.is_empty(), "{:?}", plan.moves);
    let row = skip_saying(&plan, "start with a dot").expect("a skip row");
    assert_eq!(row.path, "38 Special");
}

#[test]
fn a_folder_never_splits_to_a_hidden_name() {
    let credit = ".38 Special, Bizarrap";
    let plan = plan_of(&[song("38 Special, Bizarrap/A/01 - S.flac", credit)
        .album_artist(credit)
        .artists(&[".38 Special", "Bizarrap"])]);
    assert!(plan.moves.is_empty(), "{:?}", plan.moves);
    assert!(row_of(&plan, TidyKind::Split).is_none());
    let row = skip_saying(&plan, "start with a dot").expect("a skip row");
    assert_eq!(row.path, "38 Special, Bizarrap");
}

#[test]
fn a_capitals_only_folder_rename_on_a_case_insensitive_filesystem_is_skipped() {
    let files = [song("beyonc\u{e9}/A/01 - S.flac", "Beyonc\u{e9}")];
    let insensitive = TidyOptions {
        case_insensitive: true,
        ..TidyOptions::default()
    };
    let skipped = plan(ROOT, &files, &TidyAnswers::default(), &insensitive);
    assert!(skipped.moves.is_empty());
    assert!(skip_saying(&skipped, "capitals").is_some());
    assert_eq!(plan_of(&files).moves.len(), 1);
}

// ---- guests ---------------------------------------------------------------------------------

#[test]
fn guests_reach_artists_primary_first_title_and_artist_untouched() {
    for (artist, title) in [
        ("Drake", "Work (feat. Rihanna)"),
        ("Drake", "Work feat. Rihanna"),
        ("Drake feat. Rihanna", "Work"),
        ("Drake (feat. Rihanna)", "Work"),
    ] {
        let plan = plan_of(&[song("Drake/A/01 - Work.flac", artist).title(title)]);
        assert_eq!(plan.retags.len(), 1, "{artist} / {title}");
        assert_eq!(
            plan.retags[0].after.artists,
            ["Drake", "Rihanna"],
            "{artist} / {title}"
        );
        assert_eq!(plan.retags[0].after.artist, [artist]);
        assert!(row_of(&plan, TidyKind::Guest).is_some());
    }
}

#[test]
fn guests_are_appended_to_an_existing_artists_tag_in_its_order() {
    let plan = plan_of(&[song("A/X/01 - S.flac", "A & B")
        .artists(&["A", "B"])
        .title("S (feat. C)")]);
    assert_eq!(plan.retags[0].after.artists, ["A", "B", "C"]);
}

#[test]
fn every_artist_value_seeds_the_guest_list() {
    let mut file = song("A/X/01 - S.flac", "A").title("S (feat. C)");
    file.tags.artist.push("B".to_string());
    let plan = plan_of(&[file]);
    assert_eq!(plan.retags[0].after.artists, ["A", "B", "C"]);
}

#[test]
fn guests_already_listed_change_nothing() {
    let plan = plan_of(&[song("Drake/A/01 - Work.flac", "Drake")
        .artists(&["Drake", "Rihanna"])
        .title("Work (feat. Rihanna)")]);
    assert!(plan.retags.is_empty());
}

#[test]
fn versions_and_subtitles_are_not_guests() {
    for title in [
        "Song (Live)",
        "Song (Remix)",
        "Song - Remastered 2011",
        "Song (Radio Edit)",
        "Song, Pt. 2",
    ] {
        let plan = plan_of(&[song("Artist/A/01 - S.flac", "Artist").title(title)]);
        assert!(plan.retags.is_empty(), "{title}");
        assert!(plan.rows.is_empty(), "{title}");
    }
}

#[test]
fn a_guest_octo_cannot_read_is_a_skip() {
    let plan = plan_of(&[song("Artist/A/01 - S.flac", "Artist").title("Song feat.Somebody")]);
    assert!(plan.retags.is_empty());
    assert!(skip_saying(&plan, "not in a form Octo reads").is_some());
}

#[test]
fn an_id3v23_artist_frame_holding_a_slash_is_left_alone() {
    let mut file = song("AC/A/01 - S.flac", "AC").title("S (feat. X)");
    file.tags.artist.push("DC".to_string());
    file.slash_artist = true;
    let plan = plan_of(&[file]);
    assert!(plan.retags.is_empty());
    assert!(skip_saying(&plan, "ID3v2.3").is_some());
}

#[test]
fn a_semicolon_credit_already_naming_the_guest_changes_nothing() {
    for artist in ["SAYGRACE; G\u{2010}Eazy", "SAYGRACE; G-Eazy"] {
        let plan = plan_of(&[song("SAYGRACE/You Don't Own Me/You Don't Own Me.flac", artist)
            .album_artist("SAYGRACE")
            .title("You Don't Own Me (feat. G-Eazy)")]);
        assert!(plan.retags.is_empty(), "{artist}");
        assert!(row_of(&plan, TidyKind::Guest).is_none(), "{artist}");
    }
}

#[test]
fn a_semicolon_credit_gets_a_new_guest_after_its_own_artists() {
    let plan = plan_of(&[song("A/X/01 - S.flac", "A; B").title("S (feat. C)")]);
    assert_eq!(plan.retags[0].after.artists, ["A", "B", "C"]);
    assert_eq!(plan.retags[0].after.artist, ["A; B"]);
    assert_eq!(
        row_of(&plan, TidyKind::Guest).unwrap().after,
        "A \u{b7} B \u{b7} C"
    );
}

#[test]
fn a_slash_credit_is_split_like_navidrome() {
    let plan = plan_of(&[song("A/X/01 - S.flac", "A / B").title("S (feat. C)")]);
    assert_eq!(plan.retags[0].after.artists, ["A", "B", "C"]);
}

#[test]
fn a_credit_navidrome_reads_as_several_artists_is_never_a_spelling() {
    let files = [
        song("SAYGRACE/A/01 - S.flac", "SAYGRACE; G\u{2010}Eazy"),
        song("SAYGRACE/B/01 - T.flac", "SAYGRACE, G-Eazy"),
        song("SAYGRACE/C/01 - U.flac", "SAYGRACE, G-Eazy"),
    ];
    let plan = plan_of(&files);
    assert!(row_of(&plan, TidyKind::Rename).is_none());
    assert!(row_of(&plan, TidyKind::Respell).is_none());
    assert!(plan.retags.is_empty());
    assert!(skip_saying(&plan, "several artists").is_some());
}

// ---- boundaries -----------------------------------------------------------------------------

#[test]
fn dot_folders_are_never_touched() {
    let plan = plan_of(&[song(".octo-trash/Beyonce/A/01 - S.flac", "Beyonc\u{e9}").title("S (feat. X)")]);
    assert!(plan.retags.is_empty());
    assert!(plan.moves.is_empty());
}

#[test]
fn a_file_at_the_root_has_no_folder_to_move() {
    let plan = plan_of(&[song("Beyonce - S.flac", "Beyonc\u{e9}")]);
    assert!(plan.moves.is_empty());
}

#[test]
fn songs_loose_in_an_artist_folder_stay() {
    let plan = plan_of(&[
        song("Beyonce/01 - S.flac", "Beyonc\u{e9}"),
        song("Beyonce/A/01 - T.flac", "Beyonc\u{e9}"),
    ]);
    assert_eq!(plan.moves.len(), 1);
    assert_eq!(plan.moves[0].from, at("Beyonce/A"));
    assert!(skip_saying(&plan, "directly in the artist folder").is_some());
}

#[test]
fn an_album_folder_moves_only_when_every_song_is_in_the_run() {
    let plan = plan_of(&[
        song("Beyonce/A/01 - S.flac", "Beyonc\u{e9}"),
        song("Beyonce/A/02 - T.flac", "Beyonc\u{e9}").out_of_scope(),
        song("Beyonce/B/01 - U.flac", "Beyonc\u{e9}"),
    ]);
    assert_eq!(plan.moves.len(), 1);
    assert_eq!(plan.moves[0].from, at("Beyonce/B"));
    assert!(skip_saying(&plan, "a folder only moves whole").is_some());
}

#[test]
fn a_taken_album_folder_keeps_both() {
    let plan = plan_of(&[
        song("Beyonce/A/01 - S.flac", "Beyonc\u{e9}"),
        song("Beyonc\u{e9}/A/02 - T.flac", "Beyonc\u{e9}"),
    ]);
    assert!(plan.moves.is_empty());
    assert!(skip_saying(&plan, "already an album folder").is_some());
}

#[test]
fn an_excluded_folder_is_left_alone() {
    let files = [song("Beyonce/A/01 - S.flac", "Beyonc\u{e9}").title("S (feat. X)")];
    let options = TidyOptions {
        excluded: vec!["Beyonce".to_string()],
        ..TidyOptions::default()
    };
    let plan = plan(ROOT, &files, &TidyAnswers::default(), &options);
    assert!(plan.moves.is_empty());
    assert!(plan.retags.is_empty());
}

#[test]
fn a_folder_spelled_in_decomposed_letters_is_not_renamed_to_itself() {
    let plan = plan_of(&[song("Beyonce\u{301}/A/01 - S.flac", "Beyonc\u{e9}")]);
    assert!(plan.moves.is_empty());
}

#[test]
fn the_hash_follows_what_would_be_written_and_moved() {
    let files = [song("Beyonce/A/01 - S.flac", "Beyonc\u{e9}")];
    assert_eq!(plan_of(&files).hash(), plan_of(&files).hash());
    let other = [song("Beyonce/A/01 - S.flac", "Beyonc\u{e9}").title("S (feat. X)")];
    assert_ne!(plan_of(&files).hash(), plan_of(&other).hash());
}

// ---- the file layout --------------------------------------------------------------------------

#[test]
fn custom_layout_not_by_artist_leaves_folders() {
    let files = [song("Beyonce/A/01 - S.flac", "Beyonc\u{e9}")];
    assert_eq!(plan_of(&files).moves.len(), 1, "an artist-first layout moves it");
    let options = TidyOptions {
        leave_folders: true,
        ..TidyOptions::default()
    };
    let plan = plan(ROOT, &files, &TidyAnswers::default(), &options);
    assert!(plan.moves.is_empty());
    assert!(plan.rows.iter().all(|row| row.kind != TidyKind::Split));
}

#[test]
fn a_folder_differing_only_by_cleaning_stays() {
    // The folder is what the tags say; only the file layout's cleaning would change it.
    let plan = plan_of(&[song("Beyonce?/A/01 - S.flac", "Beyonce?")]);
    assert!(plan.moves.is_empty(), "{:?}", plan.moves);
}

#[test]
fn three_dot_artist_is_tidied() {
    let plan = plan_of(&[
        song("...And You Will Know Us/A/01 - S.flac", "...And You Will Know Us").title("S (feat. X)"),
    ]);
    assert!(row_of(&plan, TidyKind::Guest).is_some());
}

#[test]
fn row_ids_stay_the_same_across_previews() {
    let files = [
        song("Beyonc\u{e9}/A/01 - S.flac", "Beyonc\u{e9}"),
        song("Beyonc\u{e9}/A/02 - T.flac", "Beyonc\u{e9}"),
        song("Beyonce/B/01 - U.flac", "Beyonce"),
    ];
    let ids = |plan: TidyPlan| plan.rows.into_iter().map(|row| row.id).collect::<Vec<_>>();
    assert_eq!(ids(plan_of(&files)), ids(plan_of(&files)));
}
