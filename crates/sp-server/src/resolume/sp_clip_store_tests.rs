//! #275: the record of SongPlayer's clip — every host's fact combined, the
//! instant the state changed, the server's clock on each read.

use sp_core::wall_clip::{ClipState, ClipView};

use super::*;
use crate::resolume::sp_clips::Fact;

fn clip(layer: &str, clip: &str) -> ClipView {
    ClipView {
        layer: layer.to_string(),
        clip: clip.to_string(),
        source: "SP-program-MAX".to_string(),
    }
}

fn fact(state: ClipState, clips: Vec<ClipView>) -> Fact {
    Fact {
        state,
        clips,
        ..Fact::default()
    }
}

#[test]
fn nothing_published_is_unknown_with_no_clock() {
    let store = SpClipStore::default();
    let now = store.current(5_000);
    assert_eq!(now.state, ClipState::Unknown);
    assert_eq!(now.since_ms, None);
    assert_eq!(now.now_ms, Some(5_000), "stamped with the server's clock");
}

#[test]
fn the_first_fact_starts_the_clock_even_in_the_default_state() {
    let store = SpClipStore::default();
    store.publish("snv:8090", Fact::unknown("not loaded"), 1_000);
    let now = store.current(2_000);
    assert_eq!(now.state, ClipState::Unknown);
    assert_eq!(now.since_ms, Some(1_000));
    assert_eq!(now.now_ms, Some(2_000));
}

#[test]
fn the_clock_moves_only_when_the_state_changes() {
    let store = SpClipStore::default();
    store.publish(
        "snv:8090",
        fact(ClipState::Running, vec![clip("L# Fill", "Yt music")]),
        1_000,
    );
    assert_eq!(store.current(1_500).since_ms, Some(1_000));
    // The operator moved to another SongPlayer clip: still running.
    store.publish(
        "snv:8090",
        fact(ClipState::Running, vec![clip("L# Fit", "Svedectvo")]),
        2_000,
    );
    let now = store.current(2_500);
    assert_eq!(now.since_ms, Some(1_000));
    assert_eq!(now.clips, vec![clip("L# Fit", "Svedectvo")]);
    // Arena relaunched: none runs.
    store.publish("snv:8090", fact(ClipState::NotRunning, Vec::new()), 3_000);
    let now = store.current(3_500);
    assert_eq!(now.state, ClipState::NotRunning);
    assert_eq!(now.since_ms, Some(3_000));
    assert!(now.clips.is_empty());
}

#[test]
fn hosts_combine_running_first_then_unknown_then_none() {
    let running = fact(ClipState::Running, vec![clip("L# Fill", "Yt music")]);
    let none = fact(ClipState::NotRunning, Vec::new());
    let unknown = Fact::unknown("no answer");
    let state = |facts: &[&Fact]| combine(facts.iter().copied()).state;
    assert_eq!(state(&[]), ClipState::Unknown, "no Resolume host: no claim");
    assert_eq!(state(&[&running]), ClipState::Running);
    assert_eq!(state(&[&none]), ClipState::NotRunning);
    assert_eq!(state(&[&unknown]), ClipState::Unknown);
    assert_eq!(state(&[&none, &none]), ClipState::NotRunning);
    assert_eq!(state(&[&none, &unknown]), ClipState::Unknown);
    assert_eq!(state(&[&unknown, &running]), ClipState::Running);
    assert_eq!(state(&[&none, &running]), ClipState::Running);
}

#[test]
fn every_host_s_clips_are_listed() {
    let ndi = ClipView {
        layer: "L # BG".to_string(),
        clip: "SNV BG".to_string(),
        source: "NDI Source".to_string(),
    };
    let a = Fact {
        ndi_clips: vec![ndi.clone()],
        ..fact(ClipState::Running, vec![clip("L# Fill", "Yt music")])
    };
    let b = fact(ClipState::Running, vec![clip("L# Fit", "Trailer")]);
    let both = combine([&a, &b]);
    assert_eq!(
        both.clips,
        vec![clip("L# Fill", "Yt music"), clip("L# Fit", "Trailer")]
    );
    assert_eq!(both.ndi_clips, vec![ndi]);
    assert_eq!((both.since_ms, both.now_ms), (None, None));
    let store = SpClipStore::default();
    store.publish("b:8090", b, 1_000);
    store.publish("a:8090", a, 1_000);
    assert_eq!(
        store.current(1_000).clips,
        vec![clip("L# Fill", "Yt music"), clip("L# Fit", "Trailer")],
        "in host order"
    );
}

#[test]
fn the_process_has_one_record() {
    assert!(std::ptr::eq(global(), global()));
}

/// Review round 1: one unread poll (a REST hiccup) between two reads of the
/// same state must not restart the note's "(12 min)".
#[test]
fn a_known_state_counts_from_its_start_across_unknown_gaps() {
    let store = SpClipStore::default();
    store.publish("snv:8090", fact(ClipState::NotRunning, Vec::new()), 1_000);
    store.publish("snv:8090", Fact::unknown("param 7: 500"), 2_000);
    let gap = store.current(2_500);
    assert_eq!((gap.state, gap.since_ms), (ClipState::Unknown, Some(2_000)));
    assert_eq!(gap.read_error.as_deref(), Some("param 7: 500"));
    store.publish("snv:8090", Fact::unknown("param 7: 500"), 2_400);
    assert_eq!(
        store.current(2_500).since_ms,
        Some(2_000),
        "an unknown counts from its own start"
    );
    store.publish("snv:8090", fact(ClipState::NotRunning, Vec::new()), 3_000);
    let back = store.current(3_500);
    assert_eq!(
        (back.state, back.since_ms),
        (ClipState::NotRunning, Some(1_000)),
        "the black wall's count goes on"
    );
    assert_eq!(back.read_error, None);
    // Another known state after a gap starts its own count.
    store.publish("snv:8090", Fact::unknown("no answer"), 4_000);
    store.publish(
        "snv:8090",
        fact(ClipState::Running, vec![clip("L# Fill", "Yt music")]),
        5_000,
    );
    assert_eq!(store.current(5_000).since_ms, Some(5_000));
}

#[test]
fn a_reader_fault_and_the_first_reason_combine_over_hosts() {
    let a = Fact::unknown("Arena does not answer");
    let b = Fact::reader_fault("param 7: error decoding response body");
    let both = combine([&a, &b]);
    assert_eq!(
        both.read_error.as_deref(),
        Some("Arena does not answer"),
        "the first host's"
    );
    assert!(both.reader_fault);
    let running = fact(ClipState::Running, vec![clip("L# Fill", "Yt music")]);
    let fine = combine([&running]);
    assert_eq!((fine.read_error, fine.reader_fault), (None, false));
}
