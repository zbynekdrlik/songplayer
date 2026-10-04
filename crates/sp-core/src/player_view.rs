//! What the shared Player may claim about a playlist (#225).
//!
//! The Player (`sp-ui` `components/player.rs`) renders a playlist from its
//! dashboard WS store entry. Two facts can be missing there:
//!
//! - its STATE: no `PlaybackStateChanged` reached it since the page loaded (the
//!   server's on-connect replay tells every playlist's state, so this is the
//!   moment before the replay lands);
//! - its SONG: no `NowPlaying` with content yet.
//!
//! The Player never shows a guess for either. It says "Nič nehrá" only when it
//! was told nothing plays, and "● Na programe" / "○ Mimo programu" only when it
//! was told the state. The badge reads the same WS state as the state label
//! (`Playing` = on program, #170), so a program cut flips both in one render.
//! These rules live here (WASM-safe, so the workspace tests and the mutation
//! gate cover them; sp-ui has no unit-test job).

use crate::playback::{PlaybackMode, PlaybackState, TransportState};

/// The title while the song is not known yet.
pub const PENDING_TITLE: &str = "Načítavam…";
/// The title when the Player was told nothing plays.
pub const IDLE_TITLE: &str = "Nič nehrá";
/// The title of a song that plays without a name (its DB lookup failed).
pub const UNTITLED: &str = "Bez názvu";
/// The state label while the state is not known yet.
pub const UNKNOWN_STATE: &str = "—";

/// What the now-playing area (the title and the mixer slot) shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NowPlayingView {
    /// Not known yet: no state was told, or the pipeline plays and its song
    /// has not arrived. "Načítavam…".
    Pending,
    /// Told that nothing plays. "Nič nehrá".
    Idle,
    /// A song, playing or paused.
    Song,
}

/// The now-playing view. `state_known` = a `PlaybackStateChanged` reached the
/// playlist since the page loaded; `has_song` = it has a `NowPlaying` with
/// content; `transport` = the pipeline's own transport (#201).
pub fn now_playing_view(
    state_known: bool,
    has_song: bool,
    transport: TransportState,
) -> NowPlayingView {
    if has_song {
        NowPlayingView::Song
    } else if state_known && transport != TransportState::Playing {
        NowPlayingView::Idle
    } else {
        NowPlayingView::Pending
    }
}

/// The Player's title for `view`: "Song — Artist" for a song (the artist
/// only when known), else the pending or idle text.
pub fn player_title(view: NowPlayingView, song: &str, artist: &str) -> String {
    match view {
        NowPlayingView::Pending => PENDING_TITLE.to_string(),
        NowPlayingView::Idle => IDLE_TITLE.to_string(),
        NowPlayingView::Song => {
            let song = if song.is_empty() { UNTITLED } else { song };
            if artist.is_empty() {
                song.to_string()
            } else {
                format!("{song} — {artist}")
            }
        }
    }
}

/// The Player's state label. #221 L4b: a ▶ claims no program, so a playlist
/// that is not on air plays OFF program — the scene-aware `state` is
/// `WaitingForScene` while the transport is `Playing`.
pub fn state_label(
    state_known: bool,
    state: PlaybackState,
    transport: TransportState,
) -> &'static str {
    if !state_known {
        return UNKNOWN_STATE;
    }
    match (state, transport) {
        (PlaybackState::Playing, _) => "Hrá",
        (PlaybackState::Idle, _) => "Nehrá",
        (PlaybackState::WaitingForScene, TransportState::Playing) => "Hrá mimo programu",
        (PlaybackState::WaitingForScene, _) => "Čaká na scénu",
    }
}

/// The on/off-program badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramBadge {
    /// The state is not known yet.
    Unknown,
    /// The wall shows this playlist.
    OnProgram,
    /// It is not on program.
    OffProgram,
}

impl ProgramBadge {
    /// The badge's text.
    pub fn label(self) -> &'static str {
        match self {
            ProgramBadge::Unknown => "◌ —",
            ProgramBadge::OnProgram => "● Na programe",
            ProgramBadge::OffProgram => "○ Mimo programu",
        }
    }
}

/// The badge from the playlist's WS state: `Playing` is the scene-aware
/// "plays on program" (a playlist playing off program is `WaitingForScene`).
pub fn program_badge(state_known: bool, state: PlaybackState) -> ProgramBadge {
    if !state_known {
        ProgramBadge::Unknown
    } else if state == PlaybackState::Playing {
        ProgramBadge::OnProgram
    } else {
        ProgramBadge::OffProgram
    }
}

/// The play/pause toggle's `(label, title)` (review round 4). `playing` = the
/// pipeline's own transport plays (#201). Until the state is known the toggle
/// claims neither: the Player disables it too.
pub fn play_pause(state_known: bool, playing: bool) -> (&'static str, &'static str) {
    if !state_known {
        ("⏯", "Načítavam…")
    } else if playing {
        ("⏸ Pauza", "Pauza")
    } else {
        ("▶ Prehrať", "Prehrať")
    }
}

