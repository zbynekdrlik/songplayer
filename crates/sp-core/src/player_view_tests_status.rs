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

/// #266: the Player says the truth about a paused playlist off program
/// (the owner's ytfast at PP): "Pozastavené (mimo programu)", never "Čaká
/// na scénu" — since #221 L4b nothing waits for a scene; the pipeline is
/// paused. A retry of failed opens keeps its own label (#229).
#[test]
fn a_paused_playlist_off_program_reads_paused_off_program() {
    for transport in [PAUSED, IDLE] {
        assert_eq!(
            state_label(true, PlaybackState::WaitingForScene, transport),
            "Pozastavené (mimo programu)",
            "{transport:?}"
        );
        assert_eq!(
            player_state_label(true, PlaybackState::WaitingForScene, transport, false),
            "Pozastavené (mimo programu)",
            "{transport:?}"
        );
    }
    assert_eq!(
        player_state_label(true, PlaybackState::WaitingForScene, PAUSED, true),
        "Čaká na ďalší pokus"
    );
}

/// #266: a preview is offered only while the pipeline DECODES a song (its
/// own transport, #201), on program or off (the dub prepared off program);
/// a paused pipeline (the owner's ytslow at PP: `WaitingForScene`, transport
/// `Paused`) gives the encoder no picture, so it is never offered there.
#[test]
fn the_preview_is_offered_only_while_the_pipeline_decodes() {
    use super::PreviewSlot::{Idle as SlotIdle, Offered, Paused, Pending};
    let wfs = PlaybackState::WaitingForScene;
    // (state, transport, a song loaded, the slot)
    let table = [
        (PlaybackState::Playing, PLAYING, true, Offered),
        (wfs, PLAYING, true, Offered),
        (wfs, PAUSED, true, Paused),
        (wfs, IDLE, true, Paused),
        (PlaybackState::Playing, PAUSED, true, Paused),
        (PlaybackState::Idle, PLAYING, true, Offered),
        (PlaybackState::Idle, PAUSED, true, SlotIdle),
        (PlaybackState::Idle, IDLE, true, SlotIdle),
        // Decoding, its song not here yet (a ▶ before `Started`).
        (PlaybackState::Playing, PLAYING, false, Pending),
        (wfs, PLAYING, false, Pending),
        // Nothing loaded: nothing to preview.
        (wfs, PAUSED, false, SlotIdle),
        (PlaybackState::Idle, IDLE, false, SlotIdle),
    ];
    for (state, transport, song, slot) in table {
        assert_eq!(
            preview_slot(true, song, state, transport),
            slot,
            "{state:?} / {transport:?} / song {song}"
        );
        assert_eq!(
            preview_slot(false, song, state, transport),
            Pending,
            "{state:?} / {transport:?} / song {song}: not told yet"
        );
    }
}

/// The slot says why it offers no preview; the owner's paused card reads
/// the paused line, an idle one the idle line.
#[test]
fn the_preview_slot_says_why_it_offers_no_preview() {
    let paused = preview_slot(true, true, PlaybackState::WaitingForScene, PAUSED);
    assert_eq!(
        paused.text(),
        "Náhľad sa zobrazí, keď playlist hrá — tento je pozastavený."
    );
    assert_eq!(
        PreviewSlot::Idle.text(),
        "Náhľad sa zobrazí, keď playlist hrá — tento nehrá."
    );
    assert_eq!(PreviewSlot::Pending.text(), "Načítavam…");
    assert_eq!(PreviewSlot::Offered.text(), "");
    assert_eq!(
        PreviewSlot::GpuDecoded.text(),
        "Náhľad nedostupný pri GPU dekódovaní."
    );
}

