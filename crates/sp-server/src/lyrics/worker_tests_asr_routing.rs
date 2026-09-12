//! asr_path gate-routing tests for `lyrics::worker` (#116/#120) — the timed
//! yt_subs → whisperx vs untimed-genius → asr_path routing guards and the
//! `run_asr_path_branch` no-AAI-key integration test. Split out of
//! `worker_tests.rs` to keep it under the 1000-line airuleset cap. Included
//! as a sibling file via
//! `#[path = "worker_tests_asr_routing.rs"] #[cfg(test)] mod tests_asr_routing;`
//! from `worker.rs`; shares `worker`'s items via `use super::*`.

use super::*;

#[test]
fn timed_yt_subs_skips_asr_path_entirely() {
    // Regression guard: a song with a timed yt_subs candidate MUST go through
    // the existing whisperx path, not the asr_path branch.
    //
    // The worker decision tree (worker.rs::process_song gate block):
    //   1. is_allowed_text_source(cands) == true  → whisperx path
    //   2. is_allowed_text_source(cands) == false → asr_path branch, always
    //      (#120): has_any_text_candidate(cands) == true passes the gathered
    //      candidate text as AAI keyterms bias; == false runs asr_path BLIND
    //      with empty keyterms instead of the old mark_unsupported_source
    //      terminal state.
    //
    // This test asserts that a timed yt_subs candidate triggers path 1, never
    // path 2. If `is_allowed_text_source` were ever modified to reject timed
    // yt_subs, this assertion would catch the regression.

    use crate::lyrics::orchestrator::{has_any_text_candidate, is_allowed_text_source};
    use crate::lyrics::provider::CandidateText;

    let timed = CandidateText {
        source: "yt_subs".to_string(),
        lines: vec!["hello".into(), "world".into()],
        line_timings: Some(vec![(0, 1000), (1200, 2000)]),
        has_timing: true,
    };
    let cands = vec![timed];

    // Gate predicate must accept timed yt_subs → whisperx path.
    assert!(
        is_allowed_text_source(&cands),
        "timed yt_subs must be accepted by the gate; if not, regression introduced"
    );
    // And has_any_text_candidate is also true here (proves the test exercises
    // a candidate that would otherwise be eligible for asr_path if the gate
    // ever falsely rejected it).
    assert!(has_any_text_candidate(&cands));
}

#[test]
fn untimed_genius_passes_gate_to_asr_path() {
    // Mirror regression: a song with ONLY untimed genius MUST be rejected by
    // the gate but accepted by `has_any_text_candidate`, putting it on path 2
    // (asr_path). If `is_allowed_text_source` ever started accepting untimed
    // sources, this test would catch that — and asr_path would no longer be
    // reachable for these songs.

    use crate::lyrics::orchestrator::{has_any_text_candidate, is_allowed_text_source};
    use crate::lyrics::provider::CandidateText;

    let untimed = CandidateText {
        source: "genius".to_string(),
        lines: vec!["hello".into(), "world".into()],
        line_timings: None,
        has_timing: false,
    };
    let cands = vec![untimed];

    assert!(
        !is_allowed_text_source(&cands),
        "untimed genius must be gate-rejected"
    );
    assert!(
        has_any_text_candidate(&cands),
        "untimed genius must still trigger has_any_text_candidate → asr_path branch"
    );
}

// asr_path end-to-end integration test (#116 review 🟡 #6): real in-memory
// SQLite + minimal LyricsWorker, no AAI key → branch exits Ok(()) without
// mutating the row.
#[tokio::test]
async fn run_asr_path_branch_returns_ok_when_aai_key_missing() {
    use std::time::Instant;

    let pool = crate::db::create_memory_pool().await.expect("pool");
    crate::db::run_migrations(&pool).await.expect("migrate");
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name) \
         VALUES (1, 'test', 'https://youtube.com/playlist?list=test', '')",
    )
    .execute(&pool)
    .await
    .expect("insert playlist");
    let video_id: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title, song, artist, normalized) \
         VALUES (1, 'test_yt_id', 'Test Title', 'Test Song', 'Test Artist', 1) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("insert video");

    let cache_dir = std::env::temp_dir().join("sp_asr_branch_test");
    let _ = std::fs::create_dir_all(&cache_dir);
    let (events_tx, _rx) = tokio::sync::broadcast::channel::<sp_core::ws::ServerMsg>(16);
    let worker = crate::lyrics::worker::LyricsWorker::new_for_test(
        pool.clone(),
        cache_dir.clone(),
        events_tx,
    );

    let result = worker
        .run_asr_path_branch(
            &[],
            None,
            video_id,
            "test_yt_id",
            "Test Song",
            "Test Artist",
            chrono::Utc::now().timestamp_millis(),
            Instant::now(),
        )
        .await;
    assert!(result.is_ok(), "expected Ok(()), got {result:?}");
    let _ = std::fs::remove_dir_all(&cache_dir);
}
