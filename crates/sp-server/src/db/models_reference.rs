//! ★ reference-marker query functions (#142) — split out of `models.rs` to
//! keep that file under the 1000-line airuleset cap. Re-exported from
//! `models.rs` via `pub use models_reference::*;` so every existing call
//! site (`crate::db::models::get_video_lyrics_reference`, etc.) keeps
//! compiling unchanged.

use sqlx::SqlitePool;

/// Fast single-column read of `videos.lyrics_reference` by id (#142).
/// Mirrors `get_video_suppress_resolume_en` — used by the playback engine
/// hot path to decide whether the renderer should append the ★ marker to
/// this song's displayed lyric lines. Returns false when the row doesn't
/// exist.
pub async fn get_video_lyrics_reference(
    pool: &SqlitePool,
    video_id: i64,
) -> Result<bool, sqlx::Error> {
    let v: Option<i64> = sqlx::query_scalar("SELECT lyrics_reference FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_optional(pool)
        .await?;
    Ok(v.map(|n| n != 0).unwrap_or(false))
}

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
/// `post_reprocess` uses. Returns rows_affected (0 = no such video; the
/// HTTP handler maps that to 404).
pub async fn record_reference_feedback(
    pool: &SqlitePool,
    video_id: i64,
    note: &str,
) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "UPDATE videos SET lyrics_reference = 0, \
         lyrics_reference_rejected_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
         lyrics_reference_note = ?1, lyrics_manual_priority = 1 \
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
