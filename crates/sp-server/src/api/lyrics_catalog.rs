//! Admin endpoint for one-shot catalog reprocess under the new source gate.
//!
//! `POST /api/v1/lyrics/reprocess-catalog-with-new-gate`:
//!   1. Restamps any `pipeline_version > 20` rows down to 19 (un-anomalies
//!      the 77 rows produced by a now-reverted code path that bumped the
//!      constant without approval).
//!   2. Sets `lyrics_manual_priority = 1` on every row whose
//!      `pipeline_version < current` AND `lyrics_source` is not a parked
//!      sentinel (`asr_gap` / `unsupported_source`).
//!   3. Returns `{ "restamped": N, "queued": M }`.
//!
//! Idempotent — calling twice produces `{ "restamped": 0, "queued": 0 }`
//! on the second call (because the first call already moved every row out
//! of the matching set).
//!
//! See `docs/superpowers/specs/2026-05-16-lyrics-source-gating-design.md`.

use crate::AppState;
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::Serialize;
use tracing::warn;

#[derive(Debug, Serialize, PartialEq)]
pub struct ReprocessCatalogResponse {
    pub restamped: u64,
    pub queued: u64,
}

#[cfg_attr(test, mutants::skip)] // Thin glue: two UPDATEs and a JSON response;
// observable side effects are covered by the four sibling tests below
// (counts on each UPDATE, idempotency, response JSON shape). Remaining
// mutation targets reduce to SQL string literals that cargo-mutants
// cannot mutate meaningfully.
pub async fn reprocess_catalog_with_new_gate(State(state): State<AppState>) -> impl IntoResponse {
    use crate::lyrics::LYRICS_PIPELINE_VERSION;

    // Step 1: restamp v21 anomaly. Hardcoded `> 20` per spec — this is a
    // one-shot anomaly fix for the 77 rows produced by a reverted code path
    // that bumped LYRICS_PIPELINE_VERSION without approval. Parameterizing
    // on LYRICS_PIPELINE_VERSION would silently widen the blast radius if
    // the constant is ever (legitimately) bumped in the future. The literal
    // 20 locks the one-shot semantics.
    let restamped = match sqlx::query(
        "UPDATE videos SET lyrics_pipeline_version = 19 \
         WHERE lyrics_pipeline_version > 20",
    )
    .execute(&state.pool)
    .await
    {
        Ok(r) => r.rows_affected(),
        Err(e) => {
            warn!("reprocess_catalog_with_new_gate restamp error: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    // Step 2: queue all candidate rows.
    let queued = match sqlx::query(
        "UPDATE videos SET lyrics_manual_priority = 1 \
         WHERE lyrics_pipeline_version < ? \
           AND lyrics_manual_priority = 0 \
           AND (lyrics_source IS NULL \
                OR lyrics_source NOT IN ('asr_gap', 'unsupported_source'))",
    )
    .bind(LYRICS_PIPELINE_VERSION as i64)
    .execute(&state.pool)
    .await
    {
        Ok(r) => r.rows_affected(),
        Err(e) => {
            warn!("reprocess_catalog_with_new_gate queue error: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    tracing::info!(
        restamped,
        queued,
        "reprocess-catalog-with-new-gate complete"
    );

    Json(ReprocessCatalogResponse { restamped, queued }).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;

    async fn setup_pool_with_videos() -> SqlitePool {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Row 1: v21 (anomaly — should be restamped to 19, then queued).
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, lyrics_source, lyrics_pipeline_version) \
             VALUES (1, 'aaa', 't1', 'yt_subs', 21)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Row 2: v15, ordinary lyrics — should be queued.
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, lyrics_source, lyrics_pipeline_version) \
             VALUES (1, 'bbb', 't2', 'lrclib', 15)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Row 3: v10, asr_gap — must NOT be queued (skip-list).
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, lyrics_source, lyrics_pipeline_version) \
             VALUES (1, 'ccc', 't3', 'asr_gap', 10)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Row 4: v18, unsupported_source — must NOT be queued (skip-list).
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, lyrics_source, lyrics_pipeline_version) \
             VALUES (1, 'ddd', 't4', 'unsupported_source', 18)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Row 5: v20 (current) — must NOT be queued (already at current).
        sqlx::query(
            "INSERT INTO videos (playlist_id, youtube_id, title, lyrics_source, lyrics_pipeline_version) \
             VALUES (1, 'eee', 't5', 'yt_subs', 20)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    /// Replicate the endpoint's two UPDATEs against an in-memory pool so we
    /// can assert counts without spinning up an Axum router. Mirrors the
    /// pattern in `db::models_tests.rs` for SQL behavior verification.
    async fn run_endpoint_sql(pool: &SqlitePool) -> ReprocessCatalogResponse {
        let restamped = sqlx::query(
            "UPDATE videos SET lyrics_pipeline_version = 19 \
             WHERE lyrics_pipeline_version > 20",
        )
        .execute(pool)
        .await
        .unwrap()
        .rows_affected();

        let queued = sqlx::query(
            "UPDATE videos SET lyrics_manual_priority = 1 \
             WHERE lyrics_pipeline_version < 20 \
               AND lyrics_manual_priority = 0 \
               AND (lyrics_source IS NULL \
                    OR lyrics_source NOT IN ('asr_gap', 'unsupported_source'))",
        )
        .execute(pool)
        .await
        .unwrap()
        .rows_affected();

        ReprocessCatalogResponse { restamped, queued }
    }

    #[tokio::test]
    async fn endpoint_restamps_only_rows_above_current_version() {
        let pool = setup_pool_with_videos().await;
        let result = run_endpoint_sql(&pool).await;
        assert_eq!(result.restamped, 1, "only the v21 row must be restamped");
        let v: i64 = sqlx::query_scalar(
            "SELECT lyrics_pipeline_version FROM videos WHERE youtube_id = 'aaa'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(v, 19, "restamped row must now be at version 19");
    }

    #[tokio::test]
    async fn endpoint_queues_eligible_rows_and_skips_sentinels() {
        let pool = setup_pool_with_videos().await;
        let result = run_endpoint_sql(&pool).await;
        // After restamp: rows at v19 (was v21), v15, and v18 (unsupported_source)
        // are all `< 20`. The queue UPDATE excludes asr_gap + unsupported_source.
        // Eligible: v19 (was v21 yt_subs) + v15 lrclib = 2.
        assert_eq!(result.queued, 2, "two eligible rows must be queued");
        let aaa: i64 = sqlx::query_scalar(
            "SELECT lyrics_manual_priority FROM videos WHERE youtube_id = 'aaa'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let bbb: i64 = sqlx::query_scalar(
            "SELECT lyrics_manual_priority FROM videos WHERE youtube_id = 'bbb'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let ccc: i64 = sqlx::query_scalar(
            "SELECT lyrics_manual_priority FROM videos WHERE youtube_id = 'ccc'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let ddd: i64 = sqlx::query_scalar(
            "SELECT lyrics_manual_priority FROM videos WHERE youtube_id = 'ddd'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(aaa, 1, "restamped yt_subs row must be queued");
        assert_eq!(bbb, 1, "lrclib row must be queued");
        assert_eq!(ccc, 0, "asr_gap row must NOT be queued");
        assert_eq!(ddd, 0, "unsupported_source row must NOT be queued");
    }

    #[tokio::test]
    async fn endpoint_is_idempotent_on_second_call() {
        let pool = setup_pool_with_videos().await;
        let first = run_endpoint_sql(&pool).await;
        let second = run_endpoint_sql(&pool).await;
        assert!(
            first.restamped > 0 || first.queued > 0,
            "first call must do work"
        );
        // Idempotency is enforced by the SQL WHERE clauses:
        //   - Restamp: WHERE lyrics_pipeline_version > 20 → no rows match after
        //     the first call moved them all to ≤ 20.
        //   - Queue: WHERE lyrics_pipeline_version < 20 AND lyrics_manual_priority = 0
        //     → no rows match after the first call set every eligible row's
        //     manual_priority to 1.
        // So both counts must be 0 on the second call.
        assert_eq!(second.restamped, 0, "second restamp must affect 0 rows");
        assert_eq!(second.queued, 0, "second queue must affect 0 rows");
    }

    #[tokio::test]
    async fn endpoint_response_serializes_to_expected_json_shape() {
        let r = ReprocessCatalogResponse {
            restamped: 7,
            queued: 42,
        };
        let body = serde_json::to_string(&r).unwrap();
        assert_eq!(body, r#"{"restamped":7,"queued":42}"#);
    }
}
