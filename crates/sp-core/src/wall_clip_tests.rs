//! #275: SongPlayer's clip on the wall — its states, the note, the tooltip's
//! sentences, the wire shape and "a song plays on program".

use serde_json::json;

use super::*;
use crate::playback::PlaybackState;

/// 15.10.2026 12:00:00Z, the server's clock.
const NOW: i64 = 1_791_979_200_000;

/// SNV's running clip (the main session's box read, 15.10.2026).
fn yt_music() -> ClipView {
    ClipView {
        layer: "L# Fill".to_string(),
        clip: "Yt music".to_string(),
        source: "SP-program-MAX".to_string(),
    }
}

/// SNV's NDI background clip.
fn snv_bg() -> ClipView {
    ClipView {
        layer: "L # BG".to_string(),
        clip: "SNV BG".to_string(),
        source: NDI_SOURCE.to_string(),
    }
}

/// A fact in `state` since `held_ms` before [`NOW`].
fn clip(
    state: ClipState,
    clips: Vec<ClipView>,
    ndi: Vec<ClipView>,
    held_ms: i64,
) -> SongplayerClip {
    SongplayerClip {
        state,
        clips,
        ndi_clips: ndi,
        since_ms: Some(NOW - held_ms),
        now_ms: Some(NOW),
        ..SongplayerClip::default()
    }
}

#[test]
fn the_states_have_their_wire_names() {
    assert_eq!(ClipState::Unknown.name(), "unknown");
    assert_eq!(ClipState::Running.name(), "running");
    assert_eq!(ClipState::NotRunning.name(), "none");
    assert_eq!(
        serde_json::to_value([
            ClipState::Unknown,
            ClipState::Running,
            ClipState::NotRunning
        ])
        .unwrap(),
        json!(["unknown", "running", "none"])
    );
    assert_eq!(ClipState::default(), ClipState::Unknown);
}

#[test]
fn songplayer_s_sources_are_its_two_spout_senders() {
    assert_eq!(SPOUT_SOURCES, ["SP-program-MAX", "SP-program"]);
    assert_eq!(NDI_SOURCE, "NDI Source");
}

#[test]
fn the_note_is_only_for_a_song_on_program_with_no_clip_running() {
    let none = clip(ClipState::NotRunning, vec![], vec![], 0);
    assert!(none.missing(true));
    assert!(!none.missing(false), "no song on program: no note");
    let running = clip(ClipState::Running, vec![yt_music()], vec![], 0);
    assert!(!running.missing(true));
    assert!(
        !SongplayerClip::default().missing(true),
        "unknown: no claim"
    );
}

#[test]
fn how_long_is_read_on_the_server_s_clock() {
    let twelve = clip(ClipState::NotRunning, vec![], vec![], 12 * 60_000 + 30_000);
    assert_eq!(twelve.held_for_sk().as_deref(), Some("12 min"));
    assert_eq!(twelve.short_sk(), "nebeží Spout klip SongPlayeru (12 min)");
    let long = clip(
        ClipState::NotRunning,
        vec![],
        vec![],
        (5 * 60 + 26) * 60_000,
    );
    assert_eq!(long.held_for_sk().as_deref(), Some("5 h 26 min"));
    let just = clip(ClipState::NotRunning, vec![], vec![], 59_999);
    assert_eq!(just.held_for_sk().as_deref(), Some("menej ako minútu"));
    // An instant missing: no duration, the note alone.
    let no_since = SongplayerClip {
        since_ms: None,
        ..twelve.clone()
    };
    assert_eq!(no_since.held_for_sk(), None);
    assert_eq!(no_since.short_sk(), NOTE_SK);
    let no_now = SongplayerClip {
        now_ms: None,
        ..twelve
    };
    assert_eq!(no_now.held_for_sk(), None);
    assert_eq!(NOTE_SK, "nebeží Spout klip SongPlayeru");
}

