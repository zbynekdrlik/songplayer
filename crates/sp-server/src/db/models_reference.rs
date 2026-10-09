//! ★ reference-marker query functions (#142) — split out of `models.rs` to
//! keep that file under the 1000-line airuleset cap. Re-exported from
//! `models.rs` via `pub use models_reference::*;` so every call site
//! (`crate::db::models::set_video_lyrics_reference`, etc.) keeps compiling
//! unchanged. #241: the wall shows no ★ any more, so playback reads no
//! reference flag (its per-song read is deleted).

use sqlx::SqlitePool;

/// Set (or clear) `videos.lyrics_reference` for a single video (#142,
/// `POST /api/v1/lyrics/songs/{id}/reference` admin toggle). Does NOT touch
/// the rejection columns (`lyrics_reference_rejected_at` /
/// `lyrics_reference_note`) — clearing those is `record_reference_feedback`'s
/// job when the owner flags a starred song as wrong. Returns rows_affected
/// (0 = no such video; the HTTP handler maps that to 404).
pub async fn set_video_lyrics_reference(
    pool: &SqlitePool,
    video_id: i64,
    reference: bool,
) -> sqlx::Result<u64> {
    let res = sqlx::query("UPDATE videos SET lyrics_reference = ?1 WHERE id = ?2")
        .bind(reference as i32)
        .bind(video_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Record the owner's "Nesedí" feedback on a reference-flagged song (#142,
/// `POST /api/v1/lyrics/songs/{id}/reference-feedback`). Clears
/// `lyrics_reference`, stamps `lyrics_reference_rejected_at` (RFC3339 UTC),
/// stores the note, and sets `lyrics_manual_priority` so the lyrics worker
/// re-queues the song for reprocessing — the same mechanism
/// `post_reprocess` uses, with a fresh attempt budget and no backoff left
/// (#144 review round 2: `lyrics_attempts = 0`, `lyrics_next_attempt_at =
/// NULL`). Returns rows_affected (0 = no such video; the HTTP handler maps
/// that to 404).
pub async fn record_reference_feedback(
    pool: &SqlitePool,
    video_id: i64,
    note: &str,
) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "UPDATE videos SET lyrics_reference = 0, \
         lyrics_reference_rejected_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
         lyrics_reference_note = ?1, lyrics_manual_priority = 1, \
         lyrics_attempts = 0, lyrics_next_attempt_at = NULL \
         WHERE id = ?2",
    )
    .bind(note)
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
#[path = "models_reference_tests_mutants.rs"]
mod models_reference_tests_mutants;
