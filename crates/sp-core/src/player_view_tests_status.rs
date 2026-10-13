//! #266: the dashboard says in words what plays — the playlist list's
//! status, the paused label and the preview slot.

use super::*;
use crate::playback::{PlaybackState, TransportState};

const PLAYING: TransportState = TransportState::Playing;
const PAUSED: TransportState = TransportState::Paused;
const IDLE: TransportState = TransportState::Idle;

/// The owner's case (13.10.2026): ytslow on program, ytfast paused off
/// program, a dub prepared off program, an idle playlist — each said in
/// words, never as a ▶ / ⏸ glyph.
#[test]
fn every_playlist_state_is_said_in_words() {
    let table = [
        (
            PlaybackState::Playing,
            PLAYING,
            PlaylistStatus::OnProgram,
            "NA PROGRAME",
        ),
        (
            PlaybackState::WaitingForScene,
            PLAYING,
            PlaylistStatus::PlaysOffProgram,
            "hrá mimo programu",
        ),
        (
            PlaybackState::WaitingForScene,
            PAUSED,
            PlaylistStatus::Paused,
            "pozastavené",
        ),
        (
            PlaybackState::WaitingForScene,
            IDLE,
            PlaylistStatus::Paused,
            "pozastavené",
        ),
        (PlaybackState::Idle, IDLE, PlaylistStatus::Idle, "nehrá"),
        // An Idle state with a transport that says otherwise: the state
        // label's own reading ("Nehrá") wins.
        (PlaybackState::Idle, PLAYING, PlaylistStatus::Idle, "nehrá"),
        (PlaybackState::Idle, PAUSED, PlaylistStatus::Idle, "nehrá"),
    ];
    for (state, transport, status, label) in table {
        assert_eq!(
            playlist_status(true, state, transport, false),
            status,
            "{state:?} / {transport:?}"
        );
        assert_eq!(status.label(), label, "{status:?}");
    }
}

/// Until the replay tells the state, a row claims nothing (#225).
#[test]
fn a_row_says_nothing_until_the_state_is_known() {
    for state in [
        PlaybackState::Idle,
        PlaybackState::WaitingForScene,
        PlaybackState::Playing,
    ] {
        for transport in [PLAYING, PAUSED, IDLE] {
            for retry in [false, true] {
                assert_eq!(
                    playlist_status(false, state, transport, retry),
                    PlaylistStatus::Unknown,
                    "{state:?} / {transport:?} / retry {retry}"
                );
            }
        }
    }
    assert_eq!(PlaylistStatus::Unknown.label(), "");
}

/// "On program" is the Player badge's own rule: a retry of SP-program's
/// source waits on program (#229), so the row says "NA PROGRAME" with the
/// badge's "● Na programe — čaká na ďalší pokus"; a retry of a playlist
/// played off program keeps the paused word, as its badge stays off.
#[test]
fn a_row_is_on_program_exactly_when_the_players_badge_is() {
    for state in [
        PlaybackState::Idle,
        PlaybackState::WaitingForScene,
        PlaybackState::Playing,
    ] {
        for transport in [PLAYING, PAUSED, IDLE] {
            for retry in [false, true] {
                let row = playlist_status(true, state, transport, retry);
                let badge = player_program_badge(true, state, transport, retry);
                assert_eq!(
                    row == PlaylistStatus::OnProgram,
                    badge.is_on_program(),
                    "{state:?} / {transport:?} / retry {retry}: row {row:?}, badge {badge:?}"
                );
            }
        }
    }
    assert_eq!(
        playlist_status(true, PlaybackState::WaitingForScene, PAUSED, true),
        PlaylistStatus::OnProgram
    );
    assert_eq!(
        playlist_status(true, PlaybackState::WaitingForScene, PAUSED, false),
        PlaylistStatus::Paused
    );
}

#[test]
fn each_status_has_its_own_code() {
    let codes = [
        (PlaylistStatus::Unknown, "unknown"),
        (PlaylistStatus::OnProgram, "on_program"),
        (PlaylistStatus::PlaysOffProgram, "off_program"),
        (PlaylistStatus::Paused, "paused"),
        (PlaylistStatus::Idle, "idle"),
    ];
    for (status, code) in codes {
        assert_eq!(status.code(), code, "{status:?}");
    }
}

/// The mobile `<select>`: the playlist, its scene label and the word; the
/// name alone until the state is known.
#[test]
fn the_mobile_option_names_the_playlist_and_its_state() {
    assert_eq!(
        picker_option_text("ytslow", "SP-slow", PlaylistStatus::OnProgram),
        "ytslow (SP-slow) — NA PROGRAME"
    );
    assert_eq!(
        picker_option_text("ytfast", "SP-fast", PlaylistStatus::Paused),
        "ytfast (SP-fast) — pozastavené"
    );
    assert_eq!(
        picker_option_text("Dabing", "SP-dabing", PlaylistStatus::PlaysOffProgram),
        "Dabing (SP-dabing) — hrá mimo programu"
    );
    assert_eq!(
        picker_option_text("yt90s", "SP-90s", PlaylistStatus::Idle),
        "yt90s (SP-90s) — nehrá"
    );
    assert_eq!(
        picker_option_text("ytworship", "SP-worship", PlaylistStatus::Unknown),
        "ytworship (SP-worship)"
    );
}