#[test]
fn the_tooltip_names_the_running_clips() {
    let mut second = yt_music();
    second.clip = "Svedectvo".to_string();
    second.layer = "L# Fit".to_string();
    let running = clip(
        ClipState::Running,
        vec![yt_music(), second],
        vec![snv_bg()],
        0,
    );
    assert_eq!(
        running.text_sk().as_deref(),
        Some(
            "Na stene beží klip SongPlayeru: L# Fill — Yt music (SP-program-MAX); \
             L# Fit — Svedectvo (SP-program-MAX)."
        )
    );
}

#[test]
fn the_tooltip_says_none_runs_for_how_long_and_names_the_ndi_clips() {
    let none = clip(ClipState::NotRunning, vec![], vec![snv_bg()], 12 * 60_000);
    assert_eq!(
        none.text_sk().as_deref(),
        Some(
            "Na stene už 12 min nebeží žiadny Spout klip SongPlayeru (SP-program-MAX ani \
             SP-program). Klipy spúšťa len operátor zo Stream Decku, SongPlayer žiadny \
             nespúšťa. Klip so zdrojom NDI SongPlayer nerozozná (Resolume nepovie, aký NDI \
             zdroj má); viditeľné NDI klipy: L # BG — SNV BG."
        )
    );
    let bare = SongplayerClip {
        state: ClipState::NotRunning,
        ..SongplayerClip::default()
    };
    assert_eq!(
        bare.text_sk().as_deref(),
        Some(
            "Na stene nebeží žiadny Spout klip SongPlayeru (SP-program-MAX ani SP-program). \
             Klipy spúšťa len operátor zo Stream Decku, SongPlayer žiadny nespúšťa. Klip so \
             zdrojom NDI SongPlayer nerozozná (Resolume nepovie, aký NDI zdroj má)."
        )
    );
    assert_eq!(
        SongplayerClip::default().text_sk(),
        None,
        "unknown: nothing"
    );
}

#[test]
fn a_song_on_program_is_a_told_playing_playlist_with_a_song() {
    assert!(song_on_program(true, PlaybackState::Playing, true));
    assert!(
        !song_on_program(false, PlaybackState::Playing, true),
        "not told yet"
    );
    assert!(
        !song_on_program(true, PlaybackState::Playing, false),
        "no song"
    );
    assert!(
        !song_on_program(true, PlaybackState::WaitingForScene, true),
        "off program"
    );
    assert!(!song_on_program(true, PlaybackState::Idle, true));
}

#[test]
fn the_fact_travels_with_every_field_named() {
    let fact = clip(ClipState::Running, vec![yt_music()], vec![snv_bg()], 60_000);
    let wire = serde_json::to_value(&fact).unwrap();
    assert_eq!(
        wire,
        json!({
            "state": "running",
            "clips": [{"layer": "L# Fill", "clip": "Yt music", "source": "SP-program-MAX"}],
            "ndi_clips": [{"layer": "L # BG", "clip": "SNV BG", "source": "NDI Source"}],
            "since_ms": NOW - 60_000,
            "now_ms": NOW,
            "read_error": null,
            "reader_fault": false,
        })
    );
    let back: SongplayerClip = serde_json::from_value(wire).unwrap();
    assert_eq!(back, fact);
    // A host's reader fault travels with its reason.
    let fault = SongplayerClip {
        read_error: Some("param 7: error decoding response body".to_string()),
        reader_fault: true,
        ..SongplayerClip::default()
    };
    let wire = serde_json::to_value(&fault).unwrap();
    assert_eq!(wire["read_error"], "param 7: error decoding response body");
    assert_eq!(wire["reader_fault"], true);
    assert_eq!(
        serde_json::from_value::<SongplayerClip>(wire).unwrap(),
        fault
    );
    let none: SongplayerClip = serde_json::from_value(json!({"state": "none"})).unwrap();
    assert_eq!(none.state, ClipState::NotRunning);
    let empty: SongplayerClip = serde_json::from_value(json!({})).unwrap();
    assert_eq!(empty, SongplayerClip::default());
}