/// The mode select's value (review round 4): the mode the server told, or
/// `""` (the select's "—" option) until the state is known. The Player
/// disables the select then.
pub fn mode_value(state_known: bool, mode: PlaybackMode) -> &'static str {
    if state_known { mode.as_str() } else { "" }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAYING: TransportState = TransportState::Playing;
    const PAUSED: TransportState = TransportState::Paused;
    const IDLE: TransportState = TransportState::Idle;

    #[test]
    fn a_song_shows_whatever_else_is_known() {
        for known in [false, true] {
            for transport in [PLAYING, PAUSED, IDLE] {
                assert_eq!(
                    now_playing_view(known, true, transport),
                    NowPlayingView::Song
                );
            }
        }
    }

    #[test]
    fn nothing_plays_only_once_told_and_never_while_the_transport_plays() {
        // Told the state, no song, not decoding: nothing plays.
        assert_eq!(now_playing_view(true, false, PAUSED), NowPlayingView::Idle);
        assert_eq!(now_playing_view(true, false, IDLE), NowPlayingView::Idle);
        // Told it decodes, its song not arrived yet: pending, not idle.
        assert_eq!(
            now_playing_view(true, false, PLAYING),
            NowPlayingView::Pending
        );
        // Not told anything yet: pending, whatever the default transport.
        assert_eq!(
            now_playing_view(false, false, PAUSED),
            NowPlayingView::Pending
        );
        assert_eq!(
            now_playing_view(false, false, IDLE),
            NowPlayingView::Pending
        );
        assert_eq!(
            now_playing_view(false, false, PLAYING),
            NowPlayingView::Pending
        );
    }

    #[test]
    fn the_title_names_the_song_or_says_why_it_cannot() {
        assert_eq!(
            player_title(NowPlayingView::Song, "Song", "Artist"),
            "Song — Artist"
        );
        assert_eq!(player_title(NowPlayingView::Song, "Song", ""), "Song");
        // A song without a name still plays: never "Nič nehrá".
        assert_eq!(
            player_title(NowPlayingView::Song, "", "Artist"),
            "Bez názvu — Artist"
        );
        assert_eq!(player_title(NowPlayingView::Song, "", ""), "Bez názvu");
        // Pending / idle ignore any stale song text.
        assert_eq!(
            player_title(NowPlayingView::Pending, "Old", "Artist"),
            "Načítavam…"
        );
        assert_eq!(
            player_title(NowPlayingView::Idle, "Old", "Artist"),
            "Nič nehrá"
        );
    }

    #[test]
    fn the_state_label_is_neutral_until_the_state_is_known() {
        for state in [
            PlaybackState::Idle,
            PlaybackState::WaitingForScene,
            PlaybackState::Playing,
        ] {
            for transport in [PLAYING, PAUSED, IDLE] {
                assert_eq!(state_label(false, state, transport), "—");
            }
        }
        assert_eq!(state_label(true, PlaybackState::Playing, PLAYING), "Hrá");
        assert_eq!(state_label(true, PlaybackState::Idle, IDLE), "Nehrá");
        assert_eq!(
            state_label(true, PlaybackState::WaitingForScene, PLAYING),
            "Hrá mimo programu"
        );
        assert_eq!(
            state_label(true, PlaybackState::WaitingForScene, PAUSED),
            "Čaká na scénu"
        );
    }

    #[test]
    fn the_badge_follows_the_ws_state_and_is_neutral_until_known() {
        assert_eq!(
            program_badge(false, PlaybackState::Playing),
            ProgramBadge::Unknown
        );
        assert_eq!(
            program_badge(true, PlaybackState::Playing),
            ProgramBadge::OnProgram
        );
        // Playing off program is WaitingForScene on the wire (#170).
        assert_eq!(
            program_badge(true, PlaybackState::WaitingForScene),
            ProgramBadge::OffProgram
        );
        assert_eq!(
            program_badge(true, PlaybackState::Idle),
            ProgramBadge::OffProgram
        );
        assert_eq!(ProgramBadge::Unknown.label(), "◌ —");
        assert_eq!(ProgramBadge::OnProgram.label(), "● Na programe");
        assert_eq!(ProgramBadge::OffProgram.label(), "○ Mimo programu");
    }

    #[test]
    fn the_toggle_claims_neither_until_the_state_is_known() {
        assert_eq!(play_pause(false, true), ("⏯", "Načítavam…"));
        assert_eq!(play_pause(false, false), ("⏯", "Načítavam…"));
        assert_eq!(play_pause(true, true), ("⏸ Pauza", "Pauza"));
        assert_eq!(play_pause(true, false), ("▶ Prehrať", "Prehrať"));
    }

    #[test]
    fn the_mode_select_shows_the_told_mode_only() {
        assert_eq!(mode_value(false, PlaybackMode::Loop), "");
        assert_eq!(mode_value(true, PlaybackMode::Loop), "loop");
        assert_eq!(mode_value(true, PlaybackMode::Single), "single");
        assert_eq!(mode_value(true, PlaybackMode::Continuous), "continuous");
    }
}
