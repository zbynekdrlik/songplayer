//! RED unit + structural guards (#144, "Rollout blocker #3") for the
//! 30-minute lyrics duration cap.
//!
//! The catalog's five > 30-min videos (36–70 min) are live sets / mixes with
//! no single lyric sheet; each retry of one is a full ~1 h GPU burn on the
//! shared live PC. The longest actual song is 21 min (already aligned).
//! `process_song` must stamp any row over the cap `unsupported_source`
//! BEFORE any gather/network/GPU work runs.

use crate::lyrics::MAX_LYRICS_DURATION_MS;
use crate::lyrics::worker_outcome::exceeds_duration_cap;

#[test]
fn max_lyrics_duration_is_thirty_minutes() {
    assert_eq!(MAX_LYRICS_DURATION_MS, 1_800_000);
}

#[test]
fn exceeds_duration_cap_boundary() {
    // One ms over the cap is rejected; exactly at the cap is kept; unknown
    // duration is kept (still worth attempting).
    assert!(exceeds_duration_cap(Some(1_800_001)));
    assert!(!exceeds_duration_cap(Some(1_800_000)));
    assert!(!exceeds_duration_cap(None));
}

/// Structural: the cap check (`exceeds_duration_cap` / `MAX_LYRICS_DURATION_MS`)
/// must occur BEFORE the first `gather_sources(` call inside `process_song`,
/// so an over-cap row never reaches gather/network/GPU work. CRLF-normalised
/// for the Windows CI checkout.
#[test]
fn duration_cap_checked_before_gather_sources_in_process_song() {
    let src = include_str!("worker.rs").replace("\r\n", "\n");
    let ps = src
        .find("async fn process_song")
        .expect("process_song must exist");
    let body = &src[ps..];
    let cap_pos = body
        .find("exceeds_duration_cap")
        .or_else(|| body.find("MAX_LYRICS_DURATION_MS"))
        .expect("process_song must reference the duration cap");
    let gather_pos = body
        .find("gather_sources(")
        .expect("process_song must call gather_sources");
    assert!(
        cap_pos < gather_pos,
        "the duration cap must be checked BEFORE gather_sources runs in process_song"
    );
}
