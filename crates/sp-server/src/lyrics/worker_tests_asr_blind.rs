//! #120 asr_path-blind routing structural guard. Sibling file split from
//! worker_tests.rs to honor the airuleset 1000-line cap.

use super::*;

#[test]
fn zero_candidate_songs_route_to_asr_path_blind() {
    // #120: a song whose `gather_sources` produced ZERO text candidates used
    // to be stamped `unsupported_source` and never reach asr_path — path 3 of
    // the (now-collapsed) decision tree documented on
    // `timed_yt_subs_skips_asr_path_entirely` above. The settled design
    // (#130, 2026-09-12 design comment) instead routes these songs to
    // asr_path BLIND (empty keyterms): AAI Universal-3 Pro transcribes the
    // vocal unbiased, and the existing empty-transcript quarantine
    // (`asr_gap`, see `asr_path::run`) stays the safety net for genuine
    // instrumentals.
    //
    // `gather_sources` depends on real network I/O (yt_subs/lrclib/genius),
    // so — like `process_song_routes_through_should_resolve_spotify` above —
    // this is a structural guard on the gate `if` block in `process_song`
    // rather than a full integration test.
    // Normalise line endings first: the Windows CI checkout is CRLF, and a
    // pattern with bare `\n` never matched there (run 34692890502).
    let src = include_str!("worker.rs").replace("\r\n", "\n");
    let gate_start = src
        .find("GATE: per docs/superpowers/specs/2026-05-16-lyrics-source-gating-design.md")
        .expect("gate comment must exist in process_song");
    // The gate `if` block is the first brace closed at 8-space indent after
    // the comment — independent of whatever stage follows it.
    let gate_end = src[gate_start..]
        .find("\n        }\n")
        .map(|rel| gate_start + rel)
        .expect("gate `if` block must close at 8-space indent");
    let gate_block = &src[gate_start..gate_end];

    assert!(
        !gate_block.contains("mark_unsupported_source"),
        "a zero-candidate song must no longer be stamped unsupported_source — \
         it must route to asr_path blind instead (#120)"
    );
    assert!(
        gate_block.contains("run_asr_path_branch"),
        "the gate block must still call run_asr_path_branch"
    );
    assert!(
        gate_block.contains("running blind (empty keyterms)"),
        "a zero-candidate song must log the blind-routing decision (#120)"
    );
}

#[test]
fn gather_does_not_bail_on_zero_text_candidates() {
    // Follow-up defect to #120: the gate block above routes zero-candidate
    // songs to asr_path blind, but `gather_sources_impl` still `bail!`ed with
    // "no text sources available" on an empty candidate list — so
    // `process_song` returned Err BEFORE the gate and `process_next` stamped
    // the row `no_source` at the current pipeline version, freezing it until
    // the next version bump (126 songs observed on win-resolume 2026-09-12).
    // gather must instead return Ok with an empty candidate list so the blind
    // route can run.
    //
    // gather's populated paths are real network I/O (yt_subs / lrclib /
    // genius / description), so — like the gate guard above — this is a
    // structural guard on the zero-candidate tail of gather.rs rather than a
    // full integration test. CRLF-normalised for the Windows CI checkout.
    let src = include_str!("gather.rs").replace("\r\n", "\n");
    assert!(
        !src.contains("bail!(\"no text sources available"),
        "gather must not bail on zero text candidates — it must return Ok with \
         an empty candidate list for the blind asr_path route (#120)"
    );
    assert!(
        src.contains("blind asr_path route"),
        "gather's zero-candidate tail must log the blind-route decision (#120)"
    );
}
