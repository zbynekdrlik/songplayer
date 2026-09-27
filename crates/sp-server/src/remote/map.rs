//! Scene → program action for a remote `SetCurrentProgramScene` (#213) — pure.
//!
//! The scene's playlists come from the SAME scene → playlist map SongPlayer's
//! own scene detection uses (`obs::scene::check_scene_items` over the
//! `NdiSourceMap`, run by the OBS client for the facade):
//!
//! - cg OBS did not switch (unknown scene, cg OBS not reachable) → the program
//!   stays as it is;
//! - the scene shows exactly ONE SongPlayer playlist → cut `SP-program` to it;
//! - any other scene (a manual cg OBS scene — media, browser, Slido, photos —
//!   or one showing several playlists) → cut to the #212 NDI input "OBS
//!   manuál", which carries the cg OBS mix that scene now shows, while that
//!   input is a program source (enabled with a source);
//! - otherwise the program stays as it is (logged as a WARN by the caller).

use std::collections::HashSet;

use sp_core::config::PROGRAM_INPUT_ID;

/// Why a remote scene press leaves `SP-program` unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    /// cg OBS did not accept the switch (unknown scene, not reachable).
    NotSwitched,
    /// A manual or multi-playlist scene, but the NDI input is not a source.
    InputInactive,
}

impl KeepReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotSwitched => "not_switched",
            Self::InputInactive => "input_inactive",
        }
    }
}

/// What a remote `SetCurrentProgramScene` does to `SP-program`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SceneAction {
    /// Cut to this playlist's output.
    Playlist(i64),
    /// Cut to the NDI input "OBS manuál" (`PROGRAM_INPUT_ID`).
    Input,
    /// Leave the program unchanged.
    Keep(KeepReason),
}

impl SceneAction {
    /// The program source to cut to, `None` to keep the program.
    pub fn source(self) -> Option<i64> {
        match self {
            Self::Playlist(pid) => Some(pid),
            Self::Input => Some(PROGRAM_INPUT_ID),
            Self::Keep(_) => None,
        }
    }

    /// Why the program is kept, `None` for a cut.
    pub fn keep_reason(self) -> Option<KeepReason> {
        match self {
            Self::Keep(reason) => Some(reason),
            Self::Playlist(_) | Self::Input => None,
        }
    }

    /// The telemetry label (`last_remote_cut.action`).
    pub fn label(self) -> &'static str {
        match self {
            Self::Playlist(_) => "playlist",
            Self::Input => "input",
            Self::Keep(_) => "keep",
        }
    }
}

/// Decide the action. `playlists` is `None` when cg OBS did not switch to the
/// scene, else the playlists the scene shows; `input_active` is the #212 rule
/// (`InputSettings::active()`: enabled with a source).
pub fn scene_action(playlists: Option<&HashSet<i64>>, input_active: bool) -> SceneAction {
    let Some(playlists) = playlists else {
        return SceneAction::Keep(KeepReason::NotSwitched);
    };
    let mut ids = playlists.iter();
    match (ids.next(), ids.next()) {
        (Some(&pid), None) => SceneAction::Playlist(pid),
        _ if input_active => SceneAction::Input,
        _ => SceneAction::Keep(KeepReason::InputInactive),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ids: &[i64]) -> HashSet<i64> {
        ids.iter().copied().collect()
    }

    #[test]
    fn a_scene_with_exactly_one_playlist_cuts_to_it_whatever_the_input() {
        assert_eq!(
            scene_action(Some(&set(&[7])), true),
            SceneAction::Playlist(7)
        );
        assert_eq!(
            scene_action(Some(&set(&[7])), false),
            SceneAction::Playlist(7)
        );
    }

    #[test]
    fn a_manual_scene_cuts_to_the_input_while_it_is_a_source() {
        assert_eq!(scene_action(Some(&set(&[])), true), SceneAction::Input);
    }

    #[test]
    fn a_manual_scene_keeps_the_program_when_the_input_is_not_a_source() {
        assert_eq!(
            scene_action(Some(&set(&[])), false),
            SceneAction::Keep(KeepReason::InputInactive)
        );
    }

    #[test]
    fn a_multi_playlist_scene_is_a_manual_scene() {
        assert_eq!(scene_action(Some(&set(&[3, 7])), true), SceneAction::Input);
        assert_eq!(
            scene_action(Some(&set(&[3, 7])), false),
            SceneAction::Keep(KeepReason::InputInactive)
        );
    }

    #[test]
    fn an_unknown_scene_keeps_the_program_even_with_the_input_on() {
        assert_eq!(
            scene_action(None, true),
            SceneAction::Keep(KeepReason::NotSwitched)
        );
        assert_eq!(
            scene_action(None, false),
            SceneAction::Keep(KeepReason::NotSwitched)
        );
    }

    #[test]
    fn the_source_to_cut_to_and_the_labels() {
        assert_eq!(SceneAction::Playlist(7).source(), Some(7));
        assert_eq!(SceneAction::Input.source(), Some(-1));
        assert_eq!(SceneAction::Keep(KeepReason::NotSwitched).source(), None);
        assert_eq!(SceneAction::Playlist(7).label(), "playlist");
        assert_eq!(SceneAction::Input.label(), "input");
        assert_eq!(SceneAction::Keep(KeepReason::InputInactive).label(), "keep");
        assert_eq!(
            SceneAction::Keep(KeepReason::InputInactive).keep_reason(),
            Some(KeepReason::InputInactive)
        );
        assert_eq!(SceneAction::Playlist(7).keep_reason(), None);
        assert_eq!(SceneAction::Input.keep_reason(), None);
        assert_eq!(KeepReason::NotSwitched.as_str(), "not_switched");
        assert_eq!(KeepReason::InputInactive.as_str(), "input_inactive");
    }
}
