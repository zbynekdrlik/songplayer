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
//! #229: while the playlist waits for the retry of its failed opens, the
//! label and the badge both say so, decided by ONE predicate (the badge's
//! on-program claim is an open question, see [`player_program_badge`]).
//! These rules live here (WASM-safe, so the workspace tests and the mutation
//! gate cover them; sp-ui has no unit-test job).

use crate::playback::{OpenFailures, PlaybackMode, PlaybackState, TransportState};

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
    /// #229: it waits for the retry of its failed opens, taken as on
    /// program (SP-program's source, its program black; see
    /// [`player_program_badge`] for the off-program case still open).
    OnProgramRetry,
    /// It is not on program.
    OffProgram,
}

impl ProgramBadge {
    /// The badge's text.
    pub fn label(self) -> &'static str {
        match self {
            ProgramBadge::Unknown => "◌ —",
            ProgramBadge::OnProgram => "● Na programe",
            ProgramBadge::OnProgramRetry => "● Na programe — čaká na ďalší pokus",
            ProgramBadge::OffProgram => "○ Mimo programu",
        }
    }

    /// Whether the badge says the playlist is on program (the Player's
    /// `on` style).
    pub fn is_on_program(self) -> bool {
        matches!(self, ProgramBadge::OnProgram | ProgramBadge::OnProgramRetry)
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

/// #229: the Player's line for a playlist whose videos cannot be opened
/// (its health row's `open_failures`):
/// `Videá sa nedajú otvoriť (N×): {chyba} — ďalší pokus o X s`. X is the
/// wait left the server read on its own clock (`retry_in_ms`): the browser's
/// clock, on another machine, can be off. With no retry pending (the next
/// song is tried at once, an attempt is under way, the playlist was cut off
/// program or paused) the line has no "ďalší pokus".
pub fn open_failures_line(failures: &OpenFailures) -> String {
    let head = format!(
        "Videá sa nedajú otvoriť ({}×): {}",
        failures.count, failures.last_error
    );
    match failures.retry_in_ms {
        Some(left_ms) => format!("{head} — ďalší pokus o {} s", retry_in_s(left_ms)),
        None => head,
    }
}

/// `retry_in_ms` in whole seconds, rounded up: "0 s" only once it is due.
pub fn retry_in_s(retry_in_ms: u64) -> u64 {
    retry_in_ms.div_ceil(1000)
}

/// #229: the state label while a playlist waits for the retry of its
/// failed opens.
pub const RETRY_PENDING_LABEL: &str = "Čaká na ďalší pokus";

/// #229: the playlist waits for the retry of its failed opens, as far as the
/// Player knows: its state is known, its health row's `open_failures` names
/// a retry (`retry_pending`), and the pipeline is not told it decodes. A
/// pipeline told it decodes (transport `Playing`) is past the wait: the
/// retry's Play went out since the 1 Hz health row was read. The ONE rule
/// both the state label and the badge follow, so they never disagree.
fn waits_for_retry(state_known: bool, transport: TransportState, retry_pending: bool) -> bool {
    state_known && retry_pending && transport != TransportState::Playing
}

/// The Player's state label ([`state_label`]), except while the playlist
/// waits for the retry of its failed opens ([`waits_for_retry`]): "Čaká na
/// ďalší pokus", not "Čaká na scénu" (it may well be on program, black). A
/// pipeline told it decodes keeps its label ("Hrá", or "Hrá mimo programu"
/// off program). Until the state is known the label claims nothing ("—",
/// #225), the retry included.
pub fn player_state_label(
    state_known: bool,
    state: PlaybackState,
    transport: TransportState,
    retry_pending: bool,
) -> &'static str {
    if waits_for_retry(state_known, transport, retry_pending) {
        RETRY_PENDING_LABEL
    } else {
        state_label(state_known, state, transport)
    }
}

