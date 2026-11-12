//! #265: the block page's pure rules.

use super::{
    BlockedSong, CatalogRow, CatalogSong, ProgramCandidate, ProgramView, SEARCH_LIMIT, block_label,
    catalog, fold, is_blocked, program_view, search, shown_title, wall_title,
};

fn song(id: &str, title: &str) -> CatalogSong {
    CatalogSong {
        youtube_id: id.to_string(),
        title: title.to_string(),
    }
}

#[test]
fn the_wall_title_is_song_dash_artist_or_the_one_that_is_known() {
    assert_eq!(wall_title("My Song", "Artist"), "My Song - Artist");
    assert_eq!(wall_title("My Song", ""), "My Song");
    assert_eq!(wall_title("", "Artist"), "Artist");
    assert_eq!(wall_title("", ""), "");
}

#[test]
fn the_shown_title_falls_back_to_the_youtube_title() {
    assert_eq!(
        shown_title(Some(" Never Lost "), Some("Elevation"), "x"),
        "Never Lost - Elevation"
    );
    assert_eq!(shown_title(None, Some("Elevation"), "x"), "Elevation");
    assert_eq!(shown_title(None, None, " Raw YT title "), "Raw YT title");
    assert_eq!(shown_title(Some(""), Some(" "), "Raw"), "Raw");
}

#[test]
fn the_button_names_the_node_or_says_this_node() {
    assert_eq!(block_label(Some("PP")), "Zablokovať v PP");
    assert_eq!(block_label(Some(" snv ")), "Zablokovať v snv");
    assert_eq!(block_label(Some("  ")), "Zablokovať v tomto uzle");
    assert_eq!(block_label(None), "Zablokovať v tomto uzle");
}

fn row(row_id: i64, id: &str, title: &str, has_metadata: bool) -> CatalogRow {
    CatalogRow {
        row_id,
        youtube_id: id.to_string(),
        title: title.to_string(),
        has_metadata,
    }
}

/// Each song once, by title (case and diacritics folded); its title the
/// server's pick: a row with metadata first, then the oldest; an empty
/// title never.
#[test]
fn the_catalog_holds_each_song_once_by_title() {
    let rows = vec![
        row(1, "b", "", true),
        row(9, "b", "Bravo - X", true),
        row(2, "b", "yt bravo", false),
        row(12, "b", "Bravo later", true),
        row(4, "a", "alfa - Y", false),
        row(3, "a", "alfa older", false),
        row(5, "c", "Čaj - Z", true),
    ];
    let got = catalog(rows);
    assert_eq!(
        got,
        vec![
            song("a", "alfa older"),
            song("b", "Bravo - X"),
            song("c", "Čaj - Z")
        ]
    );
}

#[test]
fn search_needs_every_word_ignores_case_and_diacritics() {
    let songs = vec![
        song("1", "Svätý Boh - Elevation"),
        song("2", "Never Lost - Elevation"),
        song("3", "Holy Forever - Chris Tomlin"),
    ];
    let ids = |q: &str| -> Vec<String> {
        search(&songs, q)
            .into_iter()
            .map(|s| s.youtube_id.clone())
            .collect()
    };
    assert_eq!(ids("svaty"), ["1"]);
    assert_eq!(ids("ELEVATION"), ["1", "2"]);
    assert_eq!(ids("never elevation"), ["2"]);
    assert_eq!(ids("never tomlin"), Vec::<String>::new());
    assert_eq!(ids(""), Vec::<String>::new());
    assert_eq!(ids("   "), Vec::<String>::new());
}

#[test]
fn search_shows_at_most_the_limit() {
    let songs: Vec<CatalogSong> = (0..25).map(|i| song(&i.to_string(), "Song")).collect();
    assert_eq!(search(&songs, "song").len(), SEARCH_LIMIT);
    assert_eq!(SEARCH_LIMIT, 20);
}

#[test]
fn fold_takes_off_every_listed_diacritic() {
    assert_eq!(
        fold("ÁäàâãČçĎÉěèêëÍìîïĹľŇñÓôöòõŔřŠŤÚůüùûÝŽ-xQ"),
        "aaaaaccdeeeeeiiiillnnooooorrstuuuuuyz-xq"
    );
}

#[test]
fn the_song_on_program_is_the_on_program_playlist_s_video() {
    let c = |playlist_id, on_program, video_id| ProgramCandidate {
        playlist_id,
        on_program,
        video_id,
    };
    let song = |playlist_id, video_id| ProgramView::Song {
        playlist_id,
        video_id,
    };
    // Nothing on program claims "nothing" only once every state is known.
    assert_eq!(program_view(false, &[]), ProgramView::Loading);
    assert_eq!(program_view(true, &[]), ProgramView::Nothing);
    assert_eq!(
        program_view(true, &[c(1, false, Some(10))]),
        ProgramView::Nothing
    );
    assert_eq!(
        program_view(false, &[c(1, false, Some(10))]),
        ProgramView::Loading
    );
    assert_eq!(
        program_view(true, &[c(1, true, None)]),
        ProgramView::Nothing
    );
    // A song told to be on program shows before the rest is known.
    assert_eq!(
        program_view(false, &[c(1, false, Some(10)), c(2, true, Some(20))]),
        song(2, 20)
    );
    assert_eq!(
        program_view(
            true,
            &[c(5, true, Some(50)), c(3, true, Some(30)), c(4, true, None)]
        ),
        song(3, 30)
    );
}

#[test]
fn a_song_is_blocked_only_by_its_own_id() {
    let blocked = vec![BlockedSong {
        youtube_id: "TwzfEwsTfag".to_string(),
        title: "Never Lost".to_string(),
        blocked_at_ms: 1,
    }];
    assert!(is_blocked(&blocked, "TwzfEwsTfag"));
    assert!(!is_blocked(&blocked, "aaaaaaaaaaa"));
    assert!(!is_blocked(&[], "TwzfEwsTfag"));
}
