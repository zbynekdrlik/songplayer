//! RED structural guards (#144) for the lyrics worker deferral outcome.
//!
//! The asr_path branch used to `return Ok(())` on three "leaving row
//! unprocessed" exits (AAI key missing, vocal isolation failed, transient
//! ASR error) WITHOUT stamping the row; `process_next` treated `Ok` as done
//! and `get_next_video_for_lyrics` re-selected the same row every 5 s tick
//! (37-min hot-loop). The fix returns `SongOutcome::Deferred(reason)` and
//! `process_next` records a durable backoff via `record_lyrics_deferral`.
//!
//! Structural (source-string) guards because the branches are I/O-heavy and
//! `#[cfg_attr(test, mutants::skip)]`; include_str is CRLF-normalised for
//! the Windows CI checkout.

/// Every "leaving row unprocessed" exit in `run_asr_path_branch` must now
/// return a tagged `SongOutcome::Deferred`, never a bare `Ok(())`.
#[test]
fn asr_path_defers_instead_of_dropping_the_row() {
    let src = include_str!("worker_asr.rs").replace("\r\n", "\n");
    for reason in [
        "SongOutcome::Deferred(\"vocal_isolation_failed\")",
        "SongOutcome::Deferred(\"assemblyai_key_missing\")",
        "SongOutcome::Deferred(\"asr_error\")",
    ] {
        assert!(
            src.contains(reason),
            "worker_asr.rs must defer with {reason} instead of leaving the row unprocessed"
        );
    }
    assert!(
        !src.contains("return Ok(());"),
        "run_asr_path_branch must no longer `return Ok(())` — every early exit is Done or Deferred"
    );
}

/// `process_next` must translate a `Deferred` outcome into a durable backoff
/// by calling `record_lyrics_deferral`, otherwise the selector re-picks the
/// same unprocessable row every tick.
#[test]
fn process_next_records_deferral_backoff() {
    let src = include_str!("worker.rs").replace("\r\n", "\n");
    assert!(
        src.contains("SongOutcome::Deferred"),
        "process_next must handle the Deferred outcome"
    );
    assert!(
        src.contains("record_lyrics_deferral"),
        "process_next must record a durable retry backoff via record_lyrics_deferral"
    );
}