/// The Player's badge ([`program_badge`]), except while the playlist waits
/// for the retry of its failed opens ([`waits_for_retry`], the label's own
/// rule): "● Na programe — čaká na ďalší pokus". The engine reports
/// `WaitingForScene` then (nothing decodes), which alone reads "○ Mimo
/// programu" for SP-program's source, its program black (design record
/// 6029071745). Open (#229 Design-question 6029484142): a retry is also
/// armed for a playlist ▶'d OFF program (`failure_retry.rs::video_failed`
/// does not read the scene; a cut off program does end one), and neither
/// the WS state nor the health row tells the two apart, so off program this
/// badge claims on program too. A pipeline told it decodes keeps its badge,
/// read from the WS state as always.
pub fn player_program_badge(
    state_known: bool,
    state: PlaybackState,
    transport: TransportState,
    retry_pending: bool,
) -> ProgramBadge {
    if waits_for_retry(state_known, transport, retry_pending) {
        ProgramBadge::OnProgramRetry
    } else {
        program_badge(state_known, state)
    }
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

    /// #229: the seconds left round up, so the line never says "0 s" while
    /// the retry is still ahead; a due retry reads 0.
    #[test]
    fn the_retry_countdown_rounds_up_to_whole_seconds() {
        assert_eq!(retry_in_s(10_000), 10);
        assert_eq!(retry_in_s(4_001), 5, "a part of a second counts whole");
        assert_eq!(retry_in_s(4_000), 4);
        assert_eq!(retry_in_s(1), 1);
        assert_eq!(retry_in_s(0), 0, "due");
    }

    /// #229: while the retry waits the label says so, on or off program; a
    /// playlist told `Playing` keeps "Hrá", and with no retry the label is
    /// the usual one.
    #[test]
    fn the_state_label_says_the_playlist_waits_for_its_retry() {
        assert_eq!(
            player_state_label(true, PlaybackState::WaitingForScene, PAUSED, true),
            "Čaká na ďalší pokus"
        );
        assert_eq!(
            player_state_label(true, PlaybackState::Playing, PLAYING, true),
            "Hrá"
        );
        assert_eq!(
            player_state_label(true, PlaybackState::WaitingForScene, PLAYING, true),
            "Hrá mimo programu",
            "the retry's Play went out off program before the health row moved"
        );
        assert_eq!(
            player_state_label(true, PlaybackState::WaitingForScene, PAUSED, false),
            "Čaká na scénu"
        );
        assert_eq!(
            player_state_label(false, PlaybackState::Idle, IDLE, false),
            "—"
        );
        assert_eq!(
            player_state_label(false, PlaybackState::WaitingForScene, PAUSED, true),
            "—",
            "until the state is known it claims nothing, a retry included"
        );
    }

    /// #229 follow-up (design record 6029071745): with a retry pending, the
    /// badge for each WS state as the engine reports it (with the transport
    /// it comes with), beside the label, which follows the same rule. The
    /// engine's pause after failed opens is `WaitingForScene` / `Paused`: on
    /// program, black. A pipeline told it decodes keeps its own badge and
    /// label (on program "Hrá", off program "Hrá mimo programu").
    #[test]
    fn the_badge_says_on_program_while_the_retry_waits() {
        let table = [
            (
                PlaybackState::Playing,
                PLAYING,
                ProgramBadge::OnProgram,
                "Hrá",
            ),
            (
                PlaybackState::WaitingForScene,
                PAUSED,
                ProgramBadge::OnProgramRetry,
                "Čaká na ďalší pokus",
            ),
            (
                PlaybackState::WaitingForScene,
                PLAYING,
                ProgramBadge::OffProgram,
                "Hrá mimo programu",
            ),
            (
                PlaybackState::Idle,
                IDLE,
                ProgramBadge::OnProgramRetry,
                "Čaká na ďalší pokus",
            ),
        ];
        for (state, transport, badge, label) in table {
            assert_eq!(
                player_program_badge(true, state, transport, true),
                badge,
                "{state:?} / {transport:?}, a retry pending"
            );
            assert_eq!(
                player_state_label(true, state, transport, true),
                label,
                "{state:?} / {transport:?}: the label follows the same rule"
            );
            assert_eq!(
                player_program_badge(false, state, transport, true),
                ProgramBadge::Unknown,
                "{state:?} / {transport:?}: nothing is claimed until the state is known"
            );
            assert_eq!(
                player_program_badge(true, state, transport, false),
                program_badge(true, state),
                "{state:?} / {transport:?}: no retry, the WS state's badge"
            );
        }
        assert_eq!(
            ProgramBadge::OnProgramRetry.label(),
            "● Na programe — čaká na ďalší pokus"
        );
    }

    /// Both on-program badges take the Player's `on` style; the retry one
    /// is on program too, only black.
    #[test]
    fn both_on_program_badges_take_the_on_style() {
        assert!(ProgramBadge::OnProgram.is_on_program());
        assert!(ProgramBadge::OnProgramRetry.is_on_program());
        assert!(!ProgramBadge::OffProgram.is_on_program());
        assert!(!ProgramBadge::Unknown.is_on_program());
    }

    #[test]
    fn the_open_failures_line_names_the_count_the_error_and_the_retry() {
        let mut failures = OpenFailures {
            count: 4,
            last_error: "No video: SetCurrentMediaType failed: No suitable transform".into(),
            retry_at_ms: Some(1_791_331_230_000),
            retry_in_ms: Some(29_500),
        };
        assert_eq!(
            open_failures_line(&failures),
            "Videá sa nedajú otvoriť (4×): No video: SetCurrentMediaType failed: \
             No suitable transform — ďalší pokus o 30 s"
        );
        failures.retry_at_ms = None;
        failures.retry_in_ms = None;
        assert_eq!(
            open_failures_line(&failures),
            "Videá sa nedajú otvoriť (4×): No video: SetCurrentMediaType failed: \
             No suitable transform",
            "no retry pending: no countdown"
        );
    }
}