/// The operator's preview ends only when nothing plays: never while the
/// state is unknown (a reconnect brings it back by itself, #225) nor at a
/// pause.
#[test]
fn only_an_idle_playlist_ends_the_operators_preview() {
    assert!(PreviewSlot::Idle.ends_preview());
    assert!(!PreviewSlot::Paused.ends_preview());
    assert!(!PreviewSlot::Pending.ends_preview());
    assert!(!PreviewSlot::Offered.ends_preview());
    assert!(!PreviewSlot::GpuDecoded.ends_preview(), "#279 P3a");
}

/// #279 P3a: a playlist that decodes on the GPU offers no preview (its
/// pictures reach no tap before P3b); every other slot is untouched.
#[test]
fn a_gpu_decoded_playlist_offers_no_preview_and_says_why() {
    use super::PreviewSlot::{GpuDecoded, Idle as SlotIdle, Offered, Paused, Pending};
    for slot in [Offered, Paused, Pending, SlotIdle] {
        assert_eq!(gpu_preview(slot, false), slot, "{slot:?} on the CPU");
    }
    assert_eq!(gpu_preview(Offered, true), GpuDecoded);
    for slot in [Paused, Pending, SlotIdle] {
        assert_eq!(gpu_preview(slot, true), slot, "{slot:?} on the GPU");
    }
    for (on, showing) in [(false, false), (true, false), (true, true)] {
        assert_eq!(
            preview_view(GpuDecoded, on, showing),
            PreviewView::Placeholder,
            "started {on} / showing {showing}"
        );
    }
}

/// What the slot shows, for every slot, started or not, showing or not.
#[test]
fn a_new_preview_mounts_only_where_one_is_offered() {
    use super::PreviewSlot::{Idle as SlotIdle, Offered, Paused, Pending};
    use super::PreviewView::{Placeholder, Start, Video};
    // (slot, the operator started it, the slot shows it now, the view)
    let table = [
        (Offered, false, false, Start),
        (Offered, false, true, Start),
        (Offered, true, false, Video),
        (Offered, true, true, Video),
        // Paused: no new preview; a showing one holds its last picture.
        (Paused, false, false, Placeholder),
        (Paused, false, true, Placeholder),
        (Paused, true, false, Placeholder),
        (Paused, true, true, Video),
        (Pending, false, false, Placeholder),
        (Pending, false, true, Placeholder),
        (Pending, true, false, Placeholder),
        (Pending, true, true, Placeholder),
        (SlotIdle, false, false, Placeholder),
        (SlotIdle, false, true, Placeholder),
        (SlotIdle, true, false, Placeholder),
        (SlotIdle, true, true, Placeholder),
    ];
    for (slot, on, showing, view) in table {
        assert_eq!(
            preview_view(slot, on, showing),
            view,
            "{slot:?} / started {on} / showing {showing}"
        );
    }
}

/// The owner's path (#184, `post-deploy-preview.spec.ts`): a preview
/// started while the song plays survives a pause — the same element holds
/// the last picture — and plays on at the resume; a reconnect while paused
/// mounts none until the song plays again (it would get no picture).
#[test]
fn a_running_preview_holds_through_a_pause_and_a_reconnect_waits_for_the_resume() {
    use super::PreviewSlot::{Offered, Paused, Pending};
    // Each step: (the slot, the operator's preview on).
    let walk = |steps: &[(PreviewSlot, bool)]| -> Vec<PreviewView> {
        let mut shown = PreviewView::Placeholder;
        steps
            .iter()
            .map(|&(slot, on)| {
                shown = preview_view(slot, on, shown == PreviewView::Video);
                shown
            })
            .collect()
    };
    assert_eq!(
        walk(&[
            (Offered, false),
            (Offered, true),
            (Paused, true),
            (Offered, true)
        ]),
        [
            PreviewView::Start,
            PreviewView::Video,
            PreviewView::Video,
            PreviewView::Video
        ]
    );
    assert_eq!(
        walk(&[
            (Offered, true),
            (Pending, true),
            (Paused, true),
            (Offered, true)
        ]),
        [
            PreviewView::Video,
            PreviewView::Placeholder,
            PreviewView::Placeholder,
            PreviewView::Video
        ]
    );
}
