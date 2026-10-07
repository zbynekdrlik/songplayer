//! Why a dashboard program cut to a playlist is refused (#221, ROZHODNUTÉ
//! 6022247729): the reason codes the server records and serves, and the
//! Slovak texts the dashboard shows for them. ONE vocabulary for sp-server
//! (`playback::program_switch`, the keep reasons + `cut_refused`) and sp-ui
//! (the Program control's disabled buttons), so the two can never drift.
//!
//! Every consumer takes `SP-program`, so a cut to an inactive playlist, or
//! to one whose scene catalog names no scene, would black the LED wall, FOH,
//! the Presenter and the stream at once; the server refuses it (409).

/// The reason of a cut to an INACTIVE playlist: it has no output.
pub const PLAYLIST_INACTIVE: &str = "playlist_inactive";

/// The reason of a cut to an active playlist whose scene catalog names no
/// scene (no NDI output name, or one another active playlist shares).
pub const NO_SCENE: &str = "no_scene";

/// The tooltip of a playlist's cut button the server does not refuse.
pub const CUT_BUTTON_TITLE: &str = "Strih na program";

/// Why a cut is refused, in Slovak, for its reason code; any other code
/// (a newer server, or none known yet) gets the generic text.
pub fn refusal_text(reason: &str) -> &'static str {
    match reason {
        PLAYLIST_INACTIVE => {
            "Playlist je neaktívny — strih by zatemnil celý program (stenu, FOH, Presenter, stream)"
        }
        NO_SCENE => {
            "Playlist nemá vlastnú scénu (chýba mu NDI výstup, alebo ho zdieľa s iným aktívnym playlistom) — na program ho strihnúť nemožno"
        }
        _ => "Strih na tento playlist server odmieta",
    }
}

/// A playlist's cut button tooltip: what it does, or (refused) why it is
/// disabled.
pub fn cut_button_title(refusal: Option<&str>) -> &'static str {
    refusal.map_or(CUT_BUTTON_TITLE, refusal_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reason_codes_are_the_wire_values() {
        assert_eq!(
            [PLAYLIST_INACTIVE, NO_SCENE],
            ["playlist_inactive", "no_scene"]
        );
    }

    #[test]
    fn each_reason_has_its_own_slovak_text_and_any_other_the_generic_one() {
        assert_eq!(
            refusal_text("playlist_inactive"),
            "Playlist je neaktívny — strih by zatemnil celý program (stenu, FOH, Presenter, stream)"
        );
        assert_eq!(
            refusal_text("no_scene"),
            "Playlist nemá vlastnú scénu (chýba mu NDI výstup, alebo ho zdieľa s iným aktívnym playlistom) — na program ho strihnúť nemožno"
        );
        for other in ["", "persist_failed", "Playlist_Inactive"] {
            assert_eq!(
                refusal_text(other),
                "Strih na tento playlist server odmieta",
                "{other:?}"
            );
        }
    }

    #[test]
    fn a_button_says_what_it_does_or_why_it_is_disabled() {
        assert_eq!(cut_button_title(None), "Strih na program");
        assert_eq!(
            cut_button_title(Some(NO_SCENE)),
            refusal_text(NO_SCENE),
            "a refused button names its reason"
        );
        assert_eq!(
            cut_button_title(Some(PLAYLIST_INACTIVE)),
            refusal_text(PLAYLIST_INACTIVE)
        );
        assert_eq!(
            cut_button_title(Some("newer")),
            "Strih na tento playlist server odmieta"
        );
    }
}
